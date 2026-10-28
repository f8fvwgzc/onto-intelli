//! Durable storage behind the in-memory graph.
//!
//! The graph is the hot read path; a [`Store`] persists entities and links,
//! keeps the append-only event log, and offers a small namespaced KV used by
//! memory, conversations and caches.
//!
//! `PgStore` (PostgreSQL 16+ with pgvector) is the durable store, shared by
//! any number of processes; [`MemoryStore`] is for tests and demos.
//!
//! A shared store keeps a **commit log**: every write that changes
//! entities, links or KV gets the next sequence number and records what it
//! touched. Writers commit optimistically against the sequence they last
//! saw ([`WriteBatch::expect_seq`]); a writer that is behind gets
//! [`Error::Stale`](crate::Error::Stale), catches up from
//! [`Store::commits_since`] and retries. Readers keep their in-memory indexes
//! as caches of the store and refresh them the same way, prompted by
//! [`Store::subscribe`] notices (LISTEN/NOTIFY on PostgreSQL).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;
use crate::graph::LinkKey;
use crate::model::{Entity, Event, EventFilter, Id, Link};
use crate::time::now_ms;
use crate::trace::{
    self, MetricsQuery, ScoreRow, SessionPage, SessionQuery, TraceDetail, TraceMetrics, TracePage, TraceQuery,
    TraceRecord, TraceRow,
};

#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub use postgres::{PgOptions, PgShared, PgStore};

/// KV namespaces whose standalone writes other processes are told about
/// (their readers keep them in memory). Everything committed inside a
/// [`WriteBatch`] is in the commit log anyway.
pub const REPLICATED_KV: &[&str] = &["memory"];

#[derive(Debug, Default)]
pub struct Snapshot {
    pub entities: Vec<Entity>,
    pub links: Vec<Link>,
    /// Commit sequence the snapshot is consistent with.
    pub seq: u64,
}

/// What one committed write touched (entities and links put or deleted, KV keys).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CommitRecord {
    pub seq: u64,
    pub ts: i64,
    /// The process that wrote it.
    pub origin: String,
    #[serde(default)]
    pub entities: Vec<Id>,
    #[serde(default)]
    pub links: Vec<LinkKey>,
    #[serde(default)]
    pub kv: Vec<(String, String)>,
    /// Too much changed to list (bulk import): followers reload everything.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reload: bool,
}

/// A change made by another process, delivered to [`Store::subscribe`] callbacks.
#[derive(Clone, Debug, PartialEq)]
pub enum StoreNotice {
    /// A commit with this sequence number is durable.
    Commit { seq: u64, origin: String },
    /// A standalone write to a [`REPLICATED_KV`] namespace.
    Kv { ns: String, key: String, origin: String },
    /// Notices may have been missed (listener reconnected): catch up fully.
    Resync,
}

pub type NoticeFn = std::sync::Arc<dyn Fn(StoreNotice) + Send + Sync>;

/// One historical state of an entity (`doc = None` marks a deletion).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VersionRecord {
    pub id: Id,
    pub revision: u64,
    pub ts: i64,
    #[serde(default)]
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<Entity>,
}

/// One historical state of a link (`doc = None` marks a deletion).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkVersion {
    pub from: Id,
    pub rel: String,
    pub to: Id,
    pub ts: i64,
    #[serde(default)]
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<Link>,
}

/// Atomic unit of persistence.
#[derive(Debug, Default)]
pub struct WriteBatch {
    pub put_entities: Vec<Entity>,
    pub del_entities: Vec<Id>,
    pub put_links: Vec<Link>,
    pub del_links: Vec<LinkKey>,
    pub events: Vec<Event>,
    /// Entity history written in the same transaction.
    pub versions: Vec<VersionRecord>,
    /// Link history written in the same transaction.
    pub link_versions: Vec<LinkVersion>,
    /// KV writes that must commit together with the graph change (ns, key, value).
    pub kv: Vec<(String, String, Value)>,
    /// For stores that index vectors ([`Store::indexes_vectors`]): put
    /// entities whose searchable text did not change keep their vector;
    /// every other put entity is marked for re-embedding.
    pub keep_vectors: Vec<Id>,
    /// Other entities to re-embed (their searchable text changed with the schema).
    pub reembed: Vec<Id>,
    /// Optimistic concurrency: the commit sequence this batch was staged
    /// against. When the store's head differs, the write is refused with
    /// `Error::Stale` and nothing is written.
    pub expect_seq: Option<u64>,
}

