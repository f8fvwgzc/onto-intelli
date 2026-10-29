//! Webhook delivery on PostgreSQL (when a test database is set): two hubs
//! (two processes' worth of delivery workers) on one tenant send each row
//! once while nothing fails, a retry is due by the database clock, and the
//! tenant's wake row settles once nothing is pending. Fan-out of a change
//! subscription with writers in both processes enqueues each change once
//! (one fan-out holder at a time, fenced; the cursor is the dedupe key), and
//! commits wake the tenant while it has a subscription. A traced write's
//! trace id is kept in its change records and outbox rows, sent as
//! `traceparent`, and a failed attempt is a `delivery` trace.
#![cfg(feature = "postgres")]

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use parking_lot::Mutex;
use serde_json::json;

use onto_intelli::config::AllowConfig;
use onto_intelli::core::outbox::{ConfigEvent, OutboxQuery, OutboxState};
use onto_intelli::core::policy::ScopePrincipal;
use onto_intelli::trace::TraceCtx;
use onto_intelli::webhooks::{self, Endpoint, NS_ENDPOINT, SecretRef};
use onto_intelli::{ActionContext, Config, Hub, Principal, Role};

#[derive(Clone, Default)]
struct Receiver {
    ids: Arc<Mutex<Vec<String>>>,
    traceparents: Arc<Mutex<Vec<String>>>,
    /// Statuses to answer with first (then 200).
    script: Arc<Mutex<Vec<u16>>>,
}

async fn handle(State(r): State<Receiver>, headers: HeaderMap, _body: Bytes) -> StatusCode {
    let id = headers.get("webhook-id").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    r.ids.lock().push(id);
    let tp = headers.get("traceparent").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    r.traceparents.lock().push(tp);
    let mut s = r.script.lock();
    StatusCode::from_u16(if s.is_empty() { 200 } else { s.remove(0) }).unwrap()
}

