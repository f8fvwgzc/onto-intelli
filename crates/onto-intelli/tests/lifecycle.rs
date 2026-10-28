//! A graceful shutdown releases what a process holds (docs/design/EVENTS.md
//! section 9; crate::lifecycle): the fan-out and automation consumer
//! leases, fenced by the epoch they were taken at, and the wake scanner
//! lock, so another instance takes over at once instead of waiting out the
//! 60 s lease. Consumer cursors stay, so the next holder resumes where the
//! old one stopped: each change is enqueued once across the handover.

mod common;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use onto_intelli::config::AllowConfig;
use onto_intelli::core::leases::Fence;
use onto_intelli::core::outbox::OutboxQuery;
use onto_intelli::core::runs::CONSUMER_LEASE;
use onto_intelli::webhooks::FANOUT_LEASE;
use onto_intelli::{Config, Intelli, Principal};

fn lead() -> Principal {
    Principal::admin("lead")
}

async fn ok(i: &Intelli, action: &str, input: Value) -> Value {
    i.call(&lead(), action, input.clone()).await.unwrap_or_else(|e| panic!("{action} {input}: {e}"))
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

/// A local receiver that answers 200 to everything; returns its port.
async fn receiver() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new().fallback(|| async { axum::http::StatusCode::OK });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

fn secret_file() -> String {
    let dir = std::env::temp_dir().join("onto-intelli-lifecycle-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("secret-{}", onto_intelli::core::time::new_id("s")));
    std::fs::write(&path, "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
    format!("file:{}", path.display())
}

fn tune(cfg: &mut Config) {
    cfg.engine.default_workspace = "acme".into();
    cfg.traces.enabled = false;
    cfg.connectors.background = false;
    cfg.webhooks.poll_secs = 1;
    cfg.webhooks.allow = vec![AllowConfig { host: "127.0.0.1".into(), cidrs: vec!["127.0.0.1/32".into()] }];
}

/// A change subscription (the fan-out takes its lease) and an enabled
/// automation (the consumer takes its lease).
async fn subscribe_and_automate(i: &Intelli, port: u16, secret: &str) {
    ok(
        i,
        "webhook.add",
        json!({
            "name": "tasks", "url": format!("http://127.0.0.1:{port}/in"), "secret": secret,
            "filter": { "kinds": "task" }, "principal": { "roles": "viewer", "workspaces": "acme" },
        }),
    )
    .await;
    let def = json!({
        "description": "Resolve a blocked issue.",
        "run_as": { "roles": ["member"], "workspaces": ["acme"] },
        "trigger": { "on": "change", "kinds": ["issue"], "ops": ["entity.update"], "state": { "to": ["blocked"] } },
        "effects": [{ "action": { "type": "resolve", "params": { "issue": "{trigger.id}" } } }]
    });
    ok(i, "schema.apply", json!({ "changes": [{ "op": "put_automation", "name": "unblock", "def": def }] })).await;
    ok(i, "automation.enable", json!({ "name": "unblock" })).await;
}

async fn task(i: &Intelli, key: &str) {
    ok(i, "entity.upsert", json!({ "id": format!("task:{key}"), "title": format!("Task {key}"), "workspace": "acme" }))
        .await;
}

fn held_by_me(i: &Intelli, name: &str) -> Option<Fence> {
    i.held_leases().into_iter().find(|f| f.name == name)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_a_shutdown_releases_its_leases_and_the_next_holder_takes_them_at_once() {
    let port = receiver().await;
    let mut cfg = Config::in_memory();
    tune(&mut cfg);
    let i = Intelli::open(cfg).await.unwrap();
    subscribe_and_automate(&i, port, &secret_file()).await;
    task(&i, "1").await;
    let store = i.ontology().store().clone();
    let both = || held_by_me(&i, FANOUT_LEASE).is_some() && held_by_me(&i, CONSUMER_LEASE).is_some();
    assert!(eventually(10, both).await, "the workers took their leases: {:?}", i.held_leases());
    let fences = i.held_leases();
    let cursor = store.consumer_get("webhook:tasks").unwrap();
    assert!(cursor.is_some(), "fan-out keeps the endpoint's cursor");

    let released = i.shutdown(Duration::from_secs(10)).await;
    assert_eq!(released.leases, 2, "{released:?}");
    assert_eq!(released.cut, 0, "the workers ended their round: {released:?}");
    assert!(i.held_leases().is_empty());
    for f in &fences {
        let row = store.lease_get(&f.name).unwrap().unwrap();
        assert!(!row.held, "{} was released: {row:?}", f.name);
        // The next holder (another process on PostgreSQL; in memory every
        // instance of this process is one holder) takes it at once.
        let took = Instant::now();
        assert_eq!(store.lease(&f.name, "replacement", 60_000).unwrap(), Some(f.epoch + 1), "{}", f.name);
        assert!(took.elapsed() < Duration::from_secs(1));
        // The old term's fence releases nothing now.
        assert!(!store.release(f).unwrap(), "a stale holder cannot release a newer epoch");
        assert!(store.lease_get(&f.name).unwrap().unwrap().held);
    }
    let after = store.consumer_get("webhook:tasks").unwrap();
    assert!(after.is_some() && after >= cursor, "the cursor stays where the fan-out left it: {cursor:?} {after:?}");
    // Closed: a second shutdown has nothing left to give back.
    assert_eq!(i.shutdown(Duration::from_secs(1)).await, onto_intelli::Released::default());
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_a_shutdown_hands_leases_and_the_wake_scanner_to_another_instance_at_once() {
    use onto_intelli::Hub;

    let Some(url) = common::database_url("pg_a_shutdown_hands_leases_and_the_wake_scanner") else { return };
    let prefix = common::unique("olc_");
    let config = || {
        let mut cfg = Config::in_memory();
        tune(&mut cfg);
        cfg.engine.storage = "postgres".into();
        cfg.engine.database_url = Some(url.clone());
        cfg.engine.tenancy = "multi".into();
        cfg.engine.postgres.schema_prefix = prefix.clone();
        cfg.engine.wake_poll_secs = 1;
        cfg
    };
    let port = receiver().await;
    let secret = secret_file();
    // Instance A: holds both leases and is the elected wake scanner.
    let a = Hub::open(config()).await.unwrap();
    let ta = a.tenant("life").await.unwrap();
    subscribe_and_automate(&ta, port, &secret).await;
    task(&ta, "a1").await;
    task(&ta, "a2").await;
    assert!(a.start_wake_scanner());
    let both = || held_by_me(&ta, FANOUT_LEASE).is_some() && held_by_me(&ta, CONSUMER_LEASE).is_some();
    assert!(eventually(15, || both() && a.wake_scanner_elected()).await, "A: {:?}", ta.held_leases());
    let store = ta.ontology().store().clone();
    // The task changes enqueued for the endpoint (its filter also lets the
    // schema change of the automation through).
    let tasks = |s: &dyn onto_intelli::core::store::Store| {
        let rows = s.outbox_list(&OutboxQuery { target: Some("tasks".into()), ..Default::default() }).unwrap();
        rows.into_iter().filter(|r| r.event_type.starts_with("onto.entity.")).collect::<Vec<_>>()
    };
    let enqueued = |n: usize| {
        let s = store.clone();
        move || tasks(s.as_ref()).len() == n
    };
    assert!(eventually(15, enqueued(2)).await, "A fanned out its two changes: {:?}", tasks(store.as_ref()));

    // Instance B: its workers and its scanner candidate find everything held.
    let b = Hub::open(config()).await.unwrap();
    let tb = b.tenant("life").await.unwrap();
    assert!(b.start_wake_scanner());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(held_by_me(&tb, FANOUT_LEASE).is_none() && held_by_me(&tb, CONSUMER_LEASE).is_none());
    assert!(!b.wake_scanner_elected(), "A is the elected scanner");
    let fences = ta.held_leases();
    let before = fences.iter().map(|f| (f.name.clone(), f.holder.clone(), f.epoch)).collect::<Vec<_>>();

    let released = a.shutdown(Duration::from_secs(10)).await;
    assert_eq!(released.leases, 2, "{released:?}");
    assert!(!a.wake_scanner_elected());
    assert!(a.tenant("life").await.is_err(), "a stopping hub opens no tenant");
    // B takes over at once: well within a second or two, not after 60 s.
    let handed = Instant::now();
    task(&tb, "b1").await; // wakes B's workers
    let b_both = || held_by_me(&tb, FANOUT_LEASE).is_some() && held_by_me(&tb, CONSUMER_LEASE).is_some();
    assert!(eventually(10, || b_both() && b.wake_scanner_elected()).await, "B: {:?}", tb.held_leases());
    assert!(handed.elapsed() < Duration::from_secs(10), "{:?}", handed.elapsed());
    for (name, holder, epoch) in &before {
        let row = store.lease_get(name).unwrap().unwrap();
        assert!(row.held && row.holder != *holder && row.epoch == epoch + 1, "{name}: {row:?}");
        // A's term is over: its fence releases nothing.
        assert!(!store.release(&Fence::new(name, holder, *epoch)).unwrap(), "{name}");
    }
    // Nothing lost, nothing twice: B resumed at A's cursor.
    task(&tb, "b2").await;
    assert!(eventually(15, enqueued(4)).await, "B fanned out the changes after the handover");
    let rows = tasks(store.as_ref());
    assert_eq!(rows.iter().map(|r| r.dedupe.as_str()).collect::<HashSet<_>>().len(), 4, "{rows:?}");

    b.shutdown(Duration::from_secs(10)).await;
    drop((store, ta, tb, a, b));
    common::drop_tenant(&url, &prefix, "life");
    let pool = onto_intelli::core::sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let sys = onto_intelli::core::store::postgres::sys_schema(&prefix);
    onto_intelli::core::sqlx::query(onto_intelli::core::sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS \"{sys}\" CASCADE"
    )))
    .execute(&pool)
    .await
    .unwrap();
}
