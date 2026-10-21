//! Objects from rows: the rows of a table that backs an object type become
//! objects of that type (Palantir Foundry: object types backed by a
//! dataset), the foreign key columns named in the backing become links, and
//! the rows of a join table become many-to-many links.
//!
//! No I/O happens here. A connector reads the key and mapped columns of a
//! backed table (never the others, and only when the backing says `sync`);
//! [`map_rows`] turns the rows into entities with provenance;
//! [`plan_objects`] keeps only what changed since the last sync, through
//! the same fingerprints as catalog crawls ([`catalog::plan_changes`]);
//! [`map_join_rows`] and [`plan_links`] do the same for join tables.

use std::collections::{BTreeSet, HashSet};

use serde_json::{Map, Value, json};

use crate::catalog::{self, CrawlPlan, Fingerprints, Mapped};
use crate::graph::{Graph, LinkKey};
use crate::model::{EntityInput, Id, LinkInput, Mutation, Provenance};
use crate::schema::{Backing, LinkBacking, ObjectType, PropType, PropertyDef, Schema};
use crate::time::parse_ts;

/// One row: column name → value, as the database renders it as JSON.
pub type Row = Map<String, Value>;

/// Ids longer than this are rejected by the engine.
const MAX_ID_LEN: usize = 256;

/// How rows become objects.
#[derive(Clone, Debug, Default)]
pub struct RowOptions {
    pub workspace: String,
    /// The connector's name (provenance).
    pub connector: String,
    /// `postgres` (provenance source `connector:postgres`).
    pub engine: String,
    pub crawled_at: i64,
    /// Crawl run id (provenance).
    pub run: String,
    /// The connector turns grants into ACLs: objects of a table everyone may
    /// read are marked `internal`, lifting an earlier restriction.
    pub map_grants: bool,
    /// The backing table's ACL when the table is restricted: its objects
    /// are restricted to the same principals.
    pub acl: Option<Vec<String>>,
}

/// `{type}:{key}`.
pub fn object_id(type_name: &str, key: &str) -> Id {
    format!("{type_name}:{key}")
}

/// A key value as it appears in an object id: strings as they are, numbers
/// and booleans as JSON prints them. `None` for null and empty values.
fn key_part(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// The key of a row: the values of the key columns joined with `.`. `None`
/// when a key column is missing or null.
pub fn row_key(row: &Row, key: &[String]) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    let parts: Option<Vec<String>> = key.iter().map(|k| row.get(k).and_then(key_part)).collect();
    parts.map(|p| p.join("."))
}

/// Text of a scalar value (titles and states).
fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Number(_) | Value::Bool(_) => Some(v.to_string()),
        other => Some(other.to_string()),
    }
}

/// A number from a JSON number or from text such as `12.5`, `-3` or a
/// money value (`$1,234.50`).
fn number_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim();
            t.parse::<f64>().ok().or_else(|| {
                let negative = t.starts_with('-') || (t.starts_with('(') && t.ends_with(')'));
                let digits: String = t.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
                let x = digits.parse::<f64>().ok()?;
                Some(if negative { -x } else { x })
            })
        }
        _ => None,
    }
    .filter(|x| x.is_finite())
}

/// Milliseconds of a time of day (`10:30`, `10:30:15.5`).
fn time_of_day(s: &str) -> Option<i64> {
    parse_ts(&format!("1970-01-01T{}", s.trim()))
}

