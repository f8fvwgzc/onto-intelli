//! Model Context Protocol server (JSON-RPC 2.0; newline-delimited over stdio
//! in `onto mcp`, the Streamable HTTP transport at `POST /mcp` in onto-grpc):
//! lets agents (Claude Code, IDEs, other MCP clients) query the same verified,
//! permission-filtered ontology people use. Writes go through governed action
//! types and are disabled unless `[mcp] allow_writes = true`.
//!
//! A tool profile ([`MCP_PROFILES`]) narrows the tool list to one job
//! (catalog, authoring, reasoning, governance, retrieval, evaluation) on top
//! of a floor every profile offers ([`FLOOR_TOOLS`]); `full` offers every
//! tool. The profile comes from `onto mcp --profile`, else `ONTO_MCP_PROFILE`,
//! else `[mcp] profile` ([`resolve_profile`]), and [`McpServer::with_profile`]
//! refuses an unknown name. `initialize` reports the profile and how many
//! tools it withholds.

use std::sync::Arc;

use serde_json::{Value, json};

use onto_core::Principal;

use crate::intelli::Intelli;
use crate::trace::TraceCtx;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Protocol versions this server speaks (the HTTP transport checks
/// `Mcp-Protocol-Version` against it).
pub const SUPPORTED: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub struct McpServer {
    intelli: Arc<Intelli>,
    principal: Principal,
    allow_writes: bool,
    ctx: TraceCtx,
    profile: &'static str,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

fn schema_change_props() -> Value {
    json!({
        "changes": {
            "type": "array",
            "description": "Schema changes, each {op, ...}: put_object_type {name, def}, delete_object_type {name}, rename_object_type {from, to}, put_property {object_type, name, def: {type, required?, values?}}, delete_property {object_type, name}, put_link_type {name, def: {from, to, inverse?, cardinality?}}, delete_link_type {name}, rename_link_type {from, to}, put_action_type {name, def}, put_rule {def: {name, chain, infer}}, put_metric {name, def}, put_interface {name, def}, put_shape {name, def}, put_automation {name, def: {run_as, trigger, conditions?, effects}} (defines a change-triggered automation; a person enables it separately), delete_* {name}. Changes that break data in use need migrations.",
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
            "poll_changes",
            "changes.poll",
            "One page of the change feed after a cursor: each record says what one commit changed (op, kind, id, changed fields, old values when you could see them before, the entity after). Pass `next` back as `after` for the following page. `expired` means records were removed: re-read with find_entities (not pinned to a cursor). A `gap` record means state changed without records.",
            obj(
                json!({
                    "after": s("Cursor from a previous page's next (like 42.0)"),
                    "from": { "type": "string", "enum": ["earliest", "now"], "description": "Without after: earliest (default) or now" },
                    "limit": { "type": "integer", "description": "Records scanned (default 500, at most 1000)" },
                    "kinds": arr("Entity kinds or interfaces (links match through either end)"),
                    "ops": arr("entity.create, entity.update, entity.delete, entity.hidden, link.create, link.update, link.delete, schema.change, gap"),
                    "workspaces": arr("Workspaces"),
                    "ids": arr("Entity ids (at most 1000)"),
                    "rels": arr("Relations (link records only)"),
                    "props": arr("Updates that changed any of these fields"),
                    "sources": arr("Globs on the writer's source; !x excludes"),
                    "fields": { "type": "string", "enum": ["full", "ids"], "description": "ids: no documents or old values" },
                    "include_migrations": { "type": "boolean", "description": "Records written by schema migrations (default true)" }
                }),
                &[],
            ),
            false,
        ),
        (
            "list_automations",
            "automation.list",
            "Automations defined in the schema: what each one reacts to (its change trigger), what it does (an action type and webhooks), who it runs as, and whether it is active (or why not). Definitions are visible to everyone who can read the schema; agents change them only through propose_schema_change (put_automation) and never enable them.",
            obj(json!({}), &[]),
            false,
        ),
        (
            "automation_runs",
            "automation.runs",
            "An automation's runs, newest first: state (succeeded, failed, retry, skipped, refused, would_run, cancelled), the reason, the change that triggered it, and its effects (receipts, outbox ids). Runs of automations you do not cover are counted as hidden.",
            obj(
                json!({
                    "name": s("Automation name (all when omitted)"),
                    "state": arr("States to list"),
                    "since": { "type": "integer", "description": "Started at or after (Unix ms)" },
                    "limit": { "type": "integer", "description": "At most this many (default 50)" },
                    "id": s("One run")
                }),
                &[],
            ),
            false,
        ),
        (
            "test_automation",
            "automation.test",
            "Dry-run an automation against a change (cursor) or an entity as if it was just updated (id): whether its trigger admits it, its conditions, the action plan and the webhook payloads, rendered as the run principal and you together. Writes nothing.",
            obj(
                json!({
                    "name": s("Automation name"),
                    "cursor": s("A change cursor (like 42.0)"),
                    "id": s("An entity id")
                }),
                &["name"],
            ),
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
        (
            "query_inferred",
            "reason.query",
            "Match triple patterns over asserted AND derived facts (rule chains, inheritance, transitive and symmetric links). Atoms are [subject, predicate, object] with ?variables: [[\"?i\", \"handled_by_team\", \"team:payments\"]]. Derived is not asserted: a derived fact is the engine's opinion under the schema rules, never stored data; explain_fact shows why. With fixpoint_reached false the answer is a lower bound. state and prop.<name> facts are in the closure only when a schema rule reads them: an atom with such a predicate is refused otherwise, and an atom with a ?variable predicate is answered without them (truncated says so). At most 8 atoms.",
            obj(
                json!({
                    "pattern": { "type": "array", "description": "Atoms [s, p, o]: entity ids, relation names (inverse names allowed), `a` with a kind, `state` or `prop.<name>` (only when a rule reads them); ?x for variables", "items": { "type": "array", "items": {} } },
                    "workspace": s("Only this workspace's entities"),
                    "limit": { "type": "integer", "description": "Bindings returned (default 50, at most 1000)" }
                }),
                &["pattern"],
            ),
            false,
        ),
        (
            "explain_fact",
            "reason.explain",
            "Why a fact holds: the minimal sets of asserted facts and the rules it is derived from, each re-derived independently. A derived fact rests on supplied schema rules, which are assumptions, not facts; an asserted fact is its own justification.",
            obj(
                json!({
                    "fact": { "description": "[subject, relation, object] or {subject, relation, object}, e.g. [\"issue:421\", \"handled_by_team\", \"team:payments\"]" },
                    "workspace": s("Only this workspace's entities"),
                    "max": { "type": "integer", "description": "Justifications listed (default 10)" }
                }),
                &["fact"],
            ),
            false,
        ),
        (
            "verify_claims",
            "reason.verify",
            "Check statements before you rely on them: each claim gets asserted, derived (with its derivation), not_entailed, contradicted (with the stored facts that say otherwise) or outside_vocabulary (it names a kind, relation, property or entity this schema and reader do not have, or an entity id the reasoner refuses to spell; the reason says which). A claim may carry the rule and premises of a reasoning step to check that step. not_entailed is not false: the graph may simply not say.",
            obj(
                json!({
                    "claims": { "type": "array", "description": "Claims {subject, relation, object, rule?, premises?: [claim]}", "items": { "type": "object", "properties": { "subject": s("Entity id or schema name"), "relation": s("Relation, a, state or prop.<name>"), "object": { "description": "Entity id, name or value" }, "rule": s("Rule the step claims to apply"), "premises": { "type": "array", "items": { "type": "object" } } }, "required": ["subject", "relation", "object"] } },
                    "workspace": s("Only this workspace's entities")
                }),
                &["claims"],
            ),
            false,
        ),
        (
            "check_slice",
            "reason.slice_check",
            "Does a subset of entities (the context you plan to use) keep what the whole visible graph derives, or the goals you name? Lists the facts and entities the slice loses. Coverage is a proxy; preservation is the property.",
            obj(
                json!({
                    "entities": arr("Entity ids in the slice"),
                    "goals": { "type": "array", "description": "Claims {subject, relation, object} the slice must preserve", "items": { "type": "object" } },
                    "workspace": s("Only this workspace's entities")
                }),
                &["entities"],
            ),
            false,
        ),
        (
            "certify_reasoning",
            "reason.certify",
            "A reasoning certificate (rules.tsv, asserted.tsv, derivations.tsv, manifest.json) and the verdict of the independent onto-cert checker, which re-derives every line by pattern alone. Only `check` carries a checker verdict; entailed_under_supplied_rules means the conclusions hold IF the supplied schema rules do. A certificate binds bytes, not the state of the world. With fixpoint_reached false (also given at the top level) the closure stopped at a limit and the derivations are a lower bound.",
            obj(json!({ "workspace": s("Only this workspace's entities") }), &[]),
            false,
        ),
        (
            "validate_shapes",
            "shapes.validate",
            "Validate the schema's shapes (cardinality, classes, datatypes, values, patterns, closed shapes) over the graph. The verdict is conforms, violates or undetermined; undetermined (something was skipped) is never valid. A shape report is a measurement of the data, not a proof.",
            obj(
                json!({
                    "shapes": arr("Shape names (all when omitted)"),
                    "workspace": s("Only this workspace's entities"),
                    "inferred": { "type": "boolean", "description": "Also read derived classes and relations" },
                    "limit": { "type": "integer", "description": "Results listed (all are counted)" }
                }),
                &[],
            ),
            false,
        ),
        (
            "induce_shape",
            "shapes.induce",
            "Read a shape for one object type from its data, with the counts behind each constraint and a put_shape change set to plan or propose. It describes the data as it is, not what it should be.",
            obj(
                json!({ "kind": s("Object type"), "workspace": s("Only this workspace's entities"), "name": s("Shape name (default <kind>_shape)") }),
                &["kind"],
            ),
            false,
        ),
        (
            "check_shapes",
            "shapes.check",
            "Structural dry run of draft shape definitions against the schema (names, keys, patterns, bounds) before you propose them. Nothing is validated against data and nothing is saved.",
            obj(
                json!({ "shapes": { "type": "object", "description": "Shape definitions by name: {description?, target?, severity?, closed?, properties: [{path, min_count?, max_count?, class?, datatype?, in?, pattern?, ...}]}" } }),
                &["shapes"],
            ),
            false,
        ),
        (
            "lint_schema",
            "schema.lint",
            "Schema lint: missing descriptions and inverses, orphan and unused types and link types, dead or recursive rules, keyword collisions, duplicate labels, shape references, with a fix for each finding. A lint report is a measurement by fixed heuristics, not a consistency proof; dismissed findings are counted in `suppressed`.",
            obj(
                json!({ "checks": arr("Checks to run (all when omitted)"), "include_dismissed": { "type": "boolean" } }),
                &[],
            ),
            false,
        ),
        (
            "list_packs",
            "pack.list",
            "Industry packs (object types, link types, rules, shapes, metrics, actions) with counts and whether each is installed. The standards a pack lists are vocabulary it borrows, not compliance it claims.",
            obj(json!({}), &[]),
            false,
        ),
        (
            "show_pack",
            "pack.show",
            "Everything one pack defines, its rules as text, and the lint findings it would add to the current schema.",
            obj(json!({ "id": s("Pack id, e.g. manufacturing") }), &["id"]),
            false,
        ),
        (
            "plan_pack",
            "pack.plan",
            "The schema plan of installing a pack: changes, consequences (derived facts that appear, conservative or not), risk, lock violations and shape delta. Nothing is written.",
            obj(json!({ "id": s("Pack id") }), &["id"]),
            false,
        ),
        (
            "install_pack",
            "pack.install",
            "File a pack's schema as a change proposal for human review. Over MCP it is always a proposal: agents never apply schema. Plan it first with plan_pack.",
            obj(json!({ "id": s("Pack id"), "message": s("Why the pack is needed") }), &["id"]),
            false,
        ),
        (
            "align_schema",
            "schema.align",
            "Align a pack (or a TOML schema) onto the schema in force: scored object and link type matches with the signals behind each, SSSOM TSV and a glossary change set to propose. Scores are opinions; certify a mapping before relying on it.",
            obj(
                json!({ "pack": s("Pack id"), "toml": s("A schema TOML document (instead of pack)"), "threshold": { "type": "number", "description": "Minimum score of a candidate (default 0.5)" } }),
                &[],
            ),
            false,
        ),
        (
            "induce_sheet",
            "model.induce",
            "One sheet in, one ontology out: from CSV text, an object type (typed properties, key, title), links to existing objects and a shape, each with its evidence, plus the change set to plan or propose. Nothing is written; it describes the data as it is.",
            obj(
                json!({ "csv": s("The CSV text (header row first)"), "kind": s("Object type name"), "key": s("Key column"), "workspace": s("Workspace whose objects links may target") }),
                &["csv"],
            ),
            false,
        ),
    ]
}

/// Tool profiles, the names `[mcp] profile`, `ONTO_MCP_PROFILE` and
/// `onto mcp --profile` accept. `full` (the default) offers every tool.
pub const MCP_PROFILES: &[&str] =
    &["full", "catalog", "authoring", "reasoning", "governance", "retrieval", "evaluation"];

/// Tools every profile offers: what an agent needs to orient itself.
pub const FLOOR_TOOLS: &[&str] = &["ask", "search", "get_entity", "get_schema"];

/// The tools each profile adds to [`FLOOR_TOOLS`] (`full` offers all).
const PROFILE_TOOLS: &[(&str, &[&str])] = &[
    (
        "catalog",
        &[
            "find_data",
            "join_path",
            "trusted_metric",
            "authority",
            "learned_facts",
            "crawl",
            "object_types",
            "explore_objects",
            "find_entities",
            "neighbors",
            "metric",
            "history",
            "changes",
            "poll_changes",
            "experts",
        ],
    ),
    (
        "authoring",
        &[
            "object_types",
            "draft_ontology",
            "refine_ontology",
            "advise_ontology",
            "propose_ontology",
            "plan_schema_change",
            "propose_schema_change",
            "list_proposals",
            "get_proposal",
            "test_automation",
            "list_packs",
            "show_pack",
            "plan_pack",
            "install_pack",
            "align_schema",
            "induce_sheet",
            "induce_shape",
            "check_shapes",
            "validate_shapes",
            "lint_schema",
        ],
    ),
    (
        "reasoning",
        &[
            "query_inferred",
            "explain_fact",
            "verify_claims",
            "check_slice",
            "certify_reasoning",
            "neighbors",
            "find_entities",
            "dependencies",
            "impact",
            "explain_blockers",
            "critical_path",
            "what_if",
        ],
    ),
    (
        "governance",
        &[
            "list_actions",
            "plan_action",
            "apply_action",
            "undo_action",
            "plan_schema_change",
            "propose_schema_change",
            "list_proposals",
            "get_proposal",
            "lint_schema",
            "validate_shapes",
            "plan_pack",
            "install_pack",
            "verify_claims",
            "certify_reasoning",
            "data_quality",
            "history",
            "changes",
            "poll_changes",
            "list_automations",
            "automation_runs",
            "test_automation",
        ],
    ),
    (
        "retrieval",
        &[
            "find_entities",
            "neighbors",
            "explore_objects",
            "object_types",
            "find_data",
            "trusted_metric",
            "metric",
            "history",
            "changes",
            "poll_changes",
            "experts",
            "query_inferred",
            "explain_fact",
            "check_slice",
        ],
    ),
    (
        "evaluation",
        &[
            "verify_claims",
            "certify_reasoning",
            "check_slice",
            "explain_fact",
            "query_inferred",
            "validate_shapes",
            "lint_schema",
            "data_quality",
            "metric",
            "what_if",
        ],
    ),
];

/// Does `profile` offer `tool`? Unknown profiles offer nothing.
pub fn profile_offers(profile: &str, tool: &str) -> bool {
    profile == "full"
        || FLOOR_TOOLS.contains(&tool)
        || PROFILE_TOOLS.iter().any(|(p, tools)| *p == profile && tools.contains(&tool))
}

/// Why a `what_if` call is refused under `profile`: a scenario query runs an
/// engine action, so under a focused profile each one must be the action of
/// a tool that profile offers (else what_if would reach what it withholds).
fn scenario_refusal(profile: &str, args: &Value) -> Option<String> {
    if profile == "full" {
        return None;
    }
    let tools = tools();
    for q in args.get("queries").and_then(Value::as_array).into_iter().flatten() {
        let action = q.get("action").and_then(Value::as_str).unwrap_or_default();
        let offered: Vec<&str> = tools.iter().filter(|t| t.1 == action).map(|t| t.0).collect();
        if offered.is_empty() {
            return Some(format!(
                "what_if query {action} is no MCP tool's action, so the profile {profile} does not offer it"
            ));
        }
        if !offered.iter().any(|t| profile_offers(profile, t)) {
            return Some(format!(
                "what_if query {action} is the action of {}, which the MCP profile {profile} does not offer",
                offered.join(", ")
            ));
        }
    }
    None
}

/// The names of every tool, in table order.
pub fn tool_names() -> Vec<&'static str> {
    tools().into_iter().map(|t| t.0).collect()
}

/// Every tool with the engine action it calls: `(tool, action, writes)`.
pub fn tool_actions() -> Vec<(&'static str, &'static str, bool)> {
    tools().into_iter().map(|t| (t.0, t.1, t.4)).collect()
}

/// The profile an MCP server runs with, by precedence: the CLI flag
/// (`onto mcp --profile`), else `ONTO_MCP_PROFILE`, else `[mcp] profile`
/// (`full` by default). Empty values are skipped. The name is not checked
/// here: [`McpServer::with_profile`] refuses an unknown one.
pub fn resolve_profile(cli: Option<&str>, configured: &str) -> String {
    choose_profile(cli, std::env::var("ONTO_MCP_PROFILE").ok().as_deref(), configured)
}

/// [`resolve_profile`] with the environment value given: the first
/// non-empty of `cli`, `env`, `configured`, else `full`.
pub fn choose_profile(cli: Option<&str>, env: Option<&str>, configured: &str) -> String {
    [cli, env, Some(configured)]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or("full")
        .to_string()
}

/// `Ok` for a known profile name, else the error that lists the valid ones.
pub fn check_profile(profile: &str) -> onto_core::Result<()> {
    if MCP_PROFILES.contains(&profile) {
        return Ok(());
    }
    Err(onto_core::Error::Invalid(format!("unknown MCP profile '{profile}' (valid: {})", MCP_PROFILES.join(", "))))
}

impl McpServer {
    /// A server offering every tool (profile `full`); see [`Self::with_profile`].
    pub fn new(intelli: Arc<Intelli>, principal: Principal, allow_writes: bool) -> Self {
        Self { intelli, principal, allow_writes, ctx: TraceCtx::from_source("mcp"), profile: "full" }
    }

    /// Offer only the tools of `profile` (one of [`MCP_PROFILES`]) plus
    /// [`FLOOR_TOOLS`]. Write tools still need `allow_writes`. An unknown
    /// name is refused with an error that lists the valid ones. Callers pass
    /// [`resolve_profile`]`(cli_flag, &cfg.mcp.profile)`, so the precedence
    /// is CLI flag, then `ONTO_MCP_PROFILE`, then `[mcp] profile`.
    pub fn with_profile(mut self, profile: &str) -> onto_core::Result<Self> {
        let profile = profile.trim();
        check_profile(profile)?;
        self.profile = MCP_PROFILES.iter().copied().find(|p| *p == profile).unwrap_or("full");
        Ok(self)
    }

    /// The active tool profile.
    pub fn profile(&self) -> &str {
        self.profile
    }

    /// Tools this server lists: in the profile, and writing ones only with
    /// `allow_writes`.
    fn offered(&self, name: &str, writes: bool) -> bool {
        profile_offers(self.profile, name) && (self.allow_writes || !writes)
    }

    /// The trace context of the calls this server makes (source `mcp` by
    /// default; the HTTP transport adds the session id, `traceparent` and
    /// the API key's prefix).
    pub fn with_trace_ctx(mut self, ctx: TraceCtx) -> Self {
        self.ctx = ctx;
        self
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
                let all = tools();
                let withheld = all.iter().filter(|t| !profile_offers(self.profile, t.0)).count();
                // Write tools the profile offers but allow_writes hides, so
                // offered + withheld + hidden is every tool.
                let hidden =
                    all.iter().filter(|t| profile_offers(self.profile, t.0) && !self.offered(t.0, t.4)).count();
                let profile = format!(
                    " Tool profile: {} ({} of {} tools offered, {withheld} withheld by the profile{}; \
                     set [mcp] profile, ONTO_MCP_PROFILE or onto mcp --profile to change it).",
                    self.profile,
                    all.iter().filter(|t| self.offered(t.0, t.4)).count(),
                    all.len(),
                    if hidden > 0 {
                        format!(
                            ", {hidden} write tool{} hidden by allow_writes = false",
                            if hidden == 1 { "" } else { "s" }
                        )
                    } else {
                        String::new()
                    },
                );
                Self::reply(
                    &id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": "onto-intelli", "version": crate::VERSION },
                        "instructions": format!("Ontology of the workspace: teams, people, projects, issues, tasks, services, knowledge. \
                            Prefer `ask` for questions; use graph tools for precise lookups; cite entity ids. \
                            Writes only through plan_action/apply_action (governed, reversible). \
                            Derived facts are not asserted facts, and a clean reasoner or shape run is not a consistency result.{profile}"),
                    }),
                )
            }
            "ping" => Self::reply(&id, json!({})),
            "tools/list" => {
                let list: Vec<Value> = tools()
                    .into_iter()
                    .filter(|t| self.offered(t.0, t.4))
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
                if !profile_offers(self.profile, name) {
                    return Some(Self::error(
                        &id,
                        -32602,
                        &format!("tool {name} is not offered under the MCP profile {}", self.profile),
                    ));
                }
                if name == "what_if"
                    && let Some(why) = scenario_refusal(self.profile, &args)
                {
                    return Some(Self::error(&id, -32602, &why));
                }
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
                // Likewise a pack: always a proposal, never sample data (a
                // sample needs the pack's schema in force, which a proposal
                // is not).
                if name == "install_pack" {
                    let Some(m) = args.as_object_mut() else {
                        return Some(Self::error(&id, -32602, "install_pack takes an object of arguments"));
                    };
                    m.remove("sample");
                    m.insert("mode".into(), json!("propose"));
                }
                match self.intelli.call_traced(&self.principal, action, args, self.ctx.clone()).await.result {
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
