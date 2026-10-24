//! Shape reports: every constraint component, paths, targets, inferred mode,
//! the three verdicts, the structural check, induction and deltas.

mod common;

use std::collections::BTreeMap;

use serde_json::{Value, json};

use onto_core::reason::{self, ReasonOptions};
use onto_core::shapes::{self, ShapeInduceOptions, ShapeIssueKind, ShapeReport, ShapeVerdict, ShapesQuery};
use onto_core::{
    ActionContext, EntityInput, Mutation, Ontology, OntologyOptions, Principal, Schema, SchemaChangeSet, Shape,
    ShapeSeverity,
};

const EXTRA: &str = r#"
[object_types.incident]
extends = "issue"

[[rules]]
name = "urgent"
description = "Triaged high-priority work is urgent."
when = [["?i", "a", "work_item"], ["?i", "state", "triaged"], ["?i", "prop.priority", "high"]]
then = ["?i", "a", "urgent_work"]

[[rules]]
name = "on_call"
description = "Whoever is assigned urgent work is on call."
when = [["?i", "a", "urgent_work"], ["?i", "assigned_to", "?p"]]
then = ["?p", "a", "on_call"]
"#;

fn admin() -> Principal {
    Principal::admin("ada")
}

fn schema() -> Schema {
    let mut s = Schema::enterprise();
    s.extend_toml(EXTRA).unwrap();
    s
}

fn seed() -> Vec<Mutation> {
    vec![
        Mutation::Upsert(EntityInput::new("team", "payments", "Payments Team")),
        Mutation::Upsert(
            EntityInput::new("person", "kenji", "Kenji Tanaka")
                .prop("email", "kenji@example.com")
                .prop("role", "engineer")
                .link("member_of", "team:payments"),
        ),
        Mutation::Upsert(
            EntityInput::new("person", "aiko", "Aiko Sato")
                .prop("email", "aiko@example.com")
                .link("member_of", "team:payments")
                .link("relates_to", "person:kenji"),
        ),
        Mutation::Upsert(EntityInput::new("person", "mia", "Mia Chen").prop("email", "mia-at-example")),
        Mutation::Upsert(EntityInput::new("project", "alpha", "Project Alpha").state("active")),
        Mutation::Upsert(EntityInput::new("project", "beta", "Project Beta").state("planned")),
        Mutation::Upsert(EntityInput::new("service", "api", "Payments API")),
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
                .link("assigned_to", "person:aiko"),
        ),
        Mutation::Upsert(
            EntityInput::new("issue", "3", "Slow")
                .state("open")
                .prop("priority", "medium")
                .link("belongs_to", "project:beta")
                .link("assigned_to", "person:mia"),
        ),
        Mutation::Upsert(
            EntityInput::new("task", "t1", "Write the fix")
                .state("todo")
                .prop("estimate", 3.5)
                .link("belongs_to", "project:alpha")
                .link("part_of", "issue:1"),
        ),
        Mutation::Upsert(
            EntityInput::new("task", "t2", "Big migration")
                .state("todo")
                .prop("estimate", 12)
                .link("belongs_to", "project:beta"),
        ),
    ]
}

fn world() -> (Ontology, ActionContext) {
    let o = Ontology::in_memory(schema());
    let ctx = ActionContext::new(admin(), "acme");
    o.apply(&ctx, seed()).unwrap();
    (o, ctx)
}

fn drafts(v: Value) -> BTreeMap<String, Shape> {
    serde_json::from_value(v).expect("shapes")
}

fn query(v: Value, inferred: bool) -> ShapesQuery {
    let drafts = drafts(v);
    ShapesQuery { shapes: drafts.keys().cloned().collect(), inferred, drafts, ..Default::default() }
}

fn run(o: &Ontology, v: Value) -> ShapeReport {
    o.shapes_validate(&admin(), &query(v, false)).unwrap()
}

fn run_inferred(o: &Ontology, v: Value) -> ShapeReport {
    o.shapes_validate(&admin(), &query(v, true)).unwrap()
}

/// `(focus, component)` of every result.
fn hits(r: &ShapeReport) -> Vec<(String, String)> {
    r.results.iter().map(|x| (x.focus.clone(), x.component.clone())).collect()
}

fn hit(focus: &str, component: &str) -> (String, String) {
    (focus.to_string(), component.to_string())
}

fn conforms(r: &ShapeReport) {
    assert_eq!(r.verdict, ShapeVerdict::Conforms, "{r:#?}");
    assert_eq!(r.conforms, Some(true));
    assert!(r.results.is_empty() && r.skipped.is_empty(), "{r:#?}");
}

fn violates(r: &ShapeReport, expected: &[(&str, &str)]) {
    assert_eq!(r.verdict, ShapeVerdict::Violates, "{r:#?}");
    assert_eq!(r.conforms, Some(false));
    let want: Vec<(String, String)> = expected.iter().map(|(f, c)| hit(f, c)).collect();
    assert_eq!(hits(r), want, "{r:#?}");
}

