//! Token estimation, budget packing, usage/cost accounting.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::Pricing;

/// Fast provider-agnostic estimate: ~4 ASCII chars per token, one token per
/// CJK character. Good enough for budgeting; providers report exact usage.
pub fn estimate(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut wide = 0usize;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else if is_cjk(c) {
            wide += 1;
        } else {
            ascii += 2;
        }
    }
    ascii.div_ceil(4) + wide
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Prompt tokens served from the provider's prompt cache.
    pub cached_tokens: u64,
    pub cost_usd: f64,
    /// True when counts are local estimates rather than provider-reported.
    #[serde(default)]
    pub estimated: bool,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }

    pub fn price(&mut self, p: &Pricing) {
        let uncached = self.prompt_tokens.saturating_sub(self.cached_tokens) as f64;
        self.cost_usd = (uncached * p.input_per_mtok
            + self.cached_tokens as f64 * p.cached_input_per_mtok
            + self.completion_tokens as f64 * p.output_per_mtok)
            / 1_000_000.0;
    }
}

/// An item competing for space in the prompt.
#[derive(Clone, Debug)]
pub struct Packable<T> {
    pub item: T,
    pub score: f32,
    pub tokens: usize,
    /// Always included (if it fits at all).
    pub pinned: bool,
}

#[derive(Debug)]
pub struct Packed<T> {
    pub kept: Vec<T>,
    pub used: usize,
    pub dropped: usize,
    pub dropped_tokens: usize,
}

/// Greedy budget packing: pinned first, then by score (density breaks ties).
/// Items that do not fit are skipped so smaller ones can still use the space.
pub fn pack<T>(mut items: Vec<Packable<T>>, budget: usize) -> Packed<T> {
    items.sort_by(|a, b| {
        b.pinned.cmp(&a.pinned).then_with(|| b.score.total_cmp(&a.score)).then_with(|| {
            let da = a.score / a.tokens.max(1) as f32;
            let db = b.score / b.tokens.max(1) as f32;
            db.total_cmp(&da)
        })
    });
    let mut out = Packed { kept: Vec::new(), used: 0, dropped: 0, dropped_tokens: 0 };
    for p in items {
        if out.used + p.tokens <= budget {
            out.used += p.tokens;
            out.kept.push(p.item);
        } else {
            out.dropped += 1;
            out.dropped_tokens += p.tokens;
        }
    }
    out
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LedgerStats {
    pub asks: u64,
    pub deterministic_answers: u64,
    pub answer_cache_hits: u64,
    pub llm_calls: u64,
    pub llm_cache_hits: u64,
    pub llm_failures: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_prompt_tokens: u64,
    pub cost_usd: f64,
    /// Estimated prompt tokens avoided by deterministic answers, caches and
    /// context pruning (vs. sending the raw neighbourhood as JSON).
    pub tokens_saved: u64,
    /// Trace records dropped because the trace queue was full.
    #[serde(default)]
    pub traces_dropped: u64,
}

/// Process-wide usage accounting.
#[derive(Default)]
pub struct Ledger {
    inner: Mutex<LedgerStats>,
}

impl Ledger {
    pub fn update(&self, f: impl FnOnce(&mut LedgerStats)) {
        f(&mut self.inner.lock());
    }

    pub fn record_llm(&self, u: &TokenUsage) {
        let mut s = self.inner.lock();
        s.llm_calls += 1;
        s.prompt_tokens += u.prompt_tokens;
        s.completion_tokens += u.completion_tokens;
        s.cached_prompt_tokens += u.cached_tokens;
        s.cost_usd += u.cost_usd;
    }

    pub fn snapshot(&self) -> LedgerStats {
        self.inner.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_roughly_four_chars_per_token() {
        assert_eq!(estimate(""), 0);
        assert_eq!(estimate("abcd"), 1);
        assert_eq!(estimate("abcde"), 2);
        assert_eq!(estimate("日本語"), 3);
    }

    #[test]
    fn packs_pinned_then_best() {
        let items = vec![
            Packable { item: "low", score: 0.1, tokens: 5, pinned: false },
            Packable { item: "pin", score: 0.0, tokens: 5, pinned: true },
            Packable { item: "big", score: 0.9, tokens: 50, pinned: false },
            Packable { item: "hi", score: 0.8, tokens: 5, pinned: false },
        ];
        let p = pack(items, 15);
        assert_eq!(p.kept, vec!["pin", "hi", "low"]);
        assert_eq!(p.dropped, 1);
        assert_eq!(p.dropped_tokens, 50);
    }

    #[test]
    fn prices_cached_tokens_separately() {
        let mut u = TokenUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            cached_tokens: 500_000,
            ..Default::default()
        };
        u.price(&Pricing { input_per_mtok: 4.0, output_per_mtok: 20.0, cached_input_per_mtok: 0.2 });
        assert!((u.cost_usd - (2.0 + 0.1 + 20.0)).abs() < 1e-9);
    }
}
