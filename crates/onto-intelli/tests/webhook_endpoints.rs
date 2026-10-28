//! Webhook endpoints through their actions, and the fan-out of change
//! subscriptions (docs/design/EVENTS.md B.2, B.6, section 6.1), against a
//! local receiver (axum on 127.0.0.1, allow-listed with
//! `[[webhooks.allow]] host = "127.0.0.1", cidrs = ["127.0.0.1/32"]`):
//! registration with a generated secret returned once, deliveries rendered
//! as the endpoint's principal sees them, a backlogged endpoint resuming
//! from its own cursor without losing anything, an expired cursor's gap
//! delivery, a kind rename rewriting endpoint filters in the schema commit,
//! and a narrower admin refused on a broader endpoint.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{Value, json};

use onto_intelli::config::AllowConfig;
use onto_intelli::core::EventFilter;
use onto_intelli::core::changes::ChangeRetention;
use onto_intelli::core::outbox::{OutboxQuery, OutboxState};
use onto_intelli::hub::{Secret, SecretBox};
use onto_intelli::webhooks::{self, BACKLOG_PREFIX, Endpoint, NS_CONTROL};
use onto_intelli::{Config, Intelli, Principal, Result, Role};

// ── the receiver ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Hit {
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone, Default)]
struct Receiver {
    hits: Arc<Mutex<Vec<Hit>>>,
    /// Answers in order; 200 once they run out.
    script: Arc<Mutex<Vec<u16>>>,
    port: u16,
}

impl Receiver {
    async fn start(script: Vec<u16>) -> Receiver {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut script = script;
        script.reverse();
        let r = Receiver { hits: Default::default(), script: Arc::new(Mutex::new(script)), port };
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

    /// The CloudEvents received, verified with `secret`.
    fn events(&self, secret: &str) -> Vec<Value> {
        self.hits().iter().map(|h| verify(h, secret)).collect()
    }
}

async fn handle(State(r): State<Receiver>, headers: HeaderMap, body: Bytes) -> StatusCode {
    r.hits.lock().push(Hit { headers, body: body.to_vec() });
    let status = r.script.lock().pop().unwrap_or(200);
    StatusCode::from_u16(status).unwrap()
}

fn verify(hit: &Hit, secret: &str) -> Value {
    let h = |k: &str| hit.headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let now = onto_intelli::core::time::now_ms() / 1000;
    webhooks::verify(
        &[secret],
        &h("webhook-id"),
        &h("webhook-timestamp"),
        &h("webhook-signature"),
        &hit.body,
        now,
        webhooks::TOLERANCE_SECS,
    )
    .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&hit.body)))
}

// ── the tenant ─────────────────────────────────────────────────────────

/// Seals by encoding (tests only): what matters here is that outputs never
/// show the secret and that deliveries sign with it.
struct TestBox;

impl SecretBox for TestBox {
    fn seal(&self, purpose: &str, name: &str, plain: &[u8]) -> Result<String> {
        let mut blob = format!("{purpose}/{name}/").into_bytes();
        blob.extend_from_slice(plain);
        Ok(base64::engine::general_purpose::STANDARD.encode(blob))
    }

    fn open(&self, purpose: &str, name: &str, sealed: &str) -> Result<Secret> {
        let blob = base64::engine::general_purpose::STANDARD.decode(sealed).unwrap();
        let prefix = format!("{purpose}/{name}/");
        assert!(blob.starts_with(prefix.as_bytes()), "sealed for another purpose or name");
        Ok(Secret::new(blob[prefix.len()..].to_vec()))
    }
}

async fn open(tweak: impl FnOnce(&mut Config)) -> Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    cfg.webhooks.poll_secs = 1;
    cfg.webhooks.backoff_base_secs = 1;
    cfg.webhooks.backoff_max_secs = 1;
    cfg.webhooks.allow = vec![AllowConfig { host: "127.0.0.1".into(), cidrs: vec!["127.0.0.1/32".into()] }];
    tweak(&mut cfg);
    let i = Intelli::open(cfg).await.unwrap();
    i.set_secrets(Some(Arc::new(TestBox)));
    i
}

