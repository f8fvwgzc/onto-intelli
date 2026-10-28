//! Automate through the action surface (docs/design/EVENTS.md Part C).
//!
//! Build step 10: `put_automation` / `delete_automation` in
//! `schema.plan|apply|propose`, `[automation] max_automations` enforced at
//! apply and propose, and the automation lint checks in `schema.lint`.
//!
//! Build step 11: the runtime. A change runs an enabled automation, and its
//! webhook effect reaches a local receiver signed (Standard Webhooks) with
//! the cause in the CloudEvent; who may enable (the matrix, with audited
//! refusals); an agent proposes, an approver applies and enables; the kill
//! switch, the breaker, rate limits writing skipped rows that a rerun runs,
//! the depth limit stopping an A→B→A cascade, a changed closure
//! (`stale_definition`), an expired enablement, refused plans, dry runs and
//! `{}` inputs.

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
use onto_intelli::core::automate::{AutomationState, NS_AUTOMATION_STATE};
use onto_intelli::core::outbox::ConfigEvent;
use onto_intelli::hub::{Secret, SecretBox};
use onto_intelli::webhooks;
use onto_intelli::{ActionContext, Config, Intelli, Principal, Role};

fn put(name: &str) -> Value {
    json!({
        "op": "put_automation",
        "name": name,
        "def": {
            "description": "Resolve a blocked issue.",
            "run_as": { "roles": ["member"], "workspaces": ["acme"] },
            "trigger": { "on": "change", "kinds": ["issue"], "ops": ["entity.update"], "state": { "to": ["blocked"] } },
            "effects": [{ "action": { "type": "resolve", "params": { "issue": "{trigger.id}" } } }]
        }
    })
}

#[tokio::test]
async fn definitions_go_through_schema_actions_with_the_configured_limit() {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    cfg.automation.max_automations = 1;
    let i = Intelli::open(cfg).await.unwrap();
    let admin = Principal::admin("ada");
    let agent = Principal::new("assistant", vec![Role::Agent], vec!["*".into()]);

    let plan = i.call(&admin, "schema.plan", json!({ "changes": [put("first")] })).await.unwrap();
    assert_eq!(plan["valid"], true, "{plan}");
    assert_eq!(plan["changes"][0]["op"], "put_automation");
    i.call(&admin, "schema.apply", json!({ "changes": [put("first")] })).await.unwrap();
    let status = i.call(&admin, "schema.status", json!({})).await.unwrap();
    assert_eq!(status["automations"], 1);
    let doc = i.call(&admin, "schema.get", json!({})).await.unwrap();
    assert_eq!(doc["automations"]["first"]["trigger"]["on"], "change", "{doc}");

    // The process limit: refused at plan, apply and propose, with the setting named.
    let plan = i.call(&admin, "schema.plan", json!({ "changes": [put("second")] })).await.unwrap();
    assert_eq!(plan["valid"], false);
    assert!(plan["errors"].to_string().contains("max_automations"), "{plan}");
    let err = i.call(&admin, "schema.apply", json!({ "changes": [put("second")] })).await.unwrap_err();
    assert_eq!(err.code(), "invalid_argument");
    assert!(err.to_string().contains("the limit is 1 ([automation] max_automations)"), "{err}");
    let err =
        i.call(&agent, "schema.propose", json!({ "title": "Another", "changes": [put("second")] })).await.unwrap_err();
    assert!(err.to_string().contains("max_automations"), "{err}");
    // Agents propose definitions; they never apply them.
    let err = i.call(&agent, "schema.apply", json!({ "changes": [{ "op": "delete_automation", "name": "first" }] }));
    assert_eq!(err.await.unwrap_err().code(), "permission_denied");
    let filed = i
        .call(
            &agent,
            "schema.propose",
            json!({ "title": "Retire", "changes": [{ "op": "delete_automation", "name": "first" }] }),
        )
        .await
        .unwrap();
    assert_eq!(filed["status"], "open", "{filed}");

    // Lint lists the automation checks; the closure check reads the stored
    // enablements (none here, so it finds nothing).
    let lint = i.call(&admin, "schema.lint", json!({})).await.unwrap();
    let run: Vec<&str> = lint["checks_run"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    for check in [
        "automation_reference",
        "automation_privilege",
        "automation_loop",
        "automation_closure_changed",
        "automation_gap_blind",
    ] {
        assert!(run.contains(&check), "{check}: {run:?}");
    }
    let gap: Vec<&Value> =
        lint["findings"].as_array().unwrap().iter().filter(|f| f["check"] == "automation_gap_blind").collect();
    assert_eq!(gap.len(), 1);
    assert_eq!(gap[0]["target"], "automation:first");
}

// ── build step 11: the runtime ───────────────────────────────────────────

/// Resolve a high-priority issue when it becomes blocked, and tell `ops`.
fn resolve_blocked(webhook: bool) -> Value {
    let mut effects = vec![
        json!({ "action": { "type": "resolve", "params": { "issue": "{trigger.id}", "note": "auto: {subject.title}" } } }),
    ];
    if webhook {
        effects.push(json!({ "webhook": { "endpoint": "ops", "event": "issue.resolved", "payload": {
            "id": "{trigger.id}", "title": "{subject.title}", "from": "{trigger.state.from}"
        } } }));
    }
    json!({
        "description": "Resolve a high-priority issue when it becomes blocked.",
        "run_as": { "roles": ["member"], "workspaces": ["acme"] },
        "trigger": { "on": "change", "kinds": ["issue"], "ops": ["entity.update"], "state": { "to": ["blocked"] } },
        "conditions": [{ "check": "prop_eq", "param": "subject", "prop": "priority", "value": "high" }],
        "effects": effects,
    })
}

fn admin() -> Principal {
    Principal::admin("lead")
}

async fn open(tweak: impl FnOnce(&mut Config)) -> Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    cfg.automation.workers = false;
    cfg.webhooks.enabled = false;
    tweak(&mut cfg);
    let i = Intelli::open(cfg).await.unwrap();
    i.set_secrets(Some(Arc::new(TestBox)));
    i
}

async fn ok(i: &Intelli, p: &Principal, action: &str, input: Value) -> Value {
    i.call(p, action, input.clone()).await.unwrap_or_else(|e| panic!("{action} {input}: {e}"))
}

async fn define(i: &Intelli, name: &str, def: Value) {
    ok(i, &admin(), "schema.apply", json!({ "changes": [{ "op": "put_automation", "name": name, "def": def }] })).await;
}

/// The endpoints' signing secret (the Standard Webhooks reference secret).
const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

/// Seals by encoding (tests only).
struct TestBox;

impl SecretBox for TestBox {
    fn seal(&self, purpose: &str, name: &str, plain: &[u8]) -> onto_intelli::Result<String> {
        let mut blob = format!("{purpose}/{name}/").into_bytes();
        blob.extend_from_slice(plain);
        Ok(base64::engine::general_purpose::STANDARD.encode(blob))
    }