fn one(target: &str, prop: Value) -> Value {
    json!({ "s": { "target": target, "properties": [prop] } })
}

#[test]
fn min_and_max_count() {
    let (o, _) = world();
    conforms(&run(&o, one("issue", json!({ "path": "belongs_to", "min_count": 1, "max_count": 1 }))));
    violates(
        &run(&o, one("issue", json!({ "path": "affects", "min_count": 1 }))),
        &[("issue:2", "min_count"), ("issue:3", "min_count")],
    );
    // has_item is belongs_to walked backwards: alpha holds three items.
    let r = run(&o, one("project", json!({ "path": "has_item", "max_count": 2 })));
    violates(&r, &[("project:alpha", "max_count")]);
    assert_eq!(r.results[0].path.as_deref(), Some("has_item"));
    assert_eq!(r.focus_by_shape["s"], 2);
    assert!(r.results[0].message.contains("3 has_item value(s), more than max_count 2"), "{}", r.results[0].message);
}

#[test]
fn class_by_kind_interface_and_ancestor() {
    let (o, _) = world();
    conforms(&run(&o, one("issue", json!({ "path": "assigned_to", "class": "person" }))));
    conforms(&run(&o, one("task", json!({ "path": "part_of", "class": "work_item" }))));
    let r = run(&o, one("issue", json!({ "path": "assigned_to", "class": "team" })));
    violates(&r, &[("issue:1", "class"), ("issue:2", "class"), ("issue:3", "class")]);
    assert_eq!(r.results[0].value, Some(json!("person:kenji")));
    // A literal is not an object of any class.
    violates(
        &run(&o, one("task", json!({ "path": "prop.estimate", "class": "task" }))),
        &[("task:t1", "class"), ("task:t2", "class")],
    );
}

#[test]
fn subtypes_are_targets_and_instances() {
    let (o, ctx) = world();
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(
                EntityInput::new("incident", "9", "Outage").state("open").link("belongs_to", "project:alpha"),
            ),
            Mutation::Upsert(EntityInput::new("task", "t3", "Mitigate").state("todo").link("part_of", "incident:9")),
        ],
    )
    .unwrap();
    let r = run(&o, one("issue", json!({ "path": "belongs_to", "min_count": 1 })));
    assert_eq!(r.focus_by_shape["s"], 4, "an incident is an issue: {r:#?}");
    conforms(&run(&o, one("task", json!({ "path": "part_of", "class": "issue" }))));
    violates(&run(&o, one("task", json!({ "path": "part_of", "class": "incident" }))), &[("task:t1", "class")]);
    let ind = o.shapes_induce(&admin(), "task", &ShapeInduceOptions::default()).unwrap();
    let part = ind.shape.properties.iter().find(|p| p.path == "part_of").expect("part_of");
    assert_eq!(part.class.as_deref(), Some("issue"), "the most specific common ancestor of issue and incident");
    assert!(
        ind.evidence.iter().any(|e| e.text == "all 2 objects reached by part_of are kinds of issue"),
        "{:#?}",
        ind.evidence
    );
}

#[test]
fn datatype() {
    let (o, _) = world();
    conforms(&run(&o, one("issue", json!({ "path": "prop.priority", "datatype": "enum" }))));
    conforms(&run(&o, one("task", json!({ "path": "prop.estimate", "datatype": "float" }))));
    let r = run(&o, one("task", json!({ "path": "prop.estimate", "datatype": "int" })));
    violates(&r, &[("task:t1", "datatype")]);
    assert_eq!(r.results[0].value, Some(json!(3.5)));
    violates(
        &run(&o, one("issue", json!({ "path": "belongs_to", "datatype": "string" }))),
        &[("issue:1", "datatype"), ("issue:2", "datatype"), ("issue:3", "datatype")],
    );
}

#[test]
fn in_and_has_value() {
    let (o, _) = world();
    conforms(&run(&o, one("issue", json!({ "path": "prop.priority", "in": ["low", "medium", "high"] }))));
    let r = run(&o, one("issue", json!({ "path": "prop.priority", "in": ["low", "high"] })));
    violates(&r, &[("issue:3", "in")]);
    assert_eq!(r.results[0].value, Some(json!("medium")));
    // Numbers compare by value.
    conforms(&run(&o, one("task", json!({ "path": "prop.estimate", "in": [3.5, 12.0] }))));
    let ids =
        json!({ "s": { "target_ids": ["issue:1"], "properties": [{ "path": "state", "has_value": "triaged" }] } });
    conforms(&run(&o, ids));
    let r = run(&o, one("issue", json!({ "path": "state", "has_value": "open" })));
    violates(&r, &[("issue:1", "has_value")]);
    assert_eq!(r.results[0].value, Some(json!("open")));
}

