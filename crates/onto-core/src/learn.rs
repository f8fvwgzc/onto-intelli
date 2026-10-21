//! The learned layer: what can be inferred from a crawled catalog and its
//! query log, kept apart from curated definitions.
//!
//! * **Join paths** from foreign keys and from the join conditions queries
//!   actually use.
//! * **Metric definitions** from aggregates in queries and views
//!   (`SUM(orders.amount) AS revenue`).
//! * **Synonyms** from column and table names (`cust_id` → customer) and
//!   from comments ("also known as ARR").
//!
//! Each becomes a `learned_fact` entity with a confidence, the evidence it
//! rests on and `concerns` links to the assets it is about, written through
//! the audited path by the `learner` principal. Facts start as `suggested`;
//! a person certifies or rejects them (`certify_fact`, `reject_fact`). The
//! learner never overwrites that decision and never suggests a rejected
//! fact again. On top of the facts sit the catalog questions answered
//! without a model: where data lives, how two tables join, which metric is
//! the trusted one.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::catalog::SqlRefs;
use crate::engine::ActionContext;
use crate::graph::Graph;
use crate::model::{Entity, EntityInput, EntityRef, Id, LinkSpec, Mutation, Provenance};
use crate::policy::{self, Principal};
use crate::schema::Dir;
use crate::time::now_ms;
use crate::{Ontology, Result};

/// Words that make an aggregate or a view read as a business metric.
const METRIC_WORDS: &[&str] = &[
    "revenue",
    "sales",
    "gmv",
    "arr",
    "mrr",
    "bookings",
    "income",
    "profit",
    "margin",
    "churn",
    "retention",
    "orders",
    "signups",
    "users",
    "customers",
    "spend",
    "cost",
    "price",
    "amount",
    "total",
    "count",
    "volume",
    "billings",
    "billing",
    "invoiced",
    "payments",
    "bookings",
    "conversion",
    "sessions",
];

/// Abbreviations common in column and table names.
const ABBREVIATIONS: &[(&str, &str)] = &[
    ("cust", "customer"),
    ("amt", "amount"),
    ("qty", "quantity"),
    ("rev", "revenue"),
    ("acct", "account"),
    ("txn", "transaction"),
    ("trx", "transaction"),
    ("dt", "date"),
    ("ts", "timestamp"),
    ("num", "number"),
    ("desc", "description"),
    ("addr", "address"),
    ("org", "organization"),
    ("emp", "employee"),
    ("dept", "department"),
    ("prod", "product"),
    ("cat", "category"),
    ("inv", "invoice"),
    ("pmt", "payment"),
    ("ord", "order"),
    ("usr", "user"),
    ("sku", "stock keeping unit"),
    ("ccy", "currency"),
    ("cnt", "count"),
    ("avg", "average"),
    ("pct", "percent"),
    ("bal", "balance"),
    ("mgr", "manager"),
    ("loc", "location"),
    ("ref", "reference"),
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub id: Id,
    pub fact_type: String,
    pub statement: String,
    pub confidence: f64,
    pub support: i64,
    pub evidence: Vec<String>,
    pub expression: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub term: String,
    #[serde(default)]
    pub means: String,
    /// The assets it is about.
    pub concerns: Vec<Id>,
    pub workspace: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LearnReport {
    pub join_paths: usize,
    pub metrics: usize,
    pub synonyms: usize,
    /// Facts written (new or changed).
    pub written: usize,
    /// Suggested facts withdrawn because nothing supports them any more.
    pub withdrawn: usize,
    /// Facts people already certified or rejected (left as they decided).
    pub decided: usize,
}

fn fnv(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:012x}", h & 0xffff_ffff_ffff)
}

fn round3(x: f32) -> f64 {
    (f64::from(x) * 1000.0).round() / 1000.0
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn name_of(e: &Entity) -> String {
    e.prop_str("name").map(str::to_string).unwrap_or_else(|| e.title.rsplit('.').next().unwrap_or(&e.title).to_string())
}

/// Tables and views by data source and name, for resolving query text.
struct Relations<'a> {
    by_source: HashMap<String, HashMap<String, Vec<&'a Entity>>>,
}

/// `table:{source}.{schema}.{name}` → source key.
pub fn source_of(id: &str) -> Option<&str> {
    id.split_once(':').and_then(|(_, rest)| rest.split_once('.')).map(|(s, _)| s)
}

