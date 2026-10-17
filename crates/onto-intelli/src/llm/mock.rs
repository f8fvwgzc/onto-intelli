//! Offline, deterministic provider. Answers by selecting the supplied facts
//! that best overlap the question, so the whole pipeline (retrieval, packing,
//! caching, memory) works and is testable without an API key.

use async_trait::async_trait;

use onto_core::Result;
use onto_core::text::tokenize;

use super::{ChunkStream, LlmProvider, LlmRequest, LlmResponse, StreamChunk};
use crate::token::{TokenUsage, estimate};

#[derive(Default)]
pub struct MockProvider;

impl MockProvider {
    fn answer(req: &LlmRequest) -> String {
        let last = req.messages.last().map(|m| m.content.as_str()).unwrap_or_default();
        let question = last.rsplit("Question:").next().unwrap_or(last).trim();
        let q_terms = tokenize(question);
        let facts: Vec<&str> = last
            .split("Question:")
            .next()
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().strip_prefix("- "))
            .collect();
        if facts.is_empty() {
            return "I don't have enough workspace context to answer that.".into();
        }
        // Facts arrive ranked by the engine; keep that order, skip unrelated ones.
        let picked: Vec<&str> = facts
            .iter()
            .filter(|f| {
                let terms = tokenize(f);
                q_terms.iter().any(|t| terms.contains(t))
            })
            .take(4)
            .copied()
            .collect();
        let picked = if picked.is_empty() { facts.iter().take(3).copied().collect() } else { picked };
        format!("Based on the workspace context:\n• {}", picked.join("\n• "))
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn model(&self) -> &str {
        "mock-extractive-1"
    }

    async fn complete(&self, req: &LlmRequest) -> Result<LlmResponse> {
        let text = Self::answer(req);
        let prompt: usize = estimate(&req.system) + req.messages.iter().map(|m| estimate(&m.content)).sum::<usize>();
        Ok(LlmResponse {
            usage: TokenUsage {
                prompt_tokens: prompt as u64,
                completion_tokens: estimate(&text) as u64,
                estimated: true,
                ..Default::default()
            },
            text,
            model: self.model().into(),
            finish_reason: Some("stop".into()),
        })
    }

    async fn stream(&self, req: &LlmRequest) -> Result<ChunkStream> {
        let resp = self.complete(req).await?;
        let mut chunks: Vec<Result<StreamChunk>> =
            resp.text.split_inclusive(' ').map(|w| Ok(StreamChunk::Delta(w.to_string()))).collect();
        chunks.push(Ok(StreamChunk::Done(resp)));
        Ok(Box::pin(futures::stream::iter(chunks)))
    }
}
