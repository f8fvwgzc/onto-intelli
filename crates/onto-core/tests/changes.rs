//! The change feed on the in-memory store and on PostgreSQL (`pg`, when a
//! test database is set; see `common`): records commit with their writes,
//! readers see what they may, gaps and the floor say what is missing, and
//! replaying the feed reaches later snapshots.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use onto_core::changes::{ChangesMeta, GapReason, StoredPage};
use onto_core::graph::LinkKey;
use onto_core::store::{MemoryStore, Snapshot, Store, StoreStats, VersionRecord, WriteBatch};
use onto_core::{
    ActionContext, ChangeFilter, ChangeOp, ChangePage, ChangeRecord, Cursor, Entity, EntityInput, Error, Event,
    EventFilter, FieldMode, LinkInput, Mutation, Ontology, OntologyOptions, Principal, Role, RunCause, Schema,
    SchemaChangeSet,
};

fn admin() -> Principal {
    Principal::admin("ada")
}

fn viewer() -> Principal {
    Principal::new("user:v", vec![Role::Viewer], vec!["ws1".into()])
}

fn ctx(ws: &str) -> ActionContext {
    ActionContext::new(admin(), ws)
}

fn issue(key: &str, ws: &str) -> EntityInput {
    let mut e = EntityInput::new("issue", key, format!("Issue {key}"));
    e.workspace = Some(ws.into());
    e
}

/// Every record after `after` that `p` gets, across pages.
fn feed(o: &Ontology, p: &Principal, after: Cursor, f: &ChangeFilter) -> Vec<ChangeRecord> {
    let mut out = Vec::new();
    let mut cur = after;
    loop {
        let page = o.changes_page(p, cur, f, 50).unwrap();
        assert!(!page.expired, "cursor {cur} expired");
        out.extend(page.changes);
        if page.next == cur {
            return out;
        }
        cur = page.next;
    }
}

fn ops(records: &[ChangeRecord]) -> Vec<(ChangeOp, String)> {
    records.iter().map(|r| (r.op, r.id.clone())).collect()
}

#[test]
fn records_commit_only_with_their_commit() {
    records_commit_only_with_their_commit_on(&Ontology::in_memory(Schema::enterprise()));
}

fn records_commit_only_with_their_commit_on(o: &Ontology) {
    let head = o.change_head().unwrap();
    let member = ActionContext::new(Principal::member("mia", "ws1"), "ws1");
    let err = o
        .apply(
            &member,
            vec![
                Mutation::Upsert(issue("1", "ws1")),
                // Denied: the member has no access to ws2.
                Mutation::Upsert(issue("2", "ws2")),
            ],
        )
        .unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    assert_eq!(o.change_head().unwrap(), head);
    assert!(feed(o, &admin(), head, &ChangeFilter::default()).is_empty());

    o.apply(&member, vec![Mutation::Upsert(issue("1", "ws1").state("open"))]).unwrap();
    let records = feed(o, &admin(), head, &ChangeFilter::default());
    assert_eq!(ops(&records), vec![(ChangeOp::EntityCreate, "issue:1".into())]);
    let r = &records[0];
    assert_eq!(r.cursor.seq, o.seq());
    assert_eq!(r.entity.as_ref().unwrap().state.as_deref(), Some("open"));
    assert_eq!(r.cause.actor, "mia");
    assert_eq!(o.change_head().unwrap(), r.cursor);
}

#[test]
fn readers_see_what_they_may() {
    readers_see_what_they_may_on(&Ontology::in_memory(Schema::enterprise()));
}

fn readers_see_what_they_may_on(o: &Ontology) {
    let head = o.change_head().unwrap();
    o.apply(
        &ctx("ws1"),
        vec![
            Mutation::Upsert(issue("1", "ws1").prop("priority", "low")),
            Mutation::Upsert(issue("2", "ws1").prop("visibility", "restricted").prop("acl", json!(["user:x"]))),
            Mutation::Upsert(issue("3", "ws2")),
        ],
    )
    .unwrap();
    let at = o.change_head().unwrap();
    o.apply(
        &ctx("ws1"),
        vec![
            Mutation::Upsert(issue("1", "ws1").prop("priority", "high")),
            Mutation::Upsert(issue("2", "ws1").prop("salary", 200).prop("visibility", "public")),
            Mutation::Link(LinkInput::new("issue:1", "depends_on", "issue:2")),
        ],
    )
    .unwrap();

    let all = feed(o, &admin(), head, &ChangeFilter::default());
    assert_eq!(all.len(), 6);
    let seen = feed(o, &viewer(), head, &ChangeFilter::default());
    assert_eq!(
        ops(&seen),
        vec![
            (ChangeOp::EntityCreate, "issue:1".into()),
            (ChangeOp::EntityUpdate, "issue:1".into()),
            (ChangeOp::EntityUpdate, "issue:2".into()),
            (ChangeOp::LinkCreate, "issue:1|depends_on|issue:2".into()),
        ]
    );
    assert_eq!(seen[1].before["priority"], json!("low"));
    let flipped = &seen[2];
    assert!(flipped.became_visible && flipped.before.is_empty(), "{flipped:?}");
    assert_eq!(flipped.entity.as_ref().unwrap().props["salary"], json!(200));

    // A filter and ids-only payloads.
    let f = ChangeFilter { props: vec!["priority".into()], fields: FieldMode::Ids, ..Default::default() };
    let only = feed(o, &viewer(), at, &f);
    assert_eq!(
        ops(&only),
        vec![(ChangeOp::EntityUpdate, "issue:1".into()), (ChangeOp::LinkCreate, "issue:1|depends_on|issue:2".into())]
    );
    assert!(only.iter().all(|r| r.entity.is_none() && r.before.is_empty()));

    // `next` advances past what the reader may not see.
    let page = o.changes_page(&Principal::viewer("w", "ws3"), head, &ChangeFilter::default(), 100).unwrap();
    assert!(page.changes.is_empty());
    assert_eq!(page.next, o.change_head().unwrap());
    assert!(o.changes_page(&Principal::new("x", vec![], vec!["ws1".into()]), head, &f, 10).is_err());
}

#[test]
fn schema_changes_carry_renames_and_migrations_are_marked() {
    schema_changes_carry_renames_and_migrations_are_marked_on(&Ontology::in_memory(Schema::enterprise()));
}

fn schema_changes_carry_renames_and_migrations_are_marked_on(o: &Ontology) {
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(EntityInput::new("task", "3", "Write docs"))]).unwrap();
    let head = o.change_head().unwrap();
    let set: SchemaChangeSet =
        serde_json::from_value(json!({ "changes": [ { "op": "rename_object_type", "from": "task", "to": "chore" } ] }))
            .unwrap();
    o.schema_apply(&ctx("ws1"), set).unwrap();
    let records = feed(o, &admin(), head, &ChangeFilter::default());
    let schema = records.iter().find(|r| r.op == ChangeOp::SchemaChange).expect("a schema.change record");
    let mv = schema.schema.as_ref().unwrap();
    assert_eq!(mv.version, o.schema_version());
    assert_eq!(serde_json::to_value(&mv.renames).unwrap(), json!([{ "what": "kind", "from": "task", "to": "chore" }]));
    let migrated: Vec<&ChangeRecord> = records.iter().filter(|r| r.op != ChangeOp::SchemaChange).collect();
    assert!(!migrated.is_empty());
    assert!(migrated.iter().all(|r| r.cause.migration), "{migrated:?}");
    assert!(migrated.iter().any(|r| r.op == ChangeOp::EntityCreate && r.id == "chore:3"));
    // Automations leave migrations out but still see the schema change.
    let f = ChangeFilter { include_migrations: false, ..Default::default() };
    assert_eq!(ops(&feed(o, &admin(), head, &f)), vec![(ChangeOp::SchemaChange, schema.id.clone())]);
}

