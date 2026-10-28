//! Automation definitions (docs/design/EVENTS.md Part C): what an automation
//! is, how a definition is validated, and the closure an enablement binds to.
//!
//! * **Definitions are schema.** `[automations.<name>]` lives next to shapes
//!   and action types, is changed with `put_automation` / `delete_automation`
//!   through `schema.apply` or a proposal, and is validated in
//!   [`Schema::compile`] for structure and references only. Limits
//!   (`max_automations`) are enforced when a change is applied or proposed,
//!   never when a stored schema is loaded. Automation definitions are visible
//!   to everyone who can read the schema, so a definition may not hold
//!   secret references: it names endpoints and targets only by name.
//! * **Enablement is state.** Whether an automation runs is KV
//!   ([`NS_AUTOMATION_STATE`]), written by a person who may approve schema
//!   changes and dominates its `run_as`. Installing a pack or applying a
//!   schema never enables one.
//! * **The closure binds an enablement.** [`closure`] hashes the automation,
//!   every action type its effects name and the version and principal of
//!   every endpoint and writeback target those reach. A change to any of
//!   them makes an enabled automation inactive (`stale_definition`) until a
//!   person enables it again; lint shows which enabled automations a schema
//!   change would stop (`automation_closure_changed`).
//!
//! An automation runs as the service principal `automation:<name>` with the
//! roles and workspaces of its `run_as` (phase 1 declares these principals
//! locally, with an enablement that expires; see [`AutomationState`]).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::action_types::{ActionType, Check, CheckKind, placeholders, strings_in};
use crate::changes::{self, ChangeFilter, ChangeOp, ChangeRecord, FieldMode};
use crate::policy::{Principal, Role, ScopePrincipal};
use crate::schema::Schema;
use crate::store::Store;
use crate::{Error, Result};

/// KV namespace of each automation's enablement (key: the automation name).
pub const NS_AUTOMATION_STATE: &str = "automation.state";
/// KV namespace of the tenant's automation switches (`kill`).
pub const NS_AUTOMATION_CONTROL: &str = "automation.control";
/// KV namespace of writeback targets (Part D; key: the target name).
pub const NS_WRITEBACK_TARGET: &str = "writeback.target";
/// The principal id prefix of automations: `automation:<name>`.
pub const PRINCIPAL_PREFIX: &str = "automation:";
/// Most effects one automation may have.
pub const MAX_EFFECTS: usize = 16;
/// Most conditions one automation may have.
pub const MAX_CONDITIONS: usize = 16;
/// Longest webhook event type, in bytes.
pub const MAX_EVENT_LEN: usize = 128;
/// Roots a template in an automation may name.
pub const TEMPLATE_ROOTS: &[&str] = &["trigger", "subject", "now", "automation", "run"];
/// The only parameter a condition may read: the entity the change is about.
pub const SUBJECT: &str = "subject";
/// Change ops a trigger may name: records about entities and links.
pub const TRIGGER_OPS: [ChangeOp; 6] = [
    ChangeOp::EntityCreate,
    ChangeOp::EntityUpdate,
    ChangeOp::EntityDelete,
    ChangeOp::LinkCreate,
    ChangeOp::LinkUpdate,
    ChangeOp::LinkDelete,
];
/// What `automation.get` says about evaluation (C.3).
pub const MEANS: &str = "Trigger matching uses the change as committed; conditions, subject.* and plans are \
evaluated against the graph at the run's commit. Automation definitions are visible to everyone who can read the \
schema.";

fn is_false(b: &bool) -> bool {
    !*b
}

/// `live` runs; `shadow` records what it would have done (`would_run`) and
/// writes nothing else.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Live,
    Shadow,
}

impl Mode {
    fn is_live(&self) -> bool {
        *self == Mode::Live
    }
}

/// Per-automation rate limits; unset values take the process defaults
/// (`[automation] default_rate_per_minute`, `default_per_subject_per_hour`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_minute: Option<u32>,
    /// Runs per `(automation, subject)` per hour: the backstop against
    /// loops that leave onto and come back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_subject_per_hour: Option<u32>,
}

impl Rate {
    fn is_unset(&self) -> bool {
        self.per_minute.is_none() && self.per_subject_per_hour.is_none()
    }
}

/// `state = { from = [...], to = [...] }`: the entity's state moved from one
/// of `from` (any when empty) to one of `to` (any when empty).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateGuard {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
}

/// A change trigger: the records of the change feed that start a run.
/// Dimensions combine with AND, values within one with OR; an empty list
/// does not narrow. Unlike change filters, records written by schema
/// migrations are skipped unless `include_migrations` is set, and `gap`
/// records never trigger runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChangeTrigger {
    /// Entity kinds or interfaces (a link matches through either end).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    /// Of [`TRIGGER_OPS`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<ChangeOp>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
    /// Relations (canonical, inverse or `^rel`); when set, only link records match.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rels: Vec<String>,
    /// Updates whose changed fields include any of these.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub props: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<StateGuard>,
    /// Globs on the cause's source; `"!x"` excludes (`"!connector:*"`,
    /// `"!automation:*"`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub include_migrations: bool,
}

impl ChangeTrigger {
    /// The change filter of the trigger's dimensions (its state guard is
    /// checked separately, see [`Self::matches`]).
    pub fn filter(&self) -> ChangeFilter {
        ChangeFilter {
            kinds: self.kinds.clone(),
            ops: if self.ops.is_empty() { TRIGGER_OPS.to_vec() } else { self.ops.clone() },
            workspaces: self.workspaces.clone(),
            ids: Vec::new(),
            rels: self.rels.clone(),
            props: self.props.clone(),
            sources: self.sources.clone(),
            include_migrations: self.include_migrations,
            fields: FieldMode::Full,
        }
    }

    /// Does the committed record `r` start a run? `gap` records never do;
    /// migration records only with `include_migrations`.
    pub fn matches(&self, schema: &Schema, r: &ChangeRecord) -> bool {
        if !TRIGGER_OPS.contains(&r.op) || (r.cause.migration && !self.include_migrations) {
            return false;
        }
        changes::matches(&self.filter(), schema, r) && self.state_ok(r)
    }

    fn state_ok(&self, r: &ChangeRecord) -> bool {
        let Some(g) = &self.state else { return true };
        let Some(s) = &r.state else { return false };
        let within =
            |list: &[String], v: &Option<String>| list.is_empty() || v.as_ref().is_some_and(|x| list.contains(x));
        within(&g.from, &s.from) && within(&g.to, &s.to)
    }
}

