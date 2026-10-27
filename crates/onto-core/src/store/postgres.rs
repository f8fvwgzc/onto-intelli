//! PostgreSQL store: one PostgreSQL schema per tenant, shared by any number
//! of processes (replicas embedding the library, several onto servers).
//!
//! * Needs PostgreSQL 16 or newer with pgvector 0.7 or newer. Connecting
//!   checks the server version and runs `CREATE EXTENSION IF NOT EXISTS
//!   vector`; opening a tenant applies the versioned SQL migrations embedded
//!   in the binary (`migrations/*.sql`), recorded in `schema_migrations`.
//! * Entity vectors live in a pgvector `halfvec(512)` column with an HNSW
//!   index (cosine distance); BM25 stays in memory, where it ranks better
//!   than `ts_rank`.
//! * Writes commit optimistically against the commit head (`head` row
//!   locked `FOR UPDATE`); a writer that is behind gets `Error::Stale`, the
//!   engine catches up from `commits` and retries.
//! * Every state-changing commit sends `NOTIFY` after it is durable, on the
//!   shared channel `onto_intelli` (and the tenant's own, for older
//!   replicas); followers in other processes catch up at once. One LISTEN
//!   connection per pool serves every tenant ([`notifier`]). A periodic
//!   poll covers notices lost while the listener reconnects.
//! * The engine is synchronous: queries run on a Tokio runtime (an own one,
//!   or the runtime of a pool handed in by the embedding application) and
//!   the caller waits, yielding its worker thread when it is on one.

mod notifier;
mod traces;

pub use notifier::SHARED_CHANNEL;

use std::future::Future;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use crate::sqlx;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, Row};
use tokio::runtime::{Handle, Runtime, RuntimeFlavor};

use super::{
    CommitRecord, EmbedFn, NoticeFn, REPLICATED_KV, Snapshot, Store, StoreNotice, StoreStats, VectorFilter,
    VersionRecord, WriteBatch,
};
use crate::graph::LinkKey;
use crate::model::{Entity, Event, EventFilter, Id, Link};
use crate::time::{new_id, now_ms};
use crate::{Error, Result};

/// Commit records older than this many sequence numbers are pruned; a reader
/// further behind reloads everything.
const KEEP_COMMITS: i64 = 100_000;
/// Rows per multi-row statement.
const CHUNK: usize = 2_000;
/// Oldest PostgreSQL release the store runs on (`server_version_num`).
pub const MIN_SERVER_VERSION: i32 = 160_000;
/// Versioned migrations embedded in the binary, applied in order to every
/// tenant schema. `{schema}` is the tenant schema, `{vector}` the schema of
/// the vector extension. Never edit a released migration; add the next one.
pub const MIGRATIONS: &[(i32, &str, &str)] = &[
    (1, "init", include_str!("../../migrations/0001_init.sql")),
    (2, "vectors", include_str!("../../migrations/0002_vectors.sql")),
    (3, "append_only", include_str!("../../migrations/0003_append_only.sql")),
    (4, "hot_updates", include_str!("../../migrations/0004_hot_updates.sql")),
    (5, "traces", include_str!("../../migrations/0005_traces.sql")),
    (6, "trace_otel", include_str!("../../migrations/0006_trace_otel.sql")),
    (7, "tenant_identity", include_str!("../../migrations/0007_tenant_identity.sql")),
    (8, "audit_no_delete", include_str!("../../migrations/0008_audit_no_delete.sql")),
];
/// Above this many entities without vectors, a backfill drops the HNSW
/// index and builds it once at the end (much faster than row by row).
const REBUILD_INDEX_ABOVE: i64 = 20_000;

