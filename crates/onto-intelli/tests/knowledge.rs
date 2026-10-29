//! Knowledge through the actions: documents in, passages out, who reads
//! them, and (when a test database is set) persistence and other processes
//! on PostgreSQL.

mod common;

use serde_json::{Value, json};

use onto_intelli::{Config, Hub, Principal, Role};

const GUIDE: &str = "# Runbook\n\n## Payment API\n\nWhen captures time out, fail over to the secondary \
     acquirer and page the payments on-call.\n\n## Search\n\nReindex nightly; a stale index shows old prices.\n";

async fn hub() -> std::sync::Arc<Hub> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    Hub::open(cfg).await.unwrap()
}

#[tokio::test]
async fn documents_go_in_and_passages_come_out_through_the_actions() {
    let hub = hub().await;
    let kb = hub.tenant("default").await.unwrap();
    let ann = Principal::member("ann", "acme");
    let r = kb
        .call(&ann, "knowledge.ingest", json!({ "name": "Runbook.md", "text": GUIDE, "collection": "ops" }))
        .await
        .unwrap();
    assert_eq!((r["created"].as_bool(), r["document"]["passages"].as_u64()), (Some(true), Some(2)), "{r}");
    let id = r["document"]["id"].as_str().unwrap().to_string();
    let hits = kb.call(&ann, "knowledge.search", json!({ "query": "captures time out acquirer" })).await.unwrap();
    let top = &hits["passages"][0];
    assert_eq!(top["section_path"], "Runbook › Payment API", "{hits}");
    assert_eq!(top["citation"], "Runbook.md › Runbook › Payment API");
    assert_eq!(top["document_id"], id.as_str());
    // Documents never become entities.
    let found = kb.call(&ann, "entity.search", json!({ "query": "acquirer" })).await.unwrap();
    assert_eq!(found["hits"], json!([]), "{found}");
    // A document read back, with its passages, in order.
    let got = kb.call(&ann, "knowledge.get", json!({ "id": id, "passages": true })).await.unwrap();
    let ordinals: Vec<u64> =
        got["passages"].as_array().unwrap().iter().map(|p| p["ordinal"].as_u64().unwrap()).collect();
    assert_eq!(ordinals, [0, 1]);
    assert!(got["passages"][0].get("embedding").is_none(), "embeddings are never returned");
    let listed = kb.call(&ann, "knowledge.list", json!({ "collection": "ops" })).await.unwrap();
    assert_eq!(listed["documents"].as_array().unwrap().len(), 1);
    let cs = kb.call(&ann, "knowledge.collections", json!({})).await.unwrap();
    assert_eq!(cs["collections"], json!([{ "name": "ops", "documents": 1, "passages": 2 }]));
    // Raising sensitivity is a member's; lowering it again is an admin's.
    let up = json!({ "id": id, "sensitivity": "confidential" });
    assert_eq!(kb.call(&ann, "knowledge.update", up).await.unwrap()["document"]["sensitivity"], "confidential");
    let down = json!({ "id": id, "sensitivity": "public" });
    assert!(kb.call(&ann, "knowledge.update", down).await.is_err());
    // Agents read up to internal only, and write no knowledge.
    let agent = Principal::new("agent:helper", vec![Role::Agent], vec!["acme".into()]);
    let seen =
        kb.call(&agent, "knowledge.search", json!({ "query": "acquirer", "max_sensitivity": "restricted" })).await;
    assert_eq!(seen.unwrap()["passages"], json!([]));
    let e = kb.call(&agent, "knowledge.ingest", json!({ "name": "x", "text": "y" })).await.unwrap_err();
    assert!(e.to_string().contains("not open to agents"), "{e}");
    let stats = kb.call(&ann, "knowledge.stats", json!({})).await.unwrap();
    assert_eq!((stats["documents"].as_u64(), stats["passages"].as_u64()), (Some(1), Some(2)));
    assert_eq!(kb.call(&ann, "knowledge.delete", json!({ "id": id })).await.unwrap()["deleted"], true);
}

