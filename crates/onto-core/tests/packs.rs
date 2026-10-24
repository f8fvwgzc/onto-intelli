//! Industry packs and the marketplace: every registered pack compiles on
//! enterprise, its sample loads and makes every supplied rule fire with
//! exactly one deliberate shape violation, names never collide, and the
//! pack can be listed, shown, planned, installed (apply, propose, sample)
//! and reverted.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use onto_core::graph::Graph;
use onto_core::live_schema::Migration;
use onto_core::packs::{self, InstallMode, PackInstallOptions};
use onto_core::reason::{self, ReasonOptions};
use onto_core::schema::{PACKAGES, PropType};
use onto_core::shape_model::ShapePath;
use onto_core::store::MemoryStore;
use onto_core::{
    ActionContext, Decision, EntityInput, Error, EventFilter, Governance, Ontology, OntologyOptions, Principal, Role,
    Schema,
};

fn admin() -> Principal {
    Principal::admin("ada")
}

fn ids() -> Vec<&'static str> {
    packs::REGISTRY.iter().map(|(id, _, _)| *id).collect()
}

/// The pack's own layer, parsed as plain TOML.
fn layer(id: &str) -> toml::Table {
    toml::from_str(packs::find(id).unwrap().toml).unwrap()
}

fn table_len(t: &toml::Table, key: &str) -> usize {
    t.get(key).and_then(|v| v.as_table()).map_or(0, |t| t.len())
}

fn names_of(t: &toml::Table, key: &str) -> Vec<String> {
    t.get(key).and_then(|v| v.as_table()).map(|t| t.keys().cloned().collect()).unwrap_or_default()
}

/// enterprise ⊕ pack, as configuration would load it.
fn with_pack(id: &str) -> Schema {
    let mut s = Schema::enterprise();
    s.extend_source(&format!("builtin:{id}"), |p: &str| std::path::PathBuf::from(p)).unwrap();
    s
}

/// An in-memory ontology with enterprise ⊕ pack and the pack's sample loaded.
fn sampled(id: &str) -> Ontology {
    let o = Ontology::in_memory(with_pack(id));
    let ctx = ActionContext::new(admin(), "acme");
    o.apply(&ctx, packs::sample_mutations(packs::sample(id).unwrap())).unwrap_or_else(|e| panic!("{id} sample: {e}"));
    o
}

fn options(mode: InstallMode, sample: bool) -> PackInstallOptions {
    PackInstallOptions { mode, sample, message: None }
}

// ── registry and pack contents ───────────────────────────────────────────

#[test]
fn registry_matches_builtin_packages_and_metadata_is_honest() {
    assert!(ids().len() >= 4, "{:?}", ids());
    for (id, toml, sample) in packs::REGISTRY {
        let pkg = PACKAGES.iter().find(|(n, _)| *n == *id).unwrap_or_else(|| panic!("{id} is not in PACKAGES"));
        assert_eq!(pkg.1, *toml, "{id}: PACKAGES and the registry carry the same text");
        assert_eq!(onto_core::schema::builtin_package(id).unwrap(), *toml);
        let meta = packs::parse_meta(toml).unwrap();
        assert_eq!(meta.id, *id);
        assert_eq!(meta.version, "1.0.0", "{id}");
        assert_eq!(meta.requires, ["enterprise"], "{id}");
        assert!(!meta.title.is_empty() && !meta.industry.is_empty() && !meta.description.is_empty(), "{id}");
        assert!(!meta.standards.is_empty(), "{id} names the standards it borrows from");
        assert!(meta.description.contains("claims no compliance"), "{id} says it borrows vocabulary only");
        assert!(!meta.sample_note.is_empty(), "{id} documents its deliberate shape violation");
        let raw: toml::Table = toml::from_str(toml).unwrap();
        assert!(!raw.contains_key("name") && !raw.contains_key("version"), "{id} must not rename the base schema");
        assert!(!raw.contains_key("analysis"), "{id} must not replace the base analysis");
        assert!(!sample.is_empty());
    }
    let err = packs::find("spaceships").unwrap_err();
    assert!(matches!(err, Error::NotFound(_)) && err.to_string().contains("available: manufacturing"), "{err}");
}

