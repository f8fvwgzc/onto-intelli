//! Schema change proposals: propose → review → approve/reject → apply atomically.

use std::sync::Arc;

use serde_json::{Value, json};

use onto_core::store::MemoryStore;
use onto_core::{
    ActionContext, Decision, EntityInput, Error, EventFilter, Governance, Ontology, OntologyOptions, Principal,
    ProposalInput, ProposalStatus, Role, Schema,
};

fn input(v: Value) -> ProposalInput {
    serde_json::from_value(v).expect("proposal input")
}

fn agent() -> Principal {
    Principal::new("assistant", vec![Role::Agent], vec!["*".into()])
}

fn alice() -> Principal {
    Principal::admin("alice")
}

fn bob() -> Principal {
    Principal::admin("bob")
}

fn onto_with(gov: Governance) -> Ontology {
    let o = Ontology::open(
        Schema::enterprise(),
        Arc::new(MemoryStore::default()),
        OntologyOptions { governance: gov, ..Default::default() },
    )
    .unwrap();
    let ctx = ActionContext::new(alice(), "acme");
    o.upsert(&ctx, EntityInput::new("issue", "1", "Timeout").state("triaged")).unwrap();
    o.upsert(&ctx, EntityInput::new("issue", "2", "Refund").state("open")).unwrap();
    o
}

fn add_runbook() -> ProposalInput {
    input(json!({
        "title": "Add runbooks",
        "rationale": "Incidents keep asking for the runbook; there is no kind for it.",
        "changes": [
            { "op": "put_object_type", "name": "runbook", "def": { "category": "knowledge", "label": "Runbook" } },
            { "op": "put_link_type", "name": "documents", "def": { "from": ["runbook"], "to": ["service"] } }
        ]
    }))
}

#[test]
fn agent_proposes_human_approves_and_it_applies_atomically() {
    let o = onto_with(Governance::default());
    let ctx_agent = ActionContext::new(agent(), "acme").with_source("nexc-assistant");
    let filed = o.propose_schema(&ctx_agent, add_runbook()).unwrap();
    let id = filed.proposal.id.clone();
    assert_eq!(filed.proposal.status, ProposalStatus::Open);
    assert!(filed.proposal.proposer.agent);
    assert_eq!(filed.proposal.source.as_deref(), Some("nexc-assistant"));
    assert!(filed.proposal.plan.valid && !filed.proposal.plan.breaking);
    assert!(!o.schema().object_types.contains_key("runbook"), "proposals do not change the schema");

    // Agents cannot apply directly, nor review.
    assert!(matches!(o.schema_apply(&ctx_agent, add_runbook().change_set).unwrap_err(), Error::Denied(_)));
    let err = o.review_proposal(&ctx_agent, &id, Decision::Approve, "lgtm", None).unwrap_err();
    assert!(matches!(err, Error::Denied(ref m) if m.contains("agent")), "{err}");

    let listed = o.list_proposals(&alice(), Some(ProposalStatus::Open), 10).unwrap();
    assert_eq!(listed.len(), 1);
    let out =
        o.review_proposal(&ActionContext::new(alice(), "acme"), &id, Decision::Approve, "useful", Some(1)).unwrap();
    let applied = out.applied.expect("applied");
    assert_eq!(applied.version.version, 2);
    assert_eq!(applied.version.source, "proposal");
    assert_eq!(applied.version.proposal.as_deref(), Some(id.as_str()));
    assert_eq!(out.proposal.proposal.status, ProposalStatus::Applied);
    assert_eq!(out.proposal.proposal.applied_version, Some(2));
    assert!(o.schema().object_types.contains_key("runbook"));

    // Audited end to end, with actors.
    let events = o.events(&alice(), &EventFilter { action: Some("schema".into()), ..Default::default() }).unwrap();
    let mut seen: Vec<(String, String)> = events.iter().map(|e| (e.action.clone(), e.actor.clone())).collect();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("schema.change".to_string(), "alice".to_string()),
            ("schema.proposal.approve".to_string(), "alice".to_string()),
            ("schema.proposal.create".to_string(), "assistant".to_string()),
        ]
    );
    let change = events.iter().find(|e| e.action == "schema.change").unwrap();
    assert_eq!(change.payload["proposal"], json!(id));
    // Applied proposals cannot be reviewed again.
    assert!(matches!(
        o.review_proposal(&ActionContext::new(bob(), "acme"), &id, Decision::Reject, "", None).unwrap_err(),
        Error::Conflict(_)
    ));
}

#[test]
fn self_approval_rejection_comments_and_withdrawal() {
    let o = onto_with(Governance::default());
    let ctx = ActionContext::new(alice(), "acme");
    let id = o.propose_schema(&ctx, add_runbook()).unwrap().proposal.id;
    let err = o.review_proposal(&ctx, &id, Decision::Approve, "", None).unwrap_err();
    assert!(matches!(err, Error::Denied(ref m) if m.contains("own proposal")), "{err}");
    let member = ActionContext::new(Principal::member("mia", "acme"), "acme");
    o.review_proposal(&member, &id, Decision::Comment, "what about playbooks?", None).unwrap();
    assert!(matches!(o.review_proposal(&member, &id, Decision::Reject, "", None).unwrap_err(), Error::Denied(_)));
    let out = o.review_proposal(&ActionContext::new(bob(), "acme"), &id, Decision::Reject, "not now", None).unwrap();
    assert_eq!(out.proposal.proposal.status, ProposalStatus::Rejected);
    assert_eq!(out.proposal.proposal.reviews.len(), 2);

    let id2 = o.propose_schema(&ctx, add_runbook()).unwrap().proposal.id;
    let w = o.withdraw_proposal(&ctx, &id2).unwrap();
    assert_eq!(w.proposal.status, ProposalStatus::Withdrawn);
    assert_eq!(o.list_proposals(&alice(), None, 10).unwrap().len(), 2);
    assert_eq!(o.schema_version(), 1);
}

