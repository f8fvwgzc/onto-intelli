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

/// Streams keep a tenant from going idle (their heartbeats count as use),
/// never past `max_open_tenants`: the evicted tenant's stream ends
/// `tenant_closed` and the next request opens the only ontology for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_count_as_use_but_never_past_max_open_tenants() {
    use onto_intelli::changes::{Frame, Start};
    use onto_intelli::core::changes::ChangeFilter;

    let Some(url) = common::database_url("streams_count_as_use_but_never_past_max_open_tenants") else { return };
    let prefix = common::unique("ostr_");
    let mut cfg = config(&url, &prefix);
    cfg.engine.tenant_idle_secs = 2;
    cfg.engine.max_open_tenants = 0;
    cfg.changes.heartbeat_secs = 1;
    let hub = Hub::open(cfg).await.unwrap();
    let admin = Principal::admin("x");
    let streamed = hub.tenant("streamed").await.unwrap();
    // Subscribed before the next open, which can take seconds on a loaded
    // database: the stream's heartbeats, not the open, keep it in use.
    let sub = streamed.subscribe_changes(admin.clone(), ChangeFilter::default(), Start::Now, None).await.unwrap();
    hub.tenant("quiet").await.unwrap();
    // Both were last requested now; after the idle time only the one with a stream stays.
    tokio::time::sleep(Duration::from_millis(3200)).await;
    // (The hub's sweeper may have closed the quiet one already.)
    hub.enforce_limits(None, onto_intelli::core::time::now_ms());
    assert_eq!(hub.open_tenants(), vec!["streamed"], "the stream's heartbeats keep its tenant from going idle");
    assert!(streamed.stream_touched() > 0);

    // max_open_tenants judges the last request only.
    drop(hub);
    let mut cfg = config(&url, &prefix);
    cfg.engine.max_open_tenants = 1;
    cfg.changes.heartbeat_secs = 1;
    let hub = Hub::open(cfg).await.unwrap();
    let a = hub.tenant("a").await.unwrap();
    a.call(&admin, "entity.upsert", json!({ "id": "task:a1", "title": "a" })).await.unwrap();
    let mut sub_a = a.subscribe_changes(admin.clone(), ChangeFilter::default(), Start::Earliest, None).await.unwrap();
    let old = Arc::downgrade(&a);
    drop(a);
    tokio::time::sleep(Duration::from_millis(5)).await;
    hub.tenant("b").await.unwrap();
    assert_eq!(hub.open_tenants(), vec!["b"], "the stream did not keep a open past max_open_tenants");
    let code = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match sub_a.recv().await {
                Some(Frame::End { code, .. }) => return code,
                Some(_) => {}
                None => panic!("the stream ended without an end frame"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(code, "tenant_closed");
    let t = Instant::now();
    while old.strong_count() > 0 && t.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(old.strong_count(), 0, "nothing keeps the evicted tenant's ontology alive");
    let again = hub.tenant("a").await.unwrap();
    let r = again.call(&admin, "entity.get", json!({ "id": "task:a1" })).await.unwrap();
    assert_eq!(r["id"], "task:a1");
    drop(sub);
    cleanup(&hub, &url, &prefix).await;
}

/// Poll `f` every 50 ms for up to `secs`.
async fn eventually(secs: u64, f: impl Fn() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f()
}

/// Another process gives `tenant` a timer due `delay_ms` from now: its wake
/// row is bumped in the same transaction.
async fn work_due(url: &str, prefix: &str, tenant: &str, delay_ms: i64) {
    let (url, prefix, tenant) = (url.to_string(), prefix.to_string(), tenant.to_string());
    tokio::task::spawn_blocking(move || {
        use onto_intelli::core::store::{PgOptions, PgStore, Store};
        let store =
            PgStore::connect(&url, &tenant, PgOptions { schema_prefix: prefix, listen: false, ..Default::default() })
                .unwrap();
        store.timers_set("retry:t", delay_ms, &json!({})).unwrap();
    })
    .await
    .unwrap();
}

/// Of two hubs standing for the deployment's wake scanner, exactly one is
/// elected. It opens a closed tenant whose work is due (not one due later);
/// the other opens nothing. When the elected hub goes away, the other is
/// elected and opens the next due tenant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_wake_scanner_per_deployment_opens_due_tenants() {
    let Some(url) = common::database_url("one_wake_scanner_per_deployment_opens_due_tenants") else { return };
    let prefix = common::unique("owake_");
    let mut cfg = config(&url, &prefix);
    cfg.engine.wake_poll_secs = 1;
    assert!(!Hub::open(Config::in_memory()).await.unwrap().start_wake_scanner(), "memory tenants never close");
    let mut off = cfg.clone();
    off.engine.wake_poll_secs = 0;
    assert!(!Hub::open(off).await.unwrap().start_wake_scanner(), "wake_poll_secs = 0: no scanner");

    let h1 = Hub::open(cfg.clone()).await.unwrap();
    let h2 = Hub::open(cfg.clone()).await.unwrap();
    assert!(h1.start_wake_scanner() && h2.start_wake_scanner());
    assert!(h1.start_wake_scanner(), "standing twice is one candidate");
    let elected = |a: &Hub, b: &Hub| u8::from(a.wake_scanner_elected()) + u8::from(b.wake_scanner_elected());
    assert!(eventually(10, || elected(&h1, &h2) == 1).await, "one scanner is elected");
    // Both keep standing every poll; the lock keeps it at one.
    for _ in 0..30 {
        assert_eq!(elected(&h1, &h2), 1);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (winner, loser) = if h1.wake_scanner_elected() { (h1, h2) } else { (h2, h1) };

    work_due(&url, &prefix, "sleepy", 0).await;
    work_due(&url, &prefix, "later", 600_000).await;
    assert!(
        eventually(10, || winner.open_tenants().contains(&"sleepy".to_string())).await,
        "the elected scanner opened the tenant with work due: {:?}",
        winner.open_tenants()
    );
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(winner.open_tenants(), vec!["sleepy"], "not the tenant due in ten minutes");
    assert!(loser.open_tenants().is_empty(), "only the elected scanner opens tenants");

    // The elected process goes away: its lock goes with its session.
    drop(winner);
    assert!(eventually(10, || loser.wake_scanner_elected()).await, "the other candidate takes over");
    work_due(&url, &prefix, "sleepy2", 0).await;
    assert!(
        eventually(10, || loser.open_tenants().contains(&"sleepy2".to_string())).await,
        "{:?}",
        loser.open_tenants()
    );

    cleanup(&loser, &url, &prefix).await;
    drop(loser);
    let pool = onto_intelli::core::sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let sys = onto_intelli::core::store::postgres::sys_schema(&prefix);
    onto_intelli::core::sqlx::query(onto_intelli::core::sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS \"{sys}\" CASCADE"
    )))
    .execute(&pool)
    .await
    .unwrap();
}

