//! Automation definitions (docs/design/EVENTS.md Part C, build step 10):
//! defined and changed through the live schema (apply, or propose then
//! approve), limited at apply and propose but never at load, renames follow
//! into them, and lint and schema plans show what they reference, what they
//! may do, the loops they form and the enabled automations a change would
//! stop.

mod common;

use std::sync::Arc;

use serde_json::{Value, json};

use onto_core::automate::{self, AutomationState, Enablements, NS_AUTOMATION_STATE};
use onto_core::lint::{self, CLOSURE_CHANGED, LintQuery};
use onto_core::outbox::ConfigEvent;
use onto_core::store::{MemoryStore, Store};
use onto_core::{
    ActionContext, Decision, EntityInput, Error, Governance, Ontology, OntologyOptions, Principal, ProposalInput,
    ProposalStatus, Role, Schema, SchemaChangeSet,
};

fn set(v: Value) -> SchemaChangeSet {
    serde_json::from_value(v).expect("change set")
}

fn admin() -> Principal {
    Principal::admin("ada")
}

fn agent() -> Principal {
    Principal::new("assistant", vec![Role::Agent], vec!["*".into()])
}

/// Resolve a blocked issue (the design's example, with an enterprise action type).
fn resolve_blocked() -> Value {
    json!({
        "description": "Resolve a high-priority issue when it becomes blocked.",
        "run_as": { "roles": ["member"], "workspaces": ["acme"] },
        "trigger": {
            "on": "change", "kinds": ["issue"], "ops": ["entity.update"],
            "state": { "to": ["blocked"] }, "sources": ["!connector:*"]
        },
        "conditions": [{ "check": "prop_eq", "param": "subject", "prop": "priority", "value": "high" }],
        "effects": [
            { "action": { "type": "resolve", "params": { "issue": "{trigger.id}", "note": "auto: {subject.title}" } } },
            { "webhook": { "endpoint": "ops_pager", "event": "issue.resolved", "payload": { "id": "{trigger.id}" } } }
        ]
    })
}

fn put(name: &str, def: Value) -> Value {
    json!({ "op": "put_automation", "name": name, "def": def })
}

fn onto(opts: OntologyOptions) -> Ontology {
    let o = Ontology::open(Schema::enterprise(), Arc::new(MemoryStore::default()), opts).unwrap();
    o.upsert(&ActionContext::new(admin(), "acme"), EntityInput::new("issue", "1", "Timeout").state("triaged")).unwrap();
    o
}