impl<'a> Relations<'a> {
    fn of(g: &'a Graph) -> Self {
        let mut by_source: HashMap<String, HashMap<String, Vec<&'a Entity>>> = HashMap::new();
        for e in g.of_kind("table").chain(g.of_kind("view")) {
            let Some(src) = source_of(&e.id) else { continue };
            let names = by_source.entry(src.to_string()).or_default();
            let name = name_of(e).to_lowercase();
            if let Some(schema) = e.prop_str("schema_name") {
                names.entry(format!("{}.{name}", schema.to_lowercase())).or_default().push(e);
            }
            names.entry(name).or_default().push(e);
        }
        Self { by_source }
    }

    fn resolve(&self, source: &str, schema: Option<&str>, name: &str) -> Option<&'a Entity> {
        let names = self.by_source.get(source)?;
        let key = match schema {
            Some(s) => format!("{}.{}", s.to_lowercase(), name.to_lowercase()),
            None => name.to_lowercase(),
        };
        let found = names.get(&key)?;
        found.iter().find(|e| e.prop_str("schema_name") == Some("public")).or(found.first()).copied()
    }
}

/// Evidence for one join: foreign keys, and the queries that use it.
#[derive(Default)]
struct Join {
    fk: Vec<String>,
    queries: BTreeSet<String>,
    calls: i64,
    ws: String,
}

/// (left table, right table) ordered by id, with their column pairs.
type Joins = BTreeMap<(Id, Id, Vec<(String, String)>), Join>;

fn join_entry<'m>(joins: &'m mut Joins, a: &Entity, ac: &str, b: &Entity, bc: &str) -> Option<&'m mut Join> {
    if a.id == b.id {
        return None;
    }
    let (l, lc, r, rc) = if a.id <= b.id { (a, ac, b, bc) } else { (b, bc, a, ac) };
    let entry = joins.entry((l.id.clone(), r.id.clone(), vec![(lc.to_string(), rc.to_string())])).or_default();
    entry.ws = l.workspace.clone();
    Some(entry)
}