#[test]
fn every_pack_compiles_on_enterprise_and_never_redefines_it() {
    let base = Schema::enterprise();
    for id in ids() {
        let s = with_pack(id);
        assert_eq!((s.name.as_str(), s.version), (base.name.as_str(), base.version), "{id}");
        packs::standalone(id).unwrap();
        let raw = layer(id);
        for section in ["object_types", "link_types", "interfaces", "metrics", "action_types", "shapes"] {
            for name in names_of(&raw, section) {
                let redefined = match section {
                    "object_types" => base.object_types.contains_key(&name),
                    "link_types" => base.link_types.contains_key(&name),
                    "interfaces" => base.interfaces.contains_key(&name),
                    "metrics" => base.metrics.contains_key(&name),
                    "action_types" => base.action_types.contains_key(&name),
                    _ => base.shapes.contains_key(&name),
                };
                assert!(!redefined, "{id} redefines enterprise {section}.{name}");
            }
        }
        // Each enterprise definition stays exactly as it was.
        for (name, lt) in &base.link_types {
            assert_eq!(json!(s.link_types[name]), json!(lt), "{id} changed enterprise link type {name}");
        }
        for (name, ty) in &base.object_types {
            assert_eq!(json!(s.object_types[name]), json!(ty), "{id} changed enterprise object type {name}");
        }
    }
}

#[test]
fn every_pack_has_the_promised_content() {
    for id in ids() {
        let s = with_pack(id);
        let raw = layer(id);
        let n = |k: &str| table_len(&raw, k);
        let rules = raw.get("rules").and_then(|v| v.as_array()).map_or(0, |a| a.len());
        assert!((8..=14).contains(&n("object_types")), "{id}: {} object types", n("object_types"));
        assert!((10..=20).contains(&n("link_types")), "{id}: {} link types", n("link_types"));
        assert!((3..=6).contains(&rules), "{id}: {rules} rules");
        assert!((3..=6).contains(&n("shapes")), "{id}: {} shapes", n("shapes"));
        assert!((2..=4).contains(&n("metrics")), "{id}: {} metrics", n("metrics"));
        assert!((1..=3).contains(&n("action_types")), "{id}: {} action types", n("action_types"));
        assert!(n("interfaces") >= 1, "{id} declares an interface");
        let glossary = raw.get("glossary").and_then(|g| g.as_table()).expect("a glossary");
        assert!(table_len(glossary, "kinds") >= 8, "{id}: glossary synonyms");

        for name in names_of(&raw, "object_types") {
            let ty = &s.object_types[&name];
            assert!(!ty.label.is_empty() && !ty.description.is_empty() && !ty.icon.is_empty(), "{id}.{name}");
            assert!(
                ty.color.len() == 7
                    && ty.color.starts_with('#')
                    && ty.color[1..].chars().all(|c| c.is_ascii_hexdigit()),
                "{id}.{name} color {}",
                ty.color
            );
            for st in ty.terminal_states.iter().chain(&ty.alert_states) {
                assert!(ty.states.contains(st), "{id}.{name}: {st} is not one of its states");
            }
            for (prop, def) in &ty.properties {
                if def.ty == PropType::Enum {
                    assert!(!def.values.is_empty(), "{id}.{name}.{prop} enum has values");
                }
            }
        }
        let mut transitive = 0;
        for name in names_of(&raw, "link_types") {
            let lt = &s.link_types[&name];
            assert!(lt.inverse.is_some(), "{id}: link type {name} has an inverse");
            assert!(!lt.from.is_empty() && !lt.to.is_empty(), "{id}: link type {name} is constrained");
            assert!(!lt.description.is_empty(), "{id}: link type {name} is described");
            assert!(raw["link_types"][&name].get("weight").is_some(), "{id}: link type {name} sets its weight");
            assert!(!(lt.transitive && lt.cardinality == onto_core::schema::Cardinality::One), "{id}: {name}");
            transitive += usize::from(lt.transitive);
        }
        assert!(transitive >= 1, "{id} has a transitive relation");
        let pack_rules: Vec<_> =
            s.rules.iter().filter(|r| !Schema::enterprise().rules.iter().any(|e| e.name == r.name)).collect();
        assert!(pack_rules.iter().any(|r| r.is_horn()) && pack_rules.iter().any(|r| !r.is_horn()), "{id}: both forms");
        assert!(pack_rules.iter().all(|r| !r.description.is_empty()), "{id}: rules are described");
        for name in names_of(&raw, "shapes") {
            assert!(!s.shapes[&name].description.is_empty(), "{id}: shape {name} is described");
        }
        for name in names_of(&raw, "action_types") {
            let a = &s.action_types[&name];
            assert!(!a.description.is_empty() && !a.effects.is_empty(), "{id}: action type {name}");
        }
    }
}

