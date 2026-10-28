//! Automate (docs/design/EVENTS.md Part C): the automation consumer and the
//! `automation.*` actions.
//!
//! * **Consumer.** One process per tenant at a time holds the lease
//!   `automation:consumer` (store clock, renewed every round); its writes are
//!   fenced by the lease epoch. It reads change pages after its cursor and,
//!   for each record an active automation's trigger admits (as the run
//!   principal sees it), applies the guards, then claims a run
//!   (`INSERT … ON CONFLICT (automation, trigger_key) DO NOTHING`) and
//!   commits it ([`onto_core::Ontology::commit_run`]): graph edits, receipt,
//!   outbox rows and the finished run row in one commit. A duplicate trigger
//!   is settled by the claim and never retried.
//! * **Recovery.** A new lease holder executes every `running` row of an
//!   older epoch again: nothing of their commit happened.
//! * **Guards.** Depth (`min(max_depth, [automation] max_depth)`, refused),
//!   rate limits per tenant and automation per minute and the per-subject
//!   budget per hour (`skipped`, rerunnable), self-triggering
//!   (`allow_self`), migrations (`include_migrations`); gap records never
//!   start runs. Conditions that do not hold start no run; a plan that is
//!   not valid or needs a person's confirmation is `refused`.
//! * **Retries.** `Unavailable` and contention `Conflict` move a run to
//!   `retry` with a timer (`retry:<run>`, `min(base * 2^(attempt - 1), max)`)
//!   until `retry_max_attempts`; everything else is final. The breaker
//!   pauses an automation after `pause_after_failures` consecutive failed
//!   runs.
//! * **Enablement.** A person who may approve schema changes and dominates
//!   `run_as` enables an automation; it binds to the closure hash and
//!   expires after `[automation] enable_ttl_days` (phase 1 declares
//!   `automation:<name>` principals locally until Tutela issues them). A
//!   changed closure (`stale_definition`) or an expired enablement makes it
//!   inactive, with one event, until a person enables it again.
//! * **Kill switches.** `[automation] enabled = false` (this process runs
//!   none), `automation.control/kill` (the tenant: changes made while it is
//!   on never start runs, and pending retries are cancelled), and
//!   `automation.disable` (one).
//!
//! Webhook effects are enqueued once and delivered at least once; external
//! effects are not reversible. Automation definitions are visible to
//! everyone who can read the schema.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;

use onto_core::automate::{
    self, Automation, AutomationState, Budget, Enablements, MEANS, NS_AUTOMATION_CONTROL, NS_AUTOMATION_STATE, Paused,
    TRIGGER_OPS,
};
use onto_core::changes::{
    self, ChangeAt, ChangeFilter, ChangeOp, ChangeRecord, Cursor, FieldMode, FilterEval, MAX_PAGE,
};
use onto_core::leases::{Fence, holder_for};
use onto_core::outbox::ConfigEvent;
use onto_core::policy::dominates;
use onto_core::runs::{
    self, CONSUMER, CONSUMER_LEASE, KILL_KEY, RETRY_PREFIX, RetryPolicy, RunQuery, RunRow, RunState, RunTrigger,
    Status, Verdict,
};
use onto_core::time::now_ms;
use onto_core::{Access, ActionContext, Error, Ontology, Principal, Result, one_or_many};

use crate::actions::parse;
use crate::intelli::Intelli;

/// The automation actions, dispatched by [`Intelli::automate_call`].
pub const CALLS: &[&str] = &[
    "automation.list",
    "automation.get",
    "automation.runs",
    "automation.test",
    "automation.enable",
    "automation.disable",
    "automation.rerun",
    "automation.kill",
];

/// Records evaluated under one graph read lock.
const SLICE: usize = 64;
/// Retry timers handled per round.
const RETRIES_PER_ROUND: usize = 32;
/// Words for outputs about runs (docs/design/EVENTS.md section 13).
pub const RUN_NOTE: &str = "A run's graph edits, receipt, outbox rows and run row commit together. Webhook effects \
are enqueued once and delivered at least once; external effects are not reversible.";

/// The consumer's state in this process: its lease holder id, the epoch it
/// last held (a new epoch means recovery is due) and its run budgets.
pub(crate) struct Runtime {
    holder: Option<String>,
    epoch: Option<u64>,
    budget: Budget,
}

impl Runtime {
    pub(crate) fn new() -> Self {
        Self { holder: None, epoch: None, budget: Budget::default() }
    }
}

/// What one consumer round did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RoundReport {
    /// Nothing to do here (no enabled automation, the process switch is off,
    /// or another process holds the lease).
    pub idle: bool,
    /// A full page: go again now.
    pub more: bool,
    /// `running` rows of an older epoch executed again.
    pub recovered: usize,
    /// Runs resumed from their retry timers.
    pub retried: usize,
    /// Runs claimed and executed.
    pub executed: usize,
    /// Rows settled without executing (skipped, refused, failed).
    pub settled: usize,
    /// Triggers another claim had already settled.
    pub duplicates: usize,
}

