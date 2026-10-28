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
//!
//! A plan is semantic as well as structural. Besides the entities and links
//! in scope (the blast radius) it reports:
//!
//! * **Consequences** ([`Consequences`]): the facts the reasoner derives
//!   under the planned schema and not under the current one, and the reverse,
//!   over the same graph. An inference is not an assertion, so a change that
//!   touches no entity and no link can still change what the ontology
//!   entails: marking `depends_on` transitive adds a derived fact for every
//!   dependency chain. The change is *conservative* when no added fact is
//!   stated purely in the current vocabulary; derived facts are the engine's
//!   opinion (`derived`), not a checker's verdict.
//! * **Risk** ([`Risk`]): `high` when the change is breaking, needs a
//!   migration or violates a lock; `medium` when it deletes or renames a
//!   definition, removes a derived fact, or is not shown to be conservative;
//!   `low` when it is purely additive and conservative. Every level carries
//!   its reasons.
//! * **Lock violations** (see [`crate::locks`]): a plan that deletes, renames
//!   or breaks a locked definition is not applicable unless an admin passes
//!   `override_locks`.
//! * **Likely renames** ([`LikelyRename`]): a change set that deletes one
//!   definition and adds a similar one in the same section probably meant a
//!   rename, which keeps the data a delete would orphan.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub use crate::action_types::Risk;
use crate::action_types::{ActionType, Check};
use crate::engine::{ActionContext, Extra};
use crate::graph::{Graph, LinkKey};
use crate::locks::{self, LockViolation, SchemaLock};
use crate::metrics::MetricDef;
use crate::model::{Entity, EntityInput, EntityRef, Id, LinkInput, Mutation};
use crate::policy::{self, Access, Principal, Role};
use crate::proposals::Proposer;
use crate::reason::{self, AddedFact, Closure, Fact, ReasonOptions};
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
    /// May agents write the graph directly (`entity.upsert`, `link.create`,
    /// `graph.apply`, `ingest.batch`, …)? Off: they change it through action
    /// types, whose checks, risk and audit apply.
    pub agent_raw_writes: bool,
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
            agent_raw_writes: false,
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
    /// Proceed although the set deletes, renames or breaks locked
    /// definitions. Honoured for admins only; overridden violations stay in
    /// the plan and are recorded in the `schema.change` event.
    #[serde(default, skip_serializing_if = "is_false")]
    pub override_locks: bool,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
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
    /// Links in scope: of the relation, or touching removed entities. An
    /// edit of a link type that leaves its endpoints, cardinality, symmetry,
    /// inverse and backing alone (a flag such as `transitive`, a weight, a
    /// label) touches none; what it does to entailment is in the plan's
    /// consequences.
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
    /// The facts that appear and disappear from the closure (see
    /// [`Consequences`]). Present on every plan the engine computes (a
    /// skipped block says why when they could not be computed); absent from
    /// plans stored before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consequences: Option<Consequences>,
    /// `low`, `medium` or `high`, explained by `risk_reasons`.
    #[serde(default)]
    pub risk: Risk,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risk_reasons: Vec<String>,
    /// Locked definitions the change deletes, renames or breaks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locked_violations: Vec<LockViolation>,
    /// Deletes paired with similar new definitions: probably renames.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub likely_renames: Vec<LikelyRename>,
    /// Shape results that appear and disappear under the planned schema
    /// (see [`PlanShapes`]); absent from plans stored before the field
    /// existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shapes: Option<PlanShapes>,
}

/// What a plan's shape delta means.
pub const PLAN_SHAPES_MEANS: &str = "Shape results under the planned schema that the current schema does not \
     produce (new_results) and the reverse (resolved), over the graph as it stands now and as the planner may see \
     it, asserted facts only (inferred = false); migrations are not applied. A shape report is a measurement, not a \
     proof; with complete false some results were cut and the counts are lower bounds.";

/// Results a plan's shape delta lists per side.
const PLAN_SHAPE_SAMPLES: usize = 20;

/// Results one side of a plan's shape delta may hold before it is cut.
const PLAN_SHAPE_LIMIT: usize = 100_000;

/// The shape results a schema change makes appear and disappear.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanShapes {
    /// The verdict under the current schema (absent when skipped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<crate::shapes::ShapeVerdict>,
    /// The verdict under the planned schema (absent when skipped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<crate::shapes::ShapeVerdict>,
    /// Results the planned schema produces that the current one does not.
    pub new_results: usize,
    /// Results of the current schema the planned one no longer produces.
    pub resolved: usize,
    pub samples: PlanShapeSamples,
    /// False when either report was cut (the counts are then lower bounds).
    pub complete: bool,
    /// Why the delta was not computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    pub means: String,
}

/// The first results on each side of a plan's shape delta.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanShapeSamples {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub new_results: Vec<crate::shapes::ShapeResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved: Vec<crate::shapes::ShapeResult>,
}

impl PlanShapes {
    /// A block that says why it holds nothing.
    pub fn skipped(reason: impl Into<String>) -> Self {
        Self { skipped: Some(reason.into()), means: PLAN_SHAPES_MEANS.into(), ..Default::default() }
    }
}

/// The shape delta of replacing `base` with `new` over `g` as `can_read`
/// sees it (skipped above the size cap of [`schema_consequences`]).
pub fn plan_shapes(base: &Schema, new: &Schema, g: &Graph, can_read: &dyn Fn(&Entity) -> bool) -> PlanShapes {
    if base.shapes.is_empty() && new.shapes.is_empty() {
        return PlanShapes::skipped("neither schema defines shapes");
    }
    let floor = asserted_floor(g, can_read);
    if floor > MAX_CONSEQUENCE_FACTS {
        return PlanShapes::skipped(too_large(floor));
    }
    let q = crate::shapes::ShapesQuery { limit: PLAN_SHAPE_LIMIT, ..Default::default() };
    let before = crate::shapes::validate(base, g, None, &q, can_read);
    let after = crate::shapes::validate(new, g, None, &q, can_read);
    let d = crate::shapes::delta(&before, &after);
    PlanShapes {
        before: Some(d.before),
        after: Some(d.after),
        new_results: d.new_results.len(),
        resolved: d.resolved.len(),
        samples: PlanShapeSamples {
            new_results: d.new_results.into_iter().take(PLAN_SHAPE_SAMPLES).collect(),
            resolved: d.resolved.into_iter().take(PLAN_SHAPE_SAMPLES).collect(),
        },
        complete: d.complete,
        skipped: None,
        means: PLAN_SHAPES_MEANS.into(),
    }
}

impl SchemaPlan {
    /// Can this plan be applied as is?
    pub fn applicable(&self) -> bool {
        self.valid && self.needs_migration.is_empty() && self.locked_violations.iter().all(|v| v.overridden)
    }

    /// Strip what belongs to one reader before the plan is stored where
    /// others read it (a proposal): the consequences are facts as the
    /// planner may see them, so the stored block holds none (only a note
    /// that they are computed per reader), and the risk is reassessed
    /// without them.
    pub(crate) fn without_reader_facts(&mut self, set: &SchemaChangeSet) {
        if self.consequences.is_none() {
            return;
        }
        self.consequences = Some(Consequences::skipped(STORED_CONSEQUENCES, SCHEMA_CONSEQUENCES_MEANS));
        // Shape results name the focus entities the planner may see.
        if self.shapes.is_some() {
            self.shapes = Some(PlanShapes::skipped(STORED_SHAPES));
        }
        (self.risk, self.risk_reasons) = assess_risk(self, set);
    }

