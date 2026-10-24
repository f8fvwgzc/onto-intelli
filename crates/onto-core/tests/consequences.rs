//! Semantic schema plans: consequences for entailment, conservativity, risk,
//! schema locks, likely renames, and the opt-in consequences of action plans.

mod common;

use serde_json::{Map, Value, json};

use onto_core::live_schema::Conservativity;
use onto_core::reason::Fact;
use onto_core::{
    ActionContext, Decision, EntityInput, Error, EventFilter, Mutation, Ontology, OntologyOptions, Principal,
    ProposalInput, ProposalStatus, Risk, Role, Schema, SchemaChangeSet, SchemaPlan,
};

fn set(v: Value) -> SchemaChangeSet {
    serde_json::from_value(v).expect("change set")
}

fn admin() -> Principal {
    Principal::admin("ada")
}

fn agent() -> Principal {
    Principal::new("bot", vec![Role::Agent], vec!["*".into()])
}

/// Definitions nothing else in the enterprise schema refers to, so tests can
/// delete them.
const EXTRA: &str = r#"
[object_types.runbook]
description = "How to operate a service."
implements = ["knowledge_item"]
states = ["draft", "published"]
properties.url = { type = "string" }
properties.steps = { type = "text" }

[link_types.operates]
from = ["team"]
to = ["service"]
inverse = "operated_by"
"#;

fn schema() -> Schema {
    let mut s = Schema::enterprise();
    s.extend_toml(EXTRA).unwrap();
    s
}

fn seed(o: &Ontology, ctx: &ActionContext) {
    o.apply(
        ctx,
        vec![
            Mutation::Upsert(EntityInput::new("team", "payments", "Payments Team")),
            Mutation::Upsert(EntityInput::new("person", "kenji", "Kenji Tanaka").link("member_of", "team:payments")),
            Mutation::Upsert(EntityInput::new("person", "aiko", "Aiko Sato").link("member_of", "team:payments")),
            Mutation::Upsert(EntityInput::new("project", "alpha", "Project Alpha").state("active")),
            Mutation::Upsert(
                EntityInput::new("issue", "1", "Payment timeout")
                    .state("triaged")
                    .prop("priority", "high")
                    .link("belongs_to", "project:alpha")
                    .link("assigned_to", "person:kenji"),
            ),
            Mutation::Upsert(
                EntityInput::new("issue", "2", "Refund bug")
                    .state("open")
                    .prop("priority", "low")
                    .link("belongs_to", "project:alpha"),
            ),
            Mutation::Upsert(EntityInput::new("task", "c", "Provision").state("todo").prop("estimate", 1.0)),
            Mutation::Upsert(
                EntityInput::new("task", "b", "Build").state("todo").prop("estimate", 2.0).link("depends_on", "task:c"),
            ),
            Mutation::Upsert(
                EntityInput::new("task", "a", "Ship").state("todo").prop("estimate", 3.0).link("depends_on", "task:b"),
            ),
            Mutation::Upsert(EntityInput::new("runbook", "deploy", "Deploy").state("published").prop("url", "/d")),
            Mutation::Upsert(EntityInput::new("runbook", "rollback", "Rollback").state("draft")),
        ],
    )
    .unwrap();
}

fn world() -> (Ontology, ActionContext) {
    let o = Ontology::in_memory(schema());
    let ctx = ActionContext::new(admin(), "acme");
    seed(&o, &ctx);
    (o, ctx)
}

/// `put_link_type depends_on` with the current definition, made transitive.
fn transitive_depends_on(o: &Ontology) -> SchemaChangeSet {
    let mut def = serde_json::to_value(&o.schema().link_types["depends_on"]).unwrap();
    def["transitive"] = json!(true);
    set(
        json!({ "message": "dependencies chain", "changes": [{ "op": "put_link_type", "name": "depends_on", "def": def }] }),
    )
}

fn fact(s: &str, p: &str, o: &str) -> Fact {
    Fact::new(s, p, o)
}

fn params(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

#[test]
fn transitive_flip_touches_no_data_but_is_not_conservative() {
    let (o, ctx) = world();
    let s = transitive_depends_on(&o);
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.applicable(), "{plan:?}");
    assert!(!plan.breaking);
    assert_eq!(plan.entities_affected, 0, "a flag flip touches no entity");
    assert_eq!(plan.links_affected, 0, "a flag flip touches no link");
    assert_eq!(plan.changes[0].links, 0);

    let c = plan.consequences.as_ref().expect("consequences");
    assert!(c.skipped.is_none(), "{c:?}");
    assert!(c.fixpoint_reached);
    assert_eq!(c.removed, 0);
    // The schema now asserts `depends_on a transitive`, and the reasoner
    // derives the one chain a -> b -> c.
    assert_eq!(c.added, 2, "{c:?}");
    assert_eq!(c.by_rule.get("asserted"), Some(&1));
    assert_eq!(c.by_rule.get("builtin/transitive"), Some(&1));
    assert_eq!(c.new_over_existing_vocabulary, 2, "both facts use only existing names");
    assert!(!c.conservative);
    assert_eq!(c.verdict, Conservativity::NotConservative);
    let derived = c
        .witnesses
        .iter()
        .find(|w| w.fact == fact("<task:a>", "depends_on", "<task:c>"))
        .expect("the chain is a witness");
    assert_eq!(derived.rule.as_deref(), Some("builtin/transitive"));
    assert_eq!(
        derived.premises,
        vec![
            fact("depends_on", "a", "transitive"),
            fact("<task:a>", "depends_on", "<task:b>"),
            fact("<task:b>", "depends_on", "<task:c>"),
        ]
    );
    assert!(c.means.contains("engine's opinion"), "{}", c.means);

    assert_eq!(plan.risk, Risk::Medium, "{:?}", plan.risk_reasons);
    assert!(plan.risk_reasons.iter().any(|r| r.starts_with("not conservative")), "{:?}", plan.risk_reasons);

    // The commit carries the same block, computed before the write lock.
    let applied = o.schema_apply(&ctx, s).unwrap();
    let ac = applied.plan.consequences.as_ref().expect("commit consequences");
    assert_eq!(ac.added, 2);
    assert_eq!(applied.plan.risk, Risk::Medium);
    let ev = o.events(&admin(), &EventFilter { action: Some("schema.change".into()), ..Default::default() }).unwrap();
    assert!(ev.iter().any(|e| e.payload["risk"] == "medium"), "{ev:?}");
}

