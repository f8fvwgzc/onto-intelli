//! Keeping the in-memory indexes consistent with a store other processes
//! write to: optimistic commits with retry, catch-up from the commit log,
//! full reloads, and a background follower driven by store notices.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use super::{ActionContext, ChangeSet, Extra, Inner, Ontology, searchable_text};
use crate::graph::{Graph, LinkKey};
use crate::live_schema;
use crate::model::{Id, Mutation, MutationResult};
use crate::similarity::VectorIndex;
use crate::store::StoreNotice;
use crate::text::TextIndex;
use crate::{Error, Result};

/// Retries of a write that lost an optimistic race before giving up.
const MAX_RETRIES: usize = 32;
/// Commit records fetched per catch-up round.
const PAGE: usize = 5_000;

/// What a catch-up did.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SyncReport {
    pub commits: usize,
    pub entities: usize,
    pub links: usize,
    pub schema_reloaded: bool,
    pub full_reload: bool,
    pub seq: u64,
}

impl Ontology {
    /// Commit sequence the in-memory state reflects.
    pub fn seq(&self) -> u64 {
        self.inner.seq.load(Ordering::SeqCst)
    }

    /// Run `build` against the current state and commit what it returns;
    /// when another process committed first, catch up and run it again.
    pub(crate) fn transact<T>(
        &self,
        ctx: &ActionContext,
        mut build: impl FnMut(&Ontology, &Graph) -> Result<(Vec<Mutation>, Extra, T)>,
    ) -> Result<(MutationResult, T)> {
        let mut g = self.inner.graph.write();
        let mut attempt = 0;
        loop {
            let (muts, extra, out) = build(self, &g)?;
            match self.commit_locked(&mut g, ctx, muts, extra) {
                Ok(r) => return Ok((r, out)),
                Err(Error::Stale(m)) if attempt < MAX_RETRIES => {
                    attempt += 1;
                    self.catch_up_locked(&mut g)?;
                    let _ = m;
                }
                Err(Error::Stale(m)) => {
                    return Err(Error::Conflict(format!("write kept losing to concurrent writers: {m}")));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Catch up with commits other processes made. Cheap when nothing changed.
    pub fn sync(&self) -> Result<SyncReport> {
        if !self.inner.store.tracks_commits() {
            return Ok(SyncReport { seq: self.seq(), ..Default::default() });
        }
        let mut g = self.inner.graph.write();
        self.catch_up_locked(&mut g)
    }

    /// Catch up only if the store is ahead (one cheap head read otherwise).
    pub fn sync_if_behind(&self) -> Result<Option<SyncReport>> {
        if !self.inner.store.tracks_commits() || self.inner.store.head()? <= self.seq() {
            return Ok(None);
        }
        self.sync().map(Some)
    }

    pub(crate) fn catch_up_locked(&self, g: &mut Graph) -> Result<SyncReport> {
        let store = self.inner.store.clone();
        let mut report = SyncReport::default();
        loop {
            let from = self.seq();
            let commits = store.commits_since(from, PAGE)?;
            let Some(first) = commits.first() else { break };
            if first.seq != from + 1 || commits.iter().any(|c| c.reload) {
                // Older commits were pruned: rebuild from the store.
                self.reload_locked(g)?;
                report.full_reload = true;
                break;
            }
            let mut entities: BTreeSet<Id> = BTreeSet::new();
            let mut links: BTreeSet<LinkKey> = BTreeSet::new();
            let mut kv: BTreeSet<(String, String)> = BTreeSet::new();
            let last = commits.last().map(|c| c.seq).unwrap_or(from);
            for c in &commits {
                entities.extend(c.entities.iter().cloned());
                links.extend(c.links.iter().cloned());
                kv.extend(c.kv.iter().cloned());
            }
            report.commits += commits.len();
            report.entities += entities.len();
            report.links += links.len();
            if kv.iter().any(|(ns, k)| ns == live_schema::NS_SCHEMA && k == "head") {
                self.reload_schema_locked(g)?;
                report.schema_reloaded = true;
            }
            self.apply_remote(g, &entities, &links, &kv)?;
            self.inner.seq.store(last, Ordering::SeqCst);
            if commits.len() < PAGE {
                break;
            }
        }
        report.seq = self.seq();
        if report.commits > 0 || report.full_reload {
            let revision = self.inner.revision.fetch_add(1, Ordering::SeqCst) + 1;
            let change = ChangeSet {
                revision,
                workspace: String::new(),
                request_id: "sync".into(),
                entities: Vec::new(),
                links: report.links,
            };
            for l in self.inner.listeners.read().clone() {
                l(&change);
            }
        }
        Ok(report)
    }

    fn apply_remote(
        &self,
        g: &mut Graph,
        entities: &BTreeSet<Id>,
        links: &BTreeSet<LinkKey>,
        kv: &BTreeSet<(String, String)>,
    ) -> Result<()> {
        let store = &self.inner.store;
        let schema = self.schema();
        let ids: Vec<Id> = entities.iter().cloned().collect();
        let rows: HashMap<Id, crate::model::Entity> =
            store.get_entities(&ids)?.into_iter().map(|e| (e.id.clone(), e)).collect();
        {
            let mut text = self.inner.text.write();
            let mut vectors = self.inner.vectors.write();
            let in_memory = self.inner.vector_store.is_none();
            for id in &ids {
                match rows.get(id) {
                    Some(e) => {
                        let doc = searchable_text(&schema, e);
                        text.upsert(&e.id, &doc);
                        if in_memory {
                            vectors.upsert(&e.id, &doc);
                        }
                        g.put_entity(e.clone());
                    }
                    None => {
                        g.remove_entity(id);
                        text.remove(id);
                        vectors.remove(id);
                    }
                }
            }
        }
        let keys: Vec<LinkKey> = links.iter().cloned().collect();
        let rows: HashMap<LinkKey, crate::model::Link> =
            store.get_links(&keys)?.into_iter().map(|l| ((l.from.clone(), l.rel.clone(), l.to.clone()), l)).collect();
        for key in &keys {
            match rows.get(key) {
                Some(l) if g.contains(&l.from) && g.contains(&l.to) => g.put_link(l.clone()),
                _ => {
                    g.remove_link(key);
                }
            }
        }
        let redirect_keys: Vec<&String> = kv.iter().filter(|(ns, _)| ns == "redirect").map(|(_, k)| k).collect();
        if !redirect_keys.is_empty() {
            let mut r = self.inner.redirects.write();
            for k in redirect_keys {
                match store.kv_get("redirect", k)?.and_then(|v| v.as_str().map(str::to_string)) {
                    Some(target) => {
                        r.insert(k.clone(), target);
                    }
                    None => {
                        r.remove(k);
                    }
                }
            }
        }
        Ok(())
    }

    /// The stored schema head may have moved: adopt it and re-index kinds
    /// whose property definitions changed.
    fn reload_schema_locked(&self, g: &Graph) -> Result<()> {
        let current = self.active_schema();
        let Some(next) = live_schema::reload_active(&*self.inner.store, &current, self.inner.opts.schema_mode)? else {
            return Ok(());
        };
        let changed: Vec<String> = g
            .kinds()
            .map(|(k, _)| k.clone())
            .filter(|k| {
                let a = serde_json::to_string(&current.schema.all_props(k)).unwrap_or_default();
                let b = serde_json::to_string(&next.schema.all_props(k)).unwrap_or_default();
                a != b
            })
            .collect();
        if !changed.is_empty() {
            let mut text = self.inner.text.write();
            let mut vectors = self.inner.vectors.write();
            // A store-kept vector index was updated by the writer, in the
            // same commit as the schema.
            let in_memory = self.inner.vector_store.is_none();
            for kind in &changed {
                for e in g.of_kind(kind) {
                    let doc = searchable_text(&next.schema, e);
                    text.upsert(&e.id, &doc);
                    if in_memory {
                        vectors.upsert(&e.id, &doc);
                    }
                }
            }
        }
        *self.inner.active.write() = next;
        Ok(())
    }

    /// Rebuild everything from the store (after pruned commits).
    pub(crate) fn reload_locked(&self, g: &mut Graph) -> Result<()> {
        let store = self.inner.store.clone();
        let snap = store.load()?;
        if let Some(next) = live_schema::reload_active(&*store, &self.active_schema(), self.inner.opts.schema_mode)? {
            *self.inner.active.write() = next;
        }
        let schema = self.schema();
        let (graph, text, vectors) =
            build_indexes(&schema, snap.entities, snap.links, self.inner.vector_store.is_none());
        *g = graph;
        *self.inner.text.write() = text;
        *self.inner.vectors.write() = vectors;
        *self.inner.redirects.write() = store
            .kv_scan("redirect", "")?
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|t| (k, t.to_string())))
            .collect();
        self.inner.seq.store(snap.seq, Ordering::SeqCst);
        Ok(())
    }
}

/// Graph, text and (unless the store indexes them) vector indexes for a snapshot.
pub(crate) fn build_indexes(
    schema: &crate::schema::Schema,
    entities: Vec<crate::model::Entity>,
    links: Vec<crate::model::Link>,
    with_vectors: bool,
) -> (Graph, TextIndex, VectorIndex) {
    let mut graph = Graph::default();
    let mut text = TextIndex::default();
    let mut vectors = VectorIndex::default();
    for e in entities {
        let doc = searchable_text(schema, &e);
        text.upsert(&e.id, &doc);
        if with_vectors {
            vectors.upsert(&e.id, &doc);
        }
        graph.put_entity(e);
    }
    for l in links {
        if graph.contains(&l.from) && graph.contains(&l.to) {
            graph.put_link(l);
        }
    }
    (graph, text, vectors)
}

/// Follow a shared store: catch up whenever it says another process
/// committed, and poll at `interval` in case a notice was missed. The thread
/// ends when the ontology is dropped.
pub(crate) fn spawn_follower(inner: &Arc<Inner>, interval: Duration) -> Result<()> {
    let (tx, rx) = mpsc::channel::<()>();
    inner.store.subscribe(Arc::new(move |n: StoreNotice| {
        if !matches!(n, StoreNotice::Kv { .. }) {
            let _ = tx.send(());
        }
    }))?;
    let weak = Arc::downgrade(inner);
    std::thread::Builder::new()
        .name("onto-follow".into())
        .spawn(move || {
            while let Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(interval) {
                while rx.try_recv().is_ok() {}
                let Some(inner) = weak.upgrade() else { break };
                let onto = Ontology { inner };
                let _ = onto.sync_if_behind();
            }
        })
        .map_err(|e| Error::Other(format!("starting the store follower: {e}")))?;
    Ok(())
}
