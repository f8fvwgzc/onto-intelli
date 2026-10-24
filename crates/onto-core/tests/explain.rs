//! Explanations: justifications, provenance semirings, claim verification
//! with step labels, and slice preservation.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use onto_core::explain::{
    self, Claim, Entailment, JustifyOptions, ProvenanceValue, Semiring, SliceOptions, SliceStatus, StepLabel, Verdict,
};
use onto_core::reason::{Fact, ReasonOptions};
use onto_core::{ActionContext, EntityInput, Error, LinkSpec, Mutation, Ontology, Principal, Schema};

fn admin() -> Principal {
    Principal::admin("ada")
}

fn f(s: &str, p: &str, o: &str) -> Fact {
    Fact::new(s, p, o)
}

fn acme() -> ReasonOptions {
    ReasonOptions { workspace: Some("acme".into()), ..Default::default() }
}

const EXTRA: &str = r#"
[object_types.incident]
extends = "issue"

[link_types.primary_owner]
description = "The one person answerable for a service."
from = ["service"]
to = ["person"]
cardinality = "one"
inverse = "primary_for"
"#;

/// The enterprise world of the reasoner tests, plus a cardinality-one link.
fn world() -> (Ontology, ActionContext) {
    let mut s = Schema::enterprise();
    s.extend_toml(EXTRA).unwrap();
    let o = Ontology::in_memory(s);
    let ctx = ActionContext::new(admin(), "acme");
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("team", "payments", "Payments Team")),
            Mutation::Upsert(EntityInput::new("person", "kenji", "Kenji Tanaka").link("member_of", "team:payments")),
            Mutation::Upsert(EntityInput::new("person", "aiko", "Aiko Sato").link("member_of", "team:payments")),
            Mutation::Upsert(EntityInput::new("person", "mia", "Mia Chen").link("manages", "team:payments")),
            Mutation::Upsert(EntityInput::new("project", "alpha", "Project Alpha").state("active")),
            Mutation::Upsert(EntityInput::new("service", "api", "Payments API").link("primary_owner", "person:mia")),
            Mutation::Upsert(
                EntityInput::new("team", "payments", "Payments Team")
                    .link("owns", "project:alpha")
                    .link("owns", "service:api"),
            ),
            Mutation::Upsert(
                EntityInput::new("issue", "1", "Payment timeout")
                    .state("triaged")
                    .prop("priority", "high")
                    .link("belongs_to", "project:alpha")
                    .link("assigned_to", "person:kenji")
                    .link("affects", "service:api"),
            ),
            Mutation::Upsert(
                EntityInput::new("issue", "2", "Refund bug")
                    .state("open")
                    .prop("priority", "low")
                    .link("belongs_to", "project:alpha")
                    .link("relates_to", "issue:1"),
            ),
            Mutation::Upsert(EntityInput::new("incident", "9", "Outage").state("open")),
            Mutation::Upsert(
                EntityInput::new("knowledge", "secret", "Pay rates")
                    .prop("visibility", "restricted")
                    .prop("acl", json!(["person:aiko"]))
                    .link("about", "issue:1"),
            ),
        ],
    )
    .unwrap();
    (o, ctx)
}

const STEPS: &str = r#"
[object_types.step]
[object_types.step.properties.confidence]
type = "float"

[object_types.gate]
extends = "step"

[link_types.next]
transitive = true
inverse = "after"

[link_types.near]
symmetric = true

[[rules]]
name = "gated"
description = "A step leading to a gate is gated."
when = [["?s", "a", "step"], ["?s", "next", "?g"], ["?g", "a", "gate"]]
then = ["?s", "a", "gated"]
"#;