#[test]
fn pattern_and_lengths() {
    let (o, _) = world();
    let email = |p: &str| one("person", json!({ "path": "prop.email", "pattern": p }));
    conforms(&run(&o, email("example")));
    violates(&run(&o, email("^[^@]+@[^@]+$")), &[("person:mia", "pattern")]);
    conforms(&run(&o, one("issue", json!({ "path": "title", "min_length": 4, "max_length": 15 }))));
    violates(&run(&o, one("issue", json!({ "path": "title", "min_length": 5 }))), &[("issue:3", "min_length")]);
    violates(&run(&o, one("issue", json!({ "path": "title", "max_length": 12 }))), &[("issue:1", "max_length")]);
}

#[test]
fn value_ranges() {
    let (o, _) = world();
    conforms(&run(&o, one("task", json!({ "path": "prop.estimate", "min_value": 0, "max_value": 20 }))));
    violates(&run(&o, one("task", json!({ "path": "prop.estimate", "max_value": 10 }))), &[("task:t2", "max_value")]);
    violates(&run(&o, one("task", json!({ "path": "prop.estimate", "min_value": 4 }))), &[("task:t1", "min_value")]);
    // A string is not in any numeric range.
    violates(
        &run(&o, one("issue", json!({ "path": "prop.priority", "min_value": 0 }))),
        &[("issue:1", "min_value"), ("issue:2", "min_value"), ("issue:3", "min_value")],
    );
}

#[test]
fn inverse_and_sequence_paths() {
    let (o, _) = world();
    // works_on is the declared inverse of assigned_to; ^assigned_to is the same walk.
    for path in ["works_on", "^assigned_to"] {
        conforms(&run(&o, one("person", json!({ "path": path, "min_count": 1, "class": "issue" }))));
    }
    violates(
        &run(&o, one("person", json!({ "path": "works_on", "max_count": 0 }))),
        &[("person:aiko", "max_count"), ("person:kenji", "max_count"), ("person:mia", "max_count")],
    );
    let r = run(&o, one("issue", json!({ "path": "assigned_to.member_of", "min_count": 1, "class": "team" })));
    violates(&r, &[("issue:3", "min_count")]);
    let r = run(&o, one("issue", json!({ "path": "has_part.belongs_to", "has_value": "project:alpha" })));
    violates(&r, &[("issue:2", "has_value"), ("issue:3", "has_value")]);
    // relates_to is symmetric: one stored link reads from both ends.
    conforms(&run(
        &o,
        json!({ "s": { "target_ids": ["person:kenji", "person:aiko"],
        "properties": [{ "path": "relates_to", "min_count": 1, "max_count": 1 }] } }),
    ));
}

#[test]
fn targets_by_relation() {
    let (o, _) = world();
    let r = run(
        &o,
        json!({ "s": { "target_subjects_of": "affects", "properties": [{ "path": "state", "in": ["open"] }] } }),
    );
    violates(&r, &[("issue:1", "in")]);
    assert_eq!(r.focus_by_shape["s"], 1);
    let r = run(
        &o,
        json!({ "s": { "target_objects_of": "assigned_to", "properties": [{ "path": "member_of", "min_count": 1 }] } }),
    );
    violates(&r, &[("person:mia", "min_count")]);
    assert_eq!(r.focus_by_shape["s"], 3);
    // Subjects of an inverse name are the stored objects.
    let r = run(&o, json!({ "s": { "target_subjects_of": "works_on", "properties": [] } }));
    assert_eq!(r.focus_by_shape["s"], 3, "{r:#?}");
    conforms(&r);
}

#[test]
fn target_ids_missing_is_a_violation() {
    let (o, _) = world();
    let r = run(
        &o,
        json!({ "s": { "target_ids": ["issue:1", "issue:999"], "properties": [{ "path": "belongs_to", "min_count": 1 }] } }),
    );
    violates(&r, &[("issue:999", "target_missing")]);
    assert_eq!(r.results[0].constraint_id, "s/target");
    assert!(r.unmatched_targets.is_empty());
    // Only missing ids: still a violation, never an empty target.
    let r = run(&o, json!({ "s": { "target_ids": ["issue:999"] } }));
    violates(&r, &[("issue:999", "target_missing")]);
    assert_eq!(r.focus_nodes, 0);
}

#[test]
fn closed_shapes() {
    let (o, _) = world();
    let closed = |ignored: Value| {
        json!({ "s": { "target": "person", "closed": true, "ignored_properties": ignored,
            "properties": [{ "path": "prop.email", "min_count": 1 }] } })
    };
    let r = run(&o, closed(json!([])));
    violates(&r, &[("person:kenji", "closed")]);
    assert_eq!(r.results[0].path.as_deref(), Some("prop.role"));
    assert_eq!(r.results[0].value, Some(json!("engineer")));
    assert_eq!(r.results[0].constraint_id, "s/closed");
    conforms(&run(&o, closed(json!(["role"]))));
    conforms(&run(&o, closed(json!(["prop.role"]))));
}

