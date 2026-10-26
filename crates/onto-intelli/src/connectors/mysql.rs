//! MySQL and MariaDB metadata from `information_schema`: databases (as
//! schemas), tables and views, columns (types, keys, comments, `ENUM`
//! values), foreign keys, view definitions and the tables they read,
//! `SELECT` grants and, when readable, the most-run statements of
//! `performance_schema`. One read-only session with a statement timeout; no
//! table is read for the snapshot.
//!
//! Rows of synced tables are read a page per statement in key order (keyset
//! paging) with only the key and mapped columns, as JSON built by the
//! server (`JSON_OBJECT`). Identifiers are quoted; values are bound.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use onto_core::catalog::{
    CatalogSnapshot, ColumnInfo, ForeignKey, QueryStat, RelKind, RelationInfo, SchemaInfo, SourceInfo, ViewDep,
};
use onto_core::sqlx;
use onto_core::{Error, Result};
use serde_json::{Map, Value};
use sqlx_mysql::{MySql, MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlSslMode};

use super::restricted::{self, Target};
use super::{Connector, ConnectorSpec, RowPage, RowRequest};

pub const SCHEMES: [&str; 2] = ["mysql", "mariadb"];
/// Query parameters a restricted URL may carry.
pub const RESTRICTED_PARAMS: [&str; 2] = ["ssl-mode", "sslmode"];
/// Never crawled: the server's own databases.
const SYSTEM: &str = "('mysql', 'information_schema', 'performance_schema', 'sys')";

/// Check a URL a project credential registers ([`restricted::check`]).
pub async fn check_url(raw: &str, allow_private: bool) -> Result<Target> {
    restricted::check("mysql", raw, &SCHEMES, 3306, true, &RESTRICTED_PARAMS, allow_private).await
}

fn ssl_mode(t: &Target) -> Result<MySqlSslMode> {
    match t.params.get("ssl-mode").or_else(|| t.params.get("sslmode")) {
        None => Ok(MySqlSslMode::Preferred),
        Some(v) => v.parse().map_err(|_| Error::invalid(format!("mysql URL: ssl-mode {v} is not a mode"))),
    }
}

/// `` `name` ``, with embedded backquotes doubled.
pub fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// A string literal for `JSON_OBJECT` keys.
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// The statement for one page of `req`: `?`s are the `after` key values
/// (when given), then the limit.
pub fn rows_sql(req: &RowRequest) -> Result<String> {
    let (schema, table) = req
        .table
        .split_once('.')
        .filter(|(s, t)| !s.is_empty() && !t.is_empty())
        .ok_or_else(|| Error::invalid(format!("table {} must be named schema.table", req.table)))?;
    if req.key.is_empty() || req.key.iter().chain(&req.columns).any(|c| c.is_empty()) {
        return Err(Error::invalid(format!("reading {}: key and column names must not be empty", req.table)));
    }
    let mut names = req.key.clone();
    for c in &req.columns {
        if !names.contains(c) {
            names.push(c.clone());
        }
    }
    let object: Vec<String> = names.iter().map(|n| format!("{}, {}", literal(n), quote_ident(n))).collect();
    let keys: Vec<String> = req.key.iter().map(|k| quote_ident(k)).collect();
    let mut cond: Vec<String> = keys.iter().map(|k| format!("{k} IS NOT NULL")).collect();
    if req.after.is_some() {
        cond.push(format!("({}) > ({})", keys.join(", "), vec!["?"; keys.len()].join(", ")));
    }
    Ok(format!(
        "SELECT CAST(JSON_OBJECT({}) AS CHAR) FROM {}.{} WHERE {} ORDER BY {} LIMIT ?",
        object.join(", "),
        quote_ident(schema),
        quote_ident(table),
        cond.join(" AND "),
        keys.join(", ")
    ))
}

/// The labels of a column type `enum('a','b')`.
pub fn enum_labels(column_type: &str) -> Vec<String> {
    let Some(inner) = column_type.strip_prefix("enum(").and_then(|s| s.strip_suffix(')')) else { return Vec::new() };
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('\'', false) => quoted = true,
            ('\'', true) if chars.peek() == Some(&'\'') => {
                chars.next();
                cur.push('\'');
            }
            ('\'', true) => {
                quoted = false;
                out.push(std::mem::take(&mut cur));
            }
            (_, true) => cur.push(c),
            _ => {}
        }
    }
    out
}

/// `'app'@'%'` → `app`.
fn grantee(g: &str) -> String {
    g.split('@').next().unwrap_or(g).trim_matches(['\'', '`', '"']).to_string()
}

#[derive(Default)]
pub struct MySqlConnector {
    pool: parking_lot::Mutex<Option<MySqlPool>>,
    allow_private: bool,
}