#[test]
fn packs_install_together_and_over_the_default_layers() {
    // In registry order and reversed, alone on enterprise and over the
    // default configuration layers (data-catalog, code).
    let mut orders = vec![ids()];
    orders.push(ids().into_iter().rev().collect());
    for order in orders {
        for defaults in [false, true] {
            let mut s = Schema::enterprise();
            if defaults {
                s.extend_source("builtin:data-catalog", |p: &str| std::path::PathBuf::from(p)).unwrap();
                s.extend_source("builtin:code", |p: &str| std::path::PathBuf::from(p)).unwrap();
            }
            let before = s.clone();
            for id in &order {
                s.extend_source(&format!("builtin:{id}"), |p: &str| std::path::PathBuf::from(p))
                    .unwrap_or_else(|e| panic!("{id} after {order:?}: {e}"));
            }
            for id in &order {
                for name in names_of(&layer(id), "object_types") {
                    assert!(s.object_types.contains_key(&name), "{id}.{name} survives the other packs");
                }
            }
            for (name, lt) in &before.link_types {
                assert_eq!(json!(s.link_types[name]), json!(lt), "packs leave {name} alone");
            }
            let mut labels: BTreeMap<&str, &str> = BTreeMap::new();
            for ty in s.object_types.values().filter(|t| !t.label.is_empty()) {
                if let Some(other) = labels.insert(ty.label.as_str(), ty.name.as_str()) {
                    panic!("{} and {other} share the label {}", ty.name, ty.label);
                }
            }
        }
    }
}

#[test]
fn pack_names_never_collide() {
    let collisions = packs::collisions();
    assert!(collisions.is_empty(), "{collisions:#?}");
    // The check sees every namespace: a would-be pack reusing an object type,
    // a derived class, an inverse and a glossary word of others is caught,
    // while the base packages restating `owns` among themselves are not.
    let rogue = r#"
        [pack]
        id = "rogue"
        title = "Rogue"
        version = "1.0.0"
        [object_types.supplier]
        [object_types.gadget]
        [link_types.gizmo_of]
        from = ["gadget"]
        to = ["gadget"]
        inverse = "carries"
        [[rules]]
        name = "flag"
        when = [["?x", "a", "gadget"]]
        then = ["?x", "a", "vip_shopper"]
        [glossary.kinds]
        vendor = "gadget"
    "#;
    let mut layers: Vec<(&str, &str)> = PACKAGES.to_vec();
    layers.push(("rogue", rogue));
    let mut packs_ids = ids();
    packs_ids.push("rogue");
    let found = packs::collisions_in(&layers, &packs_ids);
    let names: BTreeSet<(String, String)> = found.iter().map(|c| (c.namespace.clone(), c.name.clone())).collect();
    for (ns, name) in
        [("names", "supplier"), ("names", "carries"), ("names", "vip_shopper"), ("glossary.kinds", "vendor")]
    {
        assert!(names.contains(&(ns.to_string(), name.to_string())), "{ns} {name} not caught: {found:#?}");
    }
    assert!(found.iter().all(|c| c.packages.contains(&"rogue".to_string())), "{found:#?}");
    assert!(!names.contains(&("names".to_string(), "owns".to_string())), "base packages may restate relations");
    let broken = packs::collisions_in(&[("bad", "[object_types")], &["bad"]);
    assert_eq!(broken[0].namespace, "unparsable");
}

// ── samples ──────────────────────────────────────────────────────────────

#[test]
fn samples_load_and_every_supplied_rule_fires() {
    for id in ids() {
        let entities = packs::sample(id).unwrap();
        assert!((25..=60).contains(&entities.len()), "{id}: {} sample lines", entities.len());
        let pack_kinds: BTreeSet<String> = names_of(&layer(id), "object_types").into_iter().collect();
        let used: BTreeSet<&str> = entities.iter().map(|e| e.kind.as_str()).collect();
        for k in &pack_kinds {
            assert!(used.contains(k.as_str()), "{id}: the sample has a {k}");
        }
        assert!(entities.iter().all(|e| e.workspace.is_none()), "{id}: workspace-agnostic");

        let o = sampled(id);
        let stats = o.read(|g, schema| reason::closure(schema, g, &ReasonOptions::default(), &|_| true).stats());
        assert!(stats.fixpoint_reached, "{id}: {stats:?}");
        assert_eq!(stats.refused, 0, "{id}: {:?}", stats.refusals);
        let rules = layer(id)["rules"].as_array().unwrap().clone();
        for r in rules {
            let name = r["name"].as_str().unwrap();
            let n = stats.by_rule.get(&format!("schema/{name}")).copied().unwrap_or(0);
            assert!(n > 0, "{id}: rule {name} derives nothing from the sample ({:?})", stats.by_rule);
        }
    }
}

