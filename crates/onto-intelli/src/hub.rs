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
}

impl std::fmt::Debug for TenantSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantSettings")
            .field("llm", &self.llm.as_ref().map(|l| format!("{}/{}", l.name(), l.model())))
            .field("limits", &self.limits)
            .field("meter", &self.meter.is_some())
            .field("version", &self.version)
            .field("traces", &self.traces)
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
        let slot = self.tenants.lock().entry(id.to_string()).or_insert_with(Slot::new).clone();
        slot.last_used.store(now_ms(), Ordering::Relaxed);
        let opened = !slot.cell.initialized();
        let intelli = slot.cell.get_or_try_init(|| self.open_tenant(id)).await.cloned();
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
        intelli
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
            if poll == 0 {
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
        let now = now_ms();
        let open: Vec<(String, i64, Arc<Intelli>)> = self
            .tenants
            .lock()
            .iter()
            .filter_map(|(k, s)| s.cell.get().map(|i| (k.clone(), s.last_used.load(Ordering::Relaxed), i.clone())))
            .collect();
        let mut out: Vec<TenantStat> = open
            .into_iter()
            .map(|(tenant, used, i)| {
                let (entities, links) = i.ontology().counts();
                TenantStat {
                    tenant,
                    estimated_bytes: i.estimated_bytes(),
                    entities,
                    links,
                    idle_ms: now - used,
                    streams: i.changes().subscribers(),
                    ring_bytes: i.changes().bytes() as u64,
                }
            })
            .collect();
        out.sort_by(|a, b| b.estimated_bytes.cmp(&a.estimated_bytes).then_with(|| a.tenant.cmp(&b.tenant)));
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
