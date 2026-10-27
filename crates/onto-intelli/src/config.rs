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
    /// The control plane: organizations, projects, API keys, console sign-in.
    pub console: ConsoleConfig,
    /// Traces of asks, actions and calls (docs/CONTROL_PLANE.md section 9).
    pub traces: TracesConfig,
    /// Reading the change feed (`changes.poll`). Capture itself is a tenant
    /// property (`changes.capture`), not process configuration.
    pub changes: ChangesConfig,
    /// Directory relative paths are resolved against (the config file's dir).
    #[serde(skip)]
    pub base_dir: PathBuf,
    /// The keys the file's `[llm]` table sets: the LLM connection contract
    /// (`LLM_*`) fills only the others.
    #[serde(skip)]
    pub llm_explicit: std::collections::BTreeSet<String>,
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
    /// PostgreSQL tenants kept open at once (0: no limit); past it, the
    /// least recently used is closed. It opens again on its next request.
    pub max_open_tenants: usize,
    /// Close a PostgreSQL tenant nobody used for this many seconds (0: never).
    pub tenant_idle_secs: u64,
    /// Close the least recently used PostgreSQL tenants while the open ones'
    /// estimated memory (`estimated_bytes`) is above this many MiB (0: no budget).
    pub tenant_memory_budget_mb: u64,
    /// The deployment's wake scanner (PostgreSQL, `Hub::start_wake_scanner`)
    /// polls this often when no wake notice arrives (0: no scanner here).
    pub wake_poll_secs: u64,
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
            max_open_tenants: 512,
            tenant_idle_secs: 1800,
            tenant_memory_budget_mb: 0,
            wake_poll_secs: 20,
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
            extend: vec!["builtin:data-catalog".into(), "builtin:code".into()],
            strict: false,
            schema_mode: SchemaMode::Live,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cached_input_per_mtok: f64,
}