/// What starts a run. Phase 1 has change triggers only; later trigger kinds
/// add variants (`on = "..."`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Trigger {
    Change(ChangeTrigger),
}

impl Default for Trigger {
    fn default() -> Self {
        Trigger::Change(ChangeTrigger::default())
    }
}

/// Run one governed action type (planned as the run principal; refused when
/// its plan needs confirmation).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionEffect {
    #[serde(rename = "type")]
    pub ty: String,
    /// Parameter templates (`"{trigger.id}"`), rendered strictly.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub params: Map<String, Value>,
}

/// Enqueue one signed delivery to a registered endpoint.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookEffect {
    /// The endpoint's name (its URL and secret stay in KV).
    pub endpoint: String,
    /// The CloudEvent type suffix (`issue.escalated`).
    pub event: String,
    /// A JSON template, rendered strictly as the run principal.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
}

/// One effect: exactly one of `action` or `webhook`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoEffect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ActionEffect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookEffect>,
}

/// `[automations.<name>]`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Automation {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Mode::is_live")]
    pub mode: Mode,
    /// What the run may do; its id is `automation:<name>`.
    pub run_as: ScopePrincipal,
    /// Cascade depth limit, at most `[automation] max_depth` (the smaller wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u8>,
    #[serde(default, skip_serializing_if = "Rate::is_unset")]
    pub rate: Rate,
    /// Run on changes this automation's own runs caused.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_self: bool,
    pub trigger: Trigger,
    /// All must pass, evaluated on the graph at the run's commit with the
    /// parameter `subject`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Check>,
    /// At most one `action` effect; any number of `webhook` effects.
    #[serde(default)]
    pub effects: Vec<AutoEffect>,
}

/// `[a-z0-9_-]`, 1 to 40 characters (webhook endpoint and writeback target names).
pub fn valid_outbound_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn valid_event(event: &str) -> bool {
    let mut chars = event.chars();
    event.len() <= MAX_EVENT_LEN
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'))
}

/// A string that looks like a secret reference (`env:`, `file:`, `sealed:`).
fn is_secret_ref(s: &str) -> bool {
    let s = s.trim_start().to_ascii_lowercase();
    ["env:", "file:", "sealed:"].iter().any(|p| s.starts_with(p))
}

/// The JSON paths of strings in `v` that look like secret references (paths
/// only: never the value).
fn secret_paths(v: &Value, path: &str, out: &mut Vec<String>) {
    match v {
        Value::String(s) if is_secret_ref(s) => out.push(if path.is_empty() { ".".into() } else { path.into() }),
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                secret_paths(x, &format!("{path}[{i}]"), out);
            }
        }
        Value::Object(m) => {
            for (k, x) in m {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                if is_secret_ref(k) {
                    out.push(p.clone());
                }
                secret_paths(x, &p, out);
            }
        }
        _ => {}
    }
}

/// Rewrite every string in `v` (values only) with `f`.
fn rewrite_strings(v: &mut Value, f: &dyn Fn(&str) -> String) {
    match v {
        Value::String(s) => *s = f(s),
        Value::Array(a) => a.iter_mut().for_each(|x| rewrite_strings(x, f)),
        Value::Object(m) => m.values_mut().for_each(|x| rewrite_strings(x, f)),
        _ => {}
    }
}

impl Automation {
    /// The service principal a run acts as.
    pub fn principal(&self, name: &str) -> Principal {
        let mut p = self.run_as.principal(format!("{PRINCIPAL_PREFIX}{name}"));
        p.via = Some(format!("{PRINCIPAL_PREFIX}{name}"));
        p
    }

    /// The change trigger.
    pub fn change_trigger(&self) -> &ChangeTrigger {
        match &self.trigger {
            Trigger::Change(t) => t,
        }
    }

    /// The action effect, when there is one.
    pub fn action(&self) -> Option<&ActionEffect> {
        self.effects.iter().find_map(|e| e.action.as_ref())
    }

    /// Endpoint names the webhook effects name, sorted and distinct.
    pub fn endpoints(&self) -> Vec<String> {
        let set: BTreeSet<String> =
            self.effects.iter().filter_map(|e| e.webhook.as_ref()).map(|w| w.endpoint.clone()).collect();
        set.into_iter().collect()
    }

    /// The depth a run may reach: the smaller of its own `max_depth` and
    /// the process limit.
    pub fn depth_limit(&self, process_max: u8) -> u8 {
        self.max_depth.map_or(process_max, |d| d.min(process_max))
    }

    /// Does the committed record `r` start a run of automation `name`?
    /// The trigger must match, and (unless `allow_self`) the change must
    /// not come from this automation's own runs. Depth is checked by the
    /// caller with [`Self::depth_limit`] (`r.cause.depth + 1` is the run's).
    pub fn admits(&self, name: &str, schema: &Schema, r: &ChangeRecord) -> bool {
        if !self.allow_self && r.cause.automation.as_deref() == Some(name) {
            return false;
        }
        self.change_trigger().matches(schema, r)
    }

    /// Structure and references only (no limits, no process configuration):
    /// called by [`Schema::compile`], so a stored schema keeps opening when
    /// configuration changes. Errors name a place, never a secret value.
    pub fn validate(&self, name: &str, schema: &Schema) -> std::result::Result<(), String> {
        if !crate::live_schema::valid_name(name) {
            return Err(format!("name '{name}' must match [a-z][a-z0-9_]* (at most 64 characters)"));
        }
        let mut secrets = Vec::new();
        secret_paths(&serde_json::to_value(self).map_err(|e| e.to_string())?, "", &mut secrets);
        if !secrets.is_empty() {
            return Err(format!(
                "a definition may not hold secret references (env:, file:, sealed:), found at {}; it names endpoints \
                 and targets by name, and their URLs and secrets stay in their registries",
                secrets.join(", ")
            ));
        }
        self.validate_run_as()?;
        if self.max_depth == Some(0) {
            return Err("max_depth must be at least 1".into());
        }
        if self.rate.per_minute == Some(0) || self.rate.per_subject_per_hour == Some(0) {
            return Err("rate limits must be at least 1".into());
        }
        self.validate_trigger(schema)?;
        if self.conditions.len() > MAX_CONDITIONS {
            return Err(format!("at most {MAX_CONDITIONS} conditions ({} given)", self.conditions.len()));
        }
        for (i, c) in self.conditions.iter().enumerate() {
            for p in c.param.iter().chain(c.other.iter()) {
                if p != SUBJECT {
                    return Err(format!("condition {i}: reads parameter {p}; conditions read only `subject`"));
                }
            }
            if matches!(c.check, CheckKind::HasLink | CheckKind::NoLink) {
                let rel = c.rel.as_deref().ok_or_else(|| format!("condition {i}: has_link/no_link need `rel`"))?;
                if schema.resolve_rel(rel).is_none() {
                    return Err(format!("condition {i}: unknown relation {rel}"));
                }
            }
        }
        self.validate_effects(schema)?;
        let mut templates = Vec::new();
        for c in &self.conditions {
            for v in c.value.iter() {
                strings_in(v, &mut templates);
            }
            templates.extend(c.to.iter().cloned());
        }
        for e in &self.effects {
            if let Some(a) = &e.action {
                strings_in(&Value::Object(a.params.clone()), &mut templates);
            }
            if let Some(w) = &e.webhook {
                strings_in(&w.payload, &mut templates);
            }
        }
        for t in &templates {
            for root in placeholders(t) {
                if !TEMPLATE_ROOTS.contains(&root.as_str()) {
                    return Err(format!(
                        "template names {{{root}}}; templates name only {}",
                        TEMPLATE_ROOTS.iter().map(|r| format!("{r}.*")).collect::<Vec<_>>().join(", ")
                    ));
                }
            }
        }
        Ok(())
    }

