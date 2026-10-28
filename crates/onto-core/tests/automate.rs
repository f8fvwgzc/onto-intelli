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
        from: None,
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

// ── runs (build step 11): claim, commit, recovery ───────────────────────

mod runtime {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::{Value, json};

    use onto_core::automate::{self, Automation, AutomationState, Enablements, NS_AUTOMATION_STATE};
    use onto_core::changes::{ChangeOp, Cursor};
    use onto_core::leases::{Fence, Lease, Timer, Wake};
    use onto_core::model::{Entity, Event, EventFilter, Id, Link};
    use onto_core::outbox::{ConfigEvent, NS_WEBHOOK_ENDPOINT, OutboxQuery};
    use onto_core::runs::{self, RetryPolicy, RunQuery, RunRow, RunState, RunTrigger, Status};
    use onto_core::store::{
        CommitRecord, MemoryStore, NoticeFn, Snapshot, Store, StoreStats, VersionRecord, WriteBatch,
    };
    use onto_core::{ActionContext, EntityInput, EntityPatch, Error, Mutation, Ontology, OntologyOptions, Principal};
    use onto_core::{Result, Schema};

    use super::{admin, common, put, resolve_blocked, set};

    /// A store that fails its next writes (a process that dies before its
    /// commit, or a database that is briefly unavailable).
    struct FlakyStore {
        inner: Arc<dyn Store>,
        fail: AtomicUsize,
        unavailable: std::sync::atomic::AtomicBool,
    }

    impl FlakyStore {
        fn new(inner: Arc<dyn Store>) -> Self {
            Self { inner, fail: AtomicUsize::new(0), unavailable: Default::default() }
        }
        /// Fail the next `n` writes: a crash (`Storage`) or unavailability.
        fn fail_next(&self, n: usize, unavailable: bool) {
            self.unavailable.store(unavailable, Ordering::SeqCst);
            self.fail.store(n, Ordering::SeqCst);
        }
    }

