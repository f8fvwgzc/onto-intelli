//! MCP over HTTP (`POST /mcp`) against a real server (memory storage, memory
//! control store): `initialize` and `tools/list` with an `agent`-scope key,
//! notifications (202), batches, the protocol version header, `GET` (405),
//! write tools only for principals that may write, traces with the source
//! `mcp` and the key's prefix, and a key for another project refused. Also
//! `GET /v1/console/projects/{ref}/connect`.

use std::sync::Arc;

use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use onto_control::{Control, ControlOptions};
use onto_intelli::Hub;
use onto_intelli::config::Config;

const BOOTSTRAP: &str = "bootstrap-token-for-mcp-http-tests-0123456789";

struct Srv {
    base: String,
    http: reqwest::Client,
    hub: Arc<Hub>,
    _stop: oneshot::Sender<()>,
}

async fn start() -> Srv {
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.console.enabled = true;
    cfg.console.public_url = "http://127.0.0.1:50051/".into();
    cfg.mcp.allow_writes = true;
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
    let srv = Srv { base: format!("http://{addr}"), http: reqwest::Client::new(), hub, _stop: tx };
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
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if !token.is_empty() {
            req = req.bearer_auth(token);
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        Reply { status, body: serde_json::from_str(&text).unwrap_or(Value::String(text)) }
    }

    async fn mcp(&self, token: &str, body: Value) -> Reply {
        self.req(Method::POST, "/mcp", token, &[("accept", "application/json, text/event-stream")], Some(body)).await
    }
}