    fn open(&self, purpose: &str, name: &str, sealed: &str) -> onto_intelli::Result<Secret> {
        let blob = base64::engine::general_purpose::STANDARD.decode(sealed).unwrap();
        let prefix = format!("{purpose}/{name}/");
        Ok(Secret::new(blob[prefix.len()..].to_vec()))
    }
}

async fn endpoint(i: &Intelli, url: &str, principal: Value) {
    ok(i, &admin(), "webhook.add", json!({ "name": "ops", "url": url, "secret": SECRET, "principal": principal }))
        .await;
}

async fn issue(i: &Intelli, key: &str, state: &str, priority: &str) {
    let mut input =
        json!({ "id": format!("issue:{key}"), "title": format!("Issue {key}"), "props": { "priority": priority } });
    if !state.is_empty() {
        input["state"] = json!(state);
    }
    ok(i, &admin(), "entity.upsert", input).await;
}

async fn set_state(i: &Intelli, key: &str, state: &str) {
    ok(i, &admin(), "entity.upsert", json!({ "id": format!("issue:{key}"), "state": state })).await;
}

/// Run consumer rounds until one finds nothing more to do.
fn drain(i: &Intelli) -> Vec<onto_intelli::automate::RoundReport> {
    let mut out = Vec::new();
    for _ in 0..20 {
        let r = i.automation_round().unwrap();
        let quiet = !r.more && r.executed == 0 && r.settled == 0 && r.retried == 0 && r.recovered == 0;
        out.push(r);
        if quiet {
            break;
        }
    }
    out
}

async fn runs(i: &Intelli, name: &str) -> Vec<Value> {
    ok(i, &admin(), "automation.runs", json!({ "name": name })).await["items"].as_array().unwrap().clone()
}

fn events(i: &Intelli, action: &str) -> Vec<onto_intelli::Event> {
    let f = EventFilter { action: Some(action.into()), ..Default::default() };
    i.ontology().events(&admin(), &f).unwrap()
}

fn state_of(i: &Intelli, id: &str) -> Option<String> {
    i.ontology().get(&admin(), id).unwrap().state
}

/// One request the receiver got: its headers and body.
type Hit = (HeaderMap, Vec<u8>);

#[derive(Clone, Default)]
struct Receiver {
    hits: Arc<Mutex<Vec<Hit>>>,
    port: u16,
}

async fn handle(State(r): State<Receiver>, headers: HeaderMap, body: Bytes) -> StatusCode {
    r.hits.lock().push((headers, body.to_vec()));
    StatusCode::OK
}

impl Receiver {
    async fn start() -> Receiver {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let r = Receiver { hits: Default::default(), port };
        let app = axum::Router::new().fallback(handle).with_state(r.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        r
    }
}

async fn until(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(15), "waited for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_runs_an_enabled_automation_and_its_webhook_arrives_signed() {
    let rx = Receiver::start().await;
    let i = open(|cfg| {
        cfg.automation.workers = true;
        cfg.webhooks.enabled = true;
        cfg.webhooks.poll_secs = 1;
        cfg.webhooks.allow = vec![AllowConfig { host: "127.0.0.1".into(), cidrs: vec!["127.0.0.1/32".into()] }];
    })
    .await;
    endpoint(
        &i,
        &format!("http://127.0.0.1:{}/hooks", rx.port),
        json!({ "roles": ["member"], "workspaces": ["acme"] }),
    )
    .await;
    define(&i, "resolve_blocked", resolve_blocked(true)).await;
    issue(&i, "1", "triaged", "high").await;
    // Defined but not enabled: nothing runs.
    set_state(&i, "1", "in_progress").await;
    let enabled = ok(&i, &admin(), "automation.enable", json!({ "name": "resolve_blocked" })).await;
    assert_eq!(enabled["enabled"], true, "{enabled}");
    assert!(enabled["note"].as_str().unwrap().contains("not reversible"), "{enabled}");
    set_state(&i, "1", "blocked").await;

    until("the run", || state_of(&i, "issue:1").as_deref() == Some("resolved")).await;
    until("the delivery", || !rx.hits.lock().is_empty()).await;
    let all = runs(&i, "resolve_blocked").await;
    assert_eq!(all.len(), 1, "{all:?}");
    let run = &all[0];
    assert_eq!(run["state"], "succeeded", "{run}");
    assert_eq!(run["depth"], 1);
    assert_eq!(run["principal"], "automation:resolve_blocked");
    assert_eq!(run["trigger"]["state"]["from"], "in_progress");
    let receipt_id = run["effects"][0]["receipt"].as_str().unwrap();
    let receipt = ok(&i, &admin(), "action.receipt", json!({ "id": receipt_id })).await;
    assert_eq!(receipt["actor"], "automation:resolve_blocked", "{receipt}");
    assert_eq!(receipt["plan"]["params"]["note"], "auto: Issue 1");

    // The delivery: a CloudEvent carrying the cause, signed with the endpoint's secret.
    let (headers, body) = rx.hits.lock()[0].clone();
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let ev = webhooks::verify(
        &[SECRET],
        &h("webhook-id"),
        &h("webhook-timestamp"),
        &h("webhook-signature"),
        &body,
        onto_intelli::core::time::now_ms() / 1000,
        webhooks::TOLERANCE_SECS,
    )
    .unwrap();
    assert_eq!(ev["type"], "onto.issue.resolved");
    assert_eq!(ev["data"], json!({ "id": "issue:1", "title": "Issue 1", "from": "in_progress" }));
    assert_eq!((ev["causeautomation"].as_str(), ev["causedepth"].as_u64()), (Some("resolve_blocked"), Some(1)));
    assert_eq!(ev["causerun"], run["id"]);
    assert_eq!(ev["id"], run["effects"][1]["outbox"]);
    // Audited: the enablement and the run.
    assert_eq!(events(&i, "automation.enable").len(), 1);
    let run_events = events(&i, "automation.run");
    assert_eq!(run_events.len(), 1);
    assert_eq!(run_events[0].actor, "automation:resolve_blocked");
    // Its own change does not trigger it again (allow_self is off), and the
    // in_progress change before the enablement started nothing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(runs(&i, "resolve_blocked").await.len(), 1);
}

#[tokio::test]
async fn who_may_enable_and_refusals_are_audited() {
    let i = open(|_| {}).await;
    let mut wide = resolve_blocked(false);
    wide["run_as"] = json!({ "roles": ["member"], "workspaces": ["*"] });
    define(&i, "wide", wide).await;
    define(&i, "narrow", resolve_blocked(false)).await;
    let scoped_admin = Principal::named("user:ws1", &["admin"], &["acme"]).unwrap();
    let viewer = Principal::named("user:v", &["viewer"], &["*"]).unwrap();
    let member = Principal::named("user:m", &["member"], &["*"]).unwrap();
    let agent = Principal::new("assistant", vec![Role::Agent], vec!["*".into()]);
    // A scoped admin covers the acme run_as, not the `*` one.
    ok(&i, &scoped_admin, "automation.enable", json!({ "name": "narrow" })).await;
    let denied_before = events(&i, "authorization.denied").len();
    for (p, name) in [(&scoped_admin, "wide"), (&viewer, "narrow"), (&member, "narrow"), (&agent, "narrow")] {
        let e = i.call(p, "automation.enable", json!({ "name": name })).await.unwrap_err();
        assert_eq!(e.code(), "permission_denied", "{} {name}: {e}", p.id);
    }
    assert!(
        events(&i, "authorization.denied").len() >= denied_before + 3,
        "refusals are audited as authorization.denied"
    );
    // An admin over every workspace enables the wide one.
    ok(&i, &admin(), "automation.enable", json!({ "name": "wide" })).await;
    let list = ok(&i, &member, "automation.list", json!({})).await;
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|a| a["status"] == "active" && a["may_enable"] == false), "{list}");
    assert!(list["note"].as_str().unwrap().contains("visible to everyone who can read the schema"));
    // Stopping: whoever may enable it, or any admin; never a member.
    let e = i.call(&member, "automation.disable", json!({ "name": "narrow" })).await.unwrap_err();
    assert_eq!(e.code(), "permission_denied");
    ok(&i, &scoped_admin, "automation.disable", json!({ "name": "wide" })).await;
    let got = ok(&i, &admin(), "automation.get", json!({ "name": "wide" })).await;
    assert_eq!((got["enabled"].as_bool(), got["reason"].as_str()), (Some(false), Some("not_enabled")), "{got}");
    assert!(got["means"].as_str().unwrap().contains("evaluated against the graph at the run's commit"));
    // An endpoint that would see less than the run renders cannot be reached.
    endpoint(&i, "https://hooks.example.com/in", json!({ "roles": ["viewer"], "workspaces": ["acme"] })).await;
    define(&i, "tell", resolve_blocked(true)).await;
    let e = i.call(&admin(), "automation.enable", json!({ "name": "tell" })).await.unwrap_err();
    assert_eq!(e.code(), "permission_denied", "{e}");
    assert!(e.to_string().contains("webhook endpoint ops may see less"), "{e}");
}

