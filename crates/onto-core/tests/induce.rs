//! Sheet induction: one CSV sheet in, one object type (with key, title,
//! state, links and a shape) out, then the rows ingested through the
//! lexical guard.

mod common;

use std::collections::BTreeMap;

use serde_json::{Value, json};

use onto_core::induce::{
    self, ColumnRole, FindingKind, InduceOptions, Induced, LinkMapping, MAX_BYTES, MAX_ROWS, SheetMapping,
};
use onto_core::live_schema::SchemaChange;
use onto_core::schema::{Cardinality, PropType, PropertyDef};
use onto_core::{
    ActionContext, EntityInput, Error, EventFilter, Mutation, Ontology, OntologyOptions, Principal, Schema,
};

fn admin() -> Principal {
    Principal::admin("ada")
}

const SHOP: &str = r#"
[object_types.customer]
label = "Customer"
description = "Someone who buys."

[object_types.customer.properties.segment]
type = "string"
"#;

fn schema() -> Schema {
    let mut s = Schema::enterprise();
    s.extend_toml(SHOP).unwrap();
    s
}

/// Ten customers in `acme`, one in `globex`, and issues keyed 1..5.
fn world() -> (Ontology, ActionContext) {
    let o = Ontology::in_memory(schema());
    let ctx = ActionContext::new(admin(), "acme");
    let mut muts: Vec<Mutation> = (1..=10)
        .map(|i| Mutation::Upsert(EntityInput::new("customer", format!("c{i}"), format!("Customer {i}"))))
        .collect();
    muts.extend((1..=5).map(|i| Mutation::Upsert(EntityInput::new("issue", i.to_string(), format!("Issue {i}")))));
    o.apply(&ctx, muts).unwrap();
    o.apply(
        &ActionContext::new(admin(), "globex"),
        vec![Mutation::Upsert(EntityInput::new("customer", "g1", "Globex Customer"))],
    )
    .unwrap();
    (o, ctx)
}

fn opts(file: &str) -> InduceOptions {
    InduceOptions { file: Some(file.into()), workspace: Some("acme".into()), ..Default::default() }
}

fn column<'a>(ind: &'a Induced, name: &str) -> &'a induce::InducedColumn {
    ind.columns.iter().find(|c| c.column == name).unwrap_or_else(|| panic!("no column {name}: {:?}", ind.columns))
}

fn apply(o: &Ontology, ctx: &ActionContext, ind: &Induced) {
    let plan = o.schema_plan(&ctx.principal, &ind.change_set).unwrap();
    assert!(plan.valid && plan.applicable(), "{:?} {:?}", plan.errors, plan.needs_migration);
    o.schema_apply(ctx, ind.change_set.clone()).unwrap();
}

/// A product sheet with one column of every type.
fn products() -> String {
    let mut s = String::from("product_id,name,qty,price,active,launched,category,notes,colour,status,empty_col\n");
    let cats = ["tools", "toys", "food"];
    for i in 1..=20 {
        let qty = if i == 7 { String::new() } else { i.to_string() };
        let price = if i % 2 == 0 { format!("{i}.5") } else { i.to_string() };
        let active = ["yes", "no", "TRUE", "false"][i % 4];
        let launched = if i % 3 == 0 { format!("2026-01-{i:02}T08:30:00Z") } else { format!("2026-02-{i:02}") };
        let notes = format!(
            "Product {i} is described at length so that the average note is clearly longer than eighty characters."
        );
        let status = if i % 4 == 0 { "retired" } else { "active" };
        s.push_str(&format!(
            "P{i:03},Widget {i},{qty},{price},{active},{launched},{},\"{notes}\",colour {i},{status},\n",
            cats[i % 3]
        ));
    }
    s
}

/// What a shape demands, checked against the stored objects (a stand-in for
/// the shapes report: min/max count, `in`, datatype and class).
fn conforms(o: &Ontology, ind: &Induced) -> Vec<String> {
    let p = admin();
    let mut problems = Vec::new();
    let (entities, _) = o
        .find(&p, &onto_core::query::FindSpec { kind: Some(ind.kind.clone()), limit: 100_000, ..Default::default() })
        .unwrap();
    assert!(!entities.is_empty(), "no {} objects", ind.kind);
    let schema = o.schema();
    for e in &entities {
        for ps in &ind.shape.properties {
            let values: Vec<Value> = match ps.path.as_str() {
                "title" => [e.title.clone()].into_iter().filter(|t| !t.is_empty()).map(Value::from).collect(),
                "state" => e.state.clone().map(Value::from).into_iter().collect(),
                p if p.starts_with("prop.") => {
                    e.props.get(&p[5..]).filter(|v| !v.is_null()).cloned().into_iter().collect()
                }
                rel => o
                    .neighbors(&admin(), &e.id, Some(rel))
                    .unwrap()
                    .into_iter()
                    .filter(|(r, _)| r == rel)
                    .map(|(_, n)| {
                        if let Some(c) = &ps.class
                            && !schema.kind_is(&n.kind, c)
                        {
                            problems.push(format!("{} {}: {} is not a {c}", e.id, ps.id, n.id));
                        }
                        Value::from(n.id)
                    })
                    .collect(),
            };
            if ps.min_count.is_some_and(|m| values.len() < m) {
                problems.push(format!("{} {}: {} values, min_count {:?}", e.id, ps.id, values.len(), ps.min_count));
            }
            if ps.max_count.is_some_and(|m| values.len() > m) {
                problems.push(format!("{} {}: {} values, max_count {:?}", e.id, ps.id, values.len(), ps.max_count));
            }
            for v in &values {
                if !ps.values.is_empty() && !ps.values.contains(v) {
                    problems.push(format!("{} {}: {v} not in {:?}", e.id, ps.id, ps.values));
                }
                if let Some(ty) = ps.datatype
                    && !(PropertyDef { ty, ..Default::default() }).accepts(v)
                {
                    problems.push(format!("{} {}: {v} is not {ty:?}", e.id, ps.id));
                }
            }
        }
    }
    problems
}

