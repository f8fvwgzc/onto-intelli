//! Single-file SQLite store (WAL mode). SQLite is compiled in, so the binary
//! has no system dependencies.

use std::path::Path;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::Value;

use super::{Snapshot, Store, StoreStats, WriteBatch};
use crate::model::{Event, EventFilter};
use crate::time::now_ms;
use crate::{Error, Result};

const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
CREATE TABLE IF NOT EXISTS entities (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    workspace  TEXT NOT NULL,
    doc        TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS entities_ws_kind ON entities(workspace, kind);
CREATE TABLE IF NOT EXISTS links (
    src TEXT NOT NULL,
    rel TEXT NOT NULL,
    dst TEXT NOT NULL,
    doc TEXT NOT NULL,
    PRIMARY KEY (src, rel, dst)
);
CREATE INDEX IF NOT EXISTS links_dst ON links(dst, rel);
CREATE TABLE IF NOT EXISTS events (
    id        TEXT PRIMARY KEY,
    ts        INTEGER NOT NULL,
    workspace TEXT NOT NULL,
    actor     TEXT NOT NULL,
    action    TEXT NOT NULL,
    target    TEXT,
    doc       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_ws_ts ON events(workspace, ts);
CREATE INDEX IF NOT EXISTS events_target ON events(target, ts);
CREATE INDEX IF NOT EXISTS events_actor ON events(actor, ts);
CREATE TABLE IF NOT EXISTS kv (
    ns         TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    expires_at INTEGER,
    PRIMARY KEY (ns, key)
);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT OR IGNORE INTO meta(key, value) VALUES ('format', '1');
"#;

pub struct SqliteStore {
    conn: Mutex<Connection>,
    path: String,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self { conn: Mutex::new(conn), path: path.display().to_string() })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self { conn: Mutex::new(conn), path: ":memory:".into() })
    }

    /// Delete expired KV rows and events older than `keep_events_ms`.
    pub fn vacuum(&self, keep_events_ms: Option<i64>) -> Result<(usize, usize)> {
        let conn = self.conn.lock();
        let kv = conn.execute("DELETE FROM kv WHERE expires_at IS NOT NULL AND expires_at <= ?1", params![now_ms()])?;
        let ev = match keep_events_ms {
            Some(keep) => conn.execute("DELETE FROM events WHERE ts < ?1", params![now_ms() - keep])?,
            None => 0,
        };
        Ok((kv, ev))
    }
}

fn parse<T: serde::de::DeserializeOwned>(doc: &str) -> Result<T> {
    serde_json::from_str(doc).map_err(|e| Error::Storage(format!("corrupt row: {e}")))
}

impl Store for SqliteStore {
    fn load(&self) -> Result<Snapshot> {
        let conn = self.conn.lock();
        let mut snap = Snapshot::default();
        let mut stmt = conn.prepare("SELECT doc FROM entities")?;
        for doc in stmt.query_map([], |r| r.get::<_, String>(0))? {
            snap.entities.push(parse(&doc?)?);
        }
        let mut stmt = conn.prepare("SELECT doc FROM links")?;
        for doc in stmt.query_map([], |r| r.get::<_, String>(0))? {
            snap.links.push(parse(&doc?)?);
        }
        Ok(snap)
    }

