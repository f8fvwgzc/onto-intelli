//! Question Context Graph: the small, relevant, permission-filtered slice of
//! the ontology that an LLM needs to answer one question, packed into a token
//! budget as compact fact lines.
//!
//! After packing, every packed `derived` fact (a relation an inference rule
//! derived) is checked for preservation: the links its derivation walks (its
//! premises) must be stated by packed facts too, or the model is handed a
//! conclusion without the facts it rests on. A claim whose premises are all
//! packed is `preserved`. When some are missing and the remaining budget
//! allows, compact `premise` facts are appended for them (`repaired`);
//! otherwise the claim is `lost`. The result is reported in
//! [`ContextPack::preservation`]. Packing itself is untouched: the facts it
//! kept stay in the same order with the same text, and premise facts only
//! ever follow them.
//!
//! A link counts as stated only when a packed fact spells it with entity ids:
//! a `relations` line (every link between packed nodes, grouped by its
//! subject), a provenance line, or a premise fact. Cause chains name entities
//! by title, so they do not count. The check is a measurement of the packed
//! text against the engine's own derivation: the derived facts are the
//! engine's opinion (`derived`), not checked certificates.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use onto_core::deps::{self, DepNode};
use onto_core::policy;
use onto_core::query::{Edge, Hop};
use onto_core::rules;
use onto_core::schema::Dir;
use onto_core::time::fmt_date;
use onto_core::{Entity, EventFilter, Id, Ontology, Principal, Schema};

use crate::config::ContextConfig;
use crate::intent::{Classified, Intent};
use crate::memory::{MemoryHit, MemoryScope, clip};
use crate::resolve::Resolved;
use crate::token::{Packable, estimate, pack};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CtxNode {
    pub id: Id,
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub score: f32,
    pub hop: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fact {
    pub text: String,
    pub score: f32,
    pub tokens: usize,
    /// entity | relations | derived | cause | impact | event | knowledge | catalog |
    /// memory | premise (a link a packed derived fact rests on, added after packing)
    pub source: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextPack {
    pub question: String,
    pub intent: String,
    pub seeds: Vec<Id>,
    pub nodes: Vec<CtxNode>,
    pub edges: Vec<Edge>,
    /// Facts that made it into the budget, best first.
    pub facts: Vec<Fact>,
    /// The facts as prompt text.
    pub text: String,
    pub tokens: usize,
    /// Tokens of every candidate fact before budget packing.
    pub candidate_tokens: usize,
    /// Tokens the same neighbourhood would cost as raw JSON rows.
    pub naive_tokens: usize,
    pub dropped_facts: usize,
    pub revision: u64,
    /// Whether the packed derived facts kept the facts they rest on. Absent
    /// when no derived fact was packed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preservation: Option<Preservation>,
}

/// Preservation of the packed derived facts (claims) under token packing.
///
/// `claims == preserved + repaired + lost.len()`. This is a measurement of
/// the packed text, not a proof.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preservation {
    /// Packed derived facts checked.
    pub claims: usize,
    /// Claims whose premises were all stated by facts that packing kept.
    pub preserved: usize,
    /// Claims whose missing premises were added as `premise` facts.
    pub repaired: usize,
    /// Claims whose missing premises did not fit the remaining budget, spelled
    /// `"<from> <relation> <to>"`, in packed order.
    pub lost: Vec<String>,
    pub means: String,
}

/// What [`Preservation::means`] says.
pub const PRESERVATION_MEANS: &str = "measured: a derived fact is preserved when every link its derivation \
walks is spelled by a packed fact (a relations line, a provenance line or a premise fact), repaired when the missing \
links were appended as premise facts within the token budget, and lost when they did not fit; the derived facts are \
the engine's opinion, not checked certificates";

/// A link stated in canonical direction: (from, relation, to).
type Link = (String, String, String);

/// One link a derived fact rests on.
#[derive(Clone, Debug)]
struct Premise {
    /// Canonical spellings that state it (two for a symmetric relation).
    links: Vec<Link>,
    /// The compact premise fact added when it is missing.
    text: String,
}

/// A derived fact and the links its derivation walked.
#[derive(Clone, Debug)]
struct Claim {
    spelled: String,
    premises: Vec<Premise>,
}

/// What a candidate fact contributes to the preservation check.
#[derive(Clone, Debug, Default)]
enum Support {
    #[default]
    None,
    /// The fact spells these links.
    States(Vec<Link>),
    /// The fact is a derived claim.
    Claims(Claim),
}

/// The premise for one hop of a derivation, in canonical direction.
fn premise(schema: &Schema, hop: &Hop) -> Premise {
    let (base, dir) = schema.resolve_rel_lenient(&hop.rel);
    let out: Link = (hop.from.clone(), base.clone(), hop.to.clone());
    let inc: Link = (hop.to.clone(), base, hop.from.clone());
    let links = match dir {
        Dir::Out => vec![out],
        Dir::In => vec![inc],
        Dir::Both => vec![out, inc],
    };
    let (from, rel, to) = &links[0];
    Premise { text: format!("premise: {from} {rel} → {to}"), links }
}

pub struct ContextInput<'a> {
    pub question: &'a str,
    pub classified: &'a Classified,
    pub seeds: &'a [Resolved],
    pub memories: &'a [MemoryHit],
    pub budget: usize,
}