#[tokio::test]
async fn an_agent_proposes_an_approver_applies_and_enables() {
    let i = open(|_| {}).await;
    let agent = Principal::new("assistant", vec![Role::Agent], vec!["*".into()]);
    let filed = ok(
        &i,
        &agent,
        "schema.propose",
        json!({ "title": "Resolve blocked", "changes": [{ "op": "put_automation", "name": "resolve_blocked", "def": resolve_blocked(false) }] }),
    )
    .await;
    let id = filed["id"].as_str().or(filed["proposal"]["id"].as_str()).unwrap_or_else(|| panic!("{filed}")).to_string();
    let e = i.call(&agent, "schema.approve", json!({ "id": id })).await.unwrap_err();
    assert_eq!(e.code(), "permission_denied");
    ok(&i, &admin(), "schema.approve", json!({ "id": id })).await;
    // Defining never enables: the agent cannot, an approver can.
    let got = ok(&i, &agent, "automation.get", json!({ "name": "resolve_blocked" })).await;
    assert_eq!(got["reason"], "not_enabled");
    assert_eq!(
        i.call(&agent, "automation.enable", json!({ "name": "resolve_blocked" })).await.unwrap_err().code(),
        "permission_denied"
    );
    ok(&i, &admin(), "automation.enable", json!({ "name": "resolve_blocked" })).await;
    issue(&i, "1", "triaged", "high").await;
    set_state(&i, "1", "blocked").await;
    drain(&i);
    assert_eq!(state_of(&i, "issue:1").as_deref(), Some("resolved"));
    // A low-priority issue does not meet the condition: no run at all.
    issue(&i, "2", "triaged", "low").await;
    set_state(&i, "2", "blocked").await;
    drain(&i);
    assert_eq!(state_of(&i, "issue:2").as_deref(), Some("blocked"));
    assert_eq!(runs(&i, "resolve_blocked").await.len(), 1);
}

#[tokio::test]
async fn the_kill_switch_stops_runs_and_the_process_switch_runs_none() {
    let i = open(|_| {}).await;
    define(&i, "resolve_blocked", resolve_blocked(false)).await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "resolve_blocked" })).await;
    issue(&i, "1", "triaged", "high").await;
    let member = Principal::named("user:m", &["member"], &["*"]).unwrap();
    assert_eq!(
        i.call(&member, "automation.kill", json!({ "on": true })).await.unwrap_err().code(),
        "permission_denied"
    );
    let k = ok(&i, &admin(), "automation.kill", json!({ "on": true })).await;
    assert_eq!(k["kill"], true);
    set_state(&i, "1", "blocked").await;
    drain(&i);
    assert_eq!(state_of(&i, "issue:1").as_deref(), Some("blocked"), "killed: nothing runs");
    let got = ok(&i, &admin(), "automation.get", json!({ "name": "resolve_blocked" })).await;
    assert_eq!(got["reason"], "killed");
    ok(&i, &admin(), "automation.kill", json!({ "off": true })).await;
    drain(&i);
    assert_eq!(state_of(&i, "issue:1").as_deref(), Some("blocked"), "changes made while killed never start runs");
    set_state(&i, "1", "in_progress").await;
    set_state(&i, "1", "blocked").await;
    drain(&i);
    assert_eq!(state_of(&i, "issue:1").as_deref(), Some("resolved"));
    for input in [json!({}), json!({ "on": true, "off": true })] {
        assert_eq!(i.call(&admin(), "automation.kill", input).await.unwrap_err().code(), "invalid_argument");
    }

    // [automation] enabled = false: this process runs none.
    let off = open(|cfg| cfg.automation.enabled = false).await;
    define(&off, "resolve_blocked", resolve_blocked(false)).await;
    ok(&off, &admin(), "automation.enable", json!({ "name": "resolve_blocked" })).await;
    issue(&off, "1", "triaged", "high").await;
    set_state(&off, "1", "blocked").await;
    assert!(off.automation_round().unwrap().idle);
    assert_eq!(state_of(&off, "issue:1").as_deref(), Some("blocked"));
    let list = ok(&off, &admin(), "automation.list", json!({})).await;
    assert_eq!(list["process_enabled"], false);
}

/// An automation whose every run fails: its payload names a property the
/// subject does not have, and templates render strictly.
fn failing_def() -> Value {
    let mut d = resolve_blocked(false);
    d["conditions"] = json!([]);
    d["trigger"] = json!({ "on": "change", "kinds": ["issue"], "ops": ["entity.update"], "props": ["description"] });
    d["effects"] = json!([{ "webhook": { "endpoint": "ops", "event": "x", "payload": { "d": "{subject.nope}" } } }]);
    d
}

