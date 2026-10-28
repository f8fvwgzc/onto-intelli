//! Webhook delivery on PostgreSQL (when a test database is set): two hubs
//! (two processes' worth of delivery workers) on one tenant send each row
//! once while nothing fails, a retry is due by the database clock, and the
//! tenant's wake row settles once nothing is pending. Fan-out of a change
//! subscription with writers in both processes enqueues each change once
//! (one fan-out holder at a time, fenced; the cursor is the dedupe key), and
//! commits wake the tenant while it has a subscription.
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
use onto_intelli::webhooks::{self, Endpoint, NS_ENDPOINT, SecretRef};
use onto_intelli::{ActionContext, Config, Hub, Principal, Role};

#[derive(Clone, Default)]
struct Receiver {
    ids: Arc<Mutex<Vec<String>>>,
    /// Statuses to answer with first (then 200).
    script: Arc<Mutex<Vec<u16>>>,
}

async fn handle(State(r): State<Receiver>, headers: HeaderMap, _body: Bytes) -> StatusCode {
    let id = headers.get("webhook-id").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    r.ids.lock().push(id);
    let mut s = r.script.lock();
    StatusCode::from_u16(if s.is_empty() { 200 } else { s.remove(0) }).unwrap()
}

async fn receiver(script: Vec<u16>) -> (Receiver, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let r = Receiver { ids: Default::default(), script: Arc::new(Mutex::new(script)) };
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
