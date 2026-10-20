//! Workspace map queries: overview and point-in-time snapshots.

mod common;

use onto_core::map::{OverviewSpec, SnapshotSpec};
use onto_core::{ActionContext, EntityInput, Ontology, Principal, Schema};

fn pause() {
    std::thread::sleep(std::time::Duration::from_millis(5));
}

#[test]
fn snapshots_show_the_graph_as_it_was() {
    snapshot_suite(Ontology::in_memory(Schema::enterprise()));
}

#[cfg(feature = "postgres")]
#[test]
fn snapshots_on_postgres() {
    let Some(pg) = common::TestTenant::new("snapshot") else { return };
    snapshot_suite(pg.open(Schema::enterprise(), Default::default()));
}

fn snapshot_suite(o: Ontology) {
    let ctx = ActionContext::new(Principal::admin("ada"), "acme");
    let admin = Principal::admin("x");
    o.upsert(&ctx, EntityInput::new("team", "core", "Core")).unwrap();
    o.upsert(&ctx, EntityInput::new("project", "p", "Portal").state("active").link("owned_by", "team:core")).unwrap();
    o.upsert(&ctx, EntityInput::new("issue", "1", "Slow").state("open").link("belongs_to", "project:p")).unwrap();
    pause();
    let t1 = onto_core::time::now_ms();
    pause();
    // Later: the issue is resolved and moved, a task appears, the project is blocked.
    o.upsert(&ctx, EntityInput { id: Some("issue:1".into()), state: Some("resolved".into()), ..Default::default() })
        .unwrap();
    o.unlink(&ctx, "issue:1", "belongs_to", "project:p").unwrap();
    o.upsert(&ctx, EntityInput::new("task", "t", "Cache").link("belongs_to", "project:p")).unwrap();
    o.upsert(&ctx, EntityInput { id: Some("project:p".into()), state: Some("blocked".into()), ..Default::default() })
        .unwrap();

    let then =
        o.snapshot(&admin, &SnapshotSpec { start: vec!["project:p".into()], ts: t1, depth: 2, limit: 50 }).unwrap();
    let ids: Vec<&str> = then.nodes.iter().map(|n| n.id.as_str()).collect();
    assert!(ids.contains(&"issue:1") && ids.contains(&"team:core"), "{ids:?}");
    assert!(!ids.contains(&"task:t"), "the task did not exist yet");
    let project = then.nodes.iter().find(|n| n.id == "project:p").unwrap();
    assert_eq!(project.state.as_deref(), Some("active"));
    let issue = then.nodes.iter().find(|n| n.id == "issue:1").unwrap();
    assert_eq!(issue.state.as_deref(), Some("open"));
    assert!(then.edges.iter().any(|e| e.from == "issue:1" && e.rel == "belongs_to"));

    let now = o
        .snapshot(
            &admin,
            &SnapshotSpec { start: vec!["project:p".into()], ts: onto_core::time::now_ms(), depth: 2, limit: 50 },
        )
        .unwrap();
    let ids: Vec<&str> = now.nodes.iter().map(|n| n.id.as_str()).collect();
    assert!(ids.contains(&"task:t") && !ids.contains(&"issue:1"), "{ids:?}");

    let before =
        o.snapshot(&admin, &SnapshotSpec { start: vec!["project:p".into()], ts: 0, depth: 2, limit: 50 }).unwrap();
    assert!(before.nodes.is_empty(), "nothing existed at the epoch");
}

#[test]
fn overview_ranks_containers_and_hubs() {
    let o = Ontology::in_memory(Schema::enterprise());
    let ctx = ActionContext::new(Principal::admin("ada"), "acme");
    o.upsert(&ctx, EntityInput::new("team", "core", "Core")).unwrap();
    for i in 0..30 {
        o.upsert(&ctx, EntityInput::new("issue", format!("{i}"), format!("Issue {i}")).link("belongs_to", "team:core"))
            .unwrap();
    }
    let ov = o.overview(&Principal::admin("x"), &OverviewSpec { limit: 10, ..Default::default() }).unwrap();
    assert_eq!(ov.graph.nodes[0].id, "team:core");
    assert_eq!(ov.graph.nodes.len(), 10);
    assert!(ov.graph.truncated);
    assert_eq!(ov.total, 31);
    assert_eq!(ov.graph.edges.len(), 9, "edges among the shown nodes only");
    assert_eq!(ov.kinds.iter().find(|k| k.kind == "issue").unwrap().count, 30);
    let only = o
        .overview(
            &Principal::admin("x"),
            &OverviewSpec { kinds: vec!["work_item".into()], limit: 100, ..Default::default() },
        )
        .unwrap();
    assert!(only.graph.nodes.iter().all(|n| n.kind == "issue"));
}
