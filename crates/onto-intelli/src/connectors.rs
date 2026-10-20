//! Connectors: crawl data sources into the catalog in the background.
//!
//! A connector reads a source's **metadata only**, never its rows: schemas,
//! tables, views, columns, types, constraints, comments, grants and usage
//! statistics. A crawl runs off the request path:
//!
//! 1. the connector fetches a [`CatalogSnapshot`] (async I/O against the
//!    source; no lock on the ontology is held);
//! 2. [`catalog::map_snapshot`] turns it into entities and links with
//!    provenance (`connector:postgres`, the connector name, `crawled_at`);
//! 3. [`catalog::plan_changes`] keeps only what changed since the last crawl
//!    (fingerprints stored per connector), so an idle database costs no
//!    writes;
//! 4. the changes are applied through the audited write path in small
//!    batches, so queries keep being served between them;
//! 5. the learner and OntoRank run over the result.
//!
//! Specs and crawl state live in the store (KV), so every replica sees the
//! same connectors; a lease in the commit log lets only one process crawl a
//! connector at a time.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use onto_core::authority::Weights;
use onto_core::catalog::{self, CatalogSnapshot, Fingerprints, MapOptions};
use onto_core::learn::LearnReport;
use onto_core::time::{new_id, now_ms};
use onto_core::{ActionContext, Error, EventInput, Mutation, Ontology, Principal, Result, Role};

use crate::intelli::Intelli;

/// KV namespaces: connector specs and their crawl state.
pub const NS_SPEC: &str = "connector";
pub const NS_STATE: &str = "connector_state";

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
    #[serde(default)]
    pub notes: Vec<String>,
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
}

pub fn connector_for(kind: &str) -> Result<Box<dyn Connector>> {
    match kind {
        #[cfg(feature = "postgres")]
        "postgres" => Ok(Box::new(postgres::PgConnector)),
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

/// Write a snapshot into the ontology: map, keep what changed, apply in
/// batches, learn, rank. Synchronous; callers run it off async workers.
pub fn ingest_snapshot(
    onto: &Ontology,
    spec: &ConnectorSpec,
    snap: CatalogSnapshot,
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
    let plan = onto.read(|g, _| catalog::plan_changes(mapped, &state.fingerprints, g));
    report.upserted = plan.upserted;
    report.unchanged = plan.unchanged;
    report.deleted = plan.deleted;
    report.links_removed = plan.links_removed;
    let ctx =
        ActionContext::new(connector_principal(&spec.name), ws.clone()).with_source(format!("connector:{}", spec.name));
    // Small batches: the graph lock is held per batch, never for the whole crawl.
    for chunk in plan.mutations.chunks(500) {
        onto.apply(&ctx, chunk.to_vec())?;
    }
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
    fn connector_spec(&self, name: &str) -> Result<ConnectorSpec> {
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
        if purge {
            let ctx = ActionContext::new(p.clone(), ws.clone()).with_source(format!("connector:{name}"));
            let ids: Vec<String> =
                onto.read(|g, _| state.fingerprints.keys().filter(|id| g.contains(id)).cloned().collect());
            deleted = ids.len();
            for chunk in ids.chunks(500) {
                onto.apply(
                    &ctx,
                    chunk.iter().map(|id| Mutation::Delete(onto_core::EntityRef { id: id.clone() })).collect(),
                )?;
            }
        }
        onto.store().kv_delete(NS_SPEC, name)?;
        onto.store().kv_delete(NS_STATE, name)?;
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
            let snap = connector_for(&spec.kind)?.snapshot(spec).await?;
            let (o, s) = (onto.clone(), spec.clone());
            tokio::task::spawn_blocking(move || ingest_snapshot(&o, &s, snap, started))
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
    //! PostgreSQL metadata: `pg_catalog`, `pg_stat_user_tables` and, when the
    //! extension is installed, `pg_stat_statements`. One read-only
    //! transaction with a statement timeout; no table is ever read.

    use std::time::Duration;

    use async_trait::async_trait;
    use onto_core::catalog::{
        CatalogSnapshot, ColumnInfo, ForeignKey, QueryStat, RelKind, RelationInfo, SchemaInfo, SourceInfo, ViewDep,
    };
    use onto_core::sqlx::{self, postgres::PgPoolOptions};
    use onto_core::{Error, Result};

    use super::{Connector, ConnectorSpec};

    pub struct PgConnector;

    type RelRow = (String, String, String, String, String, f32, Option<String>, i64, i64, Option<f64>);
    type ColRow = (String, String, String, String, bool, i32, String, bool);
    type FkRow = (String, String, String, String, String, Vec<String>, Vec<String>);
    /// queryid, text, calls, mean ms, rows.
    type StatementRow = (String, String, i64, f64, i64);

    #[async_trait]
    impl Connector for PgConnector {
        async fn snapshot(&self, spec: &ConnectorSpec) -> Result<CatalogSnapshot> {
            let url = spec.resolve_url()?;
            let parsed =
                sqlx::Url::parse(&url).map_err(|e| Error::Config(format!("connector {}: bad URL: {e}", spec.name)))?;
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(15))
                .connect(&url)
                .await
                .map_err(|e| Error::Unavailable(format!("connector {}: {e}", spec.name)))?;
            let out = read(&pool, spec, &parsed).await;
            pool.close().await;
            out
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
             EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY(i.indkey)) \
             FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
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
            for (schema, table, name, data_type, nullable, position, comment, pk) in cols {
                if let Some(&i) = index.get(&(schema, table)) {
                    relations[i].columns.push(ColumnInfo {
                        name,
                        data_type,
                        nullable,
                        position: i64::from(position),
                        primary_key: pk,
                        comment,
                    });
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
