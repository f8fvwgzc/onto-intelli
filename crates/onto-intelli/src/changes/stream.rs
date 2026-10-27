//! Live change streams (docs/design/EVENTS.md, A.4 and A.5): one
//! [`ChangeHub`] per tenant per process, one producer task per subscriber.
//!
//! The hub keeps a ring of the newest records in memory, only while the
//! tenant has subscribers; one tail task fills it from the store when a
//! commit lands (the ontology's change listener sets the head, nothing
//! more, because it runs under the graph write lock). A subscriber whose
//! cursor is at or after the ring's `covered_from` reads from the ring;
//! one that is further behind reads from the store, at most four at a time
//! per tenant.
//!
//! Each producer pulls a page, filters it for its principal and filter,
//! and sends frames into a bounded channel (64). Subscribers with the same
//! principal scope and filter share one evaluation per page. Evaluation
//! runs off the async workers with a time budget per page (a page that
//! runs over is split) and takes the graph read lock per slice of records,
//! so a writer waits at most one slice.
//!
//! A producer holds only a `Weak` reference to its tenant: when the tenant
//! closes (the hub evicts it) the stream ends `tenant_closed` and the
//! tenant's memory goes. A client that reads nothing for
//! `stall_timeout_secs` is cut (`slow_consumer`). Streams re-authenticate
//! their request every `reauth_secs`. Frames arrive in cursor order;
//! delivery is at least once across reconnects (resume after the last
//! cursor received), never exactly once.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures::Stream;
use futures::future::BoxFuture;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tracing::Instrument;

use onto_core::changes::{ChangeFilter, ChangeRecord, ChangesMeta, Cursor, StoredChange, matches, redact, view};
use onto_core::{Access, Entity, Error, Ontology, Principal, Result};

use crate::config::ChangesConfig;
use crate::intelli::Intelli;

use super::EXPIRED_MESSAGE;

/// A stored record with the entity document its commit wrote.
type Row = (StoredChange, Option<Entity>);

/// Frames buffered per subscriber.
pub const CHANNEL: usize = 64;
/// Store reads at once per tenant for subscribers behind the ring.
pub const BACKFILL_READS: usize = 4;
/// Records filtered under one graph read lock.
const SLICE: usize = 64;
/// Evaluated pages a group keeps for members at the same cursor.
const GROUP_PAGES: usize = 8;

/// Streams open in this process, all tenants together.
static PROCESS_SUBSCRIBERS: AtomicUsize = AtomicUsize::new(0);

/// Where a stream starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// After the head: only what commits from now on.
    Now,
    /// The oldest record kept (the floor's commit on).
    Earliest,
    /// After this cursor (a previous frame's id).
    After(Cursor),
}

/// What a subscriber receives, in cursor order. `Reset` and `End` are the
/// last frame of a stream.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    /// A record this reader may see (`gap` and `entity.hidden` included).
    Change(Arc<ChangeRecord>),
    /// Nothing more for this reader up to `cursor` (the last one scanned).
    Heartbeat { cursor: Cursor, head: u64 },
    /// The cursor is older than the floor: records may be missing.
    Reset { code: &'static str, floor: u64, head: u64, message: String },
    /// The stream ended; reconnect after `cursor` (unless the code says otherwise).
    End { code: &'static str, cursor: Cursor, message: String },
}

impl Frame {
    /// The SSE event name.
    pub fn event(&self) -> &'static str {
        match self {
            Frame::Change(_) => "change",
            Frame::Heartbeat { .. } => "heartbeat",
            Frame::Reset { .. } => "reset",
            Frame::End { .. } => "end",
        }
    }

    /// The SSE `id` (resume after it): records and heartbeats only.
    pub fn id(&self) -> Option<Cursor> {
        match self {
            Frame::Change(r) => Some(r.cursor),
            Frame::Heartbeat { cursor, .. } => Some(*cursor),
            _ => None,
        }
    }

    /// The SSE `data`.
    pub fn data(&self) -> Value {
        match self {
            Frame::Change(r) => serde_json::to_value(r.as_ref()).unwrap_or(Value::Null),
            Frame::Heartbeat { cursor, head } => json!({ "cursor": cursor, "head": head }),
            Frame::Reset { code, floor, head, message } => {
                json!({ "code": code, "floor": floor, "head": head, "message": message })
            }
            Frame::End { code, cursor, message } => json!({ "code": code, "cursor": cursor, "message": message }),
        }
    }

    /// `Reset` and `End` close the stream.
    pub fn is_last(&self) -> bool {
        matches!(self, Frame::Reset { .. } | Frame::End { .. })
    }
}

