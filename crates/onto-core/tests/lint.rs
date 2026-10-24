//! Schema lint: every built-in package is free of errors, a fixture schema
//! triggers each check exactly once, and reviewer feedback round-trips
//! (in memory and on PostgreSQL).

mod common;

use std::collections::BTreeMap;

use serde_json::{Map, json};

use onto_core::graph::Graph;
use onto_core::lint::{self, CHECKS, LintDecision, LintQuery, NS_FEEDBACK, Severity};
use onto_core::reason::{self, ReasonOptions};
use onto_core::schema::{PACKAGES, Rule, builtin_package};
use onto_core::shape_model::{PropertyShape, Shape};
use onto_core::{
    ActionContext, Entity, EntityInput, Error, EventFilter, Link, Mutation, Ontology, OntologyOptions, Principal,
    Schema,
};

fn all(_: &Entity) -> bool {
    true
}

fn layered(names: &[&str]) -> Schema {
    let mut s = Schema::empty();
    for n in names {
        s.extend_toml(builtin_package(n).unwrap()).unwrap_or_else(|e| panic!("{names:?}: {e}"));
    }
    s
}

#[test]
fn builtin_packages_have_no_errors() {
    let mut combos: Vec<Vec<&str>> = vec![
        vec!["enterprise"],
        vec!["nexc-execution"],
        vec!["enterprise", "nexc-execution"],
        vec!["enterprise", "data-catalog"],
        vec!["enterprise", "data-catalog", "code"],
        vec!["enterprise", "data-catalog", "code", "nexc-execution"],
    ];
    // Packages that also load on their own are linted on their own.
    for (name, text) in PACKAGES {
        if Schema::from_toml(text).is_ok() && !combos.iter().any(|c| c == &[*name]) {
            combos.push(vec![name]);
        }
    }
    assert!(PACKAGES.iter().all(|(n, _)| combos.iter().any(|c| c.contains(n))), "every package is covered");
    for names in combos {
        let s = layered(&names);
        let r = lint::lint(&s, None, &all);
        let errors: Vec<_> = r.findings.iter().filter(|f| f.severity == Severity::Error).collect();
        assert!(errors.is_empty(), "{names:?}: {errors:#?}");
        assert_eq!(r.counts.error, 0);
        // With an empty graph every type and link type is unused (info only).
        let with_graph = lint::lint(&s, Some(&Graph::default()), &all);
        assert!(with_graph.not_run.is_empty());
        assert_eq!(with_graph.counts.error, 0, "{names:?}");
        assert_eq!(
            with_graph.findings.iter().filter(|f| f.check == "unused_type").count(),
            s.object_types.len(),
            "{names:?}"
        );
    }
}

/// Clean except for exactly one trigger per check (see the comments).
const FIXTURE: &str = r#"
name = "fixture"

[interfaces.tracked]
description = "Things with an owner."
[interfaces.dormant]            # unused_interface
description = "Nothing implements this."

[object_types.org]
description = "An organization."
implements = ["tracked"]

[object_types.person]
description = "A person."
states = ["active", "left"]
terminal_states = ["gone"]      # state_lists
properties.level = { type = "enum", values = [] }   # empty_enum

[object_types.ghost]            # duplicate_label (with person); unused_type
label = "Person"
description = "A person-like organization."
extends = "org"

[object_types.lonely]           # orphan_type; missing_description

[object_types.loop_a]           # extends_cycle
description = "One half of a cycle."
extends = "loop_b"
[object_types.loop_b]
description = "The other half."
extends = "loop_a"

[object_types.vehicle]          # flat_hierarchy
description = "A vehicle."
[object_types.v1]
description = "Kind 1."
extends = "vehicle"
[object_types.v2]
description = "Kind 2."
extends = "vehicle"
[object_types.v3]
description = "Kind 3."
extends = "vehicle"
[object_types.v4]
description = "Kind 4."
extends = "vehicle"
[object_types.v5]
description = "Kind 5."
extends = "vehicle"
[object_types.v6]
description = "Kind 6."
extends = "vehicle"
[object_types.v7]
description = "Kind 7."
extends = "vehicle"
[object_types.v8]
description = "Kind 8."
extends = "vehicle"
[object_types.v9]
description = "Kind 9."
extends = "vehicle"

