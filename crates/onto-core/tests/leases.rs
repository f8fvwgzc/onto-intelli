//! Leases, write fencing, consumer cursors, timers and wake rows on the
//! in-memory store and on PostgreSQL (when a test database is set; see
//! `common`). Lease times are the store's clock: the store API takes
//! durations, never a caller's deadline, so a holder whose own clock is
//! skewed cannot hold a lease another holder also holds.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use onto_core::leases::Fence;
use onto_core::store::{MemoryStore, PgOptions, PgShared, Snapshot, Store, StoreStats, VersionRecord, WriteBatch};
use onto_core::time::now_ms;
use onto_core::{
    ActionContext, Cursor, EntityInput, Error, Event, EventFilter, Mutation, Ontology, OntologyOptions, Principal,
    Schema,
};

fn ctx() -> ActionContext {
    ActionContext::new(Principal::admin("ada"), "ws1")
}

fn issue(key: &str) -> Mutation {
    let mut e = EntityInput::new("issue", key, format!("Issue {key}"));
    e.workspace = Some("ws1".into());
    Mutation::Upsert(e)
}

fn lost(r: onto_core::Result<impl std::fmt::Debug>) -> String {
    match r {
        Err(Error::Unavailable(m)) if m.starts_with("lease lost") => m,
        other => panic!("expected lease lost, got {other:?}"),
    }
}

/// The lease protocol every store follows.
fn protocol(store: &dyn Store) {
    assert!(store.fences());
    assert_eq!(store.lease("l", "a", 60_000).unwrap(), Some(1), "first holder: epoch 1");
    assert_eq!(store.lease("l", "b", 60_000).unwrap(), None, "held by a");
    assert_eq!(store.lease("l", "a", 60_000).unwrap(), Some(1), "renewing keeps the epoch");
    let row = store.lease_get("l").unwrap().unwrap();
    assert!(row.held && row.holder == "a" && row.epoch == 1, "{row:?}");
    assert!(!store.release(&Fence::new("l", "b", 1)).unwrap(), "only the holder releases");
    assert!(!store.release(&Fence::new("l", "a", 2)).unwrap(), "only at its epoch");
    assert!(store.release(&Fence::new("l", "a", 1)).unwrap());
    assert!(!store.release(&Fence::new("l", "a", 1)).unwrap(), "released once");
    assert!(!store.lease_get("l").unwrap().unwrap().held);
    assert_eq!(store.lease("l", "b", 60_000).unwrap(), Some(2), "a new holder: the epoch grows");
    assert_eq!(store.lease("l", "a", 60_000).unwrap(), None);
    assert!(store.release(&Fence::new("l", "b", 2)).unwrap());
    assert_eq!(store.lease("l", "a", 60_000).unwrap(), Some(3));
    assert_eq!(store.lease_get("missing").unwrap(), None);

    // Expiry by the store's clock.
    assert_eq!(store.lease("t", "a", 50).unwrap(), Some(1));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(store.lease("t", "b", 60_000).unwrap(), Some(2), "expired: taken over");

    // Consumer cursors move only under the lease that fences them.
    assert_eq!(store.consumer_get("webhook:a").unwrap(), None);
    store.consumer_set("webhook:a", Cursor::new(5, 1), Some(&Fence::new("l", "a", 3))).unwrap();
    assert_eq!(store.consumer_get("webhook:a").unwrap(), Some(Cursor::new(5, 1)));
    lost(store.consumer_set("webhook:a", Cursor::new(9, 0), Some(&Fence::new("l", "b", 2))));
    lost(store.consumer_set("webhook:a", Cursor::new(9, 0), Some(&Fence::new("t", "a", 1))));
    assert_eq!(store.consumer_get("webhook:a").unwrap(), Some(Cursor::new(5, 1)), "a refused move moves nothing");
    store.consumer_set("webhook:a", Cursor::new(7, 0), None).unwrap();
    assert_eq!(store.consumer_get("webhook:a").unwrap(), Some(Cursor::new(7, 0)));
}

