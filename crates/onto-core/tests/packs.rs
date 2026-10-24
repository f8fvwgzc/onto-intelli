//! Industry packs and the marketplace. Every pack in the registry is held to
//! one standard: its `[pack]` table is honest (borrowed vocabulary, no
//! compliance claimed, supplied assumptions named as such, the deliberate
//! sample violation documented), it compiles on enterprise without
//! redefining it, carries the content the pack guide asks for, shares no
//! name or glossary word with another package, and its sample loads under a
//! strict schema, makes every supplied rule fire and holds exactly one shape
//! violation (measured by the shapes validator itself). Its action types and
//! metrics run on the sample, and the pack can be listed, shown, planned,
//! installed (apply, propose, sample) and reverted.
//!
//! The registry-driven checks come first; tests named after one pack pin
//! that pack's documented behaviour (what its rules derive, what its
//! actions refuse).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use onto_core::action_types::ApplyOptions;
use onto_core::live_schema::Migration;
use onto_core::metrics::{Measure, MetricQuery};
use onto_core::packs::{self, InstallMode, PackInstallOptions};
use onto_core::reason::{self, Fact, ReasonOptions, SUPPLIED_PREFIX};
use onto_core::schema::{Cardinality, Dir, PACKAGES, PropType};
use onto_core::shapes::{self, ShapeVerdict, ShapesQuery};
use onto_core::store::MemoryStore;
use onto_core::{
    ActionContext, Decision, EntityInput, Error, EventFilter, Governance, LinkInput, Mutation, Ontology,
    OntologyOptions, Principal, Role, Schema,
};

fn admin() -> Principal {
    Principal::admin("ada")
}

fn ctx() -> ActionContext {
    ActionContext::new(admin(), "acme")
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

/// The rule-table names (`schema/<name>`) of the rules the pack declares.
fn rule_names(id: &str) -> Vec<String> {
    let raw = layer(id);
    let rules = raw.get("rules").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    rules.iter().map(|r| format!("{SUPPLIED_PREFIX}{}", r["name"].as_str().unwrap())).collect()
}

/// enterprise ⊕ pack, as configuration would load it.
fn with_pack(id: &str) -> Schema {
    let mut s = Schema::enterprise();
    s.extend_source(&format!("builtin:{id}"), |p: &str| std::path::PathBuf::from(p)).unwrap();
    s
}

fn strict(schema: Schema) -> Ontology {
    let opts = OntologyOptions { strict: true, ..Default::default() };
    Ontology::open(schema, Arc::new(MemoryStore::default()), opts).unwrap()
}

/// A strict in-memory ontology (unknown kinds, relations and properties are
/// refused) with enterprise ⊕ pack and the pack's sample loaded.
fn sampled(id: &str) -> Ontology {
    let o = strict(with_pack(id));
    o.apply(&ctx(), packs::sample_mutations(packs::sample(id).unwrap())).unwrap_or_else(|e| panic!("{id} sample: {e}"));
    o
}

fn closure_of(o: &Ontology) -> reason::Closure {
    o.read(|g, schema| reason::closure(schema, g, &ReasonOptions::default(), &|_| true))
}

fn options(mode: InstallMode, sample: bool) -> PackInstallOptions {
    PackInstallOptions { mode, sample, message: None }
}

fn params(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().expect("an object")
}

/// Fail once with every problem found, so one run shows them all.
fn report(what: &str, problems: Vec<String>) {
    assert!(problems.is_empty(), "{what}:\n  {}", problems.join("\n  "));
}

// ── registry and metadata ────────────────────────────────────────────────

/// Top-level sections a pack layer may carry. `name`, `version` and
/// `analysis` would overwrite the base schema's, so they are not among them.
const PACK_SECTIONS: &[&str] =
    &["pack", "object_types", "link_types", "interfaces", "rules", "glossary", "shapes", "metrics", "action_types"];

/// Words that claim conformance rather than borrow vocabulary.
const COMPLIANCE_CLAIMS: &[&str] = &["complian", "certified", "conformant", "conforms"];

#[test]
fn registry_matches_builtin_packages_and_metadata_is_honest() {
    assert_eq!(ids().len(), 10, "{:?}", ids());
    let mut problems = Vec::new();
    for (id, toml, sample) in packs::REGISTRY {
        let pkg = PACKAGES.iter().find(|(n, _)| *n == *id).unwrap_or_else(|| panic!("{id} is not in PACKAGES"));
        assert_eq!(pkg.1, *toml, "{id}: PACKAGES and the registry carry the same text");
        assert_eq!(onto_core::schema::builtin_package(id).unwrap(), *toml);
        assert!(!sample.is_empty());
        let meta = packs::parse_meta(toml).unwrap();
        let mut bad = |ok: bool, what: &str| {
            if !ok {
                problems.push(format!("{id}: {what}"));
            }
        };
        bad(meta.id == *id, "[pack].id is the registry id");
        bad(meta.version == "1.0.0", "version 1.0.0");
        bad(meta.requires == ["enterprise"], "requires = [\"enterprise\"]");
        bad(!meta.title.is_empty() && !meta.industry.is_empty(), "a title and an industry");
        bad(!meta.standards.is_empty(), "names the standards whose vocabulary it borrows");
        for st in &meta.standards {
            let lower = st.to_lowercase();
            bad(!COMPLIANCE_CLAIMS.iter().any(|w| lower.contains(w)), &format!("standard '{st}' claims compliance"));
        }
        // One wording for every pack: what it borrows, what its rules are,
        // and that the sample is imperfect on purpose.
        let d = &meta.description;
        bad(d.contains("claims no compliance"), "the description says it claims no compliance");
        bad(d.contains("supplied assumptions"), "the description calls its rules supplied assumptions");
        bad(d.contains("deliberate shape violation"), "the description mentions the deliberate shape violation");
        bad(!d.contains("entailed"), "the description leaves the checker's verdict words to the checker");
        bad(!meta.sample_note.is_empty(), "a sample_note documents the deliberate shape violation");
        let raw: toml::Table = toml::from_str(toml).unwrap();
        for key in raw.keys() {
            bad(PACK_SECTIONS.contains(&key.as_str()), &format!("top-level key {key} is not a pack section"));
        }
    }
    report("pack metadata", problems);
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
        for r in &base.rules {
            assert!(s.rules.iter().any(|x| x.name == r.name && x.describe() == r.describe()), "{id}: {}", r.name);
        }
        for (word, kind) in &base.glossary.kinds {
            assert_eq!(&s.glossary.kinds[word], kind, "{id} remaps the enterprise glossary word {word}");
        }
        for (word, rel) in &base.glossary.relations {
            assert_eq!(&s.glossary.relations[word], rel, "{id} remaps the enterprise glossary word {word}");
        }
        // The engine reads every pack rule (no refusals).
        let (lines, refused) = reason::rule_table(&s);
        assert!(refused.is_empty(), "{id}: {refused:?}");
        assert_eq!(lines.iter().filter(|l| !l.is_builtin()).count(), s.rules.len(), "{id}");
        assert_eq!(s.rules.len(), base.rules.len() + rule_names(id).len(), "{id}");
    }
}