#[test]
fn node_nesting() {
    let (o, _) = world();
    let nested = json!({
        "s": { "target": "issue", "properties": [{ "path": "assigned_to", "node": "member" }] },
        "member": { "properties": [{ "path": "member_of", "min_count": 1, "class": "team" }] }
    });
    let r = run(&o, nested);
    violates(&r, &[("issue:3", "node")]);
    assert_eq!(r.results[0].value, Some(json!("person:mia")));
    assert!(r.results[0].message.contains("does not conform to shape member"), "{}", r.results[0].message);
    // A shape without a target is not counted as an unmatched target.
    assert!(r.unmatched_targets.is_empty() && !r.focus_by_shape.contains_key("member"), "{r:#?}");
    // Recursion through a symmetric relation is cut, not looped.
    let peers = json!({ "peer": { "target_ids": ["person:kenji"],
        "properties": [{ "path": "relates_to", "min_count": 1, "node": "peer" }] } });
    conforms(&run(&o, peers));
}

#[test]
fn node_depth_is_bounded() {
    let o = Ontology::in_memory(schema());
    let ctx = ActionContext::new(admin(), "acme");
    let mut muts = Vec::new();
    for i in (0..8).rev() {
        let mut t = EntityInput::new("task", format!("c{i}"), format!("Chain {i}"));
        if i < 7 {
            t = t.link("depends_on", format!("task:c{}", i + 1));
        }
        muts.push(Mutation::Upsert(t));
    }
    o.apply(&ctx, muts).unwrap();
    let r = run(
        &o,
        json!({
            "s": { "target_ids": ["task:c0"], "properties": [{ "path": "depends_on", "node": "next" }] },
            "next": { "properties": [{ "path": "depends_on", "node": "next" }] }
        }),
    );
    assert_eq!(r.verdict, ShapeVerdict::Undetermined, "{r:#?}");
    assert!(r.results.is_empty());
    assert!(r.skipped.iter().any(|s| s.key == "node" && s.reason.contains("deeper than 5")), "{:?}", r.skipped);
}

#[test]
fn inferred_targets_and_paths() {
    let (o, _) = world();
    // A derived class target needs the closure.
    let urgent =
        json!({ "s": { "target": "urgent_work", "properties": [{ "path": "handled_by_team", "min_count": 1 }] } });
    let plain = run(&o, urgent.clone());
    assert_eq!(plain.verdict, ShapeVerdict::Undetermined);
    assert_eq!(plain.conforms, None);
    assert!(plain.skipped.iter().any(|s| s.key == "target" && s.reason.contains("inferred")), "{:?}", plain.skipped);
    let r = run_inferred(&o, urgent);
    conforms(&r);
    assert_eq!(r.focus_by_shape["s"], 1);
    assert!(r.inferred);
    assert_eq!(r.fixpoint_reached, Some(true));

    // A derived relation path needs the closure too.
    let team = one("issue", json!({ "path": "handled_by_team", "min_count": 1, "class": "team" }));
    let plain = run(&o, team.clone());
    assert_eq!(plain.verdict, ShapeVerdict::Undetermined);
    assert!(plain.skipped.iter().any(|s| s.key == "path"), "{:?}", plain.skipped);
    violates(&run_inferred(&o, team), &[("issue:3", "min_count")]);

    // class with inferred: any class the closure gives the object.
    let on_call = one("issue", json!({ "path": "assigned_to", "class": "on_call" }));
    assert_eq!(run(&o, on_call.clone()).verdict, ShapeVerdict::Undetermined);
    violates(&run_inferred(&o, on_call), &[("issue:2", "class"), ("issue:3", "class")]);

    // Subjects of a derived relation.
    let r = run_inferred(&o, json!({ "s": { "target_subjects_of": "handled_by_team", "properties": [] } }));
    assert_eq!(r.focus_by_shape["s"], 2, "{r:#?}");
}

#[test]
fn partial_closure_is_undetermined() {
    let (o, _) = world();
    let q = query(one("person", json!({ "path": "works_on", "min_count": 1 })), true);
    let opts = ReasonOptions { max_rounds: 1, ..Default::default() };
    let r = o.read(|g, s| {
        let c = reason::closure(s, g, &opts, &|_| true);
        assert!(!c.fixpoint_reached());
        shapes::validate(s, g, Some(&c), &q, &|_| true)
    });
    assert_eq!(r.verdict, ShapeVerdict::Undetermined, "{r:#?}");
    assert_eq!(r.fixpoint_reached, Some(false));
    // Results under a lower bound are listed but decide nothing.
    let q = query(one("issue", json!({ "path": "affects", "min_count": 1 })), true);
    let r = o.read(|g, s| shapes::validate(s, g, Some(&reason::closure(s, g, &opts, &|_| true)), &q, &|_| true));
    assert_eq!((r.verdict, r.conforms), (ShapeVerdict::Undetermined, None), "{r:#?}");
    assert!(r.total_results > 0, "{r:#?}");
    assert!(r.means.contains("any results are listed but decide nothing"));
}