#[test]
fn every_column_type_is_read_from_its_values() {
    let (o, _) = world();
    let ind = o.induce_sheet(&admin(), &products(), &opts("products.csv")).unwrap();
    assert_eq!(ind.kind, "product");
    assert_eq!(ind.rows, 20);
    assert_eq!(ind.key.column.as_deref(), Some("product_id"), "{:?}", ind.key);
    assert_eq!(ind.title.column.as_deref(), Some("name"), "{:?}", ind.title);
    let expect = [
        ("product_id", ColumnRole::Key, PropType::String),
        ("name", ColumnRole::Title, PropType::String),
        ("qty", ColumnRole::Property, PropType::Int),
        ("price", ColumnRole::Property, PropType::Float),
        ("active", ColumnRole::Property, PropType::Bool),
        ("launched", ColumnRole::Property, PropType::Timestamp),
        ("category", ColumnRole::Property, PropType::Enum),
        ("notes", ColumnRole::Property, PropType::Text),
        ("colour", ColumnRole::Property, PropType::String),
        ("status", ColumnRole::State, PropType::Enum),
        ("empty_col", ColumnRole::Skipped, PropType::String),
    ];
    for (name, role, ty) in expect {
        let c = column(&ind, name);
        assert_eq!((c.role, c.ty), (role, ty), "{name}: {}", c.evidence);
        assert!(!c.evidence.is_empty(), "{name} has no evidence");
    }
    assert!(!column(&ind, "qty").required, "qty misses a value in row 7");
    assert!(column(&ind, "price").required);
    assert_eq!(column(&ind, "category").values, vec!["food", "tools", "toys"]);
    assert_eq!(column(&ind, "category").distinct, 3);
    assert_eq!(column(&ind, "qty").filled, 19);
    assert!(ind.warnings.iter().any(|w| w.contains("empty_col")), "{:?}", ind.warnings);

    // The change set: the object type first, the shape last.
    let SchemaChange::PutObjectType { name, def, .. } = &ind.change_set.changes[0] else {
        panic!("{:?}", ind.change_set)
    };
    assert_eq!(name, "product");
    assert_eq!(def.states, vec!["active", "retired"]);
    assert_eq!(def.properties["price"].ty, PropType::Float);
    assert!(def.properties["price"].required && !def.properties["qty"].required);
    assert_eq!(def.properties["category"].values, vec!["food", "tools", "toys"]);
    assert!(!def.properties.contains_key("product_id") && !def.properties.contains_key("name"));
    assert!(!def.properties.contains_key("empty_col"));
    assert!(matches!(ind.change_set.changes.last(), Some(SchemaChange::PutShape { name, .. }) if name == "product"));

    // The shape: required columns, enum values, datatypes; never a pattern.
    let by_id: BTreeMap<&str, &onto_core::PropertyShape> =
        ind.shape.properties.iter().map(|p| (p.id.as_str(), p)).collect();
    assert_eq!(ind.shape.target.as_deref(), Some("product"));
    assert_eq!(by_id["product/prop.price"].min_count, Some(1));
    assert_eq!(by_id["product/prop.price"].datatype, Some(PropType::Float));
    assert_eq!(by_id["product/prop.qty"].min_count, None);
    assert_eq!(by_id["product/prop.category"].values, vec![json!("food"), json!("tools"), json!("toys")]);
    assert_eq!(by_id["product/state"].values, vec![json!("active"), json!("retired")]);
    assert_eq!(by_id["product/title"].min_count, Some(1));
    assert!(ind.shape.properties.iter().all(|p| p.pattern.is_none()));
    assert_eq!(ind.shape_evidence.len(), ind.shape.properties.len());
    assert!(ind.means.contains("not what it should be"));

    // Mapping: header names to properties.
    assert_eq!(ind.mapping.state.as_deref(), Some("status"));
    assert_eq!(ind.mapping.properties["notes"], "notes");
    assert!(!ind.mapping.properties.contains_key("empty_col"));
}

#[test]
fn enum_thresholds() {
    let o = Ontology::in_memory(Schema::enterprise());
    let sheet = |values: &[&str]| {
        let mut s = String::from("id,v\n");
        for (i, v) in values.iter().enumerate() {
            s.push_str(&format!("r{i},{v}\n"));
        }
        s
    };
    let ty = |values: Vec<String>| {
        let refs: Vec<&str> = values.iter().map(String::as_str).collect();
        let ind = o.induce_sheet(&admin(), &sheet(&refs), &InduceOptions::default()).unwrap();
        column(&ind, "v").ty
    };
    let cycle = |distinct: usize, rows: usize| (0..rows).map(|i| format!("v{}", i % distinct)).collect::<Vec<_>>();
    assert_eq!(ty(cycle(2, 10)), PropType::Enum, "2 values, 10 rows");
    assert_eq!(ty(cycle(2, 9)), PropType::String, "2 values, 9 rows");
    assert_eq!(ty(cycle(12, 60)), PropType::Enum, "12 values, 60 rows");
    assert_eq!(ty(cycle(12, 59)), PropType::String, "12 values, 59 rows");
    assert_eq!(ty(cycle(13, 100)), PropType::String, "13 values");
    assert_eq!(ty(cycle(1, 30)), PropType::String, "one value");
    // Empty cells do not count as rows of the enum.
    let mut sparse = cycle(2, 10);
    sparse.extend(std::iter::repeat_n(String::new(), 5));
    assert_eq!(ty(sparse), PropType::Enum);
    let mut too_sparse = cycle(2, 9);
    too_sparse.extend(std::iter::repeat_n(String::new(), 5));
    assert_eq!(ty(too_sparse), PropType::String);
    // Mixed numbers are floats; leading zeros are identifiers, not numbers.
    assert_eq!(ty(vec!["1".into(), "2.5".into()]), PropType::Float);
    assert_eq!(ty(vec!["007".into(), "12".into()]), PropType::String);
    assert_eq!(ty(vec!["2026-10-24".into(), "2026-10-24T09:00:00+02:00".into()]), PropType::Timestamp);
    assert_eq!(ty(vec!["2026-10-24".into(), "24/10/2026".into()]), PropType::String);
}

