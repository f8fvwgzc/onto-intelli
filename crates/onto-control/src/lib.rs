//! # onto-control
//!
//! The control plane of onto-intelli (docs/CONTROL_PLANE.md): users,
//! organizations with roles, invitations, projects (each one a Hub tenant
//! whose id is the project's `ref`), API keys and the audit log, in a
//! control store next to the tenants: [`PgControl`] (one PostgreSQL schema,
//! `console.schema`) or [`MemoryControl`].
//!
//! [`Control`] adds what the server needs around the store: the key pepper,
//! caches for keys (10 s), users (30 s) and memberships (30 s), batched
//! `last_used_at` writes, the key-creation rate limit, the bootstrap and
//! development sign-ins, OpenID Connect sign-in ([`oidc`], [`signin`]), and
//! each project's own LLM provider with its encrypted key, budget, rate
//! limit and usage ([`llm`], [`vault`], [`hooks`], [`net`]).

pub mod hooks;
pub mod keys;
pub mod llm;
pub mod model;
pub mod net;
pub mod oidc;
pub mod pg;
pub mod signin;
pub mod store;
pub mod vault;

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;

use onto_core::time::now_ms;
use onto_core::{Error, Result, Role};
use onto_intelli::config::ConsoleConfig;

pub use hooks::{ControlHooks, Meter};
pub use keys::{KeyCache, Pepper, new_ref};
pub use llm::{LlmTest, LlmUpdate, RotateReport};
pub use model::*;
pub use pg::PgControl;
pub use signin::{OpenTenant, StartRequest};
pub use store::{ControlStore, MemoryControl};
pub use vault::Vault;

/// How long a user or a membership is trusted before it is read again.
pub const USER_TTL: Duration = Duration::from_secs(30);
/// Key creations (and rotations) per project per minute.
pub const KEY_CREATIONS_PER_MINUTE: usize = 10;
/// Longest grace period of a rotation.
pub const MAX_GRACE_SECS: u64 = 86_400;

/// A small map whose entries expire.
struct TtlMap<K, V> {
    ttl: Duration,
    entries: Mutex<HashMap<K, (V, Instant)>>,
}

impl<K: Eq + Hash + Clone, V: Clone> TtlMap<K, V> {
    fn new(ttl: Duration) -> Self {
        Self { ttl, entries: Mutex::new(HashMap::new()) }
    }

    fn get(&self, k: &K, now: Instant) -> Option<V> {
        let mut m = self.entries.lock();
        match m.get(k) {
            Some((v, until)) if *until > now => Some(v.clone()),
            Some(_) => {
                m.remove(k);
                None
            }
            None => None,
        }
    }

    fn put(&self, k: K, v: V, now: Instant) {
        let mut m = self.entries.lock();
        if m.len() > 50_000 {
            m.retain(|_, (_, until)| *until > now);
        }
        m.insert(k, (v, now + self.ttl));
    }

    fn retain(&self, keep: impl Fn(&K, &V) -> bool) {
        self.entries.lock().retain(|k, (v, _)| keep(k, v));
    }
}

/// Secrets the control plane reads from the environment.
#[derive(Clone, Default)]
pub struct ControlOptions {
    /// Mixed into API key hashes; without it keys cannot be created.
    pub pepper: Option<Pepper>,
    /// `console.bootstrap_token_env`'s value.
    pub bootstrap_token: Option<String>,
    /// OIDC client secrets by provider id (the values of each provider's
    /// `client_secret_env`).
    pub oidc_secrets: HashMap<String, String>,
    /// `ONTO_MASTER_KEY` (and `ONTO_MASTER_KEY_PREVIOUS`): without it, LLM
    /// keys cannot be stored.
    pub vault: Option<Arc<Vault>>,
}

impl std::fmt::Debug for ControlOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlOptions")
            .field("pepper", &self.pepper)
            .field("bootstrap_token", &self.bootstrap_token.as_ref().map(|_| "****"))
            .field("oidc_secrets", &self.oidc_secrets.keys().collect::<Vec<_>>())
            .field("vault", &self.vault)
            .finish()
    }
}