impl MySqlConnector {
    pub fn new(allow_private: bool) -> Self {
        Self { pool: Default::default(), allow_private }
    }

    async fn connect(&self, spec: &ConnectorSpec, url: &str) -> Result<(MySqlPool, Target)> {
        let t = if spec.restricted {
            check_url(url, self.allow_private)
                .await
                .map_err(|e| Error::invalid(format!("connector {}: {e}", spec.name)))?
        } else {
            restricted::parse("mysql", url, &SCHEMES, 3306, false)?
        };
        let mode = ssl_mode(&t)?;
        // The pinned address unless the certificate must match the name.
        let host = if matches!(mode, MySqlSslMode::VerifyIdentity) { t.host.clone() } else { t.connect_host() };
        let mut opts = MySqlConnectOptions::new()
            .host(&host)
            .port(t.port)
            .username(&t.user)
            .password(&t.password)
            .ssl_mode(mode)
            .charset("utf8mb4");
        if !t.database.is_empty() {
            opts = opts.database(&t.database);
        }
        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(15))
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET SESSION TRANSACTION READ ONLY").execute(&mut *conn).await?;
                    // MySQL counts milliseconds, MariaDB seconds; each ignores the other's.
                    let _ = sqlx::query("SET SESSION MAX_EXECUTION_TIME = 60000").execute(&mut *conn).await;
                    let _ = sqlx::query("SET SESSION max_statement_time = 60").execute(&mut *conn).await;
                    Ok(())
                })
            })
            .connect_with(opts)
            .await
            .map_err(|e| Error::Unavailable(format!("connector {}: {e}", spec.name)))?;
        Ok((pool, t))
    }

    async fn pool(&self, spec: &ConnectorSpec) -> Result<MySqlPool> {
        if let Some(pool) = self.pool.lock().clone() {
            return Ok(pool);
        }
        let (pool, _) = self.connect(spec, &spec.resolve_url()?).await?;
        *self.pool.lock() = Some(pool.clone());
        Ok(pool)
    }
}

type Q<'a> = sqlx::query::Query<'a, MySql, sqlx_mysql::MySqlArguments>;

fn bind<'a>(q: Q<'a>, v: &'a Value) -> Q<'a> {
    match v {
        Value::Null => q.bind(None::<String>),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => q.bind(i),
            (None, Some(u), _) => q.bind(u),
            (_, _, Some(f)) => q.bind(f),
            _ => q.bind(n.to_string()),
        },
        Value::String(s) => q.bind(s.as_str()),
        other => q.bind(other.to_string()),
    }
}

#[async_trait]
impl Connector for MySqlConnector {
    async fn snapshot(&self, spec: &ConnectorSpec) -> Result<CatalogSnapshot> {
        let url = spec.resolve_url()?;
        let (pool, target) = self.connect(spec, &url).await?;
        let out = read(&pool, spec, &target).await;
        pool.close().await;
        out
    }

    async fn rows(&self, spec: &ConnectorSpec, req: &RowRequest) -> Result<RowPage> {
        let sql = rows_sql(req)?;
        let pool = self.pool(spec).await?;
        let mut q: Q<'_> = sqlx::query(sqlx::AssertSqlSafe(sql));
        if let Some(after) = &req.after {
            for v in after {
                q = bind(q, v);
            }
        }
        let q = q.bind(req.limit.clamp(1, 10_000) as i64);
        let rows = q.fetch_all(&pool).await.map_err(|e| Error::Unavailable(format!("reading {}: {e}", req.table)))?;
        let mut out: Vec<Map<String, Value>> = Vec::with_capacity(rows.len());
        for r in &rows {
            let text: String = sqlx::Row::try_get(r, 0)?;
            out.push(serde_json::from_str(&text).map_err(|e| Error::Other(format!("reading {}: {e}", req.table)))?);
        }
        let last_key = out.last().map(|r| req.key.iter().map(|k| r.get(k).cloned().unwrap_or(Value::Null)).collect());
        Ok(RowPage { rows: out, last_key })
    }

    async fn close(&self) {
        let pool = self.pool.lock().take();
        if let Some(pool) = pool {
            pool.close().await;
        }
    }
}

type TableRow = (String, String, String, String, i64, Option<i64>);
type ColRow = (String, String, String, String, String, i64, String, String);
type FkRow = (String, String, String, String, String, String, String);
/// digest, text, calls, mean ms, rows sent.
type StatementRow = (String, String, i64, f64, i64);