#[test]
fn unknown_keys_make_it_undetermined() {
    let (o, _) = world();
    let r = run(&o, one("issue", json!({ "path": "belongs_to", "min_count": 1, "qualified_min_count": 2 })));
    assert_eq!(r.verdict, ShapeVerdict::Undetermined, "{r:#?}");
    assert_eq!(r.conforms, None);
    assert_eq!(r.skipped.len(), 1);
    assert_eq!(r.skipped[0].key, "qualified_min_count");
    assert_eq!(r.skipped[0].constraint_id.as_deref(), Some("s/0"));
    // A result still wins: any result means violates.
    let r = run(&o, one("issue", json!({ "path": "affects", "min_count": 1, "qualified_min_count": 2 })));
    assert_eq!(r.verdict, ShapeVerdict::Violates);
    assert!(!r.skipped.is_empty());
    // Annotations constrain nothing and skip nothing.
    conforms(&run(&o, one("issue", json!({ "path": "belongs_to", "min_count": 1, "name": "project", "order": 1 }))));
}

#[test]
fn empty_target_is_undetermined() {
    let (o, _) = world();
    let r = run(&o, one("deployment", json!({ "path": "deploys", "min_count": 1 })));
    assert_eq!(r.verdict, ShapeVerdict::Undetermined, "{r:#?}");
    assert_eq!(r.unmatched_targets, vec!["s".to_string()]);
    assert_eq!(r.focus_nodes, 0);
    // One shape that matched nothing leaves the report undetermined, even
    // when another shape ran over focus nodes and produced no result.
    let mixed = run(
        &o,
        json!({
            "a": { "target": "issue", "properties": [{ "path": "belongs_to", "min_count": 1 }] },
            "b": { "target": "deployment", "properties": [{ "path": "deploys", "min_count": 1 }] }
        }),
    );
    assert_eq!((mixed.verdict, mixed.conforms), (ShapeVerdict::Undetermined, None), "{mixed:#?}");
    assert_eq!(mixed.unmatched_targets, vec!["b".to_string()]);
    assert!(mixed.results.is_empty() && mixed.focus_nodes == 3, "{mixed:#?}");
    assert!(mixed.means.contains("every selected shape with a target selected at least one focus node"));
    // No shapes at all answers nothing either.
    let none = o.shapes_validate(&admin(), &ShapesQuery::default()).unwrap();
    assert_eq!(none.verdict, ShapeVerdict::Undetermined);
}

#[test]
fn deactivated_shapes_are_skipped_without_doubt() {
    let (o, _) = world();
    let r = run(
        &o,
        json!({
            "s": { "target": "issue", "properties": [{ "path": "belongs_to", "min_count": 1 }] },
            "off": { "target": "issue", "deactivated": true, "properties": [{ "path": "affects", "min_count": 1 }] },
            "half": { "target": "issue", "properties": [{ "path": "affects", "min_count": 1, "deactivated": true }] }
        }),
    );
    assert_eq!(r.verdict, ShapeVerdict::Conforms, "{r:#?}");
    let keys: Vec<(&str, &str)> = r.skipped.iter().map(|s| (s.shape.as_str(), s.key.as_str())).collect();
    assert_eq!(keys, vec![("half", "deactivated"), ("off", "deactivated")]);
    assert!(!r.focus_by_shape.contains_key("off"));
}

#[test]
fn severity_counts_limit_and_determinism() {
    let (o, _) = world();
    let v = json!({ "s": { "target": "issue", "severity": "warning", "properties": [
        { "path": "affects", "min_count": 1 },
        { "path": "prop.priority", "in": ["high"], "severity": "info", "message": "only high" }
    ] } });
    let r = run(&o, v.clone());
    assert_eq!(r.verdict, ShapeVerdict::Violates, "warnings count as results");
    assert_eq!((r.counts.violation, r.counts.warning, r.counts.info), (0, 2, 2));
    let info: Vec<_> = r.results.iter().filter(|x| x.severity == ShapeSeverity::Info).collect();
    assert!(info.iter().all(|x| x.message == "only high"));
    let mut q = query(v.clone(), false);
    q.limit = 1;
    let cut = o.shapes_validate(&admin(), &q).unwrap();
    assert!(cut.truncated && cut.results.len() == 1 && cut.total_results == 4, "{cut:#?}");
    assert_eq!(cut.counts, r.counts);
    let again = run(&o, v);
    assert_eq!(serde_json::to_string(&r).unwrap(), serde_json::to_string(&again).unwrap());
    let first = &r.results[0];
    assert_eq!((first.focus.as_str(), first.constraint_id.as_str()), ("issue:2", "s/0"));
}