/// Where pgvector lives and what it supports.
#[derive(Clone, Debug)]
struct VectorExt {
    /// Quoted schema of the extension (types and operators are qualified with it).
    schema: String,
    /// pgvector 0.8+: filtered HNSW scans continue until enough rows match.
    iterative_scan: bool,
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn parse_version(v: &str) -> (u32, u32, u32) {
    let mut it = v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// Check the server and make sure pgvector is installed (once per pool).
async fn prepare(pool: &PgPool) -> Result<VectorExt> {
    let (num, version): (String, String) =
        sqlx::query_as("SELECT current_setting('server_version_num'), current_setting('server_version')")
            .fetch_one(pool)
            .await?;
    check_server(&num, &version)?;
    let mut tx = pool.begin().await?;
    // Two processes creating the extension at the same time would collide.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('onto-intelli:extensions'))").execute(&mut *tx).await?;
    if let Err(e) = sqlx::query("CREATE EXTENSION IF NOT EXISTS vector").execute(&mut *tx).await {
        return Err(Error::Config(format!(
            "onto-intelli needs the pgvector extension, and CREATE EXTENSION vector failed ({e}). Use a server \
             with pgvector installed (the pgvector/pgvector images have it), or have a superuser run \
             CREATE EXTENSION vector in this database"
        )));
    }
    let (schema, ext_version): (String, String) = sqlx::query_as(
        "SELECT n.nspname::text, e.extversion FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace \
         WHERE e.extname = 'vector'",
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(VectorExt { schema: quote_ident(&schema), iterative_scan: check_pgvector(&ext_version)? })
}

/// Refuse servers older than PostgreSQL 16.
fn check_server(version_num: &str, version: &str) -> Result<()> {
    if version_num.trim().parse::<i32>().unwrap_or(0) < MIN_SERVER_VERSION {
        return Err(Error::Config(format!("onto-intelli needs PostgreSQL 16 or newer; this server runs {version}")));
    }
    Ok(())
}

/// Refuse pgvector before 0.7 (no halfvec); tell whether it has iterative scans (0.8).
fn check_pgvector(version: &str) -> Result<bool> {
    let v = parse_version(version);
    if v < (0, 7, 0) {
        return Err(Error::Config(format!(
            "onto-intelli needs pgvector 0.7 or newer (halfvec); this database has {version}: \
             run ALTER EXTENSION vector UPDATE"
        )));
    }
    Ok(v >= (0, 8, 0))
}

/// pgvector's text form of a quantized vector: `[1,-2,3]`.
fn halfvec_text(v: &[i8]) -> String {
    let mut s = String::with_capacity(v.len() * 4 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

#[derive(Clone, Debug)]
pub struct PgOptions {
    /// The tenant's PostgreSQL schema is `{schema_prefix}{tenant}`.
    pub schema_prefix: String,
    pub max_connections: u32,
    pub connect_timeout: Duration,
    /// Listen for other processes' commits (one LISTEN connection per pool,
    /// shared by every tenant); otherwise followers only poll.
    pub listen: bool,
}

impl Default for PgOptions {
    fn default() -> Self {
        Self {
            schema_prefix: "onto_".into(),
            max_connections: 10,
            connect_timeout: Duration::from_secs(10),
            listen: true,
        }
    }
}

/// A runtime the store started (shut down when the last user goes away).
struct OwnedRuntime(Mutex<Option<Runtime>>);

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.0.lock().take() {
            rt.shutdown_background();
        }
    }
}

#[derive(Clone)]
enum Exec {
    /// A runtime started for the connection pool (connected from a URL).
    Own(Arc<OwnedRuntime>),
    /// The runtime of a pool the application handed in.
    Shared(Handle),
}

impl Exec {
    fn handle(&self) -> (Handle, bool) {
        match self {
            Exec::Own(rt) => (rt.0.lock().as_ref().map(|r| r.handle().clone()).unwrap_or_else(Handle::current), false),
            Exec::Shared(h) => (h.clone(), true),
        }
    }
}

/// A PostgreSQL connection pool plus the runtime its queries run on, shared
/// by the stores of many tenants (`PgShared::tenant`).
#[derive(Clone)]
pub struct PgShared {
    pool: PgPool,
    exec: Exec,
    vector: Arc<VectorExt>,
    notifier: Arc<notifier::Notifier>,
}

impl PgShared {
    /// Connect with a URL; queries run on a small runtime owned by the pool.
    pub fn connect(url: &str, opts: &PgOptions) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("onto-pg")
            .enable_all()
            .build()
            .map_err(|e| Error::Unavailable(format!("starting the postgres runtime: {e}")))?;
        let (url_s, max, timeout) = (url.to_string(), opts.max_connections, opts.connect_timeout);
        let connected = run_on(rt.handle(), false, async move {
            let pool = PgPoolOptions::new().max_connections(max).acquire_timeout(timeout).connect(&url_s).await?;
            let vector = prepare(&pool).await?;
            Ok((pool, vector))
        });
        let (pool, vector) = match connected {
            Ok(c) => c,
            Err(e) => {
                // Dropping a runtime blocks, which panics inside async callers.
                rt.shutdown_background();
                return Err(e);
            }
        };
        Ok(Self {
            notifier: notifier::Notifier::new(pool.clone()),
            pool,
            exec: Exec::Own(Arc::new(OwnedRuntime(Mutex::new(Some(rt))))),
            vector: Arc::new(vector),
        })
    }

    /// Use the application's pool (e.g. its `sqlx::PgPool`). Must be called
    /// inside the Tokio runtime that owns the pool; queries run there.
    pub fn from_pool(pool: PgPool) -> Result<Self> {
        let handle = Handle::try_current().map_err(|_| {
            Error::Config("PgShared::from_pool must be called inside the Tokio runtime that owns the pool".into())
        })?;
        let p = pool.clone();
        let vector = run_on(&handle, true, async move { prepare(&p).await })?;
        Ok(Self {
            notifier: notifier::Notifier::new(pool.clone()),
            pool,
            exec: Exec::Shared(handle),
            vector: Arc::new(vector),
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The store of one tenant (its own PostgreSQL schema), created if missing.
    pub fn tenant(&self, tenant: &str, opts: PgOptions) -> Result<PgStore> {
        PgStore::finish(self.pool.clone(), tenant, opts, self.exec.clone(), self.vector.clone(), self.notifier.clone())
    }

    /// One query (`SELECT 1`): the database answers.
    pub fn ping(&self) -> Result<()> {
        let pool = self.pool.clone();
        let (h, shared) = self.exec.handle();
        run_on(&h, shared, async move {
            sqlx::query("SELECT 1").execute(&pool).await?;
            Ok(())
        })
    }

    /// Stores listening for other processes' commits through this pool's one
    /// LISTEN connection.
    pub fn listening_stores(&self) -> usize {
        self.notifier.subscribers()
    }

    /// Drop a tenant's schema and everything in it (`DROP SCHEMA … CASCADE`).
    /// Stores already open on it fail from then on. A schema another tenant
    /// id owns (a look-alike id) is refused.
    pub fn drop_tenant(&self, prefix: &str, tenant: &str) -> Result<()> {
        let s = quote_ident(&tenant_schema(prefix, tenant)?);
        let pool = self.pool.clone();
        let (h, shared) = self.exec.handle();
        let tenant = tenant.to_string();
        run_on(&h, shared, async move { drop_schema(&pool, &s, &tenant).await })
    }

    /// Tenants with a schema under `prefix`.
    pub fn list_tenants(&self, prefix: &str) -> Result<Vec<String>> {
        let pool = self.pool.clone();
        let prefix = prefix.to_string();
        let (h, shared) = self.exec.handle();
        run_on(&h, shared, async move {
            let names: Vec<String> = sqlx::query_scalar(
                "SELECT n.nspname FROM pg_namespace n JOIN pg_class c ON c.relnamespace = n.oid \
                 WHERE c.relname = 'head' AND starts_with(n.nspname, $1) ORDER BY 1",
            )
            .bind(&prefix)
            .fetch_all(&pool)
            .await?;
            Ok(names.into_iter().map(|n| n[prefix.len()..].to_string()).collect())
        })
    }
}

pub struct PgStore {
    pool: PgPool,
    schema: String,
    tenant: String,
    origin: String,
    exec: Exec,
    vector: Arc<VectorExt>,
    opts: PgOptions,
    subscribers: Arc<Mutex<Vec<NoticeFn>>>,
    notifier: Arc<notifier::Notifier>,
    /// Set once something subscribed; dropping it unsubscribes.
    listener: Mutex<Option<notifier::Subscription>>,
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::Storage(format!("postgres: {e}"))
    }
}

/// Run `fut` on `handle` and wait for it from synchronous code.
fn run_on<T, F>(handle: &Handle, shared: bool, fut: F) -> Result<T>
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel(1);
    handle.spawn(async move {
        let _ = tx.send(fut.await);
    });
    let wait = move || rx.recv().map_err(|_| Error::Unavailable("postgres task was dropped".into()))?;
    match Handle::try_current() {
        Ok(cur) if cur.runtime_flavor() == RuntimeFlavor::MultiThread => tokio::task::block_in_place(wait),
        Ok(_) if shared => Err(Error::Unavailable(
            "a PgStore on a shared pool must be used from a multi-threaded Tokio runtime or spawn_blocking".into(),
        )),
        _ => wait(),
    }
}

/// `onto_` + the tenant, reduced to `[a-z0-9_]` (63 bytes at most, hashed when cut).
pub fn tenant_schema(prefix: &str, tenant: &str) -> Result<String> {
    if tenant.is_empty() {
        return Err(Error::Config("postgres tenant must not be empty".into()));
    }
    let mut name: String = format!("{prefix}{tenant}")
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if !name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') {
        name.insert(0, '_');
    }
    if name.len() > 63 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in tenant.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        name.truncate(46);
        name.push_str(&format!("_{h:016x}"));
    }
    Ok(name)
}

/// `NOTIFY` a change on the shared channel (the payload names the schema)
/// and on the tenant's own channel, which replicas of older versions listen
/// on. Delivered when, and only if, the transaction commits.
async fn send_notice(
    tx: &mut sqlx::transaction::Transaction<'_, sqlx::Postgres>,
    schema: &str,
    mut payload: Value,
) -> Result<()> {
    let own = payload.to_string();
    payload["s"] = Value::String(schema.to_string());
    sqlx::query("SELECT pg_notify($1, $2), pg_notify($3, $4)")
        .bind(schema)
        .bind(own)
        .bind(SHARED_CHANNEL)
        .bind(payload.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn collision(tenant: &str, owner: &str, schema: &str) -> Error {
    Error::Conflict(format!(
        "tenant id '{tenant}' maps to schema {schema}, which belongs to tenant '{owner}' \
         (ids differing only in case or '-', '.', '_' share a schema name): use another id"
    ))
}

/// `DROP SCHEMA … CASCADE`, unless another tenant id owns the schema.
async fn drop_schema(pool: &PgPool, schema: &str, tenant: &str) -> Result<()> {
    let claimed: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(format!("{schema}.tenant_identity"))
        .fetch_one(pool)
        .await?;
    let owner: Option<String> = if claimed {
        sqlx::query_scalar(sql(format!("SELECT tenant FROM {schema}.tenant_identity"))).fetch_optional(pool).await?
    } else {
        None
    };
    if let Some(owner) = owner.filter(|o| o != tenant) {
        return Err(collision(tenant, &owner, schema));
    }
    sqlx::query(sql(format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))).execute(pool).await?;
    Ok(())
}

fn parse<T: serde::de::DeserializeOwned>(doc: &str) -> Result<T> {
    serde_json::from_str(doc).map_err(|e| Error::Storage(format!("corrupt row: {e}")))
}

fn sql(s: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(s)
}

impl PgStore {
    /// Connect with a URL (a pool of its own, on a runtime of its own).
    pub fn connect(url: &str, tenant: &str, opts: PgOptions) -> Result<Self> {
        PgShared::connect(url, &opts)?.tenant(tenant, opts)
    }

    /// Use the application's pool (e.g. its `sqlx::PgPool`). Must be called
    /// inside the pool's Tokio runtime; queries run there.
    pub fn with_pool(pool: PgPool, tenant: &str, opts: PgOptions) -> Result<Self> {
        PgShared::from_pool(pool)?.tenant(tenant, opts)
    }

    fn finish(
        pool: PgPool,
        tenant: &str,
        opts: PgOptions,
        exec: Exec,
        vector: Arc<VectorExt>,
        notifier: Arc<notifier::Notifier>,
    ) -> Result<Self> {
        let store = Self {
            schema: tenant_schema(&opts.schema_prefix, tenant)?,
            tenant: tenant.to_string(),
            origin: new_id("proc"),
            pool,
            exec,
            vector,
            opts,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            notifier,
            listener: Mutex::new(None),
        };
        store.migrate()?;
        Ok(store)
    }

    fn handle(&self) -> (Handle, bool) {
        self.exec.handle()
    }

    fn block<T, F>(&self, fut: F) -> Result<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let (h, shared) = self.handle();
        run_on(&h, shared, fut)
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// The tenant's PostgreSQL schema name.
    pub fn schema_name(&self) -> &str {
        &self.schema
    }

    fn t(&self, table: &str) -> String {
        format!("\"{}\".{table}", self.schema)
    }

    /// Apply the embedded migrations this tenant schema has not had yet, and
    /// claim the schema for this tenant id (or refuse one it does not belong to).
    fn migrate(&self) -> Result<()> {
        let pool = self.pool.clone();
        let s = quote_ident(&self.schema);
        let v = self.vector.schema.clone();
        let lock_key = self.schema.clone();
        let tenant = self.tenant.clone();
        self.block(async move {
            let mut tx = pool.begin().await?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))").bind(&lock_key).execute(&mut *tx).await?;
            sqlx::raw_sql(sql(format!(
                "CREATE SCHEMA IF NOT EXISTS {s}; CREATE TABLE IF NOT EXISTS {s}.schema_migrations \
                 (version INT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)"
            )))
            .execute(&mut *tx)
            .await?;
            let applied: Vec<i32> = sqlx::query_scalar(sql(format!("SELECT version FROM {s}.schema_migrations")))
                .fetch_all(&mut *tx)
                .await?;
            let known = MIGRATIONS.last().map(|m| m.0).unwrap_or(0);
            if let Some(&newest) = applied.iter().max()
                && newest > known
            {
                return Err(Error::Config(format!(
                    "tenant schema {s} is at migration {newest}, newer than this build (up to {known}): upgrade onto-intelli"
                )));
            }
            for (version, name, body) in MIGRATIONS {
                if applied.contains(version) {
                    continue;
                }
                let body = body.replace("{schema}", &s).replace("{vector}", &v);
                sqlx::raw_sql(sql(body))
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| Error::Storage(format!("migration {version:04}_{name} on {s}: {e}")))?;
                sqlx::query(sql(format!(
                    "INSERT INTO {s}.schema_migrations (version, name, applied_at) VALUES ($1, $2, $3)"
                )))
                .bind(version)
                .bind(name)
                .bind(now_ms())
                .execute(&mut *tx)
                .await?;
            }
            let owner: String = sqlx::query_scalar(sql(format!(
                "INSERT INTO {s}.tenant_identity (tenant, claimed_at) VALUES ($1, $2) \
                 ON CONFLICT (one) DO UPDATE SET tenant = {s}.tenant_identity.tenant RETURNING tenant"
            )))
            .bind(&tenant)
            .bind(now_ms())
            .fetch_one(&mut *tx)
            .await?;
            if owner != tenant {
                return Err(collision(&tenant, &owner, &s));
            }
            tx.commit().await?;
            Ok(())
        })
    }

    /// Migrations applied to this tenant, oldest first: (version, name, applied_at ms).
    pub fn migrations(&self) -> Result<Vec<(i32, String, i64)>> {
        let pool = self.pool.clone();
        let t = self.t("schema_migrations");
        self.block(async move {
            Ok(sqlx::query_as(sql(format!("SELECT version, name, applied_at FROM {t} ORDER BY version")))
                .fetch_all(&pool)
                .await?)
        })
    }

    /// Drop the tenant's schema and everything in it (tests, offboarding).
    pub fn drop_tenant(&self) -> Result<()> {
        let pool = self.pool.clone();
        let s = quote_ident(&self.schema);
        let tenant = self.tenant.clone();
        self.block(async move { drop_schema(&pool, &s, &tenant).await })
    }

    /// Delete expired KV rows and events older than `keep_events_ms`.
    pub fn vacuum(&self, keep_events_ms: Option<i64>) -> Result<(u64, u64)> {
        let pool = self.pool.clone();
        let (kv, ev) = (self.t("kv"), self.t("events"));
        self.block(async move {
            let now = now_ms();
            let a = sqlx::query(sql(format!("DELETE FROM {kv} WHERE expires_at IS NOT NULL AND expires_at <= $1")))
                .bind(now)
                .execute(&pool)
                .await?
                .rows_affected();
            let b = match keep_events_ms {
                Some(keep) => {
                    // The events table refuses DELETE except to retention (0008).
                    let mut tx = pool.begin().await?;
                    sqlx::query("SELECT set_config('onto.retention', 'on', true)").execute(&mut *tx).await?;
                    let n = sqlx::query(sql(format!("DELETE FROM {ev} WHERE ts < $1")))
                        .bind(now - keep)
                        .execute(&mut *tx)
                        .await?
                        .rows_affected();
                    tx.commit().await?;
                    n
                }
                None => 0,
            };
            Ok((a, b))
        })
    }

    fn channel(&self) -> String {
        self.schema.clone()
    }

    fn start_listener(&self) {
        let mut listener = self.listener.lock();
        if !self.opts.listen || listener.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel::<StoreNotice>();
        // Callbacks run on their own thread so they may call back into the store.
        let subs = self.subscribers.clone();
        let _ = std::thread::Builder::new().name("onto-pg-notify".into()).spawn(move || {
            while let Ok(n) = rx.recv() {
                let list: Vec<NoticeFn> = subs.lock().clone();
                for f in list {
                    f(n.clone());
                }
            }
        });
        let (h, _) = self.handle();
        *listener = Some(self.notifier.subscribe(&h, &self.channel(), &self.origin, tx));
    }

    /// Bulk-load through COPY (fast imports and benchmarks): no events and no
    /// versions; one commit tells other processes to reload. Vectors are
    /// computed when the tenant is next opened ([`Store::backfill_vectors`]).
    fn copy_import(&self, entities: Vec<Entity>, links: Vec<Link>) -> Result<u64> {
        fn csv(field: &str) -> String {
            format!("\"{}\"", field.replace('"', "\"\""))
        }
        let pool = self.pool.clone();
        let (te, tl, th, tc) = (self.t("entities"), self.t("links"), self.t("head"), self.t("commits"));
        let origin = self.origin.clone();
        let channel = self.channel();
        self.block(async move {
            let mut tx = pool.begin().await?;
            let head: i64 = sqlx::query_scalar(sql(format!("SELECT seq FROM {th} WHERE id = 1 FOR UPDATE")))
                .fetch_one(&mut *tx)
                .await?;
            sqlx::query("CREATE TEMP TABLE onto_import_e (id TEXT, kind TEXT, workspace TEXT, doc JSONB, updated_at BIGINT) ON COMMIT DROP")
                .execute(&mut *tx)
                .await?;
            sqlx::query("CREATE TEMP TABLE onto_import_l (src TEXT, rel TEXT, dst TEXT, doc JSONB) ON COMMIT DROP")
                .execute(&mut *tx)
                .await?;
            {
                let mut copy = tx
                    .copy_in_raw("COPY onto_import_e (id, kind, workspace, doc, updated_at) FROM STDIN WITH (FORMAT csv)")
                    .await?;
                let mut buf = String::with_capacity(1 << 20);
                for e in &entities {
                    let doc = serde_json::to_string(e)?;
                    buf.push_str(&format!(
                        "{},{},{},{},{}\n",
                        csv(&e.id),
                        csv(&e.kind),
                        csv(&e.workspace),
                        csv(&doc),
                        e.updated_at
                    ));
                    if buf.len() > (1 << 20) {
                        copy.send(std::mem::take(&mut buf).into_bytes()).await?;
                    }
                }
                if !buf.is_empty() {
                    copy.send(buf.into_bytes()).await?;
                }
                copy.finish().await?;
            }
            {
                let mut copy =
                    tx.copy_in_raw("COPY onto_import_l (src, rel, dst, doc) FROM STDIN WITH (FORMAT csv)").await?;
                let mut buf = String::with_capacity(1 << 20);
                for l in &links {
                    let doc = serde_json::to_string(l)?;
                    buf.push_str(&format!("{},{},{},{}\n", csv(&l.from), csv(&l.rel), csv(&l.to), csv(&doc)));
                    if buf.len() > (1 << 20) {
                        copy.send(std::mem::take(&mut buf).into_bytes()).await?;
                    }
                }
                if !buf.is_empty() {
                    copy.send(buf.into_bytes()).await?;
                }
                copy.finish().await?;
            }
            sqlx::query(sql(format!(
                "INSERT INTO {te} (id, kind, workspace, doc, updated_at) SELECT DISTINCT ON (id) id, kind, workspace, doc, updated_at \
                 FROM onto_import_e ORDER BY id ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, workspace = excluded.workspace, \
                 doc = excluded.doc, updated_at = excluded.updated_at, embedding = NULL, embedded = false"
            )))
            .execute(&mut *tx)
            .await?;
            sqlx::query(sql(format!(
                "INSERT INTO {tl} (src, rel, dst, doc) SELECT DISTINCT ON (src, rel, dst) src, rel, dst, doc FROM onto_import_l \
                 ORDER BY src, rel, dst ON CONFLICT (src, rel, dst) DO UPDATE SET doc = excluded.doc"
            )))
            .execute(&mut *tx)
            .await?;
            let seq = head + 1;
            let rec = CommitRecord { seq: seq as u64, ts: now_ms(), origin: origin.clone(), reload: true, ..Default::default() };
            sqlx::query(sql(format!("INSERT INTO {tc} (seq, ts, origin, doc) VALUES ($1, $2, $3, $4::jsonb)")))
                .bind(seq)
                .bind(rec.ts)
                .bind(&origin)
                .bind(serde_json::to_string(&rec)?)
                .execute(&mut *tx)
                .await?;
            sqlx::query(sql(format!("UPDATE {th} SET seq = $1 WHERE id = 1"))).bind(seq).execute(&mut *tx).await?;
            send_notice(&mut tx, &channel, json!({ "t": "c", "seq": seq, "o": origin })).await?;
            tx.commit().await?;
            Ok(seq as u64)
        })
    }
}