/// A secret value (an API key) whose `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// The value itself, for the one place that sends it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(****)")
    }
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
    /// A file holding the API key (a mounted Secret), read on every request
    /// so a rotated key applies at once. Wins over `api_key_env`.
    pub api_key_file: Option<PathBuf>,
    /// Send the key as `Authorization: Bearer` (a model gateway) instead of
    /// Anthropic's `x-api-key`. OpenAI-compatible servers always get a bearer token.
    pub bearer_auth: bool,
    /// The API key itself, set in code (a project's decrypted key); used
    /// before `api_key_env`. Never read from or written to a file.
    #[serde(skip)]
    pub api_key: Option<Secret>,
    pub max_output_tokens: u32,
    /// Only sent when set; several current models reject sampling parameters.
    pub temperature: Option<f32>,
    /// Anthropic `output_config.effort`: low | medium | high | xhigh | max.
    pub effort: Option<String>,
    /// Anthropic server-side refusal fallback (`fallbacks: "default"`).
    pub fallbacks: bool,
    pub timeout_secs: u64,
    /// Retries of one request after a transport error, 408, 429 or 5xx (0..=10).
    pub max_retries: u32,
    /// A small, fast model (`LLM_MODEL_SMALL`). Recorded and reported; onto
    /// makes no small-model calls today.
    pub model_small: String,
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
            api_key_file: None,
            bearer_auth: false,
            api_key: None,
            max_output_tokens: 4096,
            temperature: None,
            effort: None,
            fallbacks: true,
            timeout_secs: 120,
            max_retries: 0,
            model_small: String::new(),
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
    /// Token budget for recalled memories on the ask path (0 = unlimited).
    pub recall_budget_tokens: usize,
    /// ACT-R decay exponent `d` in `B = ln(Σ t_i^-d)` (time in hours).
    pub activation_decay: f32,
    /// Most recent access times kept per item for the activation estimate.
    pub activation_k: usize,
    /// Activation at which strength is 0.5.
    pub activation_tau: f32,
    /// Width of the logistic that maps activation to strength.
    pub activation_s: f32,
    /// Lowest strength of a current long-term fact or preference.
    pub fact_floor: f32,
    /// Consolidated, never-recalled episodes weaker than this are pruned.
    pub prune_below: f32,
    /// Current episodes kept per (workspace, subject); the weakest are evicted (0 = no cap).
    pub max_episodes_per_subject: usize,
    /// Current summaries kept per (workspace, subject) (0 = no cap).
    pub max_summaries_per_subject: usize,
    /// Similarity at which a new fact merges into an existing one.
    pub merge_threshold: f32,
    /// Token Jaccard at which a new fact about the same entities can supersede an old one.
    pub supersede_threshold: f32,
    /// Days a superseded item is kept for history and as-of recall (0 = forever).
    pub superseded_ttl_days: u32,
    /// BM25 score that maps to a lexical score of 0.5 (`s / (s + lex_saturation)`).
    pub lex_saturation: f32,
    /// MMR trade-off between relevance (1.0) and novelty (0.0).
    pub mmr_lambda: f32,
    /// Token overlap coefficient at which a candidate counts as a duplicate of a selected memory.
    pub dup_threshold: f32,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            recall_k: 4,
            episodic_ttl_days: 30,
            consolidate_after: 3,
            consolidate_every: 25,
            recall_budget_tokens: 600,
            activation_decay: 0.5,
            activation_k: 2,
            activation_tau: -2.5,
            activation_s: 1.0,
            fact_floor: 0.5,
            prune_below: 0.4,
            max_episodes_per_subject: 2000,
            max_summaries_per_subject: 200,
            merge_threshold: 0.85,
            supersede_threshold: 0.5,
            superseded_ttl_days: 180,
            lex_saturation: 3.0,
            mmr_lambda: 0.7,
            dup_threshold: 0.8,
        }
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
                "model",
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
    /// The tool profile (`full`, `catalog`, `authoring`, `reasoning`,
    /// `governance`, `retrieval`, `evaluation`; see
    /// [`crate::mcp::MCP_PROFILES`]). `ONTO_MCP_PROFILE` overrides it and
    /// `onto mcp --profile` overrides both: [`McpConfig::effective_profile`].
    pub profile: String,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            allow_writes: false,
            principal: Principal::new("mcp-agent", vec![Role::Agent], vec!["*".into()]),
            profile: "full".into(),
        }
    }
}

impl McpConfig {
    /// The profile an MCP server runs with: `cli` (the `onto mcp --profile`
    /// flag) when given, else `ONTO_MCP_PROFILE`, else `profile` (see
    /// [`crate::mcp::resolve_profile`]). Pass the result to
    /// [`crate::mcp::McpServer::with_profile`], which refuses unknown names.
    pub fn effective_profile(&self, cli: Option<&str>) -> String {
        crate::mcp::resolve_profile(cli, &self.profile)
    }

