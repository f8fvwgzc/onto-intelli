//! Traces of the orchestrator (docs/CONTROL_PLANE.md section 9) on the demo
//! workspace, in memory: the spans of one ask with their token counts;
//! deterministic, cached, LLM, fallback, error and streaming asks; action
//! traces; nested generation spans of `ontology.refine`; `traceparent`; the
//! session id from the caller, the request and the conversation;
//! `capture_content = false`; `[traces] calls`; eval scores; the trace
//! actions and their access rules; retention; the dropped counter.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use onto_intelli::core::trace::{TraceDetail, TraceQuery, TraceRecord, TraceRow};
use onto_intelli::llm::{LlmProvider, LlmRequest, LlmResponse};
use onto_intelli::token::TokenUsage;
use onto_intelli::trace::{AskTrace, TraceCtx, TraceSettings};
use onto_intelli::{AnswerMode, AskMode, AskRequest, Config, Error, Intelli, Principal, Result, Role, seed};

fn config() -> Config {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    cfg
}

async fn demo_with(tweak: impl FnOnce(&mut Config), llm: Option<Arc<dyn LlmProvider>>) -> Arc<Intelli> {
    let mut cfg = config();
    tweak(&mut cfg);
    let i = match llm {
        Some(l) => Intelli::open_with(cfg, l).await.unwrap(),
        None => Intelli::open(cfg).await.unwrap(),
    };
    seed::load_demo(&i, "acme").await.unwrap();
    i
}

async fn demo() -> Arc<Intelli> {
    demo_with(|_| {}, Some(Arc::new(Scripted::new("Alpha is blocked by issue:421 [issue:421].")))).await
}

fn admin() -> Principal {
    Principal::admin("lead")
}

/// A model with a fixed reply and fixed usage.
struct Scripted {
    reply: String,
    fail: bool,
}

impl Scripted {
    fn new(reply: &str) -> Self {
        Scripted { reply: reply.into(), fail: false }
    }
}

#[async_trait]
impl LlmProvider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }

    fn model(&self) -> &str {
        "scripted-1"
    }

    async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse> {
        if self.fail {
            return Err(Error::Llm("upstream 503: overloaded".into()));
        }
        Ok(LlmResponse {
            text: self.reply.clone(),
            usage: TokenUsage {
                prompt_tokens: 120,
                completion_tokens: 30,
                cached_tokens: 20,
                cost_usd: 0.0015,
                estimated: false,
            },
            model: "scripted-1-2026".into(),
            finish_reason: Some("stop".into()),
        })
    }
}

async fn get(i: &Intelli, id: &str) -> TraceDetail {
    i.flush_traces().await;
    i.ontology().store().trace_get(id).unwrap().unwrap_or_else(|| panic!("trace {id} was not written"))
}

fn span_names(d: &TraceDetail) -> Vec<&str> {
    d.spans.iter().map(|s| s.name.as_str()).collect()
}

fn span<'a>(d: &'a TraceDetail, name: &str) -> &'a onto_intelli::core::trace::SpanRow {
    d.spans.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no {name} span in {:?}", span_names(d)))
}

async fn traces(i: &Intelli) -> Vec<TraceRow> {
    i.flush_traces().await;
    i.ontology().store().trace_list(&TraceQuery { limit: Some(200), ..Default::default() }).unwrap().traces
}