/// Why re-authenticating a stream's request failed.
#[derive(Clone, Debug)]
pub enum ReauthError {
    /// The credential no longer authenticates (revoked, expired): the stream ends.
    Unauthenticated(String),
    /// It authenticates but may no longer use this tenant: the stream ends.
    Denied(String),
    /// Authentication could not be checked now: the stream goes on and
    /// tries again next time.
    Unavailable(String),
}

/// Authenticate the stream's original request again and derive its
/// principal. The transport provides it (HTTP: the request headers).
pub type Reauth = Arc<dyn Fn() -> BoxFuture<'static, std::result::Result<Principal, ReauthError>> + Send + Sync>;

/// One open stream: frames in cursor order, then (once) its last frame.
pub struct Subscription {
    /// Where the stream starts (records after it).
    pub cursor: Cursor,
    rx: mpsc::Receiver<Frame>,
    last: Option<oneshot::Receiver<Frame>>,
}

impl Subscription {
    /// The next frame; `None` once the stream is over (after its last frame).
    pub async fn recv(&mut self) -> Option<Frame> {
        if let Some(f) = self.rx.recv().await {
            return Some(f);
        }
        self.last.take()?.await.ok()
    }

    /// The frames as a stream.
    pub fn into_stream(self) -> impl Stream<Item = Frame> + Send + 'static {
        futures::stream::unfold(self, |mut s| async move { s.recv().await.map(|f| (f, s)) })
    }
}

/// The newest records of a tenant, in memory while it has subscribers.
struct Ring {
    generation: u64,
    /// Every record after this cursor up to `last` is here.
    covered_from: Cursor,
    /// The newest position read (the last record, or the store head).
    last: Cursor,
    head: Cursor,
    floor: u64,
    records: VecDeque<Arc<Row>>,
    bytes: usize,
}

impl Ring {
    fn new(generation: u64, meta: &ChangesMeta) -> Self {
        Ring {
            generation,
            covered_from: meta.head,
            last: meta.head,
            head: meta.head,
            floor: meta.floor,
            records: VecDeque::new(),
            bytes: 0,
        }
    }

    /// Records after `c`, at most `limit`.
    fn after(&self, c: Cursor, limit: usize) -> Vec<Arc<Row>> {
        let start = self.records.partition_point(|r| r.0.record.cursor <= c);
        self.records.range(start..).take(limit).cloned().collect()
    }
}

/// Approximate memory of one ring record.
fn row_bytes(r: &Row) -> usize {
    let doc = serde_json::to_string(&r.0).map_or(0, |s| s.len());
    let entity = r.1.as_ref().and_then(|e| serde_json::to_string(e).ok()).map_or(0, |s| s.len());
    128 + doc + entity
}

/// Subscribers that see the same records: same principal scope, same
/// filter. They share evaluated pages.
type GroupKey = (String, Option<String>, String);

#[derive(Default)]
struct Group {
    pages: tokio::sync::Mutex<VecDeque<Evaluated>>,
}

/// One evaluated page: the records after `from` up to `to` this group sees.
struct Evaluated {
    from: Cursor,
    to: Cursor,
    records: Arc<Vec<Arc<ChangeRecord>>>,
}

#[derive(Default)]
struct Counts {
    tenant: usize,
    per_principal: HashMap<String, usize>,
}

/// The change streams of one tenant in this process.
pub struct ChangeHub {
    cfg: ChangesConfig,
    /// The newest commit seq this process has applied (set by the ontology's
    /// change listener; wakes the tail).
    head: Arc<watch::Sender<u64>>,
    /// The ring's `last`, for producers waiting on new records.
    tail: watch::Sender<Cursor>,
    ring: RwLock<Option<Ring>>,
    generation: AtomicU64,
    backfill: Semaphore,
    groups: Mutex<HashMap<GroupKey, Weak<Group>>>,
    counts: Mutex<Counts>,
    me: Weak<ChangeHub>,
}

