//! Tiny BM25 full-text index and trigram similarity. No external engine.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, RandomState};

use hashbrown::HashTable;

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

/// A posting list longer than this becomes a hash map.
const PROMOTE: usize = 64;
/// A hash-map posting list shorter than this becomes a sorted vector again.
const DEMOTE: usize = 16;

/// The documents of one term with their term frequencies. Most terms occur
/// in one or two documents, so short lists are a sorted vector of
/// `(slot, tf)` pairs; only long ones pay for a hash table. The table is
/// boxed so that every term's entry stays the size of a `Vec`.
#[derive(Clone, Debug)]
enum Posting {
    Small(Vec<(u32, u32)>),
    // Boxed on purpose: an unboxed `HashMap` would make every entry 48 bytes.
    #[allow(clippy::box_collection)]
    Large(Box<HashMap<u32, u32>>),
}

impl Default for Posting {
    fn default() -> Self {
        Posting::Small(Vec::new())
    }
}

impl Posting {
    fn len(&self) -> usize {
        match self {
            Posting::Small(v) => v.len(),
            Posting::Large(m) => m.len(),
        }
    }

    fn insert(&mut self, doc: u32, tf: u32) {
        match self {
            Posting::Small(v) => {
                match v.binary_search_by_key(&doc, |&(d, _)| d) {
                    Ok(i) => v[i].1 = tf,
                    Err(i) => {
                        // A term seen once costs one pair, not the minimum growth of four.
                        if v.capacity() == 0 {
                            v.reserve_exact(1);
                        }
                        v.insert(i, (doc, tf));
                    }
                }
                if v.len() > PROMOTE {
                    *self = Posting::Large(Box::new(v.iter().copied().collect()));
                }
            }
            Posting::Large(m) => {
                m.insert(doc, tf);
            }
        }
    }

    fn remove(&mut self, doc: u32) {
        match self {
            Posting::Small(v) => {
                if let Ok(i) = v.binary_search_by_key(&doc, |&(d, _)| d) {
                    v.remove(i);
                }
            }
            Posting::Large(m) => {
                m.remove(&doc);
                if m.len() < DEMOTE {
                    let mut v: Vec<(u32, u32)> = m.iter().map(|(&d, &tf)| (d, tf)).collect();
                    v.sort_unstable();
                    *self = Posting::Small(v);
                }
            }
        }
    }

    fn iter(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let (small, large) = match self {
            Posting::Small(v) => (Some(v.iter().copied()), None),
            Posting::Large(m) => (None, Some(m.iter().map(|(&d, &tf)| (d, tf)))),
        };
        small.into_iter().flatten().chain(large.into_iter().flatten())
    }
}

/// Largest score array (in entries) a thread keeps between searches: 1 MiB
/// of `f32`. A search over a larger index allocates its own array and frees
/// it when done, so no thread holds memory sized for an index that is gone.
const MAX_RETAINED_SCORES: usize = 1 << 18;

