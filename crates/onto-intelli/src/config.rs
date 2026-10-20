//! `onto-intelli.toml` — one file configures storage, ontology, LLM provider,
//! context budget, caches, memory, gRPC listener + proto routes, and auth.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use onto_core::{Error, Governance, Principal, Result, Role, SchemaMode};

/// Annotated starter config written by `onto init`.
pub const EXAMPLE_CONFIG: &str = include_str!("../templates/onto-intelli.toml");
/// Example user proto served through `[[grpc.routes]]`, written by `onto init`.
pub const EXAMPLE_PROTO_PATH: &str = "acme/tickets/v1/tickets.proto";
pub const EXAMPLE_PROTO: &str = include_str!("../templates/protos/acme/tickets/v1/tickets.proto");

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub engine: EngineConfig,
    pub ontology: OntologyConfig,
    pub llm: LlmConfig,
    pub context: ContextConfig,
    pub cache: CacheConfig,
    pub memory: MemoryConfig,
    pub reasoning: ReasoningConfig,
    pub grpc: GrpcConfig,
    pub auth: AuthConfig,
    pub mcp: McpConfig,
    /// The principal of callers that open the store directly (CLI, FFI).
    pub local: LocalConfig,
    /// The web UI served by `onto serve`.
    pub ui: UiConfig,
    /// Who may change, propose and approve schema changes.
    pub governance: Governance,
    /// Background crawls of registered connectors.
    pub connectors: ConnectorsConfig,
    /// Directory relative paths are resolved against (the config file's dir).
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// `postgres` (durable: PostgreSQL 16+ with pgvector, shared by any
    /// number of processes) or `memory` (volatile: tests and demos).
    pub storage: String,
    /// PostgreSQL URL. Resolution order: `onto --database-url`, this value,
    /// `ONTO_DATABASE_URL`, `DATABASE_URL`. The URL holds credentials, so
    /// prefer an environment variable to writing it here.
    pub database_url: Option<String>,
    pub default_workspace: String,
    /// Capacity of the async ingestion queue (backpressure beyond this).
    pub ingest_queue: usize,
    /// `eventual`: reads see other processes' writes once the follower applies
    /// them (milliseconds with LISTEN/NOTIFY). `strong`: every read first
    /// checks the store's commit head (one cheap query) and catches up.
    pub read_consistency: String,
    /// How often the follower of a shared store polls when no notice arrives.
    pub follow_interval_ms: u64,
    /// `single`: one tenant (`engine.postgres.tenant`).
    /// `multi`: requests choose a tenant (`x-onto-tenant`); each tenant is
    /// its own PostgreSQL schema, opened on first use.
    pub tenancy: String,
    pub postgres: PostgresConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            storage: "postgres".into(),
            database_url: None,
            default_workspace: "default".into(),
            ingest_queue: 1024,
            read_consistency: "eventual".into(),
            follow_interval_ms: 1_000,
            tenancy: "single".into(),
            postgres: PostgresConfig::default(),
        }
    }
}

/// `[engine.postgres]`: tenants and the connection pool.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PostgresConfig {
    /// Tenant: one PostgreSQL schema (`{schema_prefix}{tenant}`) per tenant.
    pub tenant: String,
    pub schema_prefix: String,
    pub max_connections: u32,
    pub connect_timeout_secs: u64,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self { tenant: "default".into(), schema_prefix: "onto_".into(), max_connections: 10, connect_timeout_secs: 10 }
    }
}

/// Local development database (`make db-up`).
pub const DEV_DATABASE_URL: &str = "postgres://onto:onto@127.0.0.1:5441/onto";