/// `GET /v1/admin/tenants` counters (docs/design/EVENTS.md section 11) on
/// PostgreSQL: per open tenant, change lag per consumer and endpoint, gap
/// records over 24 h with the latest, outbox rows per state and paused
/// counts; the plain stats (memory budget, eviction) leave them out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tenant_report_carries_outbound_counters() {
    let Some(url) = common::database_url("the_tenant_report_carries_outbound_counters") else { return };
    let prefix = common::unique("orep_");
    let mut cfg = config(&url, &prefix);
    cfg.webhooks.enabled = false;
    cfg.automation.workers = false;
    let hub = Hub::open(cfg).await.unwrap();
    let admin = Principal::admin("ops");
    let i = hub.tenant("a").await.unwrap();
    i.call(&admin, "entity.upsert", json!({ "id": "team:x", "title": "X" })).await.unwrap();
    let store = i.ontology().store().clone();
    // An endpoint `ops` (the counters report only existing endpoints' cursors).
    let ep = onto_intelli::webhooks::Endpoint {
        name: "ops".into(),
        url: "https://hooks.example.com/in".into(),
        secret: onto_intelli::webhooks::SecretRef::Env("ONTO_TEST_UNSET_REPORT_SECRET".into()),
        previous_secret: None,
        previous_until: None,
        filter: None,
        principal: onto_intelli::core::policy::ScopePrincipal::new(vec![onto_intelli::Role::Viewer], vec!["*".into()]),
        enabled: false,
        paused: None,
        restricted: false,
        version: 1,
        created_by: "ops".into(),
        created_at: 0,
        incarnation: String::new(),
    };
    let kv = vec![(onto_intelli::webhooks::NS_ENDPOINT.to_string(), "ops".to_string(), json!(ep))];
    let ctx = onto_intelli::ActionContext::new(admin.clone(), "acme");
    let ev = onto_intelli::core::outbox::ConfigEvent::new("webhook.add", Some("ops"), json!({}));
    i.ontology().commit_config(&ctx, kv, ev).unwrap();
    store.consumer_set("webhook:ops", onto_intelli::core::Cursor::new(0, 0), None).unwrap();
    let mut y = i.ontology().get(&admin, "team:x").unwrap();
    y.id = "team:y".into();
    store.bulk_import(&[y], &[]).unwrap();
    let head = store.head().unwrap();

    assert!(hub.tenant_stats().iter().all(|t| t.outbound.is_none()));
    let h = hub.clone();
    let report = h.tenant_report().await;
    assert_eq!(report.len(), 1);
    let o = report[0].outbound.as_ref().expect("counters");
    assert_eq!(o["lag"]["endpoints"]["ops"]["behind"].as_u64(), Some(head), "{o}");
    assert!(o["lag"]["endpoints"]["ops"]["updated_at"].is_i64(), "{o}");
    assert_eq!(o["gaps_24h"]["count"], 1, "{o}");
    assert_eq!(o["gaps_24h"]["latest"]["cursor"], format!("{head}.0"), "{o}");
    assert_eq!(o["outbox"]["targets"], json!({}), "{o}");
    assert!(o["runs_1h"]["states"].is_object(), "{o}");
    assert_eq!(o["paused"]["endpoints"], 0, "{o}");
    let h = i.call(&admin, "system.health", json!({})).await.unwrap();
    assert_eq!(h["outbound"]["changes"]["latest_gap"]["reason"], "bulk_import", "{h}");
    drop(i);
    cleanup(&hub, &url, &prefix).await;
}
