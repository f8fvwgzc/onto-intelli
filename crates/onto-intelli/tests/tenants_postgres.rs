//! Many PostgreSQL tenants in one process: one LISTEN connection for all of
//! them, idle and least-recently-used eviction, a memory budget, and
//! sign-ups that create a schema without opening it. Runs when a test
//! database is set (see `common`).
#![cfg(feature = "postgres")]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use onto_intelli::{Config, Hub, Principal};

fn config(url: &str, prefix: &str) -> Config {
    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.database_url = Some(url.into());
    cfg.engine.tenancy = "multi".into();
    cfg.engine.default_workspace = "acme".into();
    cfg.engine.postgres.schema_prefix = prefix.into();
    // Traces and the connector scheduler have nothing to do with this.
    cfg.traces.enabled = false;
    cfg.connectors.background = false;
    cfg
}

async fn cleanup(hub: &Hub, url: &str, prefix: &str) {
    for t in hub.known_tenants().unwrap_or_default() {
        common::drop_tenant(url, prefix, &t);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hundred_and_twenty_tenants_share_a_pool_of_four() {
    let Some(url) = common::database_url("a_hundred_and_twenty_tenants_share_a_pool_of_four") else { return };
    let prefix = common::unique("o120_");
    let mut cfg = config(&url, &prefix);
    // Before the shared LISTEN, every open tenant held one of these four
    // connections forever: the fifth tenant waited for one and timed out.
    cfg.engine.postgres.max_connections = 4;
    cfg.engine.postgres.connect_timeout_secs = 5;
    cfg.engine.max_open_tenants = 0;
    let hub = Hub::open(cfg).await.unwrap();
    let admin = Principal::admin("ops");
    let started = Instant::now();
    for n in 0..120 {
        let t = hub.tenant(&format!("t{n:03}")).await.unwrap();
        t.call(&admin, "entity.upsert", json!({ "id": format!("team:t{n}"), "title": format!("Team {n}") }))
            .await
            .unwrap();
        assert!(t.call(&admin, "entity.get", json!({ "id": format!("team:t{n}") })).await.is_ok());
    }
    assert_eq!(hub.open_tenants().len(), 120);
    assert_eq!(hub.listening_stores(), Some(120), "every open tenant listens, on one connection");
    // Every tenant still answers once all are open.
    let t0 = hub.tenant("t000").await.unwrap();
    assert!(t0.call(&admin, "entity.get", json!({ "id": "team:t0" })).await.is_ok());
    eprintln!("opened 120 tenants on 4 connections in {:?}", started.elapsed());
    cleanup(&hub, &url, &prefix).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commits_reach_other_processes_through_the_shared_listener() {
    let Some(url) = common::database_url("commits_reach_other_processes_through_the_shared_listener") else { return };
    let prefix = common::unique("onot_");
    let mut cfg = config(&url, &prefix);
    // Polling alone would take a minute: only NOTIFY can be this fast.
    cfg.engine.follow_interval_ms = 60_000;
    let a = Hub::open(cfg.clone()).await.unwrap();
    let b = Hub::open(cfg).await.unwrap();
    let admin = Principal::admin("ops");
    let (a1, b1) = (a.tenant("one").await.unwrap(), b.tenant("one").await.unwrap());
    let b2 = b.tenant("two").await.unwrap();
    a1.call(&admin, "entity.upsert", json!({ "id": "team:edge", "title": "Edge" })).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while b1.call(&admin, "entity.get", json!({ "id": "team:edge" })).await.is_err() {
        assert!(Instant::now() < deadline, "b never saw a's commit");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(b2.call(&admin, "entity.get", json!({ "id": "team:edge" })).await.is_err(), "another tenant's commit");
    cleanup(&a, &url, &prefix).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_least_recently_used_tenants_close_past_the_limit() {
    let Some(url) = common::database_url("the_least_recently_used_tenants_close_past_the_limit") else { return };
    let prefix = common::unique("olru_");
    let mut cfg = config(&url, &prefix);
    cfg.engine.max_open_tenants = 10;
    let hub = Hub::open(cfg).await.unwrap();
    let admin = Principal::admin("ops");
    for n in 0..30 {
        let t = hub.tenant(&format!("t{n:02}")).await.unwrap();
        t.call(&admin, "entity.upsert", json!({ "id": "team:x", "title": format!("X {n}") })).await.unwrap();
        // Different times, so least recently used is well defined.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let open = hub.open_tenants();
    assert_eq!(open.len(), 10, "{open:?}");
    assert!(open.iter().all(|t| t.as_str() >= "t20"), "the oldest closed first: {open:?}");
    // A closed tenant opens again, with its data.
    let t0 = hub.tenant("t00").await.unwrap();
    assert_eq!(t0.call(&admin, "entity.get", json!({ "id": "team:x" })).await.unwrap()["title"], "X 0");
    assert!(hub.open_tenants().len() <= 10);
    let deadline = Instant::now() + Duration::from_secs(5);
    while hub.listening_stores().unwrap() > 10 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(hub.listening_stores().unwrap() <= 10, "closed tenants stop listening: {:?}", hub.listening_stores());
    cleanup(&hub, &url, &prefix).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_tenants_close_and_a_memory_budget_is_kept() {
    let Some(url) = common::database_url("idle_tenants_close_and_a_memory_budget_is_kept") else { return };
    let prefix = common::unique("oidl_");
    let mut cfg = config(&url, &prefix);
    cfg.engine.max_open_tenants = 0;
    cfg.engine.tenant_idle_secs = 600;
    let hub = Hub::open(cfg.clone()).await.unwrap();
    let admin = Principal::admin("ops");
    for t in ["a", "b", "c"] {
        let i = hub.tenant(t).await.unwrap();
        i.call(&admin, "entity.upsert", json!({ "id": "team:x", "title": "X" })).await.unwrap();
    }
    let stats = hub.tenant_stats();
    assert_eq!(stats.len(), 3);
    assert!(stats.iter().all(|s| s.estimated_bytes > 0 && s.entities >= 1), "{stats:?}");
    // Nothing is idle yet; ten minutes later, all are.
    let now = onto_intelli::core::time::now_ms();
    assert!(hub.enforce_limits(None, now).is_empty());
    let mut closed = hub.enforce_limits(None, now + 600_000);
    closed.sort();
    assert_eq!(closed, vec!["a", "b", "c"]);
    assert!(hub.open_tenants().is_empty());

    // A budget below what three tenants take keeps the most recently used one.
    cfg.engine.tenant_idle_secs = 0;
    cfg.engine.tenant_memory_budget_mb = 1;
    let hub = Hub::open(cfg).await.unwrap();
    let big: Vec<serde_json::Value> = (0..3000)
        .map(|n| json!({ "op": "upsert", "kind": "team", "key": format!("k{n}"), "title": format!("Team {n} {}", "x".repeat(200)) }))
        .collect();
    for t in ["a", "b", "c"] {
        let i = hub.tenant(t).await.unwrap();
        i.call(&admin, "graph.apply", json!({ "mutations": big })).await.unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let each = hub.tenant_stats()[0].estimated_bytes;
    assert!(each > 1 << 20, "a tenant with 3000 large entities is over 1 MiB: {each}");
    hub.enforce_limits(None, onto_intelli::core::time::now_ms());
    assert_eq!(
        hub.open_tenants(),
        vec!["c"],
        "over the budget, the least recently used close: {:?}",
        hub.tenant_stats()
    );
    cleanup(&hub, &url, &prefix).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_provisioned_tenant_is_not_open() {
    let Some(url) = common::database_url("a_provisioned_tenant_is_not_open") else { return };
    let prefix = common::unique("oprv_");
    let hub: Arc<Hub> = Hub::open(config(&url, &prefix)).await.unwrap();
    hub.provision("signup_1").await.unwrap();
    assert!(hub.known_tenants().unwrap().contains(&"signup_1".to_string()), "the schema exists");
    assert!(hub.open_tenants().is_empty(), "nothing in memory");
    assert_eq!(hub.listening_stores(), Some(0), "nothing listens");
    assert!(hub.provision("../x").await.is_err());
    cleanup(&hub, &url, &prefix).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn look_alike_tenant_ids_never_share_a_schema() {
    let Some(url) = common::database_url("look_alike_tenant_ids_never_share_a_schema") else { return };
    let prefix = common::unique("olk_");
    let hub = Hub::open(config(&url, &prefix)).await.unwrap();
    let admin = Principal::admin("ops");
    let acme = hub.tenant("Acme").await.unwrap();
    acme.call(&admin, "entity.upsert", json!({ "id": "team:secret", "title": "Secret" })).await.unwrap();
    // Before, "acme" opened the same schema as "Acme" and read its data.
    for other in ["acme", "ACME"] {
        let err = hub.tenant(other).await.err().unwrap_or_else(|| panic!("{other} opened Acme's schema"));
        assert!(matches!(err, onto_intelli::Error::Conflict(ref m) if m.contains("belongs to tenant 'Acme'")), "{err}");
    }
    hub.tenant("a-b").await.unwrap();
    for other in ["a_b", "a.b", "A-B"] {
        assert!(hub.tenant(other).await.is_err(), "{other} shares a-b's schema");
    }
    // Dropping through a look-alike id is refused; the owner's id works.
    assert!(hub.drop_tenant("acme").is_err());
    assert!(acme.call(&admin, "entity.get", json!({ "id": "team:secret" })).await.is_ok());
    hub.drop_tenant("Acme").unwrap();
    cleanup(&hub, &url, &prefix).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_audit_trail_refuses_delete_except_to_retention() {
    use onto_intelli::core::sqlx;
    let Some(url) = common::database_url("the_audit_trail_refuses_delete_except_to_retention") else { return };
    let prefix = common::unique("oaud_");
    let tenant = "audit";
    let store = onto_intelli::core::store::PgStore::connect(
        &url,
        tenant,
        onto_intelli::core::store::PgOptions { schema_prefix: prefix.clone(), listen: false, ..Default::default() },
    )
    .unwrap();
    let hub = Hub::open(config(&url, &prefix)).await.unwrap();
    let i = hub.tenant(tenant).await.unwrap();
    let admin = Principal::admin("ops");
    i.call(&admin, "entity.upsert", json!({ "id": "team:x", "title": "X" })).await.unwrap();
    i.call(&admin, "event.append", json!({ "action": "deploy.done", "target": "team:x" })).await.unwrap();
    i.call(&admin, "entity.patch", json!({ "id": "team:x", "title": "X2" })).await.unwrap();
    i.call(&admin, "entity.upsert", json!({ "id": "person:y", "title": "Y" })).await.unwrap();
    i.call(&admin, "link.create", json!({ "from": "person:y", "rel": "member_of", "to": "team:x" })).await.unwrap();
    let schema = store.schema_name().to_string();
    let pool = store.pool().clone();
    for table in ["events", "entity_versions", "link_versions"] {
        for stmt in [format!("DELETE FROM \"{schema}\".{table}"), format!("TRUNCATE \"{schema}\".{table}")] {
            let err = sqlx::query(sqlx::AssertSqlSafe(stmt.clone())).execute(&pool).await.unwrap_err();
            assert!(err.to_string().contains("append-only"), "{stmt}: {err}");
        }
    }
    // Retention still deletes old events.
    let before: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM \"{schema}\".events")))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(before > 0);
    let (_, deleted) = tokio::task::spawn_blocking(move || store.vacuum(Some(-60_000))).await.unwrap().unwrap();
    assert_eq!(deleted as i64, before);
    cleanup(&hub, &url, &prefix).await;
}