#[test]
fn run_cause_and_listener_seq() {
    run_cause_and_listener_seq_on(&Ontology::in_memory(Schema::enterprise()));
}

fn run_cause_and_listener_seq_on(o: &Ontology) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    o.on_change(move |c| s.lock().push(c.seq));
    let head = o.change_head().unwrap();
    let mut c = ctx("ws1").with_source("automation:triage");
    c.cause = Some(RunCause { automation: "triage".into(), run: "run_7".into(), depth: 1 });
    o.apply(&c, vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
    assert_eq!(*seen.lock(), vec![o.seq()]);
    let r = &feed(o, &admin(), head, &ChangeFilter::default())[0];
    assert_eq!(
        (r.cause.automation.as_deref(), r.cause.run.as_deref(), r.cause.depth),
        (Some("triage"), Some("run_7"), 1)
    );
    let f = ChangeFilter { sources: vec!["!automation:*".into()], ..Default::default() };
    assert!(feed(o, &admin(), head, &f).is_empty());
}

#[test]
fn forks_record_nothing() {
    forks_record_nothing_on(&Ontology::in_memory(Schema::enterprise()));
}

fn forks_record_nothing_on(o: &Ontology) {
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
    let head = o.change_head().unwrap();
    let fork = o.fork();
    assert!(!fork.capture_enabled());
    let fork_head = fork.change_head().unwrap();
    fork.apply(
        &ctx("ws1"),
        vec![Mutation::Upsert(issue("2", "ws1")), Mutation::Delete(onto_core::EntityRef { id: "issue:1".into() })],
    )
    .unwrap();
    assert!(feed(&fork, &admin(), fork_head, &ChangeFilter::default()).is_empty(), "no records, no gaps");
    assert_eq!(o.change_head().unwrap(), head);
}

fn only_gap(records: &[ChangeRecord]) -> &onto_core::changes::Gap {
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].op, ChangeOp::Gap);
    records[0].gap.as_ref().unwrap()
}

#[test]
fn uncaptured_writes_become_gaps() {
    let store = Arc::new(MemoryStore::default());
    let o = Ontology::open(Schema::enterprise(), store.clone(), OntologyOptions::default()).unwrap();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();

    // A writer around the engine.
    let head = o.change_head().unwrap();
    let e = o.get(&admin(), "issue:1").unwrap();
    store.write(&WriteBatch { put_entities: vec![e], ..Default::default() }).unwrap();
    let records = feed(&o, &admin(), head, &ChangeFilter { kinds: vec!["service".into()], ..Default::default() });
    let gap = only_gap(&records);
    assert_eq!(gap.reason, GapReason::UncapturedWriter);
    assert_eq!(gap.entities, vec!["issue:1"]);
    // The viewer of another workspace learns that something changed, not what.
    let other = feed(&o, &Principal::viewer("w", "ws3"), head, &ChangeFilter::default());
    let g = only_gap(&other);
    assert!(g.entities.is_empty());
    assert_eq!(g.hidden, 1);

    // KV-only writes are not the feed's business.
    let head = o.change_head().unwrap();
    store.write(&WriteBatch { kv: vec![("memory".into(), "k".into(), json!(1))], ..Default::default() }).unwrap();
    assert!(feed(&o, &admin(), head, &ChangeFilter::default()).is_empty());

    // Bulk imports say so.
    let head = o.change_head().unwrap();
    let mut x = o.get(&admin(), "issue:1").unwrap();
    x.id = "issue:9".into();
    store.bulk_import(&[x], &[]).unwrap();
    let records = feed(&o, &admin(), head, &ChangeFilter::default());
    assert_eq!(only_gap(&records).reason, GapReason::BulkImport);
}

/// A store with only the required methods and the change log of a memory
/// store: its `bulk_import` is the trait's default.
struct Plain(MemoryStore);

impl Store for Plain {
    fn load(&self) -> onto_core::Result<Snapshot> {
        self.0.load()
    }
    fn write(&self, batch: &WriteBatch) -> onto_core::Result<u64> {
        self.0.write(batch)
    }
    fn keeps_changes(&self) -> bool {
        true
    }
    fn changes_since(&self, after: Cursor, limit: usize) -> onto_core::Result<StoredPage> {
        self.0.changes_since(after, limit)
    }
    fn changes_meta(&self) -> onto_core::Result<ChangesMeta> {
        self.0.changes_meta()
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
fn the_default_bulk_import_yields_gaps() {
    let store = Arc::new(Plain(MemoryStore::default()));
    let o = Ontology::open(Schema::enterprise(), store.clone(), OntologyOptions::default()).unwrap();
    assert!(o.capture_enabled());
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
    let head = o.change_head().unwrap();
    let e = o.get(&admin(), "issue:1").unwrap();
    store.bulk_import(&[e], &[]).unwrap();
    let records = feed(&o, &admin(), head, &ChangeFilter::default());
    let gap = only_gap(&records);
    assert_eq!(gap.reason, GapReason::UncapturedWriter, "the default import writes plain batches");
    assert_eq!(gap.entities, vec!["issue:1"]);
}

#[test]
fn expired_cursor_at_the_retention_edge() {
    let store = Arc::new(MemoryStore::default().with_change_capacity(4));
    let o = Ontology::open(Schema::enterprise(), store, OntologyOptions::default()).unwrap();
    let lease = || assert!(o.try_lease("job", "me", 60_000).unwrap());
    let s1 = o.seq(); // the schema bootstrap: one record
    assert_eq!(o.change_head().unwrap(), Cursor::new(s1, 0));
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1")), Mutation::Upsert(issue("2", "ws1"))]).unwrap();
    let s2 = o.seq();
    lease(); // a commit without records
    let s3 = o.seq();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("3", "ws1")), Mutation::Upsert(issue("4", "ws1"))]).unwrap();
    let s4 = o.seq();
    lease();
    let s5 = o.seq();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("5", "ws1"))]).unwrap();
    let s6 = o.seq();
    assert_eq!((s2, s3, s4, s5, s6), (s1 + 1, s1 + 2, s1 + 3, s1 + 4, s1 + 5));

    // Capacity 4: the records of s1 and s2 were evicted; the floor is s3,
    // a seq without records, with another such seq (s5) above it.
    let meta = o.store().changes_meta().unwrap();
    assert_eq!(meta.floor, s3);
    assert_eq!(meta.head, Cursor::new(s6, 0));

    let page =
        |after: Cursor| -> ChangePage { o.changes_page(&admin(), after, &ChangeFilter::default(), 100).unwrap() };
    for old in [Cursor::default(), Cursor::new(s1, 0), Cursor::new(s2, 0)] {
        let p = page(old);
        assert!(p.expired && p.changes.is_empty(), "{old}");
        assert_eq!(p.floor, s3);
    }
    // Fully caught up on s2 = floor - 1: still reported expired (a false
    // reset at the exact edge, never a silent gap).
    assert!(page(Cursor::new(s2, 1)).expired);
    let at_floor = page(Cursor::new(s3, 0));
    assert!(!at_floor.expired);
    assert_eq!(
        at_floor.changes.iter().map(|r| r.cursor).collect::<Vec<_>>(),
        vec![Cursor::new(s4, 0), Cursor::new(s4, 1), Cursor::new(s6, 0)]
    );
    let above = page(Cursor::new(s5, 0));
    assert_eq!(ops(&above.changes), vec![(ChangeOp::EntityCreate, "issue:5".into())]);
    assert_eq!(above.head, s6);
}

