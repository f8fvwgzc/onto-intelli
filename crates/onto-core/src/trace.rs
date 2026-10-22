//! Traces of the engine's work (docs/CONTROL_PLANE.md section 9), following
//! Langfuse: a **trace** is one unit of work (an ask, a governed action, a
//! traced call); its **spans** are the observations inside it (`span`,
//! `generation`, `retrieval`, `tool`, `event`); **scores** attach
//! evaluations and feedback to a trace; a **session** is every trace with
//! the same `session_id`.
//!
//! This module holds the row types the stores read and write, the queries
//! ([`TraceQuery`], [`MetricsQuery`], [`SessionQuery`]) and the in-memory
//! query and aggregate code. [`MemoryStore`](crate::store::MemoryStore)
//! runs the queries here; `PgStore` runs them in SQL and shapes the result
//! with the same helpers ([`TraceMetrics::from_parts`], [`SessionRow::rounded`]),
//! so both give the same answer for the same rows.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::time::{fmt_date, fmt_ts, now_ms};
use crate::{Error, Result};

/// Rows a list returns at most.
pub const MAX_LIST: usize = 200;
/// Rows a list returns when no `limit` is given.
pub const DEFAULT_LIST: usize = 50;
/// Characters of the stored question a list row shows.
pub const PREVIEW_CHARS: usize = 120;
/// Traces the in-memory store keeps (the newest).
pub const MEMORY_TRACES: usize = 50_000;
/// Rows retention deletes per statement.
pub const VACUUM_BATCH: usize = 5_000;
/// The default window of [`MetricsQuery`].
pub const DEFAULT_METRICS_WINDOW_MS: i64 = 7 * 86_400_000;
/// Longest window of [`MetricsQuery`] with hourly buckets.
pub const MAX_HOURLY_WINDOW_MS: i64 = 31 * 86_400_000;
/// Longest window of [`MetricsQuery`].
pub const MAX_METRICS_WINDOW_MS: i64 = 400 * 86_400_000;

/// Replace every NUL character in `v` (strings and object keys) with
/// U+FFFD: PostgreSQL `text` and `jsonb` cannot hold NUL, and one such
/// character would fail a whole batch of traces.
pub fn scrub_nul(v: &mut Value) {
    const NUL: char = '\0';
    match v {
        Value::String(s) if s.contains(NUL) => *s = s.replace(NUL, "\u{FFFD}"),
        Value::Array(a) => a.iter_mut().for_each(scrub_nul),
        Value::Object(m) => {
            if m.keys().any(|k| k.contains(NUL)) {
                let old = std::mem::take(m);
                m.extend(old.into_iter().map(|(k, v)| (k.replace(NUL, "\u{FFFD}"), v)));
            }
            m.values_mut().for_each(scrub_nul);
        }
        _ => {}
    }
}

/// `rows` as one JSON array without NUL characters ([`scrub_nul`]).
pub fn nul_free_json<T: Serialize>(rows: &[T]) -> Result<String> {
    let mut v = serde_json::to_value(rows)?;
    scrub_nul(&mut v);
    Ok(serde_json::to_string(&v)?)
}

/// The message of every trace method a store does not implement.
pub fn unsupported() -> Error {
    Error::Unavailable("traces are not supported by this store".into())
}