#[test]
fn automations_are_defined_through_the_live_schema() {
    let o = onto(OntologyOptions::default());
    let ctx = ActionContext::new(admin(), "acme");
    let plan =
        o.schema_plan(&admin(), &set(json!({ "changes": [put("resolve_blocked", resolve_blocked())] }))).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
    assert!(!plan.breaking);
    assert_eq!(plan.changes[0].op, "put_automation");
    let summary = plan.changes[0].summary.clone();
    assert!(summary.contains("add automation resolve_blocked") && summary.contains("not enabled"), "{summary}");
    assert_eq!(plan.diff.entries[0].section, "automations");

    let applied = o.schema_apply(&ctx, set(json!({ "changes": [put("resolve_blocked", resolve_blocked())] }))).unwrap();
    assert_eq!(applied.version.version, 2);
    let s = o.schema();
    let a = &s.automations["resolve_blocked"];
    assert_eq!(a.action().unwrap().ty, "resolve");
    assert_eq!(a.endpoints(), ["ops_pager"]);
    assert_eq!(o.schema_status(&admin()).unwrap().automations, 1);
    // Defining is not enabling.
    assert!(o.store().kv_scan(NS_AUTOMATION_STATE, "").unwrap().is_empty());
    // Every schema reader sees the definition (schema.get is the document).
    let doc = serde_json::to_value(&*o.schema()).unwrap();
    assert_eq!(doc["automations"]["resolve_blocked"]["trigger"]["on"], "change");
    let viewer = Principal::viewer("vic", "acme");
    assert_eq!(o.schema_status(&viewer).unwrap().automations, 1);

    // Revert and undo carry automations like any section.
    let undo = o.schema_undo(&ctx, 2, Vec::new(), true).unwrap();
    assert_eq!(undo.changes.iter().map(|c| c.op()).collect::<Vec<_>>(), ["delete_automation"]);

    // Invalid definitions are refused with the automation named.
    let mut bad = resolve_blocked();
    bad["effects"][0]["action"]["type"] = json!("nope");
    let err = o.schema_apply(&ctx, set(json!({ "changes": [put("broken", bad)] }))).unwrap_err();
    assert!(
        matches!(&err, Error::Invalid(m) if m.contains("automation broken: effect 0: unknown action type nope")),
        "{err}"
    );
    let err =
        o.schema_apply(&ctx, set(json!({ "changes": [put("Bad Name", resolve_blocked())] }))).unwrap_err().to_string();
    assert!(err.contains("automation name 'Bad Name' must match"), "{err}");
    let mut secret = resolve_blocked();
    secret["effects"][1]["webhook"]["payload"]["token"] = json!("env:PAGER_TOKEN");
    let err = o.schema_apply(&ctx, set(json!({ "changes": [put("leaky", secret)] }))).unwrap_err().to_string();
    assert!(err.contains("secret references") && !err.contains("PAGER_TOKEN"), "{err}");

    // An action type an automation runs cannot be deleted from under it.
    let err = o
        .schema_apply(&ctx, set(json!({ "changes": [{ "op": "delete_action_type", "name": "resolve" }] })))
        .unwrap_err()
        .to_string();
    assert!(err.contains("automation resolve_blocked: effect 0: unknown action type resolve"), "{err}");

    // Delete.
    o.schema_apply(&ctx, set(json!({ "changes": [{ "op": "delete_automation", "name": "resolve_blocked" }] })))
        .unwrap();
    assert!(o.schema().automations.is_empty());
    let err = o
        .schema_apply(&ctx, set(json!({ "changes": [{ "op": "delete_automation", "name": "resolve_blocked" }] })))
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown automation resolve_blocked"), "{err}");
}

#[test]
fn agents_propose_automations_and_an_approver_applies_them() {
    let o = onto(OntologyOptions { governance: Governance::default(), ..Default::default() });
    let ctx_agent = ActionContext::new(agent(), "acme");
    let changes = json!([put("resolve_blocked", resolve_blocked())]);
    // Agents never apply directly.
    let err = o.schema_apply(&ctx_agent, set(json!({ "changes": changes }))).unwrap_err();
    assert!(matches!(&err, Error::Denied(m) if m.contains("agent")), "{err}");
    let input: ProposalInput = serde_json::from_value(json!({
        "title": "Resolve blocked P1s",
        "rationale": "Blocked high-priority issues wait for a person today.",
        "changes": changes,
    }))
    .unwrap();
    let filed = o.propose_schema(&ctx_agent, input).unwrap();
    assert_eq!(filed.proposal.status, ProposalStatus::Open);
    assert!(filed.proposal.plan.valid);
    assert!(o.schema().automations.is_empty(), "a proposal changes nothing");
    let err = o.review_proposal(&ctx_agent, &filed.proposal.id, Decision::Approve, "", None).unwrap_err();
    assert!(matches!(err, Error::Denied(_)));
    let out = o
        .review_proposal(&ActionContext::new(admin(), "acme"), &filed.proposal.id, Decision::Approve, "ok", None)
        .unwrap();
    assert_eq!(out.proposal.proposal.status, ProposalStatus::Applied);
    assert!(o.schema().automations.contains_key("resolve_blocked"));
    // Approving the definition enables nothing.
    assert!(o.store().kv_scan(NS_AUTOMATION_STATE, "").unwrap().is_empty());
}

