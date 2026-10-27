//! Console sign-in through OpenID Connect (docs/CONTROL_PLANE.md, sections
//! 7.2 and 7.4): the pending login and its redirect to the IdP, the
//! callback (code exchange, ID token, users, invitations, a first
//! organization) and the one-time ticket the console tab exchanges for a
//! session.
//!
//! The pieces that tie a sign-in together, all random 32 bytes:
//!
//! * `state`, stored hashed with the pending login and consumed once;
//! * `nonce`, which the ID token must carry back;
//! * the PKCE verifier, which never leaves the server;
//! * the browser's binding: `start` receives `BASE64URL(SHA-256(binding))`,
//!   `exchange` the binding itself (base64url), so a ticket only works in
//!   the tab that started the sign-in;
//! * the ticket, stored hashed, single use, 60 seconds.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::json;

use onto_core::time::now_ms;
use onto_core::{Error, Result};
use onto_intelli::config::ConsoleConfig;

use crate::Control;
use crate::keys::{b64, constant_time_eq, random_alnum, random_token, sha256, sha256_hex};
use crate::model::*;
use crate::oidc::{IdClaims, Provider};

/// A pending login is valid this long.
pub const LOGIN_TTL_MS: i64 = 10 * 60 * 1000;
/// A ticket is valid this long.
pub const TICKET_TTL_MS: i64 = 60 * 1000;
/// Sign-in starts per minute from one client (an IPv4 address or an IPv6
/// /64) on one server.
pub const STARTS_PER_MINUTE: usize = 20;
/// Sign-in starts per minute from all clients on one server: bounds the
/// pending logins stored.
pub const STARTS_PER_MINUTE_TOTAL: usize = 600;
/// Clients tracked at once; past this, only the total limit applies to new ones.
const MAX_CLIENTS: usize = 10_000;
/// The first project of a new user's organization.
pub const FIRST_PROJECT: &str = "My first project";

/// Creates a new project's tenant storage without opening it (the console
/// passes `hub.provision(ref)`).
pub type OpenTenant<'a> = &'a (dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync);

/// The query of `GET /v1/console/auth/oidc/{id}/start`.
#[derive(Clone, Debug, Default)]
pub struct StartRequest {
    /// `BASE64URL(SHA-256(binding))` of the browser's random binding.
    pub binding: String,
    /// The console route to return to; must start with `#/` (default `#/`).
    pub return_to: Option<String>,
    /// An invitation code from an invitation link.
    pub invite: Option<String>,
}

/// Why a callback did not sign anyone in: `code` goes to the console as
/// `?error=`, `message` as `error_description`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

fn refuse(code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal { code, message: message.into() }
}

impl From<Error> for Refusal {
    fn from(e: Error) -> Self {
        tracing::error!("OIDC sign-in: {e}");
        refuse("server_error", "the server could not complete the sign-in")
    }
}

/// The providers of `[[console.oidc]]`, checked: valid entries, unique ids,
/// and a resolved secret for every provider that names `client_secret_env`.
pub(crate) fn providers(cfg: &ConsoleConfig, secrets: &HashMap<String, String>) -> Result<Vec<Arc<Provider>>> {
    let mut out: Vec<Arc<Provider>> = Vec::new();
    for p in &cfg.oidc {
        if out.iter().any(|o| o.id() == p.id) {
            return Err(Error::Config(format!("console.oidc id '{}' is used twice", p.id)));
        }
        let secret = match p.client_secret_env.as_deref().filter(|v| !v.trim().is_empty()) {
            Some(var) => Some(secrets.get(&p.id).filter(|s| !s.is_empty()).cloned().ok_or_else(|| {
                Error::Config(format!(
                    "console.oidc '{}': the client secret variable {var} is not set (omit client_secret_env for a \
                     public client)",
                    p.id
                ))
            })?),
            None => None,
        };
        out.push(Arc::new(Provider::new(p.clone(), secret)?));
    }
    Ok(out)
}

/// Sign-in starts in the last minute, per client and in all.
#[derive(Debug, Default)]
pub struct StartLimiter {
    total: VecDeque<Instant>,
    clients: HashMap<Option<IpAddr>, VecDeque<Instant>>,
}

/// The client an address stands for: an IPv6 address by its /64 (one
/// host usually has a whole /64), an IPv4-mapped one by its IPv4 address.
fn client_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
        v4 => v4,
    }
}