/// Timers are due by the store's clock; a delete names the due time it saw.
fn timers(store: &dyn Store) {
    let soon = store.timers_set("retry:r1", 0, &json!({ "run": "r1" })).unwrap();
    let later = store.timers_set("retry:r2", 60_000, &json!({ "run": "r2" })).unwrap();
    assert!(later - soon >= 59_000, "{soon} {later}");
    let due = store.timers_due(10).unwrap();
    assert_eq!(due.len(), 1, "{due:?}");
    assert_eq!((due[0].name.as_str(), due[0].due_at, &due[0].doc), ("retry:r1", soon, &json!({ "run": "r1" })));
    assert_eq!(store.timers_next().unwrap(), Some(soon));
    assert!(!store.timers_delete("retry:r1", Some(soon - 1)).unwrap(), "moved meanwhile: kept");
    assert!(store.timers_delete("retry:r1", Some(soon)).unwrap());
    assert!(store.timers_due(10).unwrap().is_empty());
    assert_eq!(store.timers_next().unwrap(), Some(later));
    // Moving a timer replaces its due time and document.
    store.timers_set("retry:r2", 0, &json!({ "run": "r2", "attempt": 2 })).unwrap();
    assert_eq!(store.timers_due(10).unwrap()[0].doc["attempt"], 2);
    assert!(store.timers_delete("retry:r2", None).unwrap());
    assert_eq!(store.timers_next().unwrap(), None);
}

/// A write fenced with an old epoch is refused and writes nothing; the
/// current holder's commits.
fn fenced_writes(o: &Ontology) {
    let store = o.store().clone();
    assert_eq!(store.lease("automation:consumer", "old", 100).unwrap(), Some(1));
    let old = Fence::new("automation:consumer", "old", 1);
    o.apply_fenced(&ctx(), &old, vec![issue("1")]).unwrap();
    std::thread::sleep(Duration::from_millis(250));
    // Expired, not yet taken: the old holder's fence is already refused.
    let (seq, head) = (o.seq(), store.head().unwrap());
    lost(o.apply_fenced(&ctx(), &old, vec![issue("2")]));
    assert_eq!(store.lease("automation:consumer", "new", 60_000).unwrap(), Some(2));
    let new = Fence::new("automation:consumer", "new", 2);
    let m = lost(o.apply_fenced(&ctx(), &old, vec![issue("2")]));
    assert!(m.contains("epoch 1"), "{m}");
    assert_eq!(o.seq(), seq, "nothing committed");
    assert!(o.get(&Principal::admin("ada"), "issue:2").is_err(), "nothing applied");
    assert_eq!(store.head().unwrap(), head, "the store's head did not move");
    // Even the right holder with a stale epoch number is refused.
    lost(o.apply_fenced(&ctx(), &Fence::new("automation:consumer", "new", 1), vec![issue("2")]));
    o.apply_fenced(&ctx(), &new, vec![issue("2")]).unwrap();
    assert!(o.get(&Principal::admin("ada"), "issue:2").is_ok());
}