#[tokio::test]
async fn one_llm_ask_has_the_documented_spans_and_tokens() {
    let i = demo().await;
    let req = AskRequest { question: "Why is Project Alpha blocked?".into(), mode: AskMode::Llm, ..Default::default() };
    let r = i.ask(&admin(), req).await.unwrap();
    assert_eq!(r.mode, AnswerMode::Llm);
    let id = r.trace_id.clone().expect("the answer names its trace");
    assert_eq!(id.len(), 32);
    let d = get(&i, &id).await;
    assert_eq!(
        span_names(&d),
        ["intelli.ask", "cache.answer", "intent", "resolve", "retrieve", "llm", "remember"],
        "{:#?}",
        d.spans
    );
    let t = &d.trace;
    assert_eq!((t.name.as_str(), t.kind.as_str(), t.status.as_str()), ("intelli.ask", "ask", "ok"));
    assert_eq!((t.workspace.as_str(), t.principal.as_str(), t.source.as_str()), ("acme", "lead", "sdk"));
    assert_eq!(t.user_id.as_deref(), Some("lead"), "no entity: the principal's id");
    assert_eq!((t.mode.as_deref(), t.deterministic), (Some("llm"), false));
    assert_eq!((t.input_tokens, t.output_tokens, t.cached_tokens, t.llm_calls), (120, 30, 20, 1));
    assert!((t.cost_usd - 0.0015).abs() < 1e-12);
    assert_eq!((t.provider.as_deref(), t.model.as_deref()), (Some("scripted"), Some("scripted-1")));
    assert_eq!(t.request_id.as_deref(), Some(r.request_id.as_str()));
    assert_eq!(t.input, Some(json!({ "question": "Why is Project Alpha blocked?" })));
    assert_eq!(t.output, Some(json!({ "answer": r.answer })));
    assert!(t.context_tokens > 0 && t.context_tokens as usize == r.efficiency.context_tokens);
    assert_eq!(t.version, onto_intelli::VERSION);
    // The root has no parent; every other span hangs under it.
    let root = &d.spans[0];
    assert_eq!((root.parent_id.as_deref(), root.kind.as_str()), (None, "span"));
    assert_eq!(root.attrs["mode"], "llm");
    assert!(d.spans[1..].iter().all(|s| s.parent_id.as_deref() == Some(root.id.as_str()) && s.trace_id == id));
    assert_eq!(span(&d, "cache.answer").kind, "event");
    assert_eq!(span(&d, "cache.answer").attrs["result"], "miss");
    assert_eq!(span(&d, "intent").attrs["why"], true);
    let ret = span(&d, "retrieve");
    assert_eq!(ret.kind, "retrieval");
    assert_eq!(ret.attrs["facts_used"], json!(r.efficiency.facts_used));
    assert!(ret.input.is_none() && ret.output.is_none(), "facts are never stored");
    let g = span(&d, "llm");
    assert_eq!((g.kind.as_str(), g.model.as_deref()), ("generation", Some("scripted-1")));
    assert_eq!((g.input_tokens, g.output_tokens, g.cached_tokens), (120, 30, 20));
    assert_eq!(g.attrs["provider"], "scripted");
    assert_eq!(g.attrs["response_model"], "scripted-1-2026");
    assert_eq!(g.attrs["finish_reason"], "stop");
    assert_eq!(g.attrs["cache"], "miss");
    assert_eq!(g.attrs["budget_exceeded"], false);
    let input = g.input.as_ref().unwrap();
    assert_eq!(input["system"]["length"], json!(i.system_prompt().chars().count()), "the system prompt as its length…");
    assert!(input["system"]["hash"].is_string(), "… and hash");
    assert!(!input.to_string().contains("enterprise execution assistant"), "never its text");
    let content = input["messages"][0]["content"].as_str().unwrap();
    assert!(content.starts_with("Facts:") && content.len() <= 4096, "clipped to max_content_bytes");
    assert_eq!(g.output, Some(json!({ "text": r.answer })));
    // Spans start inside the trace.
    assert!(d.spans.iter().all(|s| s.ts >= t.ts && s.ts <= t.ts + t.duration_ms.ceil() as i64 + 1));
}

