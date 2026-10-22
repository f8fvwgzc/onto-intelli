//! Data-plane observability against a real server (memory storage, memory
//! control store): trace context on requests (`traceparent`,
//! `x-onto-session-id`, the API key's prefix) and `x-onto-trace-id` on
//! responses; the `/v1/traces`, `/v1/metrics` and `/v1/sessions` routes with
//! a console session, an admin-scope key and a read-scope key (403); and
//! `ObservabilityService` through the generated client.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tonic::transport::Channel;

use onto_control::{Control, ControlOptions};
use onto_grpc::pb;
use onto_intelli::Hub;
use onto_intelli::config::Config;

const BOOTSTRAP: &str = "bootstrap-token-for-observe-tests-0123456789";
const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

struct Srv {
    base: String,
    addr: SocketAddr,
    http: reqwest::Client,
    hub: Arc<Hub>,
    _stop: oneshot::Sender<()>,
}

async fn start() -> Srv {
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.console.enabled = true;
    let opts = ControlOptions { bootstrap_token: Some(BOOTSTRAP.into()), ..Default::default() };
    let control = Arc::new(Control::memory(cfg.console.clone(), opts).unwrap());
    let hub = Hub::open(cfg).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let opts = onto_grpc::ServeOptions { dev: false, control: Some(control) };
    tokio::spawn(onto_grpc::serve_hub_on(hub.clone(), opts, listener, async {
        let _ = rx.await;
    }));
    let srv = Srv { base: format!("http://{addr}"), addr, http: reqwest::Client::new(), hub, _stop: tx };
    for _ in 0..50 {
        if srv.http.get(format!("{}/v1/console/auth/providers", srv.base)).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    srv
}

struct Reply {
    status: u16,
    trace_id: Option<String>,
    body: Value,
}

impl Srv {
    async fn req(
        &self,
        method: Method,
        path: &str,
        token: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Reply {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(token);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let trace_id = resp.headers().get("x-onto-trace-id").map(|v| v.to_str().unwrap().to_string());
        let text = resp.text().await.unwrap_or_default();
        Reply { status, trace_id, body: serde_json::from_str(&text).unwrap_or(Value::String(text)) }
    }

    async fn flush(&self, project: &str) {
        self.hub.tenant(project).await.unwrap().flush_traces().await;
    }
}

fn json_of(s: &prost_types::Struct) -> Value {
    fn val(v: &prost_types::Value) -> Value {
        use prost_types::value::Kind;
        match &v.kind {
            Some(Kind::BoolValue(b)) => json!(b),
            Some(Kind::NumberValue(n)) => json!(n),
            Some(Kind::StringValue(s)) => json!(s),
            Some(Kind::ListValue(l)) => Value::Array(l.values.iter().map(val).collect()),
            Some(Kind::StructValue(s)) => json_of(s),
            _ => Value::Null,
        }
    }
    Value::Object(s.fields.iter().map(|(k, v)| (k.clone(), val(v))).collect())
}

fn struct_of(v: Value) -> prost_types::Struct {
    fn val(v: Value) -> prost_types::Value {
        use prost_types::value::Kind;
        let kind = match v {
            Value::Null => Kind::NullValue(0),
            Value::Bool(b) => Kind::BoolValue(b),
            Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
            Value::String(s) => Kind::StringValue(s),
            Value::Array(a) => Kind::ListValue(prost_types::ListValue { values: a.into_iter().map(val).collect() }),
            Value::Object(m) => {
                Kind::StructValue(prost_types::Struct { fields: m.into_iter().map(|(k, v)| (k, val(v))).collect() })
            }
        };
        prost_types::Value { kind: Some(kind) }
    }
    match val(v).kind {
        Some(prost_types::value::Kind::StructValue(s)) => s,
        _ => prost_types::Struct::default(),
    }
}

fn authed<T>(msg: T, token: &str) -> tonic::Request<T> {
    let mut r = tonic::Request::new(msg);
    r.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    r
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observability_routes_and_service() {
    let srv = start().await;
    // A console owner, a project with the demo data, an admin-scope and a read-scope key.
    let signed = srv.req(Method::POST, "/v1/console/auth/token", BOOTSTRAP, &[], None).await;
    assert_eq!(signed.status, 200, "{}", signed.body);
    let owner = signed.body["token"].as_str().unwrap().to_string();
    let org = signed.body["org"]["id"].as_str().unwrap().to_string();
    let p = srv
        .req(
            Method::POST,
            &format!("/v1/console/orgs/{org}/projects"),
            &owner,
            &[],
            Some(json!({ "name": "Shop", "seed": "demo" })),
        )
        .await;
    assert_eq!(p.status, 200, "{}", p.body);
    let r = p.body["ref"].as_str().unwrap().to_string();
    let key = |scopes: Value| {
        let srv = &srv;
        let (owner, r) = (owner.clone(), r.clone());
        async move {
            let k = srv
                .req(
                    Method::POST,
                    &format!("/v1/console/projects/{r}/keys"),
                    &owner,
                    &[],
                    Some(json!({ "name": "k", "scopes": scopes })),
                )
                .await;
            assert_eq!(k.status, 200, "{}", k.body);
            (k.body["secret"].as_str().unwrap().to_string(), k.body["key"]["prefix"].as_str().unwrap().to_string())
        }
    };
    let (admin_key, admin_prefix) = key(json!(["admin"])).await;
    let (read_key, _) = key(json!(["read"])).await;

    // An ask with a traceparent and a session: the trace adopts both and names the key.
    let ask = json!({ "question": "Who owns Project Alpha?" });
    let a = srv
        .req(
            Method::POST,
            "/v1/ask",
            &admin_key,
            &[("traceparent", TRACEPARENT), ("x-onto-session-id", "chat-42")],
            Some(ask.clone()),
        )
        .await;
    assert_eq!(a.status, 200, "{}", a.body);
    // The trace has an id of its own; the caller's trace id is kept as otel_trace_id.
    let a_id = a.trace_id.clone().unwrap();
    assert_ne!(a_id, "0af7651916cd43dd8448eb211c80319c");
    assert_eq!(a.body["trace_id"], json!(a_id));
    // An invalid traceparent is ignored: a trace id of its own.
    let b = srv
        .req(Method::POST, "/v1/call/intelli.ask", &read_key, &[("traceparent", "01-nope")], Some(ask.clone()))
        .await;
    assert_eq!(b.status, 200, "{}", b.body);
    let b_id = b.trace_id.clone().unwrap();
    assert_ne!(b_id, "0af7651916cd43dd8448eb211c80319c");
    // Writes are traced (calls = "writes"), reads are not.
    let w = srv
        .req(
            Method::POST,
            "/v1/call/entity.upsert",
            &admin_key,
            &[],
            Some(json!({ "kind": "team", "id": "team:obs", "title": "Obs" })),
        )
        .await;
    assert_eq!(w.status, 200, "{}", w.body);
    assert!(w.trace_id.is_some());
    let g = srv.req(Method::POST, "/v1/call/entity.get", &admin_key, &[], Some(json!({ "id": "team:obs" }))).await;
    assert_eq!((g.status, g.trace_id), (200, None));
    // Streaming asks name their trace in the response header too.
    let s = srv.req(Method::POST, "/v1/ask/stream", &admin_key, &[], Some(ask.clone())).await;
    assert_eq!(s.status, 200);
    assert!(s.trace_id.is_some());
    srv.flush(&r).await;

    // A console session (with x-onto-tenant), the admin key: 200. The read key: 403.
    let tenant = [("x-onto-tenant", r.as_str())];
    let list = srv.req(Method::GET, "/v1/traces?kind=ask&limit=10", &owner, &tenant, None).await;
    assert_eq!(list.status, 200, "{}", list.body);
    let rows = list.body["traces"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:#?}");
    let first = rows.iter().find(|t| t["id"] == json!(a_id)).unwrap();
    assert_eq!(
        (first["otel_trace_id"].as_str(), first["parent_span"].as_str()),
        (Some("0af7651916cd43dd8448eb211c80319c"), Some("b7ad6b7169203331"))
    );
    let joined =
        srv.req(Method::GET, "/v1/traces?otel_trace_id=0af7651916cd43dd8448eb211c80319c", &admin_key, &[], None).await;
    assert_eq!(
        joined.body["traces"].as_array().map(|t| t.iter().map(|t| t["id"].clone()).collect::<Vec<_>>()),
        Some(vec![json!(a_id)])
    );
    assert_eq!(first["api_key"], json!(admin_prefix));
    assert_eq!((first["source"].as_str(), first["session_id"].as_str()), (Some("http"), Some("chat-42")));
    assert_eq!(first["preview"], "Who owns Project Alpha?");
    assert!(first["input"].is_null(), "lists carry no bodies");
    let q = srv.req(Method::GET, "/v1/traces?q=2024&since=0", &admin_key, &[], None).await;
    assert_eq!((q.status, q.body["traces"].as_array().map(Vec::len)), (200, Some(0)), "q stays text: {}", q.body);
    let page = srv.req(Method::GET, "/v1/traces?limit=1", &admin_key, &[], None).await;
    let next = page.body["next"].as_str().unwrap().to_string();
    let page2 = srv.req(Method::GET, &format!("/v1/traces?limit=1&cursor={next}"), &admin_key, &[], None).await;
    assert_ne!(page2.body["traces"][0]["id"], page.body["traces"][0]["id"]);
    for path in ["/v1/traces", "/v1/metrics", "/v1/sessions", &format!("/v1/traces/{a_id}")] {
        let denied = srv.req(Method::GET, path, &read_key, &[], None).await;
        assert_eq!(denied.status, 403, "{path}: {}", denied.body);
        assert_eq!(denied.body["error"]["code"], "permission_denied");
    }
    let d = srv.req(Method::GET, &format!("/v1/traces/{a_id}"), &admin_key, &[], None).await;
    assert_eq!(d.status, 200, "{}", d.body);
    assert_eq!(d.body["trace"]["input"]["question"], "Who owns Project Alpha?");
    assert_eq!(d.body["spans"][0]["parent_id"], "b7ad6b7169203331", "the caller's span is the root's parent");
    let missing = srv.req(Method::GET, "/v1/traces/ffffffffffffffffffffffffffffffff", &admin_key, &[], None).await;
    assert_eq!(missing.status, 404);
    let m = srv.req(Method::GET, "/v1/metrics?bucket=hour", &owner, &tenant, None).await;
    assert_eq!(m.status, 200, "{}", m.body);
    assert_eq!(m.body["requests"], 4, "three asks and one write");
    let share = m.body["deterministic_share"].as_f64().unwrap();
    assert!(share > 0.0 && share <= 1.0, "{}", m.body);
    assert!(!m.body["series"].as_array().unwrap().is_empty(), "hourly buckets");
    let bad = srv.req(Method::GET, "/v1/metrics?bucket=week", &admin_key, &[], None).await;
    assert_eq!(bad.status, 400);
    let sessions = srv.req(Method::GET, "/v1/sessions", &admin_key, &[], None).await;
    assert_eq!(sessions.body["sessions"][0]["session_id"], "chat-42", "{}", sessions.body);
    let one = srv.req(Method::GET, "/v1/sessions/chat-42", &owner, &tenant, None).await;
    assert_eq!(one.body["traces"][0]["output"]["answer"], a.body["answer"]);

    // Scores: the console owner (source console); the read key may score its own trace only.
    let sc = srv
        .req(
            Method::POST,
            &format!("/v1/traces/{a_id}/scores"),
            &owner,
            &tenant,
            Some(json!({ "name": "user.feedback", "value": 1, "comment": "right" })),
        )
        .await;
    assert_eq!(sc.status, 200, "{}", sc.body);
    assert_eq!(sc.body["source"], "console");
    let other = srv
        .req(
            Method::POST,
            &format!("/v1/traces/{a_id}/scores"),
            &read_key,
            &[],
            Some(json!({ "name": "x", "value": 0 })),
        )
        .await;
    assert_eq!(other.status, 403, "{}", other.body);
    let own = srv
        .req(
            Method::POST,
            &format!("/v1/traces/{b_id}/scores"),
            &read_key,
            &[],
            Some(json!({ "name": "thumbs", "label": "down" })),
        )
        .await;
    assert_eq!((own.status, own.body["source"].as_str()), (200, Some("api")), "{}", own.body);

    // ObservabilityService through the generated client, with the admin key.
    let ch = Channel::from_shared(format!("http://{}", srv.addr)).unwrap().connect().await.unwrap();
    let mut obs = pb::observability_service_client::ObservabilityServiceClient::new(ch.clone());
    let l = obs.list_traces(authed(struct_of(json!({ "kind": "ask" })), &admin_key)).await.unwrap().into_inner();
    assert_eq!(json_of(&l)["traces"].as_array().unwrap().len(), 3);
    let t = obs.get_trace(authed(pb::EntityRef { id: a_id.clone() }, &admin_key)).await.unwrap().into_inner();
    let t = json_of(&t);
    assert_eq!(t["scores"][0]["name"], "user.feedback");
    let m = json_of(&obs.metrics(authed(struct_of(json!({})), &admin_key)).await.unwrap().into_inner());
    assert_eq!(m["requests"], json!(4.0));
    let s = json_of(&obs.list_sessions(authed(struct_of(json!({})), &admin_key)).await.unwrap().into_inner());
    assert_eq!(s["sessions"][0]["session_id"], "chat-42");
    let s = json_of(
        &obs.get_session(authed(pb::EntityRef { id: "chat-42".into() }, &admin_key)).await.unwrap().into_inner(),
    );
    assert_eq!(s["traces"].as_array().unwrap().len(), 1);
    let scored = obs
        .score(authed(struct_of(json!({ "trace_id": b_id, "name": "accuracy", "value": 0.5 })), &admin_key))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(json_of(&scored)["value"], json!(0.5));
    let denied = obs.list_traces(authed(struct_of(json!({})), &read_key)).await.unwrap_err();
    assert_eq!(denied.code(), tonic::Code::PermissionDenied);

    // gRPC asks carry trace context in and the trace id out (field and metadata).
    let mut intelli = pb::intelli_service_client::IntelliServiceClient::new(ch);
    let mut req = authed(
        pb::AskRequest {
            question: "Who owns Project Alpha?".into(),
            no_cache: true,
            trace: Some(struct_of(json!({ "session_id": "grpc-chat", "tags": ["mobile"] }))),
            ..Default::default()
        },
        &admin_key,
    );
    req.metadata_mut()
        .insert("traceparent", "00-11111111111111111111111111111111-2222222222222222-01".parse().unwrap());
    let resp = intelli.ask(req).await.unwrap();
    let id = resp.get_ref().trace_id.clone();
    assert_eq!(resp.metadata().get("x-onto-trace-id").unwrap(), id.as_str());
    assert_ne!(id, "11111111111111111111111111111111", "a trace id of its own");
    srv.flush(&r).await;
    let d = srv.req(Method::GET, &format!("/v1/traces/{id}"), &admin_key, &[], None).await;
    assert_eq!(d.status, 200, "{}", d.body);
    let tr = &d.body["trace"];
    assert_eq!(tr["otel_trace_id"], "11111111111111111111111111111111");
    assert_eq!((tr["source"].as_str(), tr["session_id"].as_str()), (Some("grpc"), Some("grpc-chat")));
    assert_eq!(tr["tags"], json!(["mobile"]));
    assert_eq!(tr["api_key"], json!(admin_prefix));
}