#[test]
fn every_pack_has_the_promised_content() {
    let enterprise = Schema::enterprise();
    let mut problems = Vec::new();
    for id in ids() {
        let s = with_pack(id);
        let raw = layer(id);
        let mut bad = |ok: bool, what: String| {
            if !ok {
                problems.push(format!("{id}: {what}"));
            }
        };
        let n = |k: &str| table_len(&raw, k);
        let rules: Vec<_> = s.rules.iter().filter(|r| !enterprise.rules.iter().any(|e| e.name == r.name)).collect();
        bad((8..=14).contains(&n("object_types")), format!("{} object types", n("object_types")));
        bad((10..=20).contains(&n("link_types")), format!("{} link types", n("link_types")));
        bad((3..=6).contains(&rules.len()), format!("{} rules", rules.len()));
        bad((3..=6).contains(&n("shapes")), format!("{} shapes", n("shapes")));
        bad((2..=4).contains(&n("metrics")), format!("{} metrics", n("metrics")));
        bad((1..=3).contains(&n("action_types")), format!("{} action types", n("action_types")));
        let glossary = raw.get("glossary").and_then(|g| g.as_table()).cloned().unwrap_or_default();
        bad(
            table_len(&glossary, "kinds") >= 8,
            format!("{} glossary synonyms for kinds", table_len(&glossary, "kinds")),
        );

        // Object types: presentable, typed, with consistent lifecycles.
        let types = names_of(&raw, "object_types");
        bad(types.iter().any(|k| !s.object_types[k].states.is_empty()), "no type has lifecycle states".into());
        for name in &types {
            let ty = &s.object_types[name];
            bad(!ty.label.is_empty() && !ty.description.is_empty() && !ty.icon.is_empty(), format!("{name}: label"));
            let hex = ty.color.strip_prefix('#').unwrap_or_default();
            bad(hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()), format!("{name}: color {}", ty.color));
            bad(!s.all_props(name).is_empty(), format!("{name} has no properties"));
            for st in ty.terminal_states.iter().chain(&ty.alert_states) {
                bad(ty.states.contains(st), format!("{name}: {st} is not one of its states"));
            }
            for (prop, def) in &ty.properties {
                bad(def.ty != PropType::Enum || !def.values.is_empty(), format!("{name}.{prop}: enum without values"));
            }
            let props = raw["object_types"][name.as_str()].get("properties").and_then(|p| p.as_table());
            for (prop, def) in props.into_iter().flatten() {
                bad(def.get("type").is_some(), format!("{name}.{prop} declares its type"));
            }
            if s.kind_is(name, "issue") && !ty.states.is_empty() {
                // Enterprise's assign, block and resolve set these states.
                for st in ["open", "triaged", "blocked", "resolved"] {
                    bad(ty.states.iter().any(|x| x == st), format!("issue subtype {name} drops {st}"));
                }
            }
        }

        // Link types: described, constrained, navigable both ways.
        let links = names_of(&raw, "link_types");
        for name in &links {
            let lt = &s.link_types[name];
            bad(!lt.description.is_empty(), format!("link type {name} is described"));
            bad(!lt.from.is_empty() && !lt.to.is_empty(), format!("link type {name}: from and to"));
            // A symmetric relation reads the same both ways: it needs no inverse.
            bad(lt.inverse.is_some() || lt.symmetric, format!("link type {name}: an inverse (or symmetric)"));
            if lt.symmetric {
                bad(lt.from == lt.to, format!("symmetric {name} has differing endpoints"));
            }
            bad(raw["link_types"][name.as_str()].get("weight").is_some(), format!("link type {name} sets its weight"));
            bad(!(lt.transitive && lt.cardinality == Cardinality::One), format!("transitive {name} is single-valued"));
        }
        bad(links.iter().any(|l| s.link_types[l].transitive), "no transitive relation".into());
        // Reuse, not reinvent: the pack connects to enterprise vocabulary.
        let reused = ["person", "team", "workspace", "project", "knowledge", "decision", "artifact", "issue"];
        let touches =
            links.iter().any(|l| {
                let lt = &s.link_types[l];
                lt.from.iter().chain(&lt.to).any(|k| reused.contains(&k.as_str()))
            }) || types.iter().any(|k| s.object_types[k].extends.as_deref().is_some_and(|p| reused.contains(&p)));
        bad(touches, "links to no enterprise type".into());

        // Rules, shapes, metrics, action types.
        bad(rules.iter().any(|r| r.is_horn()) && rules.iter().any(|r| !r.is_horn()), "both rule forms".into());
        bad(rules.iter().all(|r| !r.description.is_empty()), "every rule is described".into());
        let shape_defs: BTreeMap<_, _> =
            names_of(&raw, "shapes").into_iter().map(|n| (n.clone(), s.shapes[&n].clone())).collect();
        for (name, sh) in &shape_defs {
            bad(!sh.description.is_empty(), format!("shape {name} is described"));
        }
        let check = shapes::check(&s, &shape_defs);
        bad(check.ok, format!("shapes check: {:?}", check.issues));
        for name in names_of(&raw, "metrics") {
            bad(!s.metrics[&name].description.is_empty(), format!("metric {name} is described"));
        }
        let actions = names_of(&raw, "action_types");
        for name in &actions {
            let a = &s.action_types[name];
            bad(
                !a.description.is_empty() && !a.phrases.is_empty(),
                format!("action type {name}: description, phrases"),
            );
            bad(!a.effects.is_empty(), format!("action type {name} has effects"));
            bad(raw["action_types"][name.as_str()].get("risk").is_some(), format!("action type {name} declares risk"));
        }
        // A record-creating action may have nothing to guard; a pack still
        // guards at least one of its edits.
        bad(actions.iter().any(|a| !s.action_types[a].preconditions.is_empty()), "no guarded action type".into());
    }
    report("pack content", problems);
}

#[test]
fn glossary_words_resolve_to_their_targets() {
    let mut problems = Vec::new();
    for id in ids() {
        let s = with_pack(id);
        let raw = layer(id);
        let glossary = raw.get("glossary").and_then(|g| g.as_table()).cloned().unwrap_or_default();
        for (section, words) in &glossary {
            for (word, target) in words.as_table().into_iter().flatten() {
                let target = target.as_str().unwrap_or_default();
                let got = match section.as_str() {
                    "kinds" => s.canonical_kind(word),
                    "relations" => s.canonical_rel(word),
                    _ => continue,
                };
                if got.as_deref() != Some(target) {
                    problems.push(format!("{id}: glossary.{section}.{word} = {target} resolves to {got:?}"));
                }
            }
        }
    }
    report("glossary", problems);
}

#[test]
fn packs_install_together_and_over_the_default_layers() {
    // In registry order and reversed, alone on enterprise and over the
    // default configuration layers (data-catalog, code).
    let mut orders = vec![ids()];
    orders.push(ids().into_iter().rev().collect());
    let mut layered = Vec::new();
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
            let expected_rules = before.rules.len() + ids().iter().map(|id| rule_names(id).len()).sum::<usize>();
            assert_eq!(s.rules.len(), expected_rules, "no pack rule replaces another");
            let mut labels: BTreeMap<&str, &str> = BTreeMap::new();
            for ty in s.object_types.values().filter(|t| !t.label.is_empty()) {
                if let Some(other) = labels.insert(ty.label.as_str(), ty.name.as_str()) {
                    panic!("{} and {other} share the label {}", ty.name, ty.label);
                }
            }
            if !defaults {
                layered.push(s);
            }
        }
    }
    // Order does not matter.
    assert_eq!(json!(layered[0].object_types), json!(layered[1].object_types));
    assert_eq!(json!(layered[0].link_types), json!(layered[1].link_types));

    // Every sample loads next to all the others, and every rule still fires.
    let o = strict(layered.swap_remove(0));
    for id in ids() {
        o.apply(&ctx(), packs::sample_mutations(packs::sample(id).unwrap()))
            .unwrap_or_else(|e| panic!("{id} sample next to the others: {e}"));
    }
    let stats = closure_of(&o).stats();
    assert!(stats.fixpoint_reached && stats.refused == 0, "{stats:?}");
    for id in ids() {
        for rule in rule_names(id) {
            assert!(stats.by_rule.get(&rule).copied().unwrap_or(0) > 0, "{id}: {rule} with every pack installed");
        }
    }
}

