//! Action types: governed, parameterized edits declared in the schema.
//!
//! An action type has typed parameters, preconditions (submission criteria),
//! conditional effects (patch, link, unlink, create, delete, event,
//! writeback), allowed roles and a declared risk. Planning renders the effects
//! against the current graph and derives an inverse for every graph effect, so
//! each action knows whether it is reversible before it runs. High-risk or
//! irreversible actions require explicit confirmation. Applying writes the
//! edits, an audit event and an undo receipt in one transaction. On request a
//! plan also reports its consequences: the derived facts that appear and
//! disappear when its edits are staged on a fork (see
//! [`Ontology::plan_action_consequences`]).
//!
//! A `writeback` effect (docs/design/EVENTS.md Part D) is an HTTP call to a
//! registered target. It is rendered strictly when the action is planned and
//! committed as an outbox row with the action, so it is enqueued once and
//! delivered at least once after the commit, with `Idempotency-Key:
//! <receipt>:<effect>`. Writeback is not reversible; action.undo does not
//! undo the external write. A plan with one is not reversible and at least
//! medium risk, so it needs a person's confirmation, unless the action type
//! declares `unattended` for the caller's kind (`agent`, `automation`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::engine::{ActionContext, Extra};
use crate::graph::Graph;
use crate::live_schema::{ACTION_CONSEQUENCES_MEANS, Consequences, data_consequences};
use crate::model::*;
use crate::policy::{self, Principal, Role};
use crate::schema::{Dir, Schema};
use crate::time::{new_id, now_ms};
use crate::{Error, Ontology, Result};

/// KV namespace of action receipts (key: the receipt id).
pub(crate) const RECEIPTS: &str = "action_receipt";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    #[default]
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    #[default]
    Entity,
    String,
    Text,
    Int,
    Float,
    Bool,
    Enum,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ParamDef {
    #[serde(rename = "type", default)]
    pub ty: ParamType,
    /// Allowed kinds or interfaces for entity parameters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Words that introduce this parameter in a sentence ("to", "on", "in").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cue: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Open,
    Closed,
    StateIn,
    StateNotIn,
    StateAllowed,
    HasLink,
    NoLink,
    PropEq,
    PropSet,
    Role,
    Distinct,
}

/// A precondition, or the `when` guard of an effect.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub check: CheckKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PatchEffect {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub props: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LinkEffect {
    pub from: String,
    pub rel: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub props: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LinkTemplate {
    pub rel: String,
    pub to: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CreateEffect {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub props: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkTemplate>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EventEffect {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
}

/// An HTTP call to a registered writeback target (docs/design/EVENTS.md
/// Part D): `body` is rendered strictly when the action is planned and posted
/// to the target's `route` through the outbox after the action commits, with
/// `Idempotency-Key: <receipt>:<effect>`. Not reversible: action.undo does not
/// undo the external write.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WritebackEffect {
    /// The target's name (KV `writeback.target`; literal, no placeholders).
    pub target: String,
    /// One of the target's route names (literal).
    pub route: String,
    /// The JSON body (default `{}`); every placeholder must have a value.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub body: Value,
}

/// Caller kinds an action type may declare `unattended` for.
pub const UNATTENDED_KINDS: [&str; 2] = ["agent", "automation"];

/// One edit. Exactly one of `patch`, `link`, `unlink`, `create`, `delete`,
/// `event`, `writeback`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Effect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<PatchEffect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkEffect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlink: Option<LinkEffect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<CreateEffect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<EventEffect>,
    /// An HTTP call sent after the commit (not reversible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writeback: Option<WritebackEffect>,
    /// For `link`: remove other links of the same relation from the source first.
    #[serde(default)]
    pub replace: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Check>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ActionType {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Verbs that trigger this action from natural language ("assign", "mark").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phrases: Vec<String>,
    /// Roles allowed to run it (empty: anyone who can write).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub risk: Risk,
    #[serde(default)]
    pub parameters: BTreeMap<String, ParamDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<Check>,
    #[serde(default)]
    pub effects: Vec<Effect>,
    /// Caller kinds (`agent`, `automation`) that may apply it without the
    /// confirmation its external (writeback) effects add. Schema, so it is
    /// approved through governance and part of every automation closure that
    /// reaches it. Its declared risk still applies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unattended: Vec<String>,
}

/// The roots of the placeholders in `s` (`{issue.title}` → `issue`).
pub(crate) fn placeholders(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find('{') {
        let Some(end) = rest[start..].find('}') else { break };
        let inner = &rest[start + 1..start + end];
        if !inner.is_empty() && inner.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
            out.push(inner.split('.').next().unwrap_or(inner).to_string());
        }
        rest = &rest[start + end + 1..];
    }
    out
}

/// Every string value in `v`, depth first (object keys are not included).
pub(crate) fn strings_in(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| strings_in(x, out)),
        Value::Object(m) => m.values().for_each(|x| strings_in(x, out)),
        _ => {}
    }
}

impl ActionType {
    /// The parameter an imperative's direct object fills: the subject of the first effect.
    pub fn primary_param(&self) -> Option<String> {
        let first = self.effects.first()?;
        let subject = first
            .patch
            .as_ref()
            .map(|x| x.id.clone())
            .or_else(|| first.link.as_ref().map(|x| x.from.clone()))
            .or_else(|| first.unlink.as_ref().map(|x| x.from.clone()))
            .or_else(|| first.delete.clone())
            .or_else(|| first.event.as_ref().and_then(|x| x.target.clone()))?;
        placeholders(&subject).into_iter().find(|p| self.parameters.contains_key(p))
    }

    pub fn validate(&self, schema: &Schema) -> std::result::Result<(), String> {
        if self.effects.is_empty() {
            return Err("needs at least one effect".into());
        }
        for (name, p) in &self.parameters {
            for k in &p.kinds {
                if !schema.is_type_or_interface(k) {
                    return Err(format!("parameter {name}: unknown kind {k}"));
                }
            }
            if p.ty == ParamType::Enum && p.values.is_empty() {
                return Err(format!("parameter {name}: enum needs values"));
            }
        }
        let known = |n: &str| self.parameters.contains_key(n) || matches!(n, "actor" | "now" | "created" | "new_id");
        let mut templates: Vec<String> = Vec::new();
        let mut checks: Vec<&Check> = self.preconditions.iter().collect();
        for (i, e) in self.effects.iter().enumerate() {
            let ops = [
                e.patch.is_some(),
                e.link.is_some(),
                e.unlink.is_some(),
                e.create.is_some(),
                e.delete.is_some(),
                e.event.is_some(),
                e.writeback.is_some(),
            ];
            if ops.iter().filter(|x| **x).count() != 1 {
                return Err(format!(
                    "effect {i}: exactly one of patch, link, unlink, create, delete, event, writeback"
                ));
            }
            if let Some(w) = &e.writeback {
                for (what, v) in [("target", &w.target), ("route", &w.route)] {
                    if !outbound_name(v) {
                        return Err(format!(
                            "effect {i}: writeback {what} '{v}' must be a name (1 to 40 of a-z, 0-9, _ and -)"
                        ));
                    }
                }
            }
            let v = serde_json::to_value(e).map_err(|x| x.to_string())?;
            let mut v = v;
            if let Value::Object(m) = &mut v {
                m.remove("when");
            }
            strings_in(&v, &mut templates);
            for l in e.link.iter().chain(e.unlink.iter()) {
                if schema.resolve_rel(&l.rel).is_none() {
                    return Err(format!("effect {i}: unknown relation {}", l.rel));
                }
            }
            if let Some(c) = &e.create {
                if !schema.object_types.contains_key(&c.kind) {
                    return Err(format!("effect {i}: unknown kind {}", c.kind));
                }
                for l in &c.links {
                    if schema.resolve_rel(&l.rel).is_none() {
                        return Err(format!("effect {i}: unknown relation {}", l.rel));
                    }
                }
            }
            if let Some(w) = &e.when {
                checks.push(w);
            }
        }
        for t in &templates {
            for ph in placeholders(t) {
                if !known(&ph) {
                    return Err(format!("template references unknown parameter {{{ph}}}"));
                }
            }
        }
        for u in &self.unattended {
            if !UNATTENDED_KINDS.contains(&u.as_str()) {
                return Err(format!("unattended: '{u}' is not a caller kind ({})", UNATTENDED_KINDS.join(", ")));
            }
        }
        for c in checks {
            for p in c.param.iter().chain(c.other.iter()) {
                if !self.parameters.contains_key(p) {
                    return Err(format!("check references unknown parameter {p}"));
                }
            }
            if matches!(c.check, CheckKind::HasLink | CheckKind::NoLink) {
                let rel = c.rel.as_deref().ok_or("has_link/no_link need `rel`")?;
                if schema.resolve_rel(rel).is_none() {
                    return Err(format!("check uses unknown relation {rel}"));
                }
            }
        }
        Ok(())
    }
}