impl ControlOptions {
    /// `ONTO_KEY_PEPPER` / `ONTO_MASTER_KEY`, the bootstrap token variable,
    /// each OIDC provider's `client_secret_env`, and the vault's master keys.
    pub fn from_env(cfg: &ConsoleConfig) -> Result<Self> {
        let var = |name: &str| std::env::var(name).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let bootstrap_token = cfg.bootstrap_token_env.as_deref().filter(|v| !v.trim().is_empty()).and_then(var);
        let oidc_secrets = cfg
            .oidc
            .iter()
            .filter_map(|p| p.client_secret_env.as_deref().and_then(var).map(|s| (p.id.clone(), s)))
            .collect();
        Ok(Self { pepper: Pepper::from_env()?, bootstrap_token, oidc_secrets, vault: Vault::from_env()?.map(Arc::new) })
    }
}

/// A key to create.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct KeyRequest {
    pub name: String,
    pub scopes: Vec<KeyScope>,
    /// Default `["*"]`.
    #[serde(default)]
    pub workspaces: Option<Vec<String>>,
    /// Unix milliseconds.
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub impersonate: bool,
    /// Roles the key may grant the users it acts for; a subset of the
    /// roles of its scopes.
    #[serde(default)]
    pub impersonate_roles: Vec<Role>,
}

/// The result of a bootstrap or development sign-in.
#[derive(Clone, Debug)]
pub struct SignIn {
    pub user: User,
    pub org: Org,
    /// Tenants adopted as projects by this sign-in.
    pub adopted: Vec<Project>,
}

/// The control store plus the server-side state around it.
pub struct Control {
    store: Arc<dyn ControlStore>,
    config: ConsoleConfig,
    pepper: Option<Pepper>,
    bootstrap_token: Option<String>,
    keys: KeyCache,
    users: TtlMap<String, Option<User>>,
    members: TtlMap<(String, String), Option<(Project, OrgRole)>>,
    touches: keys::Touches,
    key_creations: Mutex<HashMap<String, Vec<Instant>>>,
    /// `[[console.oidc]]`, in configuration order.
    oidc: Vec<Arc<oidc::Provider>>,
    /// OIDC sign-in starts in the last minute, per client and in all.
    oidc_starts: Mutex<signin::StartLimiter>,
    /// Seals and opens project LLM keys.
    vault: Option<Arc<Vault>>,
    /// LLM tests per project in the last minute.
    llm_tests: Mutex<HashMap<String, Vec<Instant>>>,
    /// Why a project's own provider is not in use, by project id.
    llm_errors: Mutex<HashMap<String, String>>,
    /// Usage meters by project id.
    meters: Mutex<HashMap<String, Arc<Meter>>>,
    /// The `settings_version` each open tenant's settings were read at.
    applied: Mutex<HashMap<String, i64>>,
    /// The hooks' background task runs.
    hooks_started: AtomicBool,
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control")
            .field("schema", &self.config.schema)
            .field("pepper", &self.pepper.is_some())
            .field("bootstrap", &self.bootstrap_token.is_some())
            .field("oidc", &self.oidc)
            .field("vault", &self.vault)
            .finish_non_exhaustive()
    }
}

impl Control {
    pub fn new(store: Arc<dyn ControlStore>, config: ConsoleConfig, opts: ControlOptions) -> Result<Self> {
        if let Some(t) = &opts.bootstrap_token
            && t.chars().count() < 32
        {
            return Err(Error::Config(format!(
                "the bootstrap admin token ({}) must have at least 32 characters",
                config.bootstrap_token_env.as_deref().unwrap_or("console.bootstrap_token_env")
            )));
        }
        let oidc = signin::providers(&config, &opts.oidc_secrets)?;
        Ok(Self {
            store,
            config,
            pepper: opts.pepper,
            bootstrap_token: opts.bootstrap_token,
            keys: KeyCache::default(),
            users: TtlMap::new(USER_TTL),
            members: TtlMap::new(USER_TTL),
            touches: keys::Touches::default(),
            key_creations: Mutex::new(HashMap::new()),
            oidc,
            oidc_starts: Mutex::new(signin::StartLimiter::default()),
            vault: opts.vault,
            llm_tests: Mutex::new(HashMap::new()),
            llm_errors: Mutex::new(HashMap::new()),
            meters: Mutex::new(HashMap::new()),
            applied: Mutex::new(HashMap::new()),
            hooks_started: AtomicBool::new(false),
        })
    }

    /// The memory control store. Without a configured pepper it uses a
    /// random one for this process (keys die with it anyway).
    pub fn memory(config: ConsoleConfig, mut opts: ControlOptions) -> Result<Self> {
        if opts.pepper.is_none() {
            opts.pepper = Some(Pepper::random());
        }
        Self::new(Arc::new(MemoryControl::new()), config, opts)
    }

