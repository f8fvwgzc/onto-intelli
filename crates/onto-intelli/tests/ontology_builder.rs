//! From a real PostgreSQL database to a governed ontology with objects:
//! crawl, draft object and link types from the catalog, apply the draft
//! (which crawls again to sync rows into typed objects and links), explore
//! them, and keep them in step incrementally, over several pages of rows. Runs when a test database is set (see
//! `common`); the source schema, the role and the tenant are created for
//! the test and dropped afterwards.
#![cfg(feature = "postgres")]

mod common;

use serde_json::{Value, json};

use onto_intelli::core::Dir;
use onto_intelli::core::explorer::{ExploreSpec, Filter, FilterOp};
use onto_intelli::core::modeling::{DraftOptions, LinkVia};
use onto_intelli::core::sqlx::{self, AssertSqlSafe};
use onto_intelli::{Config, Intelli, Principal};

/// Drops the source schema, the role and the tenant even when the test fails.
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

fn explore(
    o: &Intelli,
    p: &Principal,
    kind: &str,
    filters: Vec<Filter>,
) -> onto_intelli::core::explorer::ExploreResult {
    o.ontology()
        .explore(p, &ExploreSpec { kind: kind.into(), filters, limit: 50, ..Default::default() })
        .unwrap_or_else(|e| panic!("explore {kind}: {e}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drafts_applies_and_syncs_objects_from_postgres() {
    let Some(url) = common::database_url("ontology_builder") else { return };
    let src = common::unique("build_");
    let role = common::unique("onto_test_payroll_");
    let tenant = common::unique("obld");
    let _cleanup = Cleanup { url: url.clone(), src: src.clone(), role: role.clone(), tenant: tenant.clone() };
    exec(
        &url,
        &[
            format!("CREATE ROLE {role} NOLOGIN"),
            format!("CREATE SCHEMA {src}"),
            format!("CREATE TYPE {src}.order_status AS ENUM ('placed', 'paid', 'shipped', 'cancelled')"),
            format!(
                "CREATE TABLE {src}.customers (id bigint PRIMARY KEY, name text NOT NULL, email text UNIQUE, \
                 tier text CHECK (tier IN ('gold', 'silver')), signed_up timestamptz)"
            ),
            format!("COMMENT ON TABLE {src}.customers IS 'One row per customer account'"),
            format!(
                "CREATE TABLE {src}.orders (id bigint PRIMARY KEY, customer_id bigint REFERENCES {src}.customers(id), \
                 status {src}.order_status NOT NULL, amount numeric(12,2), placed_on date, gift boolean, notes text)"
            ),
            format!("CREATE TABLE {src}.tags (id bigint PRIMARY KEY, label text NOT NULL)"),
            format!(
                "CREATE TABLE {src}.order_tags (order_id bigint REFERENCES {src}.orders(id), \
                 tag_id bigint REFERENCES {src}.tags(id), PRIMARY KEY (order_id, tag_id))"
            ),
            format!("CREATE TABLE {src}.payroll (id bigint PRIMARY KEY, employee text NOT NULL, salary numeric(12,2))"),
            // More rows than one page, under a composite text and int key.
            format!(
                "CREATE TABLE {src}.readings (sensor text, seq int, value numeric(8,2), PRIMARY KEY (sensor, seq))"
            ),
            format!(
                "INSERT INTO {src}.readings SELECT (ARRAY['eu-1', 'US-2', 'apac'])[1 + i % 3], i, i / 10.0 \
                 FROM generate_series(1, 2500) i"
            ),
            format!(
                "GRANT SELECT ON {src}.customers, {src}.orders, {src}.tags, {src}.order_tags, {src}.readings TO PUBLIC"
            ),
            format!("GRANT SELECT ON {src}.payroll TO {role}"),
            format!(
                "INSERT INTO {src}.customers VALUES \
                 (1, 'Ada', 'ada@acme.test', 'gold', '2026-01-02T09:00:00Z'), \
                 (2, 'Bo', 'bo@acme.test', 'silver', NULL), (3, 'Cy', NULL, NULL, NULL)"
            ),
            format!(
                "INSERT INTO {src}.orders VALUES \
                 (1, 1, 'paid', 12.50, '2026-10-17', true, 'leave at the door'), \
                 (2, 1, 'placed', 40.00, '2026-10-18', false, NULL), \
                 (3, 2, 'shipped', 7.25, '2026-10-19', NULL, NULL), \
                 (4, NULL, 'cancelled', 0, NULL, NULL, NULL), \
                 (5, 3, 'paid', 100.00, '2026-10-20', false, NULL)"
            ),
            format!("INSERT INTO {src}.tags VALUES (1, 'priority'), (2, 'gift wrap')"),
            format!("INSERT INTO {src}.order_tags VALUES (1, 1), (1, 2), (2, 1)"),
            format!("INSERT INTO {src}.payroll VALUES (1, 'Ada', 5000), (2, 'Bo', 4200)"),
        ],
    )
    .await;

    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.database_url = Some(url.clone());
    cfg.engine.postgres.tenant = tenant.clone();
    cfg.engine.default_workspace = "acme".into();
    cfg.connectors.background = false;
    let o = Intelli::open(cfg).await.unwrap();
    let admin = Principal::admin("ops");
    o.call(&admin, "entity.upsert", json!({ "id": "team:finance", "title": "Finance" })).await.unwrap();
    o.call(
        &admin,
        "entity.upsert",
        json!({ "id": "person:fin", "title": "Fin", "links": [{ "rel": "member_of", "to": "team:finance" }] }),
    )
    .await
    .unwrap();
    let spec = json!({
        "name": "src", "url": url, "schemas": [src], "map_grants": true,
        "grant_map": { role.clone(): "team:finance" },
    });
    o.call(&admin, "connector.add", spec).await.unwrap();

    // Before anything is backed, a crawl reads metadata only.
    let first = o.call(&admin, "connector.crawl", json!({ "name": "src" })).await.unwrap();
    assert_eq!(first["crawls"][0]["objects"], json!({}), "{first}");

    // Draft from the catalog: typed properties, enum states, keys and links.
    let draft = o.ontology().draft_ontology(&admin, &DraftOptions::for_source("src")).unwrap();
    let order = draft.object("order").unwrap_or_else(|| panic!("{draft:?}"));
    assert_eq!(order.states, ["placed", "paid", "shipped", "cancelled"]);
    assert_eq!(order.key, ["id"]);
    let customer = draft.object("customer").unwrap_or_else(|| panic!("{draft:?}"));
    assert!(customer.properties.iter().any(|p| p.name == "tier" && p.values == ["gold", "silver"]), "{customer:?}");
    assert!(customer.properties.iter().any(|p| p.name == "email" && p.unique));
    assert!(draft.object("payroll").is_some());
    let fk = draft.links.iter().find(|l| l.from == "order" && l.to == "customer").unwrap();
    let join = draft.links.iter().find(|l| l.via == LinkVia::JoinTable).unwrap_or_else(|| panic!("{:?}", draft.links));
    assert!(draft.object("order_tag").is_none(), "the join table is a link type, not an object type");
    let (fk_rel, join_rel) = (fk.name.clone(), join.name.clone());
    let (join_from, join_to) = (join.from.clone(), join.to.clone());

    // Applied without a crawl, nothing is synced and the reply says how.
    let tags_only = o
        .call(&admin, "ontology.apply_draft", json!({ "source": "src", "tables": ["tags"], "no_crawl": true }))
        .await
        .unwrap();
    assert_eq!(tags_only["mode"], "applied", "{tags_only}");
    assert!(tags_only.get("crawl").is_none(), "{tags_only}");
    assert!(tags_only["hint"].as_str().unwrap().contains("onto crawl src"), "{tags_only}");
    assert_eq!(explore(&o, &admin, "tag", vec![]).total, 0);

    // An admin applies the whole draft, which crawls and syncs the rows.
    let applied = o.call(&admin, "ontology.apply_draft", json!({ "source": "src" })).await.unwrap();
    assert_eq!(applied["mode"], "applied", "{applied}");
    let c = &applied["crawl"][0];
    for (kind, rows) in [("customer", 3), ("order", 5), ("tag", 2), ("payroll", 2), ("reading", 2500)] {
        assert_eq!(c["objects"][kind]["rows"], rows, "{kind}: {c}");
        assert_eq!(c["objects"][kind]["upserted"], rows, "{kind}: {c}");
        assert_eq!(c["objects"][kind]["invalid"], 0, "{kind}: {c}");
        assert_eq!(c["objects"][kind]["truncated"], false, "{kind}: {c}");
    }
    assert_eq!(c["links"][&join_rel]["linked"], 3, "{c}");
    let schema = o.ontology().schema();
    let tag_types: Vec<&str> = schema
        .object_types
        .values()
        .filter(|t| t.backing.as_ref().is_some_and(|b| b.table == format!("{src}.tags")))
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(tag_types, ["tag"], "the second apply updates the type the first made");
    assert_eq!(explore(&o, &admin, "reading", vec![]).total, 2500);
    let readings = explore(&o, &admin, "reading", vec![Filter::new("value", FilterOp::Gte, json!(0))]);
    assert_eq!((readings.total, readings.stats["value"].max), (2500, 250.0));
    assert!(o.ontology().get(&admin, "reading:US-2.2497").is_ok(), "keys keep their case");

    let get = |id: &str| o.ontology().get(&admin, id).unwrap_or_else(|e| panic!("{id}: {e}"));
    let o1 = get("order:1");
    assert_eq!(o1.state.as_deref(), Some("paid"));
    assert_eq!(o1.props["amount"], json!(12.5));
    assert_eq!(o1.props["placed_on"], json!(1_792_195_200_000_i64));
    assert_eq!(o1.props["gift"], json!(true));
    assert_eq!(o1.props["notes"], "leave at the door");
    assert_eq!(o1.provenance.as_ref().unwrap().connector.as_deref(), Some("src"));
    assert_eq!(o1.provenance.as_ref().unwrap().source, "connector:postgres");
    let ada = get("customer:1");
    assert_eq!(ada.title, "Ada");
    assert_eq!(ada.props["tier"], "gold");
    assert_eq!(ada.props["signed_up"], json!(1_767_344_400_000_i64));
    assert_eq!(get("tag:2").title, "gift wrap");
    let out = |id: &str, rel: &str| {
        o.ontology().read(|g, _| g.neighbors(id, rel, Dir::Out).iter().map(|x| x.to_string()).collect::<Vec<_>>())
    };
    assert_eq!(out("order:1", &fk_rel), ["customer:1"]);
    assert!(out("order:4", &fk_rel).is_empty(), "a null foreign key links nothing");
    assert_eq!(join_from, "order");
    assert_eq!(join_to, "tag");
    let mut tags = out("order:1", &join_rel);
    tags.sort();
    assert_eq!(tags, ["tag:1", "tag:2"]);

    // The restricted table's objects are restricted like it.
    let pay = get("payroll:1");
    assert_eq!(pay.props["visibility"], "restricted", "{pay:?}");
    assert!(pay.props["acl"].as_array().unwrap().contains(&json!("team:finance")), "{pay:?}");
    assert_eq!(o1.props["visibility"], "internal");
    let bo = Principal::named("user:bo", &["member"], &["*"]).unwrap().with_entity("person:bo");
    let fin = Principal::named("user:fin", &["member"], &["*"]).unwrap().with_entity("person:fin");
    assert_eq!(explore(&o, &bo, "payroll", vec![]).total, 0);
    assert_eq!(explore(&o, &fin, "payroll", vec![]).total, 2);
    assert_eq!(explore(&o, &bo, "order", vec![]).total, 5);

    // Explore with filters, facets and statistics.
    let paid = explore(&o, &admin, "order", vec![Filter::new("state", FilterOp::Eq, json!("paid"))]);
    assert_eq!(paid.total, 2);
    assert_eq!(paid.stats["amount"].max, 100.0);
    let big = explore(&o, &admin, "order", vec![Filter::new("amount", FilterOp::Gte, json!(12.5))]);
    assert_eq!(big.total, 3);
    let ada_orders =
        explore(&o, &admin, "order", vec![Filter::new(format!("link:{fk_rel}"), FilterOp::Eq, json!("customer:1"))]);
    assert_eq!(ada_orders.total, 2);
    let early = explore(&o, &admin, "order", vec![Filter::new("placed_on", FilterOp::Lt, json!("2026-10-19"))]);
    assert_eq!(early.total, 2);
    let all = explore(&o, &admin, "order", vec![]);
    let states: Vec<(Value, usize)> = all.facets["state"].iter().map(|f| (f.value.clone(), f.count)).collect();
    assert_eq!(states[0], (json!("paid"), 2), "{states:?}");
    assert!(all.actions.iter().any(|a| a == "edit_order"), "{:?}", all.actions);
    let types = o.ontology().object_types(&admin, Some("acme")).unwrap();
    let t = types.iter().find(|t| t.name == "order").unwrap();
    assert_eq!(t.count, 5);
    assert_eq!(t.backing.as_ref().unwrap().table, format!("{src}.orders"));

    // Incremental: nothing changed, nothing written.
    let third = o.call(&admin, "connector.crawl", json!({ "name": "src" })).await.unwrap();
    let c = &third["crawls"][0];
    assert_eq!(c["objects"]["order"]["unchanged"], 5, "{c}");
    assert_eq!(c["objects"]["order"]["upserted"], 0, "{c}");
    assert_eq!(c["objects"]["reading"]["unchanged"], 2500, "every page read again, nothing written: {c}");
    assert_eq!(c["links"][&join_rel]["linked"], 0, "{c}");

    // One row changed, one changed only its status, one deleted: two
    // upserts, one delete, the rest untouched.
    exec(
        &url,
        &[
            format!("UPDATE {src}.orders SET amount = 99.99, customer_id = 2 WHERE id = 2"),
            format!("UPDATE {src}.orders SET status = 'shipped' WHERE id = 5"),
            format!("DELETE FROM {src}.orders WHERE id = 4"),
            format!("DELETE FROM {src}.order_tags WHERE order_id = 2"),
        ],
    )
    .await;
    let fourth = o.call(&admin, "connector.crawl", json!({ "name": "src" })).await.unwrap();
    let c = &fourth["crawls"][0];
    assert_eq!(c["objects"]["order"]["upserted"], 2, "{c}");
    assert_eq!(c["objects"]["order"]["deleted"], 1, "{c}");
    assert_eq!(c["objects"]["order"]["unchanged"], 2, "{c}");
    assert_eq!(get("order:5").state.as_deref(), Some("shipped"), "a status change alone is synced");
    let shipped = explore(&o, &admin, "order", vec![Filter::new("state", FilterOp::Eq, json!("shipped"))]);
    assert_eq!(shipped.total, 2);
    assert_eq!(c["objects"]["customer"]["unchanged"], 3, "{c}");
    assert_eq!(c["links"][&join_rel]["unlinked"], 1, "{c}");
    assert_eq!(get("order:2").props["amount"], json!(99.99));
    assert_eq!(out("order:2", &fk_rel), ["customer:2"], "the link moved with the foreign key");
    assert!(out("order:2", &join_rel).is_empty());
    assert!(o.ontology().get(&admin, "order:4").is_err());
    assert_eq!(explore(&o, &admin, "order", vec![]).total, 4);

    // Removing the connector with purge removes its objects too.
    let removed = o.call(&admin, "connector.remove", json!({ "name": "src", "purge": true })).await.unwrap();
    assert!(removed["deleted"].as_u64().unwrap() >= 11, "{removed}");
    assert!(o.ontology().get(&admin, "order:1").is_err());
    assert!(o.ontology().store().kv_scan("connector_state", "src/").unwrap().is_empty());

    drop(o);
}