#[test]
fn stale_proposals_show_a_fresh_preview_and_check_what_was_seen() {
    let o = onto_with(Governance::default());
    let id = o.propose_schema(&ActionContext::new(agent(), "acme"), add_runbook()).unwrap().proposal.id;
    o.schema_apply(
        &ActionContext::new(alice(), "acme"),
        serde_json::from_value(json!({ "changes": [ { "op": "put_object_type", "name": "playbook", "def": {} } ] }))
            .unwrap(),
    )
    .unwrap();
    let view = o.get_proposal(&bob(), &id).unwrap();
    assert!(view.stale);
    assert_eq!(view.active_version, 2);
    assert!(view.preview.as_ref().unwrap().valid);
    let err = o.review_proposal(&ActionContext::new(bob(), "acme"), &id, Decision::Approve, "", Some(1)).unwrap_err();
    assert!(matches!(err, Error::Conflict(ref m) if m.contains("moved from version 1 to 2")), "{err}");
    let out = o.review_proposal(&ActionContext::new(bob(), "acme"), &id, Decision::Approve, "", Some(2)).unwrap();
    assert_eq!(out.applied.unwrap().version.version, 3);
    assert!(o.schema().object_types.contains_key("playbook") && o.schema().object_types.contains_key("runbook"));
}

#[test]
fn breaking_proposals_fail_until_revised_with_a_migration() {
    let o = onto_with(Governance::default());
    let mut issue = o.schema().object_types["issue"].clone();
    issue.states.retain(|s| s != "triaged");
    let ctx_agent = ActionContext::new(agent(), "acme");
    let filed = o
        .propose_schema(
            &ctx_agent,
            input(json!({ "title": "Drop triaged", "rationale": "Nobody uses it",
                "changes": [ { "op": "put_object_type", "name": "issue", "def": issue } ] })),
        )
        .unwrap();
    assert!(filed.proposal.plan.breaking);
    assert_eq!(filed.proposal.plan.needs_migration.len(), 1);
    let id = filed.proposal.id;
    let err = o.review_proposal(&ActionContext::new(alice(), "acme"), &id, Decision::Approve, "", None).unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err}");
    let failed = o.get_proposal(&alice(), &id).unwrap();
    assert_eq!(failed.proposal.status, ProposalStatus::Failed);
    assert!(failed.proposal.error.as_deref().unwrap().contains("needs a migration"));

    let revised = o
        .revise_proposal(
            &ctx_agent,
            &id,
            input(json!({ "title": "", "changes": [ { "op": "put_object_type", "name": "issue", "def": issue,
                "migrations": [ { "action": "map_states", "map": { "triaged": "open" } } ] } ] })),
        )
        .unwrap();
    assert_eq!(revised.proposal.status, ProposalStatus::Open);
    assert_eq!(revised.proposal.revision, 2);
    assert_eq!(revised.approvals, 0);
    assert_eq!(revised.proposal.title, "Drop triaged");
    let out = o.review_proposal(&ActionContext::new(alice(), "acme"), &id, Decision::Approve, "", None).unwrap();
    assert!(out.applied.is_some());
    assert_eq!(o.get(&alice(), "issue:1").unwrap().state.as_deref(), Some("open"));
}

#[test]
fn several_approvals_and_custom_approver_roles() {
    let steward = Role::parse("schema_steward").unwrap();
    let o = onto_with(Governance { approve_roles: vec![steward.clone()], required_approvals: 2, ..Default::default() });
    let id = o.propose_schema(&ActionContext::new(agent(), "acme"), add_runbook()).unwrap().proposal.id;
    let s1 = ActionContext::new(Principal::new("s1", vec![steward.clone()], vec!["*".into()]), "acme");
    let s2 = ActionContext::new(Principal::new("s2", vec![steward], vec!["*".into()]), "acme");
    // Admin without the steward role cannot approve here.
    assert!(matches!(
        o.review_proposal(&ActionContext::new(alice(), "acme"), &id, Decision::Approve, "", None).unwrap_err(),
        Error::Denied(_)
    ));
    let first = o.review_proposal(&s1, &id, Decision::Approve, "", None).unwrap();
    assert!(first.applied.is_none());
    assert_eq!(first.proposal.approvals, 1);
    assert!(matches!(o.review_proposal(&s1, &id, Decision::Approve, "", None).unwrap_err(), Error::Conflict(_)));
    let second = o.review_proposal(&s2, &id, Decision::Approve, "", None).unwrap();
    assert!(second.applied.is_some());
}

#[test]
fn invalid_proposals_are_refused_with_their_errors() {
    let o = onto_with(Governance::default());
    let err = o
        .propose_schema(
            &ActionContext::new(agent(), "acme"),
            input(json!({ "title": "Haunt", "changes": [ { "op": "put_link_type", "name": "haunts",
                "def": { "from": ["ghost"] } } ] })),
        )
        .unwrap_err();
    assert!(matches!(err, Error::Invalid(ref m) if m.contains("unknown type ghost")), "{err}");
    let viewerless = Principal::new("nobody", vec![], vec!["*".into()]);
    assert!(matches!(
        o.propose_schema(&ActionContext::new(viewerless, "acme"), add_runbook()).unwrap_err(),
        Error::Denied(_)
    ));
}
