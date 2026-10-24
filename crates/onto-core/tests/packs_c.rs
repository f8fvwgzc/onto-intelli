//! Industry packs `energy`, `telecom` and `saas` (layered on enterprise).
//!
//! Each pack must compile on top of `builtin:enterprise` without redefining
//! it, its sample must apply cleanly under a strict schema, every rule the
//! pack supplies must derive at least one fact from the sample, the sample
//! must hold exactly the one shape violation its `[pack].sample_note`
//! documents, its action types and metrics must run on the sample, and none
//! of its names may collide with enterprise or with any other pack (packs
//! are installable side by side). The registry (`packs.rs`) is merged later,
//! so this file reads the pack files directly.
//!
//! The shape check here is a small evaluator for the components these packs
//! use (counts, class, datatype, in, pattern, lengths, value bounds over
//! property, state, title and relation paths). It refuses any component it
//! does not implement, so a pack cannot pass by using one it skips.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use onto_core::action_types::ApplyOptions;
use onto_core::graph::Graph;
use onto_core::live_schema::changes_between;
use onto_core::metrics::MetricQuery;
use onto_core::reason::{self, Attested, Fact, ReasonOptions};
use onto_core::schema::ENTERPRISE_SCHEMA;
use onto_core::schema::{Dir, PropType, PropertyDef};
use onto_core::shape_model::{Shape, ShapePath};
use onto_core::store::MemoryStore;
use onto_core::{ActionContext, EntityInput, Mutation, Ontology, OntologyOptions, Principal, Schema, SchemaChangeSet};

/// One pack's files.
struct PackFiles {
    id: &'static str,
    toml: &'static str,
    sample: &'static str,
}

const PACKS: &[PackFiles] = &[
    PackFiles {
        id: "energy",
        toml: include_str!("../schemas/packs/energy.toml"),
        sample: include_str!("../schemas/packs/energy.sample.jsonl"),
    },
    PackFiles {
        id: "telecom",
        toml: include_str!("../schemas/packs/telecom.toml"),
        sample: include_str!("../schemas/packs/telecom.sample.jsonl"),
    },
    PackFiles {
        id: "saas",
        toml: include_str!("../schemas/packs/saas.toml"),
        sample: include_str!("../schemas/packs/saas.sample.jsonl"),
    },
];

/// Schema sections a pack may declare. `name`, `version` and `analysis`
/// would overwrite the base schema's, so they are not among them.
const PACK_SECTIONS: &[&str] =
    &["pack", "object_types", "link_types", "interfaces", "rules", "glossary", "shapes", "metrics", "action_types"];

fn admin() -> Principal {
    Principal::admin("ada")
}

fn ctx() -> ActionContext {
    ActionContext::new(admin(), "acme")
}

fn toml_of(text: &str, what: &str) -> toml::Table {
    toml::from_str(text).unwrap_or_else(|e| panic!("{what}: not TOML: {e}"))
}

fn meta(p: &PackFiles) -> toml::Table {
    match toml_of(p.toml, p.id).remove("pack") {
        Some(toml::Value::Table(t)) => t,
        other => panic!("{}: [pack] table missing or not a table: {other:?}", p.id),
    }
}

fn meta_str(p: &PackFiles, key: &str) -> String {
    meta(p).get(key).and_then(|v| v.as_str()).unwrap_or_else(|| panic!("{}: [pack].{key} missing", p.id)).to_string()
}

/// Enterprise with the pack layered on top.
fn pack_schema(p: &PackFiles) -> Schema {
    let mut s = Schema::enterprise();
    s.extend_toml(p.toml).unwrap_or_else(|e| panic!("{} does not compile on enterprise: {e}", p.id));
    s
}

/// The pack alone, as written (its own declarations only).
fn declared(p: &PackFiles) -> toml::Table {
    toml_of(p.toml, p.id)
}

fn section_keys(t: &toml::Table, section: &str) -> BTreeSet<String> {
    t.get(section).and_then(|v| v.as_table()).map(|m| m.keys().cloned().collect()).unwrap_or_default()
}

/// The rule-table names (`schema/<name>`) of the rules the pack declares.
fn pack_rules(p: &PackFiles) -> Vec<String> {
    let rules = declared(p).remove("rules").unwrap_or_else(|| panic!("{}: no rules", p.id));
    rules
        .as_array()
        .expect("[[rules]]")
        .iter()
        .map(|r| format!("schema/{}", r["name"].as_str().expect("rule name")))
        .collect()
}

fn sample(p: &PackFiles) -> Vec<EntityInput> {
    p.sample
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).unwrap_or_else(|e| panic!("{}.sample.jsonl line {}: {e}", p.id, i + 1)))
        .collect()
}

fn mutations(p: &PackFiles) -> Vec<Mutation> {
    sample(p).into_iter().map(Mutation::Upsert).collect()
}

fn strict(schema: Schema) -> Ontology {
    let opts = OntologyOptions { strict: true, ..Default::default() };
    Ontology::open(schema, Arc::new(MemoryStore::default()), opts).expect("open")
}

/// A strict in-memory ontology of enterprise + the pack, with the sample applied.
fn world(p: &PackFiles) -> Ontology {
    let o = strict(pack_schema(p));
    o.apply(&ctx(), mutations(p)).unwrap_or_else(|e| panic!("{} sample does not apply: {e}", p.id));
    o
}