#[test]
fn pack_names_never_collide() {
    let collisions = packs::collisions();
    assert!(collisions.is_empty(), "{collisions:#?}");
    // A glossary word is never also a name some package defines in the same
    // namespace (a kind word that is a type or interface, a relation word that
    // is a link type or inverse): the name would win over the word's target.
    let (mut kinds, mut relations) = (BTreeSet::new(), BTreeSet::new());
    for (_, text) in PACKAGES {
        let t: toml::Table = toml::from_str(text).unwrap();
        kinds.extend(names_of(&t, "object_types").into_iter().chain(names_of(&t, "interfaces")));
        relations.extend(names_of(&t, "link_types"));
        for lt in t.get("link_types").and_then(|v| v.as_table()).into_iter().flat_map(|t| t.values()) {
            relations.extend(lt.get("inverse").and_then(|v| v.as_str()).map(str::to_string));
        }
    }
    let mut problems = Vec::new();
    for id in ids() {
        let raw = layer(id);
        let glossary = raw.get("glossary").and_then(|g| g.as_table()).cloned().unwrap_or_default();
        for (section, names) in [("kinds", &kinds), ("relations", &relations)] {
            let words = glossary.get(section).and_then(|w| w.as_table()).cloned().unwrap_or_default();
            for word in words.keys().filter(|w| names.contains(*w)) {
                problems.push(format!("{id}: glossary.{section}.{word} is also a defined name"));
            }
        }
    }
    report("glossary words that are names", problems);

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
        then = ["?x", "a", "promoted_stockout"]
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
        [("names", "supplier"), ("names", "carries"), ("names", "promoted_stockout"), ("glossary.kinds", "vendor")]
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
fn samples_load_cleanly_under_a_strict_schema() {
    for id in ids() {
        let entities = packs::sample(id).unwrap();
        assert!((25..=60).contains(&entities.len()), "{id}: {} sample lines", entities.len());
        let mut seen = BTreeSet::new();
        for e in &entities {
            assert!(e.workspace.is_none() && e.id.is_none(), "{id}: sample lines are workspace-agnostic");
            let key = e.key.as_deref().unwrap_or_default();
            assert!(!key.is_empty() && e.title.as_deref().is_some_and(|t| !t.is_empty()), "{id}: {} {key}", e.kind);
            assert!(seen.insert(format!("{}:{key}", e.kind)), "{id}: {}:{key} appears twice", e.kind);
        }
        let o = sampled(id);
        let raw = layer(id);
        o.read(|g, schema| {
            assert_eq!(g.entities().count(), seen.len(), "{id}: one entity per line");
            // Every pack type has an entity and every pack relation a link.
            for k in names_of(&raw, "object_types") {
                assert!(g.entities().any(|e| e.kind == k), "{id}: the sample has no {k}");
            }
            for l in names_of(&raw, "link_types") {
                assert!(g.links().any(|x| x.rel == l), "{id}: the sample has no {l} link");
            }
            // Every stored link fits its declared endpoints.
            for l in g.links() {
                let (from, to) = (g.get(&l.from).unwrap(), g.get(&l.to).unwrap());
                schema.validate_link(&from.kind, &l.rel, &to.kind, true).unwrap_or_else(|e| panic!("{id}: {e}"));
            }
        });
        // Loading the sample again adds nothing.
        let again = o.apply(&ctx(), packs::sample_mutations(packs::sample(id).unwrap())).unwrap();
        assert_eq!(again.links, 0, "{id}: re-applying the sample adds links");
        assert_eq!(o.read(|g, _| g.entities().count()), seen.len(), "{id}");
    }
}

#[test]
fn samples_make_every_supplied_rule_fire() {
    for id in ids() {
        let o = sampled(id);
        let c = closure_of(&o);
        let stats = c.stats();
        assert!(stats.fixpoint_reached, "{id}: {stats:?}");
        assert_eq!(stats.refused, 0, "{id}: {:?}", stats.refusals);
        // The engine wrapper (admin scope) agrees with the pure reasoner.
        assert_eq!(o.reason(&admin(), &ReasonOptions::default()).unwrap().derived().len(), c.derived().len(), "{id}");
        let rules = rule_names(id);
        for rule in &rules {
            let n = stats.by_rule.get(rule).copied().unwrap_or(0);
            assert!(n > 0, "{id}: rule {rule} derives nothing from the sample ({:?})", stats.by_rule);
        }
        // The built-in semantics a pack declares are exercised by its sample
        // too: every transitive relation is chained at least once, and the
        // interfaces (if any) are implemented.
        let raw = layer(id);
        let link_types = raw.get("link_types").and_then(|v| v.as_table()).cloned().unwrap_or_default();
        for (rel, def) in &link_types {
            if def.get("transitive").and_then(|v| v.as_bool()) == Some(true) {
                let chained = c.derived().iter().any(|d| d.rule == "builtin/transitive" && d.conclusion.p == *rel);
                assert!(chained, "{id}: the sample never chains the transitive relation {rel}");
            }
        }
        if table_len(&raw, "interfaces") > 0 {
            let n = stats.by_rule.get("builtin/type_implements").copied().unwrap_or(0);
            assert!(n > 0, "{id}: builtin/type_implements never fires");
        }
        // The run yields a certificate citing the pack's rules as supplied.
        let cert = o.reason_certificate(&admin(), &ReasonOptions::default()).unwrap();
        for rule in &rules {
            assert!(cert.rules_tsv.lines().any(|l| l.starts_with(&format!("{rule}\t"))), "{id}: {rule} in rules.tsv");
        }
        assert_eq!(cert.derivations_tsv.lines().count(), stats.derived, "{id}");
    }
}

/// Facts each sample is built to entail (spelled), beyond "every rule fires".
const ENTAILED: &[(&str, [&str; 3])] = &[
    ("healthcare", ["<patient:mrn-100231>", "treated_by_clinician", "<clinician:dr-okafor>"]),
    ("healthcare", ["<patient:mrn-100233>", "treated_by_clinician", "<clinician:dr-lindqvist>"]),
    ("healthcare", ["<patient:mrn-100231>", "receives_medication", "<medication:amoxicillin-500>"]),
    ("healthcare", ["<medication:amoxicillin-500>", "in_drug_class", "<drug_class:j01c>"]),
    // Up the ATC hierarchy: J01CA within J01C within J01.
    ("healthcare", ["<medication:amoxicillin-500>", "in_drug_class", "<drug_class:j01>"]),
    ("healthcare", ["<medication_order:mo-1002>", "a", "#contraindicated_order"]),
    ("healthcare", ["<patient:mrn-100231>", "a", "#critical_result_patient"]),
    ("healthcare", ["<clinician:dr-okafor>", "notify_critical_result", "<patient:mrn-100231>"]),
    ("healthcare", ["<care_unit:st-anne-ccu>", "care_unit_within", "<care_facility:st-anne>"]),
    ("healthcare", ["<clinician:dr-okafor>", "a", "#person"]),
    ("healthcare", ["<encounter:enc-5001>", "a", "#clinical_record"]),
    ("finance", ["<private_individual:maria-lopez>", "pays_party", "<private_individual:emil-brandt>"]),
    ("finance", ["<legal_entity:northwind-holdings>", "pays_party", "<legal_entity:kestrel-trading>"]),
    ("finance", ["<private_individual:viktor-hale>", "resident_in", "<jurisdiction:xa>"]),
    // Kestrel Trading FZE resides in a free zone inside the northern province.
    ("finance", ["<legal_entity:kestrel-trading>", "resident_in", "<jurisdiction:xa>"]),
    ("finance", ["<private_individual:viktor-hale>", "a", "#sanctioned_party"]),
    ("finance", ["<legal_entity:kestrel-trading>", "a", "#sanctioned_party"]),
    ("finance", ["<legal_entity:kestrel-logistics>", "a", "#sanctioned_party"]),
    ("finance", ["<private_individual:viktor-hale>", "controls_entity", "<legal_entity:kestrel-logistics>"]),
    ("finance", ["<funds_transfer:ft-1002>", "pays_sanctioned_party", "<legal_entity:kestrel-trading>"]),
    ("finance", ["<funds_transfer:ft-1005>", "pays_sanctioned_party", "<legal_entity:kestrel-logistics>"]),
    ("finance", ["<private_individual:viktor-hale>", "a", "#enhanced_due_diligence_party"]),
    ("finance", ["<legal_entity:kestrel-trading>", "a", "#enhanced_due_diligence_party"]),
    ("finance", ["<private_individual:maria-lopez>", "connected_party", "<private_individual:emil-brandt>"]),
    ("finance", ["<private_individual:emil-brandt>", "connected_party", "<private_individual:maria-lopez>"]),
    // The cover wording, before exclusions: what the sections name.
    ("insurance", ["<insurance_policy:pol-30000101>", "peril_in_cover_wording", "<peril:flood>"]),
    ("insurance", ["<insurance_policy:pol-30000102>", "peril_in_cover_wording", "<peril:natural-catastrophe>"]),
    ("insurance", ["<peril:storm-surge>", "sub_peril_of", "<peril:natural-catastrophe>"]),
    ("insurance", ["<claim:clm-20260002>", "a", "#excluded_peril_claim"]),
    // Storm surge is a narrower kind of flood, which cov-30000102-buildings excludes.
    ("insurance", ["<claim:clm-20260007>", "a", "#excluded_peril_claim"]),
    ("insurance", ["<claim:clm-20260004>", "a", "#claim_on_lapsed_policy"]),
    ("insurance", ["<claim:clm-20260001>", "a", "#duplicate_item_claim"]),
    ("insurance", ["<claim:clm-20260002>", "a", "#duplicate_item_claim"]),
    ("insurance", ["<loss_event:storm-ada-2026>", "recoverable_under", "<reinsurance_treaty:cat-xl-2026>"]),
    ("energy", ["<service_point:sp-1001>", "within_territory", "<service_territory:coast>"]),
    ("energy", ["<service_point:sp-1001>", "within_territory", "<service_territory:west>"]),
    ("energy", ["<grid_outage:out-2026-0412>", "responsible_crew", "<team:grid-crew-north>"]),
    ("energy", ["<service_point:sp-1001>", "interrupted_by", "<grid_outage:out-2026-0412>"]),
    ("energy", ["<service_point:sp-1004>", "interrupted_by", "<grid_outage:out-2026-0413>"]),
    ("energy", ["<service_point:sp-1003>", "interrupted_by", "<grid_outage:out-2026-0420>"]),
    ("energy", ["<grid_outage:out-2026-0412>", "a", "#outage_affecting_life_support"]),
    ("energy", ["<transformer:t-1201>", "a", "#grid_asset_at_risk"]),
    ("energy", ["<grid_outage:out-2026-0412>", "a", "#condition_related_outage"]),
    ("telecom", ["<network_alarm:alm-90002>", "alarm_impacts", "<network_service:mobile-data-metro>"]),
    ("telecom", ["<network_alarm:alm-90001>", "alarm_impacts", "<network_service:vpn-harbor-logistics>"]),
    ("telecom", ["<network_alarm:alm-90001>", "alarm_impacts", "<network_service:mobile-data-metro>"]),
    ("telecom", ["<network_alarm:alm-90001>", "alarm_affects_account", "<subscriber_account:acct-700100>"]),
    // A service that runs only on a radio cell: its baseband unit and the
    // aggregation hierarchy above it reach it.
    ("telecom", ["<network_service:voice-riverside>", "realized_by", "<network_element:bbu-4471>"]),
    ("telecom", ["<network_alarm:alm-90002>", "alarm_impacts", "<network_service:voice-riverside>"]),
    ("telecom", ["<network_alarm:alm-90001>", "alarm_impacts", "<network_service:voice-riverside>"]),
    ("telecom", ["<network_service:vpn-harbor-logistics>", "a", "#sla_breach_risk"]),
    ("telecom", ["<subscriber_account:acct-5550188>", "a", "#sim_swap_risk"]),
    ("saas", ["<customer_account:northwind>", "entitled_to", "<product_feature:sso>"]),
    ("saas", ["<support_ticket:st-501>", "csm_for_ticket", "<person:csm-tomas>"]),
    ("saas", ["<customer_account:contoso>", "a", "#payment_risk_account"]),
    ("saas", ["<customer_account:contoso>", "a", "#detractor_account"]),
    ("saas", ["<customer_account:contoso>", "a", "#churn_risk_account"]),
    ("saas", ["<customer_account:fabrikam>", "candidate_plan", "<pricing_plan:flowdesk-enterprise>"]),
    ("saas", ["<product_feature:audit-stream>", "requires_feature", "<product_feature:sso>"]),
];

/// Facts the samples must not entail: each rule's conditions matter.
const NOT_ENTAILED: &[(&str, [&str; 3])] = &[
    ("healthcare", ["<medication_order:mo-1001>", "a", "#contraindicated_order"]),
    ("healthcare", ["<patient:mrn-100232>", "a", "#critical_result_patient"]),
    ("healthcare", ["<clinician:dr-lindqvist>", "notify_critical_result", "<patient:mrn-100233>"]),
    ("healthcare", ["<medication:metoprolol-50>", "in_drug_class", "<drug_class:j01c>"]),
    // mrn-100233's only metoprolol order (mo-1004) is still a draft.
    ("healthcare", ["<patient:mrn-100233>", "receives_medication", "<medication:metoprolol-50>"]),
    ("finance", ["<private_individual:oskar-lind>", "a", "#sanctioned_party"]),
    ("finance", ["<legal_entity:northwind-shipping>", "a", "#sanctioned_party"]),
    ("finance", ["<funds_transfer:ft-1003>", "pays_sanctioned_party", "<legal_entity:northwind-shipping>"]),
    ("finance", ["<legal_entity:kestrel-logistics>", "a", "#enhanced_due_diligence_party"]),
    ("insurance", ["<claim:clm-20260001>", "a", "#excluded_peril_claim"]),
    // Windstorm is not within flood, so the flood exclusion does not touch it.
    ("insurance", ["<claim:clm-20260003>", "a", "#excluded_peril_claim"]),
    ("insurance", ["<claim:clm-20260006>", "a", "#duplicate_item_claim"]),
    ("insurance", ["<claim:clm-20260003>", "a", "#claim_on_lapsed_policy"]),
    ("insurance", ["<loss_event:storm-ada-2026>", "recoverable_under", "<reinsurance_treaty:qs-motor-2026>"]),
    ("insurance", ["<insurance_policy:pol-30000103>", "peril_in_cover_wording", "<peril:flood>"]),
    // No rule decides coverage (Horn rules cannot apply exclusions), and the
    // wording holds only the perils the sections name.
    ("insurance", ["<insurance_policy:pol-30000101>", "policy_covers", "<peril:storm-surge>"]),
    ("insurance", ["<insurance_policy:pol-30000101>", "peril_in_cover_wording", "<peril:storm-surge>"]),
    ("energy", ["<service_point:sp-1004>", "interrupted_by", "<grid_outage:out-2026-0412>"]),
    ("energy", ["<grid_outage:out-2026-0413>", "a", "#outage_affecting_life_support"]),
    ("energy", ["<feeder:hb-14>", "a", "#grid_asset_at_risk"]),
    ("telecom", ["<network_service:mobile-data-metro>", "a", "#sla_breach_risk"]),
    ("telecom", ["<network_service:voice-riverside>", "a", "#sla_breach_risk"]),
    ("telecom", ["<network_service:voice-riverside>", "realized_by", "<network_element:bbu-4472>"]),
    ("telecom", ["<subscriber_account:acct-5550101>", "a", "#sim_swap_risk"]),
    ("saas", ["<customer_account:fabrikam>", "a", "#churn_risk_account"]),
    ("saas", ["<customer_account:northwind>", "a", "#detractor_account"]),
];

#[test]
fn samples_entail_what_their_packs_document() {
    let mut problems = Vec::new();
    for id in ids() {
        let c = closure_of(&sampled(id));
        for (_, f) in ENTAILED.iter().filter(|(p, _)| *p == id) {
            if c.contains(&Fact::new(f[0], f[1], f[2])).is_none() {
                problems.push(format!("{id}: expected {} {} {}", f[0], f[1], f[2]));
            }
        }
        for (_, f) in NOT_ENTAILED.iter().filter(|(p, _)| *p == id) {
            if c.contains(&Fact::new(f[0], f[1], f[2])).is_some() {
                problems.push(format!("{id}: did not expect {} {} {}", f[0], f[1], f[2]));
            }
        }
    }
    report("documented facts", problems);
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

#[test]
fn supplier_risk_climbs_tiers_only_through_the_supplied_rules() {
    // rhein-bearings buys from lumen-metals, which buys from kestrel-alloys
    // (high risk). sources_from is not transitive: rhein does not buy from
    // kestrel, but it inherits kestrel's risk under the supplied tier rule.
    let o = sampled("supply-chain");
    o.read(|g, schema| {
        let c = reason::closure(schema, g, &ReasonOptions::default(), &|_| true);
        let fact = |s: &str, p: &str, o: &str| Fact::new(format!("<{s}>"), p, format!("<{o}>"));
        assert!(!schema.link_types["sources_from"].transitive);
        assert_eq!(c.contains(&fact("supplier:rhein-bearings", "sources_from", "supplier:kestrel-alloys")), None);
        assert!(c.derived().iter().all(|d| d.conclusion.p != "sources_from"), "nothing derives a purchase relation");
        let direct = fact("supplier:lumen-metals", "inherits_supply_risk_from", "supplier:kestrel-alloys");
        assert_eq!(c.derivation(&direct).unwrap().rule, "schema/upstream_supply_risk");
        let tier2 = fact("supplier:rhein-bearings", "inherits_supply_risk_from", "supplier:kestrel-alloys");
        assert_eq!(c.derivation(&tier2).unwrap().rule, "schema/upstream_supply_risk_tiers");
        assert!(!c.rules().iter().find(|r| r.name == "schema/upstream_supply_risk_tiers").unwrap().is_builtin());
    });
}

#[test]
fn retail_promotions_cover_skus_directly_and_down_the_category_hierarchy() {
    let o = sampled("retail");
    o.read(|g, schema| {
        let c = reason::closure(schema, g, &ReasonOptions::default(), &|_| true);
        let covers = |promo: &str, sku: &str| {
            let f = Fact::new(format!("<promotion:{promo}>"), "covers_sku", format!("<sku:{sku}>"));
            c.derivation(&f).map(|d| d.rule.clone())
        };
        assert_eq!(covers("cracker-clearance", "oat-crackers").as_deref(), Some("schema/promotion_covers_sku"));
        assert_eq!(covers("snack-week", "trail-mix-500").as_deref(), Some("schema/promotion_covers_category"));
        // grocery is the parent of snacks and beverages, and snacks of snack bars.
        for sku in ["trail-mix-500", "granola-bar-6", "sparkling-water-12", "cold-brew-4"] {
            assert_eq!(covers("pantry-days", sku).as_deref(), Some("schema/promotion_covers_subcategories"), "{sku}");
        }
        assert_eq!(covers("snack-week", "granola-bar-6").as_deref(), Some("schema/promotion_covers_subcategories"));
        assert_eq!(covers("snack-week", "cold-brew-4"), None, "a category covers only its own subtree");
        // snack-week promotes the snacks category only; the stockout is still found.
        let stockout = Fact::new("<stock_position:0142-trail-mix>", "a", "#promoted_stockout");
        assert!(c.contains(&stockout).is_some(), "a category promotion makes a promoted stockout");
        // Ended and scheduled promotions do not.
        let cold = Fact::new("<stock_position:0217-cold-brew>", "a", "#promoted_stockout");
        assert_eq!(c.contains(&cold), None);
    });
}

// ── the one deliberate shape violation ───────────────────────────────────

/// A shape result: (shape, constraint id, component, focus).
type Violation = (&'static str, &'static str, &'static str, &'static str);

/// Each sample's one shape result.
const VIOLATIONS: &[(&str, Violation)] = &[
    ("manufacturing", ("production_order_shape", "production_order_shape/1", "min_count", "production_order:po-1004")),
    ("supply-chain", ("supplier_shape", "supplier_shape/0", "min_count", "supplier:northwind-distribution")),
    ("retail", ("stock_position_shape", "stock_position_shape/2", "min_value", "stock_position:0217-cold-brew")),
    ("logistics", ("handling_unit_shape", "handling_unit_shape/0", "pattern", "handling_unit:pal-2")),
    (
        "healthcare",
        ("medication_order_complete", "medication_order_complete/prescriber", "min_count", "medication_order:mo-1003"),
    ),
    (
        "finance",
        ("legal_entity_identified", "legal_entity_identified/lei", "pattern", "legal_entity:northwind-holdings"),
    ),
    ("insurance", ("claim_complete", "claim_complete/loss_date", "min_count", "claim:clm-20260006")),
    ("energy", ("smart_meter_installed", "smart_meter_installed/0", "min_count", "smart_meter:m-stock-0001")),
    ("telecom", ("transport_endpoints", "transport_endpoints/0", "min_count", "transport_link:mw-hop-77")),
    ("saas", ("nps_scale", "nps_scale/1", "max_value", "nps_response:nps-0007")),
];

/// The edit that repairs a sample's deliberate violation.
fn repair(id: &str) -> Vec<Mutation> {
    let patch = |target: &str, prop: &str, v: Value| {
        Mutation::Upsert(EntityInput {
            id: Some(target.into()),
            props: Map::from_iter([(prop.to_string(), v)]),
            ..Default::default()
        })
    };
    let link = |from: &str, rel: &str, to: &str| Mutation::Link(LinkInput::new(from, rel, to));
    match id {
        "manufacturing" => vec![link("production_order:po-1004", "scheduled_on", "production_line:us01-l1")],
        "supply-chain" => vec![
            Mutation::Upsert(
                EntityInput::new("supplier_site", "northwind-reno", "Northwind Distribution, Reno warehouse")
                    .link("site_in_region", "sourcing_region:de"),
            ),
            link("supplier:northwind-distribution", "operates_site", "supplier_site:northwind-reno"),
        ],
        "retail" => vec![patch("stock_position:0217-cold-brew", "on_hand", json!(0))],
        "logistics" => vec![patch("handling_unit:pal-2", "sscc", json!("387123450000000012"))],
        "healthcare" => vec![link("medication_order:mo-1003", "prescribed_by", "clinician:dr-okafor")],
        "finance" => vec![patch("legal_entity:northwind-holdings", "lei", json!("213800ABCDEF12345678"))],
        "insurance" => vec![patch("claim:clm-20260006", "loss_date", json!("2026-10-19"))],
        "energy" => vec![link("smart_meter:m-stock-0001", "meter_at", "service_point:sp-1005")],
        "telecom" => vec![link("transport_link:mw-hop-77", "terminates_on", "network_element:agg-sw-7")],
        "saas" => vec![patch("nps_response:nps-0007", "nps_score", json!(10))],
        other => panic!("no repair for {other}"),
    }
}

#[test]
fn each_sample_violates_exactly_one_shape_constraint() {
    let q = ShapesQuery::default();
    for id in ids() {
        let o = sampled(id);
        let rep = o.read(|g, schema| shapes::validate(schema, g, None, &q, &|_| true));
        assert_eq!(rep.verdict, ShapeVerdict::Violates, "{id}: {rep:#?}");
        assert!(rep.skipped.iter().all(|s| s.is_deactivation()), "{id}: skipped {:?}", rep.skipped);
        assert!(rep.unmatched_targets.is_empty(), "{id}: shapes that select nothing {:?}", rep.unmatched_targets);
        for shape in names_of(&layer(id), "shapes") {
            assert!(rep.focus_by_shape.get(&shape).is_some_and(|n| *n > 0), "{id}: {shape} selects nothing");
        }
        assert_eq!(rep.total_results, 1, "{id}: {:#?}", rep.results);
        let r = &rep.results[0];
        let (_, (shape, constraint, component, focus)) =
            VIOLATIONS.iter().find(|(p, _)| *p == id).unwrap_or_else(|| panic!("{id}: no expected violation"));
        assert_eq!((r.shape.as_str(), r.component.as_str(), r.focus.as_str()), (*shape, *component, *focus), "{id}");
        assert_eq!(r.constraint_id, *constraint, "{id}");
        // The note names the violation exactly: focus, shape and component.
        let note = packs::find(id).unwrap().meta.sample_note;
        for word in [focus, shape, component] {
            assert!(note.contains(word), "{id}: the sample note names {word}: {note}");
        }
        // Repairing it makes the sample conform, so it is the only one.
        o.apply(&ctx(), repair(id)).unwrap_or_else(|e| panic!("{id} repair: {e}"));
        let after = o.read(|g, schema| shapes::validate(schema, g, None, &q, &|_| true));
        assert_eq!(after.verdict, ShapeVerdict::Conforms, "{id}: {:#?}", after.results);
    }
}

// ── action types ─────────────────────────────────────────────────────────

/// A governed edit on a pack's sample, and what must hold after it.
struct ActionCase {
    pack: &'static str,
    action: &'static str,
    params: Value,
    /// The entity the action edits, or `new <kind>` for one it creates.
    entity: &'static str,
    state_after: Option<&'static str>,
    link_after: Option<(&'static str, &'static str)>,
    /// Properties the entity carries after the action.
    props_after: &'static [&'static str],
    /// (relation, state): what the entity reaches by the relation is in that state.
    linked_state_after: Option<(&'static str, &'static str)>,
}

impl ActionCase {
    fn new(pack: &'static str, action: &'static str, params: Value, entity: &'static str) -> Self {
        ActionCase {
            pack,
            action,
            params,
            entity,
            state_after: None,
            link_after: None,
            props_after: &[],
            linked_state_after: None,
        }
    }
    fn state(mut self, s: &'static str) -> Self {
        self.state_after = Some(s);
        self
    }
    fn link(mut self, rel: &'static str, to: &'static str) -> Self {
        self.link_after = Some((rel, to));
        self
    }
    fn props(mut self, props: &'static [&'static str]) -> Self {
        self.props_after = props;
        self
    }
    fn linked_state(mut self, rel: &'static str, state: &'static str) -> Self {
        self.linked_state_after = Some((rel, state));
        self
    }
}