#[tokio::test]
async fn the_breaker_pauses_after_consecutive_failures() {
    let i = open(|cfg| cfg.automation.pause_after_failures = 2).await;
    endpoint(&i, "https://hooks.example.com/in", json!({ "roles": ["member"], "workspaces": ["acme"] })).await;
    define(&i, "echo", failing_def()).await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "echo" })).await;
    issue(&i, "1", "triaged", "high").await;
    for n in 0..3 {
        ok(&i, &admin(), "entity.upsert", json!({ "id": "issue:1", "props": { "description": format!("v{n}") } }))
            .await;
        drain(&i);
    }
    let all = runs(&i, "echo").await;
    assert_eq!(all.len(), 2, "the third change found it paused: {all:?}");
    assert!(all.iter().all(|r| r["state"] == "failed"), "{all:?}");
    assert!(all[0]["reason"].as_str().unwrap().contains("invalid_template"), "{all:?}");
    let got = ok(&i, &admin(), "automation.get", json!({ "name": "echo" })).await;
    assert!(got["reason"].as_str().unwrap().starts_with("paused: failing: 2 consecutive failed runs"), "{got}");
    assert_eq!(events(&i, "automation.paused").len(), 1);
}

#[tokio::test]
async fn rate_limits_write_skipped_rows_that_a_rerun_runs() {
    let i = open(|_| {}).await;
    let mut def = resolve_blocked(false);
    def["rate"] = json!({ "per_minute": 1 });
    define(&i, "resolve_blocked", def).await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "resolve_blocked" })).await;
    issue(&i, "1", "triaged", "high").await;
    issue(&i, "2", "triaged", "high").await;
    set_state(&i, "1", "blocked").await;
    set_state(&i, "2", "blocked").await;
    drain(&i);
    let rows = runs(&i, "resolve_blocked").await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let skipped: Vec<&Value> = rows.iter().filter(|r| r["state"] == "skipped").collect();
    assert_eq!(skipped.len(), 1, "{rows:?}");
    assert!(skipped[0]["reason"].as_str().unwrap().starts_with("rate_limited"), "{rows:?}");
    assert_eq!(state_of(&i, "issue:2").as_deref(), Some("blocked"));
    // A member may not rerun; an approver who dominates run_as may.
    let id = skipped[0]["id"].as_str().unwrap().to_string();
    let member = Principal::named("user:m", &["member"], &["*"]).unwrap();
    assert_eq!(i.call(&member, "automation.rerun", json!({ "id": id })).await.unwrap_err().code(), "permission_denied");
    let r = ok(&i, &admin(), "automation.rerun", json!({ "id": id })).await;
    assert_eq!(r["state"], "retry");
    drain(&i);
    let again = ok(&i, &admin(), "automation.runs", json!({ "id": id })).await;
    assert_eq!(again["run"]["state"], "succeeded", "{again}");
    assert_eq!(state_of(&i, "issue:2").as_deref(), Some("resolved"));
    // A succeeded run is not rerun.
    let e = i.call(&admin(), "automation.rerun", json!({ "id": id })).await.unwrap_err();
    assert_eq!(e.code(), "invalid_argument");
    assert_eq!(events(&i, "automation.rerun").len(), 1);
}

/// Set `description` of an issue to `value` (low risk, reversible).
const SET_DESCRIPTION: &str = r#"{
    "description": "Set an issue's description.", "risk": "low",
    "parameters": { "issue": { "type": "entity", "kinds": ["issue"], "required": true }, "value": { "type": "string", "required": true } },
    "effects": [{ "patch": { "id": "{issue}", "props": { "description": "{value}" } } }]
}"#;

fn ping(when: &str, then: &str) -> Value {
    json!({
        "run_as": { "roles": ["member"], "workspaces": ["acme"] },
        "max_depth": 2,
        "trigger": { "on": "change", "kinds": ["issue"], "ops": ["entity.update"], "props": ["description"] },
        "conditions": [{ "check": "prop_eq", "param": "subject", "prop": "description", "value": when }],
        "effects": [{ "action": { "type": "set_description", "params": { "issue": "{trigger.id}", "value": then } } }],
    })
}

/// Move an issue to `then` when it moved to `when` (matched on the change
/// as committed, so the cascade does not depend on evaluation order).
fn bounce(when: &str, then: &str) -> Value {
    json!({
        "run_as": { "roles": ["member"], "workspaces": ["acme"] },
        "max_depth": 2,
        "trigger": { "on": "change", "kinds": ["issue"], "ops": ["entity.update"], "state": { "to": [when] } },
        "effects": [{ "action": { "type": "set_state", "params": { "issue": "{trigger.id}", "value": then } } }],
    })
}

#[tokio::test]
async fn the_depth_limit_stops_an_a_b_a_cascade() {
    let i = open(|_| {}).await;
    let at = json!({
        "description": "Move an issue to a state.", "risk": "low",
        "parameters": { "issue": { "type": "entity", "kinds": ["issue"], "required": true }, "value": { "type": "string", "required": true } },
        "effects": [{ "patch": { "id": "{issue}", "state": "{value}" } }]
    });
    ok(
        &i,
        &admin(),
        "schema.apply",
        json!({ "changes": [
            { "op": "put_action_type", "name": "set_state", "def": at },
            { "op": "put_automation", "name": "a", "def": bounce("blocked", "in_progress") },
            { "op": "put_automation", "name": "b", "def": bounce("in_progress", "blocked") },
        ] }),
    )
    .await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "a" })).await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "b" })).await;
    issue(&i, "1", "triaged", "high").await;
    // A person blocks it (depth 0): a runs at depth 1, b at depth 2, then a
    // would run at depth 3, over max_depth 2.
    set_state(&i, "1", "blocked").await;
    drain(&i);
    let a = runs(&i, "a").await;
    let b = runs(&i, "b").await;
    assert_eq!(b.len(), 1, "{b:?}");
    assert_eq!((b[0]["state"].as_str(), b[0]["depth"].as_u64()), (Some("succeeded"), Some(2)));
    assert_eq!(a.len(), 2, "{a:?}");
    assert_eq!((a[1]["state"].as_str(), a[1]["depth"].as_u64()), (Some("succeeded"), Some(1)));
    assert_eq!((a[0]["state"].as_str(), a[0]["depth"].as_u64()), (Some("refused"), Some(3)));
    assert!(a[0]["reason"].as_str().unwrap().starts_with("depth"), "{a:?}");
    assert_eq!(state_of(&i, "issue:1").as_deref(), Some("blocked"), "the cascade stopped after b");
}

