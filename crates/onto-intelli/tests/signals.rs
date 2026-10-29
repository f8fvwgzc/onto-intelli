//! External signals end to end: a feed served on loopback (development
//! reach), its items as events and knowledge (never entities), the
//! recommender's suggestions, and proposals filed but never applied.
#![cfg(feature = "outbound")]

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use onto_intelli::{Config, Hub, Intelli, Principal, Role};

const RSS: &str = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Wire</title>
<item><guid>w1</guid><title>Acme Corp recalls parts used in the Widget Pro</title><link>https://news.example.com/1</link>
<description>Owners of the Widget Pro are asked to stop using it.</description></item>
<item><guid>w2</guid><title>Globex shortage hits Gizmo Max deliveries</title><link>https://news.example.com/2</link></item>
<item><guid>w3</guid><title>Widget Pro maker Acme Corp issues a second recall</title><link>https://news.example.com/3</link></item>
<item><guid>w4</guid><title>Gizmo Max and Globex: what we know</title><link>https://news.example.com/4</link></item>
</channel></rss>"#;

/// A loopback server: `/feed` serves the RSS, `/moved` redirects to it,
/// `/metadata` redirects to the cloud metadata address.
async fn serve() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let resp = match path.as_str() {
                    "/feed" => format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/rss+xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{RSS}",
                        RSS.len()
                    ),
                    "/moved" => "HTTP/1.1 301 Moved\r\nlocation: /feed\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
                    "/metadata" => "HTTP/1.1 302 Found\r\nlocation: http://169.254.169.254/latest/meta-data/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
                    _ => "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    base
}

async fn open(allow_private: bool) -> (Arc<Hub>, Arc<Intelli>) {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    cfg.signals.allow_private_urls = allow_private;
    let hub = Hub::open(cfg).await.unwrap();
    let t = hub.tenant("default").await.unwrap();
    (hub, t)
}

fn admin() -> Principal {
    Principal::admin("ann")
}

async fn call(t: &Intelli, p: &Principal, action: &str, input: Value) -> Value {
    t.call(p, action, input).await.unwrap_or_else(|e| panic!("{action}: {e}"))
}