fn closure_of(o: &Ontology) -> reason::Closure {
    o.read(|g, schema| reason::closure(schema, g, &ReasonOptions::default(), &|_| true))
}

// ── [pack] metadata ──────────────────────────────────────────────────────────

#[test]
fn pack_tables_describe_each_pack_honestly() {
    for p in PACKS {
        let m = meta(p);
        for key in ["id", "title", "industry", "version", "description", "sample_note"] {
            let v = m.get(key).and_then(|v| v.as_str()).unwrap_or_default();
            assert!(!v.trim().is_empty(), "{}: [pack].{key} must be a non-empty string", p.id);
        }
        assert_eq!(meta_str(p, "id"), p.id);
        assert_eq!(meta_str(p, "version"), "1.0.0", "{}", p.id);
        let requires: Vec<&str> = m["requires"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(requires, ["enterprise"], "{}", p.id);
        let standards: Vec<&str> = m["standards"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert!(!standards.is_empty(), "{}: name the standards whose vocabulary it borrows", p.id);
        for s in &standards {
            let lower = s.to_lowercase();
            assert!(
                lower.contains("inspired") || lower.contains("borrowed"),
                "{}: say how a standard is used: {s}",
                p.id
            );
            for claim in ["compliant", "compliance", "certified", "conforms", "conformant"] {
                assert!(!lower.contains(claim), "{}: a pack never claims compliance: {s}", p.id);
            }
        }
        // Only schema sections a layer may carry: no name/version/analysis.
        for key in declared(p).keys() {
            assert!(PACK_SECTIONS.contains(&key.as_str()), "{}: top-level key {key} is not allowed in a pack", p.id);
        }
        // The base schema keeps its own name and version.
        let s = pack_schema(p);
        let base = Schema::enterprise();
        assert_eq!((s.name.as_str(), s.version), (base.name.as_str(), base.version), "{}", p.id);
    }
}

// ── content ──────────────────────────────────────────────────────────────────

#[test]
fn packs_compile_on_enterprise_without_redefining_it() {
    let base = Schema::enterprise();
    let ent = toml_of(ENTERPRISE_SCHEMA, "enterprise");
    for p in PACKS {
        let d = declared(p);
        for section in ["object_types", "link_types", "interfaces", "metrics", "action_types", "shapes"] {
            let redefined: Vec<String> =
                section_keys(&d, section).intersection(&section_keys(&ent, section)).cloned().collect();
            assert!(redefined.is_empty(), "{}: redefines enterprise {section}: {redefined:?}", p.id);
        }
        let s = pack_schema(p);
        // Enterprise definitions are untouched by the layer.
        for (name, lt) in &base.link_types {
            let after = serde_json::to_value(&s.link_types[name]).unwrap();
            assert_eq!(serde_json::to_value(lt).unwrap(), after, "{}: changes enterprise link type {name}", p.id);
        }
        for (name, ty) in &base.object_types {
            let after = serde_json::to_value(&s.object_types[name]).unwrap();
            assert_eq!(serde_json::to_value(ty).unwrap(), after, "{}: changes enterprise type {name}", p.id);
        }
        for (word, kind) in &base.glossary.kinds {
            assert_eq!(&s.glossary.kinds[word], kind, "{}: remaps the enterprise glossary word {word}", p.id);
        }
        for (word, rel) in &base.glossary.relations {
            assert_eq!(&s.glossary.relations[word], rel, "{}: remaps the enterprise glossary word {word}", p.id);
        }
        assert_eq!(s.rules.len(), base.rules.len() + d["rules"].as_array().unwrap().len(), "{}", p.id);
    }
}

#[test]
fn packs_have_the_content_the_guide_asks_for() {
    let base = Schema::enterprise();
    for p in PACKS {
        let s = pack_schema(p);
        let d = declared(p);
        let types = section_keys(&d, "object_types");
        let links = section_keys(&d, "link_types");
        let rules: Vec<_> = s.rules.iter().filter(|r| !base.rules.iter().any(|b| b.name == r.name)).collect();
        let count = |section: &str| section_keys(&d, section).len();
        assert!((8..=14).contains(&types.len()), "{}: {} object types", p.id, types.len());
        assert!((10..=20).contains(&links.len()), "{}: {} link types", p.id, links.len());
        assert!((3..=6).contains(&rules.len()), "{}: {} rules", p.id, rules.len());
        assert!((3..=6).contains(&count("shapes")), "{}: {} shapes", p.id, count("shapes"));
        assert!((2..=4).contains(&count("metrics")), "{}: {} metrics", p.id, count("metrics"));
        assert!((1..=3).contains(&count("action_types")), "{}: {} action types", p.id, count("action_types"));
        assert!(rules.iter().any(|r| !r.is_horn()), "{}: needs a chain rule", p.id);
        assert!(rules.iter().any(|r| r.is_horn()), "{}: needs a when/then rule", p.id);
        assert!(
            rules.iter().any(|r| r.is_horn() && r.then.get(1).and_then(Value::as_str) == Some("a")),
            "{}: needs a rule that derives a class",
            p.id
        );
        assert!(rules.iter().all(|r| !r.description.is_empty()), "{}: every rule has a description", p.id);
        let glossary = d.get("glossary").and_then(|g| g.as_table()).expect("a glossary");
        assert!(section_keys(glossary, "kinds").len() >= 5, "{}: glossary synonyms for kinds", p.id);

        for name in &types {
            let t = &s.object_types[name];
            assert!(!t.label.is_empty() && !t.description.is_empty() && !t.icon.is_empty(), "{}: {name}", p.id);
            let hex = t.color.strip_prefix('#').unwrap_or_default();
            assert!(hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()), "{}: {name} color", p.id);
            assert!(!s.all_props(name).is_empty(), "{}: {name} has no properties", p.id);
            for (prop, def) in s.all_props(name) {
                assert!(def.ty != PropType::Enum || !def.values.is_empty(), "{}: {name}.{prop} enum", p.id);
            }
            for st in t.terminal_states.iter().chain(&t.alert_states) {
                assert!(t.states.contains(st), "{}: {name} lists {st} outside its states", p.id);
            }
        }
        let mut transitive = 0;
        for name in &links {
            let lt = &s.link_types[name];
            assert!(!lt.label.is_empty() && !lt.description.is_empty(), "{}: link type {name}", p.id);
            assert!(!lt.from.is_empty() && !lt.to.is_empty(), "{}: link type {name} needs from and to", p.id);
            assert!(lt.symmetric || lt.inverse.is_some(), "{}: link type {name} needs an inverse", p.id);
            if lt.symmetric {
                assert_eq!(lt.from, lt.to, "{}: symmetric {name} has differing endpoints", p.id);
            }
            if lt.transitive {
                transitive += 1;
                assert_ne!(lt.cardinality, onto_core::schema::Cardinality::One, "{}: transitive {name}", p.id);
            }
        }
        assert!(transitive >= 1, "{}: no transitive link type", p.id);
        assert!(links.iter().any(|l| s.link_types[l].dependency), "{}: no dependency link type", p.id);
        for name in section_keys(&d, "shapes") {
            assert!(!s.shapes[&name].description.is_empty(), "{}: shape {name}", p.id);
        }
        for name in section_keys(&d, "action_types") {
            let a = &s.action_types[&name];
            assert!(!a.description.is_empty() && !a.phrases.is_empty(), "{}: action type {name}", p.id);
        }
    }
}

// ── names ────────────────────────────────────────────────────────────────────

#[test]
fn name_check_sees_definitions_of_every_kind() {
    let names = defined_names(
        r#"
[object_types.meter_reading]
[link_types.read_from]
inverse = "owned_by"
[interfaces.readable]
[metrics.reads]
[action_types.read_meter]
[shapes.reading_shape]
[glossary.kinds]
reading = "meter_reading"
[[rules]]
name = "reader"
when = [["?x", "read_from", "?y"]]
then = ["?x", "a", "read_thing"]
"#,
        "fixture",
    );
    let got: BTreeSet<&str> = names.keys().map(String::as_str).collect();
    let want = BTreeSet::from([
        "meter_reading",
        "read_from",
        "owned_by",
        "readable",
        "reads",
        "read_meter",
        "reading_shape",
        "reading",
        "reader",
        "read_thing",
    ]);
    assert_eq!(got, want);
    let ent = defined_names(ENTERPRISE_SCHEMA, "enterprise");
    assert_eq!(ent["owned_by"], "link_types.owns.inverse", "an inverse name is a definition");
    assert!(ent.contains_key("handled_by_team") && ent.contains_key("ticket"), "derived names and glossary words");
}

/// Every name a pack file defines, with where it is defined. Rule heads that
/// name a relation or class enterprise already has are reuse, not a
/// definition, and are left out by the caller.
fn defined_names(text: &str, what: &str) -> BTreeMap<String, String> {
    let t = toml_of(text, what);
    let mut out = BTreeMap::new();
    let mut put = |name: &str, place: String| {
        out.entry(name.to_string()).or_insert(place);
    };
    for section in ["object_types", "interfaces", "metrics", "action_types", "shapes"] {
        for k in section_keys(&t, section) {
            put(&k, section.to_string());
        }
    }
    if let Some(links) = t.get("link_types").and_then(|v| v.as_table()) {
        for (name, lt) in links {
            put(name, "link_types".into());
            if let Some(inv) = lt.get("inverse").and_then(|v| v.as_str()) {
                put(inv, format!("link_types.{name}.inverse"));
            }
        }
    }
    for rule in t.get("rules").and_then(|v| v.as_array()).into_iter().flatten() {
        let name = rule.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        put(name, "rules".into());
        let head = match rule.get("then").and_then(|v| v.as_array()) {
            Some(then) => match then.get(1).and_then(|v| v.as_str()) {
                Some("a") => then.get(2).and_then(|v| v.as_str()),
                other => other,
            },
            None => rule.get("infer").and_then(|v| v.as_str()),
        };
        if let Some(h) = head {
            put(h.trim_start_matches('^'), format!("derived by rule {name}"));
        }
    }
    if let Some(g) = t.get("glossary").and_then(|v| v.as_table()) {
        for section in ["kinds", "relations", "dimensions"] {
            for k in section_keys(g, section) {
                put(&k, format!("glossary.{section}"));
            }
        }
    }
    out
}

/// Other packs in `schemas/packs` (packs-a and packs-b, once merged).
fn other_pack_files() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas/packs");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("schemas/packs") {
        let path = entry.expect("dir entry").path();
        let Some(stem) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".toml")) else {
            continue;
        };
        if PACKS.iter().any(|p| p.id == stem) {
            continue;
        }
        out.push((stem.to_string(), std::fs::read_to_string(&path).expect("read pack")));
    }
    out.sort();
    out
}