    /// Take the reader-dependent parts of `fresh` (the same change set
    /// planned now, against the same schema version, for one reader): its
    /// consequences and lock violations, and the risk they imply.
    pub(crate) fn with_reader_view(&mut self, fresh: &SchemaPlan, set: &SchemaChangeSet) {
        self.consequences = fresh.consequences.clone();
        self.shapes = fresh.shapes.clone();
        self.locked_violations = fresh.locked_violations.clone();
        (self.risk, self.risk_reasons) = assess_risk(self, set);
    }

    /// The refusal for a plan that cannot be applied.
    pub fn refusal(&self) -> Error {
        if !self.valid {
            Error::Invalid(format!("schema change rejected: {}", self.errors.join("; ")))
        } else if self.locked_violations.iter().any(|v| !v.overridden) {
            Error::Conflict(locks::refusal_text(&self.locked_violations))
        } else {
            Error::Conflict(format!("breaking change needs a migration: {}", self.needs_migration.join("; ")))
        }
    }
}

/// Is a change conservative over the current vocabulary? A word, because
/// "not shown" is not "no".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conservativity {
    /// Both runs reached their fixpoint and no added fact is stated purely
    /// in the current vocabulary.
    Conservative,
    /// Some added fact is stated purely in the current vocabulary.
    NotConservative,
    /// Skipped, or a run stopped before its fixpoint: not shown either way.
    #[default]
    Undetermined,
}

/// What a change does to entailment: the facts the closure holds after it
/// and not before (`added`), and the reverse (`removed`).
///
/// Added facts are judged against the base signature: the names of the
/// current schema (object types, interfaces, link types, declared
/// properties, and the classes and relations its rules derive) plus every
/// entity of the current closure, the keywords and every literal. An added
/// fact all of whose terms are in the base signature says something new in
/// the old words; the change is conservative when there is none.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Consequences {
    pub added: usize,
    pub removed: usize,
    /// True only when shown: false when skipped or when either run stopped
    /// before its fixpoint (see `verdict`).
    pub conservative: bool,
    #[serde(default)]
    pub verdict: Conservativity,
    /// Added facts stated purely in the current vocabulary.
    pub new_over_existing_vocabulary: usize,
    /// Added facts per rule; asserted facts (a new `extends`, a link type
    /// made transitive) count under `asserted`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_rule: BTreeMap<String, usize>,
    /// The first added facts (at most 20), sorted, each with the rule and
    /// premises that derive it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples_added: Vec<AddedFact>,
    /// The first added facts over the existing vocabulary (at most 20): the
    /// witnesses that the change is not conservative.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub witnesses: Vec<AddedFact>,
    /// The first removed facts (at most 20), sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples_removed: Vec<Fact>,
    /// False when either run stopped before its fixpoint: the counts then
    /// compare two partial closures, and the verdict is undetermined.
    pub fixpoint_reached: bool,
    /// Terms the two runs refused to spell (summed). Facts naming them are
    /// left out of both closures, so the verdict is undetermined.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub refused: usize,
    /// Why the consequences were not computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    pub means: String,
}

impl Consequences {
    /// A block that says why it holds nothing.
    pub fn skipped(reason: impl Into<String>, means: &str) -> Self {
        Self { skipped: Some(reason.into()), means: means.to_string(), ..Default::default() }
    }
}

/// What a schema plan's consequences mean.
pub const SCHEMA_CONSEQUENCES_MEANS: &str = "Facts the reasoner derives (or the schema asserts) under the planned \
     schema and not under the current one, and the reverse, over the graph as it stands now and as the planner may \
     see it; migrations are not applied, their data edits are counted under `migration`. Derived facts are the \
     engine's opinion, not a checker's verdict. `conservative` means no added fact is stated purely in the current \
     vocabulary; it is shown only when both runs reached a fixpoint. Asserted state and property facts are left out: \
     the graph is the same, and only what the rules read differs.";

/// The shape delta of a stored plan: computed per reader, like its consequences.
pub const STORED_SHAPES: &str = "shape results are computed for each reader, as that reader may see the graph: \
     plan the change set (or read the proposal) to see them";

/// Why a stored proposal's plan holds no consequences.
pub const STORED_CONSEQUENCES: &str = "consequences are computed for each reader, as that reader may see the graph, \
     and are not stored with a proposal: read an open proposal to see them";

/// What an action plan's consequences mean.
pub const ACTION_CONSEQUENCES_MEANS: &str = "Facts the reasoner derives (or the store asserts) after this action's \
     mutations, staged on a fork, and not before, and the reverse, as the planner may see them. Derived facts are \
     the engine's opinion, not a checker's verdict. `conservative` uses the schema-plan definition (no added fact \
     stated purely in the current vocabulary); for data edits it is rarely meaningful, because a new link or state \
     between existing entities is such a fact by construction.";

/// Asserted facts above this many skip the consequences of a plan.
pub const MAX_CONSEQUENCE_FACTS: usize = 500_000;

/// Samples kept per list.
const CONSEQUENCE_SAMPLES: usize = 20;

/// A delete and a put in one section that look like one renamed definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LikelyRename {
    /// `object_types`, `link_types` or `properties`.
    pub section: String,
    /// The deleted definition (`issue.sev` for a property).
    pub from: String,
    /// The new definition.
    pub to: String,
    /// 0.3 × name similarity + 0.7 × structural similarity, in [0, 1].
    pub score: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

/// Pairs scoring below this are not reported.
const RENAME_THRESHOLD: f64 = 0.6;

/// Link types and properties carry only coarse structure (equal endpoints,
/// equal type), which almost any pair shares: below this name similarity
/// the structure is not counted and the name alone scores the pair.
const COARSE_NAME_FLOOR: f64 = 0.6;

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
/// Glossary words that name an object type or link type the changes delete
/// (and that no later change put back) go with it: a word for a deleted
/// definition names nothing. Words the changes delete themselves are gone
/// already; words for other definitions stay.
fn forget_deleted_words(base: &Schema, s: &mut Schema, changes: &[SchemaChange]) {
    let mut kinds: BTreeSet<&str> = BTreeSet::new();
    let mut rels: BTreeSet<String> = BTreeSet::new();
    for c in changes {
        match c {
            SchemaChange::DeleteObjectType { name, .. } => {
                kinds.insert(name);
            }
            SchemaChange::DeleteLinkType { name, .. } => {
                rels.insert(name.clone());
                rels.extend(base.link_types.get(name).and_then(|lt| lt.inverse.clone()));
            }
            _ => {}
        }
    }
    if kinds.is_empty() && rels.is_empty() {
        return;
    }
    let gone_kind = |k: &str| kinds.contains(k) && !s.is_type_or_interface(k);
    let kept: BTreeMap<String, String> =
        s.glossary.kinds.iter().filter(|(_, k)| !gone_kind(k)).map(|(w, k)| (w.clone(), k.clone())).collect();
    s.glossary.kinds = kept;
    // (The schema is not compiled yet: its inverse index is stale, so names
    // are looked up in the link types themselves.)
    let gone_rel = |r: &str| {
        let bare = r.strip_prefix('^').unwrap_or(r);
        rels.contains(bare) && !s.link_types.iter().any(|(n, lt)| n == bare || lt.inverse.as_deref() == Some(bare))
    };
    let kept: BTreeMap<String, String> =
        s.glossary.relations.iter().filter(|(_, r)| !gone_rel(r)).map(|(w, r)| (w.clone(), r.clone())).collect();
    s.glossary.relations = kept;
}