[link_types.employs]
description = "The organization employs the person."
from = ["org"]
to = ["person"]
inverse = "employed_by"

[link_types.knows]              # symmetric_endpoints; unused_link_type
description = "Two parties know each other."
from = ["person"]
to = ["person", "org"]
symmetric = true

[link_types.reports_to]         # transitive_cardinality_one
description = "Reporting line."
from = ["person"]
to = ["person"]
inverse = "manages"
transitive = true
cardinality = "one"

[link_types.anything]           # unconstrained_link
description = "A person relates to anything."
from = ["person"]
inverse = "anything_of"

[link_types.owns_thing]         # missing_inverse
description = "The organization owns the vehicle."
from = ["org"]
to = ["vehicle"]

[link_types.state]              # keyword_collision
description = "A person's state organization."
from = ["person"]
to = ["org"]
inverse = "state_of"

[[rules]]                       # dead_rule: employs reaches a person, employs starts from an org
name = "boss_of_boss"
description = "Employers of employers."
chain = ["employs", "employs"]
infer = "boss_of_boss"

[[rules]]
name = "linked_base"
description = "Anything a person relates to is linked."
chain = ["anything"]
infer = "linked"

[[rules]]                       # recursive_rule
name = "linked_step"
description = "Links chain."
when = [["?x", "linked", "?y"], ["?y", "anything", "?z"]]
then = ["?x", "linked", "?z"]
"#;

