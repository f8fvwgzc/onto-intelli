//! Recording traces (docs/CONTROL_PLANE.md section 9.2).
//!
//! * [`TraceCtx`] is what the caller knows: an adopted W3C trace id and
//!   parent span (`traceparent`), the session and user, where the call came
//!   from (`http`, `grpc`, `mcp`, `cli`, `ffi`, `sdk`, `scheduler`), the API
//!   key's prefix, tags, metadata and a name.
//! * A [`Tracer`] collects the spans of one trace. It sits in a
//!   `tokio::task_local!` for the duration of the call, so code deep inside
//!   (`Intelli::complete`, the action code) adds spans to the trace it runs
//!   in without passing it around: nested LLM calls land in the right trace.
//! * Finished traces go to the tenant's [`TraceSink`]: a bounded queue read
//!   by one task that writes batches with `Store::trace_write` on the
//!   blocking pool. A full queue drops the record and counts it; a failed
//!   write is logged and retried once. Recording never waits for storage.
//! * Content (questions, prompts, answers) is stored only with
//!   `capture_content`, clipped to `max_content_bytes` at a character
//!   boundary; the system prompt only as its hash and length; facts never.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};

use onto_core::Result;
use onto_core::store::Store;
use onto_core::time::now_ms;
use onto_core::trace::{ScoreRow, SpanRow, TraceRecord, TraceRow, new_span_id, new_trace_id};

use crate::config::TracesConfig;
use crate::hash::stable_hash;
use crate::llm::{ChatRole, LlmRequest};

/// Serialized metadata larger than this is replaced by `{"truncated": true}`.
pub const MAX_METADATA_BYTES: usize = 4096;
/// Tags kept per trace, and characters per tag.
pub const MAX_TAGS: usize = 20;
pub const MAX_TAG_CHARS: usize = 64;
/// Characters of session and user ids kept.
pub const MAX_ID_CHARS: usize = 200;

/// What the caller of a traced call knows about the trace.
#[derive(Clone, Debug)]
pub struct TraceCtx {
    /// The caller's trace id (`traceparent`), stored as the trace's
    /// `otel_trace_id`. The trace itself always gets a new id: one upstream
    /// trace often makes several calls, and each is a trace of its own.
    pub otel_trace_id: Option<String>,
    /// The caller's span (`traceparent`): the parent of the root span.
    pub parent_span: Option<String>,
    /// `x-onto-session-id`, or `AskRequest.trace.session_id`.
    pub session_id: Option<String>,
    /// `AskRequest.trace.user_id`, else the principal's entity, else its id.
    pub user_id: Option<String>,
    /// `http` | `grpc` | `mcp` | `cli` | `ffi` | `sdk` | `scheduler`
    pub source: &'static str,
    /// The API key's prefix.
    pub api_key: Option<String>,
    pub tags: Vec<String>,
    /// An object of at most 4 KB.
    pub metadata: Value,
    pub name: Option<String>,
}

impl Default for TraceCtx {
    /// A library call (`source = "sdk"`) with nothing else known.
    fn default() -> Self {
        TraceCtx::from_source("sdk")
    }
}

impl TraceCtx {
    pub fn from_source(source: &'static str) -> Self {
        TraceCtx {
            otel_trace_id: None,
            parent_span: None,
            session_id: None,
            user_id: None,
            source,
            api_key: None,
            tags: Vec::new(),
            metadata: Value::Null,
            name: None,
        }
    }

    /// Adopt a W3C `traceparent` header; an invalid one is ignored.
    pub fn with_traceparent(mut self, header: Option<&str>) -> Self {
        if let Some((t, s)) = header.and_then(onto_core::trace::parse_traceparent) {
            self.otel_trace_id = Some(t);
            self.parent_span = Some(s);
        }
        self
    }

