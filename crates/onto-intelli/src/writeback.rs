//! HTTP writeback targets (docs/design/EVENTS.md Part D, D.2 and D.3).
//!
//! An action type's `writeback = { target, route, body }` effect is planned
//! by onto-core: rendered strictly, refused while `[writeback] enabled` is
//! off, and committed as an outbox row (`kind = writeback`, `dedupe =
//! "<receipt>:<effect>"`) with the action. The delivery worker
//! ([`crate::webhooks::spawn_delivery`]) posts the stored body to the
//! target's `base_url` joined with the route's relative path, with
//! `Idempotency-Key: <receipt>:<effect>` and, when the target has a secret, the
//! Standard Webhooks signature headers. Outcomes, retries, the breaker, the
//! payload limit and the SSRF rules are the webhooks' (B.4, B.5); a dead row
//! writes a `writeback.failed` event, and `action.receipt` shows each row's
//! state.
//!
//! * Writeback is not reversible; action.undo does not undo the external
//!   write. Each call is enqueued once and delivered at least once; the
//!   receiver must honour `Idempotency-Key`.
//! * Targets are tenant KV (`writeback.target`, one value per target),
//!   written with their audit event in one commit
//!   ([`onto_core::Ontology::commit_config`]). Adding, removing and testing
//!   one needs an admin over every workspace it serves (dominance over
//!   `{admin, workspaces}`), never an agent.
//! * `base_url` is an `https://` URL, or an operator `env:`/`file:`
//!   reference to one (errors name the reference, never its value). A
//!   target a project credential registers is `restricted`: no operator
//!   allow rule applies to it, so it reaches public addresses only.
//! * Routes are relative paths without `//`, `@`, `\` or `..`; the request
//!   URL is the base (with a trailing `/`) joined with the route, and its
//!   scheme, host and port must stay the base's.
//! * Target and webhook endpoint names share one namespace (the outbox keys
//!   rows by target name), so a name is either an endpoint or a target.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use onto_core::automate::NS_WRITEBACK_TARGET;
use onto_core::outbox::ConfigEvent;
use onto_core::policy::{Role, ScopePrincipal, dominates};
use onto_core::{ActionContext, Error, Ontology, Principal, Result, one_or_many};

use crate::actions::parse;
use crate::hub::SecretBox;
use crate::intelli::Intelli;
use crate::net::{Reach, join_route};
use crate::webhooks::{
    BREAKER_PREFIX, Endpoint, NS_CONTROL, SecretRef, check_endpoint_url_as, generate_secret, is_reference,
    resolve_text, signing_key, url_origin, valid_name,
};

/// KV namespace of targets (key: the target name).
pub const NS_TARGET: &str = NS_WRITEBACK_TARGET;
/// The sealing purpose of target signing secrets.
pub const SECRET_PURPOSE: &str = "writeback-secret";
/// The writeback actions, dispatched by [`Intelli::writeback_call`].
pub const CALLS: &[&str] = &["writeback.add", "writeback.list", "writeback.remove", "writeback.test"];
/// Words for outputs about writeback (docs/design/EVENTS.md section 13).
pub const WRITEBACK_NOTE: &str = "Writeback is not reversible; action.undo does not undo the external write. Each \
                                  call is enqueued once and delivered at least once with Idempotency-Key: \
                                  <receipt>:<effect>; the receiver must honour it.";

/// A writeback target (KV `writeback.target`, key = name). It acts as
/// `{admin, workspaces}`: actions in those workspaces may write to it, and
/// only an admin over all of them may add or remove it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Target {
    /// `[a-z0-9_-]`, at most 40 characters.
    pub name: String,
    /// `https://…`, or an operator `env:`/`file:` reference to one; checked
    /// when written and before every request.
    pub base_url: String,
    /// The signing secret (Standard Webhooks headers); none: unsigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretRef>,
    /// Route name → relative path under `base_url`.
    pub routes: BTreeMap<String, String>,
    /// Actions in these workspaces may use it (`*`: every workspace).
    pub workspaces: Vec<String>,
    /// Registered by a project credential: no operator allow rules apply.
    #[serde(default)]
    pub restricted: bool,
    /// Part of automation closure hashes; it starts at the registration
    /// time (ms).
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub created_at: i64,
    /// A random id drawn at registration: with `created_at`, part of
    /// automation closure hashes, so a target removed and added again never
    /// hashes like the one before.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub incarnation: String,
}

