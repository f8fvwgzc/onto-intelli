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
    /// Who may change, propose and approve schema changes.
    pub governance: Governance,
    /// Directory relative paths are resolved against (the config file's dir).
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// `sqlite` (durable, single file, one node), `postgres` (shared by
    /// several processes) or `memory` (volatile).
    pub storage: String,
    pub data_dir: String,
    pub default_workspace: String,
    /// Capacity of the async ingestion queue (backpressure beyond this).
    pub ingest_queue: usize,
    /// `eventual`: reads see other processes' writes once the follower applies
    /// them (milliseconds with LISTEN/NOTIFY). `strong`: every read first
    /// checks the store's commit head (one cheap query) and catches up.
    pub read_consistency: String,
    /// How often the follower of a shared store polls when no notice arrives.
    pub follow_interval_ms: u64,
    pub postgres: PostgresConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            storage: "sqlite".into(),
            data_dir: ".onto".into(),
            default_workspace: "default".into(),
            ingest_queue: 1024,
            read_consistency: "eventual".into(),
            follow_interval_ms: 1_000,
            postgres: PostgresConfig::default(),
        }
    }
}

/// `[engine.postgres]`: the shared store for several processes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PostgresConfig {
    /// Connection URL (prefer `url_env`; the URL holds credentials).
    pub url: Option<String>,
    /// Environment variable holding the URL.
    pub url_env: Option<String>,
    /// Tenant: one PostgreSQL schema (`{schema_prefix}{tenant}`) per tenant.
    pub tenant: String,
    pub schema_prefix: String,
    pub max_connections: u32,
    pub connect_timeout_secs: u64,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            url: None,
            url_env: Some("ONTO_DATABASE_URL".into()),
            tenant: "default".into(),
            schema_prefix: "onto_".into(),
            max_connections: 10,
            connect_timeout_secs: 10,
        }
    }
}

impl PostgresConfig {
    /// The URL from `url_env` or `url`.
    pub fn resolve_url(&self) -> Result<String> {
        if let Some(var) = &self.url_env
            && let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            return Ok(v);
        }
        self.url.clone().filter(|u| !u.is_empty()).ok_or_else(|| {
            Error::Config(format!(
                "engine.storage = \"postgres\" needs a URL: set {} or engine.postgres.url",
                self.url_env.as_deref().unwrap_or("engine.postgres.url_env")
            ))
        })
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
        Self { schema: "builtin:enterprise".into(), extend: Vec::new(), strict: false, schema_mode: SchemaMode::Live }
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Expose apply_action / undo_action to MCP clients.
    pub allow_writes: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// `none`: every caller is `default_principal`. `token`: bearer tokens required.
    pub mode: String,
    pub default_principal: Principal,
    pub tokens: Vec<TokenConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            mode: "none".into(),
            default_principal: Principal::new("local", vec![Role::Admin], vec!["*".into()]),
            tokens: Vec::new(),
        }
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

    pub fn db_path(&self) -> PathBuf {
        self.resolve_path(&self.engine.data_dir).join("onto.db")
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
        assert_eq!(cfg.auth.default_principal.id, "local");
    }
}
