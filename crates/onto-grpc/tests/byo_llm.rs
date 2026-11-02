//! Bring your own LLM against a real server on 127.0.0.1:0 and a mock
//! OpenAI-compatible provider (axum) that checks the project's key: the
//! server's provider without settings, the project's after a refresh, the
//! test connection, budgets (no provider call), the rate limit over HTTP
//! (429) and gRPC (`RESOURCE_EXHAUSTED`), usage, the SSRF guard, the master
//! key rules and redaction. On `MemoryControl`, and with `DATABASE_URL` also
//! on `PgControl` in its own schema (dropped at the end).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use onto_control::{Control, ControlOptions, ControlStore, LlmUpdate, PgControl, ProjectLlm, Sealed, Vault};
use onto_intelli::Hub;
use onto_intelli::config::Config;

const BOOTSTRAP: &str = "bootstrap-token-for-byo-llm-tests-0123456789";
const PROJECT_KEY: &str = "sk-project-key-0123456789";

// ── the mock provider ─────────────────────────────────────────────────────

#[derive(Default)]
struct Mock {
    calls: AtomicUsize,
    last_auth: Mutex<Option<String>>,
    last_body: Mutex<Value>,
}

async fn completions(State(m): State<Arc<Mock>>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    m.calls.fetch_add(1, Ordering::SeqCst);
    let auth = h.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
    *m.last_auth.lock() = auth.clone();
    *m.last_body.lock() = body;
    if auth.as_deref() != Some(&format!("Bearer {PROJECT_KEY}")) {
        let long = format!("{{\"error\":{{\"message\":\"invalid api key {}\"}}}}", "x".repeat(1000));
        return (StatusCode::UNAUTHORIZED, long).into_response();
    }
    Json(json!({
        "model": "mock-gpt-1",
        "choices": [{ "message": { "role": "assistant", "content": "OK from the project model" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 40, "completion_tokens": 5 },
    }))
    .into_response()
}

async fn mock_provider() -> (Arc<Mock>, SocketAddr) {
    let m = Arc::new(Mock::default());
    let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(completions)).with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, addr)
}

// ── the server ────────────────────────────────────────────────────────────

fn database_url() -> Option<String> {
    ["ONTO_TEST_DATABASE_URL", "DATABASE_URL"].iter().find_map(|v| std::env::var(v).ok().filter(|u| !u.is_empty()))
}

struct DropSchema(String, String);

impl Drop for DropSchema {
    fn drop(&mut self) {
        let (url, schema) = (self.0.clone(), self.1.clone());
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                use onto_intelli::core::sqlx;
                let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
                sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")))
                    .execute(&pool)
                    .await
                    .unwrap();
            });
        })
        .join();
    }
}

struct Srv {
    base: String,
    http: reqwest::Client,
    control: Arc<Control>,
    hub: Arc<Hub>,
    owner: String,
    _stop: oneshot::Sender<()>,
    _schema: Option<DropSchema>,
}