/// `value` as the property type expects it. `None` when it cannot be read
/// as that type.
pub fn coerce(def: &PropertyDef, value: &Value) -> Option<Value> {
    if value.is_null() {
        return Some(Value::Null);
    }
    match def.ty {
        PropType::String | PropType::Text => Some(match value {
            Value::String(_) => value.clone(),
            other => json!(text_of(other)?),
        }),
        PropType::Enum => text_of(value).map(Value::String),
        PropType::Int => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => Some(value.clone()),
            Value::Bool(b) => Some(json!(i64::from(*b))),
            other => {
                let x = number_of(other)?;
                (x.fract() == 0.0 && x.abs() < 9.0e15).then(|| json!(x as i64))
            }
        },
        PropType::Float => match value {
            Value::Number(_) => Some(value.clone()),
            other => number_of(other).map(|x| json!(x)),
        },
        PropType::Bool => match value {
            Value::Bool(_) => Some(value.clone()),
            Value::Number(n) => match n.as_i64() {
                Some(0) => Some(json!(false)),
                Some(1) => Some(json!(true)),
                _ => None,
            },
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "yes" | "y" | "1" | "on" => Some(json!(true)),
                "false" | "f" | "no" | "n" | "0" | "off" => Some(json!(false)),
                _ => None,
            },
            _ => None,
        },
        PropType::Timestamp => match value {
            Value::Number(n) => n.as_i64().map(|x| json!(x)),
            Value::String(s) => parse_ts(s).or_else(|| time_of_day(s)).map(|x| json!(x)),
            _ => None,
        },
        PropType::List => Some(match value {
            Value::Array(_) => value.clone(),
            other => json!([other]),
        }),
        PropType::Json => Some(value.clone()),
    }
}

/// Objects for the rows of a table that backs `type_name`.
///
/// Each row becomes `{type}:{key}` titled by the title column (the key when
/// it has none), with the state column as its state and every mapped column
/// as a property, coerced to the property's type (a value that cannot be
/// read as that type is cleared). A mapped column that is absent from the
/// row leaves the property untouched; a null clears it, the state too. Each foreign key
/// column of `backing.links` with a value links the object to the object of
/// the link type's target type with that key. Rows without a key are left
/// out.
pub fn map_rows(
    type_name: &str,
    ty: &ObjectType,
    backing: &Backing,
    schema: &Schema,
    rows: &[Row],
    opts: &RowOptions,
) -> Vec<Mapped> {
    let targets: Vec<(&String, &String, &str)> = backing
        .links
        .iter()
        .filter_map(|(rel, column)| {
            let to = schema.link_types.get(rel)?.to.first()?;
            Some((rel, column, to.as_str()))
        })
        .collect();
    let defs: Vec<(&String, &String, Option<&PropertyDef>)> = backing
        .properties
        .iter()
        .map(|(prop, column)| (prop, column, ty.properties.get(prop).or_else(|| schema.prop_def(type_name, prop))))
        .collect();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(key) = row_key(row, &backing.key) else { continue };
        let id = object_id(type_name, &key);
        let title = backing
            .title
            .as_ref()
            .and_then(|c| row.get(c))
            .and_then(text_of)
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| key.clone());
        // The state column: a value sets the state, a null clears it, a
        // column the row lacks leaves it alone.
        let state_cell = backing.state.as_ref().and_then(|c| row.get(c));
        let state = state_cell.and_then(text_of);
        let mut e = EntityInput {
            id: Some(id.clone()),
            kind: type_name.to_string(),
            workspace: Some(opts.workspace.clone()),
            title: Some(title),
            clear_state: state_cell.is_some() && state.is_none(),
            state,
            provenance: Some(Provenance {
                source: format!("connector:{}", opts.engine),
                connector: Some(opts.connector.clone()),
                external_ref: Some(format!("{}#{key}", backing.table)),
                run: (!opts.run.is_empty()).then(|| opts.run.clone()),
                at: opts.crawled_at,
                ..Default::default()
            }),
            ..Default::default()
        };
        for (prop, column, def) in &defs {
            let Some(v) = row.get(*column) else { continue };
            let v = match def {
                Some(def) => coerce(def, v).unwrap_or(Value::Null),
                None => v.clone(),
            };
            e.props.insert((*prop).clone(), v);
        }
        if let Some(acl) = &opts.acl {
            e.props.insert("visibility".into(), json!("restricted"));
            e.props.insert("acl".into(), json!(acl));
        } else if opts.map_grants {
            e.props.insert("visibility".into(), json!("internal"));
            e.props.insert("acl".into(), json!([]));
        }
        let links = targets
            .iter()
            .filter_map(|(rel, column, to)| {
                let target = row.get(*column).and_then(key_part)?;
                Some(LinkInput::new(&id, (*rel).clone(), object_id(to, &target)))
            })
            .collect();
        out.push(Mapped { input: e, links });
    }
    out
}

