//! Load benchmark: generate a synthetic enterprise graph (teams, people,
//! services, projects, issues, tasks, deployments), bulk-load it into a
//! PostgreSQL tenant, open it the way a replica does, and time the queries
//! an application makes. Prints a markdown report.
//!
//! ```text
//! cargo run --release -p onto-bench -- --url postgres://onto:onto@127.0.0.1:55445/onto --entities 1000000
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::{Value, json};

use onto_intelli::core::map::SnapshotSpec;
use onto_intelli::core::query::TraverseSpec;
use onto_intelli::core::store::{PgOptions, PgShared, Store};
use onto_intelli::core::{EntityInput, Link, Principal};
use onto_intelli::{ActionContext, Config, Entity, Intelli};

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

    // Open the way a replica does: load the tenant, build indexes.
    let rss0 = rss_mb();
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
    eprintln!("opened in {open_s:.1}s: {} entities, {} links, rss {:.0} MB", stats.entities, stats.links, rss1);
    report.push(format!(
        "- Open (load the tenant and build graph, BM25 and vector indexes): **{:.1} s**, {} entities, {} links, resident memory {:.0} MB (+{:.0} MB)",
        open_s,
        stats.entities,
        stats.links,
        rss1,
        rss1 - rss0
    ));

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
    let replica = Intelli::open_with_store(rcfg, replica_store, onto_intelli::llm::build(&Default::default())?).await?;
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