#[test]
fn key_and_kind_preferences() {
    let o = Ontology::in_memory(Schema::enterprise());
    let p = admin();
    // A key-like name wins over an earlier unique column.
    let csv = "email,customer_code,city\na@x.io,C1,Oslo\nb@x.io,C2,Oslo\nc@x.io,C3,Rome\n";
    let ind = o.induce_sheet(&p, csv, &InduceOptions::default()).unwrap();
    assert_eq!(ind.key.column.as_deref(), Some("customer_code"), "{:?}", ind.key);
    assert_eq!(ind.kind, "customer", "{}", ind.kind_evidence);
    assert!(ind.key.evidence.contains("unique in all 3 rows"), "{}", ind.key.evidence);
    assert_eq!(ind.title.column.as_deref(), Some("email"), "the first string column that is not the key");
    // `id` itself beats another key-like column; `ID` reads as `id`.
    let csv = "ext_ref_id,ID,label\nx1,1,One\nx2,2,Two\n";
    let ind = o.induce_sheet(&p, csv, &InduceOptions { kind: Some("thing".into()), ..Default::default() }).unwrap();
    assert_eq!(ind.key.column.as_deref(), Some("ID"));
    assert_eq!(ind.title.column.as_deref(), Some("label"));
    // A repeated or partly empty column is not a key; with none, rows are numbered.
    let csv = "colour,size\nred,1\nred,2\n,3\n";
    let ind = o.induce_sheet(&p, csv, &InduceOptions::default()).unwrap();
    assert_eq!(ind.key.column.as_deref(), Some("size"), "the only filled, unique column");
    let csv = "colour,size\nred,1\nred,1\n";
    let ind = o.induce_sheet(&p, csv, &InduceOptions::default()).unwrap();
    assert_eq!(ind.key.column, None);
    assert_eq!(ind.mapping.key, None);
    assert!(ind.warnings.iter().any(|w| w.contains("row number")), "{:?}", ind.warnings);
    assert_eq!(ind.kind, "record");
    // A caller's key must be filled and unique.
    let err = o
        .induce_sheet(&p, "a,b\n1,x\n1,y\n", &InduceOptions { key: Some("a".into()), ..Default::default() })
        .unwrap_err();
    assert!(matches!(&err, Error::Invalid(m) if m.contains("repeats")), "{err}");
    let err =
        o.induce_sheet(&p, "a,b\n1,x\n", &InduceOptions { key: Some("zz".into()), ..Default::default() }).unwrap_err();
    assert!(matches!(err, Error::Invalid(_)));
    // A caller's kind must be a valid name; an existing name gets a suffix.
    let err = o
        .induce_sheet(&p, "a\n1\n", &InduceOptions { kind: Some("Bad Name".into()), ..Default::default() })
        .unwrap_err();
    assert!(matches!(err, Error::Invalid(_)));
    let ind =
        o.induce_sheet(&p, "a\n1\n", &InduceOptions { kind: Some("issue".into()), ..Default::default() }).unwrap();
    assert_eq!(ind.kind, "issue_2");
    assert!(ind.warnings.iter().any(|w| w.contains("issue exists already")), "{:?}", ind.warnings);
    // The file name names the kind (singular, snake_case).
    let ind = o
        .induce_sheet(&p, "a\n1\n", &InduceOptions { file: Some("/tmp/Order Lines.csv".into()), ..Default::default() })
        .unwrap();
    assert_eq!(ind.kind, "order_line");
    // A header without rows describes nothing.
    assert!(matches!(o.induce_sheet(&p, "a,b\n", &InduceOptions::default()), Err(Error::Invalid(_))));
}

fn orders() -> String {
    let mut s = String::from("order_id,customer_id,quantity,amount\n");
    for i in 1..=20 {
        // c99 names no customer; g1 is a customer of another workspace.
        let customer = match i {
            20 => "c99".to_string(),
            _ => format!("c{}", (i % 10) + 1),
        };
        s.push_str(&format!("o{i},{customer},{},{}.25\n", (i % 5) + 1, i * 10));
    }
    s
}

