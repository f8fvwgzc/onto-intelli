//! The outbox on the in-memory store and on PostgreSQL (when a test database
//! is set; see `common`): disjoint claims (`FOR UPDATE SKIP LOCKED`), an
//! expired lease claimed again, a late finish with an old claim token that
//! changes nothing, replay keeping the id, the unique `(target, dedupe)` key
//! absorbing a repeated fan-out page, fenced enqueue with a consumer
//! advance, rows enqueued with their audit event in one commit, backlog
//! counts per target, and schema renames rewriting endpoint filters in the
//! schema commit. (Rows
//! committed with a graph change are tested in `outbox.rs`'s unit tests,
//! where the commit's extras are reachable.)

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use onto_core::changes::RunCause;
use onto_core::leases::Fence;
use onto_core::outbox::{
    Attempt, ConfigEvent, OutboxKind, OutboxQuery, OutboxRow, OutboxSelect, OutboxState, SOURCE_CHANGE, SOURCE_TEST,
};
use onto_core::store::{MemoryStore, Store};
use onto_core::{ActionContext, Cursor, Error, EventFilter, Ontology, OntologyOptions, Principal, Role, Schema};

fn row(target: &str, dedupe: &str) -> OutboxRow {
    let mut r = OutboxRow::new(OutboxKind::Webhook, target, dedupe, SOURCE_TEST, "acme", "onto.webhook.test");
    r.payload = json!({ "hello": dedupe });
    r.cause = Some(RunCause { automation: "a".into(), run: "run_1".into(), depth: 1 });
    r.otel_trace_id = Some("4bf92f3577b34da6a3ce929d0e0e4736".into());
    r
}

fn ok() -> Attempt {
    Attempt { status: Some(200), duration_ms: 3, response: Some("ok".into()), ..Default::default() }
}