#[test]
fn pack_rules_are_supplied_rules_in_the_certificate() {
    let o = sampled("supply-chain");
    let cert = o.reason_certificate(&admin(), &ReasonOptions::default()).unwrap();
    assert!(cert.rules_tsv.lines().any(|l| l.starts_with("schema/high_risk_supplier\t")), "{}", cert.rules_tsv);
    assert!(
        cert.derivations_tsv.lines().any(|l| l.starts_with("schema/high_risk_supplier\t<supplier:kestrel-alloys>"))
    );
    let q = o
        .reason_query(
            &admin(),
            &ReasonOptions::default(),
            &[vec![json!("?s"), json!("a"), json!("high_risk_supplier")]],
            10,
        )
        .unwrap();
    let found: BTreeSet<String> = q.bindings.iter().map(|b| b["s"].clone()).collect();
    assert_eq!(
        found,
        BTreeSet::from(["<supplier:borealis-chem>".to_string(), "<supplier:kestrel-alloys>".to_string()])
    );
}

/// A shape result: `shape/constraint focus: what`.
fn shape_results(g: &Graph, schema: &Schema) -> Vec<String> {
    let mut out = Vec::new();
    for (shape_name, sh) in &schema.shapes {
        let target = sh.target.as_deref().expect("pack shapes target a kind or an interface");
        assert!(
            sh.target_ids.is_empty() && sh.target_subjects_of.is_none() && sh.target_objects_of.is_none() && !sh.closed,
            "{shape_name}: this check evaluates kind targets only"
        );
        let mut focus: Vec<_> = g.entities().filter(|e| schema.kind_is(&e.kind, target)).collect();
        focus.sort_by(|a, b| a.id.cmp(&b.id));
        assert!(!focus.is_empty(), "{shape_name} targets something in the sample");
        for ps in &sh.properties {
            assert!(ps.unknown.is_empty() && ps.node.is_none() && ps.datatype.is_none() && ps.has_value.is_none());
            assert!(ps.min_length.is_none() && ps.max_length.is_none(), "{shape_name}: unsupported here");
            for e in &focus {
                let values: Vec<Value> = match ShapePath::parse(&ps.path) {
                    ShapePath::Title => vec![json!(e.title)],
                    ShapePath::State => e.state.iter().map(|s| json!(s)).collect(),
                    ShapePath::Prop(p) => match e.props.get(&p) {
                        None | Some(Value::Null) => Vec::new(),
                        Some(Value::Array(a)) => a.clone(),
                        Some(v) => vec![v.clone()],
                    },
                    ShapePath::Relations(steps) => {
                        let mut cur = vec![e.id.clone()];
                        for step in steps {
                            let (base, dir) = schema.resolve_rel(&step).expect("pack paths use declared relations");
                            let mut next: Vec<String> =
                                cur.iter().flat_map(|id| g.neighbors(id, &base, dir)).map(str::to_string).collect();
                            next.sort();
                            next.dedup();
                            cur = next;
                        }
                        cur.into_iter().map(Value::String).collect()
                    }
                };
                let mut fail = |what: String| out.push(format!("{shape_name}/{} {}: {what}", ps.id, e.id));
                if ps.min_count.is_some_and(|m| values.len() < m) {
                    fail(format!("min_count {} (has {})", ps.min_count.unwrap(), values.len()));
                }
                if ps.max_count.is_some_and(|m| values.len() > m) {
                    fail(format!("max_count {} (has {})", ps.max_count.unwrap(), values.len()));
                }
                for v in &values {
                    if let Some(c) = &ps.class {
                        let ok = v.as_str().and_then(|id| g.get(id)).is_some_and(|t| schema.kind_is(&t.kind, c));
                        if !ok {
                            fail(format!("class {c} ({v})"));
                        }
                    }
                    if !ps.values.is_empty() && !ps.values.contains(v) {
                        fail(format!("in ({v})"));
                    }
                    if let Some(p) = &ps.pattern {
                        let re = regex_lite::Regex::new(p).unwrap();
                        if !v.as_str().is_some_and(|s| re.is_match(s)) {
                            fail(format!("pattern {p} ({v})"));
                        }
                    }
                    if let Some(lo) = ps.min_value
                        && v.as_f64().is_none_or(|x| x < lo)
                    {
                        fail(format!("min_value {lo} ({v})"));
                    }
                    if let Some(hi) = ps.max_value
                        && v.as_f64().is_none_or(|x| x > hi)
                    {
                        fail(format!("max_value {hi} ({v})"));
                    }
                }
            }
        }
    }
    out
}

