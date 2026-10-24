//! Reasoner: closures, derivations, certificates, diffs, binding checks and
//! the schema additions it reads (transitive link types, Horn rules, shapes).

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{Value, json};

use onto_core::reason::{self, AtomPattern, Attested, BindingVerdict, Fact, ReasonOptions, Status};
use onto_core::{
    ActionContext, EntityInput, Error, Mutation, Ontology, Principal, Schema, SchemaChangeSet, live_schema::DiffKind,
};

fn set(v: Value) -> SchemaChangeSet {
    serde_json::from_value(v).expect("change set")
}

fn admin() -> Principal {
    Principal::admin("ada")
}

const EXTRA: &str = r#"
[object_types.incident]
extends = "issue"

[[rules]]
name = "team_scope"
description = "What the team handling an item owns is in its scope."
chain = ["handled_by_team", "owns"]
infer = "team_scope"

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

fn schema() -> Schema {
    let mut s = Schema::enterprise();
    s.extend_toml(EXTRA).unwrap();
    s
}

fn world() -> (Ontology, ActionContext) {
    let o = Ontology::in_memory(schema());
    let ctx = ActionContext::new(admin(), "acme");
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("team", "payments", "Payments Team")),
            Mutation::Upsert(EntityInput::new("person", "kenji", "Kenji Tanaka").link("member_of", "team:payments")),
            Mutation::Upsert(EntityInput::new("person", "aiko", "Aiko Sato").link("member_of", "team:payments")),
            Mutation::Upsert(EntityInput::new("person", "mia", "Mia Chen").link("manages", "team:payments")),
            Mutation::Upsert(EntityInput::new("project", "alpha", "Project Alpha").state("active")),
            Mutation::Upsert(EntityInput::new("service", "api", "Payments API")),
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
            Mutation::Upsert(EntityInput::new("task", "3", "Retry").state("todo").link("part_of", "issue:1")),
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
    let beta = ActionContext::new(admin(), "beta");
    o.apply(
        &beta,
        vec![
            Mutation::Upsert(EntityInput::new("issue", "b2", "Other tenant")),
            Mutation::Upsert(EntityInput::new("issue", "b1", "Other tenant").link("relates_to", "issue:b2")),
        ],
    )
    .unwrap();
    (o, ctx)
}

fn f(s: &str, p: &str, o: &str) -> Fact {
    Fact::new(s, p, o)
}

fn acme() -> ReasonOptions {
    ReasonOptions { workspace: Some("acme".into()), ..Default::default() }
}

fn derived_by(c: &reason::Closure, fact: &Fact) -> String {
    c.derivation(fact).unwrap_or_else(|| panic!("{fact} is not derived: {:?}", c.contains(fact))).rule.clone()
}

/// Every premise is asserted or the conclusion of an earlier line.
fn assert_premises_earlier(c: &reason::Closure) {
    let mut known: HashSet<&Fact> = c.asserted().iter().collect();
    for (i, d) in c.derived().iter().enumerate() {
        for p in &d.premises {
            assert!(known.contains(p), "line {}: premise {p} of {} is not known yet", i + 1, d.conclusion);
        }
        assert!(!known.contains(&d.conclusion), "line {}: {} was already known", i + 1, d.conclusion);
        known.insert(&d.conclusion);
    }
}

#[test]
fn kinds_inherit_and_interfaces_apply() {
    let (o, _) = world();
    let c = o.reason(&admin(), &acme()).unwrap();
    assert!(c.fixpoint_reached());
    assert_eq!(c.contains(&f("<incident:9>", "a", "#incident")), Some(Status::Asserted));
    assert_eq!(c.contains(&f("#incident", "extends", "#issue")), Some(Status::Asserted));
    assert_eq!(derived_by(&c, &f("<incident:9>", "a", "#issue")), "builtin/type_extends");
    assert_eq!(derived_by(&c, &f("<incident:9>", "a", "#work_item")), "builtin/type_implements");
    assert_eq!(derived_by(&c, &f("<issue:1>", "a", "#work_item")), "builtin/type_implements");
    assert_eq!(c.contains(&f("<person:kenji>", "a", "#work_item")), None);
    // An inference is not an assertion: the graph is untouched.
    assert!(o.read(|g, _| g.get("issue:1").is_some_and(|e| e.kind == "issue")));
    let s = c.stats();
    assert!(s.not_evaluated.iter().any(|n| n == "shapes"), "{:?}", s.not_evaluated);
    assert!(s.not_evaluated.iter().any(|n| n == "cardinality"));
    assert_eq!(s.refused, 0, "{:?}", s.refusals);
}