impl ChangeHub {
    /// The hub of `onto`: registers the listener that sets the head.
    pub(crate) fn new(onto: &Ontology, cfg: ChangesConfig) -> Arc<Self> {
        let head = Arc::new(watch::Sender::new(0u64));
        let listener = head.clone();
        onto.on_change(move |c| {
            listener.send_if_modified(|h| {
                let newer = c.seq > *h;
                if newer {
                    *h = c.seq;
                }
                newer
            });
        });
        Arc::new_cyclic(|me| ChangeHub {
            cfg,
            head,
            tail: watch::Sender::new(Cursor::default()),
            ring: RwLock::new(None),
            generation: AtomicU64::new(0),
            backfill: Semaphore::new(BACKFILL_READS),
            groups: Mutex::new(HashMap::new()),
            counts: Mutex::new(Counts::default()),
            me: me.clone(),
        })
    }

    /// Memory of the ring (0 without subscribers), for the memory budget.
    pub fn bytes(&self) -> usize {
        self.ring.read().as_ref().map_or(0, |r| r.bytes)
    }

    /// Open streams of this tenant in this process.
    pub fn subscribers(&self) -> usize {
        self.counts.lock().tenant
    }

    /// Open streams in this process, all tenants together.
    pub fn process_subscribers() -> usize {
        PROCESS_SUBSCRIBERS.load(Ordering::Relaxed)
    }

    /// Records in the ring.
    pub fn ring_len(&self) -> usize {
        self.ring.read().as_ref().map_or(0, |r| r.records.len())
    }

    /// Open a stream for `p`. Refuses with `permission_denied` without a
    /// read role, `unavailable` when the store keeps no change log (never an
    /// empty stream that looks healthy) or the tenant is closing, and
    /// `resource_exhausted` over `max_subscribers`,
    /// `max_subscribers_per_principal` or `max_subscribers_process`.
    pub async fn subscribe(
        &self,
        intelli: Weak<Intelli>,
        p: Principal,
        f: ChangeFilter,
        start: Start,
        reauth: Option<Reauth>,
    ) -> Result<Subscription> {
        f.check()?;
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let me = self.me.upgrade().ok_or_else(|| Error::Unavailable("the tenant is closing".into()))?;
        let (store, closing) = {
            let i = intelli.upgrade().ok_or_else(|| Error::Unavailable("the tenant is closing".into()))?;
            if i.is_closing() {
                return Err(Error::Unavailable("the tenant is closing; retry".into()));
            }
            (i.ontology().store().clone(), i.closing_signal())
        };
        let meta = tokio::task::spawn_blocking(move || store.changes_meta())
            .await
            .map_err(|e| Error::Other(format!("reading the change feed head: {e}")))??;
        let (cursor, valid_from) = match start {
            Start::Now => (meta.head, meta.head.seq),
            Start::Earliest if meta.floor == 0 => (Cursor::default(), 0),
            // Records of the floor's commit are kept, so start just before it.
            Start::Earliest => (Cursor::new(meta.floor - 1, u32::MAX), meta.floor),
            Start::After(c) => (c, c.seq),
        };
        let guard = Guard::admit(&me, &p.id)?;
        if let Some(generation) = self.ensure_ring(&meta) {
            spawn_tail(Arc::downgrade(&me), intelli.clone(), generation, closing.clone());
        }
        let group = self.group(&p, &f);
        let (tx, rx) = mpsc::channel(CHANNEL);
        let (last_tx, last_rx) = oneshot::channel();
        let span = tracing::info_span!("changes.subscribe", principal = %p.id, cursor = %cursor);
        let producer = Producer {
            hub: me,
            intelli,
            closing,
            p,
            f: Arc::new(f),
            group,
            cursor,
            valid_from,
            tx,
            last: Some(last_tx),
            reauth,
            _guard: guard,
        };
        tokio::spawn(producer.run().instrument(span));
        Ok(Subscription { cursor, rx, last: Some(last_rx) })
    }

    /// Allocate the ring when there is none; returns the new generation (a
    /// tail task must start for it).
    fn ensure_ring(&self, meta: &ChangesMeta) -> Option<u64> {
        let mut ring = self.ring.write();
        if ring.is_some() {
            return None;
        }
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *ring = Some(Ring::new(generation, meta));
        self.tail.send_replace(meta.head);
        Some(generation)
    }

