//! HTTP writeback (docs/design/EVENTS.md Part D, build step 12) against a
//! local receiver (axum on 127.0.0.1, allow-listed with
//! `[[webhooks.allow]] host = "127.0.0.1", cidrs = ["127.0.0.1/32"]`): an
//! action type's writeback effect is posted after the action commits, with
//! `Idempotency-Key: <receipt>:<effect>` (the same on every retry) and the
//! Standard Webhooks signature; plans are refused while `[writeback]
//! enabled` is off; a restricted target ignores the allow list; a dead row is
//! audited as `writeback.failed`; targets are managed by admins of every
//! workspace they serve, never agents, and share their names with webhook
//! endpoints.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{Value, json};

use onto_intelli::config::AllowConfig;
use onto_intelli::core::EventFilter;
use onto_intelli::core::outbox::{ConfigEvent, OutboxQuery, OutboxState};
use onto_intelli::hub::{Secret, SecretBox};
use onto_intelli::webhooks;
use onto_intelli::writeback::{NS_TARGET, Target};
use onto_intelli::{ActionContext, Config, Intelli, Principal, Result, Role};

// ── the receiver ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Hit {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Hit {
    fn header(&self, k: &str) -> String {
        self.headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string()
    }
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
}

async fn handle(State(r): State<Receiver>, uri: Uri, headers: HeaderMap, body: Bytes) -> StatusCode {
    r.hits.lock().push(Hit { path: uri.path().to_string(), headers, body: body.to_vec() });
    let status = r.script.lock().pop().unwrap_or(200);
    StatusCode::from_u16(status).unwrap()
}

// ── the tenant ─────────────────────────────────────────────────────────

/// Seals by encoding (tests only): outputs never show the secret, and
/// requests sign with it.
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

async fn open(writeback: bool) -> Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    cfg.webhooks.poll_secs = 1;
    cfg.webhooks.backoff_base_secs = 1;
    cfg.webhooks.backoff_max_secs = 1;
    cfg.webhooks.allow = vec![AllowConfig { host: "127.0.0.1".into(), cidrs: vec!["127.0.0.1/32".into()] }];
    cfg.writeback.enabled = writeback;
    let i = Intelli::open(cfg).await.unwrap();
    i.set_secrets(Some(Arc::new(TestBox)));
    ok(&i, &lead(), "entity.upsert", json!({ "id": "issue:1", "title": "Payment timeout", "state": "open" })).await;
    let at = json!({
        "description": "Resolve an issue and close its helpdesk ticket.",
        "parameters": { "issue": { "type": "entity", "kinds": ["issue"], "required": true } },
        "preconditions": [{ "check": "open", "param": "issue" }],
        "effects": [
            { "patch": { "id": "{issue}", "state": "resolved" } },
            { "writeback": { "target": "helpdesk", "route": "close",
                             "body": { "issue": "{issue}", "title": "{issue.title}" } } }
        ]
    });
    let change = json!({ "op": "put_action_type", "name": "close_ticket", "def": at });
    ok(&i, &lead(), "schema.apply", json!({ "changes": [change] })).await;
    i
}

fn lead() -> Principal {
    Principal::admin("lead")
}

async fn ok(i: &Intelli, p: &Principal, action: &str, input: Value) -> Value {
    i.call(p, action, input.clone()).await.unwrap_or_else(|e| panic!("{action} {input}: {e}"))
}

fn events(i: &Intelli, action: &str) -> Vec<onto_intelli::Event> {
    let f = EventFilter { action: Some(action.into()), ..Default::default() };
    i.ontology().events(&lead(), &f).unwrap()
}

/// Wait (15 s at most) until `done`.
async fn until(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(15), "waited for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn row_state(i: &Intelli, id: &str) -> OutboxState {
    i.ontology().store().outbox_get(id).unwrap().unwrap().state
}

fn apply(confirm: bool) -> Value {
    json!({ "action": "close_ticket", "params": { "issue": "issue:1" }, "confirm": confirm })
}

