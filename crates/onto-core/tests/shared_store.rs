//! Several processes on one PostgreSQL tenant: optimistic commits, catch-up,
//! schema and redirect replication, and the store itself (migrations,
//! vectors, append-only history). Runs when a test database is set (see
//! `common`).
#![cfg(feature = "postgres")]

mod common;

use std::time::{Duration, Instant};

use serde_json::json;

use onto_core::{ActionContext, EntityInput, Ontology, OntologyOptions, Principal, Schema, SchemaChangeSet};

fn ctx(who: &str) -> ActionContext {
    ActionContext::new(Principal::admin(who), "acme")
}

/// Wait until `f` holds (followers apply notices asynchronously).
fn eventually(what: &str, sync: &Ontology, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        sync.sync().unwrap();
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for: {what}");
}

/// The suite: `a` and `b` are two processes on the same store.
pub fn shared_suite(a: Ontology, b: Ontology) {
    let admin = Principal::admin("x");
    // A writes; B catches up.
    a.upsert(&ctx("ann"), EntityInput::new("team", "core", "Core Team")).unwrap();
    a.upsert(&ctx("ann"), EntityInput::new("issue", "1", "Slow login").state("open").link("belongs_to", "team:core"))
        .unwrap();
    eventually("b sees a's issue", &b, || b.get(&admin, "issue:1").is_ok());
    assert_eq!(b.neighbors(&admin, "issue:1", Some("belongs_to")).unwrap()[0].1.id, "team:core");
    assert_eq!(b.seq(), a.seq());

    // B writes without having seen A's latest change: the optimistic commit
    // is refused, B catches up and retries; nothing is lost.
    a.upsert(
        &ctx("ann"),
        EntityInput { id: Some("issue:1".into()), state: Some("in_progress".into()), ..Default::default() },
    )
    .unwrap();
    b.upsert(&ctx("ben"), EntityInput::new("issue", "1", "Slow login").prop("priority", "high")).unwrap();
    for o in [&a, &b] {
        o.sync().unwrap();
        let e = o.get(&admin, "issue:1").unwrap();
        assert_eq!(e.state.as_deref(), Some("in_progress"), "A's state survives B's write");
        assert_eq!(e.prop_str("priority"), Some("high"));
        assert_eq!(e.revision, 3);
    }
    let history = b.history(&admin, "issue:1", 10).unwrap();
    assert_eq!(history.iter().map(|h| h.actor.as_str()).collect::<Vec<_>>(), vec!["ben", "ann", "ann"]);

    // Deletes and links replicate.
    a.upsert(&ctx("ann"), EntityInput::new("task", "t", "Fix it").link("part_of", "issue:1")).unwrap();
    eventually("b sees the task", &b, || b.get(&admin, "task:t").is_ok());
    b.delete(&ctx("ben"), "task:t").unwrap();
    eventually("a sees the delete", &a, || a.get(&admin, "task:t").is_err());
    assert!(a.neighbors(&admin, "issue:1", Some("has_part")).unwrap().is_empty());

    // Schema changes replicate, including what they migrate.
    let set: SchemaChangeSet = serde_json::from_value(json!({ "changes": [
        { "op": "put_property", "object_type": "team", "name": "slack", "def": { "type": "string", "searchable": true } }
    ] }))
    .unwrap();
    a.schema_apply(&ctx("ann"), set).unwrap();
    eventually("b adopts schema v2", &b, || b.schema_version() == 2);
    assert!(b.schema().object_types["team"].properties.contains_key("slack"));
    b.upsert(&ctx("ben"), EntityInput::new("team", "core", "Core Team").prop("slack", "#core-oncall")).unwrap();
    eventually("a finds the slack channel", &a, || {
        a.search(&admin, "oncall", &[], None, 5).unwrap().iter().any(|h| h.entity.id == "team:core")
    });

    // Merges replicate their redirects.
    a.upsert(&ctx("ann"), EntityInput::new("team", "core2", "Core")).unwrap();
    a.merge(&ctx("ann"), "team:core", "team:core2").unwrap();
    eventually("b follows the redirect", &b, || b.get(&admin, "team:core2").is_ok_and(|e| e.id == "team:core"));

    // Racing writers on the same entity both land.
    let (a2, b2) = (a.clone(), b.clone());
    let ta = std::thread::spawn(move || {
        for i in 0..20 {
            a2.upsert(&ctx("ann"), EntityInput::new("issue", "1", "Slow login").prop("a_count", i)).unwrap();
        }
    });
    let tb = std::thread::spawn(move || {
        for i in 0..20 {
            b2.upsert(&ctx("ben"), EntityInput::new("issue", "1", "Slow login").prop("b_count", i)).unwrap();
        }
    });
    ta.join().unwrap();
    tb.join().unwrap();
    a.sync().unwrap();
    b.sync().unwrap();
    let ea = a.get(&admin, "issue:1").unwrap();
    let eb = b.get(&admin, "issue:1").unwrap();
    assert_eq!(ea.props["a_count"], json!(19));
    assert_eq!(ea.props["b_count"], json!(19));
    assert_eq!(ea.revision, eb.revision);
    assert_eq!(ea.revision, 3 + 40, "every write is a revision; none was lost");
    assert_eq!(a.store().head().unwrap(), a.seq());
}

