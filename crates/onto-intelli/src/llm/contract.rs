//! The platform's LLM connection contract, v1: one set of `LLM_*`
//! environment variables (a Kubernetes ConfigMap `llm-config` and Secret
//! `llm-credentials`) configures model access for every core (texc-symphony,
//! nexc-engine, attache, onto-intelli).
//!
//! | Variable | Meaning |
//! |---|---|
//! | `LLM_PROVIDER` | `anthropic`, `openai` or `openai-compatible` (required unless gateway mode; there it is the wire format, default `anthropic`) |
//! | `LLM_BASE_URL` | the provider's API base URL (required for `openai-compatible`) |
//! | `LLM_API_KEY_FILE` | a file holding the key (a mounted Secret), read on every request; wins over `LLM_API_KEY` |
//! | `LLM_API_KEY` | the key as text |
//! | `LLM_MODEL` | the model (required) |
//! | `LLM_MODEL_SMALL` | a small, fast model (recorded; onto makes no small-model calls today) |
//! | `LLM_TIMEOUT_SECS` | timeout of one request (`> 0`) |
//! | `LLM_MAX_RETRIES` | retries of one request (`0..=10`) |
//! | `LLM_GATEWAY_URL` | gateway mode: every call goes to the platform's model gateway (its root URL; a trailing `/v1` is ignored), and onto holds no provider key |
//! | `LLM_GATEWAY_TOKEN_FILE` | with the gateway: a file holding its bearer token |
//!
//! The contract applies once `LLM_PROVIDER` or `LLM_GATEWAY_URL` is set.
//! What `[llm]` in the configuration file sets always wins, key by key, and a
//! project's own provider (the console's bring-your-own LLM) wins over both:
//! nothing changes for a deployment that does not set the variables. Setting
//! `LLM_MODEL`, `LLM_BASE_URL` or a key without `LLM_PROVIDER` or
//! `LLM_GATEWAY_URL` is an error, so a typo does not silently disable the
//! contract. Errors name the variable. Keys are never logged.

use std::collections::BTreeSet;
use std::path::PathBuf;

use onto_core::{Error, Result};

use crate::config::{LlmConfig, Secret};

/// The wire formats the contract names.
const PROVIDERS: [&str; 3] = ["anthropic", "openai", "openai-compatible"];

/// What the `LLM_*` variables say, validated.
#[derive(Clone, Debug, PartialEq)]
pub struct LlmContract {
    pub provider: String,
    pub base_url: Option<String>,
    pub key: Option<KeyRef>,
    pub model: String,
    pub model_small: Option<String>,
    pub timeout_secs: Option<u64>,
    pub max_retries: Option<u32>,
    /// Gateway mode: the gateway's root URL (no trailing `/v1`).
    pub gateway: Option<String>,
}

/// Where the credential comes from. The value of `LLM_API_KEY` is kept only
/// as a [`Secret`]; files are read on every request.
#[derive(Clone, PartialEq)]
pub enum KeyRef {
    File(PathBuf),
    Value(String),
}

impl std::fmt::Debug for KeyRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyRef::File(p) => write!(f, "File({})", p.display()),
            KeyRef::Value(_) => f.write_str("Value(****)"),
        }
    }
}

fn invalid(var: &str, why: impl std::fmt::Display) -> Error {
    Error::Config(format!("{var} {why} (LLM connection contract v1)"))
}