#[test]
fn pack_names_are_unique_across_enterprise_and_every_pack() {
    let ent = defined_names(ENTERPRISE_SCHEMA, "enterprise");
    let base = Schema::enterprise();
    let base_vocab = reason::vocabulary(&base);
    let mut all: Vec<(String, String)> = PACKS.iter().map(|p| (p.id.to_string(), p.toml.to_string())).collect();
    all.extend(other_pack_files());

    let mut owner: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut clashes = Vec::new();
    for (id, text) in &all {
        for (name, place) in defined_names(text, id) {
            let reuse = place.starts_with("derived by")
                && (base.resolve_rel(&name).is_some()
                    || base_vocab.relations.contains(&name)
                    || base_vocab.classes.contains(&name)
                    || base.is_type_or_interface(&name));
            if reuse {
                continue;
            }
            if let Some(at) = ent.get(&name) {
                clashes.push(format!("{id}: {name} ({place}) collides with enterprise ({at})"));
            }
            if let Some((other, at)) = owner.get(&name) {
                clashes.push(format!("{id}: {name} ({place}) collides with pack {other} ({at})"));
            } else {
                owner.insert(name, (id.clone(), place));
            }
        }
    }
    assert!(clashes.is_empty(), "name collisions:\n{}", clashes.join("\n"));
}