#[test]
fn retention_removes_whole_commits_below_the_head() {
    let store = Arc::new(MemoryStore::default());
    let o = Ontology::open(Schema::enterprise(), store.clone(), OntologyOptions::default()).unwrap();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1")), Mutation::Upsert(issue("2", "ws1"))]).unwrap();
    let s1 = o.seq();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("3", "ws1"))]).unwrap();
    let s2 = o.seq();
    let keep = |keep_ms, keep_rows| onto_core::changes::ChangeRetention { keep_ms, keep_rows };
    let v = store.vacuum_changes(&keep(None, Some(2))).unwrap();
    // Two kept rows would split s1: the whole of s1 goes.
    assert_eq!(v.floor, s2);
    assert_eq!(store.changes_since(Cursor::default(), 10).unwrap().changes.len(), 1);
    // Everything older than now: the head commit's records stay.
    let v = store.vacuum_changes(&keep(Some(-60_000), None)).unwrap();
    assert_eq!((v.deleted, v.floor), (0, s2));
    assert!(o.changes_page(&admin(), Cursor::new(s1, 1), &ChangeFilter::default(), 10).unwrap().expired);
    assert!(!o.changes_page(&admin(), o.change_head().unwrap(), &ChangeFilter::default(), 10).unwrap().expired);
}

#[test]
fn earliest_starts_at_the_floor() {
    earliest_starts_at_the_floor_on(&Ontology::in_memory(Schema::enterprise()));
}

fn earliest_starts_at_the_floor_on(o: &Ontology) {
    let f = ChangeFilter::default();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("2", "ws1")), Mutation::Upsert(issue("3", "ws1"))]).unwrap();
    let s2 = o.seq();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("4", "ws1"))]).unwrap();
    // Keep three records: the floor is s2, and s2's records stay.
    let keep = onto_core::changes::ChangeRetention { keep_ms: None, keep_rows: Some(3) };
    assert_eq!(o.store().vacuum_changes(&keep).unwrap().floor, s2);
    let first = o.changes_page_earliest(&admin(), &f, 100).unwrap();
    assert!(!first.expired);
    assert_eq!(
        ops(&first.changes),
        vec![
            (ChangeOp::EntityCreate, "issue:2".into()),
            (ChangeOp::EntityCreate, "issue:3".into()),
            (ChangeOp::EntityCreate, "issue:4".into())
        ]
    );
    assert_eq!(first.next, o.change_head().unwrap());
    // The same position as a cursor is below the floor (the edge reset).
    assert!(o.changes_page(&admin(), Cursor::new(s2 - 1, u32::MAX), &f, 100).unwrap().expired);
    // Paging from earliest in small pages reaches the same records.
    let small = o.changes_page_earliest(&admin(), &f, 1).unwrap();
    assert_eq!(ops(&small.changes), vec![(ChangeOp::EntityCreate, "issue:2".into())]);
    assert_eq!(
        ops(&feed(o, &admin(), small.next, &f)),
        vec![(ChangeOp::EntityCreate, "issue:3".into()), (ChangeOp::EntityCreate, "issue:4".into())]
    );

    // Capture off, a write, capture on: the floor is the head commit, whose
    // only record is the capture_enabled gap. Earliest gets that gap and
    // resumes from the head without a reset.
    o.set_capture(&ctx("ws1"), false).unwrap();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("5", "ws1"))]).unwrap();
    o.set_capture(&ctx("ws1"), true).unwrap();
    let first = o.changes_page_earliest(&admin(), &f, 100).unwrap();
    assert!(!first.expired, "{first:?}");
    assert_eq!(only_gap(&first.changes).reason, GapReason::CaptureEnabled);
    assert_eq!(first.next, o.change_head().unwrap());
    assert!(!o.changes_page(&admin(), first.next, &f, 100).unwrap().expired);
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("6", "ws1"))]).unwrap();
    assert_eq!(ops(&feed(o, &admin(), first.next, &f)), vec![(ChangeOp::EntityCreate, "issue:6".into())]);
    // A reader without a read role is refused.
    assert!(o.changes_page_earliest(&Principal::new("x", vec![], vec!["ws1".into()]), &f, 10).is_err());
}

#[test]
fn capture_off_then_on_expires_old_cursors() {
    capture_off_then_on_expires_old_cursors_on(&Ontology::in_memory(Schema::enterprise()));
}

fn capture_off_then_on_expires_old_cursors_on(o: &Ontology) {
    assert!(o.capture_enabled());
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
    let before = o.change_head().unwrap();

    for who in
        [Principal::member("mia", "ws1"), Principal::new("bot", vec![Role::Admin, Role::Agent], vec!["*".into()])]
    {
        let err = o.set_capture(&ActionContext::new(who, "ws1"), false).unwrap_err();
        assert!(matches!(err, Error::Denied(_)), "{err:?}");
    }
    let scoped = Principal::new("wsadmin", vec![Role::Admin], vec!["ws1".into()]);
    assert!(o.set_capture(&ActionContext::new(scoped, "ws1"), false).is_err());

    o.set_capture(&ctx("ws1"), false).unwrap();
    assert!(!o.capture_enabled());
    assert!(!o.store().changes_meta().unwrap().capture);
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("2", "ws1"))]).unwrap();
    assert!(feed(o, &admin(), before, &ChangeFilter::default()).is_empty(), "nothing recorded, no gap");
    let during = o.change_head().unwrap();

    o.set_capture(&ctx("ws1"), true).unwrap();
    assert!(o.capture_enabled());
    let p = o.changes_page(&admin(), before, &ChangeFilter::default(), 10).unwrap();
    assert!(p.expired, "a write went unrecorded after this cursor");
    // A cursor at the head while capture was off cannot tell whether writes
    // came after it unrecorded: it is not expired, but gets the
    // capture_enabled gap first.
    assert!(!o.changes_page(&admin(), during, &ChangeFilter::default(), 10).unwrap().expired);
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("3", "ws1"))]).unwrap();
    let after_on = feed(o, &admin(), during, &ChangeFilter::default());
    assert_eq!(ops(&after_on), vec![(ChangeOp::Gap, String::new()), (ChangeOp::EntityCreate, "issue:3".into())]);
    assert_eq!(after_on[0].gap.as_ref().unwrap().reason, GapReason::CaptureEnabled);
    // Turning it on again while on changes nothing.
    let head = o.change_head().unwrap();
    o.set_capture(&ctx("ws1"), true).unwrap();
    assert_eq!(o.change_head().unwrap(), head);
    let audit =
        o.events(&admin(), &EventFilter { action: Some("changes.capture".into()), ..Default::default() }).unwrap();
    assert_eq!(audit.len(), 3, "off, on, on again: each audited");
}

/// Review C1: a reader that keeps polling while capture is off is moved to
/// the head by empty pages; turning capture on again must not let it skip
/// the unrecorded commits silently.
#[test]
fn polling_through_capture_off_never_skips_silently() {
    polling_through_capture_off_never_skips_silently_on(&Ontology::in_memory(Schema::enterprise()));
}

fn polling_through_capture_off_never_skips_silently_on(o: &Ontology) {
    let f = ChangeFilter::default();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
    let mut cursor = o.change_head().unwrap();
    let stale = cursor; // a reader that stops polling here
    o.set_capture(&ctx("ws1"), false).unwrap();
    for n in 2..5 {
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue(&n.to_string(), "ws1"))]).unwrap();
        let page = o.changes_page(&admin(), cursor, &f, 100).unwrap();
        assert!(!page.expired && page.changes.is_empty(), "{page:?}");
        cursor = page.next; // moved to the head over an unrecorded commit
    }
    assert_eq!(cursor.seq, o.seq());
    o.set_capture(&ctx("ws1"), true).unwrap();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("9", "ws1"))]).unwrap();

    let page = o.changes_page(&admin(), cursor, &f, 100).unwrap();
    assert!(!page.expired);
    assert_eq!(page.changes.len(), 2, "{page:?}");
    let gap = page.changes[0].gap.as_ref().expect("the capture_enabled gap comes first");
    assert_eq!(gap.reason, GapReason::CaptureEnabled);
    assert_eq!((page.changes[1].op, page.changes[1].id.as_str()), (ChangeOp::EntityCreate, "issue:9"));
    // Every reader of the tenant gets it, whatever its filter.
    let tasks = ChangeFilter { kinds: vec!["task".into()], ..Default::default() };
    let only = o.changes_page(&viewer(), cursor, &tasks, 100).unwrap();
    assert_eq!(ops(&only.changes), vec![(ChangeOp::Gap, String::new())]);
    // The reader that stopped before capture went off is expired.
    assert!(o.changes_page(&admin(), stale, &f, 100).unwrap().expired);
    // A reader that starts after capture is on sees no gap.
    let now = o.change_head().unwrap();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("10", "ws1"))]).unwrap();
    assert_eq!(ops(&feed(o, &admin(), now, &f)), vec![(ChangeOp::EntityCreate, "issue:10".into())]);
}