/// The fixture, plus what schema compile would refuse: a rule on an unknown
/// class and a shape with an unknown property.
fn fixture() -> Schema {
    let mut s = Schema::from_toml(FIXTURE).unwrap();
    s.rules.push(Rule {
        name: "martians".into(),
        description: "Martians are aliens.".into(),
        when: vec![vec![json!("?x"), json!("a"), json!("martian")]],
        then: vec![json!("?x"), json!("a"), json!("alien")],
        ..Default::default()
    });
    s.shapes.insert(
        "person_shape".into(),
        Shape {
            description: "People have a level.".into(),
            target: Some("person".into()),
            properties: vec![PropertyShape {
                id: "person_shape/0".into(),
                path: "prop.nope".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    s
}

fn entity(kind: &str, key: &str) -> Entity {
    Entity {
        id: format!("{kind}:{key}"),
        kind: kind.into(),
        workspace: "acme".into(),
        title: key.into(),
        state: None,
        props: Map::new(),
        aliases: Vec::new(),
        provenance: None,
        created_at: 0,
        updated_at: 0,
        revision: 1,
    }
}

fn link(from: &str, rel: &str, to: &str) -> Link {
    Link {
        from: from.into(),
        rel: rel.into(),
        to: to.into(),
        props: Map::new(),
        workspace: "acme".into(),
        created_at: 0,
    }
}

/// One object of every type but ghost, one link of every type but knows.
fn fixture_graph(s: &Schema) -> Graph {
    let mut g = Graph::default();
    for kind in s.object_types.keys().filter(|k| *k != "ghost") {
        g.put_entity(entity(kind, "1"));
    }
    for (f, r, t) in [
        ("org:1", "employs", "person:1"),
        ("person:1", "reports_to", "person:1"),
        ("person:1", "anything", "lonely:1"),
        ("org:1", "owns_thing", "vehicle:1"),
        ("person:1", "state", "org:1"),
    ] {
        g.put_link(link(f, r, t));
    }
    g
}

#[test]
fn fixture_triggers_each_check_once() {
    let s = fixture();
    let g = fixture_graph(&s);
    let r = lint::lint(&s, Some(&g), &all);
    assert!(r.not_run.is_empty());
    assert_eq!(r.checks_run.len(), CHECKS.len());
    let expected: BTreeMap<&str, &str> = [
        ("missing_description", "object_type:lonely"),
        ("missing_inverse", "link_type:owns_thing"),
        ("unconstrained_link", "link_type:anything"),
        ("orphan_type", "object_type:lonely"),
        ("extends_cycle", "object_type:loop_a"),
        ("dead_rule", "rule:boss_of_boss"),
        ("recursive_rule", "rule:linked_step"),
        ("keyword_collision", "link_type:state"),
        ("transitive_cardinality_one", "link_type:reports_to"),
        ("symmetric_endpoints", "link_type:knows"),
        ("state_lists", "object_type:person"),
        ("empty_enum", "object_type:person.level"),
        ("flat_hierarchy", "object_type:vehicle"),
        ("shape_reference", "shape:person_shape/0"),
        ("unused_interface", "interface:dormant"),
        ("duplicate_label", "object_type:ghost, object_type:person"),
        ("unknown_rule_class", "rule:martians"),
        ("unused_type", "object_type:ghost"),
        ("unused_link_type", "link_type:knows"),
    ]
    .into();
    assert_eq!(expected.len(), CHECKS.len(), "the fixture covers every check");
    for c in CHECKS {
        let got: Vec<_> = r.findings.iter().filter(|f| f.check == c.name).collect();
        assert_eq!(got.len(), 1, "{}: {got:#?}", c.name);
        let f = got[0];
        assert_eq!(f.target, expected[c.name], "{}: {f:#?}", c.name);
        assert_eq!(f.severity, c.severity, "{}", c.name);
        assert_eq!(f.id, lint::finding_id(c.name, &f.target));
        assert!(!f.message.is_empty() && !f.fix.is_empty(), "{f:#?}");
    }
    assert_eq!(r.findings.len(), CHECKS.len(), "{:#?}", r.findings);
    assert_eq!((r.counts.error, r.counts.warning, r.counts.info), (6, 5, 8));

    // Messages carry the evidence.
    let msg = |check: &str| r.findings.iter().find(|f| f.check == check).unwrap().message.clone();
    let dead = msg("dead_rule");
    assert!(dead.contains("?x1") && dead.contains("target of employs") && dead.contains("source of employs"), "{dead}");
    assert!(msg("recursive_rule").contains("linked_step -> linked_step"), "{}", msg("recursive_rule"));
    assert!(msg("extends_cycle").contains("loop_a extends loop_b extends loop_a"), "{}", msg("extends_cycle"));
    assert!(msg("state_lists").contains("terminal states [gone]"), "{}", msg("state_lists"));
    assert!(msg("keyword_collision").contains("reserved keyword"), "{}", msg("keyword_collision"));
    assert!(msg("shape_reference").contains("unknown property nope"), "{}", msg("shape_reference"));
    assert!(msg("unknown_rule_class").contains("a martian"), "{}", msg("unknown_rule_class"));

    // Without a graph the graph checks are listed as not run, never dropped silently.
    let r = lint::lint(&s, None, &all);
    let not_run: Vec<&str> = r.not_run.iter().map(|n| n.check.as_str()).collect();
    assert_eq!(not_run, ["unused_type", "unused_link_type"]);
    assert_eq!(r.findings.len(), CHECKS.len() - 2);

    // Graph checks count only what the reader may see.
    let hide_vehicles = |e: &Entity| !e.id.starts_with("vehicle:");
    let r = lint::lint_checks(&s, Some(&g), &hide_vehicles, Some(&["unused_link_type".into()])).unwrap();
    let targets: Vec<&str> = r.findings.iter().map(|f| f.target.as_str()).collect();
    assert_eq!(targets, ["link_type:knows", "link_type:owns_thing"]);
}

#[test]
fn dead_rules_account_for_subtypes_interfaces_and_any() {
    let base = r#"
[interfaces.party]
description = "A party."
[object_types.org]
description = "An organization."
implements = ["party"]
[object_types.team]
description = "A team."
extends = "org"
[object_types.person]
description = "A person."
states = ["active", "left"]
[link_types.member_of]
description = "Membership."
from = ["person"]
to = ["team"]
inverse = "members"
[link_types.funds]
description = "Funding."
from = ["party"]
to = ["org"]
inverse = "funded_by"
[link_types.likes]
description = "Anything likes anything."
"#;
    let dead = |rule: &str| {
        let s = Schema::from_toml(&format!("{base}\n[[rules]]\ndescription = \"x\"\n{rule}")).unwrap();
        let r = lint::lint_checks(&s, None, &all, Some(&["dead_rule".into()])).unwrap();
        r.findings.into_iter().map(|f| f.message).collect::<Vec<_>>()
    };
    // A team is an org (subtype) and a party (interface through its parent).
    assert!(dead("name = 'r'\nchain = ['member_of', 'funds']\ninfer = 'x'").is_empty());
    // Empty from/to means any.
    assert!(dead("name = 'r'\nchain = ['likes', 'member_of']\ninfer = 'x'").is_empty());
    // A person is never funded (to = org).
    let m = dead("name = 'r'\nchain = ['funds', 'member_of']\ninfer = 'x'");
    assert_eq!(m.len(), 1, "{m:?}");
    assert!(m[0].contains("step 1 funds") && m[0].contains("step 2 member_of"), "{m:?}");
    // Inverse steps flip the ends: members of a team are people, who are not funded.
    assert!(dead("name = 'r'\nchain = ['members']\ninfer = 'x'").is_empty());
    assert_eq!(dead("name = 'r'\nchain = ['members', 'funded_by']\ninfer = 'x'").len(), 1);
    // Horn form: a class, a state and a constant's kind constrain too.
    assert!(
        dead("name = 'r'\nwhen = [['?p', 'a', 'person'], ['?p', 'member_of', '?t']]\nthen = ['?p', 'a', 'm']")
            .is_empty()
    );
    assert_eq!(
        dead("name = 'r'\nwhen = [['?o', 'a', 'org'], ['?o', 'member_of', '?t']]\nthen = ['?o', 'a', 'm']").len(),
        1
    );
    assert_eq!(
        dead("name = 'r'\nwhen = [['?t', 'a', 'team'], ['?t', 'state', 'active']]\nthen = ['?t', 'a', 'm']").len(),
        0
    );
    assert_eq!(
        dead("name = 'r'\nwhen = [['?p', 'a', 'person'], ['?p', 'state', 'gone']]\nthen = ['?p', 'a', 'm']").len(),
        1
    );
    assert_eq!(dead("name = 'r'\nwhen = [['?p', 'member_of', 'person:x']]\nthen = ['?p', 'a', 'm']").len(), 1);
    assert!(dead("name = 'r'\nwhen = [['?p', 'member_of', 'team:x']]\nthen = ['?p', 'a', 'm']").is_empty());
    // Relations other rules derive are not constrained.
    assert!(dead("name = 'r'\nchain = ['funds', 'r']\ninfer = 'r'").is_empty());
}

/// Rules may derive links of a declared link type outside its from/to lists,
/// and put any entity in a class: lint must not call such rules dead when
/// the reasoner fires them.
#[test]
fn dead_rules_account_for_facts_other_rules_derive() {
    let base = r#"
[object_types.org]
description = "An organization."
[object_types.person]
description = "A person."
[link_types.employs]
description = "Employment."
from = ["org"]
to = ["person"]
inverse = "employed_by"
[link_types.mentors]
description = "Mentoring."
from = ["person"]
to = ["person"]
inverse = "mentored_by"
"#;
    let mut g = Graph::default();
    for p in ["a", "b", "c"] {
        g.put_entity(entity("person", p));
    }
    g.put_link(link("person:a", "mentors", "person:b"));
    g.put_link(link("person:b", "mentors", "person:c"));
    let check = |rules: &str, dead_rule: Option<&str>| {
        let s = Schema::from_toml(&format!("{base}\n{rules}")).unwrap();
        let r = lint::lint_checks(&s, None, &all, Some(&["dead_rule".into()])).unwrap();
        let dead: Vec<&str> = r.findings.iter().map(|f| f.target.as_str()).collect();
        let fired = reason::closure(&s, &g, &ReasonOptions::default(), &all).stats().by_rule;
        match dead_rule {
            None => assert!(dead.is_empty(), "{dead:?} {fired:?}"),
            Some(name) => {
                assert_eq!(dead, [format!("rule:{name}")], "{fired:?}");
                assert_eq!(fired.get(&format!("schema/{name}")).copied().unwrap_or(0), 0, "{fired:?}");
            }
        }
        // Every rule lint keeps alive here does fire on the graph.
        for rule in &s.rules {
            if dead_rule != Some(rule.name.as_str()) {
                assert!(
                    fired.get(&format!("schema/{}", rule.name)).copied().unwrap_or(0) > 0,
                    "{}: {fired:?}",
                    rule.name
                );
            }
        }
    };
    // A chain rule derives employs between people, so employs twice fires.
    check(
        r#"
[[rules]]
name = "mentor_employs"
description = "x"
chain = ["mentors"]
infer = "employs"
[[rules]]
name = "employs_twice"
description = "x"
chain = ["employs", "employs"]
infer = "grand_employs"
"#,
        None,
    );
    // A Horn rule puts people in class org, so orgs_employ fires on people.
    check(
        r#"
[[rules]]
name = "employer_people"
description = "x"
when = [["?p", "mentors", "?q"]]
then = ["?p", "a", "org"]
[[rules]]
name = "orgs_employ"
description = "x"
when = [["?p", "a", "org"], ["?p", "mentors", "?q"]]
then = ["?p", "a", "boss"]
"#,
        None,
    );
    // Without the deriving rule the same rule is dead, and does not fire.
    check(
        r#"
[[rules]]
name = "employs_twice"
description = "x"
chain = ["employs", "employs"]
infer = "grand_employs"
"#,
        Some("employs_twice"),
    );
    // A rule that only a dead rule would enable stays dead (least fixpoint).
    let s = Schema::from_toml(&format!(
        "{base}\n{}",
        r#"
[[rules]]
name = "never"
description = "x"
chain = ["employs", "employs"]
infer = "mentors"
[[rules]]
name = "after_never"
description = "x"
when = [["?o", "a", "org"], ["?o", "mentored_by", "?p"]]
then = ["?o", "a", "boss"]
"#
    ))
    .unwrap();
    let r = lint::lint_checks(&s, None, &all, Some(&["dead_rule".into()])).unwrap();
    let dead: Vec<&str> = r.findings.iter().map(|f| f.target.as_str()).collect();
    // after_never would fire if never derived mentors from orgs; never cannot fire.
    assert_eq!(dead, ["rule:after_never", "rule:never"], "{:#?}", r.findings);
}

/// A schema that never went through `Schema::compile` (deserialized) lints
/// like the compiled one: inverse names resolve, nothing is misread.
#[test]
fn uncompiled_schema_resolves_inverse_names() {
    let text = r#"
[object_types.org]
description = "An organization."
[object_types.person]
description = "A person."
[link_types.employs]
description = "Employment."
from = ["org"]
to = ["person"]
inverse = "employed_by"
[[rules]]
name = "colleagues"
description = "x"
chain = ["employed_by", "employs"]
infer = "colleague_of"
[shapes.p]
description = "x"
target = "person"
[[shapes.p.properties]]
path = "employed_by"
min_count = 1
"#;
    let compiled = Schema::from_toml(text).unwrap();
    let raw: Schema = serde_json::from_value(serde_json::to_value(&compiled).unwrap()).unwrap();
    let a = lint::lint(&compiled, None, &all);
    let b = lint::lint(&raw, None, &all);
    assert!(!a.findings.iter().any(|f| f.check == "shape_reference"), "{:#?}", a.findings);
    assert_eq!(a, b);
    assert!(b.partial.is_empty(), "{:?}", b.partial);

    // A schema that does not compile at all still resolves inverse names,
    // and the rule checks say which rules they left out.
    let mut broken = raw.clone();
    broken.link_types.get_mut("employs").unwrap().weight = 0.0;
    assert!(broken.clone().compile().is_err());
    let r = lint::lint(&broken, None, &all);
    assert!(!r.findings.iter().any(|f| f.check == "shape_reference"), "{:#?}", r.findings);
    assert!(!r.findings.iter().any(|f| f.check == "keyword_collision"), "{:#?}", r.findings);
    let partial: Vec<&str> = r.partial.iter().map(|n| n.check.as_str()).collect();
    assert_eq!(partial, ["dead_rule", "recursive_rule"], "{:?}", r.partial);
    assert!(r.partial.iter().all(|n| n.reason.contains("rule colleagues does not compile")), "{:?}", r.partial);
}

#[test]
fn recursion_through_several_rules_is_found() {
    let s = Schema::from_toml(
        r#"
[object_types.node]
description = "A node."
[link_types.edge]
description = "An edge."
from = ["node"]
to = ["node"]
inverse = "edge_of"
[[rules]]
name = "a_rule"
description = "x"
chain = ["edge", "hop"]
infer = "reach"
[[rules]]
name = "b_rule"
description = "x"
when = [["?x", "reach", "?y"], ["?x", "a", "special"]]
then = ["?x", "hop", "?y"]
[[rules]]
name = "special"
description = "x"
when = [["?x", "a", "node"]]
then = ["?x", "a", "special"]
"#,
    )
    .unwrap();
    let r = lint::lint_checks(&s, None, &all, Some(&["recursive_rule".into()])).unwrap();
    let got: BTreeMap<&str, &str> = r.findings.iter().map(|f| (f.target.as_str(), f.message.as_str())).collect();
    assert_eq!(got.len(), 2, "{got:#?}");
    assert!(got["rule:a_rule"].contains("a_rule -> b_rule -> a_rule"), "{got:#?}");
    assert!(got["rule:b_rule"].contains("b_rule -> a_rule -> b_rule"), "{got:#?}");
}

fn admin() -> Principal {
    Principal::admin("ada")
}

fn world(o: &Ontology) -> ActionContext {
    let ctx = ActionContext::new(admin(), "acme");
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("team", "payments", "Payments Team")),
            Mutation::Upsert(EntityInput::new("person", "kenji", "Kenji Tanaka").link("member_of", "team:payments")),
        ],
    )
    .unwrap();
    ctx
}

fn feedback_suite(o: &Ontology) {
    let ctx = world(o);
    let before = o.schema_lint(&admin(), LintQuery::default()).unwrap();
    assert_eq!(before.schema_version, Some(o.schema_version()));
    assert_eq!(before.suppressed, 0);
    assert_eq!(before.means, lint::MEANS);
    // The graph counts: team and person are in use, task is not.
    let unused: Vec<&str> =
        before.findings.iter().filter(|f| f.check == "unused_type").map(|f| f.target.as_str()).collect();
    assert!(!unused.contains(&"object_type:team") && unused.contains(&"object_type:task"), "{unused:?}");
    let pick = |check: &str| before.findings.iter().find(|f| f.check == check).cloned().unwrap();
    let dismissed = pick("unused_type");
    let accepted = pick("unconstrained_link");

    let fb = o
        .lint_feedback(&ctx, &dismissed.id, LintDecision::Dismiss, Some("  data arrives next quarter ".into()))
        .unwrap();
    assert_eq!(
        (fb.check.as_str(), fb.target.as_str(), fb.by.as_str()),
        ("unused_type", dismissed.target.as_str(), "ada")
    );
    assert_eq!(fb.note.as_deref(), Some("data arrives next quarter"));
    o.lint_feedback(&ctx, &accepted.id.to_uppercase(), LintDecision::Accept, None).unwrap();

    let after = o.schema_lint(&admin(), LintQuery::default()).unwrap();
    assert_eq!(after.suppressed, 1);
    assert_eq!(after.findings.len(), before.findings.len() - 1);
    assert!(after.findings.iter().all(|f| f.id != dismissed.id), "dismissed findings are hidden by default");
    let acc = after.findings.iter().find(|f| f.id == accepted.id).unwrap();
    assert!(acc.accepted && !acc.dismissed && acc.note.is_none());
    let total = |r: &lint::LintReport| r.counts.error + r.counts.warning + r.counts.info;
    assert_eq!(total(&after), after.findings.len());

    let all_of = o.schema_lint(&admin(), LintQuery { include_dismissed: true, ..Default::default() }).unwrap();
    assert_eq!(all_of.suppressed, 0);
    let d = all_of.findings.iter().find(|f| f.id == dismissed.id).unwrap();
    assert!(d.dismissed && d.note.as_deref() == Some("data arrives next quarter"));

    // Selecting checks.
    let only = o
        .schema_lint(&admin(), LintQuery { checks: Some(vec!["unused_type".into()]), include_dismissed: false })
        .unwrap();
    assert_eq!(only.checks_run, ["unused_type"]);
    assert!(only.findings.iter().all(|f| f.check == "unused_type") && only.suppressed == 1);
    assert!(matches!(
        o.schema_lint(&admin(), LintQuery { checks: Some(vec!["bogus".into()]), ..Default::default() }),
        Err(Error::Invalid(_))
    ));

    // The latest decision wins: accept the dismissed one again.
    o.lint_feedback(&ctx, &dismissed.id, LintDecision::Accept, None).unwrap();
    let again = o.schema_lint(&admin(), LintQuery::default()).unwrap();
    assert_eq!(again.suppressed, 0);
    assert!(again.findings.iter().any(|f| f.id == dismissed.id && f.accepted));
    o.lint_feedback(&ctx, &dismissed.id, LintDecision::Dismiss, None).unwrap();

    let entries = o.lint_feedback_entries(&admin()).unwrap();
    assert_eq!(entries.len(), 2);
    let stored: BTreeMap<&str, LintDecision> = entries.iter().map(|e| (e.id.as_str(), e.decision)).collect();
    assert_eq!(stored[dismissed.id.as_str()], LintDecision::Dismiss);
    assert_eq!(stored[accepted.id.as_str()], LintDecision::Accept);
    assert!(o.store().kv_get(NS_FEEDBACK, &accepted.id).unwrap().is_some());

    // Audited.
    let events =
        o.events(&admin(), &EventFilter { action: Some(lint::FEEDBACK_ACTION.into()), ..Default::default() }).unwrap();
    assert_eq!(events.len(), 4, "{events:#?}");
    assert!(events.iter().any(|e| e.payload["id"] == dismissed.id.as_str() && e.payload["decision"] == "dismiss"));

    // Unknown and malformed ids, long notes.
    assert!(matches!(o.lint_feedback(&ctx, "0123456789ab", LintDecision::Dismiss, None), Err(Error::NotFound(_))));
    assert!(matches!(o.lint_feedback(&ctx, "nope", LintDecision::Dismiss, None), Err(Error::Invalid(_))));
    let long = "x".repeat(lint::MAX_NOTE_CHARS + 1);
    assert!(matches!(o.lint_feedback(&ctx, &accepted.id, LintDecision::Accept, Some(long)), Err(Error::Invalid(_))));

    // Write access is required, and refusals are audited.
    let viewer = ActionContext::new(Principal::viewer("vic", "acme"), "acme");
    assert!(matches!(o.lint_feedback(&viewer, &accepted.id, LintDecision::Dismiss, None), Err(Error::Denied(_))));
    let outsider = ActionContext::new(Principal::member("mo", "other"), "acme");
    assert!(matches!(o.lint_feedback(&outsider, &accepted.id, LintDecision::Dismiss, None), Err(Error::Denied(_))));
    let denied =
        o.events(&admin(), &EventFilter { action: Some("authorization.denied".into()), ..Default::default() }).unwrap();
    assert!(denied.iter().filter(|e| e.intent.as_deref() == Some(lint::FEEDBACK_ACTION)).count() >= 2, "{denied:#?}");
    // A viewer may read the report, with the feedback applied.
    let seen = o.schema_lint(&Principal::viewer("vic", "acme"), LintQuery::default()).unwrap();
    assert_eq!(seen.suppressed, 1);
}

#[test]
fn feedback_round_trip_in_memory() {
    let o = Ontology::in_memory(Schema::enterprise());
    feedback_suite(&o);
}

#[test]
fn feedback_round_trip_on_postgres() {
    let Some(pg) = common::TestTenant::new("lint") else { return };
    {
        let o = pg.open(Schema::enterprise(), OntologyOptions::default());
        feedback_suite(&o);
    }
    // Feedback survives a restart.
    let o = pg.open(Schema::enterprise(), OntologyOptions::default());
    let r = o.schema_lint(&admin(), LintQuery::default()).unwrap();
    assert_eq!(r.suppressed, 1);
    assert_eq!(o.lint_feedback_entries(&admin()).unwrap().len(), 2);
}

#[test]
fn keyword_named_inverses_are_warnings() {
    let s = Schema::from_toml(
        r#"
[object_types.k]
description = "A kind."
[link_types.typed_as]
description = "x"
from = ["k"]
to = ["k"]
inverse = "a"
[[rules]]
name = "r"
description = "x"
chain = ["typed_as"]
infer = "extends"
"#,
    )
    .unwrap();
    let r = lint::lint_checks(&s, None, &all, Some(&["keyword_collision".into()])).unwrap();
    let got: Vec<(Severity, &str)> = r.findings.iter().map(|f| (f.severity, f.target.as_str())).collect();
    assert_eq!(got, [(Severity::Error, "rule:r"), (Severity::Warning, "link_type:typed_as")], "{:#?}", r.findings);
    assert!(r.findings[1].message.contains("means the inverse of typed_as"), "{:#?}", r.findings);
}