#[tokio::test]
async fn deterministic_cached_and_failing_asks() {
    let i = demo().await;
    // Deterministic: no generation span.
    let r = i.ask(&admin(), AskRequest::new("Who owns Project Alpha?")).await.unwrap();
    assert_eq!(r.mode, AnswerMode::Deterministic);
    let d = get(&i, r.trace_id.as_deref().unwrap()).await;
    assert!(d.trace.deterministic);
    assert_eq!(span(&d, "deterministic").attrs["answered"], true);
    assert!(!span_names(&d).contains(&"llm"));
    assert_eq!((d.trace.llm_calls, d.trace.provider.as_deref()), (0, None));
    // The same question again: the answer cache.
    let again = i.ask(&admin(), AskRequest::new("Who owns Project Alpha?")).await.unwrap();
    assert_eq!(again.mode, AnswerMode::AnswerCache);
    assert_ne!(again.trace_id, r.trace_id, "a trace of its own");
    let d = get(&i, again.trace_id.as_deref().unwrap()).await;
    assert_eq!((d.trace.cache_hits, d.trace.mode.as_deref()), (1, Some("answer_cache")));
    assert_eq!(span(&d, "cache.answer").attrs["result"], "hit");
    // A completion cache hit: a generation span with no tokens and no call
    // (memory off: the first answer, recalled, would change the prompt).
    let i = demo_with(|c| c.memory.enabled = false, Some(Arc::new(Scripted::new("Blocked by issue:421.")))).await;
    let req = || AskRequest {
        question: "Why is Project Alpha blocked?".into(),
        mode: AskMode::Llm,
        include_context: true,
        ..Default::default()
    };
    i.ask(&admin(), req()).await.unwrap();
    let cached = i.ask(&admin(), req()).await.unwrap();
    assert_eq!(cached.mode, AnswerMode::LlmCache);
    let d = get(&i, cached.trace_id.as_deref().unwrap()).await;
    assert_eq!(span(&d, "cache.answer").attrs["result"], "off");
    let g = span(&d, "llm");
    assert_eq!((g.attrs["cache"].as_str(), g.input_tokens), (Some("hit"), 0));
    assert_eq!((d.trace.llm_calls, d.trace.cache_hits, d.trace.input_tokens), (0, 1, 0));
    // An invalid question: an error trace with the code.
    let t = i.ask_traced(&admin(), AskRequest::new("   "), TraceCtx::default()).await;
    assert!(t.result.is_err());
    let d = get(&i, t.trace_id.as_deref().unwrap()).await;
    assert_eq!((d.trace.status.as_str(), d.trace.error_code.as_deref()), ("error", Some("invalid_argument")));
    assert_eq!(d.spans[0].status, "error");

    // A failing model: the fallback answer, the trace and its generation in error.
    let failing = Scripted { reply: String::new(), fail: true };
    let i = demo_with(|_| {}, Some(Arc::new(failing))).await;
    let req = AskRequest { question: "Why is Project Alpha blocked?".into(), mode: AskMode::Llm, ..Default::default() };
    let r = i.ask(&admin(), req).await.unwrap();
    assert_eq!(r.mode, AnswerMode::Fallback);
    let d = get(&i, r.trace_id.as_deref().unwrap()).await;
    assert_eq!((d.trace.status.as_str(), d.trace.error_code.as_deref()), ("error", Some("llm_error")));
    assert!(d.trace.error_message.as_deref().unwrap().contains("overloaded"));
    let g = span(&d, "llm");
    assert_eq!(g.status, "error");
    assert!(g.error.as_deref().unwrap().contains("upstream 503"));
    assert_eq!(d.trace.llm_calls, 1, "a failed call is still a call");
}

#[tokio::test]
async fn budget_reached_is_on_the_trace() {
    struct Spent;
    impl onto_intelli::limits::UsageMeter for Spent {
        fn month_to_date(&self) -> onto_intelli::limits::MonthToDate {
            onto_intelli::limits::MonthToDate { cost_usd: 99.0, tokens: 0 }
        }
        fn record(&self, _: &TokenUsage, _: &str, _: &str, _: bool, _: bool) {}
    }
    let i = demo().await;
    i.set_meter(Some(Arc::new(Spent)));
    i.set_limits(onto_intelli::limits::Limits { monthly_cost_usd: Some(5.0), ..Default::default() });
    let req = AskRequest { question: "Why is Project Alpha blocked?".into(), mode: AskMode::Llm, ..Default::default() };
    let r = i.ask(&admin(), req).await.unwrap();
    assert_eq!(r.mode, AnswerMode::Fallback);
    let d = get(&i, r.trace_id.as_deref().unwrap()).await;
    assert_eq!(d.trace.status, "ok", "a reached budget is not an error");
    assert_eq!(d.trace.metadata["budget_exceeded"], true);
    assert_eq!(span(&d, "llm").attrs["budget_exceeded"], true);
    assert_eq!(d.trace.llm_calls, 0);
}

#[tokio::test]
async fn streaming_asks_are_traced_in_their_task() {
    let i = demo().await;
    let req = AskRequest { question: "Why is Project Alpha blocked?".into(), mode: AskMode::Llm, ..Default::default() };
    let (mut rx, id) = i.try_ask_stream_traced(admin(), req, TraceCtx::from_source("http")).unwrap();
    let id = id.unwrap();
    let mut done = Value::Null;
    while let Some(ev) = rx.recv().await {
        if ev.kind == "done" {
            done = ev.data;
        }
    }
    assert_eq!(done["trace_id"], json!(id));
    let d = get(&i, &id).await;
    assert_eq!(d.trace.source, "http");
    let g = span(&d, "llm");
    assert!(g.attrs["ttft_ms"].is_number(), "streamed: time to first token {}", g.attrs);
    assert_eq!(g.input_tokens, 120);
}