/// Review C2: a cursor ahead of the head (from before a restart, a
/// re-provisioned tenant or a restore) is expired, never silently served.
#[test]
fn a_cursor_ahead_of_the_head_is_expired() {
    let f = ChangeFilter::default();
    let old = Ontology::in_memory(Schema::enterprise());
    for n in 0..5 {
        old.apply(&ctx("ws1"), vec![Mutation::Upsert(issue(&n.to_string(), "ws1"))]).unwrap();
    }
    let held = old.change_head().unwrap();
    drop(old);
    let o = Ontology::in_memory(Schema::enterprise());
    assert!(o.change_head().unwrap() < held);
    let page = o.changes_page(&admin(), held, &f, 100).unwrap();
    assert!(page.expired && page.changes.is_empty(), "{page:?}");
    assert_eq!(page.next, held);
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("new1", "ws1"))]).unwrap();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("new2", "ws1"))]).unwrap();
    assert!(o.changes_page(&admin(), held, &f, 100).unwrap().expired, "still behind the held cursor");
    // Past the head commit's last record, too.
    let head = o.change_head().unwrap();
    assert!(o.changes_page(&admin(), Cursor::new(head.seq, head.n + 1), &f, 100).unwrap().expired);
    assert!(!o.changes_page(&admin(), head, &f, 100).unwrap().expired);
}

/// Review C3: evicting the change log never raises the floor above the
/// head, even when one commit holds more records than the capacity.
#[test]
fn eviction_keeps_the_newest_commit_and_the_head_valid() {
    let store = Arc::new(MemoryStore::default().with_change_capacity(2));
    let o = Ontology::open(Schema::enterprise(), store.clone(), OntologyOptions::default()).unwrap();
    let f = ChangeFilter::default();
    o.apply(
        &ctx("ws1"),
        vec![
            Mutation::Upsert(issue("1", "ws1")),
            Mutation::Upsert(issue("2", "ws1")),
            Mutation::Upsert(issue("3", "ws1")),
        ],
    )
    .unwrap();
    let s = o.seq();
    let meta = store.changes_meta().unwrap();
    assert!(meta.floor <= meta.head.seq, "{meta:?}");
    assert_eq!(meta.floor, s, "the older commit went");
    let head = o.change_head().unwrap();
    assert!(!o.changes_page(&admin(), head, &f, 10).unwrap().expired, "a reader at the head is not reset");
    let first = o.changes_page_earliest(&admin(), &f, 10).unwrap();
    assert!(!first.expired);
    assert_eq!(first.changes.len(), 3, "the newest commit is kept whole");
    assert!(!o.changes_page(&admin(), first.next, &f, 10).unwrap().expired);
    // The next commit evicts it.
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("4", "ws1"))]).unwrap();
    let meta = store.changes_meta().unwrap();
    assert_eq!(meta.floor, s + 1);
    assert!(o.changes_page(&admin(), first.next, &f, 10).unwrap().expired);
    let rest = o.changes_page_earliest(&admin(), &f, 10).unwrap();
    assert!(!rest.expired);
    assert_eq!(ops(&rest.changes), vec![(ChangeOp::EntityCreate, "issue:4".into())]);
}

/// Review SEC-2: filters too large to evaluate cheaply are refused.
#[test]
fn oversized_filters_are_refused() {
    use onto_core::changes::{MAX_FILTER_NAME_LEN, MAX_FILTER_NAMES, MAX_FILTER_OPS, MAX_FILTER_SOURCES};
    let o = Ontology::in_memory(Schema::enterprise());
    let names = |n: usize| (0..n).map(|i| format!("k{i}")).collect::<Vec<_>>();
    let ok = ChangeFilter { kinds: names(MAX_FILTER_NAMES), sources: names(MAX_FILTER_SOURCES), ..Default::default() };
    assert!(o.changes_page(&viewer(), Cursor::default(), &ok, 10).is_ok());
    let refused = [
        ChangeFilter { kinds: names(MAX_FILTER_NAMES + 1), ..Default::default() },
        ChangeFilter { workspaces: names(MAX_FILTER_NAMES + 1), ..Default::default() },
        ChangeFilter { rels: names(MAX_FILTER_NAMES + 1), ..Default::default() },
        ChangeFilter { props: names(MAX_FILTER_NAMES + 1), ..Default::default() },
        ChangeFilter { sources: names(MAX_FILTER_SOURCES + 1), ..Default::default() },
        ChangeFilter { ops: vec![ChangeOp::EntityCreate; MAX_FILTER_OPS + 1], ..Default::default() },
        ChangeFilter { ids: names(1001), ..Default::default() },
        ChangeFilter { kinds: vec!["k".repeat(MAX_FILTER_NAME_LEN + 1)], ..Default::default() },
        ChangeFilter { sources: vec!["*".repeat(MAX_FILTER_NAME_LEN + 1)], ..Default::default() },
    ];
    for f in refused {
        let e = o.changes_page(&viewer(), Cursor::default(), &f, 10).unwrap_err();
        assert!(matches!(e, Error::Invalid(_)), "{e}");
        assert!(o.changes_page_earliest(&viewer(), &f, 10).is_err());
    }
}

/// Review SEC-3: a filter is applied to what the reader receives, so it
/// cannot probe fields `entity.hidden` or a `became_visible` update leave out.
#[test]
fn filters_cannot_probe_what_a_view_leaves_out() {
    let o = Ontology::in_memory(Schema::enterprise());
    let api = ActionContext::new(admin(), "ws1").with_source("api");
    o.apply(&api, vec![Mutation::Upsert(issue("9", "ws1").prop("salary", 100))]).unwrap();
    let at = o.change_head().unwrap();
    o.apply(
        &api,
        vec![Mutation::Upsert(
            issue("9", "ws1").prop("visibility", "restricted").prop("acl", json!(["user:boss"])).prop("salary", 999),
        )],
    )
    .unwrap();
    let probe = |f: ChangeFilter| ops(&o.changes_page(&viewer(), at, &f, 10).unwrap().changes);
    let hidden = vec![(ChangeOp::EntityHidden, "issue:9".to_string())];
    assert_eq!(probe(ChangeFilter::default()), hidden);
    for props in [vec!["salary"], vec!["title"]] {
        let f = ChangeFilter { props: props.iter().map(|p| p.to_string()).collect(), ..Default::default() };
        assert_eq!(probe(f), hidden, "props {props:?}: the same answer");
    }
    for sources in [vec!["api"], vec!["cli"], vec!["!api"]] {
        let f = ChangeFilter { sources: sources.iter().map(|p| p.to_string()).collect(), ..Default::default() };
        assert_eq!(probe(f), hidden, "sources {sources:?}: the same answer");
    }
    // Updates asked for: the update that hid it still arrives (as hidden).
    assert_eq!(probe(ChangeFilter { ops: vec![ChangeOp::EntityUpdate], ..Default::default() }), hidden);
    assert!(probe(ChangeFilter { ops: vec![ChangeOp::EntityCreate], ..Default::default() }).is_empty());

    // Back to public with an edit made in the same commit: the reader gets
    // the entity without `changed`, so props filters cannot tell what changed.
    let at = o.change_head().unwrap();
    o.apply(&api, vec![Mutation::Upsert(issue("9", "ws1").prop("visibility", "public").prop("secret_codename", "x"))])
        .unwrap();
    let page = o.changes_page(&viewer(), at, &ChangeFilter::default(), 10).unwrap();
    let r = &page.changes[0];
    assert!(r.became_visible && r.changed.is_empty() && r.before.is_empty(), "{r:?}");
    for props in ["secret_codename", "title"] {
        let f = ChangeFilter { props: vec![props.into()], ..Default::default() };
        assert!(o.changes_page(&viewer(), at, &f, 10).unwrap().changes.is_empty(), "{props}");
    }
    // An admin, who could see it all along, gets the changed fields.
    let all = o.changes_page(&admin(), at, &ChangeFilter::default(), 10).unwrap();
    assert!(all.changes[0].changed.contains(&"secret_codename".to_string()));
}

