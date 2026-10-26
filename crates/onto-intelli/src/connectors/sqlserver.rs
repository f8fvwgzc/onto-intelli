//! SQL Server (and Azure SQL) metadata from the catalog views of one
//! database: schemas, tables and views (`sys.objects`, row counts from
//! `sys.partitions`), columns with their keys and `MS_Description` comments,
//! foreign keys, view definitions and dependencies, and `SELECT` grants. No
//! table is read for the snapshot.
//!
//! Rows of synced tables are read a page per statement in key order (keyset
//! paging) with only the key and mapped columns, as JSON built by the server
//! (`FOR JSON PATH`). Identifiers are bracket-quoted; values are bound.
//!
//! A restricted URL connects its TCP stream to the address it checked, while
//! TLS still verifies the certificate against the host name.

use std::collections::BTreeMap;

use async_trait::async_trait;
use onto_core::catalog::{
    CatalogSnapshot, ColumnInfo, ForeignKey, RelKind, RelationInfo, SchemaInfo, SourceInfo, ViewDep,
};
use onto_core::{Error, Result};
use serde_json::{Map, Value};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel, Query};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use super::restricted::{self, Target};
use super::{Connector, ConnectorSpec, RowPage, RowRequest};

pub const SCHEMES: [&str; 2] = ["sqlserver", "mssql"];
/// Query parameters a restricted URL may carry.
pub const RESTRICTED_PARAMS: [&str; 3] = ["encrypt", "trust_server_certificate", "trustservercertificate"];

type Conn = Client<Compat<TcpStream>>;

/// Check a URL a project credential registers ([`restricted::check`]).
pub async fn check_url(raw: &str, allow_private: bool) -> Result<Target> {
    restricted::check("sqlserver", raw, &SCHEMES, 1433, true, &RESTRICTED_PARAMS, allow_private).await
}

/// The values a column-level CHECK allows, when it is a list of equalities
/// SQL Server keeps as `([status]='paid' OR [status]='placed')`; empty otherwise.
pub fn check_values(definition: &str, column: &str) -> Vec<String> {
    let col = quote_ident(column);
    let body = definition.trim().trim_start_matches('(').trim_end_matches(')');
    let mut out = Vec::new();
    for part in body.split(" OR ") {
        let part = part.trim().trim_start_matches('(').trim_end_matches(')').trim();
        let Some(rest) = part.strip_prefix(&col).map(str::trim_start).and_then(|r| r.strip_prefix('=')) else {
            return Vec::new();
        };
        let rest = rest.trim();
        let Some(lit) = rest.strip_prefix("N'").or_else(|| rest.strip_prefix('\'')).and_then(|r| r.strip_suffix('\''))
        else {
            return Vec::new();
        };
        out.push(lit.replace("''", "'"));
    }
    out.sort();
    out.dedup();
    out
}

/// `[name]`, with `]` doubled.
pub fn quote_ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn flag(t: &Target, names: &[&str]) -> Option<bool> {
    names.iter().find_map(|n| t.params.get(*n)).map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1"))
}

/// The statement for one page of `req`: `@P1` is the limit, then the
/// `after` key values. SQL Server has no row-value comparison, so the
/// keyset condition is spelled out: `(a > @P2) OR (a = @P2 AND b > @P3)`.
pub fn rows_sql(req: &RowRequest) -> Result<String> {
    let (schema, table) = req
        .table
        .split_once('.')
        .filter(|(s, t)| !s.is_empty() && !t.is_empty())
        .ok_or_else(|| Error::invalid(format!("table {} must be named schema.table", req.table)))?;
    if req.key.is_empty() || req.key.iter().chain(&req.columns).any(|c| c.is_empty()) {
        return Err(Error::invalid(format!("reading {}: key and column names must not be empty", req.table)));
    }
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
        let mut ors = Vec::new();
        for i in 0..keys.len() {
            let mut and: Vec<String> = (0..i).map(|j| format!("{} = @P{}", keys[j], j + 2)).collect();
            and.push(format!("{} > @P{}", keys[i], i + 2));
            ors.push(format!("({})", and.join(" AND ")));
        }
        cond.push(format!("({})", ors.join(" OR ")));
    }
    Ok(format!(
        "SELECT TOP (@P1) {} FROM {}.{} WHERE {} ORDER BY {} FOR JSON PATH, INCLUDE_NULL_VALUES",
        select.join(", "),
        quote_ident(schema),
        quote_ident(table),
        cond.join(" AND "),
        keys.join(", ")
    ))
}