/// Links for the rows of a join table: each row links `{from_type}:{from}`
/// to `{to_type}:{to}`. Rows missing either column are left out.
pub fn map_join_rows(link: &str, lb: &LinkBacking, from_type: &str, to_type: &str, rows: &[Row]) -> Vec<LinkInput> {
    let mut seen = HashSet::new();
    rows.iter()
        .filter_map(|row| {
            let from = row.get(&lb.from).and_then(key_part)?;
            let to = row.get(&lb.to).and_then(key_part)?;
            let l = LinkInput::new(object_id(from_type, &from), link, object_id(to_type, &to));
            seen.insert((l.from.clone(), l.to.clone())).then_some(l)
        })
        .collect()
}

/// Mapped objects the schema rejects (a state or enum value it does not
/// know, a required property without a value, an id that is too long), with
/// the reason. The rest are returned as they are.
pub fn check_rows(mapped: Vec<Mapped>, schema: &Schema) -> (Vec<Mapped>, Vec<(Id, String)>) {
    let mut ok = Vec::with_capacity(mapped.len());
    let mut invalid = Vec::new();
    for m in mapped {
        let id = m.id().to_string();
        if id.len() > MAX_ID_LEN {
            invalid.push((id, format!("the id is longer than {MAX_ID_LEN} characters")));
            continue;
        }
        let i = &m.input;
        match schema.validate_entity(&i.kind, i.state.as_deref(), &i.props, true, false) {
            Ok(()) => ok.push(m),
            Err(e) => invalid.push((id, e.to_string())),
        }
    }
    (ok, invalid)
}

/// The mutations that sync one object type, and the fingerprints to keep.
///
/// * `managed_rels`: the type's foreign-key link types; links of them that
///   the rows no longer have are removed.
/// * `later`: ids that other object types of the same sync write after this
///   one. A link to one of them that does not exist yet is skipped now and
///   its object is written again by the next sync, which creates the link.
///   Links to objects that exist nowhere are dropped (a type that is not
///   synced, a row outside `max_rows`), so they cost no writes.
/// * `invalid`: ids of rows [`check_rows`] rejected. Their objects keep
///   what they had: neither written nor deleted.
pub fn plan_objects(
    mut mapped: Vec<Mapped>,
    previous: &Fingerprints,
    graph: &Graph,
    managed_rels: &[&str],
    later: &HashSet<Id>,
    invalid: &BTreeSet<Id>,
) -> CrawlPlan {
    let ids: HashSet<Id> = mapped.iter().map(|m| m.id().to_string()).collect();
    let mut waiting = BTreeSet::new();
    for m in &mut mapped {
        let id = m.id().to_string();
        m.links.retain(|l| {
            if ids.contains(&l.to) || graph.contains(&l.to) {
                true
            } else if later.contains(&l.to) {
                waiting.insert(id.clone());
                true
            } else {
                false
            }
        });
    }
    let mut prev = previous.clone();
    prev.retain(|id, _| !invalid.contains(id) && !waiting.contains(id));
    let mut plan = catalog::plan_changes(mapped, &prev, graph, managed_rels);
    for id in waiting {
        // Never matches a real fingerprint: the object is planned again.
        plan.fingerprints.insert(id, 0);
    }
    for id in invalid {
        if let Some(fp) = previous.get(id) {
            plan.fingerprints.insert(id.clone(), *fp);
        }
    }
    plan
}