#[test]
fn a_column_of_customer_keys_becomes_a_link() {
    let (o, ctx) = world();
    let ind = o.induce_sheet(&admin(), &orders(), &opts("orders.csv")).unwrap();
    assert_eq!(ind.kind, "order");
    assert_eq!(ind.key.column.as_deref(), Some("order_id"));
    assert_eq!(ind.links.len(), 1, "{:?}", ind.links);
    let l = &ind.links[0];
    assert_eq!((l.column.as_str(), l.rel.as_str(), l.target_kind.as_str()), ("customer_id", "customer", "customer"));
    assert_eq!((l.matched, l.filled), (19, 20));
    assert!(l.new_link_type && !l.self_link);
    assert_eq!(l.inverse.as_deref(), Some("orders"));
    assert!(l.evidence.contains("19 of 20"), "{}", l.evidence);
    assert!(ind.warnings.iter().any(|w| w.contains("\"c99\"")), "{:?}", ind.warnings);
    // `quantity` holds 1..5, which are issue keys, but its name refers to no issue.
    assert_eq!(column(&ind, "quantity").role, ColumnRole::Property);
    assert_eq!(column(&ind, "quantity").ty, PropType::Int);
    let lt = ind
        .change_set
        .changes
        .iter()
        .find_map(|c| match c {
            SchemaChange::PutLinkType { name, def, .. } if name == "customer" => Some(def),
            _ => None,
        })
        .expect("a put_link_type");
    assert_eq!(
        (lt.from.clone(), lt.to.clone(), lt.cardinality),
        (vec!["order".into()], vec!["customer".into()], Cardinality::One)
    );
    // 19 of 20 rows name an existing customer: no min_count on the link.
    let link_shape = ind.shape.properties.iter().find(|p| p.path == "customer").unwrap();
    assert_eq!(
        (link_shape.class.as_deref(), link_shape.min_count, link_shape.max_count),
        (Some("customer"), None, Some(1))
    );
    assert_eq!(
        ind.mapping.links["customer_id"],
        LinkMapping { rel: "customer".into(), target_kind: "customer".into() }
    );

    // Only the workspace's own customers count.
    let globex = "order_id,customer_id\no1,g1\no2,g1\n";
    let ind2 = o.induce_sheet(&admin(), globex, &opts("orders.csv")).unwrap();
    assert!(ind2.links.is_empty(), "{:?}", ind2.links);

    // Apply and ingest: the unknown customer is reported, the rest linked.
    apply(&o, &ctx, &ind);
    let out = o.ingest_sheet(&ctx, &orders(), &ind.mapping, false).unwrap();
    assert_eq!((out.rows, out.upserts, out.links), (20, 20, 19));
    assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
    assert_eq!(out.findings[0].issue, FindingKind::MissingTarget);
    assert_eq!(out.findings[0].row, 20);
    let order = o.get(&admin(), "order:o3").unwrap();
    assert_eq!(order.props["amount"], json!(30.25));
    assert_eq!(order.props["quantity"], json!(4));
    let n = o.neighbors(&admin(), "order:o3", Some("customer")).unwrap();
    assert_eq!(n.iter().map(|(_, e)| e.id.as_str()).collect::<Vec<_>>(), vec!["customer:c4"]);
    let back = o.neighbors(&admin(), "customer:c4", Some("orders")).unwrap();
    assert!(back.iter().any(|(_, e)| e.id == "order:o3"), "{back:?}");
    assert!(conforms(&o, &ind).is_empty(), "{:?}", conforms(&o, &ind));
}

#[test]
fn a_column_of_the_sheets_own_keys_becomes_a_self_link() {
    let o = Ontology::in_memory(Schema::enterprise());
    let ctx = ActionContext::new(admin(), "acme");
    let csv = "employee_id,full_name,manager_id\ne1,Ada,\ne2,Bo,e1\ne3,Cy,e1\ne4,Di,e2\n";
    let ind = o
        .induce_sheet(&admin(), csv, &InduceOptions { file: Some("employees.csv".into()), ..Default::default() })
        .unwrap();
    assert_eq!(ind.kind, "employee");
    assert_eq!(ind.title.column.as_deref(), Some("full_name"));
    let l = &ind.links[0];
    assert_eq!((l.column.as_str(), l.rel.as_str(), l.target_kind.as_str()), ("manager_id", "reports_to", "employee"));
    assert!(l.self_link && l.new_link_type);
    assert_eq!(l.inverse.as_deref(), Some("direct_reports"));
    assert_eq!((l.matched, l.filled), (3, 3));
    apply(&o, &ctx, &ind);
    let out = o.ingest_sheet(&ctx, csv, &ind.mapping, false).unwrap();
    assert!(out.findings.is_empty(), "{:?}", out.findings);
    assert_eq!((out.upserts, out.links), (4, 3));
    let reports = o.neighbors(&admin(), "employee:e1", Some("direct_reports")).unwrap();
    let mut ids: Vec<&str> = reports.iter().map(|(_, e)| e.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["employee:e2", "employee:e3"]);
    assert!(conforms(&o, &ind).is_empty(), "{:?}", conforms(&o, &ind));

    // Integer self references need a name that refers to a row: `parent`
    // links, `rank` (whose values happen to be keys too) stays a property.
    let csv = "id,label,parent,rank\n1,Root,,2\n2,Tools,1,1\n3,Hammers,2,3\n4,Saws,2,1\n";
    let ind = o
        .induce_sheet(&admin(), csv, &InduceOptions { file: Some("categories.csv".into()), ..Default::default() })
        .unwrap();
    assert_eq!(ind.kind, "category");
    assert_eq!(ind.links.len(), 1, "{:?}", ind.links);
    assert_eq!((ind.links[0].rel.as_str(), ind.links[0].inverse.as_deref()), ("parent", Some("parent_of")));
    assert_eq!(column(&ind, "rank").role, ColumnRole::Property);
    apply(&o, &ctx, &ind);
    let out = o.ingest_sheet(&ctx, csv, &ind.mapping, false).unwrap();
    assert!(out.findings.is_empty(), "{:?}", out.findings);
    assert_eq!(o.neighbors(&admin(), "category:3", Some("parent")).unwrap()[0].1.id, "category:2");
    assert!(conforms(&o, &ind).is_empty(), "{:?}", conforms(&o, &ind));
}