/// A small graph of steps: `links` are `(from, rel, to)` keys of kind step.
fn steps(links: &[(&str, &str, &str)]) -> Ontology {
    let o = Ontology::in_memory(Schema::from_toml(STEPS).unwrap());
    let ctx = ActionContext::new(admin(), "acme");
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for (a, _, b) in links {
        keys.insert(a);
        keys.insert(b);
    }
    let mut muts: Vec<Mutation> = keys.iter().map(|k| Mutation::Upsert(EntityInput::new("step", *k, *k))).collect();
    for (a, r, b) in links {
        muts.push(Mutation::Upsert(EntityInput::new("step", *a, *a).link(*r, format!("step:{b}"))));
    }
    o.apply(&ctx, muts).unwrap();
    o
}

fn links_of(j: &explain::Justification) -> Vec<(String, String, String)> {
    let key = |t: &str| t.trim_start_matches("<step:").trim_end_matches('>').to_string();
    j.facts
        .iter()
        .filter(|x| x.s.starts_with('<') && x.o.starts_with('<'))
        .map(|x| (key(&x.s), x.p.clone(), key(&x.o)))
        .collect()
}

fn derives(links: &[(String, String, String)], goal: &Fact) -> bool {
    let refs: Vec<(&str, &str, &str)> = links.iter().map(|(a, r, b)| (a.as_str(), r.as_str(), b.as_str())).collect();
    let o = steps(&refs);
    o.reason(&admin(), &ReasonOptions::default()).unwrap().contains(goal).is_some()
}

// ── justifications ───────────────────────────────────────────────────────

#[test]
fn two_independent_derivations_give_two_justifications() {
    let o = steps(&[("a", "next", "b"), ("b", "next", "d"), ("a", "next", "c"), ("c", "next", "d")]);
    let j = o
        .reason_explain(
            &admin(),
            &ReasonOptions::default(),
            &Claim::new("step:a", "next", "step:d"),
            &JustifyOptions::default(),
        )
        .unwrap();
    assert_eq!(j.status, Entailment::Derived);
    assert!(j.complete && j.fixpoint_reached, "{j:?}");
    let sets: Vec<Vec<Fact>> = j.justifications.iter().map(|x| x.facts.clone()).collect();
    let t = f("next", "a", "transitive");
    assert_eq!(
        sets,
        vec![
            vec![f("<step:a>", "next", "<step:b>"), f("<step:b>", "next", "<step:d>"), t.clone()],
            vec![f("<step:a>", "next", "<step:c>"), f("<step:c>", "next", "<step:d>"), t],
        ]
    );
    for x in &j.justifications {
        assert!(x.sufficient && x.minimal, "{x:?}");
        assert_eq!(x.builtin_rules, vec!["builtin/transitive"]);
        assert!(x.rules.is_empty());
        assert_eq!(x.proof.len(), 1);
        assert_eq!(x.proof[0].conclusion, f("<step:a>", "next", "<step:d>"));
    }
}

#[test]
fn justifications_are_minimal_and_rederive() {
    // a -> b -> c -> d, plus the shortcut a -> c.
    let o = steps(&[("a", "next", "b"), ("b", "next", "c"), ("c", "next", "d"), ("a", "next", "c")]);
    let goal = f("<step:a>", "next", "<step:d>");
    let j = o
        .reason_explain(
            &admin(),
            &ReasonOptions::default(),
            &Claim::new("step:a", "next", "step:d"),
            &JustifyOptions::default(),
        )
        .unwrap();
    assert!(j.complete);
    let shapes: Vec<usize> = j.justifications.iter().map(|x| x.facts.len()).collect();
    assert_eq!(shapes, vec![3, 4], "{:?}", j.justifications);
    assert!(j.justifications[0].facts.contains(&f("<step:a>", "next", "<step:c>")));
    assert!(!j.justifications[0].facts.contains(&f("<step:a>", "next", "<step:b>")));
    for x in &j.justifications {
        assert!(x.sufficient && x.minimal);
        // Re-run the reasoner over a graph holding exactly the justification's
        // links: the goal follows, and it no longer does without any one.
        let links = links_of(x);
        assert!(derives(&links, &goal), "{links:?}");
        for i in 0..links.len() {
            let mut less = links.clone();
            less.remove(i);
            assert!(!derives(&less, &goal), "still derived without {:?}", links[i]);
        }
    }
}

