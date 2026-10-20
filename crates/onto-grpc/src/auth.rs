//! Authentication shared by gRPC, HTTP and the web UI.
//!
//! * `auth.mode = "token"` (default): every caller presents
//!   `authorization: Bearer <token>` (or `x-api-key`). A missing or unknown
//!   token is rejected; there is no anonymous principal.
//! * `auth.mode = "dev"` (or `onto serve --dev`): no authentication, every
//!   caller is `auth.dev_principal`. Refused unless the server listens on a
//!   loopback address.
//! * A token with `impersonate = true` belongs to a trusted service that acts
//!   for its own users: `x-onto-act-as`, `x-onto-act-roles`,
//!   `x-onto-act-workspaces` and `x-onto-act-entity` name each call's
//!   principal, limited to `impersonate_roles` and the token's workspaces.
//! * `x-onto-tenant` picks the tenant (multi-tenant servers), limited to the
//!   token's `tenants`.

use http::HeaderMap;

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
}

/// Why a request was refused: 401 (no or bad credentials) or 403 (not allowed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    Unauthenticated(String),
    Forbidden(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Unauthenticated(m) | AuthError::Forbidden(m) => f.write_str(m),
        }
    }
}

/// An authenticated caller: the principal of this call and the tenant it addresses.
#[derive(Clone, Debug)]
pub struct Caller {
    pub principal: Principal,
    pub tenant: Option<String>,
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty())
}

fn list(v: &str) -> Vec<String> {
    v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

impl Auth {
    /// `dev` forces development mode (`onto serve --dev`).
    pub fn from_config(cfg: &AuthConfig, dev: bool) -> Result<Self> {
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
        if dev.is_none() && tokens.is_empty() {
            return Err(Error::Config(
                "no auth token resolved: add [[auth.tokens]] with token_env set (e.g. ONTO_TOKEN), \
                 or run `onto serve --dev` for local development"
                    .into(),
            ));
        }
        Ok(Self { dev, tokens })
    }

    pub fn dev_mode(&self) -> bool {
        self.dev.is_some()
    }

    /// Authenticate a request and work out the principal of this call.
    pub fn authenticate(&self, h: &HeaderMap) -> std::result::Result<Caller, AuthError> {
        let tenant = header(h, TENANT).map(str::to_string);
        let presented = header(h, "authorization")
            .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
            .or_else(|| header(h, "x-api-key"))
            .map(str::trim);
        let token = match presented {
            Some(tok) => Some(
                self.tokens
                    .iter()
                    .find(|t| constant_time_eq(t.secret.as_bytes(), tok.as_bytes()))
                    .ok_or_else(|| AuthError::Unauthenticated("invalid token".into()))?,
            ),
            None => None,
        };
        let (base, impersonate, allowed_roles, tenants) = match (token, &self.dev) {
            (Some(t), _) => (t.principal.clone(), t.impersonate, t.impersonate_roles.clone(), t.tenants.clone()),
            (None, Some(dev)) => (dev.clone(), true, Vec::new(), vec!["*".to_string()]),
            (None, None) => return Err(AuthError::Unauthenticated("missing bearer token".into())),
        };
        if let Some(t) = &tenant
            && !tenants.iter().any(|x| x == "*" || x == t)
        {
            return Err(AuthError::Forbidden(format!("{} may not address tenant {t}", base.id)));
        }
        let Some(act_as) = header(h, ACT_AS) else {
            return Ok(Caller { principal: base, tenant });
        };
        if !impersonate {
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
        // In dev mode anything goes; otherwise only the roles the token may grant.
        if self.dev.is_none() || token.is_some() {
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
        Ok(Caller { principal: p, tenant })
    }

    /// Back-compat helper: the principal only.
    pub fn principal(&self, h: &HeaderMap) -> std::result::Result<Principal, String> {
        self.authenticate(h).map(|c| c.principal).map_err(|e| e.to_string())
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Workspace selected by the caller (`x-onto-workspace` header).
pub fn workspace_header(headers: &HeaderMap) -> Option<String> {
    header(headers, WORKSPACE).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onto_intelli::config::TokenConfig;

    fn auth() -> Auth {
        let cfg = AuthConfig {
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
        };
        Auth::from_config(&cfg, false).unwrap()
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        h
    }

    #[test]
    fn tokens_are_required_by_default() {
        assert!(Auth::from_config(&AuthConfig::default(), false).is_err(), "no tokens, no dev flag: refuse to start");
        let none = AuthConfig { mode: "none".into(), ..Default::default() };
        assert!(Auth::from_config(&none, false).err().unwrap().to_string().contains("--dev"));
        let a = auth();
        assert!(matches!(a.authenticate(&HeaderMap::new()), Err(AuthError::Unauthenticated(_))));
        assert!(matches!(
            a.authenticate(&headers(&[("authorization", "Bearer nope")])),
            Err(AuthError::Unauthenticated(_))
        ));
        let dev = Auth::from_config(&AuthConfig::default(), true).unwrap();
        assert_eq!(dev.authenticate(&HeaderMap::new()).unwrap().principal.id, "dev");
    }

    #[test]
    fn services_act_for_their_users_within_limits() {
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
            .unwrap();
        assert_eq!(c.principal.id, "user:42");
        assert_eq!(c.principal.roles, vec![Role::Member, Role::Agent]);
        assert_eq!(c.principal.workspaces, vec!["ws1"]);
        assert_eq!(c.principal.entity.as_deref(), Some("person:42"));
        assert_eq!(c.principal.via.as_deref(), Some("nexc"));
        assert_eq!(c.tenant.as_deref(), Some("ws_7"));
        // No escalation beyond impersonate_roles.
        let err = a
            .authenticate(&headers(&[
                ("authorization", "Bearer svc-secret-0123456789"),
                (ACT_AS, "user:42"),
                (ACT_ROLES, "admin"),
            ]))
            .unwrap_err();
        assert!(matches!(err, AuthError::Forbidden(ref m) if m.contains("admin")), "{err}");
        // Default role of an impersonated user is viewer.
        let c =
            a.authenticate(&headers(&[("authorization", "Bearer svc-secret-0123456789"), (ACT_AS, "user:9")])).unwrap();
        assert_eq!(c.principal.roles, vec![Role::Viewer]);
        // Tokens without impersonate cannot, and tenants are limited.
        let dash = [("authorization", "Bearer dash-secret-0123456789")];
        assert!(matches!(a.authenticate(&headers(&[dash[0], (ACT_AS, "user:1")])), Err(AuthError::Forbidden(_))));
        assert!(matches!(a.authenticate(&headers(&[dash[0], (TENANT, "other")])), Err(AuthError::Forbidden(_))));
        assert_eq!(a.authenticate(&headers(&[dash[0], (TENANT, "acme")])).unwrap().principal.id, "dash");
        // Workspaces are limited to the token's.
        assert!(matches!(
            a.authenticate(&headers(&[dash[0]])).map(|c| c.principal.workspaces),
            Ok(ref w) if w == &vec!["acme".to_string()]
        ));
    }
}