#[tokio::test]
async fn a_changed_closure_or_an_expired_enablement_stops_it() {
    let i = open(|_| {}).await;
    define(&i, "resolve_blocked", resolve_blocked(false)).await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "resolve_blocked" })).await;
    issue(&i, "1", "triaged", "high").await;
    // Raising the action type's risk changes the closure.
    let mut resolve = serde_json::to_value(&i.ontology().schema().action_types["resolve"]).unwrap();
    resolve["description"] = json!("Resolve an issue (reworded).");
    ok(
        &i,
        &admin(),
        "schema.apply",
        json!({ "changes": [{ "op": "put_action_type", "name": "resolve", "def": resolve }] }),
    )
    .await;
    set_state(&i, "1", "blocked").await;
    drain(&i);
    assert_eq!(state_of(&i, "issue:1").as_deref(), Some("blocked"));
    let got = ok(&i, &admin(), "automation.get", json!({ "name": "resolve_blocked" })).await;
    assert_eq!(got["reason"], "paused: stale_definition: action_type:resolve", "{got}");
    assert_eq!(got["closure"]["changed_since_enabled"], json!(["action_type:resolve"]));
    assert_eq!(events(&i, "automation.stale").len(), 1);
    drain(&i);
    assert_eq!(events(&i, "automation.stale").len(), 1, "one event, not one per round");
    // A person enables it again: it runs on changes from then on.
    ok(&i, &admin(), "automation.enable", json!({ "name": "resolve_blocked" })).await;
    set_state(&i, "1", "in_progress").await;
    set_state(&i, "1", "blocked").await;
    drain(&i);
    assert_eq!(state_of(&i, "issue:1").as_deref(), Some("resolved"));

    // The enablement expires (here: written as expired).
    let st: AutomationState =
        serde_json::from_value(i.ontology().store().kv_get(NS_AUTOMATION_STATE, "resolve_blocked").unwrap().unwrap())
            .unwrap();
    assert!(st.expires_at - st.enabled_at == 30 * 86_400_000, "30 days by default");
    let expired = AutomationState { expires_at: 1, ..st };
    i.ontology()
        .commit_automation(
            &ActionContext::new(admin(), "acme"),
            vec![(NS_AUTOMATION_STATE.into(), "resolve_blocked".into(), serde_json::to_value(&expired).unwrap())],
            ConfigEvent::new("test.expire", None, json!({})),
        )
        .unwrap();
    issue(&i, "2", "triaged", "high").await;
    set_state(&i, "2", "blocked").await;
    drain(&i);
    assert_eq!(state_of(&i, "issue:2").as_deref(), Some("blocked"));
    let got = ok(&i, &admin(), "automation.get", json!({ "name": "resolve_blocked" })).await;
    assert_eq!(got["reason"], "paused: enablement_expired");
    assert_eq!(events(&i, "automation.expired").len(), 1);
}

#[tokio::test]
async fn medium_risk_and_not_reversible_plans_are_refused() {
    let i = open(|_| {}).await;
    let mut risky: Value = serde_json::from_str(SET_DESCRIPTION).unwrap();
    risky["risk"] = json!("medium");
    let start = json!({
        "description": "Start an issue.", "risk": "low",
        "parameters": { "issue": { "type": "entity", "kinds": ["issue"], "required": true } },
        "effects": [{ "patch": { "id": "{issue}", "state": "in_progress" } }]
    });
    let def = |action: &str| {
        let mut d = ping("go", "x");
        d["conditions"] = json!([]);
        d["effects"] = json!([{ "action": { "type": action, "params": { "issue": "{trigger.id}", "value": "x" } } }]);
        d
    };
    ok(
        &i,
        &admin(),
        "schema.apply",
        json!({ "changes": [
            { "op": "put_action_type", "name": "risky", "def": risky },
            { "op": "put_action_type", "name": "start", "def": start },
            { "op": "put_automation", "name": "medium", "def": def("risky") },
            { "op": "put_automation", "name": "irreversible", "def": def("start") },
        ] }),
    )
    .await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "medium" })).await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "irreversible" })).await;
    // No state yet: setting one cannot be undone, so the plan is not reversible.
    issue(&i, "1", "", "high").await;
    ok(&i, &admin(), "entity.upsert", json!({ "id": "issue:1", "props": { "description": "go" } })).await;
    drain(&i);
    for name in ["medium", "irreversible"] {
        let r = runs(&i, name).await;
        assert_eq!(r.len(), 1, "{name}: {r:?}");
        assert_eq!(r[0]["state"], "refused", "{name}: {r:?}");
        assert!(r[0]["reason"].as_str().unwrap().starts_with("needs_confirmation"), "{name}: {r:?}");
    }
    assert!(runs(&i, "irreversible").await[0]["reason"].as_str().unwrap().contains("not reversible"));
    assert_eq!(state_of(&i, "issue:1"), None);
}

#[tokio::test]
async fn dry_runs_render_as_the_run_and_the_caller_and_write_nothing() {
    let i = open(|_| {}).await;
    define(&i, "resolve_blocked", resolve_blocked(false)).await;
    issue(&i, "1", "blocked", "high").await;
    // An unguarded trigger: `id` tests the entity's current fields.
    let mut plain = resolve_blocked(false);
    plain["trigger"] = json!({ "on": "change", "kinds": ["issue"], "ops": ["entity.update"] });
    define(&i, "resolve_any", plain).await;
    let seq = i.ontology().store().head().unwrap();
    let t = ok(&i, &admin(), "automation.test", json!({ "name": "resolve_any", "id": "issue:1" })).await;
    assert_eq!(t["verdict"], "would_run", "{t}");
    assert_eq!(t["matched"], true, "{t}");
    assert_eq!(t["effects"][0]["plan"]["preview"][0], "update issue:1: state blocked → resolved");
    assert_eq!(i.ontology().store().head().unwrap(), seq, "nothing was written");
    // A caller who cannot see the subject renders nothing of it.
    let outsider = Principal::named("user:o", &["viewer"], &["elsewhere"]).unwrap();
    let e = i.call(&outsider, "automation.test", json!({ "name": "resolve_any", "id": "issue:1" })).await;
    assert!(e.is_err(), "the caller does not see issue:1");
    // By cursor: the change itself.
    set_state(&i, "1", "triaged").await;
    set_state(&i, "1", "blocked").await;
    let head = ok(&i, &admin(), "changes.head", json!({})).await;
    let t = ok(&i, &admin(), "automation.test", json!({ "name": "resolve_blocked", "cursor": head["cursor"] })).await;
    assert_eq!(t["matched"], true, "{t}");
    assert_eq!(t["trigger"]["state"]["from"], "triaged");
}