#[derive(Default)]
pub struct SqlServerConnector {
    allow_private: bool,
}

impl SqlServerConnector {
    pub fn new(allow_private: bool) -> Self {
        Self { allow_private }
    }

    async fn connect(&self, spec: &ConnectorSpec) -> Result<(Conn, Target)> {
        let url = spec.resolve_url()?;
        let t = if spec.restricted {
            check_url(&url, self.allow_private)
                .await
                .map_err(|e| Error::invalid(format!("connector {}: {e}", spec.name)))?
        } else {
            restricted::parse("sqlserver", &url, &SCHEMES, 1433, true)?
        };
        let mut config = Config::new();
        config.host(&t.host);
        config.port(t.port);
        config.authentication(AuthMethod::sql_server(&t.user, &t.password));
        config.application_name("onto-intelli");
        if !t.database.is_empty() {
            config.database(&t.database);
        }
        config.encryption(match flag(&t, &["encrypt"]) {
            Some(false) => EncryptionLevel::Off,
            _ => EncryptionLevel::Required,
        });
        if flag(&t, &["trust_server_certificate", "trustservercertificate"]).unwrap_or(false) {
            config.trust_cert();
        }
        let addr = format!("{}:{}", t.connect_host(), t.port);
        let unavailable = |e: String| Error::Unavailable(format!("connector {}: {e}", spec.name));
        let tcp = tokio::time::timeout(std::time::Duration::from_secs(15), TcpStream::connect(addr))
            .await
            .map_err(|_| unavailable("connecting timed out".into()))?
            .map_err(|e| unavailable(e.to_string()))?;
        tcp.set_nodelay(true).map_err(|e| unavailable(e.to_string()))?;
        let mut client = Client::connect(config, tcp.compat_write()).await.map_err(|e| unavailable(e.to_string()))?;
        client
            .simple_query("SET LOCK_TIMEOUT 60000; SET TRANSACTION ISOLATION LEVEL READ COMMITTED;")
            .await
            .map_err(|e| unavailable(e.to_string()))?
            .into_results()
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        Ok((client, t))
    }
}

async fn rows_of(c: &mut Conn, sql: &str) -> Result<Vec<tiberius::Row>> {
    c.simple_query(sql)
        .await
        .map_err(|e| Error::Unavailable(format!("sqlserver: {e}")))?
        .into_first_result()
        .await
        .map_err(|e| Error::Unavailable(format!("sqlserver: {e}")))
}

fn s(r: &tiberius::Row, i: usize) -> String {
    r.try_get::<&str, _>(i).ok().flatten().unwrap_or_default().trim_end().to_string()
}

fn int(r: &tiberius::Row, i: usize) -> i64 {
    r.try_get::<i64, _>(i)
        .ok()
        .flatten()
        .or_else(|| r.try_get::<i32, _>(i).ok().flatten().map(i64::from))
        .or_else(|| r.try_get::<i16, _>(i).ok().flatten().map(i64::from))
        .or_else(|| r.try_get::<u8, _>(i).ok().flatten().map(i64::from))
        .unwrap_or(0)
}

fn bit(r: &tiberius::Row, i: usize) -> bool {
    r.try_get::<bool, _>(i).ok().flatten().unwrap_or(false)
}

