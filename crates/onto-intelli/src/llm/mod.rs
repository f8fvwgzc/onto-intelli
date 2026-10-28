//! Pluggable LLM providers. Bring your own by implementing [`LlmProvider`].

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};

use onto_core::{Error, Result};

use crate::config::LlmConfig;
use crate::hash::stable_hash;
use crate::token::TokenUsage;

mod contract;
mod mock;
pub use contract::{KeyRef, LlmContract, stray_variables};
pub use mock::MockProvider;

#[cfg(feature = "http-llm")]
mod anthropic;
#[cfg(feature = "http-llm")]
mod openai;
#[cfg(feature = "http-llm")]
mod sse;
#[cfg(feature = "http-llm")]
pub use anthropic::AnthropicProvider;
#[cfg(feature = "http-llm")]
pub use openai::OpenAiCompatProvider;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: ChatRole::User, content: content.into() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: ChatRole::Assistant, content: content.into() }
    }
}

/// Provider-neutral request. `system` is kept byte-stable across requests so
/// provider prompt caches stay warm; everything volatile goes in `messages`.
#[derive(Clone, Debug)]
pub struct LlmRequest {
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
}

impl LlmRequest {
    pub fn cache_key(&self, provider: &str, model: &str) -> String {
        let mut parts: Vec<&str> = vec![provider, model, &self.system];
        let max = self.max_tokens.to_string();
        parts.push(&max);
        for m in &self.messages {
            parts.push(if m.role == ChatRole::User { "u" } else { "a" });
            parts.push(&m.content);
        }
        stable_hash(&parts)
    }