#[test]
fn each_sample_violates_exactly_one_shape_constraint() {
    let expected = BTreeMap::from([
        ("manufacturing", "production_order_shape/production_order_shape/1 production_order:po-1004: min_count 1"),
        ("supply-chain", "supplier_shape/supplier_shape/0 supplier:northwind-distribution: min_count 1"),
        ("retail", "stock_position_shape/stock_position_shape/2 stock_position:0217-cold-brew: min_value 0"),
        ("logistics", "handling_unit_shape/handling_unit_shape/0 handling_unit:pal-2: pattern"),
    ]);
    for id in ids() {
        let o = sampled(id);
        let results = o.read(shape_results);
        assert_eq!(results.len(), 1, "{id}: {results:#?}");
        if let Some(want) = expected.get(id) {
            assert!(results[0].starts_with(want), "{id}: {} does not start with {want}", results[0]);
        }
        let note = packs::find(id).unwrap().meta.sample_note;
        let focus = results[0].split_whitespace().nth(1).unwrap().trim_end_matches(':');
        let key = focus.split_once(':').unwrap().1;
        assert!(note.contains(key), "{id}: the sample note names {key}: {note}");
    }
}

#[test]
fn pack_action_types_plan_on_their_sample() {
    let cases: &[(&str, &str, Value)] = &[
        (
            "manufacturing",
            "release_production_order",
            json!({"order": "production_order:po-1004", "line": "production_line:us01-l1"}),
        ),
        ("manufacturing", "quarantine_lot", json!({"lot": "production_lot:l-2420", "reason": "suspect input lot"})),
        (
            "manufacturing",
            "raise_nonconformance",
            json!({"title": "Burr on housing", "against": "production_lot:l-2420", "severity": "minor"}),
        ),
        (
            "supply-chain",
            "suspend_supplier",
            json!({"supplier": "supplier:kestrel-alloys", "reason": "sanctioned region"}),
        ),
        ("supply-chain", "issue_purchase_order", json!({"po": "purchase_order:po-45003"})),
        (
            "supply-chain",
            "record_supply_disruption",
            json!({"title": "Strike", "site": "supplier_site:rhein-stuttgart", "cause": "labor", "impact": "medium"}),
        ),
        ("retail", "launch_promotion", json!({"promotion": "promotion:summer-drinks"})),
        (
            "retail",
            "pull_recalled_sku",
            json!({"sku": "sku:sparkling-water-12", "recall": "product_recall:rc-2025-03"}),
        ),
        (
            "logistics",
            "report_delivery_exception",
            json!({"shipment": "shipment:sh-88003", "title": "Customs hold", "exception_type": "customs_hold"}),
        ),
        ("logistics", "confirm_delivery", json!({"shipment": "shipment:sh-88002"})),
    ];
    let mut covered: BTreeSet<(String, String)> = BTreeSet::new();
    for (id, action, params) in cases {
        let o = sampled(id);
        let ctx = ActionContext::new(admin(), "acme");
        let params: Map<String, Value> = params.as_object().unwrap().clone();
        let plan = o.plan_action(&ctx, action, &params).unwrap();
        assert!(plan.valid, "{id}/{action}: {:?}", plan.checks);
        covered.insert((id.to_string(), action.to_string()));
    }
    // Every action type of every pack in this file is planned above.
    for id in ["manufacturing", "supply-chain", "retail", "logistics"] {
        for a in names_of(&layer(id), "action_types") {
            assert!(covered.contains(&(id.to_string(), a.clone())), "{id}/{a} has a plan case");
        }
    }
    // A precondition still refuses: a released order cannot be released again.
    let o = sampled("manufacturing");
    let ctx = ActionContext::new(admin(), "acme");
    let params = json!({"order": "production_order:po-1002", "line": "production_line:us01-l1"});
    let plan = o.plan_action(&ctx, "release_production_order", params.as_object().unwrap()).unwrap();
    assert!(!plan.valid);
}

// ── the marketplace ──────────────────────────────────────────────────────

