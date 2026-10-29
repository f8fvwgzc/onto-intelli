//! The counters of docs/design/EVENTS.md section 11 on the in-memory store
//! and on PostgreSQL (when a test database is set; see `common`): change lag
//! per consumer (head seq minus cursor, the age of the oldest unread record,
//! expiry below the floor), gap records over a window with the latest gap,
//! outbox rows per target and state (pending, inflight, retry, dead, each
//! capped), and runs per state since a time. On PostgreSQL the gap and
//! outbox counts read their own partial indexes (0014).

mod common;

use std::sync::Arc;

use serde_json::json;

use onto_core::automate::Automation;
use onto_core::changes::ChangeRetention;
use onto_core::outbox::{Attempt, OutboxKind, OutboxRow, OutboxState, SOURCE_TEST};
use onto_core::runs::{RunRow, RunState, RunTrigger};
use onto_core::store::{MemoryStore, Store};
use onto_core::{ActionContext, Cursor, EntityInput, Mutation, Ontology, OntologyOptions, Principal, Schema};

fn ctx() -> ActionContext {
    ActionContext::new(Principal::admin("ada"), "acme")
}

fn issue(key: &str) -> Mutation {
    let mut e = EntityInput::new("issue", key, format!("Issue {key}"));
    e.workspace = Some("acme".into());
    Mutation::Upsert(e)
}

/// Lag: head seq minus the cursor's seq, the age of the oldest record after
/// it, `expired` below the floor; a consumer at the head is 0 behind.
fn consumer_lag(o: &Ontology) {
    let store = o.store();
    assert!(store.consumer_lags().unwrap().is_empty(), "no consumer yet");
    o.apply(&ctx(), vec![issue("1")]).unwrap();
    let start = o.change_head().unwrap();
    store.consumer_set("webhook:ops", start, None).unwrap();
    store.consumer_set("automation:consumer", start, None).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    for k in 2..5 {
        o.apply(&ctx(), vec![issue(&k.to_string())]).unwrap();
    }
    let head = o.change_head().unwrap();
    store.consumer_set("automation:consumer", head, None).unwrap();

    let lags = store.consumer_lags().unwrap();
    let names: Vec<&str> = lags.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["automation:consumer", "webhook:ops"], "by name");
    let (auto, ops) = (&lags[0], &lags[1]);
    assert_eq!((auto.cursor, auto.behind, auto.age_ms, auto.expired), (head, 0, 0, false), "{auto:?}");
    assert_eq!(ops.cursor, start);
    assert_eq!(ops.behind, head.seq - start.seq, "{ops:?}");
    assert_eq!(ops.behind, 3);
    assert!(ops.age_ms >= 0 && ops.age_ms < 60_000, "{ops:?}");
    assert!(!ops.expired);

    // Retention past the endpoint's cursor: its unread records are gone.
    store.vacuum_changes(&ChangeRetention { keep_ms: None, keep_rows: Some(1) }).unwrap();
    let ops = store.consumer_lags().unwrap().into_iter().find(|l| l.name == "webhook:ops").unwrap();
    assert!(ops.expired, "{ops:?}");
    assert_eq!(ops.behind, 3);
}

/// Gaps: counted from a time, capped, and the newest one whatever its age,
/// summarized without its ids.
fn gaps(o: &Ontology) {
    let store = o.store();
    let none = store.gap_stats(0, 100).unwrap();
    assert_eq!((none.count, none.capped, none.latest.is_none()), (0, false, true), "{none:?}");
    o.apply(&ctx(), vec![issue("1")]).unwrap();
    assert_eq!(store.gap_stats(0, 100).unwrap().count, 0, "captured commits are not gaps");

    let base = o.get(&Principal::admin("ada"), "issue:1").unwrap();
    let e = |k: &str| {
        let mut e = base.clone();
        e.id = format!("issue:{k}");
        e
    };
    store.bulk_import(&[e("b1"), e("b2"), e("b3")], &[]).unwrap();
    store.bulk_import(&[e("b4")], &[]).unwrap();
    let head = store.head().unwrap();

    let g = store.gap_stats(0, 100).unwrap();
    assert_eq!((g.count, g.capped), (2, false), "{g:?}");
    let latest = g.latest.expect("the latest gap");
    assert_eq!(latest.cursor, Cursor::new(head, 0));
    assert_eq!(latest.reason, "bulk_import");
    assert_eq!(latest.touched, 1, "{latest:?}");
    let json = serde_json::to_string(&latest).unwrap();
    assert!(!json.contains("issue:b4"), "no ids: {json}");

    let capped = store.gap_stats(0, 1).unwrap();
    assert_eq!((capped.count, capped.capped), (1, true), "{capped:?}");
    let later = store.gap_stats(i64::MAX, 100).unwrap();
    assert_eq!(later.count, 0, "none in a window after them");
    assert_eq!(later.latest.map(|l| l.cursor), Some(Cursor::new(head, 0)), "the latest whatever its age");
    let only_latest = store.gap_stats(i64::MAX, 0).unwrap();
    assert_eq!((only_latest.count, only_latest.capped), (0, false));
    assert!(only_latest.latest.is_some());
}