    impl Store for FlakyStore {
        fn load(&self) -> Result<Snapshot> {
            self.inner.load()
        }
        fn write(&self, b: &WriteBatch) -> Result<u64> {
            if self.fail.load(Ordering::SeqCst) > 0 {
                self.fail.fetch_sub(1, Ordering::SeqCst);
                return Err(if self.unavailable.load(Ordering::SeqCst) {
                    Error::Unavailable("simulated: the database is unavailable".into())
                } else {
                    Error::Storage("simulated crash before the commit".into())
                });
            }
            self.inner.write(b)
        }
        fn tracks_commits(&self) -> bool {
            self.inner.tracks_commits()
        }
        fn shared(&self) -> bool {
            self.inner.shared()
        }
        fn head(&self) -> Result<u64> {
            self.inner.head()
        }
        fn commits_since(&self, seq: u64, limit: usize) -> Result<Vec<CommitRecord>> {
            self.inner.commits_since(seq, limit)
        }
        fn get_entities(&self, ids: &[Id]) -> Result<Vec<Entity>> {
            self.inner.get_entities(ids)
        }
        fn get_links(&self, keys: &[onto_core::graph::LinkKey]) -> Result<Vec<Link>> {
            self.inner.get_links(keys)
        }
        fn subscribe(&self, f: NoticeFn) -> Result<()> {
            self.inner.subscribe(f)
        }
        fn origin(&self) -> String {
            self.inner.origin()
        }
        fn keeps_changes(&self) -> bool {
            self.inner.keeps_changes()
        }
        fn changes_since(&self, after: Cursor, limit: usize) -> Result<onto_core::changes::StoredPage> {
            self.inner.changes_since(after, limit)
        }
        fn changes_meta(&self) -> Result<onto_core::changes::ChangesMeta> {
            self.inner.changes_meta()
        }
        fn fences(&self) -> bool {
            self.inner.fences()
        }
        fn clock_ms(&self) -> Result<i64> {
            self.inner.clock_ms()
        }
        fn lease(&self, name: &str, holder: &str, ttl_ms: i64) -> Result<Option<u64>> {
            self.inner.lease(name, holder, ttl_ms)
        }
        fn release(&self, name: &str, holder: &str) -> Result<bool> {
            self.inner.release(name, holder)
        }
        fn lease_get(&self, name: &str) -> Result<Option<Lease>> {
            self.inner.lease_get(name)
        }
        fn consumer_get(&self, name: &str) -> Result<Option<Cursor>> {
            self.inner.consumer_get(name)
        }
        fn consumer_set(&self, name: &str, cursor: Cursor, fence: Option<&Fence>) -> Result<()> {
            self.inner.consumer_set(name, cursor, fence)
        }
        fn timers_set(&self, name: &str, delay_ms: i64, doc: &Value) -> Result<i64> {
            self.inner.timers_set(name, delay_ms, doc)
        }
        fn timers_due(&self, limit: usize) -> Result<Vec<Timer>> {
            self.inner.timers_due(limit)
        }
        fn timers_next(&self) -> Result<Option<i64>> {
            self.inner.timers_next()
        }
        fn timers_delete(&self, name: &str, due_at: Option<i64>) -> Result<bool> {
            self.inner.timers_delete(name, due_at)
        }
        fn wake_get(&self) -> Result<Option<Wake>> {
            self.inner.wake_get()
        }
        fn keeps_outbox(&self) -> bool {
            self.inner.keeps_outbox()
        }
        fn outbox_list(&self, q: &OutboxQuery) -> Result<Vec<onto_core::outbox::OutboxRow>> {
            self.inner.outbox_list(q)
        }
        fn keeps_runs(&self) -> bool {
            self.inner.keeps_runs()
        }
        fn run_claim(&self, rows: &[RunRow], fence: Option<&Fence>) -> Result<Vec<String>> {
            self.inner.run_claim(rows, fence)
        }
        fn run_update(&self, row: &RunRow, from: &[RunState], fence: Option<&Fence>) -> Result<bool> {
            self.inner.run_update(row, from, fence)
        }
        fn run_get(&self, id: &str) -> Result<Option<RunRow>> {
            self.inner.run_get(id)
        }
        fn runs_list(&self, q: &RunQuery) -> Result<Vec<RunRow>> {
            self.inner.runs_list(q)
        }
        fn runs_running(&self, epoch: u64, limit: usize) -> Result<Vec<RunRow>> {
            self.inner.runs_running(epoch, limit)
        }
        fn events(&self, filter: &EventFilter) -> Result<Vec<Event>> {
            self.inner.events(filter)
        }
        fn kv_put(&self, ns: &str, key: &str, value: &Value, expires_at: Option<i64>) -> Result<()> {
            self.inner.kv_put(ns, key, value, expires_at)
        }
        fn kv_get(&self, ns: &str, key: &str) -> Result<Option<Value>> {
            self.inner.kv_get(ns, key)
        }
        fn kv_delete(&self, ns: &str, key: &str) -> Result<()> {
            self.inner.kv_delete(ns, key)
        }
        fn kv_scan(&self, ns: &str, prefix: &str) -> Result<Vec<(String, Value)>> {
            self.inner.kv_scan(ns, prefix)
        }
        fn stats(&self) -> Result<StoreStats> {
            self.inner.stats()
        }
        fn versions(&self, id: &str, limit: usize) -> Result<Vec<VersionRecord>> {
            self.inner.versions(id, limit)
        }
        fn versions_since(&self, since: i64, limit: usize) -> Result<Vec<VersionRecord>> {
            self.inner.versions_since(since, limit)
        }
    }

    const NAME: &str = "resolve_blocked";

    /// A tenant with `resolve_blocked` defined and enabled, its endpoint
    /// registered, and issue:1 (high priority) in `triaged`.
    struct Fixture {
        o: Ontology,
        flaky: Arc<FlakyStore>,
        def: Automation,
        /// Keeps a PostgreSQL tenant until the test ends.
        _tenant: Option<common::TestTenant>,
    }

    fn fixture_on(store: Arc<dyn Store>, tenant: Option<common::TestTenant>, def: Value) -> Fixture {
        let flaky = Arc::new(FlakyStore::new(store));
        let o = Ontology::open(Schema::enterprise(), flaky.clone(), OntologyOptions::default()).unwrap();
        let ctx = ActionContext::new(admin(), "acme");
        o.commit_config(
            &ctx,
            vec![(
                NS_WEBHOOK_ENDPOINT.into(),
                "ops_pager".into(),
                json!({ "name": "ops_pager", "version": 1, "principal": { "roles": ["member"], "workspaces": ["acme"] } }),
            )],
            ConfigEvent::new("webhook.add", Some("ops_pager"), json!({})),
        )
        .unwrap();
        o.schema_apply(&ctx, set(json!({ "changes": [put(NAME, def)] }))).unwrap();
        o.upsert(&ctx, EntityInput::new("issue", "1", "Timeout").state("triaged").prop("priority", "high")).unwrap();
        let def = o.schema().automations[NAME].clone();
        enable(&o, NAME);
        Fixture { o, flaky, def, _tenant: tenant }
    }

