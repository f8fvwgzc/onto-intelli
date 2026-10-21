//! Connectors: crawl data sources into the catalog in the background.
//!
//! A connector reads a source's **metadata**: schemas, tables, views,
//! columns, types, constraints, comments, grants and usage statistics. It
//! reads **rows** only of tables that back an object type whose schema
//! says `sync = true` (and join tables of backed link types), and of those
//! only the key and the mapped columns. A crawl runs off the request path:
//!
//! 1. the connector fetches a [`CatalogSnapshot`], then the rows of synced
//!    tables a page at a time (async I/O against the source; no lock on the
//!    ontology is held);
//! 2. [`catalog::map_snapshot`] turns the snapshot into entities and links
//!    with provenance (`connector:postgres`, the connector name,
//!    `crawled_at`);
//! 3. [`catalog::plan_changes`] keeps only what changed since the last crawl
//!    (fingerprints stored per connector), so an idle database costs no
//!    writes;
//! 4. the changes are applied through the audited write path in small
//!    batches, so queries keep being served between them;
//! 5. synced rows become objects ([`objects::map_rows`]) and links, planned
//!    the same way against fingerprints kept per object type
//!    (`{connector}/objects/{type}`) and link type (`{connector}/links/{link}`);
//! 6. the learner and OntoRank run over the result.
//!
//! Specs and crawl state live in the store (KV), so every replica sees the
//! same connectors; a lease in the commit log lets only one process crawl a
//! connector at a time.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use onto_core::authority::Weights;
use onto_core::catalog::{self, CatalogSnapshot, Fingerprints, MapOptions};
use onto_core::graph::LinkKey;
use onto_core::learn::LearnReport;
use onto_core::objects::{self, Row, RowOptions};
use onto_core::schema::{Backing, DEFAULT_MAX_ROWS, LinkBacking};
use onto_core::time::{new_id, now_ms};
use onto_core::{ActionContext, Error, EventInput, Mutation, Ontology, Principal, Result, Role, Schema};

use crate::intelli::Intelli;

/// KV namespaces: connector specs and their crawl state.
pub const NS_SPEC: &str = "connector";
pub const NS_STATE: &str = "connector_state";

/// Rows read per page when syncing objects.
pub const ROW_PAGE: usize = 1000;

/// Mutations applied per batch (the graph lock is held per batch).
const BATCH: usize = 500;

fn default_every() -> u64 {
    900
}

fn default_max_queries() -> usize {
    500
}

fn default_exclude() -> Vec<String> {
    vec!["onto_*".into()]
}

/// A data source to crawl.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectorSpec {
    /// `[a-z0-9_-]`, at most 40: also the source key in entity ids.
    pub name: String,
    /// `postgres`.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Connection URL, or `env:VAR` to read it from the environment when
    /// crawling (keeps credentials out of the store; preferred).
    pub url: String,
    /// Workspace the crawled assets belong to (default: the engine's).
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub title: String,
    /// Schemas to crawl (empty: every schema the role can use). `*` matches any run of characters.
    #[serde(default)]
    pub schemas: Vec<String>,
    /// Schemas to skip. Default: onto's own tenant schemas (`onto_*`).
    #[serde(default = "default_exclude")]
    pub exclude_schemas: Vec<String>,
    /// Seconds between background crawls (0: only on demand).
    #[serde(default = "default_every")]
    pub every_secs: u64,
    /// Turn table grants into entity ACLs (see `grant_map`).
    #[serde(default)]
    pub map_grants: bool,
    /// Database role → onto principal or team, for ACLs.
    #[serde(default)]
    pub grant_map: BTreeMap<String, String>,
    /// Database role → team or person that owns what the role owns.
    #[serde(default)]
    pub owner_map: BTreeMap<String, String>,
    /// Most-called statements read from pg_stat_statements.
    #[serde(default = "default_max_queries")]
    pub max_queries: usize,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub created_at: i64,
}

fn default_kind() -> String {
    "postgres".into()
}

/// `*` wildcards; everything else literal.
pub fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(pos) => rest = &rest[pos + part.len()..],
                None => return false,
            }
        }
    }
    true
}

impl ConnectorSpec {
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty()
            || self.name.len() > 40
            || !self.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(Error::invalid(format!(
                "connector name '{}' must be 1-40 of a-z, 0-9, '_' and '-' (it becomes part of entity ids)",
                self.name
            )));
        }
        if self.kind != "postgres" {
            return Err(Error::invalid(format!("unknown connector kind '{}' (postgres)", self.kind)));
        }
        if self.url.trim().is_empty() {
            return Err(Error::invalid("connector url is required (postgres://… or env:VAR)"));
        }
        Ok(())
    }

    /// The URL to connect with (`env:VAR` read now).
    pub fn resolve_url(&self) -> Result<String> {
        match self.url.strip_prefix("env:") {
            Some(var) => std::env::var(var).ok().filter(|v| !v.trim().is_empty()).ok_or_else(|| {
                Error::Config(format!("connector {}: environment variable {var} is not set", self.name))
            }),
            None => Ok(self.url.clone()),
        }
    }

    /// The spec as the API shows it: passwords masked.
    pub fn redacted(&self) -> Self {
        let mut s = self.clone();
        s.url = redact_url(&self.url);
        s
    }

    pub fn wants_schema(&self, schema: &str) -> bool {
        (self.schemas.is_empty() || self.schemas.iter().any(|p| glob(p, schema)))
            && !self.exclude_schemas.iter().any(|p| glob(p, schema))
    }
}

/// `postgres://user:secret@host/db` → `postgres://user:***@host/db`.
pub fn redact_url(url: &str) -> String {
    if url.starts_with("env:") {
        return url.to_string();
    }
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            let creds = &url[scheme + 3..at];
            match creds.split_once(':') {
                Some((user, _)) => format!("{}{user}:***{}", &url[..scheme + 3], &url[at..]),
                None => url.to_string(),
            }
        }
        _ => url.to_string(),
    }
}

/// What a crawl found and did.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CrawlReport {
    pub connector: String,
    pub source: String,
    pub started_at: i64,
    pub ms: u64,
    pub schemas: usize,
    pub tables: usize,
    pub views: usize,
    pub columns: usize,
    pub foreign_keys: usize,
    pub queries: usize,
    /// Entities written because they are new or changed.
    pub upserted: usize,
    pub unchanged: usize,
    pub deleted: usize,
    pub links_removed: usize,
    pub learned: LearnReport,
    pub ranked: usize,
    /// Object type → what syncing its rows did.
    #[serde(default)]
    pub objects: BTreeMap<String, ObjectSync>,
    /// Link type backed by a join table → what syncing its rows did.
    #[serde(default)]
    pub links: BTreeMap<String, LinkSync>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Rows of one object type's table, as a crawl synced them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ObjectSync {
    /// Rows read.
    pub rows: usize,
    pub upserted: usize,
    pub unchanged: usize,
    pub deleted: usize,
    /// Rows the schema rejects; their objects keep what they had.
    #[serde(default)]
    pub invalid: usize,
    /// The table has more than `max_rows` rows; the rest are not synced.
    #[serde(default)]
    pub truncated: bool,
}

/// Rows of one join table, as a crawl synced them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkSync {
    pub rows: usize,
    pub linked: usize,
    pub unlinked: usize,
    /// Rows naming an object that does not exist.
    #[serde(default)]
    pub skipped: usize,
}

/// What [`Connector::rows`] reads: one page of a table, in key order.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RowRequest {
    /// `schema.table`.
    pub table: String,
    /// Key columns: rows come in this order, and rows with a null key are
    /// left out.
    pub key: Vec<String>,
    /// Further columns to read. No other column is read.
    #[serde(default)]
    pub columns: Vec<String>,
    /// Start after this key (the previous page's `last_key`).
    #[serde(default)]
    pub after: Option<Vec<Value>>,
    pub limit: usize,
}

/// One page of rows.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RowPage {
    pub rows: Vec<Row>,
    /// Key values of the last row, to read the next page from.
    pub last_key: Option<Vec<Value>>,
}

/// The rows of one table, read for a sync.
#[derive(Clone, Debug, Default)]
pub struct TableRows {
    pub rows: Vec<Row>,
    /// More rows exist than were read.
    pub truncated: bool,
    pub notes: Vec<String>,
}

