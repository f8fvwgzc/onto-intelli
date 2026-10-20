//! Similarity without a model: signed feature-hashed word + character-trigram
//! vectors for hybrid (lexical + vector) search, and duplicate detection for
//! entity alignment.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::graph::{Graph, normalize};
use crate::model::{Entity, Id};
use crate::schema::Schema;
use crate::text::{similarity, tokenize};

/// 512 signed buckets, stored quantized to i8 (512 bytes per entity).
pub const DIM: usize = 512;

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn add(v: &mut [f32], feature: &str, weight: f32) {
    let h = fnv(feature);
    let sign = if h >> 63 == 1 { -1.0 } else { 1.0 };
    v[(h % DIM as u64) as usize] += sign * weight;
}

/// Unit vector of hashed word and character-trigram features. Robust to
/// morphology and typos ("deployments" ≈ "deploy", "postgress" ≈ "postgres").
pub fn embed(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    for tok in tokenize(text) {
        add(&mut v, &format!("w:{tok}"), 1.0);
        let padded: Vec<char> = format!("#{tok}#").chars().collect();
        for w in padded.windows(3) {
            add(&mut v, &w.iter().collect::<String>(), 0.7);
        }
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn quantize(v: &[f32]) -> Vec<i8> {
    v.iter().map(|x| (x * 127.0).round().clamp(-127.0, 127.0) as i8).collect()
}

/// The quantized vector of `text` as stores keep it (`None`: no features).
/// Every component is an integer in -127..=127, exact in half precision.
pub fn embed_quantized(text: &str) -> Option<Vec<i8>> {
    let v = embed(text);
    (!v.iter().all(|x| *x == 0.0)).then(|| quantize(&v))
}

fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    a.iter().zip(b).map(|(x, y)| i32::from(*x) * i32::from(*y)).sum()
}

/// Scale of a dot product of two quantized unit vectors.
const SCALE: f32 = 127.0 * 127.0;

/// Quantized vectors stored contiguously (`DIM` bytes each) so a scan is a
/// straight pass over memory the compiler vectorizes; large indexes are
/// scanned on several threads.
#[derive(Clone, Default)]
pub struct VectorIndex {
    ids: Vec<String>,
    data: Vec<i8>,
    slot: HashMap<String, usize>,
}

impl VectorIndex {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn upsert(&mut self, id: &str, text: &str) {
        let v = embed(text);
        if v.iter().all(|x| *x == 0.0) {
            self.remove(id);
            return;
        }
        let q = quantize(&v);
        match self.slot.get(id) {
            Some(&i) => self.data[i * DIM..(i + 1) * DIM].copy_from_slice(&q),
            None => {
                self.slot.insert(id.to_string(), self.ids.len());
                self.ids.push(id.to_string());
                self.data.extend_from_slice(&q);
            }
        }
    }

    pub fn remove(&mut self, id: &str) {
        let Some(i) = self.slot.remove(id) else { return };
        let last = self.ids.len() - 1;
        if i != last {
            self.ids.swap(i, last);
            self.data.copy_within(last * DIM..(last + 1) * DIM, i * DIM);
            self.slot.insert(self.ids[i].clone(), i);
        }
        self.ids.pop();
        self.data.truncate(last * DIM);
    }

    /// The `k` best (slot, score) at or above `min`, best first.
    fn top(&self, q: &[i8], min: i32, k: usize) -> Vec<(usize, i32)> {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let scan = |start: usize, rows: &[i8]| -> Vec<(i32, usize)> {
            let mut heap: BinaryHeap<Reverse<(i32, usize)>> = BinaryHeap::with_capacity(k + 1);
            for (n, row) in rows.as_chunks::<DIM>().0.iter().enumerate() {
                let s = dot_i8(q, row);
                if s < min || (heap.len() == k && heap.peek().is_some_and(|Reverse((m, _))| s <= *m)) {
                    continue;
                }
                heap.push(Reverse((s, start + n)));
                if heap.len() > k {
                    heap.pop();
                }
            }
            heap.into_iter().map(|Reverse(x)| x).collect()
        };
        let n = self.ids.len();
        let threads = if n < 50_000 { 1 } else { std::thread::available_parallelism().map_or(4, |p| p.get()).min(8) };
        let mut all: Vec<(i32, usize)> = if threads == 1 {
            scan(0, &self.data)
        } else {
            let per = n.div_ceil(threads);
            std::thread::scope(|sc| {
                let handles: Vec<_> = (0..threads)
                    .map(|t| {
                        let (a, b) = (t * per, ((t + 1) * per).min(n));
                        let rows = &self.data[a * DIM..b * DIM];
                        sc.spawn(move || scan(a, rows))
                    })
                    .collect();
                handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
            })
        };
        all.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| self.ids[a.1].cmp(&self.ids[b.1])));
        all.truncate(k);
        all.into_iter().map(|(s, i)| (i, s)).collect()
    }

    /// Cosine top-k of entities that `keep` admits, scores at or above `min`.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        min: f32,
        mut keep: impl FnMut(&str) -> bool,
    ) -> Vec<(String, f32)> {
        let q = embed(query);
        if q.iter().all(|x| *x == 0.0) || limit == 0 {
            return Vec::new();
        }
        let q = quantize(&q);
        let min_i = (min * SCALE) as i32;
        let want = (limit * 8).max(64);
        let best = self.top(&q, min_i, want);
        let mut out = Vec::with_capacity(limit);
        for (i, s) in &best {
            if keep(&self.ids[*i]) {
                out.push((self.ids[*i].clone(), *s as f32 / SCALE));
                if out.len() == limit {
                    return out;
                }
            }
        }
        if best.len() < want {
            return out;
        }
        // The filter rejected most of the best matches: scan with it.
        let mut hits: Vec<(String, f32)> = self
            .data
            .as_chunks::<DIM>()
            .0
            .iter()
            .enumerate()
            .filter_map(|(i, row)| {
                let s = dot_i8(&q, row);
                (s >= min_i && keep(&self.ids[i])).then(|| (self.ids[i].clone(), s as f32 / SCALE))
            })
            .collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        hits.truncate(limit);
        hits
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DuplicatePair {
    pub a: Id,
    pub b: Id,
    pub kind: String,
    pub score: f32,
    pub reasons: Vec<String>,
}

fn names(e: &Entity) -> Vec<String> {
    let mut n = vec![normalize(e.label()), normalize(&e.key().replace(['-', '_', '.'], " "))];
    n.extend(e.aliases.iter().map(|a| normalize(a)));
    n.retain(|x| !x.is_empty());
    n.sort();
    n.dedup();
    n
}

fn identity_props(schema: &Schema, kind: &str) -> Vec<String> {
    let mut props = schema.constraints(kind).unique;
    for p in ["email", "url", "external_id"] {
        if !props.iter().any(|x| x == p) {
            props.push(p.to_string());
        }
    }
    props
}

/// Likely duplicates of the same kind: shared identity properties, equal or
/// similar names, boosted by shared neighbours.
pub fn duplicates(
    schema: &Schema,
    g: &Graph,
    workspace: Option<&str>,
    kind: Option<&str>,
    threshold: f32,
    can_read: &dyn Fn(&Entity) -> bool,
) -> Vec<DuplicatePair> {
    let mut by_kind: BTreeMap<&str, Vec<&Entity>> = BTreeMap::new();
    for e in g.entities() {
        if workspace.is_some_and(|w| e.workspace != w) || !can_read(e) {
            continue;
        }
        if kind.is_some_and(|k| !schema.kind_is(&e.kind, k)) {
            continue;
        }
        by_kind.entry(e.kind.as_str()).or_default().push(e);
    }
    let mut out = Vec::new();
    for (k, list) in by_kind {
        let idprops = identity_props(schema, k);
        // Blocking: only compare entities sharing a name token or identity value.
        let mut blocks: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in list.iter().enumerate() {
            let mut keys: HashSet<String> = HashSet::new();
            for n in names(e) {
                for t in n.split(' ').filter(|t| t.chars().count() >= 3) {
                    keys.insert(format!("t:{}", t.chars().take(4).collect::<String>()));
                }
            }
            for p in &idprops {
                if let Some(v) = e.prop_str(p).filter(|v| !v.is_empty()) {
                    keys.insert(format!("p:{p}:{}", v.to_lowercase()));
                }
            }
            for key in keys {
                blocks.entry(key).or_default().push(i);
            }
        }
        let mut pairs: HashSet<(usize, usize)> = HashSet::new();
        for members in blocks.values().filter(|m| m.len() > 1 && m.len() <= 200) {
            for (x, &i) in members.iter().enumerate() {
                for &j in &members[x + 1..] {
                    pairs.insert((i.min(j), i.max(j)));
                }
            }
        }
        let neighbours = |e: &Entity| -> HashSet<&str> { g.steps(&e.id).map(|s| s.other).collect() };
        let mut found: Vec<DuplicatePair> = pairs
            .into_iter()
            .filter_map(|(i, j)| {
                let (a, b) = (list[i], list[j]);
                let mut reasons = Vec::new();
                let mut score = 0.0f32;
                for p in &idprops {
                    if let (Some(x), Some(y)) = (a.prop_str(p), b.prop_str(p))
                        && !x.is_empty()
                        && x.eq_ignore_ascii_case(y)
                    {
                        score = 1.0;
                        reasons.push(format!("same {p}"));
                    }
                }
                let (na, nb) = (names(a), names(b));
                if na.iter().any(|x| nb.contains(x)) {
                    score = score.max(0.95);
                    reasons.push("same name".into());
                } else {
                    let best = na.iter().flat_map(|x| nb.iter().map(move |y| similarity(x, y))).fold(0.0f32, f32::max);
                    if best >= 0.5 {
                        score = score.max(best);
                        reasons.push(format!("similar names ({best:.2})"));
                    }
                }
                let (sa, sb) = (neighbours(a), neighbours(b));
                if !sa.is_empty() && !sb.is_empty() {
                    let shared = sa.intersection(&sb).count();
                    if shared > 0 {
                        let jaccard = shared as f32 / sa.union(&sb).count() as f32;
                        score = (score + 0.15 * jaccard).min(1.0);
                        reasons.push(format!("{shared} shared neighbour(s)"));
                    }
                }
                (score >= threshold).then(|| DuplicatePair {
                    a: a.id.clone(),
                    b: b.id.clone(),
                    kind: k.to_string(),
                    score: (score * 100.0).round() / 100.0,
                    reasons,
                })
            })
            .collect();
        out.append(&mut found);
    }
    out.sort_by(|x, y| y.score.total_cmp(&x.score).then_with(|| x.a.cmp(&y.a)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_tolerate_morphology_and_typos() {
        let a = embed("PostgreSQL replication lag");
        assert!(cosine(&a, &embed("postgres replication")) > 0.5);
        assert!(cosine(&a, &embed("postgress lagging")) > cosine(&a, &embed("frontend button color")));
        let mut idx = VectorIndex::default();
        idx.upsert("x", "Payment timeout under load");
        idx.upsert("y", "Checkout UI polish");
        assert_eq!(idx.search("payments timing out", 5, 0.1, |_| true)[0].0, "x");
    }
}
