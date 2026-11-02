//! The delivery worker of one open tenant (docs/design/EVENTS.md B.4, D.3).
//!
//! A pass claims due rows (claim tokens, a lease by the store's clock),
//! sends each with the guarded client within the per-endpoint, per-tenant
//! and process-wide limits (all per process), records the attempt with the
//! claim token it was made under, and updates the endpoint's breaker.
//! Webhook rows are signed CloudEvents to endpoints (`[webhooks] enabled`);
//! writeback rows post their stored body to a writeback target's route with
//! `Idempotency-Key: <receipt>:<effect>` (`[writeback] enabled`), with the
//! same outcomes, retries and breaker; a dead writeback row writes a
//! `writeback.failed` event. It
//! wakes on commits that enqueue rows, on other processes' commits, and
//! every `poll_secs` (or sooner, when a retry is due sooner); after a pass
//! that found nothing more it settles the tenant's wake row, so a closed
//! tenant is woken again only when work is due.
//!
//! Every request carries a W3C `traceparent`: the trace the row continues
//! (the call or run that enqueued it), else the attempt's own, and the
//! attempt's span. Attempts become traces of kind `delivery` as `[traces]
//! deliveries` says (`failures`: retried and dead ones; `all`; `off`),
//! without bodies, headers, secrets or URLs (docs/design/EVENTS.md B.7).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};

use onto_core::changes::{ChangeAt, FieldMode};
use onto_core::outbox::{Attempt, ConfigEvent, OutboxKind, OutboxRow, OutboxState, SOURCE_CHANGE};
use onto_core::time::now_ms;
use onto_core::trace::{TraceRecord, is_trace_id, new_trace_id, traceparent};
use onto_core::{Error, EventInput, Mutation, Ontology, Result};

use super::{
    BREAKER_PREFIX, BreakerState, CLAIM_LIMIT, Class, Endpoint, LEASE_MS, NS_CONTROL, NS_ENDPOINT, backoff_ms,
    check_endpoint_url, classify, envelope, jitter, killed, next_pause, resolve_text, retry_after_ms, signature_header,
    system_ctx,
};
use crate::config::WebhooksConfig;
use crate::intelli::Intelli;
use crate::net::{Reach, guarded_client};
use crate::writeback::Target;

/// The trace of one delivery attempt: its id (used in `traceparent` when
/// the row continues no trace), the root span sent as the `traceparent`
/// parent, and when the attempt started.
#[derive(Clone)]
struct AttemptTrace {
    id: String,
    span: String,
    started: Instant,
    start_ms: i64,
}

impl AttemptTrace {
    fn new() -> Self {
        let span = crate::trace::span_ids(1).remove(0);
        AttemptTrace { id: new_trace_id(), span, started: Instant::now(), start_ms: now_ms() }
    }
}

tokio::task_local! {
    /// The attempt the current send task makes.
    static ATTEMPT: AttemptTrace;
}

/// Requests in flight in this process, all tenants together.
fn process_slots(n: usize) -> Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(n.max(1)))).clone()
}

/// What this process knows about one endpoint between passes.
#[derive(Default)]
struct EndpointState {
    /// Consecutive failures since the last success.
    failures: u32,
    /// A half-open probe is in flight.
    probing: bool,
    in_flight: usize,
}

struct Worker {
    weak: Weak<Intelli>,
    cfg: WebhooksConfig,
    /// `[webhooks] enabled`: webhook rows are sent by this process.
    webhooks_on: bool,
    /// `[writeback] enabled`: writeback rows are sent by this process.
    writeback_on: bool,
    /// `[writeback] timeout_secs`.
    writeback_timeout: Duration,
    /// This process runs the automation consumer (`[automation] enabled` and
    /// `workers`): its retry timers count toward the tenant's wake row.
    consumer: bool,
    holder: String,
    reach: Reach,
    tenant: String,
    endpoints: Mutex<HashMap<String, EndpointState>>,
    tenant_slots: Arc<Semaphore>,
    process: Arc<Semaphore>,
    /// Clients for open and for restricted endpoints.
    clients: (reqwest::Client, reqwest::Client),
    /// Clients for open and for restricted writeback targets (their timeout).
    writeback_clients: (reqwest::Client, reqwest::Client),
}

/// What a breaker or a dead row is about: an endpoint or a writeback target.
fn kind_words(kind: OutboxKind) -> (&'static str, &'static str) {
    match kind {
        OutboxKind::Webhook => ("webhook", "webhook.endpoint"),
        OutboxKind::Writeback => ("writeback", "writeback.target"),
    }
}

