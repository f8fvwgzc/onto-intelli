//! The change feed against a real server on 127.0.0.1:0 (memory storage):
//! the unary surfaces (`GET /v1/changes`, `GET /v1/changes/head`,
//! `POST /v1/changes/capture`, `ChangeService` over gRPC) and the SSE
//! stream (`GET|POST /v1/changes/stream`: order, heartbeats through
//! filtered traffic, `Last-Event-ID` resume, `reset`, `too_far_behind`,
//! limits, re-authentication), read for the caller's principal and refused
//! across tenants for config tokens and project API keys.

use std::net::SocketAddr;
use std::time::Duration;

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
    start_hub_with(|_| {}).await
}

async fn start_hub_with(tweak: impl FnOnce(&mut Config)) -> (SocketAddr, oneshot::Sender<()>) {
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
    tweak(&mut cfg);
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
        let (base, _, tx) = start_console().await;
        (base, tx)
    }

    /// The same, re-authenticating streams every second, with its control plane.
    async fn start_console() -> (String, Arc<Control>, oneshot::Sender<()>) {
        let mut cfg = Config::in_memory();
        cfg.engine.tenancy = "multi".into();
        cfg.console.enabled = true;
        cfg.console.public_url = "http://127.0.0.1:50051".into();
        cfg.changes.reauth_secs = 1;
        cfg.changes.heartbeat_secs = 1;
        let opts = ControlOptions { bootstrap_token: Some(BOOTSTRAP.into()), ..Default::default() };
        let control = Arc::new(Control::memory(cfg.console.clone(), opts).unwrap());
        let hub = Hub::open(cfg).await.unwrap();
        seed::load_demo(&hub.tenant("legacy").await.unwrap(), "default").await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let opts = onto_grpc::ServeOptions { dev: true, control: Some(control.clone()) };
        tokio::spawn(onto_grpc::serve_hub_on(hub, opts, listener, async {
            let _ = rx.await;
        }));
        (format!("http://{addr}"), control, tx)
    }

    /// Sign in with the bootstrap token (retried while the server starts):
    /// `(owner session, org id)`.
    async fn owner(http: &reqwest::Client, base: &str) -> (String, String) {
        for _ in 0..50 {
            let r = http.post(format!("{base}/v1/console/auth/token")).bearer_auth(BOOTSTRAP).send().await;
            if let Ok(r) = r
                && r.status() == 200
            {
                let v: Value = r.json().await.unwrap();
                return (v["token"].as_str().unwrap().into(), v["org"]["id"].as_str().unwrap().into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the console did not answer");
    }

    async fn post_json(http: &reqwest::Client, url: String, token: &str, body: Value) -> (u16, Value) {
        let r = http.post(url).bearer_auth(token).json(&body).send().await.unwrap();
        let st = r.status().as_u16();
        (st, r.json::<Value>().await.unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn a_revoked_key_ends_its_stream() {
        let (base, _control, _stop) = start_console().await;
        let http = reqwest::Client::new();
        let (owner, org) = owner(&http, &base).await;
        let (st, p) =
            post_json(&http, format!("{base}/v1/console/orgs/{org}/projects"), &owner, json!({ "name": "Live" })).await;
        assert_eq!(st, 200, "{p}");
        let project = p["ref"].as_str().unwrap().to_string();
        let (st, k) = post_json(
            &http,
            format!("{base}/v1/console/projects/{project}/keys"),
            &owner,
            json!({ "name": "reader", "scopes": ["read"] }),
        )
        .await;
        assert_eq!(st, 200, "{k}");
        let (key, key_id) = (k["secret"].as_str().unwrap().to_string(), k["key"]["id"].as_str().unwrap().to_string());

        // Another project's tenant is refused on the stream too.
        let other = http.get(format!("{base}/v1/changes/stream")).bearer_auth(&key).header("x-onto-tenant", "legacy");
        assert_eq!(other.send().await.unwrap().status(), 403);

        let mut sse = Sse::open(http.get(format!("{base}/v1/changes/stream")).bearer_auth(&key)).await;
        let r = http
            .delete(format!("{base}/v1/console/projects/{project}/keys/{key_id}"))
            .bearer_auth(&owner)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let end = sse.until("end", Duration::from_secs(10)).await;
        assert_eq!(end.data["code"], "unauthenticated", "{:?}", end.data);
    }

    #[tokio::test]
    async fn a_role_downgrade_ends_a_live_stream() {
        let (base, control, _stop) = start_console().await;
        let http = reqwest::Client::new();
        let (owner, org) = owner(&http, &base).await;
        let (st, p) =
            post_json(&http, format!("{base}/v1/console/orgs/{org}/projects"), &owner, json!({ "name": "Ops" })).await;
        assert_eq!(st, 200, "{p}");
        let project = p["ref"].as_str().unwrap().to_string();
        let (st, d) = post_json(&http, format!("{base}/v1/console/auth/dev"), "", json!({})).await;
        assert_eq!(st, 200, "{d}");
        let (dev, dev_id) = (d["token"].as_str().unwrap().to_string(), d["user"]["id"].as_str().unwrap().to_string());
        control.store().add_member(&org, &dev_id, onto_control::OrgRole::Admin).await.unwrap();
        control.evict_org(&org);

        let mut sse = Sse::open(
            http.get(format!("{base}/v1/changes/stream")).bearer_auth(&dev).header("x-onto-tenant", &project),
        )
        .await;
        sse.until("heartbeat", Duration::from_secs(5)).await;
        let r = http
            .patch(format!("{base}/v1/console/orgs/{org}/members/{dev_id}"))
            .bearer_auth(&owner)
            .json(&json!({ "role": "viewer" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        // Within reauth_secs (1 s) plus a heartbeat's wait.
        let end = sse.until("end", Duration::from_secs(10)).await;
        assert_eq!(end.data["code"], "principal_changed", "{:?}", end.data);
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

// ── the SSE stream ───────────────────────────────────────────────────────

/// One SSE message: `event`, `id`, `data` (JSON) and `retry`.
#[derive(Debug)]
struct Ev {
    event: String,
    id: Option<String>,
    data: Value,
    retry: Option<u64>,
}

/// A minimal SSE reader over a streaming response.
struct Sse {
    resp: reqwest::Response,
    buf: String,
}

impl Sse {
    async fn open(req: reqwest::RequestBuilder) -> Sse {
        let resp = req.send().await.unwrap();
        let status = resp.status();
        assert_eq!(status, 200, "{}", resp.text().await.unwrap_or_default());
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        Sse { resp, buf: String::new() }
    }

    /// The next message, or `None` when the stream ends.
    async fn next(&mut self, within: Duration) -> Option<Ev> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if let Some(at) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..at + 2).collect();
                let mut ev = Ev { event: "message".into(), id: None, data: Value::Null, retry: None };
                let mut seen = false;
                for line in block.lines() {
                    let Some((k, v)) = line.split_once(':') else { continue };
                    let v = v.strip_prefix(' ').unwrap_or(v);
                    seen = true;
                    match k {
                        "event" => ev.event = v.to_string(),
                        "id" => ev.id = Some(v.to_string()),
                        "data" => ev.data = serde_json::from_str(v).unwrap_or(Value::String(v.into())),
                        "retry" => ev.retry = v.parse().ok(),
                        _ => seen = false,
                    }
                }
                if seen {
                    return Some(ev);
                }
                continue;
            }
            match tokio::time::timeout_at(deadline, self.resp.chunk()).await {
                Ok(Ok(Some(c))) => self.buf.push_str(&String::from_utf8_lossy(&c)),
                Ok(_) => return None,
                Err(_) => panic!("no SSE message within {within:?}; buffered: {:?}", self.buf),
            }
        }
    }

    /// Messages up to and including the first `event`.
    async fn until(&mut self, event: &str, within: Duration) -> Ev {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.next(left).await {
                Some(ev) if ev.event == event => return ev,
                Some(_) => {}
                None => panic!("the stream ended before a {event} event"),
            }
        }
    }

    /// The next `n` change events (heartbeats skipped).
    async fn changes(&mut self, n: usize) -> Vec<Ev> {
        let mut out = Vec::new();
        while out.len() < n {
            let ev = self.next(Duration::from_secs(10)).await.expect("the stream ended early");
            match ev.event.as_str() {
                "change" => out.push(ev),
                "heartbeat" => {}
                _ if ev.retry.is_some() => {}
                other => panic!("unexpected {other} event: {ev:?}"),
            }
        }
        out
    }
}

fn cursor_of(s: &str) -> (u64, u32) {
    let (a, b) = s.split_once('.').unwrap();
    (a.parse().unwrap(), b.parse().unwrap())
}

impl Http {
    fn stream(&self, query: &str, token: &str, tenant: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}/v1/changes/stream{query}", self.base))
            .bearer_auth(token)
            .header("x-onto-tenant", tenant)
    }

    async fn upsert(&self, id: &str, title: &str) {
        let (st, v) = self.post("/v1/call/entity.upsert", OPS, "ws_a", None, json!({ "id": id, "title": title })).await;
        assert_eq!(st, 200, "{v}");
    }

    async fn head(&self) -> String {
        let (st, h) = self.get("/v1/changes/head", OPS, "ws_a", None).await;
        assert_eq!(st, 200, "{h}");
        h["cursor"].as_str().unwrap().to_string()
    }
}

#[tokio::test]
async fn stream_in_order_heartbeats_through_filtered_traffic_and_resume() {
    let (addr, _stop) = start_hub_with(|c| c.changes.heartbeat_secs = 1).await;
    let h = Http { base: format!("http://{addr}"), http: reqwest::Client::new() };
    let mut sse = Sse::open(h.stream("?ids=task:s1,task:s2", OPS, "ws_a")).await;
    let first = sse.next(Duration::from_secs(5)).await.unwrap();
    assert_eq!(first.retry, Some(2000), "the stream starts with retry: {first:?}");

    h.upsert("task:s0", "not watched").await;
    h.upsert("task:s1", "one").await;
    h.upsert("task:s2", "two").await;
    h.upsert("task:s1", "one, renamed").await;
    let got = sse.changes(3).await;
    let seen: Vec<(String, String)> =
        got.iter().map(|e| (e.data["op"].as_str().unwrap().into(), e.data["id"].as_str().unwrap().into())).collect();
    assert_eq!(
        seen,
        vec![
            ("entity.create".to_string(), "task:s1".to_string()),
            ("entity.create".into(), "task:s2".into()),
            ("entity.update".into(), "task:s1".into()),
        ]
    );
    let ids: Vec<(u64, u32)> = got.iter().map(|e| cursor_of(e.id.as_deref().unwrap())).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "in cursor order: {ids:?}");
    for e in &got {
        assert_eq!(e.id.as_deref(), e.data["cursor"].as_str(), "the SSE id is the record's cursor");
    }
    assert_eq!(got[2].data["before"]["title"], "one", "an admin sees the old title");

    // Traffic the filter drops still moves the cursor: heartbeats pass it.
    for n in 0..3 {
        h.upsert(&format!("task:x{n}"), "elsewhere").await;
    }
    let head = cursor_of(&h.head().await);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = sse.next(left).await.expect("the stream ended");
        assert_eq!(ev.event, "heartbeat", "only heartbeats for filtered traffic: {ev:?}");
        if cursor_of(ev.id.as_deref().unwrap()) >= head {
            assert_eq!(ev.data["cursor"].as_str(), ev.id.as_deref());
            break;
        }
    }
    drop(sse);

    // A reconnect with Last-Event-ID resumes after that record (it wins over after).
    let resume =
        h.stream("?ids=task:s1,task:s2&after=0.0", OPS, "ws_a").header("last-event-id", got[0].id.clone().unwrap());
    let mut again = Sse::open(resume).await;
    let rest = again.changes(2).await;
    assert_eq!(rest[0].id, got[1].id);
    assert_eq!(rest[1].id, got[2].id);

    // POST takes the same keys as a JSON body.
    let post = h
        .http
        .post(format!("{}/v1/changes/stream", h.base))
        .bearer_auth(OPS)
        .header("x-onto-tenant", "ws_a")
        .json(&json!({ "ids": ["task:s2"], "ops": ["entity.create"], "from": "earliest", "fields": "ids" }));
    let mut posted = Sse::open(post).await;
    let one = posted.changes(1).await;
    assert_eq!(one[0].data["id"], "task:s2");
    assert!(one[0].data.get("entity").is_none(), "fields = ids: {:?}", one[0].data);
}

#[tokio::test]
async fn expired_cursors_reset_and_far_behind_streams_end() {
    let (addr, _stop) = start_hub_with(|c| c.changes.max_backfill_seqs = 2).await;
    let h = Http { base: format!("http://{addr}"), http: reqwest::Client::new() };
    let start = h.head().await;
    for n in 0..4 {
        h.upsert(&format!("task:f{n}"), "far").await;
    }
    let mut sse = Sse::open(h.stream(&format!("?after={start}"), OPS, "ws_a")).await;
    let end = sse.until("end", Duration::from_secs(5)).await;
    assert_eq!(end.data["code"], "too_far_behind", "{:?}", end.data);
    assert_eq!(end.data["cursor"].as_str(), Some(start.as_str()));
    assert!(end.data["message"].as_str().unwrap().contains("changes.poll"), "{:?}", end.data);
    assert!(sse.next(Duration::from_secs(2)).await.is_none(), "end is the last event");

    // Capture off, then on: every older cursor is expired.
    for on in [false, true] {
        let (st, v) = h.post("/v1/changes/capture", OPS, "ws_a", None, json!({ "on": on })).await;
        assert_eq!(st, 200, "{v}");
    }
    let mut sse = Sse::open(h.stream(&format!("?after={start}"), OPS, "ws_a")).await;
    let reset = sse.until("reset", Duration::from_secs(5)).await;
    assert_eq!(reset.data["code"], "cursor_expired", "{:?}", reset.data);
    assert!(reset.data["floor"].as_u64().unwrap() > cursor_of(&start).0, "{:?}", reset.data);
    assert!(reset.id.is_none(), "a reset carries no id to resume from");
    assert!(reset.data["message"].as_str().unwrap().contains("entity.find"), "{:?}", reset.data);
    assert!(sse.next(Duration::from_secs(2)).await.is_none(), "reset is the last event");
}

#[tokio::test]
async fn stream_limits_answer_429_and_bad_input_400() {
    let (addr, _stop) = start_hub_with(|c| {
        c.changes.max_subscribers_per_principal = 1;
        c.changes.max_subscribers = 2;
    })
    .await;
    let h = Http { base: format!("http://{addr}"), http: reqwest::Client::new() };
    let _one = Sse::open(h.stream("", OPS, "ws_a")).await;
    let r = h.stream("", OPS, "ws_a").send().await.unwrap();
    assert_eq!(r.status(), 429);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "resource_exhausted", "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("max_subscribers_per_principal"), "{body}");
    // Another principal may, up to max_subscribers per tenant.
    let act = |who: &str| h.stream("", SVC, "ws_a").header("x-onto-act-as", who).header("x-onto-act-roles", "viewer");
    let _two = Sse::open(act("user:kenji")).await;
    let r = act("user:eve").send().await.unwrap();
    assert_eq!(r.status(), 429, "over max_subscribers");
    // Other tenants have their own count.
    let _b =
        Sse::open(h.stream("", SVC, "ws_b").header("x-onto-act-as", "user:kenji").header("x-onto-act-roles", "viewer"))
            .await;
    // Bad input is 400 before anything opens.
    assert_eq!(h.stream("?ops=entity.created", SVC, "ws_b").send().await.unwrap().status(), 400);
    assert_eq!(h.stream("?after=x", SVC, "ws_b").send().await.unwrap().status(), 400);
}

#[tokio::test]
async fn cross_tenant_streams_are_refused() {
    let (addr, _stop) = start_hub().await;
    let h = Http { base: format!("http://{addr}"), http: reqwest::Client::new() };
    let r = h.stream("", OPS, "ws_b").send().await.unwrap();
    assert_eq!(r.status(), 403);
    let r = h.http.post(format!("{}/v1/changes/stream", h.base)).bearer_auth(OPS).header("x-onto-tenant", "ws_b");
    assert_eq!(r.json(&json!({})).send().await.unwrap().status(), 403);
    assert_eq!(h.http.get(format!("{}/v1/changes/stream", h.base)).send().await.unwrap().status(), 401);
}

#[tokio::test]
async fn streams_filter_per_reader() {
    let (addr, _stop) = start_hub_with(|c| c.changes.heartbeat_secs = 1).await;
    let h = Http { base: format!("http://{addr}"), http: reqwest::Client::new() };
    let eve: ActAs = ("user:eve", "member", "person:eve");
    let kenji: ActAs = ("user:kenji", "member", "person:kenji");
    let open = |who: ActAs| {
        h.stream("?ids=knowledge:bands", SVC, "ws_a")
            .header("x-onto-act-as", who.0)
            .header("x-onto-act-roles", who.1)
            .header("x-onto-act-entity", who.2)
    };
    let mut for_eve = Sse::open(open(eve)).await;
    let mut for_kenji = Sse::open(open(kenji)).await;
    let (st, v) = h
        .post(
            "/v1/call/entity.upsert",
            OPS,
            "ws_a",
            None,
            json!({ "id": "knowledge:bands", "title": "Salary bands",
                    "props": { "visibility": "restricted", "acl": ["team:payments"] } }),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    let seen = for_kenji.changes(1).await;
    assert_eq!(seen[0].data["id"], "knowledge:bands");
    let head = cursor_of(&h.head().await);
    // Eve is not in the ACL: heartbeats pass the record, no change event.
    loop {
        let ev = for_eve.next(Duration::from_secs(10)).await.expect("the stream ended");
        if ev.retry.is_some() {
            continue;
        }
        assert_eq!(ev.event, "heartbeat", "eve never sees knowledge:bands: {ev:?}");
        if cursor_of(ev.id.as_deref().unwrap()) >= head {
            break;
        }
    }
}
