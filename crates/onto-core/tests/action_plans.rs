//! Action plans: templates read only what the acting principal may see, and a
//! plan is checked again on the locked graph when it commits, in one process
//! and across replicas.

mod common;

use std::sync::{Arc, Barrier};

use serde_json::{Map, Value, json};

use onto_core::action_types::ApplyOptions;
use onto_core::{
    ActionContext, EntityInput, EventFilter, Mutation, Ontology, OntologyOptions, Outcome, Principal, Schema,
};

fn params(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

fn admin() -> Principal {
    Principal::admin("x")
}

/// Enterprise plus two action types that read a person named by a string
/// parameter, so no entity-parameter check stands in front of the template.
fn schema() -> Schema {
    let mut s = Schema::enterprise();
    s.extend_toml(
        r#"
[action_types.note_pay]
description = "Record a person's pay in an event."
risk = "low"
parameters.who = { type = "string", required = true }

[[action_types.note_pay.effects]]
event = { action = "pay.noted", payload = { who = "{who}", salary = "{who.salary}", title = "{who.title}" } }

[action_types.review_pay]
description = "Record a pay review; only for people whose salary is set."
risk = "low"
parameters.who = { type = "string", required = true }

[[action_types.review_pay.preconditions]]
check = "prop_set"
param = "who"
prop = "salary"

[[action_types.review_pay.effects]]
event = { action = "pay.reviewed", payload = { who = "{who}" } }
"#,
    )
    .unwrap();
    s
}

fn seed(o: &Ontology, ctx: &ActionContext) {
    o.apply(
        ctx,
        vec![
            Mutation::Upsert(EntityInput::new("issue", "1", "Payment timeout").state("open")),
            Mutation::Upsert(EntityInput::new("person", "aiko", "Aiko Sato").prop("salary", 90_000)),
            Mutation::Upsert(
                EntityInput::new("person", "boss", "The Boss")
                    .prop("salary", 250_000)
                    .prop("visibility", "restricted")
                    .prop("acl", json!([])),
            ),
        ],
    )
    .unwrap();
}

fn world() -> (Ontology, ActionContext) {
    let o = Ontology::in_memory(schema());
    let ctx = ActionContext::new(Principal::admin("lead"), "acme");
    seed(&o, &ctx);
    (o, ctx)
}

fn successes(o: &Ontology, action: &str) -> usize {
    o.events(&admin(), &EventFilter { action: Some(action.into()), ..Default::default() })
        .unwrap()
        .iter()
        .filter(|e| e.outcome == Outcome::Success)
        .count()
}

#[test]
fn string_parameter_cannot_read_an_invisible_entity() {
    let (o, lead) = world();
    let bob = ActionContext::new(Principal::member("bob", "acme"), "acme");
    let who = params(json!({ "who": "person:boss" }));

    let full = o.plan_action(&lead, "note_pay", &who).unwrap();
    let Some(Mutation::Event(ev)) = full.mutations.first() else { panic!("{full:?}") };
    assert_eq!(ev.payload, json!({ "who": "person:boss", "salary": 250_000, "title": "The Boss" }));

    // Bob may not see the restricted person: its fields have no value for him,
    // and action types stay lenient, so the keys are dropped, not filled.
    let r = o.apply_action(&bob, "note_pay", &who, ApplyOptions::default()).unwrap();
    assert_eq!(r.status, "applied", "{:?}", r.reason);
    let Some(Mutation::Event(ev)) = r.plan.mutations.first() else { panic!("{:?}", r.plan) };
    assert_eq!(ev.payload, json!({ "who": "person:boss" }));
    let stored = o.events(&admin(), &EventFilter { action: Some("pay.noted".into()), ..Default::default() }).unwrap();
    assert_eq!(stored.len(), 1);
    assert!(!stored[0].payload.to_string().contains("250000"), "{:?}", stored[0].payload);

    // A visible person reads as before.
    let aiko = o.plan_action(&bob, "note_pay", &params(json!({ "who": "person:aiko" }))).unwrap();
    let Some(Mutation::Event(ev)) = aiko.mutations.first() else { panic!("{aiko:?}") };
    assert_eq!(ev.payload["salary"], json!(90_000));
}

#[test]
fn checks_on_a_string_parameter_treat_an_invisible_entity_as_missing() {
    let (o, lead) = world();
    let bob = ActionContext::new(Principal::member("bob", "acme"), "acme");
    let boss = params(json!({ "who": "person:boss" }));
    assert!(o.plan_action(&lead, "review_pay", &boss).unwrap().valid);
    let hidden = o.plan_action(&bob, "review_pay", &boss).unwrap();
    assert!(!hidden.valid);
    let missing = o.plan_action(&bob, "review_pay", &params(json!({ "who": "person:nobody" }))).unwrap();
    let msgs = |p: &onto_core::action_types::ActionPlan| -> Vec<String> {
        p.checks.iter().filter(|c| !c.ok).map(|c| c.message.clone()).collect()
    };
    assert_eq!(msgs(&hidden), msgs(&missing), "hidden reads exactly like missing");
    assert!(o.plan_action(&bob, "review_pay", &params(json!({ "who": "person:aiko" }))).unwrap().valid);
}

#[test]
fn concurrent_applies_satisfy_a_precondition_once() {
    let (o, ctx) = world();
    let threads = 8;
    let gate = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let (o, ctx, gate) = (o.clone(), ctx.clone(), gate.clone());
            std::thread::spawn(move || {
                gate.wait();
                o.apply_action(&ctx, "resolve", &params(json!({ "issue": "issue:1" })), ApplyOptions::default())
                    .unwrap()
            })
        })
        .collect();
    let receipts: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let applied = receipts.iter().filter(|r| r.status == "applied").count();
    assert_eq!(applied, 1, "{:?}", receipts.iter().map(|r| (&r.status, &r.reason)).collect::<Vec<_>>());
    assert!(receipts.iter().all(|r| r.status == "applied" || r.status == "rejected"));
    assert_eq!(successes(&o, "action.resolve"), 1);
    assert_eq!(successes(&o, "issue.resolved"), 1);
}

