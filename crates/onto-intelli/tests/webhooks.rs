//! Webhook delivery from the outbox against a local receiver (axum on
//! 127.0.0.1, allow-listed with `[[webhooks.allow]] host = "127.0.0.1",
//! cidrs = ["127.0.0.1/32"]`), using `webhook.test` rows and change rows
//! enqueued directly: signatures that verify with the SDK algorithm, retries
//! with backoff, `Retry-After`, dead letters, 410 disabling the endpoint, no
//! redirects, the SSRF guard (metadata, `localhost`, `[::ffff:127.0.0.1]`, a
//! restricted endpoint ignoring the allow list), rotation, replay keeping the
//! id, rendering change rows at send time (`not_visible`, `expired`), the
//! tenant kill switch, the breaker and the payload limit.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use parking_lot::Mutex;
use serde_json::{Value, json};

use onto_intelli::config::AllowConfig;
use onto_intelli::core::EventFilter;
use onto_intelli::core::changes::{ChangeRetention, Cursor};
use onto_intelli::core::outbox::{ConfigEvent, OutboxKind, OutboxRow, OutboxSelect, OutboxState, SOURCE_CHANGE};
use onto_intelli::core::policy::ScopePrincipal;
use onto_intelli::trace::TraceCtx;
use onto_intelli::webhooks::{
    self, BREAKER_PREFIX, BreakerState, Endpoint, KILL_KEY, NS_CONTROL, NS_ENDPOINT, SecretRef,
};
use onto_intelli::{ActionContext, Config, Intelli, Principal, Role};

// ── the receiver ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Hit {
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
}

fn reply(status: u16) -> Reply {
    Reply { status, headers: Vec::new() }
}

#[derive(Clone, Default)]
struct Receiver {
    hits: Arc<Mutex<Vec<Hit>>>,
    script: Arc<Mutex<VecDeque<Reply>>>,
    port: u16,
}

impl Receiver {
    async fn start(script: Vec<Reply>) -> Receiver {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let r = Receiver { hits: Default::default(), script: Arc::new(Mutex::new(script.into())), port };
        let app = axum::Router::new().fallback(handle).with_state(r.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        r
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn hits(&self) -> Vec<Hit> {
        self.hits.lock().clone()
    }
}

async fn handle(
    State(r): State<Receiver>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, HeaderMap, String) {
    let h = headers.iter().map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string())).collect();
    r.hits.lock().push(Hit { path: uri.path().to_string(), headers: h, body: body.to_vec() });
    let next = r.script.lock().pop_front().unwrap_or_else(|| reply(200));
    let mut out = HeaderMap::new();
    for (k, v) in next.headers {
        out.insert(k, v.parse().unwrap());
    }
    (StatusCode::from_u16(next.status).unwrap(), out, format!("answer {}", next.status))
}

// ── the tenant ─────────────────────────────────────────────────────────

async fn open(tweak: impl FnOnce(&mut Config)) -> Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    cfg.webhooks.poll_secs = 1;
    cfg.webhooks.backoff_base_secs = 1;
    cfg.webhooks.backoff_max_secs = 1;
    cfg.webhooks.allow = vec![AllowConfig { host: "127.0.0.1".into(), cidrs: vec!["127.0.0.1/32".into()] }];
    tweak(&mut cfg);
    Intelli::open(cfg).await.unwrap()
}

fn admin() -> ActionContext {
    ActionContext::new(Principal::admin("lead"), "acme")
}

/// A secret file for one test (a `file:` reference, as an operator registers).
fn secret_file(name: &str, secret: &str) -> SecretRef {
    let dir = std::env::temp_dir().join("onto-intelli-webhook-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}-{}", onto_intelli::core::time::new_id("s")));
    std::fs::write(&path, secret).unwrap();
    SecretRef::File(path)
}

const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

fn endpoint(name: &str, url: &str, secret: SecretRef) -> Endpoint {
    Endpoint {
        name: name.into(),
        url: url.into(),
        secret,
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
    }
}

