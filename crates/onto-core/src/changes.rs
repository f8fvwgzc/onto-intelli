//! The change feed: one record per changed entity, link or schema version,
//! computed in the commit that made the change (docs/design/EVENTS.md, Part A).
//!
//! [`capture`] runs inside `commit_locked`, where both the graph before the
//! commit and the staged batch are at hand, so every write path that goes
//! through the engine is recorded. A state-changing commit that was not
//! captured (an older build, a bulk import, a writer around the engine)
//! becomes a [`ChangeOp::Gap`] record written by the store: the feed says
//! that something changed without records instead of staying silent.
//!
//! Records are filtered per reader: [`matches`] applies a [`ChangeFilter`],
//! [`view`] applies visibility (old values only for readers who could see
//! the entity both before and after the change; `entity.hidden` when it
//! left their sight), and [`redact`] trims payloads to ids.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};

use crate::engine::{ActionContext, Ontology, diff_fields};
use crate::graph::{Graph, LinkKey};
use crate::model::{Entity, Id, Link};
use crate::policy::{self, Access, Principal};
use crate::schema::Schema;
use crate::store::WriteBatch;
use crate::{Error, Result};

/// Most entity ids one filter may name.
pub const MAX_FILTER_IDS: usize = 1000;
/// Most kinds, workspaces, relations or properties one filter may name (each).
pub const MAX_FILTER_NAMES: usize = 100;
/// Most `ops` one filter may name.
pub const MAX_FILTER_OPS: usize = 16;
/// Most source globs one filter may hold.
pub const MAX_FILTER_SOURCES: usize = 20;
/// Longest kind, workspace, relation, property or source glob, in bytes.
pub const MAX_FILTER_NAME_LEN: usize = 256;
/// Most records scanned per page.
pub const MAX_PAGE: usize = 1000;

/// Position in a tenant's change feed: `"<seq>.<n>"`, the commit sequence and
/// the record's place in that commit. Totally ordered per tenant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cursor {
    pub seq: u64,
    pub n: u32,
}

impl Cursor {
    pub fn new(seq: u64, n: u32) -> Self {
        Self { seq, n }
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.seq, self.n)
    }
}

impl FromStr for Cursor {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let bad = || Error::invalid(format!("cursor '{s}' must look like <seq>.<n>, for example 42.0"));
        let (seq, n) = s.trim().split_once('.').ok_or_else(bad)?;
        Ok(Self { seq: seq.parse().map_err(|_| bad())?, n: n.parse().map_err(|_| bad())? })
    }
}

impl Serialize for Cursor {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Cursor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// What a record says happened. The names reuse the audit vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeOp {
    #[serde(rename = "entity.create")]
    EntityCreate,
    #[serde(rename = "entity.update")]
    EntityUpdate,
    #[serde(rename = "entity.delete")]
    EntityDelete,
    /// The entity left this reader's sight. Computed per reader, never stored.
    #[serde(rename = "entity.hidden")]
    EntityHidden,
    #[serde(rename = "link.create")]
    LinkCreate,
    #[serde(rename = "link.update")]
    LinkUpdate,
    /// Includes links removed with a deleted end.
    #[serde(rename = "link.delete")]
    LinkDelete,
    #[serde(rename = "schema.change")]
    SchemaChange,
    /// State changed in this commit without change records.
    #[serde(rename = "gap")]
    Gap,
}

impl ChangeOp {
    pub const ALL: [ChangeOp; 9] = [
        ChangeOp::EntityCreate,
        ChangeOp::EntityUpdate,
        ChangeOp::EntityDelete,
        ChangeOp::EntityHidden,
        ChangeOp::LinkCreate,
        ChangeOp::LinkUpdate,
        ChangeOp::LinkDelete,
        ChangeOp::SchemaChange,
        ChangeOp::Gap,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ChangeOp::EntityCreate => "entity.create",
            ChangeOp::EntityUpdate => "entity.update",
            ChangeOp::EntityDelete => "entity.delete",
            ChangeOp::EntityHidden => "entity.hidden",
            ChangeOp::LinkCreate => "link.create",
            ChangeOp::LinkUpdate => "link.update",
            ChangeOp::LinkDelete => "link.delete",
            ChangeOp::SchemaChange => "schema.change",
            ChangeOp::Gap => "gap",
        }
    }

    pub fn is_entity(self) -> bool {
        matches!(
            self,
            ChangeOp::EntityCreate | ChangeOp::EntityUpdate | ChangeOp::EntityDelete | ChangeOp::EntityHidden
        )
    }

    pub fn is_link(self) -> bool {
        matches!(self, ChangeOp::LinkCreate | ChangeOp::LinkUpdate | ChangeOp::LinkDelete)
    }
}

impl fmt::Display for ChangeOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ChangeOp {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        ChangeOp::ALL.into_iter().find(|op| op.as_str() == s).ok_or_else(|| {
            let names: Vec<&str> = ChangeOp::ALL.iter().map(|o| o.as_str()).collect();
            Error::invalid(format!("unknown change op '{s}' (one of {})", names.join(", ")))
        })
    }
}

/// `{from, to}` of the state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StateMove {
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FromTo {
    pub from: String,
    pub to: String,
}

/// The entity moved to another workspace or changed kind (migrations).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Moved {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<FromTo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<FromTo>,
}

/// What [`policy::visible`] needs of an entity: id, kind, workspace,
/// `visibility` and `acl`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VisStub {
    pub id: Id,
    pub kind: String,
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acl: Vec<String>,
}

impl VisStub {
    pub fn of(e: &Entity) -> Self {
        Self {
            id: e.id.clone(),
            kind: e.kind.clone(),
            workspace: e.workspace.clone(),
            visibility: e.prop_str("visibility").map(str::to_string),
            acl: e
                .props
                .get("acl")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
        }
    }

    /// An entity carrying only the fields visibility is judged on.
    pub fn as_entity(&self) -> Entity {
        let mut props = Map::new();
        if let Some(v) = &self.visibility {
            props.insert("visibility".into(), json!(v));
        }
        if !self.acl.is_empty() {
            props.insert("acl".into(), json!(self.acl));
        }
        Entity {
            id: self.id.clone(),
            kind: self.kind.clone(),
            workspace: self.workspace.clone(),
            title: String::new(),
            state: None,
            props,
            aliases: Vec::new(),
            provenance: None,
            created_at: 0,
            updated_at: 0,
            revision: 0,
        }
    }
}

/// A link record's link (stored canonically) and its two ends.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkChange {
    pub from: Id,
    pub rel: String,
    pub to: Id,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub props: Map<String, Value>,
    /// `[from, to]` as the change left them (before, for deletes).
    pub ends: [VisStub; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenameWhat {
    Kind,
    Rel,
}

/// A rename a schema change made, so mirrors can rewrite what they hold.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rename {
    pub what: RenameWhat,
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SchemaMove {
    pub version: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub renames: Vec<Rename>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapReason {
    /// A writer that does not record changes (an older build, a path around the engine).
    UncapturedWriter,
    BulkImport,
    /// Capture was turned on again after a period off.
    CaptureEnabled,
}

/// State changed in one commit without change records. Re-read with
/// `entity.find`; the result is not pinned to a cursor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gap {
    pub reason: GapReason,
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub entities: Vec<Id>,
    #[serde(default)]
    pub links: Vec<LinkKey>,
    /// Ids and links this reader may not see (they are left out above).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub hidden: u64,
    /// The commit touched more than [`MAX_GAP_IDS`] entities and links: the
    /// lists hold only the first ones, `total` counts them all.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Entities and links the commit touched (set when `truncated`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub total: u64,
}

/// Most entity ids and link keys one gap record lists; the rest are counted.
pub const MAX_GAP_IDS: usize = 1000;

impl Gap {
    /// A gap listing at most [`MAX_GAP_IDS`] entities and links (entities
    /// first), with `truncated` and `total` when there were more.
    pub fn listing(reason: GapReason, origin: String, mut entities: Vec<Id>, mut links: Vec<LinkKey>) -> Self {
        let total = entities.len() + links.len();
        let truncated = total > MAX_GAP_IDS;
        if truncated {
            entities.truncate(MAX_GAP_IDS);
            links.truncate(MAX_GAP_IDS - entities.len());
        }
        Gap { reason, origin, entities, links, hidden: 0, truncated, total: if truncated { total as u64 } else { 0 } }
    }
}

/// Automation run that caused a write (Part C), carried on the [`ActionContext`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunCause {
    pub automation: String,
    pub run: String,
    #[serde(default)]
    pub depth: u8,
}