/// Every action type of every pack, in the order they run on one sample
/// (later cases may build on earlier ones: file_sar needs an escalated alert).
fn action_cases() -> Vec<ActionCase> {
    use ActionCase as A;
    vec![
        A::new(
            "manufacturing",
            "release_production_order",
            json!({"order": "production_order:po-1004", "line": "production_line:us01-l1"}),
            "production_order:po-1004",
        )
        .state("released")
        .link("scheduled_on", "production_line:us01-l1"),
        A::new(
            "manufacturing",
            "quarantine_lot",
            json!({"lot": "production_lot:l-2420", "reason": "suspect input lot"}),
            "production_lot:l-2420",
        )
        .state("quarantined"),
        A::new(
            "manufacturing",
            "raise_nonconformance",
            json!({"title": "Burr on housing", "against": "production_lot:l-2420", "severity": "minor"}),
            "new nonconformance",
        )
        .state("open")
        .link("raised_against", "production_lot:l-2420"),
        A::new(
            "supply-chain",
            "suspend_supplier",
            json!({"supplier": "supplier:kestrel-alloys", "reason": "sanctioned region"}),
            "supplier:kestrel-alloys",
        )
        .state("suspended"),
        A::new("supply-chain", "issue_purchase_order", json!({"po": "purchase_order:po-45003"}), "purchase_order:po-45003")
            .state("issued"),
        A::new(
            "supply-chain",
            "record_supply_disruption",
            json!({"title": "Strike", "site": "supplier_site:rhein-stuttgart", "cause": "labor", "impact": "medium"}),
            "new supply_disruption",
        )
        .state("reported")
        .link("disrupts", "supplier_site:rhein-stuttgart"),
        A::new("retail", "launch_promotion", json!({"promotion": "promotion:summer-drinks"}), "promotion:summer-drinks")
            .state("live"),
        A::new(
            "retail",
            "pull_recalled_sku",
            json!({"sku": "sku:sparkling-water-12", "recall": "product_recall:rc-2025-03"}),
            "sku:sparkling-water-12",
        )
        .state("recalled"),
        A::new(
            "logistics",
            "report_delivery_exception",
            json!({"shipment": "shipment:sh-88003", "title": "Customs hold", "exception_type": "customs_hold"}),
            "new delivery_exception",
        )
        .state("open")
        .link("exception_on", "shipment:sh-88003"),
        A::new("logistics", "confirm_delivery", json!({"shipment": "shipment:sh-88002"}), "shipment:sh-88002")
            .state("delivered"),
        A::new(
            "healthcare",
            "discharge_patient",
            json!({"encounter": "encounter:enc-5001", "note": "Stable on oral diuretics; GP review in 7 days."}),
            "encounter:enc-5001",
        )
        .state("finished")
        .props(&["discharge_note", "discharged_at"]),
        A::new(
            "healthcare",
            "hold_medication_order",
            json!({"order": "medication_order:mo-1002", "reason": "Penicillin allergy on record."}),
            "medication_order:mo-1002",
        )
        .state("on_hold"),
        A::new(
            "healthcare",
            "assign_primary_clinician",
            json!({"patient": "patient:mrn-100233", "clinician": "clinician:dr-lindqvist"}),
            "patient:mrn-100233",
        )
        .link("primary_clinician", "clinician:dr-lindqvist"),
        A::new(
            "finance",
            "freeze_bank_account",
            json!({"account": "bank_account:xa-kestrel", "reason": "Holder controlled by a designated person."}),
            "bank_account:xa-kestrel",
        )
        .state("frozen"),
        A::new(
            "finance",
            "escalate_aml_alert",
            json!({"alert": "aml_alert:al-902", "analyst": "person:ana-ruiz", "note": "Beneficiary is sanctioned."}),
            "aml_alert:al-902",
        )
        .state("escalated")
        .link("investigated_by", "person:ana-ruiz"),
        A::new(
            "finance",
            "file_sar",
            json!({
                "alert": "aml_alert:al-902",
                "title": "SAR: payment to an entity controlled by a designated person",
                "narrative": "Maria Lopez attempted a GBP 4,900 payment to Kestrel Logistics LLC."
            }),
            "aml_alert:al-902",
        )
        .state("closed_reported")
        // The alert is closed as reported only with the report filed.
        .linked_state("reported_in_sar", "filed"),
        A::new(
            "insurance",
            "refer_claim_to_investigation",
            json!({"claim": "claim:clm-20260004", "investigator": "person:hana-kim", "note": "Claim on a lapsed policy."}),
            "claim:clm-20260004",
        )
        .state("under_investigation")
        .link("adjusted_by", "person:hana-kim"),
        A::new("insurance", "settle_claim", json!({"claim": "claim:clm-20260003", "amount": 39500.0}), "claim:clm-20260003")
            .state("paid"),
        A::new(
            "insurance",
            "cancel_insurance_policy",
            json!({"policy": "insurance_policy:pol-30000102", "reason": "Premises sold."}),
            "insurance_policy:pol-30000102",
        )
        .state("cancelled"),
        A::new(
            "energy",
            "dispatch_outage_crew",
            json!({"outage": "grid_outage:out-2026-0420", "crew": "team:grid-crew-north"}),
            "grid_outage:out-2026-0420",
        )
        .state("crew_dispatched"),
        A::new("energy", "restore_outage", json!({"outage": "grid_outage:out-2026-0412"}), "grid_outage:out-2026-0412")
            .state("restored"),
        A::new(
            "energy",
            "retire_transformer",
            json!({"transformer": "transformer:t-1202", "reason": "replaced by T-1203"}),
            "transformer:t-1202",
        )
        .state("retired"),
        A::new("telecom", "acknowledge_alarm", json!({"alarm": "network_alarm:alm-90002"}), "network_alarm:alm-90002")
            .state("acknowledged"),
        A::new(
            "telecom",
            "open_trouble_ticket",
            json!({"title": "SIP trunk 17 one-way audio", "service": "network_service:sip-trunk-17"}),
            "new trouble_ticket",
        )
        .state("open"),
        A::new(
            "telecom",
            "suspend_service",
            json!({"service": "network_service:mobile-data-metro", "reason": "fraud investigation"}),
            "network_service:mobile-data-metro",
        )
        .state("suspended"),
        A::new(
            "saas",
            "change_subscription_plan",
            json!({"subscription": "subscription:sub-1002", "plan": "pricing_plan:flowdesk-enterprise"}),
            "subscription:sub-1002",
        )
        .state("active")
        .link("on_plan", "pricing_plan:flowdesk-enterprise"),
        A::new(
            "saas",
            "cancel_subscription",
            json!({"subscription": "subscription:sub-1004", "reason": "moved to a competitor"}),
            "subscription:sub-1004",
        )
        .state("cancelled"),
        A::new(
            "saas",
            "escalate_support_ticket",
            json!({"ticket": "support_ticket:st-501", "project": "project:flowdesk-reliability", "title": "Reports time out"}),
            "new issue",
        )
        .state("open"),
    ]
}