/// `[a-z0-9_-]`, 1 to 40 characters: writeback target and route names.
fn outbound_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The kind of caller `p` is, for `unattended`: `automation` (the service
/// principal `automation:<name>`), `agent`, or `None` (a person or service).
pub(crate) fn caller_kind(p: &Principal) -> Option<&'static str> {
    if p.id.starts_with(crate::automate::PRINCIPAL_PREFIX) {
        Some("automation")
    } else if p.is_agent() {
        Some("agent")
    } else {
        None
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckResult {
    pub ok: bool,
    pub check: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionPlan {
    pub action: String,
    pub params: Map<String, Value>,
    pub valid: bool,
    pub checks: Vec<CheckResult>,
    /// Human-readable list of the edits.
    pub preview: Vec<String>,
    pub mutations: Vec<Mutation>,
    /// Per effect, the mutations that undo it (applied in reverse effect order).
    pub inverse: Vec<Vec<Mutation>>,
    pub risk: Risk,
    pub reversible: bool,
    pub needs_confirmation: bool,
    pub warnings: Vec<String>,
    /// What the edits do to entailment, when asked for with
    /// [`Ontology::plan_action_consequences`]; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consequences: Option<Consequences>,
    /// External writes (writeback effects), rendered strictly: each is
    /// committed as an outbox row with the action and sent after the commit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external: Vec<PlannedExternal>,
    /// With external writes: whether the graph edits alone are reversible
    /// (`action.undo` with `force` undoes only those; the external write is
    /// not undone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_reversible: Option<bool>,
}

/// One planned external write.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlannedExternal {
    /// The effect's index in the action type (part of the idempotency key).
    pub effect: usize,
    pub target: String,
    pub route: String,
    /// The strictly rendered JSON body.
    pub body: Value,
}

/// One external write of a receipt: the outbox row that carries it and the
/// row's state (joined when the receipt is read).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExternalCall {
    pub effect: usize,
    pub target: String,
    pub route: String,
    /// The outbox row id.
    pub outbox: String,
    /// `<receipt>:<effect>`, sent as `Idempotency-Key`.
    pub idempotency_key: String,
    pub state: crate::outbox::OutboxState,
    #[serde(default)]
    pub attempts: u32,
}

/// What plans and undo say about external writes (docs/design/EVENTS.md
/// section 13).
pub const NOT_REVERSIBLE_NOTE: &str = "Writeback is not reversible; action.undo does not undo the external write.";

/// What a receipt with external writes says (docs/design/EVENTS.md D.1).
pub const RECEIPT_NOTE: &str = "not reversible: the external write is not undone by action.undo";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionReceipt {
    pub id: String,
    pub action: String,
    /// applied | proposed | rejected | undone
    pub status: String,
    pub actor: String,
    pub workspace: String,
    pub request_id: String,
    pub ts: i64,
    pub plan: ActionPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<MutationResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Revision of each touched entity right after the action (`None`: deleted).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub touched: BTreeMap<Id, Option<u64>>,
    /// The external writes it enqueued (writeback), each enqueued once and
    /// delivered at least once after the commit. Not reversible: action.undo
    /// does not undo them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external: Vec<ExternalCall>,
    /// With external writes: [`RECEIPT_NOTE`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ApplyOptions {
    /// Plan and validate only.
    #[serde(default)]
    pub dry_run: bool,
    /// Required for high-risk or irreversible actions.
    #[serde(default)]
    pub confirm: bool,
}

/// The clock and fresh id a plan renders `{now}` and `{new_id}` with. Planning
/// again with the same seed (inside the commit, see
/// [`Ontology::apply_planned`]) renders the same values, so a re-planned
/// `create` makes the id the first plan showed.
#[derive(Clone, Debug)]
pub(crate) struct PlanSeed {
    pub now: i64,
    pub new_id: String,
}

impl PlanSeed {
    pub(crate) fn fresh() -> Self {
        Self { now: now_ms(), new_id: new_id("x")[2..].to_string() }
    }

    /// The seed of a receipt: its timestamp, and its id without the `act_` prefix.
    pub(crate) fn of(id: &str, ts: i64) -> Self {
        Self { now: ts, new_id: id.split_once('_').map_or(id, |(_, rest)| rest).to_string() }
    }
}

/// True when `inner` is a placeholder name: `param` or `param.field`.
fn is_placeholder(inner: &str) -> bool {
    !inner.is_empty() && inner.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
}

/// The one template renderer: action-type effects and checks today,
/// automation payloads, webhook payloads and writeback bodies later.
///
/// Two rules hold for every caller:
/// - Every entity-field lookup (`{param.field}`) goes through `see`, the
///   acting principal's visibility: an entity it may not see has no fields,
///   whichever parameter type named it.
/// - [`render_value`](Self::render_value) is lenient, as action types have
///   always been: object keys and array items whose placeholder has no value
///   are dropped. [`render_strict`](Self::render_strict) refuses the whole
///   render instead (`invalid_template`), and drops nothing.
pub(crate) struct Templates<'a> {
    params: &'a Map<String, Value>,
    g: &'a Graph,
    see: &'a dyn Fn(&Entity) -> bool,
    actor: &'a str,
    now: i64,
    created: Option<String>,
    new_id: String,
}

impl<'a> Templates<'a> {
    pub(crate) fn new(
        g: &'a Graph,
        params: &'a Map<String, Value>,
        actor: &'a str,
        see: &'a dyn Fn(&Entity) -> bool,
        seed: &PlanSeed,
    ) -> Self {
        Self { params, g, see, actor, now: seed.now, created: None, new_id: seed.new_id.clone() }
    }