    fn memory(def: Value) -> Fixture {
        fixture_on(Arc::new(MemoryStore::default()), None, def)
    }

    fn postgres(test: &str, def: Value) -> Option<Fixture> {
        let t = common::TestTenant::new(test)?;
        let store: Arc<dyn Store> = Arc::new(t.store());
        Some(fixture_on(store, Some(t), def))
    }

    /// Enable `name` as its closure stands now (what `automation.enable` writes).
    fn enable(o: &Ontology, name: &str) {
        let en = Enablements::load(&**o.store()).unwrap();
        let s = o.schema();
        let c = automate::closure(&s, name, &s.automations[name], &en.endpoints, &en.targets);
        let state = AutomationState {
            enabled: true,
            closure_hash: c.hash,
            closure: c.items,
            schema_version: o.schema_version(),
            enabled_by: "ada".into(),
            enabled_at: onto_core::time::now_ms(),
            expires_at: i64::MAX,
            paused: None,
            from: None,
        };
        o.commit_automation(
            &ActionContext::new(admin(), "acme"),
            vec![(NS_AUTOMATION_STATE.into(), name.into(), serde_json::to_value(&state).unwrap())],
            ConfigEvent::new("automation.enable", Some(name), json!({})),
        )
        .unwrap();
    }

    impl Fixture {
        /// Block issue:1 and return the run its change triggers (not claimed yet).
        fn block(&self) -> RunRow {
            let ctx = ActionContext::new(admin(), "acme");
            self.o
                .apply(
                    &ctx,
                    vec![Mutation::Patch(EntityPatch {
                        id: "issue:1".into(),
                        state: Some("blocked".into()),
                        ..Default::default()
                    })],
                )
                .unwrap();
            let page = self.o.store().changes_since(Cursor::default(), 1000).unwrap();
            let (stored, entity) = page
                .changes
                .iter()
                .rev()
                .find(|(c, _)| c.record.op == ChangeOp::EntityUpdate && c.record.id == "issue:1")
                .cloned()
                .unwrap();
            let r = stored.joined(entity);
            let schema = self.o.schema();
            assert!(self.def.admits(NAME, &schema, &r), "the trigger matches {r:?}");
            let st = self.o.automation_state(NAME).unwrap().unwrap();
            RunRow::new(NAME, &st.closure_hash, self.o.schema_version(), &self.def, RunTrigger::of(&r))
        }

        fn lease(&self, holder: &str, ttl_ms: i64) -> Fence {
            let epoch = self.o.store().lease(runs::CONSUMER_LEASE, holder, ttl_ms).unwrap().expect("the lease");
            Fence::new(runs::CONSUMER_LEASE, holder, epoch)
        }

        fn claim(&self, run: &mut RunRow, fence: &Fence) -> bool {
            run.epoch = fence.epoch;
            !self.o.store().run_claim(std::slice::from_ref(run), Some(fence)).unwrap().is_empty()
        }

        fn issue_state(&self) -> Option<String> {
            self.o.get(&admin(), "issue:1").unwrap().state
        }

        fn outbox_of(&self, run: &str) -> usize {
            let q = OutboxQuery { run: Some(run.into()), ..Default::default() };
            self.o.store().outbox_list(&q).unwrap().len()
        }

        fn events(&self, action: &str, request: &str) -> usize {
            let f = EventFilter { action: Some(action.into()), limit: Some(1000), ..Default::default() };
            self.o.store().events(&f).unwrap().iter().filter(|e| e.request_id.as_deref() == Some(request)).count()
        }
    }

