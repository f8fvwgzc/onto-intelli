//! Change subscriptions of webhook endpoints (docs/design/EVENTS.md B.6, D8).
//!
//! One process per tenant fans out at a time: the holder of the
//! [`FANOUT_LEASE`] (store clock), whose writes carry its epoch as a fence,
//! so a process that lost the lease while paused writes nothing.
//!
//! * Each endpoint with a change filter keeps its own cursor (the consumer
//!   `webhook:<name>`). A round reads one page from the lowest cursor among
//!   the endpoints that are enabled and not backlogged; each endpoint only
//!   considers records after its own cursor.
//! * Endpoints are indexed by op (and their kinds checked once per record
//!   kind), so a record is evaluated only against endpoints that could want
//!   it. Matching is the endpoint filter on the record as the endpoint's
//!   principal would see it now ([`onto_core::changes::view`]); that is a
//!   pre-check: the authoritative check is made again when the row is sent.
//! * Rows hold only the cursor. An endpoint's rows and its cursor advance
//!   commit together ([`onto_core::store::Store::outbox_enqueue`]), fenced by
//!   the lease epoch; `(target, dedupe = cursor)` is unique, so a page
//!   fanned out twice enqueues nothing twice. Each change is enqueued once per
//!   endpoint and delivered at least once.
//! * Backlog: an endpoint with `pending_cap` undelivered rows is paused
//!   (`backlogged`, a `webhook.endpoint.paused` event); a round never
//!   enqueues past the cap. Its cursor stays where it is, and fan-out
//!   resumes it (`webhook.endpoint.resumed`) once its backlog drains below
//!   half the cap. Nothing it subscribed to is skipped.
//! * Expiry: an endpoint whose cursor fell below the change log's floor (it
//!   was disabled or backlogged past retention), or is ahead of the head (a
//!   restored store), gets one `onto.changes.gap` delivery (`{from, to}`),
//!   a `webhook.endpoint.gap` event, and resumes at the floor (the head).
//!
//! While the tenant has enabled subscriptions, commits that change entities
//! or links also wake it (the shared wake row), so a closed tenant is opened
//! for its subscriptions by the deployment's wake scanner.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;

use onto_core::changes::{ChangeFilter, ChangeOp, ChangeRecord, Cursor, FilterEval, MAX_PAGE, RunCause};
use onto_core::leases::{Fence, holder_for};
use onto_core::outbox::{ConfigEvent, OutboxKind, OutboxRow, SOURCE_CHANGE};
use onto_core::time::now_ms;
use onto_core::{EventInput, Mutation, Principal, Result};

use super::{BACKLOG_PREFIX, Endpoint, GAP_EVENT, NS_CONTROL, consumer_name, system_ctx};
use crate::intelli::Intelli;

/// The lease of the tenant's fan-out (one holder across processes).
pub const FANOUT_LEASE: &str = "webhook:fanout";
/// How long a round's lease lasts (store clock); renewed every round.
const LEASE_MS: i64 = 60_000;
/// Records evaluated under one graph read lock.
const SLICE: usize = 64;

/// What a round found.
#[derive(Debug, PartialEq, Eq)]
enum Round {
    /// Nothing to do (no subscriptions, or another process fans out).
    Idle,
    /// Caught up for now.
    Done,
    /// A full page, or an endpoint reached its cap: go again now.
    More,
}

/// Start the fan-out of `me` (`[webhooks] workers`, and stores that keep a
/// change log, an outbox and fenced leases). It holds the tenant only weakly and ends when the tenant
/// closes. It wakes on every commit (`notify`), and every `poll_secs`.
pub fn spawn_fanout(me: &Arc<Intelli>, notify: Arc<Notify>) {
    let store = me.ontology().store().clone();
    if !me.config().webhooks.workers || !store.keeps_outbox() || !store.keeps_changes() || !store.fences() {
        return;
    }
    let weak = Arc::downgrade(me);
    let holder = holder_for(store.as_ref());
    let poll = Duration::from_secs(me.config().webhooks.poll_secs.max(1));
    let mut closing = me.closing_signal();
    let worker = tokio::spawn(async move {
        loop {
            let again = {
                let Some(me) = weak.upgrade() else { break };
                if me.is_closing() {
                    break;
                }
                let holder = holder.clone();
                let me2 = me.clone();
                match tokio::task::spawn_blocking(move || round(&me2, &holder)).await {
                    Ok(Ok(r)) => r == Round::More,
                    Ok(Err(e)) => {
                        tracing::warn!("webhook fan-out: {e}");
                        false
                    }
                    Err(e) => {
                        tracing::warn!("webhook fan-out: {e}");
                        false
                    }
                }
            };
            if again {
                tokio::task::yield_now().await;
                continue;
            }
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(poll) => {}
                _ = closing.changed() => {}
            }
        }
    });
    me.holdings().worker(worker);
}