/// The outbox protocol every store follows.
fn protocol(store: &dyn Store) {
    assert!(store.keeps_outbox());
    let rows: Vec<OutboxRow> = (0..6).map(|i| row("ops", &format!("test:{i}"))).collect();
    assert_eq!(store.outbox_enqueue(&rows, None, None).unwrap(), 6);
    // A repeated page (same keys, new ids) is absorbed by (target, dedupe).
    let again: Vec<OutboxRow> = (0..6).map(|i| row("ops", &format!("test:{i}"))).collect();
    assert_eq!(store.outbox_enqueue(&again, None, None).unwrap(), 0, "a repeated page enqueues nothing");
    assert_eq!(store.outbox_enqueue(&[row("other", "test:0")], None, None).unwrap(), 1, "keys are per target");
    // Backlogs per target (fan-out pauses an endpoint past its cap).
    assert_eq!(store.outbox_open_counts().unwrap(), vec![("ops".to_string(), 6), ("other".to_string(), 1)]);
    // Fan-out's per-endpoint counts stop at its cap.
    let capped = store.outbox_open_counts_for(&["ops".into(), "none".into(), "other".into()], 4).unwrap();
    assert_eq!(capped, vec![("ops".to_string(), 4), ("none".to_string(), 0), ("other".to_string(), 1)]);
    let stored = store.outbox_get(&rows[0].id).unwrap().unwrap();
    assert_eq!(stored.state, OutboxState::Pending);
    assert!(stored.created_at > 0 && stored.next_at >= stored.created_at, "{stored:?}");
    assert_eq!((stored.payload.clone(), stored.cause.clone()), (rows[0].payload.clone(), rows[0].cause.clone()));
    assert_eq!(stored.otel_trace_id, rows[0].otel_trace_id);

    // Two claims at once take disjoint rows.
    let a = store.outbox_claim(3, 60_000, "a", &[]).unwrap();
    let b = store.outbox_claim(3, 60_000, "b", &["other".into()]).unwrap();
    assert_eq!((a.len(), b.len()), (3, 3), "{a:?} {b:?}");
    let ids: HashSet<&str> = a.iter().chain(&b).map(|r| r.id.as_str()).collect();
    assert_eq!(ids.len(), 6, "claims overlap");
    assert!(b.iter().all(|r| r.target == "ops"), "skipped targets are not claimed");
    for r in a.iter().chain(&b) {
        assert_eq!((r.state, r.attempts), (OutboxState::Inflight, 1));
        assert!(r.claim.as_deref().is_some_and(|c| c.starts_with("a:") || c.starts_with("b:")), "{r:?}");
        assert!(r.lease_until.is_some_and(|l| l > r.next_at));
    }
    let open: u64 = store.outbox_open_counts().unwrap().iter().map(|(_, n)| n).sum();
    assert_eq!(open, 7, "claimed rows are still undelivered");
    let rest = store.outbox_claim(10, 60_000, "c", &[]).unwrap();
    assert_eq!(rest.len(), 1, "only the other target's row is left");
    assert!(store.outbox_claim(10, 60_000, "c", &[]).unwrap().is_empty(), "nothing due");

    // Finishing needs the current claim token.
    let first = &a[0];
    let claim = first.claim.clone().unwrap();
    assert!(!store.outbox_finish(&first.id, "a:stale", &ok(), OutboxState::Delivered, 0).unwrap());
    assert!(store.outbox_finish(&first.id, &claim, &ok(), OutboxState::Delivered, 0).unwrap());
    let done = store.outbox_get(&first.id).unwrap().unwrap();
    assert_eq!((done.state, done.last_status, done.claim.clone()), (OutboxState::Delivered, Some(200), None));
    assert!(done.delivered_at.is_some());
    assert!(!store.outbox_finish(&first.id, &claim, &ok(), OutboxState::Dead, 0).unwrap(), "a token works once");

    // Retry: due later by the store's clock.
    let second = &a[1];
    let fail = Attempt { status: Some(503), error: Some("http 503".into()), ..Default::default() };
    assert!(
        store.outbox_finish(&second.id, second.claim.as_ref().unwrap(), &fail, OutboxState::Retry, 60_000).unwrap()
    );
    let retry = store.outbox_get(&second.id).unwrap().unwrap();
    assert_eq!((retry.state, retry.last_status, retry.attempts), (OutboxState::Retry, Some(503), 1));
    assert!(retry.next_at >= retry.created_at + 59_000, "{retry:?}");
    assert_eq!(store.outbox_next_due().unwrap().map(|d| d <= retry.next_at), Some(true));

    // An expired lease is claimed again; the late finish of the first claim changes nothing.
    let third = &a[2];
    let old = third.claim.clone().unwrap();
    assert!(store.outbox_release(&b[0].id, b[0].claim.as_ref().unwrap()).unwrap(), "released unsent");
    let released = store.outbox_get(&b[0].id).unwrap().unwrap();
    assert_eq!((released.state, released.attempts, released.claim), (OutboxState::Pending, 0, None));
    let short = store.outbox_claim(1, 30, "d", &["other".into()]).unwrap();
    assert_eq!(short.len(), 1);
    assert_eq!(short[0].id, b[0].id, "the released row is due again");
    std::thread::sleep(Duration::from_millis(120));
    let again = store.outbox_claim(1, 60_000, "e", &["other".into()]).unwrap();
    assert_eq!(again.len(), 1, "the expired lease is claimed again");
    assert_eq!((again[0].id.as_str(), again[0].attempts), (b[0].id.as_str(), 2));
    assert!(
        !store.outbox_finish(&b[0].id, short[0].claim.as_ref().unwrap(), &ok(), OutboxState::Delivered, 0).unwrap(),
        "the worker whose lease expired cannot finish it"
    );
    assert_eq!(store.outbox_get(&b[0].id).unwrap().unwrap().state, OutboxState::Inflight);
    assert!(store.outbox_finish(&third.id, &old, &Attempt::default(), OutboxState::Dead, 0).unwrap());

    // Attempts are logged, newest first.
    let log = store.outbox_attempts(&first.id, 10).unwrap();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!((log[0].attempt, log[0].state, log[0].result.status), (1, OutboxState::Delivered, Some(200)));
    assert_eq!(log[0].result.response.as_deref(), Some("ok"));

    // Replay keeps the id and starts over; inflight rows are not touched.
    let n = store
        .outbox_reset("ops", &OutboxSelect::Ids(vec![first.id.clone(), third.id.clone(), b[0].id.clone()]), &[])
        .unwrap();
    assert_eq!(n, 2, "delivered and dead rows replay, the inflight one does not");
    let replayed = store.outbox_get(&first.id).unwrap().unwrap();
    assert_eq!((replayed.state, replayed.attempts, replayed.delivered_at), (OutboxState::Pending, 0, None));
    let picked = store.outbox_claim(10, 60_000, "f", &["other".into()]).unwrap();
    let picked: HashSet<String> = picked.into_iter().map(|r| r.id).collect();
    assert!(picked.contains(&first.id) && picked.contains(&third.id), "{picked:?}");
    assert_eq!(store.outbox_reset("ops", &OutboxSelect::Since(0), &[OutboxState::Pending]).unwrap(), 0);

    // Listing: newest first, filtered, without claim tokens once listed.
    let all = store.outbox_list(&OutboxQuery { target: Some("ops".into()), ..Default::default() }).unwrap();
    assert_eq!(all.len(), 6);
    assert!(all.windows(2).all(|w| (w[0].created_at, &w[0].id) >= (w[1].created_at, &w[1].id)));
    let inflight =
        store.outbox_list(&OutboxQuery { states: vec![OutboxState::Inflight], ..Default::default() }).unwrap();
    assert!(inflight.iter().all(|r| r.state == OutboxState::Inflight) && !inflight.is_empty());
    assert!(inflight[0].clone().listed().claim.is_none());

    // Retention removes finished rows only.
    let c = &rest[0];
    assert!(store.outbox_finish(&c.id, c.claim.as_ref().unwrap(), &ok(), OutboxState::Delivered, 0).unwrap());
    let removed = store.outbox_vacuum(onto_core::time::now_ms() + 1_000).unwrap();
    assert_eq!(removed, 1, "only the delivered row of target other");
    assert!(store.outbox_get(&c.id).unwrap().is_none());
    assert!(store.outbox_attempts(&c.id, 10).unwrap().is_empty());
}

