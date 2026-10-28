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
mod wake;

pub use notifier::SHARED_CHANNEL;
use wake::{ScanJob, due_tenants, scan};
pub use wake::{WakeFn, WakeScanner};

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
    CHANGES_META_NOTICE, CommitRecord, EmbedFn, NoticeFn, REPLICATED_KV, Snapshot, Store, StoreNotice, StoreStats,
    VectorFilter, VersionRecord, WriteBatch,
};
use crate::changes::{
    ChangeRecord, ChangeRetention, ChangeVacuum, ChangesMeta, ConsumerLag, Cursor, Gap, GapReason, GapStats,
    GapSummary, MAX_LAG_CONSUMERS, StoredChange, StoredPage,
};
use crate::graph::LinkKey;
use crate::leases::{Fence, Lease, Timer, Wake, lease_lost};
use crate::model::{Entity, Event, EventFilter, Id, Link};
use crate::outbox::{
    Attempt, AttemptRecord, OutboxQuery, OutboxRow, OutboxSelect, OutboxState, STATS_STATES, replay_states,
};
use crate::runs::{RunQuery, RunRow, RunState, settled};
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
    (9, "changes", include_str!("../../migrations/0009_changes.sql")),
    (10, "leases", include_str!("../../migrations/0010_leases.sql")),
    (11, "outbox", include_str!("../../migrations/0011_outbox.sql")),
    (12, "automate", include_str!("../../migrations/0012_automate.sql")),
    (13, "outbox_claim", include_str!("../../migrations/0013_outbox_claim.sql")),
    (14, "feed_counters", include_str!("../../migrations/0014_feed_counters.sql")),
];
/// Most targets [`Store::outbox_state_counts`] reports (a guard: endpoints
/// and writeback targets are capped far below it).
const MAX_STATS_TARGETS: i64 = 1000;
/// This build's writer version. A tenant whose `head.min_writer` is higher
/// refuses this build's writes (builds before this one never read it; the
/// gap trigger covers them).
pub const WRITER_VERSION: i32 = 1;
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
    /// Shared schemas ([`sys_schema`]) known to exist.
    sys_ready: SysReady,
}

/// Names of the shared schemas this pool created or found.
type SysReady = Arc<Mutex<std::collections::HashSet<String>>>;

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
            sys_ready: SysReady::default(),
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
            sys_ready: SysReady::default(),
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The store of one tenant (its own PostgreSQL schema), created if missing.
    pub fn tenant(&self, tenant: &str, opts: PgOptions) -> Result<PgStore> {
        PgStore::finish(self, tenant, opts)
    }

    /// Create the deployment's shared schema for `prefix` ([`sys_schema`])
    /// with its `wake` table, if missing. Returns its name. Stores create it
    /// themselves the first time they bump a wake row.
    pub fn ensure_sys(&self, prefix: &str) -> Result<String> {
        let sys = sys_schema(prefix);
        if self.sys_ready.lock().contains(&sys) {
            return Ok(sys);
        }
        let (pool, name) = (self.pool.clone(), sys.clone());
        let (h, shared) = self.exec.handle();
        run_on(&h, shared, async move { create_sys(&pool, &name).await })?;
        self.sys_ready.lock().insert(sys.clone());
        Ok(sys)
    }

    /// Tenants under `prefix` whose background work is due by the database
    /// clock, earliest first, at most `limit` (none while no wake table exists).
    pub fn wake_due(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        let pool = self.pool.clone();
        let sys = sys_schema(prefix);
        let (h, shared) = self.exec.handle();
        run_on(&h, shared, async move { due_tenants(&pool, &sys, limit).await })
    }

    /// Start this process's candidate for the deployment's one wake scanner
    /// of `prefix` (docs/design/EVENTS.md section 10). It runs on the pool's
    /// runtime: it tries `pg_try_advisory_lock(hashtext('<sys>.wake'))` on a
    /// connection of its own (outside the pool) every `poll`; once elected it
    /// LISTENs on [`wake_channel`] and, on each notice (at most one scan per
    /// 200 ms) or every `poll`, calls `on_due` with the tenants due by the
    /// database clock. Losing the lock connection ends the term; the next
    /// candidate takes over within `poll`. Dropping the handle stops it and
    /// releases the lock.
    pub fn spawn_wake_scanner(&self, prefix: &str, poll: Duration, on_due: WakeFn) -> WakeScanner {
        let elected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let sys = sys_schema(prefix);
        let job = ScanJob {
            pool: self.pool.clone(),
            lock_key: format!("{sys}.wake"),
            sys,
            channel: wake_channel(prefix),
            poll: poll.max(Duration::from_millis(100)),
            elected: elected.clone(),
            on_due,
        };
        let (finished, done) = tokio::sync::watch::channel(false);
        let (h, _) = self.exec.handle();
        h.spawn(async move {
            scan(job, stopped).await;
            let _ = finished.send(true);
        });
        WakeScanner { elected, stop, done }
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
        let (tenant, sys) = (tenant.to_string(), sys_schema(prefix));
        run_on(&h, shared, async move { drop_schema(&pool, &s, &tenant, &sys).await })
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
    /// The deployment's shared schema ([`sys_schema`]) and its wake channel.
    sys: String,
    wake_channel: String,
    sys_ready: SysReady,
}

/// Unique violations are conflicts (a duplicate is final, never retried);
/// a pool that cannot hand out a connection, a lost connection and a server
/// shutting down are `unavailable` (worth retrying later); everything else
/// is a storage error.
impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        match error_class(&e) {
            ErrorClass::Conflict => Error::Conflict(format!("postgres: {e}")),
            ErrorClass::Unavailable => Error::Unavailable(format!("postgres: {e}")),
            ErrorClass::Storage => Error::Storage(format!("postgres: {e}")),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ErrorClass {
    Conflict,
    Unavailable,
    Storage,
}

fn error_class(e: &sqlx::Error) -> ErrorClass {
    match e {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) | sqlx::Error::WorkerCrashed => {
            ErrorClass::Unavailable
        }
        sqlx::Error::Database(d) => sqlstate_class(d.code().as_deref()),
        _ => ErrorClass::Storage,
    }
}

/// `23505` unique_violation → conflict; class `08` (connection exception),
/// `53300` too_many_connections and `57P01`-`57P03` (shutdown, cannot connect
/// now) → unavailable.
fn sqlstate_class(code: Option<&str>) -> ErrorClass {
    match code {
        Some("23505") => ErrorClass::Conflict,
        Some(c) if c.starts_with("08") || matches!(c, "53300" | "57P01" | "57P02" | "57P03") => ErrorClass::Unavailable,
        _ => ErrorClass::Storage,
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
/// A tenant id whose schema would be the deployment's shared one
/// ([`sys_schema`]) is refused.
pub fn tenant_schema(prefix: &str, tenant: &str) -> Result<String> {
    if tenant.is_empty() {
        return Err(Error::Config("postgres tenant must not be empty".into()));
    }
    let name = safe_ident(&format!("{prefix}{tenant}"), tenant);
    if name == sys_schema(prefix) {
        return Err(Error::invalid(format!(
            "tenant id '{tenant}' is reserved: its schema {name} is the deployment's shared schema \
             (a tenant created with this id by an earlier version must be renamed; see docs/STORAGE.md)"
        )));
    }
    Ok(name)
}

/// `raw` reduced to `[a-z0-9_]`, starting with a letter or `_`; past 63
/// bytes cut and suffixed with a hash of `key`.
fn safe_ident(raw: &str, key: &str) -> String {
    let mut name: String =
        raw.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if !name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_') {
        name.insert(0, '_');
    }
    if name.len() > 63 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in key.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        name.truncate(46);
        name.push_str(&format!("_{h:016x}"));
    }
    name
}

/// The deployment's shared schema for tenants under `prefix` (`onto_sys`
/// for `onto_`): the `wake` table, which holds tenant ids and times only.
pub fn sys_schema(prefix: &str) -> String {
    let base = prefix.trim_end_matches('_');
    safe_ident(&format!("{base}_sys"), prefix)
}

/// The channel wake bumps are announced on (`onto-wake` for `onto_`). The
/// `-` keeps it apart from every tenant's own notice channel, which is the
/// tenant's schema name and so only `[a-z0-9_]`: no tenant id (`wake`
/// included) can make its commits wake the scanner, and wake notices (which
/// name other tenants) never reach a tenant's listeners.
pub fn wake_channel(prefix: &str) -> String {
    format!("{}-wake", prefix.trim_end_matches('_'))
}

/// SQL error codes of a relation, schema or function that does not exist.
fn undefined(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).is_some_and(|c| matches!(c.as_ref(), "42P01" | "3F000" | "42883"))
}

/// Create `{prefix}_sys` and its `wake` table if missing. Idempotent; one
/// creator at a time.
///
/// Refused when the schema is a tenant's (a `head` or `tenant_identity`
/// table): tenant ids `sys` and `_sys` (any case) were valid before this
/// schema was reserved, and their data must not get a `wake` table mixed in.
async fn create_sys(pool: &PgPool, sys: &str) -> Result<()> {
    let q = quote_ident(sys);
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))").bind(sys).execute(&mut *tx).await?;
    let tenant: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL OR to_regclass($2) IS NOT NULL")
        .bind(format!("{q}.head"))
        .bind(format!("{q}.tenant_identity"))
        .fetch_one(&mut *tx)
        .await?;
    if tenant {
        return Err(sys_is_a_tenant(sys));
    }
    sqlx::raw_sql(sql(format!(
        "CREATE SCHEMA IF NOT EXISTS {q}; \
         CREATE TABLE IF NOT EXISTS {q}.wake (tenant TEXT PRIMARY KEY, due_at BIGINT, bumped BIGINT NOT NULL DEFAULT 0); \
         CREATE INDEX IF NOT EXISTS wake_due ON {q}.wake (due_at) WHERE due_at IS NOT NULL; \
         CREATE OR REPLACE FUNCTION {q}.now_ms() RETURNS BIGINT LANGUAGE sql VOLATILE AS \
         $$ SELECT (extract(epoch FROM clock_timestamp()) * 1000)::bigint $$;"
    )))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The shared schema's name is taken by a tenant created before it was