impl Store for PgStore {
    fn load(&self) -> Result<Snapshot> {
        let pool = self.pool.clone();
        let (te, tl, th) = (self.t("entities"), self.t("links"), self.t("head"));
        self.block(async move {
            use futures::TryStreamExt;
            let mut tx = pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").execute(&mut *tx).await?;
            let seq: i64 =
                sqlx::query_scalar(sql(format!("SELECT seq FROM {th} WHERE id = 1"))).fetch_one(&mut *tx).await?;
            let mut snap = Snapshot { seq: seq as u64, ..Default::default() };
            {
                let q = format!("SELECT doc::text FROM {te}");
                let mut rows = sqlx::query_scalar::<_, String>(sql(q)).fetch(&mut *tx);
                while let Some(doc) = rows.try_next().await? {
                    snap.entities.push(parse(&doc)?);
                }
            }
            {
                let q = format!("SELECT doc::text FROM {tl}");
                let mut rows = sqlx::query_scalar::<_, String>(sql(q)).fetch(&mut *tx);
                while let Some(doc) = rows.try_next().await? {
                    snap.links.push(parse(&doc)?);
                }
            }
            tx.commit().await?;
            Ok(snap)
        })
    }

    fn tracks_commits(&self) -> bool {
        true
    }

    fn shared(&self) -> bool {
        true
    }