/// A synthetic update (`{"id"}`) moves no state and changes no field, so a
/// trigger with a state guard or `props` is tested by cursor only: `id` is
/// refused up front, naming the guard and the way to find a cursor, instead
/// of a `matched: false` result whose templates cannot render.
#[tokio::test]
async fn a_guarded_trigger_is_tested_by_cursor_not_by_id() {
    let i = open(|_| {}).await;
    // The webhook effect reads {trigger.state.from}.
    endpoint(&i, "https://hooks.example.com/ops", json!({ "roles": ["member"], "workspaces": ["acme"] })).await;
    define(&i, "resolve_blocked", resolve_blocked(true)).await;
    let mut by_props = resolve_blocked(false);
    by_props["trigger"] = json!({ "on": "change", "kinds": ["issue"], "props": ["priority"] });
    define(&i, "on_priority", by_props).await;
    issue(&i, "1", "blocked", "high").await;
    let seq = i.ontology().store().head().unwrap();
    for (name, guard) in [("resolve_blocked", "a state guard"), ("on_priority", "props")] {
        let e = i.call(&admin(), "automation.test", json!({ "name": name, "id": "issue:1" })).await.unwrap_err();
        assert_eq!(e.code(), "invalid_argument", "{name}: {e}");
        let msg = e.to_string();
        assert!(msg.contains(&format!("trigger has {guard},")), "{name}: {msg}");
        assert!(msg.contains("pass cursor"), "{name}: {msg}");
        assert!(msg.contains("--ids issue:1"), "{name}: {msg}");
        assert!(!msg.contains("invalid_template"), "{name}: {msg}");
    }
    assert_eq!(i.ontology().store().head().unwrap(), seq, "nothing was written");
    // The real change by cursor: matched, and the state move renders.
    set_state(&i, "1", "triaged").await;
    set_state(&i, "1", "blocked").await;
    let head = ok(&i, &admin(), "changes.head", json!({})).await;
    let input = json!({ "name": "resolve_blocked", "cursor": head["cursor"], "id": "issue:1" });
    let t = ok(&i, &admin(), "automation.test", input).await;
    assert_eq!((t["matched"].as_bool(), t["verdict"].as_str()), (Some(true), Some("would_run")), "{t}");
    assert_eq!(t["effects"][1]["plan"]["payload"]["from"], "triaged", "{t}");
}

#[tokio::test]
async fn empty_inputs_fail_cleanly() {
    let i = open(|_| {}).await;
    for action in onto_intelli::automate::CALLS {
        match i.call(&admin(), action, json!({})).await {
            Ok(v) => assert!(v.is_object(), "{action}: {v}"),
            Err(e) => assert_eq!(e.code(), "invalid_argument", "{action}: {e}"),
        }
    }
    assert_eq!(i.call(&admin(), "automation.get", json!({ "name": "nope" })).await.unwrap_err().code(), "not_found");
    assert_eq!(
        i.call(&admin(), "automation.runs", json!({ "state": "done" })).await.unwrap_err().code(),
        "invalid_argument"
    );
    let who = ok(&i, &admin(), "system.whoami", json!({})).await;
    assert_eq!(who["capabilities"]["automate"], true);
    let agent = Principal::new("assistant", vec![Role::Agent], vec!["*".into()]);
    assert_eq!(ok(&i, &agent, "system.whoami", json!({})).await["capabilities"]["automate"], false);
}

// ── review round 1 (phase 1 steps 8-12) ──────────────────────────────────

/// C-1: a run claimed by this process whose execution ended in an error
/// (left `running` at the current epoch) is executed by the next round of
/// the same holder, which keeps its epoch.
#[tokio::test]
async fn a_run_left_running_by_an_error_is_executed_by_the_next_round() {
    use onto_intelli::core::automate::Enablements;
    use onto_intelli::core::changes::{ChangeOp, Cursor};
    use onto_intelli::core::leases::Fence;
    use onto_intelli::core::runs::{CONSUMER_LEASE, RunRow, RunTrigger};

    let i = open(|_| {}).await;
    define(&i, "resolve_blocked", resolve_blocked(false)).await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "resolve_blocked" })).await;
    issue(&i, "1", "triaged", "high").await;
    drain(&i); // takes the lease
    let store = i.ontology().store().clone();
    let lease = store.lease_get(CONSUMER_LEASE).unwrap().unwrap();
    // The state a round leaves when execute errors after the claim: the
    // trigger's run claimed at the current epoch, nothing committed.
    set_state(&i, "1", "blocked").await;
    let page = store.changes_since(Cursor::default(), 1000).unwrap();
    let (stored, entity) = page
        .changes
        .iter()
        .rev()
        .find(|(c, _)| c.record.op == ChangeOp::EntityUpdate && c.record.id == "issue:1")
        .cloned()
        .unwrap();
    let r = stored.joined(entity);
    let en = Enablements::load(&*store).unwrap();
    let schema = i.ontology().schema();
    let def = &schema.automations["resolve_blocked"];
    let mut run =
        RunRow::new("resolve_blocked", &en.states["resolve_blocked"].closure_hash, 0, def, RunTrigger::of(&r));
    run.epoch = lease.epoch;
    let fence = Fence::new(CONSUMER_LEASE, &lease.holder, lease.epoch);
    assert_eq!(store.run_claim(std::slice::from_ref(&run), Some(&fence)).unwrap().len(), 1);
    let reports = drain(&i);
    assert_eq!(reports.iter().map(|r| r.recovered).sum::<usize>(), 1, "{reports:?}");
    assert_eq!(store.lease_get(CONSUMER_LEASE).unwrap().unwrap().epoch, lease.epoch, "the same epoch");
    let got = ok(&i, &admin(), "automation.runs", json!({ "id": run.id })).await;
    assert_eq!(got["run"]["state"], "succeeded", "{got}");
    assert_eq!(state_of(&i, "issue:1").as_deref(), Some("resolved"));
    assert_eq!(runs(&i, "resolve_blocked").await.len(), 1, "the trigger ran once");
}

/// C-2: a rerun writes its timer before it moves the row to `retry`, and a
/// round keeps a fresh rerun timer whose row is not in `retry` yet (another
/// process's rerun in between), but drops a stale one.
#[tokio::test]
async fn a_rerun_timer_waits_for_its_row() {
    let i = open(|_| {}).await;
    let mut def = resolve_blocked(false);
    def["rate"] = json!({ "per_minute": 1 });
    define(&i, "resolve_blocked", def).await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "resolve_blocked" })).await;
    issue(&i, "1", "triaged", "high").await;
    issue(&i, "2", "triaged", "high").await;
    set_state(&i, "1", "blocked").await;
    set_state(&i, "2", "blocked").await;
    drain(&i);
    let skipped: Vec<Value> =
        runs(&i, "resolve_blocked").await.into_iter().filter(|r| r["state"] == "skipped").collect();
    let id = skipped[0]["id"].as_str().unwrap().to_string();
    let store = i.ontology().store().clone();
    let timer = format!("retry:{id}");
    let due = |name: &str| store.timers_due(100).unwrap().iter().any(|t| t.name == name);
    // Another process's rerun has written its timer, not yet the row.
    let now = onto_intelli::core::time::now_ms();
    store.timers_set(&timer, 0, &json!({ "run": id, "automation": "resolve_blocked", "at": now })).unwrap();
    drain(&i);
    assert!(due(&timer), "a fresh rerun timer is kept");
    assert_eq!(ok(&i, &admin(), "automation.runs", json!({ "id": id })).await["run"]["state"], "skipped");
    // A stale one (its rerun never moved the row) goes.
    store.timers_set(&timer, 0, &json!({ "run": id, "automation": "resolve_blocked", "at": now - 60_000 })).unwrap();
    drain(&i);
    assert!(!due(&timer), "a stale timer is dropped");
    // A rerun leaves the row in retry with its timer, and the next round runs it.
    let r = ok(&i, &admin(), "automation.rerun", json!({ "id": id })).await;
    assert_eq!(r["state"], "retry");
    assert!(due(&timer), "the retry row has its timer");
    drain(&i);
    assert_eq!(ok(&i, &admin(), "automation.runs", json!({ "id": id })).await["run"]["state"], "succeeded");
}