/// Relation boost per intent: what matters for this kind of question.
fn boost(intent: Intent, rel: &str, provenance_question: bool) -> f32 {
    let table: &[(&str, f32)] = match intent {
        Intent::Owner => &[("owns", 1.5), ("belongs_to", 1.2), ("member_of", 1.1), ("manages", 1.2)],
        Intent::Assignee => &[("assigned_to", 1.6), ("member_of", 1.3), ("manages", 1.1), ("affects", 1.1)],
        Intent::Blockers | Intent::Dependencies | Intent::Status => &[
            ("blocked_by", 1.6),
            ("depends_on", 1.5),
            ("affects", 1.3),
            ("deploys", 1.3),
            ("part_of", 1.2),
            ("belongs_to", 1.1),
        ],
        Intent::Impact => &[("blocked_by", 1.5), ("depends_on", 1.5), ("part_of", 1.2), ("belongs_to", 1.2)],
        Intent::Members | Intent::Workload => &[("member_of", 1.6), ("assigned_to", 1.4), ("manages", 1.2)],
        _ => &[],
    };
    let base = table.iter().find(|(r, _)| *r == rel).map(|(_, b)| *b).unwrap_or(1.0);
    if provenance_question && matches!(rel, "derived_from" | "generated_by") { base * 1.6 } else { base }
}

fn node_fact(schema: &Schema, e: &Entity) -> String {
    let mut s = format!("[{}] {}", e.id, e.kind);
    if !e.title.is_empty() {
        s.push_str(&format!(" \"{}\"", e.title));
    }
    let mut attrs: Vec<String> = Vec::new();
    if let Some(state) = &e.state {
        let flag = if schema.is_alert(&e.kind, Some(state)) { " (!)" } else { "" };
        attrs.push(format!("state: {state}{flag}"));
    }
    for (k, v) in e.props.iter().filter(|(k, _)| !matches!(k.as_str(), "visibility" | "acl")).take(6) {
        let val = match v {
            Value::String(t) => clip(t, 160),
            Value::Array(items) => items
                .iter()
                .map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()))
                .collect::<Vec<_>>()
                .join(", "),
            other => other.to_string(),
        };
        attrs.push(format!("{k}: {val}"));
    }
    if let Some(p) = &e.provenance {
        let mut prov = format!("provenance: {}", if p.source.is_empty() { "unknown" } else { &p.source });
        if !p.derived_from.is_empty() {
            prov.push_str(&format!(", derived from {}", p.derived_from.join(", ")));
        }
        if let Some(g) = &p.generated_by {
            prov.push_str(&format!(", generated by {g}"));
        }
        attrs.push(prov);
    }
    attrs.push(format!("updated {}", fmt_date(e.updated_at)));
    s.push_str(" — ");
    s.push_str(&attrs.join("; "));
    s
}

fn is_evidence(e: &onto_core::Event) -> bool {
    match e.action.as_str() {
        a if a.starts_with("intelli.") => false,
        "entity.create" | "link.create" | "link.delete" => false,
        "entity.update" => e.payload["changed"].as_array().is_some_and(|c| c.iter().any(|f| f == "state")),
        _ => true,
    }
}

fn dep_label(n: &DepNode) -> String {
    match &n.state {
        Some(s) => format!("{} [{}] ({s})", n.title, n.id),
        None => format!("{} [{}]", n.title, n.id),
    }
}

