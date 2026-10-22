//! Competency-question evaluation: run questions with expectations and report
//! pass rate, failures with their evidence, tokens and latency. Use it as a
//! regression gate for schema, data and prompt changes.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use onto_core::{Error, Principal, Result};

use crate::intelli::{AskRequest, Intelli};
use crate::memory::clip;
use crate::trace::TraceCtx;

/// Demo suite for the seeded `acme` workspace.
pub const DEMO_EVALS: &str = include_str!("../templates/evals.toml");

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EvalCase {
    #[serde(default)]
    pub name: Option<String>,
    pub question: String,
    /// Every string must appear in the answer (case-insensitive).
    #[serde(default)]
    pub expect: Vec<String>,
    /// None of these may appear.
    #[serde(default)]
    pub reject: Vec<String>,
    #[serde(default)]
    pub expect_mode: Option<String>,
    #[serde(default)]
    pub expect_intent: Option<String>,
    /// Entities that must be resolved from the question.
    #[serde(default)]
    pub expect_entities: Vec<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Suite {
    #[serde(default, rename = "case")]
    cases: Vec<EvalCase>,
}

pub fn parse_suite(toml_text: &str) -> Result<Vec<EvalCase>> {
    let s: Suite = toml::from_str(toml_text).map_err(|e| Error::Config(format!("eval suite: {e}")))?;
    if s.cases.is_empty() {
        return Err(Error::Config("eval suite has no [[case]] entries".into()));
    }
    Ok(s.cases)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaseResult {
    pub name: String,
    pub question: String,
    pub passed: bool,
    pub failures: Vec<String>,
    pub mode: String,
    pub intent: String,
    pub answer: String,
    pub tokens: u64,
    pub ms: f64,
    /// Facts the answer was based on (the evidence trail).
    pub evidence: Vec<String>,
    /// The case's trace (scored `eval.pass`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EvalReport {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub pass_rate: f64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub avg_ms: f64,
    /// Share of questions answered without the LLM.
    pub deterministic_share: f64,
    pub cases: Vec<CaseResult>,
}

impl Intelli {
    pub async fn eval(&self, p: &Principal, cases: Vec<EvalCase>) -> Result<EvalReport> {
        self.eval_traced(p, cases, &TraceCtx::default()).await
    }

    /// Run the cases; each one is a trace of kind `eval` named `eval.case`
    /// (its tags and session from `ctx`) with an `eval.pass` score of 1 or 0.
    pub async fn eval_traced(&self, p: &Principal, cases: Vec<EvalCase>, ctx: &TraceCtx) -> Result<EvalReport> {
        let mut report = EvalReport { total: cases.len(), ..Default::default() };
        let mut total_ms = 0.0;
        let mut deterministic = 0usize;
        for (i, c) in cases.into_iter().enumerate() {
            let t = Instant::now();
            let req = AskRequest {
                question: c.question.clone(),
                workspace: c.workspace.clone(),
                no_cache: true,
                ..Default::default()
            };
            let name = c.name.clone().unwrap_or_else(|| format!("case {}", i + 1));
            let mut cctx = ctx.clone();
            cctx.otel_trace_id = None;
            cctx.parent_span = None;
            cctx.name = Some("eval.case".into());
            cctx.tags.push("eval".into());
            cctx.metadata = serde_json::json!({ "case": name });
            let traced = self.ask_traced_as(p, req, cctx, "eval").await;
            let trace_id = traced.trace_id;
            let (passed, failures, mode, intent, answer, tokens, evidence) = match traced.result {
                Ok(r) => {
                    let lower = r.answer.to_lowercase();
                    let mut failures = Vec::new();
                    for e in &c.expect {
                        if !lower.contains(&e.to_lowercase()) {
                            failures.push(format!("missing \"{e}\""));
                        }
                    }
                    for x in &c.reject {
                        if lower.contains(&x.to_lowercase()) {
                            failures.push(format!("contains rejected \"{x}\""));
                        }
                    }
                    let mode = serde_json::to_value(r.mode)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default();
                    if let Some(m) = &c.expect_mode
                        && *m != mode
                    {
                        failures.push(format!("mode {mode}, expected {m}"));
                    }
                    let intent = r.intent.as_str().to_string();
                    if let Some(want) = &c.expect_intent
                        && *want != intent
                    {
                        failures.push(format!("intent {intent}, expected {want}"));
                    }
                    for id in &c.expect_entities {
                        if !r.entities.iter().any(|e| &e.id == id) {
                            failures.push(format!("did not resolve {id}"));
                        }
                    }
                    if mode == "deterministic" {
                        deterministic += 1;
                    }
                    report.prompt_tokens += r.usage.prompt_tokens;
                    report.completion_tokens += r.usage.completion_tokens;
                    (
                        failures.is_empty(),
                        failures,
                        mode,
                        intent,
                        clip(&r.answer, 300),
                        r.usage.total(),
                        r.facts.iter().take(5).cloned().collect(),
                    )
                }
                Err(e) => (
                    false,
                    vec![format!("error: {e}")],
                    "error".to_string(),
                    String::new(),
                    String::new(),
                    0,
                    Vec::new(),
                ),
            };
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            total_ms += ms;
            if let Some(id) = &trace_id {
                self.send_score(onto_core::trace::ScoreRow {
                    id: onto_core::time::new_id("score"),
                    trace_id: id.clone(),
                    ts: onto_core::time::now_ms(),
                    name: "eval.pass".into(),
                    value: Some(if passed { 1.0 } else { 0.0 }),
                    source: "eval".into(),
                    author: Some(p.id.clone()),
                    comment: (!failures.is_empty()).then(|| clip(&failures.join("; "), 500)),
                    ..Default::default()
                });
            }
            if passed {
                report.passed += 1;
            }
            report.cases.push(CaseResult {
                name,
                question: c.question,
                passed,
                failures,
                mode,
                intent,
                answer,
                tokens,
                ms: (ms * 100.0).round() / 100.0,
                evidence,
                trace_id,
            });
        }
        report.failed = report.total - report.passed;
        if report.total > 0 {
            report.pass_rate = (report.passed as f64 / report.total as f64 * 1000.0).round() / 10.0;
            report.avg_ms = (total_ms / report.total as f64 * 100.0).round() / 100.0;
            report.deterministic_share = (deterministic as f64 / report.total as f64 * 1000.0).round() / 10.0;
        }
        Ok(report)
    }
}