/// One trace (a row of `traces`). Field names are the column names.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceRow {
    /// 32 lowercase hex characters (a W3C trace id).
    pub id: String,
    /// Start, Unix ms.
    pub ts: i64,
    pub duration_ms: f64,
    /// `intelli.ask`, `action.apply:<type>`, `ontology.refine`, …
    pub name: String,
    /// `ask` | `action` | `call` | `eval`
    pub kind: String,
    /// `ok` | `error`
    pub status: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub workspace: String,
    /// The principal's id.
    pub principal: String,
    /// The service a principal acts through (impersonation).
    pub via: Option<String>,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    /// The API key's prefix (never the key).
    pub api_key: Option<String>,
    /// `http` | `grpc` | `mcp` | `cli` | `ffi` | `sdk` | `scheduler`
    pub source: String,
    /// The W3C trace id of the caller's `traceparent`: the OpenTelemetry
    /// trace this call was part of. Several traces may share it (one per
    /// call); `id` is always this trace's own.
    pub otel_trace_id: Option<String>,
    /// The caller's span of that `traceparent` (also the root span's parent).
    pub parent_span: Option<String>,
    pub request_id: Option<String>,
    pub intent: Option<String>,
    /// The answer mode of an ask.
    pub mode: Option<String>,
    /// An ask answered from the graph without the LLM.
    pub deterministic: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub llm_calls: i64,
    /// Answer cache and completion cache hits.
    pub cache_hits: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub context_tokens: i64,
    pub saved_tokens: i64,
    pub cost_usd: f64,
    /// `{question}` (clipped); `None` without `capture_content`. Lists leave it out.
    pub input: Option<Value>,
    /// `{answer}` (clipped); `None` without `capture_content`. Lists leave it out.
    pub output: Option<Value>,
    pub metadata: Value,
    pub tags: Vec<String>,
    /// The onto-intelli version that recorded it.
    pub version: String,
    /// Lists only: the first 120 characters of the stored question.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

impl TraceRow {
    /// The stored question, if any.
    pub fn question(&self) -> Option<&str> {
        self.input.as_ref().and_then(|i| i.get("question")).and_then(Value::as_str)
    }

    /// The row as a list shows it: no bodies, with the preview.
    pub fn listed(&self) -> TraceRow {
        TraceRow { preview: preview(self.question()), input: None, output: None, ..self.clone() }
    }
}

/// One observation inside a trace (a row of `spans`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpanRow {
    pub trace_id: String,
    /// 16 lowercase hex characters (a W3C span id).
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    /// `span` | `generation` | `retrieval` | `tool` | `event`
    pub kind: String,
    pub ts: i64,
    pub duration_ms: f64,
    /// `ok` | `error`
    pub status: String,
    pub error: Option<String>,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub cost_usd: f64,
    pub attrs: Value,
    pub input: Option<Value>,
    pub output: Option<Value>,
}

/// An evaluation or feedback on a trace (a row of `scores`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoreRow {
    pub id: String,
    pub trace_id: String,
    pub span_id: Option<String>,
    pub ts: i64,
    /// `eval.pass`, `user.feedback`, `accuracy`, …
    pub name: String,
    /// Numeric and boolean (0 or 1) scores.
    pub value: Option<f64>,
    /// Categorical scores.
    pub label: Option<String>,
    /// `eval` | `api` | `console`
    pub source: String,
    pub author: Option<String>,
    pub comment: Option<String>,
}

/// A trace with its spans, as it is written.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceRecord {
    pub trace: TraceRow,
    pub spans: Vec<SpanRow>,
}

/// A trace with its spans (by start) and scores (by time).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceDetail {
    pub trace: TraceRow,
    pub spans: Vec<SpanRow>,
    pub scores: Vec<ScoreRow>,
}

/// Filters of a trace list. `since` is inclusive, `until` exclusive (Unix ms).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TraceQuery {
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
    /// The trace's model.
    pub model: Option<String>,
    /// Traces of calls made under this OpenTelemetry trace (`traceparent`).
    pub otel_trace_id: Option<String>,
    pub workspace: Option<String>,
    pub mode: Option<String>,
    pub tag: Option<String>,
    /// Traces that took at least this long.
    pub min_ms: Option<f64>,
    /// A substring of the stored question, case-insensitive.
    pub q: Option<String>,
    /// At most 200; default 50.
    pub limit: Option<usize>,
    /// The `next` of the previous page.
    pub cursor: Option<String>,
}