    fn origin(&self) -> String {
        self.origin.clone()
    }

    fn head(&self) -> Result<u64> {
        let pool = self.pool.clone();
        let th = self.t("head");
        self.block(async move {
            let seq: i64 =
                sqlx::query_scalar(sql(format!("SELECT seq FROM {th} WHERE id = 1"))).fetch_one(&pool).await?;
            Ok(seq as u64)
        })
    }

    fn commits_since(&self, seq: u64, limit: usize) -> Result<Vec<CommitRecord>> {
        let pool = self.pool.clone();
        let tc = self.t("commits");
        self.block(async move {
            let rows: Vec<String> =
                sqlx::query_scalar(sql(format!("SELECT doc::text FROM {tc} WHERE seq > $1 ORDER BY seq LIMIT $2")))
                    .bind(seq as i64)
                    .bind(limit.min(100_000) as i64)
                    .fetch_all(&pool)
                    .await?;
            rows.iter().map(|d| parse(d)).collect()
        })
    }

    fn get_entities(&self, ids: &[Id]) -> Result<Vec<Entity>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool.clone();
        let te = self.t("entities");
        let ids = ids.to_vec();
        self.block(async move {
            let mut out = Vec::with_capacity(ids.len());
            for chunk in ids.chunks(CHUNK) {
                let rows: Vec<String> =
                    sqlx::query_scalar(sql(format!("SELECT doc::text FROM {te} WHERE id = ANY($1)")))
                        .bind(chunk.to_vec())
                        .fetch_all(&pool)
                        .await?;
                for d in rows {
                    out.push(parse(&d)?);
                }
            }
            Ok(out)
        })
    }

    fn get_links(&self, keys: &[LinkKey]) -> Result<Vec<Link>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool.clone();
        let tl = self.t("links");
        let keys = keys.to_vec();
        self.block(async move {
            let mut out = Vec::with_capacity(keys.len());
            for chunk in keys.chunks(CHUNK) {
                let (s, r, d): (Vec<String>, Vec<String>, Vec<String>) = chunk.iter().cloned().fold(
                    (Vec::new(), Vec::new(), Vec::new()),
                    |(mut s, mut r, mut d), (a, b, c)| {
                        s.push(a);
                        r.push(b);
                        d.push(c);
                        (s, r, d)
                    },
                );
                let rows: Vec<String> = sqlx::query_scalar(sql(format!(
                    "SELECT l.doc::text FROM {tl} l JOIN UNNEST($1::text[], $2::text[], $3::text[]) AS k(src, rel, dst) \
                     ON l.src = k.src AND l.rel = k.rel AND l.dst = k.dst"
                )))
                .bind(s)
                .bind(r)
                .bind(d)
                .fetch_all(&pool)
                .await?;
                for doc in rows {
                    out.push(parse(&doc)?);
                }
            }
            Ok(out)
        })
    }

    fn subscribe(&self, f: NoticeFn) -> Result<()> {
        self.subscribers.lock().push(f);
        self.start_listener();
        Ok(())
    }

    fn write(&self, b: &WriteBatch) -> Result<u64> {
        let pool = self.pool.clone();
        let tables = Tables::of(self);
        let origin = self.origin.clone();
        let channel = self.channel();
        let bumps = b.changes_state();
        let rec = b.commit_record(0, &origin);
        let expect = b.expect_seq;
        // Serialize documents on the caller's thread.
        let keep: std::collections::HashSet<&str> = b.keep_vectors.iter().map(String::as_str).collect();
        let put_e: Vec<EntityRow> = b
            .put_entities
            .iter()
            .map(|e| {
                Ok(EntityRow {
                    id: e.id.clone(),
                    kind: e.kind.clone(),
                    workspace: e.workspace.clone(),
                    doc: serde_json::to_string(e)?,
                    updated_at: e.updated_at,
                    keep_vector: keep.contains(e.id.as_str()),
                })
            })
            .collect::<Result<_>>()?;
        let reembed = b.reembed.clone();
        let put_l: Vec<(String, String, String, String)> = b
            .put_links
            .iter()
            .map(|l| Ok((l.from.clone(), l.rel.clone(), l.to.clone(), serde_json::to_string(l)?)))
            .collect::<Result<_>>()?;
        let events: Vec<EventRow> = b
            .events
            .iter()
            .map(|e| {
                Ok((
                    e.id.clone(),
                    e.ts,
                    e.workspace.clone(),
                    e.actor.clone(),
                    e.action.clone(),
                    e.target.clone(),
                    serde_json::to_string(e)?,
                ))
            })
            .collect::<Result<_>>()?;
        let versions: Vec<(String, i64, i64, String)> = b
            .versions
            .iter()
            .map(|v| Ok((v.id.clone(), v.revision as i64, v.ts, serde_json::to_string(v)?)))
            .collect::<Result<_>>()?;
        let link_versions: Vec<(String, String, String, i64, String)> = b
            .link_versions
            .iter()
            .map(|v| Ok((v.from.clone(), v.rel.clone(), v.to.clone(), v.ts, serde_json::to_string(v)?)))
            .collect::<Result<_>>()?;
        let kv: Vec<(String, String, String)> =
            b.kv.iter()
                .map(|(n, k, v)| Ok((n.clone(), k.clone(), serde_json::to_string(v)?)))
                .collect::<Result<_>>()?;
        let del_e = b.del_entities.clone();
        let del_l = b.del_links.clone();
        self.block(async move {
            let mut tx = pool.begin().await?;
            let mut seq: i64 = 0;
            if bumps {
                seq = sqlx::query_scalar(sql(format!("SELECT seq FROM {} WHERE id = 1 FOR UPDATE", tables.head)))
                    .fetch_one(&mut *tx)
                    .await?;
                if let Some(expected) = expect
                    && expected as i64 != seq
                {
                    return Err(Error::Stale(format!("store is at commit {seq}, batch was staged at {expected}")));
                }
            }
            for chunk in del_l.chunks(CHUNK) {
                let (s, r, d) = split3(chunk);
                sqlx::query(sql(format!(
                    "DELETE FROM {} l USING UNNEST($1::text[], $2::text[], $3::text[]) AS k(src, rel, dst) \
                     WHERE l.src = k.src AND l.rel = k.rel AND l.dst = k.dst",
                    tables.links
                )))
                .bind(s)
                .bind(r)
                .bind(d)
                .execute(&mut *tx)
                .await?;
            }
            for chunk in del_e.chunks(CHUNK) {
                let ids = chunk.to_vec();
                sqlx::query(sql(format!("DELETE FROM {} WHERE id = ANY($1)", tables.entities)))
                    .bind(&ids)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(sql(format!("DELETE FROM {} WHERE src = ANY($1) OR dst = ANY($1)", tables.links)))
                    .bind(&ids)
                    .execute(&mut *tx)
                    .await?;
            }
            // Entities whose searchable text changed are marked for the
            // vectorizer; the others keep their vector, so an update that
            // touches no indexed column can stay a HOT update.
            for keep_vector in [false, true] {
                let rows: Vec<&EntityRow> = put_e.iter().filter(|r| r.keep_vector == keep_vector).collect();
                for chunk in rows.chunks(CHUNK) {
                    let mut c: EntityColumns = Default::default();
                    for r in chunk {
                        c.0.push(r.id.clone());
                        c.1.push(r.kind.clone());
                        c.2.push(r.workspace.clone());
                        c.3.push(r.doc.clone());
                        c.4.push(r.updated_at);
                    }
                    sqlx::query(sql(format!(
                        "INSERT INTO {} (id, kind, workspace, doc, updated_at) \
                         SELECT id, kind, ws, doc::jsonb, at FROM \
                         UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::bigint[]) AS r(id, kind, ws, doc, at) \
                         ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, workspace = excluded.workspace, \
                         doc = excluded.doc, updated_at = excluded.updated_at{}",
                        tables.entities,
                        if keep_vector { "" } else { ", embedded = false" }
                    )))
                    .bind(c.0)
                    .bind(c.1)
                    .bind(c.2)
                    .bind(c.3)
                    .bind(c.4)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            for chunk in reembed.chunks(CHUNK) {
                sqlx::query(sql(format!("UPDATE {} SET embedded = false WHERE id = ANY($1) AND embedded", tables.entities)))
                    .bind(chunk.to_vec())
                    .execute(&mut *tx)
                    .await?;
            }
            for chunk in put_l.chunks(CHUNK) {
                let mut cols: (Vec<String>, Vec<String>, Vec<String>, Vec<String>) = Default::default();
                for (s, r, d, doc) in chunk.iter().cloned() {
                    cols.0.push(s);
                    cols.1.push(r);
                    cols.2.push(d);
                    cols.3.push(doc);
                }
                sqlx::query(sql(format!(
                    "INSERT INTO {} (src, rel, dst, doc) \
                     SELECT s, r, d, doc::jsonb FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[]) AS x(s, r, d, doc) \
                     ON CONFLICT (src, rel, dst) DO UPDATE SET doc = excluded.doc",
                    tables.links
                )))
                .bind(cols.0)
                .bind(cols.1)
                .bind(cols.2)
                .bind(cols.3)
                .execute(&mut *tx)
                .await?;
            }
            for chunk in events.chunks(CHUNK) {
                let mut ids = Vec::new();
                let mut tss = Vec::new();
                let mut wss = Vec::new();
                let mut actors = Vec::new();
                let mut actions = Vec::new();
                let mut targets: Vec<Option<String>> = Vec::new();
                let mut docs = Vec::new();
                for (id, ts, ws, actor, action, target, doc) in chunk.iter().cloned() {
                    ids.push(id);
                    tss.push(ts);
                    wss.push(ws);
                    actors.push(actor);
                    actions.push(action);
                    targets.push(target);
                    docs.push(doc);
                }
                sqlx::query(sql(format!(
                    "INSERT INTO {} (id, ts, workspace, actor, action, target, doc) \
                     SELECT id, ts, ws, actor, action, target, doc::jsonb FROM \
                     UNNEST($1::text[], $2::bigint[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[]) \
                     AS e(id, ts, ws, actor, action, target, doc) ON CONFLICT (id) DO NOTHING",
                    tables.events
                )))
                .bind(ids)
                .bind(tss)
                .bind(wss)
                .bind(actors)
                .bind(actions)
                .bind(targets)
                .bind(docs)
                .execute(&mut *tx)
                .await?;
            }
            for chunk in versions.chunks(CHUNK) {
                let mut c: (Vec<String>, Vec<i64>, Vec<i64>, Vec<String>) = Default::default();
                for (id, rev, ts, doc) in chunk.iter().cloned() {
                    c.0.push(id);
                    c.1.push(rev);
                    c.2.push(ts);
                    c.3.push(doc);
                }
                sqlx::query(sql(format!(
                    "INSERT INTO {} (id, revision, ts, doc) \
                     SELECT id, rev, ts, doc::jsonb FROM UNNEST($1::text[], $2::bigint[], $3::bigint[], $4::text[]) \
                     WITH ORDINALITY AS v(id, rev, ts, doc, n) ORDER BY n",
                    tables.versions
                )))
                .bind(c.0)
                .bind(c.1)
                .bind(c.2)
                .bind(c.3)
                .execute(&mut *tx)
                .await?;
            }
            for chunk in link_versions.chunks(CHUNK) {
                let (mut src, mut rel, mut dst, mut ts, mut doc) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for (a, b, c, t, d) in chunk.iter().cloned() {
                    src.push(a);
                    rel.push(b);
                    dst.push(c);
                    ts.push(t);
                    doc.push(d);
                }
                sqlx::query(sql(format!(
                    "INSERT INTO {} (src, rel, dst, ts, doc) \
                     SELECT s, r, d, t, doc::jsonb FROM UNNEST($1::text[], $2::text[], $3::text[], $4::bigint[], $5::text[]) \
                     WITH ORDINALITY AS v(s, r, d, t, doc, n) ORDER BY n",
                    tables.link_versions
                )))
                .bind(src)
                .bind(rel)
                .bind(dst)
                .bind(ts)
                .bind(doc)
                .execute(&mut *tx)
                .await?;
            }
            for chunk in kv.chunks(CHUNK) {
                let mut c: (Vec<String>, Vec<String>, Vec<String>) = Default::default();
                for (n, k, v) in chunk.iter().cloned() {
                    c.0.push(n);
                    c.1.push(k);
                    c.2.push(v);
                }
                sqlx::query(sql(format!(
                    "INSERT INTO {} (ns, key, value, expires_at) \
                     SELECT ns, key, value::jsonb, NULL FROM UNNEST($1::text[], $2::text[], $3::text[]) AS k(ns, key, value) \
                     ON CONFLICT (ns, key) DO UPDATE SET value = excluded.value, expires_at = NULL",
                    tables.kv
                )))
                .bind(c.0)
                .bind(c.1)
                .bind(c.2)
                .execute(&mut *tx)
                .await?;
            }
            if bumps {
                seq += 1;
                let mut rec = rec;
                rec.seq = seq as u64;
                sqlx::query(sql(format!("INSERT INTO {} (seq, ts, origin, doc) VALUES ($1, $2, $3, $4::jsonb)", tables.commits)))
                    .bind(seq)
                    .bind(rec.ts)
                    .bind(&rec.origin)
                    .bind(serde_json::to_string(&rec)?)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(sql(format!("UPDATE {} SET seq = $1 WHERE id = 1", tables.head)))
                    .bind(seq)
                    .execute(&mut *tx)
                    .await?;
                if seq % 1000 == 0 {
                    sqlx::query(sql(format!("DELETE FROM {} WHERE seq < $1", tables.commits)))
                        .bind(seq - KEEP_COMMITS)
                        .execute(&mut *tx)
                        .await?;
                }
                // Delivered to listeners when (and only if) the transaction commits.
                send_notice(&mut tx, &channel, json!({ "t": "c", "seq": seq, "o": rec.origin })).await?;
            }
            tx.commit().await?;
            Ok(seq as u64)
        })
    }

    fn events(&self, f: &EventFilter) -> Result<Vec<Event>> {
        let pool = self.pool.clone();
        let te = self.t("events");
        let f = f.clone();
        self.block(async move {
            let mut where_ = String::from("TRUE");
            let mut args: Vec<Value> = Vec::new();
            let mut push = |clause: &str, v: Value| {
                args.push(v);
                where_.push_str(&format!(" AND {clause} ${}", args.len()));
            };
            if let Some(w) = &f.workspace {
                push("workspace =", json!(w));
            }
            if let Some(a) = &f.actor {
                push("actor =", json!(a));
            }
            if let Some(t) = &f.target {
                push("target =", json!(t));
            }
            if let Some(s) = f.since {
                push("ts >=", json!(s));
            }
            if let Some(u) = f.until {
                push("ts <=", json!(u));
            }
            if let Some(a) = &f.action {
                let prefix = a.trim_end_matches('.').to_string();
                args.push(json!(prefix));
                let n = args.len();
                where_.push_str(&format!(" AND (action = ${n} OR starts_with(action, ${n} || '.'))"));
            }
            let limit = f.limit.unwrap_or(50).min(10_000);
            let q = format!("SELECT doc::text FROM {te} WHERE {where_} ORDER BY ts DESC, id DESC LIMIT {limit}");
            let mut query = sqlx::query_scalar::<_, String>(sql(q));
            for a in args {
                query = match a {
                    Value::Number(n) => query.bind(n.as_i64().unwrap_or_default()),
                    Value::String(s) => query.bind(s),
                    other => query.bind(other.to_string()),
                };
            }
            let rows = query.fetch_all(&pool).await?;
            rows.iter().map(|d| parse(d)).collect()
        })
    }

    fn kv_put(&self, ns: &str, key: &str, value: &Value, expires_at: Option<i64>) -> Result<()> {
        let pool = self.pool.clone();
        let tk = self.t("kv");
        let (ns, key, value) = (ns.to_string(), key.to_string(), serde_json::to_string(value)?);
        let notify = REPLICATED_KV.contains(&ns.as_str()).then(|| (self.channel(), self.origin.clone()));
        self.block(async move {
            let mut tx = pool.begin().await?;
            sqlx::query(sql(format!(
                "INSERT INTO {tk} (ns, key, value, expires_at) VALUES ($1, $2, $3::jsonb, $4) \
                 ON CONFLICT (ns, key) DO UPDATE SET value = excluded.value, expires_at = excluded.expires_at"
            )))
            .bind(&ns)
            .bind(&key)
            .bind(&value)
            .bind(expires_at)
            .execute(&mut *tx)
            .await?;
            if let Some((channel, origin)) = notify {
                send_notice(&mut tx, &channel, json!({ "t": "kv", "ns": ns, "key": key, "o": origin })).await?;
            }
            tx.commit().await?;
            Ok(())
        })
    }

    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<Value>> {
        let pool = self.pool.clone();
        let tk = self.t("kv");
        let (ns, key) = (ns.to_string(), key.to_string());
        self.block(async move {
            let row = sqlx::query(sql(format!("SELECT value::text, expires_at FROM {tk} WHERE ns = $1 AND key = $2")))
                .bind(&ns)
                .bind(&key)
                .fetch_optional(&pool)
                .await?;
            let Some(row) = row else { return Ok(None) };
            let value: String = row.try_get(0)?;
            let exp: Option<i64> = row.try_get(1)?;
            if exp.is_some_and(|e| e <= now_ms()) {
                sqlx::query(sql(format!("DELETE FROM {tk} WHERE ns = $1 AND key = $2")))
                    .bind(&ns)
                    .bind(&key)
                    .execute(&pool)
                    .await?;
                return Ok(None);
            }
            Ok(Some(parse(&value)?))
        })
    }

    fn kv_delete(&self, ns: &str, key: &str) -> Result<()> {
        let pool = self.pool.clone();
        let tk = self.t("kv");
        let (ns, key) = (ns.to_string(), key.to_string());
        let notify = REPLICATED_KV.contains(&ns.as_str()).then(|| (self.channel(), self.origin.clone()));
        self.block(async move {
            let mut tx = pool.begin().await?;
            sqlx::query(sql(format!("DELETE FROM {tk} WHERE ns = $1 AND key = $2")))
                .bind(&ns)
                .bind(&key)
                .execute(&mut *tx)
                .await?;
            if let Some((channel, origin)) = notify {
                send_notice(&mut tx, &channel, json!({ "t": "kv", "ns": ns, "key": key, "o": origin })).await?;
            }
            tx.commit().await?;
            Ok(())
        })
    }

    fn kv_scan(&self, ns: &str, prefix: &str) -> Result<Vec<(String, Value)>> {
        let pool = self.pool.clone();
        let tk = self.t("kv");
        let (ns, prefix) = (ns.to_string(), prefix.to_string());
        self.block(async move {
            let rows = sqlx::query(sql(format!(
                "SELECT key, value::text FROM {tk} WHERE ns = $1 AND starts_with(key, $2) \
                 AND (expires_at IS NULL OR expires_at > $3) ORDER BY key"
            )))
            .bind(&ns)
            .bind(&prefix)
            .bind(now_ms())
            .fetch_all(&pool)
            .await?;
            rows.into_iter()
                .map(|r| {
                    let k: String = r.try_get(0)?;
                    let v: String = r.try_get(1)?;
                    Ok((k, parse(&v)?))
                })
                .collect()
        })
    }

    fn stats(&self) -> Result<StoreStats> {
        let pool = self.pool.clone();
        let (te, tk, te_ent) = (self.t("events"), self.t("kv"), self.t("entities"));
        let schema = self.schema.clone();
        self.block(async move {
            // Event counts are estimates: exact counts of a large audit log are slow.
            let events: Option<f32> = sqlx::query_scalar(
                "SELECT c.reltuples FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = 'events'",
            )
            .bind(&schema)
            .fetch_optional(&pool)
            .await?;
            let events = match events {
                Some(n) if n >= 0.0 => n as u64,
                _ => {
                    sqlx::query_scalar::<_, i64>(sql(format!("SELECT count(*) FROM {te}"))).fetch_one(&pool).await?
                        as u64
                }
            };
            let kv: i64 = sqlx::query_scalar(sql(format!("SELECT count(*) FROM {tk}"))).fetch_one(&pool).await?;
            // The HNSW index holds exactly the rows with a vector; its
            // estimate is only kept fresh by (auto)vacuum, so small tables
            // are counted.
            let indexed: Option<f32> = sqlx::query_scalar(
                "SELECT c.reltuples FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = 'entities_embedding'",
            )
            .bind(&schema)
            .fetch_optional(&pool)
            .await?;
            let vectors = match indexed {
                Some(n) if n >= 100_000.0 => n as u64,
                _ => {
                    sqlx::query_scalar::<_, i64>(sql(format!(
                        "SELECT count(*) FROM {te_ent} WHERE embedding IS NOT NULL"
                    )))
                    .fetch_one(&pool)
                    .await? as u64
                }
            };
            Ok(StoreStats {
                backend: "postgres".into(),
                events,
                kv_entries: kv as u64,
                path: Some(schema),
                vectors: Some(vectors),
            })
        })
    }

    fn versions(&self, id: &str, limit: usize) -> Result<Vec<VersionRecord>> {
        let pool = self.pool.clone();
        let tv = self.t("entity_versions");
        let id = id.to_string();
        self.block(async move {
            let rows: Vec<String> =
                sqlx::query_scalar(sql(format!("SELECT doc::text FROM {tv} WHERE id = $1 ORDER BY seq DESC LIMIT $2")))
                    .bind(&id)
                    .bind(limit.min(100_000) as i64)
                    .fetch_all(&pool)
                    .await?;
            rows.iter().map(|d| parse(d)).collect()
        })
    }

    fn versions_since(&self, since: i64, limit: usize) -> Result<Vec<VersionRecord>> {
        let pool = self.pool.clone();
        let tv = self.t("entity_versions");
        self.block(async move {
            let rows: Vec<String> = sqlx::query_scalar(sql(format!(
                "SELECT doc::text FROM {tv} WHERE ts >= $1 ORDER BY seq DESC LIMIT $2"
            )))
            .bind(since)
            .bind(limit.min(100_000) as i64)
            .fetch_all(&pool)
            .await?;
            rows.iter().map(|d| parse(d)).collect()
        })
    }

    fn link_history(&self, id: &str, until: i64, limit: usize) -> Result<Vec<super::LinkVersion>> {
        let pool = self.pool.clone();
        let tl = self.t("link_versions");
        let id = id.to_string();
        self.block(async move {
            let rows: Vec<String> = sqlx::query_scalar(sql(format!(
                "SELECT doc::text FROM {tl} WHERE (src = $1 OR dst = $1) AND ts <= $2 ORDER BY seq DESC LIMIT $3"
            )))
            .bind(&id)
            .bind(until)
            .bind(limit.min(100_000) as i64)
            .fetch_all(&pool)
            .await?;
            rows.iter().map(|d| parse(d)).collect()
        })
    }

    fn bulk_import(&self, entities: &[Entity], links: &[Link]) -> Result<u64> {
        self.copy_import(entities.to_vec(), links.to_vec())
    }

    fn indexes_vectors(&self) -> bool {
        true
    }

    fn vectors_pending(&self) -> Result<bool> {
        let pool = self.pool.clone();
        let te = self.t("entities");
        self.block(async move {
            Ok(sqlx::query_scalar(sql(format!("SELECT EXISTS (SELECT 1 FROM {te} WHERE NOT embedded)")))
                .fetch_one(&pool)
                .await?)
        })
    }

    fn vector_search(&self, query: &[i8], limit: usize, filter: &VectorFilter) -> Result<Vec<(Id, f32)>> {
        if limit == 0 || query.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool.clone();
        let te = self.t("entities");
        let v = self.vector.schema.clone();
        let iterative = self.vector.iterative_scan;
        let q = halfvec_text(query);
        let filter = filter.clone();
        // ef_search bounds what one HNSW scan returns (pgvector caps it at 1000).
        let limit = limit.min(1_000);
        self.block(async move {
            let mut tx = pool.begin().await?;
            sqlx::query("SELECT set_config('hnsw.ef_search', $1, true)")
                .bind(limit.max(40).to_string())
                .execute(&mut *tx)
                .await?;
            if iterative {
                // Keep scanning when the workspace/kind filter rejects candidates.
                sqlx::query("SELECT set_config('hnsw.iterative_scan', 'relaxed_order', true)")
                    .execute(&mut *tx)
                    .await?;
            }
            let mut where_ = String::from("embedding IS NOT NULL");
            let mut n = 2;
            if filter.workspace.is_some() {
                n += 1;
                where_.push_str(&format!(" AND workspace = ${n}"));
            }
            if !filter.kinds.is_empty() {
                n += 1;
                where_.push_str(&format!(" AND kind = ANY(${n})"));
            }
            let stmt = format!(
                "SELECT id, (embedding OPERATOR({v}.<=>) $1::{v}.halfvec)::float8 FROM {te} WHERE {where_} \
                 ORDER BY embedding OPERATOR({v}.<=>) $1::{v}.halfvec LIMIT $2"
            );
            let mut query = sqlx::query_as::<_, (String, f64)>(sql(stmt)).bind(q).bind(limit as i64);
            if let Some(w) = filter.workspace {
                query = query.bind(w);
            }
            if !filter.kinds.is_empty() {
                query = query.bind(filter.kinds);
            }
            let rows = query.fetch_all(&mut *tx).await?;
            tx.commit().await?;
            let mut out: Vec<(Id, f32)> = rows.into_iter().map(|(id, d)| (id, (1.0 - d) as f32)).collect();
            // Relaxed iterative scans may return near ties slightly out of order.
            out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Ok(out)
        })
    }

    fn backfill_vectors(&self, embed: EmbedFn<'_>) -> Result<usize> {
        type Conn = sqlx::pool::PoolConnection<sqlx::Postgres>;
        /// Run one step on the session that holds the advisory lock. A failed
        /// step closes the session, which releases the lock.
        fn step<T, F>(store: &PgStore, conn: Conn, f: impl FnOnce(Conn) -> F + Send + 'static) -> Result<(Conn, T)>
        where
            T: Send + 'static,
            F: Future<Output = (Conn, Result<T>)> + Send + 'static,
        {
            store.block(async move {
                let (conn, r) = f(conn).await;
                match r {
                    Ok(v) => Ok((conn, v)),
                    Err(e) => {
                        drop(conn.detach());
                        Err(e)
                    }
                }
            })
        }
        let pool = self.pool.clone();
        let key = format!("{}:vectors", self.schema);
        let te = self.t("entities");
        let v = self.vector.schema.clone();
        let index = format!("{}.entities_embedding", quote_ident(&self.schema));
        // Connections go back to the pool from inside the runtime only.
        let k = key.clone();
        let Some(conn) = self.block(async move {
            let mut c: Conn = pool.acquire().await?;
            let locked: bool =
                sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))").bind(&k).fetch_one(&mut *c).await?;
            // Not locked: another process is backfilling this tenant.
            Ok(locked.then_some(c))
        })?
        else {
            return Ok(0);
        };
        let t = te.clone();
        let (mut conn, pending) = step(self, conn, move |mut c| async move {
            let r = sqlx::query_scalar::<_, i64>(sql(format!("SELECT count(*) FROM {t} WHERE NOT embedded")))
                .fetch_one(&mut *c)
                .await
                .map_err(Error::from);
            (c, r)
        })?;
        let rebuild = pending > REBUILD_INDEX_ABOVE;
        if rebuild {
            let i = index.clone();
            conn = step(self, conn, move |mut c| async move {
                let r =
                    sqlx::query(sql(format!("DROP INDEX IF EXISTS {i}"))).execute(&mut *c).await.map_err(Error::from);
                (c, r.map(|_| ()))
            })?
            .0;
        }
        let mut done = 0usize;
        if pending > 0 {
            loop {
                let t = te.clone();
                let (c, docs) = step(self, conn, move |mut c| async move {
                    let r = sqlx::query_scalar::<_, String>(sql(format!(
                        "SELECT doc::text FROM {t} WHERE NOT embedded ORDER BY id LIMIT {CHUNK}"
                    )))
                    .fetch_all(&mut *c)
                    .await
                    .map_err(Error::from);
                    (c, r)
                })?;
                conn = c;
                if docs.is_empty() {
                    break;
                }
                let entities: Vec<Entity> = match docs.iter().map(|d| parse(d)).collect::<Result<_>>() {
                    Ok(e) => e,
                    Err(e) => {
                        // Closing the session releases the lock.
                        self.block(async move {
                            drop(conn.detach());
                            Ok(())
                        })?;
                        return Err(e);
                    }
                };
                let vectors = embed(&entities);
                let ids: Vec<String> = entities.iter().map(|e| e.id.clone()).collect();
                let embs: Vec<Option<String>> =
                    (0..ids.len()).map(|i| vectors.get(i).and_then(|v| v.as_deref().map(halfvec_text))).collect();
                done += ids.len();
                let (t, v) = (te.clone(), v.clone());
                conn = step(self, conn, move |mut c| async move {
                    let r = sqlx::query(sql(format!(
                        "UPDATE {t} e SET embedding = x.emb::{v}.halfvec, embedded = true \
                         FROM UNNEST($1::text[], $2::text[]) AS x(id, emb) WHERE e.id = x.id"
                    )))
                    .bind(ids)
                    .bind(embs)
                    .execute(&mut *c)
                    .await
                    .map_err(Error::from);
                    (c, r.map(|_| ()))
                })?
                .0;
            }
        }
        // Builds the index after a large backfill, and repairs one a crashed
        // backfill left dropped; a no-op otherwise.
        let (t, v2) = (te.clone(), v.clone());
        let (conn, ()) = step(self, conn, move |mut c| async move {
            // At least 512 MB for the build (a server configured higher keeps
            // its value). A parallel build keeps that much in shared memory,
            // which containers often cap (/dev/shm); then build serially.
            let build = |parallel: bool| {
                format!(
                    "BEGIN; \
                     DO $$ BEGIN IF pg_size_bytes(current_setting('maintenance_work_mem')) < pg_size_bytes('512MB') \
                     THEN PERFORM set_config('maintenance_work_mem', '512MB', true); END IF; END $$; {} \
                     CREATE INDEX IF NOT EXISTS entities_embedding ON {t} USING hnsw (embedding {v2}.halfvec_cosine_ops); \
                     COMMIT",
                    if parallel { "" } else { "SET LOCAL max_parallel_maintenance_workers = 0;" }
                )
            };
            let mut r = sqlx::raw_sql(sql(build(true))).execute(&mut *c).await.map(|_| ());
            if r.is_err() {
                let _ = sqlx::raw_sql(sql("ROLLBACK".to_string())).execute(&mut *c).await;
                r = sqlx::raw_sql(sql(build(false))).execute(&mut *c).await.map(|_| ());
            }
            (c, r.map_err(Error::from))
        })?;
        self.block(async move {
            let mut c = conn;
            let unlocked = sqlx::query("SELECT pg_advisory_unlock(hashtext($1))").bind(&key).execute(&mut *c).await;
            if unlocked.is_err() {
                drop(c.detach());
            }
            Ok(())
        })?;
        Ok(done)
    }

    fn trace_write(&self, batch: &[crate::trace::TraceRecord]) -> Result<()> {
        self.pg_trace_write(batch)
    }

    fn trace_list(&self, q: &crate::trace::TraceQuery) -> Result<crate::trace::TracePage> {
        self.pg_trace_list(q)
    }

    fn trace_get(&self, id: &str) -> Result<Option<crate::trace::TraceDetail>> {
        self.pg_trace_get(id)
    }

    fn trace_session(&self, id: &str, limit: usize) -> Result<Vec<crate::trace::TraceRow>> {
        self.pg_trace_session(id, limit)
    }

    fn trace_metrics(&self, q: &crate::trace::MetricsQuery) -> Result<crate::trace::TraceMetrics> {
        self.pg_trace_metrics(q)
    }

    fn trace_sessions(&self, q: &crate::trace::SessionQuery) -> Result<crate::trace::SessionPage> {
        self.pg_trace_sessions(q)
    }

    fn score_write(&self, s: &crate::trace::ScoreRow) -> Result<()> {
        self.pg_score_write(s)
    }

    fn trace_vacuum(&self, before_ms: i64, budget: Duration) -> Result<u64> {
        self.pg_trace_vacuum(before_ms, budget)
    }
}

