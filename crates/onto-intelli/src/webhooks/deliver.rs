//! The delivery worker of one open tenant (docs/design/EVENTS.md B.4).
//!
//! A pass claims due rows (claim tokens, a lease by the store's clock),
//! sends each with the guarded client within the per-endpoint, per-tenant
//! and process-wide limits (all per process), records the attempt with the
//! claim token it was made under, and updates the endpoint's breaker. It
//! wakes on commits that enqueue rows, on other processes' commits, and
//! every `poll_secs` (or sooner, when a retry is due sooner); after a pass
//! that found nothing more it settles the tenant's wake row, so a closed
//! tenant is woken again only when work is due.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};

use onto_core::changes::{ChangeAt, FieldMode};
use onto_core::outbox::{Attempt, ConfigEvent, OutboxRow, OutboxState, SOURCE_CHANGE};
use onto_core::time::now_ms;
use onto_core::{Error, EventInput, Mutation, Ontology, Result};

use super::{
    BREAKER_PREFIX, BreakerState, CLAIM_LIMIT, Class, Endpoint, LEASE_MS, NS_CONTROL, NS_ENDPOINT, backoff_ms,
    classify, envelope, jitter, killed, next_pause, resolve_text, retry_after_ms, signature_header, system_ctx,
};
use crate::config::WebhooksConfig;
use crate::intelli::Intelli;
use crate::net::{Reach, check_target_url, guarded_client};

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
    holder: String,
    reach: Reach,
    tenant: String,
    endpoints: Mutex<HashMap<String, EndpointState>>,
    tenant_slots: Arc<Semaphore>,
    process: Arc<Semaphore>,
    /// Clients for open and for restricted endpoints.
    clients: (reqwest::Client, reqwest::Client),
}

