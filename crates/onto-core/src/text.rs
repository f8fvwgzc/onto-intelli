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

#[derive(Clone, Default)]
pub struct TextIndex {
    docs: HashMap<String, u32>,
    terms: HashMap<String, Vec<String>>,
    postings: HashMap<String, HashMap<String, u32>>,
    total_len: u64,
}

impl TextIndex {
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
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
        for (t, c) in &tf {
            self.postings.entry((*t).to_string()).or_default().insert(id.to_string(), *c);
        }
        self.total_len += terms.len() as u64;
        self.docs.insert(id.to_string(), terms.len() as u32);
        self.terms.insert(id.to_string(), tf.into_keys().map(str::to_string).collect());
    }

    pub fn remove(&mut self, id: &str) {
        let Some(len) = self.docs.remove(id) else { return };
        self.total_len -= u64::from(len);
        for t in self.terms.remove(id).unwrap_or_default() {
            if let Some(p) = self.postings.get_mut(&t) {
                p.remove(id);
                if p.is_empty() {
                    self.postings.remove(&t);
                }
            }
        }
    }

    /// BM25 ranking (k1 = 1.2, b = 0.75). `keep` filters candidate ids.
    pub fn search(&self, query: &str, limit: usize, mut keep: impl FnMut(&str) -> bool) -> Vec<(String, f32)> {
        if self.docs.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut q = tokenize(query);
        q.sort();
        q.dedup();
        let n = self.docs.len() as f32;
        let avg = (self.total_len as f32 / n).max(1.0);
        let (k1, b) = (1.2_f32, 0.75_f32);
        let mut scores: HashMap<&str, f32> = HashMap::new();
        for term in &q {
            let Some(posting) = self.postings.get(term) else { continue };
            let df = posting.len() as f32;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for (doc, tf) in posting {
                let dl = *self.docs.get(doc).unwrap_or(&1) as f32;
                let tf = *tf as f32;
                *scores.entry(doc.as_str()).or_default() +=
                    idf * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * dl / avg));
            }
        }
        let mut ranked: Vec<(String, f32)> =
            scores.into_iter().filter(|(id, _)| keep(id)).map(|(id, s)| (id.to_string(), s)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(limit);
        ranked
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