    fn group(&self, p: &Principal, f: &ChangeFilter) -> Arc<Group> {
        let key = (p.scope_key(), p.entity.clone(), serde_json::to_string(f).unwrap_or_default());
        let mut groups = self.groups.lock();
        groups.retain(|_, g| g.strong_count() > 0);
        if let Some(g) = groups.get(&key).and_then(Weak::upgrade) {
            return g;
        }
        let g = Arc::new(Group::default());
        groups.insert(key, Arc::downgrade(&g));
        g
    }

    /// Append a store page to the ring of `generation`. Returns whether the
    /// page was full (more may be waiting).
    fn append(&self, generation: u64, page: onto_core::changes::StoredPage, limit: usize) -> bool {
        let full = page.changes.len() >= limit;
        let last = {
            let mut guard = self.ring.write();
            let Some(ring) = guard.as_mut().filter(|r| r.generation == generation) else { return false };
            for row in page.changes {
                if row.0.record.cursor <= ring.last {
                    continue;
                }
                ring.last = row.0.record.cursor;
                ring.bytes += row_bytes(&row);
                ring.records.push_back(Arc::new(row));
                while ring.records.len() > self.cfg.ring {
                    if let Some(old) = ring.records.pop_front() {
                        ring.bytes = ring.bytes.saturating_sub(row_bytes(&old));
                        ring.covered_from = old.0.record.cursor;
                    }
                }
            }
            if !full {
                ring.last = ring.last.max(page.head);
            }
            ring.head = ring.head.max(page.head);
            ring.floor = page.floor;
            ring.last
        };
        self.tail.send_if_modified(|t| {
            let moved = *t != last;
            *t = last;
            moved
        });
        full
    }

    /// Records after `c` and the ring's position, when the ring covers `c`.
    fn ring_after(&self, c: Cursor, limit: usize) -> Option<(Vec<Arc<Row>>, Cursor)> {
        let guard = self.ring.read();
        let ring = guard.as_ref()?;
        (c >= ring.covered_from).then(|| (ring.after(c, limit), ring.last))
    }

    /// The head and floor as last read (the ring's, or the listener's head).
    fn position(&self) -> (u64, u64) {
        let local = *self.head.borrow();
        match self.ring.read().as_ref() {
            Some(r) => (r.head.seq.max(local), r.floor),
            None => (local, 0),
        }
    }

    /// The ring goes with the last subscriber.
    fn release(&self, principal: &str) {
        let mut counts = self.counts.lock();
        counts.tenant = counts.tenant.saturating_sub(1);
        if let Some(n) = counts.per_principal.get_mut(principal) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.per_principal.remove(principal);
            }
        }
        if counts.tenant == 0 {
            *self.ring.write() = None;
        }
        PROCESS_SUBSCRIBERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One subscriber counted against the limits until it is dropped.
struct Guard {
    hub: Arc<ChangeHub>,
    principal: String,
}

impl Guard {
    fn admit(hub: &Arc<ChangeHub>, principal: &str) -> Result<Guard> {
        let cfg = &hub.cfg;
        let mut counts = hub.counts.lock();
        if counts.tenant >= cfg.max_subscribers {
            return Err(Error::Limited(format!(
                "changes.max_subscribers ({}) streams are open on this tenant in this process",
                cfg.max_subscribers
            )));
        }
        let mine = counts.per_principal.get(principal).copied().unwrap_or(0);
        if mine >= cfg.max_subscribers_per_principal {
            return Err(Error::Limited(format!(
                "{principal} has changes.max_subscribers_per_principal ({}) streams open",
                cfg.max_subscribers_per_principal
            )));
        }
        let before = PROCESS_SUBSCRIBERS.fetch_add(1, Ordering::AcqRel);
        if before >= cfg.max_subscribers_process {
            PROCESS_SUBSCRIBERS.fetch_sub(1, Ordering::AcqRel);
            return Err(Error::Limited(format!(
                "changes.max_subscribers_process ({}) streams are open in this process",
                cfg.max_subscribers_process
            )));
        }
        counts.tenant += 1;
        *counts.per_principal.entry(principal.to_string()).or_default() += 1;
        Ok(Guard { hub: hub.clone(), principal: principal.to_string() })
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.hub.release(&self.principal);
    }
}

