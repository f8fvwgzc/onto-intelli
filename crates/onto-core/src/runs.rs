//! Automation runs (docs/design/EVENTS.md C.4-C.8): the run rows, how one
//! run is evaluated and committed, retried and recovered.
//!
//! * **Claim, then commit.** The consumer claims a run first
//!   ([`Store::run_claim`]: `INSERT … ON CONFLICT (automation, trigger_key)
//!   DO NOTHING`, its own statement), so a duplicate trigger is settled by
//!   the claim and never retried. Then [`Ontology::commit_run`] runs one
//!   `transact` whose build closure, on the locked graph, evaluates the
//!   conditions, plans the action effect (preconditions included), renders
//!   webhook payloads strictly, and commits the graph edits, the receipt,
//!   the outbox rows, the `automation.run` audit event and the finished run
//!   row together, fenced by the consumer's lease epoch. The store refuses a
//!   commit whose run row is no longer `running`, so a run is completed at
//!   most once.
//! * **Recovery.** A `running` row means nothing of its commit happened: at
//!   the start of every round the lease holder executes every `running` row
//!   of its epoch or an older one again ([`Ontology::recover_runs`]).
//! * **Closure at commit.** The commit runs the definition of the locked
//!   schema, and only while the automation's closure still hashes as the
//!   run's (`closure_hash`); otherwise the run is `cancelled:
//!   stale_definition: …` with nothing else written.
//! * **Outcomes.** Conditions or preconditions that no longer hold at the
//!   commit make the run `failed: precondition_changed`; a plan that needs a
//!   person's confirmation is `refused` (automations run only low-risk,
//!   reversible action types); shadow mode records `would_run` with the plan
//!   and writes nothing else. `Unavailable` and contention `Conflict` errors
//!   are retried with backoff through a timer (`retry:<run>`); every other
//!   error is final.
//!
//! Webhook effects are outbox rows committed with the run: each is enqueued
//! once and delivered at least once. External effects are not reversible.
//!
//! [`Store::run_claim`]: crate::store::Store::run_claim

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::action_types::{
    ActionPlan, ActionReceipt, PlanSeed, RECEIPTS, Templates, audit_target, eval_checks, writeback_rows,
};
use crate::automate::{
    self, Automation, AutomationState, Enablements, Mode, NS_AUTOMATION_CONTROL, OutboundRef, PRINCIPAL_PREFIX,
};
use crate::changes::{ChangeOp, ChangeRecord, Cursor, RunCause, StateMove};
use crate::engine::{ActionContext, Extra, Ontology};
use crate::graph::Graph;
use crate::leases::Fence;
use crate::model::{Entity, EventInput, Mutation};
use crate::outbox::{NS_WEBHOOK_ENDPOINT, OutboxKind, OutboxRow, SOURCE_AUTOMATION};
use crate::policy::{self, Principal, dominates};
use crate::schema::Schema;
use crate::time::{new_id, now_ms};
use crate::{Error, Result};

/// The lease of a tenant's automation consumer (one holder across processes).
pub const CONSUMER_LEASE: &str = "automation:consumer";
/// The change-feed cursor of the automation consumer.
pub const CONSUMER: &str = "automation:consumer";
/// Timer names of run retries: `retry:<run id>`.
pub const RETRY_PREFIX: &str = "retry:";
/// `automation.control` key of the tenant kill switch (`{ "on": true }`).
pub const KILL_KEY: &str = "kill";
/// Most rows one [`RunQuery`] returns.
pub const MAX_LIST: usize = 500;
/// Reason prefix of a run whose conditions or preconditions stopped holding.
pub const PRECONDITION_CHANGED: &str = "precondition_changed";
/// Error prefix a store uses when a run row in a commit is no longer
/// `running` (another execution settled it): the whole commit is refused.
pub const RUN_SETTLED: &str = "run_settled";
/// What a run row says about its guarantees.
pub const RUN_MEANS: &str = "The run's graph edits, receipt, outbox rows and this row committed together. Webhook \
effects are enqueued once and delivered at least once (receivers dedupe on the event id). External effects are not \
reversible.";

/// Where a run is in its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Claimed; its commit has not happened yet (recovery runs it again).
    #[default]
    Running,
    /// Its commit happened.
    Succeeded,
    /// Final: an error that retrying does not fix, or a condition or
    /// precondition that stopped holding.
    Failed,
    /// Waiting for its retry timer.
    Retry,
    /// Over a rate limit or a subject budget; `automation.rerun` can run it.
    Skipped,
    /// Final: the depth limit, or a plan that needs a person's confirmation.
    Refused,
    /// Shadow mode: what it would have done, nothing written.
    WouldRun,
    /// Stopped (kill switch, disabled or removed automation) before it ran.
    Cancelled,
}

impl RunState {
    pub const ALL: [RunState; 8] = [
        RunState::Running,
        RunState::Succeeded,
        RunState::Failed,
        RunState::Retry,
        RunState::Skipped,
        RunState::Refused,
        RunState::WouldRun,
        RunState::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Succeeded => "succeeded",
            RunState::Failed => "failed",
            RunState::Retry => "retry",
            RunState::Skipped => "skipped",
            RunState::Refused => "refused",
            RunState::WouldRun => "would_run",
            RunState::Cancelled => "cancelled",
        }
    }

    /// Nothing more happens to it unless a person reruns it.
    pub fn is_final(self) -> bool {
        !matches!(self, RunState::Running | RunState::Retry)
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RunState {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        RunState::ALL.into_iter().find(|st| st.as_str() == s).ok_or_else(|| {
            Error::invalid(format!(
                "run state '{s}' (running, succeeded, failed, retry, skipped, refused, would_run, cancelled)"
            ))
        })
    }
}

