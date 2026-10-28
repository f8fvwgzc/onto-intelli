//! Named actions: JSON in, JSON out. One dispatch table serves the gRPC
//! services (built-in and user protos), HTTP JSON, the CLI and the C FFI.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use serde_json::Map;

use onto_core::action_types::ApplyOptions;
use onto_core::deps::DepDirection;
use onto_core::metrics::MetricQuery;
use onto_core::query::{FindSpec, TraverseSpec};
use onto_core::{
    ActionContext, EntityInput, EntityPatch, EntityRef, Error, EventFilter, EventInput, Id, LinkInput, Mutation,
    Principal, Result, one_or_many,
};
use onto_core::{Decision, Migration, Ontology, ProposalInput, ProposalStatus, SchemaChangeSet, SearchMode};

use crate::assurance;
use crate::automate;
use crate::changes;
use crate::ingest::normalize_record;
use crate::intelli::{AskRequest, Intelli};
use crate::memory::{MemoryScope, RecallOptions, RememberInput};
use crate::trace::{self, TraceCtx, Traced};
use crate::webhooks;
use crate::writeback;
use onto_core::trace::{MetricsQuery, ScoreRow, SessionQuery, TraceDetail, TraceQuery};

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ActionInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub write: bool,
}

const fn a(name: &'static str, write: bool, description: &'static str) -> ActionInfo {
    ActionInfo { name, description, write }
}

/// Actions that change entities and links directly, without an action
/// type's checks: refused to agents unless `governance.agent_raw_writes`
/// (`ingest.sheet` with `dry_run` writes nothing and stays allowed).
pub const RAW_WRITES: &[&str] = &[
    "entity.upsert",
    "entity.patch",
    "entity.delete",
    "entity.merge",
    "link.create",
    "link.delete",
    "graph.apply",
    "ingest.batch",
    "ingest.sheet",
];

/// The write actions agents may call, because each governs itself: action
/// types (plan, risk, confirmation by a person), proposals instead of
/// direct schema changes, their own memory and trace scores. Every other
/// write action of [`ACTIONS`] is refused to agents, so a write action added
/// later is closed to them until it is listed here.
pub const AGENT_WRITES: &[&str] = &[
    "action.apply",
    "action.undo",
    "memory.remember",
    "memory.forget",
    "memory.consolidate",
    "event.append",
    "trace.score",
    "schema.apply",
    "schema.undo",
    "schema.revert",
    "schema.propose",
    "schema.revise",
    "schema.comment",
    "schema.withdraw",
    "schema.approve",
    "schema.reject",
    "schema.lint_feedback",
    "schema.lock",
    "schema.unlock",
    "pack.install",
    "ontology.apply_draft",
    "connector.crawl",
];

/// May agent `p` run write action `action` with `input`?
fn agent_may_write(p: &Principal, action: &str, input: &Value, raw_writes: bool) -> Result<()> {
    let write = ACTIONS.iter().any(|a| a.name == action && a.write);
    if !p.is_agent() || !write || AGENT_WRITES.contains(&action) {
        return Ok(());
    }
    if action == "ingest.sheet" && input.get("dry_run").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    if raw_writes && RAW_WRITES.contains(&action) {
        return Ok(());
    }
    let why = if RAW_WRITES.contains(&action) {
        "writes the graph directly, which agents may not (use an action type, action.apply; or set governance.agent_raw_writes)"
    } else {
        "is not open to agents"
    };
    Err(Error::Denied(format!("{} is an agent: {action} {why}", p.id)))
}

