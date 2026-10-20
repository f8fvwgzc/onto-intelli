//! Graph reads: traversal, shortest path, filtered find and scope sets.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::graph::{Graph, Step};
use crate::model::{Entity, Id, one_or_many};
use crate::schema::{Dir, Schema};

/// Edge in canonical (stored) direction.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    pub from: Id,
    pub rel: String,
    pub to: Id,
}

/// One step of a walk, named as walked (`owned_by` when walking `owns` backwards).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hop {
    pub from: Id,
    pub rel: String,
    pub to: Id,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Out,
    In,
    #[default]
    Both,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TraverseSpec {
    #[serde(default, deserialize_with = "one_or_many")]
    pub start: Vec<Id>,
    /// Relations to follow (canonical, inverse or `^rel`); empty = all.
    #[serde(default, deserialize_with = "one_or_many")]
    pub rels: Vec<String>,
    /// Direction used when `rels` is empty.
    #[serde(default)]
    pub direction: Direction,
    #[serde(default = "default_depth")]
    pub depth: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Only include and pass through these kinds; empty = all.
    #[serde(default, deserialize_with = "one_or_many")]
    pub kinds: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Subgraph {
    pub nodes: Vec<Entity>,
    pub edges: Vec<Edge>,
    pub truncated: bool,
}

fn default_depth() -> usize {
    2
}

fn default_limit() -> usize {
    100
}

/// Steps from `id` restricted to `rels` (resolved), or all edges in `direction`.
pub fn expand<'a>(g: &'a Graph, id: &str, rels: &'a [(String, Dir)], direction: Direction) -> Vec<Step<'a>> {
    if rels.is_empty() {
        g.steps(id)
            .filter(|s| match direction {
                Direction::Both => true,
                Direction::Out => s.dir == Dir::Out,
                Direction::In => s.dir == Dir::In,
            })
            .collect()
    } else {
        rels.iter().flat_map(|(base, dir)| g.steps_via(id, base, *dir)).collect()
    }
}

pub fn resolve_rels(schema: &Schema, rels: &[String]) -> Vec<(String, Dir)> {
    rels.iter().map(|r| schema.resolve_rel_lenient(r)).collect()
}

pub fn canonical(from: &str, step: &Step<'_>) -> Edge {
    match step.dir {
        Dir::In => Edge { from: step.other.to_string(), rel: step.rel.to_string(), to: from.to_string() },
        _ => Edge { from: from.to_string(), rel: step.rel.to_string(), to: step.other.to_string() },
    }
}

pub fn traverse(schema: &Schema, g: &Graph, spec: &TraverseSpec, can_read: &dyn Fn(&Entity) -> bool) -> Subgraph {
    let rels = resolve_rels(schema, &spec.rels);
    let kind_ok = |e: &Entity| spec.kinds.is_empty() || spec.kinds.iter().any(|k| schema.kind_is(&e.kind, k));
    let mut out = Subgraph::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut edges: HashSet<Edge> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    for id in &spec.start {
        if let Some(e) = g.get(id)
            && can_read(e)
            && seen.insert(id.clone())
        {
            out.nodes.push(e.clone());
            queue.push_back((id.clone(), 0));
        }
    }
    while let Some((id, depth)) = queue.pop_front() {
        if depth >= spec.depth {
            continue;
        }
        for step in expand(g, &id, &rels, spec.direction) {
            let Some(other) = g.get(step.other) else { continue };
            if !can_read(other) || !kind_ok(other) {
                continue;
            }
            if !seen.contains(step.other) {
                if out.nodes.len() >= spec.limit {
                    out.truncated = true;
                    continue;
                }
                seen.insert(step.other.to_string());
                out.nodes.push(other.clone());
                queue.push_back((step.other.to_string(), depth + 1));
            }
            edges.insert(canonical(&id, &step));
        }
    }
    let mut edges: Vec<Edge> = edges.into_iter().collect();
    edges.sort_by(|a, b| (&a.from, &a.rel, &a.to).cmp(&(&b.from, &b.rel, &b.to)));
    out.edges = edges;
    out
}

/// Breadth-first shortest path between two entities over any relation.
pub fn shortest_path(
    schema: &Schema,
    g: &Graph,
    from: &str,
    to: &str,
    max_depth: usize,
    can_read: &dyn Fn(&Entity) -> bool,
) -> Option<Vec<Hop>> {
    if from == to {
        return Some(Vec::new());
    }
    // Bidirectional breadth-first search: expand the smaller frontier one
    // level at a time and stop where the two searches meet. Each side keeps
    // how it reached a node: (previous node, relation, direction walked).
    type Parents = HashMap<String, Option<(String, String, Dir)>>;
    let mut fwd: Parents = HashMap::from([(from.to_string(), None)]);
    let mut bwd: Parents = HashMap::from([(to.to_string(), None)]);
    let (mut ff, mut fb) = (vec![from.to_string()], vec![to.to_string()]);
    let mut depth = 0;
    while !ff.is_empty() && !fb.is_empty() && depth < max_depth {
        let forward = ff.len() <= fb.len();
        let (frontier, mine, other) = if forward { (&mut ff, &mut fwd, &bwd) } else { (&mut fb, &mut bwd, &fwd) };
        let mut next = Vec::new();
        let mut meet: Option<String> = None;
        'level: for id in frontier.iter() {
            for step in g.steps(id) {
                if mine.contains_key(step.other) || !g.get(step.other).is_some_and(can_read) {
                    continue;
                }
                mine.insert(step.other.to_string(), Some((id.clone(), step.rel.to_string(), step.dir)));
                if other.contains_key(step.other) {
                    meet = Some(step.other.to_string());
                    break 'level;
                }
                next.push(step.other.to_string());
            }
        }
        if let Some(m) = meet {
            let mut head = Vec::new();
            let mut cur = m.clone();
            while let Some(Some((prev, rel, dir))) = fwd.get(&cur) {
                head.push(Hop { from: prev.clone(), rel: schema.display_rel(rel, *dir), to: cur.clone() });
                cur = prev.clone();
            }
            head.reverse();
            let mut cur = m;
            while let Some(Some((prev, rel, dir))) = bwd.get(&cur) {
                let back = match dir {
                    Dir::Out => Dir::In,
                    Dir::In => Dir::Out,
                    Dir::Both => Dir::Both,
                };
                head.push(Hop { from: cur.clone(), rel: schema.display_rel(rel, back), to: prev.clone() });
                cur = prev.clone();
            }
            return Some(head);
        }
        *frontier = next;
        depth += 1;
    }
    None
}

