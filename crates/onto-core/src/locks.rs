//! Schema locks: definitions an admin has marked as not to be removed or
//! broken by a live schema change.
//!
//! A lock names one definition by section and name and lives in the KV
//! namespace [`NS_LOCKS`] under the key `<section>/<name>`. The sections are
//! `object_types`, `link_types`, `interfaces`, `rules`, `metrics`,
//! `action_types`, `shapes` and `properties` (a property is named
//! `<object type>.<property>`).
//!
//! A schema change violates a lock when it deletes or renames the locked
//! definition (a property lock is also violated by deleting or renaming the
//! object type that declares it, or by a `put_object_type` that drops the
//! property), or when it changes the locked definition and that change is
//! breaking. Every violation is listed in the plan
//! ([`crate::SchemaPlan::locked_violations`]). Without
//! `override_locks` the plan is not applicable; the override is honoured for
//! admins only, and an overridden violation stays in the plan, marked
//! `overridden`, and in the `schema.change` audit event. Proposal approval
//! re-plans the change set, so locks guard proposals as well; a proposal's
//! override is honoured only when both its proposer and its approver are
//! admins, so an agent cannot route an override through an approval.
//!
//! When an overridden change deletes a locked definition the lock is
//! released, and when it renames one the lock moves to the new name, in the
//! same transaction: a lock always names a definition that exists.
//!
//! Locking and unlocking are admin-only (agents never), audited as
//! `schema.lock` and `schema.unlock` events (refusals as
//! `authorization.denied`), and committed through the transaction path every
//! write takes. An unlock leaves a tombstone record, so a key's history stays
//! readable and the write remains one atomic batch.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::engine::{ActionContext, Extra};
use crate::live_schema::{ChangeImpact, SchemaChange, SchemaChangeSet};
use crate::policy::{Access, Principal, Role};
use crate::schema::Schema;
use crate::store::Store;
use crate::time::{fmt_ts, now_ms};
use crate::{Error, Ontology, Result};

/// KV namespace holding one lock record per `<section>/<name>` key.
pub const NS_LOCKS: &str = "schema.lock";

/// Sections a lock can name.
pub const SECTIONS: &[&str] =
    &["object_types", "link_types", "interfaces", "rules", "metrics", "action_types", "shapes", "properties"];

/// One active lock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaLock {
    pub section: String,
    /// The definition's name (`issue`, `depends_on`, `issue.priority`).
    pub name: String,
    #[serde(default)]
    pub reason: String,
    pub locked_by: String,
    /// When it was locked (RFC 3339).
    pub at: String,
}

impl SchemaLock {
    /// The KV key of this lock.
    pub fn key(&self) -> String {
        key(&self.section, &self.name)
    }
}

/// The stored form: a lock, or the tombstone an unlock leaves behind.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    #[serde(flatten)]
    lock: SchemaLock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unlocked_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unlocked_at: Option<String>,
}

impl Record {
    fn active(&self) -> bool {
        self.unlocked_by.is_none()
    }

    fn released(lock: &SchemaLock, by: &str, at: &str) -> Value {
        let r = Record { lock: lock.clone(), unlocked_by: Some(by.to_string()), unlocked_at: Some(at.to_string()) };
        serde_json::to_value(r).unwrap_or(Value::Null)
    }
}

/// How a change touches a locked definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Touch {
    /// The change removes the definition.
    Delete,
    /// The change moves the definition to another name.
    Rename,
    /// The change edits the definition, and the edit is breaking.
    BreakingChange,
}

/// A change that deletes, renames or breaks a locked definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockViolation {
    pub section: String,
    pub name: String,
    /// The offending change (`delete_object_type issue`).
    pub change: String,
    pub touch: Touch,
    /// What the change does to the locked definition.
    pub reason: String,
    /// The new name, for a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_to: Option<String>,
    /// Why the definition was locked.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lock_reason: String,
    pub locked_by: String,
    /// An admin passed `override_locks`: the change may proceed.
    pub overridden: bool,
}

/// `<section>/<name>`.
pub fn key(section: &str, name: &str) -> String {
    format!("{section}/{name}")
}

/// May `p` lock, unlock and override locks? Admins (and the engine), never agents.
pub fn can_manage(p: &Principal) -> bool {
    !p.is_agent() && (p.has(Role::Admin) || p.has(Role::System))
}