    /// `[mcp] profile` names a known profile (the environment and the CLI
    /// flag are checked when an MCP server starts).
    pub fn check(&self) -> Result<()> {
        let profile = self.profile.trim();
        if crate::mcp::MCP_PROFILES.contains(&profile) {
            return Ok(());
        }
        Err(Error::Config(format!(
            "[mcp] profile: unknown MCP profile '{profile}' (valid: {})",
            crate::mcp::MCP_PROFILES.join(", ")
        )))
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

/// `[console]`: the control plane (docs/CONTROL_PLANE.md). Organizations,
/// members, projects (each one a tenant), API keys and console sign-in, kept
/// in a control store next to the tenants.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsoleConfig {
    /// Serve the control plane. Requires `engine.tenancy = "multi"`.
    pub enabled: bool,
    /// Public base URL of this server: invitation links, and the OIDC
    /// redirect URI `{public_url}/v1/console/auth/oidc/callback`. Must be
    /// `https://` unless its host is loopback.
    pub public_url: String,
    /// PostgreSQL schema of the control store (in memory when
    /// `engine.storage = "memory"`). Never a tenant's schema.
    pub schema: String,
    /// The first sign-in through an IdP creates the user. When false, only
    /// invited users (by verified email) and existing users can sign in.
    pub allow_signup: bool,
    /// A new user with no membership gets an organization of their own.
    pub create_org_on_signup: bool,
    /// Verified email domains allowed to sign up; empty: any.
    pub allowed_email_domains: Vec<String>,
    /// Environment variable with the bootstrap admin token (at least 32
    /// characters): `POST /v1/console/auth/token` signs in as the local
    /// administrator.
    pub bootstrap_token_env: Option<String>,
    /// Console session lifetime (capped by `auth.session_max_ttl_secs`).
    pub session_ttl_secs: u64,
    /// Allow project LLM base URLs, and the git and PostgreSQL URLs of
    /// connectors registered by project credentials, that resolve to
    /// loopback or private addresses (a self-hosted Ollama, an internal git
    /// server or database), and `ssh://` and `user@host:path` git URLs from
    /// project credentials.
    pub allow_private_urls: bool,
    /// Active projects per organization (0: unlimited).
    pub max_projects_per_org: usize,
    /// OpenID Connect providers for console sign-in.
    pub oidc: Vec<OidcProviderConfig>,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            public_url: "http://127.0.0.1:50051".into(),
            schema: "onto_control".into(),
            allow_signup: true,
            create_org_on_signup: true,
            allowed_email_domains: Vec::new(),
            bootstrap_token_env: Some("ONTO_ADMIN_TOKEN".into()),
            session_ttl_secs: 8 * 3600,
            allow_private_urls: false,
            max_projects_per_org: 20,
            oidc: Vec::new(),
        }
    }
}

/// `[traces]`: Langfuse-style traces of the engine's work, kept in each
/// tenant's store (docs/CONTROL_PLANE.md section 9).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TracesConfig {
    pub enabled: bool,
    /// Traces older than this are deleted (a project overrides it with its
    /// `trace_retention_days` setting, 1 to 400).
    pub retention_days: u32,
    /// Store question, prompt and answer text (clipped). Without it, only
    /// lengths and token counts (a project overrides it with `capture_content`).
    pub capture_content: bool,
    /// Each stored text is clipped to this many bytes (at a character boundary).
    pub max_content_bytes: usize,
    /// Engine calls that become traces besides `intelli.ask`: `writes`
    /// (writing actions and LLM-using actions), `all`, or `none`.
    pub calls: String,
    /// The background writer flushes every this many milliseconds …
    pub flush_ms: u64,
    /// … or this many records, whichever comes first.
    pub flush_records: usize,
    /// Records waiting to be written; beyond this they are dropped and
    /// counted (`traces_dropped` in the ledger).
    pub queue: usize,
}

impl Default for TracesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: 30,
            capture_content: true,
            max_content_bytes: 4096,
            calls: "writes".into(),
            flush_ms: 500,
            flush_records: 200,
            queue: 10_000,
        }
    }
}

impl TracesConfig {
    /// `calls` is `writes`, `all` or `none`, and the sizes are positive.
    pub fn check(&self) -> Result<()> {
        if !matches!(self.calls.as_str(), "writes" | "all" | "none") {
            return Err(Error::Config(format!("traces.calls = '{}' (writes, all, none)", self.calls)));
        }
        if self.queue == 0 || self.flush_records == 0 || self.retention_days == 0 {
            return Err(Error::Config("traces.queue, flush_records and retention_days must be positive".into()));
        }
        Ok(())
    }
}