#[tokio::test]
async fn traceparent_sessions_and_users() {
    let i = demo().await;
    // An adopted traceparent: kept as the OpenTelemetry trace, its span the
    // root's parent. Two calls under one upstream trace are two traces.
    const OTEL: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    let mut ids = Vec::new();
    for (parent, q) in [("00f067aa0ba902b7", "Who owns Project Alpha?"), ("00f067aa0ba902b8", "What blocks Alpha?")] {
        let ctx = TraceCtx::from_source("http").with_traceparent(Some(&format!("00-{OTEL}-{parent}-01")));
        let req = AskRequest { no_cache: true, ..AskRequest::new(q) };
        let id = i.ask_traced(&admin(), req, ctx).await.result.unwrap().trace_id.unwrap();
        assert_ne!(id, OTEL, "a trace of its own");
        let d = get(&i, &id).await;
        assert_eq!((d.trace.otel_trace_id.as_deref(), d.trace.parent_span.as_deref()), (Some(OTEL), Some(parent)));
        assert_eq!(d.spans.iter().filter(|s| s.parent_id.as_deref() == Some(parent)).count(), 1, "one root");
        assert_eq!(d.spans[0].parent_id.as_deref(), Some(parent));
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1]);
    let both = i.call(&admin(), "trace.list", json!({ "otel_trace_id": OTEL })).await.unwrap();
    let mut listed: Vec<String> =
        both["traces"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap().to_string()).collect();
    listed.sort();
    ids.sort();
    assert_eq!(listed, ids);
    // An invalid one is ignored.
    let ctx = TraceCtx::from_source("http").with_traceparent(Some("00-zz-yy-01"));
    let r = i.ask_traced(&admin(), AskRequest::new("Who owns Project Alpha?"), ctx).await.result.unwrap();
    assert!(get(&i, r.trace_id.as_deref().unwrap()).await.trace.otel_trace_id.is_none());

    // The session: the caller's header first, then the request, then the conversation.
    let mut ctx = TraceCtx::from_source("http");
    ctx.session_id = Some("from-header".into());
    let trace = AskTrace {
        session_id: Some("from-request".into()),
        user_id: Some("end-user-7".into()),
        tags: vec!["beta".into()],
        metadata: json!({ "plan": "pro" }),
        name: Some("support.ask".into()),
    };
    let req = AskRequest { trace: Some(trace.clone()), ..AskRequest::new("Who owns Project Alpha?") };
    let r = i.ask_traced(&admin(), req, ctx).await.result.unwrap();
    let t = get(&i, r.trace_id.as_deref().unwrap()).await.trace;
    assert_eq!((t.session_id.as_deref(), t.user_id.as_deref()), (Some("from-header"), Some("end-user-7")));
    assert_eq!(
        (t.name.as_str(), t.tags.clone(), t.metadata.clone()),
        ("support.ask", vec!["beta".to_string()], json!({ "plan": "pro" }))
    );
    let req = AskRequest { trace: Some(trace), no_cache: true, ..AskRequest::new("Who owns Project Alpha?") };
    let r = i.ask(&admin(), req).await.unwrap();
    assert_eq!(get(&i, r.trace_id.as_deref().unwrap()).await.trace.session_id.as_deref(), Some("from-request"));
    let req = AskRequest { conversation_id: Some("new".into()), ..AskRequest::new("Who owns Project Alpha?") };
    let r = i.ask(&admin(), req).await.unwrap();
    let conv = r.conversation_id.clone().unwrap();
    assert_eq!(get(&i, r.trace_id.as_deref().unwrap()).await.trace.session_id.as_deref(), Some(conv.as_str()));
    // A principal with an entity is that user.
    let kenji = Principal::new("svc:web", vec![Role::Member], vec!["*".into()]).with_entity("person:kenji");
    let r = i.ask(&kenji, AskRequest::new("What is Kenji working on?")).await.unwrap();
    assert_eq!(get(&i, r.trace_id.as_deref().unwrap()).await.trace.user_id.as_deref(), Some("person:kenji"));
    // trace.session: oldest first, with bodies.
    let s = i.call(&admin(), "trace.session", json!({ "id": conv })).await.unwrap();
    assert_eq!(s["traces"][0]["input"]["question"], "Who owns Project Alpha?");
}

