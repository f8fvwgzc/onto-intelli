//! Chat conversations. Turns live in the KV store (not the graph); the thread
//! itself is a `conversation` entity linked `about` the things it discussed.
//! Old turns fold into a deterministic rolling summary, so history costs a
//! bounded number of tokens without an extra LLM call.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use onto_core::store::Store;
use onto_core::time::{new_id, now_ms};
use onto_core::{Error, Id, Principal, Result};

use crate::llm::ChatMessage;
use crate::memory::clip;

const NS: &str = "conversation";
const SUMMARY_MAX_CHARS: usize = 1600;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub role: String,
    pub text: String,
    pub ts: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<Id>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub workspace: String,
    pub principal: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub turns: Vec<Turn>,
    /// Everything this conversation has been about.
    #[serde(default)]
    pub topics: Vec<Id>,
}

impl Conversation {
    /// Entities from the most recent turn that mentioned any (pronoun carry-over).
    pub fn last_entities(&self) -> Vec<Id> {
        self.turns.iter().rev().find(|t| !t.entities.is_empty()).map(|t| t.entities.clone()).unwrap_or_default()
    }

    /// Rolling summary plus the last `n` exchanges as chat messages.
    pub fn window(&self, n: usize) -> (String, Vec<ChatMessage>) {
        let start = self.turns.len().saturating_sub(n * 2);
        let messages = self.turns[start..]
            .iter()
            .map(|t| {
                let text = clip(&t.text, 700);
                if t.role == "user" { ChatMessage::user(text) } else { ChatMessage::assistant(text) }
            })
            .collect();
        (self.summary.clone(), messages)
    }
}

pub struct Conversations {
    store: Arc<dyn Store>,
}

impl Conversations {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    pub fn get(&self, p: &Principal, id: &str) -> Result<Conversation> {
        let conv: Conversation = match self.store.kv_get(NS, id)? {
            Some(v) => serde_json::from_value(v)?,
            None => return Err(Error::not_found(format!("conversation {id}"))),
        };
        if conv.principal != p.id && !p.is_privileged() {
            return Err(Error::not_found(format!("conversation {id}")));
        }
        Ok(conv)
    }

    /// Continue `id`, or start a new conversation when `id` is `None`/`"new"`.
    pub fn open(&self, p: &Principal, ws: &str, id: Option<&str>) -> Result<Conversation> {
        match id.filter(|s| !s.is_empty() && *s != "new") {
            Some(id) => match self.get(p, id) {
                Ok(c) if c.workspace != ws => {
                    Err(Error::invalid(format!("conversation {id} belongs to workspace {}", c.workspace)))
                }
                Ok(c) => Ok(c),
                Err(Error::NotFound(_)) if p.in_workspace(ws) => Ok(self.fresh(p, ws, Some(id.to_string()))),
                Err(e) => Err(e),
            },
            None => Ok(self.fresh(p, ws, None)),
        }
    }

    fn fresh(&self, p: &Principal, ws: &str, id: Option<String>) -> Conversation {
        let now = now_ms();
        Conversation {
            id: id.unwrap_or_else(|| new_id("conv")),
            workspace: ws.to_string(),
            principal: p.id.clone(),
            created_at: now,
            updated_at: now,
            summary: String::new(),
            turns: Vec::new(),
            topics: Vec::new(),
        }
    }

    /// Append an exchange; returns topics seen for the first time.
    pub fn append(
        &self,
        conv: &mut Conversation,
        question: &str,
        answer: &str,
        entities: &[Id],
        keep_turns: usize,
    ) -> Result<Vec<Id>> {
        let now = now_ms();
        conv.turns.push(Turn { role: "user".into(), text: question.to_string(), ts: now, entities: entities.to_vec() });
        conv.turns.push(Turn { role: "assistant".into(), text: answer.to_string(), ts: now, entities: Vec::new() });
        let keep = keep_turns.max(1) * 2;
        if conv.turns.len() > keep {
            let overflow: Vec<Turn> = conv.turns.drain(..conv.turns.len() - keep).collect();
            for pair in overflow.chunks(2) {
                let q = pair.first().map(|t| clip(&t.text, 90)).unwrap_or_default();
                let a = pair.get(1).map(|t| clip(&t.text, 140)).unwrap_or_default();
                conv.summary.push_str(&format!("\n- Q: {q} → A: {a}"));
            }
            while conv.summary.len() > SUMMARY_MAX_CHARS {
                match conv.summary[1..].find("\n- ") {
                    Some(i) => conv.summary.drain(..i + 1),
                    None => break,
                };
            }
        }
        let new_topics: Vec<Id> = entities.iter().filter(|e| !conv.topics.contains(e)).cloned().collect();
        conv.topics.extend(new_topics.iter().cloned());
        conv.updated_at = now;
        self.store.kv_put(NS, &conv.id, &serde_json::to_value(&*conv)?, None)?;
        Ok(new_topics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onto_core::store::MemoryStore;

    #[test]
    fn folds_old_turns_into_summary() {
        let c = Conversations::new(Arc::new(MemoryStore::default()));
        let p = Principal::member("a", "w");
        let mut conv = c.open(&p, "w", None).unwrap();
        for i in 0..5 {
            c.append(&mut conv, &format!("question {i}"), &format!("answer {i}"), &[format!("issue:{i}")], 2).unwrap();
        }
        assert_eq!(conv.turns.len(), 4);
        assert!(conv.summary.contains("question 0"));
        let (summary, msgs) = conv.window(2);
        assert!(!summary.is_empty());
        assert_eq!(msgs.len(), 4);
        assert_eq!(conv.last_entities(), vec!["issue:4".to_string()]);
        let reloaded = c.get(&p, &conv.id).unwrap();
        assert_eq!(reloaded.topics.len(), 5);
        assert!(c.get(&Principal::member("b", "w"), &conv.id).is_err());
    }
}