impl TraceQuery {
    /// The page size: `limit` within 1..=200, default 50.
    pub fn page_size(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIST).clamp(1, MAX_LIST)
    }

    /// The decoded cursor, if any.
    pub fn after(&self) -> Result<Option<(i64, String)>> {
        self.cursor.as_deref().filter(|c| !c.is_empty()).map(decode_cursor).transpose()
    }

    /// Does `t` pass every filter (the cursor aside)?
    pub fn matches(&self, t: &TraceRow) -> bool {
        let eq = |want: &Option<String>, have: &str| want.as_deref().is_none_or(|w| w == have);
        let eq_opt =
            |want: &Option<String>, have: &Option<String>| want.as_deref().is_none_or(|w| have.as_deref() == Some(w));
        self.since.is_none_or(|s| t.ts >= s)
            && self.until.is_none_or(|u| t.ts < u)
            && eq(&self.name, &t.name)
            && eq(&self.kind, &t.kind)
            && eq(&self.status, &t.status)
            && eq(&self.workspace, &t.workspace)
            && eq_opt(&self.user_id, &t.user_id)
            && eq_opt(&self.session_id, &t.session_id)
            && eq_opt(&self.model, &t.model)
            && eq_opt(&self.otel_trace_id, &t.otel_trace_id)
            && eq_opt(&self.mode, &t.mode)
            && self.tag.as_deref().is_none_or(|tag| t.tags.iter().any(|x| x == tag))
            && self.min_ms.is_none_or(|m| t.duration_ms >= m)
            && self
                .q
                .as_deref()
                .filter(|q| !q.is_empty())
                .is_none_or(|q| t.question().is_some_and(|text| text.to_lowercase().contains(&q.to_lowercase())))
    }
}

/// A page of traces, newest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TracePage {
    pub traces: Vec<TraceRow>,
    /// The cursor of the next page, when there is one.
    pub next: Option<String>,
}

/// Aggregates over a window. `since` is inclusive, `until` exclusive (Unix
/// ms); the default window is the last 7 days up to now (inclusive).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsQuery {
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// `hour` | `day` (default).
    pub bucket: Option<String>,
    pub name: Option<String>,
    pub workspace: Option<String>,
}

/// A [`MetricsQuery`] with its defaults applied and checked.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricsWindow {
    pub since: i64,
    pub until: i64,
    /// `hour` or `day`.
    pub bucket: &'static str,
    pub name: Option<String>,
    pub workspace: Option<String>,
}

impl MetricsWindow {
    pub fn matches(&self, t: &TraceRow) -> bool {
        t.ts >= self.since
            && t.ts < self.until
            && self.name.as_deref().is_none_or(|n| n == t.name)
            && self.workspace.as_deref().is_none_or(|w| w == t.workspace)
    }

    /// The bucket label of a time: `2026-10-21` by day, `2026-10-21T13:00` by hour (UTC).
    pub fn label(&self, ts: i64) -> String {
        if self.bucket == "hour" { format!("{}:00", &fmt_ts(ts)[..13]) } else { fmt_date(ts) }
    }
}

impl MetricsQuery {
    /// Defaults (the last 7 days, daily buckets) and limits: `since` before
    /// `until`, at most 400 days, at most 31 days by hour.
    pub fn window(&self) -> Result<MetricsWindow> {
        // By default up to and including this millisecond.
        let until = self.until.unwrap_or_else(|| now_ms() + 1);
        let since = self.since.unwrap_or(until - DEFAULT_METRICS_WINDOW_MS);
        let bucket = match self.bucket.as_deref().unwrap_or("day") {
            "day" => "day",
            "hour" => "hour",
            other => return Err(Error::invalid(format!("bucket '{other}' (hour, day)"))),
        };
        if since >= until {
            return Err(Error::invalid("metrics: since must be before until"));
        }
        if until - since > MAX_METRICS_WINDOW_MS {
            return Err(Error::invalid("metrics: the window is at most 400 days"));
        }
        if bucket == "hour" && until - since > MAX_HOURLY_WINDOW_MS {
            return Err(Error::invalid("metrics: hourly buckets cover at most 31 days"));
        }
        Ok(MetricsWindow {
            since,
            until,
            bucket,
            name: self.name.clone().filter(|n| !n.is_empty()),
            workspace: self.workspace.clone().filter(|w| !w.is_empty()),
        })
    }
}

