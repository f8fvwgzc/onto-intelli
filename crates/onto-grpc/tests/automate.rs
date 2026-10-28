//! Automate against a real server on 127.0.0.1:0 (memory storage): the HTTP
//! routes (`/v1/automations…`) and `AutomateService` over gRPC. An
//! automation defined through the schema routes is enabled over HTTP, the
//! tenant's consumer runs it on a change, its runs are listed and read one
//! by one, dry runs write nothing, the kill switch stops it, and a caller
//! acting as a member may neither enable nor switch it.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::oneshot;

use onto_grpc::pb;
use onto_intelli::config::{Config, TokenConfig};
use onto_intelli::{Hub, Principal, Role};

const OPS: &str = "ops-token-for-automate-tests";
const SVC: &str = "service-token-for-automate-tests";

async fn start() -> (SocketAddr, oneshot::Sender<()>) {
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.engine.default_workspace = "acme".into();
    cfg.webhooks.enabled = false;
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
    hub.tenant("ws_a").await.unwrap();
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
        act: bool,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
            .header("x-onto-tenant", "ws_a");
        if act {
            req = req.header("x-onto-act-as", "user:mia").header("x-onto-act-roles", "member");
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.send(reqwest::Method::GET, path, OPS, false, None).await
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, OPS, false, Some(body)).await
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

fn definition() -> Value {
    json!({ "changes": [{ "op": "put_automation", "name": "resolve_blocked", "def": {
        "description": "Resolve a blocked issue.",
        "run_as": { "roles": ["member"], "workspaces": ["acme"] },
        "trigger": { "on": "change", "kinds": ["issue"], "ops": ["entity.update"], "state": { "to": ["blocked"] } },
        "effects": [{ "action": { "type": "resolve", "params": { "issue": "{trigger.id}" } } }]
    } }] })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automation_routes_over_http_and_grpc() {
    let (addr, _stop) = start().await;
    let h = Http { base: format!("http://{addr}"), http: reqwest::Client::new() };
    let (st, out) = h.post("/v1/schema/apply", definition()).await;
    assert_eq!(st, 200, "{out}");
    let (st, out) =
        h.post("/v1/call/entity.upsert", json!({ "id": "issue:auto1", "title": "Auto", "state": "triaged" })).await;
    assert_eq!(st, 200, "{out}");

    // Listed (readers), not enabled yet.
    let (st, list) = h.get("/v1/automations").await;
    assert_eq!(st, 200, "{list}");
    assert_eq!(list["items"][0]["name"], "resolve_blocked");
    assert_eq!(list["items"][0]["reason"], "not_enabled");
    // A caller acting as a member may not enable it (audited refusal).
    let (st, out) =
        h.send(reqwest::Method::POST, "/v1/automations/resolve_blocked/enable", SVC, true, Some(json!({}))).await;
    assert_eq!(st, 403, "{out}");
    let (st, out) = h.post("/v1/automations/resolve_blocked/enable", json!({})).await;
    assert_eq!((st, &out["enabled"]), (200, &json!(true)), "{out}");
    let (st, got) = h.get("/v1/automations/resolve_blocked").await;
    assert_eq!((st, got["status"].as_str()), (200, Some("active")), "{got}");
    // A dry run writes nothing.
    let (st, t) = h.post("/v1/automations/resolve_blocked/test", json!({ "id": "issue:auto1" })).await;
    assert_eq!((st, t["verdict"].as_str()), (200, Some("would_run")), "{t}");

    // The tenant's consumer runs it on a change.
    let (st, _) = h.post("/v1/call/entity.upsert", json!({ "id": "issue:auto1", "state": "blocked" })).await;
    assert_eq!(st, 200);
    let start = Instant::now();
    let runs = loop {
        let (_, runs) = h.get("/v1/automations/resolve_blocked/runs?state=succeeded").await;
        if runs["items"].as_array().is_some_and(|a| !a.is_empty()) {
            break runs;
        }
        assert!(start.elapsed() < Duration::from_secs(15), "waited for the run: {runs}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let id = runs["items"][0]["id"].as_str().unwrap().to_string();
    let (st, one) = h.get(&format!("/v1/automations/resolve_blocked/runs/{id}")).await;
    assert_eq!((st, one["run"]["state"].as_str()), (200, Some("succeeded")), "{one}");
    assert!(one["note"].as_str().unwrap().contains("delivered at least once"));
    let (st, _) = h.get(&format!("/v1/automations/other/runs/{id}")).await;
    assert_eq!(st, 404, "a run is read under its own automation");
    let (st, out) = h.post(&format!("/v1/automations/resolve_blocked/runs/{id}/rerun"), json!({})).await;
    assert_eq!(st, 400, "a succeeded run is not rerun: {out}");
    let (st, out) = h.post("/v1/automations/resolve_blocked/frob", json!({})).await;
    assert_eq!(st, 404, "{out}");

    // gRPC: AutomateService.
    let ch = tonic::transport::Channel::from_shared(format!("http://{addr}")).unwrap().connect().await.unwrap();
    let mut auto = pb::automate_service_client::AutomateServiceClient::new(ch);
    let list = json_of(auto.list(grpc_req(())).await.unwrap().into_inner());
    assert_eq!(list["items"][0]["name"], "resolve_blocked", "{list}");
    let got = json_of(
        auto.get(grpc_req(pb::AutomationNameRequest { name: "resolve_blocked".into(), ..Default::default() }))
            .await
            .unwrap()
            .into_inner(),
    );
    assert_eq!(got["definition"]["trigger"]["on"], "change");
    let runs = json_of(
        auto.runs(grpc_req(pb::AutomationRunsRequest { name: "resolve_blocked".into(), ..Default::default() }))
            .await
            .unwrap()
            .into_inner(),
    );
    assert_eq!(runs["items"].as_array().unwrap().len(), 1, "{runs}");
    let t = json_of(
        auto.test(grpc_req(pb::AutomationTestRequest {
            name: "resolve_blocked".into(),
            id: "issue:auto1".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner(),
    );
    assert_eq!(t["verdict"], "plan_invalid", "resolved already: {t}");
    let err = auto.enable(grpc_req(pb::AutomationNameRequest::default())).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    let k = json_of(
        auto.kill(grpc_req(pb::AutomationKillRequest { on: true, ..Default::default() })).await.unwrap().into_inner(),
    );
    assert_eq!(k["kill"], true, "{k}");
    let (_, list) = h.get("/v1/automations").await;
    assert_eq!((list["kill"].as_bool(), list["items"][0]["reason"].as_str()), (Some(true), Some("killed")), "{list}");
    // GET on the switch's path reads an automation named kill (none here).
    let (st, _) = h.get("/v1/automations/kill").await;
    assert_eq!(st, 404);
    // Acting as a member: no switching back.
    let (st, out) =
        h.send(reqwest::Method::POST, "/v1/automations/kill", SVC, true, Some(json!({ "off": true }))).await;
    assert_eq!(st, 403, "{out}");
    let (st, out) = h.post("/v1/automations/kill", json!({ "off": true })).await;
    assert_eq!((st, &out["kill"]), (200, &json!(false)), "{out}");
}
