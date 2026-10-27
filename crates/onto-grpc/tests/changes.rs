//! The change feed's unary surfaces against a real server on 127.0.0.1:0
//! (memory storage): `GET /v1/changes`, `GET /v1/changes/head`,
//! `POST /v1/changes/capture` and `ChangeService` over gRPC, read for the
//! caller's principal, and refused across tenants for config tokens and
//! project API keys.

use std::net::SocketAddr;

use serde_json::{Value, json};
use tokio::sync::oneshot;

use onto_grpc::pb;
use onto_intelli::config::{Config, TokenConfig};
use onto_intelli::{Hub, Principal, Role, seed};

const OPS: &str = "ops-token-for-change-tests";
const SVC: &str = "service-token-for-change-tests";

/// A multi-tenant server: an operator admin limited to tenant ws_a, and a
/// service token on every tenant that acts for its users.
async fn start_hub() -> (SocketAddr, oneshot::Sender<()>) {
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.engine.default_workspace = "acme".into();
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
            principal: Principal::new("nexc-backend", vec![Role::Member], vec!["*".into()]),
            impersonate: true,
            impersonate_roles: vec![Role::Viewer, Role::Member, Role::Agent],
            tenants: vec!["*".into()],
        },
    ];
    let hub = Hub::open(cfg).await.unwrap();
    seed::load_demo(&hub.tenant("ws_a").await.unwrap(), "acme").await.unwrap();
    seed::load_demo(&hub.tenant("ws_b").await.unwrap(), "acme").await.unwrap();
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