/// reserved: what to do about it.
fn sys_is_a_tenant(sys: &str) -> Error {
    Error::Config(format!(
        "schema {sys} holds a tenant's data (tenant id 'sys' or '_sys' from an earlier version), but this version \
         reserves it as the deployment's shared schema. Rename that tenant before upgrading, for example: \
         ALTER SCHEMA {sys} RENAME TO {sys}_tenant; UPDATE {sys}_tenant.tenant_identity SET tenant = 'sys_tenant'; \
         then address it as tenant 'sys_tenant' (docs/STORAGE.md, upgrade notes)"
    ))
}

/// Bump `tenant`'s wake row to be due `delay_ms` from now (keeping an
/// earlier due time) and announce it on `channel`, inside `tx`.
async fn bump_wake(
    tx: &mut sqlx::transaction::Transaction<'_, sqlx::Postgres>,
    sys: &str,
    channel: &str,
    tenant: &str,
    delay_ms: i64,
) -> Result<()> {
    let q = quote_ident(sys);
    sqlx::query(sql(format!(
        "INSERT INTO {q}.wake (tenant, due_at, bumped) VALUES ($1, {q}.now_ms() + $2, 1) \
         ON CONFLICT (tenant) DO UPDATE SET due_at = LEAST({q}.wake.due_at, EXCLUDED.due_at), \
         bumped = {q}.wake.bumped + 1"
    )))
    .bind(tenant)
    .bind(delay_ms.max(0))
    .execute(&mut **tx)
    .await?;
    sqlx::query("SELECT pg_notify($1, $2)").bind(channel).bind(tenant).execute(&mut **tx).await?;
    Ok(())
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

/// `DROP SCHEMA … CASCADE`, unless another tenant id owns the schema, and
/// forget the tenant's wake row (a dropped tenant is never woken).
async fn drop_schema(pool: &PgPool, schema: &str, tenant: &str, sys: &str) -> Result<()> {
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
    let forget = sqlx::query(sql(format!("DELETE FROM {}.wake WHERE tenant = $1", quote_ident(sys))))
        .bind(tenant)
        .execute(pool)
        .await;
    match forget {
        Err(e) if !undefined(&e) => Err(e.into()),
        _ => Ok(()),
    }
}

fn parse<T: serde::de::DeserializeOwned>(doc: &str) -> Result<T> {
    serde_json::from_str(doc).map_err(|e| Error::Storage(format!("corrupt row: {e}")))
}

fn sql(s: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(s)
}

/// A consumer cursor's `n` as stored: the INT column keeps `u32::MAX` ("after
/// the whole commit", where [`Cursor::floor_edge`] parks a consumer) clamped
/// to `i32::MAX`, which no record's place in a commit reaches; it reads back
/// as `u32::MAX`, so a parked consumer is not taken for an expired one.
fn stored_n(n: i32) -> u32 {
    if n == i32::MAX { u32::MAX } else { n.max(0) as u32 }
}

/// Lock the commit head for a state-changing write and return its sequence.
/// A tenant that requires a newer writer than this build is refused.
async fn lock_head(tx: &mut sqlx::transaction::Transaction<'_, sqlx::Postgres>, head: &str) -> Result<i64> {
    let (seq, min_writer): (i64, i32) =
        sqlx::query_as(sql(format!("SELECT seq, min_writer FROM {head} WHERE id = 1 FOR UPDATE")))
            .fetch_one(&mut **tx)
            .await?;
    if min_writer > WRITER_VERSION {
        return Err(Error::Unavailable(format!(
            "tenant requires a newer onto-intelli (writer version {min_writer}; this build writes as {WRITER_VERSION})"
        )));
    }
    Ok(seq)
}

/// Refuse unless `f`'s holder has the lease at its epoch by the database
/// clock. `FOR SHARE` keeps the row: a takeover waits until this
/// transaction ends, so no fenced write commits after a new holder took over.
async fn check_fence(tx: &mut sqlx::transaction::Transaction<'_, sqlx::Postgres>, t: &Tables, f: &Fence) -> Result<()> {
    let held: Option<i32> = sqlx::query_scalar(sql(format!(
        "SELECT 1 FROM {} WHERE name = $1 AND holder = $2 AND epoch = $3 AND until >= {} FOR SHARE",
        t.leases, t.now
    )))
    .bind(&f.name)
    .bind(&f.holder)
    .bind(f.epoch as i64)
    .fetch_optional(&mut **tx)
    .await?;
    if held.is_none() {
        return Err(lease_lost(f));
    }
    Ok(())
}

/// The change log's settings and head, read on `conn`.
async fn changes_meta_on<'c, E>(conn: E, t: &Tables) -> Result<ChangesMeta>
where
    E: sqlx::executor::Executor<'c, Database = sqlx::Postgres>,
{
    let (head, floor, capture, n): (i64, i64, bool, Option<i32>) = sqlx::query_as(sql(format!(
        "SELECT h.seq, m.floor, m.capture, (SELECT max(c.n) FROM {} c WHERE c.seq = h.seq) \
         FROM {} h CROSS JOIN {} m WHERE h.id = 1",
        t.changes, t.head, t.changes_meta
    )))
    .fetch_one(conn)
    .await?;
    Ok(ChangesMeta { capture, floor: floor as u64, head: Cursor::new(head as u64, n.unwrap_or(0) as u32) })
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

    fn finish(shared: &PgShared, tenant: &str, opts: PgOptions) -> Result<Self> {
        let store = Self {
            schema: tenant_schema(&opts.schema_prefix, tenant)?,
            tenant: tenant.to_string(),
            origin: new_id("proc"),
            pool: shared.pool.clone(),
            exec: shared.exec.clone(),
            vector: shared.vector.clone(),
            sys: sys_schema(&opts.schema_prefix),
            wake_channel: wake_channel(&opts.schema_prefix),
            sys_ready: shared.sys_ready.clone(),
            opts,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            notifier: shared.notifier.clone(),
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
        let (tenant, sys) = (self.tenant.clone(), self.sys.clone());
        self.block(async move { drop_schema(&pool, &s, &tenant, &sys).await })
    }

    /// Delete expired KV rows and events older than `keep_events_ms`.
    /// Change records have their own pass ([`Store::vacuum_changes`]), which
    /// also moves the floor.
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

    /// Create the deployment's shared schema the first time this pool needs it.
    fn ensure_sys(&self) -> Result<()> {
        if self.sys_ready.lock().contains(&self.sys) {
            return Ok(());
        }
        let (pool, sys) = (self.pool.clone(), self.sys.clone());
        self.block(async move { create_sys(&pool, &sys).await })?;
        self.sys_ready.lock().insert(self.sys.clone());
        Ok(())
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
        let (tch, tm) = (self.t("changes"), self.t("changes_meta"));
        let origin = self.origin.clone();
        let channel = self.channel();
        // The import's gap record lists what it wrote.
        let mut gap_entities: Vec<Id> = entities.iter().map(|e| e.id.clone()).collect();
        gap_entities.sort();
        gap_entities.dedup();
        let mut gap_links: Vec<LinkKey> = links.iter().map(|l| (l.from.clone(), l.rel.clone(), l.to.clone())).collect();
        gap_links.sort();
        gap_links.dedup();
        self.block(async move {
            let mut tx = pool.begin().await?;
            let head = lock_head(&mut tx, &th).await?;
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
            // Captured: this commit writes its own gap record (below), so the
            // trigger leaves it alone.
            let rec = CommitRecord {
                seq: seq as u64,
                ts: now_ms(),
                origin: origin.clone(),
                reload: true,
                captured: true,
                ..Default::default()
            };
            sqlx::query(sql(format!("INSERT INTO {tc} (seq, ts, origin, doc) VALUES ($1, $2, $3, $4::jsonb)")))
                .bind(seq)
                .bind(rec.ts)
                .bind(&origin)
                .bind(serde_json::to_string(&rec)?)
                .execute(&mut *tx)
                .await?;
            // One gap for the whole import, listing at most MAX_GAP_IDS ids.
            let gap = Gap::listing(GapReason::BulkImport, origin.clone(), gap_entities, gap_links);
            let gap =
                StoredChange { record: ChangeRecord::gap(seq as u64, rec.ts, gap), before_vis: None, rev_after: None };
            sqlx::query(sql(format!(
                "INSERT INTO {tch} (seq, n, ts, workspace, kind, op, target, doc) \
                 SELECT $1, 0, $2, '', '', 'gap', '', $3::jsonb WHERE (SELECT capture FROM {tm})"
            )))
            .bind(seq)
            .bind(rec.ts)
            .bind(serde_json::to_string(&gap)?)
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
        // Change records; the commit's seq is stamped into their cursor in SQL.
        let changes: Vec<ChangeRow> = if bumps && b.captured {
            b.changes
                .iter()
                .map(|c| {
                    let r = &c.record;
                    Ok(ChangeRow {
                        n: r.cursor.n as i32,
                        ts: r.ts,
                        workspace: r.workspace.clone(),
                        kind: r.kind.clone(),
                        op: r.op.as_str().to_string(),
                        target: r.id.clone(),
                        doc: serde_json::to_string(c)?,
                    })
                })
                .collect::<Result<_>>()?
        } else {
            Vec::new()
        };
        let del_e = b.del_entities.clone();
        let del_l = b.del_links.clone();
        let fence = b.fence.clone();
        let outbox = (!b.outbox.is_empty()).then(|| outbox_json(&b.outbox)).transpose()?;
        let runs = (!b.runs.is_empty()).then(|| runs_json(&b.runs)).transpose()?;
        let run_ids: Vec<String> = b.runs.iter().map(|r| r.id.clone()).collect();
        let timers: Vec<(String, i64, String)> = b
            .timers
            .iter()
            .map(|(n, d, doc)| Ok((n.clone(), *d, serde_json::to_string(doc)?)))
            .collect::<Result<_>>()?;
        let wake = match b.wake {
            Some(delay) => {
                self.ensure_sys()?;
                Some((self.sys.clone(), self.wake_channel.clone(), self.tenant.clone(), delay))
            }
            None => None,
        };
        self.block(async move {
            let mut tx = pool.begin().await?;
            // First, so a lost lease writes nothing and a takeover waits for this commit.
            if let Some(f) = &fence {
                check_fence(&mut tx, &tables, f).await?;
            }
            // Runs settle only while they are still running: a run is
            // completed at most once.
            if let Some(doc) = &runs {
                let n = settle_runs(&mut tx, &tables, doc).await?;
                if n != run_ids.len() {
                    return Err(settled(&run_ids.join(", ")));
                }
            }
            // Timers commit with what they wait for (a run in `retry` and its timer).
            for (name, delay, doc) in &timers {
                sqlx::query(sql(format!(
                    "INSERT INTO {tm} (name, due_at, doc) VALUES ($1, {now} + $2, $3::jsonb) \
                     ON CONFLICT (name) DO UPDATE SET due_at = EXCLUDED.due_at, doc = EXCLUDED.doc",
                    tm = tables.timers,
                    now = tables.now
                )))
                .bind(name)
                .bind(*delay)
                .bind(doc)
                .execute(&mut *tx)
                .await?;
            }
            let mut seq: i64 = 0;
            if bumps {
                seq = lock_head(&mut tx, &tables.head).await?;
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
            // The commit about to be made (`seq + 1`) is known once the head
            // is locked: versions carry it, so change records join them.
            let change_seq = bumps.then_some(seq + 1);
            for chunk in versions.chunks(CHUNK) {
                let mut c: (Vec<String>, Vec<i64>, Vec<i64>, Vec<String>) = Default::default();
                for (id, rev, ts, doc) in chunk.iter().cloned() {
                    c.0.push(id);
                    c.1.push(rev);
                    c.2.push(ts);
                    c.3.push(doc);
                }
                sqlx::query(sql(format!(
                    "INSERT INTO {} (id, revision, ts, doc, change_seq) \
                     SELECT id, rev, ts, doc::jsonb, $5::bigint FROM \
                     UNNEST($1::text[], $2::bigint[], $3::bigint[], $4::text[]) \
                     WITH ORDINALITY AS v(id, rev, ts, doc, n) ORDER BY n",
                    tables.versions
                )))
                .bind(c.0)
                .bind(c.1)
                .bind(c.2)
                .bind(c.3)
                .bind(change_seq)
                .execute(&mut *tx)
                .await?;
            }
            for chunk in changes.chunks(CHUNK) {
                let mut c: ChangeColumns = Default::default();
                for r in chunk.iter().cloned() {
                    c.0.push(r.n);
                    c.1.push(r.ts);
                    c.2.push(r.workspace);
                    c.3.push(r.kind);
                    c.4.push(r.op);
                    c.5.push(r.target);
                    c.6.push(r.doc);
                }
                sqlx::query(sql(format!(
                    "INSERT INTO {} (seq, n, ts, workspace, kind, op, target, doc) \
                     SELECT $1::bigint, n, ts, ws, kind, op, target, \
                     jsonb_set(doc::jsonb, '{{cursor}}', to_jsonb(($1::bigint)::text || '.' || n::text)) FROM \
                     UNNEST($2::int[], $3::bigint[], $4::text[], $5::text[], $6::text[], $7::text[], $8::text[]) \
                     AS c(n, ts, ws, kind, op, target, doc)",
                    tables.changes
                )))
                .bind(seq + 1)
                .bind(c.0)
                .bind(c.1)
                .bind(c.2)
                .bind(c.3)
                .bind(c.4)
                .bind(c.5)
                .bind(c.6)
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
            if let Some(rows) = &outbox {
                // Unique by construction: a duplicate fails the commit (Conflict).
                insert_outbox(&mut tx, &tables, rows, false).await?;
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
            if let Some((sys, chan, tenant, delay)) = &wake {
                bump_wake(&mut tx, sys, chan, tenant, *delay).await?;
            }
            tx.commit().await?;
            Ok(seq as u64)
        })
    }

    fn keeps_runs(&self) -> bool {
        true
    }

    /// One statement: `ON CONFLICT DO NOTHING` covers the id and the unique
    /// `(automation, trigger_key)`, so a duplicate trigger inserts nothing.
    fn run_claim(&self, rows: &[RunRow], fence: Option<&Fence>) -> Result<Vec<String>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let doc = runs_json(rows)?;
        let fence = fence.cloned();
        self.block(async move {
            let mut tx = pool.begin().await?;
            if let Some(f) = &fence {
                check_fence(&mut tx, &t, f).await?;
            }
            let ids: Vec<String> = sqlx::query_scalar(sql(format!(
                "WITH c AS (SELECT {now} AS t) \
                 INSERT INTO {r} (id, automation, trigger_key, closure_hash, state, depth, epoch, started_at, \
                 finished_at, doc) \
                 SELECT x.id, x.automation, x.trigger_key, x.closure_hash, x.state, x.depth, x.epoch, c.t, \
                 CASE WHEN x.final THEN c.t END, x.doc FROM c, jsonb_to_recordset($1::jsonb) AS x(id text, \
                 automation text, trigger_key text, closure_hash text, state text, depth int, epoch bigint, \
                 final boolean, doc jsonb) ON CONFLICT DO NOTHING RETURNING id",
                r = t.runs,
                now = t.now
            )))
            .bind(&doc)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(ids)
        })
    }

    fn run_update(&self, row: &RunRow, from: &[RunState], fence: Option<&Fence>) -> Result<bool> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let doc = serde_json::to_string(row)?;
        let (id, state, epoch, fin) =
            (row.id.clone(), row.state.as_str().to_string(), row.epoch as i64, row.state.is_final());
        let from: Vec<String> = from.iter().map(|s| s.as_str().to_string()).collect();
        let fence = fence.cloned();
        self.block(async move {
            let mut tx = pool.begin().await?;
            if let Some(f) = &fence {
                check_fence(&mut tx, &t, f).await?;
            }
            let n = sqlx::query(sql(format!(
                "UPDATE {r} SET state = $2, epoch = $3, finished_at = CASE WHEN $4 THEN {now} END, doc = $5::jsonb \
                 WHERE id = $1 AND (cardinality($6::text[]) = 0 OR state = ANY($6))",
                r = t.runs,
                now = t.now
            )))
            .bind(&id)
            .bind(&state)
            .bind(epoch)
            .bind(fin)
            .bind(&doc)
            .bind(&from)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            tx.commit().await?;
            Ok(n > 0)
        })
    }

    fn run_get(&self, id: &str) -> Result<Option<RunRow>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let id = id.to_string();
        self.block(async move {
            let row = sqlx::query(sql(format!("SELECT {RUN_COLS} FROM {} WHERE id = $1", t.runs)))
                .bind(&id)
                .fetch_optional(&pool)
                .await?;
            row.as_ref().map(run_row).transpose()
        })
    }

    fn runs_list(&self, q: &RunQuery) -> Result<Vec<RunRow>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let q = q.clone();
        let states: Vec<String> = q.states.iter().map(|s| s.as_str().to_string()).collect();
        self.block(async move {
            let rows = sqlx::query(sql(format!(
                "SELECT {RUN_COLS} FROM {} WHERE ($1::text IS NULL OR automation = $1) \
                 AND (cardinality($2::text[]) = 0 OR state = ANY($2)) AND ($3::bigint IS NULL OR started_at >= $3) \
                 ORDER BY started_at DESC, id DESC LIMIT $4",
                t.runs
            )))
            .bind(&q.automation)
            .bind(&states)
            .bind(q.since)
            .bind(q.limit() as i64)
            .fetch_all(&pool)
            .await?;
            rows.iter().map(run_row).collect()
        })
    }

    /// From `automation_runs_started` (started_at DESC, id DESC); reads the
    /// state column only.
    fn runs_state_counts(&self, since_ms: i64, cap: usize) -> Result<Vec<(RunState, u64)>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        self.block(async move {
            let rows: Vec<(String, i64)> = sqlx::query_as(sql(format!(
                "SELECT state, count(*) FROM (SELECT state FROM {} WHERE started_at >= $1 \
                 ORDER BY started_at DESC, id DESC LIMIT $2) x GROUP BY state ORDER BY state",
                t.runs
            )))
            .bind(since_ms)
            .bind(cap.min(i64::MAX as usize) as i64)
            .fetch_all(&pool)
            .await?;
            rows.into_iter().map(|(st, n)| Ok((st.parse()?, n.max(0) as u64))).collect()
        })
    }

    fn runs_running(&self, epoch: u64, limit: usize) -> Result<Vec<RunRow>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        self.block(async move {
            let rows = sqlx::query(sql(format!(
                "SELECT {RUN_COLS} FROM {} WHERE state = 'running' AND epoch < $1 ORDER BY started_at, id LIMIT $2",
                t.runs
            )))
            .bind(epoch.min(i64::MAX as u64) as i64)
            .bind(limit.clamp(1, 1000) as i64)
            .fetch_all(&pool)
            .await?;
            rows.iter().map(run_row).collect()
        })
    }

    fn runs_vacuum(&self, before_ms: i64) -> Result<u64> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        self.block(async move {
            Ok(sqlx::query(sql(format!(
                "DELETE FROM {} WHERE state NOT IN ('running', 'retry') AND started_at < $1",
                t.runs
            )))
            .bind(before_ms)
            .execute(&pool)
            .await?
            .rows_affected())
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

    fn keeps_changes(&self) -> bool {
        true
    }

    /// One read-only snapshot: the head and floor, then the records after
    /// `after`, each create and update joined with the entity version its
    /// commit wrote (`entity_versions.change_seq`).
    fn changes_since(&self, after: Cursor, limit: usize) -> Result<StoredPage> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        // `n` is an INT column: past i32::MAX means "after the whole commit".
        let n = after.n.min(i32::MAX as u32) as i32;
        let limit = limit.clamp(1, 10_000) as i64;
        self.block(async move {
            let mut tx = pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").execute(&mut *tx).await?;
            let meta = changes_meta_on(&mut *tx, &t).await?;
            let rows: Vec<(i64, i32, String, Option<String>)> = sqlx::query_as(sql(format!(
                "SELECT c.seq, c.n, c.doc::text, v.entity::text FROM {} c \
                 LEFT JOIN LATERAL (SELECT x.doc->'doc' AS entity FROM {} x \
                     WHERE c.op IN ('entity.create', 'entity.update') AND x.id = c.target AND x.change_seq = c.seq \
                     ORDER BY x.seq DESC LIMIT 1) v ON true \
                 WHERE (c.seq, c.n) > ($1, $2) ORDER BY c.seq, c.n LIMIT $3",
                t.changes, t.versions
            )))
            .bind(after.seq as i64)
            .bind(n)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            let mut changes = Vec::with_capacity(rows.len());
            for (seq, n, doc, entity) in rows {
                let mut c: StoredChange = parse(&doc)?;
                c.record.cursor = Cursor::new(seq as u64, n as u32);
                let entity: Option<Entity> = match entity {
                    Some(e) => parse(&e)?,
                    None => None,
                };
                changes.push((c, entity));
            }
            Ok(StoredPage { changes, head: meta.head, floor: meta.floor })
        })
    }

    fn changes_meta(&self) -> Result<ChangesMeta> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        self.block(async move { changes_meta_on(&pool, &t).await })
    }

    /// Both from the gap-records index (`changes_gaps`, 0014): the count
    /// reads at most `cap` index entries, the newest gap one. The newest
    /// gap's id lists are counted on the server, never sent.
    fn gap_stats(&self, since_ms: i64, cap: u64) -> Result<GapStats> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        self.block(async move {
            #[allow(clippy::type_complexity)]
            let (count, seq, n, ts, reason, origin, listed, truncated, total): (
                i64,
                Option<i64>,
                Option<i32>,
                Option<i64>,
                Option<String>,
                Option<String>,
                Option<i64>,
                Option<bool>,
                Option<i64>,
            ) = sqlx::query_as(sql(format!(
                "SELECT (SELECT count(*) FROM (SELECT 1 FROM {c} WHERE op = 'gap' AND ts >= $1 LIMIT $2) x), \
                   l.seq, l.n, l.ts, l.g->>'reason', l.g->>'origin', \
                   jsonb_array_length(coalesce(l.g->'entities', '[]'::jsonb)) \
                     + jsonb_array_length(coalesce(l.g->'links', '[]'::jsonb)) \
                     + coalesce((l.g->>'hidden')::bigint, 0), \
                   (l.g->>'truncated')::boolean, (l.g->>'total')::bigint \
                 FROM (SELECT 1) one LEFT JOIN LATERAL ( \
                   SELECT seq, n, ts, doc->'gap' AS g FROM {c} WHERE op = 'gap' \
                   ORDER BY ts DESC, seq DESC, n DESC LIMIT 1) l ON true",
                c = t.changes
            )))
            .bind(since_ms)
            .bind(cap.min(i64::MAX as u64) as i64)
            .fetch_one(&pool)
            .await?;
            let latest = match (seq, n, ts) {
                (Some(seq), Some(n), Some(ts)) => {
                    let listed = listed.unwrap_or(0).max(0) as u64;
                    let total = total.unwrap_or(0).max(0) as u64;
                    Some(GapSummary {
                        cursor: Cursor::new(seq.max(0) as u64, n.max(0) as u32),
                        ts,
                        reason: reason.unwrap_or_default(),
                        origin: origin.unwrap_or_default(),
                        touched: if truncated.unwrap_or(false) { total.max(listed) } else { listed },
                    })
                }
                _ => None,
            };
            let count = count.max(0) as u64;
            Ok(GapStats { count, capped: cap > 0 && count >= cap, latest })
        })
    }

    /// Locks the head, so no commit straddles the switch: commits after it
    /// see the new setting in the gap trigger. Turning capture on after a
    /// period off raises the floor to the head (older cursors are expired)
    /// and writes a `capture_enabled` gap after every cursor at the head (a
    /// reader moved to the head while capture was off receives it). Other
    /// processes hear of it through a notice and re-read the setting.
    fn set_capture(&self, on: bool) -> Result<()> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let (channel, origin) = (self.channel(), self.origin.clone());
        self.block(async move {
            let mut tx = pool.begin().await?;
            let head = lock_head(&mut tx, &t.head).await?;
            let was_on: bool = sqlx::query_scalar(sql(format!("SELECT capture FROM {} FOR UPDATE", t.changes_meta)))
                .fetch_one(&mut *tx)
                .await?;
            sqlx::query(sql(format!(
                "UPDATE {} SET floor = CASE WHEN $1 AND NOT capture THEN GREATEST(floor, $2) ELSE floor END, \
                 capture = $1",
                t.changes_meta
            )))
            .bind(on)
            .bind(head)
            .execute(&mut *tx)
            .await?;
            if on && !was_on && head > 0 {
                // A reader moved to the head while capture was off gets this
                // gap instead of silently missing what was written meanwhile.
                let last_n: Option<i32> =
                    sqlx::query_scalar(sql(format!("SELECT max(n) FROM {} WHERE seq = $1", t.changes)))
                        .bind(head)
                        .fetch_one(&mut *tx)
                        .await?;
                let gap = super::capture_enabled_gap(head as u64, last_n.map(|n| n as u32), &origin);
                sqlx::query(sql(format!(
                    "INSERT INTO {} (seq, n, ts, workspace, kind, op, target, doc) \
                     VALUES ($1, $2, $3, '', '', 'gap', '', $4::jsonb)",
                    t.changes
                )))
                .bind(head)
                .bind(gap.record.cursor.n.min(i32::MAX as u32) as i32)
                .bind(gap.record.ts)
                .bind(serde_json::to_string(&gap)?)
                .execute(&mut *tx)
                .await?;
            }
            send_notice(
                &mut tx,
                &channel,
                json!({ "t": "kv", "ns": CHANGES_META_NOTICE, "key": "capture", "o": origin }),
            )
            .await?;
            tx.commit().await?;
            Ok(())
        })
    }

    /// Deletes whole commits below the new floor and writes the floor in the
    /// same transaction (the only DELETE the table accepts, as for events).
    /// The floor never passes the head.
    fn vacuum_changes(&self, keep: &ChangeRetention) -> Result<ChangeVacuum> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let keep = *keep;
        self.block(async move {
            let mut tx = pool.begin().await?;
            sqlx::query("SELECT set_config('onto.retention', 'on', true)").execute(&mut *tx).await?;
            // One retention pass at a time per tenant.
            let old: i64 = sqlx::query_scalar(sql(format!("SELECT floor FROM {} FOR UPDATE", t.changes_meta)))
                .fetch_one(&mut *tx)
                .await?;
            let mut floor = old;
            if let Some(ms) = keep.keep_ms {
                let newest_old: Option<i64> =
                    sqlx::query_scalar(sql(format!("SELECT max(seq) FROM {} WHERE ts < $1", t.changes)))
                        .bind(now_ms().saturating_sub(ms))
                        .fetch_one(&mut *tx)
                        .await?;
                if let Some(seq) = newest_old {
                    floor = floor.max(seq + 1);
                }
            }
            if let Some(rows) = keep.keep_rows {
                // The newest record past the ones kept: its commit and older go.
                let cut: Option<i64> = sqlx::query_scalar(sql(format!(
                    "SELECT seq FROM {} ORDER BY seq DESC, n DESC OFFSET $1 LIMIT 1",
                    t.changes
                )))
                .bind(rows.min(i64::MAX as u64) as i64)
                .fetch_optional(&mut *tx)
                .await?;
                if let Some(seq) = cut {
                    floor = floor.max(seq + 1);
                }
            }
            // Never above the head: the head cursor stays valid, so a reader
            // told to resume from it is not reset again. The newest commit's
            // records are therefore always kept.
            let head: i64 =
                sqlx::query_scalar(sql(format!("SELECT seq FROM {} WHERE id = 1", t.head))).fetch_one(&mut *tx).await?;
            let floor = floor.min(head).max(old);
            let mut deleted = 0;
            if floor > old {
                deleted = sqlx::query(sql(format!("DELETE FROM {} WHERE seq < $1", t.changes)))
                    .bind(floor)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                sqlx::query(sql(format!("UPDATE {} SET floor = $1", t.changes_meta)))
                    .bind(floor)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            Ok(ChangeVacuum { deleted, floor: floor as u64 })
        })
    }

    fn fences(&self) -> bool {
        true
    }

    fn clock_ms(&self) -> Result<i64> {
        let pool = self.pool.clone();
        let now = Tables::of(self).now;
        self.block(async move { Ok(sqlx::query_scalar(sql(format!("SELECT {now}"))).fetch_one(&pool).await?) })
    }

    /// One statement (docs/design/EVENTS.md section 9): insert, or take over
    /// an expired lease (epoch + 1), or renew one's own (same epoch).
    fn lease(&self, name: &str, holder: &str, ttl_ms: i64) -> Result<Option<u64>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let (name, holder) = (name.to_string(), holder.to_string());
        self.block(async move {
            let epoch: Option<i64> = sqlx::query_scalar(sql(format!(
                "INSERT INTO {l} AS l (name, holder, epoch, until) VALUES ($1, $2, 1, {now} + $3) \
                 ON CONFLICT (name) DO UPDATE SET \
                 epoch = CASE WHEN l.holder = EXCLUDED.holder THEN l.epoch ELSE l.epoch + 1 END, \
                 holder = EXCLUDED.holder, until = EXCLUDED.until \
                 WHERE l.until < {now} OR l.holder = EXCLUDED.holder \
                 RETURNING epoch",
                l = t.leases,
                now = t.now
            )))
            .bind(&name)
            .bind(&holder)
            .bind(ttl_ms.max(0))
            .fetch_optional(&pool)
            .await?;
            Ok(epoch.map(|e| e as u64))
        })
    }

    /// Fenced: only the holder at its epoch, while it still holds the lease.
    fn release(&self, fence: &Fence) -> Result<bool> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let f = fence.clone();
        self.block(async move {
            let n = sqlx::query(sql(format!(
                "UPDATE {l} SET until = {now} - 1 WHERE name = $1 AND holder = $2 AND epoch = $3 AND until >= {now}",
                l = t.leases,
                now = t.now
            )))
            .bind(&f.name)
            .bind(&f.holder)
            .bind(f.epoch as i64)
            .execute(&pool)
            .await?
            .rows_affected();
            Ok(n > 0)
        })
    }

    fn lease_get(&self, name: &str) -> Result<Option<Lease>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let name = name.to_string();
        self.block(async move {
            let row: Option<(String, i64, i64, bool)> = sqlx::query_as(sql(format!(
                "SELECT holder, epoch, until, until >= {} FROM {} WHERE name = $1",
                t.now, t.leases
            )))
            .bind(&name)
            .fetch_optional(&pool)
            .await?;
            Ok(row.map(|(holder, epoch, until, held)| Lease { name, holder, epoch: epoch as u64, until, held }))
        })
    }

    fn consumer_get(&self, name: &str) -> Result<Option<Cursor>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let name = name.to_string();
        self.block(async move {
            let row: Option<(i64, i32)> =
                sqlx::query_as(sql(format!("SELECT seq, n FROM {} WHERE name = $1", t.consumers)))
                    .bind(&name)
                    .fetch_optional(&pool)
                    .await?;
            Ok(row.map(|(seq, n)| Cursor::new(seq.max(0) as u64, stored_n(n))))
        })
    }

    fn consumer_delete(&self, name: &str, fence: Option<&Fence>) -> Result<bool> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let (name, fence) = (name.to_string(), fence.cloned());
        self.block(async move {
            let mut tx = pool.begin().await?;
            if let Some(f) = &fence {
                check_fence(&mut tx, &t, f).await?;
            }
            let n = sqlx::query(sql(format!("DELETE FROM {} WHERE name = $1", t.consumers)))
                .bind(&name)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            tx.commit().await?;
            Ok(n > 0)
        })
    }

    /// One statement: per consumer, the head, the floor and its oldest
    /// unread record (one primary-key probe after its cursor).
    fn consumer_lags(&self) -> Result<Vec<ConsumerLag>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        self.block(async move {
            // name, seq, n, updated_at, head seq, floor, oldest unread ts, clock
            type LagRow = (String, i64, i32, i64, i64, i64, Option<i64>, i64);
            let rows: Vec<LagRow> = sqlx::query_as(sql(format!(
                "SELECT k.name, k.seq, k.n, k.updated_at, h.seq, m.floor, \
                   (SELECT c.ts FROM {c} c WHERE (c.seq, c.n) > (k.seq, k.n) ORDER BY c.seq, c.n LIMIT 1), {now} \
                 FROM {k} k CROSS JOIN {h} h CROSS JOIN {m} m WHERE h.id = 1 ORDER BY k.name LIMIT $1",
                c = t.changes,
                k = t.consumers,
                h = t.head,
                m = t.changes_meta,
                now = t.now
            )))
            .bind(MAX_LAG_CONSUMERS as i64)
            .fetch_all(&pool)
            .await?;
            Ok(rows
                .into_iter()
                .map(|(name, seq, n, updated_at, head, floor, oldest, now)| {
                    let cursor = Cursor::new(seq.max(0) as u64, stored_n(n));
                    ConsumerLag {
                        name,
                        cursor,
                        behind: head.saturating_sub(seq).max(0) as u64,
                        age_ms: oldest.map_or(0, |ts| now.saturating_sub(ts).max(0)),
                        updated_at: Some(updated_at),
                        expired: cursor.expired_at(floor.max(0) as u64),
                    }
                })
                .collect())
        })
    }

    fn consumer_set(&self, name: &str, cursor: Cursor, fence: Option<&Fence>) -> Result<()> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let (name, fence) = (name.to_string(), fence.cloned());
        let n = cursor.n.min(i32::MAX as u32) as i32;
        self.block(async move {
            let mut tx = pool.begin().await?;
            if let Some(f) = &fence {
                check_fence(&mut tx, &t, f).await?;
            }
            sqlx::query(sql(format!(
                "INSERT INTO {c} (name, seq, n, updated_at) VALUES ($1, $2, $3, {now}) \
                 ON CONFLICT (name) DO UPDATE SET seq = EXCLUDED.seq, n = EXCLUDED.n, updated_at = EXCLUDED.updated_at",
                c = t.consumers,
                now = t.now
            )))
            .bind(&name)
            .bind(cursor.seq as i64)
            .bind(n)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(())
        })
    }

    /// The timer and the tenant's wake row move in one transaction.
    fn timers_set(&self, name: &str, delay_ms: i64, doc: &Value) -> Result<i64> {
        self.ensure_sys()?;
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let (sys, chan, tenant) = (self.sys.clone(), self.wake_channel.clone(), self.tenant.clone());
        let (name, doc) = (name.to_string(), serde_json::to_string(doc)?);
        self.block(async move {
            let mut tx = pool.begin().await?;
            let due: i64 = sqlx::query_scalar(sql(format!(
                "INSERT INTO {tm} (name, due_at, doc) VALUES ($1, {now} + $2, $3::jsonb) \
                 ON CONFLICT (name) DO UPDATE SET due_at = EXCLUDED.due_at, doc = EXCLUDED.doc RETURNING due_at",
                tm = t.timers,
                now = t.now
            )))
            .bind(&name)
            .bind(delay_ms)
            .bind(&doc)
            .fetch_one(&mut *tx)
            .await?;
            bump_wake(&mut tx, &sys, &chan, &tenant, delay_ms).await?;
            tx.commit().await?;
            Ok(due)
        })
    }

    fn timers_due(&self, limit: usize) -> Result<Vec<Timer>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        self.block(async move {
            let rows: Vec<(String, i64, String)> = sqlx::query_as(sql(format!(
                "SELECT name, due_at, doc::text FROM {} WHERE due_at <= {} ORDER BY due_at, name LIMIT $1",
                t.timers, t.now
            )))
            .bind(limit.clamp(1, 10_000) as i64)
            .fetch_all(&pool)
            .await?;
            rows.into_iter().map(|(name, due_at, doc)| Ok(Timer { name, due_at, doc: parse(&doc)? })).collect()
        })
    }

    fn timers_next(&self) -> Result<Option<i64>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        self.block(async move {
            Ok(sqlx::query_scalar(sql(format!("SELECT min(due_at) FROM {}", t.timers))).fetch_one(&pool).await?)
        })
    }

    fn timers_delete(&self, name: &str, due_at: Option<i64>) -> Result<bool> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let name = name.to_string();
        self.block(async move {
            let n = sqlx::query(sql(format!(
                "DELETE FROM {} WHERE name = $1 AND ($2::bigint IS NULL OR due_at = $2)",
                t.timers
            )))
            .bind(&name)
            .bind(due_at)
            .execute(&pool)
            .await?
            .rows_affected();
            Ok(n > 0)
        })
    }

    fn wake_get(&self) -> Result<Option<Wake>> {
        let pool = self.pool.clone();
        let (sys, tenant) = (quote_ident(&self.sys), self.tenant.clone());
        self.block(async move {
            let row = sqlx::query_as::<_, (Option<i64>, i64)>(sql(format!(
                "SELECT due_at, bumped FROM {sys}.wake WHERE tenant = $1"
            )))
            .bind(&tenant)
            .fetch_optional(&pool)
            .await;
            match row {
                Ok(r) => Ok(r.map(|(due_at, bumped)| Wake { due_at, bumped: bumped as u64 })),
                Err(e) if undefined(&e) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    /// `UPDATE … WHERE bumped = $seen` as an upsert: a row bumped since
    /// `seen` was read is left alone (false), so that bump is not lost.
    fn wake_settle(&self, seen: u64, next_due: Option<i64>) -> Result<bool> {
        if next_due.is_some() {
            self.ensure_sys()?;
        }
        let pool = self.pool.clone();
        let (sys, tenant) = (quote_ident(&self.sys), self.tenant.clone());
        self.block(async move {
            let n = sqlx::query(sql(format!(
                "INSERT INTO {sys}.wake AS w (tenant, due_at, bumped) VALUES ($1, $2, $3) \
                 ON CONFLICT (tenant) DO UPDATE SET due_at = EXCLUDED.due_at WHERE w.bumped = $3"
            )))
            .bind(&tenant)
            .bind(next_due)
            .bind(seen as i64)
            .execute(&pool)
            .await;
            match n {
                Ok(r) => Ok(r.rows_affected() > 0),
                // No wake table yet and nothing due: nothing to settle.
                Err(e) if undefined(&e) => Ok(true),
                Err(e) => Err(e.into()),
            }
        })
    }

    fn keeps_outbox(&self) -> bool {
        true
    }

    fn outbox_enqueue(
        &self,
        rows: &[OutboxRow],
        advance: Option<(&str, Cursor)>,
        fence: Option<&Fence>,
    ) -> Result<usize> {
        for r in rows {
            crate::outbox::check_row(r)?;
        }
        if rows.is_empty() && advance.is_none() {
            return Ok(0);
        }
        if !rows.is_empty() {
            self.ensure_sys()?;
        }
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let (sys, chan, tenant) = (self.sys.clone(), self.wake_channel.clone(), self.tenant.clone());
        let doc = (!rows.is_empty()).then(|| outbox_json(rows)).transpose()?;
        let advance = advance.map(|(n, c)| (n.to_string(), c));
        let fence = fence.cloned();
        self.block(async move {
            let mut tx = pool.begin().await?;
            if let Some(f) = &fence {
                check_fence(&mut tx, &t, f).await?;
            }
            let n = match &doc {
                Some(doc) => insert_outbox(&mut tx, &t, doc, true).await?,
                None => 0,
            };
            if let Some((name, cursor)) = &advance {
                sqlx::query(sql(format!(
                    "INSERT INTO {c} (name, seq, n, updated_at) VALUES ($1, $2, $3, {now}) \
                     ON CONFLICT (name) DO UPDATE SET seq = EXCLUDED.seq, n = EXCLUDED.n, updated_at = EXCLUDED.updated_at",
                    c = t.consumers,
                    now = t.now
                )))
                .bind(name)
                .bind(cursor.seq as i64)
                .bind(cursor.n.min(i32::MAX as u32) as i32)
                .execute(&mut *tx)
                .await?;
            }
            if n > 0 {
                bump_wake(&mut tx, &sys, &chan, &tenant, 0).await?;
            }
            tx.commit().await?;
            Ok(n)
        })
    }

    /// One statement (docs/design/EVENTS.md B.1): `FOR UPDATE SKIP LOCKED`
    /// keeps concurrent claims disjoint.
    fn outbox_claim(
        &self,
        limit: usize,
        lease_ms: i64,
        holder: &str,
        skip_targets: &[String],
    ) -> Result<Vec<OutboxRow>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let (holder, skip) = (holder.to_string(), skip_targets.to_vec());
        self.block(async move {
            // The clock is read once; due rows and expired leases each come
            // from their own index in claim order (0013), so a claim reads
            // O(limit) rows however long the backlog is.
            let rows = sqlx::query(sql(format!(
                "WITH clock AS (SELECT {now} AS now), \
                 ready AS (SELECT id, next_at AS due FROM {o} \
                   WHERE state IN ('pending', 'retry') AND next_at <= (SELECT now FROM clock) \
                   AND NOT (target = ANY($4::text[])) \
                   ORDER BY next_at, id LIMIT $1 FOR UPDATE SKIP LOCKED), \
                 leased AS (SELECT id, lease_until AS due FROM {o} \
                   WHERE state = 'inflight' AND lease_until < (SELECT now FROM clock) \
                   AND NOT (target = ANY($4::text[])) \
                   ORDER BY lease_until, id LIMIT $1 FOR UPDATE SKIP LOCKED), \
                 pick AS (SELECT id FROM (SELECT id, due FROM ready UNION ALL SELECT id, due FROM leased) u \
                   ORDER BY due, id LIMIT $1) \
                 UPDATE {o} SET state = 'inflight', claim = $3 || ':' || gen_random_uuid()::text, \
                 lease_until = (SELECT now FROM clock) + $2, attempts = attempts + 1 \
                 WHERE id IN (SELECT id FROM pick) \
                 RETURNING {OUTBOX_COLS}",
                o = t.outbox,
                now = t.now
            )))
            .bind(limit.clamp(1, 1000) as i64)
            .bind(lease_ms.max(0))
            .bind(&holder)
            .bind(&skip)
            .fetch_all(&pool)
            .await?;
            let mut out: Vec<OutboxRow> = rows.iter().map(outbox_row).collect::<Result<_>>()?;
            out.sort_by(|a, b| a.next_at.cmp(&b.next_at).then_with(|| a.id.cmp(&b.id)));
            Ok(out)
        })
    }

    fn outbox_finish(
        &self,
        id: &str,
        claim: &str,
        attempt: &Attempt,
        next: OutboxState,
        delay_ms: i64,
    ) -> Result<bool> {
        let retry = next == OutboxState::Retry;
        if retry {
            self.ensure_sys()?;
        }
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let (sys, chan, tenant) = (self.sys.clone(), self.wake_channel.clone(), self.tenant.clone());
        let (id, claim) = (id.to_string(), claim.to_string());
        let attempt = attempt.clone();
        let doc = serde_json::to_string(&json!({ "state": next, "result": &attempt }))?;
        self.block(async move {
            let mut tx = pool.begin().await?;
            let attempts: Option<i32> = sqlx::query_scalar(sql(format!(
                "UPDATE {o} SET state = $3, claim = NULL, lease_until = NULL, last_status = $4, last_error = $5, \
                 next_at = CASE WHEN $3 = 'retry' THEN {now} + $6 ELSE next_at END, \
                 delivered_at = CASE WHEN $3 = 'delivered' THEN {now} ELSE delivered_at END \
                 WHERE id = $1 AND claim = $2 RETURNING attempts",
                o = t.outbox,
                now = t.now
            )))
            .bind(&id)
            .bind(&claim)
            .bind(next.as_str())
            .bind(attempt.status.map(i32::from))
            .bind(&attempt.error)
            .bind(delay_ms.max(0))
            .fetch_optional(&mut *tx)
            .await?;
            let Some(n) = attempts else { return Ok(false) };
            sqlx::query(sql(format!(
                "INSERT INTO {a} (outbox_id, claim, attempt, ts, doc) VALUES ($1, $2, $3, {now}, $4::jsonb)",
                a = t.attempts,
                now = t.now
            )))
            .bind(&id)
            .bind(&claim)
            .bind(n)
            .bind(&doc)
            .execute(&mut *tx)
            .await?;
            if retry {
                bump_wake(&mut tx, &sys, &chan, &tenant, delay_ms).await?;
            }
            tx.commit().await?;
            Ok(true)
        })
    }

    fn outbox_release(&self, id: &str, claim: &str) -> Result<bool> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let (id, claim) = (id.to_string(), claim.to_string());
        self.block(async move {
            let n = sqlx::query(sql(format!(
                "UPDATE {} SET attempts = greatest(attempts - 1, 0), \
                 state = CASE WHEN attempts <= 1 THEN 'pending' ELSE 'retry' END, claim = NULL, lease_until = NULL \
                 WHERE id = $1 AND claim = $2",
                t.outbox
            )))
            .bind(&id)
            .bind(&claim)
            .execute(&pool)
            .await?
            .rows_affected();
            Ok(n > 0)
        })
    }

    fn outbox_get(&self, id: &str) -> Result<Option<OutboxRow>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let id = id.to_string();
        self.block(async move {
            let row = sqlx::query(sql(format!("SELECT {OUTBOX_COLS} FROM {} WHERE id = $1", t.outbox)))
                .bind(&id)
                .fetch_optional(&pool)
                .await?;
            row.as_ref().map(outbox_row).transpose()
        })
    }

    fn outbox_list(&self, q: &OutboxQuery) -> Result<Vec<OutboxRow>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let q = q.clone();
        let states: Vec<String> = q.states.iter().map(|s| s.as_str().to_string()).collect();
        self.block(async move {
            let rows = sqlx::query(sql(format!(
                "SELECT {OUTBOX_COLS} FROM {} WHERE ($1::text IS NULL OR target = $1) \
                 AND (cardinality($2::text[]) = 0 OR state = ANY($2)) AND ($3::text IS NULL OR source = $3) \
                 AND ($4::text IS NULL OR receipt = $4) AND ($5::text IS NULL OR run = $5) \
                 AND ($6::bigint IS NULL OR created_at >= $6) ORDER BY created_at DESC, id DESC LIMIT $7",
                t.outbox
            )))
            .bind(&q.target)
            .bind(&states)
            .bind(&q.source)
            .bind(&q.receipt)
            .bind(&q.run)
            .bind(q.since)
            .bind(q.limit() as i64)
            .fetch_all(&pool)
            .await?;
            rows.iter().map(outbox_row).collect()
        })
    }

    fn outbox_reset(&self, target: &str, sel: &OutboxSelect, states: &[OutboxState]) -> Result<usize> {
        self.ensure_sys()?;
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let (sys, chan, tenant) = (self.sys.clone(), self.wake_channel.clone(), self.tenant.clone());
        let target = target.to_string();
        let states: Vec<String> = replay_states(states).iter().map(|s| s.as_str().to_string()).collect();
        let (ids, since): (Option<Vec<String>>, Option<i64>) = match sel {
            OutboxSelect::Ids(ids) => (Some(ids.clone()), None),
            OutboxSelect::Since(t) => (None, Some(*t)),
        };
        self.block(async move {
            let mut tx = pool.begin().await?;
            let n = sqlx::query(sql(format!(
                "UPDATE {o} SET state = 'pending', attempts = 0, next_at = {now}, claim = NULL, lease_until = NULL, \
                 delivered_at = NULL WHERE target = $1 AND state = ANY($2) \
                 AND ($3::text[] IS NULL OR id = ANY($3)) AND ($4::bigint IS NULL OR created_at >= $4)",
                o = t.outbox,
                now = t.now
            )))
            .bind(&target)
            .bind(&states)
            .bind(&ids)
            .bind(since)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if n > 0 {
                bump_wake(&mut tx, &sys, &chan, &tenant, 0).await?;
            }
            tx.commit().await?;
            Ok(n as usize)
        })
    }

    fn outbox_attempts(&self, id: &str, limit: usize) -> Result<Vec<AttemptRecord>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let id = id.to_string();
        self.block(async move {
            let rows: Vec<(String, i32, i64, String)> = sqlx::query_as(sql(format!(
                "SELECT outbox_id, attempt, ts, doc::text FROM {} WHERE outbox_id = $1 \
                 ORDER BY ts DESC, attempt DESC LIMIT $2",
                t.attempts
            )))
            .bind(&id)
            .bind(limit.clamp(1, 1000) as i64)
            .fetch_all(&pool)
            .await?;
            rows.into_iter()
                .map(|(outbox_id, attempt, ts, doc)| {
                    let d: AttemptDoc = parse(&doc)?;
                    Ok(AttemptRecord {
                        outbox_id,
                        attempt: attempt.max(0) as u32,
                        ts,
                        state: d.state,
                        result: d.result,
                    })
                })
                .collect()
        })
    }

    fn outbox_next_due(&self) -> Result<Option<i64>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        self.block(async move {
            Ok(sqlx::query_scalar(sql(format!(
                "SELECT min(CASE WHEN state = 'inflight' THEN lease_until ELSE next_at END) FROM {} \
                 WHERE state IN ('pending', 'retry', 'inflight')",
                t.outbox
            )))
            .fetch_one(&pool)
            .await?)
        })
    }

    fn outbox_open_counts(&self) -> Result<Vec<(String, u64)>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        self.block(async move {
            let rows: Vec<(String, i64)> = sqlx::query_as(sql(format!(
                "SELECT target, count(*) FROM {} WHERE state IN ('pending', 'retry', 'inflight') \
                 GROUP BY target ORDER BY target",
                t.outbox
            )))
            .fetch_all(&pool)
            .await?;
            Ok(rows.into_iter().map(|(t, n)| (t, n.max(0) as u64)).collect())
        })
    }

    fn outbox_open_counts_for(&self, targets: &[String], cap: u64) -> Result<Vec<(String, u64)>> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let targets = targets.to_vec();
        self.block(async move {
            let rows: Vec<(String, i64)> = sqlx::query_as(sql(format!(
                "SELECT n.name, (SELECT count(*) FROM (SELECT 1 FROM {o} \
                   WHERE target = n.name AND state IN ('pending', 'retry', 'inflight') LIMIT $2) x) \
                 FROM unnest($1::text[]) WITH ORDINALITY AS n(name, ord) ORDER BY n.ord",
                o = t.outbox
            )))
            .bind(&targets)
            .bind(cap.min(i64::MAX as u64) as i64)
            .fetch_all(&pool)
            .await?;
            Ok(rows.into_iter().map(|(t, n)| (t, n.max(0) as u64)).collect())
        })
    }

    /// From the `outbox_stats` index (0014): the targets that have such rows
    /// by a skip scan (one probe per target), then each (target, state)
    /// counted up to `cap` from an index range. Delivered and cancelled rows
    /// are not in the index. The counts are index-only on an outbox vacuumed
    /// since its rows last changed; on a busy one PostgreSQL also reads up
    /// to `cap` heap rows per (target, state), so keep `cap` small
    /// ([`OUTBOX_STATS_CAP`](crate::outbox::OUTBOX_STATS_CAP)).
    fn outbox_state_counts(&self, cap: u64) -> Result<Vec<(String, OutboxState, u64)>> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        let states: Vec<String> = STATS_STATES.iter().map(|s| s.as_str().to_string()).collect();
        self.block(async move {
            const IN: &str = "state IN ('pending', 'inflight', 'retry', 'dead')";
            let rows: Vec<(String, String, i64)> = sqlx::query_as(sql(format!(
                "WITH RECURSIVE t(target, d) AS ( \
                   (SELECT target, 1 FROM {o} WHERE {IN} ORDER BY target LIMIT 1) \
                   UNION ALL \
                   SELECT (SELECT o.target FROM {o} o WHERE o.{IN} AND o.target > t.target ORDER BY o.target LIMIT 1), \
                          t.d + 1 \
                   FROM t WHERE t.target IS NOT NULL AND t.d < $3) \
                 SELECT t.target, s.state, \
                   (SELECT count(*) FROM (SELECT 1 FROM {o} o \
                      WHERE o.target = t.target AND o.state = s.state AND o.{IN} LIMIT $1) x) AS n \
                 FROM t CROSS JOIN unnest($2::text[]) AS s(state) \
                 WHERE t.target IS NOT NULL ORDER BY t.target, s.state",
                o = t.outbox
            )))
            .bind(cap.min(i64::MAX as u64) as i64)
            .bind(&states)
            .bind(MAX_STATS_TARGETS)
            .fetch_all(&pool)
            .await?;
            rows.into_iter()
                .filter(|(_, _, n)| *n > 0)
                .map(|(target, state, n)| Ok((target, state.parse()?, n as u64)))
                .collect()
        })
    }

    fn outbox_vacuum(&self, before_ms: i64) -> Result<u64> {
        let pool = self.pool.clone();
        let t = Tables::of(self);
        self.block(async move {
            let mut tx = pool.begin().await?;
            let n = sqlx::query(sql(format!(
                "DELETE FROM {} WHERE state IN ('delivered', 'dead', 'cancelled') AND created_at < $1",
                t.outbox
            )))
            .bind(before_ms)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            // The attempt log refuses DELETE except to retention (0011, as 0008).
            sqlx::query("SELECT set_config('onto.retention', 'on', true)").execute(&mut *tx).await?;
            sqlx::query(sql(format!("DELETE FROM {} WHERE ts < $1", t.attempts)))
                .bind(before_ms)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(n)
        })
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
    changes: String,
    changes_meta: String,
    leases: String,
    consumers: String,
    timers: String,
    outbox: String,
    attempts: String,
    runs: String,
    /// The tenant schema's `now_ms()` (the database clock).
    now: String,
}