/// Why the change happened, copied from the [`ActionContext`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cause {
    #[serde(default)]
    pub actor: String,
    #[serde(default)]
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub depth: u8,
    /// Written by a schema migration (`intent = "schema.migration"`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub migration: bool,
}

/// The intent schema migrations write with (`live_schema::migration_ctx`).
pub const MIGRATION_INTENT: &str = "schema.migration";

impl Cause {
    pub fn of(ctx: &ActionContext) -> Self {
        let run = ctx.cause.as_ref();
        Self {
            actor: ctx.principal.id.clone(),
            request_id: ctx.request_id.clone(),
            source: ctx.source.clone(),
            intent: ctx.intent.clone(),
            automation: run.map(|r| r.automation.clone()),
            run: run.map(|r| r.run.clone()),
            depth: run.map(|r| r.depth).unwrap_or(0),
            migration: ctx.intent.as_deref() == Some(MIGRATION_INTENT),
        }
    }
}

/// One change as a reader gets it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub cursor: Cursor,
    pub ts: i64,
    pub op: ChangeOp,
    /// After the change (before, for deletes and `entity.hidden`); empty for
    /// `schema.change` and `gap`.
    #[serde(default)]
    pub workspace: String,
    /// Entity kind, canonical relation, `"schema"` or `""`.
    #[serde(default)]
    pub kind: String,
    /// Entity id, `"from|rel|to"`, schema version or `""`.
    #[serde(default)]
    pub id: String,
    /// The entity's revision after the change (the deletion's, for deletes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// Changed fields in capture order: title, state, props…, aliases,
    /// provenance, workspace, kind (link updates: the link's property keys).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<StateMove>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved: Option<Moved>,
    /// Old values of the changed fields. Served only when the reader may see
    /// the entity both before and after the change.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub before: Map<String, Value>,
    /// The entity as the change left it (create, update). `None` for deletes,
    /// for `fields = "ids"`, and when the stored version is gone
    /// (`entity_missing`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<Entity>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub entity_missing: bool,
    /// This reader could not see the entity before this change (mirrors upsert it).
    #[serde(default, skip_serializing_if = "is_false")]
    pub became_visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaMove>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<Gap>,
    #[serde(default)]
    pub cause: Cause,
}

impl ChangeRecord {
    fn blank(op: ChangeOp, ts: i64, cause: Cause) -> Self {
        Self {
            cursor: Cursor::default(),
            ts,
            op,
            workspace: String::new(),
            kind: String::new(),
            id: String::new(),
            revision: None,
            changed: Vec::new(),
            state: None,
            moved: None,
            before: Map::new(),
            entity: None,
            entity_missing: false,
            became_visible: false,
            link: None,
            schema: None,
            gap: None,
            cause,
        }
    }

    /// A `gap` record for a state-changing commit written without records.
    pub fn gap(seq: u64, ts: i64, gap: Gap) -> Self {
        let mut r = Self::blank(ChangeOp::Gap, ts, Cause::default());
        r.cursor = Cursor::new(seq, 0);
        r.gap = Some(gap);
        r
    }
}

/// What a store keeps per record: the record without the entity document
/// (joined from the entity's versions when read), plus the visibility
/// fields of the entity before the change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredChange {
    #[serde(flatten)]
    pub record: ChangeRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_vis: Option<VisStub>,
    /// Revision of the entity version this record joins (create, update).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev_after: Option<u64>,
}

impl StoredChange {
    /// The record with the entity the change left (`entity_missing` when a
    /// create or update has none).
    pub fn joined(&self, entity: Option<Entity>) -> ChangeRecord {
        let mut r = self.record.clone();
        r.entity_missing = entity.is_none() && matches!(r.op, ChangeOp::EntityCreate | ChangeOp::EntityUpdate);
        r.entity = entity;
        r
    }
}

/// Rows a store returns after a cursor: each stored record with the entity
/// document it joins (create and update), the head and the floor.
#[derive(Clone, Debug, Default)]
pub struct StoredPage {
    pub changes: Vec<(StoredChange, Option<Entity>)>,
    pub head: Cursor,
    pub floor: u64,
}

/// A tenant's change log settings and position.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesMeta {
    /// Commits through the engine record changes.
    pub capture: bool,
    /// A cursor whose `seq` is below the floor is expired (records were
    /// removed, or not recorded while capture was off).
    pub floor: u64,
    /// The newest position: the last record of the head commit, or `<head>.0`.
    pub head: Cursor,
}

/// How long a store keeps change records ([`crate::store::Store::vacuum_changes`]).
/// Retention removes whole commits and raises the floor in the same
/// transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRetention {
    /// Remove records older than this many milliseconds (`None`: no age limit).
    pub keep_ms: Option<i64>,
    /// Keep at most this many records (`None`: no count limit). Whole commits
    /// are removed, so slightly fewer may remain.
    pub keep_rows: Option<u64>,
}

impl ChangeRetention {
    /// The default age limit: 7 days.
    pub const DEFAULT_KEEP_MS: i64 = 7 * 24 * 3_600_000;
}

impl Default for ChangeRetention {
    fn default() -> Self {
        Self { keep_ms: Some(Self::DEFAULT_KEEP_MS), keep_rows: None }
    }
}

/// What one retention pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeVacuum {
    /// Records removed.
    pub deleted: u64,
    /// The floor after the pass (unchanged when nothing was removed).
    pub floor: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldMode {
    /// Records with entity documents and old values.
    #[default]
    Full,
    /// Ids, kinds, ops and changed field names only.
    Ids,
}

/// Which records a stream, poll, webhook endpoint or automation wants.
/// Dimensions combine with AND, values within one with OR; an empty list
/// does not narrow. `gap` records always match: a filter never hides a gap.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChangeFilter {
    /// Entity kinds or interfaces (a link matches through either end).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<ChangeOp>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<String>,
    /// Entity ids (a link matches through either end); at most [`MAX_FILTER_IDS`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<Id>,
    /// Relations (canonical, inverse or `^rel`); when set, only link records match.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rels: Vec<String>,
    /// Updates whose changed fields include any of these (other ops pass).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub props: Vec<String>,
    /// Globs on `cause.source`; `"!x"` excludes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Records written by schema migrations.
    pub include_migrations: bool,
    pub fields: FieldMode,
}

impl Default for ChangeFilter {
    fn default() -> Self {
        Self {
            kinds: Vec::new(),
            ops: Vec::new(),
            workspaces: Vec::new(),
            ids: Vec::new(),
            rels: Vec::new(),
            props: Vec::new(),
            sources: Vec::new(),
            include_migrations: true,
            fields: FieldMode::Full,
        }
    }
}

impl ChangeFilter {
    /// Refuse filters too large to evaluate cheaply: filtering runs for
    /// every record a reader scans, and readers need only a read role.
    pub fn check(&self) -> Result<()> {
        let too_many = |what: &str, n: usize, max: usize| {
            Error::invalid(format!("a change filter names at most {max} {what} ({n} given)"))
        };
        if self.ids.len() > MAX_FILTER_IDS {
            return Err(too_many("ids", self.ids.len(), MAX_FILTER_IDS));
        }
        if self.ops.len() > MAX_FILTER_OPS {
            return Err(too_many("ops", self.ops.len(), MAX_FILTER_OPS));
        }
        if self.sources.len() > MAX_FILTER_SOURCES {
            return Err(too_many("sources", self.sources.len(), MAX_FILTER_SOURCES));
        }
        for (what, list) in
            [("kinds", &self.kinds), ("workspaces", &self.workspaces), ("rels", &self.rels), ("props", &self.props)]
        {
            if list.len() > MAX_FILTER_NAMES {
                return Err(too_many(what, list.len(), MAX_FILTER_NAMES));
            }
        }
        for (what, list) in [
            ("kinds", &self.kinds),
            ("workspaces", &self.workspaces),
            ("rels", &self.rels),
            ("props", &self.props),
            ("sources", &self.sources),
        ] {
            if let Some(long) = list.iter().find(|v| v.len() > MAX_FILTER_NAME_LEN) {
                return Err(Error::invalid(format!(
                    "a change filter's {what} are at most {MAX_FILTER_NAME_LEN} bytes each ({} given)",
                    long.len()
                )));
            }
        }
        Ok(())
    }
}