#[tokio::test]
async fn without_capture_content_no_text_is_stored() {
    let i = demo().await;
    i.set_trace_settings(TraceSettings { capture_content: Some(false), ..Default::default() });
    let req = AskRequest { question: "Why is Project Alpha blocked?".into(), mode: AskMode::Llm, ..Default::default() };
    let r = i.ask(&admin(), req).await.unwrap();
    let d = get(&i, r.trace_id.as_deref().unwrap()).await;
    assert!(d.trace.input.is_none() && d.trace.output.is_none());
    let g = span(&d, "llm");
    let text = serde_json::to_string(&d).unwrap();
    assert!(!text.contains("Project Alpha") && !text.contains(&r.answer), "{text}");
    assert_eq!(g.input.as_ref().unwrap()["messages"], 1, "only counts and lengths");
    assert!(g.output.as_ref().unwrap()["length"].as_u64().unwrap() > 0);
    assert_eq!(g.input_tokens, 120, "token counts stay");
    // Lists have no preview then.
    assert!(traces(&i).await.iter().all(|t| t.preview.is_none()));
}

#[tokio::test]
async fn content_is_clipped_at_a_character_boundary() {
    let i = demo_with(|c| c.traces.max_content_bytes = 10, None).await;
    let r = i.ask(&admin(), AskRequest::new("Who owns Prøject Alpha? ✓✓✓✓")).await.unwrap();
    let t = get(&i, r.trace_id.as_deref().unwrap()).await.trace;
    assert_eq!(t.input, Some(json!({ "question": "Who owns P" })));
    let r = i.ask(&admin(), AskRequest::new("Who owns Pr✓ject?")).await.unwrap();
    let t = get(&i, r.trace_id.as_deref().unwrap()).await.trace;
    assert_eq!(t.input, Some(json!({ "question": "Who owns P" })), "✓ (3 bytes) does not fit in 10");
}

#[tokio::test]
async fn actions_and_calls_as_traces_selects() {
    let i = demo().await;
    let input = json!({ "action": "assign", "params": { "item": "issue:433", "assignee": "person:aiko" } });
    let t = i.call_traced(&admin(), "action.apply", input, TraceCtx::from_source("grpc")).await;
    let receipt = t.result.unwrap();
    let d = get(&i, t.trace_id.as_deref().unwrap()).await;
    assert_eq!((d.trace.name.as_str(), d.trace.kind.as_str()), ("action.apply:assign", "action"));
    let tool = &d.spans[0];
    assert_eq!((tool.name.as_str(), tool.kind.as_str()), ("action.apply", "tool"));
    assert_eq!(tool.attrs["action_type"], "assign");
    assert_eq!(tool.attrs["risk"], "low");
    assert_eq!(tool.attrs["receipt"], receipt["id"]);
    assert_eq!(tool.attrs["outcome"], "applied");
    assert!(tool.attrs["edits"].as_u64().unwrap() >= 2);
    let undo = i.call_traced(&admin(), "action.undo", json!({ "receipt": receipt["id"] }), TraceCtx::default()).await;
    assert_eq!(get(&i, undo.trace_id.as_deref().unwrap()).await.trace.name, "action.undo:assign");
    // A failing action: an error trace.
    let bad =
        i.call_traced(&admin(), "action.apply", json!({ "action": "nope", "params": {} }), TraceCtx::default()).await;
    assert!(bad.result.is_err());
    assert_eq!(get(&i, bad.trace_id.as_deref().unwrap()).await.trace.status, "error");
    // `writes`: writing actions are traced, reads are not.
    let w = i
        .call_traced(
            &admin(),
            "entity.upsert",
            json!({ "kind": "team", "id": "team:x", "title": "X" }),
            TraceCtx::default(),
        )
        .await;
    assert_eq!(get(&i, w.trace_id.as_deref().unwrap()).await.trace.kind, "call");
    let r = i.call_traced(&admin(), "entity.get", json!({ "id": "team:x" }), TraceCtx::default()).await;
    assert!(r.result.is_ok() && r.trace_id.is_none());
    // The trace actions are never traced.
    assert!(i.call_traced(&admin(), "trace.list", json!({}), TraceCtx::default()).await.trace_id.is_none());

    let all = demo_with(|c| c.traces.calls = "all".into(), None).await;
    assert!(
        all.call_traced(&admin(), "entity.get", json!({ "id": "project:alpha" }), TraceCtx::default())
            .await
            .trace_id
            .is_some()
    );
    assert!(all.call_traced(&admin(), "trace.metrics", json!({}), TraceCtx::default()).await.trace_id.is_none());
    let none = demo_with(|c| c.traces.calls = "none".into(), None).await;
    let w = none
        .call_traced(
            &admin(),
            "entity.upsert",
            json!({ "kind": "team", "id": "team:y", "title": "Y" }),
            TraceCtx::default(),
        )
        .await;
    assert!(w.trace_id.is_none());
    assert!(
        none.ask(&admin(), AskRequest::new("Who owns Project Alpha?")).await.unwrap().trace_id.is_some(),
        "asks still are"
    );
    let off = demo_with(|c| c.traces.enabled = false, None).await;
    assert!(!off.traces_enabled());
    assert!(off.ask(&admin(), AskRequest::new("Who owns Project Alpha?")).await.unwrap().trace_id.is_none());
    let err = off.call(&admin(), "trace.list", json!({})).await.unwrap();
    assert_eq!(err["traces"], json!([]), "the store still answers");
}

