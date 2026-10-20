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

use crate::ingest::normalize_record;
use crate::intelli::{AskRequest, Intelli};
use crate::memory::{MemoryScope, RememberInput};

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ActionInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub write: bool,
}

const fn a(name: &'static str, write: bool, description: &'static str) -> ActionInfo {
    ActionInfo { name, description, write }
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
        "context.build",
        false,
        "Question context graph and packed facts, no LLM call. Input: {question, workspace?, token_budget?}.",
    ),
    a(
        "intelli.ask",
        false,
        "Answer a question (deterministic first, LLM when reasoning is needed). Input: AskRequest.",
    ),
    a("memory.remember", true, "Store a long-term fact or preference. Input: {text, entities?, kind?, private?}."),
    a("memory.recall", false, "Recall memories. Input: {query, entities?, k?}."),
    a("memory.list", false, "List memories. Input: {scope?, limit?}."),
    a("memory.forget", true, "Delete a memory. Input: {id}."),
    a("memory.consolidate", true, "Expire episodes and promote recurring topics to long-term memory."),
    a("event.append", true, "Record an event (audit/activity). Input: EventInput."),
    a("event.list", false, "Query events. Input: {workspace?, actor?, target?, action?, since?, until?, limit?}."),
    a("ingest.batch", true, "Ingest records (mutations, entities, GitHub webhooks). Input: {records: [...], async?}."),
    a("ingest.flush", false, "Wait until queued ingestion is applied."),
    a("conversation.get", false, "Fetch a conversation with its turns. Input: {id}."),
    a("action.list", false, "Action types (governed edits) defined in the schema."),
    a("action.describe", false, "One action type: parameters, checks, effects, risk. Input: {name}."),
    a("action.plan", false, "Validate an action and preview its edits and reversibility. Input: {action, params}."),
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
        "Dry run of a schema change set: validation, diff, impact (entities/links, breaking), migration. Input: SchemaChangeSet.",
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
    a("system.stats", false, "Graph, memory, cache, token and ingestion statistics."),
    a("system.health", false, "Liveness and version."),
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
}

#[derive(Deserialize, Default)]
struct ListMemIn {
    #[serde(default)]
    scope: Option<MemoryScope>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
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

fn parse<T: serde::de::DeserializeOwned>(action: &str, input: Value) -> Result<T> {
    let input = if input.is_null() { json!({}) } else { input };
    serde_json::from_value(input).map_err(|e| Error::invalid(format!("{action}: {e}")))
}

fn to_json<T: Serialize>(v: &T) -> Result<Value> {
    Ok(serde_json::to_value(v)?)
}

impl Intelli {
    fn ws_or_default(&self, ws: Option<String>) -> String {
        ws.filter(|w| !w.is_empty()).unwrap_or_else(|| self.default_workspace().to_string())
    }

    fn ctx(&self, p: &Principal, ws: Option<String>) -> ActionContext {
        let source = match &p.via {
            Some(service) => format!("api via {service}"),
            None => "api".to_string(),
        };
        ActionContext::new(p.clone(), self.ws_or_default(ws)).with_source(source)
    }

    /// Run a named action. See [`ACTIONS`].
    pub async fn call(&self, p: &Principal, action: &str, input: Value) -> Result<Value> {
        let onto = self.ontology();
        if GRAPH_READS.contains(&action) {
            return graph_call(onto, p, self.default_workspace(), action, input);
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
                to_json(&onto.plan_action(&self.ctx(p, i.workspace), &i.action, &i.params)?)
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
                let hits =
                    self.memory().recall(p, &ws, &r.query, &r.entities, r.k.unwrap_or(self.config().memory.recall_k));
                Ok(json!({ "items": hits }))
            }
            "memory.list" => {
                let l: ListMemIn = parse(action, input)?;
                let ws = self.ws_or_default(l.workspace);
                Ok(json!({ "items": self.memory().list(p, &ws, l.scope, l.limit.unwrap_or(50)) }))
            }
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
            "system.stats" => self.stats(),
            "system.health" => Ok(json!({
                "status": "ok",
                "version": crate::VERSION,
                "revision": onto.revision(),
                "llm": self.llm().name(),
                "llm_available": self.llm().unavailable().is_none(),
            })),
            "system.actions" => Ok(json!({ "actions": ACTIONS })),
            other => Err(Error::not_found(format!("action '{other}' (see system.actions)"))),
        }
    }
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

/// Read-only graph actions; these also run inside what-if scenarios on a fork.
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
    "metric.query",
    "quality.report",
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
        other => Err(Error::invalid(format!("{other} is not a read-only graph action"))),
    }
}