/// Fill the ring from the store whenever the head moves (and every
/// heartbeat, so the floor stays current), until the ring of `generation`
/// is gone or the tenant closes.
fn spawn_tail(hub: Weak<ChangeHub>, intelli: Weak<Intelli>, generation: u64, mut closing: watch::Receiver<bool>) {
    let Some(h) = hub.upgrade() else { return };
    let mut head = h.head.subscribe();
    let (page, every) = (h.cfg.page, Duration::from_secs(h.cfg.heartbeat_secs));
    drop(h);
    tokio::spawn(async move {
        loop {
            let (store, after) = {
                let Some(h) = hub.upgrade() else { break };
                let after = match h.ring.read().as_ref() {
                    Some(r) if r.generation == generation => r.last,
                    _ => break,
                };
                let Some(i) = intelli.upgrade() else { break };
                (i.ontology().store().clone(), after)
            };
            let read = tokio::task::spawn_blocking(move || store.changes_since(after, page)).await;
            let full = match read {
                Ok(Ok(p)) => match hub.upgrade() {
                    Some(h) => h.append(generation, p, page),
                    None => break,
                },
                Ok(Err(e)) => {
                    tracing::warn!("change stream tail: {e}");
                    false
                }
                Err(e) => {
                    tracing::warn!("change stream tail: {e}");
                    false
                }
            };
            if full {
                continue;
            }
            tokio::select! {
                changed = head.changed() => if changed.is_err() { break },
                _ = tokio::time::sleep(every) => {}
                _ = closing.changed() => break,
            }
            if *closing.borrow() {
                break;
            }
        }
    });
}

/// What the next read found.
enum Next {
    /// Records after the cursor (up to a page).
    Rows(Vec<Arc<Row>>),
    /// Nothing after the cursor up to `at` (resume there).
    Caught(Cursor),
    /// The cursor is older than the floor.
    Expired { floor: u64, head: u64 },
}

/// How a stream ends.
struct Stop(Frame);

struct Producer {
    hub: Arc<ChangeHub>,
    intelli: Weak<Intelli>,
    closing: watch::Receiver<bool>,
    p: Principal,
    f: Arc<ChangeFilter>,
    group: Arc<Group>,
    cursor: Cursor,
    /// Records before this seq were promised: expired iff it is below the floor.
    valid_from: u64,
    tx: mpsc::Sender<Frame>,
    last: Option<oneshot::Sender<Frame>>,
    reauth: Option<Reauth>,
    _guard: Guard,
}

impl Producer {
    async fn run(mut self) {
        let cfg = self.hub.cfg.clone();
        let heartbeat = Duration::from_secs(cfg.heartbeat_secs);
        let reauth_every = Duration::from_secs(cfg.reauth_secs);
        let mut next_beat = Instant::now() + heartbeat;
        let mut next_reauth = Instant::now() + reauth_every;
        let mut tail = self.hub.tail.subscribe();
        let stop = loop {
            let caught = match self.deliver(&cfg).await {
                Ok(caught) => caught,
                Err(None) => return,
                Err(Some(Stop(f))) => break f,
            };
            let now = Instant::now();
            if now >= next_beat {
                next_beat = now + heartbeat;
                if let Some(i) = self.intelli.upgrade() {
                    i.touch_stream();
                }
                let (head, _) = self.hub.position();
                let beat = Frame::Heartbeat { cursor: self.cursor, head: head.max(self.cursor.seq) };
                match self.send(beat, &cfg).await {
                    Ok(()) => {}
                    Err(None) => return,
                    Err(Some(Stop(f))) => break f,
                }
            }
            if now >= next_reauth {
                next_reauth = now + reauth_every;
                if let Err(Stop(f)) = self.reauthenticate().await {
                    break f;
                }
            }
            if self.closed() {
                break self.tenant_closed();
            }
            if !caught {
                continue;
            }
            let wake = tokio::time::Instant::from_std(next_beat.min(next_reauth));
            tokio::select! {
                changed = tail.changed() => if changed.is_err() { break self.tenant_closed() },
                _ = self.closing.changed() => {}
                _ = tokio::time::sleep_until(wake) => {}
                _ = self.tx.closed() => return,
            }
        };
        if let Some(last) = self.last.take() {
            let _ = last.send(stop);
        }
    }

