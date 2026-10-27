//! Live change streams through the engine API (memory storage): the ring
//! and the store backfill behind it, heartbeats, shared evaluation for
//! subscribers of one scope and filter, the slow-consumer cut and the
//! read check.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use onto_intelli::changes::{Frame, Reauth, ReauthError, Start, Subscription};
use onto_intelli::core::changes::{ChangeFilter, ChangeOp, Cursor, GapReason};
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
    // Every list is bounded (review SEC-2), on streams and on poll.
    let many: Vec<String> = (0..101).map(|n| format!("k{n}")).collect();
    let viewer = Principal::viewer("user:v", "acme");
    let f = ChangeFilter { kinds: many.clone(), ..Default::default() };
    let e = i.subscribe_changes(viewer.clone(), f, Start::Now, None).await.err().unwrap();
    assert_eq!(e.code(), "invalid_argument");
    let e = i.call(&viewer, "changes.poll", json!({ "kinds": many })).await.unwrap_err();
    assert_eq!(e.code(), "invalid_argument", "{e}");
    let long = "*".repeat(300);
    let e = i.call(&viewer, "changes.poll", json!({ "sources": [long] })).await.unwrap_err();
    assert_eq!(e.code(), "invalid_argument", "{e}");
    i.close();
    let e = i.subscribe_changes(Principal::admin("x"), tasks(), Start::Now, None).await.err().unwrap();
    assert_eq!(e.code(), "unavailable", "a closing tenant opens no stream");
}

/// The next frame that is not a heartbeat.
async fn next_frame(sub: &mut Subscription) -> Frame {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), sub.recv()).await.expect("a frame within 10 s") {
            Some(Frame::Heartbeat { .. }) => {}
            Some(f) => return f,
            None => panic!("the stream ended without a last frame"),
        }
    }
}

/// Review C1: a stream that stays open while capture is off is moved to the
/// head by heartbeats; when capture is on again it gets the capture_enabled
/// gap before the next record, and so does a reconnect from its last id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_open_while_capture_is_off_gets_a_gap() {
    let i = open(|c| c.changes.heartbeat_secs = 1).await;
    let admin = Principal::admin("x");
    let mut sub = i.subscribe_changes(admin.clone(), ChangeFilter::default(), Start::Now, None).await.unwrap();
    i.call(&admin, "changes.capture", json!({ "on": false })).await.unwrap();
    i.call(&admin, "entity.upsert", json!({ "id": "task:off", "title": "while off" })).await.unwrap();
    let head = i.ontology().change_head().unwrap();
    // Heartbeats carry the cursor to the head over the unrecorded commit.
    let mut last = sub.cursor;
    while last < head {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await.unwrap() {
            Some(Frame::Heartbeat { cursor, .. }) => last = cursor,
            other => panic!("expected heartbeats only: {other:?}"),
        }
    }
    i.call(&admin, "changes.capture", json!({ "on": true })).await.unwrap();
    i.call(&admin, "entity.upsert", json!({ "id": "task:on", "title": "after" })).await.unwrap();
    match next_frame(&mut sub).await {
        Frame::Change(r) => assert_eq!(r.gap.as_ref().map(|g| g.reason), Some(GapReason::CaptureEnabled), "{r:?}"),
        other => panic!("expected the gap first: {other:?}"),
    }
    match next_frame(&mut sub).await {
        Frame::Change(r) => assert_eq!(r.id, "task:on"),
        other => panic!("{other:?}"),
    }
    // A reconnect with Last-Event-ID = the heartbeat's cursor: the same.
    let mut again = i.subscribe_changes(admin, ChangeFilter::default(), Start::After(last), None).await.unwrap();
    match next_frame(&mut again).await {
        Frame::Change(r) => assert_eq!(r.op, ChangeOp::Gap),
        other => panic!("{other:?}"),
    }
}

/// Review C2: a stream resumed from a cursor ahead of the head (a cursor
/// from before a restart) resets instead of waiting silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_from_a_cursor_ahead_of_the_head_resets() {
    let i = open(|_| {}).await;
    write(&i, 0, 2).await;
    let ahead = Cursor::new(i.ontology().change_head().unwrap().seq + 100, 0);
    let mut sub = i.subscribe_changes(Principal::admin("x"), tasks(), Start::After(ahead), None).await.unwrap();
    match next_frame(&mut sub).await {
        Frame::Reset { code, message, .. } => {
            assert_eq!(code, "cursor_expired");
            assert!(message.contains("ahead of the head"), "{message}");
        }
        other => panic!("expected a reset: {other:?}"),
    }
    assert!(sub.recv().await.is_none());
}

