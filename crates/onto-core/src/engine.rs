//! The ontology engine: permission-filtered synchronous reads and authorized,
//! audited, atomic writes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{RwLock, RwLockReadGuard};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::deps::{self, CriticalPath, DepDirection, DepReport, RootCause};
use crate::graph::{Graph, LinkKey};
use crate::live_schema::{self, Governance, SchemaMode};
use crate::model::*;
use crate::policy::{self, Access, Principal};
use crate::quality::{self, QualityReport};
use crate::query::{self, FindSpec, Hop, Subgraph, TraverseSpec};
use crate::rules::{self, Inferred};
use crate::schema::{Cardinality, Dir, PropType, Schema};
use crate::similarity::{self, DuplicatePair, VectorIndex};
use crate::store::{MemoryStore, Store, StoreStats, VersionRecord, WriteBatch};
use crate::text::TextIndex;
use crate::time::{fmt_ts, new_id, now_ms};
use crate::{Error, Result};

#[path = "engine_sync.rs"]
mod sync;
pub use sync::SyncReport;

#[derive(Clone, Debug)]
pub struct OntologyOptions {
    /// Reject unknown kinds, relations and properties.
    pub strict: bool,
    pub default_workspace: String,
    /// Where the active schema comes from (stored versions or config files).
    pub schema_mode: SchemaMode,
    /// Who may change, propose and approve schema changes.
    pub governance: Governance,
    /// Before every read, check the store's commit head and catch up when
    /// another process committed (read-your-writes across replicas, one cheap
    /// query per read). Off: reads see other processes' writes as soon as
    /// the follower applies them (milliseconds after LISTEN/NOTIFY).
    pub sync_reads: bool,
    /// How often the follower of a shared store polls when no notice arrives.
    pub follow_interval_ms: u64,
}

impl Default for OntologyOptions {
    fn default() -> Self {
        Self {
            strict: false,
            default_workspace: "default".into(),
            schema_mode: SchemaMode::default(),
            governance: Governance::default(),
            sync_reads: false,
            follow_interval_ms: 1_000,
        }
    }
}

/// Who is acting, where, and why. Required for every write so that every
/// meaningful action carries actor + context + authorization + target +
/// execution + result into the audit trail.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionContext {
    pub principal: Principal,
    pub workspace: String,
    pub request_id: String,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

impl ActionContext {
    pub fn new(principal: Principal, workspace: impl Into<String>) -> Self {
        Self { principal, workspace: workspace.into(), request_id: new_id("req"), intent: None, source: None }
    }

    pub fn system(workspace: impl Into<String>) -> Self {
        Self::new(Principal::system(), workspace)
    }

