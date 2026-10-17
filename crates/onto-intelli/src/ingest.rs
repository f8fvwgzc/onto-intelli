//! Asynchronous ingestion: the slow path. Webhooks, connectors and bulk loads
//! enqueue changes; a background worker applies them in order, so queries
//! (the fast path) never wait on ingestion.

use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use onto_core::{
    ActionContext, EntityInput, Error, EventInput, LinkInput, Mutation, MutationResult, Ontology, Provenance, Result,
};

use crate::memory::clip;

struct Job {
    ctx: ActionContext,
    muts: Vec<Mutation>,
    ack: Option<oneshot::Sender<Result<MutationResult>>>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct IngestStats {
    pub enqueued_jobs: u64,
    pub applied_jobs: u64,
    pub applied_mutations: u64,
    pub failed_jobs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IngestReceipt {
    pub accepted: usize,
    /// true: queued for background application; false: applied before returning.
    pub queued: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<MutationResult>,
}

#[derive(Clone)]
pub struct IngestQueue {
    tx: mpsc::Sender<Job>,
    stats: Arc<Mutex<IngestStats>>,
}

impl IngestQueue {
    /// Start the worker on the current Tokio runtime.
    pub fn spawn(onto: Ontology, capacity: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<Job>(capacity.max(1));
        let stats = Arc::new(Mutex::new(IngestStats::default()));
        let st = stats.clone();
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                let onto = onto.clone();
                let n = job.muts.len() as u64;
                let (ctx, muts) = (job.ctx, job.muts);
                let res = tokio::task::spawn_blocking(move || onto.apply(&ctx, muts))
                    .await
                    .unwrap_or_else(|e| Err(Error::Other(format!("ingest worker panicked: {e}"))));
                {
                    let mut s = st.lock();
                    match &res {
                        Ok(_) => {
                            s.applied_jobs += 1;
                            s.applied_mutations += n;
                        }
                        Err(e) => {
                            s.failed_jobs += 1;
                            s.last_error = Some(e.to_string());
                            tracing::warn!("ingest job failed: {e}");
                        }
                    }
                }
                if let Some(ack) = job.ack {
                    let _ = ack.send(res);
                }
            }
        });
        Self { tx, stats }
    }

    /// Fire and forget (bounded: waits only when the queue is full).
    pub async fn enqueue(&self, ctx: ActionContext, muts: Vec<Mutation>) -> Result<IngestReceipt> {
        let accepted = muts.len();
        self.tx
            .send(Job { ctx, muts, ack: None })
            .await
            .map_err(|_| Error::Unavailable("ingest queue closed".into()))?;
        self.stats.lock().enqueued_jobs += 1;
        Ok(IngestReceipt { accepted, queued: true, result: None })
    }

    /// Apply through the queue (preserving order) and wait for the result.
    pub async fn apply(&self, ctx: ActionContext, muts: Vec<Mutation>) -> Result<MutationResult> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Job { ctx, muts, ack: Some(tx) })
            .await
            .map_err(|_| Error::Unavailable("ingest queue closed".into()))?;
        self.stats.lock().enqueued_jobs += 1;
        rx.await.map_err(|_| Error::Unavailable("ingest worker stopped".into()))?
    }

    /// Wait until everything enqueued so far has been applied.
    pub async fn flush(&self) -> Result<()> {
        self.apply(ActionContext::system("default"), Vec::new()).await.map(|_| ())
    }

    pub fn stats(&self) -> IngestStats {
        self.stats.lock().clone()
    }
}