/// Start the automation consumer of `me` (stores that keep a change log,
/// runs and fenced leases). It holds the tenant only weakly and ends when
/// the tenant closes. It wakes on every commit (`notify`), when a retry timer
/// is due, and every third of the lease.
pub fn spawn_automation_workers(me: &Arc<Intelli>, notify: Arc<Notify>) {
    let store = me.ontology().store().clone();
    if !me.config().automation.workers || !store.keeps_runs() || !store.keeps_changes() || !store.fences() {
        return;
    }
    let weak = Arc::downgrade(me);
    let poll = Duration::from_secs((me.config().automation.lease_secs / 3).max(1));
    let mut closing = me.closing_signal();
    tokio::spawn(async move {
        loop {
            let (again, wait) = {
                let Some(me) = weak.upgrade() else { break };
                if me.is_closing() {
                    break;
                }
                let me2 = me.clone();
                let again = match tokio::task::spawn_blocking(move || me2.automation_round()).await {
                    Ok(Ok(r)) => r.more,
                    Ok(Err(e)) => {
                        tracing::warn!("automation consumer: {e}");
                        false
                    }
                    Err(e) => {
                        tracing::warn!("automation consumer: {e}");
                        false
                    }
                };
                // Wake for the next retry timer if it is due before the poll.
                let store = me.ontology().store().clone();
                let wait = match (store.timers_next(), store.clock_ms()) {
                    (Ok(Some(due)), Ok(now)) => Duration::from_millis((due - now).clamp(0, 3_600_000) as u64).min(poll),
                    _ => poll,
                };
                (again, wait)
            };
            if again {
                tokio::task::yield_now().await;
                continue;
            }
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(wait.max(Duration::from_millis(50))) => {}
                _ = closing.changed() => {}
            }
        }
    });
}

/// The position just before the floor's commit (the oldest records kept).
fn floor_edge(floor: u64) -> Cursor {
    if floor == 0 { Cursor::default() } else { Cursor::new(floor - 1, u32::MAX) }
}

/// One automation taking part in a round.
struct Active {
    name: String,
    def: Automation,
    principal: Principal,
    filter: ChangeFilter,
    closure_hash: String,
    from: Option<Cursor>,
}

/// A system context for the consumer's own writes (pauses, events).
fn system_ctx(onto: &Ontology) -> ActionContext {
    ActionContext::system(onto.options().default_workspace.clone()).with_intent("automation.consumer")
}

impl Intelli {
    fn retry_policy(&self) -> RetryPolicy {
        let c = &self.config().automation;
        RetryPolicy {
            base_ms: (c.retry_base_secs as i64).saturating_mul(1000),
            max_ms: (c.retry_max_secs as i64).saturating_mul(1000),
            max_attempts: c.retry_max_attempts,
        }
    }

    /// Mark an enabled automation inactive (stale closure, expired
    /// enablement, failing): its state records why, with one event.
    fn pause_automation(&self, name: &str, st: &AutomationState, reason: &str, action: &str) -> Result<()> {
        let onto = self.ontology();
        let mut st = st.clone();
        st.paused = Some(Paused { reason: reason.to_string(), since: now_ms() });
        let kv = vec![(NS_AUTOMATION_STATE.to_string(), name.to_string(), serde_json::to_value(&st)?)];
        let target = format!("{}{name}", automate::PRINCIPAL_PREFIX);
        onto.commit_automation(
            &system_ctx(onto),
            kv,
            ConfigEvent::new(action, Some(&target), json!({ "reason": reason })),
        )
        .map(drop)
    }

    /// After a run failed: `pause_after_failures` consecutive failed runs
    /// (among runs that executed) pause the automation.
    fn breaker(&self, name: &str) -> Result<()> {
        let n = self.config().automation.pause_after_failures.max(1) as usize;
        let q = RunQuery {
            automation: Some(name.to_string()),
            states: vec![RunState::Succeeded, RunState::Failed, RunState::WouldRun],
            limit: Some(n),
            ..Default::default()
        };
        let last = self.ontology().store().runs_list(&q)?;
        if last.len() >= n && last.iter().all(|r| r.state == RunState::Failed) {
            let onto = self.ontology();
            if let Some(st) = onto.automation_state(name)?.filter(|s| s.enabled && s.paused.is_none()) {
                self.pause_automation(
                    name,
                    &st,
                    &format!("failing: {n} consecutive failed runs"),
                    "automation.paused",
                )?;
            }
        }
        Ok(())
    }

    /// Execute a claimed row and apply the breaker when it failed.
    fn execute(&self, row: &RunRow, def: &Automation, fence: &Fence) -> Result<RunRow> {
        let done = self.ontology().execute_run(row, def, fence, &self.retry_policy())?;
        if done.state == RunState::Failed {
            self.breaker(&done.automation)?;
        }
        Ok(done)
    }