    /// The PostgreSQL control store in `config.schema` (migrated on connect).
    pub async fn postgres(url: &str, config: ConsoleConfig, opts: ControlOptions) -> Result<Self> {
        let store = PgControl::connect(url, &config.schema).await?;
        Self::new(Arc::new(store), config, opts)
    }

    pub fn store(&self) -> &Arc<dyn ControlStore> {
        &self.store
    }

    pub fn config(&self) -> &ConsoleConfig {
        &self.config
    }

    /// `POST /v1/console/auth/token` is available.
    pub fn bootstrap_enabled(&self) -> bool {
        self.bootstrap_token.is_some()
    }

    /// The presented bootstrap token is right (constant time).
    pub fn check_bootstrap(&self, presented: &str) -> bool {
        self.bootstrap_token.as_deref().is_some_and(|t| keys::constant_time_eq(t.as_bytes(), presented.as_bytes()))
    }

    pub fn can_create_keys(&self) -> bool {
        self.pepper.is_some()
    }

    /// Write an audit entry; a failure is logged, never returned (the change
    /// it records already happened).
    pub async fn audit(&self, e: AuditEntry) {
        let action = e.action.clone();
        if let Err(err) = self.store.audit(e).await {
            tracing::error!(%action, "control audit log: {err}");
        }
    }

    // ── cached lookups ───────────────────────────────────────────────────

    /// A user, cached for [`USER_TTL`].
    pub async fn user(&self, id: &str) -> Result<Option<User>> {
        let now = Instant::now();
        if let Some(u) = self.users.get(&id.to_string(), now) {
            return Ok(u);
        }
        let u = self.store.user(id).await?;
        self.users.put(id.to_string(), u.clone(), now);
        Ok(u)
    }

    pub fn evict_user(&self, id: &str) {
        self.users.retain(|k, _| k != id);
        self.members.retain(|(u, _), _| u != id);
    }

    /// The user's role in an active project, cached for [`USER_TTL`].
    pub async fn membership(&self, user: &str, project_ref: &str) -> Result<Option<(Project, OrgRole)>> {
        let key = (user.to_string(), project_ref.to_string());
        let now = Instant::now();
        if let Some(m) = self.members.get(&key, now) {
            return Ok(m);
        }
        let m = self.store.membership(user, project_ref).await?;
        self.members.put(key, m.clone(), now);
        Ok(m)
    }

    /// Forget cached memberships in an organization (role changes, removals).
    pub fn evict_org(&self, org: &str) {
        // Cached misses go too: one of them may be a new member's.
        self.members.retain(|_, m| m.as_ref().is_some_and(|(p, _)| p.org_id != org));
    }