#[tokio::test]
async fn refine_nests_its_generation_spans() {
    let reply =
        r#"{"suggestions": [{"target": "object:customer", "field": "label", "to": "Client", "why": "sales says so"}]}"#;
    let i = demo_with(|_| {}, Some(Arc::new(Scripted::new(reply)))).await;
    let t = i
        .call_traced(&admin(), "ontology.refine", json!({ "source": "warehouse" }), TraceCtx::from_source("http"))
        .await;
    assert_eq!(t.result.unwrap()["applied"], 1);
    let d = get(&i, t.trace_id.as_deref().unwrap()).await;
    assert_eq!((d.trace.name.as_str(), d.trace.kind.as_str()), ("ontology.refine", "call"));
    assert_eq!(span_names(&d), ["ontology.refine", "llm"]);
    assert_eq!(d.spans[1].parent_id.as_deref(), Some(d.spans[0].id.as_str()));
    assert_eq!((d.trace.input_tokens, d.trace.llm_calls), (120, 1));
    assert_eq!(d.trace.model.as_deref(), Some("scripted-1"));
    // advise too (a reasoning question).
    let t = i
        .call_traced(
            &admin(),
            "ontology.advise",
            json!({ "question": "Why should orders be an object type?" }),
            TraceCtx::default(),
        )
        .await;
    t.result.unwrap();
    let d = get(&i, t.trace_id.as_deref().unwrap()).await;
    assert_eq!(span_names(&d), ["ontology.advise", "llm"]);
    assert_eq!(d.trace.input.as_ref().unwrap()["question"], "Why should orders be an object type?");
}

#[tokio::test]
async fn eval_cases_are_scored_traces() {
    let i = demo_with(|_| {}, None).await;
    let suite = r#"
[[case]]
name = "owner"
question = "Who owns Project Alpha?"
expect = ["Payments Team"]
[[case]]
name = "wrong on purpose"
question = "Who owns Project Alpha?"
expect = ["Nobody Team"]
"#;
    let r = i.call(&admin(), "eval.run", json!({ "toml": suite })).await.unwrap();
    assert_eq!((r["passed"].as_u64(), r["failed"].as_u64()), (Some(1), Some(1)));
    let ids: Vec<String> =
        r["cases"].as_array().unwrap().iter().map(|c| c["trace_id"].as_str().unwrap().to_string()).collect();
    let pass = get(&i, &ids[0]).await;
    assert_eq!((pass.trace.kind.as_str(), pass.trace.name.as_str()), ("eval", "eval.case"));
    assert_eq!(pass.trace.metadata["case"], "owner");
    assert!(pass.trace.tags.contains(&"eval".to_string()));
    assert_eq!(pass.scores.len(), 1);
    assert_eq!(
        (pass.scores[0].name.as_str(), pass.scores[0].value, pass.scores[0].source.as_str()),
        ("eval.pass", Some(1.0), "eval")
    );
    let fail = get(&i, &ids[1]).await;
    assert_eq!(fail.scores[0].value, Some(0.0));
    assert!(fail.scores[0].comment.as_deref().unwrap().contains("Nobody Team"));
    let m = i.call(&admin(), "trace.metrics", json!({})).await.unwrap();
    assert_eq!(m["scores"], json!([{ "name": "eval.pass", "count": 2, "avg": 0.5 }]), "{m:#}\n{:#?}", traces(&i).await);
}