#[tokio::test]
async fn an_import_keeps_its_ids_and_vectors_for_a_shadow_comparison() {
    let hub = hub().await;
    let kb = hub.tenant("default").await.unwrap();
    let ann = Principal::member("ann", "acme");
    // nexc-engine's chunks, as they are: ids, order, sections, embeddings.
    let input = json!({
        "id": "6f1c2b9e-0000-4000-8000-000000000001",
        "name": "Customs.pdf",
        "source": "nexc:6f1c2b9e-0000-4000-8000-000000000001",
        "sensitivity": "confidential",
        "embedding_model": "text-embedding-3-small",
        "passages": [
            { "id": "chunk-1", "content": "Bills of lading for Rotterdam arrivals", "page": 3,
              "section_path": "Imports", "embedding": [0.6, 0.8, 0.0] },
            { "id": "chunk-2", "content": "Holiday schedule for the warehouse team", "page": 9,
              "embedding": [0.0, 0.0, 1.0] },
        ],
    });
    let r = kb.call(&ann, "knowledge.ingest", input.clone()).await.unwrap();
    assert_eq!(r["document"]["embedding_model"], "text-embedding-3-small");
    // The same import again is a no-op.
    assert_eq!(kb.call(&ann, "knowledge.ingest", input).await.unwrap()["created"], false);
    let q = json!({
        "query": "shipping paperwork",
        "k": 2,
        "embedding": { "model": "text-embedding-3-small", "vector": [0.5, 0.85, 0.0] },
    });
    let hits = kb.call(&ann, "knowledge.search", q).await.unwrap();
    let ids: Vec<&str> =
        hits["passages"].as_array().unwrap().iter().map(|p| p["passage_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["chunk-1"], "{hits}");
    assert_eq!(hits["passages"][0]["citation"], "Customs.pdf, p. 3 › Imports");
    assert_eq!(hits["passages"][0]["sensitivity"], "confidential");
}

#[cfg(feature = "postgres")]
fn pg_config(url: &str, prefix: &str) -> Config {
    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.database_url = Some(url.into());
    cfg.engine.tenancy = "multi".into();
    cfg.engine.default_workspace = "acme".into();
    cfg.engine.postgres.schema_prefix = prefix.into();
    cfg.traces.enabled = false;
    cfg.connectors.background = false;
    cfg
}

#[cfg(feature = "postgres")]
async fn eventually(what: &str, mut f: impl AsyncFnMut() -> bool) {
    let start = std::time::Instant::now();
    while !f().await {
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "{what}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn knowledge_persists_and_reaches_other_processes_on_postgres() {
    let Some(url) = common::database_url("knowledge_persists_and_reaches_other_processes_on_postgres") else { return };
    let prefix = common::unique("okb_");
    let (a, b) =
        (Hub::open(pg_config(&url, &prefix)).await.unwrap(), Hub::open(pg_config(&url, &prefix)).await.unwrap());
    let (ka, kb) = (a.tenant("kb").await.unwrap(), b.tenant("kb").await.unwrap());
    let ann = Principal::member("ann", "acme");
    let search = |q: &'static str| json!({ "query": q });
    let doc = ka.call(&ann, "knowledge.ingest", json!({ "name": "Runbook.md", "text": GUIDE })).await.unwrap();
    let id = doc["document"]["id"].as_str().unwrap().to_string();
    let found = |v: &Value| v["passages"].as_array().is_some_and(|p| !p.is_empty());
    // The other process hears about every row.
    eventually("the other process sees the passages", async || {
        found(&kb.call(&ann, "knowledge.search", search("acquirer")).await.unwrap())
    })
    .await;
    // A third process opening the tenant loads them.
    let c = Hub::open(pg_config(&url, &prefix)).await.unwrap();
    let kc = c.tenant("kb").await.unwrap();
    assert!(found(&kc.call(&ann, "knowledge.search", search("reindex nightly")).await.unwrap()));
    ka.call(&ann, "knowledge.delete", json!({ "id": id })).await.unwrap();
    eventually("the other process drops the document", async || {
        !found(&kb.call(&ann, "knowledge.search", search("acquirer")).await.unwrap())
    })
    .await;
    for t in a.known_tenants().unwrap_or_default() {
        common::drop_tenant(&url, &prefix, &t);
    }
}
