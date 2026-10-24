//! Rule-based inference: a chain of relations derives a new relation.
//!
//! `chain = ["assigned_to", "member_of"]`, `infer = "handled_by_team"` means
//! `issue -assigned_to-> person -member_of-> team` ⇒ `issue -handled_by_team-> team`.
//! Inferred links are computed on demand and never stored, so they are always
//! consistent with the current graph. This is the per-entity walk used by
//! `ask` and context building; the whole-graph reasoner with certificates
//! (which also evaluates Horn-form rules and transitive link types) is
//! [`crate::reason`].

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::graph::Graph;
use crate::model::{Entity, Id};
use crate::query::Hop;
use crate::schema::Schema;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inferred {
    pub from: Id,
    pub rel: String,
    pub to: Id,
    pub rule: String,
    /// The walk that justifies the inference.
    pub because: Vec<Hop>,
}

const MAX_FRONTIER: usize = 256;

pub fn infer(schema: &Schema, g: &Graph, start: &str, can_read: &dyn Fn(&Entity) -> bool) -> Vec<Inferred> {
    let mut out = Vec::new();
    let mut emitted: HashSet<(String, String)> = HashSet::new();
    // Horn-form rules (`when`/`then`) are evaluated by the reasoner only.
    for rule in schema.rules.iter().filter(|r| !r.chain.is_empty()) {
        let mut frontier: Vec<(String, Vec<Hop>)> = vec![(start.to_string(), Vec::new())];
        for step in &rule.chain {
            let (base, dir) = schema.resolve_rel_lenient(step);
            let mut next = Vec::new();
            for (node, path) in &frontier {
                for s in g.steps_via(node, &base, dir) {
                    if !g.get(s.other).is_some_and(can_read) || path.iter().any(|h| h.from == s.other) {
                        continue;
                    }
                    let mut p = path.clone();
                    p.push(Hop { from: node.clone(), rel: schema.display_rel(s.rel, s.dir), to: s.other.to_string() });
                    next.push((s.other.to_string(), p));
                    if next.len() >= MAX_FRONTIER {
                        break;
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        for (end, because) in frontier {
            if end != start && emitted.insert((rule.infer.clone(), end.clone())) {
                out.push(Inferred {
                    from: start.to_string(),
                    rel: rule.infer.clone(),
                    to: end,
                    rule: rule.name.clone(),
                    because,
                });
            }
        }
    }
    out
}