impl Target {
    /// Is `row` this target's: a writeback row of its name created since it
    /// was registered (a target removed and added again never sends the
    /// rows of the one before)?
    pub fn owns(&self, row: &onto_core::outbox::OutboxRow) -> bool {
        row.kind == onto_core::outbox::OutboxKind::Writeback
            && row.target == self.name
            && row.created_at >= self.created_at
    }

    /// `writeback:<name>` (the audit target of its events; not a principal
    /// anything acts as).
    pub fn audit_id(&self) -> String {
        format!("writeback:{}", self.name)
    }

    /// `{admin, workspaces}`: what the caller must dominate to manage it.
    pub fn scope(&self) -> ScopePrincipal {
        scope(&self.workspaces)
    }

    /// The target `name`, if it exists.
    pub fn load(onto: &Ontology, name: &str) -> Result<Option<Target>> {
        match onto.store().kv_get(NS_TARGET, name)? {
            None | Some(Value::Null) => Ok(None),
            Some(v) => serde_json::from_value(v)
                .map(Some)
                .map_err(|e| Error::Storage(format!("corrupt writeback target {name}: {e}"))),
        }
    }

    /// Every target, by name.
    pub fn list(onto: &Ontology) -> Result<Vec<Target>> {
        let mut out = Vec::new();
        for (name, v) in onto.store().kv_scan(NS_TARGET, "")? {
            if v.is_null() {
                continue;
            }
            out.push(
                serde_json::from_value(v)
                    .map_err(|e| Error::Storage(format!("corrupt writeback target {name}: {e}")))?,
            );
        }
        Ok(out)
    }

    /// How errors name the base URL: the reference, or the target.
    pub fn base_what(&self) -> String {
        if is_reference(&self.base_url) {
            format!("the URL in {}", self.base_url.trim())
        } else {
            format!("the base URL of writeback target {}", self.name)
        }
    }

    /// The base URL, resolved and checked against `reach`, with a trailing
    /// `/` so routes join under it. Errors name references, never values.
    pub fn base(&self, reach: &Reach) -> Result<url::Url> {
        self.base_as(reach, "base_url")
    }

    /// [`Target::base`], naming a literal URL by `field` in errors.
    fn base_as(&self, reach: &Reach, field: &str) -> Result<url::Url> {
        let raw = resolve_text(&self.base_url, &self.base_what())?;
        let mut base = check_endpoint_url_as(&self.base_url, &raw, field, reach)?;
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Ok(base)
    }

    /// The request URL of `route`: the base joined with the route's path,
    /// on the base's scheme, host and port.
    pub fn request_url(&self, route: &str, reach: &Reach) -> Result<url::Url> {
        self.request_url_as(route, reach, "base_url")
    }

    /// [`Target::request_url`], naming a literal base URL by `field`.
    fn request_url_as(&self, route: &str, reach: &Reach, field: &str) -> Result<url::Url> {
        let path = self
            .routes
            .get(route)
            .ok_or_else(|| Error::invalid(format!("writeback target {} has no route {route}", self.name)))?;
        join_route(&self.base_as(reach, field)?, path)
    }

    /// The HMAC key requests are signed with, if the target has a secret.
    pub fn signing_keys(&self, sealer: Option<&dyn SecretBox>) -> Result<Vec<Vec<u8>>> {
        match &self.secret {
            None => Ok(Vec::new()),
            Some(s) => {
                let owner = format!("writeback target {}", self.name);
                Ok(vec![signing_key(s.resolve_for(sealer, SECRET_PURPOSE, &owner, &self.name)?.expose())?])
            }
        }
    }
}

fn scope(workspaces: &[String]) -> ScopePrincipal {
    ScopePrincipal::new(vec![Role::Admin], workspaces.to_vec())
}