/// What started a run: the change record as the run principal saw it when
/// it was committed. Templates read it as `{trigger.<field>}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunTrigger {
    /// The trigger kind (`change`).
    pub on: String,
    pub cursor: Cursor,
    pub op: ChangeOp,
    /// The entity id, or `from|rel|to` for a link.
    pub id: String,
    /// The entity kind, or the canonical relation for a link.
    pub kind: String,
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<StateMove>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// The entity conditions and `{subject.*}` read: the changed entity, or
    /// a link's `from` end.
    pub subject: String,
    /// Who wrote the change.
    #[serde(default)]
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The cascade depth of the change (0: not written by an automation).
    #[serde(default)]
    pub depth: u8,
    /// The W3C trace id of the call that wrote the change: the run, its
    /// writes and its deliveries continue it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

impl RunTrigger {
    /// The trigger of a change record.
    pub fn of(r: &ChangeRecord) -> Self {
        let subject = match &r.link {
            Some(l) => l.from.clone(),
            None => r.id.clone(),
        };
        RunTrigger {
            on: "change".into(),
            cursor: r.cursor,
            op: r.op,
            id: r.id.clone(),
            kind: r.kind.clone(),
            workspace: r.workspace.clone(),
            changed: r.changed.clone(),
            state: r.state.clone(),
            revision: r.revision,
            subject,
            actor: r.cause.actor.clone(),
            source: r.cause.source.clone(),
            depth: r.cause.depth,
            trace_id: r.cause.trace_id.clone().filter(|t| crate::trace::is_trace_id(t)),
        }
    }
}

/// What one effect of a run did (or would do, in shadow mode and dry runs).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunEffect {
    pub index: usize,
    /// `action` or `webhook`.
    pub kind: String,
    /// The action type, or the endpoint.
    pub target: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The action plan (shadow mode and dry runs), or the rendered payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Value>,
}

/// One run (table `automation_runs`, docs/design/EVENTS.md C.8).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRow {
    /// `run_…`; the run's `request_id`.
    pub id: String,
    pub automation: String,
    /// Unique per automation: the trigger's change cursor.
    pub trigger_key: String,
    /// The closure hash of the enablement it ran under.
    pub closure_hash: String,
    pub schema_version: u64,
    pub mode: Mode,
    /// The run's cascade depth (the change's depth plus one).
    pub depth: u8,
    pub state: RunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub trigger: RunTrigger,
    /// `automation:<name>`.
    pub principal: String,
    #[serde(default)]
    pub effects: Vec<RunEffect>,
    /// Executions so far (retries count).
    #[serde(default)]
    pub attempts: u32,
    /// The consumer lease epoch it was last claimed or resumed under.
    pub epoch: u64,
    /// Store-clock milliseconds (set by the store when it is claimed).
    #[serde(default)]
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default)]
    pub means: String,
}

impl RunRow {
    /// A `running` row for automation `name` triggered by `trigger`.
    pub fn new(name: &str, closure_hash: &str, schema_version: u64, def: &Automation, trigger: RunTrigger) -> Self {
        RunRow {
            id: new_id("run"),
            automation: name.to_string(),
            trigger_key: trigger.cursor.to_string(),
            closure_hash: closure_hash.to_string(),
            schema_version,
            mode: def.mode,
            depth: trigger.depth.saturating_add(1),
            state: RunState::Running,
            reason: None,
            principal: format!("{PRINCIPAL_PREFIX}{name}"),
            trigger,
            effects: Vec::new(),
            attempts: 0,
            epoch: 0,
            started_at: 0,
            finished_at: None,
            // The id of the run's `automation` trace, fixed at the claim so
            // every execution of the run continues the same one.
            trace_id: Some(crate::trace::new_trace_id()),
            means: RUN_MEANS.to_string(),
        }
    }

    /// The W3C trace id the run's writes and deliveries carry: the trace its
    /// trigger continues, else the run's own `automation` trace.
    pub fn continues_trace(&self) -> Option<String> {
        self.trigger.trace_id.iter().chain(self.trace_id.iter()).find(|t| crate::trace::is_trace_id(t)).cloned()
    }

    /// The same row in `state` with `reason`.
    pub fn with_state(mut self, state: RunState, reason: Option<String>) -> Self {
        self.state = state;
        self.reason = reason;
        self
    }

    /// The id of the receipt its action effect writes (stable across
    /// executions, so a re-planned `create` makes the same id).
    pub fn receipt_id(&self) -> String {
        format!("act_{}", self.id.strip_prefix("run_").unwrap_or(&self.id))
    }

    /// The summary in its `automation.run` audit event.
    pub fn summary(&self) -> Value {
        json!({
            "run": self.id,
            "automation": self.automation,
            "state": self.state,
            "reason": self.reason,
            "trigger": { "cursor": self.trigger.cursor, "op": self.trigger.op, "id": self.trigger.id },
            "depth": self.depth,
            "mode": self.mode,
            "effects": self.effects.iter().map(|e| json!({
                "index": e.index, "kind": e.kind, "target": e.target, "ok": e.ok,
                "receipt": e.receipt, "outbox": e.outbox, "error": e.error,
            })).collect::<Vec<_>>(),
            "attempts": self.attempts,
        })
    }
}