/// `[changes]`: reading the change feed (docs/design/EVENTS.md, Part A).
/// Whether commits record changes is a tenant property, switched with the
/// `changes.capture` action, so replicas never disagree about it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ChangesConfig {
    /// Records scanned per `changes.poll` page when the call gives no
    /// `limit`, and per stream page (1 to 1000).
    pub page: usize,
    /// Records kept in memory per tenant while it has streams (the ring;
    /// allocated only while subscribed and counted in the memory budget).
    pub ring: usize,
    /// Milliseconds one stream page may spend filtering before it is split.
    pub eval_budget_ms: u64,
    /// A stream sends a heartbeat (the last cursor scanned) this often.
    pub heartbeat_secs: u64,
    /// A stream whose client reads nothing for this long ends `slow_consumer`.
    pub stall_timeout_secs: u64,
    /// Commits (seqs) a stream may be behind the head before it ends
    /// `too_far_behind` (page with `changes.poll`, then stream again).
    pub max_backfill_seqs: u64,
    /// Streams authenticate their request again this often; a revoked
    /// credential or a changed principal ends the stream.
    pub reauth_secs: u64,
    /// Streams per tenant in this process.
    pub max_subscribers: usize,
    /// Streams per principal id per tenant in this process.
    pub max_subscribers_per_principal: usize,
    /// Streams in this process, all tenants together.
    pub max_subscribers_process: usize,
}

impl Default for ChangesConfig {
    fn default() -> Self {
        Self {
            page: 500,
            ring: 4096,
            eval_budget_ms: 50,
            heartbeat_secs: 15,
            stall_timeout_secs: 60,
            max_backfill_seqs: 100_000,
            reauth_secs: 60,
            max_subscribers: 200,
            max_subscribers_per_principal: 20,
            max_subscribers_process: 2000,
        }
    }
}

impl ChangesConfig {
    /// `page` is 1 to [`onto_core::changes::MAX_PAGE`]; times, the ring and
    /// the limits are at least 1.
    pub fn check(&self) -> Result<()> {
        let max = onto_core::changes::MAX_PAGE;
        if self.page == 0 || self.page > max {
            return Err(Error::Config(format!("changes.page = {} (1 to {max})", self.page)));
        }
        let at_least_one = [
            ("ring", self.ring as u64),
            ("eval_budget_ms", self.eval_budget_ms),
            ("heartbeat_secs", self.heartbeat_secs),
            ("stall_timeout_secs", self.stall_timeout_secs),
            ("max_backfill_seqs", self.max_backfill_seqs),
            ("reauth_secs", self.reauth_secs),
            ("max_subscribers", self.max_subscribers as u64),
            ("max_subscribers_per_principal", self.max_subscribers_per_principal as u64),
            ("max_subscribers_process", self.max_subscribers_process as u64),
        ];
        if let Some((name, v)) = at_least_one.iter().find(|(_, v)| *v == 0) {
            return Err(Error::Config(format!("changes.{name} = {v} (at least 1)")));
        }
        Ok(())
    }
}

/// `[[console.oidc]]`: one OpenID Connect provider (Clerk, Auth0, Supabase
/// Auth, Google, Keycloak, Okta, Entra, ...).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OidcProviderConfig {
    /// `[a-z0-9-]`, used in URLs.
    pub id: String,
    /// Shown on the sign-in button.
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    /// Environment variable with the client secret; omit for a public
    /// client (PKCE only).
    #[serde(default)]
    pub client_secret_env: Option<String>,
    #[serde(default = "default_oidc_scopes")]
    pub scopes: Vec<String>,
    /// Extra accepted `aud` values besides `client_id`.
    #[serde(default)]
    pub audiences: Vec<String>,
}