#[test]
fn all_packs_layer_together_on_enterprise() {
    let mut s = Schema::enterprise();
    for p in PACKS {
        s.extend_toml(p.toml).unwrap_or_else(|e| panic!("{} on top of the others: {e}", p.id));
    }
    for (id, text) in other_pack_files() {
        s.extend_toml(&text).unwrap_or_else(|e| panic!("{id} together with energy, telecom and saas: {e}"));
    }
    let o = strict(s);
    for p in PACKS {
        o.apply(&ctx(), mutations(p)).unwrap_or_else(|e| panic!("{} sample next to the others: {e}", p.id));
    }
    let stats = closure_of(&o).stats();
    for p in PACKS {
        for rule in pack_rules(p) {
            let n = stats.by_rule.get(&rule).copied().unwrap_or(0);
            assert!(n > 0, "{}: {rule} derives nothing when all packs are installed", p.id);
        }
    }
}

// ── samples ──────────────────────────────────────────────────────────────────

#[test]
fn samples_apply_cleanly_under_a_strict_schema() {
    for p in PACKS {
        let lines = sample(p);
        assert!((25..=60).contains(&lines.len()), "{}: {} sample lines", p.id, lines.len());
        let mut ids = BTreeSet::new();
        for e in &lines {
            assert!(e.workspace.is_none() && e.id.is_none(), "{}: sample lines are workspace agnostic", p.id);
            assert!(e.key.is_some() && e.title.is_some(), "{}: every line has a key and a title", p.id);
            assert!(ids.insert(format!("{}:{}", e.kind, e.key.as_deref().unwrap())), "{}: duplicate line", p.id);
        }
        let o = strict(pack_schema(p));
        let r = o.apply(&ctx(), mutations(p)).unwrap_or_else(|e| panic!("{} sample: {e}", p.id));
        assert!(r.links > 0, "{}", p.id);
        let n = o.read(|g, _| g.len());
        assert_eq!(n, ids.len(), "{}: one entity per sample line", p.id);
        // Re-applying the sample changes nothing.
        let again = o.apply(&ctx(), mutations(p)).unwrap();
        assert_eq!(again.links, 0, "{}: re-applying the sample adds links", p.id);
        assert_eq!(o.read(|g, _| g.len()), n, "{}", p.id);
    }
}

