//! Demo workspace ("acme"): teams, people, projects, issues, tasks, services,
//! a failed deployment, knowledge, a decision, a generated artifact with
//! provenance, audit events and a long-term memory. `onto seed demo`.

use serde::Serialize;
use serde_json::json;

use onto_core::{
    ActionContext, EntityInput, EventInput, Mutation, MutationResult, Outcome, Principal, Provenance, Result,
};

use crate::Intelli;
use crate::memory::{MemoryKind, RememberInput};

fn e(kind: &str, key: &str, title: &str) -> EntityInput {
    EntityInput::new(kind, key, title)
}

pub fn demo_mutations() -> Vec<Mutation> {
    let entities = vec![
        e("team", "payments", "Payments Team").prop("mission", "Checkout, payment processing and refunds"),
        e("team", "platform", "Platform Team").prop("mission", "Databases, infrastructure and deployment tooling"),
        e("person", "sarah", "Sarah Lee").prop("role", "Engineering Manager").prop("email", "sarah@acme.test")
            .link("member_of", "team:payments").link("manages", "team:payments"),
        e("person", "kenji", "Kenji Tanaka").prop("role", "Backend Engineer").prop("email", "kenji@acme.test")
            .link("member_of", "team:payments"),
        e("person", "aiko", "Aiko Sato").prop("role", "Frontend Engineer").link("member_of", "team:payments"),
        e("person", "maria", "Maria Garcia").prop("role", "SRE Lead")
            .link("member_of", "team:platform").link("manages", "team:platform"),
        e("person", "john", "John Smith").prop("role", "Developer").link("member_of", "team:platform"),
        e("agent", "ops-bot", "Ops Bot").prop("model", "claude-opus-5-5").prop("purpose", "Infrastructure automation and IaC generation")
            .link("member_of", "team:platform"),
        e("repository", "payments-repo", "github/acme-payments").prop("url", "https://github.com/acme/payments")
            .link("owned_by", "team:payments"),
        e("environment", "production", "Production").link("owned_by", "team:platform"),
        e("service", "postgres", "PostgreSQL").state("degraded").prop("tier", "critical")
            .alias("primary database").link("owned_by", "team:platform").link("deployed_to", "environment:production"),
        e("service", "auth-service", "Auth Service").state("healthy").link("owned_by", "team:platform"),
        e("service", "payment-api", "Payment API").state("degraded").prop("tier", "critical")
            .link("owned_by", "team:payments").link("implemented_by", "repository:payments-repo")
            .link("depends_on", "service:postgres").link("depends_on", "service:auth-service")
            .link("deployed_to", "environment:production"),
        e("deployment", "182", "Deployment #182 (PostgreSQL 16.4 upgrade)").state("failed").prop("version", "16.4")
            .link("deploys", "service:postgres").link("deployed_to", "environment:production").link("requested_by", "person:maria"),
        e("deployment", "190", "Deployment #190 (Payment API 2.3.0)").state("pending").prop("version", "2.3.0")
            .link("deploys", "service:payment-api").link("deployed_to", "environment:production").link("requested_by", "person:john"),
        e("project", "alpha", "Project Alpha").state("blocked").prop("goal", "Launch one-click checkout")
            .link("owned_by", "team:payments").link("depends_on", "service:payment-api"),
        e("project", "beacon", "Project Beacon").state("active").prop("goal", "High availability for the primary database")
            .link("owned_by", "team:platform"),
        e("milestone", "alpha-beta", "Alpha beta launch").state("at_risk").link("belongs_to", "project:alpha"),
        e("issue", "421", "Payment timeout under load").state("blocked").prop("priority", "critical")
            .prop("description", "Checkout requests time out after 30s when PostgreSQL replication lag spikes.")
            .link("belongs_to", "project:alpha").link("assigned_to", "person:kenji")
            .link("affects", "service:payment-api").link("reported_by", "person:sarah"),
        e("issue", "433", "Refund webhook retries create duplicates").state("open").prop("priority", "medium")
            .link("belongs_to", "project:alpha").link("assigned_to", "person:kenji").link("affects", "service:payment-api"),
        e("issue", "450", "PostgreSQL replication lag").state("in_progress").prop("priority", "high")
            .link("belongs_to", "project:beacon").link("assigned_to", "person:maria").link("affects", "service:postgres"),
        e("task", "31", "Implement retries with idempotency keys").state("blocked").prop("estimate", 3.0)
            .link("belongs_to", "project:alpha").link("assigned_to", "person:kenji").link("blocked_by", "issue:421"),
        e("task", "32", "Load-test the checkout flow").state("todo").prop("estimate", 2.0)
            .link("belongs_to", "project:alpha").link("assigned_to", "person:kenji").link("depends_on", "task:31"),
        e("task", "33", "One-click checkout UI").state("in_progress").prop("estimate", 4.0)
            .link("belongs_to", "project:alpha").link("assigned_to", "person:aiko"),
        e("task", "34", "Write the launch runbook").state("todo").prop("estimate", 1.0)
            .link("belongs_to", "project:alpha").link("assigned_to", "person:aiko").link("depends_on", "task:32").link("depends_on", "task:33"),
        e("task", "35", "Roll back PostgreSQL 16.4").state("done").prop("estimate", 1.0)
            .link("belongs_to", "project:beacon").link("assigned_to", "person:maria"),
        e("person", "kenji.t", "Kenji T.")
            .prop("email", "kenji@acme.test")
            .prop("role", "Backend Engineer")
            .link("member_of", "team:payments"),
        e("issue", "182", "Status page shows stale uptime")
            .state("open")
            .prop("priority", "low")
            .link("belongs_to", "project:beacon")
            .link("assigned_to", "person:john"),
        e("issue", "380", "Login latency spike")
            .state("open")
            .prop("priority", "high")
            .link("belongs_to", "project:beacon")
            .link("assigned_to", "person:maria")
            .link("affects", "service:auth-service"),
        e("knowledge", "runbook-payments", "Payments incident runbook")
            .prop("body", "When Payment API latency exceeds 2s, check PostgreSQL replication lag and the connection pool first. Fail over to the standby if lag exceeds 30s.")
            .prop("tags", json!(["runbook", "payments", "incident"]))
            .link("owned_by", "team:payments").link("references", "service:payment-api"),
        e("decision", "adr-12", "ADR-12: Idempotency keys for payment retries").state("accepted")
            .prop("rationale", "Retries without idempotency keys caused duplicate charges in March; every payment mutation now carries a client key.")
            .link("belongs_to", "project:alpha").link("references", "issue:433"),
        e("knowledge", "salary-bands", "Engineering salary bands").prop("visibility", "restricted").prop("acl", json!(["person:sarah"]))
            .prop("body", "Confidential compensation bands."),
        e("artifact", "postgres-ha-tf", "postgres-ha.tf").prop("media_type", "text/x-terraform").prop("size", 4210)
            .prop("summary", "Terraform module adding a hot standby and automatic failover for PostgreSQL")
            .link("owned_by", "team:platform")
            .provenance(Provenance {
                source: "ai_generation".into(),
                derived_from: vec!["issue:450".into(), "service:postgres".into()],
                generated_by: Some("agent:ops-bot".into()),
                run: Some("run_2026_10_16_ha".into()),
                ..Default::default()
            }),
    ];
    entities.into_iter().map(Mutation::Upsert).collect()
}

