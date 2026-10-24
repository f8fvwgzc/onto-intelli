//! Industry packs `healthcare`, `finance` (banking, KYC and AML) and
//! `insurance` (property and casualty).
//!
//! A pack is a TOML layer over `builtin:enterprise` with a `[pack]` table of
//! metadata, plus a sample of entities (one `EntityInput` per line). These
//! tests hold each pack to the pack guide: the layer compiles on enterprise
//! alone and next to every other pack, reuses enterprise instead of
//! redefining it, names nothing that enterprise or another pack already
//! names, and its sample applies under a strict schema, makes every supplied
//! rule derive at least one fact, contains exactly the one documented shape
//! violation, and carries the pack's action types and metrics.
//!
//! The shape evaluation here is a small reference evaluator for the subset of
//! shape components the packs use (it refuses anything else), so the sample
//! guarantee does not depend on the full shapes report.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use onto_core::action_types::ApplyOptions;
use onto_core::graph::Graph;
use onto_core::live_schema::changes_between;
use onto_core::metrics::{Measure, MetricQuery};
use onto_core::reason::{self, Fact, ReasonOptions, SUPPLIED_PREFIX};
use onto_core::schema::{PACKAGES, PropertyDef};
use onto_core::shape_model::{PropertyShape, ShapePath};
use onto_core::store::MemoryStore;
use onto_core::{
    ActionContext, Entity, EntityInput, Mutation, Ontology, OntologyOptions, Principal, Schema, SchemaChangeSet,
};

mod common;

// ── the packs under test ─────────────────────────────────────────────────────

/// One shape result: which shape and constraint, the component that failed
/// and the focus entity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ShapeHit {
    shape: String,
    constraint: String,
    component: &'static str,
    focus: String,
}

/// A governed edit run against the applied sample, and what must hold after.
struct ActionCase {
    action: &'static str,
    params: Value,
    entity: &'static str,
    state_after: Option<&'static str>,
    link_after: Option<(&'static str, &'static str)>,
}

struct PackCase {
    id: &'static str,
    toml: &'static str,
    sample: &'static str,
    /// The one deliberate shape violation: (shape, constraint id, component, focus).
    violation: (&'static str, &'static str, &'static str, &'static str),
    /// Facts (spelled) the closure of the sample must entail.
    entailed: &'static [[&'static str; 3]],
    /// Facts (spelled) the closure of the sample must not entail.
    not_entailed: &'static [[&'static str; 3]],
    /// Applied in order on one ontology holding the sample.
    actions: fn() -> Vec<ActionCase>,
}

const HEALTHCARE: PackCase = PackCase {
    id: "healthcare",
    toml: include_str!("../schemas/packs/healthcare.toml"),
    sample: include_str!("../schemas/packs/healthcare.sample.jsonl"),
    violation: (
        "medication_order_complete",
        "medication_order_complete/prescriber",
        "min_count",
        "medication_order:mo-1003",
    ),
    entailed: &[
        ["<patient:mrn-100231>", "treated_by_clinician", "<clinician:dr-okafor>"],
        ["<patient:mrn-100233>", "treated_by_clinician", "<clinician:dr-lindqvist>"],
        ["<patient:mrn-100231>", "receives_medication", "<medication:amoxicillin-500>"],
        ["<medication:amoxicillin-500>", "in_drug_class", "<drug_class:j01c>"],
        ["<medication_order:mo-1002>", "a", "#contraindicated_order"],
        ["<patient:mrn-100231>", "a", "#critical_result_patient"],
        ["<clinician:dr-okafor>", "notify_critical_result", "<patient:mrn-100231>"],
        ["<care_unit:st-anne-ccu>", "care_unit_within", "<care_facility:st-anne>"],
        ["<clinician:dr-okafor>", "a", "#person"],
        ["<encounter:enc-5001>", "a", "#clinical_record"],
    ],
    not_entailed: &[
        ["<medication_order:mo-1001>", "a", "#contraindicated_order"],
        ["<patient:mrn-100232>", "a", "#critical_result_patient"],
        ["<clinician:dr-lindqvist>", "notify_critical_result", "<patient:mrn-100233>"],
        ["<medication:metoprolol-50>", "in_drug_class", "<drug_class:j01c>"],
    ],
    actions: healthcare_actions,
};

fn healthcare_actions() -> Vec<ActionCase> {
    vec![
        ActionCase {
            action: "discharge_patient",
            params: json!({ "encounter": "encounter:enc-5001", "note": "Stable on oral diuretics; GP review in 7 days." }),
            entity: "encounter:enc-5001",
            state_after: Some("finished"),
            link_after: None,
        },
        ActionCase {
            action: "hold_medication_order",
            params: json!({ "order": "medication_order:mo-1002", "reason": "Penicillin allergy on record." }),
            entity: "medication_order:mo-1002",
            state_after: Some("on_hold"),
            link_after: None,
        },
        ActionCase {
            action: "assign_primary_clinician",
            params: json!({ "patient": "patient:mrn-100233", "clinician": "clinician:dr-lindqvist" }),
            entity: "patient:mrn-100233",
            state_after: None,
            link_after: Some(("primary_clinician", "clinician:dr-lindqvist")),
        },
    ]
}