/// Releasing is fenced: a holder whose epoch is stale (the lease went to
/// another holder and came back to it) cannot give back the newer epoch;
/// a lease released by its holder is taken by the next one at once, without
/// waiting out its TTL; and the consumer cursor the old holder moved stays.
fn release_is_fenced(a: &dyn Store, b: &dyn Store) {
    let e1 = a.lease("r", "a", 60_000).unwrap().unwrap();
    let stale = Fence::new("r", "a", e1);
    a.consumer_set("automation:r", Cursor::new(4, 2), Some(&stale)).unwrap();
    assert!(a.release(&stale).unwrap(), "the holder at its epoch releases");
    // A second instance takes it at once (the TTL was a minute).
    let e2 = b.lease("r", "b", 60_000).unwrap().expect("released: taken at once");
    assert_eq!(e2, e1 + 1, "a new holder: the epoch grows");
    assert_eq!(b.consumer_get("automation:r").unwrap(), Some(Cursor::new(4, 2)), "the cursor resumes");
    assert!(b.release(&Fence::new("r", "b", e2)).unwrap());
    let e3 = a.lease("r", "a", 60_000).unwrap().unwrap();
    assert_eq!(e3, e2 + 1);
    // `a` again, at a newer epoch: its stale fence releases nothing.
    assert!(!a.release(&stale).unwrap(), "a stale holder cannot release a newer epoch");
    assert!(!b.release(&Fence::new("r", "b", e2)).unwrap(), "nor can the previous holder");
    let row = a.lease_get("r").unwrap().unwrap();
    assert!(row.held && row.holder == "a" && row.epoch == e3, "{row:?}");
    assert_eq!(b.lease("r", "b", 60_000).unwrap(), None, "still held");
    // An expired lease is not released (it is free already, maybe taken).
    assert_eq!(a.lease("x", "a", 50).unwrap(), Some(1));
    std::thread::sleep(Duration::from_millis(150));
    assert!(!a.release(&Fence::new("x", "a", 1)).unwrap());
    assert!(a.release(&Fence::new("r", "a", e3)).unwrap());
}

#[test]
fn memory_leases_follow_the_protocol() {
    protocol(&MemoryStore::default());
    timers(&MemoryStore::default());
    let m = MemoryStore::default();
    assert_eq!(m.wake_get().unwrap(), None, "memory tenants never close: no wake rows");
    assert!(m.wake_settle(0, Some(1)).unwrap());
}

#[test]
fn memory_release_is_fenced_and_hands_over_at_once() {
    let m = MemoryStore::default();
    release_is_fenced(&m, &m);
}

#[test]
fn memory_fenced_writes_are_refused_after_a_handover() {
    fenced_writes(&Ontology::in_memory(Schema::enterprise()));
}

/// Only the required methods: no leases, no fencing.
struct Plain(MemoryStore);

impl Store for Plain {
    fn load(&self) -> onto_core::Result<Snapshot> {
        self.0.load()
    }
    fn write(&self, batch: &WriteBatch) -> onto_core::Result<u64> {
        self.0.write(batch)
    }
    fn events(&self, filter: &EventFilter) -> onto_core::Result<Vec<Event>> {
        self.0.events(filter)
    }
    fn kv_put(&self, ns: &str, key: &str, value: &Value, expires_at: Option<i64>) -> onto_core::Result<()> {
        self.0.kv_put(ns, key, value, expires_at)
    }
    fn kv_get(&self, ns: &str, key: &str) -> onto_core::Result<Option<Value>> {
        self.0.kv_get(ns, key)
    }
    fn kv_delete(&self, ns: &str, key: &str) -> onto_core::Result<()> {
        self.0.kv_delete(ns, key)
    }
    fn kv_scan(&self, ns: &str, prefix: &str) -> onto_core::Result<Vec<(String, Value)>> {
        self.0.kv_scan(ns, prefix)
    }
    fn stats(&self) -> onto_core::Result<StoreStats> {
        self.0.stats()
    }
    fn versions(&self, id: &str, limit: usize) -> onto_core::Result<Vec<VersionRecord>> {
        self.0.versions(id, limit)
    }
    fn versions_since(&self, since: i64, limit: usize) -> onto_core::Result<Vec<VersionRecord>> {
        self.0.versions_since(since, limit)
    }
}

#[test]
fn a_store_that_cannot_fence_refuses_fenced_writes() {
    let o = Ontology::open(Schema::enterprise(), Arc::new(Plain(MemoryStore::default())), OntologyOptions::default())
        .unwrap();
    let e = o.apply_fenced(&ctx(), &Fence::new("x", "h", 1), vec![issue("1")]).unwrap_err();
    assert!(matches!(&e, Error::Unavailable(m) if m.contains("cannot fence")), "{e}");
    assert!(o.get(&Principal::admin("ada"), "issue:1").is_err());
    assert!(matches!(o.store().lease("x", "h", 1000), Err(Error::Unavailable(_))), "no leases: refused, not faked");
    o.apply(&ctx(), vec![issue("1")]).unwrap();
}