/// The mutations that turn the links of the previous sync into `new`:
/// links that are new are added, links that vanished are removed. Returns
/// them with the keys to keep for the next sync.
pub fn plan_links(new: Vec<LinkInput>, previous: &BTreeSet<LinkKey>) -> (Vec<Mutation>, BTreeSet<LinkKey>) {
    let mut keys = BTreeSet::new();
    let mut muts = Vec::new();
    for l in new {
        let key: LinkKey = (l.from.clone(), l.rel.clone(), l.to.clone());
        if !keys.insert(key.clone()) {
            continue;
        }
        if !previous.contains(&key) {
            muts.push(Mutation::Link(l));
        }
    }
    for (from, rel, to) in previous.difference(&keys) {
        muts.push(Mutation::Unlink(LinkInput::new(from, rel, to)));
    }
    (muts, keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ActionContext;
    use crate::policy::Principal;
    use crate::{Ontology, schema::Dir};

    fn schema() -> Schema {
        Schema::from_toml(
            r#"
            [object_types.customer]
            properties.email = { type = "string" }
            backing = { source = "wh", table = "sales.customers", key = ["id"], sync = true, title = "name", properties = { email = "email" } }

            [object_types.order]
            states = ["placed", "paid", "cancelled"]
            properties.amount = { type = "float" }
            properties.qty = { type = "int" }
            properties.gift = { type = "bool" }
            properties.placed_at = { type = "timestamp" }
            properties.channel = { type = "enum", values = ["web", "store"] }
            properties.extra = { type = "json" }
            properties.tags = { type = "list" }
            properties.notes = { type = "text" }
            properties.code = { type = "string", required = true }

            [object_types.order.backing]
            source = "wh"
            table = "sales.orders"
            key = ["id"]
            sync = true
            title = "code"
            state = "status"
            links = { customer = "customer_id" }

            [object_types.order.backing.properties]
            amount = "amount"
            qty = "qty"
            gift = "gift"
            placed_at = "placed_at"
            channel = "channel"
            extra = "extra"
            tags = "tags"
            notes = "notes"
            code = "code"

            [object_types.line]
            properties.qty = { type = "int" }
            backing = { source = "wh", table = "sales.order_items", key = ["order_id", "sku"], sync = true, properties = { qty = "qty" }, links = { line_of = "order_id" } }

            [object_types.tag]

            [link_types.customer]
            from = ["order"]
            to = ["customer"]
            inverse = "orders"
            cardinality = "one"

            [link_types.line_of]
            from = ["line"]
            to = ["order"]
            cardinality = "one"

            [link_types.tags_of]
            from = ["order"]
            to = ["tag"]
            backing = { source = "wh", table = "sales.order_tags", from = "order_id", to = "tag_id" }
            "#,
        )
        .unwrap()
    }

    fn rows(v: Value) -> Vec<Row> {
        v.as_array().unwrap().iter().map(|r| r.as_object().unwrap().clone()).collect()
    }

    fn opts() -> RowOptions {
        RowOptions {
            workspace: "acme".into(),
            connector: "wh".into(),
            engine: "postgres".into(),
            crawled_at: 5,
            run: "crawl_1".into(),
            ..Default::default()
        }
    }

    fn map(s: &Schema, kind: &str, r: Value, o: &RowOptions) -> Vec<Mapped> {
        let ty = &s.object_types[kind];
        map_rows(kind, ty, ty.backing.as_ref().unwrap(), s, &rows(r), o)
    }

    #[test]
    fn rows_become_typed_objects_with_links() {
        let s = schema();
        let m = map(
            &s,
            "order",
            json!([
                { "id": 7, "code": "A-7", "status": "paid", "customer_id": 1, "amount": "12.50", "qty": 3.0,
                  "gift": "t", "placed_at": "2026-10-17T14:00:00+02:00", "channel": "web",
                  "extra": { "src": "ad" }, "tags": "x", "notes": 42 },
                { "id": 8, "code": "A-8", "status": "placed", "customer_id": null, "amount": "$1,234.50",
                  "qty": "4", "gift": 0, "placed_at": "2026-10-17", "channel": "store", "notes": null },
                { "id": null, "code": "lost" },
                { "id": 9, "code": null, "qty": 2.5, "gift": "maybe", "placed_at": "someday" }
            ]),
            &opts(),
        );
        assert_eq!(m.len(), 3, "the row without a key is left out");
        let a = &m[0].input;
        assert_eq!(a.id.as_deref(), Some("order:7"));
        assert_eq!(a.kind, "order");
        assert_eq!(a.title.as_deref(), Some("A-7"));
        assert_eq!(a.state.as_deref(), Some("paid"));
        assert_eq!(a.props["amount"], json!(12.5));
        assert_eq!(a.props["qty"], json!(3));
        assert_eq!(a.props["gift"], json!(true));
        assert_eq!(a.props["placed_at"], json!(1_792_238_400_000_i64));
        assert_eq!(a.props["channel"], json!("web"));
        assert_eq!(a.props["extra"], json!({ "src": "ad" }));
        assert_eq!(a.props["tags"], json!(["x"]));
        assert_eq!(a.props["notes"], json!("42"));
        assert!(!a.props.contains_key("visibility"), "no grants mapped");
        let p = a.provenance.as_ref().unwrap();
        assert_eq!(p.source, "connector:postgres");
        assert_eq!(p.connector.as_deref(), Some("wh"));
        assert_eq!(p.external_ref.as_deref(), Some("sales.orders#7"));
        assert_eq!(p.run.as_deref(), Some("crawl_1"));
        assert_eq!(p.at, 5);
        assert_eq!(m[0].links.len(), 1);
        assert_eq!((m[0].links[0].rel.as_str(), m[0].links[0].to.as_str()), ("customer", "customer:1"));

        let b = &m[1].input;
        assert_eq!(b.props["amount"], json!(1234.5), "money text");
        assert_eq!(b.props["qty"], json!(4));
        assert_eq!(b.props["gift"], json!(false));
        assert_eq!(b.props["placed_at"], json!(1_792_195_200_000_i64), "a date is midnight UTC");
        assert_eq!(b.props["notes"], Value::Null, "a null clears the property");
        assert!(!b.props.contains_key("extra"), "a column the row lacks leaves the property alone");
        assert!(m[1].links.is_empty(), "a null foreign key links nothing");

        let c = &m[2].input;
        assert_eq!(c.title.as_deref(), Some("9"), "untitled rows show their key");
        for prop in ["qty", "gift", "placed_at"] {
            assert_eq!(c.props[prop], Value::Null, "{prop} cannot be read as its type");
        }

        // The schema accepts the first two; the third lacks its required code.
        let (ok, invalid) = check_rows(m, &s);
        assert_eq!(ok.len(), 2);
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].0, "order:9");
        assert!(invalid[0].1.contains("code is required"), "{:?}", invalid[0]);
        let (_, bad_state) =
            check_rows(map(&s, "order", json!([{ "id": 1, "code": "x", "status": "lost" }]), &opts()), &s);
        assert!(bad_state[0].1.contains("state 'lost'"), "{bad_state:?}");
    }

    #[test]
    fn composite_keys_strings_and_acls() {
        let s = schema();
        let restricted = RowOptions { acl: Some(vec!["team:analytics".into()]), map_grants: true, ..opts() };
        let m = map(
            &s,
            "line",
            json!([
                { "order_id": 7, "sku": "SKU-1", "qty": 2 },
                { "order_id": 7, "sku": null, "qty": 1 },
                { "order_id": 8, "sku": "", "qty": 1 }
            ]),
            &restricted,
        );
        assert_eq!(m.len(), 1, "keys need every column");
        assert_eq!(m[0].id(), "line:7.SKU-1");
        assert_eq!(m[0].input.title.as_deref(), Some("7.SKU-1"));
        assert_eq!(m[0].input.props["visibility"], "restricted");
        assert_eq!(m[0].input.props["acl"], json!(["team:analytics"]));
        assert_eq!(m[0].links[0].to, "order:7");
        let public = RowOptions { map_grants: true, ..opts() };
        let c = map(&s, "customer", json!([{ "id": "c-1", "name": "Ada", "email": "ada@x.test" }]), &public);
        assert_eq!(c[0].id(), "customer:c-1");
        assert_eq!(c[0].input.title.as_deref(), Some("Ada"));
        assert_eq!(c[0].input.props["visibility"], "internal", "a public table lifts a restriction");
        assert_eq!(row_key(&rows(json!([{ "a": true, "b": 1.5 }]))[0], &["a".into(), "b".into()]).unwrap(), "true.1.5");
    }

    #[test]
    fn join_rows_and_link_plans() {
        let s = schema();
        let lb = s.link_types["tags_of"].backing.clone().unwrap();
        let links = map_join_rows(
            "tags_of",
            &lb,
            "order",
            "tag",
            &rows(json!([
                { "order_id": 7, "tag_id": 1 },
                { "order_id": 7, "tag_id": 1 },
                { "order_id": 7, "tag_id": 2 },
                { "order_id": 8, "tag_id": null }
            ])),
        );
        let keys: Vec<(&str, &str)> = links.iter().map(|l| (l.from.as_str(), l.to.as_str())).collect();
        assert_eq!(keys, [("order:7", "tag:1"), ("order:7", "tag:2")], "duplicates and nulls dropped");

        let (muts, first) = plan_links(links.clone(), &BTreeSet::new());
        assert_eq!(muts.len(), 2);
        assert!(muts.iter().all(|m| matches!(m, Mutation::Link(_))));
        let (muts, again) = plan_links(links, &first);
        assert!(muts.is_empty(), "nothing changed");
        assert_eq!(again, first);
        let (muts, third) = plan_links(vec![LinkInput::new("order:7", "tags_of", "tag:3")], &again);
        assert_eq!(third.len(), 1);
        let linked: Vec<&str> =
            muts.iter().filter_map(|m| if let Mutation::Link(l) = m { Some(l.to.as_str()) } else { None }).collect();
        let unlinked: Vec<&str> =
            muts.iter().filter_map(|m| if let Mutation::Unlink(l) = m { Some(l.to.as_str()) } else { None }).collect();
        assert_eq!(linked, ["tag:3"]);
        assert_eq!(unlinked, ["tag:1", "tag:2"]);
    }

    fn apply(o: &Ontology, plan: &CrawlPlan) {
        o.apply(&ActionContext::new(Principal::admin("crawler"), "acme"), plan.mutations.clone()).unwrap();
    }

    #[test]
    fn object_plans_are_incremental_and_keep_links_in_step() {
        let s = schema();
        let o = Ontology::in_memory(s.clone());
        let none = HashSet::new();
        let no_invalid = BTreeSet::new();
        let customers = map(&s, "customer", json!([{ "id": 1, "name": "Ada" }, { "id": 2, "name": "Bo" }]), &opts());
        let order_rows = |cust: Value, code: &str| {
            json!([
                { "id": 7, "code": code, "status": "paid", "customer_id": cust, "amount": 10 },
                { "id": 8, "code": "B", "status": "placed", "customer_id": 1 },
                { "id": 9, "code": "C", "status": "placed", "customer_id": 99 }
            ])
        };

        // Orders first: customers are written later in the same sync.
        let later: HashSet<Id> = customers.iter().map(|m| m.id().to_string()).collect();
        let orders = map(&s, "order", order_rows(json!(1), "A"), &opts());
        let plan = o.read(|g, _| plan_objects(orders, &Fingerprints::new(), g, &["customer"], &later, &no_invalid));
        assert_eq!(plan.upserted, 3);
        assert_eq!(plan.links_skipped, 2, "customer:1 does not exist yet");
        apply(&o, &plan);
        let first_orders = plan.fingerprints;
        assert_eq!(first_orders["order:7"], 0, "written again next time");
        assert_ne!(first_orders["order:9"], 0, "customer:99 exists nowhere: dropped, not retried");
        let plan = o.read(|g, _| plan_objects(customers.clone(), &Fingerprints::new(), g, &[], &none, &no_invalid));
        apply(&o, &plan);
        let first_customers = plan.fingerprints;

        // The next sync creates the links that waited.
        let orders = map(&s, "order", order_rows(json!(1), "A"), &opts());
        let plan = o.read(|g, _| plan_objects(orders, &first_orders, g, &["customer"], &none, &no_invalid));
        assert_eq!((plan.upserted, plan.unchanged), (2, 1), "{plan:?}");
        apply(&o, &plan);
        let linked = |id: &str| {
            o.read(|g, _| g.neighbors(id, "customer", Dir::Out).iter().map(|x| x.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(linked("order:7"), ["customer:1"]);
        let synced = plan.fingerprints;
        assert!(synced.values().all(|fp| *fp != 0));

        // Unchanged rows cost nothing.
        let orders = map(&s, "order", order_rows(json!(1), "A"), &opts());
        let plan = o.read(|g, _| plan_objects(orders, &synced, g, &["customer"], &none, &no_invalid));
        assert!(plan.mutations.is_empty() && plan.unchanged == 3, "{plan:?}");

        // A changed foreign key moves the link; a null removes it.
        let orders = map(&s, "order", order_rows(json!(2), "A"), &opts());
        let plan = o.read(|g, _| plan_objects(orders, &synced, g, &["customer"], &none, &no_invalid));
        assert_eq!(plan.upserted, 1);
        apply(&o, &plan);
        assert_eq!(linked("order:7"), ["customer:2"]);
        let moved = plan.fingerprints;
        let orders = map(&s, "order", order_rows(Value::Null, "A"), &opts());
        let plan = o.read(|g, _| plan_objects(orders, &moved, g, &["customer"], &none, &no_invalid));
        assert_eq!(plan.links_removed, 1);
        apply(&o, &plan);
        assert!(linked("order:7").is_empty());
        let cleared = plan.fingerprints;

        // A row the schema rejects keeps its object; a vanished row is deleted.
        let orders = map(
            &s,
            "order",
            json!([{ "id": 7, "code": "A", "status": "bogus" }, { "id": 9, "code": "C", "status": "placed", "customer_id": 99 }]),
            &opts(),
        );
        let (ok, bad) = check_rows(orders, &s);
        let invalid: BTreeSet<Id> = bad.into_iter().map(|(id, _)| id).collect();
        let plan = o.read(|g, _| plan_objects(ok, &cleared, g, &["customer"], &none, &invalid));
        assert_eq!((plan.upserted, plan.unchanged, plan.deleted), (0, 1, 1), "{plan:?}");
        assert_eq!(plan.fingerprints.get("order:7"), cleared.get("order:7"), "kept for the next sync");
        apply(&o, &plan);
        o.read(|g, _| {
            assert!(g.contains("order:7") && g.contains("order:9") && !g.contains("order:8"));
            assert_eq!(g.get("order:7").unwrap().state.as_deref(), Some("paid"));
        });
        assert_eq!(first_customers.len(), 2);
    }

    #[test]
    fn a_state_change_alone_is_synced_and_a_null_state_clears_it() {
        let s = schema();
        let o = Ontology::in_memory(s.clone());
        let none = HashSet::new();
        let no_invalid = BTreeSet::new();
        let sync = |status: Value, previous: &Fingerprints| {
            let orders = map(&s, "order", json!([{ "id": 7, "code": "A", "status": status }]), &opts());
            let plan = o.read(|g, _| plan_objects(orders, previous, g, &[], &none, &no_invalid));
            apply(&o, &plan);
            plan
        };
        let state = || o.read(|g, _| g.get("order:7").and_then(|e| e.state.clone()));
        let first = sync(json!("placed"), &Fingerprints::new());
        assert_eq!(state().as_deref(), Some("placed"));

        // Only the status column changed: the object is written again.
        let paid = sync(json!("paid"), &first.fingerprints);
        assert_eq!((paid.upserted, paid.unchanged), (1, 0), "{paid:?}");
        assert_eq!(state().as_deref(), Some("paid"));
        let same = sync(json!("paid"), &paid.fingerprints);
        assert_eq!((same.upserted, same.unchanged), (0, 1));

        // A null status clears the state rather than keeping the old one.
        let cleared = sync(Value::Null, &same.fingerprints);
        assert_eq!(cleared.upserted, 1, "{cleared:?}");
        assert_eq!(state(), None);
        let m = map(&s, "order", json!([{ "id": 7, "code": "A" }]), &opts());
        assert!(!m[0].input.clear_state && m[0].input.state.is_none(), "a column the row lacks changes nothing");
    }
}