async fn start(allow_private: bool, vault: bool, pg: Option<&str>) -> Srv {
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.console.enabled = true;
    cfg.console.allow_private_urls = allow_private;
    let opts = ControlOptions {
        bootstrap_token: Some(BOOTSTRAP.into()),
        pepper: Some(onto_control::Pepper::random()),
        vault: vault.then(|| Arc::new(Vault::new(onto_control::keys::random_bytes(32), None).unwrap())),
        ..Default::default()
    };
    let (control, schema) = match pg {
        Some(url) => {
            let schema = format!("onto_control_test_{}", onto_control::new_ref());
            let guard = DropSchema(url.to_string(), schema.clone());
            let store = PgControl::connect(url, &schema).await.unwrap();
            (Control::new(Arc::new(store), cfg.console.clone(), opts).unwrap(), Some(guard))
        }
        None => (Control::memory(cfg.console.clone(), opts).unwrap(), None),
    };
    let control = Arc::new(control);
    let hub = Hub::open(cfg).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let opts = onto_grpc::ServeOptions { dev: false, control: Some(control.clone()) };
    tokio::spawn(onto_grpc::serve_hub_on(hub.clone(), opts, listener, async {
        let _ = rx.await;
    }));
    let http = reqwest::Client::new();
    let base = format!("http://{addr}");
    for _ in 0..50 {
        if http.get(format!("{base}/v1/console/auth/providers")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let resp = http.post(format!("{base}/v1/console/auth/token")).bearer_auth(BOOTSTRAP).send().await.unwrap();
    let v: Value = resp.json().await.unwrap();
    let owner = v["token"].as_str().unwrap().to_string();
    Srv { base, http, control, hub, owner, _stop: tx, _schema: schema }
}

impl Srv {
    async fn call(&self, method: Method, path: &str, token: &str, body: Option<Value>) -> (u16, Value, HeaderMap) {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let text = resp.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)), headers)
    }

    async fn console(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let (st, v, _) = self.call(method, &format!("/v1/console{path}"), &self.owner, body).await;
        (st, v)
    }

    /// A project with demo data and a read key.
    async fn project(&self) -> (String, String) {
        let (st, p) = self
            .console(Method::POST, "/orgs/default/projects", Some(json!({ "name": "Payments", "seed": "demo" })))
            .await;
        assert_eq!(st, 200, "{p}");
        let r = p["ref"].as_str().unwrap().to_string();
        let (st, k) = self
            .console(Method::POST, &format!("/projects/{r}/keys"), Some(json!({ "name": "svc", "scopes": ["read"] })))
            .await;
        assert_eq!(st, 200, "{k}");
        (r, k["secret"].as_str().unwrap().to_string())
    }

    /// An LLM question (never cached) with the API key.
    async fn ask(&self, key: &str) -> (u16, Value, HeaderMap) {
        let q = json!({ "question": "Why is Project Alpha blocked?", "mode": "llm", "no_cache": true });
        self.call(Method::POST, "/v1/ask", key, Some(q)).await
    }
}

fn settings(base_url: &str, extra: Value) -> Value {
    let mut v = json!({ "provider": "openai-compatible", "base_url": base_url, "model": "gpt-test" });
    if let (Value::Object(m), Value::Object(e)) = (&mut v, extra) {
        m.extend(e);
    }
    v
}

// ── the tests ─────────────────────────────────────────────────────────────