impl WriteBatch {
    /// Does this batch change state other processes cache (as opposed to
    /// only appending events and versions)?
    pub fn changes_state(&self) -> bool {
        !(self.put_entities.is_empty()
            && self.del_entities.is_empty()
            && self.put_links.is_empty()
            && self.del_links.is_empty()
            && self.kv.is_empty())
    }

    /// The commit record of this batch (sequence and time filled in by the store).
    pub fn commit_record(&self, seq: u64, origin: &str) -> CommitRecord {
        let mut entities: Vec<Id> =
            self.put_entities.iter().map(|e| e.id.clone()).chain(self.del_entities.iter().cloned()).collect();
        entities.sort();
        entities.dedup();
        let mut links: Vec<LinkKey> = self
            .put_links
            .iter()
            .map(|l| (l.from.clone(), l.rel.clone(), l.to.clone()))
            .chain(self.del_links.iter().cloned())
            .collect();
        links.sort();
        links.dedup();
        let mut kv: Vec<(String, String)> = self.kv.iter().map(|(n, k, _)| (n.clone(), k.clone())).collect();
        kv.sort();
        kv.dedup();
        CommitRecord { seq, ts: now_ms(), origin: origin.to_string(), entities, links, kv, reload: false }
    }

    pub fn is_empty(&self) -> bool {
        self.put_entities.is_empty()
            && self.del_entities.is_empty()
            && self.put_links.is_empty()
            && self.del_links.is_empty()
            && self.events.is_empty()
            && self.versions.is_empty()
            && self.link_versions.is_empty()
            && self.kv.is_empty()
            && self.reembed.is_empty()
    }
}

/// Narrows a [`Store::vector_search`].
#[derive(Clone, Debug, Default)]
pub struct VectorFilter {
    pub workspace: Option<String>,
    /// Concrete kinds (empty: all).
    pub kinds: Vec<String>,
}

/// Computes the vectors of entities a store has none for yet.
pub type EmbedFn<'a> = &'a dyn Fn(&[Entity]) -> Vec<Option<Vec<i8>>>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoreStats {
    pub backend: String,
    pub events: u64,
    pub kv_entries: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Entity vectors the store indexes (an estimate on large tables).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vectors: Option<u64>,
}

