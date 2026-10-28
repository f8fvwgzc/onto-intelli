//! The outbox (docs/design/EVENTS.md B.1, D7): outbound deliveries
//! (webhooks, writeback calls) waiting to be sent, with their retries, claims
//! and dead letters, in the tenant's store.
//!
//! * Rows from actions and automations are inserted in the same commit as
//!   the graph change ([`WriteBatch::outbox`]); change-sourced rows hold
//!   only the change cursor and are rendered when they are sent. A row is
//!   unique per `(target, dedupe)`: each change is enqueued once per
//!   endpoint and delivered at least once, so receivers dedupe on its id.
//! * A worker **claims** due rows ([`Store::outbox_claim`]): each claimed
//!   row gets a fresh claim token and a lease by the store's clock. Only the
//!   holder of the current token can finish it ([`Store::outbox_finish`]),
//!   so a worker whose lease expired (and whose row another worker claimed
//!   since) changes nothing. A row whose lease expired is claimed again.
//! * Every attempt appends a row to the append-only attempt log.
//!
//! Configuration the delivery reads (endpoints, kill switches) is tenant KV,
//! written with its audit event in one commit by
//! [`Ontology::commit_config`], limited to [`CONFIG_NAMESPACES`].
//!
//! [`WriteBatch::outbox`]: crate::store::WriteBatch::outbox
//! [`Store::outbox_claim`]: crate::store::Store::outbox_claim
//! [`Store::outbox_finish`]: crate::store::Store::outbox_finish

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::changes::{Cursor, RunCause};
use crate::engine::{ActionContext, Extra, Ontology};
use crate::model::MutationResult;
use crate::time::{new_id, now_ms};
use crate::{Error, Result};

/// KV namespaces [`Ontology::commit_config`] writes: outbound configuration
/// and the operational state of webhooks, writeback targets and automations.
pub const CONFIG_NAMESPACES: &[&str] =
    &["webhook.endpoint", "webhook.control", "writeback.target", "automation.state", "automation.control"];

/// KV namespace of webhook endpoints (key: the endpoint name). onto-intelli
/// owns their type; a schema rename rewrites their change filters in the
/// same commit.
pub const NS_WEBHOOK_ENDPOINT: &str = "webhook.endpoint";

/// Bytes of a receiver's response body kept with an attempt (diagnostics).
pub const MAX_RESPONSE_BYTES: usize = 4096;
/// Most rows one [`OutboxQuery`] returns.
pub const MAX_LIST: usize = 500;

/// What a row delivers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxKind {
    /// A signed CloudEvent to a webhook endpoint.
    #[default]
    Webhook,
    /// An HTTP call to a writeback target (Part D).
    Writeback,
}

impl OutboxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxKind::Webhook => "webhook",
            OutboxKind::Writeback => "writeback",
        }
    }
}

impl FromStr for OutboxKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "webhook" => Ok(OutboxKind::Webhook),
            "writeback" => Ok(OutboxKind::Writeback),
            _ => Err(Error::Storage(format!("unknown outbox kind '{s}'"))),
        }
    }
}

/// Where a row is in its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxState {
    /// Waiting for its first attempt.
    #[default]
    Pending,
    /// Claimed by a worker until its lease ends.
    Inflight,
    /// Waiting for another attempt at `next_at`.
    Retry,
    /// A receiver answered 2xx.
    Delivered,
    /// Given up (a permanent refusal, too many attempts, too large, expired).
    Dead,
    /// Not sent, and never will be (the change is no longer visible to the endpoint).
    Cancelled,
}

impl OutboxState {
    pub const ALL: [OutboxState; 6] = [
        OutboxState::Pending,
        OutboxState::Inflight,
        OutboxState::Retry,
        OutboxState::Delivered,
        OutboxState::Dead,
        OutboxState::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            OutboxState::Pending => "pending",
            OutboxState::Inflight => "inflight",
            OutboxState::Retry => "retry",
            OutboxState::Delivered => "delivered",
            OutboxState::Dead => "dead",
            OutboxState::Cancelled => "cancelled",
        }
    }

    /// Still to be sent (pending, inflight or retry).
    pub fn is_open(self) -> bool {
        matches!(self, OutboxState::Pending | OutboxState::Inflight | OutboxState::Retry)
    }
}

/// The states `system.stats` counts per target (docs/design/EVENTS.md
/// section 11): what is still to be sent, and what was given up.
pub const STATS_STATES: [OutboxState; 4] =
    [OutboxState::Pending, OutboxState::Inflight, OutboxState::Retry, OutboxState::Dead];

/// Records a stats count reads at most (gap records).
pub const STATS_COUNT_CAP: u64 = 10_000;

