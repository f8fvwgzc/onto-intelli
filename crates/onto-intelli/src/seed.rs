//! Demo workspace ("acme"): teams, people, projects, issues, tasks, services,
//! a failed deployment, knowledge, a decision, a generated artifact with
//! provenance, audit events, a long-term memory, and an analytics warehouse
//! catalog (as a PostgreSQL connector would crawl it) with one certified
//! metric. `onto seed demo`.

use serde::Serialize;
use serde_json::json;

use onto_core::catalog::{
    CatalogSnapshot, ColumnInfo, ForeignKey, QueryStat, RelKind, RelationInfo, SchemaInfo, SourceInfo, ViewDep,
};
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
        e("team", "finance", "Finance Team").prop("mission", "Revenue recognition, reporting and forecasting"),
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
    /// The demo warehouse catalog (when the data-catalog package is layered).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warehouse: Option<crate::connectors::CrawlReport>,
}

fn onto_has_catalog(intelli: &Intelli) -> bool {
    intelli.ontology().schema().object_type("table").is_some()
}

fn col(name: &str, data_type: &str, comment: &str) -> ColumnInfo {
    ColumnInfo {
        name: name.into(),
        data_type: data_type.into(),
        nullable: true,
        comment: comment.into(),
        ..Default::default()
    }
}

fn pk(name: &str, comment: &str) -> ColumnInfo {
    ColumnInfo { primary_key: true, nullable: false, ..col(name, "bigint", comment) }
}

/// The demo "analytics" warehouse as a PostgreSQL connector reads it:
/// documented sales and finance schemas, a finance view that defines
/// revenue, a stale undocumented copy in staging, and the query log.
pub fn demo_warehouse() -> CatalogSnapshot {
    let day = 86_400_000;
    let now = onto_core::time::now_ms();
    let rel = |schema: &str, name: &str, comment: &str, owner: &str, columns: Vec<ColumnInfo>| RelationInfo {
        schema: schema.into(),
        name: name.into(),
        comment: comment.into(),
        owner: owner.into(),
        columns,
        public_read: true,
        last_activity: Some(now - day),
        ..Default::default()
    };
    CatalogSnapshot {
        source: SourceInfo {
            key: "warehouse".into(),
            title: "Analytics warehouse".into(),
            engine: "postgres".into(),
            host: "warehouse.acme.internal".into(),
            database: "analytics".into(),
            server_version: "16.4".into(),
        },
        schemas: vec![
            SchemaInfo {
                name: "sales".into(),
                comment: "Orders, customers and line items from the shop".into(),
                owner: "sales_owner".into(),
            },
            SchemaInfo {
                name: "finance".into(),
                comment: "Finance-curated reporting".into(),
                owner: "finance_owner".into(),
            },
            SchemaInfo { name: "staging".into(), comment: String::new(), owner: "etl".into() },
        ],
        relations: vec![
            RelationInfo {
                reads: 52_000,
                writes: 1_200,
                row_estimate: 48_000,
                ..rel(
                    "sales",
                    "customers",
                    "One row per customer account",
                    "sales_owner",
                    vec![
                        pk("id", "Customer id"),
                        col("cust_name", "text", "Customer name"),
                        col("region", "text", "Sales region"),
                        col("created_at", "timestamptz", "When the account was opened"),
                    ],
                )
            },
            RelationInfo {
                reads: 120_000,
                writes: 45_000,
                row_estimate: 2_400_000,
                ..rel(
                    "sales",
                    "orders",
                    "One row per order placed in the shop",
                    "sales_owner",
                    vec![
                        pk("id", "Order id"),
                        col("customer_id", "bigint", "Customer who placed the order"),
                        col("amount", "numeric(12,2)", "Order total in EUR, also known as GMV"),
                        col("status", "text", "placed, paid, refunded"),
                        col("created_at", "timestamptz", "When the order was placed"),
                    ],
                )
            },
            RelationInfo {
                reads: 30_000,
                writes: 90_000,
                row_estimate: 7_100_000,
                ..rel(
                    "sales",
                    "order_items",
                    "Line items of each order",
                    "sales_owner",
                    vec![
                        col("order_id", "bigint", "Order the line belongs to"),
                        col("product_sku", "text", "Stock keeping unit"),
                        col("qty", "integer", ""),
                        col("unit_price", "numeric(12,2)", "Price per unit in EUR"),
                    ],
                )
            },
            RelationInfo {
                kind: RelKind::View,
                definition: Some(
                    "SELECT date_trunc('day', o.created_at) AS day, sum(o.amount) AS revenue FROM sales.orders o \
                     WHERE o.status = 'paid' GROUP BY 1"
                        .into(),
                ),
                ..rel(
                    "finance",
                    "revenue_recognized",
                    "Recognized revenue per day: the finance definition used in board reporting",
                    "finance_owner",
                    vec![
                        col("day", "timestamptz", "Calendar day"),
                        col("revenue", "numeric", "Recognized revenue in EUR"),
                    ],
                )
            },
            RelationInfo {
                reads: 2,
                writes: 10,
                row_estimate: 400,
                last_activity: Some(now - 200 * day),
                ..rel("staging", "revenue_tmp", "", "etl", vec![col("day", "date", ""), col("rev", "numeric", "")])
            },
        ],
        foreign_keys: vec![
            ForeignKey {
                name: "orders_customer_id_fkey".into(),
                from_schema: "sales".into(),
                from_table: "orders".into(),
                from_columns: vec!["customer_id".into()],
                to_schema: "sales".into(),
                to_table: "customers".into(),
                to_columns: vec!["id".into()],
            },
            ForeignKey {
                name: "order_items_order_id_fkey".into(),
                from_schema: "sales".into(),
                from_table: "order_items".into(),
                from_columns: vec!["order_id".into()],
                to_schema: "sales".into(),
                to_table: "orders".into(),
                to_columns: vec!["id".into()],
            },
        ],
        view_deps: vec![ViewDep {
            view_schema: "finance".into(),
            view: "revenue_recognized".into(),
            schema: "sales".into(),
            name: "orders".into(),
        }],
        queries: vec![
            QueryStat {
                queryid: "4871032266119012334".into(),
                text: "SELECT day, revenue FROM finance.revenue_recognized WHERE day >= $1 ORDER BY day".into(),
                calls: 9_800,
                mean_ms: 4.2,
                rows: 290_000,
            },
            QueryStat {
                queryid: "6170358721453327845".into(),
                text: "SELECT c.region, sum(o.amount) AS revenue FROM sales.orders o JOIN sales.customers c \
                       ON o.customer_id = c.id WHERE o.created_at >= $1 GROUP BY c.region"
                    .into(),
                calls: 4_200,
                mean_ms: 38.0,
                rows: 21_000,
            },
            QueryStat {
                queryid: "2301984467730021116".into(),
                text: "SELECT i.product_sku, sum(i.qty) AS units FROM sales.order_items i JOIN sales.orders o \
                       ON i.order_id = o.id GROUP BY 1"
                    .into(),
                calls: 900,
                mean_ms: 120.0,
                rows: 81_000,
            },
            QueryStat {
                queryid: "1123581321345589144".into(),
                text: "SELECT sum(rev) AS revenue FROM staging.revenue_tmp".into(),
                calls: 3,
                mean_ms: 0.4,
                rows: 3,
            },
        ],
        notes: Vec::new(),
    }
}