/// Rows a crawl read for the object types and join tables it syncs, or why
/// they could not be read.
#[derive(Clone, Debug, Default)]
pub struct SyncedRows {
    pub objects: BTreeMap<String, std::result::Result<TableRows, String>>,
    pub links: BTreeMap<String, std::result::Result<TableRows, String>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConnectorState {
    pub last_crawl_at: i64,
    pub last_success_at: i64,
    #[serde(default)]
    pub last_error: Option<String>,
    pub crawls: u64,
    #[serde(default)]
    pub fingerprints: Fingerprints,
    #[serde(default)]
    pub last: Option<CrawlReport>,
}

/// Reads one kind of data source.
#[async_trait]
pub trait Connector: Send + Sync {
    /// The source's metadata. Never reads row data.
    async fn snapshot(&self, spec: &ConnectorSpec) -> Result<CatalogSnapshot>;

    /// One page of a table's rows: the key and the requested columns only,
    /// in key order, starting after `req.after`. Read-only.
    async fn rows(&self, spec: &ConnectorSpec, req: &RowRequest) -> Result<RowPage>;

    /// Release connections held for [`rows`](Self::rows).
    async fn close(&self) {}
}

pub fn connector_for(kind: &str) -> Result<Box<dyn Connector>> {
    match kind {
        #[cfg(feature = "postgres")]
        "postgres" => Ok(Box::new(postgres::PgConnector::default())),
        other => Err(Error::Config(format!("no connector for '{other}' in this build"))),
    }
}

fn connector_principal(name: &str) -> Principal {
    Principal::new(format!("connector:{name}"), vec![Role::System], vec!["*".into()])
}

fn require_admin(p: &Principal, what: &str) -> Result<()> {
    if p.is_privileged() {
        Ok(())
    } else {
        Err(Error::Denied(format!("{} may not {what}: connectors need an admin", p.id)))
    }
}

fn load_state(onto: &Ontology, name: &str) -> Result<ConnectorState> {
    match onto.store().kv_get(NS_STATE, name)? {
        Some(v) => Ok(serde_json::from_value(v)?),
        None => Ok(ConnectorState::default()),
    }
}

fn save_state(onto: &Ontology, name: &str, state: &ConnectorState) -> Result<()> {
    onto.store().kv_put(NS_STATE, name, &serde_json::to_value(state)?, None)
}

/// Object types whose rows `connector` syncs.
pub fn synced_types<'a>(schema: &'a Schema, connector: &str) -> Vec<(&'a str, &'a Backing)> {
    schema
        .object_types
        .values()
        .filter_map(|t| t.backing.as_ref().filter(|b| b.sync && b.source == connector).map(|b| (t.name.as_str(), b)))
        .collect()
}

/// Link types backed by a join table of `connector` whose two ends are
/// synced object types.
pub fn synced_join_links<'a>(schema: &'a Schema, connector: &str) -> Vec<(&'a str, &'a LinkBacking)> {
    let synced = |kind: Option<&String>| {
        kind.and_then(|k| schema.object_types.get(k)).and_then(|t| t.backing.as_ref()).is_some_and(|b| b.sync)
    };
    schema
        .link_types
        .values()
        .filter(|lt| synced(lt.from.first()) && synced(lt.to.first()))
        .filter_map(|lt| lt.backing.as_ref().filter(|b| b.source == connector).map(|b| (lt.name.as_str(), b)))
        .collect()
}

/// Read every page of one table (up to `max` rows) for a sync. Only the
/// key and `columns` are read; columns the crawl did not see are skipped
/// with a note.
async fn read_table(
    conn: &dyn Connector,
    spec: &ConnectorSpec,
    snap: &CatalogSnapshot,
    table: &str,
    key: &[String],
    columns: Vec<String>,
    max: usize,
) -> std::result::Result<TableRows, String> {
    let (schema, name) = table
        .split_once('.')
        .filter(|(s, n)| !s.is_empty() && !n.is_empty())
        .ok_or_else(|| format!("table {table} must be named schema.table"))?;
    let rel =
        snap.relations.iter().find(|r| r.schema == schema && r.name == name).ok_or_else(|| {
            format!("{table} is not among the tables this crawl read (check the connector's schemas)")
        })?;
    let has = |c: &str| rel.columns.iter().any(|x| x.name == c);
    if key.is_empty() {
        return Err(format!("{table}: no key columns"));
    }
    if let Some(k) = key.iter().find(|k| !has(k)) {
        return Err(format!("{table} has no key column {k}"));
    }
    let mut out = TableRows::default();
    let mut cols: Vec<String> = Vec::new();
    for c in columns {
        if key.contains(&c) || cols.contains(&c) {
            continue;
        }
        if has(&c) {
            cols.push(c);
        } else {
            out.notes.push(format!("{table} has no column {c}: it is not synced"));
        }
    }
    let mut after = None;
    loop {
        let want = (max.saturating_add(1) - out.rows.len()).min(ROW_PAGE);
        let req = RowRequest { table: table.to_string(), key: key.to_vec(), columns: cols.clone(), after, limit: want };
        let page = conn.rows(spec, &req).await.map_err(|e| e.to_string())?;
        let n = page.rows.len();
        out.rows.extend(page.rows);
        if out.rows.len() > max {
            out.rows.truncate(max);
            out.truncated = true;
            break;
        }
        match page.last_key {
            Some(k) if n == want => after = Some(k),
            _ => break,
        }
    }
    Ok(out)
}

/// Read the rows of every table the crawl syncs. A table that cannot be
/// read is reported for its type; it never fails the crawl.
pub async fn fetch_rows(
    conn: &dyn Connector,
    spec: &ConnectorSpec,
    schema: &Schema,
    snap: &CatalogSnapshot,
) -> SyncedRows {
    let mut out = SyncedRows::default();
    for (name, b) in synced_types(schema, &spec.name) {
        let mut columns: Vec<String> = b.properties.values().chain(b.links.values()).cloned().collect();
        columns.extend(b.title.iter().chain(b.state.iter()).cloned());
        let rows = read_table(conn, spec, snap, &b.table, &b.key, columns, b.max_rows()).await;
        out.objects.insert(name.to_string(), rows);
    }
    for (name, lb) in synced_join_links(schema, &spec.name) {
        let key = vec![lb.from.clone(), lb.to.clone()];
        let rows = read_table(conn, spec, snap, &lb.table, &key, Vec::new(), DEFAULT_MAX_ROWS).await;
        out.links.insert(name.to_string(), rows);
    }
    out
}

