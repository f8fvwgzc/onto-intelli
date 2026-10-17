//! Dependency intelligence: upstream blockers, downstream impact, root-cause
//! chains, cycle detection and critical-path planning.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::graph::Graph;
use crate::model::{Entity, Id};
use crate::query::{Direction, Hop, expand, hops_text, resolve_rels};
use crate::schema::{Dir, Schema};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepDirection {
    /// What the root waits on.
    #[default]
    Upstream,
    /// What waits on the root.
    Downstream,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DepNode {
    pub id: Id,
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub open: bool,
    pub alert: bool,
    pub depth: usize,
}

impl DepNode {
    fn of(schema: &Schema, e: &Entity, depth: usize) -> Self {
        Self {
            id: e.id.clone(),
            kind: e.kind.clone(),
            title: e.label().to_string(),
            state: e.state.clone(),
            open: schema.is_open(&e.kind, e.state.as_deref()),
            alert: schema.is_alert(&e.kind, e.state.as_deref()),
            depth,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DepReport {
    pub root: Id,
    pub direction: String,
    pub nodes: Vec<DepNode>,
    pub edges: Vec<Hop>,
    /// Upstream: unfinished things the root waits on. Downstream/impact: open dependents.
    pub open: Vec<Id>,
    pub cycles: Vec<Vec<Id>>,
}

/// Breadth-first walk with predecessor hops.
fn walk(
    schema: &Schema,
    g: &Graph,
    root: &str,
    rels: &[(String, Dir)],
    depth: usize,
    can_read: &dyn Fn(&Entity) -> bool,
) -> (Vec<DepNode>, Vec<Hop>, HashMap<String, Hop>) {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut prev: HashMap<String, Hop> = HashMap::new();
    let mut seen = HashSet::from([root.to_string()]);
    let mut queue = VecDeque::from([(root.to_string(), 0usize)]);
    while let Some((id, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        for step in expand(g, &id, rels, Direction::Both) {
            let Some(other) = g.get(step.other) else { continue };
            if !can_read(other) {
                continue;
            }
            let hop = Hop { from: id.clone(), rel: schema.display_rel(step.rel, step.dir), to: step.other.to_string() };
            edges.push(hop.clone());
            if seen.insert(step.other.to_string()) {
                nodes.push(DepNode::of(schema, other, d + 1));
                prev.insert(step.other.to_string(), hop);
                queue.push_back((step.other.to_string(), d + 1));
            }
        }
    }
    (nodes, edges, prev)
}

pub fn dependencies(
    schema: &Schema,
    g: &Graph,
    root: &str,
    direction: DepDirection,
    depth: usize,
    can_read: &dyn Fn(&Entity) -> bool,
) -> DepReport {
    let dir = match direction {
        DepDirection::Upstream => Dir::Out,
        DepDirection::Downstream => Dir::In,
    };
    let rels: Vec<(String, Dir)> = schema.dependency_rels().into_iter().map(|r| (r, dir)).collect();
    let (nodes, edges, _) = walk(schema, g, root, &rels, depth, can_read);
    let open = nodes.iter().filter(|n| n.open).map(|n| n.id.clone()).collect();
    let mut ids: Vec<Id> = nodes.iter().map(|n| n.id.clone()).collect();
    ids.push(root.to_string());
    DepReport {
        root: root.to_string(),
        direction: format!("{direction:?}").to_lowercase(),
        nodes,
        edges,
        open,
        cycles: cycles_among(schema, g, &ids),
    }
}

/// What is affected if `root` fails or slips (schema `analysis.impact`).
pub fn impact(schema: &Schema, g: &Graph, root: &str, depth: usize, can_read: &dyn Fn(&Entity) -> bool) -> DepReport {
    let mut rels = resolve_rels(schema, &schema.analysis.impact);
    if rels.is_empty() {
        rels = schema.dependency_rels().into_iter().map(|r| (r, Dir::In)).collect();
    }
    let (nodes, edges, _) = walk(schema, g, root, &rels, depth, can_read);
    let open = nodes.iter().filter(|n| n.open).map(|n| n.id.clone()).collect();
    DepReport { root: root.to_string(), direction: "impact".into(), nodes, edges, open, cycles: Vec::new() }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CauseChain {
    pub culprit: DepNode,
    pub hops: Vec<Hop>,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RootCause {
    pub root: Id,
    /// Deepest culprits first: the further down the chain, the more likely the root cause.
    pub chains: Vec<CauseChain>,
    pub visited: usize,
}

/// Explain why `root` is blocked/failing: walk `analysis.root_cause` relations
/// and return the path to every entity in an alert state.
pub fn root_cause(
    schema: &Schema,
    g: &Graph,
    root: &str,
    depth: usize,
    can_read: &dyn Fn(&Entity) -> bool,
) -> RootCause {
    let mut rels = resolve_rels(schema, &schema.analysis.root_cause);
    if rels.is_empty() {
        rels = schema.dependency_rels().into_iter().map(|r| (r, Dir::Out)).collect();
    }
    let (nodes, _, prev) = walk(schema, g, root, &rels, depth, can_read);
    let mut chains: Vec<CauseChain> = nodes
        .iter()
        .filter(|n| n.alert)
        .map(|n| {
            let mut hops = Vec::new();
            let mut cur = n.id.clone();
            while let Some(h) = prev.get(&cur) {
                hops.push(h.clone());
                cur = h.from.clone();
            }
            hops.reverse();
            CauseChain { culprit: n.clone(), text: hops_text(g, &hops), hops }
        })
        .collect();
    chains.sort_by(|a, b| b.culprit.depth.cmp(&a.culprit.depth).then_with(|| a.culprit.id.cmp(&b.culprit.id)));
    chains.truncate(12);
    RootCause { root: root.to_string(), chains, visited: nodes.len() }
}

/// Dependency cycles (strongly connected components) in a workspace.
pub fn cycles(schema: &Schema, g: &Graph, workspace: Option<&str>, can_read: &dyn Fn(&Entity) -> bool) -> Vec<Vec<Id>> {
    let ids: Vec<Id> = g
        .entities()
        .filter(|e| workspace.is_none_or(|w| e.workspace == w) && can_read(e))
        .map(|e| e.id.clone())
        .collect();
    cycles_among(schema, g, &ids)
}

fn cycles_among(schema: &Schema, g: &Graph, ids: &[Id]) -> Vec<Vec<Id>> {
    let set: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let succ = |id: &str| -> Vec<String> {
        g.out_edges(id)
            .iter()
            .filter(|(r, t)| schema.is_dependency(r) && set.contains(t.as_str()))
            .map(|(_, t)| t.clone())
            .collect()
    };
    // Iterative Tarjan.
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut low: HashMap<String, usize> = HashMap::new();
    let mut on_stack: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();
    let mut next = 0usize;
    let mut out = Vec::new();
    let mut sorted: Vec<&str> = set.iter().copied().collect();
    sorted.sort();
    for start in sorted {
        if index.contains_key(start) {
            continue;
        }
        let mut work: Vec<(String, Vec<String>, usize)> = vec![(start.to_string(), succ(start), 0)];
        index.insert(start.to_string(), next);
        low.insert(start.to_string(), next);
        next += 1;
        stack.push(start.to_string());
        on_stack.insert(start.to_string());
        while let Some(top) = work.last_mut() {
            if top.2 < top.1.len() {
                let w = top.1[top.2].clone();
                top.2 += 1;
                let v = top.0.clone();
                if !index.contains_key(&w) {
                    index.insert(w.clone(), next);
                    low.insert(w.clone(), next);
                    next += 1;
                    stack.push(w.clone());
                    on_stack.insert(w.clone());
                    let ws = succ(&w);
                    work.push((w, ws, 0));
                } else if on_stack.contains(&w) {
                    let lw = index[&w].min(low[&v]);
                    low.insert(v, lw);
                }
            } else {
                let (v, _, _) = work.pop().expect("non-empty");
                if let Some((parent, _, _)) = work.last() {
                    let lp = low[parent.as_str()].min(low[&v]);
                    low.insert(parent.clone(), lp);
                }
                if low[&v] == index[&v] {
                    let mut comp = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack.remove(&w);
                        let done = w == v;
                        comp.push(w);
                        if done {
                            break;
                        }
                    }
                    let self_loop = comp.len() == 1 && succ(&comp[0]).contains(&comp[0]);
                    if comp.len() > 1 || self_loop {
                        comp.sort();
                        out.push(comp);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CriticalPath {
    pub root: Id,
    /// Longest chain of remaining work, first prerequisite first.
    pub path: Vec<DepNode>,
    /// Sum of remaining estimates along the path (days by convention).
    pub cost: f64,
    /// A valid execution order for all open work items under the root.
    pub order: Vec<Id>,
    /// Items stuck in a dependency cycle (cannot be ordered).
    pub cyclic: Vec<Id>,
}

/// Plan the open work under `root` (via `analysis.containment`) and find its critical path.
pub fn critical_path(schema: &Schema, g: &Graph, root: &str, can_read: &dyn Fn(&Entity) -> bool) -> CriticalPath {
    let containment = resolve_rels(schema, &schema.analysis.containment);
    let mut items: Vec<Id> = Vec::new();
    let mut seen = HashSet::from([root.to_string()]);
    let mut queue = VecDeque::from([(root.to_string(), 0usize)]);
    while let Some((id, d)) = queue.pop_front() {
        if d >= 4 {
            continue;
        }
        for step in expand(g, &id, &containment, Direction::Both) {
            if seen.insert(step.other.to_string()) {
                items.push(step.other.to_string());
                queue.push_back((step.other.to_string(), d + 1));
            }
        }
    }
    if items.is_empty() {
        items.push(root.to_string());
    }
    let open: Vec<&Entity> = items
        .iter()
        .filter_map(|id| g.get(id))
        .filter(|e| can_read(e) && schema.is_open(&e.kind, e.state.as_deref()))
        .collect();
    let in_set: HashSet<&str> = open.iter().map(|e| e.id.as_str()).collect();
    // prerequisites: A depends_on/blocked_by B  =>  B before A
    let prereqs: HashMap<&str, Vec<&str>> = open
        .iter()
        .map(|e| {
            let p = g
                .out_edges(&e.id)
                .iter()
                .filter(|(r, t)| schema.is_dependency(r) && in_set.contains(t.as_str()))
                .map(|(_, t)| t.as_str())
                .collect();
            (e.id.as_str(), p)
        })
        .collect();
    let estimate = |id: &str| -> f64 {
        g.get(id)
            .and_then(|e| e.props.get(&schema.analysis.estimate_property))
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(1.0)
    };
    // Kahn's algorithm.
    let mut indeg: HashMap<&str, usize> = open.iter().map(|e| (e.id.as_str(), prereqs[e.id.as_str()].len())).collect();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (node, ps) in &prereqs {
        for p in ps {
            dependents.entry(*p).or_default().push(*node);
        }
    }
    // Kept sorted descending so `pop` yields the smallest id: deterministic order.
    let mut ready: Vec<&str> = indeg.iter().filter(|(_, d)| **d == 0).map(|(k, _)| *k).collect();
    ready.sort_by(|a, b| b.cmp(a));
    let mut order: Vec<&str> = Vec::new();
    let mut dist: HashMap<&str, (f64, Option<&str>)> = HashMap::new();
    while let Some(n) = ready.pop() {
        order.push(n);
        let best =
            prereqs[n].iter().filter_map(|p| dist.get(p).map(|(d, _)| (*d, *p))).max_by(|a, b| a.0.total_cmp(&b.0));
        let (base, pred) = best.map(|(d, p)| (d, Some(p))).unwrap_or((0.0, None));
        dist.insert(n, (base + estimate(n), pred));
        if let Some(ds) = dependents.get(n) {
            for d in ds {
                let e = indeg.get_mut(d).expect("dependent in set");
                *e -= 1;
                if *e == 0 {
                    ready.push(*d);
                    ready.sort_by(|a, b| b.cmp(a));
                }
            }
        }
    }
    let cyclic: Vec<Id> = {
        let ordered: HashSet<&str> = order.iter().copied().collect();
        let mut c: Vec<Id> = in_set.iter().filter(|id| !ordered.contains(*id)).map(|s| s.to_string()).collect();
        c.sort();
        c
    };
    let end = dist.iter().max_by(|a, b| a.1.0.total_cmp(&b.1.0).then_with(|| b.0.cmp(a.0))).map(|(k, v)| (*k, v.0));
    let mut path = Vec::new();
    let mut cost = 0.0;
    if let Some((end, total)) = end {
        cost = total;
        let mut cur = Some(end);
        while let Some(id) = cur {
            if let Some(e) = g.get(id) {
                path.push(DepNode::of(schema, e, 0));
            }
            cur = dist.get(id).and_then(|(_, p)| *p);
        }
        path.reverse();
        for (i, n) in path.iter_mut().enumerate() {
            n.depth = i;
        }
    }
    CriticalPath { root: root.to_string(), path, cost, order: order.into_iter().map(str::to_string).collect(), cyclic }
}
