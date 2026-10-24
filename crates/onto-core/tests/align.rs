//! Schema alignment, certification and SSSOM: two fixture schemas (a plant
//! maintenance source and a maintenance-service target), the built-in
//! packages, and the engine wrappers.

use serde_json::json;

use onto_core::align::{
    self, AlignOptions, AlignSource, Certification, Gap, Mapping, MatchPredicate, MatchStatus, SSSOM_COLUMNS, Side,
};
use onto_core::live_schema::{GlossarySection, SchemaChange};
use onto_core::{ActionContext, Error, Ontology, Principal, Schema};

/// The source: how a plant names its maintenance work.
const PLANT: &str = r#"
name = "plant"
version = 1

[object_types.job_order]
label = "Job order"
description = "Planned maintenance work."
states = ["planned", "released", "done"]
terminal_states = ["done"]
properties.priority = { type = "enum", values = ["low", "normal", "high"] }
properties.due = { type = "timestamp" }
properties.cost = { type = "float" }

[object_types.fault]
label = "Fault"
extends = "job_order"
implements = ["safety_relevant"]
properties.symptom = { type = "text" }

[object_types.machine]
label = "Machine"
implements = ["maintainable"]
states = ["running", "down"]
properties.serial = { type = "string" }
properties.model = { type = "string" }

[object_types.crew]
label = "Crew"
properties.shift = { type = "enum", values = ["day", "night"] }
properties.size = { type = "int" }

[object_types.client]
label = "Client"
properties.email = { type = "string" }
properties.tier = { type = "enum", values = ["gold", "silver"] }

[interfaces.safety_relevant]
description = "Work with a safety impact."

[interfaces.maintainable]
description = "Things that get maintained."

[link_types.performed_on]
from = ["job_order"]
to = ["machine"]
inverse = "work_done"

[link_types.assigned_crew]
from = ["job_order"]
to = ["crew"]
inverse = "crew_jobs"
cardinality = "one"

[link_types.requested_by]
from = ["job_order"]
to = ["client"]
inverse = "requests"
"#;

/// The target: a maintenance service's vocabulary for the same things.
const SERVICE: &str = r#"
name = "service"
version = 1

[object_types.work_order]
label = "Work order"
states = ["planned", "released", "done"]
terminal_states = ["done"]
properties.priority = { type = "enum", values = ["low", "normal", "high"] }
properties.due = { type = "timestamp" }
properties.cost = { type = "float" }

[object_types.breakdown]
label = "Breakdown"
extends = "work_order"
properties.symptom = { type = "text" }

[object_types.asset]
label = "Asset"
implements = ["trackable"]
states = ["running", "down"]
properties.serial = { type = "string" }
properties.model = { type = "string" }

[object_types.day_crew]
label = "Day crew"
properties.shift = { type = "enum", values = ["day", "night"] }
properties.size = { type = "int" }

[object_types.night_crew]
label = "Night crew"
properties.shift = { type = "enum", values = ["day", "night"] }
properties.size = { type = "int" }

[object_types.customer]
label = "Customer"
properties.email = { type = "string" }
properties.tier = { type = "enum", values = ["gold", "silver"] }

[object_types.invoice]
label = "Invoice"
properties.amount = { type = "float" }

[interfaces.trackable]
description = "Things with a serial number."

[link_types.performed_on]
from = ["work_order"]
to = ["asset"]
inverse = "work_done"

[link_types.assigned_crew]
from = ["work_order"]
to = ["day_crew", "night_crew"]
inverse = "crew_jobs"
cardinality = "one"

[link_types.requested_by]
from = ["work_order"]
to = ["customer"]
inverse = "requests"

[link_types.billed_to]
from = ["invoice"]
to = ["customer"]

[glossary.kinds]
client = "customer"
"#;

fn plant() -> Schema {
    Schema::from_toml(PLANT).unwrap()
}

fn service() -> Schema {
    Schema::from_toml(SERVICE).unwrap()
}

fn report() -> align::AlignReport {
    align::align(&plant(), &service(), &AlignOptions::default())
}

fn row<'a>(rows: &'a [align::Match], source: &str) -> &'a align::Match {
    rows.iter().find(|m| m.source == source).unwrap_or_else(|| panic!("no row for {source}: {rows:#?}"))
}

fn mapping(source: &str, target: &str, predicate: MatchPredicate) -> Mapping {
    Mapping { source: source.into(), target: target.into(), predicate }
}

#[test]
fn glossary_synonym_aligns_differently_named_kinds() {
    let r = report();
    let m = row(&r.kinds, "client");
    assert_eq!(m.target, "customer", "{m:#?}");
    assert!(m.signals.glossary && m.signals.name == 1.0, "{m:#?}");
    assert_eq!(m.predicate, MatchPredicate::ExactMatch, "{m:#?}");
    assert_eq!(m.status, MatchStatus::Accepted, "{m:#?}");
    assert!(m.evidence[0].contains("glossary"), "{:?}", m.evidence);
    // The target glossary already says client = customer: nothing to propose.
    assert!(
        !r.change_set
            .changes
            .iter()
            .any(|c| matches!(c, SchemaChange::PutGlossaryTerm { word, .. } if word == "client")),
        "{:?}",
        r.change_set
    );
}