#[test]
fn a_supplied_rule_is_named_in_rules() {
    let (o, _) = world();
    let j = o
        .reason_explain(
            &admin(),
            &acme(),
            &Claim::new("issue:1", "handled_by_team", "team:payments"),
            &JustifyOptions::default(),
        )
        .unwrap();
    assert_eq!(j.status, Entailment::Derived);
    assert!(j.complete);
    assert_eq!(j.justifications.len(), 1, "{:?}", j.justifications);
    let x = &j.justifications[0];
    assert_eq!(
        x.facts,
        vec![f("<issue:1>", "assigned_to", "<person:kenji>"), f("<person:kenji>", "member_of", "<team:payments>")]
    );
    assert_eq!(x.rules, vec!["schema/responsible_team"]);
    assert!(x.builtin_rules.is_empty());
    assert!(x.sufficient && x.minimal);
    assert!(j.means.contains("assumptions"), "{}", j.means);
}

#[test]
fn schema_facts_appear_in_justifications() {
    let (o, _) = world();
    let j = o
        .reason_explain(&admin(), &acme(), &Claim::new("incident:9", "a", "issue"), &JustifyOptions::default())
        .unwrap();
    assert_eq!(j.justifications.len(), 1);
    assert_eq!(
        j.justifications[0].facts,
        vec![f("#incident", "extends", "#issue"), f("<incident:9>", "a", "#incident")]
    );
    assert_eq!(j.justifications[0].builtin_rules, vec!["builtin/type_extends"]);
    let w = o
        .reason_explain(&admin(), &acme(), &Claim::new("incident:9", "a", "work_item"), &JustifyOptions::default())
        .unwrap();
    assert!(!w.justifications.is_empty());
    for x in &w.justifications {
        assert!(x.sufficient && x.minimal, "{x:?}");
        assert!(x.facts.iter().any(|s| s.p == "implements"), "{x:?}");
        assert!(x.facts.contains(&f("<incident:9>", "a", "#incident")));
    }
    // A schema statement can be explained itself.
    let s = o
        .reason_explain(&admin(), &acme(), &Claim::new("incident", "extends", "issue"), &JustifyOptions::default())
        .unwrap();
    assert_eq!(s.status, Entailment::Asserted);
    assert_eq!(s.justifications[0].facts, vec![f("#incident", "extends", "#issue")]);
}

#[test]
fn store_values_and_unknown_terms() {
    let (o, _) = world();
    let st = o
        .reason_explain(&admin(), &acme(), &Claim::new("issue:1", "state", "triaged"), &JustifyOptions::default())
        .unwrap();
    assert_eq!(st.status, Entailment::Asserted);
    assert_eq!(st.justifications[0].facts, vec![f("<issue:1>", "state", "\"triaged\"")]);
    let no = o
        .reason_explain(&admin(), &acme(), &Claim::new("issue:1", "state", "done"), &JustifyOptions::default())
        .unwrap();
    assert_eq!(no.status, Entailment::NotEntailed);
    let err = o
        .reason_explain(&admin(), &acme(), &Claim::new("issue:1", "fixes", "person:kenji"), &JustifyOptions::default())
        .unwrap_err();
    assert!(matches!(&err, Error::Invalid(m) if m.contains("outside vocabulary: relation fixes")), "{err}");
}

#[test]
fn bounds_mark_the_search_incomplete() {
    let o = steps(&[("a", "next", "b"), ("b", "next", "d"), ("a", "next", "c"), ("c", "next", "d")]);
    let bounds = JustifyOptions { max_justifications: 1, ..Default::default() };
    let j = o
        .reason_explain(&admin(), &ReasonOptions::default(), &Claim::new("step:a", "next", "step:d"), &bounds)
        .unwrap();
    assert_eq!(j.justifications.len(), 1);
    assert!(!j.complete);
    assert!(j.stopped.as_deref().is_some_and(|s| s.contains("max_justifications")), "{:?}", j.stopped);
    let tiny = JustifyOptions { max_steps: 1, ..Default::default() };
    let j =
        o.reason_explain(&admin(), &ReasonOptions::default(), &Claim::new("step:a", "next", "step:d"), &tiny).unwrap();
    assert!(!j.complete);
    assert!(j.justifications.iter().all(|x| x.sufficient && x.minimal));
}

