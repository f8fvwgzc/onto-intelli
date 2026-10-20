//! Derived properties (functions over relations, evaluated on read) and
//! governed metrics (measure + filter + dimensions), including process KPIs
//! computed from the event log and entity version history: lead time,
//! throughput, failure rate and event volume.

use std::collections::{BTreeMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::graph::Graph;
use crate::model::{Entity, EventFilter, Id, one_or_many};
use crate::policy;
use crate::query::{FindSpec, find};
use crate::schema::Schema;
use crate::time::{fmt_date, now_ms};
use crate::{Error, Ontology, Principal, Result};

const DAY_MS: f64 = 86_400_000.0;

fn is_false(b: &bool) -> bool {
    !*b
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

// ── derived properties ─────────────────────────────────────────────────

/// `open_issues = { count = "has_item", kind = "issue", open = true }`
/// `remaining_days = { sum = "estimate", over = "has_item", open = true }`
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DerivedDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sum: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<String>,
    /// Relation walked for sum/avg/min/max.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state: Vec<String>,
    /// Follow the relation repeatedly (e.g. all transitive blockers).
    #[serde(default, skip_serializing_if = "is_false")]
    pub transitive: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl DerivedDef {
    /// (operation, property, relation)
    fn op(&self) -> Option<(&'static str, Option<&str>, Option<&str>)> {
        let over = self.over.as_deref();
        match (&self.count, &self.sum, &self.avg, &self.min, &self.max) {
            (Some(rel), None, None, None, None) => Some(("count", None, Some(rel))),
            (None, Some(p), None, None, None) => Some(("sum", Some(p), over)),
            (None, None, Some(p), None, None) => Some(("avg", Some(p), over)),
            (None, None, None, Some(p), None) => Some(("min", Some(p), over)),
            (None, None, None, None, Some(p)) => Some(("max", Some(p), over)),
            _ => None,
        }
    }

    pub fn validate(&self, schema: &Schema) -> std::result::Result<(), String> {
        let (_, _, rel) = self.op().ok_or("exactly one of count, sum, avg, min, max is required")?;
        let rel = rel.ok_or("sum/avg/min/max need `over = <relation>`")?;
        if schema.resolve_rel(rel).is_none() {
            return Err(format!("unknown relation {rel}"));
        }
        if let Some(k) = &self.kind
            && !schema.is_type_or_interface(k)
        {
            return Err(format!("unknown kind {k}"));
        }
        Ok(())
    }
}

fn related<'g>(
    schema: &Schema,
    g: &'g Graph,
    start: &str,
    rel: &str,
    transitive: bool,
    can_read: &dyn Fn(&Entity) -> bool,
) -> Vec<&'g Entity> {
    let (base, dir) = schema.resolve_rel_lenient(rel);
    let mut out = Vec::new();
    let mut seen = HashSet::from([start.to_string()]);
    let mut queue = VecDeque::from([(start.to_string(), 0usize)]);
    let max_depth = if transitive { 8 } else { 1 };
    while let Some((id, d)) = queue.pop_front() {
        if d >= max_depth {
            continue;
        }
        for step in g.steps_via(&id, &base, dir) {
            if seen.insert(step.other.to_string())
                && let Some(e) = g.get(step.other)
                && can_read(e)
            {
                out.push(e);
                queue.push_back((step.other.to_string(), d + 1));
            }
        }
    }
    out
}