const TYPE_NAME: &str = "CASE \
    WHEN t.name IN ('varchar', 'char', 'varbinary', 'binary') \
      THEN t.name + '(' + IIF(c.max_length = -1, 'max', CAST(c.max_length AS varchar(10))) + ')' \
    WHEN t.name IN ('nvarchar', 'nchar') \
      THEN t.name + '(' + IIF(c.max_length = -1, 'max', CAST(c.max_length / 2 AS varchar(10))) + ')' \
    WHEN t.name IN ('decimal', 'numeric') \
      THEN t.name + '(' + CAST(c.precision AS varchar(5)) + ',' + CAST(c.scale AS varchar(5)) + ')' \
    ELSE t.name END";

#[async_trait]
impl Connector for SqlServerConnector {
    async fn snapshot(&self, spec: &ConnectorSpec) -> Result<CatalogSnapshot> {
        let (mut c, t) = self.connect(spec).await?;
        let head = rows_of(&mut c, "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), DB_NAME()").await?;
        let (version, database) = head.first().map(|r| (s(r, 0), s(r, 1))).unwrap_or_default();
        let schemas: Vec<String> = rows_of(
            &mut c,
            "SELECT name FROM sys.schemas WHERE schema_id < 16384 AND name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') ORDER BY name",
        )
        .await?
        .iter()
        .map(|r| s(r, 0))
        .filter(|n| spec.wants_schema(n))
        .collect();
        let wanted = |n: &str| schemas.iter().any(|x| x == n);
        let objects = rows_of(
            &mut c,
            "SELECT s.name, o.name, o.type, CAST(ISNULL(ep.value, '') AS nvarchar(4000)), CAST(ISNULL(p.rows, 0) AS bigint), \
             m.definition, CAST(DATEDIFF_BIG(ms, '1970-01-01', o.modify_date) AS bigint) \
             FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
             LEFT JOIN (SELECT object_id, SUM(rows) AS rows FROM sys.partitions WHERE index_id IN (0, 1) GROUP BY object_id) p \
               ON p.object_id = o.object_id \
             LEFT JOIN sys.extended_properties ep ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 \
               AND ep.name = 'MS_Description' \
             LEFT JOIN sys.sql_modules m ON m.object_id = o.object_id \
             WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 ORDER BY s.name, o.name",
        )
        .await?;
        let columns = rows_of(
            &mut c,
            &format!(
                "SELECT s.name, o.name, c.name, {TYPE_NAME}, c.is_nullable, c.column_id, \
                 CAST(ISNULL(ep.value, '') AS nvarchar(4000)), \
                 CAST(CASE WHEN EXISTS (SELECT 1 FROM sys.indexes i JOIN sys.index_columns ic \
                   ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                   WHERE i.object_id = c.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id) THEN 1 ELSE 0 END AS bit), \
                 CAST(CASE WHEN EXISTS (SELECT 1 FROM sys.indexes i JOIN sys.index_columns ic \
                   ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                   WHERE i.object_id = c.object_id AND i.is_unique = 1 AND i.is_primary_key = 0 AND ic.column_id = c.column_id \
                   AND (SELECT COUNT(*) FROM sys.index_columns x WHERE x.object_id = i.object_id AND x.index_id = i.index_id \
                        AND x.is_included_column = 0) = 1) THEN 1 ELSE 0 END AS bit) \
                 FROM sys.columns c JOIN sys.objects o ON o.object_id = c.object_id \
                 JOIN sys.schemas s ON s.schema_id = o.schema_id JOIN sys.types t ON t.user_type_id = c.user_type_id \
                 LEFT JOIN sys.extended_properties ep ON ep.class = 1 AND ep.major_id = c.object_id \
                   AND ep.minor_id = c.column_id AND ep.name = 'MS_Description' \
                 WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 ORDER BY s.name, o.name, c.column_id"
            ),
        )
        .await?;
        let checks = rows_of(
            &mut c,
            "SELECT s.name, o.name, c.name, cc.definition FROM sys.check_constraints cc \
             JOIN sys.objects o ON o.object_id = cc.parent_object_id JOIN sys.schemas s ON s.schema_id = o.schema_id \
             JOIN sys.columns c ON c.object_id = cc.parent_object_id AND c.column_id = cc.parent_column_id \
             WHERE cc.parent_column_id > 0",
        )
        .await?;
        let mut allowed: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
        for r in &checks {
            let values = check_values(&s_raw(r, 3), &s(r, 2));
            if !values.is_empty() {
                allowed.insert((s(r, 0), s(r, 1), s(r, 2)), values);
            }
        }
        let fks = rows_of(
            &mut c,
            "SELECT fk.name, ps.name, pt.name, pc.name, rs.name, rt.name, rc.name \
             FROM sys.foreign_keys fk JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
             JOIN sys.tables pt ON pt.object_id = fkc.parent_object_id JOIN sys.schemas ps ON ps.schema_id = pt.schema_id \
             JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
             JOIN sys.tables rt ON rt.object_id = fkc.referenced_object_id JOIN sys.schemas rs ON rs.schema_id = rt.schema_id \
             JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
             ORDER BY ps.name, pt.name, fk.name, fkc.constraint_column_id",
        )
        .await?;
        let deps = rows_of(
            &mut c,
            "SELECT DISTINCT vs.name, v.name, rs.name, r.name FROM sys.sql_expression_dependencies d \
             JOIN sys.views v ON v.object_id = d.referencing_id JOIN sys.schemas vs ON vs.schema_id = v.schema_id \
             JOIN sys.objects r ON r.object_id = d.referenced_id JOIN sys.schemas rs ON rs.schema_id = r.schema_id",
        )
        .await?;
        let mut readers: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        if spec.map_grants {
            let grants = rows_of(
                &mut c,
                "SELECT pr.name, s.name, o.name FROM sys.database_permissions p \
                 JOIN sys.database_principals pr ON pr.principal_id = p.grantee_principal_id \
                 JOIN sys.objects o ON o.object_id = p.major_id JOIN sys.schemas s ON s.schema_id = o.schema_id \
                 WHERE p.class = 1 AND p.permission_name = 'SELECT' AND p.state IN ('G', 'W')",
            )
            .await?;
            for r in &grants {
                readers.entry((s(r, 1), s(r, 2))).or_default().push(s(r, 0));
            }
        }
        let mut by_table: BTreeMap<(String, String), Vec<ColumnInfo>> = BTreeMap::new();
        for r in &columns {
            by_table.entry((s(r, 0), s(r, 1))).or_default().push(ColumnInfo {
                enum_values: allowed.remove(&(s(r, 0), s(r, 1), s(r, 2))).unwrap_or_default(),
                name: s(r, 2),
                data_type: s(r, 3),
                nullable: bit(r, 4),
                position: int(r, 5),
                comment: s(r, 6),
                primary_key: bit(r, 7),
                unique: bit(r, 8),
            });
        }
        let relations: Vec<RelationInfo> = objects
            .iter()
            .filter(|r| wanted(&s(r, 0)))
            .map(|r| {
                let key = (s(r, 0), s(r, 1));
                let mut rd = readers.get(&key).cloned().unwrap_or_default();
                rd.sort();
                rd.dedup();
                let public_read = rd.iter().any(|g| g == "public");
                RelationInfo {
                    kind: if s(r, 2) == "V" { RelKind::View } else { RelKind::Table },
                    comment: s(r, 3),
                    row_estimate: int(r, 4),
                    definition: r.try_get::<&str, _>(5).ok().flatten().map(str::to_string),
                    last_activity: Some(int(r, 6)).filter(|v| *v > 0),
                    columns: by_table.remove(&key).unwrap_or_default(),
                    readers: rd.into_iter().filter(|g| g != "public").collect(),
                    public_read,
                    schema: key.0,
                    name: key.1,
                    ..Default::default()
                }
            })
            .collect();
        let mut foreign_keys: Vec<ForeignKey> = Vec::new();
        for r in fks.iter().filter(|r| wanted(&s(r, 1))) {
            let (name, fs, ft) = (s(r, 0), s(r, 1), s(r, 2));
            match foreign_keys.last_mut() {
                Some(fk) if fk.name == name && fk.from_schema == fs && fk.from_table == ft => {
                    fk.from_columns.push(s(r, 3));
                    fk.to_columns.push(s(r, 6));
                }
                _ => foreign_keys.push(ForeignKey {
                    name,
                    from_schema: fs,
                    from_table: ft,
                    from_columns: vec![s(r, 3)],
                    to_schema: s(r, 4),
                    to_table: s(r, 5),
                    to_columns: vec![s(r, 6)],
                }),
            }
        }
        let _ = c.close().await;
        Ok(CatalogSnapshot {
            source: SourceInfo {
                key: spec.name.clone(),
                title: if spec.title.is_empty() { database.clone() } else { spec.title.clone() },
                engine: "sqlserver".into(),
                host: t.host.clone(),
                database,
                server_version: version,
            },
            schemas: schemas.iter().map(|n| SchemaInfo { name: n.clone(), ..Default::default() }).collect(),
            relations,
            foreign_keys,
            view_deps: deps
                .iter()
                .filter(|r| wanted(&s(r, 0)))
                .map(|r| ViewDep { view_schema: s(r, 0), view: s(r, 1), schema: s(r, 2), name: s(r, 3) })
                .collect(),
            queries: Vec::new(),
            notes: vec!["query statistics are not read from SQL Server (they need VIEW SERVER STATE)".into()],
        })
    }

