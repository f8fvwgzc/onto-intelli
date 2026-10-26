//! The MySQL/MariaDB, SQL Server and MongoDB connectors against real
//! servers: a crawl reads their metadata into the catalog, the ontology
//! builder drafts object and link types from it, and rows page in key order.
//! Each runs when its URL is set, with `{db}` where the database goes, and is
//! skipped otherwise (CI starts the servers as service containers):
//!
//! * `ONTO_TEST_MYSQL_URL`, `ONTO_TEST_MARIADB_URL`: `mysql://root:pw@127.0.0.1:3306/{db}`
//! * `ONTO_TEST_SQLSERVER_URL`: `sqlserver://sa:pw@127.0.0.1:1433/{db}?trust_server_certificate=true`
//! * `ONTO_TEST_MONGODB_URL`: `mongodb://127.0.0.1:27017/{db}`

use std::sync::Arc;

use serde_json::{Value, json};

use onto_intelli::connectors::{Connector, ConnectorSpec, RowRequest};
use onto_intelli::core::modeling::DraftOptions;
use onto_intelli::{Intelli, Principal};

fn template(var: &str) -> Option<String> {
    let v = std::env::var(var).ok().filter(|v| !v.trim().is_empty());
    if v.is_none() {
        eprintln!("skipped: set {var} to run this test");
    }
    v
}

fn unique() -> String {
    format!("ct_{}", &onto_intelli::core::time::new_id("x")[2..10])
}

/// Crawl `url` as connector `shop` and check what every engine must give.
async fn crawl(kind: &str, url: &str, schema: &str) -> Arc<Intelli> {
    let o = Intelli::in_memory().await.unwrap();
    let admin = Principal::admin("ops");
    o.call(
        &admin,
        "connector.add",
        json!({ "name": "shop", "kind": kind, "url": url, "schemas": [schema], "map_grants": false }),
    )
    .await
    .unwrap();
    let report = o.call(&admin, "connector.crawl", json!({ "name": "shop" })).await.unwrap();
    let c = &report["crawls"][0];
    assert_eq!(c["tables"], 2, "{kind}: {c}");
    assert_eq!(c["foreign_keys"], 1, "{kind}: {c}");
    o
}

fn entity(o: &Intelli, id: &str) -> Value {
    o.ontology()
        .get(&Principal::admin("ops"), id)
        .map(|e| serde_json::to_value(e).unwrap())
        .unwrap_or_else(|e| panic!("{id}: {e}"))
}

/// The builder models customers and orders, and the order → customer link.
fn drafts_orders_and_customers(o: &Intelli, states: &[&str]) {
    let draft = o.ontology().draft_ontology(&Principal::admin("ops"), &DraftOptions::for_source("shop")).unwrap();
    let order = draft.object("order").unwrap_or_else(|| panic!("{draft:?}"));
    assert!(draft.object("customer").is_some(), "{draft:?}");
    if !states.is_empty() {
        assert_eq!(order.states, states, "{order:?}");
    }
    assert!(draft.links.iter().any(|l| l.to == "customer"), "{:?}", draft.links);
    let plan = o.ontology().schema_plan(&Principal::admin("ops"), &draft.changes).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
}

async fn pages(conn: &dyn Connector, spec: &ConnectorSpec, table: &str, key: &str) -> Vec<Value> {
    let mut req = RowRequest {
        table: table.into(),
        key: vec![key.into()],
        columns: vec!["status".into()],
        after: None,
        limit: 2,
    };
    let mut ids = Vec::new();
    loop {
        let page = conn.rows(spec, &req).await.unwrap();
        if page.rows.is_empty() {
            break;
        }
        assert!(page.rows.len() <= 2);
        for r in &page.rows {
            assert!(r.contains_key("status") && r.len() == 2, "only the key and the asked columns: {r:?}");
            ids.push(r[key].clone());
        }
        req.after = page.last_key;
    }
    conn.close().await;
    ids
}

fn spec(kind: &str, url: &str) -> ConnectorSpec {
    serde_json::from_value(json!({ "name": "shop", "kind": kind, "url": url })).unwrap()
}