#[test]
fn pg_leases_follow_the_protocol() {
    let Some(pg) = common::TestTenant::new("leases_protocol") else { return };
    let store = pg.store();
    let applied = store.migrations().unwrap();
    assert!(applied.iter().any(|(v, n, _)| *v == 10 && n == "leases"), "{applied:?}");
    protocol(&store);
    timers(&store);
}

#[test]
fn pg_release_is_fenced_and_hands_over_at_once() {
    let Some(pg) = common::TestTenant::new("leases_release") else { return };
    // Two stores on one tenant: two processes' worth.
    let (a, b) = (pg.store(), pg.store());
    release_is_fenced(&a, &b);
}

#[test]
fn pg_fenced_writes_are_refused_after_a_handover() {
    let Some(pg) = common::TestTenant::new("leases_fence") else { return };
    let o = pg.open(Schema::enterprise(), OntologyOptions::default());
    fenced_writes(&o);
    // Another process (its own pool) sees only the fenced writes that committed.
    let other = pg.open(Schema::enterprise(), OntologyOptions::default());
    assert!(other.get(&Principal::admin("ada"), "issue:1").is_ok());
    assert!(other.get(&Principal::admin("ada"), "issue:2").is_ok());
    let versions = other.history(&Principal::admin("ada"), "issue:2", 10).unwrap();
    assert_eq!(versions.len(), 1, "the refused attempts wrote no version: {versions:?}");
}

/// Two holders, each in its own process (its own pool and runtime), with
/// process clocks skewed by -5 and +5 minutes. A holder only ever reasons
/// with its own clock; the store only with its own. The holder that thinks
/// the other's lease expired long ago is still refused, and while both race
/// for the lease (taking it, renewing it, pausing past its expiry and then
/// writing as if they still held it) no epoch is ever held by both, and
/// every write fenced by an epoch its holder no longer has is refused.
#[test]
fn pg_two_holders_with_skewed_clocks_never_both_hold_a_lease() {
    let Some(pg) = common::TestTenant::new("leases_skew") else { return };
    let (a, b) = (Arc::new(pg.store()), Arc::new(pg.store()));
    const SKEW_A: i64 = -5 * 60_000;
    const SKEW_B: i64 = 5 * 60_000;
    let ttl = 400;

    // A takes the lease and notes, by its clock, when it should end.
    let a_epoch = a.lease("fanout", "a", 60_000).unwrap().unwrap();
    let a_thinks_until = now_ms() + SKEW_A + 60_000;
    // The store set the end by its own clock.
    let row = a.lease_get("fanout").unwrap().unwrap();
    let db_now = a.clock_ms().unwrap();
    assert!((row.until - db_now - 60_000).abs() < 5_000, "until {} vs database now {db_now}", row.until);
    // B's clock says A's lease ended minutes ago; the database says no.
    assert!(now_ms() + SKEW_B > a_thinks_until, "B believes the lease is free");
    assert_eq!(b.lease("fanout", "b", 60_000).unwrap(), None);
    lost(b.consumer_set("webhook:x", Cursor::new(1, 0), Some(&Fence::new("fanout", "b", a_epoch))));
    assert!(a.release(&Fence::new("fanout", "a", a_epoch)).unwrap());

    let race = |store: Arc<dyn Store>, me: &'static str, skew: i64, seed: u64| {
        std::thread::spawn(move || {
            let mut rng = fastrand::Rng::with_seed(seed);
            let mut held: Vec<u64> = Vec::new();
            let mut last: Option<Fence> = None;
            let (mut refused, mut paused) = (0, 0);
            let end = Instant::now() + Duration::from_secs(4);
            while Instant::now() < end {
                let Some(epoch) = store.lease("fanout", me, ttl).unwrap() else {
                    std::thread::sleep(Duration::from_millis(rng.u64(5..40)));
                    continue;
                };
                let fence = Fence::new("fanout", me, epoch);
                last = Some(fence.clone());
                // What this holder believes, by its own clock. A constant
                // skew cancels out within one process; across processes it
                // never enters, because the store is given no client time.
                let believed_until = now_ms() + skew + ttl;
                match store.consumer_set("webhook:x", Cursor::new(epoch, 0), Some(&fence)) {
                    Ok(()) => held.push(epoch),
                    // Only when the database was slower than the TTL.
                    Err(Error::Unavailable(m)) if m.starts_with("lease lost") => refused += 1,
                    Err(e) => panic!("{e}"),
                }
                if rng.u8(..10) < 3 {
                    // Paused past the lease's end (as a stopped process would
                    // be); whatever it believes on waking, the fence decides.
                    paused += 1;
                    std::thread::sleep(Duration::from_millis(ttl as u64 + 250));
                    assert!(now_ms() + skew > believed_until);
                    let r = store.consumer_set("webhook:x", Cursor::new(epoch, 1), Some(&fence));
                    lost(r);
                    refused += 1;
                } else {
                    std::thread::sleep(Duration::from_millis(rng.u64(10..60)));
                }
            }
            if let Some(f) = &last {
                let _ = store.release(f);
            }
            (me, held, refused, paused)
        })
    };
    let ta = race(a.clone(), "a", SKEW_A, 1);
    let tb = race(b.clone(), "b", SKEW_B, 2);
    let mut owner: BTreeMap<u64, &str> = BTreeMap::new();
    let mut pauses = 0;
    for t in [ta, tb] {
        let (me, held, refused, paused) = t.join().unwrap();
        assert!(refused >= paused, "{me}: every write after a pause was refused");
        pauses += paused;
        for epoch in held {
            if let Some(prev) = owner.insert(epoch, me) {
                assert_eq!(prev, me, "epoch {epoch} held by both a and b");
            }
        }
    }
    assert!(owner.values().any(|h| *h == "a") && owner.values().any(|h| *h == "b"), "both held it: {owner:?}");
    assert!(pauses > 0 && owner.len() > 2, "the lease changed hands: {owner:?}");
    // The cursor was last moved by the holder of the newest epoch it saw.
    let last = a.consumer_get("webhook:x").unwrap().unwrap();
    assert!(owner.contains_key(&last.seq) && last.n == 0, "{last:?}");
}