// ── provenance ───────────────────────────────────────────────────────────

#[test]
fn trust_uses_confidence() {
    let o = Ontology::in_memory(Schema::from_toml(STEPS).unwrap());
    let ctx = ActionContext::new(admin(), "acme");
    let conf = |c: f64| -> Map<String, Value> { [("confidence".to_string(), json!(c))].into_iter().collect() };
    let mut a = EntityInput::new("step", "a", "A").prop("confidence", 0.7);
    a.links.push(LinkSpec { rel: "next".into(), to: "gate:x".into(), props: conf(0.3) });
    a.links.push(LinkSpec { rel: "next".into(), to: "gate:y".into(), props: conf(0.8) });
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("gate", "x", "X")),
            Mutation::Upsert(EntityInput::new("gate", "y", "Y")),
            Mutation::Upsert(a),
        ],
    )
    .unwrap();
    let claim = Claim::new("step:a", "a", "gated");
    let p = o
        .reason_provenance(&admin(), &ReasonOptions::default(), &claim, Semiring::Trust, &JustifyOptions::default())
        .unwrap();
    // max(min(0.7, 0.3, 1.0), min(0.7, 0.8, 1.0)) = 0.7
    assert_eq!(p.value, ProvenanceValue::Trust(0.7), "{p:?}");
    assert!(p.exact && !p.cycle_in_support);
    assert!(p.means.contains("opinion"), "{}", p.means);
    // The link's own confidence bounds a fact resting on it.
    let ax = Claim::new("step:a", "next", "gate:x");
    let p = o
        .reason_provenance(&admin(), &ReasonOptions::default(), &ax, Semiring::Trust, &JustifyOptions::default())
        .unwrap();
    assert_eq!(p.value, ProvenanceValue::Trust(0.3));
    // Explicit weights override the graph.
    let c = o.reason(&admin(), &ReasonOptions::default()).unwrap();
    let weights: BTreeMap<Fact, f64> = [(f("<step:a>", "a", "#step"), 0.5)].into_iter().collect();
    let p = explain::provenance(&c, &f("<step:a>", "a", "#gated"), Semiring::Trust, &weights);
    assert_eq!(p.value, ProvenanceValue::Trust(0.5));
}

#[test]
fn count_on_a_diamond_is_two() {
    let o = steps(&[("a", "next", "b"), ("b", "next", "d"), ("a", "next", "c"), ("c", "next", "d")]);
    let claim = Claim::new("step:a", "next", "step:d");
    let p = o
        .reason_provenance(&admin(), &ReasonOptions::default(), &claim, Semiring::Count, &JustifyOptions::default())
        .unwrap();
    assert_eq!(p.value, ProvenanceValue::Count(2));
    assert!(p.exact && !p.cycle_in_support);
    assert_eq!(serde_json::to_value(&p).unwrap()["value"], json!(2));
    // An asserted fact is one tree of its own.
    let ab = Claim::new("step:a", "next", "step:b");
    let p = o
        .reason_provenance(&admin(), &ReasonOptions::default(), &ab, Semiring::Count, &JustifyOptions::default())
        .unwrap();
    assert_eq!(p.value, ProvenanceValue::Count(1));
    assert_eq!(p.status, Entailment::Asserted);
}