#[test]
fn the_automation_limit_holds_at_apply_and_propose_but_never_at_load() {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::default());
    let roomy = Ontology::open(Schema::enterprise(), store.clone(), OntologyOptions::default()).unwrap();
    let ctx = ActionContext::new(admin(), "acme");
    roomy
        .schema_apply(&ctx, set(json!({ "changes": [put("one", resolve_blocked()), put("two", resolve_blocked())] })))
        .unwrap();
    drop(roomy);

    // Lowering the limit below what is stored never stops the tenant opening,
    // nor changes that add no automation.
    let tight = Ontology::open(
        Schema::enterprise(),
        store.clone(),
        OntologyOptions { max_automations: 1, ..Default::default() },
    )
    .unwrap();
    assert_eq!(tight.schema().automations.len(), 2);
    let mut edited = resolve_blocked();
    edited["description"] = json!("Edited.");
    tight.schema_apply(&ctx, set(json!({ "changes": [put("two", edited)] }))).unwrap();
    tight.schema_apply(&ctx, set(json!({ "changes": [{ "op": "delete_automation", "name": "two" }] }))).unwrap();

    // At the limit, adding one is refused in the plan, at apply and at propose.
    let add = set(json!({ "changes": [put("three", resolve_blocked())] }));
    let plan = tight.schema_plan(&admin(), &add).unwrap();
    assert!(!plan.valid);
    assert!(plan.errors.iter().any(|e| e.contains("would hold 2 automations; the limit is 1")), "{:?}", plan.errors);
    let err = tight.schema_apply(&ctx, add.clone()).unwrap_err();
    assert!(matches!(&err, Error::Invalid(m) if m.contains("max_automations")), "{err}");
    let input: ProposalInput =
        serde_json::from_value(json!({ "title": "One more", "changes": [put("three", resolve_blocked())] })).unwrap();
    let err = tight.propose_schema(&ActionContext::new(agent(), "acme"), input).unwrap_err();
    assert!(matches!(&err, Error::Invalid(m) if m.contains("max_automations")), "{err}");
    // Swapping one for another adds none.
    tight
        .schema_apply(
            &ctx,
            set(json!({ "changes": [
                { "op": "delete_automation", "name": "one" }, put("three", resolve_blocked())
            ] })),
        )
        .unwrap();
    assert_eq!(tight.schema().automations.keys().collect::<Vec<_>>(), ["three"]);
}

#[test]
fn renames_follow_into_automation_definitions() {
    let o = onto(OntologyOptions::default());
    let ctx = ActionContext::new(admin(), "acme");
    let mut def = resolve_blocked();
    def["conditions"] = json!([
        { "check": "prop_eq", "param": "subject", "prop": "priority", "value": "high" },
        { "check": "no_link", "param": "subject", "rel": "blocked_by" }
    ]);
    def["effects"][1]["webhook"]["payload"] = json!({ "id": "{trigger.id}", "p": "{subject.priority}" });
    let mut links = resolve_blocked();
    links["trigger"] = json!({ "on": "change", "kinds": ["issue"], "ops": ["link.create"], "rels": ["^blocked_by"] });
    o.schema_apply(&ctx, set(json!({ "changes": [put("on_issue", def), put("on_link", links)] }))).unwrap();

    o.schema_apply(
        &ctx,
        set(json!({ "changes": [{ "op": "rename_property", "object_type": "issue", "from": "priority", "to": "severity" }] })),
    )
    .unwrap();
    o.schema_apply(
        &ctx,
        set(json!({ "changes": [{ "op": "rename_link_type", "from": "blocked_by", "to": "held_by" }] })),
    )
    .unwrap();
    o.schema_apply(&ctx, set(json!({ "changes": [{ "op": "rename_object_type", "from": "issue", "to": "ticket" }] })))
        .unwrap();

    let s = o.schema();
    let a = serde_json::to_value(&s.automations["on_issue"]).unwrap();
    assert_eq!(a["trigger"]["kinds"], json!(["ticket"]));
    assert_eq!(a["conditions"][0]["prop"], "severity");
    assert_eq!(a["conditions"][1]["rel"], "held_by");
    assert_eq!(a["effects"][1]["webhook"]["payload"]["p"], "{subject.severity}");
    let b = serde_json::to_value(&s.automations["on_link"]).unwrap();
    assert_eq!(b["trigger"]["rels"], json!(["^held_by"]));
    assert_eq!(b["trigger"]["kinds"], json!(["ticket"]));
}

/// Enable `name` as its closure stands now (what `automation.enable` will
/// write in build step 11).
fn enable(o: &Ontology, name: &str) {
    let enabled = Enablements::load(&**o.store()).unwrap();
    let s = o.schema();
    let c = automate::closure(&s, name, &s.automations[name], &enabled.endpoints, &enabled.targets);
    let state = AutomationState {
        enabled: true,
        closure_hash: c.hash,
        closure: c.items,
        schema_version: o.schema_version(),
        enabled_by: "ada".into(),
        enabled_at: 1,
        expires_at: i64::MAX,
        paused: None,
    };
    o.commit_config(
        &ActionContext::new(admin(), "acme"),
        vec![(NS_AUTOMATION_STATE.into(), name.into(), serde_json::to_value(&state).unwrap())],
        ConfigEvent::new("automation.enable", Some(name), json!({})),
    )
    .unwrap();
}

