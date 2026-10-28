//! The webhook actions ([`CALLS`]): endpoints and their deliveries
//! (docs/design/EVENTS.md B.2, section 6.1, section 7).
//!
//! * Admins only, never agents. Every operation that changes, tests,
//!   replays or resumes an endpoint, or shows its deliveries (errors and
//!   receiver responses), also needs the caller to dominate the endpoint's
//!   principal ([`onto_core::policy::dominates`]): a narrower admin cannot
//!   redirect, replay or read what a broader endpoint receives. Stopping
//!   (`webhook.kill` on, disabling through `webhook.update`) needs only that
//!   right over the endpoint, or admin for the tenant switch. Refusals are
//!   audited as `authorization.denied`.
//! * Endpoint configuration is written with its audit event in one commit
//!   ([`onto_core::Ontology::commit_config`]); the version moves on every
//!   configuration change.
//! * Signing secrets: a plaintext secret is sealed with the tenant's vault;
//!   no secret generates a `whsec_` one, sealed and returned once; `env:` and
//!   `file:` references are the operator's (the surfaces refuse them from
//!   anyone else). Outputs show references and `sealed (kek <id>)`, never a
//!   secret.
//! * A change subscription (`filter`) starts at the head of the change feed
//!   when it is set; fan-out keeps one cursor per endpoint from then on.
//!
//! Each change is enqueued once per endpoint and delivered at least once;
//! receivers dedupe on the event id.

use serde::Deserialize;
use serde_json::{Value, json};

use onto_core::outbox::{ConfigEvent, OutboxQuery, OutboxSelect, OutboxState};
use onto_core::policy::{Role, ScopePrincipal, dominates};
use onto_core::time::now_ms;
use onto_core::{ActionContext, Error, Ontology, Principal, Result, one_or_many};

use super::{
    BACKLOG_PREFIX, BREAKER_PREFIX, BreakerState, DELIVERY_NOTE, Endpoint, KILL_KEY, NS_CONTROL, NS_ENDPOINT,
    SecretRef, check_endpoint_url, consumer_name, enqueue_test, generate_secret, is_reference, killed, resolve_text,
    valid_name,
};
use crate::actions::parse;
use crate::changes::FilterIn;
use crate::intelli::Intelli;
use crate::net::Reach;

/// The webhook actions, dispatched by [`Intelli::webhooks_call`].
pub const CALLS: &[&str] = &[
    "webhook.add",
    "webhook.update",
    "webhook.remove",
    "webhook.list",
    "webhook.test",
    "webhook.rotate_secret",
    "webhook.deliveries",
    "webhook.replay",
    "webhook.resume",
    "webhook.kill",
    "outbox.list",
];

/// How long the previous secret keeps signing after a rotation, by default.
pub const ROTATION_KEEP_SECS: u64 = 86_400;
/// The longest a previous secret may keep signing.
pub const ROTATION_MAX_SECS: u64 = 30 * 86_400;

#[derive(Deserialize)]
struct PrincipalIn {
    #[serde(default, deserialize_with = "one_or_many")]
    roles: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    workspaces: Vec<String>,
}

impl PrincipalIn {
    fn scope(self) -> Result<ScopePrincipal> {
        let roles = self.roles.iter().map(|r| Role::parse(r)).collect::<Result<Vec<_>>>()?;
        let workspaces: Vec<String> =
            self.workspaces.into_iter().map(|w| w.trim().to_string()).filter(|w| !w.is_empty()).collect();
        let sp = ScopePrincipal::new(roles, workspaces);
        check_scope(&sp)?;
        Ok(sp)
    }
}

/// An endpoint principal needs a role and a workspace, and holds neither
/// `system` nor `agent`.
fn check_scope(sp: &ScopePrincipal) -> Result<()> {
    if sp.roles.is_empty() || sp.workspaces.is_empty() {
        return Err(Error::invalid("an endpoint principal names roles and workspaces"));
    }
    if sp.roles.iter().any(|r| matches!(r, Role::System | Role::Agent)) {
        return Err(Error::invalid("an endpoint principal may not hold system or agent"));
    }
    Ok(())
}