/// id, ts, workspace, actor, action, target, document.
type EventRow = (String, i64, String, String, String, Option<String>, String);

#[derive(Clone)]
struct EntityRow {
    id: String,
    kind: String,
    workspace: String,
    doc: String,
    updated_at: i64,
    /// Its searchable text did not change: keep the stored vector.
    keep_vector: bool,
}

type EntityColumns = (Vec<String>, Vec<String>, Vec<String>, Vec<String>, Vec<i64>);

#[derive(Clone)]
struct Tables {
    head: String,
    entities: String,
    links: String,
    events: String,
    versions: String,
    link_versions: String,
    kv: String,
    commits: String,
}

impl Tables {
    fn of(s: &PgStore) -> Self {
        Self {
            head: s.t("head"),
            entities: s.t("entities"),
            links: s.t("links"),
            events: s.t("events"),
            versions: s.t("entity_versions"),
            link_versions: s.t("link_versions"),
            kv: s.t("kv"),
            commits: s.t("commits"),
        }
    }
}

fn split3(keys: &[LinkKey]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut out = (Vec::with_capacity(keys.len()), Vec::with_capacity(keys.len()), Vec::with_capacity(keys.len()));
    for (a, b, c) in keys {
        out.0.push(a.clone());
        out.1.push(b.clone());
        out.2.push(c.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{check_pgvector, check_server, halfvec_text, tenant_schema};

    #[test]
    fn old_servers_and_old_pgvector_are_refused() {
        let err = check_server("150004", "15.4").unwrap_err().to_string();
        assert!(err.contains("PostgreSQL 16 or newer") && err.contains("15.4"), "{err}");
        assert!(check_server("160015", "16.15").is_ok());
        assert!(check_server("170002", "17.2").is_ok());
        assert!(check_pgvector("0.6.2").unwrap_err().to_string().contains("pgvector 0.7"));
        assert!(!check_pgvector("0.7.4").unwrap(), "0.7 has no iterative scans");
        assert!(check_pgvector("0.8.7").unwrap());
        assert!(check_pgvector("1.0.0").unwrap());
        assert_eq!(halfvec_text(&[1, -127, 0]), "[1,-127,0]");
    }

    #[test]
    fn tenant_schema_names_are_safe() {
        assert_eq!(tenant_schema("onto_", "acme").unwrap(), "onto_acme");
        assert_eq!(tenant_schema("onto_", "Acme Corp/EU").unwrap(), "onto_acme_corp_eu");
        assert_eq!(tenant_schema("", "9lives").unwrap(), "_9lives");
        let long = tenant_schema("onto_", &"x".repeat(100)).unwrap();
        assert!(long.len() <= 63 && long.starts_with("onto_xxx"));
        assert!(tenant_schema("onto_", "").is_err());
    }
}