/// One change record to insert (its seq is the commit's).
#[derive(Clone)]
struct ChangeRow {
    n: i32,
    ts: i64,
    workspace: String,
    kind: String,
    op: String,
    target: String,
    doc: String,
}

type ChangeColumns = (Vec<i32>, Vec<i64>, Vec<String>, Vec<String>, Vec<String>, Vec<String>, Vec<String>);

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
            changes: s.t("changes"),
            changes_meta: s.t("changes_meta"),
            leases: s.t("leases"),
            consumers: s.t("consumers"),
            timers: s.t("timers"),
            outbox: s.t("outbox"),
            attempts: s.t("delivery_attempts"),
            runs: s.t("automation_runs"),
            now: s.t("now_ms()"),
        }
    }
}

/// The columns of an outbox row, in [`outbox_row`]'s order.
const OUTBOX_COLS: &str = "id, created_at, kind, target, dedupe, source, workspace, receipt, run, cursor_seq, \
     cursor_n, event_type, payload::text, cause::text, state, attempts, next_at, claim, lease_until, last_status, \
     last_error, delivered_at, otel_trace_id";

fn outbox_row(r: &sqlx::postgres::PgRow) -> Result<OutboxRow> {
    let kind: String = r.try_get(2)?;
    let (seq, n): (Option<i64>, Option<i32>) = (r.try_get(9)?, r.try_get(10)?);
    let payload: String = r.try_get(12)?;
    let cause: Option<String> = r.try_get(13)?;
    let state: String = r.try_get(14)?;
    let status: Option<i32> = r.try_get(19)?;
    Ok(OutboxRow {
        id: r.try_get(0)?,
        created_at: r.try_get(1)?,
        kind: kind.parse()?,
        target: r.try_get(3)?,
        dedupe: r.try_get(4)?,
        source: r.try_get(5)?,
        workspace: r.try_get(6)?,
        receipt: r.try_get(7)?,
        run: r.try_get(8)?,
        cursor: seq.map(|s| Cursor::new(s as u64, n.unwrap_or(0).max(0) as u32)),
        event_type: r.try_get(11)?,
        payload: parse(&payload)?,
        cause: cause.as_deref().map(parse).transpose()?,
        state: state.parse().map_err(|_| Error::Storage(format!("corrupt row: outbox state '{state}'")))?,
        attempts: r.try_get::<i32, _>(15)?.max(0) as u32,
        next_at: r.try_get(16)?,
        claim: r.try_get(17)?,
        lease_until: r.try_get(18)?,
        last_status: status.map(|s| s.clamp(0, u16::MAX as i32) as u16),
        last_error: r.try_get(20)?,
        delivered_at: r.try_get(21)?,
        otel_trace_id: r.try_get(22)?,
    })
}

