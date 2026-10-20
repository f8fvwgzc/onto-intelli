//! Data-quality report (SHACL-style shape checks plus operational hygiene):
//! required properties and links, valid states, uniqueness, staleness,
//! orphans, dangling provenance, likely duplicates and low-confidence facts,
//! rolled up into per-kind and overall scores.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::graph::Graph;
use crate::model::{Entity, Id};
use crate::schema::{Dir, Schema};
use crate::similarity::duplicates;
use crate::time::now_ms;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Constraints {
    /// Relations every entity of the kind must have at least once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_links: Vec<String>,
    /// Properties whose values must be unique within a workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unique: Vec<String>,
    /// Open entities untouched for longer than this are reported as stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_after_days: Option<u32>,
}

impl Constraints {
    pub fn is_empty(&self) -> bool {
        self.required_links.is_empty() && self.unique.is_empty() && self.stale_after_days.is_none()
    }

    pub fn merge(&mut self, other: &Constraints) {
        for r in &other.required_links {
            if !self.required_links.contains(r) {
                self.required_links.push(r.clone());
            }
        }
        for u in &other.unique {
            if !self.unique.contains(u) {
                self.unique.push(u.clone());
            }
        }
        if other.stale_after_days.is_some() {
            self.stale_after_days = other.stale_after_days;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    fn penalty(self) -> f64 {
        match self {
            Severity::Error => 1.0,
            Severity::Warning => 0.5,
            Severity::Info => 0.1,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Violation {
    pub rule: String,
    pub severity: Severity,
    pub entity: Id,
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KindQuality {
    pub entities: usize,
    pub with_issues: usize,
    pub score: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QualityReport {
    pub score: f64,
    pub entities: usize,
    pub violations: Vec<Violation>,
    pub by_rule: BTreeMap<String, usize>,
    pub by_kind: BTreeMap<String, KindQuality>,
    pub truncated: bool,
}

fn low_confidence(props: &serde_json::Map<String, Value>) -> Option<f64> {
    let c = props.get("confidence").and_then(Value::as_f64)?;
    let verified = props.get("verified").and_then(Value::as_bool).unwrap_or(false);
    (c < 0.5 && !verified).then_some(c)
}

pub fn report(
    schema: &Schema,
    g: &Graph,
    workspace: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    can_read: &dyn Fn(&Entity) -> bool,
) -> QualityReport {
    let now = now_ms();
    let entities: Vec<&Entity> = g
        .entities()
        .filter(|e| workspace.is_none_or(|w| e.workspace == w))
        .filter(|e| kind.is_none_or(|k| schema.kind_is(&e.kind, k)))
        .filter(|e| can_read(e))
        .collect();
    let mut v: Vec<Violation> = Vec::new();
    let mut push = |e: &Entity, rule: &str, severity: Severity, message: String| {
        v.push(Violation { rule: rule.into(), severity, entity: e.id.clone(), kind: e.kind.clone(), message });
    };

    // Uniqueness: value → entities, per (workspace, kind, prop).
    let mut unique_index: HashMap<(String, String, String, String), Vec<Id>> = HashMap::new();
    for e in &entities {
        let c = schema.constraints(&e.kind);
        for p in &c.unique {
            if let Some(val) = e.prop_str(p).filter(|x| !x.is_empty()) {
                unique_index
                    .entry((e.workspace.clone(), e.kind.clone(), p.clone(), val.to_lowercase()))
                    .or_default()
                    .push(e.id.clone());
            }
        }
    }

    for e in &entities {
        let c = schema.constraints(&e.kind);
        for (name, def) in schema.all_props(&e.kind) {
            if def.required && e.props.get(name).is_none_or(Value::is_null) {
                push(e, "missing_property", Severity::Error, format!("{} is missing required property {name}", e.id));
            }
        }
        if let (Some(ty), Some(state)) = (schema.object_type(&e.kind), &e.state)
            && !ty.states.is_empty()
            && !ty.states.contains(state)
        {
            push(e, "invalid_state", Severity::Error, format!("{} has state '{state}' not in the schema", e.id));
        }
        for rel in &c.required_links {
            let (base, dir) = schema.resolve_rel_lenient(rel);
            if g.steps_via(&e.id, &base, dir).is_empty() {
                push(e, "missing_link", Severity::Warning, format!("{} has no {rel} relation", e.id));
            }
        }
        for p in &c.unique {
            if let Some(val) = e.prop_str(p).filter(|x| !x.is_empty()) {
                let key = (e.workspace.clone(), e.kind.clone(), p.clone(), val.to_lowercase());
                if let Some(ids) = unique_index.get(&key).filter(|ids| ids.len() > 1) {
                    let others: Vec<&str> = ids.iter().filter(|i| **i != e.id).map(String::as_str).collect();
                    push(
                        e,
                        "unique_violation",
                        Severity::Error,
                        format!("{p} '{val}' is also used by {}", others.join(", ")),
                    );
                }
            }
        }
        if let Some(days) = c.stale_after_days
            && schema.is_open(&e.kind, e.state.as_deref())
            && e.updated_at > 0
            && now - e.updated_at > i64::from(days) * 86_400_000
        {
            let idle = (now - e.updated_at) / 86_400_000;
            push(
                e,
                "stale",
                Severity::Warning,
                format!("{} is open and untouched for {idle} days (limit {days})", e.id),
            );
        }
        if e.kind != "workspace" && g.steps(&e.id).next().is_none() {
            push(e, "orphan", Severity::Info, format!("{} has no relations", e.id));
        }
        if let Some(pv) = &e.provenance {
            for src in pv.derived_from.iter().chain(pv.generated_by.iter()) {
                if !g.contains(src) {
                    push(e, "dangling_provenance", Severity::Warning, format!("{} references missing {src}", e.id));
                }
            }
        }
        // The data catalog: what a reader would need to trust an asset.
        if matches!(e.kind.as_str(), "table" | "view") {
            if e.prop_str("description").is_none_or(|d| d.trim().is_empty()) {
                let what = if e.kind == "view" { "VIEW" } else { "TABLE" };
                push(e, "undocumented", Severity::Info, format!("{} has no description (COMMENT ON {what})", e.id));
            }
            if g.neighbors(&e.id, "owns", Dir::In).is_empty() {
                push(e, "unowned", Severity::Info, format!("{} has no owner (link a team with owns)", e.id));
            }
        }
        if e.kind == "learned_fact"
            && e.state.as_deref() == Some("suggested")
            && e.props.get("confidence").and_then(Value::as_f64).unwrap_or(0.0) >= 0.8
        {
            push(
                e,
                "awaiting_review",
                Severity::Info,
                format!("{} awaits review: {}", e.id, e.prop_str("statement").unwrap_or(&e.title)),
            );
        }
        if let Some(c) = low_confidence(&e.props) {
            push(e, "low_confidence", Severity::Info, format!("{} has unverified confidence {c:.2}", e.id));
        }
        for (rel, to) in g.out_edges(&e.id) {
            if let Some(l) = g.get_link(&e.id, rel, to)
                && let Some(c) = low_confidence(&l.props)
            {
                push(
                    e,
                    "low_confidence",
                    Severity::Info,
                    format!("{} -{rel}-> {to} is unverified (confidence {c:.2})", e.id),
                );
            }
        }
    }
    for d in duplicates(schema, g, workspace, kind, 0.9, can_read) {
        if let Some(e) = g.get(&d.a) {
            push(
                e,
                "duplicate",
                Severity::Warning,
                format!("{} looks like a duplicate of {} ({})", d.a, d.b, d.reasons.join(", ")),
            );
        }
    }

    // Scores: each entity pays the penalty of its worst violation.
    let mut worst: HashMap<&str, Severity> = HashMap::new();
    for x in &v {
        let w = worst.entry(x.entity.as_str()).or_insert(x.severity);
        if x.severity > *w {
            *w = x.severity;
        }
    }
    let mut by_kind: BTreeMap<String, (usize, usize, f64)> = BTreeMap::new();
    for e in &entities {
        let slot = by_kind.entry(e.kind.clone()).or_default();
        slot.0 += 1;
        if let Some(s) = worst.get(e.id.as_str()) {
            slot.1 += 1;
            slot.2 += s.penalty();
        }
    }
    let total_penalty: f64 = by_kind.values().map(|s| s.2).sum();
    let score = |n: usize, p: f64| if n == 0 { 100.0 } else { ((1.0 - p / n as f64) * 1000.0).round() / 10.0 };
    let mut by_rule: BTreeMap<String, usize> = BTreeMap::new();
    for x in &v {
        *by_rule.entry(x.rule.clone()).or_default() += 1;
    }
    v.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.entity.cmp(&b.entity)));
    let truncated = v.len() > limit;
    v.truncate(limit);
    QualityReport {
        score: score(entities.len(), total_penalty),
        entities: entities.len(),
        violations: v,
        by_rule,
        by_kind: by_kind
            .into_iter()
            .map(|(k, (n, bad, p))| (k, KindQuality { entities: n, with_issues: bad, score: score(n, p) }))
            .collect(),
        truncated,
    }
}