#[derive(Deserialize)]
struct AddIn {
    name: String,
    url: String,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    filter: Option<FilterIn>,
    #[serde(default)]
    principal: Option<PrincipalIn>,
    /// Register it switched off (proto3 drops `enabled: false`).
    #[serde(default)]
    disabled: bool,
    /// Set by the server for project credentials.
    #[serde(default)]
    restricted: bool,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct UpdateIn {
    name: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    filter: Option<FilterIn>,
    /// Drop the change subscription (effect deliveries only).
    #[serde(default)]
    clear_filter: bool,
    #[serde(default)]
    principal: Option<PrincipalIn>,
    #[serde(default)]
    enabled: Option<bool>,
    /// The negative form gRPC needs.
    #[serde(default)]
    disabled: Option<bool>,
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
struct RotateIn {
    name: String,
    #[serde(default)]
    secret: Option<String>,
    /// Seconds the previous secret keeps signing (default a day, 0: not at all).
    #[serde(default)]
    keep_secs: Option<u64>,
    #[serde(default)]
    restricted: bool,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct DeliveriesIn {
    name: String,
    /// One row with its attempts (newest first).
    #[serde(default)]
    id: Option<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    states: Vec<String>,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ReplayIn {
    name: String,
    #[serde(default, deserialize_with = "one_or_many")]
    ids: Vec<String>,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default, deserialize_with = "one_or_many")]
    states: Vec<String>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct KillIn {
    #[serde(default)]
    on: Option<bool>,
    #[serde(default)]
    off: Option<bool>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct OutboxIn {
    #[serde(default)]
    target: Option<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    states: Vec<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    receipt: Option<String>,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
}

fn states(raw: &[String]) -> Result<Vec<OutboxState>> {
    raw.iter().map(|s| s.trim().parse()).collect()
}

/// `scheme://host[:port]`: what a caller who may not manage an endpoint
/// sees of its URL (a path or query may carry a receiver's token).
pub fn url_origin(raw: &str) -> String {
    if is_reference(raw) {
        return raw.trim().to_string();
    }
    match url::Url::parse(raw.trim()) {
        Ok(u) => u.origin().ascii_serialization(),
        Err(_) => "(not a URL)".into(),
    }
}

/// The host an audit event names (never the path or query).
fn audit_host(raw: &str) -> String {
    if is_reference(raw) {
        return raw.trim().to_string();
    }
    url::Url::parse(raw.trim()).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()
}

/// Admins only, never agents.
fn admin_check(onto: &Ontology, ctx: &ActionContext, action: &str) -> Result<()> {
    let p = &ctx.principal;
    if p.is_agent() || !p.is_privileged() {
        return Err(onto.refuse_configure(ctx, format!("{} may not {action} (admins only, never agents)", p.id)));
    }
    Ok(())
}

/// The caller must dominate the endpoint's principal (section 6.1).
fn dominance_check(onto: &Ontology, ctx: &ActionContext, action: &str, ep: &Endpoint) -> Result<()> {
    let p = &ctx.principal;
    if !dominates(p, &ep.principal) {
        return Err(onto.refuse_configure(
            ctx,
            format!(
                "{} may not {action} webhook endpoint {}: it acts as {} in {}, which {} does not cover",
                p.id,
                ep.name,
                roles_text(&ep.principal),
                ep.principal.workspaces.join(", "),
                p.id
            ),
        ));
    }
    Ok(())
}

fn roles_text(sp: &ScopePrincipal) -> String {
    sp.roles.iter().map(|r| r.as_str()).collect::<Vec<_>>().join("+")
}

fn load(onto: &Ontology, name: &str) -> Result<Endpoint> {
    Endpoint::load(onto, name)?.ok_or_else(|| Error::not_found(format!("webhook endpoint {name}")))
}

/// Operational state of one endpoint, kept apart from its configuration.
#[derive(Default)]
struct Control {
    breaker: Option<BreakerState>,
    backlog: Option<Value>,
}

fn controls(onto: &Ontology) -> Result<(bool, std::collections::HashMap<String, Control>)> {
    let mut out: std::collections::HashMap<String, Control> = std::collections::HashMap::new();
    let mut kill = false;
    for (k, v) in onto.store().kv_scan(NS_CONTROL, "")? {
        if v.is_null() {
            continue;
        }
        if k == KILL_KEY {
            kill = v.get("on").and_then(Value::as_bool) == Some(true);
        } else if let Some(name) = k.strip_prefix(BREAKER_PREFIX) {
            out.entry(name.to_string()).or_default().breaker = serde_json::from_value(v).ok();
        } else if let Some(name) = k.strip_prefix(BACKLOG_PREFIX) {
            out.entry(name.to_string()).or_default().backlog = Some(v);
        }
    }
    Ok((kill, out))
}

impl Intelli {
    fn reach(&self, restricted: bool) -> Result<Reach> {
        Ok(Reach {
            allow: std::sync::Arc::new(self.config().webhooks.allow_rules()?),
            restricted,
            allow_private: self.config().console.allow_private_urls,
        })
    }

    /// Check a URL input: `https://…` the SSRF rules allow (an IP literal is
    /// judged now; names are resolved and judged at every delivery), or an
    /// operator `env:`/`file:` reference whose value is checked the same
    /// way. Errors name the reference, never its value.
    fn check_url_input(&self, raw: &str, restricted: bool) -> Result<String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(Error::invalid("url: an endpoint needs an https URL"));
        }
        let reach = self.reach(restricted)?;
        if is_reference(raw) {
            let r = SecretRef::parse(raw).map_err(|e| Error::invalid(format!("url: {e}")))?;
            let value = resolve_text(raw, &format!("the URL in {}", r.stored()))?;
            check_endpoint_url(&r.stored(), &value, &reach)?;
            return Ok(r.stored());
        }
        check_endpoint_url(raw, raw, &reach)?;
        Ok(raw.to_string())
    }

    /// The secret to store for `name` from a `secret` input; also the
    /// generated plaintext, returned once.
    fn secret_input(&self, name: &str, input: Option<&str>) -> Result<(SecretRef, Option<String>)> {
        let sealer = self.secrets();
        let given = input.map(str::trim).filter(|s| !s.is_empty());
        match given {
            None => {
                let plain = generate_secret()?;
                let sealed = SecretRef::seal(sealer.as_deref(), name, plain.as_bytes())?;
                Ok((sealed, Some(plain)))
            }
            Some(s) if is_reference(s) => {
                Ok((SecretRef::parse(s).map_err(|e| Error::invalid(format!("secret: {e}")))?, None))
            }
            Some(s) if s.starts_with("sealed:") => {
                Err(Error::invalid("secret: give the secret itself (it is sealed here), not a sealed value"))
            }
            Some(s) => Ok((SecretRef::seal(sealer.as_deref(), name, s.as_bytes())?, None)),
        }
    }

    /// What outputs show of an endpoint: its configuration (no secret), its
    /// status and, for callers who may manage it, the full URL and its
    /// change cursor.
    fn endpoint_view(&self, p: &Principal, ep: &Endpoint, ctl: Option<&Control>, open: Option<u64>) -> Value {
        let manage = dominates(p, &ep.principal);
        let now = now_ms();
        let person_paused = ep.paused.as_ref().filter(|x| x.until.is_none_or(|u| u > now));
        let breaker = ctl.and_then(|c| c.breaker.as_ref()).filter(|b| b.is_open());
        let backlog = ctl.and_then(|c| c.backlog.as_ref());
        let paused = if let Some(x) = person_paused {
            Some(json!(x))
        } else if let Some(b) = backlog {
            Some(json!({ "reason": "backlogged", "since": b.get("since"), "pending": b.get("pending") }))
        } else {
            breaker.map(|b| json!({ "reason": "failing", "until": b.until, "failures": b.failures }))
        };
        let status = if !ep.enabled {
            "disabled"
        } else if paused.is_some() {
            "paused"
        } else {
            "active"
        };
        let mut v = json!({
            "name": ep.name,
            "principal_id": ep.principal_id(),
            "url": if manage { ep.url.clone() } else { url_origin(&ep.url) },
            "secret": ep.secret.describe(),
            "principal": ep.principal,
            "filter": ep.filter,
            "enabled": ep.enabled,
            "status": status,
            "paused": paused,
            "restricted": ep.restricted,
            "version": ep.version,
            "created_by": ep.created_by,
            "created_at": ep.created_at,
            "may_manage": manage,
        });
        if let (Some(prev), Some(until)) = (&ep.previous_secret, ep.previous_until)
            && until > now
        {
            v["previous_secret"] = json!(prev.describe());
            v["previous_until"] = json!(until);
        }
        if manage {
            if let Some(n) = open {
                v["pending"] = json!(n);
            }
            if ep.filter.is_some()
                && let Ok(Some(c)) = self.ontology().store().consumer_get(&consumer_name(&ep.name))
            {
                v["cursor"] = json!(c.to_string());
            }
        }
        v
    }

    fn put_endpoint(&self, ctx: &ActionContext, ep: &Endpoint, action: &str, extra: Value) -> Result<()> {
        let mut payload = json!({
            "name": ep.name,
            "host": audit_host(&ep.url),
            "principal": ep.principal,
            "subscribed": ep.filter.is_some(),
            "enabled": ep.enabled,
            "restricted": ep.restricted,
            "version": ep.version,
        });
        if let (Value::Object(p), Value::Object(e)) = (&mut payload, extra) {
            p.extend(e);
        }
        let kv = vec![(NS_ENDPOINT.to_string(), ep.name.clone(), serde_json::to_value(ep)?)];
        self.ontology().commit_config(ctx, kv, ConfigEvent::new(action, Some(&ep.principal_id()), payload)).map(drop)
    }

    /// Start the endpoint's change subscription at the head of the feed.
    fn start_cursor(&self, name: &str) -> Result<()> {
        let store = self.ontology().store();
        if !store.keeps_outbox() || !store.keeps_changes() {
            return Ok(());
        }
        let head = self.ontology().change_head()?;
        store.consumer_set(&consumer_name(name), head, None)
    }

    /// The webhook actions ([`CALLS`]). Synchronous: the store calls block,
    /// so the caller runs this off the async workers.
    pub(crate) fn webhooks_call(&self, p: &Principal, action: &str, input: Value) -> Result<Value> {
        let onto = self.ontology();
        match action {
            "webhook.list" => {
                let ctx = self.ctx(p, None);
                admin_check(onto, &ctx, "list webhook endpoints")?;
                let (kill, ctl) = controls(onto)?;
                let open: std::collections::HashMap<String, u64> = if onto.store().keeps_outbox() {
                    onto.store().outbox_open_counts()?.into_iter().collect()
                } else {
                    Default::default()
                };
                let mut eps = Endpoint::list(onto)?;
                eps.sort_by(|a, b| a.name.cmp(&b.name));
                let items: Vec<Value> = eps
                    .iter()
                    .map(|ep| self.endpoint_view(p, ep, ctl.get(&ep.name), open.get(&ep.name).copied()))
                    .collect();
                Ok(json!({
                    "items": items,
                    "kill": kill,
                    "max_endpoints": self.config().webhooks.max_endpoints,
                    "note": DELIVERY_NOTE,
                }))
            }
            "webhook.add" => {
                let i: AddIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                admin_check(onto, &ctx, "add webhook endpoints")?;
                let name = i.name.trim().to_string();
                if !valid_name(&name) {
                    return Err(Error::invalid(format!(
                        "webhook.add: name '{name}' must be 1 to 40 of a-z, 0-9, _ and -"
                    )));
                }
                if name == KILL_KEY {
                    return Err(Error::invalid("webhook.add: kill is reserved (POST /v1/webhooks/kill is the switch)"));
                }
                let principal = match i.principal {
                    Some(sp) => sp.scope()?,
                    None => ScopePrincipal::new(vec![Role::Viewer], vec![ctx.workspace.clone()]),
                };
                let probe = Endpoint::bare(&name, principal.clone());
                dominance_check(onto, &ctx, action, &probe)?;
                if Endpoint::load(onto, &name)?.is_some() {
                    return Err(Error::Conflict(format!("webhook endpoint {name} exists (webhook.update changes it)")));
                }
                let max = self.config().webhooks.max_endpoints;
                if Endpoint::list(onto)?.len() >= max {
                    return Err(Error::Limited(format!(
                        "this tenant has {max} webhook endpoints, the most [webhooks] max_endpoints allows"
                    )));
                }
                let url = self.check_url_input(&i.url, i.restricted)?;
                let filter = i.filter.map(FilterIn::into_filter).transpose().map_err(|e| prefixed(action, e))?;
                let (secret, generated) = self.secret_input(&name, i.secret.as_deref())?;
                let ep = Endpoint {
                    name: name.clone(),
                    url,
                    secret,
                    previous_secret: None,
                    previous_until: None,
                    filter,
                    principal,
                    enabled: !i.disabled,
                    paused: None,
                    restricted: i.restricted,
                    version: 1,
                    created_by: p.id.clone(),
                    created_at: now_ms(),
                };
                if ep.filter.is_some() {
                    self.start_cursor(&name)?;
                }
                self.put_endpoint(&ctx, &ep, action, json!({}))?;
                let mut out = json!({
                    "endpoint": self.endpoint_view(p, &ep, None, None),
                    "note": DELIVERY_NOTE,
                });
                if let Some(s) = generated {
                    out["secret"] = json!(s);
                    out["secret_note"] = json!("the signing secret is shown once; store it with the receiver");
                }
                Ok(out)
            }
            "webhook.update" => {
                let i: UpdateIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                admin_check(onto, &ctx, "change webhook endpoints")?;
                let mut ep = load(onto, i.name.trim())?;
                dominance_check(onto, &ctx, action, &ep)?;
                let mut changed: Vec<&str> = Vec::new();
                // Once a project credential registered or changed it, it stays restricted.
                let restricted = ep.restricted || i.restricted;
                if restricted != ep.restricted {
                    ep.restricted = true;
                    changed.push("restricted");
                }
                if let Some(u) = &i.url {
                    ep.url = self.check_url_input(u, restricted)?;
                    changed.push("url");
                } else if restricted && !is_reference(&ep.url) {
                    // A restricted endpoint's URL is judged as one.
                    self.check_url_input(&ep.url.clone(), true)?;
                }
                if let Some(sp) = i.principal {
                    let sp = sp.scope()?;
                    let probe = Endpoint::bare(&ep.name, sp.clone());
                    dominance_check(onto, &ctx, action, &probe)?;
                    ep.principal = sp;
                    changed.push("principal");
                }
                let mut generated = None;
                if let Some(s) = &i.secret {
                    let (secret, g) = self.secret_input(&ep.name, Some(s))?;
                    ep.secret = secret;
                    ep.previous_secret = None;
                    ep.previous_until = None;
                    generated = g;
                    changed.push("secret");
                }
                let had_filter = ep.filter.is_some();
                if i.clear_filter && i.filter.is_some() {
                    return Err(Error::invalid("webhook.update: give filter or clear_filter, not both"));
                }
                if let Some(f) = i.filter {
                    ep.filter = Some(f.into_filter().map_err(|e| prefixed(action, e))?);
                    changed.push("filter");
                } else if i.clear_filter && had_filter {
                    ep.filter = None;
                    changed.push("filter");
                }
                let enabled = match (i.enabled, i.disabled) {
                    (Some(e), Some(d)) if e == d => {
                        return Err(Error::invalid("webhook.update: enabled and disabled contradict each other"));
                    }
                    (Some(e), _) => Some(e),
                    (None, Some(d)) => Some(!d),
                    (None, None) => None,
                };
                if let Some(e) = enabled
                    && e != ep.enabled
                {
                    ep.enabled = e;
                    changed.push(if e { "enabled" } else { "disabled" });
                }
                if changed.is_empty() {
                    return Err(Error::invalid(
                        "webhook.update: nothing to change (url, secret, filter, clear_filter, principal, enabled)",
                    ));
                }
                ep.version += 1;
                if !had_filter && ep.filter.is_some() {
                    self.start_cursor(&ep.name)?;
                }
                self.put_endpoint(&ctx, &ep, action, json!({ "changed": changed }))?;
                if ep.enabled {
                    self.wake_outbox();
                }
                let mut out = json!({ "endpoint": self.endpoint_view(p, &ep, None, None), "changed": changed });
                if let Some(s) = generated {
                    out["secret"] = json!(s);
                    out["secret_note"] = json!("the signing secret is shown once; store it with the receiver");
                }
                Ok(out)
            }
            "webhook.remove" => {
                let i: NameIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                admin_check(onto, &ctx, "remove webhook endpoints")?;
                let ep = load(onto, i.name.trim())?;
                dominance_check(onto, &ctx, action, &ep)?;
                let (_, ctl) = controls(onto)?;
                let mut kv = vec![(NS_ENDPOINT.to_string(), ep.name.clone(), Value::Null)];
                if let Some(c) = ctl.get(&ep.name) {
                    if c.breaker.is_some() {
                        kv.push((NS_CONTROL.to_string(), format!("{BREAKER_PREFIX}{}", ep.name), Value::Null));
                    }
                    if c.backlog.is_some() {
                        kv.push((NS_CONTROL.to_string(), format!("{BACKLOG_PREFIX}{}", ep.name), Value::Null));
                    }
                }
                let payload = json!({ "name": ep.name, "host": audit_host(&ep.url), "version": ep.version });
                onto.commit_config(&ctx, kv, ConfigEvent::new(action, Some(&ep.principal_id()), payload))?;
                Ok(json!({
                    "removed": ep.name,
                    "note": "rows not yet sent to it are cancelled when the worker reaches them",
                }))
            }
            "webhook.test" => {
                let i: NameIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                admin_check(onto, &ctx, "send test deliveries")?;
                let ep = load(onto, i.name.trim())?;
                dominance_check(onto, &ctx, action, &ep)?;
                let out = enqueue_test(onto, &ctx, &ep.name)?;
                self.wake_outbox();
                Ok(out)
            }
            "webhook.rotate_secret" => {
                let i: RotateIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                admin_check(onto, &ctx, "rotate webhook secrets")?;
                let mut ep = load(onto, i.name.trim())?;
                dominance_check(onto, &ctx, action, &ep)?;
                let keep = i.keep_secs.unwrap_or(ROTATION_KEEP_SECS);
                if keep > ROTATION_MAX_SECS {
                    return Err(Error::invalid(format!(
                        "webhook.rotate_secret: keep_secs is at most {ROTATION_MAX_SECS} (30 days)"
                    )));
                }
                let (secret, generated) = self.secret_input(&ep.name, i.secret.as_deref())?;
                let now = now_ms();
                let old = std::mem::replace(&mut ep.secret, secret);
                if keep > 0 {
                    ep.previous_secret = Some(old);
                    ep.previous_until = Some(now + keep as i64 * 1000);
                } else {
                    ep.previous_secret = None;
                    ep.previous_until = None;
                }
                if i.restricted {
                    ep.restricted = true;
                }
                ep.version += 1;
                self.put_endpoint(&ctx, &ep, action, json!({ "previous_until": ep.previous_until }))?;
                let mut out = json!({
                    "name": ep.name,
                    "secret_ref": ep.secret.describe(),
                    "previous_until": ep.previous_until,
                    "version": ep.version,
                    "note": "deliveries are signed with both secrets until previous_until",
                });
                if let Some(s) = generated {
                    out["secret"] = json!(s);
                }
                Ok(out)
            }
            "webhook.deliveries" => {
                let i: DeliveriesIn = parse(action, input)?;
                let ctx = self.ctx(p, None).with_intent(action);
                admin_check(onto, &ctx, "read webhook deliveries")?;
                let ep = load(onto, i.name.trim())?;
                dominance_check(onto, &ctx, action, &ep)?;
                if let Some(id) = i.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    let row =
                        onto.store().outbox_get(id)?.filter(|r| r.target == ep.name).ok_or_else(|| {
                            Error::not_found(format!("delivery {id} of webhook endpoint {}", ep.name))
                        })?;
                    let attempts = onto.store().outbox_attempts(id, i.limit.unwrap_or(50).clamp(1, 500))?;
                    return Ok(json!({ "row": row.listed(), "attempts": attempts, "note": DELIVERY_NOTE }));
                }
                let q = OutboxQuery {
                    target: Some(ep.name.clone()),
                    states: states(&i.states)?,
                    since: i.since,
                    limit: i.limit,
                    ..Default::default()
                };
                let items: Vec<_> = onto.store().outbox_list(&q)?.into_iter().map(|r| r.listed()).collect();
                Ok(json!({ "name": ep.name, "items": items, "note": DELIVERY_NOTE }))
            }
            "webhook.replay" => {
                let i: ReplayIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                admin_check(onto, &ctx, "replay webhook deliveries")?;
                let ep = load(onto, i.name.trim())?;
                dominance_check(onto, &ctx, action, &ep)?;
                let sel = match (i.ids.is_empty(), i.since) {
                    (false, None) => OutboxSelect::Ids(i.ids.clone()),
                    (true, Some(t)) => OutboxSelect::Since(t),
                    (false, Some(_)) => return Err(Error::invalid("webhook.replay: give ids or since, not both")),
                    (true, None) => return Err(Error::invalid("webhook.replay: give ids or since")),
                };
                let st = states(&i.states)?;
                let n = onto.store().outbox_reset(&ep.name, &sel, &st)?;
                let payload = json!({
                    "name": ep.name,
                    "ids": i.ids.len(),
                    "since": i.since,
                    "states": st,
                    "replayed": n,
                });
                onto.commit_config(&ctx, Vec::new(), ConfigEvent::new(action, Some(&ep.principal_id()), payload))?;
                if n > 0 {
                    self.wake_outbox();
                }
                Ok(json!({
                    "name": ep.name,
                    "replayed": n,
                    "note": "replayed rows keep their ids, so receivers that dedupe on the event id see them as \
                             repeats",
                }))
            }
            "webhook.resume" => {
                let i: NameIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                admin_check(onto, &ctx, "resume webhook endpoints")?;
                let mut ep = load(onto, i.name.trim())?;
                dominance_check(onto, &ctx, action, &ep)?;
                let (_, ctl) = controls(onto)?;
                let mut kv = Vec::new();
                let mut cleared: Vec<&str> = Vec::new();
                if ep.paused.take().is_some() {
                    // Operational state: the version (configuration) stays.
                    kv.push((NS_ENDPOINT.to_string(), ep.name.clone(), serde_json::to_value(&ep)?));
                    cleared.push("paused");
                }
                if let Some(c) = ctl.get(&ep.name) {
                    if c.breaker.as_ref().is_some_and(|b| b.is_open()) {
                        let closed = BreakerState { state: "closed".into(), ..Default::default() };
                        kv.push((
                            NS_CONTROL.to_string(),
                            format!("{BREAKER_PREFIX}{}", ep.name),
                            serde_json::to_value(closed)?,
                        ));
                        cleared.push("breaker");
                    }
                    if c.backlog.is_some() {
                        kv.push((NS_CONTROL.to_string(), format!("{BACKLOG_PREFIX}{}", ep.name), Value::Null));
                        cleared.push("backlog");
                    }
                }
                if !kv.is_empty() {
                    let payload = json!({ "name": ep.name, "cleared": cleared });
                    onto.commit_config(&ctx, kv, ConfigEvent::new(action, Some(&ep.principal_id()), payload))?;
                    self.wake_outbox();
                }
                let note = if ep.enabled {
                    "resumed; an endpoint still over its backlog cap is paused again by fan-out"
                } else {
                    "the endpoint is disabled: webhook.update with enabled: true sends to it again"
                };
                Ok(json!({ "name": ep.name, "cleared": cleared, "note": note }))
            }
            "webhook.kill" => {
                let i: KillIn = parse(action, input)?;
                let on = match (i.on, i.off) {
                    (Some(a), Some(b)) if a == b => {
                        return Err(Error::invalid("webhook.kill: on and off contradict each other"));
                    }
                    (Some(a), _) => a,
                    (None, Some(b)) => !b,
                    (None, None) => {
                        return Err(Error::invalid("webhook.kill: give on: true or on: false (or off: true)"));
                    }
                };
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                admin_check(onto, &ctx, "switch webhook deliveries")?;
                if !on {
                    // Switching deliveries back on resumes every endpoint.
                    for ep in Endpoint::list(onto)? {
                        dominance_check(onto, &ctx, action, &ep)?;
                    }
                }
                let v = json!({ "on": on, "by": p.id, "at": now_ms() });
                let kv = vec![(NS_CONTROL.to_string(), KILL_KEY.to_string(), v)];
                onto.commit_config(&ctx, kv, ConfigEvent::new(action, None, json!({ "on": on })))?;
                if !on {
                    self.wake_outbox();
                }
                Ok(json!({
                    "kill": killed(onto)?,
                    "note": if on {
                        "deliveries are held: rows wait in the outbox and change subscriptions keep their cursors"
                    } else {
                        "deliveries resume from the outbox"
                    },
                }))
            }
            "outbox.list" => {
                let i: OutboxIn = parse(action, input)?;
                let ctx = self.ctx(p, None);
                admin_check(onto, &ctx, "read the outbox")?;
                let q = OutboxQuery {
                    target: i.target.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
                    states: states(&i.states)?,
                    source: i.source,
                    receipt: i.receipt,
                    run: i.run,
                    since: i.since,
                    limit: i.limit,
                };
                let endpoints: std::collections::HashMap<String, Endpoint> =
                    Endpoint::list(onto)?.into_iter().map(|e| (e.name.clone(), e)).collect();
                let everywhere = p.workspaces.iter().any(|w| w == "*");
                let mut hidden = 0usize;
                let mut items = Vec::new();
                for r in onto.store().outbox_list(&q)? {
                    // Rows of endpoints the caller may not manage stay
                    // hidden (they show what the endpoint's principal sees);
                    // rows of removed targets only to tenant-wide admins.
                    let ok = match endpoints.get(&r.target) {
                        Some(ep) => dominates(p, &ep.principal),
                        None => everywhere,
                    };
                    if ok {
                        items.push(r.listed());
                    } else {
                        hidden += 1;
                    }
                }
                Ok(json!({ "items": items, "hidden": hidden, "note": DELIVERY_NOTE }))
            }
            other => Err(Error::invalid(format!("unknown action '{other}' (see system.actions)"))),
        }
    }
}

/// Name the action in an input error, as `parse` does.
fn prefixed(action: &str, e: Error) -> Error {
    match e {
        Error::Invalid(m) => Error::invalid(format!("{action}: {m}")),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_principals_need_roles_and_workspaces_and_no_service_roles() {
        let ok = PrincipalIn { roles: vec!["viewer".into()], workspaces: vec!["acme".into()] }.scope().unwrap();
        assert_eq!(ok.workspaces, vec!["acme"]);
        assert!(PrincipalIn { roles: vec![], workspaces: vec!["acme".into()] }.scope().is_err());
        assert!(PrincipalIn { roles: vec!["viewer".into()], workspaces: vec![" ".into()] }.scope().is_err());
        for bad in ["agent", "system"] {
            let e = PrincipalIn { roles: vec![bad.into()], workspaces: vec!["*".into()] }.scope().unwrap_err();
            assert!(e.to_string().contains("system or agent"), "{e}");
        }
    }

    #[test]
    fn origins_hide_paths_and_queries() {
        assert_eq!(url_origin("https://hooks.example.com/in?token=abc"), "https://hooks.example.com");
        assert_eq!(url_origin("http://127.0.0.1:9000/x"), "http://127.0.0.1:9000");
        assert_eq!(url_origin(" env:HOOK_URL "), "env:HOOK_URL");
        assert_eq!(audit_host("https://hooks.example.com/in?token=abc"), "hooks.example.com");
        assert_eq!(url_origin("::"), "(not a URL)");
    }
}
