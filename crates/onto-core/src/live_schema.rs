//! Live schema: object types, properties, link types, interfaces, action
//! types, rules, shapes, metrics and glossary terms changed as data at runtime.
//!
//! A [`SchemaChangeSet`] is validated (the resulting schema must compile),
//! previewed with an impact report over the live graph ([`SchemaPlan`]:
//! entities and links in scope, breaking or not), migrated explicitly when it
//! breaks data in use ([`Migration`]), and committed atomically with its
//! migration as a new [`SchemaVersion`] plus a `schema.change` audit event.
//! Every version keeps the full schema document and its diff, so any version
//! can be read back, compared, restored or undone.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::action_types::{ActionType, Check};
use crate::engine::{ActionContext, Extra};
use crate::graph::{Graph, LinkKey};
use crate::metrics::MetricDef;
use crate::model::{Entity, EntityInput, EntityRef, Id, LinkInput, Mutation};
use crate::policy::{Access, Principal, Role};
use crate::schema::{Analysis, Cardinality, InterfaceType, LinkType, ObjectType, PropType, PropertyDef, Rule, Schema};
use crate::shape_model::{Shape, ShapePath};
use crate::store::Store;
use crate::time::{fmt_ts, now_ms};
use crate::{Error, Ontology, Result};

/// KV namespace holding the head pointer (`head`) and the config baseline (`config`).
pub const NS_SCHEMA: &str = "schema";
/// KV namespace holding one [`SchemaVersion`] per key (`{version:012}`).
pub const NS_VERSIONS: &str = "schema.version";

/// Who may change the schema, file proposals and approve them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Governance {
    /// Accept schema changes through the API at all.
    pub live_edits: bool,
    /// Roles that may apply a change set directly, without a proposal.
    pub apply_roles: Vec<Role>,
    /// Roles that may approve or reject proposals.
    pub approve_roles: Vec<Role>,
    /// Roles that may file proposals (empty: anyone who can read).
    pub propose_roles: Vec<Role>,
    /// Approvals a proposal needs before it is applied.
    pub required_approvals: usize,
    /// May the proposer approve their own proposal?
    pub allow_self_approval: bool,
}

impl Default for Governance {
    fn default() -> Self {
        Self {
            live_edits: true,
            apply_roles: vec![Role::Admin],
            approve_roles: vec![Role::Admin],
            propose_roles: Vec::new(),
            required_approvals: 1,
            allow_self_approval: false,
        }
    }
}

impl Governance {
    /// Agents never apply schema changes directly, whatever roles they hold.
    pub fn can_apply(&self, p: &Principal) -> bool {
        p.has(Role::System) || (!p.is_agent() && p.has_any(&self.apply_roles))
    }

    /// Agents never approve, whatever roles they hold.
    pub fn can_approve(&self, p: &Principal) -> bool {
        p.has(Role::System) || (!p.is_agent() && p.has_any(&self.approve_roles))
    }

    pub fn can_propose(&self, p: &Principal) -> bool {
        p.allows(Access::Read) && (self.propose_roles.is_empty() || p.is_privileged() || p.has_any(&self.propose_roles))
    }
}

/// Where the active schema comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaMode {
    /// Stored versions are authoritative; edits to the schema files since the
    /// last import are applied on open as a new version.
    #[default]
    Live,
    /// The schema files are authoritative; API changes are refused.
    Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlossarySection {
    Kinds,
    Relations,
    Dimensions,
}

/// One edit to the schema. `put_*` creates or replaces; `delete_*` removes;
/// `rename_*` moves a definition, rewrites references to it and migrates the
/// data that uses it.
// Object type definitions dwarf the other variants; change sets are short
// lists, so boxing them would only cost every caller a `Box::new`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SchemaChange {
    PutObjectType {
        name: String,
        def: ObjectType,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        migrations: Vec<Migration>,
    },
    DeleteObjectType {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        migrations: Vec<Migration>,
    },
    /// Entity ids change from `from:key` to `to:key`; old ids redirect.
    RenameObjectType {
        from: String,
        to: String,
    },
    PutProperty {
        object_type: String,
        name: String,
        def: PropertyDef,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        migrations: Vec<Migration>,
    },
    DeleteProperty {
        object_type: String,
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        migrations: Vec<Migration>,
    },
    RenameProperty {
        object_type: String,
        from: String,
        to: String,
    },
    PutLinkType {
        name: String,
        def: LinkType,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        migrations: Vec<Migration>,
    },
    DeleteLinkType {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        migrations: Vec<Migration>,
    },
    RenameLinkType {
        from: String,
        to: String,
    },
    PutInterface {
        name: String,
        def: InterfaceType,
    },
    DeleteInterface {
        name: String,
    },
    PutActionType {
        name: String,
        def: ActionType,
    },
    DeleteActionType {
        name: String,
    },
    PutRule {
        def: Rule,
    },
    DeleteRule {
        name: String,
    },
    PutShape {
        name: String,
        def: Shape,
    },
    DeleteShape {
        name: String,
    },
    PutMetric {
        name: String,
        def: MetricDef,
    },
    DeleteMetric {
        name: String,
    },
    PutGlossaryTerm {
        section: GlossarySection,
        word: String,
        target: String,
    },
    DeleteGlossaryTerm {
        section: GlossarySection,
        word: String,
    },
    SetAnalysis {
        def: Analysis,
    },
}

/// What to do with existing data that a change would otherwise break.
/// Attached to a change, unset selectors (`kind`, `property`, `rel`) default
/// to the change's own target.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Migration {
    /// Delete entities of a kind that is removed.
    DeleteEntities {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    /// Move entities to another kind (ids become `to:key`; old ids redirect).
    Retype {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        to: String,
    },
    /// Rewrite states the new definition no longer allows.
    MapStates {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default)]
        map: BTreeMap<String, String>,
        /// State for values not in `map`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    /// Replace values that no longer fit (and fill missing required values).
    SetDefault {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        property: Option<String>,
        value: Value,
    },
    /// Coerce values to the new type (`"3"` → 3, 3 → `"3"`, `"true"` → true,
    /// scalar → list); values that cannot be converted are dropped.
    Convert {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        property: Option<String>,
    },
    /// Remove values that no longer fit, or all values of a removed property.
    DropValues {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        property: Option<String>,
    },
    /// Remove links of a removed relation, or links that no longer fit.
    DeleteLinks {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rel: Option<String>,
    },
    /// Re-create links of a removed relation under another relation.
    MoveLinks {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rel: Option<String>,
        to: String,
    },
    /// For a relation that becomes single-valued: keep each source's newest link.
    KeepLatest {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rel: Option<String>,
    },
    /// Accept a break that needs no data change (a renamed inverse, a kind or
    /// relation kept as untyped data in a non-strict ontology).
    Acknowledge {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
    },
}

/// Changes applied together as one schema version.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SchemaChangeSet {
    #[serde(default)]
    pub changes: Vec<SchemaChange>,
    /// Why (stored with the version and the audit event).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Refuse with a conflict when the active schema version differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<u64>,
    /// Migrations for the whole set, in addition to per-change ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub migrations: Vec<Migration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Added,
    Removed,
    Modified,
}

/// One differing definition between two schemas.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffEntry {
    /// `object_types`, `link_types`, `interfaces`, `action_types`, `metrics`,
    /// `rules`, `shapes`, `glossary.kinds|relations|dimensions` or `analysis`.
    pub section: String,
    pub name: String,
    pub change: DiffKind,
    /// Changed fields of a modified entry (`states`, `properties.priority`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SchemaDiff {
    pub entries: Vec<DiffEntry>,
}

impl SchemaDiff {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `+object_types.incident ~link_types.owns -rules.escalate`.
    pub fn summary(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|e| {
                let sign = match e.change {
                    DiffKind::Added => '+',
                    DiffKind::Removed => '-',
                    DiffKind::Modified => '~',
                };
                format!("{sign}{}.{}", e.section, e.name)
            })
            .collect()
    }
}

/// What one change does to the live graph.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChangeImpact {
    pub index: usize,
    pub op: String,
    pub target: String,
    pub summary: String,
    pub breaking: bool,
    /// Why it is breaking (each resolved by a migration, or listed in `needs_migration`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    /// Entities in scope: of the kind (and its subtypes), or holding the property.
    pub entities: usize,
    /// Links in scope: of the relation, or touching removed entities.
    pub links: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample: Vec<Id>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationSummary {
    pub entities_updated: usize,
    pub entities_deleted: usize,
    pub entities_retyped: usize,
    pub links_deleted: usize,
    pub links_moved: usize,
    pub mutations: usize,
}

impl MigrationSummary {
    pub fn is_empty(&self) -> bool {
        self.mutations == 0
    }
}

/// Dry-run result of a change set against the current schema and graph.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SchemaPlan {
    pub base_version: u64,
    /// The result compiles and the migration stages cleanly.
    pub valid: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    /// Some change invalidates existing data or vocabulary in use.
    pub breaking: bool,
    /// Breaking effects no migration resolves; applying is refused until
    /// each one gets a migration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs_migration: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub changes: Vec<ChangeImpact>,
    pub diff: SchemaDiff,
    /// Distinct entities in scope of any change.
    pub entities_affected: usize,
    /// Distinct links in scope of any change.
    pub links_affected: usize,
    pub migration: MigrationSummary,
}

impl SchemaPlan {
    /// Can this plan be applied as is?
    pub fn applicable(&self) -> bool {
        self.valid && self.needs_migration.is_empty()
    }

    /// The refusal for a plan that cannot be applied.
    pub fn refusal(&self) -> Error {
        if !self.valid {
            Error::Invalid(format!("schema change rejected: {}", self.errors.join("; ")))
        } else {
            Error::Conflict(format!("breaking change needs a migration: {}", self.needs_migration.join("; ")))
        }
    }
}

/// One stored schema version.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaVersion {
    pub version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    pub ts: i64,
    pub at: String,
    pub actor: String,
    #[serde(default)]
    pub request_id: String,
    /// `config` (first open), `import` (schema files changed), `api`,
    /// `proposal`, `revert` or `undo`.
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<String>,
    #[serde(default)]
    pub changes: Vec<SchemaChange>,
    #[serde(default)]
    pub diff: SchemaDiff,
    #[serde(default)]
    pub breaking: bool,
    #[serde(default)]
    pub migration: MigrationSummary,
    /// The complete schema document of this version.
    pub schema: Schema,
}

/// A version without its document, for listings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaVersionInfo {
    pub version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    pub ts: i64,
    pub at: String,
    pub actor: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<String>,
    pub breaking: bool,
    pub changes: usize,
    pub diff: Vec<String>,
    pub migration: MigrationSummary,
}

impl From<&SchemaVersion> for SchemaVersionInfo {
    fn from(v: &SchemaVersion) -> Self {
        Self {
            version: v.version,
            parent: v.parent,
            ts: v.ts,
            at: v.at.clone(),
            actor: v.actor.clone(),
            source: v.source.clone(),
            message: v.message.clone(),
            proposal: v.proposal.clone(),
            breaking: v.breaking,
            changes: v.changes.len(),
            diff: v.diff.summary(),
            migration: v.migration.clone(),
        }
    }
}

/// Result of applying a change set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaApplied {
    pub version: SchemaVersionInfo,
    pub plan: SchemaPlan,
    pub revision: u64,
}

/// Status of the live schema.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaStatus {
    pub version: u64,
    pub mode: SchemaMode,
    pub name: String,
    pub object_types: usize,
    pub link_types: usize,
    pub interfaces: usize,
    pub action_types: usize,
    pub rules: usize,
    #[serde(default)]
    pub shapes: usize,
    pub metrics: usize,
    pub governance: Governance,
    /// Why changed schema files were not imported on open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_error: Option<String>,
}

// ── pure schema edits ────────────────────────────────────────────────────