async fn byo_llm_flow(pg: Option<&str>) {
    let (mock, addr) = mock_provider().await;
    let srv = start(true, true, pg).await;
    let (r, key) = srv.project().await;
    let base_url = format!("http://{addr}/v1");
    let project = srv.control.store().project_by_ref(&r).await.unwrap().unwrap();

    // Without settings the server's provider answers.
    let (st, a, _) = srv.ask(&key).await;
    assert_eq!(st, 200, "{a}");
    assert_eq!((a["provider"].as_str(), a["mode"].as_str()), (Some("mock"), Some("llm")));
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);

    // Settings stored elsewhere (another replica) apply once the tenant is refreshed.
    let update = LlmUpdate::from_json(settings(&base_url, json!({ "api_key": PROJECT_KEY }))).unwrap();
    srv.control.put_llm(&project, update, "usr_elsewhere", false).await.unwrap();
    assert_eq!(srv.ask(&key).await.1["provider"], "mock", "not refreshed yet");
    assert_eq!(srv.control.poll_settings(&srv.hub).await.unwrap(), vec![r.clone()]);
    assert!(srv.control.poll_settings(&srv.hub).await.unwrap().is_empty(), "nothing moved since");
    let (st, a, _) = srv.ask(&key).await;
    assert_eq!(st, 200, "{a}");
    assert_eq!((a["provider"].as_str(), a["model"].as_str()), (Some("openai-compatible"), Some("gpt-test")));
    assert_eq!(a["answer"], "OK from the project model");
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    assert_eq!(mock.last_auth.lock().clone(), Some(format!("Bearer {PROJECT_KEY}")));
    assert!(srv.hub.refresh_tenant(&r).await.unwrap());
    assert!(!srv.hub.refresh_tenant("not-open").await.unwrap());

    // The settings show whether there is a key, never the key.
    let (st, v) = srv.console(Method::GET, &format!("/projects/{r}/llm"), None).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!((v["llm"]["has_api_key"].as_bool(), v["llm"]["api_key_last4"].as_str()), (Some(true), Some("6789")));
    assert!(v["llm"].get("api_key").is_none() && v["llm"].get("api_key_ct").is_none());
    assert!(v["error"].is_null() && v["master_key"] == true);
    assert_eq!(v["server_default"]["provider"], "mock");
    assert!(!v.to_string().contains(PROJECT_KEY));
    let stored = srv.control.store().llm(&project.id).await.unwrap().unwrap();
    let debug = format!("{stored:?} {:?}", srv.control);
    assert!(!debug.contains(PROJECT_KEY) && debug.contains("Sealed("), "{debug}");

    // Test connection: the stored settings, max_tokens 8, not metered.
    let meter = srv.control.meter(&project.id).await.unwrap();
    let before = meter.month_to_date();
    let (st, t) = srv.console(Method::POST, &format!("/projects/{r}/llm/test"), Some(json!({}))).await;
    assert_eq!(st, 200, "{t}");
    assert_eq!((t["ok"].as_bool(), t["response_model"].as_str()), (Some(true), Some("mock-gpt-1")));
    assert_eq!(t["provider"], "openai-compatible");
    assert_eq!(mock.last_body.lock()["max_tokens"], 8);
    assert_eq!(mock.last_body.lock()["messages"][0]["content"], "Reply with the single word OK.");
    assert_eq!(meter.month_to_date(), before);
    // A failing draft: the provider's status and at most 200 characters.
    let draft = settings(&base_url, json!({ "api_key": "sk-wrong-key-000000000" }));
    let (st, t) = srv.console(Method::POST, &format!("/projects/{r}/llm/test"), Some(draft)).await;
    assert_eq!(st, 200, "{t}");
    let err = t["error"].as_str().unwrap();
    assert!(t["ok"] == false && err.contains("401") && err.chars().count() <= 201, "{err}");
    // A draft for another base URL never gets the stored key.
    let elsewhere = settings(&format!("http://localhost:{}/v1", addr.port()), json!({}));
    let (_, t) = srv.console(Method::POST, &format!("/projects/{r}/llm/test"), Some(elsewhere.clone())).await;
    assert_eq!(t["ok"], false);
    assert_eq!(mock.last_auth.lock().clone(), None, "the stored key stays with its base URL");
    let (st, v) = srv.console(Method::PUT, &format!("/projects/{r}/llm"), Some(elsewhere)).await;
    assert_eq!(st, 400, "a new base URL needs the key again: {v}");

    // Refused settings.
    for (body, why) in [
        (json!({ "provider": "openai", "api_key_env": "ONTO_MASTER_KEY" }), "api_key_env"),
        (settings(&base_url, json!({ "headers": { "Authorization": "Bearer x" } })), "credentials"),
        (settings(&base_url, json!({ "headers": { "Cookie": "a=b" } })), "credentials"),
        (json!({ "provider": "mock" }), "development mode"),
        (json!({ "provider": "openai" }), "needs the api_key again"),
        (json!({ "provider": "ollama" }), "needs a base_url"),
        (json!({ "provider": "skynet" }), "unknown LLM provider"),
        (settings(&base_url, json!({ "temperature": 9 })), "temperature"),
    ] {
        let (st, v) = srv.console(Method::PUT, &format!("/projects/{r}/llm"), Some(body)).await;
        assert_eq!(st, 400, "{why}: {v}");
        assert!(v["error"]["message"].as_str().unwrap().contains(why), "{why}: {v}");
    }

    // Budget: at or over it, no provider call and the Fallback mode with a note.
    let budget = settings(
        &base_url,
        json!({ "pricing": { "input_per_mtok": 1_000_000.0, "output_per_mtok": 0.0 }, "monthly_budget_usd": 1.0 }),
    );
    let (st, v) = srv.console(Method::PUT, &format!("/projects/{r}/llm"), Some(budget)).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["llm"]["has_api_key"], true, "same provider and base URL: the key is kept");
    let (_, a, _) = srv.ask(&key).await;
    assert_eq!(a["mode"], "llm");
    assert!((a["usage"]["cost_usd"].as_f64().unwrap() - 40.0).abs() < 1e-9, "{a}");
    let calls = mock.calls.load(Ordering::SeqCst);
    let (st, a, _) = srv.ask(&key).await;
    assert_eq!(st, 200, "{a}");
    assert_eq!(
        (a["mode"].as_str(), a["note"].as_str()),
        (Some("fallback"), Some("monthly LLM budget of $1.00 reached"))
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), calls, "no provider call");

    // Usage: the rows written by the meter, the month to date and the budget.
    assert!(srv.control.flush_usage().await.unwrap() >= 1);
    let (st, u) = srv.console(Method::GET, &format!("/projects/{r}/usage"), None).await;
    assert_eq!(st, 200, "{u}");
    let rows = u["rows"].as_array().unwrap();
    let asks: i64 = rows.iter().map(|r| r["asks"].as_i64().unwrap()).sum();
    let errors: i64 = rows.iter().map(|r| r["errors"].as_i64().unwrap()).sum();
    assert_eq!((asks, errors), (5, 0), "{u}");
    assert!(rows.iter().any(|r| r["provider"] == "openai-compatible" && r["model"] == "gpt-test"));
    assert!(u["month_to_date"]["cost_usd"].as_f64().unwrap() >= 40.0);
    assert_eq!(u["budget"]["reached"], "monthly LLM budget of $1.00 reached");
    srv.control.reload_usage().await.unwrap();
    assert!(meter.month_to_date().cost_usd >= 40.0, "re-read from usage_daily, counted once");
    assert!(meter.month_to_date().cost_usd < 41.0);
    let (st, _) = srv.console(Method::GET, &format!("/projects/{r}/usage?from=2026-13-01"), None).await;
    assert_eq!(st, 400);

    // Rate limit: 429 with retry-after over HTTP, RESOURCE_EXHAUSTED over gRPC.
    let limited = settings(&base_url, json!({ "asks_per_minute": 1 }));
    assert_eq!(srv.console(Method::PUT, &format!("/projects/{r}/llm"), Some(limited)).await.0, 200);
    assert_eq!(srv.ask(&key).await.0, 200);
    let (st, v, h) = srv.ask(&key).await;
    assert_eq!(st, 429, "{v}");
    assert_eq!(v["error"]["code"], "resource_exhausted");
    assert!(v["error"]["message"].as_str().unwrap().contains("asks_per_minute (1) reached"), "{v}");
    let retry: u64 = h["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry));
    let stream = json!({ "question": "Why is Project Alpha blocked?" });
    assert_eq!(srv.call(Method::POST, "/v1/ask/stream", &key, Some(stream)).await.0, 429);
    let ch = tonic::transport::Channel::from_shared(srv.base.clone()).unwrap().connect().await.unwrap();
    let mut intelli = onto_grpc::pb::intelli_service_client::IntelliServiceClient::new(ch);
    let mut req = tonic::Request::new(onto_grpc::pb::AskRequest {
        question: "Why is Project Alpha blocked?".into(),
        ..Default::default()
    });
    req.metadata_mut().insert("authorization", format!("Bearer {key}").parse().unwrap());
    let status = intelli.ask(req).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::ResourceExhausted, "{status}");

    // A key that does not decrypt: the server's provider, and the error on the LLM page.
    let broken = ProjectLlm { api_key_ct: Some(Sealed(vec![1; 120])), asks_per_minute: None, ..stored.clone() };
    srv.control.store().put_llm(broken).await.unwrap();
    assert!(srv.hub.refresh_tenant(&r).await.unwrap());
    let (_, v) = srv.console(Method::GET, &format!("/projects/{r}/llm"), None).await;
    assert!(v["error"].as_str().unwrap().contains("the stored LLM API key"), "{v}");
    assert_eq!(srv.ask(&key).await.1["provider"], "mock");

    // Back to the server's provider.
    let (st, v) = srv.console(Method::DELETE, &format!("/projects/{r}/llm"), None).await;
    assert_eq!((st, v["deleted"].as_bool()), (200, Some(true)));
    let (_, v) = srv.console(Method::GET, &format!("/projects/{r}/llm"), None).await;
    assert!(v["llm"].is_null() && v["error"].is_null(), "{v}");
    assert_eq!(srv.ask(&key).await.1["provider"], "mock");

    // The audit log has the changes, never the key.
    let (_, audit) = srv.console(Method::GET, "/orgs/default/audit?limit=200", None).await;
    let text = audit.to_string();
    assert!(text.contains("llm.update") && text.contains("llm.delete"));
    assert!(!text.contains(PROJECT_KEY));

    // Ten tests a minute per project.
    let mut last = 0;
    for _ in 0..12 {
        last = srv.console(Method::POST, &format!("/projects/{r}/llm/test"), None).await.0;
    }
    assert_eq!(last, 429);
}

