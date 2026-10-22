//! Per-tenant limits (docs/CONTROL_PLANE.md section 8.5): a rate limit on
//! questions and a monthly LLM budget, with the usage meter that tells the
//! engine how much of the month is spent.
//!
//! * The rate limit is a token bucket per tenant (`asks_per_minute`, burst
//!   equal to the per-minute rate), taken at the start of `intelli.ask`,
//!   `intelli.ask_stream`, `ontology.refine` and `ontology.advise`. Empty:
//!   [`Error::Limited`] ("asks_per_minute (60) reached; retry in 3 s").
//!   Limits are per process.
//! * The budget is checked by `Intelli::complete` before each provider call:
//!   at or over `monthly_cost_usd` or `monthly_tokens` (month to date, UTC,
//!   from the [`UsageMeter`]), the provider is skipped.

use std::time::Instant;

use parking_lot::Mutex;

use onto_core::{Error, Result};

use crate::token::TokenUsage;

/// The limits of one tenant; `None` is no limit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limits {
    pub asks_per_minute: Option<u32>,
    /// USD per calendar month (UTC).
    pub monthly_cost_usd: Option<f64>,
    /// Input plus output tokens per calendar month (UTC).
    pub monthly_tokens: Option<u64>,
}

impl Limits {
    /// Why the month's budget is spent, or `None` while there is room.
    pub fn budget_reached(&self, mtd: &MonthToDate) -> Option<String> {
        if let Some(b) = self.monthly_cost_usd
            && mtd.cost_usd >= b
        {
            return Some(format!("monthly LLM budget of ${b:.2} reached"));
        }
        if let Some(t) = self.monthly_tokens
            && mtd.tokens >= t
        {
            return Some(format!("monthly LLM token budget of {t} tokens reached"));
        }
        None
    }
}

/// What a tenant spent on LLM calls this calendar month (UTC).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MonthToDate {
    pub cost_usd: f64,
    /// Input plus output tokens.
    pub tokens: u64,
}

/// Where a tenant's usage goes (the control plane's `usage_daily`). Both
/// methods are called on the request path and must not block: keep totals
/// in memory and write them in the background.
pub trait UsageMeter: Send + Sync {
    /// Month-to-date totals, including usage recorded but not yet written.
    fn month_to_date(&self) -> MonthToDate;

    /// One question or LLM-using action: its token usage (zero when no
    /// provider was called), the provider and model, whether it was answered
    /// without the LLM, and whether the provider call failed.
    fn record(&self, usage: &TokenUsage, provider: &str, model: &str, deterministic: bool, error: bool);
}

/// A token bucket refilled at `rate` per minute, holding at most `rate`.
#[derive(Default)]
pub struct RateLimiter {
    bucket: Mutex<Option<Bucket>>,
}

struct Bucket {
    rate: u32,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// Take one token at the rate `per_minute` (`None`: no limit). A new
    /// rate starts with a full bucket.
    pub fn take(&self, per_minute: Option<u32>) -> Result<()> {
        self.take_at(per_minute, Instant::now())
    }

    pub fn take_at(&self, per_minute: Option<u32>, now: Instant) -> Result<()> {
        let mut slot = self.bucket.lock();
        let Some(rate) = per_minute.filter(|r| *r > 0) else {
            *slot = None;
            return Ok(());
        };
        let b = match slot.as_mut() {
            Some(b) if b.rate == rate => b,
            _ => slot.insert(Bucket { rate, tokens: rate as f64, last: now }),
        };
        let per_sec = rate as f64 / 60.0;
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * per_sec).min(rate as f64);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            return Ok(());
        }
        let wait = ((1.0 - b.tokens) / per_sec).ceil().max(1.0) as u64;
        Err(Error::Limited(format!("asks_per_minute ({rate}) reached; retry in {wait} s")))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_bucket_refills_at_the_rate() {
        let r = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            r.take_at(Some(3), t0).unwrap();
        }
        let err = r.take_at(Some(3), t0).unwrap_err();
        assert_eq!(err.to_string(), "limited: asks_per_minute (3) reached; retry in 20 s");
        assert_eq!(err.retry_after_secs(), Some(20));
        // One token every 20 s.
        r.take_at(Some(3), t0 + Duration::from_secs(20)).unwrap();
        assert!(r.take_at(Some(3), t0 + Duration::from_secs(21)).is_err());
        // No limit, and a new rate starts full.
        r.take_at(None, t0).unwrap();
        r.take_at(Some(1), t0).unwrap();
        assert!(r.take_at(Some(1), t0).is_err());
    }

    #[test]
    fn budgets() {
        let l = Limits { monthly_cost_usd: Some(50.0), monthly_tokens: Some(1000), ..Default::default() };
        assert_eq!(l.budget_reached(&MonthToDate { cost_usd: 49.99, tokens: 999 }), None);
        assert_eq!(
            l.budget_reached(&MonthToDate { cost_usd: 50.0, tokens: 0 }).as_deref(),
            Some("monthly LLM budget of $50.00 reached")
        );
        assert_eq!(
            l.budget_reached(&MonthToDate { cost_usd: 0.0, tokens: 1000 }).as_deref(),
            Some("monthly LLM token budget of 1000 tokens reached")
        );
        assert_eq!(Limits::default().budget_reached(&MonthToDate { cost_usd: 1e9, tokens: u64::MAX }), None);
    }
}