    /// Fold in what an `AskRequest` says (`trace`): the session and user
    /// only when the caller did not set them, tags added, metadata merged.
    pub fn merge(mut self, t: Option<&AskTrace>) -> Self {
        let Some(t) = t else { return self };
        if self.session_id.is_none() {
            self.session_id = t.session_id.clone();
        }
        if self.user_id.is_none() {
            self.user_id = t.user_id.clone();
        }
        self.tags.extend(t.tags.iter().cloned());
        if let Value::Object(extra) = &t.metadata {
            let mut m = match std::mem::take(&mut self.metadata) {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            m.extend(extra.clone());
            self.metadata = Value::Object(m);
        }
        if self.name.is_none() {
            self.name = t.name.clone();
        }
        self
    }

    /// Ids clipped, at most 20 tags of 64 characters (deduplicated), metadata an object of at most 4 KB.
    fn sanitized(mut self) -> Self {
        let id = |v: Option<String>| {
            v.map(|s| s.trim().chars().take(MAX_ID_CHARS).collect::<String>()).filter(|s| !s.is_empty())
        };
        self.session_id = id(self.session_id);
        self.user_id = id(self.user_id);
        self.name = self.name.map(|n| n.chars().take(MAX_ID_CHARS).collect()).filter(|n: &String| !n.is_empty());
        let mut tags: Vec<String> = Vec::new();
        for t in self.tags.iter().map(|t| t.trim().chars().take(MAX_TAG_CHARS).collect::<String>()) {
            if !t.is_empty() && !tags.contains(&t) && tags.len() < MAX_TAGS {
                tags.push(t);
            }
        }
        self.tags = tags;
        self.metadata = match std::mem::take(&mut self.metadata) {
            Value::Object(m) if serde_json::to_string(&m).map_or(0, |s| s.len()) <= MAX_METADATA_BYTES => {
                Value::Object(m)
            }
            Value::Object(_) => json!({ "truncated": true }),
            _ => json!({}),
        };
        self
    }
}

/// `AskRequest.trace`: what SDKs set instead of headers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AskTrace {
    pub session_id: Option<String>,
    pub user_id: Option<String>,
    pub tags: Vec<String>,
    pub metadata: Value,
    pub name: Option<String>,
}

/// A result and the id of the trace that recorded it (none when the call
/// was not traced).
#[derive(Debug)]
pub struct Traced<T> {
    pub result: Result<T>,
    pub trace_id: Option<String>,
}

impl<T> Traced<T> {
    pub fn untraced(result: Result<T>) -> Self {
        Traced { result, trace_id: None }
    }
}

/// A project's trace settings (`TenantSettings.traces`); `None` uses `[traces]`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceSettings {
    /// 1 to 400.
    pub retention_days: Option<u32>,
    pub capture_content: Option<bool>,
}

/// `s` clipped to at most `max` bytes at a character boundary.
pub fn clip_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

tokio::task_local! {
    static CURRENT: Arc<Tracer>;
}

/// The tracer of the call this task runs in, if any.
pub fn current() -> Option<Arc<Tracer>> {
    CURRENT.try_with(Arc::clone).ok()
}

/// Run `f` with `tracer` as the current one.
pub async fn scope<F: Future>(tracer: Arc<Tracer>, f: F) -> F::Output {
    CURRENT.scope(tracer, f).await
}

/// Add a span to the current trace, if there is one (`attrs` is only built then).
pub fn record(name: &str, kind: &str, started: Instant, attrs: impl FnOnce() -> Value) {
    if let Some(t) = current() {
        t.span(name, kind, started, attrs());
    }
}

/// Add an event (a point in time) to the current trace, if there is one.
pub fn event(name: &str, attrs: impl FnOnce() -> Value) {
    if let Some(t) = current() {
        t.span(name, "event", Instant::now(), attrs());
    }
}

#[derive(Default)]
struct TracerState {
    spans: Vec<SpanRow>,
    llm_calls: i64,
    cache_hits: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    cost_usd: f64,
    provider: Option<String>,
    model: Option<String>,
    budget_exceeded: bool,
    llm_error: Option<String>,
}