    /// One round of the automation consumer: recovery when this process
    /// newly holds the lease, due retries, then one page of the change feed.
    /// Synchronous (store calls block); the worker runs it off the async
    /// runtime. Tests and tools may call it directly.
    pub fn automation_round(&self) -> Result<RoundReport> {
        let mut rt = self.automation_runtime().lock();
        let onto = self.ontology();
        let store = onto.store().clone();
        let cfg = self.config().automation.clone();
        let mut report = RoundReport::default();
        if !store.keeps_runs() || !store.keeps_changes() || !store.fences() || !cfg.enabled {
            report.idle = true;
            return Ok(report);
        }
        let schema = onto.schema();
        let en = Enablements::load(&*store)?;
        let now = now_ms();
        let kill = runs::killed(&*store)?;
        let mut actives: Vec<Active> = Vec::new();
        let mut enabled_any = false;
        for (name, st) in en.states.iter().filter(|(_, s)| s.enabled) {
            if st.paused.is_none() {
                enabled_any = true;
            }
            match runs::status(&schema, &en, name, now, kill) {
                Status::Active => {
                    let def = schema.automations[name].clone();
                    actives.push(Active {
                        principal: def.principal(name),
                        filter: def.change_trigger().filter(),
                        closure_hash: st.closure_hash.clone(),
                        from: st.from,
                        name: name.clone(),
                        def,
                    });
                }
                Status::Inactive(why) if st.paused.is_none() && why.starts_with("stale_definition") => {
                    self.pause_automation(name, st, &why, "automation.stale")?;
                }
                Status::Inactive(why) if st.paused.is_none() && why == "enablement_expired" => {
                    self.pause_automation(name, st, &why, "automation.expired")?;
                }
                Status::Inactive(_) => {}
            }
        }
        onto.set_wake_for_automations(enabled_any && !kill);
        if actives.is_empty() && !kill && store.runs_running(u64::MAX, 1)?.is_empty() && !self.retries_due()? {
            report.idle = true;
            return Ok(report);
        }
        let holder = rt.holder.get_or_insert_with(|| holder_for(&*store)).clone();
        let ttl = (cfg.lease_secs as i64).saturating_mul(1000);
        let Some(epoch) = store.lease(CONSUMER_LEASE, &holder, ttl)? else {
            report.idle = true;
            return Ok(report);
        };
        let fence = Fence::new(CONSUMER_LEASE, &holder, epoch);
        let retry = self.retry_policy();
        if rt.epoch != Some(epoch) {
            // Newly held: what an earlier holder claimed but never committed.
            let recovered = onto.recover_runs(&fence, &retry, kill)?;
            report.recovered = recovered.len();
            for r in recovered.iter().filter(|r| r.state == RunState::Failed) {
                self.breaker(&r.automation)?;
            }
            rt.epoch = Some(epoch);
        }
        report.retried = self.run_retries(&fence, &actives)?;

        let meta = store.changes_meta()?;
        let stored = store.consumer_get(CONSUMER)?;
        if kill || actives.is_empty() {
            // Changes made while nothing is active (or the tenant kill
            // switch is on) never start runs: an enablement starts at the
            // head of the feed.
            if stored.is_none_or(|c| c < meta.head) {
                store.consumer_set(CONSUMER, meta.head, Some(&fence))?;
            }
            return Ok(report);
        }
        // The first round starts at the earliest enablement.
        let mut cursor = match stored {
            Some(c) => c,
            None => actives.iter().filter_map(|a| a.from).min().unwrap_or(meta.head),
        };
        let edge = floor_edge(meta.floor);
        if cursor < edge {
            tracing::warn!("automation consumer: its cursor {cursor} expired (floor {}); resuming there", meta.floor);
            cursor = edge;
        }
        // Nothing before the earliest enablement can start a run.
        if let Some(min_from) = actives.iter().map(|a| a.from.unwrap_or_default()).min()
            && cursor < min_from
        {
            cursor = min_from;
        }
        let limit = self.config().changes.page.clamp(1, MAX_PAGE);
        let page = store.changes_since(cursor, limit)?;
        report.more = page.changes.len() >= limit;
        let evals: Vec<FilterEval> = actives.iter().map(|a| FilterEval::new(&a.filter)).collect();
        // Records each automation's trigger admits, as its run principal sees them.
        let mut hits: Vec<(usize, ChangeRecord)> = Vec::new();
        for slice in page.changes.chunks(SLICE) {
            let any = slice.iter().any(|(s, _)| TRIGGER_OPS.contains(&s.record.op));
            if !any {
                continue;
            }
            onto.read(|g, schema| {
                for (stored, entity) in slice {
                    let r = &stored.record;
                    if !TRIGGER_OPS.contains(&r.op) {
                        continue;
                    }
                    for (i, a) in actives.iter().enumerate() {
                        if a.from.is_some_and(|f| r.cursor <= f) || !evals[i].may_match(schema, r) {
                            continue;
                        }
                        let Some(v) = evals[i].view_and_match(&a.principal, schema, g, stored, entity.clone()) else {
                            continue;
                        };
                        if a.def.admits(&a.name, schema, &v) {
                            hits.push((i, v));
                        }
                    }
                }
            });
        }
        let mut settled_rows: Vec<RunRow> = Vec::new();
        let mut seen: HashSet<(usize, Cursor)> = HashSet::new();
        for (i, r) in hits {
            if !seen.insert((i, r.cursor)) {
                continue;
            }
            let a = &actives[i];
            let mut row = RunRow::new(&a.name, &a.closure_hash, onto.schema_version(), &a.def, RunTrigger::of(&r));
            row.epoch = epoch;
            let limit_depth = a.def.depth_limit(cfg.max_depth);
            if row.depth > limit_depth {
                let why = format!("depth: the run would be at depth {}, over the limit {limit_depth}", row.depth);
                settled_rows.push(row.with_state(RunState::Refused, Some(why)));
                continue;
            }
            // Evaluated on the graph now; the commit evaluates it again.
            let verdict = match onto.evaluate_run(&row, &a.def, None) {
                Ok(v) => v,
                Err(e) if runs::retryable(&e) => Verdict::Ready(Box::default()),
                Err(e) => {
                    settled_rows.push(row.with_state(RunState::Failed, Some(e.to_string())));
                    continue;
                }
            };
            match verdict {
                Verdict::NotMet(_) => continue,
                Verdict::Invalid(why) => {
                    settled_rows
                        .push(row.with_state(RunState::Refused, Some(format!("plan_invalid: {}", why.join("; ")))));
                    continue;
                }
                Verdict::NeedsConfirmation(why) => {
                    settled_rows.push(row.with_state(RunState::Refused, Some(format!("needs_confirmation: {why}"))));
                    continue;
                }
                Verdict::Ready(_) => {}
            }
            let limits = a.def.rate.limits(
                cfg.max_runs_per_minute,
                cfg.default_rate_per_minute,
                cfg.default_per_subject_per_hour,
            );
            if let Err((over, n)) = rt.budget.admit(now_ms(), &a.name, &row.trigger.subject, &limits) {
                settled_rows.push(row.with_state(RunState::Skipped, Some(over.reason(n))));
                continue;
            }
            // Claim first, in its own statement: a duplicate is settled here.
            if store.run_claim(std::slice::from_ref(&row), Some(&fence))?.is_empty() {
                report.duplicates += 1;
                continue;
            }
            report.executed += 1;
            self.execute(&row, &a.def, &fence)?;
        }
        if !settled_rows.is_empty() {
            let ids = onto.record_runs(&settled_rows, Some(&fence))?;
            report.settled = ids.len();
            report.duplicates += settled_rows.len() - ids.len();
            let failed: HashSet<&str> = settled_rows
                .iter()
                .filter(|r| r.state == RunState::Failed && ids.contains(&r.id))
                .map(|r| r.automation.as_str())
                .collect();
            for name in failed {
                self.breaker(name)?;
            }
        }
        let last = page.changes.last().map(|(s, _)| s.record.cursor).unwrap_or(page.head);
        let to = last.max(cursor);
        if stored != Some(to) {
            store.consumer_set(CONSUMER, to, Some(&fence))?;
        }
        rt.budget.trim(now_ms());
        Ok(report)
    }