pub const ACTIONS: &[ActionInfo] = &[
    a("entity.upsert", true, "Create or update an entity (merge props). Input: EntityInput."),
    a(
        "entity.patch",
        true,
        "Change title/state/props of an existing entity. Input: {id, title?, state?, props?, unset?}.",
    ),
    a("entity.get", false, "Fetch one entity. Input: {id}."),
    a("entity.delete", true, "Delete an entity and its links. Input: {id}."),
    a("entity.find", false, "Filter entities by kind, state, open, props, related, within. Input: FindSpec."),
    a("entity.search", false, "Full-text (BM25) search. Input: {query, kinds?, workspace?, limit?}."),
    a("link.create", true, "Link two entities (inverse names allowed). Input: {from, rel, to, props?}."),
    a("link.delete", true, "Remove a link. Input: {from, rel, to}."),
    a("graph.apply", true, "Apply a batch of mutations atomically. Input: {mutations: [...], workspace?}."),
    a("graph.neighbors", false, "Direct neighbours. Input: {id, rel?}."),
    a("graph.traverse", false, "Breadth-first subgraph. Input: {start, rels?, direction?, depth?, limit?, kinds?}."),
    a("graph.path", false, "Shortest connection between two entities. Input: {from, to, depth?}."),
    a("graph.infer", false, "Relations derived by schema rules. Input: {id}."),
    a("graph.dependencies", false, "Upstream (default) or downstream dependencies. Input: {id, direction?, depth?}."),
    a("graph.impact", false, "What is affected if an entity fails. Input: {id, depth?}."),
    a("graph.root_cause", false, "Chains from an entity to things in alert states. Input: {id, depth?}."),
    a("graph.cycles", false, "Dependency cycles. Input: {workspace?}."),
    a("graph.critical_path", false, "Execution order and critical path of open work under an entity. Input: {id}."),
    a(
        "graph.overview",
        false,
        "The most connected entities of a workspace and the links among them, with per-kind counts. Input: {workspace?, kinds?, limit?}.",
    ),
    a(
        "graph.snapshot",
        false,
        "The graph around some entities as it was at a time (entity and link history). Input: {start, ts, depth?, limit?}.",
    ),
    a(
        "context.build",
        false,
        "Question context graph and packed facts, no LLM call. Input: {question, workspace?, token_budget?}.",
    ),
    a(
        "intelli.ask",
        false,
        "Answer a question (deterministic first, LLM when reasoning is needed). Input: AskRequest.",
    ),
    a(
        "memory.remember",
        true,
        "Store a long-term fact or preference. A repeat merges into the existing memory; a changed fact about the same entities supersedes the old one. Input: {text, entities?, kind?, private?, supersedes? (id to replace), supersede? (false = never supersede automatically), valid_from? (ms)}.",
    ),
    a(
        "memory.recall",
        false,
        "Recall current memories, without near-duplicates. Input: {query, entities?, k?, as_of? (ms: what was true then), budget? (tokens), include_superseded?}.",
    ),
    a("memory.list", false, "List memories, newest first. Input: {scope?, limit?, include_superseded?}."),
    a("memory.forget", true, "Delete a memory; deleting a replacement reopens the memory it superseded. Input: {id}."),
    a(
        "memory.consolidate",
        true,
        "Persist access statistics, expire old items, promote recurring topics to long-term memory, prune faded episodes.",
    ),
    a(
        "memory.stats",
        false,
        "Memory counts and efficiency counters (merged, superseded, pruned, evicted, reinforced, recall tokens).",
    ),
    a("event.append", true, "Record an event (audit/activity). Input: EventInput."),
    a("event.list", false, "Query events. Input: {workspace?, actor?, target?, action?, since?, until?, limit?}."),
    a("ingest.batch", true, "Ingest records (mutations, entities, GitHub webhooks). Input: {records: [...], async?}."),
    a("ingest.flush", false, "Wait until queued ingestion is applied."),
    a("conversation.get", false, "Fetch a conversation with its turns. Input: {id}."),
    a("action.list", false, "Action types (governed edits) defined in the schema."),
    a("action.describe", false, "One action type: parameters, checks, effects, risk. Input: {name}."),
    a(
        "action.plan",
        false,
        "Validate an action and preview its edits and reversibility; consequences=true adds the derived facts it changes. Input: {action, params, consequences?}.",
    ),
    a(
        "action.apply",
        true,
        "Apply an action type (high-risk/irreversible ones need confirm=true). Input: {action, params, confirm?, dry_run?}.",
    ),
    a("action.undo", true, "Undo an applied action from its receipt. Input: {receipt, force?}."),
    a("action.receipt", false, "Fetch an action receipt. Input: {id}."),
    a("entity.derived", false, "Computed properties of an entity. Input: {id}."),
    a("entity.history", false, "Version history with changed fields. Input: {id, limit?}."),
    a("entity.as_of", false, "An entity as it was at a time. Input: {id, ts? | days_ago?}."),
    a("graph.changes", false, "What changed in a window. Input: {since_days? | since?, within?, kinds?, limit?}."),
    a("metric.list", false, "Governed metrics defined in the schema."),
    a(
        "metric.query",
        false,
        "Evaluate a metric, named or ad hoc. Input: {metric? | kind, measure, filter, group_by, within, window_days}.",
    ),
    a("entity.duplicates", false, "Likely duplicate entities. Input: {kind?, threshold?}."),
    a("entity.merge", true, "Merge a duplicate into the entity to keep; the old id redirects. Input: {keep, merge}."),
    a("quality.report", false, "Data-quality report with scores. Input: {kind?, limit?}."),
    a(
        "scenario.run",
        false,
        "What-if: apply mutations to a fork and compare read queries. Input: {mutations, queries: [{action, input}]}.",
    ),
    a("people.experts", false, "Who knows about a topic. Input: {topic, entities?, limit?}."),
    a("schema.profile", false, "Populated patterns, kinds, properties and states."),
    a("schema.suggest", false, "Schema additions proposed from data the schema does not describe."),
    a("export.graph", false, "Export as Mermaid, Turtle (RDF/OWL + PROV-O) or JSON. Input: {format, start?, depth?}."),
    a("eval.run", false, "Run competency questions and report pass rate, tokens, latency. Input: {cases? | toml?}."),
    a("schema.get", false, "The active ontology schema document (or one version's: {version?})."),
    a("schema.status", false, "Live schema status: version, mode, counts, governance, import problems."),
    a(
        "schema.plan",
        false,
        "Dry run of a schema change set: validation, diff, impact (entities/links, breaking), migration, enabled automations it would stop (lint). Input: SchemaChangeSet.",
    ),
    a(
        "schema.apply",
        true,
        "Apply a schema change set as a new version (apply roles only; agents must propose). Input: SchemaChangeSet + dry_run?.",
    ),
    a("schema.history", false, "Schema versions, newest first. Input: {limit?}."),
    a("schema.version", false, "One schema version with its document, changes and diff. Input: {version}."),
    a("schema.as_of", false, "The schema version in force at a time. Input: {ts? | days_ago?}."),
    a("schema.diff", false, "Differences between two schema versions. Input: {from, to?}."),
    a("schema.revert", true, "Make an earlier version's schema active again. Input: {version, migrations?, dry_run?}."),
    a(
        "schema.undo",
        true,
        "Undo one version's changes on top of the active schema. Input: {version, migrations?, dry_run?}.",
    ),
    a(
        "schema.propose",
        true,
        "File a schema change proposal (agents too). Input: {title, rationale, changes, migrations?, message?, source?}.",
    ),
    a("schema.proposals", false, "Schema change proposals, newest first. Input: {status?, limit?}."),
    a("schema.proposal", false, "One proposal with its diff, impact and a fresh preview when stale. Input: {id}."),
    a(
        "schema.approve",
        true,
        "Approve a proposal (approve roles, not agents, not the proposer); the last needed approval applies it atomically. Input: {id, comment?, seen_version?}.",
    ),
    a("schema.reject", true, "Reject a proposal (approve roles). Input: {id, comment?}."),
    a("schema.comment", true, "Comment on a proposal. Input: {id, comment}."),
    a("schema.withdraw", true, "Withdraw an open or failed proposal (its proposer or an approver). Input: {id}."),
    a(
        "schema.revise",
        true,
        "Replace an open or failed proposal's change set; approvals reset. Input: {id, title?, rationale?, changes, migrations?}.",
    ),
    a("execution.graphs", false, "nexc execution graphs (needs builtin:nexc-execution). Input: {workspace?, limit?}."),
    a("execution.vocabulary", false, "A graph's node types and relation types in nexc's shape. Input: {graph}."),
    a(
        "execution.set_vocabulary",
        true,
        "Replace a graph's node types and relation types (created, updated, removed atomically). Input: {graph, node_types, relation_types}.",
    ),
    a(
        "execution.init_graph",
        true,
        "Create an execution graph, with the starter vocabulary unless starter=false or a vocabulary is given. Input: {graph, title?, starter?, node_types?, relation_types?}.",
    ),
    a(
        "connector.add",
        true,
        "Register a data source to crawl (metadata only, never rows); admins only. Input: {name, kind?=postgres|repo, url (or env:VAR; repo: a directory path or git URL), workspace?, schemas?, exclude_schemas?, every_secs?, map_grants?, grant_map?, owner_map?, max_queries?, repo?: {branch?, include?, exclude?, owners?}}.",
    ),
    a("connector.list", false, "Registered connectors with their last crawl (URLs redacted)."),
    a("connector.remove", true, "Remove a connector; purge=true also deletes what it crawled. Input: {name, purge?}."),
    a(
        "connector.crawl",
        true,
        "Crawl a connector (or all) now: incremental, through the audited write path, then learn and rank. Input: {name?}.",
    ),
    a(
        "catalog.find",
        false,
        "Which tables or views hold something, ranked by OntoRank. Input: {term, workspace?, limit?}.",
    ),
    a("catalog.join", false, "How two tables join: certified or learned paths, else foreign keys. Input: {from, to}."),
    a("catalog.metric", false, "Metric definitions for a term, certified first. Input: {term, workspace?}."),
    a("catalog.rank", false, "OntoRank of one entity with every component and its evidence. Input: {id}."),
    a("catalog.top", false, "The highest-authority entities. Input: {kinds?, workspace?, limit?}."),
    a("catalog.rerank", true, "Recompute OntoRank. Input: {kinds?, weights?}."),
    a(
        "learned.list",
        false,
        "Learned facts (join paths, metrics, synonyms), certified first. Input: {fact_type?, state?, about?, limit?}.",
    ),
    a("learned.run", true, "Infer facts from the catalog again. Input: {workspace?}."),
    a("learned.certify", true, "Certify a learned fact (approvers and admins, never agents). Input: {id, note?}."),
    a("learned.reject", true, "Reject a learned fact (approvers and admins, never agents). Input: {id, note?}."),
    a(
        "ontology.draft",
        false,
        "Draft an ontology from a crawled data source: object types from tables (typed properties, keys, titles, states), link types from foreign keys, join tables and learned joins, action types; each with its reason. No LLM. Input: {source?, tables?, exclude?, prefix?, sync?, actions?, views?}.",
    ),
    a(
        "ontology.refine",
        false,
        "The draft improved by the LLM: renames, labels, descriptions, property types, inverses, transitions; each suggestion is kept only if the draft stays a valid schema change. Without an LLM: the draft and a note. Input: draft options + instructions?.",
    ),
    a(
        "ontology.advise",
        false,
        "Modeling advice grounded in the draft: which tables become which object types, links, actions and property types, and why. Input: {question, source?}.",
    ),
    a(
        "ontology.apply_draft",
        true,
        "Apply a draft as a new schema version (apply roles; agents are refused and must propose) or file it as a proposal; applying with sync crawls the source. Input: draft options or {draft}, mode? (apply|propose), title?, rationale?, no_crawl?; preview=true returns the draft with its change set rebuilt and applies nothing.",
    ),
    a(
        "objects.types",
        false,
        "Object types with how many objects the caller can see, their backing table, properties, links and actions. Input: {workspace?}.",
    ),
    a(
        "objects.explore",
        false,
        "Object Explorer: objects of one type filtered, searched, sorted and paged, with facet counts, numeric statistics, link and action columns. Input: {kind, filters?: [{prop, op, value}], search?, sort?, limit?, offset?, facets?, stats?, workspace?}.",
    ),
    a(
        "reason.run",
        false,
        "Run the reasoner over what the caller may read: counts per rule, sample derivations, the constructs it did not evaluate (a clean run is not a consistency result). Derived facts are never written. Input: {workspace?, max_derived?, samples?}.",
    ),
    a(
        "reason.query",
        false,
        "Match triple patterns over asserted and derived facts. Input: {pattern: [[s, p, o], ...] (at most 8 atoms), workspace?, limit? (at most 1000)}.",
    ),
    a(
        "reason.explain",
        false,
        "Minimal sets of asserted facts a fact is derived from, each re-derived independently. Input: {fact: [s, p, o] | {subject, relation, object}, workspace?, max?}.",
    ),
    a(
        "reason.provenance",
        false,
        "A fact's support in a semiring: why (witness sets), trust (confidence) or count (derivation trees). Input: {fact, semiring?: why|trust|count, workspace?}.",
    ),
    a(
        "reason.certify",
        false,
        "A reasoning certificate (rules.tsv, asserted.tsv, derivations.tsv, manifest.json) and the independent onto-cert checker's report on it, with the closure's fixpoint_reached (false: the derivations are a lower bound). Input: {workspace?}.",
    ),
    a(
        "reason.check_certificate",
        false,
        "Check certificate files with the independent onto-cert checker (no graph access). Input: {rules_tsv, asserted_tsv, derivations_tsv, manifest_json?}.",
    ),
    a(
        "reason.binding",
        false,
        "Is a certificate's asserted.tsv what the store yields now for the caller? Input: {asserted_tsv, manifest_json?}.",
    ),
    a(
        "reason.verify",
        false,
        "Verdicts on claims (asserted, derived, not_entailed, contradicted, outside_vocabulary) with the derivation behind each. Input: {claims: [{subject, relation, object, rule?, premises?}], workspace?}.",
    ),
    a(
        "reason.slice_check",
        false,
        "Does a slice of entities keep what the whole visible graph derives (or the given goals)? Input: {entities: [...], goals?: [...], workspace?}.",
    ),
    a(
        "shapes.validate",
        false,
        "Validate the schema's shapes over the graph: conforms, violates or undetermined, with every result. Input: {shapes?: [name], workspace?, inferred?, limit?}.",
    ),
    a(
        "shapes.induce",
        false,
        "A shape for one object type read from its data, with the evidence and a put_shape change set. Input: {kind, workspace?, name?}.",
    ),
    a(
        "shapes.check",
        false,
        "Structural dry run of shape definitions against the schema (names, keys, patterns, bounds). Input: {shapes: {name: Shape}}.",
    ),
    a(
        "schema.lint",
        false,
        "Schema lint findings with fixes; dismissed ones are counted. Input: {checks?: [..], include_dismissed?}.",
    ),
    a(
        "schema.lint_feedback",
        true,
        "Accept or dismiss a lint finding (write access; the latest decision wins). Input: {id, decision: accept|dismiss, note?, workspace?}.",
    ),
    a("schema.locks", false, "Locked schema definitions."),
    a(
        "schema.lock",
        true,
        "Lock a schema definition against delete, rename and breaking change (admins only, never agents). Input: {section, name, reason}.",
    ),
    a("schema.unlock", true, "Release a schema lock (admins only). Input: {section, name}."),
    a(
        "schema.align",
        false,
        "Align a pack or a TOML schema onto the schema in force: scored matches (opinions), SSSOM and a glossary change set. Input: {pack? | toml?, threshold?}.",
    ),
    a(
        "schema.align_certify",
        false,
        "Certify claimed mappings against both hierarchies: upheld, upheld with gaps, downgraded or uncertified. Input: {pack? | toml?, mappings: [{source, target, predicate}]}.",
    ),
    a("pack.list", false, "Industry packs with counts and whether each is installed."),
    a(
        "pack.show",
        false,
        "One pack's types, rules, shapes, metrics and actions, with the lint findings it would add to the current schema. Input: {id}.",
    ),
    a(
        "pack.plan",
        false,
        "The schema plan of installing a pack (consequences, risk, locks, shape delta). Input: {id}.",
    ),
    a(
        "pack.install",
        true,
        "Install a pack (apply roles; agents always propose) and optionally load its sample data. Input: {id, mode?: apply|propose, sample?, workspace?, message?}.",
    ),
    a(
        "model.induce",
        false,
        "One sheet in, one ontology out: an object type, links and a shape induced from CSV text (nothing written). Input: {csv, kind?, key?, workspace?, file?}.",
    ),
    a(
        "ingest.sheet",
        true,
        "Write a sheet's rows as objects with a mapping; when absent, the rows go into the existing object type the sheet names (kind, else induced), column by column, and a type the schema lacks must be applied from model.induce first. dry_run validates only. Input: {csv, mapping?, kind?, key?, workspace?, dry_run?}.",
    ),
    a(
        "changes.poll",
        false,
        "One page of the change feed after a cursor: records the caller may see that the filter wants (gap records always pass), next (the last cursor scanned), head, floor, and expired with a message when the cursor is older than the floor. Delivery is at least once; exactly once is not offered. Input: {after? | from?: earliest (default) | now, limit? (at most 1000), kinds?, ops?, workspaces?, ids?, rels?, props?, sources?, fields?: full | ids, include_migrations? | exclude_migrations?}.",
    ),
    a("changes.head", false, "The change feed's position: {cursor, head, floor, capture}."),
    a(
        "changes.capture",
        true,
        "Turn change recording on or off for the tenant (admins of every workspace, never agents); turning it on after a period off expires every older cursor, since nothing was recorded meanwhile. Input: {on | off, workspace?}.",
    ),
    a(
        "webhook.list",
        false,
        "Webhook endpoints by name: URL (its origin only for endpoints the caller may not manage), secret reference (never the secret), principal, change filter, status (active, paused, disabled), version, and pending rows and cursor for endpoints the caller manages; plus the tenant kill switch. Admins, never agents.",
    ),
    a(
        "webhook.add",
        true,
        "Register a webhook endpoint: signed CloudEvents (Standard Webhooks) to an https URL, as the endpoint's principal sees the data. With a filter, every change it wants from now on is enqueued once and delivered at least once; dedupe on the event id. No secret: a whsec_ secret is generated, sealed and returned once. Admins who dominate the principal (default viewer in the call's workspace), never agents. Input: {name, url, secret?, filter?: {kinds?, ops?, workspaces?, ids?, rels?, props?, sources?, fields?, exclude_migrations?}, principal?: {roles, workspaces}, disabled?, workspace?}.",
    ),
    a(
        "webhook.update",
        true,
        "Change a webhook endpoint (its version moves): url, secret (replaced, no overlap; see webhook.rotate_secret), filter or clear_filter, principal, enabled or disabled. Admins who dominate the old and new principal. Input: {name, url?, secret?, filter?, clear_filter?, principal?, enabled? | disabled?, workspace?}.",
    ),
    a(
        "webhook.remove",
        true,
        "Remove a webhook endpoint; rows not yet sent to it are cancelled. Admins who dominate its principal. Input: {name, workspace?}.",
    ),
    a(
        "webhook.test",
        true,
        "Enqueue a test delivery (onto.webhook.test) to an endpoint. Admins who dominate its principal. Input: {name, workspace?}.",
    ),
    a(
        "webhook.rotate_secret",
        true,
        "Rotate an endpoint's signing secret: the new one (given, or generated and returned once) signs from now, the previous one also signs for keep_secs (default a day). Input: {name, secret?, keep_secs?, workspace?}.",
    ),
    a(
        "webhook.deliveries",
        false,
        "An endpoint's deliveries, newest first, or one delivery with its attempts (status, error, the start of the receiver's response). Admins who dominate its principal. Input: {name, id?, states?, since?, limit?}.",
    ),
    a(
        "webhook.replay",
        true,
        "Send an endpoint's deliveries again (by ids, or created since a time; retry, delivered, dead and cancelled by default). They keep their ids, so receivers that dedupe see repeats. Input: {name, ids? | since?, states?, workspace?}.",
    ),
    a(
        "webhook.resume",
        true,
        "Resume a paused endpoint: clears a pause, an open breaker and a backlog pause (fan-out pauses it again while it is over its cap). Input: {name, workspace?}.",
    ),
    a(
        "webhook.kill",
        true,
        "The tenant's webhook kill switch: on holds every delivery in the outbox (change subscriptions keep their cursors); off resumes them (admins who dominate every endpoint). Input: {on | off, workspace?}.",
    ),
    a(
        "outbox.list",
        false,
        "Outbound deliveries waiting, sent or given up, newest first, for targets the caller may manage. Input: {target?, states?, source?, receipt?, run?, since?, limit?}.",
    ),
    a(
        "automation.list",
        false,
        "Automations by name: description, mode, run_as, trigger, effects, status (active, or inactive with the reason: not_enabled, paused, stale_definition, enablement_expired, killed), who enabled it and when it expires, and whether the caller may enable it; plus the tenant kill switch. Definitions are visible to everyone who can read the schema.",
    ),
    a(
        "automation.get",
        false,
        "One automation: its definition, status, enablement and closure (the items its enablement binds to, and those that changed since). Input: {name}.",
    ),
    a(
        "automation.runs",
        false,
        "Automation runs, newest first (ties by id): state (running, succeeded, failed, retry, skipped, refused, would_run, cancelled), reason, trigger, effects with receipts and outbox ids; runs of automations the caller does not dominate are counted as hidden. Input: {name?, state?, since?, limit?} or {id}.",
    ),
    a(
        "automation.test",
        false,
        "A dry run of an automation against a change (cursor) or an entity as if it was just updated (id), rendered and planned as the run principal and the caller together: whether the trigger admits it, the conditions, the plan and the webhook payloads. Writes nothing. Input: {name, cursor? | id?}.",
    ),
    a(
        "automation.enable",
        true,
        "Enable an automation (approvers and admins who dominate its run_as, never agents; every endpoint it reaches must dominate run_as too). It runs on changes from now on, binds to its closure hash (any change to what it reaches stops it) and expires after [automation] enable_ttl_days. Webhook effects are enqueued once and delivered at least once. Input: {name, workspace?}.",
    ),
    a(
        "automation.disable",
        true,
        "Stop an automation (admins, or whoever may enable it); its pending retries are cancelled. Input: {name, workspace?}.",
    ),
    a(
        "automation.rerun",
        true,
        "Run a skipped or failed run again (as enable); it is evaluated and planned on the graph at its commit. Input: {id, workspace?}.",
    ),
    a(
        "automation.kill",
        true,
        "The tenant's automation kill switch: on stops every automation (changes made meanwhile never start runs; pending retries are cancelled); off resumes them (admins who may enable every enabled automation). Input: {on | off, workspace?}.",
    ),
    a(
        "writeback.add",
        true,
        "Register an HTTP writeback target for action types' writeback effects: a base https URL (or an operator env:/file: reference), named routes (relative paths under it), the workspaces whose actions may use it (default the call's) and an optional signing secret (Standard Webhooks headers; plaintext is sealed, generate_secret returns one once). Writeback is not reversible; action.undo does not undo the external write. Each call is enqueued once and delivered at least once with Idempotency-Key: <receipt>:<effect>, which the receiver must honour. Admins of every workspace it serves, never agents. Input: {name, base_url, routes: {name: path}, workspaces?, secret?, generate_secret?, workspace?}.",
    ),
    a(
        "writeback.list",
        false,
        "Writeback targets by name: base URL (its origin only, and route names only, for targets the caller may not manage), routes, workspaces, secret reference (never the secret), version; plus whether [writeback] enabled is on. Admins, never agents.",
    ),
    a(
        "writeback.remove",
        true,
        "Remove a writeback target: rows not yet sent to it are cancelled, plans that name it are refused and automations that reach it stop. Admins of every workspace it serves. Input: {name, workspace?}.",
    ),
    a(
        "writeback.test",
        false,
        "Check a writeback target without sending anything (a request could write): its references resolve, and its base URL and every route (or one) pass the outbound rules. Admins of every workspace it serves. Input: {name, route?}.",
    ),
    a(
        "system.stats",
        false,
        "Graph, memory, cache, token and ingestion statistics; outbox rows per target and state, change lag per consumer and endpoint, gap records over 24 h and runs per state over 1 h.",
    ),
    a(
        "system.health",
        false,
        "Liveness and version, the webhook and automation switches and paused counts, change capture and the latest gap record.",
    ),
    a("system.whoami", false, "The caller's principal and what it may do (write, apply, approve, propose schema)."),
    a(
        "trace.list",
        false,
        "Traces, newest first, without bodies (observers). Input: {since?, until?, name?, kind?, status?, user_id?, session_id?, model?, workspace?, mode?, tag?, min_ms?, q?, limit?, cursor?}.",
    ),
    a("trace.get", false, "One trace with its spans and scores (observers). Input: {id}."),
    a(
        "trace.metrics",
        false,
        "Requests, errors, latency percentiles, tokens, cost, by bucket, model, name and score (observers). Input: {since?, until?, bucket?, name?, workspace?}.",
    ),
    a(
        "trace.sessions",
        false,
        "Sessions, the most recently active first (observers). Input: {since?, until?, user_id?, limit?, cursor?}.",
    ),
    a(
        "trace.session",
        false,
        "A session's traces, oldest first, with inputs and outputs (observers). Input: {id, limit?}.",
    ),
    a(
        "trace.score",
        true,
        "Score a trace (observers who may write, or the trace's own principal). Input: {trace_id, span_id?, name, value? | label?, comment?}.",
    ),
    a("system.actions", false, "This list."),
];