const FINANCE: PackCase = PackCase {
    id: "finance",
    toml: include_str!("../schemas/packs/finance.toml"),
    sample: include_str!("../schemas/packs/finance.sample.jsonl"),
    violation: ("legal_entity_identified", "legal_entity_identified/lei", "pattern", "legal_entity:northwind-holdings"),
    entailed: &[
        ["<private_individual:maria-lopez>", "pays_party", "<private_individual:emil-brandt>"],
        ["<legal_entity:northwind-holdings>", "pays_party", "<legal_entity:kestrel-trading>"],
        ["<private_individual:viktor-hale>", "resident_in", "<jurisdiction:xa>"],
        ["<private_individual:viktor-hale>", "a", "#sanctioned_party"],
        ["<legal_entity:kestrel-trading>", "a", "#sanctioned_party"],
        ["<legal_entity:kestrel-logistics>", "a", "#sanctioned_party"],
        ["<private_individual:viktor-hale>", "controls_entity", "<legal_entity:kestrel-logistics>"],
        ["<funds_transfer:ft-1002>", "pays_sanctioned_party", "<legal_entity:kestrel-trading>"],
        ["<funds_transfer:ft-1005>", "pays_sanctioned_party", "<legal_entity:kestrel-logistics>"],
        ["<private_individual:viktor-hale>", "a", "#enhanced_due_diligence_party"],
        ["<legal_entity:kestrel-trading>", "a", "#enhanced_due_diligence_party"],
        ["<private_individual:maria-lopez>", "connected_party", "<private_individual:emil-brandt>"],
        ["<private_individual:emil-brandt>", "connected_party", "<private_individual:maria-lopez>"],
    ],
    not_entailed: &[
        ["<private_individual:oskar-lind>", "a", "#sanctioned_party"],
        ["<legal_entity:northwind-shipping>", "a", "#sanctioned_party"],
        ["<funds_transfer:ft-1003>", "pays_sanctioned_party", "<legal_entity:northwind-shipping>"],
        ["<legal_entity:kestrel-logistics>", "a", "#enhanced_due_diligence_party"],
    ],
    actions: finance_actions,
};

fn finance_actions() -> Vec<ActionCase> {
    vec![
        ActionCase {
            action: "freeze_bank_account",
            params: json!({ "account": "bank_account:xa-kestrel", "reason": "Holder controlled by a designated person." }),
            entity: "bank_account:xa-kestrel",
            state_after: Some("frozen"),
            link_after: None,
        },
        ActionCase {
            action: "escalate_aml_alert",
            params: json!({ "alert": "aml_alert:al-902", "analyst": "person:ana-ruiz", "note": "Beneficiary is sanctioned." }),
            entity: "aml_alert:al-902",
            state_after: Some("escalated"),
            link_after: Some(("investigated_by", "person:ana-ruiz")),
        },
        ActionCase {
            action: "file_sar",
            params: json!({
                "alert": "aml_alert:al-902",
                "title": "SAR: payment to an entity controlled by a designated person",
                "narrative": "Maria Lopez attempted a GBP 4,900 payment to Kestrel Logistics LLC."
            }),
            entity: "aml_alert:al-902",
            state_after: Some("closed_reported"),
            link_after: None,
        },
    ]
}

const INSURANCE: PackCase = PackCase {
    id: "insurance",
    toml: include_str!("../schemas/packs/insurance.toml"),
    sample: include_str!("../schemas/packs/insurance.sample.jsonl"),
    violation: ("claim_complete", "claim_complete/loss_date", "min_count", "claim:clm-20260006"),
    entailed: &[
        ["<insurance_policy:pol-30000101>", "policy_covers", "<peril:flood>"],
        ["<insurance_policy:pol-30000101>", "policy_covers", "<peril:storm-surge>"],
        ["<insurance_policy:pol-30000102>", "policy_covers", "<peril:storm-surge>"],
        ["<peril:storm-surge>", "sub_peril_of", "<peril:natural-catastrophe>"],
        ["<claim:clm-20260002>", "a", "#excluded_peril_claim"],
        ["<claim:clm-20260004>", "a", "#claim_on_lapsed_policy"],
        ["<claim:clm-20260001>", "a", "#duplicate_item_claim"],
        ["<claim:clm-20260002>", "a", "#duplicate_item_claim"],
        ["<loss_event:storm-ada-2026>", "recoverable_under", "<reinsurance_treaty:cat-xl-2026>"],
    ],
    not_entailed: &[
        ["<claim:clm-20260001>", "a", "#excluded_peril_claim"],
        ["<claim:clm-20260006>", "a", "#duplicate_item_claim"],
        ["<claim:clm-20260003>", "a", "#claim_on_lapsed_policy"],
        ["<loss_event:storm-ada-2026>", "recoverable_under", "<reinsurance_treaty:qs-motor-2026>"],
        ["<insurance_policy:pol-30000103>", "policy_covers", "<peril:flood>"],
    ],
    actions: insurance_actions,
};

fn insurance_actions() -> Vec<ActionCase> {
    vec![
        ActionCase {
            action: "refer_claim_to_investigation",
            params: json!({ "claim": "claim:clm-20260004", "investigator": "person:hana-kim", "note": "Claim on a lapsed policy." }),
            entity: "claim:clm-20260004",
            state_after: Some("under_investigation"),
            link_after: Some(("adjusted_by", "person:hana-kim")),
        },
        ActionCase {
            action: "settle_claim",
            params: json!({ "claim": "claim:clm-20260003", "amount": 39500.0 }),
            entity: "claim:clm-20260003",
            state_after: Some("paid"),
            link_after: None,
        },
        ActionCase {
            action: "cancel_insurance_policy",
            params: json!({ "policy": "insurance_policy:pol-30000102", "reason": "Premises sold." }),
            entity: "insurance_policy:pol-30000102",
            state_after: Some("cancelled"),
            link_after: None,
        },
    ]
}

const PACKS: [PackCase; 3] = [HEALTHCARE, FINANCE, INSURANCE];

// ── helpers ──────────────────────────────────────────────────────────────────

fn admin() -> Principal {
    Principal::admin("ada")
}

fn table(text: &str) -> toml::Table {
    text.parse::<toml::Table>().expect("pack TOML parses")
}

/// The keys of one top-level section of a TOML file.
fn keys(t: &toml::Table, section: &str) -> BTreeSet<String> {
    t.get(section).and_then(toml::Value::as_table).map(|s| s.keys().cloned().collect()).unwrap_or_default()
}

fn enterprise_plus(packs: &[&str]) -> Schema {
    let mut s = Schema::enterprise();
    for p in packs {
        s.extend_toml(p).unwrap_or_else(|e| panic!("pack does not compile on enterprise: {e}"));
    }
    s
}