/// One page of the change feed for one reader.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChangePage {
    pub changes: Vec<ChangeRecord>,
    /// Resume after this cursor: the last one scanned, so a reader advances
    /// past records its filter or visibility dropped.
    pub next: Cursor,
    pub head: u64,
    pub floor: u64,
    /// The cursor is older than the floor: records may be missing. Re-read
    /// with `entity.find` (not pinned to a cursor) and resume from the head.
    pub expired: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_zero_u8(n: &u8) -> bool {
    *n == 0
}

// ── capture ──────────────────────────────────────────────────────────────

/// [`diff_fields`] plus `workspace` and `kind`.
pub fn capture_fields(old: &Entity, new: &Entity) -> Vec<String> {
    let mut changed = diff_fields(old, new);
    if old.workspace != new.workspace {
        changed.push("workspace".into());
    }
    if old.kind != new.kind {
        changed.push("kind".into());
    }
    changed
}

fn field_value(e: &Entity, field: &str) -> Value {
    match field {
        "title" => json!(e.title),
        "state" => json!(e.state),
        "aliases" => json!(e.aliases),
        "provenance" => json!(e.provenance),
        "workspace" => json!(e.workspace),
        "kind" => json!(e.kind),
        prop => e.props.get(prop).cloned().unwrap_or(Value::Null),
    }
}

fn link_id(from: &str, rel: &str, to: &str) -> String {
    format!("{from}|{rel}|{to}")
}

/// The change records of a staged batch, computed against the graph before
/// the commit: entities in `put_entities` order (no record when neither a
/// captured field nor the revision changed), then deletions, then put
/// links (sorted; new or with changed properties), deleted links (sorted;
/// cascades included), and one `schema.change` when the batch carries a
/// schema (`schema_move` = its version and renames). Cursors carry `n`;
/// the store stamps `seq`.
pub(crate) fn capture(
    g: &Graph,
    schema: &Schema,
    batch: &WriteBatch,
    ctx: &ActionContext,
    schema_move: Option<(u64, &[Rename])>,
    now: i64,
) -> Vec<StoredChange> {
    let cause = Cause::of(ctx);
    let put: HashMap<&str, &Entity> = batch.put_entities.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut out: Vec<StoredChange> = Vec::new();
    let stored = |record: ChangeRecord| StoredChange { record, before_vis: None, rev_after: None };

    for new in &batch.put_entities {
        if !schema.captures(&new.kind) {
            continue;
        }
        let old = g.get(&new.id);
        let mut r = ChangeRecord::blank(ChangeOp::EntityCreate, now, cause.clone());
        r.workspace = new.workspace.clone();
        r.kind = new.kind.clone();
        r.id = new.id.clone();
        r.revision = Some(new.revision);
        let mut c = stored(r);
        c.rev_after = Some(new.revision);
        if let Some(old) = old {
            let changed = capture_fields(old, new);
            // Nothing changed: no record. A batch that changes a field and
            // changes it back still writes a new revision, so it gets an
            // update with no changed fields (mirrors keep the revision).
            if changed.is_empty() && old.revision == new.revision {
                continue;
            }
            c.record.op = ChangeOp::EntityUpdate;
            c.record.before = changed.iter().map(|f| (f.clone(), field_value(old, f))).collect();
            if old.state != new.state {
                c.record.state = Some(StateMove { from: old.state.clone(), to: new.state.clone() });
            }
            let ws = (old.workspace != new.workspace)
                .then(|| FromTo { from: old.workspace.clone(), to: new.workspace.clone() });
            let kind = (old.kind != new.kind).then(|| FromTo { from: old.kind.clone(), to: new.kind.clone() });
            if ws.is_some() || kind.is_some() {
                c.record.moved = Some(Moved { workspace: ws, kind });
            }
            c.record.changed = changed;
            c.before_vis = Some(VisStub::of(old));
        }
        out.push(c);
    }

    for id in &batch.del_entities {
        let Some(old) = g.get(id) else { continue };
        if !schema.captures(&old.kind) {
            continue;
        }
        let mut r = ChangeRecord::blank(ChangeOp::EntityDelete, now, cause.clone());
        r.workspace = old.workspace.clone();
        r.kind = old.kind.clone();
        r.id = old.id.clone();
        r.revision = Some(old.revision + 1);
        let mut c = stored(r);
        c.before_vis = Some(VisStub::of(old));
        out.push(c);
    }

    // Ends as the commit leaves them (put in this batch, else as they are).
    let end = |id: &str, ws: &str| -> VisStub {
        match put.get(id).copied().or_else(|| g.get(id)) {
            Some(e) => VisStub::of(e),
            None => VisStub { id: id.to_string(), workspace: ws.to_string(), ..Default::default() },
        }
    };
    let ends_captured = |ends: &[VisStub; 2]| ends.iter().all(|s| s.kind.is_empty() || schema.captures(&s.kind));

    let mut puts: Vec<&Link> = batch.put_links.iter().collect();
    puts.sort_by(|a, b| (&a.from, &a.rel, &a.to).cmp(&(&b.from, &b.rel, &b.to)));
    for l in puts {
        let old = g.get_link(&l.from, &l.rel, &l.to);
        let (op, changed, before) = match old {
            None => (ChangeOp::LinkCreate, Vec::new(), Map::new()),
            Some(o) if o.props == l.props => continue,
            Some(o) => {
                let keys: std::collections::BTreeSet<&String> = o.props.keys().chain(l.props.keys()).collect();
                let changed: Vec<String> =
                    keys.into_iter().filter(|k| o.props.get(*k) != l.props.get(*k)).cloned().collect();
                let before =
                    changed.iter().map(|k| (k.clone(), o.props.get(k).cloned().unwrap_or(Value::Null))).collect();
                (ChangeOp::LinkUpdate, changed, before)
            }
        };
        let ends = [end(&l.from, &l.workspace), end(&l.to, &l.workspace)];
        if !ends_captured(&ends) {
            continue;
        }
        let mut r = ChangeRecord::blank(op, now, cause.clone());
        r.workspace = if l.workspace.is_empty() { ends[0].workspace.clone() } else { l.workspace.clone() };
        r.kind = l.rel.clone();
        r.id = link_id(&l.from, &l.rel, &l.to);
        r.changed = changed;
        r.before = before;
        r.link = Some(LinkChange {
            from: l.from.clone(),
            rel: l.rel.clone(),
            to: l.to.clone(),
            props: l.props.clone(),
            ends,
        });
        out.push(stored(r));
    }

    let mut dels: Vec<&LinkKey> = batch.del_links.iter().collect();
    dels.sort();
    for (from, rel, to) in dels {
        let Some(old) = g.get_link(from, rel, to) else { continue };
        let stub = |id: &str| match g.get(id) {
            Some(e) => VisStub::of(e),
            None => VisStub { id: id.to_string(), workspace: old.workspace.clone(), ..Default::default() },
        };
        let ends = [stub(from), stub(to)];
        if !ends_captured(&ends) {
            continue;
        }
        let mut r = ChangeRecord::blank(ChangeOp::LinkDelete, now, cause.clone());
        r.workspace = if old.workspace.is_empty() { ends[0].workspace.clone() } else { old.workspace.clone() };
        r.kind = rel.clone();
        r.id = link_id(from, rel, to);
        r.link =
            Some(LinkChange { from: from.clone(), rel: rel.clone(), to: to.clone(), props: old.props.clone(), ends });
        out.push(stored(r));
    }

    if let Some((version, renames)) = schema_move {
        let mut r = ChangeRecord::blank(ChangeOp::SchemaChange, now, cause);
        r.kind = "schema".into();
        r.id = version.to_string();
        r.schema = Some(SchemaMove { version, renames: renames.to_vec() });
        out.push(stored(r));
    }

    for (n, c) in out.iter_mut().enumerate() {
        c.record.cursor = Cursor::new(0, n as u32);
    }
    out
}

