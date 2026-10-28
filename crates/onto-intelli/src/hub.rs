//! Many tenants in one process. Each tenant is its own ontology (its own
//! PostgreSQL schema, or an in-memory store) with its own schema
//! versions, proposals, memory and caches. The LLM provider is the server's
//! unless [`TenantHooks`] (the control plane) give a tenant its own, with
//! its limits and usage meter. Tenants open on first use. PostgreSQL
//! tenants close again when idle (`engine.tenant_idle_secs`), when more than
//! `engine.max_open_tenants` are open, or while the open ones' estimated
//! memory is above `engine.tenant_memory_budget_mb` (least recently used
//! first); the next request opens them again. In-memory tenants stay open:
//! their data lives only there.
//!
//! On PostgreSQL, one process per deployment can be the elected **wake
//! scanner** ([`Hub::start_wake_scanner`]): tenants with background work due
//! bump a row in the deployment's shared `wake` table at write time, and the
//! scanner opens the closed ones, so closed tenants are never polled.
//!
//! An application with workspaces maps each workspace to a tenant and passes
//! the caller's principal on every call:
//!
//! ```no_run
//! # async fn demo() -> onto_intelli::Result<()> {
//! use onto_intelli::{Config, Hub, Principal, Role};
//! let hub = Hub::open(Config::in_memory()).await?;
//! let ws = hub.tenant("ws_42").await?;
//! let alice = Principal::new("user:alice", vec![Role::Member], vec!["*".into()]).with_entity("person:alice");
//! ws.call(&alice, "entity.find", serde_json::json!({ "kind": "issue" })).await?;
//! # Ok(()) }
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use tokio::sync::OnceCell;

use onto_core::store::{MemoryStore, Store};
use onto_core::time::now_ms;
use onto_core::{Error, Result};

use crate::config::Config;
use crate::intelli::Intelli;
use crate::limits::{Limits, UsageMeter};
use crate::llm::{self, LlmProvider};

/// What the control plane sets for one tenant (a project).
#[derive(Clone, Default)]
pub struct TenantSettings {
    /// The project's own provider; `None` uses the server's `[llm]`.
    pub llm: Option<Arc<dyn LlmProvider>>,
    pub limits: Limits,
    pub meter: Option<Arc<dyn UsageMeter>>,
    /// The project's `settings_version` these settings were read at.
    pub version: u64,
    /// Trace retention and content capture (`None` fields: `[traces]`).
    pub traces: crate::trace::TraceSettings,
    /// Seals and opens the tenant's outbound secrets (webhook signing
    /// secrets); `None` without `ONTO_MASTER_KEY`: plaintext and generated
    /// secrets are refused, the operator registers `env:` or `file:` ones.
    pub secrets: Option<Arc<dyn SecretBox>>,
}

/// A secret in memory: never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// Envelope encryption of a tenant's outbound secrets (the control plane's
/// vault). `purpose` (`webhook-secret`, …) and `name` (the endpoint) are
/// bound to the sealed value, with the project, so a sealed value cannot be
/// moved to another endpoint or project. Errors never contain the secret.
pub trait SecretBox: Send + Sync {
    /// The sealed value (base64), stored as `sealed:v1:<value>`.
    fn seal(&self, purpose: &str, name: &str, plain: &[u8]) -> Result<String>;
    /// The plaintext of a value [`seal`](Self::seal) returned for the same purpose and name.
    fn open(&self, purpose: &str, name: &str, sealed: &str) -> Result<Secret>;
}

impl std::fmt::Debug for TenantSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantSettings")
            .field("llm", &self.llm.as_ref().map(|l| format!("{}/{}", l.name(), l.model())))
            .field("limits", &self.limits)
            .field("meter", &self.meter.is_some())
            .field("version", &self.version)
            .field("traces", &self.traces)
            .field("secrets", &self.secrets.is_some())
            .finish()
    }
}

/// Per-tenant settings from outside the engine (the control plane).
#[async_trait]
pub trait TenantHooks: Send + Sync {
    /// Settings for a tenant; called when it opens and on refresh.
    async fn settings(&self, tenant: &str) -> Result<TenantSettings>;
}

