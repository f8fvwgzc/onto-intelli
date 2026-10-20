//! The orchestrator: deterministic retrieval first, the LLM only for reasoning.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use onto_core::store::{MemoryStore, Store};
use onto_core::time::new_id;
use onto_core::{
    Access, ActionContext, EntityInput, Error, EventInput, Id, LinkInput, Mutation, Ontology, OntologyOptions, Outcome,
    Principal, Result, Schema,
};

use crate::answer;
use crate::cache::TtlCache;
use crate::config::Config;
use crate::context::{self, ContextInput, ContextPack};
use crate::conversation::{Conversation, Conversations};
use crate::hash::{normalize_question, stable_hash};
use crate::ingest::IngestQueue;
use crate::intent::{self, Intent};
use crate::llm::{self, ChatMessage, LlmProvider, LlmRequest, LlmResponse, StreamChunk};
use crate::memory::{Memory, clip};
use crate::resolve::{self, Resolved};
use crate::token::{Ledger, LedgerStats, TokenUsage, estimate};

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
    llm: Arc<dyn LlmProvider>,
    prompt: parking_lot::Mutex<(u64, Instant, String)>,
    answers: TtlCache<AskResponse>,
    resolutions: TtlCache<Vec<Resolved>>,
    completions: TtlCache<LlmResponse>,
    ledger: Ledger,
    ingest: IngestQueue,
    asks: AtomicUsize,
    started: Instant,
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
            },
        )?;
        let memory = Memory::open(store.clone(), config.memory.clone())?;
        let conversations = Conversations::new(store);
        let c = &config.cache;
        let prompt = parking_lot::Mutex::new((0, Instant::now(), String::new()));
        let crawl = config.connectors.clone();
        let me = Arc::new(Self {
            answers: TtlCache::new(c.max_entries, Duration::from_secs(c.answer_ttl_secs)),
            resolutions: TtlCache::new(c.max_entries, Duration::from_secs(c.answer_ttl_secs)),
            completions: TtlCache::new(c.max_entries, Duration::from_secs(c.llm_ttl_secs)),
            ingest: IngestQueue::spawn(onto.clone(), config.engine.ingest_queue),
            ledger: Ledger::default(),
            asks: AtomicUsize::new(0),
            started: Instant::now(),
            prompt,
            conversations,
            memory,
            onto,
            llm,
            config,
        });
        if crawl.background {
            crate::connectors::spawn_scheduler(&me, Duration::from_secs(crawl.tick_secs.max(1)));
        }
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

    pub fn ontology(&self) -> &Ontology {
        &self.onto
    }

    pub fn memory(&self) -> &Memory {
        &self.memory
    }

    pub fn conversations(&self) -> &Conversations {
        &self.conversations
    }

    pub fn llm(&self) -> &Arc<dyn LlmProvider> {
        &self.llm
    }

    pub fn ingest_queue(&self) -> &IngestQueue {
        &self.ingest
    }

    pub fn ledger(&self) -> LedgerStats {
        self.ledger.snapshot()
    }

    pub fn default_workspace(&self) -> &str {
        &self.config.engine.default_workspace
    }

    pub async fn ask(&self, p: &Principal, req: AskRequest) -> Result<AskResponse> {
        self.ask_inner(p, req, None).await
    }

    /// Streamed answer: stages, resolved entities, text deltas, then `done`.
    pub fn ask_stream(self: &Arc<Self>, p: Principal, req: AskRequest) -> mpsc::Receiver<AskEvent> {
        let (tx, rx) = mpsc::channel(128);
        let this = self.clone();
        tokio::spawn(async move {
            let ev = match this.ask_inner(&p, req, Some(&tx)).await {
                Ok(resp) => AskEvent::new("done", "", serde_json::to_value(&resp).unwrap_or(Value::Null)),
                Err(e) => AskEvent::new("error", e.to_string(), json!({ "code": e.code() })),
            };
            let _ = tx.send(ev).await;
        });
        rx
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
            self.memory.recall(p, ws, question, &ids, self.config.memory.recall_k)
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
            &self.memory.revision().to_string(),
            &budget.to_string(),
        ]);
        if use_cache && let Some(mut hit) = self.answers.get(&answer_key) {
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
            return Ok(hit);
        }

        // 2. Intent + entity resolution (deterministic, cached per revision).
        let classified = intent::classify(&question, &schema);
        emit(sink, AskEvent::new("stage", "intent", json!({ "intent": classified.intent, "why": classified.why })))
            .await;
        let t_res = Instant::now();
        let res_key = stable_hash(&[&ws, &p.scope_key(), &normalized, &revision.to_string()]);
        let mut entities = match self.resolutions.get(&res_key) {
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
        emit(sink, AskEvent::new("entities", "", serde_json::to_value(&entities).unwrap_or(Value::Null))).await;

        let mut usage = TokenUsage::default();
        let mut efficiency = Efficiency::default();
        let mut memories_used = Vec::new();
        let mut context_out = None;
        let mut note = None;
        let mut retrieve_ms = 0.0;
        let mut llm_ms = 0.0;

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
        let pre: Option<(String, Vec<String>, AnswerMode)> = if classified.intent == Intent::Command
            && let Some(action) = classified.action.clone()
        {
            let out = crate::command::run(&self.onto, p, &ws, &action, &question, &entities, req.confirm)?;
            let mode = if out.receipt.status == "applied" { AnswerMode::Action } else { AnswerMode::Proposal };
            action_out = serde_json::to_value(&out.receipt).ok();
            emit(sink, AskEvent::new("delta", out.answer.clone(), Value::Null)).await;
            Some((out.answer, out.facts, mode))
        } else if deterministic_allowed && let Some((q, group)) = self.clarification(&entities) {
            options = group;
            emit(sink, AskEvent::new("delta", q.clone(), Value::Null)).await;
            Some((q, Vec::new(), AnswerMode::Clarify))
        } else {
            None
        };
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
            emit(sink, AskEvent::new("delta", msg.clone(), Value::Null)).await;
            (msg, Vec::new(), AnswerMode::Fallback)
        } else {
            // 4. Build the question context graph and pack it into the budget.
            let t_ret = Instant::now();
            let ids: Vec<Id> = entities.iter().map(|r| r.id.clone()).collect();
            // An earlier answer to this very question is not evidence (caches cover repeats).
            let memories: Vec<_> = if self.config.memory.enabled {
                self.memory
                    .recall(p, &ws, &question, &ids, self.config.memory.recall_k + 2)
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
                LlmRequest { system: self.system_prompt(), messages, max_tokens: self.config.llm.max_output_tokens };
            if req.include_context {
                context_out = Some(pack);
            }

            let t_llm = Instant::now();
            let result = match self.llm.unavailable() {
                Some(reason) => Err(Error::Unavailable(reason)),
                None => self.complete(&llm_req, sink, req.no_cache).await,
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
                Err(e) => {
                    self.ledger.update(|s| s.llm_failures += 1);
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
        if self.config.memory.enabled
            && matches!(mode, AnswerMode::Deterministic | AnswerMode::Llm | AnswerMode::LlmCache | AnswerMode::Action)
        {
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
        if let Err(e) = self.onto.apply(&ctx, vec![Mutation::Event(audit)]) {
            tracing::warn!("audit event failed: {e}");
        }
        let n = self.asks.fetch_add(1, Ordering::Relaxed) + 1;
        if self.config.memory.consolidate_every > 0 && n.is_multiple_of(self.config.memory.consolidate_every) {
            let _ = self.memory.consolidate();
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
            provider: self.llm.name().to_string(),
            model: self.llm.model().to_string(),
            request_id,
            revision,
            memories: memories_used,
            context: context_out,
            options,
            action: action_out,
            note,
        };
        if use_cache && matches!(mode, AnswerMode::Deterministic | AnswerMode::Llm | AnswerMode::LlmCache) {
            self.answers.put(answer_key, resp.clone());
        }
        Ok(resp)
    }

    /// LLM call with the completion cache (memory, then store).
    async fn complete(
        &self,
        req: &LlmRequest,
        sink: Option<&mpsc::Sender<AskEvent>>,
        no_cache: bool,
    ) -> Result<(LlmResponse, bool)> {
        let key = req.cache_key(self.llm.name(), self.llm.model());
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
        let resp = match sink {
            Some(tx) => {
                let mut stream = self.llm.stream(req).await?;
                let mut text = String::new();
                let mut done = None;
                while let Some(chunk) = stream.next().await {
                    match chunk? {
                        StreamChunk::Delta(d) => {
                            text.push_str(&d);
                            let _ = tx.send(AskEvent::new("delta", d, Value::Null)).await;
                        }
                        StreamChunk::Done(r) => done = Some(r),
                    }
                }
                done.unwrap_or(LlmResponse { text, model: self.llm.model().into(), ..Default::default() })
            }
            None => self.llm.complete(req).await?,
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
        Ok(json!({
            "version": crate::VERSION,
            "uptime_secs": self.started.elapsed().as_secs(),
            "ontology": self.onto.stats()?,
            "memory": self.memory.stats(),
            "llm": {
                "provider": self.llm.name(),
                "model": self.llm.model(),
                "available": self.llm.unavailable().is_none(),
                "unavailable_reason": self.llm.unavailable(),
            },
            "ledger": self.ledger.snapshot(),
            "caches": {
                "answers": self.answers.stats(),
                "resolutions": self.resolutions.stats(),
                "completions": self.completions.stats(),
            },
            "ingest": self.ingest.stats(),
        }))
    }
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