impl SchemaChange {
    pub fn op(&self) -> &'static str {
        match self {
            SchemaChange::PutObjectType { .. } => "put_object_type",
            SchemaChange::DeleteObjectType { .. } => "delete_object_type",
            SchemaChange::RenameObjectType { .. } => "rename_object_type",
            SchemaChange::PutProperty { .. } => "put_property",
            SchemaChange::DeleteProperty { .. } => "delete_property",
            SchemaChange::RenameProperty { .. } => "rename_property",
            SchemaChange::PutLinkType { .. } => "put_link_type",
            SchemaChange::DeleteLinkType { .. } => "delete_link_type",
            SchemaChange::RenameLinkType { .. } => "rename_link_type",
            SchemaChange::PutInterface { .. } => "put_interface",
            SchemaChange::DeleteInterface { .. } => "delete_interface",
            SchemaChange::PutActionType { .. } => "put_action_type",
            SchemaChange::DeleteActionType { .. } => "delete_action_type",
            SchemaChange::PutRule { .. } => "put_rule",
            SchemaChange::DeleteRule { .. } => "delete_rule",
            SchemaChange::PutShape { .. } => "put_shape",
            SchemaChange::DeleteShape { .. } => "delete_shape",
            SchemaChange::PutMetric { .. } => "put_metric",
            SchemaChange::DeleteMetric { .. } => "delete_metric",
            SchemaChange::PutGlossaryTerm { .. } => "put_glossary_term",
            SchemaChange::DeleteGlossaryTerm { .. } => "delete_glossary_term",
            SchemaChange::SetAnalysis { .. } => "set_analysis",
        }
    }

    /// What the change is about: `issue`, `issue.priority`, `owns`, `rules.x`.
    pub fn target(&self) -> String {
        match self {
            SchemaChange::PutObjectType { name, .. }
            | SchemaChange::DeleteObjectType { name, .. }
            | SchemaChange::PutLinkType { name, .. }
            | SchemaChange::DeleteLinkType { name, .. }
            | SchemaChange::PutInterface { name, .. }
            | SchemaChange::DeleteInterface { name }
            | SchemaChange::PutActionType { name, .. }
            | SchemaChange::DeleteActionType { name }
            | SchemaChange::DeleteRule { name }
            | SchemaChange::PutShape { name, .. }
            | SchemaChange::DeleteShape { name }
            | SchemaChange::PutMetric { name, .. }
            | SchemaChange::DeleteMetric { name } => name.clone(),
            SchemaChange::RenameObjectType { from, to } | SchemaChange::RenameLinkType { from, to } => {
                format!("{from} → {to}")
            }
            SchemaChange::PutProperty { object_type, name, .. }
            | SchemaChange::DeleteProperty { object_type, name, .. } => format!("{object_type}.{name}"),
            SchemaChange::RenameProperty { object_type, from, to } => format!("{object_type}.{from} → {to}"),
            SchemaChange::PutRule { def } => def.name.clone(),
            SchemaChange::PutGlossaryTerm { section, word, .. }
            | SchemaChange::DeleteGlossaryTerm { section, word } => {
                format!("{}.{word}", section_name(*section))
            }
            SchemaChange::SetAnalysis { .. } => "analysis".into(),
        }
    }

    fn migrations(&self) -> &[Migration] {
        match self {
            SchemaChange::PutObjectType { migrations, .. }
            | SchemaChange::DeleteObjectType { migrations, .. }
            | SchemaChange::PutProperty { migrations, .. }
            | SchemaChange::DeleteProperty { migrations, .. }
            | SchemaChange::PutLinkType { migrations, .. }
            | SchemaChange::DeleteLinkType { migrations, .. } => migrations,
            _ => &[],
        }
    }
}

fn section_name(s: GlossarySection) -> &'static str {
    match s {
        GlossarySection::Kinds => "glossary.kinds",
        GlossarySection::Relations => "glossary.relations",
        GlossarySection::Dimensions => "glossary.dimensions",
    }
}

/// `[a-z][a-z0-9_]*`, at most 64 characters.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn check_new_name(what: &str, name: &str) -> std::result::Result<(), String> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(format!("{what} name '{name}' must match [a-z][a-z0-9_]* (at most 64 characters)"))
    }
}

/// Apply `changes` to a copy of `base` and compile the result.
pub fn apply_changes(base: &Schema, changes: &[SchemaChange]) -> std::result::Result<Schema, Vec<String>> {
    let mut s = base.clone();
    let mut errors = Vec::new();
    for (i, c) in changes.iter().enumerate() {
        if let Err(e) = apply_one(&mut s, c) {
            errors.push(format!("change {} ({} {}): {e}", i + 1, c.op(), c.target()));
        }
    }
    if errors.is_empty()
        && let Err(e) = s.compile()
    {
        errors.push(match e {
            Error::Config(m) => m,
            other => other.to_string(),
        });
    }
    // Two object types syncing the same table would read every row twice.
    // Only a clash the changes create is refused, so an older schema that
    // has one can still change elsewhere.
    let before = synced_tables(base);
    for ((source, table), types) in synced_tables(&s) {
        if types.len() > 1 && before.get(&(source.clone(), table.clone())) != Some(&types) {
            errors.push(format!(
                "object types {} all sync {source} table {table}: one object type per table (rename the existing type instead)",
                types.join(", ")
            ));
        }
    }
    if errors.is_empty() { Ok(s) } else { Err(errors) }
}

/// Object types that sync rows, by the `(source, table)` they read.
fn synced_tables(s: &Schema) -> BTreeMap<(String, String), Vec<String>> {
    let mut out: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for ty in s.object_types.values() {
        if let Some(b) = ty.backing.as_ref().filter(|b| b.sync) {
            out.entry((b.source.clone(), b.table.clone())).or_default().push(ty.name.clone());
        }
    }
    out
}

fn apply_one(s: &mut Schema, c: &SchemaChange) -> std::result::Result<(), String> {
    match c {
        SchemaChange::PutObjectType { name, def, .. } => {
            if !s.object_types.contains_key(name) {
                check_new_name("object type", name)?;
                if s.interfaces.contains_key(name) {
                    return Err(format!("{name} is an interface"));
                }
            }
            let mut d = def.clone();
            d.name = name.clone();
            s.object_types.insert(name.clone(), d);
        }
        SchemaChange::DeleteObjectType { name, .. } => {
            s.object_types.remove(name).ok_or_else(|| format!("unknown object type {name}"))?;
        }
        SchemaChange::RenameObjectType { from, to } => {
            check_new_name("object type", to)?;
            if s.is_type_or_interface(to) {
                return Err(format!("{to} already exists"));
            }
            let mut def = s.object_types.remove(from).ok_or_else(|| format!("unknown object type {from}"))?;
            def.name = to.clone();
            s.object_types.insert(to.clone(), def);
            rewrite_kind(s, from, to);
        }
        SchemaChange::PutProperty { object_type, name, def, .. } => {
            let ty = s.object_types.get_mut(object_type).ok_or_else(|| format!("unknown object type {object_type}"))?;
            if !ty.properties.contains_key(name) {
                check_new_name("property", name)?;
            }
            ty.properties.insert(name.clone(), def.clone());
        }
        SchemaChange::DeleteProperty { object_type, name, .. } => {
            let ty = s.object_types.get_mut(object_type).ok_or_else(|| format!("unknown object type {object_type}"))?;
            if ty.properties.remove(name).is_none() {
                return Err(format!("{object_type} does not declare property {name}"));
            }
            if let Some(b) = &mut ty.backing {
                b.properties.remove(name);
            }
        }
        SchemaChange::RenameProperty { object_type, from, to } => {
            check_new_name("property", to)?;
            let ty = s.object_types.get_mut(object_type).ok_or_else(|| format!("unknown object type {object_type}"))?;
            if ty.properties.contains_key(to) {
                return Err(format!("{object_type} already declares {to}"));
            }
            let def = ty.properties.remove(from).ok_or_else(|| format!("{object_type} does not declare {from}"))?;
            ty.properties.insert(to.clone(), def);
            if let Some(b) = &mut ty.backing
                && let Some(column) = b.properties.remove(from)
            {
                b.properties.insert(to.clone(), column);
            }
            for u in ty.constraints.unique.iter_mut().filter(|u| *u == from) {
                *u = to.clone();
            }
            let renamed = |v: &mut Option<String>| {
                if v.as_deref() == Some(from.as_str()) {
                    *v = Some(to.clone());
                }
            };
            for d in ty.derived.values_mut() {
                renamed(&mut d.sum);
                renamed(&mut d.avg);
                renamed(&mut d.min);
                renamed(&mut d.max);
            }
            let kinds: Vec<String> = s.object_types.keys().filter(|k| s.kind_is(k, object_type)).cloned().collect();
            for m in s.metrics.values_mut() {
                if m.kind.as_ref().is_some_and(|k| kinds.contains(k)) {
                    renamed(&mut m.property);
                }
            }
            let (old_path, new_path) = (format!("prop.{from}"), format!("prop.{to}"));
            for sh in s.shapes.values_mut().filter(|sh| sh.target.as_ref().is_some_and(|t| kinds.contains(t))) {
                for p in sh.properties.iter_mut().filter(|p| p.path == old_path) {
                    p.path = new_path.clone();
                }
                for i in sh.ignored_properties.iter_mut().filter(|i| *i == from) {
                    *i = to.clone();
                }
            }
        }
        SchemaChange::PutLinkType { name, def, .. } => {
            if !s.link_types.contains_key(name) {
                check_new_name("link type", name)?;
            }
            let mut d = def.clone();
            d.name = name.clone();
            s.link_types.insert(name.clone(), d);
        }
        SchemaChange::DeleteLinkType { name, .. } => {
            s.link_types.remove(name).ok_or_else(|| format!("unknown link type {name}"))?;
            for b in s.object_types.values_mut().filter_map(|t| t.backing.as_mut()) {
                b.links.remove(name);
            }
        }
        SchemaChange::RenameLinkType { from, to } => {
            check_new_name("link type", to)?;
            if s.resolve_rel(to).is_some() {
                return Err(format!("relation {to} already exists"));
            }
            let mut def = s.link_types.remove(from).ok_or_else(|| format!("unknown link type {from}"))?;
            def.name = to.clone();
            s.link_types.insert(to.clone(), def);
            rewrite_rel(s, from, to);
        }
        SchemaChange::PutInterface { name, def } => {
            if !s.interfaces.contains_key(name) {
                check_new_name("interface", name)?;
            }
            let mut d = def.clone();
            d.name = name.clone();
            s.interfaces.insert(name.clone(), d);
        }
        SchemaChange::DeleteInterface { name } => {
            s.interfaces.remove(name).ok_or_else(|| format!("unknown interface {name}"))?;
        }
        SchemaChange::PutActionType { name, def } => {
            if !s.action_types.contains_key(name) {
                check_new_name("action type", name)?;
            }
            let mut d = def.clone();
            d.name = name.clone();
            s.action_types.insert(name.clone(), d);
        }
        SchemaChange::DeleteActionType { name } => {
            s.action_types.remove(name).ok_or_else(|| format!("unknown action type {name}"))?;
        }
        SchemaChange::PutRule { def } => {
            if def.name.is_empty() {
                return Err("rule needs a name".into());
            }
            match s.rules.iter_mut().find(|r| r.name == def.name) {
                Some(r) => *r = def.clone(),
                None => {
                    check_new_name("rule", &def.name)?;
                    s.rules.push(def.clone());
                }
            }
        }
        SchemaChange::DeleteRule { name } => {
            let before = s.rules.len();
            s.rules.retain(|r| &r.name != name);
            if s.rules.len() == before {
                return Err(format!("unknown rule {name}"));
            }
        }
        SchemaChange::PutShape { name, def } => {
            if !s.shapes.contains_key(name) {
                check_new_name("shape", name)?;
            }
            s.shapes.insert(name.clone(), def.clone());
        }
        SchemaChange::DeleteShape { name } => {
            s.shapes.remove(name).ok_or_else(|| format!("unknown shape {name}"))?;
        }
        SchemaChange::PutMetric { name, def } => {
            if !s.metrics.contains_key(name) {
                check_new_name("metric", name)?;
            }
            let mut d = def.clone();
            d.name = name.clone();
            s.metrics.insert(name.clone(), d);
        }
        SchemaChange::DeleteMetric { name } => {
            s.metrics.remove(name).ok_or_else(|| format!("unknown metric {name}"))?;
        }
        SchemaChange::PutGlossaryTerm { section, word, target } => {
            let word = word.trim().to_lowercase();
            if word.is_empty() || target.is_empty() {
                return Err("glossary terms need a word and a target".into());
            }
            glossary_map(s, *section).insert(word, target.clone());
        }
        SchemaChange::DeleteGlossaryTerm { section, word } => {
            let word = word.trim().to_lowercase();
            if glossary_map(s, *section).remove(&word).is_none() {
                return Err(format!("{} has no term '{word}'", section_name(*section)));
            }
        }
        SchemaChange::SetAnalysis { def } => s.analysis = def.clone(),
    }
    Ok(())
}