    /// Forget everything cached about a project (deleted).
    pub fn evict_project(&self, project: &Project) {
        self.members.retain(|(_, r), _| *r != project.r#ref);
        self.keys.evict_project(&project.id);
    }

    // ── API keys ─────────────────────────────────────────────────────────

    /// The key and its project, when `presented` is a valid, usable key of
    /// an active project; `None` otherwise (every refusal looks the same).
    pub async fn authenticate_key(&self, presented: &str) -> Result<Option<(ApiKey, Project)>> {
        self.authenticate_key_at(presented, Instant::now()).await
    }

    /// [`authenticate_key`](Self::authenticate_key) with the cache's clock given.
    pub async fn authenticate_key_at(&self, presented: &str, now: Instant) -> Result<Option<(ApiKey, Project)>> {
        let Some((prefix, _)) = keys::parse(presented) else { return Ok(None) };
        let Some(pepper) = &self.pepper else { return Ok(None) };
        let Some((key, project)) = self.cached_key(prefix, now).await? else { return Ok(None) };
        if !pepper.verify(presented, &key.hash) {
            return Ok(None);
        }
        let ms = now_ms();
        if !key.usable(ms) || !project.active() {
            return Ok(None);
        }
        self.touches.record(&key.id, ms);
        Ok(Some((key, project)))
    }

    async fn cached_key(&self, prefix: &str, now: Instant) -> Result<Option<(ApiKey, Project)>> {
        if let Some(e) = self.keys.get(prefix, now) {
            return Ok(e);
        }
        let found = match self.store.key_by_prefix(prefix).await? {
            Some(k) => self.store.project(&k.project_id).await?.map(|p| (k, p)),
            None => None,
        };
        self.keys.put(prefix, found.clone(), now);
        Ok(found)
    }

    /// A key that is still usable, by prefix, without its secret: for
    /// sessions a key minted (their signature already proves the key).
    pub async fn active_key(&self, prefix: &str) -> Result<Option<(ApiKey, Project)>> {
        let Some((key, project)) = self.cached_key(prefix, Instant::now()).await? else { return Ok(None) };
        Ok(Some((key, project)).filter(|(k, p)| k.usable(now_ms()) && p.active()))
    }

    /// Room for one more key creation in this project this minute.
    pub fn allow_key_creation(&self, project_id: &str) -> bool {
        let now = Instant::now();
        let mut m = self.key_creations.lock();
        let v = m.entry(project_id.to_string()).or_default();
        v.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if v.len() >= KEY_CREATIONS_PER_MINUTE {
            return false;
        }
        v.push(now);
        true
    }

    /// Create a key in `project`; returns it and the full secret (shown once).
    pub async fn create_key(&self, project: &Project, req: KeyRequest, by: &str) -> Result<(ApiKey, String)> {
        let (key, secret) = self.insert_key(project, req, by, None).await?;
        self.audit(AuditEntry::new(by, "key.create").project(project).target(&key.id).detail(json!({
            "name": key.name, "prefix": key.prefix, "scopes": key.scopes,
            "impersonate": key.impersonate, "expires_at": key.expires_at,
        })))
        .await;
        Ok((key, secret))
    }

    async fn insert_key(
        &self,
        project: &Project,
        req: KeyRequest,
        by: &str,
        rotated_from: Option<String>,
    ) -> Result<(ApiKey, String)> {
        let Some(pepper) = &self.pepper else {
            return Err(Error::invalid(
                "neither ONTO_KEY_PEPPER nor ONTO_MASTER_KEY is set; refusing to create API keys",
            ));
        };
        if !project.active() {
            return Err(Error::not_found(format!("project {}", project.r#ref)));
        }
        let name = check_name("key", &req.name)?;
        let mut scopes: Vec<KeyScope> = Vec::new();
        for s in req.scopes {
            if !scopes.contains(&s) {
                scopes.push(s);
            }
        }
        if scopes.is_empty() {
            return Err(Error::invalid("a key needs at least one scope (read, write, agent, admin)"));
        }
        let workspaces = req.workspaces.unwrap_or_else(|| vec!["*".into()]);
        if workspaces.is_empty() || workspaces.iter().any(|w| w.trim().is_empty() || w.len() > 128 || w.contains(',')) {
            return Err(Error::invalid("workspaces must be non-empty names (or \"*\")"));
        }
        let now = now_ms();
        if let Some(e) = req.expires_at
            && e <= now
        {
            return Err(Error::invalid("expires_at must be in the future (Unix milliseconds)"));
        }
        let roles = KeyScope::roles(&scopes);
        if !req.impersonate && !req.impersonate_roles.is_empty() {
            return Err(Error::invalid("impersonate_roles needs impersonate = true"));
        }
        if req.impersonate && req.impersonate_roles.is_empty() {
            return Err(Error::invalid("an impersonating key needs impersonate_roles"));
        }
        if let Some(r) = req.impersonate_roles.iter().find(|r| !roles.contains(r)) {
            return Err(Error::invalid(format!(
                "impersonate_roles must be roles of the key's scopes ({}); {r} is not",
                roles.iter().map(Role::as_str).collect::<Vec<_>>().join(", ")
            )));
        }
        for _ in 0..5 {
            let (prefix, full) = keys::generate();
            let new = NewKey {
                project_id: project.id.clone(),
                name: name.clone(),
                prefix,
                hash: pepper.hash(&full),
                scopes: scopes.clone(),
                workspaces: workspaces.clone(),
                impersonate: req.impersonate,
                impersonate_roles: req.impersonate_roles.clone(),
                created_by: by.to_string(),
                created_at: now,
                expires_at: req.expires_at,
                rotated_from: rotated_from.clone(),
            };
            match self.store.create_key(new).await {
                Ok(k) => {
                    self.keys.evict(&k.prefix);
                    return Ok((k, full));
                }
                // A prefix collision: pick another.
                Err(Error::Conflict(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::Conflict("could not pick a unique key prefix".into()))
    }

    /// A key of `project` by id.
    pub async fn project_key(&self, project: &Project, id: &str) -> Result<ApiKey> {
        match self.store.key(id).await? {
            Some(k) if k.project_id == project.id => Ok(k),
            _ => Err(Error::not_found(format!("key {id}"))),
        }
    }

    /// Replace a key with a new one (same name, scopes, workspaces and
    /// impersonation, `rotated_from` set). The old key is revoked now, or
    /// expires after `grace_secs` (at most a day).
    pub async fn rotate_key(&self, project: &Project, id: &str, grace_secs: u64, by: &str) -> Result<(ApiKey, String)> {
        if grace_secs > MAX_GRACE_SECS {
            return Err(Error::invalid(format!("grace_secs is at most {MAX_GRACE_SECS}")));
        }
        let old = self.project_key(project, id).await?;
        let now = now_ms();
        if !old.usable(now) {
            return Err(Error::Conflict(format!("key {} is revoked or expired", old.prefix)));
        }
        let req = KeyRequest {
            name: old.name.clone(),
            scopes: old.scopes.clone(),
            workspaces: Some(old.workspaces.clone()),
            expires_at: old.expires_at,
            impersonate: old.impersonate,
            impersonate_roles: old.impersonate_roles.clone(),
        };
        let (key, secret) = self.insert_key(project, req, by, Some(old.id.clone())).await?;
        if grace_secs == 0 {
            self.store.revoke_key(&old.id, by, now).await?;
        } else {
            self.store.expire_key(&old.id, now + grace_secs as i64 * 1000).await?;
        }
        self.keys.evict(&old.prefix);
        self.audit(AuditEntry::new(by, "key.rotate").project(project).target(&old.id).detail(json!({
            "old_prefix": old.prefix, "new_prefix": key.prefix, "new_key": key.id, "grace_secs": grace_secs,
        })))
        .await;
        Ok((key, secret))
    }

    /// Revoke a key of `project`; this replica stops accepting it at once.
    pub async fn revoke_key(&self, project: &Project, id: &str, by: &str) -> Result<ApiKey> {
        let key = self.project_key(project, id).await?;
        self.store.revoke_key(&key.id, by, now_ms()).await?;
        self.keys.evict(&key.prefix);
        self.audit(
            AuditEntry::new(by, "key.revoke")
                .project(project)
                .target(&key.id)
                .detail(json!({ "prefix": key.prefix, "name": key.name })),
        )
        .await;
        self.project_key(project, id).await
    }

    /// Write the key uses due now (each key at most once a minute).
    pub async fn flush_touches(&self) -> Result<usize> {
        let due = self.touches.due(now_ms());
        if due.is_empty() {
            return Ok(0);
        }
        self.store.touch_keys(&due).await?;
        Ok(due.len())
    }

    /// Background upkeep for as long as the control plane lives: key uses
    /// written every 10 s (each key at most once a minute), expired OIDC
    /// logins and tickets swept every 10 minutes.
    pub fn spawn_maintenance(self: &Arc<Self>) {
        let weak: Weak<Control> = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick: u64 = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let Some(me) = weak.upgrade() else { break };
                if let Err(e) = me.flush_touches().await {
                    tracing::warn!("writing API key last_used_at: {e}");
                }
                tick += 1;
                if tick.is_multiple_of(60) {
                    match me.store.sweep(now_ms()).await {
                        Ok(n) if n > 0 => tracing::debug!(rows = n, "swept expired logins and tickets"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("sweeping expired logins: {e}"),
                    }
                }
            }
        });
    }

    // ── sign-in without an IdP ───────────────────────────────────────────

    /// The bootstrap administrator (identity `local/admin`): the `default`
    /// organization is created on first use with the administrator as its
    /// owner, and existing tenants become its projects.
    pub async fn sign_in_local(&self, tenants: &[String]) -> Result<SignIn> {
        self.sign_in_as("local", "admin", "Administrator", "default", "Default", tenants).await
    }

    /// Development mode (`dev/dev`): the `dev` organization, adopting
    /// existing tenants the same way.
    pub async fn sign_in_dev(&self, tenants: &[String]) -> Result<SignIn> {
        self.sign_in_as("dev", "dev", "Developer", "dev", "Development", tenants).await
    }

    async fn sign_in_as(
        &self,
        provider: &str,
        subject: &str,
        name: &str,
        slug: &str,
        org_name: &str,
        tenants: &[String],
    ) -> Result<SignIn> {
        let now = now_ms();
        let user = self
            .store
            .upsert_login(LoginUpsert {
                provider: provider.into(),
                subject: subject.into(),
                name: name.into(),
                claims: json!({ "name": name }),
                now,
                ..Default::default()
            })
            .await?;
        if user.disabled {
            return Err(Error::Denied(format!("user {} is disabled", user.id)));
        }
        self.evict_user(&user.id);
        let org = match self.store.org(slug).await?.filter(|o| o.slug == slug) {
            Some(o) => o,
            None => match self.store.create_org(org_name, slug, &user.id).await {
                Ok(o) => {
                    self.audit(AuditEntry::new(&user.id, "org.create").org(&o.id).target(&o.id).detail(json!({
                        "name": o.name, "slug": o.slug,
                    })))
                    .await;
                    o
                }
                // Another replica created it at the same moment.
                Err(Error::Conflict(_)) => {
                    self.store.org(slug).await?.ok_or_else(|| Error::not_found(format!("organization {slug}")))?
                }
                Err(e) => return Err(e),
            },
        };
        let adopted = self.adopt_tenants(&org, tenants, &user.id).await?;
        // Cached misses for the adopted projects (or a new membership) go.
        self.evict_user(&user.id);
        self.audit(
            AuditEntry::new(&user.id, "auth.signin")
                .org(&org.id)
                .target(&user.id)
                .detail(json!({ "provider": provider })),
        )
        .await;
        Ok(SignIn { user, org, adopted })
    }

    /// Existing tenants that are no project yet become projects of `org`
    /// (`ref` = tenant id, named after it).
    pub async fn adopt_tenants(&self, org: &Org, tenants: &[String], by: &str) -> Result<Vec<Project>> {
        let mut adopted = Vec::new();
        for t in tenants {
            if let Some(p) = self.store.adopt_project(&org.id, t, by).await? {
                self.audit(AuditEntry::new(by, "project.adopt").project(&p).target(&p.r#ref)).await;
                adopted.push(p);
            }
        }
        Ok(adopted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn control() -> (Control, Project) {
        let c = Control::memory(ConsoleConfig::default(), ControlOptions::default()).unwrap();
        let s = c.sign_in_local(&["t1".to_string()]).await.unwrap();
        (c, s.adopted[0].clone())
    }

    fn req(scopes: &[KeyScope]) -> KeyRequest {
        KeyRequest { name: "ci".into(), scopes: scopes.to_vec(), ..Default::default() }
    }

    #[tokio::test]
    async fn revocation_is_local_at_once_and_remote_after_the_ttl() {
        let (a, project) = control().await;
        let (key, secret) = a.create_key(&project, req(&[KeyScope::Read]), "usr_x").await.unwrap();
        let t0 = Instant::now();
        assert!(a.authenticate_key_at(&secret, t0).await.unwrap().is_some());
        // Revoked through the store directly: what another replica does.
        a.store().revoke_key(&key.id, "usr_x", now_ms()).await.unwrap();
        assert!(a.authenticate_key_at(&secret, t0 + Duration::from_secs(5)).await.unwrap().is_some(), "cached");
        assert!(a.authenticate_key_at(&secret, t0 + Duration::from_secs(11)).await.unwrap().is_none(), "after TTL");
        // Revoked on this replica: refused at once.
        let (key2, secret2) = a.create_key(&project, req(&[KeyScope::Read]), "usr_x").await.unwrap();
        assert!(a.authenticate_key_at(&secret2, t0).await.unwrap().is_some());
        a.revoke_key(&project, &key2.id, "usr_x").await.unwrap();
        assert!(a.authenticate_key_at(&secret2, t0).await.unwrap().is_none());
        // Unknown prefixes are remembered as misses for 5 s.
        let (_, unknown) = keys::generate();
        assert!(a.authenticate_key_at(&unknown, t0).await.unwrap().is_none());
        assert_eq!(a.keys.get(&unknown[..keys::PREFIX_LEN], t0 + Duration::from_secs(4)), Some(None));
        assert_eq!(a.keys.get(&unknown[..keys::PREFIX_LEN], t0 + Duration::from_secs(6)), None);
    }

    #[tokio::test]
    async fn keys_are_validated_rotated_and_limited() {
        let (c, project) = control().await;
        assert!(c.create_key(&project, req(&[]), "u").await.is_err(), "no scope");
        let mut r = req(&[KeyScope::Write]);
        r.impersonate = true;
        r.impersonate_roles = vec![Role::Admin];
        assert!(c.create_key(&project, r.clone(), "u").await.unwrap_err().to_string().contains("impersonate_roles"));
        r.impersonate_roles = vec![Role::Member];
        let (k, _) = c.create_key(&project, r, "u").await.unwrap();
        assert!(k.impersonate);
        let mut past = req(&[KeyScope::Read]);
        past.expires_at = Some(now_ms() - 1);
        assert!(c.create_key(&project, past, "u").await.is_err());

        // Rotation without grace revokes the old key at once.
        let (old, old_secret) = c.create_key(&project, req(&[KeyScope::Agent]), "u").await.unwrap();
        let (new, new_secret) = c.rotate_key(&project, &old.id, 0, "u").await.unwrap();
        assert_eq!(new.rotated_from.as_deref(), Some(old.id.as_str()));
        assert_eq!((new.name.as_str(), new.scopes.clone()), ("ci", vec![KeyScope::Agent]));
        assert!(c.authenticate_key(&old_secret).await.unwrap().is_none());
        assert!(c.authenticate_key(&new_secret).await.unwrap().is_some());
        // With grace both work until the old one expires.
        let (newer, newer_secret) = c.rotate_key(&project, &new.id, 60, "u").await.unwrap();
        assert!(c.authenticate_key(&new_secret).await.unwrap().is_some());
        assert!(c.authenticate_key(&newer_secret).await.unwrap().is_some());
        let expires = c.store().key(&new.id).await.unwrap().unwrap().expires_at.unwrap();
        assert!(expires > now_ms() + 55_000 && expires <= now_ms() + 60_000);
        assert!(c.rotate_key(&project, &newer.id, MAX_GRACE_SECS + 1, "u").await.is_err());

        // Ten creations a minute per project.
        let fresh = c.store().create_project(&project.org_id, "p2", "u").await.unwrap();
        assert!((0..KEY_CREATIONS_PER_MINUTE).all(|_| c.allow_key_creation(&fresh.id)));
        assert!(!c.allow_key_creation(&fresh.id));

        // Keys of a deleted project stop working.
        c.store().delete_project(&project.r#ref, "u").await.unwrap();
        c.evict_project(&project);
        assert!(c.authenticate_key(&newer_secret).await.unwrap().is_none());

        // last_used_at is written in batches.
        let (k2, s2) = c.create_key(&fresh, req(&[KeyScope::Read]), "u").await.unwrap();
        assert!(c.authenticate_key(&s2).await.unwrap().is_some());
        assert!(c.flush_touches().await.unwrap() >= 1);
        assert!(c.store().key(&k2.id).await.unwrap().unwrap().last_used_at.is_some());
        assert!(c.authenticate_key(&s2).await.unwrap().is_some());
        assert_eq!(c.flush_touches().await.unwrap(), 0, "at most once a minute");
    }

    #[tokio::test]
    async fn bootstrap_token_and_pepper_rules() {
        let short = ControlOptions { bootstrap_token: Some("short".into()), ..Default::default() };
        assert!(Control::memory(ConsoleConfig::default(), short).is_err());
        let ok = ControlOptions { bootstrap_token: Some("t".repeat(40)), ..Default::default() };
        let c = Control::memory(ConsoleConfig::default(), ok).unwrap();
        assert!(c.check_bootstrap(&"t".repeat(40)));
        assert!(!c.check_bootstrap(&"t".repeat(39)));
        // Without a pepper (PostgreSQL store, no env), keys cannot be created.
        let no_pepper =
            Control::new(Arc::new(MemoryControl::new()), ConsoleConfig::default(), Default::default()).unwrap();
        let s = no_pepper.sign_in_local(&["t".to_string()]).await.unwrap();
        let err = no_pepper.create_key(&s.adopted[0], req(&[KeyScope::Read]), "u").await.unwrap_err();
        assert!(err.to_string().contains("ONTO_KEY_PEPPER"), "{err}");
        // Signing in again adopts only new tenants and keeps the organization.
        let again = no_pepper.sign_in_local(&["t".to_string(), "u".to_string()]).await.unwrap();
        assert_eq!(again.org.id, s.org.id);
        assert_eq!(again.adopted.iter().map(|p| p.r#ref.as_str()).collect::<Vec<_>>(), vec!["u"]);
        assert_eq!(again.user.id, s.user.id);
    }
}