/// Facts each sample is built to entail, one or more per pack rule.
const EXPECTED: &[(&str, &str, &str, &str)] = &[
    ("energy", "<service_point:sp-1001>", "within_territory", "<service_territory:coast>"),
    ("energy", "<service_point:sp-1001>", "within_territory", "<service_territory:west>"),
    ("energy", "<grid_outage:out-2026-0412>", "responsible_crew", "<team:grid-crew-north>"),
    ("energy", "<service_point:sp-1001>", "interrupted_by", "<grid_outage:out-2026-0412>"),
    ("energy", "<service_point:sp-1004>", "interrupted_by", "<grid_outage:out-2026-0413>"),
    ("energy", "<service_point:sp-1003>", "interrupted_by", "<grid_outage:out-2026-0420>"),
    ("energy", "<grid_outage:out-2026-0412>", "a", "#life_support_outage"),
    ("energy", "<transformer:t-1201>", "a", "#grid_asset_at_risk"),
    ("energy", "<grid_outage:out-2026-0412>", "a", "#condition_related_outage"),
    ("telecom", "<network_alarm:alm-90002>", "alarm_impacts", "<network_service:mobile-data-metro>"),
    ("telecom", "<network_alarm:alm-90001>", "alarm_impacts", "<network_service:vpn-harbor-logistics>"),
    ("telecom", "<network_alarm:alm-90001>", "alarm_impacts", "<network_service:mobile-data-metro>"),
    ("telecom", "<network_alarm:alm-90001>", "alarm_affects_account", "<subscriber_account:acct-700100>"),
    ("telecom", "<network_service:vpn-harbor-logistics>", "a", "#impacted_service"),
    ("telecom", "<network_service:vpn-harbor-logistics>", "a", "#sla_breach_risk"),
    ("telecom", "<subscriber_account:acct-5550188>", "a", "#sim_swap_risk"),
    ("saas", "<customer_account:northwind>", "entitled_to", "<product_feature:sso>"),
    ("saas", "<support_ticket:st-501>", "csm_for_ticket", "<person:csm-tomas>"),
    ("saas", "<customer_account:contoso>", "a", "#payment_risk_account"),
    ("saas", "<customer_account:contoso>", "a", "#detractor_account"),
    ("saas", "<customer_account:contoso>", "a", "#churn_risk_account"),
    ("saas", "<customer_account:fabrikam>", "candidate_plan", "<pricing_plan:flowdesk-enterprise>"),
    ("saas", "<product_feature:audit-stream>", "requires_feature", "<product_feature:sso>"),
];

/// Facts the rules must NOT derive from the sample (each rule's conditions matter).
const NOT_EXPECTED: &[(&str, &str, &str, &str)] = &[
    ("energy", "<service_point:sp-1004>", "interrupted_by", "<grid_outage:out-2026-0412>"),
    ("energy", "<grid_outage:out-2026-0413>", "a", "#life_support_outage"),
    ("energy", "<feeder:hb-14>", "a", "#grid_asset_at_risk"),
    ("telecom", "<network_service:mobile-data-metro>", "a", "#sla_breach_risk"),
    ("telecom", "<subscriber_account:acct-5550101>", "a", "#sim_swap_risk"),
    ("saas", "<customer_account:fabrikam>", "a", "#churn_risk_account"),
    ("saas", "<customer_account:northwind>", "a", "#detractor_account"),
];

#[test]
fn every_supplied_rule_fires_on_the_sample() {
    for p in PACKS {
        let o = world(p);
        let c = closure_of(&o);
        let stats = c.stats();
        assert!(stats.fixpoint_reached, "{}: {:?}", p.id, stats.truncated);
        assert_eq!(stats.refused, 0, "{}: refused {:?}", p.id, stats.refusals);
        let pack_rules = pack_rules(p);
        for rule in &pack_rules {
            let n = stats.by_rule.get(rule).copied().unwrap_or(0);
            assert!(n > 0, "{}: {rule} derives nothing from the sample", p.id);
        }
        // Built-in semantics the packs rely on fire too.
        for builtin in ["builtin/transitive", "builtin/type_implements"] {
            assert!(stats.by_rule[builtin] > 0, "{}: {builtin}", p.id);
        }
        for (id, s, pr, ob) in EXPECTED.iter().filter(|e| e.0 == p.id) {
            let f = Fact::new(*s, *pr, *ob);
            let d = c.derivation(&f).unwrap_or_else(|| panic!("{id}: {} is not derived", f.tsv().trim_end()));
            assert!(d.rule.starts_with("schema/") || d.rule.starts_with("builtin/"), "{}", d.rule);
        }
        for (id, s, pr, ob) in NOT_EXPECTED.iter().filter(|e| e.0 == p.id) {
            let f = Fact::new(*s, *pr, *ob);
            assert!(c.contains(&f).is_none(), "{id}: {} should not follow", f.tsv().trim_end());
        }
        // An inference carries a certificate: the closure renders one.
        let cert = c.certificate(Attested::default()).unwrap_or_else(|e| panic!("{}: {e}", p.id));
        for rule in &pack_rules {
            assert!(cert.rules_tsv.contains(&format!("{rule}\t")), "{}: {rule} not in rules.tsv", p.id);
        }
    }
}

// ── shapes ───────────────────────────────────────────────────────────────────

/// One failed constraint.
#[derive(Debug)]
struct ShapeResult {
    shape: String,
    constraint: String,
    component: &'static str,
    focus: String,
}

/// Values reached from `focus` through `path`.
fn path_values(schema: &Schema, g: &Graph, focus: &str, path: &str) -> Vec<Value> {
    let e = g.get(focus).expect("focus exists");
    match ShapePath::parse(path) {
        ShapePath::Title => vec![json!(e.title)],
        ShapePath::State => e.state.iter().map(|s| json!(s)).collect(),
        ShapePath::Prop(name) => e.props.get(&name).filter(|v| !v.is_null()).cloned().into_iter().collect(),
        ShapePath::Relations(steps) => {
            let mut frontier: BTreeSet<String> = BTreeSet::from([focus.to_string()]);
            for step in &steps {
                let (base, dir) = schema.resolve_rel(step).unwrap_or_else(|| panic!("path step {step} resolves"));
                frontier = frontier
                    .iter()
                    .flat_map(|id| g.neighbors(id, &base, dir))
                    .map(str::to_string)
                    .collect::<BTreeSet<_>>();
            }
            frontier.into_iter().map(Value::String).collect()
        }
    }
}