#[derive(Deserialize)]
struct IdIn {
    id: String,
}

#[derive(Deserialize)]
struct SearchIn {
    #[serde(default)]
    mode: Option<SearchMode>,
    #[serde(default)]
    personalize: Option<bool>,
    query: String,
    #[serde(default, deserialize_with = "one_or_many")]
    kinds: Vec<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct NeighborsIn {
    id: String,
    #[serde(default)]
    rel: Option<String>,
}

#[derive(Deserialize)]
struct PathIn {
    from: String,
    to: String,
    #[serde(default)]
    depth: Option<usize>,
}

#[derive(Deserialize)]
struct DepsIn {
    id: String,
    #[serde(default)]
    direction: Option<DepDirection>,
    #[serde(default)]
    depth: Option<usize>,
}

#[derive(Deserialize)]
struct RemoveConnectorIn {
    name: String,
    #[serde(default)]
    purge: bool,
}

#[derive(Deserialize, Default)]
struct CrawlIn {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct TermIn {
    #[serde(alias = "query", alias = "q")]
    term: String,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct JoinIn {
    from: String,
    to: String,
}

#[derive(Deserialize, Default)]
struct TopIn {
    #[serde(default, deserialize_with = "one_or_many")]
    kinds: Vec<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize, Default)]
struct RerankIn {
    #[serde(default, deserialize_with = "one_or_many")]
    kinds: Vec<String>,
    #[serde(default)]
    weights: Option<onto_core::authority::Weights>,
}

#[derive(Deserialize, Default)]
struct LearnedIn {
    #[serde(default, alias = "type")]
    fact_type: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    about: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct DecideFactIn {
    id: String,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize, Default)]
struct WsIn {
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct ContextIn {
    question: String,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    token_budget: Option<usize>,
}

#[derive(Deserialize)]
struct RecallIn {
    query: String,
    #[serde(default)]
    entities: Vec<Id>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    as_of: Option<i64>,
    #[serde(default)]
    budget: Option<usize>,
    #[serde(default)]
    include_superseded: Option<bool>,
}

#[derive(Deserialize, Default)]
struct ListMemIn {
    #[serde(default)]
    scope: Option<MemoryScope>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    include_superseded: Option<bool>,
}

#[derive(Deserialize)]
struct ApplyIn {
    mutations: Vec<Mutation>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize, Default)]
struct IngestIn {
    #[serde(default)]
    records: Vec<Value>,
    #[serde(default)]
    mutations: Vec<Value>,
    #[serde(default, rename = "async")]
    background: bool,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

pub(crate) fn parse<T: serde::de::DeserializeOwned>(action: &str, input: Value) -> Result<T> {
    let input = if input.is_null() { json!({}) } else { input };
    serde_json::from_value(input).map_err(|e| Error::invalid(format!("{action}: {e}")))
}

pub(crate) fn to_json<T: Serialize>(v: &T) -> Result<Value> {
    Ok(serde_json::to_value(v)?)
}

impl Intelli {
    pub(crate) fn ws_or_default(&self, ws: Option<String>) -> String {
        ws.filter(|w| !w.is_empty()).unwrap_or_else(|| self.default_workspace().to_string())
    }