/// Start the delivery worker of `me` (with `[webhooks] workers`, when
/// `[webhooks] enabled` or `[writeback] enabled`, and the store keeps an
/// outbox). It holds the
/// tenant only weakly and ends when the tenant closes. Rows of a kind this
/// process does not send are left in the outbox.
pub fn spawn_delivery(me: &Arc<Intelli>, notify: Arc<Notify>) -> Result<()> {
    let cfg = me.config().webhooks.clone();
    let wb = me.config().writeback.clone();
    let store = me.ontology().store().clone();
    if !cfg.workers || !(cfg.enabled || wb.enabled) || !store.keeps_outbox() {
        return Ok(());
    }
    let writeback_timeout = Duration::from_secs(wb.timeout_secs.max(1));
    let allow = Arc::new(cfg.allow_rules()?);
    let allow_private = me.config().console.allow_private_urls;
    let timeout = Duration::from_secs(cfg.timeout_secs);
    let open = Reach { allow: allow.clone(), restricted: false, allow_private };
    let restricted = Reach { allow, restricted: true, allow_private };
    let tenant = me.config().engine.postgres.tenant.trim().to_string();
    let worker = Arc::new(Worker {
        weak: Arc::downgrade(me),
        holder: onto_core::leases::holder_for(store.as_ref()),
        tenant: if tenant.is_empty() { "default".into() } else { tenant },
        clients: (guarded_client(&open, timeout)?, guarded_client(&restricted, timeout)?),
        writeback_clients: (guarded_client(&open, writeback_timeout)?, guarded_client(&restricted, writeback_timeout)?),
        webhooks_on: cfg.enabled,
        writeback_on: wb.enabled,
        writeback_timeout,
        consumer: me.config().automation.enabled && me.config().automation.workers,
        reach: open,
        endpoints: Mutex::new(HashMap::new()),
        tenant_slots: Arc::new(Semaphore::new(cfg.max_in_flight_per_tenant.max(1))),
        process: process_slots(cfg.max_in_flight_process),
        cfg,
    });
    let mut closing = me.closing_signal();
    let task = tokio::spawn(async move {
        let poll = Duration::from_secs(worker.cfg.poll_secs.max(1));
        // Retention is the tenant's periodic task (Intelli::vacuum_outbound).
        loop {
            let wait = {
                let Some(me) = worker.weak.upgrade() else { break };
                if me.is_closing() {
                    break;
                }
                match worker.drain(&me).await {
                    Ok(next) => next.map_or(poll, |d| d.min(poll)),
                    Err(e) => {
                        tracing::warn!(tenant = %worker.tenant, "webhook delivery: {e}");
                        poll
                    }
                }
            };
            tokio::select! {
                _ = notify.notified() => {}
                _ = tokio::time::sleep(wait) => {}
                _ = closing.changed() => {}
            }
        }
    });
    me.holdings().worker(task);
    Ok(())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::Other(format!("webhook delivery: {e}")))?
}

/// Breakers by endpoint, from `webhook.control`.
fn breakers(onto: &Ontology) -> Result<HashMap<String, BreakerState>> {
    let mut out = HashMap::new();
    for (k, v) in onto.store().kv_scan(NS_CONTROL, BREAKER_PREFIX)? {
        if let (Some(name), Ok(b)) = (k.strip_prefix(BREAKER_PREFIX), serde_json::from_value::<BreakerState>(v)) {
            out.insert(name.to_string(), b);
        }
    }
    Ok(out)
}

impl Worker {
    /// Claim and send until nothing is due; returns how long until the next
    /// row is due (`None`: nothing pending).
    async fn drain(self: &Arc<Self>, me: &Arc<Intelli>) -> Result<Option<Duration>> {
        let onto = me.ontology().clone();
        loop {
            // Shutting down (or the tenant closing): claim nothing more.
            if me.is_closing() {
                return Ok(None);
            }
            let (seen, stop, skip) = {
                let onto = onto.clone();
                let cap = self.cfg.max_in_flight_per_endpoint;
                let (webhooks_on, writeback_on) = (self.webhooks_on, self.writeback_on);
                let states: HashMap<String, (usize, bool)> =
                    self.endpoints.lock().iter().map(|(k, s)| (k.clone(), (s.in_flight, s.probing))).collect();
                blocking(move || {
                    let seen = onto.store().wake_get()?.map(|w| w.bumped);
                    if killed(&onto)? {
                        return Ok((seen, true, Vec::new()));
                    }
                    let now = now_ms();
                    let br = breakers(&onto)?;
                    let mut skip = Vec::new();
                    let held = |name: &str| {
                        let (in_flight, probing) = states.get(name).copied().unwrap_or((0, false));
                        let open = br.get(name).is_some_and(|b| b.is_open() && (b.until > now || probing));
                        open || in_flight >= cap
                    };
                    for ep in Endpoint::list(&onto)? {
                        let paused = ep.paused.as_ref().is_some_and(|p| p.until.is_none_or(|u| u > now));
                        if !webhooks_on || !ep.enabled || paused || held(&ep.name) {
                            skip.push(ep.name);
                        }
                    }
                    for t in Target::list(&onto)? {
                        if !writeback_on || held(&t.name) {
                            skip.push(t.name);
                        }
                    }
                    Ok((seen, false, skip))
                })
                .await?
            };
            if stop {
                return Ok(None);
            }
            let free = self.tenant_slots.available_permits();
            if free == 0 {
                return Ok(Some(Duration::from_millis(200)));
            }
            let rows = {
                let (store, holder) = (onto.store().clone(), self.holder.clone());
                let limit = CLAIM_LIMIT.min(free.max(1));
                blocking(move || store.outbox_claim(limit, LEASE_MS, &holder, &skip)).await?
            };
            if rows.is_empty() {
                return self.settle(&onto, seen).await;
            }
            // Given back on shutdown when not settled by then.
            let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
            for r in &rows {
                me.holdings().claimed(&r.id, r.claim.as_deref().unwrap_or_default());
            }
            let claimed = Instant::now();
            let mut tasks = tokio::task::JoinSet::new();
            for row in rows {
                let (w, me) = (self.clone(), me.clone());
                // No URL and no secret in the span: the row's target and id.
                let span = tracing::info_span!(
                    "webhook.deliver",
                    target = %row.target,
                    id = %row.id,
                    attempt = row.attempts,
                    kind = row.kind.as_str()
                );
                let send = ATTEMPT.scope(AttemptTrace::new(), async move { w.send(&me, row, claimed).await });
                tasks.spawn(tracing::Instrument::instrument(send, span));
            }
            while let Some(r) = tasks.join_next().await {
                if let Err(e) = r {
                    tracing::warn!("webhook delivery task: {e}");
                }
            }
            for id in &ids {
                me.holdings().settled(id);
            }
        }
    }

