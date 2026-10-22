//! Anthropic Messages API over raw HTTP (Rust has no official SDK).
//!
//! * The stable system prompt carries `cache_control` so repeated questions
//!   reuse the provider-side prompt cache.
//! * Sampling parameters are never sent: current Claude models reject them.
//!   Depth/cost is tuned with `output_config.effort` instead.
//! * Server-side refusal fallback (`fallbacks: "default"`) is on by default;
//!   disable with `llm.fallbacks = false` when going through a proxy that
//!   rejects beta fields.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};

use onto_core::{Error, Result};

use super::openai::{describe, truncate};
use super::{ChatRole, ChunkStream, LlmProvider, LlmRequest, LlmResponse, StreamChunk, api_key, sse};
use crate::config::{LlmConfig, Pricing};
use crate::token::TokenUsage;

const DEFAULT_MODEL: &str = "claude-opus-5-5";
const API_VERSION: &str = "2023-06-01";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

pub struct AnthropicProvider {
    client: reqwest::Client,
    max_output_tokens: u32,
    base: String,
    key: Option<String>,
    model: String,
    effort: Option<String>,
    fallbacks: bool,
    pricing: Pricing,
    headers: Vec<(String, String)>,
    unavailable: Option<String>,
}

/// List prices (USD per MTok) used when `[llm.pricing]` is not configured.
fn default_pricing(model: &str) -> Pricing {
    let (input, output, cached) = match model {
        "claude-opus-5-5" => (4.0, 20.0, 0.20),
        "claude-sonnet-5-5" => (2.0, 10.0, 0.20),
        _ => (0.0, 0.0, 0.0),
    };
    Pricing { input_per_mtok: input, output_per_mtok: output, cached_input_per_mtok: cached }
}

impl AnthropicProvider {
    pub fn new(cfg: &LlmConfig) -> Result<Self> {
        Self::with_client(cfg, None)
    }

    /// On `client` when given (its timeout and policies apply), else on a
    /// client with `cfg.timeout_secs`.
    pub fn with_client(cfg: &LlmConfig, client: Option<reqwest::Client>) -> Result<Self> {
        let (key, var) = api_key(cfg, "ANTHROPIC_API_KEY");
        let model = if cfg.model.is_empty() { DEFAULT_MODEL.to_string() } else { cfg.model.clone() };
        let pricing = if cfg.pricing.input_per_mtok == 0.0 && cfg.pricing.output_per_mtok == 0.0 {
            default_pricing(&model)
        } else {
            cfg.pricing.clone()
        };
        let client = match client {
            Some(c) => c,
            None => reqwest::Client::builder()
                .timeout(Duration::from_secs(cfg.timeout_secs.max(1)))
                .build()
                .map_err(|e| Error::Config(format!("http client: {e}")))?,
        };
        Ok(Self {
            client,
            max_output_tokens: cfg.max_output_tokens.max(1),
            base: cfg
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.anthropic.com".into())
                .trim_end_matches('/')
                .to_string(),
            unavailable: key.is_none().then(|| super::missing_key(&var)),
            key,
            model,
            effort: cfg.effort.clone(),
            fallbacks: cfg.fallbacks,
            pricing,
            headers: cfg.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        })
    }

    fn body(&self, req: &LlmRequest, stream: bool) -> Value {
        let messages: Vec<Value> = req
            .alternating()
            .into_iter()
            .map(|m| json!({ "role": if m.role == ChatRole::User { "user" } else { "assistant" }, "content": m.content }))
            .collect();
        let mut body = json!({
            "model": self.model,
            "max_tokens": req.max_tokens.min(self.max_output_tokens),
            "system": [{ "type": "text", "text": req.system, "cache_control": { "type": "ephemeral" } }],
            "messages": messages,
        });
        if let Some(effort) = &self.effort {
            body["output_config"] = json!({ "effort": effort });
        }
        if self.fallbacks {
            body["fallbacks"] = json!("default");
        }
        if stream {
            body["stream"] = json!(true);
        }
        body
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response> {
        if let Some(why) = &self.unavailable {
            return Err(Error::Unavailable(format!("anthropic: {why}")));
        }
        let mut rb = self
            .client
            .post(format!("{}/v1/messages", self.base))
            .header("x-api-key", self.key.as_deref().unwrap_or_default())
            .header("anthropic-version", API_VERSION)
            .json(body);
        if self.fallbacks {
            rb = rb.header("anthropic-beta", FALLBACK_BETA);
        }
        for (k, v) in &self.headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().await.map_err(|e| Error::Llm(format!("anthropic: {}", describe(&e.without_url()))))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Llm(format!("anthropic returned {status}: {}", truncate(&text, 600))));
        }
        Ok(resp)
    }

    fn usage(&self, v: &Value, output_tokens: u64) -> TokenUsage {
        let input = v["input_tokens"].as_u64().unwrap_or(0);
        let read = v["cache_read_input_tokens"].as_u64().unwrap_or(0);
        let created = v["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let mut u = TokenUsage {
            prompt_tokens: input + read + created,
            completion_tokens: output_tokens,
            cached_tokens: read,
            ..Default::default()
        };
        u.price(&self.pricing);
        u
    }
}

