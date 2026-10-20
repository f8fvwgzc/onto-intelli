//! OntoRank: an authority score for data assets and knowledge, so search
//! and context packing put the trusted, used, documented things first.
//!
//! The score is a weighted sum of components, each in 0..1 and stored with
//! the raw evidence behind it, so every score can be explained:
//!
//! | component | from |
//! |---|---|
//! | `centrality` | PageRank over the asset graph: queries and views pass authority to what they read (weighted by calls), foreign keys to what they reference, columns to their table |
//! | `usage` | scans and query calls (tables, views), calls (queries), the table's usage (columns), support (learned facts) |
//! | `freshness` | days since the last activity or change, halving about every three weeks |
//! | `owner` | owned by a team (1), a person (0.8), only a database role (0.3), inherited from the container |
//! | `documentation` | comments on the asset and on its columns |
//! | `certification` | certified by a person (learned facts), or a certified governed metric (1); the subject of a certified fact (0.5) |

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::engine::ActionContext;
use crate::graph::Graph;
use crate::model::{Entity, Id};
use crate::policy::{self, Principal};
use crate::schema::{Dir, Schema};
use crate::time::now_ms;
use crate::{Error, Ontology, Result};

/// KV namespace of per-entity scores, and of the run they belong to.
pub const NS: &str = "authority";
pub const NS_META: &str = "authority_meta";