    pub(crate) fn ctx(&self, p: &Principal, ws: Option<String>) -> ActionContext {
        let source = match &p.via {
            Some(service) => format!("api via {service}"),
            None => "api".to_string(),
        };
        ActionContext::new(p.clone(), self.ws_or_default(ws)).with_source(source)
    }

    /// Run a named action. See [`ACTIONS`]. Traced as `[traces] calls`
    /// selects (`intelli.ask` always), with `source = "sdk"`.
    pub async fn call(&self, p: &Principal, action: &str, input: Value) -> Result<Value> {
        self.call_traced(p, action, input, TraceCtx::default()).await.result
    }

    /// [`call`](Self::call) with what the caller knows about the trace.
    /// `intelli.ask` becomes an ask trace; `action.apply` and `action.undo`
    /// an `action` trace named `action.apply:<type>`; other calls a `call`
    /// trace when `[traces] calls` selects them (`writes`: writing and
    /// LLM-using actions). `eval.run` records one trace per case; the
    /// `trace.*` actions are never traced.
    pub async fn call_traced(&self, p: &Principal, action: &str, input: Value, ctx: TraceCtx) -> Traced<Value> {
        match action {
            "intelli.ask" => {
                let req: AskRequest = match parse(action, input) {
                    Ok(r) => r,
                    Err(e) => return Traced::untraced(Err(e)),
                };
                let t = Box::pin(self.ask_traced(p, req, ctx)).await;
                Traced { result: t.result.and_then(|r| to_json(&r)), trace_id: t.trace_id }
            }
            "eval.run" => {
                let run = async {
                    let i: EvalIn = parse(action, input)?;
                    let cases = match (i.cases.is_empty(), &i.toml) {
                        (false, _) => i.cases,
                        (true, Some(t)) => crate::eval::parse_suite(t)?,
                        (true, None) => crate::eval::parse_suite(crate::eval::DEMO_EVALS)?,
                    };
                    to_json(&self.eval_traced(p, cases, &ctx).await?)
                };
                Traced::untraced(run.await)
            }
            a if a.starts_with("trace.") || !self.traces_call(a) => {
                // Not recorded, but the caller's trace still continues in
                // what the call enqueues (change records, outbox rows).
                let p = p.clone().with_trace(ctx.otel_trace_id.as_deref());
                Traced::untraced(Box::pin(self.dispatch(&p, action, input)).await)
            }
            _ => self.call_recorded(p, action, input, ctx).await,
        }
    }

    /// Does `[traces] calls` make a trace of `action`?
    /// (Secret-bearing actions are traced without content: [`secret_bearing`].)
    fn traces_call(&self, action: &str) -> bool {
        match self.config().traces.calls.as_str() {
            "all" => true,
            "writes" => find(action).is_some_and(|a| a.write) || LLM_ACTIONS.contains(&action),
            _ => false,
        }
    }

    /// Run `action` inside a trace of its own: one `tool` span (the root),
    /// with the generation spans of any LLM call under it.
    async fn call_recorded(&self, p: &Principal, action: &str, input: Value, ctx: TraceCtx) -> Traced<Value> {
        let Some(tracer) = self.start_trace(&ctx) else {
            let p = p.clone().with_trace(ctx.otel_trace_id.as_deref());
            return Traced::untraced(Box::pin(self.dispatch(&p, action, input)).await);
        };
        // What the call enqueues continues the caller's trace, else this one
        // (docs/design/EVENTS.md B.7).
        let p = &p.clone().with_trace(Some(ctx.otel_trace_id.as_deref().unwrap_or(tracer.id())));
        let ws = self.ws_or_default(input.get("workspace").and_then(Value::as_str).map(str::to_string));
        let (kind, name) = match action {
            "action.apply" => {
                ("action", format!("action.apply:{}", input.get("action").and_then(Value::as_str).unwrap_or("?")))
            }
            "action.undo" => ("action", "action.undo".to_string()),
            other => ("call", other.to_string()),
        };
        let mut row = trace::base_row(&ctx, p, kind, &name, &ws);
        // Outbound configuration carries signing secrets (given, generated
        // and rotated ones are returned once) and receivers' URLs: its call
        // traces keep no input or output, whatever capture_content says.
        let capture = tracer.captures() && !secret_bearing(action);
        if capture {
            let mut m = Map::new();
            if let Some(q) = input.get("question").and_then(Value::as_str).and_then(|q| tracer.text(q)) {
                m.insert("question".into(), Value::String(q));
            }
            if let Some(text) = tracer.text(&input.to_string()) {
                m.insert("input".into(), Value::String(text));
            }
            row.input = Some(Value::Object(m));
        }
        let result = trace::scope(tracer.clone(), Box::pin(self.dispatch(p, action, input))).await;
        let mut attrs = json!({ "action": action });
        match &result {
            Ok(v) => {
                if kind == "action" {
                    let plan = &v["plan"];
                    attrs = json!({
                        "action": action,
                        "action_type": v["action"],
                        "risk": plan["risk"],
                        "receipt": v["id"],
                        "edits": plan["mutations"].as_array().map_or(0, Vec::len),
                        "outcome": v["status"],
                    });
                    if action == "action.undo"
                        && ctx.name.is_none()
                        && let Some(t) = v["action"].as_str()
                    {
                        row.name = format!("action.undo:{t}");
                    }
                }
                row.output = if capture { tracer.text(&v.to_string()).map(|o| json!({ "output": o })) } else { None };
                if let Some(e) = tracer.llm_error() {
                    trace::fail(&mut row, "llm_error", &e);
                }
            }
            Err(e) => trace::fail(&mut row, e.code(), &e.to_string()),
        }
        let id = tracer.id().to_string();
        self.send_trace(tracer.finish(row, action, "tool", attrs));
        Traced { result, trace_id: Some(id) }
    }

    /// The checks every call passes before it runs, for the action table
    /// and for surfaces that serve an action outside it (the change stream):
    /// agents run only the write actions open to them ([`AGENT_WRITES`],
    /// raw writes with `governance.agent_raw_writes`). The action's own arm
    /// checks roles, workspaces and governance.
    pub fn admit(&self, p: &Principal, action: &str, input: &Value) -> Result<()> {
        agent_may_write(p, action, input, self.ontology().options().governance.agent_raw_writes)
    }

