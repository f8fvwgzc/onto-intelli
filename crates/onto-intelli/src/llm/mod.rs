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

mod mock;
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
    let provider = cfg.provider.trim().to_lowercase();
    match provider.as_str() {
        "mock" | "" => Ok(Arc::new(MockProvider)),
        "none" | "disabled" | "off" => Ok(Arc::new(DisabledProvider)),
        #[cfg(feature = "http-llm")]
        "openai" | "openai-compatible" | "ollama" | "vllm" | "lmstudio" | "openrouter" | "groq" | "together" => {
            Ok(Arc::new(OpenAiCompatProvider::new(cfg, &provider)?))
        }
        #[cfg(feature = "http-llm")]
        "anthropic" | "claude" => Ok(Arc::new(AnthropicProvider::new(cfg)?)),
        other => Err(Error::Config(format!(
            "unknown llm.provider '{other}' (mock, none, openai, ollama, openai-compatible, anthropic{})",
            if cfg!(feature = "http-llm") { "" } else { "; http providers need the http-llm feature" }
        ))),
    }
}

/// Read an API key from the configured (or conventional) environment variable.
pub fn api_key(cfg: &LlmConfig, default_env: &str) -> (Option<String>, String) {
    let var = cfg.api_key_env.clone().unwrap_or_else(|| default_env.to_string());
    let key = if var.is_empty() { None } else { std::env::var(&var).ok().filter(|k| !k.trim().is_empty()) };
    (key, var)
}