    /// The entity `id`, when it exists and the acting principal may see it.
    pub(crate) fn entity(&self, id: &str) -> Option<&'a Entity> {
        self.g.get(id).filter(|e| (self.see)(e))
    }

    /// The value of one placeholder; `None` when it has none (unset, null, or
    /// a field of an entity that does not exist or is not visible).
    pub(crate) fn lookup(&self, path: &str) -> Option<Value> {
        let mut parts = path.split('.');
        let root = parts.next()?;
        let field: Vec<&str> = parts.collect();
        let base = match root {
            "actor" => Value::String(self.actor.to_string()),
            "now" => json!(self.now),
            "created" => Value::String(self.created.clone()?),
            "new_id" => Value::String(self.new_id.clone()),
            p => self.params.get(p)?.clone(),
        };
        if field.is_empty() {
            return (!base.is_null()).then_some(base);
        }
        // A structured value (an automation's `{trigger.state.from}`) is
        // walked by field; a string names an entity, read as the principal.
        if base.is_object() {
            let mut cur = &base;
            for f in &field {
                cur = cur.get(*f)?;
            }
            return (!cur.is_null()).then(|| cur.clone());
        }
        let e = self.entity(base.as_str()?)?;
        let v = match field[0] {
            "id" => json!(e.id),
            "title" => json!(e.label()),
            "state" => json!(e.state.clone()?),
            "kind" => json!(e.kind),
            "workspace" => json!(e.workspace),
            "key" => json!(e.key()),
            "props" => e.props.get(*field.get(1)?)?.clone(),
            other => e.props.get(other)?.clone(),
        };
        (!v.is_null()).then_some(v)
    }

    /// Render a template string; `None` when a placeholder has no value.
    pub(crate) fn render(&self, t: &str) -> Option<String> {
        self.string(t, &mut Vec::new())
    }

    /// Render JSON leniently: a string that is exactly one placeholder keeps
    /// its type; object keys and array items without a value are dropped.
    pub(crate) fn render_value(&self, v: &Value) -> Option<Value> {
        self.value(v, false, &mut Vec::new())
    }

    pub(crate) fn render_props(&self, m: &Map<String, Value>) -> Map<String, Value> {
        m.iter().filter_map(|(k, v)| self.render_value(v).map(|x| (k.clone(), x))).collect()
    }

    /// Render JSON strictly: any placeholder without a value, at any depth,
    /// refuses the render with `invalid_template`, naming the placeholders
    /// (never a value).
    pub(crate) fn render_strict(&self, v: &Value) -> Result<Value> {
        let mut missing = Vec::new();
        match self.value(v, true, &mut missing) {
            Some(out) if missing.is_empty() => Ok(out),
            _ => {
                missing.sort();
                missing.dedup();
                let names: Vec<String> = missing.iter().map(|m| format!("{{{m}}}")).collect();
                Err(Error::invalid(format!(
                    "invalid_template: no value for {} (unset, or the entity is not visible)",
                    names.join(", ")
                )))
            }
        }
    }

    /// Render `t`, recording every placeholder that has no value.
    fn string(&self, t: &str, missing: &mut Vec<String>) -> Option<String> {
        let mut out = String::new();
        let mut whole = true;
        let mut rest = t;
        while let Some(start) = rest.find('{') {
            let Some(end) = rest[start..].find('}') else { break };
            let inner = &rest[start + 1..start + end];
            out.push_str(&rest[..start]);
            if is_placeholder(inner) {
                match self.lookup(inner) {
                    Some(Value::String(s)) => out.push_str(&s),
                    Some(v) => out.push_str(&v.to_string()),
                    None => {
                        missing.push(inner.to_string());
                        whole = false;
                    }
                }
            } else {
                out.push_str(&rest[start..=start + end]);
            }
            rest = &rest[start + end + 1..];
        }
        out.push_str(rest);
        whole.then_some(out)
    }

    fn value(&self, v: &Value, strict: bool, missing: &mut Vec<String>) -> Option<Value> {
        match v {
            Value::String(s) => {
                let t = s.trim();
                if t.starts_with('{')
                    && t.ends_with('}')
                    && placeholders(t).len() == 1
                    && is_placeholder(&t[1..t.len() - 1])
                {
                    let name = &t[1..t.len() - 1];
                    let found = self.lookup(name);
                    if found.is_none() {
                        missing.push(name.to_string());
                    }
                    return found;
                }
                self.string(s, missing).map(Value::String)
            }
            Value::Object(m) => {
                let mut out = Map::new();
                let mut whole = true;
                for (k, x) in m {
                    match self.value(x, strict, missing) {
                        Some(y) => {
                            out.insert(k.clone(), y);
                        }
                        None => whole = false,
                    }
                }
                (whole || !strict).then_some(Value::Object(out))
            }
            Value::Array(a) => {
                let mut out = Vec::new();
                let mut whole = true;
                for x in a {
                    match self.value(x, strict, missing) {
                        Some(y) => out.push(y),
                        None => whole = false,
                    }
                }
                (whole || !strict).then_some(Value::Array(out))
            }
            other => Some(other.clone()),
        }
    }
}

fn check_name(c: &Check) -> String {
    format!("{:?}", c.check).to_lowercase()
}

/// Evaluate one check. The entity a check reads is looked up through the
/// templates, so an entity the principal may not see fails the check as if
/// it did not exist.
fn eval_check(schema: &Schema, g: &Graph, c: &Check, env: &Templates<'_>, p: &Principal) -> (bool, String) {
    let ent = c.param.as_ref().and_then(|n| env.params.get(n)).and_then(Value::as_str).and_then(|id| env.entity(id));
    let label = ent.map(|e| e.id.clone()).unwrap_or_else(|| c.param.clone().unwrap_or_default());
    let state = ent.and_then(|e| e.state.clone());
    let (ok, default_msg) = match c.check {
        CheckKind::Open => {
            (ent.is_some_and(|e| schema.is_open(&e.kind, e.state.as_deref())), format!("{label} must be open"))
        }
        CheckKind::Closed => {
            (ent.is_some_and(|e| !schema.is_open(&e.kind, e.state.as_deref())), format!("{label} must be closed"))
        }
        CheckKind::StateIn => (
            state.as_ref().is_some_and(|s| c.values.contains(s)),
            format!("{label} must be in state {}", c.values.join("|")),
        ),
        CheckKind::StateNotIn => (
            !state.as_ref().is_some_and(|s| c.values.contains(s)),
            format!("{label} must not be in state {}", c.values.join("|")),
        ),
        CheckKind::StateAllowed => {
            let target =
                c.value.as_ref().and_then(|v| env.render_value(v)).and_then(|v| v.as_str().map(str::to_string));
            let ok = match (ent, &target) {
                (Some(e), Some(t)) => {
                    schema.object_type(&e.kind).is_none_or(|ty| ty.states.is_empty() || ty.states.contains(t))
                }
                _ => false,
            };
            (ok, format!("{label} cannot take state {}", target.unwrap_or_default()))
        }
        CheckKind::HasLink | CheckKind::NoLink => {
            let rel = c.rel.clone().unwrap_or_default();
            let (base, dir) = schema.resolve_rel_lenient(&rel);
            let to = c.to.as_ref().and_then(|t| env.render(t));
            let has = ent.is_some_and(|e| {
                g.steps_via(&e.id, &base, dir).iter().any(|s| to.as_deref().is_none_or(|t| s.other == t))
            });
            let target = to.clone().unwrap_or_else(|| "anything".into());
            if c.check == CheckKind::HasLink {
                (has, format!("{label} must have {rel} {target}"))
            } else {
                (!has, format!("{label} already has {rel} {target}"))
            }
        }
        CheckKind::PropEq => {
            let prop = c.prop.clone().unwrap_or_default();
            let want = c.value.as_ref().and_then(|v| env.render_value(v));
            (
                ent.is_some_and(|e| e.props.get(&prop) == want.as_ref()),
                format!("{label}.{prop} must equal {}", want.unwrap_or(Value::Null)),
            )
        }
        CheckKind::PropSet => {
            let prop = c.prop.clone().unwrap_or_default();
            (
                ent.is_some_and(|e| e.props.get(&prop).is_some_and(|v| !v.is_null())),
                format!("{label}.{prop} must be set"),
            )
        }
        CheckKind::Role => {
            let ok = p.is_privileged()
                || c.values
                    .iter()
                    .any(|r| serde_json::from_value::<Role>(json!(r)).is_ok_and(|role| p.has_role(&role)));
            (ok, format!("requires role {}", c.values.join("|")))
        }
        CheckKind::Distinct => {
            let a = c.param.as_ref().and_then(|n| env.params.get(n));
            let b = c.other.as_ref().and_then(|n| env.params.get(n));
            (
                a != b,
                format!(
                    "{} and {} must differ",
                    c.param.clone().unwrap_or_default(),
                    c.other.clone().unwrap_or_default()
                ),
            )
        }
    };
    (ok, c.message.clone().unwrap_or(default_msg))
}

/// Evaluate `checks` (an automation's conditions) with the templates `env`
/// as `p`: the failed checks' messages, empty when all pass. Every entity a
/// check reads goes through `env`'s visibility.
pub(crate) fn eval_checks(
    schema: &Schema,
    g: &Graph,
    checks: &[Check],
    env: &Templates<'_>,
    p: &Principal,
) -> Vec<String> {
    checks.iter().map(|c| eval_check(schema, g, c, env, p)).filter(|(ok, _)| !ok).map(|(_, message)| message).collect()
}

fn as_input(e: &Entity) -> EntityInput {
    EntityInput {
        id: Some(e.id.clone()),
        kind: e.kind.clone(),
        workspace: Some(e.workspace.clone()),
        title: Some(e.title.clone()),
        state: e.state.clone(),
        props: e.props.clone(),
        aliases: e.aliases.clone(),
        provenance: e.provenance.clone(),
        replace: true,
        ..Default::default()
    }
}

/// Reason prefix of a plan that no longer holds at commit.
pub(crate) const PRECONDITION_CHANGED: &str = "precondition_changed";

