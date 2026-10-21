//! The ontology builder over the action API: draft the demo warehouse,
//! refine it with a model, ask for advice, apply it as an admin, and see
//! that agents can only propose (also over MCP). Then explore the objects.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use onto_intelli::llm::{LlmProvider, LlmRequest, LlmResponse};
use onto_intelli::mcp::McpServer;
use onto_intelli::token::TokenUsage;
use onto_intelli::{AnswerMode, AskRequest, Config, Intelli, Principal, Result, Role, seed};

fn config() -> Config {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    cfg
}

async fn demo() -> Arc<Intelli> {
    let i = Intelli::open(config()).await.unwrap();
    seed::load_demo(&i, "acme").await.unwrap();
    i
}

fn admin() -> Principal {
    Principal::admin("lead")
}

fn agent() -> Principal {
    Principal::new("assistant", vec![Role::Agent], vec!["*".into()])
}

/// The `name`s of a list, sorted (drafts order object types by OntoRank).
fn names(v: &Value, key: &str) -> Vec<String> {
    let mut out: Vec<String> =
        v[key].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap().to_string()).collect();
    out.sort();
    out
}

/// A model that answers with fixed suggestions, wrapped in prose and a fence.
struct Scripted(String);

#[async_trait]
impl LlmProvider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }

    fn model(&self) -> &str {
        "scripted-1"
    }

    async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse> {
        Ok(LlmResponse {
            text: self.0.clone(),
            usage: TokenUsage { prompt_tokens: 100, completion_tokens: 50, ..Default::default() },
            model: "scripted-1".into(),
            finish_reason: Some("stop".into()),
        })
    }
}

#[tokio::test]
async fn drafts_the_demo_warehouse() {
    let i = demo().await;
    // With one crawled source, the source may be left out.
    let d = i.call(&admin(), "ontology.draft", json!({})).await.unwrap();
    assert_eq!(d["source"], "warehouse");
    assert_eq!(names(&d, "objects"), ["customer", "order"], "{d:#}");
    let order = d["objects"].as_array().unwrap().iter().find(|o| o["name"] == "order").unwrap();
    assert_eq!(order["table"], "sales.orders");
    assert_eq!(order["key"], json!(["id"]));
    let link = &d["links"][0];
    assert_eq!(
        (link["name"].as_str(), link["from"].as_str(), link["to"].as_str()),
        (Some("customer"), Some("order"), Some("customer"))
    );
    assert_eq!(link["column"], "customer_id");
    assert!(names(&d, "actions").contains(&"create_order".to_string()));
    let skipped: Vec<&str> = d["skipped"].as_array().unwrap().iter().filter_map(|s| s["table"].as_str()).collect();
    assert!(skipped.contains(&"sales.order_items") && skipped.contains(&"staging.revenue_tmp"), "{skipped:?}");
    // The draft is a valid schema change set.
    let plan = i.call(&admin(), "schema.plan", d["changes"].clone()).await.unwrap();
    assert_eq!(plan["valid"], true, "{plan}");
    // Options: no actions, only one table.
    let d = i
        .call(
            &admin(),
            "ontology.draft",
            json!({ "source": "warehouse", "tables": "sales.customers", "no_actions": true }),
        )
        .await
        .unwrap();
    assert_eq!(names(&d, "objects"), ["customer"]);
    assert!(d["actions"].as_array().unwrap().is_empty());
    let missing = i.call(&admin(), "ontology.draft", json!({ "source": "nope" })).await.unwrap_err();
    assert!(missing.to_string().contains("not found"), "{missing}");
}

#[tokio::test]
async fn refine_without_a_model_returns_the_draft_with_a_note() {
    let i = demo().await;
    let r = i.call(&admin(), "ontology.refine", json!({ "source": "warehouse" })).await.unwrap();
    assert_eq!(r["llm"], "none");
    assert_eq!(r["applied"], 0);
    assert_eq!(r["tokens"], 0);
    assert_eq!(names(&r["draft"], "objects"), ["customer", "order"]);
    let notes = r["draft"]["notes"].to_string();
    assert!(notes.contains("deterministic draft"), "{notes}");
}