/// One LLM call as `Intelli::complete` saw it.
pub struct Generation<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub request: &'a LlmRequest,
    pub started: Instant,
    pub ttft_ms: Option<f64>,
    /// `hit`, `miss` or `off` (the completion cache).
    pub cache: &'static str,
    /// Provider-reported usage (zeros for cache hits and refusals).
    pub usage: &'a crate::token::TokenUsage,
    pub response_model: Option<&'a str>,
    pub finish_reason: Option<&'a str>,
    pub text: Option<&'a str>,
    pub budget_exceeded: bool,
    pub error: Option<String>,
}

/// The spans of one trace in the making.
pub struct Tracer {
    trace_id: String,
    /// Span ids are this random prefix and a sequence number, so spans that
    /// start in the same millisecond sort in the order they were recorded.
    span_prefix: String,
    seq: std::sync::atomic::AtomicU32,
    root: String,
    parent: Option<String>,
    started: Instant,
    start_ms: i64,
    capture: bool,
    max_bytes: usize,
    state: Mutex<TracerState>,
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1_000_000.0).round() / 1000.0
}

impl Tracer {
    pub fn new(ctx: &TraceCtx, capture: bool, max_bytes: usize) -> Self {
        let span_prefix = new_span_id()[..8].to_string();
        Tracer {
            trace_id: new_trace_id(),
            root: format!("{span_prefix}00000001"),
            span_prefix,
            seq: std::sync::atomic::AtomicU32::new(1),
            parent: ctx.parent_span.clone(),
            started: Instant::now(),
            start_ms: now_ms(),
            capture,
            max_bytes,
            state: Mutex::new(TracerState::default()),
        }
    }

    pub fn id(&self) -> &str {
        &self.trace_id
    }

    /// The root span's id: the parent of every other span.
    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn captures(&self) -> bool {
        self.capture
    }

    /// `text` clipped, when content is captured.
    pub fn text(&self, text: &str) -> Option<String> {
        self.capture.then(|| clip_bytes(text, self.max_bytes))
    }

    fn at(&self, t: Instant) -> i64 {
        self.start_ms + t.saturating_duration_since(self.started).as_millis() as i64
    }

    fn base(&self, name: &str, kind: &str, started: Instant) -> SpanRow {
        SpanRow {
            trace_id: self.trace_id.clone(),
            id: format!("{}{:08x}", self.span_prefix, self.seq.fetch_add(1, Ordering::Relaxed) + 1),
            parent_id: Some(self.root.clone()),
            name: name.to_string(),
            kind: kind.to_string(),
            ts: self.at(started),
            duration_ms: if kind == "event" { 0.0 } else { ms(started.elapsed()) },
            status: "ok".into(),
            attrs: json!({}),
            ..Default::default()
        }
    }

    /// A finished child span that started at `started`.
    pub fn span(&self, name: &str, kind: &str, started: Instant, attrs: Value) {
        let mut s = self.base(name, kind, started);
        s.attrs = attrs;
        self.state.lock().spans.push(s);
    }

    /// A `generation` span, counted on the trace.
    pub fn generation(&self, g: Generation<'_>) {
        let mut s = self.base("llm", "generation", g.started);
        s.model = Some(g.model.to_string());
        let called = g.cache != "hit" && !g.budget_exceeded;
        s.input_tokens = g.usage.prompt_tokens as i64;
        s.output_tokens = g.usage.completion_tokens as i64;
        s.cached_tokens = g.usage.cached_tokens as i64;
        s.cost_usd = g.usage.cost_usd;
        s.attrs = json!({
            "provider": g.provider,
            "model": g.model,
            "response_model": g.response_model,
            "max_tokens": g.request.max_tokens,
            "latency_ms": s.duration_ms,
            "ttft_ms": g.ttft_ms,
            "finish_reason": g.finish_reason,
            "cache": g.cache,
            "budget_exceeded": g.budget_exceeded,
            "estimated": g.usage.estimated,
            "error": g.error,
        });
        if self.capture {
            let messages: Vec<Value> = g
                .request
                .messages
                .iter()
                .map(|m| {
                    json!({
                        "role": if m.role == ChatRole::User { "user" } else { "assistant" },
                        "content": clip_bytes(&m.content, self.max_bytes),
                    })
                })
                .collect();
            s.input = Some(json!({
                "system": { "hash": stable_hash(&[&g.request.system]), "length": g.request.system.chars().count() },
                "messages": messages,
            }));
            s.output = g.text.map(|t| json!({ "text": clip_bytes(t, self.max_bytes) }));
        } else {
            s.input = Some(json!({
                "system": { "hash": stable_hash(&[&g.request.system]), "length": g.request.system.chars().count() },
                "messages": g.request.messages.len(),
                "length": g.request.messages.iter().map(|m| m.content.chars().count()).sum::<usize>(),
            }));
            s.output = g.text.map(|t| json!({ "length": t.chars().count() }));
        }
        if let Some(e) = &g.error {
            s.status = "error".into();
            s.error = Some(clip_bytes(e, 500));
        }
        let mut st = self.state.lock();
        if called {
            st.llm_calls += 1;
        }
        if g.cache == "hit" {
            st.cache_hits += 1;
        }
        st.input_tokens += s.input_tokens;
        st.output_tokens += s.output_tokens;
        st.cached_tokens += s.cached_tokens;
        st.cost_usd += s.cost_usd;
        st.provider = Some(g.provider.to_string());
        st.model = Some(g.model.to_string());
        st.budget_exceeded |= g.budget_exceeded;
        if let Some(e) = g.error {
            st.llm_error = Some(e);
        }
        st.spans.push(s);
    }

