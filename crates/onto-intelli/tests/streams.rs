//! Live change streams through the engine API (memory storage): the ring
//! and the store backfill behind it, heartbeats, shared evaluation for
//! subscribers of one scope and filter, the slow-consumer cut and the
//! read check.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use onto_intelli::changes::{Frame, Start, Subscription};
use onto_intelli::core::changes::{ChangeFilter, ChangeOp};
use onto_intelli::{Config, Intelli, Principal};

async fn open(tweak: impl FnOnce(&mut Config)) -> Arc<Intelli> {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    tweak(&mut cfg);
    Intelli::open(cfg).await.unwrap()
}

fn tasks() -> ChangeFilter {
    ChangeFilter { kinds: vec!["task".into()], ops: vec![ChangeOp::EntityCreate], ..Default::default() }
}

async fn write(i: &Intelli, from: usize, to: usize) {
    let admin = Principal::admin("x");
    for n in from..to {
        i.call(&admin, "entity.upsert", json!({ "id": format!("task:t{n}"), "title": format!("t{n}") })).await.unwrap();
    }
}

/// The next `n` records (heartbeats skipped).
async fn records(sub: &mut Subscription, n: usize) -> Vec<Arc<onto_intelli::core::changes::ChangeRecord>> {
    let mut out = Vec::new();
    while out.len() < n {
        match tokio::time::timeout(Duration::from_secs(10), sub.recv()).await.expect("a frame within 10 s") {
            Some(Frame::Change(r)) => out.push(r),
            Some(Frame::Heartbeat { .. }) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backfill_behind_a_small_ring_then_live_records_in_order() {
    let i = open(|c| {
        c.changes.ring = 4;
        c.changes.page = 3;
    })
    .await;
    write(&i, 0, 20).await;
    let mut sub = i.subscribe_changes(Principal::admin("x"), tasks(), Start::Earliest, None).await.unwrap();
    let ids: Vec<String> = records(&mut sub, 20).await.iter().map(|r| r.id.clone()).collect();
    assert_eq!(ids, (0..20).map(|n| format!("task:t{n}")).collect::<Vec<_>>(), "every record, in order");
    write(&i, 20, 30).await;
    let live = records(&mut sub, 10).await;
    let ids: Vec<String> = live.iter().map(|r| r.id.clone()).collect();
    assert_eq!(ids, (20..30).map(|n| format!("task:t{n}")).collect::<Vec<_>>());
    assert!(live.windows(2).all(|w| w[0].cursor < w[1].cursor));
    assert!(i.changes().ring_len() <= 4, "the ring keeps at most changes.ring records");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeats_carry_the_cursor_and_the_head() {
    let i = open(|c| c.changes.heartbeat_secs = 1).await;
    let mut sub = i.subscribe_changes(Principal::admin("x"), tasks(), Start::Now, None).await.unwrap();
    let start = sub.cursor;
    // Records the filter drops still move the cursor.
    let admin = Principal::admin("x");
    i.call(&admin, "entity.upsert", json!({ "id": "issue:quiet", "title": "not a task" })).await.unwrap();
    let head = i.ontology().change_head().unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await.unwrap() {
            Some(Frame::Heartbeat { cursor, head: h }) => {
                assert!(cursor >= start && h >= cursor.seq);
                if cursor >= head {
                    break;
                }
            }
            other => panic!("expected heartbeats only: {other:?}"),
        }
    }
    assert!(i.stream_touched() > 0, "heartbeats touch the tenant");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_scope_and_filter_share_one_evaluation() {
    let i = open(|_| {}).await;
    let p = Principal::admin("x");
    let mut a = i.subscribe_changes(p.clone(), tasks(), Start::Now, None).await.unwrap();
    let mut b = i.subscribe_changes(p.clone(), tasks(), Start::Now, None).await.unwrap();
    assert_eq!(a.cursor, b.cursor);
    write(&i, 0, 5).await;
    let (ra, rb) = (records(&mut a, 5).await, records(&mut b, 5).await);
    assert!(ra.iter().zip(&rb).all(|(x, y)| Arc::ptr_eq(x, y)), "the same evaluated records, shared");
    assert_eq!(i.changes().subscribers(), 2);
    drop((a, b));
    // The producers notice the closed channels on their next frame or wait.
    write(&i, 5, 6).await;
    for _ in 0..100 {
        if i.changes().subscribers() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(i.changes().subscribers(), 0);
    assert_eq!(i.changes().bytes(), 0, "the ring goes with the last subscriber");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_stops_reading_is_cut() {
    let i = open(|c| c.changes.stall_timeout_secs = 1).await;
    let mut sub = i.subscribe_changes(Principal::admin("x"), tasks(), Start::Now, None).await.unwrap();
    write(&i, 0, onto_intelli::changes::stream::CHANNEL + 20).await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let mut got = 0;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await.unwrap() {
            Some(Frame::Change(_)) => got += 1,
            Some(Frame::Heartbeat { .. }) => {}
            Some(Frame::End { code, cursor, .. }) => {
                assert_eq!(code, "slow_consumer");
                assert!(got <= onto_intelli::changes::stream::CHANNEL, "{got}");
                assert!(cursor.seq > 0);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(sub.recv().await.is_none());
}

#[tokio::test]
async fn streams_need_a_read_role_and_a_valid_filter() {
    let i = open(|_| {}).await;
    let nobody = Principal::new("user:nobody", vec![], vec!["*".into()]);
    let e = i.subscribe_changes(nobody, ChangeFilter::default(), Start::Now, None).await.err().unwrap();
    assert_eq!(e.code(), "permission_denied");
    let f = ChangeFilter { ids: (0..1001).map(|n| format!("task:{n}")).collect(), ..Default::default() };
    let e = i.subscribe_changes(Principal::admin("x"), f, Start::Now, None).await.err().unwrap();
    assert_eq!(e.code(), "invalid_argument");
    i.close();
    let e = i.subscribe_changes(Principal::admin("x"), tasks(), Start::Now, None).await.err().unwrap();
    assert_eq!(e.code(), "unavailable", "a closing tenant opens no stream");
}