enum Backend {
    Memory,
    #[cfg(feature = "postgres")]
    Postgres(onto_core::store::PgShared),
    /// One ontology opened elsewhere (`Hub::single`).
    Fixed,
}

/// An open (or opening) tenant and when it was last used.
struct Slot {
    cell: OnceCell<Arc<Intelli>>,
    last_used: AtomicI64,
}

impl Slot {
    fn new() -> Arc<Self> {
        Arc::new(Self { cell: OnceCell::new(), last_used: AtomicI64::new(now_ms()) })
    }
}

/// One open tenant, as `GET /v1/admin/tenants` reports it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TenantStat {
    pub tenant: String,
    /// [`Intelli::estimated_bytes`]: the ontology plus the change stream ring.
    pub estimated_bytes: u64,
    pub entities: usize,
    pub links: usize,
    /// Milliseconds since the last request.
    pub idle_ms: i64,
    /// Open change streams in this process.
    pub streams: usize,
    /// Memory of the change stream ring (0 without streams).
    pub ring_bytes: u64,
    /// [`Intelli::outbound_counters`]: outbox rows per state, change lag,
    /// gaps, runs and paused counts. Only [`Hub::tenant_report`] reads
    /// them: about eight store round trips per tenant (`{"timed_out":
    /// true}` when they take longer than [`REPORT_TENANT_TIMEOUT`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbound: Option<serde_json::Value>,
}

/// Tenants whose outbound counters [`Hub::tenant_report`] reads at once.
pub const REPORT_CONCURRENCY: usize = 8;
/// Longest [`Hub::tenant_report`] waits for one tenant's outbound counters.
pub const REPORT_TENANT_TIMEOUT: Duration = Duration::from_secs(5);

/// `f` over `items` on blocking threads, at most `limit` at a time, each
/// given up after `timeout` (`None`; its thread finishes on its own). In
/// the order of `items`.
async fn each_bounded<T, R>(
    items: Vec<T>,
    limit: usize,
    timeout: Duration,
    f: impl Fn(T) -> R + Clone + Send + 'static,
) -> Vec<Option<R>>
where
    T: Send + 'static,
    R: Send + 'static,
{
    use futures::StreamExt as _;
    futures::stream::iter(items.into_iter().map(|t| {
        let f = f.clone();
        async move { tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || f(t))).await.ok()?.ok() }
    }))
    .buffered(limit.max(1))
    .collect()
    .await
}

pub struct Hub {
    config: Config,
    llm: Arc<dyn LlmProvider>,
    backend: Backend,
    default_tenant: String,
    tenants: Mutex<HashMap<String, Arc<Slot>>>,
    hooks: RwLock<Option<Arc<dyn TenantHooks>>>,
    me: Weak<Hub>,
    sweeping: AtomicBool,
    /// [`begin_shutdown`](Hub::begin_shutdown) was called: no tenant opens.
    stopping: AtomicBool,
    /// This process's wake scanner candidate (stopped when the hub goes away).
    #[cfg(feature = "postgres")]
    wake: Mutex<Option<onto_core::store::WakeScanner>>,
}