/// One endpoint taking part in a round.
struct Sub {
    name: String,
    principal: Principal,
    cursor: Cursor,
    /// Rows it may still take before it reaches `pending_cap`.
    room: usize,
    rows: Vec<OutboxRow>,
    /// Where its cursor moves to.
    last: Cursor,
    /// It reached its cap in this page.
    stopped: bool,
}

/// May a filter want a stored record of `op` (after the reader's view)?
fn op_may_match(f: &ChangeFilter, op: ChangeOp) -> bool {
    op == ChangeOp::Gap
        || f.ops.is_empty()
        || f.ops.contains(&op)
        // An update may reach a reader as `entity.hidden`.
        || (op == ChangeOp::EntityUpdate && f.ops.contains(&ChangeOp::EntityHidden))
}

/// The row that delivers `r` (as the endpoint saw it) to `endpoint`.
fn change_row(endpoint: &str, r: &ChangeRecord) -> OutboxRow {
    let mut row = OutboxRow::new(
        OutboxKind::Webhook,
        endpoint,
        r.cursor.to_string(),
        SOURCE_CHANGE,
        r.workspace.clone(),
        format!("onto.{}", r.op.as_str()),
    );
    row.cursor = Some(r.cursor);
    // The delivery continues the trace of the call that wrote the change.
    row.otel_trace_id = r.cause.trace_id.clone().filter(|t| onto_core::trace::is_trace_id(t));
    if let Some(a) = &r.cause.automation {
        row.cause = Some(RunCause {
            automation: a.clone(),
            run: r.cause.run.clone().unwrap_or_default(),
            depth: r.cause.depth,
        });
    }
    row
}

fn event(me: &Intelli, action: &str, endpoint: &str, payload: Value) {
    let onto = me.ontology();
    let ctx = system_ctx(onto, "");
    let ev = EventInput {
        action: action.to_string(),
        target: Some(format!("webhook:{endpoint}")),
        workspace: Some(ctx.workspace.clone()),
        payload,
        ..Default::default()
    };
    if let Err(e) = onto.apply(&ctx, vec![Mutation::Event(ev)]) {
        tracing::warn!("webhook fan-out event: {e}");
    }
}

/// Pause (`Some(pending)`) or resume (`None`) an endpoint for its backlog.
fn set_backlog(me: &Intelli, name: &str, pending: u64, pause: bool, cap: u64) -> Result<()> {
    let onto = me.ontology();
    let ctx = system_ctx(onto, "");
    let key = format!("{BACKLOG_PREFIX}{name}");
    let (value, action, payload) = if pause {
        (
            json!({ "since": now_ms(), "pending": pending }),
            "webhook.endpoint.paused",
            json!({ "reason": "backlogged", "pending": pending, "pending_cap": cap }),
        )
    } else {
        (
            Value::Null,
            "webhook.endpoint.resumed",
            json!({ "reason": "backlog drained", "pending": pending, "pending_cap": cap }),
        )
    };
    let ev = ConfigEvent::new(action, Some(&format!("webhook:{name}")), payload);
    onto.commit_config(&ctx, vec![(NS_CONTROL.to_string(), key, value)], ev).map(drop)
}

/// The position just before the floor's commit (the oldest records kept):
/// a cursor below it may have missed removed records.
fn floor_edge(floor: u64) -> Cursor {
    if floor == 0 { Cursor::default() } else { Cursor::new(floor - 1, u32::MAX) }
}

/// The endpoint's cursor expired (below the floor) or is ahead of the head:
/// one gap delivery, and the cursor moves to the floor (the head).
fn gap(me: &Intelli, name: &str, from: Cursor, floor: u64, head: Cursor, fence: &Fence) -> Result<()> {
    let store = me.ontology().store();
    let edge = floor_edge(floor);
    let (reason, to, resume) = if from > head && from > edge {
        ("ahead_of_head", head, head.max(edge))
    } else {
        ("expired", Cursor::new(floor, 0), edge)
    };
    let message = "records between from and to may be missing: they were removed by retention (or the store was \
                   restored) before this endpoint received them. Consistent resync is not offered; re-read with \
                   entity.find (the result is not pinned to a cursor).";
    let mut row = OutboxRow::new(OutboxKind::Webhook, name, format!("gap:{from}:{to}"), SOURCE_CHANGE, "", GAP_EVENT);
    row.payload = json!({ "from": from, "to": to, "reason": reason, "message": message });
    store.outbox_enqueue(&[row], Some((&consumer_name(name), resume)), Some(fence))?;
    event(me, "webhook.endpoint.gap", name, json!({ "from": from, "to": to, "reason": reason }));
    me.wake_outbox();
    Ok(())
}