    async fn rows(&self, spec: &ConnectorSpec, req: &RowRequest) -> Result<RowPage> {
        let sql = rows_sql(req)?;
        let (mut c, _) = self.connect(spec).await?;
        let mut q = Query::new(sql);
        q.bind(req.limit.clamp(1, 10_000) as i64);
        if let Some(after) = &req.after {
            for v in after {
                match v {
                    Value::Bool(b) => q.bind(*b),
                    Value::Number(n) if n.is_i64() => q.bind(n.as_i64().unwrap_or_default()),
                    Value::Number(n) => q.bind(n.as_f64().unwrap_or_default()),
                    Value::String(s) => q.bind(s.clone()),
                    Value::Null => q.bind(Option::<String>::None),
                    other => q.bind(other.to_string()),
                }
            }
        }
        let fail = |e: tiberius::error::Error| Error::Unavailable(format!("reading {}: {e}", req.table));
        let rows = q.query(&mut c).await.map_err(fail)?.into_first_result().await.map_err(fail)?;
        let _ = c.close().await;
        // FOR JSON splits its text over rows of at most 2033 characters.
        let text: String = rows.iter().map(|r| s_raw(r, 0)).collect();
        let rows: Vec<Map<String, Value>> = if text.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&text).map_err(|e| Error::Other(format!("reading {}: {e}", req.table)))?
        };
        let last_key = rows.last().map(|r| req.key.iter().map(|k| r.get(k).cloned().unwrap_or(Value::Null)).collect());
        Ok(RowPage { rows, last_key })
    }
}

