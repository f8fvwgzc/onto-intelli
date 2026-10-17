//! End-to-end behaviour of the orchestrator on the demo workspace.

use std::sync::Arc;

use serde_json::json;

use onto_intelli::blocking::BlockingIntelli;
use onto_intelli::intent::Intent;
use onto_intelli::{AnswerMode, AskMode, AskRequest, Config, Intelli, Principal, Role, seed};

async fn demo() -> Arc<Intelli> {
    demo_with(|_| {}).await
}

async fn demo_with(tweak: impl FnOnce(&mut Config)) -> Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    tweak(&mut cfg);
    let intelli = Intelli::open(cfg).await.unwrap();
    seed::load_demo(&intelli, "acme").await.unwrap();
    intelli
}

fn admin() -> Principal {
    Principal::admin("tester")
}

async fn ask(i: &Intelli, q: &str) -> onto_intelli::AskResponse {
    i.ask(&admin(), AskRequest::new(q)).await.unwrap()
}

#[tokio::test]
async fn ownership_and_assignment_are_deterministic() {
    let i = demo().await;
    let r = ask(&i, "Who owns Project Alpha?").await;
    assert_eq!(r.mode, AnswerMode::Deterministic);
    assert!(r.answer.contains("Payments Team"), "{}", r.answer);
    assert_eq!(r.usage.total(), 0);

    let r = ask(&i, "Who is assigned to issue #421?").await;
    assert_eq!(r.intent, Intent::Assignee);
    assert!(r.answer.contains("Kenji Tanaka"), "{}", r.answer);
    assert!(r.answer.contains("Payments Team"), "{}", r.answer);
}

#[tokio::test]
async fn counts_lists_and_workload_use_scope() {
    let i = demo().await;
    let r = ask(&i, "How many open issues does the Payments Team have?").await;
    assert_eq!(r.mode, AnswerMode::Deterministic);
    assert!(r.answer.contains("2 open issues"), "{}", r.answer);

    let r = ask(&i, "What is Kenji working on?").await;
    assert_eq!(r.intent, Intent::Workload);
    assert!(r.answer.contains("Implement retries"), "{}", r.answer);
    assert!(!r.answer.contains("Roll back"), "done work is excluded: {}", r.answer);
}

#[tokio::test]
async fn blockers_impact_and_relations() {
    let i = demo().await;
    let r = ask(&i, "What is blocking task 31?").await;
    assert_eq!(r.mode, AnswerMode::Deterministic);
    assert!(r.answer.contains("issue:421"), "{}", r.answer);

    let r = ask(&i, "What breaks if PostgreSQL goes down?").await;
    assert_eq!(r.intent, Intent::Impact);
    assert!(r.answer.contains("Payment API"), "{}", r.answer);
    assert!(r.answer.contains("Project Alpha"), "{}", r.answer);

    let r = ask(&i, "How is Kenji related to PostgreSQL?").await;
    assert!(r.answer.contains("—"), "{}", r.answer);
}