#[test]
fn pack_action_types_plan_apply_and_undo_on_their_sample() {
    let cases = action_cases();
    for id in ids() {
        let declared: BTreeSet<String> = names_of(&layer(id), "action_types").into_iter().collect();
        let covered: BTreeSet<String> = cases.iter().filter(|c| c.pack == id).map(|c| c.action.into()).collect();
        assert_eq!(covered, declared, "{id}: every action type has a case");
    }
    let confirmed = ApplyOptions { dry_run: false, confirm: true };
    for id in ids() {
        let o = sampled(id);
        let mut reversible = 0;
        for c in cases.iter().filter(|c| c.pack == id) {
            let what = format!("{id}.{}", c.action);
            let p = params(&c.params);
            let entities =
                |o: &Ontology| -> BTreeSet<String> { o.read(|g, _| g.entities().map(|e| e.id.clone()).collect()) };
            let before_ids = entities(&o);
            let before = c.entity.strip_prefix("new ").is_none().then(|| o.get(&admin(), c.entity).unwrap());
            let plan = o.plan_action(&ctx(), c.action, &p).unwrap();
            assert!(plan.valid, "{what}: {:?}", plan.checks);
            let apply = || {
                let r = o.apply_action(&ctx(), c.action, &p, confirmed).unwrap_or_else(|e| panic!("{what}: {e}"));
                assert_eq!(r.status, "applied", "{what}: {:?} {:?}", r.reason, r.plan.checks);
                r
            };
            let receipt = apply();
            let target = match c.entity.strip_prefix("new ") {
                // The created entity: touched, of that kind, and not there before.
                Some(kind) => receipt
                    .touched
                    .keys()
                    .find(|k| k.starts_with(&format!("{kind}:")) && !before_ids.contains(*k))
                    .cloned()
                    .unwrap_or_else(|| panic!("{what}: no new {kind}: {:?}", receipt.touched)),
                None => c.entity.to_string(),
            };
            let e = o.get(&admin(), &target).unwrap();
            if let Some(s) = c.state_after {
                assert_eq!(e.state.as_deref(), Some(s), "{what}: {target}");
            }
            let reaches = |rel: &str| -> Vec<String> {
                o.read(|g, schema| {
                    let (base, dir) = schema.resolve_rel(rel).unwrap();
                    g.neighbors(&target, &base, dir).into_iter().map(String::from).collect()
                })
            };
            if let Some((rel, to)) = c.link_after {
                assert!(reaches(rel).iter().any(|x| x == to), "{what}: {target} {rel} {to}");
            }
            for prop in c.props_after {
                assert!(e.props.get(*prop).is_some_and(|v| !v.is_null()), "{what}: {target} has no {prop}");
            }
            if let Some((rel, state)) = c.linked_state_after {
                let others = reaches(rel);
                assert!(!others.is_empty(), "{what}: {target} has no {rel}");
                for other in others {
                    assert_eq!(o.get(&admin(), &other).unwrap().state.as_deref(), Some(state), "{what}: {other}");
                }
            }
            // An edit of an existing entity is guarded: it does not apply twice.
            if before.is_some() {
                assert!(!o.plan_action(&ctx(), c.action, &p).unwrap().valid, "{what}: applies twice");
            }
            // Undo restores what was there; the action then applies again so
            // later cases can build on it.
            if receipt.plan.reversible {
                o.undo_action(&ctx(), &receipt.id, false).unwrap_or_else(|e| panic!("{what} undo: {e}"));
                assert_eq!(entities(&o), before_ids, "{what}: undo restores the entity set");
                if let Some(before) = &before {
                    let undone = o.get(&admin(), c.entity).unwrap();
                    assert_eq!((&undone.state, &undone.props), (&before.state, &before.props), "{what}");
                    if let Some((rel, to)) = c.link_after {
                        assert!(!reaches(rel).iter().any(|x| x == to), "{what}: undo keeps {rel} {to}");
                    }
                }
                apply();
                reversible += 1;
            }
        }
        assert!(reversible > 0, "{id}: no action type is reversible");
    }
}