#[test]
fn a_cycle_in_the_support_is_flagged() {
    // a near b (symmetric), and the loop b -> c -> d -> b (transitive).
    let o = steps(&[("a", "near", "b"), ("b", "next", "c"), ("c", "next", "d"), ("d", "next", "b")]);
    for claim in [Claim::new("step:b", "near", "step:a"), Claim::new("step:b", "next", "step:b")] {
        let p = o
            .reason_provenance(&admin(), &ReasonOptions::default(), &claim, Semiring::Count, &JustifyOptions::default())
            .unwrap();
        assert!(p.cycle_in_support, "{p:?}");
        assert!(!p.exact, "a count over a cycle is a lower bound");
        assert!(matches!(p.value, ProvenanceValue::Count(n) if n >= 1), "{p:?}");
        let t = o
            .reason_provenance(&admin(), &ReasonOptions::default(), &claim, Semiring::Trust, &JustifyOptions::default())
            .unwrap();
        assert!(t.cycle_in_support && t.exact);
    }
}

#[test]
fn why_provenance_lists_the_justifications() {
    let o = steps(&[("a", "next", "b"), ("b", "next", "d"), ("a", "next", "c"), ("c", "next", "d")]);
    let claim = Claim::new("step:a", "next", "step:d");
    let p = o
        .reason_provenance(&admin(), &ReasonOptions::default(), &claim, Semiring::Why, &JustifyOptions::default())
        .unwrap();
    let j = o.reason_explain(&admin(), &ReasonOptions::default(), &claim, &JustifyOptions::default()).unwrap();
    let ProvenanceValue::Why(sets) = &p.value else { panic!("{p:?}") };
    assert_eq!(sets, &j.justifications.iter().map(|x| x.facts.clone()).collect::<Vec<_>>());
    assert!(p.exact);
}

// ── verification ─────────────────────────────────────────────────────────

#[test]
fn verify_reports_every_verdict() {
    let (o, _) = world();
    let claims = vec![
        Claim::new("issue:1", "assigned_to", "person:kenji"),
        Claim::new("person:kenji", "works_on", "issue:1"),
        Claim::new("issue:1", "handled_by_team", "team:payments"),
        Claim::new("incident:9", "a", "issue"),
        Claim::new("issue:1", "state", "open"),
        Claim::new("issue:1", "prop.priority", "low"),
        Claim::new("service:api", "primary_owner", "person:kenji"),
        Claim::new("issue:1", "a", "person"),
        Claim::new("issue:2", "handled_by_team", "team:payments"),
        Claim::new("issue:1", "fixes", "person:kenji"),
        Claim::new("person:nobody", "member_of", "team:payments"),
        Claim::new("issue:1", "a", "dragon"),
        Claim::new("issue:1", "prop.nope", 1),
        Claim::new("issue:1", "state", "triaged"),
    ];
    let r = o.reason_verify(&admin(), &acme(), &claims).unwrap();
    let verdicts: Vec<Verdict> = r.verdicts.iter().map(|v| v.verdict).collect();
    use Verdict::*;
    assert_eq!(
        verdicts,
        vec![
            Asserted,
            Asserted,
            Derived,
            Derived,
            Contradicted,
            Contradicted,
            Contradicted,
            Contradicted,
            NotEntailed,
            OutsideVocabulary,
            OutsideVocabulary,
            OutsideVocabulary,
            OutsideVocabulary,
            Asserted,
        ],
        "{:#?}",
        r.verdicts
    );
    let v = &r.verdicts;
    assert_eq!(v[1].fact, Some(f("<issue:1>", "assigned_to", "<person:kenji>")), "inverse names read canonically");
    assert_eq!(v[2].rules, vec!["schema/responsible_team"]);
    assert_eq!(
        v[2].leaves,
        vec![f("<issue:1>", "assigned_to", "<person:kenji>"), f("<person:kenji>", "member_of", "<team:payments>")]
    );
    assert_eq!(v[2].derivation.len(), 1);
    assert_eq!(v[3].rules, vec!["builtin/type_extends"]);
    assert_eq!(v[4].conflicting, vec![f("<issue:1>", "state", "\"triaged\"")]);
    assert_eq!(v[5].conflicting, vec![f("<issue:1>", "prop.priority", "\"high\"")]);
    assert_eq!(v[6].conflicting, vec![f("<service:api>", "primary_owner", "<person:mia>")]);
    assert!(v[6].reason.contains("at most one"), "{}", v[6].reason);
    assert_eq!(v[7].conflicting, vec![f("<issue:1>", "a", "#issue")]);
    assert_eq!(v[9].unknown, vec!["relation fixes"]);
    assert_eq!(v[10].unknown, vec!["entity person:nobody"]);
    assert_eq!(v[11].unknown, vec!["class dragon"]);
    assert_eq!(v[12].unknown, vec!["property nope"]);
    assert_eq!(r.counts["contradicted"], 4);
    assert_eq!(r.counts["outside_vocabulary"], 4);
    assert!(r.fixpoint_reached);
    assert!(r.means.contains("not falsity"), "{}", r.means);
}