async fn read(pool: &MySqlPool, spec: &ConnectorSpec, t: &Target) -> Result<CatalogSnapshot> {
    let mut notes = Vec::new();
    let version: String = sqlx::query_scalar("SELECT CAST(VERSION() AS CHAR)").fetch_one(pool).await?;
    let engine = if version.to_ascii_lowercase().contains("mariadb") { "mariadb" } else { "mysql" };
    let schemas: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(SCHEMA_NAME AS CHAR) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME NOT IN {SYSTEM} ORDER BY 1"
    )))
    .fetch_all(pool)
    .await?;
    let schemas: Vec<String> = schemas.into_iter().filter(|s| spec.wants_schema(s)).collect();
    let wanted = |s: &str| schemas.iter().any(|x| x == s);
    let tables: Vec<TableRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(TABLE_SCHEMA AS CHAR), CAST(TABLE_NAME AS CHAR), CAST(TABLE_TYPE AS CHAR), \
         CAST(COALESCE(TABLE_COMMENT, '') AS CHAR), CAST(COALESCE(TABLE_ROWS, 0) AS SIGNED), \
         CAST(UNIX_TIMESTAMP(COALESCE(UPDATE_TIME, CREATE_TIME)) AS SIGNED) \
         FROM information_schema.TABLES WHERE TABLE_SCHEMA NOT IN {SYSTEM} ORDER BY 1, 2"
    )))
    .fetch_all(pool)
    .await?;
    let columns: Vec<ColRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(TABLE_SCHEMA AS CHAR), CAST(TABLE_NAME AS CHAR), CAST(COLUMN_NAME AS CHAR), \
         CAST(COLUMN_TYPE AS CHAR), CAST(IS_NULLABLE AS CHAR), CAST(ORDINAL_POSITION AS SIGNED), \
         CAST(COALESCE(COLUMN_COMMENT, '') AS CHAR), CAST(COLUMN_KEY AS CHAR) \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA NOT IN {SYSTEM} ORDER BY 1, 2, 6"
    )))
    .fetch_all(pool)
    .await?;
    let fks: Vec<FkRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(CONSTRAINT_NAME AS CHAR), CAST(TABLE_SCHEMA AS CHAR), CAST(TABLE_NAME AS CHAR), \
         CAST(COLUMN_NAME AS CHAR), CAST(REFERENCED_TABLE_SCHEMA AS CHAR), CAST(REFERENCED_TABLE_NAME AS CHAR), \
         CAST(REFERENCED_COLUMN_NAME AS CHAR) FROM information_schema.KEY_COLUMN_USAGE \
         WHERE REFERENCED_TABLE_NAME IS NOT NULL AND TABLE_SCHEMA NOT IN {SYSTEM} \
         ORDER BY TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION"
    )))
    .fetch_all(pool)
    .await?;
    let views: Vec<(String, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(TABLE_SCHEMA AS CHAR), CAST(TABLE_NAME AS CHAR), CAST(COALESCE(VIEW_DEFINITION, '') AS CHAR) \
         FROM information_schema.VIEWS WHERE TABLE_SCHEMA NOT IN {SYSTEM}"
    )))
    .fetch_all(pool)
    .await?;
    // MySQL 8.0.13+; MariaDB has no such view.
    let view_deps: Vec<(String, String, String, String)> = match sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(VIEW_SCHEMA AS CHAR), CAST(VIEW_NAME AS CHAR), CAST(TABLE_SCHEMA AS CHAR), CAST(TABLE_NAME AS CHAR) \
         FROM information_schema.VIEW_TABLE_USAGE WHERE VIEW_SCHEMA NOT IN {SYSTEM}"
    )))
    .fetch_all(pool)
    .await
    {
        Ok(v) => v,
        Err(_) => {
            notes.push("view dependencies: information_schema.VIEW_TABLE_USAGE is not available".into());
            Vec::new()
        }
    };
    let mut readers: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut schema_readers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if spec.map_grants {
        let table_grants: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT CAST(GRANTEE AS CHAR), CAST(TABLE_SCHEMA AS CHAR), CAST(TABLE_NAME AS CHAR) \
             FROM information_schema.TABLE_PRIVILEGES WHERE PRIVILEGE_TYPE = 'SELECT'",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        for (g, s, tb) in table_grants {
            readers.entry((s, tb)).or_default().push(grantee(&g));
        }
        let db_grants: Vec<(String, String)> = sqlx::query_as(
            "SELECT CAST(GRANTEE AS CHAR), CAST(TABLE_SCHEMA AS CHAR) FROM information_schema.SCHEMA_PRIVILEGES \
             WHERE PRIVILEGE_TYPE = 'SELECT'",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        for (g, s) in db_grants {
            schema_readers.entry(s).or_default().push(grantee(&g));
        }
    }
    let queries = if spec.max_queries == 0 {
        Vec::new()
    } else {
        let rows: std::result::Result<Vec<StatementRow>, _> = sqlx::query_as(
            "SELECT CAST(DIGEST AS CHAR), CAST(DIGEST_TEXT AS CHAR), CAST(COUNT_STAR AS SIGNED), \
             CAST(AVG_TIMER_WAIT / 1000000000 AS DOUBLE), CAST(SUM_ROWS_SENT AS SIGNED) \
             FROM performance_schema.events_statements_summary_by_digest \
             WHERE DIGEST_TEXT IS NOT NULL ORDER BY COUNT_STAR DESC LIMIT ?",
        )
        .bind(spec.max_queries as i64)
        .fetch_all(pool)
        .await;
        match rows {
            Ok(r) => r
                .into_iter()
                .map(|(queryid, text, calls, mean_ms, rows)| QueryStat { queryid, text, calls, mean_ms, rows })
                .collect(),
            Err(_) => {
                notes.push("query statistics: performance_schema is not readable".into());
                Vec::new()
            }
        }
    };

    let definitions: BTreeMap<(String, String), String> = views.into_iter().map(|(s, v, d)| ((s, v), d)).collect();
    let mut relations: Vec<RelationInfo> = tables
        .into_iter()
        .filter(|(s, ..)| wanted(s))
        .map(|(schema, name, kind, comment, rows, updated)| {
            let key = (schema.clone(), name.clone());
            let mut r = readers.get(&key).cloned().unwrap_or_default();
            r.extend(schema_readers.get(&schema).cloned().unwrap_or_default());
            r.sort();
            r.dedup();
            RelationInfo {
                kind: if kind.contains("VIEW") { RelKind::View } else { RelKind::Table },
                comment,
                row_estimate: rows,
                last_activity: updated.map(|s| s * 1000),
                definition: definitions.get(&key).cloned(),
                readers: r,
                schema,
                name,
                ..Default::default()
            }
        })
        .collect();
    let mut by_table: BTreeMap<(String, String), Vec<ColumnInfo>> = BTreeMap::new();
    for (schema, table, name, column_type, nullable, position, comment, key) in columns {
        by_table.entry((schema, table)).or_default().push(ColumnInfo {
            enum_values: enum_labels(&column_type),
            data_type: column_type,
            nullable: nullable.eq_ignore_ascii_case("YES"),
            position,
            primary_key: key == "PRI",
            unique: key == "UNI",
            comment,
            name,
        });
    }
    for r in &mut relations {
        r.columns = by_table.remove(&(r.schema.clone(), r.name.clone())).unwrap_or_default();
    }
    let mut foreign_keys: Vec<ForeignKey> = Vec::new();
    for (name, fs, ft, fc, ts, tt, tc) in fks.into_iter().filter(|f| wanted(&f.1)) {
        match foreign_keys.last_mut() {
            Some(fk) if fk.name == name && fk.from_schema == fs && fk.from_table == ft => {
                fk.from_columns.push(fc);
                fk.to_columns.push(tc);
            }
            _ => foreign_keys.push(ForeignKey {
                name,
                from_schema: fs,
                from_table: ft,
                from_columns: vec![fc],
                to_schema: ts,
                to_table: tt,
                to_columns: vec![tc],
            }),
        }
    }
    Ok(CatalogSnapshot {
        source: SourceInfo {
            key: spec.name.clone(),
            title: if spec.title.is_empty() { t.database.clone() } else { spec.title.clone() },
            engine: engine.into(),
            host: t.host.clone(),
            database: t.database.clone(),
            server_version: version,
        },
        schemas: schemas.iter().map(|s| SchemaInfo { name: s.clone(), ..Default::default() }).collect(),
        relations,
        foreign_keys,
        view_deps: view_deps
            .into_iter()
            .filter(|v| wanted(&v.0))
            .map(|(view_schema, view, schema, name)| ViewDep { view_schema, view, schema, name })
            .collect(),
        queries,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_read_in_key_order_with_bound_values() {
        let req = RowRequest {
            table: "shop.order`s".into(),
            key: vec!["id".into(), "line".into()],
            columns: vec!["it's".into(), "id".into()],
            after: Some(vec![Value::from(7), Value::from(2)]),
            limit: 50,
        };
        assert_eq!(
            rows_sql(&req).unwrap(),
            "SELECT CAST(JSON_OBJECT('id', `id`, 'line', `line`, 'it''s', `it's`) AS CHAR) FROM `shop`.`order``s` \
             WHERE `id` IS NOT NULL AND `line` IS NOT NULL AND (`id`, `line`) > (?, ?) ORDER BY `id`, `line` LIMIT ?"
        );
        assert!(rows_sql(&RowRequest { table: "nodot".into(), key: vec!["id".into()], ..Default::default() }).is_err());
    }

    #[test]
    fn enum_types_list_their_labels() {
        assert_eq!(enum_labels("enum('open','it''s done','x,y')"), vec!["open", "it's done", "x,y"]);
        assert!(enum_labels("varchar(10)").is_empty());
        assert_eq!(grantee("'reporting'@'%'"), "reporting");
    }
}