    fn check_atomic_run(f: Fixture) {
        let fence = f.lease("worker-a", 60_000);
        let mut run = f.block();
        assert!(f.claim(&mut run, &fence));
        let before = f.o.store().head().unwrap();
        let done = f.o.commit_run(&run, &f.def, Some(&fence)).unwrap();
        assert_eq!(done.run.state, RunState::Succeeded, "{:?}", done.run.reason);
        // One commit: the action, its receipt, the outbox row and the run row.
        assert_eq!(f.o.store().head().unwrap(), before + 1, "everything in one commit");
        assert_eq!(f.issue_state().as_deref(), Some("resolved"));
        let receipt = done.receipt.expect("a receipt");
        assert_eq!(receipt.id, run.receipt_id());
        assert_eq!(receipt.actor, "automation:resolve_blocked");
        assert_eq!(f.o.action_receipt(&admin(), &receipt.id).unwrap().status, "applied");
        assert_eq!(f.outbox_of(&run.id), 1);
        let row = f.o.store().outbox_list(&OutboxQuery { run: Some(run.id.clone()), ..Default::default() }).unwrap();
        assert_eq!((row[0].target.as_str(), row[0].event_type.as_str()), ("ops_pager", "onto.issue.resolved"));
        assert_eq!(row[0].dedupe, format!("{}:1", run.id));
        assert_eq!(row[0].payload, json!({ "id": "issue:1" }));
        assert_eq!(row[0].cause.as_ref().unwrap().depth, 1);
        let stored = f.o.store().run_get(&run.id).unwrap().unwrap();
        assert_eq!(stored.state, RunState::Succeeded);
        assert!(stored.finished_at.is_some() && stored.started_at > 0);
        assert_eq!(stored.effects.len(), 2);
        assert!(stored.effects.iter().all(|e| e.ok));
        assert_eq!(f.events("automation.run", &run.id), 1);
        assert_eq!(f.events("action.resolve", &run.id), 1);
        // Its change records carry the run as their cause.
        let page = f.o.store().changes_since(Cursor::default(), 1000).unwrap();
        let caused: Vec<_> =
            page.changes.iter().filter(|(c, _)| c.record.cause.run.as_deref() == Some(run.id.as_str())).collect();
        assert_eq!(caused.len(), 1);
        assert_eq!(caused[0].0.record.cause.depth, 1);
        assert_eq!(caused[0].0.record.cause.automation.as_deref(), Some(NAME));
        assert_eq!(caused[0].0.record.cause.source.as_deref(), Some("automation:resolve_blocked"));
    }

    #[test]
    fn a_run_commits_action_receipt_outbox_row_and_run_row_in_one_commit() {
        check_atomic_run(memory(resolve_blocked()));
    }

    #[test]
    fn a_run_commits_in_one_transaction_postgres() {
        if let Some(f) = postgres("run_atomic", resolve_blocked()) {
            check_atomic_run(f);
        }
    }

    fn check_duplicate(f: Fixture) {
        let fence = f.lease("worker-a", 60_000);
        let mut run = f.block();
        assert!(f.claim(&mut run, &fence));
        // The same trigger again (a page read twice, two consumers): the
        // claim settles it before any work, and it is never retried.
        let mut again = RunRow::new(NAME, &run.closure_hash, run.schema_version, &f.def, run.trigger.clone());
        assert_ne!(again.id, run.id);
        assert!(!f.claim(&mut again, &fence), "a duplicate trigger is not claimed");
        assert!(f.o.store().run_get(&again.id).unwrap().is_none());
        f.o.commit_run(&run, &f.def, Some(&fence)).unwrap();
        // Executing the unclaimed duplicate anyway writes nothing.
        let err = f.o.commit_run(&again, &f.def, Some(&fence)).unwrap_err();
        assert!(runs::is_settled(&err), "{err}");
        assert!(!runs::retryable(&err), "a duplicate is final, never retried");
        assert_eq!(f.outbox_of(&run.id) + f.outbox_of(&again.id), 1);
        let q = RunQuery { automation: Some(NAME.into()), ..Default::default() };
        assert_eq!(f.o.store().runs_list(&q).unwrap().len(), 1);
        // Committing the settled run a second time is refused as well.
        assert!(runs::is_settled(&f.o.commit_run(&run, &f.def, Some(&fence)).unwrap_err()));
    }

    #[test]
    fn a_duplicate_trigger_is_skipped_by_the_claim_and_never_retried() {
        check_duplicate(memory(resolve_blocked()));
    }

    #[test]
    fn a_duplicate_trigger_is_skipped_by_the_claim_postgres() {
        if let Some(f) = postgres("run_duplicate", resolve_blocked()) {
            check_duplicate(f);
        }
    }

