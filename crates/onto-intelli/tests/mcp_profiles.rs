//! MCP tool profiles: every tool is reachable from some focused profile and
//! names a real action, `tools/list` under a profile lists exactly its tools
//! plus the floor, unknown profiles are refused, and the write rules still
//! apply inside a profile.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Value, json};

use onto_intelli::actions::ACTIONS;
use onto_intelli::mcp::{
    FLOOR_TOOLS, MCP_PROFILES, McpServer, check_profile, choose_profile, profile_offers, resolve_profile, tool_actions,
    tool_names,
};
use onto_intelli::{Config, Intelli, Principal, Role, seed};

async fn demo() -> Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    let intelli = Intelli::open(cfg).await.unwrap();
    seed::load_demo(&intelli, "acme").await.unwrap();
    intelli
}

fn agent() -> Principal {
    Principal::new("bot", vec![Role::Agent], vec!["*".into()])
}

async fn listed(mcp: &McpServer) -> BTreeSet<String> {
    let list = mcp.handle(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).await.unwrap();
    list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

async fn call(mcp: &McpServer, name: &str, arguments: Value) -> Value {
    mcp.handle(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": name, "arguments": arguments } }))
        .await
        .unwrap()
}

#[test]
fn every_tool_names_a_listed_action() {
    let actions: BTreeSet<&str> = ACTIONS.iter().map(|a| a.name).collect();
    for (tool, action, _) in tool_actions() {
        assert!(actions.contains(action), "tool {tool} calls {action}, which ACTIONS does not list");
    }
    let names = tool_names();
    let unique: BTreeSet<&str> = names.iter().copied().collect();
    assert_eq!(unique.len(), names.len(), "tool names are unique");
}

#[test]
fn every_tool_belongs_to_a_focused_profile() {
    for tool in tool_names() {
        let homes: Vec<&str> =
            MCP_PROFILES.iter().copied().filter(|p| *p != "full" && profile_offers(p, tool)).collect();
        assert!(!homes.is_empty(), "{tool} is only reachable with the full profile");
    }
    for floor in FLOOR_TOOLS {
        assert!(tool_names().contains(floor), "floor tool {floor} exists");
        assert!(MCP_PROFILES.iter().all(|p| profile_offers(p, floor)), "{floor} is in every profile");
    }
    assert!(MCP_PROFILES.iter().all(|p| check_profile(p).is_ok()));
}

#[test]
fn the_new_assurance_tools_are_offered() {
    let names = tool_names();
    for t in [
        "query_inferred",
        "explain_fact",
        "verify_claims",
        "check_slice",
        "certify_reasoning",
        "validate_shapes",
        "induce_shape",
        "check_shapes",
        "lint_schema",
        "list_packs",
        "show_pack",
        "plan_pack",
        "install_pack",
        "align_schema",
        "induce_sheet",
    ] {
        assert!(names.contains(&t), "{t} missing");
    }
}

#[tokio::test]
async fn unknown_profiles_are_refused_with_the_valid_names() {
    let i = demo().await;
    let e = McpServer::new(i.clone(), agent(), false).with_profile("everything").err().expect("refused");
    let msg = e.to_string();
    assert!(msg.contains("everything"), "{msg}");
    for p in MCP_PROFILES {
        assert!(msg.contains(p), "{msg} lists {p}");
    }
    let mut cfg = Config::in_memory();
    cfg.mcp.profile = "nope".into();
    let refused = Intelli::open(cfg).await.err().expect("an unknown [mcp] profile refuses to start");
    assert!(refused.to_string().contains("reasoning"), "{refused}");
}

#[tokio::test]
async fn tools_list_under_a_profile_is_exactly_its_tools_and_the_floor() {
    let i = demo().await;
    let writes: BTreeSet<&str> = tool_actions().into_iter().filter(|t| t.2).map(|t| t.0).collect();
    for profile in MCP_PROFILES {
        for allow_writes in [false, true] {
            let mcp = McpServer::new(i.clone(), agent(), allow_writes).with_profile(profile).unwrap();
            assert_eq!(mcp.profile(), *profile);
            let expected: BTreeSet<String> = tool_names()
                .into_iter()
                .filter(|t| profile_offers(profile, t) && (allow_writes || !writes.contains(t)))
                .map(str::to_string)
                .collect();
            let got = listed(&mcp).await;
            assert_eq!(got, expected, "profile {profile}, allow_writes {allow_writes}");
            for f in FLOOR_TOOLS {
                assert!(got.contains(*f), "{profile} lists the floor tool {f}");
            }
        }
    }
    let full = McpServer::new(i.clone(), agent(), true).with_profile("full").unwrap();
    assert_eq!(listed(&full).await.len(), tool_names().len(), "full offers every tool");
    let reasoning = McpServer::new(i.clone(), agent(), true).with_profile(" reasoning ").unwrap();
    let got = listed(&reasoning).await;
    assert!(got.contains("query_inferred") && got.contains("certify_reasoning"));
    assert!(!got.contains("draft_ontology") && !got.contains("crawl"), "{got:?}");
}

#[tokio::test]
async fn initialize_reports_the_profile_and_what_it_withholds() {
    let i = demo().await;
    let mcp = McpServer::new(i.clone(), agent(), false).with_profile("evaluation").unwrap();
    let init = mcp
        .handle(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } }),
        )
        .await
        .unwrap();
    let text = init["result"]["instructions"].as_str().unwrap();
    let withheld = tool_names().into_iter().filter(|t| !profile_offers("evaluation", t)).count();
    assert!(text.contains("Tool profile: evaluation"), "{text}");
    assert!(text.contains(&format!("{withheld} withheld")), "{text}");
    let full = McpServer::new(i, agent(), false);
    let init = full.handle(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })).await.unwrap();
    assert!(init["result"]["instructions"].as_str().unwrap().contains("Tool profile: full"), "{init}");
}