/// Timers bump the tenant's wake row; a worker that drained settles it only
/// if nothing bumped it since it looked (compare-and-set on `bumped`).
#[test]
fn pg_wake_settle_is_compare_and_set() {
    let Some(pg) = common::TestTenant::new("leases_wake") else { return };
    let shared = PgShared::connect(&pg.url, &PgOptions { listen: false, ..Default::default() }).unwrap();
    let store = shared.tenant(&pg.tenant, PgOptions { listen: false, ..Default::default() }).unwrap();
    assert_eq!(store.wake_get().unwrap(), None, "never bumped");
    assert!(!shared.wake_due("onto_", 10_000).unwrap().contains(&pg.tenant));

    store.timers_set("retry:r1", 0, &json!({})).unwrap();
    let w = store.wake_get().unwrap().unwrap();
    assert_eq!(w.bumped, 1);
    assert!(w.due_at.is_some_and(|d| d <= store.clock_ms().unwrap()));
    assert!(shared.wake_due("onto_", 10_000).unwrap().contains(&pg.tenant), "due by the database clock");

    // A worker read bumped = 1 and drains; meanwhile another write bumps.
    let seen = w.bumped;
    store.timers_set("retry:r2", 60_000, &json!({})).unwrap();
    let w2 = store.wake_get().unwrap().unwrap();
    assert_eq!((w2.bumped, w2.due_at), (2, w.due_at), "a later due time keeps the earlier one");
    assert!(!store.wake_settle(seen, None).unwrap(), "bumped since: not settled");
    assert_eq!(store.wake_get().unwrap().unwrap().due_at, w.due_at, "the bump is not lost");

    // The worker drains again, now from bumped = 2, and settles to the next timer.
    assert!(store.timers_delete("retry:r1", None).unwrap());
    let next = store.timers_next().unwrap();
    assert!(store.wake_settle(w2.bumped, next).unwrap());
    let w3 = store.wake_get().unwrap().unwrap();
    assert_eq!((w3.bumped, w3.due_at), (2, next));
    assert!(!shared.wake_due("onto_", 10_000).unwrap().contains(&pg.tenant), "not due for a minute");

    // A commit carrying a wake bumps in its own transaction.
    store.write(&WriteBatch { wake: Some(0), ..Default::default() }).unwrap();
    let w4 = store.wake_get().unwrap().unwrap();
    assert_eq!(w4.bumped, 3);
    assert!(w4.due_at.unwrap() < next.unwrap(), "earlier wins");
    assert!(store.wake_settle(3, None).unwrap());
    assert_eq!(store.wake_get().unwrap().unwrap().due_at, None);

    // A fenced write whose fence is lost bumps nothing either.
    let r = store.write(&WriteBatch { wake: Some(0), fence: Some(Fence::new("nobody", "h", 1)), ..Default::default() });
    lost(r);
    assert_eq!(store.wake_get().unwrap().unwrap().bumped, 3);

    // Dropping the tenant forgets its wake row, even a due one.
    store.write(&WriteBatch { wake: Some(0), ..Default::default() }).unwrap();
    assert!(shared.wake_due("onto_", 10_000).unwrap().contains(&pg.tenant));
    shared.drop_tenant("onto_", &pg.tenant).unwrap();
    assert!(!shared.wake_due("onto_", 10_000).unwrap().contains(&pg.tenant));
}