/// CP-2: commits wake the fan-out and the automation consumer only while
/// the tenant has a subscription or an enabled automation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commits_wake_only_workers_that_have_work() {
    let i = open(|cfg| cfg.automation.workers = true).await;
    let rounds = |w: &str| i.worker_rounds()[w].as_u64().unwrap();
    until("both workers ran a first round", || {
        let w = i.worker_rounds();
        w["automation"].as_u64() > Some(0)
            && w["fanout"].as_u64() > Some(0)
            && w["commits_wake_automation"] == false
            && w["commits_wake_fanout"] == false
    })
    .await;
    let (a0, f0) = (rounds("automation"), rounds("fanout"));
    for n in 0..50 {
        issue(&i, &n.to_string(), "triaged", "high").await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (a1, f1) = (rounds("automation"), rounds("fanout"));
    assert!(a1 - a0 <= 2 && f1 - f0 <= 2, "50 commits ran {} automation and {} fan-out rounds", a1 - a0, f1 - f0);
    // An enabled automation: commits wake the consumer again.
    define(&i, "resolve_blocked", resolve_blocked(false)).await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "resolve_blocked" })).await;
    until("the consumer wants commits", || i.worker_rounds()["commits_wake_automation"] == true).await;
    set_state(&i, "1", "blocked").await;
    until("the run", || state_of(&i, "issue:1").as_deref() == Some("resolved")).await;
}

/// CP-1: with automations off in this process, a due retry timer neither
/// spins the consumer nor keeps the delivery worker awake.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_due_timer_does_not_spin_a_process_with_automations_off() {
    let i = open(|cfg| {
        cfg.automation.workers = true;
        cfg.automation.enabled = false;
        cfg.webhooks.enabled = true;
    })
    .await;
    let store = i.ontology().store().clone();
    store.timers_set("retry:run_left_over", 0, &json!({ "run": "run_left_over" })).unwrap();
    i.wake_outbox();
    let a0 = i.worker_rounds()["automation"].as_u64().unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let a1 = i.worker_rounds()["automation"].as_u64().unwrap();
    assert!(a1 - a0 <= 2, "{} automation rounds in 1.5 s", a1 - a0);
    assert!(store.timers_due(10).unwrap().iter().any(|t| t.name == "retry:run_left_over"), "left for a consumer");
}

/// COMP-2: `system.health` reports the switches and paused counts, and
/// `system.stats` the open outbox rows and the runs of the last hour.
#[tokio::test]
async fn health_and_stats_show_switches_paused_counts_and_runs() {
    let i = open(|cfg| cfg.automation.pause_after_failures = 1).await;
    endpoint(&i, "https://hooks.example.com/in", json!({ "roles": ["member"], "workspaces": ["acme"] })).await;
    define(&i, "echo", failing_def()).await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "echo" })).await;
    issue(&i, "1", "triaged", "high").await;
    ok(&i, &admin(), "entity.upsert", json!({ "id": "issue:1", "props": { "description": "v1" } })).await;
    drain(&i);
    ok(&i, &admin(), "automation.kill", json!({ "on": true })).await;
    ok(&i, &admin(), "webhook.kill", json!({ "on": true })).await;
    let h = ok(&i, &admin(), "system.health", json!({})).await;
    let o = &h["outbound"];
    assert_eq!(o["changes"]["capture"], true, "{h}");
    assert_eq!((o["automation"]["kill"].as_bool(), o["automation"]["paused"].as_u64()), (Some(true), Some(1)), "{h}");
    assert_eq!((o["webhooks"]["kill"].as_bool(), o["webhooks"]["endpoints"].as_u64()), (Some(true), Some(1)), "{h}");
    let s = ok(&i, &admin(), "system.stats", json!({})).await;
    assert_eq!(s["outbound"]["runs_1h"]["states"]["failed"], 1, "{s}");
    assert!(s["workers"]["automation"].is_u64(), "{s}");
}