/// Outbox rows of one target in one state that a stats count reads at
/// most. On an outbox not vacuumed since its rows last changed, PostgreSQL
/// counts them from the heap as well as the index, so the cap bounds that
/// read: at most four times this many rows per target.
pub const OUTBOX_STATS_CAP: u64 = 1_000;

impl fmt::Display for OutboxState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OutboxState {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        OutboxState::ALL.into_iter().find(|st| st.as_str() == s).ok_or_else(|| {
            Error::invalid(format!("outbox state '{s}' (pending, inflight, retry, delivered, dead, cancelled)"))
        })
    }
}

/// Row sources ([`OutboxRow::source`]).
pub const SOURCE_CHANGE: &str = "change";
pub const SOURCE_ACTION: &str = "action";
pub const SOURCE_AUTOMATION: &str = "automation";
pub const SOURCE_TEST: &str = "test";

/// One outbound delivery.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OutboxRow {
    /// `obx_…`: the CloudEvent id and the `webhook-id` header. Kept on replay.
    pub id: String,
    /// Store-clock milliseconds (set by the store when the row is inserted).
    pub created_at: i64,
    pub kind: OutboxKind,
    /// The endpoint or writeback target name.
    pub target: String,
    /// Unique per target: `<cursor>`, `<receipt>:<effect>`, `<run>:<effect>` or `test:<id>`.
    pub dedupe: String,
    /// `change`, `action`, `automation` or `test`.
    pub source: String,
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Change-sourced rows: the only content; the body is rendered at send time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
    /// The CloudEvent `type` (`onto.entity.update`, `onto.webhook.test`, …);
    /// for a writeback row, the target route it is posted to.
    pub event_type: String,
    /// Effect-sourced rows: the body as the run or action rendered it (a
    /// writeback row's request body).
    #[serde(default)]
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<RunCause>,
    pub state: OutboxState,
    pub attempts: u32,
    /// Store-clock milliseconds the row is due at.
    pub next_at: i64,
    /// The current claim token (inflight rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<i64>,
    /// The trace id of the call that caused the row (`traceparent` on send).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otel_trace_id: Option<String>,
}

impl OutboxRow {
    /// A pending row with a new `obx_` id. `created_at` and `next_at` are
    /// set by the store's clock when it is inserted.
    pub fn new(
        kind: OutboxKind,
        target: impl Into<String>,
        dedupe: impl Into<String>,
        source: &str,
        workspace: impl Into<String>,
        event_type: impl Into<String>,
    ) -> Self {
        Self {
            id: new_id("obx"),
            created_at: 0,
            kind,
            target: target.into(),
            dedupe: dedupe.into(),
            source: source.to_string(),
            workspace: workspace.into(),
            event_type: event_type.into(),
            ..Default::default()
        }
    }

    /// The row as an operator lists it: without its claim token.
    pub fn listed(mut self) -> Self {
        self.claim = None;
        self
    }
}

/// What one attempt to send a row found.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    /// The receiver's HTTP status, when it answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Why it failed, or why it was not sent (never a secret or a resolved reference).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub duration_ms: u64,
    /// The start of the receiver's response body (at most [`MAX_RESPONSE_BYTES`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
}

impl Attempt {
    /// The response body clipped to [`MAX_RESPONSE_BYTES`] at a character boundary.
    pub fn clip(body: &str) -> String {
        if body.len() <= MAX_RESPONSE_BYTES {
            return body.to_string();
        }
        let mut end = MAX_RESPONSE_BYTES;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body[..end].to_string()
    }
}

/// One logged attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub outbox_id: String,
    /// 1 for the first attempt of the row (counted again from 1 after a replay).
    pub attempt: u32,
    /// Store-clock milliseconds.
    pub ts: i64,
    /// The state the row moved to.
    pub state: OutboxState,
    #[serde(flatten)]
    pub result: Attempt,
}

/// Narrows [`Store::outbox_list`](crate::store::Store::outbox_list).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OutboxQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Any of these states (empty: all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<OutboxState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Rows created at or after this store-clock time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    /// At most this many (default 50, at most [`MAX_LIST`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl OutboxQuery {
    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(50).clamp(1, MAX_LIST)
    }

    /// Does `r` match (the memory store's filter; PostgreSQL filters in SQL)?
    pub fn matches(&self, r: &OutboxRow) -> bool {
        self.target.as_ref().is_none_or(|t| &r.target == t)
            && (self.states.is_empty() || self.states.contains(&r.state))
            && self.source.as_ref().is_none_or(|s| &r.source == s)
            && self.receipt.as_ref().is_none_or(|x| r.receipt.as_ref() == Some(x))
            && self.run.as_ref().is_none_or(|x| r.run.as_ref() == Some(x))
            && self.since.is_none_or(|s| r.created_at >= s)
    }
}