/// Load the demo warehouse the way a crawl would, and have the finance
/// lead certify the board revenue definition.
pub fn load_demo_warehouse(intelli: &Intelli, workspace: &str) -> Result<crate::connectors::CrawlReport> {
    let onto = intelli.ontology();
    let spec: crate::connectors::ConnectorSpec = serde_json::from_value(json!({
        "name": "warehouse",
        "url": "env:WAREHOUSE_URL",
        "workspace": workspace,
        "owner_map": { "finance_owner": "team:finance", "sales_owner": "team:payments" },
    }))?;
    let report = crate::connectors::ingest_snapshot(
        onto,
        &spec,
        demo_warehouse(),
        Default::default(),
        onto_core::time::now_ms(),
    )?;
    let lead = Principal::named("finance-lead", &["admin"], &["*"])?;
    if let Some(fact) = onto
        .learned_facts(&lead, Some("metric"), Some("suggested"), None, 100)?
        .into_iter()
        .find(|f| f.name == "revenue_recognized")
    {
        let ctx = ActionContext::new(lead.clone(), workspace).with_source("seed");
        let mut params = serde_json::Map::new();
        params.insert("fact".into(), json!(fact.id));
        params.insert("note".into(), json!("Board reporting definition"));
        let confirm = onto_core::action_types::ApplyOptions { confirm: true, ..Default::default() };
        onto.apply_action(&ctx, "certify_fact", &params, confirm)?;
        let system = ActionContext::system(workspace).with_source("seed");
        onto.compute_authority(&system, &[], &Default::default())?;
    }
    Ok(report)
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
    let warehouse = if onto_has_catalog(intelli) { Some(load_demo_warehouse(intelli, workspace)?) } else { None };
    Ok(SeedReport { workspace: workspace.into(), graph, events: n_events, memories: 1, warehouse })
}
