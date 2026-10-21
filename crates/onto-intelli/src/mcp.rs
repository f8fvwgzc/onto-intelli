//! Model Context Protocol server (JSON-RPC 2.0, newline-delimited over stdio):
//! lets agents (Claude Code, IDEs, other MCP clients) query the same verified,
//! permission-filtered ontology people use. Writes go through governed action
//! types and are disabled unless `[mcp] allow_writes = true`.

use std::sync::Arc;

use serde_json::{Value, json};

use onto_core::Principal;

use crate::intelli::Intelli;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub struct McpServer {
    intelli: Arc<Intelli>,
    principal: Principal,
    allow_writes: bool,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

fn schema_change_props() -> Value {
    json!({
        "changes": {
            "type": "array",
            "description": "Schema changes, each {op, ...}: put_object_type {name, def}, delete_object_type {name}, rename_object_type {from, to}, put_property {object_type, name, def: {type, required?, values?}}, delete_property {object_type, name}, put_link_type {name, def: {from, to, inverse?, cardinality?}}, delete_link_type {name}, rename_link_type {from, to}, put_action_type {name, def}, put_rule {def: {name, chain, infer}}, put_metric {name, def}, put_interface {name, def}, delete_* {name}. Changes that break data in use need migrations.",
            "items": { "type": "object", "properties": { "op": { "type": "string" } }, "required": ["op"] }
        },
        "migrations": {
            "type": "array",
            "description": "Migrations for breaking changes: {action: delete_entities|retype {to}|map_states {map, default}|set_default {property, value}|convert|drop_values|delete_links|move_links {to}|keep_latest|acknowledge}.",
            "items": { "type": "object" }
        },
        "message": { "type": "string" }
    })
}

fn draft_props() -> Value {
    let s = |d: &str| json!({ "type": "string", "description": d });
    json!({
        "source": s("Data source (connector name); the only crawled source when omitted"),
        "tables": { "type": "array", "items": { "type": "string" }, "description": "schema.table names or globs (sales.*); all when omitted" },
        "exclude": { "type": "array", "items": { "type": "string" }, "description": "Globs to leave out (default staging.*, *_tmp, *_old, *_bak, *_backup)" },
        "prefix": s("Put before every object type name"),
        "sync": { "type": "boolean", "description": "Materialize rows as objects (default true)" },
        "actions": { "type": "boolean", "description": "Generate action types (default true)" },
        "views": { "type": "boolean", "description": "Model views too" }
    })
}

/// (tool name, engine action, description, input schema, writes)
fn tools() -> Vec<(&'static str, &'static str, &'static str, Value, bool)> {
    let s = |d: &str| json!({ "type": "string", "description": d });
    let arr = |d: &str| json!({ "type": "array", "items": { "type": "string" }, "description": d });
    vec![
        (
            "ask",
            "intelli.ask",
            "Answer a question about the workspace (deterministic from the ontology when possible, LLM reasoning otherwise). Returns the answer with the facts it used.",
            obj(
                json!({ "question": s("The question"), "conversation_id": s("Continue a conversation ('new' starts one)"), "workspace": s("Workspace") }),
                &["question"],
            ),
            false,
        ),
        (
            "search",
            "entity.search",
            "Hybrid permission-aware search over entities.",
            obj(
                json!({ "query": s("Search text"), "kinds": arr("Kinds or interfaces to include"), "limit": { "type": "integer" } }),
                &["query"],
            ),
            false,
        ),
        (
            "get_entity",
            "entity.get",
            "Fetch one entity (id like issue:421) with computed properties.",
            obj(json!({ "id": s("Entity id"), "derived": { "type": "boolean", "default": true } }), &["id"]),
            false,
        ),
        (
            "neighbors",
            "graph.neighbors",
            "Direct relations of an entity, optionally filtered by relation (inverse names allowed, e.g. owned_by).",
            obj(json!({ "id": s("Entity id"), "rel": s("Relation") }), &["id"]),
            false,
        ),
        (
            "find_data",
            "catalog.find",
            "Which tables or views hold something (\"revenue\", \"customer churn\"), ranked by OntoRank authority, with the reasons: descriptions, columns, certified metric definitions.",
            obj(json!({ "term": s("What to look for"), "limit": { "type": "integer" } }), &["term"]),
            false,
        ),
        (
            "join_path",
            "catalog.join",
            "How two tables join: certified or learned join paths first, else foreign-key chains (up to 4 hops), with the conditions.",
            obj(
                json!({ "from": s("Table id, e.g. table:warehouse.sales.orders"), "to": s("Table id") }),
                &["from", "to"],
            ),
            false,
        ),
        (
            "trusted_metric",
            "catalog.metric",
            "Metric definitions for a term, certified ones first, with who certified them and what they read.",
            obj(json!({ "term": s("Metric name or topic, e.g. revenue") }), &["term"]),
            false,
        ),
        (
            "authority",
            "catalog.rank",
            "OntoRank of an entity: the score and each component (centrality, usage, freshness, owner, documentation, certification) with its evidence.",
            obj(json!({ "id": s("Entity id") }), &["id"]),
            false,
        ),
        (
            "learned_facts",
            "learned.list",
            "Inferred join paths, metric definitions and synonyms with confidence and state (suggested, certified, rejected).",
            obj(
                json!({ "fact_type": { "type": "string", "enum": ["join_path", "metric", "synonym"] }, "state": s("suggested | certified | rejected"), "about": s("Entity id the facts concern") }),
                &[],
            ),
            false,
        ),
        (
            "crawl",
            "connector.crawl",
            "Crawl a registered connector now (metadata only); admins only.",
            obj(json!({ "name": s("Connector name (all when omitted)") }), &[]),
            true,
        ),
        (
            "find_entities",
            "entity.find",
            "Filter entities by kind/interface, state, open, and scope (within a team/project/person).",
            obj(
                json!({ "kind": s("Kind or interface"), "state": arr("States"), "open": { "type": "boolean" }, "within": s("Scope entity id"), "limit": { "type": "integer" } }),
                &[],
            ),
            false,
        ),
        (
            "explain_blockers",
            "graph.root_cause",
            "Root-cause chains from an entity to things in alert states (deepest first).",
            obj(json!({ "id": s("Entity id") }), &["id"]),
            false,
        ),
        (
            "impact",
            "graph.impact",
            "What is affected if an entity fails or slips.",
            obj(json!({ "id": s("Entity id") }), &["id"]),
            false,
        ),
        (
            "dependencies",
            "graph.dependencies",
            "Upstream (default) or downstream dependencies.",
            obj(
                json!({ "id": s("Entity id"), "direction": { "type": "string", "enum": ["upstream", "downstream"] } }),
                &["id"],
            ),
            false,
        ),
        (
            "critical_path",
            "graph.critical_path",
            "Execution order and critical path of open work under an entity.",
            obj(json!({ "id": s("Project, milestone or issue id") }), &["id"]),
            false,
        ),
        (
            "metric",
            "metric.query",
            "Evaluate a governed metric (see list in schema) or an ad-hoc one, optionally grouped.",
            obj(
                json!({ "metric": s("Named metric"), "kind": s("Kind for ad-hoc metrics"), "measure": { "type": "string", "enum": ["count", "sum", "avg", "min", "max", "lead_time", "throughput", "failure_rate", "event_count"] }, "group_by": arr("Dimensions"), "within": s("Scope entity id"), "window_days": { "type": "integer" } }),
                &[],
            ),
            false,
        ),
        (
            "history",
            "entity.history",
            "Version history of an entity with changed fields and actors.",
            obj(json!({ "id": s("Entity id") }), &["id"]),
            false,
        ),
        (
            "changes",
            "graph.changes",
            "What changed recently, optionally inside an entity's scope.",
            obj(json!({ "since_days": { "type": "number" }, "within": s("Scope entity id") }), &[]),
            false,
        ),
        (
            "experts",
            "people.experts",
            "Who knows about a topic, with reasons.",
            obj(json!({ "topic": s("Topic") }), &["topic"]),
            false,
        ),
        (
            "data_quality",
            "quality.report",
            "Data-quality report: missing links, stale items, duplicates, orphans.",
            obj(json!({ "kind": s("Restrict to a kind") }), &[]),
            false,
        ),
        (
            "what_if",
            "scenario.run",
            "Apply hypothetical mutations to a fork of the graph and compare read queries before/after. Nothing is persisted.",
            obj(
                json!({ "mutations": { "type": "array", "items": { "type": "object" } }, "queries": { "type": "array", "items": { "type": "object", "properties": { "action": { "type": "string" }, "input": { "type": "object" } } } } }),
                &["mutations", "queries"],
            ),
            false,
        ),
        (
            "list_actions",
            "action.list",
            "Governed action types: parameters, preconditions, effects, risk.",
            obj(json!({}), &[]),
            false,
        ),
        (
            "plan_action",
            "action.plan",
            "Validate an action and preview its edits, risk and reversibility without writing.",
            obj(json!({ "action": s("Action type"), "params": { "type": "object" } }), &["action", "params"]),
            false,
        ),
        (
            "apply_action",
            "action.apply",
            "Apply an action type (high-risk or irreversible actions need confirm=true). Disabled unless the server allows writes.",
            obj(
                json!({ "action": s("Action type"), "params": { "type": "object" }, "confirm": { "type": "boolean" } }),
                &["action", "params"],
            ),
            true,
        ),
        (
            "undo_action",
            "action.undo",
            "Undo an applied action by receipt id. Disabled unless the server allows writes.",
            obj(json!({ "receipt": s("Receipt id") }), &["receipt"]),
            true,
        ),
        (
            "get_schema",
            "schema.get",
            "The active ontology schema: object types (properties, states), link types (from/to, cardinality, inverse), interfaces, action types, rules, metrics, and its version.",
            obj(json!({}), &[]),
            false,
        ),
        (
            "plan_schema_change",
            "schema.plan",
            "Dry-run a schema change set: validation errors, diff, impact (entities/links affected, breaking or not) and the migrations it still needs. Nothing is written.",
            obj(schema_change_props(), &["changes"]),
            false,
        ),
        (
            "propose_schema_change",
            "schema.propose",
            "File a schema change proposal for human review (new or changed object types, properties, link types, action types, rules...). It is applied only when an approver accepts it. Plan it first with plan_schema_change.",
            obj(
                {
                    let mut p = schema_change_props();
                    p["title"] = s("Short title");
                    p["rationale"] = s("Why the change is needed, with the evidence that motivates it");
                    p
                },
                &["title", "rationale", "changes"],
            ),
            false,
        ),
        (
            "draft_ontology",
            "ontology.draft",
            "Draft an ontology from a crawled data source, the way Palantir's ontology builder starts one: an object type per table (key, title, typed properties, states), link types from foreign keys and join tables, and create/edit/transition/relink action types, each with its reason. Read-only.",
            obj(draft_props(), &[]),
            false,
        ),
        (
            "refine_ontology",
            "ontology.refine",
            "The draft improved by the server's language model (names, labels, descriptions, property types, inverses, transitions). Suggestions that would make the schema invalid are rejected and reported. Read-only.",
            obj(
                {
                    let mut p = draft_props();
                    p["instructions"] = s("What to pay attention to, e.g. 'use the names the sales team uses'");
                    p
                },
                &[],
            ),
            false,
        ),
        (
            "advise_ontology",
            "ontology.advise",
            "Modeling advice grounded in the draft: which tables become object types, which links and actions they get, which property types columns map to, and why.",
            obj(
                json!({ "question": s("The modeling question"), "source": s("Data source (connector name)") }),
                &["question"],
            ),
            false,
        ),
        (
            "propose_ontology",
            "ontology.apply_draft",
            "File the drafted ontology (or a reviewed `draft` object from draft_ontology/refine_ontology) as a schema change proposal. It is applied only when a person approves it; agents never apply schema.",
            obj(
                {
                    let mut p = draft_props();
                    p["draft"] = json!({ "type": "object", "description": "A draft returned by draft_ontology or refine_ontology, possibly edited" });
                    p["title"] = s("Short title");
                    p["rationale"] = s("Why the ontology is needed, with the evidence");
                    p
                },
                &[],
            ),
            false,
        ),
        (
            "object_types",
            "objects.types",
            "Object types with how many objects you can see, their backing table, properties, links and actions.",
            obj(json!({ "workspace": s("Workspace") }), &[]),
            false,
        ),
        (
            "explore_objects",
            "objects.explore",
            "Object Explorer: the objects of one type, filtered, searched, sorted and paged, with facet counts and numeric statistics.",
            obj(
                json!({
                    "kind": s("Object type or interface"),
                    "filters": { "type": "array", "description": "Conditions that must all hold", "items": { "type": "object", "properties": {
                        "prop": s("Property, state, title, workspace, id, created_at, updated_at or link:<rel> (a property of the same name wins; _state, _created_at and so on always name the metadata)"),
                        "op": { "type": "string", "enum": ["eq", "ne", "in", "gt", "gte", "lt", "lte", "contains", "exists", "missing"] },
                        "value": { "description": "Compared value (a list for in)" } }, "required": ["prop", "op"] } },
                    "search": s("Full-text search within the type"),
                    "sort": s("prop ascending or -prop descending"),
                    "workspace": s("Only objects of this workspace; empty explores every workspace you may read"),
                    "facets": { "type": "array", "items": { "type": "string" }, "description": "Fields to count values of; omitted picks the state, enum, bool and low-cardinality string properties" },
                    "stats": { "type": "array", "items": { "type": "string" }, "description": "Properties to summarize (count, min, max, mean, sum, histogram); omitted picks the int, float and timestamp properties" },
                    "limit": { "type": "integer" },
                    "offset": { "type": "integer" }
                }),
                &["kind"],
            ),
            false,
        ),
        (
            "list_proposals",
            "schema.proposals",
            "Schema change proposals with status (open, applied, rejected, failed, withdrawn), approvals and impact.",
            obj(
                json!({ "status": { "type": "string", "enum": ["open", "applied", "rejected", "failed", "withdrawn"] }, "limit": { "type": "integer" } }),
                &[],
            ),
            false,
        ),
        (
            "get_proposal",
            "schema.proposal",
            "One schema change proposal with its diff, impact, reviews and a fresh preview if the schema moved on.",
            obj(json!({ "id": s("Proposal id") }), &["id"]),
            false,
        ),
    ]
}

impl McpServer {
    pub fn new(intelli: Arc<Intelli>, principal: Principal, allow_writes: bool) -> Self {
        Self { intelli, principal, allow_writes }
    }

    fn reply(id: &Value, result: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "result": result })
    }

    fn error(id: &Value, code: i64, message: &str) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
    }

    /// Handle one JSON-RPC message; `None` for notifications.
    pub async fn handle(&self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            return None; // notification (e.g. notifications/initialized)
        };
        Some(match method.as_str() {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
                let version = if SUPPORTED.contains(&asked) { asked } else { PROTOCOL_VERSION };
                Self::reply(
                    &id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": "onto-intelli", "version": crate::VERSION },
                        "instructions": "Ontology of the workspace: teams, people, projects, issues, tasks, services, knowledge. \
                            Prefer `ask` for questions; use graph tools for precise lookups; cite entity ids. \
                            Writes only through plan_action/apply_action (governed, reversible).",
                    }),
                )
            }
            "ping" => Self::reply(&id, json!({})),
            "tools/list" => {
                let list: Vec<Value> = tools()
                    .into_iter()
                    .filter(|t| self.allow_writes || !t.4)
                    .map(|(name, _, description, schema, _)| json!({ "name": name, "description": description, "inputSchema": schema }))
                    .collect();
                Self::reply(&id, json!({ "tools": list }))
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                // Absent and null arguments are both "no arguments".
                let mut args = params.get("arguments").cloned().filter(|a| !a.is_null()).unwrap_or_else(|| json!({}));
                let Some((_, action, _, _, writes)) = tools().into_iter().find(|t| t.0 == name) else {
                    return Some(Self::error(&id, -32602, &format!("unknown tool {name}")));
                };
                if writes && !self.allow_writes {
                    return Some(Self::reply(
                        &id,
                        json!({ "content": [{ "type": "text", "text": "Writes are disabled on this server ([mcp] allow_writes = false). Use plan_action to preview." }], "isError": true }),
                    ));
                }
                if name == "get_entity" && args.get("derived").is_none() {
                    args["derived"] = json!(true);
                }
                if name == "propose_schema_change" && args.get("source").is_none() {
                    args["source"] = json!("mcp");
                }
                // Agents never apply schema: an ontology from MCP is always a
                // proposal, filed whatever the arguments say (no preview).
                if name == "propose_ontology" {
                    let Some(m) = args.as_object_mut() else {
                        return Some(Self::error(&id, -32602, "propose_ontology takes an object of arguments"));
                    };
                    for k in ["propose", "preview", "no_crawl"] {
                        m.remove(k);
                    }
                    m.insert("mode".into(), json!("propose"));
                    m.insert("proposal_source".into(), json!("mcp"));
                }
                match self.intelli.call(&self.principal, action, args).await {
                    Ok(v) => {
                        let text = if name == "ask" {
                            let facts: Vec<String> = v["facts"]
                                .as_array()
                                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| format!("- {s}"))).collect())
                                .unwrap_or_default();
                            format!(
                                "{}{}",
                                v["answer"].as_str().unwrap_or_default(),
                                if facts.is_empty() {
                                    String::new()
                                } else {
                                    format!("\n\nFacts:\n{}", facts.join("\n"))
                                }
                            )
                        } else {
                            serde_json::to_string_pretty(&v).unwrap_or_default()
                        };
                        Self::reply(
                            &id,
                            json!({ "content": [{ "type": "text", "text": text }], "structuredContent": v, "isError": false }),
                        )
                    }
                    Err(e) => Self::reply(
                        &id,
                        json!({ "content": [{ "type": "text", "text": e.to_string() }], "isError": true }),
                    ),
                }
            }
            "resources/list" => Self::reply(&id, json!({ "resources": [] })),
            "prompts/list" => Self::reply(&id, json!({ "prompts": [] })),
            other => Self::error(&id, -32601, &format!("method not found: {other}")),
        })
    }
}