#[tokio::test]
async fn byo_llm_on_memory_control() {
    byo_llm_flow(None).await;
}

#[tokio::test]
async fn byo_llm_on_postgres_control() {
    let Some(url) = database_url() else {
        eprintln!("DATABASE_URL not set: skipping the PostgreSQL BYO LLM test");
        return;
    };
    byo_llm_flow(Some(&url)).await;
}

#[tokio::test]
async fn ssrf_guard_and_master_key() {
    let (_mock, addr) = mock_provider().await;
    // Private addresses refused, and no ONTO_MASTER_KEY.
    let srv = start(false, false, None).await;
    let (r, _) = srv.project().await;
    let path = format!("/projects/{r}/llm");
    let put = |body: Value| srv.console(Method::PUT, &path, Some(body));
    let (st, v) =
        put(json!({ "provider": "ollama", "base_url": format!("http://127.0.0.1:{}/v1", addr.port()) })).await;
    assert_eq!(st, 400, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("https"), "{v}");
    // Credentials in a refused URL are not echoed (a 400, like every refusal here).
    let (st, v) = put(json!({ "provider": "ollama", "base_url": "http://user:s3cret@llm.example.com/v1" })).await;
    assert_eq!(st, 400, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("may not carry credentials"), "{v}");
    assert!(!v.to_string().contains("s3cret"), "{v}");
    let (st, v) =
        put(json!({ "provider": "ollama", "base_url": format!("https://127.0.0.1:{}/v1", addr.port()) })).await;
    assert_eq!(st, 400, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("not a public address"), "{v}");
    assert_eq!(put(json!({ "provider": "ollama", "base_url": "https://[::1]/v1" })).await.0, 400);
    // A name is checked where it resolves, before connecting.
    let (st, v) =
        put(json!({ "provider": "ollama", "base_url": format!("https://localhost:{}/v1", addr.port()) })).await;
    assert_eq!(st, 200, "{v}");
    let (st, t) = srv.console(Method::POST, &format!("/projects/{r}/llm/test"), None).await;
    assert_eq!(st, 200, "{t}");
    assert_eq!(t["ok"], false);
    assert!(t["error"].as_str().unwrap().contains("not a public address"), "{t}");
    // Storing a key needs ONTO_MASTER_KEY; settings without one still work.
    let (st, v) = put(json!({ "provider": "openai-compatible", "base_url": "https://llm.example.com/v1",
                              "api_key": "sk-abcdefghijklmnop" }))
    .await;
    assert_eq!(st, 400, "{v}");
    assert_eq!(v["error"]["message"], "invalid: ONTO_MASTER_KEY is not set; refusing to store LLM API keys");
    assert_eq!(put(json!({ "provider": "none" })).await.0, 200);
    let (_, v) = srv.console(Method::GET, &format!("/projects/{r}/llm"), None).await;
    assert_eq!((v["llm"]["provider"].as_str(), v["master_key"].as_bool()), (Some("none"), Some(false)));
}