    /// Messages that strictly alternate and start with the user (required by
    /// some providers): leading assistant turns dropped, same-role runs merged.
    pub fn alternating(&self) -> Vec<ChatMessage> {
        let mut out: Vec<ChatMessage> = Vec::new();
        for m in &self.messages {
            if out.is_empty() && m.role == ChatRole::Assistant {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.role == m.role => {
                    last.content.push_str("\n\n");
                    last.content.push_str(&m.content);
                }
                _ => out.push(m.clone()),
            }
        }
        out
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LlmResponse {
    pub text: String,
    pub usage: TokenUsage,
    pub model: String,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

pub enum StreamChunk {
    Delta(String),
    Done(LlmResponse),
}

pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;

    /// `None` when usable; otherwise why not (e.g. missing API key).
    fn unavailable(&self) -> Option<String> {
        None
    }

    /// The most output tokens this provider asks for (its own
    /// `max_output_tokens`, which may be a project's). `None`: requests use
    /// the server's `[llm] max_output_tokens`.
    fn max_output_tokens(&self) -> Option<u32> {
        None
    }

    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse>;

    /// Token stream. Default: one chunk from [`complete`](Self::complete).
    async fn stream(&self, req: &LlmRequest) -> Result<ChunkStream> {
        let resp = self.complete(req).await?;
        let chunks = vec![Ok(StreamChunk::Delta(resp.text.clone())), Ok(StreamChunk::Done(resp))];
        Ok(Box::pin(futures::stream::iter(chunks)))
    }
}

/// Provider used when the LLM is switched off: the orchestrator falls back to
/// deterministic answers and fact summaries.
pub struct DisabledProvider;

#[async_trait]
impl LlmProvider for DisabledProvider {
    fn name(&self) -> &str {
        "none"
    }

    fn model(&self) -> &str {
        "none"
    }

    fn unavailable(&self) -> Option<String> {
        Some("LLM provider is disabled (llm.provider = \"none\")".into())
    }

    async fn complete(&self, _req: &LlmRequest) -> Result<LlmResponse> {
        Err(Error::Unavailable("LLM provider is disabled".into()))
    }
}

/// Build the provider named in config.
pub fn build(cfg: &LlmConfig) -> Result<Arc<dyn LlmProvider>> {
    build_provider(cfg, None)
}

/// Build the provider named in config on the given HTTP client (a client
/// with its own DNS resolver, timeout and redirect policy) instead of one of
/// its own. The mock and disabled providers ignore it.
#[cfg(feature = "http-llm")]
pub fn build_with_client(cfg: &LlmConfig, client: reqwest::Client) -> Result<Arc<dyn LlmProvider>> {
    build_provider(cfg, Some(client))
}

#[cfg(feature = "http-llm")]
type Client = Option<reqwest::Client>;
#[cfg(not(feature = "http-llm"))]
type Client = Option<std::convert::Infallible>;

fn build_provider(cfg: &LlmConfig, client: Client) -> Result<Arc<dyn LlmProvider>> {
    let provider = cfg.provider.trim().to_lowercase();
    #[cfg(not(feature = "http-llm"))]
    let _ = client;
    match provider.as_str() {
        "mock" | "" => Ok(Arc::new(MockProvider)),
        "none" | "disabled" | "off" => Ok(Arc::new(DisabledProvider)),
        #[cfg(feature = "http-llm")]
        "openai" | "openai-compatible" | "ollama" | "vllm" | "lmstudio" | "openrouter" | "groq" | "together" => {
            Ok(Arc::new(OpenAiCompatProvider::with_client(cfg, &provider, client)?))
        }
        #[cfg(feature = "http-llm")]
        "anthropic" | "claude" => Ok(Arc::new(AnthropicProvider::with_client(cfg, client)?)),
        other => Err(Error::Config(format!(
            "unknown llm.provider '{other}' (mock, none, openai, ollama, openai-compatible, anthropic{})",
            if cfg!(feature = "http-llm") { "" } else { "; http providers need the http-llm feature" }
        ))),
    }
}

/// Where a provider's key comes from when it sends a request.
#[derive(Clone)]
#[cfg_attr(not(feature = "http-llm"), allow(dead_code))]
pub(crate) enum KeySource {
    /// No key: the variable named (empty: none applies).
    Missing(String),
    Value(String),
    /// Read on every request (a mounted Secret rotates in place).
    File(std::path::PathBuf),
}

#[cfg_attr(not(feature = "http-llm"), allow(dead_code))]
impl KeySource {
    /// `cfg.api_key`, else `cfg.api_key_file`, else the configured (or
    /// conventional) environment variable.
    pub(crate) fn of(cfg: &LlmConfig, default_env: &str) -> KeySource {
        if let Some(k) = cfg.api_key.as_ref().filter(|k| !k.expose().trim().is_empty()) {
            return KeySource::Value(k.expose().to_string());
        }
        if let Some(f) = &cfg.api_key_file {
            return KeySource::File(f.clone());
        }
        match api_key(cfg, default_env) {
            (Some(k), _) => KeySource::Value(k),
            (None, var) => KeySource::Missing(var),
        }
    }

    /// The key now (a file is read again).
    pub(crate) fn get(&self) -> Result<Option<String>> {
        match self {
            KeySource::Missing(_) => Ok(None),
            KeySource::Value(v) => Ok(Some(v.clone())),
            KeySource::File(p) => match std::fs::read_to_string(p) {
                Ok(k) if !k.trim().is_empty() => Ok(Some(k.trim().to_string())),
                Ok(_) => Err(Error::Unavailable(format!("the LLM key file {} is empty", p.display()))),
                Err(e) => Err(Error::Unavailable(format!("reading the LLM key file {}: {}", p.display(), e.kind()))),
            },
        }
    }

    /// Why a provider that needs a key cannot run (`None` for a file: it is
    /// checked on every request).
    pub(crate) fn missing(&self) -> Option<String> {
        match self {
            KeySource::Missing(var) => Some(missing_key(var)),
            _ => None,
        }
    }
}

/// The API key: `cfg.api_key` when set, else the configured (or
/// conventional) environment variable. Returns the key and the variable it
/// came from (empty for `cfg.api_key`, or when no variable applies).
pub fn api_key(cfg: &LlmConfig, default_env: &str) -> (Option<String>, String) {
    if let Some(k) = cfg.api_key.as_ref().filter(|k| !k.expose().trim().is_empty()) {
        return (Some(k.expose().to_string()), String::new());
    }
    let var = cfg.api_key_env.clone().unwrap_or_else(|| default_env.to_string());
    let key = if var.is_empty() { None } else { std::env::var(&var).ok().filter(|k| !k.trim().is_empty()) };
    (key, var)
}

/// Why a provider that needs a key has none: the variable is unset, or no
/// key was given at all.
#[cfg_attr(not(feature = "http-llm"), allow(dead_code))]
pub(crate) fn missing_key(var: &str) -> String {
    if var.is_empty() { "no API key is set".into() } else { format!("environment variable {var} is not set") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Secret;

    #[test]
    fn the_given_key_comes_before_the_environment() {
        let cfg = LlmConfig {
            api_key: Some(Secret::new("sk-given")),
            api_key_env: Some("PATH".into()),
            ..Default::default()
        };
        assert_eq!(api_key(&cfg, "OPENAI_API_KEY"), (Some("sk-given".into()), String::new()));
        // An empty variable name reads no environment at all (project providers).
        let none = LlmConfig { api_key_env: Some(String::new()), ..Default::default() };
        assert_eq!(api_key(&none, "PATH"), (None, String::new()));
        assert_eq!(missing_key(""), "no API key is set");
    }
}