fn sample_inputs(pack: &PackCase) -> Vec<EntityInput> {
    pack.sample
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| serde_json::from_str(l).unwrap_or_else(|e| panic!("{} sample line {}: {e}", pack.id, i + 1)))
        .collect()
}

/// A strict ontology (unknown kinds, relations and properties are refused)
/// holding enterprise + the pack and the applied sample.
fn world(pack: &PackCase) -> (Ontology, ActionContext) {
    let opts = OntologyOptions { strict: true, ..Default::default() };
    let o = Ontology::open(enterprise_plus(&[pack.toml]), Arc::new(MemoryStore::default()), opts).unwrap();
    let ctx = ActionContext::new(admin(), "acme");
    let muts = sample_inputs(pack).into_iter().map(Mutation::Upsert).collect();
    o.apply(&ctx, muts).unwrap_or_else(|e| panic!("{} sample does not apply: {e}", pack.id));
    (o, ctx)
}

fn fact(f: &[&str; 3]) -> Fact {
    Fact::new(f[0], f[1], f[2])
}

/// Spelled names a schema layer defines, by namespace: `kinds` (object
/// types, interfaces, derived classes), `relations` (link types, inverse
/// names, derived relations), `rules`, `shapes`, `metrics`, `action_types`;
/// and its glossary as (section, word) → target. Derived names that `known`
/// (or the layer itself) already defines are reuse, not definitions.
#[derive(Clone, Debug, Default)]
struct Names {
    spaces: BTreeMap<&'static str, BTreeSet<String>>,
    glossary: BTreeMap<(String, String), String>,
}

fn space<'a>(n: &'a mut Names, s: &'static str) -> &'a mut BTreeSet<String> {
    n.spaces.entry(s).or_default()
}

fn names_of(text: &str, known: &Names) -> Names {
    let t = table(text);
    let mut n = Names::default();
    for k in keys(&t, "object_types").into_iter().chain(keys(&t, "interfaces")) {
        space(&mut n, "kinds").insert(k);
    }
    if let Some(lts) = t.get("link_types").and_then(toml::Value::as_table) {
        for (name, lt) in lts {
            space(&mut n, "relations").insert(name.clone());
            if let Some(inv) = lt.get("inverse").and_then(toml::Value::as_str) {
                space(&mut n, "relations").insert(inv.to_string());
            }
        }
    }
    for s in ["shapes", "metrics", "action_types"] {
        let ks = keys(&t, s);
        n.spaces.entry(s).or_default().extend(ks);
    }
    let has = |n: &Names, s: &str, name: &str| {
        n.spaces.get(s).is_some_and(|x| x.contains(name)) || known.spaces.get(s).is_some_and(|x| x.contains(name))
    };
    let rules = t.get("rules").and_then(toml::Value::as_array).cloned().unwrap_or_default();
    for r in &rules {
        let name = r.get("name").and_then(toml::Value::as_str).expect("rule name").to_string();
        space(&mut n, "rules").insert(name);
        let then: Vec<&str> = r
            .get("then")
            .and_then(toml::Value::as_array)
            .map(|a| a.iter().filter_map(toml::Value::as_str).collect())
            .unwrap_or_default();
        let (spc, derived) = match (then.get(1), then.get(2), r.get("infer").and_then(toml::Value::as_str)) {
            (Some(&"a"), Some(class), _) if !class.starts_with('?') => ("kinds", class.to_string()),
            (Some(rel), _, _) if *rel != "a" => ("relations", rel.trim_start_matches('^').to_string()),
            (None, _, Some(infer)) => ("relations", infer.trim_start_matches('^').to_string()),
            _ => continue,
        };
        if !has(&n, spc, &derived) {
            space(&mut n, spc).insert(derived);
        }
    }
    if let Some(g) = t.get("glossary").and_then(toml::Value::as_table) {
        for (section, words) in g {
            for (word, target) in words.as_table().into_iter().flatten() {
                let target = target.as_str().unwrap_or_default().to_string();
                n.glossary.insert((section.clone(), word.clone()), target);
            }
        }
    }
    n
}

/// Every name `a` and `b` both define (per namespace) and every glossary
/// word they map to different targets.
fn collisions(a: &Names, b: &Names) -> Vec<String> {
    let mut out = Vec::new();
    for (space, names) in &a.spaces {
        if let Some(other) = b.spaces.get(space) {
            out.extend(names.intersection(other).map(|n| format!("{space}: {n}")));
        }
    }
    for (word, target) in &a.glossary {
        if let Some(t) = b.glossary.get(word)
            && t != target
        {
            out.push(format!("glossary.{}.{}: {target} vs {t}", word.0, word.1));
        }
    }
    out
}

/// Every pack TOML in `schemas/packs` (other builders' packs included once
/// they are merged), as (id, text).
fn packs_on_disk() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas/packs");
    let mut out: Vec<(String, String)> = std::fs::read_dir(&dir)
        .expect("schemas/packs exists")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .map(|p| {
            let id = p.file_stem().unwrap().to_string_lossy().to_string();
            (id, std::fs::read_to_string(&p).expect("read pack"))
        })
        .collect();
    out.sort();
    out
}

// ── reference shape evaluator (the subset the packs use) ─────────────────────

enum Val<'g> {
    Lit(Value),
    Node(&'g Entity),
}

fn path_values<'g>(schema: &Schema, g: &'g Graph, e: &'g Entity, p: &PropertyShape) -> Vec<Val<'g>> {
    match ShapePath::parse(&p.path) {
        ShapePath::Title => {
            (!e.title.is_empty()).then(|| Val::Lit(Value::String(e.title.clone()))).into_iter().collect()
        }
        ShapePath::State => e.state.clone().map(|s| Val::Lit(Value::String(s))).into_iter().collect(),
        ShapePath::Prop(name) => {
            e.props.get(&name).filter(|v| !v.is_null()).map(|v| Val::Lit(v.clone())).into_iter().collect()
        }
        ShapePath::Relations(steps) => {
            assert_eq!(steps.len(), 1, "{}: the reference evaluator walks single-step paths only", p.id);
            let (base, dir) = schema.resolve_rel(&steps[0]).expect("shape path relation resolves");
            let mut seen = BTreeSet::new();
            g.steps_via(&e.id, &base, dir)
                .into_iter()
                .filter(|s| seen.insert(s.other.to_string()))
                .filter_map(|s| g.get(s.other))
                .map(Val::Node)
                .collect()
        }
    }
}