/// Which rows of one target [`Store::outbox_reset`] replays.
///
/// [`Store::outbox_reset`]: crate::store::Store::outbox_reset
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxSelect {
    Ids(Vec<String>),
    /// Rows created at or after this store-clock time.
    Since(i64),
}

/// States a replay moves back to `pending` when the caller names none:
/// everything that is not being sent right now.
pub const REPLAYABLE: [OutboxState; 4] =
    [OutboxState::Retry, OutboxState::Delivered, OutboxState::Dead, OutboxState::Cancelled];

/// The states a replay with `states` resets (never `inflight`: its claim
/// holder is sending it).
pub fn replay_states(states: &[OutboxState]) -> Vec<OutboxState> {
    let pick: Vec<OutboxState> = if states.is_empty() { REPLAYABLE.to_vec() } else { states.to_vec() };
    pick.into_iter().filter(|s| *s != OutboxState::Inflight).collect()
}

/// The audit event written with [`Ontology::commit_config`] and
/// [`Ontology::enqueue_outbox`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigEvent {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub payload: Value,
}

impl ConfigEvent {
    pub fn new(action: impl Into<String>, target: Option<&str>, payload: Value) -> Self {
        Self { action: action.into(), target: target.map(str::to_string), payload }
    }
}

pub(crate) fn no_outbox() -> Error {
    Error::Unavailable("this store keeps no outbox".into())
}

/// A row as inserted: validated, with the store's times to be filled in.
pub(crate) fn check_row(r: &OutboxRow) -> Result<()> {
    if r.id.is_empty() || r.target.is_empty() || r.dedupe.is_empty() || r.event_type.is_empty() {
        return Err(Error::invalid("an outbox row needs an id, a target, a dedupe key and an event type"));
    }
    if r.source.is_empty() {
        return Err(Error::invalid(format!("outbox row {}: no source", r.id)));
    }
    Ok(())
}

impl Ontology {
    fn may_configure(&self, ctx: &ActionContext, what: &str) -> Result<()> {
        let p = &ctx.principal;
        if p.is_agent() || !p.is_privileged() {
            let reason = format!("{} may not {what} (admins only, never agents)", p.id);
            self.audit_denied(ctx, &reason, now_ms());
            return Err(Error::Denied(reason));
        }
        Ok(())
    }

    /// Refuse a configuration call: the refusal is audited as
    /// `authorization.denied` and returned as `Error::Denied(reason)`.
    pub fn refuse_configure(&self, ctx: &ActionContext, reason: impl Into<String>) -> Error {
        let reason = reason.into();
        self.audit_denied(ctx, &reason, now_ms());
        Error::Denied(reason)
    }