#[test]
fn the_lexical_guard_leaves_out_what_does_not_read() {
    let (o, ctx) = world();
    let ind = o.induce_sheet(&admin(), &products(), &opts("products.csv")).unwrap();
    apply(&o, &ctx, &ind);
    let mut mapping = ind.mapping.clone();
    mapping.links.insert("buyer".into(), LinkMapping { rel: "customer_of".into(), target_kind: "customer".into() });
    // `customer_of` does not exist: the mapping is refused before any row is read.
    let err = o.ingest_sheet(&ctx, &products(), &mapping, true).unwrap_err();
    assert!(matches!(&err, Error::Invalid(m) if m.contains("customer_of")), "{err}");

    let bad = "product_id,name,qty,price,active,launched,category,notes,colour,status\n\
               P100,Bad numbers,12.5,cheap,maybe,yesterday,garden,x,c,active\n\
               ,No key,1,1,yes,2026-01-01,toys,x,c,active\n\
               P101,Bad state,1,1,yes,2026-01-01,toys,x,c,lost\n\
               P102,Missing price,1,,yes,2026-01-01,toys,x,c,active\n\
               P103,Good,0,-1.5e2,No,2026-01-01T00:00:00Z,food,x,c,retired\n";
    let out = o.ingest_sheet(&ctx, bad, &ind.mapping, false).unwrap();
    let issues: Vec<(usize, FindingKind, Option<&str>)> =
        out.findings.iter().map(|f| (f.row, f.issue, f.column.as_deref())).collect();
    assert_eq!(
        issues,
        vec![
            (1, FindingKind::BadValue, Some("active")),
            (1, FindingKind::BadValue, Some("category")),
            (1, FindingKind::BadValue, Some("launched")),
            (1, FindingKind::BadValue, Some("price")),
            (1, FindingKind::BadValue, Some("qty")),
            (1, FindingKind::MissingRequired, Some("active")),
            (2, FindingKind::MissingKey, Some("product_id")),
            (3, FindingKind::BadState, Some("status")),
            (4, FindingKind::MissingRequired, Some("price")),
        ],
        "{:#?}",
        out.findings
    );
    assert!(out.findings.iter().all(|f| !f.message.is_empty()));
    assert_eq!(out.findings[0].value.as_deref(), Some("maybe"));
    // Row 3 is written without its state; row 5 in full; rows 1, 2 and 4 not at all.
    assert_eq!(out.upserts, 2);
    assert!(o.get(&admin(), "product:P100").is_err());
    let p101 = o.get(&admin(), "product:P101").unwrap();
    assert_eq!(p101.state, None);
    let p103 = o.get(&admin(), "product:P103").unwrap();
    assert_eq!(p103.props["qty"], json!(0));
    assert_eq!(p103.props["price"], json!(-150.0));
    assert_eq!(p103.props["active"], json!(false));
    assert_eq!(p103.props["launched"], json!("2026-01-01T00:00:00Z"));
    assert_eq!(p103.state.as_deref(), Some("retired"));

    // An existing object needs no required values again: a sparse update row
    // fills what it has.
    let update = "product_id,qty\nP103,9\n";
    let mut sparse = ind.mapping.clone();
    sparse.properties.retain(|c, _| c == "qty");
    sparse.title = None;
    sparse.state = None;
    let out = o.ingest_sheet(&ctx, update, &sparse, false).unwrap();
    assert!(out.findings.is_empty(), "{:?}", out.findings);
    assert_eq!(o.get(&admin(), "product:P103").unwrap().props["qty"], json!(9));

    // A link to itself is refused cell by cell.
    let csv = "employee_id,manager_id\ne1,e1\ne2,e1\ne3,e2\n";
    let ind =
        o.induce_sheet(&admin(), csv, &InduceOptions { kind: Some("staff".into()), ..Default::default() }).unwrap();
    assert_eq!(ind.links.len(), 1, "{:?} {:?}", ind.links, ind.warnings);
    assert_eq!(ind.links[0].matched, 2);
    assert!(ind.warnings.iter().any(|w| w.contains("name themselves")), "{:?}", ind.warnings);
    apply(&o, &ctx, &ind);
    let out = o.ingest_sheet(&ctx, csv, &ind.mapping, false).unwrap();
    assert_eq!(out.findings.iter().map(|f| f.issue).collect::<Vec<_>>(), vec![FindingKind::SelfLink]);
    assert_eq!(out.links, 2);
    assert!(conforms(&o, &ind).is_empty(), "{:?}", conforms(&o, &ind));
}

#[test]
fn quoted_csv_edge_cases() {
    let o = Ontology::in_memory(Schema::enterprise());
    let ctx = ActionContext::new(admin(), "acme");
    let csv = "\u{feff}code;title;remark\r\n\
               a1;\"Semi; colon\";\"He said \"\"hi\"\"\"\r\n\
               a2;\"Two\r\nlines\";plain\r\n\
               \r\n\
               a3;  padded  ;\"\"\r\n";
    let ind =
        o.induce_sheet(&admin(), csv, &InduceOptions { kind: Some("note".into()), ..Default::default() }).unwrap();
    assert_eq!(ind.delimiter, ";");
    assert_eq!(ind.rows, 3, "the blank line is skipped");
    assert_eq!(ind.key.column.as_deref(), Some("code"));
    assert_eq!(ind.title.column.as_deref(), Some("title"));
    assert_eq!(column(&ind, "remark").filled, 2, "an empty quoted cell is absent");
    apply(&o, &ctx, &ind);
    let out = o.ingest_sheet(&ctx, csv, &ind.mapping, false).unwrap();
    assert!(out.findings.is_empty(), "{:?}", out.findings);
    assert_eq!(o.get(&admin(), "note:a1").unwrap().title, "Semi; colon");
    assert_eq!(o.get(&admin(), "note:a1").unwrap().props["remark"], json!("He said \"hi\""));
    assert_eq!(o.get(&admin(), "note:a2").unwrap().title, "Two\r\nlines");
    assert_eq!(o.get(&admin(), "note:a3").unwrap().title, "padded");
    assert!(o.get(&admin(), "note:a3").unwrap().props.get("remark").is_none());

    // Malformed sheets are refused with the line.
    for bad in ["a,b\n\"x,1\n", "a,b\n\"x\"y,1\n", "a,b\n1,2,3\n", "", "a,a\n1,2\n"] {
        assert!(matches!(o.induce_sheet(&admin(), bad, &InduceOptions::default()), Err(Error::Invalid(_))), "{bad:?}");
    }
}