#[tokio::test]
async fn master_key_rotation_rewraps_every_key() {
    let old = Arc::new(Vault::new(vec![7; 32], None).unwrap());
    let opts = |v: Arc<Vault>| ControlOptions { vault: Some(v), ..Default::default() };
    let a = Control::memory(Default::default(), opts(old.clone())).unwrap();
    let signed = a.sign_in_local(&["t1".to_string(), "t2".to_string()]).await.unwrap();
    for p in &signed.adopted {
        let u =
            LlmUpdate::from_json(settings("https://llm.example.com/v1", json!({ "api_key": PROJECT_KEY }))).unwrap();
        a.put_llm(p, u, "usr_x", false).await.unwrap();
    }
    assert_eq!(a.rotate_master_key().await.unwrap().current, 2);
    // The same store under a new master key, the old one as previous.
    let store: Arc<dyn ControlStore> = a.store().clone();
    let new = Arc::new(Vault::new(vec![8; 32], Some(vec![7; 32])).unwrap());
    let b = Control::new(store.clone(), Default::default(), opts(new)).unwrap();
    let report = b.rotate_master_key().await.unwrap();
    assert_eq!((report.rewrapped, report.current, report.failed.len()), (2, 0, 0));
    assert_eq!(b.rotate_master_key().await.unwrap().current, 2, "nothing left to rewrap");
    // Only the new key is needed now.
    let only_new =
        Control::new(store, Default::default(), opts(Arc::new(Vault::new(vec![8; 32], None).unwrap()))).unwrap();
    let s = only_new.store().llm(&signed.adopted[0].id).await.unwrap().unwrap();
    assert!(only_new.provider_for(&signed.adopted[0], &s, false).is_ok());
    let s0 = a.store().llm(&signed.adopted[0].id).await.unwrap().unwrap();
    assert!(a.provider_for(&signed.adopted[0], &s0, false).is_err(), "the old key alone no longer opens it");
}