fn tool_names(r: &Reply) -> Vec<String> {
    r.body["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_over_http() {
    let srv = start().await;
    let signed = srv.req(Method::POST, "/v1/console/auth/token", BOOTSTRAP, &[], None).await;
    assert_eq!(signed.status, 200, "{}", signed.body);
    let owner = signed.body["token"].as_str().unwrap().to_string();
    let org = signed.body["org"]["id"].as_str().unwrap().to_string();
    let project = |name: &'static str| {
        let (srv, owner, org) = (&srv, owner.clone(), org.clone());
        async move {
            let p = srv
                .req(
                    Method::POST,
                    &format!("/v1/console/orgs/{org}/projects"),
                    &owner,
                    &[],
                    Some(json!({ "name": name, "seed": "demo" })),
                )
                .await;
            assert_eq!(p.status, 200, "{}", p.body);
            p.body["ref"].as_str().unwrap().to_string()
        }
    };
    let a = project("Shop").await;
    let b = project("Other").await;
    let key = |r: String, scopes: Value| {
        let (srv, owner) = (&srv, owner.clone());
        async move {
            let k = srv
                .req(
                    Method::POST,
                    &format!("/v1/console/projects/{r}/keys"),
                    &owner,
                    &[],
                    Some(json!({ "name": "mcp", "scopes": scopes })),
                )
                .await;
            assert_eq!(k.status, 200, "{}", k.body);
            (k.body["secret"].as_str().unwrap().to_string(), k.body["key"]["prefix"].as_str().unwrap().to_string())
        }
    };
    let (agent, agent_prefix) = key(a.clone(), json!(["agent"])).await;
    let (reader, _) = key(a.clone(), json!(["read"])).await;
    let (other, _) = key(b.clone(), json!(["agent"])).await;

    // initialize with an agent-scope key.
    let init = srv
        .mcp(
            &agent,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                                "clientInfo": { "name": "test", "version": "0" } } }),
        )
        .await;
    assert_eq!(init.status, 200, "{}", init.body);
    assert_eq!(init.body["id"], 1);
    assert_eq!(init.body["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init.body["result"]["serverInfo"]["name"], "onto-intelli");

    // A notification is accepted with no body.
    let note = srv.mcp(&agent, json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
    assert_eq!(note.status, 202);
    assert_eq!(note.body, Value::String(String::new()));

    // tools/list: the agent may write and [mcp] allow_writes is on, so the
    // write tools are listed; a read-scope key does not get them.
    let tools = srv.mcp(&agent, json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
    assert_eq!(tools.status, 200, "{}", tools.body);
    let names = tool_names(&tools);
    assert!(names.iter().any(|n| n == "ask"), "{names:?}");
    assert!(names.iter().any(|n| n == "apply_action"), "{names:?}");
    let tools = srv.mcp(&reader, json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" })).await;
    assert_eq!(tools.status, 200, "{}", tools.body);
    let names = tool_names(&tools);
    assert!(names.iter().any(|n| n == "ask"), "{names:?}");
    assert!(!names.iter().any(|n| n == "apply_action"), "{names:?}");
    let refused = srv
        .mcp(
            &reader,
            json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": { "name": "undo_action", "arguments": { "receipt": "r" } } }),
        )
        .await;
    assert_eq!(refused.body["result"]["isError"], true, "{}", refused.body);

    // A batch: answers for the requests only, in order.
    let batch = srv
        .mcp(
            &agent,
            json!([
                { "jsonrpc": "2.0", "id": "p", "method": "ping" },
                { "jsonrpc": "2.0", "method": "notifications/initialized" },
                { "jsonrpc": "2.0", "id": "q", "method": "tools/call",
                  "params": { "name": "ask", "arguments": { "question": "Who owns Project Alpha?" } } },
            ]),
        )
        .await;
    assert_eq!(batch.status, 200, "{}", batch.body);
    let replies = batch.body.as_array().unwrap();
    assert_eq!(replies.len(), 2, "{}", batch.body);
    assert_eq!(replies[0]["id"], "p");
    assert_eq!(replies[1]["id"], "q");
    assert_eq!(replies[1]["result"]["isError"], false, "{}", batch.body);
    let only_notes = srv.mcp(&agent, json!([{ "jsonrpc": "2.0", "method": "notifications/initialized" }])).await;
    assert_eq!(only_notes.status, 202);

    // The ask is traced with the source mcp and the key's prefix.
    srv.hub.tenant(&a).await.unwrap().flush_traces().await;
    let traces = srv.req(Method::GET, "/v1/traces?name=intelli.ask", &owner, &[("x-onto-tenant", &a)], None).await;
    assert_eq!(traces.status, 200, "{}", traces.body);
    let t = &traces.body["traces"][0];
    assert_eq!(t["source"], "mcp", "{}", traces.body);
    assert_eq!(t["api_key"], agent_prefix.as_str());

    // Protocol version header, malformed bodies, GET.
    let bad = srv
        .req(
            Method::POST,
            "/mcp",
            &agent,
            &[("mcp-protocol-version", "1999-01-01")],
            Some(json!({ "jsonrpc": "2.0", "id": 5, "method": "ping" })),
        )
        .await;
    assert_eq!(bad.status, 400, "{}", bad.body);
    let ok = srv
        .req(
            Method::POST,
            "/mcp",
            &agent,
            &[("mcp-protocol-version", "2025-03-26")],
            Some(json!({ "jsonrpc": "2.0", "id": 6, "method": "ping" })),
        )
        .await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    let empty = srv.mcp(&agent, json!([])).await;
    assert_eq!(empty.status, 400);
    let scalar = srv.mcp(&agent, json!(42)).await;
    assert_eq!(scalar.status, 400);
    assert_eq!(scalar.body["error"]["code"], -32600);
    let get = srv.req(Method::GET, "/mcp", &agent, &[], None).await;
    assert_eq!(get.status, 405);

    // No credential: 401. A key of another project addressing this one: 403.
    let anon = srv.mcp("", json!({ "jsonrpc": "2.0", "id": 7, "method": "ping" })).await;
    assert_eq!(anon.status, 401, "{}", anon.body);
    let cross = srv
        .req(
            Method::POST,
            "/mcp",
            &other,
            &[("x-onto-tenant", &a)],
            Some(json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/list" })),
        )
        .await;
    assert_eq!(cross.status, 403, "{}", cross.body);
    // The same key reaches its own project.
    let own = srv.mcp(&other, json!({ "jsonrpc": "2.0", "id": 9, "method": "ping" })).await;
    assert_eq!(own.status, 200, "{}", own.body);

    // The Connect route: URLs from public_url, the usable keys by prefix.
    let c = srv.req(Method::GET, &format!("/v1/console/projects/{a}/connect"), &owner, &[], None).await;
    assert_eq!(c.status, 200, "{}", c.body);
    assert_eq!(c.body["api_url"], "http://127.0.0.1:50051");
    assert_eq!(c.body["mcp_url"], "http://127.0.0.1:50051/mcp");
    assert_eq!(c.body["ref"], a.as_str());
    let keys = c.body["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 2, "{}", c.body);
    assert!(keys.iter().all(|k| k.get("secret").is_none() && k["prefix"].as_str().unwrap().starts_with("onto_sk_")));
    assert!(keys.iter().any(|k| k["prefix"] == agent_prefix.as_str() && k["scopes"] == json!(["agent"])));
    let id = srv.req(Method::GET, &format!("/v1/console/projects/{a}/keys"), &owner, &[], None).await.body["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["prefix"] == agent_prefix.as_str())
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let rev = srv.req(Method::DELETE, &format!("/v1/console/projects/{a}/keys/{id}"), &owner, &[], None).await;
    assert_eq!(rev.status, 200, "{}", rev.body);
    let c = srv.req(Method::GET, &format!("/v1/console/projects/{a}/connect"), &owner, &[], None).await;
    assert_eq!(c.body["keys"].as_array().unwrap().len(), 1, "{}", c.body);
    // A revoked key no longer reaches /mcp.
    let gone = srv.mcp(&agent, json!({ "jsonrpc": "2.0", "id": 10, "method": "ping" })).await;
    assert_eq!(gone.status, 401, "{}", gone.body);
}