/// Start the delivery worker of `me` (when `[webhooks] enabled` and the
/// store keeps an outbox). It holds the tenant only weakly and ends when the
/// tenant closes.
pub fn spawn_delivery(me: &Arc<Intelli>, notify: Arc<Notify>) -> Result<()> {
    let cfg = me.config().webhooks.clone();
    let store = me.ontology().store().clone();
    if !cfg.enabled || !store.keeps_outbox() {
        return Ok(());
    }
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
        reach: open,
        endpoints: Mutex::new(HashMap::new()),
        tenant_slots: Arc::new(Semaphore::new(cfg.max_in_flight_per_tenant.max(1))),
        process: process_slots(cfg.max_in_flight_process),
        cfg,
    });
    let mut closing = me.closing_signal();
    tokio::spawn(async move {
        let poll = Duration::from_secs(worker.cfg.poll_secs.max(1));
        let mut vacuumed = Instant::now();
        loop {
            let wait = {
                let Some(me) = worker.weak.upgrade() else { break };
                if me.is_closing() {
                    break;
                }
                if vacuumed.elapsed() >= Duration::from_secs(3600) {
                    vacuumed = Instant::now();
                    let store = me.ontology().store().clone();
                    let before = now_ms() - worker.cfg.retain_days as i64 * 86_400_000;
                    match blocking(move || store.outbox_vacuum(before)).await {
                        Ok(n) if n > 0 => tracing::info!(rows = n, "outbox retention"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("outbox retention: {e}"),
                    }
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
            let (seen, stop, skip) = {
                let onto = onto.clone();
                let cap = self.cfg.max_in_flight_per_endpoint;
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
                    for ep in Endpoint::list(&onto)? {
                        let paused = ep.paused.as_ref().is_some_and(|p| p.until.is_none_or(|u| u > now));
                        let (in_flight, probing) = states.get(&ep.name).copied().unwrap_or((0, false));
                        let open = br.get(&ep.name).is_some_and(|b| b.is_open() && (b.until > now || probing));
                        if !ep.enabled || paused || open || in_flight >= cap {
                            skip.push(ep.name);
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
            let claimed = Instant::now();
            let mut tasks = tokio::task::JoinSet::new();
            for row in rows {
                let (w, me) = (self.clone(), me.clone());
                tasks.spawn(async move { w.send(&me, row, claimed).await });
            }
            while let Some(r) = tasks.join_next().await {
                if let Err(e) = r {
                    tracing::warn!("webhook delivery task: {e}");
                }
            }
        }
    }

    /// Nothing more is due now: set the tenant's next wake time (unless it
    /// was bumped meanwhile, then drain again) and say when to look again.
    async fn settle(&self, onto: &Ontology, seen: Option<u64>) -> Result<Option<Duration>> {
        let store = onto.store().clone();
        blocking(move || {
            let next = match (store.outbox_next_due()?, store.timers_next().unwrap_or(None)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            if let Some(seen) = seen
                && !store.wake_settle(seen, next)?
            {
                return Ok(Some(Duration::ZERO));
            }
            let now = store.clock_ms()?;
            Ok(next.map(|n| Duration::from_millis((n - now).max(0) as u64 + 5)))
        })
        .await
    }

    /// Send one claimed row and record what happened.
    async fn send(self: &Arc<Self>, me: &Arc<Intelli>, row: OutboxRow, claimed: Instant) {
        let onto = me.ontology().clone();
        let claim = row.claim.clone().unwrap_or_default();
        let release = |reason: &'static str| {
            let (store, id, claim) = (onto.store().clone(), row.id.clone(), claim.clone());
            async move {
                tracing::debug!(id, reason, "outbox row released unsent");
                let _ = blocking(move || store.outbox_release(&id, &claim)).await;
            }
        };
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
        let Some(ep) = ep else {
            let a =
                Attempt { error: Some("endpoint_missing: the endpoint no longer exists".into()), ..Default::default() };
            return self.finish(me, &row, a, OutboxState::Dead, 0).await;
        };
        if !ep.enabled {
            return release("endpoint disabled").await;
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

        // Render.
        let (data, subject) = if row.source == SOURCE_CHANGE {
            let Some(cursor) = row.cursor else {
                let a = Attempt { error: Some("no cursor on a change row".into()), ..Default::default() };
                return self.finish(me, &row, a, OutboxState::Dead, 0).await;
            };
            let (onto2, p) = (onto.clone(), ep.principal());
            let fields = ep.filter.as_ref().map(|f| f.fields).unwrap_or(FieldMode::Full);
            match blocking(move || onto2.change_at(&p, cursor, fields)).await {
                Ok(ChangeAt::Visible(r)) => {
                    let subject = Some(r.id.clone()).filter(|s| !s.is_empty());
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
        let body = match serde_json::to_vec(&envelope(&row, &self.tenant, subject.as_deref(), data)) {
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
        let url = match resolve_text(&ep.url, &what).and_then(|u| check_target_url(&u, &what, &reach)) {
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
        let mut req = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/cloudevents+json")
            .header(reqwest::header::USER_AGENT, format!("onto-intelli/{}", crate::VERSION))
            .header("webhook-id", &row.id)
            .header("webhook-timestamp", ts.to_string())
            .header("webhook-signature", signature)
            .body(body);
        if let Some(t) =
            row.otel_trace_id.as_deref().filter(|t| t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            req = req.header("traceparent", format!("00-{t}-{:016x}-01", nonzero_span()));
        }
        let started = Instant::now();
        let sent = req.send().await;
        let duration_ms = started.elapsed().as_millis() as u64;
        match sent {
            Err(e) => {
                let refused = chain(&e).contains("refused:");
                let a = Attempt { error: Some(chain(&e)), duration_ms, ..Default::default() };
                if refused {
                    self.finish(me, &row, a, OutboxState::Dead, 0).await
                } else {
                    self.failed(me, &row, &ep.name, a, None).await
                }
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                let retry_after =
                    resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).map(str::to_string);
                let response = read_some(resp).await;
                let mut a = Attempt { status: Some(status), duration_ms, response, ..Default::default() };
                match classify(status) {
                    Class::Delivered => {
                        self.succeeded(me, &ep.name).await;
                        self.finish(me, &row, a, OutboxState::Delivered, 0).await
                    }
                    Class::Retry => {
                        a.error = Some(format!("http {status}"));
                        let max = self.cfg.backoff_max_secs as i64 * 1000;
                        let after = retry_after_ms(retry_after.as_deref(), max);
                        self.failed(me, &row, &ep.name, a, after).await
                    }
                    Class::Gone => {
                        a.error = Some("http 410: the endpoint is gone and was disabled".into());
                        self.disable(me, &ep).await;
                        self.finish(me, &row, a, OutboxState::Dead, 0).await
                    }
                    Class::Dead => {
                        a.error = Some(format!("http {status}"));
                        self.finish(me, &row, a, OutboxState::Dead, 0).await
                    }
                }
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
        self.trip(me, name).await;
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
        if next == OutboxState::Dead {
            let payload = json!({
                "id": row.id, "endpoint": row.target, "event_type": row.event_type,
                "attempts": row.attempts, "status": a.status, "reason": a.error,
            });
            self.event(me, &row.workspace, "webhook.dead", &row.target, payload).await;
        }
    }

    async fn event(&self, me: &Arc<Intelli>, workspace: &str, action: &str, target: &str, payload: Value) {
        let onto = me.ontology().clone();
        let (ws, action, target) = (workspace.to_string(), action.to_string(), target.to_string());
        let r = blocking(move || {
            let ctx = system_ctx(&onto, &ws);
            let ev = EventInput {
                action,
                target: Some(format!("webhook:{target}")),
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

    /// A success closes the endpoint's breaker.
    async fn succeeded(&self, me: &Arc<Intelli>, name: &str) {
        self.endpoints.lock().entry(name.to_string()).or_default().failures = 0;
        let onto = me.ontology().clone();
        let n = name.to_string();
        let was_open =
            blocking(move || Ok(breakers(&onto)?.get(&n).is_some_and(|b| b.is_open()))).await.unwrap_or(false);
        if was_open {
            let closed = BreakerState { state: "closed".into(), ..Default::default() };
            self.put_breaker(
                me,
                name,
                closed,
                "webhook.endpoint.resumed",
                json!({ "reason": "a probe was delivered" }),
            )
            .await;
        }
    }

    /// A failure: open the breaker after `pause_after_failures` in a row, or
    /// again (with a doubled pause) when a half-open probe failed.
    async fn trip(&self, me: &Arc<Intelli>, name: &str) {
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
        self.put_breaker(me, name, b, "webhook.endpoint.paused", payload).await;
    }

    async fn put_breaker(&self, me: &Arc<Intelli>, name: &str, b: BreakerState, action: &str, payload: Value) {
        let onto = me.ontology().clone();
        let (name, action) = (name.to_string(), action.to_string());
        let r = blocking(move || {
            let ctx = system_ctx(&onto, "");
            let kv = vec![(NS_CONTROL.to_string(), format!("{BREAKER_PREFIX}{name}"), serde_json::to_value(&b)?)];
            onto.commit_config(&ctx, kv, ConfigEvent::new(action, Some(&format!("webhook:{name}")), payload)).map(drop)
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

fn nonzero_span() -> u64 {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    u64::from_le_bytes(b).max(1)
}