/// The insertable fields of outbox rows as one JSON array (`jsonb_to_recordset`).
fn outbox_json(rows: &[OutboxRow]) -> Result<String> {
    let v: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id, "kind": r.kind.as_str(), "target": r.target, "dedupe": r.dedupe,
                "source": r.source, "workspace": r.workspace, "receipt": r.receipt, "run": r.run,
                "cursor_seq": r.cursor.map(|c| c.seq as i64), "cursor_n": r.cursor.map(|c| c.n.min(i32::MAX as u32)),
                "event_type": r.event_type, "payload": r.payload, "cause": r.cause, "otel_trace_id": r.otel_trace_id,
            })
        })
        .collect();
    Ok(serde_json::to_string(&v)?)
}

/// Insert outbox rows (`doc` from [`outbox_json`]) with the database
/// clock's times. `skip_existing`: rows whose key exists are skipped;
/// otherwise a duplicate fails the transaction (unique violation, `Conflict`).
async fn insert_outbox(
    tx: &mut sqlx::transaction::Transaction<'_, sqlx::Postgres>,
    t: &Tables,
    doc: &str,
    skip_existing: bool,
) -> Result<usize> {
    let n = sqlx::query(sql(format!(
        "INSERT INTO {o} (id, created_at, kind, target, dedupe, source, workspace, receipt, run, cursor_seq, cursor_n, \
         event_type, payload, cause, state, attempts, next_at, otel_trace_id) \
         SELECT r.id, {now}, r.kind, r.target, r.dedupe, r.source, r.workspace, r.receipt, r.run, r.cursor_seq, \
         r.cursor_n, r.event_type, coalesce(r.payload, 'null'::jsonb), r.cause, 'pending', 0, {now}, r.otel_trace_id \
         FROM jsonb_to_recordset($1::jsonb) AS r(id text, kind text, target text, dedupe text, source text, \
         workspace text, receipt text, run text, cursor_seq bigint, cursor_n int, event_type text, payload jsonb, \
         cause jsonb, otel_trace_id text){}",
        if skip_existing { " ON CONFLICT DO NOTHING" } else { "" },
        o = t.outbox,
        now = t.now
    )))
    .bind(doc)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(n as usize)
}

