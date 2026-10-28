//! MongoDB: databases become schemas and collections tables; their fields
//! are inferred from a sample of documents (`$sample`, `sample_rows` per
//! collection, default 100): each field's most common BSON type (`mixed(a|b)`
//! when several), whether it is missing or null in some (nullable), and
//! nested documents as dotted fields one level down. `_id` is the primary
//! key; a single-field unique index marks a field unique. A field `x_id` or
//! `xId` holding object ids, in a database with a collection `x` or `xs`,
//! becomes a foreign key to it (a heuristic: MongoDB declares none). Views
//! are views, with their pipeline as definition.
//!
//! The client connects directly to the one host of the URL (no replica-set
//! discovery, so a server cannot point a crawl elsewhere) and never resolves
//! `mongodb+srv`. Rows of synced collections are read in `_id` (or key)
//! order, with only the key and mapped fields; object ids become their hex
//! text.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::TryStreamExt;
use mongodb::bson::{Bson, Document, doc, oid::ObjectId};
use mongodb::options::{ClientOptions, ServerAddress};
use onto_core::catalog::{CatalogSnapshot, ColumnInfo, ForeignKey, RelKind, RelationInfo, SchemaInfo, SourceInfo};
use onto_core::{Error, Result};
use serde_json::{Map, Value};

use super::restricted::{self, Target};
use super::{Connector, ConnectorSpec, RowPage, RowRequest};

pub const SCHEMES: [&str; 1] = ["mongodb"];
/// Query parameters a restricted URL may carry.
pub const RESTRICTED_PARAMS: [&str; 5] = ["authSource", "authMechanism", "tls", "ssl", "appName"];
/// Never crawled: the server's own databases.
const SYSTEM_DBS: [&str; 3] = ["admin", "local", "config"];

/// Check a URL a project credential registers ([`restricted::check`]).
pub async fn check_url(raw: &str, allow_private: bool) -> Result<Target> {
    restricted::check("mongodb", raw, &SCHEMES, 27017, false, &RESTRICTED_PARAMS, allow_private).await
}

#[derive(Default)]
pub struct MongoConnector {
    allow_private: bool,
}

impl MongoConnector {
    pub fn new(allow_private: bool) -> Self {
        Self { allow_private }
    }

    async fn client(&self, spec: &ConnectorSpec) -> Result<(mongodb::Client, Target)> {
        let url = spec.resolve_url()?;
        if url.trim_start().to_ascii_lowercase().starts_with("mongodb+srv:") {
            return Err(Error::invalid(format!(
                "connector {}: mongodb+srv URLs are not supported (name one host: mongodb://user@host:27017/db)",
                spec.name
            )));
        }
        let t = if spec.restricted {
            check_url(&url, self.allow_private)
                .await
                .map_err(|e| Error::invalid(format!("connector {}: {e}", spec.name)))?
        } else {
            restricted::parse("mongodb", &url, &SCHEMES, 27017, false)?
        };
        let hide = |m: String| if t.password.is_empty() { m } else { m.replace(&t.password, "***") };
        let mut opts = ClientOptions::parse(&url)
            .await
            .map_err(|e| Error::invalid(format!("connector {}: {}", spec.name, hide(e.to_string()))))?;
        let tls = matches!(t.params.get("tls").or_else(|| t.params.get("ssl")).map(String::as_str), Some("true"));
        // The checked address, unless TLS must match the certificate to the name.
        let host = if tls { t.host.clone() } else { t.connect_host() };
        opts.hosts = vec![ServerAddress::Tcp { host, port: Some(t.port) }];
        opts.direct_connection = Some(true);
        opts.app_name = Some("onto-intelli".into());
        opts.server_selection_timeout = Some(Duration::from_secs(15));
        opts.connect_timeout = Some(Duration::from_secs(15));
        let client = mongodb::Client::with_options(opts)
            .map_err(|e| Error::Unavailable(format!("connector {}: {e}", spec.name)))?;
        Ok((client, t))
    }
}