fn lead() -> Principal {
    Principal::admin("lead")
}

async fn ok(i: &Intelli, p: &Principal, action: &str, input: Value) -> Value {
    i.call(p, action, input.clone()).await.unwrap_or_else(|e| panic!("{action} {input}: {e}"))
}

async fn upsert(i: &Intelli, id: &str, ws: &str) {
    ok(i, &lead(), "entity.upsert", json!({ "id": id, "title": id, "workspace": ws })).await;
}

fn events(i: &Intelli, action: &str) -> Vec<onto_intelli::Event> {
    let f = EventFilter { action: Some(action.into()), ..Default::default() };
    i.ontology().events(&lead(), &f).unwrap()
}

/// Wait (10 s at most) until `done`.
async fn until(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(15), "waited for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Add an endpoint subscribed to `filter`; returns its generated secret.
async fn add(i: &Intelli, name: &str, url: &str, filter: Value, principal: Value) -> String {
    let out =
        ok(i, &lead(), "webhook.add", json!({ "name": name, "url": url, "filter": filter, "principal": principal }))
            .await;
    out["secret"].as_str().unwrap_or_else(|| panic!("a generated secret: {out}")).to_string()
}

fn ids_of(evs: &[Value]) -> Vec<String> {
    evs.iter().filter_map(|e| e["subject"].as_str().map(str::to_string)).collect()
}

// ── tests ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn endpoints_are_managed_through_their_actions() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    let added = ok(
        &i,
        &lead(),
        "webhook.add",
        json!({ "name": "ops", "url": rx.url("/hooks/ops?token=t0p"), "filter": { "kinds": "task" } }),
    )
    .await;
    let secret = added["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("whsec_"), "{added}");
    let ep = &added["endpoint"];
    assert_eq!(ep["principal"], json!({ "roles": ["viewer"], "workspaces": ["acme"] }), "{ep}");
    assert_eq!((ep["status"].as_str(), ep["version"].as_u64()), (Some("active"), Some(1)));
    assert!(ep["secret"].as_str().unwrap().starts_with("sealed"), "{ep}");

    // The listing never shows the secret again.
    let list = ok(&i, &lead(), "webhook.list", json!({})).await;
    assert!(!list.to_string().contains(&secret), "{list}");
    assert_eq!(list["items"][0]["name"], "ops");
    assert!(list["items"][0]["cursor"].as_str().is_some(), "the subscription starts at the head: {list}");
    assert!(list["note"].as_str().unwrap().contains("delivered at least once"));

    // A subscribed change is delivered, signed with the returned secret.
    upsert(&i, "task:a", "acme").await;
    upsert(&i, "person:z", "acme").await;
    until("one delivery", || !rx.hits().is_empty()).await;
    let evs = rx.events(&secret);
    assert_eq!(ids_of(&evs), vec!["task:a"]);
    assert_eq!(
        (evs[0]["type"].as_str(), evs[0]["data"]["op"].as_str()),
        (Some("onto.entity.create"), Some("entity.create"))
    );
    assert_eq!(evs[0]["specversion"], "1.0");

    // Deliveries (admin who dominates), test, rotation, replay, resume, update, remove.
    let d = ok(&i, &lead(), "webhook.deliveries", json!({ "name": "ops" })).await;
    let row_id = d["items"][0]["id"].as_str().unwrap().to_string();
    let one = ok(&i, &lead(), "webhook.deliveries", json!({ "name": "ops", "id": row_id })).await;
    assert_eq!(one["attempts"][0]["status"], 200, "{one}");
    let t = ok(&i, &lead(), "webhook.test", json!({ "name": "ops" })).await;
    until("the test delivery", || rx.hits().len() == 2).await;
    assert_eq!(rx.events(&secret)[1]["id"], t["id"]);
    let r = ok(&i, &lead(), "webhook.replay", json!({ "name": "ops", "ids": [row_id] })).await;
    assert_eq!(r["replayed"], 1, "{r}");
    until("the replay", || rx.hits().len() == 3).await;
    assert_eq!(rx.events(&secret)[2]["id"].as_str(), Some(row_id.as_str()), "a replay keeps the id");
    let rot = ok(&i, &lead(), "webhook.rotate_secret", json!({ "name": "ops" })).await;
    let new_secret = rot["secret"].as_str().unwrap().to_string();
    assert_ne!(new_secret, secret);
    upsert(&i, "task:b", "acme").await;
    until("a delivery after the rotation", || rx.hits().len() == 4).await;
    // Both secrets sign during the overlap.
    let last = rx.hits()[3].clone();
    verify(&last, &secret);
    verify(&last, &new_secret);
    let up = ok(&i, &lead(), "webhook.update", json!({ "name": "ops", "disabled": true })).await;
    assert_eq!((up["endpoint"]["status"].as_str(), up["endpoint"]["version"].as_u64()), (Some("disabled"), Some(3)));
    assert!(i.call(&lead(), "webhook.update", json!({ "name": "ops" })).await.is_err(), "nothing to change");
    let res = ok(&i, &lead(), "webhook.resume", json!({ "name": "ops" })).await;
    assert!(res["note"].as_str().unwrap().contains("disabled"), "{res}");
    ok(&i, &lead(), "webhook.remove", json!({ "name": "ops" })).await;
    assert_eq!(ok(&i, &lead(), "webhook.list", json!({})).await["items"], json!([]));
    for action in ["webhook.add", "webhook.update", "webhook.remove", "webhook.rotate_secret", "webhook.replay"] {
        assert_eq!(events(&i, action).len(), 1, "{action} is audited");
    }
    let added_ev = &events(&i, "webhook.add")[0];
    assert!(!added_ev.payload.to_string().contains("t0p"), "audit events name the host only: {:?}", added_ev.payload);

    // Inputs: {} is refused without I/O; a duplicate name conflicts; a bad URL names itself.
    for action in ["webhook.add", "webhook.update", "webhook.remove", "webhook.test", "webhook.kill"] {
        let e = i.call(&lead(), action, json!({})).await.unwrap_err();
        assert_eq!(e.code(), "invalid_argument", "{action}: {e}");
    }
    add(&i, "dup", &rx.url("/"), json!({}), json!({ "roles": ["viewer"], "workspaces": ["acme"] })).await;
    let e = i.call(&lead(), "webhook.add", json!({ "name": "dup", "url": rx.url("/") })).await.unwrap_err();
    assert_eq!(e.code(), "conflict", "{e}");
    let e = i.call(&lead(), "webhook.add", json!({ "name": "meta", "url": "https://169.254.169.254/" })).await;
    assert!(e.unwrap_err().to_string().contains("link-local"));
    let e = i.call(&lead(), "webhook.add", json!({ "name": "plain", "url": "http://hooks.example.com/" })).await;
    assert!(e.unwrap_err().to_string().contains("https"));
    let e = i
        .call(
            &lead(),
            "webhook.add",
            json!({ "name": "bot", "url": rx.url("/"), "principal": { "roles": "agent", "workspaces": "*" } }),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("system or agent"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_need_a_sealer_unless_the_operator_gives_a_reference() {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    cfg.webhooks.allow = vec![AllowConfig { host: "127.0.0.1".into(), cidrs: vec!["127.0.0.1/32".into()] }];
    let i = Intelli::open(cfg).await.unwrap();
    let e = i.call(&lead(), "webhook.add", json!({ "name": "ops", "url": "http://127.0.0.1:9/" })).await.unwrap_err();
    assert!(e.to_string().contains("failed_precondition") && e.to_string().contains("env:VAR"), "{e}");
    let out = ok(
        &i,
        &lead(),
        "webhook.add",
        json!({ "name": "ops", "url": "http://127.0.0.1:9/", "secret": "env:HOOK_SECRET_UNSET" }),
    )
    .await;
    assert!(out.get("secret").is_none(), "nothing generated: {out}");
    assert_eq!(out["endpoint"]["secret"], "env:HOOK_SECRET_UNSET");
    // A URL reference is checked through its value, and errors name only the reference.
    // SAFETY: test-only variable that no other test reads.
    unsafe { std::env::set_var("ONTO_TEST_ENDPOINT_URL_PRIVATE", "https://10.1.2.3/secret-path") };
    let e = i
        .call(
            &lead(),
            "webhook.add",
            json!({ "name": "ref", "url": "env:ONTO_TEST_ENDPOINT_URL_PRIVATE", "secret": "env:X" }),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("env:ONTO_TEST_ENDPOINT_URL_PRIVATE") && !e.contains("secret-path"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fan_out_renders_each_change_as_the_endpoint_principal_sees_it() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    let secret = add(
        &i,
        "acme-tasks",
        &rx.url("/"),
        json!({ "kinds": ["task"] }),
        json!({ "roles": "viewer", "workspaces": "acme" }),
    )
    .await;
    upsert(&i, "task:a", "acme").await;
    upsert(&i, "task:hidden", "ops").await;
    ok(&i, &lead(), "entity.patch", json!({ "id": "task:a", "title": "A, renamed" })).await;
    upsert(&i, "task:b", "acme").await;
    until("three deliveries", || rx.hits().len() >= 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Delivery order is not promised: compare in feed order.
    let mut evs = rx.events(&secret);
    let pos = |e: &Value| {
        let c = e["cursor"].as_str().unwrap();
        let (seq, n) = c.split_once('.').unwrap();
        (seq.parse::<u64>().unwrap(), n.parse::<u32>().unwrap())
    };
    evs.sort_by_key(pos);
    let seen: Vec<(String, String)> = evs
        .iter()
        .map(|e| (e["subject"].as_str().unwrap().to_string(), e["type"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("task:a".into(), "onto.entity.create".into()),
            ("task:a".into(), "onto.entity.update".into()),
            ("task:b".into(), "onto.entity.create".into()),
        ],
        "the ops workspace is not the endpoint's"
    );
    // Each row is enqueued once per change: its dedupe key is the cursor.
    let rows = i
        .ontology()
        .store()
        .outbox_list(&OutboxQuery { target: Some("acme-tasks".into()), ..Default::default() })
        .unwrap();
    let keys: BTreeSet<&str> = rows.iter().map(|r| r.dedupe.as_str()).collect();
    assert_eq!((rows.len(), keys.len()), (3, 3));
    assert!(rows.iter().all(|r| r.cursor.is_some() && r.payload.is_null()), "rows hold only the cursor");
    let cursors: BTreeSet<&str> = evs.iter().map(|e| e["cursor"].as_str().unwrap()).collect();
    assert_eq!(cursors.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backlogged_endpoint_resumes_from_its_own_cursor_and_loses_nothing() {
    // The slow receiver fails six times before it answers; the fast one never fails.
    let slow = Receiver::start(vec![503; 6]).await;
    let fast = Receiver::start(vec![]).await;
    let i = open(|c| c.webhooks.pending_cap = 3).await;
    let tasks = json!({ "kinds": "task" });
    let scope = json!({ "roles": "viewer", "workspaces": "acme" });
    let slow_secret = add(&i, "slow", &slow.url("/"), tasks.clone(), scope.clone()).await;
    let fast_secret = add(&i, "fast", &fast.url("/"), tasks, scope).await;
    for n in 0..8 {
        upsert(&i, &format!("task:t{n}"), "acme").await;
    }
    // The slow endpoint is paused for its backlog with at most `pending_cap` rows.
    until("the backlog pause", || {
        events(&i, "webhook.endpoint.paused").iter().any(|e| e.payload["reason"] == "backlogged")
    })
    .await;
    let slow_rows = |i: &Intelli| {
        i.ontology().store().outbox_list(&OutboxQuery { target: Some("slow".into()), ..Default::default() }).unwrap()
    };
    assert!(slow_rows(&i).len() <= 3, "never past the cap: {:?}", slow_rows(&i).len());
    let listed = ok(&i, &lead(), "webhook.list", json!({})).await;
    let s = listed["items"].as_array().unwrap().iter().find(|e| e["name"] == "slow").unwrap().clone();
    assert_eq!((s["status"].as_str(), s["paused"]["reason"].as_str()), (Some("paused"), Some("backlogged")), "{s}");
    // The fast endpoint is not held back by the slow one.
    until("the fast endpoint's eight", || fast.hits().len() >= 8).await;
    // Once its backlog drains, the slow endpoint resumes where it stopped.
    until("all eight to the slow endpoint", || {
        slow_rows(&i).iter().filter(|r| r.state == OutboxState::Delivered).count() == 8
    })
    .await;
    assert!(!events(&i, "webhook.endpoint.resumed").is_empty());
    // With its backlog drained, the next fan-out round lifts the pause.
    until("the backlog pause lifted", || {
        i.ontology().store().kv_get(NS_CONTROL, &format!("{BACKLOG_PREFIX}slow")).unwrap().is_none_or(|v| v.is_null())
    })
    .await;
    let want: BTreeSet<String> = (0..8).map(|n| format!("task:t{n}")).collect();
    let got_slow: BTreeSet<String> = ids_of(&slow.events(&slow_secret)).into_iter().collect();
    let got_fast: BTreeSet<String> = ids_of(&fast.events(&fast_secret)).into_iter().collect();
    assert_eq!(got_slow, want, "nothing lost while backlogged");
    assert_eq!(got_fast, want);
    assert_eq!(slow_rows(&i).len(), 8, "each change enqueued once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_cursor_gets_one_gap_delivery_and_resumes_at_the_floor() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    let secret =
        add(&i, "ops", &rx.url("/"), json!({ "kinds": "task" }), json!({ "roles": "viewer", "workspaces": "acme" }))
            .await;
    ok(&i, &lead(), "webhook.update", json!({ "name": "ops", "disabled": true })).await;
    for n in 0..4 {
        upsert(&i, &format!("task:old{n}"), "acme").await;
    }
    let v = i.ontology().store().vacuum_changes(&ChangeRetention { keep_ms: None, keep_rows: Some(1) }).unwrap();
    assert!(v.floor > 0, "{v:?}");
    ok(&i, &lead(), "webhook.update", json!({ "name": "ops", "enabled": true })).await;
    upsert(&i, "task:new", "acme").await;
    until("the gap and the new change", || rx.hits().len() >= 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let evs = rx.events(&secret);
    let gaps: Vec<&Value> = evs.iter().filter(|e| e["type"] == webhooks::GAP_EVENT).collect();
    assert_eq!(gaps.len(), 1, "{evs:?}");
    assert_eq!(gaps[0]["data"]["reason"], "expired");
    assert!(gaps[0]["data"]["from"].as_str().is_some() && gaps[0]["data"]["to"].as_str().is_some());
    assert_eq!(events(&i, "webhook.endpoint.gap").len(), 1);
    let ids = ids_of(&evs);
    assert!(ids.contains(&"task:new".to_string()), "{ids:?}");
    assert!(ids.contains(&"task:old3".to_string()), "the floor's commit is kept and delivered: {ids:?}");
    assert!(!ids.contains(&"task:old0".to_string()), "removed records are reported as a gap, not sent: {ids:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kind_rename_rewrites_endpoint_filters_in_the_schema_commit() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    add(
        &i,
        "ops",
        &rx.url("/"),
        json!({ "kinds": ["task", "person"] }),
        json!({ "roles": "viewer", "workspaces": "acme" }),
    )
    .await;
    add(&i, "other", &rx.url("/"), json!({ "kinds": "person" }), json!({ "roles": "viewer", "workspaces": "acme" }))
        .await;
    ok(
        &i,
        &lead(),
        "schema.apply",
        json!({ "changes": [{ "op": "rename_object_type", "from": "task", "to": "chore" }] }),
    )
    .await;
    let ep = Endpoint::load(i.ontology(), "ops").unwrap().unwrap();
    assert_eq!(ep.filter.as_ref().unwrap().kinds, vec!["chore", "person"]);
    assert_eq!(ep.version, 2, "a rewritten filter is a new version");
    let other = Endpoint::load(i.ontology(), "other").unwrap().unwrap();
    assert_eq!(other.version, 1, "untouched");
    // The rewrite is part of the schema change's commit and its audit event.
    let change = events(&i, "schema.change");
    let last = change.last().unwrap();
    assert_eq!(last.payload["webhook_filters"], json!(["ops"]), "{:?}", last.payload);
    // Deliveries follow the new name.
    upsert(&i, "chore:x", "acme").await;
    until("the renamed kind's change", || {
        rx.hits().iter().any(|h| serde_json::from_slice::<Value>(&h.body).is_ok_and(|v| v["subject"] == "chore:x"))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_narrower_admin_cannot_manage_a_broader_endpoint() {
    let rx = Receiver::start(vec![]).await;
    let i = open(|_| {}).await;
    add(&i, "broad", &rx.url("/in?token=abc"), json!({}), json!({ "roles": "admin", "workspaces": "*" })).await;
    let row = ok(&i, &lead(), "webhook.test", json!({ "name": "broad" })).await["id"].as_str().unwrap().to_string();
    let narrow = Principal::new("user:ws1-admin", vec![Role::Admin], vec!["ws1".into()]);
    let before = events(&i, "authorization.denied").len();
    for (action, input) in [
        ("webhook.update", json!({ "name": "broad", "url": rx.url("/elsewhere") })),
        ("webhook.replay", json!({ "name": "broad", "ids": [row.clone()] })),
        ("webhook.deliveries", json!({ "name": "broad" })),
        ("webhook.deliveries", json!({ "name": "broad", "id": row.clone() })),
        ("webhook.test", json!({ "name": "broad" })),
        ("webhook.rotate_secret", json!({ "name": "broad" })),
        ("webhook.resume", json!({ "name": "broad" })),
        ("webhook.remove", json!({ "name": "broad" })),
        ("webhook.kill", json!({ "off": true })),
    ] {
        let e = i.call(&narrow, action, input).await.unwrap_err();
        assert_eq!(e.code(), "permission_denied", "{action}: {e}");
        assert!(e.to_string().contains("does not cover"), "{action}: {e}");
    }
    assert_eq!(events(&i, "authorization.denied").len() - before, 9, "every refusal is audited");
    // It lists the endpoint without the path and query, and not its cursor or pending rows.
    let list = ok(&i, &narrow, "webhook.list", json!({})).await;
    let item = &list["items"][0];
    assert_eq!(
        (item["url"].as_str(), item["may_manage"].as_bool()),
        (Some(rx.url("").as_str()), Some(false)),
        "{item}"
    );
    assert!(item.get("cursor").is_none() && item.get("pending").is_none(), "{item}");
    // Its outbox listing hides the broad endpoint's rows.
    let ob = ok(&i, &narrow, "outbox.list", json!({})).await;
    assert_eq!(ob["items"], json!([]), "{ob}");
    assert!(ob["hidden"].as_u64().unwrap() >= 1);
    // It may switch deliveries off (stopping needs only admin), and register endpoints within its scope.
    ok(&i, &narrow, "webhook.kill", json!({ "on": true })).await;
    ok(&i, &lead(), "webhook.kill", json!({ "off": true })).await;
    let e = i
        .call(
            &narrow,
            "webhook.add",
            json!({ "name": "wide", "url": rx.url("/"), "principal": { "roles": "viewer", "workspaces": "*" } }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code(), "permission_denied", "{e}");
    ok(
        &i,
        &narrow,
        "webhook.add",
        json!({ "name": "mine", "url": rx.url("/"), "principal": { "roles": "viewer", "workspaces": "ws1" } }),
    )
    .await;
    // Members, viewers and agents may not configure endpoints at all.
    for p in [
        Principal::member("user:m", "acme"),
        Principal::viewer("user:v", "acme"),
        Principal::new("bot", vec![Role::Agent, Role::Admin], vec!["*".into()]),
    ] {
        for action in ["webhook.list", "webhook.add", "outbox.list"] {
            let input = json!({ "name": "x", "url": rx.url("/") });
            let e = i.call(&p, action, input).await.unwrap_err();
            assert_eq!(e.code(), "permission_denied", "{} {action}: {e}", p.id);
        }
    }
    let who = ok(&i, &lead(), "system.whoami", json!({})).await;
    assert_eq!(who["capabilities"]["configure_outbound"], true);
    let who = ok(&i, &Principal::member("user:m", "acme"), "system.whoami", json!({})).await;
    assert_eq!(who["capabilities"]["configure_outbound"], false);
}
