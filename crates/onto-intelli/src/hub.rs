//! Many tenants in one process. Each tenant is its own ontology (its own
//! PostgreSQL schema, or an in-memory store) with its own schema
//! versions, proposals, memory and caches. The LLM provider is the server's
//! unless [`TenantHooks`] (the control plane) give a tenant its own, with
//! its limits and usage meter. Tenants open on first use and stay open.
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
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use tokio::sync::OnceCell;

use onto_core::store::{MemoryStore, Store};
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

pub struct Hub {
    config: Config,
    llm: Arc<dyn LlmProvider>,
    backend: Backend,
    default_tenant: String,
    tenants: Mutex<HashMap<String, Arc<OnceCell<Arc<Intelli>>>>>,
    hooks: RwLock<Option<Arc<dyn TenantHooks>>>,
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
        let cell = Arc::new(OnceCell::new_with(Some(intelli.clone())));
        Arc::new(Hub {
            llm: intelli.llm(),
            backend: Backend::Fixed,
            default_tenant: tenant.clone(),
            tenants: Mutex::new(HashMap::from([(tenant, cell)])),
            hooks: RwLock::new(None),
            config,
        })
    }

    fn with_backend(config: Config, llm: Arc<dyn LlmProvider>, backend: Backend) -> Arc<Hub> {
        Arc::new(Hub {
            default_tenant: default_tenant(&config),
            tenants: Mutex::new(HashMap::new()),
            hooks: RwLock::new(None),
            config,
            llm,
            backend,
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
        let open = self.tenants.lock().get(tenant).and_then(|c| c.get().cloned());
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
        let cell = self.tenants.lock().entry(id.to_string()).or_insert_with(|| Arc::new(OnceCell::new())).clone();
        cell.get_or_try_init(|| self.open_tenant(id)).await.cloned()
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
    /// flight keep the ontology they hold. Returns whether it was known.
    pub fn evict(&self, tenant: &str) -> bool {
        self.tenants.lock().remove(tenant).is_some()
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
            self.tenants.lock().iter().filter(|(_, c)| c.initialized()).map(|(k, _)| k.clone()).collect();
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
