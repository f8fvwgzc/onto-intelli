//! The orchestrator: deterministic retrieval first, the LLM only for reasoning.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use onto_core::store::{MemoryStore, Store};
use onto_core::time::new_id;
use onto_core::{
    Access, ActionContext, EntityInput, Error, EventInput, Id, LinkInput, Mutation, Ontology, OntologyOptions, Outcome,
    Principal, Result, Schema,
};

use crate::answer;
use crate::cache::TtlCache;
use crate::changes::{ChangeHub, Reauth, Start, Subscription};
use crate::config::Config;
use crate::context::{self, ContextInput, ContextPack};
use crate::conversation::{Conversation, Conversations};
use crate::hash::{normalize_question, stable_hash};
use crate::ingest::IngestQueue;
use crate::intent::{self, Intent};
use crate::limits::{Limits, RateLimiter, UsageMeter};
use crate::llm::{self, ChatMessage, LlmProvider, LlmRequest, LlmResponse, StreamChunk};
use crate::memory::{Memory, RecallOptions, clip};
use crate::resolve::{self, Resolved};
use crate::token::{Ledger, LedgerStats, TokenUsage, estimate};
use crate::trace::{self, AskTrace, Generation, TraceCtx, TraceItem, TraceSettings, TraceSink, Traced, Tracer};

