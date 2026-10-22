//! Episodic and long-term memory.
//!
//! * **Episodic**: what was asked and answered, by whom, about which entities.
//!   Expires after `episodic_ttl_days`. Asking the same question again updates
//!   its episode in place instead of adding a new one.
//! * **Long-term**: durable facts, preferences and consolidated summaries.
//!   Written explicitly (`memory.remember`) or promoted by [`Memory::consolidate`]
//!   when the same entities keep coming up. A write that repeats a fact merges
//!   into it; a write that changes a fact about the same entities supersedes it
//!   (the old item is closed with `valid_to`, not deleted).
//!
//! Two timelines, as in Zep: `valid_from`/`valid_to` are world time (when a
//! fact was true), `created_at` is ingestion time, and `expires_at` plus
//! deletion are system expiry.
//!
//! Recall blends saturating BM25 relevance, entity overlap, ACT-R activation
//! (use and age) and salience, drops near-duplicates (MMR) and can fill a token
//! budget. It is scoped: private memories are only visible to their owner.
//!
//! Background: `docs/MEMORY_RESEARCH.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use onto_core::store::{Store, StoreNotice};
use onto_core::text::{TextIndex, similarity, tokenize};
use onto_core::time::{new_id, now_ms};
use onto_core::{Error, Id, Principal, Result};

use crate::config::MemoryConfig;

const NS: &str = "memory";
const DAY_MS: i64 = 86_400_000;
const HOUR_MS: f64 = 3_600_000.0;
/// Characters of a memory that the context keeps (context.rs clips to the same length).
const PACKED_CHARS: usize = 320;
/// Tokens per packed memory beyond its text ("memory (long-term): " and the separator).
const PACKED_OVERHEAD: usize = 8;
/// Candidates considered when deciding whether a write merges or supersedes.
const MAX_WRITE_CANDIDATES: usize = 50;

/// Milliseconds since the epoch. Injectable so time-based behaviour is testable.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Episodic,
    LongTerm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Episode,
    Fact,
    Preference,
    Summary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: String,
    pub workspace: String,
    pub scope: MemoryScope,
    pub kind: MemoryKind,
    pub text: String,
    #[serde(default)]
    pub entities: Vec<Id>,
    /// Owner principal for private memories; `None` = shared in the workspace.
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub conversation: Option<String>,
    pub salience: f32,
    #[serde(default)]
    pub hits: u32,
    /// Ingestion time.
    pub created_at: i64,
    pub last_access: i64,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub consolidated: bool,
    /// Normalized question for episodes (lets callers skip repeats of the same question).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// World time the fact became true; `None` means `created_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<i64>,
    /// World time the fact stopped being true; `None` means it is current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<i64>,
    /// The item that replaced this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// The item this one replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// Most recent access times, newest first, at most `activation_k`. Empty on
    /// items written before access tracking, which read it as `[last_access]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<i64>,
}

impl MemoryItem {
    /// True at world time `as_of`.
    pub fn is_current(&self, as_of: i64) -> bool {
        self.valid_from.unwrap_or(self.created_at) <= as_of && self.valid_to.is_none_or(|v| v > as_of)
    }