/// Narrows [`Store::runs_list`](crate::store::Store::runs_list).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RunQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<String>,
    /// Any of these states (empty: all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<RunState>,
    /// Runs started at or after this store-clock time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    /// At most this many (default 50, at most [`MAX_LIST`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl RunQuery {
    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(50).clamp(1, MAX_LIST)
    }

    /// Does `r` match (the memory store's filter; PostgreSQL filters in SQL)?
    pub fn matches(&self, r: &RunRow) -> bool {
        self.automation.as_ref().is_none_or(|a| &r.automation == a)
            && (self.states.is_empty() || self.states.contains(&r.state))
            && self.since.is_none_or(|s| r.started_at >= s)
    }
}

pub(crate) fn no_runs() -> Error {
    Error::Unavailable("this store keeps no automation runs".into())
}

/// A run row as a commit carries it: rows must exist and be `running`.
pub(crate) fn settled(id: &str) -> Error {
    Error::Conflict(format!("{RUN_SETTLED}: run {id} is no longer running (another execution settled it)"))
}

// ── evaluation ───────────────────────────────────────────────────────────

/// What evaluating a run on a graph found.
#[derive(Debug)]
pub enum Verdict {
    /// Every condition holds and every effect is ready.
    Ready(Box<Prepared>),
    /// These conditions do not hold.
    NotMet(Vec<String>),
    /// The action's plan is not valid (failed checks, or nothing to change).
    Invalid(Vec<String>),
    /// The action's plan needs a person's confirmation (risk, not reversible).
    NeedsConfirmation(String),
}

/// A run ready to commit.
#[derive(Debug, Default)]
pub struct Prepared {
    pub mutations: Vec<Mutation>,
    /// The receipt of the action effect (status `applied` once committed).
    pub receipt: Option<ActionReceipt>,
    pub outbox: Vec<OutboxRow>,
    pub effects: Vec<RunEffect>,
    pub plan: Option<ActionPlan>,
}

/// What a committed run left.
#[derive(Clone, Debug)]
pub struct RunResult {
    pub run: RunRow,
    pub receipt: Option<ActionReceipt>,
}

/// Why an automation runs now, or why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Active,
    /// The reason: `not_enabled`, `killed`, `paused: …`, `stale_definition: …`,
    /// `enablement_expired`, `not_defined`.
    Inactive(String),
}

impl Status {
    pub fn is_active(&self) -> bool {
        *self == Status::Active
    }
}

/// Is the tenant's automation kill switch on?
pub fn killed(store: &dyn crate::store::Store) -> Result<bool> {
    Ok(store
        .kv_get(NS_AUTOMATION_CONTROL, KILL_KEY)?
        .is_some_and(|v| v.get("on").and_then(Value::as_bool) == Some(true)))
}

/// Whether automation `name` runs now under `schema` and the stored
/// enablements `en` (`now`: ms). Inactive with the first reason that
/// applies; a stale closure names the items that changed.
pub fn status(schema: &Schema, en: &Enablements, name: &str, now: i64, killed: bool) -> Status {
    let Some(def) = schema.automations.get(name) else { return Status::Inactive("not_defined".into()) };
    let Some(st) = en.states.get(name).filter(|s| s.enabled) else {
        return Status::Inactive("not_enabled".into());
    };
    if killed {
        return Status::Inactive("killed".into());
    }
    if let Some(p) = &st.paused {
        return Status::Inactive(format!("paused: {}", p.reason));
    }
    if st.expires_at <= now {
        return Status::Inactive("enablement_expired".into());
    }
    let changed = st.bound().changed(&automate::closure(schema, name, def, &en.endpoints, &en.targets));
    if !changed.is_empty() {
        return Status::Inactive(format!("stale_definition: {}", changed.join(", ")));
    }
    Status::Active
}

/// How runs that failed on unavailability or contention are retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub base_ms: i64,
    pub max_ms: i64,
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { base_ms: 10_000, max_ms: 3_600_000, max_attempts: 8 }
    }
}

impl RetryPolicy {
    /// `min(base * 2^(attempt - 1), max)`.
    pub fn delay_ms(&self, attempt: u32) -> i64 {
        let exp = attempt.saturating_sub(1).min(40);
        self.base_ms.saturating_mul(1i64 << exp).min(self.max_ms).max(0)
    }
}

/// Is `e` worth retrying (unavailability, contention), as opposed to final?
/// A lost lease is neither: the worker stops and the next holder recovers.
pub fn retryable(e: &Error) -> bool {
    match e {
        Error::Unavailable(m) => !is_lease_lost(e) && !m.is_empty(),
        Error::Conflict(m) => !m.starts_with(RUN_SETTLED) && !m.starts_with(PRECONDITION_CHANGED),
        _ => false,
    }
}

/// The write was refused because the writer's lease is gone.
pub fn is_lease_lost(e: &Error) -> bool {
    matches!(e, Error::Unavailable(m) if m.starts_with("lease lost"))
}

/// The run's commit was refused because its row is no longer `running`.
pub fn is_settled(e: &Error) -> bool {
    matches!(e, Error::Conflict(m) if m.starts_with(RUN_SETTLED))
}