pub trait Store: Send + Sync {
    /// Everything needed to rebuild the graph at startup.
    fn load(&self) -> Result<Snapshot>;
    /// Commit atomically; returns the head sequence afterwards. Fails with
    /// `Error::Stale` when `batch.expect_seq` is set and the head moved.
    fn write(&self, batch: &WriteBatch) -> Result<u64>;
    /// Does this store keep a commit log other processes can follow?
    fn tracks_commits(&self) -> bool {
        false
    }
    /// Is the store shared by other processes that may write concurrently
    /// (so a background follower should keep the caches fresh)?
    fn shared(&self) -> bool {
        false
    }
    /// Latest committed sequence.
    fn head(&self) -> Result<u64> {
        Ok(0)
    }
    /// Commits after `seq`, ascending, at most `limit`. A first record later
    /// than `seq + 1` means older commits were pruned: reload everything.
    fn commits_since(&self, _seq: u64, _limit: usize) -> Result<Vec<CommitRecord>> {
        Ok(Vec::new())
    }
    /// Current rows of these entities (missing ones were deleted).
    fn get_entities(&self, _ids: &[Id]) -> Result<Vec<Entity>> {
        Ok(Vec::new())
    }
    /// Current rows of these links (missing ones were deleted).
    fn get_links(&self, _keys: &[LinkKey]) -> Result<Vec<Link>> {
        Ok(Vec::new())
    }
    /// Be told when another process commits (best effort; callers also poll).
    fn subscribe(&self, _f: NoticeFn) -> Result<()> {
        Ok(())
    }
    /// This process's id in commit records.
    fn origin(&self) -> String {
        String::new()
    }
    /// Load many entities and links fast (bulk import, benchmarks): no
    /// events, no versions, one commit. Existing rows with the same keys are
    /// replaced.
    fn bulk_import(&self, entities: &[Entity], links: &[Link]) -> Result<u64> {
        let mut seq = 0;
        for chunk in entities.chunks(5_000) {
            seq = self.write(&WriteBatch { put_entities: chunk.to_vec(), ..Default::default() })?;
        }
        for chunk in links.chunks(5_000) {
            seq = self.write(&WriteBatch { put_links: chunk.to_vec(), ..Default::default() })?;
        }
        Ok(seq)
    }
    /// Newest first.
    fn events(&self, filter: &EventFilter) -> Result<Vec<Event>>;
    fn kv_put(&self, ns: &str, key: &str, value: &Value, expires_at: Option<i64>) -> Result<()>;
    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<Value>>;
    fn kv_delete(&self, ns: &str, key: &str) -> Result<()>;
    /// All live entries of a namespace whose key starts with `prefix`, key order.
    fn kv_scan(&self, ns: &str, prefix: &str) -> Result<Vec<(String, Value)>>;
    /// Delete at most `limit` expired entries of a namespace; how many.
    /// Reads skip them anyway: this only keeps a namespace that nothing
    /// reads back whole from growing. Stores that keep no expired rows need not.
    fn kv_purge_expired(&self, ns: &str, limit: usize) -> Result<u64> {
        let _ = (ns, limit);
        Ok(0)
    }
    fn stats(&self) -> Result<StoreStats>;
    /// Version history of one entity, newest first.
    fn versions(&self, id: &str, limit: usize) -> Result<Vec<VersionRecord>>;
    /// Versions written at or after `since`, newest first.
    fn versions_since(&self, since: i64, limit: usize) -> Result<Vec<VersionRecord>>;
    /// Link versions touching `id` (either end) written at or before
    /// `until`, newest first.
    fn link_history(&self, _id: &str, _until: i64, _limit: usize) -> Result<Vec<LinkVersion>> {
        Ok(Vec::new())
    }
    /// Does the store index entity vectors itself (pgvector)? Then the
    /// engine keeps no vector index in memory: writes mark entities for
    /// embedding ([`WriteBatch::keep_vectors`], [`WriteBatch::reembed`]), a
    /// background thread computes them right after the commit
    /// ([`backfill_vectors`](Self::backfill_vectors)), and searches ask
    /// [`vector_search`](Self::vector_search). Writes never wait for the
    /// vector index.
    fn indexes_vectors(&self) -> bool {
        false
    }
    /// Are entities waiting for their vector?
    fn vectors_pending(&self) -> Result<bool> {
        Ok(false)
    }
    /// Entities nearest to `query` (a quantized embedding) by cosine
    /// similarity, best first: at most `limit` of (id, similarity).
    fn vector_search(&self, _query: &[i8], _limit: usize, _filter: &VectorFilter) -> Result<Vec<(Id, f32)>> {
        Ok(Vec::new())
    }
    /// Store vectors for entities that have none yet (rows from a bulk
    /// import or written before vectors were indexed), computing them with
    /// `embed`. Returns how many entities it embedded. One process at a time
    /// does this; the others return 0 at once.
    fn backfill_vectors(&self, _embed: EmbedFn<'_>) -> Result<usize> {
        Ok(0)
    }

    // ── traces (docs/CONTROL_PLANE.md section 9); stores without them refuse ──

    /// Write traces with their spans (each trace with its spans in one
    /// transaction). A trace whose id is already stored is skipped.
    fn trace_write(&self, _batch: &[TraceRecord]) -> Result<()> {
        Err(trace::unsupported())
    }
    /// Traces matching `q`, newest first, without bodies.
    fn trace_list(&self, _q: &TraceQuery) -> Result<TracePage> {
        Err(trace::unsupported())
    }
    /// One trace with its spans and scores.
    fn trace_get(&self, _id: &str) -> Result<Option<TraceDetail>> {
        Err(trace::unsupported())
    }
    /// A session's traces, oldest first, with their bodies (at most `limit`, ≤ 200).
    fn trace_session(&self, _id: &str, _limit: usize) -> Result<Vec<TraceRow>> {
        Err(trace::unsupported())
    }
    /// Aggregates over a window.
    fn trace_metrics(&self, _q: &MetricsQuery) -> Result<TraceMetrics> {
        Err(trace::unsupported())
    }
    /// One row per session, the most recently active first.
    fn trace_sessions(&self, _q: &SessionQuery) -> Result<SessionPage> {
        Err(trace::unsupported())
    }
    fn score_write(&self, _s: &ScoreRow) -> Result<()> {
        Err(trace::unsupported())
    }
    /// Delete traces, spans and scores that started before `before_ms`, in
    /// batches, for at most about `budget`. Returns the rows deleted.
    fn trace_vacuum(&self, _before_ms: i64, _budget: Duration) -> Result<u64> {
        Err(trace::unsupported())
    }
}