#[test]
fn transitive_closure_over_a_long_chain() {
    let mut s = schema();
    s.link_types.get_mut("depends_on").unwrap().transitive = true;
    s.compile().unwrap();
    let o = Ontology::in_memory(s);
    let ctx = ActionContext::new(admin(), "acme");
    let n = 64;
    let mut muts = Vec::new();
    for i in (0..n).rev() {
        let mut t = EntityInput::new("task", format!("{i:03}"), format!("Step {i}"));
        if i + 1 < n {
            t = t.link("depends_on", format!("task:{:03}", i + 1));
        }
        muts.push(Mutation::Upsert(t));
    }
    o.apply(&ctx, muts).unwrap();
    let c = o.reason(&admin(), &acme()).unwrap();
    let st = c.stats();
    assert!(st.fixpoint_reached);
    assert_eq!(st.by_rule["builtin/transitive"], n * (n - 1) / 2 - (n - 1));
    assert!(st.rounds >= 6, "{} rounds", st.rounds);
    let far = f("<task:000>", "depends_on", &format!("<task:{:03}>", n - 1));
    assert!(matches!(c.contains(&far), Some(Status::Derived { .. })));
    assert_eq!(c.contains(&f("<task:063>", "depends_on", "<task:000>")), None);
    assert_premises_earlier(&c);
}

#[test]
fn symmetric_links_hold_both_ways() {
    let (o, _) = world();
    let c = o.reason(&admin(), &acme()).unwrap();
    // Symmetric links are stored one way (whichever the engine picks); the
    // other direction is derived.
    let one = f("<issue:2>", "relates_to", "<issue:1>");
    let other = f("<issue:1>", "relates_to", "<issue:2>");
    let (stored, back) = if c.contains(&one) == Some(Status::Asserted) { (one, other) } else { (other, one) };
    assert_eq!(c.contains(&stored), Some(Status::Asserted));
    let d = c.derivation(&back).unwrap();
    assert_eq!(d.rule, "builtin/symmetric");
    assert_eq!(d.premises, vec![f("relates_to", "a", "symmetric"), stored]);
}

#[test]
fn chain_rules_follow_inverse_steps_and_other_rules() {
    let (o, _) = world();
    let c = o.reason(&admin(), &acme()).unwrap();
    assert_eq!(derived_by(&c, &f("<issue:1>", "handled_by_team", "<team:payments>")), "schema/responsible_team");
    // owned_by is the inverse of owns: the premise is stored the canonical way.
    let acc = c.derivation(&f("<issue:1>", "accountable_team", "<team:payments>")).unwrap();
    assert_eq!(acc.rule, "schema/accountable_team");
    assert_eq!(
        acc.premises,
        vec![f("<issue:1>", "affects", "<service:api>"), f("<team:payments>", "owns", "<service:api>")]
    );
    assert_eq!(derived_by(&c, &f("<issue:1>", "escalate_to", "<person:mia>")), "schema/escalation");
    assert_eq!(derived_by(&c, &f("<task:3>", "owning_team", "<team:payments>")), "schema/task_owning_team");
    // A chain step may be a relation another rule derives.
    let scope = c.derivation(&f("<issue:1>", "team_scope", "<project:alpha>")).unwrap();
    assert_eq!(scope.rule, "schema/team_scope");
    assert_eq!(scope.premises[0], f("<issue:1>", "handled_by_team", "<team:payments>"));
    let rules = c.rules();
    let line = rules.iter().find(|r| r.name == "schema/accountable_team").unwrap();
    assert_eq!(line.tsv(), "schema/accountable_team\t?x0\taccountable_team\t?x2\t?x0\taffects\t?x1\t?x2\towns\t?x1");
}