/// Kinds OntoRank scores unless told otherwise.
pub const DEFAULT_KINDS: &[&str] = &[
    "data_source",
    "db_schema",
    "table",
    "view",
    "column",
    "query",
    "learned_fact",
    "knowledge",
    "decision",
    "artifact",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Weights {
    pub centrality: f64,
    pub usage: f64,
    pub freshness: f64,
    pub owner: f64,
    pub documentation: f64,
    pub certification: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self { centrality: 0.25, usage: 0.25, freshness: 0.10, owner: 0.15, documentation: 0.15, certification: 0.10 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Components {
    pub centrality: f64,
    pub usage: f64,
    pub freshness: f64,
    pub owner: f64,
    pub documentation: f64,
    pub certification: f64,
}

impl Components {
    fn score(&self, w: &Weights) -> f64 {
        let total = w.centrality + w.usage + w.freshness + w.owner + w.documentation + w.certification;
        let s = w.centrality * self.centrality
            + w.usage * self.usage
            + w.freshness * self.freshness
            + w.owner * self.owner
            + w.documentation * self.documentation
            + w.certification * self.certification;
        if total > 0.0 { s / total } else { 0.0 }
    }
}

/// The authority of one entity, with what it was computed from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Authority {
    pub id: Id,
    pub kind: String,
    /// 0..1.
    pub score: f64,
    /// 1 is the highest score among the entities ranked in the same run.
    pub rank: usize,
    pub components: Components,
    /// Raw inputs: PageRank, usage counts, age, owner, documented share.
    pub evidence: BTreeMap<String, Value>,
    pub computed_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthorityRun {
    pub computed_at: i64,
    pub ranked: usize,
    pub weights: Weights,
    pub kinds: Vec<String>,
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn num(e: &Entity, prop: &str) -> f64 {
    e.props.get(prop).and_then(Value::as_f64).unwrap_or(0.0)
}

fn has_text(e: &Entity, prop: &str) -> bool {
    e.prop_str(prop).is_some_and(|s| !s.trim().is_empty())
}

/// Score `kinds` (concrete names) in `g`.
pub fn compute(g: &Graph, schema: &Schema, kinds: &[String], weights: &Weights, now: i64) -> Vec<Authority> {
    let wanted: HashSet<String> =
        kinds.iter().flat_map(|k| schema.concrete_kinds(k)).chain(kinds.iter().cloned()).collect();
    let nodes: Vec<&Entity> = g.entities().filter(|e| wanted.contains(&e.kind)).collect();
    if nodes.is_empty() {
        return Vec::new();
    }
    let index: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, e)| (e.id.as_str(), i)).collect();
    let n = nodes.len();

    // Raw usage per node.
    let parent = |id: &str| -> Option<&str> { g.neighbors(id, "contains", Dir::In).into_iter().next() };
    let mut usage_raw = vec![0.0f64; n];
    for (i, e) in nodes.iter().enumerate() {
        usage_raw[i] = match e.kind.as_str() {
            "table" | "view" => num(e, "reads") + num(e, "query_calls") + 0.2 * num(e, "writes"),
            "query" => num(e, "calls"),
            "learned_fact" => num(e, "support"),
            _ => g.in_edges(&e.id).len() as f64,
        };
    }
    // Columns inherit half their table's usage; schemas and sources sum their children.
    for (i, e) in nodes.iter().enumerate() {
        if e.kind == "column"
            && let Some(t) = parent(&e.id).and_then(|p| index.get(p))
        {
            usage_raw[i] = 0.5 * usage_raw[*t];
        }
    }
    for kind in ["db_schema", "data_source"] {
        for (i, e) in nodes.iter().enumerate().filter(|(_, e)| e.kind == kind) {
            usage_raw[i] = g
                .neighbors(&e.id, "contains", Dir::Out)
                .iter()
                .filter_map(|c| index.get(c))
                .map(|&c| usage_raw[c])
                .sum();
        }
    }

    // Weighted edges among nodes: authority flows from → to.
    let mut edges: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for (i, e) in nodes.iter().enumerate() {
        for (rel, to) in g.out_edges(&e.id) {
            let Some(&j) = index.get(to.as_str()) else { continue };
            let (fwd, back) = match rel.as_str() {
                "reads_from" => (1.0 + (1.0 + num(e, "calls")).ln(), 0.0),
                "references" => (1.0, 0.2),
                // Children lend authority to their container more than the reverse.
                "contains" => (0.3, 0.5),
                "concerns" => {
                    let certified = e.state.as_deref() == Some("certified");
                    (0.5 * num(e, "confidence") + if certified { 1.0 } else { 0.0 }, 0.0)
                }
                _ => (0.5, 0.5),
            };
            if fwd > 0.0 {
                edges[i].push((j, fwd));
            }
            if back > 0.0 {
                edges[j].push((i, back));
            }
        }
    }
    // Personalized PageRank: teleport by usage, so heavily used assets seed authority.
    let max_usage = usage_raw.iter().copied().fold(0.0, f64::max);
    let mut prior: Vec<f64> = usage_raw
        .iter()
        .map(|u| 1.0 + if max_usage > 0.0 { (1.0 + u).ln() / (1.0 + max_usage).ln() } else { 0.0 })
        .collect();
    let total: f64 = prior.iter().sum();
    prior.iter_mut().for_each(|p| *p /= total);
    let out_w: Vec<f64> = edges.iter().map(|es| es.iter().map(|(_, w)| w).sum()).collect();
    let damping = 0.85;
    let mut rank = prior.clone();
    for _ in 0..40 {
        let mut next: Vec<f64> = prior.iter().map(|p| (1.0 - damping) * p).collect();
        let mut dangling = 0.0;
        for i in 0..n {
            if out_w[i] == 0.0 {
                dangling += rank[i];
                continue;
            }
            for &(j, w) in &edges[i] {
                next[j] += damping * rank[i] * w / out_w[i];
            }
        }
        for (x, p) in next.iter_mut().zip(&prior) {
            *x += damping * dangling * p;
        }
        let delta: f64 = next.iter().zip(&rank).map(|(a, b)| (a - b).abs()).sum();
        rank = next;
        if delta < 1e-9 {
            break;
        }
    }
    let max_rank = rank.iter().copied().fold(0.0, f64::max);
    let nf = n as f64;

    // Usage normalized within each kind (the busiest column scores like the busiest table).
    let mut max_by_kind: HashMap<&str, f64> = HashMap::new();
    for (i, e) in nodes.iter().enumerate() {
        let m = max_by_kind.entry(e.kind.as_str()).or_default();
        *m = m.max(usage_raw[i]);
    }

    // The first owner: a team (container) 1, a person 0.8, an agent 0.5.
    let owner_of = |id: &str| -> (f64, Option<String>) {
        let Some(o) = g.neighbors(id, "owns", Dir::In).into_iter().next() else { return (0.0, None) };
        let kind = o.split_once(':').map(|(k, _)| k).unwrap_or_default();
        let s = match schema.category(kind) {
            crate::schema::Category::Container => 1.0,
            _ if kind == "person" => 0.8,
            _ if kind == "agent" => 0.5,
            _ => 0.6,
        };
        (s, Some(o.to_string()))
    };
    // Assets a certified learned fact is about (the view that defines a
    // certified metric, the tables of a certified join).
    let mut certified_about: HashMap<&str, &str> = HashMap::new();
    for f in g.of_kind("learned_fact").filter(|f| f.state.as_deref() == Some("certified")) {
        for a in g.neighbors(&f.id, "concerns", Dir::Out) {
            certified_about.entry(a).or_insert(f.id.as_str());
        }
    }
    let certified_metrics: HashSet<&str> =
        schema.metrics.values().filter(|m| m.certified).flat_map(|m| [m.name.as_str()]).collect();

    let mut out: Vec<Authority> = nodes
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut ev: BTreeMap<String, Value> = BTreeMap::new();
            let centrality = if max_rank > 0.0 { (1.0 + rank[i] * nf).ln() / (1.0 + max_rank * nf).ln() } else { 0.0 };
            ev.insert("pagerank".into(), json!(round3(rank[i] * nf)));
            let km = max_by_kind.get(e.kind.as_str()).copied().unwrap_or(0.0);
            let usage = if km > 0.0 { (1.0 + usage_raw[i]).ln() / (1.0 + km).ln() } else { 0.0 };
            ev.insert("usage".into(), json!(usage_raw[i].round() as i64));
            let last = e.props.get("last_activity").and_then(Value::as_i64).unwrap_or(0).max(e.updated_at);
            let age_days = ((now - last).max(0) as f64) / 86_400_000.0;
            let freshness = (-age_days / 30.0).exp();
            ev.insert("age_days".into(), json!((age_days * 10.0).round() / 10.0));
            // Owner: direct, else inherited from the container (column → table → schema).
            let (mut owner, mut who) = owner_of(&e.id);
            if who.is_none() {
                let mut cur = parent(&e.id);
                for _ in 0..3 {
                    let Some(p) = cur else { break };
                    let (s, w) = owner_of(p);
                    if w.is_some() {
                        owner = 0.8 * s;
                        who = w.map(|w| format!("{w} (via {p})"));
                        break;
                    }
                    cur = parent(p);
                }
            }
            if who.is_none() && has_text(e, "owner_role") {
                owner = 0.3;
                who = e.prop_str("owner_role").map(|r| format!("role {r}"));
            }
            if let Some(w) = who {
                ev.insert("owner".into(), json!(w));
            }
            let documentation = match e.kind.as_str() {
                "table" | "view" => {
                    let share = num(e, "documented_columns");
                    ev.insert("documented_columns".into(), json!(share));
                    0.5 * f64::from(u8::from(has_text(e, "description"))) + 0.5 * share
                }
                "query" => 0.5,
                "learned_fact" => f64::from(u8::from(has_text(e, "statement"))),
                _ => f64::from(u8::from(
                    has_text(e, "description")
                        || has_text(e, "body")
                        || has_text(e, "summary")
                        || has_text(e, "content"),
                )),
            };
            let certified = e.state.as_deref() == Some("certified")
                || e.props.get("certified").and_then(Value::as_bool) == Some(true)
                || (e.kind == "learned_fact" && e.prop_str("name").is_some_and(|m| certified_metrics.contains(m)));
            if let Some(by) = e.prop_str("certified_by") {
                ev.insert("certified_by".into(), json!(by));
            }
            let vouched = (!certified).then(|| certified_about.get(e.id.as_str())).flatten();
            if let Some(fact) = vouched {
                ev.insert("certified_fact".into(), json!(fact));
            }
            let components = Components {
                centrality: round3(centrality),
                usage: round3(usage),
                freshness: round3(freshness),
                owner: round3(owner),
                documentation: round3(documentation),
                certification: if certified {
                    1.0
                } else if vouched.is_some() {
                    0.5
                } else {
                    0.0
                },
            };
            Authority {
                id: e.id.clone(),
                kind: e.kind.clone(),
                score: round3(components.score(weights)),
                rank: 0,
                components,
                evidence: ev,
                computed_at: now,
            }
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    for (i, a) in out.iter_mut().enumerate() {
        a.rank = i + 1;
    }
    out
}

/// Scores held in memory for ranking (id → score).
pub type Scores = Arc<HashMap<Id, f32>>;

impl Ontology {
    /// Recompute OntoRank over `kinds` (default [`DEFAULT_KINDS`]) and store
    /// every score with its components, in one commit other processes follow.
    pub fn compute_authority(&self, ctx: &ActionContext, kinds: &[String], weights: &Weights) -> Result<AuthorityRun> {
        if !ctx.principal.allows(policy::Access::Write) {
            return Err(Error::Denied(format!("{} may not recompute authority", ctx.principal.id)));
        }
        let kinds: Vec<String> =
            if kinds.is_empty() { DEFAULT_KINDS.iter().map(|k| k.to_string()).collect() } else { kinds.to_vec() };
        let now = now_ms();
        let (run, rows, stale) = {
            let schema = self.schema();
            let g = self.inner_graph_read();
            let ranked = compute(&g, &schema, &kinds, weights, now);
            let keep: HashSet<&str> = ranked.iter().map(|a| a.id.as_str()).collect();
            let stale: Vec<String> = self
                .store()
                .kv_scan(NS, "")?
                .into_iter()
                .map(|(k, _)| k)
                .filter(|k| !keep.contains(k.as_str()))
                .collect();
            let run = AuthorityRun { computed_at: now, ranked: ranked.len(), weights: weights.clone(), kinds };
            let rows: Vec<(String, String, Value)> = ranked
                .iter()
                .map(|a| Ok((NS.to_string(), a.id.clone(), serde_json::to_value(a)?)))
                .collect::<Result<_>>()?;
            (run, rows, stale)
        };
        let mut kv = rows;
        kv.push((NS_META.into(), "current".into(), serde_json::to_value(&run)?));
        let extra = crate::engine::Extra {
            kv,
            events: vec![self.event(
                ctx,
                &ctx.workspace,
                "authority.computed",
                None,
                now,
                json!({ "ranked": run.ranked, "kinds": run.kinds }),
                Vec::new(),
            )],
            ..Default::default()
        };
        self.apply_inner(ctx, Vec::new(), extra)?;
        for k in stale {
            let _ = self.store().kv_delete(NS, &k);
        }
        self.reload_authority()?;
        Ok(run)
    }

    /// Load stored scores into memory (at open, and when another process recomputed them).
    pub fn reload_authority(&self) -> Result<()> {
        let Some(run) = self.store().kv_get(NS_META, "current")? else {
            self.set_authority(Arc::new(HashMap::new()));
            return Ok(());
        };
        let at = run.get("computed_at").and_then(Value::as_i64).unwrap_or(0);
        let scores: HashMap<Id, f32> = self
            .store()
            .kv_scan(NS, "")?
            .into_iter()
            .filter(|(_, v)| v.get("computed_at").and_then(Value::as_i64) == Some(at))
            .filter_map(|(k, v)| v.get("score").and_then(Value::as_f64).map(|s| (k, s as f32)))
            .collect();
        self.set_authority(Arc::new(scores));
        Ok(())
    }

    /// The stored authority of one entity the caller can see.
    pub fn authority(&self, p: &Principal, id: &str) -> Result<Option<Authority>> {
        let id = self.resolve_id(id);
        {
            let g = self.inner_graph_read();
            match g.get(&id) {
                Some(e) if policy::visible(p, e, &g) => {}
                _ => return Err(Error::not_found(&id)),
            }
        }
        match self.store().kv_get(NS, &id)? {
            Some(v) => Ok(Some(serde_json::from_value(v)?)),
            None => Ok(None),
        }
    }

    /// The highest-authority entities the caller can see.
    pub fn authority_top(
        &self,
        p: &Principal,
        kinds: &[String],
        workspace: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Authority>> {
        let scores = self.authority_scores();
        let schema = self.schema();
        let ids: Vec<Id> = {
            let g = self.inner_graph_read();
            let mut v: Vec<(&Id, f32)> = scores
                .iter()
                .filter(|(id, _)| {
                    g.get(id).is_some_and(|e| {
                        policy::visible(p, e, &g)
                            && workspace.is_none_or(|w| e.workspace == w)
                            && (kinds.is_empty() || kinds.iter().any(|k| schema.kind_is(&e.kind, k)))
                    })
                })
                .map(|(id, s)| (id, *s))
                .collect();
            v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            v.into_iter().take(limit.clamp(1, 500)).map(|(id, _)| id.clone()).collect()
        };
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(v) = self.store().kv_get(NS, &id)? {
                out.push(serde_json::from_value(v)?);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EntityInput, Mutation};
    use crate::{ActionContext, Principal};

    #[test]
    fn used_documented_owned_assets_rank_first_and_explain_why() {
        let mut schema = Schema::enterprise();
        schema.extend_toml(crate::schema::builtin_package("data-catalog").unwrap()).unwrap();
        let o = Ontology::in_memory(schema);
        let ctx = ActionContext::new(Principal::admin("ada"), "acme");
        let table = |k: &str, desc: &str, reads: i64| {
            EntityInput::new("table", k, k)
                .prop("description", desc)
                .prop("reads", reads)
                .prop("documented_columns", if desc.is_empty() { 0.0 } else { 1.0 })
        };
        o.apply(
            &ctx,
            vec![
                Mutation::Upsert(EntityInput::new("team", "finance", "Finance")),
                Mutation::Upsert(
                    table("wh.fin.revenue", "Recognized revenue by day", 5_000).link("owned_by", "team:finance"),
                ),
                Mutation::Upsert(table("wh.tmp.revenue_old", "", 3)),
                Mutation::Upsert(
                    EntityInput::new("query", "wh.1", "select sum(amount) from revenue")
                        .prop("calls", 12_000)
                        .link("reads_from", "table:wh.fin.revenue"),
                ),
            ],
        )
        .unwrap();
        let run = o.compute_authority(&ctx, &[], &Weights::default()).unwrap();
        assert_eq!(run.ranked, 3, "two tables and a query; teams are not ranked by default");
        let admin = Principal::admin("x");
        let good = o.authority(&admin, "table:wh.fin.revenue").unwrap().unwrap();
        let bad = o.authority(&admin, "table:wh.tmp.revenue_old").unwrap().unwrap();
        assert!(good.score > bad.score + 0.3, "{good:?} vs {bad:?}");
        assert_eq!(good.components.owner, 1.0);
        assert_eq!(good.components.documentation, 1.0);
        assert!(good.components.centrality > bad.components.centrality);
        assert_eq!(good.evidence["owner"], "team:finance");
        assert!(o.authority_scores().get("table:wh.fin.revenue").is_some(), "kept in memory for ranking");
        let top = o.authority_top(&admin, &["table".into()], None, 5).unwrap();
        assert_eq!(top[0].id, "table:wh.fin.revenue");
        // Search ranks the authoritative table above the stale copy.
        let hits = o.search(&admin, "revenue", &["table".into()], None, 5).unwrap();
        assert_eq!(hits[0].entity.id, "table:wh.fin.revenue");
    }
}