/// Evaluate the named shapes over every entity of the graph. Returns the
/// results and the number of focus nodes per shape.
fn evaluate_shapes(
    schema: &Schema,
    g: &Graph,
    names: &BTreeSet<String>,
) -> (Vec<ShapeResult>, BTreeMap<String, usize>) {
    let mut results = Vec::new();
    let mut focus_count = BTreeMap::new();
    for name in names {
        let shape: &Shape = &schema.shapes[name];
        assert!(
            shape.target_ids.is_empty()
                && shape.target_subjects_of.is_none()
                && shape.target_objects_of.is_none()
                && !shape.closed
                && !shape.deactivated,
            "shape {name}: this evaluator supports kind targets only"
        );
        let target = shape.target.as_deref().unwrap_or_else(|| panic!("shape {name}: needs a target"));
        let mut focus: Vec<&str> =
            g.entities().filter(|e| schema.kind_is(&e.kind, target)).map(|e| e.id.as_str()).collect();
        focus.sort_unstable();
        focus_count.insert(name.clone(), focus.len());
        for ps in &shape.properties {
            assert!(ps.unknown.is_empty(), "shape {name}: unsupported keys {:?}", ps.unknown.keys());
            assert!(ps.node.is_none() && ps.has_value.is_none(), "shape {name}: unsupported component");
            let regex = ps.pattern.as_deref().map(|p| regex_lite::Regex::new(p).expect("pattern compiles"));
            for id in &focus {
                let values = path_values(schema, g, id, &ps.path);
                let mut fail = |component: &'static str| {
                    results.push(ShapeResult {
                        shape: name.clone(),
                        constraint: ps.id.clone(),
                        component,
                        focus: id.to_string(),
                    })
                };
                if ps.min_count.is_some_and(|m| values.len() < m) {
                    fail("min_count");
                }
                if ps.max_count.is_some_and(|m| values.len() > m) {
                    fail("max_count");
                }
                for v in &values {
                    if let Some(class) = &ps.class {
                        let ok = v.as_str().and_then(|id| g.get(id)).is_some_and(|t| schema.kind_is(&t.kind, class));
                        if !ok {
                            fail("class");
                        }
                    }
                    if let Some(ty) = ps.datatype {
                        assert!(ty != PropType::Enum, "shape {name}: datatype enum is not evaluated here");
                        if !(PropertyDef { ty, ..Default::default() }).accepts(v) {
                            fail("datatype");
                        }
                    }
                    if !ps.values.is_empty() && !ps.values.contains(v) {
                        fail("in");
                    }
                    if let Some(r) = &regex
                        && !v.as_str().is_some_and(|s| r.is_match(s))
                    {
                        fail("pattern");
                    }
                    let len = v.as_str().map(|s| s.chars().count());
                    if ps.min_length.is_some_and(|m| len.is_none_or(|l| l < m)) {
                        fail("min_length");
                    }
                    if ps.max_length.is_some_and(|m| len.is_none_or(|l| l > m)) {
                        fail("max_length");
                    }
                    if ps.min_value.is_some_and(|m| v.as_f64().is_none_or(|x| x < m)) {
                        fail("min_value");
                    }
                    if ps.max_value.is_some_and(|m| v.as_f64().is_none_or(|x| x > m)) {
                        fail("max_value");
                    }
                }
            }
        }
    }
    (results, focus_count)
}

#[test]
fn each_sample_holds_exactly_the_documented_shape_violation() {
    for p in PACKS {
        let o = world(p);
        let names = section_keys(&declared(p), "shapes");
        let (results, focus) = o.read(|g, schema| evaluate_shapes(schema, g, &names));
        for (shape, n) in &focus {
            assert!(*n > 0, "{}: shape {shape} selects nothing in the sample (undetermined)", p.id);
        }
        assert_eq!(results.len(), 1, "{}: expected exactly one shape violation, got {results:#?}", p.id);
        let note = meta_str(p, "sample_note");
        let r = &results[0];
        assert!(note.contains(&r.focus), "{}: sample_note does not name {}", p.id, r.focus);
        assert!(note.contains(&r.shape), "{}: sample_note does not name shape {}", p.id, r.shape);
        assert!(note.contains(r.component), "{}: sample_note does not name {}", p.id, r.component);
        assert!(!r.constraint.is_empty());
    }
}

#[test]
fn shape_evaluator_sees_a_repaired_violation() {
    // Fixing the documented violation leaves every shape satisfied, so the
    // single result above is the deliberate one and nothing else.
    let fixes: &[(&str, Mutation)] = &[
        (
            "energy",
            Mutation::Upsert(
                EntityInput::new("smart_meter", "m-stock-0001", "Meter in stock 0001")
                    .link("meter_at", "service_point:sp-1005"),
            ),
        ),
        (
            "telecom",
            Mutation::Upsert(
                EntityInput::new("transport_link", "mw-hop-77", "Microwave hop 77")
                    .link("terminates_on", "network_element:agg-sw-7"),
            ),
        ),
        (
            "saas",
            Mutation::Upsert(
                EntityInput::new("nps_response", "nps-0007", "Fabrikam, Dee Patel, October").prop("nps_score", 10),
            ),
        ),
    ];
    for (id, fix) in fixes {
        let p = PACKS.iter().find(|p| p.id == *id).unwrap();
        let o = world(p);
        o.apply(&ctx(), vec![fix.clone()]).unwrap();
        let names = section_keys(&declared(p), "shapes");
        let (results, _) = o.read(|g, schema| evaluate_shapes(schema, g, &names));
        assert!(results.is_empty(), "{id}: {results:#?}");
    }
}