#[test]
fn horn_rules_read_state_and_properties() {
    let (o, _) = world();
    let c = o.reason(&admin(), &acme()).unwrap();
    let urgent = c.derivation(&f("<issue:1>", "a", "#urgent_work")).unwrap();
    assert_eq!(urgent.rule, "schema/urgent");
    assert_eq!(
        urgent.premises,
        vec![
            f("<issue:1>", "a", "#work_item"),
            f("<issue:1>", "state", "\"triaged\""),
            f("<issue:1>", "prop.priority", "\"high\""),
        ]
    );
    assert_eq!(c.contains(&f("<issue:2>", "a", "#urgent_work")), None, "issue 2 is open and low");
    // Rule-to-rule: on_call reads the class urgent derives.
    assert_eq!(derived_by(&c, &f("<person:kenji>", "a", "#on_call")), "schema/on_call");
    // Only the properties rule bodies name are read.
    assert!(c.asserted().iter().all(|f| !f.p.starts_with("prop.") || f.p == "prop.priority"));
    assert_premises_earlier(&c);
}

#[test]
fn derivations_cite_only_earlier_premises() {
    let (o, _) = world();
    let c = o.reason(&admin(), &ReasonOptions::default()).unwrap();
    assert!(c.derived().len() > 10);
    assert_premises_earlier(&c);
    // Line numbers are certificate lines.
    for (i, d) in c.derived().iter().enumerate() {
        assert_eq!(c.contains(&d.conclusion), Some(Status::Derived { line: i + 1 }));
    }
}

#[test]
fn certificates_are_deterministic_and_digested() {
    let attested = Attested { engine: "test".into(), created_at: "2026-10-24T12:00:00Z".into(), ..Default::default() };
    let (a, _) = world();
    let (b, _) = world();
    let ca = a.reason(&admin(), &acme()).unwrap().certificate(attested.clone()).unwrap();
    let cb = b.reason(&admin(), &acme()).unwrap().certificate(attested).unwrap();
    assert_eq!(ca, cb, "same input, same bytes");
    let ra = a.reason_certificate(&admin(), &acme()).unwrap();
    assert_eq!(ra.derivations_tsv, ca.derivations_tsv);
    assert!(ca.rules_tsv.starts_with(reason::BUILTIN_RULES));
    assert!(ca.rules_tsv.ends_with('\n') && ca.asserted_tsv.ends_with('\n') && ca.derivations_tsv.ends_with('\n'));
    let mut lines: Vec<&str> = ca.asserted_tsv.lines().collect();
    let before = lines.clone();
    lines.sort();
    lines.dedup();
    assert_eq!(lines, before, "asserted.tsv is sorted and unique");
    let m: Value = serde_json::from_str(&ra.manifest_json).unwrap();
    assert_eq!(m["format"], "oi-cert/1");
    assert_eq!(m["derived"]["asserted_sha256"], reason::sha256_hex(ra.asserted_tsv.as_bytes()));
    assert_eq!(m["derived"]["rules_sha256"], reason::sha256_hex(ra.rules_tsv.as_bytes()));
    assert_eq!(m["derived"]["derivations_sha256"], reason::sha256_hex(ra.derivations_tsv.as_bytes()));
    assert_eq!(m["derived"]["derivations"], ra.derivations_tsv.lines().count());
    assert_eq!(m["attested"]["workspace"], "acme");
    assert_eq!(m["attested"]["principal"], "ada");
    assert_eq!(m["attested"]["fixpoint_reached"], true);
    assert!(m["attested"]["engine"].as_str().unwrap().starts_with("onto-intelli "));
    assert!(m["means"].as_str().unwrap().contains("claims of the engine"));
    // The engine never speaks the checker's words.
    for (_, text) in ra.files() {
        assert!(!text.contains("entailed") && !text.contains("checked"));
    }
}

