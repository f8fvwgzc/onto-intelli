//! Bounded TTL + LRU cache with hit statistics.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;

pub struct TtlCache<V: Clone> {
    inner: Mutex<Inner<V>>,
    capacity: usize,
    ttl: Duration,
    hits: AtomicU64,
    misses: AtomicU64,
}

struct Inner<V> {
    map: HashMap<String, Slot<V>>,
    tick: u64,
}

struct Slot<V> {
    value: V,
    inserted: Instant,
    used: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CacheStats {
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
}

impl<V: Clone> TtlCache<V> {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner { map: HashMap::new(), tick: 0 }),
            capacity: capacity.max(1),
            ttl,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn get(&self, key: &str) -> Option<V> {
        let mut inner = self.inner.lock();
        inner.tick += 1;
        let tick = inner.tick;
        let expired = match inner.map.get_mut(key) {
            Some(slot) if slot.inserted.elapsed() <= self.ttl => {
                slot.used = tick;
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Some(slot.value.clone());
            }
            Some(_) => true,
            None => false,
        };
        if expired {
            inner.map.remove(key);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub fn put(&self, key: impl Into<String>, value: V) {
        let mut inner = self.inner.lock();
        inner.tick += 1;
        let tick = inner.tick;
        if inner.map.len() >= self.capacity {
            // Evict the least recently used ~10% in one pass to amortize the scan.
            let mut ages: Vec<(u64, String)> = inner.map.iter().map(|(k, s)| (s.used, k.clone())).collect();
            ages.sort_unstable();
            for (_, k) in ages.into_iter().take((self.capacity / 10).max(1)) {
                inner.map.remove(&k);
            }
        }
        inner.map.insert(key.into(), Slot { value, inserted: Instant::now(), used: tick });
    }

    pub fn clear(&self) {
        self.inner.lock().map.clear();
    }

    pub fn stats(&self) -> CacheStats {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        CacheStats {
            entries: self.inner.lock().map.len(),
            hits,
            misses,
            hit_rate: if hits + misses == 0 { 0.0 } else { hits as f64 / (hits + misses) as f64 },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used() {
        let c = TtlCache::new(2, Duration::from_secs(60));
        c.put("a", 1);
        c.put("b", 2);
        assert_eq!(c.get("a"), Some(1));
        c.put("c", 3);
        assert_eq!(c.get("b"), None);
        assert_eq!(c.get("a"), Some(1));
        assert_eq!(c.get("c"), Some(3));
    }

    #[test]
    fn expires_entries() {
        let c = TtlCache::new(10, Duration::from_millis(0));
        c.put("a", 1);
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(c.get("a"), None);
        assert_eq!(c.stats().misses, 1);
    }
}
