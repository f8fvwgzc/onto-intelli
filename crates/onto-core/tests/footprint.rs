//! Heap footprint of the in-memory graph and BM25 index, counted by a global
//! allocator. The ceilings lock in the compact layouts of `Graph` and
//! `TextIndex`: a change that makes either of them bigger per entity fails
//! here.
//!
//! An integration test is its own crate, so this counting allocator applies
//! to this test binary only. The file holds a single test, so no other test
//! allocates while it measures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use onto_core::engine::searchable_text;
use onto_core::graph::Graph;
use onto_core::text::TextIndex;
use onto_core::{Entity, Link, Schema};
use serde_json::{Map, Value, json};

static LIVE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: `GlobalAlloc` can only be implemented with `unsafe`. Every method
// forwards to `System` unchanged and only updates a counter; it keeps no
// pointer state, so `System`'s guarantees carry over.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `System` with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_add(new_size, Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

const ENTITIES: usize = 50_000;

/// Bytes per entity as measured with the current layouts, plus 15%. Before
/// the compact layouts the same data took 5,532 (graph) and 765 (text).
const CEIL_G: f64 = 3_422.0 * 1.15;
const CEIL_T: f64 = 382.0 * 1.15;

const WORDS: &[&str] = &[
    "payment",
    "checkout",
    "billing",
    "search",
    "ranking",
    "login",
    "session",
    "cache",
    "queue",
    "webhook",
    "invoice",
    "refund",
    "catalog",
    "pricing",
    "inventory",
    "shipping",
    "fraud",
    "ledger",
    "report",
    "export",
    "import",
    "sync",
    "gateway",
    "auth",
    "token",
    "latency",
    "timeout",
    "retry",
    "migration",
    "schema",
    "index",
    "replica",
    "backup",
    "alert",
    "dashboard",
    "mobile",
    "android",
    "ios",
    "onboarding",
    "email",
    "notification",
    "upload",
    "storage",
    "kafka",
    "postgres",
    "redis",
    "kubernetes",
    "terraform",
    "ingress",
    "certificate",
];

fn words(rng: &mut fastrand::Rng, n: usize) -> String {
    (0..n).map(|_| WORDS[rng.usize(..WORDS.len())]).collect::<Vec<_>>().join(" ")
}

fn entity(id: String, title: String, props: Value) -> Entity {
    Entity {
        kind: id.split(':').next().unwrap_or_default().into(),
        workspace: "bench".into(),
        title,
        state: Some("open".into()),
        props: props.as_object().cloned().unwrap_or_default(),
        aliases: Vec::new(),
        provenance: None,
        created_at: 1,
        updated_at: 1,
        revision: 1,
        id,
    }
}

fn link(from: String, rel: &str, to: String) -> Link {
    Link { from, rel: rel.into(), to, props: Map::new(), workspace: "bench".into(), created_at: 1 }
}

/// The onto-bench data set in miniature: projects, people, then issues and
/// tasks with 3–6 word titles, 1–2 props and about 2.8 links each.
struct Gen {
    rng: fastrand::Rng,
    projects: usize,
    people: usize,
    issues: usize,
    tasks: usize,
}

impl Gen {
    fn new(n: usize) -> Self {
        let (projects, people) = (n / 200, n / 50);
        let rest = n - projects - people;
        let issues = rest * 35 / 100;
        Gen { rng: fastrand::Rng::with_seed(42), projects, people, issues, tasks: rest - issues }
    }

    /// Calls `put` for every entity, then `link` for every link, generating
    /// each just before use so the generator itself holds no heap.
    fn run(&mut self, mut put: impl FnMut(Entity), mut add: impl FnMut(Link)) {
        let rng = &mut self.rng;
        for p in 0..self.projects {
            put(entity(format!("project:p{p}"), format!("Project {} {p}", words(rng, 2)), json!({ "goal": "ship" })));
        }
        for u in 0..self.people {
            put(entity(format!("person:u{u}"), format!("Person {u}"), json!({ "role": words(rng, 1) })));
        }
        for i in 0..self.issues {
            let n = rng.usize(3..=6);
            let props = if rng.bool() {
                json!({ "priority": "high", "description": words(rng, 8) })
            } else {
                json!({ "priority": "low" })
            };
            put(entity(format!("issue:i{i}"), format!("{} {i}", words(rng, n)), props));
        }
        for t in 0..self.tasks {
            let n = rng.usize(3..=6);
            put(entity(format!("task:k{t}"), format!("{} {t}", words(rng, n)), json!({ "estimate": rng.u8(1..8) })));
        }
        for i in 0..self.issues {
            add(link(format!("issue:i{i}"), "belongs_to", format!("project:p{}", rng.usize(..self.projects))));
            add(link(format!("issue:i{i}"), "assigned_to", format!("person:u{}", rng.usize(..self.people))));
            if i > 0 && rng.usize(..8) == 0 {
                add(link(format!("issue:i{i}"), "blocked_by", format!("issue:i{}", rng.usize(..i))));
            }
        }
        for t in 0..self.tasks {
            add(link(format!("task:k{t}"), "belongs_to", format!("project:p{}", rng.usize(..self.projects))));
            add(link(format!("task:k{t}"), "assigned_to", format!("person:u{}", rng.usize(..self.people))));
            add(link(format!("task:k{t}"), "part_of", format!("issue:i{}", rng.usize(..self.issues))));
            if t > 0 && rng.usize(..6) == 0 {
                add(link(format!("task:k{t}"), "blocked_by", format!("task:k{}", rng.usize(..t))));
            }
        }
    }
}

#[test]
fn graph_and_text_index_stay_under_their_byte_ceilings() {
    let schema = Schema::default();
    let base = live();
    let mut graph = Graph::default();
    let mut pending_links = 0usize;
    // Links go in after every entity, as `build_indexes` does on open.
    let mut links = Vec::new();
    Gen::new(ENTITIES).run(|e| graph.put_entity(e), |l| links.push(l));
    for l in links.drain(..) {
        graph.put_link(l);
        pending_links += 1;
    }
    drop(links);
    let graph_bytes = live() - base;

    let base = live();
    let mut text = TextIndex::default();
    for e in graph.entities() {
        text.upsert(&e.id, &searchable_text(&schema, e));
    }
    let text_bytes = live() - base;

    let n = graph.len();
    assert_eq!(n, ENTITIES);
    assert_eq!(graph.link_count(), pending_links);
    assert_eq!(text.len(), n);
    let (g, t) = (graph_bytes as f64 / n as f64, text_bytes as f64 / n as f64);
    println!(
        "footprint at {n} entities, {} links: graph {:.1} MB ({g:.0} B/entity), text {:.1} MB ({t:.0} B/entity)",
        graph.link_count(),
        graph_bytes as f64 / 1048576.0,
        text_bytes as f64 / 1048576.0,
    );
    assert!(g <= CEIL_G, "graph uses {g:.0} B/entity, ceiling {CEIL_G:.0}");
    assert!(t <= CEIL_T, "text index uses {t:.0} B/entity, ceiling {CEIL_T:.0}");
}