#[tokio::test]
async fn refine_keeps_only_suggestions_that_stay_valid() {
    let reply = r#"Here are my suggestions:
```json
{"suggestions": [
  {"target": "object:order", "field": "name", "from": "order", "to": "purchase", "why": "the business says purchase"},
  {"target": "object:customer", "field": "label", "to": "Client", "why": "sales calls them clients"},
  {"target": "link:customer", "field": "inverse", "from": "orders", "to": "purchases", "why": "follows the rename"},
  {"target": "property:customer.region", "field": "type", "to": "enum", "why": "few regions"},
  {"target": "object:customer", "field": "name", "to": "person", "why": "clash with an existing type"},
  {"target": "object:purchase", "field": "transition", "to": "shipped", "why": "no such state"},
  {"target": "action:edit_customer", "field": "description", "to": "Change a client's name or region.", "why": "clearer"},
  {"target": "nowhere", "field": "name", "to": "x"}
]}
```
That is all."#;
    let i = Intelli::open_with(config(), Arc::new(Scripted(reply.into()))).await.unwrap();
    seed::load_demo(&i, "acme").await.unwrap();
    let r = i
        .call(&admin(), "ontology.refine", json!({ "source": "warehouse", "instructions": "use business names" }))
        .await
        .unwrap();
    assert_eq!(r["llm"], "scripted");
    assert_eq!(r["tokens"], 150);
    assert_eq!(r["suggestions"].as_array().unwrap().len(), 8);
    assert_eq!(r["applied"], 4, "{r:#}");
    let errors: Vec<&str> = r["rejected"].as_array().unwrap().iter().filter_map(|x| x["error"].as_str()).collect();
    assert_eq!(errors.len(), 4, "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("enum property needs values")), "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("existing object type")), "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("not a state")), "{errors:?}");
    let d = &r["draft"];
    assert_eq!(names(d, "objects"), ["customer", "purchase"]);
    let customer = d["objects"].as_array().unwrap().iter().find(|o| o["name"] == "customer").unwrap();
    assert_eq!(customer["label"], "Client");
    assert_eq!(d["links"][0]["from"], "purchase");
    assert_eq!(d["links"][0]["inverse"], "purchases");
    let actions = names(d, "actions");
    assert!(actions.contains(&"create_purchase".to_string()) && !actions.contains(&"create_order".to_string()));
    // The rebuilt change set carries the renames and still plans cleanly.
    let plan = i.call(&admin(), "schema.plan", d["changes"].clone()).await.unwrap();
    assert_eq!(plan["valid"], true, "{plan}");
    assert!(d["changes"].to_string().contains("\"purchase\""));
    // Reasoning questions go to the model; lookups stay deterministic.
    let why =
        i.call(&admin(), "ontology.advise", json!({ "question": "Why is order not a container?" })).await.unwrap();
    assert_eq!(why["mode"], "llm", "{why}");
    let which =
        i.call(&admin(), "ontology.advise", json!({ "question": "Which link types should orders have?" })).await;
    assert_eq!(which.unwrap()["mode"], "deterministic");
}

#[tokio::test]
async fn advice_and_questions_come_from_the_draft() {
    let i = demo().await;
    let a = i.call(&admin(), "ontology.advise", json!({ "question": "Which link types should orders have?" })).await;
    let a = a.unwrap();
    assert_eq!(a["mode"], "deterministic");
    let answer = a["answer"].as_str().unwrap();
    assert!(answer.contains("order → customer") && answer.contains("sales.orders.customer_id"), "{answer}");
    let a = i
        .call(&admin(), "ontology.advise", json!({ "question": "What column types does the customers table get?" }))
        .await
        .unwrap();
    assert!(a["answer"].as_str().unwrap().contains("created_at timestamp"), "{a}");
    let r = i.ask(&admin(), AskRequest::new("What object types can we create from the warehouse?")).await.unwrap();
    assert_eq!(r.mode, AnswerMode::Deterministic, "{}", r.answer);
    assert_eq!(r.intent.as_str(), "model");
    assert!(r.answer.contains("customer") && r.answer.contains("sales.orders"), "{}", r.answer);
}