async fn receiver(script: Vec<u16>) -> (Receiver, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let r =
        Receiver { ids: Default::default(), traceparents: Default::default(), script: Arc::new(Mutex::new(script)) };
    let app = axum::Router::new().fallback(handle).with_state(r.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (r, port)
}

fn config(url: &str, prefix: &str) -> Config {
    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.database_url = Some(url.into());
    cfg.engine.tenancy = "multi".into();
    cfg.engine.default_workspace = "acme".into();
    cfg.engine.postgres.schema_prefix = prefix.into();
    cfg.traces.enabled = false;
    cfg.connectors.background = false;
    cfg.webhooks.poll_secs = 1;
    cfg.webhooks.backoff_base_secs = 1;
    cfg.webhooks.backoff_max_secs = 1;
    cfg.webhooks.allow = vec![AllowConfig { host: "127.0.0.1".into(), cidrs: vec!["127.0.0.1/32".into()] }];
    cfg
}

async fn eventually(secs: u64, f: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_processes_deliver_each_row_once_and_settle_the_wake_row() {
    let Some(url) = common::database_url("two_processes_deliver_each_row_once") else { return };
    let prefix = common::unique("owh_");
    let (rx, port) = receiver(vec![500]).await;
    let a = Hub::open(config(&url, &prefix)).await.unwrap();
    let b = Hub::open(config(&url, &prefix)).await.unwrap();
    let (ta, tb) = (a.tenant("hooks").await.unwrap(), b.tenant("hooks").await.unwrap());

    let dir = std::env::temp_dir().join("onto-intelli-webhook-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("pg-{}", onto_intelli::core::time::new_id("s")));
    std::fs::write(&path, "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
    let ep = Endpoint {
        name: "ops".into(),
        url: format!("http://127.0.0.1:{port}/in"),
        secret: SecretRef::File(path),
        previous_secret: None,
        previous_until: None,
        filter: None,
        principal: ScopePrincipal::new(vec![Role::Viewer], vec!["*".into()]),
        enabled: true,
        paused: None,
        restricted: false,
        version: 1,
        created_by: "lead".into(),
        created_at: 0,
        incarnation: String::new(),
    };
    let admin = ActionContext::new(Principal::admin("lead"), "acme");
    let kv = vec![(NS_ENDPOINT.to_string(), "ops".to_string(), serde_json::to_value(&ep).unwrap())];
    ta.ontology().commit_config(&admin, kv, ConfigEvent::new("webhook.add", Some("ops"), json!({}))).unwrap();

    let ids: Vec<String> = (0..20)
        .map(|_| webhooks::enqueue_test(ta.ontology(), &admin, "ops").unwrap()["id"].as_str().unwrap().to_string())
        .collect();
    tb.wake_outbox();
    let store = tb.ontology().store().clone();
    let all_delivered = || {
        let rows = store.outbox_list(&OutboxQuery { target: Some("ops".into()), ..Default::default() }).unwrap();
        rows.len() == 20 && rows.iter().all(|r| r.state == OutboxState::Delivered)
    };
    assert!(eventually(20, all_delivered).await, "{:?}", rx.ids.lock());
    let got = rx.ids.lock().clone();
    // One 500 then a retry of that row: 21 requests, 20 distinct ids.
    assert_eq!(got.len(), 21, "{got:?}");
    let distinct: HashSet<&String> = got.iter().collect();
    assert_eq!(distinct.len(), 20);
    assert!(ids.iter().all(|i| distinct.contains(i)));
    let retried = ids.iter().find(|i| got.iter().filter(|g| g == i).count() == 2).expect("one row was retried");
    let log = store.outbox_attempts(retried, 10).unwrap();
    assert_eq!(log.iter().map(|a| a.result.status).collect::<Vec<_>>(), vec![Some(200), Some(500)]);
    // Nothing pending: the wake row settles (no due time).
    let settled = || store.wake_get().ok().flatten().is_some_and(|w| w.due_at.is_none());
    assert!(eventually(10, settled).await, "{:?}", store.wake_get());

    drop((ta, tb, a, b));
    common::drop_tenant(&url, &prefix, "hooks");
    let pool = onto_intelli::core::sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let sys = onto_intelli::core::store::postgres::sys_schema(&prefix);
    onto_intelli::core::sqlx::query(onto_intelli::core::sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS \"{sys}\" CASCADE"
    )))
    .execute(&pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_processes_fan_out_each_change_once() {
    let Some(url) = common::database_url("two_processes_fan_out_each_change_once") else { return };
    let prefix = common::unique("ofo_");
    let (rx, port) = receiver(vec![]).await;
    let a = Hub::open(config(&url, &prefix)).await.unwrap();
    let b = Hub::open(config(&url, &prefix)).await.unwrap();
    let (ta, tb) = (a.tenant("fan").await.unwrap(), b.tenant("fan").await.unwrap());
    let dir = std::env::temp_dir().join("onto-intelli-webhook-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("pg-fan-{}", onto_intelli::core::time::new_id("s")));
    std::fs::write(&path, "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
    let lead = Principal::admin("lead");
    ta.call(
        &lead,
        "webhook.add",
        json!({
            "name": "tasks", "url": format!("http://127.0.0.1:{port}/in"), "secret": format!("file:{}", path.display()),
            "filter": { "kinds": "task" }, "principal": { "roles": "viewer", "workspaces": "acme" },
        }),
    )
    .await
    .unwrap();
    let store = tb.ontology().store().clone();
    // Writers in both processes; the subscription wakes the tenant on commits.
    let bumped = store.wake_get().unwrap().map_or(0, |w| w.bumped);
    for n in 0..10 {
        let (t, who) = if n % 2 == 0 { (&ta, "a") } else { (&tb, "b") };
        t.call(&lead, "entity.upsert", json!({ "id": format!("task:{who}{n}"), "title": "T", "workspace": "acme" }))
            .await
            .unwrap();
    }
    assert!(store.wake_get().unwrap().is_some_and(|w| w.bumped > bumped), "commits wake a subscribed tenant");
    let delivered = || {
        let rows = store.outbox_list(&OutboxQuery { target: Some("tasks".into()), ..Default::default() }).unwrap();
        rows.len() == 10 && rows.iter().all(|r| r.state == OutboxState::Delivered)
    };
    assert!(eventually(20, delivered).await, "{:?}", rx.ids.lock());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rows = store.outbox_list(&OutboxQuery { target: Some("tasks".into()), ..Default::default() }).unwrap();
    let keys: HashSet<&str> = rows.iter().map(|r| r.dedupe.as_str()).collect();
    assert_eq!((rows.len(), keys.len()), (10, 10), "each change enqueued once");
    let got = rx.ids.lock().clone();
    assert_eq!(got.iter().collect::<HashSet<_>>().len(), 10, "{got:?}");
    let lease = store.lease_get(webhooks::FANOUT_LEASE).unwrap().expect("the fan-out lease was taken");
    assert!(lease.epoch >= 1);

    drop((ta, tb, a, b));
    common::drop_tenant(&url, &prefix, "fan");
    let pool = onto_intelli::core::sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let sys = onto_intelli::core::store::postgres::sys_schema(&prefix);
    onto_intelli::core::sqlx::query(onto_intelli::core::sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS \"{sys}\" CASCADE"
    )))
    .execute(&pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_traced_write_is_delivered_under_its_trace_on_postgres() {
    const OTEL: &str = "0af7651916cd43dd8448eb211c80319c";
    let Some(url) = common::database_url("a_traced_write_is_delivered_under_its_trace") else { return };
    let prefix = common::unique("otr_");
    let (rx, port) = receiver(vec![500]).await;
    let mut cfg = config(&url, &prefix);
    cfg.traces.enabled = true;
    let hub = Hub::open(cfg).await.unwrap();
    let t = hub.tenant("traced").await.unwrap();
    let dir = std::env::temp_dir().join("onto-intelli-webhook-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("pg-trace-{}", onto_intelli::core::time::new_id("s")));
    std::fs::write(&path, "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
    let lead = Principal::admin("lead");
    t.call(
        &lead,
        "webhook.add",
        json!({
            "name": "tasks", "url": format!("http://127.0.0.1:{port}/in"), "secret": format!("file:{}", path.display()),
            "filter": { "kinds": "task" }, "principal": { "roles": "viewer", "workspaces": "acme" },
        }),
    )
    .await
    .unwrap();
    let ctx = TraceCtx::from_source("http").with_traceparent(Some(&format!("00-{OTEL}-b7ad6b7169203331-01")));
    let input = json!({ "id": "task:x", "title": "T", "workspace": "acme" });
    t.call_traced(&lead, "entity.upsert", input, ctx).await.result.unwrap();
    let store = t.ontology().store().clone();
    let delivered = || {
        let rows = store.outbox_list(&OutboxQuery { target: Some("tasks".into()), ..Default::default() }).unwrap();
        rows.len() == 1 && rows[0].state == OutboxState::Delivered
    };
    assert!(eventually(20, delivered).await, "{:?}", rx.ids.lock());
    let rows = store.outbox_list(&OutboxQuery { target: Some("tasks".into()), ..Default::default() }).unwrap();
    assert_eq!(rows[0].otel_trace_id.as_deref(), Some(OTEL));
    let page = store.changes_since(Default::default(), 100).unwrap();
    let rec = page.changes.iter().find(|(s, _)| s.record.id == "task:x").expect("the change").0.record.clone();
    assert_eq!(rec.cause.trace_id.as_deref(), Some(OTEL));
    let tps = rx.traceparents.lock().clone();
    assert_eq!(tps.len(), 2, "a 500, then the retry");
    assert!(tps.iter().all(|tp| tp.starts_with(&format!("00-{OTEL}-"))), "{tps:?}");
    // The failed attempt is a delivery trace in the tenant's trace tables.
    t.flush_traces().await;
    let list = t.call(&lead, "trace.list", json!({ "kind": "delivery" })).await.unwrap();
    let traces = list["traces"].as_array().unwrap();
    assert_eq!(traces.len(), 1, "{list}");
    assert_eq!((traces[0]["otel_trace_id"].as_str(), traces[0]["error_code"].as_str()), (Some(OTEL), Some("retry")));
    let d = t.call(&lead, "trace.get", json!({ "id": traces[0]["id"] })).await.unwrap();
    let root = d["spans"].as_array().unwrap().iter().find(|s| s["parent_id"].is_null()).unwrap().clone();
    assert_eq!(root["id"].as_str(), Some(&tps[0][36..52]));
    assert!(!d.to_string().contains("whsec_") && !d.to_string().contains(&format!(":{port}")), "{d}");

    drop((t, hub));
    common::drop_tenant(&url, &prefix, "traced");
    let pool = onto_intelli::core::sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let sys = onto_intelli::core::store::postgres::sys_schema(&prefix);
    onto_intelli::core::sqlx::query(onto_intelli::core::sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS \"{sys}\" CASCADE"
    )))
    .execute(&pool)
    .await
    .unwrap();
}