#[test]
fn viewers_see_no_facts_about_what_they_cannot_read() {
    let (o, _) = world();
    let everything = o.reason(&admin(), &ReasonOptions::default()).unwrap();
    let mentions = |c: &reason::Closure, id: &str| {
        c.asserted().iter().chain(c.derived().iter().map(|d| &d.conclusion)).any(|f| f.s == id || f.o == id)
    };
    assert!(mentions(&everything, "<issue:b1>") && mentions(&everything, "<knowledge:secret>"));
    let scoped = o.reason(&admin(), &acme()).unwrap();
    assert!(!mentions(&scoped, "<issue:b1>"), "the workspace scope leaves beta out");
    let viewer = Principal::viewer("vic", "acme");
    let c = o.reason(&viewer, &ReasonOptions::default()).unwrap();
    assert!(!mentions(&c, "<issue:b1>"), "other workspace");
    assert!(!mentions(&c, "<knowledge:secret>"), "restricted");
    for d in c.derived() {
        assert!(d.premises.iter().all(|p| p.s != "<knowledge:secret>" && p.s != "<issue:b1>"));
    }
    assert!(mentions(&c, "<issue:1>"));
    let aiko = Principal::viewer("aiko", "acme").with_entity("person:aiko");
    assert!(mentions(&o.reason(&aiko, &ReasonOptions::default()).unwrap(), "<knowledge:secret>"), "on the acl");
    let denied = o.reason(&viewer, &ReasonOptions { workspace: Some("beta".into()), ..Default::default() });
    assert!(matches!(denied, Err(Error::Denied(_))));
}

#[test]
fn limits_report_a_lower_bound() {
    let (o, _) = world();
    let full = o.reason(&admin(), &acme()).unwrap();
    let cut = o.reason(&admin(), &ReasonOptions { max_derived: 3, ..acme() }).unwrap();
    let st = cut.stats();
    assert!(!st.fixpoint_reached);
    assert_eq!(st.derived, 3);
    assert!(st.truncated.as_deref().unwrap().contains("max_derived"));
    assert_premises_earlier(&cut);
    let cert = o.reason_certificate(&admin(), &ReasonOptions { max_derived: 3, ..acme() }).unwrap();
    assert!(cert.manifest_json.contains("\"fixpoint_reached\": false"));
    let answer = o
        .reason_query(
            &admin(),
            &ReasonOptions { max_rounds: 1, ..acme() },
            &[vec![json!("?p"), json!("a"), json!("on_call")]],
            10,
        )
        .unwrap();
    assert!(!answer.fixpoint_reached && answer.bindings.is_empty(), "{answer:?}");
    assert!(full.fixpoint_reached());
}

#[test]
fn unsafe_ids_are_refused_not_spelled() {
    let (o, ctx) = world();
    o.upsert(&ctx, EntityInput::new("issue", "a>b", "Odd id").link("belongs_to", "project:alpha")).unwrap();
    let c = o.reason(&admin(), &acme()).unwrap();
    let st = c.stats();
    assert_eq!(st.refused, 1, "{:?}", st.refusals);
    assert!(st.refusals[0].reason.contains("'>'"), "{:?}", st.refusals);
    // The rest of the closure is still computed.
    assert!(c.contains(&f("<issue:1>", "handled_by_team", "<team:payments>")).is_some());
    assert!(c.asserted().iter().all(|f| !f.s.contains("a>b") && !f.o.contains("a>b")));
    let err = o.reason_certificate(&admin(), &acme()).unwrap_err();
    assert!(matches!(&err, Error::Invalid(m) if m.starts_with("certificate refused")), "{err}");
}