#[test]
fn manufacturing_release_refuses_a_released_order() {
    let o = sampled("manufacturing");
    let p = params(&json!({"order": "production_order:po-1002", "line": "production_line:us01-l1"}));
    assert!(!o.plan_action(&ctx(), "release_production_order", &p).unwrap().valid);
}

#[test]
fn energy_actions_refuse_closed_outages_unconfirmed_and_unauthorized_retirements() {
    let energy = sampled("energy");
    let run = |c: &ActionContext, action: &str, v: Value, opts: ApplyOptions| {
        energy.apply_action(c, action, &params(&v), opts).unwrap().status
    };
    let dispatch = json!({"outage": "grid_outage:out-2026-0413", "crew": "team:grid-crew-north"});
    assert_eq!(run(&ctx(), "dispatch_outage_crew", dispatch, ApplyOptions::default()), "rejected", "restored outage");
    let retire = json!({"transformer": "transformer:t-1202", "reason": "x"});
    let unconfirmed = run(&ctx(), "retire_transformer", retire.clone(), ApplyOptions::default());
    assert_ne!(unconfirmed, "applied", "a high-risk action needs confirmation");
    let member = ActionContext::new(Principal::member("bo", "acme"), "acme");
    let confirmed = ApplyOptions { confirm: true, ..Default::default() };
    assert_eq!(run(&member, "retire_transformer", retire, confirmed), "rejected", "retiring an asset is for admins");
}