/// `(who, roles, entity)` a service token acts as.
type ActAs = (&'static str, &'static str, &'static str);

impl Http {
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        tenant: &str,
        act: Option<ActAs>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
            .header("x-onto-tenant", tenant);
        if let Some((who, roles, entity)) = act {
            req =
                req.header("x-onto-act-as", who).header("x-onto-act-roles", roles).header("x-onto-act-entity", entity);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    async fn get(&self, path: &str, token: &str, tenant: &str, act: Option<ActAs>) -> (u16, Value) {
        self.send(reqwest::Method::GET, path, token, tenant, act, None).await
    }

    async fn post(&self, path: &str, token: &str, tenant: &str, act: Option<ActAs>, body: Value) -> (u16, Value) {
        self.send(reqwest::Method::POST, path, token, tenant, act, Some(body)).await
    }
}

fn ids(page: &Value) -> Vec<(String, String)> {
    page["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("no changes in {page}"))
        .iter()
        .map(|c| (c["op"].as_str().unwrap().to_string(), c["id"].as_str().unwrap().to_string()))
        .collect()
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

fn grpc_req<T>(msg: T, token: &str, tenant: &str) -> tonic::Request<T> {
    let mut req = tonic::Request::new(msg);
    req.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    req.metadata_mut().insert("x-onto-tenant", tenant.parse().unwrap());
    req
}

#[tokio::test]
async fn poll_head_and_capture_over_http_and_grpc() {
    let (addr, _stop) = start_hub().await;
    let h = Http { base: format!("http://{addr}"), http: reqwest::Client::new() };

    let (st, head) = h.get("/v1/changes/head", OPS, "ws_a", None).await;
    assert_eq!(st, 200, "{head}");
    assert_eq!(head["capture"], true, "{head}");
    let start = head["cursor"].as_str().unwrap().to_string();
    assert_eq!(start.split_once('.').unwrap().0, head["head"].to_string(), "{head}");

    // A restricted entity readable by Payments members only, and an update.
    let (st, _) = h
        .post(
            "/v1/call/entity.upsert",
            OPS,
            "ws_a",
            None,
            json!({ "id": "knowledge:bands", "title": "Salary bands",
                    "props": { "visibility": "restricted", "acl": ["team:payments"] } }),
        )
        .await;
    assert_eq!(st, 200);
    let (st, _) = h
        .post("/v1/call/entity.patch", OPS, "ws_a", None, json!({ "id": "issue:421", "props": { "note": "on it" } }))
        .await;
    assert_eq!(st, 200);

    let (st, all) = h.get(&format!("/v1/changes?after={start}"), OPS, "ws_a", None).await;
    assert_eq!(st, 200, "{all}");
    assert_eq!(
        ids(&all),
        vec![("entity.create".into(), "knowledge:bands".into()), ("entity.update".into(), "issue:421".into())]
    );
    assert_eq!(all["changes"][1]["changed"], json!(["note"]), "{all}");
    assert_eq!(all["changes"][1]["cause"]["actor"], "ops");
    assert_eq!(all["expired"], false);

    // Per-user visibility through a service acting for its users; `next`
    // still advances past what the user may not see.
    let kenji = Some(("user:kenji", "member", "person:kenji"));
    let eve = Some(("user:eve", "member", "person:eve"));
    let (_, k) = h.get(&format!("/v1/changes?after={start}"), SVC, "ws_a", kenji).await;
    assert_eq!(ids(&k).len(), 2, "kenji is in team:payments: {k}");
    let (_, e) = h.get(&format!("/v1/changes?after={start}"), SVC, "ws_a", eve).await;
    assert_eq!(ids(&e), vec![("entity.update".into(), "issue:421".into())], "eve is not: {e}");
    assert_eq!(e["next"], all["next"]);

    // Query filters: comma-separated lists, ids-only payloads.
    let (st, f) = h
        .get(&format!("/v1/changes?after={start}&kinds=issue,task&ops=entity.update&fields=ids"), OPS, "ws_a", None)
        .await;
    assert_eq!(st, 200, "{f}");
    assert_eq!(ids(&f), vec![("entity.update".into(), "issue:421".into())]);
    assert!(f["changes"][0]["entity"].is_null(), "{f}");
    let (st, _) = h.get(&format!("/v1/changes?after={start}&ops=entity.created"), OPS, "ws_a", None).await;
    assert_eq!(st, 400);
    let (st, _) = h.get("/v1/changes?after=yesterday", OPS, "ws_a", None).await;
    assert_eq!(st, 400);
    let (st, _) = h.get(&format!("/v1/changes?after={start}&from=now"), OPS, "ws_a", None).await;
    assert_eq!(st, 400, "after or from, not both");
    // `from=now` reads nothing and returns the head to resume from; the
    // default starts at the oldest record kept.
    let (_, now) = h.get("/v1/changes?from=now", OPS, "ws_a", None).await;
    assert!(ids(&now).is_empty());
    assert_eq!(now["next"], all["next"]);
    let (_, earliest) = h.get("/v1/changes?limit=1", OPS, "ws_a", None).await;
    assert_eq!(earliest["changes"].as_array().unwrap().len(), 1, "{earliest}");

    // gRPC: the same actions through ChangeService.
    let ch = tonic::transport::Channel::from_shared(format!("http://{addr}")).unwrap().connect().await.unwrap();
    let mut changes = pb::change_service_client::ChangeServiceClient::new(ch);
    let poll = pb::ChangesPollRequest {
        after: start.clone(),
        kinds: vec!["issue".into()],
        exclude_migrations: true,
        ..Default::default()
    };
    let page = json_of(changes.poll(grpc_req(poll, OPS, "ws_a")).await.unwrap().into_inner());
    assert_eq!(ids(&page), vec![("entity.update".into(), "issue:421".into())], "{page}");
    let head = json_of(changes.head(grpc_req((), OPS, "ws_a")).await.unwrap().into_inner());
    assert_eq!(head["capture"], true, "{head}");
    let off = pb::ChangesCaptureRequest { off: true, ..Default::default() };
    let r = json_of(changes.capture(grpc_req(off, OPS, "ws_a")).await.unwrap().into_inner());
    assert_eq!(r["capture"], false, "{r}");
    let on = pb::ChangesCaptureRequest { on: true, ..Default::default() };
    let r = json_of(changes.capture(grpc_req(on, OPS, "ws_a")).await.unwrap().into_inner());
    assert_eq!(r["capture"], true, "{r}");
    let err = changes.capture(grpc_req(pb::ChangesCaptureRequest::default(), OPS, "ws_a")).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // Capture: admins of every workspace; agents and members are refused.
    let (st, _) = h.post("/v1/changes/capture", OPS, "ws_a", None, json!({})).await;
    assert_eq!(st, 400, "on or off is required");
    let (st, _) = h.post("/v1/changes/capture", SVC, "ws_a", kenji, json!({ "on": false })).await;
    assert_eq!(st, 403);
    let (st, _) =
        h.post("/v1/changes/capture", SVC, "ws_a", Some(("bot", "agent", "agent:bot")), json!({ "on": false })).await;
    assert_eq!(st, 403);
    let (st, off) = h.post("/v1/changes/capture", OPS, "ws_a", None, json!({ "on": false })).await;
    assert_eq!((st, &off["capture"]), (200, &json!(false)), "{off}");
    let before = all["next"].as_str().unwrap().to_string();
    let (st, _) = h
        .post(
            "/v1/call/entity.patch",
            OPS,
            "ws_a",
            None,
            json!({ "id": "issue:421", "props": { "note": "unrecorded" } }),
        )
        .await;
    assert_eq!(st, 200);
    let (_, on) = h.post("/v1/changes/capture", OPS, "ws_a", None, json!({ "on": true })).await;
    assert_eq!(on["capture"], true);
    let (st, expired) = h.get(&format!("/v1/changes?after={before}"), OPS, "ws_a", None).await;
    assert_eq!(st, 200);
    assert_eq!(expired["expired"], true, "{expired}");
    assert!(expired["message"].as_str().unwrap().contains("entity.find"), "{expired}");
    let resumed = on["cursor"].as_str().unwrap();
    let (_, fresh) = h.get(&format!("/v1/changes?after={resumed}"), OPS, "ws_a", None).await;
    assert_eq!(fresh["expired"], false, "{fresh}");

    // Tenants have feeds of their own: ws_b's, read to its end, never
    // mentions what ws_a wrote.
    let mut after = String::new();
    loop {
        let q = if after.is_empty() {
            "ids=knowledge:bands".to_string()
        } else {
            format!("ids=knowledge:bands&after={after}")
        };
        let (st, b) = h.get(&format!("/v1/changes?{q}"), SVC, "ws_b", None).await;
        assert_eq!(st, 200, "{b}");
        assert!(!ids(&b).iter().any(|(_, id)| id == "knowledge:bands"), "ws_b never saw knowledge:bands: {b}");
        let next = b["next"].as_str().unwrap().to_string();
        if next == after {
            break;
        }
        after = next;
    }
}

#[tokio::test]
async fn cross_tenant_poll_is_refused() {
    let (addr, _stop) = start_hub().await;
    let h = Http { base: format!("http://{addr}"), http: reqwest::Client::new() };
    // The operator token is limited to ws_a.
    assert_eq!(h.get("/v1/changes", OPS, "ws_a", None).await.0, 200);
    for path in ["/v1/changes", "/v1/changes?from=now", "/v1/changes/head"] {
        let (st, v) = h.get(path, OPS, "ws_b", None).await;
        assert_eq!(st, 403, "{path}: {v}");
    }
    assert_eq!(h.post("/v1/call/changes.poll", OPS, "ws_b", None, json!({})).await.0, 403);
    assert_eq!(h.post("/v1/changes/capture", OPS, "ws_b", None, json!({ "on": true })).await.0, 403);
    let ch = tonic::transport::Channel::from_shared(format!("http://{addr}")).unwrap().connect().await.unwrap();
    let mut changes = pb::change_service_client::ChangeServiceClient::new(ch);
    let err = changes.poll(grpc_req(pb::ChangesPollRequest::default(), OPS, "ws_b")).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    let err = changes.head(grpc_req((), OPS, "ws_b")).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
}

mod api_keys {
    use super::*;

    use std::sync::Arc;

    use onto_control::{Control, ControlOptions};

    const BOOTSTRAP: &str = "bootstrap-token-for-change-tests-0123456789";

    /// A console server: projects are tenants, API keys belong to one project.
    async fn start() -> (String, oneshot::Sender<()>) {
        let mut cfg = Config::in_memory();
        cfg.engine.tenancy = "multi".into();
        cfg.console.enabled = true;
        cfg.console.public_url = "http://127.0.0.1:50051".into();
        let opts = ControlOptions { bootstrap_token: Some(BOOTSTRAP.into()), ..Default::default() };
        let control = Arc::new(Control::memory(cfg.console.clone(), opts).unwrap());
        let hub = Hub::open(cfg).await.unwrap();
        seed::load_demo(&hub.tenant("legacy").await.unwrap(), "default").await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let opts = onto_grpc::ServeOptions { dev: true, control: Some(control) };
        tokio::spawn(onto_grpc::serve_hub_on(hub, opts, listener, async {
            let _ = rx.await;
        }));
        (format!("http://{addr}"), tx)
    }

    #[tokio::test]
    async fn a_project_key_polls_its_own_project_only() {
        let (base, _stop) = start().await;
        let http = reqwest::Client::new();
        let post = |path: String, token: String, body: Value| {
            let http = http.clone();
            let url = format!("{base}{path}");
            async move {
                let r = http.post(url).bearer_auth(token).json(&body).send().await.unwrap();
                let st = r.status().as_u16();
                (st, r.json::<Value>().await.unwrap_or(Value::Null))
            }
        };
        let mut signed = (0, Value::Null);
        for _ in 0..50 {
            signed = post("/v1/console/auth/token".into(), BOOTSTRAP.into(), json!({})).await;
            if signed.0 == 200 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(signed.0, 200, "{}", signed.1);
        let owner = signed.1["token"].as_str().unwrap().to_string();
        let org = signed.1["org"]["id"].as_str().unwrap().to_string();
        let (st, p) = post(format!("/v1/console/orgs/{org}/projects"), owner.clone(), json!({ "name": "Feed" })).await;
        assert_eq!(st, 200, "{p}");
        let project = p["ref"].as_str().unwrap().to_string();
        let (st, k) = post(
            format!("/v1/console/projects/{project}/keys"),
            owner.clone(),
            json!({ "name": "reader", "scopes": ["read"] }),
        )
        .await;
        assert_eq!(st, 200, "{k}");
        let key = k["secret"].as_str().unwrap().to_string();

        let get = |path: &str, tenant: Option<&str>| {
            let mut req = http.get(format!("{base}{path}")).bearer_auth(&key);
            if let Some(t) = tenant {
                req = req.header("x-onto-tenant", t);
            }
            async move { req.send().await.unwrap().status().as_u16() }
        };
        // The key's project is its tenant.
        assert_eq!(get("/v1/changes", None).await, 200);
        assert_eq!(get("/v1/changes/head", Some(project.as_str())).await, 200);
        // Another project's tenant is refused on poll and head.
        assert_eq!(get("/v1/changes", Some("legacy")).await, 403);
        assert_eq!(get("/v1/changes/head", Some("legacy")).await, 403);
        let r = http
            .post(format!("{base}/v1/call/changes.poll"))
            .bearer_auth(&key)
            .header("x-onto-tenant", "legacy")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }
}
