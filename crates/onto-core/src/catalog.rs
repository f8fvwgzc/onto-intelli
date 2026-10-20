//! Data catalog: the metadata of a database as ontology data (the
//! `data-catalog` schema package), the change detection behind incremental
//! crawls, and a reader for the normalized SQL of query logs.
//!
//! No I/O happens here. A connector fetches a [`CatalogSnapshot`] (schemas,
//! tables, views, columns, comments, constraints, grants and usage
//! statistics, never row data); [`map_snapshot`] turns it into entities and
//! links with provenance; [`plan_changes`] compares that with what the last
//! crawl wrote and returns only the mutations that change something.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::graph::Graph;
use crate::model::{EntityInput, EntityRef, Id, LinkInput, Mutation, Provenance};
use crate::schema::Dir;

/// Relations a connector owns: links of these kinds from crawled entities
/// that a crawl no longer sees are removed. Curated links (`owns` set by
/// people, `concerns` from the learner) are left alone.
pub const MANAGED_RELS: &[&str] = &["contains", "references", "reads_from"];

/// Everything a connector read from one data source.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub source: SourceInfo,
    #[serde(default)]
    pub schemas: Vec<SchemaInfo>,
    #[serde(default)]
    pub relations: Vec<RelationInfo>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
    /// Views and the tables or views they read.
    #[serde(default)]
    pub view_deps: Vec<ViewDep>,
    /// Normalized statements from the query log, most called first.
    #[serde(default)]
    pub queries: Vec<QueryStat>,
    /// Why some metadata could not be read (no pg_stat_statements, ...).
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceInfo {
    /// Stable short name: ids are `table:{key}.{schema}.{name}`.
    pub key: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub server_version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SchemaInfo {
    pub name: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub owner: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelKind {
    #[default]
    Table,
    Partitioned,
    Foreign,
    MaterializedView,
    View,
}

impl RelKind {
    fn table_type(self) -> &'static str {
        match self {
            RelKind::Table => "table",
            RelKind::Partitioned => "partitioned",
            RelKind::Foreign => "foreign",
            RelKind::MaterializedView => "materialized_view",
            RelKind::View => "view",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RelationInfo {
    pub schema: String,
    pub name: String,
    #[serde(default)]
    pub kind: RelKind,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub owner: String,
    /// Planner estimate (`reltuples`), not a count.
    #[serde(default)]
    pub row_estimate: i64,
    /// A view's SQL.
    #[serde(default)]
    pub definition: Option<String>,
    #[serde(default)]
    pub columns: Vec<ColumnInfo>,
    /// Sequential plus index scans (`pg_stat_user_tables`).
    #[serde(default)]
    pub reads: i64,
    /// Rows inserted, updated and deleted.
    #[serde(default)]
    pub writes: i64,
    /// Last analyze or vacuum, unix ms.
    #[serde(default)]
    pub last_activity: Option<i64>,
    /// Roles granted SELECT (directly).
    #[serde(default)]
    pub readers: Vec<String>,
    /// PUBLIC may SELECT.
    #[serde(default)]
    pub public_read: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    #[serde(default)]
    pub data_type: String,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default)]
    pub position: i64,
    #[serde(default)]
    pub primary_key: bool,
    #[serde(default)]
    pub comment: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ForeignKey {
    pub name: String,
    pub from_schema: String,
    pub from_table: String,
    pub from_columns: Vec<String>,
    pub to_schema: String,
    pub to_table: String,
    pub to_columns: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ViewDep {
    pub view_schema: String,
    pub view: String,
    pub schema: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueryStat {
    pub queryid: String,
    pub text: String,
    #[serde(default)]
    pub calls: i64,
    #[serde(default)]
    pub mean_ms: f64,
    #[serde(default)]
    pub rows: i64,
}

/// How a snapshot becomes entities.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MapOptions {
    pub workspace: String,
    /// The connector's name (provenance).
    pub connector: String,
    pub crawled_at: i64,
    /// Crawl run id (provenance).
    #[serde(default)]
    pub run: String,
    /// Restrict tables that PUBLIC cannot read to the principals their
    /// readers map to (`grant_map`), as entity ACLs.
    #[serde(default)]
    pub map_grants: bool,
    /// Database role → onto principal or team (`team:analytics`). Unmapped
    /// roles appear in ACLs as `role:{name}`.
    #[serde(default)]
    pub grant_map: BTreeMap<String, String>,
    /// Database role → the team or person that owns what the role owns.
    #[serde(default)]
    pub owner_map: BTreeMap<String, String>,
}

/// One entity of the catalog and the links it should have.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mapped {
    pub input: EntityInput,
    pub links: Vec<LinkInput>,
}

impl Mapped {
    pub fn id(&self) -> &str {
        self.input.id.as_deref().unwrap_or_default()
    }
}

pub fn source_id(key: &str) -> Id {
    format!("data_source:{key}")
}

pub fn schema_id(key: &str, schema: &str) -> Id {
    format!("db_schema:{key}.{schema}")
}

pub fn relation_id(key: &str, schema: &str, name: &str, kind: RelKind) -> Id {
    let prefix = if kind == RelKind::View { "view" } else { "table" };
    format!("{prefix}:{key}.{schema}.{name}")
}

pub fn column_id(key: &str, schema: &str, table: &str, column: &str) -> Id {
    format!("column:{key}.{schema}.{table}.{column}")
}

pub fn query_id(key: &str, queryid: &str) -> Id {
    format!("query:{key}.{queryid}")
}

fn words(name: &str) -> String {
    name.replace(['_', '-', '.'], " ")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max).collect::<String>() + "…" }
}

/// Entities and links for a snapshot.
pub fn map_snapshot(snap: &CatalogSnapshot, opts: &MapOptions) -> Vec<Mapped> {
    let key = snap.source.key.as_str();
    let provenance = Provenance {
        source: format!("connector:{}", snap.source.engine),
        run: (!opts.run.is_empty()).then(|| opts.run.clone()),
        external_ref: Some(format!("{}://{}/{}", snap.source.engine, snap.source.host, snap.source.database)),
        at: opts.crawled_at,
        connector: Some(opts.connector.clone()),
        ..Default::default()
    };
    let entity = |kind: &str, id: Id, title: String| EntityInput {
        id: Some(id),
        kind: kind.to_string(),
        workspace: Some(opts.workspace.clone()),
        title: Some(title),
        provenance: Some(provenance.clone()),
        ..Default::default()
    };
    let mut out = Vec::new();
    let src = source_id(key);
    let mut source = entity(
        "data_source",
        src.clone(),
        if snap.source.title.is_empty() { key.to_string() } else { snap.source.title.clone() },
    );
    for (k, v) in [
        ("engine", &snap.source.engine),
        ("host", &snap.source.host),
        ("database", &snap.source.database),
        ("server_version", &snap.source.server_version),
    ] {
        source.props.insert(k.into(), json!(v));
    }
    source.aliases.push(snap.source.database.clone());
    let mut source_links = Vec::new();
    for s in &snap.schemas {
        source_links.push(LinkInput::new(&src, "contains", schema_id(key, &s.name)));
    }
    out.push(Mapped { input: source, links: source_links });

    // Per-relation ACLs (when grants are mapped), reused by columns and queries.
    let mut acl: HashMap<(String, String), Option<Vec<String>>> = HashMap::new();
    for r in &snap.relations {
        let restricted = opts.map_grants && !r.public_read;
        let list = restricted.then(|| {
            let mut a: Vec<String> = r
                .readers
                .iter()
                .map(|role| opts.grant_map.get(role).cloned().unwrap_or_else(|| format!("role:{role}")))
                .collect();
            a.sort();
            a.dedup();
            a
        });
        acl.insert((r.schema.clone(), r.name.clone()), list);
    }
    let restrict = |e: &mut EntityInput, list: &Option<Vec<String>>| {
        if let Some(list) = list {
            e.props.insert("visibility".into(), json!("restricted"));
            e.props.insert("acl".into(), json!(list));
        } else if opts.map_grants {
            // Grants changed back to public: lift an earlier restriction.
            e.props.insert("visibility".into(), json!("internal"));
            e.props.insert("acl".into(), json!([]));
        }
    };
    let owner_link = |role: &str, id: &str| {
        opts.owner_map.get(role).map(|owner| LinkInput::new(owner.clone(), "owns", id.to_string()))
    };

    let mut by_schema: BTreeMap<&str, Vec<&RelationInfo>> = BTreeMap::new();
    for r in &snap.relations {
        by_schema.entry(r.schema.as_str()).or_default().push(r);
    }
    for s in &snap.schemas {
        let id = schema_id(key, &s.name);
        let mut e = entity("db_schema", id.clone(), s.name.clone());
        e.props.insert("name".into(), json!(s.name));
        e.props.insert("description".into(), json!(s.comment));
        e.props.insert("owner_role".into(), json!(s.owner));
        e.aliases.push(format!("{key}.{}", s.name));
        let mut links: Vec<LinkInput> = by_schema
            .get(s.name.as_str())
            .map(|rs| {
                rs.iter()
                    .map(|r| LinkInput::new(&id, "contains", relation_id(key, &r.schema, &r.name, r.kind)))
                    .collect()
            })
            .unwrap_or_default();
        links.extend(owner_link(&s.owner, &id));
        out.push(Mapped { input: e, links });
    }

    let calls_by_table = query_calls_by_relation(snap);
    for r in &snap.relations {
        let id = relation_id(key, &r.schema, &r.name, r.kind);
        let is_view = r.kind == RelKind::View;
        let mut e = entity(if is_view { "view" } else { "table" }, id.clone(), r.name.clone());
        e.aliases.push(format!("{}.{}", r.schema, r.name));
        if words(&r.name) != r.name {
            e.aliases.push(words(&r.name));
        }
        e.props.insert("name".into(), json!(r.name));
        e.props.insert("schema_name".into(), json!(r.schema));
        e.props.insert("description".into(), json!(r.comment));
        e.props.insert("owner_role".into(), json!(r.owner));
        let documented = r.columns.iter().filter(|c| !c.comment.trim().is_empty()).count();
        let share = if r.columns.is_empty() { 0.0 } else { documented as f64 / r.columns.len() as f64 };
        e.props.insert("documented_columns".into(), json!((share * 100.0).round() / 100.0));
        e.props.insert("column_names".into(), json!(r.columns.iter().map(|c| words(&c.name)).collect::<Vec<_>>()));
        e.props.insert(
            "query_calls".into(),
            json!(calls_by_table.get(&(r.schema.clone(), r.name.clone())).copied().unwrap_or(0)),
        );
        if is_view {
            e.props.insert("definition".into(), json!(clip(r.definition.as_deref().unwrap_or_default(), 8000)));
        } else {
            e.props.insert("table_type".into(), json!(r.kind.table_type()));
            e.props.insert("row_estimate".into(), json!(r.row_estimate.max(0)));
            e.props.insert("reads".into(), json!(r.reads));
            e.props.insert("writes".into(), json!(r.writes));
            if let Some(ts) = r.last_activity {
                e.props.insert("last_activity".into(), json!(ts));
            }
        }
        let list = acl.get(&(r.schema.clone(), r.name.clone())).cloned().flatten();
        restrict(&mut e, &list);
        let mut links: Vec<LinkInput> = r
            .columns
            .iter()
            .map(|c| LinkInput::new(&id, "contains", column_id(key, &r.schema, &r.name, &c.name)))
            .collect();
        links.extend(owner_link(&r.owner, &id));
        out.push(Mapped { input: e, links });

        for c in &r.columns {
            let cid = column_id(key, &r.schema, &r.name, &c.name);
            let mut ce = entity("column", cid, format!("{}.{}", r.name, c.name));
            ce.aliases.push(format!("{}.{}.{}", r.schema, r.name, c.name));
            ce.aliases.push(words(&format!("{} {}", r.name, c.name)));
            ce.props.insert("name".into(), json!(c.name));
            ce.props.insert("data_type".into(), json!(c.data_type));
            ce.props.insert("nullable".into(), json!(c.nullable));
            ce.props.insert("position".into(), json!(c.position));
            ce.props.insert("primary_key".into(), json!(c.primary_key));
            ce.props.insert("description".into(), json!(c.comment));
            restrict(&mut ce, &list);
            out.push(Mapped { input: ce, links: Vec::new() });
        }
    }

    // Foreign keys: column → column, and table → table for the join graph.
    let rel_kind: HashMap<(&str, &str), RelKind> =
        snap.relations.iter().map(|r| ((r.schema.as_str(), r.name.as_str()), r.kind)).collect();
    let mut extra: HashMap<Id, Vec<LinkInput>> = HashMap::new();
    for fk in &snap.foreign_keys {
        let from_kind = rel_kind.get(&(fk.from_schema.as_str(), fk.from_table.as_str())).copied().unwrap_or_default();
        let to_kind = rel_kind.get(&(fk.to_schema.as_str(), fk.to_table.as_str())).copied().unwrap_or_default();
        let from_t = relation_id(key, &fk.from_schema, &fk.from_table, from_kind);
        let to_t = relation_id(key, &fk.to_schema, &fk.to_table, to_kind);
        let mut l = LinkInput::new(&from_t, "references", &to_t);
        l.props.insert("constraint".into(), json!(fk.name));
        l.props.insert(
            "on".into(),
            json!(
                fk.from_columns
                    .iter()
                    .zip(&fk.to_columns)
                    .map(|(a, b)| format!("{}.{a} = {}.{b}", fk.from_table, fk.to_table))
                    .collect::<Vec<_>>()
            ),
        );
        extra.entry(from_t.clone()).or_default().push(l);
        for (a, b) in fk.from_columns.iter().zip(&fk.to_columns) {
            let mut l = LinkInput::new(
                column_id(key, &fk.from_schema, &fk.from_table, a),
                "references",
                column_id(key, &fk.to_schema, &fk.to_table, b),
            );
            l.props.insert("constraint".into(), json!(fk.name));
            extra.entry(l.from.clone()).or_default().push(l);
        }
    }
    for d in &snap.view_deps {
        let view = relation_id(key, &d.view_schema, &d.view, RelKind::View);
        let kind = rel_kind.get(&(d.schema.as_str(), d.name.as_str())).copied().unwrap_or_default();
        extra.entry(view.clone()).or_default().push(LinkInput::new(
            &view,
            "reads_from",
            relation_id(key, &d.schema, &d.name, kind),
        ));
    }
    for m in &mut out {
        if let Some(mut more) = extra.remove(m.id()) {
            m.links.append(&mut more);
        }
    }

    // Queries: what reads what, and how often.
    let names = RelationNames::of(snap);
    for q in &snap.queries {
        let refs = SqlRefs::read(&q.text);
        let reads: BTreeSet<Id> =
            refs.tables.iter().filter_map(|t| names.resolve(t.schema.as_deref(), &t.name)).collect();
        if reads.is_empty() {
            continue; // catalog or utility statements
        }
        let id = query_id(key, &q.queryid);
        let mut e = entity("query", id.clone(), clip(&one_line(&q.text), 90));
        e.props.insert("text".into(), json!(clip(&q.text, 4000)));
        e.props.insert("queryid".into(), json!(q.queryid));
        e.props.insert("calls".into(), json!(q.calls));
        e.props.insert("mean_ms".into(), json!((q.mean_ms * 100.0).round() / 100.0));
        e.props.insert("rows".into(), json!(q.rows));
        // A query is visible to whoever may read every table it reads.
        let mut common: Option<Vec<String>> = None;
        let mut restricted = false;
        for t in &refs.tables {
            if let Some((schema, name)) = names.key_of(t.schema.as_deref(), &t.name)
                && let Some(Some(list)) = acl.get(&(schema, name))
            {
                restricted = true;
                common = Some(match common {
                    None => list.clone(),
                    Some(c) => c.into_iter().filter(|x| list.contains(x)).collect(),
                });
            }
        }
        restrict(&mut e, &if restricted { Some(common.unwrap_or_default()) } else { None });
        let links = reads.iter().map(|t| LinkInput::new(&id, "reads_from", t)).collect();
        out.push(Mapped { input: e, links });
    }
    out
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Calls of crawled queries per relation (schema, name).
fn query_calls_by_relation(snap: &CatalogSnapshot) -> HashMap<(String, String), i64> {
    let names = RelationNames::of(snap);
    let mut out: HashMap<(String, String), i64> = HashMap::new();
    for q in &snap.queries {
        let refs = SqlRefs::read(&q.text);
        let mut seen = HashSet::new();
        for t in &refs.tables {
            if let Some(k) = names.key_of(t.schema.as_deref(), &t.name)
                && seen.insert(k.clone())
            {
                *out.entry(k).or_default() += q.calls;
            }
        }
    }
    out
}

/// Resolves the table names a query uses to the snapshot's relations.
struct RelationNames {
    key: String,
    exact: HashMap<(String, String), RelKind>,
    by_name: HashMap<String, Vec<(String, RelKind)>>,
}

impl RelationNames {
    fn of(snap: &CatalogSnapshot) -> Self {
        let mut exact = HashMap::new();
        let mut by_name: HashMap<String, Vec<(String, RelKind)>> = HashMap::new();
        for r in &snap.relations {
            exact.insert((r.schema.to_lowercase(), r.name.to_lowercase()), r.kind);
            by_name.entry(r.name.to_lowercase()).or_default().push((r.schema.clone(), r.kind));
        }
        Self { key: snap.source.key.clone(), exact, by_name }
    }

    /// (schema, name) as the snapshot spells them. Unqualified names prefer
    /// `public`, then a name that exists in only one schema.
    fn key_of(&self, schema: Option<&str>, name: &str) -> Option<(String, String)> {
        let lname = name.to_lowercase();
        match schema {
            Some(s) => {
                self.exact.contains_key(&(s.to_lowercase(), lname.clone())).then(|| (s.to_string(), name.to_string()))
            }
            None => {
                let candidates = self.by_name.get(&lname)?;
                let pick = candidates
                    .iter()
                    .find(|(s, _)| s == "public")
                    .or_else(|| (candidates.len() == 1).then(|| &candidates[0]))?;
                Some((pick.0.clone(), name.to_string()))
            }
        }
    }

    fn resolve(&self, schema: Option<&str>, name: &str) -> Option<Id> {
        let (s, n) = self.key_of(schema, name)?;
        let kind = *self.exact.get(&(s.to_lowercase(), n.to_lowercase()))?;
        Some(relation_id(&self.key, &s, &n, kind))
    }
}

// ── incremental crawls ───────────────────────────────────────────────────

/// What the previous crawl of a connector wrote: entity id → fingerprint.
pub type Fingerprints = BTreeMap<Id, u64>;

/// The mutations a crawl needs, and the fingerprints to keep for the next.
#[derive(Clone, Debug, Default)]
pub struct CrawlPlan {
    pub mutations: Vec<Mutation>,
    pub fingerprints: Fingerprints,
    pub upserted: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub links_removed: usize,
    /// Links whose other end does not exist.
    pub links_skipped: usize,
}

fn fnv(bytes: &[u8], mut h: u64) -> u64 {
    for b in bytes {
        h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Usage statistics change on every crawl; they count as changed only when
/// they move to another power of two, so busy tables are not rewritten
/// (and versioned) each time.
const COARSE: &[&str] =
    &["reads", "writes", "query_calls", "calls", "rows", "row_estimate", "mean_ms", "last_activity"];

fn coarse(v: &Value) -> Value {
    match v.as_f64() {
        Some(x) if x > 0.0 => json!((x + 1.0).log2().floor() as i64),
        _ => v.clone(),
    }
}

/// Fingerprint of an entity and its managed links (provenance excluded).
pub fn fingerprint(m: &Mapped) -> u64 {
    let mut props = Map::new();
    for (k, v) in &m.input.props {
        let v = if COARSE.contains(&k.as_str()) {
            if k == "last_activity" { json!(v.as_i64().unwrap_or(0) / 86_400_000) } else { coarse(v) }
        } else {
            v.clone()
        };
        props.insert(k.clone(), v);
    }
    let mut links: Vec<String> =
        m.links.iter().map(|l| format!("{}|{}|{}|{}", l.from, l.rel, l.to, Value::Object(l.props.clone()))).collect();
    links.sort();
    let doc = json!({
        "kind": m.input.kind, "title": m.input.title, "ws": m.input.workspace,
        "aliases": m.input.aliases, "props": props, "links": links,
    });
    fnv(doc.to_string().as_bytes(), 0xcbf2_9ce4_8422_2325)
}

/// Compare a crawl with the previous one and the graph: upsert what
/// changed, delete what vanished (only entities this connector wrote), and
/// remove managed links the source no longer has.
pub fn plan_changes(mapped: Vec<Mapped>, previous: &Fingerprints, graph: &Graph) -> CrawlPlan {
    let mut plan = CrawlPlan::default();
    let seen: HashSet<String> = mapped.iter().map(|m| m.id().to_string()).collect();
    let mut links_after: Vec<LinkInput> = Vec::new();
    for m in mapped {
        let fp = fingerprint(&m);
        let id = m.id().to_string();
        plan.fingerprints.insert(id.clone(), fp);
        if previous.get(&id) == Some(&fp) && graph.contains(&id) {
            plan.unchanged += 1;
            continue;
        }
        // Managed links the source dropped.
        let want: HashSet<(String, String)> =
            m.links.iter().filter(|l| l.from == id).map(|l| (l.rel.clone(), l.to.clone())).collect();
        for rel in MANAGED_RELS {
            for to in graph.neighbors(&id, rel, Dir::Out) {
                if !want.contains(&(rel.to_string(), to.to_string())) && previous.contains_key(to) {
                    plan.mutations.push(Mutation::Unlink(LinkInput::new(&id, *rel, to)));
                    plan.links_removed += 1;
                }
            }
        }
        plan.upserted += 1;
        plan.mutations.push(Mutation::Upsert(m.input));
        links_after.extend(m.links);
    }
    // Links go after every upsert, so both ends exist. Links to entities
    // that exist nowhere (an owner_map naming a missing team) are skipped.
    for l in links_after {
        let known = |id: &str| seen.contains(id) || graph.contains(id);
        if known(&l.from) && known(&l.to) {
            plan.mutations.push(Mutation::Link(l));
        } else {
            plan.links_skipped += 1;
        }
    }
    for id in previous.keys().filter(|id| !seen.contains(*id)) {
        if graph.contains(id) {
            plan.mutations.push(Mutation::Delete(EntityRef { id: id.clone() }));
            plan.deleted += 1;
        }
    }
    plan
}

// ── reading SQL ──────────────────────────────────────────────────────────

/// A table a statement reads (`FROM`/`JOIN`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
}

/// `a.x = b.y` between two tables of the statement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinCond {
    /// Indexes into [`SqlRefs::tables`].
    pub left: usize,
    pub left_column: String,
    pub right: usize,
    pub right_column: String,
}

/// `SUM(o.amount) AS revenue`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aggregate {
    pub func: String,
    /// Index into [`SqlRefs::tables`] of the aggregated column's table.
    pub table: Option<usize>,
    pub column: Option<String>,
    pub alias: Option<String>,
    /// The call as written, normalized (`sum(orders.amount)`).
    pub expr: String,
}

/// What a statement reads, how it joins, and what it aggregates. A
/// tolerant reader for normalized query-log SQL, not a full parser:
/// subqueries and CTEs contribute their tables too.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SqlRefs {
    pub tables: Vec<TableRef>,
    pub joins: Vec<JoinCond>,
    pub aggregates: Vec<Aggregate>,
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Quoted(String),
    Sym(char),
    Op(String),
    Other,
}