/// Review SEC-4: a stream whose credential cannot be re-checked ends
/// `unavailable` after a bounded number of tries instead of streaming on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_that_cannot_reauthenticate_ends() {
    let i = open(|c| {
        c.changes.reauth_secs = 1;
        c.changes.heartbeat_secs = 1;
    })
    .await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = calls.clone();
    let reauth: Reauth = Arc::new(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Err(ReauthError::Unavailable("control store unreachable".into())) })
    });
    let mut sub =
        i.subscribe_changes(Principal::admin("key:revoked"), tasks(), Start::Now, Some(reauth)).await.unwrap();
    let started = std::time::Instant::now();
    let end = loop {
        match tokio::time::timeout(Duration::from_secs(10), sub.recv()).await.unwrap() {
            Some(Frame::End { code, message, .. }) => break (code, message),
            Some(_) => {}
            None => panic!("no last frame"),
        }
    };
    assert_eq!(end.0, "unavailable", "{}", end.1);
    let n = calls.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(n as u32, onto_intelli::changes::stream::REAUTH_UNAVAILABLE_LIMIT);
    assert!(started.elapsed() < Duration::from_secs(6), "{:?}", started.elapsed());
}

/// Review CP-1: evaluated pages are kept only while another member of the
/// group still needs them, and their memory is counted in the hub's bytes
/// (and so in `estimated_bytes`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_pages_are_counted_and_dropped_once_passed() {
    let i = open(|c| {
        c.changes.page = 10;
        c.changes.heartbeat_secs = 1;
    })
    .await;
    write(&i, 0, 200).await;
    let hub = i.changes().clone();
    // One stream alone keeps no pages.
    let mut solo = i.subscribe_changes(Principal::admin("solo"), tasks(), Start::Earliest, None).await.unwrap();
    records(&mut solo, 200).await;
    assert_eq!(hub.cache_bytes(), 0, "a group of one keeps nothing");
    drop(solo);

    // Two of one group: one reads all, the other reads nothing yet.
    let p = Principal::admin("pair");
    let mut a = i.subscribe_changes(p.clone(), tasks(), Start::Earliest, None).await.unwrap();
    let mut b = i.subscribe_changes(p, tasks(), Start::Earliest, None).await.unwrap();
    records(&mut a, 200).await;
    let mut held = 0;
    for _ in 0..200 {
        held = hub.cache_bytes();
        if held > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(held > 0, "pages kept for the member behind");
    assert!(hub.bytes() >= held);
    assert!(i.estimated_bytes() >= held as u64 + i.ontology().estimated_bytes());
    // Once both have passed them, the pages go.
    records(&mut b, 200).await;
    let mut left = usize::MAX;
    for _ in 0..500 {
        left = hub.cache_bytes();
        if left == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(left, 0, "idle members at the head hold no pages");
    drop((a, b));
}

/// Review CP-2: the change feed is trimmed by `[changes] retain_days` and
/// `retain_max_rows`, raising the floor, by the retention pass the tenant
/// runs 5 minutes after it opens and hourly after that.
#[tokio::test(start_paused = true)]
async fn retention_trims_the_feed_and_raises_the_floor() {
    let defaults = onto_intelli::config::ChangesConfig::default();
    assert_eq!((defaults.retain_days, defaults.retain_max_rows), (7, 5_000_000));
    let r = defaults.retention();
    assert_eq!((r.keep_ms, r.keep_rows), (Some(7 * 86_400_000), Some(5_000_000)));
    let i = open(|c| c.changes.retain_max_rows = 3).await;
    write(&i, 0, 10).await;
    let meta = || i.ontology().store().changes_meta().unwrap();
    assert_eq!(meta().floor, 0);
    // The scheduled pass (time is paused: sleeping advances it).
    let mut floor = 0;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_secs(10)).await;
        floor = meta().floor;
        if floor > 0 {
            break;
        }
    }
    assert!(floor > 0 && floor <= meta().head.seq, "the floor rose to {floor}");
    let kept = i.ontology().store().changes_since(Cursor::default(), 100).unwrap().changes.len();
    assert!(kept <= 3, "{kept} records kept");
    // Called directly: nothing more to trim.
    let v = i.vacuum_changes().unwrap();
    assert_eq!((v.deleted, v.floor), (0, floor));
}