    /// The action table itself.
    async fn dispatch(&self, p: &Principal, action: &str, input: Value) -> Result<Value> {
        let onto = self.ontology();
        self.admit(p, action, &input)?;
        if GRAPH_READS.contains(&action) || assurance::GRAPH_READS.contains(&action) {
            return blocking(|| graph_call(onto, p, self.default_workspace(), action, input));
        }
        if assurance::CALLS.contains(&action) {
            return blocking(|| self.assurance_call(p, action, input));
        }
        if changes::CALLS.contains(&action) {
            return blocking(|| self.changes_call(p, action, input));
        }
        if webhooks::CALLS.contains(&action) {
            return blocking(|| self.webhooks_call(p, action, input));
        }
        if automate::CALLS.contains(&action) {
            return blocking(|| self.automate_call(p, action, input));
        }
        if writeback::CALLS.contains(&action) {
            return blocking(|| self.writeback_call(p, action, input));
        }
        let ws_hint = input.get("workspace").and_then(Value::as_str).map(str::to_string);
        match action {
            "action.list" => Ok(json!({ "action_types": onto.schema().action_types.values().collect::<Vec<_>>() })),
            "action.describe" => {
                let i: NameIn = parse(action, input)?;
                let schema = onto.schema();
                let at = schema
                    .action_types
                    .get(&i.name)
                    .ok_or_else(|| Error::not_found(format!("action type {}", i.name)))?;
                to_json(at)
            }
            "action.plan" => {
                let i: ActionIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace);
                if i.consequences {
                    return to_json(&onto.plan_action_consequences(&ctx, &i.action, &i.params)?);
                }
                to_json(&onto.plan_action(&ctx, &i.action, &i.params)?)
            }
            "action.apply" => {
                let i: ActionIn = parse(action, input)?;
                let opts = ApplyOptions { dry_run: i.dry_run, confirm: i.confirm };
                to_json(&onto.apply_action(&self.ctx(p, i.workspace), &i.action, &i.params, opts)?)
            }
            "action.undo" => {
                let i: UndoIn = parse(action, input)?;
                to_json(&onto.undo_action(&self.ctx(p, i.workspace), &i.receipt, i.force)?)
            }
            "action.receipt" => {
                let i: IdIn = parse(action, input)?;
                to_json(&onto.action_receipt(p, &i.id)?)
            }
            "entity.history" => {
                let i: HistoryIn = parse(action, input)?;
                Ok(json!({ "id": onto.resolve_id(&i.id), "history": onto.history(p, &i.id, i.limit.unwrap_or(50))? }))
            }
            "entity.as_of" => {
                let i: AsOfIn = parse(action, input)?;
                let ts = i
                    .ts
                    .unwrap_or_else(|| onto_core::time::now_ms() - (i.days_ago.unwrap_or(0.0) * 86_400_000.0) as i64);
                Ok(
                    json!({ "id": i.id, "ts": ts, "at": onto_core::time::fmt_ts(ts), "entity": onto.as_of(p, &i.id, ts)? }),
                )
            }
            "graph.changes" => {
                let i: ChangesIn = parse(action, input)?;
                let since = i
                    .since
                    .unwrap_or_else(|| onto_core::time::now_ms() - (i.since_days.unwrap_or(7.0) * 86_400_000.0) as i64);
                let changes = onto.changes(
                    p,
                    since,
                    i.workspace.as_deref(),
                    &i.kinds,
                    i.within.as_deref(),
                    i.limit.unwrap_or(100),
                )?;
                Ok(json!({ "since": since, "changes": changes }))
            }
            "metric.list" => Ok(json!({ "metrics": onto.schema().metrics.values().collect::<Vec<_>>() })),
            "entity.duplicates" => {
                let i: DupesIn = parse(action, input)?;
                let pairs =
                    onto.duplicates(p, i.workspace.as_deref(), i.kind.as_deref(), i.threshold.unwrap_or(0.8))?;
                Ok(json!({ "pairs": pairs }))
            }
            "entity.merge" => {
                let i: MergeIn = parse(action, input)?;
                to_json(&onto.merge(&self.ctx(p, i.workspace), &i.keep, &i.merge)?)
            }
            "scenario.run" => {
                let i: ScenarioIn = parse(action, input)?;
                if i.queries.len() > MAX_SCENARIO_QUERIES {
                    return Err(Error::invalid(format!(
                        "a scenario runs at most {MAX_SCENARIO_QUERIES} queries (this one has {})",
                        i.queries.len()
                    )));
                }
                let ws = self.ws_or_default(i.workspace);
                let fork = onto.fork();
                let ctx = ActionContext::new(p.clone(), ws.clone()).with_source("scenario");
                let applied = fork.apply(&ctx, i.mutations)?;
                let mut results = Vec::new();
                for q in i.queries {
                    let before = graph_call(onto, p, &ws, &q.action, q.input.clone())?;
                    let after = graph_call(&fork, p, &ws, &q.action, q.input)?;
                    results.push(
                        json!({ "action": q.action, "changed": before != after, "before": before, "after": after }),
                    );
                }
                Ok(json!({ "applied": applied, "results": results }))
            }
            "people.experts" => {
                let i: ExpertsIn = parse(action, input)?;
                let ws = self.ws_or_default(i.workspace);
                Ok(json!({ "experts": onto.experts(p, Some(&ws), &i.topic, &i.entities, i.limit.unwrap_or(5))? }))
            }
            "schema.profile" => {
                let w: WsIn = parse(action, input)?;
                to_json(&onto.profile(p, w.workspace.as_deref())?)
            }
            "schema.suggest" => {
                let w: WsIn = parse(action, input)?;
                to_json(&onto.suggest_schema(p, w.workspace.as_deref())?)
            }
            "export.graph" => {
                let i: ExportIn = parse(action, input)?;
                let spec = (!i.start.is_empty()).then(|| TraverseSpec {
                    start: i.start.clone(),
                    rels: i.rels.clone(),
                    depth: i.depth.unwrap_or(2),
                    limit: i.limit.unwrap_or(200),
                    ..Default::default()
                });
                let content = match i.format.as_str() {
                    "mermaid" => {
                        let spec = spec.ok_or_else(|| Error::invalid("mermaid export needs `start`"))?;
                        onto.export_mermaid(p, &spec)?
                    }
                    "turtle" | "ttl" | "rdf" => onto.export_turtle(
                        p,
                        i.workspace.as_deref(),
                        i.include_schema.unwrap_or(true),
                        i.include_data.unwrap_or(true),
                        spec.as_ref(),
                    )?,
                    "json" => {
                        let spec = spec.ok_or_else(|| Error::invalid("json export needs `start`"))?;
                        serde_json::to_string_pretty(&onto.traverse(p, &spec)?)?
                    }
                    other => {
                        return Err(Error::invalid(format!("unknown export format '{other}' (mermaid, turtle, json)")));
                    }
                };
                Ok(json!({ "format": i.format, "content": content }))
            }
            "eval.run" => {
                let i: EvalIn = parse(action, input)?;
                let cases = match (i.cases.is_empty(), &i.toml) {
                    (false, _) => i.cases,
                    (true, Some(t)) => crate::eval::parse_suite(t)?,
                    (true, None) => crate::eval::parse_suite(crate::eval::DEMO_EVALS)?,
                };
                to_json(&self.eval(p, cases).await?)
            }
            "entity.upsert" => {
                let e: EntityInput = parse(action, input)?;
                to_json(&onto.upsert(&self.ctx(p, ws_hint), e)?)
            }
            "entity.patch" => {
                let patch: EntityPatch = parse(action, input)?;
                to_json(&onto.patch(&self.ctx(p, ws_hint), patch)?)
            }
            "entity.delete" => {
                let i: IdIn = parse(action, input)?;
                let r = onto.apply(&self.ctx(p, ws_hint), vec![Mutation::Delete(EntityRef { id: i.id.clone() })])?;
                Ok(json!({ "id": i.id, "deleted": true, "revision": r.revision }))
            }
            "link.create" => {
                let l: LinkInput = parse(action, input)?;
                let (base, dir) = onto.schema().resolve_rel_lenient(&l.rel);
                let (from, to) = if dir == onto_core::Dir::In {
                    (l.to.clone(), l.from.clone())
                } else {
                    (l.from.clone(), l.to.clone())
                };
                let r = onto.apply(&self.ctx(p, ws_hint), vec![Mutation::Link(l)])?;
                Ok(json!({ "from": from, "rel": base, "to": to, "revision": r.revision, "changed": r.links > 0 }))
            }
            "link.delete" => {
                let l: LinkInput = parse(action, input)?;
                let r = onto.apply(&self.ctx(p, ws_hint), vec![Mutation::Unlink(l)])?;
                Ok(json!({ "deleted": r.links > 0, "revision": r.revision }))
            }
            "graph.apply" => {
                let a: ApplyIn = parse(action, input)?;
                to_json(&onto.apply(&self.ctx(p, a.workspace), a.mutations)?)
            }
            "context.build" => {
                let c: ContextIn = parse(action, input)?;
                let ws = self.ws_or_default(c.workspace);
                to_json(&self.build_context(p, &ws, &c.question, c.token_budget)?)
            }
            "intelli.ask" => {
                let req: AskRequest = parse(action, input)?;
                to_json(&self.ask(p, req).await?)
            }
            "memory.remember" => {
                let r: RememberInput = parse(action, input)?;
                let item = self.memory().remember(p, self.default_workspace(), r)?;
                to_json(&item)
            }
            "memory.recall" => {
                let r: RecallIn = parse(action, input)?;
                let ws = self.ws_or_default(r.workspace);
                let opts = RecallOptions {
                    as_of: r.as_of,
                    budget_tokens: r.budget,
                    include_superseded: r.include_superseded.unwrap_or(false),
                    ..Default::default()
                };
                let k = r.k.unwrap_or(self.config().memory.recall_k);
                let hits = self.memory().recall_with(p, &ws, &r.query, &r.entities, k, &opts);
                Ok(json!({ "items": hits }))
            }
            "memory.list" => {
                let l: ListMemIn = parse(action, input)?;
                let ws = self.ws_or_default(l.workspace);
                let all = l.include_superseded.unwrap_or(false);
                Ok(json!({ "items": self.memory().list_with(p, &ws, l.scope, l.limit.unwrap_or(50), all) }))
            }
            "memory.stats" => to_json(&self.memory().stats()),
            "memory.forget" => {
                let i: IdIn = parse(action, input)?;
                Ok(json!({ "deleted": self.memory().forget(p, &i.id)? }))
            }
            "memory.consolidate" => {
                if !p.allows(onto_core::Access::Write) {
                    return Err(Error::Denied(format!("{} may not consolidate memory", p.id)));
                }
                to_json(&self.memory().consolidate()?)
            }
            "event.append" => {
                let e: EventInput = parse(action, input)?;
                to_json(&onto.record_event(&self.ctx(p, ws_hint), e)?)
            }
            "event.list" => {
                let f: EventFilter = parse(action, input)?;
                Ok(json!({ "events": onto.events(p, &f)? }))
            }
            "ingest.batch" => {
                let i: IngestIn = parse(action, input)?;
                let mut muts = Vec::new();
                for (n, rec) in i.records.into_iter().chain(i.mutations).enumerate() {
                    muts.extend(normalize_record(rec).map_err(|e| Error::invalid(format!("record {n}: {e}")))?);
                }
                let mut ctx = self.ctx(p, i.workspace);
                ctx.source = Some(i.source.unwrap_or_else(|| "ingest".into()));
                if i.background {
                    to_json(&self.ingest_queue().enqueue(ctx, muts).await?)
                } else {
                    let accepted = muts.len();
                    let result = self.ingest_queue().apply(ctx, muts).await?;
                    to_json(&crate::ingest::IngestReceipt { accepted, queued: false, result: Some(result) })
                }
            }
            "ingest.flush" => {
                self.ingest_queue().flush().await?;
                Ok(json!({ "flushed": true, "stats": self.ingest_queue().stats() }))
            }
            "conversation.get" => {
                let i: IdIn = parse(action, input)?;
                to_json(&self.conversations().get(p, &i.id)?)
            }
            "schema.get" => {
                let i: VersionOptIn = parse(action, input)?;
                match i.version {
                    Some(v) => to_json(&onto.schema_version_get(p, v)?.schema),
                    None => {
                        let mut doc = to_json(&*onto.schema())?;
                        doc["schema_version"] = json!(onto.schema_version());
                        Ok(doc)
                    }
                }
            }
            "schema.status" => to_json(&onto.schema_status(p)?),
            "schema.plan" => {
                let set: SchemaChangeSet = parse(action, input)?;
                to_json(&onto.schema_plan(p, &set)?)
            }
            "schema.apply" => {
                let i: SchemaApplyIn = parse(action, input)?;
                if i.dry_run {
                    return to_json(&onto.schema_plan(p, &i.set)?);
                }
                to_json(&onto.schema_apply(&self.ctx(p, i.workspace).with_intent("schema.apply"), i.set)?)
            }
            "schema.history" => {
                let i: LimitIn = parse(action, input)?;
                Ok(
                    json!({ "active": onto.schema_version(), "versions": onto.schema_history(p, i.limit.unwrap_or(50))? }),
                )
            }
            "schema.version" => {
                let i: VersionIn = parse(action, input)?;
                to_json(&onto.schema_version_get(p, i.version)?)
            }
            "schema.as_of" => {
                let i: AsOfTimeIn = parse(action, input)?;
                let ts = i
                    .ts
                    .unwrap_or_else(|| onto_core::time::now_ms() - (i.days_ago.unwrap_or(0.0) * 86_400_000.0) as i64);
                to_json(&onto.schema_as_of(p, ts)?)
            }
            "schema.diff" => {
                let i: DiffIn = parse(action, input)?;
                to_json(&onto.schema_diff(p, i.from, i.to)?)
            }
            "schema.propose" => {
                let i: ProposeIn = parse(action, input)?;
                to_json(&onto.propose_schema(&self.ctx(p, i.workspace).with_intent(action), i.input)?)
            }
            "schema.proposals" => {
                let i: ProposalsIn = parse(action, input)?;
                Ok(json!({ "proposals": onto.list_proposals(p, i.status, i.limit.unwrap_or(100))? }))
            }
            "schema.proposal" => {
                let i: IdIn = parse(action, input)?;
                to_json(&onto.get_proposal(p, &i.id)?)
            }
            "schema.approve" | "schema.reject" | "schema.comment" => {
                let i: ReviewIn = parse(action, input)?;
                let decision = match action {
                    "schema.approve" => Decision::Approve,
                    "schema.reject" => Decision::Reject,
                    _ => Decision::Comment,
                };
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                to_json(&onto.review_proposal(&ctx, &i.id, decision, &i.comment, i.seen_version)?)
            }
            "schema.withdraw" => {
                let i: IdWsIn = parse(action, input)?;
                to_json(&onto.withdraw_proposal(&self.ctx(p, i.workspace).with_intent(action), &i.id)?)
            }
            "schema.revise" => {
                let i: ReviseIn = parse(action, input)?;
                to_json(&onto.revise_proposal(&self.ctx(p, i.workspace).with_intent(action), &i.id, i.input)?)
            }
            "execution.graphs" => {
                let i: GraphsIn = parse(action, input)?;
                let spec = FindSpec {
                    kind: Some(onto_core::execution::GRAPH_KIND.into()),
                    workspace: i.workspace,
                    limit: i.limit.unwrap_or(200),
                    ..Default::default()
                };
                let (graphs, total) = onto.find(p, &spec)?;
                Ok(json!({ "graphs": graphs, "total": total }))
            }
            "execution.vocabulary" => {
                let i: GraphIn = parse(action, input)?;
                let v = onto.execution_vocabulary(p, &i.graph)?;
                Ok(
                    json!({ "graph": onto_core::execution::graph_id(&i.graph), "node_types": v.node_types, "relation_types": v.relation_types }),
                )
            }
            "execution.set_vocabulary" => {
                let i: SetVocabIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                to_json(&onto.set_execution_vocabulary(&ctx, &i.graph, i.vocabulary)?)
            }
            "execution.init_graph" => {
                let i: InitGraphIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                let given = !i.vocabulary.node_types.is_empty() || !i.vocabulary.relation_types.is_empty();
                let vocab = if given {
                    Some(i.vocabulary)
                } else if i.starter.unwrap_or(!i.empty) {
                    Some(onto_core::execution::Vocabulary::starter())
                } else {
                    None
                };
                to_json(&onto.init_execution_graph(&ctx, &i.graph, i.title.as_deref().unwrap_or(""), vocab)?)
            }
            "schema.revert" | "schema.undo" => {
                let i: RevertIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                let r = if action == "schema.revert" {
                    onto.schema_revert(&ctx, i.version, i.migrations, i.dry_run)?
                } else {
                    onto.schema_undo(&ctx, i.version, i.migrations, i.dry_run)?
                };
                to_json(&r)
            }
            "connector.add" => {
                let spec: crate::connectors::ConnectorSpec = parse(action, input)?;
                to_json(&self.add_connector(p, spec)?)
            }
            "connector.list" => Ok(json!({ "connectors": self.connectors(p)? })),
            "connector.remove" => {
                let i: RemoveConnectorIn = parse(action, input)?;
                self.remove_connector(p, &i.name, i.purge)
            }
            "connector.crawl" => {
                let i: CrawlIn = parse(action, input)?;
                Ok(json!({ "crawls": self.crawl(p, i.name.as_deref()).await? }))
            }
            "catalog.find" => {
                let i: TermIn = parse(action, input)?;
                let ws = self.ws_or_default(i.workspace);
                Ok(json!({ "term": i.term, "locations": onto.find_data(p, &i.term, Some(&ws), i.limit.unwrap_or(5))? }))
            }
            "catalog.join" => {
                let i: JoinIn = parse(action, input)?;
                Ok(json!({ "join": onto.join_path(p, &i.from, &i.to)? }))
            }
            "catalog.metric" => {
                let i: TermIn = parse(action, input)?;
                let ws = self.ws_or_default(i.workspace);
                Ok(json!({ "term": i.term, "metrics": onto.trusted_metrics(p, &i.term, Some(&ws))? }))
            }
            "catalog.rank" => {
                let i: IdIn = parse(action, input)?;
                match onto.authority(p, &i.id)? {
                    Some(a) => to_json(&a),
                    None => Err(Error::not_found(format!(
                        "no authority computed for {} (run catalog.rerank or crawl a connector)",
                        i.id
                    ))),
                }
            }
            "catalog.top" => {
                let i: TopIn = parse(action, input)?;
                Ok(json!({ "top": onto.authority_top(p, &i.kinds, i.workspace.as_deref(), i.limit.unwrap_or(20))? }))
            }
            "catalog.rerank" => {
                let i: RerankIn = parse(action, input)?;
                let ctx = self.ctx(p, None).with_intent(action);
                to_json(&onto.compute_authority(&ctx, &i.kinds, &i.weights.unwrap_or_default())?)
            }
            "learned.list" => {
                let i: LearnedIn = parse(action, input)?;
                Ok(json!({ "facts": onto.learned_facts(
                    p,
                    i.fact_type.as_deref(),
                    i.state.as_deref(),
                    i.about.as_deref(),
                    i.limit.unwrap_or(100),
                )? }))
            }
            "learned.run" => {
                let w: WsIn = parse(action, input)?;
                if !p.allows(onto_core::Access::Write) {
                    return Err(Error::Denied(format!("{} may not run the learner", p.id)));
                }
                to_json(&onto.learn(w.workspace.as_deref())?)
            }
            "learned.certify" | "learned.reject" => {
                let i: DecideFactIn = parse(action, input)?;
                if !onto.options().governance.can_approve(p) && !p.is_privileged() {
                    return Err(Error::Denied(format!(
                        "{} may not {} facts: approvers and admins decide, never agents",
                        p.id,
                        if action == "learned.certify" { "certify" } else { "reject" }
                    )));
                }
                let id = onto.resolve_id(&i.id);
                let ws = onto.get(p, &id)?.workspace;
                let mut params = Map::new();
                params.insert("fact".into(), json!(id));
                if let Some(n) = i.note {
                    params.insert("note".into(), json!(n));
                }
                let name = if action == "learned.certify" { "certify_fact" } else { "reject_fact" };
                let ctx = self.ctx(p, Some(ws)).with_intent(action);
                let out = onto.apply_action(&ctx, name, &params, ApplyOptions { dry_run: false, confirm: true })?;
                Ok(
                    json!({ "receipt": out, "fact": onto.learned_facts(p, None, None, None, 1_000)?.into_iter().find(|f| f.id == id) }),
                )
            }
            "ontology.draft" => {
                let i: crate::modeling::DraftIn = parse(action, input)?;
                to_json(&self.draft_ontology(p, &i)?)
            }
            "ontology.refine" => {
                let i: crate::modeling::RefineIn = parse(action, input)?;
                to_json(&self.refine_ontology(p, i).await?)
            }
            "ontology.advise" => {
                let i: crate::modeling::AdviseIn = parse(action, input)?;
                to_json(&self.advise_ontology(p, i).await?)
            }
            "ontology.apply_draft" => {
                let i: crate::modeling::ApplyDraftIn = parse(action, input)?;
                self.apply_ontology_draft(p, i).await
            }
            "system.stats" => self.stats(),
            "system.whoami" => {
                let gov = &onto.options().governance;
                Ok(json!({
                    "principal": p,
                    "capabilities": {
                        "read": p.allows(onto_core::Access::Read),
                        "write": p.allows(onto_core::Access::Write),
                        "apply_schema": gov.can_apply(p) && gov.live_edits,
                        "approve": gov.can_approve(p),
                        "propose": gov.can_propose(p),
                        "configure_outbound": p.is_privileged() && !p.is_agent(),
                        "automate": !p.is_agent() && (gov.can_approve(p) || p.is_privileged()),
                    },
                    "schema_version": onto.schema_version(),
                }))
            }
            "system.health" => Ok(json!({
                "status": "ok",
                "version": crate::VERSION,
                "revision": onto.revision(),
                "llm": self.llm().name(),
                "llm_available": self.llm().unavailable().is_none(),
                "outbound": self.outbound_health(),
            })),
            "system.actions" => Ok(json!({ "actions": ACTIONS })),
            a if a.starts_with("trace.") => self.trace_action(p, a, input),
            other => Err(Error::not_found(format!("action '{other}' (see system.actions)"))),
        }
    }