#[tokio::test]
async fn admins_apply_agents_propose() {
    let i = demo().await;
    // An agent asking to apply is refused and pointed at proposing.
    let denied = i.call(&agent(), "ontology.apply_draft", json!({ "source": "warehouse" })).await.unwrap_err();
    assert!(matches!(denied, onto_intelli::Error::Denied(_)), "{denied}");
    assert!(denied.to_string().contains("propose"), "{denied}");
    assert!(i.ontology().schema().object_type("order").is_none());

    // It may propose; a person approves.
    let filed =
        i.call(&agent(), "ontology.apply_draft", json!({ "source": "warehouse", "mode": "propose" })).await.unwrap();
    assert_eq!(filed["mode"], "proposed");
    assert_eq!(filed["proposal"]["status"], "open");
    assert_eq!(filed["proposal"]["proposer"]["agent"], true);
    assert!(filed["proposal"]["title"].as_str().unwrap().contains("warehouse"));
    assert!(i.ontology().schema().object_type("order").is_none(), "a proposal changes nothing");

    // An admin applies directly: a new schema version with backed types.
    let before = i.ontology().schema_version();
    let out = i.call(&admin(), "ontology.apply_draft", json!({ "source": "warehouse" })).await.unwrap();
    assert_eq!(out["mode"], "applied", "{out}");
    assert_eq!(out["version"]["version"], json!(before + 1));
    assert_eq!(out["counts"]["objects"], 2);
    assert!(out["hint"].as_str().unwrap().contains("onto crawl warehouse"), "{out}");
    let schema = i.ontology().schema();
    let order = schema.object_type("order").unwrap();
    assert_eq!(order.backing.as_ref().unwrap().table, "sales.orders");
    assert!(schema.action_types.contains_key("create_customer"));

    // Explore: object types with counts, then objects with facets and stats.
    for name in ["Ada", "Bo"] {
        let r = i
            .call(
                &admin(),
                "action.apply",
                json!({ "action": "create_customer", "params": { "cust_name": name, "region": "EU" }, "workspace": "acme" }),
            )
            .await
            .unwrap();
        assert_eq!(r["status"], "applied", "{r}");
    }
    let types = i.call(&admin(), "objects.types", json!({})).await.unwrap();
    let customer = types["types"].as_array().unwrap().iter().find(|t| t["name"] == "customer").unwrap().clone();
    assert_eq!(customer["count"], 2, "{customer}");
    assert_eq!(customer["backing"]["table"], "sales.customers");
    let ex = i
        .call(
            &admin(),
            "objects.explore",
            json!({ "kind": "customer", "filters": [{ "prop": "cust_name", "op": "eq", "value": "Ada" }] }),
        )
        .await
        .unwrap();
    assert_eq!(ex["total"], 1, "{ex}");
    assert_eq!(ex["objects"][0]["title"], "Ada");
    assert!(ex["actions"].to_string().contains("edit_customer"), "{ex}");
    let all = i.call(&admin(), "objects.explore", json!({ "kind": "customer", "sort": "-title" })).await.unwrap();
    assert_eq!(all["objects"][0]["title"], "Bo");
    assert_eq!(all["total"], 2);
    // created_at is the source column (no customer created here has one),
    // not the time the object was created; that is _created_at.
    assert!(all["stats"].get("created_at").is_none(), "{all}");
    let made = i
        .call(&admin(), "objects.explore", json!({ "kind": "customer", "stats": ["created_at", "_created_at"] }))
        .await
        .unwrap();
    assert_eq!(made["stats"]["_created_at"]["count"], 2, "{made}");
    assert!(made["stats"].get("created_at").is_none(), "{made}");
}

#[tokio::test]
async fn preview_rebuilds_the_change_set_of_an_edited_draft() {
    let i = demo().await;
    let mut d = i.call(&admin(), "ontology.draft", json!({ "source": "warehouse" })).await.unwrap();
    // The builder UI drops a type with its links and actions, and renames another.
    let keep = |list: &Value, f: &dyn Fn(&Value) -> bool| -> Value {
        Value::Array(list.as_array().unwrap().iter().filter(|x| f(x)).cloned().collect())
    };
    d["objects"] = keep(&d["objects"], &|o| o["name"] != "order");
    d["links"] = keep(&d["links"], &|l| l["from"] != "order" && l["to"] != "order");
    d["actions"] = keep(&d["actions"], &|a| a["object"] != "order");
    for o in d["objects"].as_array_mut().unwrap() {
        o["name"] = json!("client");
    }
    for a in d["actions"].as_array_mut().unwrap() {
        a["object"] = json!("client");
    }
    let before = i.ontology().schema_version();
    let out = i.call(&admin(), "ontology.apply_draft", json!({ "draft": d, "preview": true })).await.unwrap();
    assert_eq!(out["mode"], "preview", "{out}");
    assert_eq!(i.ontology().schema_version(), before, "a preview applies nothing");
    let changes = out["draft"]["changes"]["changes"].as_array().unwrap();
    let objects: Vec<&str> =
        changes.iter().filter(|c| c["op"] == "put_object_type").map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(objects, ["client"], "{out:#}");
    assert!(changes.iter().all(|c| c["op"] != "put_link_type"), "{out:#}");
    let plan = i.call(&admin(), "schema.plan", out["draft"]["changes"].clone()).await.unwrap();
    assert_eq!(plan["valid"], true, "{plan:#}");
}

#[tokio::test]
async fn refine_refuses_what_the_rows_would_not_meet() {
    let reply = r#"{"suggestions": [
      {"target": "property:customer.region", "field": "values", "to": ["north", "south"], "why": "two regions"},
      {"target": "property:customer.region", "field": "required", "to": true, "why": "every customer has one"},
      {"target": "property:customer.region", "field": "description", "to": "The sales region", "why": "clearer"}
    ]}"#;
    let i = Intelli::open_with(config(), Arc::new(Scripted(reply.into()))).await.unwrap();
    seed::load_demo(&i, "acme").await.unwrap();
    let r = i.call(&admin(), "ontology.refine", json!({ "source": "warehouse" })).await.unwrap();
    assert_eq!(r["applied"], 1, "{r:#}");
    let errors: Vec<&str> = r["rejected"].as_array().unwrap().iter().filter_map(|x| x["error"].as_str()).collect();
    assert!(errors.iter().any(|e| e.contains("no value list")), "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("nullable")), "{errors:?}");
    let customer = r["draft"]["objects"].as_array().unwrap().iter().find(|o| o["name"] == "customer").unwrap();
    let region = customer["properties"].as_array().unwrap().iter().find(|p| p["name"] == "region").unwrap();
    assert_eq!((region["ty"].as_str(), region["required"].as_bool()), (Some("string"), Some(false)), "{region}");
}