#[tokio::test]
async fn trace_actions_and_their_access() {
    let i = demo().await;
    let r = i.ask(&admin(), AskRequest::new("Who owns Project Alpha?")).await.unwrap();
    let id = r.trace_id.unwrap();
    // trace.get flushes the queue when the trace is not written yet (multi-threaded runtimes);
    // here the test flushes.
    i.flush_traces().await;
    let member = Principal::member("m", "*");
    let err = i.call(&member, "trace.list", json!({})).await.unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err}");
    let observer = Principal::named("v", &["viewer", "observer"], &["*"]).unwrap();
    let developer = Principal::named("d", &["member", "developer"], &["*"]).unwrap();
    for p in [&admin(), &observer, &developer] {
        let l = i.call(p, "trace.list", json!({ "kind": "ask" })).await.unwrap();
        assert_eq!(l["traces"][0]["id"], json!(id), "{}", p.id);
        assert!(l["traces"][0].get("input").is_none_or(Value::is_null), "no bodies in lists");
        // The question may quote what the observer cannot read: admins only.
        if p.is_privileged() {
            assert_eq!(l["traces"][0]["preview"], "Who owns Project Alpha?");
        } else {
            assert!(l["traces"][0].get("preview").is_none_or(Value::is_null), "{}: {l}", p.id);
            assert_eq!(l["traces"][0]["redacted"], true);
        }
        // Searching the question would rebuild it: admins only.
        let found = i.call(p, "trace.list", json!({ "q": "project alpha" })).await;
        if p.is_privileged() {
            assert_eq!(found.unwrap()["traces"][0]["id"], json!(id));
        } else {
            assert!(matches!(found, Err(Error::Denied(_))), "{}: {found:?}", p.id);
        }
    }
    let g = i.call(&admin(), "trace.get", json!({ "id": id })).await.unwrap();
    assert_eq!(g["trace"]["input"]["question"], "Who owns Project Alpha?");
    assert!(g["trace"].get("redacted").is_none(), "admins see everything");
    for p in [&observer, &developer] {
        let g = i.call(p, "trace.get", json!({ "id": id })).await.unwrap();
        assert!(g["trace"]["input"].is_null() && g["trace"]["output"].is_null(), "{}: {g}", p.id);
        assert!(g["spans"].as_array().unwrap().iter().all(|s| s["input"].is_null() && s["output"].is_null()));
        assert_eq!(g["trace"]["redacted"], true);
        assert!(!g["spans"].as_array().unwrap().is_empty(), "timings and names stay");
    }
    assert!(i.call(&observer, "trace.get", json!({ "id": "00000000000000000000000000000001" })).await.is_err());
    let m = i.call(&observer, "trace.metrics", json!({ "bucket": "hour" })).await.unwrap();
    assert_eq!(m["requests"], 1);
    let s = i.call(&observer, "trace.sessions", json!({})).await.unwrap();
    assert_eq!(s["sessions"], json!([]));

    // Scores: observers who may write, or the trace's own principal.
    let score = json!({ "trace_id": id, "name": "user.feedback", "value": 1, "comment": "spot on" });
    let err = i.call(&observer, "trace.score", score.clone()).await.unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "a viewer may not score: {err}");
    let s = i.call(&developer, "trace.score", score.clone()).await.unwrap();
    assert_eq!((s["source"].as_str(), s["author"].as_str()), (Some("api"), Some("d")));
    let mine = i.ask(&member, AskRequest::new("Who owns Project Alpha?")).await.unwrap().trace_id.unwrap();
    i.flush_traces().await;
    let own = json!({ "trace_id": mine, "name": "thumbs", "label": "up" });
    assert_eq!(i.call(&member, "trace.score", own).await.unwrap()["label"], "up", "its own trace");
    for bad in [
        json!({ "trace_id": id, "name": "", "value": 1 }),
        json!({ "trace_id": id, "name": "x" }),
        json!({ "trace_id": id, "name": "x", "label": "" }),
        json!({ "trace_id": id, "name": "x", "value": 1, "span_id": "ffffffffffffffff" }),
        json!({ "trace_id": "00000000000000000000000000000001", "name": "x", "value": 1 }),
    ] {
        assert!(i.call(&developer, "trace.score", bad.clone()).await.is_err(), "{bad}");
    }
    let d = get(&i, &id).await;
    assert_eq!(d.scores.len(), 1);

    // A principal limited to one workspace reads that workspace's traces only.
    let scoped = Principal::named("k", &["admin"], &["other"]).unwrap();
    assert_eq!(i.call(&scoped, "trace.list", json!({})).await.unwrap()["traces"], json!([]));
    assert!(i.call(&scoped, "trace.list", json!({ "workspace": "acme" })).await.is_err());
    assert!(i.call(&scoped, "trace.get", json!({ "id": id })).await.is_err());
    // Sessions too: a session of another workspace is not listed.
    let trace = AskTrace { session_id: Some("sess-1".into()), ..Default::default() };
    let req = AskRequest { trace: Some(trace), no_cache: true, ..AskRequest::new("Who owns Project Alpha?") };
    let ws = get(&i, &i.ask(&admin(), req).await.unwrap().trace_id.unwrap()).await.trace.workspace;
    let listed = |v: Value| v["sessions"].as_array().unwrap().iter().any(|s| s["session_id"] == "sess-1");
    assert!(listed(i.call(&admin(), "trace.sessions", json!({})).await.unwrap()));
    assert!(!listed(i.call(&scoped, "trace.sessions", json!({})).await.unwrap()), "not in {ws}");
    assert!(i.call(&scoped, "trace.sessions", json!({ "workspace": ws })).await.is_err());
    let own = Principal::named("k1", &["admin"], &[ws.as_str()]).unwrap();
    assert!(listed(i.call(&own, "trace.sessions", json!({})).await.unwrap()));
    let two = Principal::named("k2", &["admin"], &["a", "b"]).unwrap();
    assert!(i.call(&two, "trace.sessions", json!({})).await.is_err(), "name one of them");
    assert!(i.call(&two, "trace.metrics", json!({})).await.is_err(), "name one of them");
}