/// The columns of a run row, in [`run_row`]'s order.
const RUN_COLS: &str = "state, epoch, started_at, finished_at, doc::text";

/// A run row: its document, with the columns the store keeps authoritative.
fn run_row(r: &sqlx::postgres::PgRow) -> Result<RunRow> {
    let state: String = r.try_get(0)?;
    let doc: String = r.try_get(4)?;
    let mut row: RunRow =
        serde_json::from_str(&doc).map_err(|e| Error::Storage(format!("corrupt automation run: {e}")))?;
    row.state = state.parse().map_err(|_| Error::Storage(format!("unknown run state '{state}'")))?;
    row.epoch = r.try_get::<i64, _>(1)?.max(0) as u64;
    row.started_at = r.try_get(2)?;
    row.finished_at = r.try_get(3)?;
    Ok(row)
}

/// Run rows as one JSON array for `jsonb_to_recordset`.
fn runs_json(rows: &[RunRow]) -> Result<String> {
    let v: Vec<Value> = rows
        .iter()
        .map(|r| {
            Ok(json!({
                "id": r.id,
                "automation": r.automation,
                "trigger_key": r.trigger_key,
                "closure_hash": r.closure_hash,
                "state": r.state.as_str(),
                "depth": r.depth,
                "epoch": r.epoch.min(i64::MAX as u64),
                "final": r.state.is_final(),
                "doc": serde_json::to_value(r)?,
            }))
        })
        .collect::<Result<_>>()?;
    Ok(serde_json::to_string(&v)?)
}