pub(crate) fn event_matches(e: &Event, f: &EventFilter) -> bool {
    f.workspace.as_ref().is_none_or(|w| &e.workspace == w)
        && f.actor.as_ref().is_none_or(|a| &e.actor == a)
        && f.target.as_ref().is_none_or(|t| e.target.as_ref() == Some(t))
        && f.action.as_ref().is_none_or(|a| {
            let prefix = a.trim_end_matches('.');
            e.action == prefix || e.action.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('.'))
        })
        && f.since.is_none_or(|s| e.ts >= s)
        && f.until.is_none_or(|u| e.ts <= u)
}

/// Volatile store: the graph itself holds entities and links; events are kept
/// in a bounded ring buffer. Ideal for tests, demos and embedded caches.
pub struct MemoryStore {
    inner: Mutex<MemInner>,
    max_events: usize,
    traces: Mutex<MemTraces>,
}

/// The newest [`trace::MEMORY_TRACES`] traces, and scores.
#[derive(Default)]
struct MemTraces {
    records: VecDeque<TraceRecord>,
    ids: HashSet<String>,
    scores: VecDeque<ScoreRow>,
}

#[derive(Default)]
struct MemInner {
    seq: u64,
    events: VecDeque<Event>,
    versions: VecDeque<VersionRecord>,
    link_versions: VecDeque<LinkVersion>,
    kv: HashMap<String, std::collections::BTreeMap<String, (Value, Option<i64>)>>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::with_event_capacity(100_000)
    }
}

impl MemoryStore {
    pub fn with_event_capacity(max_events: usize) -> Self {
        Self { inner: Mutex::new(MemInner::default()), max_events, traces: Mutex::new(MemTraces::default()) }
    }
}

impl Store for MemoryStore {
    fn load(&self) -> Result<Snapshot> {
        Ok(Snapshot::default())
    }

    fn write(&self, batch: &WriteBatch) -> Result<u64> {
        let mut inner = self.inner.lock();
        for e in &batch.events {
            if inner.events.len() >= self.max_events {
                inner.events.pop_front();
            }
            inner.events.push_back(e.clone());
        }
        for v in &batch.versions {
            if inner.versions.len() >= self.max_events {
                inner.versions.pop_front();
            }
            inner.versions.push_back(v.clone());
        }
        for v in &batch.link_versions {
            if inner.link_versions.len() >= self.max_events {
                inner.link_versions.pop_front();
            }
            inner.link_versions.push_back(v.clone());
        }
        for (ns, key, value) in &batch.kv {
            inner.kv.entry(ns.clone()).or_default().insert(key.clone(), (value.clone(), None));
        }
        if batch.changes_state() {
            inner.seq += 1;
        }
        Ok(inner.seq)
    }

    fn head(&self) -> Result<u64> {
        Ok(self.inner.lock().seq)
    }

    fn events(&self, filter: &EventFilter) -> Result<Vec<Event>> {
        let inner = self.inner.lock();
        let limit = filter.limit.unwrap_or(50);
        Ok(inner.events.iter().rev().filter(|e| event_matches(e, filter)).take(limit).cloned().collect())
    }

    fn kv_put(&self, ns: &str, key: &str, value: &Value, expires_at: Option<i64>) -> Result<()> {
        self.inner.lock().kv.entry(ns.to_string()).or_default().insert(key.to_string(), (value.clone(), expires_at));
        Ok(())
    }

    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<Value>> {
        let mut inner = self.inner.lock();
        let Some(map) = inner.kv.get_mut(ns) else { return Ok(None) };
        match map.get(key) {
            Some((_, Some(exp))) if *exp <= now_ms() => {
                map.remove(key);
                Ok(None)
            }
            Some((v, _)) => Ok(Some(v.clone())),
            None => Ok(None),
        }
    }

    fn kv_delete(&self, ns: &str, key: &str) -> Result<()> {
        if let Some(map) = self.inner.lock().kv.get_mut(ns) {
            map.remove(key);
        }
        Ok(())
    }

    fn kv_purge_expired(&self, ns: &str, limit: usize) -> Result<u64> {
        let now = now_ms();
        let mut inner = self.inner.lock();
        let Some(map) = inner.kv.get_mut(ns) else { return Ok(0) };
        let expired: Vec<String> = map
            .iter()
            .filter(|(_, (_, exp))| exp.is_some_and(|e| e <= now))
            .map(|(k, _)| k.clone())
            .take(limit)
            .collect();
        for k in &expired {
            map.remove(k);
        }
        Ok(expired.len() as u64)
    }