    /// Nothing more is due now: set the tenant's next wake time (unless it
    /// was bumped meanwhile, then drain again) and say when to look again.
    async fn settle(&self, onto: &Ontology, seen: Option<u64>) -> Result<Option<Duration>> {
        let store = onto.store().clone();
        let consumer = self.consumer;
        blocking(move || {
            let outbox = store.outbox_next_due()?;
            let timers = if consumer { store.timers_next().unwrap_or(None) } else { None };
            let (wake, own) = next_due(outbox, timers, consumer);
            if let Some(seen) = seen
                && !store.wake_settle(seen, wake)?
            {
                return Ok(Some(Duration::ZERO));
            }
            let now = store.clock_ms()?;
            Ok(own.map(|n| Duration::from_millis((n - now).max(0) as u64 + 5)))
        })
        .await
    }

    /// Send one claimed row and record what happened.
    async fn send(self: &Arc<Self>, me: &Arc<Intelli>, row: OutboxRow, claimed: Instant) {
        if me.is_closing() {
            // Not sent: the next claim (another process, or this one after
            // the restart) sends it.
            return self.release(me, &row, "shutting down").await;
        }
        match row.kind {
            OutboxKind::Webhook => self.send_webhook(me, row, claimed).await,
            OutboxKind::Writeback => self.send_writeback(me, row, claimed).await,
        }
    }

    /// Give a claimed row back unsent, without counting the attempt.
    async fn release(&self, me: &Arc<Intelli>, row: &OutboxRow, reason: &'static str) {
        let (store, id, claim) = (me.ontology().store().clone(), row.id.clone(), row.claim.clone().unwrap_or_default());
        tracing::debug!(id, reason, "outbox row released unsent");
        let _ = blocking(move || store.outbox_release(&id, &claim)).await;
    }

