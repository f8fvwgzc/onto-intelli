//! Events: who reads them, and what an append returns. Each test runs on
//! the memory store and, when a test database is set, on PostgreSQL.

mod common;

use serde_json::json;

use onto_core::policy::Role;
use onto_core::{ActionContext, EntityInput, EventFilter, EventInput, Ontology, OntologyOptions, Principal, Schema};

fn stores(test: &str) -> Vec<(Ontology, Option<common::TestTenant>)> {
    let mut out = vec![(Ontology::in_memory(Schema::enterprise()), None)];
    if let Some(pg) = common::TestTenant::new(test) {
        out.push((pg.open(Schema::enterprise(), OntologyOptions::default()), Some(pg)));
    }
    out
}

fn admin() -> ActionContext {
    ActionContext::new(Principal::admin("ann"), "acme")
}

/// Ids of the events `p` reads about `target`, or of all of them.
fn listed(o: &Ontology, p: &Principal, target: Option<&str>) -> Vec<(String, Option<String>)> {
    let f = EventFilter { target: target.map(String::from), limit: Some(500), ..Default::default() };
    o.events(p, &f).unwrap().into_iter().map(|e| (e.action, e.target)).collect()
}

#[test]
fn events_about_restricted_entities_follow_the_entity_reads() {
    for (o, _pg) in stores("events_acl") {
        let ctx = admin();
        o.upsert(&ctx, EntityInput::new("team", "payments", "Payments Team")).unwrap();
        o.upsert(&ctx, EntityInput::new("person", "kenji", "Kenji Tanaka").link("member_of", "team:payments")).unwrap();
        o.upsert(
            &ctx,
            EntityInput::new("knowledge", "salaries", "Salary bands")
                .prop("visibility", "restricted")
                .prop("acl", json!(["team:payments"])),
        )
        .unwrap();
        let note = |target: &str| EventInput {
            action: "note.added".into(),
            target: Some(target.into()),
            payload: json!({ "text": "raised" }),
            ..Default::default()
        };
        o.record_event(&ctx, note("knowledge:salaries")).unwrap();
        // An id that never named an entity hides nothing.
        o.record_event(&ctx, note("ticket:ext-42")).unwrap();
        let kenji = Principal::new("kenji", vec![Role::Member], vec!["acme".into()]).with_entity("person:kenji");
        let eve = Principal::member("eve", "acme");
        let about = |p: &Principal| listed(&o, p, Some("knowledge:salaries"));
        assert!(about(&kenji).iter().any(|(a, _)| a == "note.added"), "{:?}", about(&kenji));
        assert!(about(&eve).is_empty(), "eve reads no event about an entity she cannot read: {:?}", about(&eve));
        let all = listed(&o, &eve, None);
        assert!(all.iter().all(|(_, t)| t.as_deref() != Some("knowledge:salaries")), "{all:?}");
        assert!(all.iter().any(|(_, t)| t.as_deref() == Some("ticket:ext-42")), "{all:?}");
        assert!(all.iter().any(|(_, t)| t.as_deref() == Some("team:payments")), "visible entities stay: {all:?}");
        // Deleted, the entity is judged as it was last written.
        o.delete(&ctx, "knowledge:salaries").unwrap();
        assert!(about(&eve).is_empty(), "{:?}", about(&eve));
        let kept = about(&kenji);
        assert!(
            kept.iter().any(|(a, _)| a == "entity.delete") && kept.iter().any(|(a, _)| a == "note.added"),
            "{kept:?}"
        );
        // Admins read everything in their workspaces.
        assert!(!about(&Principal::admin("x")).is_empty());
    }
}

#[test]
fn append_returns_the_event_it_wrote_however_many_others_are_newer() {
    for (o, _pg) in stores("events_append") {
        let ctx = admin();
        for i in 0..70 {
            o.record_event(&ctx, EventInput { action: format!("tick.{i}"), ..Default::default() }).unwrap();
        }
        // Backdated (an import of history): 70 newer events sort before it,
        // so reading the newest 64 back never found it on PostgreSQL.
        let input = EventInput {
            action: "import.issue".into(),
            actor: Some("nexc:user-7".into()),
            target: Some("issue:old-1".into()),
            ts: Some(1_000),
            payload: json!({ "source": "nexc" }),
            ..Default::default()
        };
        let ev = o.record_event(&ctx, input).unwrap();
        assert_eq!((ev.action.as_str(), ev.ts, ev.actor.as_str()), ("import.issue", 1_000, "nexc:user-7"));
        assert_eq!(ev.payload["recorded_by"], "ann", "{:?}", ev.payload);
        assert_eq!(ev.workspace, "acme");
        // What it returned is what the store keeps.
        let stored = o
            .events(&Principal::admin("x"), &EventFilter { target: Some("issue:old-1".into()), ..Default::default() })
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(serde_json::to_value(&stored[0]).unwrap(), serde_json::to_value(&ev).unwrap());
    }
}