/// Section 11 counters: `system.stats` reports change lag per consumer and
/// per endpoint, gap records over 24 h with the latest, and outbox rows per
/// target split by state; `system.health` the latest gap; the hub's tenant
/// report (`GET /v1/admin/tenants`) the same counters with paused counts.
#[tokio::test]
async fn stats_health_and_tenant_report_show_lag_gaps_and_outbox_states() {
    use onto_intelli::core::outbox::{Attempt, OutboxKind, OutboxRow, OutboxState, SOURCE_TEST};
    let i = open(|_| {}).await;
    ok(
        &i,
        &admin(),
        "webhook.add",
        json!({ "name": "ops", "url": "https://hooks.example.com/in", "secret": SECRET,
                "filter": { "kinds": "issue" }, "principal": { "roles": ["member"], "workspaces": ["acme"] } }),
    )
    .await;
    let s = ok(&i, &admin(), "system.stats", json!({})).await;
    let lag = &s["outbound"]["lag"];
    // Behind by the endpoint's own config commit (no records): nothing to read.
    let b0 = lag["endpoints"]["ops"]["behind"].as_u64().expect("the endpoint's lag");
    assert_eq!(lag["endpoints"]["ops"]["age_ms"], 0, "{s}");
    assert_eq!(lag["consumers"]["webhook:ops"], lag["endpoints"]["ops"], "{s}");
    assert_eq!(s["outbound"]["gaps_24h"]["count"], 0, "{s}");
    assert!(s["outbound"]["gaps_24h"]["latest"].is_null(), "{s}");

    // Fan-out is off here: the endpoint's cursor stays while commits land.
    issue(&i, "1", "triaged", "high").await;
    issue(&i, "2", "triaged", "low").await;
    let store = i.ontology().store().clone();
    let base = i.ontology().get(&admin(), "issue:1").unwrap();
    let mut bulk = base.clone();
    bulk.id = "issue:bulk".into();
    store.bulk_import(&[bulk], &[]).unwrap();
    // Outbox rows of `ops`: one dead, one left pending.
    let row = |d: &str| OutboxRow::new(OutboxKind::Webhook, "ops", d, SOURCE_TEST, "acme", "onto.webhook.test");
    store.outbox_enqueue(&[row("a"), row("b")], None, None).unwrap();
    let c = store.outbox_claim(1, 60_000, "w", &[]).unwrap();
    let gone = Attempt { status: Some(410), duration_ms: 1, ..Default::default() };
    assert!(store.outbox_finish(&c[0].id, c[0].claim.as_deref().unwrap(), &gone, OutboxState::Dead, 0).unwrap());

    let s = ok(&i, &admin(), "system.stats", json!({})).await;
    let o = &s["outbound"];
    let ep = &o["lag"]["endpoints"]["ops"];
    assert_eq!(ep["behind"].as_u64(), Some(b0 + 3), "two upserts and the import: {s}");
    assert!(ep["age_ms"].as_i64().is_some_and(|a| a >= 0), "{s}");
    assert_eq!(ep["expired"], false, "{s}");
    assert_eq!(o["gaps_24h"]["count"], 1, "{s}");
    assert_eq!(o["gaps_24h"]["latest"]["reason"], "bulk_import", "{s}");
    assert_eq!(o["gaps_24h"]["latest"]["touched"], 1, "{s}");
    assert_eq!(o["outbox"]["targets"]["ops"], json!({ "dead": 1, "pending": 1 }), "{s}");
    assert_eq!(o["outbox_open"]["ops"], 1, "{s}");
    assert_eq!(o["outbox"]["cap"], onto_intelli::core::outbox::STATS_COUNT_CAP, "{s}");

    let h = ok(&i, &admin(), "system.health", json!({})).await;
    let g = &h["outbound"]["changes"]["latest_gap"];
    assert_eq!((g["reason"].as_str(), &g["cursor"]), (Some("bulk_import"), &o["gaps_24h"]["latest"]["cursor"]), "{h}");
    assert!(!h.to_string().contains("issue:bulk"), "the latest gap names no ids: {h}");

    let hub = onto_intelli::Hub::single(i.clone());
    assert!(hub.tenant_stats()[0].outbound.is_none(), "only the report reads the store");
    let report = tokio::task::spawn_blocking(move || hub.tenant_report()).await.unwrap();
    let t = report[0].outbound.as_ref().expect("the tenant's counters");
    let tep = &t["lag"]["endpoints"]["ops"];
    assert_eq!((&tep["behind"], &tep["cursor"]), (&ep["behind"], &ep["cursor"]), "{t}");
    assert_eq!(t["gaps_24h"], o["gaps_24h"], "{t}");
    assert_eq!(t["outbox"], o["outbox"], "{t}");
    assert_eq!(t["paused"], json!({ "automations": 0, "endpoints": 0 }), "{t}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_is_traced_and_continues_the_trace_of_its_trigger() {
    const OTEL: &str = "0af7651916cd43dd8448eb211c80319c";
    let rx = Receiver::start().await;
    let i = open(|cfg| {
        cfg.webhooks.enabled = true;
        cfg.webhooks.poll_secs = 1;
        cfg.webhooks.allow = vec![AllowConfig { host: "127.0.0.1".into(), cidrs: vec!["127.0.0.1/32".into()] }];
    })
    .await;
    endpoint(
        &i,
        &format!("http://127.0.0.1:{}/hooks", rx.port),
        json!({ "roles": ["member"], "workspaces": ["acme"] }),
    )
    .await;
    define(&i, "resolve_blocked", resolve_blocked(true)).await;
    issue(&i, "1", "triaged", "high").await;
    ok(&i, &admin(), "automation.enable", json!({ "name": "resolve_blocked" })).await;
    // The trigger is written under the caller's traceparent.
    let ctx = onto_intelli::trace::TraceCtx::from_source("http")
        .with_traceparent(Some(&format!("00-{OTEL}-b7ad6b7169203331-01")));
    let w = i.call_traced(&admin(), "entity.upsert", json!({ "id": "issue:1", "state": "blocked" }), ctx).await;
    w.result.unwrap();
    drain(&i);
    let all = runs(&i, "resolve_blocked").await;
    assert_eq!(all.len(), 1, "{all:?}");
    let run = &all[0];
    assert_eq!(run["state"], "succeeded", "{run}");
    let trace_id = run["trace_id"].as_str().expect("the run's trace").to_string();
    assert_eq!(trace_id.len(), 32);
    assert_eq!(run["trigger"]["trace_id"], OTEL);

    // Its outbox row and its own change carry the trigger's trace.
    let row = run["effects"][1]["outbox"].as_str().unwrap();
    assert_eq!(i.ontology().store().outbox_get(row).unwrap().unwrap().otel_trace_id.as_deref(), Some(OTEL));
    let page = i.ontology().store().changes_since(Default::default(), 100).unwrap();
    let own: Vec<_> =
        page.changes.iter().filter(|(s, _)| s.record.cause.run.as_deref() == run["id"].as_str()).collect();
    assert!(!own.is_empty());
    assert!(own.iter().all(|(s, _)| s.record.cause.trace_id.as_deref() == Some(OTEL)));
    // The delivery sends it as traceparent.
    until("the delivery", || !rx.hits.lock().is_empty()).await;
    let tp = rx.hits.lock()[0].0.get("traceparent").unwrap().to_str().unwrap().to_string();
    assert!(tp.starts_with(&format!("00-{OTEL}-")), "{tp}");

    // One `automation` trace: the run as the root span, a span per effect,
    // no payload, plan or secret.
    i.flush_traces().await;
    let list = ok(&i, &admin(), "trace.list", json!({ "kind": "automation" })).await;
    assert_eq!(list["traces"].as_array().unwrap().len(), 1, "{list}");
    let d = ok(&i, &admin(), "trace.get", json!({ "id": trace_id })).await;
    let t = &d["trace"];
    assert_eq!(
        (t["kind"].as_str(), t["name"].as_str(), t["status"].as_str()),
        (Some("automation"), Some("automation.run:resolve_blocked"), Some("ok"))
    );
    assert_eq!(
        (t["otel_trace_id"].as_str(), t["principal"].as_str()),
        (Some(OTEL), Some("automation:resolve_blocked"))
    );
    assert_eq!(t["request_id"], run["id"]);
    let spans = d["spans"].as_array().unwrap();
    let root = spans.iter().find(|s| s["parent_id"].is_null()).unwrap();
    assert_eq!(
        (root["attrs"]["run"].as_str(), root["attrs"]["state"].as_str()),
        (run["id"].as_str(), Some("succeeded"))
    );
    let mut effects: Vec<&str> =
        spans.iter().filter(|s| s["parent_id"] == root["id"]).map(|s| s["name"].as_str().unwrap()).collect();
    effects.sort();
    assert_eq!(effects, vec!["effect.action:resolve", "effect.webhook:ops"]);
    let text = d.to_string();
    assert!(!text.contains("Issue 1") && !text.contains("auto: ") && !text.contains(SECRET), "{text}");
}