fn glossary_map(s: &mut Schema, section: GlossarySection) -> &mut BTreeMap<String, String> {
    match section {
        GlossarySection::Kinds => &mut s.glossary.kinds,
        GlossarySection::Relations => &mut s.glossary.relations,
        GlossarySection::Dimensions => &mut s.glossary.dimensions,
    }
}

/// Point every reference to kind `from` at `to`.
fn rewrite_kind(s: &mut Schema, from: &str, to: &str) {
    let swap = |v: &mut String| {
        if v == from {
            *v = to.to_string();
        }
    };
    for t in s.object_types.values_mut() {
        if let Some(e) = &mut t.extends {
            swap(e);
        }
        for d in t.derived.values_mut() {
            if let Some(k) = &mut d.kind {
                swap(k);
            }
        }
    }
    for lt in s.link_types.values_mut() {
        lt.from.iter_mut().for_each(swap);
        lt.to.iter_mut().for_each(swap);
    }
    s.glossary.kinds.values_mut().for_each(swap);
    for a in s.action_types.values_mut() {
        for p in a.parameters.values_mut() {
            p.kinds.iter_mut().for_each(swap);
        }
        for e in &mut a.effects {
            if let Some(c) = &mut e.create {
                swap(&mut c.kind);
            }
        }
    }
    for m in s.metrics.values_mut() {
        if let Some(k) = &mut m.kind {
            swap(k);
        }
    }
    // Horn rules name kinds as the object of `a`.
    let swap_value = |v: &mut Value| {
        if v.as_str() == Some(from) {
            *v = Value::String(to.to_string());
        }
    };
    for r in &mut s.rules {
        for atom in r.when.iter_mut().chain(std::iter::once(&mut r.then)) {
            if atom.len() == 3 && atom[1].as_str() == Some("a") {
                swap_value(&mut atom[2]);
            }
        }
    }
    for sh in s.shapes.values_mut() {
        if let Some(t) = &mut sh.target {
            swap(t);
        }
        for p in &mut sh.properties {
            if let Some(c) = &mut p.class {
                swap(c);
            }
        }
    }
}

/// Point every reference to relation `from` at `to` (also `^from` and the
/// segments of dimension paths such as `assigned_to.member_of`).
fn rewrite_rel(s: &mut Schema, from: &str, to: &str) {
    let step = |v: &mut String| {
        if v == from {
            *v = to.to_string();
        } else if v.strip_prefix('^') == Some(from) {
            *v = format!("^{to}");
        }
    };
    let path = |v: &mut String| {
        if v.contains('.') || v == from || v.strip_prefix('^') == Some(from) {
            let parts: Vec<String> = v
                .split('.')
                .map(|p| {
                    let mut p = p.to_string();
                    step(&mut p);
                    p
                })
                .collect();
            *v = parts.join(".");
        }
    };
    for r in &mut s.rules {
        r.chain.iter_mut().for_each(step);
        for atom in r.when.iter_mut().chain(std::iter::once(&mut r.then)) {
            if let Some(Value::String(pred)) = atom.get_mut(1) {
                step(pred);
            }
        }
    }
    for sh in s.shapes.values_mut() {
        sh.target_subjects_of.iter_mut().chain(sh.target_objects_of.iter_mut()).for_each(step);
        for p in &mut sh.properties {
            if matches!(ShapePath::parse(&p.path), ShapePath::Relations(_)) {
                path(&mut p.path);
            }
        }
    }
    for i in s.interfaces.values_mut() {
        i.links.iter_mut().for_each(step);
    }
    s.glossary.relations.values_mut().for_each(step);
    s.glossary.dimensions.values_mut().for_each(path);
    for t in s.object_types.values_mut() {
        t.constraints.required_links.iter_mut().for_each(step);
        for d in t.derived.values_mut() {
            if let Some(c) = &mut d.count {
                step(c);
            }
            if let Some(o) = &mut d.over {
                step(o);
            }
        }
    }
    for m in s.metrics.values_mut() {
        m.dimensions.values_mut().for_each(path);
    }
    let fix_check = |c: &mut Check| {
        if let Some(r) = &mut c.rel {
            step(r);
        }
    };
    for a in s.action_types.values_mut() {
        a.preconditions.iter_mut().for_each(fix_check);
        for e in &mut a.effects {
            if let Some(l) = &mut e.link {
                step(&mut l.rel);
            }
            if let Some(l) = &mut e.unlink {
                step(&mut l.rel);
            }
            if let Some(w) = &mut e.when {
                fix_check(w);
            }
            if let Some(c) = &mut e.create {
                for l in &mut c.links {
                    step(&mut l.rel);
                }
            }
        }
    }
    let a = &mut s.analysis;
    for list in [&mut a.root_cause, &mut a.impact, &mut a.scope, &mut a.containment] {
        list.iter_mut().for_each(step);
    }
    for b in s.object_types.values_mut().filter_map(|t| t.backing.as_mut()) {
        if let Some(column) = b.links.remove(from) {
            b.links.insert(to.to_string(), column);
        }
    }
}

// ── diffs ────────────────────────────────────────────────────────────────

fn entries<T: Serialize>(m: &BTreeMap<String, T>) -> BTreeMap<String, Value> {
    m.iter().map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or(Value::Null))).collect()
}

fn sections(s: &Schema) -> Vec<(&'static str, BTreeMap<String, Value>)> {
    let strings = |m: &BTreeMap<String, String>| -> BTreeMap<String, Value> {
        m.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()
    };
    vec![
        ("object_types", entries(&s.object_types)),
        ("link_types", entries(&s.link_types)),
        ("interfaces", entries(&s.interfaces)),
        ("action_types", entries(&s.action_types)),
        ("metrics", entries(&s.metrics)),
        ("rules", s.rules.iter().map(|r| (r.name.clone(), serde_json::to_value(r).unwrap_or(Value::Null))).collect()),
        ("shapes", entries(&s.shapes)),
        ("glossary.kinds", strings(&s.glossary.kinds)),
        ("glossary.relations", strings(&s.glossary.relations)),
        ("glossary.dimensions", strings(&s.glossary.dimensions)),
        ("analysis", BTreeMap::from([("analysis".to_string(), serde_json::to_value(&s.analysis).unwrap_or_default())])),
    ]
}

/// Fields that differ between two definitions; one level deeper for maps of
/// named members (`properties.priority`, `parameters.item`).
fn changed_fields(a: &Value, b: &Value) -> Vec<String> {
    let (Some(a), Some(b)) = (a.as_object(), b.as_object()) else { return Vec::new() };
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    let mut out = Vec::new();
    for k in keys {
        let (x, y) = (a.get(k), b.get(k));
        if x == y {
            continue;
        }
        let nested = matches!(k.as_str(), "properties" | "parameters" | "derived");
        match (x.and_then(Value::as_object), y.and_then(Value::as_object)) {
            (Some(xm), Some(ym)) if nested => {
                let sub: BTreeSet<&String> = xm.keys().chain(ym.keys()).collect();
                for s in sub {
                    if xm.get(s) != ym.get(s) {
                        out.push(format!("{k}.{s}"));
                    }
                }
            }
            _ if nested => {
                let empty = Map::new();
                let xm = x.and_then(Value::as_object).unwrap_or(&empty);
                let ym = y.and_then(Value::as_object).unwrap_or(&empty);
                let sub: BTreeSet<&String> = xm.keys().chain(ym.keys()).collect();
                out.extend(sub.into_iter().map(|s| format!("{k}.{s}")));
            }
            _ => out.push(k.clone()),
        }
    }
    out
}

/// Every definition that differs between `a` and `b`.
pub fn diff(a: &Schema, b: &Schema) -> SchemaDiff {
    let mut out = Vec::new();
    for ((section, am), (_, bm)) in sections(a).into_iter().zip(sections(b)) {
        let names: BTreeSet<&String> = am.keys().chain(bm.keys()).collect();
        for name in names {
            let (x, y) = (am.get(name), bm.get(name));
            let entry = match (x, y) {
                (None, Some(after)) => DiffEntry {
                    section: section.into(),
                    name: name.clone(),
                    change: DiffKind::Added,
                    fields: Vec::new(),
                    before: None,
                    after: Some(after.clone()),
                },
                (Some(before), None) => DiffEntry {
                    section: section.into(),
                    name: name.clone(),
                    change: DiffKind::Removed,
                    fields: Vec::new(),
                    before: Some(before.clone()),
                    after: None,
                },
                (Some(before), Some(after)) if before != after => DiffEntry {
                    section: section.into(),
                    name: name.clone(),
                    change: DiffKind::Modified,
                    fields: changed_fields(before, after),
                    before: Some(before.clone()),
                    after: Some(after.clone()),
                },
                _ => continue,
            };
            out.push(entry);
        }
    }
    SchemaDiff { entries: out }
}

/// The changes that turn `a` into `b` (puts for added and modified
/// definitions, deletes for removed ones; no migrations).
pub fn changes_between(a: &Schema, b: &Schema) -> Vec<SchemaChange> {
    let mut out = Vec::new();
    for e in diff(a, b).entries {
        let removed = e.change == DiffKind::Removed;
        let name = e.name.clone();
        let c = match e.section.as_str() {
            "object_types" if removed => SchemaChange::DeleteObjectType { name, migrations: Vec::new() },
            "object_types" => {
                SchemaChange::PutObjectType { def: b.object_types[&name].clone(), name, migrations: Vec::new() }
            }
            "link_types" if removed => SchemaChange::DeleteLinkType { name, migrations: Vec::new() },
            "link_types" => {
                SchemaChange::PutLinkType { def: b.link_types[&name].clone(), name, migrations: Vec::new() }
            }
            "interfaces" if removed => SchemaChange::DeleteInterface { name },
            "interfaces" => SchemaChange::PutInterface { def: b.interfaces[&name].clone(), name },
            "action_types" if removed => SchemaChange::DeleteActionType { name },
            "action_types" => SchemaChange::PutActionType { def: b.action_types[&name].clone(), name },
            "metrics" if removed => SchemaChange::DeleteMetric { name },
            "metrics" => SchemaChange::PutMetric { def: b.metrics[&name].clone(), name },
            "shapes" if removed => SchemaChange::DeleteShape { name },
            "shapes" => SchemaChange::PutShape { def: b.shapes[&name].clone(), name },
            "rules" if removed => SchemaChange::DeleteRule { name },
            "rules" => match b.rules.iter().find(|r| r.name == name) {
                Some(r) => SchemaChange::PutRule { def: r.clone() },
                None => continue,
            },
            "analysis" => SchemaChange::SetAnalysis { def: b.analysis.clone() },
            g => {
                let section = match g {
                    "glossary.kinds" => GlossarySection::Kinds,
                    "glossary.relations" => GlossarySection::Relations,
                    _ => GlossarySection::Dimensions,
                };
                if removed {
                    SchemaChange::DeleteGlossaryTerm { section, word: name }
                } else {
                    let target = e.after.as_ref().and_then(Value::as_str).unwrap_or_default().to_string();
                    SchemaChange::PutGlossaryTerm { section, word: name, target }
                }
            }
        };
        out.push(c);
    }
    out
}

