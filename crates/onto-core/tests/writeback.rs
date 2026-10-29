//! Writeback effects of action types (docs/design/EVENTS.md Part D, build
//! step 12), on the in-memory store and on PostgreSQL (when a test database
//! is set; see `common`): validation, the plan and confirmation rules (an
//! external write is not reversible and at least medium risk, so it needs a
//! person's confirmation unless the action type declares `unattended` for
//! the caller's kind, and its declared risk still applies), refusal while
//! `[writeback] enabled` is off or the target does not serve the action,
//! strict bodies, the outbox row committed with the receipt, receipts that
//! join their rows' states, and undo that never claims to undo the external
//! write.

mod common;

use std::sync::Arc;

use serde_json::{Map, Value, json};

use onto_core::action_types::{
    ActionPlan, ActionType, ApplyOptions, NOT_REVERSIBLE_NOTE, PlannedExternal, RECEIPT_NOTE, Risk, UNATTENDED_KINDS,
};
use onto_core::automate::NS_WRITEBACK_TARGET;
use onto_core::outbox::{Attempt, ConfigEvent, OutboxKind, OutboxQuery, OutboxState};
use onto_core::store::{MemoryStore, Store};
use onto_core::{ActionContext, EntityInput, Mutation, Ontology, OntologyOptions, Principal, Role, Schema};

const BODY: &str = r#"{ "issue": "{issue}", "title": "{issue.title}" }"#;

fn close_ticket(unattended: &[&str], risk: Risk, body: Value) -> ActionType {
    serde_json::from_value(json!({
        "name": "close_ticket",
        "description": "Resolve an issue and close its helpdesk ticket.",
        "risk": risk,
        "parameters": { "issue": { "type": "entity", "kinds": ["issue"], "required": true } },
        "preconditions": [{ "check": "open", "param": "issue", "message": "The issue is already resolved or closed." }],
        "effects": [
            { "patch": { "id": "{issue}", "state": "resolved" } },
            { "writeback": { "target": "helpdesk", "route": "close", "body": body } }
        ],
        "unattended": unattended,
    }))
    .unwrap()
}

fn body() -> Value {
    serde_json::from_str(BODY).unwrap()
}

fn admin() -> ActionContext {
    ActionContext::new(Principal::admin("lead"), "acme")
}

fn target(workspaces: &[&str], routes: Value) -> Value {
    json!({
        "name": "helpdesk",
        "base_url": "https://helpdesk.example.com/api/",
        "routes": routes,
        "workspaces": workspaces,
        "restricted": false,
        "version": 1,
        "created_by": "lead",
        "created_at": 0,
    })
}

fn put_target(o: &Ontology, v: Value) {
    o.commit_config(
        &admin(),
        vec![(NS_WRITEBACK_TARGET.into(), "helpdesk".into(), v)],
        ConfigEvent::new("writeback.add", Some("writeback:helpdesk"), json!({})),
    )
    .unwrap();
}

/// An ontology with `close_ticket`, issue:1 open, and the `helpdesk` target
/// serving `acme` with a `close` route.
fn world(store: Arc<dyn Store>, writeback: bool, at: ActionType) -> Ontology {
    let mut schema = Schema::enterprise();
    schema.action_types.insert(at.name.clone(), at);
    let o = Ontology::open(schema, store, OntologyOptions { writeback, ..Default::default() }).unwrap();
    o.apply(&admin(), vec![Mutation::Upsert(EntityInput::new("issue", "1", "Payment timeout").state("open"))]).unwrap();
    put_target(&o, target(&["acme"], json!({ "close": "tickets/close" })));
    o
}

fn memory(writeback: bool, at: ActionType) -> Ontology {
    world(Arc::new(MemoryStore::default()), writeback, at)
}

fn args() -> Map<String, Value> {
    json!({ "issue": "issue:1" }).as_object().cloned().unwrap()
}

fn plan(o: &Ontology, p: &Principal) -> ActionPlan {
    o.plan_action(&ActionContext::new(p.clone(), "acme"), "close_ticket", &args()).unwrap()
}

fn person() -> Principal {
    Principal::member("bob", "acme")
}

fn agent() -> Principal {
    Principal::new("assistant", vec![Role::Agent], vec!["acme".into()])
}

/// The service principal of an automation run (`automation:<name>`).
fn automation() -> Principal {
    let mut p = Principal::new("automation:close_blocked", vec![Role::Member], vec!["acme".into()]);
    p.via = Some("automation:close_blocked".into());
    p
}

fn failed(p: &ActionPlan) -> String {
    assert!(!p.valid, "{p:?}");
    p.checks.iter().filter(|c| !c.ok).map(|c| c.message.clone()).collect::<Vec<_>>().join("; ")
}