    fn kv_scan(&self, ns: &str, prefix: &str) -> Result<Vec<(String, Value)>> {
        let now = now_ms();
        let inner = self.inner.lock();
        Ok(inner
            .kv
            .get(ns)
            .map(|m| {
                m.range(prefix.to_string()..)
                    .take_while(|(k, _)| k.starts_with(prefix))
                    .filter(|(_, (_, exp))| exp.is_none_or(|e| e > now))
                    .map(|(k, (v, _))| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn versions(&self, id: &str, limit: usize) -> Result<Vec<VersionRecord>> {
        let inner = self.inner.lock();
        Ok(inner.versions.iter().rev().filter(|v| v.id == id).take(limit).cloned().collect())
    }

    fn versions_since(&self, since: i64, limit: usize) -> Result<Vec<VersionRecord>> {
        let inner = self.inner.lock();
        Ok(inner.versions.iter().rev().take_while(|v| v.ts >= since).take(limit).cloned().collect())
    }

    fn link_history(&self, id: &str, until: i64, limit: usize) -> Result<Vec<LinkVersion>> {
        let inner = self.inner.lock();
        Ok(inner
            .link_versions
            .iter()
            .rev()
            .filter(|v| v.ts <= until && (v.from == id || v.to == id))
            .take(limit)
            .cloned()
            .collect())
    }

    fn stats(&self) -> Result<StoreStats> {
        let inner = self.inner.lock();
        Ok(StoreStats {
            backend: "memory".into(),
            events: inner.events.len() as u64,
            kv_entries: inner.kv.values().map(|m| m.len() as u64).sum(),
            path: None,
            vectors: None,
        })
    }

    fn trace_write(&self, batch: &[TraceRecord]) -> Result<()> {
        let mut t = self.traces.lock();
        for r in batch {
            if !t.ids.insert(r.trace.id.clone()) {
                continue;
            }
            t.records.push_back(r.clone());
            if t.records.len() > trace::MEMORY_TRACES
                && let Some(old) = t.records.pop_front()
            {
                t.ids.remove(&old.trace.id);
            }
        }
        Ok(())
    }

    fn trace_list(&self, q: &TraceQuery) -> Result<TracePage> {
        trace::list(self.traces.lock().records.iter(), q)
    }

    fn trace_get(&self, id: &str) -> Result<Option<TraceDetail>> {
        let t = self.traces.lock();
        Ok(t.records.iter().find(|r| r.trace.id == id).map(|r| trace::detail(r, t.scores.iter())))
    }

    fn trace_session(&self, id: &str, limit: usize) -> Result<Vec<TraceRow>> {
        Ok(trace::session(self.traces.lock().records.iter(), id, limit))
    }

    fn trace_metrics(&self, q: &MetricsQuery) -> Result<TraceMetrics> {
        let t = self.traces.lock();
        trace::metrics(t.records.iter(), t.scores.iter(), q)
    }

    fn trace_sessions(&self, q: &SessionQuery) -> Result<SessionPage> {
        trace::sessions(self.traces.lock().records.iter(), q)
    }

    fn score_write(&self, s: &ScoreRow) -> Result<()> {
        let mut t = self.traces.lock();
        if t.scores.iter().any(|x| x.id == s.id) {
            return Ok(());
        }
        t.scores.push_back(s.clone());
        if t.scores.len() > 4 * trace::MEMORY_TRACES {
            t.scores.pop_front();
        }
        Ok(())
    }

    fn trace_vacuum(&self, before_ms: i64, _budget: Duration) -> Result<u64> {
        // One pass under the lock: memory holds at most 50 000 traces.
        let mut t = self.traces.lock();
        let mut deleted = 0u64;
        let MemTraces { records, ids, scores } = &mut *t;
        records.retain_mut(|r| {
            if r.trace.ts < before_ms {
                deleted += 1 + r.spans.len() as u64;
                ids.remove(&r.trace.id);
                return false;
            }
            let n = r.spans.len();
            r.spans.retain(|s| s.ts >= before_ms);
            deleted += (n - r.spans.len()) as u64;
            true
        });
        let n = scores.len();
        scores.retain(|s| s.ts >= before_ms);
        deleted += (n - scores.len()) as u64;
        Ok(deleted)
    }
}