/// Everything the learner can infer from `g` (in `workspace`, or all).
pub fn infer(g: &Graph, workspace: Option<&str>) -> Vec<Fact> {
    let in_ws = |e: &Entity| workspace.is_none_or(|w| e.workspace == w);
    let rels = Relations::of(g);
    let mut facts: Vec<Fact> = Vec::new();

    // ── join paths ──
    let mut joins: Joins = BTreeMap::new();
    for col in g.of_kind("column").filter(|e| in_ws(e)) {
        for (rel, to) in g.out_edges(&col.id) {
            if rel != "references" {
                continue;
            }
            let (Some(ta), Some(tb)) = (
                g.neighbors(&col.id, "contains", Dir::In).first().and_then(|t| g.get(t)),
                g.neighbors(to, "contains", Dir::In).first().and_then(|t| g.get(t)),
            ) else {
                continue;
            };
            let (Some(cb), constraint) = (
                g.get(to),
                g.get_link(&col.id, rel, to).and_then(|l| l.props.get("constraint")).and_then(Value::as_str),
            ) else {
                continue;
            };
            if let Some(j) = join_entry(&mut joins, ta, &name_of(col), tb, &name_of(cb)) {
                j.fk.push(constraint.unwrap_or("foreign key").to_string());
            }
        }
    }
    // Queries: joins, and aggregates for metrics.
    #[derive(Default)]
    struct Metric {
        expression: String,
        table: Option<Id>,
        queries: BTreeSet<String>,
        views: Vec<Id>,
        calls: i64,
        ws: String,
    }
    let mut metrics: BTreeMap<(String, String), Metric> = BTreeMap::new();
    let mut read_sql = |sql: &str, source: &str, origin: &Entity, calls: i64, is_view: bool| {
        let refs = SqlRefs::read(sql);
        let tables: Vec<Option<&Entity>> =
            refs.tables.iter().map(|t| rels.resolve(source, t.schema.as_deref(), &t.name)).collect();
        for j in &refs.joins {
            let (Some(Some(a)), Some(Some(b))) = (tables.get(j.left), tables.get(j.right)) else { continue };
            if let Some(entry) = join_entry(&mut joins, a, &j.left_column, b, &j.right_column)
                && entry.queries.insert(origin.id.clone())
            {
                entry.calls += calls;
            }
        }
        for agg in &refs.aggregates {
            let table = agg.table.and_then(|t| tables.get(t).copied().flatten());
            let expr = match (table, &agg.column) {
                (Some(t), Some(c)) => format!("{}({}.{c})", agg.func, name_of(t)),
                _ => agg.expr.clone(),
            };
            let name = if is_view {
                name_of(origin)
            } else {
                match &agg.alias {
                    Some(a) => a.clone(),
                    None => match &agg.column {
                        Some(c) if METRIC_WORDS.iter().any(|w| c.contains(w)) => c.clone(),
                        _ => continue,
                    },
                }
            };
            let lname = name.to_lowercase();
            if !METRIC_WORDS.iter().any(|w| lname.contains(w)) {
                continue;
            }
            let m = metrics.entry((lname, expr.clone())).or_default();
            m.expression = expr;
            m.table = m.table.clone().or_else(|| table.map(|t| t.id.clone()));
            m.ws = origin.workspace.clone();
            if is_view {
                m.views.push(origin.id.clone());
            } else if m.queries.insert(origin.id.clone()) {
                m.calls += calls;
            }
        }
    };
    for q in g.of_kind("query").filter(|e| in_ws(e)) {
        let (Some(src), Some(text)) = (source_of(&q.id), q.prop_str("text")) else { continue };
        let calls = q.props.get("calls").and_then(Value::as_i64).unwrap_or(0);
        read_sql(text, src, q, calls, false);
    }
    for v in g.of_kind("view").filter(|e| in_ws(e)) {
        let (Some(src), Some(def)) = (source_of(&v.id), v.prop_str("definition")) else { continue };
        read_sql(def, src, v, 0, true);
    }

    for ((l, r, cols), j) in joins {
        let on = cols
            .iter()
            .map(|(a, b)| {
                format!(
                    "{}.{a} = {}.{b}",
                    g.get(&l).map(name_of).unwrap_or_default(),
                    g.get(&r).map(name_of).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let n_q = j.queries.len() as f64;
        let base = if j.fk.is_empty() { 0.45 } else { 0.9 };
        let confidence = (base + 0.08 * (1.0 + n_q).ln() + 0.02 * (1.0 + j.calls as f64).ln().min(5.0)).min(0.99);
        let mut evidence: Vec<String> = j.fk.iter().map(|c| format!("constraint {c}")).collect();
        evidence.extend(j.queries.iter().take(8).cloned());
        let (ln, rn) = (g.get(&l).map(name_of).unwrap_or_default(), g.get(&r).map(name_of).unwrap_or_default());
        facts.push(Fact {
            id: format!("learned_fact:join_path.{}", fnv(&format!("{l}|{r}|{on}"))),
            fact_type: "join_path".into(),
            statement: format!("{ln} joins {rn} on {on}"),
            confidence: round2(confidence),
            support: (j.fk.len() + j.queries.len()) as i64,
            evidence,
            expression: on,
            name: String::new(),
            term: String::new(),
            means: String::new(),
            concerns: vec![l, r],
            workspace: j.ws,
        });
    }
    for ((name, expr), m) in metrics {
        let n_q = m.queries.len() as f64;
        let base = if m.views.is_empty() { 0.4 } else { 0.6 };
        let confidence = (base + 0.1 * (1.0 + n_q).ln() + 0.02 * (1.0 + m.calls as f64).ln().min(5.0)).min(0.95);
        let mut concerns: Vec<Id> = m.views.clone();
        concerns.extend(m.table.clone());
        concerns.sort();
        concerns.dedup();
        let mut evidence: Vec<String> = m.views.iter().map(|v| format!("view {v}")).collect();
        evidence.extend(m.queries.iter().take(8).cloned());
        facts.push(Fact {
            id: format!("learned_fact:metric.{}", fnv(&format!("{name}|{expr}"))),
            fact_type: "metric".into(),
            statement: format!("{name} = {expr}"),
            confidence: round2(confidence),
            support: (m.views.len() + m.queries.len()) as i64,
            evidence,
            expression: expr,
            name,
            term: String::new(),
            means: String::new(),
            concerns,
            workspace: m.ws,
        });
    }

    // ── synonyms ──
    #[derive(Default)]
    struct Syn {
        assets: BTreeSet<Id>,
        from_comment: bool,
        ws: String,
    }
    let mut syns: BTreeMap<(String, String), Syn> = BTreeMap::new();
    for e in g.of_kind("column").chain(g.of_kind("table")).chain(g.of_kind("view")).filter(|e| in_ws(e)) {
        let name = name_of(e).to_lowercase();
        for part in name.split(['_', '-', ' ', '.']) {
            if let Some((_, full)) = ABBREVIATIONS.iter().find(|(abbr, _)| *abbr == part) {
                let s = syns.entry((part.to_string(), full.to_string())).or_default();
                s.assets.insert(e.id.clone());
                s.ws = e.workspace.clone();
            }
        }
        if let Some(c) = e.prop_str("description") {
            let lc = c.to_lowercase();
            for cue in ["also known as ", "also called ", "a.k.a. ", "aka "] {
                if let Some(pos) = lc.find(cue) {
                    let rest = &c[pos + cue.len()..];
                    let term: String = rest
                        .trim_start_matches(['"', '\'', '“'])
                        .chars()
                        .take_while(|ch| ch.is_alphanumeric() || *ch == ' ' || *ch == '-' || *ch == '_')
                        .collect::<String>()
                        .split_whitespace()
                        .take(4)
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !term.is_empty() {
                        let s = syns.entry((term.to_lowercase(), name.replace('_', " "))).or_default();
                        s.assets.insert(e.id.clone());
                        s.from_comment = true;
                        s.ws = e.workspace.clone();
                    }
                }
            }
        }
    }
    for ((term, means), s) in syns {
        let support = s.assets.len() as i64;
        let confidence = if s.from_comment { 0.85 } else { (0.5 + 0.05 * support as f64).min(0.8) };
        facts.push(Fact {
            id: format!("learned_fact:synonym.{}", fnv(&format!("{term}|{means}"))),
            fact_type: "synonym".into(),
            statement: format!("\"{term}\" means {means}"),
            confidence: round2(confidence),
            support,
            evidence: s.assets.iter().take(10).cloned().collect(),
            expression: String::new(),
            name: String::new(),
            term,
            means,
            concerns: s.assets.into_iter().take(25).collect(),
            workspace: s.ws,
        });
    }
    facts
}

fn props_of(f: &Fact) -> serde_json::Map<String, Value> {
    let mut p = serde_json::Map::new();
    p.insert("fact_type".into(), json!(f.fact_type));
    p.insert("statement".into(), json!(f.statement));
    p.insert("confidence".into(), json!(f.confidence));
    p.insert("support".into(), json!(f.support));
    p.insert("evidence".into(), json!(f.evidence));
    p.insert("learner".into(), json!("onto-intelli"));
    for (k, v) in [("expression", &f.expression), ("name", &f.name), ("term", &f.term), ("means", &f.means)] {
        if !v.is_empty() {
            p.insert(k.into(), json!(v));
        }
    }
    p
}

/// A join, metric or synonym and how much to trust it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactView {
    pub id: Id,
    pub fact_type: String,
    pub statement: String,
    pub state: String,
    pub confidence: f64,
    #[serde(default)]
    pub expression: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub certified_by: Option<String>,
    pub concerns: Vec<Id>,
    /// OntoRank of the fact, when computed.
    #[serde(default)]
    pub authority: Option<f64>,
}

/// Where a term lives: a table or view, and why it was chosen.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataLocation {
    pub id: Id,
    pub kind: String,
    pub title: String,
    pub score: f32,
    pub authority: Option<f64>,
    pub reasons: Vec<String>,
}

/// How two tables join: a certified or learned path, else a foreign-key chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JoinAnswer {
    pub from: Id,
    pub to: Id,
    /// Each hop: (left table, right table, condition).
    pub hops: Vec<(Id, Id, String)>,
    pub facts: Vec<FactView>,
    pub via: String,
}

