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

use crate::changes::{
    ChangeOp, ChangeRecord, ChangeRetention, ChangeVacuum, ChangesMeta, ConsumerLag, Cursor, Gap, GapReason, GapStats,
    GapSummary, MAX_LAG_CONSUMERS, StoredChange, StoredPage,
};
use crate::graph::LinkKey;
use crate::leases::{Fence, Lease, Timer, Wake, lease_lost, no_leases};
use crate::model::{Entity, Event, EventFilter, Id, Link};
use crate::outbox::{
    Attempt, AttemptRecord, OutboxQuery, OutboxRow, OutboxSelect, OutboxState, no_outbox, replay_states,
};
use crate::runs::{RunQuery, RunRow, RunState, no_runs, settled};
use crate::time::now_ms;
use crate::trace::{
    self, MetricsQuery, ScoreRow, SessionPage, SessionQuery, TraceDetail, TraceMetrics, TracePage, TraceQuery,
    TraceRecord, TraceRow,
};
use crate::{Error, Result};

#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub use postgres::{PgOptions, PgShared, PgStore, WakeFn, WakeScanner};

/// KV namespaces whose standalone writes other processes are told about
/// (their readers keep them in memory). Everything committed inside a
/// [`WriteBatch`] is in the commit log anyway.
pub const REPLICATED_KV: &[&str] = &["memory"];

/// The namespace of the [`StoreNotice::Kv`] a shared store sends when the
/// tenant's change capture is switched: followers re-read
/// [`Store::changes_meta`].
pub const CHANGES_META_NOTICE: &str = "changes_meta";

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
    /// The writer recorded this commit's changes (see [`WriteBatch::captured`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub captured: bool,
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
    /// Change records of this batch ([`crate::changes`]); the store stamps
    /// their `seq` and keeps them with the commit.
    pub changes: Vec<StoredChange>,
    /// The writer computed `changes` (possibly none). A state-changing batch
    /// that is not captured gets a `gap` record from a store that keeps changes.
    pub captured: bool,
    /// Commit only while this lease is still held at this epoch (by the
    /// store's clock); otherwise refuse with `Error::Unavailable("lease
    /// lost …")` and write nothing. Only stores that [fence](Store::fences).
    pub fence: Option<Fence>,
    /// Background work of the tenant is due this many milliseconds after the
    /// commit (store clock): a store with a shared wake table bumps the
    /// tenant's row in the same transaction ([`crate::leases`]).
    pub wake: Option<i64>,
    /// Outbox rows inserted in the same transaction ([`crate::outbox`]);
    /// the store sets their `created_at` and `next_at` by its clock. Keys
    /// are unique by construction: a duplicate `(target, dedupe)` fails the
    /// whole write with `Error::Conflict`.
    pub outbox: Vec<OutboxRow>,
    /// Automation run rows settled in the same transaction ([`crate::runs`]):
    /// each must exist and still be `running`, otherwise the whole write is
    /// refused (`Conflict`, `run_settled`), so a run is completed at most
    /// once. The store stamps `finished_at` of final rows by its clock.
    pub runs: Vec<RunRow>,
    /// Timers set (or moved) in the same transaction: `(name, delay_ms,
    /// doc)`, due `delay_ms` after the commit by the store's clock (a run
    /// moved to `retry` and its `retry:<run>` timer commit together).
    pub timers: Vec<(String, i64, Value)>,
}

impl WriteBatch {
    /// Does this batch put or delete entities or links?
    pub fn changes_graph(&self) -> bool {
        !(self.put_entities.is_empty()
            && self.del_entities.is_empty()
            && self.put_links.is_empty()
            && self.del_links.is_empty())
    }

    /// Does this batch change state other processes cache (as opposed to
    /// only appending events and versions)?
    pub fn changes_state(&self) -> bool {
        self.changes_graph() || !self.kv.is_empty()
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
        CommitRecord {
            seq,
            ts: now_ms(),
            origin: origin.to_string(),
            entities,
            links,
            kv,
            reload: false,
            captured: self.captured,
        }
    }