fn lex(sql: &str) -> Vec<Tok> {
    let c: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '-' && c.get(i + 1) == Some(&'-') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < c.len() && !(c[i] == '*' && c[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if ch == '\'' {
            i += 1;
            while i < c.len() {
                if c[i] == '\'' {
                    if c.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            out.push(Tok::Other);
        } else if ch == '"' {
            let mut s = String::new();
            i += 1;
            while i < c.len() {
                if c[i] == '"' {
                    if c.get(i + 1) == Some(&'"') {
                        s.push('"');
                        i += 2;
                        continue;
                    }
                    break;
                }
                s.push(c[i]);
                i += 1;
            }
            i += 1;
            out.push(Tok::Quoted(s));
        } else if ch.is_alphabetic() || ch == '_' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$') {
                i += 1;
            }
            out.push(Tok::Ident(c[start..i].iter().collect::<String>().to_lowercase()));
        } else if ch.is_ascii_digit() || ch == '$' {
            i += 1;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '.') {
                i += 1;
            }
            out.push(Tok::Other);
        } else if matches!(ch, '(' | ')' | ',' | '.' | ';' | '*') {
            out.push(Tok::Sym(ch));
            i += 1;
        } else {
            let start = i;
            while i < c.len() && "=<>!|&+-/%~^:".contains(c[i]) {
                i += 1;
            }
            if i == start {
                i += 1;
                out.push(Tok::Other);
            } else {
                out.push(Tok::Op(c[start..i].iter().collect()));
            }
        }
    }
    out
}

