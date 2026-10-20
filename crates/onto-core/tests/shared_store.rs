//! Several processes on one store: optimistic commits, catch-up, schema and
//! redirect replication. Runs on a shared SQLite file always, and on
//! PostgreSQL when `ONTO_TEST_POSTGRES_URL` is set.

use std::sync::Arc;
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
fn two_processes_on_one_sqlite_file() {
    let dir = std::env::temp_dir().join(format!("onto-shared-{}", onto_core::time::new_id("t")));
    let db = dir.join("onto.db");
    let open = || Ontology::open_sqlite(Schema::enterprise(), &db, OntologyOptions::default()).unwrap();
    let a = open();
    let b = open();
    shared_suite(a, b);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sqlite_readers_catch_up_many_commits() {
    let dir = std::env::temp_dir().join(format!("onto-reload-{}", onto_core::time::new_id("t")));
    let db = dir.join("onto.db");
    let a = Ontology::open_sqlite(Schema::enterprise(), &db, OntologyOptions::default()).unwrap();
    let b = Ontology::open_sqlite(Schema::enterprise(), &db, OntologyOptions::default()).unwrap();
    for i in 0..30 {
        a.upsert(&ctx("ann"), EntityInput::new("task", format!("{i}"), format!("Task {i}"))).unwrap();
    }
    let report = b.sync().unwrap();
    assert_eq!(report.commits, 30);
    assert_eq!(b.stats().unwrap().entities, a.stats().unwrap().entities);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(feature = "postgres")]
#[test]
fn two_processes_on_one_postgres_tenant() {
    let Ok(url) = std::env::var("ONTO_TEST_POSTGRES_URL") else {
        eprintln!("skipped: set ONTO_TEST_POSTGRES_URL to run the PostgreSQL suite");
        return;
    };
    let tenant = format!("t{}", &onto_core::time::new_id("x")[2..]);
    let open = || {
        let store = onto_core::store::PgStore::connect(&url, &tenant, onto_core::store::PgOptions::default()).unwrap();
        let opts = OntologyOptions { follow_interval_ms: 200, ..Default::default() };
        Ontology::open(Schema::enterprise(), Arc::new(store), opts).unwrap()
    };
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
    onto_core::store::PgStore::connect(&url, &tenant, onto_core::store::PgOptions::default())
        .unwrap()
        .drop_tenant()
        .unwrap();
}
