//! Workspace map queries: a readable overview of a workspace, and the graph
//! around some entities as it was at a point in time (history slider).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::model::{Entity, Id, Link};
use crate::policy::{self, Principal};
use crate::query::{Edge, Subgraph};
use crate::schema::Category;
use crate::time::now_ms;
use crate::{Error, Ontology, Result};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OverviewSpec {
    #[serde(default)]
    pub workspace: Option<String>,
    /// Kinds or interfaces to include (empty: all).
    #[serde(default, deserialize_with = "crate::model::one_or_many")]
    pub kinds: Vec<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    150
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotSpec {
    /// Where the walk starts.
    #[serde(deserialize_with = "crate::model::one_or_many")]
    pub start: Vec<Id>,
    /// Unix ms.
    pub ts: i64,
    #[serde(default = "default_depth")]
    pub depth: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_depth() -> usize {
    2
}

/// Per-kind counts with the schema's display hints, for map legends.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KindCount {
    pub kind: String,
    pub count: usize,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Overview {
    #[serde(flatten)]
    pub graph: Subgraph,
    pub kinds: Vec<KindCount>,
    /// Visible entities in scope (the map shows at most `limit`).
    pub total: usize,
}

impl Ontology {
    /// The most connected entities of a workspace (containers and actors
    /// first) and the links among them: a first view of the map.
    pub fn overview(&self, p: &Principal, spec: &OverviewSpec) -> Result<Overview> {
        let schema = self.schema();
        let g = self.inner_graph_read();
        if !p.allows(crate::policy::Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let now = now_ms();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut scored: Vec<(f64, &Entity)> = Vec::new();
        for e in g.entities() {
            if !policy::visible(p, e, &g)
                || spec.workspace.as_ref().is_some_and(|w| &e.workspace != w)
                || (!spec.kinds.is_empty() && !spec.kinds.iter().any(|k| schema.kind_is(&e.kind, k)))
            {
                continue;
            }
            *counts.entry(e.kind.clone()).or_default() += 1;
            let degree = (g.out_edges(&e.id).len() + g.in_edges(&e.id).len()) as f64;
            let bonus = match schema.category(&e.kind) {
                Category::Container => 6.0,
                Category::Actor => 3.0,
                _ => 0.0,
            };
            let idle_days = ((now - e.updated_at).max(0) as f64) / 86_400_000.0;
            let recency = 2.0 * (-idle_days / 30.0).exp();
            scored.push((degree.ln_1p() * 3.0 + bonus + recency, e));
        }
        let total = scored.len();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        scored.truncate(spec.limit.clamp(1, 2_000));
        let ids: HashSet<&str> = scored.iter().map(|(_, e)| e.id.as_str()).collect();
        let mut edges = Vec::new();
        for (_, e) in &scored {
            for (rel, to) in g.out_edges(&e.id) {
                if ids.contains(to.as_str()) {
                    edges.push(Edge { from: e.id.clone(), rel: rel.clone(), to: to.clone() });
                }
            }
        }
        edges.sort_by(|a, b| (&a.from, &a.rel, &a.to).cmp(&(&b.from, &b.rel, &b.to)));
        let kinds = counts
            .into_iter()
            .map(|(kind, count)| {
                let ty = schema.object_type(&kind);
                KindCount {
                    label: ty.map(|t| t.label.clone()).filter(|l| !l.is_empty()).unwrap_or_else(|| kind.clone()),
                    color: ty.map(|t| t.color.clone()).unwrap_or_default(),
                    icon: ty.map(|t| t.icon.clone()).unwrap_or_default(),
                    kind,
                    count,
                }
            })
            .collect();
        Ok(Overview {
            graph: Subgraph {
                nodes: scored.into_iter().map(|(_, e)| e.clone()).collect(),
                edges,
                truncated: total > spec.limit,
            },
            kinds,
            total,
        })
    }

    /// The graph around `start` as it was at `ts`, from entity and link
    /// history. Entities and links older than the history (imported in bulk)
    /// count as present since their creation time.
    pub fn snapshot(&self, p: &Principal, spec: &SnapshotSpec) -> Result<Subgraph> {
        if !p.allows(crate::policy::Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let store = self.store().clone();
        let mut entities: HashMap<Id, Option<Entity>> = HashMap::new();
        let entity_at = |id: &str, cache: &mut HashMap<Id, Option<Entity>>| -> Result<Option<Entity>> {
            if let Some(e) = cache.get(id) {
                return Ok(e.clone());
            }
            let versions = store.versions(id, 10_000)?;
            let at = if versions.is_empty() {
                // No history: present now and created before `ts`.
                let g = self.inner_graph_read();
                g.get(id).filter(|e| e.created_at <= spec.ts).cloned()
            } else {
                versions.iter().find(|v| v.ts <= spec.ts).and_then(|v| v.doc.clone())
            };
            let at = at.filter(|e| {
                let g = self.inner_graph_read();
                policy::visible(p, e, &g)
            });
            cache.insert(id.to_string(), at.clone());
            Ok(at)
        };
        let links_at = |id: &str| -> Result<Vec<Link>> {
            let history = store.link_history(id, spec.ts, 50_000)?;
            let mut latest: HashMap<(Id, String, Id), Option<Link>> = HashMap::new();
            for v in history {
                latest.entry((v.from.clone(), v.rel.clone(), v.to.clone())).or_insert(v.doc);
            }
            let mut out: Vec<Link> = latest.values().flatten().cloned().collect();
            // Links with no history at all count from their creation time.
            let g = self.inner_graph_read();
            for (rel, to) in g.out_edges(id) {
                let key = (id.to_string(), rel.clone(), to.clone());
                if !latest.contains_key(&key)
                    && let Some(l) = g.get_link(id, rel, to)
                    && l.created_at <= spec.ts
                {
                    out.push(l.clone());
                }
            }
            for (rel, from) in g.in_edges(id) {
                let key = (from.clone(), rel.clone(), id.to_string());
                if !latest.contains_key(&key)
                    && let Some(l) = g.get_link(from, rel, id)
                    && l.created_at <= spec.ts
                {
                    out.push(l.clone());
                }
            }
            Ok(out)
        };
        let limit = spec.limit.clamp(1, 1_000);
        let mut out = Subgraph::default();
        let mut seen: HashSet<Id> = HashSet::new();
        let mut edges: HashSet<Edge> = HashSet::new();
        let mut queue: VecDeque<(Id, usize)> = VecDeque::new();
        for s in &spec.start {
            let id = self.resolve_id(s);
            if entity_at(&id, &mut entities)?.is_some() && seen.insert(id.clone()) {
                queue.push_back((id, 0));
            }
        }
        while let Some((id, depth)) = queue.pop_front() {
            if let Some(Some(e)) = entities.get(&id) {
                out.nodes.push(e.clone());
            }
            if depth >= spec.depth {
                continue;
            }
            for l in links_at(&id)? {
                let other = if l.from == id { l.to.clone() } else { l.from.clone() };
                if entity_at(&other, &mut entities)?.is_none() {
                    continue;
                }
                if !seen.contains(&other) {
                    if seen.len() >= limit {
                        out.truncated = true;
                        continue;
                    }
                    seen.insert(other.clone());
                    queue.push_back((other.clone(), depth + 1));
                }
                edges.insert(Edge { from: l.from.clone(), rel: l.rel.clone(), to: l.to.clone() });
            }
        }
        let mut edges: Vec<Edge> =
            edges.into_iter().filter(|e| seen.contains(&e.from) && seen.contains(&e.to)).collect();
        edges.sort_by(|a, b| (&a.from, &a.rel, &a.to).cmp(&(&b.from, &b.rel, &b.to)));
        out.edges = edges;
        Ok(out)
    }
}