/// Enqueue and the consumer cursor move together, under the fence.
fn fenced_enqueue(store: &dyn Store) {
    assert_eq!(store.lease("webhook:fanout", "a", 60_000).unwrap(), Some(1));
    let good = Fence::new("webhook:fanout", "a", 1);
    let mut r = row("ops", "7.0");
    r.source = SOURCE_CHANGE.into();
    r.cursor = Some(Cursor::new(7, 0));
    assert_eq!(store.outbox_enqueue(&[r], Some(("webhook:ops", Cursor::new(7, 0))), Some(&good)).unwrap(), 1);
    assert_eq!(store.consumer_get("webhook:ops").unwrap(), Some(Cursor::new(7, 0)));
    let listed = store.outbox_list(&OutboxQuery { source: Some(SOURCE_CHANGE.into()), ..Default::default() }).unwrap();
    assert_eq!(listed[0].cursor, Some(Cursor::new(7, 0)));
    let stale = Fence::new("webhook:fanout", "a", 9);
    let mut r = row("ops", "8.0");
    r.cursor = Some(Cursor::new(8, 0));
    let e = store.outbox_enqueue(std::slice::from_ref(&r), Some(("webhook:ops", Cursor::new(8, 0))), Some(&stale));
    assert!(matches!(e, Err(Error::Unavailable(ref m)) if m.starts_with("lease lost")), "{e:?}");
    assert!(store.outbox_get(&r.id).unwrap().is_none(), "nothing written");
    assert_eq!(store.consumer_get("webhook:ops").unwrap(), Some(Cursor::new(7, 0)), "the cursor did not move");
}