    fn closed(&self) -> bool {
        *self.closing.borrow() || self.intelli.strong_count() == 0
    }

    fn tenant_closed(&self) -> Frame {
        Frame::End {
            code: "tenant_closed",
            cursor: self.cursor,
            message: "the tenant was closed on this server; reconnect with Last-Event-ID to resume".into(),
        }
    }

    /// Read and send one page; `Ok(true)` when nothing new is left.
    /// `Err(None)`: the client went away.
    async fn deliver(&mut self, cfg: &ChangesConfig) -> std::result::Result<bool, Option<Stop>> {
        if self.closed() {
            return Err(Some(Stop(self.tenant_closed())));
        }
        let (head, floor) = self.hub.position();
        if self.valid_from.max(self.cursor.seq) < floor {
            return Err(Some(Stop(self.reset(floor, head))));
        }
        let behind = head.saturating_sub(self.cursor.seq);
        if behind > cfg.max_backfill_seqs {
            return Err(Some(Stop(Frame::End {
                code: "too_far_behind",
                cursor: self.cursor,
                message: format!(
                    "{behind} commits behind the head (more than changes.max_backfill_seqs = {}): page with \
                     changes.poll after this cursor, then stream again",
                    cfg.max_backfill_seqs
                ),
            })));
        }
        let next = match self.read(cfg).await {
            Ok(n) => n,
            Err(e) => {
                return Err(Some(Stop(Frame::End {
                    code: e.code(),
                    cursor: self.cursor,
                    message: format!("reading the change feed: {e}"),
                })));
            }
        };
        let rows = match next {
            Next::Expired { floor, head } => return Err(Some(Stop(self.reset(floor, head)))),
            Next::Caught(at) => {
                self.cursor = self.cursor.max(at);
                return Ok(true);
            }
            Next::Rows(rows) => rows,
        };
        let (records, to) = match self.evaluate(rows, cfg).await {
            Ok(r) => r,
            Err(e) => {
                return Err(Some(Stop(Frame::End {
                    code: e.code(),
                    cursor: self.cursor,
                    message: format!("filtering the change feed: {e}"),
                })));
            }
        };
        for r in records.iter() {
            self.send(Frame::Change(r.clone()), cfg).await?;
        }
        self.cursor = to;
        Ok(false)
    }

    fn reset(&self, floor: u64, head: u64) -> Frame {
        Frame::Reset { code: "cursor_expired", floor, head, message: EXPIRED_MESSAGE.into() }
    }

    async fn send(&self, frame: Frame, cfg: &ChangesConfig) -> std::result::Result<(), Option<Stop>> {
        let stall = Duration::from_secs(cfg.stall_timeout_secs);
        match self.tx.send_timeout(frame, stall).await {
            Ok(()) => Ok(()),
            Err(mpsc::error::SendTimeoutError::Closed(_)) => Err(None),
            Err(mpsc::error::SendTimeoutError::Timeout(_)) => Err(Some(Stop(Frame::End {
                code: "slow_consumer",
                cursor: self.cursor,
                message: format!(
                    "the client read nothing for {} s (changes.stall_timeout_secs); reconnect with Last-Event-ID to \
                     resume",
                    cfg.stall_timeout_secs
                ),
            }))),
        }
    }

    /// The next page after the cursor: from the ring when it covers the
    /// cursor, else from the store (at most [`BACKFILL_READS`] at once).
    async fn read(&self, cfg: &ChangesConfig) -> Result<Next> {
        if let Some((rows, last)) = self.hub.ring_after(self.cursor, cfg.page) {
            return Ok(if rows.is_empty() { Next::Caught(last) } else { Next::Rows(rows) });
        }
        let _permit = self.hub.backfill.acquire().await.map_err(|e| Error::Unavailable(e.to_string()))?;
        let store = {
            let i = self.intelli.upgrade().ok_or_else(|| Error::Unavailable("the tenant closed".into()))?;
            i.ontology().store().clone()
        };
        let (after, limit) = (self.cursor, cfg.page);
        let page = tokio::task::spawn_blocking(move || store.changes_since(after, limit))
            .await
            .map_err(|e| Error::Other(format!("reading the change feed: {e}")))??;
        if self.valid_from.max(self.cursor.seq) < page.floor {
            return Ok(Next::Expired { floor: page.floor, head: page.head.seq });
        }
        if page.changes.is_empty() {
            return Ok(Next::Caught(page.head));
        }
        Ok(Next::Rows(page.changes.into_iter().map(Arc::new).collect()))
    }