#[test]
fn size_limits_are_refused() {
    let mut rows = String::with_capacity(2 * MAX_ROWS + 10);
    rows.push_str("a\n");
    for _ in 0..=MAX_ROWS {
        rows.push_str("1\n");
    }
    let err = induce::parse_csv(&rows).unwrap_err();
    assert!(matches!(&err, Error::Invalid(m) if m.contains("rows")), "{err}");
    let big = "a\n".to_string() + &"x".repeat(MAX_BYTES);
    let err = induce::parse_csv(&big).unwrap_err();
    assert!(matches!(&err, Error::Invalid(m) if m.contains("bytes")), "{err}");
}

#[test]
fn apply_then_ingest_is_atomic_audited_and_idempotent() {
    let (o, ctx) = world();
    let ind = o.induce_sheet(&admin(), &products(), &opts("products.csv")).unwrap();
    apply(&o, &ctx, &ind);

    // A dry run validates and writes nothing.
    let before = o.revision();
    let dry = o.ingest_sheet(&ctx, &products(), &ind.mapping, true).unwrap();
    assert!(dry.dry_run && dry.revision.is_none());
    assert_eq!((dry.upserts, dry.links), (20, 0));
    assert_eq!(o.revision(), before);
    assert!(o.get(&admin(), "product:P001").is_err());

    let first = o.ingest_sheet(&ctx, &products(), &ind.mapping, false).unwrap();
    assert!(first.findings.is_empty(), "{:?}", first.findings);
    assert_eq!(first.upserts, 20);
    assert!(first.revision.is_some());
    let p4 = o.get(&admin(), "product:P004").unwrap();
    assert_eq!(p4.title, "Widget 4");
    assert_eq!(p4.state.as_deref(), Some("retired"));
    assert_eq!(p4.props["category"], json!("toys"));
    assert_eq!(p4.props["active"], json!(true));
    assert_eq!(p4.props["price"], json!(4.5));
    assert!(p4.props.get("product_id").is_none());
    assert_eq!(p4.provenance.as_ref().map(|p| p.source.as_str()), Some("sheet"));
    assert!(conforms(&o, &ind).is_empty(), "{:?}", conforms(&o, &ind));

    let events =
        o.events(&admin(), &EventFilter { action: Some("ingest.sheet".into()), ..Default::default() }).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["upserts"], 20);
    assert_eq!(events[0].workspace, "acme");

    // The same sheet again changes no object.
    let count = o.stats().unwrap().entities;
    let second = o.ingest_sheet(&ctx, &products(), &ind.mapping, false).unwrap();
    assert!(second.findings.is_empty());
    assert_eq!(o.stats().unwrap().entities, count);
    assert_eq!(o.get(&admin(), "product:P004").unwrap().revision, p4.revision);
    assert_eq!(o.get(&admin(), "product:P004").unwrap(), p4);

    // The orders sheet links into these customers, and re-ingesting it keeps
    // one link per order.
    let orders_ind = o.induce_sheet(&admin(), &orders(), &opts("orders.csv")).unwrap();
    apply(&o, &ctx, &orders_ind);
    o.ingest_sheet(&ctx, &orders(), &orders_ind.mapping, false).unwrap();
    let links = o.stats().unwrap().links;
    o.ingest_sheet(&ctx, &orders(), &orders_ind.mapping, false).unwrap();
    assert_eq!(o.stats().unwrap().links, links);

    // A mapping to an unknown kind is refused (apply the change set first).
    let mut m = ind.mapping.clone();
    m.kind = "gadget".into();
    assert!(matches!(o.ingest_sheet(&ctx, &products(), &m, false), Err(Error::Invalid(_))));

    // The shape describes the sheet, not every product: one written by hand
    // without a title or state is flagged (the check above is not vacuous).
    let p = o.get(&admin(), "product:P001").unwrap();
    let mut untitled = EntityInput { kind: "product".into(), key: Some("P999".into()), ..Default::default() };
    untitled.props = p.props.clone();
    o.apply(&ctx, vec![Mutation::Upsert(untitled)]).unwrap();
    let problems = conforms(&o, &ind);
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].starts_with("product:P999 product/title"), "{problems:?}");
    assert!(problems[1].starts_with("product:P999 product/state"), "{problems:?}");
}

#[test]
fn readers_may_not_ingest() {
    let (o, _) = world();
    let ind = o.induce_sheet(&admin(), &products(), &opts("products.csv")).unwrap();
    o.schema_apply(&ActionContext::new(admin(), "acme"), ind.change_set.clone()).unwrap();
    let viewer = ActionContext::new(Principal::viewer("vic", "acme"), "acme");
    let err = o.ingest_sheet(&viewer, &products(), &ind.mapping, false).unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err}");
    let denied =
        o.events(&admin(), &EventFilter { action: Some("authorization.denied".into()), ..Default::default() }).unwrap();
    assert!(denied.iter().any(|e| e.actor == "vic"), "{denied:?}");
    // Reading is enough to induce.
    assert!(o.induce_sheet(&Principal::viewer("vic", "acme"), &products(), &opts("products.csv")).is_ok());
}