impl EngineConfig {
    /// The PostgreSQL URL: `database_url` (set by `--database-url` or the
    /// file), else `ONTO_DATABASE_URL`, else `DATABASE_URL`.
    pub fn resolve_database_url(&self) -> Result<String> {
        if let Some(url) = self.database_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            return Ok(url.to_string());
        }
        for var in ["ONTO_DATABASE_URL", "DATABASE_URL"] {
            if let Ok(v) = std::env::var(var)
                && !v.trim().is_empty()
            {
                return Ok(v.trim().to_string());
            }
        }
        Err(Error::Config(format!(
            "engine.storage = \"postgres\" needs a database: pass --database-url, set engine.database_url, or set \
             ONTO_DATABASE_URL (or DATABASE_URL). For a local one, run `make db-up` and use {DEV_DATABASE_URL}"
        )))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct OntologyConfig {
    /// `builtin:enterprise`, `builtin:empty`, or a TOML schema path.
    pub schema: String,
    /// Extra schema layers applied in order (`builtin:<package>` or a TOML path).
    pub extend: Vec<String>,
    pub strict: bool,
    /// `live`: the stored schema is authoritative and file edits are imported
    /// as new versions; `config`: the files are authoritative, API edits refused.
    pub schema_mode: SchemaMode,
}

impl Default for OntologyConfig {
    fn default() -> Self {
        Self {
            schema: "builtin:enterprise".into(),
            extend: vec!["builtin:data-catalog".into()],
            strict: false,
            schema_mode: SchemaMode::Live,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cached_input_per_mtok: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// `mock` (offline, deterministic), `none`, `openai`, `ollama`,
    /// `openai-compatible` (vLLM, LM Studio, OpenRouter, ...), `anthropic`.
    pub provider: String,
    /// Empty = provider default.
    pub model: String,
    pub base_url: Option<String>,
    /// Environment variable holding the API key (keys never live in the file).
    pub api_key_env: Option<String>,
    pub max_output_tokens: u32,
    /// Only sent when set; several current models reject sampling parameters.
    pub temperature: Option<f32>,
    /// Anthropic `output_config.effort`: low | medium | high | xhigh | max.
    pub effort: Option<String>,
    /// Anthropic server-side refusal fallback (`fallbacks: "default"`).
    pub fallbacks: bool,
    pub timeout_secs: u64,
    pub pricing: Pricing,
    pub headers: BTreeMap<String, String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "mock".into(),
            model: String::new(),
            base_url: None,
            api_key_env: None,
            max_output_tokens: 4096,
            temperature: None,
            effort: None,
            fallbacks: true,
            timeout_secs: 120,
            pricing: Pricing::default(),
            headers: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    /// Max tokens of facts sent to the LLM per question.
    pub token_budget: usize,
    pub max_depth: usize,
    pub max_nodes: usize,
    /// Conversation turns kept verbatim; older turns are folded into a summary.
    pub history_turns: usize,
    pub recent_events: usize,
    pub knowledge_hits: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self { token_budget: 1800, max_depth: 2, max_nodes: 80, history_turns: 6, recent_events: 5, knowledge_hits: 4 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    pub enabled: bool,
    pub max_entries: usize,
    /// Whole-answer cache; keys include the graph revision, so writes invalidate.
    pub answer_ttl_secs: u64,
    /// LLM completion cache keyed by the exact prompt.
    pub llm_ttl_secs: u64,
    /// Keep LLM completions in the store so they survive restarts.
    pub persist_llm: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { enabled: true, max_entries: 10_000, answer_ttl_secs: 300, llm_ttl_secs: 86_400, persist_llm: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub enabled: bool,
    pub recall_k: usize,
    pub episodic_ttl_days: u32,
    /// Episodes about the same entities before they are promoted to long-term memory.
    pub consolidate_after: usize,
    /// Run consolidation automatically every N questions (0 = never).
    pub consolidate_every: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { enabled: true, recall_k: 4, episodic_ttl_days: 30, consolidate_after: 3, consolidate_every: 25 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasoningConfig {
    /// Intents answered straight from the graph when entities resolve cleanly.
    pub deterministic_intents: Vec<String>,
    /// Minimum resolution confidence for a deterministic answer.
    pub min_confidence: f32,
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self {
            deterministic_intents: [
                "lookup",
                "owner",
                "assignee",
                "status",
                "members",
                "count",
                "list",
                "blockers",
                "dependencies",
                "impact",
                "relation",
                "workload",
                "history",
                "expert",
                "metric",
                "changes",
                "quality",
                "what_if",
                "data_location",
                "join_path",
                "trusted_metric",
            ]
            .map(String::from)
            .to_vec(),
            min_confidence: 0.8,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GrpcConfig {
    pub listen: String,
    /// Serve the built-in onto_intelli.v1 API.
    pub builtin_api: bool,
    pub reflection: bool,
    pub health: bool,
    /// JSON over HTTP/1.1 on the same port (`POST /v1/call/{action}`).
    pub http: bool,
    /// Include paths for your own .proto files.
    pub proto_dirs: Vec<String>,
    /// Proto files (relative to `proto_dirs`) whose services are mapped by `routes`.
    pub proto_files: Vec<String>,
    pub routes: Vec<RouteConfig>,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:50051".into(),
            builtin_api: true,
            reflection: true,
            health: true,
            http: true,
            proto_dirs: Vec::new(),
            proto_files: Vec::new(),
            routes: Vec::new(),
        }
    }
}

/// Maps one RPC of a user proto onto an engine action.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RouteConfig {
    /// `package.Service/Method`.
    pub method: String,
    /// Engine action, e.g. `entity.upsert`, `intelli.ask` (see `onto actions`).
    pub action: String,
    /// Template building the action input from the request (`{{field.path}}`).
    /// Omitted: the request message's JSON is passed through unchanged.
    #[serde(default)]
    pub request: Option<Value>,
    /// Template building the response message from the action result.
    #[serde(default)]
    pub response: Option<Value>,
    /// Workspace override for this route.
    #[serde(default)]
    pub workspace: Option<String>,
}

/// `[connectors]`: connectors themselves are registered at runtime
/// (`onto connect`, `connector.add`) and stored in the database.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectorsConfig {
    /// Crawl due connectors in the background while the ontology is open.
    pub background: bool,
    /// How often to look for due connectors.
    pub tick_secs: u64,
}

impl Default for ConnectorsConfig {
    fn default() -> Self {
        Self { background: true, tick_secs: 30 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Expose apply_action / undo_action to MCP clients.
    pub allow_writes: bool,
    /// Who MCP clients act as. An agent by default: it reads, proposes
    /// schema changes and (with `allow_writes`) runs action types, but never
    /// applies or approves schema changes.
    pub principal: Principal,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self { allow_writes: false, principal: Principal::new("mcp-agent", vec![Role::Agent], vec!["*".into()]) }
    }
}

/// `[local]`: the CLI and in-process callers (FFI) that open the store
/// themselves. They hold the store's credentials anyway, so this is an
/// operator principal, not something a network caller can reach.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalConfig {
    pub principal: Principal,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self { principal: Principal::new("local", vec![Role::Admin], vec!["*".into()]) }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// `token` (default): every gRPC/HTTP caller presents a bearer token.
    /// `dev`: no authentication, every caller is `dev_principal`; for local
    /// development only (refused on non-loopback addresses, or use
    /// `onto serve --dev`).
    pub mode: String,
    #[serde(alias = "default_principal")]
    pub dev_principal: Principal,
    pub tokens: Vec<TokenConfig>,
    /// Environment variable holding the secret that signs browser session
    /// tokens (`POST /v1/auth/session`). Set the same value on every
    /// replica; unset, each process signs with a random secret of its own.
    pub session_secret_env: Option<String>,
    /// Longest session a token may mint.
    pub session_max_ttl_secs: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            mode: "token".into(),
            dev_principal: Principal::new("dev", vec![Role::Admin], vec!["*".into()]),
            tokens: Vec::new(),
            session_secret_env: Some("ONTO_SESSION_SECRET".into()),
            session_max_ttl_secs: 12 * 3600,
        }
    }
}

/// `[ui]`: the operator web UI (`/ui`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub enabled: bool,
    /// Origins allowed to embed the UI in an iframe (CSP `frame-ancestors`)
    /// and to hand it a session with `postMessage`, e.g. `https://nexc.example.com`.
    pub embed_origins: Vec<String>,
    /// Shown in the header and the browser tab.
    pub title: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self { enabled: true, embed_origins: Vec::new(), title: "onto-intelli".into() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenConfig {
    /// Prefer `token_env`; a literal `token` is accepted for local testing.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    pub principal: Principal,
    /// A trusted service (e.g. an application backend) may act for its own
    /// users: `x-onto-act-as`, `x-onto-act-roles`, `x-onto-act-workspaces`,
    /// `x-onto-act-entity` then name the principal of each call, so per-user
    /// ACLs and roles apply.
    #[serde(default)]
    pub impersonate: bool,
    /// Roles an impersonating service may grant its users (never more).
    #[serde(default)]
    pub impersonate_roles: Vec<Role>,
    /// Tenants this token may address (`*`: all).
    #[serde(default = "all_tenants")]
    pub tenants: Vec<String>,
}

fn all_tenants() -> Vec<String> {
    vec!["*".into()]
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text =
            std::fs::read_to_string(path).map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
        let mut cfg = Self::from_toml(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        cfg.base_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Ok(cfg)
    }

    /// Explicit path, else `$ONTO_CONFIG`, `./onto-intelli.toml`,
    /// `~/.config/onto-intelli/config.toml`; defaults when none exist.
    pub fn discover(explicit: Option<&Path>) -> Result<Self> {
        if let Some(p) = explicit {
            return Self::load(p);
        }
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("ONTO_CONFIG") {
            candidates.push(p.into());
        }
        candidates.push("onto-intelli.toml".into());
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            candidates.push(PathBuf::from(home).join(".config/onto-intelli/config.toml"));
        }
        for c in candidates {
            if c.is_file() {
                return Self::load(&c);
            }
        }
        Ok(Self::default())
    }

    /// Volatile config with the offline mock provider: tests and embedding.
    pub fn in_memory() -> Self {
        let mut cfg = Self::default();
        cfg.engine.storage = "memory".into();
        cfg
    }

    pub fn resolve_path(&self, p: &str) -> PathBuf {
        let path = PathBuf::from(p);
        if path.is_absolute() || self.base_dir.as_os_str().is_empty() { path } else { self.base_dir.join(path) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let cfg = Config::from_toml(EXAMPLE_CONFIG).unwrap();
        assert_eq!(cfg.ontology.schema, "builtin:enterprise");
        assert!(!cfg.grpc.routes.is_empty());
        assert!(cfg.grpc.routes[0].request.is_some());
    }

    #[test]
    fn empty_config_uses_defaults() {
        let cfg = Config::from_toml("").unwrap();
        assert_eq!(cfg.llm.provider, "mock");
        assert_eq!(cfg.auth.mode, "token", "authentication is on unless asked otherwise");
        assert_eq!(cfg.local.principal.id, "local");
        assert!(cfg.mcp.principal.is_agent());
        let legacy = Config::from_toml("[auth]\ndefault_principal = { id = 'old', roles = ['viewer'] }\n").unwrap();
        assert_eq!(legacy.auth.dev_principal.id, "old");
    }
}