/// Why `run` may not commit under `schema` now (`None`: it may): its
/// automation was deleted, or its closure is no longer the one the run was
/// claimed under (the enablement's).
fn stale_at_commit(schema: &Schema, en: &Enablements, run: &RunRow) -> Option<String> {
    let Some(def) = schema.automations.get(&run.automation) else {
        return Some(format!("stale_definition: automation:{} was deleted", run.automation));
    };
    let now = automate::closure(schema, &run.automation, def, &en.endpoints, &en.targets);
    if now.hash == run.closure_hash {
        return None;
    }
    let changed = match en.states.get(&run.automation).filter(|s| s.closure_hash == run.closure_hash) {
        Some(st) => st.bound().changed(&now),
        None => vec!["enablement".to_string()],
    };
    Some(format!("stale_definition: {}", changed.join(", ")))
}

impl Ontology {
    /// The context a run of `def` writes with: the service principal
    /// `automation:<name>` with its `run_as`, `request_id = run.id`, intent and
    /// source `automation:<name>`, and the cause `{automation, run, depth}`.
    /// Its workspace is the trigger's when the run may act there, else the
    /// first of `run_as` (the default workspace for `*`).
    pub fn run_ctx(&self, run: &RunRow, def: &Automation) -> ActionContext {
        let p = def.principal(&run.automation);
        let ws = if !run.trigger.workspace.is_empty() && p.in_workspace(&run.trigger.workspace) {
            run.trigger.workspace.clone()
        } else {
            match def.run_as.workspaces.iter().find(|w| *w != "*") {
                Some(w) => w.clone(),
                None => self.options().default_workspace.clone(),
            }
        };
        let source = format!("{PRINCIPAL_PREFIX}{}", run.automation);
        let mut ctx =
            ActionContext::new(p, ws).with_request_id(run.id.clone()).with_intent(source.clone()).with_source(source);
        ctx.cause = Some(RunCause { automation: run.automation.clone(), run: run.id.clone(), depth: run.depth });
        ctx.trace_id = run.continues_trace();
        ctx
    }

