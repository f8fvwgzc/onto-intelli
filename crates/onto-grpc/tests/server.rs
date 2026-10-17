//! Real server on a random port, driven by the generated client, a fully
//! dynamic client for a user proto, streaming RPCs and HTTP JSON.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tonic::transport::Channel;

use onto_grpc::dynamic::{DynCodec, from_json, to_json};
use onto_grpc::pb;
use onto_intelli::config::{Config, EXAMPLE_CONFIG, EXAMPLE_PROTO, EXAMPLE_PROTO_PATH};
use onto_intelli::{Intelli, seed};

struct Server {
    addr: SocketAddr,
    dir: PathBuf,
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn start(tweak: impl FnOnce(&mut Config)) -> Server {
    let dir = std::env::temp_dir().join(format!("onto-grpc-test-{}", onto_intelli::core::time::new_id("t")));
    let proto = dir.join("protos").join(EXAMPLE_PROTO_PATH);
    std::fs::create_dir_all(proto.parent().unwrap()).unwrap();
    std::fs::write(&proto, EXAMPLE_PROTO).unwrap();
    let mut cfg = Config::from_toml(EXAMPLE_CONFIG).unwrap();
    cfg.base_dir = dir.clone();
    cfg.engine.storage = "memory".into();
    tweak(&mut cfg);
    let intelli = Intelli::open(cfg).await.unwrap();
    seed::load_demo(&intelli, "acme").await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(onto_grpc::serve_on(intelli, listener, async {
        let _ = rx.await;
    }));
    Server { addr, dir, stop: Some(tx) }
}

async fn channel(addr: SocketAddr) -> Channel {
    Channel::from_shared(format!("http://{addr}")).unwrap().connect().await.unwrap()
}

#[tokio::test]
async fn generated_client_talks_to_the_dynamic_server() {
    let srv = start(|_| {}).await;
    let ch = channel(srv.addr).await;
    let mut onto = pb::ontology_service_client::OntologyServiceClient::new(ch.clone());
    let mut intelli = pb::intelli_service_client::IntelliServiceClient::new(ch.clone());

    let e = onto.get_entity(pb::EntityRef { id: "issue:421".into() }).await.unwrap().into_inner();
    assert_eq!(e.title, "Payment timeout under load");
    assert_eq!(e.state, "blocked");
    let props = e.props.unwrap();
    assert_eq!(props.fields["priority"].kind, Some(prost_types::value::Kind::StringValue("critical".into())));

    let created = onto
        .upsert_entity(pb::EntityInput {
            kind: "task".into(),
            key: "77".into(),
            title: "Add circuit breaker".into(),
            state: "todo".into(),
            links: vec![pb::LinkSpec { rel: "blocked_by".into(), to: "issue:421".into(), props: None }],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(created.id, "task:77");
    assert_eq!(created.workspace, "acme");
    assert_eq!(created.revision, 1);

    let missing = onto.get_entity(pb::EntityRef { id: "issue:0".into() }).await.unwrap_err();
    assert_eq!(missing.code(), tonic::Code::NotFound);

    let r = intelli
        .ask(pb::AskRequest { question: "Who owns Project Alpha?".into(), ..Default::default() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.mode, "deterministic");
    assert!(r.answer.contains("Payments Team"), "{}", r.answer);
    assert_eq!(r.entities[0].id, "project:alpha");

    let rc = intelli
        .root_cause(pb::GraphQuery { id: "project:alpha".into(), ..Default::default() })
        .await
        .unwrap()
        .into_inner();
    let chains = &rc.fields["chains"];
    assert!(format!("{chains:?}").contains("deployment:182"));
}

#[tokio::test]
async fn streaming_rpcs() {
    let srv = start(|_| {}).await;
    let ch = channel(srv.addr).await;
    let mut intelli = pb::intelli_service_client::IntelliServiceClient::new(ch.clone());
    let mut stream = intelli
        .ask_stream(pb::AskRequest {
            question: "Why is Project Alpha blocked?".into(),
            no_cache: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let mut types = Vec::new();
    while let Some(ev) = stream.message().await.unwrap() {
        types.push(ev.r#type);
    }
    assert_eq!(types.first().map(String::as_str), Some("stage"));
    assert!(types.iter().filter(|t| *t == "delta").count() > 1);
    assert_eq!(types.last().map(String::as_str), Some("done"));

    let mut events = pb::event_service_client::EventServiceClient::new(ch);
    let records = vec![
        struct_of(json!({ "op": "upsert", "kind": "issue", "key": "600", "title": "Streamed issue", "state": "open" })),
        struct_of(json!({ "op": "link", "from": "issue:600", "rel": "affects", "to": "service:postgres" })),
    ];
    let summary = events.ingest(tokio_stream::iter(records)).await.unwrap().into_inner();
    assert_eq!(summary.accepted, 2);
    assert!(!summary.queued);
    assert_eq!(summary.result.unwrap().entities, vec!["issue:600".to_string()]);
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

/// Call a method of a runtime-loaded proto with JSON in and JSON out.
async fn dynamic_call(ch: Channel, pool: &DescriptorPool, method: &str, input: Value) -> Result<Value, tonic::Status> {
    let (svc, name) = method.rsplit_once('/').unwrap();
    let md = pool.get_service_by_name(svc).unwrap().methods().find(|m| m.name() == name).unwrap();
    let mut client = tonic::client::Grpc::new(ch);
    client.ready().await.unwrap();
    let req = from_json(&md.input(), input).unwrap();
    let path: http::uri::PathAndQuery = format!("/{method}").parse().unwrap();
    let resp: tonic::Response<DynamicMessage> =
        client.unary(tonic::Request::new(req), path, DynCodec::client(&md)).await?;
    Ok(to_json(resp.get_ref()).unwrap())
}

#[tokio::test]
async fn user_proto_routes_map_onto_actions() {
    let srv = start(|_| {}).await;
    let ch = channel(srv.addr).await;
    let fds = protox::compile([EXAMPLE_PROTO_PATH], [srv.dir.join("protos")]).unwrap();
    let pool = DescriptorPool::from_file_descriptor_set(fds).unwrap();

    let created = dynamic_call(
        ch.clone(),
        &pool,
        "acme.tickets.v1.TicketService/CreateTicket",
        json!({ "ticket_id": "T-9", "title": "Checkout 500s", "priority": "high", "project": "alpha", "assignee": "kenji" }),
    )
    .await
    .unwrap();
    assert_eq!(created, json!({ "id": "issue:T-9", "title": "Checkout 500s", "state": "open", "priority": "high" }));

    // Optional fields missing: the assignee link is dropped, not broken.
    let partial = dynamic_call(
        ch.clone(),
        &pool,
        "acme.tickets.v1.TicketService/CreateTicket",
        json!({ "ticket_id": "T-10", "title": "No owner yet" }),
    )
    .await
    .unwrap();
    assert_eq!(partial["id"], "issue:T-10");

    let got = dynamic_call(ch.clone(), &pool, "acme.tickets.v1.TicketService/GetTicket", json!({ "ticket_id": "T-9" }))
        .await
        .unwrap();
    assert_eq!(got["priority"], "high");

    let answer = dynamic_call(
        ch.clone(),
        &pool,
        "acme.tickets.v1.TicketService/Ask",
        json!({ "question": "Who is assigned to issue T-9?" }),
    )
    .await
    .unwrap();
    assert!(answer["text"].as_str().unwrap().contains("Kenji"), "{answer}");
    assert_eq!(answer["mode"], "deterministic");

    let err = dynamic_call(ch, &pool, "acme.tickets.v1.TicketService/GetTicket", json!({ "ticket_id": "nope" }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn http_json_and_auth() {
    let srv = start(|c| {
        c.auth.mode = "token".into();
        c.auth.tokens = vec![onto_intelli::config::TokenConfig {
            token: Some("s3cret".into()),
            token_env: None,
            principal: onto_intelli::Principal::viewer("dash", "acme"),
        }];
    })
    .await;
    let base = format!("http://{}", srv.addr);
    let http = reqwest::Client::new();

    let denied = http.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(denied.status(), 401);

    let ok: Value =
        http.get(format!("{base}/healthz")).bearer_auth("s3cret").send().await.unwrap().json().await.unwrap();
    assert_eq!(ok["status"], "ok");

    let ans: Value = http
        .post(format!("{base}/v1/ask"))
        .bearer_auth("s3cret")
        .json(&json!({ "question": "How many open issues does the Payments Team have?" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(ans["answer"].as_str().unwrap().contains("2 open issues"), "{ans}");

    // Viewer role: writes are forbidden and audited.
    let write = http
        .post(format!("{base}/v1/call/entity.upsert"))
        .bearer_auth("s3cret")
        .json(&json!({ "kind": "task", "key": "x", "title": "nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(write.status(), 403);

    let ch = channel(srv.addr).await;
    let mut onto = pb::ontology_service_client::OntologyServiceClient::new(ch);
    let err = onto.get_entity(pb::EntityRef { id: "issue:421".into() }).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
    let mut req = tonic::Request::new(pb::EntityRef { id: "issue:421".into() });
    req.metadata_mut().insert("authorization", "Bearer s3cret".parse().unwrap());
    assert_eq!(onto.get_entity(req).await.unwrap().into_inner().id, "issue:421");
}

#[test]
fn route_table_rejects_bad_config() {
    let mut cfg = Config::default();
    cfg.grpc.routes.push(onto_intelli::config::RouteConfig {
        method: "nope.Svc/Do".into(),
        action: "entity.get".into(),
        ..Default::default()
    });
    assert!(onto_grpc::RouteTable::build(&cfg).is_err());
    let cfg = Config::default();
    let table = onto_grpc::RouteTable::build(&cfg).unwrap();
    assert!(table.routes.contains_key("/onto_intelli.v1.IntelliService/Ask"));
    let _ = Arc::new(table);
}
