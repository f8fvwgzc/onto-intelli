//! The nexc-execution package: graphs, node types, relation types.

use serde_json::{Map, Value, json};

use onto_core::action_types::ApplyOptions;
use onto_core::execution::{NodeTypeDef, RelationTypeDef, Vocabulary};
use onto_core::{ActionContext, Error, EventFilter, Ontology, Principal, Schema};

fn schema() -> Schema {
    let mut s = Schema::enterprise();
    s.extend_source("builtin:nexc-execution", |p| std::path::PathBuf::from(p)).unwrap();
    s
}

fn onto() -> (Ontology, ActionContext) {
    (Ontology::in_memory(schema()), ActionContext::new(Principal::admin("ada"), "main"))
}

fn sorted(mut v: Vocabulary) -> Vocabulary {
    v.node_types.sort_by(|a, b| (a.stage, &a.key).cmp(&(b.stage, &b.key)));
    v.relation_types.sort_by(|a, b| a.key.cmp(&b.key));
    v
}

#[test]
fn package_layers_on_the_enterprise_schema_and_the_audit_vocabulary() {
    let s = schema();
    for k in ["execution_graph", "node_type", "relation_type", "node"] {
        assert!(s.object_types.contains_key(k), "{k}");
    }
    assert_eq!(s.resolve_rel("defines").map(|(b, _)| b).as_deref(), Some("defined_in"));
    assert_eq!(s.canonical_kind("graphs").as_deref(), Some("execution_graph"));
    // nexc's audit layer also defines `node`: layers merge.
    let mut s2 = schema();
    s2.extend_toml("[object_types.node]\ndescription = \"A node a run executed.\"\n[object_types.run]\n[link_types.executed]\nfrom = ['run']\nto = ['node']\n")
        .unwrap();
    assert!(s2.validate_link("node", "instance_of", "node_type", true).is_ok());
}

#[test]
fn a_new_graph_starts_with_nexcs_starter_vocabulary() {
    let (o, ctx) = onto();
    let out = o.init_execution_graph(&ctx, "g1", "Launch plan", Some(Vocabulary::starter())).unwrap();
    assert_eq!(out.graph, "execution_graph:g1");
    assert_eq!(out.vocabulary, sorted(Vocabulary::starter()));
    let code = o.get(&Principal::admin("x"), "node_type:g1.code").unwrap();
    assert_eq!(code.props["allow_code_exec"], json!(true));
    assert_eq!(code.props["default_executor"], json!("llm"));
    assert_eq!(
        o.neighbors(&Principal::admin("x"), "node_type:g1.code", Some("defined_in")).unwrap()[0].1.id,
        "execution_graph:g1"
    );
    // Keys are unique per graph, not across graphs.
    o.init_execution_graph(&ctx, "g2", "", Some(Vocabulary::starter())).unwrap();
    assert!(o.get(&Principal::admin("x"), "node_type:g2.task").is_ok());
    assert_eq!(o.execution_vocabulary(&Principal::admin("x"), "g2").unwrap().node_types.len(), 6);
}

#[test]
fn replacing_a_vocabulary_creates_updates_and_removes_atomically() {
    let (o, ctx) = onto();
    o.init_execution_graph(&ctx, "g1", "Plan", Some(Vocabulary::starter())).unwrap();
    let mut v = Vocabulary::starter();
    v.node_types.iter_mut().find(|t| t.key == "task").unwrap().stage = 3;
    v.node_types.push(NodeTypeDef {
        key: "review".into(),
        label: "Review".into(),
        description: "Checking a deliverable.".into(),
        color: "#14b8a6".into(),
        icon: "search".into(),
        default_role: "reviewer".into(),
        default_executor: "agent".into(),
        stage: 4,
        produces_artifact: false,
        allow_code_exec: false,
    });
    v.relation_types.retain(|r| r.key != "relates_to");
    v.relation_types.push(RelationTypeDef {
        key: "reviews".into(),
        label: "Reviews".into(),
        description: "".into(),
        blocking: true,
    });
    let out = o.set_execution_vocabulary(&ctx, "g1", v.clone()).unwrap();
    assert_eq!(out.created, vec!["review", "reviews"]);
    assert_eq!(out.updated, vec!["task"]);
    assert_eq!(out.removed, vec!["relates_to"]);
    assert_eq!(out.vocabulary, sorted(v));
    let h = o.history(&Principal::admin("x"), "node_type:g1.task", 5).unwrap();
    assert!(h[0].changed.contains(&"props".to_string()) || h[0].changed.contains(&"stage".to_string()), "{h:?}");
    assert!(o.get(&Principal::admin("x"), "relation_type:g1.relates_to").is_err());
    let deletes = o
        .events(&Principal::admin("x"), &EventFilter { action: Some("entity.delete".into()), ..Default::default() })
        .unwrap();
    assert_eq!(deletes[0].target.as_deref(), Some("relation_type:g1.relates_to"));
    assert_eq!(deletes[0].actor, "ada");
}

