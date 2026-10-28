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

/// A database name no other test in this run shares: the whole id, since
/// its first digits are a millisecond clock that tests started together share.
fn unique() -> String {
    format!("ct_{}", &onto_intelli::core::time::new_id("x")[2..])
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
type SqlServer = tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>;

/// A SQL Server session on `database`, as the test's admin.
#[cfg(feature = "sqlserver")]
async fn sql_server(tpl: &str, database: &str) -> SqlServer {
    use tokio_util::compat::TokioAsyncWriteCompatExt;
    let t = onto_intelli::connectors::restricted::parse(
        "sqlserver",
        &tpl.replace("{db}", "master"),
        &["sqlserver", "mssql"],
        1433,
        true,
    )
    .unwrap();
    let mut config = tiberius::Config::new();
    config.host(&t.host);
    config.port(t.port);
    config.authentication(tiberius::AuthMethod::sql_server(&t.user, &t.password));
    config.database(database);
    config.trust_cert();
    let tcp = tokio::net::TcpStream::connect(format!("{}:{}", t.host, t.port)).await.unwrap();
    tiberius::Client::connect(config, tcp.compat_write()).await.unwrap()
}

#[cfg(feature = "sqlserver")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql_server_metadata_feeds_the_builder() {
    let Some(tpl) = template("ONTO_TEST_SQLSERVER_URL") else { return };
    let db = unique();
    let mut m = sql_server(&tpl, "master").await;
    m.simple_query(format!("CREATE DATABASE [{db}]")).await.unwrap().into_results().await.unwrap();
    let mut c = sql_server(&tpl, &db).await;
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

#[cfg(feature = "sqlserver")]
async fn exec(c: &mut SqlServer, stmt: &str) {
    c.simple_query(stmt).await.unwrap().into_results().await.unwrap();
}

/// The grants SQL against a real server, with `map_grants`, crawled by an
/// account with no more than `db_datareader`: without VIEW DEFINITION it
/// cannot see the DENYs, so nothing is mapped open and the crawl says why.
/// With it (and as `sa`), a column grant is not a table grant, and a DENY
/// (of SELECT or CONTROL) wins through roles and at schema and database
/// scope, so the table is no longer public.
#[cfg(feature = "sqlserver")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql_server_grants_and_denies_map_to_readers() {
    let Some(tpl) = template("ONTO_TEST_SQLSERVER_URL") else { return };
    let db = unique();
    let (login, password) = (format!("crawler_{db}"), "Crawl3r-pw-for-tests");
    let mut m = sql_server(&tpl, "master").await;
    exec(&mut m, &format!("CREATE DATABASE [{db}]")).await;
    exec(&mut m, &format!("CREATE LOGIN [{login}] WITH PASSWORD = '{password}', CHECK_POLICY = OFF")).await;
    let mut c = sql_server(&tpl, &db).await;
    for stmt in [
        "CREATE TABLE dbo.open_t (id INT PRIMARY KEY)",
        "CREATE TABLE dbo.patients (id INT PRIMARY KEY, name NVARCHAR(20))",
        "CREATE TABLE dbo.orders (id INT PRIMARY KEY, total INT)",
        "CREATE TABLE dbo.ctl (id INT PRIMARY KEY)",
        "CREATE SCHEMA hr",
        "CREATE TABLE hr.salaries (id INT PRIMARY KEY)",
        "CREATE ROLE contractors",
        "CREATE ROLE staff",
        "CREATE ROLE analysts",
        "CREATE USER eve WITHOUT LOGIN",
        "CREATE USER bob WITHOUT LOGIN",
        "ALTER ROLE contractors ADD MEMBER eve",
        "ALTER ROLE staff ADD MEMBER eve",
        "ALTER ROLE staff ADD MEMBER bob",
        "GRANT SELECT ON dbo.open_t TO public",
        "GRANT SELECT (id) ON dbo.patients TO public",
        "GRANT SELECT ON dbo.orders TO public",
        "GRANT SELECT ON dbo.orders TO analysts",
        "GRANT SELECT ON dbo.orders TO staff",
        "GRANT SELECT ON dbo.orders TO bob",
        "DENY SELECT (total) ON dbo.orders TO contractors",
        "GRANT SELECT ON dbo.ctl TO public",
        "GRANT SELECT ON dbo.ctl TO analysts",
        "DENY CONTROL ON OBJECT::dbo.ctl TO bob",
        "GRANT SELECT ON hr.salaries TO public",
        "GRANT SELECT ON hr.salaries TO analysts",
        "DENY SELECT ON SCHEMA::hr TO staff",
    ] {
        exec(&mut c, stmt).await;
    }
    exec(&mut c, &format!("CREATE USER crawler FOR LOGIN [{login}]")).await;
    exec(&mut c, "ALTER ROLE db_datareader ADD MEMBER crawler").await;
    let admin_url = tpl.replace("{db}", &db);
    let t = onto_intelli::connectors::restricted::parse("sqlserver", &admin_url, &["sqlserver", "mssql"], 1433, true)
        .unwrap();
    let crawler_url =
        format!("sqlserver://{login}:{password}@{}:{}/{db}?trust_server_certificate=true", t.host, t.port);
    let spec = |url: &str| -> ConnectorSpec {
        serde_json::from_value(
            json!({ "name": "shop", "kind": "sqlserver", "url": url, "schemas": ["dbo", "hr"], "map_grants": true }),
        )
        .unwrap()
    };
    let conn = onto_intelli::connectors::sqlserver::SqlServerConnector::new(true);
    let readers = |snap: &onto_intelli::core::catalog::CatalogSnapshot, schema: &str, table: &str| {
        let r = snap.relations.iter().find(|r| r.schema == schema && r.name == table).unwrap();
        (r.public_read, r.readers.clone())
    };
    let hidden = |snap: &onto_intelli::core::catalog::CatalogSnapshot| {
        snap.notes.iter().any(|n| n.contains("lacks VIEW DEFINITION"))
    };
    // The crawler sees GRANT ... TO public but none of the DENYs.
    let snap = conn.snapshot(&spec(&crawler_url)).await.unwrap();
    for (schema, table) in
        [("dbo", "open_t"), ("dbo", "patients"), ("dbo", "orders"), ("dbo", "ctl"), ("hr", "salaries")]
    {
        assert_eq!(readers(&snap, schema, table), (false, vec![]), "{schema}.{table}: restricted, no readers");
    }
    assert!(hidden(&snap), "{:?}", snap.notes);
    exec(&mut c, "GRANT VIEW DEFINITION TO crawler").await;
    for url in [&crawler_url, &admin_url] {
        let snap = conn.snapshot(&spec(url)).await.unwrap();
        assert!(!hidden(&snap), "{:?}", snap.notes);
        assert_eq!(readers(&snap, "dbo", "open_t"), (true, vec![]));
        assert_eq!(readers(&snap, "dbo", "patients"), (false, vec![]), "a column grant is not the table");
        // eve (a contractor) is denied a column, so public and staff (which holds eve) are no readers.
        assert_eq!(readers(&snap, "dbo", "orders"), (false, vec!["analysts".to_string(), "bob".to_string()]));
        assert_eq!(readers(&snap, "dbo", "ctl"), (false, vec!["analysts".to_string()]), "DENY CONTROL");
        assert_eq!(readers(&snap, "hr", "salaries"), (false, vec!["analysts".to_string()]), "DENY on the schema");
    }
    // DENY SELECT TO bob: the database, every table.
    exec(&mut c, "DENY SELECT TO bob").await;
    let snap = conn.snapshot(&spec(&crawler_url)).await.unwrap();
    assert_eq!(readers(&snap, "dbo", "open_t"), (false, vec![]));
    assert_eq!(readers(&snap, "dbo", "orders"), (false, vec!["analysts".to_string()]));
    conn.close().await;
    drop(c);
    exec(&mut m, &format!("ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]")).await;
    exec(&mut m, &format!("DROP LOGIN [{login}]")).await;
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
