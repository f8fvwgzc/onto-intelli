//! Tiny BM25 full-text index and trigram similarity. No external engine.

use std::collections::{HashMap, HashSet};

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "is", "are", "was", "were", "be", "been", "to", "of", "and", "or", "in", "on", "for", "with",
    "by", "at", "it", "its", "this", "that", "these", "those", "what", "who", "whom", "why", "how", "which", "when",
    "where", "does", "do", "did", "has", "have", "had", "as", "from", "about", "me", "my", "our", "we", "you", "your",
    "i", "can", "could", "should", "would", "will", "there", "their", "into", "than", "then", "so", "if", "not", "no",
    "any", "all", "tell", "show", "please", "us", "them", "they",
];

pub fn is_stopword(word: &str) -> bool {
    STOPWORDS.contains(&word)
}

/// Lowercased alphanumeric terms without stopwords; plural `s` stripped.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .filter(|w| !is_stopword(w) && (w.chars().count() > 1 || w.chars().all(|c| c.is_ascii_digit())))
        .map(|w| stem(&w))
        .collect()
}

fn stem(w: &str) -> String {
    if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") && w.is_ascii() {
        w[..w.len() - 1].to_string()
    } else {
        w.to_string()
    }
}

/// BM25 over integer document slots: postings map slots to term frequencies,
/// scores accumulate in a dense array, and only the best candidates are
/// passed through the caller's filter.
#[derive(Clone, Default)]
pub struct TextIndex {
    slot: HashMap<String, u32>,
    ids: Vec<String>,
    /// Document length in terms; 0 marks a free slot.
    lens: Vec<u32>,
    free: Vec<u32>,
    terms: Vec<Vec<String>>,
    postings: HashMap<String, HashMap<u32, u32>>,
    total_len: u64,
    live: usize,
}

impl TextIndex {
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    pub fn upsert(&mut self, id: &str, text: &str) {
        self.remove(id);
        let terms = tokenize(text);
        if terms.is_empty() {
            return;
        }
        let mut tf: HashMap<&str, u32> = HashMap::new();
        for t in &terms {
            *tf.entry(t.as_str()).or_default() += 1;
        }
        let s = match self.free.pop() {
            Some(s) => s,
            None => {
                self.ids.push(String::new());
                self.lens.push(0);
                self.terms.push(Vec::new());
                (self.ids.len() - 1) as u32
            }
        };
        for (t, c) in &tf {
            self.postings.entry((*t).to_string()).or_default().insert(s, *c);
        }
        self.total_len += terms.len() as u64;
        self.ids[s as usize] = id.to_string();
        self.lens[s as usize] = terms.len() as u32;
        self.terms[s as usize] = tf.into_keys().map(str::to_string).collect();
        self.slot.insert(id.to_string(), s);
        self.live += 1;
    }

    pub fn remove(&mut self, id: &str) {
        let Some(s) = self.slot.remove(id) else { return };
        let i = s as usize;
        self.total_len -= u64::from(self.lens[i]);
        for t in std::mem::take(&mut self.terms[i]) {
            if let Some(p) = self.postings.get_mut(&t) {
                p.remove(&s);
                if p.is_empty() {
                    self.postings.remove(&t);
                }
            }
        }
        self.lens[i] = 0;
        self.ids[i].clear();
        self.free.push(s);
        self.live -= 1;
    }

    pub fn search(&self, query: &str, limit: usize, mut keep: impl FnMut(&str) -> bool) -> Vec<(String, f32)> {
        if self.live == 0 || limit == 0 {
            return Vec::new();
        }
        let mut q = tokenize(query);
        q.sort();
        q.dedup();
        let n = self.live as f32;
        let avg = (self.total_len as f32 / n).max(1.0);
        let (k1, b) = (1.2_f32, 0.75_f32);
        let mut scores = vec![0f32; self.ids.len()];
        let mut touched: Vec<u32> = Vec::new();
        for term in &q {
            let Some(posting) = self.postings.get(term) else { continue };
            let df = posting.len() as f32;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for (&doc, &tf) in posting {
                let slot = &mut scores[doc as usize];
                if *slot == 0.0 {
                    touched.push(doc);
                }
                let dl = self.lens[doc as usize] as f32;
                let tf = tf as f32;
                *slot += idf * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * dl / avg));
            }
        }
        let order = |a: &u32, b: &u32| {
            scores[*b as usize]
                .total_cmp(&scores[*a as usize])
                .then_with(|| self.ids[*a as usize].cmp(&self.ids[*b as usize]))
        };
        // Best candidates first; the filter only sees as many as it takes.
        let want = (limit * 8).max(64).min(touched.len());
        if want < touched.len() {
            touched.select_nth_unstable_by(want, order);
        }
        let (head, tail) = touched.split_at_mut(want);
        head.sort_unstable_by(order);
        let mut out = Vec::with_capacity(limit);
        for &d in head.iter() {
            if keep(&self.ids[d as usize]) {
                out.push((self.ids[d as usize].clone(), scores[d as usize]));
                if out.len() == limit {
                    return out;
                }
            }
        }
        // The filter rejected most of the best matches: keep going in order.
        tail.sort_unstable_by(order);
        for &d in tail.iter() {
            if keep(&self.ids[d as usize]) {
                out.push((self.ids[d as usize].clone(), scores[d as usize]));
                if out.len() == limit {
                    break;
                }
            }
        }
        out
    }
}

fn trigrams(s: &str) -> HashSet<[char; 3]> {
    let chars: Vec<char> = format!("  {} ", s.to_lowercase()).chars().collect();
    chars.windows(3).map(|w| [w[0], w[1], w[2]]).collect()
}

/// Jaccard similarity of character trigrams, 0.0..=1.0.
pub fn similarity(a: &str, b: &str) -> f32 {
    let (ta, tb) = (trigrams(a), trigrams(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count();
    inter as f32 / (ta.len() + tb.len() - inter) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_without_stopwords() {
        assert_eq!(
            tokenize("Why is Issue #421 blocking the deployments?"),
            vec!["issue", "421", "blocking", "deployment"]
        );
    }

    #[test]
    fn bm25_ranks_relevant_docs_first() {
        let mut idx = TextIndex::default();
        idx.upsert("a", "payment api timeout under load");
        idx.upsert("b", "frontend button color");
        idx.upsert("c", "payment retries and idempotency keys for the payment service");
        let hits = idx.search("payment timeout", 10, |_| true);
        assert_eq!(hits[0].0, "a");
        assert!(hits.iter().all(|(id, _)| id != "b"));
        idx.remove("a");
        assert_eq!(idx.search("timeout", 10, |_| true).len(), 0);
    }

    #[test]
    fn trigram_similarity_tolerates_typos() {
        assert!(similarity("kenji", "kenji") > 0.99);
        assert!(similarity("payments team", "payment team") > 0.6);
        assert!(similarity("payments", "frontend") < 0.2);
    }
}