    /// Count an answer cache hit on the trace.
    pub fn cache_hit(&self) {
        self.state.lock().cache_hits += 1;
    }

    /// The failure of the last LLM call that failed, if any.
    pub fn llm_error(&self) -> Option<String> {
        self.state.lock().llm_error.clone()
    }

    /// The record: `trace` with the totals of the spans filled in, the root
    /// span (`root_name`, `root_kind`, `root_attrs`) and the children.
    pub fn finish(&self, mut trace: TraceRow, root_name: &str, root_kind: &str, root_attrs: Value) -> TraceRecord {
        let mut st = self.state.lock();
        trace.id = self.trace_id.clone();
        trace.ts = self.start_ms;
        trace.duration_ms = ms(self.started.elapsed());
        trace.llm_calls = st.llm_calls;
        trace.cache_hits += st.cache_hits;
        trace.input_tokens = st.input_tokens;
        trace.output_tokens = st.output_tokens;
        trace.cached_tokens = st.cached_tokens;
        trace.cost_usd = st.cost_usd;
        if trace.provider.is_none() {
            trace.provider = st.provider.clone();
        }
        if trace.model.is_none() {
            trace.model = st.model.clone();
        }
        if st.budget_exceeded
            && let Value::Object(m) = &mut trace.metadata
        {
            m.insert("budget_exceeded".into(), Value::Bool(true));
        }
        trace.version = crate::VERSION.to_string();
        let root = SpanRow {
            trace_id: self.trace_id.clone(),
            id: self.root.clone(),
            parent_id: self.parent.clone(),
            name: root_name.to_string(),
            kind: root_kind.to_string(),
            ts: self.start_ms,
            duration_ms: trace.duration_ms,
            status: trace.status.clone(),
            error: trace.error_message.clone(),
            attrs: root_attrs,
            ..Default::default()
        };
        let mut spans = Vec::with_capacity(st.spans.len() + 1);
        spans.push(root);
        spans.append(&mut st.spans);
        TraceRecord { trace, spans }
    }
}

/// The trace row every kind starts from: who, where, from where.
pub fn base_row(ctx: &TraceCtx, p: &onto_core::Principal, kind: &str, name: &str, workspace: &str) -> TraceRow {
    let ctx = ctx.clone().sanitized();
    TraceRow {
        name: ctx.name.clone().unwrap_or_else(|| name.to_string()),
        kind: kind.to_string(),
        status: "ok".into(),
        workspace: workspace.to_string(),
        principal: p.id.clone(),
        via: p.via.clone(),
        user_id: ctx.user_id.or_else(|| p.entity.clone()).or_else(|| Some(p.id.clone())),
        session_id: ctx.session_id,
        api_key: ctx.api_key,
        source: ctx.source.to_string(),
        otel_trace_id: ctx.otel_trace_id,
        parent_span: ctx.parent_span,
        metadata: ctx.metadata,
        tags: ctx.tags,
        ..Default::default()
    }
}