#[test]
fn induction_is_deterministic() {
    let (o, _) = world();
    let a = serde_json::to_string(&o.induce_sheet(&admin(), &orders(), &opts("orders.csv")).unwrap()).unwrap();
    let b = serde_json::to_string(&o.induce_sheet(&admin(), &orders(), &opts("orders.csv")).unwrap()).unwrap();
    assert_eq!(a, b);
    // The mapping round-trips as JSON.
    let ind = o.induce_sheet(&admin(), &orders(), &opts("orders.csv")).unwrap();
    let back: SheetMapping = serde_json::from_value(serde_json::to_value(&ind.mapping).unwrap()).unwrap();
    assert_eq!(back, ind.mapping);
}

#[test]
fn postgres_ingest_survives_a_reopen() {
    let Some(pg) = common::TestTenant::new("induce") else { return };
    let ctx = ActionContext::new(admin(), "acme");
    let ind = {
        let o = pg.open(schema(), OntologyOptions::default());
        o.apply(&ctx, vec![Mutation::Upsert(EntityInput::new("customer", "c1", "Customer 1"))]).unwrap();
        let csv = "order_id,customer_id,total\no1,c1,10\no2,c1,12\n";
        let ind = o.induce_sheet(&admin(), csv, &opts("orders.csv")).unwrap();
        apply(&o, &ctx, &ind);
        let out = o.ingest_sheet(&ctx, csv, &ind.mapping, false).unwrap();
        assert_eq!((out.upserts, out.links), (2, 2));
        ind
    };
    let o = pg.open(schema(), OntologyOptions::default());
    assert!(o.schema().object_types.contains_key(&ind.kind));
    assert_eq!(o.get(&admin(), "order:o2").unwrap().props["total"], json!(12));
    assert_eq!(o.neighbors(&admin(), "order:o1", Some("customer")).unwrap().len(), 1);
    let events =
        o.events(&admin(), &EventFilter { action: Some("ingest.sheet".into()), ..Default::default() }).unwrap();
    assert_eq!(events.len(), 1);
}

#[test]
fn ingest_never_writes_over_another_workspace() {
    let (o, ctx) = world();
    let csv = "order_id,total\no1,10\no2,12\n";
    let ind = o.induce_sheet(&admin(), csv, &opts("orders.csv")).unwrap();
    apply(&o, &ctx, &ind);
    assert_eq!(o.ingest_sheet(&ctx, csv, &ind.mapping, false).unwrap().upserts, 2);
    // The same mapping into globex, whose sheet reuses the key o1.
    let globex = ActionContext::new(admin(), "globex");
    let other = "order_id,total\no1,999\no9,5\n";
    let dry = o.ingest_sheet(&globex, other, &ind.mapping, true).unwrap();
    assert_eq!((dry.upserts, dry.findings.len()), (1, 1), "{:?}", dry.findings);
    let out = o.ingest_sheet(&globex, other, &ind.mapping, false).unwrap();
    assert_eq!(out.upserts, 1);
    assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
    let f = &out.findings[0];
    assert_eq!((f.issue, f.row, f.value.as_deref()), (FindingKind::OtherWorkspace, 1, Some("o1")));
    assert!(!f.message.contains("acme"), "the other workspace is not named: {}", f.message);
    let o1 = o.get(&admin(), "order:o1").unwrap();
    assert_eq!((o1.workspace.as_str(), &o1.props["total"]), ("acme", &json!(10)));
    assert_eq!(o.get(&admin(), "order:o9").unwrap().workspace, "globex");
}

#[test]
fn without_a_workspace_targets_are_counted_per_workspace() {
    let (o, ctx) = world();
    // g1 is a globex customer: the link is induced for globex only, and the
    // shape does not require it, since the ingest workspace is unknown.
    let csv = "order_id,customer_id\no1,g1\no2,g1\n";
    let file = InduceOptions { file: Some("orders.csv".into()), ..Default::default() };
    let ind = o.induce_sheet(&admin(), csv, &file).unwrap();
    assert_eq!(ind.links.len(), 1, "{:?}", ind.links);
    assert_eq!(ind.links[0].workspace.as_deref(), Some("globex"));
    assert_eq!((ind.links[0].matched, ind.links[0].filled), (2, 2));
    let link_shape = ind.shape.properties.iter().find(|p| p.path == "customer").unwrap();
    assert_eq!((link_shape.min_count, link_shape.max_count), (None, Some(1)));
    assert!(
        ind.warnings.iter().any(|w| w.contains("no workspace was given") && w.contains("globex")),
        "{:?}",
        ind.warnings
    );
    apply(&o, &ctx, &ind);
    // Ingested into acme, no link is written, and the shape still holds.
    let out = o.ingest_sheet(&ctx, csv, &ind.mapping, false).unwrap();
    assert_eq!((out.upserts, out.links), (2, 0));
    assert!(out.findings.iter().all(|f| f.issue == FindingKind::MissingTarget), "{:?}", out.findings);
    assert_eq!(conforms(&o, &ind), Vec::<String>::new());
    // Ingested into globex, every row is linked.
    let globex = ActionContext::new(admin(), "globex");
    let out = o.ingest_sheet(&globex, "order_id,customer_id\no3,g1\no4,g1\n", &ind.mapping, false).unwrap();
    assert_eq!((out.upserts, out.links), (2, 2), "{:?}", out.findings);
    assert_eq!(conforms(&o, &ind), Vec::<String>::new());

    // Keys spread over workspaces: no ingest into one workspace links them all.
    let spread = "sale_id,customer_id\ns1,c1\ns2,g1\n";
    let ind =
        o.induce_sheet(&admin(), spread, &InduceOptions { kind: Some("sale".into()), ..Default::default() }).unwrap();
    assert!(ind.links.is_empty(), "{:?}", ind.links);
    assert!(ind.warnings.iter().any(|w| w.contains("several workspaces")), "{:?}", ind.warnings);
    // With the workspace given, the shape requires a link every row has.
    let ind = o.induce_sheet(&admin(), "sale_id,customer_id\ns1,c1\ns2,c2\n", &opts("sales.csv")).unwrap();
    assert_eq!(ind.links.len(), 1, "{:?}", ind.links);
    let link_shape = ind.shape.properties.iter().find(|p| p.path == ind.links[0].rel).unwrap();
    assert_eq!(link_shape.min_count, Some(1));
    assert_eq!(ind.links[0].workspace.as_deref(), Some("acme"));
}