    pub fn with_intent(mut self, intent: impl Into<String>) -> Self {
        self.intent = Some(intent.into());
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = id.into();
        self
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ChangeSet {
    pub revision: u64,
    pub workspace: String,
    pub request_id: String,
    pub entities: Vec<Id>,
    pub links: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub entity: Entity,
    pub score: f32,
    /// Why it ranked: lexical/vector ranks, personal-graph distance, recency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signals: Option<Value>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// BM25 + hashed-vector similarity fused with reciprocal rank fusion.
    #[default]
    Hybrid,
    Lexical,
    Vector,
}

/// One entry of an entity's version history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub revision: u64,
    pub ts: i64,
    pub at: String,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub changed: Vec<String>,
}

/// What happened to one entity inside a time window.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeSummary {
    pub id: Id,
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_state: Option<String>,
    pub created: bool,
    pub deleted: bool,
    pub changed: Vec<String>,
    pub versions: usize,
    pub last_ts: i64,
    pub actors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeResult {
    pub kept: Id,
    pub merged: Id,
    pub links_moved: usize,
    pub revision: u64,
}

/// Extra items committed atomically with a mutation batch.
#[derive(Clone, Default)]
pub(crate) struct Extra {
    pub kv: Vec<(String, String, Value)>,
    pub redirects: Vec<(Id, Id)>,
    /// Audit events written with the batch.
    pub events: Vec<Event>,
    /// Schema (and its version) the mutations are validated against; it
    /// becomes active when the batch commits.
    pub schema: Option<(Arc<Schema>, u64)>,
    /// Kinds whose search documents are rebuilt after the commit.
    pub reindex: Vec<String>,
}

impl Extra {
    fn is_empty(&self) -> bool {
        self.kv.is_empty() && self.redirects.is_empty() && self.events.is_empty() && self.schema.is_none()
    }
}

/// The schema in force and its stored version.
#[derive(Clone)]
pub(crate) struct ActiveSchema {
    pub schema: Arc<Schema>,
    pub version: u64,
}

/// Fields that differ between two versions of an entity.
pub fn diff_fields(old: &Entity, new: &Entity) -> Vec<String> {
    let mut changed = Vec::new();
    if old.title != new.title {
        changed.push("title".to_string());
    }
    if old.state != new.state {
        changed.push("state".to_string());
    }
    let keys: std::collections::BTreeSet<&String> = old.props.keys().chain(new.props.keys()).collect();
    for k in keys {
        if old.props.get(k) != new.props.get(k) {
            changed.push(k.clone());
        }
    }
    if old.aliases != new.aliases {
        changed.push("aliases".to_string());
    }
    if old.provenance != new.provenance {
        changed.push("provenance".to_string());
    }
    changed
}

#[derive(Clone, Debug, Serialize)]
pub struct OntoStats {
    pub schema: String,
    pub entities: usize,
    pub links: usize,
    pub revision: u64,
    pub kinds: BTreeMap<String, usize>,
    pub indexed_docs: usize,
    pub vectors: usize,
    pub redirects: usize,
    pub store: StoreStats,
}

type Listener = Arc<dyn Fn(&ChangeSet) + Send + Sync>;

/// Vector candidates below this cosine similarity are noise.
const MIN_VECTOR_SIMILARITY: f32 = 0.25;

struct Inner {
    active: RwLock<ActiveSchema>,
    /// Why the last import of changed schema files was not applied.
    import_error: RwLock<Option<String>>,
    graph: RwLock<Graph>,
    text: RwLock<TextIndex>,
    /// In-memory vectors, used when no store indexes them.
    vectors: RwLock<VectorIndex>,
    /// The store that indexes entity vectors (pgvector); `vectors` stays
    /// empty then. A fork keeps its parent's.
    vector_store: Option<Arc<dyn Store>>,
    /// Merged-away id -> surviving id.
    redirects: RwLock<HashMap<Id, Id>>,
    store: Arc<dyn Store>,
    revision: AtomicU64,
    /// Store commit sequence the in-memory state reflects.
    seq: AtomicU64,
    opts: OntologyOptions,
    listeners: RwLock<Vec<Listener>>,
}

/// Cheap to clone; all clones share the same graph.
#[derive(Clone)]
pub struct Ontology {
    inner: Arc<Inner>,
}

impl Ontology {
    /// Load the graph from `store` and build the indexes.
    ///
    /// `schema` is the schema of the configuration (built-in packages plus
    /// files). The first open stores it as schema version 1; afterwards the
    /// stored live schema is authoritative and changes to the files are
    /// imported as new versions (see [`SchemaMode`]).
    pub fn open(schema: Schema, store: Arc<dyn Store>, opts: OntologyOptions) -> Result<Self> {
        let config_schema = schema;
        // Snapshot first: a schema read afterwards is at least as new, and
        // catching up from the snapshot's sequence re-applies anything newer.
        let snap = store.load()?;
        let (schema, version) = live_schema::load_active(&*store, &config_schema, opts.schema_mode)?;
        let vector_store = store.indexes_vectors().then(|| store.clone());
        let (graph, text, vectors) = sync::build_indexes(&schema, snap.entities, snap.links, vector_store.is_none());
        let redirects: HashMap<Id, Id> = store
            .kv_scan("redirect", "")?
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|t| (k, t.to_string())))
            .collect();
        let follow = store.shared().then(|| std::time::Duration::from_millis(opts.follow_interval_ms.max(50)));
        let onto = Self {
            inner: Arc::new(Inner {
                active: RwLock::new(ActiveSchema { schema: Arc::new(schema), version }),
                import_error: RwLock::new(None),
                graph: RwLock::new(graph),
                text: RwLock::new(text),
                vectors: RwLock::new(vectors),
                vector_store,
                redirects: RwLock::new(redirects),
                store,
                revision: AtomicU64::new(1),
                seq: AtomicU64::new(snap.seq),
                opts,
                listeners: RwLock::new(Vec::new()),
            }),
        };
        onto.bootstrap_schema(config_schema)?;
        onto.backfill_vectors()?;
        if let Some(interval) = follow {
            sync::spawn_follower(&onto.inner, interval)?;
        }
        Ok(onto)
    }

    /// Volatile ontology, handy for tests and embedding.
    pub fn in_memory(schema: Schema) -> Self {
        Self::open(schema, Arc::new(MemoryStore::default()), OntologyOptions::default())
            .expect("memory store cannot fail")
    }

    /// The schema in force. Cheap (an `Arc` clone); hold it for the duration
    /// of one operation so a concurrent schema change cannot split it.
    pub fn schema(&self) -> Arc<Schema> {
        self.inner.active.read().schema.clone()
    }

    /// Version of the schema in force (see [`schema_history`](Self::schema_history)).
    pub fn schema_version(&self) -> u64 {
        self.inner.active.read().version
    }

    pub(crate) fn active_schema(&self) -> ActiveSchema {
        self.inner.active.read().clone()
    }

    pub(crate) fn set_import_error(&self, err: Option<String>) {
        *self.inner.import_error.write() = err;
    }

    /// Why changed schema files could not be imported, if they could not.
    pub fn schema_import_error(&self) -> Option<String> {
        self.inner.import_error.read().clone()
    }

    pub fn store(&self) -> &Arc<dyn Store> {
        &self.inner.store
    }

    pub fn options(&self) -> &OntologyOptions {
        &self.inner.opts
    }

    /// Bumped on every successful write; part of every cache key.
    pub fn revision(&self) -> u64 {
        self.inner.revision.load(Ordering::SeqCst)
    }

    /// Called synchronously after each committed write.
    pub fn on_change(&self, f: impl Fn(&ChangeSet) + Send + Sync + 'static) {
        self.inner.listeners.write().push(Arc::new(f));
    }

    /// Give entities the store holds without a vector one (after a bulk
    /// import or a migration that added vectors). Returns how many.
    pub fn backfill_vectors(&self) -> Result<usize> {
        let Some(store) = &self.inner.vector_store else { return Ok(0) };
        let schema = self.schema();
        let embed = |entities: &[Entity]| -> Vec<Option<Vec<i8>>> {
            entities.iter().map(|e| similarity::embed_quantized(&searchable_text(&schema, e))).collect()
        };
        store.backfill_vectors(&embed)
    }

    /// Vector candidates from the store's index: (id, similarity) at or
    /// above `min`, best first, at most `want` (before permission filtering).
    fn store_vectors(
        &self,
        store: &Arc<dyn Store>,
        schema: &Schema,
        text: &str,
        kinds: &[String],
        workspace: Option<&str>,
        want: usize,
    ) -> Result<Vec<(String, f32)>> {
        let Some(q) = similarity::embed_quantized(text) else { return Ok(Vec::new()) };
        let mut concrete: Vec<String> = kinds.iter().flat_map(|k| schema.concrete_kinds(k)).collect();
        concrete.sort();
        concrete.dedup();
        if !kinds.is_empty() && concrete.is_empty() {
            return Ok(Vec::new());
        }
        let filter = crate::store::VectorFilter { workspace: workspace.map(str::to_string), kinds: concrete };
        let mut hits = store.vector_search(&q, want, &filter)?;
        hits.retain(|(_, s)| *s >= MIN_VECTOR_SIMILARITY);
        Ok(hits)
    }

    /// Escape hatch for custom read-only algorithms (no permission filtering).
    pub fn read<R>(&self, f: impl FnOnce(&Graph, &Schema) -> R) -> R {
        let schema = self.schema();
        let g = self.inner.graph.read();
        f(&g, &schema)
    }

    /// Independent in-memory copy for what-if scenarios: edits to the fork
    /// never reach this ontology or its store.
    pub fn fork(&self) -> Ontology {
        let graph = self.inner.graph.read().clone();
        Ontology {
            inner: Arc::new(Inner {
                active: RwLock::new(self.active_schema()),
                import_error: RwLock::new(None),
                graph: RwLock::new(graph),
                text: RwLock::new(self.inner.text.read().clone()),
                vectors: RwLock::new(self.inner.vectors.read().clone()),
                // Vector search keeps using the parent's index; entities edited
                // in the fork are found lexically.
                vector_store: self.inner.vector_store.clone(),
                redirects: RwLock::new(self.inner.redirects.read().clone()),
                store: Arc::new(MemoryStore::default()),
                revision: AtomicU64::new(self.revision()),
                seq: AtomicU64::new(0),
                opts: self.inner.opts.clone(),
                listeners: RwLock::new(Vec::new()),
            }),
        }
    }

    /// Follow merge redirects to the surviving id.
    pub fn resolve_id(&self, id: &str) -> String {
        let g = self.inner.graph.read();
        self.resolve_in(&g, id)
    }

    pub(crate) fn resolve_in(&self, g: &Graph, id: &str) -> String {
        if g.contains(id) {
            return id.to_string();
        }
        let redirects = self.inner.redirects.read();
        let mut cur = id;
        for _ in 0..8 {
            match redirects.get(cur) {
                Some(next) => cur = next,
                None => break,
            }
        }
        cur.to_string()
    }

    pub(crate) fn inner_graph_read(&self) -> RwLockReadGuard<'_, Graph> {
        self.inner.graph.read()
    }

    fn reader(&self, p: &Principal) -> Result<RwLockReadGuard<'_, Graph>> {
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        if self.inner.opts.sync_reads {
            self.sync_if_behind()?;
        }
        Ok(self.inner.graph.read())
    }

    // ── reads ────────────────────────────────────────────────────────────

    pub fn get(&self, p: &Principal, id: &str) -> Result<Entity> {
        let g = self.reader(p)?;
        let id = self.resolve_in(&g, id);
        match g.get(&id) {
            Some(e) if policy::visible(p, e, &g) => Ok(e.clone()),
            _ => Err(Error::not_found(&id)),
        }
    }

    pub fn get_many(&self, p: &Principal, ids: &[Id]) -> Result<Vec<Entity>> {
        let g = self.reader(p)?;
        Ok(ids
            .iter()
            .filter_map(|id| g.get(&self.resolve_in(&g, id)))
            .filter(|e| policy::visible(p, e, &g))
            .cloned()
            .collect())
    }

    /// Direct neighbours as (relation as walked, entity). `rel` filters.
    pub fn neighbors(&self, p: &Principal, id: &str, rel: Option<&str>) -> Result<Vec<(String, Entity)>> {
        let g = self.reader(p)?;
        let schema = self.schema();
        let id = self.resolve_in(&g, id);
        let id = id.as_str();
        if !g.get(id).is_some_and(|e| policy::visible(p, e, &g)) {
            return Err(Error::not_found(id));
        }
        let filter = rel.map(|r| schema.resolve_rel_lenient(r));
        let mut out: Vec<(String, Entity)> = g
            .steps(id)
            .filter(|s| match &filter {
                None => true,
                Some((base, dir)) => s.rel == base && (*dir == Dir::Both || *dir == s.dir),
            })
            .filter_map(|s| {
                let e = g.get(s.other)?;
                policy::visible(p, e, &g).then(|| (schema.display_rel(s.rel, s.dir), e.clone()))
            })
            .collect();
        out.sort_by(|a, b| (&a.0, &a.1.id).cmp(&(&b.0, &b.1.id)));
        Ok(out)
    }

    pub fn traverse(&self, p: &Principal, spec: &TraverseSpec) -> Result<Subgraph> {
        let g = self.reader(p)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(query::traverse(&self.schema(), &g, spec, &can))
    }

    /// Returns (page, total).
    pub fn find(&self, p: &Principal, spec: &FindSpec) -> Result<(Vec<Entity>, usize)> {
        let g = self.reader(p)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(query::find(&self.schema(), &g, spec, &can))
    }

    /// Hybrid, permission-aware search personalised to the caller.
    pub fn search(
        &self,
        p: &Principal,
        text: &str,
        kinds: &[String],
        workspace: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        self.search_with(p, text, kinds, workspace, limit, SearchMode::Hybrid, true)
    }

    /// Search with an explicit mode. Hybrid fuses BM25 and vector ranks
    /// (reciprocal rank fusion), then boosts things near the caller in the graph
    /// (their team, their work) and recently active things.
    #[allow(clippy::too_many_arguments)]
    pub fn search_with(
        &self,
        p: &Principal,
        text: &str,
        kinds: &[String],
        workspace: Option<&str>,
        limit: usize,
        mode: SearchMode,
        personalize: bool,
    ) -> Result<Vec<SearchHit>> {
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let schema = self.schema();
        let pool = (limit * 3).max(30);
        // Ask the store's vector index before taking the graph lock.
        let remote = match (&self.inner.vector_store, mode) {
            (Some(store), SearchMode::Hybrid | SearchMode::Vector) => {
                Some(self.store_vectors(store, &schema, text, kinds, workspace, (pool * 4).max(64))?)
            }
            _ => None,
        };
        let g = self.reader(p)?;
        let keep = |id: &str| {
            g.get(id).is_some_and(|e| {
                policy::visible(p, e, &g)
                    && workspace.is_none_or(|w| e.workspace == w)
                    && (kinds.is_empty() || kinds.iter().any(|k| schema.kind_is(&e.kind, k)))
            })
        };
        let lexical =
            if mode == SearchMode::Vector { Vec::new() } else { self.inner.text.read().search(text, pool, keep) };
        let vector = match remote {
            Some(mut hits) => {
                hits.retain(|(id, _)| keep(id));
                hits.truncate(pool);
                hits
            }
            None if mode == SearchMode::Lexical => Vec::new(),
            None => self.inner.vectors.read().search(text, pool, MIN_VECTOR_SIMILARITY, keep),
        };
        let mut fused: HashMap<String, (f32, Option<usize>, Option<usize>)> = HashMap::new();
        for (rank, (id, _)) in lexical.iter().enumerate() {
            let slot = fused.entry(id.clone()).or_default();
            slot.0 += 1.0 / (60.0 + rank as f32 + 1.0);
            slot.1 = Some(rank + 1);
        }
        for (rank, (id, _)) in vector.iter().enumerate() {
            let slot = fused.entry(id.clone()).or_default();
            slot.0 += 1.0 / (60.0 + rank as f32 + 1.0);
            slot.2 = Some(rank + 1);
        }
        // Personal graph: distance from the caller's own entity.
        let mut near: HashMap<String, usize> = HashMap::new();
        if personalize && let Some(me) = p.entity.as_deref().filter(|me| g.contains(me)) {
            let mut frontier = vec![me.to_string()];
            for d in 1..=2 {
                let mut next = Vec::new();
                for id in &frontier {
                    for s in g.steps(id) {
                        if s.other != me && !near.contains_key(s.other) {
                            near.insert(s.other.to_string(), d);
                            next.push(s.other.to_string());
                        }
                    }
                }
                frontier = next;
            }
        }
        let now = now_ms();
        let mut hits: Vec<SearchHit> = fused
            .into_iter()
            .filter_map(|(id, (base, lr, vr))| {
                let e = g.get(&id)?;
                let dist = near.get(&id).copied();
                let personal = match dist {
                    Some(1) => 1.3,
                    Some(2) => 1.15,
                    _ => 1.0,
                };
                let idle_days = ((now - e.updated_at).max(0) as f32) / 86_400_000.0;
                let recency = 1.0 + 0.2 * (-idle_days / 14.0).exp();
                let score = base * personal * recency * 100.0;
                Some(SearchHit {
                    entity: e.clone(),
                    score: (score * 1000.0).round() / 1000.0,
                    signals: Some(json!({
                        "lexical_rank": lr,
                        "vector_rank": vr,
                        "personal_distance": dist,
                        "recency": (recency * 100.0).round() / 100.0,
                    })),
                })
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.entity.id.cmp(&b.entity.id)));
        hits.truncate(limit);
        Ok(hits)
    }

    /// Likely duplicate entities (entity alignment candidates).
    pub fn duplicates(
        &self,
        p: &Principal,
        workspace: Option<&str>,
        kind: Option<&str>,
        threshold: f32,
    ) -> Result<Vec<DuplicatePair>> {
        let g = self.reader(p)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(similarity::duplicates(&self.schema(), &g, workspace, kind, threshold, &can))
    }

    /// Data-quality report over what the caller can see.
    pub fn quality(
        &self,
        p: &Principal,
        workspace: Option<&str>,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<QualityReport> {
        let g = self.reader(p)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(quality::report(&self.schema(), &g, workspace, kind, limit, &can))
    }

    fn version_visible(&self, p: &Principal, doc: &Entity) -> bool {
        let g = self.inner.graph.read();
        policy::visible(p, doc, &g)
    }

    /// Version history of an entity, newest first, with changed fields.
    pub fn history(&self, p: &Principal, id: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
        let id = self.resolve_id(id);
        let versions = self.inner.store.versions(&id, limit + 1)?;
        let last_doc = versions.iter().find_map(|v| v.doc.clone());
        match &last_doc {
            Some(doc) if self.version_visible(p, doc) => {}
            _ => return Err(Error::not_found(&id)),
        }
        let mut out = Vec::new();
        for (i, v) in versions.iter().enumerate().take(limit) {
            let older = versions.get(i + 1).and_then(|o| o.doc.as_ref());
            let changed = match (&v.doc, older) {
                (Some(new), Some(old)) => diff_fields(old, new),
                (Some(_), None) if i + 1 >= versions.len() => vec!["created".to_string()],
                (Some(_), None) => vec!["recreated".to_string()],
                (None, _) => vec!["deleted".to_string()],
            };
            out.push(HistoryEntry {
                revision: v.revision,
                ts: v.ts,
                at: fmt_ts(v.ts),
                actor: v.actor.clone(),
                request_id: v.request_id.clone(),
                deleted: v.doc.is_none(),
                state: v.doc.as_ref().and_then(|d| d.state.clone()),
                title: v.doc.as_ref().map(|d| d.title.clone()),
                changed,
            });
        }
        Ok(out)
    }

    /// The entity as it was at `ts` (None if it did not exist or was deleted).
    pub fn as_of(&self, p: &Principal, id: &str, ts: i64) -> Result<Option<Entity>> {
        let id = self.resolve_id(id);
        let versions = self.inner.store.versions(&id, 100_000)?;
        let at = versions.iter().find(|v| v.ts <= ts).and_then(|v| v.doc.clone());
        Ok(at.filter(|doc| self.version_visible(p, doc)))
    }

    /// Entities that changed since `since`, one summary each, newest first.
    pub fn changes(
        &self,
        p: &Principal,
        since: i64,
        workspace: Option<&str>,
        kinds: &[String],
        within: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ChangeSummary>> {
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let versions = self.inner.store.versions_since(since, 50_000)?;
        let mut order: Vec<Id> = Vec::new();
        let mut by_id: HashMap<Id, Vec<VersionRecord>> = HashMap::new();
        for v in versions {
            if !by_id.contains_key(&v.id) {
                order.push(v.id.clone());
            }
            by_id.entry(v.id.clone()).or_default().push(v);
        }
        let scope: Option<HashSet<Id>> = within.map(|root| {
            let root = self.resolve_id(root);
            let g = self.inner.graph.read();
            let mut set: HashSet<Id> = query::scope_set(&self.schema(), &g, &root, 3).into_iter().collect();
            set.insert(root);
            set
        });
        let schema = self.schema();
        let mut out = Vec::new();
        for id in order {
            let vs = &by_id[&id];
            let newest = &vs[0];
            let Some(doc) = vs.iter().find_map(|v| v.doc.as_ref()) else { continue };
            if workspace.is_some_and(|w| doc.workspace != w)
                || (!kinds.is_empty() && !kinds.iter().any(|k| schema.kind_is(&doc.kind, k)))
                || scope.as_ref().is_some_and(|s| !s.contains(&id))
                || !self.version_visible(p, doc)
            {
                continue;
            }
            let oldest = vs.last().expect("non-empty");
            let before = self.inner.store.versions(&id, 64)?.into_iter().find(|v| v.ts < since).and_then(|v| v.doc);
            let created = before.is_none() && oldest.revision <= 1;
            let mut changed: Vec<String> = Vec::new();
            let mut prev = before.clone();
            for v in vs.iter().rev() {
                if let (Some(old), Some(new)) = (&prev, &v.doc) {
                    for f in diff_fields(old, new) {
                        if !changed.contains(&f) {
                            changed.push(f);
                        }
                    }
                }
                prev = v.doc.clone().or(prev);
            }
            let mut actors: Vec<String> = vs.iter().map(|v| v.actor.clone()).collect();
            actors.sort();
            actors.dedup();
            out.push(ChangeSummary {
                id: id.clone(),
                kind: doc.kind.clone(),
                title: doc.label().to_string(),
                from_state: before.as_ref().and_then(|b| b.state.clone()),
                to_state: newest.doc.as_ref().and_then(|d| d.state.clone()),
                created,
                deleted: newest.doc.is_none(),
                changed,
                versions: vs.len(),
                last_ts: newest.ts,
                actors,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub fn path(&self, p: &Principal, from: &str, to: &str, max_depth: usize) -> Result<Option<Vec<Hop>>> {
        let g = self.reader(p)?;
        for id in [from, to] {
            if !g.get(id).is_some_and(|e| policy::visible(p, e, &g)) {
                return Err(Error::not_found(id));
            }
        }
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(query::shortest_path(&self.schema(), &g, from, to, max_depth, &can))
    }

    pub fn path_text(&self, hops: &[Hop]) -> String {
        query::hops_text(&self.inner.graph.read(), hops)
    }

    pub fn infer(&self, p: &Principal, id: &str) -> Result<Vec<Inferred>> {
        let g = self.reader(p)?;
        self.visible_or_404(p, &g, id)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(rules::infer(&self.schema(), &g, id, &can))
    }

    pub fn dependencies(&self, p: &Principal, id: &str, direction: DepDirection, depth: usize) -> Result<DepReport> {
        let g = self.reader(p)?;
        self.visible_or_404(p, &g, id)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(deps::dependencies(&self.schema(), &g, id, direction, depth, &can))
    }

    pub fn impact(&self, p: &Principal, id: &str, depth: usize) -> Result<DepReport> {
        let g = self.reader(p)?;
        self.visible_or_404(p, &g, id)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(deps::impact(&self.schema(), &g, id, depth, &can))
    }

    pub fn root_cause(&self, p: &Principal, id: &str, depth: usize) -> Result<RootCause> {
        let g = self.reader(p)?;
        self.visible_or_404(p, &g, id)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(deps::root_cause(&self.schema(), &g, id, depth, &can))
    }

    pub fn cycles(&self, p: &Principal, workspace: Option<&str>) -> Result<Vec<Vec<Id>>> {
        let g = self.reader(p)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(deps::cycles(&self.schema(), &g, workspace, &can))
    }

    pub fn critical_path(&self, p: &Principal, id: &str) -> Result<CriticalPath> {
        let g = self.reader(p)?;
        self.visible_or_404(p, &g, id)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(deps::critical_path(&self.schema(), &g, id, &can))
    }

    pub fn events(&self, p: &Principal, filter: &EventFilter) -> Result<Vec<Event>> {
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        if let Some(w) = &filter.workspace
            && !p.in_workspace(w)
        {
            return Err(Error::Denied(format!("{} has no access to workspace {w}", p.id)));
        }
        Ok(self.inner.store.events(filter)?.into_iter().filter(|e| p.in_workspace(&e.workspace)).collect())
    }

    pub fn stats(&self) -> Result<OntoStats> {
        let g = self.inner.graph.read();
        Ok(OntoStats {
            schema: format!("{} v{}", self.schema().name, self.schema().version),
            entities: g.len(),
            links: g.link_count(),
            revision: self.revision(),
            kinds: g.kinds().map(|(k, n)| (k.clone(), n)).collect(),
            indexed_docs: self.inner.text.read().len(),
            vectors: match &self.inner.vector_store {
                Some(store) => store.stats()?.vectors.unwrap_or_default() as usize,
                None => self.inner.vectors.read().len(),
            },
            redirects: self.inner.redirects.read().len(),
            store: self.inner.store.stats()?,
        })
    }

    fn visible_or_404(&self, p: &Principal, g: &Graph, id: &str) -> Result<()> {
        match g.get(id) {
            Some(e) if policy::visible(p, e, g) => Ok(()),
            _ => Err(Error::not_found(id)),
        }
    }

    // ── writes ───────────────────────────────────────────────────────────

    pub fn upsert(&self, ctx: &ActionContext, input: EntityInput) -> Result<Entity> {
        let res = self.apply(ctx, vec![Mutation::Upsert(input)])?;
        let id = res.entities.first().ok_or_else(|| Error::Other("upsert produced no entity".into()))?;
        self.get(&Principal::system(), id)
    }

    pub fn patch(&self, ctx: &ActionContext, patch: EntityPatch) -> Result<Entity> {
        let id = patch.id.clone();
        self.apply(ctx, vec![Mutation::Patch(patch)])?;
        self.get(&Principal::system(), &id)
    }

    pub fn link(&self, ctx: &ActionContext, from: &str, rel: &str, to: &str) -> Result<MutationResult> {
        self.apply(ctx, vec![Mutation::Link(LinkInput::new(from, rel, to))])
    }

    pub fn unlink(&self, ctx: &ActionContext, from: &str, rel: &str, to: &str) -> Result<MutationResult> {
        self.apply(ctx, vec![Mutation::Unlink(LinkInput::new(from, rel, to))])
    }

    pub fn delete(&self, ctx: &ActionContext, id: &str) -> Result<MutationResult> {
        self.apply(ctx, vec![Mutation::Delete(EntityRef { id: id.to_string() })])
    }

    pub fn record_event(&self, ctx: &ActionContext, input: EventInput) -> Result<Event> {
        let res = self.apply(ctx, vec![Mutation::Event(input)])?;
        let id = res.events.first().cloned().unwrap_or_default();
        self.inner
            .store
            .events(&EventFilter { limit: Some(64), ..Default::default() })?
            .into_iter()
            .find(|e| e.id == id)
            .ok_or_else(|| Error::Storage("event not readable after write".into()))
    }

    /// Merge `dup` into `keep`: links move over, properties and names fill in,
    /// `dup` is deleted and its id redirects to `keep` from now on.
    pub fn merge(&self, ctx: &ActionContext, keep: &str, dup: &str) -> Result<MergeResult> {
        let (keep, dup) = (self.resolve_id(keep), self.resolve_id(dup));
        if keep == dup {
            return Err(Error::invalid("cannot merge an entity into itself"));
        }
        let (muts, moved) = {
            let g = self.inner.graph.read();
            let ke = g.get(&keep).ok_or_else(|| Error::not_found(&keep))?;
            let de = g.get(&dup).ok_or_else(|| Error::not_found(&dup))?;
            if ke.kind != de.kind || ke.workspace != de.workspace {
                return Err(Error::invalid(format!("{keep} and {dup} must share kind and workspace")));
            }
            let mut muts = Vec::new();
            let mut moved = 0;
            for (rel, other) in g.out_edges(&dup) {
                if other != &keep {
                    let props = g.get_link(&dup, rel, other).map(|l| l.props.clone()).unwrap_or_default();
                    muts.push(Mutation::Link(LinkInput {
                        from: keep.clone(),
                        rel: rel.clone(),
                        to: other.clone(),
                        props,
                    }));
                    moved += 1;
                }
            }
            for (rel, other) in g.in_edges(&dup) {
                if other != &keep {
                    let props = g.get_link(other, rel, &dup).map(|l| l.props.clone()).unwrap_or_default();
                    muts.push(Mutation::Link(LinkInput {
                        from: other.clone(),
                        rel: rel.clone(),
                        to: keep.clone(),
                        props,
                    }));
                    moved += 1;
                }
            }
            let mut aliases: Vec<String> = vec![de.label().to_string(), de.key().to_string()];
            aliases.extend(de.aliases.iter().cloned());
            aliases.retain(|a| !a.is_empty() && *a != ke.title && !ke.aliases.contains(a));
            aliases.dedup();
            muts.push(Mutation::Upsert(EntityInput {
                id: Some(keep.clone()),
                kind: ke.kind.clone(),
                props: de.props.clone(),
                aliases,
                fill_only: true,
                ..Default::default()
            }));
            muts.push(Mutation::Delete(EntityRef { id: dup.clone() }));
            muts.push(Mutation::Event(EventInput {
                action: "entity.merge".into(),
                target: Some(keep.clone()),
                workspace: Some(ke.workspace.clone()),
                payload: json!({ "merged": dup, "links_moved": moved }),
                ..Default::default()
            }));
            (muts, moved)
        };
        let extra = Extra {
            kv: vec![("redirect".into(), dup.clone(), json!(keep))],
            redirects: vec![(dup.clone(), keep.clone())],
            ..Default::default()
        };
        let res = self.apply_inner(ctx, muts, extra)?;
        Ok(MergeResult { kept: keep, merged: dup, links_moved: moved, revision: res.revision })
    }

    /// Apply mutations atomically: all succeed and are persisted, or none are.
    pub fn apply(&self, ctx: &ActionContext, muts: Vec<Mutation>) -> Result<MutationResult> {
        self.apply_inner(ctx, muts, Extra::default())
    }

    pub(crate) fn apply_inner(&self, ctx: &ActionContext, muts: Vec<Mutation>, extra: Extra) -> Result<MutationResult> {
        if muts.is_empty() && extra.is_empty() {
            return Ok(MutationResult { revision: self.revision(), ..Default::default() });
        }
        let (res, ()) = self.transact(ctx, |_, _| Ok((muts.clone(), extra.clone(), ())))?;
        Ok(res)
    }

    /// Validate `muts` against `schema` on a copy-on-write overlay without
    /// writing anything (dry runs).
    pub(crate) fn stage_only(
        &self,
        g: &Graph,
        schema: &Schema,
        ctx: &ActionContext,
        muts: Vec<Mutation>,
    ) -> Result<(WriteBatch, Vec<Id>)> {
        let now = now_ms();
        let mut st = Stage::new(g);
        let mut events = Vec::new();
        let mut touched = Vec::new();
        for m in muts {
            events.append(&mut self.stage(schema, ctx, &mut st, m, now, &mut touched)?);
        }
        let staged = st.finish(events, touched);
        Ok((staged.batch, staged.touched))
    }

    pub(crate) fn commit_locked(
        &self,
        g: &mut Graph,
        ctx: &ActionContext,
        muts: Vec<Mutation>,
        extra: Extra,
    ) -> Result<MutationResult> {
        let now = now_ms();
        let applied = muts.len();
        let schema = extra.schema.as_ref().map(|(s, _)| s.clone()).unwrap_or_else(|| self.schema());
        let staged = {
            let mut st = Stage::new(g);
            let mut events = Vec::new();
            let mut touched = Vec::new();
            for m in muts {
                match self.stage(&schema, ctx, &mut st, m, now, &mut touched) {
                    Ok(mut evs) => events.append(&mut evs),
                    Err(err) => {
                        if let Error::Denied(reason) = &err {
                            self.audit_denied(ctx, reason, now);
                        }
                        return Err(err);
                    }
                }
            }
            st.finish(events, touched)
        };
        let mut staged = staged;
        staged.batch.events.extend(extra.events);
        for e in &staged.batch.put_entities {
            staged.batch.versions.push(VersionRecord {
                id: e.id.clone(),
                revision: e.revision,
                ts: now,
                actor: ctx.principal.id.clone(),
                request_id: Some(ctx.request_id.clone()),
                doc: Some(e.clone()),
            });
        }
        for l in &staged.batch.put_links {
            staged.batch.link_versions.push(crate::store::LinkVersion {
                from: l.from.clone(),
                rel: l.rel.clone(),
                to: l.to.clone(),
                ts: now,
                actor: ctx.principal.id.clone(),
                doc: Some(l.clone()),
            });
        }
        for (from, rel, to) in &staged.batch.del_links {
            staged.batch.link_versions.push(crate::store::LinkVersion {
                from: from.clone(),
                rel: rel.clone(),
                to: to.clone(),
                ts: now,
                actor: ctx.principal.id.clone(),
                doc: None,
            });
        }
        for id in &staged.batch.del_entities {
            staged.batch.versions.push(VersionRecord {
                id: id.clone(),
                revision: g.get(id).map(|e| e.revision + 1).unwrap_or(0),
                ts: now,
                actor: ctx.principal.id.clone(),
                request_id: Some(ctx.request_id.clone()),
                doc: None,
            });
        }
        staged.batch.kv.extend(extra.kv);
        let in_memory_vectors = self.inner.vector_store.is_none();
        if self.inner.store.indexes_vectors() {
            let mut vectors: Vec<(Id, Option<Vec<i8>>)> = staged
                .batch
                .put_entities
                .iter()
                .map(|e| (e.id.clone(), similarity::embed_quantized(&searchable_text(&schema, e))))
                .collect();
            if extra.schema.is_some() {
                let put: HashSet<&str> = staged.batch.put_entities.iter().map(|e| e.id.as_str()).collect();
                for kind in &extra.reindex {
                    for e in g.of_kind(kind).filter(|e| !put.contains(e.id.as_str())) {
                        vectors.push((e.id.clone(), similarity::embed_quantized(&searchable_text(&schema, e))));
                    }
                }
            }
            staged.batch.vectors = vectors;
        }
        if self.inner.store.tracks_commits() {
            staged.batch.expect_seq = Some(self.inner.seq.load(Ordering::SeqCst));
        }
        let seq = self.inner.store.write(&staged.batch)?;
        if staged.batch.changes_state() {
            self.inner.seq.store(seq, Ordering::SeqCst);
        }
        {
            let mut text = self.inner.text.write();
            let mut vectors = self.inner.vectors.write();
            for key in &staged.batch.del_links {
                g.remove_link(key);
            }
            for id in &staged.batch.del_entities {
                g.remove_entity(id);
                text.remove(id);
                vectors.remove(id);
            }
            for e in &staged.batch.put_entities {
                let doc = searchable_text(&schema, e);
                text.upsert(&e.id, &doc);
                if in_memory_vectors {
                    vectors.upsert(&e.id, &doc);
                }
                g.put_entity(e.clone());
            }
            for l in &staged.batch.put_links {
                g.put_link(l.clone());
            }
            if extra.schema.is_some() {
                for kind in &extra.reindex {
                    for e in g.of_kind(kind) {
                        let doc = searchable_text(&schema, e);
                        text.upsert(&e.id, &doc);
                        if in_memory_vectors {
                            vectors.upsert(&e.id, &doc);
                        }
                    }
                }
            }
            if !extra.redirects.is_empty() {
                let mut r = self.inner.redirects.write();
                for (from, to) in extra.redirects {
                    // Re-point older redirects that targeted the merged id.
                    for v in r.values_mut() {
                        if *v == from {
                            *v = to.clone();
                        }
                    }
                    r.insert(from, to);
                }
            }
        }
        let schema_changed = extra.schema.is_some();
        if let Some((schema, version)) = extra.schema {
            *self.inner.active.write() = ActiveSchema { schema, version };
        }
        let links = staged.batch.put_links.len() + staged.batch.del_links.len();
        let wrote = schema_changed
            || !(staged.batch.put_entities.is_empty() && staged.batch.del_entities.is_empty() && links == 0);
        let revision = if wrote { self.inner.revision.fetch_add(1, Ordering::SeqCst) + 1 } else { self.revision() };
        if wrote {
            let change = ChangeSet {
                revision,
                workspace: ctx.workspace.clone(),
                request_id: ctx.request_id.clone(),
                entities: staged.touched.clone(),
                links,
            };
            let listeners = self.inner.listeners.read().clone();
            for l in listeners {
                l(&change);
            }
        }
        Ok(MutationResult {
            applied,
            entities: staged.touched,
            links,
            events: staged.batch.events.iter().map(|e| e.id.clone()).collect(),
            revision,
        })
    }

    pub(crate) fn audit_denied(&self, ctx: &ActionContext, reason: &str, now: i64) {
        let mut ev =
            self.event(ctx, &ctx.workspace, "authorization.denied", None, now, json!({ "reason": reason }), Vec::new());
        ev.outcome = Outcome::Denied;
        let _ = self.inner.store.write(&WriteBatch { events: vec![ev], ..Default::default() });
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn event(
        &self,
        ctx: &ActionContext,
        ws: &str,
        action: &str,
        target: Option<&str>,
        now: i64,
        payload: Value,
        derived_from: Vec<Id>,
    ) -> Event {
        Event {
            id: new_id("evt"),
            ts: now,
            workspace: ws.to_string(),
            actor: ctx.principal.id.clone(),
            action: action.to_string(),
            target: target.map(str::to_string),
            request_id: Some(ctx.request_id.clone()),
            outcome: Outcome::Success,
            intent: ctx.intent.clone(),
            source: ctx.source.clone(),
            derived_from,
            payload,
        }
    }

    fn stage(
        &self,
        schema: &Schema,
        ctx: &ActionContext,
        st: &mut Stage<'_>,
        m: Mutation,
        now: i64,
        touched: &mut Vec<Id>,
    ) -> Result<Vec<Event>> {
        let strict = self.inner.opts.strict;
        match m {
            Mutation::Upsert(input) => {
                let (id, kind) = entity_id(&input)?;
                let id = match st.entity(&id) {
                    Some(_) => id,
                    None => {
                        let target = self.resolve_in(st.g, &id);
                        if st.entity(&target).is_some_and(|e| e.kind == kind) { target } else { id }
                    }
                };
                let existing = st.entity(&id).cloned();
                let ws = match (&existing, &input.workspace) {
                    (Some(ex), Some(w)) if &ex.workspace != w => {
                        return Err(Error::Conflict(format!("{id} already exists in workspace {}", ex.workspace)));
                    }
                    (Some(ex), _) => ex.workspace.clone(),
                    (None, Some(w)) if !w.is_empty() => w.clone(),
                    _ => ctx.workspace.clone(),
                };
                policy::check(&ctx.principal, Access::Write, &ws, existing.as_ref(), st.g)?;
                if let Some(ex) = &existing
                    && ex.kind != kind
                {
                    return Err(Error::Conflict(format!("{id} already exists as a {}", ex.kind)));
                }
                schema.validate_entity(&kind, input.state.as_deref(), &input.props, existing.is_none(), strict)?;
                let provenance = input.provenance.clone().map(|mut p| {
                    if p.at == 0 {
                        p.at = now;
                    }
                    p
                });
                let (entity, action, changed) = match existing {
                    Some(mut e) => {
                        let mut changed: Vec<String> = Vec::new();
                        let fill = input.fill_only;
                        if let Some(t) = &input.title
                            && *t != e.title
                            && (!fill || e.title.is_empty())
                        {
                            e.title = t.clone();
                            changed.push("title".into());
                        }
                        if let Some(s) = &input.state
                            && e.state.as_ref() != Some(s)
                            && (!fill || e.state.is_none())
                        {
                            e.state = Some(s.clone());
                            changed.push("state".into());
                        }
                        if input.replace {
                            let props: Map<String, Value> = input
                                .props
                                .iter()
                                .filter(|(_, v)| !v.is_null())
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect();
                            if props != e.props {
                                e.props = props;
                                changed.push("props".into());
                            }
                        } else {
                            for (k, v) in &input.props {
                                if v.is_null() {
                                    if e.props.remove(k).is_some() {
                                        changed.push(k.clone());
                                    }
                                } else if e.props.get(k) != Some(v) && (!fill || !e.props.contains_key(k)) {
                                    e.props.insert(k.clone(), v.clone());
                                    changed.push(k.clone());
                                }
                            }
                        }
                        for a in &input.aliases {
                            if !e.aliases.contains(a) {
                                e.aliases.push(a.clone());
                                changed.push("aliases".into());
                            }
                        }
                        if let Some(p) = &provenance
                            && (!fill || e.provenance.is_none())
                            && e.provenance.as_ref().map(|x| (&x.source, &x.derived_from, &x.generated_by))
                                != Some((&p.source, &p.derived_from, &p.generated_by))
                        {
                            e.provenance = Some(p.clone());
                            changed.push("provenance".into());
                        }
                        changed.dedup();
                        if !changed.is_empty() {
                            e.revision += 1;
                            e.updated_at = now;
                        }
                        (e, "entity.update", changed)
                    }
                    None => (
                        Entity {
                            id: id.clone(),
                            kind: kind.clone(),
                            workspace: ws.clone(),
                            title: input.title.clone().unwrap_or_default(),
                            state: input.state.clone(),
                            props: input
                                .props
                                .iter()
                                .filter(|(_, v)| !v.is_null())
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                            aliases: input.aliases.clone(),
                            provenance: provenance.clone(),
                            created_at: now,
                            updated_at: now,
                            revision: 1,
                        },
                        "entity.create",
                        Vec::new(),
                    ),
                };
                let mut events = Vec::new();
                if action == "entity.create" || !changed.is_empty() {
                    let derived = entity.provenance.as_ref().map(|p| p.derived_from.clone()).unwrap_or_default();
                    let payload =
                        json!({ "kind": kind, "title": entity.title, "state": entity.state, "changed": changed });
                    events.push(self.event(ctx, &ws, action, Some(&id), now, payload, derived));
                    st.put_entity(entity);
                }
                touched.push(id.clone());
                for spec in input.links {
                    let link = LinkInput { from: id.clone(), rel: spec.rel, to: spec.to, props: spec.props };
                    events.extend(self.stage_link(schema, ctx, st, link, now)?);
                }
                // Provenance becomes graph edges when the referenced things exist.
                if let Some(p) = provenance {
                    let mut edges: Vec<(&str, &str)> =
                        p.derived_from.iter().map(|s| ("derived_from", s.as_str())).collect();
                    if let Some(gb) = &p.generated_by {
                        edges.push(("generated_by", gb));
                    }
                    for (rel, target) in edges {
                        let ok = schema.link_type(rel).is_some()
                            && st.entity(target).is_some_and(|t| {
                                st.entity(&id)
                                    .is_some_and(|me| schema.validate_link(&me.kind, rel, &t.kind, strict).is_ok())
                            });
                        if ok {
                            events.extend(self.stage_link(
                                schema,
                                ctx,
                                st,
                                LinkInput::new(id.clone(), rel, target),
                                now,
                            )?);
                        }
                    }
                }
                Ok(events)
            }
            Mutation::Patch(patch) => {
                let pid =
                    if st.entity(&patch.id).is_some() { patch.id.clone() } else { self.resolve_in(st.g, &patch.id) };
                let ex = st.entity(&pid).cloned().ok_or_else(|| Error::not_found(&patch.id))?;
                let mut props = patch.props;
                for k in patch.unset {
                    props.insert(k, Value::Null);
                }
                let input = EntityInput {
                    id: Some(ex.id.clone()),
                    kind: ex.kind.clone(),
                    title: patch.title,
                    state: patch.state,
                    props,
                    ..Default::default()
                };
                self.stage(schema, ctx, st, Mutation::Upsert(input), now, touched)
            }
            Mutation::Delete(mut r) => {
                if st.entity(&r.id).is_none() {
                    r.id = self.resolve_in(st.g, &r.id);
                }
                let ex = st.entity(&r.id).cloned().ok_or_else(|| Error::not_found(&r.id))?;
                policy::check(&ctx.principal, Access::Delete, &ex.workspace, Some(&ex), st.g)?;
                st.del_entity(&r.id);
                touched.push(r.id.clone());
                Ok(vec![self.event(
                    ctx,
                    &ex.workspace,
                    "entity.delete",
                    Some(&r.id),
                    now,
                    json!({ "kind": ex.kind, "title": ex.title }),
                    Vec::new(),
                )])
            }
            Mutation::Link(input) => self.stage_link(schema, ctx, st, input, now),
            Mutation::Unlink(input) => {
                let (base, dir) = schema.resolve_rel_lenient(&input.rel);
                let (mut from, mut to) = if dir == Dir::In { (input.to, input.from) } else { (input.from, input.to) };
                if schema.link_type(&base).is_some_and(|lt| lt.symmetric) && from > to {
                    std::mem::swap(&mut from, &mut to);
                }
                let fe = st.entity(&from).cloned().ok_or_else(|| Error::not_found(&from))?;
                policy::check(&ctx.principal, Access::Write, &fe.workspace, Some(&fe), st.g)?;
                let key = (from.clone(), base.clone(), to.clone());
                if st.del_link(key) {
                    Ok(vec![self.event(
                        ctx,
                        &fe.workspace,
                        "link.delete",
                        Some(&from),
                        now,
                        json!({ "rel": base, "to": to }),
                        Vec::new(),
                    )])
                } else {
                    Ok(Vec::new())
                }
            }
            Mutation::Event(input) => {
                if input.action.trim().is_empty() {
                    return Err(Error::invalid("event action is required"));
                }
                let ws = input.workspace.clone().filter(|w| !w.is_empty()).unwrap_or_else(|| ctx.workspace.clone());
                policy::check(&ctx.principal, Access::Write, &ws, None, st.g)?;
                let mut payload = input.payload;
                let actor = input.actor.unwrap_or_else(|| ctx.principal.id.clone());
                if actor != ctx.principal.id {
                    match &mut payload {
                        Value::Object(m) => {
                            m.insert("recorded_by".into(), json!(ctx.principal.id));
                        }
                        Value::Null => payload = json!({ "recorded_by": ctx.principal.id }),
                        _ => {}
                    }
                }
                let mut ev = self.event(
                    ctx,
                    &ws,
                    &input.action,
                    input.target.as_deref(),
                    input.ts.unwrap_or(now),
                    payload,
                    input.derived_from,
                );
                ev.actor = actor;
                ev.outcome = input.outcome;
                if input.intent.is_some() {
                    ev.intent = input.intent;
                }
                if input.source.is_some() {
                    ev.source = input.source;
                }
                Ok(vec![ev])
            }
        }
    }

    fn stage_link(
        &self,
        schema: &Schema,
        ctx: &ActionContext,
        st: &mut Stage<'_>,
        input: LinkInput,
        now: i64,
    ) -> Result<Vec<Event>> {
        let (base, dir) = schema.resolve_rel_lenient(&input.rel);
        if base.is_empty() {
            return Err(Error::invalid("relation name is required"));
        }
        let (mut from, mut to) = if dir == Dir::In { (input.to, input.from) } else { (input.from, input.to) };
        if st.entity(&from).is_none() {
            from = self.resolve_in(st.g, &from);
        }
        if st.entity(&to).is_none() {
            to = self.resolve_in(st.g, &to);
        }
        if from == to {
            return Err(Error::invalid(format!("{from} cannot link to itself")));
        }
        let fe = st.entity(&from).cloned().ok_or_else(|| Error::not_found(format!("link source {from}")))?;
        let te = st.entity(&to).cloned().ok_or_else(|| Error::not_found(format!("link target {to}")))?;
        if fe.workspace != te.workspace {
            return Err(Error::invalid(format!("{from} and {to} are in different workspaces")));
        }
        policy::check(&ctx.principal, Access::Write, &fe.workspace, Some(&fe), st.g)?;
        policy::check(&ctx.principal, Access::Read, &te.workspace, Some(&te), st.g)?;
        schema.validate_link(&fe.kind, &base, &te.kind, self.inner.opts.strict)?;
        let lt = schema.link_type(&base);
        if lt.is_some_and(|l| l.symmetric) && from > to {
            std::mem::swap(&mut from, &mut to);
        }
        let key = (from.clone(), base.clone(), to.clone());
        let existing = st.link(&key).cloned();
        if existing.as_ref().is_some_and(|l| l.props == input.props) {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        if lt.is_some_and(|l| l.cardinality == Cardinality::One) {
            for k in st.out_keys(&from, &base) {
                if k.2 != to {
                    let old = k.2.clone();
                    st.del_link(k);
                    events.push(self.event(
                        ctx,
                        &fe.workspace,
                        "link.delete",
                        Some(&from),
                        now,
                        json!({ "rel": base, "to": old, "replaced_by": to }),
                        Vec::new(),
                    ));
                }
            }
        }
        st.put_link(Link {
            from: from.clone(),
            rel: base.clone(),
            to: to.clone(),
            props: input.props,
            workspace: fe.workspace.clone(),
            created_at: existing.map(|l| l.created_at).unwrap_or(now),
        });
        events.push(self.event(
            ctx,
            &fe.workspace,
            "link.create",
            Some(&from),
            now,
            json!({ "rel": base, "to": to }),
            Vec::new(),
        ));
        Ok(events)
    }
}

/// Derive `(id, kind)` from an input: `id`, else `kind:key`, else generated.
pub fn entity_id(input: &EntityInput) -> Result<(Id, String)> {
    let prefix = input.id.as_deref().and_then(|id| id.split_once(':')).map(|(k, _)| k.to_string());
    let kind = if !input.kind.is_empty() {
        input.kind.clone()
    } else if let Some(k) = prefix {
        k
    } else {
        return Err(Error::invalid("kind is required (or an id such as 'issue:421')"));
    };
    if !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(Error::invalid(format!("kind '{kind}' may only contain letters, digits, '_' and '-'")));
    }
    let id = match (&input.id, &input.key) {
        (Some(id), _) if id.contains(':') => id.clone(),
        (Some(id), _) if !id.is_empty() => format!("{kind}:{id}"),
        (_, Some(key)) if !key.is_empty() => format!("{kind}:{key}"),
        _ => format!("{kind}:{}", &new_id("x")[2..]),
    };
    if id.split_once(':').map(|(k, _)| k) != Some(kind.as_str()) {
        return Err(Error::invalid(format!("id {id} must start with '{kind}:'")));
    }
    if id.len() > 256 || id.ends_with(':') {
        return Err(Error::invalid(format!("invalid id '{id}'")));
    }
    Ok((id, kind))
}

/// Text indexed for search: kind, title, key, aliases and searchable props.
pub fn searchable_text(schema: &Schema, e: &Entity) -> String {
    let mut s = format!("{} {} {}", e.kind, e.title, e.key().replace(['-', '_'], " "));
    for a in &e.aliases {
        s.push(' ');
        s.push_str(a);
    }
    for (k, v) in &e.props {
        if matches!(k.as_str(), "visibility" | "acl") {
            continue;
        }
        let include = schema.prop_def(&e.kind, k).is_none_or(|d| {
            d.searchable || matches!(d.ty, PropType::Text | PropType::String | PropType::Enum | PropType::List)
        });
        if !include {
            continue;
        }
        match v {
            Value::String(t) => {
                s.push(' ');
                s.push_str(truncate(t, 4000));
            }
            Value::Array(items) => {
                for t in items.iter().filter_map(Value::as_str) {
                    s.push(' ');
                    s.push_str(t);
                }
            }
            _ => {}
        }
    }
    s
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Copy-on-write overlay used to validate a batch before anything is persisted.
struct Stage<'g> {
    g: &'g Graph,
    ents: HashMap<Id, Option<Entity>>,
    order: Vec<Id>,
    link_put: HashMap<LinkKey, Link>,
    link_del: HashSet<LinkKey>,
}

struct Staged {
    batch: WriteBatch,
    touched: Vec<Id>,
}

impl<'g> Stage<'g> {
    fn new(g: &'g Graph) -> Self {
        Self { g, ents: HashMap::new(), order: Vec::new(), link_put: HashMap::new(), link_del: HashSet::new() }
    }

    fn entity(&self, id: &str) -> Option<&Entity> {
        match self.ents.get(id) {
            Some(slot) => slot.as_ref(),
            None => self.g.get(id),
        }
    }

    fn put_entity(&mut self, e: Entity) {
        if !self.ents.contains_key(&e.id) {
            self.order.push(e.id.clone());
        }
        self.ents.insert(e.id.clone(), Some(e));
    }

    fn del_entity(&mut self, id: &str) {
        for key in self.g.incident_keys(id) {
            self.link_del.insert(key);
        }
        self.link_put.retain(|k, _| k.0 != id && k.2 != id);
        if !self.ents.contains_key(id) {
            self.order.push(id.to_string());
        }
        self.ents.insert(id.to_string(), None);
    }

    fn link(&self, key: &LinkKey) -> Option<&Link> {
        if self.link_del.contains(key) {
            return None;
        }
        self.link_put.get(key).or_else(|| self.g.get_link(&key.0, &key.1, &key.2))
    }

    fn out_keys(&self, from: &str, rel: &str) -> Vec<LinkKey> {
        let mut keys: Vec<LinkKey> = self
            .g
            .out_edges(from)
            .iter()
            .filter(|(r, _)| r == rel)
            .map(|(r, t)| (from.to_string(), r.clone(), t.clone()))
            .filter(|k| !self.link_del.contains(k))
            .collect();
        keys.extend(self.link_put.keys().filter(|k| k.0 == from && k.1 == rel).cloned());
        keys.sort();
        keys.dedup();
        keys
    }

    fn put_link(&mut self, link: Link) {
        let key = (link.from.clone(), link.rel.clone(), link.to.clone());
        self.link_del.remove(&key);
        self.link_put.insert(key, link);
    }

    /// Returns whether the link existed.
    fn del_link(&mut self, key: LinkKey) -> bool {
        let existed = self.link(&key).is_some();
        self.link_put.remove(&key);
        if self.g.get_link(&key.0, &key.1, &key.2).is_some() {
            self.link_del.insert(key);
        }
        existed
    }

    fn finish(mut self, events: Vec<Event>, touched: Vec<Id>) -> Staged {
        let mut batch = WriteBatch::default();
        for id in &self.order {
            match self.ents.remove(id) {
                Some(Some(e)) => batch.put_entities.push(e),
                Some(None) if self.g.contains(id) => batch.del_entities.push(id.clone()),
                _ => {}
            }
        }
        let mut puts: Vec<Link> = self.link_put.into_values().collect();
        puts.sort_by(|a, b| (&a.from, &a.rel, &a.to).cmp(&(&b.from, &b.rel, &b.to)));
        batch.put_links = puts;
        let mut dels: Vec<LinkKey> = self.link_del.into_iter().collect();
        dels.sort();
        batch.del_links = dels;
        batch.events = events;
        let mut seen = HashSet::new();
        let touched = touched.into_iter().filter(|id| seen.insert(id.clone())).collect();
        Staged { batch, touched }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Role;

    fn onto() -> (Ontology, ActionContext) {
        let o = Ontology::in_memory(Schema::enterprise());
        let ctx = ActionContext::new(Principal::admin("tester"), "acme");
        o.apply(
            &ctx,
            vec![
                Mutation::Upsert(EntityInput::new("team", "payments", "Payments Team")),
                Mutation::Upsert(EntityInput::new("person", "sarah", "Sarah Lee").link("manages", "team:payments")),
                Mutation::Upsert(
                    EntityInput::new("person", "kenji", "Kenji Tanaka").link("member_of", "team:payments"),
                ),
                Mutation::Upsert(
                    EntityInput::new("service", "payment-api", "Payment API")
                        .state("degraded")
                        .link("owned_by", "team:payments"),
                ),
                Mutation::Upsert(
                    EntityInput::new("issue", "421", "Payment timeout")
                        .state("open")
                        .prop("priority", "high")
                        .link("assigned_to", "person:kenji")
                        .link("affects", "service:payment-api"),
                ),
            ],
        )
        .unwrap();
        (o, ctx)
    }

    #[test]
    fn writes_are_canonical_and_audited() {
        let (o, ctx) = onto();
        let admin = Principal::admin("x");
        // owned_by was stored canonically as team -owns-> service
        let n = o.neighbors(&admin, "team:payments", Some("owns")).unwrap();
        assert_eq!(n[0].1.id, "service:payment-api");
        let events = o.events(&admin, &EventFilter { target: Some("issue:421".into()), ..Default::default() }).unwrap();
        assert!(events.iter().any(|e| e.action == "entity.create" && e.actor == "tester"));
        assert!(events.iter().all(|e| e.request_id.as_deref() == Some(ctx.request_id.as_str())));
    }

    #[test]
    fn batch_is_atomic() {
        let (o, ctx) = onto();
        let before = o.stats().unwrap().entities;
        let err = o
            .apply(
                &ctx,
                vec![
                    Mutation::Upsert(EntityInput::new("task", "t1", "New task")),
                    Mutation::Link(LinkInput::new("task:t1", "blocked_by", "issue:999")),
                ],
            )
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
        assert_eq!(o.stats().unwrap().entities, before);
    }

    #[test]
    fn upsert_merges_and_bumps_revision() {
        let (o, ctx) = onto();
        let e = o
            .upsert(
                &ctx,
                EntityInput { id: Some("issue:421".into()), state: Some("blocked".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!(e.state.as_deref(), Some("blocked"));
        assert_eq!(e.prop_str("priority"), Some("high"));
        assert_eq!(e.revision, 2);
        let same = o
            .upsert(
                &ctx,
                EntityInput { id: Some("issue:421".into()), state: Some("blocked".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!(same.revision, 2, "no-op update keeps revision");
    }

    #[test]
    fn fill_only_never_overwrites_curated_fields() {
        let (o, ctx) = onto();
        let e = o
            .upsert(
                &ctx,
                EntityInput {
                    id: Some("person:kenji".into()),
                    title: Some("kenji".into()),
                    fill_only: true,
                    ..Default::default()
                }
                .prop("role", "bot")
                .prop("timezone", "Asia/Tokyo"),
            )
            .unwrap();
        assert_eq!(e.title, "Kenji Tanaka");
        assert_eq!(e.prop_str("timezone"), Some("Asia/Tokyo"));
        let fresh = o
            .upsert(
                &ctx,
                EntityInput {
                    id: Some("person:new".into()),
                    title: Some("new".into()),
                    fill_only: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(fresh.title, "new");
    }

    #[test]
    fn workspace_isolation_and_roles() {
        let (o, _) = onto();
        let outsider = Principal::member("eve", "other");
        assert!(o.get(&outsider, "issue:421").is_err());
        let viewer = Principal::viewer("val", "acme");
        assert!(o.get(&viewer, "issue:421").is_ok());
        let err = o.upsert(&ActionContext::new(viewer, "acme"), EntityInput::new("task", "x", "X")).unwrap_err();
        assert!(matches!(err, Error::Denied(_)));
        let denied = o
            .events(&Principal::admin("a"), &EventFilter { action: Some("authorization".into()), ..Default::default() })
            .unwrap();
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].outcome, Outcome::Denied);
    }

    #[test]
    fn restricted_entities_follow_acl() {
        let (o, ctx) = onto();
        o.upsert(
            &ctx,
            EntityInput::new("knowledge", "salaries", "Salary bands")
                .prop("visibility", "restricted")
                .prop("acl", serde_json::json!(["team:payments"])),
        )
        .unwrap();
        let kenji = Principal::new("kenji", vec![Role::Member], vec!["acme".into()]).with_entity("person:kenji");
        let eve = Principal::member("eve", "acme");
        assert!(o.get(&kenji, "knowledge:salaries").is_ok());
        assert!(o.get(&eve, "knowledge:salaries").is_err());
        assert!(o.search(&eve, "salary", &[], None, 5).unwrap().is_empty());
    }

    #[test]
    fn inference_and_root_cause() {
        let (o, _) = onto();
        let admin = Principal::admin("x");
        let inferred = o.infer(&admin, "issue:421").unwrap();
        assert!(inferred.iter().any(|i| i.rel == "handled_by_team" && i.to == "team:payments"));
        assert!(inferred.iter().any(|i| i.rel == "escalate_to" && i.to == "person:sarah"));
        let rc = o.root_cause(&admin, "issue:421", 4).unwrap();
        assert_eq!(rc.chains[0].culprit.id, "service:payment-api");
        let path = o.path(&admin, "person:kenji", "service:payment-api", 4).unwrap().unwrap();
        assert_eq!(path.len(), 2);
    }

    #[test]
    fn cardinality_one_replaces() {
        let o = Ontology::in_memory({
            let mut s = Schema::enterprise();
            s.extend_toml("[link_types.primary_owner]\ncardinality = 'one'\n").unwrap();
            s
        });
        let ctx = ActionContext::new(Principal::admin("t"), "acme");
        for k in ["a", "b", "x"] {
            o.upsert(&ctx, EntityInput::new("person", k, k)).unwrap();
        }
        o.link(&ctx, "person:x", "primary_owner", "person:a").unwrap();
        o.link(&ctx, "person:x", "primary_owner", "person:b").unwrap();
        let n = o.neighbors(&Principal::admin("t"), "person:x", Some("primary_owner")).unwrap();
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].1.id, "person:b");
    }
}