/// One bucket of [`TraceMetrics::series`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SeriesPoint {
    pub bucket: String,
    pub requests: u64,
    pub errors: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

/// Generation spans of one model.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelStat {
    pub model: String,
    /// Generation spans (completion cache hits included, with no tokens).
    pub calls: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    pub p95_ms: f64,
}

/// Traces of one name.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NameStat {
    pub name: String,
    pub requests: u64,
    pub errors: u64,
    pub p95_ms: f64,
}

/// Scores of one name on the window's traces.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreStat {
    pub name: String,
    pub count: u64,
    /// The mean of the numeric values; `None` when every score is a label.
    pub avg: Option<f64>,
}

/// What [`Store::trace_metrics`](crate::store::Store::trace_metrics) returns.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceMetrics {
    pub requests: u64,
    pub errors: u64,
    pub error_rate: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub cost_usd: f64,
    /// Deterministic asks over all asks.
    pub deterministic_share: f64,
    /// Asks served at least in part from a cache (answer or completion) over all asks.
    pub cache_hit_rate: f64,
    pub llm_calls: i64,
    pub series: Vec<SeriesPoint>,
    pub by_model: Vec<ModelStat>,
    pub by_name: Vec<NameStat>,
    pub scores: Vec<ScoreStat>,
}

/// The window's totals before ratios and rounding.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetricTotals {
    pub requests: u64,
    pub errors: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub cost_usd: f64,
    pub llm_calls: i64,
    pub asks: u64,
    pub deterministic_asks: u64,
    pub cached_asks: u64,
}

fn ratio(n: u64, d: u64) -> f64 {
    if d == 0 { 0.0 } else { round_to(n as f64 / d as f64, 4) }
}

/// `v` rounded to `places` decimals.
pub fn round_to(v: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (v * f).round() / f
}

fn ms3(v: f64) -> f64 {
    round_to(v, 3)
}

fn usd(v: f64) -> f64 {
    round_to(v, 6)
}

impl TraceMetrics {
    /// The metrics from raw aggregates: ratios computed, milliseconds rounded
    /// to 0.001 and costs to 0.000001, lists in their order (series by
    /// bucket, models by calls, names by requests, scores by name).
    pub fn from_parts(
        t: MetricTotals,
        mut series: Vec<SeriesPoint>,
        mut by_model: Vec<ModelStat>,
        mut by_name: Vec<NameStat>,
        mut scores: Vec<ScoreStat>,
    ) -> TraceMetrics {
        for s in &mut series {
            s.p50_ms = ms3(s.p50_ms);
            s.p95_ms = ms3(s.p95_ms);
            s.cost_usd = usd(s.cost_usd);
        }
        series.sort_by(|a, b| a.bucket.cmp(&b.bucket));
        for m in &mut by_model {
            m.p95_ms = ms3(m.p95_ms);
            m.cost_usd = usd(m.cost_usd);
        }
        by_model.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.model.cmp(&b.model)));
        for n in &mut by_name {
            n.p95_ms = ms3(n.p95_ms);
        }
        by_name.sort_by(|a, b| b.requests.cmp(&a.requests).then_with(|| a.name.cmp(&b.name)));
        for s in &mut scores {
            s.avg = s.avg.map(|a| round_to(a, 6));
        }
        scores.sort_by(|a, b| a.name.cmp(&b.name));
        TraceMetrics {
            requests: t.requests,
            errors: t.errors,
            error_rate: ratio(t.errors, t.requests),
            p50_ms: ms3(t.p50_ms),
            p95_ms: ms3(t.p95_ms),
            p99_ms: ms3(t.p99_ms),
            input_tokens: t.input_tokens,
            output_tokens: t.output_tokens,
            cached_tokens: t.cached_tokens,
            cost_usd: usd(t.cost_usd),
            deterministic_share: ratio(t.deterministic_asks, t.asks),
            cache_hit_rate: ratio(t.cached_asks, t.asks),
            llm_calls: t.llm_calls,
            series,
            by_model,
            by_name,
            scores,
        }
    }
}