#[test]
fn a_rule_with_new_names_is_conservative_and_low_risk() {
    let (o, _) = world();
    let s = set(json!({ "changes": [
        { "op": "put_rule", "def": {
            "name": "assignee_team", "description": "the team of whoever is assigned",
            "chain": ["assigned_to", "member_of"], "infer": "assignee_team" } },
        { "op": "put_rule", "def": {
            "name": "hot_issue", "description": "high-priority issues are hot",
            "when": [["?i", "a", "issue"], ["?i", "prop.priority", "high"]],
            "then": ["?i", "a", "hot_issue"] } }
    ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.applicable(), "{plan:?}");
    let c = plan.consequences.as_ref().unwrap();
    // The new rule reads prop.priority, so the planned closure asserts the
    // priority facts the current one never read; they are store data, not
    // consequences, and are left out.
    assert_eq!(c.added, 2, "{c:?}");
    assert_eq!(c.by_rule.get("schema/assignee_team"), Some(&1));
    assert_eq!(c.by_rule.get("schema/hot_issue"), Some(&1));
    assert!(c.samples_added.iter().any(|a| a.fact == fact("<issue:1>", "assignee_team", "<team:payments>")));
    assert!(c.samples_added.iter().any(|a| a.fact == fact("<issue:1>", "a", "#hot_issue")));
    assert_eq!(c.new_over_existing_vocabulary, 0);
    assert!(c.witnesses.is_empty());
    assert!(c.conservative);
    assert_eq!(c.verdict, Conservativity::Conservative);
    assert_eq!(plan.risk, Risk::Low, "{:?}", plan.risk_reasons);
    assert!(plan.risk_reasons.is_empty());
}

#[test]
fn redefining_an_existing_rule_is_not_conservative() {
    let (o, _) = world();
    // `owning_team` already exists as a derived relation; deriving it along a
    // new chain says something new in old words.
    let s = set(json!({ "changes": [{ "op": "put_rule", "def": {
        "name": "assigned_team_owns", "chain": ["assigned_to", "member_of"], "infer": "owning_team" } }] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    let c = plan.consequences.as_ref().unwrap();
    assert_eq!(c.verdict, Conservativity::NotConservative, "{c:?}");
    assert!(c.witnesses.iter().any(|w| w.fact == fact("<issue:1>", "owning_team", "<team:payments>")));
    assert_eq!(plan.risk, Risk::Medium);
}

#[test]
fn deleting_a_type_in_use_is_high_risk() {
    let (o, _) = world();
    let bare = set(json!({ "changes": [{ "op": "delete_object_type", "name": "runbook" }] }));
    let plan = o.schema_plan(&admin(), &bare).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
    assert!(!plan.needs_migration.is_empty());
    assert_eq!(plan.risk, Risk::High);
    assert!(plan.risk_reasons[0].starts_with("breaking"), "{:?}", plan.risk_reasons);
    assert!(plan.risk_reasons.iter().any(|r| r.contains("have no migration")));
    assert!(plan.risk_reasons.iter().any(|r| r.contains("delete_object_type runbook")));
    // Over the same graph the runbooks keep their stored kind, but the schema
    // no longer says `#runbook implements #knowledge_item`.
    let c = plan.consequences.as_ref().unwrap();
    assert!(c.samples_removed.contains(&fact("#runbook", "implements", "#knowledge_item")), "{c:?}");
    assert!(c.samples_removed.contains(&fact("<runbook:deploy>", "a", "#knowledge_item")));
    assert_eq!(c.removed, 3, "{c:?}");

    let migrated = set(json!({ "changes": [{ "op": "delete_object_type", "name": "runbook",
        "migrations": [{ "action": "delete_entities" }] }] }));
    let plan = o.schema_plan(&admin(), &migrated).unwrap();
    assert!(plan.applicable(), "{plan:?}");
    assert_eq!(plan.risk, Risk::High, "still breaking: data is deleted");
}

#[test]
fn deleting_an_unused_definition_is_medium_risk() {
    let (o, _) = world();
    let plan = o
        .schema_plan(&admin(), &set(json!({ "changes": [{ "op": "delete_link_type", "name": "deployed_to" }] })))
        .unwrap();
    assert!(plan.applicable(), "{plan:?}");
    assert!(!plan.breaking);
    assert_eq!(plan.risk, Risk::Medium, "{:?}", plan.risk_reasons);
    assert!(plan.risk_reasons.iter().any(|r| r.contains("delete_link_type deployed_to")));
}

#[test]
fn locks_refuse_deletes_until_an_admin_overrides() {
    let (o, ctx) = world();
    let lock = o.schema_lock(&ctx, "object_types", "runbook", "on-call depends on it").unwrap();
    assert_eq!(lock.locked_by, "ada");
    assert_eq!(o.schema_locks(&admin()).unwrap(), vec![lock.clone()]);
    // Locking again keeps the first lock.
    assert_eq!(o.schema_lock(&ctx, "object_types", "runbook", "other").unwrap(), lock);
    let locks_events =
        o.events(&admin(), &EventFilter { action: Some("schema.lock".into()), ..Default::default() }).unwrap();
    assert_eq!(locks_events.len(), 1, "a repeated lock writes nothing");

    let delete = json!({ "changes": [{ "op": "delete_object_type", "name": "runbook",
        "migrations": [{ "action": "delete_entities" }] }] });
    let plan = o.schema_plan(&admin(), &set(delete.clone())).unwrap();
    assert_eq!(plan.locked_violations.len(), 1, "{plan:?}");
    let v = &plan.locked_violations[0];
    assert_eq!((v.section.as_str(), v.name.as_str()), ("object_types", "runbook"));
    assert!(!v.overridden);
    assert_eq!(v.lock_reason, "on-call depends on it");
    assert!(!plan.applicable());
    assert_eq!(plan.risk, Risk::High);
    match o.schema_apply(&ctx, set(delete.clone())) {
        Err(Error::Conflict(m)) => {
            assert!(m.contains("locked definitions") && m.contains("object_types/runbook"), "{m}")
        }
        other => panic!("expected a lock conflict, got {other:?}"),
    }
    assert!(o.schema().object_types.contains_key("runbook"));

    // A non-admin cannot override (and could not apply anyway): the plan
    // says the override is ignored.
    let mut over = delete.clone();
    over["override_locks"] = json!(true);
    let member = Principal::member("bob", "acme");
    let plan = o.schema_plan(&member, &set(over.clone())).unwrap();
    assert!(!plan.locked_violations[0].overridden);
    assert!(plan.warnings.iter().any(|w| w.contains("override_locks is ignored")), "{:?}", plan.warnings);

    // An admin override applies, stays visible in the plan and the audit
    // event, and releases the lock of the deleted definition.
    let plan = o.schema_plan(&admin(), &set(over.clone())).unwrap();
    assert!(plan.applicable(), "{plan:?}");
    assert!(plan.locked_violations[0].overridden);
    let applied = o.schema_apply(&ctx, set(over)).unwrap();
    assert!(applied.plan.locked_violations[0].overridden);
    assert!(!o.schema().object_types.contains_key("runbook"));
    let ev = o.events(&admin(), &EventFilter { action: Some("schema.change".into()), ..Default::default() }).unwrap();
    let last = ev.iter().find(|e| e.payload["version"] == applied.version.version).unwrap();
    assert_eq!(last.payload["override_locks"]["by"], "ada");
    assert_eq!(last.payload["override_locks"]["violations"][0]["name"], "runbook");
    assert_eq!(last.payload["override_locks"]["released"], json!(["object_types/runbook"]));
    assert!(o.schema_locks(&admin()).unwrap().is_empty(), "the lock went with its definition");
}

#[test]
fn breaking_edits_of_locked_definitions_are_violations_but_safe_edits_are_not() {
    let (o, ctx) = world();
    o.schema_lock(&ctx, "properties", "issue.priority", "reports group by it").unwrap();
    o.schema_lock(&ctx, "object_types", "issue", "").unwrap();
    // Adding a property to a locked type is fine.
    let safe = set(json!({ "changes": [{ "op": "put_property", "object_type": "issue", "name": "sla",
        "def": { "type": "string" } }] }));
    let plan = o.schema_plan(&admin(), &safe).unwrap();
    assert!(plan.locked_violations.is_empty(), "{:?}", plan.locked_violations);
    assert!(plan.applicable());
    // Narrowing the enum breaks issue:1 (priority high), even with a migration.
    let narrow = set(json!({ "changes": [{ "op": "put_property", "object_type": "issue", "name": "priority",
        "def": { "type": "enum", "values": ["low", "medium"] },
        "migrations": [{ "action": "set_default", "value": "medium" }] }] }));
    let plan = o.schema_plan(&admin(), &narrow).unwrap();
    assert!(plan.valid && plan.needs_migration.is_empty(), "{plan:?}");
    let names: Vec<&str> = plan.locked_violations.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["issue", "issue.priority"], "{:?}", plan.locked_violations);
    assert!(plan.locked_violations.iter().all(|v| v.reason.starts_with("a breaking change")));
    assert!(!plan.applicable());
}

#[test]
fn an_overridden_rename_moves_the_lock() {
    let (o, ctx) = world();
    o.schema_lock(&ctx, "object_types", "task", "core").unwrap();
    o.schema_lock(&ctx, "properties", "task.estimate", "planning").unwrap();
    let s = set(json!({ "override_locks": true,
        "changes": [{ "op": "rename_object_type", "from": "task", "to": "chore" }] }));
    let applied = o.schema_apply(&ctx, s).unwrap();
    assert_eq!(applied.plan.locked_violations.len(), 2, "{:?}", applied.plan.locked_violations);
    let keys: Vec<String> = o.schema_locks(&admin()).unwrap().iter().map(|l| l.key()).collect();
    assert_eq!(keys, ["object_types/chore", "properties/chore.estimate"]);
}

#[test]
fn only_admins_lock_and_unlock() {
    let (o, ctx) = world();
    for p in [
        Principal::member("bob", "acme"),
        agent(),
        Principal::new("root-bot", vec![Role::Admin, Role::Agent], vec!["*".into()]),
    ] {
        let c = ActionContext::new(p.clone(), "acme");
        assert!(matches!(o.schema_lock(&c, "object_types", "task", "x"), Err(Error::Denied(_))), "{}", p.id);
    }
    let denied =
        o.events(&admin(), &EventFilter { action: Some("authorization.denied".into()), ..Default::default() }).unwrap();
    assert_eq!(denied.len(), 3, "every refusal is audited");
    assert!(matches!(o.schema_lock(&ctx, "object_types", "nope", ""), Err(Error::NotFound(_))));
    assert!(matches!(o.schema_lock(&ctx, "colors", "red", ""), Err(Error::Invalid(_))));
    assert!(matches!(o.schema_lock(&ctx, "properties", "issue.due", ""), Err(Error::NotFound(_))));

    o.schema_lock(&ctx, "link_types", "operates", "").unwrap();
    let member = ActionContext::new(Principal::member("bob", "acme"), "acme");
    assert!(matches!(o.schema_unlock(&member, "link_types", "operates"), Err(Error::Denied(_))));
    let released = o.schema_unlock(&ctx, "link_types", "operates").unwrap();
    assert_eq!(released.name, "operates");
    assert!(o.schema_locks(&admin()).unwrap().is_empty());
    assert!(matches!(o.schema_unlock(&ctx, "link_types", "operates"), Err(Error::NotFound(_))));
    let unlocks =
        o.events(&admin(), &EventFilter { action: Some("schema.unlock".into()), ..Default::default() }).unwrap();
    assert_eq!(unlocks.len(), 1);
    assert_eq!(unlocks[0].payload["lock"], "link_types/operates");
    // Unlocked, the definition can go.
    let s = set(json!({ "changes": [{ "op": "delete_link_type", "name": "operates" }] }));
    assert!(o.schema_plan(&admin(), &s).unwrap().applicable());
}

#[test]
fn locks_guard_proposals_at_approval() {
    let (o, ctx) = world();
    let bot = ActionContext::new(agent(), "acme");
    let reviewer = ActionContext::new(Principal::admin("lead"), "acme");
    let drop_operates = |title: &str, override_locks: bool| -> ProposalInput {
        serde_json::from_value(json!({
            "title": title, "override_locks": override_locks,
            "changes": [{ "op": "delete_link_type", "name": "operates" }]
        }))
        .unwrap()
    };
    // Filed before the lock: approval re-plans, so the lock still guards it.
    let filed = o.propose_schema(&bot, drop_operates("drop operates", false)).unwrap();
    assert!(filed.proposal.plan.locked_violations.is_empty());
    o.schema_lock(&ctx, "link_types", "operates", "on-call").unwrap();
    match o.review_proposal(&reviewer, &filed.proposal.id, Decision::Approve, "ok", None) {
        Err(Error::Conflict(m)) => assert!(m.contains("locked definitions"), "{m}"),
        other => panic!("expected a lock conflict, got {other:?}"),
    }
    let failed = o.get_proposal(&admin(), &filed.proposal.id).unwrap();
    assert_eq!(failed.proposal.status, ProposalStatus::Failed);
    assert!(failed.proposal.error.unwrap_or_default().contains("locked"));
    assert!(o.schema().link_types.contains_key("operates"));

    // An agent cannot smuggle an override through an admin's approval.
    let filed = o.propose_schema(&bot, drop_operates("drop operates, overriding", true)).unwrap();
    assert_eq!(filed.proposal.plan.locked_violations.len(), 1, "the filed plan shows the lock");
    assert!(!filed.proposal.plan.locked_violations[0].overridden);
    assert_eq!(filed.proposal.plan.risk, Risk::High);
    assert!(matches!(
        o.review_proposal(&reviewer, &filed.proposal.id, Decision::Approve, "ok", None),
        Err(Error::Conflict(_))
    ));
    assert!(o.schema().link_types.contains_key("operates"));

    // An admin's proposal with an override, approved by another admin, applies.
    let filed = o.propose_schema(&ctx, drop_operates("drop operates (admin)", true)).unwrap();
    let outcome = o.review_proposal(&reviewer, &filed.proposal.id, Decision::Approve, "ok", None).unwrap();
    let applied = outcome.applied.expect("applied");
    assert!(applied.plan.locked_violations[0].overridden);
    assert!(!o.schema().link_types.contains_key("operates"));
    assert!(o.schema_locks(&admin()).unwrap().is_empty());
}

#[test]
fn delete_and_put_pairs_are_reported_as_likely_renames() {
    let (o, ctx) = world();
    o.schema_apply(
        &ctx,
        set(json!({ "changes": [
            { "op": "put_object_type", "name": "bug", "def": { "label": "Bug", "states": ["open", "fixed"],
                "properties": { "severity": { "type": "enum", "values": ["s1", "s2", "s3"] },
                                "steps": { "type": "text" } } } },
            { "op": "put_link_type", "name": "reviews", "def": { "from": ["person"], "to": ["issue"] } }
        ] })),
    )
    .unwrap();
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("bug", "1", "Crash").state("open").prop("severity", "s1")),
            Mutation::Upsert(EntityInput::new("bug", "2", "Typo").state("fixed")),
            Mutation::Upsert(EntityInput::new("person", "mia", "Mia").link("reviews", "issue:1")),
        ],
    )
    .unwrap();
    let s = set(json!({ "changes": [
        { "op": "delete_object_type", "name": "bug", "migrations": [{ "action": "delete_entities" }] },
        { "op": "put_object_type", "name": "defect", "def": { "label": "Defect", "states": ["open", "fixed"],
            "properties": { "severity": { "type": "enum", "values": ["s1", "s2", "s3"] },
                            "steps": { "type": "text" } } } },
        { "op": "put_object_type", "name": "invoice", "def": { "properties": { "amount": { "type": "float" } } } },
        { "op": "delete_link_type", "name": "reviews", "migrations": [{ "action": "delete_links" }] },
        { "op": "put_link_type", "name": "reviewed", "def": { "from": ["person"], "to": ["issue"] } },
        { "op": "delete_property", "object_type": "task", "name": "estimate",
          "migrations": [{ "action": "drop_values" }] },
        { "op": "put_property", "object_type": "task", "name": "estimate_days", "def": { "type": "float" } }
    ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.applicable(), "{plan:?}");
    let pairs: Vec<(&str, &str, &str)> =
        plan.likely_renames.iter().map(|r| (r.section.as_str(), r.from.as_str(), r.to.as_str())).collect();
    assert_eq!(
        pairs,
        [
            ("object_types", "bug", "defect"),
            ("link_types", "reviews", "reviewed"),
            ("properties", "task.estimate", "task.estimate_days")
        ],
        "{:?}",
        plan.likely_renames
    );
    assert!(plan.likely_renames.iter().all(|r| r.score >= 0.6 && r.score <= 1.0));
    assert!(plan.likely_renames[0].evidence.iter().any(|e| e.contains("properties: 2 of 2")));
    let w = |needle: &str| plan.warnings.iter().any(|w| w.contains(needle));
    assert!(w("bug looks renamed to defect (score "), "{:?}", plan.warnings);
    assert!(w("use rename_object_type to keep its 2 entities"), "{:?}", plan.warnings);
    assert!(w("use rename_link_type to keep its 1 links"), "{:?}", plan.warnings);
    assert!(w("task.estimate looks renamed to task.estimate_days"), "{:?}", plan.warnings);
    assert!(w("use rename_property to keep its 3 values"), "{:?}", plan.warnings);

    // Explicit renames are not guesses.
    let explicit = set(json!({ "changes": [{ "op": "rename_object_type", "from": "bug", "to": "defect" }] }));
    assert!(o.schema_plan(&admin(), &explicit).unwrap().likely_renames.is_empty());
}

#[test]
fn action_plans_report_consequences_on_request() {
    let (o, ctx) = world();
    let input = params(json!({ "item": "issue:2", "assignee": "person:aiko" }));
    let plain = o.plan_action(&ctx, "assign", &input).unwrap();
    assert!(plain.consequences.is_none());
    assert!(serde_json::to_value(&plain).unwrap().get("consequences").is_none());

    let plan = o.plan_action_consequences(&ctx, "assign", &input).unwrap();
    assert!(plan.valid, "{:?}", plan.checks);
    let c = plan.consequences.as_ref().unwrap();
    assert!(c.skipped.is_none(), "{c:?}");
    let added: Vec<&Fact> = c.samples_added.iter().map(|a| &a.fact).collect();
    assert!(added.contains(&&fact("<issue:2>", "assigned_to", "<person:aiko>")), "{added:?}");
    let team = c
        .samples_added
        .iter()
        .find(|a| a.fact == fact("<issue:2>", "handled_by_team", "<team:payments>"))
        .expect("handled_by_team is derived");
    assert_eq!(team.rule.as_deref(), Some("schema/responsible_team"));
    assert_eq!(c.by_rule.get("schema/responsible_team"), Some(&1));
    assert_eq!(c.removed, 0);
    // By the schema-plan definition a data edit over existing entities is
    // never conservative; the means says so.
    assert_eq!(c.verdict, Conservativity::NotConservative);
    assert!(c.means.contains("rarely meaningful"), "{}", c.means);
    // Nothing was written.
    let live = o.reason(&admin(), &Default::default()).unwrap();
    assert!(live.contains(&fact("<issue:2>", "handled_by_team", "<team:payments>")).is_none());

    // Reassigning removes the old derived team as well.
    let reassign = o
        .plan_action_consequences(&ctx, "assign", &params(json!({ "item": "issue:1", "assignee": "team:payments" })))
        .unwrap();
    let c = reassign.consequences.unwrap();
    assert!(c.samples_removed.contains(&fact("<issue:1>", "assigned_to", "<person:kenji>")), "{c:?}");
    assert!(c.samples_removed.contains(&fact("<issue:1>", "handled_by_team", "<team:payments>")));

    // An invalid plan says why it has no consequences.
    let bad = o.plan_action_consequences(&ctx, "assign", &params(json!({ "item": "issue:404" }))).unwrap();
    assert!(!bad.valid);
    assert!(bad.consequences.unwrap().skipped.is_some());
}

#[test]
fn plans_stored_before_these_fields_still_load() {
    let old = json!({
        "base_version": 3,
        "valid": true,
        "breaking": false,
        "changes": [{ "index": 0, "op": "put_object_type", "target": "incident",
            "summary": "add object type incident", "breaking": false, "entities": 0, "links": 0 }],
        "diff": { "entries": [] },
        "entities_affected": 0,
        "links_affected": 0,
        "migration": { "entities_updated": 0, "entities_deleted": 0, "entities_retyped": 0,
            "links_deleted": 0, "links_moved": 0, "mutations": 0 }
    });
    let plan: SchemaPlan = serde_json::from_value(old).expect("an old plan deserializes");
    assert!(plan.consequences.is_none());
    assert_eq!(plan.risk, Risk::Low);
    assert!(plan.locked_violations.is_empty() && plan.likely_renames.is_empty());
    assert!(plan.applicable());
    // New fields are omitted when empty, so a new plan without them reads as before.
    let out = serde_json::to_value(&plan).unwrap();
    assert!(out.get("locked_violations").is_none() && out.get("consequences").is_none());
    assert_eq!(out["risk"], "low");

    let set: SchemaChangeSet = serde_json::from_value(json!({ "changes": [] })).unwrap();
    assert!(!set.override_locks);
    assert!(serde_json::to_value(&set).unwrap().get("override_locks").is_none());
}

#[test]
fn locks_persist_in_postgres() {
    let Some(pg) = common::TestTenant::new("locks") else { return };
    let ctx = ActionContext::new(admin(), "acme");
    {
        let o = pg.open(schema(), OntologyOptions::default());
        seed(&o, &ctx);
        o.schema_lock(&ctx, "object_types", "runbook", "on-call").unwrap();
    }
    let o = pg.open(schema(), OntologyOptions::default());
    let locks = o.schema_locks(&admin()).unwrap();
    assert_eq!(locks.len(), 1);
    assert_eq!(locks[0].reason, "on-call");
    let s = set(json!({ "changes": [{ "op": "delete_object_type", "name": "runbook",
        "migrations": [{ "action": "delete_entities" }] }] }));
    assert!(matches!(o.schema_apply(&ctx, s), Err(Error::Conflict(_))));
    o.schema_unlock(&ctx, "object_types", "runbook").unwrap();
    let o = pg.open(schema(), OntologyOptions::default());
    assert!(o.schema_locks(&admin()).unwrap().is_empty());
    let plan = o.schema_plan(&admin(), &transitive_depends_on(&o)).unwrap();
    assert_eq!(plan.consequences.unwrap().verdict, Conservativity::NotConservative);
}

#[test]
fn proposals_show_each_reader_only_the_facts_it_may_see() {
    let (o, ctx) = world();
    o.apply(
        &ctx,
        vec![Mutation::Upsert(
            EntityInput::new("issue", "secret", "Layoff plan")
                .state("open")
                .prop("priority", "high")
                .prop("visibility", "restricted")
                .prop("acl", json!(["person:kenji"])),
        )],
    )
    .unwrap();
    let input: ProposalInput = serde_json::from_value(json!({
        "title": "hot issues",
        "changes": [{ "op": "put_rule", "def": {
            "name": "hot_issue", "when": [["?i", "a", "issue"], ["?i", "prop.priority", "high"]],
            "then": ["?i", "a", "hot_issue"] } }]
    }))
    .unwrap();
    let filed = o.propose_schema(&ctx, input).unwrap();
    // The proposer sees its own consequences, the restricted issue included.
    let mine = filed.proposal.plan.consequences.as_ref().unwrap();
    assert!(mine.samples_added.iter().any(|a| a.fact.s == "<issue:secret>"), "{mine:?}");

    let bob = Principal::member("bob", "acme");
    let read = o.get_proposal(&bob, &filed.proposal.id).unwrap();
    let theirs = read.proposal.plan.consequences.as_ref().unwrap();
    assert!(theirs.samples_added.iter().any(|a| a.fact.s == "<issue:1>"), "{theirs:?}");
    assert_eq!(theirs.added, mine.added - 1, "counts are the reader's too");
    let text = serde_json::to_string(&read).unwrap();
    assert!(!text.contains("issue:secret"), "{text}");
    // Listings return the stored plan, which holds no facts at all.
    for v in o.list_proposals(&bob, None, 10).unwrap() {
        let text = serde_json::to_string(&v).unwrap();
        assert!(!text.contains("issue:secret"), "{text}");
        let stored = v.proposal.plan.consequences.as_ref().unwrap();
        assert!(stored.skipped.as_deref().unwrap_or_default().contains("computed for each reader"), "{stored:?}");
        assert_eq!(stored.added, 0);
    }
}

#[test]
fn a_stale_preview_honours_an_override_only_as_approval_would() {
    let (o, ctx) = world();
    o.schema_lock(&ctx, "link_types", "operates", "on-call").unwrap();
    let bot = ActionContext::new(agent(), "acme");
    let input: ProposalInput = serde_json::from_value(json!({
        "title": "drop operates", "override_locks": true,
        "changes": [{ "op": "delete_link_type", "name": "operates" }]
    }))
    .unwrap();
    let filed = o.propose_schema(&bot, input).unwrap();
    o.schema_apply(
        &ctx,
        set(json!({ "changes": [{ "op": "put_property", "object_type": "issue", "name": "sla",
            "def": { "type": "string" } }] })),
    )
    .unwrap();
    let lead = Principal::admin("lead");
    let read = o.get_proposal(&lead, &filed.proposal.id).unwrap();
    assert!(read.stale);
    let preview = read.preview.expect("stale proposals get a preview");
    assert!(!preview.locked_violations[0].overridden, "{:?}", preview.locked_violations);
    assert!(!preview.applicable());
    assert_eq!(preview.risk, Risk::High);
    let reviewer = ActionContext::new(lead, "acme");
    assert!(matches!(
        o.review_proposal(&reviewer, &filed.proposal.id, Decision::Approve, "ok", None),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn locks_follow_chained_renames_and_deletes() {
    let (o, ctx) = world();
    o.schema_lock(&ctx, "object_types", "runbook", "").unwrap();
    o.schema_lock(&ctx, "link_types", "operates", "").unwrap();
    let s = set(json!({ "override_locks": true, "changes": [
        { "op": "rename_object_type", "from": "runbook", "to": "playbook" },
        { "op": "rename_object_type", "from": "playbook", "to": "guide" },
        { "op": "rename_link_type", "from": "operates", "to": "runs" },
        { "op": "delete_link_type", "name": "runs", "migrations": [{ "action": "delete_links" }] }
    ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    let seen: Vec<(&str, &str)> = plan.locked_violations.iter().map(|v| (v.name.as_str(), v.change.as_str())).collect();
    assert_eq!(
        seen,
        [
            ("runbook", "rename_object_type runbook → playbook"),
            ("runbook", "rename_object_type playbook → guide"),
            ("operates", "rename_link_type operates → runs"),
            ("operates", "delete_link_type runs"),
        ],
        "{:?}",
        plan.locked_violations
    );
    assert!(plan.locked_violations[3].reason.contains("named runs by then"), "{:?}", plan.locked_violations[3]);
    let applied = o.schema_apply(&ctx, s).unwrap();
    assert!(applied.plan.locked_violations.iter().all(|v| v.overridden));
    let keys: Vec<String> = o.schema_locks(&admin()).unwrap().iter().map(|l| l.key()).collect();
    assert_eq!(keys, ["object_types/guide"]);
    assert!(o.schema().object_types.contains_key("guide"));

    // The moved lock guards the new name.
    let s = set(json!({ "changes": [{ "op": "rename_object_type", "from": "guide", "to": "howto" }] }));
    assert!(!o.schema_plan(&admin(), &s).unwrap().applicable());
}

#[test]
fn a_change_set_that_does_not_validate_is_not_low_risk() {
    let (o, _) = world();
    let s = set(
        json!({ "changes": [{ "op": "put_link_type", "name": "zz", "def": { "from": ["nope"], "to": ["issue"] } }] }),
    );
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(!plan.valid);
    assert_eq!(plan.risk, Risk::High, "{:?}", plan.risk_reasons);
    assert!(plan.risk_reasons.iter().any(|r| r.contains("does not validate")), "{:?}", plan.risk_reasons);
    assert!(plan.risk_reasons.iter().any(|r| r.contains("conservativity not shown")), "{:?}", plan.risk_reasons);
}

#[test]
fn unrelated_definitions_with_alike_structure_are_not_renames() {
    let (o, _) = world();
    let s = set(json!({ "changes": [
        { "op": "delete_property", "object_type": "runbook", "name": "url", "migrations": [{ "action": "drop_values" }] },
        { "op": "put_property", "object_type": "runbook", "name": "owner_email", "def": { "type": "string" } },
        { "op": "delete_link_type", "name": "operates" },
        { "op": "put_link_type", "name": "sponsors", "def": { "from": ["team"], "to": ["service"] } }
    ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.likely_renames.is_empty(), "{:?}", plan.likely_renames);
    assert!(!plan.warnings.iter().any(|w| w.contains("looks renamed")), "{:?}", plan.warnings);
}

#[test]
fn refused_terms_make_conservativity_undetermined() {
    // `task:q>r` cannot be spelled: it and its links are left out of both
    // closures, so what the rule would add through it is not seen.
    let chain = |q: &str| {
        let o = Ontology::in_memory(schema());
        let ctx = ActionContext::new(admin(), "acme");
        let q_id = format!("task:{q}");
        o.apply(
            &ctx,
            vec![
                Mutation::Upsert(EntityInput::new("task", "s", "S")),
                Mutation::Upsert(EntityInput::new("task", q, "Q").link("depends_on", "task:s")),
                Mutation::Upsert(EntityInput::new("task", "p", "P").link("depends_on", &q_id)),
            ],
        )
        .unwrap();
        o
    };
    let rule = set(json!({ "changes": [{ "op": "put_rule", "def": {
        "name": "dep_chain", "description": "dependencies of dependencies",
        "when": [["?x", "depends_on", "?y"], ["?y", "depends_on", "?z"]],
        "then": ["?x", "depends_on", "?z"] } }] }));
    let plan = chain("q>r").schema_plan(&admin(), &rule).unwrap();
    let c = plan.consequences.as_ref().unwrap();
    assert_eq!(c.refused, 2, "one refusal per closure: {c:?}");
    assert_eq!(c.verdict, Conservativity::Undetermined, "{c:?}");
    assert!(!c.conservative);
    assert_eq!(plan.risk, Risk::Medium, "{:?}", plan.risk_reasons);
    assert!(plan.risk_reasons.iter().any(|r| r.contains("could not be spelled")), "{:?}", plan.risk_reasons);
    // The same graph with a spellable id shows the new fact.
    let control = chain("q").schema_plan(&admin(), &rule).unwrap();
    let c = control.consequences.as_ref().unwrap();
    assert_eq!((c.refused, c.verdict), (0, Conservativity::NotConservative), "{c:?}");
}

#[test]
fn plan_consequences_do_not_depend_on_the_closure_cache() {
    let (o, ctx) = world();
    let flip = transitive_depends_on(&o);
    let cold = o.schema_plan(&admin(), &flip).unwrap();
    o.reason(&admin(), &Default::default()).unwrap();
    let warm = o.schema_plan(&admin(), &flip).unwrap();
    assert_eq!(cold.consequences, warm.consequences);
    // A write between two plans is seen by the second one (the cached
    // closure of the older revision is not used).
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("task", "x1", "X1")),
            Mutation::Upsert(EntityInput::new("task", "x2", "X2").link("depends_on", "task:x1")),
            Mutation::Upsert(EntityInput::new("task", "x3", "X3").link("depends_on", "task:x2")),
        ],
    )
    .unwrap();
    let later = o.schema_plan(&admin(), &flip).unwrap();
    let c = later.consequences.as_ref().unwrap();
    assert!(c.samples_added.iter().any(|a| a.fact == fact("<task:x3>", "depends_on", "<task:x1>")), "{c:?}");
}

#[test]
fn a_plan_stored_without_a_risk_is_assessed_not_read_as_low() {
    use onto_core::store::{MemoryStore, Store};
    let store = std::sync::Arc::new(MemoryStore::default());
    let o = Ontology::open(schema(), store.clone(), OntologyOptions::default()).unwrap();
    let ctx = ActionContext::new(admin(), "acme");
    seed(&o, &ctx);
    let bot = ActionContext::new(agent(), "acme");
    let input: ProposalInput = serde_json::from_value(json!({
        "title": "drop operates", "changes": [{ "op": "delete_link_type", "name": "operates" }]
    }))
    .unwrap();
    let filed = o.propose_schema(&bot, input).unwrap();
    let id = filed.proposal.id.clone();
    let reviewer = ActionContext::new(Principal::admin("lead"), "acme");
    o.review_proposal(&reviewer, &id, Decision::Reject, "not now", None).unwrap();
    // Cut the stored plan down to the keys plans had before risk existed.
    let ns = onto_core::proposals::NS_PROPOSALS;
    let mut v = store.kv_get(ns, &id).unwrap().unwrap();
    let plan = v["plan"].as_object_mut().unwrap();
    for k in ["risk", "risk_reasons", "consequences", "shapes", "locked_violations", "likely_renames"] {
        plan.remove(k);
    }
    store.kv_put(ns, &id, &v, None).unwrap();
    let read = o.get_proposal(&admin(), &id).unwrap();
    assert_eq!(read.proposal.status, ProposalStatus::Rejected);
    assert_eq!(read.proposal.plan.risk, Risk::Medium, "{:?}", read.proposal.plan.risk_reasons);
    assert!(read.proposal.plan.risk_reasons.iter().any(|r| r.contains("delete_link_type operates")));
    assert!(read.proposal.plan.risk_reasons.iter().any(|r| r.contains("holds no consequences")));
    let listed = o.list_proposals(&admin(), None, 10).unwrap();
    assert_eq!(listed[0].proposal.plan.risk, Risk::Medium);
}

#[test]
fn deleting_a_link_type_forgets_its_glossary_words() {
    let glossed = || {
        let mut s = schema();
        s.extend_toml("[glossary.relations]\nruns = \"operates\"\nrun_by = \"operated_by\"\n").unwrap();
        Ontology::in_memory(s)
    };
    let drop = || set(json!({ "changes": [{ "op": "delete_link_type", "name": "operates" }] }));
    let o = glossed();
    let plan = o.schema_plan(&admin(), &drop()).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
    let ctx = ActionContext::new(admin(), "acme");
    o.schema_apply(&ctx, drop()).unwrap();
    let words = &o.schema().glossary.relations;
    assert!(!words.contains_key("runs") && !words.contains_key("run_by"), "{words:?}");
    assert_eq!(words.get("dependency").map(String::as_str), Some("depends_on"), "other words stay");
    // Deleting a word explicitly in the same set still works.
    let both = set(json!({ "changes": [
        { "op": "delete_link_type", "name": "operates" },
        { "op": "delete_glossary_term", "section": "relations", "word": "runs" }
    ] }));
    let plan = glossed().schema_plan(&admin(), &both).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
}