/// Stable fingerprint of a schema document (FNV-1a over its JSON).
pub fn fingerprint(s: &Schema) -> String {
    let text = serde_json::to_string(s).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

// ── impact analysis and migration planning ───────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Issue {
    /// The entity's kind is no longer defined.
    UnknownKind,
    /// The entity's state is not allowed.
    BadState(String),
    /// A value no longer fits its property.
    BadValue(String),
    /// A value of a property that is no longer defined.
    RemovedProperty(String),
    /// A newly required property is missing (warning).
    MissingRequired(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum LinkIssue {
    RemovedRel,
    BadEndpoints,
    Cardinality,
}

/// Normalized migration with resolved selectors and the change it came from.
#[derive(Clone, Debug)]
struct Mig {
    change: Option<usize>,
    m: Migration,
}

#[derive(Default)]
struct Scope {
    kinds: BTreeSet<String>,
    property: Option<String>,
    rels: BTreeSet<String>,
}

#[derive(Default)]
struct Fix {
    delete: bool,
    retype: Option<String>,
    state: Option<String>,
    set: Map<String, Value>,
}

/// Everything the planner works out for one change set.
pub(crate) struct Planned {
    pub plan: SchemaPlan,
    pub schema: Option<Schema>,
    pub mutations: Vec<Mutation>,
    pub redirects: Vec<(Id, Id)>,
    pub reindex: Vec<String>,
}

fn descendants(base: &Schema, new: &Schema, g: &Graph, name: &str) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    out.insert(name.to_string());
    for k in base.object_types.keys().chain(new.object_types.keys()) {
        if base.kind_is(k, name) || new.kind_is(k, name) {
            out.insert(k.clone());
        }
    }
    for (k, _) in g.kinds() {
        if base.kind_is(k, name) || new.kind_is(k, name) {
            out.insert(k.clone());
        }
    }
    out
}

fn scope_of(base: &Schema, new: &Schema, g: &Graph, c: &SchemaChange) -> Scope {
    let mut s = Scope::default();
    match c {
        SchemaChange::PutObjectType { name, .. } | SchemaChange::DeleteObjectType { name, .. } => {
            s.kinds = descendants(base, new, g, name);
        }
        SchemaChange::RenameObjectType { from, to } => {
            s.kinds = descendants(base, new, g, from);
            s.kinds.extend(descendants(base, new, g, to));
        }
        SchemaChange::PutProperty { object_type, name, .. }
        | SchemaChange::DeleteProperty { object_type, name, .. } => {
            s.kinds = descendants(base, new, g, object_type);
            s.property = Some(name.clone());
        }
        SchemaChange::RenameProperty { object_type, from, .. } => {
            s.kinds = descendants(base, new, g, object_type);
            s.property = Some(from.clone());
        }
        SchemaChange::PutLinkType { name, .. } | SchemaChange::DeleteLinkType { name, .. } => {
            s.rels.insert(name.clone());
        }
        SchemaChange::RenameLinkType { from, .. } => {
            s.rels.insert(from.clone());
        }
        SchemaChange::PutInterface { name, .. } | SchemaChange::DeleteInterface { name } => {
            s.kinds = descendants(base, new, g, name);
            s.kinds.remove(name);
        }
        _ => {}
    }
    s
}

fn normalize(set: &SchemaChangeSet) -> Vec<Mig> {
    let mut out = Vec::new();
    for (i, c) in set.changes.iter().enumerate() {
        let (kind, property, rel): (Option<String>, Option<String>, Option<String>) = match c {
            SchemaChange::PutObjectType { name, .. } | SchemaChange::DeleteObjectType { name, .. } => {
                (Some(name.clone()), None, None)
            }
            SchemaChange::PutProperty { object_type, name, .. }
            | SchemaChange::DeleteProperty { object_type, name, .. } => {
                (Some(object_type.clone()), Some(name.clone()), None)
            }
            SchemaChange::PutLinkType { name, .. } | SchemaChange::DeleteLinkType { name, .. } => {
                (None, None, Some(name.clone()))
            }
            _ => (None, None, None),
        };
        for m in c.migrations() {
            let mut m = m.clone();
            match &mut m {
                Migration::DeleteEntities { kind: k }
                | Migration::Retype { kind: k, .. }
                | Migration::MapStates { kind: k, .. } => {
                    if k.is_none() {
                        *k = kind.clone();
                    }
                }
                Migration::SetDefault { kind: k, property: p, .. }
                | Migration::Convert { kind: k, property: p }
                | Migration::DropValues { kind: k, property: p } => {
                    if k.is_none() {
                        *k = kind.clone();
                    }
                    if p.is_none() {
                        *p = property.clone();
                    }
                }
                Migration::DeleteLinks { rel: r }
                | Migration::MoveLinks { rel: r, .. }
                | Migration::KeepLatest { rel: r } => {
                    if r.is_none() {
                        *r = rel.clone();
                    }
                }
                Migration::Acknowledge { .. } => {}
            }
            out.push(Mig { change: Some(i), m });
        }
    }
    out.extend(set.migrations.iter().cloned().map(|m| Mig { change: None, m }));
    out
}

fn kind_matches(base: &Schema, new: &Schema, kind: &str, selector: Option<&str>) -> bool {
    match selector {
        None => true,
        Some(a) => kind == a || base.kind_is(kind, a) || new.kind_is(kind, a),
    }
}

/// Best-effort conversion of `v` to fit `def`.
fn convert(def: &PropertyDef, v: &Value) -> Option<Value> {
    let s = match v {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    let out = match def.ty {
        PropType::String | PropType::Text => match v {
            Value::Array(a) => Some(Value::String(a.iter().map(value_text).collect::<Vec<_>>().join(", "))),
            Value::Object(_) => Some(Value::String(v.to_string())),
            _ => s.map(Value::String),
        },
        PropType::Int => s
            .as_deref()
            .and_then(|s| {
                s.parse::<i64>().ok().or_else(|| s.parse::<f64>().ok().filter(|f| f.fract() == 0.0).map(|f| f as i64))
            })
            .map(Value::from),
        PropType::Float => s.as_deref().and_then(|s| s.parse::<f64>().ok()).map(Value::from),
        PropType::Bool => match s.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("true" | "yes" | "1" | "on") => Some(Value::Bool(true)),
            Some("false" | "no" | "0" | "off") => Some(Value::Bool(false)),
            _ => None,
        },
        PropType::Enum => {
            s.and_then(|s| def.values.iter().find(|x| x.eq_ignore_ascii_case(&s)).map(|x| Value::String(x.clone())))
        }
        PropType::Timestamp => match v {
            Value::Number(n) => n.as_i64().map(Value::from),
            _ => s.map(Value::String),
        },
        PropType::List => match v {
            Value::Array(_) => Some(v.clone()),
            Value::Null => None,
            other => Some(Value::Array(vec![other.clone()])),
        },
        PropType::Json => Some(v.clone()),
    };
    out.filter(|x| def.accepts(x))
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn link_key(l: &crate::model::Link) -> LinkKey {
    (l.from.clone(), l.rel.clone(), l.to.clone())
}

/// Work out the impact and the migration of `set` against `base` and `g`.
pub(crate) fn plan(base: &Schema, base_version: u64, g: &Graph, set: &SchemaChangeSet, strict: bool) -> Planned {
    let mut plan = SchemaPlan { base_version, ..Default::default() };
    if set.changes.is_empty() {
        plan.errors.push("the change set is empty".into());
        return Planned { plan, schema: None, mutations: Vec::new(), redirects: Vec::new(), reindex: Vec::new() };
    }
    if let Some(expected) = set.expected_version
        && expected != base_version
    {
        plan.errors.push(format!("expected schema version {expected} but the active version is {base_version}"));
    }
    let new = match apply_changes(base, &set.changes) {
        Ok(s) => s,
        Err(errors) => {
            plan.errors.extend(errors);
            return Planned { plan, schema: None, mutations: Vec::new(), redirects: Vec::new(), reindex: Vec::new() };
        }
    };
    plan.diff = diff(base, &new);
    let migs = normalize(set);
    let scopes: Vec<Scope> = set.changes.iter().map(|c| scope_of(base, &new, g, c)).collect();

    // Validate selectors of migrations against the new schema.
    for mg in &migs {
        match &mg.m {
            Migration::Retype { to, .. } if !new.object_types.contains_key(to) => {
                plan.errors.push(format!("retype target {to} is not an object type of the new schema"));
            }
            Migration::MoveLinks { to, .. } if !new.link_types.contains_key(to) => {
                plan.errors.push(format!("move_links target {to} is not a link type of the new schema"));
            }
            Migration::MapStates { kind: Some(k), map, default } => {
                if let Some(ty) = new.object_types.get(k)
                    && !ty.states.is_empty()
                {
                    for s in map.values().chain(default.iter()) {
                        if !ty.states.contains(s) {
                            plan.errors.push(format!("map_states: {s} is not a state of {k}"));
                        }
                    }
                }
            }
            Migration::SetDefault { kind: Some(k), property: Some(p), value } => {
                if let Some(def) = new.prop_def(k, p)
                    && !def.accepts(value)
                {
                    plan.errors.push(format!("set_default: {value} does not fit {k}.{p} ({})", def.expected()));
                }
            }
            Migration::SetDefault { property: None, .. }
            | Migration::Convert { property: None, .. }
            | Migration::DropValues { property: None, .. }
                if mg.change.is_none() =>
            {
                plan.errors.push("set-level value migrations need a property".into());
            }
            _ => {}
        }
    }

    // Entities and links in scope of each change.
    let mut impacts: Vec<ChangeImpact> = Vec::new();
    let mut scoped_entities: HashSet<&str> = HashSet::new();
    let mut scoped_links: HashSet<LinkKey> = HashSet::new();
    let mut links_by_rel: HashMap<&str, Vec<&crate::model::Link>> = HashMap::new();
    let wanted_rels: BTreeSet<&String> = scopes.iter().flat_map(|s| s.rels.iter()).collect();
    if !wanted_rels.is_empty() {
        for l in g.links() {
            if wanted_rels.contains(&l.rel) {
                links_by_rel.entry(l.rel.as_str()).or_default().push(l);
            }
        }
    }
    for (i, (c, sc)) in set.changes.iter().zip(&scopes).enumerate() {
        let mut imp = ChangeImpact {
            index: i,
            op: c.op().into(),
            target: c.target(),
            summary: summarize(base, c),
            ..Default::default()
        };
        for k in &sc.kinds {
            for e in g.of_kind(k) {
                if sc.property.as_ref().is_some_and(|p| !e.props.contains_key(p)) {
                    continue;
                }
                imp.entities += 1;
                if imp.sample.len() < 10 {
                    imp.sample.push(e.id.clone());
                }
                scoped_entities.insert(e.id.as_str());
                if matches!(c, SchemaChange::DeleteObjectType { .. } | SchemaChange::RenameObjectType { .. }) {
                    for key in g.incident_keys(&e.id) {
                        imp.links += 1;
                        scoped_links.insert(key);
                    }
                }
            }
        }
        for r in &sc.rels {
            for l in links_by_rel.get(r.as_str()).into_iter().flatten() {
                imp.links += 1;
                if imp.sample.len() < 10 {
                    imp.sample.push(format!("{} -{}-> {}", l.from, l.rel, l.to));
                }
                scoped_links.insert(link_key(l));
            }
        }
        impacts.push(imp);
    }

    // Find what breaks.
    let all_kinds: BTreeSet<&String> = scopes.iter().flat_map(|s| s.kinds.iter()).collect();
    let mut per_kind: BTreeMap<(String, Issue), Vec<&Entity>> = BTreeMap::new();
    for k in &all_kinds {
        for e in g.of_kind(k) {
            for issue in entity_issues(base, &new, e) {
                per_kind.entry((e.kind.clone(), issue)).or_default().push(e);
            }
        }
    }
    let mut link_issues: BTreeMap<(String, LinkIssue), Vec<&crate::model::Link>> = BTreeMap::new();
    for (rel, links) in &links_by_rel {
        let old_lt = base.link_type(rel);
        let new_lt = new.link_type(rel);
        let mut per_source: HashMap<&str, Vec<&crate::model::Link>> = HashMap::new();
        for l in links {
            let issue = match new_lt {
                None if old_lt.is_some() => Some(LinkIssue::RemovedRel),
                None => None,
                Some(_) => {
                    let (Some(f), Some(t)) = (g.get(&l.from), g.get(&l.to)) else { continue };
                    if new.validate_link(&f.kind, rel, &t.kind, strict).is_err() {
                        Some(LinkIssue::BadEndpoints)
                    } else {
                        per_source.entry(l.from.as_str()).or_default().push(l);
                        None
                    }
                }
            };
            if let Some(issue) = issue {
                link_issues.entry((rel.to_string(), issue)).or_default().push(l);
            }
        }
        let becomes_one = new_lt.is_some_and(|lt| lt.cardinality == Cardinality::One)
            && old_lt.is_none_or(|lt| lt.cardinality != Cardinality::One);
        if becomes_one {
            for (_, mut ls) in per_source {
                if ls.len() > 1 {
                    ls.sort_by_key(|l| std::cmp::Reverse(l.created_at));
                    link_issues
                        .entry((rel.to_string(), LinkIssue::Cardinality))
                        .or_default()
                        .extend(ls.into_iter().skip(1));
                }
            }
        }
    }

    let attribute = |kind: Option<&str>, property: Option<&str>, rel: Option<&str>| -> Option<usize> {
        scopes.iter().position(|s| {
            (kind.is_some_and(|k| s.kinds.contains(k))
                && (s.property.is_none() || property.is_none() || s.property.as_deref() == property))
                || rel.is_some_and(|r| s.rels.contains(r))
        })
    };
    let acknowledged = |change: Option<usize>| -> bool {
        migs.iter()
            .any(|mg| matches!(mg.m, Migration::Acknowledge { .. }) && (mg.change.is_none() || mg.change == change))
    };

    // Renames migrate their own data; the old names are not orphaned.
    let renamed_kinds: HashSet<&str> = set
        .changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::RenameObjectType { from, .. } => Some(from.as_str()),
            _ => None,
        })
        .collect();
    let renamed_rels: HashSet<&str> = set
        .changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::RenameLinkType { from, .. } => Some(from.as_str()),
            _ => None,
        })
        .collect();
    let renamed_props: Vec<(&str, &str)> = set
        .changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::RenameProperty { object_type, from, .. } => Some((object_type.as_str(), from.as_str())),
            _ => None,
        })
        .collect();
    let prop_renamed =
        |kind: &str, prop: &str| renamed_props.iter().any(|(t, f)| *f == prop && (kind == *t || base.kind_is(kind, t)));

    let mut fixes: BTreeMap<Id, Fix> = BTreeMap::new();
    let mut link_del: BTreeSet<LinkKey> = BTreeSet::new();
    let mut link_move: BTreeMap<LinkKey, String> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut needs: Vec<String> = Vec::new();
    let mut breaking = false;
    let add_reason = |impacts: &mut Vec<ChangeImpact>, at: Option<usize>, reason: String| {
        if let Some(imp) = at.and_then(|i| impacts.get_mut(i)) {
            imp.breaking = true;
            if !imp.reasons.contains(&reason) {
                imp.reasons.push(reason);
            }
        }
    };

    // Entity issues, grouped by (kind, issue).
    for ((kind, issue), ents) in &per_kind {
        let n = ents.len();
        let renamed = match issue {
            Issue::UnknownKind => renamed_kinds.contains(kind.as_str()),
            Issue::RemovedProperty(p) => prop_renamed(kind, p),
            _ => false,
        };
        if renamed {
            continue;
        }
        match issue {
            Issue::UnknownKind => {
                let at = attribute(Some(kind), None, None);
                breaking = true;
                let reason = format!("{n} {kind} entities have a kind the new schema no longer defines");
                add_reason(&mut impacts, at, reason.clone());
                let resolver = migs.iter().find(|mg| match &mg.m {
                    Migration::DeleteEntities { kind: k } | Migration::Retype { kind: k, .. } => {
                        k.as_deref() == Some(kind.as_str())
                    }
                    _ => false,
                });
                match resolver.map(|mg| &mg.m) {
                    Some(Migration::DeleteEntities { .. }) => {
                        for e in ents {
                            fixes.entry(e.id.clone()).or_default().delete = true;
                        }
                    }
                    Some(Migration::Retype { to, .. }) => {
                        for e in ents {
                            fixes.entry(e.id.clone()).or_default().retype = Some(to.clone());
                        }
                    }
                    _ if !strict && acknowledged(at) => {
                        warnings.push(format!("{n} {kind} entities stay as untyped data (acknowledged)"));
                    }
                    _ => needs.push(format!("{reason}: add delete_entities or retype")),
                }
            }
            Issue::BadState(state) => {
                let at = attribute(Some(kind), None, None);
                breaking = true;
                let reason = format!("{n} {kind} entities are in state '{state}', which the new schema does not allow");
                add_reason(&mut impacts, at, reason.clone());
                let target = migs.iter().find_map(|mg| match &mg.m {
                    Migration::MapStates { kind: k, map, default } if kind_matches(base, &new, kind, k.as_deref()) => {
                        map.get(state).or(default.as_ref()).cloned()
                    }
                    _ => None,
                });
                let removed = migs.iter().any(|mg| match &mg.m {
                    Migration::DeleteEntities { kind: k } | Migration::Retype { kind: k, .. } => {
                        k.as_deref() == Some(kind.as_str())
                    }
                    _ => false,
                });
                match target {
                    Some(t) => {
                        for e in ents {
                            fixes.entry(e.id.clone()).or_default().state = Some(t.clone());
                        }
                    }
                    None if removed => {}
                    None => needs.push(format!("{reason}: add map_states")),
                }
            }
            Issue::BadValue(prop) | Issue::RemovedProperty(prop) => {
                let removed_prop = matches!(issue, Issue::RemovedProperty(_));
                let at = attribute(Some(kind), Some(prop), None);
                breaking = true;
                let reason = if removed_prop {
                    format!("{n} {kind} entities hold values of {prop}, which the new schema no longer defines")
                } else {
                    let expected = new.prop_def(kind, prop).map(PropertyDef::expected).unwrap_or_default();
                    format!("{n} {kind} entities hold values of {prop} that are not {expected}")
                };
                add_reason(&mut impacts, at, reason.clone());
                let resolver = migs.iter().find(|mg| match &mg.m {
                    Migration::SetDefault { kind: k, property: p, .. }
                    | Migration::Convert { kind: k, property: p }
                    | Migration::DropValues { kind: k, property: p } => {
                        p.as_deref() == Some(prop.as_str()) && kind_matches(base, &new, kind, k.as_deref())
                    }
                    _ => false,
                });
                let gone = migs.iter().any(|mg| match &mg.m {
                    Migration::DeleteEntities { kind: k } | Migration::Retype { kind: k, .. } => {
                        k.as_deref() == Some(kind.as_str())
                    }
                    _ => false,
                });
                match resolver.map(|mg| &mg.m) {
                    Some(Migration::SetDefault { value, .. }) => {
                        for e in ents {
                            fixes.entry(e.id.clone()).or_default().set.insert(prop.clone(), value.clone());
                        }
                    }
                    Some(Migration::Convert { .. }) => {
                        let def = new.prop_def(kind, prop).cloned();
                        let mut dropped = 0;
                        for e in ents {
                            let converted = match (&def, e.props.get(prop)) {
                                (Some(d), Some(v)) => convert(d, v),
                                _ => None,
                            };
                            if converted.is_none() {
                                dropped += 1;
                            }
                            fixes
                                .entry(e.id.clone())
                                .or_default()
                                .set
                                .insert(prop.clone(), converted.unwrap_or(Value::Null));
                        }
                        if dropped > 0 {
                            warnings.push(format!(
                                "convert {kind}.{prop}: {dropped} values could not be converted and are dropped"
                            ));
                        }
                    }
                    Some(Migration::DropValues { .. }) => {
                        for e in ents {
                            fixes.entry(e.id.clone()).or_default().set.insert(prop.clone(), Value::Null);
                        }
                    }
                    _ if gone => {}
                    _ if removed_prop && !strict && acknowledged(at) => {
                        warnings.push(format!("{n} {kind} entities keep untyped values of {prop} (acknowledged)"));
                    }
                    _ if removed_prop => needs.push(format!("{reason}: add drop_values or acknowledge")),
                    _ => needs.push(format!("{reason}: add set_default, convert or drop_values")),
                }
            }
            Issue::MissingRequired(prop) => {
                let fill = migs.iter().find_map(|mg| match &mg.m {
                    Migration::SetDefault { kind: k, property: p, value }
                        if p.as_deref() == Some(prop.as_str()) && kind_matches(base, &new, kind, k.as_deref()) =>
                    {
                        Some(value.clone())
                    }
                    _ => None,
                });
                match fill {
                    Some(v) => {
                        for e in ents {
                            fixes.entry(e.id.clone()).or_default().set.insert(prop.clone(), v.clone());
                        }
                    }
                    None => warnings.push(format!(
                        "{n} existing {kind} entities lack the newly required {prop} (add set_default to fill it)"
                    )),
                }
            }
        }
    }

    // Link issues.
    for ((rel, issue), links) in &link_issues {
        if *issue == LinkIssue::RemovedRel && renamed_rels.contains(rel.as_str()) {
            continue;
        }
        let n = links.len();
        let at = attribute(None, None, Some(rel));
        breaking = true;
        let reason = match issue {
            LinkIssue::RemovedRel => format!("{n} links use {rel}, which the new schema no longer defines"),
            LinkIssue::BadEndpoints => format!("{n} {rel} links connect kinds the new definition does not allow"),
            LinkIssue::Cardinality => format!("{n} {rel} links exceed the new single-valued cardinality"),
        };
        add_reason(&mut impacts, at, reason.clone());
        let resolver = migs.iter().find(|mg| match (&mg.m, issue) {
            (Migration::DeleteLinks { rel: r }, _) => r.as_deref() == Some(rel.as_str()),
            (Migration::MoveLinks { rel: r, .. }, LinkIssue::RemovedRel) => r.as_deref() == Some(rel.as_str()),
            (Migration::KeepLatest { rel: r }, LinkIssue::Cardinality) => r.as_deref() == Some(rel.as_str()),
            _ => false,
        });
        match resolver.map(|mg| &mg.m) {
            Some(Migration::DeleteLinks { .. }) | Some(Migration::KeepLatest { .. }) => {
                link_del.extend(links.iter().map(|l| link_key(l)));
            }
            Some(Migration::MoveLinks { to, .. }) => {
                for l in links {
                    link_move.insert(link_key(l), to.clone());
                }
            }
            _ if *issue == LinkIssue::RemovedRel && !strict && acknowledged(at) => {
                warnings.push(format!("{n} {rel} links stay as untyped relations (acknowledged)"));
            }
            _ => needs.push(format!(
                "{reason}: add {}",
                match issue {
                    LinkIssue::RemovedRel => "delete_links or move_links",
                    LinkIssue::BadEndpoints => "delete_links",
                    LinkIssue::Cardinality => "keep_latest or delete_links",
                }
            )),
        }
    }

    // Vocabulary breaks and the data moves implied by renames.
    for (i, c) in set.changes.iter().enumerate() {
        match c {
            SchemaChange::PutLinkType { name, def, .. } => {
                if let Some(old) = base.link_type(name) {
                    let in_use = links_by_rel.get(name.as_str()).is_some_and(|v| !v.is_empty());
                    if let Some(inv) = &old.inverse
                        && def.inverse.as_ref() != Some(inv)
                    {
                        breaking = true;
                        let reason = format!("the inverse name {inv} of {name} no longer resolves");
                        add_reason(&mut impacts, Some(i), reason.clone());
                        if !acknowledged(Some(i)) {
                            needs.push(format!("{reason}: add acknowledge"));
                        }
                    }
                    if old.symmetric != def.symmetric && in_use {
                        breaking = true;
                        let reason = format!("{name} changes symmetry while links use it");
                        add_reason(&mut impacts, Some(i), reason.clone());
                        if !acknowledged(Some(i)) {
                            needs.push(format!("{reason}: add acknowledge"));
                        }
                    }
                }
            }
            SchemaChange::DeleteInterface { name } => {
                warnings.push(format!("queries for kind {name} stop matching its former implementers"));
            }
            SchemaChange::DeleteActionType { name } => {
                warnings.push(format!("callers of action {name} will get not_found; existing receipts stay undoable"));
            }
            SchemaChange::RenameObjectType { from, to } => {
                let ents: Vec<&Entity> = g.of_kind(from).collect();
                if !ents.is_empty() {
                    breaking = true;
                    add_reason(
                        &mut impacts,
                        Some(i),
                        format!("ids of {} entities change from {from}:… to {to}:… (old ids redirect)", ents.len()),
                    );
                    for e in ents {
                        fixes.entry(e.id.clone()).or_default().retype = Some(to.clone());
                    }
                }
            }
            SchemaChange::RenameProperty { object_type, from, to } => {
                let kinds = &scopes[i].kinds;
                let mut n = 0;
                for k in kinds {
                    for e in g.of_kind(k) {
                        if let Some(v) = e.props.get(from) {
                            let f = fixes.entry(e.id.clone()).or_default();
                            f.set.insert(from.clone(), Value::Null);
                            f.set.insert(to.clone(), v.clone());
                            n += 1;
                        }
                    }
                }
                if n > 0 {
                    breaking = true;
                    add_reason(
                        &mut impacts,
                        Some(i),
                        format!(
                            "{n} entities move their {object_type}.{from} values to {to}; callers must use the new name"
                        ),
                    );
                }
            }
            SchemaChange::RenameLinkType { from, to } => {
                let links = links_by_rel.get(from.as_str()).cloned().unwrap_or_default();
                if !links.is_empty() {
                    breaking = true;
                    add_reason(
                        &mut impacts,
                        Some(i),
                        format!("{} links move from {from} to {to}; callers must use the new name", links.len()),
                    );
                    for l in links {
                        link_move.insert(link_key(l), to.clone());
                    }
                }
            }
            _ => {}
        }
    }

    // Build the migration.
    let mut mutations: Vec<Mutation> = Vec::new();
    let mut redirects: Vec<(Id, Id)> = Vec::new();
    let mut summary = MigrationSummary::default();
    let mut id_map: HashMap<&str, String> = HashMap::new();
    for (id, f) in &fixes {
        if let (Some(to), false) = (&f.retype, f.delete) {
            let key = id.split_once(':').map(|(_, k)| k).unwrap_or(id);
            let new_id = format!("{to}:{key}");
            if g.contains(&new_id) && !fixes.get(&new_id).is_some_and(|x| x.delete || x.retype.is_some()) {
                plan.errors.push(format!("cannot retype {id}: {new_id} already exists"));
            }
            id_map.insert(id.as_str(), new_id);
        }
    }
    let mapped = |id: &str| -> String { id_map.get(id).cloned().unwrap_or_else(|| id.to_string()) };
    // 1. Retyped entities are re-created under their new ids.
    let mut relinked: BTreeSet<LinkKey> = BTreeSet::new();
    for (id, f) in &fixes {
        let Some(new_id) = id_map.get(id.as_str()) else { continue };
        let Some(e) = g.get(id) else { continue };
        let mut props = e.props.clone();
        for (k, v) in &f.set {
            if v.is_null() {
                props.remove(k);
            } else {
                props.insert(k.clone(), v.clone());
            }
        }
        mutations.push(Mutation::Upsert(EntityInput {
            id: Some(new_id.clone()),
            kind: f.retype.clone().unwrap_or_default(),
            workspace: Some(e.workspace.clone()),
            title: Some(e.title.clone()),
            state: f.state.clone().or_else(|| e.state.clone()),
            props,
            aliases: e.aliases.clone(),
            provenance: e.provenance.clone(),
            ..Default::default()
        }));
        redirects.push((id.clone(), new_id.clone()));
        summary.entities_retyped += 1;
        for key in g.incident_keys(id) {
            if !relinked.insert(key.clone()) || link_del.contains(&key) {
                continue;
            }
            let (from, rel, to) = &key;
            if fixes.get(from).is_some_and(|x| x.delete) || fixes.get(to).is_some_and(|x| x.delete) {
                continue;
            }
            let rel = link_move.get(&key).cloned().unwrap_or_else(|| rel.clone());
            let props = g.get_link(&key.0, &key.1, &key.2).map(|l| l.props.clone()).unwrap_or_default();
            mutations.push(Mutation::Link(LinkInput { from: mapped(from), rel, to: mapped(to), props }));
        }
    }
    // 2. Other fixes patch entities in place.
    for (id, f) in &fixes {
        if f.delete || id_map.contains_key(id.as_str()) {
            continue;
        }
        let Some(e) = g.get(id) else { continue };
        if f.state.is_none() && f.set.is_empty() {
            continue;
        }
        mutations.push(Mutation::Upsert(EntityInput {
            id: Some(id.clone()),
            kind: e.kind.clone(),
            state: f.state.clone(),
            props: f.set.clone(),
            ..Default::default()
        }));
        summary.entities_updated += 1;
    }
    // 3. Link deletions and moves between entities that stay.
    let staying = |k: &LinkKey| {
        !relinked.contains(k)
            && !fixes.get(&k.0).is_some_and(|x| x.delete || x.retype.is_some())
            && !fixes.get(&k.2).is_some_and(|x| x.delete || x.retype.is_some())
    };
    for key in &link_del {
        summary.links_deleted += 1;
        if staying(key) {
            mutations.push(Mutation::Unlink(LinkInput::new(key.0.clone(), key.1.clone(), key.2.clone())));
        }
    }
    for (key, to_rel) in &link_move {
        summary.links_moved += 1;
        if link_del.contains(key) || !staying(key) {
            continue;
        }
        let props = g.get_link(&key.0, &key.1, &key.2).map(|l| l.props.clone()).unwrap_or_default();
        mutations.push(Mutation::Unlink(LinkInput::new(key.0.clone(), key.1.clone(), key.2.clone())));
        mutations.push(Mutation::Link(LinkInput {
            from: key.0.clone(),
            rel: to_rel.clone(),
            to: key.2.clone(),
            props,
        }));
    }
    // 4. Deletions (removed kinds and the old ids of retyped entities).
    for (id, f) in &fixes {
        if f.delete {
            summary.entities_deleted += 1;
            mutations.push(Mutation::Delete(EntityRef { id: id.clone() }));
        } else if id_map.contains_key(id.as_str()) {
            mutations.push(Mutation::Delete(EntityRef { id: id.clone() }));
        }
    }
    summary.mutations = mutations.len();

    // Unused migrations are worth a warning: they usually mean a typo.
    for mg in &migs {
        let used = match &mg.m {
            Migration::Acknowledge { .. } => true,
            Migration::DeleteEntities { kind } | Migration::Retype { kind, .. } => {
                kind.as_ref().is_some_and(|k| per_kind.keys().any(|(kk, i)| kk == k && *i == Issue::UnknownKind))
            }
            Migration::MapStates { kind, .. } => per_kind
                .keys()
                .any(|(kk, i)| matches!(i, Issue::BadState(_)) && kind_matches(base, &new, kk, kind.as_deref())),
            Migration::SetDefault { kind, property, .. }
            | Migration::Convert { kind, property }
            | Migration::DropValues { kind, property } => per_kind.keys().any(|(kk, i)| match i {
                Issue::BadValue(p) | Issue::RemovedProperty(p) | Issue::MissingRequired(p) => {
                    property.as_deref() == Some(p.as_str()) && kind_matches(base, &new, kk, kind.as_deref())
                }
                _ => false,
            }),
            Migration::DeleteLinks { rel } | Migration::MoveLinks { rel, .. } | Migration::KeepLatest { rel } => {
                rel.as_ref().is_some_and(|r| link_issues.keys().any(|(rr, _)| rr == r))
            }
        };
        if !used {
            warnings.push(format!(
                "migration {} matches no affected data",
                serde_json::to_string(&mg.m).unwrap_or_default()
            ));
        }
    }

    let reindex: Vec<String> = all_kinds.iter().map(|k| k.to_string()).collect();
    plan.entities_affected = scoped_entities.len();
    plan.links_affected = scoped_links.len();
    plan.changes = impacts;
    plan.breaking = breaking;
    plan.needs_migration = needs;
    plan.warnings = warnings;
    plan.migration = summary;
    plan.valid = plan.errors.is_empty();
    Planned { plan, schema: Some(new), mutations, redirects, reindex }
}