/// Review CP-4: a bulk import is one gap listing at most MAX_GAP_IDS ids.
#[test]
fn large_gaps_list_a_bounded_number_of_ids() {
    use onto_core::changes::MAX_GAP_IDS;
    let store = Arc::new(MemoryStore::default());
    let o = Ontology::open(Schema::enterprise(), store.clone(), OntologyOptions::default()).unwrap();
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
    let head = o.change_head().unwrap();
    let e = o.get(&admin(), "issue:1").unwrap();
    let many: Vec<Entity> = (0..MAX_GAP_IDS + 500)
        .map(|n| {
            let mut x = e.clone();
            x.id = format!("issue:b{n:05}");
            x
        })
        .collect();
    // The stored gap (readers get the listed ids they can see in the graph).
    let stored_gap = |after: Cursor| {
        let page = store.changes_since(after, 10).unwrap();
        assert_eq!(page.changes.len(), 1);
        page.changes[0].0.record.gap.clone().unwrap()
    };
    store.bulk_import(&many, &[]).unwrap();
    let gap = stored_gap(head);
    assert_eq!((gap.entities.len(), gap.truncated, gap.total), (MAX_GAP_IDS, true, (MAX_GAP_IDS + 500) as u64));
    assert_eq!(only_gap(&feed(&o, &admin(), head, &ChangeFilter::default())).reason, GapReason::BulkImport);
    // A small one is listed whole.
    let head = store.changes_meta().unwrap().head;
    store.bulk_import(&many[..3], &[]).unwrap();
    let gap = stored_gap(head);
    assert_eq!((gap.entities.len(), gap.truncated, gap.total), (3, false, 0));
}

// ── model-based property test ────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
struct View {
    entities: BTreeMap<String, Entity>,
    links: BTreeMap<LinkKey, Map<String, Value>>,
}

struct Snap {
    head: Cursor,
    admin: View,
    viewer: View,
}

/// What `o` holds, as of its own commit sequence (every record of that
/// commit included). `None` when the store moved on while the snapshot was
/// taken: a follower that caught up from current documents may then hold
/// documents newer than its sequence, so the snapshot would not be the
/// state at one cursor.
fn try_snapshot(o: &Ontology) -> Option<Snap> {
    o.read(|g, _| {
        // Writers of this process hold the graph write lock across the store
        // write, so the sequence read here matches the graph.
        let seq = o.seq();
        let view = |p: &Principal| View {
            entities: g
                .entities()
                .filter(|e| onto_core::policy::visible(p, e, g))
                .map(|e| (e.id.clone(), e.clone()))
                .collect(),
            links: g
                .links()
                .filter(|l| {
                    [&l.from, &l.to].iter().all(|id| g.get(id).is_some_and(|e| onto_core::policy::visible(p, e, g)))
                })
                .map(|l| ((l.from.clone(), l.rel.clone(), l.to.clone()), l.props.clone()))
                .collect(),
        };
        let snap = Snap { head: Cursor::new(seq, u32::MAX), admin: view(&admin()), viewer: view(&viewer()) };
        // Checked after reading the graph: no commit after `seq` existed then.
        (o.store().head().unwrap() == seq).then_some(snap)
    })
}