#[tokio::test]
async fn withheld_tools_are_not_callable_and_write_rules_still_apply() {
    let i = demo().await;
    let catalog = McpServer::new(i.clone(), agent(), true).with_profile("catalog").unwrap();
    let r = call(&catalog, "certify_reasoning", json!({})).await;
    assert_eq!(r["error"]["code"], -32602, "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("catalog"), "{r}");
    let ok = call(&catalog, "get_schema", json!({})).await;
    assert_eq!(ok["result"]["isError"], false, "the floor is always callable: {ok}");

    // governance offers apply_action, but only a server that allows writes runs it.
    let gov = McpServer::new(i.clone(), agent(), false).with_profile("governance").unwrap();
    assert!(!listed(&gov).await.contains("apply_action"));
    let r = call(&gov, "apply_action", json!({ "action": "assign", "params": {} })).await;
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("allow_writes"), "{r}");

    // install_pack is a proposal over MCP, whatever the arguments say.
    let r = call(&gov, "install_pack", json!({ "id": "manufacturing", "mode": "apply", "sample": true })).await;
    assert_eq!(r["result"]["isError"], false, "{r}");
    let out = &r["result"]["structuredContent"];
    assert_eq!(out["mode"], "propose", "{out}");
    assert!(out["proposal"]["id"].is_string(), "{out}");
    let admin = McpServer::new(i.clone(), Principal::admin("lead"), true).with_profile("authoring").unwrap();
    let r = call(&admin, "install_pack", json!({ "id": "retail", "mode": "apply" })).await;
    assert_eq!(r["result"]["structuredContent"]["mode"], "propose", "even for an admin over MCP: {r}");

    let reasoning = McpServer::new(i, agent(), false).with_profile("reasoning").unwrap();
    let r = call(&reasoning, "certify_reasoning", json!({})).await;
    assert_eq!(r["result"]["structuredContent"]["check"]["ok"], true, "{r}");
    let r =
        call(&reasoning, "query_inferred", json!({ "pattern": [["?i", "handled_by_team", "team:payments"]] })).await;
    assert!(!r["result"]["structuredContent"]["bindings"].as_array().unwrap().is_empty(), "{r}");
}

#[test]
fn profile_precedence_is_cli_then_env_then_config() {
    assert_eq!(choose_profile(None, None, "catalog"), "catalog");
    assert_eq!(choose_profile(None, None, ""), "full");
    assert_eq!(choose_profile(Some("reasoning"), None, "catalog"), "reasoning");
    assert_eq!(choose_profile(None, Some("governance"), "catalog"), "governance", "env over config");
    assert_eq!(choose_profile(Some("retrieval"), Some("governance"), "catalog"), "retrieval", "CLI over env");
    assert_eq!(choose_profile(Some(" "), Some(""), "catalog"), "catalog", "empty values are not given");
    // Without ONTO_MCP_PROFILE in the environment the config decides.
    if std::env::var_os("ONTO_MCP_PROFILE").is_none() {
        let cfg = onto_intelli::config::McpConfig { profile: "authoring".into(), ..Default::default() };
        assert_eq!(cfg.effective_profile(None), "authoring");
        assert_eq!(cfg.effective_profile(Some("evaluation")), "evaluation");
        assert_eq!(resolve_profile(None, "catalog"), "catalog");
    }
}