fn entity_issues(base: &Schema, new: &Schema, e: &Entity) -> Vec<Issue> {
    let mut out = Vec::new();
    let Some(ty) = new.object_types.get(&e.kind) else {
        if base.object_types.contains_key(&e.kind) {
            out.push(Issue::UnknownKind);
        }
        return out;
    };
    if let Some(s) = &e.state
        && !ty.states.is_empty()
        && !ty.states.contains(s)
    {
        out.push(Issue::BadState(s.clone()));
    }
    for (k, v) in &e.props {
        match new.prop_def(&e.kind, k) {
            Some(def) if !def.accepts(v) => out.push(Issue::BadValue(k.clone())),
            None if base.prop_def(&e.kind, k).is_some() => out.push(Issue::RemovedProperty(k.clone())),
            _ => {}
        }
    }
    for (k, def) in new.all_props(&e.kind) {
        let was_required = base.prop_def(&e.kind, k).is_some_and(|d| d.required);
        if def.required && !was_required && e.props.get(k).is_none_or(Value::is_null) {
            out.push(Issue::MissingRequired(k.clone()));
        }
    }
    out
}

fn summarize(base: &Schema, c: &SchemaChange) -> String {
    let verb = |exists: bool| if exists { "change" } else { "add" };
    match c {
        SchemaChange::PutObjectType { name, .. } => {
            format!("{} object type {name}", verb(base.object_types.contains_key(name)))
        }
        SchemaChange::DeleteObjectType { name, .. } => format!("delete object type {name}"),
        SchemaChange::RenameObjectType { from, to } => format!("rename object type {from} to {to}"),
        SchemaChange::PutProperty { object_type, name, def, .. } => format!(
            "{} property {object_type}.{name} ({})",
            verb(base.object_types.get(object_type).is_some_and(|t| t.properties.contains_key(name))),
            def.expected()
        ),
        SchemaChange::DeleteProperty { object_type, name, .. } => format!("delete property {object_type}.{name}"),
        SchemaChange::RenameProperty { object_type, from, to } => {
            format!("rename property {object_type}.{from} to {to}")
        }
        SchemaChange::PutLinkType { name, def, .. } => {
            let side = |v: &[String]| if v.is_empty() { "*".to_string() } else { v.join("|") };
            let mut out = format!(
                "{} link type {} -{name}-> {}",
                verb(base.link_types.contains_key(name)),
                side(&def.from),
                side(&def.to)
            );
            let before = base.link_types.get(name);
            let flag = |on: bool, was: Option<bool>, word: &str| match (on, was) {
                (true, Some(true)) | (true, None) => format!(" ({word})"),
                (true, Some(false)) => format!(" (now {word})"),
                (false, Some(true)) => format!(" (no longer {word})"),
                _ => String::new(),
            };
            out.push_str(&flag(def.transitive, before.map(|b| b.transitive), "transitive"));
            out.push_str(&flag(def.symmetric, before.map(|b| b.symmetric), "symmetric"));
            out
        }
        SchemaChange::DeleteLinkType { name, .. } => format!("delete link type {name}"),
        SchemaChange::RenameLinkType { from, to } => format!("rename link type {from} to {to}"),
        SchemaChange::PutInterface { name, .. } => {
            format!("{} interface {name}", verb(base.interfaces.contains_key(name)))
        }
        SchemaChange::DeleteInterface { name } => format!("delete interface {name}"),
        SchemaChange::PutActionType { name, .. } => {
            format!("{} action type {name}", verb(base.action_types.contains_key(name)))
        }
        SchemaChange::DeleteActionType { name } => format!("delete action type {name}"),
        SchemaChange::PutRule { def } => {
            format!("{} rule {} ({})", verb(base.rules.iter().any(|r| r.name == def.name)), def.name, def.describe())
        }
        SchemaChange::DeleteRule { name } => format!("delete rule {name}"),
        SchemaChange::PutShape { name, def } => {
            let target = match (&def.target, &def.target_subjects_of, &def.target_objects_of) {
                (Some(t), _, _) => format!("on {t}"),
                (_, Some(r), _) => format!("on subjects of {r}"),
                (_, _, Some(r)) => format!("on objects of {r}"),
                _ => format!("on {} listed ids", def.target_ids.len()),
            };
            format!(
                "{} shape {name} ({target}, {} property shapes)",
                verb(base.shapes.contains_key(name)),
                def.properties.len()
            )
        }
        SchemaChange::DeleteShape { name } => format!("delete shape {name}"),
        SchemaChange::PutMetric { name, .. } => format!("{} metric {name}", verb(base.metrics.contains_key(name))),
        SchemaChange::DeleteMetric { name } => format!("delete metric {name}"),
        SchemaChange::PutGlossaryTerm { section, word, target } => {
            format!("map '{word}' to {target} ({})", section_name(*section))
        }
        SchemaChange::DeleteGlossaryTerm { section, word } => format!("forget '{word}' ({})", section_name(*section)),
        SchemaChange::SetAnalysis { .. } => "replace the analysis settings".into(),
    }
}