/// Review CP-5: a tenant named `sys` by an earlier version owns the schema
/// the shared `wake` table now lives in. The new build refuses to put the
/// table inside that tenant's data and says how to rename it; the tenant id
/// itself is refused with a pointer to the same note.
#[test]
fn pg_shared_schema_refuses_an_old_tenant_of_that_name() {
    let Some(url) = common::database_url() else {
        eprintln!("skipped pg_shared_schema_refuses_an_old_tenant_of_that_name: set DATABASE_URL");
        return;
    };
    let prefix = format!("zzsys{}_", &onto_core::time::new_id("x")[2..10]).to_ascii_lowercase();
    let sys = onto_core::store::postgres::sys_schema(&prefix);
    let sql = |body: String| {
        let url = url.clone();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let pool = onto_core::sqlx::PgPool::connect(&url).await.unwrap();
            onto_core::sqlx::raw_sql(onto_core::sqlx::AssertSqlSafe(body)).execute(&pool).await.unwrap();
            pool.close().await;
        })
    };
    // What a tenant "sys" created by an earlier version looks like.
    sql(format!(
        "CREATE SCHEMA \"{sys}\"; CREATE TABLE \"{sys}\".head (id INT PRIMARY KEY, seq BIGINT NOT NULL); \
         CREATE TABLE \"{sys}\".tenant_identity (one BOOLEAN PRIMARY KEY, tenant TEXT NOT NULL, claimed_at BIGINT NOT NULL)"
    ));
    let shared = PgShared::connect(&url, &PgOptions { listen: false, ..Default::default() }).unwrap();
    let err = shared.ensure_sys(&prefix).unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, Error::Config(_)) && msg.contains("RENAME") && msg.contains(&sys), "{msg}");
    let opts = PgOptions { listen: false, schema_prefix: prefix.clone(), ..Default::default() };
    let err = shared.tenant("sys", opts).err().unwrap().to_string();
    assert!(err.contains("reserved") && err.contains("STORAGE.md"), "{err}");
    sql(format!("DROP SCHEMA \"{sys}\" CASCADE"));
    // A fresh deployment creates it.
    assert_eq!(shared.ensure_sys(&prefix).unwrap(), sys);
    sql(format!("DROP SCHEMA \"{sys}\" CASCADE"));
}