/// Does `schema` define `name` in `section`? (A property must be declared
/// by the object type itself, the definition a change would delete.)
pub fn target_exists(schema: &Schema, section: &str, name: &str) -> std::result::Result<(), String> {
    let found = match section {
        "object_types" => schema.object_types.contains_key(name),
        "link_types" => schema.link_types.contains_key(name),
        "interfaces" => schema.interfaces.contains_key(name),
        "rules" => schema.rules.iter().any(|r| r.name == name),
        "metrics" => schema.metrics.contains_key(name),
        "action_types" => schema.action_types.contains_key(name),
        "shapes" => schema.shapes.contains_key(name),
        "properties" => {
            let Some((kind, prop)) = name.split_once('.') else {
                return Err(format!("a property lock names <object type>.<property>, not '{name}'"));
            };
            let Some(ty) = schema.object_types.get(kind) else {
                return Err(format!("unknown object type {kind}"));
            };
            if !ty.properties.contains_key(prop) {
                return Err(format!("{kind} does not declare property {prop} (lock it on the type that declares it)"));
            }
            true
        }
        other => return Err(format!("unknown section '{other}' (one of {})", SECTIONS.join(", "))),
    };
    if found { Ok(()) } else { Err(format!("{section} has no definition named {name}")) }
}

/// The active locks in `store`, sorted by key.
pub(crate) fn read_locks(store: &dyn Store) -> Result<Vec<SchemaLock>> {
    let mut out: Vec<SchemaLock> = store
        .kv_scan(NS_LOCKS, "")?
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_value::<Record>(v).ok())
        .filter(Record::active)
        .map(|r| r.lock)
        .collect();
    out.sort_by_key(SchemaLock::key);
    Ok(out)
}

fn read_record(store: &dyn Store, section: &str, name: &str) -> Result<Option<Record>> {
    Ok(store.kv_get(NS_LOCKS, &key(section, name))?.and_then(|v| serde_json::from_value::<Record>(v).ok()))
}

/// What one change does to one definition.
struct Touched {
    section: &'static str,
    name: String,
    touch: Touch,
    reason: String,
    renamed_to: Option<String>,
}

fn touched(section: &'static str, name: &str, touch: Touch, reason: impl Into<String>) -> Touched {
    Touched { section, name: name.to_string(), touch, reason: reason.into(), renamed_to: None }
}

fn renamed(section: &'static str, name: &str, to: String, reason: impl Into<String>) -> Touched {
    Touched { section, name: name.to_string(), touch: Touch::Rename, reason: reason.into(), renamed_to: Some(to) }
}