#[test]
fn renamed_kind_with_equal_structure_is_an_exact_match() {
    let r = report();
    let m = row(&r.kinds, "job_order");
    assert_eq!(m.target, "work_order", "{m:#?}");
    assert_eq!(m.predicate, MatchPredicate::ExactMatch, "{m:#?}");
    assert_eq!(m.status, MatchStatus::Accepted, "{m:#?}");
    assert_eq!(m.signals.property_jaccard, Some(1.0), "{m:#?}");
    assert_eq!(m.signals.states, Some(1.0), "{m:#?}");
    assert!(m.score >= align::EXACT_MIN && m.score < 1.0, "{m:#?}");
    assert_eq!(m.justification, "semapv:CompositeMatching");
    // Neither side has a parent: the signal is left out, not counted as zero.
    assert_eq!(m.signals.parents, None, "{m:#?}");
    assert!(m.evidence_coverage < 1.0, "{m:#?}");
    assert!(m.evidence.iter().any(|e| e.starts_with("parents: not compared")), "{:?}", m.evidence);
    // An accepted rename becomes a glossary proposal for the target.
    assert!(
        r.change_set.changes.iter().any(|c| matches!(c,
            SchemaChange::PutGlossaryTerm { section: GlossarySection::Kinds, word, target }
                if word == "job_order" && target == "work_order")),
        "{:?}",
        r.change_set
    );
}

#[test]
fn equal_competitors_are_a_borderline_tie() {
    let r = report();
    let m = row(&r.kinds, "crew");
    assert!(m.target == "day_crew" || m.target == "night_crew", "{m:#?}");
    assert_eq!(m.status, MatchStatus::Borderline, "{m:#?}");
    assert_eq!(m.tied_with.len(), 1, "{m:#?}");
    let other = &m.tied_with[0];
    assert_eq!(other.source, "crew");
    assert_ne!(other.target, m.target);
    assert!((other.score - m.score).abs() <= align::DEFAULT_TIE_EPSILON, "{m:#?}");
    assert!(m.evidence.last().unwrap().starts_with("borderline: tied"), "{:?}", m.evidence);
    // A tie is never turned into a glossary term.
    assert!(
        !r.change_set.changes.iter().any(|c| matches!(c, SchemaChange::PutGlossaryTerm { word, .. } if word == "crew")),
        "{:?}",
        r.change_set
    );
    // The losing crew kind is left for review with its nearest counterpart.
    let loser = r.unmatched_target.iter().find(|u| u.name == other.target).expect("tied target unmatched");
    assert_eq!(loser.nearest.as_ref().map(|c| c.source.as_str()), Some("crew"));
}

#[test]
fn links_align_through_the_kind_matches() {
    let r = report();
    let m = row(&r.links, "performed_on");
    assert_eq!(m.target, "performed_on");
    // job_order -> work_order and machine -> asset are accepted matches.
    assert_eq!(m.signals.endpoints, Some(1.0), "{m:#?}");
    assert!(m.evidence.contains(&"to: [machine] vs [asset] (1.00)".to_string()), "{:?}", m.evidence);
    assert_eq!(m.predicate, MatchPredicate::ExactMatch, "{m:#?}");
    let q = row(&r.links, "requested_by");
    assert_eq!((q.target.as_str(), q.signals.endpoints), ("requested_by", Some(1.0)), "{q:#?}");
    // billed_to (invoice -> customer) has no source counterpart.
    assert!(r.unmatched_target.iter().any(|u| u.section == "link_types" && u.name == "billed_to"), "{r:#?}");
    assert!(r.unmatched_target.iter().any(|u| u.section == "object_types" && u.name == "invoice"), "{r:#?}");
}

#[test]
fn endpoints_decide_between_differently_named_links() {
    let source = Schema::from_toml(
        r#"
name = "shop"
[object_types.purchase]
states = ["open", "paid"]
properties.total = { type = "float" }
properties.placed_at = { type = "timestamp" }
[object_types.shopper]
states = ["active", "blocked"]
properties.email = { type = "string" }
[object_types.product]
states = ["listed", "retired"]
properties.sku = { type = "string" }
[link_types.placed_by]
from = ["purchase"]
to = ["shopper"]
[link_types.contains_item]
from = ["purchase"]
to = ["product"]
"#,
    )
    .unwrap();
    let target = Schema::from_toml(
        r#"
name = "store"
[object_types.purchase]
states = ["open", "paid"]
properties.total = { type = "float" }
properties.placed_at = { type = "timestamp" }
[object_types.shopper]
states = ["active", "blocked"]
properties.email = { type = "string" }
[object_types.product]
states = ["listed", "retired"]
properties.sku = { type = "string" }
[link_types.submitted_by]
from = ["purchase"]
to = ["shopper"]
[link_types.includes]
from = ["purchase"]
to = ["product"]
"#,
    )
    .unwrap();
    let r = align::align(&source, &target, &AlignOptions::default());
    let p = row(&r.links, "placed_by");
    assert_eq!(p.target, "submitted_by", "{p:#?}");
    assert_eq!(p.signals.endpoints, Some(1.0), "{p:#?}");
    let c = row(&r.links, "contains_item");
    assert_eq!(c.target, "includes", "{c:#?}");
    assert_eq!(c.signals.endpoints, Some(1.0), "{c:#?}");
    // Different names: the endpoints carry the score, the name keeps it short of exact.
    assert!(r.links.iter().all(|m| m.predicate != MatchPredicate::ExactMatch), "{:#?}", r.links);
}