    /// Send one claimed writeback row: its stored body to the target's
    /// route, with `Idempotency-Key: <receipt>:<effect>` (the row's dedupe
    /// key) and, when the target has a secret, the Standard Webhooks headers.
    async fn send_writeback(self: &Arc<Self>, me: &Arc<Intelli>, row: OutboxRow, claimed: Instant) {
        let onto = me.ontology().clone();
        // The target is read again right before sending: a change applies
        // to rows not yet sent.
        let (target, kill) = {
            let (onto, name) = (onto.clone(), row.target.clone());
            match blocking(move || Ok((Target::load(&onto, &name)?, killed(&onto)?))).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("writeback delivery: {e}");
                    return self.release(me, &row, "store error").await;
                }
            }
        };
        if kill {
            return self.release(me, &row, "kill switch").await;
        }
        let Some(t) = target.filter(|t| t.owns(&row)) else {
            // Removed by a person (perhaps added again since): what was
            // waiting for it is not sent.
            let a = Attempt {
                error: Some("target_removed: the writeback target no longer exists".into()),
                ..Default::default()
            };
            return self.finish(me, &row, a, OutboxState::Cancelled, 0).await;
        };
        if !self.writeback_on {
            // Registered after this pass chose what to skip; the next pass skips it.
            return self.release(me, &row, "writeback is off in this process").await;
        }
        if claimed.elapsed() + self.writeback_timeout + Duration::from_secs(5) >= Duration::from_millis(LEASE_MS as u64)
        {
            return self.release(me, &row, "lease too short").await;
        }
        let half_open = {
            let onto = onto.clone();
            blocking(move || breakers(&onto)).await.unwrap_or_default().get(&t.name).is_some_and(|b| b.is_open())
        };
        let Some(_permits) = self.acquire(&t.name, half_open).await else {
            return self.release(me, &row, "target at its limit").await;
        };
        let _done = Done { w: self.clone(), name: t.name.clone(), probe: half_open };

        let body = match serde_json::to_vec(&row.payload) {
            Ok(b) => b,
            Err(e) => {
                let a = Attempt { error: Some(format!("render: {e}")), ..Default::default() };
                return self.finish(me, &row, a, OutboxState::Dead, 0).await;
            }
        };
        if body.len() > self.cfg.max_payload_bytes {
            let a = Attempt { error: Some("payload_too_large".into()), ..Default::default() };
            return self.finish(me, &row, a, OutboxState::Dead, 0).await;
        }
        // Where to: errors name references, never values.
        let reach = Reach { restricted: t.restricted, ..self.reach.clone() };
        let url = match t.request_url(&row.event_type, &reach) {
            Ok(u) => u,
            Err(e) => {
                let a = Attempt { error: Some(format!("refused: {e}")), ..Default::default() };
                return self.finish(me, &row, a, OutboxState::Dead, 0).await;
            }
        };
        let keys = match t.signing_keys(me.secrets().as_deref()) {
            Ok(k) => k,
            Err(e) => {
                // An operator can fix a missing variable or file: retry.
                let a = Attempt { error: Some(e.to_string()), ..Default::default() };
                return self.failed(me, &row, &t.name, a, None).await;
            }
        };
        let client = if t.restricted { &self.writeback_clients.1 } else { &self.writeback_clients.0 };
        let mut req = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, format!("onto-intelli/{}", crate::VERSION))
            .header("idempotency-key", &row.dedupe);
        if !keys.is_empty() {
            let ts = now_ms() / 1000;
            req = req
                .header("webhook-id", &row.id)
                .header("webhook-timestamp", ts.to_string())
                .header("webhook-signature", signature_header(&keys, &row.id, ts, &body));
        }
        let req = traced(req.body(body), &row);
        let reference = super::is_reference(&t.base_url).then(|| t.base_what());
        if let Some(mut a) = self.exchange(me, &row, &t.name, req, reference.as_deref()).await {
            a.error = Some("http 410: the target answered gone".into());
            self.finish(me, &row, a, OutboxState::Dead, 0).await;
        }
    }

    /// Send one claimed webhook row and record what happened.
    async fn send_webhook(self: &Arc<Self>, me: &Arc<Intelli>, row: OutboxRow, claimed: Instant) {
        let onto = me.ontology().clone();
        let release = |reason: &'static str| self.release(me, &row, reason);
        // The endpoint is read again right before sending: a URL or secret
        // change applies to rows not yet sent.
        let (ep, kill) = {
            let (onto, name) = (onto.clone(), row.target.clone());
            match blocking(move || Ok((Endpoint::load(&onto, &name)?, killed(&onto)?))).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("webhook delivery: {e}");
                    return release("store error").await;
                }
            }
        };
        if kill {
            return release("kill switch").await;
        }
        let Some(ep) = ep.filter(|e| e.owns(&row)) else {
            // Removed by a person (perhaps added again since): what was
            // waiting for it is not sent.
            let a =
                Attempt { error: Some("endpoint_removed: the endpoint no longer exists".into()), ..Default::default() };
            return self.finish(me, &row, a, OutboxState::Cancelled, 0).await;
        };
        if !ep.enabled {
            return release("endpoint disabled").await;
        }
        if !self.webhooks_on {
            // Registered after this pass chose what to skip; the next pass skips it.
            return release("webhooks are off in this process").await;
        }
        let timeout = Duration::from_secs(self.cfg.timeout_secs);
        // Too little of the lease left to finish before another worker may claim it.
        if claimed.elapsed() + timeout + Duration::from_secs(5) >= Duration::from_millis(LEASE_MS as u64) {
            return release("lease too short").await;
        }
        let half_open = {
            let br = {
                let onto = onto.clone();
                blocking(move || breakers(&onto)).await.unwrap_or_default()
            };
            br.get(&ep.name).is_some_and(|b| b.is_open())
        };
        let Some(_permits) = self.acquire(&ep.name, half_open).await else {
            return release("endpoint at its limit").await;
        };
        let _done = Done { w: self.clone(), name: ep.name.clone(), probe: half_open };

        // Render. A change row is rendered from its record as the endpoint's
        // principal sees it now (its type follows: an update this principal
        // no longer sees is `onto.entity.hidden`); a change row without a
        // cursor is a gap notice, sent as stored.
        let mut event_type = row.event_type.clone();
        let (data, subject) = if let (true, Some(cursor)) = (row.source == SOURCE_CHANGE, row.cursor) {
            let (onto2, p) = (onto.clone(), ep.principal());
            let fields = ep.filter.as_ref().map(|f| f.fields).unwrap_or(FieldMode::Full);
            match blocking(move || onto2.change_at(&p, cursor, fields)).await {
                Ok(ChangeAt::Visible(r)) => {
                    let subject = Some(r.id.clone()).filter(|s| !s.is_empty());
                    event_type = format!("onto.{}", r.op.as_str());
                    (serde_json::to_value(&*r).unwrap_or(Value::Null), subject)
                }
                Ok(ChangeAt::NotVisible) => {
                    let a = Attempt { error: Some("not_visible".into()), ..Default::default() };
                    return self.finish(me, &row, a, OutboxState::Cancelled, 0).await;
                }
                Ok(ChangeAt::Expired) => {
                    let a = Attempt { error: Some("expired".into()), ..Default::default() };
                    return self.finish(me, &row, a, OutboxState::Dead, 0).await;
                }
                Err(e) => {
                    let a = Attempt { error: Some(format!("render: {e}")), ..Default::default() };
                    return self.finish(me, &row, a, OutboxState::Dead, 0).await;
                }
            }
        } else {
            let subject = row.payload.get("subject").and_then(Value::as_str).map(str::to_string);
            (row.payload.clone(), subject)
        };
        let mut ev = envelope(&row, &self.tenant, subject.as_deref(), data);
        ev["type"] = json!(event_type);
        let body = match serde_json::to_vec(&ev) {
            Ok(b) => b,
            Err(e) => {
                let a = Attempt { error: Some(format!("render: {e}")), ..Default::default() };
                return self.finish(me, &row, a, OutboxState::Dead, 0).await;
            }
        };
        if body.len() > self.cfg.max_payload_bytes {
            let a = Attempt { error: Some("payload_too_large".into()), ..Default::default() };
            return self.finish(me, &row, a, OutboxState::Dead, 0).await;
        }

        // Where to, and the keys: errors name references, never values.
        let what = if super::is_reference(&ep.url) {
            format!("the URL in {}", ep.url.trim())
        } else {
            format!("the URL of endpoint {}", ep.name)
        };
        let reach = Reach { restricted: ep.restricted, ..self.reach.clone() };
        let url = match resolve_text(&ep.url, &what).and_then(|u| check_endpoint_url(&ep.url, &u, &reach)) {
            Ok(u) => u,
            Err(e) => {
                let a = Attempt { error: Some(format!("refused: {e}")), ..Default::default() };
                return self.finish(me, &row, a, OutboxState::Dead, 0).await;
            }
        };
        let sealer = me.secrets();
        let keys = match ep.signing_keys(now_ms(), sealer.as_deref()) {
            Ok(k) => k,
            Err(e) => {
                // An operator can fix a missing variable or file: retry.
                return self
                    .failed(me, &row, &ep.name, Attempt { error: Some(e.to_string()), ..Default::default() }, None)
                    .await;
            }
        };
        let ts = now_ms() / 1000;
        let signature = signature_header(&keys, &row.id, ts, &body);
        let client = if ep.restricted { &self.clients.1 } else { &self.clients.0 };
        let req = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/cloudevents+json")
            .header(reqwest::header::USER_AGENT, format!("onto-intelli/{}", crate::VERSION))
            .header("webhook-id", &row.id)
            .header("webhook-timestamp", ts.to_string())
            .header("webhook-signature", signature)
            .body(body);
        let req = traced(req, &row);
        let reference = super::is_reference(&ep.url).then_some(what);
        if let Some(mut a) = self.exchange(me, &row, &ep.name, req, reference.as_deref()).await {
            a.error = Some("http 410: the endpoint is gone and was disabled".into());
            self.disable(me, &ep).await;
            self.finish(me, &row, a, OutboxState::Dead, 0).await;
        }
    }

    /// Send `req` for `row` (to endpoint or target `name`) and record the
    /// outcome: delivered, retried (408, 409, 425, 429, 5xx, timeouts and
    /// connection errors, honouring `Retry-After`) or dead (a refused
    /// address, other answers). A 410 is returned unrecorded for the caller
    /// to settle. `reference` names the URL when it came from an operator
    /// reference: then no error text of the client is stored (a refused
    /// address, a DNS name or a TLS name mismatch would echo part of the
    /// reference's value), only what kind of failure it was.
    async fn exchange(
        &self,
        me: &Arc<Intelli>,
        row: &OutboxRow,
        name: &str,
        req: reqwest::RequestBuilder,
        reference: Option<&str>,
    ) -> Option<Attempt> {
        let started = Instant::now();
        let sent = req.send().await;
        let duration_ms = started.elapsed().as_millis() as u64;
        match sent {
            Err(e) => {
                let (error, refused) = send_error(&chain(&e), e.is_timeout(), e.is_connect(), reference);
                let a = Attempt { error: Some(error), duration_ms, ..Default::default() };
                if refused {
                    self.finish(me, row, a, OutboxState::Dead, 0).await
                } else {
                    self.failed(me, row, name, a, None).await
                }
                None
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after =
                    resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).map(str::to_string);
                let response = read_some(resp).await;
                let mut a = Attempt { status: Some(status), duration_ms, response, ..Default::default() };
                match classify(status) {
                    Class::Delivered => {
                        self.succeeded(me, row.kind, name).await;
                        self.finish(me, row, a, OutboxState::Delivered, 0).await;
                    }
                    Class::Retry => {
                        a.error = Some(format!("http {status}"));
                        let max = self.cfg.backoff_max_secs as i64 * 1000;
                        let after = retry_after_ms(retry_after.as_deref(), max);
                        self.failed(me, row, name, a, after).await;
                    }
                    Class::Gone => return Some(a),
                    Class::Dead => {
                        a.error = Some(format!("http {status}"));
                        self.finish(me, row, a, OutboxState::Dead, 0).await;
                    }
                }
                None
            }
        }
    }

    /// Permits for one request: the endpoint's (a single probe while
    /// half-open), the tenant's and the process's. `None`: the endpoint is
    /// at its limit.
    async fn acquire(
        &self,
        name: &str,
        half_open: bool,
    ) -> Option<(tokio::sync::OwnedSemaphorePermit, tokio::sync::OwnedSemaphorePermit)> {
        {
            let mut eps = self.endpoints.lock();
            let s = eps.entry(name.to_string()).or_default();
            if (half_open && (s.probing || s.in_flight > 0)) || s.in_flight >= self.cfg.max_in_flight_per_endpoint {
                return None;
            }
            s.in_flight += 1;
            if half_open {
                s.probing = true;
            }
        }
        let t = self.tenant_slots.clone().acquire_owned().await.ok()?;
        let p = self.process.clone().acquire_owned().await.ok()?;
        Some((t, p))
    }

    /// A failure that may be retried: retry (or dead after `max_attempts`),
    /// and count it against the endpoint's breaker.
    async fn failed(&self, me: &Arc<Intelli>, row: &OutboxRow, name: &str, a: Attempt, retry_after: Option<i64>) {
        let max = self.cfg.backoff_max_secs as i64 * 1000;
        let base = self.cfg.backoff_base_secs as i64 * 1000;
        if row.attempts >= self.cfg.max_attempts {
            let mut a = a;
            a.error = Some(format!("{} (max_attempts {} reached)", a.error.unwrap_or_default(), self.cfg.max_attempts));
            self.finish(me, row, a, OutboxState::Dead, 0).await;
        } else {
            let delay = retry_after.unwrap_or_else(|| backoff_ms(row.attempts, base, max, jitter()));
            self.finish(me, row, a, OutboxState::Retry, delay).await;
        }
        self.trip(me, row.kind, name).await;
    }

    async fn finish(&self, me: &Arc<Intelli>, row: &OutboxRow, a: Attempt, next: OutboxState, delay: i64) {
        let store = me.ontology().store().clone();
        let (id, claim) = (row.id.clone(), row.claim.clone().unwrap_or_default());
        let a2 = a.clone();
        match blocking(move || store.outbox_finish(&id, &claim, &a2, next, delay)).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(id = %row.id, "outbox row was claimed again after its lease ended; this attempt is not recorded");
                return;
            }
            Err(e) => {
                tracing::warn!(id = %row.id, "recording a delivery attempt: {e}");
                return;
            }
        }
        self.trace_attempt(me, row, &a, next, delay);
        if next == OutboxState::Dead {
            match row.kind {
                OutboxKind::Webhook => {
                    let payload = json!({
                        "id": row.id, "endpoint": row.target, "event_type": row.event_type,
                        "attempts": row.attempts, "status": a.status, "reason": a.error,
                    });
                    self.event(me, &row.workspace, "webhook.dead", &format!("webhook:{}", row.target), payload).await;
                }
                OutboxKind::Writeback => {
                    // Not reversible, and not sent: a person decides what to do.
                    let payload = json!({
                        "id": row.id, "target": row.target, "route": row.event_type, "receipt": row.receipt,
                        "idempotency_key": row.dedupe, "attempts": row.attempts, "status": a.status,
                        "reason": a.error,
                    });
                    let target = format!("writeback:{}", row.target);
                    self.event(me, &row.workspace, "writeback.failed", &target, payload).await;
                }
            }
        }
    }

    /// Record the attempt as a `delivery` trace when `[traces] deliveries`
    /// selects it.
    fn trace_attempt(&self, me: &Arc<Intelli>, row: &OutboxRow, a: &Attempt, next: OutboxState, delay: i64) {
        let failed = matches!(next, OutboxState::Retry | OutboxState::Dead);
        let wanted = match me.config().traces.deliveries.as_str() {
            "all" => true,
            "failures" => failed,
            _ => false,
        };
        if !wanted || !me.traces_enabled() {
            return;
        }
        let Ok(at) = ATTEMPT.try_with(Clone::clone) else { return };
        let ws = if row.workspace.is_empty() { me.default_workspace().to_string() } else { row.workspace.clone() };
        me.send_trace(delivery_trace(&at, row, a, next, delay, ws));
    }

    async fn event(&self, me: &Arc<Intelli>, workspace: &str, action: &str, target: &str, payload: Value) {
        let onto = me.ontology().clone();
        let (ws, action, target) = (workspace.to_string(), action.to_string(), target.to_string());
        let r = blocking(move || {
            let ctx = system_ctx(&onto, &ws);
            let ev = EventInput {
                action,
                target: Some(target),
                workspace: Some(ctx.workspace.clone()),
                payload,
                ..Default::default()
            };
            onto.apply(&ctx, vec![Mutation::Event(ev)]).map(drop)
        })
        .await;
        if let Err(e) = r {
            tracing::warn!("webhook event: {e}");
        }
    }

    /// A success closes the endpoint's (or target's) breaker.
    async fn succeeded(&self, me: &Arc<Intelli>, kind: OutboxKind, name: &str) {
        self.endpoints.lock().entry(name.to_string()).or_default().failures = 0;
        let onto = me.ontology().clone();
        let n = name.to_string();
        let was_open =
            blocking(move || Ok(breakers(&onto)?.get(&n).is_some_and(|b| b.is_open()))).await.unwrap_or(false);
        if was_open {
            let closed = BreakerState { state: "closed".into(), ..Default::default() };
            let action = format!("{}.resumed", kind_words(kind).1);
            self.put_breaker(me, kind, name, closed, &action, json!({ "reason": "a probe was delivered" })).await;
        }
    }

    /// A failure: open the breaker after `pause_after_failures` in a row, or
    /// again (with a doubled pause) when a half-open probe failed.
    async fn trip(&self, me: &Arc<Intelli>, kind: OutboxKind, name: &str) {
        let failures = {
            let mut eps = self.endpoints.lock();
            let s = eps.entry(name.to_string()).or_default();
            s.failures += 1;
            s.failures
        };
        let onto = me.ontology().clone();
        let n = name.to_string();
        let current = blocking(move || Ok(breakers(&onto)?.remove(&n))).await.unwrap_or(None);
        let probe_failed = current.as_ref().is_some_and(|b| b.is_open() && b.until <= now_ms());
        if !(probe_failed
            || (failures >= self.cfg.pause_after_failures && !current.as_ref().is_some_and(|b| b.is_open())))
        {
            return;
        }
        let base = self.cfg.pause_secs as i64 * 1000;
        let pause = next_pause(current.filter(|b| b.is_open()).map(|b| b.pause_ms), base);
        let b = BreakerState { state: "open".into(), until: now_ms() + pause, pause_ms: pause, failures };
        let payload = json!({ "reason": "failing", "failures": failures, "pause_ms": pause });
        let action = format!("{}.paused", kind_words(kind).1);
        self.put_breaker(me, kind, name, b, &action, payload).await;
    }

    async fn put_breaker(
        &self,
        me: &Arc<Intelli>,
        kind: OutboxKind,
        name: &str,
        b: BreakerState,
        action: &str,
        payload: Value,
    ) {
        let onto = me.ontology().clone();
        let (name, action) = (name.to_string(), action.to_string());
        let audit = format!("{}:{name}", kind_words(kind).0);
        let r = blocking(move || {
            let ctx = system_ctx(&onto, "");
            let kv = vec![(NS_CONTROL.to_string(), format!("{BREAKER_PREFIX}{name}"), serde_json::to_value(&b)?)];
            onto.commit_config(&ctx, kv, ConfigEvent::new(action, Some(&audit), payload)).map(drop)
        })
        .await;
        if let Err(e) = r {
            tracing::warn!("webhook breaker: {e}");
        }
    }

    /// 410 Gone: the endpoint is disabled until a person enables it again.
    async fn disable(&self, me: &Arc<Intelli>, ep: &Endpoint) {
        let onto = me.ontology().clone();
        let name = ep.name.clone();
        let r = blocking(move || {
            let Some(mut ep) = Endpoint::load(&onto, &name)? else { return Ok(()) };
            if !ep.enabled {
                return Ok(());
            }
            ep.enabled = false;
            ep.version += 1;
            let ctx = system_ctx(&onto, "");
            let kv = vec![(NS_ENDPOINT.to_string(), name.clone(), serde_json::to_value(&ep)?)];
            let ev = ConfigEvent::new(
                "webhook.endpoint.disabled",
                Some(&format!("webhook:{name}")),
                json!({ "reason": "http 410" }),
            );
            onto.commit_config(&ctx, kv, ev).map(drop)
        })
        .await;
        if let Err(e) = r {
            tracing::warn!("disabling a gone webhook endpoint: {e}");
        }
    }
}