fn round(me: &Intelli, holder: &str) -> Result<Round> {
    me.count_fanout_round();
    let onto = me.ontology();
    let store = onto.store();
    let cfg = &me.config().webhooks;
    let mut eps: Vec<Endpoint> = Endpoint::list(onto)?.into_iter().filter(|e| e.filter.is_some()).collect();
    eps.sort_by(|a, b| a.name.cmp(&b.name));
    let subscribed = eps.iter().any(|e| e.enabled);
    onto.set_wake_on_change(subscribed);
    me.set_wants_fanout(subscribed);
    if !subscribed {
        return Ok(Round::Idle);
    }
    let Some(epoch) = store.lease(FANOUT_LEASE, holder, LEASE_MS)? else {
        me.holdings().lost(FANOUT_LEASE);
        return Ok(Round::Idle);
    };
    let fence = Fence::new(FANOUT_LEASE, holder, epoch);
    // Released on shutdown, so a replacement process fans out at once.
    me.holdings().took(&fence);
    // Cursors first, then the feed's position: a cursor set meanwhile (a
    // new subscription starts at the head) is never ahead of the head read.
    let mut cursors = HashMap::new();
    for ep in eps.iter().filter(|e| e.enabled) {
        cursors.insert(ep.name.clone(), store.consumer_get(&consumer_name(&ep.name))?);
    }
    let meta = store.changes_meta()?;
    let cap = cfg.pending_cap.max(1);
    let names: Vec<String> = eps.iter().filter(|e| e.enabled).map(|e| e.name.clone()).collect();
    let open: HashMap<String, u64> = store.outbox_open_counts_for(&names, cap)?.into_iter().collect();
    let backlogged: HashMap<String, Value> = store
        .kv_scan(NS_CONTROL, BACKLOG_PREFIX)?
        .into_iter()
        .filter(|(_, v)| !v.is_null())
        .filter_map(|(k, v)| k.strip_prefix(BACKLOG_PREFIX).map(|n| (n.to_string(), v)))
        .collect();

    let mut subs: Vec<Sub> = Vec::new();
    let mut filters: Vec<ChangeFilter> = Vec::new();
    let mut transitions = false;
    for ep in eps.into_iter().filter(|e| e.enabled) {
        let consumer = consumer_name(&ep.name);
        let Some(cursor) = cursors.get(&ep.name).copied().flatten() else {
            // A subscription without a cursor (registered before fan-out
            // kept one): it starts now.
            store.consumer_set(&consumer, meta.head, Some(&fence))?;
            continue;
        };
        let pending = open.get(&ep.name).copied().unwrap_or(0);
        if backlogged.contains_key(&ep.name) {
            if pending.saturating_mul(2) >= cap {
                continue;
            }
            set_backlog(me, &ep.name, pending, false, cap)?;
            transitions = true;
        } else if pending >= cap {
            set_backlog(me, &ep.name, pending, true, cap)?;
            transitions = true;
            continue;
        }
        // Records before the floor's commit are gone; a cursor just before
        // it (where a gap leaves an endpoint) is complete.
        let edge = floor_edge(meta.floor);
        if cursor < edge || (cursor > meta.head && cursor > edge) {
            gap(me, &ep.name, cursor, meta.floor, meta.head, &fence)?;
            transitions = true;
            continue;
        }
        let Some(filter) = ep.filter.clone() else { continue };
        subs.push(Sub {
            principal: ep.principal(),
            name: ep.name,
            cursor,
            room: (cap - pending) as usize,
            rows: Vec::new(),
            last: cursor,
            stopped: false,
        });
        filters.push(filter);
    }
    if subs.is_empty() {
        return Ok(if transitions { Round::More } else { Round::Done });
    }

    let low = subs.iter().map(|s| s.cursor).min().unwrap_or_default();
    let limit = me.config().changes.page.clamp(1, MAX_PAGE);
    let page = store.changes_since(low, limit)?;
    let full = page.changes.len() >= limit;
    // Retention ran between the position read and the page: those endpoints
    // get their gap next round.
    for s in subs.iter_mut().filter(|s| s.cursor < floor_edge(page.floor)) {
        s.stopped = true;
    }
    let evals: Vec<FilterEval> = filters.iter().map(FilterEval::new).collect();
    let mut by_op: HashMap<ChangeOp, Vec<usize>> = HashMap::new();
    for op in ChangeOp::ALL {
        by_op.insert(op, (0..filters.len()).filter(|&i| op_may_match(&filters[i], op)).collect());
    }
    let schema = onto.schema();
    let mut kind_ok: HashMap<(usize, String), bool> = HashMap::new();
    let mut prev = low;
    for slice in page.changes.chunks(SLICE) {
        // The graph-free part first: op index, kinds, then the filter's
        // own cheap test, for endpoints that have not seen the record.
        let mut candidates: Vec<Vec<usize>> = Vec::with_capacity(slice.len());
        for (stored, _) in slice {
            let r = &stored.record;
            let mut want = Vec::new();
            for &i in by_op.get(&r.op).map(Vec::as_slice).unwrap_or_default() {
                if subs[i].stopped || subs[i].cursor >= r.cursor {
                    continue;
                }
                if matches!(r.op, ChangeOp::EntityCreate | ChangeOp::EntityUpdate) && !filters[i].kinds.is_empty() {
                    let old = r.moved.as_ref().and_then(|m| m.kind.as_ref()).map(|k| k.from.as_str());
                    let mut ok = false;
                    for k in std::iter::once(r.kind.as_str()).chain(old) {
                        ok |= *kind_ok
                            .entry((i, k.to_string()))
                            .or_insert_with(|| filters[i].kinds.iter().any(|want| schema.kind_is(k, want)));
                    }
                    if !ok {
                        continue;
                    }
                }
                if evals[i].may_match(&schema, r) {
                    want.push(i);
                }
            }
            candidates.push(want);
        }
        if candidates.iter().all(Vec::is_empty) {
            if let Some((last, _)) = slice.last() {
                prev = last.record.cursor;
            }
            continue;
        }
        onto.read(|g, schema| {
            for ((stored, entity), want) in slice.iter().zip(&candidates) {
                for &i in want {
                    let sub = &mut subs[i];
                    if sub.stopped {
                        continue;
                    }
                    let Some(v) = evals[i].view_and_match(&sub.principal, schema, g, stored, entity.clone()) else {
                        continue;
                    };
                    if sub.rows.len() >= sub.room {
                        // At the cap: stop before this record, so it is
                        // enqueued when the backlog drains.
                        sub.stopped = true;
                        sub.last = sub.cursor.max(prev);
                        continue;
                    }
                    sub.rows.push(change_row(&sub.name, &v));
                }
                prev = stored.record.cursor;
            }
        });
    }
    let scanned_to = page.changes.last().map(|(s, _)| s.record.cursor).unwrap_or(page.head);
    let mut capped = false;
    let mut inserted = 0usize;
    for sub in &mut subs {
        if sub.stopped {
            capped = true;
        } else {
            sub.last = sub.cursor.max(scanned_to);
        }
        if sub.last <= sub.cursor && sub.rows.is_empty() {
            continue;
        }
        inserted += store.outbox_enqueue(&sub.rows, Some((&consumer_name(&sub.name), sub.last)), Some(&fence))?;
    }
    if inserted > 0 {
        me.wake_outbox();
    }
    Ok(if full || capped || transitions { Round::More } else { Round::Done })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_op_index_keeps_gaps_and_updates_that_may_hide() {
        let all = ChangeFilter::default();
        assert!(ChangeOp::ALL.iter().all(|op| op_may_match(&all, *op)));
        let creates = ChangeFilter { ops: vec![ChangeOp::EntityCreate], ..Default::default() };
        assert!(op_may_match(&creates, ChangeOp::EntityCreate));
        assert!(op_may_match(&creates, ChangeOp::Gap), "a filter never hides a gap");
        assert!(!op_may_match(&creates, ChangeOp::EntityUpdate));
        let hidden = ChangeFilter { ops: vec![ChangeOp::EntityHidden], ..Default::default() };
        assert!(op_may_match(&hidden, ChangeOp::EntityUpdate), "an update can reach a reader as entity.hidden");
    }

    #[test]
    fn change_rows_hold_the_cursor_and_the_cause() {
        let r: ChangeRecord = serde_json::from_value(json!({
            "cursor": "7.2", "ts": 0, "op": "entity.update", "workspace": "acme", "kind": "issue", "id": "issue:1",
            "cause": { "automation": "escalate", "run": "run_1", "depth": 2 },
        }))
        .unwrap();
        let row = change_row("ops", &r);
        assert_eq!((row.dedupe.as_str(), row.cursor), ("7.2", Some(Cursor::new(7, 2))));
        assert_eq!((row.event_type.as_str(), row.workspace.as_str()), ("onto.entity.update", "acme"));
        assert_eq!(row.cause.unwrap().depth, 2);
        assert!(row.payload.is_null(), "change rows are rendered when sent");
    }
}