/// Turn an inbound record into mutations. Accepts:
/// * a mutation: `{"op": "upsert", ...}`, `{"op": "link", ...}`, `{"op": "event", ...}`
/// * a bare entity: `{"id": "issue:421", "title": "...", ...}`
/// * a GitHub `issues` webhook payload
pub fn normalize_record(v: Value) -> Result<Vec<Mutation>> {
    if v.get("op").is_some() {
        return Ok(vec![serde_json::from_value::<Mutation>(v)?]);
    }
    if v.get("issue").is_some() && v.get("repository").is_some() {
        return github_issue(&v);
    }
    if v.get("kind").is_some() || v.get("id").is_some() {
        return Ok(vec![Mutation::Upsert(serde_json::from_value::<EntityInput>(v)?)]);
    }
    Err(Error::invalid("unrecognized record: expected a mutation (op), an entity (id/kind) or a GitHub issues webhook"))
}

fn github_issue(v: &Value) -> Result<Vec<Mutation>> {
    let repo = v["repository"]["full_name"].as_str().ok_or_else(|| Error::invalid("repository.full_name missing"))?;
    let number = v["issue"]["number"].as_u64().ok_or_else(|| Error::invalid("issue.number missing"))?;
    let repo_key = repo.replace('/', "-").to_lowercase();
    let issue_id = format!("issue:{repo_key}-{number}");
    let repo_id = format!("repository:{repo_key}");
    let url = v["issue"]["html_url"].as_str().map(str::to_string);
    let state = if v["issue"]["state"].as_str() == Some("closed") { "closed" } else { "open" };

    let mut muts = vec![Mutation::Upsert(EntityInput {
        id: Some(repo_id.clone()),
        title: Some(repo.to_string()),
        fill_only: true,
        props: serde_json::from_value(json!({ "url": v["repository"]["html_url"] })).unwrap_or_default(),
        ..Default::default()
    })];
    let mut issue = EntityInput {
        id: Some(issue_id.clone()),
        title: v["issue"]["title"].as_str().map(str::to_string),
        state: Some(state.into()),
        provenance: Some(Provenance { source: "github".into(), external_ref: url.clone(), ..Default::default() }),
        ..Default::default()
    };
    if let Some(body) = v["issue"]["body"].as_str() {
        issue.props.insert("description".into(), json!(clip(body, 2000)));
    }
    issue.links.push(onto_core::LinkSpec { rel: "affects".into(), to: repo_id, props: Default::default() });
    muts.push(Mutation::Upsert(issue));
    for a in v["issue"]["assignees"].as_array().into_iter().flatten() {
        if let Some(login) = a["login"].as_str() {
            let person = format!("person:{}", login.to_lowercase());
            muts.push(Mutation::Upsert(EntityInput {
                id: Some(person.clone()),
                title: Some(login.to_string()),
                aliases: vec![login.to_string()],
                fill_only: true,
                ..Default::default()
            }));
            muts.push(Mutation::Link(LinkInput::new(issue_id.clone(), "assigned_to", person)));
        }
    }
    muts.push(Mutation::Event(EventInput {
        action: format!("github.issues.{}", v["action"].as_str().unwrap_or("event")),
        actor: v["sender"]["login"].as_str().map(|s| format!("github:{s}")),
        target: Some(issue_id),
        source: Some("github".into()),
        payload: json!({ "number": number, "url": url }),
        ..Default::default()
    }));
    Ok(muts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_records() {
        let m = normalize_record(json!({"op": "upsert", "kind": "issue", "key": "1", "title": "x"})).unwrap();
        assert!(matches!(m[0], Mutation::Upsert(_)));
        let m = normalize_record(json!({"id": "task:9", "title": "y"})).unwrap();
        assert!(matches!(m[0], Mutation::Upsert(_)));
        let gh = json!({
            "action": "opened",
            "issue": {"number": 7, "title": "Crash", "state": "open", "assignees": [{"login": "Kenji"}], "html_url": "https://x"},
            "repository": {"full_name": "acme/pay", "html_url": "https://y"},
            "sender": {"login": "sarah"}
        });
        let m = normalize_record(gh).unwrap();
        assert_eq!(m.len(), 5);
        assert!(normalize_record(json!({"nope": 1})).is_err());
    }
}