fn put(i: &Intelli, ep: &Endpoint) {
    let kv = vec![(NS_ENDPOINT.to_string(), ep.name.clone(), serde_json::to_value(ep).unwrap())];
    i.ontology().commit_config(&admin(), kv, ConfigEvent::new("webhook.add", Some(&ep.name), json!({}))).unwrap();
}

fn test_delivery(i: &Intelli, name: &str) -> String {
    webhooks::enqueue_test(i.ontology(), &admin(), name).unwrap()["id"].as_str().unwrap().to_string()
}

fn row(i: &Intelli, id: &str) -> OutboxRow {
    i.ontology().store().outbox_get(id).unwrap().unwrap()
}

/// Wait until the row satisfies `done` (10 s at most).
async fn wait_for(i: &Intelli, id: &str, what: &str, done: impl Fn(&OutboxRow) -> bool) -> OutboxRow {
    let start = Instant::now();
    loop {
        let r = row(i, id);
        if done(&r) {
            return r;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "waited for {what}: {r:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_state(i: &Intelli, id: &str, state: OutboxState) -> OutboxRow {
    wait_for(i, id, state.as_str(), |r| r.state == state).await
}

fn events(i: &Intelli, action: &str) -> Vec<onto_intelli::Event> {
    let f = EventFilter { action: Some(action.into()), ..Default::default() };
    i.ontology().events(&Principal::admin("lead"), &f).unwrap()
}

fn now_secs() -> i64 {
    onto_intelli::core::time::now_ms() / 1000
}

/// Verify a received request the way the SDK helpers do.
fn verify(hit: &Hit, secrets: &[&str]) -> onto_intelli::Result<Value> {
    let h = |k: &str| hit.headers.get(k).cloned().unwrap_or_default();
    webhooks::verify(
        secrets,
        &h("webhook-id"),
        &h("webhook-timestamp"),
        &h("webhook-signature"),
        &hit.body,
        now_secs(),
        webhooks::TOLERANCE_SECS,
    )
}

// ── tests ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_cloudevents_are_delivered_and_verify() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    put(&i, &endpoint("ops", &rx.url("/hooks/ops"), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    let done = wait_state(&i, &id, OutboxState::Delivered).await;
    assert_eq!((done.attempts, done.last_status), (1, Some(200)));
    let hits = rx.hits();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let hit = &hits[0];
    assert_eq!(hit.path, "/hooks/ops");
    assert_eq!(hit.headers["webhook-id"], id);
    assert_eq!(hit.headers["content-type"], "application/cloudevents+json");
    assert!(hit.headers["user-agent"].starts_with("onto-intelli/"), "{:?}", hit.headers);
    let sigs: Vec<&str> = hit.headers["webhook-signature"].split(' ').collect();
    assert_eq!(sigs.len(), 1);
    assert!(sigs[0].starts_with("v1,"));
    let ev = verify(hit, &[SECRET]).unwrap();
    assert_eq!((ev["specversion"].as_str(), ev["id"].as_str()), (Some("1.0"), Some(id.as_str())));
    assert_eq!(
        (ev["type"].as_str(), ev["source"].as_str()),
        (Some(webhooks::TEST_EVENT), Some("onto-intelli://default"))
    );
    assert_eq!(ev["data"]["endpoint"], "ops");
    assert!(verify(hit, &["whsec_b3RoZXI="]).is_err(), "another secret does not verify");
    let log = i.ontology().store().outbox_attempts(&id, 10).unwrap();
    assert_eq!((log.len(), log[0].result.status), (1, Some(200)));
    assert_eq!(log[0].result.response.as_deref(), Some("answer 200"));
    assert_eq!(events(&i, "webhook.test").len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_trace_context_travels_as_traceparent() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let r = webhooks::test_row("ops", "acme", Some("4bf92f3577b34da6a3ce929d0e0e4736".into()));
    let id = r.id.clone();
    i.ontology().enqueue_outbox(&admin(), vec![r], None).unwrap();
    wait_state(&i, &id, OutboxState::Delivered).await;
    let tp = rx.hits()[0].headers["traceparent"].clone();
    assert!(tp.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-") && tp.ends_with("-01") && tp.len() == 55, "{tp}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failures_retry_with_backoff_then_deliver() {
    let rx = Receiver::start(vec![reply(500), reply(503)]).await;
    let i = open(|_| {}).await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    let retry = wait_state(&i, &id, OutboxState::Retry).await;
    assert_eq!((retry.attempts, retry.last_status, retry.last_error.as_deref()), (1, Some(500), Some("http 500")));
    // Base 1 s, ±20% jitter, never above the 1 s maximum.
    let wait = retry.next_at - onto_intelli::core::time::now_ms();
    assert!((-100..=1_000).contains(&wait), "{wait}");
    let done = wait_state(&i, &id, OutboxState::Delivered).await;
    assert_eq!(done.attempts, 3);
    let hits = rx.hits();
    assert_eq!(hits.len(), 3);
    assert!(hits.iter().all(|h| h.headers["webhook-id"] == id), "every attempt carries the same id");
    let log = i.ontology().store().outbox_attempts(&id, 10).unwrap();
    let statuses: Vec<Option<u16>> = log.iter().map(|a| a.result.status).collect();
    assert_eq!(statuses, vec![Some(200), Some(503), Some(500)], "newest first");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_after_is_honoured_up_to_the_maximum() {
    let mut r429 = reply(429);
    r429.headers.push(("retry-after", "120".into()));
    let rx = Receiver::start(vec![r429]).await;
    let i = open(|c| c.webhooks.backoff_max_secs = 3600).await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    let retry = wait_state(&i, &id, OutboxState::Retry).await;
    let wait = retry.next_at - onto_intelli::core::time::now_ms();
    assert!((115_000..=120_000).contains(&wait), "{wait}");
    assert_eq!(retry.last_status, Some(429));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_client_errors_are_dead_and_audited() {
    let rx = Receiver::start(vec![reply(400)]).await;
    let i = open(|_| {}).await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    let dead = wait_state(&i, &id, OutboxState::Dead).await;
    assert_eq!((dead.attempts, dead.last_status), (1, Some(400)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rx.hits().len(), 1, "never retried");
    let ev = events(&i, "webhook.dead");
    assert_eq!(ev.len(), 1);
    assert_eq!((ev[0].payload["id"].as_str(), ev[0].payload["status"].as_u64()), (Some(id.as_str()), Some(400)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gone_disables_the_endpoint() {
    let rx = Receiver::start(vec![reply(410)]).await;
    let i = open(|_| {}).await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    wait_state(&i, &id, OutboxState::Dead).await;
    let ep = Endpoint::load(i.ontology(), "ops").unwrap().unwrap();
    assert!(!ep.enabled && ep.version == 2, "{ep:?}");
    assert_eq!(events(&i, "webhook.endpoint.disabled").len(), 1);
    // Rows of a disabled endpoint wait; they are not sent.
    let next = test_delivery(&i, "ops");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(row(&i, &next).state, OutboxState::Pending);
    assert_eq!(rx.hits().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redirects_are_not_followed() {
    let elsewhere = Receiver::start(vec![]).await;
    let mut moved = reply(307);
    moved.headers.push(("location", elsewhere.url("/stolen")));
    let rx = Receiver::start(vec![moved]).await;
    let i = open(|_| {}).await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    let dead = wait_state(&i, &id, OutboxState::Dead).await;
    assert_eq!(dead.last_status, Some(307));
    assert!(elsewhere.hits().is_empty(), "the redirect target got nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_guard_refuses_metadata_private_names_and_restricted_endpoints() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    let port = rx.port;
    let mut restricted = endpoint("proj", &format!("https://127.0.0.1:{port}/"), secret_file("proj", SECRET));
    restricted.restricted = true;
    let cases = [
        endpoint("meta", "https://169.254.169.254/latest/meta-data", secret_file("meta", SECRET)),
        // A name: refused by the client's resolver right before connecting.
        endpoint("local", &format!("https://localhost:{port}/"), secret_file("local", SECRET)),
        endpoint("mapped", &format!("https://[::ffff:127.0.0.1]:{port}/"), secret_file("mapped", SECRET)),
        endpoint("private", &format!("https://10.0.0.1:{port}/"), secret_file("private", SECRET)),
        endpoint("plain-http", "http://hooks.example.com/in", secret_file("plain", SECRET)),
        restricted,
    ];
    let mut ids = Vec::new();
    for ep in &cases {
        put(&i, ep);
        ids.push((ep.name.clone(), test_delivery(&i, &ep.name)));
    }
    for (name, id) in ids {
        let dead = wait_state(&i, &id, OutboxState::Dead).await;
        let e = dead.last_error.clone().unwrap_or_default();
        let why = match name.as_str() {
            "meta" => "169.254.169.254, a link-local, metadata or reserved address",
            "local" => "localhost resolves to",
            "mapped" => "::ffff:127.0.0.1",
            "private" => "10.0.0.1, not a public address",
            "plain-http" => "must be an https URL",
            _ => "public addresses only",
        };
        assert!(e.starts_with("refused:") || e.contains("refused: "), "{name}: {e}");
        assert!(e.contains(why), "{name}: {e}");
        assert_eq!(dead.last_status, None, "{name}: never sent");
    }
    assert!(rx.hits().is_empty(), "nothing reached the receiver");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rotation_signs_with_both_secrets() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    let old = "whsec_dGVzdC1zZWNyZXQtdHdvLXR3by10d28tdHdvLXR3bw==";
    let mut ep = endpoint("ops", &rx.url("/"), secret_file("ops", SECRET));
    ep.previous_secret = Some(secret_file("ops-old", old));
    ep.previous_until = Some(onto_intelli::core::time::now_ms() + 3_600_000);
    put(&i, &ep);
    let id = test_delivery(&i, "ops");
    wait_state(&i, &id, OutboxState::Delivered).await;
    let hit = &rx.hits()[0];
    assert_eq!(hit.headers["webhook-signature"].split(' ').count(), 2);
    verify(hit, &[SECRET]).unwrap();
    verify(hit, &[old]).unwrap();
    // After the rotation window only the current secret signs.
    ep.previous_until = Some(onto_intelli::core::time::now_ms() - 1);
    put(&i, &ep);
    let id = test_delivery(&i, "ops");
    wait_state(&i, &id, OutboxState::Delivered).await;
    let hit = &rx.hits()[1];
    assert_eq!(hit.headers["webhook-signature"].split(' ').count(), 1);
    assert!(verify(hit, &[old]).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_keeps_the_id() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    wait_state(&i, &id, OutboxState::Delivered).await;
    let n = i.ontology().store().outbox_reset("ops", &OutboxSelect::Ids(vec![id.clone()]), &[]).unwrap();
    assert_eq!(n, 1);
    i.wake_outbox();
    wait_for(&i, &id, "a second delivery", |r| r.state == OutboxState::Delivered && rx.hits().len() == 2).await;
    let hits = rx.hits();
    assert_eq!(
        (hits[0].headers["webhook-id"].as_str(), hits[1].headers["webhook-id"].as_str()),
        (id.as_str(), id.as_str())
    );
    assert_eq!(verify(&hits[1], &[SECRET]).unwrap()["id"], id);
    assert_eq!(i.ontology().store().outbox_attempts(&id, 10).unwrap().len(), 2);
}

fn change_row(target: &str, cursor: Cursor) -> OutboxRow {
    let mut r = OutboxRow::new(OutboxKind::Webhook, target, cursor.to_string(), SOURCE_CHANGE, "acme", "onto.change");
    r.cursor = Some(cursor);
    r
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn change_rows_are_rendered_as_the_endpoint_principal_at_send_time() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    let lead = Principal::admin("lead");
    i.call(&lead, "entity.upsert", json!({ "id": "task:a", "title": "A", "workspace": "acme" })).await.unwrap();
    let seen = i.ontology().change_head().unwrap();
    i.call(&lead, "entity.upsert", json!({ "id": "task:b", "title": "B", "workspace": "ops" })).await.unwrap();
    let hidden = i.ontology().change_head().unwrap();
    // The endpoint sees workspace acme only.
    let mut ep = endpoint("acme-only", &rx.url("/"), secret_file("acme", SECRET));
    ep.principal = ScopePrincipal::new(vec![Role::Viewer], vec!["acme".into()]);
    put(&i, &ep);
    let store = i.ontology().store().clone();
    let (a, b) = (change_row("acme-only", seen), change_row("acme-only", hidden));
    store.outbox_enqueue(&[a.clone(), b.clone()], None, None).unwrap();
    i.wake_outbox();
    wait_state(&i, &a.id, OutboxState::Delivered).await;
    let cancelled = wait_state(&i, &b.id, OutboxState::Cancelled).await;
    assert_eq!(cancelled.last_error.as_deref(), Some("not_visible"));
    let hits = rx.hits();
    assert_eq!(hits.len(), 1);
    let ev = verify(&hits[0], &[SECRET]).unwrap();
    assert_eq!((ev["subject"].as_str(), ev["cursor"].as_str()), (Some("task:a"), Some(seen.to_string().as_str())));
    assert_eq!((ev["data"]["op"].as_str(), ev["data"]["id"].as_str()), (Some("entity.create"), Some("task:a")));
    // A change removed by retention is dead: expired.
    for n in 0..3 {
        i.call(&lead, "entity.upsert", json!({ "id": format!("task:c{n}"), "title": "C", "workspace": "acme" }))
            .await
            .unwrap();
    }
    let v = store.vacuum_changes(&ChangeRetention { keep_ms: None, keep_rows: Some(1) }).unwrap();
    assert!(v.floor > seen.seq, "{v:?}");
    let old = change_row("acme-only", Cursor::new(seen.seq, seen.n));
    let mut old = old;
    old.dedupe = format!("again:{seen}");
    store.outbox_enqueue(std::slice::from_ref(&old), None, None).unwrap();
    i.wake_outbox();
    let dead = wait_state(&i, &old.id, OutboxState::Dead).await;
    assert_eq!(dead.last_error.as_deref(), Some("expired"));
    assert_eq!(rx.hits().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_kill_switch_holds_rows_until_it_is_off() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let kill = |on: bool| {
        let kv = vec![(NS_CONTROL.to_string(), KILL_KEY.to_string(), json!({ "on": on }))];
        i.ontology().commit_config(&admin(), kv, ConfigEvent::new("webhook.kill", None, json!({ "on": on }))).unwrap();
    };
    kill(true);
    let id = test_delivery(&i, "ops");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let held = row(&i, &id);
    assert_eq!((held.state, held.attempts), (OutboxState::Pending, 0), "released unsent, not counted");
    assert!(rx.hits().is_empty());
    kill(false);
    i.wake_outbox();
    wait_state(&i, &id, OutboxState::Delivered).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_breaker_pauses_then_probes_and_closes() {
    let rx = Receiver::start(vec![reply(500), reply(500)]).await;
    let i = open(|c| {
        c.webhooks.pause_after_failures = 2;
        c.webhooks.pause_secs = 1;
    })
    .await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    // Two failures open the breaker.
    let start = Instant::now();
    loop {
        let b = i.ontology().store().kv_get(NS_CONTROL, &format!("{BREAKER_PREFIX}ops")).unwrap();
        if let Some(b) = b.and_then(|v| serde_json::from_value::<BreakerState>(v).ok())
            && b.is_open()
        {
            assert_eq!(b.pause_ms, 1_000);
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "the breaker did not open");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(events(&i, "webhook.endpoint.paused").len(), 1);
    // After the pause one probe is sent; it succeeds and the breaker closes.
    wait_state(&i, &id, OutboxState::Delivered).await;
    let b: BreakerState = serde_json::from_value(
        i.ontology().store().kv_get(NS_CONTROL, &format!("{BREAKER_PREFIX}ops")).unwrap().unwrap(),
    )
    .unwrap();
    assert!(!b.is_open(), "{b:?}");
    assert_eq!(events(&i, "webhook.endpoint.resumed").len(), 1);
    assert_eq!(rx.hits().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bodies_over_the_limit_are_not_sent() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|c| c.webhooks.max_payload_bytes = 64).await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    let dead = wait_state(&i, &id, OutboxState::Dead).await;
    assert_eq!(dead.last_error.as_deref(), Some("payload_too_large"));
    assert!(rx.hits().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_secret_names_its_reference_and_retries() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    put(&i, &endpoint("ops", &rx.url("/"), SecretRef::Env("ONTO_TEST_SURELY_UNSET_WEBHOOK_SECRET".into())));
    let id = test_delivery(&i, "ops");
    let retry = wait_state(&i, &id, OutboxState::Retry).await;
    let e = retry.last_error.unwrap_or_default();
    assert!(e.contains("env:ONTO_TEST_SURELY_UNSET_WEBHOOK_SECRET"), "{e}");
    assert!(rx.hits().is_empty());
}

// ── traces (docs/design/EVENTS.md B.7, section 11) ─────────────────────

const OTEL: &str = "0af7651916cd43dd8448eb211c80319c";

/// The trace id and span id of a received `traceparent`.
fn traceparent_of(hit: &Hit) -> (String, String) {
    let tp = hit.headers.get("traceparent").cloned().unwrap_or_default();
    let parts: Vec<&str> = tp.split('-').collect();
    assert!(parts.len() == 4 && parts[0] == "00" && parts[3] == "01", "{tp}");
    assert!(parts[1].len() == 32 && parts[2].len() == 16, "{tp}");
    (parts[1].to_string(), parts[2].to_string())
}

/// The delivery traces of `i`, oldest first, with their spans.
async fn delivery_traces(i: &Intelli) -> Vec<Value> {
    i.flush_traces().await;
    let lead = Principal::admin("lead");
    let list = i.call(&lead, "trace.list", json!({ "kind": "delivery" })).await.unwrap();
    let mut out = Vec::new();
    for t in list["traces"].as_array().unwrap().iter().rev() {
        out.push(i.call(&lead, "trace.get", json!({ "id": t["id"] })).await.unwrap());
    }
    out
}

/// The root span of a trace detail.
fn root_span(d: &Value) -> &Value {
    d["spans"].as_array().unwrap().iter().find(|s| s["parent_id"].is_null()).expect("a root span")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_continues_its_trace_into_the_test_delivery() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let lead = Principal::admin("lead");
    // The caller's traceparent: the delivery continues that trace.
    let ctx = TraceCtx::from_source("http").with_traceparent(Some(&format!("00-{OTEL}-b7ad6b7169203331-01")));
    let t = i.call_traced(&lead, "webhook.test", json!({ "name": "ops" }), ctx).await;
    let first = t.result.unwrap()["id"].as_str().unwrap().to_string();
    assert_eq!(row(&i, &first).otel_trace_id.as_deref(), Some(OTEL));
    wait_state(&i, &first, OutboxState::Delivered).await;
    // Without one: the delivery continues the call's own trace.
    let t = i.call_traced(&lead, "webhook.test", json!({ "name": "ops" }), TraceCtx::from_source("http")).await;
    let own = t.trace_id.expect("webhook.test is traced as a write");
    let second = t.result.unwrap()["id"].as_str().unwrap().to_string();
    wait_state(&i, &second, OutboxState::Delivered).await;
    let by_id: HashMap<String, Hit> = rx.hits().into_iter().map(|h| (h.headers["webhook-id"].clone(), h)).collect();
    assert_eq!(traceparent_of(&by_id[&first]).0, OTEL);
    assert_eq!(traceparent_of(&by_id[&second]).0, own);
    assert_ne!(traceparent_of(&by_id[&first]).1, traceparent_of(&by_id[&second]).1, "a span per attempt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_traces_hold_no_secret_signature_body_or_referenced_url() {
    let rx = Receiver::start(vec![reply(500)]).await;
    let i = open(|c| c.traces.deliveries = "all".into()).await;
    // The URL comes from an operator reference: its value is never recorded.
    let dir = std::env::temp_dir().join("onto-intelli-webhook-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let url_file = dir.join(format!("url-{}", onto_intelli::core::time::new_id("u")));
    let url = rx.url("/hooks/review-hidden-path");
    std::fs::write(&url_file, &url).unwrap();
    put(&i, &endpoint("ops", &format!("file:{}", url_file.display()), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    wait_state(&i, &id, OutboxState::Delivered).await;
    let hits = rx.hits();
    assert_eq!(hits.len(), 2);
    let traces = delivery_traces(&i).await;
    assert_eq!(traces.len(), 2, "all: the failure and the delivery");
    let key = &SECRET["whsec_".len()..];
    for (d, hit) in traces.iter().zip(&hits) {
        let text = d.to_string();
        assert!(!text.contains(SECRET) && !text.contains(key), "no signing secret: {text}");
        for h in &hits {
            let sig = &h.headers["webhook-signature"];
            assert!(!text.contains(sig.as_str()) && !text.contains(&sig["v1,".len()..]), "no signature: {text}");
            assert!(!text.contains("a test delivery from onto-intelli"), "no body: {text}");
        }
        assert!(!text.contains(&url) && !text.contains("review-hidden-path") && !text.contains("127.0.0.1"), "{text}");
        // The trace is the attempt the receiver saw: its id (the row
        // continues no trace) and its root span are the traceparent's.
        let (trace, span) = traceparent_of(hit);
        assert_eq!(d["trace"]["id"], trace.as_str());
        assert_eq!(root_span(d)["id"], span.as_str());
        assert_eq!(
            (d["trace"]["kind"].as_str(), d["trace"]["name"].as_str()),
            (Some("delivery"), Some("webhook.deliver:ops"))
        );
        assert_eq!(d["trace"]["request_id"], id.as_str());
        assert_eq!(d["trace"]["principal"], "webhook:ops");
        assert!(d["trace"]["otel_trace_id"].is_null());
    }
    let (failed, delivered) = (&traces[0], &traces[1]);
    assert_eq!(
        (failed["trace"]["status"].as_str(), failed["trace"]["error_code"].as_str()),
        (Some("error"), Some("retry"))
    );
    assert_eq!(failed["trace"]["error_message"], "http 500");
    assert_eq!(root_span(failed)["attrs"]["http_status"], 500);
    assert_eq!(root_span(failed)["attrs"]["attempt"], 1);
    assert_eq!(delivered["trace"]["status"], "ok");
    assert_eq!(root_span(delivered)["attrs"]["outcome"], "delivered");
    assert_eq!(delivered["spans"].as_array().unwrap().len(), 2, "the attempt and its request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivery_traces_follow_traces_deliveries() {
    // Default (failures): the failed attempt only.
    let rx = Receiver::start(vec![reply(503)]).await;
    let i = open(|_| {}).await;
    assert_eq!(i.config().traces.deliveries, "failures");
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    wait_state(&i, &id, OutboxState::Delivered).await;
    let traces = delivery_traces(&i).await;
    assert_eq!(traces.len(), 1, "{traces:?}");
    assert_eq!(traces[0]["trace"]["error_code"], "retry");
    // A dead row is a failure too.
    let rx = Receiver::start(vec![reply(400)]).await;
    put(&i, &endpoint("gone", &rx.url("/"), secret_file("gone", SECRET)));
    let id = test_delivery(&i, "gone");
    wait_state(&i, &id, OutboxState::Dead).await;
    let traces = delivery_traces(&i).await;
    assert_eq!(traces.len(), 2);
    assert_eq!(traces[1]["trace"]["error_code"], "dead");

    // off: none, failed or not.
    let rx = Receiver::start(vec![reply(500)]).await;
    let i = open(|c| c.traces.deliveries = "off".into()).await;
    put(&i, &endpoint("ops", &rx.url("/"), secret_file("ops", SECRET)));
    let id = test_delivery(&i, "ops");
    wait_state(&i, &id, OutboxState::Delivered).await;
    assert!(delivery_traces(&i).await.is_empty());
    // The traceparent is sent all the same.
    assert!(rx.hits().iter().all(|h| h.headers.contains_key("traceparent")));
}