#[test]
fn a_subtype_without_a_counterpart_is_broader_matched() {
    let source = Schema::from_toml(
        r#"
name = "desk"
[object_types.ticket]
states = ["open", "closed"]
properties.priority = { type = "enum", values = ["low", "high"] }
properties.summary = { type = "string" }
[object_types.bug]
extends = "ticket"
properties.stack = { type = "text" }
properties.build = { type = "string" }
"#,
    )
    .unwrap();
    let target = Schema::from_toml(
        r#"
name = "tracker"
[object_types.issue]
states = ["open", "closed"]
properties.priority = { type = "enum", values = ["low", "high"] }
properties.summary = { type = "string" }
[glossary.kinds]
ticket = "issue"
bug = "issue"
"#,
    )
    .unwrap();
    let r = align::align(&source, &target, &AlignOptions::default());
    let t = row(&r.kinds, "ticket");
    assert_eq!((t.target.as_str(), t.predicate), ("issue", MatchPredicate::ExactMatch), "{t:#?}");
    let b = row(&r.kinds, "bug");
    assert_eq!((b.target.as_str(), b.predicate), ("issue", MatchPredicate::BroadMatch), "{b:#?}");
    assert!(b.evidence.iter().any(|e| e.contains("an ancestor of bug")), "{:?}", b.evidence);
    // A broad match is never proposed as a synonym.
    assert!(r.change_set.changes.is_empty(), "{:?}", r.change_set);
    assert!(r.unmatched_source.is_empty(), "{:?}", r.unmatched_source);
}

#[test]
fn a_target_subtype_without_a_counterpart_is_narrower_matched() {
    let source = Schema::from_toml(
        r#"
name = "tracker"
[object_types.issue]
states = ["open", "closed"]
properties.priority = { type = "enum", values = ["low", "high"] }
properties.summary = { type = "string" }
[glossary.kinds]
ticket = "issue"
bug = "issue"
"#,
    )
    .unwrap();
    let target = Schema::from_toml(
        r#"
name = "desk"
[object_types.ticket]
states = ["open", "closed"]
properties.priority = { type = "enum", values = ["low", "high"] }
properties.summary = { type = "string" }
[object_types.bug]
extends = "ticket"
properties.stack = { type = "text" }
"#,
    )
    .unwrap();
    let r = align::align(&source, &target, &AlignOptions::default());
    assert_eq!(r.kinds.len(), 2, "{:#?}", r.kinds);
    let exact = r.kinds.iter().find(|m| m.target == "ticket").unwrap();
    assert_eq!((exact.source.as_str(), exact.predicate), ("issue", MatchPredicate::ExactMatch), "{exact:#?}");
    assert!(exact.signals.glossary, "{exact:#?}");
    let narrow = r.kinds.iter().find(|m| m.target == "bug").unwrap();
    assert_eq!((narrow.source.as_str(), narrow.predicate), ("issue", MatchPredicate::NarrowMatch), "{narrow:#?}");
    assert!(r.unmatched_target.is_empty(), "{:?}", r.unmatched_target);
}

#[test]
fn similar_names_alone_stay_borderline() {
    let source = Schema::from_toml("name = \"a\"\n[object_types.colour]\n[object_types.folder]\n").unwrap();
    let target = Schema::from_toml("name = \"b\"\n[object_types.color]\n[object_types.folders]\n").unwrap();
    let r = align::align(&source, &target, &AlignOptions::default());
    // A similar spelling with nothing else to compare is a borderline close match.
    let c = row(&r.kinds, "colour");
    assert_eq!(
        (c.target.as_str(), c.predicate, c.status),
        ("color", MatchPredicate::CloseMatch, MatchStatus::Borderline)
    );
    assert_eq!(c.justification, "semapv:LexicalMatching");
    assert!(c.score >= align::ACCEPT_MIN, "{c:#?}");
    assert!(c.evidence.last().unwrap().contains("similar name alone"), "{:?}", c.evidence);
    // The same word (a plural) with nothing else is accepted, but names alone
    // never make an exact match.
    let f = row(&r.kinds, "folder");
    assert_eq!(
        (f.target.as_str(), f.predicate, f.status),
        ("folders", MatchPredicate::CloseMatch, MatchStatus::Accepted)
    );
    assert_eq!(f.evidence_coverage, 0.35, "{f:#?}");
    assert!(f.evidence.iter().any(|e| e.contains("without properties or link types to compare")), "{:?}", f.evidence);
    assert_eq!(r.change_set.changes.len(), 1, "{:?}", r.change_set);
}