#[test]
fn queries_bind_variables() {
    let (o, _) = world();
    let atoms = vec![
        vec![json!("?i"), json!("handled_by_team"), json!("team:payments")],
        vec![json!("?i"), json!("a"), json!("urgent_work")],
    ];
    let a = o.reason_query(&admin(), &acme(), &atoms, 10).unwrap();
    assert!(a.fixpoint_reached);
    assert_eq!(a.bindings.len(), 1);
    assert_eq!(a.bindings[0]["i"], "<issue:1>");
    // Inverse names read the canonical relation backwards.
    let who = o
        .reason_query(&admin(), &acme(), &[vec![json!("team:payments"), json!("has_member"), json!("?p")]], 10)
        .unwrap();
    let people: Vec<&str> = who.bindings.iter().map(|b| b["p"].as_str()).collect();
    assert_eq!(people, vec!["<person:aiko>", "<person:kenji>"]);
    // Literals and variable predicates.
    let triaged =
        o.reason_query(&admin(), &acme(), &[vec![json!("?x"), json!("state"), json!("triaged")]], 10).unwrap();
    assert_eq!(triaged.bindings.len(), 1);
    let any = o
        .reason_query(&admin(), &acme(), &[vec![json!("issue:1"), json!("?rel"), json!("team:payments")]], 10)
        .unwrap();
    let rels: Vec<&str> = any.bindings.iter().map(|b| b["rel"].as_str()).collect();
    assert_eq!(rels, vec!["accountable_team", "handled_by_team", "owning_team"]);
    let bad = o.reason_query(&admin(), &acme(), &[vec![json!("?x"), json!("nope"), json!("?y")]], 10);
    assert!(matches!(bad, Err(Error::Invalid(m)) if m.contains("unknown relation nope")));
    // The same patterns, spelled, on the closure itself.
    let c = o.reason(&admin(), &acme()).unwrap();
    let p = AtomPattern { s: "?x".into(), p: "a".into(), o: "#on_call".into() };
    assert_eq!(c.query(&[p], 5).len(), 1);
}

#[test]
fn closures_are_cached_per_revision_and_scope() {
    let (o, ctx) = world();
    let a = o.reason(&admin(), &acme()).unwrap();
    let b = o.reason(&admin(), &acme()).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    let other = o.reason(&Principal::viewer("vic", "acme"), &acme()).unwrap();
    assert!(!Arc::ptr_eq(&a, &other));
    o.upsert(&ctx, EntityInput::new("task", "4", "New")).unwrap();
    let c = o.reason(&admin(), &acme()).unwrap();
    assert!(!Arc::ptr_eq(&a, &c));
    assert!(c.contains(&f("<task:4>", "a", "#task")).is_some());
}

#[test]
fn marking_a_link_transitive_adds_facts() {
    let (o, ctx) = world();
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("task", "c", "C")),
            Mutation::Upsert(EntityInput::new("task", "b", "B").link("depends_on", "task:c")),
            Mutation::Upsert(EntityInput::new("task", "a", "A").link("depends_on", "task:b")),
        ],
    )
    .unwrap();
    let before = o.reason(&admin(), &acme()).unwrap();
    let mut def = o.schema().link_types["depends_on"].clone();
    def.transitive = true;
    let change = set(json!({ "changes": [{ "op": "put_link_type", "name": "depends_on", "def": def }] }));
    let plan = o.schema_plan(&admin(), &change).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
    assert!(plan.changes[0].summary.contains("now transitive"), "{}", plan.changes[0].summary);
    assert!(plan.diff.entries[0].fields.contains(&"transitive".to_string()), "{:?}", plan.diff.entries);
    o.schema_apply(&ctx, change).unwrap();
    let after = o.reason(&admin(), &acme()).unwrap();
    let diff = before.diff(&after);
    assert!(diff.fixpoint_reached && diff.removed.is_empty(), "{diff:?}");
    let facts: Vec<String> = diff.added.iter().map(|a| a.fact.to_string()).collect();
    assert_eq!(facts, vec!["<task:a> depends_on <task:c>", "depends_on a transitive"]);
    assert_eq!(diff.added[0].rule.as_deref(), Some("builtin/transitive"));
    assert_eq!(diff.added[0].premises.len(), 3);
    assert_eq!(diff.added[1].rule, None, "asserted by the schema");
    assert!(after.diff(&before).added.is_empty());
}