#[test]
fn nexc_validation_rules_hold() {
    let (o, ctx) = onto();
    o.init_execution_graph(&ctx, "g1", "", None).unwrap();
    let bad = |f: &dyn Fn(&mut NodeTypeDef)| {
        let mut t = Vocabulary::starter().node_types.remove(0);
        f(&mut t);
        o.set_execution_vocabulary(&ctx, "g1", Vocabulary { node_types: vec![t], relation_types: vec![] }).unwrap_err()
    };
    assert!(bad(&|t| t.key = "Bad Key".into()).to_string().contains("matching"));
    assert!(bad(&|t| t.color = "red".into()).to_string().contains("#[0-9a-fA-F]{6}"));
    assert!(bad(&|t| t.label = "x".repeat(61)).to_string().contains("at most 60"));
    assert!(bad(&|t| t.default_executor = "robot".into()).to_string().contains("llm|agent|symphony"));
    let t = Vocabulary::starter().node_types.remove(0);
    let dup = Vocabulary { node_types: vec![t.clone(), t], relation_types: vec![] };
    assert!(
        matches!(o.set_execution_vocabulary(&ctx, "g1", dup).unwrap_err(), Error::Invalid(m) if m.contains("twice"))
    );
    // Viewers cannot edit; the package must be loaded.
    let viewer = ActionContext::new(Principal::viewer("val", "main"), "main");
    assert!(matches!(o.set_execution_vocabulary(&viewer, "g1", Vocabulary::starter()).unwrap_err(), Error::Denied(_)));
    let plain = Ontology::in_memory(Schema::enterprise());
    assert!(matches!(plain.execution_vocabulary(&Principal::admin("x"), "g1").unwrap_err(), Error::Config(_)));
}

#[test]
fn governed_actions_define_and_retire_types() {
    let (o, ctx) = onto();
    o.init_execution_graph(&ctx, "g1", "", None).unwrap();
    let params = |v: Value| -> Map<String, Value> { v.as_object().cloned().unwrap() };
    let define = params(json!({ "graph": "execution_graph:g1", "key": "qa", "label": "QA", "default_role": "tester",
        "stage": 5, "allow_code_exec": true }));
    // Node types can allow code execution: medium risk, confirmation required.
    let pending = o.apply_action(&ctx, "define_node_type", &define, ApplyOptions::default()).unwrap();
    assert_eq!(pending.status, "proposed");
    let r = o
        .apply_action(&ctx, "define_node_type", &define, ApplyOptions { confirm: true, ..Default::default() })
        .unwrap();
    assert_eq!(r.status, "applied", "{r:?}");
    let v = o.execution_vocabulary(&Principal::admin("x"), "g1").unwrap();
    assert_eq!(v.node_types[0].key, "qa");
    assert_eq!(v.node_types[0].stage, 5);
    assert!(v.node_types[0].allow_code_exec);
    o.apply_action(
        &ctx,
        "define_relation_type",
        &params(json!({ "graph": "execution_graph:g1", "key": "blocks", "label": "Blocks", "blocking": true })),
        ApplyOptions::default(),
    )
    .unwrap();
    assert!(o.execution_vocabulary(&Principal::admin("x"), "g1").unwrap().relation_types[0].blocking);
    // Retiring is high risk: it needs confirmation.
    let item = params(json!({ "item": "node_type:g1.qa" }));
    let pending = o.apply_action(&ctx, "retire_execution_type", &item, ApplyOptions::default()).unwrap();
    assert_eq!(pending.status, "proposed");
    o.apply_action(&ctx, "retire_execution_type", &item, ApplyOptions { confirm: true, ..Default::default() }).unwrap();
    assert!(o.execution_vocabulary(&Principal::admin("x"), "g1").unwrap().node_types.is_empty());
}
