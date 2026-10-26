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
//! What `[llm]` in the configuration file sets wins, and a project's own
//! provider (the console's bring-your-own LLM) wins over both: nothing changes
//! for a deployment that does not set the variables. The connection (provider,
//! endpoint, credential and model) applies as one, and only when `[llm]` names
//! no endpoint or credential of its own and no provider of another wire
//! format, so a key never goes to a host it was not issued for; timeout and
//! retries apply key by key. `LLM_MODEL`, `LLM_BASE_URL` or a key without
//! `LLM_PROVIDER` or `LLM_GATEWAY_URL` are ignored, with a warning
//! ([`stray_variables`]). Errors name the variable. Keys are never logged.

use std::collections::BTreeSet;
use std::path::PathBuf;

use onto_core::{Error, Result};

use crate::config::{LlmConfig, Secret};

/// The wire formats the contract names.
const PROVIDERS: [&str; 3] = ["anthropic", "openai", "openai-compatible"];

/// The variables that mean nothing without `LLM_PROVIDER` or `LLM_GATEWAY_URL`.
const NEEDS_PROVIDER: [&str; 6] =
    ["LLM_BASE_URL", "LLM_API_KEY", "LLM_API_KEY_FILE", "LLM_MODEL", "LLM_MODEL_SMALL", "LLM_GATEWAY_TOKEN_FILE"];

/// The wire format a provider name speaks: `anthropic` (also `claude`),
/// `openai` (also `openai-compatible`), else the name itself.
fn wire(provider: &str) -> String {
    match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => "anthropic".into(),
        "openai-compatible" => "openai".into(),
        p => p.into(),
    }
}

