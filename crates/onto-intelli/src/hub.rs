//! Many tenants in one process. Each tenant is its own ontology (its own
//! PostgreSQL schema, SQLite file or in-memory store) with its own schema
//! versions, proposals, memory and caches; the LLM provider is shared.
//! Tenants open on first use and stay open.
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
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::OnceCell;

use onto_core::store::{MemoryStore, Store};
use onto_core::{Error, Result};

use crate::config::Config;
use crate::intelli::Intelli;
use crate::llm::{self, LlmProvider};

enum Backend {
    Memory,
    #[cfg(feature = "sqlite")]
    Sqlite(PathBuf),
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
}

/// `[A-Za-z0-9_.-]`, 1–128 characters.
pub fn valid_tenant(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

impl Hub {
    /// Backend from `engine.storage`: one PostgreSQL schema per tenant,
    /// one SQLite file per tenant (`{data_dir}/tenants/{id}/onto.db`, the
    /// default tenant keeps `{data_dir}/onto.db`), or memory.
    pub async fn open(config: Config) -> Result<Arc<Hub>> {
        let llm = llm::build(&config.llm)?;
        let backend = match config.engine.storage.as_str() {
            "memory" => Backend::Memory,
            #[cfg(feature = "sqlite")]
            "sqlite" => Backend::Sqlite(config.resolve_path(&config.engine.data_dir)),
            #[cfg(feature = "postgres")]
            "postgres" | "postgresql" => {
                let pg = &config.engine.postgres;
                Backend::Postgres(onto_core::store::PgShared::connect(&pg.resolve_url()?, &pg_options(&config))?)
            }
            other => return Err(Error::Config(format!("unknown engine.storage '{other}' (sqlite, postgres, memory)"))),
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
            llm: intelli.llm().clone(),
            backend: Backend::Fixed,
            default_tenant: tenant.clone(),
            tenants: Mutex::new(HashMap::from([(tenant, cell)])),
            config,
        })
    }

    fn with_backend(config: Config, llm: Arc<dyn LlmProvider>, backend: Backend) -> Arc<Hub> {
        Arc::new(Hub {
            default_tenant: default_tenant(&config),
            tenants: Mutex::new(HashMap::new()),
            config,
            llm,
            backend,
        })
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
            #[cfg(feature = "sqlite")]
            Backend::Sqlite(dir) => {
                let mut out = vec![self.default_tenant.clone()];
                if let Ok(rd) = std::fs::read_dir(dir.join("tenants")) {
                    for e in rd.flatten() {
                        if e.path().join("onto.db").is_file()
                            && let Some(n) = e.file_name().to_str()
                        {
                            out.push(n.to_string());
                        }
                    }
                }
                out.sort();
                out.dedup();
                Ok(out)
            }
            _ => Ok(self.open_tenants()),
        }
    }

    async fn open_tenant(&self, id: &str) -> Result<Arc<Intelli>> {
        let mut cfg = self.config.clone();
        cfg.engine.postgres.tenant = id.to_string();
        let store: Arc<dyn Store> = match &self.backend {
            Backend::Memory => Arc::new(MemoryStore::default()),
            #[cfg(feature = "sqlite")]
            Backend::Sqlite(dir) => {
                let path = if id == self.default_tenant {
                    dir.join("onto.db")
                } else {
                    dir.join("tenants").join(id).join("onto.db")
                };
                Arc::new(onto_core::store::SqliteStore::open(path)?)
            }
            #[cfg(feature = "postgres")]
            Backend::Postgres(shared) => Arc::new(shared.tenant(id, pg_options(&cfg))?),
            Backend::Fixed => {
                return Err(Error::not_found(format!("tenant {id} (this hub serves one ontology)")));
            }
        };
        Intelli::open_with_store(cfg, store, self.llm.clone()).await
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