const STOP: &[&str] = &[
    "where",
    "group",
    "order",
    "limit",
    "offset",
    "having",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "outer",
    "on",
    "using",
    "union",
    "intersect",
    "except",
    "lateral",
    "natural",
    "window",
    "returning",
    "set",
    "values",
    "fetch",
    "for",
    "select",
    "as",
    "and",
    "or",
    "not",
    "when",
    "then",
    "else",
    "end",
    "with",
];

const AGGREGATES: &[&str] = &["sum", "count", "avg", "min", "max"];

fn name_of(t: &Tok) -> Option<&str> {
    match t {
        Tok::Ident(s) | Tok::Quoted(s) => Some(s),
        _ => None,
    }
}

impl SqlRefs {
    pub fn read(sql: &str) -> Self {
        let toks = lex(sql);
        let mut refs = SqlRefs::default();
        let ident = |i: usize| toks.get(i).and_then(name_of);
        let is_kw = |i: usize, kw: &str| matches!(toks.get(i), Some(Tok::Ident(s)) if s == kw);
        // Tables after FROM / JOIN, and comma lists after FROM.
        let mut i = 0;
        while i < toks.len() {
            let starts = is_kw(i, "from") || is_kw(i, "join") || is_kw(i, "update") || is_kw(i, "into");
            if !starts {
                i += 1;
                continue;
            }
            let from_list = is_kw(i, "from");
            i += 1;
            loop {
                if matches!(toks.get(i), Some(Tok::Sym('('))) || is_kw(i, "lateral") {
                    break; // a subquery: its own FROM is read on its own
                }
                let Some(first) = ident(i) else { break };
                if STOP.contains(&first) && matches!(toks.get(i), Some(Tok::Ident(_))) {
                    break;
                }
                let (schema, name, mut j) = if matches!(toks.get(i + 1), Some(Tok::Sym('.'))) && ident(i + 2).is_some()
                {
                    (Some(first.to_string()), ident(i + 2).unwrap_or_default().to_string(), i + 3)
                } else {
                    (None, first.to_string(), i + 1)
                };
                if is_kw(j, "as") {
                    j += 1;
                }
                let alias = match toks.get(j) {
                    Some(Tok::Ident(a)) if !STOP.contains(&a.as_str()) => {
                        j += 1;
                        Some(a.clone())
                    }
                    Some(Tok::Quoted(a)) => {
                        j += 1;
                        Some(a.clone())
                    }
                    _ => None,
                };
                if !name.starts_with("pg_")
                    && !(schema.as_deref() == Some("pg_catalog") || schema.as_deref() == Some("information_schema"))
                {
                    refs.tables.push(TableRef { schema, name, alias });
                }
                i = j;
                if from_list && matches!(toks.get(i), Some(Tok::Sym(','))) {
                    i += 1;
                    continue;
                }
                break;
            }
        }
        refs.tables.dedup();
        // Column references: alias.column → table index.
        let table_of = |q: &str| -> Option<usize> {
            refs.tables
                .iter()
                .position(|t| t.alias.as_deref() == Some(q))
                .or_else(|| refs.tables.iter().position(|t| t.name == q))
        };
        let colref = |i: usize| -> Option<(usize, String, usize)> {
            let q = ident(i)?;
            if !matches!(toks.get(i + 1), Some(Tok::Sym('.'))) {
                return None;
            }
            let col = ident(i + 2)?;
            Some((table_of(q)?, col.to_string(), i + 3))
        };
        let mut joins = Vec::new();
        let mut aggregates = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            if let Some((lt, lc, next)) = colref(i)
                && matches!(toks.get(next), Some(Tok::Op(op)) if op == "=")
                && let Some((rt, rc, after)) = colref(next + 1)
            {
                if lt != rt {
                    joins.push(JoinCond { left: lt, left_column: lc, right: rt, right_column: rc });
                }
                i = after;
                continue;
            }
            if let Some(Tok::Ident(f)) = toks.get(i)
                && AGGREGATES.contains(&f.as_str())
                && matches!(toks.get(i + 1), Some(Tok::Sym('(')))
            {
                // Up to the matching parenthesis.
                let mut depth = 0;
                let mut j = i + 1;
                let mut inner = Vec::new();
                while j < toks.len() {
                    match &toks[j] {
                        Tok::Sym('(') => depth += 1,
                        Tok::Sym(')') => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    if depth > 0 && j > i + 1 {
                        inner.push(j);
                    }
                    j += 1;
                }
                let (mut table, mut column) = (None, None);
                for &k in &inner {
                    if let Some((t, c, _)) = colref(k) {
                        table = Some(t);
                        column = Some(c);
                        break;
                    }
                }
                if column.is_none() && refs.tables.len() == 1 {
                    // Unqualified column of the only table.
                    if let Some(c) = inner.iter().find_map(|&k| match &toks[k] {
                        Tok::Ident(c) if !STOP.contains(&c.as_str()) && c != "distinct" => Some(c.clone()),
                        _ => None,
                    }) {
                        table = Some(0);
                        column = Some(c);
                    }
                }
                let mut k = j + 1;
                if is_kw(k, "as") {
                    k += 1;
                }
                let alias = match toks.get(k) {
                    Some(Tok::Ident(a)) if !STOP.contains(&a.as_str()) && a != "from" => Some(a.clone()),
                    Some(Tok::Quoted(a)) => Some(a.clone()),
                    _ => None,
                };
                let target = match (table, &column) {
                    (Some(t), Some(c)) => format!("{}.{c}", refs.tables[t].name),
                    (_, Some(c)) => c.clone(),
                    _ if inner.iter().any(|&k| toks[k] == Tok::Sym('*')) => "*".into(),
                    _ => "…".into(),
                };
                aggregates.push(Aggregate { func: f.clone(), table, column, alias, expr: format!("{f}({target})") });
                i = j + 1;
                continue;
            }
            i += 1;
        }
        joins.dedup();
        refs.joins = joins;
        refs.aggregates = aggregates;
        refs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tables_joins_and_aggregates() {
        let r = SqlRefs::read(
            "SELECT c.region, sum(o.amount) AS revenue FROM sales.orders o JOIN sales.customers AS c \
             ON o.customer_id = c.id WHERE o.status = $1 GROUP BY c.region",
        );
        assert_eq!(r.tables.len(), 2);
        assert_eq!(
            r.tables[0],
            TableRef { schema: Some("sales".into()), name: "orders".into(), alias: Some("o".into()) }
        );
        assert_eq!(r.tables[1].alias.as_deref(), Some("c"));
        assert_eq!(
            r.joins,
            vec![JoinCond { left: 0, left_column: "customer_id".into(), right: 1, right_column: "id".into() }]
        );
        assert_eq!(r.aggregates[0].expr, "sum(orders.amount)");
        assert_eq!(r.aggregates[0].alias.as_deref(), Some("revenue"));
    }

    #[test]
    fn comma_joins_quotes_and_catalog_queries() {
        let r = SqlRefs::read(
            "select count(*) from public.\"Order Items\" i, products p where i.product_id = p.id and i.note <> 'x = y.z'",
        );
        assert_eq!(r.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["Order Items", "products"]);
        assert_eq!(r.joins.len(), 1);
        assert_eq!(r.aggregates[0].expr, "count(*)");
        assert!(SqlRefs::read("SELECT * FROM pg_catalog.pg_class").tables.is_empty());
        let sub = SqlRefs::read("SELECT * FROM (SELECT id FROM accounts) a JOIN users u ON u.account_id = a.id");
        assert!(sub.tables.iter().any(|t| t.name == "accounts") && sub.tables.iter().any(|t| t.name == "users"));
    }

    fn snapshot() -> CatalogSnapshot {
        let col = |n: &str, pk: bool, comment: &str| ColumnInfo {
            name: n.into(),
            data_type: "bigint".into(),
            primary_key: pk,
            comment: comment.into(),
            ..Default::default()
        };
        CatalogSnapshot {
            source: SourceInfo {
                key: "wh".into(),
                engine: "postgres".into(),
                database: "shop".into(),
                ..Default::default()
            },
            schemas: vec![SchemaInfo {
                name: "sales".into(),
                comment: "Orders and customers".into(),
                owner: "sales_owner".into(),
            }],
            relations: vec![
                RelationInfo {
                    schema: "sales".into(),
                    name: "customers".into(),
                    comment: "One row per customer".into(),
                    columns: vec![col("id", true, "Customer id"), col("region", false, "")],
                    public_read: true,
                    ..Default::default()
                },
                RelationInfo {
                    schema: "sales".into(),
                    name: "orders".into(),
                    owner: "sales_owner".into(),
                    columns: vec![
                        col("id", true, ""),
                        col("customer_id", false, ""),
                        col("amount", false, "Order total"),
                    ],
                    readers: vec!["analyst".into()],
                    ..Default::default()
                },
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
            queries: vec![QueryStat {
                queryid: "42".into(),
                text: "select sum(amount) from sales.orders where id = $1".into(),
                calls: 900,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn snapshots_map_to_entities_links_and_acls() {
        let opts = MapOptions {
            workspace: "acme".into(),
            connector: "wh".into(),
            map_grants: true,
            grant_map: BTreeMap::from([("analyst".to_string(), "team:analytics".to_string())]),
            owner_map: BTreeMap::from([("sales_owner".to_string(), "team:sales".to_string())]),
            ..Default::default()
        };
        let m = map_snapshot(&snapshot(), &opts);
        let get = |id: &str| m.iter().find(|x| x.id() == id).unwrap_or_else(|| panic!("{id} missing"));
        let orders = get("table:wh.sales.orders");
        assert_eq!(orders.input.props["visibility"], "restricted");
        assert_eq!(orders.input.props["acl"], json!(["team:analytics"]));
        assert_eq!(orders.input.props["query_calls"], 900);
        assert!(orders.links.iter().any(|l| l.rel == "references" && l.to == "table:wh.sales.customers"));
        assert!(orders.links.iter().any(|l| l.from == "team:sales" && l.rel == "owns"));
        assert_eq!(get("column:wh.sales.orders.amount").input.props["acl"], json!(["team:analytics"]));
        assert!(get("column:wh.sales.orders.customer_id").links.iter().any(|l| l.to == "column:wh.sales.customers.id"));
        assert_eq!(get("table:wh.sales.customers").input.props["visibility"], "internal");
        let q = get("query:wh.42");
        assert_eq!(q.links, vec![LinkInput::new("query:wh.42", "reads_from", "table:wh.sales.orders")]);
        assert_eq!(q.input.props["acl"], json!(["team:analytics"]), "a query is as restricted as what it reads");
        assert_eq!(
            get("column:wh.sales.customers.id").input.provenance.as_ref().unwrap().connector.as_deref(),
            Some("wh")
        );
    }

    #[test]
    fn crawls_write_only_what_changed() {
        let opts = MapOptions { workspace: "acme".into(), connector: "wh".into(), ..Default::default() };
        let mut g = Graph::default();
        let first = plan_changes(map_snapshot(&snapshot(), &opts), &Fingerprints::new(), &g);
        assert_eq!(first.unchanged, 0);
        assert!(first.upserted >= 8);
        for id in first.fingerprints.keys() {
            let (kind, _) = id.split_once(':').unwrap();
            g.put_entity(serde_json::from_value(json!({ "id": id, "kind": kind, "workspace": "acme" })).unwrap());
        }
        // Busier, but within the same power of two: nothing to write.
        let mut snap = snapshot();
        snap.queries[0].calls = 1000;
        let again = plan_changes(map_snapshot(&snap, &opts), &first.fingerprints, &g);
        assert_eq!(again.upserted, 0, "{:?}", again.mutations);
        // A comment changes and a column is dropped.
        snap.relations[0].comment = "Customers, deduplicated".into();
        snap.relations[0].columns.pop();
        let third = plan_changes(map_snapshot(&snap, &opts), &first.fingerprints, &g);
        assert_eq!(third.upserted, 1);
        assert_eq!(third.deleted, 1, "the dropped column");
    }
}