/// Mark `row` failed with `e`.
pub fn fail(row: &mut TraceRow, code: &str, message: &str) {
    row.status = "error".into();
    row.error_code = Some(code.to_string());
    row.error_message = Some(clip_bytes(message, 500));
}

// ── worker traces ────────────────────────────────────────────────────────

/// `n` span ids that sort in the order given: a random 8-hex prefix and a
/// sequence number from 1 (the first is a root's).
pub fn span_ids(n: usize) -> Vec<String> {
    let prefix = new_span_id()[..8].to_string();
    (1..=n).map(|i| format!("{prefix}{i:08x}")).collect()
}

/// The trace row of work a background worker did (an automation run, a
/// delivery attempt): no caller, so no session, user or API key; `source
/// = "worker"`. `otel_trace_id` is the trace the work continues.
pub fn worker_row(
    id: String,
    kind: &str,
    name: String,
    workspace: String,
    principal: String,
    otel_trace_id: Option<String>,
) -> TraceRow {
    TraceRow {
        id,
        name,
        kind: kind.to_string(),
        status: "ok".into(),
        workspace,
        user_id: Some(principal.clone()),
        principal,
        source: "worker".into(),
        otel_trace_id: otel_trace_id.filter(|t| onto_core::trace::is_trace_id(t)),
        metadata: json!({}),
        version: crate::VERSION.to_string(),
        ..Default::default()
    }
}

/// A finished span of a worker trace.
pub fn worker_span(trace: &TraceRow, id: String, parent: Option<&str>, name: String, ts: i64, ms: f64) -> SpanRow {
    SpanRow {
        trace_id: trace.id.clone(),
        id,
        parent_id: parent.map(str::to_string),
        name,
        kind: "span".into(),
        ts,
        duration_ms: ms,
        status: "ok".into(),
        attrs: json!({}),
        ..Default::default()
    }
}

// ── the sink ─────────────────────────────────────────────────────────────

/// What the sink writes.
#[derive(Clone, Debug)]
pub enum TraceItem {
    Trace(Box<TraceRecord>),
    Score(Box<ScoreRow>),
}

enum Msg {
    Item(TraceItem),
    Flush(oneshot::Sender<()>),
}

/// A tenant's background trace writer.
pub struct TraceSink {
    tx: mpsc::Sender<Msg>,
    dropped: Arc<AtomicU64>,
    written: Arc<AtomicU64>,
}

impl TraceSink {
    /// Start the writer task on `store` (inside a Tokio runtime).
    pub fn spawn(store: Arc<dyn Store>, cfg: &TracesConfig) -> TraceSink {
        let (tx, rx) = mpsc::channel(cfg.queue.max(1));
        let written = Arc::new(AtomicU64::new(0));
        let every = Duration::from_millis(cfg.flush_ms.max(1));
        tokio::spawn(writer(store, rx, every, cfg.flush_records.max(1), written.clone()));
        TraceSink { tx, dropped: Arc::new(AtomicU64::new(0)), written }
    }