#[test]
fn sssom_has_the_curie_map_and_one_row_per_match() {
    let r = report();
    let lines: Vec<&str> = r.sssom_tsv.lines().collect();
    assert_eq!(lines[0], "#curie_map:");
    assert!(lines.contains(&"#  skos: http://www.w3.org/2004/02/skos/core#"), "{}", r.sssom_tsv);
    assert!(lines.contains(&"#  semapv: https://w3id.org/semapv/vocab/"), "{}", r.sssom_tsv);
    assert!(lines.contains(&"#  src: https://onto-intelli.dev/schema/plant/"), "{}", r.sssom_tsv);
    assert!(lines.contains(&"#  tgt: https://onto-intelli.dev/schema/service/"), "{}", r.sssom_tsv);
    assert!(lines.iter().any(|l| l.starts_with("#mapping_set_id: ")), "{}", r.sssom_tsv);
    let header = lines.iter().position(|l| !l.starts_with('#')).unwrap();
    assert_eq!(lines[header], SSSOM_COLUMNS);
    let rows = &lines[header + 1..];
    assert_eq!(rows.len(), r.kinds.len() + r.links.len(), "{}", r.sssom_tsv);
    assert!(rows.iter().all(|l| l.split('\t').count() == 8), "{}", r.sssom_tsv);
    let job = rows.iter().find(|l| l.starts_with("src:class/job_order\t")).unwrap();
    let cells: Vec<&str> = job.split('\t').collect();
    assert_eq!(
        &cells[..6],
        &[
            "src:class/job_order",
            "Job order",
            "skos:exactMatch",
            "tgt:class/work_order",
            "Work order",
            "semapv:CompositeMatching"
        ]
    );
    assert_eq!(cells[6], format!("{:.3}", row(&r.kinds, "job_order").score));
    assert!(cells[7].starts_with("accepted; "), "{job}");
    assert!(rows.iter().any(|l| l.starts_with("src:relation/performed_on\t")), "{}", r.sssom_tsv);
    // The TSV reads back as the same mappings.
    let back = align::parse_sssom(&r.sssom_tsv).unwrap();
    let expect: Vec<Mapping> =
        r.kinds.iter().chain(&r.links).map(|m| mapping(&m.source, &m.target, m.predicate)).collect();
    assert_eq!(back, expect);
}

#[test]
fn alignment_is_deterministic_and_identity_is_exact() {
    let a = serde_json::to_string(&report()).unwrap();
    let b = serde_json::to_string(&report()).unwrap();
    assert_eq!(a, b);
    let e = Schema::enterprise();
    let r = align::align(&e, &e, &AlignOptions::default());
    assert!(r.unmatched_source.is_empty() && r.unmatched_target.is_empty(), "{:?}", r.unmatched_source);
    for m in r.kinds.iter().chain(&r.links) {
        assert_eq!(m.source, m.target, "{m:#?}");
        assert_eq!(m.score, 1.0, "{m:#?}");
    }
    assert!(
        r.kinds
            .iter()
            .filter(|m| m.signals.property_jaccard.is_some())
            .all(|m| m.predicate == MatchPredicate::ExactMatch)
    );
    assert!(r.change_set.changes.is_empty());
    // Two schemas with one name get distinct prefixes.
    assert!(
        r.sssom_tsv.contains("#  src: https://onto-intelli.dev/schema/enterprise-execution/source/"),
        "{}",
        r.sssom_tsv
    );
    assert!(
        r.sssom_tsv.contains("#  tgt: https://onto-intelli.dev/schema/enterprise-execution/target/"),
        "{}",
        r.sssom_tsv
    );
}

#[test]
fn certification_downgrades_an_exact_claim_with_a_reason() {
    let rows = align::certify(
        &plant(),
        &service(),
        &[
            mapping("job_order", "work_order", MatchPredicate::ExactMatch),
            mapping("fault", "work_order", MatchPredicate::ExactMatch),
            mapping("job_order", "breakdown", MatchPredicate::ExactMatch),
        ],
    );
    // Two roots: nothing to compare, so the claim stays as written.
    assert_eq!(rows[0].certification, Certification::Uncertified, "{:#?}", rows[0]);
    assert_eq!(rows[0].predicate, MatchPredicate::ExactMatch);
    assert!(rows[0].reason.contains("neither side has an ancestor"), "{}", rows[0].reason);
    // fault extends job_order (= work_order): it sits deeper than work_order.
    let f = &rows[1];
    assert_eq!(f.certification, Certification::Downgraded, "{f:#?}");
    assert_eq!((f.claimed, f.predicate), (MatchPredicate::ExactMatch, MatchPredicate::BroadMatch));
    assert_eq!(f.carried, vec!["work_order"]);
    assert!(f.comparable.is_empty(), "{f:#?}");
    assert!(f.reason.contains("so work_order is broader"), "{}", f.reason);
    // breakdown extends work_order (= job_order): the target sits deeper.
    let b = &rows[2];
    assert_eq!((b.certification, b.predicate), (Certification::Downgraded, MatchPredicate::NarrowMatch), "{b:#?}");
    assert!(b.reason.contains("so breakdown is narrower"), "{}", b.reason);
}