thread_local! {
    /// The dense score array of `search`, kept between calls so small indexes
    /// do not allocate per search. Every entry is 0.0 while it sits here. It
    /// costs each searching thread up to `MAX_RETAINED_SCORES` entries.
    static SCORES: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

/// Hand a zeroed score array back for the next search on this thread, unless
/// it is over the retained limit.
fn give_back_scores(scores: Vec<f32>) {
    if scores.capacity() <= MAX_RETAINED_SCORES {
        SCORES.with(|c| *c.borrow_mut() = scores);
    }
}

/// BM25 over integer document slots: each distinct term is stored once and
/// referred to by a `u32` id, postings map slots to term frequencies, scores
/// accumulate in a reused dense array, and only the best candidates are
/// passed through the caller's filter.
#[derive(Clone, Default)]
pub struct TextIndex {
    slot: HashMap<Box<str>, u32>,
    ids: Vec<Box<str>>,
    /// Document length in terms; 0 marks a free slot.
    lens: Vec<u32>,
    free: Vec<u32>,
    /// The distinct term ids of each document, ascending.
    doc_terms: Vec<Box<[u32]>>,
    /// Term ids, hashed by their term string.
    dict: HashTable<u32>,
    /// The string of each term id; empty for a free id.
    term_str: Vec<Box<str>>,
    term_free: Vec<u32>,
    /// The posting list of each term id.
    postings: Vec<Posting>,
    hasher: RandomState,
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

    fn term(&self, term: &str) -> Option<u32> {
        let strs = &self.term_str;
        self.dict.find(self.hasher.hash_one(term), |&t| &*strs[t as usize] == term).copied()
    }

    fn term_or_insert(&mut self, term: &str) -> u32 {
        let h = self.hasher.hash_one(term);
        let strs = &self.term_str;
        if let Some(&t) = self.dict.find(h, |&t| &*strs[t as usize] == term) {
            return t;
        }
        let t = match self.term_free.pop() {
            Some(t) => {
                self.term_str[t as usize] = term.into();
                t
            }
            None => {
                self.term_str.push(term.into());
                self.postings.push(Posting::default());
                (self.term_str.len() - 1) as u32
            }
        };
        let (strs, hasher) = (&self.term_str, &self.hasher);
        self.dict.insert_unique(h, t, |&t| hasher.hash_one(&*strs[t as usize]));
        t
    }

    fn free_term(&mut self, t: u32) {
        let h = self.hasher.hash_one(&*self.term_str[t as usize]);
        if let Ok(entry) = self.dict.find_entry(h, |&x| x == t) {
            entry.remove();
        }
        self.term_str[t as usize] = Box::default();
        self.postings[t as usize] = Posting::default();
        self.term_free.push(t);
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
                self.ids.push(Box::default());
                self.lens.push(0);
                self.doc_terms.push(Box::default());
                (self.ids.len() - 1) as u32
            }
        };
        let mut tids: Vec<u32> = Vec::with_capacity(tf.len());
        for (t, c) in &tf {
            let tid = self.term_or_insert(t);
            self.postings[tid as usize].insert(s, *c);
            tids.push(tid);
        }
        tids.sort_unstable();
        self.total_len += terms.len() as u64;
        self.ids[s as usize] = id.into();
        self.lens[s as usize] = terms.len() as u32;
        self.doc_terms[s as usize] = tids.into_boxed_slice();
        self.slot.insert(id.into(), s);
        self.live += 1;
    }

    pub fn remove(&mut self, id: &str) {
        let Some(s) = self.slot.remove(id) else { return };
        let i = s as usize;
        self.total_len -= u64::from(self.lens[i]);
        for &t in std::mem::take(&mut self.doc_terms[i]).iter() {
            let posting = &mut self.postings[t as usize];
            posting.remove(s);
            if posting.len() == 0 {
                self.free_term(t);
            }
        }
        self.lens[i] = 0;
        self.ids[i] = Box::default();
        self.free.push(s);
        self.live -= 1;
    }

    pub fn search(&self, query: &str, limit: usize, keep: impl FnMut(&str) -> bool) -> Vec<(String, f32)> {
        if self.live == 0 || limit == 0 {
            return Vec::new();
        }
        let mut q = tokenize(query);
        q.sort();
        q.dedup();
        // Taken, not borrowed: a `keep` that searches another index on this
        // thread then gets a fresh buffer instead of a borrow panic.
        let mut scores = SCORES.with(|c| std::mem::take(&mut *c.borrow_mut()));
        scores.resize(self.ids.len(), 0.0);
        let n = self.live as f32;
        let avg = (self.total_len as f32 / n).max(1.0);
        let (k1, b) = (1.2_f32, 0.75_f32);
        let mut touched: Vec<u32> = Vec::new();
        for term in &q {
            let Some(t) = self.term(term) else { continue };
            let posting = &self.postings[t as usize];
            let df = posting.len() as f32;
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for (doc, tf) in posting.iter() {
                let slot = &mut scores[doc as usize];
                if *slot == 0.0 {
                    touched.push(doc);
                }
                let dl = self.lens[doc as usize] as f32;
                let tf = tf as f32;
                *slot += idf * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * dl / avg));
            }
        }
        let out = self.best(&scores, &mut touched, limit, keep);
        for &d in &touched {
            scores[d as usize] = 0.0;
        }
        give_back_scores(scores);
        out
    }

    /// The `limit` best of `touched` that `keep` accepts, best first.
    fn best(
        &self,
        scores: &[f32],
        touched: &mut [u32],
        limit: usize,
        mut keep: impl FnMut(&str) -> bool,
    ) -> Vec<(String, f32)> {
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
                out.push((self.ids[d as usize].to_string(), scores[d as usize]));
                if out.len() == limit {
                    return out;
                }
            }
        }
        // The filter rejected most of the best matches: keep going in order.
        tail.sort_unstable_by(order);
        for &d in tail.iter() {
            if keep(&self.ids[d as usize]) {
                out.push((self.ids[d as usize].to_string(), scores[d as usize]));
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

/// The previous `TextIndex`, unchanged, as the oracle for the differential
/// test: a `String` per term per document and a hash map per posting list.
#[cfg(test)]
mod reference {
    use std::collections::HashMap;

    use super::tokenize;

    #[derive(Clone, Default)]
    pub struct TextIndex {
        slot: HashMap<String, u32>,
        ids: Vec<String>,
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

    #[test]
    fn postings_promote_and_demote_at_their_bounds() {
        let mut p = Posting::default();
        for d in 0..PROMOTE as u32 {
            p.insert(d, 1);
        }
        assert!(matches!(&p, Posting::Small(v) if v.len() == PROMOTE));
        p.insert(PROMOTE as u32, 2);
        assert!(matches!(&p, Posting::Large(m) if m.len() == PROMOTE + 1));
        for d in (DEMOTE as u32..=PROMOTE as u32).rev() {
            assert!(matches!(p, Posting::Large(_)), "still a map with {} docs", p.len());
            p.remove(d);
        }
        // Now DEMOTE docs remain; one more removal goes below the bound.
        assert!(matches!(&p, Posting::Large(m) if m.len() == DEMOTE));
        p.remove(0);
        assert!(matches!(&p, Posting::Small(v) if v.len() == DEMOTE - 1));
        let docs: Vec<(u32, u32)> = p.iter().collect();
        assert_eq!(docs, (1..DEMOTE as u32).map(|d| (d, 1)).collect::<Vec<_>>());
        p.insert(3, 7);
        assert_eq!(p.iter().find(|&(d, _)| d == 3), Some((3, 7)));
        assert_eq!(p.len(), DEMOTE - 1);
    }

    #[test]
    fn only_small_score_arrays_are_kept_between_searches() {
        let retained = || SCORES.with(|c| c.borrow().capacity());
        let mut idx = TextIndex::default();
        idx.upsert("a", "alpha");
        assert_eq!(idx.search("alpha", 10, |_| true).len(), 1);
        assert!(retained() >= 1, "a small array is kept");
        give_back_scores(Vec::with_capacity(MAX_RETAINED_SCORES + 1));
        assert!(retained() <= MAX_RETAINED_SCORES, "an oversized array is dropped, not kept");
        // The kept array is the earlier small one, untouched by the drop.
        let mut zeroed = vec![0.0; 4];
        zeroed.clear();
        give_back_scores(zeroed);
        assert_eq!(retained(), 4);
    }

    #[test]
    fn freed_terms_are_reused() {
        let mut idx = TextIndex::default();
        idx.upsert("a", "alpha beta");
        idx.upsert("b", "gamma");
        idx.remove("a");
        assert_eq!(idx.term_free.len(), 2);
        idx.upsert("c", "delta alpha");
        assert!(idx.term_free.is_empty());
        assert_eq!(idx.term_str.len(), 3);
        assert_eq!(idx.search("alpha", 10, |_| true)[0].0, "c");
        assert!(idx.search("beta", 10, |_| true).is_empty());
    }

    #[test]
    fn matches_the_reference_index_exactly() {
        let mut rng = fastrand::Rng::with_seed(1709);
        let vocab: Vec<String> = (0..300).map(|i| format!("w{i}x")).collect();
        // Skewed towards the first words, so some posting lists grow past
        // `PROMOTE` and shrink back below `DEMOTE` as documents change.
        let word = |rng: &mut fastrand::Rng| vocab[rng.usize(..300) * rng.usize(..300) / 300].as_str();
        let text = |rng: &mut fastrand::Rng| (0..rng.usize(..14)).map(|_| word(rng)).collect::<Vec<_>>().join(" ");
        let mut new = TextIndex::default();
        let mut old = reference::TextIndex::default();
        for _ in 0..5_000 {
            let id = format!("doc:{}", rng.usize(..1_200));
            if rng.u8(..5) == 0 {
                new.remove(&id);
                old.remove(&id);
            } else {
                let t = text(&mut rng);
                new.upsert(&id, &t);
                old.upsert(&id, &t);
            }
            assert_eq!(new.len(), old.len());
        }
        let mut large = 0;
        for p in &new.postings {
            if matches!(p, Posting::Large(_)) {
                large += 1;
            }
        }
        assert!(large > 0, "the run exercises map postings");
        for i in 0..500 {
            let q = (0..rng.usize(1..4)).map(|_| word(&mut rng)).collect::<Vec<_>>().join(" ");
            let limit = [10, 1, 50][i % 3];
            let m = rng.u32(1..5);
            let keep = |id: &str| m == 1 || !(id.len() as u32).is_multiple_of(m);
            let a = new.search(&q, limit, keep);
            let b = old.search(&q, limit, keep);
            assert_eq!(a.len(), b.len(), "query {q:?}");
            for ((ia, sa), (ib, sb)) in a.iter().zip(&b) {
                assert_eq!(ia, ib, "query {q:?}");
                assert_eq!(sa.to_bits(), sb.to_bits(), "query {q:?}, {ia}");
            }
        }
    }
}
