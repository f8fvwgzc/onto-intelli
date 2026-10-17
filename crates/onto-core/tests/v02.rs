//! v0.2 features: action types, versions, merge, metrics, quality, fork, search, export.

use serde_json::{Map, Value, json};

use onto_core::action_types::ApplyOptions;
use onto_core::metrics::MetricQuery;
use onto_core::query::TraverseSpec;
use onto_core::{ActionContext, EntityInput, EntityPatch, Mutation, Ontology, Principal, Role, Schema};

fn params(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

fn admin() -> Principal {
    Principal::admin("x")
}

fn world() -> (Ontology, ActionContext) {
    let o = Ontology::in_memory(Schema::enterprise());
    let ctx = ActionContext::new(Principal::admin("lead").with_entity("person:kenji"), "acme");
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("team", "payments", "Payments Team")),
            Mutation::Upsert(EntityInput::new("team", "platform", "Platform Team")),
            Mutation::Upsert(
                EntityInput::new("person", "kenji", "Kenji Tanaka")
                    .prop("email", "kenji@acme.test")
                    .link("member_of", "team:payments"),
            ),
            Mutation::Upsert(EntityInput::new("person", "aiko", "Aiko Sato").link("member_of", "team:payments")),
            Mutation::Upsert(
                EntityInput::new("person", "ktanaka", "K. Tanaka")
                    .prop("email", "KENJI@acme.test")
                    .link("member_of", "team:payments"),
            ),
            Mutation::Upsert(
                EntityInput::new("project", "alpha", "Project Alpha").state("active").link("owned_by", "team:payments"),
            ),
            Mutation::Upsert(
                EntityInput::new("service", "pay", "Payment API").state("degraded").link("owned_by", "team:payments"),
            ),
            Mutation::Upsert(
                EntityInput::new("issue", "1", "Payment timeout")
                    .state("open")
                    .prop("priority", "high")
                    .link("belongs_to", "project:alpha")
                    .link("assigned_to", "person:kenji")
                    .link("affects", "service:pay"),
            ),
            Mutation::Upsert(
                EntityInput::new("task", "2", "Retry logic")
                    .state("todo")
                    .prop("estimate", 3.0)
                    .link("belongs_to", "project:alpha")
                    .link("assigned_to", "person:ktanaka"),
            ),
            Mutation::Upsert(
                EntityInput::new("task", "3", "Load test")
                    .state("todo")
                    .prop("estimate", 2.0)
                    .link("belongs_to", "project:alpha")
                    .link("depends_on", "task:2"),
            ),
            Mutation::Upsert(EntityInput::new("issue", "9", "Orphan bug").state("open")),
        ],
    )
    .unwrap();
    (o, ctx)
}

#[test]
fn assign_action_plans_applies_and_undoes() {
    let (o, ctx) = world();
    let plan = o
        .plan_action(
            &ctx,
            "assign",
            &params(json!({ "item": "issue:1", "assignee": "person:aiko", "note": "pairing" })),
        )
        .unwrap();
    assert!(plan.valid, "{:?}", plan.checks);
    assert!(plan.reversible);
    assert!(plan.preview.iter().any(|l| l.contains("replacing person:kenji")), "{:?}", plan.preview);
    assert!(plan.preview.iter().any(|l| l.contains("state open → triaged")), "{:?}", plan.preview);

    let r = o.apply_action(&ctx, "assign", &plan.params, ApplyOptions::default()).unwrap();
    assert_eq!(r.status, "applied", "{:?}", r.reason);
    let n = o.neighbors(&admin(), "issue:1", Some("assigned_to")).unwrap();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].1.id, "person:aiko");
    assert_eq!(o.get(&admin(), "issue:1").unwrap().state.as_deref(), Some("triaged"));

    let undone = o.undo_action(&ctx, &r.id, false).unwrap();
    assert_eq!(undone.status, "undone");
    let n = o.neighbors(&admin(), "issue:1", Some("assigned_to")).unwrap();
    assert_eq!(n[0].1.id, "person:kenji");
    assert_eq!(o.get(&admin(), "issue:1").unwrap().state.as_deref(), Some("open"));
    assert!(o.undo_action(&ctx, &r.id, false).is_err(), "double undo refused");
}