fn evaluate_shapes(schema: &Schema, g: &Graph) -> (Vec<ShapeHit>, BTreeMap<String, usize>) {
    let mut hits = Vec::new();
    let mut focus_by_shape = BTreeMap::new();
    for (name, shape) in &schema.shapes {
        assert!(
            shape.target_ids.is_empty()
                && shape.target_subjects_of.is_none()
                && shape.target_objects_of.is_none()
                && !shape.closed
                && !shape.deactivated,
            "shape {name} uses a feature the reference evaluator does not implement"
        );
        let target = shape.target.as_deref().expect("shape has a target");
        let mut focus: Vec<&Entity> = g.entities().filter(|e| schema.kind_is(&e.kind, target)).collect();
        focus.sort_by(|a, b| a.id.cmp(&b.id));
        focus_by_shape.insert(name.clone(), focus.len());
        for e in focus {
            for p in &shape.properties {
                assert!(
                    p.unknown.is_empty() && p.node.is_none() && p.has_value.is_none(),
                    "{}: unsupported component for the reference evaluator",
                    p.id
                );
                let vals = path_values(schema, g, e, p);
                let mut hit = |component: &'static str| {
                    hits.push(ShapeHit {
                        shape: name.clone(),
                        constraint: p.id.clone(),
                        component,
                        focus: e.id.clone(),
                    })
                };
                if p.min_count.is_some_and(|n| vals.len() < n) {
                    hit("min_count");
                }
                if p.max_count.is_some_and(|n| vals.len() > n) {
                    hit("max_count");
                }
                for v in &vals {
                    match v {
                        Val::Node(t) => {
                            if p.class.as_deref().is_some_and(|c| !schema.kind_is(&t.kind, c)) {
                                hit("class");
                            }
                            assert!(p.datatype.is_none() && p.pattern.is_none(), "{}: literal checks on links", p.id);
                        }
                        Val::Lit(x) => {
                            assert!(p.class.is_none(), "{}: class on a literal path", p.id);
                            if let Some(ty) = p.datatype
                                && !(PropertyDef { ty, ..Default::default() }).accepts(x)
                            {
                                hit("datatype");
                            }
                            if !p.values.is_empty() && !p.values.contains(x) {
                                hit("in");
                            }
                            if let Some(pat) = &p.pattern {
                                let re = regex_lite::Regex::new(pat).expect("shape pattern compiles");
                                if !x.as_str().is_some_and(|s| re.is_match(s)) {
                                    hit("pattern");
                                }
                            }
                            let chars = x.as_str().map(|s| s.chars().count());
                            if p.min_length.is_some_and(|n| chars.is_none_or(|c| c < n)) {
                                hit("min_length");
                            }
                            if p.max_length.is_some_and(|n| chars.is_none_or(|c| c > n)) {
                                hit("max_length");
                            }
                            let num = x.as_f64();
                            if p.min_value.is_some_and(|m| num.is_none_or(|v| v < m)) {
                                hit("min_value");
                            }
                            if p.max_value.is_some_and(|m| num.is_none_or(|v| v > m)) {
                                hit("max_value");
                            }
                        }
                    }
                }
            }
        }
    }
    hits.sort();
    (hits, focus_by_shape)
}

// ── metadata and content ─────────────────────────────────────────────────────

#[test]
fn pack_metadata_is_complete_and_honest() {
    for pack in &PACKS {
        let t = table(pack.toml);
        assert!(
            t.get("name").is_none() && t.get("version").is_none(),
            "{}: top-level name/version overwrite the base",
            pack.id
        );
        let meta =
            t.get("pack").and_then(toml::Value::as_table).unwrap_or_else(|| panic!("{}: no [pack] table", pack.id));
        let s = |k: &str| meta.get(k).and_then(toml::Value::as_str).unwrap_or_default().to_string();
        assert_eq!(s("id"), pack.id);
        assert_eq!(s("version"), "1.0.0", "{}", pack.id);
        for k in ["title", "industry", "description", "sample_note"] {
            assert!(!s(k).trim().is_empty(), "{}: [pack].{k} is empty", pack.id);
        }
        let list = |k: &str| -> Vec<String> {
            meta.get(k)
                .and_then(toml::Value::as_array)
                .map(|a| a.iter().filter_map(toml::Value::as_str).map(str::to_string).collect())
                .unwrap_or_default()
        };
        assert_eq!(list("requires"), vec!["enterprise".to_string()], "{}", pack.id);
        let standards = list("standards");
        assert!(!standards.is_empty(), "{}: names no standards", pack.id);
        for st in &standards {
            let lower = st.to_lowercase();
            assert!(
                !["complian", "certified", "conformant", "conforms to"].iter().any(|w| lower.contains(w)),
                "{}: '{st}' claims compliance; say what vocabulary it borrows",
                pack.id
            );
        }
        // The sample note and the description point at the deliberate violation.
        let (shape, constraint, _, focus) = pack.violation;
        let note = s("sample_note");
        assert!(note.contains(shape) && note.contains(constraint) && note.contains(focus), "{}: {note}", pack.id);
        assert!(s("description").contains("deliberate shape violation"), "{}", pack.id);
        // `requires` holds: every object type of each required package is
        // present once the pack is layered, and the pack builds on them.
        let layered = enterprise_plus(&[pack.toml]);
        for req in list("requires") {
            let required = Schema::from_toml(onto_core::schema::builtin_package(&req).unwrap()).unwrap();
            let missing: Vec<&String> =
                required.object_types.keys().filter(|k| !layered.object_types.contains_key(*k)).collect();
            assert!(missing.is_empty(), "{}: requires {req} but lacks {missing:?}", pack.id);
        }
        let reused = ["person", "team", "knowledge"];
        assert!(
            layered.link_types.values().any(|lt| lt.to.iter().chain(&lt.from).any(|k| reused.contains(&k.as_str())))
                || layered.object_types.values().any(|t| t.extends.as_deref().is_some_and(|p| reused.contains(&p))),
            "{}: links to no enterprise type",
            pack.id
        );
    }
}