/// Rows enqueued through the engine commit with their audit event; a
/// duplicate key fails the whole commit; agents and members are refused.
fn through_the_engine(o: &Ontology) {
    let admin = ActionContext::new(Principal::admin("ada"), "acme");
    let r = row("ops", "test:engine");
    let id = r.id.clone();
    o.enqueue_outbox(&admin, vec![r], Some(ConfigEvent::new("webhook.test", Some("ops"), json!({ "id": id }))))
        .unwrap();
    assert_eq!(o.store().outbox_get(&id).unwrap().unwrap().state, OutboxState::Pending);
    let evs =
        o.events(&Principal::admin("ada"), &EventFilter { action: Some("webhook.test".into()), ..Default::default() });
    assert_eq!(evs.unwrap().len(), 1);
    let dup = row("ops", "test:engine");
    let dup_id = dup.id.clone();
    let e = o.enqueue_outbox(&admin, vec![dup], Some(ConfigEvent::new("webhook.test", Some("ops"), json!({}))));
    assert!(matches!(e, Err(Error::Conflict(_))), "{e:?}");
    assert!(o.store().outbox_get(&dup_id).unwrap().is_none());
    let evs =
        o.events(&Principal::admin("ada"), &EventFilter { action: Some("webhook.test".into()), ..Default::default() });
    assert_eq!(evs.unwrap().len(), 1, "the failed commit wrote no event");
    for p in [
        Principal::new("bot", vec![Role::Agent], vec!["*".into()]),
        Principal::named("user:m", &["member"], &["*"]).unwrap(),
    ] {
        let ctx = ActionContext::new(p, "acme");
        let e = o.enqueue_outbox(&ctx, vec![row("ops", "test:denied")], None);
        assert!(matches!(e, Err(Error::Denied(_))), "{e:?}");
        let e =
            o.commit_config(&ctx, vec![("webhook.control".into(), "kill".into(), json!(true))], ConfigEvent::default());
        assert!(matches!(e, Err(Error::Denied(_))), "{e:?}");
    }
    // Configuration: only the outbound namespaces, with its event, in one commit.
    let e = o.commit_config(&admin, vec![("schema".into(), "head".into(), json!(1))], ConfigEvent::default());
    assert!(matches!(e, Err(Error::Invalid(_))), "{e:?}");
    o.commit_config(
        &admin,
        vec![("webhook.control".into(), "kill".into(), json!({ "on": true }))],
        ConfigEvent::new("webhook.kill", None, json!({ "on": true })),
    )
    .unwrap();
    assert_eq!(o.store().kv_get("webhook.control", "kill").unwrap(), Some(json!({ "on": true })));
}