#[test]
fn verify_labels_every_step() {
    let (o, _) = world();
    let assigned = Claim::new("issue:1", "assigned_to", "person:kenji");
    let member = Claim::new("person:kenji", "member_of", "team:payments");
    let handled = Claim::new("issue:1", "handled_by_team", "team:payments");
    let claims = vec![
        handled.clone().step(Some("responsible_team"), vec![assigned.clone(), member.clone()]),
        // Premises in any order, rule found by the checker.
        handled.clone().step(None, vec![member.clone(), assigned.clone()]),
        Claim::new("issue:2", "handled_by_team", "team:payments").step(
            Some("schema/responsible_team"),
            vec![
                Claim::new("issue:2", "assigned_to", "person:aiko"),
                Claim::new("person:aiko", "member_of", "team:payments"),
            ],
        ),
        Claim::new("issue:2", "handled_by_team", "team:payments").step(
            Some("responsible_team"),
            vec![
                Claim::new("issue:2", "assigned_to", "person:ghost"),
                Claim::new("person:ghost", "member_of", "team:payments"),
            ],
        ),
        handled.clone().step(Some("made_up"), vec![assigned.clone()]),
        handled.clone().step(Some("accountable_team"), vec![assigned.clone(), member.clone()]),
        handled.clone().step(Some("responsible_team"), vec![assigned.clone()]),
        assigned.clone().step(Some("responsible_team"), vec![member.clone()]),
    ];
    let r = o.reason_verify(&admin(), &acme(), &claims).unwrap();
    let labels: Vec<StepLabel> = r.verdicts.iter().map(|v| v.step.as_ref().expect("a step").label).collect();
    use StepLabel::*;
    assert_eq!(
        labels,
        vec![
            SoundStep,
            SoundStep,
            PremisesNotInGraph,
            PremisesNotInGraph,
            RuleNotInTable,
            RuleDoesNotConcludeThis,
            RuleDoesNotConcludeThis,
            NoRuleConcludesThisRelation,
        ],
        "{:#?}",
        r.verdicts
    );
    let step = |i: usize| r.verdicts[i].step.clone().unwrap();
    assert_eq!(step(1).rule.as_deref(), Some("schema/responsible_team"));
    assert_eq!(step(2).missing, vec![f("<issue:2>", "assigned_to", "<person:aiko>")]);
    assert_eq!(r.verdicts[2].verdict, Verdict::NotEntailed);
    assert_eq!(step(3).missing.len(), 2, "{:?}", step(3));
    assert!(step(5).reason.contains("accountable_team"), "{}", step(5).reason);
    // A claim without a rule or premises carries no step.
    let plain = o.reason_verify(&admin(), &acme(), &[handled]).unwrap();
    assert!(plain.verdicts[0].step.is_none());
}