#[test]
fn validation_counts_writeback_in_the_one_of_rule_and_checks_names_and_unattended() {
    let schema = Schema::enterprise();
    close_ticket(&["agent", "automation"], Risk::Low, body()).validate(&schema).unwrap();
    assert_eq!(UNATTENDED_KINDS, ["agent", "automation"]);
    let mut two = close_ticket(&[], Risk::Low, body());
    two.effects[1].patch = two.effects[0].patch.clone();
    let e = two.validate(&schema).unwrap_err();
    assert!(e.contains("exactly one of patch, link, unlink, create, delete, event, writeback"), "{e}");
    let mut templated = close_ticket(&[], Risk::Low, body());
    templated.effects[1].writeback.as_mut().unwrap().target = "{issue}".into();
    assert!(templated.validate(&schema).unwrap_err().contains("writeback target '{issue}' must be a name"));
    let e = close_ticket(&["person"], Risk::Low, body()).validate(&schema).unwrap_err();
    assert!(e.contains("unattended: 'person' is not a caller kind"), "{e}");
    let e = close_ticket(&[], Risk::Low, json!({ "x": "{nope}" })).validate(&schema).unwrap_err();
    assert!(e.contains("unknown parameter {nope}"), "{e}");
    // Existing action types serialize as before (closure hashes stay put).
    let resolve = serde_json::to_value(&schema.action_types["resolve"]).unwrap();
    assert!(resolve.get("unattended").is_none());
    assert!(resolve["effects"].as_array().unwrap().iter().all(|e| e.get("writeback").is_none()));
}

#[test]
fn an_external_write_is_not_reversible_and_needs_confirmation_unless_unattended() {
    let o = memory(true, close_ticket(&[], Risk::Low, body()));
    for p in [person(), agent(), automation()] {
        let pl = plan(&o, &p);
        assert!(pl.valid, "{:?}", pl.checks);
        assert!(!pl.reversible && pl.needs_confirmation, "{}: {pl:?}", p.id);
        assert_eq!(pl.risk, Risk::Medium, "an external write is at least medium risk");
        assert_eq!(pl.graph_reversible, Some(true));
        assert_eq!(
            pl.external,
            vec![PlannedExternal {
                effect: 1,
                target: "helpdesk".into(),
                route: "close".into(),
                body: json!({ "issue": "issue:1", "title": "Payment timeout" }),
            }]
        );
        let line = pl.preview.iter().find(|l| l.starts_with("writeback helpdesk.close ")).expect("a preview line");
        assert!(line.ends_with("[async]"), "{line}");
        assert!(pl.warnings.iter().any(|w| w == NOT_REVERSIBLE_NOTE));
    }

    // `unattended` waives the confirmation for that caller kind only.
    let o = memory(true, close_ticket(&["automation"], Risk::Low, body()));
    let auto = plan(&o, &automation());
    assert!(!auto.needs_confirmation && !auto.reversible && auto.risk == Risk::Medium, "{auto:?}");
    assert!(plan(&o, &agent()).needs_confirmation);
    assert!(plan(&o, &person()).needs_confirmation);
    let o = memory(true, close_ticket(&["agent"], Risk::Low, body()));
    assert!(!plan(&o, &agent()).needs_confirmation);
    assert!(plan(&o, &automation()).needs_confirmation);

    // Its declared risk still applies.
    let o = memory(true, close_ticket(&["agent", "automation"], Risk::Medium, body()));
    assert!(plan(&o, &agent()).needs_confirmation && plan(&o, &automation()).needs_confirmation);

    // An agent cannot confirm it; an unattended agent applies it.
    let o = memory(true, close_ticket(&[], Risk::Low, body()));
    let r = o
        .apply_action(
            &ActionContext::new(agent(), "acme"),
            "close_ticket",
            &args(),
            ApplyOptions { confirm: true, dry_run: false },
        )
        .unwrap();
    assert_eq!(r.status, "proposed");
    assert!(r.reason.unwrap().contains("is an agent and cannot confirm it"));
    let o = memory(true, close_ticket(&["agent"], Risk::Low, body()));
    let r =
        o.apply_action(&ActionContext::new(agent(), "acme"), "close_ticket", &args(), ApplyOptions::default()).unwrap();
    assert_eq!(r.status, "applied", "{:?}", r.reason);
    assert_eq!(r.external.len(), 1);
}

