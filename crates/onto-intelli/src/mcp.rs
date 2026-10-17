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
                let mut args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
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
