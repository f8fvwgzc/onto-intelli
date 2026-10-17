//! Episodic and long-term memory.
//!
//! * **Episodic**: what was asked and answered, by whom, about which entities.
//!   Expires after `episodic_ttl_days`.
//! * **Long-term**: durable facts, preferences and consolidated summaries.
//!   Written explicitly (`memory.remember`) or promoted by [`Memory::consolidate`]
//!   when the same entities keep coming up.
//!
//! Recall blends BM25 relevance, entity overlap, recency and salience, and is
//! scoped: private memories are only visible to their owner.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use onto_core::store::Store;
use onto_core::text::TextIndex;
use onto_core::time::{new_id, now_ms};
use onto_core::{Error, Id, Principal, Result};

use crate::config::MemoryConfig;

const NS: &str = "memory";
const DAY_MS: i64 = 86_400_000;

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
    pub created_at: i64,
    pub last_access: i64,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub consolidated: bool,
    /// Normalized question for episodes (lets callers skip repeats of the same question).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
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
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ConsolidationReport {
    pub promoted: usize,
    pub updated: usize,
    pub expired: usize,
    pub long_term: usize,
    pub episodic: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MemoryStats {
    pub episodic: usize,
    pub long_term: usize,
    pub revision: u64,
}

pub struct Memory {
    store: Arc<dyn Store>,
    items: RwLock<HashMap<String, MemoryItem>>,
    by_entity: RwLock<HashMap<Id, HashSet<String>>>,
    index: RwLock<TextIndex>,
    cfg: MemoryConfig,
    revision: AtomicU64,
}

impl Memory {
    pub fn open(store: Arc<dyn Store>, cfg: MemoryConfig) -> Result<Self> {
        let mem = Self {
            store,
            items: RwLock::new(HashMap::new()),
            by_entity: RwLock::new(HashMap::new()),
            index: RwLock::new(TextIndex::default()),
            cfg,
            revision: AtomicU64::new(1),
        };
        let now = now_ms();
        for (_, v) in mem.store.kv_scan(NS, "")? {
            if let Ok(item) = serde_json::from_value::<MemoryItem>(v)
                && item.expires_at.is_none_or(|e| e > now)
            {
                mem.index_item(&item);
                mem.items.write().insert(item.id.clone(), item);
            }
        }
        Ok(mem)
    }

    /// Changes whenever long-term memory changes (part of answer-cache keys).
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    fn index_item(&self, item: &MemoryItem) {
        self.index.write().upsert(&item.id, &format!("{} {}", item.text, item.entities.join(" ")));
        let mut by = self.by_entity.write();
        for e in &item.entities {
            by.entry(e.clone()).or_default().insert(item.id.clone());
        }
    }

    fn unindex_item(&self, item: &MemoryItem) {
        self.index.write().remove(&item.id);
        let mut by = self.by_entity.write();
        for e in &item.entities {
            if let Some(set) = by.get_mut(e) {
                set.remove(&item.id);
            }
        }
    }

    fn save(&self, item: MemoryItem) -> Result<MemoryItem> {
        self.store.kv_put(NS, &item.id, &serde_json::to_value(&item)?, item.expires_at)?;
        if let Some(old) = self.items.read().get(&item.id).cloned() {
            self.unindex_item(&old);
        }
        self.index_item(&item);
        self.items.write().insert(item.id.clone(), item.clone());
        // Episodes are recorded on every question; only durable memory changes
        // the revision, so answer caches survive ordinary traffic.
        if item.scope == MemoryScope::LongTerm {
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
        Ok(item)
    }

    /// Store a durable fact or preference.
    pub fn remember(&self, p: &Principal, default_ws: &str, input: RememberInput) -> Result<MemoryItem> {
        let text = input.text.trim();
        if text.is_empty() {
            return Err(Error::invalid("memory text is required"));
        }
        let ws = input.workspace.unwrap_or_else(|| default_ws.to_string());
        if !p.in_workspace(&ws) {
            return Err(Error::Denied(format!("{} has no access to workspace {ws}", p.id)));
        }
        let now = now_ms();
        self.save(MemoryItem {
            id: new_id("mem"),
            workspace: ws,
            scope: MemoryScope::LongTerm,
            kind: input.kind.unwrap_or(MemoryKind::Fact),
            text: text.to_string(),
            entities: input.entities,
            subject: input.private.then(|| p.id.clone()),
            conversation: None,
            salience: input.salience.unwrap_or(0.7).clamp(0.0, 1.0),
            hits: 0,
            created_at: now,
            last_access: now,
            expires_at: None,
            consolidated: false,
            key: None,
        })
    }

    /// Record one question/answer exchange.
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
        let now = now_ms();
        let text = format!("Q: {} A: {}", clip(question, 240), clip(answer, 400));
        self.save(MemoryItem {
            id: new_id("ep"),
            workspace: ws.to_string(),
            scope: MemoryScope::Episodic,
            kind: MemoryKind::Episode,
            text,
            entities: entities.to_vec(),
            subject: Some(p.id.clone()),
            conversation: conversation.map(str::to_string),
            salience: salience.clamp(0.0, 1.0),
            hits: 0,
            created_at: now,
            last_access: now,
            expires_at: Some(now + i64::from(self.cfg.episodic_ttl_days.max(1)) * DAY_MS),
            consolidated: false,
            key: Some(crate::hash::normalize_question(question)),
        })
    }

    fn visible(p: &Principal, ws: &str, item: &MemoryItem) -> bool {
        item.workspace == ws && item.subject.as_ref().is_none_or(|s| s == &p.id || p.is_privileged())
    }

    /// Top-`k` memories for a query, boosted by shared entities.
    pub fn recall(&self, p: &Principal, ws: &str, query: &str, entities: &[Id], k: usize) -> Vec<MemoryHit> {
        if k == 0 {
            return Vec::new();
        }
        let now = now_ms();
        let items = self.items.read();
        let keep =
            |id: &str| items.get(id).is_some_and(|i| Self::visible(p, ws, i) && i.expires_at.is_none_or(|e| e > now));
        let lexical = self.index.read().search(query, 50, keep);
        let max_lex = lexical.first().map(|(_, s)| *s).unwrap_or(1.0).max(1e-6);
        let mut candidates: HashMap<String, f32> = lexical.into_iter().map(|(id, s)| (id, s / max_lex)).collect();
        {
            let by = self.by_entity.read();
            for e in entities {
                for id in by.get(e).into_iter().flatten() {
                    if keep(id) {
                        candidates.entry(id.clone()).or_insert(0.0);
                    }
                }
            }
        }
        let wanted: HashSet<&str> = entities.iter().map(String::as_str).collect();
        let mut hits: Vec<MemoryHit> = candidates
            .into_iter()
            .filter_map(|(id, lex)| {
                let item = items.get(&id)?;
                let overlap = if wanted.is_empty() || item.entities.is_empty() {
                    0.0
                } else {
                    item.entities.iter().filter(|e| wanted.contains(e.as_str())).count() as f32 / wanted.len() as f32
                };
                let recency = match item.scope {
                    MemoryScope::LongTerm => 1.0,
                    MemoryScope::Episodic => (-((now - item.created_at) as f32 / (7.0 * DAY_MS as f32))).exp(),
                };
                let long_term_bonus = if item.scope == MemoryScope::LongTerm { 0.1 } else { 0.0 };
                let score = 0.5 * lex + 0.3 * overlap + 0.1 * recency + 0.05 * item.salience + long_term_bonus;
                (lex > 0.0 || overlap > 0.0).then(|| MemoryHit { item: item.clone(), score })
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| b.item.created_at.cmp(&a.item.created_at)));
        hits.truncate(k);
        drop(items);
        if !hits.is_empty() {
            let mut items = self.items.write();
            for h in &hits {
                if let Some(i) = items.get_mut(&h.item.id) {
                    i.hits += 1;
                    i.last_access = now;
                }
            }
        }
        hits
    }

    pub fn get(&self, id: &str) -> Option<MemoryItem> {
        self.items.read().get(id).cloned()
    }

    pub fn forget(&self, p: &Principal, id: &str) -> Result<bool> {
        let Some(item) = self.get(id) else { return Ok(false) };
        if !(p.is_privileged()
            || item.subject.as_deref() == Some(p.id.as_str())
            || (item.subject.is_none() && p.in_workspace(&item.workspace)))
        {
            return Err(Error::Denied(format!("{} may not delete memory {id}", p.id)));
        }
        self.store.kv_delete(NS, id)?;
        self.unindex_item(&item);
        self.items.write().remove(id);
        self.revision.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    /// Expire old episodes and promote recurring ones into long-term summaries.
    pub fn consolidate(&self) -> Result<ConsolidationReport> {
        let now = now_ms();
        let mut report = ConsolidationReport::default();
        let expired: Vec<MemoryItem> =
            self.items.read().values().filter(|i| i.expires_at.is_some_and(|e| e <= now)).cloned().collect();
        for item in expired {
            self.store.kv_delete(NS, &item.id)?;
            self.unindex_item(&item);
            self.items.write().remove(&item.id);
            report.expired += 1;
        }
        // Group unconsolidated episodes by (workspace, subject, top entities).
        let mut groups: BTreeMap<(String, Option<String>, String), Vec<MemoryItem>> = BTreeMap::new();
        for item in self.items.read().values() {
            if item.scope != MemoryScope::Episodic || item.consolidated || item.entities.is_empty() {
                continue;
            }
            let mut ents = item.entities.clone();
            ents.sort();
            ents.truncate(3);
            groups
                .entry((item.workspace.clone(), item.subject.clone(), ents.join("|")))
                .or_default()
                .push(item.clone());
        }
        let threshold = self.cfg.consolidate_after.max(2);
        for ((ws, subject, key), mut episodes) in groups {
            if episodes.len() < threshold {
                continue;
            }
            episodes.sort_by_key(|e| e.created_at);
            let entities: Vec<Id> = key.split('|').map(str::to_string).collect();
            let recent: Vec<String> = episodes.iter().rev().take(3).map(|e| clip(&e.text, 220)).collect();
            let text = format!(
                "Recurring topic ({} questions) about {}: {}",
                episodes.len(),
                entities.join(", "),
                recent.join(" | ")
            );
            let existing = self
                .items
                .read()
                .values()
                .find(|i| {
                    i.scope == MemoryScope::LongTerm
                        && i.kind == MemoryKind::Summary
                        && i.workspace == ws
                        && i.subject == subject
                        && i.entities == entities
                })
                .cloned();
            let salience = (0.5 + 0.1 * episodes.len() as f32).min(1.0);
            match existing {
                Some(mut s) => {
                    s.text = text;
                    s.hits += episodes.len() as u32;
                    s.salience = s.salience.max(salience);
                    s.last_access = now;
                    self.save(s)?;
                    report.updated += 1;
                }
                None => {
                    self.save(MemoryItem {
                        id: new_id("mem"),
                        workspace: ws,
                        scope: MemoryScope::LongTerm,
                        kind: MemoryKind::Summary,
                        text,
                        entities,
                        subject,
                        conversation: None,
                        salience,
                        hits: episodes.len() as u32,
                        created_at: now,
                        last_access: now,
                        expires_at: None,
                        consolidated: false,
                        key: None,
                    })?;
                    report.promoted += 1;
                }
            }
            for mut e in episodes {
                e.consolidated = true;
                self.save(e)?;
            }
        }
        let stats = self.stats();
        report.long_term = stats.long_term;
        report.episodic = stats.episodic;
        Ok(report)
    }

    pub fn stats(&self) -> MemoryStats {
        let items = self.items.read();
        MemoryStats {
            episodic: items.values().filter(|i| i.scope == MemoryScope::Episodic).count(),
            long_term: items.values().filter(|i| i.scope == MemoryScope::LongTerm).count(),
            revision: self.revision(),
        }
    }

    pub fn list(&self, p: &Principal, ws: &str, scope: Option<MemoryScope>, limit: usize) -> Vec<MemoryItem> {
        let mut v: Vec<MemoryItem> = self
            .items
            .read()
            .values()
            .filter(|i| Self::visible(p, ws, i) && scope.is_none_or(|s| s == i.scope))
            .cloned()
            .collect();
        v.sort_by_key(|i| std::cmp::Reverse(i.created_at));
        v.truncate(limit);
        v
    }
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
    use super::*;
    use onto_core::store::MemoryStore;

    fn mem() -> Memory {
        Memory::open(Arc::new(MemoryStore::default()), MemoryConfig { consolidate_after: 2, ..Default::default() })
            .unwrap()
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
}