impl LlmContract {
    /// From the process environment; `None` when neither `LLM_PROVIDER` nor
    /// `LLM_GATEWAY_URL` is set (and no other `LLM_*` either).
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// From any variable source (tests).
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let var = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let provider = var("LLM_PROVIDER").map(|p| p.to_ascii_lowercase());
        let gateway = var("LLM_GATEWAY_URL");
        if provider.is_none() && gateway.is_none() {
            for k in ["LLM_MODEL", "LLM_BASE_URL", "LLM_API_KEY", "LLM_API_KEY_FILE", "LLM_GATEWAY_TOKEN_FILE"] {
                if var(k).is_some() {
                    return Err(invalid(k, "is set without LLM_PROVIDER or LLM_GATEWAY_URL"));
                }
            }
            return Ok(None);
        }
        if let Some(p) = &provider
            && !PROVIDERS.contains(&p.as_str())
        {
            return Err(invalid("LLM_PROVIDER", format!("is invalid ('{p}'; {})", PROVIDERS.join(", "))));
        }
        let model = var("LLM_MODEL").ok_or_else(|| invalid("LLM_MODEL", "can't be blank"))?;
        if model.chars().any(char::is_whitespace) {
            return Err(invalid("LLM_MODEL", "is invalid (no whitespace)"));
        }
        let timeout_secs = match var("LLM_TIMEOUT_SECS") {
            Some(v) => match v.parse::<u64>() {
                Ok(n) if n > 0 => Some(n),
                _ => return Err(invalid("LLM_TIMEOUT_SECS", format!("is invalid ('{v}'; seconds > 0)"))),
            },
            None => None,
        };
        let max_retries = match var("LLM_MAX_RETRIES") {
            Some(v) => match v.parse::<u32>() {
                Ok(n) if n <= 10 => Some(n),
                _ => return Err(invalid("LLM_MAX_RETRIES", format!("is invalid ('{v}'; 0 to 10)"))),
            },
            None => None,
        };
        let url = |k: &str, v: String| -> Result<String> {
            let u = v.trim_end_matches('/').to_string();
            if !(u.starts_with("https://") || u.starts_with("http://")) {
                return Err(invalid(k, format!("is invalid ('{u}'; an http(s) URL)")));
            }
            Ok(u)
        };
        if let Some(g) = gateway {
            let root = url("LLM_GATEWAY_URL", g)?;
            let root = root.strip_suffix("/v1").unwrap_or(&root).to_string();
            let token = var("LLM_GATEWAY_TOKEN_FILE")
                .ok_or_else(|| invalid("LLM_GATEWAY_TOKEN_FILE", "is required with LLM_GATEWAY_URL"))?;
            return Ok(Some(Self {
                provider: provider.unwrap_or_else(|| "anthropic".into()),
                base_url: None,
                key: Some(KeyRef::File(PathBuf::from(token))),
                model,
                model_small: var("LLM_MODEL_SMALL"),
                timeout_secs,
                max_retries,
                gateway: Some(root),
            }));
        }
        let provider = provider.unwrap_or_default();
        let base_url = var("LLM_BASE_URL").map(|v| url("LLM_BASE_URL", v)).transpose()?;
        if provider == "openai-compatible" && base_url.is_none() {
            return Err(invalid("LLM_BASE_URL", "is required with LLM_PROVIDER=openai-compatible"));
        }
        let key = match (var("LLM_API_KEY_FILE"), get("LLM_API_KEY").filter(|k| !k.trim().is_empty())) {
            (Some(f), _) => Some(KeyRef::File(PathBuf::from(f))),
            (None, Some(k)) => Some(KeyRef::Value(k.trim().to_string())),
            (None, None) => None,
        };
        Ok(Some(Self {
            provider,
            base_url,
            key,
            model,
            model_small: var("LLM_MODEL_SMALL"),
            timeout_secs,
            max_retries,
            gateway: None,
        }))
    }

    /// Fill `cfg`'s fields that `[llm]` did not set (`explicit`: the keys
    /// present in the file's `[llm]` table).
    pub fn apply(&self, cfg: &mut LlmConfig, explicit: &BTreeSet<String>) {
        let free = |k: &str| !explicit.contains(k);
        if free("provider") {
            cfg.provider = self.provider.clone();
        }
        if free("model") {
            cfg.model = self.model.clone();
        }
        if free("model_small")
            && let Some(m) = &self.model_small
        {
            cfg.model_small = m.clone();
        }
        if free("timeout_secs")
            && let Some(t) = self.timeout_secs
        {
            cfg.timeout_secs = t;
        }
        if free("max_retries")
            && let Some(r) = self.max_retries
        {
            cfg.max_retries = r;
        }
        if free("base_url") {
            match &self.gateway {
                // Anthropic Messages is at {root}/v1/messages; OpenAI's base includes /v1.
                Some(root) if cfg.provider == "anthropic" => cfg.base_url = Some(root.clone()),
                Some(root) => cfg.base_url = Some(format!("{root}/v1")),
                None if self.base_url.is_some() => cfg.base_url = self.base_url.clone(),
                None => {}
            }
        }
        // The credential, unless the file names its own.
        let key_set = ["api_key_env", "api_key_file"].iter().any(|k| explicit.contains(*k));
        if !key_set {
            match &self.key {
                Some(KeyRef::File(p)) => cfg.api_key_file = Some(p.clone()),
                Some(KeyRef::Value(v)) => cfg.api_key = Some(Secret::new(v.clone())),
                None => {}
            }
            if self.gateway.is_some() {
                // The gateway token only: no provider key from the environment.
                cfg.api_key_env = Some(String::new());
                cfg.bearer_auth = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Result<Option<LlmContract>> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        LlmContract::from_vars(|k| m.get(k).cloned())
    }

    fn err(pairs: &[(&str, &str)]) -> String {
        vars(pairs).unwrap_err().to_string()
    }

    #[test]
    fn nothing_set_changes_nothing() {
        assert_eq!(vars(&[]).unwrap(), None);
        let mut cfg = LlmConfig::default();
        let before = format!("{cfg:?}");
        if let Some(c) = vars(&[]).unwrap() {
            c.apply(&mut cfg, &BTreeSet::new());
        }
        assert_eq!(format!("{cfg:?}"), before);
    }

    #[test]
    fn errors_name_the_variable() {
        assert!(err(&[("LLM_MODEL", "m")]).contains("LLM_MODEL is set without LLM_PROVIDER"));
        assert!(err(&[("LLM_API_KEY", "k")]).contains("LLM_API_KEY"));
        assert!(err(&[("LLM_PROVIDER", "cohere"), ("LLM_MODEL", "m")]).contains("LLM_PROVIDER is invalid"));
        assert!(err(&[("LLM_PROVIDER", "anthropic")]).contains("LLM_MODEL can't be blank"));
        assert!(err(&[("LLM_PROVIDER", "openai-compatible"), ("LLM_MODEL", "m")]).contains("LLM_BASE_URL is required"));
        let t = [("LLM_PROVIDER", "openai"), ("LLM_MODEL", "m"), ("LLM_TIMEOUT_SECS", "0")];
        assert!(err(&t).contains("LLM_TIMEOUT_SECS is invalid"));
        let r = [("LLM_PROVIDER", "openai"), ("LLM_MODEL", "m"), ("LLM_MAX_RETRIES", "11")];
        assert!(err(&r).contains("LLM_MAX_RETRIES is invalid"));
        let g = [("LLM_GATEWAY_URL", "https://gw.example"), ("LLM_MODEL", "m")];
        assert!(err(&g).contains("LLM_GATEWAY_TOKEN_FILE is required"));
        let secret = [("LLM_PROVIDER", "nope"), ("LLM_MODEL", "m"), ("LLM_API_KEY", "sk-secret-value")];
        assert!(!err(&secret).contains("sk-secret-value"), "never the key");
        assert!(!format!("{:?}", KeyRef::Value("sk-secret-value".into())).contains("sk-secret"));
    }

    #[test]
    fn provider_mode_fills_what_the_file_left_unset() {
        let c = vars(&[
            ("LLM_PROVIDER", "openai-compatible"),
            ("LLM_BASE_URL", "http://vllm:8000/v1/"),
            ("LLM_API_KEY_FILE", "/var/run/secrets/llm/api-key"),
            ("LLM_API_KEY", "ignored-when-a-file-is-set"),
            ("LLM_MODEL", "qwen3"),
            ("LLM_MODEL_SMALL", "qwen3-small"),
            ("LLM_TIMEOUT_SECS", "30"),
            ("LLM_MAX_RETRIES", "2"),
        ])
        .unwrap()
        .unwrap();
        let mut cfg = LlmConfig::default();
        c.apply(&mut cfg, &BTreeSet::new());
        assert_eq!(cfg.provider, "openai-compatible");
        assert_eq!(cfg.base_url.as_deref(), Some("http://vllm:8000/v1"));
        assert_eq!(cfg.api_key_file, Some(PathBuf::from("/var/run/secrets/llm/api-key")));
        assert!(cfg.api_key.is_none());
        assert_eq!((cfg.model.as_str(), cfg.model_small.as_str()), ("qwen3", "qwen3-small"));
        assert_eq!((cfg.timeout_secs, cfg.max_retries), (30, 2));
        // What [llm] sets wins, key by key.
        let mut cfg = LlmConfig { model: "from-file".into(), ..Default::default() };
        let explicit: BTreeSet<String> = ["model".to_string(), "api_key_env".to_string()].into();
        c.apply(&mut cfg, &explicit);
        assert_eq!((cfg.model.as_str(), cfg.provider.as_str()), ("from-file", "openai-compatible"));
        assert_eq!(cfg.api_key_file, None, "the file named its own key");
    }

    #[test]
    fn the_file_s_llm_keys_are_recorded_for_precedence() {
        let cfg = crate::config::Config::from_toml("[llm]\nprovider = \"mock\"\nmodel = \"x\"\n").unwrap();
        assert_eq!(cfg.llm_explicit, ["model".to_string(), "provider".to_string()].into());
        // An explicit `provider = "mock"` keeps the mock, whatever LLM_* says.
        let c = vars(&[("LLM_PROVIDER", "anthropic"), ("LLM_MODEL", "claude-opus-5-5")]).unwrap().unwrap();
        let mut llm = cfg.llm.clone();
        c.apply(&mut llm, &cfg.llm_explicit);
        assert_eq!((llm.provider.as_str(), llm.model.as_str()), ("mock", "x"));
        assert!(crate::config::Config::from_toml("").unwrap().llm_explicit.is_empty());
    }

    #[test]
    fn gateway_mode_holds_no_provider_key() {
        let c = vars(&[
            ("LLM_GATEWAY_URL", "https://gateway.attache:8443/v1"),
            ("LLM_GATEWAY_TOKEN_FILE", "/var/run/secrets/llm/gateway-token"),
            ("LLM_API_KEY", "sk-provider"),
            ("LLM_MODEL", "claude-opus-5-5"),
        ])
        .unwrap()
        .unwrap();
        let mut cfg = LlmConfig::default();
        c.apply(&mut cfg, &BTreeSet::new());
        assert_eq!(cfg.provider, "anthropic");
        assert_eq!(cfg.base_url.as_deref(), Some("https://gateway.attache:8443"));
        assert_eq!(cfg.api_key_file, Some(PathBuf::from("/var/run/secrets/llm/gateway-token")));
        assert!(cfg.api_key.is_none(), "LLM_API_KEY is ignored in gateway mode");
        assert_eq!(cfg.api_key_env.as_deref(), Some(""), "no ANTHROPIC_API_KEY either");
        assert!(cfg.bearer_auth);
        let openai = vars(&[
            ("LLM_GATEWAY_URL", "https://gateway.attache:8443"),
            ("LLM_GATEWAY_TOKEN_FILE", "/t"),
            ("LLM_PROVIDER", "openai"),
            ("LLM_MODEL", "gpt-5"),
        ])
        .unwrap()
        .unwrap();
        let mut cfg = LlmConfig::default();
        openai.apply(&mut cfg, &BTreeSet::new());
        assert_eq!(cfg.base_url.as_deref(), Some("https://gateway.attache:8443/v1"));
    }
}
