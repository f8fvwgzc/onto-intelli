//! Live schema: change sets, impact reports, migrations, versions, reverts.

mod common;

use serde_json::{Value, json};

use onto_core::live_schema::{DiffKind, SchemaMode};
use onto_core::{
    ActionContext, EntityInput, Error, EventFilter, Mutation, Ontology, OntologyOptions, Principal, Role, Schema,
    SchemaChangeSet,
};

fn set(v: Value) -> SchemaChangeSet {
    serde_json::from_value(v).expect("change set")
}

fn admin() -> Principal {
    Principal::admin("ada")
}

fn world() -> (Ontology, ActionContext) {
    let o = Ontology::in_memory(Schema::enterprise());
    let ctx = ActionContext::new(admin(), "acme");
    o.apply(
        &ctx,
        vec![
            Mutation::Upsert(EntityInput::new("team", "payments", "Payments Team")),
            Mutation::Upsert(EntityInput::new("person", "kenji", "Kenji Tanaka").link("member_of", "team:payments")),
            Mutation::Upsert(EntityInput::new("person", "aiko", "Aiko Sato").link("member_of", "team:payments")),
            Mutation::Upsert(EntityInput::new("project", "alpha", "Project Alpha").state("active")),
            Mutation::Upsert(
                EntityInput::new("issue", "1", "Payment timeout")
                    .state("triaged")
                    .prop("priority", "high")
                    .link("belongs_to", "project:alpha")
                    .link("assigned_to", "person:kenji"),
            ),
            Mutation::Upsert(
                EntityInput::new("issue", "2", "Refund bug")
                    .state("open")
                    .prop("priority", "low")
                    .link("belongs_to", "project:alpha"),
            ),
            Mutation::Upsert(EntityInput::new("task", "3", "Retry").state("todo").prop("estimate", 3.0)),
        ],
    )
    .unwrap();
    (o, ctx)
}

#[test]
fn additive_changes_are_versioned_and_audited() {
    let (o, ctx) = world();
    assert_eq!(o.schema_version(), 1);
    let s = set(json!({
        "message": "track incidents",
        "changes": [
            { "op": "put_object_type", "name": "incident", "def": {
                "label": "Incident", "color": "#ef4444", "extends": "issue",
                "properties": { "sev": { "type": "int" } } } },
            { "op": "put_property", "object_type": "team", "name": "slack", "def": { "type": "string" } },
            { "op": "put_link_type", "name": "pages", "def": {
                "from": ["incident"], "to": ["person"], "inverse": "paged_by" } }
        ]
    }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.valid, "{:?}", plan.errors);
    assert!(!plan.breaking);
    assert_eq!(plan.changes.len(), 3);
    assert_eq!(plan.changes[1].entities, 0, "no team holds slack yet");
    assert!(plan.diff.entries.iter().any(|e| e.name == "incident" && e.change == DiffKind::Added));
    let before = onto_core::time::now_ms();
    let applied = o.schema_apply(&ctx, s).unwrap();
    assert_eq!(applied.version.version, 2);
    assert_eq!(o.schema_version(), 2);
    assert!(o.schema().object_types.contains_key("incident"));
    // New vocabulary is usable at once.
    o.upsert(
        &ctx,
        EntityInput::new("incident", "9", "Outage").state("open").prop("sev", 1).link("pages", "person:aiko"),
    )
    .unwrap();
    assert_eq!(o.neighbors(&admin(), "person:aiko", Some("paged_by")).unwrap()[0].1.id, "incident:9");
    // History, point in time, diff, audit.
    let history = o.schema_history(&admin(), 10).unwrap();
    assert_eq!(history.iter().map(|h| h.version).collect::<Vec<_>>(), vec![2, 1]);
    assert_eq!(history[0].actor, "ada");
    assert_eq!(history[0].message, "track incidents");
    let v1 = o.schema_as_of(&admin(), before - 1).unwrap();
    assert_eq!(v1.version, 1);
    assert!(!v1.schema.object_types.contains_key("incident"));
    let d = o.schema_diff(&admin(), 1, Some(2)).unwrap();
    assert!(
        d.entries.iter().any(|e| e.section == "object_types" && e.name == "team" && e.fields == ["properties.slack"])
    );
    let events =
        o.events(&admin(), &EventFilter { action: Some("schema.change".into()), ..Default::default() }).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].actor, "ada");
    assert_eq!(events[0].payload["version"], 2);
}