// ── tests ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writeback_posts_the_body_with_an_idempotency_key_and_a_signature() {
    let rx = Receiver::start(vec![503]).await;
    let i = open(true).await;
    let added = ok(
        &i,
        &lead(),
        "writeback.add",
        json!({ "name": "helpdesk", "base_url": rx.url("/api"), "routes": { "close": "tickets/close" },
                "generate_secret": true }),
    )
    .await;
    let secret = added["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("whsec_"), "{added}");
    assert!(added["target"]["secret"].as_str().unwrap().starts_with("sealed"), "{added}");
    assert!(added["note"].as_str().unwrap().contains("not reversible"));
    let list = ok(&i, &lead(), "writeback.list", json!({})).await;
    assert!(!list.to_string().contains(&secret), "{list}");
    assert_eq!(list["items"][0]["routes"], json!({ "close": "tickets/close" }));
    assert_eq!(list["enabled"], true);

    // A person must confirm the external write.
    let held = ok(&i, &lead(), "action.apply", apply(false)).await;
    assert_eq!(held["status"], "proposed", "{held}");
    assert!(held["reason"].as_str().unwrap().contains("action.undo does not undo it"), "{held}");
    assert_eq!(held["plan"]["external"][0]["target"], "helpdesk");
    assert!(rx.hits().is_empty());

    let r = ok(&i, &lead(), "action.apply", apply(true)).await;
    assert_eq!(r["status"], "applied", "{r}");
    let receipt = r["id"].as_str().unwrap().to_string();
    let key = format!("{receipt}:1");
    assert_eq!(r["external"][0]["idempotency_key"], key);
    let row = r["external"][0]["outbox"].as_str().unwrap().to_string();

    // 503, then 200: every attempt carries the same key and body.
    until("a retry and a delivery", || rx.hits().len() == 2).await;
    until("the row delivered", || row_state(&i, &row) == OutboxState::Delivered).await;
    let hits = rx.hits();
    for h in &hits {
        assert_eq!(h.path, "/api/tickets/close");
        assert_eq!(h.header("idempotency-key"), key);
        assert_eq!(h.header("content-type"), "application/json");
        assert_eq!(h.header("webhook-id"), row);
        let body: Value = serde_json::from_slice(&h.body).unwrap();
        assert_eq!(body, json!({ "issue": "issue:1", "title": "Payment timeout" }));
        let now = onto_intelli::core::time::now_ms() / 1000;
        webhooks::verify(
            &[&secret],
            &h.header("webhook-id"),
            &h.header("webhook-timestamp"),
            &h.header("webhook-signature"),
            &h.body,
            now,
            webhooks::TOLERANCE_SECS,
        )
        .expect("the signature verifies");
    }
    // The receipt joins the row's state; the outbox lists it.
    let read = ok(&i, &lead(), "action.receipt", json!({ "id": receipt })).await;
    assert_eq!(
        (read["external"][0]["state"].as_str(), read["external"][0]["attempts"].as_u64()),
        (Some("delivered"), Some(2))
    );
    let ob = ok(&i, &lead(), "outbox.list", json!({ "receipt": receipt })).await;
    assert_eq!(ob["items"][0]["kind"], "writeback", "{ob}");
    // Undo never undoes the external write.
    let e = i.call(&lead(), "action.undo", json!({ "receipt": receipt })).await.unwrap_err();
    assert!(e.to_string().contains("action.undo does not undo it"), "{e}");
    let u = ok(&i, &lead(), "action.undo", json!({ "receipt": receipt, "force": true })).await;
    assert!(u["reason"].as_str().unwrap().contains("the external write was not undone"), "{u}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rx.hits().len(), 2, "undo sends nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writeback_is_refused_without_writeback_enabled() {
    let rx = Receiver::start(vec![]).await;
    let i = open(false).await;
    // Targets can be registered; plans that use them are refused.
    let added = ok(
        &i,
        &lead(),
        "writeback.add",
        json!({ "name": "helpdesk", "base_url": rx.url("/"), "routes": { "close": "close" } }),
    )
    .await;
    assert_eq!(added["enabled"], false);
    assert!(added["enabled_note"].as_str().unwrap().contains("refused"), "{added}");
    let plan =
        ok(&i, &lead(), "action.plan", json!({ "action": "close_ticket", "params": { "issue": "issue:1" } })).await;
    assert_eq!(plan["valid"], false);
    assert!(plan["checks"].to_string().contains("[writeback] enabled = false"), "{plan}");
    let r = ok(&i, &lead(), "action.apply", apply(true)).await;
    assert_eq!(r["status"], "rejected", "{r}");
    assert!(r["reason"].as_str().unwrap().contains("writeback is off on this server"), "{r}");
    let issue = ok(&i, &lead(), "entity.get", json!({ "id": "issue:1" })).await;
    assert_eq!(issue["state"], "open", "nothing applied: {issue}");
    assert!(i.ontology().store().outbox_list(&OutboxQuery::default()).unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rx.hits().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restricted_target_ignores_the_allow_list() {
    let rx = Receiver::start(vec![]).await;
    let i = open(true).await;
    // Registered by a project credential (the server sets `restricted`):
    // the allow-listed loopback address is refused when written ...
    let e = i
        .call(
            &lead(),
            "writeback.add",
            json!({ "name": "helpdesk", "base_url": format!("https://127.0.0.1:{}/", rx.port),
                    "routes": { "close": "close" }, "restricted": true }),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("a project-registered target reaches public addresses only"), "{e}");
    // ... and when sent (a restricted target stored by other means).
    let t = Target {
        name: "helpdesk".into(),
        base_url: format!("https://127.0.0.1:{}/", rx.port),
        secret: None,
        routes: [("close".to_string(), "close".to_string())].into_iter().collect(),
        workspaces: vec!["acme".into()],
        restricted: true,
        version: 1,
        created_by: "lead".into(),
        created_at: 0,
        incarnation: String::new(),
    };
    let ctx = ActionContext::new(lead(), "acme");
    let kv = vec![(NS_TARGET.to_string(), t.name.clone(), serde_json::to_value(&t).unwrap())];
    i.ontology()
        .commit_config(&ctx, kv, ConfigEvent::new("writeback.add", Some("writeback:helpdesk"), json!({})))
        .unwrap();
    let r = ok(&i, &lead(), "action.apply", apply(true)).await;
    assert_eq!(r["status"], "applied", "{r}");
    let row = r["external"][0]["outbox"].as_str().unwrap().to_string();
    until("the row refused", || row_state(&i, &row) == OutboxState::Dead).await;
    let stored = i.ontology().store().outbox_get(&row).unwrap().unwrap();
    let err = stored.last_error.unwrap();
    assert!(err.starts_with("refused:") && err.contains("public addresses only"), "{err}");
    assert!(rx.hits().is_empty(), "nothing reached the allow-listed address");
    // A dead writeback is audited, naming the receipt and the key.
    until("the writeback.failed event", || !events(&i, "writeback.failed").is_empty()).await;
    let ev = &events(&i, "writeback.failed")[0];
    assert_eq!(ev.payload["receipt"], r["id"]);
    assert_eq!(ev.payload["idempotency_key"], format!("{}:1", r["id"].as_str().unwrap()));
    assert_eq!(ev.target.as_deref(), Some("writeback:helpdesk"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_error_is_dead_and_audited() {
    let rx = Receiver::start(vec![400]).await;
    let i = open(true).await;
    ok(
        &i,
        &lead(),
        "writeback.add",
        json!({ "name": "helpdesk", "base_url": rx.url("/"), "routes": { "close": "close" } }),
    )
    .await;
    let r = ok(&i, &lead(), "action.apply", apply(true)).await;
    let row = r["external"][0]["outbox"].as_str().unwrap().to_string();
    until("the row dead", || row_state(&i, &row) == OutboxState::Dead).await;
    assert_eq!(rx.hits().len(), 1, "a 400 is not retried");
    assert!(rx.hits()[0].header("webhook-signature").is_empty(), "a target without a secret is unsigned");
    until("the writeback.failed event", || !events(&i, "writeback.failed").is_empty()).await;
    assert_eq!(events(&i, "writeback.failed")[0].payload["status"], 400);
    let read = ok(&i, &lead(), "action.receipt", json!({ "id": r["id"] })).await;
    assert_eq!(read["external"][0]["state"], "dead");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn targets_are_managed_by_admins_of_every_workspace_they_serve() {
    let rx = Receiver::start(vec![]).await;
    let i = open(true).await;
    ok(
        &i,
        &lead(),
        "writeback.add",
        json!({ "name": "broad", "base_url": rx.url("/in?token=abc"), "routes": { "close": "close" },
                "workspaces": ["*"] }),
    )
    .await;
    let narrow = Principal::new("user:ws1-admin", vec![Role::Admin], vec!["ws1".into()]);
    let before = events(&i, "authorization.denied").len();
    for (action, input) in [
        ("writeback.remove", json!({ "name": "broad" })),
        ("writeback.test", json!({ "name": "broad" })),
        (
            "writeback.add",
            json!({ "name": "wide", "base_url": rx.url("/"), "routes": { "a": "a" }, "workspaces": "*" }),
        ),
    ] {
        let e = i.call(&narrow, action, input).await.unwrap_err();
        assert_eq!(e.code(), "permission_denied", "{action}: {e}");
        assert!(e.to_string().contains("only an admin of every one of them"), "{action}: {e}");
    }
    assert_eq!(events(&i, "authorization.denied").len() - before, 3, "every refusal is audited");
    // It sees the target's origin and route names only.
    let list = ok(&i, &narrow, "writeback.list", json!({})).await;
    let item = &list["items"][0];
    assert_eq!((item["base_url"].as_str(), item["may_manage"].as_bool()), (Some(rx.url("").as_str()), Some(false)));
    assert_eq!(item["routes"], json!(["close"]));
    // Within its own workspaces it may register targets.
    ok(
        &i,
        &narrow,
        "writeback.add",
        json!({ "name": "mine", "base_url": rx.url("/"), "routes": { "a": "a" }, "workspaces": "ws1" }),
    )
    .await;
    // Members, viewers and agents may not configure targets at all.
    for p in [
        Principal::member("user:m", "acme"),
        Principal::viewer("user:v", "acme"),
        Principal::new("bot", vec![Role::Agent, Role::Admin], vec!["*".into()]),
    ] {
        for action in ["writeback.list", "writeback.add", "writeback.remove"] {
            let input = json!({ "name": "x", "base_url": rx.url("/"), "routes": { "a": "a" } });
            let e = i.call(&p, action, input).await.unwrap_err();
            assert_eq!(e.code(), "permission_denied", "{} {action}: {e}", p.id);
        }
    }
    // writeback.test checks without sending anything.
    let t = ok(&i, &lead(), "writeback.test", json!({ "name": "broad" })).await;
    assert_eq!((t["ok"].as_bool(), t["routes"][0]["ok"].as_bool()), (Some(true), Some(true)), "{t}");
    assert!(t["note"].as_str().unwrap().contains("nothing was sent"));
    assert!(rx.hits().is_empty());
    // Names are shared with webhook endpoints.
    ok(&i, &lead(), "webhook.add", json!({ "name": "ops", "url": rx.url("/") })).await;
    let e = i
        .call(&lead(), "writeback.add", json!({ "name": "ops", "base_url": rx.url("/"), "routes": { "a": "a" } }))
        .await
        .unwrap_err();
    assert_eq!(e.code(), "conflict", "{e}");
    let e = i.call(&lead(), "webhook.add", json!({ "name": "broad", "url": rx.url("/") })).await.unwrap_err();
    assert_eq!(e.code(), "conflict", "{e}");
    // Inputs: {} is refused without I/O; bad routes and URLs name themselves.
    for action in ["writeback.add", "writeback.remove", "writeback.test"] {
        let e = i.call(&lead(), action, json!({})).await.unwrap_err();
        assert_eq!(e.code(), "invalid_argument", "{action}: {e}");
    }
    for (routes, why) in [
        (json!({}), "at least one route"),
        (json!({ "a": "//evil.example/x" }), "may not contain //"),
        (json!({ "a": "../up" }), "may not contain .."),
        (json!({ "Bad": "a" }), "route name 'Bad'"),
    ] {
        let input = json!({ "name": "x", "base_url": rx.url("/"), "routes": routes });
        let e = i.call(&lead(), "writeback.add", input).await.unwrap_err();
        assert!(e.to_string().contains(why), "{why}: {e}");
    }
    let e = i
        .call(
            &lead(),
            "writeback.add",
            json!({ "name": "meta", "base_url": "https://169.254.169.254/", "routes": { "a": "a" } }),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("link-local"), "{e}");
    // Remove: audited; plans that name it are refused afterwards.
    ok(&i, &lead(), "writeback.remove", json!({ "name": "broad" })).await;
    for action in ["writeback.add", "writeback.remove"] {
        assert!(!events(&i, action).is_empty(), "{action} is audited");
    }
    let added = &events(&i, "writeback.add")[0];
    assert!(!added.payload.to_string().contains("abc"), "audit events name the host only: {:?}", added.payload);
}

/// A refused base URL names the field the caller gave it in: `base_url`, or
/// `url` (its alias), never the other one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writeback_refusals_name_the_field_the_caller_used() {
    let i = open(true).await;
    for field in ["base_url", "url"] {
        for (bad, why) in [
            ("ftp://desk.example.com/", "scheme ftp is not https"),
            ("https://169.254.169.254/", "link-local"),
            ("https://user:pw@desk.example.com/", "may not carry credentials"),
            ("", "needs an https URL"),
        ] {
            let input = json!({ "name": "x", field: bad, "routes": { "a": "a" } });
            let e = i.call(&lead(), "writeback.add", input).await.unwrap_err();
            assert_eq!(e.code(), "invalid_argument", "{field} {bad}: {e}");
            let msg = e.to_string();
            assert!(msg.contains(why), "{field} {bad}: {msg}");
            assert!(msg.starts_with(&format!("invalid: {field}")), "names {field}: {msg}");
        }
    }
    // Missing altogether: named by its canonical field.
    let e = i.call(&lead(), "writeback.add", json!({ "name": "x", "routes": { "a": "a" } })).await.unwrap_err();
    assert!(e.to_string().starts_with("invalid: base_url:"), "{e}");
    // Both: refused, naming both.
    let both = json!({ "name": "x", "base_url": "https://a.example.com/", "url": "https://b.example.com/",
                       "routes": { "a": "a" } });
    let e = i.call(&lead(), "writeback.add", both).await.unwrap_err();
    assert!(e.to_string().contains("give base_url or url"), "{e}");
}

/// SEC3-R: a writeback target removed and added again with the same version
/// and principal (registered within the same millisecond, say) is a new
/// incarnation: an automation that reaches it is stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_added_again_at_the_enabled_version_still_stops_the_automation() {
    let i = open(true).await;
    let store = i.ontology().store().clone();
    let add =
        json!({ "name": "helpdesk", "base_url": "https://desk.example.com/", "routes": { "close": "tickets/close" } });
    ok(&i, &lead(), "writeback.add", add).await;
    let record = || store.kv_get(NS_TARGET, "helpdesk").unwrap().unwrap();
    let enabled = record();
    let def = json!({
        "run_as": { "roles": ["member"], "workspaces": ["acme"] },
        "trigger": { "on": "change", "kinds": ["issue"], "ops": ["entity.update"] },
        "effects": [{ "action": { "type": "close_ticket", "params": { "issue": "{trigger.id}" } } }]
    });
    ok(&i, &lead(), "schema.apply", json!({ "changes": [{ "op": "put_automation", "name": "close", "def": def }] }))
        .await;
    ok(&i, &lead(), "automation.enable", json!({ "name": "close" })).await;
    let got = ok(&i, &lead(), "automation.get", json!({ "name": "close" })).await;
    assert_eq!(got["status"], "active", "{got}");
    ok(&i, &lead(), "writeback.remove", json!({ "name": "helpdesk" })).await;
    let again = json!({ "name": "helpdesk", "base_url": "https://elsewhere.example.net/", "routes": { "close": "tickets/close" } });
    ok(&i, &lead(), "writeback.add", again).await;
    // The new target's record at exactly the enabled version and
    // registration time, with the same principal.
    let mut rec = record();
    assert_ne!(rec["incarnation"], enabled["incarnation"], "{rec}");
    rec["version"] = enabled["version"].clone();
    rec["created_at"] = enabled["created_at"].clone();
    let ctx = ActionContext::new(lead(), "acme");
    let event = ConfigEvent::new("writeback.add", Some("writeback:helpdesk"), json!({}));
    i.ontology().commit_config(&ctx, vec![(NS_TARGET.to_string(), "helpdesk".to_string(), rec)], event).unwrap();
    let got = ok(&i, &lead(), "automation.get", json!({ "name": "close" })).await;
    assert_eq!(
        (got["status"].as_str(), got["reason"].as_str()),
        (Some("inactive"), Some("stale_definition: target:helpdesk")),
        "{got}"
    );
    assert_eq!(got["closure"]["changed_since_enabled"], json!(["target:helpdesk"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writeback_continues_the_callers_trace_and_traces_failed_attempts() {
    const OTEL: &str = "0af7651916cd43dd8448eb211c80319c";
    let rx = Receiver::start(vec![503]).await;
    let i = open(true).await;
    let added = ok(
        &i,
        &lead(),
        "writeback.add",
        json!({ "name": "helpdesk", "base_url": rx.url("/api"), "routes": { "close": "tickets/close" },
                "generate_secret": true }),
    )
    .await;
    let secret = added["secret"].as_str().unwrap().to_string();
    let ctx = onto_intelli::trace::TraceCtx::from_source("http")
        .with_traceparent(Some(&format!("00-{OTEL}-b7ad6b7169203331-01")));
    let t = i.call_traced(&lead(), "action.apply", apply(true), ctx).await;
    let r = t.result.unwrap();
    assert_eq!(r["status"], "applied", "{r}");
    let row = r["external"][0]["outbox"].as_str().unwrap().to_string();
    assert_eq!(i.ontology().store().outbox_get(&row).unwrap().unwrap().otel_trace_id.as_deref(), Some(OTEL));
    until("the row delivered", || row_state(&i, &row) == OutboxState::Delivered).await;
    let hits = rx.hits();
    assert_eq!(hits.len(), 2);
    let spans: Vec<String> = hits
        .iter()
        .map(|h| {
            let tp = h.header("traceparent");
            assert!(tp.starts_with(&format!("00-{OTEL}-")) && tp.ends_with("-01") && tp.len() == 55, "{tp}");
            tp[36..52].to_string()
        })
        .collect();
    assert_ne!(spans[0], spans[1], "a span per attempt");

    // [traces] deliveries = "failures": the 503 only, under the caller's trace.
    i.flush_traces().await;
    let list = ok(&i, &lead(), "trace.list", json!({ "kind": "delivery" })).await;
    let traces = list["traces"].as_array().unwrap();
    assert_eq!(traces.len(), 1, "{list}");
    let d = ok(&i, &lead(), "trace.get", json!({ "id": traces[0]["id"] })).await;
    assert_eq!(
        (d["trace"]["name"].as_str(), d["trace"]["otel_trace_id"].as_str(), d["trace"]["error_code"].as_str()),
        (Some("writeback.deliver:helpdesk"), Some(OTEL), Some("retry"))
    );
    let root = d["spans"].as_array().unwrap().iter().find(|s| s["parent_id"].is_null()).unwrap();
    assert_eq!(root["id"], spans[0].as_str(), "the span the receiver was given");
    assert_eq!((root["attrs"]["http_status"].as_u64(), root["attrs"]["kind"].as_str()), (Some(503), Some("writeback")));
    let text = d.to_string();
    assert!(!text.contains(&secret) && !text.contains(&secret["whsec_".len()..]), "{text}");
    for h in &hits {
        let sig = h.header("webhook-signature");
        assert!(!sig.is_empty() && !text.contains(&sig) && !text.contains(&sig["v1,".len()..]), "{text}");
    }
    assert!(!text.contains("Payment timeout") && !text.contains(&rx.url("")), "no body and no URL: {text}");
    // The action's own trace is in the same distributed trace.
    let call = ok(&i, &lead(), "trace.get", json!({ "id": t.trace_id.unwrap() })).await;
    assert_eq!(call["trace"]["otel_trace_id"], OTEL);
}
