//! The change feed on the in-memory store: records commit with their
//! writes, readers see what they may, gaps and the floor say what is
//! missing, and replaying the feed reaches later snapshots.

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
    let o = Ontology::in_memory(Schema::enterprise());
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
    assert!(feed(&o, &admin(), head, &ChangeFilter::default()).is_empty());

    o.apply(&member, vec![Mutation::Upsert(issue("1", "ws1").state("open"))]).unwrap();
    let records = feed(&o, &admin(), head, &ChangeFilter::default());
    assert_eq!(ops(&records), vec![(ChangeOp::EntityCreate, "issue:1".into())]);
    let r = &records[0];
    assert_eq!(r.cursor.seq, o.seq());
    assert_eq!(r.entity.as_ref().unwrap().state.as_deref(), Some("open"));
    assert_eq!(r.cause.actor, "mia");
    assert_eq!(o.change_head().unwrap(), r.cursor);
}

#[test]
fn readers_see_what_they_may() {
    let o = Ontology::in_memory(Schema::enterprise());
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

    let all = feed(&o, &admin(), head, &ChangeFilter::default());
    assert_eq!(all.len(), 6);
    let seen = feed(&o, &viewer(), head, &ChangeFilter::default());
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
    let only = feed(&o, &viewer(), at, &f);
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
    let o = Ontology::in_memory(Schema::enterprise());
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(EntityInput::new("task", "3", "Write docs"))]).unwrap();
    let head = o.change_head().unwrap();
    let set: SchemaChangeSet =
        serde_json::from_value(json!({ "changes": [ { "op": "rename_object_type", "from": "task", "to": "chore" } ] }))
            .unwrap();
    o.schema_apply(&ctx("ws1"), set).unwrap();
    let records = feed(&o, &admin(), head, &ChangeFilter::default());
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
    assert_eq!(ops(&feed(&o, &admin(), head, &f)), vec![(ChangeOp::SchemaChange, schema.id.clone())]);
}

#[test]
fn run_cause_and_listener_seq() {
    let o = Ontology::in_memory(Schema::enterprise());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    o.on_change(move |c| s.lock().push(c.seq));
    let head = o.change_head().unwrap();
    let mut c = ctx("ws1").with_source("automation:triage");
    c.cause = Some(RunCause { automation: "triage".into(), run: "run_7".into(), depth: 1 });
    o.apply(&c, vec![Mutation::Upsert(issue("1", "ws1"))]).unwrap();
    assert_eq!(*seen.lock(), vec![o.seq()]);
    let r = &feed(&o, &admin(), head, &ChangeFilter::default())[0];
    assert_eq!(
        (r.cause.automation.as_deref(), r.cause.run.as_deref(), r.cause.depth),
        (Some("triage"), Some("run_7"), 1)
    );
    let f = ChangeFilter { sources: vec!["!automation:*".into()], ..Default::default() };
    assert!(feed(&o, &admin(), head, &f).is_empty());
}

#[test]
fn forks_record_nothing() {
    let o = Ontology::in_memory(Schema::enterprise());
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
fn capture_off_then_on_expires_old_cursors() {
    let o = Ontology::in_memory(Schema::enterprise());
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
    assert!(feed(&o, &admin(), before, &ChangeFilter::default()).is_empty(), "nothing recorded, no gap");
    let during = o.change_head().unwrap();

    o.set_capture(&ctx("ws1"), true).unwrap();
    assert!(o.capture_enabled());
    let p = o.changes_page(&admin(), before, &ChangeFilter::default(), 10).unwrap();
    assert!(p.expired, "a write went unrecorded after this cursor");
    // A cursor taken while capture was off missed nothing.
    assert!(!o.changes_page(&admin(), during, &ChangeFilter::default(), 10).unwrap().expired);
    o.apply(&ctx("ws1"), vec![Mutation::Upsert(issue("3", "ws1"))]).unwrap();
    assert_eq!(
        ops(&feed(&o, &admin(), during, &ChangeFilter::default())),
        vec![(ChangeOp::EntityCreate, "issue:3".into())]
    );
    let audit =
        o.events(&admin(), &EventFilter { action: Some("changes.capture".into()), ..Default::default() }).unwrap();
    assert_eq!(audit.len(), 2);
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

fn snapshot(o: &Ontology) -> Snap {
    o.read(|g, _| {
        // Writers hold the graph write lock across the store write, so the
        // head read here matches the graph.
        let head = o.store().changes_meta().unwrap().head;
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
        Snap { head, admin: view(&admin()), viewer: view(&viewer()) }
    })
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
    let seed = std::env::var("ONTO_TEST_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| fastrand::u64(..));
    eprintln!("seed {seed} (ONTO_TEST_SEED reruns it)");
    let o = Ontology::in_memory(Schema::enterprise());
    let snaps = Mutex::new(vec![snapshot(&o)]);
    let committed = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for t in 0..8u64 {
            let (o, snaps, committed) = (o.clone(), &snaps, &committed);
            s.spawn(move || {
                let mut rng = fastrand::Rng::with_seed(seed.wrapping_add(t));
                let ws = if t % 2 == 0 { "ws1" } else { "ws2" };
                for i in 0..80 {
                    // Missing ends and deleted targets fail the whole batch: fine.
                    if o.apply(&ctx(ws), random_batch(&mut rng)).is_ok() {
                        committed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if i % 8 == 0 {
                        snaps.lock().push(snapshot(&o));
                    }
                    if i % 20 == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            });
        }
    });
    let mut snaps = snaps.into_inner();
    snaps.push(snapshot(&o));
    snaps.sort_by_key(|s| s.head);
    assert!(committed.into_inner() > 100, "the random batches should mostly commit");

    let mut feeds: HashMap<String, Vec<ChangeRecord>> = HashMap::new();
    for (name, p) in [("admin", admin()), ("viewer", viewer())] {
        feeds.insert(name.into(), feed(&o, &p, snaps[0].head, &ChangeFilter::default()));
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