/// Object types in the order a sync writes them: the targets of a type's
/// foreign-key links before the type, so its links find their objects.
fn sync_order(schema: &Schema, types: &[String]) -> Vec<String> {
    fn visit(schema: &Schema, t: &str, types: &[String], seen: &mut HashSet<String>, out: &mut Vec<String>) {
        if !seen.insert(t.to_string()) {
            return;
        }
        if let Some(b) = schema.object_types.get(t).and_then(|ty| ty.backing.as_ref()) {
            for rel in b.links.keys() {
                if let Some(to) = schema.link_types.get(rel).and_then(|lt| lt.to.first())
                    && types.contains(to)
                {
                    visit(schema, to, types, seen, out);
                }
            }
        }
        out.push(t.to_string());
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for t in types {
        visit(schema, t, types, &mut seen, &mut out);
    }
    out
}

/// The ACL objects of `table` inherit: the table entity's, when it is
/// restricted.
fn table_acl(g: &onto_core::graph::Graph, source: &str, table: &str) -> Option<Vec<String>> {
    let e = ["table", "view"].iter().find_map(|k| g.get(&format!("{k}:{source}.{table}")))?;
    (e.prop_str("visibility") == Some("restricted")).then(|| {
        e.props
            .get("acl")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default()
    })
}

/// Is a join table with ACL `join` (when restricted) readable by fewer
/// principals than the tables it joins, whose ACLs are `ends`? It is not
/// when one end is restricted to principals the join table also admits.
fn join_restricted(join: Option<Vec<String>>, ends: [Option<Vec<String>>; 2]) -> bool {
    let Some(join) = join else { return false };
    !ends.iter().flatten().any(|acl| !acl.is_empty() && acl.iter().all(|a| join.contains(a)))
}

fn apply_batches(onto: &Ontology, ctx: &ActionContext, muts: Vec<Mutation>) -> Result<()> {
    for chunk in muts.chunks(BATCH) {
        onto.apply(ctx, chunk.to_vec())?;
    }
    Ok(())
}

fn kv_load<T: serde::de::DeserializeOwned + Default>(onto: &Ontology, key: &str) -> Result<T> {
    match onto.store().kv_get(NS_STATE, key)? {
        Some(v) => Ok(serde_json::from_value(v)?),
        None => Ok(T::default()),
    }
}

/// Write synced rows as objects and links: each object type against its
/// own fingerprints, targets of links first, then the join tables. A type
/// that fails is a note; the others are still written.
fn sync_rows(
    onto: &Ontology,
    spec: &ConnectorSpec,
    source: &str,
    ctx: &ActionContext,
    base: RowOptions,
    rows: SyncedRows,
    report: &mut CrawlReport,
) -> Result<()> {
    let schema = onto.schema();
    let mut planned: HashMap<String, (Vec<catalog::Mapped>, BTreeSet<String>, ObjectSync)> = HashMap::new();
    for (name, read) in rows.objects {
        let Some((ty, b)) = schema
            .object_types
            .get(&name)
            .and_then(|t| t.backing.as_ref().filter(|b| b.sync && b.source == spec.name).map(|b| (t, b)))
        else {
            continue;
        };
        let t = match read {
            Ok(t) => t,
            Err(e) => {
                report.notes.push(format!("objects {name}: not synced: {e}"));
                continue;
            }
        };
        report.notes.extend(t.notes.iter().map(|n| format!("objects {name}: {n}")));
        if t.truncated {
            report.notes.push(format!(
                "objects {name}: {} has more than {} rows; only the first {} by key are synced (backing.max_rows)",
                b.table,
                b.max_rows(),
                b.max_rows()
            ));
        }
        let acl = if spec.map_grants { onto.read(|g, _| table_acl(g, source, &b.table)) } else { None };
        let opts = RowOptions { acl, ..base.clone() };
        let (ok, invalid) = objects::check_rows(objects::map_rows(&name, ty, b, &schema, &t.rows, &opts), &schema);
        // Counts only: the report is kept in the connector state, which
        // every reader may list, and a row's key or values may be restricted.
        if !invalid.is_empty() {
            report.notes.push(format!(
                "objects {name}: {} rows do not fit the schema (a state or enum value it does not allow, a required \
                 property without a value, or an id that is too long) and keep their objects as they were",
                invalid.len()
            ));
        }
        let sync =
            ObjectSync { rows: t.rows.len(), invalid: invalid.len(), truncated: t.truncated, ..Default::default() };
        planned.insert(name, (ok, invalid.into_iter().map(|(id, _)| id).collect(), sync));
    }
    let names: Vec<String> = {
        let mut n: Vec<String> = planned.keys().cloned().collect();
        n.sort();
        sync_order(&schema, &n)
    };
    let mut later: HashSet<String> =
        planned.values().flat_map(|(m, _, _)| m.iter().map(|x| x.id().to_string())).collect();
    for name in names {
        let Some((mapped, invalid, mut sync)) = planned.remove(&name) else { continue };
        for m in &mapped {
            later.remove(m.id());
        }
        let Some(b) = schema.object_types.get(&name).and_then(|t| t.backing.as_ref()) else { continue };
        let key = format!("{}/objects/{name}", spec.name);
        let previous: Fingerprints = kv_load(onto, &key)?;
        let managed: Vec<&str> = b.links.keys().map(String::as_str).collect();
        let plan = onto.read(|g, _| objects::plan_objects(mapped, &previous, g, &managed, &later, &invalid));
        if let Err(e) = apply_batches(onto, ctx, plan.mutations) {
            report.notes.push(format!("objects {name}: writing failed, the next crawl retries: {e}"));
            continue;
        }
        onto.store().kv_put(NS_STATE, &key, &serde_json::to_value(&plan.fingerprints)?, None)?;
        sync.upserted = plan.upserted;
        sync.unchanged = plan.unchanged;
        sync.deleted = plan.deleted;
        report.objects.insert(name, sync);
    }
    for (name, read) in rows.links {
        let Some((lt, lb)) = schema
            .link_types
            .get(&name)
            .and_then(|lt| lt.backing.as_ref().filter(|b| b.source == spec.name).map(|b| (lt, b)))
        else {
            continue;
        };
        let (Some(from), Some(to)) = (lt.from.first(), lt.to.first()) else { continue };
        let t = match read {
            Ok(t) => t,
            Err(e) => {
                report.notes.push(format!("links {name}: not synced: {e}"));
                continue;
            }
        };
        if t.truncated {
            report
                .notes
                .push(format!("links {name}: only the first {DEFAULT_MAX_ROWS} rows of {} are synced", lb.table));
        }
        let key = format!("{}/links/{name}", spec.name);
        let previous: BTreeSet<LinkKey> = kv_load(onto, &key)?;
        // Links carry no ACL of their own: anyone who sees both ends sees
        // the link. A join table readable by fewer principals than one of
        // the tables it joins is not synced, and links synced from it
        // before are removed.
        let hidden = spec.map_grants
            && onto.read(|g, _| {
                let end = |kind: &str| {
                    schema
                        .object_types
                        .get(kind)
                        .and_then(|t| t.backing.as_ref())
                        .map(|b| table_acl(g, source, &b.table))
                };
                join_restricted(table_acl(g, source, &lb.table), [end(from).flatten(), end(to).flatten()])
            });
        let new = if hidden {
            report.notes.push(format!(
                "links {name}: not synced: {} is restricted beyond the tables it joins, and links have no ACL of their own",
                lb.table
            ));
            Vec::new()
        } else {
            objects::map_join_rows(&name, lb, from, to, &t.rows)
        };
        let (muts, keys, skipped) = onto.read(|g, _| {
            // Links removed since (with an object they joined) are made again.
            let previous: BTreeSet<LinkKey> =
                previous.into_iter().filter(|(f, r, t)| g.get_link(f, r, t).is_some()).collect();
            let (ok, missing): (Vec<_>, Vec<_>) =
                new.into_iter().partition(|l| g.contains(&l.from) && g.contains(&l.to));
            let (muts, keys) = objects::plan_links(ok, &previous);
            (muts, keys, missing.len())
        });
        let linked = muts.iter().filter(|m| matches!(m, Mutation::Link(_))).count();
        let unlinked = muts.len() - linked;
        if let Err(e) = apply_batches(onto, ctx, muts) {
            report.notes.push(format!("links {name}: writing failed, the next crawl retries: {e}"));
            continue;
        }
        onto.store().kv_put(NS_STATE, &key, &serde_json::to_value(&keys)?, None)?;
        report.links.insert(name, LinkSync { rows: t.rows.len(), linked, unlinked, skipped });
    }
    Ok(())
}

/// Write a snapshot into the ontology: map, keep what changed, apply in
/// batches, sync the rows read for backed object types, learn, rank.
/// Synchronous; callers run it off async workers.
pub fn ingest_snapshot(
    onto: &Ontology,
    spec: &ConnectorSpec,
    snap: CatalogSnapshot,
    rows: SyncedRows,
    started: i64,
) -> Result<CrawlReport> {
    let mut state = load_state(onto, &spec.name)?;
    let ws = spec.workspace.clone().unwrap_or_else(|| onto.options().default_workspace.clone());
    let opts = MapOptions {
        workspace: ws.clone(),
        connector: spec.name.clone(),
        crawled_at: started,
        run: new_id("crawl"),
        map_grants: spec.map_grants,
        grant_map: spec.grant_map.clone(),
        owner_map: spec.owner_map.clone(),
    };
    let mut report = CrawlReport {
        connector: spec.name.clone(),
        source: catalog::source_id(&snap.source.key),
        started_at: started,
        schemas: snap.schemas.len(),
        tables: snap.relations.iter().filter(|r| r.kind != catalog::RelKind::View).count(),
        views: snap.relations.iter().filter(|r| r.kind == catalog::RelKind::View).count(),
        columns: snap.relations.iter().map(|r| r.columns.len()).sum(),
        foreign_keys: snap.foreign_keys.len(),
        queries: snap.queries.len(),
        notes: snap.notes.clone(),
        ..Default::default()
    };
    let mapped = catalog::map_snapshot(&snap, &opts);
    let base = RowOptions {
        workspace: ws.clone(),
        connector: spec.name.clone(),
        engine: snap.source.engine.clone(),
        crawled_at: started,
        run: opts.run.clone(),
        map_grants: spec.map_grants,
        acl: None,
    };
    let plan = onto.read(|g, _| catalog::plan_changes(mapped, &state.fingerprints, g, catalog::MANAGED_RELS));
    report.upserted = plan.upserted;
    report.unchanged = plan.unchanged;
    report.deleted = plan.deleted;
    report.links_removed = plan.links_removed;
    let ctx =
        ActionContext::new(connector_principal(&spec.name), ws.clone()).with_source(format!("connector:{}", spec.name));
    // Small batches: the graph lock is held per batch, never for the whole crawl.
    apply_batches(onto, &ctx, plan.mutations)?;
    sync_rows(onto, spec, &snap.source.key, &ctx, base, rows, &mut report)?;
    report.learned = onto.learn(Some(&ws))?;
    report.ranked = onto.compute_authority(&ctx, &[], &Weights::default())?.ranked;
    report.ms = (now_ms() - started).max(0) as u64;
    onto.apply(
        &ctx,
        vec![Mutation::Event(EventInput {
            action: "connector.crawled".into(),
            target: Some(report.source.clone()),
            payload: json!({
                "upserted": report.upserted, "unchanged": report.unchanged, "deleted": report.deleted,
                "tables": report.tables, "views": report.views, "columns": report.columns, "queries": report.queries,
                "learned": report.learned.written, "ms": report.ms,
                "objects": report.objects, "links": report.links,
            }),
            ..Default::default()
        })],
    )?;
    state.fingerprints = plan.fingerprints;
    state.last_crawl_at = started;
    state.last_success_at = now_ms();
    state.last_error = None;
    state.crawls += 1;
    state.last = Some(report.clone());
    save_state(onto, &spec.name, &state)?;
    Ok(report)
}

impl Intelli {
    pub(crate) fn connector_spec(&self, name: &str) -> Result<ConnectorSpec> {
        let v = self
            .ontology()
            .store()
            .kv_get(NS_SPEC, name)?
            .ok_or_else(|| Error::not_found(format!("connector {name}")))?;
        Ok(serde_json::from_value(v)?)
    }

    fn connector_specs(&self) -> Result<Vec<ConnectorSpec>> {
        self.ontology().store().kv_scan(NS_SPEC, "")?.into_iter().map(|(_, v)| Ok(serde_json::from_value(v)?)).collect()
    }

    /// Register (or replace) a connector. Admins only.
    pub fn add_connector(&self, p: &Principal, mut spec: ConnectorSpec) -> Result<ConnectorSpec> {
        require_admin(p, "add connectors")?;
        spec.validate()?;
        spec.created_by = p.id.clone();
        spec.created_at = now_ms();
        let ctx = ActionContext::new(p.clone(), self.default_workspace()).with_source("api");
        self.ontology().store().kv_put(NS_SPEC, &spec.name, &serde_json::to_value(&spec)?, None)?;
        self.ontology().apply(
            &ctx,
            vec![Mutation::Event(EventInput {
                action: "connector.added".into(),
                target: Some(catalog::source_id(&spec.name)),
                payload: serde_json::to_value(spec.redacted())?,
                ..Default::default()
            })],
        )?;
        Ok(spec.redacted())
    }

    /// Connectors with their last crawl (URLs redacted).
    pub fn connectors(&self, p: &Principal) -> Result<Vec<Value>> {
        if !p.allows(onto_core::Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let onto = self.ontology();
        self.connector_specs()?
            .into_iter()
            .map(|s| {
                let st = load_state(onto, &s.name)?;
                Ok(json!({
                    "spec": s.redacted(),
                    "last_crawl_at": st.last_crawl_at,
                    "last_success_at": st.last_success_at,
                    "last_error": st.last_error,
                    "crawls": st.crawls,
                    "assets": st.fingerprints.len(),
                    "last": st.last,
                }))
            })
            .collect()
    }

    /// Remove a connector; `purge` also deletes the entities it crawled.
    pub fn remove_connector(&self, p: &Principal, name: &str, purge: bool) -> Result<Value> {
        require_admin(p, "remove connectors")?;
        let spec = self.connector_spec(name)?;
        let onto = self.ontology();
        let state = load_state(onto, name)?;
        let mut deleted = 0;
        let ws = spec.workspace.clone().unwrap_or_else(|| self.default_workspace().to_string());
        // Fingerprints of synced objects and join links, per type.
        let synced = onto.store().kv_scan(NS_STATE, &format!("{name}/"))?;
        if purge {
            let ctx = ActionContext::new(p.clone(), ws.clone()).with_source(format!("connector:{name}"));
            let mut written: BTreeSet<String> = state.fingerprints.keys().cloned().collect();
            for (key, v) in &synced {
                if key.starts_with(&format!("{name}/objects/")) {
                    let fps: Fingerprints = serde_json::from_value(v.clone())?;
                    written.extend(fps.into_keys());
                }
            }
            let ids: Vec<String> = onto.read(|g, _| written.into_iter().filter(|id| g.contains(id)).collect());
            deleted = ids.len();
            apply_batches(
                onto,
                &ctx,
                ids.into_iter().map(|id| Mutation::Delete(onto_core::EntityRef { id })).collect(),
            )?;
        }
        onto.store().kv_delete(NS_SPEC, name)?;
        onto.store().kv_delete(NS_STATE, name)?;
        for (key, _) in synced {
            onto.store().kv_delete(NS_STATE, &key)?;
        }
        let ctx = ActionContext::new(p.clone(), ws).with_source("api");
        onto.apply(
            &ctx,
            vec![Mutation::Event(EventInput {
                action: "connector.removed".into(),
                target: Some(catalog::source_id(name)),
                payload: json!({ "purge": purge, "deleted": deleted }),
                ..Default::default()
            })],
        )?;
        Ok(json!({ "removed": name, "deleted": deleted }))
    }

    /// Crawl one connector (or all) now and wait for the result. Admins only.
    pub async fn crawl(&self, p: &Principal, name: Option<&str>) -> Result<Vec<CrawlReport>> {
        require_admin(p, "crawl")?;
        let specs = match name {
            Some(n) => vec![self.connector_spec(n)?],
            None => self.connector_specs()?,
        };
        let mut out = Vec::new();
        for spec in specs {
            out.push(self.crawl_one(&spec).await?);
        }
        Ok(out)
    }

    /// Crawl under a lease, so replicas never crawl the same source at once.
    pub(crate) async fn crawl_one(&self, spec: &ConnectorSpec) -> Result<CrawlReport> {
        let onto = self.ontology().clone();
        let lease = format!("crawl:{}", spec.name);
        let holder = new_id("crawler");
        if !onto.try_lease(&lease, &holder, 30 * 60_000)? {
            return Err(Error::Conflict(format!("connector {} is being crawled by another process", spec.name)));
        }
        let started = now_ms();
        let result = async {
            let connector = connector_for(&spec.kind)?;
            let snap = connector.snapshot(spec).await;
            let rows = match &snap {
                Ok(snap) => fetch_rows(connector.as_ref(), spec, &onto.schema(), snap).await,
                Err(_) => SyncedRows::default(),
            };
            connector.close().await;
            let snap = snap?;
            let (o, s) = (onto.clone(), spec.clone());
            tokio::task::spawn_blocking(move || ingest_snapshot(&o, &s, snap, rows, started))
                .await
                .map_err(|e| Error::Other(format!("crawl task: {e}")))?
        }
        .await;
        if let Err(e) = &result {
            let mut st = load_state(&onto, &spec.name).unwrap_or_default();
            st.last_crawl_at = started;
            st.last_error = Some(e.to_string());
            let _ = save_state(&onto, &spec.name, &st);
        }
        let _ = onto.release_lease(&lease, &holder);
        result
    }
}

/// Crawl due connectors every `tick`, for as long as the ontology is open.
pub(crate) fn spawn_scheduler(intelli: &Arc<Intelli>, tick: Duration) {
    let weak = Arc::downgrade(intelli);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            let Some(me) = weak.upgrade() else { break };
            let Ok(specs) = me.connector_specs() else { continue };
            for spec in specs.into_iter().filter(|s| s.every_secs > 0) {
                let st = load_state(me.ontology(), &spec.name).unwrap_or_default();
                if now_ms() - st.last_crawl_at < (spec.every_secs as i64) * 1000 {
                    continue;
                }
                if let Err(e) = me.crawl_one(&spec).await {
                    tracing::warn!(connector = %spec.name, "crawl failed: {e}");
                }
            }
        }
    });
}