fn refusal_error(v: &Value) -> Error {
    let category = v["stop_details"]["category"].as_str().unwrap_or("unspecified");
    Error::Llm(format!("the model declined this request (refusal category: {category})"))
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn unavailable(&self) -> Option<String> {
        self.unavailable.clone()
    }

    fn max_output_tokens(&self) -> Option<u32> {
        Some(self.max_output_tokens)
    }

    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let resp = self.post(&self.body(req, false)).await?;
        let v: Value = resp.json().await.map_err(|e| Error::Llm(format!("anthropic: bad response: {e}")))?;
        if v["stop_reason"] == "refusal" {
            return Err(refusal_error(&v));
        }
        let text: String = v["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        let output = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
        Ok(LlmResponse {
            text,
            usage: self.usage(&v["usage"], output),
            model: v["model"].as_str().unwrap_or(&self.model).to_string(),
            finish_reason: v["stop_reason"].as_str().map(str::to_string),
        })
    }

    async fn stream(&self, req: &LlmRequest) -> Result<ChunkStream> {
        let resp = self.post(&self.body(req, true)).await?;
        let pricing = self.pricing.clone();
        let default_model = self.model.clone();
        struct St {
            text: String,
            input: Value,
            output: u64,
            model: String,
            stop: Option<String>,
            done: bool,
        }
        let st =
            St { text: String::new(), input: Value::Null, output: 0, model: default_model, stop: None, done: false };
        let events = Box::pin(sse::events(resp));
        let stream = futures::stream::unfold((events, st), move |(mut events, mut st)| {
            let pricing = pricing.clone();
            async move {
                if st.done {
                    return None;
                }
                loop {
                    let ev = match events.next().await {
                        Some(Ok(ev)) => ev,
                        Some(Err(e)) => {
                            st.done = true;
                            return Some((Err(e), (events, st)));
                        }
                        None => {
                            st.done = true;
                            let input = st.input.clone();
                            let mut usage = TokenUsage {
                                prompt_tokens: input["input_tokens"].as_u64().unwrap_or(0)
                                    + input["cache_read_input_tokens"].as_u64().unwrap_or(0)
                                    + input["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                                completion_tokens: st.output,
                                cached_tokens: input["cache_read_input_tokens"].as_u64().unwrap_or(0),
                                ..Default::default()
                            };
                            usage.price(&pricing);
                            if st.stop.as_deref() == Some("refusal") {
                                let err = Error::Llm("the model declined this request (refusal)".into());
                                return Some((Err(err), (events, st)));
                            }
                            let done = LlmResponse {
                                text: st.text.clone(),
                                usage,
                                model: st.model.clone(),
                                finish_reason: st.stop.clone(),
                            };
                            return Some((Ok(StreamChunk::Done(done)), (events, st)));
                        }
                    };
                    let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { continue };
                    match v["type"].as_str().unwrap_or_default() {
                        "message_start" => {
                            st.input = v["message"]["usage"].clone();
                            if let Some(m) = v["message"]["model"].as_str() {
                                st.model = m.to_string();
                            }
                        }
                        "content_block_delta" if v["delta"]["type"] == "text_delta" => {
                            let delta = v["delta"]["text"].as_str().unwrap_or_default().to_string();
                            if !delta.is_empty() {
                                st.text.push_str(&delta);
                                return Some((Ok(StreamChunk::Delta(delta)), (events, st)));
                            }
                        }
                        "message_delta" => {
                            if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                                st.output = n;
                            }
                            if let Some(s) = v["delta"]["stop_reason"].as_str() {
                                st.stop = Some(s.to_string());
                            }
                        }
                        "error" => {
                            st.done = true;
                            let msg = v["error"]["message"].as_str().unwrap_or("stream error").to_string();
                            return Some((Err(Error::Llm(format!("anthropic: {msg}"))), (events, st)));
                        }
                        _ => {}
                    }
                }
            }
        });
        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ChatMessage;

    #[test]
    fn request_body_has_cache_control_and_no_sampling() {
        let cfg = LlmConfig {
            provider: "anthropic".into(),
            temperature: Some(0.3),
            effort: Some("low".into()),
            ..Default::default()
        };
        let p = AnthropicProvider::new(&cfg).unwrap();
        let req = LlmRequest {
            system: "stable".into(),
            messages: vec![ChatMessage::assistant("dropped"), ChatMessage::user("q1"), ChatMessage::user("q2")],
            max_tokens: 1000,
        };
        let b = p.body(&req, false);
        assert_eq!(b["model"], "claude-opus-5-5");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(b.get("temperature").is_none());
        assert_eq!(b["output_config"]["effort"], "low");
        assert_eq!(b["fallbacks"], "default");
        assert_eq!(b["messages"].as_array().unwrap().len(), 1);
        assert_eq!(b["messages"][0]["content"], "q1\n\nq2");
    }
}