/// Filters of a session list. `since` is inclusive, `until` exclusive (on
/// the traces' start).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionQuery {
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub user_id: Option<String>,
    /// Only traces of this workspace count (a principal limited to some
    /// workspaces reads one of them at a time).
    pub workspace: Option<String>,
    /// At most 200; default 50.
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

impl SessionQuery {
    pub fn page_size(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIST).clamp(1, MAX_LIST)
    }

    pub fn after(&self) -> Result<Option<(i64, String)>> {
        self.cursor.as_deref().filter(|c| !c.is_empty()).map(decode_cursor).transpose()
    }

    /// Does `t` count towards its session (a session id, in the window, of
    /// the user and the workspace)?
    pub fn matches(&self, t: &TraceRow) -> bool {
        t.session_id.is_some()
            && self.since.is_none_or(|s| t.ts >= s)
            && self.until.is_none_or(|u| t.ts < u)
            && self.user_id.as_deref().is_none_or(|u| t.user_id.as_deref() == Some(u))
            && self.workspace.as_deref().is_none_or(|w| t.workspace == w)
    }
}

/// One session: its traces in the window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionRow {
    pub session_id: String,
    /// The user of its first trace.
    pub user_id: Option<String>,
    pub first_ts: i64,
    pub last_ts: i64,
    pub traces: u64,
    pub errors: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    /// The first 120 characters of its first stored question.
    pub preview: Option<String>,
}

impl SessionRow {
    /// Costs rounded to 0.000001.
    pub fn rounded(mut self) -> Self {
        self.cost_usd = usd(self.cost_usd);
        self
    }
}

/// A page of sessions, the most recently active first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionPage {
    pub sessions: Vec<SessionRow>,
    pub next: Option<String>,
}

/// The first 120 characters of a question.
pub fn preview(question: Option<&str>) -> Option<String> {
    question.map(|q| q.chars().take(PREVIEW_CHARS).collect())
}

/// PostgreSQL's `percentile_cont(p) WITHIN GROUP (ORDER BY …)` over sorted
/// values: linear interpolation between the two nearest ranks. 0 when empty.
pub fn percentile_cont(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let pos = p * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let (a, b) = (sorted[lo], sorted[hi]);
    if lo == hi { a } else { a + (pos - lo as f64) * (b - a) }
}

fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(f64::total_cmp);
    v
}