#[test]
fn binding_checks_compare_with_the_store() {
    let (o, ctx) = world();
    let cert = o.reason_certificate(&admin(), &acme()).unwrap();
    let same = o.reason_binding(&admin(), &cert.asserted_tsv, Some(&cert.manifest_json)).unwrap();
    assert_eq!(same.verdict, BindingVerdict::SameGraph, "{same:?}");
    assert_eq!(same.workspace.as_deref(), Some("acme"));
    assert_eq!(same.manifest_digest_matches, Some(true));
    assert_eq!(same.certificate_sha256, same.store_sha256);
    assert!(same.means.contains("not to a state of the world"));

    o.upsert(&ctx, EntityInput::new("task", "late", "Added later")).unwrap();
    let moved = o.reason_binding(&admin(), &cert.asserted_tsv, Some(&cert.manifest_json)).unwrap();
    assert_eq!(moved.verdict, BindingVerdict::DifferentGraph);
    assert_eq!((moved.only_in_store, moved.only_in_certificate), (1, 0));
    assert_eq!(moved.sample_only_in_store, vec![f("<task:late>", "a", "#task")]);

    let tampered = cert.asserted_tsv.replacen("<issue:1>\ta\t#issue\n", "", 1);
    let t = o.reason_binding(&admin(), &tampered, Some(&cert.manifest_json)).unwrap();
    assert_eq!(t.manifest_digest_matches, Some(false));
    assert_eq!(t.only_in_store, 2);
    assert!(o.reason_binding(&admin(), "only\ttwo\n", None).is_err());
    let viewer = Principal::viewer("vic", "beta");
    assert!(matches!(o.reason_binding(&viewer, &cert.asserted_tsv, Some(&cert.manifest_json)), Err(Error::Denied(_))));
}

#[test]
fn shapes_are_live_editable() {
    let (o, ctx) = world();
    let put = set(json!({
        "message": "issues need a project",
        "changes": [{ "op": "put_shape", "name": "issue_has_project", "def": {
            "target": "issue",
            "description": "Every issue belongs to a project.",
            "properties": [{ "path": "belongs_to", "min_count": 1, "class": "project" }]
        } }]
    }));
    let plan = o.schema_plan(&admin(), &put).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
    assert!(!plan.breaking);
    assert_eq!(plan.changes[0].op, "put_shape");
    assert!(plan.changes[0].summary.contains("add shape issue_has_project (on issue, 1 property shapes)"));
    assert!(plan.diff.entries.iter().any(|e| e.section == "shapes" && e.change == DiffKind::Added));
    let v2 = o.schema_apply(&ctx, put).unwrap();
    assert_eq!(v2.version.version, 2);
    assert!(v2.version.diff.contains(&"+shapes.issue_has_project".to_string()), "{:?}", v2.version.diff);
    assert_eq!(o.schema().shapes["issue_has_project"].properties[0].id, "issue_has_project/0");
    assert_eq!(o.schema_status(&admin()).unwrap().shapes, 1);

    let bad = set(json!({ "changes": [{ "op": "put_shape", "name": "x", "def": { "target": "spaceship" } }] }));
    let plan = o.schema_plan(&admin(), &bad).unwrap();
    assert!(!plan.valid && plan.errors[0].contains("shape x: unknown target spaceship"), "{:?}", plan.errors);

    let del = set(json!({ "changes": [{ "op": "delete_shape", "name": "issue_has_project" }] }));
    o.schema_apply(&ctx, del).unwrap();
    assert!(o.schema().shapes.is_empty());
    let missing = set(json!({ "changes": [{ "op": "delete_shape", "name": "issue_has_project" }] }));
    assert!(!o.schema_plan(&admin(), &missing).unwrap().valid);

    let back = o.schema_revert(&ctx, 2, Vec::new(), false).unwrap();
    assert!(back.changes.iter().any(|c| c.op() == "put_shape"));
    assert!(o.schema().shapes.contains_key("issue_has_project"));
    let undo = o.schema_undo(&ctx, o.schema_version(), Vec::new(), false).unwrap();
    assert!(undo.changes.iter().any(|c| c.op() == "delete_shape"), "{:?}", undo.changes);
    assert!(o.schema().shapes.is_empty());
}