/// The entity an action's audit event names: its first id-like parameter.
pub(crate) fn audit_target(params: &Map<String, Value>) -> Option<String> {
    params.values().find_map(|v| v.as_str().filter(|s| s.contains(':')).map(str::to_string))
}

/// Why a plan made again at commit may not replace the plan that was checked
/// and confirmed: it is no longer valid, its risk rose, or it stopped being
/// reversible. `None` when it may.
fn drift(planned: &ActionPlan, fresh: &ActionPlan) -> Option<Vec<String>> {
    let mut why = Vec::new();
    if !fresh.valid {
        why.extend(fresh.checks.iter().filter(|c| !c.ok).map(|c| c.message.clone()));
        if why.is_empty() {
            why.push("nothing to change".into());
        }
    }
    if fresh.risk > planned.risk {
        why.push(format!("risk rose from {:?} to {:?}", planned.risk, fresh.risk).to_lowercase());
    }
    if planned.reversible && !fresh.reversible {
        why.push("the action is no longer reversible".into());
    }
    (!why.is_empty()).then_some(why)
}

/// The preview line of a planned external write.
fn writeback_preview(x: &PlannedExternal) -> String {
    let mut body = x.body.to_string();
    if body.len() > 120 {
        let mut end = 117;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push('…');
    }
    format!("writeback {}.{} {body} [async]", x.target, x.route)
}

/// The outbox rows of `plan`'s external writes under receipt `receipt` (one
/// per writeback effect, `dedupe = "<receipt>:<effect>"`, the route in
/// `event_type`, the body as the payload), and what the receipt records of
/// them.
pub(crate) fn writeback_rows(
    receipt: &str,
    plan: &ActionPlan,
    ctx: &ActionContext,
    source: &str,
    run: Option<&str>,
) -> (Vec<crate::outbox::OutboxRow>, Vec<ExternalCall>) {
    use crate::outbox::{OutboxKind, OutboxRow, OutboxState};
    let mut rows = Vec::new();
    let mut calls = Vec::new();
    for x in &plan.external {
        let key = format!("{receipt}:{}", x.effect);
        let mut row =
            OutboxRow::new(OutboxKind::Writeback, &x.target, key.clone(), source, ctx.workspace.clone(), &x.route);
        row.receipt = Some(receipt.to_string());
        row.run = run.map(str::to_string);
        row.cause = ctx.cause.clone();
        row.otel_trace_id = ctx.trace_id.clone();
        row.payload = x.body.clone();
        calls.push(ExternalCall {
            effect: x.effect,
            target: x.target.clone(),
            route: x.route.clone(),
            outbox: row.id.clone(),
            idempotency_key: key,
            state: OutboxState::Pending,
            attempts: 0,
        });
        rows.push(row);
    }
    (rows, calls)
}

impl Ontology {
    /// Validate parameters and preconditions and render the edits, without writing.
    pub fn plan_action(&self, ctx: &ActionContext, name: &str, params: &Map<String, Value>) -> Result<ActionPlan> {
        self.read(|g, schema| self.plan_on(g, schema, ctx, name, params, &PlanSeed::fresh()))
    }

    /// [`plan_action`](Self::plan_action) on a graph the caller already holds:
    /// the read lock, or the write lock inside a `transact` build closure, so
    /// the plan is made against the state it commits on. It never takes the
    /// graph lock itself. Every entity the plan reads is checked for
    /// visibility as `ctx.principal`.
    pub(crate) fn plan_on(
        &self,
        g: &Graph,
        schema: &Schema,
        ctx: &ActionContext,
        name: &str,
        params: &Map<String, Value>,
        seed: &PlanSeed,
    ) -> Result<ActionPlan> {
        self.plan_on_as(g, schema, ctx, name, params, seed, None)
    }