/// Settle runs inside a commit: each row is replaced only while it is still
/// `running`. Returns how many were.
async fn settle_runs(
    tx: &mut sqlx::transaction::Transaction<'_, sqlx::Postgres>,
    t: &Tables,
    doc: &str,
) -> Result<usize> {
    let n = sqlx::query(sql(format!(
        "UPDATE {r} SET state = x.state, epoch = x.epoch, closure_hash = x.closure_hash, \
         finished_at = CASE WHEN x.final THEN {now} END, doc = x.doc \
         FROM jsonb_to_recordset($1::jsonb) AS x(id text, state text, epoch bigint, closure_hash text, \
         final boolean, doc jsonb) WHERE {r}.id = x.id AND {r}.state = 'running'",
        r = t.runs,
        now = t.now
    )))
    .bind(doc)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(n as usize)
}

/// An attempt as the attempt log stores it.
#[derive(serde::Deserialize)]
struct AttemptDoc {
    state: OutboxState,
    #[serde(default)]
    result: Attempt,
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
    use super::{check_pgvector, check_server, halfvec_text, sys_schema, tenant_schema, wake_channel};

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

    #[test]
    fn sqlx_errors_map_to_conflict_unavailable_or_storage() {
        use super::{ErrorClass, sql, sqlstate_class};
        use crate::Error;
        use crate::sqlx;
        assert_eq!(sqlstate_class(Some("23505")), ErrorClass::Conflict);
        for c in ["08006", "08001", "53300", "57P01", "57P03"] {
            assert_eq!(sqlstate_class(Some(c)), ErrorClass::Unavailable, "{c}");
        }
        for c in [Some("42P01"), Some("23503"), Some("40001"), None] {
            assert_eq!(sqlstate_class(c), ErrorClass::Storage, "{c:?}");
        }
        assert!(matches!(Error::from(sqlx::Error::PoolTimedOut), Error::Unavailable(_)));
        assert!(matches!(Error::from(sqlx::Error::PoolClosed), Error::Unavailable(_)));
        assert!(matches!(Error::from(sqlx::Error::Io(std::io::Error::other("reset"))), Error::Unavailable(_)));
        assert!(matches!(Error::from(sqlx::Error::RowNotFound), Error::Storage(_)));
        // Against a real server (behind DATABASE_URL): a duplicate key and a pool timeout.
        let Some(url) = ["ONTO_TEST_DATABASE_URL", "DATABASE_URL"]
            .iter()
            .find_map(|v| std::env::var(v).ok().filter(|u| !u.trim().is_empty()))
        else {
            eprintln!("skipped the PostgreSQL half: set DATABASE_URL");
            return;
        };
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_millis(200))
                .connect(&url)
                .await
                .unwrap();
            let mut c = pool.acquire().await.unwrap();
            sqlx::raw_sql(sql("CREATE TEMP TABLE dup (k INT PRIMARY KEY); INSERT INTO dup VALUES (1)".into()))
                .execute(&mut *c)
                .await
                .unwrap();
            let e = sqlx::raw_sql(sql("INSERT INTO dup VALUES (1)".into())).execute(&mut *c).await.unwrap_err();
            assert!(matches!(Error::from(e), Error::Conflict(ref m) if m.contains("duplicate")));
            // The only connection is held, so the pool times out.
            let e = pool.acquire().await.unwrap_err();
            assert!(matches!(Error::from(e), Error::Unavailable(_)));
            drop(c);
            pool.close().await;
        });
    }

    #[test]
    fn the_shared_schema_is_no_tenant_s() {
        assert_eq!(sys_schema("onto_"), "onto_sys");
        assert_eq!(sys_schema("onto"), "onto_sys");
        assert_eq!(sys_schema("o120_ab12"), "o120_ab12_sys");
        assert_eq!(wake_channel("onto_"), "onto-wake");
        // No tenant's schema (its own notice channel) is the wake channel.
        for (prefix, t) in [("onto_", "wake"), ("onto", "_wake"), ("onto_", "WAKE"), ("o120_ab12_", "wake")] {
            let schema = tenant_schema(prefix, t).unwrap();
            assert_ne!(schema, wake_channel(prefix), "{prefix}{t}");
        }
        for t in ["sys", "SYS", "Sys"] {
            let e = tenant_schema("onto_", t).unwrap_err().to_string();
            assert!(e.contains("reserved") && e.contains("onto_sys"), "{e}");
        }
        assert!(tenant_schema("onto", "_sys").is_err(), "onto + _sys is onto_sys too");
        assert_eq!(tenant_schema("onto_", "sysops").unwrap(), "onto_sysops");
        assert_eq!(tenant_schema("onto_", "_sys").unwrap(), "onto__sys");
    }
}
