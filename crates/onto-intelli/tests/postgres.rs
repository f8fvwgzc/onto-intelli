//! Two orchestrators on one PostgreSQL tenant (two replicas of an app), and
//! hubs of tenants. Runs when a test database is set (see `common`).
#![cfg(feature = "postgres")]

mod common;

use std::time::{Duration, Instant};

use serde_json::json;

use onto_intelli::{Config, Intelli, Principal, Role, seed};

async fn replica(url: &str, tenant: &str) -> std::sync::Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.database_url = Some(url.into());
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
    let Some(url) = common::database_url("replicas") else { return };
    let tenant = common::unique("it");
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

    common::drop_tenant(&url, "onto_", &tenant);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hub_on_an_application_pool() {
    let Some(url) = common::database_url("hub_pool") else { return };
    // The application's own pool, as nexc would hand it over.
    let pool = onto_intelli::core::sqlx::postgres::PgPoolOptions::new().max_connections(8).connect(&url).await.unwrap();
    let shared = onto_intelli::core::store::PgShared::from_pool(pool).unwrap();
    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.postgres.schema_prefix = common::unique("onto_hub") + "_";
    cfg.engine.tenancy = "multi".into();
    let prefix = cfg.engine.postgres.schema_prefix.clone();
    let hub = onto_intelli::Hub::with_postgres(cfg, shared.clone()).unwrap();
    let w1 = hub.tenant("ws_1").await.unwrap();
    let w2 = hub.tenant("ws_2").await.unwrap();
    let alice = Principal::named("user:alice", &["member"], &["*"]).unwrap();
    w1.call(&alice, "entity.upsert", json!({ "id": "team:core", "title": "Core" })).await.unwrap();
    assert!(w2.call(&alice, "entity.get", json!({ "id": "team:core" })).await.is_err());
    let mut known = hub.known_tenants().unwrap();
    known.sort();
    assert_eq!(known, vec!["ws_1", "ws_2"]);
    for t in ["ws_1", "ws_2"] {
        shared
            .tenant(t, onto_intelli::core::store::PgOptions { schema_prefix: prefix.clone(), ..Default::default() })
            .unwrap()
            .drop_tenant()
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configured_hub_gives_each_tenant_its_own_schema() {
    let Some(url) = common::database_url("hub_config") else { return };
    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.database_url = Some(url.clone());
    cfg.engine.tenancy = "multi".into();
    cfg.engine.postgres.schema_prefix = common::unique("onto_cfg") + "_";
    let prefix = cfg.engine.postgres.schema_prefix.clone();
    let hub = onto_intelli::Hub::open(cfg).await.unwrap();
    hub.tenant("default").await.unwrap();
    hub.tenant("ws_9").await.unwrap();
    let mut known = hub.known_tenants().unwrap();
    known.sort();
    assert_eq!(known, vec!["default", "ws_9"]);
    assert!(hub.resolve(Some("ws_10")).await.is_ok(), "multi-tenant hubs open new tenants");
    for t in ["default", "ws_9", "ws_10"] {
        common::drop_tenant(&url, &prefix, t);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_database_url_is_a_clear_error() {
    let mut cfg = Config::default();
    cfg.engine.database_url = None;
    if std::env::var_os("ONTO_DATABASE_URL").is_some() || std::env::var_os("DATABASE_URL").is_some() {
        return; // the environment provides one
    }
    let err = Intelli::open(cfg).await.err().unwrap().to_string();
    assert!(err.contains("--database-url") && err.contains("ONTO_DATABASE_URL"), "{err}");
}