pub fn demo_events() -> Vec<EventInput> {
    let ev = |actor: &str, action: &str, target: &str, outcome: Outcome, payload: serde_json::Value| EventInput {
        action: action.into(),
        actor: Some(actor.into()),
        target: Some(target.into()),
        outcome,
        source: Some("demo".into()),
        payload,
        ..Default::default()
    };
    vec![
        ev(
            "person:maria",
            "deployment.run",
            "deployment:182",
            Outcome::Failure,
            json!({ "error": "migration timed out after 900s; replica fell 45s behind" }),
        ),
        ev(
            "person:john",
            "deployment.request",
            "deployment:190",
            Outcome::Denied,
            json!({ "reason": "john lacks the production-deploy permission for payment-api" }),
        ),
        ev("person:sarah", "file.upload", "knowledge:runbook-payments", Outcome::Success, json!({ "size": 1830 })),
        ev(
            "agent:ops-bot",
            "file.generate",
            "artifact:postgres-ha-tf",
            Outcome::Success,
            json!({ "derived_from": ["issue:450", "service:postgres"] }),
        ),
    ]
}

#[derive(Debug, Serialize)]
pub struct SeedReport {
    pub workspace: String,
    pub graph: MutationResult,
    pub events: usize,
    pub memories: usize,
}

pub async fn load_demo(intelli: &Intelli, workspace: &str) -> Result<SeedReport> {
    let ctx = ActionContext::system(workspace).with_source("seed");
    let graph = intelli.ontology().apply(&ctx, demo_mutations())?;
    // A second write gives version history (lead time, change feeds).
    intelli.ontology().apply(
        &ctx,
        vec![Mutation::Patch(onto_core::EntityPatch {
            id: "issue:380".into(),
            state: Some("resolved".into()),
            ..Default::default()
        })],
    )?;
    let events: Vec<Mutation> = demo_events().into_iter().map(Mutation::Event).collect();
    let n_events = events.len();
    intelli.ontology().apply(&ctx, events)?;
    intelli.memory().remember(
        &Principal::system(),
        workspace,
        RememberInput {
            text: "Kenji is on call for Payments this week; page him first for checkout incidents.".into(),
            entities: vec!["person:kenji".into(), "team:payments".into()],
            kind: Some(MemoryKind::Fact),
            ..Default::default()
        },
    )?;
    Ok(SeedReport { workspace: workspace.into(), graph, events: n_events, memories: 1 })
}
