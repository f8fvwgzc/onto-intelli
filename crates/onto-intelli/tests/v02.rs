//! v0.2 end to end: competency suite, governed commands, scenarios, MCP, exports.

use std::sync::Arc;

use serde_json::json;

use onto_intelli::eval::{DEMO_EVALS, parse_suite};
use onto_intelli::mcp::McpServer;
use onto_intelli::{AnswerMode, AskRequest, Config, Intelli, Principal, seed};

async fn demo() -> Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    let intelli = Intelli::open(cfg).await.unwrap();
    seed::load_demo(&intelli, "acme").await.unwrap();
    intelli
}

fn admin() -> Principal {
    Principal::admin("lead")
}

#[tokio::test]
async fn demo_competency_suite_passes() {
    let i = demo().await;
    let report = i.eval(&admin(), parse_suite(DEMO_EVALS).unwrap()).await.unwrap();
    let failed: Vec<String> = report
        .cases
        .iter()
        .filter(|c| !c.passed)
        .map(|c| format!("{} — {:?} — answer: {}", c.question, c.failures, c.answer))
        .collect();
    assert!(failed.is_empty(), "failed cases:\n{}", failed.join("\n"));
    assert_eq!(report.pass_rate, 100.0);
    assert!(report.deterministic_share > 50.0, "most questions avoid the LLM: {}", report.deterministic_share);
}

#[tokio::test]
async fn commands_are_proposals_until_confirmed_and_undoable() {
    let i = demo().await;
    let ask = |q: &str, confirm: bool| AskRequest { question: q.into(), confirm, no_cache: true, ..Default::default() };
    let r = i.ask(&admin(), ask("Assign issue 433 to Aiko", false)).await.unwrap();
    assert_eq!(r.mode, AnswerMode::Proposal, "{}", r.answer);
    assert!(r.answer.contains("replacing person:kenji"), "{}", r.answer);
    let before = i.call(&admin(), "graph.neighbors", json!({ "id": "issue:433", "rel": "assigned_to" })).await.unwrap();
    assert!(before.to_string().contains("person:kenji"), "proposal must not write");

    let r = i.ask(&admin(), ask("Assign issue 433 to Aiko", true)).await.unwrap();
    assert_eq!(r.mode, AnswerMode::Action, "{}", r.answer);
    let receipt = r.action.unwrap()["id"].as_str().unwrap().to_string();
    let after = i.call(&admin(), "graph.neighbors", json!({ "id": "issue:433", "rel": "assigned_to" })).await.unwrap();
    assert!(after.to_string().contains("person:aiko"));
    let who = i.ask(&admin(), AskRequest::new("Who is assigned to issue 433?")).await.unwrap();
    assert!(who.answer.contains("Aiko Sato"), "answers reflect the write: {}", who.answer);

    i.call(&admin(), "action.undo", json!({ "receipt": receipt })).await.unwrap();
    let back = i.call(&admin(), "graph.neighbors", json!({ "id": "issue:433", "rel": "assigned_to" })).await.unwrap();
    assert!(back.to_string().contains("person:kenji"));

    let rejected = i.ask(&admin(), ask("resolve issue 380", true)).await.unwrap();
    assert_eq!(rejected.mode, AnswerMode::Proposal);
    assert!(rejected.answer.contains("already resolved"), "{}", rejected.answer);
}

#[tokio::test]
async fn scenarios_compare_without_persisting() {
    let i = demo().await;
    let r = i
        .call(
            &admin(),
            "scenario.run",
            json!({
                "mutations": [{ "op": "patch", "id": "task:31", "state": "done" }],
                "queries": [{ "action": "graph.critical_path", "input": { "id": "project:alpha" } }]
            }),
        )
        .await
        .unwrap();
    let before = r["results"][0]["before"]["cost"].as_f64().unwrap();
    let after = r["results"][0]["after"]["cost"].as_f64().unwrap();
    assert!(after < before, "{before} -> {after}");
    let live = i.call(&admin(), "entity.get", json!({ "id": "task:31" })).await.unwrap();
    assert_eq!(live["state"], "blocked", "scenario edits never persist");
}

#[tokio::test]
async fn mcp_server_speaks_the_protocol() {
    let i = demo().await;
    let mcp = McpServer::new(i.clone(), admin(), false);
    let init = mcp
        .handle(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } }))
        .await
        .unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(mcp.handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await.is_none());
    let list = mcp.handle(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await.unwrap();
    let names: Vec<&str> =
        list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"ask") && names.contains(&"plan_action"));
    assert!(!names.contains(&"apply_action"), "writes hidden when disabled");
    let call = mcp
        .handle(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "ask", "arguments": { "question": "Who owns Project Alpha?" } } }))
        .await
        .unwrap();
    assert_eq!(call["result"]["isError"], false);
    assert!(call["result"]["content"][0]["text"].as_str().unwrap().contains("Payments Team"));
    let denied = mcp
        .handle(json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "apply_action", "arguments": { "action": "resolve", "params": {} } } }))
        .await
        .unwrap();
    assert_eq!(denied["result"]["isError"], true);
    let unknown = mcp.handle(json!({ "jsonrpc": "2.0", "id": 5, "method": "nope" })).await.unwrap();
    assert_eq!(unknown["error"]["code"], -32601);
}

#[tokio::test]
async fn exports_profiles_and_clarification() {
    let i = demo().await;
    let ttl = i.call(&admin(), "export.graph", json!({ "format": "turtle" })).await.unwrap();
    assert!(ttl["content"].as_str().unwrap().contains("prov:wasDerivedFrom"));
    let mm = i
        .call(&admin(), "export.graph", json!({ "format": "mermaid", "start": "project:alpha", "depth": 1 }))
        .await
        .unwrap();
    assert!(mm["content"].as_str().unwrap().contains("classDef alert"));
    let prof = i.call(&admin(), "schema.profile", json!({})).await.unwrap();
    assert!(prof["patterns"].as_array().unwrap().len() > 10);
    let clar = i.ask(&admin(), AskRequest::new("What is #182?")).await.unwrap();
    assert_eq!(clar.mode, AnswerMode::Clarify);
    assert_eq!(clar.options.len(), 2);
}

#[tokio::test]
async fn system_prompt_uses_populated_patterns_and_stays_stable() {
    let i = demo().await;
    let p1 = i.system_prompt();
    assert!(p1.contains("Relations in use"), "{p1}");
    assert!(p1.contains("Certified metrics"), "{p1}");
    i.call(&admin(), "entity.patch", json!({ "id": "task:35", "title": "Roll back PostgreSQL 16.4 (done)" }))
        .await
        .unwrap();
    assert_eq!(p1, i.system_prompt(), "unrelated writes keep the prompt (and provider caches) stable");
}