fn snapshot(o: &Ontology) -> Snap {
    for _ in 0..50 {
        o.sync().unwrap();
        if let Some(s) = try_snapshot(o) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("no consistent snapshot: the store kept moving");
}

/// What a mirror does with the records it gets.
fn replay(mut v: View, records: &[ChangeRecord]) -> View {
    for r in records {
        match r.op {
            ChangeOp::EntityCreate | ChangeOp::EntityUpdate => {
                let e = r.entity.clone().unwrap_or_else(|| panic!("{} has no entity: {r:?}", r.cursor));
                v.entities.insert(e.id.clone(), e);
            }
            ChangeOp::EntityDelete | ChangeOp::EntityHidden => {
                v.entities.remove(&r.id);
                v.links.retain(|(f, _, t), _| *f != r.id && *t != r.id);
            }
            ChangeOp::LinkCreate | ChangeOp::LinkUpdate => {
                let l = r.link.as_ref().unwrap();
                v.links.insert((l.from.clone(), l.rel.clone(), l.to.clone()), l.props.clone());
            }
            ChangeOp::LinkDelete => {
                let l = r.link.as_ref().unwrap();
                v.links.remove(&(l.from.clone(), l.rel.clone(), l.to.clone()));
            }
            ChangeOp::SchemaChange => {}
            ChangeOp::Gap => panic!("unexpected gap {r:?}"),
        }
    }
    v
}

fn random_batch(rng: &mut fastrand::Rng) -> Vec<Mutation> {
    let key = |rng: &mut fastrand::Rng| rng.u32(0..16);
    let ws = |k: u32| if k.is_multiple_of(2) { "ws1" } else { "ws2" };
    (0..rng.usize(1..4))
        .map(|_| match rng.u32(0..10) {
            0..=4 => {
                let k = key(rng);
                let mut e = issue(&k.to_string(), ws(k));
                e.title = Some(["Timeout", "Refund", "Crash"][rng.usize(0..3)].into());
                if rng.bool() {
                    e.state = Some(["open", "triaged", "resolved"][rng.usize(0..3)].into());
                }
                if rng.bool() {
                    e.props.insert("priority".into(), Value::from(["low", "high"][rng.usize(0..2)]));
                }
                match rng.u32(0..4) {
                    0 => {
                        e.props.insert("visibility".into(), json!("restricted"));
                        e.props.insert("acl".into(), json!(vec![["user:v", "user:x"][rng.usize(0..2)]]));
                    }
                    1 => {
                        e.props.insert("visibility".into(), Value::Null);
                        e.props.insert("acl".into(), Value::Null);
                    }
                    _ => {}
                }
                Mutation::Upsert(e)
            }
            5 => Mutation::Delete(onto_core::EntityRef { id: format!("issue:{}", key(rng)) }),
            6..=8 => {
                let a = key(rng);
                let b = (a + 2 * rng.u32(1..8)) % 16;
                let mut l = LinkInput::new(format!("issue:{a}"), "depends_on", format!("issue:{b}"));
                if rng.bool() {
                    l.props.insert("weight".into(), json!(rng.u32(0..3)));
                }
                Mutation::Link(l)
            }
            _ => {
                let a = key(rng);
                let b = (a + 2 * rng.u32(1..8)) % 16;
                Mutation::Unlink(LinkInput::new(format!("issue:{a}"), "depends_on", format!("issue:{b}")))
            }
        })
        .collect()
}

#[test]
fn replaying_the_feed_from_a_snapshot_reaches_a_later_one() {
    replay_property(&[Ontology::in_memory(Schema::enterprise())], 80);
}

/// Eight threads of random batches spread over `writers` (instances on one
/// store), snapshots along the way, then replaying the visible feed from a
/// snapshot A reaches every later snapshot B.
fn replay_property(writers: &[Ontology], rounds: usize) {
    let seed = std::env::var("ONTO_TEST_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| fastrand::u64(..));
    eprintln!("seed {seed} (ONTO_TEST_SEED reruns it)");
    let o = &writers[0];
    let snaps = Mutex::new(vec![snapshot(o)]);
    let committed = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for t in 0..8u64 {
            let (o, snaps, committed) = (writers[t as usize % writers.len()].clone(), &snaps, &committed);
            s.spawn(move || {
                let mut rng = fastrand::Rng::with_seed(seed.wrapping_add(t));
                let ws = if t % 2 == 0 { "ws1" } else { "ws2" };
                for i in 0..rounds {
                    // Missing ends and deleted targets fail the whole batch: fine.
                    if o.apply(&ctx(ws), random_batch(&mut rng)).is_ok() {
                        committed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if i % 8 == 0
                        && let Some(snap) = try_snapshot(&o)
                    {
                        snaps.lock().push(snap);
                    }
                    if i % 20 == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            });
        }
    });
    let mut snaps = snaps.into_inner();
    for w in writers {
        snaps.push(snapshot(w));
    }
    snaps.sort_by_key(|s| s.head);
    let committed = committed.into_inner();
    eprintln!("{committed} batches committed, {} consistent snapshots", snaps.len());
    assert!(committed > 100, "the random batches should mostly commit");

    let mut feeds: HashMap<String, Vec<ChangeRecord>> = HashMap::new();
    for (name, p) in [("admin", admin()), ("viewer", viewer())] {
        feeds.insert(name.into(), feed(o, &p, snaps[0].head, &ChangeFilter::default()));
    }
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut pairs: Vec<(usize, usize)> = vec![(0, snaps.len() - 1)];
    for _ in 0..40 {
        let a = rng.usize(0..snaps.len());
        let b = rng.usize(a..snaps.len());
        pairs.push((a, b));
    }
    for (a, b) in pairs {
        let (sa, sb) = (&snaps[a], &snaps[b]);
        let between = |name: &str| -> Vec<ChangeRecord> {
            feeds[name].iter().filter(|r| r.cursor > sa.head && r.cursor <= sb.head).cloned().collect()
        };
        let admin_view = replay(sa.admin.clone(), &between("admin"));
        if admin_view != sb.admin {
            for (k, e) in &sb.admin.entities {
                if admin_view.entities.get(k) != Some(e) {
                    eprintln!("entity {k}: replay {:?}\n  snapshot {:?}", admin_view.entities.get(k), e);
                }
            }
            for k in admin_view.entities.keys().filter(|k| !sb.admin.entities.contains_key(*k)) {
                eprintln!("entity {k} only in replay");
            }
            for (k, l) in &sb.admin.links {
                if admin_view.links.get(k) != Some(l) {
                    eprintln!("link {k:?}: replay {:?} snapshot {l:?}", admin_view.links.get(k));
                }
            }
            for k in admin_view.links.keys().filter(|k| !sb.admin.links.contains_key(*k)) {
                eprintln!("link {k:?} only in replay");
            }
            for r in between("admin") {
                eprintln!("  {} {} {}", r.cursor, r.op, r.id);
            }
        }
        assert_eq!(admin_view, sb.admin, "admin replay from {} to {} (seed {seed})", sa.head, sb.head);
        // Links are not re-issued when an end comes into sight (phase 1), so
        // a restricted reader's mirror is exact for entities.
        let viewer_view = replay(sa.viewer.clone(), &between("viewer"));
        assert_eq!(
            viewer_view.entities, sb.viewer.entities,
            "viewer replay from {} to {} (seed {seed})",
            sa.head, sb.head
        );
    }
}

// ── PostgreSQL ───────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
mod pg {
    use std::time::Instant;

    use onto_core::changes::{ChangeRetention, Gap};
    use onto_core::store::postgres::tenant_schema;

    use super::*;
    use crate::common::TestTenant;

    fn open(pg: &TestTenant) -> Ontology {
        pg.open(Schema::enterprise(), OntologyOptions::default())
    }

    /// Run SQL on the tenant's schema (`%S%` names it), outside the store.
    fn exec(pg: &TestTenant, body: &str) -> std::result::Result<(), String> {
        let schema = format!("\"{}\"", tenant_schema("onto_", &pg.tenant).unwrap());
        let body = body.replace("%S%", &schema);
        let url = pg.url.clone();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let pool = onto_core::sqlx::PgPool::connect(&url).await.map_err(|e| e.to_string())?;
            let r = onto_core::sqlx::raw_sql(onto_core::sqlx::AssertSqlSafe(body)).execute(&pool).await;
            pool.close().await;
            r.map(|_| ()).map_err(|e| e.to_string())
        })
    }

    /// A commit the way a build before migration 0009 writes it: the head
    /// locked, the rows written, a commit document without `captured`, no
    /// change rows and no `change_seq`.
    fn old_build_commit(pg: &TestTenant, write: &str, commit: &str) {
        exec(
            pg,
            &format!(
                "BEGIN; SELECT seq FROM %S%.head WHERE id = 1 FOR UPDATE; {write} \
                 INSERT INTO %S%.commits (seq, ts, origin, doc) SELECT seq + 1, 1, 'proc_old', \
                 jsonb_build_object('seq', seq + 1, 'ts', 1, 'origin', 'proc_old') || '{commit}'::jsonb \
                 FROM %S%.head WHERE id = 1; UPDATE %S%.head SET seq = seq + 1 WHERE id = 1; COMMIT;"
            ),
        )
        .unwrap();
    }

    fn eventually(what: &str, mut f: impl FnMut() -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for: {what}");
    }

    fn gaps(records: &[ChangeRecord]) -> Vec<&Gap> {
        assert!(records.iter().all(|r| r.op == ChangeOp::Gap), "{records:?}");
        records.iter().map(|r| r.gap.as_ref().unwrap()).collect()
    }

    #[test]
    fn records_commit_only_with_their_commit() {
        let Some(pg) = TestTenant::new("chg_commit") else { return };
        super::records_commit_only_with_their_commit_on(&open(&pg));
    }

    #[test]
    fn readers_see_what_they_may() {
        let Some(pg) = TestTenant::new("chg_readers") else { return };
        super::readers_see_what_they_may_on(&open(&pg));
    }

    #[test]
    fn schema_changes_carry_renames_and_migrations_are_marked() {
        let Some(pg) = TestTenant::new("chg_schema") else { return };
        super::schema_changes_carry_renames_and_migrations_are_marked_on(&open(&pg));
    }

    #[test]
    fn run_cause_and_listener_seq() {
        let Some(pg) = TestTenant::new("chg_cause") else { return };
        super::run_cause_and_listener_seq_on(&open(&pg));
    }

    #[test]
    fn forks_record_nothing() {
        let Some(pg) = TestTenant::new("chg_fork") else { return };
        super::forks_record_nothing_on(&open(&pg));
    }

    #[test]
    fn earliest_starts_at_the_floor() {
        let Some(pg) = TestTenant::new("chg_earliest") else { return };
        super::earliest_starts_at_the_floor_on(&open(&pg));
    }

    #[test]
    fn polling_through_capture_off_never_skips_silently() {
        let Some(pg) = TestTenant::new("changes_capture_poll") else { return };
        super::polling_through_capture_off_never_skips_silently_on(&open(&pg));
    }

    #[test]
    fn capture_off_then_on_expires_old_cursors() {
        let Some(pg) = TestTenant::new("chg_capture") else { return };
        super::capture_off_then_on_expires_old_cursors_on(&open(&pg));
    }

    #[test]
    fn create_and_update_join_the_version_their_commit_wrote() {
        let Some(pg) = TestTenant::new("chg_join") else { return };
        let o = open(&pg);
        let head = o.change_head().unwrap();
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1").state("open"))]).unwrap();
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1").state("resolved"))]).unwrap();
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1").prop("priority", "high"))]).unwrap();
        let records = feed(&o, &admin(), head, &ChangeFilter::default());
        let states: Vec<(ChangeOp, Option<u64>, Option<String>)> =
            records.iter().map(|r| (r.op, r.revision, r.entity.as_ref().and_then(|e| e.state.clone()))).collect();
        assert_eq!(
            states,
            vec![
                (ChangeOp::EntityCreate, Some(1), Some("open".into())),
                (ChangeOp::EntityUpdate, Some(2), Some("resolved".into())),
                (ChangeOp::EntityUpdate, Some(3), Some("resolved".into())),
            ],
            "each record carries the document its own commit wrote, not the current one"
        );
        assert_eq!(records[1].before["state"], json!("open"));
        assert_eq!(records[1].state.as_ref().unwrap().to.as_deref(), Some("resolved"));
        assert!(records.iter().all(|r| !r.entity_missing));
    }

    #[test]
    fn writers_that_do_not_capture_leave_gaps() {
        let Some(pg) = TestTenant::new("chg_rolling") else { return };
        let o = open(&pg);
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1")), Mutation::Upsert(issue("2", "ws1"))]).unwrap();
        let head = o.change_head().unwrap();

        // A replica still on a build before 0009 updates an entity...
        old_build_commit(
            &pg,
            "UPDATE %S%.entities SET doc = jsonb_set(doc, '{title}', '\"Renamed by the old build\"') \
             WHERE id = 'issue:1';",
            r#"{"entities": ["issue:1"], "links": [], "kv": []}"#,
        );
        // ...writes KV only (nothing for the feed)...
        old_build_commit(
            &pg,
            "INSERT INTO %S%.kv (ns, key, value) VALUES ('memory', 'k', '1');",
            r#"{"kv": [["memory", "k"]]}"#,
        );
        // ...and bulk-imports (a reload commit).
        old_build_commit(&pg, "", r#"{"reload": true}"#);
        let old_head = o.store().head().unwrap();
        // A writer of this build that goes around the engine.
        let e = o.get(&admin(), "issue:2").unwrap();
        o.store().write(&WriteBatch { put_entities: vec![e], ..Default::default() }).unwrap();

        let records = feed(&o, &admin(), head, &ChangeFilter { kinds: vec!["service".into()], ..Default::default() });
        let g = gaps(&records);
        assert_eq!(g.len(), 3, "{records:?}");
        assert_eq!((g[0].reason, g[0].origin.as_str()), (GapReason::UncapturedWriter, "proc_old"));
        assert_eq!(g[0].entities, vec!["issue:1"]);
        assert_eq!(records[0].cursor, Cursor::new(head.seq + 1, 0));
        assert_eq!(g[1].reason, GapReason::BulkImport);
        assert_eq!(records[1].cursor, Cursor::new(old_head, 0));
        assert_eq!((g[2].reason, g[2].entities.clone()), (GapReason::UncapturedWriter, vec!["issue:2".to_string()]));
        // The viewer of another workspace learns that something changed, not what.
        let other = feed(&o, &Principal::viewer("w", "ws3"), head, &ChangeFilter::default());
        assert_eq!(gaps(&other)[0].hidden, 1);
        assert!(gaps(&other)[0].entities.is_empty());

        // The follower still converges on what the old build wrote.
        o.sync().unwrap();
        assert_eq!(o.get(&admin(), "issue:1").unwrap().title, "Renamed by the old build");
    }

    #[test]
    fn bulk_imports_record_one_gap() {
        let Some(pg) = TestTenant::new("chg_import") else { return };
        let o = open(&pg);
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
        let template = o.get(&admin(), "issue:1").unwrap();
        let mut a = template.clone();
        a.id = "issue:8".into();
        let mut b = template.clone();
        b.id = "issue:7".into();
        let link = onto_core::Link {
            from: "issue:8".into(),
            rel: "depends_on".into(),
            to: "issue:7".into(),
            workspace: "ws1".into(),
            props: Map::new(),
            created_at: 1,
        };
        let head = o.change_head().unwrap();
        o.store().bulk_import(&[a.clone(), b.clone()], std::slice::from_ref(&link)).unwrap();
        // A gap lists the ids the reader can see in the current graph.
        o.sync().unwrap();
        let records = feed(&o, &admin(), head, &ChangeFilter::default());
        let g = gaps(&records);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].reason, GapReason::BulkImport);
        assert_eq!(g[0].entities, vec!["issue:7", "issue:8"]);
        assert_eq!(g[0].links, vec![("issue:8".to_string(), "depends_on".to_string(), "issue:7".to_string())]);

        // With capture off, an import records nothing (and turning capture on
        // again expires the cursors from before).
        o.set_capture(&ctx("ws1"), false).unwrap();
        let off = o.change_head().unwrap();
        a.id = "issue:6".into();
        o.store().bulk_import(&[a], &[]).unwrap();
        assert!(o.changes_page(&admin(), off, &ChangeFilter::default(), 10).unwrap().changes.is_empty());
        o.set_capture(&ctx("ws1"), true).unwrap();
        assert!(o.changes_page(&admin(), off, &ChangeFilter::default(), 10).unwrap().expired);
    }

    #[test]
    fn expired_cursor_at_the_retention_edge() {
        let Some(pg) = TestTenant::new("chg_edge") else { return };
        let o = open(&pg);
        let lease = || assert!(o.try_lease("job", "me", 60_000).unwrap());
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1")), Mutation::Upsert(issue("2", "ws1"))]).unwrap();
        let s2 = o.seq();
        lease(); // a commit without records
        let s3 = o.seq();
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("3", "ws1")), Mutation::Upsert(issue("4", "ws1"))]).unwrap();
        let s4 = o.seq();
        lease();
        let s5 = o.seq();
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("5", "ws1"))]).unwrap();
        let s6 = o.seq();
        assert_eq!((s3, s4, s5, s6), (s2 + 1, s2 + 2, s2 + 3, s2 + 4));

        // Keep three records: s6.0, s4.1 and s4.0. The next one is s2's, so
        // s2 and everything older goes; the floor is s3, a seq without
        // records, with another such seq (s5) above it.
        let keep = ChangeRetention { keep_ms: None, keep_rows: Some(3) };
        let v = o.store().vacuum_changes(&keep).unwrap();
        assert_eq!(v.floor, s3);
        assert!(v.deleted >= 2, "{v:?}");
        assert_eq!(
            o.store().vacuum_changes(&keep).unwrap(),
            onto_core::changes::ChangeVacuum { deleted: 0, floor: s3 }
        );
        let meta = o.store().changes_meta().unwrap();
        assert_eq!((meta.floor, meta.head), (s3, Cursor::new(s6, 0)));

        let page =
            |after: Cursor| -> ChangePage { o.changes_page(&admin(), after, &ChangeFilter::default(), 100).unwrap() };
        for old in [Cursor::default(), Cursor::new(s2, 0)] {
            let p = page(old);
            assert!(p.expired && p.changes.is_empty(), "{old}");
            assert_eq!(p.floor, s3);
        }
        // Fully caught up on s2 = floor - 1: still reported expired (a false
        // reset at the exact edge, never a silent gap).
        assert!(page(Cursor::new(s2, 1)).expired);
        let at_floor = page(Cursor::new(s3, 0));
        assert!(!at_floor.expired);
        assert_eq!(
            at_floor.changes.iter().map(|r| r.cursor).collect::<Vec<_>>(),
            vec![Cursor::new(s4, 0), Cursor::new(s4, 1), Cursor::new(s6, 0)]
        );
        let above = page(Cursor::new(s5, 0));
        assert_eq!(ops(&above.changes), vec![(ChangeOp::EntityCreate, "issue:5".into())]);
        assert_eq!(above.head, s6);
    }

    #[test]
    fn retention_by_age_and_the_table_is_append_only() {
        let Some(pg) = TestTenant::new("chg_age") else { return };
        let o = open(&pg);
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
        let head = o.change_head().unwrap();
        assert!(o.store().changes_since(Cursor::default(), 10).unwrap().changes.len() >= 2);

        for (stmt, refused) in [
            ("UPDATE %S%.changes SET op = 'gap'", "append-only"),
            ("DELETE FROM %S%.changes", "append-only"),
            ("TRUNCATE %S%.changes", "append-only"),
        ] {
            let err = exec(&pg, stmt).unwrap_err();
            assert!(err.contains(refused), "{stmt}: {err}");
        }

        // A week's default keeps everything just written.
        assert_eq!(o.store().vacuum_changes(&ChangeRetention::default()).unwrap().deleted, 0);
        // A cutoff in the future removes every record except the newest
        // commit's: the floor stops at the head, so the head cursor stays
        // valid. A reader fully caught up on the commit before it is
        // reported expired (the documented false reset at the edge).
        let before = Cursor::new(head.seq - 1, u32::MAX);
        let v = o.store().vacuum_changes(&ChangeRetention { keep_ms: Some(-60_000), keep_rows: None }).unwrap();
        assert_eq!(v.floor, head.seq);
        let left = o.store().changes_since(Cursor::default(), 10).unwrap().changes;
        assert_eq!(left.iter().map(|(c, _)| c.record.cursor).collect::<Vec<_>>(), vec![head]);
        assert!(o.changes_page(&admin(), before, &ChangeFilter::default(), 10).unwrap().expired);
        assert!(!o.changes_page(&admin(), head, &ChangeFilter::default(), 10).unwrap().expired);
        // Keeping no rows still keeps the head commit's.
        let none = o.store().vacuum_changes(&ChangeRetention { keep_ms: None, keep_rows: Some(0) }).unwrap();
        assert_eq!(none, onto_core::changes::ChangeVacuum { deleted: 0, floor: head.seq });
        // A reset reader resumes from the head.
        let resume = o.change_head().unwrap();
        assert!(!o.changes_page(&admin(), resume, &ChangeFilter::default(), 10).unwrap().expired);
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("2", "ws1"))]).unwrap();
        assert_eq!(
            ops(&feed(&o, &admin(), resume, &ChangeFilter::default())),
            vec![(ChangeOp::EntityCreate, "issue:2".into())]
        );
    }

    #[test]
    fn capture_switches_reach_other_processes() {
        let Some(pg) = TestTenant::new("chg_switch") else { return };
        let (a, b) = (open(&pg), open(&pg));
        assert!(a.capture_enabled() && b.capture_enabled());
        a.set_capture(&ctx("ws1"), false).unwrap();
        eventually("b hears that capture is off", || !b.capture_enabled());
        let off = b.change_head().unwrap();
        b.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
        assert!(a.changes_page(&admin(), off, &ChangeFilter::default(), 10).unwrap().changes.is_empty(), "no gap");

        a.set_capture(&ctx("ws1"), true).unwrap();
        eventually("b hears that capture is on", || b.capture_enabled());
        let on = b.change_head().unwrap();
        b.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("2", "ws1"))]).unwrap();
        assert_eq!(
            ops(&feed(&a, &admin(), on, &ChangeFilter::default())),
            vec![(ChangeOp::EntityCreate, "issue:2".into())]
        );
        // A process that missed the notice still reads the setting on demand.
        assert!(b.refresh_capture().unwrap());
    }

    #[test]
    fn a_tenant_that_needs_a_newer_writer_refuses_writes() {
        let Some(pg) = TestTenant::new("chg_writer") else { return };
        let o = open(&pg);
        exec(&pg, "UPDATE %S%.head SET min_writer = 1000").unwrap();
        let err = o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap_err();
        assert!(matches!(&err, Error::Unavailable(m) if m.contains("newer onto-intelli")), "{err:?}");
        let e = Entity { id: "issue:9".into(), ..blank() };
        let err = o.store().bulk_import(&[e], &[]).unwrap_err();
        assert!(matches!(&err, Error::Unavailable(m) if m.contains("newer onto-intelli")), "{err:?}");
        exec(&pg, "UPDATE %S%.head SET min_writer = 0").unwrap();
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
    }

    fn blank() -> Entity {
        Entity {
            id: String::new(),
            kind: "issue".into(),
            workspace: "ws1".into(),
            title: "x".into(),
            state: None,
            props: Map::new(),
            aliases: Vec::new(),
            provenance: None,
            created_at: 1,
            updated_at: 1,
            revision: 1,
        }
    }

    #[test]
    fn a_populated_tenant_from_before_0009_migrates_and_captures() {
        let Some(pg) = TestTenant::new("chg_pre0009") else { return };
        {
            let o = open(&pg);
            o.apply(
                &ctx("ws1"),
                vec![
                    Mutation::Upsert(issue("1", "ws1")),
                    Mutation::Upsert(issue("2", "ws1")),
                    Mutation::Link(LinkInput::new("issue:1", "depends_on", "issue:2")),
                ],
            )
            .unwrap();
        }
        // Take the tenant back to migration 0008, as a build before this one
        // left it.
        exec(
            &pg,
            "BEGIN; DROP TRIGGER commits_capture ON %S%.commits; DROP FUNCTION %S%.changes_gap(); \
             DROP FUNCTION %S%.commit_touches_schema(jsonb); DROP TABLE %S%.changes; DROP TABLE %S%.changes_meta; \
             DROP INDEX %S%.entity_versions_change; ALTER TABLE %S%.entity_versions DROP COLUMN change_seq; \
             ALTER TABLE %S%.head DROP COLUMN min_writer; DELETE FROM %S%.schema_migrations WHERE version >= 9; \
             COMMIT;",
        )
        .unwrap();
        old_build_commit(
            &pg,
            "UPDATE %S%.entities SET doc = jsonb_set(doc, '{state}', '\"blocked\"') WHERE id = 'issue:2';",
            r#"{"entities": ["issue:2"]}"#,
        );

        let store = pg.store();
        assert_eq!(
            store.migrations().unwrap().last().map(|m| m.0),
            onto_core::store::postgres::MIGRATIONS.last().map(|m| m.0)
        );
        let before = store.head().unwrap();
        let meta = store.changes_meta().unwrap();
        assert!(meta.capture);
        assert_eq!(meta.floor, before, "nothing before the migration was recorded");
        let o = Ontology::open(Schema::enterprise(), Arc::new(store), OntologyOptions::default()).unwrap();
        assert!(o.capture_enabled());
        assert_eq!(o.get(&admin(), "issue:2").unwrap().state.as_deref(), Some("blocked"));
        assert_eq!(o.neighbors(&admin(), "issue:1", Some("depends_on")).unwrap().len(), 1);
        assert_eq!(o.history(&admin(), "issue:1", 10).unwrap().len(), 1, "versions without change_seq still read");

        // Older cursors are expired, not silently empty.
        assert!(o.changes_page(&admin(), Cursor::default(), &ChangeFilter::default(), 10).unwrap().expired);
        // The first commit afterwards is captured, with its document joined.
        o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("2", "ws1").state("open"))]).unwrap();
        let records = feed(&o, &admin(), Cursor::new(before, 0), &ChangeFilter::default());
        assert_eq!(ops(&records), vec![(ChangeOp::EntityUpdate, "issue:2".into())]);
        assert_eq!(records[0].before["state"], json!("blocked"));
        assert_eq!(records[0].entity.as_ref().unwrap().state.as_deref(), Some("open"));
    }

    #[test]
    fn replaying_the_feed_across_two_processes() {
        let Some(pg) = TestTenant::new("chg_replay") else { return };
        let (a, b) = (open(&pg), open(&pg));
        replay_property(&[a, b], 60);
    }
}