// ── cursors ──────────────────────────────────────────────────────────────

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url without padding.
pub fn b64url(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// Decode base64url (with or without padding).
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The opaque cursor of a keyset position: base64url of `ts:id`.
pub fn encode_cursor(ts: i64, id: &str) -> String {
    b64url(format!("{ts}:{id}").as_bytes())
}

pub fn decode_cursor(c: &str) -> Result<(i64, String)> {
    let bad = || Error::invalid("invalid cursor");
    let raw = String::from_utf8(b64url_decode(c).ok_or_else(bad)?).map_err(|_| bad())?;
    let (ts, id) = raw.split_once(':').ok_or_else(bad)?;
    Ok((ts.parse().map_err(|_| bad())?, id.to_string()))
}

/// Is `(ts, id)` after the cursor in descending `(ts, id)` order?
fn before(ts: i64, id: &str, cursor: &Option<(i64, String)>) -> bool {
    cursor.as_ref().is_none_or(|(cts, cid)| ts < *cts || (ts == *cts && id < cid.as_str()))
}

// ── ids ──────────────────────────────────────────────────────────────────

fn hex_id(bytes: usize) -> String {
    loop {
        let s: String = (0..bytes).map(|_| format!("{:02x}", fastrand::u8(..))).collect();
        if s.bytes().any(|b| b != b'0') {
            return s;
        }
    }
}

/// A new W3C trace id: 32 lowercase hex characters, not all zero.
pub fn new_trace_id() -> String {
    hex_id(16)
}

/// A new W3C span id: 16 lowercase hex characters, not all zero.
pub fn new_span_id() -> String {
    hex_id(8)
}

fn is_hex_id(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && s.bytes().any(|b| b != b'0')
}

/// The trace id and parent span id of a W3C `traceparent` header
/// (`00-<32 hex>-<16 hex>-<2 hex>`), or `None` when it is invalid.
pub fn parse_traceparent(v: &str) -> Option<(String, String)> {
    let v = v.trim();
    let parts: Vec<&str> = v.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    let (version, trace, parent, flags) = (parts[0], parts[1], parts[2], parts[3]);
    let hex2 = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    // Version 00 has exactly four fields; later versions may append more.
    if !hex2(version) || version == "ff" || (version == "00" && parts.len() != 4) || !hex2(flags) {
        return None;
    }
    (is_hex_id(trace, 32) && is_hex_id(parent, 16)).then(|| (trace.to_string(), parent.to_string()))
}

// ── in-memory queries ────────────────────────────────────────────────────

/// A page of `records` matching `q`, newest first, without bodies.
pub fn list<'a>(records: impl Iterator<Item = &'a TraceRecord>, q: &TraceQuery) -> Result<TracePage> {
    let after = q.after()?;
    let size = q.page_size();
    let mut rows: Vec<&TraceRow> =
        records.map(|r| &r.trace).filter(|t| q.matches(t) && before(t.ts, &t.id, &after)).collect();
    rows.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| b.id.cmp(&a.id)));
    let next = (rows.len() > size).then(|| encode_cursor(rows[size - 1].ts, &rows[size - 1].id));
    Ok(TracePage { traces: rows.into_iter().take(size).map(TraceRow::listed).collect(), next })
}

/// A trace with its spans and scores.
pub fn detail<'a>(record: &TraceRecord, scores: impl Iterator<Item = &'a ScoreRow>) -> TraceDetail {
    let mut spans = record.spans.clone();
    spans.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
    let mut scores: Vec<ScoreRow> = scores.filter(|s| s.trace_id == record.trace.id).cloned().collect();
    scores.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
    TraceDetail { trace: record.trace.clone(), spans, scores }
}

/// A session's traces, oldest first, with their bodies.
pub fn session<'a>(records: impl Iterator<Item = &'a TraceRecord>, id: &str, limit: usize) -> Vec<TraceRow> {
    let mut rows: Vec<TraceRow> =
        records.filter(|r| r.trace.session_id.as_deref() == Some(id)).map(|r| r.trace.clone()).collect();
    rows.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
    rows.truncate(limit.clamp(1, MAX_LIST));
    rows
}

#[derive(Default)]
struct Acc {
    durations: Vec<f64>,
    requests: u64,
    errors: u64,
    input: i64,
    output: i64,
    cost: f64,
}

impl Acc {
    fn add(&mut self, t: &TraceRow) {
        self.durations.push(t.duration_ms);
        self.requests += 1;
        self.errors += u64::from(t.status == "error");
        self.input += t.input_tokens;
        self.output += t.output_tokens;
        self.cost += t.cost_usd;
    }
}