/// All derived values for an entity, plus `age_days` and `idle_days`.
pub fn compute_derived(
    schema: &Schema,
    g: &Graph,
    e: &Entity,
    can_read: &dyn Fn(&Entity) -> bool,
) -> Map<String, Value> {
    let now = now_ms() as f64;
    let mut out = Map::new();
    if e.created_at > 0 {
        out.insert("age_days".into(), json!(((now - e.created_at as f64) / DAY_MS * 10.0).round() / 10.0));
        out.insert("idle_days".into(), json!(((now - e.updated_at as f64) / DAY_MS * 10.0).round() / 10.0));
    }
    for (name, d) in schema.derived_defs(&e.kind) {
        let Some((op, prop, Some(rel))) = d.op() else { continue };
        let items: Vec<&Entity> = related(schema, g, &e.id, rel, d.transitive, can_read)
            .into_iter()
            .filter(|t| d.kind.as_ref().is_none_or(|k| schema.kind_is(&t.kind, k)))
            .filter(|t| d.open.is_none_or(|o| schema.is_open(&t.kind, t.state.as_deref()) == o))
            .filter(|t| d.state.is_empty() || t.state.as_ref().is_some_and(|s| d.state.contains(s)))
            .collect();
        let value = if op == "count" {
            json!(items.len())
        } else {
            let nums: Vec<f64> = items.iter().filter_map(|t| t.props.get(prop?).and_then(Value::as_f64)).collect();
            match op {
                "sum" => json!(round2(nums.iter().sum())),
                _ if nums.is_empty() => Value::Null,
                "avg" => json!(round2(nums.iter().sum::<f64>() / nums.len() as f64)),
                "min" => json!(nums.iter().copied().fold(f64::INFINITY, f64::min)),
                _ => json!(nums.iter().copied().fold(f64::NEG_INFINITY, f64::max)),
            }
        };
        out.insert(name.clone(), value);
    }
    out
}

// ── metrics ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Measure {
    #[default]
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// Days from creation to reaching a terminal state (from version history).
    LeadTime,
    /// Items that reached a terminal state inside the window.
    Throughput,
    /// Share of matching events whose outcome was failure or denied.
    FailureRate,
    /// Number of matching events (volume).
    EventCount,
}

impl Measure {
    pub fn unit(self) -> &'static str {
        match self {
            Measure::Count | Measure::Throughput => "items",
            Measure::Sum | Measure::Avg | Measure::Min | Measure::Max => "value",
            Measure::LeadTime => "days",
            Measure::FailureRate => "ratio",
            Measure::EventCount => "events",
        }
    }

    fn is_event_based(self) -> bool {
        matches!(self, Measure::FailureRate | Measure::EventCount)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetricFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<bool>,
    #[serde(default, deserialize_with = "one_or_many", skip_serializing_if = "Vec::is_empty")]
    pub state: Vec<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub props: Map<String, Value>,
}

/// A governed KPI definition (like a metric view): what to measure, over
/// which objects, sliced by which dimensions.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetricDef {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default)]
    pub measure: Measure,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    #[serde(default)]
    pub filter: MetricFilter,
    /// alias → expression: `kind`, `state`, a property, a relation path
    /// (`assigned_to.member_of`) or, for event measures, `actor`/`action`/`outcome`/`day`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dimensions: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_days: Option<u32>,
    /// Reviewed and trusted (shown to agents as authoritative).
    #[serde(default, skip_serializing_if = "is_false")]
    pub certified: bool,
}