#[test]
fn scope_and_visibility() {
    let (o, ctx) = world();
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(
                EntityInput::new("issue", "secret", "Secret")
                    .state("open")
                    .prop("visibility", "restricted")
                    .link("belongs_to", "project:alpha"),
            ),
            Mutation::Upsert(EntityInput::new("issue", "elsewhere", "Elsewhere").state("open")),
        ],
    )
    .unwrap();
    let other = ActionContext::new(admin(), "other");
    o.apply(&other, vec![Mutation::Upsert(EntityInput::new("issue", "far", "Far away"))]).unwrap();
    let v = one("issue", json!({ "path": "belongs_to", "min_count": 1 }));
    let all = run(&o, v.clone());
    violates(&all, &[("issue:elsewhere", "min_count"), ("issue:far", "min_count")]);
    let mut q = query(v.clone(), false);
    q.workspace = Some("acme".into());
    let acme = o.shapes_validate(&admin(), &q).unwrap();
    violates(&acme, &[("issue:elsewhere", "min_count")]);
    let member = Principal::member("m", "acme");
    let mine = o.shapes_validate(&member, &query(v.clone(), false)).unwrap();
    assert_eq!(mine.focus_by_shape["s"], 4, "the restricted issue is not a focus node: {mine:#?}");
    let missing = json!({ "s": { "target_ids": ["issue:secret"] } });
    let r = o.shapes_validate(&member, &query(missing, false)).unwrap();
    assert_eq!(hits(&r), vec![hit("issue:secret", "target_missing")]);
    q.workspace = Some("other".into());
    assert!(o.shapes_validate(&member, &q).is_err());
    let unknown = ShapesQuery { shapes: vec!["nope".into()], ..Default::default() };
    assert!(matches!(o.shapes_validate(&admin(), &unknown), Err(onto_core::Error::NotFound(_))));
}

#[test]
fn check_reports_every_issue_kind() {
    let o = Ontology::in_memory(schema());
    let good = drafts(json!({
        "good": { "target": "issue", "properties": [
            { "path": "belongs_to", "min_count": 1, "class": "project" },
            { "path": "prop.priority", "in": ["low"], "name": "annotation" },
            { "path": "handled_by_team", "class": "team" }
        ] },
        "urgent": { "target": "urgent_work" }
    }));
    let c = o.shapes_check(&good);
    assert!(c.ok, "{:?}", c.issues);
    assert_eq!((c.shapes, c.constraints), (2, 3));
    let bad = drafts(json!({
        "Bad-Name": { "target": "spaceship", "target_subjects_of": "nope", "target_ids": [""], "properties": [
            { "id": "x", "path": "prop.nope" },
            { "id": "x", "path": "belongs_to.nowhere", "class": "unicorn", "node": "ghost" },
            { "path": "title", "pattern": "(", "min_count": 3, "max_count": 1, "min_length": 9, "max_length": 2 },
            { "path": "title", "qualified_min_count": 1 }
        ] },
        "task_props": { "target": "task", "properties": [{ "path": "prop.email" }] }
    }));
    let c = o.shapes_check(&bad);
    assert!(!c.ok);
    let kinds: std::collections::BTreeSet<ShapeIssueKind> = c.issues.iter().map(|i| i.issue).collect();
    for k in [
        ShapeIssueKind::MissingKind,
        ShapeIssueKind::MissingRelation,
        ShapeIssueKind::MissingProperty,
        ShapeIssueKind::MissingShape,
        ShapeIssueKind::UnknownKey,
        ShapeIssueKind::BadPattern,
        ShapeIssueKind::BadBounds,
        ShapeIssueKind::DuplicateId,
        ShapeIssueKind::BadTargetId,
        ShapeIssueKind::BadName,
    ] {
        assert!(kinds.contains(&k), "{k:?} not reported: {:#?}", c.issues);
    }
    let task = c.issues.iter().find(|i| i.shape == "task_props").expect("task_props issue");
    assert_eq!(task.issue, ShapeIssueKind::MissingProperty);
    assert!(task.detail.contains("task objects have no property email"), "{}", task.detail);
    assert_eq!(task.constraint_id.as_deref(), Some("task_props/0"));
    // An explicit id equal to another property's default id is a duplicate,
    // as compile sees it after normalization.
    let clash = drafts(json!({ "x": { "target": "issue", "properties": [
        { "id": "x/1", "path": "title" },
        { "path": "title" }
    ] } }));
    let c = o.shapes_check(&clash);
    assert!(!c.ok);
    assert!(c.issues.iter().any(|i| i.issue == ShapeIssueKind::DuplicateId), "{:#?}", c.issues);
    // work_item declares priority, so a work_item shape may read it.
    let iface = drafts(json!({ "w": { "target": "work_item", "properties": [{ "path": "prop.priority" }] } }));
    assert!(o.shapes_check(&iface).ok);
}

