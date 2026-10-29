//! Webhook endpoints against a real server on 127.0.0.1:0 (memory storage):
//! the HTTP routes (`/v1/webhooks…`, `/v1/outbox`) and `WebhookService`
//! over gRPC, a test delivery to a local receiver, the kill switch, and the
//! refusals: callers acting for someone else (`x-onto-act-as`) may not
//! register `env:` references, and errors never contain a resolved value.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use onto_grpc::pb;
use onto_intelli::config::{AllowConfig, Config, TokenConfig};
use onto_intelli::{Hub, Principal, Role, seed};

const OPS: &str = "ops-token-for-webhook-tests";
const SVC: &str = "service-token-for-webhook-tests";
const SECRET_VAR: &str = "ONTO_GRPC_TEST_WEBHOOK_SECRET";
const SECRET_VALUE: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

async fn start() -> (SocketAddr, oneshot::Sender<()>) {
    // SAFETY: test-only variable that no other test reads.
    unsafe { std::env::set_var(SECRET_VAR, SECRET_VALUE) };
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.engine.default_workspace = "acme".into();
    cfg.webhooks.poll_secs = 1;
    cfg.webhooks.allow = vec![AllowConfig { host: "127.0.0.1".into(), cidrs: vec!["127.0.0.1/32".into()] }];
    cfg.auth.tokens = vec![
        TokenConfig {
            token: Some(OPS.into()),
            token_env: None,
            principal: Principal::admin("ops"),
            impersonate: false,
            impersonate_roles: Vec::new(),
            tenants: vec!["ws_a".into()],
        },
        TokenConfig {
            token: Some(SVC.into()),
            token_env: None,
            principal: Principal::new("nexc-backend", vec![Role::Admin], vec!["*".into()]),
            impersonate: true,
            impersonate_roles: vec![Role::Viewer, Role::Member, Role::Admin],
            tenants: vec!["*".into()],
        },
    ];
    let hub = Hub::open(cfg).await.unwrap();
    seed::load_demo(&hub.tenant("ws_a").await.unwrap(), "acme").await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(onto_grpc::serve_hub_on(hub, onto_grpc::ServeOptions::default(), listener, async {
        let _ = rx.await;
    }));
    (addr, tx)
}

