//! Load benchmark: generate a synthetic enterprise graph (teams, people,
//! services, projects, issues, tasks, deployments), bulk-load it into a
//! PostgreSQL tenant, open it the way a replica does, and time the queries
//! an application makes. Prints a markdown report.
//!
//! ```text
//! cargo run --release -p onto-bench -- --url postgres://onto:onto@127.0.0.1:55445/onto --entities 1000000
//! ```
//!
//! `--changes` runs the change feed scenarios instead (docs/design/EVENTS.md
//! section 12): commit latency with change capture off and on, and write
//! latency and delivery lag with 0, 50 and 200 live stream subscribers on
//! one tenant.
//!
//! `--fanout` runs the webhook fan-out scenario (docs/design/EVENTS.md
//! section 12): a crawl of `--crawl` entities in commits of 500, on a tenant
//! with no endpoints and on one with 50 subscribed endpoints (each to one of
//! five kinds, so each entity matches ten), timing the commits and how fast
//! fan-out enqueues the outbox rows. Deliveries are off: rows are enqueued,
//! not sent.
//!
//! `--delivery` runs the webhook delivery scenario (docs/design/EVENTS.md
//! section 12): a local HTTP receiver on 127.0.0.1 and, for each count in
//! `--endpoints`, a fresh tenant with that many endpoints; it times each
//! delivery from the commit to the receiver, one task per commit with
//! nothing queued and during a burst of about `--rows` deliveries, and the
//! rows per second the burst drains at.

use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::{Value, json};

use onto_intelli::core::map::SnapshotSpec;
use onto_intelli::core::query::TraverseSpec;
use onto_intelli::core::store::{PgOptions, PgShared, Store};
use onto_intelli::core::{EntityInput, Link, Principal};
use onto_intelli::{ActionContext, Config, Entity, Intelli};

#[cfg(feature = "count-alloc")]
mod alloc;
mod delivery;

#[cfg(feature = "count-alloc")]
#[global_allocator]
static GLOBAL: alloc::Counting = alloc::Counting;

#[derive(Parser)]
struct Args {
    /// PostgreSQL URL.
    #[arg(long, env = "ONTO_BENCH_URL")]
    url: String,
    #[arg(long, default_value_t = 1_000_000)]
    entities: usize,
    /// Repetitions of each fast query (ask runs a tenth as many).
    #[arg(long, default_value_t = 2_000)]
    iterations: usize,
    #[arg(long, default_value = "bench")]
    tenant: String,
    /// Reuse an already loaded tenant.
    #[arg(long)]
    skip_load: bool,
    /// Keep the tenant afterwards.
    #[arg(long)]
    keep: bool,
    /// Import and open once (computing vectors, building the HNSW index),
    /// then stop and keep the tenant; measure later with --skip-load.
    #[arg(long)]
    load_only: bool,
    /// Run the change feed scenarios (capture off/on, 0/50/200 subscribers)
    /// on a small tenant instead of the load benchmark.
    #[arg(long)]
    changes: bool,
    /// Commits per change feed scenario.
    #[arg(long, default_value_t = 500)]
    writes: usize,
    /// Run the webhook fan-out scenario (0 and 50 endpoints during a crawl).
    #[arg(long)]
    fanout: bool,
    /// Entities the fan-out scenario's crawl writes.
    #[arg(long, default_value_t = 100_000)]
    crawl: usize,
    /// Run the webhook delivery scenario (a local receiver on 127.0.0.1).
    #[arg(long)]
    delivery: bool,
    /// Endpoint counts of the delivery scenario, one fresh tenant each (1 to 50).
    #[arg(long, value_delimiter = ',', default_values_t = [1usize, 10, 50])]
    endpoints: Vec<usize>,
    /// Deliveries in each burst of the delivery scenario (tasks × endpoints).
    #[arg(long, default_value_t = 20_000)]
    rows: usize,
    /// Single-task commits timed in each idle run of the delivery scenario.
    #[arg(long, default_value_t = 200)]
    latency_commits: usize,
    /// Override `[webhooks] max_in_flight_per_tenant` (default 8) in the
    /// delivery scenario.
    #[arg(long)]
    in_flight_tenant: Option<usize>,
    /// Override `[webhooks] max_in_flight_per_endpoint` (default 4) in the
    /// delivery scenario.
    #[arg(long)]
    in_flight_endpoint: Option<usize>,
}

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