    fn alive(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|e| e > now)
    }

    /// Record one use at `now`.
    fn touch(&mut self, now: i64, k: usize) {
        if self.recent.is_empty() {
            self.recent.push(self.last_access);
        }
        self.recent.insert(0, now);
        self.recent.truncate(k.max(1));
        self.hits = self.hits.saturating_add(1);
        self.last_access = now;
    }

    fn is_fact_like(&self) -> bool {
        self.scope == MemoryScope::LongTerm && matches!(self.kind, MemoryKind::Fact | MemoryKind::Preference)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryHit {
    pub item: MemoryItem,
    pub score: f32,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RememberInput {
    pub text: String,
    #[serde(default)]
    pub entities: Vec<Id>,
    #[serde(default)]
    pub kind: Option<MemoryKind>,
    /// Only visible to the principal who wrote it.
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub salience: Option<f32>,
    /// Explicitly replace this memory (it is closed, not deleted).
    #[serde(default)]
    pub supersedes: Option<String>,
    /// `false` turns off automatic superseding for this write.
    #[serde(default)]
    pub supersede: Option<bool>,
    /// World time the fact became true (default: now).
    #[serde(default)]
    pub valid_from: Option<i64>,
}

/// Options for [`MemoryState::recall_with`].
#[derive(Clone, Debug, Default)]
pub struct RecallOptions {
    /// Recall what was true at this world time (ms). Does not reinforce.
    pub as_of: Option<i64>,
    /// Stop adding memories once their packed size would exceed this many tokens.
    pub budget_tokens: Option<usize>,
    /// Also return superseded items.
    pub include_superseded: bool,
    /// Skip episodes of this normalized question (an answer is not evidence for itself).
    pub exclude_key: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ConsolidationReport {
    pub promoted: usize,
    pub updated: usize,
    pub expired: usize,
    pub long_term: usize,
    pub episodic: usize,
    /// Consolidated episodes deleted because they faded.
    pub pruned: usize,
    /// Items deleted to keep a subject under its cap.
    pub evicted: usize,
    /// Items whose access statistics were persisted.
    pub flushed: usize,
    /// Summaries left alone because nothing changed.
    pub unchanged: usize,
    /// Another consolidation was already running; nothing was done.
    pub skipped: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MemoryStats {
    pub episodic: usize,
    pub long_term: usize,
    pub revision: u64,
    /// Items closed by a newer one (kept for history).
    pub superseded: usize,
    // Counters below are for this process, since it opened the memory.
    pub merged: u64,
    pub superseded_total: u64,
    pub pruned: u64,
    pub evicted: u64,
    pub reinforced: u64,
    pub recalls: u64,
    pub recall_items: u64,
    pub recall_tokens: u64,
    pub avg_recall_tokens: f32,
}

#[derive(Default)]
struct Counters {
    merged: AtomicU64,
    superseded: AtomicU64,
    pruned: AtomicU64,
    evicted: AtomicU64,
    reinforced: AtomicU64,
    recalls: AtomicU64,
    recall_items: AtomicU64,
    recall_tokens: AtomicU64,
}

fn inc(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

/// Episodic and long-term memory. Cheap to clone and share; other processes'
/// writes to a shared store arrive through store notices.
#[derive(Clone)]
pub struct Memory {
    state: Arc<MemoryState>,
}

impl std::ops::Deref for Memory {
    type Target = MemoryState;
    fn deref(&self) -> &MemoryState {
        &self.state
    }
}

impl Memory {
    pub fn open(store: Arc<dyn Store>, cfg: MemoryConfig) -> Result<Self> {
        Self::open_with_clock(store, cfg, Arc::new(now_ms))
    }

    pub fn open_with_clock(store: Arc<dyn Store>, cfg: MemoryConfig, clock: Clock) -> Result<Self> {
        let state = Arc::new(MemoryState::load(store, cfg, clock)?);
        let weak = Arc::downgrade(&state);
        state.store.subscribe(Arc::new(move |n: StoreNotice| {
            let Some(state) = weak.upgrade() else { return };
            match n {
                StoreNotice::Kv { ns, key, .. } if ns == NS => {
                    let _ = state.reload(&key);
                }
                StoreNotice::Resync => {
                    let _ = state.reload_all();
                }
                _ => {}
            }
        }))?;
        Ok(Self { state })
    }
}

/// (workspace, subject, normalized question) of an episode.
type EpisodeKey = (String, Option<String>, String);
/// (workspace, subject, first three sorted entity ids joined by `|`).
type GroupKey = (String, Option<String>, String);

/// Lookup structures over the items, kept under one lock.
#[derive(Default)]
struct Indexes {
    text: TextIndex,
    by_entity: HashMap<Id, HashSet<String>>,
    keys: HashMap<EpisodeKey, String>,
}

fn episode_key(item: &MemoryItem) -> Option<EpisodeKey> {
    if item.scope != MemoryScope::Episodic || item.valid_to.is_some() {
        return None;
    }
    item.key.as_ref().map(|k| (item.workspace.clone(), item.subject.clone(), k.clone()))
}

impl Indexes {
    fn add(&mut self, item: &MemoryItem) {
        self.text.upsert(&item.id, &format!("{} {}", item.text, item.entities.join(" ")));
        for e in &item.entities {
            self.by_entity.entry(e.clone()).or_default().insert(item.id.clone());
        }
        if let Some(k) = episode_key(item) {
            self.keys.insert(k, item.id.clone());
        }
    }

    fn remove(&mut self, item: &MemoryItem) {
        self.text.remove(&item.id);
        for e in &item.entities {
            if let Some(set) = self.by_entity.get_mut(e) {
                set.remove(&item.id);
                if set.is_empty() {
                    self.by_entity.remove(e);
                }
            }
        }
        if let Some(k) = episode_key(item)
            && self.keys.get(&k) == Some(&item.id)
        {
            self.keys.remove(&k);
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

pub struct MemoryState {
    store: Arc<dyn Store>,
    items: RwLock<HashMap<String, MemoryItem>>,
    idx: RwLock<Indexes>,
    cfg: MemoryConfig,
    clock: Clock,
    /// Bumped whenever visible long-term memory changes (for stats).
    revision: AtomicU64,
    /// Per-workspace bump counts; see [`MemoryState::revision_for`].
    ws_rev: RwLock<HashMap<String, u64>>,
    /// Bumped when every workspace may have changed at once (a full reload).
    epoch: AtomicU64,
    counters: Counters,
    /// Items reinforced by recall whose access statistics are not persisted yet.
    dirty: Mutex<HashSet<String>>,
    /// Serialises write decisions (merge, supersede) within this process.
    write_lock: Mutex<()>,
    consolidating: AtomicBool,
}

/// Visible content of two versions is the same: they recall and render alike.
/// Access statistics, expiry and the consolidation flag do not count.
fn visible_eq(a: &MemoryItem, b: &MemoryItem) -> bool {
    a.workspace == b.workspace
        && a.scope == b.scope
        && a.kind == b.kind
        && a.text == b.text
        && a.entities == b.entities
        && a.subject == b.subject
        && a.valid_from == b.valid_from
        && a.valid_to == b.valid_to
        && a.superseded_by == b.superseded_by
}

/// Keep `local`'s unflushed reinforcement when another process rewrote the item.
fn merge_access(fresh: &mut MemoryItem, local: &MemoryItem, k: usize) {
    fresh.hits = fresh.hits.max(local.hits);
    fresh.last_access = fresh.last_access.max(local.last_access);
    let mut recent: Vec<i64> = fresh.recent.iter().chain(&local.recent).copied().collect();
    recent.sort_unstable_by(|a, b| b.cmp(a));
    recent.dedup();
    recent.truncate(k.max(1));
    fresh.recent = recent;
}

impl MemoryState {
    fn load(store: Arc<dyn Store>, cfg: MemoryConfig, clock: Clock) -> Result<Self> {
        let mem = Self {
            store,
            items: RwLock::new(HashMap::new()),
            idx: RwLock::new(Indexes::default()),
            cfg,
            clock,
            revision: AtomicU64::new(1),
            ws_rev: RwLock::new(HashMap::new()),
            epoch: AtomicU64::new(0),
            counters: Counters::default(),
            dirty: Mutex::new(HashSet::new()),
            write_lock: Mutex::new(()),
            consolidating: AtomicBool::new(false),
        };
        let now = mem.now();
        {
            let mut items = mem.items.write();
            let mut idx = mem.idx.write();
            for (_, v) in mem.store.kv_scan(NS, "")? {
                if let Ok(item) = serde_json::from_value::<MemoryItem>(v)
                    && item.alive(now)
                {
                    idx.add(&item);
                    items.insert(item.id.clone(), item);
                }
            }
        }
        Ok(mem)
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn bump(&self, ws: &str) {
        self.revision.fetch_add(1, Ordering::SeqCst);
        *self.ws_rev.write().entry(ws.to_string()).or_insert(0) += 1;
    }

    /// Another process wrote or deleted this item.
    fn reload(&self, id: &str) -> Result<()> {
        let mut fresh = self.store.kv_get(NS, id)?.and_then(|v| serde_json::from_value::<MemoryItem>(v).ok());
        let old = self.items.write().remove(id);
        if let Some(old) = &old {
            self.idx.write().remove(old);
        }
        if let (Some(f), Some(o)) = (fresh.as_mut(), old.as_ref())
            && self.dirty.lock().contains(id)
        {
            merge_access(f, o, self.cfg.activation_k);
        }
        let changed = match (&old, &fresh) {
            (Some(o), Some(f)) => {
                (o.scope == MemoryScope::LongTerm || f.scope == MemoryScope::LongTerm) && !visible_eq(o, f)
            }
            (Some(o), None) => o.scope == MemoryScope::LongTerm,
            (None, Some(f)) => f.scope == MemoryScope::LongTerm,
            (None, None) => false,
        };
        let ws = fresh.as_ref().or(old.as_ref()).map(|i| i.workspace.clone());
        if let Some(item) = fresh {
            self.idx.write().add(&item);
            self.items.write().insert(item.id.clone(), item);
        } else {
            self.dirty.lock().remove(id);
        }
        if changed && let Some(ws) = ws {
            self.bump(&ws);
        }
        Ok(())
    }

    /// Notices may have been missed: reload everything.
    fn reload_all(&self) -> Result<()> {
        let now = self.now();
        let mut fresh: Vec<MemoryItem> = self
            .store
            .kv_scan(NS, "")?
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_value::<MemoryItem>(v).ok())
            .filter(|i| i.alive(now))
            .collect();
        let dirty = self.dirty.lock().clone();
        let mut items = self.items.write();
        let mut idx = self.idx.write();
        for f in &mut fresh {
            if dirty.contains(&f.id)
                && let Some(local) = items.get(&f.id)
            {
                merge_access(f, local, self.cfg.activation_k);
            }
        }
        items.clear();
        idx.clear();
        for item in fresh {
            idx.add(&item);
            items.insert(item.id.clone(), item);
        }
        self.ws_rev.write().clear();
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.revision.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Changes whenever visible long-term memory changes, in any workspace.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    /// Changes whenever visible long-term memory of workspace `ws` changes
    /// (part of answer-cache keys, so a write in one workspace leaves the
    /// others' caches alone).
    pub fn revision_for(&self, ws: &str) -> u64 {
        let n = self.ws_rev.read().get(ws).copied().unwrap_or(0);
        self.epoch.load(Ordering::SeqCst).wrapping_shl(32) ^ n
    }

    /// Write the whole item and index it. Bumps the revision only when visible
    /// long-term content changed.
    fn save(&self, item: MemoryItem) -> Result<MemoryItem> {
        self.store.kv_put(NS, &item.id, &serde_json::to_value(&item)?, item.expires_at)?;
        let old = self.items.write().insert(item.id.clone(), item.clone());
        {
            let mut idx = self.idx.write();
            if let Some(o) = &old {
                idx.remove(o);
            }
            idx.add(&item);
        }
        // The full item, access statistics included, is persisted now.
        self.dirty.lock().remove(&item.id);
        let long_term =
            item.scope == MemoryScope::LongTerm || old.as_ref().is_some_and(|o| o.scope == MemoryScope::LongTerm);
        if long_term && old.as_ref().is_none_or(|o| !visible_eq(o, &item)) {
            self.bump(&item.workspace);
        }
        Ok(item)
    }

    /// Persist an item whose visible content is unchanged (access statistics):
    /// no revision bump, no reindexing.
    fn save_quiet(&self, item: &MemoryItem) -> Result<()> {
        self.store.kv_put(NS, &item.id, &serde_json::to_value(item)?, item.expires_at)
    }

    fn delete_item(&self, item: &MemoryItem) -> Result<()> {
        self.store.kv_delete(NS, &item.id)?;
        let removed = self.items.write().remove(&item.id);
        self.idx.write().remove(removed.as_ref().unwrap_or(item));
        self.dirty.lock().remove(&item.id);
        if item.scope == MemoryScope::LongTerm {
            self.bump(&item.workspace);
        }
        Ok(())
    }

    /// Persist the access statistics of items reinforced by recall. Returns how
    /// many were written.
    pub fn flush_access(&self) -> Result<usize> {
        let ids: Vec<String> = self.dirty.lock().drain().collect();
        let mut written = 0;
        for (i, id) in ids.iter().enumerate() {
            match self.flush_one(id) {
                Ok(true) => written += 1,
                Ok(false) => {}
                Err(e) => {
                    self.dirty.lock().extend(ids[i..].iter().cloned());
                    return Err(e);
                }
            }
        }
        Ok(written)
    }

    /// Merge one item's local access statistics into its stored version and
    /// write that back. The local copy is never written whole: a supersede or
    /// delete that reached the store since it was read (from this process or
    /// another) must stay. Returns false when the item is gone.
    fn flush_one(&self, id: &str) -> Result<bool> {
        let _write = self.write_lock.lock();
        let Some(local) = self.get(id) else { return Ok(false) };
        let stored = self.store.kv_get(NS, id)?.and_then(|v| serde_json::from_value::<MemoryItem>(v).ok());
        let Some(mut stored) = stored else { return Ok(false) };
        merge_access(&mut stored, &local, self.cfg.activation_k);
        self.save_quiet(&stored)?;
        Ok(true)
    }

    fn writable(p: &Principal, item: &MemoryItem) -> bool {
        p.is_privileged()
            || item.subject.as_deref() == Some(p.id.as_str())
            || (item.subject.is_none() && p.in_workspace(&item.workspace))
    }

    /// Current facts or preferences a new write could merge into or supersede.
    fn write_candidates(
        &self,
        ws: &str,
        subject: &Option<String>,
        kind: MemoryKind,
        text: &str,
        entities: &[Id],
        now: i64,
    ) -> Vec<MemoryItem> {
        let items = self.items.read();
        let ok = |i: &MemoryItem| {
            i.workspace == ws
                && &i.subject == subject
                && i.kind == kind
                && i.scope == MemoryScope::LongTerm
                && i.valid_to.is_none()
                && i.alive(now)
        };
        let idx = self.idx.read();
        let mut ids: Vec<String> =
            idx.text.search(text, 10, |id| items.get(id).is_some_and(ok)).into_iter().map(|(id, _)| id).collect();
        let mut seen: HashSet<String> = ids.iter().cloned().collect();
        'outer: for e in entities {
            let mut by: Vec<&String> = idx.by_entity.get(e).into_iter().flatten().collect();
            by.sort();
            for id in by {
                if ids.len() >= MAX_WRITE_CANDIDATES {
                    break 'outer;
                }
                if items.get(id).is_some_and(ok) && seen.insert(id.clone()) {
                    ids.push(id.clone());
                }
            }
        }
        ids.iter().filter_map(|id| items.get(id).cloned()).collect()
    }

    /// Store a durable fact or preference. A repeat merges into the existing
    /// item, and a changed fact about the same entities supersedes the old one;
    /// either way the resulting item is returned.
    pub fn remember(&self, p: &Principal, default_ws: &str, input: RememberInput) -> Result<MemoryItem> {
        let text = input.text.trim().to_string();
        if text.is_empty() {
            return Err(Error::invalid("memory text is required"));
        }
        let ws = input.workspace.clone().unwrap_or_else(|| default_ws.to_string());
        if !p.in_workspace(&ws) {
            return Err(Error::Denied(format!("{} has no access to workspace {ws}", p.id)));
        }
        let kind = input.kind.unwrap_or(MemoryKind::Fact);
        let subject = input.private.then(|| p.id.clone());
        let salience = input.salience.unwrap_or(0.7).clamp(0.0, 1.0);
        let _write = self.write_lock.lock();
        let now = self.now();
        let new = MemoryItem {
            id: new_id("mem"),
            workspace: ws.clone(),
            scope: MemoryScope::LongTerm,
            kind,
            text: text.clone(),
            entities: input.entities.clone(),
            subject: subject.clone(),
            conversation: None,
            salience,
            hits: 0,
            created_at: now,
            last_access: now,
            expires_at: None,
            consolidated: false,
            key: None,
            valid_from: input.valid_from,
            valid_to: None,
            superseded_by: None,
            supersedes: None,
            recent: Vec::new(),
        };
        if let Some(target) = input.supersedes.as_deref() {
            let old = self.get(target).filter(|o| {
                o.workspace == ws && Self::visible(p, &ws, o) && Self::writable(p, o) && o.valid_to.is_none()
            });
            let Some(old) = old else {
                return Err(Error::invalid(format!(
                    "memory {target} cannot be superseded: it does not exist, is not current, or is not writable in workspace {ws}"
                )));
            };
            // The replacement must be visible to exactly those who saw the old
            // fact: a private write must not close a shared fact, nor a shared
            // write a private one.
            if old.subject != subject {
                let scope = |s: &Option<String>| if s.is_some() { "private" } else { "shared" };
                return Err(Error::invalid(format!(
                    "memory {target} cannot be superseded by a {} memory: it is {}{}; write the replacement in the same scope",
                    scope(&subject),
                    scope(&old.subject),
                    old.subject.as_deref().map(|s| format!(" to {s}")).unwrap_or_default()
                )));
            }
            return self.supersede(old, new, input.valid_from, now);
        }
        if !matches!(kind, MemoryKind::Fact | MemoryKind::Preference) {
            return self.save(new);
        }
        let candidates = self.write_candidates(&ws, &subject, kind, &text, &input.entities, now);
        match decide(&self.cfg, &text, &input.entities, &candidates, input.supersede != Some(false)) {
            Decision::Merge(id) => {
                // `decide` merges only into an item whose entities already
                // cover the write's, so the entities stay as they are.
                let Some(mut e) = self.get(&id) else { return self.save(new) };
                e.touch(now, self.cfg.activation_k);
                e.salience = e.salience.max(salience);
                let (new_toks, old_toks) = (content_tokens(&text), content_tokens(&e.text));
                if new_toks.len() > old_toks.len() && new_toks.is_superset(&old_toks) {
                    e.text = text;
                }
                inc(&self.counters.merged, 1);
                self.save(e)
            }
            Decision::Supersede(id) => match self.get(&id) {
                Some(old) => self.supersede(old, new, input.valid_from, now),
                None => self.save(new),
            },
            Decision::Add => self.save(new),
        }
    }

    /// Close `old` and store `new` in its place.
    fn supersede(
        &self,
        mut old: MemoryItem,
        mut new: MemoryItem,
        valid_from: Option<i64>,
        now: i64,
    ) -> Result<MemoryItem> {
        let from = valid_from.unwrap_or(now);
        // The old fact stops being true when the new one starts (never before it began).
        old.valid_to = Some(from.max(old.valid_from.unwrap_or(old.created_at)));
        old.superseded_by = Some(new.id.clone());
        old.expires_at =
            (self.cfg.superseded_ttl_days > 0).then(|| now + i64::from(self.cfg.superseded_ttl_days) * DAY_MS);
        new.supersedes = Some(old.id.clone());
        new.valid_from = Some(from);
        // Old first: a replica between the two writes sees no current version,
        // never two conflicting ones.
        self.save(old)?;
        let new = self.save(new)?;
        inc(&self.counters.superseded, 1);
        Ok(new)
    }

    /// Record one question/answer exchange. Asking the same question again
    /// updates the existing episode instead of adding one.
    #[allow(clippy::too_many_arguments)]
    pub fn record_episode(
        &self,
        p: &Principal,
        ws: &str,
        conversation: Option<&str>,
        question: &str,
        answer: &str,
        entities: &[Id],
        salience: f32,
    ) -> Result<MemoryItem> {
        let _write = self.write_lock.lock();
        let now = self.now();
        let text = format!("Q: {} A: {}", clip(question, 240), clip(answer, 400));
        let key = crate::hash::normalize_question(question);
        let subject = Some(p.id.clone());
        let expires_at = Some(now + i64::from(self.cfg.episodic_ttl_days.max(1)) * DAY_MS);
        let salience = salience.clamp(0.0, 1.0);
        let lookup = (ws.to_string(), subject.clone(), key.clone());
        // Take the items lock only after releasing the index lock: recall holds
        // items while it reads the index.
        let id = self.idx.read().keys.get(&lookup).cloned();
        let existing = id.and_then(|id| self.get(&id)).filter(|e| e.scope == MemoryScope::Episodic && e.alive(now));
        if let Some(mut e) = existing {
            e.text = text;
            for x in entities {
                if !e.entities.contains(x) {
                    e.entities.push(x.clone());
                }
            }
            e.touch(now, self.cfg.activation_k);
            e.salience = e.salience.max(salience);
            if let Some(c) = conversation {
                e.conversation = Some(c.to_string());
            }
            e.expires_at = expires_at;
            inc(&self.counters.merged, 1);
            return self.save(e);
        }
        self.save(MemoryItem {
            id: new_id("ep"),
            workspace: ws.to_string(),
            scope: MemoryScope::Episodic,
            kind: MemoryKind::Episode,
            text,
            entities: entities.to_vec(),
            subject,
            conversation: conversation.map(str::to_string),
            salience,
            hits: 0,
            created_at: now,
            last_access: now,
            expires_at,
            consolidated: false,
            key: Some(key),
            valid_from: None,
            valid_to: None,
            superseded_by: None,
            supersedes: None,
            recent: Vec::new(),
        })
    }

    fn visible(p: &Principal, ws: &str, item: &MemoryItem) -> bool {
        item.workspace == ws && item.subject.as_ref().is_none_or(|s| s == &p.id || p.is_privileged())
    }

    /// Top-`k` current memories for a query, boosted by shared entities.
    pub fn recall(&self, p: &Principal, ws: &str, query: &str, entities: &[Id], k: usize) -> Vec<MemoryHit> {
        self.recall_with(p, ws, query, entities, k, &RecallOptions::default())
    }

    /// Top-`k` memories for a query: relevance-ranked, without near-duplicates,
    /// optionally as of a past time and within a token budget.
    pub fn recall_with(
        &self,
        p: &Principal,
        ws: &str,
        query: &str,
        entities: &[Id],
        k: usize,
        opts: &RecallOptions,
    ) -> Vec<MemoryHit> {
        if k == 0 {
            return Vec::new();
        }
        let now = self.now();
        let as_of = opts.as_of.unwrap_or(now);
        let cfg = &self.cfg;
        let items = self.items.read();
        let keep = |id: &str| {
            items.get(id).is_some_and(|i| {
                Self::visible(p, ws, i)
                    && i.alive(now)
                    && (opts.include_superseded || i.is_current(as_of))
                    && opts.exclude_key.as_deref().is_none_or(|k| i.key.as_deref() != Some(k))
            })
        };
        let mut candidates: HashMap<String, f32> = HashMap::new();
        {
            let idx = self.idx.read();
            let sat = cfg.lex_saturation.max(1e-6);
            for (id, s) in idx.text.search(query, 50, keep) {
                candidates.insert(id, s / (s + sat));
            }
            for e in entities {
                for id in idx.by_entity.get(e).into_iter().flatten() {
                    if keep(id) {
                        candidates.entry(id.clone()).or_insert(0.0);
                    }
                }
            }
        }
        let wanted: HashSet<&str> = entities.iter().map(String::as_str).collect();
        let mut pool: Vec<MemoryHit> = candidates
            .into_iter()
            .filter_map(|(id, lex)| {
                let item = items.get(&id)?;
                let overlap = if wanted.is_empty() || item.entities.is_empty() {
                    0.0
                } else {
                    item.entities.iter().filter(|e| wanted.contains(e.as_str())).count() as f32 / wanted.len() as f32
                };
                let long_term_bonus = if item.scope == MemoryScope::LongTerm { 0.1 } else { 0.0 };
                let score =
                    0.5 * lex + 0.3 * overlap + 0.1 * strength(item, now, cfg) + 0.05 * item.salience + long_term_bonus;
                (lex > 0.0 || overlap > 0.0).then(|| MemoryHit { item: item.clone(), score })
            })
            .collect();
        drop(items);
        pool.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.item.created_at.cmp(&a.item.created_at))
                .then_with(|| a.item.id.cmp(&b.item.id))
        });
        pool.truncate((4 * k).max(20));
        let hits = select(pool, k, opts.budget_tokens, cfg.mmr_lambda, cfg.dup_threshold);
        inc(&self.counters.recalls, 1);
        inc(&self.counters.recall_items, hits.len() as u64);
        inc(&self.counters.recall_tokens, hits.iter().map(|h| recall_cost(&h.item) as u64).sum());
        if opts.as_of.is_none() && !hits.is_empty() {
            let mut items = self.items.write();
            let mut dirty = self.dirty.lock();
            for h in &hits {
                if let Some(i) = items.get_mut(&h.item.id) {
                    i.touch(now, cfg.activation_k);
                    dirty.insert(i.id.clone());
                }
            }
            inc(&self.counters.reinforced, hits.len() as u64);
        }
        hits
    }

    pub fn get(&self, id: &str) -> Option<MemoryItem> {
        self.items.read().get(id).cloned()
    }

    /// Delete a memory. Deleting the current item of a supersede chain reopens
    /// the one it replaced; deleting an older link joins its neighbours, so the
    /// item before it stays closed and now ends where the deleted one ended.
    pub fn forget(&self, p: &Principal, id: &str) -> Result<bool> {
        let _write = self.write_lock.lock();
        let Some(item) = self.get(id) else { return Ok(false) };
        if !Self::writable(p, &item) {
            return Err(Error::Denied(format!("{} may not delete memory {id}", p.id)));
        }
        self.delete_item(&item)?;
        let prev =
            item.supersedes.as_deref().and_then(|x| self.get(x)).filter(|x| x.superseded_by.as_deref() == Some(id));
        let next =
            item.superseded_by.as_deref().and_then(|x| self.get(x)).filter(|x| x.supersedes.as_deref() == Some(id));
        let prev_id = prev.as_ref().map(|x| x.id.clone());
        if let Some(mut prev) = prev {
            if item.valid_to.is_none() {
                prev.valid_to = None;
                prev.superseded_by = None;
                prev.expires_at = None;
            } else {
                prev.valid_to = item.valid_to;
                prev.superseded_by = next.as_ref().map(|n| n.id.clone());
            }
            self.save(prev)?;
        }
        if let Some(mut next) = next {
            next.supersedes = prev_id;
            self.save(next)?;
        }
        Ok(true)
    }

    /// Persist access statistics, expire old items, promote recurring episodes
    /// into long-term summaries, then prune faded episodes and enforce caps.
    ///
    /// Each step plans from a snapshot without the write lock, then takes the
    /// lock for one item (or one summary group) at a time and checks the item
    /// again before writing. Asks and remembers wait for at most one such
    /// write, not for the whole pass.
    pub fn consolidate(&self) -> Result<ConsolidationReport> {
        if self.consolidating.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Ok(ConsolidationReport { skipped: true, ..Default::default() });
        }
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _release = Release(&self.consolidating);
        let mut report = ConsolidationReport { flushed: self.flush_access()?, ..Default::default() };
        let now = self.now();
        let expired: Vec<String> = self.items.read().values().filter(|i| !i.alive(now)).map(|i| i.id.clone()).collect();
        for id in expired {
            let _write = self.write_lock.lock();
            // A repeated question may have renewed the episode since.
            if let Some(item) = self.get(&id).filter(|i| !i.alive(now)) {
                self.delete_item(&item)?;
                report.expired += 1;
            }
        }
        self.summarise(now, &mut report)?;
        self.prune(now, &mut report)?;
        let stats = self.stats();
        report.long_term = stats.long_term;
        report.episodic = stats.episodic;
        Ok(report)
    }

    /// Group current episodes by (workspace, subject, top entities) and write
    /// one summary per group that recurs. Each group is written under the
    /// write lock from its episodes as they are then.
    fn summarise(&self, now: i64, report: &mut ConsolidationReport) -> Result<()> {
        let threshold = self.cfg.consolidate_after.max(2);
        let mut summaries: HashMap<GroupKey, String> = HashMap::new();
        let mut groups: BTreeMap<GroupKey, Vec<String>> = BTreeMap::new();
        for item in self.items.read().values() {
            if item.scope == MemoryScope::LongTerm && item.kind == MemoryKind::Summary {
                let key = (item.workspace.clone(), item.subject.clone(), item.entities.join("|"));
                let det = summary_id(&key);
                // Prefer the deterministic id, then the smallest legacy id.
                match summaries.get(&key) {
                    Some(have) if *have == det || (item.id != det && item.id >= *have) => {}
                    _ => {
                        summaries.insert(key, item.id.clone());
                    }
                }
                continue;
            }
            if let Some(key) = episode_group(item, now) {
                groups.entry(key).or_default().push(item.id.clone());
            }
        }
        for (key, ids) in groups {
            let _write = self.write_lock.lock();
            let mut episodes: Vec<MemoryItem> = ids
                .iter()
                .filter_map(|id| self.get(id))
                .filter(|e| e.alive(now) && episode_group(e, now).as_ref() == Some(&key))
                .collect();
            let existing = summaries.get(&key).and_then(|id| self.get(id));
            let fresh: Vec<String> = episodes.iter().filter(|e| !e.consolidated).map(|e| e.id.clone()).collect();
            if fresh.is_empty() {
                if existing.is_some() {
                    report.unchanged += 1;
                }
                continue;
            }
            if fresh.len() < threshold {
                continue;
            }
            episodes.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
            let entities: Vec<Id> = key.2.split('|').map(str::to_string).collect();
            let recent: Vec<String> = episodes.iter().rev().take(3).map(|e| clip(&e.text, 220)).collect();
            let text = format!(
                "Recurring topic ({} questions) about {}: {}",
                episodes.len(),
                entities.join(", "),
                recent.join(" | ")
            );
            let salience = (0.5 + 0.1 * episodes.len() as f32).min(1.0);
            match existing {
                Some(mut s) => {
                    let salience = s.salience.max(salience);
                    if s.text == text && s.salience == salience {
                        report.unchanged += 1;
                    } else {
                        s.text = text;
                        s.touch(now, self.cfg.activation_k);
                        s.hits = s.hits.saturating_add(fresh.len() as u32 - 1);
                        s.salience = salience;
                        self.save(s)?;
                        report.updated += 1;
                    }
                }
                None => {
                    self.save(MemoryItem {
                        id: summary_id(&key),
                        workspace: key.0.clone(),
                        scope: MemoryScope::LongTerm,
                        kind: MemoryKind::Summary,
                        text,
                        entities,
                        subject: key.1.clone(),
                        conversation: None,
                        salience,
                        hits: fresh.len() as u32,
                        created_at: now,
                        last_access: now,
                        expires_at: None,
                        consolidated: false,
                        key: None,
                        valid_from: None,
                        valid_to: None,
                        superseded_by: None,
                        supersedes: None,
                        recent: Vec::new(),
                    })?;
                    report.promoted += 1;
                }
            }
            for id in fresh {
                if let Some(mut e) = self.get(&id).filter(|e| !e.consolidated) {
                    e.consolidated = true;
                    self.save(e)?;
                }
            }
        }
        Ok(())
    }

    /// Delete consolidated episodes that faded, then the weakest items of any
    /// subject over its cap. Current facts and preferences are never evicted.
    fn prune(&self, now: i64, report: &mut ConsolidationReport) -> Result<()> {
        let cfg = &self.cfg;
        let fade = |i: &MemoryItem| {
            i.scope == MemoryScope::Episodic
                && i.consolidated
                && i.is_current(now)
                && i.hits == 0
                && strength(i, now, cfg) < cfg.prune_below
        };
        let faded: Vec<String> = self.items.read().values().filter(|i| fade(i)).map(|i| i.id.clone()).collect();
        let mut pruned = 0;
        for id in &faded {
            let _write = self.write_lock.lock();
            if let Some(item) = self.get(id).filter(|i| fade(i)) {
                self.delete_item(&item)?;
                pruned += 1;
            }
        }
        report.pruned += pruned;
        inc(&self.counters.pruned, pruned as u64);

        type Ranked = (f32, i64, String);
        let mut per: HashMap<(String, Option<String>, bool), Vec<Ranked>> = HashMap::new();
        for i in self.items.read().values() {
            let episode = i.scope == MemoryScope::Episodic;
            let summary = i.scope == MemoryScope::LongTerm && i.kind == MemoryKind::Summary;
            if (episode || summary) && i.is_current(now) {
                per.entry((i.workspace.clone(), i.subject.clone(), episode)).or_default().push((
                    strength(i, now, cfg),
                    i.last_access,
                    i.id.clone(),
                ));
            }
        }
        let mut evict: Vec<(String, i64)> = Vec::new();
        for ((_, _, episode), mut ranked) in per {
            let cap = if episode { cfg.max_episodes_per_subject } else { cfg.max_summaries_per_subject };
            if cap == 0 || ranked.len() <= cap {
                continue;
            }
            ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));
            let excess = ranked.len() - cap;
            evict.extend(ranked.into_iter().take(excess).map(|r| (r.2, r.1)));
        }
        evict.sort();
        for (id, last_access) in &evict {
            let _write = self.write_lock.lock();
            // An item used since the ranking was taken is kept this pass.
            if let Some(item) = self.get(id).filter(|i| i.is_current(now) && i.last_access == *last_access) {
                self.delete_item(&item)?;
                report.evicted += 1;
            }
        }
        inc(&self.counters.evicted, report.evicted as u64);
        Ok(())
    }

    pub fn stats(&self) -> MemoryStats {
        let (mut episodic, mut long_term, mut superseded) = (0, 0, 0);
        for i in self.items.read().values() {
            match i.scope {
                MemoryScope::Episodic => episodic += 1,
                MemoryScope::LongTerm => long_term += 1,
            }
            if i.valid_to.is_some() {
                superseded += 1;
            }
        }
        let c = &self.counters;
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let (recalls, recall_tokens) = (load(&c.recalls), load(&c.recall_tokens));
        MemoryStats {
            episodic,
            long_term,
            revision: self.revision(),
            superseded,
            merged: load(&c.merged),
            superseded_total: load(&c.superseded),
            pruned: load(&c.pruned),
            evicted: load(&c.evicted),
            reinforced: load(&c.reinforced),
            recalls,
            recall_items: load(&c.recall_items),
            recall_tokens,
            avg_recall_tokens: if recalls == 0 { 0.0 } else { recall_tokens as f32 / recalls as f32 },
        }
    }

    /// Current memories, newest first.
    pub fn list(&self, p: &Principal, ws: &str, scope: Option<MemoryScope>, limit: usize) -> Vec<MemoryItem> {
        self.list_with(p, ws, scope, limit, false)
    }

    /// Memories, newest first; superseded ones only when asked for.
    pub fn list_with(
        &self,
        p: &Principal,
        ws: &str,
        scope: Option<MemoryScope>,
        limit: usize,
        include_superseded: bool,
    ) -> Vec<MemoryItem> {
        let now = self.now();
        let mut v: Vec<MemoryItem> = self
            .items
            .read()
            .values()
            .filter(|i| {
                Self::visible(p, ws, i)
                    && scope.is_none_or(|s| s == i.scope)
                    && (include_superseded || i.valid_to.is_none_or(|t| t > now))
            })
            .cloned()
            .collect();
        v.sort_by_key(|i| std::cmp::Reverse(i.created_at));
        v.truncate(limit);
        v
    }

    #[cfg(test)]
    pub(crate) fn has_entity(&self, e: &str) -> bool {
        self.idx.read().by_entity.contains_key(e)
    }
}

/// The summary group of a current episode that names entities.
fn episode_group(item: &MemoryItem, now: i64) -> Option<GroupKey> {
    if item.scope != MemoryScope::Episodic || item.entities.is_empty() || !item.is_current(now) {
        return None;
    }
    let mut ents = item.entities.clone();
    ents.sort();
    ents.truncate(3);
    Some((item.workspace.clone(), item.subject.clone(), ents.join("|")))
}

/// Deterministic id of the summary for a group, so replicas upsert one row.
fn summary_id(key: &GroupKey) -> String {
    let h = crate::hash::stable_hash(&[&key.0, key.1.as_deref().unwrap_or(""), &key.2]);
    format!("mem_sum_{}", &h[..20])
}

/// Tokens a recalled memory costs in the prompt (mirrors context.rs).
pub(crate) fn recall_cost(item: &MemoryItem) -> usize {
    crate::token::estimate(&clip(&item.text, PACKED_CHARS)) + PACKED_OVERHEAD
}

// ---------------------------------------------------------------------------
// Activation (ACT-R base level, Petrov's hybrid approximation)

/// Base-level activation `B ≈ ln(Σ t_i^-d)` from the use count, the lifetime
/// and the `activation_k` most recent uses (times in hours, at least 1 h).
/// Creation counts as a use.
pub(crate) fn base_level(item: &MemoryItem, now: i64, cfg: &MemoryConfig) -> f64 {
    let d = f64::from(cfg.activation_decay).clamp(0.01, 0.99);
    let hours = |t: i64| ((now - t) as f64 / HOUR_MS).max(1.0);
    let n = u64::from(item.hits) + 1;
    let t_n = hours(item.created_at);
    let lags: Vec<f64> = if item.recent.is_empty() {
        vec![hours(item.last_access)]
    } else {
        item.recent.iter().take(cfg.activation_k.max(1)).map(|&t| hours(t)).collect()
    };
    let k_eff = lags.len().min(n as usize);
    let mut sum: f64 = lags[..k_eff].iter().map(|l| l.powf(-d)).sum();
    if n > k_eff as u64 {
        let rest = (n - k_eff as u64) as f64;
        let t_k = lags[k_eff - 1];
        if t_n > t_k + 1e-6 {
            sum += rest * (t_n.powf(1.0 - d) - t_k.powf(1.0 - d)) / ((1.0 - d) * (t_n - t_k));
        } else {
            sum += rest * t_n.powf(-d);
        }
    }
    sum.max(f64::MIN_POSITIVE).ln()
}

/// Activation mapped to 0..1: about 0.92 one hour after a single use, 0.48
/// after a week, 0.31 after 30 days. Current long-term facts and preferences
/// never drop below `fact_floor`.
pub(crate) fn strength(item: &MemoryItem, now: i64, cfg: &MemoryConfig) -> f32 {
    let b = base_level(item, now, cfg);
    let s = f64::from(cfg.activation_s).max(1e-3);
    let mut v = (1.0 / (1.0 + (-(b - f64::from(cfg.activation_tau)) / s).exp())) as f32;
    if item.is_fact_like() && item.valid_to.is_none() {
        v = v.max(cfg.fact_floor);
    }
    v
}

// ---------------------------------------------------------------------------
// Write-time decisions (deterministic; no LLM)

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Merge(String),
    Supersede(String),
    Add,
}

/// Content terms (BM25 tokenizer: lowercased, stopwords dropped, plural `s` stripped).
pub(crate) fn content_tokens(s: &str) -> BTreeSet<String> {
    tokenize(s).into_iter().collect()
}

pub(crate) fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f32 {
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 { 0.0 } else { inter as f32 / union as f32 }
}

/// `|A∩B| / min(|A|,|B|)`: 1.0 when one text's terms are all in the other.
pub(crate) fn overlap_coef(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f32 {
    let min = a.len().min(b.len());
    if min == 0 { 0.0 } else { a.intersection(b).count() as f32 / min as f32 }
}

/// Raw lowercase tokens that contain a digit, sorted ("10k", "2025", "v2").
pub(crate) fn numbers(s: &str) -> Vec<String> {
    let mut v: Vec<String> = s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().any(|c| c.is_ascii_digit()))
        .map(str::to_lowercase)
        .collect();
    v.sort();
    v
}

const NEGATIONS: &[&str] = &[
    "not", "no", "never", "none", "without", "cannot", "can't", "cant", "don't", "dont", "doesn't", "doesnt", "isn't",
    "isnt", "won't", "wont", "aren't", "arent",
];

/// The text contains a negation. Computed on raw words: the tokenizer drops
/// "not" and "no" as stopwords.
pub(crate) fn negated(s: &str) -> bool {
    s.to_lowercase()
        .replace('\u{2019}', "'")
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .any(|w| NEGATIONS.contains(&w))
}

fn same_text(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    norm(a) == norm(b)
}

/// Mem0's ADD / UPDATE / DELETE / NOOP, decided by rules instead of an LLM:
/// merge a repeat, supersede a small change to a fact about the same
/// entities, otherwise add.
///
/// A repeat is a write about the same entities (or about none) whose content
/// terms all appear in the candidate's, or the other way round, with the same
/// numbers and negation. Similar text is not enough: a long sentence that
/// changes one name scores high on trigram similarity but states a new fact.
/// Merging never changes the candidate's entities, so a later change to the
/// fact still finds it with the exact entity match that superseding needs.
fn decide(
    cfg: &MemoryConfig,
    text: &str,
    entities: &[Id],
    candidates: &[MemoryItem],
    allow_supersede: bool,
) -> Decision {
    let new_toks = content_tokens(text);
    let (new_nums, new_neg) = (numbers(text), negated(text));
    let mut ranked: Vec<(&MemoryItem, BTreeSet<String>, f32)> = candidates
        .iter()
        .map(|c| {
            let toks = content_tokens(&c.text);
            let sim = jaccard(&new_toks, &toks).max(similarity(&text.to_lowercase(), &c.text.to_lowercase()));
            (c, toks, sim)
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.2.total_cmp(&a.2).then_with(|| b.0.created_at.cmp(&a.0.created_at)).then_with(|| a.0.id.cmp(&b.0.id))
    });
    let new_ents: BTreeSet<&Id> = entities.iter().collect();
    for (c, toks, sim) in &ranked {
        if !new_ents.is_empty() && c.entities.iter().collect::<BTreeSet<_>>() != new_ents {
            continue;
        }
        let contained = new_toks.is_subset(toks) || toks.is_subset(&new_toks);
        let near =
            *sim >= cfg.merge_threshold && contained && numbers(&c.text) == new_nums && negated(&c.text) == new_neg;
        if same_text(text, &c.text) || near {
            return Decision::Merge(c.id.clone());
        }
    }
    if !allow_supersede || entities.is_empty() {
        return Decision::Add;
    }
    for (c, toks, _) in &ranked {
        if c.entities.is_empty() || c.entities.iter().collect::<BTreeSet<_>>() != new_ents {
            continue;
        }
        if jaccard(&new_toks, toks) < cfg.supersede_threshold {
            continue;
        }
        let (only_new, only_old) = (new_toks.difference(toks).count(), toks.difference(&new_toks).count());
        let small_edit = only_new + only_old > 0 && only_new <= 4 && only_old <= 4;
        if numbers(&c.text) != new_nums || negated(&c.text) != new_neg || small_edit {
            return Decision::Supersede(c.id.clone());
        }
    }
    Decision::Add
}

// ---------------------------------------------------------------------------
// Recall selection (MMR with hard dedup and greedy budget packing)

/// Pick up to `k` hits from `pool` (sorted best first): maximal marginal
/// relevance `λ·score − (1−λ)·redundancy`, dropping candidates whose term
/// overlap with a selected hit reaches `dup`, and skipping any that no longer
/// fit the token budget. With a budget too small for anything, the best hit is
/// returned alone.
fn select(pool: Vec<MemoryHit>, k: usize, budget: Option<usize>, lambda: f32, dup: f32) -> Vec<MemoryHit> {
    let toks: Vec<BTreeSet<String>> = pool.iter().map(|h| content_tokens(&h.item.text)).collect();
    let costs: Vec<usize> = pool.iter().map(|h| recall_cost(&h.item)).collect();
    let mut red = vec![0f32; pool.len()];
    let mut open: Vec<usize> = (0..pool.len()).collect();
    let mut chosen: Vec<usize> = Vec::new();
    let mut left = budget;
    while chosen.len() < k && !open.is_empty() {
        open.retain(|&c| red[c] < dup && left.is_none_or(|b| costs[c] <= b));
        let mmr = |c: usize| lambda * pool[c].score - (1.0 - lambda) * red[c];
        let best = open.iter().copied().max_by(|&a, &b| {
            mmr(a)
                .total_cmp(&mmr(b))
                .then_with(|| pool[a].score.total_cmp(&pool[b].score))
                .then_with(|| pool[a].item.created_at.cmp(&pool[b].item.created_at))
                .then_with(|| pool[b].item.id.cmp(&pool[a].item.id))
        });
        let Some(best) = best else { break };
        chosen.push(best);
        open.retain(|&c| c != best);
        if let Some(b) = left.as_mut() {
            *b -= costs[best];
        }
        for &c in &open {
            red[c] = red[c].max(overlap_coef(&toks[c], &toks[best]));
        }
    }
    if chosen.is_empty() && !pool.is_empty() {
        chosen.push(0);
    }
    let mut slots: Vec<Option<MemoryHit>> = pool.into_iter().map(Some).collect();
    chosen.into_iter().filter_map(|i| slots[i].take()).collect()
}

pub(crate) fn clip(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Convenience for actions: memory item as JSON.
pub fn to_json(item: &MemoryItem) -> Value {
    serde_json::to_value(item).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicI64;

    use super::*;
    use onto_core::store::MemoryStore;

    /// Far in the future, so the in-memory store's own (wall-clock) expiry never fires.
    const T0: i64 = 4_000_000_000_000;
    const HOUR: i64 = 3_600_000;

    fn mem() -> Memory {
        Memory::open(Arc::new(MemoryStore::default()), MemoryConfig { consolidate_after: 2, ..Default::default() })
            .unwrap()
    }

    fn clocked_on(store: Arc<dyn Store>, cfg: MemoryConfig) -> (Memory, Arc<AtomicI64>) {
        let t = Arc::new(AtomicI64::new(T0));
        let clock: Clock = {
            let t = t.clone();
            Arc::new(move || t.load(Ordering::SeqCst))
        };
        (Memory::open_with_clock(store, cfg, clock).unwrap(), t)
    }

    fn clocked(cfg: MemoryConfig) -> (Memory, Arc<AtomicI64>) {
        clocked_on(Arc::new(MemoryStore::default()), cfg)
    }

    fn advance(t: &AtomicI64, ms: i64) {
        t.fetch_add(ms, Ordering::SeqCst);
    }

    fn fact(text: &str, entities: &[&str]) -> RememberInput {
        RememberInput {
            text: text.into(),
            entities: entities.iter().map(|e| e.to_string()).collect(),
            ..Default::default()
        }
    }

    fn texts(hits: &[MemoryHit]) -> Vec<&str> {
        hits.iter().map(|h| h.item.text.as_str()).collect()
    }

    fn alice() -> Principal {
        Principal::member("alice", "w")
    }

    fn bare(created_at: i64, hits: u32, recent: Vec<i64>) -> MemoryItem {
        MemoryItem {
            id: "x".into(),
            workspace: "w".into(),
            scope: MemoryScope::Episodic,
            kind: MemoryKind::Episode,
            text: "x".into(),
            entities: Vec::new(),
            subject: None,
            conversation: None,
            salience: 0.5,
            hits,
            created_at,
            last_access: recent.first().copied().unwrap_or(created_at),
            expires_at: None,
            consolidated: false,
            key: None,
            valid_from: None,
            valid_to: None,
            superseded_by: None,
            supersedes: None,
            recent,
        }
    }

    #[test]
    fn recall_prefers_relevant_and_respects_privacy() {
        let m = mem();
        let alice = Principal::member("alice", "w");
        let bob = Principal::member("bob", "w");
        m.remember(
            &alice,
            "w",
            RememberInput {
                text: "Kenji is on call for payments this week".into(),
                entities: vec!["person:kenji".into()],
                ..Default::default()
            },
        )
        .unwrap();
        m.remember(
            &alice,
            "w",
            RememberInput {
                text: "Alice prefers terse answers".into(),
                private: true,
                kind: Some(MemoryKind::Preference),
                ..Default::default()
            },
        )
        .unwrap();
        let hits = m.recall(&bob, "w", "who is on call for payments", &[], 5);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].item.text.contains("on call"));
        assert!(m.recall(&bob, "w", "terse answers", &[], 5).is_empty());
        assert_eq!(m.recall(&alice, "w", "terse answers", &[], 5).len(), 1);
        let by_entity = m.recall(&bob, "w", "zzz", &["person:kenji".into()], 5);
        assert_eq!(by_entity.len(), 1);
    }

    #[test]
    fn consolidation_promotes_recurring_topics() {
        let m = mem();
        let p = Principal::member("alice", "w");
        for q in ["who owns alpha", "is alpha blocked", "alpha status"] {
            m.record_episode(&p, "w", None, q, "answer", &["project:alpha".into()], 0.5).unwrap();
        }
        let r = m.consolidate().unwrap();
        assert_eq!(r.promoted, 1);
        assert_eq!(m.stats().long_term, 1);
        let again = m.consolidate().unwrap();
        assert_eq!(again.promoted, 0, "episodes are only consolidated once");
    }

    // --- M1: clock and metrics -------------------------------------------

    #[test]
    fn injected_clock_drives_episode_times_and_expiry() {
        let (m, t) = clocked(MemoryConfig::default());
        let e = m
            .record_episode(&alice(), "w", None, "who owns alpha", "payments", &["project:alpha".into()], 0.5)
            .unwrap();
        assert_eq!(e.created_at, T0);
        assert_eq!(e.expires_at, Some(T0 + 30 * DAY_MS));
        advance(&t, 31 * DAY_MS);
        let r = m.consolidate().unwrap();
        assert_eq!(r.expired, 1);
        assert!(m.get(&e.id).is_none());
    }

    #[test]
    fn recall_counts_calls_items_and_tokens() {
        let (m, _t) = clocked(MemoryConfig::default());
        m.remember(&alice(), "w", fact("Kenji is on call for payments", &[])).unwrap();
        assert_eq!(m.stats().recalls, 0);
        let hits = m.recall(&alice(), "w", "on call payments", &[], 4);
        let s = m.stats();
        assert_eq!((s.recalls, s.recall_items), (1, 1));
        assert_eq!(s.recall_tokens, recall_cost(&hits[0].item) as u64);
        assert!(s.recall_tokens > 0 && s.avg_recall_tokens > 0.0);
        assert_eq!(s.reinforced, 1);
    }

    // --- M2: write-time consolidation --------------------------------------

    #[test]
    fn write_helpers() {
        assert_eq!(numbers("Alpha budget is 10K, due 2025-03"), vec!["03", "10k", "2025"]);
        assert!(numbers("no digits here").is_empty());
        assert!(negated("Deploy freeze is not on Friday"));
        assert!(negated("We can\u{2019}t deploy") && negated("it doesn't page"));
        assert!(!negated("Notes on Friday") && !negated("Nothing is known"));
        let a = content_tokens("Kenji is on call for payments");
        let b = content_tokens("Priya is on call for payments");
        assert_eq!(a, ["call", "kenji", "payment"].iter().map(|s| s.to_string()).collect());
        assert!((jaccard(&a, &b) - 0.5).abs() < 1e-6);
        let c = content_tokens("Kenji is on call for payments this week");
        assert!((overlap_coef(&a, &c) - 1.0).abs() < 1e-6);
        assert_eq!(overlap_coef(&a, &BTreeSet::new()), 0.0);
    }

    #[test]
    fn remembering_the_same_fact_twice_merges() {
        let (m, t) = clocked(MemoryConfig::default());
        let first = m.remember(&alice(), "w", fact("Kenji is on call for payments", &["team:payments"])).unwrap();
        let rev = m.revision_for("w");
        advance(&t, HOUR);
        let second = m.remember(&alice(), "w", fact("kenji is on call for  payments.", &["team:payments"])).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(m.list(&alice(), "w", None, 10).len(), 1);
        let item = m.get(&first.id).unwrap();
        assert_eq!(item.hits, 1);
        assert_eq!(item.recent, vec![T0 + HOUR, T0]);
        assert_eq!(m.stats().merged, 1);
        assert_eq!(m.revision_for("w"), rev, "a repeat changes nothing visible");
        // More information about the same thing replaces the text.
        let richer = fact("Kenji is on call for payments; page Kenji first for payments incidents", &["team:payments"]);
        let (m2, _) = clocked(MemoryConfig { merge_threshold: 0.5, ..Default::default() });
        let a = m2.remember(&alice(), "w", fact("Kenji is on call for payments", &["team:payments"])).unwrap();
        let b = m2.remember(&alice(), "w", richer).unwrap();
        assert_eq!(a.id, b.id);
        assert!(b.text.contains("incidents"));
    }

    #[test]
    fn a_changed_fact_supersedes_the_old_one() {
        let (m, t) = clocked(MemoryConfig::default());
        let old = m.remember(&alice(), "w", fact("Kenji is on call for payments", &["team:payments"])).unwrap();
        advance(&t, HOUR);
        let new = m.remember(&alice(), "w", fact("Priya is on call for payments", &["team:payments"])).unwrap();
        assert_ne!(old.id, new.id);
        assert_eq!(
            texts(&m.recall(&alice(), "w", "who is on call for payments", &[], 5)),
            vec!["Priya is on call for payments"]
        );
        let old = m.get(&old.id).unwrap();
        assert_eq!(old.valid_to, Some(T0 + HOUR));
        assert_eq!(old.superseded_by.as_deref(), Some(new.id.as_str()));
        assert_eq!(old.expires_at, Some(T0 + HOUR + 180 * DAY_MS));
        assert_eq!(new.supersedes.as_deref(), Some(old.id.as_str()));
        assert_eq!(new.valid_from, Some(T0 + HOUR));
        let s = m.stats();
        assert_eq!((s.superseded, s.superseded_total, s.long_term), (1, 1, 2));
    }

    #[test]
    fn negation_and_numbers_supersede_instead_of_merging() {
        let (m, _t) = clocked(MemoryConfig::default());
        let p = alice();
        let a = m.remember(&p, "w", fact("Deploy freeze is on Friday", &["team:platform"])).unwrap();
        let b = m.remember(&p, "w", fact("Deploy freeze is not on Friday", &["team:platform"])).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(b.supersedes.as_deref(), Some(a.id.as_str()));
        let c = m.remember(&p, "w", fact("Alpha budget is 10k", &["project:alpha"])).unwrap();
        let d = m.remember(&p, "w", fact("Alpha budget is 12k", &["project:alpha"])).unwrap();
        assert_eq!(d.supersedes.as_deref(), Some(c.id.as_str()));
        assert_eq!(m.stats().merged, 0);
    }

    #[test]
    fn unrelated_facts_about_one_entity_are_both_kept() {
        let (m, _t) = clocked(MemoryConfig::default());
        m.remember(&alice(), "w", fact("Alpha owner is Kenji", &["project:alpha"])).unwrap();
        let b = m.remember(&alice(), "w", fact("Alpha deploys from main", &["project:alpha"])).unwrap();
        assert!(b.supersedes.is_none());
        assert_eq!(m.list(&alice(), "w", None, 10).len(), 2);
    }

    #[test]
    fn private_and_shared_facts_never_merge() {
        let (m, _t) = clocked(MemoryConfig::default());
        let shared = m.remember(&alice(), "w", fact("Alice prefers terse answers", &[])).unwrap();
        let private = m
            .remember(&alice(), "w", RememberInput { private: true, ..fact("Alice prefers terse answers", &[]) })
            .unwrap();
        assert_ne!(shared.id, private.id);
        assert_eq!(private.subject.as_deref(), Some("alice"));
    }

    #[test]
    fn supersede_false_forces_add() {
        let (m, _t) = clocked(MemoryConfig::default());
        m.remember(&alice(), "w", fact("Kenji is on call for payments", &["team:payments"])).unwrap();
        let b = m
            .remember(
                &alice(),
                "w",
                RememberInput { supersede: Some(false), ..fact("Priya is on call for payments", &["team:payments"]) },
            )
            .unwrap();
        assert!(b.supersedes.is_none());
        assert_eq!(m.recall(&alice(), "w", "on call payments", &[], 5).len(), 2);
    }

    #[test]
    fn explicit_supersede_checks_its_target() {
        let (m, t) = clocked(MemoryConfig::default());
        let admin = Principal::admin("root");
        let a = m.remember(&alice(), "w", fact("Release train leaves Monday", &[])).unwrap();
        let other = m.remember(&admin, "x", fact("Release train leaves Monday", &[])).unwrap();
        let bad = RememberInput { supersedes: Some("mem_missing".into()), ..fact("Release train leaves Tuesday", &[]) };
        assert!(m.remember(&alice(), "w", bad).is_err());
        let cross = RememberInput { supersedes: Some(other.id.clone()), ..fact("Release train leaves Tuesday", &[]) };
        assert!(m.remember(&admin, "w", cross).is_err(), "target in another workspace");
        let bob_private = m
            .remember(&Principal::member("bob", "w"), "w", RememberInput { private: true, ..fact("Bob's note", &[]) })
            .unwrap();
        let foreign = RememberInput { supersedes: Some(bob_private.id), ..fact("Alice's note", &[]) };
        assert!(m.remember(&alice(), "w", foreign).is_err(), "not visible to alice");
        advance(&t, HOUR);
        let ok = RememberInput { supersedes: Some(a.id.clone()), ..fact("Release train leaves Tuesday", &[]) };
        let b = m.remember(&alice(), "w", ok).unwrap();
        assert_eq!(b.supersedes.as_deref(), Some(a.id.as_str()));
        assert_eq!(m.get(&a.id).unwrap().valid_to, Some(T0 + HOUR));
    }

    #[test]
    fn a_repeated_question_updates_one_episode() {
        let (m, t) = clocked(MemoryConfig::default());
        let p = alice();
        let mut ids = Vec::new();
        for (i, q) in ["Who owns Alpha?", "who owns alpha", "Who  owns Alpha"].iter().enumerate() {
            let e = m.record_episode(&p, "w", Some("c1"), q, &format!("answer {i}"), &["project:alpha".into()], 0.5);
            ids.push(e.unwrap().id);
            advance(&t, HOUR);
        }
        assert!(ids.iter().all(|id| id == &ids[0]));
        let s = m.stats();
        assert_eq!((s.episodic, s.merged), (1, 2));
        let e = m.get(&ids[0]).unwrap();
        assert_eq!(e.hits, 2);
        assert!(e.text.ends_with("answer 2"));
        assert_eq!(e.expires_at, Some(T0 + 2 * HOUR + 30 * DAY_MS));
        // The ask path's own-question filter still recognises it.
        let key = crate::hash::normalize_question("Who owns Alpha?");
        assert_eq!(e.key.as_deref(), Some(key.as_str()));
        let opts = RecallOptions { exclude_key: Some(key), ..Default::default() };
        assert!(m.recall_with(&p, "w", "who owns alpha", &[], 4, &opts).is_empty());
        // Another asker gets their own episode.
        m.record_episode(&Principal::member("bob", "w"), "w", None, "who owns alpha", "x", &[], 0.5).unwrap();
        assert_eq!(m.stats().episodic, 2);
    }

    #[test]
    fn legacy_items_still_load() {
        let legacy = serde_json::json!({
            "id": "mem_old", "workspace": "w", "scope": "long_term", "kind": "fact",
            "text": "Payments freezes deploys on Fridays", "entities": ["team:payments"],
            "subject": null, "salience": 0.7, "hits": 3, "created_at": T0 - DAY_MS, "last_access": T0 - DAY_MS
        });
        let item: MemoryItem = serde_json::from_value(legacy.clone()).unwrap();
        assert!(item.is_current(T0) && item.recent.is_empty() && item.valid_from.is_none());
        let store = Arc::new(MemoryStore::default());
        store.kv_put(NS, "mem_old", &legacy, None).unwrap();
        let (m, _t) = clocked_on(store, MemoryConfig::default());
        let hits = m.recall(&alice(), "w", "deploy freeze friday", &[], 4);
        assert_eq!(texts(&hits), vec!["Payments freezes deploys on Fridays"]);
        // Written back without the new fields it does not use.
        let out = to_json(&m.get("mem_old").unwrap());
        assert!(out.get("valid_to").is_none() && out.get("supersedes").is_none());
    }

    // --- M3: bi-temporal validity -------------------------------------------

    fn supersede_pair(m: &Memory, t: &AtomicI64) -> (MemoryItem, MemoryItem) {
        let a = m.remember(&alice(), "w", fact("Kenji is on call for payments", &["team:payments"])).unwrap();
        advance(t, HOUR);
        let b = m.remember(&alice(), "w", fact("Priya is on call for payments", &["team:payments"])).unwrap();
        (a, b)
    }

    #[test]
    fn as_of_recall_and_history() {
        let (m, t) = clocked(MemoryConfig::default());
        let (a, b) = supersede_pair(&m, &t);
        let q = "who is on call for payments";
        assert_eq!(texts(&m.recall(&alice(), "w", q, &[], 5)), vec![b.text.as_str()]);
        let past = RecallOptions { as_of: Some(T0 + 1), ..Default::default() };
        assert_eq!(texts(&m.recall_with(&alice(), "w", q, &[], 5, &past)), vec![a.text.as_str()]);
        let all = RecallOptions { include_superseded: true, ..Default::default() };
        assert_eq!(m.recall_with(&alice(), "w", q, &[], 5, &all).len(), 2);
        let listed = m.list(&alice(), "w", None, 10);
        assert_eq!(listed.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec![b.id.as_str()]);
        let history = m.list_with(&alice(), "w", None, 10, true);
        assert_eq!(history.len(), 2);
        assert_eq!(history.iter().find(|i| i.id == a.id).unwrap().valid_to, Some(T0 + HOUR));
    }

    #[test]
    fn as_of_recall_does_not_reinforce() {
        let (m, t) = clocked(MemoryConfig::default());
        let (a, _b) = supersede_pair(&m, &t);
        let past = RecallOptions { as_of: Some(T0 + 1), ..Default::default() };
        assert_eq!(m.recall_with(&alice(), "w", "on call payments", &[], 5, &past).len(), 1);
        assert_eq!(m.get(&a.id).unwrap().hits, 0);
        assert_eq!(m.flush_access().unwrap(), 0);
    }

    #[test]
    fn forgetting_the_replacement_reopens_the_old_fact() {
        let (m, t) = clocked(MemoryConfig::default());
        let (a, b) = supersede_pair(&m, &t);
        assert!(m.forget(&alice(), &b.id).unwrap());
        let a = m.get(&a.id).unwrap();
        assert!(a.valid_to.is_none() && a.superseded_by.is_none() && a.expires_at.is_none());
        assert_eq!(texts(&m.recall(&alice(), "w", "who is on call for payments", &[], 5)), vec![a.text.as_str()]);
    }

    #[test]
    fn superseded_items_expire_after_their_ttl() {
        let (m, t) = clocked(MemoryConfig::default());
        let (a, b) = supersede_pair(&m, &t);
        advance(&t, 179 * DAY_MS);
        assert_eq!(m.consolidate().unwrap().expired, 0);
        advance(&t, 2 * DAY_MS);
        let r = m.consolidate().unwrap();
        assert_eq!(r.expired, 1);
        assert!(m.get(&a.id).is_none() && m.get(&b.id).is_some());
    }

    #[test]
    fn a_past_valid_from_closes_the_old_fact_at_that_time() {
        let (m, t) = clocked(MemoryConfig::default());
        let a = m.remember(&alice(), "w", fact("Alpha budget is 10k", &["project:alpha"])).unwrap();
        advance(&t, 10 * HOUR);
        let b = m
            .remember(
                &alice(),
                "w",
                RememberInput { valid_from: Some(T0 + 4 * HOUR), ..fact("Alpha budget is 12k", &["project:alpha"]) },
            )
            .unwrap();
        assert_eq!(m.get(&a.id).unwrap().valid_to, Some(T0 + 4 * HOUR));
        let at = |ts| {
            let opts = RecallOptions { as_of: Some(ts), ..Default::default() };
            m.recall_with(&alice(), "w", "alpha budget", &[], 5, &opts)
                .into_iter()
                .map(|h| h.item.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(at(T0 + 2 * HOUR), vec![a.id.clone()]);
        assert_eq!(at(T0 + 6 * HOUR), vec![b.id.clone()]);
    }

    // --- M4: activation, reinforcement, pruning, caps ------------------------

    #[test]
    fn strength_matches_reference_values() {
        let cfg = MemoryConfig::default();
        let one = bare(T0, 0, Vec::new());
        for (lag, want) in [(HOUR, 0.92), (7 * DAY_MS, 0.48), (30 * DAY_MS, 0.31)] {
            let s = strength(&one, T0 + lag, &cfg);
            assert!((s - want).abs() <= 0.02, "lag {lag}: {s} vs {want}");
        }
        // About 14 days unused crosses prune_below.
        assert!(strength(&one, T0 + 13 * DAY_MS, &cfg) > cfg.prune_below);
        assert!(strength(&one, T0 + 15 * DAY_MS, &cfg) < cfg.prune_below);
    }

    #[test]
    fn petrov_approximation_tracks_exact_activation() {
        let cfg = MemoryConfig::default();
        // Five uses evenly spaced over 10 days (creation is the first).
        let uses: Vec<i64> = (0..5).map(|i| T0 + i * 60 * HOUR).collect();
        let now = T0 + 11 * 24 * HOUR;
        let item = bare(T0, 4, vec![uses[4], uses[3]]);
        let exact: f64 = uses.iter().map(|&u| ((now - u) as f64 / HOUR_MS).max(1.0).powf(-0.5)).sum();
        let approx = base_level(&item, now, &cfg).exp();
        assert!((approx / exact - 1.0).abs() < 0.05, "approx {approx} vs exact {exact}");
        // With k covering every use the estimate is exact.
        let all = MemoryConfig { activation_k: 5, ..cfg.clone() };
        let full = bare(T0, 4, uses.iter().rev().copied().collect());
        assert!((base_level(&full, now, &all).exp() / exact - 1.0).abs() < 1e-9);
    }

    #[test]
    fn strength_is_monotone_and_floored() {
        let cfg = MemoryConfig::default();
        let now = T0 + 20 * DAY_MS;
        let stale = bare(T0, 1, vec![T0 + DAY_MS, T0]);
        let fresh = bare(T0, 1, vec![T0 + 19 * DAY_MS, T0]);
        assert!(strength(&fresh, now, &cfg) > strength(&stale, now, &cfg));
        let used = bare(T0, 6, vec![T0 + DAY_MS, T0]);
        assert!(strength(&used, now, &cfg) > strength(&stale, now, &cfg));
        let mut old_fact = bare(T0, 0, Vec::new());
        (old_fact.scope, old_fact.kind) = (MemoryScope::LongTerm, MemoryKind::Fact);
        assert!((strength(&old_fact, T0 + 365 * DAY_MS, &cfg) - cfg.fact_floor).abs() < 1e-6);
        assert!(strength(&bare(T0, 0, Vec::new()), T0 + 365 * DAY_MS, &cfg) < cfg.fact_floor);
    }

    #[test]
    fn recall_reinforces_and_persists_lazily() {
        let store = Arc::new(MemoryStore::default());
        let (m, t) = clocked_on(store.clone(), MemoryConfig::default());
        let f = m.remember(&alice(), "w", fact("Kenji is on call for payments", &[])).unwrap();
        let (rev, ws_rev) = (m.revision(), m.revision_for("w"));
        advance(&t, HOUR);
        assert_eq!(m.recall(&alice(), "w", "on call payments", &[], 4).len(), 1);
        let item = m.get(&f.id).unwrap();
        assert_eq!((item.hits, item.recent[0]), (1, T0 + HOUR));
        assert_eq!((m.revision(), m.revision_for("w")), (rev, ws_rev), "reinforcement is not a visible change");
        let stored = |s: &MemoryStore| s.kv_get(NS, &f.id).unwrap().unwrap()["hits"].as_u64().unwrap();
        assert_eq!(stored(&store), 0, "not persisted yet");
        let r = m.consolidate().unwrap();
        assert_eq!(r.flushed, 1);
        assert_eq!(stored(&store), 1);
        assert_eq!(m.revision(), rev);
    }

    #[test]
    fn frequently_recalled_memories_rank_first() {
        let (m, t) = clocked(MemoryConfig::default());
        m.remember(&alice(), "w", fact("Alpha deploys from main", &[])).unwrap();
        m.remember(&alice(), "w", fact("Beta deploys from main", &[])).unwrap();
        advance(&t, 2 * DAY_MS);
        for _ in 0..3 {
            assert_eq!(m.recall(&alice(), "w", "beta", &[], 1).len(), 1);
            advance(&t, HOUR);
        }
        let hits = m.recall(&alice(), "w", "deploys from main", &[], 2);
        assert_eq!(texts(&hits), vec!["Beta deploys from main", "Alpha deploys from main"]);
    }

    fn three_alpha_questions(m: &Memory) {
        for q in ["who owns alpha", "is alpha blocked", "alpha status"] {
            m.record_episode(&alice(), "w", None, q, "answer", &["project:alpha".into()], 0.5).unwrap();
        }
    }

    #[test]
    fn faded_consolidated_episodes_are_pruned() {
        let (m, t) = clocked(MemoryConfig::default());
        three_alpha_questions(&m);
        assert_eq!(m.consolidate().unwrap().promoted, 1);
        advance(&t, 10 * DAY_MS);
        let r = m.consolidate().unwrap();
        assert_eq!((r.pruned, r.episodic), (0, 3));
        advance(&t, 5 * DAY_MS);
        let r = m.consolidate().unwrap();
        assert_eq!((r.pruned, r.episodic, r.long_term), (3, 0, 1), "the summary keeps their content");
        assert_eq!(m.stats().pruned, 3);
    }

    #[test]
    fn caps_evict_the_weakest_and_spare_facts() {
        let (m, t) = clocked(MemoryConfig { max_episodes_per_subject: 3, ..Default::default() });
        m.remember(&alice(), "w", fact("Kenji is on call for payments", &[])).unwrap();
        m.remember(&alice(), "w", fact("Alpha deploys from main", &[])).unwrap();
        let mut ids = Vec::new();
        for q in ["q one", "q two", "q three", "q four", "q five"] {
            ids.push(m.record_episode(&alice(), "w", None, q, "a", &[], 0.5).unwrap().id);
            advance(&t, HOUR);
        }
        let r = m.consolidate().unwrap();
        assert_eq!(r.evicted, 2);
        assert!(m.get(&ids[0]).is_none() && m.get(&ids[1]).is_none());
        assert!(ids[2..].iter().all(|id| m.get(id).is_some()));
        let s = m.stats();
        assert_eq!((s.episodic, s.long_term, s.evicted), (3, 2, 2));
    }

    #[test]
    fn reload_keeps_unflushed_reinforcement() {
        let store = Arc::new(MemoryStore::default());
        let (m, t) = clocked_on(store.clone(), MemoryConfig::default());
        let f = m.remember(&alice(), "w", fact("Kenji is on call for payments", &[])).unwrap();
        for _ in 0..3 {
            advance(&t, HOUR);
            m.recall(&alice(), "w", "on call payments", &[], 4);
        }
        // Another replica rewrites the item (new salience) from its stale copy.
        let mut theirs = f.clone();
        theirs.salience = 0.9;
        store.kv_put(NS, &f.id, &serde_json::to_value(&theirs).unwrap(), None).unwrap();
        m.reload(&f.id).unwrap();
        let item = m.get(&f.id).unwrap();
        assert_eq!((item.hits, item.salience), (3, 0.9));
        assert_eq!(item.recent, vec![T0 + 3 * HOUR, T0 + 2 * HOUR]);
        assert_eq!(m.flush_access().unwrap(), 1, "still dirty, so it is flushed");
    }

    // --- M5: budgeted, redundancy-aware recall -------------------------------

    #[test]
    fn near_duplicates_are_not_recalled_together() {
        let (m, _t) = clocked(MemoryConfig::default());
        for text in [
            "Kenji is on call for payments",
            "Kenji is on call for payments this week",
            "Kenji is on call for payments today",
            "Checkout deploys from main",
        ] {
            m.remember(&alice(), "w", fact(text, &[])).unwrap();
        }
        assert_eq!(m.stats().long_term, 4, "different enough not to merge at write time");
        let hits = m.recall(&alice(), "w", "kenji on call payments checkout deploys", &[], 2);
        let got = texts(&hits);
        assert_eq!(got.len(), 2);
        assert_eq!(got.iter().filter(|t| t.starts_with("Kenji")).count(), 1, "{got:?}");
        assert!(got.contains(&"Checkout deploys from main"));
    }

    #[test]
    fn a_summary_and_its_episodes_are_not_recalled_together() {
        let (m, _t) = clocked(MemoryConfig::default());
        three_alpha_questions(&m);
        m.consolidate().unwrap();
        let hits = m.recall(&alice(), "w", "alpha owns blocked status", &["project:alpha".into()], 4);
        let summary = hits.iter().any(|h| h.item.kind == MemoryKind::Summary);
        let episodes = hits.iter().filter(|h| h.item.kind == MemoryKind::Episode).count();
        assert!(!hits.is_empty());
        assert!(!(summary && episodes > 0), "summary and the episodes it contains: {:?}", texts(&hits));
    }

    #[test]
    fn the_token_budget_bounds_recall() {
        let (m, _t) = clocked(MemoryConfig::default());
        let long =
            m.remember(&alice(), "w", fact("Payments uses idempotency keys for every retried call", &[])).unwrap();
        let short = m.remember(&alice(), "w", fact("Payments retry thrice", &[])).unwrap();
        let (cl, cs) = (recall_cost(&long), recall_cost(&short));
        assert!(cl + cs > 30 && cl <= 30 && cs <= 30, "{cl} {cs}");
        let q = "payments retried retry keys";
        let with =
            |b| m.recall_with(&alice(), "w", q, &[], 4, &RecallOptions { budget_tokens: b, ..Default::default() });
        assert_eq!(with(None).len(), 2);
        assert_eq!(with(Some(30)).len(), 1);
        let top = with(None)[0].item.id.clone();
        let tiny = with(Some(5));
        assert_eq!(tiny.len(), 1, "the best item is returned even when nothing fits");
        assert_eq!(tiny[0].item.id, top);
    }

    #[test]
    fn a_weak_lexical_match_ranks_below_an_entity_match() {
        let (m, _t) = clocked(MemoryConfig::default());
        let cfg = MemoryConfig::default();
        m.remember(&alice(), "w", fact("Payments retries use idempotency keys", &[])).unwrap();
        m.remember(&alice(), "w", fact("Kenji leads the checkout squad", &["person:kenji"])).unwrap();
        let hits = m.recall(&alice(), "w", "payments outage", &["person:kenji".into()], 4);
        assert_eq!(texts(&hits), vec!["Kenji leads the checkout squad", "Payments retries use idempotency keys"]);
        let weak = &hits[1];
        let lex = (weak.score - 0.1 * strength(&weak.item, T0, &cfg) - 0.05 * weak.item.salience - 0.1) / 0.5;
        assert!(lex > 0.0 && lex < 0.5, "saturated lexical score {lex}");
    }

    // --- M6: consolidation and revisions -------------------------------------

    #[test]
    fn replicas_write_one_summary_per_group() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::default());
        let (a, _) = clocked_on(store.clone(), MemoryConfig::default());
        let (b, _) = clocked_on(store.clone(), MemoryConfig::default());
        three_alpha_questions(&a);
        for q in ["alpha owner", "alpha blockers", "alpha eta"] {
            b.record_episode(&alice(), "w", None, q, "answer", &["project:alpha".into()], 0.5).unwrap();
        }
        assert_eq!(a.consolidate().unwrap().promoted, 1);
        assert_eq!(b.consolidate().unwrap().promoted, 1);
        assert_eq!(store.kv_scan(NS, "mem_sum_").unwrap().len(), 1);
    }

    #[test]
    fn an_idle_consolidation_changes_nothing() {
        let (m, _t) = clocked(MemoryConfig::default());
        three_alpha_questions(&m);
        m.consolidate().unwrap();
        let (rev, ws_rev) = (m.revision(), m.revision_for("w"));
        let r = m.consolidate().unwrap();
        assert!(r.unchanged >= 1);
        assert_eq!((r.promoted, r.updated), (0, 0));
        assert_eq!((m.revision(), m.revision_for("w")), (rev, ws_rev));
    }

    #[test]
    fn revisions_are_per_workspace_and_ignore_recall() {
        let (m, _t) = clocked(MemoryConfig::default());
        let admin = Principal::admin("root");
        let (a0, b0) = (m.revision_for("a"), m.revision_for("b"));
        m.remember(&admin, "a", fact("Alpha deploys from main", &[])).unwrap();
        assert_ne!(m.revision_for("a"), a0);
        assert_eq!(m.revision_for("b"), b0);
        let a1 = m.revision_for("a");
        assert_eq!(m.recall(&admin, "a", "alpha deploys", &[], 4).len(), 1);
        assert_eq!(m.revision_for("a"), a1);
        m.record_episode(&admin, "a", None, "q", "a", &[], 0.5).unwrap();
        assert_eq!(m.revision_for("a"), a1, "episodes do not change the revision");
    }

    #[test]
    fn forgetting_frees_the_entity_index() {
        let (m, _t) = clocked(MemoryConfig::default());
        let f = m.remember(&alice(), "w", fact("X is retired", &["x:1"])).unwrap();
        assert!(m.has_entity("x:1"));
        m.forget(&alice(), &f.id).unwrap();
        assert!(!m.has_entity("x:1"));
    }

    #[test]
    fn a_legacy_summary_is_updated_in_place() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::default());
        let (m, _t) = clocked_on(store.clone(), MemoryConfig::default());
        let mut legacy = bare(T0, 3, Vec::new());
        legacy.id = "mem_0legacy".into();
        (legacy.scope, legacy.kind, legacy.subject) =
            (MemoryScope::LongTerm, MemoryKind::Summary, Some("alice".into()));
        legacy.entities = vec!["project:alpha".into()];
        legacy.text = "Recurring topic (3 questions) about project:alpha: older".into();
        m.save(legacy).unwrap();
        three_alpha_questions(&m);
        let r = m.consolidate().unwrap();
        assert_eq!((r.promoted, r.updated), (0, 1));
        assert!(store.kv_scan(NS, "mem_sum_").unwrap().is_empty());
        assert!(m.get("mem_0legacy").unwrap().text.contains("alpha status"));
    }

    #[test]
    fn a_concurrent_consolidation_is_skipped() {
        let (m, _t) = clocked(MemoryConfig::default());
        m.consolidating.store(true, Ordering::SeqCst);
        assert!(m.consolidate().unwrap().skipped);
        m.consolidating.store(false, Ordering::SeqCst);
        assert!(!m.consolidate().unwrap().skipped);
    }

    // --- Write decisions, history and concurrency regressions ----------------

    #[test]
    fn a_changed_name_in_a_long_sentence_supersedes_instead_of_merging() {
        let cases = [
            (
                "The checkout service is owned by the platform team and its on-call rotation is managed by Alice",
                "The checkout service is owned by the platform team and its on-call rotation is managed by Bob",
            ),
            (
                "The checkout service on-call rotation for the payments platform is managed by Alice Smith",
                "The checkout service on-call rotation for the payments platform is managed by Alice Jones",
            ),
            (
                "Release manager for the checkout service quarterly release train is Maria",
                "Release manager for the checkout service quarterly release train is Mario",
            ),
        ];
        for (before, after) in cases {
            let (m, t) = clocked(MemoryConfig::default());
            let lower = |s: &str| s.to_lowercase();
            assert!(similarity(&lower(before), &lower(after)) >= 0.85, "the case must pass the similarity threshold");
            let old = m.remember(&alice(), "w", fact(before, &["svc:checkout"])).unwrap();
            advance(&t, HOUR);
            let new = m.remember(&alice(), "w", fact(after, &["svc:checkout"])).unwrap();
            assert_ne!(old.id, new.id, "{after}");
            assert_eq!(new.text, after);
            assert_eq!(new.supersedes.as_deref(), Some(old.id.as_str()));
            let current = m.list(&alice(), "w", None, 10);
            assert_eq!(current.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), vec![after]);
            assert_eq!(m.stats().merged, 0);
        }
    }

    #[test]
    fn identical_text_about_different_entities_is_kept_apart() {
        let (m, t) = clocked(MemoryConfig::default());
        let a = m.remember(&alice(), "w", fact("The service status is deprecated", &["svc:a"])).unwrap();
        let b = m.remember(&alice(), "w", fact("The service status is deprecated", &["svc:b"])).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(m.get(&a.id).unwrap().entities, vec!["svc:a".to_string()]);
        advance(&t, HOUR);
        let c = m.remember(&alice(), "w", fact("The service status is active", &["svc:a"])).unwrap();
        assert_eq!(c.supersedes.as_deref(), Some(a.id.as_str()));
        let current = |e: &str| {
            m.list(&alice(), "w", None, 10)
                .into_iter()
                .filter(|i| i.entities.iter().any(|x| x == e))
                .map(|i| i.text)
                .collect::<Vec<_>>()
        };
        assert_eq!(current("svc:a"), vec!["The service status is active"]);
        assert_eq!(current("svc:b"), vec!["The service status is deprecated"]);
        // An untagged repeat still merges into the tagged fact, leaving its entities alone.
        let again = m.remember(&alice(), "w", fact("The service status is deprecated", &[])).unwrap();
        assert_eq!(again.id, b.id);
        assert_eq!(again.entities, vec!["svc:b".to_string()]);
    }

    #[test]
    fn forgetting_a_middle_link_keeps_one_current_fact() {
        let (m, t) = clocked(MemoryConfig::default());
        let x = m.remember(&alice(), "w", fact("owner is alice", &[])).unwrap();
        advance(&t, HOUR);
        let y = m
            .remember(&alice(), "w", RememberInput { supersedes: Some(x.id.clone()), ..fact("owner is bob", &[]) })
            .unwrap();
        advance(&t, HOUR);
        let z = m
            .remember(&alice(), "w", RememberInput { supersedes: Some(y.id.clone()), ..fact("owner is carol", &[]) })
            .unwrap();
        assert!(m.forget(&alice(), &y.id).unwrap());
        assert_eq!(texts(&m.recall(&alice(), "w", "owner", &[], 5)), vec!["owner is carol"]);
        let x = m.get(&x.id).unwrap();
        assert_eq!(x.valid_to, Some(T0 + 2 * HOUR), "now ends where the deleted link ended");
        assert_eq!(x.superseded_by.as_deref(), Some(z.id.as_str()));
        assert_eq!(m.get(&z.id).unwrap().supersedes.as_deref(), Some(x.id.as_str()));
        // Forgetting the current end of the chain still reopens the one before it.
        assert!(m.forget(&alice(), &z.id).unwrap());
        assert_eq!(texts(&m.recall(&alice(), "w", "owner", &[], 5)), vec!["owner is alice"]);
    }

    #[test]
    fn explicit_supersede_keeps_the_scope() {
        let (m, _t) = clocked(MemoryConfig::default());
        let bob = Principal::member("bob", "w");
        let shared = m.remember(&alice(), "w", fact("payments on-call is kenji", &[])).unwrap();
        let mine =
            RememberInput { private: true, supersedes: Some(shared.id.clone()), ..fact("payments on-call is me", &[]) };
        let err = m.remember(&bob, "w", mine).unwrap_err().to_string();
        assert!(err.contains("shared"), "{err}");
        assert!(m.get(&shared.id).unwrap().valid_to.is_none());
        assert_eq!(texts(&m.recall(&alice(), "w", "payments on-call", &[], 5)), vec!["payments on-call is kenji"]);
        let private =
            m.remember(&alice(), "w", RememberInput { private: true, ..fact("my standup is at 9", &[]) }).unwrap();
        let admin = Principal::admin("root");
        let public = RememberInput { supersedes: Some(private.id.clone()), ..fact("standup is at 10", &[]) };
        assert!(m.remember(&admin, "w", public).is_err(), "a shared write may not close a private fact");
        assert!(m.get(&private.id).unwrap().valid_to.is_none());
        // The same scope still works.
        let ok =
            RememberInput { private: true, supersedes: Some(private.id.clone()), ..fact("my standup is at 10", &[]) };
        assert_eq!(m.remember(&alice(), "w", ok).unwrap().supersedes.as_deref(), Some(private.id.as_str()));
    }

    #[test]
    fn flushing_access_never_undoes_a_newer_store_write() {
        let store = Arc::new(MemoryStore::default());
        let (m, t) = clocked_on(store.clone(), MemoryConfig::default());
        let x = m.remember(&alice(), "w", fact("Kenji is on call for payments", &[])).unwrap();
        let y = m.remember(&alice(), "w", fact("Alpha deploys from main", &[])).unwrap();
        advance(&t, HOUR);
        assert_eq!(m.recall(&alice(), "w", "kenji on call payments", &[], 1).len(), 1);
        assert_eq!(m.recall(&alice(), "w", "alpha deploys main", &[], 1).len(), 1);
        // Another replica closes X and deletes Y; their notices have not arrived.
        let mut closed = x.clone();
        (closed.valid_to, closed.superseded_by) = (Some(T0 + HOUR), Some("mem_other".into()));
        store.kv_put(NS, &x.id, &serde_json::to_value(&closed).unwrap(), None).unwrap();
        store.kv_delete(NS, &y.id).unwrap();
        assert_eq!(m.flush_access().unwrap(), 1);
        let stored: MemoryItem = serde_json::from_value(store.kv_get(NS, &x.id).unwrap().unwrap()).unwrap();
        assert_eq!(stored.valid_to, Some(T0 + HOUR), "the supersede stays");
        assert_eq!(stored.superseded_by.as_deref(), Some("mem_other"));
        assert_eq!((stored.hits, stored.recent[0]), (1, T0 + HOUR), "the reinforcement is kept");
        assert!(store.kv_get(NS, &y.id).unwrap().is_none(), "the delete stays");
    }

    /// A store whose deletes are slow, so a test can act while a pass runs.
    struct SlowDeletes {
        inner: MemoryStore,
        deletes: AtomicU64,
        first: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    }

    impl Store for SlowDeletes {
        fn load(&self) -> Result<onto_core::store::Snapshot> {
            self.inner.load()
        }
        fn write(&self, batch: &onto_core::store::WriteBatch) -> Result<u64> {
            self.inner.write(batch)
        }
        fn events(&self, filter: &onto_core::EventFilter) -> Result<Vec<onto_core::Event>> {
            self.inner.events(filter)
        }
        fn kv_put(&self, ns: &str, key: &str, value: &Value, expires_at: Option<i64>) -> Result<()> {
            self.inner.kv_put(ns, key, value, expires_at)
        }
        fn kv_get(&self, ns: &str, key: &str) -> Result<Option<Value>> {
            self.inner.kv_get(ns, key)
        }
        fn kv_delete(&self, ns: &str, key: &str) -> Result<()> {
            if let Some(tx) = self.first.lock().take() {
                let _ = tx.send(());
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
            self.deletes.fetch_add(1, Ordering::SeqCst);
            self.inner.kv_delete(ns, key)
        }
        fn kv_scan(&self, ns: &str, prefix: &str) -> Result<Vec<(String, Value)>> {
            self.inner.kv_scan(ns, prefix)
        }
        fn stats(&self) -> Result<onto_core::store::StoreStats> {
            self.inner.stats()
        }
        fn versions(&self, id: &str, limit: usize) -> Result<Vec<onto_core::store::VersionRecord>> {
            self.inner.versions(id, limit)
        }
        fn versions_since(&self, since: i64, limit: usize) -> Result<Vec<onto_core::store::VersionRecord>> {
            self.inner.versions_since(since, limit)
        }
    }

    #[test]
    fn a_consolidation_does_not_hold_up_writes_for_the_whole_pass() {
        let (tx, rx) = std::sync::mpsc::channel();
        let store = Arc::new(SlowDeletes {
            inner: MemoryStore::default(),
            deletes: AtomicU64::new(0),
            first: Mutex::new(Some(tx)),
        });
        let (m, t) = clocked_on(store.clone(), MemoryConfig::default());
        for i in 0..10 {
            m.record_episode(&alice(), "w", None, &format!("question {i}"), "a", &[], 0.5).unwrap();
        }
        advance(&t, 31 * DAY_MS);
        let bg = m.clone();
        let pass = std::thread::spawn(move || bg.consolidate().unwrap());
        rx.recv().unwrap();
        m.remember(&alice(), "w", fact("Kenji is on call for payments", &[])).unwrap();
        let done = store.deletes.load(Ordering::SeqCst);
        assert!(done < 10, "the write waited for all {done} deletes of the pass");
        assert_eq!(pass.join().unwrap().expired, 10);
    }

    // --- End to end: a redundant stream, before and after ---------------------

    /// The memory before these changes, kept as a baseline: every write
    /// appends, and recall ranks by max-normalised BM25, entity overlap,
    /// created-at recency and salience, with no dedup and no budget.
    #[derive(Default)]
    struct Legacy {
        items: HashMap<String, MemoryItem>,
        index: TextIndex,
        by_entity: HashMap<Id, HashSet<String>>,
    }

    impl Legacy {
        fn add(&mut self, mut item: MemoryItem) {
            item.id = format!("legacy{}", self.items.len());
            self.index.upsert(&item.id, &format!("{} {}", item.text, item.entities.join(" ")));
            for e in &item.entities {
                self.by_entity.entry(e.clone()).or_default().insert(item.id.clone());
            }
            self.items.insert(item.id.clone(), item);
        }

        fn recall(&self, query: &str, entities: &[Id], k: usize, now: i64) -> Vec<MemoryItem> {
            let lexical = self.index.search(query, 50, |_| true);
            let max_lex = lexical.first().map(|(_, s)| *s).unwrap_or(1.0).max(1e-6);
            let mut cands: HashMap<String, f32> = lexical.into_iter().map(|(id, s)| (id, s / max_lex)).collect();
            for e in entities {
                for id in self.by_entity.get(e).into_iter().flatten() {
                    cands.entry(id.clone()).or_insert(0.0);
                }
            }
            let wanted: HashSet<&str> = entities.iter().map(String::as_str).collect();
            let mut hits: Vec<(f32, &MemoryItem)> = cands
                .into_iter()
                .filter_map(|(id, lex)| {
                    let item = self.items.get(&id)?;
                    let overlap = if wanted.is_empty() || item.entities.is_empty() {
                        0.0
                    } else {
                        item.entities.iter().filter(|e| wanted.contains(e.as_str())).count() as f32
                            / wanted.len() as f32
                    };
                    let recency = match item.scope {
                        MemoryScope::LongTerm => 1.0,
                        MemoryScope::Episodic => (-((now - item.created_at) as f32 / (7.0 * DAY_MS as f32))).exp(),
                    };
                    let bonus = if item.scope == MemoryScope::LongTerm { 0.1 } else { 0.0 };
                    let score = 0.5 * lex + 0.3 * overlap + 0.1 * recency + 0.05 * item.salience + bonus;
                    (lex > 0.0 || overlap > 0.0).then_some((score, item))
                })
                .collect();
            hits.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.created_at.cmp(&a.1.created_at)));
            hits.into_iter().take(k).map(|(_, i)| i.clone()).collect()
        }
    }

    #[derive(Debug, Default)]
    struct Outcome {
        items: usize,
        tokens: usize,
        right_in_k: usize,
        right_first: usize,
        stale: usize,
    }

    #[test]
    fn a_redundant_stream_stores_less_and_recalls_cheaper_without_losing_facts() {
        // (entity, first version, current version, question)
        let topics: &[(&str, &str, &str, &str)] = &[
            (
                "team:payments",
                "Kenji is on call for payments",
                "Priya is on call for payments",
                "who is on call for payments",
            ),
            ("project:alpha", "Alpha budget is 10k", "Alpha budget is 12k", "what is the alpha budget"),
            (
                "policy:retention",
                "Data retention is 90 days",
                "Data retention is 180 days",
                "how long is data retention",
            ),
            ("team:platform", "The deploy freeze starts on Friday", "", "when does the deploy freeze start"),
            ("service:postgres", "The Postgres primary runs in eu-west-1", "", "where does the postgres primary run"),
            ("service:checkout", "Checkout uses Stripe for card payments", "", "which provider does checkout use"),
            ("app:mobile", "Aiko owns the mobile app", "", "who owns the mobile app"),
            ("team:sre", "Incidents are reviewed every Tuesday", "", "when are incidents reviewed"),
            ("service:search", "Search runs on Elasticsearch 8", "", "what does search run on"),
            ("team:billing", "Billing invoices go out on the 1st of the month", "", "when do invoices go out"),
        ];
        let (m, t) = clocked(MemoryConfig::default());
        let mut legacy = Legacy::default();
        let p = alice();
        let mut now = T0;
        let mut tick = || {
            advance(&t, HOUR);
            now += HOUR;
            now
        };
        let legacy_fact = |at: i64, text: &str, ent: &str| {
            let mut i = bare(at, 0, Vec::new());
            (i.scope, i.kind, i.salience) = (MemoryScope::LongTerm, MemoryKind::Fact, 0.7);
            (i.text, i.entities) = (text.to_string(), vec![ent.to_string()]);
            i
        };
        // Each fact arrives three times, with cosmetic differences.
        for round in 0..3 {
            for (ent, first, _, _) in topics {
                let at = tick();
                let text = match round {
                    0 => first.to_string(),
                    1 => format!("{first}."),
                    _ => first.to_lowercase(),
                };
                m.remember(&p, "w", fact(&text, &[ent])).unwrap();
                legacy.add(legacy_fact(at, &text, ent));
            }
        }
        // Each question is asked three times.
        for _ in 0..3 {
            for (ent, first, _, q) in topics {
                let at = tick();
                let ents = vec![ent.to_string()];
                m.record_episode(&p, "w", None, q, first, &ents, 0.5).unwrap();
                let mut e = bare(at, 0, Vec::new());
                e.text = format!("Q: {} A: {}", clip(q, 240), clip(first, 400));
                (e.entities, e.key) = (ents, Some(crate::hash::normalize_question(q)));
                legacy.add(e);
            }
        }
        // Three facts change, and each change arrives twice.
        for _ in 0..2 {
            for (ent, _, current, _) in topics.iter().filter(|t| !t.2.is_empty()) {
                let at = tick();
                m.remember(&p, "w", fact(current, &[ent])).unwrap();
                legacy.add(legacy_fact(at, current, ent));
            }
        }

        let k = MemoryConfig::default().recall_k;
        let budget = MemoryConfig::default().recall_budget_tokens;
        let score = |out: &mut Outcome, got: &[MemoryItem], first: &str, current: &str| {
            let right = if current.is_empty() { first } else { current };
            let norm = |s: &str| s.trim_end_matches('.').to_lowercase();
            out.tokens += got.iter().map(recall_cost).sum::<usize>();
            let facts: Vec<&MemoryItem> = got.iter().filter(|i| i.scope == MemoryScope::LongTerm).collect();
            out.right_in_k += usize::from(facts.iter().any(|i| norm(&i.text) == norm(right)));
            out.right_first += usize::from(facts.first().is_some_and(|i| norm(&i.text) == norm(right)));
            if !current.is_empty() {
                out.stale += facts.iter().filter(|i| norm(&i.text) == norm(first)).count();
            }
        };
        let mut before = Outcome { items: legacy.items.len(), ..Default::default() };
        let s = m.stats();
        let mut after = Outcome { items: s.episodic + s.long_term, ..Default::default() };
        for (ent, first, current, q) in topics {
            let ents = vec![ent.to_string()];
            let key = crate::hash::normalize_question(q);
            // As the ask path does: k + 2, own question dropped, then k.
            let old: Vec<MemoryItem> = legacy
                .recall(q, &ents, k + 2, now)
                .into_iter()
                .filter(|i| i.key.as_deref() != Some(key.as_str()))
                .take(k)
                .collect();
            let opts =
                RecallOptions { budget_tokens: Some(budget), exclude_key: Some(key.clone()), ..Default::default() };
            let new: Vec<MemoryItem> =
                m.recall_with(&p, "w", q, &ents, k + 2, &opts).into_iter().map(|h| h.item).take(k).collect();
            score(&mut before, &old, first, current);
            score(&mut after, &new, first, current);
        }
        println!("redundant stream, {} topics: before {before:?}", topics.len());
        println!("redundant stream, {} topics: after  {after:?}", topics.len());
        assert!(after.items < before.items);
        assert!(after.tokens < before.tokens);
        assert!(after.right_in_k >= before.right_in_k && after.right_first >= before.right_first);
        assert_eq!(after.right_in_k, topics.len());
        assert_eq!(after.stale, 0);
    }
}