fn row(target: &str, dedupe: &str) -> OutboxRow {
    let mut r = OutboxRow::new(OutboxKind::Webhook, target, dedupe, SOURCE_TEST, "acme", "onto.webhook.test");
    r.payload = json!({ "n": dedupe });
    r
}

/// Outbox rows per target and state: pending, inflight, retry and dead,
/// each capped; delivered and cancelled are not counted.
fn outbox_states(store: &dyn Store) {
    assert!(store.outbox_state_counts(100).unwrap().is_empty());
    let rows: Vec<OutboxRow> = (0..6).map(|i| row("ops", &format!("k{i}"))).collect();
    store.outbox_enqueue(&rows, None, None).unwrap();
    store.outbox_enqueue(&[row("billing", "k0")], None, None).unwrap();
    // ops: 2 claimed and left inflight, 1 retry, 1 dead, 1 delivered, 1 pending.
    let claimed = store.outbox_claim(5, 60_000, "w1", &["billing".to_string()]).unwrap();
    assert_eq!(claimed.len(), 5);
    let attempt = Attempt { status: Some(500), duration_ms: 1, ..Default::default() };
    assert!(
        store
            .outbox_finish(&claimed[0].id, claimed[0].claim.as_deref().unwrap(), &attempt, OutboxState::Retry, 60_000)
            .unwrap()
    );
    assert!(
        store
            .outbox_finish(&claimed[1].id, claimed[1].claim.as_deref().unwrap(), &attempt, OutboxState::Dead, 0)
            .unwrap()
    );
    let ok = Attempt { status: Some(200), duration_ms: 1, ..Default::default() };
    assert!(
        store
            .outbox_finish(&claimed[2].id, claimed[2].claim.as_deref().unwrap(), &ok, OutboxState::Delivered, 0)
            .unwrap()
    );

    let counts = store.outbox_state_counts(100).unwrap();
    assert_eq!(
        counts,
        vec![
            ("billing".to_string(), OutboxState::Pending, 1),
            ("ops".to_string(), OutboxState::Dead, 1),
            ("ops".to_string(), OutboxState::Inflight, 2),
            ("ops".to_string(), OutboxState::Pending, 1),
            ("ops".to_string(), OutboxState::Retry, 1),
        ],
        "by target, then state; delivered left out"
    );
    let capped = store.outbox_state_counts(1).unwrap();
    assert!(capped.iter().all(|(_, _, n)| *n == 1), "{capped:?}");
    assert_eq!(capped.len(), 5);
}

fn run(automation: &str, n: u64, state: RunState) -> RunRow {
    let trigger: RunTrigger = serde_json::from_value(json!({
        "on": "change", "cursor": format!("{n}.0"), "op": "entity.update",
        "id": "issue:1", "kind": "issue", "workspace": "acme", "subject": "issue:1",
    }))
    .unwrap();
    let mut r = RunRow::new(automation, "h", 1, &Automation::default(), trigger);
    r.state = state;
    r
}

/// Runs per state since a time, counting at most the newest `cap`.
fn run_states(store: &dyn Store) {
    let since = store.clock_ms().unwrap() - 1_000;
    let rows = [
        run("a", 1, RunState::Succeeded),
        run("a", 2, RunState::Succeeded),
        run("a", 3, RunState::Failed),
        run("b", 1, RunState::Skipped),
        run("b", 2, RunState::Running),
    ];
    assert_eq!(store.run_claim(&rows, None).unwrap().len(), 5);
    let by = store.runs_state_counts(since, 100).unwrap();
    assert_eq!(
        by,
        vec![(RunState::Failed, 1), (RunState::Running, 1), (RunState::Skipped, 1), (RunState::Succeeded, 2)],
        "by state name"
    );
    assert_eq!(store.runs_state_counts(since, 2).unwrap().iter().map(|(_, n)| n).sum::<u64>(), 2, "capped");
    assert!(store.runs_state_counts(store.clock_ms().unwrap() + 60_000, 100).unwrap().is_empty());
}

#[test]
fn memory_consumer_lag() {
    consumer_lag(&Ontology::in_memory(Schema::enterprise()));
}

#[test]
fn memory_gaps() {
    gaps(&Ontology::in_memory(Schema::enterprise()));
}

#[test]
fn memory_outbox_states() {
    outbox_states(&MemoryStore::default());
}

#[test]
fn memory_run_states() {
    run_states(&MemoryStore::default());
}

#[test]
fn postgres_consumer_lag() {
    let Some(pg) = common::TestTenant::new("lag") else { return };
    consumer_lag(&pg.open(Schema::enterprise(), OntologyOptions::default()));
}