    /// A kind was renamed: trigger kinds follow it.
    pub(crate) fn rename_kind(&mut self, from: &str, to: &str) {
        let Trigger::Change(t) = &mut self.trigger;
        for k in t.kinds.iter_mut().filter(|k| *k == from) {
            *k = to.to_string();
        }
    }

    /// A relation was renamed: `step` rewrites one relation name (also
    /// `^rel`); trigger rels and condition rels follow.
    pub(crate) fn rename_rels(&mut self, step: &dyn Fn(&mut String)) {
        let Trigger::Change(t) = &mut self.trigger;
        t.rels.iter_mut().for_each(step);
        for c in &mut self.conditions {
            if let Some(r) = &mut c.rel {
                step(r);
            }
        }
    }

    /// Property `from` of the object types `kinds` became `to`. When the
    /// trigger names one of `kinds` (or no other type still declares
    /// `from`), the trigger's `props`, the conditions' `prop` and
    /// `{subject.<from>}` / `{subject.props.<from>}` templates follow.
    pub(crate) fn rename_prop(&mut self, kinds: &[String], still_declared: bool, from: &str, to: &str) {
        let Trigger::Change(t) = &mut self.trigger;
        if still_declared && !t.kinds.iter().any(|k| kinds.contains(k)) {
            return;
        }
        for p in t.props.iter_mut().filter(|p| *p == from) {
            *p = to.to_string();
        }
        for c in &mut self.conditions {
            if c.prop.as_deref() == Some(from) {
                c.prop = Some(to.to_string());
            }
        }
        let pairs = [
            (format!("{{subject.{from}}}"), format!("{{subject.{to}}}")),
            (format!("{{subject.props.{from}}}"), format!("{{subject.props.{to}}}")),
        ];
        let fix = |v: &mut Value| {
            rewrite_strings(v, &|s: &str| pairs.iter().fold(s.to_string(), |acc, (a, b)| acc.replace(a, b)))
        };
        for c in &mut self.conditions {
            if let Some(v) = &mut c.value {
                fix(v);
            }
            if let Some(x) = &mut c.to {
                let mut v = Value::String(x.clone());
                fix(&mut v);
                *x = v.as_str().unwrap_or_default().to_string();
            }
        }
        for e in &mut self.effects {
            if let Some(a) = &mut e.action {
                a.params.values_mut().for_each(fix);
            }
            if let Some(w) = &mut e.webhook {
                fix(&mut w.payload);
            }
        }
    }

    fn validate_run_as(&self) -> std::result::Result<(), String> {
        let r = &self.run_as;
        if r.roles.is_empty() {
            return Err("run_as needs at least one role".into());
        }
        if let Some(bad) = r.roles.iter().find(|x| matches!(x, Role::System | Role::Agent)) {
            return Err(format!("run_as may not hold the {bad} role"));
        }
        if r.workspaces.is_empty() || r.workspaces.iter().any(|w| w.trim().is_empty()) {
            return Err("run_as needs workspaces (names, or \"*\" for all)".into());
        }
        Ok(())
    }