/// When the tenant next has work, from the earliest due outbox row and the
/// earliest retry timer: `(wake row, this worker)`. The worker waits on
/// outbox rows only (retry timers are the automation consumer's); the wake
/// row also counts timers, but only from a process that runs the consumer,
/// so a process with automations off never keeps a tenant awake for them.
fn next_due(outbox: Option<i64>, timers: Option<i64>, consumer: bool) -> (Option<i64>, Option<i64>) {
    let wake = match (outbox, timers.filter(|_| consumer)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    (wake, outbox)
}

/// Releases an endpoint's in-flight slot (and its probe) when a send ends.
struct Done {
    w: Arc<Worker>,
    name: String,
    probe: bool,
}

impl Drop for Done {
    fn drop(&mut self) {
        let mut eps = self.w.endpoints.lock();
        if let Some(s) = eps.get_mut(&self.name) {
            s.in_flight = s.in_flight.saturating_sub(1);
            if self.probe {
                s.probing = false;
            }
        }
    }
}

/// `req` with a `traceparent` header: the trace `row` continues (else the
/// attempt's own) and the attempt's span.
fn traced(req: reqwest::RequestBuilder, row: &OutboxRow) -> reqwest::RequestBuilder {
    let at = ATTEMPT.try_with(Clone::clone).unwrap_or_else(|_| AttemptTrace::new());
    req.header("traceparent", attempt_traceparent(&at, row))
}

/// The `traceparent` an attempt sends.
fn attempt_traceparent(at: &AttemptTrace, row: &OutboxRow) -> String {
    let trace = row.otel_trace_id.as_deref().filter(|t| is_trace_id(t)).unwrap_or(&at.id);
    traceparent(trace, &at.span)
}

/// The `delivery` trace of one attempt (`[traces] deliveries`): its root
/// span is the one the request named in `traceparent`, with a child for
/// the HTTP exchange when a request was made. It keeps the row's id,
/// target, source, attempt and outcome, the receiver's status and the
/// attempt's error (which never holds a secret or a referenced URL), never
/// the body, the headers (the signature among them), the receiver's answer
/// or the URL.
fn delivery_trace(
    at: &AttemptTrace,
    row: &OutboxRow,
    a: &Attempt,
    next: OutboxState,
    delay: i64,
    workspace: String,
) -> TraceRecord {
    let (word, principal) = match row.kind {
        OutboxKind::Webhook => ("webhook", format!("webhook:{}", row.target)),
        OutboxKind::Writeback => ("writeback", format!("writeback:{}", row.target)),
    };
    let mut t = crate::trace::worker_row(
        at.id.clone(),
        "delivery",
        format!("{word}.deliver:{}", row.target),
        workspace,
        principal,
        row.otel_trace_id.clone(),
    );
    t.ts = at.start_ms;
    t.duration_ms = at.started.elapsed().as_secs_f64() * 1000.0;
    t.request_id = Some(row.id.clone());
    let failed = matches!(next, OutboxState::Retry | OutboxState::Dead);
    if next != OutboxState::Delivered {
        t.error_code = Some(next.as_str().to_string());
        t.error_message = a.error.as_deref().map(|e| crate::trace::clip_bytes(e, 500));
    }
    if failed {
        t.status = "error".into();
    }
    let mut root = crate::trace::worker_span(&t, at.span.clone(), None, format!("{word}.deliver"), t.ts, t.duration_ms);
    root.status = t.status.clone();
    root.error = t.error_message.clone();
    root.attrs = json!({
        "outbox": row.id,
        "kind": row.kind.as_str(),
        "target": row.target,
        "source": row.source,
        "event_type": row.event_type,
        "attempt": row.attempts,
        "outcome": next.as_str(),
        "http_status": a.status,
        "retry_in_ms": (next == OutboxState::Retry).then_some(delay),
        "receipt": row.receipt,
        "run": row.run,
    });
    let mut spans = vec![root];
    if a.status.is_some() || a.duration_ms > 0 {
        let child = at.span[..8].to_string() + "00000002";
        let ts = (now_ms() - a.duration_ms as i64).max(t.ts);
        let mut s = crate::trace::worker_span(&t, child, Some(&at.span), "http.post".into(), ts, a.duration_ms as f64);
        s.attrs = json!({ "http_status": a.status });
        if failed {
            s.status = "error".into();
        }
        spans.push(s);
    }
    TraceRecord { trace: t, spans }
}

/// What an attempt records of a failed send, and whether the outbound rules
/// refused the address. With `reference` (the URL came from an operator
/// `env:`/`file:` reference) the client's text is never stored: it may hold
/// the host (a DNS failure, a certificate that is not valid for the name),
/// which is part of the reference's value; the error names the reference
/// and the kind of failure only.
fn send_error(text: &str, timeout: bool, connect: bool, reference: Option<&str>) -> (String, bool) {
    let refused = text.contains("refused:");
    let Some(what) = reference else { return (text.to_string(), refused) };
    if refused {
        return (format!("refused: {what} resolves to an address the outbound rules refuse"), true);
    }
    let lower = text.to_ascii_lowercase();
    // TLS first: a certificate error names DNS names too.
    let kind = if timeout {
        "timeout"
    } else if ["certificate", "tls", "ssl", "handshake"].iter().any(|w| lower.contains(w)) {
        "TLS error"
    } else if lower.contains("dns") || lower.contains("resolv") || lower.contains("lookup") {
        "DNS error"
    } else if connect {
        "connect error"
    } else {
        "request error"
    };
    (format!("the request to {what} failed: {kind}"), false)
}

/// The error and its sources, without the request URL.
fn chain(e: &reqwest::Error) -> String {
    let mut out = if e.is_timeout() { "timeout".to_string() } else { String::new() };
    let mut cur: Option<&dyn std::error::Error> = Some(e);
    while let Some(err) = cur {
        let msg = err.to_string();
        // reqwest prints the URL in its own message; the sources say what failed.
        let msg = match msg.find(" for url (") {
            Some(i) => msg[..i].to_string(),
            None => msg,
        };
        if !msg.is_empty() && !out.contains(&msg) {
            if !out.is_empty() {
                out.push_str(": ");
            }
            out.push_str(&msg);
        }
        cur = err.source();
    }
    out
}

/// At most [`onto_core::outbox::MAX_RESPONSE_BYTES`] of the response body.
async fn read_some(mut resp: reqwest::Response) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < onto_core::outbox::MAX_RESPONSE_BYTES {
        match resp.chunk().await {
            Ok(Some(c)) => buf.extend_from_slice(&c),
            _ => break,
        }
    }
    if buf.is_empty() {
        return None;
    }
    Some(Attempt::clip(&String::from_utf8_lossy(&buf)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worker_waits_on_outbox_rows_and_wakes_for_timers_only_with_a_consumer() {
        // A due retry timer and no consumer here: neither the wake row nor
        // the worker's own sleep counts it (no busy loop, no wake churn).
        assert_eq!(next_due(None, Some(5), false), (None, None));
        assert_eq!(next_due(Some(9), Some(5), false), (Some(9), Some(9)));
        // A process that runs the consumer keeps the tenant's wake row for it,
        // but the delivery worker itself still waits on its own rows only.
        assert_eq!(next_due(None, Some(5), true), (Some(5), None));
        assert_eq!(next_due(Some(9), Some(5), true), (Some(5), Some(9)));
        assert_eq!(next_due(Some(3), None, true), (Some(3), Some(3)));
    }

    #[test]
    fn errors_of_a_referenced_url_never_echo_its_host() {
        let tls = "error sending request: client error (Connect): invalid peer certificate: certificate not valid \
                   for name \"review-hidden-host.example\"; certificate is only valid for DnsName(\"other.example\")";
        let (e, refused) = send_error(tls, false, true, Some("the URL in env:HOOK_URL"));
        assert!(!refused);
        assert_eq!(e, "the request to the URL in env:HOOK_URL failed: TLS error");
        assert!(!e.contains("review-hidden-host") && !e.contains("other.example"));
        let dns = "error sending request: client error (Connect): dns error: failed to lookup address \
                   information: review-hidden-host.invalid";
        let (e, _) = send_error(dns, false, true, Some("the URL in env:HOOK_URL"));
        assert_eq!(e, "the request to the URL in env:HOOK_URL failed: DNS error");
        let (e, _) = send_error("operation timed out", true, false, Some("the URL in file:/run/hook"));
        assert_eq!(e, "the request to the URL in file:/run/hook failed: timeout");
        let (e, _) = send_error("tcp connect error: 10.1.2.3:443", false, true, Some("the URL in env:X"));
        assert_eq!(e, "the request to the URL in env:X failed: connect error");
        let (e, refused) =
            send_error("client error (Connect): refused: 10.0.0.7 is private", false, true, Some("the URL in env:X"));
        assert!(refused && e.starts_with("refused: the URL in env:X") && !e.contains("10.0.0.7"), "{e}");
        // A URL typed into the endpoint is the caller's own: its text is kept.
        let (e, refused) = send_error(tls, false, true, None);
        assert!(!refused && e.contains("certificate not valid"));
    }

    /// What an attempt records (and the API shows) keeps the cause and leaves
    /// out the URL, which can hold a password or a signed token.
    #[tokio::test]
    async fn the_error_chain_has_the_cause_and_not_the_url() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let url = format!("http://hook:s3cret-pw@127.0.0.1:{port}/hooks/s3cret-token?sig=s3cret-sig");
        let e = reqwest::Client::new().post(url).send().await.unwrap_err();
        assert!(e.to_string().contains("s3cret-token"), "reqwest prints the URL itself: {e}");
        let chain = chain(&e);
        assert!(chain.starts_with("error sending request"), "{chain}");
        assert!(chain.to_lowercase().contains("connect"), "the cause is kept: {chain}");
        assert!(!chain.contains("s3cret"), "{chain}");
    }
}