fn type_name(b: &Bson) -> &'static str {
    match b {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null | Bson::Undefined => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "javascript",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal",
        Bson::MaxKey | Bson::MinKey | Bson::DbPointer(_) => "other",
    }
}

#[derive(Default)]
struct FieldStats {
    order: usize,
    seen: usize,
    nulls: usize,
    types: BTreeMap<&'static str, usize>,
}

/// The fields of sampled documents, in first-seen order (`_id` first).
pub fn infer_fields(docs: &[Document]) -> Vec<ColumnInfo> {
    let mut stats: BTreeMap<String, FieldStats> = BTreeMap::new();
    let mut next = 0;
    let mut note = |name: String, v: &Bson, stats: &mut BTreeMap<String, FieldStats>| {
        let f = stats.entry(name).or_insert_with(|| {
            next += 1;
            FieldStats { order: next, ..Default::default() }
        });
        f.seen += 1;
        match v {
            Bson::Null | Bson::Undefined => f.nulls += 1,
            other => *f.types.entry(type_name(other)).or_default() += 1,
        }
    };
    for d in docs {
        for (k, v) in d {
            note(k.clone(), v, &mut stats);
            if let Bson::Document(inner) = v {
                for (ik, iv) in inner {
                    note(format!("{k}.{ik}"), iv, &mut stats);
                }
            }
        }
    }
    let total = docs.len();
    let mut cols: Vec<(usize, ColumnInfo)> = stats
        .into_iter()
        .map(|(name, f)| {
            let mut types: Vec<(&str, usize)> = f.types.into_iter().collect();
            types.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            let data_type = match types.len() {
                0 => "null".to_string(),
                1 => types[0].0.to_string(),
                _ => format!("mixed({})", types.iter().map(|t| t.0).collect::<Vec<_>>().join("|")),
            };
            let order = if name == "_id" { 0 } else { f.order };
            (
                order,
                ColumnInfo {
                    primary_key: name == "_id",
                    nullable: name != "_id" && (f.seen < total || f.nulls > 0),
                    data_type,
                    name,
                    ..Default::default()
                },
            )
        })
        .collect();
    cols.sort_by_key(|(o, _)| *o);
    cols.into_iter()
        .enumerate()
        .map(|(i, (_, mut c))| {
            c.position = i as i64 + 1;
            c
        })
        .collect()
}

/// `customer_id` / `customerId` of object ids → collection `customer`,
/// `customers` or `customeres` when it exists.
pub fn reference_target<'a>(field: &str, data_type: &str, collections: &'a [String]) -> Option<&'a String> {
    if data_type != "objectId" || field == "_id" || field.contains('.') {
        return None;
    }
    let base = field.strip_suffix("_id").or_else(|| field.strip_suffix("Id"))?;
    if base.is_empty() {
        return None;
    }
    let base = base.to_ascii_lowercase();
    [base.clone(), format!("{base}s"), format!("{base}es")]
        .iter()
        .find_map(|cand| collections.iter().find(|c| c.to_ascii_lowercase() == *cand))
}

fn json(b: Bson) -> Value {
    match b {
        Bson::ObjectId(o) => Value::String(o.to_hex()),
        Bson::Document(d) => Value::Object(d.into_iter().map(|(k, v)| (k, json(v))).collect()),
        Bson::Array(a) => Value::Array(a.into_iter().map(json).collect()),
        other => other.into_relaxed_extjson(),
    }
}

/// A key value back to BSON: 24 hex characters are taken as an object id.
fn key_bson(v: &Value) -> Bson {
    match v {
        Value::String(s) if s.len() == 24 => {
            ObjectId::parse_str(s).map(Bson::ObjectId).unwrap_or(Bson::String(s.clone()))
        }
        other => Bson::try_from(other.clone()).unwrap_or(Bson::Null),
    }
}