    // ── traces (docs/CONTROL_PLANE.md section 9.5) ──────────────────────────

    fn trace_action(&self, p: &Principal, action: &str, input: Value) -> Result<Value> {
        if action != "trace.score" && !p.can_observe() {
            return Err(Error::Denied(format!(
                "{} may not read traces (admin, developer or observer roles; an API key needs the admin scope)",
                p.id
            )));
        }
        let store = self.ontology().store();
        // Questions, answers and error texts may quote entities the observer
        // cannot read (restricted ones): only admins see them.
        let redact = !p.is_privileged();
        match action {
            "trace.list" => {
                let mut q: TraceQuery = parse(action, input)?;
                observe_scope(p, &mut q.workspace)?;
                // Matching on the question would rebuild it, a guess at a time.
                if redact && q.q.as_deref().is_some_and(|text| !text.is_empty()) {
                    return Err(Error::Denied(format!(
                        "{} may not search trace questions (admins only: they may quote what others cannot read)",
                        p.id
                    )));
                }
                let mut page = store.trace_list(&q)?;
                if redact {
                    page.traces.iter_mut().for_each(onto_core::trace::TraceRow::redact);
                }
                to_json(&page)
            }
            "trace.get" => {
                let i: IdIn = parse(action, input)?;
                let mut d = self.find_trace(&i.id)?;
                check_trace_workspace(p, &d)?;
                if redact {
                    d.redact();
                }
                to_json(&d)
            }
            "trace.metrics" => {
                let mut q: MetricsQuery = parse(action, input)?;
                observe_scope(p, &mut q.workspace)?;
                to_json(&store.trace_metrics(&q)?)
            }
            "trace.sessions" => {
                let mut q: SessionQuery = parse(action, input)?;
                observe_scope(p, &mut q.workspace)?;
                let mut page = store.trace_sessions(&q)?;
                if redact {
                    page.sessions.iter_mut().for_each(|s| s.preview = None);
                }
                to_json(&page)
            }
            "trace.session" => {
                let i: SessionIn = parse(action, input)?;
                let traces = store.trace_session(&i.id, i.limit.unwrap_or(onto_core::trace::DEFAULT_LIST))?;
                let mut traces: Vec<_> = traces.into_iter().filter(|t| p.in_workspace(&t.workspace)).collect();
                if redact {
                    traces.iter_mut().for_each(onto_core::trace::TraceRow::redact);
                }
                if traces.is_empty() {
                    return Err(Error::not_found(format!("session {}", i.id)));
                }
                Ok(json!({ "session_id": i.id, "traces": traces }))
            }
            "trace.score" => {
                let i: ScoreIn = parse(action, input)?;
                let score = self.score_trace(p, i)?;
                to_json(&score)
            }
            other => Err(Error::not_found(format!("action '{other}' (see system.actions)"))),
        }
    }