// ── storage of versions ──────────────────────────────────────────────────

pub(crate) fn version_key(v: u64) -> String {
    format!("{v:012}")
}

fn read_head(store: &dyn Store) -> Result<Option<u64>> {
    Ok(store.kv_get(NS_SCHEMA, "head")?.and_then(|v| v.get("version").and_then(Value::as_u64)))
}

fn read_version(store: &dyn Store, v: u64) -> Result<Option<SchemaVersion>> {
    match store.kv_get(NS_VERSIONS, &version_key(v))? {
        Some(doc) => {
            let mut sv: SchemaVersion =
                serde_json::from_value(doc).map_err(|e| Error::Storage(format!("schema version {v}: {e}")))?;
            sv.schema.compile().map_err(|e| Error::Storage(format!("schema version {v} does not compile: {e}")))?;
            Ok(Some(sv))
        }
        None => Ok(None),
    }
}

/// The schema to open with: the stored head (live mode) or the config schema.
pub(crate) fn load_active(store: &dyn Store, config: &Schema, mode: SchemaMode) -> Result<(Schema, u64)> {
    let Some(head) = read_head(store)? else { return Ok((config.clone(), 0)) };
    let Some(sv) = read_version(store, head)? else {
        return Err(Error::Storage(format!("schema head points at missing version {head}")));
    };
    match mode {
        SchemaMode::Live => Ok((sv.schema, head)),
        SchemaMode::Config => Ok((config.clone(), head)),
    }
}