    fn write(&self, b: &WriteBatch) -> Result<()> {
        if b.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        for (src, rel, dst) in &b.del_links {
            tx.execute("DELETE FROM links WHERE src = ?1 AND rel = ?2 AND dst = ?3", params![src, rel, dst])?;
        }
        for id in &b.del_entities {
            tx.execute("DELETE FROM entities WHERE id = ?1", params![id])?;
            tx.execute("DELETE FROM links WHERE src = ?1 OR dst = ?1", params![id])?;
        }
        for e in &b.put_entities {
            tx.execute(
                "INSERT INTO entities(id, kind, workspace, doc, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, workspace = excluded.workspace,
                 doc = excluded.doc, updated_at = excluded.updated_at",
                params![e.id, e.kind, e.workspace, serde_json::to_string(e)?, e.updated_at],
            )?;
        }
        for l in &b.put_links {
            tx.execute(
                "INSERT INTO links(src, rel, dst, doc) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(src, rel, dst) DO UPDATE SET doc = excluded.doc",
                params![l.from, l.rel, l.to, serde_json::to_string(l)?],
            )?;
        }
        for ev in &b.events {
            tx.execute(
                "INSERT OR IGNORE INTO events(id, ts, workspace, actor, action, target, doc) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![ev.id, ev.ts, ev.workspace, ev.actor, ev.action, ev.target, serde_json::to_string(ev)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn events(&self, f: &EventFilter) -> Result<Vec<Event>> {
        let mut sql = String::from("SELECT doc FROM events WHERE 1 = 1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        let mut push = |clause: &str, v: rusqlite::types::Value| {
            args.push(v);
            sql.push_str(&format!(" AND {clause} ?{}", args.len()));
        };
        if let Some(w) = &f.workspace {
            push("workspace =", w.clone().into());
        }
        if let Some(a) = &f.actor {
            push("actor =", a.clone().into());
        }
        if let Some(t) = &f.target {
            push("target =", t.clone().into());
        }
        if let Some(s) = f.since {
            push("ts >=", s.into());
        }
        if let Some(u) = f.until {
            push("ts <=", u.into());
        }
        if let Some(a) = &f.action {
            let prefix = a.trim_end_matches('.');
            args.push(prefix.to_string().into());
            let n = args.len();
            sql.push_str(&format!(" AND (action = ?{n} OR substr(action, 1, length(?{n}) + 1) = ?{n} || '.')"));
        }
        let limit = f.limit.unwrap_or(50).min(10_000);
        sql.push_str(&format!(" ORDER BY ts DESC, id DESC LIMIT {limit}"));
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args.iter()), |r| r.get::<_, String>(0))?;
        rows.map(|doc| parse(&doc?)).collect()
    }

    fn kv_put(&self, ns: &str, key: &str, value: &Value, expires_at: Option<i64>) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO kv(ns, key, value, expires_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(ns, key) DO UPDATE SET value = excluded.value, expires_at = excluded.expires_at",
            params![ns, key, serde_json::to_string(value)?, expires_at],
        )?;
        Ok(())
    }

    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<Value>> {
        let conn = self.conn.lock();
        let row: Option<(String, Option<i64>)> = conn
            .query_row("SELECT value, expires_at FROM kv WHERE ns = ?1 AND key = ?2", params![ns, key], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        match row {
            Some((_, Some(exp))) if exp <= now_ms() => {
                conn.execute("DELETE FROM kv WHERE ns = ?1 AND key = ?2", params![ns, key])?;
                Ok(None)
            }
            Some((v, _)) => Ok(Some(parse(&v)?)),
            None => Ok(None),
        }
    }

    fn kv_delete(&self, ns: &str, key: &str) -> Result<()> {
        self.conn.lock().execute("DELETE FROM kv WHERE ns = ?1 AND key = ?2", params![ns, key])?;
        Ok(())
    }

    fn kv_scan(&self, ns: &str, prefix: &str) -> Result<Vec<(String, Value)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT key, value FROM kv WHERE ns = ?1 AND substr(key, 1, length(?2)) = ?2
             AND (expires_at IS NULL OR expires_at > ?3) ORDER BY key",
        )?;
        let rows =
            stmt.query_map(params![ns, prefix, now_ms()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (k, v) = row?;
            Ok((k, parse(&v)?))
        })
        .collect()
    }

    fn stats(&self) -> Result<StoreStats> {
        let conn = self.conn.lock();
        let events: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        let kv: i64 = conn.query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))?;
        Ok(StoreStats {
            backend: "sqlite".into(),
            events: events as u64,
            kv_entries: kv as u64,
            path: Some(self.path.clone()),
        })
    }
}