    /// Does this batch change what the change feed reports (entities, links
    /// or the live schema), as opposed to other KV only?
    pub fn touches_feed(&self) -> bool {
        !(self.put_entities.is_empty()
            && self.del_entities.is_empty()
            && self.put_links.is_empty()
            && self.del_links.is_empty())
            || self
                .kv
                .iter()
                .any(|(ns, _, _)| ns == crate::live_schema::NS_SCHEMA || ns == crate::live_schema::NS_VERSIONS)
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
            && self.outbox.is_empty()
            && self.runs.is_empty()
            && self.timers.is_empty()
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
    // ── change feed (docs/design/EVENTS.md, Part A); stores without one refuse ──

    /// Does this store keep a change log?
    fn keeps_changes(&self) -> bool {
        false
    }
    /// Change records after `after`, ascending, at most `limit`, each with
    /// the entity document it joins (create, update), plus head and floor.
    fn changes_since(&self, _after: Cursor, _limit: usize) -> Result<StoredPage> {
        Err(no_change_log())
    }
    /// Whether commits are captured, the floor and the head.
    fn changes_meta(&self) -> Result<ChangesMeta> {
        Err(no_change_log())
    }
    /// Turn capture on or off for the tenant. Turning it on after a period
    /// off raises the floor to the head (nothing was recorded meanwhile, so
    /// older cursors are expired) and appends a `capture_enabled` gap after
    /// every cursor at the head, so a reader that was moved to the head
    /// while capture was off is told rather than silently skipped.
    fn set_capture(&self, _on: bool) -> Result<()> {
        Err(no_change_log())
    }
    /// Remove records past `keep` (whole commits) and raise the floor above
    /// them in the same step, so their cursors read as expired. The floor
    /// never passes the head, so the newest commit's records are always kept
    /// and a reader resuming from the head is not reset again.
    fn vacuum_changes(&self, _keep: &ChangeRetention) -> Result<ChangeVacuum> {
        Err(no_change_log())
    }
    /// Gap records with `ts >= since_ms` (store clock), counted up to `cap`,
    /// and the newest gap record kept, however old (docs/design/EVENTS.md
    /// section 11). Reads gap records only, never the rest of the feed.
    fn gap_stats(&self, _since_ms: i64, _cap: u64) -> Result<GapStats> {
        Err(no_change_log())
    }

    // ── leases, consumers, timers and wake-ups (docs/design/EVENTS.md D12, section 9) ──
    // Times are the store's clock (the database clock on PostgreSQL). Stores
    // without them refuse; none of them goes through the commit log.

    /// Does [`write`](Self::write) honour [`WriteBatch::fence`]? The engine
    /// refuses fenced writes to a store that does not.
    fn fences(&self) -> bool {
        false
    }
    /// The store's clock in milliseconds (the database clock on PostgreSQL).
    fn clock_ms(&self) -> Result<i64> {
        Ok(now_ms())
    }
    /// Take or renew the lease `name` for `ttl_ms` of the store's clock.
    /// Returns its epoch, or `None` while another holder has it. The epoch
    /// grows whenever the holder changes; a holder renewing keeps it.
    fn lease(&self, _name: &str, _holder: &str, _ttl_ms: i64) -> Result<Option<u64>> {
        Err(no_leases())
    }
    /// Give the lease back if `fence.holder` still has it at `fence.epoch`
    /// (it expires now; the epoch is kept, so the next holder's is higher).
    /// Fenced: a stale holder, one whose epoch was since taken by another
    /// holder or again by itself, releases nothing. Returns whether it did.
    /// What the lease guarded is not lost: consumer cursors stay where the
    /// holder's fenced writes left them, and the next holder resumes there.
    fn release(&self, _fence: &Fence) -> Result<bool> {
        Err(no_leases())
    }
    /// The lease row, if the lease was ever taken.
    fn lease_get(&self, _name: &str) -> Result<Option<Lease>> {
        Err(no_leases())
    }
    /// Where the consumer `name` resumes (after this cursor).
    fn consumer_get(&self, _name: &str) -> Result<Option<Cursor>> {
        Err(no_leases())
    }
    /// Move the consumer `name` to `cursor`; with a fence, only while that
    /// lease is held at its epoch (otherwise "lease lost" and nothing moves).
    fn consumer_set(&self, _name: &str, _cursor: Cursor, _fence: Option<&Fence>) -> Result<()> {
        Err(no_leases())
    }
    /// Forget the consumer `name` (its subscription was removed); with a
    /// fence, only while that lease is held at its epoch. Returns whether
    /// it existed.
    fn consumer_delete(&self, _name: &str, _fence: Option<&Fence>) -> Result<bool> {
        Err(no_leases())
    }
    /// Every consumer's cursor and how far it is behind the head, by name
    /// (at most [`MAX_LAG_CONSUMERS`]). One lookup per consumer, never a
    /// scan of the feed.
    fn consumer_lags(&self) -> Result<Vec<ConsumerLag>> {
        Err(no_leases())
    }
    /// Set (or move) the timer `name` to `delay_ms` from now by the store's
    /// clock, with `doc`. Returns its due time. Bumps the tenant's wake row
    /// where there is one.
    fn timers_set(&self, _name: &str, _delay_ms: i64, _doc: &Value) -> Result<i64> {
        Err(no_leases())
    }
    /// Timers due by the store's clock, earliest first, at most `limit`.
    fn timers_due(&self, _limit: usize) -> Result<Vec<Timer>> {
        Err(no_leases())
    }
    /// The earliest due time of any timer.
    fn timers_next(&self) -> Result<Option<i64>> {
        Err(no_leases())
    }
    /// Delete the timer `name`; with `due_at`, only if it is still due then
    /// (a timer moved meanwhile stays). Returns whether it deleted one.
    fn timers_delete(&self, _name: &str, _due_at: Option<i64>) -> Result<bool> {
        Err(no_leases())
    }
    /// The tenant's wake row (`None`: the store has no wake table, or the
    /// tenant was never bumped).
    fn wake_get(&self) -> Result<Option<Wake>> {
        Ok(None)
    }
    /// After draining, set the tenant's next due time (store clock; `None`:
    /// nothing due) unless the row was bumped since `seen` was read. Returns
    /// false when it was: drain again. Stores without a wake table have
    /// nothing to settle.
    fn wake_settle(&self, _seen: u64, _next_due: Option<i64>) -> Result<bool> {
        Ok(true)
    }

    // ── outbox (docs/design/EVENTS.md B.1); stores without one refuse ──
    // Times are the store's clock. Rows committed with a graph change come
    // through `WriteBatch::outbox`; none of these goes through the commit log.

    /// Does this store keep an outbox?
    fn keeps_outbox(&self) -> bool {
        false
    }
    /// Insert rows unless their `(target, dedupe)` exists (those are
    /// skipped), and move the consumer `advance.0` to `advance.1`, in one
    /// transaction; with a fence, only while that lease is held at its epoch
    /// (otherwise "lease lost" and nothing is written). Returns the rows
    /// inserted. Bumps the tenant's wake row when it inserted any.
    fn outbox_enqueue(
        &self,
        _rows: &[OutboxRow],
        _advance: Option<(&str, Cursor)>,
        _fence: Option<&Fence>,
    ) -> Result<usize> {
        Err(no_outbox())
    }
    /// Claim up to `limit` due rows (pending or retry whose `next_at` passed,
    /// or inflight whose lease ended), earliest first, skipping rows of
    /// `skip_targets` and rows another claim is taking right now: each gets
    /// state `inflight`, a fresh claim token starting with `holder`, a lease
    /// of `lease_ms` and one more attempt.
    fn outbox_claim(
        &self,
        _limit: usize,
        _lease_ms: i64,
        _holder: &str,
        _skip_targets: &[String],
    ) -> Result<Vec<OutboxRow>> {
        Err(no_outbox())
    }
    /// Record an attempt and move the row to `next` (`retry`: due `delay_ms`
    /// from now; `delivered` also stamps `delivered_at`), only while `claim`
    /// is still the row's claim token. Returns false (and changes nothing)
    /// when it is not: the lease expired and another worker claimed it.
    fn outbox_finish(
        &self,
        _id: &str,
        _claim: &str,
        _attempt: &Attempt,
        _next: OutboxState,
        _delay_ms: i64,
    ) -> Result<bool> {
        Err(no_outbox())
    }
    /// Give a claimed row back unsent (kill switch, paused endpoint, a lease
    /// too short to send it in): its attempt is not counted. Only while
    /// `claim` is its token.
    fn outbox_release(&self, _id: &str, _claim: &str) -> Result<bool> {
        Err(no_outbox())
    }
    /// One row.
    fn outbox_get(&self, _id: &str) -> Result<Option<OutboxRow>> {
        Err(no_outbox())
    }
    /// Rows matching `q`, newest first (ties by id, descending).
    fn outbox_list(&self, _q: &OutboxQuery) -> Result<Vec<OutboxRow>> {
        Err(no_outbox())
    }
    /// Replay: rows of `target` selected by `sel` whose state is one of
    /// [`replay_states(states)`](crate::outbox::replay_states) go back to
    /// `pending`, due now, with no attempts and no claim. They keep their
    /// id, so receivers see the same event id again. Returns how many.
    fn outbox_reset(&self, _target: &str, _sel: &OutboxSelect, _states: &[OutboxState]) -> Result<usize> {
        Err(no_outbox())
    }
    /// The attempts of a row, newest first, at most `limit`.
    fn outbox_attempts(&self, _id: &str, _limit: usize) -> Result<Vec<AttemptRecord>> {
        Err(no_outbox())
    }
    /// When the next row is due: the earliest `next_at` of pending and retry
    /// rows, or `lease_until` of inflight ones.
    fn outbox_next_due(&self) -> Result<Option<i64>> {
        Err(no_outbox())
    }
    /// Rows not yet finished (pending, inflight, retry) per target, by
    /// target name (fan-out pauses an endpoint whose backlog passes its cap).
    fn outbox_open_counts(&self) -> Result<Vec<(String, u64)>> {
        Err(no_outbox())
    }
    /// Open rows of each of `targets`, counted up to `cap` (fan-out compares
    /// them with its pending cap every round, so a count never reads more
    /// than `cap` rows of a target).
    fn outbox_open_counts_for(&self, targets: &[String], cap: u64) -> Result<Vec<(String, u64)>> {
        let all: std::collections::HashMap<String, u64> = self.outbox_open_counts()?.into_iter().collect();
        Ok(targets.iter().map(|t| (t.clone(), all.get(t).copied().unwrap_or(0).min(cap))).collect())
    }
    /// Rows per target in each of [`STATS_STATES`](crate::outbox::STATS_STATES)
    /// (pending, inflight, retry, dead), each counted up to `cap`, by target
    /// then state; states without rows are left out. Delivered and cancelled
    /// rows (most of the table) are never read.
    fn outbox_state_counts(&self, _cap: u64) -> Result<Vec<(String, OutboxState, u64)>> {
        Err(no_outbox())
    }
    /// Delete finished rows (delivered, dead, cancelled) created before
    /// `before_ms`, and attempts logged before it. Returns the rows deleted.
    fn outbox_vacuum(&self, _before_ms: i64) -> Result<u64> {
        Err(no_outbox())
    }

    // ── automation runs (docs/design/EVENTS.md C.7, C.8); stores without them refuse ──
    // Times are the store's clock; none of these goes through the commit log.

    /// Does this store keep automation runs?
    fn keeps_runs(&self) -> bool {
        false
    }
    /// Claim runs: insert each row unless its id or its `(automation,
    /// trigger_key)` exists (a duplicate trigger, settled here and never
    /// retried), with `started_at` (and `finished_at` for final rows) by the
    /// store's clock; with a fence, only while that lease is held at its
    /// epoch. One statement. Returns the ids inserted.
    fn run_claim(&self, _rows: &[RunRow], _fence: Option<&Fence>) -> Result<Vec<String>> {
        Err(no_runs())
    }
    /// Replace a run row (state, epoch, attempts, document; `finished_at`
    /// stamped when it is final, cleared otherwise) only while its state is
    /// one of `from` (any, when empty); with a fence, only while that lease
    /// is held. Returns whether it changed.
    fn run_update(&self, _row: &RunRow, _from: &[RunState], _fence: Option<&Fence>) -> Result<bool> {
        Err(no_runs())
    }
    /// One run.
    fn run_get(&self, _id: &str) -> Result<Option<RunRow>> {
        Err(no_runs())
    }
    /// Runs matching `q`, newest first (ties by id, descending).
    fn runs_list(&self, _q: &RunQuery) -> Result<Vec<RunRow>> {
        Err(no_runs())
    }
    /// Runs per state among the newest `cap` started at or after `since_ms`
    /// (store clock), by state. Their documents are not read.
    fn runs_state_counts(&self, since_ms: i64, cap: usize) -> Result<Vec<(RunState, u64)>> {
        let q = RunQuery { since: Some(since_ms), limit: Some(cap), ..Default::default() };
        let mut by: std::collections::BTreeMap<&'static str, (RunState, u64)> = Default::default();
        for r in self.runs_list(&q)? {
            by.entry(r.state.as_str()).or_insert((r.state, 0)).1 += 1;
        }
        Ok(by.into_values().collect())
    }
    /// `running` rows claimed under an epoch below `epoch` (what a new
    /// holder of the consumer lease recovers), oldest first, at most `limit`.
    fn runs_running(&self, _epoch: u64, _limit: usize) -> Result<Vec<RunRow>> {
        Err(no_runs())
    }
    /// Retention: delete final runs (succeeded, failed, skipped, refused,
    /// would_run, cancelled) started before `before_ms`. Returns how many.
    /// Run rows are not the audit trail (the `automation.run` events are).
    fn runs_vacuum(&self, _before_ms: i64) -> Result<u64> {
        Err(no_runs())
    }

    /// Newest first.
    fn events(&self, filter: &EventFilter) -> Result<Vec<Event>>;
    fn kv_put(&self, ns: &str, key: &str, value: &Value, expires_at: Option<i64>) -> Result<()>;
    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<Value>>;
    fn kv_delete(&self, ns: &str, key: &str) -> Result<()>;
    /// All live entries of a namespace whose key starts with `prefix`, key order.
    fn kv_scan(&self, ns: &str, prefix: &str) -> Result<Vec<(String, Value)>>;
    /// Delete at most `limit` entries of a namespace that expired at or
    /// before `before` (Unix ms); how many. Reads skip expired entries
    /// anyway: this only keeps a namespace that nothing reads back whole
    /// from growing. Stores that keep no expired rows need not.
    fn kv_purge_expired(&self, ns: &str, before: i64, limit: usize) -> Result<u64> {
        let _ = (ns, before, limit);
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

fn no_change_log() -> Error {
    Error::Unavailable("this store keeps no change log".into())
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
///
/// It keeps a bounded change log with full entity documents (memory only).
/// Evicting the oldest commit's records raises the floor; a state-changing
/// write without change records gets a `gap` record.
pub struct MemoryStore {
    inner: Mutex<MemInner>,
    max_events: usize,
    max_changes: usize,
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
    /// Change records in cursor order, each with the entity it left (create, update).
    changes: VecDeque<(StoredChange, Option<Entity>)>,
    /// Records of commits below the floor are gone.
    floor: u64,
    capture_off: bool,
    leases: HashMap<String, MemLease>,
    consumers: HashMap<String, Cursor>,
    /// name -> (due_at, doc)
    timers: HashMap<String, (i64, Value)>,
    /// id -> row
    outbox: HashMap<String, OutboxRow>,
    /// (target, dedupe) of every row in `outbox`
    outbox_keys: HashSet<(String, String)>,
    attempts: Vec<AttemptRecord>,
    /// id -> run
    runs: HashMap<String, RunRow>,
    /// (automation, trigger_key) of every run in `runs`
    run_keys: HashSet<(String, String)>,
}

struct MemLease {
    holder: String,
    epoch: u64,
    until: i64,
}

impl MemInner {
    /// Insert outbox rows (times by the store's clock); with `strict`, a
    /// duplicate key fails before anything is inserted, otherwise it is skipped.
    fn outbox_insert(&mut self, rows: &[OutboxRow], strict: bool) -> Result<usize> {
        if strict {
            let mut seen = HashSet::new();
            for r in rows {
                let key = (r.target.clone(), r.dedupe.clone());
                if self.outbox_keys.contains(&key) || self.outbox.contains_key(&r.id) || !seen.insert(key) {
                    return Err(Error::Conflict(format!(
                        "duplicate key: outbox row ({}, {}) exists",
                        r.target, r.dedupe
                    )));
                }
            }
        }
        let now = now_ms();
        let mut n = 0;
        for r in rows {
            let key = (r.target.clone(), r.dedupe.clone());
            if self.outbox_keys.contains(&key) || self.outbox.contains_key(&r.id) {
                continue;
            }
            let mut r = r.clone();
            r.created_at = now;
            r.next_at = now;
            r.state = OutboxState::Pending;
            r.attempts = 0;
            r.claim = None;
            r.lease_until = None;
            r.delivered_at = None;
            self.outbox_keys.insert(key);
            self.outbox.insert(r.id.clone(), r);
            n += 1;
        }
        Ok(n)
    }

    /// Settle runs in a commit: every row must exist and be `running`.
    fn runs_settle(&mut self, rows: &[RunRow]) -> Result<()> {
        for r in rows {
            if self.runs.get(&r.id).is_none_or(|cur| cur.state != RunState::Running) {
                return Err(settled(&r.id));
            }
        }
        let now = now_ms();
        for r in rows {
            let started = self.runs.get(&r.id).map(|c| c.started_at).unwrap_or(now);
            let mut r = r.clone();
            r.started_at = started;
            r.finished_at = r.state.is_final().then_some(now);
            self.runs.insert(r.id.clone(), r);
        }
        Ok(())
    }

    /// The fence's lease is held by its holder at its epoch right now.
    fn check_fence(&self, f: &Fence) -> Result<()> {
        match self.leases.get(&f.name) {
            Some(l) if l.holder == f.holder && l.epoch == f.epoch && l.until >= now_ms() => Ok(()),
            _ => Err(lease_lost(f)),
        }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::with_event_capacity(100_000)
    }
}

impl MemoryStore {
    /// Keeps at most `max_events` events, versions and change records.
    pub fn with_event_capacity(max_events: usize) -> Self {
        Self {
            inner: Mutex::new(MemInner::default()),
            max_events,
            max_changes: max_events,
            traces: Mutex::new(MemTraces::default()),
        }
    }

    /// Keep at most `max` change records (whole commits are evicted).
    pub fn with_change_capacity(mut self, max: usize) -> Self {
        self.max_changes = max.max(1);
        self
    }

    fn write_as(&self, batch: &WriteBatch, uncaptured: GapReason) -> Result<u64> {
        let mut inner = self.inner.lock();
        // Checked under the lock that serializes writes and leases: nothing
        // is written when the fence is lost.
        if let Some(f) = &batch.fence {
            inner.check_fence(f)?;
        }
        // Before anything else is written: a duplicate key, or a run that is
        // no longer running, fails the write.
        if let Some(r) = batch.runs.iter().find(|r| inner.runs.get(&r.id).is_none_or(|c| c.state != RunState::Running))
        {
            return Err(settled(&r.id));
        }
        inner.outbox_insert(&batch.outbox, true)?;
        inner.runs_settle(&batch.runs)?;
        let now = now_ms();
        for (name, delay, doc) in &batch.timers {
            inner.timers.insert(name.clone(), (now.saturating_add(*delay), doc.clone()));
        }
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
            let seq = inner.seq;
            if batch.captured {
                // Kept even while capture is off (a writer with a stale flag):
                // extra records, never missing ones.
                let docs: HashMap<&str, &Entity> = batch.put_entities.iter().map(|e| (e.id.as_str(), e)).collect();
                for c in &batch.changes {
                    let mut c = c.clone();
                    c.record.cursor.seq = seq;
                    let doc = match c.record.op {
                        ChangeOp::EntityCreate | ChangeOp::EntityUpdate => {
                            docs.get(c.record.id.as_str()).map(|e| (*e).clone())
                        }
                        _ => None,
                    };
                    inner.changes.push_back((c, doc));
                }
            } else if !inner.capture_off && batch.touches_feed() {
                let rec = batch.commit_record(seq, "");
                let gap = Gap::listing(uncaptured, rec.origin, rec.entities, rec.links);
                inner.changes.push_back((
                    StoredChange { record: ChangeRecord::gap(seq, rec.ts, gap), before_vis: None, rev_after: None },
                    None,
                ));
            }
            self.evict_changes(&mut inner);
        }
        Ok(inner.seq)
    }

    /// Evict whole commits, oldest first, past `max_changes`, raising the
    /// floor over them. The newest commit's records are never evicted (as in
    /// `vacuum_changes`), so the floor never passes the head and a reader at
    /// the head is never reset; a single commit larger than the capacity is
    /// kept whole until the next one.
    fn evict_changes(&self, inner: &mut MemInner) {
        let newest = inner.seq;
        while inner.changes.len() > self.max_changes {
            let Some(oldest) = inner.changes.front().map(|(c, _)| c.record.cursor.seq) else { break };
            if oldest >= newest {
                break;
            }
            while inner.changes.front().is_some_and(|(c, _)| c.record.cursor.seq == oldest) {
                inner.changes.pop_front();
            }
            inner.floor = inner.floor.max(oldest + 1);
        }
    }
}

/// The `capture_enabled` gap written when capture is turned on again: after
/// every cursor a reader can hold at the head commit `seq` (`<seq>.0` or its
/// last record), so a reader that kept polling while capture was off (and so
/// was moved to the head) receives it instead of silently missing what was
/// written meanwhile. `last_n` is the last record's `n` at `seq`, if any.
pub(crate) fn capture_enabled_gap(seq: u64, last_n: Option<u32>, origin: &str) -> StoredChange {
    let gap = Gap::listing(GapReason::CaptureEnabled, origin.to_string(), Vec::new(), Vec::new());
    let mut record = ChangeRecord::gap(seq, now_ms(), gap);
    record.cursor.n = last_n.map_or(1, |n| n.saturating_add(1));
    StoredChange { record, before_vis: None, rev_after: None }
}

fn head_cursor(inner: &MemInner) -> Cursor {
    match inner.changes.back() {
        Some((c, _)) if c.record.cursor.seq == inner.seq => c.record.cursor,
        _ => Cursor::new(inner.seq, 0),
    }
}

impl Store for MemoryStore {
    fn load(&self) -> Result<Snapshot> {
        Ok(Snapshot::default())
    }

    fn write(&self, batch: &WriteBatch) -> Result<u64> {
        self.write_as(batch, GapReason::UncapturedWriter)
    }

    /// Like the default, one commit per chunk; each uncaptured commit's gap
    /// says `bulk_import`.
    fn bulk_import(&self, entities: &[Entity], links: &[Link]) -> Result<u64> {
        let mut seq = self.head()?;
        for chunk in entities.chunks(5_000) {
            seq = self
                .write_as(&WriteBatch { put_entities: chunk.to_vec(), ..Default::default() }, GapReason::BulkImport)?;
        }
        for chunk in links.chunks(5_000) {
            seq =
                self.write_as(&WriteBatch { put_links: chunk.to_vec(), ..Default::default() }, GapReason::BulkImport)?;
        }
        Ok(seq)
    }

    fn keeps_changes(&self) -> bool {
        true
    }

    fn changes_since(&self, after: Cursor, limit: usize) -> Result<StoredPage> {
        let inner = self.inner.lock();
        let start = inner.changes.partition_point(|(c, _)| c.record.cursor <= after);
        Ok(StoredPage {
            changes: inner.changes.iter().skip(start).take(limit).cloned().collect(),
            head: head_cursor(&inner),
            floor: inner.floor,
        })
    }

    fn changes_meta(&self) -> Result<ChangesMeta> {
        let inner = self.inner.lock();
        Ok(ChangesMeta { capture: !inner.capture_off, floor: inner.floor, head: head_cursor(&inner) })
    }

    fn set_capture(&self, on: bool) -> Result<()> {
        let mut inner = self.inner.lock();
        if on && inner.capture_off {
            let seq = inner.seq;
            inner.floor = inner.floor.max(seq);
            if seq > 0 {
                let last_n =
                    inner.changes.back().filter(|(c, _)| c.record.cursor.seq == seq).map(|(c, _)| c.record.cursor.n);
                inner.changes.push_back((capture_enabled_gap(seq, last_n, ""), None));
                self.evict_changes(&mut inner);
            }
        }
        inner.capture_off = !on;
        Ok(())
    }

    fn vacuum_changes(&self, keep: &ChangeRetention) -> Result<ChangeVacuum> {
        let mut inner = self.inner.lock();
        let mut floor = inner.floor;
        if let Some(ms) = keep.keep_ms {
            let cutoff = now_ms().saturating_sub(ms);
            if let Some(seq) =
                inner.changes.iter().filter(|(c, _)| c.record.ts < cutoff).map(|(c, _)| c.record.cursor.seq).max()
            {
                floor = floor.max(seq + 1);
            }
        }
        if let Some(rows) = keep.keep_rows
            && inner.changes.len() as u64 > rows
        {
            let (c, _) = &inner.changes[inner.changes.len() - rows as usize - 1];
            floor = floor.max(c.record.cursor.seq + 1);
        }
        // Never above the head (see the trait method).
        let floor = floor.min(inner.seq).max(inner.floor);
        let mut deleted = 0;
        while inner.changes.front().is_some_and(|(c, _)| c.record.cursor.seq < floor) {
            inner.changes.pop_front();
            deleted += 1;
        }
        inner.floor = floor;
        Ok(ChangeVacuum { deleted, floor })
    }

    fn gap_stats(&self, since_ms: i64, cap: u64) -> Result<GapStats> {
        let inner = self.inner.lock();
        let mut out = GapStats::default();
        // Newest first; records are in commit order, so stop at the first
        // one older than `since_ms` once the newest gap is known.
        for (c, _) in inner.changes.iter().rev() {
            let r = &c.record;
            if r.ts < since_ms && out.latest.is_some() {
                break;
            }
            let Some(gap) = r.gap.as_ref().filter(|_| r.op == ChangeOp::Gap) else { continue };
            if out.latest.is_none() {
                out.latest = Some(GapSummary::of(r.cursor, r.ts, gap));
            }
            if r.ts >= since_ms && out.count < cap {
                out.count += 1;
            }
        }
        out.capped = cap > 0 && out.count >= cap;
        Ok(out)
    }

    fn head(&self) -> Result<u64> {
        Ok(self.inner.lock().seq)
    }

    fn fences(&self) -> bool {
        true
    }

    fn lease(&self, name: &str, holder: &str, ttl_ms: i64) -> Result<Option<u64>> {
        let mut inner = self.inner.lock();
        let now = now_ms();
        let until = now.saturating_add(ttl_ms.max(0));
        match inner.leases.get_mut(name) {
            None => {
                inner.leases.insert(name.to_string(), MemLease { holder: holder.to_string(), epoch: 1, until });
                Ok(Some(1))
            }
            Some(l) if l.holder == holder => {
                l.until = until;
                Ok(Some(l.epoch))
            }
            Some(l) if l.until < now => {
                l.holder = holder.to_string();
                l.epoch += 1;
                l.until = until;
                Ok(Some(l.epoch))
            }
            Some(_) => Ok(None),
        }
    }

    fn release(&self, fence: &Fence) -> Result<bool> {
        let mut inner = self.inner.lock();
        let now = now_ms();
        match inner.leases.get_mut(&fence.name) {
            Some(l) if l.holder == fence.holder && l.epoch == fence.epoch && l.until >= now => {
                l.until = now - 1;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn lease_get(&self, name: &str) -> Result<Option<Lease>> {
        let now = now_ms();
        Ok(self.inner.lock().leases.get(name).map(|l| Lease {
            name: name.to_string(),
            holder: l.holder.clone(),
            epoch: l.epoch,
            until: l.until,
            held: l.until >= now,
        }))
    }

    fn consumer_get(&self, name: &str) -> Result<Option<Cursor>> {
        Ok(self.inner.lock().consumers.get(name).copied())
    }

    fn consumer_set(&self, name: &str, cursor: Cursor, fence: Option<&Fence>) -> Result<()> {
        let mut inner = self.inner.lock();
        if let Some(f) = fence {
            inner.check_fence(f)?;
        }
        inner.consumers.insert(name.to_string(), cursor);
        Ok(())
    }

    fn consumer_delete(&self, name: &str, fence: Option<&Fence>) -> Result<bool> {
        let mut inner = self.inner.lock();
        if let Some(f) = fence {
            inner.check_fence(f)?;
        }
        Ok(inner.consumers.remove(name).is_some())
    }

    fn consumer_lags(&self) -> Result<Vec<ConsumerLag>> {
        let inner = self.inner.lock();
        let now = now_ms();
        let mut names: Vec<&String> = inner.consumers.keys().collect();
        names.sort();
        names.truncate(MAX_LAG_CONSUMERS);
        Ok(names
            .into_iter()
            .map(|name| {
                let cursor = inner.consumers[name];
                let start = inner.changes.partition_point(|(c, _)| c.record.cursor <= cursor);
                let oldest = inner.changes.get(start).map(|(c, _)| c.record.ts);
                ConsumerLag {
                    name: name.clone(),
                    cursor,
                    behind: inner.seq.saturating_sub(cursor.seq),
                    age_ms: oldest.map_or(0, |ts| now.saturating_sub(ts).max(0)),
                    updated_at: None,
                    expired: cursor.expired_at(inner.floor),
                }
            })
            .collect())
    }

    fn timers_set(&self, name: &str, delay_ms: i64, doc: &Value) -> Result<i64> {
        let due = now_ms().saturating_add(delay_ms);
        self.inner.lock().timers.insert(name.to_string(), (due, doc.clone()));
        Ok(due)
    }

    fn timers_due(&self, limit: usize) -> Result<Vec<Timer>> {
        let now = now_ms();
        let inner = self.inner.lock();
        let mut due: Vec<Timer> = inner
            .timers
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(name, (at, doc))| Timer { name: name.clone(), due_at: *at, doc: doc.clone() })
            .collect();
        due.sort_by(|a, b| a.due_at.cmp(&b.due_at).then_with(|| a.name.cmp(&b.name)));
        due.truncate(limit);
        Ok(due)
    }

    fn timers_next(&self) -> Result<Option<i64>> {
        Ok(self.inner.lock().timers.values().map(|(at, _)| *at).min())
    }

    fn timers_delete(&self, name: &str, due_at: Option<i64>) -> Result<bool> {
        let mut inner = self.inner.lock();
        match inner.timers.get(name) {
            Some((at, _)) if due_at.is_none_or(|d| d == *at) => {
                inner.timers.remove(name);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn keeps_runs(&self) -> bool {
        true
    }

    fn run_claim(&self, rows: &[RunRow], fence: Option<&Fence>) -> Result<Vec<String>> {
        let mut inner = self.inner.lock();
        if let Some(f) = fence {
            inner.check_fence(f)?;
        }
        let now = now_ms();
        let mut out = Vec::new();
        for r in rows {
            let key = (r.automation.clone(), r.trigger_key.clone());
            if inner.runs.contains_key(&r.id) || inner.run_keys.contains(&key) {
                continue;
            }
            let mut r = r.clone();
            r.started_at = now;
            r.finished_at = r.state.is_final().then_some(now);
            inner.run_keys.insert(key);
            out.push(r.id.clone());
            inner.runs.insert(r.id.clone(), r);
        }
        Ok(out)
    }

    fn run_update(&self, row: &RunRow, from: &[RunState], fence: Option<&Fence>) -> Result<bool> {
        let mut inner = self.inner.lock();
        if let Some(f) = fence {
            inner.check_fence(f)?;
        }
        let Some(cur) = inner.runs.get(&row.id) else { return Ok(false) };
        if !from.is_empty() && !from.contains(&cur.state) {
            return Ok(false);
        }
        let mut r = row.clone();
        r.started_at = cur.started_at;
        r.automation = cur.automation.clone();
        r.trigger_key = cur.trigger_key.clone();
        r.finished_at = r.state.is_final().then(now_ms);
        inner.runs.insert(r.id.clone(), r);
        Ok(true)
    }

    fn run_get(&self, id: &str) -> Result<Option<RunRow>> {
        Ok(self.inner.lock().runs.get(id).cloned())
    }

    fn runs_list(&self, q: &RunQuery) -> Result<Vec<RunRow>> {
        let inner = self.inner.lock();
        let mut out: Vec<RunRow> = inner.runs.values().filter(|r| q.matches(r)).cloned().collect();
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at).then_with(|| b.id.cmp(&a.id)));
        out.truncate(q.limit());
        Ok(out)
    }

    fn runs_state_counts(&self, since_ms: i64, cap: usize) -> Result<Vec<(RunState, u64)>> {
        let inner = self.inner.lock();
        let mut recent: Vec<&RunRow> = inner.runs.values().filter(|r| r.started_at >= since_ms).collect();
        recent.sort_by(|a, b| b.started_at.cmp(&a.started_at).then_with(|| b.id.cmp(&a.id)));
        let mut by: std::collections::BTreeMap<&'static str, (RunState, u64)> = Default::default();
        for r in recent.into_iter().take(cap) {
            by.entry(r.state.as_str()).or_insert((r.state, 0)).1 += 1;
        }
        Ok(by.into_values().collect())
    }

    fn runs_running(&self, epoch: u64, limit: usize) -> Result<Vec<RunRow>> {
        let inner = self.inner.lock();
        let mut out: Vec<RunRow> =
            inner.runs.values().filter(|r| r.state == RunState::Running && r.epoch < epoch).cloned().collect();
        out.sort_by(|a, b| a.started_at.cmp(&b.started_at).then_with(|| a.id.cmp(&b.id)));
        out.truncate(limit);
        Ok(out)
    }

    fn runs_vacuum(&self, before_ms: i64) -> Result<u64> {
        let mut inner = self.inner.lock();
        let gone: Vec<(String, (String, String))> = inner
            .runs
            .values()
            .filter(|r| r.state.is_final() && r.started_at < before_ms)
            .map(|r| (r.id.clone(), (r.automation.clone(), r.trigger_key.clone())))
            .collect();
        for (id, key) in &gone {
            inner.runs.remove(id);
            inner.run_keys.remove(key);
        }
        Ok(gone.len() as u64)
    }

    fn keeps_outbox(&self) -> bool {
        true
    }

    fn outbox_enqueue(
        &self,
        rows: &[OutboxRow],
        advance: Option<(&str, Cursor)>,
        fence: Option<&Fence>,
    ) -> Result<usize> {
        for r in rows {
            crate::outbox::check_row(r)?;
        }
        let mut inner = self.inner.lock();
        if let Some(f) = fence {
            inner.check_fence(f)?;
        }
        let n = inner.outbox_insert(rows, false)?;
        if let Some((name, cursor)) = advance {
            inner.consumers.insert(name.to_string(), cursor);
        }
        Ok(n)
    }

    fn outbox_claim(
        &self,
        limit: usize,
        lease_ms: i64,
        holder: &str,
        skip_targets: &[String],
    ) -> Result<Vec<OutboxRow>> {
        let now = now_ms();
        let mut inner = self.inner.lock();
        let mut due: Vec<(i64, String)> = inner
            .outbox
            .values()
            .filter(|r| !skip_targets.contains(&r.target))
            .filter(|r| match r.state {
                OutboxState::Pending | OutboxState::Retry => r.next_at <= now,
                OutboxState::Inflight => r.lease_until.is_some_and(|l| l < now),
                _ => false,
            })
            .map(|r| (r.next_at, r.id.clone()))
            .collect();
        due.sort();
        due.truncate(limit);
        let mut out = Vec::with_capacity(due.len());
        for (_, id) in due {
            if let Some(r) = inner.outbox.get_mut(&id) {
                r.state = OutboxState::Inflight;
                r.claim = Some(format!("{holder}:{:016x}{:016x}", fastrand::u64(..), fastrand::u64(..)));
                r.lease_until = Some(now.saturating_add(lease_ms.max(0)));
                r.attempts += 1;
                out.push(r.clone());
            }
        }
        Ok(out)
    }

    fn outbox_finish(
        &self,
        id: &str,
        claim: &str,
        attempt: &Attempt,
        next: OutboxState,
        delay_ms: i64,
    ) -> Result<bool> {
        let now = now_ms();
        let mut inner = self.inner.lock();
        let Some(r) = inner.outbox.get_mut(id).filter(|r| r.claim.as_deref() == Some(claim)) else {
            return Ok(false);
        };
        r.state = next;
        r.claim = None;
        r.lease_until = None;
        r.last_status = attempt.status;
        r.last_error = attempt.error.clone();
        if next == OutboxState::Retry {
            r.next_at = now.saturating_add(delay_ms.max(0));
        }
        if next == OutboxState::Delivered {
            r.delivered_at = Some(now);
        }
        let rec = AttemptRecord {
            outbox_id: id.to_string(),
            attempt: r.attempts,
            ts: now,
            state: next,
            result: attempt.clone(),
        };
        inner.attempts.push(rec);
        if inner.attempts.len() > self.max_events {
            inner.attempts.remove(0);
        }
        Ok(true)
    }

    fn outbox_release(&self, id: &str, claim: &str) -> Result<bool> {
        let mut inner = self.inner.lock();
        let Some(r) = inner.outbox.get_mut(id).filter(|r| r.claim.as_deref() == Some(claim)) else {
            return Ok(false);
        };
        r.attempts = r.attempts.saturating_sub(1);
        r.state = if r.attempts == 0 { OutboxState::Pending } else { OutboxState::Retry };
        r.claim = None;
        r.lease_until = None;
        Ok(true)
    }

    fn outbox_get(&self, id: &str) -> Result<Option<OutboxRow>> {
        Ok(self.inner.lock().outbox.get(id).cloned())
    }

    fn outbox_list(&self, q: &OutboxQuery) -> Result<Vec<OutboxRow>> {
        let inner = self.inner.lock();
        let mut rows: Vec<OutboxRow> = inner.outbox.values().filter(|r| q.matches(r)).cloned().collect();
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
        rows.truncate(q.limit());
        Ok(rows)
    }

    fn outbox_reset(&self, target: &str, sel: &OutboxSelect, states: &[OutboxState]) -> Result<usize> {
        let states = replay_states(states);
        let now = now_ms();
        let mut inner = self.inner.lock();
        let mut n = 0;
        for r in inner.outbox.values_mut() {
            let picked = match sel {
                OutboxSelect::Ids(ids) => ids.contains(&r.id),
                OutboxSelect::Since(t) => r.created_at >= *t,
            };
            if r.target == target && picked && states.contains(&r.state) {
                r.state = OutboxState::Pending;
                r.attempts = 0;
                r.next_at = now;
                r.claim = None;
                r.lease_until = None;
                r.delivered_at = None;
                n += 1;
            }
        }
        Ok(n)
    }

    fn outbox_attempts(&self, id: &str, limit: usize) -> Result<Vec<AttemptRecord>> {
        let inner = self.inner.lock();
        Ok(inner.attempts.iter().rev().filter(|a| a.outbox_id == id).take(limit).cloned().collect())
    }

    fn outbox_next_due(&self) -> Result<Option<i64>> {
        let inner = self.inner.lock();
        Ok(inner
            .outbox
            .values()
            .filter_map(|r| match r.state {
                OutboxState::Pending | OutboxState::Retry => Some(r.next_at),
                OutboxState::Inflight => r.lease_until,
                _ => None,
            })
            .min())
    }

    fn outbox_open_counts(&self) -> Result<Vec<(String, u64)>> {
        let inner = self.inner.lock();
        let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for r in inner.outbox.values().filter(|r| r.state.is_open()) {
            *counts.entry(r.target.clone()).or_default() += 1;
        }
        Ok(counts.into_iter().collect())
    }

    fn outbox_state_counts(&self, cap: u64) -> Result<Vec<(String, OutboxState, u64)>> {
        let inner = self.inner.lock();
        let mut counts: std::collections::BTreeMap<(String, &'static str), (OutboxState, u64)> = Default::default();
        for r in inner.outbox.values().filter(|r| crate::outbox::STATS_STATES.contains(&r.state)) {
            let c = counts.entry((r.target.clone(), r.state.as_str())).or_insert((r.state, 0));
            c.1 = (c.1 + 1).min(cap);
        }
        Ok(counts.into_iter().map(|((t, _), (st, n))| (t, st, n)).collect())
    }

    fn outbox_vacuum(&self, before_ms: i64) -> Result<u64> {
        let mut inner = self.inner.lock();
        let MemInner { outbox, outbox_keys, attempts, .. } = &mut *inner;
        let mut n = 0u64;
        outbox.retain(|_, r| {
            let gone = !r.state.is_open() && r.created_at < before_ms;
            if gone {
                outbox_keys.remove(&(r.target.clone(), r.dedupe.clone()));
                n += 1;
            }
            !gone
        });
        attempts.retain(|a| a.ts >= before_ms);
        Ok(n)
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

    fn kv_purge_expired(&self, ns: &str, before: i64, limit: usize) -> Result<u64> {
        let mut inner = self.inner.lock();
        let Some(map) = inner.kv.get_mut(ns) else { return Ok(0) };
        let expired: Vec<String> = map
            .iter()
            .filter(|(_, (_, exp))| exp.is_some_and(|e| e <= before))
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