/// A text column as is (no trimming: JSON chunks).
fn s_raw(r: &tiberius::Row, i: usize) -> String {
    r.try_get::<&str, _>(i).ok().flatten().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_lists_become_allowed_values() {
        let d = "([status]='refunded' OR [status]='paid' OR [status]=N'it''s')";
        assert_eq!(check_values(d, "status"), vec!["it's", "paid", "refunded"]);
        assert!(check_values("([total]>(0))", "total").is_empty());
        assert!(check_values("([a]='x' OR [b]='y')", "a").is_empty(), "another column");
    }

    #[test]
    fn keyset_pages_spell_out_the_comparison() {
        let req = RowRequest {
            table: "dbo.Order]s".into(),
            key: vec!["id".into(), "line".into()],
            columns: vec!["note".into()],
            after: Some(vec![Value::from(7), Value::from(2)]),
            limit: 50,
        };
        assert_eq!(
            rows_sql(&req).unwrap(),
            "SELECT TOP (@P1) [id], [line], [note] FROM [dbo].[Order]]s] WHERE [id] IS NOT NULL AND [line] IS NOT NULL \
             AND (([id] > @P2) OR ([id] = @P2 AND [line] > @P3)) ORDER BY [id], [line] FOR JSON PATH, INCLUDE_NULL_VALUES"
        );
    }
}