// ── filtering and visibility ─────────────────────────────────────────────

/// A source glob, lowercased and split into characters once per filter.
struct SourceGlob {
    exclude: bool,
    pattern: Vec<char>,
}

/// A [`ChangeFilter`] ready to evaluate many records: its source globs are
/// parsed once, not once per record.
pub struct FilterEval<'a> {
    pub filter: &'a ChangeFilter,
    sources: Vec<SourceGlob>,
}

impl<'a> FilterEval<'a> {
    pub fn new(filter: &'a ChangeFilter) -> Self {
        let sources = filter
            .sources
            .iter()
            .map(|p| match p.strip_prefix('!') {
                Some(x) => SourceGlob { exclude: true, pattern: x.to_lowercase().chars().collect() },
                None => SourceGlob { exclude: false, pattern: p.to_lowercase().chars().collect() },
            })
            .collect();
        Self { filter, sources }
    }

    fn source_matches(&self, source: Option<&str>) -> bool {
        if self.sources.is_empty() {
            return true;
        }
        let text: Vec<char> = source.unwrap_or("").to_lowercase().chars().collect();
        if source.is_some() && self.sources.iter().any(|g| g.exclude && crate::modeling::glob_chars(&g.pattern, &text))
        {
            return false;
        }
        let mut includes = self.sources.iter().filter(|g| !g.exclude).peekable();
        includes.peek().is_none()
            || source.is_some() && includes.any(|g| crate::modeling::glob_chars(&g.pattern, &text))
    }

    /// Does `r`, the record a reader receives (after [`view`]), pass the
    /// filter? See [`matches`].
    pub fn matches(&self, schema: &Schema, r: &ChangeRecord) -> bool {
        let f = self.filter;
        if r.op == ChangeOp::Gap {
            return true;
        }
        let op_ok = f.ops.is_empty()
            || f.ops.contains(&r.op)
            // A reader asking for updates also gets the ones that hid the entity.
            || (r.op == ChangeOp::EntityHidden && f.ops.contains(&ChangeOp::EntityUpdate));
        if !op_ok {
            return false;
        }
        // `entity.hidden` carries no cause: the source and migration
        // dimensions do not apply to it, so a filter cannot probe them.
        if r.op != ChangeOp::EntityHidden && !self.source_matches(r.cause.source.as_deref()) {
            return false;
        }
        if r.op == ChangeOp::SchemaChange {
            return true;
        }
        if r.cause.migration && !f.include_migrations {
            return false;
        }
        let link = r.link.as_ref();
        if !f.kinds.is_empty() {
            let mut kinds: Vec<&str> = match link {
                Some(l) => l.ends.iter().map(|s| s.kind.as_str()).collect(),
                None => vec![r.kind.as_str()],
            };
            if let Some(k) = r.moved.as_ref().and_then(|m| m.kind.as_ref()) {
                kinds.push(&k.from);
            }
            if !kinds.iter().any(|k| f.kinds.iter().any(|want| schema.kind_is(k, want))) {
                return false;
            }
        }
        if !f.workspaces.is_empty() {
            let old = r.moved.as_ref().and_then(|m| m.workspace.as_ref()).map(|w| w.from.as_str());
            if !f.workspaces.iter().any(|w| *w == r.workspace || Some(w.as_str()) == old) {
                return false;
            }
        }
        if !f.ids.is_empty() {
            let hit = match link {
                Some(l) => f.ids.iter().any(|id| *id == l.from || *id == l.to),
                None => f.ids.contains(&r.id),
            };
            if !hit {
                return false;
            }
        }
        if !f.rels.is_empty() {
            let Some(l) = link else { return false };
            if !f.rels.iter().any(|name| schema.resolve_rel_lenient(name).0 == l.rel) {
                return false;
            }
        }
        // On the viewed record: a `became_visible` update carries no changed
        // fields, so a `props` filter does not match it (and cannot probe
        // what changed while the reader could not see the entity).
        if !f.props.is_empty()
            && matches!(r.op, ChangeOp::EntityUpdate | ChangeOp::LinkUpdate)
            && !r.changed.iter().any(|c| f.props.contains(c))
        {
            return false;
        }
        true
    }

    /// A cheap test on the stored record, without the graph: `false` only
    /// when the reader's record certainly fails the filter. Exact for every
    /// op whose record no reader sees differently (deletes, links, schema,
    /// gaps); for creates and updates, which [`view`] may turn into
    /// `entity.hidden` or a `became_visible` upsert, only the op and id are
    /// checked here (they do not change), the rest after [`view`].
    pub fn may_match(&self, schema: &Schema, r: &ChangeRecord) -> bool {
        let f = self.filter;
        match r.op {
            ChangeOp::EntityCreate | ChangeOp::EntityUpdate => {
                let op_ok = f.ops.is_empty()
                    || f.ops.contains(&r.op)
                    || (r.op == ChangeOp::EntityUpdate && f.ops.contains(&ChangeOp::EntityHidden));
                op_ok && (f.ids.is_empty() || f.ids.contains(&r.id))
            }
            _ => self.matches(schema, r),
        }
    }

    /// The record `p` receives for `stored` (joined with `entity`), or none:
    /// [`view`] first, then the filter on what the reader would receive
    /// (never on fields the view leaves out), then [`redact`].
    pub fn reader_record(
        &self,
        p: &Principal,
        schema: &Schema,
        g: &Graph,
        stored: &StoredChange,
        entity: Option<Entity>,
    ) -> Option<ChangeRecord> {
        if !self.may_match(schema, &stored.record) {
            return None;
        }
        self.view_and_match(p, schema, g, stored, entity)
    }

    /// [`reader_record`](Self::reader_record) for a record that already
    /// passed [`may_match`](Self::may_match) (checked outside the graph lock).
    pub fn view_and_match(
        &self,
        p: &Principal,
        schema: &Schema,
        g: &Graph,
        stored: &StoredChange,
        entity: Option<Entity>,
    ) -> Option<ChangeRecord> {
        let record = stored.joined(entity);
        let mut v = view(p, &record, stored.before_vis.as_ref(), g)?;
        if !self.matches(schema, &v) {
            return None;
        }
        redact(&mut v, self.filter.fields);
        Some(v)
    }
}

/// Does `r`, the record a reader receives (after [`view`]), pass the filter?
/// Visibility is [`view`]'s job; filter the viewed record, so a filter can
/// never test a field the reader is not given ([`FilterEval::reader_record`]
/// does both in order).
///
/// `gap` records always match. `schema.change` is tenant-wide: it passes the
/// kind, workspace, id, relation and property dimensions and is not counted
/// as a migration record. `entity.hidden` matches on its op (or
/// `entity.update`), id, kind and workspace only. `props` needs one of its
/// fields in the viewed record's `changed` (empty for `became_visible`).
pub fn matches(f: &ChangeFilter, schema: &Schema, r: &ChangeRecord) -> bool {
    FilterEval::new(f).matches(schema, r)
}

fn stub_visible(p: &Principal, s: &VisStub, g: &Graph) -> bool {
    policy::visible(p, &s.as_entity(), g)
}

/// Judged on the entity as it is now when it exists, else on the stub.
fn end_visible(p: &Principal, s: &VisStub, g: &Graph) -> bool {
    match g.get(&s.id) {
        Some(e) => policy::visible(p, e, g),
        None => stub_visible(p, s, g),
    }
}

/// May `p` read tenant-wide records (`schema.change`, `gap`)?
fn reads_tenant(p: &Principal) -> bool {
    p.allows(Access::Read) && !p.workspaces.is_empty()
}