#[test]
fn preconditions_roles_and_confirmation_gate_actions() {
    let (o, ctx) = world();
    let bad = o
        .apply_action(
            &ctx,
            "assign",
            &params(json!({ "item": "issue:1", "assignee": "person:kenji" })),
            ApplyOptions::default(),
        )
        .unwrap();
    assert_eq!(bad.status, "rejected");
    assert!(bad.reason.unwrap().contains("Already assigned"));
    let wrong_kind =
        o.plan_action(&ctx, "assign", &params(json!({ "item": "person:aiko", "assignee": "person:kenji" }))).unwrap();
    assert!(!wrong_kind.valid);

    let viewer = ActionContext::new(Principal::new("v", vec![Role::Viewer], vec!["acme".into()]), "acme");
    let denied = o.plan_action(&viewer, "set_state", &params(json!({ "item": "task:2", "state": "done" }))).unwrap();
    assert!(!denied.valid);

    let del = o.apply_action(&ctx, "delete", &params(json!({ "target": "task:3" })), ApplyOptions::default()).unwrap();
    assert_eq!(del.status, "proposed", "high risk needs confirmation");
    let del = o
        .apply_action(
            &ctx,
            "delete",
            &params(json!({ "target": "task:3" })),
            ApplyOptions { confirm: true, ..Default::default() },
        )
        .unwrap();
    assert_eq!(del.status, "applied");
    assert!(o.get(&admin(), "task:3").is_err());
    o.undo_action(&ctx, &del.id, false).unwrap();
    let back = o.neighbors(&admin(), "task:3", Some("depends_on")).unwrap();
    assert_eq!(back[0].1.id, "task:2", "delete undo restores links");
}

#[test]
fn create_task_and_undo_conflict_detection() {
    let (o, ctx) = world();
    let r = o
        .apply_action(
            &ctx,
            "create_task",
            &params(json!({ "title": "Write docs", "project": "project:alpha", "estimate": "1.5" })),
            ApplyOptions::default(),
        )
        .unwrap();
    assert_eq!(r.status, "applied", "{:?}", r.reason);
    let id = r.result.as_ref().unwrap().entities[0].clone();
    let t = o.get(&admin(), &id).unwrap();
    assert_eq!(t.props["estimate"], json!(1.5));
    assert!(t.props.get("priority").is_none(), "missing optional params are dropped");
    o.apply(
        &ctx,
        vec![Mutation::Patch(EntityPatch { id: id.clone(), state: Some("in_progress".into()), ..Default::default() })],
    )
    .unwrap();
    assert!(o.undo_action(&ctx, &r.id, false).is_err(), "changed since: needs force");
    o.undo_action(&ctx, &r.id, true).unwrap();
    assert!(o.get(&admin(), &id).is_err());
}

#[test]
fn versions_history_as_of_and_changes() {
    let (o, ctx) = world();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let t0 = onto_core::time::now_ms();
    std::thread::sleep(std::time::Duration::from_millis(5));
    o.apply(
        &ctx,
        vec![Mutation::Patch(EntityPatch {
            id: "issue:1".into(),
            state: Some("in_progress".into()),
            ..Default::default()
        })],
    )
    .unwrap();
    let h = o.history(&admin(), "issue:1", 10).unwrap();
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].changed, vec!["state"]);
    assert_eq!(h[1].changed, vec!["created"]);
    assert_eq!(o.as_of(&admin(), "issue:1", t0).unwrap().unwrap().state.as_deref(), Some("open"));
    let ch = o.changes(&admin(), t0, None, &[], None, 50).unwrap();
    assert_eq!(ch.len(), 1, "{ch:?}");
    assert_eq!(ch[0].from_state.as_deref(), Some("open"));
    assert_eq!(ch[0].to_state.as_deref(), Some("in_progress"));
}

#[test]
fn duplicates_merge_and_redirects() {
    let (o, ctx) = world();
    let d = o.duplicates(&admin(), None, Some("person"), 0.75).unwrap();
    assert!(
        d.iter().any(|p| [p.a.as_str(), p.b.as_str()] == ["person:kenji", "person:ktanaka"]
            || [p.a.as_str(), p.b.as_str()] == ["person:ktanaka", "person:kenji"]),
        "{d:?}"
    );
    let m = o.merge(&ctx, "person:kenji", "person:ktanaka").unwrap();
    assert_eq!(m.links_moved, 2);
    assert_eq!(o.get(&admin(), "person:ktanaka").unwrap().id, "person:kenji", "old id redirects");
    let work = o.neighbors(&admin(), "person:ktanaka", Some("works_on")).unwrap();
    assert_eq!(work.len(), 2, "{work:?}");
    assert!(o.get(&admin(), "person:kenji").unwrap().aliases.contains(&"K. Tanaka".to_string()));
    o.link(&ctx, "task:3", "assigned_to", "person:ktanaka").unwrap();
    assert!(o.neighbors(&admin(), "task:3", Some("assigned_to")).unwrap().iter().any(|(_, e)| e.id == "person:kenji"));
}