#[test]
fn certification_lists_gaps_and_never_upgrades() {
    let rows = align::certify(
        &plant(),
        &service(),
        &[
            mapping("job_order", "work_order", MatchPredicate::ExactMatch),
            mapping("fault", "breakdown", MatchPredicate::ExactMatch),
            mapping("machine", "asset", MatchPredicate::ExactMatch),
            mapping("job_order", "breakdown", MatchPredicate::BroadMatch),
            mapping("crew", "day_crew", MatchPredicate::CloseMatch),
            mapping("crane", "asset", MatchPredicate::ExactMatch),
            mapping("performed_on", "performed_on", MatchPredicate::ExactMatch),
        ],
    );
    // fault: job_order is carried and comparable, safety_relevant is not carried.
    let f = &rows[1];
    assert_eq!(f.certification, Certification::UpheldWithGaps, "{f:#?}");
    assert_eq!(f.predicate, MatchPredicate::ExactMatch);
    assert_eq!(f.gaps, vec![Gap { side: Side::Source, ancestor: "safety_relevant".into() }]);
    assert!(f.reason.contains("does not carry safety_relevant"), "{}", f.reason);
    // machine and asset only have interfaces no row maps: nothing to compare.
    let m = &rows[2];
    assert_eq!((m.certification, m.predicate), (Certification::Uncertified, MatchPredicate::ExactMatch), "{m:#?}");
    assert_eq!(
        m.gaps,
        vec![
            Gap { side: Side::Source, ancestor: "maintainable".into() },
            Gap { side: Side::Target, ancestor: "trackable".into() }
        ]
    );
    assert!(m.reason.starts_with("nothing to compare"), "{}", m.reason);
    // A broad claim the hierarchy contradicts (breakdown is narrower) is weakened.
    let b = &rows[3];
    assert_eq!((b.certification, b.predicate), (Certification::Downgraded, MatchPredicate::CloseMatch), "{b:#?}");
    // Two roots: nothing to compare; a close claim stays as written.
    let c = &rows[4];
    assert_eq!((c.certification, c.predicate), (Certification::Uncertified, MatchPredicate::CloseMatch), "{c:#?}");
    assert!(c.reason.contains("neither side has an ancestor"), "{}", c.reason);
    // Unknown terms and link types are uncertified, by name.
    assert_eq!(rows[5].certification, Certification::Uncertified);
    assert!(rows[5].reason.contains("crane is not an object type"), "{}", rows[5].reason);
    assert!(rows[6].reason.contains("link types have no hierarchy"), "{}", rows[6].reason);
    // Never strengthened: no row ends with a stronger predicate than claimed.
    for r in &rows {
        assert!(r.certification != Certification::Downgraded || r.predicate != MatchPredicate::ExactMatch, "{r:#?}");
    }
}

#[test]
fn certify_report_counts_and_reads_sssom() {
    let tsv = "#curie_map:\n#  skos: http://www.w3.org/2004/02/skos/core#\n\
               subject_id\tpredicate_id\tobject_id\n\
               src:class/job_order\tskos:exactMatch\ttgt:class/work_order\n\
               src:class/fault\tskos:exactMatch\ttgt:class/work_order\n";
    let mappings = align::parse_sssom(tsv).unwrap();
    assert_eq!(mappings.len(), 2);
    let r = align::certify_report(&plant(), &service(), &mappings, &[]);
    assert_eq!((r.counts.uncertified, r.counts.downgraded), (1, 1), "{r:#?}");
    assert_eq!(r.unmapped_source, vec!["client", "crew", "machine"]);
    assert!(r.unmapped_target.contains(&"invoice".to_string()));
    assert!(r.means.contains("never strengthen"));
    let rows: Vec<&str> = r.sssom_tsv.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(rows.len(), 3, "{}", r.sssom_tsv);
    assert!(
        rows[2].contains("skos:broadMatch") && rows[2].contains("downgraded; claimed skos:exactMatch"),
        "{}",
        rows[2]
    );
    // Malformed SSSOM is refused by line, not skipped.
    let bad = "subject_id\tpredicate_id\tobject_id\nsrc:class/a\towl:equivalentClass\ttgt:class/b\n";
    assert!(matches!(align::parse_sssom(bad), Err(Error::Invalid(m)) if m.contains("line 2")));
    let short = "subject_id\tpredicate_id\tobject_id\nsrc:class/a\tskos:exactMatch\n";
    assert!(matches!(align::parse_sssom(short), Err(Error::Invalid(m)) if m.contains("missing object_id")));
    assert!(align::parse_sssom("a\tb\n").is_err());
}