#[cfg(feature = "mysql")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mysql_and_mariadb_metadata_feed_the_builder() {
    use onto_intelli::core::sqlx;
    for var in ["ONTO_TEST_MYSQL_URL", "ONTO_TEST_MARIADB_URL"] {
        let Some(tpl) = template(var) else { continue };
        let db = unique();
        let admin_pool =
            sqlx_mysql::MySqlPoolOptions::new().max_connections(1).connect(&tpl.replace("{db}", "")).await.unwrap();
        for stmt in [
            format!("CREATE DATABASE `{db}`"),
            format!(
                "CREATE TABLE `{db}`.customers (id INT PRIMARY KEY, name VARCHAR(80) NOT NULL, \
                 email VARCHAR(120) UNIQUE, tier ENUM('free','pro') NOT NULL) COMMENT='People who buy'"
            ),
            format!(
                "CREATE TABLE `{db}`.orders (id INT PRIMARY KEY, customer_id INT NOT NULL, \
                 status ENUM('placed','paid','refunded') NOT NULL, total DECIMAL(10,2), \
                 FOREIGN KEY (customer_id) REFERENCES `{db}`.customers(id)) COMMENT='One row per order'"
            ),
            format!("CREATE VIEW `{db}`.paid AS SELECT id, total FROM `{db}`.orders WHERE status = 'paid'"),
            format!(
                "INSERT INTO `{db}`.customers VALUES (1, 'Ann', 'ann@example.com', 'pro'), (2, 'Bo', NULL, 'free')"
            ),
            format!(
                "INSERT INTO `{db}`.orders VALUES (10, 1, 'paid', 5.00), (11, 1, 'placed', 7.50), \
                 (12, 2, 'refunded', 1.25), (13, 2, 'paid', 9.99), (14, 1, 'paid', 3.00)"
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(stmt)).execute(&admin_pool).await.unwrap();
        }
        let url = tpl.replace("{db}", &db);
        let o = crawl("mysql", &url, &db).await;
        let orders = entity(&o, &format!("table:shop.{db}.orders"));
        assert_eq!(orders["props"]["description"], "One row per order", "{var}");
        let status = entity(&o, &format!("column:shop.{db}.orders.status"));
        assert_eq!(status["props"]["enum_values"], json!(["placed", "paid", "refunded"]), "{var}");
        assert_eq!(entity(&o, &format!("column:shop.{db}.customers.email"))["props"]["unique"], true);
        assert!(o.ontology().get(&Principal::admin("ops"), &format!("view:shop.{db}.paid")).is_ok(), "{var}");
        drafts_orders_and_customers(&o, &["placed", "paid", "refunded"]);
        let conn = onto_intelli::connectors::mysql::MySqlConnector::new(true);
        let ids = pages(&conn, &spec("mysql", &url), &format!("{db}.orders"), "id").await;
        assert_eq!(ids, [10, 11, 12, 13, 14].map(Value::from), "{var}");
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{db}`"))).execute(&admin_pool).await.unwrap();
    }
}

#[cfg(feature = "sqlserver")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql_server_metadata_feeds_the_builder() {
    use tokio_util::compat::TokioAsyncWriteCompatExt;
    let Some(tpl) = template("ONTO_TEST_SQLSERVER_URL") else { return };
    let master = onto_intelli::connectors::restricted::parse(
        "sqlserver",
        &tpl.replace("{db}", "master"),
        &["sqlserver", "mssql"],
        1433,
        true,
    )
    .unwrap();
    let connect = |database: String| {
        let t = master.clone();
        async move {
            let mut config = tiberius::Config::new();
            config.host(&t.host);
            config.port(t.port);
            config.authentication(tiberius::AuthMethod::sql_server(&t.user, &t.password));
            config.database(database);
            config.trust_cert();
            let tcp = tokio::net::TcpStream::connect(format!("{}:{}", t.host, t.port)).await.unwrap();
            tiberius::Client::connect(config, tcp.compat_write()).await.unwrap()
        }
    };
    let db = unique();
    let mut m = connect("master".into()).await;
    m.simple_query(format!("CREATE DATABASE [{db}]")).await.unwrap().into_results().await.unwrap();
    let mut c = connect(db.clone()).await;
    for stmt in [
        "CREATE TABLE dbo.customers (id INT PRIMARY KEY, name NVARCHAR(80) NOT NULL, email NVARCHAR(120) NULL UNIQUE)",
        "CREATE TABLE dbo.orders (id INT PRIMARY KEY, customer_id INT NOT NULL REFERENCES dbo.customers(id), \
         status NVARCHAR(20) NOT NULL CHECK (status IN ('placed', 'paid', 'refunded')), total DECIMAL(10,2))",
        "CREATE VIEW dbo.paid AS SELECT id, total FROM dbo.orders WHERE status = 'paid'",
        "EXEC sp_addextendedproperty 'MS_Description', 'One row per order', 'SCHEMA', 'dbo', 'TABLE', 'orders'",
        "INSERT INTO dbo.customers VALUES (1, 'Ann', 'ann@example.com'), (2, 'Bo', NULL)",
        "INSERT INTO dbo.orders VALUES (10, 1, 'paid', 5.00), (11, 1, 'placed', 7.50), (12, 2, 'refunded', 1.25), \
         (13, 2, 'paid', 9.99), (14, 1, 'paid', 3.00)",
    ] {
        c.simple_query(stmt).await.unwrap().into_results().await.unwrap();
    }
    drop(c);
    let url = tpl.replace("{db}", &db);
    let o = crawl("sqlserver", &url, "dbo").await;
    assert_eq!(entity(&o, "table:shop.dbo.orders")["props"]["description"], "One row per order");
    let status = entity(&o, "column:shop.dbo.orders.status");
    assert_eq!(status["props"]["enum_values"], json!(["paid", "placed", "refunded"]), "{status}");
    assert_eq!(entity(&o, "column:shop.dbo.customers.email")["props"]["unique"], true);
    assert!(o.ontology().get(&Principal::admin("ops"), "view:shop.dbo.paid").is_ok());
    drafts_orders_and_customers(&o, &[]);
    let conn = onto_intelli::connectors::sqlserver::SqlServerConnector::new(true);
    let ids = pages(&conn, &spec("sqlserver", &url), "dbo.orders", "id").await;
    assert_eq!(ids, [10, 11, 12, 13, 14].map(Value::from));
    m.simple_query(format!("ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]"))
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
}

#[cfg(feature = "mongodb")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mongodb_samples_feed_the_builder() {
    use mongodb::bson::{doc, oid::ObjectId};
    let Some(tpl) = template("ONTO_TEST_MONGODB_URL") else { return };
    let db_name = unique();
    let client = mongodb::Client::with_uri_str(tpl.replace("{db}", "")).await.unwrap();
    let db = client.database(&db_name);
    let (ann, bo) = (ObjectId::new(), ObjectId::new());
    db.collection("customers")
        .insert_many([
            doc! { "_id": ann, "name": "Ann", "email": "ann@example.com", "address": { "city": "Oslo" } },
            doc! { "_id": bo, "name": "Bo", "email": "bo@example.com" },
        ])
        .await
        .unwrap();
    db.collection::<mongodb::bson::Document>("customers")
        .create_index(
            mongodb::IndexModel::builder()
                .keys(doc! { "email": 1 })
                .options(mongodb::options::IndexOptions::builder().unique(true).build())
                .build(),
        )
        .await
        .unwrap();
    let orders: Vec<_> =
        [(10, ann, "paid"), (11, ann, "placed"), (12, bo, "refunded"), (13, bo, "paid"), (14, ann, "paid")]
            .into_iter()
            .map(|(id, c, s)| doc! { "_id": id, "customer_id": c, "status": s, "total": 5.0 })
            .collect();
    db.collection("orders").insert_many(orders).await.unwrap();
    let url = tpl.replace("{db}", &db_name);
    let o = crawl("mongodb", &url, &db_name).await;
    let id = entity(&o, &format!("column:shop.{db_name}.orders._id"));
    assert_eq!(id["props"]["primary_key"], true, "{id}");
    assert_eq!(entity(&o, &format!("column:shop.{db_name}.customers.email"))["props"]["unique"], true);
    assert!(
        o.ontology().get(&Principal::admin("ops"), &format!("column:shop.{db_name}.customers.address.city")).is_ok()
    );
    drafts_orders_and_customers(&o, &[]);
    let conn = onto_intelli::connectors::mongo::MongoConnector::new(true);
    let ids = pages(&conn, &spec("mongodb", &url), &format!("{db_name}.orders"), "_id").await;
    assert_eq!(ids, [10, 11, 12, 13, 14].map(Value::from));
    db.drop().await.unwrap();
}