/// A receiver that records the `webhook-id` of every request and answers 200.
async fn receiver() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let ids: Arc<Mutex<Vec<String>>> = Default::default();
    let seen = ids.clone();
    let app = axum::Router::new().fallback(move |headers: axum::http::HeaderMap| {
        let seen = seen.clone();
        async move {
            let id = headers.get("webhook-id").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            seen.lock().push(id);
            axum::http::StatusCode::OK
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}/hooks"), ids)
}

struct Http {
    base: String,
    http: reqwest::Client,
}

impl Http {
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        act: Option<(&str, &str)>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
            .header("x-onto-tenant", "ws_a");
        if let Some((who, roles)) = act {
            req = req.header("x-onto-act-as", who).header("x-onto-act-roles", roles);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    async fn get(&self, path: &str, token: &str) -> (u16, Value) {
        self.send(reqwest::Method::GET, path, token, None, None).await
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, token, None, Some(body)).await
    }
}

fn json_of(s: prost_types::Struct) -> Value {
    fn val(v: prost_types::Value) -> Value {
        use prost_types::value::Kind;
        match v.kind {
            None | Some(Kind::NullValue(_)) => Value::Null,
            Some(Kind::BoolValue(b)) => Value::Bool(b),
            Some(Kind::NumberValue(n)) => json!(n),
            Some(Kind::StringValue(s)) => Value::String(s),
            Some(Kind::ListValue(l)) => Value::Array(l.values.into_iter().map(val).collect()),
            Some(Kind::StructValue(s)) => json_of(s),
        }
    }
    Value::Object(s.fields.into_iter().map(|(k, v)| (k, val(v))).collect())
}

fn grpc_req<T>(msg: T) -> tonic::Request<T> {
    let mut req = tonic::Request::new(msg);
    req.metadata_mut().insert("authorization", format!("Bearer {OPS}").parse().unwrap());
    req.metadata_mut().insert("x-onto-tenant", "ws_a".parse().unwrap());
    req
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhook_routes_over_http() {
    let (addr, _stop) = start().await;
    let (url, received) = receiver().await;
    let h = Http { base: format!("http://{addr}"), http: reqwest::Client::new() };
    let add = json!({
        "name": "ops", "url": url, "secret": format!("env:{SECRET_VAR}"),
        "filter": { "kinds": ["issue"] }, "principal": { "roles": ["viewer"], "workspaces": ["acme"] },
    });
    let (st, added) = h.post("/v1/webhooks", OPS, add.clone()).await;
    assert_eq!(st, 200, "{added}");
    assert_eq!(added["endpoint"]["secret"], format!("env:{SECRET_VAR}"));
    assert!(!added.to_string().contains(SECRET_VALUE), "{added}");
    let (st, list) = h.get("/v1/webhooks", OPS).await;
    assert_eq!((st, list["items"][0]["name"].as_str()), (200, Some("ops")), "{list}");
    let (_, who) = h.get("/v1/auth/whoami", OPS).await;
    assert_eq!(who["capabilities"]["configure_outbound"], true, "{who}");

    // Update through PATCH; unknown verbs are not found.
    let (st, up) =
        h.send(reqwest::Method::PATCH, "/v1/webhooks/ops", OPS, None, Some(json!({ "disabled": true }))).await;
    assert_eq!((st, up["endpoint"]["status"].as_str()), (200, Some("disabled")), "{up}");
    let (st, _) = h.send(reqwest::Method::PATCH, "/v1/webhooks/ops", OPS, None, Some(json!({ "enabled": true }))).await;
    assert_eq!(st, 200);
    let (st, _) = h.post("/v1/webhooks/ops/bogus", OPS, json!({})).await;
    assert_eq!(st, 404);

    // A test delivery reaches the receiver, and its row is listed.
    let (st, t) = h.post("/v1/webhooks/ops/test", OPS, json!({})).await;
    assert_eq!(st, 200, "{t}");
    let id = t["id"].as_str().unwrap().to_string();
    let start = Instant::now();
    while !received.lock().contains(&id) {
        assert!(start.elapsed() < Duration::from_secs(10), "no delivery");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (st, d) = h.get("/v1/webhooks/ops/deliveries?states=delivered,dead&limit=5", OPS).await;
    assert_eq!((st, d["items"][0]["id"].as_str()), (200, Some(id.as_str())), "{d}");
    let (st, one) = h.get(&format!("/v1/webhooks/ops/deliveries?id={id}"), OPS).await;
    assert_eq!((st, one["attempts"][0]["status"].as_u64()), (200, Some(200)), "{one}");
    let (st, ob) = h.get("/v1/outbox?target=ops", OPS).await;
    assert_eq!((st, ob["items"].as_array().map(Vec::len)), (200, Some(1)), "{ob}");
    let (st, r) = h.post("/v1/webhooks/ops/replay", OPS, json!({ "ids": [id] })).await;
    assert_eq!((st, r["replayed"].as_u64()), (200, Some(1)), "{r}");

    // The kill switch.
    let (st, k) = h.post("/v1/webhooks/kill", OPS, json!({ "on": true })).await;
    assert_eq!((st, k["kill"].as_bool()), (200, Some(true)), "{k}");
    let (st, k) = h.post("/v1/webhooks/kill", OPS, json!({ "off": true })).await;
    assert_eq!((st, k["kill"].as_bool()), (200, Some(false)), "{k}");
    let (st, _) = h.post("/v1/webhooks", OPS, json!({ "name": "kill", "url": "https://hooks.example.com/" })).await;
    assert_eq!(st, 400, "kill is the switch's path");

    // Acting for someone else: no server references, whatever the role; the
    // error names the reference, never its value.
    let mut other = add.clone();
    other["name"] = json!("acting");
    let (st, e) = h.send(reqwest::Method::POST, "/v1/webhooks", SVC, Some(("user:7", "admin")), Some(other)).await;
    assert_eq!(st, 403, "{e}");
    let text = e.to_string();
    assert!(text.contains(SECRET_VAR) && !text.contains(SECRET_VALUE), "{text}");
    // Members are refused the routes altogether.
    let (st, _) = h.send(reqwest::Method::GET, "/v1/webhooks", SVC, Some(("user:8", "member")), None).await;
    assert_eq!(st, 403);

    let (st, _) = h.send(reqwest::Method::DELETE, "/v1/webhooks/ops", OPS, None, None).await;
    assert_eq!(st, 200);
    let (_, list) = h.get("/v1/webhooks", OPS).await;
    assert_eq!(list["items"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhook_service_over_grpc() {
    let (addr, _stop) = start().await;
    let ch = tonic::transport::Channel::from_shared(format!("http://{addr}")).unwrap().connect().await.unwrap();
    let mut hooks = pb::webhook_service_client::WebhookServiceClient::new(ch);
    let add = pb::WebhookAddRequest {
        name: "grpc".into(),
        url: "https://hooks.example.com/in".into(),
        secret: format!("env:{SECRET_VAR}"),
        filter: Some(pb::WebhookFilter { kinds: vec!["issue".into()], exclude_migrations: true, ..Default::default() }),
        principal: Some(pb::WebhookPrincipal { roles: vec!["viewer".into()], workspaces: vec!["acme".into()] }),
        ..Default::default()
    };
    let out = json_of(hooks.add(grpc_req(add)).await.unwrap().into_inner());
    assert_eq!(out["endpoint"]["filter"]["kinds"], json!(["issue"]), "{out}");
    assert_eq!(out["endpoint"]["filter"]["include_migrations"], false, "{out}");
    let list = json_of(hooks.list(grpc_req(())).await.unwrap().into_inner());
    assert_eq!(list["items"][0]["name"], "grpc", "{list}");
    let up = pb::WebhookUpdateRequest { name: "grpc".into(), disabled: true, ..Default::default() };
    let up = json_of(hooks.update(grpc_req(up)).await.unwrap().into_inner());
    assert_eq!(up["endpoint"]["enabled"], false, "{up}");
    let err = hooks.add(grpc_req(pb::WebhookAddRequest::default())).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    let kill = json_of(
        hooks.kill(grpc_req(pb::WebhookKillRequest { on: true, ..Default::default() })).await.unwrap().into_inner(),
    );
    assert_eq!(kill["kill"], true);
    let ob = json_of(hooks.outbox(grpc_req(pb::OutboxListRequest::default())).await.unwrap().into_inner());
    assert_eq!(ob["items"], json!([]), "{ob}");
    let rm = pb::WebhookNameRequest { name: "grpc".into(), ..Default::default() };
    let rm = json_of(hooks.remove(grpc_req(rm)).await.unwrap().into_inner());
    assert_eq!(rm["removed"], "grpc");
}