#[cfg(feature = "postgres")]
pub mod postgres {
    //! PostgreSQL metadata: `pg_catalog` (including enum labels, unique
    //! indexes and CHECK constraints), `pg_stat_user_tables` and, when the
    //! extension is installed, `pg_stat_statements`. One read-only
    //! transaction with a statement timeout; no table is read for it.
    //!
    //! Rows of synced tables are read a page per read-only transaction, in
    //! key order (keyset paging), with only the key and mapped columns
    //! selected. Identifiers are always quoted; values are bound, never
    //! written into the statement.

    use std::time::Duration;

    use async_trait::async_trait;
    use onto_core::catalog::{
        self, CatalogSnapshot, ColumnInfo, ForeignKey, QueryStat, RelKind, RelationInfo, SchemaInfo, SourceInfo,
        ViewDep,
    };
    use onto_core::sqlx::{self, postgres::PgPoolOptions};
    use onto_core::{Error, Result};
    use serde_json::{Map, Value};

    use super::{Connector, ConnectorSpec, RowPage, RowRequest};

    /// Reads one source for one crawl: the connection used for rows is
    /// opened on the first page and kept until [`Connector::close`].
    #[derive(Default)]
    pub struct PgConnector {
        pool: parking_lot::Mutex<Option<sqlx::PgPool>>,
    }

