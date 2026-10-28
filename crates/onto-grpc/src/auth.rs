//! Authentication shared by gRPC, HTTP and the web UI.
//!
//! The presented credential (`authorization: Bearer …` or `x-api-key`) is
//! classified by its form:
//!
//! 1. `onto1.…`: a session token, signed with HMAC-SHA256 and short-lived.
//!    Minted by `POST /v1/auth/session`, it carries one principal and tenant
//!    and is what a browser holds, so an application never hands its
//!    service token to a page. A **console session** (minted by the
//!    `/v1/console/auth/…` routes) names a user instead: each data-plane
//!    request names a project with `x-onto-tenant`, and the user's role in
//!    that project's organization gives the principal. Sessions cannot
//!    impersonate.
//! 2. `onto_sk_…`: a project API key from the control store. Its project is
//!    its tenant (`x-onto-tenant`, when sent, must be that project's ref);
//!    its scopes give its roles.
//! 3. Anything else: the `[[auth.tokens]]` of the configuration.
//!
//! Without a credential, development mode (`auth.mode = "dev"` or `onto serve
//! --dev`, loopback only) makes every caller `auth.dev_principal`.
//!
//! A token or key with `impersonate = true` belongs to a trusted service that
//! acts for its own users: `x-onto-act-as`, `x-onto-act-roles`,
//! `x-onto-act-workspaces` and `x-onto-act-entity` name each call's
//! principal, limited to `impersonate_roles` and the credential's workspaces.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::HeaderMap;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;

use onto_control::{Control, OrgRole, User};
use onto_intelli::config::AuthConfig;
use onto_intelli::{Error, Principal, Result, Role};

pub const ACT_AS: &str = "x-onto-act-as";
pub const ACT_ROLES: &str = "x-onto-act-roles";
pub const ACT_WORKSPACES: &str = "x-onto-act-workspaces";
pub const ACT_ENTITY: &str = "x-onto-act-entity";
pub const TENANT: &str = "x-onto-tenant";
pub const WORKSPACE: &str = "x-onto-workspace";

struct Token {
    secret: String,
    principal: Principal,
    impersonate: bool,
    impersonate_roles: Vec<Role>,
    tenants: Vec<String>,
}

pub struct Auth {
    dev: Option<Principal>,
    tokens: Vec<Token>,
    session_key: Vec<u8>,
    session_max_ttl: u64,
    control: Option<Arc<Control>>,
    revocations: RwLock<Option<Arc<dyn Revocations>>>,
    /// Session ids by whether they are revoked, and when that was read.
    revoked: Mutex<HashMap<String, (bool, Instant)>>,
}

/// How long a session id read as not revoked is trusted before it is read
/// again. A revocation on this replica applies at once.
pub const REVOCATION_TTL: Duration = Duration::from_secs(10);

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// How long past a session's expiry its revocation is kept, so a request
/// verified just before the expiry still finds it.
pub const REVOCATION_GRACE_SECS: u64 = 60;

/// Where revoked session ids are kept, shared by every replica and by every
/// tenant (a session id names no tenant): the default tenant's KV store,
/// namespace [`crate::REVOKED_NS`], until `exp` (Unix seconds).
pub trait Revocations: Send + Sync {
    /// Revoke `id` for the sessions of `tenants`, or of every tenant (`None`).
    fn revoke<'a>(&'a self, id: &'a str, tenants: Option<&'a [String]>, exp: u64) -> BoxFuture<'a, ()>;
    /// Is `id` revoked for a session of `tenant`?
    fn is_revoked<'a>(&'a self, id: &'a str, tenant: Option<&'a str>) -> BoxFuture<'a, bool>;
}

/// A session presented with the request: when it expires and its ids.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    /// Unix seconds.
    pub exp: u64,
    /// `None` for sessions minted before sessions had ids.
    pub jti: Option<String>,
    /// The login the session belongs to: the `jti` of the first session,
    /// inherited by every session minted from it. Revoking it revokes them all.
    pub sid: Option<String>,
}

impl SessionInfo {
    /// The login: `sid`, or the session's own id.
    pub fn login(&self) -> Option<&str> {
        self.sid.as_deref().or(self.jti.as_deref())
    }

    /// Minted from another session (not the first of its login). Such a
    /// session mints no sessions, so revoking its `jti` leaves none behind.
    pub fn is_derived(&self) -> bool {
        matches!((&self.jti, &self.sid), (Some(jti), Some(sid)) if jti != sid)
    }
}

/// The expiry a session id carries (`<random>.<exp>`, Unix seconds); ids
/// minted before carry none.
pub fn id_expiry(id: &str) -> Option<u64> {
    id.rsplit_once('.').and_then(|(_, exp)| exp.parse().ok())
}

/// The console part of a session token: the user it was issued to.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ConsoleClaims {
    pub user: String,
    /// Issued at, Unix milliseconds (compared with `sessions_valid_after`).
    pub iat: u64,
}

/// What a session token says.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SessionClaims {
    pub principal: Principal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Unix seconds.
    pub exp: u64,
    /// Set on console sessions only; older tokens have none and still verify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console: Option<ConsoleClaims>,
    /// The prefix of the API key that minted this session: the session is
    /// that key's delegate, refused once the key is revoked or expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The session's id, for revocation. Older tokens have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
    /// The login's id ([`SessionInfo::sid`]); refused once it is revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
}

const SESSION_PREFIX: &str = "onto1.";
const KEY_PREFIX: &str = onto_control::keys::KEY_PREFIX;

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Why a request was refused: 401 (no or bad credentials), 403 (not
/// allowed) or 503 (the control store could not be reached).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    Unauthenticated(String),
    Forbidden(String),
    Unavailable(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Unauthenticated(m) | AuthError::Forbidden(m) | AuthError::Unavailable(m) => f.write_str(m),
        }
    }
}

fn unavailable(e: Error) -> AuthError {
    AuthError::Unavailable(format!("control store: {e}"))
}

/// What authenticated a request.
#[derive(Clone, Debug, PartialEq)]
pub enum Credential {
    /// A `[[auth.tokens]]` entry and the tenants it may address (`*`: every one).
    Config { tenants: Vec<String> },
    /// A project API key.
    ApiKey { id: String, prefix: String, project: String },
    /// A console session; `role` is the user's role in the project's organization `org`.
    Console { user: String, role: OrgRole, org: String },
    /// A session minted by `POST /v1/auth/session`.
    Session,
    /// Development mode, no credential.
    Dev,
}

impl Credential {
    /// API keys and console sessions act for a project's members, not for
    /// the operator: some calls that touch the server are refused to them.
    pub fn is_project(&self) -> bool {
        matches!(self, Credential::ApiKey { .. } | Credential::Console { .. })
    }
}

/// An authenticated caller: the principal of this call and the tenant it addresses.
#[derive(Clone, Debug)]
pub struct Caller {
    pub principal: Principal,
    pub tenant: Option<String>,
    pub credential: Credential,
    /// Set when a session token (not a key or a config token) authenticated
    /// the request: a session it mints may not outlive it.
    pub session: Option<SessionInfo>,
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty())
}