#[test]
fn breaking_state_removal_needs_a_migration() {
    let (o, ctx) = world();
    let mut issue = o.schema().object_types["issue"].clone();
    issue.states.retain(|s| s != "triaged");
    let s = set(json!({ "changes": [ { "op": "put_object_type", "name": "issue", "def": issue } ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.valid);
    assert!(plan.breaking);
    assert_eq!(plan.changes[0].entities, 2);
    assert_eq!(plan.needs_migration.len(), 1, "{:?}", plan.needs_migration);
    assert!(plan.needs_migration[0].contains("triaged"));
    let err = o.schema_apply(&ctx, s).unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err}");
    assert_eq!(o.schema_version(), 1, "refused changes leave the schema alone");

    let s = set(json!({ "changes": [ { "op": "put_object_type", "name": "issue", "def": issue,
        "migrations": [ { "action": "map_states", "map": { "triaged": "open" } } ] } ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.applicable(), "{plan:?}");
    assert_eq!(plan.migration.entities_updated, 1);
    o.schema_apply(&ctx, s).unwrap();
    let e = o.get(&admin(), "issue:1").unwrap();
    assert_eq!(e.state.as_deref(), Some("open"));
    let h = o.history(&admin(), "issue:1", 5).unwrap();
    assert_eq!(h[0].actor, "ada");
    assert_eq!(h[0].changed, vec!["state"]);
}

#[test]
fn property_type_change_converts_values() {
    let (o, ctx) = world();
    o.upsert(&ctx, EntityInput::new("service", "pay", "Payment API").prop("tier", "1")).unwrap();
    o.upsert(&ctx, EntityInput::new("service", "web", "Web").prop("tier", "gold")).unwrap();
    let base = set(json!({ "changes": [ { "op": "put_property", "object_type": "service", "name": "tier",
        "def": { "type": "int" } } ] }));
    let plan = o.schema_plan(&admin(), &base).unwrap();
    assert!(plan.breaking);
    assert!(plan.needs_migration[0].contains("not int"), "{:?}", plan.needs_migration);
    let s = set(json!({ "changes": [ { "op": "put_property", "object_type": "service", "name": "tier",
        "def": { "type": "int" }, "migrations": [ { "action": "convert" } ] } ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.applicable(), "{plan:?}");
    assert!(plan.warnings.iter().any(|w| w.contains("1 values could not be converted")));
    o.schema_apply(&ctx, s).unwrap();
    assert_eq!(o.get(&admin(), "service:pay").unwrap().props["tier"], json!(1));
    assert!(o.get(&admin(), "service:web").unwrap().props.get("tier").is_none());
}

#[test]
fn deleting_a_type_in_use_retypes_and_redirects() {
    let (o, ctx) = world();
    let s = set(json!({ "changes": [ { "op": "delete_object_type", "name": "task" } ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(!plan.valid, "work_item interface and links still reference task: {:?}", plan.errors);

    // Retype tasks into issues via a rename, which also rewrites references.
    let (o, ctx2) = world();
    let _ = ctx;
    let s = set(json!({ "changes": [ { "op": "rename_object_type", "from": "task", "to": "chore" } ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.applicable(), "{plan:?}");
    assert_eq!(plan.migration.entities_retyped, 1);
    o.schema_apply(&ctx2, s).unwrap();
    let e = o.get(&admin(), "task:3").unwrap();
    assert_eq!(e.id, "chore:3", "old ids redirect");
    assert_eq!(e.kind, "chore");
    assert_eq!(e.prop_str("estimate"), None);
    assert_eq!(e.props["estimate"], json!(3.0));
    assert!(
        o.schema().link_types["belongs_to"].from.contains(&"chore".to_string())
            || o.schema().link_types["belongs_to"].from.is_empty()
    );
}

#[test]
fn removing_a_kind_requires_choosing_what_happens_to_its_entities() {
    let o = Ontology::in_memory({
        let mut s = Schema::enterprise();
        s.extend_toml("[object_types.note]\nproperties.body = { type = 'text' }\n").unwrap();
        s
    });
    let ctx = ActionContext::new(admin(), "acme");
    o.upsert(&ctx, EntityInput::new("note", "a", "A note")).unwrap();
    o.upsert(&ctx, EntityInput::new("note", "b", "B note")).unwrap();
    let s = set(json!({ "changes": [ { "op": "delete_object_type", "name": "note" } ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.needs_migration[0].contains("2 note entities"), "{:?}", plan.needs_migration);
    let s = set(json!({ "changes": [ { "op": "delete_object_type", "name": "note",
        "migrations": [ { "action": "retype", "to": "knowledge" } ] } ] }));
    o.schema_apply(&ctx, s).unwrap();
    assert_eq!(o.get(&admin(), "note:a").unwrap().id, "knowledge:a");
    let s = set(json!({ "changes": [ { "op": "put_object_type", "name": "memo", "def": {} } ] }));
    o.schema_apply(&ctx, s).unwrap();
    o.upsert(&ctx, EntityInput::new("memo", "x", "Memo")).unwrap();
    let s = set(json!({ "changes": [ { "op": "delete_object_type", "name": "memo",
        "migrations": [ { "action": "delete_entities" } ] } ] }));
    let applied = o.schema_apply(&ctx, s).unwrap();
    assert_eq!(applied.plan.migration.entities_deleted, 1);
    assert!(o.get(&admin(), "memo:x").is_err());
}

#[test]
fn link_type_changes_migrate_links() {
    let (o, ctx) = world();
    o.link(&ctx, "issue:2", "assigned_to", "person:kenji").unwrap();
    o.link(&ctx, "issue:2", "assigned_to", "person:aiko").unwrap();
    let mut lt = o.schema().link_types["assigned_to"].clone();
    lt.cardinality = onto_core::schema::Cardinality::One;
    let s = set(json!({ "changes": [ { "op": "put_link_type", "name": "assigned_to", "def": lt } ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.needs_migration.iter().any(|n| n.contains("single-valued")), "{:?}", plan.needs_migration);
    let s = set(json!({ "changes": [ { "op": "put_link_type", "name": "assigned_to", "def": lt,
        "migrations": [ { "action": "keep_latest" } ] } ] }));
    o.schema_apply(&ctx, s).unwrap();
    let n = o.neighbors(&admin(), "issue:2", Some("assigned_to")).unwrap();
    assert_eq!(n.len(), 1);

    // Renaming a relation moves its links and rewrites rules that use it.
    let s = set(json!({ "changes": [ { "op": "rename_link_type", "from": "member_of", "to": "belongs_to_team" } ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(plan.applicable(), "{plan:?}");
    assert_eq!(plan.migration.links_moved, 2);
    o.schema_apply(&ctx, s).unwrap();
    assert_eq!(o.neighbors(&admin(), "person:kenji", Some("belongs_to_team")).unwrap().len(), 1);
    assert!(o.schema().rules.iter().any(|r| r.chain.iter().any(|c| c == "belongs_to_team")));
    assert!(o.infer(&admin(), "issue:1").unwrap().iter().any(|i| i.to == "team:payments"));
}

#[test]
fn agents_and_members_cannot_apply_and_are_audited() {
    let (o, _) = world();
    let s = set(json!({ "changes": [ { "op": "put_object_type", "name": "x", "def": {} } ] }));
    let agent = Principal::new("bot", vec![Role::Agent, Role::Admin], vec!["*".into()]);
    let err = o.schema_apply(&ActionContext::new(agent, "acme"), s.clone()).unwrap_err();
    assert!(matches!(err, Error::Denied(ref m) if m.contains("agent")), "{err}");
    let member = Principal::member("mia", "acme");
    let err = o.schema_apply(&ActionContext::new(member, "acme"), s.clone()).unwrap_err();
    assert!(matches!(err, Error::Denied(ref m) if m.contains("proposal")), "{err}");
    let denied =
        o.events(&admin(), &EventFilter { action: Some("authorization.denied".into()), ..Default::default() }).unwrap();
    assert_eq!(denied.len(), 2);
    // A custom role can be the apply role.
    let o = Ontology::open(
        Schema::enterprise(),
        std::sync::Arc::new(onto_core::store::MemoryStore::default()),
        OntologyOptions {
            governance: onto_core::Governance {
                apply_roles: vec![Role::parse("schema_admin").unwrap()],
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    let steward = Principal::new("sam", vec![Role::parse("schema_admin").unwrap()], vec!["*".into()]);
    o.schema_apply(&ActionContext::new(steward, "acme"), s.clone()).unwrap();
    let err = o.schema_apply(&ActionContext::new(admin(), "acme"), s).unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "admin is not in apply_roles here");
}

#[test]
fn revert_and_undo_restore_earlier_versions() {
    let (o, ctx) = world();
    let add = |name: &str| set(json!({ "changes": [ { "op": "put_object_type", "name": name, "def": {} } ] }));
    o.schema_apply(&ctx, add("alpha_type")).unwrap();
    o.schema_apply(&ctx, add("beta_type")).unwrap();
    assert_eq!(o.schema_version(), 3);
    // Undo version 2 only: alpha goes, beta stays.
    let r = o.schema_undo(&ctx, 2, Vec::new(), false).unwrap();
    assert_eq!(r.applied.unwrap().version.version, 4);
    assert!(!o.schema().object_types.contains_key("alpha_type"));
    assert!(o.schema().object_types.contains_key("beta_type"));
    // Revert to version 1: everything since goes; dry run first.
    let dry = o.schema_revert(&ctx, 1, Vec::new(), true).unwrap();
    assert!(dry.plan.unwrap().applicable());
    assert_eq!(o.schema_version(), 4);
    o.schema_revert(&ctx, 1, Vec::new(), false).unwrap();
    assert_eq!(o.schema_version(), 5);
    assert!(o.schema_diff(&admin(), 1, None).unwrap().is_empty());
    let history = o.schema_history(&admin(), 10).unwrap();
    assert_eq!(history[0].source, "revert");
    assert_eq!(history[1].source, "undo");
}

#[test]
fn invalid_and_stale_change_sets_are_rejected() {
    let (o, ctx) = world();
    let s = set(json!({ "changes": [ { "op": "put_link_type", "name": "haunts", "def": { "from": ["ghost"] } } ] }));
    let plan = o.schema_plan(&admin(), &s).unwrap();
    assert!(!plan.valid);
    assert!(plan.errors[0].contains("unknown type ghost"), "{:?}", plan.errors);
    assert!(matches!(o.schema_apply(&ctx, s).unwrap_err(), Error::Invalid(_)));
    let s = set(json!({ "expected_version": 7, "changes": [ { "op": "put_object_type", "name": "x", "def": {} } ] }));
    assert!(!o.schema_plan(&admin(), &s).unwrap().valid);
    let s = set(json!({ "changes": [ { "op": "put_object_type", "name": "Bad Name", "def": {} } ] }));
    assert!(o.schema_plan(&admin(), &s).unwrap().errors[0].contains("must match"));
}

#[cfg(feature = "postgres")]
#[test]
fn stored_schema_survives_restarts_and_imports_file_changes() {
    let Some(pg) = common::TestTenant::new("restarts") else { return };
    let ctx = ActionContext::new(admin(), "acme");
    {
        let o = pg.open(Schema::enterprise(), OntologyOptions::default());
        o.upsert(&ctx, EntityInput::new("team", "core", "Core")).unwrap();
        o.schema_apply(
            &ctx,
            set(json!({ "changes": [
            { "op": "put_property", "object_type": "team", "name": "slack", "def": { "type": "string" } } ] })),
        )
        .unwrap();
    }
    {
        let o = pg.open(Schema::enterprise(), OntologyOptions::default());
        assert_eq!(o.schema_version(), 2, "the live edit is still in force");
        assert!(o.schema().object_types["team"].properties.contains_key("slack"));
    }
    {
        // The schema files gain a type: imported as version 3 on top of the live edit.
        let mut files = Schema::enterprise();
        files.extend_toml("[object_types.runbook]\ncategory = 'knowledge'\n").unwrap();
        let o = pg.open(files, OntologyOptions::default());
        assert_eq!(o.schema_version(), 3);
        assert!(o.schema_import_error().is_none());
        assert!(o.schema().object_types.contains_key("runbook"));
        assert!(o.schema().object_types["team"].properties.contains_key("slack"));
        assert_eq!(o.schema_history(&admin(), 1).unwrap()[0].source, "import");
    }
    {
        // Config mode: files win and API edits are refused.
        let opts = OntologyOptions { schema_mode: SchemaMode::Config, ..Default::default() };
        let o = pg.open(Schema::enterprise(), opts);
        assert!(!o.schema().object_types.contains_key("runbook"));
        let err = o
            .schema_apply(&ctx, set(json!({ "changes": [ { "op": "put_object_type", "name": "x", "def": {} } ] })))
            .unwrap_err();
        assert!(matches!(err, Error::Denied(_)));
    }
}