    /// Are any run retry timers due?
    fn retries_due(&self) -> Result<bool> {
        Ok(self.ontology().store().timers_due(1)?.iter().any(|t| t.name.starts_with(RETRY_PREFIX)))
    }

    /// Resume runs whose retry timer is due: active automations run them
    /// again (`retry` → `running` under this epoch); others are cancelled.
    fn run_retries(&self, fence: &Fence, actives: &[Active]) -> Result<usize> {
        let onto = self.ontology();
        let store = onto.store();
        let mut n = 0;
        for t in store.timers_due(RETRIES_PER_ROUND)? {
            let Some(id) = t.name.strip_prefix(RETRY_PREFIX) else { continue };
            let row = store.run_get(id)?.filter(|r| r.state == RunState::Retry);
            if let Some(row) = row {
                match actives.iter().find(|a| a.name == row.automation) {
                    Some(a) => {
                        let mut running = row.clone().with_state(RunState::Running, None);
                        running.epoch = fence.epoch;
                        if store.run_update(&running, &[RunState::Retry], Some(fence))? {
                            n += 1;
                            self.execute(&running, &a.def, fence)?;
                        }
                    }
                    None => {
                        let why = Some("cancelled: the automation is not active".to_string());
                        store.run_update(&row.with_state(RunState::Cancelled, why), &[RunState::Retry], Some(fence))?;
                    }
                }
            }
            store.timers_delete(&t.name, Some(t.due_at))?;
        }
        Ok(n)
    }

    /// Cancel pending retries (of `name`, or of every automation): rows
    /// waiting in `retry` become `cancelled` and their timers go. Returns how many.
    fn cancel_retries(&self, name: Option<&str>, why: &str) -> Result<usize> {
        let store = self.ontology().store();
        if !store.keeps_runs() {
            return Ok(0);
        }
        let q = RunQuery {
            automation: name.map(str::to_string),
            states: vec![RunState::Retry],
            limit: Some(runs::MAX_LIST),
            ..Default::default()
        };
        let mut n = 0;
        for r in store.runs_list(&q)? {
            let id = r.id.clone();
            if store.run_update(&r.with_state(RunState::Cancelled, Some(why.to_string())), &[RunState::Retry], None)? {
                n += 1;
            }
            store.timers_delete(&format!("{RETRY_PREFIX}{id}"), None)?;
        }
        Ok(n)
    }
}