#[test]
fn postgres_gaps() {
    let Some(pg) = common::TestTenant::new("gaps") else { return };
    gaps(&pg.open(Schema::enterprise(), OntologyOptions::default()));
}

#[test]
fn postgres_outbox_states() {
    let Some(pg) = common::TestTenant::new("outbox_states") else { return };
    outbox_states(&pg.store());
}

#[test]
fn postgres_run_states() {
    let Some(pg) = common::TestTenant::new("run_states") else { return };
    run_states(&pg.store());
}

/// The gap and outbox counts can be answered from the 0014 partial indexes
/// (with sequential scans priced out, as on a large table), so they read
/// gap records and unfinished or dead rows only.
#[test]
fn postgres_counters_read_their_indexes() {
    let Some(pg) = common::TestTenant::new("counter_plans") else { return };
    let store = Arc::new(pg.store());
    let schema = format!("\"{}\"", onto_core::store::postgres::tenant_schema("onto_", &pg.tenant).unwrap());
    let plan = |q: &str| -> String {
        let url = pg.url.clone();
        let q = format!("EXPLAIN {}", q.replace("%S%", &schema));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let pool = onto_core::sqlx::PgPool::connect(&url).await.unwrap();
            let mut conn = pool.acquire().await.unwrap();
            onto_core::sqlx::raw_sql(onto_core::sqlx::AssertSqlSafe("SET enable_seqscan = off".to_string()))
                .execute(&mut *conn)
                .await
                .unwrap();
            let rows: Vec<(String,)> =
                onto_core::sqlx::query_as(onto_core::sqlx::AssertSqlSafe(q)).fetch_all(&mut *conn).await.unwrap();
            drop(conn);
            pool.close().await;
            rows.into_iter().map(|r| r.0).collect::<Vec<_>>().join("\n")
        })
    };
    store.gap_stats(0, 10).unwrap(); // migrated
    let gaps = plan("SELECT count(*) FROM (SELECT 1 FROM %S%.changes WHERE op = 'gap' AND ts >= 0 LIMIT 10) x");
    assert!(gaps.contains("changes_gaps"), "{gaps}");
    let dead = plan(
        "SELECT count(*) FROM (SELECT 1 FROM %S%.outbox o WHERE o.target = 'ops' AND o.state = 'dead' \
         AND o.state IN ('pending', 'inflight', 'retry', 'dead') LIMIT 10) x",
    );
    assert!(dead.contains("outbox_stats"), "{dead}");
}

/// Review findings OBS-2 and OBS-1: a consumer parked at the floor's edge
/// (where the fan-out and the automation consumer leave it once its gap is
/// handled) reads back exactly there, also on PostgreSQL whose INT column
/// clamps `n`, and is not expired; one below it is. A deleted consumer is
/// gone from the lags.
fn parked_and_deleted_consumers(o: &Ontology) {
    let store = o.store();
    for k in 1..5 {
        o.apply(&ctx(), vec![issue(&k.to_string())]).unwrap();
    }
    store.vacuum_changes(&ChangeRetention { keep_ms: None, keep_rows: Some(1) }).unwrap();
    let floor = store.changes_meta().unwrap().floor;
    assert!(floor > 1, "retention moved the floor: {floor}");
    let edge = Cursor::floor_edge(floor);
    store.consumer_set("webhook:parked", edge, None).unwrap();
    assert_eq!(store.consumer_get("webhook:parked").unwrap(), Some(edge), "parked exactly at the edge");
    assert!(!edge.expired_at(floor) && Cursor::new(floor - 1, 0).expired_at(floor));
    store.consumer_set("webhook:behind", Cursor::new(floor - 1, 0), None).unwrap();
    let lags = store.consumer_lags().unwrap();
    let lag = |n: &str| lags.iter().find(|l| l.name == n).cloned().unwrap();
    assert_eq!((lag("webhook:parked").cursor, lag("webhook:parked").expired), (edge, false), "{lags:?}");
    assert!(lag("webhook:behind").expired, "{lags:?}");

    assert!(store.consumer_delete("webhook:parked", None).unwrap());
    assert!(!store.consumer_delete("webhook:parked", None).unwrap(), "already gone");
    assert_eq!(store.consumer_get("webhook:parked").unwrap(), None);
    assert!(store.consumer_lags().unwrap().iter().all(|l| l.name != "webhook:parked"));
}

#[test]
fn memory_parked_and_deleted_consumers() {
    parked_and_deleted_consumers(&Ontology::in_memory(Schema::enterprise()));
}

#[test]
fn postgres_parked_and_deleted_consumers() {
    let Some(pg) = common::TestTenant::new("parked") else { return };
    parked_and_deleted_consumers(&pg.open(Schema::enterprise(), OntologyOptions::default()));
}