    /// Write outbound configuration or operational state (`kv`, only in
    /// [`CONFIG_NAMESPACES`]) and its audit event in one commit. Admins and
    /// the system only, never agents; callers check the finer rules
    /// (dominance of the configured principal) first.
    pub fn commit_config(
        &self,
        ctx: &ActionContext,
        kv: Vec<(String, String, Value)>,
        event: ConfigEvent,
    ) -> Result<MutationResult> {
        self.may_configure(ctx, "change outbound configuration")?;
        if let Some((ns, _, _)) = kv.iter().find(|(ns, _, _)| !CONFIG_NAMESPACES.contains(&ns.as_str())) {
            return Err(Error::invalid(format!(
                "commit_config writes only {} (not {ns})",
                CONFIG_NAMESPACES.join(", ")
            )));
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

    /// Insert outbox rows with an audit event in one commit (test deliveries,
    /// replays of what a person asked for). Rows from actions and
    /// automations are not added this way: they commit with the change that
    /// caused them. Admins and the system only, never agents. A row whose
    /// `(target, dedupe)` exists fails the commit with `Conflict`.
    pub fn enqueue_outbox(
        &self,
        ctx: &ActionContext,
        rows: Vec<OutboxRow>,
        event: Option<ConfigEvent>,
    ) -> Result<MutationResult> {
        self.may_configure(ctx, "enqueue outbound deliveries")?;
        if !self.store().keeps_outbox() {
            return Err(no_outbox());
        }
        for r in &rows {
            check_row(r)?;
        }
        let (res, ()) = self.transact(ctx, |o, _| {
            let events = event
                .iter()
                .map(|e| {
                    o.event(
                        ctx,
                        &ctx.workspace,
                        &e.action,
                        e.target.as_deref(),
                        now_ms(),
                        e.payload.clone(),
                        Vec::new(),
                    )
                })
                .collect();
            Ok((Vec::new(), Extra { outbox: rows.clone(), events, ..Default::default() }, ()))
        })?;
        Ok(res)
    }
}

/// The summary of a row for audit events and outputs (no payload, no claim).
pub fn row_summary(r: &OutboxRow) -> Value {
    json!({
        "id": r.id,
        "target": r.target,
        "kind": r.kind,
        "source": r.source,
        "event_type": r.event_type,
        "state": r.state,
        "attempts": r.attempts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_and_kinds_round_trip() {
        for s in OutboxState::ALL {
            assert_eq!(s.as_str().parse::<OutboxState>().unwrap(), s);
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.as_str()));
        }
        assert!("sent".parse::<OutboxState>().is_err());
        assert_eq!("writeback".parse::<OutboxKind>().unwrap(), OutboxKind::Writeback);
        assert!(OutboxState::Retry.is_open() && !OutboxState::Dead.is_open());
        assert_eq!(replay_states(&[]), REPLAYABLE.to_vec());
        assert_eq!(replay_states(&[OutboxState::Inflight, OutboxState::Dead]), vec![OutboxState::Dead]);
    }

    #[test]
    fn rows_validate_and_clip() {
        let r = OutboxRow::new(OutboxKind::Webhook, "ops", "test:1", SOURCE_TEST, "acme", "onto.webhook.test");
        assert!(r.id.starts_with("obx_") && r.state == OutboxState::Pending);
        check_row(&r).unwrap();
        let mut bad = r.clone();
        bad.dedupe.clear();
        assert!(check_row(&bad).is_err());
        let long = "é".repeat(MAX_RESPONSE_BYTES);
        let clipped = Attempt::clip(&long);
        assert!(clipped.len() <= MAX_RESPONSE_BYTES && long.starts_with(&clipped));
        let q = OutboxQuery { states: vec![OutboxState::Pending], target: Some("ops".into()), ..Default::default() };
        assert!(q.matches(&r));
        assert!(!OutboxQuery { target: Some("x".into()), ..Default::default() }.matches(&r));
        assert_eq!(OutboxQuery { limit: Some(10_000), ..Default::default() }.limit(), MAX_LIST);
    }

    /// Rows committed with a graph change exist only with that commit: a
    /// denied mutation in the same batch leaves none; an allowed one commits
    /// both, and listeners hear of the rows.
    #[test]
    fn rows_commit_only_with_their_commit() {
        use crate::model::{EntityInput, Mutation};
        use crate::policy::Principal;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let o = Ontology::in_memory(crate::Schema::enterprise());
        let heard = std::sync::Arc::new(AtomicUsize::new(0));
        let h = heard.clone();
        o.on_change(move |c| {
            h.fetch_add(c.outbox, Ordering::SeqCst);
        });
        let r =
            OutboxRow::new(OutboxKind::Webhook, "ops", "run_1:0", SOURCE_AUTOMATION, "acme", "onto.issue.escalated");
        let id = r.id.clone();
        let commit = |ctx: &ActionContext| {
            o.transact(ctx, |_, _| {
                let m = Mutation::Upsert(EntityInput::new("issue", "9", "Escalated"));
                Ok((vec![m], Extra { outbox: vec![r.clone()], ..Default::default() }, ()))
            })
        };
        let viewer = ActionContext::new(Principal::named("user:v", &["viewer"], &["*"]).unwrap(), "acme");
        assert!(matches!(commit(&viewer), Err(Error::Denied(_))));
        assert!(o.store().outbox_get(&id).unwrap().is_none(), "no row without its commit");
        assert_eq!(heard.load(Ordering::SeqCst), 0);
        let admin = ActionContext::new(Principal::admin("ada"), "acme");
        commit(&admin).unwrap();
        assert_eq!(o.store().outbox_get(&id).unwrap().unwrap().state, OutboxState::Pending);
        assert_eq!(heard.load(Ordering::SeqCst), 1, "listeners hear of the row");
        // The same key again fails the whole commit, entity included.
        let again = |ctx: &ActionContext| {
            o.transact(ctx, |_, _| {
                let m = Mutation::Upsert(EntityInput::new("issue", "10", "Again"));
                let mut dup = r.clone();
                dup.id = new_id("obx");
                Ok((vec![m], Extra { outbox: vec![dup], ..Default::default() }, ()))
            })
        };
        assert!(matches!(again(&admin), Err(Error::Conflict(_))));
        assert!(o.get(&Principal::admin("ada"), "issue:10").is_err(), "nothing of the failed commit was written");
    }
}
