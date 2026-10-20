//! PostgreSQL store: one PostgreSQL schema per tenant, shared by any number
//! of processes (replicas embedding the library, several onto servers).
//!
//! * Writes commit optimistically against the commit head (`head` row
//!   locked `FOR UPDATE`); a writer that is behind gets `Error::Stale`, the
//!   engine catches up from `commits` and retries.
//! * Every state-changing commit sends `NOTIFY` on the tenant's channel
//!   after it is durable; followers in other processes catch up at once.
//!   A periodic poll covers notices lost while a listener reconnects.
//! * The engine is synchronous: queries run on a Tokio runtime (an own one,
//!   or the runtime of a pool handed in by the embedding application) and
//!   the caller waits, yielding its worker thread when it is on one.

use std::future::Future;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};
use sqlx::postgres::{PgListener, PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, Row};
use tokio::runtime::{Handle, Runtime, RuntimeFlavor};

use super::{
    CommitRecord, NoticeFn, REPLICATED_KV, Snapshot, Store, StoreNotice, StoreStats, VersionRecord, WriteBatch,
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

#[derive(Clone, Debug)]
pub struct PgOptions {
    /// The tenant's PostgreSQL schema is `{schema_prefix}{tenant}`.
    pub schema_prefix: String,
    pub max_connections: u32,
    pub connect_timeout: Duration,
    /// Run a LISTEN connection that tells followers about other processes'
    /// commits (otherwise they only poll).
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
        let pool = run_on(rt.handle(), false, async move {
            Ok(PgPoolOptions::new().max_connections(max).acquire_timeout(timeout).connect(&url_s).await?)
        })?;
        Ok(Self { pool, exec: Exec::Own(Arc::new(OwnedRuntime(Mutex::new(Some(rt))))) })
    }

    /// Use the application's pool (e.g. its `sqlx::PgPool`). Must be called
    /// inside the Tokio runtime that owns the pool; queries run there.
    pub fn from_pool(pool: PgPool) -> Result<Self> {
        let handle = Handle::try_current().map_err(|_| {
            Error::Config("PgShared::from_pool must be called inside the Tokio runtime that owns the pool".into())
        })?;
        Ok(Self { pool, exec: Exec::Shared(handle) })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The store of one tenant (its own PostgreSQL schema), created if missing.
    pub fn tenant(&self, tenant: &str, opts: PgOptions) -> Result<PgStore> {
        PgStore::finish(self.pool.clone(), tenant, opts, self.exec.clone())
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
    opts: PgOptions,
    subscribers: Arc<Mutex<Vec<NoticeFn>>>,
    listener: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for PgStore {
    fn drop(&mut self) {
        if let Some(h) = self.listener.lock().take() {
            h.abort();
        }
    }
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

    fn finish(pool: PgPool, tenant: &str, opts: PgOptions, exec: Exec) -> Result<Self> {
        let store = Self {
            schema: tenant_schema(&opts.schema_prefix, tenant)?,
            tenant: tenant.to_string(),
            origin: new_id("proc"),
            pool,
            exec,
            opts,
            subscribers: Arc::new(Mutex::new(Vec::new())),
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

    fn migrate(&self) -> Result<()> {
        let pool = self.pool.clone();
        let s = format!("\"{}\"", self.schema);
        let lock_key = self.schema.clone();
        self.block(async move {
            let mut tx = pool.begin().await?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))").bind(&lock_key).execute(&mut *tx).await?;
            let ddl = [
                format!("CREATE SCHEMA IF NOT EXISTS {s}"),
                format!("CREATE TABLE IF NOT EXISTS {s}.meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"),
                format!(
                    "CREATE TABLE IF NOT EXISTS {s}.head (id INT PRIMARY KEY CHECK (id = 1), seq BIGINT NOT NULL)"
                ),
                format!("INSERT INTO {s}.head (id, seq) VALUES (1, 0) ON CONFLICT DO NOTHING"),
                format!(
                    "CREATE TABLE IF NOT EXISTS {s}.entities (id TEXT PRIMARY KEY, kind TEXT NOT NULL, \
                     workspace TEXT NOT NULL, doc JSONB NOT NULL, updated_at BIGINT NOT NULL)"
                ),
                format!("CREATE INDEX IF NOT EXISTS entities_ws_kind ON {s}.entities (workspace, kind)"),
                format!(
                    "CREATE TABLE IF NOT EXISTS {s}.links (src TEXT NOT NULL, rel TEXT NOT NULL, dst TEXT NOT NULL, \
                     doc JSONB NOT NULL, PRIMARY KEY (src, rel, dst))"
                ),
                format!("CREATE INDEX IF NOT EXISTS links_dst ON {s}.links (dst, rel)"),
                format!(
                    "CREATE TABLE IF NOT EXISTS {s}.events (id TEXT PRIMARY KEY, ts BIGINT NOT NULL, \
                     workspace TEXT NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL, target TEXT, doc JSONB NOT NULL)"
                ),
                format!("CREATE INDEX IF NOT EXISTS events_ws_ts ON {s}.events (workspace, ts)"),
                format!("CREATE INDEX IF NOT EXISTS events_target ON {s}.events (target, ts)"),
                format!("CREATE INDEX IF NOT EXISTS events_actor ON {s}.events (actor, ts)"),
                format!("CREATE INDEX IF NOT EXISTS events_action ON {s}.events (action, ts)"),
                format!(
                    "CREATE TABLE IF NOT EXISTS {s}.kv (ns TEXT NOT NULL, key TEXT NOT NULL, value JSONB NOT NULL, \
                     expires_at BIGINT, PRIMARY KEY (ns, key))"
                ),
                format!(
                    "CREATE TABLE IF NOT EXISTS {s}.entity_versions (seq BIGSERIAL PRIMARY KEY, id TEXT NOT NULL, \
                     revision BIGINT NOT NULL, ts BIGINT NOT NULL, doc JSONB NOT NULL)"
                ),
                format!("CREATE INDEX IF NOT EXISTS entity_versions_id ON {s}.entity_versions (id, seq)"),
                format!("CREATE INDEX IF NOT EXISTS entity_versions_ts ON {s}.entity_versions (ts)"),
                format!(
                    "CREATE TABLE IF NOT EXISTS {s}.commits (seq BIGINT PRIMARY KEY, ts BIGINT NOT NULL, \
                     origin TEXT NOT NULL, doc JSONB NOT NULL)"
                ),
                format!("INSERT INTO {s}.meta (key, value) VALUES ('format', '1') ON CONFLICT DO NOTHING"),
            ];
            for stmt in ddl {
                sqlx::query(sql(stmt)).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            Ok(())
        })
    }

    /// Drop the tenant's schema and everything in it (tests, offboarding).
    pub fn drop_tenant(&self) -> Result<()> {
        let pool = self.pool.clone();
        let stmt = format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema);
        self.block(async move {
            sqlx::query(sql(stmt)).execute(&pool).await?;
            Ok(())
        })
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
                Some(keep) => sqlx::query(sql(format!("DELETE FROM {ev} WHERE ts < $1")))
                    .bind(now - keep)
                    .execute(&pool)
                    .await?
                    .rows_affected(),
                None => 0,
            };
            Ok((a, b))
        })
    }

    fn channel(&self) -> String {
        self.schema.clone()
    }

    fn start_listener(&self) {
        if !self.opts.listen || self.listener.lock().is_some() {
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
        let pool = self.pool.clone();
        let channel = self.channel();
        let me = self.origin.clone();
        let (h, _) = self.handle();
        let task = h.spawn(async move {
            loop {
                let mut listener = match PgListener::connect_with(&pool).await {
                    Ok(l) => l,
                    Err(_) => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                if listener.listen(&channel).await.is_err() {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                // Anything committed while we were not listening is caught up now.
                if tx.send(StoreNotice::Resync).is_err() {
                    return;
                }
                loop {
                    match listener.try_recv().await {
                        Ok(Some(n)) => {
                            let Ok(v) = serde_json::from_str::<Value>(n.payload()) else { continue };
                            let origin = v["o"].as_str().unwrap_or_default().to_string();
                            if origin == me {
                                continue;
                            }
                            let notice = match v["t"].as_str() {
                                Some("kv") => StoreNotice::Kv {
                                    ns: v["ns"].as_str().unwrap_or_default().into(),
                                    key: v["key"].as_str().unwrap_or_default().into(),
                                    origin,
                                },
                                _ => StoreNotice::Commit { seq: v["seq"].as_u64().unwrap_or(0), origin },
                            };
                            if tx.send(notice).is_err() {
                                return;
                            }
                        }
                        // Connection lost; notices may be missing until it is back.
                        Ok(None) => {
                            if tx.send(StoreNotice::Resync).is_err() {
                                return;
                            }
                        }
                        Err(_) => break,
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
        *self.listener.lock() = Some(task);
    }

    /// Bulk-load through COPY (fast imports and benchmarks): no events and no
    /// versions; one commit tells other processes to reload.
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
                "INSERT INTO {te} (id, kind, workspace, doc, updated_at) SELECT id, kind, workspace, doc, updated_at \
                 FROM onto_import_e ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, workspace = excluded.workspace, \
                 doc = excluded.doc, updated_at = excluded.updated_at"
            )))
            .execute(&mut *tx)
            .await?;
            sqlx::query(sql(format!(
                "INSERT INTO {tl} (src, rel, dst, doc) SELECT src, rel, dst, doc FROM onto_import_l \
                 ON CONFLICT (src, rel, dst) DO UPDATE SET doc = excluded.doc"
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
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(&channel)
                .bind(json!({ "t": "c", "seq": seq, "o": origin }).to_string())
                .execute(&mut *tx)
                .await?;
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
        let put_e: Vec<(String, String, String, String, i64)> = b
            .put_entities
            .iter()
            .map(|e| Ok((e.id.clone(), e.kind.clone(), e.workspace.clone(), serde_json::to_string(e)?, e.updated_at)))
            .collect::<Result<_>>()?;
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
            for chunk in put_e.chunks(CHUNK) {
                let (mut ids, mut kinds, mut wss, mut docs, mut ats) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for (id, kind, ws, doc, at) in chunk.iter().cloned() {
                    ids.push(id);
                    kinds.push(kind);
                    wss.push(ws);
                    docs.push(doc);
                    ats.push(at);
                }
                sqlx::query(sql(format!(
                    "INSERT INTO {} (id, kind, workspace, doc, updated_at) \
                     SELECT id, kind, ws, doc::jsonb, at FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::bigint[]) \
                     AS r(id, kind, ws, doc, at) \
                     ON CONFLICT (id) DO UPDATE SET kind = excluded.kind, workspace = excluded.workspace, \
                     doc = excluded.doc, updated_at = excluded.updated_at",
                    tables.entities
                )))
                .bind(ids)
                .bind(kinds)
                .bind(wss)
                .bind(docs)
                .bind(ats)
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
                sqlx::query("SELECT pg_notify($1, $2)")
                    .bind(&channel)
                    .bind(json!({ "t": "c", "seq": seq, "o": rec.origin }).to_string())
                    .execute(&mut *tx)
                    .await?;
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
                sqlx::query("SELECT pg_notify($1, $2)")
                    .bind(channel)
                    .bind(json!({ "t": "kv", "ns": ns, "key": key, "o": origin }).to_string())
                    .execute(&mut *tx)
                    .await?;
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
                sqlx::query("SELECT pg_notify($1, $2)")
                    .bind(channel)
                    .bind(json!({ "t": "kv", "ns": ns, "key": key, "o": origin }).to_string())
                    .execute(&mut *tx)
                    .await?;
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
        let (te, tk) = (self.t("events"), self.t("kv"));
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
            Ok(StoreStats { backend: "postgres".into(), events, kv_entries: kv as u64, path: Some(schema) })
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

    fn bulk_import(&self, entities: &[Entity], links: &[Link]) -> Result<u64> {
        self.copy_import(entities.to_vec(), links.to_vec())
    }
}

/// id, ts, workspace, actor, action, target, document.
type EventRow = (String, i64, String, String, String, Option<String>, String);

#[derive(Clone)]
struct Tables {
    head: String,
    entities: String,
    links: String,
    events: String,
    versions: String,
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
    use super::tenant_schema;

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