/// Products use components that vendors make; nothing links products to vendors.
async fn model(t: &Intelli) {
    let changes = json!({ "changes": [
        { "op": "put_object_type", "name": "vendor", "def": {} },
        { "op": "put_object_type", "name": "product", "def": {} },
        { "op": "put_object_type", "name": "component", "def": {} },
        { "op": "put_link_type", "name": "uses", "def": { "from": ["product"], "to": ["component"] } },
        { "op": "put_link_type", "name": "made_by", "def": { "from": ["component"], "to": ["vendor"] } },
    ] });
    call(t, &admin(), "schema.apply", changes).await;
    let mutations = json!({ "mutations": [
        { "op": "upsert", "kind": "vendor", "key": "acme", "title": "Acme Corp" },
        { "op": "upsert", "kind": "vendor", "key": "globex", "title": "Globex" },
        { "op": "upsert", "kind": "component", "key": "x1", "title": "X1 board" },
        { "op": "upsert", "kind": "component", "key": "g7", "title": "G7 cell" },
        { "op": "upsert", "kind": "product", "key": "widget", "title": "Widget Pro" },
        { "op": "upsert", "kind": "product", "key": "gizmo", "title": "Gizmo Max" },
        { "op": "link", "from": "product:widget", "rel": "uses", "to": "component:x1" },
        { "op": "link", "from": "component:x1", "rel": "made_by", "to": "vendor:acme" },
        { "op": "link", "from": "product:gizmo", "rel": "uses", "to": "component:g7" },
        { "op": "link", "from": "component:g7", "rel": "made_by", "to": "vendor:globex" },
    ] });
    call(t, &admin(), "graph.apply", mutations).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feed_items_become_events_and_knowledge_and_suggestions_become_proposals() {
    let base = serve().await;
    let (_hub, t) = open(true).await;
    model(&t).await;
    let entities = call(&t, &admin(), "system.stats", json!({})).await["entities"].clone();
    let version = call(&t, &admin(), "schema.status", json!({})).await["version"].clone();
    // A redirect is followed, and checked again.
    let added = call(&t, &admin(), "signals.feed_add", json!({ "name": "wire", "url": format!("{base}/moved") })).await;
    assert_eq!(added["collection"], "signals:wire");
    let r = call(&t, &admin(), "signals.fetch", json!({ "name": "wire" })).await;
    assert_eq!(
        (r["items"].as_u64(), r["stored"].as_u64(), r["documents"].as_u64()),
        (Some(4), Some(4), Some(4)),
        "{r}"
    );
    let again = call(&t, &admin(), "signals.fetch", json!({ "name": "wire" })).await;
    assert_eq!(again["stored"], 0, "items are stored once");
    // Items are events and knowledge, never entities.
    let events = call(&t, &admin(), "event.list", json!({ "action": "signal.item" })).await;
    assert_eq!(events["events"].as_array().unwrap().len(), 4);
    assert_eq!(events["events"][0]["source"], "feed:wire");
    let found =
        call(&t, &admin(), "knowledge.search", json!({ "query": "second recall", "collection": "signals:wire" })).await;
    assert_eq!(found["passages"][0]["document_name"], "Widget Pro maker Acme Corp issues a second recall", "{found}");
    assert_eq!(call(&t, &admin(), "system.stats", json!({})).await["entities"], entities);
    // Suggestions are read-only.
    let rec = call(&t, &admin(), "signals.recommend", json!({ "min_support": 2 })).await;
    let kinds: Vec<(&str, &str)> = rec["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["kind"].as_str().unwrap(), s["title"].as_str().unwrap()))
        .collect();
    assert!(kinds.iter().any(|(k, t)| *k == "link_type" && t.contains("product_named_with_vendor")), "{rec}");
    assert!(kinds.iter().any(|(k, t)| *k == "rule" && t.contains("uses then made_by")), "{rec}");
    let lt = rec["suggestions"].as_array().unwrap().iter().find(|s| s["kind"] == "link_type").unwrap();
    assert_eq!(lt["support"], 4);
    assert!(lt["evidence"][0]["link"].as_str().unwrap().starts_with("https://news.example.com/"));
    assert_eq!(call(&t, &admin(), "schema.status", json!({})).await["version"], version, "recommend changes nothing");
    // Proposals are filed, once, and not applied.
    let filed = call(&t, &admin(), "signals.propose", json!({ "min_support": 2 })).await;
    assert_eq!(filed["proposed"].as_array().unwrap().len(), 1, "the rule goes with its link type: {filed}");
    let id = filed["proposed"][0]["id"].as_str().unwrap().to_string();
    let p = call(&t, &admin(), "schema.proposal", json!({ "id": id })).await;
    assert_eq!((p["status"].as_str(), p["source"].as_str()), (Some("open"), Some("signals")), "{p}");
    let ops: Vec<&str> =
        p["change_set"]["changes"].as_array().unwrap().iter().map(|c| c["op"].as_str().unwrap()).collect();
    assert_eq!(ops, ["put_link_type", "put_rule"]);
    assert_eq!(call(&t, &admin(), "schema.status", json!({})).await["version"], version, "proposing applies nothing");
    let twice = call(&t, &admin(), "signals.propose", json!({ "min_support": 2 })).await;
    assert_eq!((twice["proposed"].as_array().unwrap().len(), twice["skipped"].as_array().unwrap().len()), (0, 1));
    // Agents may read suggestions; they register, fetch and propose nothing.
    let agent = Principal::new("agent:scout", vec![Role::Agent], vec!["acme".into()]);
    assert!(t.call(&agent, "signals.recommend", json!({ "min_support": 2 })).await.is_ok());
    for (action, input) in [
        ("signals.propose", json!({})),
        ("signals.fetch", json!({ "name": "wire" })),
        ("signals.feed_add", json!({ "name": "x", "url": format!("{base}/feed") })),
    ] {
        assert!(t.call(&agent, action, input).await.is_err(), "{action}");
    }
    // Members read feeds but do not register them.
    let member = Principal::member("bob", "acme");
    assert_eq!(call(&t, &member, "signals.feeds", json!({})).await["feeds"][0]["items"], 4);
    let e =
        t.call(&member, "signals.feed_add", json!({ "name": "y", "url": format!("{base}/feed") })).await.unwrap_err();
    assert!(e.to_string().contains("admins can"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feeds_reach_public_addresses_only() {
    let base = serve().await;
    // Without the development switch, loopback is refused when registering.
    let (_hub, strict) = open(false).await;
    let e = strict.call(&admin(), "signals.feed_add", json!({ "name": "w", "url": format!("{base}/feed") })).await;
    assert!(e.unwrap_err().to_string().contains("https"), "plain http is refused");
    for url in ["https://127.0.0.1/feed", "https://169.254.169.254/latest/meta-data/", "https://[::1]/feed"] {
        assert!(strict.call(&admin(), "signals.feed_add", json!({ "name": "w", "url": url })).await.is_err(), "{url}");
    }
    // With it, a redirect to the metadata address is still refused, and the feed says why.
    let (_hub, dev) = open(true).await;
    call(&dev, &admin(), "signals.feed_add", json!({ "name": "meta", "url": format!("{base}/metadata") })).await;
    let e = dev.call(&admin(), "signals.fetch", json!({ "name": "meta" })).await.unwrap_err();
    assert!(e.to_string().contains("169.254.169.254"), "{e}");
    let feeds = call(&dev, &admin(), "signals.feeds", json!({})).await;
    assert!(feeds["feeds"][0]["last_error"].as_str().unwrap().contains("169.254.169.254"), "{feeds}");
}