pub fn build(onto: &Ontology, p: &Principal, ws: &str, cfg: &ContextConfig, input: ContextInput<'_>) -> ContextPack {
    let intent = input.classified.intent;
    let q_lower = input.question.to_lowercase();
    let provenance_q =
        ["generated", "generate", "created", "derived", "source", "come from", "came from", "provenance", "where did"]
            .iter()
            .any(|w| q_lower.contains(w));
    let depth = cfg.max_depth + usize::from(matches!(intent, Intent::Blockers | Intent::Impact | Intent::Dependencies));
    let seed_ids: Vec<Id> = input.seeds.iter().map(|r| r.id.clone()).collect();
    let mut facts: Vec<(Fact, bool)> = Vec::new();
    // Preservation support of a candidate fact, by its index in `facts`.
    let mut support: BTreeMap<usize, Support> = BTreeMap::new();
    let add = |facts: &mut Vec<(Fact, bool)>, text: String, score: f32, source: &str, pinned: bool| -> usize {
        let tokens = estimate(&text) + 1;
        facts.push((Fact { text, score, tokens, source: source.to_string() }, pinned));
        facts.len() - 1
    };

    // Graph work happens under one read lock; no other ontology call inside.
    let (nodes, edges, naive_tokens) = onto.read(|g, schema| {
        let can = |e: &Entity| policy::visible(p, e, g) && e.workspace == ws;
        let mut score: HashMap<String, (f32, usize)> = HashMap::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        for r in input.seeds {
            if g.get(&r.id).is_some_and(can) {
                score.insert(r.id.clone(), (r.confidence.max(0.5), 0));
                queue.push_back(r.id.clone());
            }
        }
        while let Some(id) = queue.pop_front() {
            let (s, hop) = score[&id];
            if hop >= depth {
                continue;
            }
            for step in g.steps(&id) {
                let Some(other) = g.get(step.other) else { continue };
                if !can(other) {
                    continue;
                }
                let ns = s * schema.weight(step.rel) * boost(intent, step.rel, provenance_q) * 0.6;
                match score.get_mut(step.other) {
                    Some(prev) if prev.0 >= ns => {}
                    Some(prev) => prev.0 = ns,
                    None => {
                        score.insert(step.other.to_string(), (ns, hop + 1));
                        queue.push_back(step.other.to_string());
                    }
                }
            }
        }
        let mut ranked: Vec<(String, f32, usize)> = score.into_iter().map(|(k, (s, h))| (k, s, h)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(cfg.max_nodes.max(1));
        let included: HashMap<&str, (f32, usize)> = ranked.iter().map(|(k, s, h)| (k.as_str(), (*s, *h))).collect();

        let mut nodes = Vec::new();
        let mut naive = 0usize;
        // OntoRank lifts authoritative entities when the budget is tight.
        let authority = onto.authority_scores();
        for (id, s, hop) in &ranked {
            let e = g.get(id).expect("ranked ids exist");
            naive += estimate(&serde_json::to_string(e).unwrap_or_default());
            let mut fact = node_fact(schema, e);
            if *hop == 0 {
                let derived: Vec<String> = onto_core::metrics::compute_derived(schema, g, e, &can)
                    .into_iter()
                    .filter(|(k, v)| k != "age_days" && !v.is_null())
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                if !derived.is_empty() {
                    fact.push_str(&format!("; computed: {}", derived.join(", ")));
                }
            }
            let auth = authority.get(id).copied().unwrap_or(0.0);
            add(&mut facts, fact, *s * (1.0 + 0.4 * auth), "entity", *hop == 0);
            nodes.push(CtxNode {
                id: id.clone(),
                kind: e.kind.clone(),
                title: e.label().to_string(),
                state: e.state.clone(),
                score: *s,
                hop: *hop,
            });
        }

        // Relations grouped by subject: "issue:421: assigned_to → person:kenji; affects → service:payment-api".
        let mut edges = Vec::new();
        let mut grouped: BTreeMap<&str, (f32, BTreeMap<&str, Vec<&str>>)> = BTreeMap::new();
        for (id, _, _) in &ranked {
            for (rel, to) in g.out_edges(id) {
                let (Some((sa, _)), Some((sb, _))) = (included.get(id.as_str()), included.get(to.as_str())) else {
                    continue;
                };
                edges.push(Edge { from: id.clone(), rel: rel.clone(), to: to.clone() });
                naive += 12;
                let es = sa.min(*sb) * schema.weight(rel);
                let entry = grouped.entry(id.as_str()).or_insert((0.0, BTreeMap::new()));
                entry.0 = entry.0.max(es);
                entry.1.entry(rel.as_str()).or_default().push(to.as_str());
            }
        }
        for (from, (s, rels)) in grouped {
            let parts: Vec<String> = rels.iter().map(|(rel, tos)| format!("{rel} → {}", tos.join(", "))).collect();
            let stated: Vec<Link> = rels
                .iter()
                .flat_map(|(rel, tos)| tos.iter().map(move |to| (from.to_string(), rel.to_string(), to.to_string())))
                .collect();
            let i = add(&mut facts, format!("{from}: {}", parts.join("; ")), s * 0.95 + 0.02, "relations", false);
            support.insert(i, Support::States(stated));
        }

        // Derived relations from inference rules.
        for r in input.seeds.iter().take(4) {
            for inf in rules::infer(schema, g, &r.id, &can).into_iter().take(6) {
                let via: Vec<&str> = inf.because.iter().map(|h| h.rel.as_str()).collect();
                let i = add(
                    &mut facts,
                    format!("derived: {} {} {} (via {})", inf.from, inf.rel, inf.to, via.join(" > ")),
                    0.85 * r.confidence,
                    "derived",
                    false,
                );
                let claim = Claim {
                    spelled: format!("{} {} {}", inf.from, inf.rel, inf.to),
                    premises: inf.because.iter().map(|h| premise(schema, h)).collect(),
                };
                support.insert(i, Support::Claims(claim));
            }
        }

        // Root-cause chains and impact, depending on the question.
        let wants_cause = matches!(intent, Intent::Blockers | Intent::Status | Intent::Dependencies)
            || (input.classified.why && matches!(intent, Intent::Reasoning));
        if wants_cause {
            for r in input.seeds.iter().take(3) {
                let rc = deps::root_cause(schema, g, &r.id, depth + 2, &can);
                for chain in rc.chains.iter().take(4) {
                    let state = chain.culprit.state.as_deref().unwrap_or("?");
                    // Deeper culprits are likelier root causes: rank them first.
                    let score = 1.2 + 0.02 * chain.culprit.depth as f32;
                    add(
                        &mut facts,
                        format!("cause chain: {} [{} is {state}]", chain.text, chain.culprit.id),
                        score,
                        "cause",
                        intent == Intent::Blockers,
                    );
                }
            }
        }
        if intent == Intent::Impact {
            for r in input.seeds.iter().take(2) {
                let rep = deps::impact(schema, g, &r.id, 4, &can);
                if !rep.nodes.is_empty() {
                    let list: Vec<String> = rep.nodes.iter().take(12).map(dep_label).collect();
                    add(&mut facts, format!("impact: if {} fails → {}", r.id, list.join(", ")), 1.15, "impact", true);
                }
            }
        }
        if provenance_q {
            for r in input.seeds.iter().take(2) {
                if let Some(e) = g.get(&r.id) {
                    let steps: Vec<_> = g
                        .steps_via(&r.id, "derived_from", Dir::Out)
                        .into_iter()
                        .chain(g.steps_via(&r.id, "generated_by", Dir::Out))
                        .collect();
                    let chain: Vec<String> = steps.iter().map(|s| format!("{} {}", s.rel, s.other)).collect();
                    if !chain.is_empty() {
                        let stated =
                            steps.iter().map(|s| (e.id.clone(), s.rel.to_string(), s.other.to_string())).collect();
                        let i = add(
                            &mut facts,
                            format!("provenance of {}: {}", e.id, chain.join("; ")),
                            1.1,
                            "derived",
                            true,
                        );
                        support.insert(i, Support::States(stated));
                    }
                }
            }
        }
        (nodes, edges, naive)
    });

    // Recent events about the seeds ("what happened").
    for id in seed_ids.iter().take(3) {
        let filter = EventFilter {
            target: Some(id.clone()),
            workspace: Some(ws.to_string()),
            limit: Some(cfg.recent_events * 4),
            ..Default::default()
        };
        // Domain activity and state changes only: the orchestrator's Q&A audit
        // trail and plain CRUD bookkeeping are not evidence.
        let events = onto.events(p, &filter).unwrap_or_default();
        for ev in events.into_iter().filter(is_evidence).take(cfg.recent_events) {
            let mut detail = String::new();
            if let Some(obj) = ev.payload.as_object() {
                for key in ["reason", "error", "changed", "state", "rel"] {
                    if let Some(v) = obj.get(key).filter(|v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty()))
                    {
                        detail.push_str(&format!(" {key}={}", clip(&v.to_string(), 80)));
                    }
                }
            }
            add(
                &mut facts,
                format!(
                    "event {}: {} {} {} → {}{detail}",
                    fmt_date(ev.ts),
                    ev.actor,
                    ev.action,
                    id,
                    ev.outcome.as_str()
                ),
                0.45,
                "event",
                false,
            );
        }
    }

    // Knowledge that matches the question text.
    let knowledge_kinds: Vec<String> = if onto.schema().is_interface("knowledge_item") {
        vec!["knowledge_item".to_string()]
    } else {
        ["knowledge", "decision", "artifact", "conversation"].map(String::from).to_vec()
    };
    let hits = onto.search(p, input.question, &knowledge_kinds, Some(ws), cfg.knowledge_hits).unwrap_or_default();
    let top = hits.first().map(|h| h.score).unwrap_or(1.0).max(1e-6);
    let known: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    for h in &hits {
        if known.contains(h.entity.id.as_str()) {
            continue;
        }
        let body = ["body", "rationale", "summary", "description"]
            .iter()
            .find_map(|k| h.entity.prop_str(k))
            .unwrap_or_default();
        add(
            &mut facts,
            format!("knowledge [{}] \"{}\": {}", h.entity.id, h.entity.label(), clip(body, 300)),
            0.5 + 0.3 * h.score / top,
            "knowledge",
            false,
        );
    }

    // Data assets (tables, views, certified definitions) that match the
    // question, highest authority first.
    if onto.schema().object_type("table").is_some() {
        let kinds: Vec<String> = ["table", "view", "learned_fact"].map(String::from).to_vec();
        let assets = onto.search(p, input.question, &kinds, Some(ws), cfg.knowledge_hits).unwrap_or_default();
        let scores = onto.authority_scores();
        let top = assets.first().map(|h| h.score).unwrap_or(1.0).max(1e-6);
        for h in &assets {
            let e = &h.entity;
            if known.contains(e.id.as_str()) || e.state.as_deref() == Some("rejected") {
                continue;
            }
            let auth = scores.get(&e.id).copied().unwrap_or(0.0);
            let what = match e.kind.as_str() {
                "learned_fact" if e.state.as_deref() == Some("certified") => "certified fact".to_string(),
                "learned_fact" => format!(
                    "learned fact (confidence {})",
                    e.props.get("confidence").and_then(serde_json::Value::as_f64).unwrap_or(0.0)
                ),
                k => k.to_string(),
            };
            let body = ["description", "statement"].iter().find_map(|k| e.prop_str(k)).unwrap_or_default();
            add(
                &mut facts,
                format!("{what} [{}] \"{}\" (authority {auth:.2}): {}", e.id, e.label(), clip(body, 240)),
                0.45 + 0.35 * auth + 0.2 * h.score / top,
                "catalog",
                false,
            );
        }
    }

    // Memories recalled by the orchestrator. Durable facts rank with graph
    // facts; past Q&A episodes are weaker hints.
    for m in input.memories {
        let (scope, score) = match m.item.scope {
            MemoryScope::LongTerm => ("long-term", 0.55 + 0.35 * m.score.min(1.0)),
            MemoryScope::Episodic => ("earlier answer", 0.25 + 0.2 * m.score.min(1.0)),
        };
        add(&mut facts, format!("memory ({scope}): {}", clip(&m.item.text, 320)), score, "memory", false);
    }

    let candidate_tokens: usize = facts.iter().map(|(f, _)| f.tokens).sum();
    let items = facts
        .into_iter()
        .enumerate()
        .map(|(i, (f, pinned))| Packable {
            score: f.score,
            tokens: f.tokens,
            pinned,
            item: (support.remove(&i).unwrap_or_default(), f),
        })
        .collect();
    let budget = input.budget.max(64);
    let packed = pack(items, budget);
    let (kept_support, mut kept): (Vec<Support>, Vec<Fact>) = packed.kept.into_iter().unzip();
    let mut used = packed.used;
    let preservation = preserve(&mut kept, &kept_support, budget, &mut used);
    let text = kept.iter().map(|f| format!("- {}", f.text)).collect::<Vec<_>>().join("\n");
    ContextPack {
        question: input.question.to_string(),
        intent: intent.as_str().to_string(),
        seeds: seed_ids,
        nodes,
        edges,
        tokens: used,
        facts: kept,
        text,
        candidate_tokens,
        naive_tokens: naive_tokens.max(candidate_tokens),
        dropped_facts: packed.dropped,
        revision: onto.revision(),
        preservation,
    }
}