// ── action types and metrics ─────────────────────────────────────────────────

fn params(v: Value) -> Map<String, Value> {
    v.as_object().cloned().expect("object")
}

#[test]
fn pack_action_types_apply_on_the_sample() {
    // (pack, action, params, entity or "new <kind>" for a created one, state after)
    let cases: &[(&str, &str, Value, &str, &str)] = &[
        (
            "energy",
            "dispatch_outage_crew",
            json!({"outage": "grid_outage:out-2026-0420", "crew": "team:grid-crew-north"}),
            "grid_outage:out-2026-0420",
            "crew_dispatched",
        ),
        (
            "energy",
            "restore_outage",
            json!({"outage": "grid_outage:out-2026-0412"}),
            "grid_outage:out-2026-0412",
            "restored",
        ),
        (
            "energy",
            "retire_transformer",
            json!({"transformer": "transformer:t-1202", "reason": "replaced by T-1203"}),
            "transformer:t-1202",
            "retired",
        ),
        (
            "telecom",
            "acknowledge_alarm",
            json!({"alarm": "network_alarm:alm-90002"}),
            "network_alarm:alm-90002",
            "acknowledged",
        ),
        (
            "telecom",
            "open_trouble_ticket",
            json!({"title": "SIP trunk 17 one-way audio", "service": "network_service:sip-trunk-17"}),
            "new trouble_ticket",
            "open",
        ),
        (
            "telecom",
            "suspend_service",
            json!({"service": "network_service:mobile-data-metro", "reason": "fraud investigation"}),
            "network_service:mobile-data-metro",
            "suspended",
        ),
        (
            "saas",
            "change_subscription_plan",
            json!({"subscription": "subscription:sub-1002", "plan": "pricing_plan:flowdesk-enterprise"}),
            "subscription:sub-1002",
            "active",
        ),
        (
            "saas",
            "cancel_subscription",
            json!({"subscription": "subscription:sub-1004", "reason": "moved to a competitor"}),
            "subscription:sub-1004",
            "cancelled",
        ),
        (
            "saas",
            "escalate_support_ticket",
            json!({"ticket": "support_ticket:st-501", "project": "project:flowdesk-reliability", "title": "Reports time out"}),
            "new issue",
            "open",
        ),
    ];
    for p in PACKS {
        let declared_actions = section_keys(&declared(p), "action_types");
        let covered: BTreeSet<String> = cases.iter().filter(|c| c.0 == p.id).map(|c| c.1.to_string()).collect();
        assert_eq!(declared_actions, covered, "{}: every action type has a case", p.id);
    }
    for (id, action, input, target, state) in cases {
        let p = PACKS.iter().find(|p| p.id == *id).unwrap();
        let o = world(p);
        let before: BTreeSet<String> = o.read(|g, _| g.entities().map(|e| e.id.clone()).collect());
        let receipt = o
            .apply_action(&ctx(), action, &params(input.clone()), ApplyOptions { confirm: true, ..Default::default() })
            .unwrap_or_else(|e| panic!("{id}.{action}: {e}"));
        assert_eq!(receipt.status, "applied", "{id}.{action}: {:?} {:?}", receipt.reason, receipt.plan.checks);
        let touched = match target.strip_prefix("new ") {
            // The created entity: touched, of that kind, and not there before.
            Some(kind) => receipt
                .touched
                .keys()
                .find(|k| k.starts_with(&format!("{kind}:")) && !before.contains(*k))
                .cloned()
                .unwrap_or_else(|| panic!("{id}.{action}: no new {kind}: {:?}", receipt.touched)),
            None => target.to_string(),
        };
        let e = o.get(&admin(), &touched).unwrap();
        assert_eq!(e.state.as_deref(), Some(*state), "{id}.{action}: {touched}");
        // Every applied action is undoable through its receipt.
        o.undo_action(&ctx(), &receipt.id, false).unwrap_or_else(|e| panic!("{id}.{action} undo: {e}"));
        let after: BTreeSet<String> = o.read(|g, _| g.entities().map(|e| e.id.clone()).collect());
        assert_eq!(after, before, "{id}.{action}: undo restores the entity set");
    }

    // Effects beyond the state change.
    let saas = world(&PACKS[2]);
    saas.apply_action(
        &ctx(),
        "change_subscription_plan",
        &params(json!({"subscription": "subscription:sub-1002", "plan": "pricing_plan:flowdesk-enterprise"})),
        ApplyOptions::default(),
    )
    .unwrap();
    let plans = saas.read(|g, _| g.neighbors("subscription:sub-1002", "on_plan", Dir::Out).len());
    assert_eq!(plans, 1, "the plan link is replaced, not added");
    let r = saas
        .apply_action(
            &ctx(),
            "escalate_support_ticket",
            &params(json!({"ticket": "support_ticket:st-501", "project": "project:flowdesk-reliability", "title": "Reports time out"})),
            ApplyOptions::default(),
        )
        .unwrap();
    assert_eq!(r.status, "applied");
    let escalated = saas.read(|g, s| {
        g.neighbors("support_ticket:st-501", "escalated_as", Dir::Out)
            .into_iter()
            .filter(|id| g.get(id).is_some_and(|e| s.kind_is(&e.kind, "issue")))
            .count()
    });
    assert_eq!(escalated, 1, "the new issue is linked back to the ticket");

    // Preconditions refuse what does not apply.
    let energy = world(&PACKS[0]);
    let r = energy
        .apply_action(
            &ctx(),
            "dispatch_outage_crew",
            &params(json!({"outage": "grid_outage:out-2026-0413", "crew": "team:grid-crew-north"})),
            ApplyOptions::default(),
        )
        .unwrap();
    assert_eq!(r.status, "rejected", "a restored outage is not open");
    let r = energy
        .apply_action(
            &ctx(),
            "retire_transformer",
            &params(json!({"transformer": "transformer:t-1202", "reason": "x"})),
            ApplyOptions::default(),
        )
        .unwrap();
    assert_ne!(r.status, "applied", "a high-risk action needs confirmation");
    let member = ActionContext::new(Principal::member("bo", "acme"), "acme");
    let r = energy
        .apply_action(
            &member,
            "retire_transformer",
            &params(json!({"transformer": "transformer:t-1202", "reason": "x"})),
            ApplyOptions { confirm: true, ..Default::default() },
        )
        .unwrap();
    assert_eq!(r.status, "rejected", "retiring an asset is for admins");
}