/// The stored head when it differs from `current` (another process moved it).
pub(crate) fn reload_active(
    store: &dyn Store,
    current: &crate::engine::ActiveSchema,
    mode: SchemaMode,
) -> Result<Option<crate::engine::ActiveSchema>> {
    let Some(head) = read_head(store)? else { return Ok(None) };
    if head == current.version {
        return Ok(None);
    }
    let schema = match mode {
        SchemaMode::Live => Arc::new(
            read_version(store, head)?.ok_or_else(|| Error::Storage(format!("missing schema version {head}")))?.schema,
        ),
        SchemaMode::Config => current.schema.clone(),
    };
    Ok(Some(crate::engine::ActiveSchema { schema, version: head }))
}

/// KV writes that store a version and move the head to it.
fn version_kv(sv: &SchemaVersion) -> Result<Vec<(String, String, Value)>> {
    Ok(vec![
        (NS_VERSIONS.into(), version_key(sv.version), serde_json::to_value(sv)?),
        (NS_SCHEMA.into(), "head".into(), json!({ "version": sv.version })),
    ])
}

fn config_kv(config: &Schema) -> Result<(String, String, Value)> {
    Ok((NS_SCHEMA.into(), "config".into(), json!({ "hash": fingerprint(config), "schema": config })))
}

/// How a version came about.
#[derive(Clone, Debug)]
pub(crate) struct VersionOrigin {
    pub source: &'static str,
    pub proposal: Option<String>,
    /// Extra KV committed with the version.
    pub kv: Vec<(String, String, Value)>,
    /// Config baseline to record (imports).
    pub config: Option<Schema>,
    /// Extra audit events committed with the version.
    pub events: Vec<crate::model::Event>,
    /// The proposal this version applies and the approval that applies it;
    /// the proposal is recorded as applied in the same transaction.
    pub approve: Option<(String, crate::proposals::Review, u32)>,
}

impl VersionOrigin {
    pub fn api() -> Self {
        Self::of("api")
    }

    pub fn of(source: &'static str) -> Self {
        Self { source, proposal: None, kv: Vec::new(), config: None, events: Vec::new(), approve: None }
    }
}

// ── the live schema API on the ontology ──────────────────────────────────

impl Ontology {
    fn schema_reader(&self, p: &Principal) -> Result<()> {
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        Ok(())
    }

    /// Store the first version or import changed schema files (called by `open`).
    pub(crate) fn bootstrap_schema(&self, config: Schema) -> Result<()> {
        let store = self.store().clone();
        let mode = self.options().schema_mode;
        let ctx = ActionContext::system(self.options().default_workspace.clone()).with_source("schema");
        // First open: store version 1, unless another process just did.
        self.transact(&ctx, |o, _| {
            let active = o.active_schema();
            if active.version != 0 {
                return Ok((Vec::new(), Extra::default(), ()));
            }
            let now = now_ms();
            let sv = SchemaVersion {
                version: 1,
                parent: None,
                ts: now,
                at: fmt_ts(now),
                actor: "system".into(),
                request_id: ctx.request_id.clone(),
                source: "config".into(),
                message: "initial schema from configuration".into(),
                proposal: None,
                changes: Vec::new(),
                diff: diff(&Schema::empty(), &config),
                breaking: false,
                migration: MigrationSummary::default(),
                schema: config.clone(),
            };
            let mut kv = version_kv(&sv)?;
            kv.push(config_kv(&config)?);
            Ok((Vec::new(), Extra { kv, schema: Some((active.schema.clone(), 1)), ..Default::default() }, ()))
        })?;
        let hash = fingerprint(&config);
        let baseline = store.kv_get(NS_SCHEMA, "config")?;
        let base_hash = baseline.as_ref().and_then(|b| b.get("hash")).and_then(Value::as_str).unwrap_or_default();
        if base_hash == hash {
            return Ok(());
        }
        match mode {
            SchemaMode::Config => {
                // Files are authoritative: record them as a version, data untouched.
                self.record_version(&ctx, config, "config", "schema files changed")
            }
            SchemaMode::Live => {
                let old: Option<Schema> =
                    baseline.and_then(|b| b.get("schema").cloned()).and_then(|v| Schema::from_json(v).ok());
                let changes = match &old {
                    Some(old) => changes_between(old, &config),
                    None => Vec::new(),
                };
                if changes.is_empty() {
                    let kv = vec![config_kv(&config)?];
                    self.transact(&ctx, |_, _| Ok((Vec::new(), Extra { kv: kv.clone(), ..Default::default() }, ())))?;
                    return Ok(());
                }
                let set = SchemaChangeSet {
                    changes,
                    message: "schema files changed since the last import".into(),
                    ..Default::default()
                };
                let origin = VersionOrigin { config: Some(config), ..VersionOrigin::of("import") };
                match self.schema_commit(&ctx, set, origin) {
                    Ok(_) => self.set_import_error(None),
                    Err(e) => self.set_import_error(Some(format!(
                        "schema files changed but were not imported: {e}; the stored version stays active \
                         (re-run with the needed migrations through schema.apply)"
                    ))),
                }
                Ok(())
            }
        }
    }

    /// Record `schema` as a new version without data migration (config mode).
    fn record_version(&self, ctx: &ActionContext, schema: Schema, source: &'static str, message: &str) -> Result<()> {
        self.transact(ctx, |o, _| {
            let active = o.active_schema();
            if fingerprint(&active.schema) == fingerprint(&schema) && active.version > 0 {
                // Another process recorded the same files already.
                let kv = vec![config_kv(&schema)?];
                return Ok((Vec::new(), Extra { kv, ..Default::default() }, ()));
            }
            let previous = o.schema_at_version(active.version)?.schema;
            let now = now_ms();
            let version = active.version + 1;
            let sv = SchemaVersion {
                version,
                parent: Some(active.version),
                ts: now,
                at: fmt_ts(now),
                actor: ctx.principal.id.clone(),
                request_id: ctx.request_id.clone(),
                source: source.into(),
                message: message.into(),
                proposal: None,
                diff: diff(&previous, &schema),
                changes: changes_between(&previous, &schema),
                breaking: false,
                migration: MigrationSummary::default(),
                schema: schema.clone(),
            };
            let mut kv = version_kv(&sv)?;
            kv.push(config_kv(&schema)?);
            let reindex: Vec<String> = schema.object_types.keys().cloned().collect();
            Ok((
                Vec::new(),
                Extra { kv, schema: Some((Arc::new(schema.clone()), version)), reindex, ..Default::default() },
                (),
            ))
        })?;
        Ok(())
    }

    /// Live schema status: version, mode, counts, governance.
    pub fn schema_status(&self, p: &Principal) -> Result<SchemaStatus> {
        self.schema_reader(p)?;
        let a = self.active_schema();
        let s = &a.schema;
        Ok(SchemaStatus {
            version: a.version,
            mode: self.options().schema_mode,
            name: s.name.clone(),
            object_types: s.object_types.len(),
            link_types: s.link_types.len(),
            interfaces: s.interfaces.len(),
            action_types: s.action_types.len(),
            rules: s.rules.len(),
            shapes: s.shapes.len(),
            metrics: s.metrics.len(),
            governance: self.options().governance.clone(),
            import_error: self.schema_import_error(),
        })
    }

    /// Dry run: validate `set`, report its impact and the migration it implies.
    pub fn schema_plan(&self, p: &Principal, set: &SchemaChangeSet) -> Result<SchemaPlan> {
        self.schema_reader(p)?;
        let active = self.active_schema();
        let g = self.inner_graph_read();
        let mut planned = plan(&active.schema, active.version, &g, set, self.options().strict);
        self.check_migration(&g, &mut planned, p);
        Ok(planned.plan)
    }