#[test]
fn pack_content_fits_the_guide() {
    let enterprise = table(onto_core::schema::ENTERPRISE_SCHEMA);
    let hex = regex_lite::Regex::new("^#[0-9a-f]{6}$").unwrap();
    for pack in &PACKS {
        let t = table(pack.toml);
        let schema = enterprise_plus(&[pack.toml]);
        let id = pack.id;
        // Reuse, never redefine: no section key of enterprise appears in the pack.
        for section in ["object_types", "link_types", "interfaces", "metrics", "action_types", "shapes"] {
            let redefined: Vec<String> = keys(&t, section).intersection(&keys(&enterprise, section)).cloned().collect();
            assert!(redefined.is_empty(), "{id}: redefines enterprise {section} {redefined:?}");
        }
        let kinds = keys(&t, "object_types");
        assert!((8..=14).contains(&kinds.len()), "{id}: {} object types", kinds.len());
        let mut labels = BTreeSet::new();
        for k in &kinds {
            let ty = &schema.object_types[k];
            assert!(!ty.description.is_empty() && !ty.icon.is_empty(), "{id}.{k}: description and icon");
            assert!(hex.is_match(&ty.color), "{id}.{k}: color {}", ty.color);
            assert!(labels.insert(ty.label.clone()) && !ty.label.is_empty(), "{id}.{k}: label {:?}", ty.label);
            for s in ty.terminal_states.iter().chain(&ty.alert_states) {
                assert!(ty.states.contains(s), "{id}.{k}: terminal/alert state {s} is not a state");
            }
            for (p, def) in &ty.properties {
                assert!(
                    def.ty != onto_core::schema::PropType::Enum || !def.values.is_empty(),
                    "{id}.{k}.{p}: enum values"
                );
            }
            let raw = &t["object_types"][k.as_str()];
            for (p, def) in raw.get("properties").and_then(toml::Value::as_table).into_iter().flatten() {
                assert!(def.get("type").is_some(), "{id}.{k}.{p}: every property declares its type");
            }
        }
        assert!(kinds.iter().any(|k| !schema.object_types[k].states.is_empty()), "{id}: no lifecycle states");
        let links = keys(&t, "link_types");
        assert!((10..=20).contains(&links.len()), "{id}: {} link types", links.len());
        for l in &links {
            let lt = &schema.link_types[l];
            assert!(!lt.description.is_empty(), "{id}.{l}: description");
            assert!(!lt.from.is_empty() && !lt.to.is_empty(), "{id}.{l}: from and to");
            assert!(lt.inverse.is_some() || lt.symmetric, "{id}.{l}: inverse");
            assert!(!(lt.transitive && lt.cardinality == onto_core::schema::Cardinality::One), "{id}.{l}");
        }
        assert!(links.iter().any(|l| schema.link_types[l].transitive), "{id}: no transitive relation");
        let glossary = t.get("glossary").and_then(toml::Value::as_table).expect("glossary");
        assert!(glossary.get("kinds").and_then(toml::Value::as_table).is_some_and(|k| k.len() >= 5), "{id}: glossary");
        let rules: Vec<_> =
            schema.rules.iter().filter(|r| !Schema::enterprise().rules.iter().any(|e| e.name == r.name)).collect();
        assert!((3..=6).contains(&rules.len()), "{id}: {} rules", rules.len());
        assert!(
            rules.iter().any(|r| r.is_horn()) && rules.iter().any(|r| !r.is_horn()),
            "{id}: chain and when/then rules"
        );
        assert!(rules.iter().all(|r| !r.description.is_empty()), "{id}: rule descriptions");
        let shapes = keys(&t, "shapes");
        assert!((3..=6).contains(&shapes.len()), "{id}: {} shapes", shapes.len());
        assert!(shapes.iter().all(|s| !schema.shapes[s].description.is_empty()), "{id}: shape descriptions");
        let metrics = keys(&t, "metrics");
        assert!((2..=4).contains(&metrics.len()), "{id}: {} metrics", metrics.len());
        assert!(metrics.iter().all(|m| !schema.metrics[m].description.is_empty()), "{id}: metric descriptions");
        let actions = keys(&t, "action_types");
        assert!((1..=3).contains(&actions.len()), "{id}: {} action types", actions.len());
        for a in &actions {
            let at = &schema.action_types[a];
            assert!(!at.description.is_empty() && !at.preconditions.is_empty() && !at.effects.is_empty(), "{id}.{a}");
            assert!(t["action_types"][a.as_str()].get("risk").is_some(), "{id}.{a}: declares its risk");
        }
    }
}