#[test]
fn saas_plan_change_replaces_the_plan_and_escalation_links_back() {
    let saas = sampled("saas");
    let run = |action: &str, v: Value| saas.apply_action(&ctx(), action, &params(&v), ApplyOptions::default()).unwrap();
    run(
        "change_subscription_plan",
        json!({"subscription": "subscription:sub-1002", "plan": "pricing_plan:flowdesk-enterprise"}),
    );
    let plans = saas.read(|g, _| g.neighbors("subscription:sub-1002", "on_plan", Dir::Out).len());
    assert_eq!(plans, 1, "the plan link is replaced, not added");
    let r = run(
        "escalate_support_ticket",
        json!({"ticket": "support_ticket:st-501", "project": "project:flowdesk-reliability", "title": "Reports time out"}),
    );
    assert_eq!(r.status, "applied");
    let escalated = saas.read(|g, s| {
        g.neighbors("support_ticket:st-501", "escalated_as", Dir::Out)
            .into_iter()
            .filter(|id| g.get(id).is_some_and(|e| s.kind_is(&e.kind, "issue")))
            .count()
    });
    assert_eq!(escalated, 1, "the new issue is linked back to the ticket");
}

#[test]
fn enterprise_work_item_actions_apply_to_pack_issue_subtypes() {
    // (pack, ticket, assignee, blocker)
    let cases = [
        ("telecom", "trouble_ticket:tt-3301", "person:noc-engineer-ravi", "trouble_ticket:tt-3302"),
        ("saas", "support_ticket:st-501", "person:csm-tomas", "issue:flowdesk-4410"),
    ];
    for (id, ticket, assignee, blocker) in cases {
        let o = sampled(id);
        let state = |o: &Ontology| o.get(&admin(), ticket).unwrap().state;
        assert_eq!(state(&o).as_deref(), Some("open"), "{id}: {ticket} starts open");
        let run = |action: &str, input: Value| {
            let r = o
                .apply_action(&ctx(), action, &params(&input), ApplyOptions { confirm: true, ..Default::default() })
                .unwrap_or_else(|e| panic!("{id}.{action} on {ticket}: {e}"));
            assert_eq!(r.status, "applied", "{id}.{action} on {ticket}: {:?} {:?}", r.reason, r.plan.checks);
        };
        let reach = |rel: &str| -> Vec<String> {
            o.read(|g, _| g.neighbors(ticket, rel, Dir::Out).into_iter().map(String::from).collect())
        };
        run("assign", json!({"item": ticket, "assignee": assignee}));
        assert_eq!(state(&o).as_deref(), Some("triaged"), "{id}: assign triages {ticket}");
        assert_eq!(reach("assigned_to"), [assignee.to_string()], "{id}: assign replaces the assignee");
        run("block", json!({"item": ticket, "blocker": blocker}));
        assert_eq!(state(&o).as_deref(), Some("blocked"), "{id}: block sets {ticket} blocked");
        assert_eq!(reach("blocked_by"), [blocker.to_string()], "{id}: block links the blocker");
        run("resolve", json!({"issue": ticket}));
        assert_eq!(state(&o).as_deref(), Some("resolved"), "{id}: resolve resolves {ticket}");
    }
}

// ── metrics ──────────────────────────────────────────────────────────────

#[test]
fn pack_metrics_evaluate_on_their_sample() {
    for id in ids() {
        let o = sampled(id);
        let s = o.schema();
        for name in names_of(&layer(id), "metrics") {
            let q = MetricQuery { metric: Some(name.clone()), ..Default::default() };
            let r = o.metric(&admin(), &q).unwrap_or_else(|e| panic!("{id}.{name}: {e}"));
            if matches!(r.measure, Measure::Count | Measure::Sum) {
                assert!(r.count > 0 && r.value > 0.0 && !r.rows.is_empty(), "{id}.{name}: {r:?}");
            }
            if r.measure == Measure::LeadTime {
                // Lead time runs from the record's creation, so the sample
                // (created now) reports zero days; the unit is what is pinned.
                assert_eq!(r.unit, "days", "{id}.{name}");
                assert!(r.value >= 0.0, "{id}.{name}: {r:?}");
            }
            for dim in s.metrics[&name].dimensions.keys() {
                let q = MetricQuery { metric: Some(name.clone()), group_by: vec![dim.clone()], ..Default::default() };
                o.metric(&admin(), &q).unwrap_or_else(|e| panic!("{id}.{name} by {dim}: {e}"));
            }
        }
    }
}

#[test]
fn energy_and_saas_metrics_report_their_sample_values() {
    let value = |id: &str, metric: &str| {
        let q = MetricQuery { metric: Some(metric.into()), ..Default::default() };
        sampled(id).metric(&admin(), &q).unwrap().value
    };
    assert_eq!(value("energy", "customers_interrupted"), 3.0);
    assert_eq!(value("saas", "monthly_recurring_revenue"), 12500.0 + 1600.0 + 1000.0 + 95.0);
}