/// `[A-Za-z0-9_.-]`, 1–128 characters.
pub fn valid_tenant(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

impl Hub {
    /// Backend from `engine.storage`: one PostgreSQL schema per tenant
    /// (`{schema_prefix}{tenant}`), or memory.
    pub async fn open(config: Config) -> Result<Arc<Hub>> {
        let llm = llm::build(&config.llm)?;
        let backend = match config.engine.storage.as_str() {
            "memory" => Backend::Memory,
            #[cfg(feature = "postgres")]
            "postgres" | "postgresql" => Backend::Postgres(onto_core::store::PgShared::connect(
                &config.engine.resolve_database_url()?,
                &pg_options(&config),
            )?),
            other => return Err(Error::Config(format!("unknown engine.storage '{other}' (postgres, memory)"))),
        };
        Ok(Self::with_backend(config, llm, backend))
    }

    /// Tenants on the application's PostgreSQL pool (e.g. its `sqlx::PgPool`
    /// wrapped with `PgShared::from_pool`).
    #[cfg(feature = "postgres")]
    pub fn with_postgres(config: Config, shared: onto_core::store::PgShared) -> Result<Arc<Hub>> {
        let llm = llm::build(&config.llm)?;
        Ok(Self::with_backend(config, llm, Backend::Postgres(shared)))
    }

    /// One already-open ontology as the hub's only tenant.
    pub fn single(intelli: Arc<Intelli>) -> Arc<Hub> {
        let config = intelli.config().clone();
        let tenant = default_tenant(&config);
        let slot =
            Arc::new(Slot { cell: OnceCell::new_with(Some(intelli.clone())), last_used: AtomicI64::new(now_ms()) });
        Arc::new_cyclic(|me| Hub {
            llm: intelli.llm(),
            backend: Backend::Fixed,
            default_tenant: tenant.clone(),
            tenants: Mutex::new(HashMap::from([(tenant, slot)])),
            hooks: RwLock::new(None),
            config,
            me: me.clone(),
            sweeping: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            #[cfg(feature = "postgres")]
            wake: Mutex::new(None),
        })
    }

    fn with_backend(config: Config, llm: Arc<dyn LlmProvider>, backend: Backend) -> Arc<Hub> {
        Arc::new_cyclic(|me| Hub {
            default_tenant: default_tenant(&config),
            tenants: Mutex::new(HashMap::new()),
            hooks: RwLock::new(None),
            config,
            llm,
            backend,
            me: me.clone(),
            sweeping: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            #[cfg(feature = "postgres")]
            wake: Mutex::new(None),
        })
    }

    /// Settings for every tenant opened from now on (and on
    /// [`refresh_tenant`](Self::refresh_tenant)). Tenants already open keep
    /// theirs until refreshed.
    pub fn set_hooks(&self, hooks: Arc<dyn TenantHooks>) {
        *self.hooks.write() = Some(hooks);
    }

    /// The server's `[llm]` provider (tenants without their own use it).
    pub fn default_llm(&self) -> Arc<dyn LlmProvider> {
        self.llm.clone()
    }

    /// Read an open tenant's settings again and apply them (provider,
    /// limits, meter). Returns whether the tenant was open; a tenant that is
    /// not open gets its settings when it opens.
    pub async fn refresh_tenant(&self, tenant: &str) -> Result<bool> {
        let open = self.tenants.lock().get(tenant).and_then(|c| c.cell.get().cloned());
        let Some(intelli) = open else { return Ok(false) };
        let settings = self.settings_of(tenant).await?;
        self.apply(&intelli, settings);
        Ok(true)
    }

    async fn settings_of(&self, tenant: &str) -> Result<TenantSettings> {
        let hooks = self.hooks.read().clone();
        match hooks {
            Some(h) => h.settings(tenant).await,
            None => Ok(TenantSettings::default()),
        }
    }

    fn apply(&self, intelli: &Intelli, s: TenantSettings) {
        let llm = s.llm.unwrap_or_else(|| self.llm.clone());
        if !Arc::ptr_eq(&llm, &intelli.llm()) {
            intelli.set_llm(llm);
        }
        intelli.set_limits(s.limits);
        intelli.set_meter(s.meter);
        intelli.set_trace_settings(s.traces);
        intelli.set_secrets(s.secrets);
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn default_tenant(&self) -> &str {
        &self.default_tenant
    }

    /// Requests may name tenants other than the default one.
    pub fn multi_tenant(&self) -> bool {
        self.config.engine.tenancy == "multi"
    }

    /// The tenant's ontology, opened on first use.
    pub async fn tenant(&self, id: &str) -> Result<Arc<Intelli>> {
        if !valid_tenant(id) {
            return Err(Error::invalid(format!("invalid tenant id '{id}' (letters, digits, '_', '-', '.')")));
        }
        self.check_reserved(id)?;
        let slot = {
            let mut tenants = self.tenants.lock();
            if self.stopping.load(Ordering::SeqCst) {
                // Draining: tenants already open keep serving the requests
                // still in flight (until the process exits); none opens.
                match tenants.get(id) {
                    Some(s) if s.cell.initialized() => s.clone(),
                    _ => {
                        return Err(Error::Unavailable("the server is shutting down; retry on another replica".into()));
                    }
                }
            } else {
                tenants.entry(id.to_string()).or_insert_with(Slot::new).clone()
            }
        };
        slot.last_used.store(now_ms(), Ordering::Relaxed);
        let opened = !slot.cell.initialized();
        let intelli = slot.cell.get_or_try_init(|| self.open_tenant(id)).await.cloned();
        if opened && intelli.is_ok() {
            // Opening (migrations included) is not idle time: the idle clock
            // starts again when the tenant is ready.
            slot.last_used.store(now_ms(), Ordering::Relaxed);
        }
        if intelli.is_err() {
            // A failed open is not remembered: the next request tries again.
            let mut tenants = self.tenants.lock();
            if tenants.get(id).is_some_and(|s| Arc::ptr_eq(s, &slot)) {
                tenants.remove(id);
            }
        } else if opened && self.evicts() {
            self.enforce_limits(Some(id), now_ms());
            self.start_sweeper();
        }
        if let Ok(i) = &intelli
            && self.stopping.load(Ordering::SeqCst)
        {
            // Opened while the shutdown began: it stops with the others.
            i.begin_shutdown();
        }
        intelli
    }

    /// Begin a graceful shutdown: no tenant opens from now on, the wake
    /// scanner stops standing (an elected one gives the lock back), and
    /// every open tenant closes: its change streams end with their final
    /// frame and its workers start no new round. Returns at once;
    /// [`finish_shutdown`](Self::finish_shutdown) waits and releases.
    pub fn begin_shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        #[cfg(feature = "postgres")]
        if let Some(w) = self.wake.lock().as_ref() {
            w.stop();
        }
        for i in self.open_intellis() {
            i.begin_shutdown();
        }
    }

    /// Wait until `deadline` for the wake scanner to end its term and every
    /// open tenant's workers to end their round, then release the leases,
    /// claimed outbox rows and crawl leases this process holds, so a
    /// replacement process takes over at once instead of waiting for them
    /// to expire (see [`crate::lifecycle`]). Tenants stay open for requests
    /// still in flight.
    pub async fn finish_shutdown(&self, deadline: tokio::time::Instant) -> crate::Released {
        if !self.stopping.load(Ordering::SeqCst) {
            self.begin_shutdown();
        }
        #[cfg(feature = "postgres")]
        {
            let wake = self.wake.lock().take();
            if let Some(w) = wake
                && tokio::time::timeout_at(deadline, w.stopped()).await.is_err()
            {
                tracing::warn!("the wake scanner did not stop in time; its lock goes with the connection");
            }
        }
        let tenants = self.open_intellis();
        let done = futures::future::join_all(tenants.iter().map(|i| i.finish_shutdown(deadline))).await;
        let mut out = crate::Released::default();
        for r in &done {
            out.add(r);
        }
        out
    }

    /// [`begin_shutdown`](Self::begin_shutdown), then
    /// [`finish_shutdown`](Self::finish_shutdown) within `grace`.
    pub async fn shutdown(&self, grace: Duration) -> crate::Released {
        self.begin_shutdown();
        self.finish_shutdown(tokio::time::Instant::now() + grace).await
    }

    /// The hub is shutting down.
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    fn open_intellis(&self) -> Vec<Arc<Intelli>> {
        self.tenants.lock().values().filter_map(|s| s.cell.get().cloned()).collect()
    }

    /// Create a tenant's storage (its PostgreSQL schema and migrations)
    /// without opening it: nothing stays in memory, and nothing listens. New
    /// sign-ups get their first project this way; it opens on first use.
    pub async fn provision(&self, id: &str) -> Result<()> {
        if !valid_tenant(id) {
            return Err(Error::invalid(format!("invalid tenant id '{id}' (letters, digits, '_', '-', '.')")));
        }
        self.check_reserved(id)?;
        match &self.backend {
            #[cfg(feature = "postgres")]
            Backend::Postgres(shared) => {
                let (shared, id) = (shared.clone(), id.to_string());
                let opts = pg_options(&self.config);
                tokio::task::spawn_blocking(move || shared.tenant(&id, opts).map(drop))
                    .await
                    .map_err(|e| Error::Other(format!("provisioning a tenant: {e}")))?
            }
            Backend::Memory => Ok(()),
            Backend::Fixed => Err(Error::not_found(format!("tenant {id} (this hub serves one ontology)"))),
        }
    }

    /// Only PostgreSQL tenants close when idle or over the limits: an
    /// in-memory tenant's data would go with it.
    fn evicts(&self) -> bool {
        match &self.backend {
            #[cfg(feature = "postgres")]
            Backend::Postgres(_) => true,
            _ => false,
        }
    }

    /// Close idle tenants, then the least recently used ones while more than
    /// `max_open_tenants` are open or their estimated memory is over the
    /// budget. `keep` (the tenant just opened) and the most recently used
    /// tenant stay, whatever the budget. Returns those closed.
    ///
    /// An open change stream keeps its tenant from going idle (its
    /// heartbeats count as use), but not past `max_open_tenants` or the
    /// memory budget: those judge the last request only, and the stream
    /// ends `tenant_closed`.
    pub fn enforce_limits(&self, keep: Option<&str>, now: i64) -> Vec<String> {
        if !self.evicts() {
            return Vec::new();
        }
        let e = &self.config.engine;
        let idle_ms = (e.tenant_idle_secs as i64).saturating_mul(1000);
        let budget = e.tenant_memory_budget_mb.saturating_mul(1 << 20);
        let mut open: Vec<(String, i64, Arc<Intelli>)> = self
            .tenants
            .lock()
            .iter()
            .filter_map(|(k, s)| s.cell.get().map(|i| (k.clone(), s.last_used.load(Ordering::Relaxed), i.clone())))
            .collect();
        // Oldest first.
        open.sort_by_key(|(k, used, _)| (*used, k.clone()));
        let mut bytes: u64 = if budget > 0 { open.iter().map(|(_, _, i)| i.estimated_bytes()).sum() } else { 0 };
        let mut count = open.len();
        let newest = open.last().map(|(k, _, _)| k.clone());
        let mut closed = Vec::new();
        for (id, used, intelli) in &open {
            if Some(id.as_str()) == keep {
                continue;
            }
            let idle = idle_ms > 0 && now - (*used).max(intelli.stream_touched()) >= idle_ms;
            if !idle && Some(id) == newest.as_ref() {
                continue;
            }
            let over_count = e.max_open_tenants > 0 && count > e.max_open_tenants;
            let over_budget = budget > 0 && bytes > budget;
            if !(idle || over_count || over_budget) {
                continue;
            }
            if self.close_if_unused_since(id, *used) {
                count -= 1;
                if budget > 0 {
                    bytes = bytes.saturating_sub(intelli.estimated_bytes());
                }
                closed.push(id.clone());
            }
        }
        if !closed.is_empty() {
            tracing::info!(closed = closed.len(), open = count, "closed tenants (idle or over engine limits)");
        }
        closed
    }

    /// Forget a tenant unless a request used it after `used`, and close it
    /// (its streams end `tenant_closed`).
    fn close_if_unused_since(&self, id: &str, used: i64) -> bool {
        let removed = {
            let mut tenants = self.tenants.lock();
            match tenants.get(id) {
                Some(s) if s.last_used.load(Ordering::Relaxed) <= used && s.cell.initialized() => tenants.remove(id),
                _ => None,
            }
        };
        match removed {
            Some(slot) => {
                close_slot(&slot);
                true
            }
            None => false,
        }
    }

    /// Close idle tenants every minute (or a quarter of the idle time).
    fn start_sweeper(&self) {
        if self.config.engine.tenant_idle_secs == 0 || self.sweeping.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.sweeping.store(false, Ordering::Release);
            return;
        };
        let every = Duration::from_secs((self.config.engine.tenant_idle_secs / 4).clamp(1, 60));
        let me = self.me.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(hub) = me.upgrade() else { break };
                hub.enforce_limits(None, now_ms());
            }
        });
    }

    /// Stand for election as the deployment's wake scanner (docs/design/EVENTS.md
    /// section 10): PostgreSQL only, and not with `engine.wake_poll_secs = 0`.
    /// Every process may stand; one at a time is elected (an advisory lock on a
    /// connection of its own). The elected one opens, in this process, the
    /// tenants whose background work is due by the database clock and that are
    /// not open here (open tenants' own workers see their work). Another
    /// candidate takes over within `wake_poll_secs` when the elected process
    /// goes away. Returns whether a candidate runs in this process. `onto
    /// serve` calls it; one-shot CLI commands do not.
    pub fn start_wake_scanner(&self) -> bool {
        #[cfg(feature = "postgres")]
        if let Backend::Postgres(shared) = &self.backend {
            let poll = self.config.engine.wake_poll_secs;
            let Ok(rt) = tokio::runtime::Handle::try_current() else { return false };
            if poll == 0 || self.stopping.load(Ordering::SeqCst) {
                return false;
            }
            let mut slot = self.wake.lock();
            if slot.is_some() {
                return true;
            }
            let me = self.me.clone();
            let on_due: onto_core::store::WakeFn = Arc::new(move |tenants: Vec<String>| {
                let me = me.clone();
                rt.spawn(async move {
                    for t in tenants {
                        let Some(hub) = me.upgrade() else { return };
                        hub.wake_tenant(&t).await;
                    }
                });
            });
            *slot = Some(shared.spawn_wake_scanner(
                &self.config.engine.postgres.schema_prefix,
                Duration::from_secs(poll),
                on_due,
            ));
            return true;
        }
        false
    }

    /// This process is the deployment's elected wake scanner right now.
    pub fn wake_scanner_elected(&self) -> bool {
        #[cfg(feature = "postgres")]
        {
            self.wake.lock().as_ref().is_some_and(|w| w.elected())
        }
        #[cfg(not(feature = "postgres"))]
        false
    }

    /// Open a tenant the wake scanner found due, unless it is open here
    /// already or this hub does not serve it.
    #[cfg(feature = "postgres")]
    async fn wake_tenant(&self, id: &str) {
        if !valid_tenant(id) || (!self.multi_tenant() && id != self.default_tenant) {
            return;
        }
        if self.tenants.lock().get(id).is_some_and(|s| s.cell.initialized()) {
            return;
        }
        match self.tenant(id).await {
            Ok(_) => tracing::debug!(tenant = id, "opened a tenant with background work due"),
            Err(e) => tracing::warn!(tenant = id, error = %e, "could not open a tenant with background work due"),
        }
    }

    /// The backend answers: one query on PostgreSQL. For readiness probes.
    pub async fn ready(&self) -> Result<()> {
        match &self.backend {
            #[cfg(feature = "postgres")]
            Backend::Postgres(shared) => {
                let shared = shared.clone();
                tokio::task::spawn_blocking(move || shared.ping())
                    .await
                    .map_err(|e| Error::Other(format!("readiness: {e}")))?
            }
            _ => Ok(()),
        }
    }

    /// Stores listening for other processes' commits on the pool's one
    /// LISTEN connection (`None` without PostgreSQL).
    pub fn listening_stores(&self) -> Option<usize> {
        match &self.backend {
            #[cfg(feature = "postgres")]
            Backend::Postgres(shared) => Some(shared.listening_stores()),
            _ => None,
        }
    }

    /// The open tenants with their estimated memory, largest first.
    pub fn tenant_stats(&self) -> Vec<TenantStat> {
        self.open_stats().into_iter().map(|(s, _)| s).collect()
    }

    /// [`tenant_stats`](Self::tenant_stats) with each tenant's outbound
    /// counters (docs/design/EVENTS.md section 11), for `GET
    /// /v1/admin/tenants`. Each tenant's counters are read on a blocking
    /// thread, [`REPORT_CONCURRENCY`] tenants at a time; a tenant whose
    /// counters take longer than [`REPORT_TENANT_TIMEOUT`] reports
    /// `{"timed_out": true}`, so the report never waits on one slow store.
    pub async fn tenant_report(&self) -> Vec<TenantStat> {
        let open = self.open_stats();
        let intellis: Vec<Arc<Intelli>> = open.iter().map(|(_, i)| i.clone()).collect();
        let counters =
            each_bounded(intellis, REPORT_CONCURRENCY, REPORT_TENANT_TIMEOUT, |i: Arc<Intelli>| i.outbound_counters())
                .await;
        open.into_iter()
            .zip(counters)
            .map(|((mut stat, _), c)| {
                stat.outbound = Some(c.unwrap_or_else(|| serde_json::json!({ "timed_out": true })));
                stat
            })
            .collect()
    }

    /// The open tenants' in-memory stats (no store reads), largest first.
    fn open_stats(&self) -> Vec<(TenantStat, Arc<Intelli>)> {
        let now = now_ms();
        let open: Vec<(String, i64, Arc<Intelli>)> = self
            .tenants
            .lock()
            .iter()
            .filter_map(|(k, s)| s.cell.get().map(|i| (k.clone(), s.last_used.load(Ordering::Relaxed), i.clone())))
            .collect();
        let mut out: Vec<(TenantStat, Arc<Intelli>)> = open
            .into_iter()
            .map(|(tenant, used, i)| {
                let (entities, links) = i.ontology().counts();
                let stat = TenantStat {
                    tenant,
                    estimated_bytes: i.estimated_bytes(),
                    entities,
                    links,
                    idle_ms: now - used,
                    streams: i.changes().subscribers(),
                    ring_bytes: i.changes().bytes() as u64,
                    outbound: None,
                };
                (stat, i)
            })
            .collect();
        out.sort_by(|(a, _), (b, _)| b.estimated_bytes.cmp(&a.estimated_bytes).then_with(|| a.tenant.cmp(&b.tenant)));
        out
    }

    /// The default tenant, or the requested one when the hub is multi-tenant.
    pub async fn resolve(&self, requested: Option<&str>) -> Result<Arc<Intelli>> {
        match requested.filter(|t| !t.is_empty()) {
            Some(t) if t != self.default_tenant && !self.multi_tenant() => Err(Error::not_found(format!(
                "tenant {t} (this server serves only '{}'; set engine.tenancy = \"multi\")",
                self.default_tenant
            ))),
            Some(t) => self.tenant(t).await,
            None => self.tenant(&self.default_tenant.clone()).await,
        }
    }

    /// A tenant whose PostgreSQL schema would be the control store's
    /// (`console.schema`) is refused: with the default names, tenant `control`.
    fn check_reserved(&self, id: &str) -> Result<()> {
        #[cfg(feature = "postgres")]
        {
            let schema = onto_core::store::postgres::tenant_schema(&self.config.engine.postgres.schema_prefix, id)?;
            if schema == self.config.console.schema {
                return Err(Error::invalid(format!(
                    "tenant id '{id}' is reserved: its schema {schema} is the control store's (console.schema)"
                )));
            }
        }
        #[cfg(not(feature = "postgres"))]
        let _ = id;
        Ok(())
    }

    /// Forget an open tenant: the next request opens it again. Requests in
    /// flight keep the ontology they hold; its change streams end
    /// `tenant_closed`. Returns whether it was known.
    pub fn evict(&self, tenant: &str) -> bool {
        let removed = self.tenants.lock().remove(tenant);
        removed.inspect(|slot| close_slot(slot)).is_some()
    }

    /// Evict a tenant and delete its data: `DROP SCHEMA {prefix}{tenant}
    /// CASCADE` on PostgreSQL; in memory, the store goes with the last reference.
    pub fn drop_tenant(&self, tenant: &str) -> Result<()> {
        if !valid_tenant(tenant) {
            return Err(Error::invalid(format!("invalid tenant id '{tenant}'")));
        }
        self.evict(tenant);
        match &self.backend {
            #[cfg(feature = "postgres")]
            Backend::Postgres(shared) => shared.drop_tenant(&self.config.engine.postgres.schema_prefix, tenant),
            Backend::Memory => Ok(()),
            Backend::Fixed => Err(Error::invalid("this hub serves one ontology; its tenant cannot be dropped")),
        }
    }

    /// Tenants opened in this process.
    pub fn open_tenants(&self) -> Vec<String> {
        let mut v: Vec<String> =
            self.tenants.lock().iter().filter(|(_, c)| c.cell.initialized()).map(|(k, _)| k.clone()).collect();
        v.sort();
        v
    }

    /// Tenants the backend holds (opened or not).
    pub fn known_tenants(&self) -> Result<Vec<String>> {
        match &self.backend {
            #[cfg(feature = "postgres")]
            Backend::Postgres(shared) => shared.list_tenants(&self.config.engine.postgres.schema_prefix),
            _ => Ok(self.open_tenants()),
        }
    }

    async fn open_tenant(&self, id: &str) -> Result<Arc<Intelli>> {
        let mut cfg = self.config.clone();
        cfg.engine.postgres.tenant = id.to_string();
        let store: Arc<dyn Store> = match &self.backend {
            Backend::Memory => Arc::new(MemoryStore::default()),
            #[cfg(feature = "postgres")]
            Backend::Postgres(shared) => Arc::new(shared.tenant(id, pg_options(&cfg))?),
            Backend::Fixed => {
                return Err(Error::not_found(format!("tenant {id} (this hub serves one ontology)")));
            }
        };
        let settings = self.settings_of(id).await?;
        let llm = settings.llm.clone().unwrap_or_else(|| self.llm.clone());
        let intelli = Intelli::open_with_store(cfg, store, llm).await?;
        self.apply(&intelli, settings);
        Ok(intelli)
    }
}