#[test]
fn packs_compile_on_enterprise_alone_and_together() {
    let enterprise = Schema::enterprise();
    let all: Vec<&str> = PACKS.iter().map(|p| p.toml).collect();
    let together = enterprise_plus(&all);
    let mut expect_types = enterprise.object_types.len();
    let mut expect_links = enterprise.link_types.len();
    let mut expect_rules = enterprise.rules.len();
    let mut expect_shapes = enterprise.shapes.len();
    for pack in &PACKS {
        let alone = enterprise_plus(&[pack.toml]);
        expect_types += alone.object_types.len() - enterprise.object_types.len();
        expect_links += alone.link_types.len() - enterprise.link_types.len();
        expect_rules += alone.rules.len() - enterprise.rules.len();
        expect_shapes += alone.shapes.len() - enterprise.shapes.len();
        // Layering keeps the base: same name, same version, enterprise rules intact.
        assert_eq!((alone.name.as_str(), alone.version), (enterprise.name.as_str(), enterprise.version));
        for r in &enterprise.rules {
            assert!(alone.rules.iter().any(|x| x.name == r.name && x.describe() == r.describe()), "{}", pack.id);
        }
        // The engine sees every pack rule (no refusals).
        let (lines, refused) = onto_core::reason::rule_table(&alone);
        assert!(refused.is_empty(), "{}: {refused:?}", pack.id);
        assert_eq!(lines.iter().filter(|l| !l.is_builtin()).count(), alone.rules.len());
    }
    // Installed together, nothing replaces anything.
    assert_eq!(together.object_types.len(), expect_types);
    assert_eq!(together.link_types.len(), expect_links);
    assert_eq!(together.rules.len(), expect_rules);
    assert_eq!(together.shapes.len(), expect_shapes);
    // Order does not matter.
    let reversed = enterprise_plus(&all.iter().rev().copied().collect::<Vec<_>>());
    assert_eq!(
        serde_json::to_value(&reversed.object_types).unwrap(),
        serde_json::to_value(&together.object_types).unwrap()
    );
    // Next to every other pack on disk (other builders' packs once merged).
    for (other, text) in packs_on_disk() {
        if PACKS.iter().any(|p| p.id == other) {
            continue;
        }
        let mut s = together.clone();
        s.extend_toml(&text).unwrap_or_else(|e| panic!("packs-b packs and {other} cannot be installed together: {e}"));
    }
}

#[test]
fn names_do_not_collide_with_enterprise_or_other_packs() {
    let enterprise_names = names_of(onto_core::schema::ENTERPRISE_SCHEMA, &Names::default());
    let mut base = enterprise_names.clone();
    let disk = packs_on_disk();
    for pack in &PACKS {
        assert!(disk.iter().any(|(id, _)| id == pack.id), "{} is not under schemas/packs", pack.id);
    }
    // Other built-in layers count as base vocabulary; packs registered in
    // PACKAGES later are checked as packs below.
    for (name, text) in PACKAGES {
        if *name == "enterprise" || disk.iter().any(|(id, _)| id == name) {
            continue;
        }
        let layer = names_of(text, &enterprise_names);
        for (space, names) in layer.spaces {
            base.spaces.entry(space).or_default().extend(names);
        }
        base.glossary.extend(layer.glossary);
    }
    let all: Vec<(String, Names)> =
        disk.iter().map(|(id, text)| (id.clone(), names_of(text, &enterprise_names))).collect();
    for pack in &PACKS {
        let mine = &all.iter().find(|(id, _)| id == pack.id).unwrap().1;
        let with_base = collisions(mine, &base);
        assert!(with_base.is_empty(), "{} collides with enterprise or a built-in layer: {with_base:?}", pack.id);
        for (other, names) in &all {
            if other == pack.id {
                continue;
            }
            let c = collisions(mine, names);
            assert!(c.is_empty(), "{} collides with pack {other}: {c:?}", pack.id);
        }
        // Glossary words are new words, not reassignments of enterprise words.
        for (section, word) in mine.glossary.keys() {
            assert!(
                !enterprise_names.glossary.contains_key(&(section.clone(), word.clone())),
                "{}: glossary.{section}.{word} is already an enterprise word",
                pack.id
            );
        }
    }
}

#[test]
fn collision_check_catches_reused_names() {
    let enterprise = names_of(onto_core::schema::ENTERPRISE_SCHEMA, &Names::default());
    let healthcare = names_of(HEALTHCARE.toml, &enterprise);
    let rogue = names_of(
        r#"
[object_types.patient]
description = "Same name, other meaning."

[link_types.cares_for]
from = ["patient"]
to = ["person"]
inverse = "for_patient"

[[rules]]
name = "critical_result"
chain = ["cares_for"]
infer = "treated_by_clinician"

[glossary.kinds]
hospital = "patient"
"#,
        &enterprise,
    );
    let found = collisions(&rogue, &healthcare);
    for expected in
        ["kinds: patient", "relations: for_patient", "rules: critical_result", "relations: treated_by_clinician"]
    {
        assert!(found.iter().any(|f| f == expected), "missing {expected} in {found:?}");
    }
    assert!(found.iter().any(|f| f.starts_with("glossary.kinds.hospital")), "{found:?}");
    // Reusing an enterprise relation as a rule head is reuse, not a definition.
    let reuse = names_of("[[rules]]\nname = \"x\"\nchain = [\"owns\"]\ninfer = \"owning_team\"\n", &enterprise);
    assert!(reuse.spaces.get("relations").is_none_or(|r| r.is_empty()), "{:?}", reuse.spaces);
}

#[test]
fn glossary_words_resolve_to_their_targets() {
    for pack in &PACKS {
        let schema = enterprise_plus(&[pack.toml]);
        let own = names_of(pack.toml, &Names::default());
        for ((section, word), target) in &own.glossary {
            match section.as_str() {
                "kinds" => {
                    assert_eq!(schema.canonical_kind(word).as_deref(), Some(target.as_str()), "{}: {word}", pack.id)
                }
                "relations" => {
                    assert_eq!(schema.canonical_rel(word).as_deref(), Some(target.as_str()), "{}: {word}", pack.id)
                }
                _ => {}
            }
        }
    }
}

// ── samples ──────────────────────────────────────────────────────────────────