// ── actions ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct NameIn {
    name: String,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct RunsIn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, alias = "states", deserialize_with = "one_or_many")]
    state: Vec<String>,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
    /// One run.
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
struct TestIn {
    name: String,
    /// Test against this entity as if it was just updated.
    #[serde(default)]
    id: Option<String>,
    /// Test against this change.
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Deserialize)]
struct RerunIn {
    id: String,
    /// The automation the run must belong to (HTTP paths name it).
    #[serde(default)]
    automation: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct KillIn {
    #[serde(default)]
    on: Option<bool>,
    #[serde(default)]
    off: Option<bool>,
    #[serde(default)]
    workspace: Option<String>,
}

fn defined<'a>(schema: &'a onto_core::Schema, name: &str) -> Result<&'a Automation> {
    schema.automations.get(name).ok_or_else(|| Error::not_found(format!("automation {name}")))
}

fn roles_text(def: &Automation) -> String {
    def.run_as.roles.iter().map(|r| r.as_str()).collect::<Vec<_>>().join("+")
}

/// May `p` read automation definitions (every schema reader)?
fn reader(p: &Principal) -> Result<()> {
    if !p.allows(Access::Read) {
        return Err(Error::Denied(format!("{} may not read automations", p.id)));
    }
    Ok(())
}

/// May `p` enable (and so rerun) automation `name`: an approver or admin,
/// never an agent, who dominates its `run_as` (section 6.1).
fn may_enable(onto: &Ontology, p: &Principal, def: &Automation) -> bool {
    let gov = &onto.options().governance;
    !p.is_agent() && (gov.can_approve(p) || p.is_privileged()) && dominates(p, &def.run_as)
}

fn enable_check(onto: &Ontology, ctx: &ActionContext, action: &str, name: &str, def: &Automation) -> Result<()> {
    let p = &ctx.principal;
    let gov = &onto.options().governance;
    if p.is_agent() || !(gov.can_approve(p) || p.is_privileged()) {
        return Err(onto.refuse_configure(
            ctx,
            format!("{} may not {action} automations (approvers and admins, never agents)", p.id),
        ));
    }
    if !dominates(p, &def.run_as) {
        return Err(onto.refuse_configure(
            ctx,
            format!(
                "{} may not {action} automation {name}: it runs as {} in {}, which {} does not cover",
                p.id,
                roles_text(def),
                def.run_as.workspaces.join(", "),
                p.id
            ),
        ));
    }
    Ok(())
}

impl Intelli {
    fn wake_automations(&self) {
        self.automation_wake().notify_one();
    }

    /// A run as the caller may see it: everything for callers who dominate
    /// the run principal (they could see what it saw), else `None`.
    fn run_view(&self, p: &Principal, schema: &onto_core::Schema, r: &RunRow) -> Option<RunRow> {
        let visible = match schema.automations.get(&r.automation) {
            Some(def) => dominates(p, &def.run_as),
            None => p.workspaces.iter().any(|w| w == "*") && p.is_privileged(),
        };
        visible.then(|| r.clone())
    }

    fn automation_view(&self, p: &Principal, name: &str, def: &Automation, en: &Enablements, kill: bool) -> Value {
        let schema = self.ontology().schema();
        let status = runs::status(&schema, en, name, now_ms(), kill || !self.config().automation.enabled);
        let st = en.states.get(name);
        json!({
            "name": name,
            "principal_id": format!("{}{name}", automate::PRINCIPAL_PREFIX),
            "description": def.description,
            "mode": def.mode,
            "run_as": def.run_as,
            "trigger": def.trigger,
            "effects": def.effects.iter().map(|e| match (&e.action, &e.webhook) {
                (Some(a), _) => json!({ "kind": "action", "target": a.ty }),
                (_, Some(w)) => json!({ "kind": "webhook", "target": w.endpoint, "event": w.event }),
                _ => json!({}),
            }).collect::<Vec<_>>(),
            "status": if status.is_active() { "active".to_string() } else { "inactive".to_string() },
            "reason": match &status { Status::Inactive(r) => Some(r.clone()), Status::Active => None },
            "enabled": st.is_some_and(|s| s.enabled),
            "enabled_by": st.filter(|s| s.enabled).map(|s| s.enabled_by.clone()),
            "enabled_at": st.filter(|s| s.enabled).map(|s| s.enabled_at),
            "expires_at": st.filter(|s| s.enabled).map(|s| s.expires_at),
            "paused": st.and_then(|s| s.paused.clone()),
            "may_enable": may_enable(self.ontology(), p, def),
        })
    }