impl MetricDef {
    pub fn validate(&self, schema: &Schema) -> std::result::Result<(), String> {
        if let Some(k) = &self.kind
            && !schema.is_type_or_interface(k)
        {
            return Err(format!("unknown kind {k}"));
        }
        if matches!(self.measure, Measure::Sum | Measure::Avg | Measure::Min | Measure::Max) && self.property.is_none()
        {
            return Err(format!("{:?} needs `property`", self.measure).to_lowercase());
        }
        if matches!(self.measure, Measure::LeadTime | Measure::Throughput) && self.kind.is_none() {
            return Err("lead_time/throughput need `kind`".into());
        }
        for (alias, expr) in &self.dimensions {
            if expr.contains('.') {
                for seg in expr.split('.') {
                    if schema.resolve_rel(seg).is_none() {
                        return Err(format!("dimension {alias}: unknown relation {seg}"));
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetricQuery {
    /// Named metric from the schema; or describe one ad hoc with the fields below.
    #[serde(default)]
    pub metric: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub measure: Option<Measure>,
    #[serde(default)]
    pub property: Option<String>,
    #[serde(default)]
    pub filter: Option<MetricFilter>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub group_by: Vec<String>,
    /// Only objects inside this entity's scope (team, project, person).
    #[serde(default)]
    pub within: Option<Id>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub window_days: Option<u32>,
    #[serde(default)]
    pub event_action: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetricRow {
    pub group: BTreeMap<String, String>,
    pub value: f64,
    pub count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetricResult {
    pub metric: String,
    pub measure: Measure,
    pub unit: String,
    pub value: f64,
    pub count: usize,
    pub rows: Vec<MetricRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_days: Option<u32>,
    #[serde(default)]
    pub certified: bool,
}

/// Values of one dimension for an entity (several when a relation fans out).
fn entity_dim(schema: &Schema, g: &Graph, e: &Entity, expr: &str, can_read: &dyn Fn(&Entity) -> bool) -> Vec<String> {
    let none = || vec!["(none)".to_string()];
    match expr {
        "kind" => return vec![e.kind.clone()],
        "state" => return vec![e.state.clone().unwrap_or_else(|| "(none)".into())],
        "workspace" => return vec![e.workspace.clone()],
        _ => {}
    }
    let first = expr.split('.').next().unwrap_or(expr);
    if expr.contains('.') || schema.resolve_rel(first).is_some() {
        let mut frontier: Vec<&str> = vec![&e.id];
        for seg in expr.split('.') {
            let (base, dir) = schema.resolve_rel_lenient(seg);
            let mut next: Vec<&str> = Vec::new();
            for id in &frontier {
                for s in g.steps_via(id, &base, dir) {
                    if g.get(s.other).is_some_and(can_read) && !next.contains(&s.other) {
                        next.push(s.other);
                    }
                }
            }
            frontier = next;
        }
        let labels: Vec<String> = frontier.iter().filter_map(|id| g.get(id)).map(|t| t.label().to_string()).collect();
        return if labels.is_empty() { none() } else { labels };
    }
    match e.props.get(expr) {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) if !a.is_empty() => {
            a.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect()
        }
        Some(Value::Null) | None => none(),
        Some(other) => vec![other.to_string()],
    }
}

/// Cartesian product of per-dimension values.
fn combos(dims: &[(String, Vec<String>)]) -> Vec<BTreeMap<String, String>> {
    let mut out = vec![BTreeMap::new()];
    for (name, values) in dims {
        let mut next = Vec::new();
        for base in &out {
            for v in values {
                let mut m = base.clone();
                m.insert(name.clone(), v.clone());
                next.push(m);
            }
        }
        out = next;
    }
    out
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[derive(Default)]
struct Acc {
    values: Vec<f64>,
    hits: usize,
    total: usize,
}

impl Ontology {
    /// Resolve a group-by name to an expression: metric alias, glossary
    /// dimension, glossary relation, or the name itself.
    fn dim_expr(&self, def: &MetricDef, name: &str) -> String {
        let schema = self.schema();
        def.dimensions
            .get(name)
            .or_else(|| schema.glossary.dimensions.get(name))
            .cloned()
            .or_else(|| schema.glossary.relations.get(name).cloned())
            .unwrap_or_else(|| name.to_string())
    }

    /// Evaluate a named or ad-hoc metric.
    pub fn metric(&self, p: &Principal, q: &MetricQuery) -> Result<MetricResult> {
        let schema = self.schema();
        let mut def = match &q.metric {
            Some(name) => schema
                .metrics
                .get(name)
                .cloned()
                .ok_or_else(|| Error::not_found(format!("metric {name} (see schema metrics)")))?,
            None => MetricDef { name: "ad_hoc".into(), ..Default::default() },
        };
        if let Some(k) = &q.kind {
            def.kind = Some(schema.canonical_kind(k).unwrap_or_else(|| k.clone()));
        }
        if let Some(m) = q.measure {
            def.measure = m;
        }
        if q.property.is_some() {
            def.property = q.property.clone();
        }
        if let Some(f) = &q.filter {
            def.filter = f.clone();
        }
        if q.event_action.is_some() {
            def.event_action = q.event_action.clone();
        }
        if q.window_days.is_some() {
            def.window_days = q.window_days;
        }
        def.validate(&schema).map_err(Error::invalid)?;
        let dims: Vec<(String, String)> = q.group_by.iter().map(|n| (n.clone(), self.dim_expr(&def, n))).collect();
        let limit = q.limit.unwrap_or(50);
        let window = def.window_days.map(|d| d as i64 * 86_400_000);
        let since = window.map(|w| now_ms() - w);
        let mut groups: BTreeMap<Vec<(String, String)>, Acc> = BTreeMap::new();
        // Counted once per item even when a dimension fans out into several groups.
        let mut overall = Acc::default();

        if def.measure.is_event_based() {
            if !p.allows(crate::Access::Read) {
                return Err(Error::Denied(format!("{} has no read role", p.id)));
            }
            let filter = EventFilter {
                workspace: q.workspace.clone(),
                action: def.event_action.clone(),
                since,
                limit: Some(100_000),
                ..Default::default()
            };
            let events = self.store().events(&filter)?;
            for ev in events.into_iter().filter(|e| p.in_workspace(&e.workspace)) {
                if let Some(k) = &def.kind {
                    let tk = ev.target.as_deref().and_then(|t| t.split_once(':')).map(|(k, _)| k).unwrap_or("");
                    if !schema.kind_is(tk, k) {
                        continue;
                    }
                }
                let per_dim: Vec<(String, Vec<String>)> = dims
                    .iter()
                    .map(|(name, expr)| {
                        let v = match expr.as_str() {
                            "actor" => ev.actor.clone(),
                            "action" => ev.action.clone(),
                            "outcome" => ev.outcome.as_str().to_string(),
                            "day" => fmt_date(ev.ts),
                            "target" => ev.target.clone().unwrap_or_else(|| "(none)".into()),
                            "target_kind" => ev
                                .target
                                .as_deref()
                                .and_then(|t| t.split_once(':'))
                                .map(|(k, _)| k.to_string())
                                .unwrap_or_else(|| "(none)".into()),
                            other => ev
                                .payload
                                .get(other)
                                .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
                                .unwrap_or_else(|| "(none)".into()),
                        };
                        (name.clone(), vec![v])
                    })
                    .collect();
                let failed = matches!(ev.outcome, crate::Outcome::Failure | crate::Outcome::Denied);
                overall.total += 1;
                overall.hits += usize::from(failed);
                for combo in combos(&per_dim) {
                    let acc = groups.entry(combo.into_iter().collect()).or_default();
                    acc.total += 1;
                    acc.hits += usize::from(failed);
                }
            }
        } else {
            // Entity-based measures: compute members and group keys under the read lock,
            // then (for history measures) consult version history.
            let members: Vec<(Entity, Vec<BTreeMap<String, String>>)> = self.read(|g, schema| {
                let can = |e: &Entity| policy::visible(p, e, g);
                let spec = FindSpec {
                    kind: def.kind.clone(),
                    workspace: q.workspace.clone(),
                    state: def.filter.state.clone(),
                    open: match def.measure {
                        Measure::LeadTime | Measure::Throughput => None,
                        _ => def.filter.open,
                    },
                    props: def.filter.props.clone(),
                    within: q.within.clone(),
                    limit: usize::MAX,
                    ..Default::default()
                };
                let (items, _) = find(schema, g, &spec, &can);
                items
                    .into_iter()
                    .map(|e| {
                        let per_dim: Vec<(String, Vec<String>)> =
                            dims.iter().map(|(n, expr)| (n.clone(), entity_dim(schema, g, &e, expr, &can))).collect();
                        let keys = combos(&per_dim);
                        (e, keys)
                    })
                    .collect()
            });
            for (e, keys) in members {
                let value = match def.measure {
                    Measure::Count => Some(1.0),
                    Measure::Sum | Measure::Avg | Measure::Min | Measure::Max => {
                        def.property.as_ref().and_then(|prop| e.props.get(prop)).and_then(Value::as_f64)
                    }
                    Measure::LeadTime | Measure::Throughput => {
                        if schema.is_open(&e.kind, e.state.as_deref()) {
                            None
                        } else {
                            let done_at = self.terminal_since(&e)?;
                            match (done_at, since) {
                                (Some(t), Some(s)) if t < s => None,
                                (Some(t), _) => Some(if def.measure == Measure::LeadTime {
                                    (t - e.created_at).max(0) as f64 / DAY_MS
                                } else {
                                    1.0
                                }),
                                (None, _) => None,
                            }
                        }
                    }
                    _ => None,
                };
                let Some(v) = value else { continue };
                overall.values.push(v);
                overall.total += 1;
                for key in keys {
                    let acc = groups.entry(key.into_iter().collect()).or_default();
                    acc.values.push(v);
                    acc.total += 1;
                }
            }
        }

        let finish = |acc: &Acc| -> (f64, usize, Option<f64>, Option<f64>) {
            let n = acc.values.len();
            match def.measure {
                Measure::Count | Measure::Throughput => (n as f64, n, None, None),
                Measure::Sum => (round2(acc.values.iter().sum()), n, None, None),
                Measure::Avg | Measure::LeadTime => {
                    if n == 0 {
                        return (0.0, 0, None, None);
                    }
                    let mut sorted = acc.values.clone();
                    sorted.sort_by(f64::total_cmp);
                    let avg = round2(sorted.iter().sum::<f64>() / n as f64);
                    (avg, n, Some(round2(percentile(&sorted, 0.5))), Some(round2(percentile(&sorted, 0.9))))
                }
                Measure::Min => (acc.values.iter().copied().fold(f64::INFINITY, f64::min), n, None, None),
                Measure::Max => (acc.values.iter().copied().fold(f64::NEG_INFINITY, f64::max), n, None, None),
                Measure::FailureRate => (
                    if acc.total == 0 { 0.0 } else { round2(acc.hits as f64 / acc.total as f64) },
                    acc.total,
                    None,
                    None,
                ),
                Measure::EventCount => (acc.total as f64, acc.total, None, None),
            }
        };

        let (value, count, _, _) = finish(&overall);
        let mut rows: Vec<MetricRow> = groups
            .iter()
            .map(|(k, acc)| {
                let (value, count, p50, p90) = finish(acc);
                MetricRow { group: k.iter().cloned().collect(), value, count, p50, p90 }
            })
            .collect();
        rows.sort_by(|a, b| b.value.total_cmp(&a.value).then_with(|| a.group.cmp(&b.group)));
        rows.truncate(limit);
        Ok(MetricResult {
            metric: def.name.clone(),
            measure: def.measure,
            unit: def
                .property
                .clone()
                .filter(|_| def.measure.unit() == "value")
                .unwrap_or_else(|| def.measure.unit().into()),
            value: if value.is_finite() { value } else { 0.0 },
            count,
            rows,
            window_days: def.window_days,
            certified: def.certified,
        })
    }

    /// When the entity first reached its current terminal state, from version history.
    fn terminal_since(&self, e: &Entity) -> Result<Option<i64>> {
        let schema = self.schema();
        let versions = self.store().versions(&e.id, 1000)?;
        // Versions are newest first; walk back while the state stays terminal.
        let mut since = None;
        for v in &versions {
            let Some(doc) = &v.doc else { break };
            if schema.is_open(&doc.kind, doc.state.as_deref()) {
                break;
            }
            since = Some(v.ts);
        }
        Ok(since.or(if versions.is_empty() { Some(e.updated_at) } else { None }))
    }

    /// Derived values for an entity (permission-filtered inputs).
    pub fn derived(&self, p: &Principal, id: &str) -> Result<Map<String, Value>> {
        let e = self.get(p, id)?;
        Ok(self.read(|g, schema| {
            let can = |x: &Entity| policy::visible(p, x, g);
            compute_derived(schema, g, &e, &can)
        }))
    }
}