#[test]
fn samples_apply_cleanly_under_a_strict_schema() {
    for pack in &PACKS {
        let lines: Vec<&str> = pack.sample.lines().filter(|l| !l.trim().is_empty()).collect();
        assert!((25..=60).contains(&lines.len()), "{}: {} sample lines", pack.id, lines.len());
        for (i, l) in lines.iter().enumerate() {
            let v: Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("{} line {}: {e}", pack.id, i + 1));
            let obj = v.as_object().expect("one JSON object per line");
            assert!(
                obj.get("workspace").is_none() && obj.get("id").is_none(),
                "{} line {}: workspace-agnostic",
                pack.id,
                i + 1
            );
            for k in ["kind", "key", "title"] {
                assert!(
                    obj.get(k).and_then(Value::as_str).is_some_and(|s| !s.is_empty()),
                    "{} line {}: {k}",
                    pack.id,
                    i + 1
                );
            }
        }
        let (o, _) = world(pack);
        let t = table(pack.toml);
        o.read(|g, schema| {
            assert_eq!(g.entities().count(), lines.len(), "{}: one entity per line", pack.id);
            // Every pack type has an entity and every pack relation a link.
            for k in keys(&t, "object_types") {
                assert!(g.entities().any(|e| e.kind == k), "{}: no sample {k}", pack.id);
            }
            for l in keys(&t, "link_types") {
                assert!(g.links().any(|x| x.rel == l), "{}: no sample {l} link", pack.id);
            }
            // Every stored link fits its declared endpoints.
            for l in g.links() {
                let (from, to) = (g.get(&l.from).unwrap(), g.get(&l.to).unwrap());
                schema.validate_link(&from.kind, &l.rel, &to.kind, true).unwrap();
            }
        });
    }
}

#[test]
fn every_supplied_rule_fires_on_its_sample() {
    for pack in &PACKS {
        let (o, _) = world(pack);
        // The pure reasoner over the stored graph, everything visible.
        let closure = o.read(|g, schema| reason::closure(schema, g, &ReasonOptions::default(), &|_| true));
        let stats = closure.stats();
        // The engine wrapper (admin scope) agrees.
        let wrapped = o.reason(&admin(), &ReasonOptions::default()).unwrap();
        assert_eq!(wrapped.derived().len(), closure.derived().len(), "{}", pack.id);
        assert!(stats.fixpoint_reached, "{}: {:?}", pack.id, stats.truncated);
        assert_eq!(stats.refused, 0, "{}: {:?}", pack.id, stats.refusals);
        let t = table(pack.toml);
        let rules = t.get("rules").and_then(toml::Value::as_array).expect("rules");
        for r in rules {
            let name = r.get("name").and_then(toml::Value::as_str).unwrap();
            let n = stats.by_rule.get(&format!("{SUPPLIED_PREFIX}{name}")).copied().unwrap_or(0);
            assert!(n > 0, "{}: rule {name} derives nothing from the sample", pack.id);
        }
        for f in pack.entailed {
            assert!(closure.contains(&fact(f)).is_some(), "{}: expected {}", pack.id, fact(f).tsv());
        }
        for f in pack.not_entailed {
            assert!(closure.contains(&fact(f)).is_none(), "{}: did not expect {}", pack.id, fact(f).tsv());
        }
        // The run yields a certificate (no term had to be refused).
        let cert = o.reason_certificate(&admin(), &ReasonOptions::default()).unwrap();
        assert!(cert.rules_tsv.lines().any(|l| l.starts_with(SUPPLIED_PREFIX)), "{}", pack.id);
        assert_eq!(cert.derivations_tsv.lines().count(), stats.derived, "{}", pack.id);
    }
}

#[test]
fn samples_hold_exactly_the_documented_shape_violation() {
    for pack in &PACKS {
        let (o, _) = world(pack);
        let (hits, focus) = o.read(|g, schema| evaluate_shapes(schema, g));
        let (shape, constraint, component, target) = pack.violation;
        let expected = ShapeHit { shape: shape.into(), constraint: constraint.into(), component, focus: target.into() };
        assert_eq!(hits, vec![expected], "{}: shape results of the sample", pack.id);
        // Every pack shape selects something: an empty target is undetermined, not conforming.
        for s in keys(&table(pack.toml), "shapes") {
            assert!(focus.get(&s).is_some_and(|n| *n > 0), "{}: shape {s} selects nothing in the sample", pack.id);
        }
        // Repairing the one violation makes the sample conform.
        let ctx = ActionContext::new(admin(), "acme");
        let repair = match pack.id {
            "healthcare" => {
                vec![Mutation::Link(onto_core::LinkInput::new(target, "prescribed_by", "clinician:dr-okafor"))]
            }
            "finance" => vec![Mutation::Upsert(EntityInput {
                id: Some(target.into()),
                props: Map::from_iter([("lei".to_string(), json!("213800ABCDEF12345678"))]),
                ..Default::default()
            })],
            "insurance" => vec![Mutation::Upsert(EntityInput {
                id: Some(target.into()),
                props: Map::from_iter([("loss_date".to_string(), json!("2026-10-19"))]),
                ..Default::default()
            })],
            other => panic!("no repair for {other}"),
        };
        o.apply(&ctx, repair).unwrap();
        let (after, _) = o.read(|g, schema| evaluate_shapes(schema, g));
        assert!(after.is_empty(), "{}: still {after:?}", pack.id);
    }
}

// ── action types and metrics ─────────────────────────────────────────────────

#[test]
fn action_types_plan_apply_and_undo_on_the_sample() {
    for pack in &PACKS {
        let (o, ctx) = world(pack);
        let cases = (pack.actions)();
        let declared = keys(&table(pack.toml), "action_types");
        let covered: BTreeSet<String> = cases.iter().map(|c| c.action.to_string()).collect();
        assert_eq!(covered, declared, "{}: every action type has a case", pack.id);
        let mut reverted = 0;
        for c in &cases {
            let params: Map<String, Value> = c.params.as_object().unwrap().clone();
            let before = o.get(&admin(), c.entity).unwrap();
            let plan = o.plan_action(&ctx, c.action, &params).unwrap();
            assert!(plan.valid, "{}.{}: {:?}", pack.id, c.action, plan.checks);
            let apply = || {
                let r = o
                    .apply_action(&ctx, c.action, &params, ApplyOptions { dry_run: false, confirm: true })
                    .unwrap_or_else(|e| panic!("{}.{}: {e}", pack.id, c.action));
                assert_eq!(r.status, "applied", "{}.{}", pack.id, c.action);
                r
            };
            let receipt = apply();
            let e = o.get(&admin(), c.entity).unwrap();
            if let Some(s) = c.state_after {
                assert_eq!(e.state.as_deref(), Some(s), "{}.{}", pack.id, c.action);
            }
            let linked = |rel: &str, to: &str| {
                o.read(|g, schema| {
                    let (base, dir) = schema.resolve_rel(rel).unwrap();
                    g.steps_via(c.entity, &base, dir).iter().any(|s| s.other == to)
                })
            };
            if let Some((rel, to)) = c.link_after {
                assert!(linked(rel, to), "{}.{}: {} {rel} {to}", pack.id, c.action, c.entity);
            }
            // The precondition now refuses the same edit.
            let again = o.plan_action(&ctx, c.action, &params).unwrap();
            assert!(!again.valid, "{}.{}: applies twice", pack.id, c.action);
            // Undo restores the entity; the action then applies again (later
            // cases build on it, as file_sar builds on escalate_aml_alert).
            if receipt.plan.reversible {
                o.undo_action(&ctx, &receipt.id, false)
                    .unwrap_or_else(|e| panic!("{}.{} undo: {e}", pack.id, c.action));
                let undone = o.get(&admin(), c.entity).unwrap();
                assert_eq!((&undone.state, &undone.props), (&before.state, &before.props), "{}.{}", pack.id, c.action);
                if let Some((rel, to)) = c.link_after {
                    assert!(!linked(rel, to), "{}.{}: undo keeps {rel} {to}", pack.id, c.action);
                }
                apply();
                reverted += 1;
            }
        }
        assert!(reverted > 0, "{}: no action type is reversible", pack.id);
    }
}