#[test]
fn plans_and_proposals_show_the_enabled_automations_a_change_would_stop() {
    let o = onto(OntologyOptions::default());
    let ctx = ActionContext::new(admin(), "acme");
    o.schema_apply(
        &ctx,
        set(json!({ "changes": [put("resolve_blocked", resolve_blocked()), put("idle", resolve_blocked())] })),
    )
    .unwrap();
    enable(&o, "resolve_blocked");
    assert!(o.schema_lint(&admin(), LintQuery::default()).unwrap().findings.iter().all(|f| f.check != CLOSURE_CHANGED));

    // Raising the risk of the action type it runs changes its closure.
    let mut resolve = serde_json::to_value(&o.schema().action_types["resolve"]).unwrap();
    resolve["risk"] = json!("medium");
    let change = set(json!({ "changes": [{ "op": "put_action_type", "name": "resolve", "def": resolve }] }));
    let plan = o.schema_plan(&admin(), &change).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
    let stops: Vec<&str> = plan.lint.iter().map(|f| f.target.as_str()).collect();
    assert_eq!(stops, ["automation:resolve_blocked"], "only enabled automations are listed");
    assert_eq!(plan.lint[0].check, CLOSURE_CHANGED);
    assert!(plan.lint[0].message.contains("action_type:resolve changed"), "{}", plan.lint[0].message);
    assert!(
        plan.risk_reasons.iter().any(|r| r.contains("stops enabled automation:resolve_blocked")),
        "{:?}",
        plan.risk_reasons
    );
    // A change elsewhere stops nothing.
    let other =
        set(json!({ "changes": [{ "op": "put_object_type", "name": "runbook", "def": { "label": "Runbook" } }] }));
    assert!(o.schema_plan(&admin(), &other).unwrap().lint.is_empty());
    // Registering the endpoint it reaches changes its closure too.
    o.commit_config(
        &ctx,
        vec![(
            onto_core::outbox::NS_WEBHOOK_ENDPOINT.into(),
            "ops_pager".into(),
            json!({ "name": "ops_pager", "version": 1, "principal": { "roles": ["viewer"], "workspaces": ["acme"] } }),
        )],
        ConfigEvent::new("webhook.add", Some("ops_pager"), json!({})),
    )
    .unwrap();
    let lint = o
        .schema_lint(&admin(), LintQuery { checks: Some(vec![CLOSURE_CHANGED.into()]), ..Default::default() })
        .unwrap();
    assert_eq!(lint.findings.len(), 1);
    assert!(lint.findings[0].message.contains("endpoint:ops_pager changed"), "{}", lint.findings[0].message);
    enable(&o, "resolve_blocked");

    // On a proposal, for the proposer and for the reviewer.
    let input: ProposalInput =
        serde_json::from_value(json!({ "title": "Riskier resolve", "changes": change.changes })).unwrap();
    let filed = o.propose_schema(&ActionContext::new(agent(), "acme"), input).unwrap();
    assert_eq!(filed.proposal.plan.lint.len(), 1, "{:?}", filed.proposal.plan.lint);
    let seen = o.get_proposal(&admin(), &filed.proposal.id).unwrap();
    assert_eq!(seen.proposal.plan.lint[0].target, "automation:resolve_blocked");

    // Once applied, lint reports it as stopped; enabling again clears it.
    o.review_proposal(&ctx, &filed.proposal.id, Decision::Approve, "", None).unwrap();
    let lint = o.schema_lint(&admin(), LintQuery::default()).unwrap();
    let stale: Vec<&str> =
        lint.findings.iter().filter(|f| f.check == CLOSURE_CHANGED).map(|f| f.target.as_str()).collect();
    assert_eq!(stale, ["automation:resolve_blocked"]);
    // Feedback can refer to the finding.
    let id = lint.findings.iter().find(|f| f.check == CLOSURE_CHANGED).unwrap().id.clone();
    o.lint_feedback(&ctx, &id, onto_core::LintDecision::Accept, None).unwrap();
    enable(&o, "resolve_blocked");
    let lint = o.schema_lint(&admin(), LintQuery::default()).unwrap();
    assert!(lint.findings.iter().all(|f| f.check != CLOSURE_CHANGED));
    // Deleting an enabled automation would stop it.
    let plan = o
        .schema_plan(&admin(), &set(json!({ "changes": [{ "op": "delete_automation", "name": "resolve_blocked" }] })))
        .unwrap();
    assert_eq!(plan.lint.len(), 1);
}