    fn check_crash(f: Fixture) {
        let retry = RetryPolicy::default();
        // Worker A claims, then dies before its commit lands.
        let a = f.lease("worker-a", 200);
        let mut run = f.block();
        assert!(f.claim(&mut run, &a));
        f.flaky.fail_next(1, false);
        assert!(f.o.commit_run(&run, &f.def, Some(&a)).is_err());
        let left = f.o.store().run_get(&run.id).unwrap().unwrap();
        assert_eq!(left.state, RunState::Running, "a crash leaves the claimed row running");
        assert_eq!(f.issue_state().as_deref(), Some("blocked"), "nothing of the commit was written");
        assert_eq!(f.outbox_of(&run.id), 0);
        // A's own recovery (same epoch) does not see it: only a new holder does.
        assert!(f.o.store().runs_running(a.epoch, 10).unwrap().is_empty());

        // Its lease runs out; worker B takes over and recovers the run.
        std::thread::sleep(std::time::Duration::from_millis(400));
        let b = f.lease("worker-b", 60_000);
        assert!(b.epoch > a.epoch);
        let recovered = f.o.recover_runs(&b, &retry, false).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].state, RunState::Succeeded, "{:?}", recovered[0].reason);
        assert_eq!(recovered[0].attempts, 1);
        assert_eq!(f.issue_state().as_deref(), Some("resolved"));
        // Exactly once: one receipt event, one outbox row, one run event.
        assert_eq!(f.outbox_of(&run.id), 1);
        assert_eq!(f.events("action.resolve", &run.id), 1);
        assert_eq!(f.events("automation.run", &run.id), 1);
        assert!(f.o.recover_runs(&b, &retry, false).unwrap().is_empty(), "nothing left to recover");
        // A, waking up late, can write nothing: its lease is gone, and the run is settled.
        let err = f.o.commit_run(&run, &f.def, Some(&a)).unwrap_err();
        assert!(runs::is_lease_lost(&err), "{err}");
        assert!(runs::is_settled(&f.o.commit_run(&run, &f.def, None).unwrap_err()));
        assert_eq!(f.outbox_of(&run.id), 1);
    }

    #[test]
    fn a_crash_between_claim_and_commit_is_recovered_exactly_once() {
        check_crash(memory(resolve_blocked()));
    }

    #[test]
    fn a_crash_between_claim_and_commit_is_recovered_exactly_once_postgres() {
        if let Some(f) = postgres("run_crash", resolve_blocked()) {
            check_crash(f);
        }
    }

    #[test]
    fn unavailability_is_retried_with_a_timer_and_other_errors_are_final() {
        let f = memory(resolve_blocked());
        let retry = RetryPolicy { base_ms: 1, max_ms: 10, max_attempts: 2 };
        let fence = f.lease("worker-a", 60_000);
        let mut run = f.block();
        assert!(f.claim(&mut run, &fence));
        f.flaky.fail_next(1, true);
        let row = f.o.execute_run(&run, &f.def, &fence, &retry).unwrap();
        assert_eq!(row.state, RunState::Retry, "{:?}", row.reason);
        assert!(row.reason.as_deref().unwrap().contains("unavailable"), "{:?}", row.reason);
        let timers = {
            std::thread::sleep(std::time::Duration::from_millis(5));
            f.o.store().timers_due(10).unwrap()
        };
        assert_eq!(timers.len(), 1);
        assert_eq!(timers[0].name, format!("retry:{}", run.id));
        // The timer resumes it; unavailable again at the last attempt: final.
        let mut resumed = f.o.store().run_get(&run.id).unwrap().unwrap().with_state(RunState::Running, None);
        assert!(f.o.store().run_update(&resumed, &[RunState::Retry], Some(&fence)).unwrap());
        resumed = f.o.store().run_get(&run.id).unwrap().unwrap();
        f.flaky.fail_next(1, true);
        let row = f.o.execute_run(&resumed, &f.def, &fence, &retry).unwrap();
        assert_eq!(row.state, RunState::Failed);
        assert!(row.reason.as_deref().unwrap().starts_with("gave up after 2 attempts"), "{:?}", row.reason);
        assert_eq!(f.issue_state().as_deref(), Some("blocked"));
        // A storage error is final at once.
        let mut other = f.block();
        other.trigger.cursor = Cursor::new(other.trigger.cursor.seq, other.trigger.cursor.n + 7);
        other.trigger_key = other.trigger.cursor.to_string();
        assert!(f.claim(&mut other, &fence));
        f.flaky.fail_next(1, false);
        let row = f.o.execute_run(&other, &f.def, &fence, &retry).unwrap();
        assert_eq!(row.state, RunState::Failed);
        assert!(row.reason.as_deref().unwrap().contains("simulated crash"), "{:?}", row.reason);
    }

    #[test]
    fn a_precondition_that_changed_before_the_commit_fails_the_run() {
        let f = memory(resolve_blocked());
        let fence = f.lease("worker-a", 60_000);
        let mut run = f.block();
        assert!(matches!(f.o.evaluate_run(&run, &f.def, None).unwrap(), runs::Verdict::Ready(_)));
        assert!(f.claim(&mut run, &fence));
        // Between the trigger and the commit, someone closes the issue.
        f.o.apply(
            &ActionContext::new(admin(), "acme"),
            vec![Mutation::Patch(EntityPatch {
                id: "issue:1".into(),
                state: Some("closed".into()),
                ..Default::default()
            })],
        )
        .unwrap();
        let done = f.o.commit_run(&run, &f.def, Some(&fence)).unwrap();
        assert_eq!(done.run.state, RunState::Failed);
        let reason = done.run.reason.unwrap();
        assert!(
            reason.starts_with("precondition_changed") && reason.contains("already resolved or closed"),
            "{reason}"
        );
        assert!(done.receipt.is_none());
        assert_eq!(f.outbox_of(&run.id), 0, "no webhook for a run that did not happen");
        assert_eq!(f.issue_state().as_deref(), Some("closed"));
        assert_eq!(f.events("automation.run", &run.id), 1, "the failure is audited");
        // A condition that stopped holding is the same.
        let f = memory(resolve_blocked());
        let mut run = f.block();
        let fence = f.lease("worker-a", 60_000);
        assert!(f.claim(&mut run, &fence));
        f.o.upsert(
            &ActionContext::new(admin(), "acme"),
            EntityInput::new("issue", "1", "Timeout").prop("priority", "low"),
        )
        .unwrap();
        let done = f.o.commit_run(&run, &f.def, Some(&fence)).unwrap();
        assert_eq!(done.run.state, RunState::Failed);
        assert!(done.run.reason.unwrap().contains("priority must equal"));
    }

    #[test]
    fn shadow_mode_records_would_run_and_writes_nothing_else() {
        let mut def = resolve_blocked();
        def["mode"] = json!("shadow");
        let f = memory(def);
        let fence = f.lease("worker-a", 60_000);
        let mut run = f.block();
        assert!(f.claim(&mut run, &fence));
        let done = f.o.commit_run(&run, &f.def, Some(&fence)).unwrap();
        assert_eq!(done.run.state, RunState::WouldRun);
        assert!(done.receipt.is_none());
        assert_eq!(f.issue_state().as_deref(), Some("blocked"));
        assert_eq!(f.outbox_of(&run.id), 0);
        let effects = &done.run.effects;
        assert_eq!(effects[0].plan.as_ref().unwrap()["preview"][0], "update issue:1: state blocked → resolved");
        assert_eq!(effects[1].plan.as_ref().unwrap()["payload"], json!({ "id": "issue:1" }));
        assert!(effects.iter().all(|e| e.receipt.is_none() && e.outbox.is_none()));
        assert_eq!(f.o.store().run_get(&run.id).unwrap().unwrap().state, RunState::WouldRun);
    }

    #[test]
    fn plans_that_need_confirmation_are_refused() {
        // `delete` is high risk: automations run only low-risk, reversible action types.
        let mut def = resolve_blocked();
        def["effects"] = json!([{ "action": { "type": "delete", "params": { "target": "{trigger.id}" } } }]);
        let f = memory(def);
        let fence = f.lease("worker-a", 60_000);
        let mut run = f.block();
        match f.o.evaluate_run(&run, &f.def, None).unwrap() {
            runs::Verdict::NeedsConfirmation(why) => assert!(why.contains("risk high"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(f.claim(&mut run, &fence));
        let done = f.o.commit_run(&run, &f.def, Some(&fence)).unwrap();
        assert_eq!(done.run.state, RunState::Refused);
        assert!(done.run.reason.unwrap().starts_with("needs_confirmation"));
        assert!(f.o.get(&admin(), "issue:1").is_ok(), "nothing was deleted");
    }

    #[test]
    fn templates_render_strictly_and_only_what_the_run_may_see() {
        let mut def = resolve_blocked();
        def["effects"][1]["webhook"]["payload"] = json!({ "id": "{trigger.id}", "missing": ["{subject.nope}"] });
        let f = memory(def);
        let run = f.block();
        let err = f.o.evaluate_run(&run, &f.def, None).unwrap_err().to_string();
        assert!(err.contains("invalid_template") && err.contains("{subject.nope}"), "{err}");
        // A dry run with a caller who cannot see the subject renders nothing of it.
        let mut def = resolve_blocked();
        def["effects"] =
            json!([{ "webhook": { "endpoint": "ops_pager", "event": "x", "payload": { "t": "{subject.title}" } } }]);
        def["conditions"] = json!([]);
        let f = memory(def);
        let run = f.block();
        let outsider = Principal::named("user:o", &["viewer"], &["elsewhere"]).unwrap();
        match f.o.evaluate_run(&run, &f.def, None).unwrap() {
            runs::Verdict::Ready(p) => assert_eq!(p.outbox[0].payload, json!({ "t": "Timeout" })),
            other => panic!("{other:?}"),
        }
        let err = f.o.evaluate_run(&run, &f.def, Some(&outsider)).unwrap_err().to_string();
        assert!(err.contains("invalid_template"), "{err}");
    }

    #[test]
    fn status_says_why_an_automation_does_not_run() {
        let f = memory(resolve_blocked());
        let now = onto_core::time::now_ms();
        let en = || Enablements::load(&**f.o.store()).unwrap();
        let s = f.o.schema();
        assert_eq!(runs::status(&s, &en(), NAME, now, false), Status::Active);
        assert_eq!(runs::status(&s, &en(), NAME, now, true), Status::Inactive("killed".into()));
        assert_eq!(runs::status(&s, &en(), "nope", now, false), Status::Inactive("not_defined".into()));
        assert_eq!(runs::status(&s, &en(), NAME, i64::MAX, false), Status::Inactive("enablement_expired".into()));
        // The endpoint it reaches changed: stale, naming the item.
        f.o.commit_config(
            &ActionContext::new(admin(), "acme"),
            vec![(
                NS_WEBHOOK_ENDPOINT.into(),
                "ops_pager".into(),
                json!({ "name": "ops_pager", "version": 2, "principal": { "roles": ["member"], "workspaces": ["acme"] } }),
            )],
            ConfigEvent::new("webhook.update", Some("ops_pager"), json!({})),
        )
        .unwrap();
        assert_eq!(
            runs::status(&s, &en(), NAME, now, false),
            Status::Inactive("stale_definition: endpoint:ops_pager".into())
        );
        // Recovery cancels a running row of an automation that is no longer active.
        let fence = f.lease("worker-a", 60_000);
        let mut run = f.block();
        run.epoch = 0;
        f.o.store().run_claim(std::slice::from_ref(&run), None).unwrap();
        let done = f.o.recover_runs(&fence, &RetryPolicy::default(), false).unwrap();
        assert_eq!(done[0].state, RunState::Cancelled);
        assert!(done[0].reason.as_deref().unwrap().contains("stale_definition"), "{:?}", done[0].reason);
        assert_eq!(f.issue_state().as_deref(), Some("blocked"));
    }

    #[test]
    fn enabling_is_for_approvers_never_agents() {
        let f = memory(resolve_blocked());
        let kv = vec![(NS_AUTOMATION_STATE.to_string(), NAME.to_string(), json!({ "enabled": false }))];
        for p in [
            Principal::named("user:m", &["member"], &["acme"]).unwrap(),
            Principal::new("bot", vec![onto_core::Role::Agent], vec!["*".into()]),
        ] {
            let err =
                f.o.commit_automation(
                    &ActionContext::new(p, "acme"),
                    kv.clone(),
                    ConfigEvent::new("automation.disable", None, json!({})),
                )
                .unwrap_err();
            assert_eq!(err.code(), "permission_denied", "{err}");
        }
        let err =
            f.o.commit_automation(
                &ActionContext::new(admin(), "acme"),
                vec![(NS_WEBHOOK_ENDPOINT.to_string(), "x".into(), json!({}))],
                ConfigEvent::new("x", None, json!({})),
            )
            .unwrap_err();
        assert!(err.to_string().contains("writes only"), "{err}");
    }

    // ── writeback (build step 12) ───────────────────────────────────────────

    /// An automation whose action resolves the issue and closes its helpdesk
    /// ticket (a writeback), with `[writeback] enabled` on and the target
    /// registered; `unattended` lists the caller kinds the action type waives
    /// the external write's confirmation for.
    fn writeback_fixture(store: Arc<dyn Store>, tenant: Option<common::TestTenant>, unattended: &[&str]) -> Fixture {
        let flaky = Arc::new(FlakyStore::new(store));
        let opts = OntologyOptions { writeback: true, ..Default::default() };
        let o = Ontology::open(Schema::enterprise(), flaky.clone(), opts).unwrap();
        let ctx = ActionContext::new(admin(), "acme");
        let target = json!({
            "name": "helpdesk", "base_url": "https://helpdesk.example.com/", "version": 1,
            "workspaces": ["acme"], "routes": { "close": "tickets/close" },
        });
        o.commit_config(
            &ctx,
            vec![(automate::NS_WRITEBACK_TARGET.into(), "helpdesk".into(), target)],
            ConfigEvent::new("writeback.add", Some("writeback:helpdesk"), json!({})),
        )
        .unwrap();
        let at = json!({
            "parameters": { "issue": { "type": "entity", "kinds": ["issue"], "required": true } },
            "preconditions": [{ "check": "open", "param": "issue" }],
            "effects": [
                { "patch": { "id": "{issue}", "state": "resolved" } },
                { "writeback": { "target": "helpdesk", "route": "close", "body": { "issue": "{issue}" } } }
            ],
            "unattended": unattended,
        });
        let mut def = resolve_blocked();
        def["effects"] = json!([{ "action": { "type": "close_ticket", "params": { "issue": "{trigger.id}" } } }]);
        let changes = json!({ "changes": [
            { "op": "put_action_type", "name": "close_ticket", "def": at },
            put(NAME, def),
        ] });
        o.schema_apply(&ctx, set(changes)).unwrap();
        o.upsert(&ctx, EntityInput::new("issue", "1", "Timeout").state("triaged").prop("priority", "high")).unwrap();
        let def = o.schema().automations[NAME].clone();
        enable(&o, NAME);
        // The target is part of the closure the enablement binds to.
        let st = o.automation_state(NAME).unwrap().unwrap();
        assert!(st.closure.contains_key("target:helpdesk"), "{:?}", st.closure);
        assert_ne!(st.closure["target:helpdesk"], automate::MISSING);
        Fixture { o, flaky, def, _tenant: tenant }
    }

    fn check_writeback_run(f: Fixture) {
        let fence = f.lease("worker-a", 60_000);
        let mut run = f.block();
        assert!(f.claim(&mut run, &fence));
        let before = f.o.store().head().unwrap();
        let done = f.o.commit_run(&run, &f.def, Some(&fence)).unwrap();
        assert_eq!(done.run.state, RunState::Succeeded, "{:?}", done.run.reason);
        assert_eq!(f.o.store().head().unwrap(), before + 1, "edits, receipt, writeback row and run row in one commit");
        assert_eq!(f.issue_state().as_deref(), Some("resolved"));
        let receipt = done.receipt.expect("a receipt");
        assert_eq!(receipt.external.len(), 1);
        let key = format!("{}:1", receipt.id);
        assert_eq!(receipt.external[0].idempotency_key, key);
        let rows = f.o.store().outbox_list(&OutboxQuery { run: Some(run.id.clone()), ..Default::default() }).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.kind, onto_core::outbox::OutboxKind::Writeback);
        assert_eq!(
            (row.target.as_str(), row.dedupe.as_str(), row.source.as_str()),
            ("helpdesk", key.as_str(), "automation")
        );
        assert_eq!(row.receipt.as_deref(), Some(receipt.id.as_str()));
        assert_eq!(row.payload, json!({ "issue": "issue:1" }));
        assert_eq!(row.cause.as_ref().map(|c| c.depth), Some(1));
        assert_eq!(f.o.action_receipt(&admin(), &receipt.id).unwrap().external[0].outbox, row.id);
    }

    #[test]
    fn an_unattended_writeback_runs_with_the_run_commit() {
        check_writeback_run(writeback_fixture(Arc::new(MemoryStore::default()), None, &["automation"]));
    }

    #[test]
    fn postgres_commits_an_unattended_writeback_with_the_run() {
        let Some(t) = common::TestTenant::new("automate_writeback") else { return };
        let store: Arc<dyn Store> = Arc::new(t.store());
        check_writeback_run(writeback_fixture(store, Some(t), &["automation"]));
    }

    #[test]
    fn a_writeback_not_declared_unattended_for_automations_is_refused() {
        for unattended in [&[][..], &["agent"][..]] {
            let f = writeback_fixture(Arc::new(MemoryStore::default()), None, unattended);
            let fence = f.lease("worker-a", 60_000);
            let mut run = f.block();
            assert!(f.claim(&mut run, &fence));
            let done = f.o.commit_run(&run, &f.def, Some(&fence)).unwrap();
            assert_eq!(done.run.state, RunState::Refused);
            let reason = done.run.reason.unwrap();
            assert!(reason.starts_with("needs_confirmation") && reason.contains("not reversible"), "{reason}");
            assert!(done.receipt.is_none());
            assert_eq!(f.outbox_of(&run.id), 0, "nothing is sent for a refused run");
            assert_eq!(f.issue_state().as_deref(), Some("blocked"));
        }
    }
}