/// A schema change that renames a kind or a relation rewrites the change
/// filters of webhook endpoints in its own commit (their version moves);
/// endpoints that do not name them stay as they were.
fn renames_rewrite_endpoint_filters(o: &Ontology) {
    let admin = ActionContext::new(Principal::admin("lead"), "acme");
    let ep = |name: &str, filter: serde_json::Value| {
        json!({ "name": name, "url": "https://hooks.example.com/", "secret": "env:S", "filter": filter,
                "principal": { "roles": ["viewer"], "workspaces": ["*"] }, "enabled": true, "version": 4 })
    };
    let kv = vec![
        ("webhook.endpoint".to_string(), "tasks".to_string(), ep("tasks", json!({ "kinds": ["task", "person"] }))),
        ("webhook.endpoint".to_string(), "links".to_string(), ep("links", json!({ "rels": ["^part_of", "owns"] }))),
        ("webhook.endpoint".to_string(), "none".to_string(), ep("none", json!({ "kinds": ["person"] }))),
        ("webhook.endpoint".to_string(), "effects".to_string(), ep("effects", serde_json::Value::Null)),
    ];
    o.commit_config(&admin, kv, ConfigEvent::new("webhook.add", None, json!({}))).unwrap();
    let set: onto_core::SchemaChangeSet = serde_json::from_value(json!({ "changes": [
        { "op": "rename_object_type", "from": "task", "to": "chore" },
        { "op": "rename_link_type", "from": "part_of", "to": "within" },
    ] }))
    .unwrap();
    let before = o.store().head().unwrap();
    o.schema_apply(&admin, set).unwrap();
    let get = |name: &str| o.store().kv_get("webhook.endpoint", name).unwrap().unwrap();
    assert_eq!(get("tasks")["filter"]["kinds"], json!(["chore", "person"]));
    assert_eq!(get("tasks")["version"], 5);
    assert_eq!(get("links")["filter"]["rels"], json!(["^within", "owns"]));
    assert_eq!(get("links")["version"], 5);
    assert_eq!((get("none")["version"].as_u64(), get("effects")["version"].as_u64()), (Some(4), Some(4)));
    // One commit carried the schema and the rewritten filters.
    assert_eq!(o.store().head().unwrap(), before + 1);
    let f = EventFilter { action: Some("schema.change".into()), ..Default::default() };
    let ev = o.events(&Principal::admin("lead"), &f).unwrap();
    assert_eq!(ev.last().unwrap().payload["webhook_filters"], json!(["links", "tasks"]));
}

#[test]
fn memory_outbox() {
    protocol(&MemoryStore::default());
    fenced_enqueue(&MemoryStore::default());
    through_the_engine(&Ontology::in_memory(Schema::enterprise()));
    renames_rewrite_endpoint_filters(&Ontology::in_memory(Schema::enterprise()));
}

#[test]
fn postgres_renames_rewrite_endpoint_filters() {
    let Some(pg) = common::TestTenant::new("outbox_renames") else { return };
    renames_rewrite_endpoint_filters(&pg.open(Schema::enterprise(), OntologyOptions::default()));
}

#[test]
fn postgres_outbox() {
    let Some(pg) = common::TestTenant::new("outbox") else { return };
    protocol(&pg.store());
}

#[test]
fn postgres_fenced_enqueue_and_engine_rows() {
    let Some(pg) = common::TestTenant::new("outbox_fenced") else { return };
    fenced_enqueue(&pg.store());
    through_the_engine(&pg.open(Schema::enterprise(), OntologyOptions::default()));
}

/// Two stores (two processes' worth of workers) claiming from one tenant
/// concurrently never get the same row.
#[test]
fn postgres_concurrent_claims_are_disjoint() {
    let Some(pg) = common::TestTenant::new("outbox_claims") else { return };
    let seed = pg.store();
    let rows: Vec<OutboxRow> = (0..200).map(|i| row("ops", &format!("test:{i}"))).collect();
    assert_eq!(seed.outbox_enqueue(&rows, None, None).unwrap(), 200);
    let stores: Vec<Arc<dyn Store>> = (0..2).map(|_| Arc::new(pg.store()) as Arc<dyn Store>).collect();
    let mut handles = Vec::new();
    for t in 0..8 {
        let s = stores[t % 2].clone();
        handles.push(std::thread::spawn(move || {
            let mut got = Vec::new();
            loop {
                let batch = s.outbox_claim(7, 60_000, &format!("w{t}"), &[]).unwrap();
                if batch.is_empty() {
                    break got;
                }
                got.extend(batch.into_iter().map(|r| r.id));
            }
        }));
    }
    let mut all: Vec<String> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    let n = all.len();
    all.sort();
    all.dedup();
    assert_eq!((n, all.len()), (200, 200), "every row claimed exactly once");
}
