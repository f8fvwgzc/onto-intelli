//! OpenAI Chat Completions and compatible servers (Ollama, vLLM, LM Studio,
//! OpenRouter, Groq, Together, ...).

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};

use onto_core::{Error, Result};

use super::{ChatRole, ChunkStream, LlmProvider, LlmRequest, LlmResponse, StreamChunk, api_key, sse};
use crate::config::{LlmConfig, Pricing};
use crate::token::TokenUsage;

pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    name: String,
    base: String,
    key: Option<String>,
    model: String,
    temperature: Option<f32>,
    max_field: &'static str,
    pricing: Pricing,
    headers: Vec<(String, String)>,
    unavailable: Option<String>,
}

impl OpenAiCompatProvider {
    pub fn new(cfg: &LlmConfig, flavor: &str) -> Result<Self> {
        let (base, default_model, key_env) = match flavor {
            "openai" => ("https://api.openai.com/v1", "gpt-4o-mini", "OPENAI_API_KEY"),
            "ollama" => ("http://localhost:11434/v1", "llama3.2", ""),
            "openrouter" => ("https://openrouter.ai/api/v1", "openai/gpt-4o-mini", "OPENROUTER_API_KEY"),
            "groq" => ("https://api.groq.com/openai/v1", "llama-3.3-70b-versatile", "GROQ_API_KEY"),
            "together" => {
                ("https://api.together.xyz/v1", "meta-llama/Llama-3.3-70B-Instruct-Turbo", "TOGETHER_API_KEY")
            }
            _ => ("http://localhost:8000/v1", "default", ""),
        };
        let (key, var) = api_key(cfg, key_env);
        let unavailable = (key.is_none() && !var.is_empty() && flavor != "ollama")
            .then(|| format!("environment variable {var} is not set"));
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs.max(1)))
            .build()
            .map_err(|e| Error::Config(format!("http client: {e}")))?;
        Ok(Self {
            client,
            name: flavor.to_string(),
            base: cfg.base_url.clone().unwrap_or_else(|| base.to_string()).trim_end_matches('/').to_string(),
            key,
            model: if cfg.model.is_empty() { default_model.to_string() } else { cfg.model.clone() },
            temperature: cfg.temperature,
            max_field: if flavor == "openai" { "max_completion_tokens" } else { "max_tokens" },
            pricing: cfg.pricing.clone(),
            headers: cfg.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            unavailable,
        })
    }

    fn body(&self, req: &LlmRequest, stream: bool) -> Value {
        let mut messages = vec![json!({ "role": "system", "content": req.system })];
        for m in &req.messages {
            let role = if m.role == ChatRole::User { "user" } else { "assistant" };
            messages.push(json!({ "role": role, "content": m.content }));
        }
        let mut body = json!({ "model": self.model, "messages": messages });
        body[self.max_field] = json!(req.max_tokens);
        if let Some(t) = self.temperature {
            body["temperature"] = json!(t);
        }
        if stream {
            body["stream"] = json!(true);
            body["stream_options"] = json!({ "include_usage": true });
        }
        body
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response> {
        if let Some(why) = &self.unavailable {
            return Err(Error::Unavailable(format!("{}: {why}", self.name)));
        }
        let mut rb = self.client.post(format!("{}/chat/completions", self.base)).json(body);
        if let Some(key) = &self.key {
            rb = rb.bearer_auth(key);
        }
        for (k, v) in &self.headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().await.map_err(|e| Error::Llm(format!("{}: {e}", self.name)))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Llm(format!("{} returned {status}: {}", self.name, truncate(&text, 600))));
        }
        Ok(resp)
    }

    fn usage(&self, v: &Value) -> TokenUsage {
        let mut u = TokenUsage {
            prompt_tokens: v["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: v["completion_tokens"].as_u64().unwrap_or(0),
            cached_tokens: v["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
            ..Default::default()
        };
        u.price(&self.pricing);
        u
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn unavailable(&self) -> Option<String> {
        self.unavailable.clone()
    }

    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let resp = self.post(&self.body(req, false)).await?;
        let v: Value = resp.json().await.map_err(|e| Error::Llm(format!("{}: bad response: {e}", self.name)))?;
        let choice = &v["choices"][0];
        Ok(LlmResponse {
            text: choice["message"]["content"].as_str().unwrap_or_default().to_string(),
            usage: self.usage(&v["usage"]),
            model: v["model"].as_str().unwrap_or(&self.model).to_string(),
            finish_reason: choice["finish_reason"].as_str().map(str::to_string),
        })
    }

    async fn stream(&self, req: &LlmRequest) -> Result<ChunkStream> {
        let resp = self.post(&self.body(req, true)).await?;
        let model = self.model.clone();
        let pricing = self.pricing.clone();
        let events = sse::events(resp);
        let stream = futures::stream::unfold(
            (Box::pin(events), String::new(), TokenUsage::default(), None::<String>, false),
            move |(mut events, mut text, mut usage, mut finish, finished)| {
                let model = model.clone();
                let pricing = pricing.clone();
                async move {
                    if finished {
                        return None;
                    }
                    loop {
                        match events.next().await {
                            Some(Ok(ev)) => {
                                if ev.data.trim() == "[DONE]" {
                                    continue;
                                }
                                let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { continue };
                                if v["usage"].is_object() {
                                    usage = TokenUsage {
                                        prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                                        completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                                        cached_tokens: v["usage"]["prompt_tokens_details"]["cached_tokens"]
                                            .as_u64()
                                            .unwrap_or(0),
                                        ..Default::default()
                                    };
                                    usage.price(&pricing);
                                }
                                if let Some(f) = v["choices"][0]["finish_reason"].as_str() {
                                    finish = Some(f.to_string());
                                }
                                if let Some(delta) = v["choices"][0]["delta"]["content"].as_str()
                                    && !delta.is_empty()
                                {
                                    text.push_str(delta);
                                    let chunk = StreamChunk::Delta(delta.to_string());
                                    return Some((Ok(chunk), (events, text, usage, finish, false)));
                                }
                            }
                            Some(Err(e)) => return Some((Err(e), (events, text, usage, finish, true))),
                            None => {
                                let done = LlmResponse {
                                    text: text.clone(),
                                    usage: usage.clone(),
                                    model,
                                    finish_reason: finish.clone(),
                                };
                                return Some((Ok(StreamChunk::Done(done)), (events, text, usage, finish, true)));
                            }
                        }
                    }
                }
            },
        );
        Ok(Box::pin(stream))
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
