//! Crawling a real PostgreSQL database: metadata, grants as ACLs, learned
//! joins, OntoRank, incremental crawls, and the catalog questions. Runs when
//! a test database is set (see `common`); the crawled schema, the role and
//! the tenant are created for the test and dropped afterwards.
#![cfg(feature = "postgres")]

mod common;

use serde_json::json;

use onto_intelli::core::sqlx::{self, AssertSqlSafe};
use onto_intelli::{Config, Intelli, Principal};

/// Drops the crawled schema, the role and the tenant even when the test fails.
struct Cleanup {
    url: String,
    src: String,
    role: String,
    tenant: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let (url, src, role, tenant) = (self.url.clone(), self.src.clone(), self.role.clone(), self.tenant.clone());
        let _ = std::thread::spawn(move || {
            common::drop_tenant(&url, "onto_", &tenant);
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                if let Ok(pool) = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&url).await {
                    for s in [format!("DROP SCHEMA IF EXISTS {src} CASCADE"), format!("DROP ROLE IF EXISTS {role}")] {
                        let _ = sqlx::raw_sql(AssertSqlSafe(s)).execute(&pool).await;
                    }
                    pool.close().await;
                }
            });
        })
        .join();
    }
}

async fn exec(url: &str, statements: &[String]) {
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(url).await.unwrap();
    for s in statements {
        sqlx::raw_sql(AssertSqlSafe(s.clone())).execute(&pool).await.unwrap_or_else(|e| panic!("{s}: {e}"));
    }
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crawls_metadata_grants_joins_and_answers() {
    let Some(url) = common::database_url("catalog_crawl") else { return };
    let src = common::unique("crawl_");
    let role = common::unique("onto_test_analyst_");
    let tenant = common::unique("cat");
    let _cleanup = Cleanup { url: url.clone(), src: src.clone(), role: role.clone(), tenant: tenant.clone() };
    exec(
        &url,
        &[
            format!("CREATE ROLE {role} NOLOGIN"),
            format!("CREATE SCHEMA {src}"),
            format!("COMMENT ON SCHEMA {src} IS 'Shop data'"),
            format!("CREATE TABLE {src}.customers (id bigint PRIMARY KEY, cust_name text, region text)"),
            format!("COMMENT ON TABLE {src}.customers IS 'One row per customer'"),
            format!("COMMENT ON COLUMN {src}.customers.id IS 'Customer id'"),
            format!(
                "CREATE TABLE {src}.orders (id bigint PRIMARY KEY, customer_id bigint REFERENCES {src}.customers(id), \
                 amount numeric(12,2), status text)"
            ),
            format!("COMMENT ON TABLE {src}.orders IS 'One row per order'"),
            format!("COMMENT ON COLUMN {src}.orders.amount IS 'Order total, also known as GMV'"),
            format!(
                "CREATE VIEW {src}.revenue_daily AS SELECT o.status, sum(o.amount) AS revenue FROM {src}.orders o GROUP BY 1"
            ),
            format!("COMMENT ON VIEW {src}.revenue_daily IS 'Revenue by order status'"),
            format!("GRANT SELECT ON {src}.customers TO PUBLIC"),
            format!("GRANT SELECT ON {src}.orders TO {role}"),
            format!("INSERT INTO {src}.customers VALUES (1, 'Ada', 'EU')"),
            format!("INSERT INTO {src}.orders VALUES (1, 1, 10.5, 'paid')"),
        ],
    )
    .await;
    // Query usage, when pg_stat_statements is available here.
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    let pss = sqlx::raw_sql(AssertSqlSafe("CREATE EXTENSION IF NOT EXISTS pg_stat_statements".to_string()))
        .execute(&pool)
        .await
        .is_ok()
        && sqlx::raw_sql(AssertSqlSafe("SELECT count(*) FROM pg_stat_statements".to_string()))
            .execute(&pool)
            .await
            .is_ok();
    for _ in 0..3 {
        let q = format!(
            "SELECT c.region, sum(o.amount) AS revenue FROM {src}.orders o JOIN {src}.customers c \
             ON o.customer_id = c.id WHERE o.status = 'paid' GROUP BY c.region"
        );
        sqlx::raw_sql(AssertSqlSafe(q)).execute(&pool).await.unwrap();
    }
    pool.close().await;

    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.database_url = Some(url.clone());
    cfg.engine.postgres.tenant = tenant.clone();
    cfg.engine.default_workspace = "acme".into();
    cfg.connectors.background = false;
    let o = Intelli::open(cfg).await.unwrap();
    let admin = Principal::admin("ops");
    o.call(&admin, "entity.upsert", json!({ "id": "team:analytics", "title": "Analytics" })).await.unwrap();
    o.call(
        &admin,
        "entity.upsert",
        json!({ "id": "person:ana", "title": "Ana", "links": [{ "rel": "member_of", "to": "team:analytics" }] }),
    )
    .await
    .unwrap();

    // Only admins add connectors.
    let member = Principal::named("user:bo", &["member"], &["*"]).unwrap().with_entity("person:bo");
    let spec = json!({
        "name": "src", "url": url, "schemas": [src], "map_grants": true,
        "grant_map": { role.clone(): "team:analytics" },
    });
    assert!(o.call(&member, "connector.add", spec.clone()).await.is_err());
    let saved = o.call(&admin, "connector.add", spec).await.unwrap();
    assert!(saved["url"].as_str().unwrap().contains("***"), "password masked: {saved}");

    let report = o.call(&admin, "connector.crawl", json!({ "name": "src" })).await.unwrap();
    let c = &report["crawls"][0];
    assert_eq!(c["tables"], 2, "{c}");
    assert_eq!(c["views"], 1);
    assert_eq!(c["foreign_keys"], 1);
    assert!(c["upserted"].as_u64().unwrap() >= 11, "{c}");

    let orders = format!("table:src.{src}.orders");
    let customers = format!("table:src.{src}.customers");
    let view = format!("view:src.{src}.revenue_daily");
    let e = o.call(&admin, "entity.get", json!({ "id": orders })).await.unwrap();
    assert_eq!(e["props"]["description"], "One row per order");
    assert_eq!(e["props"]["visibility"], "restricted");
    // The analytics team through the grant; the owning role reads it too.
    let acl = e["props"]["acl"].as_array().unwrap();
    assert!(acl.contains(&json!("team:analytics")), "{acl:?}");
    assert_eq!(e["provenance"]["connector"], "src");
    assert_eq!(e["provenance"]["source"], "connector:postgres");
    let c_ent = o.call(&admin, "entity.get", json!({ "id": customers })).await.unwrap();
    assert_eq!(c_ent["props"]["visibility"], "internal", "PUBLIC can read customers");
    let reads = o.call(&admin, "graph.neighbors", json!({ "id": view, "rel": "reads_from" })).await.unwrap();
    assert!(reads.to_string().contains(&orders), "the view reads orders: {reads}");

    // Grants become ACLs: only the analytics team sees orders and its columns.
    assert!(o.call(&member, "entity.get", json!({ "id": orders })).await.is_err());
    let ana = Principal::named("user:ana", &["member"], &["*"]).unwrap().with_entity("person:ana");
    assert!(o.call(&ana, "entity.get", json!({ "id": orders })).await.is_ok());
    let col = format!("column:src.{src}.orders.amount");
    assert!(o.call(&member, "entity.get", json!({ "id": col })).await.is_err());

    // The learner found the join, and the answer comes from the graph.
    let joins = o.call(&admin, "learned.list", json!({ "fact_type": "join_path", "about": orders })).await.unwrap();
    let j = &joins["facts"][0];
    assert!(j["expression"].as_str().unwrap().contains("customer_id"), "{joins}");
    let ask = o.call(&admin, "intelli.ask", json!({ "question": "How do orders join to customers?" })).await.unwrap();
    assert_eq!(ask["mode"], "deterministic", "{ask}");
    assert!(ask["answer"].as_str().unwrap().contains("orders.customer_id = customers.id"), "{ask}");
    let syn = o.call(&admin, "learned.list", json!({ "fact_type": "synonym" })).await.unwrap().to_string();
    assert!(syn.contains("gmv") && syn.contains("cust"), "{syn}");
    if pss {
        // Query usage: the statement became a query entity, and the join it
        // uses backs the learned fact next to the foreign key.
        let q = o.call(&admin, "entity.find", json!({ "kind": "query" })).await.unwrap();
        assert!(q["total"].as_u64().unwrap_or(0) >= 1, "{q}");
        let fact = o.call(&admin, "entity.get", json!({ "id": j["id"] })).await.unwrap();
        let evidence = fact["props"]["evidence"].to_string();
        assert!(evidence.contains("query:src.") && evidence.contains("constraint"), "{evidence}");
    } else {
        eprintln!("pg_stat_statements unavailable here: query usage not checked");
    }

    // OntoRank explains itself.
    let r = o.call(&admin, "catalog.rank", json!({ "id": customers })).await.unwrap();
    for k in ["centrality", "usage", "freshness", "owner", "documentation", "certification"] {
        assert!(r["components"][k].is_number(), "{k}: {r}");
    }
    assert!(r["components"]["documentation"].as_f64().unwrap() > 0.5, "described, one column documented: {r}");

    // Incremental: nothing changed, nothing written.
    let again = o.call(&admin, "connector.crawl", json!({ "name": "src" })).await.unwrap();
    assert_eq!(again["crawls"][0]["upserted"], 0, "{again}");
    exec(
        &url,
        &[
            format!("COMMENT ON TABLE {src}.customers IS 'Customers, deduplicated'"),
            format!("ALTER TABLE {src}.customers DROP COLUMN region"),
        ],
    )
    .await;
    let third = o.call(&admin, "connector.crawl", json!({ "name": "src" })).await.unwrap();
    let t = &third["crawls"][0];
    assert_eq!(t["deleted"], 1, "the dropped column: {t}");
    assert!(t["upserted"].as_u64().unwrap() >= 1, "{t}");
    let c_ent = o.call(&admin, "entity.get", json!({ "id": customers })).await.unwrap();
    assert_eq!(c_ent["props"]["description"], "Customers, deduplicated");

    let listed = o.call(&admin, "connector.list", json!({})).await.unwrap();
    assert_eq!(listed["connectors"][0]["crawls"], 3);
    let removed = o.call(&admin, "connector.remove", json!({ "name": "src", "purge": true })).await.unwrap();
    assert!(removed["deleted"].as_u64().unwrap() >= 10, "{removed}");
    assert!(o.call(&admin, "entity.get", json!({ "id": orders })).await.is_err());

    drop(o);
}