#[test]
fn verification_respects_visibility_and_scope() {
    let (o, _) = world();
    let about = Claim::new("knowledge:secret", "about", "issue:1");
    let admin_view = o.reason_verify(&admin(), &acme(), std::slice::from_ref(&about)).unwrap();
    assert_eq!(admin_view.verdicts[0].verdict, Verdict::Asserted);
    let bob = Principal::member("bob", "acme");
    let bob_view = o.reason_verify(&bob, &acme(), std::slice::from_ref(&about)).unwrap();
    assert_eq!(bob_view.verdicts[0].verdict, Verdict::OutsideVocabulary, "{:?}", bob_view.verdicts[0]);
    assert_eq!(bob_view.verdicts[0].unknown, vec!["entity knowledge:secret"]);
    let beta = ReasonOptions { workspace: Some("beta".into()), ..Default::default() };
    assert!(matches!(o.reason_verify(&bob, &beta, &[about]), Err(Error::Denied(_))));
    let slice: BTreeSet<String> = ["issue:1".to_string()].into();
    let opts = SliceOptions { reason: beta, ..Default::default() };
    assert!(matches!(o.reason_slice_check(&bob, &opts, &slice, None), Err(Error::Denied(_))));
}

// ── slices ───────────────────────────────────────────────────────────────

#[test]
fn slices_preserve_and_lose_goals() {
    let (o, _) = world();
    let opts = SliceOptions { reason: acme(), ..Default::default() };
    let goals = vec![
        Claim::new("issue:1", "handled_by_team", "team:payments"),
        Claim::new("incident:9", "a", "issue"),
        Claim::new("issue:2", "handled_by_team", "team:payments"),
        Claim::new("issue:1", "state", "triaged"),
        Claim::new("issue:1", "fixes", "team:payments"),
    ];
    let ids = |xs: &[&str]| -> BTreeSet<String> { xs.iter().map(|s| s.to_string()).collect() };
    let whole = o
        .reason_slice_check(&admin(), &opts, &ids(&["issue:1", "person:kenji", "team:payments"]), Some(&goals))
        .unwrap();
    let statuses: Vec<SliceStatus> = whole.goals.iter().map(|g| g.status).collect();
    use SliceStatus::*;
    assert_eq!(statuses, vec![Preserved, Lost, NotEntailed, Preserved, OutsideVocabulary], "{:#?}", whole.goals);
    assert_eq!(whole.goals[1].missing_premises, vec![f("<incident:9>", "a", "#incident")]);
    assert_eq!(whole.goals[1].missing_entities, vec!["incident:9"]);
    assert!(whole.slice_only.is_empty());

    let cut = o
        .reason_slice_check(&admin(), &opts, &ids(&["issue:1", "team:payments", "person:ghost"]), Some(&goals[..1]))
        .unwrap();
    let g = &cut.goals[0];
    assert_eq!(g.status, Lost);
    assert_eq!(
        g.missing_premises,
        vec![f("<issue:1>", "assigned_to", "<person:kenji>"), f("<person:kenji>", "member_of", "<team:payments>")]
    );
    assert_eq!(g.missing_entities, vec!["person:kenji"]);
    assert_eq!(cut.unknown_ids, vec!["person:ghost"]);
    assert_eq!((cut.preserved, cut.lost), (0, 1));
    assert_eq!(cut.counts["lost"], 1);
    // Coverage is a proxy: well below one here, and it is not preservation.
    assert!(cut.coverage.is_some_and(|c| c > 0.0 && c < 1.0), "{:?}", cut.coverage);
    assert!(cut.means.contains("Coverage is a proxy; preservation is the property"));
    assert!(cut.fixpoint_reached);
}