    /// A trace by id; when it is not stored yet, the queue is flushed and it
    /// is looked up once more (a trace is written up to `flush_ms` after it ends).
    fn find_trace(&self, id: &str) -> Result<TraceDetail> {
        let store = self.ontology().store();
        if let Some(d) = store.trace_get(id)? {
            return Ok(d);
        }
        if self.traces_enabled() {
            let flushed = match tokio::runtime::Handle::try_current() {
                Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                    tokio::task::block_in_place(|| h.block_on(self.flush_traces()));
                    true
                }
                _ => false,
            };
            if flushed && let Some(d) = store.trace_get(id)? {
                return Ok(d);
            }
        }
        Err(Error::not_found(format!("trace {id}")))
    }

    fn score_trace(&self, p: &Principal, i: ScoreIn) -> Result<ScoreRow> {
        let name = i.name.trim();
        if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
            return Err(Error::invalid("trace.score: name is 1 to 100 printable characters"));
        }
        if i.value.is_none() && i.label.is_none() {
            return Err(Error::invalid("trace.score: give a value or a label"));
        }
        if i.value.is_some_and(|v| !v.is_finite()) {
            return Err(Error::invalid("trace.score: value must be a finite number"));
        }
        let label = i.label.map(|l| l.trim().to_string());
        if label.as_ref().is_some_and(|l| l.is_empty() || l.chars().count() > 100) {
            return Err(Error::invalid("trace.score: label is 1 to 100 characters"));
        }
        if i.comment.as_ref().is_some_and(|c| c.chars().count() > 2000) {
            return Err(Error::invalid("trace.score: comment is at most 2000 characters"));
        }
        let d = self.find_trace(&i.trace_id)?;
        let own = d.trace.principal == p.id;
        if !own && !(p.can_observe() && p.allows(onto_core::Access::Write)) {
            return Err(Error::Denied(format!(
                "{} may not score traces (observers who may write, or the trace's own principal)",
                p.id
            )));
        }
        check_trace_workspace(p, &d)?;
        if let Some(s) = &i.span_id
            && !d.spans.iter().any(|x| &x.id == s)
        {
            return Err(Error::not_found(format!("span {s} of trace {}", i.trace_id)));
        }
        let score = ScoreRow {
            id: onto_core::time::new_id("score"),
            trace_id: d.trace.id,
            span_id: i.span_id,
            ts: onto_core::time::now_ms(),
            name: name.to_string(),
            value: i.value,
            label,
            source: if p.id.starts_with("user:") { "console".into() } else { "api".into() },
            author: Some(p.id.clone()),
            comment: i.comment,
        };
        self.ontology().store().score_write(&score)?;
        Ok(score)
    }
}

/// Actions that call the LLM: traced with `[traces] calls = "writes"`.
pub const LLM_ACTIONS: &[&str] = &["ontology.refine", "ontology.advise"];

/// A principal limited to some workspaces reads traces of those only: it
/// names one (or has exactly one), and only then.
fn observe_scope(p: &Principal, ws: &mut Option<String>) -> Result<()> {
    if p.workspaces.iter().any(|w| w == "*") {
        return Ok(());
    }
    match ws {
        Some(w) if p.in_workspace(w) => Ok(()),
        Some(w) => Err(Error::Denied(format!("{} has no access to workspace {w}", p.id))),
        None if p.workspaces.len() == 1 => {
            *ws = Some(p.workspaces[0].clone());
            Ok(())
        }
        None => Err(Error::Denied(format!("{} reads traces of one workspace at a time: name it", p.id))),
    }
}

fn check_trace_workspace(p: &Principal, d: &TraceDetail) -> Result<()> {
    if p.in_workspace(&d.trace.workspace) { Ok(()) } else { Err(Error::not_found(format!("trace {}", d.trace.id))) }
}