impl StartLimiter {
    pub fn allow(&mut self, client: Option<IpAddr>, now: Instant) -> bool {
        let fresh = |t: &Instant| now.duration_since(*t) < Duration::from_secs(60);
        while self.total.front().is_some_and(|t| !fresh(t)) {
            self.total.pop_front();
        }
        if self.total.len() >= STARTS_PER_MINUTE_TOTAL {
            return false;
        }
        let key = client.map(client_key);
        if !self.clients.contains_key(&key) && self.clients.len() >= MAX_CLIENTS {
            self.clients.retain(|_, v| v.back().is_some_and(fresh));
        }
        if self.clients.contains_key(&key) || self.clients.len() < MAX_CLIENTS {
            let v = self.clients.entry(key).or_default();
            while v.front().is_some_and(|t| !fresh(t)) {
                v.pop_front();
            }
            if v.len() >= STARTS_PER_MINUTE {
                return false;
            }
            v.push_back(now);
        }
        self.total.push_back(now);
        true
    }
}

/// `binding` is base64url of exactly `n` bytes.
fn b64_of_len(v: &str, n: usize) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(v).ok().filter(|b| b.len() == n)
}

/// A console route: starts with `#/`, at most 512 printable characters.
pub fn valid_return_to(r: &str) -> bool {
    r.starts_with("#/") && r.len() <= 512 && !r.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// An IdP error code reduced to `[a-z0-9_]`, at most 64 characters.
fn error_code(s: &str) -> String {
    let c: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(64).collect();
    if c.is_empty() { "idp_error".into() } else { c.to_ascii_lowercase() }
}

fn clip(s: &str, n: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(n).collect()
}

/// The domain of an email address, lowercase.
fn email_domain(email: &str) -> Option<String> {
    email.rsplit_once('@').map(|(_, d)| d.trim().to_ascii_lowercase()).filter(|d| !d.is_empty())
}

impl Control {
    /// `(id, name)` of each OIDC provider, in configuration order.
    pub fn oidc_providers(&self) -> Vec<(String, String)> {
        self.oidc.iter().map(|p| (p.id().to_string(), p.name().to_string())).collect()
    }

    pub fn oidc_provider(&self, id: &str) -> Option<&Arc<Provider>> {
        self.oidc.iter().find(|p| p.id() == id)
    }

    /// Room for one more sign-in start this minute from `client` (the
    /// caller's address; `None` when unknown, which all such callers share):
    /// [`STARTS_PER_MINUTE`] per client, [`STARTS_PER_MINUTE_TOTAL`] in all.
    pub fn allow_oidc_start(&self, client: Option<IpAddr>) -> bool {
        self.oidc_starts.lock().allow(client, Instant::now())
    }

    fn public_base(&self) -> &str {
        self.config.public_url.trim_end_matches('/')
    }

    /// Start a sign-in: store the pending login (state hashed, 10 minutes)
    /// and return the IdP's authorization URL. Discovery is fetched here the
    /// first time; an unreachable IdP is `Unavailable` with the reason.
    pub async fn oidc_start(&self, id: &str, req: StartRequest) -> Result<String> {
        let p = self.oidc_provider(id).ok_or_else(|| Error::not_found(format!("sign-in provider '{id}'")))?.clone();
        if b64_of_len(&req.binding, 32).is_none() {
            return Err(Error::invalid("binding must be BASE64URL(SHA-256(binding)): 43 characters, no padding"));
        }
        let return_to = req.return_to.filter(|r| !r.is_empty()).unwrap_or_else(|| "#/".into());
        if !valid_return_to(&return_to) {
            return Err(Error::invalid("return_to must be a console route starting with #/"));
        }
        let invite = req.invite.filter(|c| !c.is_empty());
        if invite.as_ref().is_some_and(|c| c.len() > 256) {
            return Err(Error::invalid("invite is not an invitation code"));
        }
        let d = p.discovery().await.map_err(|e| {
            Error::Unavailable(format!("identity provider '{id}' ({}) cannot be reached: {e}", p.config().issuer))
        })?;
        let (state, nonce, verifier) = (random_token(32), random_token(32), random_token(32));
        let now = now_ms();
        self.store
            .put_login(PendingLogin {
                state_hash: sha256_hex(state.as_bytes()),
                provider: id.to_string(),
                nonce: nonce.clone(),
                code_verifier: verifier.clone(),
                binding_hash: req.binding,
                invite_code_hash: invite.map(|c| sha256_hex(c.as_bytes())),
                return_to,
                created_at: now,
                expires_at: now + LOGIN_TTL_MS,
            })
            .await?;
        p.authorization_url(&d, &self.config.public_url, &state, &nonce, &verifier)
    }

    /// The IdP's redirect back: always the console URL to send the browser
    /// to, `{public_url}/ui/#/auth/callback?ticket=…&return_to=…` on success
    /// or `…?error=…&error_description=…`.
    pub async fn oidc_callback(&self, q: &HashMap<String, String>, open: OpenTenant<'_>) -> String {
        let base = self.public_base().to_string();
        let fail = |r: Refusal| {
            format!("{base}/ui/#/auth/callback?error={}&error_description={}", enc(r.code), enc(&clip(&r.message, 200)))
        };
        let Some(state) = q.get("state").filter(|s| !s.is_empty() && s.len() <= 512) else {
            return fail(refuse("invalid_state", "the sign-in has no state; start again"));
        };
        let login = match self.store.take_login(&sha256_hex(state.as_bytes()), now_ms()).await {
            Ok(Some(l)) => l,
            Ok(None) => return fail(refuse("invalid_state", "this sign-in expired or was already used; start again")),
            Err(e) => return fail(e.into()),
        };
        // The IdP refused (the user cancelled, consent denied, ...): the state is consumed.
        if let Some(err) = q.get("error") {
            let code = error_code(err);
            let desc = q.get("error_description").map(|d| clip(d, 200)).unwrap_or_else(|| code.clone());
            tracing::info!(provider = %login.provider, error = %code, "OIDC sign-in refused by the IdP");
            return format!("{base}/ui/#/auth/callback?error={}&error_description={}", enc(&code), enc(&desc));
        }
        match self.complete(&login, q.get("code").map(String::as_str), open).await {
            Ok(user) => {
                let ticket = random_token(32);
                let put = self
                    .store
                    .put_ticket(LoginTicket {
                        ticket_hash: sha256_hex(ticket.as_bytes()),
                        user_id: user.id.clone(),
                        binding_hash: login.binding_hash.clone(),
                        expires_at: now_ms() + TICKET_TTL_MS,
                    })
                    .await;
                match put {
                    Ok(()) => format!("{base}/ui/#/auth/callback?ticket={ticket}&return_to={}", enc(&login.return_to)),
                    Err(e) => fail(e.into()),
                }
            }
            Err(r) => {
                tracing::warn!(provider = %login.provider, code = r.code, "OIDC sign-in refused: {}", r.message);
                if r.code != "server_error" {
                    let audited = self
                        .audit(AuditEntry::new("system", "auth.signin_refused").detail(json!({
                            "provider": login.provider, "reason": r.code,
                        })))
                        .await;
                    if let Err(e) = audited {
                        return fail(e.into());
                    }
                }
                fail(r)
            }
        }
    }

    /// Exchange the code, verify the ID token (with userinfo when it has no
    /// email) and sign the identity in.
    async fn complete(
        &self,
        login: &PendingLogin,
        code: Option<&str>,
        open: OpenTenant<'_>,
    ) -> std::result::Result<User, Refusal> {
        let p = self
            .oidc_provider(&login.provider)
            .ok_or_else(|| refuse("invalid_state", format!("sign-in provider '{}' is gone", login.provider)))?
            .clone();
        let code = code
            .filter(|c| !c.is_empty() && c.len() <= 4096)
            .ok_or_else(|| refuse("invalid_request", "the IdP sent no authorization code"))?;
        let unavailable = |e: Error| match e {
            Error::Unavailable(m) => refuse("idp_unavailable", m),
            other => refuse("token_rejected", other.to_string()),
        };
        let d = p.discovery().await.map_err(unavailable)?;
        let tokens =
            p.exchange_code(&d, &self.config.public_url, code, &login.code_verifier).await.map_err(unavailable)?;
        let now = now_ms() / 1000;
        let mut claims = p
            .verify_id_token(&d, &tokens.id_token, &login.nonce, now)
            .await
            .map_err(unavailable)?
            .map_err(|e| refuse("token_rejected", e.to_string()))?;
        if claims.email.is_none()
            && let Some(at) = tokens.access_token.as_deref()
            && let Some(info) = p.userinfo(&d, at, &claims.sub).await.map_err(unavailable)?
        {
            claims.merge_profile(&info);
        }
        self.sign_in_identity(p.id(), &claims, login.invite_code_hash.as_deref(), open).await
    }

    /// Section 7.4: find or create the user of `(provider, sub)`, apply the
    /// sign-up rules to new identities, accept invitations, and give a user
    /// without any membership an organization and a first project when
    /// `create_org_on_signup`.
    pub async fn sign_in_identity(
        &self,
        provider: &str,
        claims: &IdClaims,
        invite_code_hash: Option<&str>,
        open: OpenTenant<'_>,
    ) -> std::result::Result<User, Refusal> {
        let verified_email = claims.email.as_deref().filter(|_| claims.email_verified).map(str::trim);
        let existing = self.store.identity(provider, &claims.sub).await?;
        if existing.is_none() {
            let cfg = &self.config;
            if !cfg.allow_signup && !self.store.invitation_pending(verified_email, invite_code_hash).await? {
                return Err(refuse("signup_closed", "sign-up is closed: ask an administrator for an invitation"));
            }
            if !cfg.allowed_email_domains.is_empty() {
                let allowed = verified_email.and_then(email_domain).is_some_and(|d| {
                    cfg.allowed_email_domains.iter().any(|a| a.trim().trim_start_matches('@').eq_ignore_ascii_case(&d))
                });
                if !allowed {
                    let why = if verified_email.is_none() {
                        "sign-up needs a verified email address, and the IdP did not verify this one"
                    } else {
                        "sign-up is limited to email addresses of allowed domains"
                    };
                    return Err(refuse("domain_not_allowed", why));
                }
            }
        }
        let now = now_ms();
        let user = self
            .store
            .upsert_login(LoginUpsert {
                provider: provider.to_string(),
                subject: claims.sub.clone(),
                email: claims.email.clone(),
                email_verified: claims.email_verified,
                name: claims.name.clone().or_else(|| claims.preferred_username.clone()).unwrap_or_default(),
                avatar_url: claims.picture.clone(),
                claims: claims.profile(),
                now,
            })
            .await?;
        if user.disabled {
            return Err(refuse("user_disabled", "this account is disabled"));
        }
        let new_user = existing.is_none() && user.created_at == now;
        let joined = self.store.accept_invitations(&user, invite_code_hash).await?;
        for org in &joined {
            self.audit(
                AuditEntry::new(&user.id, "invitation.accept")
                    .org(org)
                    .target(&user.id)
                    .detail(json!({ "provider": provider })),
            )
            .await?;
        }
        let mut orgs = self.store.orgs_of(&user.id).await?;
        if orgs.is_empty() && self.config.create_org_on_signup {
            let org = self.first_org(&user, open).await?;
            orgs.push((org, OrgRole::Owner));
        }
        self.evict_user(&user.id);
        let detail = json!({ "provider": provider, "new_user": new_user, "invitations": joined.len() });
        if orgs.is_empty() {
            self.audit(AuditEntry::new(&user.id, "auth.signin").target(&user.id).detail(detail.clone())).await?;
        }
        for (org, _) in &orgs {
            self.audit(AuditEntry::new(&user.id, "auth.signin").org(&org.id).target(&user.id).detail(detail.clone()))
                .await?;
        }
        Ok(user)
    }

    /// `"<name>'s team"` (slug from the email's local part and 4 random
    /// characters) with the user as owner, and an empty first project whose
    /// tenant storage is created at once (it opens on first use). A project
    /// whose storage cannot be created is marked deleted; the sign-in goes on.
    async fn first_org(&self, user: &User, open: OpenTenant<'_>) -> Result<Org> {
        let local = user.email.as_deref().and_then(|e| e.split('@').next()).filter(|l| !l.is_empty());
        let display = Some(user.name.trim()).filter(|n| !n.is_empty()).or(local);
        let name = match display {
            Some(n) => check_name("organization", &format!("{}'s team", n.chars().take(80).collect::<String>()))?,
            None => "My team".to_string(),
        };
        let base = slugify(local.unwrap_or(&name));
        let mut org = None;
        for _ in 0..5 {
            match self.store.create_org(&name, &format!("{base}-{}", random_alnum(4)), &user.id).await {
                Ok(o) => {
                    org = Some(o);
                    break;
                }
                Err(Error::Conflict(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        let org = org.ok_or_else(|| Error::Conflict("could not pick a unique organization slug".into()))?;
        self.audit(
            AuditEntry::new(&user.id, "org.create")
                .org(&org.id)
                .target(&org.id)
                .detail(json!({ "name": org.name, "slug": org.slug, "signup": true })),
        )
        .await?;
        let project = self.store.create_project(&org.id, FIRST_PROJECT, &user.id).await?;
        match open(project.r#ref.clone()).await {
            Ok(()) => {
                self.audit(
                    AuditEntry::new(&user.id, "project.create")
                        .project(&project)
                        .target(&project.r#ref)
                        .detail(json!({ "name": project.name, "signup": true })),
                )
                .await?;
            }
            Err(e) => {
                tracing::error!(project = %project.r#ref, "opening the first project of a new user: {e}");
                if let Err(e2) = self.store.delete_project(&project.r#ref, &user.id).await {
                    tracing::error!(project = %project.r#ref, "marking it deleted: {e2}");
                }
            }
        }
        Ok(org)
    }

    /// `POST /v1/console/auth/exchange`: the ticket (single use) and the
    /// binding of the tab that started the sign-in; `Denied` when either is
    /// wrong.
    pub async fn exchange_ticket(&self, ticket: &str, binding: &str) -> Result<User> {
        let t = self
            .store
            .take_ticket(&sha256_hex(ticket.as_bytes()), now_ms())
            .await?
            .ok_or_else(|| Error::Denied("the sign-in ticket is unknown, used or expired; sign in again".into()))?;
        let presented = b64_of_len(binding, 32).map(|raw| b64(&sha256(&raw))).unwrap_or_default();
        if !constant_time_eq(presented.as_bytes(), t.binding_hash.as_bytes()) {
            return Err(Error::Denied("the sign-in ticket belongs to another browser tab".into()));
        }
        let user = self
            .store
            .user(&t.user_id)
            .await?
            .filter(|u| !u.disabled)
            .ok_or_else(|| Error::Denied("this account is disabled".into()))?;
        self.evict_user(&user.id);
        Ok(user)
    }
}

/// Axum handlers need `Send` futures: this fails to compile when the
/// callback's future stops being `Send`.
#[allow(dead_code)]
fn callback_future_is_send(c: &Control, q: &HashMap<String, String>, open: OpenTenant<'_>) {
    fn s<T: Send>(_: T) {}
    s(c.oidc_callback(q, open));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_rules() {
        assert!(valid_return_to("#/"));
        assert!(valid_return_to("#/projects/abc?tab=keys"));
        for bad in ["", "/", "#", "#x", "https://evil.example", "//evil.example", "#/a b", "#/\n"] {
            assert!(!valid_return_to(bad), "{bad:?}");
        }
        assert_eq!(error_code("access_denied"), "access_denied");
        assert_eq!(error_code("<script>"), "script");
        assert_eq!(error_code("!!"), "idp_error");
        assert_eq!(email_domain("A@Example.COM").as_deref(), Some("example.com"));
        assert!(b64_of_len(&b64(&[0u8; 32]), 32).is_some());
        assert!(b64_of_len(&b64(&[0u8; 31]), 32).is_none());
    }

    #[test]
    fn one_client_cannot_use_up_sign_in_starts() {
        let mut l = StartLimiter::default();
        let now = Instant::now();
        let attacker: IpAddr = "203.0.113.9".parse().unwrap();
        for _ in 0..STARTS_PER_MINUTE {
            assert!(l.allow(Some(attacker), now));
        }
        assert!(!l.allow(Some(attacker), now), "the client is limited");
        assert!(l.allow(Some("198.51.100.4".parse().unwrap()), now), "others still sign in");
        assert!(l.allow(None, now), "so do callers of unknown address");
        // One IPv6 /64 is one client; an IPv4-mapped address is its IPv4 address.
        let v6 = |i: u16| Some(IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, i)));
        for i in 0..STARTS_PER_MINUTE as u16 {
            assert!(l.allow(v6(i + 1), now));
        }
        assert!(!l.allow(v6(999), now));
        assert!(!l.allow(Some("::ffff:203.0.113.9".parse().unwrap()), now));
        // A minute later the client may start again.
        assert!(l.allow(Some(attacker), now + Duration::from_secs(61)));
        // The total bounds all clients together.
        let mut l = StartLimiter::default();
        for i in 0..STARTS_PER_MINUTE_TOTAL {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000 + i as u32));
            assert!(l.allow(Some(ip), now));
        }
        assert!(!l.allow(Some("192.0.2.1".parse().unwrap()), now));
    }
}
