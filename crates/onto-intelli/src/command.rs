//! Natural-language commands → action-type parameters → governed proposal.
//!
//! "assign issue 433 to Aiko" fills `assign(item = issue:433, assignee = person:aiko)`
//! from resolved entities (kind compatibility, preposition cues, the action's
//! primary parameter), quoted text, state words and numbers. Commands are
//! proposals until confirmed: a person (or an agent with authority) commits.

use serde_json::{Map, Value, json};

use onto_core::action_types::{ActionReceipt, ActionType, ApplyOptions, ParamType};
use onto_core::{ActionContext, Ontology, Principal, Result, Schema};

use crate::answer::hypothetical_state;
use crate::resolve::{Resolved, words};

#[derive(Clone, Debug)]
pub struct CommandOutcome {
    pub answer: String,
    pub facts: Vec<String>,
    pub receipt: ActionReceipt,
}

fn quoted(question: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = question.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let close = match chars[i] {
            '"' => Some('"'),
            '\'' if i == 0 || chars[i - 1].is_whitespace() => Some('\''),
            '“' => Some('”'),
            '‘' => Some('’'),
            _ => None,
        };
        if let Some(close) = close
            && let Some(end) = chars[i + 1..].iter().position(|c| *c == close)
        {
            let text: String = chars[i + 1..i + 1 + end].iter().collect();
            if !text.trim().is_empty() {
                out.push(text.trim().to_string());
            }
            i += end + 2;
            continue;
        }
        i += 1;
    }
    out
}

/// Fill parameters for `at` from the question and its resolved entities.
pub fn fill_params(schema: &Schema, at: &ActionType, question: &str, entities: &[Resolved]) -> Map<String, Value> {
    let ws = words(question);
    let mut params = Map::new();
    let primary = at.primary_param();
    let mut ordered: Vec<&Resolved> = entities.iter().filter(|r| r.position != usize::MAX).collect();
    ordered.sort_by_key(|r| r.position);
    for r in ordered {
        let prev = r.position.checked_sub(1).and_then(|i| ws.get(i)).map(String::as_str).unwrap_or("");
        let prev2 = r.position.checked_sub(2).and_then(|i| ws.get(i)).map(String::as_str).unwrap_or("");
        let candidates: Vec<(&String, _)> = at
            .parameters
            .iter()
            .filter(|(name, def)| def.ty == ParamType::Entity && !params.contains_key(*name))
            .filter(|(_, def)| def.kinds.is_empty() || def.kinds.iter().any(|k| schema.kind_is(&r.kind, k)))
            .collect();
        let cued = candidates.iter().find(|(_, d)| d.cue.iter().any(|c| c == prev || c == prev2));
        let chosen = cued
            .or_else(|| candidates.iter().find(|(n, _)| primary.as_deref() == Some(n.as_str())))
            .or_else(|| candidates.iter().find(|(_, d)| d.cue.is_empty()))
            .or_else(|| candidates.first());
        if let Some((name, _)) = chosen {
            params.insert((*name).clone(), json!(r.id));
        }
    }
    let texts = quoted(question);
    let mut text_iter = texts.iter();
    let lower = question.to_lowercase();
    for (name, def) in &at.parameters {
        if params.contains_key(name) {
            continue;
        }
        match def.ty {
            ParamType::String | ParamType::Text if name.contains("state") => {
                let item_kind = params
                    .values()
                    .filter_map(Value::as_str)
                    .find_map(|id| entities.iter().find(|r| r.id == id).map(|r| r.kind.clone()));
                let joined = lower.replace("in progress", "in_progress");
                let mut cands: Vec<&str> = joined.split_whitespace().collect();
                // Prefer the word right after a cue ("as done", "to blocked").
                if let Some(i) = cands.iter().position(|w| def.cue.iter().any(|c| c == w)) {
                    cands = cands.split_off(i + 1);
                }
                let state = cands
                    .iter()
                    .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '_'))
                    .find_map(|w| item_kind.as_deref().and_then(|k| hypothetical_state(schema, k, w)));
                if let Some(s) = state {
                    params.insert(name.clone(), json!(s));
                }
            }
            ParamType::String | ParamType::Text => {
                if let Some(t) = text_iter.next() {
                    params.insert(name.clone(), json!(t));
                }
            }
            ParamType::Int | ParamType::Float => {
                let toks: Vec<&str> = lower.split_whitespace().collect();
                let num = toks.iter().enumerate().find_map(|(i, w)| {
                    let n: f64 = w.trim_end_matches(['d', ',', '.']).parse().ok()?;
                    let before = i.checked_sub(1).map(|j| toks[j]).unwrap_or("");
                    let after = toks.get(i + 1).copied().unwrap_or("");
                    (before.starts_with(name.as_str()) || after.starts_with("day") || w.ends_with('d')).then_some(n)
                });
                if let Some(n) = num {
                    params.insert(name.clone(), if def.ty == ParamType::Int { json!(n as i64) } else { json!(n) });
                }
            }
            ParamType::Enum => {
                if let Some(v) = def.values.iter().find(|v| {
                    lower.split_whitespace().any(|w| w.trim_matches(|c: char| !c.is_alphanumeric()) == v.as_str())
                }) {
                    params.insert(name.clone(), json!(v));
                }
            }
            _ => {}
        }
    }
    params
}