struct Plan {
    entities: Vec<Entity>,
    links: Vec<Link>,
    projects: usize,
    services: usize,
    people: usize,
}

fn entity(id: String, kind: &str, title: String, state: Option<&str>, props: Value, ts: i64) -> Entity {
    Entity {
        kind: kind.into(),
        workspace: "bench".into(),
        title,
        state: state.map(str::to_string),
        props: props.as_object().cloned().unwrap_or_default(),
        aliases: Vec::new(),
        provenance: None,
        created_at: ts,
        updated_at: ts,
        revision: 1,
        id,
    }
}

fn generate(n: usize) -> Plan {
    let mut rng = fastrand::Rng::with_seed(42);
    let now = onto_intelli::core::time::now_ms();
    let ts = |rng: &mut fastrand::Rng| now - rng.i64(0..90 * 86_400_000);
    let teams = (n / 1000).max(5);
    let people = (n / 50).max(20);
    let services = (n / 500).max(10);
    let projects = (n / 200).max(10);
    let deployments = (n / 1000).max(5);
    let rest = n.saturating_sub(teams + people + services + projects + deployments);
    let issues = rest * 35 / 100;
    let tasks = rest - issues;
    let mut e = Vec::with_capacity(n);
    let mut l = Vec::with_capacity(n * 3);
    let link = |l: &mut Vec<Link>, from: String, rel: &str, to: String, at: i64| {
        l.push(Link { from, rel: rel.into(), to, props: Default::default(), workspace: "bench".into(), created_at: at })
    };
    for t in 0..teams {
        e.push(entity(format!("team:t{t}"), "team", format!("{} team {t}", words(&mut rng, 1)), None, json!({}), now));
    }
    for p in 0..people {
        let at = ts(&mut rng);
        e.push(entity(
            format!("person:u{p}"),
            "person",
            format!("Person {p}"),
            None,
            json!({ "role": words(&mut rng, 1) }),
            at,
        ));
        link(&mut l, format!("person:u{p}"), "member_of", format!("team:t{}", p % teams), at);
    }
    let svc_states = ["healthy", "healthy", "healthy", "healthy", "degraded", "down"];
    for s in 0..services {
        let at = ts(&mut rng);
        let state = svc_states[rng.usize(..svc_states.len())];
        e.push(entity(
            format!("service:s{s}"),
            "service",
            format!("{} service {s}", words(&mut rng, 2)),
            Some(state),
            json!({ "tier": "standard" }),
            at,
        ));
        link(&mut l, format!("team:t{}", s % teams), "owns", format!("service:s{s}"), at);
        if s > 0 {
            link(&mut l, format!("service:s{s}"), "depends_on", format!("service:s{}", rng.usize(..s)), at);
        }
        if s > 1 && rng.bool() {
            link(&mut l, format!("service:s{s}"), "depends_on", format!("service:s{}", rng.usize(..s)), at);
        }
    }
    for d in 0..deployments {
        let at = ts(&mut rng);
        let state = if rng.usize(..10) == 0 { "failed" } else { "succeeded" };
        e.push(entity(format!("deployment:d{d}"), "deployment", format!("Deployment {d}"), Some(state), json!({}), at));
        link(&mut l, format!("deployment:d{d}"), "deployed_to", format!("service:s{}", rng.usize(..services)), at);
    }
    let proj_states = ["planned", "active", "active", "active", "blocked", "done"];
    for p in 0..projects {
        let at = ts(&mut rng);
        let state = proj_states[rng.usize(..proj_states.len())];
        e.push(entity(
            format!("project:p{p}"),
            "project",
            format!("Project {} {p}", words(&mut rng, 2)),
            Some(state),
            json!({ "goal": words(&mut rng, 6) }),
            at,
        ));
        link(&mut l, format!("team:t{}", p % teams), "owns", format!("project:p{p}"), at);
        link(&mut l, format!("project:p{p}"), "depends_on", format!("service:s{}", rng.usize(..services)), at);
    }
    let issue_states = ["open", "triaged", "in_progress", "blocked", "resolved", "closed"];
    let prios = ["low", "medium", "high", "critical"];
    for i in 0..issues {
        let at = ts(&mut rng);
        let state = issue_states[rng.usize(..issue_states.len())];
        let prio = prios[rng.usize(..prios.len())];
        e.push(entity(
            format!("issue:i{i}"),
            "issue",
            format!("{} {}", words(&mut rng, 3), i),
            Some(state),
            json!({ "priority": prio, "description": words(&mut rng, 12) }),
            at,
        ));
        link(&mut l, format!("issue:i{i}"), "belongs_to", format!("project:p{}", rng.usize(..projects)), at);
        link(&mut l, format!("issue:i{i}"), "assigned_to", format!("person:u{}", rng.usize(..people)), at);
        link(&mut l, format!("issue:i{i}"), "affects", format!("service:s{}", rng.usize(..services)), at);
        if i > 0 && rng.usize(..8) == 0 {
            link(&mut l, format!("issue:i{i}"), "blocked_by", format!("issue:i{}", rng.usize(..i)), at);
        }
    }
    let task_states = ["todo", "todo", "in_progress", "blocked", "review", "done"];
    for t in 0..tasks {
        let at = ts(&mut rng);
        let state = task_states[rng.usize(..task_states.len())];
        e.push(entity(
            format!("task:k{t}"),
            "task",
            format!("{} {t}", words(&mut rng, 3)),
            Some(state),
            json!({ "estimate": rng.u8(1..8) as f64 }),
            at,
        ));
        link(&mut l, format!("task:k{t}"), "belongs_to", format!("project:p{}", rng.usize(..projects)), at);
        link(&mut l, format!("task:k{t}"), "assigned_to", format!("person:u{}", rng.usize(..people)), at);
        if rng.bool() && issues > 0 {
            link(&mut l, format!("task:k{t}"), "part_of", format!("issue:i{}", rng.usize(..issues)), at);
        }
        if t > 0 && rng.usize(..6) == 0 {
            link(&mut l, format!("task:k{t}"), "blocked_by", format!("task:k{}", rng.usize(..t)), at);
        }
    }
    Plan { entities: e, links: l, projects, services, people }
}