/// The definitions change `c` deletes, renames or edits (edits are
/// [`Touch::BreakingChange`] candidates; the caller keeps them only when the
/// change is breaking).
fn touches(base: &Schema, c: &SchemaChange) -> Vec<Touched> {
    let deletes = "deletes the locked definition";
    let edits = "edits the locked definition";
    let mut out = Vec::new();
    let base_props = |kind: &str| -> Vec<String> {
        base.object_types.get(kind).map(|t| t.properties.keys().cloned().collect()).unwrap_or_default()
    };
    match c {
        SchemaChange::PutObjectType { name, def, .. } => {
            if let Some(old) = base.object_types.get(name) {
                out.push(touched("object_types", name, Touch::BreakingChange, edits));
                for (p, pdef) in &old.properties {
                    let prop = format!("{name}.{p}");
                    match def.properties.get(p) {
                        None => out.push(touched(
                            "properties",
                            &prop,
                            Touch::Delete,
                            format!("redefines object type {name} without the locked property"),
                        )),
                        Some(d) if d != pdef => out.push(touched("properties", &prop, Touch::BreakingChange, edits)),
                        Some(_) => {}
                    }
                }
            }
        }
        SchemaChange::DeleteObjectType { name, .. } => {
            out.push(touched("object_types", name, Touch::Delete, deletes));
            for p in base_props(name) {
                out.push(touched(
                    "properties",
                    &format!("{name}.{p}"),
                    Touch::Delete,
                    format!("deletes object type {name}, which declares the locked property"),
                ));
            }
        }
        SchemaChange::RenameObjectType { from, to } => {
            out.push(renamed("object_types", from, to.clone(), format!("renames the locked definition to {to}")));
            for p in base_props(from) {
                out.push(renamed(
                    "properties",
                    &format!("{from}.{p}"),
                    format!("{to}.{p}"),
                    format!("renames object type {from}, which declares the locked property, to {to}"),
                ));
            }
        }
        SchemaChange::PutProperty { object_type, name, .. } => {
            out.push(touched("object_types", object_type, Touch::BreakingChange, edits));
            out.push(touched("properties", &format!("{object_type}.{name}"), Touch::BreakingChange, edits));
        }
        SchemaChange::DeleteProperty { object_type, name, .. } => {
            out.push(touched("object_types", object_type, Touch::BreakingChange, edits));
            out.push(touched("properties", &format!("{object_type}.{name}"), Touch::Delete, deletes));
        }
        SchemaChange::RenameProperty { object_type, from, to } => {
            out.push(touched("object_types", object_type, Touch::BreakingChange, edits));
            out.push(renamed(
                "properties",
                &format!("{object_type}.{from}"),
                format!("{object_type}.{to}"),
                format!("renames the locked definition to {object_type}.{to}"),
            ));
        }
        SchemaChange::PutLinkType { name, .. } => out.push(touched("link_types", name, Touch::BreakingChange, edits)),
        SchemaChange::DeleteLinkType { name, .. } => out.push(touched("link_types", name, Touch::Delete, deletes)),
        SchemaChange::RenameLinkType { from, to } => {
            out.push(renamed("link_types", from, to.clone(), format!("renames the locked definition to {to}")))
        }
        SchemaChange::PutInterface { name, .. } => out.push(touched("interfaces", name, Touch::BreakingChange, edits)),
        SchemaChange::DeleteInterface { name } => out.push(touched("interfaces", name, Touch::Delete, deletes)),
        SchemaChange::PutActionType { name, .. } => {
            out.push(touched("action_types", name, Touch::BreakingChange, edits))
        }
        SchemaChange::DeleteActionType { name } => out.push(touched("action_types", name, Touch::Delete, deletes)),
        SchemaChange::PutRule { def } => out.push(touched("rules", &def.name, Touch::BreakingChange, edits)),
        SchemaChange::DeleteRule { name } => out.push(touched("rules", name, Touch::Delete, deletes)),
        SchemaChange::PutShape { name, .. } => out.push(touched("shapes", name, Touch::BreakingChange, edits)),
        SchemaChange::DeleteShape { name } => out.push(touched("shapes", name, Touch::Delete, deletes)),
        SchemaChange::PutMetric { name, .. } => out.push(touched("metrics", name, Touch::BreakingChange, edits)),
        SchemaChange::DeleteMetric { name } => out.push(touched("metrics", name, Touch::Delete, deletes)),
        SchemaChange::PutGlossaryTerm { .. }
        | SchemaChange::DeleteGlossaryTerm { .. }
        | SchemaChange::SetAnalysis { .. } => {}
    }
    out
}

/// The locks `set` violates. `impacts` are the plan's per-change impacts (by
/// change index); an edit violates a lock only when its impact is breaking.
/// `overridden` marks every violation when the override is honoured.
pub fn violations(
    base: &Schema,
    set: &SchemaChangeSet,
    impacts: &[ChangeImpact],
    locks: &[SchemaLock],
    overridden: bool,
) -> Vec<LockViolation> {
    if locks.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<LockViolation> = Vec::new();
    for (i, c) in set.changes.iter().enumerate() {
        let impact = impacts.get(i);
        for t in touches(base, c) {
            let Some(lock) = locks.iter().find(|l| l.section == t.section && l.name == t.name) else { continue };
            let reason = match t.touch {
                Touch::BreakingChange => match impact.filter(|x| x.breaking) {
                    Some(x) if x.reasons.is_empty() => {
                        format!("a breaking change to the locked definition ({})", t.reason)
                    }
                    Some(x) => format!("a breaking change to the locked definition: {}", x.reasons.join("; ")),
                    None => continue,
                },
                _ => t.reason,
            };
            let change = format!("{} {}", c.op(), c.target());
            if out.iter().any(|v| v.section == lock.section && v.name == lock.name && v.change == change) {
                continue;
            }
            out.push(LockViolation {
                section: lock.section.clone(),
                name: lock.name.clone(),
                change,
                touch: t.touch,
                reason,
                renamed_to: t.renamed_to,
                lock_reason: lock.reason.clone(),
                locked_by: lock.locked_by.clone(),
                overridden,
            });
        }
    }
    out
}

/// The refusal text for violations that are not overridden.
pub(crate) fn refusal_text(violations: &[LockViolation]) -> String {
    let parts: Vec<String> = violations
        .iter()
        .filter(|v| !v.overridden)
        .map(|v| {
            let why = if v.lock_reason.is_empty() { String::new() } else { format!(": {}", v.lock_reason) };
            format!("{}/{} ({}: {}; locked by {}{why})", v.section, v.name, v.change, v.reason, v.locked_by)
        })
        .collect();
    format!("schema change touches locked definitions: {}; an admin may pass override_locks", parts.join("; "))
}