/// The record `p` gets, or none. `before_vis` is the stored visibility of
/// the entity before the change; `g` is the graph at delivery (team
/// membership in `acl` is judged on it).
///
/// Create or update: visible after and before → the record with `before`;
/// visible only after → without `before`, `changed`, old state or move,
/// with `became_visible` (updates); visible only before → `entity.hidden` with
/// the old id, kind and workspace and nothing else. Deletes carry id, kind
/// and workspace. Links need both ends visible. `schema.change` and `gap`
/// go to every reader; a gap lists only what the reader can see now and
/// counts the rest in `hidden`.
pub fn view(p: &Principal, r: &ChangeRecord, before_vis: Option<&VisStub>, g: &Graph) -> Option<ChangeRecord> {
    match r.op {
        ChangeOp::EntityCreate | ChangeOp::EntityUpdate => {
            let after = match &r.entity {
                Some(e) => policy::visible(p, e, g),
                None => g.get(&r.id).is_some_and(|e| policy::visible(p, e, g)),
            };
            let before = r.op == ChangeOp::EntityUpdate && before_vis.is_some_and(|s| stub_visible(p, s, g));
            match (after, before) {
                (true, true) => Some(r.clone()),
                (true, false) => {
                    let mut v = r.clone();
                    v.before.clear();
                    // Which fields changed while the reader could not see
                    // the entity is not theirs to know either.
                    v.changed.clear();
                    v.moved = None;
                    if let Some(s) = &mut v.state {
                        s.from = None;
                    }
                    v.became_visible = r.op == ChangeOp::EntityUpdate;
                    Some(v)
                }
                (false, true) => {
                    let s = before_vis?;
                    let mut v = ChangeRecord::blank(ChangeOp::EntityHidden, r.ts, Cause::default());
                    v.cursor = r.cursor;
                    v.id = s.id.clone();
                    v.kind = s.kind.clone();
                    v.workspace = s.workspace.clone();
                    Some(v)
                }
                (false, false) => None,
            }
        }
        ChangeOp::EntityHidden => None,
        ChangeOp::EntityDelete => {
            let s = before_vis?;
            if !stub_visible(p, s, g) {
                return None;
            }
            let mut v = ChangeRecord::blank(ChangeOp::EntityDelete, r.ts, r.cause.clone());
            v.cursor = r.cursor;
            v.revision = r.revision;
            v.id = r.id.clone();
            v.kind = r.kind.clone();
            v.workspace = r.workspace.clone();
            Some(v)
        }
        ChangeOp::LinkCreate | ChangeOp::LinkUpdate | ChangeOp::LinkDelete => {
            let l = r.link.as_ref()?;
            l.ends.iter().all(|s| end_visible(p, s, g)).then(|| r.clone())
        }
        ChangeOp::SchemaChange => reads_tenant(p).then(|| r.clone()),
        ChangeOp::Gap => {
            if !reads_tenant(p) {
                return None;
            }
            let mut v = r.clone();
            if let Some(gap) = &mut v.gap {
                let seen = |id: &str| g.get(id).is_some_and(|e| policy::visible(p, e, g));
                let total = gap.entities.len() + gap.links.len();
                gap.entities.retain(|id| seen(id));
                gap.links.retain(|(from, _, to)| seen(from) && seen(to));
                gap.hidden += (total - gap.entities.len() - gap.links.len()) as u64;
            }
            Some(v)
        }
    }
}

/// Trim a record to what `mode` asks for.
pub fn redact(r: &mut ChangeRecord, mode: FieldMode) {
    if mode == FieldMode::Ids {
        r.entity = None;
        r.before.clear();
        if let Some(l) = &mut r.link {
            l.props.clear();
        }
    }
}

// ── reads ────────────────────────────────────────────────────────────────

impl Ontology {
    /// Records after `after` that `p` may see and `f` wants, at most `limit`
    /// scanned (≤ [`MAX_PAGE`]). Refuses with `unavailable` when the store
    /// keeps no change log. An `after` older than the floor, or ahead of the
    /// head (a cursor from before a restart or restore), returns no records
    /// and `expired = true`.
    pub fn changes_page(&self, p: &Principal, after: Cursor, f: &ChangeFilter, limit: usize) -> Result<ChangePage> {
        self.changes_page_valid_from(p, after, after.seq, f, limit)
    }

    /// The first page of the oldest records kept (`from = earliest`): every
    /// record from the floor on. When retention raises the floor while the
    /// page is read, the page is `expired` instead of silently starting
    /// later.
    pub fn changes_page_earliest(&self, p: &Principal, f: &ChangeFilter, limit: usize) -> Result<ChangePage> {
        let floor = self.store().changes_meta()?.floor;
        // Records of the floor's commit are kept, so start just before it.
        let after = if floor == 0 { Cursor::default() } else { Cursor::new(floor - 1, u32::MAX) };
        self.changes_page_valid_from(p, after, floor, f, limit)
    }

    /// A page after `after` that is complete only when nothing below
    /// `valid_from` was removed: expired iff `valid_from < floor`.
    fn changes_page_valid_from(
        &self,
        p: &Principal,
        after: Cursor,
        valid_from: u64,
        f: &ChangeFilter,
        limit: usize,
    ) -> Result<ChangePage> {
        f.check()?;
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let page = self.store().changes_since(after, limit.clamp(1, MAX_PAGE))?;
        let (head, floor) = (page.head.seq, page.floor);
        // Older than the floor, or ahead of the head: a cursor from before
        // the store restarted, was provisioned again or restored from an
        // older backup. Either way records may be missing; never skip them
        // silently.
        if valid_from < floor || after > page.head {
            return Ok(ChangePage { changes: Vec::new(), next: after, head, floor, expired: true });
        }
        if page.changes.is_empty() {
            // Nothing after `after` up to the head (read in the same
            // snapshot): resume from the head, so a reader that started
            // before the floor's commit is not reported expired next time.
            // (Commits made while capture is off leave no records; turning
            // it on again writes a gap after the head, so a reader moved
            // here meanwhile is told.)
            return Ok(ChangePage { changes: Vec::new(), next: after.max(page.head), head, floor, expired: false });
        }
        let schema = self.schema();
        let eval = FilterEval::new(f);
        let mut next = after;
        // The filter's graph-free part runs before the graph read lock is
        // taken, so a writer waits only for the visibility checks.
        let candidates: Vec<_> = page
            .changes
            .into_iter()
            .filter(|(stored, _)| {
                next = stored.record.cursor;
                eval.may_match(&schema, &stored.record)
            })
            .collect();
        let mut changes = Vec::new();
        if !candidates.is_empty() {
            let g = self.reader(p)?;
            for (stored, entity) in candidates {
                if let Some(v) = eval.view_and_match(p, &schema, &g, &stored, entity) {
                    changes.push(v);
                }
            }
        }
        Ok(ChangePage { changes, next, head, floor, expired: false })
    }