    /// The records of `rows` this subscriber gets, and the cursor it has
    /// scanned up to: evaluated once per group (same principal scope and
    /// filter) and page, off the async workers, split when it runs over
    /// `eval_budget_ms`.
    async fn evaluate(
        &self,
        rows: Vec<Arc<Row>>,
        cfg: &ChangesConfig,
    ) -> Result<(Arc<Vec<Arc<ChangeRecord>>>, Cursor)> {
        let from = self.cursor;
        let mut pages = self.group.pages.lock().await;
        if let Some(e) = pages.iter().find(|e| e.from == from) {
            return Ok((e.records.clone(), e.to));
        }
        let onto = {
            let i = self.intelli.upgrade().ok_or_else(|| Error::Unavailable("the tenant closed".into()))?;
            i.ontology().clone()
        };
        let (p, f) = (self.p.clone(), self.f.clone());
        let budget = Duration::from_millis(cfg.eval_budget_ms);
        let (records, to) = tokio::task::spawn_blocking(move || evaluate_rows(&onto, &p, &f, &rows, from, budget))
            .await
            .map_err(|e| Error::Other(format!("filtering the change feed: {e}")))?;
        let records = Arc::new(records);
        pages.push_back(Evaluated { from, to, records: records.clone() });
        while pages.len() > GROUP_PAGES {
            pages.pop_front();
        }
        Ok((records, to))
    }

    /// Re-derive the principal from the original request. A revoked
    /// credential ends `unauthenticated`, a tenant it may no longer use
    /// `permission_denied`, a different scope (id, roles, workspaces or
    /// entity) `principal_changed`.
    async fn reauthenticate(&mut self) -> std::result::Result<(), Stop> {
        let Some(reauth) = self.reauth.clone() else { return Ok(()) };
        let end = |code: &'static str, message: String| Stop(Frame::End { code, cursor: self.cursor, message });
        match reauth().await {
            Ok(p) if p.scope_key() != self.p.scope_key() || p.entity != self.p.entity => Err(end(
                "principal_changed",
                "the caller's roles, workspaces or entity changed; reconnect to stream with the new scope".into(),
            )),
            Ok(p) if !p.allows(Access::Read) => Err(end("permission_denied", format!("{} has no read role", p.id))),
            Ok(_) => Ok(()),
            Err(ReauthError::Unauthenticated(m)) => {
                Err(end("unauthenticated", format!("the credential no longer authenticates: {m}")))
            }
            Err(ReauthError::Denied(m)) => Err(end("permission_denied", m)),
            Err(ReauthError::Unavailable(m)) => {
                tracing::warn!("change stream re-authentication could not be checked: {m}");
                Ok(())
            }
        }
    }
}

/// Filter `rows` (records after `from`) for `p` and `f`, a slice at a time
/// under the graph read lock. Stops after the slice that passes `budget`
/// (at least one slice); returns what was found and the last cursor scanned.
fn evaluate_rows(
    onto: &Ontology,
    p: &Principal,
    f: &ChangeFilter,
    rows: &[Arc<Row>],
    from: Cursor,
    budget: Duration,
) -> (Vec<Arc<ChangeRecord>>, Cursor) {
    let started = Instant::now();
    let mut out = Vec::new();
    let mut to = from;
    for slice in rows.chunks(SLICE) {
        onto.read(|g, schema| {
            for row in slice {
                let (stored, entity) = row.as_ref();
                to = stored.record.cursor;
                if !matches(f, schema, &stored.record) {
                    continue;
                }
                let record = stored.joined(entity.clone());
                if let Some(mut v) = view(p, &record, stored.before_vis.as_ref(), g) {
                    redact(&mut v, f.fields);
                    out.push(Arc::new(v));
                }
            }
        });
        if started.elapsed() >= budget {
            break;
        }
    }
    (out, to)
}