const SYSTEM_PROMPT: &str = "You are onto-intelli, an enterprise execution assistant. \
Answer using only the workspace facts in the user message: they were retrieved from the \
organization's ontology and filtered to what the asker may see. Cite entity ids in square \
brackets, e.g. [issue:421]. If the facts are not enough, say what is missing instead of \
guessing. Lead with the answer, then the key reasons. Be concise.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskMode {
    /// Deterministic when possible, LLM otherwise.
    #[default]
    Auto,
    /// Never call the LLM.
    Deterministic,
    /// Always use the LLM (still with deterministic retrieval).
    Llm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerMode {
    Deterministic,
    Llm,
    LlmCache,
    AnswerCache,
    Fallback,
    /// The question was ambiguous; `options` lists the candidates.
    Clarify,
    /// A command was planned but not applied (needs confirmation or failed checks).
    Proposal,
    /// A command was applied; `action` holds the receipt.
    Action,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AskRequest {
    pub question: String,
    #[serde(default)]
    pub workspace: Option<String>,
    /// Continue a conversation (`"new"` starts one). Omit for a one-off question.
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub mode: AskMode,
    #[serde(default)]
    pub token_budget: Option<usize>,
    /// Return the full context graph with the answer.
    #[serde(default)]
    pub include_context: bool,
    #[serde(default)]
    pub no_cache: bool,
    /// Apply commands instead of only proposing them.
    #[serde(default)]
    pub confirm: bool,
    /// Session, user, tags, metadata and name of this question's trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<AskTrace>,
}

impl AskRequest {
    pub fn new(question: impl Into<String>) -> Self {
        Self { question: question.into(), ..Default::default() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Timings {
    pub resolve_ms: f64,
    pub retrieve_ms: f64,
    pub llm_ms: f64,
    pub total_ms: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Efficiency {
    /// Tokens of facts actually sent.
    pub context_tokens: usize,
    /// Tokens the same neighbourhood would cost as raw rows.
    pub naive_tokens: usize,
    pub saved_tokens: usize,
    pub facts_used: usize,
    pub facts_dropped: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AskResponse {
    pub answer: String,
    pub mode: AnswerMode,
    pub intent: Intent,
    pub why: bool,
    pub entities: Vec<Resolved>,
    pub facts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub usage: TokenUsage,
    pub efficiency: Efficiency,
    pub timings: Timings,
    pub provider: String,
    pub model: String,
    pub request_id: String,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextPack>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<Resolved>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Value>,
    /// The trace that recorded this answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Streaming event: `stage`, `entities`, `delta`, `done` or `error`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AskEvent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

impl AskEvent {
    fn new(kind: &str, text: impl Into<String>, data: Value) -> Self {
        Self { kind: kind.into(), text: text.into(), data }
    }

    pub fn is_terminal(&self) -> bool {
        self.kind == "done" || self.kind == "error"
    }
}

async fn emit(sink: Option<&mpsc::Sender<AskEvent>>, ev: AskEvent) {
    if let Some(tx) = sink {
        let _ = tx.send(ev).await;
    }
}

fn ms(t: Instant) -> f64 {
    (t.elapsed().as_secs_f64() * 1000.0 * 100.0).round() / 100.0
}

pub struct Intelli {
    config: Config,
    onto: Ontology,
    memory: Memory,
    conversations: Conversations,
    /// Swappable: a project's own provider replaces the server's (`set_llm`).
    llm: parking_lot::RwLock<Arc<dyn LlmProvider>>,
    limits: parking_lot::RwLock<Limits>,
    rate: RateLimiter,
    meter: parking_lot::RwLock<Option<Arc<dyn UsageMeter>>>,
    prompt: parking_lot::Mutex<(u64, Instant, String)>,
    answers: TtlCache<AskResponse>,
    resolutions: TtlCache<Vec<Resolved>>,
    completions: TtlCache<LlmResponse>,
    ledger: Ledger,
    /// The background trace writer (`None` with `[traces] enabled = false`).
    traces: Option<TraceSink>,
    trace_settings: parking_lot::RwLock<TraceSettings>,
    ingest: IngestQueue,
    asks: AtomicUsize,
    started: Instant,
    /// Live change streams of this tenant in this process.
    changes: Arc<ChangeHub>,
    /// Set when the hub closes this tenant: streams and workers end.
    closing: watch::Sender<bool>,
    /// When an open stream last sent a heartbeat (ms; 0 = never).
    stream_touch: AtomicI64,
    /// Seals and opens outbound secrets (the control plane's vault).
    secrets: parking_lot::RwLock<Option<Arc<dyn crate::hub::SecretBox>>>,
    /// Wakes the delivery worker: a commit enqueued outbox rows, or another
    /// process committed.
    outbox_wake: Arc<tokio::sync::Notify>,
    /// Wakes the fan-out of change subscriptions: something was committed.
    fanout_wake: Arc<tokio::sync::Notify>,
    /// Wakes the automation consumer: something was committed, or an
    /// automation was enabled, rerun or switched.
    automation_wake: Arc<tokio::sync::Notify>,
    /// The automation consumer's state in this process.
    automation: parking_lot::Mutex<crate::automate::Runtime>,
    /// Which workers commits wake, and how often they ran.
    wants: Arc<Wants>,
    /// Leases, claimed rows and workers of this tenant in this process,
    /// released on shutdown (crate::lifecycle).
    holdings: crate::lifecycle::Holdings,
    /// The server is shutting down (not only this tenant closing).
    stopping: AtomicBool,
}

/// Commits wake the fan-out only while the tenant has an enabled change
/// subscription, and the automation consumer only while it has an enabled
/// automation; their rounds keep these up to date (they start on, so the
/// first commit wakes both). Actions that change what they want wake them
/// explicitly, and both poll as well.
struct Wants {
    fanout: AtomicBool,
    automation: AtomicBool,
    fanout_rounds: AtomicU64,
    automation_rounds: AtomicU64,
}

impl Intelli {
    /// Open storage, schema, memory and the configured LLM provider.
    /// Must run inside a Tokio runtime (it starts the ingestion worker).
    pub async fn open(config: Config) -> Result<Arc<Self>> {
        let provider = llm::build(&config.llm)?;
        Self::open_with(config, provider).await
    }

    /// Same as [`open`](Self::open) with a custom [`LlmProvider`].
    pub async fn open_with(config: Config, llm: Arc<dyn LlmProvider>) -> Result<Arc<Self>> {
        let store = open_store(&config)?;
        Self::open_with_store(config, store, llm).await
    }

    /// Open on a store the caller built (a tenant's `PgStore`, a shared pool,
    /// a custom backend). The configured schema seeds the store's first version.
    pub async fn open_with_store(
        config: Config,
        store: Arc<dyn Store>,
        llm: Arc<dyn LlmProvider>,
    ) -> Result<Arc<Self>> {
        let schema = load_schema(&config)?;
        config.traces.check()?;
        config.mcp.check()?;
        config.changes.check()?;
        config.webhooks.check()?;
        config.automation.check()?;
        config.writeback.check()?;
        let traces = config.traces.enabled.then(|| TraceSink::spawn(store.clone(), &config.traces));
        let onto = Ontology::open(
            schema,
            store.clone(),
            OntologyOptions {
                strict: config.ontology.strict,
                default_workspace: config.engine.default_workspace.clone(),
                schema_mode: config.ontology.schema_mode,
                governance: config.governance.clone(),
                sync_reads: config.engine.read_consistency == "strong",
                follow_interval_ms: config.engine.follow_interval_ms,
                max_automations: config.automation.max_automations,
                writeback: config.writeback.enabled,
            },
        )?;
        let memory = Memory::open(store.clone(), config.memory.clone())?;
        let conversations = Conversations::new(store);
        let c = &config.cache;
        let prompt = parking_lot::Mutex::new((0, Instant::now(), String::new()));
        let crawl = config.connectors.clone();
        let changes = ChangeHub::new(&onto, config.changes.clone());
        let outbox_wake = Arc::new(tokio::sync::Notify::new());
        let wake = outbox_wake.clone();
        let fanout_wake = Arc::new(tokio::sync::Notify::new());
        let fanout = fanout_wake.clone();
        let automation_wake = Arc::new(tokio::sync::Notify::new());
        let automation = automation_wake.clone();
        let wants = Arc::new(Wants {
            fanout: AtomicBool::new(true),
            automation: AtomicBool::new(true),
            fanout_rounds: AtomicU64::new(0),
            automation_rounds: AtomicU64::new(0),
        });
        let wanted = wants.clone();
        onto.on_change(move |c| {
            // Rows this process enqueued, or another process's commit (which
            // may have enqueued some): look for due rows now.
            if c.outbox > 0 || c.request_id == "sync" {
                wake.notify_one();
            }
            // A commit may hold records a subscription or an automation
            // wants: wake only the workers that have one.
            if wanted.fanout.load(Ordering::Relaxed) {
                fanout.notify_one();
            }
            if wanted.automation.load(Ordering::Relaxed) {
                automation.notify_one();
            }
        });
        let me = Arc::new(Self {
            secrets: parking_lot::RwLock::new(None),
            outbox_wake,
            fanout_wake,
            automation_wake,
            automation: parking_lot::Mutex::new(crate::automate::Runtime::new()),
            wants,
            holdings: Default::default(),
            stopping: AtomicBool::new(false),
            changes,
            closing: watch::Sender::new(false),
            stream_touch: AtomicI64::new(0),
            answers: TtlCache::new(c.max_entries, Duration::from_secs(c.answer_ttl_secs)),
            resolutions: TtlCache::new(c.max_entries, Duration::from_secs(c.answer_ttl_secs)),
            completions: TtlCache::new(c.max_entries, Duration::from_secs(c.llm_ttl_secs)),
            ingest: IngestQueue::spawn(onto.clone(), config.engine.ingest_queue),
            ledger: Ledger::default(),
            traces,
            trace_settings: parking_lot::RwLock::new(TraceSettings::default()),
            asks: AtomicUsize::new(0),
            started: Instant::now(),
            prompt,
            conversations,
            memory,
            onto,
            llm: parking_lot::RwLock::new(llm),
            limits: parking_lot::RwLock::new(Limits::default()),
            rate: RateLimiter::default(),
            meter: parking_lot::RwLock::new(None),
            config,
        });
        if crawl.background {
            crate::connectors::spawn_scheduler(&me, Duration::from_secs(crawl.tick_secs.max(1)));
        }
        if me.traces.is_some() {
            spawn_trace_retention(&me);
        }
        let store = me.onto.store();
        if store.keeps_changes() || store.keeps_outbox() || store.keeps_runs() {
            spawn_retention(&me);
        }
        #[cfg(feature = "outbound")]
        crate::webhooks::spawn_delivery(&me, me.outbox_wake.clone())?;
        crate::webhooks::spawn_fanout(&me, me.fanout_wake.clone());
        crate::automate::spawn_automation_workers(&me, me.automation_wake.clone());
        Ok(me)
    }

    /// Stable system prompt: instructions plus the populated patterns of the
    /// graph (not the whole schema). Recomputed at most every 30 s after a
    /// write; the text only changes when a new pattern appears, so provider
    /// prompt caches stay warm.
    pub fn system_prompt(&self) -> String {
        let rev = self.onto.revision();
        let mut cache = self.prompt.lock();
        if !cache.2.is_empty() && (cache.0 == rev || cache.1.elapsed() < Duration::from_secs(30)) {
            return cache.2.clone();
        }
        let schema = self.onto.schema();
        let body = match self.onto.profile(&Principal::system(), None) {
            Ok(prof) if prof.entities > 0 => onto_core::profile::pattern_summary(&schema, &prof, 60),
            _ => schema.summary(),
        };
        let mut extra = String::new();
        if !schema.interfaces.is_empty() {
            let ifaces: Vec<String> =
                schema.interfaces.keys().map(|i| format!("{i}={}", schema.concrete_kinds(i).join("|"))).collect();
            extra.push_str(&format!("\nInterfaces: {}", ifaces.join(", ")));
        }
        let certified: Vec<&str> = schema.metrics.values().filter(|m| m.certified).map(|m| m.name.as_str()).collect();
        if !certified.is_empty() {
            extra.push_str(&format!("\nCertified metrics: {}", certified.join(", ")));
        }
        let text = format!("{SYSTEM_PROMPT}\n\nOntology:\n{body}{extra}");
        *cache = (rev, Instant::now(), text.clone());
        text
    }

    /// Ambiguous mentions with no confident entity: ask instead of guessing.
    fn clarification(&self, entities: &[Resolved]) -> Option<(String, Vec<Resolved>)> {
        let min = self.config.reasoning.min_confidence;
        if entities.is_empty() || entities.iter().any(|r| r.confidence >= min) {
            return None;
        }
        let first = &entities[0];
        let group: Vec<Resolved> = entities.iter().filter(|r| r.mention == first.mention).cloned().collect();
        if group.len() < 2 {
            return None;
        }
        let names: Vec<String> = group.iter().map(|r| format!("{} ({})", r.title, r.id)).collect();
        Some((format!("Which one do you mean by \"{}\": {}?", first.mention, names.join(" or ")), group))
    }

    /// Volatile engine with the offline mock provider.
    pub async fn in_memory() -> Result<Arc<Self>> {
        Self::open(Config::in_memory()).await
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The tenant's change streams.
    pub fn changes(&self) -> &Arc<ChangeHub> {
        &self.changes
    }

    /// Open a live change stream for `p` (see [`ChangeHub::subscribe`]).
    /// The stream holds only a weak reference to this tenant: it ends
    /// `tenant_closed` when the tenant closes. `reauth` re-derives the
    /// caller's principal every `changes.reauth_secs`.
    pub async fn subscribe_changes(
        self: &Arc<Self>,
        p: Principal,
        f: onto_core::changes::ChangeFilter,
        start: Start,
        reauth: Option<Reauth>,
    ) -> Result<Subscription> {
        self.changes.subscribe(Arc::downgrade(self), p, f, start, reauth).await
    }

    /// Close this tenant for streams and background work: they end (streams
    /// with `tenant_closed`). The hub calls it when it forgets the tenant.
    pub fn close(&self) {
        self.closing.send_replace(true);
    }

    /// The tenant's sealer of outbound secrets (`None` without a master key).
    pub fn secrets(&self) -> Option<Arc<dyn crate::hub::SecretBox>> {
        self.secrets.read().clone()
    }

    /// Set by the hub from the tenant's settings.
    pub fn set_secrets(&self, s: Option<Arc<dyn crate::hub::SecretBox>>) {
        *self.secrets.write() = s;
    }

    /// Wake the delivery worker (rows were enqueued or replayed).
    pub fn wake_outbox(&self) {
        self.outbox_wake.notify_one();
    }

    /// Wakes the automation consumer.
    pub(crate) fn automation_wake(&self) -> &Arc<tokio::sync::Notify> {
        &self.automation_wake
    }

    pub(crate) fn automation_runtime(&self) -> &parking_lot::Mutex<crate::automate::Runtime> {
        &self.automation
    }

    /// Wakes the fan-out of change subscriptions.
    pub(crate) fn wake_fanout(&self) {
        self.fanout_wake.notify_one();
    }

    /// Whether commits wake the fan-out (an enabled change subscription).
    pub(crate) fn set_wants_fanout(&self, on: bool) {
        self.wants.fanout.store(on, Ordering::Relaxed);
    }

    /// Whether commits wake the automation consumer (an enabled automation).
    pub(crate) fn set_wants_automation(&self, on: bool) {
        self.wants.automation.store(on, Ordering::Relaxed);
    }

    pub(crate) fn count_fanout_round(&self) {
        self.wants.fanout_rounds.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_automation_round(&self) {
        self.wants.automation_rounds.fetch_add(1, Ordering::Relaxed);
    }

    /// Switches and paused counts of webhooks and automations, and whether
    /// the change feed captures (docs/design/EVENTS.md section 11), for
    /// `system.health`. A store error leaves the field `null`.
    pub fn outbound_health(&self) -> Value {
        let onto = &self.onto;
        let store = onto.store();
        let now = onto_core::time::now_ms();
        let automation = store.keeps_runs().then(|| {
            let en = onto_core::automate::Enablements::load(&**store).ok();
            json!({
                "process_enabled": self.config.automation.enabled,
                "kill": onto_core::runs::killed(&**store).ok(),
                "enabled": en.as_ref().map(|e| e.states.values().filter(|s| s.enabled).count()),
                "paused": en.as_ref().map(|e| e.states.values().filter(|s| s.enabled && s.paused.is_some()).count()),
            })
        });
        let webhooks = store.keeps_outbox().then(|| {
            let eps = crate::webhooks::Endpoint::list(onto).ok();
            let control: Vec<(String, Value)> =
                store.kv_scan(crate::webhooks::NS_CONTROL, "").unwrap_or_default().into_iter().collect();
            let held = |name: &str| {
                control.iter().any(|(k, v)| {
                    (k == &format!("{}{name}", crate::webhooks::BACKLOG_PREFIX) && !v.is_null())
                        || (k == &format!("{}{name}", crate::webhooks::BREAKER_PREFIX)
                            && serde_json::from_value::<crate::webhooks::BreakerState>(v.clone())
                                .is_ok_and(|b| b.is_open()))
                })
            };
            json!({
                "process_enabled": self.config.webhooks.enabled,
                "kill": crate::webhooks::killed(onto).ok(),
                "endpoints": eps.as_ref().map(Vec::len),
                "paused": eps.as_ref().map(|eps| eps
                    .iter()
                    .filter(|e| e.enabled && (e.paused.as_ref().is_some_and(|p| p.until.is_none_or(|u| u > now)) || held(&e.name)))
                    .count()),
            })
        });
        json!({
            "changes": { "capture": onto.capture_enabled() },
            "automation": automation,
            "webhooks": webhooks,
        })
    }

    /// Outbox rows not yet finished per target, and runs per state over the
    /// last hour (at most the newest 500 counted), for `system.stats`.
    fn outbound_stats(&self) -> Value {
        let store = self.onto.store();
        let open: Option<serde_json::Map<String, Value>> = store
            .keeps_outbox()
            .then(|| store.outbox_open_counts().ok())
            .flatten()
            .map(|v| v.into_iter().map(|(t, n)| (t, json!(n))).collect());
        let runs = store.keeps_runs().then(|| {
            let q = onto_core::runs::RunQuery {
                since: Some(store.clock_ms().unwrap_or_else(|_| onto_core::time::now_ms()) - 3_600_000),
                limit: Some(onto_core::runs::MAX_LIST),
                ..Default::default()
            };
            store.runs_list(&q).ok().map(|rows| {
                let mut by: std::collections::BTreeMap<&'static str, u64> = Default::default();
                for r in &rows {
                    *by.entry(r.state.as_str()).or_default() += 1;
                }
                json!({ "states": by, "counted": rows.len(), "capped": rows.len() >= onto_core::runs::MAX_LIST })
            })
        });
        json!({ "outbox_open": open, "runs_1h": runs.flatten() })
    }

    /// Rounds the fan-out and the automation consumer ran in this process
    /// since the tenant opened (what commits and polls cost), and whether
    /// commits wake them now.
    pub fn worker_rounds(&self) -> Value {
        let w = &self.wants;
        json!({
            "fanout": w.fanout_rounds.load(Ordering::Relaxed),
            "automation": w.automation_rounds.load(Ordering::Relaxed),
            "commits_wake_fanout": w.fanout.load(Ordering::Relaxed),
            "commits_wake_automation": w.automation.load(Ordering::Relaxed),
        })
    }

    pub fn is_closing(&self) -> bool {
        *self.closing.borrow()
    }

    /// The server is shutting down (its streams end saying so).
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    pub(crate) fn mark_stopping(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    pub(crate) fn holdings(&self) -> &crate::lifecycle::Holdings {
        &self.holdings
    }

    pub(crate) fn closing_signal(&self) -> watch::Receiver<bool> {
        self.closing.subscribe()
    }

    /// An open stream is alive (it counts toward the idle timer, never
    /// toward the open-tenant count or the memory budget).
    pub fn touch_stream(&self) {
        self.stream_touch.store(onto_core::time::now_ms(), Ordering::Relaxed);
    }

    /// When an open stream last sent a heartbeat (Unix ms; 0 = never).
    pub fn stream_touched(&self) -> i64 {
        self.stream_touch.load(Ordering::Relaxed)
    }

    /// Estimated memory: the ontology plus the change stream ring.
    pub fn estimated_bytes(&self) -> u64 {
        self.onto.estimated_bytes() + self.changes.bytes() as u64
    }

    pub fn ontology(&self) -> &Ontology {
        &self.onto
    }

    pub fn memory(&self) -> &Memory {
        &self.memory
    }

    /// Recall options for building a question's context: the configured token
    /// budget, and never the episode of the question being answered.
    fn recall_options(&self, exclude_key: Option<String>) -> RecallOptions {
        let budget = self.config.memory.recall_budget_tokens;
        RecallOptions { budget_tokens: (budget > 0).then_some(budget), exclude_key, ..Default::default() }
    }

    pub fn conversations(&self) -> &Conversations {
        &self.conversations
    }

    /// The provider in use now (a project's own, or the server's).
    /// `max_tokens` for a request to `llm`: the provider's own limit (a
    /// project's `max_output_tokens`), else the server's `[llm]` one.
    pub(crate) fn max_output_tokens(&self, llm: &Arc<dyn LlmProvider>) -> u32 {
        llm.max_output_tokens().unwrap_or(self.config.llm.max_output_tokens)
    }

    pub fn llm(&self) -> Arc<dyn LlmProvider> {
        self.llm.read().clone()
    }

    /// Use another provider from the next call on. Cached completions and
    /// answers go (they came from the previous provider); calls in flight
    /// finish on the provider they started with.
    pub fn set_llm(&self, llm: Arc<dyn LlmProvider>) {
        *self.llm.write() = llm;
        self.completions.clear();
        self.answers.clear();
    }

    pub fn limits(&self) -> Limits {
        self.limits.read().clone()
    }

    /// The rate limit and monthly budget from the next call on.
    pub fn set_limits(&self, limits: Limits) {
        *self.limits.write() = limits;
    }

    /// Where usage is recorded, and where the budget check reads the month
    /// so far (`None`: not metered, no budget applies).
    pub fn set_meter(&self, meter: Option<Arc<dyn UsageMeter>>) {
        *self.meter.write() = meter;
    }

    /// One token of the tenant's `asks_per_minute` bucket, or
    /// [`Error::Limited`]: taken by `intelli.ask`, `intelli.ask_stream`,
    /// `ontology.refine` and `ontology.advise`.
    pub fn take_ask(&self) -> Result<()> {
        let rate = self.limits.read().asks_per_minute;
        self.rate.take(rate)
    }

    /// Record one question or LLM-using action with the meter, if any.
    pub(crate) fn record_usage(&self, usage: &TokenUsage, llm: &dyn LlmProvider, deterministic: bool, error: bool) {
        let meter = self.meter.read().clone();
        if let Some(m) = meter {
            m.record(usage, llm.name(), llm.model(), deterministic, error);
        }
    }

    pub fn ingest_queue(&self) -> &IngestQueue {
        &self.ingest
    }

    pub fn ledger(&self) -> LedgerStats {
        let mut s = self.ledger.snapshot();
        s.traces_dropped = self.traces.as_ref().map_or(0, TraceSink::dropped);
        s
    }

    /// A project's trace settings (retention, capture_content) from the next call on.
    pub fn set_trace_settings(&self, s: TraceSettings) {
        *self.trace_settings.write() = s;
    }

    /// Days traces are kept: the project's setting, else `[traces] retention_days`.
    pub fn trace_retention_days(&self) -> u32 {
        self.trace_settings.read().retention_days.unwrap_or(self.config.traces.retention_days).clamp(1, 400)
    }

    /// Whether question, prompt and answer text is stored on traces.
    pub fn captures_content(&self) -> bool {
        self.trace_settings.read().capture_content.unwrap_or(self.config.traces.capture_content)
    }

    /// Are traces recorded?
    pub fn traces_enabled(&self) -> bool {
        self.traces.is_some()
    }

    /// Wait until every trace and score recorded so far is written (tests,
    /// `onto eval`, shutdown).
    pub async fn flush_traces(&self) {
        if let Some(s) = &self.traces {
            s.flush().await;
        }
    }

    /// A tracer for a new trace, when traces are on.
    pub(crate) fn start_trace(&self, ctx: &TraceCtx) -> Option<Arc<Tracer>> {
        self.traces.as_ref()?;
        Some(Arc::new(Tracer::new(ctx, self.captures_content(), self.config.traces.max_content_bytes)))
    }

    /// Queue a finished trace for writing.
    pub(crate) fn send_trace(&self, rec: onto_core::trace::TraceRecord) {
        if let Some(s) = &self.traces {
            s.send(TraceItem::Trace(Box::new(rec)));
        }
    }

    /// Queue a score for writing (after the traces queued before it).
    pub(crate) fn send_score(&self, score: onto_core::trace::ScoreRow) {
        if let Some(s) = &self.traces {
            s.send(TraceItem::Score(Box::new(score)));
        }
    }

    /// Delete traces older than the retention (at most 60 s of work).
    /// Blocking: the retention task runs it on the blocking pool.
    pub fn vacuum_traces(&self) -> Result<u64> {
        let before = onto_core::time::now_ms() - i64::from(self.trace_retention_days()) * 86_400_000;
        self.onto.store().trace_vacuum(before, Duration::from_secs(60))
    }

    /// Remove change records past `[changes] retain_days` and
    /// `retain_max_rows` (whole commits) and raise the floor over them in the
    /// same step, so their cursors read as expired, never as a silent skip.
    /// Blocking: the retention task runs it on the blocking pool.
    pub fn vacuum_changes(&self) -> Result<onto_core::changes::ChangeVacuum> {
        self.onto.store().vacuum_changes(&self.config.changes.retention())
    }

    /// Delete finished outbox rows and attempt logs older than `[webhooks]
    /// retain_days`, and final run rows older than `[automation]
    /// retain_days`, by the store's clock (whatever the delivery and
    /// automation switches of this process are). Returns `(outbox, runs)`.
    /// Blocking: the retention task runs it on the blocking pool.
    pub fn vacuum_outbound(&self) -> Result<(u64, u64)> {
        let store = self.onto.store();
        let now = store.clock_ms()?;
        let outbox = if store.keeps_outbox() {
            store.outbox_vacuum(crate::config::retention_cutoff(now, self.config.webhooks.retain_days))?
        } else {
            0
        };
        let runs = if store.keeps_runs() {
            store.runs_vacuum(crate::config::retention_cutoff(now, self.config.automation.retain_days))?
        } else {
            0
        };
        Ok((outbox, runs))
    }

    pub fn default_workspace(&self) -> &str {
        &self.config.engine.default_workspace
    }

    pub async fn ask(&self, p: &Principal, req: AskRequest) -> Result<AskResponse> {
        self.ask_traced(p, req, TraceCtx::default()).await.result
    }

    /// [`ask`](Self::ask) recorded as a trace with what the caller knows
    /// (`traceparent`, session, source, API key). The response carries
    /// `trace_id`; a refused question (rate limit) is not traced.
    pub async fn ask_traced(&self, p: &Principal, req: AskRequest, ctx: TraceCtx) -> Traced<AskResponse> {
        self.ask_traced_as(p, req, ctx, "ask").await
    }

    /// An ask traced as `kind` (`ask`, or `eval` for evaluation cases).
    pub(crate) async fn ask_traced_as(
        &self,
        p: &Principal,
        req: AskRequest,
        ctx: TraceCtx,
        kind: &'static str,
    ) -> Traced<AskResponse> {
        if let Err(e) = self.take_ask() {
            return Traced::untraced(Err(e));
        }
        let ctx = ctx.merge(req.trace.as_ref());
        let tracer = self.start_trace(&ctx);
        self.ask_in(p, req, ctx, kind, None, tracer).await
    }

    /// Run the question inside `tracer` (when given) and record the trace.
    async fn ask_in(
        &self,
        p: &Principal,
        req: AskRequest,
        ctx: TraceCtx,
        kind: &'static str,
        sink: Option<&mpsc::Sender<AskEvent>>,
        tracer: Option<Arc<Tracer>>,
    ) -> Traced<AskResponse> {
        let Some(tracer) = tracer else { return Traced::untraced(Box::pin(self.ask_inner(p, req, sink)).await) };
        let question = req.question.trim().to_string();
        let ws = req
            .workspace
            .clone()
            .filter(|w| !w.is_empty())
            .unwrap_or_else(|| self.config.engine.default_workspace.clone());
        let conversation = req.conversation_id.clone().filter(|c| !c.is_empty() && c != "new");
        let mut result = trace::scope(tracer.clone(), Box::pin(self.ask_inner(p, req, sink))).await;
        let id = tracer.id().to_string();
        if let Ok(r) = &mut result {
            r.trace_id = Some(id.clone());
        }
        let mut row = trace::base_row(&ctx, p, kind, "intelli.ask", &ws);
        if row.session_id.is_none() {
            row.session_id = result.as_ref().ok().and_then(|r| r.conversation_id.clone()).or(conversation);
        }
        row.input = tracer.text(&question).map(|q| json!({ "question": q }));
        let mut attrs = json!({ "workspace": ws });
        match &result {
            Ok(r) => {
                let mode = serde_json::to_value(r.mode).ok().and_then(|v| v.as_str().map(str::to_string));
                row.request_id = Some(r.request_id.clone());
                row.intent = Some(r.intent.as_str().to_string());
                row.deterministic = r.mode == AnswerMode::Deterministic;
                row.cache_hits = i64::from(r.mode == AnswerMode::AnswerCache);
                row.context_tokens = r.efficiency.context_tokens as i64;
                row.saved_tokens = r.efficiency.saved_tokens as i64;
                row.output = tracer.text(&r.answer).map(|a| json!({ "answer": a }));
                attrs = json!({ "workspace": ws, "intent": r.intent.as_str(), "mode": mode, "revision": r.revision });
                row.mode = mode;
                if let Some(e) = tracer.llm_error() {
                    trace::fail(&mut row, "llm_error", &e);
                }
            }
            Err(e) => trace::fail(&mut row, e.code(), &e.to_string()),
        }
        self.send_trace(tracer.finish(row, "intelli.ask", "span", attrs));
        Traced { result, trace_id: Some(id) }
    }

    /// Streamed answer: stages, resolved entities, text deltas, then `done`.
    /// A refused question (rate limit) is one `error` event.
    pub fn ask_stream(self: &Arc<Self>, p: Principal, req: AskRequest) -> mpsc::Receiver<AskEvent> {
        match self.try_ask_stream(p, req) {
            Ok(rx) => rx,
            Err(e) => {
                let (tx, rx) = mpsc::channel(1);
                let _ = tx.try_send(AskEvent::new("error", e.to_string(), json!({ "code": e.code() })));
                rx
            }
        }
    }

    /// [`ask_stream`](Self::ask_stream), refusing before the stream starts
    /// when the rate limit is reached (HTTP 429 / `RESOURCE_EXHAUSTED`).
    pub fn try_ask_stream(self: &Arc<Self>, p: Principal, req: AskRequest) -> Result<mpsc::Receiver<AskEvent>> {
        self.try_ask_stream_traced(p, req, TraceCtx::default()).map(|(rx, _)| rx)
    }

    /// [`try_ask_stream`](Self::try_ask_stream) recorded as a trace; also
    /// returns the trace's id (known before the stream starts).
    pub fn try_ask_stream_traced(
        self: &Arc<Self>,
        p: Principal,
        req: AskRequest,
        ctx: TraceCtx,
    ) -> Result<(mpsc::Receiver<AskEvent>, Option<String>)> {
        self.take_ask()?;
        let ctx = ctx.merge(req.trace.as_ref());
        let tracer = self.start_trace(&ctx);
        let trace_id = tracer.as_ref().map(|t| t.id().to_string());
        let (tx, rx) = mpsc::channel(128);
        let this = self.clone();
        tokio::spawn(async move {
            // The tracer is the task-local of this task (ask_in scopes it).
            let ev = match this.ask_in(&p, req, ctx, "ask", Some(&tx), tracer).await.result {
                Ok(resp) => AskEvent::new("done", "", serde_json::to_value(&resp).unwrap_or(Value::Null)),
                Err(e) => AskEvent::new("error", e.to_string(), json!({ "code": e.code() })),
            };
            let _ = tx.send(ev).await;
        });
        Ok((rx, trace_id))
    }

    /// The question context graph for `question`, without calling the LLM.
    pub fn build_context(&self, p: &Principal, ws: &str, question: &str, budget: Option<usize>) -> Result<ContextPack> {
        if !p.in_workspace(ws) {
            return Err(Error::Denied(format!("{} has no access to workspace {ws}", p.id)));
        }
        let classified = intent::classify(question, &self.onto.schema());
        let entities = resolve::resolve(&self.onto, p, ws, question);
        let ids: Vec<Id> = entities.iter().map(|r| r.id.clone()).collect();
        let memories = if self.config.memory.enabled {
            self.memory.recall_with(p, ws, question, &ids, self.config.memory.recall_k, &self.recall_options(None))
        } else {
            Vec::new()
        };
        Ok(context::build(
            &self.onto,
            p,
            ws,
            &self.config.context,
            ContextInput {
                question,
                classified: &classified,
                seeds: &entities,
                memories: &memories,
                budget: budget.unwrap_or(self.config.context.token_budget),
            },
        ))
    }

    async fn ask_inner(
        &self,
        p: &Principal,
        req: AskRequest,
        sink: Option<&mpsc::Sender<AskEvent>>,
    ) -> Result<AskResponse> {
        let t0 = Instant::now();
        let question = req.question.trim().to_string();
        if question.is_empty() {
            return Err(Error::invalid("question is required"));
        }
        let ws = req
            .workspace
            .clone()
            .filter(|w| !w.is_empty())
            .unwrap_or_else(|| self.config.engine.default_workspace.clone());
        if !p.in_workspace(&ws) || !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} may not ask questions in workspace {ws}", p.id)));
        }
        let request_id = new_id("ask");
        // One provider for the whole question, even if it is swapped meanwhile.
        let llm = self.llm();
        let schema = self.onto.schema();
        let revision = self.onto.revision();
        let normalized = normalize_question(&question);
        let budget = req.token_budget.unwrap_or(self.config.context.token_budget);
        let mut conv: Option<Conversation> = match req.conversation_id.as_deref() {
            Some(id) if !id.is_empty() => Some(self.conversations.open(p, &ws, Some(id))?),
            _ => None,
        };
        self.ledger.update(|s| s.asks += 1);

        // 1. Whole-answer cache (stateless questions). Keys include the graph and
        //    long-term memory revisions, so any write invalidates naturally.
        let use_cache = self.config.cache.enabled && !req.no_cache && conv.is_none() && !req.include_context;
        let mode_key = format!("{:?}", req.mode);
        let answer_key = stable_hash(&[
            &ws,
            &p.scope_key(),
            &normalized,
            &mode_key,
            &revision.to_string(),
            &self.memory.revision_for(&ws).to_string(),
            &budget.to_string(),
        ]);
        let cached_answer = if use_cache { self.answers.get(&answer_key) } else { None };
        trace::event(
            "cache.answer",
            || json!({ "result": if !use_cache { "off" } else if cached_answer.is_some() { "hit" } else { "miss" } }),
        );
        if let Some(mut hit) = cached_answer {
            let saved = hit.usage.total() + hit.efficiency.context_tokens as u64;
            self.ledger.update(|s| {
                s.answer_cache_hits += 1;
                s.tokens_saved += saved;
            });
            hit.mode = AnswerMode::AnswerCache;
            hit.request_id = request_id;
            hit.usage = TokenUsage::default();
            hit.timings = Timings { total_ms: ms(t0), ..Default::default() };
            emit(sink, AskEvent::new("delta", hit.answer.clone(), Value::Null)).await;
            self.record_usage(&TokenUsage::default(), llm.as_ref(), false, false);
            return Ok(hit);
        }

        // 2. Intent + entity resolution (deterministic, cached per revision).
        let t_int = Instant::now();
        let classified = intent::classify(&question, &schema);
        trace::record(
            "intent",
            "span",
            t_int,
            || json!({ "intent": classified.intent, "why": classified.why, "action": classified.action }),
        );
        emit(sink, AskEvent::new("stage", "intent", json!({ "intent": classified.intent, "why": classified.why })))
            .await;
        let t_res = Instant::now();
        let res_key = stable_hash(&[&ws, &p.scope_key(), &normalized, &revision.to_string()]);
        let resolution_hit = self.resolutions.get(&res_key);
        let resolution_cache = if resolution_hit.is_some() { "hit" } else { "miss" };
        let mut entities = match resolution_hit {
            Some(r) => r,
            None => {
                let r = resolve::resolve(&self.onto, p, &ws, &question);
                self.resolutions.put(res_key, r.clone());
                r
            }
        };
        if entities.is_empty()
            && let Some(c) = &conv
        {
            entities = self
                .onto
                .get_many(p, &c.last_entities())?
                .into_iter()
                .map(|e| Resolved {
                    title: e.label().to_string(),
                    id: e.id,
                    kind: e.kind,
                    mention: "(previous turn)".into(),
                    confidence: 0.85,
                    position: 0,
                })
                .collect();
        }
        let resolve_ms = ms(t_res);
        trace::record("resolve", "span", t_res, || json!({ "entities": entities.len(), "cache": resolution_cache }));
        emit(sink, AskEvent::new("entities", "", serde_json::to_value(&entities).unwrap_or(Value::Null))).await;

        let mut usage = TokenUsage::default();
        let mut efficiency = Efficiency::default();
        let mut memories_used = Vec::new();
        let mut context_out = None;
        let mut note = None;
        let mut retrieve_ms = 0.0;
        let mut llm_ms = 0.0;
        // Whether the LLM step ran, and whether its provider call failed.
        let mut llm_step = false;
        let mut llm_error = false;

        // 3. Deterministic answer straight from the graph?
        let deterministic_allowed = match req.mode {
            AskMode::Llm => false,
            AskMode::Deterministic => true,
            AskMode::Auto => {
                !classified.why
                    && self.config.reasoning.deterministic_intents.iter().any(|i| i == classified.intent.as_str())
            }
        };
        // 3a. Commands become governed action proposals (applied only when confirmed);
        //     ambiguous mentions get a clarifying question.
        let mut options: Vec<Resolved> = Vec::new();
        let mut action_out: Option<Value> = None;
        let t_pre = Instant::now();
        let pre: Option<(String, Vec<String>, AnswerMode)> = if classified.intent == Intent::Command
            && let Some(action) = classified.action.clone()
        {
            let out = crate::command::run(&self.onto, p, &ws, &action, &question, &entities, req.confirm)?;
            let mode = if out.receipt.status == "applied" { AnswerMode::Action } else { AnswerMode::Proposal };
            action_out = serde_json::to_value(&out.receipt).ok();
            trace::record(
                "command",
                "span",
                t_pre,
                || json!({ "action": action, "status": out.receipt.status, "receipt": out.receipt.id }),
            );
            emit(sink, AskEvent::new("delta", out.answer.clone(), Value::Null)).await;
            Some((out.answer, out.facts, mode))
        } else if deterministic_allowed && let Some((q, group)) = self.clarification(&entities) {
            options = group;
            trace::record("clarify", "span", t_pre, || json!({ "options": options.len() }));
            emit(sink, AskEvent::new("delta", q.clone(), Value::Null)).await;
            Some((q, Vec::new(), AnswerMode::Clarify))
        } else {
            None
        };
        let t_det = Instant::now();
        let direct = if pre.is_none() && deterministic_allowed {
            answer::try_answer(
                &self.onto,
                &question,
                p,
                &ws,
                &classified,
                &entities,
                self.config.reasoning.min_confidence,
            )
        } else {
            None
        };

        let (answer_text, facts, mode) = if let Some(x) = pre {
            x
        } else if let Some(d) = direct {
            trace::record("deterministic", "span", t_det, || json!({ "answered": true, "facts": d.facts.len() }));
            let avoided = budget.min(600) + estimate(&self.system_prompt());
            efficiency.saved_tokens = avoided;
            self.ledger.update(|s| {
                s.deterministic_answers += 1;
                s.tokens_saved += avoided as u64;
            });
            emit(sink, AskEvent::new("delta", d.answer.clone(), Value::Null)).await;
            (d.answer, d.facts, AnswerMode::Deterministic)
        } else if req.mode == AskMode::Deterministic {
            let hits = self.onto.search(p, &question, &[], Some(&ws), 5).unwrap_or_default();
            let msg = if hits.is_empty() {
                "I can't answer that directly from the graph, and found nothing related.".to_string()
            } else {
                let list: Vec<String> =
                    hits.iter().map(|h| format!("{} ({})", h.entity.label(), h.entity.id)).collect();
                format!("I can't answer that directly from the graph. Possibly relevant: {}.", list.join(", "))
            };
            trace::record("deterministic", "span", t_det, || json!({ "answered": false, "related": hits.len() }));
            emit(sink, AskEvent::new("delta", msg.clone(), Value::Null)).await;
            (msg, Vec::new(), AnswerMode::Fallback)
        } else {
            // 4. Build the question context graph and pack it into the budget.
            let t_ret = Instant::now();
            let ids: Vec<Id> = entities.iter().map(|r| r.id.clone()).collect();
            // An earlier answer to this very question is not evidence (caches cover repeats).
            let memories: Vec<_> = if self.config.memory.enabled {
                let opts = self.recall_options(Some(normalized.clone()));
                self.memory
                    .recall_with(p, &ws, &question, &ids, self.config.memory.recall_k + 2, &opts)
                    .into_iter()
                    .filter(|m| m.item.key.as_deref() != Some(normalized.as_str()))
                    .take(self.config.memory.recall_k)
                    .collect()
            } else {
                Vec::new()
            };
            memories_used = memories.iter().map(|m| clip(&m.item.text, 160)).collect();
            let pack = context::build(
                &self.onto,
                p,
                &ws,
                &self.config.context,
                ContextInput {
                    question: &question,
                    classified: &classified,
                    seeds: &entities,
                    memories: &memories,
                    budget,
                },
            );
            retrieve_ms = ms(t_ret);
            efficiency = Efficiency {
                context_tokens: pack.tokens,
                naive_tokens: pack.naive_tokens,
                saved_tokens: pack.naive_tokens.saturating_sub(pack.tokens),
                facts_used: pack.facts.len(),
                facts_dropped: pack.dropped_facts,
            };
            // Facts are never stored on traces, only their counts.
            trace::record("retrieve", "retrieval", t_ret, || {
                json!({
                    "memories": memories.len(),
                    "facts_used": efficiency.facts_used,
                    "facts_dropped": efficiency.facts_dropped,
                    "context_tokens": efficiency.context_tokens,
                    "naive_tokens": efficiency.naive_tokens,
                    "saved_tokens": efficiency.saved_tokens,
                })
            });
            self.ledger.update(|s| s.tokens_saved += efficiency.saved_tokens as u64);
            emit(
                sink,
                AskEvent::new(
                    "stage",
                    "context",
                    json!({ "facts": pack.facts.len(), "tokens": pack.tokens, "dropped": pack.dropped_facts }),
                ),
            )
            .await;
            let facts: Vec<String> = pack.facts.iter().map(|f| f.text.clone()).collect();

            // 5. Prompt: stable system prefix (cache-friendly) + history + facts + question.
            let (summary, mut messages) =
                conv.as_ref().map(|c| c.window(self.config.context.history_turns)).unwrap_or_default();
            let mut user = String::new();
            if !summary.is_empty() {
                user.push_str("Earlier in this conversation:");
                user.push_str(&summary);
                user.push_str("\n\n");
            }
            user.push_str("Facts:\n");
            user.push_str(if pack.text.is_empty() { "- (no matching workspace facts)" } else { &pack.text });
            user.push_str("\n\nQuestion: ");
            user.push_str(&question);
            messages.push(ChatMessage::user(user));
            let llm_req =
                LlmRequest { system: self.system_prompt(), messages, max_tokens: self.max_output_tokens(&llm) };
            if req.include_context {
                context_out = Some(pack);
            }

            let t_llm = Instant::now();
            llm_step = true;
            let result = match llm.unavailable() {
                Some(reason) => Err(Error::Unavailable(reason)),
                None => self.complete(&llm, &llm_req, sink, req.no_cache).await,
            };
            llm_ms = ms(t_llm);
            match result {
                Ok((resp, cached)) => {
                    if cached {
                        self.ledger.update(|s| {
                            s.llm_cache_hits += 1;
                            s.tokens_saved += resp.usage.total();
                        });
                    } else {
                        usage = resp.usage.clone();
                    }
                    (resp.text, facts, if cached { AnswerMode::LlmCache } else { AnswerMode::Llm })
                }
                Err(Error::Limited(budget)) => {
                    // The month's budget is spent: no provider call, the facts instead.
                    note = Some(budget.clone());
                    let fallback = if facts.is_empty() {
                        format!("I couldn't find workspace facts for this question ({budget}).")
                    } else {
                        let top: Vec<&str> = facts.iter().take(6).map(String::as_str).collect();
                        format!("The {budget}. Most relevant facts:\n• {}", top.join("\n• "))
                    };
                    emit(sink, AskEvent::new("delta", fallback.clone(), Value::Null)).await;
                    (fallback, facts, AnswerMode::Fallback)
                }
                Err(e) => {
                    self.ledger.update(|s| s.llm_failures += 1);
                    llm_error = true;
                    note = Some(e.to_string());
                    let fallback = if facts.is_empty() {
                        format!("I couldn't find workspace facts for this question ({e}).")
                    } else {
                        let top: Vec<&str> = facts.iter().take(6).map(String::as_str).collect();
                        format!("The language model is unavailable ({e}). Most relevant facts:\n• {}", top.join("\n• "))
                    };
                    emit(sink, AskEvent::new("delta", fallback.clone(), Value::Null)).await;
                    (fallback, facts, AnswerMode::Fallback)
                }
            }
        };

        // 6. Remember: conversation, graph links, episodic memory, audit trail.
        let t_rem = Instant::now();
        let entity_ids: Vec<Id> = entities.iter().map(|r| r.id.clone()).collect();
        let mut conversation_id = None;
        if let Some(c) = conv.as_mut() {
            let first_turn = c.turns.is_empty();
            let new_topics = self.conversations.append(
                c,
                &question,
                &answer_text,
                &entity_ids,
                self.config.context.history_turns,
            )?;
            conversation_id = Some(c.id.clone());
            if first_turn || !new_topics.is_empty() {
                self.link_conversation(c, &new_topics);
            }
        }
        let episode = self.config.memory.enabled
            && matches!(mode, AnswerMode::Deterministic | AnswerMode::Llm | AnswerMode::LlmCache | AnswerMode::Action);
        if episode {
            let salience = if entity_ids.is_empty() { 0.3 } else { 0.5 };
            let _ = self.memory.record_episode(
                p,
                &ws,
                conversation_id.as_deref(),
                &question,
                &answer_text,
                &entity_ids,
                salience,
            );
        }
        let audit = EventInput {
            action: "intelli.ask".into(),
            actor: Some(p.id.clone()),
            target: entity_ids.first().cloned(),
            workspace: Some(ws.clone()),
            outcome: if mode == AnswerMode::Fallback { Outcome::Failure } else { Outcome::Success },
            source: Some("orchestrator".into()),
            payload: json!({
                "question": clip(&question, 200),
                "mode": mode,
                "intent": classified.intent,
                "prompt_tokens": usage.prompt_tokens,
                "completion_tokens": usage.completion_tokens,
                "cost_usd": usage.cost_usd,
            }),
            ..Default::default()
        };
        let ctx = ActionContext::system(&ws).with_request_id(&request_id).with_intent(classified.intent.as_str());
        let audited = match self.onto.apply(&ctx, vec![Mutation::Event(audit)]) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!("audit event failed: {e}");
                false
            }
        };
        trace::record(
            "remember",
            "span",
            t_rem,
            || json!({ "conversation_turn": conversation_id.is_some(), "episode": episode, "audit": audited }),
        );
        let n = self.asks.fetch_add(1, Ordering::Relaxed) + 1;
        if self.config.memory.consolidate_every > 0 && n.is_multiple_of(self.config.memory.consolidate_every) {
            // Off the ask path; a pass already running makes this one a no-op.
            let m = self.memory.clone();
            let run = move || {
                if let Err(e) = m.consolidate() {
                    tracing::warn!("memory consolidation failed: {e}");
                }
            };
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => drop(rt.spawn_blocking(run)),
                Err(_) => run(),
            }
        }

        let resp = AskResponse {
            answer: answer_text,
            mode,
            intent: classified.intent,
            why: classified.why,
            entities,
            facts,
            conversation_id,
            usage,
            efficiency,
            timings: Timings { resolve_ms, retrieve_ms, llm_ms, total_ms: ms(t0) },
            provider: llm.name().to_string(),
            model: llm.model().to_string(),
            request_id,
            revision,
            memories: memories_used,
            context: context_out,
            options,
            action: action_out,
            note,
            trace_id: None,
        };
        self.record_usage(&resp.usage, llm.as_ref(), !llm_step, llm_error);
        if use_cache && matches!(mode, AnswerMode::Deterministic | AnswerMode::Llm | AnswerMode::LlmCache) {
            self.answers.put(answer_key, resp.clone());
        }
        Ok(resp)
    }

    /// LLM call on `llm` with the completion cache (memory, then store). A
    /// cached completion is served whatever the budget; otherwise, at or over
    /// the monthly budget the provider is not called and the result is
    /// [`Error::Limited`] with the reason ("monthly LLM budget of $50.00
    /// reached").
    pub(crate) async fn complete(
        &self,
        llm: &Arc<dyn LlmProvider>,
        req: &LlmRequest,
        sink: Option<&mpsc::Sender<AskEvent>>,
        no_cache: bool,
    ) -> Result<(LlmResponse, bool)> {
        let tracer = trace::current();
        let started = Instant::now();
        let mut ttft = None;
        let result = self.complete_inner(llm, req, sink, no_cache, started, &mut ttft).await;
        if let Some(t) = tracer {
            let none = TokenUsage::default();
            let cache_on = self.config.cache.enabled && !no_cache;
            let (usage, cache, resp) = match &result {
                Ok((r, true)) => (&none, "hit", Some(r)),
                Ok((r, false)) => (&r.usage, if cache_on { "miss" } else { "off" }, Some(r)),
                Err(_) => (&none, if cache_on { "miss" } else { "off" }, None),
            };
            t.generation(Generation {
                provider: llm.name(),
                model: llm.model(),
                request: req,
                started,
                ttft_ms: ttft,
                cache,
                usage,
                response_model: resp.map(|r| r.model.as_str()),
                finish_reason: resp.and_then(|r| r.finish_reason.as_deref()),
                text: resp.map(|r| r.text.as_str()),
                budget_exceeded: matches!(result, Err(Error::Limited(_))),
                error: match &result {
                    Err(Error::Limited(_)) | Ok(_) => None,
                    Err(e) => Some(e.to_string()),
                },
            });
        }
        result
    }

    async fn complete_inner(
        &self,
        llm: &Arc<dyn LlmProvider>,
        req: &LlmRequest,
        sink: Option<&mpsc::Sender<AskEvent>>,
        no_cache: bool,
        started: Instant,
        ttft: &mut Option<f64>,
    ) -> Result<(LlmResponse, bool)> {
        let key = req.cache_key(llm.name(), llm.model());
        let cache_on = self.config.cache.enabled && !no_cache;
        if cache_on {
            let mut hit = self.completions.get(&key);
            if hit.is_none() && self.config.cache.persist_llm {
                hit = self
                    .onto
                    .store()
                    .kv_get("llm_cache", &key)
                    .ok()
                    .flatten()
                    .and_then(|v| serde_json::from_value::<LlmResponse>(v).ok());
                if let Some(h) = &hit {
                    self.completions.put(key.clone(), h.clone());
                }
            }
            if let Some(h) = hit {
                emit(sink, AskEvent::new("delta", h.text.clone(), Value::Null)).await;
                return Ok((h, true));
            }
        }
        let meter = self.meter.read().clone();
        if let Some(m) = meter
            && let Some(reason) = self.limits.read().budget_reached(&m.month_to_date())
        {
            return Err(Error::Limited(reason));
        }
        let resp = match sink {
            Some(tx) => {
                let mut stream = llm.stream(req).await?;
                let mut text = String::new();
                let mut done = None;
                while let Some(chunk) = stream.next().await {
                    match chunk? {
                        StreamChunk::Delta(d) => {
                            if ttft.is_none() {
                                *ttft = Some(ms(started));
                            }
                            text.push_str(&d);
                            let _ = tx.send(AskEvent::new("delta", d, Value::Null)).await;
                        }
                        StreamChunk::Done(r) => done = Some(r),
                    }
                }
                done.unwrap_or(LlmResponse { text, model: llm.model().into(), ..Default::default() })
            }
            None => llm.complete(req).await?,
        };
        self.ledger.record_llm(&resp.usage);
        if cache_on && !resp.text.trim().is_empty() {
            self.completions.put(key.clone(), resp.clone());
            if self.config.cache.persist_llm {
                let expires = onto_core::time::now_ms() + self.config.cache.llm_ttl_secs as i64 * 1000;
                if let Ok(v) = serde_json::to_value(&resp) {
                    let _ = self.onto.store().kv_put("llm_cache", &key, &v, Some(expires));
                }
            }
        }
        Ok((resp, false))
    }

    /// The conversation thread becomes a Thing linked `about` its topics.
    fn link_conversation(&self, c: &Conversation, new_topics: &[Id]) {
        let schema = self.onto.schema();
        if self.config.ontology.strict && schema.object_type("conversation").is_none() {
            return;
        }
        let conv_id = format!("conversation:{}", c.id);
        let title = c.turns.first().map(|t| clip(&t.text, 80)).unwrap_or_default();
        let mut muts = vec![Mutation::Upsert(EntityInput {
            id: Some(conv_id.clone()),
            kind: "conversation".into(),
            title: Some(title),
            props: serde_json::from_value(json!({ "channel": "intelli", "started_by": c.principal }))
                .unwrap_or_default(),
            ..Default::default()
        })];
        for t in new_topics {
            muts.push(Mutation::Link(LinkInput::new(conv_id.clone(), "about", t.clone())));
        }
        let ctx = ActionContext::system(&c.workspace).with_source("conversation");
        if let Err(e) = self.onto.apply(&ctx, muts) {
            tracing::debug!("conversation link skipped: {e}");
        }
    }

    pub fn stats(&self) -> Result<Value> {
        let llm = self.llm();
        Ok(json!({
            "version": crate::VERSION,
            "uptime_secs": self.started.elapsed().as_secs(),
            "ontology": self.onto.stats()?,
            "estimated_bytes": self.estimated_bytes(),
            "changes": {
                "subscribers": self.changes.subscribers(),
                "subscribers_process": ChangeHub::process_subscribers(),
                "ring_records": self.changes.ring_len(),
                "ring_bytes": self.changes.bytes(),
            },
            "memory": self.memory.stats(),
            "llm": {
                "provider": llm.name(),
                "model": llm.model(),
                "available": llm.unavailable().is_none(),
                "unavailable_reason": llm.unavailable(),
            },
            "outbound": self.outbound_stats(),
            "workers": self.worker_rounds(),
            "ledger": self.ledger(),
            "caches": {
                "answers": self.answers.stats(),
                "resolutions": self.resolutions.stats(),
                "completions": self.completions.stats(),
            },
            "ingest": self.ingest.stats(),
        }))
    }
}

/// Delete old traces 5 minutes after the tenant opens, then hourly, for as
/// long as it is open.
fn spawn_trace_retention(me: &Arc<Intelli>) {
    let weak = Arc::downgrade(me);
    tokio::spawn(async move {
        let mut wait = Duration::from_secs(300);
        loop {
            tokio::time::sleep(wait).await;
            wait = Duration::from_secs(3600);
            let Some(me) = weak.upgrade() else { break };
            match tokio::task::spawn_blocking(move || me.vacuum_traces()).await {
                Ok(Ok(n)) if n > 0 => tracing::info!(rows = n, "trace retention"),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!("trace retention: {e}"),
                Err(e) => tracing::warn!("trace retention: {e}"),
            }
        }
    });
}

/// Trim the change feed ([`Intelli::vacuum_changes`]), the outbox and the
/// run rows ([`Intelli::vacuum_outbound`]) 5 minutes after the tenant opens,
/// then hourly, for as long as it is open (a tenant that idles out before an
/// hour still gets its pass). Every process with the tenant open runs it; a
/// change pass is one transaction that locks the floor, so passes from
/// several replicas only repeat the same trim.
fn spawn_retention(me: &Arc<Intelli>) {
    let weak = Arc::downgrade(me);
    tokio::spawn(async move {
        let mut wait = Duration::from_secs(300);
        loop {
            tokio::time::sleep(wait).await;
            wait = Duration::from_secs(3600);
            let Some(me) = weak.upgrade() else { break };
            let me2 = me.clone();
            if me.onto.store().keeps_changes() {
                match tokio::task::spawn_blocking(move || me2.vacuum_changes()).await {
                    Ok(Ok(v)) if v.deleted > 0 => {
                        tracing::info!(rows = v.deleted, floor = v.floor, "change retention")
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::warn!("change retention: {e}"),
                    Err(e) => tracing::warn!("change retention: {e}"),
                }
            }
            match tokio::task::spawn_blocking(move || me.vacuum_outbound()).await {
                Ok(Ok((outbox, runs))) if outbox + runs > 0 => tracing::info!(outbox, runs, "outbox and run retention"),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!("outbox and run retention: {e}"),
                Err(e) => tracing::warn!("outbox and run retention: {e}"),
            }
        }
    });
}

/// Built-in or file schema plus `ontology.extend` layers, as configured.
pub fn load_schema(cfg: &Config) -> Result<Schema> {
    let src = cfg.ontology.schema.as_str();
    let mut schema = if src.starts_with("builtin:") || matches!(src, "enterprise" | "empty") {
        Schema::load(src)?
    } else {
        Schema::from_file(cfg.resolve_path(src))?
    };
    for ext in &cfg.ontology.extend {
        schema.extend_source(ext, |p| cfg.resolve_path(p))?;
    }
    Ok(schema)
}

fn open_store(cfg: &Config) -> Result<Arc<dyn Store>> {
    match cfg.engine.storage.as_str() {
        "memory" => Ok(Arc::new(MemoryStore::default())),
        #[cfg(feature = "postgres")]
        "postgres" | "postgresql" => {
            let pg = &cfg.engine.postgres;
            let opts = onto_core::store::PgOptions {
                schema_prefix: pg.schema_prefix.clone(),
                max_connections: pg.max_connections,
                connect_timeout: Duration::from_secs(pg.connect_timeout_secs),
                listen: true,
            };
            Ok(Arc::new(onto_core::store::PgStore::connect(&cfg.engine.resolve_database_url()?, &pg.tenant, opts)?))
        }
        other => Err(Error::Config(format!("unknown engine.storage '{other}' (postgres, memory)"))),
    }
}