#[tokio::test]
async fn why_questions_go_to_the_llm_with_root_cause_context() {
    let i = demo().await;
    let r = i
        .ask(
            &admin(),
            AskRequest {
                question: "Why is Project Alpha blocked?".into(),
                include_context: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.mode, AnswerMode::Llm);
    let ctx = r.context.expect("context requested");
    assert!(ctx.facts.iter().any(|f| f.source == "cause" && f.text.contains("deployment:182")), "{:#?}", ctx.facts);
    assert!(ctx.tokens <= 1800);
    assert!(r.efficiency.naive_tokens >= r.efficiency.context_tokens);
    assert!(r.usage.prompt_tokens > 0);
}

#[tokio::test]
async fn caches_answers_until_the_graph_changes() {
    // Memory off: recalled episodes would otherwise (correctly) change the prompt.
    let i = demo_with(|c| c.memory.enabled = false).await;
    let q = "Why did we generate postgres-ha.tf?";
    let first = ask(&i, q).await;
    assert_eq!(first.mode, AnswerMode::Llm);
    let second = ask(&i, q).await;
    assert_eq!(second.mode, AnswerMode::AnswerCache);
    assert_eq!(second.usage.total(), 0);

    // A write outside this question's context graph: the answer cache is
    // invalidated (new revision) but the prompt is identical, so the LLM
    // completion cache answers.
    i.call(&admin(), "entity.patch", json!({ "id": "task:35", "title": "Roll back PostgreSQL 16.4 (done)" }))
        .await
        .unwrap();
    let third = ask(&i, q).await;
    assert_eq!(third.mode, AnswerMode::LlmCache);
    assert!(i.ledger().tokens_saved > 0);
}

#[tokio::test]
async fn conversations_carry_entities_between_turns() {
    let i = demo().await;
    let first = i
        .ask(
            &admin(),
            AskRequest {
                question: "What is Project Alpha?".into(),
                conversation_id: Some("new".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let conv = first.conversation_id.clone().unwrap();
    let second = i
        .ask(
            &admin(),
            AskRequest { question: "Who owns it?".into(), conversation_id: Some(conv.clone()), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(second.answer.contains("Payments Team"), "{}", second.answer);
    let thread = i.call(&admin(), "graph.neighbors", json!({ "id": format!("conversation:{conv}") })).await.unwrap();
    assert!(thread.to_string().contains("project:alpha"), "{thread}");
}

#[tokio::test]
async fn permissions_filter_answers_and_context() {
    let i = demo().await;
    let outsider = Principal::new("eve", vec![Role::Member], vec!["other".into()]);
    assert!(i.ask(&outsider, AskRequest::new("Who owns Project Alpha?")).await.is_err());
    let member = Principal::new("aiko", vec![Role::Member], vec!["acme".into()]).with_entity("person:aiko");
    let r = i
        .ask(
            &member,
            AskRequest {
                question: "What are the engineering salary bands?".into(),
                include_context: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let ctx = r.context.unwrap();
    assert!(!ctx.text.contains("Confidential"), "restricted knowledge leaked: {}", ctx.text);
    let r = i.ask(&member, AskRequest::new("what am I working on")).await.unwrap();
    assert!(r.answer.contains("One-click checkout UI"), "{}", r.answer);
}

#[tokio::test]
async fn deterministic_mode_never_calls_the_llm() {
    let i = demo().await;
    let r = i
        .ask(
            &admin(),
            AskRequest {
                question: "Should we delay the launch?".into(),
                mode: AskMode::Deterministic,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.mode, AnswerMode::Fallback);
    assert_eq!(i.ledger().llm_calls, 0);
}

#[tokio::test]
async fn streaming_emits_stages_deltas_and_done() {
    let i = demo().await;
    let mut rx = i.ask_stream(
        admin(),
        AskRequest { question: "Why is Project Alpha blocked?".into(), no_cache: true, ..Default::default() },
    );
    let mut kinds = Vec::new();
    while let Some(ev) = rx.recv().await {
        kinds.push(ev.kind.clone());
        if ev.is_terminal() {
            break;
        }
    }
    assert!(kinds.contains(&"entities".to_string()));
    assert!(kinds.iter().filter(|k| *k == "delta").count() > 1);
    assert_eq!(kinds.last().map(String::as_str), Some("done"));
}

#[tokio::test]
async fn actions_cover_graph_memory_events_and_ingest() {
    let i = demo().await;
    let p = admin();
    let cp = i.call(&p, "graph.critical_path", json!({ "id": "project:alpha" })).await.unwrap();
    let path: Vec<String> =
        cp["path"].as_array().unwrap().iter().map(|n| n["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(path, vec!["issue:421", "task:31", "task:32", "task:34"], "{cp}");

    let rc = i.call(&p, "graph.root_cause", json!({ "id": "project:alpha" })).await.unwrap();
    assert_eq!(rc["chains"][0]["culprit"]["id"], "deployment:182");

    let mem = i.call(&p, "memory.recall", json!({ "query": "who is on call for checkout incidents" })).await.unwrap();
    assert!(mem.to_string().contains("on call"), "{mem}");

    let receipt = i
        .call(
            &p,
            "ingest.batch",
            json!({ "async": true, "records": [
                { "op": "upsert", "kind": "issue", "key": "500", "title": "Card declines spike", "state": "open" },
                { "op": "link", "from": "issue:500", "rel": "affects", "to": "service:payment-api" }
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(receipt["queued"], true);
    i.call(&p, "ingest.flush", json!({})).await.unwrap();
    let got = i.call(&p, "entity.get", json!({ "id": "issue:500" })).await.unwrap();
    assert_eq!(got["title"], "Card declines spike");

    let events = i.call(&p, "event.list", json!({ "target": "deployment:190" })).await.unwrap();
    assert_eq!(events["events"][0]["outcome"], "denied");
    assert!(i.call(&p, "no.such.action", json!({})).await.is_err());
}

#[test]
fn blocking_api_works_without_a_runtime() {
    let engine = BlockingIntelli::in_memory().unwrap();
    let p = Principal::admin("cli");
    engine
        .call(&p, "graph.apply", json!({ "workspace": "default", "mutations": [
            { "op": "upsert", "kind": "team", "key": "core", "title": "Core Team" },
            { "op": "upsert", "kind": "project", "key": "x", "title": "Project X", "links": [{ "rel": "owned_by", "to": "team:core" }] }
        ]}))
        .unwrap();
    let r = engine.ask(&p, "Who owns Project X?").unwrap();
    assert!(r.answer.contains("Core Team"), "{}", r.answer);
    assert_eq!(engine.ontology().stats().unwrap().entities, 2);
}

#[tokio::test]
async fn repeated_questions_do_not_feed_their_own_answers_back() {
    let i = demo().await;
    let req = || AskRequest {
        question: "Why is Project Alpha blocked?".into(),
        include_context: true,
        no_cache: true,
        ..Default::default()
    };
    i.ask(&admin(), req()).await.unwrap();
    let again = i.ask(&admin(), req()).await.unwrap();
    let ctx = again.context.unwrap();
    assert!(!ctx.text.contains("Q: Why is Project Alpha blocked"), "own answer leaked back: {}", ctx.text);
    let first_cause = ctx.facts.iter().find(|f| f.source == "cause").unwrap();
    assert!(first_cause.text.contains("deployment:182"), "deepest root cause should lead: {}", first_cause.text);
    assert!(!ctx.text.contains("entity.create"), "CRUD audit noise in context");
}