impl Ontology {
    /// Infer facts from the catalog and write the ones that changed, as
    /// the `learner`, leaving people's decisions alone.
    pub fn learn(&self, workspace: Option<&str>) -> Result<LearnReport> {
        let learner = Principal::new("learner", vec![crate::policy::Role::System], vec!["*".into()]);
        let now = now_ms();
        let mut report = LearnReport::default();
        let muts = {
            let g = self.inner_graph_read();
            let facts = infer(&g, workspace);
            report.join_paths = facts.iter().filter(|f| f.fact_type == "join_path").count();
            report.metrics = facts.iter().filter(|f| f.fact_type == "metric").count();
            report.synonyms = facts.iter().filter(|f| f.fact_type == "synonym").count();
            let current: BTreeSet<&str> = facts.iter().map(|f| f.id.as_str()).collect();
            let mut muts: BTreeMap<String, Vec<Mutation>> = BTreeMap::new();
            for f in &facts {
                let props = props_of(f);
                let existing = g.get(&f.id);
                let state = existing.and_then(|e| e.state.clone());
                if matches!(state.as_deref(), Some("certified") | Some("rejected")) {
                    report.decided += 1;
                    if state.as_deref() == Some("rejected") {
                        continue;
                    }
                }
                let unchanged = existing.is_some_and(|e| {
                    props.iter().all(|(k, v)| e.props.get(k) == Some(v))
                        && f.concerns.iter().all(|c| g.neighbors(&f.id, "concerns", Dir::Out).contains(&c.as_str()))
                });
                if unchanged || f.concerns.iter().any(|c| !g.contains(c)) {
                    continue;
                }
                let input = EntityInput {
                    id: Some(f.id.clone()),
                    kind: "learned_fact".into(),
                    workspace: Some(f.workspace.clone()),
                    title: Some(f.statement.clone()),
                    state: if existing.is_none() { Some("suggested".into()) } else { None },
                    props,
                    provenance: Some(Provenance {
                        source: "learner".into(),
                        derived_from: f.concerns.clone(),
                        at: now,
                        ..Default::default()
                    }),
                    links: f
                        .concerns
                        .iter()
                        .map(|c| LinkSpec { rel: "concerns".into(), to: c.clone(), props: Default::default() })
                        .collect(),
                    ..Default::default()
                };
                report.written += 1;
                muts.entry(f.workspace.clone()).or_default().push(Mutation::Upsert(input));
            }
            // Suggestions nothing supports any more are withdrawn.
            for e in g.of_kind("learned_fact") {
                if workspace.is_some_and(|w| e.workspace != w) || current.contains(e.id.as_str()) {
                    continue;
                }
                if e.state.as_deref() == Some("suggested") && e.prop_str("learner").is_some() {
                    report.withdrawn += 1;
                    muts.entry(e.workspace.clone()).or_default().push(Mutation::Delete(EntityRef { id: e.id.clone() }));
                }
            }
            muts
        };
        for (ws, batch) in muts {
            let ctx = ActionContext::new(learner.clone(), ws).with_source("learner");
            for chunk in batch.chunks(500) {
                self.apply(&ctx, chunk.to_vec())?;
            }
        }
        Ok(report)
    }

