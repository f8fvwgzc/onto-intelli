//! Bearer-token authentication shared by gRPC and HTTP.

use http::HeaderMap;

use onto_intelli::config::AuthConfig;
use onto_intelli::{Error, Principal, Result};

pub struct Auth {
    required: bool,
    default: Principal,
    tokens: Vec<(String, Principal)>,
}

impl Auth {
    pub fn from_config(cfg: &AuthConfig) -> Result<Self> {
        let required = match cfg.mode.as_str() {
            "none" | "" => false,
            "token" => true,
            other => return Err(Error::Config(format!("unknown auth.mode '{other}' (none, token)"))),
        };
        let mut tokens = Vec::new();
        for t in &cfg.tokens {
            let value = match (&t.token_env, &t.token) {
                (Some(var), _) => std::env::var(var).ok(),
                (None, Some(tok)) => Some(tok.clone()),
                (None, None) => None,
            };
            match value.filter(|v| !v.trim().is_empty()) {
                Some(v) => tokens.push((v, t.principal.clone())),
                None => tracing::warn!("auth token for {} has no value (set {:?})", t.principal.id, t.token_env),
            }
        }
        if required && tokens.is_empty() {
            return Err(Error::Config(
                "auth.mode = \"token\" but no token resolved; set the token_env variables".into(),
            ));
        }
        Ok(Self { required, default: cfg.default_principal.clone(), tokens })
    }

    /// Resolve the caller from `authorization: Bearer <token>` (or `x-api-key`).
    pub fn principal(&self, headers: &HeaderMap) -> std::result::Result<Principal, String> {
        let presented = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
            .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
            .map(str::trim);
        match presented {
            Some(tok) => self
                .tokens
                .iter()
                .find(|(t, _)| constant_time_eq(t.as_bytes(), tok.as_bytes()))
                .map(|(_, p)| p.clone())
                .ok_or_else(|| "invalid token".to_string()),
            None if self.required => Err("missing bearer token".into()),
            None => Ok(self.default.clone()),
        }
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
    headers.get("x-onto-workspace").and_then(|v| v.to_str().ok()).map(str::to_string).filter(|w| !w.is_empty())
}