#[test]
fn derived_properties_and_metrics() {
    let (o, _) = world();
    let d = o.derived(&admin(), "project:alpha").unwrap();
    assert_eq!(d["open_issues"], json!(1));
    assert_eq!(d["open_tasks"], json!(2));
    assert_eq!(d["remaining_days"], json!(5.0));
    let m = o
        .metric(
            &admin(),
            &MetricQuery { metric: Some("open_work".into()), group_by: vec!["kind".into()], ..Default::default() },
        )
        .unwrap();
    assert_eq!(m.value, 4.0);
    let tasks = m.rows.iter().find(|r| r.group["kind"] == "task").unwrap();
    assert_eq!(tasks.value, 2.0);
    let by_team = o
        .metric(
            &admin(),
            &MetricQuery { metric: Some("open_work".into()), group_by: vec!["team".into()], ..Default::default() },
        )
        .unwrap();
    assert!(by_team.rows.iter().any(|r| r.group["team"] == "Payments Team"), "{by_team:?}");
    let effort =
        o.metric(&admin(), &MetricQuery { metric: Some("remaining_effort".into()), ..Default::default() }).unwrap();
    assert_eq!(effort.value, 5.0);
    let adhoc = o
        .metric(
            &admin(),
            &MetricQuery { kind: Some("tickets".into()), group_by: vec!["priority".into()], ..Default::default() },
        )
        .unwrap();
    assert_eq!(adhoc.count, 2);
}

#[test]
fn quality_report_finds_problems() {
    let (o, _) = world();
    let q = o.quality(&admin(), None, None, 100).unwrap();
    assert!(q.score < 100.0);
    assert!(q.violations.iter().any(|v| v.rule == "missing_link" && v.entity == "issue:9"));
    assert!(q.violations.iter().any(|v| v.rule == "unique_violation"));
    assert!(q.violations.iter().any(|v| v.rule == "orphan" && v.entity == "issue:9"));
}

#[test]
fn fork_isolates_what_if_edits() {
    let (o, ctx) = world();
    let f = o.fork();
    f.apply(
        &ctx,
        vec![Mutation::Patch(EntityPatch { id: "task:2".into(), state: Some("done".into()), ..Default::default() })],
    )
    .unwrap();
    assert_eq!(f.get(&admin(), "task:2").unwrap().state.as_deref(), Some("done"));
    assert_eq!(o.get(&admin(), "task:2").unwrap().state.as_deref(), Some("todo"));
    let before = o.critical_path(&admin(), "project:alpha").unwrap();
    let after = f.critical_path(&admin(), "project:alpha").unwrap();
    assert!(after.cost < before.cost, "{} vs {}", after.cost, before.cost);
}

#[test]
fn hybrid_search_personalizes() {
    let (o, _) = world();
    let me = Principal::new("kenji", vec![Role::Member], vec!["acme".into()]).with_entity("person:kenji");
    let hits = o.search(&me, "paymnt timeout", &[], None, 5).unwrap();
    assert_eq!(hits[0].entity.id, "issue:1", "{hits:?}");
    let sig = hits[0].signals.as_ref().unwrap();
    assert_eq!(sig["personal_distance"], json!(1));
}

#[test]
fn profile_suggest_and_export() {
    let (o, ctx) = world();
    o.apply(
        &ctx,
        vec![Mutation::Upsert(
            EntityInput::new("vendor", "acme-cloud", "Acme Cloud").prop("tier", "gold").link("supplies", "service:pay"),
        )],
    )
    .unwrap();
    let s = o.suggest_schema(&admin(), None).unwrap();
    assert!(s.toml.contains("[object_types.vendor]"), "{}", s.toml);
    assert!(s.toml.contains("[link_types.supplies]"), "{}", s.toml);
    let mut schema = Schema::enterprise();
    schema.extend_toml(&s.toml).expect("suggestion is a valid schema layer");
    let ttl = o.export_turtle(&admin(), None, true, true, None).unwrap();
    assert!(ttl.contains("owl:inverseOf"));
    assert!(ttl.contains("<https://onto-intelli.dev/entity/issue:1>"));
    let mm = o
        .export_mermaid(
            &admin(),
            &TraverseSpec { start: vec!["issue:1".into()], depth: 1, limit: 20, ..Default::default() },
        )
        .unwrap();
    assert!(mm.starts_with("graph LR"));
    assert!(mm.contains("-->|assigned_to|"));
}

#[test]
fn experts_rank_people_by_connection() {
    let (o, _) = world();
    let ex = o.experts(&admin(), None, "payment timeout", &[], 5).unwrap();
    assert_eq!(ex[0].id, "person:kenji", "{ex:?}");
    assert!(ex[0].reasons.iter().any(|r| r.contains("assigned to Payment timeout")), "{ex:?}");
}