#[test]
fn list_show_plan_install_and_revert_every_pack() {
    for id in ids() {
        let o = Ontology::in_memory(Schema::enterprise());
        let ctx = ActionContext::new(admin(), "acme");
        let info = |o: &Ontology| o.pack_list(&admin()).unwrap().into_iter().find(|p| p.meta.id == id).unwrap();
        let before = info(&o);
        assert!(!before.installed && before.pending_changes > 0 && before.blocked.is_none(), "{id}: {before:?}");
        assert!(before.missing_requires.is_empty());

        let shown = o.pack_show(&admin(), id).unwrap();
        assert_eq!(shown.counts, before.counts);
        assert_eq!(shown.object_types.len(), shown.counts.object_types);
        assert!(shown.rules.iter().any(|r| r.form == "horn") && shown.rules.iter().any(|r| r.form == "chain"));
        assert!(!shown.derived_classes.is_empty() || !shown.derived_relations.is_empty());
        assert!(shown.means.contains("not compliance"));
        let doc = serde_json::to_value(&shown).unwrap();
        assert_eq!(doc["id"], id, "metadata is flattened into the detail");

        let plan = o.pack_plan(&admin(), id).unwrap();
        assert!(plan.applicable(), "{id}: {:?} {:?}", plan.errors, plan.needs_migration);
        assert!(!plan.breaking, "{id}: installing on an empty graph breaks nothing");
        assert_eq!(plan.changes.len(), before.pending_changes);
        assert!(plan.diff.entries.iter().all(|e| e.change == onto_core::live_schema::DiffKind::Added), "{id}");

        let v0 = o.schema_version();
        let done = o.pack_install(&ctx, id, options(InstallMode::Apply, false)).unwrap();
        assert!(!done.already_installed && done.proposal.is_none() && done.sample_entities == 0);
        let version = done.version.expect("a new schema version");
        assert_eq!(version.version, v0 + 1);
        assert_eq!(o.schema_version(), v0 + 1);
        for name in names_of(&layer(id), "object_types") {
            assert!(o.schema().object_types.contains_key(&name), "{id}.{name} is live");
        }
        assert!(info(&o).installed, "{id}");
        let ev =
            o.events(&admin(), &EventFilter { action: Some("schema.change".into()), ..Default::default() }).unwrap();
        let last = ev.iter().find(|e| e.payload["version"] == json!(v0 + 1)).expect("an audited schema.change");
        assert_eq!(last.intent.as_deref(), Some("pack.install"));
        assert!(last.payload["message"].as_str().unwrap().starts_with(&format!("install pack {id} 1.0.0")));

        // Re-installing is a no-op.
        let again = o.pack_plan(&admin(), id).unwrap();
        assert!(again.valid && again.changes.is_empty() && again.diff.is_empty(), "{id}: {again:?}");
        assert!(again.warnings[0].contains("already installed"));
        let twice = o.pack_install(&ctx, id, options(InstallMode::Apply, false)).unwrap();
        assert!(twice.already_installed && twice.version.is_none() && twice.changes == 0);
        assert_eq!(o.schema_version(), v0 + 1);

        // Revert to the version before the install.
        o.schema_revert(&ctx, v0, Vec::new(), false).unwrap();
        for name in names_of(&layer(id), "object_types") {
            assert!(!o.schema().object_types.contains_key(&name), "{id}.{name} is gone after the revert");
        }
        assert!(!info(&o).installed);
    }
}