fn list(v: &str) -> Vec<String> {
    v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// The bearer token or `x-api-key` of a request.
pub fn presented(h: &HeaderMap) -> Option<&str> {
    header(h, "authorization")
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .or_else(|| header(h, "x-api-key"))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// The `x-onto-act-*` principal of a call, or `base` when none is named.
/// `limit_roles`: only `allowed_roles` may be granted (off in dev mode).
fn act_for(
    base: Principal,
    h: &HeaderMap,
    may_impersonate: bool,
    allowed_roles: &[Role],
    limit_roles: bool,
) -> std::result::Result<Principal, AuthError> {
    let Some(act_as) = header(h, ACT_AS) else {
        return Ok(base);
    };
    if !may_impersonate {
        return Err(AuthError::Forbidden(format!("{} may not act for other principals", base.id)));
    }
    let roles: Vec<Role> = match header(h, ACT_ROLES) {
        Some(v) => list(v)
            .iter()
            .map(|r| Role::parse(r))
            .collect::<Result<Vec<_>>>()
            .map_err(|e| AuthError::Forbidden(e.to_string()))?,
        None => vec![Role::Viewer],
    };
    if limit_roles {
        for r in &roles {
            if !allowed_roles.contains(r) {
                return Err(AuthError::Forbidden(format!(
                    "{} may not grant role {r} (impersonate_roles: {})",
                    base.id,
                    allowed_roles.iter().map(Role::as_str).collect::<Vec<_>>().join(", ")
                )));
            }
        }
    }
    let workspaces = match header(h, ACT_WORKSPACES) {
        Some(v) => {
            let asked = list(v);
            if let Some(w) = asked.iter().find(|w| !base.in_workspace(w)) {
                return Err(AuthError::Forbidden(format!("{} has no access to workspace {w}", base.id)));
            }
            asked
        }
        None => base.workspaces.clone(),
    };
    let mut p = Principal::new(act_as, roles, workspaces);
    p.entity = header(h, ACT_ENTITY).map(str::to_string);
    p.via = Some(base.id.clone());
    Ok(p)
}

impl Auth {
    /// `dev` forces development mode (`onto serve --dev`).
    pub fn from_config(cfg: &AuthConfig, dev: bool) -> Result<Self> {
        Self::from_config_with(cfg, dev, None)
    }

    /// With a control plane: API keys and console sessions are accepted, and
    /// `[[auth.tokens]]` become optional when the console has a sign-in path
    /// (an OIDC provider or the bootstrap token).
    pub fn from_config_with(cfg: &AuthConfig, dev: bool, control: Option<Arc<Control>>) -> Result<Self> {
        let dev = match (cfg.mode.as_str(), dev) {
            (_, true) | ("dev", _) => Some(cfg.dev_principal.clone()),
            ("token", false) | ("", false) => None,
            ("none", false) => {
                return Err(Error::Config(
                    "auth.mode = \"none\" is no longer supported: configure [[auth.tokens]], or run \
                     `onto serve --dev` (or auth.mode = \"dev\") for local development"
                        .into(),
                ));
            }
            (other, false) => return Err(Error::Config(format!("unknown auth.mode '{other}' (token, dev)"))),
        };
        let mut tokens = Vec::new();
        for t in &cfg.tokens {
            let value = match (&t.token_env, &t.token) {
                (Some(var), _) => std::env::var(var).ok(),
                (None, Some(tok)) => Some(tok.clone()),
                (None, None) => None,
            };
            match value.filter(|v| !v.trim().is_empty()) {
                Some(v) if v.trim().len() < 16 && t.token_env.is_some() => {
                    return Err(Error::Config(format!(
                        "auth token for {} is too short (use at least 16 random characters)",
                        t.principal.id
                    )));
                }
                Some(v) if v.trim().starts_with(SESSION_PREFIX) || v.trim().starts_with(KEY_PREFIX) => {
                    return Err(Error::Config(format!(
                        "auth token for {} starts with {SESSION_PREFIX} or {KEY_PREFIX}, which are reserved",
                        t.principal.id
                    )));
                }
                Some(v) => tokens.push(Token {
                    secret: v.trim().to_string(),
                    principal: t.principal.clone(),
                    impersonate: t.impersonate,
                    impersonate_roles: t.impersonate_roles.clone(),
                    tenants: t.tenants.clone(),
                }),
                None => tracing::warn!("auth token for {} has no value (set {:?})", t.principal.id, t.token_env),
            }
        }
        let console_sign_in = control.as_ref().is_some_and(|c| c.bootstrap_enabled() || !c.config().oidc.is_empty());
        if dev.is_none() && tokens.is_empty() && !console_sign_in {
            return Err(Error::Config(
                "no auth token resolved: add [[auth.tokens]] with token_env set (e.g. ONTO_TOKEN), \
                 or run `onto serve --dev` for local development"
                    .into(),
            ));
        }
        let session_key = match cfg.session_secret_env.as_deref().and_then(|v| std::env::var(v).ok()) {
            Some(s) if s.trim().len() >= 32 => s.trim().as_bytes().to_vec(),
            Some(_) => {
                return Err(Error::Config("the session secret must have at least 32 characters".into()));
            }
            None => {
                let mut key = vec![0u8; 32];
                getrandom::fill(&mut key).map_err(|e| Error::Unavailable(format!("random session key: {e}")))?;
                key
            }
        };
        Ok(Self {
            dev,
            tokens,
            session_key,
            session_max_ttl: cfg.session_max_ttl_secs.max(60),
            control,
            revocations: RwLock::new(None),
            revoked: Mutex::new(HashMap::new()),
        })
    }

    /// Where revoked session ids are kept (the server passes the hub's).
    pub fn set_revocations(&self, r: Arc<dyn Revocations>) {
        *self.revocations.write() = Some(r);
    }

    /// Revoke a session id or a login id (a `sid`, which is its first
    /// session's `jti`) for the sessions of `tenants`, or of every tenant
    /// (`None`): refused from now on, on every replica within
    /// [`REVOCATION_TTL`] and on this one at once. It stays revoked until
    /// the expiry the id carries (no session of the id lives longer), plus
    /// [`REVOCATION_GRACE_SECS`]: the same clock that expires the session
    /// drops the revocation, whatever `session_max_ttl_secs` is now. Ids
    /// minted before carry no expiry: those stay revoked until `exp_hint`,
    /// and at least as long as a session minted now could live.
    pub async fn revoke_session(&self, id: &str, tenants: Option<&[String]>, exp_hint: u64) -> Result<()> {
        let r = self.revocations.read().clone();
        let Some(r) = r else {
            return Err(Error::Unavailable("this server keeps no session revocations".into()));
        };
        let until = match id_expiry(id) {
            Some(exp) => exp.saturating_add(REVOCATION_GRACE_SECS),
            None => exp_hint.max(now_secs() + self.session_max_ttl),
        };
        r.revoke(id, tenants, until).await?;
        let mut cache = self.revoked.lock();
        match tenants {
            None => {
                cache.insert(id.to_string(), (true, Instant::now()));
            }
            Some(tenants) => {
                for t in tenants {
                    cache.insert(scoped_id(id, Some(t)), (true, Instant::now()));
                }
            }
        }
        Ok(())
    }

    /// Is `id` revoked for a session of `tenant`: for every tenant, or for that one?
    async fn session_revoked(&self, id: &str, tenant: Option<&str>) -> std::result::Result<bool, AuthError> {
        let now = Instant::now();
        let key = scoped_id(id, tenant);
        {
            let cache = self.revoked.lock();
            // Revoked for every tenant on this replica.
            if cache.get(id).is_some_and(|(revoked, _)| *revoked) {
                return Ok(true);
            }
            if let Some((revoked, at)) = cache.get(&key).cloned()
                && (revoked || now.duration_since(at) < REVOCATION_TTL)
            {
                return Ok(revoked);
            }
        }
        let r = self.revocations.read().clone();
        let Some(r) = r else { return Ok(false) };
        let revoked =
            r.is_revoked(id, tenant).await.map_err(|e| AuthError::Unavailable(format!("revocations: {e}")))?;
        let mut cache = self.revoked.lock();
        if cache.len() > 100_000 {
            cache.retain(|_, (r, at)| *r || now.duration_since(*at) < REVOCATION_TTL);
        }
        cache.insert(key, (revoked, now));
        Ok(revoked)
    }

    pub fn control(&self) -> Option<&Arc<Control>> {
        self.control.as_ref()
    }

    /// `console.allow_private_urls` of the control plane (false without one).
    pub fn allow_private_urls(&self) -> bool {
        self.control.as_ref().is_some_and(|c| c.config().allow_private_urls)
    }

    fn sign(&self, payload: &str) -> String {
        use base64::Engine;
        use hmac::{KeyInit, Mac};
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.session_key).expect("any key length");
        mac.update(payload.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    fn mint(&self, claims: &SessionClaims) -> String {
        use base64::Engine;
        let body =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap_or_default());
        let payload = format!("{SESSION_PREFIX}{body}");
        let sig = self.sign(&payload);
        format!("{payload}.{sig}")
    }

    /// A signed, short-lived token for one principal and tenant.
    pub fn issue_session(&self, principal: Principal, tenant: Option<String>, ttl_secs: u64) -> (String, u64) {
        let (token, s) = self.issue_session_from(principal, tenant, ttl_secs, None);
        (token, s.exp)
    }

    /// [`issue_session`](Self::issue_session), minted by the session
    /// `parent` when there is one: it expires no later and belongs to the
    /// same login. Also returns the new session's ids.
    pub fn issue_session_from(
        &self,
        principal: Principal,
        tenant: Option<String>,
        ttl_secs: u64,
        parent: Option<&SessionInfo>,
    ) -> (String, SessionInfo) {
        let s = self.next_session(ttl_secs, parent);
        let claims = SessionClaims {
            principal,
            tenant,
            exp: s.exp,
            console: None,
            key: None,
            jti: s.jti.clone(),
            sid: s.sid.clone(),
        };
        (self.mint(&claims), s)
    }

    /// A session minted by an API key: bound to the key's project and valid
    /// only while the key is.
    pub fn issue_key_session(&self, principal: Principal, prefix: &str, project: &str, ttl_secs: u64) -> (String, u64) {
        let (token, s) = self.issue_key_session_from(principal, prefix, project, ttl_secs, None);
        (token, s.exp)
    }

    /// [`issue_key_session`](Self::issue_key_session), minted by `parent`
    /// as in [`issue_session_from`](Self::issue_session_from).
    pub fn issue_key_session_from(
        &self,
        principal: Principal,
        prefix: &str,
        project: &str,
        ttl_secs: u64,
        parent: Option<&SessionInfo>,
    ) -> (String, SessionInfo) {
        let s = self.next_session(ttl_secs, parent);
        let claims = SessionClaims {
            principal,
            tenant: Some(project.to_string()),
            exp: s.exp,
            console: None,
            key: Some(prefix.to_string()),
            jti: s.jti.clone(),
            sid: s.sid.clone(),
        };
        (self.mint(&claims), s)
    }

    /// A new session's expiry and ids: a session minting a session cannot
    /// outlive itself, and passes its login on. The id carries the expiry,
    /// so a revocation of it lasts exactly as long as it must.
    fn next_session(&self, ttl_secs: u64, parent: Option<&SessionInfo>) -> SessionInfo {
        let mut exp = now_secs() + ttl_secs.clamp(60, self.session_max_ttl);
        if let Some(parent) = parent {
            exp = exp.min(parent.exp);
        }
        let jti = new_jti(exp);
        let sid = parent.and_then(SessionInfo::login).map_or_else(|| jti.clone(), str::to_string);
        SessionInfo { exp, jti: Some(jti), sid: Some(sid) }
    }

    /// A console session for a user: principal `user:<id>` without roles and
    /// without a tenant, for `console.session_ttl_secs` (capped by
    /// `auth.session_max_ttl_secs`).
    pub fn issue_console_session(&self, user: &str, ttl_secs: u64) -> (String, u64) {
        let exp = now_secs() + ttl_secs.clamp(60, self.session_max_ttl);
        let iat = onto_intelli::core::time::now_ms().max(0) as u64;
        let claims = SessionClaims {
            principal: Principal::new(format!("user:{user}"), Vec::new(), vec!["*".into()]),
            tenant: None,
            exp,
            console: Some(ConsoleClaims { user: user.to_string(), iat }),
            key: None,
            jti: None,
            sid: None,
        };
        (self.mint(&claims), exp)
    }

    fn verify_session(&self, token: &str) -> std::result::Result<SessionClaims, AuthError> {
        use base64::Engine;
        let bad = || AuthError::Unauthenticated("invalid session token".into());
        let (payload, sig) = token.rsplit_once('.').ok_or_else(bad)?;
        if !constant_time_eq(self.sign(payload).as_bytes(), sig.as_bytes()) {
            return Err(bad());
        }
        let body = payload.strip_prefix(SESSION_PREFIX).ok_or_else(bad)?;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body).map_err(|_| bad())?;
        let claims: SessionClaims = serde_json::from_slice(&raw).map_err(|_| bad())?;
        if claims.exp <= now_secs() {
            return Err(AuthError::Unauthenticated("session expired".into()));
        }
        Ok(claims)
    }

    pub fn dev_mode(&self) -> bool {
        self.dev.is_some()
    }

    /// The user of a console session: it must exist, not be disabled, and the
    /// session must be newer than the user's last "sign out everywhere".
    async fn console_user(&self, cc: &ConsoleClaims) -> std::result::Result<User, AuthError> {
        let control = self.control.as_ref().ok_or_else(|| {
            AuthError::Unauthenticated("this server has no console; console sessions are invalid".into())
        })?;
        let revoked = || AuthError::Unauthenticated("session revoked; sign in again".into());
        let user = control.user(&cc.user).await.map_err(unavailable)?.ok_or_else(revoked)?;
        if user.disabled || (cc.iat as i64) < user.sessions_valid_after {
            return Err(revoked());
        }
        Ok(user)
    }

    /// Authenticate a console route: a console session, and its user.
    pub async fn authenticate_console(&self, h: &HeaderMap) -> std::result::Result<(User, ConsoleClaims), AuthError> {
        let tok = presented(h)
            .filter(|t| t.starts_with(SESSION_PREFIX))
            .ok_or_else(|| AuthError::Unauthenticated("console routes need a console session".into()))?;
        let claims = self.verify_session(tok)?;
        let cc = claims.console.ok_or_else(|| AuthError::Forbidden("console routes need a console session".into()))?;
        let user = self.console_user(&cc).await?;
        Ok((user, cc))
    }

    /// Authenticate a request and work out the principal of this call.
    pub async fn authenticate(&self, h: &HeaderMap) -> std::result::Result<Caller, AuthError> {
        let tenant = header(h, TENANT).map(str::to_string);
        let presented = presented(h);
        if let Some(tok) = presented.filter(|t| t.starts_with(SESSION_PREFIX)) {
            let claims = self.verify_session(tok)?;
            if header(h, ACT_AS).is_some() {
                return Err(AuthError::Forbidden("session tokens cannot act for other principals".into()));
            }
            if let Some(cc) = &claims.console {
                return self.console_caller(cc, tenant).await;
            }
            let tenant = match (&claims.tenant, tenant) {
                (Some(bound), Some(asked)) if *bound != asked => {
                    return Err(AuthError::Forbidden(format!("this session is for tenant {bound}, not {asked}")));
                }
                (Some(bound), _) => Some(bound.clone()),
                (None, asked) => asked,
            };
            let session = SessionInfo { exp: claims.exp, jti: claims.jti.clone(), sid: claims.sid.clone() };
            // The session itself, or the login it belongs to.
            for id in session.jti.iter().chain(session.sid.iter().filter(|sid| session.jti.as_ref() != Some(*sid))) {
                if self.session_revoked(id, tenant.as_deref()).await? {
                    return Err(AuthError::Unauthenticated("session revoked".into()));
                }
            }
            let session = Some(session);
            let credential = match &claims.key {
                Some(prefix) => {
                    let control = self
                        .control
                        .as_ref()
                        .ok_or_else(|| AuthError::Unauthenticated("invalid session token".into()))?;
                    let (key, project) = control
                        .active_key(prefix)
                        .await
                        .map_err(unavailable)?
                        .filter(|(_, p)| tenant.as_deref() == Some(p.r#ref.as_str()))
                        .ok_or_else(|| AuthError::Unauthenticated("the API key of this session was revoked".into()))?;
                    Credential::ApiKey { id: key.id, prefix: key.prefix, project: project.r#ref }
                }
                None => Credential::Session,
            };
            return Ok(Caller { principal: claims.principal, tenant, credential, session });
        }
        if let Some(tok) = presented.filter(|t| t.starts_with(KEY_PREFIX)) {
            return self.key_caller(tok, h, tenant).await;
        }
        let token = match presented {
            Some(tok) => Some(
                self.tokens
                    .iter()
                    .find(|t| constant_time_eq(t.secret.as_bytes(), tok.as_bytes()))
                    .ok_or_else(|| AuthError::Unauthenticated("invalid token".into()))?,
            ),
            None => None,
        };
        let (base, impersonate, allowed_roles, tenants, credential) = match (token, &self.dev) {
            (Some(t), _) => (
                t.principal.clone(),
                t.impersonate,
                t.impersonate_roles.clone(),
                t.tenants.clone(),
                Credential::Config { tenants: t.tenants.clone() },
            ),
            (None, Some(dev)) => (dev.clone(), true, Vec::new(), vec!["*".to_string()], Credential::Dev),
            (None, None) => return Err(AuthError::Unauthenticated("missing bearer token".into())),
        };
        let tenant = token_tenant(&base.id, &tenants, tenant)?;
        // In dev mode anything goes; otherwise only the roles the token may grant.
        let limit = self.dev.is_none() || token.is_some();
        let principal = act_for(base, h, impersonate, &allowed_roles, limit)?;
        Ok(Caller { principal, tenant, credential, session: None })
    }

    /// A console session on the data plane: `x-onto-tenant` names a project
    /// and the user's role there gives the principal.
    async fn console_caller(
        &self,
        cc: &ConsoleClaims,
        tenant: Option<String>,
    ) -> std::result::Result<Caller, AuthError> {
        let user = self.console_user(cc).await?;
        let Some(r) = tenant else {
            return Err(AuthError::Forbidden(
                "console sessions name a project with the x-onto-tenant header on data-plane requests".into(),
            ));
        };
        let control = self.control.as_ref().ok_or_else(|| AuthError::Unauthenticated("no console".into()))?;
        let Some((project, role)) = control.membership(&user.id, &r).await.map_err(unavailable)? else {
            return Err(AuthError::Forbidden(format!("user {} is not a member of project {r}", user.id)));
        };
        let principal = Principal::new(format!("user:{}", user.id), role.principal_roles(), vec!["*".into()]);
        Ok(Caller {
            principal,
            tenant: Some(project.r#ref),
            credential: Credential::Console { user: user.id, role, org: project.org_id },
            session: None,
        })
    }

    /// A project API key: its project is the tenant, its scopes the roles.
    async fn key_caller(
        &self,
        tok: &str,
        h: &HeaderMap,
        tenant: Option<String>,
    ) -> std::result::Result<Caller, AuthError> {
        let invalid = || AuthError::Unauthenticated("invalid API key".into());
        let control = self.control.as_ref().ok_or_else(invalid)?;
        let (key, project) = control.authenticate_key(tok).await.map_err(unavailable)?.ok_or_else(invalid)?;
        if let Some(t) = &tenant
            && *t != project.r#ref
        {
            return Err(AuthError::Forbidden(format!(
                "key {} belongs to project {}, not {t}",
                key.prefix, project.r#ref
            )));
        }
        let roles = onto_control::KeyScope::roles(&key.scopes);
        // impersonate_roles ⊆ the key's roles is checked when the key is
        // created; checked again here so an edited row cannot widen it.
        let allowed: Vec<Role> = key.impersonate_roles.iter().filter(|r| roles.contains(r)).cloned().collect();
        let base = Principal::new(format!("key:{}", key.prefix), roles, key.workspaces.clone());
        let principal = act_for(base, h, key.impersonate, &allowed, true)?;
        Ok(Caller {
            principal,
            tenant: Some(project.r#ref.clone()),
            credential: Credential::ApiKey { id: key.id, prefix: key.prefix, project: project.r#ref },
            session: None,
        })
    }

    /// Back-compat helper: the principal only.
    pub async fn principal(&self, h: &HeaderMap) -> std::result::Result<Principal, String> {
        self.authenticate(h).await.map(|c| c.principal).map_err(|e| e.to_string())
    }
}

/// The tenant a `[[auth.tokens]]` request addresses. A token limited to
/// some tenants never reaches the default one by sending no
/// `x-onto-tenant`: with one tenant it addresses that one, with several it
/// must name one of them.
fn token_tenant(id: &str, allowed: &[String], asked: Option<String>) -> std::result::Result<Option<String>, AuthError> {
    let all = allowed.iter().any(|x| x == "*");
    match asked {
        Some(t) if all || allowed.contains(&t) => Ok(Some(t)),
        Some(t) => Err(AuthError::Forbidden(format!("{id} may not address tenant {t}"))),
        None if all => Ok(None),
        None => match allowed {
            [only] => Ok(Some(only.clone())),
            [] => Err(AuthError::Forbidden(format!("{id} may not address any tenant (its tenants list is empty)"))),
            _ => Err(AuthError::Forbidden(format!(
                "{id} may address several tenants ({}): name one with the {TENANT} header",
                allowed.join(", ")
            ))),
        },
    }
}

/// Calls a project credential (API key, console session) may not make,
/// because they would act on the server rather than the project: a
/// connector whose URL is `env:VAR` (it would read the server's
/// environment) or a filesystem path, a repository connector whose git URL
/// fails the SSRF rules (`https://` resolving only to public addresses,
/// unless `allow_private`, which is `console.allow_private_urls`), and a
/// PostgreSQL connector whose URL fails them (a TCP host resolving only to
/// public addresses, no Unix socket, no `host`/`hostaddr`/`port`/`options`
/// or SSL file parameters). A connector such a credential registers is
/// marked `restricted` in `input`, whatever the input said, so every crawl
/// checks its URL again and connects only to the addresses it checked.
/// Operators (config tokens, the CLI) are not limited.
pub async fn guard_project_call(
    credential: &Credential,
    action: &str,
    input: &mut Value,
    allow_private: bool,
) -> Result<()> {
    if !credential.is_project() || action != "connector.add" {
        return Ok(());
    }
    let refuse = |why: &str| {
        Err(Error::Denied(format!(
            "{why}: API keys and console sessions may not register it (operators can, with a config token or the CLI)"
        )))
    };
    if input.get("path").is_some_and(|p| !p.is_null()) {
        return refuse("a connector on a filesystem path reads the server's files");
    }
    let raw = input.get("url").and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let url = raw.to_ascii_lowercase();
    if url.starts_with("env:") {
        return refuse("a connector URL env:VAR reads the server's environment");
    }
    let git_like = onto_intelli::connectors::repo::looks_like_url(&raw);
    if url.starts_with("file:")
        || url.starts_with('/')
        || url.starts_with('.')
        || url.starts_with('~')
        || url.starts_with('\\')
        || (!url.is_empty() && !git_like)
    {
        return refuse("a connector on a filesystem path reads the server's files");
    }
    let checked = match input.get("kind").and_then(Value::as_str) {
        Some("repo") => onto_control::net::check_git_url(&raw, allow_private).await,
        // The default kind; another kind fails validation later.
        Some("postgres") | None if !raw.is_empty() => onto_control::net::check_pg_url(&raw, allow_private).await,
        Some("mysql" | "mariadb") => onto_intelli::connectors::mysql::check_url(&raw, allow_private).await.map(drop),
        Some("sqlserver" | "mssql") => {
            onto_intelli::connectors::sqlserver::check_url(&raw, allow_private).await.map(drop)
        }
        Some("mongodb") => onto_intelli::connectors::mongo::check_url(&raw, allow_private).await.map(drop),
        _ => Ok(()),
    };
    if let Err(e) = checked {
        return refuse(&e.to_string().replace("invalid argument: ", ""));
    }
    if let Value::Object(m) = input {
        m.insert("restricted".into(), Value::Bool(true));
    }
    Ok(())
}

/// A random session id.
/// A session id: random, then the session's expiry ([`id_expiry`]).
fn new_jti(exp: u64) -> String {
    format!("{}.{exp}", onto_control::keys::random_token(16))
}

/// The key of a revocation that holds for the sessions of one tenant only.
/// Session ids never contain `@`.
pub(crate) fn scoped_id(id: &str, tenant: Option<&str>) -> String {
    format!("{id}@{}", tenant.unwrap_or_default())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    onto_control::keys::constant_time_eq(a, b)
}

/// A trimmed, non-empty header value.
pub fn header_value(h: &HeaderMap, name: &str) -> Option<String> {
    header(h, name).map(str::to_string)
}

/// Workspace selected by the caller (`x-onto-workspace` header).
pub fn workspace_header(headers: &HeaderMap) -> Option<String> {
    header(headers, WORKSPACE).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onto_control::{ControlOptions, KeyRequest, KeyScope};
    use onto_intelli::config::{ConsoleConfig, TokenConfig};

    fn config() -> AuthConfig {
        AuthConfig {
            tokens: vec![
                TokenConfig {
                    token: Some("svc-secret-0123456789".into()),
                    token_env: None,
                    principal: Principal::new("nexc", vec![Role::Member], vec!["*".into()]),
                    impersonate: true,
                    impersonate_roles: vec![Role::Viewer, Role::Member, Role::Agent],
                    tenants: vec!["*".into()],
                },
                TokenConfig {
                    token: Some("dash-secret-0123456789".into()),
                    token_env: None,
                    principal: Principal::viewer("dash", "acme"),
                    impersonate: false,
                    impersonate_roles: Vec::new(),
                    tenants: vec!["acme".into()],
                },
            ],
            ..Default::default()
        }
    }

    fn auth() -> Auth {
        Auth::from_config(&config(), false).unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        h
    }

    #[tokio::test]
    async fn tokens_are_required_by_default() {
        assert!(Auth::from_config(&AuthConfig::default(), false).is_err(), "no tokens, no dev flag: refuse to start");
        let none = AuthConfig { mode: "none".into(), ..Default::default() };
        assert!(Auth::from_config(&none, false).err().unwrap().to_string().contains("--dev"));
        let a = auth();
        assert!(matches!(a.authenticate(&HeaderMap::new()).await, Err(AuthError::Unauthenticated(_))));
        assert!(matches!(
            a.authenticate(&headers(&[("authorization", "Bearer nope")])).await,
            Err(AuthError::Unauthenticated(_))
        ));
        let dev = Auth::from_config(&AuthConfig::default(), true).unwrap();
        let c = dev.authenticate(&HeaderMap::new()).await.unwrap();
        assert_eq!((c.principal.id.as_str(), c.credential), ("dev", Credential::Dev));
    }

    #[tokio::test]
    async fn services_act_for_their_users_within_limits() {
        let a = auth();
        let c = a
            .authenticate(&headers(&[
                ("authorization", "Bearer svc-secret-0123456789"),
                (ACT_AS, "user:42"),
                (ACT_ROLES, "member, agent"),
                (ACT_WORKSPACES, "ws1"),
                (ACT_ENTITY, "person:42"),
                (TENANT, "ws_7"),
            ]))
            .await
            .unwrap();
        assert_eq!(c.principal.id, "user:42");
        assert_eq!(c.principal.roles, vec![Role::Member, Role::Agent]);
        assert_eq!(c.principal.workspaces, vec!["ws1"]);
        assert_eq!(c.principal.entity.as_deref(), Some("person:42"));
        assert_eq!(c.principal.via.as_deref(), Some("nexc"));
        assert_eq!(c.tenant.as_deref(), Some("ws_7"));
        assert!(matches!(c.credential, Credential::Config { .. }), "{:?}", c.credential);
        // No escalation beyond impersonate_roles.
        let err = a
            .authenticate(&headers(&[
                ("authorization", "Bearer svc-secret-0123456789"),
                (ACT_AS, "user:42"),
                (ACT_ROLES, "admin"),
            ]))
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Forbidden(ref m) if m.contains("admin")), "{err}");
        // Default role of an impersonated user is viewer.
        let c = a
            .authenticate(&headers(&[("authorization", "Bearer svc-secret-0123456789"), (ACT_AS, "user:9")]))
            .await
            .unwrap();
        assert_eq!(c.principal.roles, vec![Role::Viewer]);
        // Tokens without impersonate cannot, and tenants are limited.
        let dash = [("authorization", "Bearer dash-secret-0123456789")];
        assert!(matches!(a.authenticate(&headers(&[dash[0], (ACT_AS, "user:1")])).await, Err(AuthError::Forbidden(_))));
        assert!(matches!(a.authenticate(&headers(&[dash[0], (TENANT, "other")])).await, Err(AuthError::Forbidden(_))));
        assert_eq!(a.authenticate(&headers(&[dash[0], (TENANT, "acme")])).await.unwrap().principal.id, "dash");
        // Workspaces are limited to the token's; without x-onto-tenant a token
        // limited to one tenant addresses that tenant, never the default one.
        let c = a.authenticate(&headers(&[dash[0]])).await.unwrap();
        assert_eq!(c.principal.workspaces, vec!["acme".to_string()]);
        assert_eq!(c.tenant.as_deref(), Some("acme"));
    }

    #[test]
    fn limited_tokens_never_fall_back_to_the_default_tenant() {
        let two = ["a".to_string(), "b".to_string()];
        let err = token_tenant("svc", &two, None).unwrap_err();
        assert!(matches!(err, AuthError::Forbidden(ref m) if m.contains(TENANT)), "{err}");
        assert_eq!(token_tenant("svc", &two, Some("b".into())).unwrap().as_deref(), Some("b"));
        assert!(token_tenant("svc", &two, Some("c".into())).is_err());
        assert!(token_tenant("svc", &[], None).is_err(), "an empty list allows nothing");
        assert_eq!(token_tenant("svc", &["*".into()], None).unwrap(), None, "all tenants: the default one");
        assert_eq!(token_tenant("svc", &["only".into()], None).unwrap().as_deref(), Some("only"));
    }

    /// A revocation: the id, its tenants (`None`: every one), until when.
    type Revoked = (String, Option<Vec<String>>, u64);

    /// Records what is revoked, and answers from that.
    #[derive(Default)]
    struct Recorded(Mutex<Vec<Revoked>>);

    impl Revocations for Recorded {
        fn revoke<'a>(&'a self, id: &'a str, tenants: Option<&'a [String]>, exp: u64) -> BoxFuture<'a, ()> {
            self.0.lock().push((id.to_string(), tenants.map(<[String]>::to_vec), exp));
            Box::pin(async { Ok(()) })
        }

        fn is_revoked<'a>(&'a self, id: &'a str, tenant: Option<&'a str>) -> BoxFuture<'a, bool> {
            let revoked = self.0.lock().iter().any(|(i, tenants, _)| {
                i == id && tenants.as_ref().is_none_or(|ts| tenant.is_some_and(|t| ts.iter().any(|x| x == t)))
            });
            Box::pin(async move { Ok(revoked) })
        }
    }

    #[tokio::test]
    async fn a_revocation_lasts_as_long_as_its_session_whatever_the_limit_is_now() {
        // One replica minted sessions for up to 12 hours; another one (or the
        // same after a restart) allows one minute now.
        const SECRET: &str = "ONTO_TEST_SESSION_SECRET_REVOCATION_TTL";
        // SAFETY: set before these replicas read it; no other test reads it.
        unsafe { std::env::set_var(SECRET, "a-session-secret-of-at-least-32-characters") };
        let replica = |max_ttl| {
            let mut cfg = config();
            cfg.session_secret_env = Some(SECRET.into());
            cfg.session_max_ttl_secs = max_ttl;
            Auth::from_config(&cfg, false).unwrap()
        };
        let (before, now) = (replica(12 * 3600), replica(60));
        let recorded = Arc::new(Recorded::default());
        now.set_revocations(recorded.clone());
        let p = Principal::viewer("u", "acme");
        let (token, s) = before.issue_session_from(p, Some("acme".into()), 12 * 3600, None);
        let jti = s.jti.clone().unwrap();
        assert_eq!(id_expiry(&jti), Some(s.exp), "the id carries its expiry");
        // The caller's expires_at (in the past here) does not shorten it.
        now.revoke_session(&jti, None, 1).await.unwrap();
        let until = recorded.0.lock()[0].2;
        assert_eq!(until, s.exp + REVOCATION_GRACE_SECS, "until the session expires, not for the current limit");
        let bearer = format!("Bearer {token}");
        let refused = now.authenticate(&headers(&[("authorization", &bearer)])).await;
        assert!(matches!(refused, Err(AuthError::Unauthenticated(_))), "{refused:?}");
    }

    #[tokio::test]
    async fn old_session_tokens_without_console_still_verify() {
        use base64::Engine;
        let a = auth();
        // A token as earlier versions minted it: no `console` member at all.
        let exp = now_secs() + 600;
        let body = format!(
            r#"{{"principal":{{"id":"u1","roles":["viewer"],"workspaces":["*"]}},"tenant":"acme","exp":{exp}}}"#
        );
        let payload =
            format!("{SESSION_PREFIX}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body.as_bytes()));
        let token = format!("{payload}.{}", a.sign(&payload));
        let c = a.authenticate(&headers(&[("authorization", &format!("Bearer {token}"))])).await.unwrap();
        assert_eq!(
            (c.principal.id.as_str(), c.tenant.as_deref(), c.credential),
            ("u1", Some("acme"), Credential::Session)
        );
        let (fresh, _) = a.issue_session(Principal::viewer("u2", "acme"), None, 60);
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(fresh.strip_prefix(SESSION_PREFIX).unwrap().rsplit_once('.').unwrap().0)
            .unwrap();
        assert!(!String::from_utf8(raw).unwrap().contains("console"), "plain sessions carry no console claims");
    }

    async fn with_control() -> (Auth, Arc<Control>, onto_control::SignIn) {
        let opts = ControlOptions { bootstrap_token: Some("b".repeat(40)), ..Default::default() };
        let control = Arc::new(Control::memory(ConsoleConfig::default(), opts).unwrap());
        // No config tokens: the bootstrap token is a sign-in path.
        let a = Auth::from_config_with(&AuthConfig::default(), false, Some(control.clone())).unwrap();
        let s = control.sign_in_local(&["t1".into(), "t2".into()]).await.unwrap();
        (a, control, s)
    }

    #[tokio::test]
    async fn console_sessions_resolve_roles_per_project() {
        let (a, control, s) = with_control().await;
        let (tok, _) = a.issue_console_session(&s.user.id, 3600);
        let bearer = format!("Bearer {tok}");
        let c = a.authenticate(&headers(&[("authorization", &bearer), (TENANT, "t1")])).await.unwrap();
        assert_eq!(c.principal.id, format!("user:{}", s.user.id));
        assert_eq!(c.principal.roles, vec![Role::Admin]);
        assert_eq!(c.tenant.as_deref(), Some("t1"));
        assert_eq!(
            c.credential,
            Credential::Console { user: s.user.id.clone(), role: OrgRole::Owner, org: s.org.id.clone() }
        );
        // A tenant is required on the data plane, and it must be the user's project.
        let err = a.authenticate(&headers(&[("authorization", &bearer)])).await.unwrap_err();
        assert!(matches!(err, AuthError::Forbidden(ref m) if m.contains("x-onto-tenant")), "{err}");
        let err = a.authenticate(&headers(&[("authorization", &bearer), (TENANT, "elsewhere")])).await.unwrap_err();
        assert!(matches!(err, AuthError::Forbidden(_)), "{err}");
        // Console sessions cannot impersonate.
        let err = a
            .authenticate(&headers(&[("authorization", &bearer), (TENANT, "t1"), (ACT_AS, "user:x")]))
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Forbidden(_)));
        // A developer is member + developer; a viewer viewer + observer.
        let dev = control
            .store()
            .upsert_login(onto_control::LoginUpsert {
                provider: "dev".into(),
                subject: "d".into(),
                now: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        control.store().add_member(&s.org.id, &dev.id, OrgRole::Developer).await.unwrap();
        let (dtok, _) = a.issue_console_session(&dev.id, 3600);
        let dh = headers(&[("authorization", &format!("Bearer {dtok}")), (TENANT, "t2")]);
        let c = a.authenticate(&dh).await.unwrap();
        assert_eq!(c.principal.roles, vec![Role::Member, Role::Custom("developer".into())]);
        assert!(c.principal.can_observe());
        control.store().set_role(&s.org.id, &dev.id, OrgRole::Viewer).await.unwrap();
        assert_eq!(a.authenticate(&dh).await.unwrap().principal.roles[0], Role::Member, "cached for 30 s");
        control.evict_org(&s.org.id);
        let c = a.authenticate(&dh).await.unwrap();
        assert_eq!(c.principal.roles, vec![Role::Viewer, Role::Custom("observer".into())]);
        // Removed members are refused once the change is seen.
        control.store().remove_member(&s.org.id, &dev.id).await.unwrap();
        control.evict_org(&s.org.id);
        assert!(matches!(a.authenticate(&dh).await, Err(AuthError::Forbidden(_))));
        // Console routes see the user; plain sessions are refused there.
        let (u, _) = a.authenticate_console(&headers(&[("authorization", &bearer)])).await.unwrap();
        assert_eq!(u.id, s.user.id);
        let (plain, _) = a.issue_session(Principal::admin("x"), None, 60);
        let err = a.authenticate_console(&headers(&[("authorization", &format!("Bearer {plain}"))])).await;
        assert!(matches!(err, Err(AuthError::Forbidden(_))));
        // Sign out everywhere refuses sessions issued before it.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        control.store().sign_out_everywhere(&s.user.id, onto_intelli::core::time::now_ms()).await.unwrap();
        control.evict_user(&s.user.id);
        assert!(matches!(
            a.authenticate(&headers(&[("authorization", &bearer), (TENANT, "t1")])).await,
            Err(AuthError::Unauthenticated(_))
        ));
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let (again, _) = a.issue_console_session(&s.user.id, 3600);
        assert!(
            a.authenticate(&headers(&[("authorization", &format!("Bearer {again}")), (TENANT, "t1")])).await.is_ok()
        );
    }

    #[tokio::test]
    async fn api_keys_map_scopes_and_stay_in_their_project() {
        let (a, control, s) = with_control().await;
        let project = s.adopted.iter().find(|p| p.r#ref == "t1").unwrap().clone();
        let req = KeyRequest {
            name: "svc".into(),
            scopes: vec![KeyScope::Read, KeyScope::Write],
            workspaces: Some(vec!["acme".into()]),
            impersonate: true,
            impersonate_roles: vec![Role::Viewer],
            ..Default::default()
        };
        let (key, secret) = control.create_key(&project, req, &s.user.id).await.unwrap();
        let bearer = format!("Bearer {secret}");
        let c = a.authenticate(&headers(&[("authorization", &bearer)])).await.unwrap();
        assert_eq!(c.principal.id, format!("key:{}", key.prefix));
        assert_eq!(c.principal.roles, vec![Role::Viewer, Role::Member]);
        assert_eq!(c.principal.workspaces, vec!["acme"]);
        assert_eq!(c.tenant.as_deref(), Some("t1"));
        assert_eq!(
            c.credential,
            Credential::ApiKey { id: key.id.clone(), prefix: key.prefix.clone(), project: "t1".into() }
        );
        // x-api-key works the same; another project's tenant is refused.
        assert!(a.authenticate(&headers(&[("x-api-key", &secret), (TENANT, "t1")])).await.is_ok());
        let err = a.authenticate(&headers(&[("authorization", &bearer), (TENANT, "t2")])).await.unwrap_err();
        assert!(matches!(err, AuthError::Forbidden(_)), "{err}");
        // Impersonation within impersonate_roles only.
        let c = a
            .authenticate(&headers(&[("authorization", &bearer), (ACT_AS, "user:7"), (ACT_ROLES, "viewer")]))
            .await
            .unwrap();
        assert_eq!((c.principal.id.as_str(), c.principal.via.clone()), ("user:7", Some(format!("key:{}", key.prefix))));
        let err = a
            .authenticate(&headers(&[("authorization", &bearer), (ACT_AS, "user:7"), (ACT_ROLES, "member")]))
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Forbidden(_)));
        let err = a
            .authenticate(&headers(&[("authorization", &bearer), (ACT_AS, "user:7"), (ACT_WORKSPACES, "other")]))
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::Forbidden(_)));
        // Unknown prefix and wrong secret: the same 401.
        let (_, unknown) = onto_control::keys::generate();
        let wrong = format!("{}_{}", key.prefix, onto_control::keys::random_token(32));
        let e1 = a.authenticate(&headers(&[("authorization", &format!("Bearer {unknown}"))])).await.unwrap_err();
        let e2 = a.authenticate(&headers(&[("authorization", &format!("Bearer {wrong}"))])).await.unwrap_err();
        let e3 = a.authenticate(&headers(&[("authorization", "Bearer onto_sk_malformed")])).await.unwrap_err();
        assert_eq!(e1, AuthError::Unauthenticated("invalid API key".into()));
        assert_eq!(e1, e2);
        assert_eq!(e1, e3);
        // A session the key minted is the key's delegate.
        let (st, _) = a.issue_key_session(Principal::viewer("user:7", "acme"), &key.prefix, "t1", 600);
        let sh = headers(&[("authorization", &format!("Bearer {st}"))]);
        let c = a.authenticate(&sh).await.unwrap();
        assert_eq!((c.tenant.as_deref(), c.credential.is_project()), (Some("t1"), true));
        // Revoked keys are refused at once on this replica, with their sessions.
        control.revoke_key(&project, &key.id, &s.user.id).await.unwrap();
        assert_eq!(a.authenticate(&headers(&[("authorization", &bearer)])).await.unwrap_err(), e1);
        assert!(matches!(a.authenticate(&sh).await, Err(AuthError::Unauthenticated(_))));
        // A key without impersonation cannot act for others; an admin key reads traces.
        let req = KeyRequest { name: "ops".into(), scopes: vec![KeyScope::Admin], ..Default::default() };
        let (_, admin) = control.create_key(&project, req, &s.user.id).await.unwrap();
        let ah = format!("Bearer {admin}");
        assert!(a.authenticate(&headers(&[("authorization", &ah)])).await.unwrap().principal.can_observe());
        assert!(matches!(
            a.authenticate(&headers(&[("authorization", &ah), (ACT_AS, "user:1")])).await,
            Err(AuthError::Forbidden(_))
        ));
        // Without a control plane, keys are just invalid.
        assert_eq!(auth().authenticate(&headers(&[("authorization", &ah)])).await.unwrap_err(), e1);
    }

    #[tokio::test]
    async fn project_connectors_to_other_databases_follow_the_same_ssrf_rules() {
        let key = Credential::ApiKey { id: "k".into(), prefix: "p".into(), project: "t".into() };
        let add = |kind: &str, url: &str| serde_json::json!({ "name": "x", "kind": kind, "url": url });
        for (kind, public, private, param) in [
            ("mysql", "mysql://ro@8.8.8.8/shop", "mysql://ro@127.0.0.1/shop", "mysql://ro@8.8.8.8/shop?socket=/tmp/s"),
            (
                "sqlserver",
                "sqlserver://ro@8.8.8.8/shop",
                "sqlserver://ro@10.0.0.1/shop",
                "sqlserver://ro@8.8.8.8/shop?server=h",
            ),
            (
                "mongodb",
                "mongodb://ro@8.8.8.8/shop",
                "mongodb://169.254.169.254/x",
                "mongodb://8.8.8.8/x?tlsCAFile=/etc/k",
            ),
        ] {
            let mut input = add(kind, public);
            guard_project_call(&key, "connector.add", &mut input, false).await.unwrap();
            assert_eq!(input["restricted"], true, "{kind}");
            for bad in [private, param, "env:DB_URL"] {
                let mut input = add(kind, bad);
                let e = guard_project_call(&key, "connector.add", &mut input, false).await.unwrap_err();
                assert!(matches!(e, Error::Denied(_)), "{kind} {bad}: {e}");
            }
            // Operators are not limited.
            let mut input = add(kind, private);
            assert!(guard_project_call(&Credential::Config, "connector.add", &mut input, false).await.is_ok());
        }
    }

    #[tokio::test]
    async fn project_credentials_cannot_reach_the_server() {
        let key = Credential::ApiKey { id: "k".into(), prefix: "p".into(), project: "t".into() };
        let console = Credential::Console { user: "u".into(), role: OrgRole::Owner, org: "o".into() };
        let guard = |c: &Credential, action: &str, mut input: Value, allow_private: bool| {
            let (c, action) = (c.clone(), action.to_string());
            async move { guard_project_call(&c, &action, &mut input, allow_private).await.map(|_| input) }
        };
        for c in [&key, &console] {
            // A PostgreSQL URL to a public address is accepted, and marked restricted.
            let pg = serde_json::json!({ "name": "wh", "url": "postgres://ro@8.8.8.8/wh", "restricted": false });
            let marked = guard(c, "connector.add", pg, false).await.unwrap();
            assert_eq!(marked["restricted"], true);
            for url in ["env:DATABASE_URL", " ENV:X", "/var/run/postgresql", "./repo", "file:///etc", "~/x", "repo"] {
                let input = serde_json::json!({ "name": "x", "url": url });
                assert!(guard(c, "connector.add", input, false).await.is_err(), "{url}");
                let input = serde_json::json!({ "name": "x", "kind": "repo", "url": url });
                assert!(guard(c, "connector.add", input, true).await.is_err(), "{url}");
            }
            let path = serde_json::json!({ "path": "/srv/repo" });
            assert!(guard(c, "connector.add", path, false).await.is_err());
            let env = serde_json::json!({ "url": "env:X" });
            assert!(guard(c, "entity.get", env, false).await.is_ok());
            // PostgreSQL URLs: a TCP host with public addresses only, no
            // socket, no parameter that moves the connection or reads a file.
            let pg = |url: &str| serde_json::json!({ "name": "p", "kind": "postgres", "url": url });
            for url in [
                "postgres://onto:onto@127.0.0.1:5441/onto",
                "postgres://u@10.0.0.7/db",
                "postgres://u@localhost/db",
                "postgres://u@[::1]/db",
            ] {
                let e = guard(c, "connector.add", pg(url), false).await.unwrap_err();
                assert!(matches!(e, Error::Denied(_)), "{url}: {e}");
                assert!(guard(c, "connector.add", pg(url), true).await.is_ok(), "{url} with allow_private");
            }
            for url in [
                "postgresql:///onto?host=/var/run/postgresql",
                "postgres://u@%2Fvar%2Frun%2Fpostgresql/onto",
                "postgres://u@8.8.8.8/db?host=127.0.0.1",
                "postgres://u@8.8.8.8/db?hostaddr=127.0.0.1",
                "postgres://u@8.8.8.8/db?port=5441",
                "postgres://u@8.8.8.8/db?sslrootcert=/etc/ssl/key.pem",
                "postgres://u@8.8.8.8/db?sslkey=/home/onto/.postgresql/postgresql.key",
                "postgres://u@8.8.8.8/db?options=-csearch_path%3Donto_control",
                "postgres://8.8.8.8/db",
                "mysql://u@8.8.8.8/db",
            ] {
                for allow_private in [false, true] {
                    let e = guard(c, "connector.add", pg(url), allow_private).await.unwrap_err();
                    assert!(matches!(e, Error::Denied(_)), "{url}: {e}");
                }
            }
            assert!(
                guard(c, "connector.add", pg("postgres://u:pw@8.8.8.8:6432/db?sslmode=require"), false).await.is_ok()
            );
            // Git URLs: https to public addresses only, unless private URLs are allowed.
            let repo = |url: &str| serde_json::json!({ "name": "r", "kind": "repo", "url": url });
            let marked = guard(c, "connector.add", repo("https://8.8.8.8/acme/platform"), false).await.unwrap();
            assert_eq!(marked["restricted"], true);
            for url in
                ["https://127.0.0.1/acme/platform", "https://10.0.0.7/r", "git@github.com:acme/platform.git", "ext::sh"]
            {
                let e = guard(c, "connector.add", repo(url), false).await.unwrap_err();
                assert!(matches!(e, Error::Denied(_)), "{url}: {e}");
            }
            for url in ["https://127.0.0.1/acme/platform", "git@git.internal:acme/platform.git"] {
                assert!(guard(c, "connector.add", repo(url), true).await.is_ok(), "{url}");
            }
        }
        let env = serde_json::json!({ "name": "x", "url": "env:DATABASE_URL" });
        for c in [Credential::Config { tenants: vec!["*".into()] }, Credential::Dev, Credential::Session] {
            let out = guard(&c, "connector.add", env.clone(), false).await.expect("operators are not limited");
            assert!(out.get("restricted").is_none(), "operator connectors are not restricted");
            let local = serde_json::json!({ "name": "x", "kind": "repo", "url": "https://127.0.0.1/r" });
            assert!(guard(&c, "connector.add", local, false).await.is_ok());
            let pg = serde_json::json!({ "name": "x", "url": "postgres://onto@127.0.0.1:5441/onto" });
            assert!(guard(&c, "connector.add", pg, false).await.is_ok());
        }
    }
}