/// Tell a forgotten tenant's streams and workers to end.
fn close_slot(slot: &Slot) {
    if let Some(i) = slot.cell.get() {
        i.close();
    }
}

fn default_tenant(cfg: &Config) -> String {
    let t = cfg.engine.postgres.tenant.trim();
    if t.is_empty() { "default".into() } else { t.to_string() }
}

#[cfg(feature = "postgres")]
fn pg_options(cfg: &Config) -> onto_core::store::PgOptions {
    let pg = &cfg.engine.postgres;
    onto_core::store::PgOptions {
        schema_prefix: pg.schema_prefix.clone(),
        max_connections: pg.max_connections,
        connect_timeout: std::time::Duration::from_secs(pg.connect_timeout_secs),
        listen: true,
    }
}

#[cfg(test)]
mod report_tests {
    use std::time::{Duration, Instant};

    /// The tenant report reads tenants concurrently (bounded) and gives up
    /// on a slow one instead of waiting for it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tenants_are_read_concurrently_and_a_slow_one_times_out() {
        let t0 = Instant::now();
        let out = super::each_bounded((0..16u64).collect(), 8, Duration::from_secs(5), |n| {
            std::thread::sleep(Duration::from_millis(100));
            n * 2
        })
        .await;
        assert_eq!(out, (0..16u64).map(|n| Some(n * 2)).collect::<Vec<_>>(), "in order, all read");
        // One after another would take 1.6 s; eight at a time take ~0.2 s.
        assert!(t0.elapsed() < Duration::from_millis(1000), "{:?}", t0.elapsed());

        let t0 = Instant::now();
        let out = super::each_bounded(vec![0u64, 1], 2, Duration::from_millis(200), |n| {
            std::thread::sleep(Duration::from_millis(if n == 1 { 2_000 } else { 1 }));
            n
        })
        .await;
        assert_eq!(out, vec![Some(0), None]);
        assert!(t0.elapsed() < Duration::from_millis(1500), "{:?}", t0.elapsed());
    }
}