fn default_oidc_scopes() -> Vec<String> {
    ["openid", "email", "profile"].map(String::from).to_vec()
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
        let mut cfg: Self = toml::from_str(text)?;
        if let Ok(t) = toml::from_str::<toml::Table>(text)
            && let Some(toml::Value::Table(llm)) = t.get("llm")
        {
            cfg.llm_explicit = llm.keys().cloned().collect();
        }
        Ok(cfg)
    }

    /// Apply the platform's LLM connection contract (`LLM_*` environment
    /// variables) to the `[llm]` keys the file did not set. Project providers
    /// (the console's bring-your-own LLM) are not touched; they win anyway.
    pub fn apply_llm_contract(&mut self) -> Result<()> {
        self.apply_llm_contract_from(|k| std::env::var(k).ok()).map(|_| ())
    }

    /// [`apply_llm_contract`](Self::apply_llm_contract) from any variable
    /// source. What it ignores is logged as a warning, and returned.
    pub fn apply_llm_contract_from(&mut self, get: impl Fn(&str) -> Option<String>) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        match crate::llm::LlmContract::from_vars(&get)? {
            Some(c) => warnings.extend(c.apply(&mut self.llm, &self.llm_explicit)),
            None => {
                let stray = crate::llm::stray_variables(&get);
                if !stray.is_empty() {
                    warnings.push(format!(
                        "Ignoring {} without LLM_PROVIDER or LLM_GATEWAY_URL (the LLM connection contract is off)",
                        stray.join(", ")
                    ));
                }
            }
        }
        for w in &warnings {
            tracing::warn!("{w}");
        }
        Ok(warnings)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text =
            std::fs::read_to_string(path).map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?;
        let mut cfg = Self::from_toml(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        cfg.base_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        cfg.apply_llm_contract()?;
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
        let mut cfg = Self::default();
        cfg.apply_llm_contract()?;
        Ok(cfg)
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
        assert_eq!(cfg.mcp.profile, "full");
        assert!(cfg.mcp.check().is_ok());
        let bad = Config::from_toml("[mcp]\nprofile = 'everything'\n").unwrap();
        assert!(bad.mcp.check().unwrap_err().to_string().contains("catalog, authoring"));
        let legacy = Config::from_toml("[auth]\ndefault_principal = { id = 'old', roles = ['viewer'] }\n").unwrap();
        assert_eq!(legacy.auth.dev_principal.id, "old");
        assert!(!cfg.console.enabled);
        assert_eq!(cfg.console.schema, "onto_control");
        assert!(cfg.traces.enabled && cfg.traces.capture_content);
        assert_eq!((cfg.traces.calls.as_str(), cfg.traces.retention_days, cfg.traces.queue), ("writes", 30, 10_000));
        assert!(cfg.traces.check().is_ok());
        let bad = Config::from_toml("[traces]\ncalls = 'some'\n").unwrap();
        assert!(bad.traces.check().unwrap_err().to_string().contains("writes, all, none"));
    }

    #[test]
    fn api_keys_never_show() {
        let cfg = LlmConfig { api_key: Some(Secret::new("sk-live-123456")), ..Default::default() };
        let debug = format!("{cfg:?}");
        assert!(debug.contains("Secret(****)") && !debug.contains("sk-live"), "{debug}");
        let toml = toml::to_string(&cfg).unwrap();
        assert!(!toml.contains("sk-live") && !toml.contains("api_key ="), "{toml}");
        let back: LlmConfig = toml::from_str("api_key = 'from-a-file'\nprovider = 'openai'").unwrap();
        assert!(back.api_key.is_none(), "never read from a file");
    }

    #[test]
    fn console_section_parses() {
        let cfg = Config::from_toml(
            "[console]\nenabled = true\npublic_url = 'https://onto.example.com'\n\
             [[console.oidc]]\nid = 'clerk'\nname = 'Clerk'\nissuer = 'https://x.clerk.accounts.dev'\n\
             client_id = 'abc'\nclient_secret_env = 'ONTO_OIDC_CLERK_SECRET'\n",
        )
        .unwrap();
        assert!(cfg.console.enabled);
        assert_eq!(cfg.console.oidc[0].scopes, vec!["openid", "email", "profile"]);
        assert_eq!(cfg.console.session_ttl_secs, 28_800);
        assert_eq!(cfg.console.bootstrap_token_env.as_deref(), Some("ONTO_ADMIN_TOKEN"));
    }
}