    fn put_state(
        &self,
        ctx: &ActionContext,
        name: &str,
        st: &AutomationState,
        action: &str,
        payload: Value,
    ) -> Result<()> {
        let kv = vec![(NS_AUTOMATION_STATE.to_string(), name.to_string(), serde_json::to_value(st)?)];
        let target = format!("{}{name}", automate::PRINCIPAL_PREFIX);
        self.ontology().commit_automation(ctx, kv, ConfigEvent::new(action, Some(&target), payload)).map(drop)
    }

    /// The trigger record of a dry run: a change by cursor (as the caller
    /// sees it), or the entity `id` as if it was just updated.
    fn test_record(&self, p: &Principal, i: &TestIn) -> Result<ChangeRecord> {
        let onto = self.ontology();
        if let Some(c) = i.cursor.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            let cursor: Cursor = c.parse()?;
            return match onto.change_at(p, cursor, FieldMode::Full)? {
                ChangeAt::Visible(r) => Ok(*r),
                ChangeAt::NotVisible => Err(Error::not_found(format!("change {cursor}"))),
                ChangeAt::Expired => Err(Error::not_found(format!("change {cursor} (expired by retention)"))),
            };
        }
        let id = i.id.as_deref().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| {
            Error::invalid("automation.test: give id (an entity, as if it was just updated) or cursor (a change)")
        })?;
        let e = onto.get(p, id)?;
        Ok(serde_json::from_value(json!({
            "cursor": "0.0", "ts": now_ms(), "op": ChangeOp::EntityUpdate, "workspace": e.workspace,
            "kind": e.kind, "id": e.id, "revision": e.revision, "entity": e,
            "cause": { "actor": p.id },
        }))?)
    }

    /// The automation actions ([`CALLS`]). Synchronous: the store calls
    /// block, so the caller runs this off the async workers.
    pub(crate) fn automate_call(&self, p: &Principal, action: &str, input: Value) -> Result<Value> {
        let onto = self.ontology();
        let store = onto.store();
        match action {
            "automation.list" => {
                reader(p)?;
                let schema = onto.schema();
                let en = Enablements::load(&**store)?;
                let kill = runs::killed(&**store)?;
                let items: Vec<Value> =
                    schema.automations.iter().map(|(n, d)| self.automation_view(p, n, d, &en, kill)).collect();
                Ok(json!({
                    "items": items,
                    "kill": kill,
                    "process_enabled": self.config().automation.enabled,
                    "note": "Automation definitions are visible to everyone who can read the schema.",
                }))
            }
            "automation.get" => {
                let i: NameIn = parse(action, input)?;
                reader(p)?;
                let schema = onto.schema();
                let def = defined(&schema, i.name.trim())?;
                let name = i.name.trim();
                let en = Enablements::load(&**store)?;
                let kill = runs::killed(&**store)?;
                let closure = automate::closure(&schema, name, def, &en.endpoints, &en.targets);
                let changed = en.states.get(name).filter(|s| s.enabled).map(|s| s.bound().changed(&closure));
                let mut v = self.automation_view(p, name, def, &en, kill);
                v["definition"] = serde_json::to_value(def)?;
                v["closure"] =
                    json!({ "hash": closure.hash, "items": closure.items, "changed_since_enabled": changed });
                v["means"] = json!(MEANS);
                Ok(v)
            }
            "automation.runs" => {
                let i: RunsIn = parse(action, input)?;
                reader(p)?;
                if !store.keeps_runs() {
                    return Ok(json!({ "items": [], "hidden": 0, "note": RUN_NOTE }));
                }
                let schema = onto.schema();
                if let Some(id) = i.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    let name = i.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
                    let r = store
                        .run_get(id)?
                        .filter(|r| name.is_none_or(|n| r.automation == n))
                        .ok_or_else(|| Error::not_found(format!("run {id}")))?;
                    let r = self.run_view(p, &schema, &r).ok_or_else(|| Error::not_found(format!("run {id}")))?;
                    return Ok(json!({ "run": r, "note": RUN_NOTE }));
                }
                let states = i.state.iter().map(|s| s.trim().parse()).collect::<Result<Vec<RunState>>>()?;
                let q = RunQuery {
                    automation: i.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
                    states,
                    since: i.since,
                    limit: i.limit,
                };
                let mut hidden = 0usize;
                let mut items = Vec::new();
                for r in store.runs_list(&q)? {
                    match self.run_view(p, &schema, &r) {
                        Some(r) => items.push(r),
                        None => hidden += 1,
                    }
                }
                Ok(json!({ "items": items, "hidden": hidden, "note": RUN_NOTE }))
            }
            "automation.test" => {
                let i: TestIn = parse(action, input)?;
                reader(p)?;
                let schema = onto.schema();
                let name = i.name.trim().to_string();
                let def = defined(&schema, &name)?.clone();
                let r = self.test_record(p, &i)?;
                let run_p = def.principal(&name);
                // The trigger as the run principal would see it.
                let viewed = onto.read(|g, _| changes::view(&run_p, &r, None, g));
                let matched = viewed.as_ref().is_some_and(|v| def.admits(&name, &schema, v));
                let en = Enablements::load(&**store)?;
                let hash = en.states.get(&name).map(|s| s.closure_hash.clone()).unwrap_or_default();
                let mut row = RunRow::new(&name, &hash, onto.schema_version(), &def, RunTrigger::of(&r));
                row.started_at = now_ms();
                let mut out = json!({
                    "automation": name,
                    "matched": matched,
                    "trigger": row.trigger,
                    "depth": row.depth,
                    "depth_limit": def.depth_limit(self.config().automation.max_depth),
                    "means": MEANS,
                    "note": "A dry run as the run principal and you together (an entity either may not see has no \
                             fields): nothing was written.",
                });
                if viewed.is_none() {
                    out["why_not"] = json!(format!("{} does not see this change", run_p.id));
                } else if !matched {
                    out["why_not"] = json!("the trigger does not admit this change");
                }
                match onto.evaluate_run(&row, &def, Some(p)) {
                    Ok(Verdict::Ready(prepared)) => {
                        out["verdict"] =
                            json!(if def.mode == automate::Mode::Shadow { "would_run (shadow)" } else { "would_run" });
                        out["effects"] = json!(prepared.effects);
                    }
                    Ok(Verdict::NotMet(why)) => {
                        out["verdict"] = json!("conditions_not_met");
                        out["reasons"] = json!(why);
                    }
                    Ok(Verdict::Invalid(why)) => {
                        out["verdict"] = json!("plan_invalid");
                        out["reasons"] = json!(why);
                    }
                    Ok(Verdict::NeedsConfirmation(why)) => {
                        out["verdict"] = json!("needs_confirmation");
                        out["reasons"] = json!([why]);
                    }
                    Err(e) => {
                        out["verdict"] = json!("error");
                        out["reasons"] = json!([e.to_string()]);
                    }
                }
                Ok(out)
            }
            "automation.enable" => {
                let i: NameIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                let schema = onto.schema();
                let name = i.name.trim().to_string();
                let def = defined(&schema, &name)?;
                enable_check(onto, &ctx, "enable", &name, def)?;
                if !store.keeps_runs() || !store.keeps_changes() {
                    return Err(Error::Unavailable(
                        "failed_precondition: this store keeps no change log or automation runs".into(),
                    ));
                }
                let en = Enablements::load(&**store)?;
                // Every endpoint it reaches must exist and may see what a run renders.
                for ep in def.endpoints() {
                    let r = en.endpoints.iter().find(|r| r.name == ep).ok_or_else(|| {
                        Error::invalid(format!("automation.enable: webhook endpoint {ep} is not registered"))
                    })?;
                    if !dominates(&r.principal.principal(format!("webhook:{ep}")), &def.run_as) {
                        return Err(onto.refuse_configure(
                            &ctx,
                            format!(
                                "automation {name} renders what {} in {} may see; webhook endpoint {ep} may see \
                                 less (its principal does not dominate run_as)",
                                roles_text(def),
                                def.run_as.workspaces.join(", ")
                            ),
                        ));
                    }
                }
                let closure = automate::closure(&schema, &name, def, &en.endpoints, &en.targets);
                let now = now_ms();
                let ttl = (self.config().automation.enable_ttl_days as i64).saturating_mul(86_400_000);
                let st = AutomationState {
                    enabled: true,
                    closure_hash: closure.hash.clone(),
                    closure: closure.items.clone(),
                    schema_version: onto.schema_version(),
                    enabled_by: p.id.clone(),
                    enabled_at: now,
                    expires_at: now.saturating_add(ttl),
                    paused: None,
                    from: Some(onto.change_head()?),
                };
                let payload = json!({
                    "name": name, "closure_hash": closure.hash, "expires_at": st.expires_at,
                    "run_as": def.run_as, "mode": def.mode,
                });
                self.put_state(&ctx, &name, &st, action, payload)?;
                self.wake_automations();
                Ok(json!({
                    "name": name,
                    "enabled": true,
                    "closure_hash": closure.hash,
                    "expires_at": st.expires_at,
                    "from": st.from,
                    "note": format!(
                        "runs start with changes after {}; the enablement expires after {} days (a person enables it \
                         again), and any change to what it reaches stops it until then. {RUN_NOTE}",
                        st.from.unwrap_or_default(),
                        self.config().automation.enable_ttl_days
                    ),
                }))
            }
            "automation.disable" => {
                let i: NameIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                let name = i.name.trim().to_string();
                let schema = onto.schema();
                // Stopping: whoever may enable it, plus any admin.
                let allowed = match schema.automations.get(&name) {
                    Some(def) => may_enable(onto, p, def) || (p.is_privileged() && !p.is_agent()),
                    None => p.is_privileged() && !p.is_agent(),
                };
                if !allowed {
                    return Err(onto.refuse_configure(
                        &ctx,
                        format!("{} may not disable automation {name} (admins, or whoever may enable it)", p.id),
                    ));
                }
                let Some(mut st) = onto.automation_state(&name)? else {
                    return Err(Error::not_found(format!("automation {name} was never enabled")));
                };
                st.enabled = false;
                let cancelled = self.cancel_retries(Some(&name), "cancelled: the automation was disabled")?;
                self.put_state(&ctx, &name, &st, action, json!({ "name": name, "cancelled_retries": cancelled }))?;
                Ok(json!({ "name": name, "enabled": false, "cancelled_retries": cancelled }))
            }
            "automation.rerun" => {
                let i: RerunIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                let id = i.id.trim();
                let name = i.automation.as_deref().map(str::trim).filter(|n| !n.is_empty());
                let row = store
                    .run_get(id)?
                    .filter(|r| name.is_none_or(|n| r.automation == n))
                    .ok_or_else(|| Error::not_found(format!("run {id}")))?;
                let schema = onto.schema();
                let def = defined(&schema, &row.automation)?;
                enable_check(onto, &ctx, "rerun runs of", &row.automation, def)?;
                if !matches!(row.state, RunState::Skipped | RunState::Failed) {
                    return Err(Error::invalid(format!(
                        "automation.rerun: run {id} is {}; only skipped and failed runs are run again",
                        row.state
                    )));
                }
                let en = Enablements::load(&**store)?;
                if let Status::Inactive(why) =
                    runs::status(&schema, &en, &row.automation, now_ms(), runs::killed(&**store)?)
                {
                    return Err(Error::invalid(format!(
                        "automation.rerun: automation {} is not active ({why}); enable it first",
                        row.automation
                    )));
                }
                let reason = Some(format!("rerun by {}", p.id));
                let queued = row.clone().with_state(RunState::Retry, reason);
                if !store.run_update(&queued, &[RunState::Skipped, RunState::Failed], None)? {
                    return Err(Error::Conflict(format!("run {id} changed meanwhile; read it again")));
                }
                store.timers_set(
                    &format!("{RETRY_PREFIX}{id}"),
                    0,
                    &json!({ "run": id, "automation": row.automation }),
                )?;
                let target = format!("{}{}", automate::PRINCIPAL_PREFIX, row.automation);
                onto.commit_automation(
                    &ctx,
                    Vec::new(),
                    ConfigEvent::new(action, Some(&target), json!({ "run": id, "was": row.state })),
                )?;
                self.wake_automations();
                Ok(json!({
                    "id": id,
                    "state": "retry",
                    "note": "the automation consumer runs it again: it is evaluated and planned on the graph at its \
                             commit",
                }))
            }
            "automation.kill" => {
                let i: KillIn = parse(action, input)?;
                let on = match (i.on, i.off) {
                    (Some(a), Some(b)) if a == b => {
                        return Err(Error::invalid("automation.kill: on and off contradict each other"));
                    }
                    (Some(a), _) => a,
                    (None, Some(b)) => !b,
                    (None, None) => {
                        return Err(Error::invalid("automation.kill: give on: true or on: false (or off: true)"));
                    }
                };
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                if p.is_agent() || !p.is_privileged() {
                    return Err(onto.refuse_configure(
                        &ctx,
                        format!("{} may not switch automations (admins, never agents)", p.id),
                    ));
                }
                if !on {
                    // Switching back on resumes every enabled automation.
                    let schema = onto.schema();
                    let en = Enablements::load(&**store)?;
                    for (name, _) in en.states.iter().filter(|(_, s)| s.enabled) {
                        if let Some(def) = schema.automations.get(name) {
                            enable_check(onto, &ctx, "resume", name, def)?;
                        }
                    }
                }
                let cancelled =
                    if on { self.cancel_retries(None, "cancelled: the tenant kill switch is on")? } else { 0 };
                let v = json!({ "on": on, "by": p.id, "at": now_ms() });
                let kv = vec![(NS_AUTOMATION_CONTROL.to_string(), KILL_KEY.to_string(), v)];
                onto.commit_automation(
                    &ctx,
                    kv,
                    ConfigEvent::new(action, None, json!({ "on": on, "cancelled_retries": cancelled })),
                )?;
                self.wake_automations();
                Ok(json!({
                    "kill": runs::killed(&**store)?,
                    "cancelled_retries": cancelled,
                    "note": if on {
                        "automations are stopped: changes made while the switch is on never start runs, and pending \
                         retries were cancelled"
                    } else {
                        "automations run again on changes from now on"
                    },
                }))
            }
            other => Err(Error::invalid(format!("unknown action '{other}' (see system.actions)"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_floor_edge_is_just_before_the_floor() {
        assert_eq!(floor_edge(0), Cursor::default());
        assert_eq!(floor_edge(5), Cursor::new(4, u32::MAX));
    }

    #[test]
    fn calls_are_named_like_the_design() {
        assert!(CALLS.iter().all(|c| c.starts_with("automation.")));
        assert_eq!(CALLS.len(), 8);
        let distinct: HashSet<&str> = CALLS.iter().copied().collect();
        assert_eq!(distinct.len(), CALLS.len());
    }
}