#[test]
fn engine_aligns_a_toml_source_and_the_change_set_applies() {
    let o = Ontology::in_memory(service());
    let admin = Principal::admin("ada");
    let r = o.schema_align(&admin, &AlignSource::Toml(PLANT.into()), &AlignOptions::default()).unwrap();
    assert_eq!(r.target_schema, "service");
    assert!(!r.change_set.changes.is_empty(), "{r:#?}");
    let plan = o.schema_plan(&admin, &r.change_set).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
    o.schema_apply(&ActionContext::new(admin.clone(), "acme"), r.change_set.clone()).unwrap();
    assert_eq!(o.schema().canonical_kind("job_order").as_deref(), Some("work_order"));
    // Aligning again never re-proposes an applied term (the new synonyms may
    // raise other scores, which is what they are for).
    let again = o.schema_align(&admin, &AlignSource::Toml(PLANT.into()), &AlignOptions::default()).unwrap();
    let word = |c: &SchemaChange| match c {
        SchemaChange::PutGlossaryTerm { word, .. } => word.clone(),
        other => panic!("unexpected change {other:?}"),
    };
    let first: Vec<String> = r.change_set.changes.iter().map(word).collect();
    assert!(again.change_set.changes.iter().map(word).all(|w| !first.contains(&w)), "{:?}", again.change_set);
    // Certify through the engine, with mappings as JSON.
    let mappings: Vec<Mapping> = serde_json::from_value(json!([
        {"source": "fault", "target": "work_order", "predicate": "skos:exactMatch"}
    ]))
    .unwrap();
    let c = o.schema_align_certify(&admin, &AlignSource::Toml(PLANT.into()), &mappings).unwrap();
    assert_eq!(c.rows[0].certification, Certification::Uncertified, "{c:#?}");
}