#[tokio::test]
async fn a_draft_given_as_its_parts_is_rebuilt() {
    let i = demo().await;
    let d = i.call(&admin(), "ontology.draft", json!({ "source": "warehouse" })).await.unwrap();
    // Only the parts the server rebuilds from: no source, change set, skipped or notes.
    let parts = json!({ "objects": d["objects"], "links": d["links"], "actions": d["actions"] });
    let out = i
        .call(&admin(), "ontology.apply_draft", json!({ "source": "warehouse", "draft": parts, "preview": true }))
        .await
        .unwrap();
    assert_eq!((out["mode"].as_str(), out["source"].as_str()), (Some("preview"), Some("warehouse")), "{out}");
    assert_eq!(out["draft"]["changes"], d["changes"], "the same change set as the full draft");
    // The source is resolved when the draft does not name it.
    let out = i.call(&admin(), "ontology.apply_draft", json!({ "draft": parts, "mode": "propose" })).await.unwrap();
    assert_eq!(out["mode"], "proposed", "{out}");
}

#[tokio::test]
async fn mcp_propose_ontology_proposes_whatever_the_arguments() {
    let i = demo().await;
    // A trusted local principal on a read-only server: still only a proposal.
    let mcp = McpServer::new(i.clone(), admin(), false);
    let call = |id: i64, arguments: Value| {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": "propose_ontology", "arguments": arguments } })
    };
    let filed = mcp.handle(call(1, Value::Null)).await.unwrap();
    assert_eq!(filed["result"]["structuredContent"]["mode"], "proposed", "{filed}");
    assert_eq!(filed["result"]["structuredContent"]["proposal"]["source"], "mcp");
    let previewed = mcp.handle(call(2, json!({ "source": "warehouse", "preview": true, "propose": false }))).await;
    assert_eq!(previewed.unwrap()["result"]["structuredContent"]["mode"], "proposed", "preview is not honoured");
    assert!(i.ontology().schema().object_type("order").is_none(), "nothing applied");
    let bad = mcp.handle(call(3, json!(["warehouse"]))).await.unwrap();
    assert_eq!(bad["error"]["code"], -32602, "{bad}");

    // The explorer tool advertises every input objects.explore takes.
    let list = mcp.handle(json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/list" })).await.unwrap();
    let explore = list["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "explore_objects").unwrap();
    for input in ["kind", "filters", "search", "sort", "workspace", "facets", "stats", "limit", "offset"] {
        assert!(explore["inputSchema"]["properties"].get(input).is_some(), "{input}: {explore}");
    }
}

#[tokio::test]
async fn mcp_propose_ontology_files_a_proposal_even_with_writes_allowed() {
    let i = demo().await;
    let mcp = McpServer::new(i.clone(), agent(), true);
    let list = mcp.handle(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).await.unwrap();
    let tools: Vec<&str> =
        list["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    for t in
        ["draft_ontology", "refine_ontology", "advise_ontology", "propose_ontology", "explore_objects", "object_types"]
    {
        assert!(tools.contains(&t), "{t} missing from {tools:?}");
    }
    let draft = mcp
        .handle(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "draft_ontology", "arguments": { "source": "warehouse" } } }))
        .await
        .unwrap();
    assert_eq!(draft["result"]["isError"], false, "{draft}");
    let filed = mcp
        .handle(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "propose_ontology", "arguments": {
                "source": "warehouse", "mode": "apply", "rationale": "The sales tables are queried daily" } } }))
        .await
        .unwrap();
    assert_eq!(filed["result"]["isError"], false, "{filed}");
    let out = &filed["result"]["structuredContent"];
    assert_eq!(out["mode"], "proposed", "always a proposal, whatever mode the agent asks for");
    assert_eq!(out["proposal"]["source"], "mcp");
    assert_eq!(out["proposal"]["rationale"], "The sales tables are queried daily");
    assert!(i.ontology().schema().object_type("order").is_none());
    let id = out["proposal"]["id"].as_str().unwrap().to_string();
    let approved = i.call(&admin(), "schema.approve", json!({ "id": id })).await.unwrap();
    assert_eq!(approved["proposal"]["status"], "applied", "{approved}");
    assert!(i.ontology().schema().object_type("order").is_some());
}