/// `LLM_*` variables that are set although the contract is off (no
/// `LLM_PROVIDER` or `LLM_GATEWAY_URL`): ignored, and worth a warning.
pub fn stray_variables(get: impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
    let set = |k: &str| get(k).is_some_and(|v| !v.trim().is_empty());
    if set("LLM_PROVIDER") || set("LLM_GATEWAY_URL") {
        return Vec::new();
    }
    NEEDS_PROVIDER.into_iter().filter(|k| set(k)).collect()
}

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
    /// `LLM_GATEWAY_URL` is set (other `LLM_*` are then ignored).
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    /// From any variable source (tests).
    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let var = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let provider = var("LLM_PROVIDER").map(|p| p.to_ascii_lowercase());
        let gateway = var("LLM_GATEWAY_URL");
        if provider.is_none() && gateway.is_none() {
            // Off. Other LLM_* alone must not break a deployment that never
            // used the contract (see stray_variables).
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

    /// Fill `cfg` where `[llm]` left it unset (`explicit`: the keys present
    /// in the file's `[llm]` table). The connection (provider, endpoint,
    /// credential, model) applies as one, or not at all when `[llm]` names an
    /// endpoint or a credential of its own, or a provider of another wire
    /// format: then the answer says why, for the startup log.
    pub fn apply(&self, cfg: &mut LlmConfig, explicit: &BTreeSet<String>) -> Option<String> {
        let free = |k: &str| !explicit.contains(k);
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
        let own: Vec<&str> =
            ["base_url", "api_key", "api_key_env", "api_key_file"].into_iter().filter(|k| !free(k)).collect();
        let why = if !free("provider") && wire(&cfg.provider) != wire(&self.provider) {
            Some(format!(
                "[llm] provider = \"{}\" does not speak the contract's {}",
                cfg.provider,
                wire(&self.provider)
            ))
        } else if !own.is_empty() {
            Some(format!("[llm] sets {}", own.join(", ")))
        } else {
            None
        };
        if let Some(why) = why {
            return Some(format!(
                "{why}: the LLM_* connection (provider, endpoint, key and model) is ignored, [llm] applies as written"
            ));
        }
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
        match &self.key {
            Some(KeyRef::File(p)) => cfg.api_key_file = Some(p.clone()),
            Some(KeyRef::Value(v)) => cfg.api_key = Some(Secret::new(v.clone())),
            None => {}
        }
        match &self.gateway {
            Some(root) => {
                // Anthropic Messages is at {root}/v1/messages; OpenAI's base includes /v1.
                let anthropic = wire(&cfg.provider) == "anthropic";
                cfg.base_url = Some(if anthropic { root.clone() } else { format!("{root}/v1") });
                // The gateway token only: no provider key from the environment.
                cfg.api_key_env = Some(String::new());
                cfg.bearer_auth = true;
            }
            None if self.base_url.is_some() => {
                cfg.base_url = self.base_url.clone();
                if self.key.is_none() {
                    // A keyless endpoint: the provider's conventional key
                    // variable is not for this host.
                    cfg.api_key_env = Some(String::new());
                }
            }
            _ => {}
        }
        None
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
    fn stray_variables_are_ignored_and_named() {
        let stray = [("LLM_MODEL", "m"), ("LLM_API_KEY", "k"), ("LLM_API_KEY_FILE", "/f"), ("OTHER", "x")];
        assert_eq!(vars(&stray).unwrap(), None, "the contract is off, and nothing fails");
        let m: HashMap<&str, &str> = stray.into_iter().collect();
        let get = |k: &str| m.get(k).map(|v| v.to_string());
        assert_eq!(stray_variables(get), vec!["LLM_API_KEY", "LLM_API_KEY_FILE", "LLM_MODEL"]);
        let on: HashMap<&str, &str> = [("LLM_PROVIDER", "openai"), ("LLM_MODEL", "m")].into_iter().collect();
        assert!(stray_variables(|k| on.get(k).map(|v| v.to_string())).is_empty());
        // The chart's key file without the optional ConfigMap: onto still starts.
        let mut cfg = crate::config::Config::from_toml("").unwrap();
        let before = format!("{:?}", cfg.llm);
        cfg.apply_llm_contract_from(|k| (k == "LLM_API_KEY_FILE").then(|| "/var/run/secrets/llm/api-key".into()))
            .unwrap();
        assert_eq!(format!("{:?}", cfg.llm), before);
    }

    #[test]
    fn errors_name_the_variable() {
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
        // What [llm] sets wins.
        let mut cfg = LlmConfig { model: "from-file".into(), timeout_secs: 9, ..Default::default() };
        let explicit: BTreeSet<String> = ["model".to_string(), "timeout_secs".to_string()].into();
        assert_eq!(c.apply(&mut cfg, &explicit), None);
        assert_eq!((cfg.model.as_str(), cfg.provider.as_str()), ("from-file", "openai-compatible"));
        assert_eq!((cfg.timeout_secs, cfg.max_retries), (9, 2));
    }

    fn file(toml: &str) -> crate::config::Config {
        crate::config::Config::from_toml(toml).unwrap()
    }

    #[test]
    fn the_connection_applies_as_one_or_not_at_all() {
        // (a) The file's provider speaks another wire format: the Anthropic
        // key and model do not go to api.openai.com.
        let c = vars(&[
            ("LLM_PROVIDER", "anthropic"),
            ("LLM_MODEL", "claude-opus-5-5"),
            ("LLM_API_KEY_FILE", "/var/run/secrets/llm/api-key"),
            ("LLM_MAX_RETRIES", "4"),
        ])
        .unwrap()
        .unwrap();
        let cfg = file("[llm]\nprovider = \"openai\"\n");
        let mut llm = cfg.llm.clone();
        let why = c.apply(&mut llm, &cfg.llm_explicit).expect("set aside");
        assert!(why.contains("provider = \"openai\""), "{why}");
        assert_eq!((llm.provider.as_str(), llm.model.as_str()), ("openai", cfg.llm.model.as_str()));
        assert_eq!((llm.api_key_file.clone(), llm.base_url.clone()), (None, None));
        assert_eq!(llm.max_retries, 4, "retries still apply");
        // (b) The file names its endpoint: the gateway token is not sent there.
        let gw = vars(&[
            ("LLM_GATEWAY_URL", "https://gateway:8443"),
            ("LLM_GATEWAY_TOKEN_FILE", "/var/run/secrets/llm/gateway-token"),
            ("LLM_MODEL", "claude-opus-5-5"),
        ])
        .unwrap()
        .unwrap();
        let cfg = file("[llm]\nprovider = \"anthropic\"\nbase_url = \"https://api.anthropic.com\"\n");
        let mut llm = cfg.llm.clone();
        assert!(gw.apply(&mut llm, &cfg.llm_explicit).is_some_and(|w| w.contains("sets base_url")));
        assert_eq!(llm.base_url.as_deref(), Some("https://api.anthropic.com"));
        assert_eq!((llm.api_key_file.clone(), llm.bearer_auth), (None, false));
        // (c) The file names its key: the provider key does not go to the gateway.
        let cfg = file("[llm]\napi_key_env = \"ANTHROPIC_API_KEY\"\n");
        let mut llm = cfg.llm.clone();
        assert!(gw.apply(&mut llm, &cfg.llm_explicit).is_some_and(|w| w.contains("sets api_key_env")));
        assert_eq!(llm.base_url, None);
        assert_eq!(llm.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(!llm.bearer_auth);
        // The same wire format under another name joins the gateway at the right path.
        let cfg = file("[llm]\nprovider = \"Claude\"\n");
        let mut llm = cfg.llm.clone();
        assert_eq!(gw.apply(&mut llm, &cfg.llm_explicit), None);
        assert_eq!(llm.base_url.as_deref(), Some("https://gateway:8443"), "not {{root}}/v1/v1/messages");
        assert_eq!(llm.api_key_file, Some(PathBuf::from("/var/run/secrets/llm/gateway-token")));
        // A contract endpoint without a contract key gets no conventional key either.
        let keyless = vars(&[("LLM_PROVIDER", "openai"), ("LLM_BASE_URL", "http://vllm:8000/v1"), ("LLM_MODEL", "m")])
            .unwrap()
            .unwrap();
        let mut llm = LlmConfig::default();
        assert_eq!(keyless.apply(&mut llm, &BTreeSet::new()), None);
        assert_eq!(llm.api_key_env.as_deref(), Some(""), "OPENAI_API_KEY is not sent to vllm");
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