#[test]
fn metrics_evaluate_on_the_sample() {
    for pack in &PACKS {
        let (o, _) = world(pack);
        for name in keys(&table(pack.toml), "metrics") {
            let q = MetricQuery { metric: Some(name.clone()), ..Default::default() };
            let r = o.metric(&admin(), &q).unwrap_or_else(|e| panic!("{}.{name}: {e}", pack.id));
            if matches!(r.measure, Measure::Count | Measure::Sum) {
                assert!(r.count > 0 && r.value > 0.0, "{}.{name}: {r:?}", pack.id);
                assert!(!r.rows.is_empty(), "{}.{name}: no rows", pack.id);
            }
        }
    }
}

// ── live installation ────────────────────────────────────────────────────────

fn install_set(current: &Schema, pack: &PackCase) -> SchemaChangeSet {
    let mut target = current.clone();
    target.extend_toml(pack.toml).unwrap();
    SchemaChangeSet {
        changes: changes_between(current, &target),
        message: format!("install pack {}", pack.id),
        ..Default::default()
    }
}

fn install_and_load(o: &Ontology, pack: &PackCase) {
    let ctx = ActionContext::new(admin(), "acme");
    let set = install_set(&o.schema(), pack);
    assert!(!set.changes.is_empty());
    let plan = o.schema_plan(&admin(), &set).unwrap();
    assert!(plan.valid, "{}: {:?}", pack.id, plan.errors);
    assert!(!plan.breaking, "{}: installing a pack breaks nothing", pack.id);
    o.schema_apply(&ctx, set).unwrap_or_else(|e| panic!("{} live install: {e}", pack.id));
    // The live schema now equals enterprise + pack, so a re-install is a no-op.
    assert!(install_set(&o.schema(), pack).changes.is_empty(), "{}: re-install changes something", pack.id);
    let muts = sample_inputs(pack).into_iter().map(Mutation::Upsert).collect();
    o.apply(&ctx, muts).unwrap_or_else(|e| panic!("{} sample after live install: {e}", pack.id));
}

#[test]
fn packs_install_live_through_a_schema_change_set() {
    for pack in &PACKS {
        let o = Ontology::in_memory(Schema::enterprise());
        install_and_load(&o, pack);
        let expected = enterprise_plus(&[pack.toml]);
        let live = o.schema();
        assert_eq!(
            live.object_types.keys().collect::<Vec<_>>(),
            expected.object_types.keys().collect::<Vec<_>>(),
            "{}",
            pack.id
        );
        assert_eq!(live.shapes.len(), expected.shapes.len());
        assert_eq!(live.rules.len(), expected.rules.len());
        let stats = o.reason_stats(&admin(), &ReasonOptions::default()).unwrap();
        for r in expected.rules.iter().filter(|r| !Schema::enterprise().rules.iter().any(|e| e.name == r.name)) {
            assert!(stats.by_rule[&format!("{SUPPLIED_PREFIX}{}", r.name)] > 0, "{}: {}", pack.id, r.name);
        }
    }
    // All three, one after another, on one ontology.
    let o = Ontology::in_memory(Schema::enterprise());
    for pack in &PACKS {
        install_and_load(&o, pack);
    }
    assert_eq!(o.schema_version(), 1 + PACKS.len() as u64);
}

#[test]
fn packs_install_live_on_postgres_and_survive_a_restart() {
    let Some(pg) = common::TestTenant::new("packs_b") else { return };
    let mut entities = 0;
    {
        let o = pg.open(Schema::enterprise(), OntologyOptions::default());
        for pack in &PACKS {
            install_and_load(&o, pack);
            entities += sample_inputs(pack).len();
        }
    }
    let o = pg.open(Schema::enterprise(), OntologyOptions::default());
    let expected = enterprise_plus(&PACKS.iter().map(|p| p.toml).collect::<Vec<_>>());
    let live = o.schema();
    assert_eq!(live.object_types.len(), expected.object_types.len());
    assert_eq!(live.link_types.len(), expected.link_types.len());
    assert_eq!(live.rules.len(), expected.rules.len());
    assert_eq!(serde_json::to_value(&live.shapes).unwrap(), serde_json::to_value(&expected.shapes).unwrap());
    assert_eq!(o.read(|g, _| g.entities().count()), entities);
    let stats = o.reason_stats(&admin(), &ReasonOptions::default()).unwrap();
    for pack in &PACKS {
        for r in table(pack.toml)["rules"].as_array().unwrap() {
            let name = r["name"].as_str().unwrap();
            assert!(stats.by_rule[&format!("{SUPPLIED_PREFIX}{name}")] > 0, "{}: {name} after restart", pack.id);
        }
    }
}