/// [`TraceMetrics`] of `records` (and the `scores` on them) for `q`.
pub fn metrics<'a>(
    records: impl Iterator<Item = &'a TraceRecord>,
    scores: impl Iterator<Item = &'a ScoreRow>,
    q: &MetricsQuery,
) -> Result<TraceMetrics> {
    let w = q.window()?;
    let records: Vec<&TraceRecord> = records.filter(|r| w.matches(&r.trace)).collect();
    let mut totals = MetricTotals::default();
    let mut all = Acc::default();
    let mut buckets: BTreeMap<String, Acc> = BTreeMap::new();
    let mut names: BTreeMap<String, Acc> = BTreeMap::new();
    let mut models: BTreeMap<String, (ModelStat, Vec<f64>)> = BTreeMap::new();
    for r in &records {
        let t = &r.trace;
        all.add(t);
        buckets.entry(w.label(t.ts)).or_default().add(t);
        names.entry(t.name.clone()).or_default().add(t);
        totals.cached_tokens += t.cached_tokens;
        totals.llm_calls += t.llm_calls;
        if t.kind == "ask" {
            totals.asks += 1;
            totals.deterministic_asks += u64::from(t.deterministic);
            totals.cached_asks += u64::from(t.cache_hits > 0);
        }
        for s in r.spans.iter().filter(|s| s.kind == "generation") {
            let model = s.model.clone().unwrap_or_default();
            let (m, d) =
                models.entry(model.clone()).or_insert_with(|| (ModelStat { model, ..Default::default() }, Vec::new()));
            m.calls += 1;
            m.input_tokens += s.input_tokens;
            m.output_tokens += s.output_tokens;
            m.cost_usd += s.cost_usd;
            d.push(s.duration_ms);
        }
    }
    let d = sorted(std::mem::take(&mut all.durations));
    totals.requests = all.requests;
    totals.errors = all.errors;
    totals.p50_ms = percentile_cont(&d, 0.5);
    totals.p95_ms = percentile_cont(&d, 0.95);
    totals.p99_ms = percentile_cont(&d, 0.99);
    totals.input_tokens = all.input;
    totals.output_tokens = all.output;
    totals.cost_usd = all.cost;
    let series = buckets
        .into_iter()
        .map(|(bucket, a)| {
            let d = sorted(a.durations);
            SeriesPoint {
                bucket,
                requests: a.requests,
                errors: a.errors,
                p50_ms: percentile_cont(&d, 0.5),
                p95_ms: percentile_cont(&d, 0.95),
                input_tokens: a.input,
                output_tokens: a.output,
                cost_usd: a.cost,
            }
        })
        .collect();
    let by_name = names
        .into_iter()
        .map(|(name, a)| NameStat {
            name,
            requests: a.requests,
            errors: a.errors,
            p95_ms: percentile_cont(&sorted(a.durations), 0.95),
        })
        .collect();
    let by_model = models
        .into_values()
        .map(|(mut m, d)| {
            m.p95_ms = percentile_cont(&sorted(d), 0.95);
            m
        })
        .collect();
    let ids: std::collections::HashSet<&str> = records.iter().map(|r| r.trace.id.as_str()).collect();
    let mut by_score: BTreeMap<String, (u64, f64, u64)> = BTreeMap::new();
    for s in scores.filter(|s| ids.contains(s.trace_id.as_str())) {
        let e = by_score.entry(s.name.clone()).or_default();
        e.0 += 1;
        if let Some(v) = s.value {
            e.1 += v;
            e.2 += 1;
        }
    }
    let scores = by_score
        .into_iter()
        .map(|(name, (count, sum, n))| ScoreStat { name, count, avg: (n > 0).then(|| sum / n as f64) })
        .collect();
    Ok(TraceMetrics::from_parts(totals, series, by_model, by_name, scores))
}