#[async_trait]
impl Connector for MongoConnector {
    async fn snapshot(&self, spec: &ConnectorSpec) -> Result<CatalogSnapshot> {
        let (client, t) = self.client(spec).await?;
        let fail = |e: mongodb::error::Error| Error::Unavailable(format!("connector {}: {e}", spec.name));
        let dbs: Vec<String> = if t.database.is_empty() {
            client.list_database_names().await.map_err(fail)?
        } else {
            vec![t.database.clone()]
        };
        let dbs: Vec<String> =
            dbs.into_iter().filter(|d| !SYSTEM_DBS.contains(&d.as_str()) && spec.wants_schema(d)).collect();
        let version = client
            .database("admin")
            .run_command(doc! { "buildInfo": 1 })
            .await
            .ok()
            .and_then(|d| d.get_str("version").ok().map(str::to_string))
            .unwrap_or_default();
        let sample = spec.sample_rows.clamp(1, 1000) as i64;
        let mut relations = Vec::new();
        let mut foreign_keys = Vec::new();
        let mut notes = Vec::new();
        for db_name in &dbs {
            let db = client.database(db_name);
            let specs: Vec<mongodb::results::CollectionSpecification> =
                db.list_collections().await.map_err(fail)?.try_collect().await.map_err(fail)?;
            let names: Vec<String> =
                specs.iter().map(|c| c.name.clone()).filter(|n| !n.starts_with("system.")).collect();
            for cs in specs.iter().filter(|c| !c.name.starts_with("system.")) {
                let coll = db.collection::<Document>(&cs.name);
                let is_view = matches!(cs.collection_type, mongodb::results::CollectionType::View);
                let docs: Vec<Document> = match coll
                    .aggregate([doc! { "$sample": { "size": sample } }])
                    .max_time(Duration::from_secs(60))
                    .await
                {
                    Ok(cur) => cur.try_collect().await.map_err(fail)?,
                    Err(e) => {
                        notes.push(format!("{db_name}.{}: no sample ({e})", cs.name));
                        Vec::new()
                    }
                };
                let mut columns = infer_fields(&docs);
                if !is_view && let Ok(cur) = coll.list_indexes().await {
                    let idx: Vec<mongodb::IndexModel> = cur.try_collect().await.unwrap_or_default();
                    for i in idx {
                        let unique = i.options.as_ref().and_then(|o| o.unique).unwrap_or(false);
                        if unique
                            && i.keys.len() == 1
                            && let Some((k, _)) = i.keys.iter().next()
                            && let Some(c) = columns.iter_mut().find(|c| &c.name == k)
                        {
                            c.unique = true;
                        }
                    }
                }
                for c in &columns {
                    if let Some(target) = reference_target(&c.name, &c.data_type, &names) {
                        foreign_keys.push(ForeignKey {
                            name: format!("{}_{}_ref", cs.name, c.name),
                            from_schema: db_name.clone(),
                            from_table: cs.name.clone(),
                            from_columns: vec![c.name.clone()],
                            to_schema: db_name.clone(),
                            to_table: target.clone(),
                            to_columns: vec!["_id".into()],
                        });
                    }
                }
                let row_estimate =
                    if is_view { 0 } else { coll.estimated_document_count().await.map(|n| n as i64).unwrap_or(0) };
                relations.push(RelationInfo {
                    schema: db_name.clone(),
                    name: cs.name.clone(),
                    kind: if is_view { RelKind::View } else { RelKind::Table },
                    row_estimate,
                    definition: is_view.then(|| {
                        let pipeline = cs.options.pipeline.clone().unwrap_or_default();
                        format!(
                            "{} on {}",
                            serde_json::to_string(
                                &Bson::Array(pipeline.into_iter().map(Bson::Document).collect()).into_relaxed_extjson()
                            )
                            .unwrap_or_default(),
                            cs.options.view_on.clone().unwrap_or_default()
                        )
                    }),
                    columns,
                    ..Default::default()
                });
            }
        }
        notes.push(format!("fields inferred from up to {sample} sampled documents per collection"));
        Ok(CatalogSnapshot {
            source: SourceInfo {
                key: spec.name.clone(),
                title: if spec.title.is_empty() { t.database.clone() } else { spec.title.clone() },
                engine: "mongodb".into(),
                host: t.host.clone(),
                database: t.database.clone(),
                server_version: version,
            },
            schemas: dbs.iter().map(|d| SchemaInfo { name: d.clone(), ..Default::default() }).collect(),
            relations,
            foreign_keys,
            view_deps: Vec::new(),
            queries: Vec::new(),
            notes,
        })
    }