fn lint_of(extra: &str) -> lint::LintReport {
    let mut s = Schema::enterprise();
    s.extend_toml(extra).unwrap();
    lint::lint(&s, None, &|_| true)
}

fn findings<'a>(r: &'a lint::LintReport, check: &str) -> Vec<(&'a str, &'a str)> {
    r.findings.iter().filter(|f| f.check == check).map(|f| (f.target.as_str(), f.message.as_str())).collect()
}

const PING_PONG: &str = r#"
[automations.resolve_when_blocked]
description = "Blocked issues get resolved."
run_as = { roles = ["member"], workspaces = ["acme"] }
trigger = { on = "change", kinds = ["issue"], ops = ["entity.update"], state = { to = ["blocked"] } }
effects = [{ action = { type = "resolve", params = { issue = "{trigger.id}" } } }]

[automations.block_when_resolved]
description = "Resolved issues get blocked again."
run_as = { roles = ["member"], workspaces = ["acme"] }
trigger = { on = "change", kinds = ["issue"], ops = ["entity.update"], state = { to = ["resolved"] } }
effects = [{ action = { type = "set_state", params = { item = "{trigger.id}", state = "blocked" } } }]
"#;

#[test]
fn lint_finds_automation_loops() {
    let r = lint_of(PING_PONG);
    let loops = findings(&r, "automation_loop");
    assert_eq!(loops.len(), 1, "{loops:?}");
    assert_eq!(loops[0].0, "automation:block_when_resolved, automation:resolve_when_blocked");
    assert!(loops[0].1.contains("block_when_resolved → resolve_when_blocked"), "{}", loops[0].1);
    assert!(loops[0].1.contains("depth guard"), "{}", loops[0].1);

    // Excluding automation-caused changes breaks the cycle.
    let r = lint_of(&PING_PONG.replace(
        r#"state = { to = ["resolved"] } }"#,
        r#"state = { to = ["resolved"] }, sources = ["!automation:*"] }"#,
    ));
    assert!(findings(&r, "automation_loop").is_empty());
    // So does a state guard the other's writes never reach.
    let r = lint_of(&PING_PONG.replace(r#"state = { to = ["resolved"] }"#, r#"state = { to = ["closed"] }"#));
    assert!(findings(&r, "automation_loop").is_empty());
    // So does a workspace the writer never acts in.
    let r = lint_of(
        &PING_PONG
            .replace(r#"state = { to = ["resolved"] } }"#, r#"state = { to = ["resolved"] }, workspaces = ["beta"] }"#),
    );
    assert!(findings(&r, "automation_loop").is_empty());

    // An automation re-triggering itself is a loop only with allow_self.
    let selfish = r#"
[automations.reassign]
description = "Keep assigning."
run_as = { roles = ["member"], workspaces = ["acme"] }
trigger = { on = "change", ops = ["link.create"], rels = ["assigned_to"] }
effects = [{ action = { type = "assign", params = { item = "{trigger.from}", assignee = "person:kenji" } } }]
"#;
    assert!(findings(&lint_of(selfish), "automation_loop").is_empty());
    let r = lint_of(&selfish.replace("run_as =", "allow_self = true\nrun_as ="));
    let loops = findings(&r, "automation_loop");
    assert_eq!(loops.len(), 1);
    assert!(loops[0].1.contains("can trigger itself"), "{}", loops[0].1);
}

#[test]
fn lint_reports_references_privilege_and_gap_blindness() {
    let r = lint_of(
        r#"
[action_types.triage]
description = "Triage an issue."
roles = ["triager"]
parameters.issue = { type = "entity", kinds = ["issue"], required = true }
[[action_types.triage.effects]]
patch = { id = "{issue}", state = "triaged" }

[automations.sloppy]
description = "References that do not resolve."
run_as = { roles = ["member"], workspaces = ["acme"] }
trigger = { on = "change", kinds = ["issue"], ops = ["entity.update"], props = ["nope"], state = { to = ["frozen"] } }
conditions = [{ check = "state_in", param = "subject", values = ["melted"] }, { check = "prop_set", param = "subject", prop = "zzz" }]
effects = [{ action = { type = "resolve", params = { extra = "x" } } }]

[automations.powerful]
description = "Admin everywhere."
run_as = { roles = ["admin"], workspaces = ["*"] }
trigger = { on = "change", kinds = ["issue"] }
effects = [{ webhook = { endpoint = "ops", event = "issue.changed" } }]

[automations.unable]
description = "Its run_as lacks the action type's role."
run_as = { roles = ["member"], workspaces = ["acme"] }
trigger = { on = "change", kinds = ["issue"], ops = ["entity.create"] }
effects = [{ action = { type = "triage", params = { issue = "{trigger.id}" } } }]

[automations.reader]
description = "Its run_as cannot write."
run_as = { roles = ["viewer"], workspaces = ["acme"] }
trigger = { on = "change", kinds = ["issue"], ops = ["entity.create"] }
effects = [{ action = { type = "resolve", params = { issue = "{trigger.id}" } } }]
"#,
    );
    let refs = findings(&r, "automation_reference");
    assert_eq!(refs.len(), 1, "{refs:?}");
    let (target, msg) = refs[0];
    assert_eq!(target, "automation:sloppy");
    for want in [
        "trigger property nope is not declared on issue",
        "trigger state frozen is not a state of issue",
        "condition 0: state melted is not a state of issue",
        "condition 1: property zzz is not declared on issue",
        "action type resolve has no parameter extra",
        "action type resolve requires parameter issue, which is not given",
    ] {
        assert!(msg.contains(want), "{want}: {msg}");
    }
    let lint::Severity::Error = r.findings.iter().find(|f| f.check == "automation_reference").unwrap().severity else {
        panic!("references are errors")
    };

    let priv_: Vec<(&str, &str)> = findings(&r, "automation_privilege");
    let by: std::collections::BTreeMap<&str, &str> = priv_.into_iter().collect();
    assert!(
        by["automation:powerful"].contains("runs as admin") && by["automation:powerful"].contains("every workspace")
    );
    assert!(
        by["automation:unable"].contains("holds none of the roles action triage allows (triager)"),
        "{}",
        by["automation:unable"]
    );
    assert!(by["automation:reader"].contains("cannot write"), "{}", by["automation:reader"]);
    assert!(!by.contains_key("automation:sloppy"));

    let blind = findings(&r, "automation_gap_blind");
    assert_eq!(blind.len(), 4, "one per change-triggered automation");
    assert!(blind.iter().all(|(_, m)| m.contains("gap records never trigger runs")));
    // Without the enablements, the closure check says it did not run.
    assert!(r.not_run.iter().any(|n| n.check == CLOSURE_CHANGED));
}

#[test]
fn postgres_keeps_definitions_and_has_the_runs_table() {
    let Some(pg) = common::TestTenant::new("automate") else { return };
    let o = pg.open(Schema::enterprise(), OntologyOptions::default());
    let ctx = ActionContext::new(admin(), "acme");
    o.schema_apply(&ctx, set(json!({ "changes": [put("resolve_blocked", resolve_blocked())] }))).unwrap();
    enable(&o, "resolve_blocked");
    drop(o);
    let again = pg.open(Schema::enterprise(), OntologyOptions { max_automations: 1, ..Default::default() });
    assert!(again.schema().automations.contains_key("resolve_blocked"));
    let enabled = Enablements::load(&**again.store()).unwrap();
    assert!(enabled.stale(&again.schema()).is_empty(), "the closure reads back the same");
    let migrations = pg.store().migrations().unwrap();
    assert!(migrations.iter().any(|m| m.0 == 12 && m.1 == "automate"), "{migrations:?}");
    // The limit holds on PostgreSQL too.
    let err = again.schema_apply(&ctx, set(json!({ "changes": [put("second", resolve_blocked())] }))).unwrap_err();
    assert!(err.to_string().contains("max_automations"), "{err}");
}