#[derive(Deserialize)]
struct AddIn {
    name: String,
    /// The base URL; `url` is accepted in its place, and refusals name the
    /// field the caller used.
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    url: Option<String>,
    /// Plaintext (sealed here) or an operator reference; none: unsigned.
    #[serde(default)]
    secret: Option<String>,
    /// Generate a `whsec_` secret, sealed and returned once.
    #[serde(default)]
    generate_secret: bool,
    #[serde(default)]
    routes: BTreeMap<String, String>,
    /// The workspaces whose actions may use it (default: the call's).
    #[serde(default, deserialize_with = "one_or_many")]
    workspaces: Vec<String>,
    /// Set by the server for project credentials.
    #[serde(default)]
    restricted: bool,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct NameIn {
    name: String,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct TestIn {
    name: String,
    /// One route (default: every route).
    #[serde(default)]
    route: Option<String>,
}

/// Admins only, never agents.
fn admin_check(onto: &Ontology, ctx: &ActionContext, action: &str) -> Result<()> {
    let p = &ctx.principal;
    if p.is_agent() || !p.is_privileged() {
        return Err(onto.refuse_configure(ctx, format!("{} may not {action} (admins only, never agents)", p.id)));
    }
    Ok(())
}

/// The caller must be an admin over every workspace the target serves.
fn dominance_check(onto: &Ontology, ctx: &ActionContext, action: &str, name: &str, sp: &ScopePrincipal) -> Result<()> {
    let p = &ctx.principal;
    if !dominates(p, sp) {
        return Err(onto.refuse_configure(
            ctx,
            format!(
                "{} may not {action} writeback target {name}: it serves {}, and only an admin of every one of them \
                 may manage it",
                p.id,
                sp.workspaces.join(", ")
            ),
        ));
    }
    Ok(())
}

fn load(onto: &Ontology, name: &str) -> Result<Target> {
    Target::load(onto, name)?.ok_or_else(|| Error::not_found(format!("writeback target {name}")))
}

/// The host an audit event names (never the path or query).
fn audit_host(raw: &str) -> String {
    if is_reference(raw) {
        return raw.trim().to_string();
    }
    url::Url::parse(raw.trim()).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()
}

impl Intelli {
    /// Check a base URL input (as [`Intelli::reach`] judges it now; names are
    /// resolved and judged at every request) and every route under it.
    /// Errors name references, never their values.
    /// `field` is the input field the URL came in (`base_url` or `url`).
    fn check_target_input(
        &self,
        field: &str,
        raw: &str,
        routes: &BTreeMap<String, String>,
        restricted: bool,
    ) -> Result<String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(Error::invalid(format!("{field}: a writeback target needs an https URL")));
        }
        let stored = if is_reference(raw) {
            SecretRef::parse(raw).map_err(|e| Error::invalid(format!("{field}: {e}")))?.stored()
        } else {
            raw.to_string()
        };
        let probe = Target {
            name: String::new(),
            base_url: stored.clone(),
            secret: None,
            routes: routes.clone(),
            workspaces: Vec::new(),
            restricted,
            version: 0,
            created_by: String::new(),
            created_at: 0,
            incarnation: String::new(),
        };
        let reach = self.reach(restricted)?;
        if routes.is_empty() {
            return Err(Error::invalid("routes: name at least one route (name = relative path)"));
        }
        for route in routes.keys() {
            if !valid_name(route) {
                return Err(Error::invalid(format!(
                    "routes: route name '{route}' must be 1 to 40 of a-z, 0-9, _ and -"
                )));
            }
            probe.request_url_as(route, &reach, field)?;
        }
        Ok(stored)
    }

    /// The secret to store for target `name`, and a generated plaintext,
    /// returned once.
    fn target_secret(
        &self,
        name: &str,
        input: Option<&str>,
        generate: bool,
    ) -> Result<(Option<SecretRef>, Option<String>)> {
        let sealer = self.secrets();
        let given = input.map(str::trim).filter(|s| !s.is_empty());
        match (given, generate) {
            (Some(_), true) => Err(Error::invalid("writeback.add: give secret or generate_secret, not both")),
            (None, false) => Ok((None, None)),
            (None, true) => {
                let plain = generate_secret()?;
                Ok((Some(SecretRef::seal_for(sealer.as_deref(), SECRET_PURPOSE, name, plain.as_bytes())?), Some(plain)))
            }
            (Some(s), _) if is_reference(s) => {
                Ok((Some(SecretRef::parse(s).map_err(|e| Error::invalid(format!("secret: {e}")))?), None))
            }
            (Some(s), _) if s.starts_with("sealed:") => {
                Err(Error::invalid("secret: give the secret itself (it is sealed here), not a sealed value"))
            }
            (Some(s), _) => {
                crate::webhooks::check_secret(s.as_bytes(), "secret")?;
                Ok((Some(SecretRef::seal_for(sealer.as_deref(), SECRET_PURPOSE, name, s.as_bytes())?), None))
            }
        }
    }