/// "Payment timeout —affects→ Payment API —owned_by→ Payments Team"
pub fn hops_text(g: &Graph, hops: &[Hop]) -> String {
    let label = |id: &str| g.get(id).map(|e| e.label().to_string()).unwrap_or_else(|| id.to_string());
    let Some(first) = hops.first() else { return String::new() };
    let mut s = label(&first.from);
    for h in hops {
        s.push_str(&format!(" —{}→ {}", h.rel, label(&h.to)));
    }
    s
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Related {
    pub rel: String,
    pub to: Id,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FindSpec {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub state: Vec<String>,
    /// true: only non-terminal states; false: only terminal ones.
    #[serde(default)]
    pub open: Option<bool>,
    /// Property equality filters.
    #[serde(default)]
    pub props: Map<String, Value>,
    /// Must have this relation to `to` (e.g. assigned_to person:kenji).
    #[serde(default)]
    pub related: Option<Related>,
    /// Restrict to things inside this entity's scope (team, project, person).
    #[serde(default)]
    pub within: Option<Id>,
    #[serde(default = "default_find_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

fn default_find_limit() -> usize {
    50
}

/// Returns (page, total matches).
pub fn find(schema: &Schema, g: &Graph, spec: &FindSpec, can_read: &dyn Fn(&Entity) -> bool) -> (Vec<Entity>, usize) {
    let candidates: Vec<&Entity> = match (&spec.within, &spec.kind) {
        (Some(root), _) => scope_set(schema, g, root, 3).iter().filter_map(|id| g.get(id)).collect(),
        (None, Some(kind)) => {
            let mut v: Vec<&Entity> = g.of_kind(kind).collect();
            for (k, _) in g.kinds() {
                if k != kind && schema.kind_is(k, kind) {
                    v.extend(g.of_kind(k));
                }
            }
            v
        }
        (None, None) => g.entities().collect(),
    };
    let related = spec.related.as_ref().map(|r| (schema.resolve_rel_lenient(&r.rel), r.to.as_str()));
    let mut hits: Vec<&Entity> = candidates
        .into_iter()
        .filter(|e| spec.kind.as_ref().is_none_or(|k| schema.kind_is(&e.kind, k)))
        .filter(|e| spec.workspace.as_ref().is_none_or(|w| &e.workspace == w))
        .filter(|e| spec.state.is_empty() || e.state.as_ref().is_some_and(|s| spec.state.contains(s)))
        .filter(|e| spec.open.is_none_or(|open| schema.is_open(&e.kind, e.state.as_deref()) == open))
        .filter(|e| spec.props.iter().all(|(k, v)| e.props.get(k) == Some(v)))
        .filter(|e| related.as_ref().is_none_or(|((base, dir), to)| g.neighbors(&e.id, base, *dir).contains(to)))
        .filter(|e| can_read(e))
        .collect();
    hits.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
    let total = hits.len();
    let page = hits.into_iter().skip(spec.offset).take(spec.limit).cloned().collect();
    (page, total)
}

/// Everything "inside" `root` following the schema's `analysis.scope` relations.
pub fn scope_set(schema: &Schema, g: &Graph, root: &str, depth: usize) -> Vec<Id> {
    let rels = resolve_rels(schema, &schema.analysis.scope);
    let mut seen: HashSet<String> = HashSet::from([root.to_string()]);
    let mut order = Vec::new();
    let mut queue = VecDeque::from([(root.to_string(), 0usize)]);
    while let Some((id, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        for step in expand(g, &id, &rels, Direction::Both) {
            if seen.insert(step.other.to_string()) {
                order.push(step.other.to_string());
                queue.push_back((step.other.to_string(), d + 1));
            }
        }
    }
    order
}