/// Plan (and, when `confirm`, apply) the action an imperative describes.
pub fn run(
    onto: &Ontology,
    p: &Principal,
    ws: &str,
    action: &str,
    question: &str,
    entities: &[Resolved],
    confirm: bool,
) -> Result<CommandOutcome> {
    let schema = onto.schema();
    let at = schema.action_types.get(action).cloned().ok_or_else(|| onto_core::Error::not_found(action))?;
    let params = fill_params(schema, &at, question, entities);
    let ctx = ActionContext::new(p.clone(), ws).with_source("command").with_intent(format!("command:{action}"));
    let receipt = onto.apply_action(&ctx, action, &params, ApplyOptions { dry_run: !confirm, confirm })?;
    let plan = &receipt.plan;
    let shown: Vec<String> = plan
        .params
        .iter()
        .map(|(k, v)| format!("{k}={}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
        .collect();
    let preview =
        if plan.preview.is_empty() { String::new() } else { format!("\n• {}", plan.preview.join("\n• ")) };
    let answer = match receipt.status.as_str() {
        "applied" => format!(
            "Done: {action}({}).{preview}\nReceipt {} — undo with action.undo{}.",
            shown.join(", "),
            receipt.id,
            if plan.reversible { "" } else { " (not fully reversible)" }
        ),
        "rejected" => format!("Cannot {action}: {}.", receipt.reason.clone().unwrap_or_default()),
        _ => format!(
            "Proposed {action}({}) — risk {:?}, {}.{preview}\nConfirm to apply (confirm=true).",
            shown.join(", "),
            plan.risk,
            if plan.reversible { "reversible" } else { "not reversible" }
        ),
    };
    let mut facts = plan.preview.clone();
    facts.extend(plan.checks.iter().filter(|c| !c.ok).map(|c| format!("failed check: {}", c.message)));
    Ok(CommandOutcome { answer, facts, receipt })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(id: &str, pos: usize) -> Resolved {
        Resolved {
            id: id.into(),
            kind: id.split(':').next().unwrap().into(),
            title: id.into(),
            mention: id.into(),
            confidence: 1.0,
            position: pos,
        }
    }

    #[test]
    fn fills_parameters_from_cues_kinds_and_text() {
        let s = Schema::enterprise();
        let assign = &s.action_types["assign"];
        let p = fill_params(&s, assign, "assign issue 433 to aiko", &[r("issue:433", 1), r("person:aiko", 4)]);
        assert_eq!(p["item"], "issue:433");
        assert_eq!(p["assignee"], "person:aiko");
        let block = &s.action_types["block"];
        let p = fill_params(&s, block, "block task 34 on issue 433", &[r("task:34", 1), r("issue:433", 4)]);
        assert_eq!(p["item"], "task:34");
        assert_eq!(p["blocker"], "issue:433");
        let set = &s.action_types["set_state"];
        let p = fill_params(&s, set, "mark task 32 as done", &[r("task:32", 1)]);
        assert_eq!(p["state"], "done");
        let p = fill_params(&s, set, "mark issue 9 as fixed", &[r("issue:9", 1)]);
        assert_eq!(p["state"], "resolved");
        let create = &s.action_types["create_task"];
        let p = fill_params(
            &s,
            create,
            "create task \"Write docs\" in project alpha estimate 2 high",
            &[r("project:alpha", 5)],
        );
        assert_eq!(p["title"], "Write docs");
        assert_eq!(p["project"], "project:alpha");
        assert_eq!(p["estimate"], 2.0);
        assert_eq!(p["priority"], "high");
    }
}