#[test]
fn pack_metrics_evaluate_on_the_sample() {
    for p in PACKS {
        let o = world(p);
        let s = pack_schema(p);
        for name in section_keys(&declared(p), "metrics") {
            let def = &s.metrics[&name];
            let q = MetricQuery { metric: Some(name.clone()), ..Default::default() };
            o.metric(&admin(), &q).unwrap_or_else(|e| panic!("{}: metric {name}: {e}", p.id));
            for dim in def.dimensions.keys() {
                let q = MetricQuery { metric: Some(name.clone()), group_by: vec![dim.clone()], ..Default::default() };
                o.metric(&admin(), &q).unwrap_or_else(|e| panic!("{}: metric {name} by {dim}: {e}", p.id));
            }
        }
    }
    // Spot values computed from the samples.
    let energy = world(&PACKS[0]);
    let q = MetricQuery { metric: Some("customers_interrupted".into()), ..Default::default() };
    let r = serde_json::to_value(energy.metric(&admin(), &q).unwrap()).unwrap();
    assert_eq!(r["value"].as_f64(), Some(3.0), "{r}");
    let saas = world(&PACKS[2]);
    let q = MetricQuery { metric: Some("monthly_recurring_revenue".into()), ..Default::default() };
    let r = serde_json::to_value(saas.metric(&admin(), &q).unwrap()).unwrap();
    assert_eq!(r["value"].as_f64(), Some(12500.0 + 1600.0 + 1000.0 + 95.0), "{r}");
}

// ── live install ─────────────────────────────────────────────────────────────

#[test]
fn packs_install_live_through_a_schema_change_set() {
    let o = Ontology::in_memory(Schema::enterprise());
    let c = ctx();
    for p in PACKS {
        let current = o.schema();
        let target = {
            let mut t = (*current).clone();
            t.extend_toml(p.toml).unwrap();
            t
        };
        let set = SchemaChangeSet {
            changes: changes_between(&current, &target),
            message: format!("install pack {}", p.id),
            ..Default::default()
        };
        assert!(!set.changes.is_empty(), "{}", p.id);
        let plan = o.schema_plan(&admin(), &set).unwrap();
        assert!(plan.applicable(), "{}: {:?} {:?}", p.id, plan.errors, plan.needs_migration);
        assert!(!plan.breaking, "{}: installing a pack breaks nothing", p.id);
        o.schema_apply(&c, set).unwrap_or_else(|e| panic!("{}: {e}", p.id));
        o.apply(&c, mutations(p)).unwrap_or_else(|e| panic!("{} sample after a live install: {e}", p.id));
        let installed = o.schema();
        assert_eq!(
            installed.shapes.keys().collect::<Vec<_>>(),
            target.shapes.keys().collect::<Vec<_>>(),
            "{}: shapes arrive with the change set",
            p.id
        );
        assert_eq!(installed.rules.len(), target.rules.len(), "{}", p.id);
        // Installing again changes nothing.
        assert!(changes_between(&installed, &target).is_empty(), "{}: re-install is a no-op", p.id);
    }
    let stats = o.reason_stats(&admin(), &ReasonOptions::default()).unwrap();
    for p in PACKS {
        for rule in pack_rules(p) {
            assert!(stats.by_rule.get(&rule).copied().unwrap_or(0) > 0, "{rule} after a live install");
        }
    }
}