fn rss_mb() -> f64 {
    let pid = std::process::id().to_string();
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

/// Live and peak heap in MB, when the counting allocator is linked in.
fn heap_mb() -> Option<(f64, f64)> {
    #[cfg(feature = "count-alloc")]
    {
        Some(alloc::heap_mb())
    }
    #[cfg(not(feature = "count-alloc"))]
    {
        None
    }
}

fn reset_heap_peak() {
    #[cfg(feature = "count-alloc")]
    alloc::reset_peak();
}

/// Heap rows for one open: what stays allocated afterwards, per entity, and
/// the peak above the starting point while it ran.
fn heap_rows(what: &str, before: Option<(f64, f64)>, entities: usize) -> Vec<String> {
    let (Some((live0, _)), Some((live1, peak))) = (before, heap_mb()) else { return Vec::new() };
    let live = live1 - live0;
    let per = live * 1024.0 * 1024.0 / entities.max(1) as f64;
    vec![
        format!("- {what}: heap live after load **{live:.0} MB** ({per:.0} B/entity, allocator-counted)"),
        format!("- {what}: heap peak during open **{:.0} MB** above the starting point", peak - live0),
    ]
}

struct Stat {
    name: &'static str,
    detail: String,
    samples: Vec<f64>,
}

impl Stat {
    fn row(&mut self) -> String {
        self.samples.sort_by(f64::total_cmp);
        let n = self.samples.len().max(1);
        let pct = |p: f64| {
            self.samples.get(((n as f64 * p).ceil() as usize).saturating_sub(1).min(n - 1)).copied().unwrap_or(0.0)
        };
        let mean = self.samples.iter().sum::<f64>() / n as f64;
        format!(
            "| {} | {} | {} | {:.3} | {:.3} | {:.3} | {:.3} |",
            self.name,
            self.detail,
            self.samples.len(),
            pct(0.50),
            pct(0.99),
            mean,
            self.samples.last().copied().unwrap_or(0.0)
        )
    }
}

fn time<T>(samples: &mut Vec<f64>, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let out = f();
    samples.push(t.elapsed().as_secs_f64() * 1000.0);
    out
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let opts = PgOptions { max_connections: 16, ..Default::default() };
    let shared = PgShared::connect(&args.url, &opts)?;
    if args.changes {
        return changes_bench(&args, &shared, &opts).await;
    }
    if args.fanout {
        return fanout_bench(&args, &shared, &opts).await;
    }
    if args.delivery {
        return delivery::delivery_bench(&args, &shared, &opts).await;
    }
    let mut report: Vec<String> = Vec::new();

    let gen_t = Instant::now();
    let plan = generate(args.entities);
    let gen_s = gen_t.elapsed().as_secs_f64();
    eprintln!("generated {} entities, {} links in {gen_s:.1}s", plan.entities.len(), plan.links.len());

    if !args.skip_load {
        let fresh = shared.tenant(&args.tenant, opts.clone())?;
        fresh.drop_tenant()?;
        drop(fresh);
        let store = shared.tenant(&args.tenant, opts.clone())?;
        let t = Instant::now();
        store.bulk_import(&plan.entities, &plan.links)?;
        let s = t.elapsed().as_secs_f64();
        eprintln!("bulk import (COPY) in {s:.1}s");
        report.push(format!(
            "- Bulk import (COPY, one commit): **{:.1} s** for {} entities and {} links",
            s,
            plan.entities.len(),
            plan.links.len()
        ));
    }

    // The first open after an import also computes every vector and builds
    // the HNSW index; a replica opening later only loads the graph.
    let rss0 = rss_mb();
    reset_heap_peak();
    let heap0 = heap_mb();
    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.default_workspace = "bench".into();
    cfg.memory.enabled = false;
    cfg.cache.enabled = false;
    let store: Arc<dyn Store> = Arc::new(shared.tenant(&args.tenant, opts.clone())?);
    let t = Instant::now();
    let intelli = Intelli::open_with_store(cfg, store, onto_intelli::llm::build(&Default::default())?).await?;
    let open_s = t.elapsed().as_secs_f64();
    let onto = intelli.ontology().clone();
    let stats = onto.stats()?;
    let rss1 = rss_mb();
    eprintln!(
        "opened in {open_s:.1}s: {} entities, {} links, {} vectors, rss {:.0} MB",
        stats.entities, stats.links, stats.vectors, rss1
    );
    report.push(format!(
        "- First open{}: **{:.1} s** ({} entities, {} links, {} vectors in pgvector), RSS {:.0} MB (+{:.0} MB)",
        if args.skip_load { "" } else { " after the import, computing every vector and building the HNSW index" },
        open_s,
        stats.entities,
        stats.links,
        stats.vectors,
        rss1,
        rss1 - rss0
    ));
    report.extend(heap_rows("First open", heap0, stats.entities));

    if args.load_only {
        println!("## Load\n");
        for r in &report {
            println!("{r}");
        }
        return Ok(());
    }

    let p = Principal::admin("bench");
    let mut rng = fastrand::Rng::with_seed(7);
    let iters = args.iterations;
    let mut stats: Vec<Stat> = Vec::new();

    let mut s = Stat { name: "get", detail: "entity by id".into(), samples: Vec::new() };
    for _ in 0..iters {
        let id = format!("issue:i{}", rng.usize(..plan.entities.len() / 3));
        let _ = time(&mut s.samples, || onto.get(&p, &id));
    }
    stats.push(s);

    let mut s = Stat { name: "search", detail: "hybrid BM25 + vector, 2 words, top 10".into(), samples: Vec::new() };
    for _ in 0..iters {
        let q = words(&mut rng, 2);
        let _ = time(&mut s.samples, || onto.search(&p, &q, &[], None, 10));
    }
    stats.push(s);

    let mut s =
        Stat { name: "search (vector)", detail: "pgvector HNSW only, 2 words, top 10".into(), samples: Vec::new() };
    for _ in 0..iters {
        let q = words(&mut rng, 2);
        let _ = time(&mut s.samples, || {
            onto.search_with(&p, &q, &[], None, 10, onto_intelli::core::SearchMode::Vector, false)
        });
    }
    stats.push(s);

    let mut s = Stat { name: "search (lexical)", detail: "BM25 only, 2 words, top 10".into(), samples: Vec::new() };
    for _ in 0..iters {
        let q = words(&mut rng, 2);
        let _ = time(&mut s.samples, || {
            onto.search_with(&p, &q, &[], None, 10, onto_intelli::core::SearchMode::Lexical, false)
        });
    }
    stats.push(s);

    let mut s = Stat { name: "traverse", detail: "from a project, depth 2, ≤200 nodes".into(), samples: Vec::new() };
    for _ in 0..iters {
        let spec = TraverseSpec {
            start: vec![format!("project:p{}", rng.usize(..plan.projects))],
            depth: 2,
            limit: 200,
            ..Default::default()
        };
        let _ = time(&mut s.samples, || onto.traverse(&p, &spec));
    }
    stats.push(s);

    let mut s = Stat { name: "path", detail: "person → service, ≤5 hops".into(), samples: Vec::new() };
    for _ in 0..iters {
        let a = format!("person:u{}", rng.usize(..plan.people));
        let b = format!("service:s{}", rng.usize(..plan.services));
        let _ = time(&mut s.samples, || onto.path(&p, &a, &b, 5));
    }
    stats.push(s);

    let mut s = Stat { name: "impact", detail: "of a service, depth 4".into(), samples: Vec::new() };
    for _ in 0..iters {
        let id = format!("service:s{}", rng.usize(..plan.services));
        let _ = time(&mut s.samples, || onto.impact(&p, &id, 4));
    }
    stats.push(s);

    let mut s = Stat { name: "root cause", detail: "of a project, depth 6".into(), samples: Vec::new() };
    for _ in 0..iters {
        let id = format!("project:p{}", rng.usize(..plan.projects));
        let _ = time(&mut s.samples, || onto.root_cause(&p, &id, 6));
    }
    stats.push(s);

    let mut s =
        Stat { name: "snapshot", detail: "graph around a project 30 days ago, depth 1".into(), samples: Vec::new() };
    let ago = onto_intelli::core::time::now_ms() - 30 * 86_400_000;
    for _ in 0..(iters / 10).max(20) {
        let spec = SnapshotSpec {
            start: vec![format!("project:p{}", rng.usize(..plan.projects))],
            ts: ago,
            depth: 1,
            limit: 200,
        };
        let _ = time(&mut s.samples, || onto.snapshot(&p, &spec));
    }
    stats.push(s);

    let mut s = Stat {
        name: "ask (deterministic)",
        detail: "“Who owns project:pN?” full pipeline, 0 tokens".into(),
        samples: Vec::new(),
    };
    for _ in 0..(iters / 10).max(20) {
        let q = format!("Who owns project:p{}?", rng.usize(..plan.projects));
        let t = Instant::now();
        let _ = intelli.call(&p, "intelli.ask", json!({ "question": q, "no_cache": true })).await;
        s.samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    stats.push(s);

    let mut s = Stat {
        name: "ask (reasoning)",
        detail: "“Why is project:pN blocked?” context graph + mock LLM".into(),
        samples: Vec::new(),
    };
    for _ in 0..(iters / 10).max(20) {
        let q = format!("Why is project:p{} blocked?", rng.usize(..plan.projects));
        let t = Instant::now();
        let _ = intelli.call(&p, "intelli.ask", json!({ "question": q, "no_cache": true })).await;
        s.samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    stats.push(s);

    let mut s = Stat { name: "write", detail: "upsert one entity (PostgreSQL commit)".into(), samples: Vec::new() };
    let ctx = ActionContext::new(p.clone(), "bench");
    for i in 0..(iters / 4).max(50) {
        let input = EntityInput::new("task", format!("bench{i}"), format!("bench write {i}"))
            .link("belongs_to", format!("project:p{}", rng.usize(..plan.projects)));
        let _ = time(&mut s.samples, || onto.upsert(&ctx, input));
    }
    stats.push(s);

    // A second replica sees a write after LISTEN/NOTIFY.
    let replica_store: Arc<dyn Store> = Arc::new(shared.tenant(&args.tenant, opts.clone())?);
    let mut rcfg = Config::in_memory();
    rcfg.engine.storage = "postgres".into();
    rcfg.engine.follow_interval_ms = 1_000;
    rcfg.memory.enabled = false;
    reset_heap_peak();
    let heap2 = heap_mb();
    let (rss2, t) = (rss_mb(), Instant::now());
    let replica = Intelli::open_with_store(rcfg, replica_store, onto_intelli::llm::build(&Default::default())?).await?;
    let replica_s = t.elapsed().as_secs_f64();
    report.push(format!(
        "- Open as another replica (graph and BM25 into memory; vectors stay in PostgreSQL): **{:.1} s**, +{:.0} MB resident (RSS)",
        replica_s,
        rss_mb() - rss2
    ));
    let replica_entities = replica.ontology().stats()?.entities;
    report.extend(heap_rows("Replica open", heap2, replica_entities));
    let mut s =
        Stat { name: "replication", detail: "write on one replica → visible on another".into(), samples: Vec::new() };
    for i in 0..50 {
        let id = format!("task:repl{i}");
        let t = Instant::now();
        onto.upsert(&ctx, EntityInput::new("task", format!("repl{i}"), "replicated"))?;
        while replica.ontology().get(&p, &id).is_err() {
            if t.elapsed() > Duration::from_secs(10) {
                break;
            }
            tokio::time::sleep(Duration::from_micros(200)).await;
        }
        s.samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    stats.push(s);

    println!("## Results\n");
    for r in &report {
        println!("{r}");
    }
    println!("\n| Operation | What | Runs | p50 ms | p99 ms | mean ms | max ms |\n|---|---|---:|---:|---:|---:|---:|");
    for s in &mut stats {
        println!("{}", s.row());
    }
    if !args.keep {
        shared.tenant(&args.tenant, opts)?.drop_tenant()?;
    }
    Ok(())
}

/// The change feed scenarios on a fresh tenant `<tenant>_changes`: single
/// entity commits through the engine with capture off and on, then with 0,
/// 50 and 200 open streams (one principal each, so no two share an
/// evaluation: the worst case), and how long a commit takes to reach a
/// subscriber.
async fn changes_bench(args: &Args, shared: &PgShared, opts: &PgOptions) -> Result<(), Box<dyn std::error::Error>> {
    use onto_intelli::changes::{Frame, Start};
    use onto_intelli::core::changes::ChangeFilter;

    let tenant = format!("{}_changes", args.tenant);
    shared.tenant(&tenant, opts.clone())?.drop_tenant()?;
    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.default_workspace = "bench".into();
    cfg.memory.enabled = false;
    cfg.cache.enabled = false;
    cfg.traces.enabled = false;
    cfg.connectors.background = false;
    cfg.changes.max_subscribers = 1000;
    cfg.changes.max_subscribers_process = 1000;
    let store: Arc<dyn Store> = Arc::new(shared.tenant(&tenant, opts.clone())?);
    let intelli = Intelli::open_with_store(cfg, store, onto_intelli::llm::build(&Default::default())?).await?;
    let onto = intelli.ontology().clone();
    let p = Principal::admin("bench");
    let ctx = ActionContext::new(p.clone(), "bench");
    let n = args.writes;
    let mut seq = 0usize;
    let mut write = |samples: &mut Vec<f64>| -> Result<(), onto_intelli::Error> {
        seq += 1;
        let t = Instant::now();
        onto.upsert(&ctx, EntityInput::new("task", format!("c{seq}"), format!("change bench {seq}")))?;
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
        Ok(())
    };
    // Warm the connection pool and the plan cache.
    let mut warm = Vec::new();
    for _ in 0..50 {
        write(&mut warm)?;
    }

    let mut stats: Vec<Stat> = Vec::new();
    for on in [false, true] {
        onto.set_capture(&ctx, on)?;
        let mut s = Stat {
            name: if on { "commit, capture on" } else { "commit, capture off" },
            detail: "one entity upsert through the engine (PostgreSQL)".into(),
            samples: Vec::new(),
        };
        for _ in 0..n {
            write(&mut s.samples)?;
        }
        stats.push(s);
    }

    let mut lag =
        Stat { name: "delivery lag", detail: "commit start → frame at 1 subscriber".into(), samples: Vec::new() };
    for subscribers in [0usize, 50, 200] {
        let mut drains = Vec::new();
        for k in 0..subscribers {
            let who = Principal::admin(format!("bench-{k}"));
            let mut sub = intelli.subscribe_changes(who, ChangeFilter::default(), Start::Now, None).await?;
            drains.push(tokio::spawn(async move { while sub.recv().await.is_some() {} }));
        }
        let name = match subscribers {
            0 => "commit, 0 streams",
            50 => "commit, 50 streams",
            _ => "commit, 200 streams",
        };
        let mut s =
            Stat { name, detail: format!("capture on, {subscribers} subscribers draining"), samples: Vec::new() };
        for _ in 0..n {
            write(&mut s.samples)?;
        }
        stats.push(s);
        if subscribers == 50 {
            // Lag: one more subscriber, timed from the commit to its frame.
            let mut sub = intelli
                .subscribe_changes(Principal::admin("bench-lag"), ChangeFilter::default(), Start::Now, None)
                .await?;
            for _ in 0..100 {
                let mut one = Vec::new();
                let t = Instant::now();
                write(&mut one)?;
                loop {
                    match tokio::time::timeout(Duration::from_secs(10), sub.recv()).await {
                        Ok(Some(Frame::Change(_))) => break,
                        Ok(Some(_)) => {}
                        _ => return Err("no frame within 10 s".into()),
                    }
                }
                lag.samples.push(t.elapsed().as_secs_f64() * 1000.0);
            }
        }
        let ring = intelli.changes().bytes();
        eprintln!("{subscribers} subscribers: ring {} records, {ring} bytes", intelli.changes().ring_len());
        // Drop the subscriptions; their producers stop on the closed channels.
        for d in drains {
            d.abort();
        }
        let t = Instant::now();
        while intelli.changes().subscribers() > 0 && t.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    stats.push(lag);

    println!("## Change feed ({n} commits per row)\n");
    println!("| Operation | What | Runs | p50 ms | p99 ms | mean ms | max ms |\n|---|---|---:|---:|---:|---:|---:|");
    for s in &mut stats {
        println!("{}", s.row());
    }
    drop(intelli);
    if !args.keep {
        shared.tenant(&tenant, opts.clone())?.drop_tenant()?;
    }
    Ok(())
}

/// Webhook fan-out during a crawl: commit latency with 0 and with 50
/// subscribed endpoints, and the rate at which fan-out enqueues their rows.
async fn fanout_bench(args: &Args, shared: &PgShared, opts: &PgOptions) -> Result<(), Box<dyn std::error::Error>> {
    use onto_intelli::core::Mutation;

    const KINDS: [&str; 5] = ["task", "issue", "service", "project", "person"];
    const BATCH: usize = 500;
    let total = args.crawl.max(BATCH);
    let mut stats: Vec<Stat> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for endpoints in [0usize, 50] {
        let tenant = format!("{}_fanout{endpoints}", args.tenant);
        shared.tenant(&tenant, opts.clone())?.drop_tenant()?;
        let mut cfg = Config::in_memory();
        cfg.engine.storage = "postgres".into();
        cfg.engine.default_workspace = "bench".into();
        cfg.memory.enabled = false;
        cfg.cache.enabled = false;
        cfg.traces.enabled = false;
        cfg.connectors.background = false;
        // Rows are enqueued, not sent; no automations.
        cfg.webhooks.enabled = false;
        cfg.automation.workers = false;
        let store: Arc<dyn Store> = Arc::new(shared.tenant(&tenant, opts.clone())?);
        let intelli =
            Intelli::open_with_store(cfg, store.clone(), onto_intelli::llm::build(&Default::default())?).await?;
        let onto = intelli.ontology().clone();
        let p = Principal::admin("bench");
        let ctx = ActionContext::new(p.clone(), "bench");
        onto.set_capture(&ctx, true)?;
        for k in 0..endpoints {
            intelli
                .call(
                    &p,
                    "webhook.add",
                    json!({
                        "name": format!("ep{k}"),
                        "url": format!("https://hooks.example.com/bench/{k}"),
                        "secret": "env:ONTO_BENCH_HOOK_SECRET",
                        "filter": { "kinds": [KINDS[k % KINDS.len()]], "ops": ["entity.create"] },
                        "principal": { "roles": "admin", "workspaces": "*" },
                    }),
                )
                .await?;
        }
        let started = Instant::now();
        let mut s = Stat {
            name: if endpoints == 0 { "crawl commit, 0 endpoints" } else { "crawl commit, 50 endpoints" },
            detail: format!("{BATCH} new entities per commit, fan-out running"),
            samples: Vec::new(),
        };
        let mut n = 0usize;
        while n < total {
            let muts: Vec<Mutation> = (n..(n + BATCH).min(total))
                .map(|i| {
                    let kind = KINDS[i % KINDS.len()];
                    Mutation::Upsert(EntityInput::new(kind, format!("f{i}"), format!("{kind} {i}")))
                })
                .collect();
            n += muts.len();
            let o = onto.clone();
            let c = ctx.clone();
            let t = Instant::now();
            tokio::task::spawn_blocking(move || o.apply(&c, muts)).await??;
            s.samples.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let crawl = started.elapsed();
        stats.push(s);
        if endpoints > 0 {
            // Fan-out has caught up when every endpoint's cursor is at the head.
            let head = onto.change_head()?;
            let names: Vec<String> = (0..endpoints).map(|k| format!("webhook:ep{k}")).collect();
            loop {
                let behind =
                    names.iter().filter(|c| store.consumer_get(c).ok().flatten().is_none_or(|x| x < head)).count();
                if behind == 0 {
                    break;
                }
                if started.elapsed() > Duration::from_secs(1800) {
                    return Err(format!("fan-out did not catch up in 30 min ({behind} endpoints behind)").into());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let done = started.elapsed();
            let rows: u64 = store.outbox_open_counts()?.iter().map(|(_, n)| n).sum();
            lines.push(format!(
                "- 50 endpoints: {total} entities crawled in {:.1} s; fan-out enqueued {rows} rows and caught up \
                 {:.1} s after the crawl began ({:.0} rows/s, {:.0} entities/s through fan-out).",
                crawl.as_secs_f64(),
                done.as_secs_f64(),
                rows as f64 / done.as_secs_f64(),
                total as f64 / done.as_secs_f64()
            ));
        } else {
            lines.push(format!("- 0 endpoints: {total} entities crawled in {:.1} s.", crawl.as_secs_f64()));
        }
        drop(intelli);
        if !args.keep {
            shared.tenant(&tenant, opts.clone())?.drop_tenant()?;
        }
    }
    println!("## Webhook fan-out during a crawl ({total} entities, commits of {BATCH})\n");
    println!("| Operation | What | Runs | p50 ms | p99 ms | mean ms | max ms |\n|---|---|---:|---:|---:|---:|---:|");
    for s in &mut stats {
        println!("{}", s.row());
    }
    println!();
    for l in lines {
        println!("{l}");
    }
    Ok(())
}