#[derive(Deserialize)]
struct SessionIn {
    id: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ScoreIn {
    trace_id: String,
    #[serde(default)]
    span_id: Option<String>,
    name: String,
    #[serde(default)]
    value: Option<f64>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    comment: Option<String>,
}

pub fn find(name: &str) -> Option<&'static ActionInfo> {
    ACTIONS.iter().find(|a| a.name == name)
}

#[derive(Deserialize)]
struct NameIn {
    name: String,
}

#[derive(Deserialize, Default)]
struct VersionOptIn {
    #[serde(default)]
    version: Option<u64>,
}

#[derive(Deserialize)]
struct VersionIn {
    version: u64,
}

#[derive(Deserialize, Default)]
struct LimitIn {
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct SchemaApplyIn {
    #[serde(flatten)]
    set: SchemaChangeSet,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize, Default)]
struct AsOfTimeIn {
    #[serde(default)]
    ts: Option<i64>,
    #[serde(default)]
    days_ago: Option<f64>,
}

#[derive(Deserialize)]
struct DiffIn {
    from: u64,
    #[serde(default)]
    to: Option<u64>,
}

#[derive(Deserialize, Default)]
struct GraphsIn {
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct GraphIn {
    graph: String,
}

#[derive(Deserialize)]
struct SetVocabIn {
    graph: String,
    #[serde(flatten)]
    vocabulary: onto_core::execution::Vocabulary,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct InitGraphIn {
    graph: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    starter: Option<bool>,
    /// gRPC spelling of `starter: false`.
    #[serde(default)]
    empty: bool,
    #[serde(flatten)]
    vocabulary: onto_core::execution::Vocabulary,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct ProposeIn {
    #[serde(flatten)]
    input: ProposalInput,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize, Default)]
struct ProposalsIn {
    #[serde(default)]
    status: Option<ProposalStatus>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ReviewIn {
    id: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    seen_version: Option<u64>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct IdWsIn {
    id: String,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct ReviseIn {
    id: String,
    #[serde(flatten)]
    input: ProposalInput,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct RevertIn {
    version: u64,
    #[serde(default)]
    migrations: Vec<Migration>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct GetIn {
    id: String,
    #[serde(default)]
    derived: bool,
}

#[derive(Deserialize)]
struct ActionIn {
    action: String,
    #[serde(default)]
    params: Map<String, Value>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    dry_run: bool,
    /// `action.plan`: also report the derived facts the edits change.
    #[serde(default)]
    consequences: bool,
}

#[derive(Deserialize)]
struct UndoIn {
    receipt: String,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct HistoryIn {
    id: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct AsOfIn {
    id: String,
    #[serde(default)]
    ts: Option<i64>,
    #[serde(default)]
    days_ago: Option<f64>,
}

#[derive(Deserialize)]
struct ChangesIn {
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    since_days: Option<f64>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    kinds: Vec<String>,
    #[serde(default)]
    within: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct DupesIn {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    threshold: Option<f32>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct MergeIn {
    keep: String,
    merge: String,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct QueryIn {
    action: String,
    #[serde(default)]
    input: Value,
}

#[derive(Deserialize)]
struct ScenarioIn {
    #[serde(default)]
    mutations: Vec<Mutation>,
    #[serde(default)]
    queries: Vec<QueryIn>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct ExpertsIn {
    topic: String,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ExportIn {
    #[serde(default = "default_format")]
    format: String,
    #[serde(default, deserialize_with = "one_or_many")]
    start: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    rels: Vec<String>,
    #[serde(default)]
    depth: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    include_schema: Option<bool>,
    #[serde(default)]
    include_data: Option<bool>,
}

fn default_format() -> String {
    "turtle".into()
}

#[derive(Deserialize)]
struct EvalIn {
    #[serde(default)]
    cases: Vec<crate::eval::EvalCase>,
    #[serde(default)]
    toml: Option<String>,
}

#[derive(Deserialize)]
struct QualityIn {
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// Queries one scenario may run (each runs twice: live and on the fork).
const MAX_SCENARIO_QUERIES: usize = 20;

/// Run CPU-bound engine work (a closure, a shape evaluation) without
/// stalling the async runtime: on a multi-thread runtime the worker hands its
/// other tasks to another thread while `f` runs.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Read-only graph actions; these also run inside what-if scenarios on a fork
/// (with [`assurance::GRAPH_READS`]).
pub const GRAPH_READS: &[&str] = &[
    "entity.get",
    "entity.find",
    "entity.search",
    "entity.derived",
    "graph.neighbors",
    "graph.traverse",
    "graph.path",
    "graph.infer",
    "graph.dependencies",
    "graph.impact",
    "graph.root_cause",
    "graph.cycles",
    "graph.critical_path",
    "graph.overview",
    "graph.snapshot",
    "metric.query",
    "quality.report",
    "objects.types",
    "objects.explore",
];

/// Run a read-only graph action against any ontology (live or a scenario fork).
pub fn graph_call(onto: &Ontology, p: &Principal, default_ws: &str, action: &str, input: Value) -> Result<Value> {
    match action {
        "entity.get" => {
            let i: GetIn = parse(action, input)?;
            let e = onto.get(p, &i.id)?;
            let mut v = to_json(&e)?;
            if i.derived {
                v["derived"] = Value::Object(onto.derived(p, &e.id)?);
            }
            Ok(v)
        }
        "entity.derived" => {
            let i: IdIn = parse(action, input)?;
            Ok(json!({ "id": i.id, "derived": onto.derived(p, &i.id)? }))
        }
        "entity.find" => {
            let spec: FindSpec = parse(action, input)?;
            let (entities, total) = onto.find(p, &spec)?;
            Ok(json!({ "entities": entities, "total": total }))
        }
        "entity.search" => {
            let s: SearchIn = parse(action, input)?;
            let hits = onto.search_with(
                p,
                &s.query,
                &s.kinds,
                s.workspace.as_deref(),
                s.limit.unwrap_or(10),
                s.mode.unwrap_or_default(),
                s.personalize.unwrap_or(true),
            )?;
            Ok(json!({ "hits": hits }))
        }
        "graph.neighbors" => {
            let n: NeighborsIn = parse(action, input)?;
            let list: Vec<Value> = onto
                .neighbors(p, &n.id, n.rel.as_deref())?
                .into_iter()
                .map(|(rel, entity)| json!({ "rel": rel, "entity": entity }))
                .collect();
            Ok(json!({ "neighbors": list }))
        }
        "graph.traverse" => {
            let spec: TraverseSpec = parse(action, input)?;
            to_json(&onto.traverse(p, &spec)?)
        }
        "graph.path" => {
            let i: PathIn = parse(action, input)?;
            match onto.path(p, &i.from, &i.to, i.depth.unwrap_or(5))? {
                Some(hops) => Ok(json!({ "found": true, "text": onto.path_text(&hops), "hops": hops })),
                None => Ok(json!({ "found": false, "hops": [] })),
            }
        }
        "graph.infer" => {
            let i: IdIn = parse(action, input)?;
            Ok(json!({ "inferred": onto.infer(p, &i.id)? }))
        }
        "graph.dependencies" => {
            let d: DepsIn = parse(action, input)?;
            to_json(&onto.dependencies(p, &d.id, d.direction.unwrap_or_default(), d.depth.unwrap_or(4))?)
        }
        "graph.impact" => {
            let d: DepsIn = parse(action, input)?;
            to_json(&onto.impact(p, &d.id, d.depth.unwrap_or(4))?)
        }
        "graph.root_cause" => {
            let d: DepsIn = parse(action, input)?;
            to_json(&onto.root_cause(p, &d.id, d.depth.unwrap_or(6))?)
        }
        "graph.cycles" => {
            let w: WsIn = parse(action, input)?;
            Ok(json!({ "cycles": onto.cycles(p, w.workspace.as_deref())? }))
        }
        "graph.critical_path" => {
            let i: IdIn = parse(action, input)?;
            to_json(&onto.critical_path(p, &i.id)?)
        }
        "graph.overview" => {
            let mut spec: onto_core::map::OverviewSpec = parse(action, input)?;
            if spec.limit == 0 {
                spec.limit = 150;
            }
            to_json(&onto.overview(p, &spec)?)
        }
        "graph.snapshot" => {
            let spec: onto_core::map::SnapshotSpec = parse(action, input)?;
            to_json(&onto.snapshot(p, &spec)?)
        }
        "metric.query" => {
            let mut q: MetricQuery = parse(action, input)?;
            if q.workspace.is_none() {
                q.workspace = Some(default_ws.to_string());
            }
            to_json(&onto.metric(p, &q)?)
        }
        "quality.report" => {
            let i: QualityIn = parse(action, input)?;
            let ws = i.workspace.unwrap_or_else(|| default_ws.to_string());
            to_json(&onto.quality(p, Some(&ws), i.kind.as_deref(), i.limit.unwrap_or(200))?)
        }
        "objects.types" => {
            let w: WsIn = parse(action, input)?;
            Ok(json!({ "types": onto.object_types(p, w.workspace.as_deref())? }))
        }
        "objects.explore" => {
            let spec: onto_core::explorer::ExploreSpec = parse(action, input)?;
            to_json(&onto.explore(p, &spec)?)
        }
        a if assurance::GRAPH_READS.contains(&a) => assurance::graph_read(onto, p, a, input),
        other => Err(Error::invalid(format!("{other} is not a read-only graph action"))),
    }
}

/// Actions whose input or output may hold a signing secret, a credential or
/// a receiver's URL with its token: webhook endpoints, writeback targets and
/// connectors. Their call traces record no content.
pub(crate) fn secret_bearing(action: &str) -> bool {
    crate::webhooks::CALLS.contains(&action)
        || crate::writeback::CALLS.contains(&action)
        || action.starts_with("connector.")
}