#[test]
fn renames_reach_horn_rules_and_shapes() {
    let (o, ctx) = world();
    let add = set(json!({ "changes": [
        { "op": "put_object_type", "name": "vendor", "def": {} },
        { "op": "put_rule", "def": { "name": "vendor_contact",
            "when": [["?v", "a", "vendor"], ["?p", "member_of", "?v"]],
            "then": ["?p", "a", "vendor_contact"] } },
        { "op": "put_shape", "name": "people", "def": { "target": "person",
            "properties": [{ "path": "member_of", "class": "vendor" }, { "path": "member_of.owns" }] } }
    ] }));
    let plan = o.schema_plan(&admin(), &add).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
    assert!(
        plan.changes[1].summary.contains("?p a vendor_contact <= ?v a vendor, ?p member_of ?v"),
        "{}",
        plan.changes[1].summary
    );
    o.schema_apply(&ctx, add).unwrap();
    let rename = set(json!({ "changes": [
        { "op": "rename_link_type", "from": "member_of", "to": "in_group" },
        { "op": "rename_object_type", "from": "vendor", "to": "supplier" }
    ] }));
    let plan = o.schema_plan(&admin(), &rename).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
    o.schema_apply(&ctx, rename).unwrap();
    let s = o.schema();
    let r = s.rules.iter().find(|r| r.name == "vendor_contact").unwrap();
    assert_eq!(
        r.when,
        vec![vec![json!("?v"), json!("a"), json!("supplier")], vec![json!("?p"), json!("in_group"), json!("?v")]]
    );
    let sh = &s.shapes["people"];
    assert_eq!(sh.properties[0].path, "in_group");
    assert_eq!(sh.properties[0].class.as_deref(), Some("supplier"));
    assert_eq!(sh.properties[1].path, "in_group.owns");
    // The chain rule that used member_of follows too, and still derives.
    let c = o.reason(&admin(), &acme()).unwrap();
    assert!(c.contains(&f("<issue:1>", "handled_by_team", "<team:payments>")).is_some());
}

#[test]
fn horn_rules_are_validated_when_put_live() {
    let (o, ctx) = world();
    let plan_of =
        |def: Value| o.schema_plan(&admin(), &set(json!({ "changes": [{ "op": "put_rule", "def": def }] }))).unwrap();
    let p = plan_of(json!({ "name": "r", "when": [["?x", "located_in", "?c"]], "then": ["?x", "a", "risky"] }));
    assert!(
        !p.valid && p.errors.iter().any(|e| e.contains("rule r: when[0]: unknown relation located_in")),
        "{:?}",
        p.errors
    );
    let p = plan_of(json!({ "name": "r", "when": [["?x", "a", "issue"]], "then": ["?x", "owned_by", "?t"] }));
    assert!(
        !p.valid && p.errors.iter().any(|e| e.contains("head variable ?t does not occur in when")),
        "{:?}",
        p.errors
    );
    let p = plan_of(json!({ "name": "r", "when": [["?x", "a", "issue"]], "then": ["?x", "state", "done"] }));
    assert!(!p.valid && p.errors.iter().any(|e| e.contains("not property values")), "{:?}", p.errors);
    // A valid rule applies and the reasoner evaluates it at once.
    let good = json!({ "name": "watched", "when": [["?i", "a", "issue"], ["?i", "affects", "service:api"]],
        "then": ["?i", "watched_by", "team:payments"] });
    let p = plan_of(good.clone());
    assert!(p.valid, "{:?}", p.errors);
    assert!(p.changes[0].summary.contains("add rule watched"), "{}", p.changes[0].summary);
    o.schema_apply(&ctx, set(json!({ "changes": [{ "op": "put_rule", "def": good }] }))).unwrap();
    let c = o.reason(&admin(), &acme()).unwrap();
    assert_eq!(derived_by(&c, &f("<issue:1>", "watched_by", "<team:payments>")), "schema/watched");
    // The per-entity walk keeps working and skips Horn rules.
    assert!(o.infer(&admin(), "issue:1").unwrap().iter().all(|i| i.rule != "watched"));
}