#[test]
fn a_slice_without_goals_lists_what_it_loses() {
    let (o, _) = world();
    let opts = SliceOptions { reason: acme(), ..Default::default() };
    let slice: BTreeSet<String> = ["issue:1", "team:payments"].iter().map(|s| s.to_string()).collect();
    let r = o.reason_slice_check(&admin(), &opts, &slice, None).unwrap();
    let lost = r
        .lost_in_slice_vocabulary
        .iter()
        .find(|l| l.fact == f("<issue:1>", "handled_by_team", "<team:payments>"))
        .unwrap_or_else(|| panic!("{:#?}", r.lost_in_slice_vocabulary));
    assert_eq!(lost.rule, "schema/responsible_team");
    assert_eq!(lost.missing_entities, vec!["person:kenji"]);
    assert_eq!(r.lost, r.lost_in_slice_vocabulary.len());
    assert!(r.lost >= 2, "accountable and owning team go too: {:?}", r.lost_in_slice_vocabulary);
    // `<issue:1> a #work_item` needs only the slice and schema facts.
    assert!(r.preserved >= 1);
    assert!(r.goals.is_empty());
    let capped = o.reason_slice_check(&admin(), &SliceOptions { limit: 1, ..opts }, &slice, None).unwrap();
    assert_eq!(capped.lost_in_slice_vocabulary.len(), 1);
    assert_eq!(capped.lost, r.lost);
    assert!(capped.truncated);
}

#[test]
fn a_long_chain_has_one_justification() {
    let keys: Vec<String> = (0..48).map(|i| format!("{i:02}")).collect();
    let links: Vec<(&str, &str, &str)> = keys.windows(2).map(|w| (w[0].as_str(), "next", w[1].as_str())).collect();
    let o = steps(&links);
    let claim = Claim::new("step:00", "next", "step:47");
    let j = o.reason_explain(&admin(), &ReasonOptions::default(), &claim, &JustifyOptions::default()).unwrap();
    assert!(j.complete, "{:?}", j.stopped);
    assert_eq!(j.justifications.len(), 1);
    let x = &j.justifications[0];
    assert_eq!(x.facts.len(), 48, "47 links and the transitive declaration");
    assert!(x.sufficient && x.minimal);
    assert!(x.proof.last().is_some_and(|d| d.conclusion == f("<step:00>", "next", "<step:47>")));
    let p = o
        .reason_provenance(&admin(), &ReasonOptions::default(), &claim, Semiring::Count, &JustifyOptions::default())
        .unwrap();
    // Binary bracketings of a 47-link path number C(46) > u64::MAX: the
    // count saturates and says so.
    assert_eq!(p.value, ProvenanceValue::Count(u64::MAX), "{p:?}");
    assert!(!p.exact);
    assert!(p.stopped.as_deref().is_some_and(|s| s.contains("u64::MAX")), "{:?}", p.stopped);
}

#[test]
fn a_supplied_horn_rule_rests_on_its_premises() {
    let o = Ontology::in_memory(Schema::from_toml(STEPS).unwrap());
    let ctx = ActionContext::new(admin(), "acme");
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("gate", "x", "X")),
            Mutation::Upsert(EntityInput::new("step", "a", "A").link("next", "gate:x")),
        ],
    )
    .unwrap();
    let j = o
        .reason_explain(
            &admin(),
            &ReasonOptions::default(),
            &Claim::new("step:a", "a", "gated"),
            &JustifyOptions::default(),
        )
        .unwrap();
    assert_eq!(j.justifications.len(), 1, "{:?}", j.justifications);
    let x = &j.justifications[0];
    assert_eq!(
        x.facts,
        vec![f("<gate:x>", "a", "#gate"), f("<step:a>", "a", "#step"), f("<step:a>", "next", "<gate:x>")]
    );
    assert_eq!(x.rules, vec!["schema/gated"]);
    // A builtin rule named for a relation that is not transitive.
    let r = o
        .reason_verify(
            &admin(),
            &ReasonOptions::default(),
            &[Claim::new("step:a", "near", "gate:x")
                .step(Some("transitive"), vec![Claim::new("step:a", "near", "step:a")])],
        )
        .unwrap();
    let step = r.verdicts[0].step.clone().unwrap();
    assert_eq!(step.label, StepLabel::RuleDoesNotConcludeThis, "{step:?}");
    assert!(step.reason.contains("near a transitive does not hold"), "{}", step.reason);
}