    async fn rows(&self, spec: &ConnectorSpec, req: &RowRequest) -> Result<RowPage> {
        let (db, coll) = req
            .table
            .split_once('.')
            .filter(|(d, c)| !d.is_empty() && !c.is_empty())
            .ok_or_else(|| Error::invalid(format!("collection {} must be named database.collection", req.table)))?;
        let [key] = req.key.as_slice() else {
            return Err(Error::invalid(format!("reading {}: MongoDB rows need a single key field", req.table)));
        };
        let (client, _) = self.client(spec).await?;
        let mut filter = doc! { key.as_str(): { "$exists": true, "$ne": Bson::Null } };
        if let Some(after) = req.after.as_ref().and_then(|a| a.first()) {
            filter = doc! { "$and": [filter, { key.as_str(): { "$gt": key_bson(after) } }] };
        }
        let mut projection = Document::new();
        for f in std::iter::once(key).chain(&req.columns) {
            projection.insert(f.as_str(), 1);
        }
        if !projection.contains_key("_id") {
            projection.insert("_id", 0);
        }
        let fail = |e: mongodb::error::Error| Error::Unavailable(format!("reading {}: {e}", req.table));
        let docs: Vec<Document> = client
            .database(db)
            .collection::<Document>(coll)
            .find(filter)
            .sort(doc! { key.as_str(): 1 })
            .projection(projection)
            .limit(req.limit.clamp(1, 10_000) as i64)
            .max_time(Duration::from_secs(60))
            .await
            .map_err(fail)?
            .try_collect()
            .await
            .map_err(fail)?;
        let rows: Vec<Map<String, Value>> = docs
            .into_iter()
            .map(|d| match json(Bson::Document(d)) {
                Value::Object(m) => m,
                _ => Map::new(),
            })
            .collect();
        let last_key = rows.last().map(|r| vec![r.get(key.as_str()).cloned().unwrap_or(Value::Null)]);
        Ok(RowPage { rows, last_key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_types_and_references_come_from_the_sample() {
        let oid = ObjectId::new();
        let docs = vec![
            doc! { "_id": oid, "name": "Ann", "customer_id": ObjectId::new(), "address": { "city": "Oslo" }, "age": 41 },
            doc! { "_id": ObjectId::new(), "name": "Bo", "customer_id": ObjectId::new(), "age": "unknown", "vip": Bson::Null },
        ];
        let cols = infer_fields(&docs);
        let by: BTreeMap<&str, &ColumnInfo> = cols.iter().map(|c| (c.name.as_str(), c)).collect();
        assert_eq!(cols[0].name, "_id");
        assert!(by["_id"].primary_key && !by["_id"].nullable);
        assert_eq!(by["name"].data_type, "string");
        assert!(!by["name"].nullable);
        assert_eq!(by["age"].data_type, "mixed(int|string)");
        assert!(by["address.city"].nullable, "missing from one document");
        assert!(by["vip"].nullable);
        assert_eq!(by["customer_id"].data_type, "objectId");
        let names = vec!["orders".to_string(), "customers".to_string()];
        assert_eq!(reference_target("customer_id", "objectId", &names).map(String::as_str), Some("customers"));
        assert_eq!(reference_target("customerId", "objectId", &names).map(String::as_str), Some("customers"));
        assert_eq!(reference_target("customer_id", "string", &names), None);
        assert_eq!(reference_target("vendor_id", "objectId", &names), None);
        assert_eq!(json(Bson::ObjectId(oid)), Value::String(oid.to_hex()));
        assert_eq!(key_bson(&Value::String(oid.to_hex())), Bson::ObjectId(oid));
        assert!(matches!(key_bson(&Value::from(5)), Bson::Int32(5) | Bson::Int64(5)));
    }
}