#[cfg(feature = "postgres")]
#[test]
fn replicas_satisfy_a_precondition_once_on_postgres() {
    use onto_core::store::{PgOptions, PgStore};

    let Some(pg) = common::TestTenant::new("action_race") else { return };
    let a = pg.open(schema(), OntologyOptions::default());
    let ctx = ActionContext::new(Principal::admin("lead"), "acme");
    seed(&a, &ctx);
    // A second replica that does not follow store notices, so its graph stays
    // behind until a commit of its own loses the race and catches up.
    let store = PgStore::connect(&pg.url, &pg.tenant, PgOptions { listen: false, ..Default::default() }).unwrap();
    let b = Ontology::open(schema(), Arc::new(store), OntologyOptions::default()).unwrap();
    let resolve = params(json!({ "issue": "issue:1" }));

    let first = a.apply_action(&ctx, "resolve", &resolve, ApplyOptions::default()).unwrap();
    assert_eq!(first.status, "applied", "{:?}", first.reason);
    // b still plans against an open issue; the commit re-plans after catching up.
    assert!(b.plan_action(&ctx, "resolve", &resolve).unwrap().valid, "b has not caught up yet");
    let second = b.apply_action(&ctx, "resolve", &resolve, ApplyOptions::default()).unwrap();
    assert_eq!(second.status, "rejected");
    let reason = second.reason.unwrap();
    assert!(reason.starts_with("precondition_changed: "), "{reason}");
    assert_eq!(b.get(&admin(), "issue:1").unwrap().state.as_deref(), Some("resolved"));
    a.sync().unwrap();
    assert_eq!(successes(&a, "action.resolve"), 1);
    assert_eq!(successes(&a, "issue.resolved"), 1);
}