#[test]
fn writeback_is_refused_while_off_or_when_the_target_does_not_serve_the_action() {
    // `[writeback] enabled = false` (the default): the plan is refused.
    let o = memory(false, close_ticket(&["agent", "automation"], Risk::Low, body()));
    let msg = failed(&plan(&o, &person()));
    assert!(msg.contains("writeback is off on this server ([writeback] enabled = false)"), "{msg}");
    let r = o.apply_action(&admin(), "close_ticket", &args(), ApplyOptions { confirm: true, dry_run: false }).unwrap();
    assert_eq!(r.status, "rejected");
    assert_eq!(o.get(&Principal::admin("x"), "issue:1").unwrap().state.as_deref(), Some("open"), "nothing applied");
    assert!(o.store().outbox_list(&OutboxQuery::default()).unwrap().is_empty());

    let o = memory(true, close_ticket(&[], Risk::Low, body()));
    put_target(&o, target(&["other"], json!({ "close": "tickets/close" })));
    assert!(failed(&plan(&o, &person())).contains("target helpdesk does not serve workspace acme"));
    put_target(&o, target(&["*"], json!({ "reopen": "tickets/reopen" })));
    assert!(failed(&plan(&o, &person())).contains("target helpdesk has no route close"));
    put_target(&o, Value::Null);
    assert!(failed(&plan(&o, &person())).contains("target helpdesk is not registered"));

    // Bodies render strictly: a placeholder without a value refuses the plan,
    // whether the field is missing or the entity is not visible.
    let o = memory(true, close_ticket(&[], Risk::Low, json!({ "a": { "b": ["{issue.nope}"] } })));
    let msg = failed(&plan(&o, &person()));
    assert!(msg.contains("invalid_template") && msg.contains("{issue.nope}"), "{msg}");
}

fn check_commit_receipt_and_undo(o: Ontology) {
    let ctx = admin();
    let held = o.apply_action(&ctx, "close_ticket", &args(), ApplyOptions::default()).unwrap();
    assert_eq!(held.status, "proposed");
    let reason = held.reason.unwrap();
    assert!(reason.contains("not reversible (an external write: action.undo does not undo it)"), "{reason}");
    assert!(o.store().outbox_list(&OutboxQuery::default()).unwrap().is_empty(), "nothing enqueued unconfirmed");

    let before = o.store().head().unwrap();
    let r = o.apply_action(&ctx, "close_ticket", &args(), ApplyOptions { confirm: true, dry_run: false }).unwrap();
    assert_eq!(r.status, "applied", "{:?}", r.reason);
    assert_eq!(o.store().head().unwrap(), before + 1, "edits, receipt and outbox row in one commit");
    assert_eq!(r.external.len(), 1);
    let x = &r.external[0];
    let key = format!("{}:1", r.id);
    assert_eq!((x.target.as_str(), x.route.as_str(), x.idempotency_key.as_str()), ("helpdesk", "close", key.as_str()));
    assert_eq!(x.state, OutboxState::Pending);
    assert_eq!(r.note.as_deref(), Some(RECEIPT_NOTE));
    let row = o.store().outbox_get(&x.outbox).unwrap().expect("the outbox row");
    assert_eq!(row.kind, OutboxKind::Writeback);
    assert_eq!(
        (row.target.as_str(), row.dedupe.as_str(), row.event_type.as_str()),
        ("helpdesk", key.as_str(), "close")
    );
    assert_eq!((row.source.as_str(), row.workspace.as_str()), ("action", "acme"));
    assert_eq!(row.receipt.as_deref(), Some(r.id.as_str()));
    assert_eq!(row.payload, json!({ "issue": "issue:1", "title": "Payment timeout" }));
    assert_eq!(o.get(&Principal::admin("x"), "issue:1").unwrap().state.as_deref(), Some("resolved"));

    // Reading the receipt joins its rows' current states.
    let claimed = o.store().outbox_claim(10, 60_000, "worker", &[]).unwrap();
    assert_eq!(claimed.len(), 1);
    let ok = Attempt { status: Some(200), ..Default::default() };
    assert!(
        o.store().outbox_finish(&row.id, claimed[0].claim.as_deref().unwrap(), &ok, OutboxState::Delivered, 0).unwrap()
    );
    let read = o.action_receipt(&ctx.principal, &r.id).unwrap();
    assert_eq!((read.external[0].state, read.external[0].attempts), (OutboxState::Delivered, 1));

    // Undo never claims to undo the external write: refused without force,
    // and with force only the graph edits are undone.
    let e = o.undo_action(&ctx, &r.id, false).unwrap_err().to_string();
    assert!(e.contains("not reversible: action.undo does not undo it") && e.contains("force=true"), "{e}");
    let u = o.undo_action(&ctx, &r.id, true).unwrap();
    assert_eq!(u.status, "undone");
    let reason = u.reason.unwrap();
    assert!(reason.contains("the external write was not undone") && reason.contains(NOT_REVERSIBLE_NOTE), "{reason}");
    assert_eq!(o.get(&Principal::admin("x"), "issue:1").unwrap().state.as_deref(), Some("open"));
    assert_eq!(o.store().outbox_list(&OutboxQuery::default()).unwrap().len(), 1, "nothing new was sent");
}

#[test]
fn a_confirmed_writeback_commits_its_outbox_row_with_the_receipt() {
    check_commit_receipt_and_undo(memory(true, close_ticket(&[], Risk::Low, body())));
}

#[test]
fn postgres_commits_the_writeback_row_with_the_receipt() {
    let Some(t) = common::TestTenant::new("writeback_commit") else { return };
    check_commit_receipt_and_undo(world(Arc::new(t.store()), true, close_ticket(&[], Risk::Low, body())));
}
