//! The AssuranceService RPCs and the assurance HTTP routes on a real server:
//! the reasoner, certificates and the independent checker, packs and lint.

use std::net::SocketAddr;
use std::path::PathBuf;

use prost_reflect::{DescriptorPool, DynamicMessage};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tonic::transport::Channel;

use onto_grpc::dynamic::{DynCodec, from_json, to_json};
use onto_grpc::{BUILTIN_DESCRIPTOR, pb};
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

async fn start() -> Server {
    let dir = std::env::temp_dir().join(format!("onto-grpc-assure-{}", onto_intelli::core::time::new_id("t")));
    let proto = dir.join("protos").join(EXAMPLE_PROTO_PATH);
    std::fs::create_dir_all(proto.parent().unwrap()).unwrap();
    std::fs::write(&proto, EXAMPLE_PROTO).unwrap();
    let mut cfg = Config::from_toml(EXAMPLE_CONFIG).unwrap();
    cfg.base_dir = dir.clone();
    cfg.engine.storage = "memory".into();
    cfg.auth.mode = "dev".into();
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

/// Call a built-in method with JSON in and JSON out.
async fn call(ch: Channel, method: &str, input: Value) -> Result<Value, tonic::Status> {
    let pool = DescriptorPool::decode(BUILTIN_DESCRIPTOR).unwrap();
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

const SVC: &str = "onto_intelli.v1.AssuranceService";

#[tokio::test]
async fn assurance_rpcs_reason_certify_and_check() {
    let srv = start().await;
    let ch = channel(srv.addr).await;

    // The generated client works too.
    let mut client = pb::assurance_service_client::AssuranceServiceClient::new(ch.clone());
    let run = client.run(pb::ReasonRunRequest { samples: 2, ..Default::default() }).await.unwrap().into_inner();
    assert!(run.fields.contains_key("not_evaluated") && run.fields.contains_key("samples"));
    let packs = client.list_packs(()).await.unwrap().into_inner();
    assert!(packs.fields.contains_key("items"));

    let q = call(
        ch.clone(),
        &format!("{SVC}/Query"),
        json!({ "pattern": [["?i", "handled_by_team", "team:payments"]], "limit": 10 }),
    )
    .await
    .unwrap();
    assert!(q["bindings"].as_array().unwrap().iter().any(|b| b["i"] == "<issue:421>"), "{q}");

    let ex = call(
        ch.clone(),
        &format!("{SVC}/Explain"),
        json!({ "fact": ["issue:421", "handled_by_team", "team:payments"] }),
    )
    .await
    .unwrap();
    assert_eq!(ex["status"], "derived", "{ex}");

    let cert = call(ch.clone(), &format!("{SVC}/Certify"), json!({})).await.unwrap();
    assert_eq!(cert["check"]["ok"], true, "{}", cert["check"]);
    assert_eq!(cert["check"]["verdict"], "entailed_under_supplied_rules");
    let c = &cert["certificate"];
    let mut files = json!({
        "rules_tsv": c["rules_tsv"], "asserted_tsv": c["asserted_tsv"],
        "derivations_tsv": c["derivations_tsv"], "manifest_json": c["manifest_json"],
    });
    let checked = call(ch.clone(), &format!("{SVC}/CheckCertificate"), files.clone()).await.unwrap();
    assert_eq!(checked, cert["check"], "the same files, the same report");
    // An appended forged conclusion is refused at its line.
    let n = c["derivations_tsv"].as_str().unwrap().lines().count();
    let forged = format!(
        "{}schema/responsible_team\t<issue:421>\thandled_by_team\t<team:forged>\t<issue:421>\tassigned_to\t<person:x>\t<person:x>\tmember_of\t<team:forged>\n",
        c["derivations_tsv"].as_str().unwrap()
    );
    files["derivations_tsv"] = json!(forged);
    files["manifest_json"] = json!("");
    let refused = call(ch.clone(), &format!("{SVC}/CheckCertificate"), files).await.unwrap();
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["first_rejected"], n + 1, "{refused}");

    let lint = call(ch.clone(), &format!("{SVC}/Lint"), json!({})).await.unwrap();
    assert!(lint["checks_run"].is_array(), "{lint}");
    let plan = call(ch.clone(), &format!("{SVC}/PlanPack"), json!({ "id": "retail" })).await.unwrap();
    assert_eq!(plan["valid"], true, "{plan}");
    let missing = call(ch, &format!("{SVC}/ShowPack"), json!({ "id": "nope" })).await.unwrap_err();
    assert_eq!(missing.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn assurance_http_routes() {
    let srv = start().await;
    let base = format!("http://{}", srv.addr);
    let http = reqwest::Client::new();
    let get = |path: &str| http.get(format!("{base}{path}")).send();
    let packs: Value = get("/v1/packs").await.unwrap().json().await.unwrap();
    assert!(packs["items"].as_array().unwrap().iter().any(|p| p["id"] == "retail"), "{packs}");
    let pack: Value = get("/v1/packs/retail").await.unwrap().json().await.unwrap();
    assert_eq!(pack["id"], "retail");
    assert!(pack["lint"].is_object());
    let lint: Value = get("/v1/schema/lint?checks=missing_inverse,orphan_type&include_dismissed=true")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(lint["means"].is_string(), "{lint}");
    assert_eq!(lint["checks_run"], json!(["missing_inverse", "orphan_type"]), "{lint}");

    let post = |path: &str, body: Value| http.post(format!("{base}{path}")).json(&body).send();
    let q: Value = post("/v1/reason/query", json!({ "pattern": [["?i", "handled_by_team", "?t"]] }))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!q["bindings"].as_array().unwrap().is_empty(), "{q}");
    let cert: Value = post("/v1/reason/certify", json!({})).await.unwrap().json().await.unwrap();
    assert_eq!(cert["check"]["ok"], true);
    let v: Value = post(
        "/v1/reason/verify",
        json!({ "claims": [{ "subject": "issue:421", "relation": "handled_by_team", "object": "team:payments" }] }),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(v["verdicts"][0]["verdict"], "derived", "{v}");
    let shapes: Value = post("/v1/shapes/validate", json!({})).await.unwrap().json().await.unwrap();
    assert!(shapes["verdict"].is_string(), "{shapes}");
    let plan: Value = post("/v1/packs/retail/plan", json!({})).await.unwrap().json().await.unwrap();
    assert_eq!(plan["valid"], true, "{plan}");
    let installed: Value =
        post("/v1/packs/retail/install", json!({ "sample": true })).await.unwrap().json().await.unwrap();
    assert_eq!(installed["pack"], "retail", "{installed}");
    assert!(installed["sample_entities"].as_u64().unwrap() > 0, "{installed}");
    let bad = post("/v1/packs/retail/explode", json!({})).await.unwrap();
    assert_eq!(bad.status(), 404);
}