/// A page of sessions of `records` for `q`, the most recently active first.
pub fn sessions<'a>(records: impl Iterator<Item = &'a TraceRecord>, q: &SessionQuery) -> Result<SessionPage> {
    let after = q.after()?;
    let size = q.page_size();
    let mut groups: BTreeMap<String, Vec<&TraceRow>> = BTreeMap::new();
    for r in records.filter(|r| q.matches(&r.trace)) {
        if let Some(sid) = &r.trace.session_id {
            groups.entry(sid.clone()).or_default().push(&r.trace);
        }
    }
    let mut rows: Vec<SessionRow> = groups
        .into_iter()
        .map(|(session_id, mut ts)| {
            ts.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
            SessionRow {
                user_id: ts[0].user_id.clone(),
                first_ts: ts[0].ts,
                last_ts: ts.iter().map(|t| t.ts).max().unwrap_or_default(),
                traces: ts.len() as u64,
                errors: ts.iter().filter(|t| t.status == "error").count() as u64,
                input_tokens: ts.iter().map(|t| t.input_tokens).sum(),
                output_tokens: ts.iter().map(|t| t.output_tokens).sum(),
                cost_usd: ts.iter().map(|t| t.cost_usd).sum(),
                preview: preview(ts.iter().find_map(|t| t.question())),
                session_id,
            }
            .rounded()
        })
        .filter(|s| before(s.last_ts, &s.session_id, &after))
        .collect();
    rows.sort_by(|a, b| b.last_ts.cmp(&a.last_ts).then_with(|| b.session_id.cmp(&a.session_id)));
    let next = (rows.len() > size).then(|| encode_cursor(rows[size - 1].last_ts, &rows[size - 1].session_id));
    rows.truncate(size);
    Ok(SessionPage { sessions: rows, next })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_match_postgres() {
        // SELECT percentile_cont(p) WITHIN GROUP (ORDER BY x) FROM unnest('{1,2,3,4,10}'::float8[]) x
        let v = [1.0, 2.0, 3.0, 4.0, 10.0];
        assert_eq!(percentile_cont(&v, 0.5), 3.0);
        assert!((percentile_cont(&v, 0.95) - 8.8).abs() < 1e-12);
        assert!((percentile_cont(&v, 0.99) - 9.76).abs() < 1e-12);
        assert_eq!(percentile_cont(&[7.0], 0.95), 7.0);
        assert_eq!(percentile_cont(&[], 0.5), 0.0);
    }

    #[test]
    fn cursors_round_trip() {
        for (ts, id) in [(0, ""), (1_760_000_000_123, "ab12"), (-5, "a:b:c")] {
            assert_eq!(decode_cursor(&encode_cursor(ts, id)).unwrap(), (ts, id.to_string()));
        }
        assert_eq!(b64url(b"hello?"), "aGVsbG8_");
        assert_eq!(b64url_decode("aGVsbG8_").unwrap(), b"hello?");
        assert!(decode_cursor("!!").is_err());
        assert!(decode_cursor(&b64url(b"nocolon")).is_err());
    }

    #[test]
    fn traceparent() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert_eq!(parse_traceparent(tp), Some(("4bf92f3577b34da6a3ce929d0e0e4736".into(), "00f067aa0ba902b7".into())));
        for bad in [
            "",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
        ] {
            assert_eq!(parse_traceparent(bad), None, "{bad}");
        }
        assert!(parse_traceparent("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-future").is_some());
        let (t, s) = (new_trace_id(), new_span_id());
        assert!(is_hex_id(&t, 32) && is_hex_id(&s, 16));
    }

    #[test]
    fn metric_windows() {
        let w = MetricsQuery {
            since: Some(0),
            until: Some(3_600_000 * 5),
            bucket: Some("hour".into()),
            ..Default::default()
        }
        .window()
        .unwrap();
        assert_eq!(w.label(3_600_000 * 2 + 5), "1970-01-01T02:00");
        let d = MetricsQuery { until: Some(86_400_000 * 10), ..Default::default() }.window().unwrap();
        assert_eq!((d.since, d.bucket), (86_400_000 * 3, "day"));
        assert_eq!(d.label(86_400_000 * 4 + 1), "1970-01-05");
        assert!(MetricsQuery { bucket: Some("week".into()), ..Default::default() }.window().is_err());
        assert!(MetricsQuery { since: Some(5), until: Some(5), ..Default::default() }.window().is_err());
        let long = MetricsQuery {
            since: Some(0),
            until: Some(MAX_HOURLY_WINDOW_MS + 1),
            bucket: Some("hour".into()),
            ..Default::default()
        };
        assert!(long.window().is_err());
    }
}