/// Check every packed claim against the links the packed facts state, and
/// append premise facts for missing links while `budget` allows.
///
/// `kept` and `support` are parallel, in packed order. Claims are visited in
/// that order (best first). A claim is repaired only when ALL its missing
/// premises fit: half a derivation supports nothing, so a claim that does not
/// fit adds no premise. A premise added for one claim is reused by later
/// claims at no cost (those count as repaired, not preserved). Returns `None`
/// when no claim was packed, so the context JSON is unchanged for questions
/// without derived facts.
fn preserve(kept: &mut Vec<Fact>, support: &[Support], budget: usize, used: &mut usize) -> Option<Preservation> {
    let packed: HashSet<&Link> = support
        .iter()
        .filter_map(|s| match s {
            Support::States(links) => Some(links),
            _ => None,
        })
        .flatten()
        .collect();
    let mut added: HashSet<Link> = HashSet::new();
    let mut report = Preservation { means: PRESERVATION_MEANS.to_string(), ..Default::default() };
    let scores: Vec<f32> = kept.iter().map(|f| f.score).collect();
    for (claim, score) in support.iter().zip(scores) {
        let Support::Claims(claim) = claim else { continue };
        report.claims += 1;
        let missing: Vec<&Premise> =
            claim.premises.iter().filter(|p| !p.links.iter().any(|l| packed.contains(l))).collect();
        if missing.is_empty() {
            report.preserved += 1;
            continue;
        }
        let mut new: Vec<&Premise> = Vec::new();
        for p in missing {
            let known = p.links.iter().any(|l| added.contains(l) || new.iter().any(|q| q.links.contains(l)));
            if !known {
                new.push(p);
            }
        }
        let cost: usize = new.iter().map(|p| estimate(&p.text) + 1).sum();
        if *used + cost > budget {
            report.lost.push(claim.spelled.clone());
            continue;
        }
        for p in new {
            let tokens = estimate(&p.text) + 1;
            kept.push(Fact { text: p.text.clone(), score, tokens, source: "premise".to_string() });
            added.extend(p.links.iter().cloned());
        }
        *used += cost;
        report.repaired += 1;
    }
    (report.claims > 0).then_some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(s: &str, p: &str, o: &str) -> Link {
        (s.to_string(), p.to_string(), o.to_string())
    }

    fn hop(from: &str, rel: &str, to: &str) -> Hop {
        Hop { from: from.into(), rel: rel.into(), to: to.into() }
    }

    fn fact(text: &str, source: &str) -> Fact {
        Fact { text: text.into(), score: 0.5, tokens: estimate(text) + 1, source: source.into() }
    }

    fn claim(spelled: &str, hops: &[Hop]) -> Support {
        let schema = Schema::enterprise();
        Support::Claims(Claim { spelled: spelled.into(), premises: hops.iter().map(|h| premise(&schema, h)).collect() })
    }

    #[test]
    fn premises_are_spelled_in_canonical_direction() {
        let schema = Schema::enterprise();
        let p = premise(&schema, &hop("person:kenji", "member_of", "team:payments"));
        assert_eq!(p.links, vec![link("person:kenji", "member_of", "team:payments")]);
        assert_eq!(p.text, "premise: person:kenji member_of → team:payments");
        // `owned_by` is the declared inverse of `owns`.
        let p = premise(&schema, &hop("project:alpha", "owned_by", "team:payments"));
        assert_eq!(p.links, vec![link("team:payments", "owns", "project:alpha")]);
        assert_eq!(p.text, "premise: team:payments owns → project:alpha");
        let p = premise(&schema, &hop("task:2", "^depends_on", "task:1"));
        assert_eq!(p.links, vec![link("task:1", "depends_on", "task:2")]);
        // A symmetric link is stated by either spelling.
        let p = premise(&schema, &hop("doc:b", "relates_to", "doc:a"));
        assert_eq!(p.links, vec![link("doc:b", "relates_to", "doc:a"), link("doc:a", "relates_to", "doc:b")]);
    }

    #[test]
    fn no_claim_means_no_report() {
        let mut kept = vec![fact("issue:1: assigned_to → person:a", "relations")];
        let support = vec![Support::States(vec![link("issue:1", "assigned_to", "person:a")])];
        let mut used = kept[0].tokens;
        assert_eq!(preserve(&mut kept, &support, 1000, &mut used), None);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn stated_premises_preserve_and_missing_ones_are_repaired_once() {
        let mut kept = vec![
            fact("derived: issue:1 handled_by_team team:x (via assigned_to > member_of)", "derived"),
            fact("derived: issue:2 handled_by_team team:x (via assigned_to > member_of)", "derived"),
            fact("issue:1: assigned_to → person:a", "relations"),
        ];
        let support = vec![
            claim(
                "issue:1 handled_by_team team:x",
                &[hop("issue:1", "assigned_to", "person:a"), hop("person:a", "member_of", "team:x")],
            ),
            claim(
                "issue:2 handled_by_team team:x",
                &[hop("issue:2", "assigned_to", "person:a"), hop("person:a", "member_of", "team:x")],
            ),
            Support::States(vec![link("issue:1", "assigned_to", "person:a")]),
        ];
        let before: Vec<String> = kept.iter().map(|f| f.text.clone()).collect();
        let mut used: usize = kept.iter().map(|f| f.tokens).sum();
        let start = used;
        let r = preserve(&mut kept, &support, 1000, &mut used).unwrap();
        assert_eq!((r.claims, r.preserved, r.repaired), (2, 0, 2), "{r:?}");
        assert!(r.lost.is_empty());
        let texts: Vec<&str> = kept.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(&texts[..3], before.iter().map(String::as_str).collect::<Vec<_>>().as_slice());
        // member_of is added once for both claims; issue:2's assignment is new.
        assert_eq!(&texts[3..], ["premise: person:a member_of → team:x", "premise: issue:2 assigned_to → person:a"]);
        assert!(kept[3..].iter().all(|f| f.source == "premise" && f.score == 0.5));
        assert_eq!(used, start + kept[3..].iter().map(|f| f.tokens).sum::<usize>());

        // With the member_of link packed too, the first claim is preserved.
        let mut kept = vec![fact("derived: ...", "derived"), fact("relations", "relations")];
        let support = vec![
            claim(
                "issue:1 handled_by_team team:x",
                &[hop("issue:1", "assigned_to", "person:a"), hop("person:a", "member_of", "team:x")],
            ),
            Support::States(vec![link("issue:1", "assigned_to", "person:a"), link("person:a", "member_of", "team:x")]),
        ];
        let mut used = 0;
        let r = preserve(&mut kept, &support, 1000, &mut used).unwrap();
        assert_eq!((r.claims, r.preserved, r.repaired), (1, 1, 0));
        assert_eq!(kept.len(), 2);
        assert_eq!(used, 0);
    }

    #[test]
    fn a_claim_that_does_not_fit_adds_nothing() {
        let hops = [hop("issue:1", "assigned_to", "person:a"), hop("person:a", "member_of", "team:x")];
        let one = premise(&Schema::enterprise(), &hops[0]);
        let cost_one = estimate(&one.text) + 1;
        let mut kept = vec![fact("derived: issue:1 handled_by_team team:x", "derived")];
        let support = vec![claim("issue:1 handled_by_team team:x", &hops)];
        // Room for exactly one of the two premises: half a derivation is not added.
        let mut used = 100;
        let r = preserve(&mut kept, &support, 100 + cost_one, &mut used).unwrap();
        assert_eq!((r.claims, r.preserved, r.repaired), (1, 0, 0));
        assert_eq!(r.lost, vec!["issue:1 handled_by_team team:x".to_string()]);
        assert_eq!(kept.len(), 1);
        assert_eq!(used, 100);
        assert_eq!(r.means, PRESERVATION_MEANS);
    }

    #[test]
    fn a_symmetric_premise_is_stated_by_either_direction() {
        let mut kept = vec![fact("derived: x", "derived"), fact("doc:a: relates_to → doc:b", "relations")];
        let support = vec![
            claim("doc:b see_also doc:a", &[hop("doc:b", "relates_to", "doc:a")]),
            Support::States(vec![link("doc:a", "relates_to", "doc:b")]),
        ];
        let mut used = 0;
        let r = preserve(&mut kept, &support, 1000, &mut used).unwrap();
        assert_eq!((r.claims, r.preserved), (1, 1));
    }
}