    /// Stage the migration to surface validation errors in the plan.
    fn check_migration(&self, g: &Graph, planned: &mut Planned, p: &Principal) {
        let (Some(schema), true) = (&planned.schema, planned.plan.valid) else { return };
        if planned.mutations.is_empty() {
            return;
        }
        let ctx = migration_ctx(&ActionContext::new(p.clone(), self.options().default_workspace.clone()));
        if let Err(e) = self.stage_only(g, schema, &ctx, planned.mutations.clone()) {
            planned.plan.errors.push(format!("migration does not validate: {e}"));
            planned.plan.valid = false;
        }
    }

    /// Apply a change set directly (roles in `governance.apply_roles`; never agents).
    pub fn schema_apply(&self, ctx: &ActionContext, set: SchemaChangeSet) -> Result<SchemaApplied> {
        self.authorize_schema_write(ctx, "schema.apply")?;
        self.schema_commit(ctx, set, VersionOrigin::api())
    }

    pub(crate) fn authorize_schema_write(&self, ctx: &ActionContext, action: &str) -> Result<()> {
        let gov = &self.options().governance;
        let reason = if self.options().schema_mode == SchemaMode::Config {
            Some("the schema is managed by configuration files (ontology.schema_mode = \"config\")".to_string())
        } else if !gov.live_edits && !ctx.principal.has(Role::System) {
            Some("live schema edits are disabled (governance.live_edits = false)".to_string())
        } else if !gov.can_apply(&ctx.principal) {
            Some(if ctx.principal.is_agent() {
                format!("{} is an agent: agents may propose schema changes but not apply them", ctx.principal.id)
            } else {
                format!(
                    "{} may not apply schema changes (needs one of: {}); file a proposal instead",
                    ctx.principal.id,
                    gov.apply_roles.iter().map(Role::as_str).collect::<Vec<_>>().join(", ")
                )
            })
        } else {
            None
        };
        match reason {
            Some(r) => {
                let mut ctx = ctx.clone();
                ctx.intent = Some(action.to_string());
                self.audit_denied(&ctx, &r, now_ms());
                Err(Error::Denied(r))
            }
            None => Ok(()),
        }
    }

    /// Plan and commit against the same state (retried when another process
    /// commits first). No authorization here: callers check it.
    pub(crate) fn schema_commit(
        &self,
        ctx: &ActionContext,
        set: SchemaChangeSet,
        origin: VersionOrigin,
    ) -> Result<SchemaApplied> {
        let mctx = migration_ctx(ctx);
        let (res, (info, plan)) = self.transact(&mctx, |o, g| {
            let active = o.active_schema();
            let mut planned = plan(&active.schema, active.version, g, &set, o.options().strict);
            o.check_migration(g, &mut planned, &ctx.principal);
            if !planned.plan.applicable() {
                return Err(planned.plan.refusal());
            }
            if planned.plan.diff.is_empty() && planned.mutations.is_empty() {
                return Err(Error::invalid("the change set does not change the schema"));
            }
            let schema = planned.schema.take().expect("valid plans carry a schema");
            let now = now_ms();
            let version = active.version + 1;
            let sv = SchemaVersion {
                version,
                parent: Some(active.version),
                ts: now,
                at: fmt_ts(now),
                actor: ctx.principal.id.clone(),
                request_id: ctx.request_id.clone(),
                source: origin.source.into(),
                message: set.message.clone(),
                proposal: origin.proposal.clone(),
                changes: set.changes.clone(),
                diff: planned.plan.diff.clone(),
                breaking: planned.plan.breaking,
                migration: planned.plan.migration.clone(),
                schema: schema.clone(),
            };
            let mut kv = version_kv(&sv)?;
            kv.extend(origin.kv.iter().cloned());
            if let Some(c) = &origin.config {
                kv.push(config_kv(c)?);
            }
            if let Some((pid, review, revision)) = &origin.approve {
                // Read the proposal inside the transaction: if another
                // approver applied it first, this attempt must not.
                let mut rec = o.load_proposal(pid)?;
                if rec.status != crate::proposals::ProposalStatus::Open {
                    return Err(Error::Conflict(
                        format!("proposal {pid} is {:?}, not open", rec.status).to_lowercase(),
                    ));
                }
                if rec.approvals().contains(&review.by.as_str()) {
                    return Err(Error::Conflict(format!("{} already approved {pid}", review.by)));
                }
                if rec.revision != *revision {
                    return Err(Error::Conflict(format!("proposal {pid} was revised; review it again")));
                }
                rec.reviews.push(review.clone());
                rec.status = crate::proposals::ProposalStatus::Applied;
                rec.applied_version = Some(version);
                rec.error = None;
                rec.updated_at = now;
                kv.push((crate::proposals::NS_PROPOSALS.into(), rec.id.clone(), serde_json::to_value(&rec)?));
            }
            let event = o.event(
                ctx,
                &ctx.workspace,
                "schema.change",
                None,
                now,
                json!({
                    "version": version,
                    "parent": active.version,
                    "source": origin.source,
                    "proposal": origin.proposal,
                    "message": set.message,
                    "changes": set.changes.iter().map(|c| format!("{} {}", c.op(), c.target())).collect::<Vec<_>>(),
                    "diff": sv.diff.summary(),
                    "breaking": sv.breaking,
                    "migration": sv.migration,
                }),
                Vec::new(),
            );
            let mut events = origin.events.clone();
            events.push(event);
            let extra = Extra {
                kv,
                redirects: planned.redirects.clone(),
                events,
                schema: Some((Arc::new(schema), version)),
                reindex: planned.reindex.clone(),
            };
            Ok((planned.mutations, extra, (SchemaVersionInfo::from(&sv), planned.plan)))
        })?;
        Ok(SchemaApplied { version: info, plan, revision: res.revision })
    }

    /// Schema versions, newest first.
    pub fn schema_history(&self, p: &Principal, limit: usize) -> Result<Vec<SchemaVersionInfo>> {
        self.schema_reader(p)?;
        let mut out: Vec<SchemaVersionInfo> = self
            .store()
            .kv_scan(NS_VERSIONS, "")?
            .into_iter()
            .rev()
            .take(limit)
            .filter_map(|(_, v)| serde_json::from_value::<SchemaVersion>(v).ok())
            .map(|sv| SchemaVersionInfo::from(&sv))
            .collect();
        out.sort_by_key(|v| std::cmp::Reverse(v.version));
        Ok(out)
    }

    pub(crate) fn schema_at_version(&self, version: u64) -> Result<SchemaVersion> {
        read_version(&**self.store(), version)?.ok_or_else(|| Error::not_found(format!("schema version {version}")))
    }

    /// One stored version with its full document.
    pub fn schema_version_get(&self, p: &Principal, version: u64) -> Result<SchemaVersion> {
        self.schema_reader(p)?;
        self.schema_at_version(version)
    }

    /// The version in force at `ts`.
    pub fn schema_as_of(&self, p: &Principal, ts: i64) -> Result<SchemaVersion> {
        self.schema_reader(p)?;
        let mut best: Option<SchemaVersion> = None;
        for (_, v) in self.store().kv_scan(NS_VERSIONS, "")? {
            if let Ok(sv) = serde_json::from_value::<SchemaVersion>(v)
                && sv.ts <= ts
                && best.as_ref().is_none_or(|b| sv.version > b.version)
            {
                best = Some(sv);
            }
        }
        let mut sv = best.ok_or_else(|| Error::not_found(format!("schema version at {}", fmt_ts(ts))))?;
        sv.schema.compile()?;
        Ok(sv)
    }

    /// Differences between two versions (`to` defaults to the active one).
    pub fn schema_diff(&self, p: &Principal, from: u64, to: Option<u64>) -> Result<SchemaDiff> {
        self.schema_reader(p)?;
        let a = self.schema_at_version(from)?.schema;
        let b = match to {
            Some(v) => self.schema_at_version(v)?.schema,
            None => (*self.schema()).clone(),
        };
        Ok(diff(&a, &b))
    }

    /// Make `version`'s schema the active one again (as a new version).
    pub fn schema_revert(
        &self,
        ctx: &ActionContext,
        version: u64,
        migrations: Vec<Migration>,
        dry_run: bool,
    ) -> Result<SchemaRevert> {
        let target = self.schema_at_version(version)?;
        let set = SchemaChangeSet {
            changes: changes_between(&self.schema(), &target.schema),
            message: format!("revert to schema version {version}"),
            migrations,
            expected_version: None,
        };
        self.revert_with(ctx, set, "revert", dry_run)
    }

    /// Undo one version: apply the inverse of its diff on top of the active schema.
    pub fn schema_undo(
        &self,
        ctx: &ActionContext,
        version: u64,
        migrations: Vec<Migration>,
        dry_run: bool,
    ) -> Result<SchemaRevert> {
        let sv = self.schema_at_version(version)?;
        let parent = sv.parent.ok_or_else(|| Error::invalid(format!("version {version} has no parent to undo to")))?;
        let before = self.schema_at_version(parent)?.schema;
        let inverse = changes_between(&sv.schema, &before);
        let current = self.schema();
        // Only undo definitions the version touched, against what is active now.
        let mut changes = Vec::new();
        for c in inverse {
            let still = match &c {
                SchemaChange::PutObjectType { name, def, .. } => {
                    current.object_types.get(name).map(|d| json!(d)) != Some(json!(def))
                }
                SchemaChange::DeleteObjectType { name, .. } => current.object_types.contains_key(name),
                SchemaChange::PutLinkType { name, def, .. } => {
                    current.link_types.get(name).map(|d| json!(d)) != Some(json!(def))
                }
                SchemaChange::DeleteLinkType { name, .. } => current.link_types.contains_key(name),
                SchemaChange::DeleteInterface { name } => current.interfaces.contains_key(name),
                SchemaChange::DeleteActionType { name } => current.action_types.contains_key(name),
                SchemaChange::DeleteMetric { name } => current.metrics.contains_key(name),
                SchemaChange::DeleteRule { name } => current.rules.iter().any(|r| &r.name == name),
                SchemaChange::DeleteShape { name } => current.shapes.contains_key(name),
                _ => true,
            };
            if still {
                changes.push(c);
            }
        }
        let set = SchemaChangeSet {
            changes,
            message: format!("undo schema version {version}"),
            migrations,
            expected_version: None,
        };
        self.revert_with(ctx, set, "undo", dry_run)
    }

    fn revert_with(
        &self,
        ctx: &ActionContext,
        set: SchemaChangeSet,
        source: &'static str,
        dry_run: bool,
    ) -> Result<SchemaRevert> {
        if set.changes.is_empty() {
            return Err(Error::invalid("nothing to revert: the active schema already matches"));
        }
        if dry_run {
            let plan = self.schema_plan(&ctx.principal, &set)?;
            return Ok(SchemaRevert { changes: set.changes, plan: Some(plan), applied: None });
        }
        self.authorize_schema_write(ctx, &format!("schema.{source}"))?;
        let changes = set.changes.clone();
        let origin = VersionOrigin::of(source);
        let applied = self.schema_commit(ctx, set, origin)?;
        Ok(SchemaRevert { changes, plan: None, applied: Some(applied) })
    }
}

/// Result of a revert or undo (a dry run carries the plan).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaRevert {
    pub changes: Vec<SchemaChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<SchemaPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<SchemaApplied>,
}

/// Migrations run with system authority across workspaces, attributed to the caller.
pub(crate) fn migration_ctx(ctx: &ActionContext) -> ActionContext {
    let mut p = Principal::new(ctx.principal.id.clone(), vec![Role::System], vec!["*".into()]);
    p.entity = ctx.principal.entity.clone();
    let mut m = ctx.clone();
    m.principal = p;
    m.intent = Some("schema.migration".into());
    m
}
