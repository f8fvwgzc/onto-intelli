//! Durable storage behind the in-memory graph.
//!
//! The graph is the hot read path; a [`Store`] persists entities and links,
//! keeps the append-only event log, and offers a small namespaced KV used by
//! memory, conversations and caches.

use std::collections::{HashMap, VecDeque};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Result;
use crate::graph::LinkKey;
use crate::model::{Entity, Event, EventFilter, Id, Link};
use crate::time::now_ms;

#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteStore;

#[derive(Debug, Default)]
pub struct Snapshot {
    pub entities: Vec<Entity>,
    pub links: Vec<Link>,
}

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
    /// KV writes that must commit together with the graph change (ns, key, value).
    pub kv: Vec<(String, String, Value)>,
}

impl WriteBatch {
    pub fn is_empty(&self) -> bool {
        self.put_entities.is_empty()
            && self.del_entities.is_empty()
            && self.put_links.is_empty()
            && self.del_links.is_empty()
            && self.events.is_empty()
            && self.versions.is_empty()
            && self.kv.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoreStats {
    pub backend: String,
    pub events: u64,
    pub kv_entries: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

pub trait Store: Send + Sync {
    /// Everything needed to rebuild the graph at startup.
    fn load(&self) -> Result<Snapshot>;
    fn write(&self, batch: &WriteBatch) -> Result<()>;
    /// Newest first.
    fn events(&self, filter: &EventFilter) -> Result<Vec<Event>>;
    fn kv_put(&self, ns: &str, key: &str, value: &Value, expires_at: Option<i64>) -> Result<()>;
    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<Value>>;
    fn kv_delete(&self, ns: &str, key: &str) -> Result<()>;
    /// All live entries of a namespace whose key starts with `prefix`, key order.
    fn kv_scan(&self, ns: &str, prefix: &str) -> Result<Vec<(String, Value)>>;
    fn stats(&self) -> Result<StoreStats>;
    /// Version history of one entity, newest first.
    fn versions(&self, id: &str, limit: usize) -> Result<Vec<VersionRecord>>;
    /// Versions written at or after `since`, newest first.
    fn versions_since(&self, since: i64, limit: usize) -> Result<Vec<VersionRecord>>;
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
}

#[derive(Default)]
struct MemInner {
    events: VecDeque<Event>,
    versions: VecDeque<VersionRecord>,
    kv: HashMap<String, std::collections::BTreeMap<String, (Value, Option<i64>)>>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::with_event_capacity(100_000)
    }
}

impl MemoryStore {
    pub fn with_event_capacity(max_events: usize) -> Self {
        Self { inner: Mutex::new(MemInner::default()), max_events }
    }
}

impl Store for MemoryStore {
    fn load(&self) -> Result<Snapshot> {
        Ok(Snapshot::default())
    }

    fn write(&self, batch: &WriteBatch) -> Result<()> {
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
        for (ns, key, value) in &batch.kv {
            inner.kv.entry(ns.clone()).or_default().insert(key.clone(), (value.clone(), None));
        }
        Ok(())
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

    fn stats(&self) -> Result<StoreStats> {
        let inner = self.inner.lock();
        Ok(StoreStats {
            backend: "memory".into(),
            events: inner.events.len() as u64,
            kv_entries: inner.kv.values().map(|m| m.len() as u64).sum(),
            path: None,
        })
    }
}
