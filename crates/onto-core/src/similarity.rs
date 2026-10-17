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

fn dot_q(q: &[f32], v: &[i8]) -> f32 {
    q.iter().zip(v).map(|(a, b)| a * f32::from(*b)).sum::<f32>() / 127.0
}

#[derive(Clone, Default)]
pub struct VectorIndex {
    vecs: HashMap<String, Vec<i8>>,
}

impl VectorIndex {
    pub fn len(&self) -> usize {
        self.vecs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vecs.is_empty()
    }

    pub fn upsert(&mut self, id: &str, text: &str) {
        let v = embed(text);
        if v.iter().all(|x| *x == 0.0) {
            self.vecs.remove(id);
        } else {
            self.vecs.insert(id.to_string(), quantize(&v));
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.vecs.remove(id);
    }

    /// Brute-force cosine top-k (fast enough for ~100k entities).
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        min: f32,
        mut keep: impl FnMut(&str) -> bool,
    ) -> Vec<(String, f32)> {
        let q = embed(query);
        if q.iter().all(|x| *x == 0.0) {
            return Vec::new();
        }
        let mut hits: Vec<(String, f32)> = self
            .vecs
            .iter()
            .map(|(id, v)| (id, dot_q(&q, v)))
            .filter(|(_, s)| *s >= min)
            .filter(|(id, _)| keep(id))
            .map(|(id, s)| (id.clone(), s))
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