    fn fact_view(&self, e: &Entity, g: &Graph) -> FactView {
        FactView {
            id: e.id.clone(),
            fact_type: e.prop_str("fact_type").unwrap_or_default().to_string(),
            statement: e.prop_str("statement").unwrap_or(&e.title).to_string(),
            state: e.state.clone().unwrap_or_else(|| "suggested".into()),
            confidence: e.props.get("confidence").and_then(Value::as_f64).unwrap_or(0.0),
            expression: e.prop_str("expression").unwrap_or_default().to_string(),
            name: e.prop_str("name").unwrap_or_default().to_string(),
            certified_by: e.prop_str("certified_by").map(str::to_string),
            concerns: g.neighbors(&e.id, "concerns", Dir::Out).into_iter().map(str::to_string).collect(),
            authority: self.authority_scores().get(&e.id).map(|a| round3(*a)),
        }
    }

    /// Learned facts the caller can see, certified first, then by confidence.
    pub fn learned_facts(
        &self,
        p: &Principal,
        fact_type: Option<&str>,
        state: Option<&str>,
        about: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FactView>> {
        let g = self.inner_graph_read();
        let about = about.map(|a| self.resolve_in(&g, a));
        let mut out: Vec<FactView> = g
            .of_kind("learned_fact")
            .filter(|e| policy::visible(p, e, &g))
            .filter(|e| fact_type.is_none_or(|t| e.prop_str("fact_type") == Some(t)))
            .filter(|e| state.is_none_or(|s| e.state.as_deref() == Some(s)))
            .filter(|e| about.as_ref().is_none_or(|a| g.neighbors(&e.id, "concerns", Dir::Out).contains(&a.as_str())))
            .map(|e| self.fact_view(e, &g))
            .collect();
        out.sort_by(|a, b| {
            (b.state == "certified")
                .cmp(&(a.state == "certified"))
                .then(b.confidence.total_cmp(&a.confidence))
                .then_with(|| a.id.cmp(&b.id))
        });
        out.truncate(limit.clamp(1, 1_000));
        Ok(out)
    }

    /// Which tables or views hold `term`: catalog search ranked by
    /// OntoRank, plus columns and metric definitions that point at a table.
    pub fn find_data(
        &self,
        p: &Principal,
        term: &str,
        workspace: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DataLocation>> {
        let kinds: Vec<String> = ["table", "view", "column", "learned_fact"].iter().map(|k| k.to_string()).collect();
        let hits = self.search(p, term, &kinds, workspace, 40)?;
        let scores = self.authority_scores();
        let words: Vec<String> = crate::text::tokenize(&term.replace('_', " "));
        // A column counts when its own name or comment matches, not just its table's name.
        let own_match = |e: &Entity| {
            let own = crate::text::tokenize(
                &format!("{} {}", name_of(e), e.prop_str("description").unwrap_or_default()).replace('_', " "),
            );
            words.iter().any(|w| own.contains(w))
        };
        let g = self.inner_graph_read();
        let mut best: BTreeMap<Id, DataLocation> = BTreeMap::new();
        let mut credit = |id: &str, score: f32, reason: String| {
            let Some(e) = g.get(id).filter(|e| policy::visible(p, e, &g)) else { return };
            if !matches!(e.kind.as_str(), "table" | "view") {
                return;
            }
            let loc = best.entry(id.to_string()).or_insert_with(|| DataLocation {
                id: id.to_string(),
                kind: e.kind.clone(),
                title: e
                    .prop_str("schema_name")
                    .map(|s| format!("{s}.{}", name_of(e)))
                    .unwrap_or_else(|| e.title.clone()),
                score: 0.0,
                authority: scores.get(id).map(|a| round3(*a)),
                reasons: Vec::new(),
            });
            loc.score += score;
            if loc.reasons.len() < 4 && !loc.reasons.contains(&reason) {
                loc.reasons.push(reason);
            }
        };
        for h in &hits {
            let e = &h.entity;
            match e.kind.as_str() {
                "table" | "view" => {
                    let why = match e.prop_str("description").filter(|d| !d.is_empty()) {
                        Some(d) => format!("described as \"{}\"", d.chars().take(80).collect::<String>()),
                        None => "name matches".to_string(),
                    };
                    credit(&e.id, h.score, why);
                }
                "column" if own_match(e) => {
                    if let Some(t) = g.neighbors(&e.id, "contains", Dir::In).first() {
                        let why = match e.prop_str("description").filter(|d| !d.is_empty()) {
                            Some(d) => format!("column {} (\"{}\")", e.title, d.chars().take(60).collect::<String>()),
                            None => format!("column {}", e.title),
                        };
                        credit(t, 0.6 * h.score, why);
                    }
                }
                "learned_fact"
                    if e.prop_str("fact_type") == Some("metric") && e.state.as_deref() != Some("rejected") =>
                {
                    let certified = e.state.as_deref() == Some("certified");
                    for t in g.neighbors(&e.id, "concerns", Dir::Out) {
                        let why = format!(
                            "{}metric {}",
                            if certified { "certified " } else { "" },
                            e.prop_str("statement").unwrap_or(&e.title)
                        );
                        credit(t, if certified { 1.2 } else { 0.5 } * h.score, why);
                    }
                }
                _ => {}
            }
        }
        let mut out: Vec<DataLocation> = best
            .into_values()
            .map(|mut l| {
                l.score = ((l.score * (1.0 + l.authority.unwrap_or(0.0) as f32)) * 1000.0).round() / 1000.0;
                l
            })
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        out.truncate(limit.clamp(1, 50));
        Ok(out)
    }

    /// How table `a` joins table `b`: certified or learned join paths first,
    /// else the shortest chain of foreign keys and learned joins (≤ 4 hops).
    pub fn join_path(&self, p: &Principal, a: &str, b: &str) -> Result<Option<JoinAnswer>> {
        let g = self.inner_graph_read();
        let (a, b) = (self.resolve_in(&g, a), self.resolve_in(&g, b));
        for id in [&a, &b] {
            match g.get(id) {
                Some(e) if policy::visible(p, e, &g) => {}
                _ => return Err(crate::Error::not_found(id)),
            }
        }
        // Edges: learned join facts (not rejected) and table-level foreign keys.
        let mut adj: HashMap<String, Vec<(String, String, Option<FactView>)>> = HashMap::new();
        for e in g.of_kind("learned_fact") {
            if e.prop_str("fact_type") != Some("join_path")
                || e.state.as_deref() == Some("rejected")
                || !policy::visible(p, e, &g)
            {
                continue;
            }
            let ends = g.neighbors(&e.id, "concerns", Dir::Out);
            if ends.len() != 2 {
                continue;
            }
            let view = self.fact_view(e, &g);
            let on = view.expression.clone();
            adj.entry(ends[0].to_string()).or_default().push((ends[1].to_string(), on.clone(), Some(view.clone())));
            adj.entry(ends[1].to_string()).or_default().push((ends[0].to_string(), on, Some(view)));
        }
        for t in g.of_kind("table").chain(g.of_kind("view")) {
            for (rel, to) in g.out_edges(&t.id) {
                if rel != "references" || !g.get(to).is_some_and(|x| matches!(x.kind.as_str(), "table" | "view")) {
                    continue;
                }
                let on = g
                    .get_link(&t.id, rel, to)
                    .and_then(|l| l.props.get("on"))
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" AND "))
                    .unwrap_or_else(|| "foreign key".into());
                adj.entry(t.id.clone()).or_default().push((to.clone(), on.clone(), None));
                adj.entry(to.clone()).or_default().push((t.id.clone(), on, None));
            }
        }
        // Prefer certified, then learned, then plain foreign keys on each hop.
        let rank = |f: &Option<FactView>| match f {
            Some(v) if v.state == "certified" => 0,
            Some(_) => 1,
            None => 2,
        };
        for list in adj.values_mut() {
            list.sort_by(|x, y| {
                rank(&x.2).cmp(&rank(&y.2)).then(
                    y.2.as_ref()
                        .map(|v| v.confidence)
                        .unwrap_or(0.0)
                        .total_cmp(&x.2.as_ref().map(|v| v.confidence).unwrap_or(0.0)),
                )
            });
        }
        let mut prev: HashMap<String, (String, String, Option<FactView>)> = HashMap::new();
        let mut queue = VecDeque::from([(a.clone(), 0usize)]);
        let mut seen = BTreeSet::from([a.clone()]);
        while let Some((cur, d)) = queue.pop_front() {
            if cur == b || d >= 4 {
                continue;
            }
            for (next, on, fact) in adj.get(&cur).cloned().unwrap_or_default() {
                if !g.get(&next).is_some_and(|e| policy::visible(p, e, &g)) || !seen.insert(next.clone()) {
                    continue;
                }
                prev.insert(next.clone(), (cur.clone(), on, fact));
                queue.push_back((next, d + 1));
            }
        }
        if !prev.contains_key(&b) {
            return Ok(None);
        }
        let mut hops = Vec::new();
        let mut facts = Vec::new();
        let mut cur = b.clone();
        while cur != a {
            let (from, on, fact) = prev.get(&cur).cloned().expect("on the path");
            hops.push((from.clone(), cur.clone(), on));
            facts.extend(fact);
            cur = from;
        }
        hops.reverse();
        facts.reverse();
        let via = if facts.iter().any(|f| f.state == "certified") {
            "certified join path"
        } else if !facts.is_empty() {
            "learned join path"
        } else {
            "foreign keys"
        };
        Ok(Some(JoinAnswer { from: a, to: b, hops, facts, via: via.into() }))
    }

