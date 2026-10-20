//! Many tenants in one process, per-call principals.

use serde_json::json;

use onto_intelli::{Config, Hub, Principal, seed};

#[tokio::test]
async fn tenants_are_separate_ontologies() {
    let hub = Hub::open(Config::in_memory()).await.unwrap();
    let a = hub.tenant("ws_a").await.unwrap();
    let b = hub.tenant("ws_b").await.unwrap();
    seed::load_demo(&a, "acme").await.unwrap();
    let admin = Principal::admin("x");
    assert!(a.call(&admin, "entity.get", json!({ "id": "project:alpha" })).await.is_ok());
    assert!(b.call(&admin, "entity.get", json!({ "id": "project:alpha" })).await.is_err());
    // Schema changes are per tenant too.
    a.call(&admin, "schema.apply", json!({ "changes": [ { "op": "put_object_type", "name": "runbook", "def": {} } ] }))
        .await
        .unwrap();
    assert_eq!(a.call(&admin, "schema.status", json!({})).await.unwrap()["version"], 2);
    assert_eq!(b.call(&admin, "schema.status", json!({})).await.unwrap()["version"], 1);
    // The same handle comes back; ids are validated.
    assert!(std::sync::Arc::ptr_eq(&a, &hub.tenant("ws_a").await.unwrap()));
    assert!(hub.tenant("../etc").await.is_err());
    assert_eq!(hub.open_tenants(), vec!["ws_a", "ws_b"]);
    // Single-tenant hubs refuse other tenants.
    assert!(hub.resolve(Some("ws_c")).await.is_err());
}

#[tokio::test]
async fn per_call_principals_apply_roles_and_acls() {
    let hub = Hub::open(Config::in_memory()).await.unwrap();
    let ws = hub.tenant("ws_1").await.unwrap();
    seed::load_demo(&ws, "acme").await.unwrap();
    let admin = Principal::admin("x");
    ws.call(
        &admin,
        "entity.upsert",
        json!({ "id": "knowledge:bands", "title": "Salary bands",
        "props": { "visibility": "restricted", "acl": ["team:payments"] } }),
    )
    .await
    .unwrap();
    let kenji = Principal::named("user:kenji", &["member"], &["*"]).unwrap().with_entity("person:kenji");
    let eve = Principal::named("user:eve", &["member", "approver"], &["*"]).unwrap().with_entity("person:eve");
    assert!(ws.call(&kenji, "entity.get", json!({ "id": "knowledge:bands" })).await.is_ok());
    assert!(ws.call(&eve, "entity.get", json!({ "id": "knowledge:bands" })).await.is_err());
    let viewer = Principal::named("user:v", &["viewer"], &["*"]).unwrap();
    assert!(ws.call(&viewer, "entity.upsert", json!({ "id": "task:z", "title": "z" })).await.is_err());
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_tenants_get_their_own_files() {
    let dir = std::env::temp_dir().join(format!("onto-hub-{}", onto_intelli::core::time::new_id("t")));
    let mut cfg = Config { base_dir: dir.clone(), ..Default::default() };
    cfg.engine.tenancy = "multi".into();
    let hub = Hub::open(cfg).await.unwrap();
    hub.tenant("default").await.unwrap();
    hub.tenant("ws_9").await.unwrap();
    assert!(dir.join(".onto/onto.db").is_file());
    assert!(dir.join(".onto/tenants/ws_9/onto.db").is_file());
    assert_eq!(hub.known_tenants().unwrap(), vec!["default", "ws_9"]);
    assert!(hub.resolve(Some("ws_10")).await.is_ok(), "multi-tenant hubs open new tenants");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn every_listed_action_is_dispatched() {
    let hub = Hub::open(Config::in_memory()).await.unwrap();
    let i = hub.tenant("t").await.unwrap();
    let admin = Principal::admin("x");
    for a in onto_intelli::actions::ACTIONS {
        if matches!(a.name, "ingest.flush" | "eval.run" | "intelli.ask" | "memory.consolidate") {
            continue;
        }
        if let Err(e) = i.call(&admin, a.name, json!({})).await {
            assert!(!e.to_string().contains("see system.actions"), "{} is listed but not dispatched", a.name);
        }
    }
}