#[test]
fn induction_round_trip() {
    let (o, ctx) = world();
    let ind = o.shapes_induce(&admin(), "issue", &ShapeInduceOptions::default()).unwrap();
    assert_eq!((ind.kind.as_str(), ind.name.as_str(), ind.entities), ("issue", "issue_shape", 3));
    assert!(ind.means.contains("describes the data as it is, not what it should be"));
    let by_path: BTreeMap<&str, &onto_core::PropertyShape> =
        ind.shape.properties.iter().map(|p| (p.path.as_str(), p)).collect();
    let belongs = by_path["belongs_to"];
    assert_eq!((belongs.min_count, belongs.max_count, belongs.class.as_deref()), (Some(1), Some(1), Some("project")));
    assert_eq!(by_path["assigned_to"].class.as_deref(), Some("person"));
    // One of three issues affects a service: no min_count, no max_count (< 50%).
    let affects = by_path["affects"];
    assert_eq!((affects.min_count, affects.max_count, affects.class.as_deref()), (None, None, Some("service")));
    assert_eq!(by_path["prop.priority"].datatype, Some(onto_core::schema::PropType::Enum));
    assert!(by_path["prop.priority"].values.is_empty(), "3 distinct values in 3 rows is not an enum by evidence");
    assert!(by_path["has_part"].class.as_deref() == Some("task"));
    assert!(by_path.values().all(|p| p.pattern.is_none()));
    let texts: Vec<&str> = ind.evidence.iter().map(|e| e.text.as_str()).collect();
    assert!(texts.contains(&"3 of 3 issues have a belongs_to link"), "{texts:#?}");
    assert!(ind.evidence.iter().any(|e| e.constraint == "issue_shape/belongs_to min_count"), "{:#?}", ind.evidence);
    assert!(ind.evidence.iter().all(|e| !e.text.is_empty()));

    // The data satisfies the induced shape by construction.
    let mut q = ShapesQuery::default();
    q.drafts.insert(ind.name.clone(), ind.shape.clone());
    q.shapes = vec![ind.name.clone()];
    conforms(&o.shapes_validate(&admin(), &q).unwrap());
    assert!(o.shapes_check(&q.drafts).ok);

    // The change set saves it; the next object is then measured against it.
    let json = serde_json::to_value(&ind.change_set).unwrap();
    assert_eq!(json["changes"][0]["op"], "put_shape");
    let set: SchemaChangeSet = serde_json::from_value(json).unwrap();
    o.schema_apply(&ctx, set).unwrap();
    conforms(&o.shapes_validate(&admin(), &ShapesQuery::default()).unwrap());
    o.apply(&ctx, vec![Mutation::Upsert(EntityInput::new("issue", "4", "Orphan").state("open"))]).unwrap();
    let r = o.shapes_validate(&admin(), &ShapesQuery::default()).unwrap();
    assert_eq!(r.verdict, ShapeVerdict::Violates);
    assert!(hits(&r).contains(&hit("issue:4", "min_count")), "{r:#?}");
}

#[test]
fn induction_reads_enums_and_round_trips_at_scale() {
    let o = Ontology::in_memory(schema());
    let ctx = ActionContext::new(admin(), "acme");
    let mut muts = vec![Mutation::Upsert(EntityInput::new("project", "p", "P"))];
    for i in 0..20 {
        let state = if i % 2 == 0 { "todo" } else { "done" };
        let mut t = EntityInput::new("task", format!("{i}"), format!("Task {i}"))
            .state(state)
            .prop("estimate", (i % 4) as i64)
            .link("belongs_to", "project:p");
        if i % 5 != 0 {
            t = t.prop("visibility", "public");
        }
        muts.push(Mutation::Upsert(t));
    }
    o.apply(&ctx, muts).unwrap();
    let ind = o
        .shapes_induce(&admin(), "task", &ShapeInduceOptions { name: Some("tasks".into()), ..Default::default() })
        .unwrap();
    let by_path: BTreeMap<&str, &onto_core::PropertyShape> =
        ind.shape.properties.iter().map(|p| (p.path.as_str(), p)).collect();
    assert_eq!(by_path["state"].values, vec![json!("done"), json!("todo")]);
    assert_eq!(by_path["state"].min_count, Some(1));
    assert_eq!(by_path["prop.estimate"].values, vec![json!(0), json!(1), json!(2), json!(3)]);
    // Declared float, and every integer fits it: the declaration wins.
    assert_eq!(by_path["prop.estimate"].datatype, Some(onto_core::schema::PropType::Float));
    assert!(
        ind.evidence.iter().any(|e| e.text == "20 state values take 2 distinct values (done, todo)"),
        "{:#?}",
        ind.evidence
    );
    assert!(ind.notes.iter().any(|n| n.contains("visibility")), "{:?}", ind.notes);
    assert!(!by_path.contains_key("prop.visibility"));
    let mut q = ShapesQuery::default();
    q.drafts.insert(ind.name.clone(), ind.shape.clone());
    conforms(&o.shapes_validate(&admin(), &q).unwrap());
    assert!(o.shapes_induce(&admin(), "spaceship", &ShapeInduceOptions::default()).is_err());
    let none = o.shapes_induce(&admin(), "deployment", &ShapeInduceOptions::default()).unwrap();
    assert_eq!(none.entities, 0);
    assert!(none.notes.iter().any(|n| n.contains("nothing to induce")));
}