#[test]
fn numbers_are_read_exactly_or_not_at_all() {
    let (o, ctx) = world();
    // Negative fractions are floats.
    let ind = o.induce_sheet(&admin(), "id,delta\na,-0.5\nb,1.5\nc,-2.25\n", &InduceOptions::default()).unwrap();
    assert_eq!(column(&ind, "delta").ty, PropType::Float, "{}", column(&ind, "delta").evidence);
    // Integers beyond 64 bits are identifiers, kept as written.
    let csv = "id,iccid\na,89440012345678901234\nb,89440012345678901299\n";
    let ind = o.induce_sheet(&admin(), csv, &opts("sims.csv")).unwrap();
    let c = column(&ind, "iccid");
    assert_eq!(c.ty, PropType::String, "{}", c.evidence);
    assert!(c.evidence.contains("beyond 64 bits"), "{}", c.evidence);
    apply(&o, &ctx, &ind);
    let out = o.ingest_sheet(&ctx, csv, &ind.mapping, false).unwrap();
    assert!(out.findings.is_empty(), "{:?}", out.findings);
    let a = o.get(&admin(), "sim:a").unwrap();
    let b = o.get(&admin(), "sim:b").unwrap();
    assert_eq!(a.title, "89440012345678901234");
    assert_ne!(a.title, b.title);
    // Into a float property, a number that would be rounded is refused.
    let lt = "[object_types.reading]\nlabel = \"Reading\"\n\n[object_types.reading.properties.v]\ntype = \"float\"\n";
    let mut s = schema();
    s.extend_toml(lt).unwrap();
    let o2 = Ontology::in_memory(s);
    let m = SheetMapping {
        kind: "reading".into(),
        key: Some("id".into()),
        properties: BTreeMap::from([("v".to_string(), "v".to_string())]),
        ..Default::default()
    };
    let out = o2.ingest_sheet(&ctx, "id,v\nr1,-0.5\nr2,89440012345678901234\n", &m, false).unwrap();
    assert_eq!(out.upserts, 2);
    assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
    assert_eq!((out.findings[0].issue, out.findings[0].row), (FindingKind::BadValue, 2));
    assert_eq!(o2.get(&admin(), "reading:r1").unwrap().props["v"], json!(-0.5));
    assert!(!o2.get(&admin(), "reading:r2").unwrap().props.contains_key("v"));
}

#[test]
fn kinds_in_use_count_only_where_the_principal_reads() {
    let (o, _) = world();
    o.apply(
        &ActionContext::new(admin(), "globex"),
        vec![Mutation::Upsert(EntityInput::new("secretproject", "x", "Secret"))],
    )
    .unwrap();
    let named = InduceOptions { kind: Some("secretproject".into()), ..Default::default() };
    let viewer = o.induce_sheet(&Principal::viewer("vic", "acme"), "a\n1\n", &named).unwrap();
    assert_eq!(viewer.kind, "secretproject", "{:?}", viewer.warnings);
    assert!(viewer.warnings.iter().all(|w| !w.contains("exists already")), "{:?}", viewer.warnings);
    // Someone who reads globex sees the kind in use.
    assert_eq!(o.induce_sheet(&admin(), "a\n1\n", &named).unwrap().kind, "secretproject_2");
}

#[test]
fn a_sheet_maps_onto_the_existing_type_it_names() {
    let (o, ctx) = world();
    let ind = o.induce_sheet(&admin(), &orders(), &opts("orders.csv")).unwrap();
    apply(&o, &ctx, &ind);
    // Induced again, the sheet describes a free kind; its base is order.
    let again = o.induce_sheet(&admin(), &orders(), &opts("orders.csv")).unwrap();
    assert_eq!((again.kind.as_str(), again.base_kind.as_str()), ("order_2", "order"));
    let m = induce::mapping_onto(&o.schema(), &again, "order").unwrap();
    assert_eq!(m.kind, "order");
    assert_eq!(m.key.as_deref(), Some("order_id"));
    assert_eq!(m.links["customer_id"], LinkMapping { rel: "customer".into(), target_kind: "customer".into() });
    assert_eq!(m.properties.get("amount").map(String::as_str), Some("amount"));
    assert_eq!(m, ind.mapping, "the same mapping as the induction that made the type");
    let out = o.ingest_sheet(&ctx, &orders(), &m, true).unwrap();
    assert_eq!((out.upserts, out.links), (20, 19));

    // A column the type does not declare is named, never dropped.
    let extra = "order_id,amount,weight\no1,1.5,3\no2,2.5,4\n";
    let ind3 = o.induce_sheet(&admin(), extra, &opts("orders.csv")).unwrap();
    let e = induce::mapping_onto(&o.schema(), &ind3, "order").unwrap_err();
    assert!(e.to_string().contains("weight"), "{e}");
    assert!(induce::mapping_onto(&o.schema(), &ind3, "nope").is_err());
}
