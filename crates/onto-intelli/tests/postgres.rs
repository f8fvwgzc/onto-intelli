//! Two orchestrators on one PostgreSQL tenant (two replicas of an app).
//! Runs when `ONTO_TEST_POSTGRES_URL` is set.
#![cfg(feature = "postgres")]

use std::time::{Duration, Instant};

use serde_json::json;

use onto_intelli::{Config, Intelli, Principal, Role, seed};

async fn replica(url: &str, tenant: &str) -> std::sync::Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.postgres.url = Some(url.into());
    cfg.engine.postgres.url_env = None;
    cfg.engine.postgres.tenant = tenant.into();
    cfg.engine.default_workspace = "acme".into();
    cfg.engine.follow_interval_ms = 200;
    Intelli::open(cfg).await.unwrap()
}

async fn eventually(what: &str, mut f: impl AsyncFnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replicas_share_graph_schema_proposals_and_memory() {
    let Ok(url) = std::env::var("ONTO_TEST_POSTGRES_URL") else {
        eprintln!("skipped: set ONTO_TEST_POSTGRES_URL to run the PostgreSQL suite");
        return;
    };
    let tenant = format!("it{}", &onto_intelli::core::time::new_id("x")[2..]);
    let a = replica(&url, &tenant).await;
    seed::load_demo(&a, "acme").await.unwrap();
    let b = replica(&url, &tenant).await;
    let admin = Principal::admin("lead");

    // B opened after the seed: same graph, same answers.
    let ask = |i: &std::sync::Arc<Intelli>| {
        let i = i.clone();
        let admin = admin.clone();
        async move { i.call(&admin, "intelli.ask", json!({ "question": "Who owns Project Alpha?" })).await.unwrap() }
    };
    assert_eq!(ask(&a).await["answer"], ask(&b).await["answer"]);

    // A write on A reaches B through LISTEN/NOTIFY.
    a.call(&admin, "entity.upsert", json!({ "id": "team:edge", "title": "Edge Team" })).await.unwrap();
    eventually("b sees team:edge", async || b.call(&admin, "entity.get", json!({ "id": "team:edge" })).await.is_ok())
        .await;

    // Long-term memory written on A is recalled on B.
    a.call(
        &admin,
        "memory.remember",
        json!({ "text": "Payments freezes deploys on Fridays", "entities": ["team:payments"] }),
    )
    .await
    .unwrap();
    eventually("b recalls the memory", async || {
        let r = b.call(&admin, "memory.recall", json!({ "query": "deploy freeze friday" })).await.unwrap();
        r["items"].as_array().is_some_and(|i| !i.is_empty())
    })
    .await;

    // A proposal filed by an agent on A is approved on B and applies once.
    let agent = Principal::new("assistant", vec![Role::Agent], vec!["*".into()]);
    let filed = a
        .call(
            &agent,
            "schema.propose",
            json!({ "title": "Runbooks", "rationale": "on-call",
                "changes": [ { "op": "put_object_type", "name": "runbook", "def": { "category": "knowledge" } } ] }),
        )
        .await
        .unwrap();
    let id = filed["id"].as_str().unwrap().to_string();
    let out = b.call(&admin, "schema.approve", json!({ "id": id })).await.unwrap();
    assert_eq!(out["proposal"]["status"], "applied");
    eventually("a adopts the new schema", async || {
        a.call(&admin, "schema.status", json!({})).await.unwrap()["version"] == json!(2)
    })
    .await;
    let again = a.call(&Principal::admin("other"), "schema.approve", json!({ "id": id })).await.unwrap_err();
    assert!(again.to_string().contains("not open"), "{again}");
    a.call(&admin, "entity.upsert", json!({ "id": "runbook:pay", "title": "Payments runbook" })).await.unwrap();
    eventually("b validates against the new type", async || {
        b.call(&admin, "entity.get", json!({ "id": "runbook:pay" })).await.is_ok()
    })
    .await;

    let store = onto_intelli::core::store::PgStore::connect(&url, &tenant, Default::default()).unwrap();
    store.drop_tenant().unwrap();
}
