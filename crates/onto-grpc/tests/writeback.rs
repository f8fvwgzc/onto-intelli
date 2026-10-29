//! Writeback targets against a real server on 127.0.0.1:0 (memory storage):
//! the HTTP routes (`/v1/writeback/targets…`) and `WritebackService` over
//! gRPC, and the refusals: callers acting for someone else
//! (`x-onto-act-as`) may not register `env:` references, and errors never
//! contain a resolved value.

use std::net::SocketAddr;

use serde_json::{Value, json};
use tokio::sync::oneshot;

use onto_grpc::pb;
use onto_intelli::config::{AllowConfig, Config, TokenConfig};
use onto_intelli::{Hub, Principal, Role, seed};

const OPS: &str = "ops-token-for-writeback-tests";
const SVC: &str = "service-token-for-writeback-tests";
const URL_VAR: &str = "ONTO_GRPC_TEST_WRITEBACK_URL";
const URL_VALUE: &str = "http://127.0.0.1:9/helpdesk/";

async fn start() -> (SocketAddr, oneshot::Sender<()>) {
    // SAFETY: test-only variable that no other test reads.
    unsafe { std::env::set_var(URL_VAR, URL_VALUE) };
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.engine.default_workspace = "acme".into();
    cfg.writeback.enabled = true;
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
async fn writeback_routes_over_http() {
    let (addr, _stop) = start().await;
    let h = Http { base: format!("http://{addr}"), http: reqwest::Client::new() };
    let get = |path: &'static str| h.send(reqwest::Method::GET, path, OPS, None, None);
    // The operator registers a base URL by reference; outputs name the reference only.
    let add =
        json!({ "name": "helpdesk", "base_url": format!("env:{URL_VAR}"), "routes": { "close": "tickets/close" } });
    let (st, added) = h.send(reqwest::Method::POST, "/v1/writeback/targets", OPS, None, Some(add.clone())).await;
    assert_eq!(st, 200, "{added}");
    assert_eq!(added["target"]["base_url"], format!("env:{URL_VAR}"));
    assert!(!added.to_string().contains("127.0.0.1:9"), "{added}");
    assert!(added["note"].as_str().unwrap().contains("not reversible"));
    let (st, list) = get("/v1/writeback/targets").await;
    assert_eq!((st, list["items"][0]["name"].as_str(), list["enabled"].as_bool()), (200, Some("helpdesk"), Some(true)));

    // Test checks the configuration and sends nothing.
    let (st, t) = h.send(reqwest::Method::POST, "/v1/writeback/targets/helpdesk/test", OPS, None, None).await;
    assert_eq!((st, t["ok"].as_bool()), (200, Some(true)), "{t}");
    assert_eq!(t["base"], format!("env:{URL_VAR}"), "{t}");
    assert!(!t.to_string().contains("127.0.0.1:9"), "the resolved URL is not echoed: {t}");

    // Acting for someone else: no server references, whatever the role; the
    // error names the reference, never its value.
    let mut other = add.clone();
    other["name"] = json!("acting");
    let (st, e) =
        h.send(reqwest::Method::POST, "/v1/writeback/targets", SVC, Some(("user:7", "admin")), Some(other)).await;
    assert_eq!(st, 403, "{e}");
    let text = e.to_string();
    assert!(text.contains(URL_VAR) && !text.contains("127.0.0.1:9"), "{text}");
    // Members are refused the routes altogether.
    let (st, _) = h.send(reqwest::Method::GET, "/v1/writeback/targets", SVC, Some(("user:8", "member")), None).await;
    assert_eq!(st, 403);

    let (st, rm) = h.send(reqwest::Method::DELETE, "/v1/writeback/targets/helpdesk", OPS, None, None).await;
    assert_eq!((st, rm["removed"].as_str()), (200, Some("helpdesk")), "{rm}");
    let (_, list) = get("/v1/writeback/targets").await;
    assert_eq!(list["items"], json!([]));
    let (st, _) = h.send(reqwest::Method::DELETE, "/v1/writeback/targets/helpdesk", OPS, None, None).await;
    assert_eq!(st, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writeback_service_over_grpc() {
    let (addr, _stop) = start().await;
    let ch = tonic::transport::Channel::from_shared(format!("http://{addr}")).unwrap().connect().await.unwrap();
    let mut wb = pb::writeback_service_client::WritebackServiceClient::new(ch);
    let add = pb::WritebackAddRequest {
        name: "crm".into(),
        base_url: "https://crm.example.com/api".into(),
        routes: [("update".to_string(), "contacts/update".to_string())].into_iter().collect(),
        workspaces: vec!["acme".into()],
        ..Default::default()
    };
    let out = json_of(wb.add(grpc_req(add)).await.unwrap().into_inner());
    assert_eq!(out["target"]["routes"], json!({ "update": "contacts/update" }), "{out}");
    assert_eq!(out["target"]["signed"], false, "{out}");
    let list = json_of(wb.list(grpc_req(())).await.unwrap().into_inner());
    assert_eq!(list["items"][0]["name"], "crm", "{list}");
    let t = pb::WritebackTestRequest { name: "crm".into(), route: "update".into() };
    let t = json_of(wb.test(grpc_req(t)).await.unwrap().into_inner());
    assert_eq!((t["ok"].as_bool(), t["routes"][0]["route"].as_str()), (Some(true), Some("update")), "{t}");
    let err = wb.add(grpc_req(pb::WritebackAddRequest::default())).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    let rm = pb::WritebackNameRequest { name: "crm".into(), ..Default::default() };
    let rm = json_of(wb.remove(grpc_req(rm)).await.unwrap().into_inner());
    assert_eq!(rm["removed"], "crm");
}