    async fn connect(spec: &ConnectorSpec, url: &str) -> Result<sqlx::PgPool> {
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(15))
            .connect(url)
            .await
            .map_err(|e| Error::Unavailable(format!("connector {}: {e}", spec.name)))
    }

    impl PgConnector {
        async fn pool(&self, spec: &ConnectorSpec) -> Result<sqlx::PgPool> {
            if let Some(pool) = self.pool.lock().clone() {
                return Ok(pool);
            }
            let pool = connect(spec, &spec.resolve_url()?).await?;
            *self.pool.lock() = Some(pool.clone());
            Ok(pool)
        }
    }

    /// `"name"`, with embedded quotes doubled.
    pub fn quote_ident(name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    /// The statement that reads one page of `req`: `$1` is the limit and,
    /// when reading after a key, `$2` that key as a JSON object, turned into
    /// the key columns' own types by `jsonb_populate_record` so the
    /// comparison and the `ORDER BY` agree.
    pub fn rows_sql(req: &RowRequest) -> Result<String> {
        let (schema, table) = req
            .table
            .split_once('.')
            .filter(|(s, t)| !s.is_empty() && !t.is_empty())
            .ok_or_else(|| Error::invalid(format!("table {} must be named schema.table", req.table)))?;
        if req.key.is_empty() || req.key.iter().chain(&req.columns).any(|c| c.is_empty()) {
            return Err(Error::invalid(format!("reading {}: key and column names must not be empty", req.table)));
        }
        let rel = format!("{}.{}", quote_ident(schema), quote_ident(table));
        let keys: Vec<String> = req.key.iter().map(|k| quote_ident(k)).collect();
        let mut select = keys.clone();
        for c in &req.columns {
            let q = quote_ident(c);
            if !select.contains(&q) {
                select.push(q);
            }
        }
        let mut cond: Vec<String> = keys.iter().map(|k| format!("{k} IS NOT NULL")).collect();
        if req.after.is_some() {
            let typed: Vec<String> = keys.iter().map(|k| format!("r.{k}")).collect();
            cond.push(format!(
                "({}) > (SELECT {} FROM jsonb_populate_record(NULL::{rel}, ($2::text)::jsonb) r)",
                keys.join(", "),
                typed.join(", ")
            ));
        }
        let outer: Vec<String> = keys.iter().map(|k| format!("t.{k}")).collect();
        Ok(format!(
            "SELECT to_jsonb(t)::text FROM (SELECT {} FROM {rel} WHERE {} ORDER BY {} LIMIT $1) t ORDER BY {}",
            select.join(", "),
            cond.join(" AND "),
            keys.join(", "),
            outer.join(", ")
        ))
    }

    type RelRow = (String, String, String, String, String, f32, Option<String>, i64, i64, Option<f64>);
    /// schema, table, column, type, nullable, position, comment, primary key,
    /// unique, enum labels.
    type ColRow = (String, String, String, String, bool, i32, String, bool, bool, Vec<String>);
    type FkRow = (String, String, String, String, String, Vec<String>, Vec<String>);
    /// queryid, text, calls, mean ms, rows.
    type StatementRow = (String, String, i64, f64, i64);

    #[async_trait]
    impl Connector for PgConnector {
        async fn snapshot(&self, spec: &ConnectorSpec) -> Result<CatalogSnapshot> {
            let url = spec.resolve_url()?;
            let parsed =
                sqlx::Url::parse(&url).map_err(|e| Error::Config(format!("connector {}: bad URL: {e}", spec.name)))?;
            let pool = connect(spec, &url).await?;
            let out = read(&pool, spec, &parsed).await;
            pool.close().await;
            out
        }

        async fn rows(&self, spec: &ConnectorSpec, req: &RowRequest) -> Result<RowPage> {
            let sql = rows_sql(req)?;
            let pool = self.pool(spec).await?;
            let mut tx = pool.begin().await?;
            sqlx::query("SET TRANSACTION READ ONLY").execute(&mut *tx).await?;
            sqlx::query("SET LOCAL statement_timeout = '60s'").execute(&mut *tx).await?;
            let mut query = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).bind(req.limit.clamp(1, 10_000) as i64);
            if let Some(after) = &req.after {
                let key: Map<String, Value> = req.key.iter().cloned().zip(after.iter().cloned()).collect();
                query = query.bind(Value::Object(key).to_string());
            }
            let texts: Vec<String> = query.fetch_all(&mut *tx).await?;
            tx.commit().await?;
            let rows: Vec<Map<String, Value>> = texts
                .iter()
                .map(|t| serde_json::from_str(t).map_err(|e| Error::Other(format!("reading {}: {e}", req.table))))
                .collect::<Result<_>>()?;
            let last_key =
                rows.last().map(|r| req.key.iter().map(|k| r.get(k).cloned().unwrap_or(Value::Null)).collect());
            Ok(RowPage { rows, last_key })
        }

        async fn close(&self) {
            let pool = self.pool.lock().take();
            if let Some(pool) = pool {
                pool.close().await;
            }
        }
    }

    async fn read(pool: &sqlx::PgPool, spec: &ConnectorSpec, url: &sqlx::Url) -> Result<CatalogSnapshot> {
        let mut tx = pool.begin().await?;
        sqlx::query("SET TRANSACTION READ ONLY").execute(&mut *tx).await?;
        sqlx::query("SET LOCAL statement_timeout = '60s'").execute(&mut *tx).await?;
        let (database, version): (String, String) =
            sqlx::query_as("SELECT current_database()::text, current_setting('server_version')")
                .fetch_one(&mut *tx)
                .await?;
        let mut snap = CatalogSnapshot {
            source: SourceInfo {
                key: spec.name.clone(),
                title: if spec.title.is_empty() { format!("{database} ({})", spec.name) } else { spec.title.clone() },
                engine: "postgres".into(),
                host: url.host_str().unwrap_or_default().to_string(),
                database,
                server_version: version,
            },
            ..Default::default()
        };

        let schemas: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT n.nspname::text, coalesce(obj_description(n.oid, 'pg_namespace'), ''), pg_get_userbyid(n.nspowner)::text \
             FROM pg_namespace n WHERE n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%' \
             AND has_schema_privilege(n.oid, 'USAGE') ORDER BY 1",
        )
        .fetch_all(&mut *tx)
        .await?;
        let schemas: Vec<(String, String, String)> =
            schemas.into_iter().filter(|(n, _, _)| spec.wants_schema(n)).collect();
        let names: Vec<String> = schemas.iter().map(|s| s.0.clone()).collect();
        snap.schemas = schemas.into_iter().map(|(name, comment, owner)| SchemaInfo { name, comment, owner }).collect();
        if names.is_empty() {
            snap.notes.push("no schemas matched the connector's filters".into());
            tx.commit().await?;
            return Ok(snap);
        }

        let rels: Vec<RelRow> = sqlx::query_as(
            "SELECT n.nspname::text, c.relname::text, c.relkind::text, coalesce(obj_description(c.oid, 'pg_class'), ''), \
             pg_get_userbyid(c.relowner)::text, c.reltuples, \
             CASE WHEN c.relkind IN ('v', 'm') THEN pg_get_viewdef(c.oid, true) END, \
             coalesce(s.seq_scan, 0) + coalesce(s.idx_scan, 0), \
             coalesce(s.n_tup_ins, 0) + coalesce(s.n_tup_upd, 0) + coalesce(s.n_tup_del, 0), \
             extract(epoch FROM greatest(s.last_analyze, s.last_autoanalyze, s.last_vacuum, s.last_autovacuum))::float8 * 1000 \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid \
             WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND NOT c.relispartition AND n.nspname = ANY($1) \
             ORDER BY 1, 2",
        )
        .bind(&names)
        .fetch_all(&mut *tx)
        .await?;
        let grants: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT n.nspname::text, c.relname::text, \
             CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_get_userbyid(a.grantee)::text END \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace, \
             aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) a \
             WHERE a.privilege_type = 'SELECT' AND c.relkind IN ('r', 'p', 'v', 'm', 'f') AND n.nspname = ANY($1)",
        )
        .bind(&names)
        .fetch_all(&mut *tx)
        .await?;
        let cols: Vec<ColRow> = sqlx::query_as(
            "SELECT n.nspname::text, c.relname::text, a.attname::text, format_type(a.atttypid, a.atttypmod), \
             NOT a.attnotnull, a.attnum::int4, coalesce(col_description(c.oid, a.attnum), ''), \
             EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY(i.indkey)), \
             EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.oid AND i.indisunique AND i.indnkeyatts = 1 \
                     AND i.indkey[0] = a.attnum AND i.indpred IS NULL), \
             array(SELECT e.enumlabel::text FROM pg_enum e \
                   WHERE e.enumtypid = CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.oid END \
                   ORDER BY e.enumsortorder) \
             FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_type t ON t.oid = a.atttypid \
             WHERE a.attnum > 0 AND NOT a.attisdropped AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
             AND NOT c.relispartition AND n.nspname = ANY($1) ORDER BY 1, 2, a.attnum",
        )
        .bind(&names)
        .fetch_all(&mut *tx)
        .await?;
        let mut relations: Vec<RelationInfo> = rels
            .into_iter()
            .map(|(schema, name, kind, comment, owner, tuples, definition, reads, writes, last)| RelationInfo {
                kind: match kind.as_str() {
                    "p" => RelKind::Partitioned,
                    "f" => RelKind::Foreign,
                    "m" => RelKind::MaterializedView,
                    "v" => RelKind::View,
                    _ => RelKind::Table,
                },
                public_read: grants.iter().any(|(s, n, g)| s == &schema && n == &name && g == "PUBLIC"),
                readers: grants
                    .iter()
                    .filter(|(s, n, g)| s == &schema && n == &name && g != "PUBLIC")
                    .map(|(_, _, g)| g.clone())
                    .collect(),
                row_estimate: tuples.max(0.0) as i64,
                last_activity: last.map(|ms| ms as i64),
                schema,
                name,
                comment,
                owner,
                definition,
                reads,
                writes,
                columns: Vec::new(),
            })
            .collect();
        {
            let index: std::collections::HashMap<(String, String), usize> =
                relations.iter().enumerate().map(|(i, r)| ((r.schema.clone(), r.name.clone()), i)).collect();
            for (schema, table, name, data_type, nullable, position, comment, pk, unique, enum_values) in cols {
                if let Some(&i) = index.get(&(schema, table)) {
                    relations[i].columns.push(ColumnInfo {
                        name,
                        data_type,
                        nullable,
                        position: i64::from(position),
                        primary_key: pk,
                        comment,
                        enum_values,
                        unique,
                    });
                }
            }
            // Columns limited to a list of values by a CHECK constraint.
            let checks: Vec<(String, String, String, String)> = sqlx::query_as(
                "SELECT n.nspname::text, c.relname::text, a.attname::text, pg_get_constraintdef(con.oid) \
                 FROM pg_constraint con JOIN pg_class c ON c.oid = con.conrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = con.conkey[1] \
                 WHERE con.contype = 'c' AND cardinality(con.conkey) = 1 AND NOT c.relispartition \
                 AND n.nspname = ANY($1) ORDER BY con.conname",
            )
            .bind(&names)
            .fetch_all(&mut *tx)
            .await?;
            for (schema, table, column, def) in checks {
                let values = catalog::check_values(&def, &column);
                let Some(&i) = index.get(&(schema, table)) else { continue };
                if let Some(c) = relations[i].columns.iter_mut().find(|c| c.name == column && c.enum_values.is_empty())
                    && !values.is_empty()
                {
                    c.enum_values = values;
                }
            }
        }
        snap.relations = relations;

        let fks: Vec<FkRow> = sqlx::query_as(
            "SELECT con.conname::text, sn.nspname::text, sc.relname::text, tn.nspname::text, tc.relname::text, \
             array(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(n, o) \
                   JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.n ORDER BY k.o), \
             array(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY k(n, o) \
                   JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.n ORDER BY k.o) \
             FROM pg_constraint con \
             JOIN pg_class sc ON sc.oid = con.conrelid JOIN pg_namespace sn ON sn.oid = sc.relnamespace \
             JOIN pg_class tc ON tc.oid = con.confrelid JOIN pg_namespace tn ON tn.oid = tc.relnamespace \
             WHERE con.contype = 'f' AND sn.nspname = ANY($1) AND tn.nspname = ANY($1) AND NOT sc.relispartition ORDER BY 1",
        )
        .bind(&names)
        .fetch_all(&mut *tx)
        .await?;
        snap.foreign_keys = fks
            .into_iter()
            .map(|(name, from_schema, from_table, to_schema, to_table, from_columns, to_columns)| ForeignKey {
                name,
                from_schema,
                from_table,
                from_columns,
                to_schema,
                to_table,
                to_columns,
            })
            .collect();

        let deps: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT DISTINCT vn.nspname::text, v.relname::text, tn.nspname::text, t.relname::text \
             FROM pg_rewrite r JOIN pg_class v ON v.oid = r.ev_class JOIN pg_namespace vn ON vn.oid = v.relnamespace \
             JOIN pg_depend d ON d.objid = r.oid AND d.classid = 'pg_rewrite'::regclass AND d.refclassid = 'pg_class'::regclass \
             JOIN pg_class t ON t.oid = d.refobjid JOIN pg_namespace tn ON tn.oid = t.relnamespace \
             WHERE v.relkind IN ('v', 'm') AND t.oid <> v.oid AND t.relkind IN ('r', 'p', 'v', 'm', 'f') \
             AND vn.nspname = ANY($1) AND tn.nspname = ANY($1)",
        )
        .bind(&names)
        .fetch_all(&mut *tx)
        .await?;
        snap.view_deps = deps
            .into_iter()
            .map(|(view_schema, view, schema, name)| ViewDep { view_schema, view, schema, name })
            .collect();

        // Query usage: normalized SELECTs only (literals are $n there; utility
        // statements are skipped because they are not normalized).
        let pss: Option<String> = sqlx::query_scalar(
            "SELECT n.nspname::text FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace \
             WHERE e.extname = 'pg_stat_statements'",
        )
        .fetch_optional(&mut *tx)
        .await?;
        match pss {
            None => snap.notes.push(
                "pg_stat_statements is not installed in this database: no query usage or learned joins from queries \
                 (CREATE EXTENSION pg_stat_statements, with it in shared_preload_libraries)"
                    .into(),
            ),
            Some(ns) => {
                sqlx::query("SAVEPOINT onto_pss").execute(&mut *tx).await?;
                // Only statements that can touch the crawled schemas: those
                // naming one of them, or any when `public` (the default
                // search path) is crawled; never ones naming an excluded schema.
                let like = |s: &str| s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                let mentions: Vec<String> =
                    names.iter().flat_map(|n| [format!("%{}.%", like(n)), format!("%\"{}\".%", like(n))]).collect();
                let excluded: Vec<String> =
                    spec.exclude_schemas.iter().map(|p| format!("%{}.%", like(p).replace('*', "%"))).collect();
                let unqualified = names.iter().any(|n| n == "public");
                let q = format!(
                    "SELECT queryid::text, query, calls, mean_exec_time, rows FROM \"{}\".pg_stat_statements \
                     WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) AND toplevel \
                     AND query ~* '^\\s*(select|with)\\M' AND ($2 OR query ILIKE ANY($3)) \
                     AND NOT query ILIKE ANY($4) ORDER BY calls DESC LIMIT $1",
                    ns.replace('"', "\"\"")
                );
                let rows: std::result::Result<Vec<StatementRow>, _> = sqlx::query_as(sqlx::AssertSqlSafe(q))
                    .bind(spec.max_queries.clamp(1, 10_000) as i64)
                    .bind(unqualified)
                    .bind(&mentions)
                    .bind(&excluded)
                    .fetch_all(&mut *tx)
                    .await;
                match rows {
                    Ok(rows) => {
                        let hidden = rows.iter().filter(|r| r.1.starts_with('<')).count();
                        if hidden > 0 {
                            snap.notes.push(format!(
                                "{hidden} statements of other roles are hidden: grant pg_read_all_stats to the connector's role"
                            ));
                        }
                        snap.queries = rows
                            .into_iter()
                            .filter(|r| !r.1.starts_with('<'))
                            .map(|(queryid, text, calls, mean_ms, rows)| QueryStat {
                                queryid,
                                text,
                                calls,
                                mean_ms,
                                rows,
                            })
                            .collect();
                    }
                    Err(e) => {
                        sqlx::query("ROLLBACK TO SAVEPOINT onto_pss").execute(&mut *tx).await?;
                        snap.notes.push(format!("pg_stat_statements could not be read: {e}"));
                    }
                }
            }
        }
        tx.commit().await?;
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onto_core::modeling::DraftOptions;
    use parking_lot::Mutex;

    /// Tables in memory, read the way the PostgreSQL connector reads them:
    /// key order, keyset paging, only the requested columns.
    #[derive(Default)]
    struct FakeSource {
        tables: Mutex<BTreeMap<String, Vec<Row>>>,
        broken: Mutex<BTreeSet<String>>,
        requests: Mutex<Vec<RowRequest>>,
    }

    fn key_cmp(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
        let one = |x: &Value, y: &Value| match (x.as_f64(), y.as_f64()) {
            (Some(p), Some(q)) => p.total_cmp(&q),
            _ => x.to_string().cmp(&y.to_string()),
        };
        a.iter().zip(b).map(|(x, y)| one(x, y)).find(|o| o.is_ne()).unwrap_or(std::cmp::Ordering::Equal)
    }

    #[async_trait]
    impl Connector for FakeSource {
        async fn snapshot(&self, _: &ConnectorSpec) -> Result<CatalogSnapshot> {
            Ok(crate::seed::demo_warehouse())
        }

        async fn rows(&self, _: &ConnectorSpec, req: &RowRequest) -> Result<RowPage> {
            self.requests.lock().push(req.clone());
            if self.broken.lock().contains(&req.table) {
                return Err(Error::Unavailable(format!("{} is offline", req.table)));
            }
            let key_of = |r: &Row| req.key.iter().map(|k| r.get(k).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>();
            let mut rows: Vec<Row> = self.tables.lock().get(&req.table).cloned().unwrap_or_default();
            rows.sort_by(|a, b| key_cmp(&key_of(a), &key_of(b)));
            let rows: Vec<Row> = rows
                .into_iter()
                .filter(|r| req.after.as_ref().is_none_or(|after| key_cmp(&key_of(r), after).is_gt()))
                .take(req.limit)
                .map(|r| r.into_iter().filter(|(c, _)| req.key.contains(c) || req.columns.contains(c)).collect::<Row>())
                .collect();
            let last_key = rows.last().map(key_of);
            Ok(RowPage { rows, last_key })
        }
    }

    fn row(v: Value) -> Row {
        v.as_object().unwrap().clone()
    }

    fn orders(n: i64) -> Vec<Row> {
        (1..=n)
            .map(|i| {
                row(json!({ "id": i, "customer_id": i % 3 + 1, "amount": i as f64 + 0.5, "status": "paid",
                            "created_at": "2026-10-17T12:00:00+00:00", "secret": "never read" }))
            })
            .collect()
    }

    #[tokio::test]
    async fn crawls_sync_backed_tables_into_objects() {
        let mut schema = Schema::enterprise();
        schema.extend_toml(onto_core::schema::builtin_package("data-catalog").unwrap()).unwrap();
        let o = Ontology::in_memory(schema);
        let mut snap = crate::seed::demo_warehouse();
        let customers_rel = snap.relations.iter_mut().find(|r| r.name == "customers").unwrap();
        customers_rel.public_read = false;
        customers_rel.readers = vec!["analyst".into()];
        let spec: ConnectorSpec = serde_json::from_value(json!({
            "name": "warehouse", "url": "env:WAREHOUSE_URL", "workspace": "acme",
            "map_grants": true, "grant_map": { "analyst": "team:analytics" },
        }))
        .unwrap();
        let admin = Principal::admin("ada");

        // Nothing is backed yet: a crawl reads metadata only.
        let first = ingest_snapshot(&o, &spec, snap.clone(), SyncedRows::default(), 1).unwrap();
        assert!(first.objects.is_empty());
        let draft = o.draft_ontology(&admin, &DraftOptions::for_source("warehouse")).unwrap();
        let ctx = ActionContext::new(admin.clone(), "acme");
        o.schema_apply(&ctx, draft.changes.clone()).unwrap();
        assert_eq!(synced_types(&o.schema(), "warehouse").len(), 2, "customers and orders: {:?}", draft.skipped);
        assert!(synced_types(&o.schema(), "other").is_empty());

        let source = FakeSource::default();
        source.tables.lock().insert(
            "sales.customers".into(),
            vec![
                row(json!({ "id": 1, "cust_name": "Ada", "region": "EU", "created_at": "2026-01-02" })),
                row(json!({ "id": 2, "cust_name": "Bo", "region": "US" })),
                row(json!({ "id": 3, "cust_name": null, "region": "EU" })),
            ],
        );
        source.tables.lock().insert("sales.orders".into(), orders(1201));
        let crawl = |n: i64| {
            let rows = futures::executor::block_on(fetch_rows(&source, &spec, &o.schema(), &snap));
            ingest_snapshot(&o, &spec, snap.clone(), rows, n).unwrap()
        };
        let r = crawl(2);
        assert_eq!(r.objects["customer"], ObjectSync { rows: 3, upserted: 3, ..Default::default() }, "{r:?}");
        assert_eq!(r.objects["order"], ObjectSync { rows: 1201, upserted: 1201, ..Default::default() });
        // Keyset paging, and never a column the backing does not map.
        let order_reads: Vec<RowRequest> =
            source.requests.lock().iter().filter(|q| q.table == "sales.orders").cloned().collect();
        assert_eq!(order_reads.len(), 2);
        assert_eq!(order_reads[1].after, Some(vec![json!(1000)]));
        assert!(order_reads.iter().all(|q| !q.columns.iter().any(|c| c == "secret")), "{order_reads:?}");

        let get = |id: &str| o.get(&admin, id).unwrap();
        let order = get("order:7");
        assert_eq!(order.props["amount"], json!(7.5));
        assert_eq!(order.props["created_at"], json!(1_792_238_400_000_i64));
        assert_eq!(order.provenance.as_ref().unwrap().external_ref.as_deref(), Some("sales.orders#7"));
        assert_eq!(order.props["visibility"], "internal");
        let linked = o.read(|g, _| {
            g.neighbors("order:7", "customer", onto_core::Dir::Out).iter().map(|x| x.to_string()).collect::<Vec<_>>()
        });
        assert_eq!(linked, ["customer:2"]);
        let ada = get("customer:1");
        assert_eq!(ada.title, "Ada");
        assert_eq!(ada.props["visibility"], "restricted", "customers are restricted like their table");
        assert_eq!(ada.props["acl"], json!(["team:analytics"]));
        assert_eq!(get("customer:3").title, "3");
        assert!(o.get(&Principal::named("user:bo", &["member"], &["*"]).unwrap(), "customer:1").is_err());

        // Nothing changed: nothing written.
        let r = crawl(3);
        assert_eq!(r.objects["order"], ObjectSync { rows: 1201, unchanged: 1201, ..Default::default() });
        assert_eq!(r.objects["customer"].unchanged, 3);

        // One row changed, one deleted, one table offline.
        {
            let mut t = source.tables.lock();
            let o = t.get_mut("sales.orders").unwrap();
            o[0].insert("amount".into(), json!(99.0));
            o.retain(|r| r["id"] != json!(5));
        }
        source.broken.lock().insert("sales.customers".into());
        let r = crawl(4);
        assert_eq!(
            r.objects["order"],
            ObjectSync { rows: 1200, upserted: 1, unchanged: 1199, deleted: 1, ..Default::default() }
        );
        assert!(!r.objects.contains_key("customer"));
        assert!(r.notes.iter().any(|n| n.contains("objects customer") && n.contains("offline")), "{:?}", r.notes);
        assert_eq!(get("order:1").props["amount"], json!(99.0));
        assert!(o.get(&admin, "order:5").is_err());
        assert!(o.get(&admin, "customer:1").is_ok(), "a failed read deletes nothing");

        // The listing and purge cover synced objects too.
        source.broken.lock().clear();
        let rows = futures::executor::block_on(fetch_rows(&source, &spec, &o.schema(), &snap));
        assert!(rows.objects.values().all(|r| r.is_ok()));
        let synced = o.store().kv_scan(NS_STATE, "warehouse/").unwrap();
        assert_eq!(synced.len(), 2, "{synced:?}");
    }

    /// A clinic: patients and allergies anyone may read, the join table
    /// between them only doctors, and salaries only HR.
    fn clinic() -> CatalogSnapshot {
        use onto_core::catalog::{ColumnInfo, ForeignKey, RelationInfo, SchemaInfo, SourceInfo};
        let col = |name: &str, data_type: &str| ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
            ..Default::default()
        };
        let pk =
            |name: &str, data_type: &str| ColumnInfo { primary_key: true, nullable: false, ..col(name, data_type) };
        let rel = |name: &str, columns: Vec<ColumnInfo>, readers: &[&str]| RelationInfo {
            schema: "hr".into(),
            name: name.into(),
            columns,
            public_read: readers.is_empty(),
            readers: readers.iter().map(|r| r.to_string()).collect(),
            ..Default::default()
        };
        let fk = |column: &str, to: &str| ForeignKey {
            name: format!("patient_allergies_{column}_fkey"),
            from_schema: "hr".into(),
            from_table: "patient_allergies".into(),
            from_columns: vec![column.into()],
            to_schema: "hr".into(),
            to_table: to.into(),
            to_columns: vec!["id".into()],
        };
        CatalogSnapshot {
            source: SourceInfo { key: "clinic".into(), engine: "postgres".into(), ..Default::default() },
            schemas: vec![SchemaInfo { name: "hr".into(), ..Default::default() }],
            relations: vec![
                rel("patients", vec![pk("id", "bigint"), col("name", "text")], &[]),
                rel("allergies", vec![pk("id", "bigint"), col("label", "text")], &[]),
                rel("patient_allergies", vec![pk("patient_id", "bigint"), pk("allergy_id", "bigint")], &["doctor"]),
                rel(
                    "salaries",
                    vec![
                        pk("email", "text"),
                        ColumnInfo {
                            enum_values: vec!["retired".into(), "resigned".into()],
                            ..col("termination_reason", "hr.reason")
                        },
                    ],
                    &["hr"],
                ),
            ],
            foreign_keys: vec![fk("patient_id", "patients"), fk("allergy_id", "allergies")],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn restricted_rows_stay_out_of_notes_and_restricted_join_tables_out_of_links() {
        let mut schema = Schema::enterprise();
        schema.extend_toml(onto_core::schema::builtin_package("data-catalog").unwrap()).unwrap();
        let o = Ontology::in_memory(schema);
        let snap = clinic();
        let spec: ConnectorSpec = serde_json::from_value(json!({
            "name": "clinic", "url": "env:CLINIC_URL", "workspace": "acme",
            "map_grants": true, "grant_map": { "doctor": "team:doctors", "hr": "team:hr" },
        }))
        .unwrap();
        let admin = Principal::admin("ada");
        ingest_snapshot(&o, &spec, snap.clone(), SyncedRows::default(), 1).unwrap();
        let draft = o.draft_ontology(&admin, &DraftOptions::for_source("clinic")).unwrap();
        let join = draft
            .links
            .iter()
            .find(|l| l.via == onto_core::modeling::LinkVia::JoinTable)
            .unwrap_or_else(|| panic!("{:#?}", draft.links))
            .name
            .clone();
        o.schema_apply(&ActionContext::new(admin.clone(), "acme"), draft.changes.clone()).unwrap();

        let source = FakeSource::default();
        source.tables.lock().insert(
            "hr.patients".into(),
            vec![row(json!({ "id": 1, "name": "Ada" })), row(json!({ "id": 2, "name": "Bo" }))],
        );
        source.tables.lock().insert("hr.allergies".into(), vec![row(json!({ "id": 7, "label": "penicillin" }))]);
        source
            .tables
            .lock()
            .insert("hr.patient_allergies".into(), vec![row(json!({ "patient_id": 1, "allergy_id": 7 }))]);
        source.tables.lock().insert(
            "hr.salaries".into(),
            vec![
                row(json!({ "email": "jane.doe@corp.test", "termination_reason": "misconduct" })),
                row(json!({ "email": "joe@corp.test", "termination_reason": "retired" })),
            ],
        );
        let rows = fetch_rows(&source, &spec, &o.schema(), &snap).await;
        let r = ingest_snapshot(&o, &spec, snap.clone(), rows, 2).unwrap();

        // The rejected row is counted; its key and value are in no note.
        assert_eq!(r.objects["salary"].invalid, 1, "{r:?}");
        let notes = r.notes.join("\n");
        assert!(notes.contains("objects salary: 1 rows do not fit the schema"), "{notes}");
        assert!(!notes.contains("jane") && !notes.contains("misconduct"), "{notes}");
        let listed = serde_json::to_string(&load_state(&o, "clinic").unwrap().last).unwrap();
        assert!(!listed.contains("jane") && !listed.contains("misconduct"), "{listed}");

        // Patients and allergies sync; the doctors-only join table does not.
        assert_eq!(r.objects["patient"].upserted, 2);
        assert!(notes.contains(&format!("links {join}: not synced")), "{notes}");
        assert_eq!(r.links[&join].linked, 0);
        let linked = o.read(|g, _| g.neighbors("patient:1", &join, onto_core::Dir::Out).len());
        assert_eq!(linked, 0, "every reader of both ends would see the relation");

        // Readable like its ends, it syncs.
        let mut open = snap.clone();
        open.relations.iter_mut().find(|r| r.name == "patient_allergies").unwrap().public_read = true;
        let rows = fetch_rows(&source, &spec, &o.schema(), &open).await;
        let r = ingest_snapshot(&o, &spec, open, rows, 3).unwrap();
        assert_eq!(r.links[&join].linked, 1, "{r:?}");
    }

    #[test]
    fn join_tables_restricted_beyond_their_ends() {
        let acl = |a: &[&str]| Some(a.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert!(!join_restricted(None, [None, None]), "a public join table");
        assert!(join_restricted(acl(&["team:doctors"]), [None, None]), "public ends, a restricted join table");
        assert!(!join_restricted(acl(&["team:doctors"]), [acl(&["team:doctors"]), None]), "an end as restricted");
        assert!(join_restricted(acl(&["team:doctors"]), [acl(&["team:clinic"]), None]));
        assert!(join_restricted(acl(&["team:doctors"]), [acl(&[]), None]), "an empty ACL admits nobody");
    }

    #[test]
    fn sync_order_puts_link_targets_first() {
        let s = Schema::from_toml(
            r#"
            [object_types.a]
            backing = { source = "s", table = "x.a", key = ["id"], sync = true, links = { to_b = "b_id" } }
            [object_types.b]
            backing = { source = "s", table = "x.b", key = ["id"], sync = true, links = { to_c = "c_id", to_a = "a_id" } }
            [object_types.c]
            backing = { source = "s", table = "x.c", key = ["id"], sync = true }
            [link_types.to_b]
            from = ["a"]
            to = ["b"]
            [link_types.to_c]
            from = ["b"]
            to = ["c"]
            [link_types.to_a]
            from = ["b"]
            to = ["a"]
            "#,
        )
        .unwrap();
        let all = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(sync_order(&s, &all(&["a", "b", "c"])), all(&["c", "b", "a"]), "a cycle is cut once");
        assert_eq!(sync_order(&s, &all(&["a", "b"])), all(&["b", "a"]));
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn row_reads_quote_names_and_bind_values() {
        use super::postgres::{quote_ident, rows_sql};
        assert_eq!(quote_ident(r#"we"ird"#), r#""we""ird""#);
        let req = |table: &str, after: Option<Vec<Value>>| RowRequest {
            table: table.into(),
            key: vec!["order_id".into(), "sku".into()],
            columns: vec!["qty".into(), "sku".into(), r#"x"; DROP TABLE t; --"#.into()],
            after,
            limit: 10,
        };
        let first = rows_sql(&req("sales.order_items", None)).unwrap();
        assert_eq!(
            first,
            r#"SELECT to_jsonb(t)::text FROM (SELECT "order_id", "sku", "qty", "x""; DROP TABLE t; --" FROM "sales"."order_items" WHERE "order_id" IS NOT NULL AND "sku" IS NOT NULL ORDER BY "order_id", "sku" LIMIT $1) t ORDER BY t."order_id", t."sku""#
        );
        let next = rows_sql(&req("sales.order_items", Some(vec![json!(1), json!("a")]))).unwrap();
        assert!(
            next.contains(r#"AND ("order_id", "sku") > (SELECT r."order_id", r."sku" FROM jsonb_populate_record(NULL::"sales"."order_items", ($2::text)::jsonb) r)"#),
            "{next}"
        );
        assert!(!next.contains("'a'"), "values are bound, never inlined");
        assert!(rows_sql(&req("order_items", None)).is_err());
        assert!(rows_sql(&RowRequest { key: vec![], ..req("s.t", None) }).is_err());
    }

    #[test]
    fn globs_urls_and_names() {
        assert!(glob("onto_*", "onto_acme"));
        assert!(!glob("onto_*", "sales"));
        assert!(glob("*_stage", "orders_stage"));
        assert!(glob("a*c*e", "abcde"));
        assert!(glob("sales", "sales") && !glob("sales", "sales2"));
        assert_eq!(redact_url("postgres://bob:s3cret@db:5432/shop"), "postgres://bob:***@db:5432/shop");
        assert_eq!(redact_url("env:WAREHOUSE_URL"), "env:WAREHOUSE_URL");
        let spec: ConnectorSpec = serde_json::from_value(json!({ "name": "wh", "url": "env:X" })).unwrap();
        assert!(spec.wants_schema("sales") && !spec.wants_schema("onto_default"));
        assert!(
            serde_json::from_value::<ConnectorSpec>(json!({ "name": "Bad Name", "url": "x" }))
                .unwrap()
                .validate()
                .is_err()
        );
    }
}