/// What becomes of the locks an overridden change deletes or renames.
#[derive(Default)]
pub(crate) struct Followed {
    /// KV writes: tombstones for released locks, records for moved ones.
    pub kv: Vec<(String, String, Value)>,
    /// Keys of released locks.
    pub released: Vec<String>,
    /// `old key → new key` of moved locks.
    pub moved: Vec<String>,
}

/// Release the locks of deleted definitions and move the locks of renamed
/// ones (overridden violations only).
pub(crate) fn follow(violations: &[LockViolation], locks: &[SchemaLock], by: &str, at: &str) -> Followed {
    let mut f = Followed::default();
    let mut done: Vec<String> = Vec::new();
    for v in violations.iter().filter(|v| v.overridden) {
        let k = key(&v.section, &v.name);
        if done.contains(&k) {
            continue;
        }
        let Some(lock) = locks.iter().find(|l| l.key() == k) else { continue };
        match (v.touch, &v.renamed_to) {
            (Touch::Delete, _) => {
                f.kv.push((NS_LOCKS.into(), k.clone(), Record::released(lock, by, at)));
                f.released.push(k.clone());
            }
            (Touch::Rename, Some(to)) => {
                let moved = SchemaLock { name: to.clone(), ..lock.clone() };
                let new_key = moved.key();
                f.kv.push((NS_LOCKS.into(), k.clone(), Record::released(lock, by, at)));
                let record = Record { lock: moved, unlocked_by: None, unlocked_at: None };
                f.kv.push((NS_LOCKS.into(), new_key.clone(), serde_json::to_value(record).unwrap_or(Value::Null)));
                f.moved.push(format!("{k} → {new_key}"));
            }
            _ => continue,
        }
        done.push(k);
    }
    f
}

impl Ontology {
    fn authorize_lock(&self, ctx: &ActionContext, action: &str) -> Result<()> {
        let p = &ctx.principal;
        if can_manage(p) {
            return Ok(());
        }
        let reason = if p.is_agent() {
            format!("{} is an agent: agents may not lock or unlock schema definitions", p.id)
        } else {
            format!("{} may not lock or unlock schema definitions (admins only)", p.id)
        };
        let mut c = ctx.clone();
        c.intent = Some(action.to_string());
        self.audit_denied(&c, &reason, now_ms());
        Err(Error::Denied(reason))
    }

    /// The active schema locks, sorted by `<section>/<name>`.
    pub fn schema_locks(&self, p: &Principal) -> Result<Vec<SchemaLock>> {
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        read_locks(&**self.store())
    }

    /// Lock a definition of the active schema (admins only). Locking a
    /// definition that is already locked returns the existing lock unchanged.
    pub fn schema_lock(&self, ctx: &ActionContext, section: &str, name: &str, reason: &str) -> Result<SchemaLock> {
        self.authorize_lock(ctx, "schema.lock")?;
        if !SECTIONS.contains(&section) {
            return Err(Error::invalid(format!("unknown section '{section}' (one of {})", SECTIONS.join(", "))));
        }
        let (_, lock) = self.transact(ctx, |o, _| {
            target_exists(&o.schema(), section, name).map_err(Error::NotFound)?;
            if let Some(r) = read_record(&**o.store(), section, name)?
                && r.active()
            {
                return Ok((Vec::new(), Extra::default(), r.lock));
            }
            let now = now_ms();
            let lock = SchemaLock {
                section: section.to_string(),
                name: name.to_string(),
                reason: reason.trim().to_string(),
                locked_by: ctx.principal.id.clone(),
                at: fmt_ts(now),
            };
            let record = Record { lock: lock.clone(), unlocked_by: None, unlocked_at: None };
            let payload = json!({ "lock": lock.key(), "section": section, "name": name, "reason": lock.reason });
            let extra = Extra {
                kv: vec![(NS_LOCKS.into(), lock.key(), serde_json::to_value(&record)?)],
                events: vec![o.event(ctx, &ctx.workspace, "schema.lock", None, now, payload, Vec::new())],
                ..Default::default()
            };
            Ok((Vec::new(), extra, lock))
        })?;
        Ok(lock)
    }