    /// Queue an item without waiting; a full queue drops it (and counts it).
    pub fn send(&self, item: TraceItem) -> bool {
        match self.tx.try_send(Msg::Item(item)) {
            Ok(()) => true,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Wait until everything queued before this call is written (or failed).
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Flush(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// Records dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Records written so far.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }
}

async fn writer(
    store: Arc<dyn Store>,
    mut rx: mpsc::Receiver<Msg>,
    every: Duration,
    max: usize,
    written: Arc<AtomicU64>,
) {
    let mut batch: Vec<TraceItem> = Vec::new();
    let mut waiters: Vec<oneshot::Sender<()>> = Vec::new();
    let take = |m: Msg, batch: &mut Vec<TraceItem>, waiters: &mut Vec<oneshot::Sender<()>>| match m {
        Msg::Item(i) => batch.push(i),
        Msg::Flush(w) => waiters.push(w),
    };
    while let Some(m) = rx.recv().await {
        take(m, &mut batch, &mut waiters);
        let deadline = tokio::time::Instant::now() + every;
        while waiters.is_empty() && batch.len() < max {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(m)) => take(m, &mut batch, &mut waiters),
                Ok(None) | Err(_) => break,
            }
        }
        let n = write(&store, std::mem::take(&mut batch)).await;
        written.fetch_add(n, Ordering::Relaxed);
        for w in waiters.drain(..) {
            let _ = w.send(());
        }
    }
}

/// Write one batch on the blocking pool; one retry, then the items one at a
/// time, so one record the store refuses does not take the others with it.
/// Returns how many items were written.
async fn write(store: &Arc<dyn Store>, items: Vec<TraceItem>) -> u64 {
    if items.is_empty() {
        return 0;
    }
    let n = items.len() as u64;
    let mut traces = Vec::new();
    let mut scores = Vec::new();
    for i in items {
        match i {
            TraceItem::Trace(t) => traces.push(*t),
            TraceItem::Score(s) => scores.push(*s),
        }
    }
    let batch = Arc::new((traces, scores));
    for attempt in 0..2 {
        let (st, b) = (store.clone(), batch.clone());
        let res = tokio::task::spawn_blocking(move || -> Result<()> {
            st.trace_write(&b.0)?;
            for s in &b.1 {
                st.score_write(s)?;
            }
            Ok(())
        })
        .await;
        match res {
            Ok(Ok(())) => return n,
            Ok(Err(e)) if attempt == 0 => {
                tracing::warn!("writing traces: {e}; retrying once");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(Err(e)) => tracing::warn!("writing traces: {e}; writing the batch one record at a time"),
            Err(e) => {
                tracing::error!("writing traces: {e}");
                return 0;
            }
        }
    }
    let st = store.clone();
    tokio::task::spawn_blocking(move || {
        let mut ok = 0u64;
        for t in &batch.0 {
            match st.trace_write(std::slice::from_ref(t)) {
                Ok(()) => ok += 1,
                Err(e) => tracing::error!(trace = %t.trace.id, "writing a trace: {e}; dropped"),
            }
        }
        for s in &batch.1 {
            match st.score_write(s) {
                Ok(()) => ok += 1,
                Err(e) => tracing::error!(score = %s.id, "writing a score: {e}; dropped"),
            }
        }
        ok
    })
    .await
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onto_core::store::MemoryStore;
    use onto_core::trace::TraceQuery;

    fn record(i: usize) -> TraceItem {
        TraceItem::Trace(Box::new(TraceRecord {
            trace: TraceRow { id: format!("{:032x}", i + 1), ts: i as i64, ..Default::default() },
            spans: Vec::new(),
        }))
    }

    /// A store that refuses any batch holding a trace named `bad`.
    #[derive(Default)]
    struct Picky(MemoryStore);

    impl Store for Picky {
        fn load(&self) -> Result<onto_core::store::Snapshot> {
            self.0.load()
        }
        fn write(&self, b: &onto_core::store::WriteBatch) -> Result<u64> {
            self.0.write(b)
        }
        fn events(&self, f: &onto_core::EventFilter) -> Result<Vec<onto_core::Event>> {
            self.0.events(f)
        }
        fn kv_put(&self, ns: &str, key: &str, value: &Value, expires_at: Option<i64>) -> Result<()> {
            self.0.kv_put(ns, key, value, expires_at)
        }
        fn kv_get(&self, ns: &str, key: &str) -> Result<Option<Value>> {
            self.0.kv_get(ns, key)
        }
        fn kv_delete(&self, ns: &str, key: &str) -> Result<()> {
            self.0.kv_delete(ns, key)
        }
        fn kv_scan(&self, ns: &str, prefix: &str) -> Result<Vec<(String, Value)>> {
            self.0.kv_scan(ns, prefix)
        }
        fn stats(&self) -> Result<onto_core::store::StoreStats> {
            self.0.stats()
        }
        fn versions(&self, id: &str, limit: usize) -> Result<Vec<onto_core::store::VersionRecord>> {
            self.0.versions(id, limit)
        }
        fn versions_since(&self, since: i64, limit: usize) -> Result<Vec<onto_core::store::VersionRecord>> {
            self.0.versions_since(since, limit)
        }
        fn trace_write(&self, batch: &[TraceRecord]) -> Result<()> {
            if batch.iter().any(|r| r.trace.name == "bad") {
                return Err(onto_core::Error::Storage("unsupported Unicode escape sequence".into()));
            }
            self.0.trace_write(batch)
        }
        fn trace_list(&self, q: &TraceQuery) -> Result<onto_core::trace::TracePage> {
            self.0.trace_list(q)
        }
    }

    #[tokio::test]
    async fn one_refused_record_does_not_drop_its_batch() {
        let store: Arc<dyn Store> = Arc::new(Picky::default());
        let cfg = TracesConfig { flush_ms: 50, ..Default::default() };
        let sink = TraceSink::spawn(store.clone(), &cfg);
        for i in 0..10 {
            let mut r = record(i);
            if let (5, TraceItem::Trace(t)) = (i, &mut r) {
                t.trace.name = "bad".into();
            }
            assert!(sink.send(r));
        }
        sink.flush().await;
        assert_eq!(sink.written(), 9);
        assert_eq!(store.trace_list(&TraceQuery::default()).unwrap().traces.len(), 9);
    }

    #[tokio::test]
    async fn a_full_queue_drops_and_counts_without_blocking() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::default());
        let cfg = TracesConfig { queue: 4, flush_ms: 5, ..Default::default() };
        let sink = TraceSink::spawn(store.clone(), &cfg);
        // The writer cannot run until this task yields: 4 fit, 96 are dropped.
        let started = Instant::now();
        let sent = (0..100).filter(|&i| sink.send(record(i))).count();
        assert!(started.elapsed() < Duration::from_millis(50), "sending never waits");
        assert_eq!((sent, sink.dropped()), (4, 96));
        sink.flush().await;
        assert_eq!(sink.written(), 4);
        assert_eq!(store.trace_list(&TraceQuery::default()).unwrap().traces.len(), 4);
        // Room again once written.
        assert!(sink.send(record(200)));
        sink.flush().await;
        assert_eq!(store.trace_list(&TraceQuery::default()).unwrap().traces.len(), 5);
    }