#[test]
fn two_processes_on_one_postgres_tenant() {
    let Some(pg) = common::TestTenant::new("shared") else { return };
    let open = || pg.open(Schema::enterprise(), OntologyOptions { follow_interval_ms: 200, ..Default::default() });
    let a = open();
    let b = open();
    shared_suite(a.clone(), b.clone());
    // Followers apply other processes' writes without explicit syncs.
    a.upsert(&ctx("ann"), EntityInput::new("team", "edge", "Edge Team")).unwrap();
    let start = Instant::now();
    while b.get(&Principal::admin("x"), "team:edge").is_err() {
        assert!(start.elapsed() < Duration::from_secs(5), "LISTEN/NOTIFY follower did not deliver");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn readers_catch_up_many_commits() {
    let Some(pg) = common::TestTenant::new("catchup") else { return };
    let a = pg.open(Schema::enterprise(), OntologyOptions::default());
    let b = pg.open(Schema::enterprise(), OntologyOptions::default());
    for i in 0..30 {
        a.upsert(&ctx("ann"), EntityInput::new("task", format!("{i}"), format!("Task {i}"))).unwrap();
    }
    let report = b.sync().unwrap();
    assert!(report.commits >= 30 || b.seq() == a.seq(), "{report:?}");
    assert_eq!(b.stats().unwrap().entities, a.stats().unwrap().entities);
}

#[test]
fn migrations_are_versioned_and_idempotent() {
    let Some(pg) = common::TestTenant::new("migrate") else { return };
    let store = pg.store();
    let applied = store.migrations().unwrap();
    let known: Vec<i32> = onto_core::store::postgres::MIGRATIONS.iter().map(|m| m.0).collect();
    assert_eq!(applied.iter().map(|m| m.0).collect::<Vec<_>>(), known);
    assert_eq!(applied[0].1, "init");
    // Opening again applies nothing new.
    let again = pg.store();
    assert_eq!(again.migrations().unwrap(), applied);
}

#[test]
fn vectors_live_in_pgvector_and_rank_like_memory() {
    let Some(pg) = common::TestTenant::new("vectors") else { return };
    let o = pg.open(Schema::enterprise(), OntologyOptions::default());
    let mem = Ontology::in_memory(Schema::enterprise());
    let docs = [
        ("1", "Payment timeout under load"),
        ("2", "Checkout UI polish"),
        ("3", "Postgres replication lag"),
        ("4", "Payments retries time out at peak"),
    ];
    for (k, title) in docs {
        for x in [&o, &mem] {
            x.upsert(&ctx("ann"), EntityInput::new("issue", k, title)).unwrap();
        }
    }
    // Vectors are computed right after each write, off the write path.
    o.flush_vectors().unwrap();
    let admin = Principal::admin("x");
    let vector = |x: &Ontology, q: &str| -> Vec<String> {
        x.search_with(&admin, q, &[], None, 3, onto_core::SearchMode::Vector, false)
            .unwrap()
            .into_iter()
            .map(|h| h.entity.id)
            .collect()
    };
    let pg_hits = vector(&o, "payments timing out");
    assert!(pg_hits.starts_with(&["issue:4".into(), "issue:1".into()]), "{pg_hits:?}");
    assert_eq!(pg_hits, vector(&mem, "payments timing out"), "pgvector ranks like the in-memory index");
    assert_eq!(vector(&o, "postgress replication").first().map(String::as_str), Some("issue:3"));
    assert_eq!(o.stats().unwrap().vectors, 4);
    // Kind filters reach the index; a fresh process finds the same.
    assert!(
        o.search_with(&admin, "payment", &["team".into()], None, 3, onto_core::SearchMode::Vector, false)
            .unwrap()
            .is_empty()
    );
    let b = pg.open(Schema::enterprise(), OntologyOptions::default());
    assert_eq!(vector(&b, "payments timing out"), pg_hits);

    // A state change keeps the stored vector (no re-embedding, and the row
    // update can stay HOT); a new title is re-embedded in the background.
    let store = pg.store();
    use onto_core::store::Store;
    o.upsert(&ctx("ann"), EntityInput { id: Some("issue:3".into()), state: Some("open".into()), ..Default::default() })
        .unwrap();
    assert!(!store.vectors_pending().unwrap(), "unchanged text keeps its vector");
    o.upsert(&ctx("ann"), EntityInput::new("issue", "3", "Kafka consumer lag")).unwrap();
    o.flush_vectors().unwrap();
    assert_eq!(vector(&o, "kafka consumers").first().map(String::as_str), Some("issue:3"));
    // The background vectorizer gets there on its own too.
    o.upsert(&ctx("ann"), EntityInput::new("issue", "5", "Flaky checkout tests")).unwrap();
    let start = Instant::now();
    while store.vectors_pending().unwrap() {
        assert!(start.elapsed() < Duration::from_secs(10), "the vectorizer did not run");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(vector(&o, "flaky checkout").first().map(String::as_str), Some("issue:5"));
}

#[test]
fn bulk_imports_get_their_vectors_on_open() {
    let Some(pg) = common::TestTenant::new("backfill") else { return };
    let store = pg.store();
    let entities: Vec<onto_core::Entity> = (0..50)
        .map(|i| {
            let title = if i == 7 { "Kafka consumer stuck".to_string() } else { format!("Routine chore {i}") };
            serde_json::from_value(
                json!({ "id": format!("issue:{i}"), "kind": "issue", "workspace": "acme", "title": title }),
            )
            .unwrap()
        })
        .collect();
    use onto_core::store::Store;
    store.bulk_import(&entities, &[]).unwrap();
    let o = pg.open(Schema::enterprise(), OntologyOptions::default());
    assert_eq!(o.stats().unwrap().vectors, 50, "opening backfilled every vector");
    let hits = o
        .search_with(&Principal::admin("x"), "kafka consumers", &[], None, 1, onto_core::SearchMode::Vector, false)
        .unwrap();
    assert_eq!(hits[0].entity.id, "issue:7");
    assert_eq!(o.backfill_vectors().unwrap(), 0, "nothing left to do");
}

#[test]
fn history_is_append_only() {
    let Some(pg) = common::TestTenant::new("append") else { return };
    let o = pg.open(Schema::enterprise(), OntologyOptions::default());
    o.upsert(&ctx("ann"), EntityInput::new("team", "core", "Core")).unwrap();
    let schema = pg.store().schema_name().to_string();
    let url = pg.url.clone();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let res = rt.block_on(async move {
        let pool = onto_core::sqlx::PgPool::connect(&url).await.unwrap();
        let update = format!("UPDATE \"{schema}\".events SET actor = 'mallory'");
        onto_core::sqlx::raw_sql(onto_core::sqlx::AssertSqlSafe(update)).execute(&pool).await
    });
    let msg = res.unwrap_err().to_string();
    assert!(msg.contains("append-only"), "{msg}");
}

/// Expired KV rows of one namespace go, `limit` at a time; live ones and
/// other namespaces stay.
fn purge_suite(store: &dyn onto_core::store::Store) {
    let now = onto_core::time::now_ms();
    for (ns, key, exp) in [
        ("auth.revoked", "old1", Some(now - 1_000)),
        ("auth.revoked", "old2", Some(now - 1_000)),
        ("auth.revoked", "old3", Some(now - 1_000)),
        ("auth.revoked", "live", Some(now + 600_000)),
        ("auth.revoked", "kept", None),
        ("other", "old", Some(now - 1_000)),
    ] {
        store.kv_put(ns, key, &json!(1), exp).unwrap();
    }
    assert_eq!(store.kv_purge_expired("auth.revoked", 2).unwrap(), 2, "one batch");
    assert_eq!(store.kv_purge_expired("auth.revoked", 2).unwrap(), 1, "the rest");
    assert_eq!(store.kv_purge_expired("auth.revoked", 2).unwrap(), 0);
    let keys: Vec<String> = store.kv_scan("auth.revoked", "").unwrap().into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["kept", "live"]);
    assert_eq!(store.kv_purge_expired("other", 2).unwrap(), 1, "the other namespace was left alone");
}

#[test]
fn expired_kv_rows_are_purged_one_namespace_at_a_time() {
    purge_suite(&onto_core::store::MemoryStore::default());
    let Some(pg) = common::TestTenant::new("kv_purge") else { return };
    purge_suite(&pg.store());
}