    /// Release a lock (admins only); returns the lock that was released.
    pub fn schema_unlock(&self, ctx: &ActionContext, section: &str, name: &str) -> Result<SchemaLock> {
        self.authorize_lock(ctx, "schema.unlock")?;
        let (_, lock) = self.transact(ctx, |o, _| {
            let lock = match read_record(&**o.store(), section, name)? {
                Some(r) if r.active() => r.lock,
                _ => return Err(Error::not_found(format!("schema lock {}", key(section, name)))),
            };
            let now = now_ms();
            let payload = json!({ "lock": lock.key(), "section": section, "name": name, "reason": lock.reason });
            let extra = Extra {
                kv: vec![(NS_LOCKS.into(), lock.key(), Record::released(&lock, &ctx.principal.id, &fmt_ts(now)))],
                events: vec![o.event(ctx, &ctx.workspace, "schema.unlock", None, now, payload, Vec::new())],
                ..Default::default()
            };
            Ok((Vec::new(), extra, lock))
        })?;
        Ok(lock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lock(section: &str, name: &str) -> SchemaLock {
        SchemaLock {
            section: section.into(),
            name: name.into(),
            reason: "core".into(),
            locked_by: "ada".into(),
            at: "2026-10-24T12:00:00Z".into(),
        }
    }

    fn set(v: Value) -> SchemaChangeSet {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn targets_must_exist() {
        let s = Schema::enterprise();
        assert!(target_exists(&s, "object_types", "issue").is_ok());
        assert!(target_exists(&s, "link_types", "depends_on").is_ok());
        assert!(target_exists(&s, "rules", "responsible_team").is_ok());
        assert!(target_exists(&s, "rules", "handled_by_team").is_err(), "rules are named by rule, not relation");
        assert!(target_exists(&s, "object_types", "nope").is_err());
        assert!(target_exists(&s, "properties", "issue").is_err());
        assert!(target_exists(&s, "properties", "issue.due").is_err(), "inherited from work_item, not declared");
        assert!(target_exists(&s, "properties", "issue.priority").is_ok());
        assert!(target_exists(&s, "colors", "x").is_err());
    }

    #[test]
    fn deletes_and_renames_violate_edits_only_when_breaking() {
        let s = Schema::enterprise();
        let locks = vec![lock("object_types", "task"), lock("link_types", "depends_on")];
        let del = set(json!({ "changes": [{ "op": "delete_object_type", "name": "task" }] }));
        let v = violations(&s, &del, &[], &locks, false);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].touch, Touch::Delete);
        assert!(!v[0].overridden);
        let ren = set(json!({ "changes": [{ "op": "rename_link_type", "from": "depends_on", "to": "needs" }] }));
        let v = violations(&s, &ren, &[], &locks, true);
        assert_eq!(v[0].renamed_to.as_deref(), Some("needs"));
        assert!(v[0].overridden);
        let mut def = s.link_types["depends_on"].clone();
        def.transitive = true;
        let edit = SchemaChangeSet {
            changes: vec![SchemaChange::PutLinkType { name: "depends_on".into(), def, migrations: Vec::new() }],
            ..Default::default()
        };
        assert!(violations(&s, &edit, &[ChangeImpact::default()], &locks, false).is_empty());
        let breaking = ChangeImpact { breaking: true, reasons: vec!["2 links break".into()], ..Default::default() };
        let v = violations(&s, &edit, &[breaking], &locks, false);
        assert_eq!(v[0].touch, Touch::BreakingChange);
        assert!(v[0].reason.contains("2 links break"));
    }

    #[test]
    fn follow_releases_and_moves() {
        let locks = vec![lock("object_types", "task"), lock("properties", "task.estimate")];
        let v = vec![
            LockViolation {
                section: "object_types".into(),
                name: "task".into(),
                change: "rename_object_type task → chore".into(),
                touch: Touch::Rename,
                reason: String::new(),
                renamed_to: Some("chore".into()),
                lock_reason: String::new(),
                locked_by: "ada".into(),
                overridden: true,
            },
            LockViolation {
                section: "properties".into(),
                name: "task.estimate".into(),
                change: "delete_property task.estimate".into(),
                touch: Touch::Delete,
                reason: String::new(),
                renamed_to: None,
                lock_reason: String::new(),
                locked_by: "ada".into(),
                overridden: true,
            },
        ];
        let f = follow(&v, &locks, "ada", "now");
        assert_eq!(f.released, vec!["properties/task.estimate"]);
        assert_eq!(f.moved, vec!["object_types/task → object_types/chore"]);
        assert_eq!(f.kv.len(), 3);
    }
}