    fn validate_trigger(&self, schema: &Schema) -> std::result::Result<(), String> {
        let t = self.change_trigger();
        if let Some(op) = t.ops.iter().find(|op| !TRIGGER_OPS.contains(op)) {
            return Err(format!(
                "trigger: op {op} cannot start a run (one of {})",
                TRIGGER_OPS.iter().map(|o| o.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        t.filter().check().map_err(|e| format!("trigger: {}", e.to_string().trim_start_matches("invalid: ")))?;
        for k in &t.kinds {
            if !schema.is_type_or_interface(k) {
                return Err(format!("trigger: unknown kind {k}"));
            }
        }
        for r in &t.rels {
            if schema.resolve_rel(r).is_none() {
                return Err(format!("trigger: unknown relation {r}"));
            }
        }
        if let Some(g) = &t.state {
            if g.from.is_empty() && g.to.is_empty() {
                return Err("trigger: state needs from or to".into());
            }
            if t.ops.iter().any(|op| !matches!(op, ChangeOp::EntityCreate | ChangeOp::EntityUpdate)) {
                return Err("trigger: a state guard matches only entity.create and entity.update".into());
            }
            if !t.rels.is_empty() {
                return Err("trigger: a state guard cannot be combined with rels (link records carry no state)".into());
            }
        }
        if let Some(s) = t.sources.iter().find(|s| s.trim_start_matches('!').trim().is_empty()) {
            return Err(format!("trigger: empty source glob '{s}'"));
        }
        Ok(())
    }

    fn validate_effects(&self, schema: &Schema) -> std::result::Result<(), String> {
        if self.effects.is_empty() {
            return Err("needs at least one effect".into());
        }
        if self.effects.len() > MAX_EFFECTS {
            return Err(format!("at most {MAX_EFFECTS} effects ({} given)", self.effects.len()));
        }
        let mut actions = 0;
        for (i, e) in self.effects.iter().enumerate() {
            match (&e.action, &e.webhook) {
                (Some(a), None) => {
                    actions += 1;
                    if !schema.action_types.contains_key(&a.ty) {
                        return Err(format!("effect {i}: unknown action type {}", a.ty));
                    }
                }
                (None, Some(w)) => {
                    if !valid_outbound_name(&w.endpoint) {
                        return Err(format!(
                            "effect {i}: endpoint name '{}' must be 1 to 40 of [a-z0-9_-]",
                            w.endpoint
                        ));
                    }
                    if !valid_event(&w.event) {
                        return Err(format!(
                            "effect {i}: event '{}' must match [a-z][a-z0-9_.-]* (at most {MAX_EVENT_LEN} bytes)",
                            w.event
                        ));
                    }
                }
                _ => return Err(format!("effect {i}: exactly one of action, webhook")),
            }
        }
        if actions > 1 {
            return Err(format!("at most one action effect ({actions} given)"));
        }
        Ok(())
    }
}

// ── closure ──────────────────────────────────────────────────────────────

/// A webhook endpoint or writeback target as a closure sees it: its name,
/// configuration version and principal (the records live in onto-intelli KV).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundRef {
    pub name: String,
    pub version: u64,
    pub principal: ScopePrincipal,
}

/// The hash an enablement binds to, with the hash of each item it covers:
/// `automation:<name>`, `action_type:<name>`, `endpoint:<name>`,
/// `target:<name>`. A reached item that does not exist hashes as `missing`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closure {
    /// SHA-256 (hex) over the canonical JSON of `items`.
    pub hash: String,
    pub items: BTreeMap<String, String>,
}

impl Closure {
    /// Items that differ between `self` (enabled) and `now`, sorted: changed,
    /// added and removed alike.
    pub fn changed(&self, now: &Closure) -> Vec<String> {
        let keys: BTreeSet<&String> = self.items.keys().chain(now.items.keys()).collect();
        keys.into_iter().filter(|k| self.items.get(*k) != now.items.get(*k)).cloned().collect()
    }
}

/// The value hash of a missing reached item.
pub const MISSING: &str = "missing";

/// JSON with object keys sorted at every level, written compactly: field
/// order in TOML or JSON never changes a hash.
pub fn canonical_json(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for k in keys {
                    out.insert(k.clone(), sorted(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sorted(v)).unwrap_or_default()
}

fn sha256_hex(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_of<T: Serialize>(v: &T) -> String {
    sha256_hex(&canonical_json(&serde_json::to_value(v).unwrap_or(Value::Null)))
}

/// Writeback targets an action type's effects name (`effects[].writeback.target`).
fn reached_targets(at: &ActionType) -> Vec<String> {
    targets_in(&serde_json::to_value(at).unwrap_or(Value::Null))
}

/// `effects[].writeback.target` of an action type document.
fn targets_in(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v
        .get("effects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.get("writeback")?.get("target")?.as_str().map(str::to_string))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The closure of automation `name` defined as `def` under `schema`, given
/// the registered endpoints and writeback targets (only those it reaches
/// count): SHA-256 over the canonical JSON of the automation, every action
/// type its effects name (effects, roles, risk, preconditions, everything
/// the action type declares), and the `version` and principal of every
/// endpoint and target those reach.
pub fn closure(
    schema: &Schema,
    name: &str,
    def: &Automation,
    endpoints: &[OutboundRef],
    targets: &[OutboundRef],
) -> Closure {
    let mut items = BTreeMap::new();
    items.insert(format!("automation:{name}"), hash_of(&json!({ "name": name, "def": def })));
    let outbound = |list: &[OutboundRef], n: &str| -> String {
        list.iter().find(|r| r.name == n).map_or_else(
            || MISSING.to_string(),
            |r| hash_of(&json!({ "version": r.version, "principal": r.principal })),
        )
    };
    let mut reached_targets_all = BTreeSet::new();
    for e in &def.effects {
        if let Some(a) = &e.action {
            let value = match schema.action_types.get(&a.ty) {
                Some(at) => {
                    reached_targets_all.extend(reached_targets(at));
                    hash_of(at)
                }
                None => MISSING.to_string(),
            };
            items.insert(format!("action_type:{}", a.ty), value);
        }
    }
    for ep in def.endpoints() {
        items.insert(format!("endpoint:{ep}"), outbound(endpoints, &ep));
    }
    for t in reached_targets_all {
        items.insert(format!("target:{t}"), outbound(targets, &t));
    }
    Closure { hash: hash_of(&items), items }
}

// ── run budgets ──────────────────────────────────────────────────────────

/// Which budget a trigger was over (C.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Over {
    /// The tenant's runs per minute (`[automation] max_runs_per_minute`).
    Tenant,
    /// The automation's runs per minute (`rate.per_minute`).
    Automation,
    /// Runs per `(automation, subject)` per hour (`rate.per_subject_per_hour`).
    Subject,
}

impl Over {
    /// The reason a `skipped` run row records.
    pub fn reason(self, limit: u32) -> String {
        match self {
            Over::Tenant => {
                format!("rate_limited: the tenant's {limit} runs per minute ([automation] max_runs_per_minute)")
            }
            Over::Automation => format!("rate_limited: the automation's {limit} runs per minute (rate.per_minute)"),
            Over::Subject => {
                format!("subject_budget: {limit} runs per subject per hour (rate.per_subject_per_hour)")
            }
        }
    }
}

/// The limits one trigger is checked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub tenant_per_minute: u32,
    pub per_minute: u32,
    pub per_subject_per_hour: u32,
}

impl Rate {
    /// This automation's limits, with the process defaults for unset values.
    pub fn limits(&self, tenant_per_minute: u32, default_per_minute: u32, default_per_subject_per_hour: u32) -> Limits {
        Limits {
            tenant_per_minute,
            per_minute: self.per_minute.unwrap_or(default_per_minute),
            per_subject_per_hour: self.per_subject_per_hour.unwrap_or(default_per_subject_per_hour),
        }
    }
}

/// Sliding-window run budgets of the automation consumer: runs per tenant
/// per minute, per automation per minute, and per `(automation, subject)`
/// per hour (the backstop against loops that leave onto and come back). Kept
/// in the consumer's memory, so they are approximate across lease handoffs:
/// a new holder starts with empty windows.
#[derive(Debug, Default)]
pub struct Budget {
    windows: std::collections::HashMap<String, std::collections::VecDeque<i64>>,
}

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;

impl Budget {
    fn used(&mut self, key: &str, now: i64, window: i64) -> usize {
        match self.windows.get_mut(key) {
            Some(q) => {
                while q.front().is_some_and(|t| *t <= now - window) {
                    q.pop_front();
                }
                q.len()
            }
            None => 0,
        }
    }

    /// Admit one run of automation `name` about `subject` at `now` (ms), or
    /// say which budget it is over. An admitted run counts against all three.
    pub fn admit(&mut self, now: i64, name: &str, subject: &str, l: &Limits) -> std::result::Result<(), (Over, u32)> {
        let tenant = "tenant".to_string();
        let auto = format!("automation:{name}");
        let subj = format!("subject:{name}:{subject}");
        if self.used(&tenant, now, MINUTE_MS) >= l.tenant_per_minute as usize {
            return Err((Over::Tenant, l.tenant_per_minute));
        }
        if self.used(&auto, now, MINUTE_MS) >= l.per_minute as usize {
            return Err((Over::Automation, l.per_minute));
        }
        if self.used(&subj, now, HOUR_MS) >= l.per_subject_per_hour as usize {
            return Err((Over::Subject, l.per_subject_per_hour));
        }
        for k in [tenant, auto, subj] {
            self.windows.entry(k).or_default().push_back(now);
        }
        Ok(())
    }

    /// Forget windows with nothing in them (called now and then).
    pub fn trim(&mut self, now: i64) {
        self.windows.retain(|k, q| {
            let window = if k.starts_with("subject:") { HOUR_MS } else { MINUTE_MS };
            while q.front().is_some_and(|t| *t <= now - window) {
                q.pop_front();
            }
            !q.is_empty()
        });
    }
}

// ── enablement state ─────────────────────────────────────────────────────

/// Why an enabled automation does not run now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    pub reason: String,
    #[serde(default)]
    pub since: i64,
}

/// KV `automation.state/<name>`: one automation's enablement. The closure
/// it binds to is stored with its per-item hashes, so a mismatch names what
/// changed. Enablement expires (`[automation] enable_ttl_days`): onto cannot
/// tell when the enabler loses the approver role, so a person re-enables.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationState {
    pub enabled: bool,
    #[serde(default)]
    pub closure_hash: String,
    #[serde(default)]
    pub closure: BTreeMap<String, String>,
    #[serde(default)]
    pub schema_version: u64,
    #[serde(default)]
    pub enabled_by: String,
    #[serde(default)]
    pub enabled_at: i64,
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<Paused>,
    /// The head of the change feed when it was enabled: runs start with the
    /// changes after it, never with what happened before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<changes::Cursor>,
}

impl AutomationState {
    /// The closure this enablement binds to.
    pub fn bound(&self) -> Closure {
        Closure { hash: self.closure_hash.clone(), items: self.closure.clone() }
    }
}

/// Enablements and the outbound configuration closures read: what lint and
/// schema plans need to tell which enabled automations a schema would stop.
#[derive(Clone, Debug, Default)]
pub struct Enablements {
    pub states: BTreeMap<String, AutomationState>,
    pub endpoints: Vec<OutboundRef>,
    pub targets: Vec<OutboundRef>,
}

/// `(name, version, principal)` of the records in KV namespace `ns`
/// (webhook endpoints, writeback targets). A writeback target has no
/// principal of its own: it acts as `{admin, workspaces}` (who may add or
/// remove it). Records with neither are skipped.
pub fn outbound_refs(store: &dyn Store, ns: &str) -> Result<Vec<OutboundRef>> {
    Ok(store
        .kv_scan(ns, "")?
        .into_iter()
        .filter_map(|(name, v)| {
            let principal: ScopePrincipal = match v.get("principal") {
                Some(p) => serde_json::from_value(p.clone()).ok()?,
                None => {
                    let ws: Vec<String> = serde_json::from_value(v.get("workspaces")?.clone()).ok()?;
                    ScopePrincipal::new(vec![crate::policy::Role::Admin], ws)
                }
            };
            Some(OutboundRef { name, version: v.get("version").and_then(Value::as_u64).unwrap_or(0), principal })
        })
        .collect())
}

impl Enablements {
    /// Read from a tenant's store. Unreadable state records are skipped.
    pub fn load(store: &dyn Store) -> Result<Self> {
        let states = store
            .kv_scan(NS_AUTOMATION_STATE, "")?
            .into_iter()
            .filter_map(|(k, v)| serde_json::from_value::<AutomationState>(v).ok().map(|s| (k, s)))
            .collect();
        Ok(Self {
            states,
            endpoints: outbound_refs(store, crate::outbox::NS_WEBHOOK_ENDPOINT)?,
            targets: outbound_refs(store, NS_WRITEBACK_TARGET)?,
        })
    }

    /// Enabled automations whose closure under `schema` differs from the one
    /// they were enabled with, by name, with the items that differ (an
    /// automation the schema no longer defines lists `automation:<name>`).
    pub fn stale(&self, schema: &Schema) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::new();
        for (name, st) in self.states.iter().filter(|(_, s)| s.enabled) {
            let changed = match schema.automations.get(name) {
                Some(def) => st.bound().changed(&closure(schema, name, def, &self.endpoints, &self.targets)),
                None => vec![format!("automation:{name}")],
            };
            if !changed.is_empty() {
                out.insert(name.clone(), changed);
            }
        }
        out
    }
}

/// Refuse a change that would leave more than `max` automations, unless it
/// does not add any (lowering the limit never stops a stored schema from
/// changing elsewhere).
pub fn check_limit(base: &Schema, new: &Schema, max: usize) -> Result<()> {
    let n = new.automations.len();
    if n > max && n > base.automations.len() {
        return Err(Error::invalid(format!(
            "the schema would hold {n} automations; the limit is {max} ([automation] max_automations)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{Cause, StateMove};

    fn schema() -> Schema {
        let mut s = Schema::enterprise();
        s.extend_toml(
            r#"
[action_types.escalate_issue]
description = "Escalate an issue."
risk = "low"
parameters = { issue = { type = "entity", kinds = ["issue"], required = true }, reason = { type = "string" } }
[[action_types.escalate_issue.effects]]
patch = { id = "{issue}", props = { escalated = true } }
"#,
        )
        .unwrap();
        s
    }

    const EXAMPLE: &str = r#"
[automations.escalate_blocked_p1]
description = "Escalate a high-priority issue when it becomes blocked."
mode = "live"
run_as = { roles = ["member", "triager"], workspaces = ["acme"] }
max_depth = 2
rate = { per_minute = 30, per_subject_per_hour = 10 }
allow_self = false

[automations.escalate_blocked_p1.trigger]
on = "change"
kinds = ["issue"]
ops = ["entity.update"]
state = { from = ["triaged", "in_progress"], to = ["blocked"] }
sources = ["!connector:*"]
workspaces = ["acme"]

[[automations.escalate_blocked_p1.conditions]]
check = "prop_eq"
param = "subject"
prop = "priority"
value = "high"

[[automations.escalate_blocked_p1.effects]]
action = { type = "escalate_issue", params = { issue = "{trigger.id}", reason = "blocked from {trigger.state.from}" } }

[[automations.escalate_blocked_p1.effects]]
webhook = { endpoint = "ops_pager", event = "issue.escalated", payload = { id = "{trigger.id}", title = "{subject.title}" } }
"#;

    fn with_example() -> Schema {
        let mut s = schema();
        s.extend_toml(EXAMPLE).unwrap();
        s
    }

    fn def(v: Value) -> Automation {
        serde_json::from_value(v).unwrap()
    }

    fn base() -> Value {
        json!({
            "run_as": { "roles": ["member"], "workspaces": ["acme"] },
            "trigger": { "on": "change", "kinds": ["issue"], "ops": ["entity.update"] },
            "effects": [{ "action": { "type": "escalate_issue", "params": { "issue": "{trigger.id}" } } }]
        })
    }

    /// `v` with the value at the dotted `path` (numbers index arrays) set to `x`.
    fn with(mut v: Value, path: &str, x: Value) -> Value {
        let pointer = format!("/{}", path.replace('.', "/"));
        let (parent, last) = pointer.rsplit_once('/').unwrap();
        let slot = if parent.is_empty() { &mut v } else { v.pointer_mut(parent).unwrap() };
        slot.as_object_mut().unwrap().insert(last.to_string(), x);
        v
    }

    fn invalid(v: Value) -> String {
        def(v).validate("a", &schema()).unwrap_err()
    }

    #[test]
    fn the_design_example_parses_and_validates() {
        let s = with_example();
        let a = &s.automations["escalate_blocked_p1"];
        assert_eq!(a.mode, Mode::Live);
        assert_eq!(a.max_depth, Some(2));
        assert_eq!(a.rate.per_minute, Some(30));
        let t = a.change_trigger();
        assert_eq!(t.ops, [ChangeOp::EntityUpdate]);
        assert_eq!(t.state.as_ref().unwrap().to, ["blocked"]);
        assert!(!t.include_migrations, "automations skip migrations by default");
        assert_eq!(a.action().unwrap().ty, "escalate_issue");
        assert_eq!(a.endpoints(), ["ops_pager"]);
        let p = a.principal("escalate_blocked_p1");
        assert_eq!(p.id, "automation:escalate_blocked_p1");
        assert_eq!(p.via.as_deref(), Some("automation:escalate_blocked_p1"));
        assert!(crate::policy::is_reserved_id(&p.id));
        // Round trip through JSON (the stored schema document) keeps it.
        let back = Schema::from_json(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(hash_of(&back.automations["escalate_blocked_p1"]), hash_of(a));
    }

    #[test]
    fn validation_refuses_bad_definitions() {
        assert!(def(base()).validate("a", &schema()).is_ok());
        assert!(def(base()).validate("Bad-Name", &schema()).unwrap_err().contains("must match"));
        let cases: Vec<(Value, &str)> = vec![
            (with(base(), "effects", json!([])), "at least one effect"),
            (
                with(
                    base(),
                    "effects",
                    json!([
                        { "action": { "type": "escalate_issue" } },
                        { "action": { "type": "escalate_issue" } }
                    ]),
                ),
                "at most one action effect",
            ),
            (with(base(), "effects", json!([{}])), "exactly one of action, webhook"),
            (with(base(), "effects", json!([{ "action": { "type": "nope" } }])), "unknown action type nope"),
            (
                with(base(), "effects", json!([{ "webhook": { "endpoint": "Ops Pager", "event": "x" } }])),
                "endpoint name",
            ),
            (with(base(), "effects", json!([{ "webhook": { "endpoint": "ops", "event": "Bad Event" } }])), "event"),
            (with(base(), "trigger.kinds", json!(["spaceship"])), "unknown kind spaceship"),
            (with(base(), "trigger.rels", json!(["frobs"])), "unknown relation frobs"),
            (with(base(), "trigger.ops", json!(["gap"])), "cannot start a run"),
            (with(base(), "trigger.ops", json!(["entity.hidden"])), "cannot start a run"),
            (with(base(), "trigger.ops", json!(["schema.change"])), "cannot start a run"),
            (with(base(), "trigger.state", json!({})), "state needs from or to"),
            (
                with(
                    with(base(), "trigger.ops", json!(["link.create"])),
                    "trigger.state",
                    json!({ "to": ["blocked"] }),
                ),
                "state guard",
            ),
            (with(base(), "run_as.roles", json!(["member", "system"])), "system"),
            (with(base(), "run_as.roles", json!(["agent"])), "agent"),
            (with(base(), "run_as.roles", json!([])), "at least one role"),
            (with(base(), "run_as.workspaces", json!([])), "workspaces"),
            (with(base(), "max_depth", json!(0)), "max_depth"),
            (with(base(), "rate", json!({ "per_minute": 0 })), "rate limits"),
            (
                with(base(), "conditions", json!([{ "check": "prop_eq", "param": "issue", "prop": "x" }])),
                "conditions read only `subject`",
            ),
            (
                with(base(), "conditions", json!([{ "check": "has_link", "param": "subject", "rel": "frobs" }])),
                "unknown relation frobs",
            ),
            (
                with(base(), "effects.0.action.params", json!({ "issue": "{issue}" })),
                "templates name only trigger.*, subject.*",
            ),
            (
                with(
                    base(),
                    "conditions",
                    json!([{ "check": "prop_eq", "param": "subject", "prop": "x", "value": "{actor}" }]),
                ),
                "{actor}",
            ),
        ];
        for (v, want) in cases {
            let e = invalid(v.clone());
            assert!(e.contains(want), "{v}: {e} (want {want})");
        }
    }

    #[test]
    fn definitions_never_hold_secret_references() {
        for s in ["env:HOOK_URL", "file:/run/secrets/x", "sealed:abc", "  ENV:X"] {
            let v = with(
                base(),
                "effects",
                json!([{ "webhook": { "endpoint": "ops", "event": "x", "payload": { "auth": s } } }]),
            );
            let e = invalid(v);
            assert!(e.contains("secret references") && e.contains("effects[0].webhook.payload.auth"), "{e}");
            assert!(!e.contains(s.trim()), "the error names the place, never the value: {e}");
        }
        let e = invalid(with(base(), "description", json!("env:SECRET")));
        assert!(e.contains("at description"), "{e}");
    }

    #[test]
    fn unknown_fields_are_refused_rather_than_ignored() {
        // A typo must not quietly widen a trigger or drop a condition.
        for (path, x) in
            [("trigger.kind", json!(["issue"])), ("condition", json!([])), ("effects.0.action.param", json!({}))]
        {
            let v = with(base(), path, x);
            assert!(serde_json::from_value::<Automation>(v.clone()).is_err(), "{v}");
        }
        let mut s = schema();
        let e = s
            .extend_toml("[automations.x]\nrun_as = { roles = [\"member\"], workspaces = [\"*\"] }\ntriger = {}\n")
            .unwrap_err();
        assert!(e.to_string().contains("triger"), "{e}");
    }

    #[test]
    fn closure_hash_is_stable_across_field_order() {
        let s = schema();
        let a = def(base());
        let reordered: Automation = serde_json::from_str(
            r#"{"effects":[{"action":{"params":{"issue":"{trigger.id}"},"type":"escalate_issue"}}],
                "trigger":{"ops":["entity.update"],"kinds":["issue"],"on":"change"},
                "run_as":{"workspaces":["acme"],"roles":["member"]}}"#,
        )
        .unwrap();
        let c1 = closure(&s, "a", &a, &[], &[]);
        let c2 = closure(&s, "a", &reordered, &[], &[]);
        assert_eq!(c1, c2);
        assert_eq!(c1.hash.len(), 64);
        assert_eq!(c1.items.keys().collect::<Vec<_>>(), ["action_type:escalate_issue", "automation:a"]);
        // The name is part of the definition's identity.
        assert_ne!(closure(&s, "b", &a, &[], &[]).hash, c1.hash);
        // An action type read back from TOML in another order hashes alike.
        let mut t = Schema::enterprise();
        t.extend_toml(
            r#"
[action_types.escalate_issue]
parameters = { reason = { type = "string" }, issue = { required = true, kinds = ["issue"], type = "entity" } }
risk = "low"
description = "Escalate an issue."
[[action_types.escalate_issue.effects]]
patch = { props = { escalated = true }, id = "{issue}" }
"#,
        )
        .unwrap();
        assert_eq!(closure(&t, "a", &a, &[], &[]), c1);
    }

    #[test]
    fn closure_changes_with_every_item() {
        let s = with_example();
        let name = "escalate_blocked_p1";
        let a = s.automations[name].clone();
        let ep = |version: u64, roles: &[&str], ws: &[&str]| OutboundRef {
            name: "ops_pager".into(),
            version,
            principal: ScopePrincipal::new(
                roles.iter().map(|r| Role::parse(r).unwrap()).collect(),
                ws.iter().map(|w| w.to_string()).collect(),
            ),
        };
        let eps = [ep(1, &["viewer"], &["acme"])];
        let c = closure(&s, name, &a, &eps, &[]);
        assert!(c.items.contains_key("endpoint:ops_pager"));
        let changed = |s2: &Schema, a2: &Automation, eps2: &[OutboundRef]| c.changed(&closure(s2, name, a2, eps2, &[]));

        // The automation itself.
        let mut a2 = a.clone();
        a2.allow_self = true;
        assert_eq!(changed(&s, &a2, &eps), ["automation:escalate_blocked_p1"]);
        // Its action type: effects, roles and risk.
        for edit in [
            r#"[[action_types.escalate_issue.effects]]
event = { action = "issue.escalated" }"#,
            "[action_types.escalate_issue]\nroles = [\"triager\"]",
            "[action_types.escalate_issue]\nrisk = \"medium\"",
            r#"[[action_types.escalate_issue.preconditions]]
check = "open"
param = "issue""#,
        ] {
            let mut s2 = s.clone();
            let mut at = s2.action_types["escalate_issue"].clone();
            let layer: toml::Value = toml::from_str(edit).unwrap();
            let patch = &layer["action_types"]["escalate_issue"];
            let mut v = serde_json::to_value(&at).unwrap();
            for (k, x) in patch.as_table().unwrap() {
                let x = serde_json::to_value(x).unwrap();
                match (v.get_mut(k), x) {
                    (Some(Value::Array(cur)), Value::Array(more)) => cur.extend(more),
                    (_, x) => v[k] = x,
                }
            }
            at = serde_json::from_value(v).unwrap();
            s2.action_types.insert("escalate_issue".into(), at);
            assert_eq!(changed(&s2, &a, &eps), ["action_type:escalate_issue"], "{edit}");
        }
        // The endpoint's version and principal.
        assert_eq!(changed(&s, &a, &[ep(2, &["viewer"], &["acme"])]), ["endpoint:ops_pager"]);
        assert_eq!(changed(&s, &a, &[ep(1, &["admin"], &["acme"])]), ["endpoint:ops_pager"]);
        assert_eq!(changed(&s, &a, &[ep(1, &["viewer"], &["*"])]), ["endpoint:ops_pager"]);
        // An endpoint that is gone (or not yet registered) is `missing`.
        let gone = closure(&s, name, &a, &[], &[]);
        assert_eq!(gone.items["endpoint:ops_pager"], MISSING);
        // Endpoints the automation does not reach do not count.
        let other = OutboundRef { name: "elsewhere".into(), ..eps[0].clone() };
        assert_eq!(closure(&s, name, &a, &[eps[0].clone(), other], &[]), c);
    }

    #[test]
    fn writeback_targets_reached_through_action_types_count() {
        // Action types gain writeback effects in build step 12; the closure
        // reads them from the action type document.
        let s = schema();
        assert!(reached_targets(&s.action_types["escalate_issue"]).is_empty());
        let mut at = serde_json::to_value(&s.action_types["escalate_issue"]).unwrap();
        let effects = at["effects"].as_array_mut().unwrap();
        effects.push(json!({ "writeback": { "target": "helpdesk" } }));
        effects.push(json!({ "writeback": { "target": "crm" } }));
        effects.push(json!({ "writeback": { "target": "helpdesk" } }));
        assert_eq!(targets_in(&at), ["crm", "helpdesk"]);
    }

    fn record(op: ChangeOp, kind: &str, state: Option<(&str, &str)>) -> ChangeRecord {
        let mut r: ChangeRecord = serde_json::from_value(json!({
            "cursor": "5.0", "ts": 1, "op": op, "workspace": "acme", "kind": kind, "id": format!("{kind}:1"),
        }))
        .unwrap();
        r.state = state.map(|(f, t)| StateMove { from: Some(f.into()), to: Some(t.into()) });
        r.changed = vec!["state".into()];
        r
    }

    #[test]
    fn triggers_match_committed_changes() {
        let s = with_example();
        let name = "escalate_blocked_p1";
        let a = &s.automations[name];
        let hit = record(ChangeOp::EntityUpdate, "issue", Some(("triaged", "blocked")));
        assert!(a.admits(name, &s, &hit));
        assert!(!a.admits(name, &s, &record(ChangeOp::EntityUpdate, "issue", Some(("new", "blocked")))));
        assert!(!a.admits(name, &s, &record(ChangeOp::EntityUpdate, "issue", Some(("triaged", "done")))));
        assert!(!a.admits(name, &s, &record(ChangeOp::EntityUpdate, "issue", None)), "no state move");
        assert!(!a.admits(name, &s, &record(ChangeOp::EntityCreate, "issue", Some(("triaged", "blocked")))));
        assert!(!a.admits(name, &s, &record(ChangeOp::EntityUpdate, "task", Some(("triaged", "blocked")))));
        // Gaps never trigger runs, although every change filter passes them.
        let gap = ChangeRecord::gap(
            5,
            1,
            changes::Gap::listing(changes::GapReason::BulkImport, String::new(), vec![], vec![]),
        );
        assert!(changes::matches(&a.change_trigger().filter(), &s, &gap));
        assert!(!a.admits(name, &s, &gap));
        // Migrations are skipped unless asked for.
        let mut m = hit.clone();
        m.cause = Cause { migration: true, ..Cause::default() };
        assert!(!a.admits(name, &s, &m));
        let mut b = a.clone();
        let Trigger::Change(t) = &mut b.trigger;
        t.include_migrations = true;
        assert!(b.admits(name, &s, &m));
        // Sources: connector writes are excluded.
        let mut c = hit.clone();
        c.cause.source = Some("connector:jira".into());
        assert!(!a.admits(name, &s, &c));
        // Its own runs do not trigger it unless allow_self.
        let mut own = hit.clone();
        own.cause.automation = Some(name.into());
        own.cause.depth = 1;
        assert!(!a.admits(name, &s, &own));
        let mut selfish = a.clone();
        selfish.allow_self = true;
        assert!(selfish.admits(name, &s, &own));
        // Another automation's run does.
        own.cause.automation = Some("other".into());
        assert!(a.admits(name, &s, &own));
    }

    #[test]
    fn depth_limit_is_the_smaller_of_definition_and_process() {
        let mut a = def(base());
        assert_eq!(a.depth_limit(3), 3);
        a.max_depth = Some(2);
        assert_eq!(a.depth_limit(3), 2);
        a.max_depth = Some(9);
        assert_eq!(a.depth_limit(3), 3);
    }

    #[test]
    fn budgets_limit_runs_per_tenant_automation_and_subject() {
        let mut b = Budget::default();
        let l = Limits { tenant_per_minute: 5, per_minute: 2, per_subject_per_hour: 3 };
        let t0 = 1_000_000;
        assert!(b.admit(t0, "a", "issue:1", &l).is_ok());
        assert!(b.admit(t0 + 1, "a", "issue:2", &l).is_ok());
        // The automation's minute is used up; another automation still runs.
        assert_eq!(b.admit(t0 + 2, "a", "issue:3", &l), Err((Over::Automation, 2)));
        assert!(b.admit(t0 + 2, "b", "issue:3", &l).is_ok());
        // A minute later the window has moved on.
        assert!(b.admit(t0 + MINUTE_MS + 1, "a", "issue:1", &l).is_ok());
        assert!(b.admit(t0 + 2 * MINUTE_MS + 2, "a", "issue:1", &l).is_ok());
        // Three runs about issue:1 within the hour: the subject budget stops a loop.
        assert_eq!(b.admit(t0 + 3 * MINUTE_MS + 3, "a", "issue:1", &l), Err((Over::Subject, 3)));
        assert!(b.admit(t0 + HOUR_MS + 1, "a", "issue:1", &l).is_ok(), "the hour moved on");
        // The tenant's budget covers every automation.
        let tight = Limits { tenant_per_minute: 1, per_minute: 10, per_subject_per_hour: 10 };
        let mut b = Budget::default();
        assert!(b.admit(t0, "a", "x", &tight).is_ok());
        assert_eq!(b.admit(t0, "b", "y", &tight), Err((Over::Tenant, 1)));
        assert!(Over::Tenant.reason(1).contains("max_runs_per_minute"));
        assert!(Over::Subject.reason(3).starts_with("subject_budget"));
        // Minute windows empty after a minute; the subject's lasts the hour.
        b.trim(t0 + 2 * MINUTE_MS);
        assert_eq!(b.windows.keys().collect::<Vec<_>>(), ["subject:a:x"]);
        b.trim(t0 + HOUR_MS + 1);
        assert!(b.windows.is_empty());
        // Unset rates take the process defaults.
        let r = Rate { per_minute: Some(30), per_subject_per_hour: None };
        assert_eq!(r.limits(600, 60, 10), Limits { tenant_per_minute: 600, per_minute: 30, per_subject_per_hour: 10 });
    }

    #[test]
    fn limits_count_only_changes_that_add_automations() {
        let base_s = with_example();
        let mut more = base_s.clone();
        more.automations.insert("second".into(), def(base()));
        assert!(check_limit(&base_s, &more, 2).is_ok());
        let e = check_limit(&base_s, &more, 1).unwrap_err();
        assert!(e.to_string().contains("limit is 1"), "{e}");
        // Already over a lowered limit: edits that add nothing still pass.
        assert!(check_limit(&more, &more, 1).is_ok());
    }

    #[test]
    fn stale_names_the_items_that_changed() {
        let s = with_example();
        let name = "escalate_blocked_p1";
        let eps = vec![OutboundRef {
            name: "ops_pager".into(),
            version: 1,
            principal: ScopePrincipal::new(vec![Role::Viewer], vec!["acme".into()]),
        }];
        let c = closure(&s, name, &s.automations[name], &eps, &[]);
        let mut en = Enablements { endpoints: eps, ..Default::default() };
        en.states.insert(
            name.into(),
            AutomationState {
                enabled: true,
                closure_hash: c.hash.clone(),
                closure: c.items.clone(),
                ..Default::default()
            },
        );
        assert!(en.stale(&s).is_empty());
        let mut s2 = s.clone();
        s2.action_types.get_mut("escalate_issue").unwrap().risk = crate::action_types::Risk::High;
        assert_eq!(en.stale(&s2)[name], ["action_type:escalate_issue"]);
        let mut s3 = s.clone();
        s3.automations.clear();
        assert_eq!(en.stale(&s3)[name], [format!("automation:{name}")]);
        // Disabled automations are never reported.
        en.states.get_mut(name).unwrap().enabled = false;
        assert!(en.stale(&s2).is_empty());
    }
}