#[test]
fn install_with_sample_loads_it_and_reverts_with_migrations() {
    for id in ids() {
        let o = Ontology::in_memory(Schema::enterprise());
        let ctx = ActionContext::new(admin(), "acme");
        let v0 = o.schema_version();
        let done = o.pack_install(&ctx, id, options(InstallMode::Apply, true)).unwrap();
        let lines = packs::sample(id).unwrap();
        assert_eq!(done.sample_entities, lines.len(), "{id}");
        assert_eq!(done.sample_links, lines.iter().map(|e| e.links.len()).sum::<usize>(), "{id}");
        for e in &lines {
            let got = o.get(&admin(), &format!("{}:{}", e.kind, e.key.as_deref().unwrap())).unwrap();
            assert_eq!(got.workspace, "acme");
        }
        // Loading the sample again changes nothing.
        let rev = o.revision();
        let again = o.pack_install(&ctx, id, options(InstallMode::Apply, true)).unwrap();
        assert!(again.already_installed && again.sample_entities == lines.len());
        assert_eq!(o.revision(), rev, "{id}: a reloaded sample writes nothing");

        // Reverting a schema whose types hold data needs migrations.
        let refused = o.schema_revert(&ctx, v0, Vec::new(), false).unwrap_err();
        assert!(matches!(refused, Error::Conflict(_)), "{id}: {refused}");
        let mut migrations: Vec<Migration> = names_of(&layer(id), "object_types")
            .into_iter()
            .map(|k| Migration::DeleteEntities { kind: Some(k) })
            .collect();
        migrations
            .extend(names_of(&layer(id), "link_types").into_iter().map(|r| Migration::DeleteLinks { rel: Some(r) }));
        o.schema_revert(&ctx, v0, migrations, false).unwrap_or_else(|e| panic!("{id}: {e}"));
        let pack_kinds = names_of(&layer(id), "object_types");
        o.read(|g, _| assert!(g.entities().all(|e| !pack_kinds.contains(&e.kind)), "{id}: data migrated away"));
    }
}

#[test]
fn sample_respects_curated_data_and_is_checked_before_the_schema_changes() {
    // An existing entity keeps its curated title: samples only fill gaps.
    let o = Ontology::in_memory(Schema::enterprise());
    let ctx = ActionContext::new(admin(), "acme");
    o.upsert(&ctx, EntityInput::new("person", "maria-lopez", "Maria L. (procurement lead)")).unwrap();
    o.pack_install(&ctx, "supply-chain", options(InstallMode::Apply, true)).unwrap();
    assert_eq!(o.get(&admin(), "person:maria-lopez").unwrap().title, "Maria L. (procurement lead)");
    assert_eq!(o.get(&admin(), "person:maria-lopez").unwrap().prop_str("role"), Some("Category buyer"));

    // A sample that cannot load refuses the whole install: no schema version.
    let o = Ontology::in_memory(Schema::enterprise());
    o.upsert(&ActionContext::new(admin(), "other"), EntityInput::new("team", "procurement", "Elsewhere")).unwrap();
    let v0 = o.schema_version();
    let err = o.pack_install(&ctx, "supply-chain", options(InstallMode::Apply, true)).unwrap_err();
    assert!(err.to_string().contains("sample of pack supply-chain does not load"), "{err}");
    assert_eq!(o.schema_version(), v0, "nothing was applied");
    assert!(!o.schema().object_types.contains_key("supplier"));
}

fn governed(gov: Governance) -> Ontology {
    Ontology::open(
        Schema::enterprise(),
        Arc::new(MemoryStore::default()),
        OntologyOptions { governance: gov, ..Default::default() },
    )
    .unwrap()
}

#[test]
fn apply_needs_governance_and_propose_files_a_proposal() {
    let o = governed(Governance::default());
    let member = Principal::member("mo", "acme");
    let agent = Principal::new("assistant", vec![Role::Agent], vec!["*".into()]);
    for p in [&member, &agent] {
        let err = o.pack_install(&ActionContext::new(p.clone(), "acme"), "retail", options(InstallMode::Apply, false));
        let err = err.unwrap_err();
        assert!(matches!(err, Error::Denied(_)) && err.to_string().contains("propose instead"), "{err}");
    }
    let denied =
        o.events(&admin(), &EventFilter { action: Some("authorization.denied".into()), ..Default::default() }).unwrap();
    assert!(denied.iter().any(|e| e.actor == "mo" && e.intent.as_deref() == Some("pack.install")), "{denied:?}");
    assert!(!o.schema().object_types.contains_key("sku"));

    // A sample cannot ride on a proposal that is not applied yet.
    let actx = ActionContext::new(agent.clone(), "acme");
    let err = o.pack_install(&actx, "retail", options(InstallMode::Propose, true)).unwrap_err();
    assert!(matches!(err, Error::Invalid(_)) && err.to_string().contains("needs its schema in force"), "{err}");

    let filed = o.pack_install(&actx, "retail", options(InstallMode::Propose, false)).unwrap();
    let view = filed.proposal.expect("a proposal");
    assert!(filed.version.is_none() && !filed.already_installed);
    assert!(view.proposal.title.contains("Retail and omnichannel commerce 1.0.0"), "{}", view.proposal.title);
    assert_eq!(view.proposal.change_set.changes.len(), filed.changes);
    assert!(view.proposal.change_set.expected_version.is_none(), "proposals re-plan on approval");
    assert!(!o.schema().object_types.contains_key("sku"), "proposing changes nothing");

    let out = o.review_proposal(&ActionContext::new(admin(), "acme"), &view.proposal.id, Decision::Approve, "ok", None);
    assert!(out.unwrap().applied.is_some());
    assert!(o.schema().object_types.contains_key("sku"));
    let listed = o.pack_list(&admin()).unwrap();
    assert!(listed.iter().find(|p| p.meta.id == "retail").unwrap().installed);

    // Once applied, a member loads the sample: the schema part is a no-op.
    let loaded =
        o.pack_install(&ActionContext::new(member, "acme"), "retail", options(InstallMode::Propose, true)).unwrap();
    assert!(loaded.already_installed && loaded.proposal.is_none() && loaded.sample_entities > 25);
    // A viewer may look but not load.
    let viewer = Principal::viewer("vi", "acme");
    assert!(o.pack_plan(&viewer, "retail").unwrap().valid);
    let err = o.pack_install(&ActionContext::new(viewer, "acme"), "retail", options(InstallMode::Apply, true));
    assert!(matches!(err.unwrap_err(), Error::Denied(_)));
}