    /// [`plan_on`](Self::plan_on) where an entity is visible only when
    /// `also` may see it too (an automation's dry run, made with the run
    /// principal and the caller together).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn plan_on_as(
        &self,
        g: &Graph,
        schema: &Schema,
        ctx: &ActionContext,
        name: &str,
        params: &Map<String, Value>,
        seed: &PlanSeed,
        also: Option<&Principal>,
    ) -> Result<ActionPlan> {
        let at = schema.action_types.get(name).ok_or_else(|| Error::not_found(format!("action type {name}")))?;
        let p = &ctx.principal;
        let see = |e: &Entity| policy::visible(p, e, g) && also.is_none_or(|a| policy::visible(a, e, g));
        let mut checks = Vec::new();
        let mut warnings = Vec::new();
        let mut norm: Map<String, Value> = Map::new();

        if !at.roles.is_empty() && !p.is_privileged() && !at.roles.iter().any(|r| p.has_role(r)) {
            let roles: Vec<String> = at.roles.iter().map(|r| format!("{r:?}").to_lowercase()).collect();
            checks.push(CheckResult {
                ok: false,
                check: "role".into(),
                message: format!("{name} requires role {}", roles.join("|")),
            });
        }
        if !p.allows(crate::Access::Write) {
            checks.push(CheckResult { ok: false, check: "role".into(), message: format!("{} may not write", p.id) });
        }
        for k in params.keys() {
            if !at.parameters.contains_key(k) {
                warnings.push(format!("unknown parameter {k} ignored"));
            }
        }

        for (pname, def) in &at.parameters {
            let raw = params
                .get(pname)
                .filter(|v| !v.is_null() && v.as_str() != Some(""))
                .cloned()
                .or_else(|| def.default.clone());
            let Some(raw) = raw else {
                if def.required {
                    checks.push(CheckResult {
                        ok: false,
                        check: "parameter".into(),
                        message: format!("missing required parameter {pname}"),
                    });
                }
                continue;
            };
            let bad = |msg: String| CheckResult { ok: false, check: "parameter".into(), message: msg };
            let value = match def.ty {
                ParamType::Entity => {
                    let Some(id) = raw.as_str() else {
                        checks.push(bad(format!("{pname} must be an entity id")));
                        continue;
                    };
                    let id = self.resolve_in(g, id);
                    match g.get(&id) {
                        Some(e) if see(e) => {
                            if e.workspace != ctx.workspace && !p.in_workspace(&e.workspace) {
                                checks.push(bad(format!("{pname}: {id} is outside your workspaces")));
                            } else if !def.kinds.is_empty() && !def.kinds.iter().any(|k| schema.kind_is(&e.kind, k)) {
                                checks.push(bad(format!(
                                    "{pname}: {id} is a {}, expected {}",
                                    e.kind,
                                    def.kinds.join("|")
                                )));
                            }
                            json!(id)
                        }
                        _ => {
                            checks.push(bad(format!("{pname}: {id} not found")));
                            continue;
                        }
                    }
                }
                ParamType::String | ParamType::Text => match &raw {
                    Value::String(_) => raw.clone(),
                    other => json!(other.to_string()),
                },
                ParamType::Int => match raw.as_i64().or_else(|| raw.as_str().and_then(|s| s.trim().parse().ok())) {
                    Some(n) => json!(n),
                    None => {
                        checks.push(bad(format!("{pname} must be an integer")));
                        continue;
                    }
                },
                ParamType::Float => match raw.as_f64().or_else(|| raw.as_str().and_then(|s| s.trim().parse().ok())) {
                    Some(n) => json!(n),
                    None => {
                        checks.push(bad(format!("{pname} must be a number")));
                        continue;
                    }
                },
                ParamType::Bool => match raw.as_bool().or_else(|| raw.as_str().and_then(|s| s.parse().ok())) {
                    Some(b) => json!(b),
                    None => {
                        checks.push(bad(format!("{pname} must be true or false")));
                        continue;
                    }
                },
                ParamType::Enum => match raw.as_str() {
                    Some(s) if def.values.iter().any(|v| v == s) => raw.clone(),
                    _ => {
                        checks.push(bad(format!("{pname} must be one of {}", def.values.join("|"))));
                        continue;
                    }
                },
            };
            norm.insert(pname.clone(), value);
        }

        let params_ok = checks.iter().all(|c| c.ok);
        let mut mutations = Vec::new();
        let mut inverse: Vec<Vec<Mutation>> = Vec::new();
        let mut preview = Vec::new();
        let mut reversible = true;
        let mut risk = at.risk;
        let mut external: Vec<PlannedExternal> = Vec::new();
        if params_ok {
            let mut env = Templates::new(g, &norm, &p.id, &see, seed);
            for c in &at.preconditions {
                let (ok, message) = eval_check(schema, g, c, &env, p);
                checks.push(CheckResult { ok, check: check_name(c), message });
            }
            for (i, eff) in at.effects.iter().enumerate() {
                if let Some(w) = &eff.when
                    && !eval_check(schema, g, w, &env, p).0
                {
                    continue;
                }
                let skip = |what: &str, warnings: &mut Vec<String>| {
                    warnings.push(format!("effect {i} skipped: {what} has no value"))
                };
                if let Some(pe) = &eff.patch {
                    let Some(id) = env.render(&pe.id) else {
                        skip("id", &mut warnings);
                        continue;
                    };
                    let Some(cur) = env.entity(&id) else {
                        checks.push(CheckResult {
                            ok: false,
                            check: "effect".into(),
                            message: format!("{id} not found"),
                        });
                        continue;
                    };
                    let title = pe.title.as_ref().and_then(|t| env.render(t));
                    let state = pe.state.as_ref().and_then(|t| env.render(t));
                    let props = env.render_props(&pe.props);
                    let mut undo = EntityPatch { id: id.clone(), ..Default::default() };
                    let mut parts = Vec::new();
                    if let Some(t) = &title
                        && *t != cur.title
                    {
                        undo.title = Some(cur.title.clone());
                        parts.push(format!("title → {t}"));
                    }
                    if let Some(s) = &state
                        && cur.state.as_ref() != Some(s)
                    {
                        match &cur.state {
                            Some(prev) => undo.state = Some(prev.clone()),
                            None => reversible = false,
                        }
                        parts.push(format!("state {} → {s}", cur.state.as_deref().unwrap_or("∅")));
                    }
                    for (k, v) in &props {
                        if cur.props.get(k) != Some(v) {
                            match cur.props.get(k) {
                                Some(prev) => {
                                    undo.props.insert(k.clone(), prev.clone());
                                }
                                None => undo.unset.push(k.clone()),
                            }
                            parts.push(format!("{k} → {v}"));
                        }
                    }
                    if parts.is_empty() {
                        warnings.push(format!("effect {i}: {id} already matches"));
                        continue;
                    }
                    preview.push(format!("update {id}: {}", parts.join(", ")));
                    mutations.push(Mutation::Patch(EntityPatch { id, title, state, props, unset: Vec::new() }));
                    inverse.push(vec![Mutation::Patch(undo)]);
                } else if let Some(le) = &eff.link {
                    let (Some(from), Some(to)) = (env.render(&le.from), env.render(&le.to)) else {
                        skip("link endpoint", &mut warnings);
                        continue;
                    };
                    let (base, dir) = schema.resolve_rel_lenient(&le.rel);
                    let (cf, ct) = if dir == Dir::In { (to.clone(), from.clone()) } else { (from.clone(), to.clone()) };
                    let mut undo = Vec::new();
                    let mut note = String::new();
                    if eff.replace {
                        for (r, other) in g.out_edges(&cf) {
                            if *r == base && *other != ct {
                                let props = g.get_link(&cf, r, other).map(|l| l.props.clone()).unwrap_or_default();
                                mutations.push(Mutation::Unlink(LinkInput::new(
                                    cf.clone(),
                                    base.clone(),
                                    other.clone(),
                                )));
                                undo.push(Mutation::Link(LinkInput {
                                    from: cf.clone(),
                                    rel: base.clone(),
                                    to: other.clone(),
                                    props,
                                }));
                                note = format!(" (replacing {other})");
                            }
                        }
                    }
                    let existed = g.get_link(&cf, &base, &ct).is_some();
                    if existed && note.is_empty() {
                        warnings.push(format!("effect {i}: {cf} -{base}-> {ct} already exists"));
                        continue;
                    }
                    let props = env.render_props(&le.props);
                    mutations.push(Mutation::Link(LinkInput {
                        from: from.clone(),
                        rel: le.rel.clone(),
                        to: to.clone(),
                        props,
                    }));
                    if !existed {
                        undo.insert(0, Mutation::Unlink(LinkInput::new(cf.clone(), base.clone(), ct.clone())));
                    }
                    preview.push(format!("link {from} -{}-> {to}{note}", le.rel));
                    inverse.push(undo);
                } else if let Some(ue) = &eff.unlink {
                    let Some(from) = env.render(&ue.from) else {
                        skip("unlink source", &mut warnings);
                        continue;
                    };
                    let to = env.render(&ue.to).unwrap_or_else(|| "*".into());
                    let (base, dir) = schema.resolve_rel_lenient(&ue.rel);
                    let mut undo = Vec::new();
                    let mut removed = 0;
                    for s in g.steps_via(&from, &base, dir) {
                        if to != "*" && s.other != to {
                            continue;
                        }
                        let (cf, ct) = if s.dir == Dir::In {
                            (s.other.to_string(), from.clone())
                        } else {
                            (from.clone(), s.other.to_string())
                        };
                        let props = g.get_link(&cf, &base, &ct).map(|l| l.props.clone()).unwrap_or_default();
                        mutations.push(Mutation::Unlink(LinkInput::new(cf.clone(), base.clone(), ct.clone())));
                        undo.push(Mutation::Link(LinkInput {
                            from: cf.clone(),
                            rel: base.clone(),
                            to: ct.clone(),
                            props,
                        }));
                        preview.push(format!("unlink {cf} -{base}-> {ct}"));
                        removed += 1;
                    }
                    if removed == 0 {
                        warnings.push(format!("effect {i}: no {} link from {from} to {to}", ue.rel));
                        continue;
                    }
                    inverse.push(undo);
                } else if let Some(ce) = &eff.create {
                    let key = match &ce.key {
                        Some(k) => match env.render(k) {
                            Some(k) => k,
                            None => {
                                skip("key", &mut warnings);
                                continue;
                            }
                        },
                        None => env.new_id.clone(),
                    };
                    let id = format!("{}:{key}", ce.kind);
                    let links: Vec<LinkSpec> = ce
                        .links
                        .iter()
                        .filter_map(|l| {
                            env.render(&l.to).map(|to| LinkSpec { rel: l.rel.clone(), to, props: Map::new() })
                        })
                        .collect();
                    let input = EntityInput {
                        id: Some(id.clone()),
                        kind: ce.kind.clone(),
                        title: ce.title.as_ref().and_then(|t| env.render(t)),
                        state: ce.state.as_ref().and_then(|t| env.render(t)),
                        props: env.render_props(&ce.props),
                        links,
                        ..Default::default()
                    };
                    if g.contains(&id) {
                        warnings.push(format!("effect {i}: {id} exists and will be updated"));
                        reversible = false;
                        inverse.push(Vec::new());
                    } else {
                        inverse.push(vec![Mutation::Delete(EntityRef { id: id.clone() })]);
                    }
                    preview.push(format!("create {id} \"{}\"", input.title.clone().unwrap_or_default()));
                    mutations.push(Mutation::Upsert(input));
                    env.created = Some(id);
                } else if let Some(d) = &eff.delete {
                    let Some(id) = env.render(d) else {
                        skip("delete target", &mut warnings);
                        continue;
                    };
                    let Some(cur) = env.entity(&id) else {
                        checks.push(CheckResult {
                            ok: false,
                            check: "effect".into(),
                            message: format!("{id} not found"),
                        });
                        continue;
                    };
                    let mut undo = vec![Mutation::Upsert(as_input(cur))];
                    for (r, other) in g.out_edges(&id) {
                        let props = g.get_link(&id, r, other).map(|l| l.props.clone()).unwrap_or_default();
                        undo.push(Mutation::Link(LinkInput {
                            from: id.clone(),
                            rel: r.clone(),
                            to: other.clone(),
                            props,
                        }));
                    }
                    for (r, other) in g.in_edges(&id) {
                        let props = g.get_link(other, r, &id).map(|l| l.props.clone()).unwrap_or_default();
                        undo.push(Mutation::Link(LinkInput {
                            from: other.clone(),
                            rel: r.clone(),
                            to: id.clone(),
                            props,
                        }));
                    }
                    preview.push(format!("delete {id} \"{}\" and {} link(s)", cur.label(), undo.len() - 1));
                    mutations.push(Mutation::Delete(EntityRef { id }));
                    inverse.push(undo);
                    risk = risk.max(Risk::High);
                } else if let Some(ev) = &eff.event {
                    let Some(action) = env.render(&ev.action) else {
                        skip("event action", &mut warnings);
                        continue;
                    };
                    let target = ev.target.as_ref().and_then(|t| env.render(t));
                    let payload = env.render_value(&ev.payload).unwrap_or(Value::Null);
                    preview.push(format!(
                        "record event {action}{}",
                        target.as_ref().map(|t| format!(" on {t}")).unwrap_or_default()
                    ));
                    mutations.push(Mutation::Event(EventInput {
                        action: action.clone(),
                        target: target.clone(),
                        payload,
                        ..Default::default()
                    }));
                    // Events are facts; undo records a compensating event.
                    inverse.push(vec![Mutation::Event(EventInput {
                        action: format!("{action}.compensated"),
                        target,
                        ..Default::default()
                    })]);
                } else if let Some(wb) = &eff.writeback {
                    match self.plan_writeback(&env, &ctx.workspace, i, wb) {
                        Ok(x) => {
                            preview.push(writeback_preview(&x));
                            external.push(x);
                        }
                        Err(message) => checks.push(CheckResult { ok: false, check: "writeback".into(), message }),
                    }
                }
            }
        }
        let valid = checks.iter().all(|c| c.ok) && (!mutations.is_empty() || !external.is_empty());
        if params_ok && mutations.is_empty() && external.is_empty() && checks.iter().all(|c| c.ok) {
            warnings.push("nothing to change".into());
        }
        let mut needs_confirmation = risk >= Risk::Medium || !reversible;
        let mut graph_reversible = None;
        if !external.is_empty() {
            // An external write is not reversible and at least medium risk,
            // so it needs a person's confirmation; `unattended` waives what
            // the external write added for that caller kind only.
            let waived = caller_kind(p).is_some_and(|k| at.unattended.iter().any(|u| u == k));
            if !waived {
                needs_confirmation = true;
            }
            graph_reversible = Some(reversible);
            reversible = false;
            risk = risk.max(Risk::Medium);
            warnings.push(NOT_REVERSIBLE_NOTE.into());
        }
        Ok(ActionPlan {
            action: name.to_string(),
            params: norm,
            valid,
            checks,
            preview,
            needs_confirmation,
            mutations,
            inverse,
            risk,
            reversible,
            warnings,
            consequences: None,
            external,
            graph_reversible,
        })
    }

    /// Plan one writeback effect (effect `i`) of an action in `workspace`:
    /// writeback is on, the target is registered, serves the workspace and
    /// has the route, and the body renders strictly. `Err` is the failed
    /// check's message (names and placeholders only, never a value).
    fn plan_writeback(
        &self,
        env: &Templates<'_>,
        workspace: &str,
        i: usize,
        wb: &WritebackEffect,
    ) -> std::result::Result<PlannedExternal, String> {
        let what = format!("effect {i}: writeback {}.{}", wb.target, wb.route);
        if !self.options().writeback {
            return Err(format!("{what}: writeback is off on this server ([writeback] enabled = false)"));
        }
        if !self.store().keeps_outbox() {
            return Err(format!("{what}: this store keeps no outbox"));
        }
        let target = match self.store().kv_get(crate::automate::NS_WRITEBACK_TARGET, &wb.target) {
            Ok(Some(v)) if !v.is_null() => v,
            Ok(_) => return Err(format!("{what}: target {} is not registered", wb.target)),
            Err(e) => return Err(format!("{what}: reading target {}: {e}", wb.target)),
        };
        let serves = target
            .get("workspaces")
            .and_then(Value::as_array)
            .is_some_and(|ws| ws.iter().filter_map(Value::as_str).any(|w| w == "*" || w == workspace));
        if !serves {
            return Err(format!("{what}: target {} does not serve workspace {workspace}", wb.target));
        }
        if target.get("routes").and_then(|r| r.get(&wb.route)).is_none() {
            return Err(format!("{what}: target {} has no route {}", wb.target, wb.route));
        }
        let body = if wb.body.is_null() { json!({}) } else { wb.body.clone() };
        let body = env.render_strict(&body).map_err(|e| match e {
            Error::Invalid(m) => format!("{what}: {m}"),
            other => format!("{what}: {other}"),
        })?;
        Ok(PlannedExternal { effect: i, target: wb.target.clone(), route: wb.route.clone(), body })
    }

    /// [`plan_action`](Self::plan_action) plus its consequences: the plan's
    /// mutations are staged on a [`fork`](Self::fork), and the closure of the
    /// live graph is compared with the closure of the fork, both as the
    /// planner may see them. Opt-in, because it copies the graph and runs the
    /// reasoner twice; nothing is written. An invalid plan, or mutations that
    /// do not stage, give a skipped block that says why.
    pub fn plan_action_consequences(
        &self,
        ctx: &ActionContext,
        name: &str,
        params: &Map<String, Value>,
    ) -> Result<ActionPlan> {
        let mut plan = self.plan_action(ctx, name, params)?;
        plan.consequences = Some(self.action_consequences(ctx, &plan)?);
        Ok(plan)
    }

    fn action_consequences(&self, ctx: &ActionContext, plan: &ActionPlan) -> Result<Consequences> {
        if !plan.valid {
            return Ok(Consequences::skipped(
                "the plan is not valid, so nothing would change",
                ACTION_CONSEQUENCES_MEANS,
            ));
        }
        let p = &ctx.principal;
        let fork = self.fork();
        if let Err(e) = fork.apply(ctx, plan.mutations.clone()) {
            return Ok(Consequences::skipped(
                format!("the mutations do not stage on a fork: {e}"),
                ACTION_CONSEQUENCES_MEANS,
            ));
        }
        let schema = self.schema();
        let before = self.reader(p)?;
        let after = fork.reader(p)?;
        let can_before = |e: &Entity| policy::visible(p, e, &before);
        let can_after = |e: &Entity| policy::visible(p, e, &after);
        Ok(data_consequences(&schema, &before, &after, &can_before, &can_after))
    }

    /// Plan, then apply when valid and (if required) confirmed. The plan is
    /// made again inside the commit (see [`apply_planned`](Self::apply_planned)),
    /// so a precondition that stopped holding in between rejects the action
    /// instead of committing a stale plan.
    pub fn apply_action(
        &self,
        ctx: &ActionContext,
        name: &str,
        params: &Map<String, Value>,
        opts: ApplyOptions,
    ) -> Result<ActionReceipt> {
        let id = new_id("act");
        let ts = now_ms();
        let seed = PlanSeed::of(&id, ts);
        let plan = self.read(|g, schema| self.plan_on(g, schema, ctx, name, params, &seed))?;
        let mut receipt = ActionReceipt {
            id,
            action: name.to_string(),
            status: "proposed".into(),
            actor: ctx.principal.id.clone(),
            workspace: ctx.workspace.clone(),
            request_id: ctx.request_id.clone(),
            ts,
            plan,
            result: None,
            reason: None,
            touched: BTreeMap::new(),
            external: Vec::new(),
            note: None,
        };
        if !receipt.plan.valid {
            let failed: Vec<String> = receipt.plan.checks.iter().filter(|c| !c.ok).map(|c| c.message.clone()).collect();
            let reason = if failed.is_empty() { "nothing to change".into() } else { failed.join("; ") };
            return Ok(self.reject(ctx, receipt, failed, reason));
        }
        if opts.dry_run {
            receipt.reason = Some("dry run".into());
            return Ok(receipt);
        }
        let not_reversible = if receipt.plan.reversible {
            ""
        } else if receipt.plan.external.is_empty() {
            ", not reversible"
        } else {
            ", not reversible (an external write: action.undo does not undo it)"
        };
        if receipt.plan.needs_confirmation && !opts.confirm {
            receipt.reason = Some(format!("risk {:?}{not_reversible}: pass confirm=true to apply", receipt.plan.risk));
            return Ok(receipt);
        }
        // An agent cannot confirm its own risky action: a person (any
        // principal that is not an agent) applies it with confirm=true.
        if receipt.plan.needs_confirmation && ctx.principal.is_agent() {
            receipt.reason = Some(format!(
                "risk {:?}{not_reversible}: {} is an agent and cannot confirm it; a person must apply it with confirm=true",
                receipt.plan.risk, ctx.principal.id
            ));
            return Ok(receipt);
        }
        let ctx = ctx.clone().with_intent(format!("action:{name}"));
        self.apply_planned(&ctx, receipt, Extra::default())
    }

    /// Commit a receipt whose plan is valid and, when it needs it, confirmed.
    ///
    /// Inside the commit, on the locked graph, the action is planned again
    /// with the receipt's parameters, clock and id ([`plan_on`](Self::plan_on)).
    /// When that plan is no longer valid, or its risk rose, or it stopped
    /// being reversible, nothing is written: the receipt comes back
    /// `rejected` with a reason that starts with `precondition_changed`, and a
    /// failed audit event is recorded. Otherwise the receipt records the plan
    /// as committed, and the edits, the `action.<name>` event, the receipt and
    /// `extra` commit together. The caller sets `ctx`'s intent.
    pub(crate) fn apply_planned(
        &self,
        ctx: &ActionContext,
        receipt: ActionReceipt,
        extra: Extra,
    ) -> Result<ActionReceipt> {
        if !receipt.plan.valid {
            return Err(Error::invalid(format!("receipt {} has no valid plan to apply", receipt.id)));
        }
        let name = receipt.action.clone();
        let seed = PlanSeed::of(&receipt.id, receipt.ts);
        let params = receipt.plan.params.clone();
        let target = audit_target(&params);
        let mut refused: Option<Vec<String>> = None;
        let committed = self.transact(ctx, |o, g| {
            let schema = o.schema();
            let fresh = o.plan_on(g, &schema, ctx, &name, &params, &seed)?;
            if let Some(why) = drift(&receipt.plan, &fresh) {
                let msg = format!("{PRECONDITION_CHANGED}: {}", why.join("; "));
                refused = Some(why);
                return Err(Error::Conflict(msg));
            }
            let mut r = receipt.clone();
            r.status = "applied".into();
            r.plan = fresh;
            let (rows, calls) = writeback_rows(&r.id, &r.plan, ctx, crate::outbox::SOURCE_ACTION, None);
            r.note = (!calls.is_empty()).then(|| RECEIPT_NOTE.to_string());
            r.external = calls;
            let mut muts = r.plan.mutations.clone();
            let mut payload = json!({ "params": r.plan.params, "receipt": r.id, "risk": r.plan.risk });
            if !r.external.is_empty() {
                payload["external"] = json!(r.external.iter().map(|x| &x.outbox).collect::<Vec<_>>());
            }
            muts.push(Mutation::Event(EventInput {
                action: format!("action.{name}"),
                target: target.clone(),
                payload,
                ..Default::default()
            }));
            let mut x = extra.clone();
            x.outbox.extend(rows);
            x.kv.insert(0, (RECEIPTS.into(), r.id.clone(), serde_json::to_value(&r)?));
            Ok((muts, x, r))
        });
        let (result, mut receipt) = match committed {
            Ok(done) => done,
            Err(e) => {
                return match refused {
                    Some(why) => {
                        let reason = format!("{PRECONDITION_CHANGED}: {}", why.join("; "));
                        Ok(self.reject(ctx, receipt, why, reason))
                    }
                    None => Err(e),
                };
            }
        };
        receipt.touched =
            self.read(|g, _| result.entities.iter().map(|id| (id.clone(), g.get(id).map(|e| e.revision))).collect());
        receipt.result = Some(result);
        // Persist the result too (best effort; the receipt itself committed atomically).
        let _ = self.store().kv_put(RECEIPTS, &receipt.id, &serde_json::to_value(&receipt)?, None);
        Ok(receipt)
    }

    /// Mark `receipt` rejected and record a failed audit event (best effort).
    fn reject(
        &self,
        ctx: &ActionContext,
        mut receipt: ActionReceipt,
        failed: Vec<String>,
        reason: String,
    ) -> ActionReceipt {
        receipt.status = "rejected".into();
        receipt.reason = Some(reason);
        let audit = EventInput {
            action: format!("action.{}", receipt.action),
            target: audit_target(&receipt.plan.params),
            outcome: Outcome::Failure,
            payload: json!({ "params": receipt.plan.params, "rejected": failed }),
            ..Default::default()
        };
        let _ = self.apply(ctx, vec![Mutation::Event(audit)]);
        receipt
    }

    /// A receipt, for its actor or an admin of its workspace. External
    /// writes carry their outbox rows' current states.
    pub fn action_receipt(&self, p: &Principal, id: &str) -> Result<ActionReceipt> {
        let v = self.store().kv_get(RECEIPTS, id)?.ok_or_else(|| Error::not_found(format!("receipt {id}")))?;
        let mut r: ActionReceipt = serde_json::from_value(v)?;
        if r.actor != p.id && !(p.is_privileged() && p.in_workspace(&r.workspace)) {
            return Err(Error::not_found(format!("receipt {id}")));
        }
        if !r.external.is_empty() && self.store().keeps_outbox() {
            let q = crate::outbox::OutboxQuery {
                receipt: Some(r.id.clone()),
                limit: Some(crate::outbox::MAX_LIST),
                ..Default::default()
            };
            let rows = self.store().outbox_list(&q)?;
            for x in &mut r.external {
                if let Some(row) = rows.iter().find(|row| row.id == x.outbox) {
                    x.state = row.state;
                    x.attempts = row.attempts;
                }
            }
        }
        Ok(r)
    }

    /// Undo an applied action. Refuses when the touched entities changed since,
    /// unless `force`.
    pub fn undo_action(&self, ctx: &ActionContext, id: &str, force: bool) -> Result<ActionReceipt> {
        let mut r = self.action_receipt(&ctx.principal, id)?;
        match r.status.as_str() {
            "applied" => {}
            "undone" => return Err(Error::Conflict(format!("receipt {id} was already undone"))),
            other => return Err(Error::invalid(format!("receipt {id} is {other}, nothing to undo"))),
        }
        let graph_only = !r.plan.external.is_empty();
        if graph_only {
            // The external write is never undone; with force, the graph part is.
            if r.plan.graph_reversible != Some(true) {
                return Err(Error::invalid(format!(
                    "action {} is not reversible (its graph edits cannot be undone, and {})",
                    r.action,
                    NOT_REVERSIBLE_NOTE.to_lowercase().trim_end_matches('.')
                )));
            }
            if !force {
                return Err(Error::invalid(format!(
                    "action {} made an external write, which is not reversible: action.undo does not undo it. Pass \
                     force=true to undo the graph edits only",
                    r.action
                )));
            }
        } else if !r.plan.reversible {
            return Err(Error::invalid(format!("action {} is not reversible", r.action)));
        }
        if !force {
            let changed: Vec<Id> = self.read(|g, _| {
                r.touched
                    .iter()
                    .filter(|(id, rev)| g.get(id).map(|e| e.revision) != **rev)
                    .map(|(id, _)| id.clone())
                    .collect()
            });
            if !changed.is_empty() {
                return Err(Error::Conflict(format!(
                    "{} changed after the action (pass force=true to undo anyway)",
                    changed.join(", ")
                )));
            }
        }
        let mut muts: Vec<Mutation> = r.plan.inverse.iter().rev().flat_map(|g| g.iter().cloned()).collect();
        let mut payload = json!({ "receipt": r.id, "action": r.action });
        if graph_only {
            payload["external_not_undone"] = json!(r.external.iter().map(|x| &x.outbox).collect::<Vec<_>>());
        }
        muts.push(Mutation::Event(EventInput { action: "action.undo".into(), payload, ..Default::default() }));
        r.status = "undone".into();
        r.reason = Some(if graph_only {
            format!(
                "graph edits undone by {} (force); the external write was not undone: {NOT_REVERSIBLE_NOTE}",
                ctx.principal.id
            )
        } else {
            format!("undone by {}", ctx.principal.id)
        });
        let extra = Extra {
            kv: vec![(RECEIPTS.into(), r.id.clone(), serde_json::to_value(&r)?)],
            redirects: Vec::new(),
            ..Default::default()
        };
        self.apply_inner(ctx, muts, extra)?;
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    /// A project, an open issue, a public person and a restricted person only admins see.
    fn world() -> (Ontology, ActionContext) {
        let o = Ontology::in_memory(Schema::enterprise());
        let ctx = ActionContext::new(Principal::admin("lead"), "acme");
        o.apply(
            &ctx,
            vec![
                Mutation::Upsert(EntityInput::new("project", "alpha", "Project Alpha").state("active")),
                Mutation::Upsert(EntityInput::new("issue", "1", "Payment timeout").state("open")),
                Mutation::Upsert(EntityInput::new("person", "aiko", "Aiko Sato").prop("level", 3)),
                Mutation::Upsert(
                    EntityInput::new("person", "boss", "The Boss")
                        .prop("salary", 250_000)
                        .prop("visibility", "restricted")
                        .prop("acl", json!([])),
                ),
            ],
        )
        .unwrap();
        (o, ctx)
    }

    /// Render `v` with string parameters, as `p` sees the graph.
    fn render(o: &Ontology, p: &Principal, args: Value, v: Value, strict: bool) -> Result<Option<Value>> {
        let args = params(args);
        o.read(|g, _| {
            let see = |e: &Entity| policy::visible(p, e, g);
            let t = Templates::new(g, &args, &p.id, &see, &PlanSeed::fresh());
            if strict { t.render_strict(&v).map(Some) } else { Ok(t.render_value(&v)) }
        })
    }

    #[test]
    fn templates_hide_fields_of_invisible_entities() {
        let (o, _) = world();
        let member = Principal::member("bob", "acme");
        let tpl = json!({ "salary": "{who.salary}", "title": "{who.title}", "who": "{who}" });
        let admin = render(&o, &Principal::admin("x"), json!({ "who": "person:boss" }), tpl.clone(), false).unwrap();
        assert_eq!(admin, Some(json!({ "salary": 250_000, "title": "The Boss", "who": "person:boss" })));
        // The member may name the entity, but it has no fields for them.
        let hidden = render(&o, &member, json!({ "who": "person:boss" }), tpl.clone(), false).unwrap();
        assert_eq!(hidden, Some(json!({ "who": "person:boss" })));
        let visible = render(&o, &member, json!({ "who": "person:aiko" }), json!("{who.level}"), false).unwrap();
        assert_eq!(visible, Some(json!(3)));
        // A hidden entity reads exactly like a missing one, in strict mode too.
        let a = render(&o, &member, json!({ "who": "person:boss" }), json!("{who.salary}"), true).unwrap_err();
        let b = render(&o, &member, json!({ "who": "person:nobody" }), json!("{who.salary}"), true).unwrap_err();
        assert_eq!(a.to_string(), b.to_string());
        assert!(!a.to_string().contains("250"), "{a}");
    }

    #[test]
    fn strict_render_refuses_missing_values_in_nested_objects_and_arrays() {
        let (o, _) = world();
        let p = Principal::admin("x");
        let args = json!({ "who": "person:aiko" });
        let nested = json!({ "a": { "b": ["{who}", { "c": "{gone}" }], "d": "x {who.missing} y" }, "n": 1 });
        // Lenient (action types): keys and items without a value are dropped.
        let lenient = render(&o, &p, args.clone(), nested.clone(), false).unwrap();
        assert_eq!(lenient, Some(json!({ "a": { "b": ["person:aiko", {}] }, "n": 1 })));
        // Strict: the whole render is refused, naming every placeholder and no value.
        let err = render(&o, &p, args.clone(), nested, true).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("invalid_template"), "{msg}");
        assert!(msg.contains("{gone}") && msg.contains("{who.missing}"), "{msg}");
        let in_array = render(&o, &p, args.clone(), json!(["{who}", "{gone}"]), true).unwrap_err();
        assert!(in_array.to_string().contains("{gone}"), "{in_array}");
        // Everything present: strict and lenient agree, types kept.
        let full =
            json!({ "id": "{who}", "level": "{who.level}", "list": ["{who.title}", 2], "text": "hi {who.title}" });
        let strict = render(&o, &p, args.clone(), full.clone(), true).unwrap();
        assert_eq!(strict, render(&o, &p, args, full, false).unwrap());
        assert_eq!(
            strict,
            Some(json!({ "id": "person:aiko", "level": 3, "list": ["Aiko Sato", 2], "text": "hi Aiko Sato" }))
        );
    }

    fn dry_run(o: &Ontology, ctx: &ActionContext, name: &str, args: Value) -> ActionReceipt {
        let r = o.apply_action(ctx, name, &params(args), ApplyOptions { dry_run: true, confirm: false }).unwrap();
        assert!(r.plan.valid, "{:?}", r.plan.checks);
        r
    }

    #[test]
    fn apply_planned_refuses_a_plan_whose_precondition_changed() {
        let (o, ctx) = world();
        let planned = dry_run(&o, &ctx, "resolve", json!({ "issue": "issue:1" }));
        // Someone closes the issue between planning and commit.
        o.apply(
            &ctx,
            vec![Mutation::Patch(EntityPatch {
                id: "issue:1".into(),
                state: Some("closed".into()),
                ..Default::default()
            })],
        )
        .unwrap();
        let before = o.seq();
        let r = o.apply_planned(&ctx, planned, Extra::default()).unwrap();
        assert_eq!(r.status, "rejected");
        let reason = r.reason.unwrap();
        assert!(reason.starts_with("precondition_changed: "), "{reason}");
        assert!(reason.contains("already resolved or closed"), "{reason}");
        assert_eq!(o.get(&Principal::admin("x"), "issue:1").unwrap().state.as_deref(), Some("closed"));
        assert!(o.store().kv_get(RECEIPTS, &r.id).unwrap().is_none(), "no receipt was committed");
        assert_eq!(o.seq(), before, "no state-changing commit");
        let audit = o
            .events(
                &Principal::admin("x"),
                &EventFilter { action: Some("action.resolve".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].outcome, Outcome::Failure);
    }

    #[test]
    fn apply_planned_commits_the_plan_it_showed() {
        let (o, ctx) = world();
        let planned = dry_run(
            &o,
            &ctx,
            "create_task",
            json!({ "title": "Write docs", "project": "project:alpha", "estimate": 2 }),
        );
        let Some(Mutation::Upsert(shown)) = planned.plan.mutations.first() else { panic!("{:?}", planned.plan) };
        let shown = shown.id.clone().unwrap();
        let r = o.apply_planned(&ctx, planned.clone(), Extra::default()).unwrap();
        assert_eq!(r.status, "applied", "{:?}", r.reason);
        assert_eq!(r.result.as_ref().unwrap().entities[0], shown, "the re-plan keeps the seeded id");
        assert_eq!(
            serde_json::to_value(&r.plan.mutations).unwrap(),
            serde_json::to_value(&planned.plan.mutations).unwrap()
        );
        let stored: ActionReceipt =
            serde_json::from_value(o.store().kv_get(RECEIPTS, &r.id).unwrap().unwrap()).unwrap();
        assert_eq!(stored.status, "applied");
    }

    #[test]
    fn apply_planned_refuses_a_plan_that_stopped_being_reversible() {
        let (o, ctx) = world();
        let planned = dry_run(&o, &ctx, "create_task", json!({ "title": "Write docs", "project": "project:alpha" }));
        assert!(planned.plan.reversible && !planned.plan.needs_confirmation);
        let Some(Mutation::Upsert(shown)) = planned.plan.mutations.first() else { panic!("{:?}", planned.plan) };
        let id = shown.id.clone().unwrap();
        let (kind, key) = id.split_once(':').unwrap();
        // The id the plan would create appears first, so the create becomes an update.
        o.apply(&ctx, vec![Mutation::Upsert(EntityInput::new(kind, key, "Already here").state("todo"))]).unwrap();
        let r = o.apply_planned(&ctx, planned, Extra::default()).unwrap();
        assert_eq!(r.status, "rejected");
        assert!(r.reason.as_deref().unwrap().contains("no longer reversible"), "{:?}", r.reason);
        assert_eq!(o.get(&Principal::admin("x"), &id).unwrap().title, "Already here");
    }
}