    /// The newest position of the change feed.
    pub fn change_head(&self) -> Result<Cursor> {
        Ok(self.store().changes_meta()?.head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntityInput, Mutation};
    use crate::policy::Role;

    fn ent(id: &str, ws: &str) -> Entity {
        let (kind, _) = id.split_once(':').unwrap();
        Entity {
            id: id.into(),
            kind: kind.into(),
            workspace: ws.into(),
            title: id.into(),
            state: None,
            props: Map::new(),
            aliases: Vec::new(),
            provenance: None,
            created_at: 1,
            updated_at: 1,
            revision: 1,
        }
    }

    fn link(from: &str, rel: &str, to: &str) -> Link {
        Link {
            from: from.into(),
            rel: rel.into(),
            to: to.into(),
            props: Map::new(),
            workspace: "ws1".into(),
            created_at: 1,
        }
    }

    fn ctx() -> ActionContext {
        ActionContext::new(Principal::admin("ada"), "ws1").with_source("connector:jira")
    }

    fn graph(entities: Vec<Entity>, links: Vec<Link>) -> Graph {
        let mut g = Graph::default();
        for e in entities {
            g.put_entity(e);
        }
        for l in links {
            g.put_link(l);
        }
        g
    }

    fn cap(g: &Graph, batch: &WriteBatch) -> Vec<StoredChange> {
        capture(g, &Schema::enterprise(), batch, &ctx(), None, 7)
    }

    fn restricted(mut e: Entity, acl: &[&str]) -> Entity {
        e.props.insert("visibility".into(), json!("restricted"));
        e.props.insert("acl".into(), json!(acl));
        e
    }

    #[test]
    fn cursor_format_and_parse() {
        let c = Cursor::new(42, 3);
        assert_eq!(c.to_string(), "42.3");
        assert_eq!("42.3".parse::<Cursor>().unwrap(), c);
        assert_eq!(serde_json::to_value(c).unwrap(), json!("42.3"));
        assert_eq!(serde_json::from_value::<Cursor>(json!("0.0")).unwrap(), Cursor::default());
        for bad in ["42", "a.b", "1.-1", "", "1.2.3"] {
            assert!(bad.parse::<Cursor>().is_err(), "{bad}");
        }
        assert!(Cursor::new(1, 9) < Cursor::new(2, 0));
        assert!(Cursor::new(2, 0) < Cursor::new(2, 1));
        assert_eq!("link.delete".parse::<ChangeOp>().unwrap(), ChangeOp::LinkDelete);
        assert_eq!(serde_json::to_value(ChangeOp::EntityHidden).unwrap(), json!("entity.hidden"));
    }

    #[test]
    fn capture_create() {
        let g = Graph::default();
        let e = ent("issue:1", "ws1");
        let out = cap(&g, &WriteBatch { put_entities: vec![e.clone()], ..Default::default() });
        assert_eq!(out.len(), 1);
        let r = &out[0].record;
        assert_eq!(
            (r.op, r.id.as_str(), r.kind.as_str(), r.workspace.as_str()),
            (ChangeOp::EntityCreate, "issue:1", "issue", "ws1")
        );
        assert_eq!(r.revision, Some(1));
        assert!(r.before.is_empty() && r.changed.is_empty() && out[0].before_vis.is_none());
        assert_eq!(out[0].rev_after, Some(1));
        assert_eq!(r.cause.actor, "ada");
        assert_eq!(r.cause.source.as_deref(), Some("connector:jira"));
        assert!(r.entity.is_none(), "the document is joined, not stored");
    }

    #[test]
    fn capture_update_with_before_and_state() {
        let mut old = ent("issue:1", "ws1");
        old.state = Some("open".into());
        old.props.insert("priority".into(), json!("low"));
        let g = graph(vec![old.clone()], vec![]);
        let mut new = old.clone();
        new.state = Some("triaged".into());
        new.props.insert("priority".into(), json!("high"));
        new.props.insert("description".into(), json!("x"));
        new.revision = 2;
        let out = cap(&g, &WriteBatch { put_entities: vec![new], ..Default::default() });
        let c = &out[0];
        assert_eq!(c.record.op, ChangeOp::EntityUpdate);
        assert_eq!(c.record.changed, vec!["state", "description", "priority"]);
        assert_eq!(c.record.before["state"], json!("open"));
        assert_eq!(c.record.before["priority"], json!("low"));
        assert_eq!(c.record.before["description"], Value::Null);
        assert_eq!(c.record.state, Some(StateMove { from: Some("open".into()), to: Some("triaged".into()) }));
        assert_eq!(c.before_vis, Some(VisStub::of(&old)));
        assert_eq!((c.record.revision, c.rev_after), (Some(2), Some(2)));
        assert!(c.record.moved.is_none());
    }

    #[test]
    fn capture_noop_upsert_is_nothing() {
        let old = ent("issue:1", "ws1");
        let g = graph(vec![old.clone()], vec![]);
        let mut same = old.clone();
        same.updated_at = 99;
        assert!(cap(&g, &WriteBatch { put_entities: vec![same], ..Default::default() }).is_empty());
        // Changed and changed back inside one batch: a new revision, no changed field.
        let mut bumped = old.clone();
        bumped.revision = 3;
        let out = cap(&g, &WriteBatch { put_entities: vec![bumped], ..Default::default() });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].record.op, ChangeOp::EntityUpdate);
        assert!(out[0].record.changed.is_empty() && out[0].record.before.is_empty());
        assert_eq!(out[0].rev_after, Some(3));
        // A link put with the same properties is not a change either.
        let g = graph(vec![old, ent("issue:2", "ws1")], vec![link("issue:1", "depends_on", "issue:2")]);
        let batch = WriteBatch { put_links: vec![link("issue:1", "depends_on", "issue:2")], ..Default::default() };
        assert!(cap(&g, &batch).is_empty());
    }

    #[test]
    fn capture_workspace_move_and_kind_change() {
        let old = ent("issue:1", "ws1");
        let g = graph(vec![old.clone()], vec![]);
        let mut new = old.clone();
        new.workspace = "ws2".into();
        new.kind = "task".into();
        new.title = "renamed".into();
        let out = cap(&g, &WriteBatch { put_entities: vec![new], ..Default::default() });
        let r = &out[0].record;
        assert_eq!(r.changed, vec!["title", "workspace", "kind"]);
        let moved = r.moved.as_ref().unwrap();
        assert_eq!(moved.workspace, Some(FromTo { from: "ws1".into(), to: "ws2".into() }));
        assert_eq!(moved.kind, Some(FromTo { from: "issue".into(), to: "task".into() }));
        assert_eq!(r.before["workspace"], json!("ws1"));
        assert_eq!(r.before["kind"], json!("issue"));
        assert_eq!((r.workspace.as_str(), r.kind.as_str()), ("ws2", "task"));
    }

    #[test]
    fn capture_delete_with_cascaded_links_and_n_order() {
        let a = restricted(ent("issue:1", "ws1"), &["user:v"]);
        let g = graph(
            vec![a.clone(), ent("issue:2", "ws1"), ent("issue:3", "ws1")],
            vec![link("issue:1", "depends_on", "issue:2"), link("issue:3", "depends_on", "issue:1")],
        );
        let batch = WriteBatch {
            put_entities: vec![ent("issue:4", "ws1")],
            del_entities: vec!["issue:1".into()],
            put_links: vec![link("issue:4", "depends_on", "issue:2")],
            del_links: vec![
                ("issue:3".into(), "depends_on".into(), "issue:1".into()),
                ("issue:1".into(), "depends_on".into(), "issue:2".into()),
            ],
            ..Default::default()
        };
        let out = cap(&g, &batch);
        let ops: Vec<(ChangeOp, &str)> = out.iter().map(|c| (c.record.op, c.record.id.as_str())).collect();
        assert_eq!(
            ops,
            vec![
                (ChangeOp::EntityCreate, "issue:4"),
                (ChangeOp::EntityDelete, "issue:1"),
                (ChangeOp::LinkCreate, "issue:4|depends_on|issue:2"),
                (ChangeOp::LinkDelete, "issue:1|depends_on|issue:2"),
                (ChangeOp::LinkDelete, "issue:3|depends_on|issue:1"),
            ]
        );
        let ns: Vec<u32> = out.iter().map(|c| c.record.cursor.n).collect();
        assert_eq!(ns, vec![0, 1, 2, 3, 4]);
        let del = &out[1];
        assert_eq!(del.record.revision, Some(2));
        assert_eq!(del.before_vis.as_ref().unwrap().acl, vec!["user:v"]);
        let ends = &out[4].record.link.as_ref().unwrap().ends;
        assert_eq!(
            ends[1].visibility.as_deref(),
            Some("restricted"),
            "end stubs come from the graph before the commit"
        );
        let created = &out[2].record.link.as_ref().unwrap().ends;
        assert_eq!(created[0].id, "issue:4", "a new end comes from the batch");
    }

    #[test]
    fn capture_link_update() {
        let g = graph(
            vec![ent("issue:1", "ws1"), ent("issue:2", "ws1")],
            vec![Link {
                props: json!({"why": "a", "keep": 1}).as_object().cloned().unwrap(),
                ..link("issue:1", "depends_on", "issue:2")
            }],
        );
        let new = Link {
            props: json!({"why": "b", "keep": 1}).as_object().cloned().unwrap(),
            ..link("issue:1", "depends_on", "issue:2")
        };
        let out = cap(&g, &WriteBatch { put_links: vec![new], ..Default::default() });
        assert_eq!(out[0].record.op, ChangeOp::LinkUpdate);
        assert_eq!(out[0].record.changed, vec!["why"]);
        assert_eq!(out[0].record.before["why"], json!("a"));
        assert_eq!(out[0].record.link.as_ref().unwrap().props["why"], json!("b"));
    }

    #[test]
    fn capture_merge() {
        let o = Ontology::in_memory(Schema::enterprise());
        let c = ActionContext::new(Principal::admin("ada"), "ws1");
        o.apply(
            &c,
            vec![
                Mutation::Upsert(EntityInput::new("issue", "1", "Timeout")),
                Mutation::Upsert(EntityInput::new("issue", "2", "Timeout dup").prop("priority", "high")),
                Mutation::Upsert(EntityInput::new("issue", "3", "Other").link("depends_on", "issue:2")),
            ],
        )
        .unwrap();
        let head = o.change_head().unwrap();
        o.merge(&c, "issue:1", "issue:2").unwrap();
        let page = o.changes_page(&Principal::admin("ada"), head, &ChangeFilter::default(), 100).unwrap();
        let ops: Vec<(ChangeOp, &str)> = page.changes.iter().map(|r| (r.op, r.id.as_str())).collect();
        assert_eq!(
            ops,
            vec![
                (ChangeOp::EntityUpdate, "issue:1"),
                (ChangeOp::EntityDelete, "issue:2"),
                (ChangeOp::LinkCreate, "issue:3|depends_on|issue:1"),
                (ChangeOp::LinkDelete, "issue:3|depends_on|issue:2"),
            ]
        );
        assert!(page.changes[0].changed.contains(&"priority".to_string()));
        assert!(page.changes.iter().all(|r| r.cursor.seq == page.changes[0].cursor.seq), "one commit");
    }

    #[test]
    fn capture_schema_change_with_renames() {
        let g = Graph::default();
        let renames = [Rename { what: RenameWhat::Kind, from: "issue".into(), to: "ticket".into() }];
        let out = capture(&g, &Schema::enterprise(), &WriteBatch::default(), &ctx(), Some((5, &renames)), 7);
        assert_eq!(out.len(), 1);
        let r = &out[0].record;
        assert_eq!((r.op, r.kind.as_str(), r.id.as_str()), (ChangeOp::SchemaChange, "schema", "5"));
        assert_eq!(r.schema, Some(SchemaMove { version: 5, renames: renames.to_vec() }));
    }

    #[test]
    fn migration_and_run_cause() {
        let mut c = ctx().with_intent(MIGRATION_INTENT);
        c.cause = Some(RunCause { automation: "triage".into(), run: "run_1".into(), depth: 2 });
        let cause = Cause::of(&c);
        assert!(cause.migration);
        assert_eq!(
            (cause.automation.as_deref(), cause.run.as_deref(), cause.depth),
            (Some("triage"), Some("run_1"), 2)
        );
        assert!(!Cause::of(&ctx()).migration);
    }

    fn rec(op: ChangeOp, kind: &str, id: &str) -> ChangeRecord {
        let mut r = ChangeRecord::blank(op, 1, Cause::default());
        r.kind = kind.into();
        r.id = id.into();
        r.workspace = "ws1".into();
        r
    }

    fn link_rec(op: ChangeOp, from: &Entity, rel: &str, to: &Entity) -> ChangeRecord {
        let mut r = rec(op, rel, &link_id(&from.id, rel, &to.id));
        r.link = Some(LinkChange {
            from: from.id.clone(),
            rel: rel.into(),
            to: to.id.clone(),
            props: Map::new(),
            ends: [VisStub::of(from), VisStub::of(to)],
        });
        r
    }

    #[test]
    fn filters() {
        let s = Schema::enterprise();
        let f = |v: Value| -> ChangeFilter { serde_json::from_value(v).unwrap() };
        let issue = rec(ChangeOp::EntityUpdate, "issue", "issue:1");
        let svc = rec(ChangeOp::EntityCreate, "service", "service:api");
        // kinds, including interfaces
        assert!(matches(&f(json!({"kinds": ["issue"]})), &s, &issue));
        assert!(matches(&f(json!({"kinds": ["work_item"]})), &s, &issue));
        assert!(!matches(&f(json!({"kinds": ["work_item"]})), &s, &svc));
        // ops
        assert!(matches(&f(json!({"ops": ["entity.update"]})), &s, &issue));
        assert!(!matches(&f(json!({"ops": ["entity.update"]})), &s, &svc));
        // A stored update may reach a reader as entity.hidden: it passes the
        // graph-free check, and the viewed record decides.
        let hidden_only = f(json!({"ops": ["entity.hidden"]}));
        assert!(FilterEval::new(&hidden_only).may_match(&s, &issue), "updates can become entity.hidden");
        assert!(!matches(&hidden_only, &s, &issue), "a visible update is not entity.hidden");
        let mut hid = rec(ChangeOp::EntityHidden, "issue", "issue:1");
        hid.cause.source = Some("api".into());
        assert!(matches(&hidden_only, &s, &hid));
        assert!(matches(&f(json!({"ops": ["entity.update"]})), &s, &hid), "asked for updates: hidden ones too");
        assert!(!matches(&f(json!({"ops": ["entity.create"]})), &s, &hid));
        // entity.hidden ignores props and sources (it carries neither).
        assert!(matches(&f(json!({"sources": ["cli"], "props": ["salary"]})), &s, &hid));
        assert!(!FilterEval::new(&f(json!({"ops": ["entity.create"]}))).may_match(&s, &issue));
        // ids
        assert!(matches(&f(json!({"ids": ["issue:1"]})), &s, &issue));
        assert!(!matches(&f(json!({"ids": ["issue:2"]})), &s, &issue));
        // workspaces, including the one a move left
        let mut moved = issue.clone();
        moved.workspace = "ws2".into();
        moved.moved = Some(Moved { workspace: Some(FromTo { from: "ws1".into(), to: "ws2".into() }), kind: None });
        assert!(matches(&f(json!({"workspaces": ["ws1"]})), &s, &moved));
        assert!(!matches(&f(json!({"workspaces": ["ws3"]})), &s, &moved));
        // rels: canonical, inverse, ^rel; entities do not match a rel filter
        let (a, b) = (ent("issue:1", "ws1"), ent("issue:2", "ws1"));
        let dep = link_rec(ChangeOp::LinkCreate, &a, "depends_on", &b);
        for name in ["depends_on", "required_by", "^depends_on"] {
            assert!(matches(&f(json!({"rels": [name]})), &s, &dep), "{name}");
        }
        assert!(!matches(&f(json!({"rels": ["owns"]})), &s, &dep));
        assert!(!matches(&f(json!({"rels": ["depends_on"]})), &s, &issue));
        assert!(matches(&f(json!({"kinds": ["issue"], "ids": ["issue:2"]})), &s, &dep), "links match through ends");
        // props: any changed field; other ops pass
        let mut pri = issue.clone();
        pri.changed = vec!["priority".into()];
        assert!(matches(&f(json!({"props": ["priority", "title"]})), &s, &pri));
        assert!(!matches(&f(json!({"props": ["title"]})), &s, &pri));
        assert!(matches(&f(json!({"props": ["title"]})), &s, &svc));
        // sources: globs and exclusions
        let mut jira = issue.clone();
        jira.cause.source = Some("connector:jira".into());
        assert!(matches(&f(json!({"sources": ["connector:*"]})), &s, &jira));
        assert!(!matches(&f(json!({"sources": ["!connector:*"]})), &s, &jira));
        assert!(matches(&f(json!({"sources": ["!automation:*"]})), &s, &jira));
        assert!(matches(&f(json!({"sources": ["!automation:*"]})), &s, &issue), "no source is not excluded");
        assert!(!matches(&f(json!({"sources": ["connector:*"]})), &s, &issue), "no source does not match an include");
        assert!(!matches(&f(json!({"sources": ["connector:*", "!connector:jira"]})), &s, &jira));
        // migrations: included by default
        let mut mig = issue.clone();
        mig.cause.migration = true;
        assert!(ChangeFilter::default().include_migrations);
        assert!(matches(&ChangeFilter::default(), &s, &mig));
        assert!(matches(&f(json!({})), &s, &mig));
        assert!(!matches(&f(json!({"include_migrations": false})), &s, &mig));
        // gaps always match; schema.change is tenant-wide
        let gap = ChangeRecord::gap(
            3,
            1,
            Gap {
                reason: GapReason::UncapturedWriter,
                origin: String::new(),
                entities: vec![],
                links: vec![],
                hidden: 0,
                truncated: false,
                total: 0,
            },
        );
        assert!(matches(
            &f(json!({"kinds": ["service"], "ops": ["entity.create"], "include_migrations": false})),
            &s,
            &gap
        ));
        let mut sc = rec(ChangeOp::SchemaChange, "schema", "4");
        sc.workspace.clear();
        sc.cause.migration = true;
        assert!(matches(&f(json!({"kinds": ["issue"], "include_migrations": false})), &s, &sc));
        assert!(!matches(&f(json!({"ops": ["entity.create"]})), &s, &sc));
        // limits
        let many: Vec<String> = (0..=MAX_FILTER_IDS).map(|i| format!("issue:{i}")).collect();
        assert!(f(json!({"ids": many})).check().is_err());
    }

    fn viewer() -> Principal {
        Principal::new("user:v", vec![Role::Viewer], vec!["ws1".into()])
    }

    #[test]
    fn view_visible_to_visible_keeps_before() {
        let old = ent("issue:1", "ws1");
        let mut new = old.clone();
        new.title = "new".into();
        let mut r = rec(ChangeOp::EntityUpdate, "issue", "issue:1");
        r.before.insert("title".into(), json!("issue:1"));
        r.entity = Some(new.clone());
        let g = graph(vec![new], vec![]);
        let v = view(&viewer(), &r, Some(&VisStub::of(&old)), &g).unwrap();
        assert_eq!(v.before["title"], json!("issue:1"));
        assert!(!v.became_visible);
        // Another workspace sees nothing.
        assert!(view(&Principal::viewer("w", "ws2"), &r, Some(&VisStub::of(&old)), &g).is_none());
    }

    #[test]
    fn view_hidden_to_visible_has_no_before() {
        let old = restricted(ent("issue:1", "ws1"), &["user:x"]);
        let mut new = old.clone();
        new.props.insert("visibility".into(), json!("public"));
        new.props.insert("salary".into(), json!(200));
        let mut r = rec(ChangeOp::EntityUpdate, "issue", "issue:1");
        r.changed = vec!["salary".into(), "visibility".into()];
        r.before.insert("salary".into(), json!(100));
        r.before.insert("visibility".into(), json!("restricted"));
        r.state = Some(StateMove { from: Some("secret_state".into()), to: Some("open".into()) });
        r.entity = Some(new.clone());
        let g = graph(vec![new], vec![]);
        let v = view(&viewer(), &r, Some(&VisStub::of(&old)), &g).unwrap();
        assert!(v.became_visible);
        assert!(v.before.is_empty(), "the old salary never reaches a reader who could not see it");
        assert_eq!(v.state, Some(StateMove { from: None, to: Some("open".into()) }));
        let text = serde_json::to_string(&v).unwrap();
        assert!(!text.contains("100") && !text.contains("secret_state"), "{text}");
        // The admin saw it before and after.
        let a = view(&Principal::admin("a"), &r, Some(&VisStub::of(&old)), &graph(vec![], vec![])).unwrap();
        assert_eq!(a.before["salary"], json!(100));
    }

    #[test]
    fn view_visible_to_hidden_is_entity_hidden() {
        let old = ent("issue:1", "ws1");
        let new = restricted(old.clone(), &["user:x"]);
        let mut r = rec(ChangeOp::EntityUpdate, "issue", "issue:1");
        r.changed = vec!["acl".into(), "visibility".into()];
        r.before.insert("visibility".into(), Value::Null);
        r.entity = Some(new.clone());
        r.cause.actor = "ada".into();
        r.cursor = Cursor::new(9, 2);
        let g = graph(vec![new], vec![]);
        let v = view(&viewer(), &r, Some(&VisStub::of(&old)), &g).unwrap();
        assert_eq!(v.op, ChangeOp::EntityHidden);
        assert_eq!(
            (v.id.as_str(), v.kind.as_str(), v.workspace.as_str(), v.cursor),
            ("issue:1", "issue", "ws1", r.cursor)
        );
        assert!(v.entity.is_none() && v.before.is_empty() && v.changed.is_empty() && v.cause == Cause::default());
    }

    #[test]
    fn view_deletes_and_creates() {
        let gone = restricted(ent("issue:1", "ws1"), &["user:x"]);
        let mut d = rec(ChangeOp::EntityDelete, "issue", "issue:1");
        d.before.insert("title".into(), json!("t"));
        assert!(view(&viewer(), &d, Some(&VisStub::of(&gone)), &Graph::default()).is_none());
        let open = ent("issue:2", "ws1");
        let v = view(&viewer(), &d, Some(&VisStub::of(&open)), &Graph::default()).unwrap();
        assert!(v.before.is_empty() && v.id == "issue:1");
        let mut c = rec(ChangeOp::EntityCreate, "issue", "issue:2");
        c.entity = Some(open);
        let v = view(&viewer(), &c, None, &Graph::default()).unwrap();
        assert!(!v.became_visible && v.entity.is_some());
    }

    #[test]
    fn view_link_with_a_deleted_end() {
        let a = ent("issue:1", "ws1");
        let hidden = restricted(ent("issue:2", "ws1"), &["user:x"]);
        // issue:2 was deleted with the link: judged on its stub.
        let r = link_rec(ChangeOp::LinkDelete, &a, "depends_on", &hidden);
        let g = graph(vec![a.clone()], vec![]);
        assert!(view(&viewer(), &r, None, &g).is_none());
        assert!(view(&Principal::admin("a"), &r, None, &g).is_some());
        let open = ent("issue:3", "ws1");
        let r = link_rec(ChangeOp::LinkDelete, &a, "depends_on", &open);
        assert!(view(&viewer(), &r, None, &g).is_some());
        // An end that exists now is judged as it is now.
        let now_hidden = restricted(open.clone(), &["user:x"]);
        assert!(view(&viewer(), &r, None, &graph(vec![a, now_hidden], vec![])).is_none());
    }

    #[test]
    fn view_gap_and_schema() {
        let g = graph(vec![ent("issue:1", "ws1"), restricted(ent("issue:2", "ws1"), &["user:x"])], vec![]);
        let r = ChangeRecord::gap(
            4,
            1,
            Gap {
                reason: GapReason::BulkImport,
                origin: "p1".into(),
                entities: vec!["issue:1".into(), "issue:2".into(), "issue:3".into()],
                links: vec![("issue:1".into(), "depends_on".into(), "issue:2".into())],
                hidden: 0,
                truncated: false,
                total: 0,
            },
        );
        let v = view(&viewer(), &r, None, &g).unwrap();
        let gap = v.gap.unwrap();
        assert_eq!(gap.entities, vec!["issue:1"]);
        assert!(gap.links.is_empty());
        assert_eq!(gap.hidden, 3);
        let sc = rec(ChangeOp::SchemaChange, "schema", "2");
        assert!(view(&viewer(), &sc, None, &g).is_some());
        assert!(view(&Principal::new("nobody", vec![], vec!["ws1".into()]), &sc, None, &g).is_none());
    }

    #[test]
    fn redact_ids() {
        let a = ent("issue:1", "ws1");
        let mut r = link_rec(ChangeOp::LinkUpdate, &a, "depends_on", &ent("issue:2", "ws1"));
        r.link.as_mut().unwrap().props.insert("why".into(), json!("x"));
        r.before.insert("why".into(), json!("y"));
        r.entity = Some(a);
        r.changed = vec!["why".into()];
        redact(&mut r, FieldMode::Full);
        assert!(r.entity.is_some());
        redact(&mut r, FieldMode::Ids);
        assert!(r.entity.is_none() && r.before.is_empty() && r.link.as_ref().unwrap().props.is_empty());
        assert_eq!(r.changed, vec!["why"]);
    }

    #[test]
    fn stored_change_round_trips() {
        let mut c = StoredChange {
            record: rec(ChangeOp::EntityUpdate, "issue", "issue:1"),
            before_vis: Some(VisStub::of(&ent("issue:1", "ws1"))),
            rev_after: Some(3),
        };
        c.record.cursor = Cursor::new(5, 1);
        c.record.before.insert("title".into(), json!("t"));
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["op"], json!("entity.update"));
        assert_eq!(v["cursor"], json!("5.1"));
        assert!(v.get("entity").is_none());
        assert_eq!(serde_json::from_value::<StoredChange>(v).unwrap(), c);
        let joined = c.joined(None);
        assert!(joined.entity_missing);
    }
}