#[test]
fn engine_aligns_a_layered_package_over_its_own_definitions() {
    let o = Ontology::in_memory(Schema::enterprise());
    let admin = Principal::admin("ada");
    let r =
        o.schema_align(&admin, &AlignSource::Pack("builtin:data-catalog".into()), &AlignOptions::default()).unwrap();
    // data-catalog does not compile alone: it is layered on enterprise and
    // only its own definitions are aligned.
    assert!(r.kinds.iter().all(|m| m.source != "person"), "{r:#?}");
    assert!(r.unmatched_source.iter().any(|u| u.name == "data_source"), "{:?}", r.unmatched_source);
    let src = align::resolve_source(&AlignSource::Pack("data-catalog".into())).unwrap();
    assert_eq!(src.layered_on, vec!["enterprise"]);
    assert!(src.kinds.as_ref().is_some_and(|k| k.contains("table") && !k.contains("person")));
    // The same package aligned onto a schema that has it matches itself.
    let mut full = Schema::enterprise();
    full.extend_toml(onto_core::schema::DATA_CATALOG_SCHEMA).unwrap();
    let o2 = Ontology::in_memory(full);
    let r2 = o2.schema_align(&admin, &AlignSource::Pack("data-catalog".into()), &AlignOptions::default()).unwrap();
    assert!(r2.kinds.iter().all(|m| m.source == m.target), "{:#?}", r2.kinds);
    assert!(r2.unmatched_source.is_empty(), "{:?}", r2.unmatched_source);
    assert!(matches!(
        o.schema_align(&admin, &AlignSource::Pack("no-such-pack".into()), &AlignOptions::default()),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn engine_refuses_principals_without_read_and_oversized_sources() {
    let o = Ontology::in_memory(service());
    let nobody = Principal::new("nobody", vec![], vec!["*".into()]);
    let src = AlignSource::Toml(PLANT.into());
    assert!(matches!(o.schema_align(&nobody, &src, &AlignOptions::default()), Err(Error::Denied(_))));
    assert!(matches!(o.schema_align_certify(&nobody, &src, &[]), Err(Error::Denied(_))));
    let viewer = Principal::viewer("vic", "acme");
    assert!(o.schema_align(&viewer, &src, &AlignOptions::default()).is_ok());
    let huge = AlignSource::Toml(format!("# {}\n", "x".repeat(align::MAX_SOURCE_BYTES)));
    assert!(matches!(o.schema_align(&viewer, &huge, &AlignOptions::default()), Err(Error::Invalid(_))));
    assert!(matches!(
        o.schema_align(
            &viewer,
            &AlignSource::Toml("[object_types.x]\nextends = \"nope\"\n".into()),
            &AlignOptions::default()
        ),
        Err(Error::Config(_))
    ));
}

#[test]
fn options_deserialize_with_defaults_and_scope_the_source() {
    let opts: AlignOptions = serde_json::from_value(json!({"source_kinds": ["client"]})).unwrap();
    assert_eq!(opts.min_score, align::DEFAULT_MIN_SCORE);
    let r = align::align(&plant(), &service(), &opts);
    assert_eq!(r.kinds.len(), 1, "{:#?}", r.kinds);
    assert_eq!(r.kinds[0].source, "client");
    let src: AlignSource = serde_json::from_value(json!({"pack": "enterprise"})).unwrap();
    assert!(matches!(src, AlignSource::Pack(ref id) if id == "enterprise"));
    let json = serde_json::to_value(report()).unwrap();
    assert_eq!(json["means"], align::MEANS);
    assert!(json["kinds"][0]["signals"].get("endpoints").is_none(), "{}", json["kinds"][0]);
}

#[test]
fn negated_sssom_rows_are_refused_not_read_as_claims() {
    let tsv = "subject_id\tpredicate_id\tpredicate_modifier\tobject_id\n\
               src:class/p\tskos:exactMatch\t\ttgt:class/q\n\
               src:class/a\tskos:exactMatch\tNot\ttgt:class/b\n";
    let err = align::parse_sssom(tsv).unwrap_err();
    assert!(
        matches!(&err, Error::Invalid(m) if m.contains("line 3") && m.contains("negated") && m.contains("Not")),
        "{err:?}"
    );
    // An empty modifier column is a positive claim.
    let ok = "subject_id\tpredicate_id\tpredicate_modifier\tobject_id\nsrc:class/p\tskos:exactMatch\t\ttgt:class/q\n";
    assert_eq!(align::parse_sssom(ok).unwrap(), vec![mapping("p", "q", MatchPredicate::ExactMatch)]);
}

#[test]
fn reordered_words_alone_are_not_the_same_word() {
    let source = Schema::from_toml("name = \"a\"\n[object_types.account_user]\n").unwrap();
    let target = Schema::from_toml("name = \"b\"\n[object_types.user_account]\n").unwrap();
    let r = align::align(&source, &target, &AlignOptions::default());
    let m = row(&r.kinds, "account_user");
    assert_eq!(m.target, "user_account", "{m:#?}");
    assert_eq!((m.predicate, m.status), (MatchPredicate::CloseMatch, MatchStatus::Borderline), "{m:#?}");
    assert!(m.evidence[0].contains("not the same word"), "{:?}", m.evidence);
    assert!(m.evidence.last().unwrap().contains("similar name alone"), "{:?}", m.evidence);
    assert!(r.change_set.changes.is_empty(), "{:?}", r.change_set);
}

#[test]
fn proposed_glossary_words_are_normalized_like_lookups() {
    let source = Schema::from_toml(
        "name = \"a\"\n[object_types.work-order]\nstates = [\"open\", \"done\"]\n\
         properties.due = { type = \"timestamp\" }\nproperties.cost = { type = \"float\" }\n",
    )
    .unwrap();
    let mut target = Schema::from_toml(
        "name = \"b\"\n[object_types.job]\nlabel = \"Work order\"\nstates = [\"open\", \"done\"]\n\
         properties.due = { type = \"timestamp\" }\nproperties.cost = { type = \"float\" }\n",
    )
    .unwrap();
    let r = align::align(&source, &target, &AlignOptions::default());
    let m = row(&r.kinds, "work-order");
    assert_eq!((m.target.as_str(), m.status), ("job", MatchStatus::Accepted), "{m:#?}");
    let terms: Vec<(String, String)> = r
        .change_set
        .changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::PutGlossaryTerm { section: GlossarySection::Kinds, word, target } => {
                Some((word.clone(), target.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(terms, vec![("work_order".to_string(), "job".to_string())], "{:?}", r.change_set);
    for (word, kind) in terms {
        target.glossary.kinds.insert(word, kind);
    }
    for word in ["work-order", "work order", "work_order", "Work Order"] {
        assert_eq!(target.canonical_kind(word).as_deref(), Some("job"), "{word}");
    }
}

#[test]
fn a_target_taken_by_a_better_match_is_not_a_tie() {
    let source = Schema::from_toml(
        "name = \"a\"\n\
         [object_types.crew]\nstates = [\"on\", \"off\"]\n\
         properties.shift = { type = \"string\" }\nproperties.size = { type = \"int\" }\n\
         [object_types.squad]\nstates = [\"on\", \"off\"]\n\
         properties.shift = { type = \"string\" }\nproperties.size = { type = \"int\" }\n\
         properties.extra = { type = \"string\" }\n\
         [glossary.kinds]\ncrew = \"squad\"\n",
    )
    .unwrap();
    let target = Schema::from_toml(
        "name = \"b\"\n\
         [object_types.crew]\nstates = [\"on\", \"off\"]\n\
         properties.shift = { type = \"string\" }\nproperties.size = { type = \"int\" }\n\
         [object_types.troop]\nstates = [\"on\", \"off\"]\n\
         properties.shift = { type = \"string\" }\nproperties.size = { type = \"int\" }\n\
         [glossary.kinds]\nsquad = \"troop\"\n",
    )
    .unwrap();
    let r = align::align(&source, &target, &AlignOptions::default());
    let crew = row(&r.kinds, "crew");
    assert_eq!((crew.target.as_str(), crew.status), ("crew", MatchStatus::Accepted), "{crew:#?}");
    let squad = row(&r.kinds, "squad");
    assert_eq!(squad.target, "troop", "{squad:#?}");
    // squad -> crew scores the same as squad -> troop, but crew was already
    // taken by crew -> crew at a clearly higher score: not a competitor.
    assert!(squad.tied_with.is_empty(), "{squad:#?}");
    assert_eq!(squad.status, MatchStatus::Accepted, "{squad:#?}");
    assert!(squad.alternatives.iter().any(|c| c.target == "crew"), "{squad:#?}");
}

/// A help desk's own words, aligned in the docs against enterprise with the
/// manufacturing pack installed.
const HELPDESK: &str = r#"
name = "helpdesk"

[interfaces.work_unit]
description = "Anything someone works on."

[object_types.ticket]
implements = ["work_unit"]
description = "A customer request or incident."
states = ["open", "in_progress", "blocked", "resolved", "closed"]
properties.priority = { type = "enum", values = ["low", "medium", "high", "critical"] }
properties.description = { type = "text" }

[object_types.employee]
description = "Someone on staff."
properties.email = { type = "string" }
properties.role = { type = "string" }
properties.timezone = { type = "string" }

[object_types.squad]
description = "A group of employees."

[link_types.assignee]
from = ["ticket"]
to = ["employee"]
inverse = "assigned_tickets"

[link_types.in_squad]
from = ["employee"]
to = ["squad"]
inverse = "squad_members"

[object_types.incident]
extends = "ticket"
description = "An outage reported by a customer."
"#;

fn manufacturing() -> Ontology {
    let mut s = Schema::enterprise();
    s.extend_source("builtin:manufacturing", |p: &str| std::path::PathBuf::from(p)).unwrap();
    Ontology::in_memory(s)
}

#[test]
fn a_glossary_match_names_both_definitions() {
    let o = manufacturing();
    let r = o
        .schema_align(&Principal::admin("ada"), &AlignSource::Toml(HELPDESK.into()), &AlignOptions::default())
        .unwrap();
    let employee = r.kinds.iter().find(|m| m.source == "employee").expect("employee is aligned");
    assert_eq!(employee.target, "person");
    let name = &employee.evidence[0];
    assert!(name.contains("'employee' and 'person'") && name.contains("glossary word 'employee'"), "{name}");
}

#[test]
fn name_evidence_always_names_both_definitions() {
    let o = manufacturing();
    let admin = Principal::admin("ada");
    for source in [AlignSource::Pack("supply-chain".into()), AlignSource::Toml(HELPDESK.into())] {
        let r = o.schema_align(&admin, &source, &AlignOptions::default()).unwrap();
        for m in r.kinds.iter().chain(&r.links) {
            let name = &m.evidence[0];
            assert!(
                name.contains(&format!("'{}'", m.source)) && name.contains(&format!("'{}'", m.target)),
                "{} -> {}: {name}",
                m.source,
                m.target
            );
        }
    }
}

#[test]
fn one_shared_endpoint_does_not_outweigh_the_names() {
    let o = manufacturing();
    let admin = Principal::admin("ada");
    // ticket is not mapped, so only `to` compares: employee → person agrees
    // for every link type to person, and the name decides.
    let r = o.schema_align(&admin, &AlignSource::Toml(HELPDESK.into()), &AlignOptions::default()).unwrap();
    let assignee = r.links.iter().find(|m| m.source == "assignee").expect("assignee is aligned");
    assert_eq!(assignee.target, "assigned_to", "{assignee:#?}");
    assert!(assignee.evidence.iter().any(|e| e.contains("counts as 0.50")), "{:?}", assignee.evidence);
    // A pack's buyer and category manager are not a plant manager or a
    // maintenance assignee, whatever their shared `person` end says.
    let r = o.schema_align(&admin, &AlignSource::Pack("supply-chain".into()), &AlignOptions::default()).unwrap();
    for (s, t) in [("po_buyer", "plant_manager"), ("category_managed_by", "maintenance_assignee")] {
        assert!(
            !r.links.iter().any(|m| m.source == s && m.target == t && m.status == MatchStatus::Accepted),
            "{s} -> {t} accepted: {:#?}",
            r.links.iter().find(|m| m.source == s)
        );
    }
    let words: Vec<String> = r
        .change_set
        .changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::PutGlossaryTerm { target, .. } => Some(target.clone()),
            _ => None,
        })
        .collect();
    assert!(!words.iter().any(|t| t == "plant_manager" || t == "maintenance_assignee"), "{words:?}");
}

#[test]
fn a_pack_source_is_named_by_its_id() {
    let o = manufacturing();
    let r = o
        .schema_align(&Principal::admin("ada"), &AlignSource::Pack("supply-chain".into()), &AlignOptions::default())
        .unwrap();
    assert_eq!(r.source_schema, "supply-chain");
    assert_eq!(r.target_schema, "enterprise-execution");
    assert!(r.sssom_tsv.contains("align/supply-chain/enterprise-execution"), "{}", &r.sssom_tsv[..600]);
}