#[test]
fn requires_are_checked_against_the_current_schema() {
    let empty = Schema::empty();
    let err = packs::change_set(&empty, "logistics").unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err}");
    let text = err.to_string();
    assert!(text.contains("requires enterprise") && text.contains("person") && text.contains("team"), "{text}");
    let listed = packs::list(&empty);
    assert!(listed.iter().all(|p| !p.installed && !p.missing_requires.is_empty() && p.blocked.is_some()));

    let o = Ontology::in_memory(Schema::empty());
    let err = o.pack_plan(&admin(), "manufacturing").unwrap_err();
    assert!(err.to_string().contains("enterprise: "), "{err}");

    // With enterprise in force the change set only adds.
    let set = packs::change_set(&Schema::enterprise(), "manufacturing").unwrap();
    assert!(set.changes.iter().all(|c| c.op().starts_with("put_")), "{:?}", set.changes);
    assert!(set.message.starts_with("install pack manufacturing 1.0.0"));
    let mut meta = packs::find("retail").unwrap().meta;
    meta.requires.push("no-such-package".into());
    assert!(packs::missing_requires(&Schema::enterprise(), &meta).contains(&"no-such-package: unknown package".into()));
}

#[test]
fn installed_schema_round_trips_and_extends_from_configuration() {
    // A live install equals the configuration layer, so reopening a store whose
    // configuration now extends the pack imports nothing new.
    let o = Ontology::in_memory(Schema::enterprise());
    let ctx = ActionContext::new(admin(), "acme");
    for id in ids() {
        o.pack_install(&ctx, id, options(InstallMode::Apply, false)).unwrap();
    }
    let mut configured = Schema::enterprise();
    for id in ids() {
        configured.extend_source(&format!("builtin:{id}"), |p: &str| std::path::PathBuf::from(p)).unwrap();
    }
    assert!(onto_core::live_schema::diff(&o.schema(), &configured).is_empty());
    let doc = serde_json::to_value(&*o.schema()).unwrap();
    Schema::from_json(doc).unwrap();
}

// ── PostgreSQL ───────────────────────────────────────────────────────────

#[test]
fn pg_install_with_sample_survives_a_reopen() {
    let Some(pg) = common::TestTenant::new("packs_install") else { return };
    let ctx = ActionContext::new(admin(), "acme");
    {
        let o = pg.open(Schema::enterprise(), OntologyOptions::default());
        let done = o.pack_install(&ctx, "logistics", options(InstallMode::Apply, true)).unwrap();
        assert!(done.version.is_some() && done.sample_entities > 0);
    }
    let o = pg.open(Schema::enterprise(), OntologyOptions::default());
    assert!(o.schema().object_types.contains_key("shipment"), "the live schema is stored");
    assert_eq!(o.get(&admin(), "shipment:sh-88001").unwrap().state.as_deref(), Some("in_transit"));
    let listed = o.pack_list(&admin()).unwrap();
    assert!(listed.iter().find(|p| p.meta.id == "logistics").unwrap().installed);
    let again = o.pack_install(&ctx, "logistics", options(InstallMode::Apply, false)).unwrap();
    assert!(again.already_installed);
    let stats = o.reason_stats(&admin(), &ReasonOptions::default()).unwrap();
    assert!(stats.by_rule["schema/hazmat_vehicle"] > 0, "{stats:?}");
}