pub fn apply_changes(base: &Schema, changes: &[SchemaChange]) -> std::result::Result<Schema, Vec<String>> {
    let mut s = base.clone();
    let mut errors = Vec::new();
    for (i, c) in changes.iter().enumerate() {
        if let Err(e) = apply_one(&mut s, c) {
            errors.push(format!("change {} ({} {}): {e}", i + 1, c.op(), c.target()));
        }
    }
    forget_deleted_words(base, &mut s, changes);
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

pub(crate) fn apply_one(s: &mut Schema, c: &SchemaChange) -> std::result::Result<(), String> {
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
            // When no other type or interface still declares the old name, every
            // reader of it meant this property and follows the rename.
            let still_declared = s.object_types.values().any(|t| t.properties.contains_key(from))
                || s.interfaces.values().any(|i| i.properties.contains_key(from));
            for r in &mut s.rules {
                rename_prop_atoms(r, &kinds, still_declared, &old_path, &new_path);
            }
            let ours = |sh: &Shape| !still_declared || sh.target.as_ref().is_some_and(|t| kinds.contains(t));
            for sh in s.shapes.values_mut().filter(|sh| ours(sh)) {
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

/// Rename `prop.<from>` atoms of a Horn rule's body to `new_pred` where they
/// read a property of one of `kinds`: the subject is typed as one of them in
/// the body (`[?v, "a", kind]`) or is an id of one of them. With
/// `still_declared` false no other type declares the old name, so every
/// `old_pred` atom is renamed. Heads cannot hold property atoms.
fn rename_prop_atoms(r: &mut Rule, kinds: &[String], still_declared: bool, old_pred: &str, new_pred: &str) {
    let typed: BTreeSet<String> = r
        .when
        .iter()
        .filter(|a| a.len() == 3 && a[1].as_str() == Some("a"))
        .filter(|a| a[2].as_str().is_some_and(|k| kinds.iter().any(|x| x == k)))
        .filter_map(|a| a[0].as_str().map(str::to_string))
        .collect();
    for atom in r.when.iter_mut().filter(|a| a.len() == 3 && a[1].as_str() == Some(old_pred)) {
        let subject = atom[0].as_str().unwrap_or_default();
        let of_kind = subject.split_once(':').is_some_and(|(k, _)| kinds.iter().any(|x| x == k));
        if !still_declared || typed.contains(subject) || of_kind {
            atom[1] = Value::String(new_pred.to_string());
        }
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
    // Entities of the renamed kind move from `from:<key>` to `to:<key>`, so
    // rules and shapes that name one by id follow it.
    let prefix = format!("{from}:");
    let swap_id = |v: &mut String| {
        if let Some(key) = v.strip_prefix(&prefix) {
            *v = format!("{to}:{key}");
        }
    };
    let swap_id_value = |v: &mut Value| {
        if let Value::String(id) = v {
            swap_id(id);
        }
    };
    for r in &mut s.rules {
        for atom in r.when.iter_mut().chain(std::iter::once(&mut r.then)) {
            if atom.len() != 3 {
                continue;
            }
            let pred = atom[1].as_str().unwrap_or_default();
            let literal_object = pred == "a" || pred == "state" || pred.starts_with("prop.");
            if pred == "a" {
                swap_value(&mut atom[2]);
            }
            swap_id_value(&mut atom[0]);
            if !literal_object {
                swap_id_value(&mut atom[2]);
            }
        }
    }
    for sh in s.shapes.values_mut() {
        if let Some(t) = &mut sh.target {
            swap(t);
        }
        sh.target_ids.iter_mut().for_each(swap_id);
        for p in &mut sh.properties {
            if let Some(c) = &mut p.class {
                swap(c);
            }
            if matches!(ShapePath::parse(&p.path), ShapePath::Relations(_))
                && let Some(v) = &mut p.has_value
            {
                swap_id_value(v);
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
        let touches_links = match c {
            SchemaChange::PutLinkType { name, def, .. } => {
                base.link_types.get(name).is_none_or(|old| link_data_changes(old, def))
            }
            _ => true,
        };
        for r in sc.rels.iter().filter(|_| touches_links) {
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
                // Rules whose subject the rename could not tie to the renamed
                // type keep reading the old name (another type declares it).
                let old_pred = format!("prop.{from}");
                for r in new
                    .rules
                    .iter()
                    .filter(|r| r.when.iter().any(|a| a.get(1).and_then(Value::as_str) == Some(&old_pred)))
                {
                    warnings.push(format!(
                        "rule {} still reads {old_pred}, which {object_type} entities no longer carry; edit it if it meant {object_type}.{from}",
                        r.name
                    ));
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
    // 1. Retyped entities are re-created under their new ids, then their
    // links (after every new id exists: retyped entities may link each other).
    let mut relinked: BTreeSet<LinkKey> = BTreeSet::new();
    let mut relinks: Vec<Mutation> = Vec::new();
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
            relinks.push(Mutation::Link(LinkInput { from: mapped(from), rel, to: mapped(to), props }));
        }
    }
    mutations.append(&mut relinks);
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

    // Deletes that look like renames.
    let renames = likely_renames(base, &new, set);
    for r in &renames {
        warnings.push(rename_warning(base, g, r));
    }

    let reindex: Vec<String> = all_kinds.iter().map(|k| k.to_string()).collect();
    plan.likely_renames = renames;
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
            let n = def.properties.len();
            format!(
                "{} shape {name} ({target}, {n} property {})",
                verb(base.shapes.contains_key(name)),
                if n == 1 { "shape" } else { "shapes" }
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

/// Does `new` change what the stored links of a link type mean or allow
/// (endpoints, cardinality, symmetry, inverse, backing)? Flags such as
/// `transitive` or `dependency`, weights and labels leave the links alone.
fn link_data_changes(old: &LinkType, new: &LinkType) -> bool {
    let set = |v: &[String]| v.iter().cloned().collect::<BTreeSet<String>>();
    set(&old.from) != set(&new.from)
        || set(&old.to) != set(&new.to)
        || old.cardinality != new.cardinality
        || old.symmetric != new.symmetric
        || old.inverse != new.inverse
        || old.backing != new.backing
}

// ── likely renames ───────────────────────────────────────────────────────

/// Jaro-Winkler similarity of two strings, 0.0..=1.0.
fn jaro_winkler(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut a_hit = vec![false; a.len()];
    let mut b_hit = vec![false; b.len()];
    let mut matches = 0usize;
    for (i, ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if !b_hit[j] && b[j] == *ca {
                a_hit[i] = true;
                b_hit[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let a_seq = a.iter().zip(&a_hit).filter(|(_, h)| **h).map(|(c, _)| c);
    let b_seq = b.iter().zip(&b_hit).filter(|(_, h)| **h).map(|(c, _)| c);
    let transpositions = a_seq.zip(b_seq).filter(|(x, y)| x != y).count() / 2;
    let m = matches as f64;
    let jaro = (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64) / m) / 3.0;
    let prefix = a.iter().zip(&b).take(4).take_while(|(x, y)| x == y).count() as f64;
    jaro + prefix * 0.1 * (1.0 - jaro)
}

/// The best similarity between any name or label of one definition and any
/// of the other (trigram Jaccard or Jaro-Winkler, whichever is higher).
fn name_similarity(a: &[&str], b: &[&str]) -> f64 {
    let mut best = 0.0f64;
    for x in a.iter().filter(|x| !x.is_empty()) {
        for y in b.iter().filter(|y| !y.is_empty()) {
            let (x, y) = (x.to_lowercase(), y.to_lowercase());
            let s = if x == y { 1.0 } else { (crate::text::similarity(&x, &y) as f64).max(jaro_winkler(&x, &y)) };
            best = best.max(s);
        }
    }
    best
}

/// Jaccard similarity with the shared and total counts; `None` when both
/// sets are empty (the signal is undefined).
fn jaccard<T: Ord>(a: &BTreeSet<T>, b: &BTreeSet<T>) -> Option<(f64, usize, usize)> {
    let union = a.union(b).count();
    if union == 0 {
        return None;
    }
    let shared = a.intersection(b).count();
    Some((shared as f64 / union as f64, shared, union))
}

fn names_of<'a>(it: impl IntoIterator<Item = &'a String>) -> BTreeSet<&'a str> {
    it.into_iter().map(String::as_str).collect()
}

/// `0.3 × name + 0.7 × structure`, where structure is the weighted mean of
/// the defined signals (renormalized); the name alone when none is defined.
fn rename_score(name: f64, signals: &[(f64, f64)]) -> f64 {
    let total: f64 = signals.iter().map(|(w, _)| w).sum();
    let score = if total > 0.0 {
        let structure = signals.iter().map(|(w, v)| w * v).sum::<f64>() / total;
        0.3 * name + 0.7 * structure
    } else {
        name
    };
    (score * 100.0).round() / 100.0
}

fn object_type_similarity(from: &str, old: &ObjectType, to: &str, new: &ObjectType) -> (f64, Vec<String>) {
    let name = name_similarity(&[from, &old.label], &[to, &new.label]);
    let mut evidence = vec![format!("name similarity {name:.2} ({from} vs {to})")];
    let mut signals = Vec::new();
    if let Some((j, shared, union)) = jaccard(&names_of(old.properties.keys()), &names_of(new.properties.keys())) {
        signals.push((0.4, j));
        evidence.push(format!("properties: {shared} of {union} names shared"));
    }
    if old.extends.is_some() || new.extends.is_some() || !old.implements.is_empty() || !new.implements.is_empty() {
        let same = old.extends == new.extends && names_of(&old.implements) == names_of(&new.implements);
        signals.push((0.15, if same { 1.0 } else { 0.0 }));
        evidence.push(if same { "same extends and implements" } else { "different extends or implements" }.into());
    }
    if let Some((j, shared, union)) = jaccard(&names_of(&old.states), &names_of(&new.states)) {
        signals.push((0.15, j));
        evidence.push(format!("states: {shared} of {union} shared"));
    }
    (rename_score(name, &signals), evidence)
}

fn link_type_similarity(from: &str, old: &LinkType, to: &str, new: &LinkType) -> (f64, Vec<String>) {
    let name = name_similarity(&[from, &old.label], &[to, &new.label]);
    let mut evidence = vec![format!("name similarity {name:.2} ({from} vs {to})")];
    // Empty endpoint lists mean "any kind": two empty lists agree.
    let side = |a: &[String], b: &[String]| jaccard(&names_of(a), &names_of(b)).map_or(1.0, |(j, _, _)| j);
    let (f, t) = (side(&old.from, &new.from), side(&old.to, &new.to));
    evidence.push(format!("endpoints: from {f:.2}, to {t:.2} alike"));
    let same_card = old.cardinality == new.cardinality;
    evidence.push(if same_card { "same cardinality" } else { "different cardinality" }.into());
    let flags = [old.dependency == new.dependency, old.transitive == new.transitive, old.symmetric == new.symmetric];
    let equal = flags.iter().filter(|x| **x).count();
    evidence.push(format!("dependency/transitive/symmetric: {equal} of 3 flags equal"));
    let signals = [(0.5, (f + t) / 2.0), (0.1, if same_card { 1.0 } else { 0.0 }), (0.1, equal as f64 / 3.0)];
    (coarse_score(name, &signals, &mut evidence), evidence)
}

/// [`rename_score`] for a section with coarse structure: the name alone
/// when it is below [`COARSE_NAME_FLOOR`].
fn coarse_score(name: f64, signals: &[(f64, f64)], evidence: &mut Vec<String>) -> f64 {
    if name < COARSE_NAME_FLOOR {
        evidence.push(format!(
            "names too far apart (below {COARSE_NAME_FLOOR:.2}) for the structure to count: the name alone scores it"
        ));
        rename_score(name, &[])
    } else {
        rename_score(name, signals)
    }
}

fn property_similarity(from: &str, old: &PropertyDef, to: &str, new: &PropertyDef) -> (f64, Vec<String>) {
    let name = name_similarity(&[from], &[to]);
    let mut evidence = vec![format!("name similarity {name:.2} ({from} vs {to})")];
    let same_type = old.ty == new.ty;
    evidence.push(if same_type {
        format!("same type ({})", old.expected())
    } else {
        format!("type {} vs {}", old.expected(), new.expected())
    });
    let mut signals = vec![(0.4, if same_type { 1.0 } else { 0.0 })];
    if let Some((j, shared, union)) = jaccard(&names_of(&old.values), &names_of(&new.values)) {
        signals.push((0.3, j));
        evidence.push(format!("values: {shared} of {union} shared"));
    }
    (coarse_score(name, &signals, &mut evidence), evidence)
}

/// Pairs at or above the threshold, assigned one-to-one, best score first.
fn assign_renames(mut candidates: Vec<LikelyRename>) -> Vec<LikelyRename> {
    candidates.retain(|c| c.score >= RENAME_THRESHOLD);
    candidates
        .sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.from.cmp(&b.from)).then_with(|| a.to.cmp(&b.to)));
    let mut used_from: BTreeSet<String> = BTreeSet::new();
    let mut used_to: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for c in candidates {
        if used_from.contains(&c.from) || used_to.contains(&c.to) {
            continue;
        }
        used_from.insert(c.from.clone());
        used_to.insert(c.to.clone());
        out.push(c);
    }
    out
}

/// Definitions the set deletes that look renamed to definitions it adds in
/// the same section: object types, link types, and the properties of one
/// object type. Explicit renames are not candidates.
fn likely_renames(base: &Schema, new: &Schema, set: &SchemaChangeSet) -> Vec<LikelyRename> {
    let mut explicit_from: BTreeSet<String> = BTreeSet::new();
    let mut explicit_to: BTreeSet<String> = BTreeSet::new();
    for c in &set.changes {
        let (section, from, to) = match c {
            SchemaChange::RenameObjectType { from, to } => ("object_types", from.clone(), to.clone()),
            SchemaChange::RenameLinkType { from, to } => ("link_types", from.clone(), to.clone()),
            SchemaChange::RenameProperty { object_type, from, to } => {
                ("properties", format!("{object_type}.{from}"), format!("{object_type}.{to}"))
            }
            _ => continue,
        };
        explicit_from.insert(format!("{section}/{from}"));
        explicit_to.insert(format!("{section}/{to}"));
    }
    let candidate = |section: &str, from: &str, to: &str| {
        !explicit_from.contains(&format!("{section}/{from}")) && !explicit_to.contains(&format!("{section}/{to}"))
    };
    let pair = |section: &str, from: String, to: String, (score, evidence): (f64, Vec<String>)| LikelyRename {
        section: section.into(),
        from,
        to,
        score,
        evidence,
    };
    let mut out = Vec::new();

    let mut cands = Vec::new();
    for (x, old) in base.object_types.iter().filter(|(k, _)| !new.object_types.contains_key(*k)) {
        for (y, def) in new.object_types.iter().filter(|(k, _)| !base.object_types.contains_key(*k)) {
            if candidate("object_types", x, y) {
                cands.push(pair("object_types", x.clone(), y.clone(), object_type_similarity(x, old, y, def)));
            }
        }
    }
    out.extend(assign_renames(cands));

    let mut cands = Vec::new();
    for (x, old) in base.link_types.iter().filter(|(k, _)| !new.link_types.contains_key(*k)) {
        for (y, def) in new.link_types.iter().filter(|(k, _)| !base.link_types.contains_key(*k)) {
            if candidate("link_types", x, y) {
                cands.push(pair("link_types", x.clone(), y.clone(), link_type_similarity(x, old, y, def)));
            }
        }
    }
    out.extend(assign_renames(cands));

    for (kind, old) in &base.object_types {
        let Some(cur) = new.object_types.get(kind) else { continue };
        let mut cands = Vec::new();
        for (x, xdef) in old.properties.iter().filter(|(k, _)| !cur.properties.contains_key(*k)) {
            for (y, ydef) in cur.properties.iter().filter(|(k, _)| !old.properties.contains_key(*k)) {
                let (from, to) = (format!("{kind}.{x}"), format!("{kind}.{y}"));
                if candidate("properties", &from, &to) {
                    cands.push(pair("properties", from, to, property_similarity(x, xdef, y, ydef)));
                }
            }
        }
        out.extend(assign_renames(cands));
    }
    out
}

/// `"<X> looks renamed to <Y> (score 0.82): use rename_object_type to keep its N entities"`.
fn rename_warning(base: &Schema, g: &Graph, r: &LikelyRename) -> String {
    let (op, n, what) = match r.section.as_str() {
        "object_types" => ("rename_object_type", g.of_kind(&r.from).count(), "entities"),
        "link_types" => ("rename_link_type", g.links().filter(|l| l.rel == r.from).count(), "links"),
        _ => {
            let (kind, prop) = r.from.split_once('.').unwrap_or((r.from.as_str(), ""));
            let kinds: Vec<&String> =
                g.kinds().map(|(k, _)| k).filter(|k| *k == kind || base.kind_is(k, kind)).collect();
            let n = kinds
                .iter()
                .map(|k| g.of_kind(k).filter(|e| e.props.get(prop).is_some_and(|v| !v.is_null())).count())
                .sum();
            ("rename_property", n, "values")
        }
    };
    format!("{} looks renamed to {} (score {:.2}): use {op} to keep its {n} {what}", r.from, r.to, r.score)
}

// ── consequences ─────────────────────────────────────────────────────────

/// The base signature a change is judged against, as spelled terms.
struct Signature {
    entities: HashSet<String>,
    classes: HashSet<String>,
    relations: HashSet<String>,
    props: HashSet<String>,
}

impl Signature {
    /// The names of `base` plus every entity of `before` (keywords and
    /// literals are always in).
    fn of(base: &Schema, before: &Closure) -> Self {
        let vocab = reason::vocabulary(base);
        let classes = base
            .object_types
            .keys()
            .chain(base.interfaces.keys())
            .chain(vocab.classes.iter())
            .map(|n| format!("#{n}"))
            .collect();
        let relations = base.link_types.keys().chain(vocab.relations.iter()).cloned().collect();
        let props = base
            .object_types
            .values()
            .flat_map(|t| t.properties.keys())
            .chain(base.interfaces.values().flat_map(|i| i.properties.keys()))
            .cloned()
            .collect();
        let mut entities = HashSet::new();
        for f in before.asserted() {
            for t in [&f.s, &f.o] {
                if t.starts_with('<') {
                    entities.insert(t.clone());
                }
            }
        }
        Self { entities, classes, relations, props }
    }

    fn term(&self, t: &str) -> bool {
        if t.starts_with('<') {
            self.entities.contains(t)
        } else if t.starts_with('#') {
            self.classes.contains(t)
        } else if let Some(p) = t.strip_prefix("prop.") {
            self.props.contains(p)
        } else {
            reason::KEYWORDS.contains(&t) || self.relations.contains(t)
        }
    }

    /// Are all terms of `f` in the signature? The object of a `state` or
    /// `prop.*` fact is a literal, and every literal is.
    fn covers(&self, f: &Fact) -> bool {
        let literal = f.p == "state" || f.p.starts_with("prop.");
        self.term(&f.s) && self.term(&f.p) && (literal || self.term(&f.o))
    }
}

/// The consequences of going from `before` to `after`, judged against the
/// vocabulary of `base`. With `same_data` (both closures read the same graph)
/// asserted `state` and `prop.*` facts are left out: they differ only by
/// what the rules of each schema read, not by what the store holds.
pub fn closure_consequences(
    base: &Schema,
    before: &Closure,
    after: &Closure,
    same_data: bool,
    means: &str,
) -> Consequences {
    let diff = before.diff(after);
    let data_read = |f: &Fact| same_data && (f.p == "state" || f.p.starts_with("prop."));
    let added: Vec<AddedFact> = diff.added.into_iter().filter(|a| !(a.rule.is_none() && data_read(&a.fact))).collect();
    let removed: Vec<Fact> = diff
        .removed
        .into_iter()
        .filter(|f| !(data_read(f) && before.contains(f) == Some(reason::Status::Asserted)))
        .collect();
    let sig = Signature::of(base, before);
    let mut by_rule: BTreeMap<String, usize> = BTreeMap::new();
    let mut witnesses = Vec::new();
    let mut over = 0;
    for a in &added {
        *by_rule.entry(a.rule.clone().unwrap_or_else(|| "asserted".into())).or_default() += 1;
        if sig.covers(&a.fact) {
            over += 1;
            if witnesses.len() < CONSEQUENCE_SAMPLES {
                witnesses.push(a.clone());
            }
        }
    }
    // A refused term drops facts from a closure: what it would add is not
    // seen, so conservativity is not shown.
    let refused = before.stats().refused + after.stats().refused;
    let verdict = match (diff.fixpoint_reached && refused == 0, over) {
        (false, _) => Conservativity::Undetermined,
        (true, 0) => Conservativity::Conservative,
        (true, _) => Conservativity::NotConservative,
    };
    Consequences {
        added: added.len(),
        removed: removed.len(),
        conservative: verdict == Conservativity::Conservative,
        verdict,
        new_over_existing_vocabulary: over,
        by_rule,
        samples_added: added.into_iter().take(CONSEQUENCE_SAMPLES).collect(),
        witnesses,
        samples_removed: removed.into_iter().take(CONSEQUENCE_SAMPLES).collect(),
        fixpoint_reached: diff.fixpoint_reached,
        refused,
        skipped: None,
        means: means.to_string(),
    }
}

/// A lower bound of the asserted facts a closure of `g` would hold: one per
/// visible entity and one per link between visible entities. Stops counting
/// past [`MAX_CONSEQUENCE_FACTS`].
fn asserted_floor(g: &Graph, can_read: &dyn Fn(&Entity) -> bool) -> usize {
    let visible: HashSet<&str> = g.entities().filter(|e| can_read(e)).map(|e| e.id.as_str()).collect();
    let mut n = visible.len();
    for l in g.links() {
        if n > MAX_CONSEQUENCE_FACTS {
            break;
        }
        if visible.contains(l.from.as_str()) && visible.contains(l.to.as_str()) {
            n += 1;
        }
    }
    n
}

fn too_large(n: usize) -> String {
    format!(
        "the graph holds at least {n} asserted facts in scope, more than {MAX_CONSEQUENCE_FACTS}: consequences are \
         not computed (conservativity is not shown)"
    )
}

/// The consequences of replacing schema `base` with `new` over graph `g`, as
/// `can_read` sees it.
pub fn schema_consequences(base: &Schema, new: &Schema, g: &Graph, can_read: &dyn Fn(&Entity) -> bool) -> Consequences {
    prepare_schema_consequences(base, new, g, can_read, None).finish(base).0
}

/// The closure before a schema change: one already computed, or the facts
/// to compute it from.
pub(crate) enum Before {
    Cached(Arc<Closure>),
    Pending(reason::Prepared),
}

/// The consequences of a schema change with the graph already read: the
/// rules run in [`ConsequenceJob::finish`], which needs no read guard.
pub(crate) enum ConsequenceJob {
    Done(Consequences),
    Run { before: Before, after: reason::Prepared },
}

/// Collect what the two closures of [`schema_consequences`] read (under the
/// caller's read guard); `cached` is the closure of `base` for the same
/// reader and graph, when one is at hand.
pub(crate) fn prepare_schema_consequences(
    base: &Schema,
    new: &Schema,
    g: &Graph,
    can_read: &dyn Fn(&Entity) -> bool,
    cached: Option<Arc<Closure>>,
) -> ConsequenceJob {
    let floor = asserted_floor(g, can_read);
    if floor > MAX_CONSEQUENCE_FACTS {
        return ConsequenceJob::Done(Consequences::skipped(too_large(floor), SCHEMA_CONSEQUENCES_MEANS));
    }
    let opts = ReasonOptions::default();
    let before = match cached {
        Some(c) => Before::Cached(c),
        None => Before::Pending(reason::prepare(base, g, &opts, can_read)),
    };
    let asserted = match &before {
        Before::Cached(c) => c.asserted().len(),
        Before::Pending(p) => p.asserted(),
    };
    if asserted > MAX_CONSEQUENCE_FACTS {
        return ConsequenceJob::Done(Consequences::skipped(too_large(asserted), SCHEMA_CONSEQUENCES_MEANS));
    }
    ConsequenceJob::Run { before, after: reason::prepare(new, g, &opts, can_read) }
}

impl ConsequenceJob {
    /// Run the closures and compare them; also returns the `before` closure
    /// when this job computed it (for the caller to cache).
    pub(crate) fn finish(self, base: &Schema) -> (Consequences, Option<Arc<Closure>>) {
        match self {
            ConsequenceJob::Done(c) => (c, None),
            ConsequenceJob::Run { before, after } => {
                let (before, fresh) = match before {
                    Before::Cached(c) => (c, false),
                    Before::Pending(p) => (Arc::new(p.run()), true),
                };
                let after = after.run();
                let c = closure_consequences(base, &before, &after, true, SCHEMA_CONSEQUENCES_MEANS);
                (c, fresh.then_some(before))
            }
        }
    }
}

/// The consequences of a data edit under one schema: graph `before` as
/// `can_before` sees it against graph `after` as `can_after` sees it.
pub fn data_consequences(
    schema: &Schema,
    before: &Graph,
    after: &Graph,
    can_before: &dyn Fn(&Entity) -> bool,
    can_after: &dyn Fn(&Entity) -> bool,
) -> Consequences {
    let floor = asserted_floor(before, can_before).max(asserted_floor(after, can_after));
    if floor > MAX_CONSEQUENCE_FACTS {
        return Consequences::skipped(too_large(floor), ACTION_CONSEQUENCES_MEANS);
    }
    let opts = ReasonOptions::default();
    let b = reason::closure(schema, before, &opts, can_before);
    let a = reason::closure(schema, after, &opts, can_after);
    closure_consequences(schema, &b, &a, false, ACTION_CONSEQUENCES_MEANS)
}

// ── risk ─────────────────────────────────────────────────────────────────

/// The risk word of a plan and its reasons, most severe first.
///
/// * `high`: the change is breaking, a breaking effect has no migration, or
///   a lock is violated without override.
/// * `medium`: a change deletes or renames a definition, a lock is
///   overridden, a fact the current schema entails disappears, or the change
///   is not shown to be conservative (not conservative, skipped, or a run
///   stopped before its fixpoint).
/// * `low`: purely additive and conservative.
///
/// A plan that does not validate is `high`: it cannot be applied, and what
/// it would do is not shown.
pub fn assess_risk(plan: &SchemaPlan, set: &SchemaChangeSet) -> (Risk, Vec<String>) {
    let mut high = Vec::new();
    let mut medium = Vec::new();
    if !plan.valid {
        let n = plan.errors.len();
        high.push(format!(
            "the change set does not validate ({n} {}): it cannot be applied, and its effects are not shown",
            if n == 1 { "error" } else { "errors" }
        ));
    }
    if plan.breaking {
        for imp in plan.changes.iter().filter(|i| i.breaking) {
            high.push(format!("breaking: {} {} ({})", imp.op, imp.target, imp.reasons.join("; ")));
        }
        if high.is_empty() {
            high.push("breaking: the change invalidates data or vocabulary in use".into());
        }
    }
    if !plan.needs_migration.is_empty() {
        high.push(format!("{} breaking effects have no migration", plan.needs_migration.len()));
    }
    for v in &plan.locked_violations {
        let line = format!("{}/{} ({}, locked by {})", v.section, v.name, v.change, v.locked_by);
        if v.overridden {
            medium.push(format!("overrides the lock on {line}"));
        } else {
            high.push(format!("touches locked {line} without override"));
        }
    }
    for c in &set.changes {
        let op = c.op();
        if op.starts_with("delete_") || op.starts_with("rename_") {
            medium.push(format!("removes or moves a definition: {op} {}", c.target()));
        }
    }
    if let Some(c) = &plan.consequences {
        if c.removed > 0 {
            medium.push(format!("{} facts the closure holds now disappear from it", c.removed));
        }
        match c.verdict {
            Conservativity::Conservative => {}
            Conservativity::NotConservative => {
                let example = c.witnesses.first().map(|w| format!(", e.g. {}", w.fact)).unwrap_or_default();
                medium.push(format!(
                    "not conservative: {} added facts are stated purely in the current vocabulary{example}",
                    c.new_over_existing_vocabulary
                ));
            }
            Conservativity::Undetermined => medium.push(format!(
                "conservativity not shown: {}",
                match (&c.skipped, c.refused) {
                    (Some(why), _) => why.clone(),
                    (None, n) if n > 0 && c.fixpoint_reached => format!(
                        "{n} term(s) could not be spelled, so the facts naming them are left out of the closures"
                    ),
                    (None, _) => "a reasoning run stopped before its fixpoint".to_string(),
                }
            )),
        }
    } else {
        medium.push("conservativity not shown: the plan holds no consequences (it was planned without them)".into());
    }
    let risk = if !high.is_empty() {
        Risk::High
    } else if !medium.is_empty() {
        Risk::Medium
    } else {
        Risk::Low
    };
    high.extend(medium);
    (risk, high)
}

/// May a proposal's `override_locks` be honoured, as far as its proposer
/// goes? Only for an admin that is not an agent (the approver must be an
/// admin too).
pub(crate) fn proposer_may_override(r: &Proposer) -> bool {
    !r.agent && r.roles.iter().any(|x| matches!(x, Role::Admin | Role::System))
}

/// Fill a plan's lock violations and risk. The override is honoured only
/// when `may_override` (an admin) and the set asks for it.
pub(crate) fn assess(
    plan: &mut SchemaPlan,
    base: &Schema,
    set: &SchemaChangeSet,
    held: &[SchemaLock],
    may_override: bool,
) {
    plan.locked_violations = locks::violations(base, set, &plan.changes, held, set.override_locks && may_override);
    if set.override_locks && !may_override && !plan.locked_violations.is_empty() {
        plan.warnings.push(
            "override_locks is ignored: only admins may override schema locks (for a proposal, both its proposer \
             and its approver)"
                .into(),
        );
    }
    let (risk, reasons) = assess_risk(plan, set);
    plan.risk = risk;
    plan.risk_reasons = reasons;
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

    /// Dry run: validate `set`, report its impact, the migration it implies,
    /// its consequences for entailment (as `p` may see the graph), the locks
    /// it touches, likely renames and its risk.
    pub fn schema_plan(&self, p: &Principal, set: &SchemaChangeSet) -> Result<SchemaPlan> {
        self.schema_plan_as(p, set, None)
    }

    /// [`Self::schema_plan`] for a proposal's change set: its override is
    /// honoured as the approval's commit honours it, only when the proposer
    /// is also an admin (and not an agent).
    pub(crate) fn schema_plan_as(
        &self,
        p: &Principal,
        set: &SchemaChangeSet,
        proposer: Option<&Proposer>,
    ) -> Result<SchemaPlan> {
        self.schema_reader(p)?;
        let held = locks::read_locks(&**self.store())?;
        let active = self.active_schema();
        let g = self.inner_graph_read();
        let revision = self.revision();
        let mut planned = plan(&active.schema, active.version, &g, set, self.options().strict);
        self.check_migration(&g, &mut planned, p);
        let can = |e: &Entity| policy::visible(p, e, &g);
        let job = match &planned.schema {
            Some(new) => {
                let cached = self.cached_closure(p, &ReasonOptions::default(), revision, active.version);
                prepare_schema_consequences(&active.schema, new, &g, &can, cached)
            }
            None => ConsequenceJob::Done(Consequences::skipped(
                "the change set does not compile",
                SCHEMA_CONSEQUENCES_MEANS,
            )),
        };
        planned.plan.shapes = Some(match &planned.schema {
            Some(new) => plan_shapes(&active.schema, new, &g, &can),
            None => PlanShapes::skipped("the change set does not compile"),
        });
        // The facts are collected; the rules run without the read guard, so
        // a plan does not hold writers (and readers queued behind them) back.
        drop(g);
        let (consequences, before) = job.finish(&active.schema);
        if let Some(before) = before {
            self.keep_closure(p, &ReasonOptions::default(), revision, active.version, before);
        }
        planned.plan.consequences = Some(consequences);
        let may_override = locks::can_manage(p) && proposer.is_none_or(proposer_may_override);
        assess(&mut planned.plan, &active.schema, set, &held, may_override);
        Ok(planned.plan)
    }

    /// The consequences of `set` against the active schema and graph now,
    /// with the schema version they were computed against (a commit computes
    /// them before it takes the write lock).
    fn consequences_now(&self, p: &Principal, set: &SchemaChangeSet) -> Option<(u64, Consequences)> {
        let active = self.active_schema();
        let new = apply_changes(&active.schema, &set.changes).ok()?;
        let g = self.inner_graph_read();
        let revision = self.revision();
        let cached = self.cached_closure(p, &ReasonOptions::default(), revision, active.version);
        let job = {
            let can = |e: &Entity| policy::visible(p, e, &g);
            prepare_schema_consequences(&active.schema, &new, &g, &can, cached)
        };
        drop(g);
        Some((active.version, job.finish(&active.schema).0))
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
        // A proposal's override is the proposer's request: it is honoured
        // only when both the proposer and the approver are admins, so an
        // agent cannot route an override through an approval.
        let proposer_may = match &origin.approve {
            Some((pid, _, _)) => self.load_proposal(pid).is_ok_and(|r| proposer_may_override(&r.proposer)),
            None => true,
        };
        let may_override = locks::can_manage(&ctx.principal) && proposer_may;
        // Two closures can take a while: compute them before the write lock,
        // and keep them only if the schema did not move in between.
        let early = self.consequences_now(&ctx.principal, &set);
        let (res, (info, plan)) = self.transact(&mctx, |o, g| {
            let active = o.active_schema();
            let mut planned = plan(&active.schema, active.version, g, &set, o.options().strict);
            o.check_migration(g, &mut planned, &ctx.principal);
            planned.plan.consequences = match &early {
                Some((v, c)) if *v == active.version => Some(c.clone()),
                Some(_) => Some(Consequences::skipped(
                    "the schema moved while the change was being committed; plan it again to see its consequences",
                    SCHEMA_CONSEQUENCES_MEANS,
                )),
                None => None,
            };
            let held = locks::read_locks(&**o.store())?;
            assess(&mut planned.plan, &active.schema, &set, &held, may_override);
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
            let mut payload = json!({
                "version": version,
                "parent": active.version,
                "source": origin.source,
                "proposal": origin.proposal,
                "message": set.message,
                "changes": set.changes.iter().map(|c| format!("{} {}", c.op(), c.target())).collect::<Vec<_>>(),
                "diff": sv.diff.summary(),
                "breaking": sv.breaking,
                "migration": sv.migration,
                "risk": planned.plan.risk,
            });
            // Overridden locks are recorded with the change, and locks follow
            // the definitions they name: released on delete, moved on rename.
            let overridden: Vec<&LockViolation> =
                planned.plan.locked_violations.iter().filter(|v| v.overridden).collect();
            if !overridden.is_empty() {
                let followed =
                    locks::follow(&planned.plan.locked_violations, &held, &schema, &ctx.principal.id, &fmt_ts(now));
                payload["override_locks"] = json!({
                    "by": ctx.principal.id,
                    "violations": overridden,
                    "released": followed.released,
                    "moved": followed.moved,
                });
                kv.extend(followed.kv);
            }
            // Renames reach outbound configuration in the same commit:
            // webhook endpoints' change filters follow renamed kinds and
            // relations (their version moves, so automations that reach them
            // are enabled again by a person).
            let renames = renames_of(&set.changes);
            let rewritten = rewrite_endpoint_filters(&**o.store(), &renames)?;
            if !rewritten.is_empty() {
                payload["webhook_filters"] = json!(rewritten.iter().map(|(_, k, _)| k).collect::<Vec<_>>());
            }
            kv.extend(rewritten);
            let event = o.event(ctx, &ctx.workspace, "schema.change", None, now, payload, Vec::new());
            let mut events = origin.events.clone();
            events.push(event);
            let extra = Extra {
                kv,
                redirects: planned.redirects.clone(),
                events,
                schema: Some((Arc::new(schema), version)),
                reindex: planned.reindex.clone(),
                renames,
                ..Default::default()
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
            override_locks: false,
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
            override_locks: false,
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

/// The kind and relation renames of a change set, for its change record.
fn renames_of(changes: &[SchemaChange]) -> Vec<crate::changes::Rename> {
    use crate::changes::{Rename, RenameWhat};
    changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::RenameObjectType { from, to } => {
                Some(Rename { what: RenameWhat::Kind, from: from.clone(), to: to.clone() })
            }
            SchemaChange::RenameLinkType { from, to } => {
                Some(Rename { what: RenameWhat::Rel, from: from.clone(), to: to.clone() })
            }
            _ => None,
        })
        .collect()
}

/// The `webhook.endpoint` values whose change filter names a renamed kind
/// (`filter.kinds`) or relation (`filter.rels`, also as `^rel`), rewritten to
/// the new names with their `version` bumped, as KV writes for the schema
/// commit. Endpoint values are read as JSON: onto-core does not own their
/// type, only the filter it defines ([`crate::changes::ChangeFilter`]).
pub(crate) fn rewrite_endpoint_filters(
    store: &dyn crate::store::Store,
    renames: &[crate::changes::Rename],
) -> Result<Vec<(String, String, Value)>> {
    use crate::changes::RenameWhat;
    if renames.is_empty() {
        return Ok(Vec::new());
    }
    let ns = crate::outbox::NS_WEBHOOK_ENDPOINT;
    let mut out = Vec::new();
    for (name, mut v) in store.kv_scan(ns, "")? {
        let Some(filter) = v.get_mut("filter").and_then(Value::as_object_mut) else { continue };
        let mut touched = false;
        for r in renames {
            let (key, inverse) = match r.what {
                RenameWhat::Kind => ("kinds", false),
                RenameWhat::Rel => ("rels", true),
            };
            let Some(list) = filter.get_mut(key).and_then(Value::as_array_mut) else { continue };
            for item in list.iter_mut() {
                let Some(text) = item.as_str() else { continue };
                let new = if text == r.from {
                    Some(r.to.clone())
                } else if inverse && text.strip_prefix('^') == Some(r.from.as_str()) {
                    Some(format!("^{}", r.to))
                } else {
                    None
                };
                if let Some(n) = new {
                    *item = Value::String(n);
                    touched = true;
                }
            }
        }
        if touched {
            let version = v.get("version").and_then(Value::as_u64).unwrap_or(0) + 1;
            v["version"] = json!(version);
            out.push((ns.to_string(), name, v));
        }
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaro_winkler_matches_reference_values() {
        let close = |a: &str, b: &str, want: f64| {
            let got = jaro_winkler(a, b);
            assert!((got - want).abs() < 0.005, "{a} vs {b}: {got} (want {want})");
        };
        close("martha", "marhta", 0.961);
        close("dixon", "dicksonx", 0.813);
        close("dwayne", "duane", 0.840);
        assert_eq!(jaro_winkler("same", "same"), 1.0);
        assert_eq!(jaro_winkler("abc", ""), 0.0);
        assert_eq!(jaro_winkler("abc", "xyz"), 0.0);
    }

    #[test]
    fn rename_scores_renormalize_undefined_signals() {
        assert_eq!(rename_score(0.5, &[]), 0.5, "no structure: the name alone");
        assert_eq!(rename_score(0.0, &[(0.4, 1.0)]), 0.7);
        assert_eq!(rename_score(1.0, &[(0.4, 1.0), (0.15, 0.0)]), 0.81);
        let a = ObjectType { properties: BTreeMap::from([("x".into(), PropertyDef::default())]), ..Default::default() };
        let (score, evidence) = object_type_similarity("bug", &a, "defect", &a.clone());
        assert!(score >= RENAME_THRESHOLD, "{score} {evidence:?}");
        assert!(evidence.iter().any(|e| e == "properties: 1 of 1 names shared"));
        let b = ObjectType { properties: BTreeMap::from([("y".into(), PropertyDef::default())]), ..Default::default() };
        assert!(object_type_similarity("bug", &a, "invoice", &b).0 < RENAME_THRESHOLD);
    }

    #[test]
    fn renames_are_assigned_one_to_one_best_first() {
        let r = |from: &str, to: &str, score: f64| LikelyRename {
            section: "object_types".into(),
            from: from.into(),
            to: to.into(),
            score,
            evidence: Vec::new(),
        };
        let out = assign_renames(vec![r("a", "x", 0.7), r("a", "y", 0.9), r("b", "y", 0.8), r("c", "z", 0.59)]);
        let pairs: Vec<(&str, &str)> = out.iter().map(|r| (r.from.as_str(), r.to.as_str())).collect();
        assert_eq!(pairs, [("a", "y")], "b loses y to a; a takes y, not x; c is below the threshold");
    }

    #[test]
    fn signature_covers_existing_names_keywords_and_literals() {
        let schema = Schema::enterprise();
        let before = reason::closure(&schema, &Graph::default(), &ReasonOptions::default(), &|_| true);
        let mut sig = Signature::of(&schema, &before);
        sig.entities.insert("<issue:1>".into());
        assert!(sig.covers(&Fact::new("<issue:1>", "depends_on", "<issue:1>")));
        assert!(sig.covers(&Fact::new("depends_on", "a", "transitive")));
        assert!(sig.covers(&Fact::new("<issue:1>", "prop.priority", "\"anything\"")));
        assert!(sig.covers(&Fact::new("<issue:1>", "state", "\"new\"")));
        assert!(sig.covers(&Fact::new("<issue:1>", "handled_by_team", "<issue:1>")), "rule-derived relation");
        assert!(!sig.covers(&Fact::new("<issue:1>", "a", "#brand_new")));
        assert!(!sig.covers(&Fact::new("<issue:2>", "depends_on", "<issue:1>")), "unknown entity");
        assert!(!sig.covers(&Fact::new("<issue:1>", "prop.unheard_of", "1")));
    }

    #[test]
    fn link_flag_edits_leave_links_alone() {
        let old = LinkType { from: vec!["a".into(), "b".into()], ..Default::default() };
        let flags = LinkType { from: vec!["b".into(), "a".into()], transitive: true, weight: 0.2, ..old.clone() };
        assert!(!link_data_changes(&old, &flags), "order of endpoints and flags do not matter");
        let narrowed = LinkType { from: vec!["a".into()], ..old.clone() };
        assert!(link_data_changes(&old, &narrowed));
        let single = LinkType { cardinality: Cardinality::One, ..old.clone() };
        assert!(link_data_changes(&old, &single));
    }
}