    /// Metric definitions for `term`: certified first, then by OntoRank and confidence.
    pub fn trusted_metrics(&self, p: &Principal, term: &str, workspace: Option<&str>) -> Result<Vec<FactView>> {
        // Names are snake_case: "revenue_recognized" matches "revenue".
        let words: Vec<String> = crate::text::tokenize(&term.replace('_', " "));
        let scores = self.authority_scores();
        let g = self.inner_graph_read();
        let mut out: Vec<FactView> = g
            .of_kind("learned_fact")
            .filter(|e| e.prop_str("fact_type") == Some("metric") && e.state.as_deref() != Some("rejected"))
            .filter(|e| policy::visible(p, e, &g) && workspace.is_none_or(|w| e.workspace == w))
            .filter(|e| {
                let hay = crate::text::tokenize(
                    &format!(
                        "{} {} {}",
                        e.prop_str("name").unwrap_or_default(),
                        e.prop_str("statement").unwrap_or_default(),
                        e.title
                    )
                    .replace('_', " "),
                );
                words.is_empty() || words.iter().any(|w| hay.contains(w))
            })
            .map(|e| self.fact_view(e, &g))
            .collect();
        let key = |f: &FactView| {
            let concern_auth = f.concerns.iter().filter_map(|c| scores.get(c)).copied().fold(0.0f32, f32::max) as f64;
            (f.state == "certified", f.authority.unwrap_or(0.0) + concern_auth + f.confidence)
        };
        out.sort_by(|a, b| {
            let (ka, kb) = (key(a), key(b));
            kb.0.cmp(&ka.0).then(kb.1.total_cmp(&ka.1)).then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        self, CatalogSnapshot, ColumnInfo, ForeignKey, MapOptions, QueryStat, RelationInfo, SchemaInfo, SourceInfo,
    };
    use crate::schema::Schema;

    fn world() -> Ontology {
        let mut schema = Schema::enterprise();
        schema.extend_toml(crate::schema::builtin_package("data-catalog").unwrap()).unwrap();
        let o = Ontology::in_memory(schema);
        let col = |n: &str, comment: &str| ColumnInfo {
            name: n.into(),
            data_type: "bigint".into(),
            comment: comment.into(),
            ..Default::default()
        };
        let snap = CatalogSnapshot {
            source: SourceInfo { key: "wh".into(), engine: "postgres".into(), ..Default::default() },
            schemas: vec![SchemaInfo { name: "sales".into(), ..Default::default() }],
            relations: vec![
                RelationInfo { schema: "sales".into(), name: "customers".into(), columns: vec![col("id", ""), col("cust_name", "")], ..Default::default() },
                RelationInfo { schema: "sales".into(), name: "orders".into(), columns: vec![col("id", ""), col("customer_id", ""), col("amount", "Order total, also known as GMV")], ..Default::default() },
                RelationInfo { schema: "sales".into(), name: "order_items".into(), columns: vec![col("order_id", ""), col("qty", "")], ..Default::default() },
            ],
            foreign_keys: vec![ForeignKey {
                name: "orders_customer_fk".into(),
                from_schema: "sales".into(),
                from_table: "orders".into(),
                from_columns: vec!["customer_id".into()],
                to_schema: "sales".into(),
                to_table: "customers".into(),
                to_columns: vec!["id".into()],
            }],
            queries: vec![
                QueryStat { queryid: "1".into(), text: "select c.cust_name, sum(o.amount) as revenue from sales.orders o join sales.customers c on o.customer_id = c.id group by 1".into(), calls: 500, ..Default::default() },
                QueryStat { queryid: "2".into(), text: "select sum(i.qty) from sales.order_items i join sales.orders o on i.order_id = o.id".into(), calls: 20, ..Default::default() },
            ],
            ..Default::default()
        };
        let opts = MapOptions { workspace: "acme".into(), connector: "wh".into(), ..Default::default() };
        let plan = catalog::plan_changes(
            catalog::map_snapshot(&snap, &opts),
            &Default::default(),
            &crate::graph::Graph::default(),
            catalog::MANAGED_RELS,
        );
        let ctx = ActionContext::new(Principal::admin("crawler"), "acme");
        o.apply(&ctx, plan.mutations).unwrap();
        o
    }

    #[test]
    fn learns_joins_metrics_and_synonyms_and_respects_decisions() {
        let o = world();
        let r = o.learn(None).unwrap();
        assert!(r.join_paths >= 2 && r.metrics >= 1 && r.synonyms >= 2, "{r:?}");
        let admin = Principal::admin("x");
        let joins = o.learned_facts(&admin, Some("join_path"), None, Some("table:wh.sales.customers"), 10).unwrap();
        assert_eq!(joins[0].expression, "customers.id = orders.customer_id");
        assert!(joins[0].confidence > 0.9, "a foreign key plus a query: {:?}", joins[0]);
        let syn = o.learned_facts(&admin, Some("synonym"), None, None, 50).unwrap();
        assert!(syn.iter().any(|s| s.statement == "\"cust\" means customer"), "{syn:?}");
        assert!(syn.iter().any(|s| s.statement.starts_with("\"gmv\"")), "{syn:?}");
        let metric = o.trusted_metrics(&admin, "revenue", None).unwrap();
        assert_eq!(metric[0].expression, "sum(orders.amount)");

        // How two tables join, over two hops.
        let path = o.join_path(&admin, "table:wh.sales.order_items", "table:wh.sales.customers").unwrap().unwrap();
        assert_eq!(path.hops.len(), 2);
        assert_eq!(path.via, "learned join path");

        // Certify one, reject another: relearning leaves both as decided.
        let ctx = ActionContext::new(Principal::admin("ada"), "acme");
        let fact = metric[0].id.clone();
        let confirm = crate::action_types::ApplyOptions { confirm: true, ..Default::default() };
        o.apply_action(&ctx, "certify_fact", &json!({ "fact": fact }).as_object().unwrap().clone(), confirm).unwrap();
        let rejected = syn.iter().find(|s| s.statement == "\"cust\" means customer").unwrap().id.clone();
        o.apply_action(&ctx, "reject_fact", &json!({ "fact": rejected }).as_object().unwrap().clone(), confirm)
            .unwrap();
        let again = o.learn(None).unwrap();
        assert_eq!(again.written, 0, "{again:?}");
        assert_eq!(again.decided, 2);
        let m = o.trusted_metrics(&admin, "revenue", None).unwrap();
        assert_eq!(m[0].state, "certified");
        assert_eq!(m[0].certified_by.as_deref(), Some("ada"));
        assert_eq!(o.get(&admin, &rejected).unwrap().state.as_deref(), Some("rejected"));
        let found = o.find_data(&admin, "revenue", None, 3).unwrap();
        assert_eq!(found[0].id, "table:wh.sales.orders", "{found:?}");
        assert!(found[0].reasons.iter().any(|r| r.contains("certified metric")), "{found:?}");
    }
}
