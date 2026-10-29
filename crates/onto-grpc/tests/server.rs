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
    cfg.auth.mode = "dev".into();
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
            impersonate: false,
            impersonate_roles: Vec::new(),
            tenants: vec!["*".into()],
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

#[tokio::test]
async fn action_and_analytics_services() {
    let srv = start(|_| {}).await;
    let ch = channel(srv.addr).await;
    let mut actions = pb::action_service_client::ActionServiceClient::new(ch.clone());
    let req = |confirm: bool| pb::ActionRequest {
        action: "assign".into(),
        params: Some(struct_of(json!({ "item": "issue:433", "assignee": "person:aiko" }))),
        confirm,
        ..Default::default()
    };
    let plan = actions.plan(req(false)).await.unwrap().into_inner();
    assert_eq!(plan.fields["valid"].kind, Some(prost_types::value::Kind::BoolValue(true)));
    let applied = actions.apply(req(false)).await.unwrap().into_inner();
    let status = |s: &prost_types::Struct| match &s.fields["status"].kind {
        Some(prost_types::value::Kind::StringValue(v)) => v.clone(),
        other => format!("{other:?}"),
    };
    assert_eq!(status(&applied), "applied", "low-risk, reversible: applies without confirm");
    let id = match &applied.fields["id"].kind {
        Some(prost_types::value::Kind::StringValue(v)) => v.clone(),
        _ => panic!("no receipt id"),
    };
    let undone = actions.undo(pb::UndoRequest { receipt: id, ..Default::default() }).await.unwrap().into_inner();
    assert_eq!(status(&undone), "undone");

    let mut analytics = pb::analytics_service_client::AnalyticsServiceClient::new(ch);
    let m =
        analytics.metric(struct_of(json!({ "metric": "open_work", "group_by": ["team"] }))).await.unwrap().into_inner();
    assert!(format!("{:?}", m.fields["rows"]).contains("Payments Team"));
    let q = analytics.quality(struct_of(json!({}))).await.unwrap().into_inner();
    assert!(q.fields.contains_key("score"));
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

#[tokio::test]
async fn schema_service_plans_applies_and_versions() {
    let srv = start(|_| {}).await;
    let ch = channel(srv.addr).await;
    let mut schema = pb::schema_service_client::SchemaServiceClient::new(ch);
    let change = struct_of(json!({
        "op": "put_object_type", "name": "incident",
        "def": { "label": "Incident", "extends": "issue", "properties": { "sev": { "type": "int" } } }
    }));
    let plan = schema
        .plan(pb::SchemaChangeRequest { changes: vec![change.clone()], ..Default::default() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(plan.fields["valid"].kind, Some(prost_types::value::Kind::BoolValue(true)));
    let applied = schema
        .apply(pb::SchemaChangeRequest {
            changes: vec![change],
            message: "incidents".into(),
            expected_version: 1,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(format!("{:?}", applied.fields["version"]).contains("NumberValue(2.0)"));
    let status = schema.status(()).await.unwrap().into_inner();
    assert!(format!("{:?}", status.fields["version"]).contains("2.0"));
    let v1 = schema.get(pb::SchemaVersionRequest { version: 1 }).await.unwrap().into_inner();
    assert!(!format!("{:?}", v1.fields["object_types"]).contains("\"incident\""));
    // Stale expected_version is refused.
    let err = schema
        .apply(pb::SchemaChangeRequest {
            changes: vec![struct_of(json!({ "op": "put_object_type", "name": "x", "def": {} }))],
            expected_version: 1,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");

    // HTTP: a breaking change reports what it needs; history lists versions.
    let base = format!("http://{}", srv.addr);
    let http = reqwest::Client::new();
    let issue: Value = http.get(format!("{base}/v1/schema")).send().await.unwrap().json().await.unwrap();
    assert_eq!(issue["schema_version"], 2);
    let mut def = issue["object_types"]["issue"].clone();
    def["states"] = json!(["open", "resolved", "closed"]);
    let plan: Value = http
        .post(format!("{base}/v1/schema/plan"))
        .json(&json!({ "changes": [ { "op": "put_object_type", "name": "issue", "def": def } ] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(plan["breaking"], true);
    assert!(!plan["needs_migration"].as_array().unwrap().is_empty());
    let refused = http
        .post(format!("{base}/v1/schema/apply"))
        .json(&json!({ "changes": [ { "op": "put_object_type", "name": "issue", "def": def } ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 409);
    let history: Value =
        http.get(format!("{base}/v1/schema/history?limit=5")).send().await.unwrap().json().await.unwrap();
    assert_eq!(history["versions"][0]["version"], 2);
    let diff: Value = http.get(format!("{base}/v1/schema/diff?from=1")).send().await.unwrap().json().await.unwrap();
    assert_eq!(diff["entries"][0]["name"], "incident");
}

#[tokio::test]
async fn proposals_over_grpc_and_http() {
    let srv = start(|_| {}).await;
    let ch = channel(srv.addr).await;
    let mut schema = pb::schema_service_client::SchemaServiceClient::new(ch);
    let filed = schema
        .propose(pb::ProposeRequest {
            title: "Runbooks".into(),
            rationale: "on-call needs them".into(),
            changes: vec![struct_of(json!({ "op": "put_object_type", "name": "runbook", "def": {} }))],
            source: "test".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let id = match &filed.fields["id"].kind {
        Some(prost_types::value::Kind::StringValue(s)) => s.clone(),
        other => panic!("no id: {other:?}"),
    };
    let listed =
        schema.list_proposals(pb::ListProposalsRequest { status: "open".into(), limit: 0 }).await.unwrap().into_inner();
    assert!(format!("{listed:?}").contains(&id));

    let base = format!("http://{}", srv.addr);
    let http = reqwest::Client::new();
    let one: Value = http.get(format!("{base}/v1/schema/proposals/{id}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(one["status"], "open");
    assert_eq!(one["plan"]["changes"][0]["summary"], "add object type runbook");
    // The default test principal proposed it, so it cannot approve its own proposal.
    let own = http.post(format!("{base}/v1/schema/proposals/{id}/approve")).json(&json!({})).send().await.unwrap();
    assert_eq!(own.status(), 403);
    let rejected: Value = http
        .post(format!("{base}/v1/schema/proposals/{id}/withdraw"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rejected["status"], "withdrawn");
    let bad = http.post(format!("{base}/v1/schema/proposals/{id}/explode")).send().await.unwrap();
    assert_eq!(bad.status(), 404);
}

/// A multi-tenant server with a service token that acts for its users.
async fn start_hub() -> (SocketAddr, oneshot::Sender<()>) {
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.engine.default_workspace = "acme".into();
    cfg.auth.tokens = vec![
        onto_intelli::config::TokenConfig {
            token: Some("service-token-for-tests".into()),
            token_env: None,
            principal: onto_intelli::Principal::new("nexc-backend", vec![onto_intelli::Role::Member], vec!["*".into()]),
            impersonate: true,
            impersonate_roles: vec![onto_intelli::Role::Viewer, onto_intelli::Role::Member, onto_intelli::Role::Agent],
            tenants: vec!["*".into()],
        },
        onto_intelli::config::TokenConfig {
            token: Some("tenant-a-only-token".into()),
            token_env: None,
            principal: onto_intelli::Principal::admin("ops"),
            impersonate: false,
            impersonate_roles: Vec::new(),
            tenants: vec!["ws_a".into()],
        },
        onto_intelli::config::TokenConfig {
            token: Some("operator-token-for-tests".into()),
            token_env: None,
            principal: onto_intelli::Principal::admin("operator"),
            impersonate: true,
            impersonate_roles: vec![onto_intelli::Role::Admin],
            tenants: vec!["*".into()],
        },
    ];
    let hub = onto_intelli::Hub::open(cfg).await.unwrap();
    seed::load_demo(&hub.tenant("ws_a").await.unwrap(), "acme").await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(onto_grpc::serve_hub_on(hub, onto_grpc::ServeOptions::default(), listener, async {
        let _ = rx.await;
    }));
    (addr, tx)
}

#[tokio::test]
async fn tenants_are_isolated_and_services_act_for_their_users() {
    let (addr, _stop) = start_hub().await;
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();
    let call = |tenant: &'static str,
                act: Option<(&'static str, &'static str, &'static str)>,
                action: &str,
                body: Value| {
        let mut req = http
            .post(format!("{base}/v1/call/{action}"))
            .bearer_auth("service-token-for-tests")
            .header("x-onto-tenant", tenant)
            .json(&body);
        if let Some((who, roles, entity)) = act {
            req =
                req.header("x-onto-act-as", who).header("x-onto-act-roles", roles).header("x-onto-act-entity", entity);
        }
        req.send()
    };
    // Tenant isolation: ws_b has none of ws_a's data.
    let a = call("ws_a", None, "entity.get", json!({ "id": "project:alpha" })).await.unwrap();
    assert_eq!(a.status(), 200);
    let b = call("ws_b", None, "entity.get", json!({ "id": "project:alpha" })).await.unwrap();
    assert_eq!(b.status(), 404);

    // Per-user ACLs: a restricted entity readable by Payments members only.
    let kenji = Some(("user:kenji", "member", "person:kenji"));
    let eve = Some(("user:eve", "member", "person:eve"));
    call(
        "ws_a",
        None,
        "entity.upsert",
        json!({ "id": "knowledge:bands", "title": "Salary bands",
        "props": { "visibility": "restricted", "acl": ["team:payments"] } }),
    )
    .await
    .unwrap();
    let r = call("ws_a", kenji, "entity.get", json!({ "id": "knowledge:bands" })).await.unwrap();
    assert_eq!(r.status(), 200, "kenji is in team:payments");
    let r = call("ws_a", eve, "entity.get", json!({ "id": "knowledge:bands" })).await.unwrap();
    assert_eq!(r.status(), 404, "eve is not");
    // Writes are attributed to the user, via the service.
    let e: Value = call("ws_a", kenji, "entity.patch", json!({ "id": "issue:421", "props": { "note": "on it" } }))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(e["id"], "issue:421");
    let events: Value = call("ws_a", None, "event.list", json!({ "target": "issue:421", "action": "entity.update" }))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(events["events"][0]["actor"], "user:kenji");
    assert_eq!(events["events"][0]["source"], "api via nexc-backend");
    // A viewer cannot write; an agent cannot apply schema changes.
    let viewer = Some(("user:val", "viewer", "person:val"));
    let r = call("ws_a", viewer, "entity.upsert", json!({ "id": "task:x", "title": "x" })).await.unwrap();
    assert_eq!(r.status(), 403);
    let agent = Some(("assistant", "agent", "agent:assistant"));
    let r = call(
        "ws_a",
        agent,
        "schema.apply",
        json!({ "changes": [ { "op": "put_object_type", "name": "x", "def": {} } ] }),
    )
    .await
    .unwrap();
    assert_eq!(r.status(), 403);
    // No escalation beyond impersonate_roles.
    let admin = Some(("user:mallory", "admin", "person:mallory"));
    let r = call("ws_a", admin, "entity.get", json!({ "id": "project:alpha" })).await.unwrap();
    assert_eq!(r.status(), 403);

    // A token limited to ws_a cannot address ws_b; no token at all is refused.
    let r = http
        .post(format!("{base}/v1/call/entity.get"))
        .bearer_auth("tenant-a-only-token")
        .header("x-onto-tenant", "ws_b")
        .json(&json!({ "id": "project:alpha" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let r = http.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 401);

    // gRPC metadata works the same way.
    let ch = channel(addr).await;
    let mut onto = pb::ontology_service_client::OntologyServiceClient::new(ch);
    let mut req = tonic::Request::new(pb::EntityRef { id: "knowledge:bands".into() });
    req.metadata_mut().insert("authorization", "Bearer service-token-for-tests".parse().unwrap());
    req.metadata_mut().insert("x-onto-tenant", "ws_a".parse().unwrap());
    req.metadata_mut().insert("x-onto-act-as", "user:eve".parse().unwrap());
    req.metadata_mut().insert("x-onto-act-roles", "member".parse().unwrap());
    req.metadata_mut().insert("x-onto-act-entity", "person:eve".parse().unwrap());
    assert_eq!(onto.get_entity(req).await.unwrap_err().code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn development_mode_stays_on_loopback() {
    let intelli = Intelli::in_memory().await.unwrap();
    let hub = onto_intelli::Hub::single(intelli);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let err =
        onto_grpc::serve_hub_on(hub, onto_grpc::ServeOptions { dev: true, ..Default::default() }, listener, async {})
            .await
            .unwrap_err();
    assert!(err.to_string().contains("loopback"), "{err}");
    // Without tokens and without --dev the server refuses to start.
    let intelli = Intelli::in_memory().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err = onto_grpc::serve_on(intelli, listener, async {}).await.unwrap_err();
    assert!(err.to_string().contains("--dev"), "{err}");
}

#[tokio::test]
async fn execution_vocabulary_over_http_and_grpc() {
    let srv = start(|c| c.ontology.extend.push("builtin:nexc-execution".into())).await;
    let base = format!("http://{}", srv.addr);
    let http = reqwest::Client::new();
    let created: Value = http
        .post(format!("{base}/v1/execution/graphs"))
        .json(&json!({ "graph": "g1", "title": "Launch" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["vocabulary"]["node_types"].as_array().unwrap().len(), 6, "{created}");
    let mut vocab: Value =
        http.get(format!("{base}/v1/execution/graphs/g1/vocabulary")).send().await.unwrap().json().await.unwrap();
    assert_eq!(vocab["node_types"][0]["key"], "topic");
    vocab["relation_types"] = json!([{ "key": "depends_on", "label": "Depends on", "blocking": true }]);
    let changed: Value = http
        .put(format!("{base}/v1/execution/graphs/g1/vocabulary"))
        .json(&vocab)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changed["removed"], json!(["relates_to"]));
    let graphs: Value = http.get(format!("{base}/v1/execution/graphs")).send().await.unwrap().json().await.unwrap();
    assert_eq!(graphs["graphs"][0]["id"], "execution_graph:g1");
    // gRPC: whole-number stages arrive as integers.
    let ch = channel(srv.addr).await;
    let mut exec = pb::execution_service_client::ExecutionServiceClient::new(ch);
    let v = exec.vocabulary(pb::GraphRef { graph: "g1".into() }).await.unwrap().into_inner();
    assert!(format!("{:?}", v.fields["node_types"]).contains("\"stage\""));
    let mut node = struct_of(json!({ "key": "qa", "label": "QA", "default_role": "tester", "stage": 5 }));
    node.fields
        .insert("allow_code_exec".into(), prost_types::Value { kind: Some(prost_types::value::Kind::BoolValue(true)) });
    let r = exec
        .init_graph(pb::InitGraphRequest { graph: "g2".into(), node_types: vec![node], ..Default::default() })
        .await
        .unwrap()
        .into_inner();
    assert!(format!("{:?}", r.fields["vocabulary"]).contains("\"qa\""), "{r:?}");
}

#[tokio::test]
async fn web_ui_is_served_and_sessions_carry_one_principal() {
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.ui.embed_origins = vec!["https://nexc.example.com".into(), "*".into()];
    cfg.auth.tokens = vec![onto_intelli::config::TokenConfig {
        token: Some("service-token-for-tests".into()),
        token_env: None,
        principal: onto_intelli::Principal::new("nexc-backend", vec![onto_intelli::Role::Member], vec!["*".into()]),
        impersonate: true,
        impersonate_roles: vec![onto_intelli::Role::Viewer, onto_intelli::Role::Member],
        tenants: vec!["*".into()],
    }];
    let hub = onto_intelli::Hub::open(cfg).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (_tx, rx) = oneshot::channel::<()>();
    tokio::spawn(onto_grpc::serve_hub_on(hub, onto_grpc::ServeOptions::default(), listener, async {
        let _ = rx.await;
    }));
    let base = format!("http://{addr}");
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();

    // The UI shell, with a CSP that only names valid embed origins.
    let ui = http.get(format!("{base}/ui/")).send().await.unwrap();
    if onto_grpc::ui::bundled() {
        assert_eq!(ui.status(), 200);
        let csp = ui.headers()["content-security-policy"].to_str().unwrap().to_string();
        assert!(csp.contains("frame-ancestors 'self' https://nexc.example.com"), "{csp}");
        assert!(!csp.contains(" * "));
        let deep = http.get(format!("{base}/ui/proposals/abc")).send().await.unwrap();
        assert_eq!(deep.status(), 200, "client routes fall back to the shell");
        assert_eq!(http.get(format!("{base}/ui/assets/missing.js")).send().await.unwrap().status(), 404);
    }
    let cfg_json: Value = http.get(format!("{base}/ui/config.json")).send().await.unwrap().json().await.unwrap();
    assert_eq!(cfg_json["embed_origins"], json!(["https://nexc.example.com"]));
    assert_eq!(http.get(format!("{base}/")).send().await.unwrap().status(), 307);

    // A service mints a browser session for one of its users.
    let minted: Value = http
        .post(format!("{base}/v1/auth/session"))
        .bearer_auth("service-token-for-tests")
        .json(&json!({ "act_as": "user:7", "roles": ["member"], "entity": "person:7", "tenant": "ws_1", "ttl_secs": 600 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let session = minted["token"].as_str().unwrap().to_string();
    assert!(session.starts_with("onto1."));
    let me: Value =
        http.get(format!("{base}/v1/auth/whoami")).bearer_auth(&session).send().await.unwrap().json().await.unwrap();
    assert_eq!(me["principal"]["id"], "user:7");
    assert_eq!(me["principal"]["via"], "nexc-backend");
    assert_eq!(me["tenant"], "ws_1");
    assert_eq!(me["capabilities"]["write"], true);
    assert_eq!(me["capabilities"]["apply_schema"], false);
    // Sessions are bound to their tenant, cannot impersonate, and cannot be forged.
    let other = http
        .get(format!("{base}/v1/auth/whoami"))
        .bearer_auth(&session)
        .header("x-onto-tenant", "ws_2")
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 403);
    let act = http
        .get(format!("{base}/v1/auth/whoami"))
        .bearer_auth(&session)
        .header("x-onto-act-as", "user:1")
        .send()
        .await
        .unwrap();
    assert_eq!(act.status(), 403);
    // Change one character inside the signature to one it is not: always a
    // different signature, whatever the token ends with.
    let mut forged = session.clone();
    let i = forged.len() - 5;
    let swap = if forged.as_bytes()[i] == b'A' { "B" } else { "A" };
    forged.replace_range(i..i + 1, swap);
    assert_ne!(forged, session);
    let bad = http.get(format!("{base}/v1/auth/whoami")).bearer_auth(&forged).send().await.unwrap();
    assert_eq!(bad.status(), 401);
    // No escalation through the session endpoint either.
    let admin = http
        .post(format!("{base}/v1/auth/session"))
        .bearer_auth("service-token-for-tests")
        .json(&json!({ "act_as": "user:7", "roles": ["admin"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(admin.status(), 403);
}

#[tokio::test]
async fn ontology_builder_and_object_explorer_over_grpc_and_http() {
    let srv = start(|_| {}).await;
    let ch = channel(srv.addr).await;
    let kind_str = |v: &prost_types::Value| match &v.kind {
        Some(prost_types::value::Kind::StringValue(s)) => s.clone(),
        other => format!("{other:?}"),
    };
    let list_of = |v: &prost_types::Value| match &v.kind {
        Some(prost_types::value::Kind::ListValue(l)) => l.values.clone(),
        _ => Vec::new(),
    };

    // Draft over gRPC: proto3 drops false, so options are negative flags.
    let mut builder = pb::ontology_builder_service_client::OntologyBuilderServiceClient::new(ch.clone());
    let draft = builder
        .draft_ontology(pb::DraftRequest { source: "warehouse".into(), no_actions: true, ..Default::default() })
        .await
        .unwrap()
        .into_inner();
    let mut objects: Vec<String> = list_of(&draft.fields["objects"])
        .iter()
        .filter_map(|o| match &o.kind {
            Some(prost_types::value::Kind::StructValue(s)) => Some(kind_str(&s.fields["name"])),
            _ => None,
        })
        .collect();
    objects.sort();
    assert_eq!(objects, ["customer", "order"]);
    assert!(list_of(&draft.fields["actions"]).is_empty(), "no_actions");
    let advice = builder
        .advise_ontology(pb::AdviseRequest {
            question: "Which link types should orders have?".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(kind_str(&advice.fields["answer"]).contains("customer_id"), "{advice:?}");

    // A preview over gRPC applies nothing; a proposal records its origin.
    let preview = builder
        .apply_draft(pb::ApplyDraftRequest { source: "warehouse".into(), preview: true, ..Default::default() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(kind_str(&preview.fields["mode"]), "preview", "{preview:?}");
    let proposed = builder
        .apply_draft(pb::ApplyDraftRequest {
            source: "warehouse".into(),
            propose: true,
            proposal_source: "grpc-client".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(kind_str(&proposed.fields["mode"]), "proposed", "{proposed:?}");
    assert!(format!("{:?}", proposed.fields["proposal"]).contains("grpc-client"), "{proposed:?}");

    // Apply over HTTP (the development principal is an admin).
    let base = format!("http://{}", srv.addr);
    let http = reqwest::Client::new();
    let applied: Value = http
        .post(format!("{base}/v1/ontology/apply"))
        .json(&json!({ "source": "warehouse" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(applied["mode"], "applied", "{applied}");
    let created: Value = http
        .post(format!("{base}/v1/call/action.apply"))
        .json(&json!({ "action": "create_customer", "params": { "cust_name": "Ada" }, "workspace": "acme" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["status"], "applied", "{created}");
    let types: Value = http.get(format!("{base}/v1/objects/types")).send().await.unwrap().json().await.unwrap();
    let customer = types["types"].as_array().unwrap().iter().find(|t| t["name"] == "customer").unwrap();
    assert_eq!(customer["count"], 1, "{customer}");
    let explored: Value = http
        .post(format!("{base}/v1/objects/explore"))
        .json(&json!({ "kind": "customer", "search": "Ada" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(explored["total"], 1, "{explored}");

    // The Object Explorer over gRPC, with a filter.
    let mut objects = pb::object_service_client::ObjectServiceClient::new(ch);
    let r = objects
        .explore(pb::ExploreRequest {
            kind: "customer".into(),
            filters: vec![struct_of(json!({ "prop": "cust_name", "op": "eq", "value": "Ada" }))],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.fields["total"].kind, Some(prost_types::value::Kind::NumberValue(1.0)), "{r:?}");
    assert_eq!(kind_str(&r.fields["label"]), "Customer");
    let t = objects.object_types(pb::ObjectTypesRequest::default()).await.unwrap().into_inner();
    assert!(format!("{:?}", t.fields["types"]).contains("sales.customers"));
}

#[tokio::test]
async fn sessions_cannot_renew_themselves_and_can_be_revoked() {
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.auth.tokens = vec![
        onto_intelli::config::TokenConfig {
            token: Some("service-token-for-tests".into()),
            token_env: None,
            principal: onto_intelli::Principal::new("svc", vec![onto_intelli::Role::Member], vec!["*".into()]),
            impersonate: true,
            impersonate_roles: vec![onto_intelli::Role::Member],
            tenants: vec!["*".into()],
        },
        onto_intelli::config::TokenConfig {
            token: Some("tenant-a-only-token".into()),
            token_env: None,
            principal: onto_intelli::Principal::viewer("dash", "*"),
            impersonate: false,
            impersonate_roles: Vec::new(),
            tenants: vec!["ws_a".into()],
        },
        onto_intelli::config::TokenConfig {
            token: Some("operator-token-for-tests".into()),
            token_env: None,
            principal: onto_intelli::Principal::admin("operator"),
            impersonate: true,
            impersonate_roles: vec![onto_intelli::Role::Admin],
            tenants: vec!["*".into()],
        },
    ];
    let hub = onto_intelli::Hub::open(cfg).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (_tx, rx) = oneshot::channel::<()>();
    tokio::spawn(onto_grpc::serve_hub_on(hub, onto_grpc::ServeOptions::default(), listener, async {
        let _ = rx.await;
    }));
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();

    // Probes answer without credentials; the health with details still needs them.
    assert_eq!(http.get(format!("{base}/livez")).send().await.unwrap().status(), 200);
    let ready: Value = http.get(format!("{base}/readyz")).send().await.unwrap().json().await.unwrap();
    assert_eq!(ready, json!({ "status": "ready" }));
    assert_eq!(http.get(format!("{base}/healthz")).send().await.unwrap().status(), 401);

    let mint = |bearer: String, body: Value| {
        let (http, base) = (http.clone(), base.clone());
        async move {
            let r = http.post(format!("{base}/v1/auth/session")).bearer_auth(bearer).json(&body).send().await.unwrap();
            assert_eq!(r.status(), 200);
            r.json::<Value>().await.unwrap()
        }
    };
    let first = mint(
        "service-token-for-tests".into(),
        json!({ "act_as": "user:7", "roles": ["member"], "tenant": "ws_1", "ttl_secs": 120 }),
    )
    .await;
    let token = first["token"].as_str().unwrap().to_string();
    assert!(first["jti"].as_str().is_some_and(|j| !j.is_empty()));
    // Before, a session minted another one for another hour, and so on forever.
    let renewed = mint(token.clone(), json!({ "ttl_secs": 3600 })).await;
    assert_eq!(renewed["expires_at"], first["expires_at"], "a session cannot outlive the one that minted it");
    let (renewed, renewed_sid) = (renewed["token"].as_str().unwrap().to_string(), renewed["sid"].clone());

    let whoami = |bearer: String| {
        let (http, base) = (http.clone(), base.clone());
        async move { http.get(format!("{base}/v1/auth/whoami")).bearer_auth(bearer).send().await.unwrap().status() }
    };
    assert_eq!(whoami(renewed.clone()).await, 200);
    // The service revokes the session it minted, by id; the session minted
    // from it goes too.
    let r = http
        .post(format!("{base}/v1/auth/session/revoke"))
        .bearer_auth("service-token-for-tests")
        .header("x-onto-tenant", "ws_1")
        .json(&json!({ "jti": first["jti"], "expires_at": first["expires_at"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(whoami(token.clone()).await, 401);
    assert_eq!(whoami(renewed).await, 401, "a session minted from a revoked one");
    assert_eq!(renewed_sid, first["jti"], "it was in the same login");
    // Sign out ends the login: the session and the one it was minted from.
    let login =
        mint("service-token-for-tests".into(), json!({ "act_as": "user:9", "roles": ["member"], "tenant": "ws_1" }))
            .await;
    let parent = login["token"].as_str().unwrap().to_string();
    let child = mint(parent.clone(), json!({})).await["token"].as_str().unwrap().to_string();
    let out = http.delete(format!("{base}/v1/auth/session")).bearer_auth(&child).send().await.unwrap();
    assert_eq!(out.status(), 200);
    assert_eq!(whoami(child).await, 401);
    assert_eq!(whoami(parent).await, 401);
    // A session cannot revoke by id.
    let other =
        mint("service-token-for-tests".into(), json!({ "act_as": "user:8", "roles": ["member"], "tenant": "ws_1" }))
            .await;
    let r = http
        .post(format!("{base}/v1/auth/session/revoke"))
        .bearer_auth(other["token"].as_str().unwrap())
        .json(&json!({ "jti": other["jti"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);

    let revoke = |bearer: &'static str, jti: Value| {
        let (http, base) = (http.clone(), base.clone());
        async move {
            let r = http
                .post(format!("{base}/v1/auth/session/revoke"))
                .bearer_auth(bearer)
                .json(&json!({ "jti": jti }))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            r.json::<Value>().await.unwrap()
        }
    };
    // Only the first session of a login mints, so revoking any session's id
    // leaves no session minted from it behind.
    let root =
        mint("service-token-for-tests".into(), json!({ "act_as": "user:5", "roles": ["member"], "tenant": "ws_1" }))
            .await;
    let derived = mint(root["token"].as_str().unwrap().to_string(), json!({})).await;
    let r = http
        .post(format!("{base}/v1/auth/session"))
        .bearer_auth(derived["token"].as_str().unwrap())
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "a session minted from a session mints none");
    revoke("service-token-for-tests", derived["jti"].clone()).await;
    assert_eq!(whoami(derived["token"].as_str().unwrap().to_string()).await, 401);
    assert_eq!(whoami(root["token"].as_str().unwrap().to_string()).await, 200, "the session it came from stays");

    // A token limited to one tenant mints sessions for that tenant, never the default one.
    let dash = mint("tenant-a-only-token".into(), json!({})).await;
    assert_eq!(dash["tenant"], "ws_a");
    // It revokes by id for its own tenants only: another tenant's session survives.
    let other =
        mint("service-token-for-tests".into(), json!({ "act_as": "user:6", "roles": ["member"], "tenant": "ws_1" }))
            .await;
    let r = revoke("tenant-a-only-token", other["jti"].clone()).await;
    assert_eq!(whoami(other["token"].as_str().unwrap().to_string()).await, 200, "a ws_1 session");
    assert_eq!(r["tenants"], json!(["ws_a"]), "{r}");
    revoke("tenant-a-only-token", dash["jti"].clone()).await;
    assert_eq!(whoami(dash["token"].as_str().unwrap().to_string()).await, 401, "a ws_a session");
    // A token for every tenant still revokes it, without naming the tenant.
    revoke("service-token-for-tests", other["jti"].clone()).await;
    assert_eq!(whoami(other["token"].as_str().unwrap().to_string()).await, 401);
}

#[tokio::test]
async fn a_revoked_session_is_refused_on_every_replica_whatever_tenant_the_revoke_named() {
    // Two replicas: one hub's storage, one session secret.
    const SECRET: &str = "ONTO_TEST_SESSION_SECRET_REPLICAS";
    // SAFETY: set before this test's servers start; no other test reads it.
    unsafe { std::env::set_var(SECRET, "a-session-secret-of-at-least-32-characters") };
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.auth.session_secret_env = Some(SECRET.into());
    let token = |secret: &str, tenants: &[&str]| onto_intelli::config::TokenConfig {
        token: Some(secret.into()),
        token_env: None,
        principal: onto_intelli::Principal::new("svc", vec![onto_intelli::Role::Member], vec!["*".into()]),
        impersonate: true,
        impersonate_roles: vec![onto_intelli::Role::Member],
        tenants: tenants.iter().map(|t| t.to_string()).collect(),
    };
    cfg.auth.tokens = vec![
        token("service-token-for-tests", &["*"]),
        token("ws1-only-token-for-tests", &["ws_1"]),
        token("ws1-and-ws2-token-for-tests", &["ws_1", "ws_2"]),
    ];
    let hub = onto_intelli::Hub::open(cfg).await.unwrap();
    let mut bases = Vec::new();
    for _ in 0..2 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        bases.push(format!("http://{}", listener.local_addr().unwrap()));
        let serve = onto_grpc::serve_hub_on(hub.clone(), onto_grpc::ServeOptions::default(), listener, async {
            std::future::pending::<()>().await
        });
        tokio::spawn(serve);
    }
    let http = reqwest::Client::new();
    let status = |base: &str, bearer: &str| {
        let r = http.get(format!("{base}/v1/auth/whoami")).bearer_auth(bearer);
        async move { r.send().await.unwrap().status() }
    };
    for (n, header) in [(1, None), (2, Some("ws_2"))] {
        let minted: Value = http
            .post(format!("{}/v1/auth/session", bases[0]))
            .bearer_auth("service-token-for-tests")
            .json(&json!({ "act_as": format!("user:{n}"), "roles": ["member"], "tenant": "ws_1" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = minted["token"].as_str().unwrap();
        // The tenant the revoke names (none, or another one) and an expiry
        // in the past do not matter: the session id is revoked everywhere.
        let mut revoke = http
            .post(format!("{}/v1/auth/session/revoke", bases[0]))
            .bearer_auth("service-token-for-tests")
            .json(&json!({ "jti": minted["jti"], "expires_at": 1 }));
        if let Some(t) = header {
            revoke = revoke.header("x-onto-tenant", t);
        }
        assert_eq!(revoke.send().await.unwrap().status(), 200);
        assert_eq!(status(&bases[0], token).await, 401, "{header:?}: this replica");
        assert_eq!(status(&bases[1], token).await, 401, "{header:?}: the other replica");
    }

    let mint = |bearer: String, body: Value| {
        let r = http.post(format!("{}/v1/auth/session", bases[0])).bearer_auth(bearer).json(&body);
        async move {
            let r = r.send().await.unwrap();
            assert_eq!(r.status(), 200);
            r.json::<Value>().await.unwrap()
        }
    };
    let revoke = |bearer: &str, tenant: Option<&str>, jti: &Value| {
        let mut r =
            http.post(format!("{}/v1/auth/session/revoke", bases[0])).bearer_auth(bearer).json(&json!({ "jti": jti }));
        if let Some(t) = tenant {
            r = r.header("x-onto-tenant", t);
        }
        async move {
            let r = r.send().await.unwrap();
            (r.status(), r.json::<Value>().await.unwrap_or_default())
        }
    };
    let user = |n: u32, tenant: &str| json!({ "act_as": format!("user:{n}"), "roles": ["member"], "tenant": tenant });
    let svc = || "service-token-for-tests".to_string();
    // A token limited to ws_1 revokes for ws_1 only (not for the look-alike
    // WS-1). The other replica has never seen these ids, so it reads the store.
    let root = mint(svc(), user(3, "ws_1")).await;
    let derived = mint(root["token"].as_str().unwrap().to_string(), json!({})).await;
    let alias = mint(svc(), user(4, "WS-1")).await;
    // The same id in another tenant (signed as the server signs).
    let (jti, exp) = (root["jti"].as_str().unwrap(), root["expires_at"].as_u64().unwrap());
    let elsewhere = signed_session("a-session-secret-of-at-least-32-characters", "ws_2", jti, exp);
    for id in [&root["jti"], &alias["jti"]] {
        let (st, body) = revoke("ws1-only-token-for-tests", None, id).await;
        assert_eq!((st.as_u16(), &body["tenants"]), (200, &json!(["ws_1"])), "{body}");
    }
    let on_other = |minted: &Value| status(&bases[1], minted["token"].as_str().unwrap());
    assert_eq!(on_other(&root).await, 401, "the session");
    assert_eq!(on_other(&derived).await, 401, "a session minted from it: its login, for ws_1");
    assert_eq!(on_other(&alias).await, 200, "WS-1 is another tenant than ws_1");
    assert_eq!(status(&bases[1], &elsewhere).await, 200, "the same id in ws_2");
    // A token limited to two tenants names one of them, and revokes in both.
    let other = mint(svc(), user(5, "ws_2")).await;
    let (st, _) = revoke("ws1-and-ws2-token-for-tests", None, &other["jti"]).await;
    assert_eq!(st.as_u16(), 403, "two tenants: the request names one");
    let (st, body) = revoke("ws1-and-ws2-token-for-tests", Some("ws_1"), &other["jti"]).await;
    assert_eq!((st.as_u16(), &body["tenants"]), (200, &json!(["ws_1", "ws_2"])), "{body}");
    assert_eq!(on_other(&other).await, 401, "a ws_2 session, revoked by a request that named ws_1");
}

/// A session token for `tenant` with the id `jti` (also its login), signed
/// with `secret` the way the server signs them.
fn signed_session(secret: &str, tenant: &str, jti: &str, exp: u64) -> String {
    use base64::Engine;
    use hmac::{KeyInit, Mac};
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = json!({
        "principal": { "id": "user:x", "roles": ["member"], "workspaces": ["*"] },
        "tenant": tenant, "exp": exp, "jti": jti, "sid": jti,
    });
    let payload = format!("onto1.{}", b64.encode(serde_json::to_vec(&claims).unwrap()));
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(payload.as_bytes());
    format!("{payload}.{}", b64.encode(mac.finalize().into_bytes()))
}

/// `GET /v1/admin/tenants` carries each open tenant's outbound counters
/// (docs/design/EVENTS.md section 11) and the process's subscriber count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_tenants_report_outbound_counters() {
    let (addr, _stop) = start_hub().await;
    let http = reqwest::Client::new();
    let r: Value = http
        .get(format!("http://{addr}/v1/admin/tenants"))
        .bearer_auth("operator-token-for-tests")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(r["subscribers_process"].is_u64(), "{r}");
    let t = r["tenants"].as_array().unwrap().iter().find(|t| t["tenant"] == "ws_a").expect("ws_a is open");
    let o = &t["outbound"];
    for key in ["lag", "outbox", "outbox_open", "gaps_24h", "runs_1h", "paused"] {
        assert!(o.get(key).is_some(), "{key} missing: {o}");
    }
    assert_eq!(o["gaps_24h"]["count"], 0, "{o}");
    assert!(o["outbox"]["targets"].is_object(), "{o}");
    assert!(o["lag"]["endpoints"].is_object(), "{o}");

    // A member is refused.
    let denied = http
        .get(format!("http://{addr}/v1/admin/tenants"))
        .bearer_auth("service-token-for-tests")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
}

/// An admin token limited to some tenants is not an operator: the report
/// would show other tenants' endpoint, target and consumer names and
/// counts, which every per-tenant route refuses it. Neither is an admin
/// acting for someone else, nor a session bound to one tenant.
#[tokio::test]
async fn admin_tenants_is_refused_to_credentials_limited_to_some_tenants() {
    let (addr, _stop) = start_hub().await;
    let http = reqwest::Client::new();
    let status = |token: &'static str, act: Option<(&'static str, &'static str)>| {
        let http = http.clone();
        async move {
            let mut rq = http.get(format!("http://{addr}/v1/admin/tenants")).bearer_auth(token);
            if let Some((who, roles)) = act {
                rq = rq.header("x-onto-act-as", who).header("x-onto-act-roles", roles);
            }
            let r = rq.send().await.unwrap();
            let code = r.status().as_u16();
            (code, r.text().await.unwrap())
        }
    };
    let (code, body) = status("tenant-a-only-token", None).await;
    assert_eq!(code, 403, "{body}");
    assert!(!body.contains("ws_a"), "{body}");
    let (code, body) = status("operator-token-for-tests", Some(("alice", "admin"))).await;
    assert_eq!(code, 403, "acting for someone else: {body}");
    let (code, body) = status("operator-token-for-tests", None).await;
    assert_eq!(code, 200, "{body}");

    // A session minted by the operator token is bound to one tenant.
    let minted: Value = http
        .post(format!("http://{addr}/v1/auth/session"))
        .bearer_auth("operator-token-for-tests")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = minted["token"].as_str().expect("a session").to_string();
    let r = http.get(format!("http://{addr}/v1/admin/tenants")).bearer_auth(token).send().await.unwrap();
    assert_eq!(r.status(), 403);
}

/// A graceful shutdown with a request whose body is still arriving: one
/// that completes within the grace period is served by its open tenant (no
/// 503), and one still open at the deadline is cut, so it is never served
/// after `run_graceful` returned and released the leases.
#[tokio::test]
async fn graceful_shutdown_serves_requests_in_flight_and_cuts_the_rest_at_the_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve(grace: std::time::Duration) -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let hub = onto_intelli::Hub::open(Config::in_memory()).await.unwrap();
        hub.resolve(None).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let opts = onto_grpc::ServeOptions { dev: true, ..Default::default() };
        let server = onto_grpc::prepare(hub, opts, listener).await.unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let run = tokio::spawn(async move {
            server
                .run_graceful(
                    async {
                        let _ = rx.await;
                    },
                    grace,
                )
                .await
                .unwrap();
        });
        (addr, tx, run)
    }
    const HEAD: &str = "POST /v1/call/system.health HTTP/1.1\r\nhost: x\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{";

    // Completed within the grace period: served.
    let (addr, stop, run) = serve(std::time::Duration::from_secs(5)).await;
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(HEAD.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    stop.send(()).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    c.write_all(b"}").await.unwrap();
    let mut out = vec![0u8; 4096];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), c.read(&mut out)).await.unwrap().unwrap();
    let text = String::from_utf8_lossy(&out[..n]).to_string();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    drop(c);
    tokio::time::timeout(std::time::Duration::from_secs(10), run).await.unwrap().unwrap();

    // Still open at the deadline: cut, never answered afterwards.
    let (addr, stop, run) = serve(std::time::Duration::from_secs(1)).await;
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(HEAD.as_bytes()).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let t0 = std::time::Instant::now();
    stop.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), run).await.unwrap().unwrap();
    assert!(t0.elapsed() < std::time::Duration::from_secs(4), "{:?}", t0.elapsed());
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let _ = c.write_all(b"}").await;
    let mut out = vec![0u8; 4096];
    match tokio::time::timeout(std::time::Duration::from_secs(3), c.read(&mut out)).await {
        Ok(Ok(n)) => assert_eq!(n, 0, "answered after the cut: {}", String::from_utf8_lossy(&out[..n])),
        Ok(Err(_)) => {}
        Err(_) => panic!("the connection was neither cut nor answered"),
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

#[tokio::test]
async fn knowledge_over_grpc_with_blocks_and_imported_embeddings() {
    let srv = start(|_| {}).await;
    let mut kb = pb::knowledge_service_client::KnowledgeServiceClient::new(channel(srv.addr).await);
    // Parsed blocks, as a document parser returns them: a heading, text, a table.
    let block = |kind: &str, text: &str| pb::KnowledgeBlock {
        kind: kind.into(),
        text: text.into(),
        page: 1,
        ..Default::default()
    };
    let row = |cells: &[&str]| pb::KnowledgeRow { cells: cells.iter().map(|c| c.to_string()).collect() };
    let table = pb::KnowledgeBlock {
        kind: "table".into(),
        rows: vec![row(&["Region", "Revenue"]), row(&["EMEA", "12"]), row(&["APAC", "9"])],
        header_rows: 1,
        page: 2,
        ..Default::default()
    };
    let doc = kb
        .ingest(pb::KnowledgeIngestRequest {
            name: "Report.pdf".into(),
            blocks: vec![block("heading", "Revenue"), block("text", "Revenue grew in every region."), table],
            pages: 2,
            sensitivity: "internal".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let doc = json_of(doc);
    assert_eq!(doc["document"]["passages"], 2.0, "{doc}");
    assert_eq!(doc["document"]["pages"], 2.0);
    let hits = kb
        .search(pb::KnowledgeSearchRequest { query: "APAC revenue".into(), k: 3, ..Default::default() })
        .await
        .unwrap()
        .into_inner();
    let hits = json_of(hits);
    assert_eq!(hits["passages"][0]["content"], "Region: EMEA; Revenue: 12\nRegion: APAC; Revenue: 9", "{hits}");
    assert_eq!(hits["passages"][0]["citation"], "Report.pdf, p. 2 › Revenue");
    // Imported passages with their vectors, found by the query's vector.
    kb.ingest(pb::KnowledgeIngestRequest {
        id: "doc-9".into(),
        name: "Imported.md".into(),
        embedding_model: "m-3".into(),
        passages: vec![pb::KnowledgePassageInput {
            id: "c-1".into(),
            content: "Lorem ipsum".into(),
            embedding: vec![0.0, 1.0],
            ..Default::default()
        }],
        ..Default::default()
    })
    .await
    .unwrap();
    let by_vector = kb
        .search(pb::KnowledgeSearchRequest {
            query: "unrelated words".into(),
            embedding: Some(pb::KnowledgeQueryEmbedding { model: "m-3".into(), vector: vec![0.1, 0.9] }),
            mode: "vector".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(json_of(by_vector)["passages"][0]["passage_id"], "c-1");
    let stats = json_of(kb.stats(()).await.unwrap().into_inner());
    assert_eq!((stats["documents"].clone(), stats["embedded"]["m-3"].clone()), (json!(2.0), json!(1.0)));
}