    #[tokio::test]
    async fn batches_flush_by_size_and_time() {
        let store: Arc<dyn Store> = Arc::new(MemoryStore::default());
        let cfg = TracesConfig { flush_ms: 20, flush_records: 3, ..Default::default() };
        let sink = TraceSink::spawn(store.clone(), &cfg);
        for i in 0..7 {
            sink.send(record(i));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(store.trace_list(&TraceQuery::default()).unwrap().traces.len(), 7, "the last one by time");
    }

    #[test]
    fn clipping_and_context() {
        assert_eq!(clip_bytes("héllo", 2), "h");
        assert_eq!(clip_bytes("héllo", 3), "hé");
        assert_eq!(clip_bytes("abc", 10), "abc");
        let ctx = TraceCtx {
            session_id: Some("  s1  ".into()),
            tags: vec!["a".into(), "a".into(), "".into(), "x".repeat(100)],
            metadata: json!({ "big": "y".repeat(5000) }),
            ..Default::default()
        }
        .with_traceparent(Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"))
        .merge(Some(&AskTrace {
            session_id: Some("ignored".into()),
            user_id: Some("u1".into()),
            tags: vec!["b".into()],
            ..Default::default()
        }))
        .sanitized();
        assert_eq!(ctx.otel_trace_id.as_deref(), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
        assert_eq!((ctx.session_id.as_deref(), ctx.user_id.as_deref()), (Some("s1"), Some("u1")));
        assert_eq!(ctx.tags, vec!["a".to_string(), "x".repeat(64), "b".into()]);
        assert_eq!(ctx.metadata, json!({ "truncated": true }));
        let bad = TraceCtx::default().with_traceparent(Some("garbage"));
        assert!(bad.otel_trace_id.is_none() && bad.parent_span.is_none());
    }
}