#[tokio::test]
async fn retention_deletes_old_traces_only() {
    let i = demo().await;
    i.set_trace_settings(TraceSettings { retention_days: Some(2), ..Default::default() });
    assert_eq!(i.trace_retention_days(), 2);
    let now = onto_intelli::core::time::now_ms();
    let old = |n: i64, age_days: i64| TraceRecord {
        trace: TraceRow {
            id: format!("{n:032x}"),
            ts: now - age_days * 86_400_000,
            name: "old".into(),
            ..Default::default()
        },
        spans: Vec::new(),
    };
    i.ontology().store().trace_write(&[old(1, 3), old(2, 1)]).unwrap();
    let fresh = i.ask(&admin(), AskRequest::new("Who owns Project Alpha?")).await.unwrap().trace_id.unwrap();
    i.flush_traces().await;
    assert_eq!(i.vacuum_traces().unwrap(), 1);
    let left: Vec<String> = traces(&i).await.into_iter().map(|t| t.id).collect();
    assert_eq!(left, vec![fresh, format!("{:032x}", 2)]);
}

#[tokio::test]
async fn a_full_queue_is_counted_in_the_ledger() {
    let i = demo_with(
        |c| {
            c.traces.queue = 1;
            c.traces.flush_ms = 60_000;
        },
        None,
    )
    .await;
    // Deterministic asks never wait, so the writer cannot run in between.
    let asks = (0..5).map(|_| {
        let i = i.clone();
        async move { i.ask(&admin(), AskRequest { no_cache: true, ..AskRequest::new("Who owns Project Alpha?") }).await }
    });
    for r in futures::future::join_all(asks).await {
        r.unwrap();
    }
    assert!(i.ledger().traces_dropped >= 1, "{:?}", i.ledger());
    assert!(i.stats(&admin()).unwrap()["ledger"]["traces_dropped"].as_u64().unwrap() >= 1);
    tokio::time::timeout(Duration::from_secs(5), i.flush_traces()).await.expect("flushing never hangs");
}

/// A model that remembers the `max_tokens` it was asked for.
struct Limited {
    own: Option<u32>,
    asked: parking_lot::Mutex<Vec<u32>>,
}

#[async_trait]
impl LlmProvider for Limited {
    fn name(&self) -> &str {
        "limited"
    }

    fn model(&self) -> &str {
        "limited-1"
    }

    fn max_output_tokens(&self) -> Option<u32> {
        self.own
    }

    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse> {
        self.asked.lock().push(req.max_tokens);
        Ok(LlmResponse { text: "Blocked by issue:421.".into(), ..Default::default() })
    }
}

#[tokio::test]
async fn a_providers_own_output_limit_is_what_requests_ask_for() {
    for (own, expected) in [(Some(16_000), 16_000), (None, 4096)] {
        let llm = Arc::new(Limited { own, asked: Default::default() });
        let i = demo_with(|c| c.llm.max_output_tokens = 4096, Some(llm.clone())).await;
        let req =
            AskRequest { question: "Why is Project Alpha blocked?".into(), mode: AskMode::Llm, ..Default::default() };
        i.ask(&admin(), req).await.unwrap();
        assert_eq!(*llm.asked.lock(), vec![expected], "own limit {own:?}");
    }
}