    /// What outputs show of a target: never a secret; the full base URL and
    /// route paths only to callers who may manage it.
    fn target_view(&self, p: &Principal, t: &Target) -> Value {
        let manage = dominates(p, &t.scope());
        let routes: Value = if manage { json!(t.routes) } else { json!(t.routes.keys().collect::<Vec<_>>()) };
        json!({
            "name": t.name,
            "base_url": if manage { t.base_url.clone() } else { url_origin(&t.base_url) },
            "routes": routes,
            "workspaces": t.workspaces,
            "secret": t.secret.as_ref().map(SecretRef::describe),
            "signed": t.secret.is_some(),
            "restricted": t.restricted,
            "version": t.version,
            "created_by": t.created_by,
            "created_at": t.created_at,
            "may_manage": manage,
        })
    }

    /// The writeback actions ([`CALLS`]). Synchronous: the store calls block,
    /// so the caller runs this off the async workers.
    pub(crate) fn writeback_call(&self, p: &Principal, action: &str, input: Value) -> Result<Value> {
        let onto = self.ontology();
        let enabled = self.config().writeback.enabled;
        match action {
            "writeback.list" => {
                let ctx = self.ctx(p, None);
                admin_check(onto, &ctx, "list writeback targets")?;
                let mut ts = Target::list(onto)?;
                ts.sort_by(|a, b| a.name.cmp(&b.name));
                let items: Vec<Value> = ts.iter().map(|t| self.target_view(p, t)).collect();
                Ok(json!({
                    "items": items,
                    "enabled": enabled,
                    "max_targets": self.config().writeback.max_targets,
                    "note": WRITEBACK_NOTE,
                }))
            }
            "writeback.add" => {
                let i: AddIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                admin_check(onto, &ctx, "add writeback targets")?;
                let name = i.name.trim().to_string();
                if !valid_name(&name) {
                    return Err(Error::invalid(format!(
                        "writeback.add: name '{name}' must be 1 to 40 of a-z, 0-9, _ and -"
                    )));
                }
                let mut workspaces: Vec<String> =
                    i.workspaces.iter().map(|w| w.trim().to_string()).filter(|w| !w.is_empty()).collect();
                if workspaces.is_empty() {
                    workspaces.push(ctx.workspace.clone());
                }
                workspaces.sort();
                workspaces.dedup();
                dominance_check(onto, &ctx, action, &name, &scope(&workspaces))?;
                if Target::load(onto, &name)?.is_some() {
                    return Err(Error::Conflict(format!(
                        "writeback target {name} exists (writeback.remove it, then add it again)"
                    )));
                }
                if Endpoint::load(onto, &name)?.is_some() {
                    return Err(Error::Conflict(format!(
                        "{name} is a webhook endpoint: endpoints and writeback targets share one namespace"
                    )));
                }
                let max = self.config().writeback.max_targets;
                if Target::list(onto)?.len() >= max {
                    return Err(Error::Limited(format!(
                        "this tenant has {max} writeback targets, the most [writeback] max_targets allows"
                    )));
                }
                let routes: BTreeMap<String, String> =
                    i.routes.into_iter().map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
                let (field, raw) = match (&i.base_url, &i.url) {
                    (Some(_), Some(_)) => {
                        return Err(Error::invalid("writeback.add: give base_url or url (its alias), not both"));
                    }
                    (Some(b), None) => ("base_url", b.as_str()),
                    (None, Some(u)) => ("url", u.as_str()),
                    (None, None) => ("base_url", ""),
                };
                let base_url = self.check_target_input(field, raw, &routes, i.restricted)?;
                let (secret, generated) = self.target_secret(&name, i.secret.as_deref(), i.generate_secret)?;
                // The store's clock, after every row of an earlier target of
                // this name: rows created for this one are never older.
                let now = crate::webhooks::incarnation_start(onto, &name)?;
                let t = Target {
                    name: name.clone(),
                    base_url,
                    secret,
                    routes,
                    workspaces,
                    restricted: i.restricted,
                    version: now.max(1) as u64,
                    created_by: p.id.clone(),
                    created_at: now,
                    incarnation: crate::webhooks::new_incarnation()?,
                };
                let payload = json!({
                    "name": t.name,
                    "host": audit_host(&t.base_url),
                    "routes": t.routes.keys().collect::<Vec<_>>(),
                    "workspaces": t.workspaces,
                    "signed": t.secret.is_some(),
                    "restricted": t.restricted,
                    "version": t.version,
                });
                let kv = vec![(NS_TARGET.to_string(), t.name.clone(), serde_json::to_value(&t)?)];
                onto.commit_config(&ctx, kv, ConfigEvent::new(action, Some(&t.audit_id()), payload))?;
                let mut out = json!({ "target": self.target_view(p, &t), "enabled": enabled, "note": WRITEBACK_NOTE });
                if !enabled {
                    out["enabled_note"] =
                        json!("[writeback] enabled is off on this server: plans with writeback effects are refused");
                }
                if let Some(s) = generated {
                    out["secret"] = json!(s);
                    out["secret_note"] = json!("the signing secret is shown once; store it with the receiver");
                }
                Ok(out)
            }
            "writeback.remove" => {
                let i: NameIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                admin_check(onto, &ctx, "remove writeback targets")?;
                let t = load(onto, i.name.trim())?;
                dominance_check(onto, &ctx, action, &t.name, &t.scope())?;
                let mut kv = vec![(NS_TARGET.to_string(), t.name.clone(), Value::Null)];
                let breaker = format!("{BREAKER_PREFIX}{}", t.name);
                if onto.store().kv_get(NS_CONTROL, &breaker)?.is_some_and(|v| !v.is_null()) {
                    kv.push((NS_CONTROL.to_string(), breaker, Value::Null));
                }
                let payload = json!({ "name": t.name, "host": audit_host(&t.base_url), "version": t.version });
                onto.commit_config(&ctx, kv, ConfigEvent::new(action, Some(&t.audit_id()), payload))?;
                Ok(json!({
                    "removed": t.name,
                    "note": "rows not yet sent to it are cancelled when the worker reaches them; plans that name it \
                             are refused, and enabled automations that reach it stop (their closure changed)",
                }))
            }
            "writeback.test" => {
                let i: TestIn = parse(action, input)?;
                let ctx = self.ctx(p, None).with_intent(action);
                admin_check(onto, &ctx, "test writeback targets")?;
                let t = load(onto, i.name.trim())?;
                dominance_check(onto, &ctx, action, &t.name, &t.scope())?;
                let reach = self.reach(t.restricted)?;
                let picked: Vec<&String> = match i.route.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
                    Some(r) => {
                        let (k, _) = t
                            .routes
                            .get_key_value(r)
                            .ok_or_else(|| Error::not_found(format!("route {r} of writeback target {}", t.name)))?;
                        vec![k]
                    }
                    None => t.routes.keys().collect(),
                };
                let mut problems = Vec::new();
                let base = match t.base(&reach) {
                    Ok(u) => Some(u),
                    Err(e) => {
                        problems.push(e.to_string());
                        None
                    }
                };
                let routes: Vec<Value> = picked
                    .into_iter()
                    .map(|r| {
                        let path = &t.routes[r];
                        match base.as_ref().map(|b| join_route(b, path)) {
                            Some(Ok(_)) => json!({ "route": r, "path": path, "ok": true }),
                            Some(Err(e)) => {
                                problems.push(e.to_string());
                                json!({ "route": r, "path": path, "ok": false, "error": e.to_string() })
                            }
                            None => json!({ "route": r, "path": path, "ok": false }),
                        }
                    })
                    .collect();
                match t.signing_keys(self.secrets().as_deref()) {
                    Err(e) => problems.push(e.to_string()),
                    Ok(keys) if keys.iter().any(|k| k.len() < crate::webhooks::MIN_KEY_BYTES) => {
                        problems.push(format!(
                            "the signing secret ({}) gives a key under {} bytes",
                            t.secret.as_ref().map(SecretRef::describe).unwrap_or_default(),
                            crate::webhooks::MIN_KEY_BYTES
                        ))
                    }
                    Ok(_) => {}
                }
                Ok(json!({
                    "name": t.name,
                    "ok": problems.is_empty(),
                    "enabled": enabled,
                    // A reference is named, never resolved in outputs.
                    "base": if is_reference(&t.base_url) { t.base_url.trim().to_string() } else { url_origin(&t.base_url) },
                    "routes": routes,
                    "signed": t.secret.is_some(),
                    "problems": problems,
                    "note": "nothing was sent: writeback.test checks the configuration only (references resolve, \
                             the URL and every route pass the outbound rules), since a request could write to the \
                             target; host names are resolved and judged at every request",
                }))
            }
            other => Err(Error::invalid(format!("unknown action '{other}' (see system.actions)"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::AllowRule;
    use std::sync::Arc;

    fn target(base: &str) -> Target {
        Target {
            name: "helpdesk".into(),
            base_url: base.into(),
            secret: None,
            routes: [("close".to_string(), "tickets/close".to_string()), ("bad".into(), "//evil.example/x".into())]
                .into_iter()
                .collect(),
            workspaces: vec!["acme".into()],
            restricted: false,
            version: 1,
            created_by: "lead".into(),
            created_at: 0,
            incarnation: String::new(),
        }
    }

    fn reach(restricted: bool) -> Reach {
        let allow = vec![AllowRule::new("127.0.0.1", &["127.0.0.1/32".to_string()]).unwrap()];
        Reach { allow: Arc::new(allow), restricted, allow_private: false }
    }

    #[test]
    fn routes_join_under_the_base_and_never_leave_its_host() {
        let t = target("https://helpdesk.example.com/api");
        let u = t.request_url("close", &reach(false)).unwrap();
        assert_eq!(u.as_str(), "https://helpdesk.example.com/api/tickets/close");
        let e = t.request_url("bad", &reach(false)).unwrap_err().to_string();
        assert!(e.contains("may not contain //"), "{e}");
        assert!(t.request_url("missing", &reach(false)).unwrap_err().to_string().contains("has no route missing"));
        // An allow-listed loopback base works for an operator's target only.
        let local = target("http://127.0.0.1:9000/");
        assert!(local.request_url("close", &reach(false)).is_ok());
        assert!(local.request_url("close", &reach(true)).is_err(), "a restricted target ignores the allow list");
        // A metadata address is refused, allow-listed or not.
        let meta = target("https://169.254.169.254/");
        assert!(meta.request_url("close", &reach(false)).is_err());
        assert_eq!(t.scope(), ScopePrincipal::new(vec![Role::Admin], vec!["acme".into()]));
        assert_eq!(t.audit_id(), "writeback:helpdesk");
    }

    #[test]
    fn a_base_url_reference_is_named_never_resolved_in_errors() {
        let path = std::env::temp_dir().join(format!("onto-wb-{}", onto_core::time::new_id("x")));
        std::fs::write(&path, "https://169.254.169.254/latest/").unwrap();
        let t = target(&format!("file:{}", path.display()));
        let e = t.request_url("close", &reach(false)).unwrap_err().to_string();
        assert!(e.contains(&format!("file:{}", path.display())), "{e}");
        assert!(!e.contains("169.254"), "the value never shows: {e}");
        let unset = target("env:ONTO_TEST_SURELY_UNSET_WRITEBACK_URL");
        let e = unset.request_url("close", &reach(false)).unwrap_err().to_string();
        assert!(e.contains("env:ONTO_TEST_SURELY_UNSET_WRITEBACK_URL"), "{e}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn targets_without_a_secret_are_unsigned() {
        assert!(target("https://x.example.com/").signing_keys(None).unwrap().is_empty());
        let mut t = target("https://x.example.com/");
        t.secret = Some(SecretRef::Env("ONTO_TEST_SURELY_UNSET_WB_SECRET".into()));
        let e = t.signing_keys(None).unwrap_err().to_string();
        assert!(e.contains("env:ONTO_TEST_SURELY_UNSET_WB_SECRET"), "{e}");
    }
}
