//! Named actions: JSON in, JSON out. One dispatch table serves the gRPC
//! services (built-in and user protos), HTTP JSON, the CLI and the C FFI.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use onto_core::deps::DepDirection;
use onto_core::query::{FindSpec, TraverseSpec};
use onto_core::{
    ActionContext, EntityInput, EntityPatch, EntityRef, Error, EventFilter, EventInput, Id, LinkInput, Mutation,
    Principal, Result, one_or_many,
};

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
    a("schema.get", false, "The active ontology schema."),
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
        ActionContext::new(p.clone(), self.ws_or_default(ws)).with_source("api")
    }

    /// Run a named action. See [`ACTIONS`].
    pub async fn call(&self, p: &Principal, action: &str, input: Value) -> Result<Value> {
        let onto = self.ontology();
        let ws_hint = input.get("workspace").and_then(Value::as_str).map(str::to_string);
        match action {
            "entity.upsert" => {
                let e: EntityInput = parse(action, input)?;
                to_json(&onto.upsert(&self.ctx(p, ws_hint), e)?)
            }
            "entity.patch" => {
                let patch: EntityPatch = parse(action, input)?;
                to_json(&onto.patch(&self.ctx(p, ws_hint), patch)?)
            }
            "entity.get" => {
                let i: IdIn = parse(action, input)?;
                to_json(&onto.get(p, &i.id)?)
            }
            "entity.delete" => {
                let i: IdIn = parse(action, input)?;
                let r = onto.apply(&self.ctx(p, ws_hint), vec![Mutation::Delete(EntityRef { id: i.id.clone() })])?;
                Ok(json!({ "id": i.id, "deleted": true, "revision": r.revision }))
            }
            "entity.find" => {
                let spec: FindSpec = parse(action, input)?;
                let (entities, total) = onto.find(p, &spec)?;
                Ok(json!({ "entities": entities, "total": total }))
            }
            "entity.search" => {
                let s: SearchIn = parse(action, input)?;
                let hits = onto.search(p, &s.query, &s.kinds, s.workspace.as_deref(), s.limit.unwrap_or(10))?;
                Ok(json!({ "hits": hits }))
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
            "schema.get" => to_json(onto.schema()),
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