#[test]
fn induction_keeps_mixed_json_values_apart() {
    let mut schema = schema();
    schema.extend_toml("[object_types.widget]\nproperties.code = { type = \"json\" }\n").unwrap();
    let o = Ontology::in_memory(schema);
    let ctx = ActionContext::new(admin(), "acme");
    let codes = [json!(1), json!("1"), json!(2)];
    let muts = (0..15)
        .map(|i| {
            Mutation::Upsert(
                EntityInput::new("widget", format!("{i}"), format!("W {i}")).prop("code", codes[i % 3].clone()),
            )
        })
        .collect();
    o.apply(&ctx, muts).unwrap();
    let ind = o.shapes_induce(&admin(), "widget", &ShapeInduceOptions::default()).unwrap();
    let code = ind.shape.properties.iter().find(|p| p.path == "prop.code").expect("prop.code");
    assert_eq!(code.values, vec![json!(1), json!(2), json!("1")]);
    assert_eq!(code.datatype, None);
    assert!(ind.evidence.iter().any(|e| e.text.contains("3 distinct values (1, 2, \"1\")")), "{:#?}", ind.evidence);
    let mut q = ShapesQuery::default();
    q.drafts.insert(ind.name.clone(), ind.shape.clone());
    conforms(&o.shapes_validate(&admin(), &q).unwrap());
}

#[test]
fn delta_between_reports() {
    let (o, ctx) = world();
    let v = one("issue", json!({ "path": "assigned_to.member_of", "min_count": 1 }));
    let before = run(&o, v.clone());
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("person", "mia", "Mia Chen").link("member_of", "team:payments")),
            Mutation::Upsert(EntityInput::new("issue", "5", "New")),
        ],
    )
    .unwrap();
    let after = run(&o, v);
    let d = shapes::delta(&before, &after);
    assert_eq!(d.resolved.iter().map(|r| r.focus.as_str()).collect::<Vec<_>>(), vec!["issue:3"]);
    assert_eq!(d.new_results.iter().map(|r| r.focus.as_str()).collect::<Vec<_>>(), vec!["issue:5"]);
    assert_eq!((d.before, d.after), (ShapeVerdict::Violates, ShapeVerdict::Violates));
    assert!(d.complete);
}

#[test]
fn live_put_shape_then_validate() {
    let (o, ctx) = world();
    let put: SchemaChangeSet = serde_json::from_value(json!({
        "message": "issues should name the service they affect",
        "changes": [{ "op": "put_shape", "name": "issue_affects", "def": {
            "target": "issue", "severity": "warning",
            "properties": [{ "path": "affects", "min_count": 1, "class": "service" }]
        } }]
    }))
    .unwrap();
    o.schema_apply(&ctx, put).unwrap();
    let r = o.shapes_validate(&admin(), &ShapesQuery::default()).unwrap();
    violates(&r, &[("issue:2", "min_count"), ("issue:3", "min_count")]);
    assert_eq!(r.results[0].constraint_id, "issue_affects/0");
    assert_eq!(r.counts.warning, 2);
    // Drafts replace a saved shape of the same name without saving anything.
    let q = ShapesQuery {
        drafts: drafts(
            json!({ "issue_affects": { "target": "issue", "properties": [{ "path": "belongs_to", "min_count": 1 }] } }),
        ),
        ..Default::default()
    };
    conforms(&o.shapes_validate(&admin(), &q).unwrap());
    assert_eq!(o.schema().shapes["issue_affects"].properties[0].path, "affects");
}

#[test]
fn pg_live_put_shape_then_validate() {
    let Some(pg) = common::TestTenant::new("shapes") else { return };
    let o = pg.open(schema(), OntologyOptions::default());
    let ctx = ActionContext::new(admin(), "acme");
    o.apply(&ctx, seed()).unwrap();
    let ind = o.shapes_induce(&admin(), "issue", &ShapeInduceOptions::default()).unwrap();
    o.schema_apply(&ctx, ind.change_set).unwrap();
    drop(o);
    let o = pg.open(schema(), OntologyOptions::default());
    assert!(o.schema().shapes.contains_key("issue_shape"));
    conforms(&o.shapes_validate(&admin(), &ShapesQuery::default()).unwrap());
}