// ── the marketplace ──────────────────────────────────────────────────────

#[test]
fn list_show_plan_install_and_revert_every_pack() {
    for id in ids() {
        let o = Ontology::in_memory(Schema::enterprise());
        let ctx = ctx();
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
        let ctx = ctx();
        let v0 = o.schema_version();
        let done = o.pack_install(&ctx, id, options(InstallMode::Apply, true)).unwrap();
        let lines = packs::sample(id).unwrap();
        assert_eq!(done.sample_entities, lines.len(), "{id}");
        assert_eq!(done.sample_links, lines.iter().map(|e| e.links.len()).sum::<usize>(), "{id}");
        assert_eq!(done.sample_links_skipped, 0, "{id}: nothing curated to keep");
        for e in &lines {
            let got = o.get(&admin(), &format!("{}:{}", e.kind, e.key.as_deref().unwrap())).unwrap();
            assert_eq!(got.workspace, "acme");
        }
        // The installed pack's rules fire on the installed sample.
        let stats = o.reason_stats(&admin(), &ReasonOptions::default()).unwrap();
        for rule in rule_names(id) {
            assert!(stats.by_rule.get(&rule).copied().unwrap_or(0) > 0, "{id}: {rule} after a live install");
        }
        // Loading the sample again changes nothing.
        let rev = o.revision();
        let again = o.pack_install(&ctx, id, options(InstallMode::Apply, true)).unwrap();
        assert!(again.already_installed && again.sample_entities == lines.len());
        assert_eq!(again.sample_links_skipped, 0, "{id}: the sample's own links are no-ops, not conflicts");
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
    let ctx = ctx();
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

#[test]
fn reloading_a_sample_keeps_curated_single_valued_links() {
    let o = Ontology::in_memory(Schema::enterprise());
    let ctx = ctx();
    o.pack_install(&ctx, "logistics", options(InstallMode::Apply, true)).unwrap();
    let carrier = |o: &Ontology| -> Vec<String> {
        o.read(|g, _| g.neighbors("shipment:sh-88001", "carried_by", Dir::Out).into_iter().map(String::from).collect())
    };
    assert_eq!(carrier(&o), vec!["carrier:blue-anchor".to_string()]);
    assert_eq!(o.schema().link_types["carried_by"].cardinality, Cardinality::One);
    // A curator reassigns the shipment.
    o.link(&ctx, "shipment:sh-88001", "carried_by", "carrier:nexa-freight").unwrap();
    assert_eq!(carrier(&o), vec!["carrier:nexa-freight".to_string()]);
    let rev = o.revision();
    let again = o.pack_install(&ctx, "logistics", options(InstallMode::Apply, true)).unwrap();
    assert!(again.already_installed);
    assert_eq!(again.sample_links_skipped, 1, "the sample's carrier is held back");
    assert_eq!(carrier(&o), vec!["carrier:nexa-freight".to_string()], "the curated link survives the reload");
    assert_eq!(o.revision(), rev, "the reload writes nothing");

    // The same holds for an entity curated before the sample first loads.
    let o = Ontology::in_memory(Schema::enterprise());
    o.pack_install(&ctx, "logistics", options(InstallMode::Apply, false)).unwrap();
    o.upsert(&ctx, EntityInput::new("carrier", "own-fleet", "Own fleet")).unwrap();
    o.upsert(
        &ctx,
        EntityInput::new("shipment", "sh-88001", "Curated SH-88001")
            .prop("tracking_number", "CUR-1")
            .link("carried_by", "carrier:own-fleet"),
    )
    .unwrap();
    let done = o.pack_install(&ctx, "logistics", options(InstallMode::Apply, true)).unwrap();
    assert_eq!(done.sample_links_skipped, 1);
    assert_eq!(carrier(&o), vec!["carrier:own-fleet".to_string()]);
    assert_eq!(o.get(&admin(), "shipment:sh-88001").unwrap().title, "Curated SH-88001");
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

    let out = o.review_proposal(&ctx(), &view.proposal.id, Decision::Approve, "ok", None);
    assert!(out.unwrap().applied.is_some());
    assert!(o.schema().object_types.contains_key("sku"));
    let listed = o.pack_list(&admin()).unwrap();
    assert!(listed.iter().find(|p| p.meta.id == "retail").unwrap().installed);

    // A sample the policy refuses during the dry run is audited too.
    let audited = |o: &Ontology| {
        o.events(&admin(), &EventFilter { action: Some("authorization.denied".into()), ..Default::default() })
            .unwrap()
            .into_iter()
            .filter(|e| e.actor == "mo" && e.payload["reason"].as_str().is_some_and(|r| r.contains("other")))
            .count()
    };
    let before = audited(&o);
    let err = o.pack_install(&ActionContext::new(member.clone(), "other"), "retail", options(InstallMode::Apply, true));
    let err = err.unwrap_err();
    assert!(matches!(err, Error::Denied(_)) && err.to_string().contains("does not load"), "{err}");
    assert_eq!(audited(&o), before + 1, "the cross-workspace sample load is audited");
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
fn propose_and_approve_installs_every_pack() {
    let agent = Principal::new("assistant", vec![Role::Agent], vec!["*".into()]);
    for id in ids() {
        let o = governed(Governance::default());
        let filed =
            o.pack_install(&ActionContext::new(agent.clone(), "acme"), id, options(InstallMode::Propose, false));
        let filed = filed.unwrap_or_else(|e| panic!("{id}: {e}"));
        let view = filed.proposal.unwrap_or_else(|| panic!("{id}: a proposal"));
        assert!(filed.version.is_none() && !filed.already_installed && filed.changes > 0, "{id}");
        assert_eq!(view.proposal.change_set.changes.len(), filed.changes, "{id}");
        assert!(!o.pack_list(&admin()).unwrap().iter().find(|p| p.meta.id == id).unwrap().installed, "{id}");
        let out = o
            .review_proposal(&ctx(), &view.proposal.id, Decision::Approve, "ok", None)
            .unwrap_or_else(|e| panic!("{id}: {e}"));
        assert!(out.applied.is_some(), "{id}: the approval applies it");
        assert!(o.pack_list(&admin()).unwrap().iter().find(|p| p.meta.id == id).unwrap().installed, "{id}");
        let diff = onto_core::live_schema::diff(&o.schema(), &with_pack(id));
        assert!(diff.is_empty(), "{id}: the approved schema equals enterprise + builtin:{id}: {diff:?}");
    }
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
    let ctx = ctx();
    for id in ids() {
        o.pack_install(&ctx, id, options(InstallMode::Apply, false)).unwrap();
    }
    assert_eq!(o.schema_version(), 1 + ids().len() as u64, "one schema version per pack");
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
    let ctx = ctx();
    let mut entities = BTreeSet::new();
    {
        let o = pg.open(Schema::enterprise(), OntologyOptions::default());
        for id in ids() {
            let done = o.pack_install(&ctx, id, options(InstallMode::Apply, true)).unwrap();
            assert!(done.version.is_some() && done.sample_entities > 0, "{id}");
            entities
                .extend(packs::sample(id).unwrap().iter().map(|e| format!("{}:{}", e.kind, e.key.clone().unwrap())));
        }
    }
    let o = pg.open(Schema::enterprise(), OntologyOptions::default());
    let mut configured = Schema::enterprise();
    for id in ids() {
        configured.extend_source(&format!("builtin:{id}"), |p: &str| std::path::PathBuf::from(p)).unwrap();
    }
    assert!(onto_core::live_schema::diff(&o.schema(), &configured).is_empty(), "the live schema is stored");
    assert_eq!(o.read(|g, _| g.entities().count()), entities.len());
    assert_eq!(o.get(&admin(), "shipment:sh-88001").unwrap().state.as_deref(), Some("in_transit"));
    let listed = o.pack_list(&admin()).unwrap();
    assert!(listed.iter().all(|p| p.installed), "{listed:?}");
    let again = o.pack_install(&ctx, "logistics", options(InstallMode::Apply, false)).unwrap();
    assert!(again.already_installed);
    let stats = o.reason_stats(&admin(), &ReasonOptions::default()).unwrap();
    for id in ids() {
        for rule in rule_names(id) {
            assert!(stats.by_rule.get(&rule).copied().unwrap_or(0) > 0, "{id}: {rule} after a reopen");
        }
    }
}