    /// Evaluate `run` of `def` on `g`: conditions, the action's plan
    /// (preconditions included) and every webhook payload, rendered strictly
    /// as the run principal. With `also`, an entity is visible only when
    /// `also` may see it too (dry runs). An endpoint that is missing, or whose
    /// principal does not dominate `run_as`, is an error, as is a template
    /// without a value (`invalid_template`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn evaluate_on(
        &self,
        g: &Graph,
        schema: &Schema,
        run: &RunRow,
        def: &Automation,
        endpoints: &[OutboundRef],
        also: Option<&Principal>,
    ) -> Result<Verdict> {
        let ctx = self.run_ctx(run, def);
        let p = &ctx.principal;
        let see = |e: &Entity| policy::visible(p, e, g) && also.is_none_or(|a| policy::visible(a, e, g));
        let mut params = Map::new();
        params.insert("trigger".into(), serde_json::to_value(&run.trigger)?);
        params.insert("subject".into(), json!(run.trigger.subject));
        params.insert("automation".into(), json!(run.automation));
        params.insert("run".into(), json!(run.id));
        let receipt_id = run.receipt_id();
        let ts = if run.started_at > 0 { run.started_at } else { now_ms() };
        let seed = PlanSeed::of(&receipt_id, ts);
        let env = Templates::new(g, &params, &p.id, &see, &seed);
        let failed = eval_checks(schema, g, &def.conditions, &env, p);
        if !failed.is_empty() {
            return Ok(Verdict::NotMet(failed));
        }
        let mut out = Prepared::default();
        for (i, e) in def.effects.iter().enumerate() {
            if let Some(a) = &e.action {
                let rendered = env.render_strict(&Value::Object(a.params.clone()))?;
                let rendered = rendered.as_object().cloned().unwrap_or_default();
                let plan = self.plan_on_as(g, schema, &ctx, &a.ty, &rendered, &seed, also)?;
                if !plan.valid {
                    let mut why: Vec<String> =
                        plan.checks.iter().filter(|c| !c.ok).map(|c| c.message.clone()).collect();
                    if why.is_empty() {
                        why.push("nothing to change".into());
                    }
                    return Ok(Verdict::Invalid(why));
                }
                if plan.needs_confirmation {
                    return Ok(Verdict::NeedsConfirmation(format!(
                        "action {} is risk {}{}: automations run only low-risk, reversible action types, or action \
                         types that declare unattended = [\"automation\"] for their external writes (a person applies \
                         it with confirm=true)",
                        a.ty,
                        format!("{:?}", plan.risk).to_lowercase(),
                        if plan.reversible { "" } else { ", not reversible" }
                    )));
                }
                // External writes (writeback) commit as outbox rows with the run.
                let (rows, external) = writeback_rows(&receipt_id, &plan, &ctx, SOURCE_AUTOMATION, Some(&run.id));
                let receipt = ActionReceipt {
                    id: receipt_id.clone(),
                    action: a.ty.clone(),
                    status: "applied".into(),
                    actor: p.id.clone(),
                    workspace: ctx.workspace.clone(),
                    request_id: ctx.request_id.clone(),
                    ts,
                    plan: plan.clone(),
                    result: None,
                    reason: None,
                    touched: Default::default(),
                    note: (!external.is_empty()).then(|| crate::action_types::RECEIPT_NOTE.to_string()),
                    external,
                };
                out.mutations.extend(plan.mutations.iter().cloned());
                let mut payload =
                    json!({ "params": plan.params, "receipt": receipt_id, "risk": plan.risk, "run": run.id });
                if !rows.is_empty() {
                    payload["external"] = json!(rows.iter().map(|r| &r.id).collect::<Vec<_>>());
                }
                out.mutations.push(Mutation::Event(EventInput {
                    action: format!("action.{}", a.ty),
                    target: audit_target(&plan.params),
                    payload,
                    ..Default::default()
                }));
                out.effects.push(RunEffect {
                    index: i,
                    kind: "action".into(),
                    target: a.ty.clone(),
                    ok: true,
                    receipt: Some(receipt_id.clone()),
                    plan: Some(json!({
                        "preview": plan.preview, "risk": plan.risk, "reversible": plan.reversible,
                        "warnings": plan.warnings, "external": plan.external,
                    })),
                    ..Default::default()
                });
                out.outbox.extend(rows);
                out.plan = Some(plan);
                out.receipt = Some(receipt);
            }
            if let Some(w) = &e.webhook {
                let ep = endpoints
                    .iter()
                    .find(|r| r.name == w.endpoint)
                    .ok_or_else(|| Error::not_found(format!("webhook endpoint {}", w.endpoint)))?;
                let ep_principal = ep.principal.principal(format!("webhook:{}", ep.name));
                if !dominates(&ep_principal, &def.run_as) {
                    return Err(Error::Denied(format!(
                        "webhook endpoint {} may see less than automation {} renders (its principal does not \
                         dominate run_as)",
                        ep.name, run.automation
                    )));
                }
                let payload = env.render_strict(&w.payload)?;
                let mut row = OutboxRow::new(
                    OutboxKind::Webhook,
                    &w.endpoint,
                    format!("{}:{i}", run.id),
                    SOURCE_AUTOMATION,
                    ctx.workspace.clone(),
                    format!("onto.{}", w.event),
                );
                row.run = Some(run.id.clone());
                row.receipt = def.action().map(|_| receipt_id.clone());
                row.cause = ctx.cause.clone();
                row.otel_trace_id = ctx.trace_id.clone();
                row.payload = payload.clone();
                out.effects.push(RunEffect {
                    index: i,
                    kind: "webhook".into(),
                    target: w.endpoint.clone(),
                    ok: true,
                    outbox: Some(row.id.clone()),
                    plan: Some(json!({ "event": row.event_type, "payload": payload })),
                    ..Default::default()
                });
                out.outbox.push(row);
            }
        }
        Ok(Verdict::Ready(Box::new(out)))
    }

    /// Evaluate `run` of `def` on the current graph without writing (the
    /// consumer's check before it claims a run, and dry runs with `also`).
    pub fn evaluate_run(&self, run: &RunRow, def: &Automation, also: Option<&Principal>) -> Result<Verdict> {
        let endpoints = automate::outbound_refs(&**self.store(), NS_WEBHOOK_ENDPOINT)?;
        self.read(|g, schema| self.evaluate_on(g, schema, run, def, &endpoints, also))
    }

    /// Commit `run` (a claimed `running` row) of `def`: evaluated again on
    /// the locked graph, then the action's edits and receipt, the webhook
    /// outbox rows, the `automation.run` event and the finished run row in
    /// one commit, fenced by `fence`. A condition or precondition that no
    /// longer holds makes the run `failed: precondition_changed`; a plan that
    /// needs confirmation makes it `refused`; shadow mode records `would_run`
    /// with the plan and writes nothing else. The definition evaluated is
    /// the locked schema's (`def` names the run context only); a deleted
    /// automation or a closure that no longer hashes as `run.closure_hash`
    /// makes the run `cancelled: stale_definition: …`. Errors (a lost lease,
    /// unavailability, an invalid template, a refused endpoint) leave the row
    /// as it was: the caller decides between retry and failure
    /// ([`execute_run`](Self::execute_run)).
    pub fn commit_run(&self, run: &RunRow, def: &Automation, fence: Option<&Fence>) -> Result<RunResult> {
        let ctx = self.run_ctx(run, def);
        let en = Enablements::load(&**self.store())?;
        let (result, (row, receipt)) = self.transact(&ctx, |o, g| {
            let schema = o.schema();
            let mut row = run.clone();
            let mut muts = Vec::new();
            let mut extra = Extra::default();
            let mut receipt = None;
            // The definition the commit runs is the locked schema's, and only
            // while its closure is still the one the run was enabled under
            // (C.5): a schema or outbound change since the round started
            // cancels the run, and the next round pauses the automation.
            let verdict = match stale_at_commit(&schema, &en, run) {
                Some(why) => Err(why),
                None => {
                    let def = &schema.automations[&run.automation];
                    Ok(o.evaluate_on(g, &schema, run, def, &en.endpoints, None)?)
                }
            };
            let mode = schema.automations.get(&run.automation).map_or(def.mode, |d| d.mode);
            let verdict = match verdict {
                Err(why) => {
                    row.state = RunState::Cancelled;
                    row.reason = Some(format!("cancelled: {why}"));
                    None
                }
                Ok(v) => Some(v),
            };
            match verdict {
                None => {}
                Some(Verdict::Ready(p)) if mode == Mode::Shadow => {
                    row.state = RunState::WouldRun;
                    row.reason = Some("shadow mode: nothing was written".into());
                    row.effects =
                        p.effects.into_iter().map(|e| RunEffect { receipt: None, outbox: None, ..e }).collect();
                }
                Some(Verdict::Ready(p)) => {
                    let p = *p;
                    row.state = RunState::Succeeded;
                    row.reason = None;
                    row.effects = p.effects.into_iter().map(|e| RunEffect { plan: None, ..e }).collect();
                    muts = p.mutations;
                    if let Some(r) = &p.receipt {
                        extra.kv.push((RECEIPTS.into(), r.id.clone(), serde_json::to_value(r)?));
                    }
                    extra.outbox = p.outbox;
                    receipt = p.receipt;
                }
                Some(Verdict::NotMet(why) | Verdict::Invalid(why)) => {
                    row.state = RunState::Failed;
                    row.reason = Some(format!("{PRECONDITION_CHANGED}: {}", why.join("; ")));
                }
                Some(Verdict::NeedsConfirmation(why)) => {
                    row.state = RunState::Refused;
                    row.reason = Some(format!("needs_confirmation: {why}"));
                }
            }
            row.finished_at = Some(now_ms());
            extra.events.push(o.event(
                &ctx,
                &ctx.workspace,
                "automation.run",
                Some(&run.principal),
                now_ms(),
                row.summary(),
                Vec::new(),
            ));
            extra.runs = vec![row.clone()];
            extra.fence = fence.cloned();
            Ok((muts, extra, (row, receipt)))
        })?;
        let receipt = match receipt {
            Some(mut r) => {
                r.touched = self.read(|g, _| {
                    result.entities.iter().map(|id| (id.clone(), g.get(id).map(|e| e.revision))).collect()
                });
                r.result = Some(result);
                // The result too (best effort; the receipt itself committed with the run).
                let _ = self.store().kv_put(RECEIPTS, &r.id, &serde_json::to_value(&r)?, None);
                Some(r)
            }
            None => None,
        };
        Ok(RunResult { run: row, receipt })
    }

    /// Write `row` (an update of a `running` row: retry, failed, cancelled)
    /// with its `automation.run` event in one commit, fenced by `fence`. The
    /// store refuses it when the row is no longer `running`.
    pub fn finish_run(&self, row: &RunRow, def: Option<&Automation>, fence: Option<&Fence>) -> Result<()> {
        self.settle_run(row, def, fence, Vec::new())
    }

    /// Move `row` (a `running` row) to `retry` with its `retry:<run>` timer,
    /// due after `delay_ms`, and its `automation.run` event in one commit,
    /// fenced by `fence`: a run is never left in `retry` without the timer
    /// that resumes it.
    pub fn retry_run(
        &self,
        row: &RunRow,
        def: Option<&Automation>,
        fence: Option<&Fence>,
        delay_ms: i64,
    ) -> Result<()> {
        let timer = (
            format!("{RETRY_PREFIX}{}", row.id),
            delay_ms.max(0),
            json!({ "run": row.id, "automation": row.automation }),
        );
        self.settle_run(row, def, fence, vec![timer])
    }

    fn settle_run(
        &self,
        row: &RunRow,
        def: Option<&Automation>,
        fence: Option<&Fence>,
        timers: Vec<(String, i64, Value)>,
    ) -> Result<()> {
        let ctx = match def {
            Some(d) => self.run_ctx(row, d),
            None => {
                let mut c = ActionContext::system(self.options().default_workspace.clone())
                    .with_request_id(row.id.clone())
                    .with_intent(format!("{PRINCIPAL_PREFIX}{}", row.automation));
                c.cause = Some(RunCause { automation: row.automation.clone(), run: row.id.clone(), depth: row.depth });
                c
            }
        };
        let mut row = row.clone();
        if row.state.is_final() {
            row.finished_at = Some(now_ms());
        }
        self.transact(&ctx, |o, _| {
            let ev = o.event(
                &ctx,
                &ctx.workspace,
                "automation.run",
                Some(&row.principal),
                now_ms(),
                row.summary(),
                Vec::new(),
            );
            Ok((
                Vec::new(),
                Extra {
                    events: vec![ev],
                    runs: vec![row.clone()],
                    fence: fence.cloned(),
                    timers: timers.clone(),
                    ..Default::default()
                },
                (),
            ))
        })
        .map(drop)
    }

    /// Insert final rows (skipped, refused, failed) the consumer settled
    /// before any execution, with one `automation.run` event each, and
    /// return the ids inserted (a trigger already claimed inserts nothing).
    pub fn record_runs(&self, rows: &[RunRow], fence: Option<&Fence>) -> Result<Vec<String>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let ids = self.store().run_claim(rows, fence)?;
        let inserted: Vec<&RunRow> = rows.iter().filter(|r| ids.contains(&r.id)).collect();
        if !inserted.is_empty() {
            let ctx = ActionContext::system(self.options().default_workspace.clone()).with_intent("automation.run");
            self.transact(&ctx, |o, _| {
                let events = inserted
                    .iter()
                    .map(|r| {
                        let ws =
                            if r.trigger.workspace.is_empty() { ctx.workspace.as_str() } else { &r.trigger.workspace };
                        let mut ev =
                            o.event(&ctx, ws, "automation.run", Some(&r.principal), now_ms(), r.summary(), Vec::new());
                        ev.actor = r.principal.clone();
                        ev.request_id = Some(r.id.clone());
                        ev
                    })
                    .collect();
                Ok((Vec::new(), Extra { events, ..Default::default() }, ()))
            })?;
        }
        Ok(ids)
    }

    /// Execute a claimed `running` row once (one more attempt), fenced by
    /// `fence`: commit it, or settle it. `Unavailable` and contention
    /// `Conflict` errors move it to `retry` with a timer (`retry:<run>`, due
    /// after `min(base * 2^(attempt - 1), max)`, committed with the row) until `max_attempts`; every
    /// other error makes it `failed` with the error (which never holds a
    /// secret or a resolved reference). A lost lease is returned: the row
    /// stays `running` for the next holder's recovery. A row another
    /// execution settled is returned as stored.
    pub fn execute_run(&self, run: &RunRow, def: &Automation, fence: &Fence, retry: &RetryPolicy) -> Result<RunRow> {
        let mut run = run.clone();
        run.attempts += 1;
        run.epoch = fence.epoch;
        match self.commit_run(&run, def, Some(fence)) {
            Ok(r) => Ok(r.run),
            Err(e) if is_lease_lost(&e) => Err(e),
            Err(e) if is_settled(&e) => Ok(self.store().run_get(&run.id)?.unwrap_or(run)),
            Err(e) if retryable(&e) && run.attempts < retry.max_attempts => {
                let delay = retry.delay_ms(run.attempts);
                let row = run.with_state(RunState::Retry, Some(format!("retry in {} s: {e}", delay / 1000)));
                // The row and its timer commit together; if this fails too,
                // the row stays `running` and the next round recovers it.
                match self.retry_run(&row, Some(def), Some(fence), delay) {
                    Ok(()) => Ok(row),
                    Err(e) if is_settled(&e) => Ok(self.store().run_get(&row.id)?.unwrap_or(row)),
                    Err(e) => Err(e),
                }
            }
            Err(e) => {
                let reason =
                    if retryable(&e) { format!("gave up after {} attempts: {e}", run.attempts) } else { e.to_string() };
                let row = run.with_state(RunState::Failed, Some(reason));
                match self.finish_run(&row, Some(def), Some(fence)) {
                    Ok(()) => Ok(row),
                    Err(e) if is_settled(&e) => Ok(self.store().run_get(&row.id)?.unwrap_or(row)),
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// Recovery by the holder of the consumer lease (`fence`), at the start
    /// of every round: every `running` row of its epoch or an older one is
    /// executed again, or cancelled when its automation is no longer active.
    /// Rounds of one holder never overlap, so a `running` row at the start of
    /// a round is one whose execution ended in an error (or whose process
    /// stopped): nothing of its commit happened, so this completes each one
    /// at most once. The same holder keeps its epoch when it renews, or takes
    /// the lease again after losing it, so its own epoch is included.
    /// Returns the rows as they ended.
    pub fn recover_runs(&self, fence: &Fence, retry: &RetryPolicy, killed_process: bool) -> Result<Vec<RunRow>> {
        let mut out = Vec::new();
        loop {
            let rows = self.store().runs_running(fence.epoch.saturating_add(1), 100)?;
            if rows.is_empty() {
                break;
            }
            let schema = self.schema();
            let en = Enablements::load(&**self.store())?;
            let kill = killed_process || killed(&**self.store())?;
            for row in rows {
                let st = status(&schema, &en, &row.automation, now_ms(), kill);
                let done = match (st, schema.automations.get(&row.automation)) {
                    (Status::Active, Some(def)) => self.execute_run(&row, def, fence, retry)?,
                    (Status::Inactive(why), def) => {
                        let mut r = row.clone().with_state(RunState::Cancelled, Some(format!("cancelled: {why}")));
                        r.epoch = fence.epoch;
                        match self.finish_run(&r, def, Some(fence)) {
                            Ok(()) => r,
                            Err(e) if is_settled(&e) => self.store().run_get(&r.id)?.unwrap_or(r),
                            Err(e) => return Err(e),
                        }
                    }
                    (Status::Active, None) => unreachable!("an active automation is defined"),
                };
                out.push(done);
            }
        }
        Ok(out)
    }

    /// Write the enablement of automations (KV `automation.state`) or the
    /// tenant switch (`automation.control`) with its audit event in one
    /// commit. People who may approve schema changes (and admins), never
    /// agents; callers check dominance of `run_as` first.
    pub fn commit_automation(
        &self,
        ctx: &ActionContext,
        kv: Vec<(String, String, Value)>,
        event: crate::outbox::ConfigEvent,
    ) -> Result<crate::model::MutationResult> {
        let p = &ctx.principal;
        let gov = &self.options().governance;
        if p.is_agent() || !(p.is_privileged() || gov.can_approve(p)) {
            return Err(self.refuse_configure(
                ctx,
                format!("{} may not change automations (approvers and admins, never agents)", p.id),
            ));
        }
        let allowed = [automate::NS_AUTOMATION_STATE, NS_AUTOMATION_CONTROL];
        if let Some((ns, _, _)) = kv.iter().find(|(ns, _, _)| !allowed.contains(&ns.as_str())) {
            return Err(Error::invalid(format!("commit_automation writes only {} (not {ns})", allowed.join(", "))));
        }
        let (res, ()) = self.transact(ctx, |o, _| {
            let ev = o.event(
                ctx,
                &ctx.workspace,
                &event.action,
                event.target.as_deref(),
                now_ms(),
                event.payload.clone(),
                Vec::new(),
            );
            Ok((Vec::new(), Extra { kv: kv.clone(), events: vec![ev], ..Default::default() }, ()))
        })?;
        Ok(res)
    }

    /// The stored enablement of `name`, if any.
    pub fn automation_state(&self, name: &str) -> Result<Option<AutomationState>> {
        match self.store().kv_get(automate::NS_AUTOMATION_STATE, name)? {
            None | Some(Value::Null) => Ok(None),
            Some(v) => serde_json::from_value(v)
                .map(Some)
                .map_err(|e| Error::Storage(format!("corrupt automation state {name}: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_and_finality() {
        for s in RunState::ALL {
            assert_eq!(s.as_str().parse::<RunState>().unwrap(), s);
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.as_str()));
        }
        assert!("done".parse::<RunState>().is_err());
        assert!(!RunState::Running.is_final() && !RunState::Retry.is_final());
        assert!(RunState::Skipped.is_final() && RunState::WouldRun.is_final());
    }

    #[test]
    fn retry_delays_double_up_to_the_maximum() {
        let r = RetryPolicy::default();
        assert_eq!(r.delay_ms(1), 10_000);
        assert_eq!(r.delay_ms(2), 20_000);
        assert_eq!(r.delay_ms(4), 80_000);
        assert_eq!(r.delay_ms(30), 3_600_000);
        assert_eq!(r.delay_ms(u32::MAX), 3_600_000);
    }

    #[test]
    fn only_unavailability_and_contention_are_retried() {
        assert!(retryable(&Error::Unavailable("postgres: pool timed out".into())));
        assert!(retryable(&Error::Conflict("write kept losing to concurrent writers".into())));
        assert!(!retryable(&Error::Unavailable("lease lost: automation:consumer".into())));
        assert!(is_lease_lost(&Error::Unavailable("lease lost: x".into())));
        assert!(!retryable(&settled("run_1")) && is_settled(&settled("run_1")));
        assert!(!retryable(&Error::invalid("invalid_template: no value for {subject.title}")));
        assert!(!retryable(&Error::Denied("no".into())));
        assert!(!retryable(&Error::Storage("disk".into())));
    }

    #[test]
    fn triggers_name_the_subject_and_rows_key_on_the_cursor() {
        let r: ChangeRecord = serde_json::from_value(json!({
            "cursor": "7.2", "ts": 0, "op": "link.create", "workspace": "acme", "kind": "blocks",
            "id": "issue:1|blocks|issue:2",
            "link": { "from": "issue:1", "rel": "blocks", "to": "issue:2", "ends": [
                { "id": "issue:1", "kind": "issue", "workspace": "acme" },
                { "id": "issue:2", "kind": "issue", "workspace": "acme" }
            ] },
            "cause": { "actor": "ada", "automation": "x", "run": "run_9", "depth": 2 },
        }))
        .unwrap();
        let t = RunTrigger::of(&r);
        assert_eq!((t.subject.as_str(), t.depth, t.on.as_str()), ("issue:1", 2, "change"));
        let def: Automation = serde_json::from_value(json!({
            "run_as": { "roles": ["member"], "workspaces": ["acme"] },
            "trigger": { "on": "change" },
            "effects": [{ "webhook": { "endpoint": "ops", "event": "x" } }]
        }))
        .unwrap();
        let row = RunRow::new("a", "h", 3, &def, t);
        assert_eq!((row.trigger_key.as_str(), row.depth, row.principal.as_str()), ("7.2", 3, "automation:a"));
        assert!(row.id.starts_with("run_") && row.receipt_id().starts_with("act_"));
        assert_eq!(row.receipt_id(), format!("act_{}", &row.id[4..]));
        let q = RunQuery { automation: Some("a".into()), states: vec![RunState::Running], ..Default::default() };
        assert!(q.matches(&row));
        assert!(!RunQuery { states: vec![RunState::Failed], ..Default::default() }.matches(&row));
        assert_eq!(RunQuery { limit: Some(10_000), ..Default::default() }.limit(), MAX_LIST);
    }

    #[test]
    fn runs_continue_the_trace_of_their_trigger_else_their_own() {
        const OTEL: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
        // A call's principal carries its trace into contexts, change causes
        // and, through the trigger, the run's context.
        let p = Principal::admin("ada").with_trace(Some(OTEL));
        let ctx = ActionContext::new(p, "acme");
        assert_eq!(ctx.trace_id.as_deref(), Some(OTEL));
        let cause = crate::changes::Cause::of(&ctx);
        assert_eq!(cause.trace_id.as_deref(), Some(OTEL));
        assert!(Principal::admin("x").with_trace(Some("not-a-trace")).trace_id.is_none());
        assert!(Principal::admin("x").with_trace(Some(&"0".repeat(32))).trace_id.is_none(), "all-zero is invalid");
        let mut r: ChangeRecord = serde_json::from_value(json!({
            "cursor": "3.0", "ts": 0, "op": "entity.update", "workspace": "acme", "kind": "issue", "id": "issue:1",
        }))
        .unwrap();
        r.cause = cause;
        let def: Automation = serde_json::from_value(json!({
            "run_as": { "roles": ["member"], "workspaces": ["acme"] },
            "trigger": { "on": "change" },
            "effects": [{ "webhook": { "endpoint": "ops", "event": "x" } }]
        }))
        .unwrap();
        let row = RunRow::new("a", "h", 1, &def, RunTrigger::of(&r));
        let own = row.trace_id.clone().unwrap();
        assert!(crate::trace::is_trace_id(&own) && own != OTEL, "every run has a trace of its own");
        assert_eq!(row.continues_trace().as_deref(), Some(OTEL));
        // A trigger written outside any trace: the run's own trace continues.
        r.cause.trace_id = None;
        let row = RunRow::new("a", "h", 1, &def, RunTrigger::of(&r));
        assert_eq!(row.continues_trace(), row.trace_id);
        // Not serialized from a principal, and kept on contexts as data.
        assert!(!serde_json::to_string(&Principal::admin("ada").with_trace(Some(OTEL))).unwrap().contains(OTEL));
    }
}
