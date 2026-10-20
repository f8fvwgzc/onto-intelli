//! `onto` — the onto-intelli command line.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use onto_intelli::config::{Config, EXAMPLE_CONFIG, EXAMPLE_PROTO, EXAMPLE_PROTO_PATH};
use onto_intelli::{AskRequest, Error, Intelli, Principal, Result, seed};

#[derive(Parser)]
#[command(name = "onto", version, about = "onto-intelli: ontology-based Enterprise Execution Intelligence")]
struct Cli {
    /// Config file (default: $ONTO_CONFIG, ./onto-intelli.toml, ~/.config/onto-intelli/config.toml).
    #[arg(long, short, global = true, env = "ONTO_CONFIG")]
    config: Option<PathBuf>,
    /// Talk to a running server instead of opening the local database, e.g. http://127.0.0.1:50051.
    #[arg(long, global = true, env = "ONTO_REMOTE")]
    remote: Option<String>,
    /// Bearer token for --remote.
    #[arg(long, global = true, env = "ONTO_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Workspace (default: engine.default_workspace).
    #[arg(long, short, global = true)]
    workspace: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a starter onto-intelli.toml and example proto into DIR.
    Init {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Serve gRPC (built-in API + your protos), reflection, health and HTTP JSON.
    Serve {
        /// Override grpc.listen, e.g. 0.0.0.0:50051.
        #[arg(long)]
        listen: Option<String>,
    },
    /// Ask a question.
    Ask {
        question: Vec<String>,
        /// auto | deterministic | llm
        #[arg(long, default_value = "auto")]
        mode: String,
        /// Print the full JSON response.
        #[arg(long)]
        json: bool,
        /// Print the context graph facts sent to the LLM.
        #[arg(long)]
        context: bool,
        /// Stream the answer as it is generated.
        #[arg(long)]
        stream: bool,
        #[arg(long)]
        no_cache: bool,
        /// Max tokens of context facts.
        #[arg(long)]
        budget: Option<usize>,
    },
    /// Interactive chat with conversation memory.
    Chat,
    /// Run any engine action with JSON input (inline, @file or - for stdin).
    Call { action: String, input: Option<String> },
    /// List engine actions.
    Actions,
    /// Load demo data.
    Seed {
        #[arg(default_value = "demo")]
        dataset: String,
    },
    /// Ingest JSONL records (mutations, entities or GitHub webhook payloads).
    Ingest {
        file: String,
        /// Queue in the background worker instead of applying synchronously.
        #[arg(long = "async")]
        background: bool,
        #[arg(long, default_value = "ingest")]
        source: String,
    },
    /// Show an entity and its neighbourhood.
    Show { id: String },
    /// Dependency intelligence for an entity.
    Deps {
        id: String,
        /// Downstream: what depends on it.
        #[arg(long)]
        down: bool,
        /// Impact analysis.
        #[arg(long)]
        impact: bool,
        /// Root-cause chains.
        #[arg(long)]
        cause: bool,
        /// Execution order and critical path of the work under it.
        #[arg(long)]
        plan: bool,
    },
    /// Print the ontology schema, its populated profile, or suggested additions.
    Schema {
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        sub: Option<SchemaCmd>,
    },
    /// Governed actions: list them, or plan/apply one with key=value parameters.
    Do {
        action: Option<String>,
        /// Parameters as key=value (values may be JSON).
        params: Vec<String>,
        /// Apply high-risk or irreversible actions.
        #[arg(long)]
        confirm: bool,
        /// Plan only.
        #[arg(long)]
        dry_run: bool,
    },
    /// Undo an applied action by receipt id.
    Undo {
        receipt: String,
        #[arg(long)]
        force: bool,
    },
    /// Version history of an entity.
    History { id: String },
    /// What changed recently, optionally inside an entity's scope.
    Changes {
        #[arg(long, default_value_t = 7.0)]
        days: f64,
        #[arg(long)]
        within: Option<String>,
    },
    /// Evaluate a governed metric (or an ad-hoc one with --kind/--measure).
    Metric {
        name: Option<String>,
        /// Group by dimension (repeatable): team, priority, assignee, state, kind...
        #[arg(long)]
        by: Vec<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        measure: Option<String>,
        #[arg(long)]
        within: Option<String>,
        #[arg(long)]
        days: Option<u32>,
    },
    /// Data-quality report.
    Quality {
        #[arg(long)]
        kind: Option<String>,
    },
    /// Likely duplicate entities.
    Dupes {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 0.8)]
        threshold: f32,
    },
    /// Merge a duplicate into the entity to keep (the old id keeps working).
    Merge { keep: String, duplicate: String },
    /// What-if: hypothetically move an entity to a state and see what changes.
    Whatif { id: String, state: String },
    /// Who knows about a topic.
    Experts { topic: Vec<String> },
    /// Export as Turtle (RDF/OWL + PROV-O), Mermaid or JSON.
    Export {
        #[arg(long, default_value = "turtle")]
        format: String,
        #[arg(long)]
        start: Vec<String>,
        #[arg(long, default_value_t = 2)]
        depth: usize,
        #[arg(long)]
        no_schema: bool,
    },
    /// Serve the ontology to AI agents over MCP (stdio).
    Mcp,
    /// Run competency questions (default: the built-in demo suite).
    Eval {
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Compile configured proto files and print the gRPC route table.
    Protos,
    /// Graph, memory, cache and token statistics.
    Stats,
}

#[derive(Subcommand)]
enum SchemaCmd {
    /// Populated patterns, kinds, properties and states.
    Profile,
    /// TOML layer proposing kinds/relations/properties seen in data but not in the schema.
    Suggest,
    /// Live schema version, mode and governance.
    Status,
    /// Dry run of a change set file (JSON or TOML): diff, impact, migration.
    Plan {
        /// Change set file (`-` reads stdin).
        file: PathBuf,
    },
    /// Apply a change set file as a new schema version.
    Apply {
        file: PathBuf,
        /// Only print the plan.
        #[arg(long)]
        dry_run: bool,
    },
    /// Schema versions, newest first.
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// One version: who, when, why, what changed.
    Show { version: u64 },
    /// Differences between two versions (TO defaults to the active one).
    Diff { from: u64, to: Option<u64> },
    /// Make an earlier version's schema active again.
    Revert {
        version: u64,
        #[arg(long)]
        dry_run: bool,
    },
    /// Undo one version's changes on top of the active schema.
    Undo {
        version: u64,
        #[arg(long)]
        dry_run: bool,
    },
}

/// A change set from a JSON or TOML file (`-`: JSON on stdin).
fn read_change_set(path: &std::path::Path) -> Result<Value> {
    let text = if path.as_os_str() == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        buf
    } else {
        std::fs::read_to_string(path).map_err(|e| Error::Config(format!("reading {}: {e}", path.display())))?
    };
    if path.extension().is_some_and(|e| e == "toml") {
        let v: toml::Value = toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        Ok(serde_json::to_value(v)?)
    } else {
        Ok(serde_json::from_str(&text)?)
    }
}

fn print_plan(st: &Style, plan: &Value) {
    let valid = plan["valid"].as_bool().unwrap_or(false);
    let needs = plan["needs_migration"].as_array().cloned().unwrap_or_default();
    let verdict = if !valid {
        st.red("invalid")
    } else if !needs.is_empty() {
        st.red("needs migration")
    } else if plan["breaking"].as_bool().unwrap_or(false) {
        st.cyan("breaking, migrated")
    } else {
        st.bold("non-breaking")
    };
    println!(
        "{} {}",
        verdict,
        st.dim(&format!(
            "on schema v{} · {} entities · {} links in scope",
            plan["base_version"], plan["entities_affected"], plan["links_affected"]
        ))
    );
    for c in plan["changes"].as_array().into_iter().flatten() {
        let mark = if c["breaking"].as_bool().unwrap_or(false) { st.red("!") } else { st.dim("·") };
        println!(
            "  {mark} {} {}",
            c["summary"].as_str().unwrap_or(""),
            st.dim(&format!("({} entities, {} links)", c["entities"], c["links"]))
        );
        for r in c["reasons"].as_array().into_iter().flatten() {
            println!("      {}", st.dim(r.as_str().unwrap_or("")));
        }
    }
    for e in plan["errors"].as_array().into_iter().flatten() {
        println!("  {} {}", st.red("error"), e.as_str().unwrap_or(""));
    }
    for n in &needs {
        println!("  {} {}", st.red("needs"), n.as_str().unwrap_or(""));
    }
    for w in plan["warnings"].as_array().into_iter().flatten() {
        println!("  {} {}", st.cyan("warning"), w.as_str().unwrap_or(""));
    }
    let m = &plan["migration"];
    if m["mutations"].as_u64().unwrap_or(0) > 0 {
        println!(
            "  migration: {} updated, {} retyped, {} deleted entities; {} links deleted, {} moved",
            m["entities_updated"], m["entities_retyped"], m["entities_deleted"], m["links_deleted"], m["links_moved"]
        );
    }
}

fn print_version_line(st: &Style, v: &Value, active: Option<u64>) {
    let n = v["version"].as_u64().unwrap_or(0);
    let mark = if Some(n) == active { st.bold("*") } else { " ".into() };
    println!(
        "{mark} v{:<4} {} {:<9} {:<12} {}{}",
        n,
        st.dim(v["at"].as_str().unwrap_or("")),
        v["source"].as_str().unwrap_or(""),
        v["actor"].as_str().unwrap_or(""),
        v["message"].as_str().unwrap_or(""),
        if v["breaking"].as_bool().unwrap_or(false) { st.red(" [breaking]") } else { String::new() }
    );
}

fn parse_params(items: &[String]) -> Result<Value> {
    let mut m = serde_json::Map::new();
    for it in items {
        let (k, v) = it.split_once('=').ok_or_else(|| Error::invalid(format!("parameter '{it}' must be key=value")))?;
        let value = serde_json::from_str::<Value>(v).unwrap_or_else(|_| json!(v));
        m.insert(k.to_string(), value);
    }
    Ok(Value::Object(m))
}

fn print_receipt(st: &Style, r: &Value) {
    let status = r["status"].as_str().unwrap_or("?");
    let mark = match status {
        "applied" | "undone" => st.bold(status),
        "rejected" => st.red(status),
        _ => st.cyan(status),
    };
    println!("{} {} {}", mark, r["action"].as_str().unwrap_or(""), st.dim(r["id"].as_str().unwrap_or("")));
    let plan = &r["plan"];
    for line in plan["preview"].as_array().into_iter().flatten() {
        println!("  • {}", line.as_str().unwrap_or(""));
    }
    for c in plan["checks"].as_array().into_iter().flatten().filter(|c| c["ok"] == false) {
        println!("  {} {}", st.red("✗"), c["message"].as_str().unwrap_or(""));
    }
    for w in plan["warnings"].as_array().into_iter().flatten() {
        println!("  {} {}", st.dim("!"), w.as_str().unwrap_or(""));
    }
    println!(
        "  {}",
        st.dim(&format!(
            "risk {} · {}",
            plan["risk"].as_str().unwrap_or("?"),
            if plan["reversible"] == true { "reversible" } else { "not reversible" }
        ))
    );
    if let Some(reason) = r["reason"].as_str() {
        println!("  {}", st.dim(reason));
    }
}

// ── backend: local engine or remote server ────────────────────────────

enum Backend {
    Local { intelli: Arc<Intelli>, principal: Principal },
    Remote { http: reqwest::Client, base: String, token: Option<String> },
}

impl Backend {
    async fn call(&self, action: &str, input: Value) -> Result<Value> {
        match self {
            Backend::Local { intelli, principal } => intelli.call(principal, action, input).await,
            Backend::Remote { http, base, token } => {
                let mut req = http.post(format!("{base}/v1/call/{action}")).json(&input);
                if let Some(t) = token {
                    req = req.bearer_auth(t);
                }
                let resp = req.send().await.map_err(|e| Error::Unavailable(format!("{base}: {e}")))?;
                let status = resp.status();
                let body: Value = resp.json().await.map_err(|e| Error::Other(format!("bad response: {e}")))?;
                if status.is_success() {
                    Ok(body)
                } else {
                    let msg = body["error"]["message"].as_str().unwrap_or("request failed").to_string();
                    Err(match body["error"]["code"].as_str() {
                        Some("not_found") => Error::NotFound(msg),
                        Some("invalid_argument") => Error::Invalid(msg),
                        Some("permission_denied") => Error::Denied(msg),
                        _ => Error::Other(msg),
                    })
                }
            }
        }
    }
}

// ── output helpers ────────────────────────────────────────────────────

struct Style {
    on: bool,
}

impl Style {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.on { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn bold(&self, s: &str) -> String {
        self.paint("1", s)
    }
    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
    fn cyan(&self, s: &str) -> String {
        self.paint("36", s)
    }
    fn red(&self, s: &str) -> String {
        self.paint("31", s)
    }
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn read_input(arg: Option<String>) -> Result<Value> {
    let text = match arg.as_deref() {
        None => return Ok(json!({})),
        Some("-") => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            s
        }
        Some(a) if a.starts_with('@') => std::fs::read_to_string(&a[1..])?,
        Some(a) => a.to_string(),
    };
    Ok(serde_json::from_str(&text)?)
}

fn with_ws(mut v: Value, ws: &Option<String>) -> Value {
    if let (Value::Object(m), Some(w)) = (&mut v, ws)
        && !m.contains_key("workspace")
    {
        m.insert("workspace".into(), json!(w));
    }
    v
}

fn entity_line(st: &Style, e: &Value) -> String {
    let title = e["title"].as_str().filter(|t| !t.is_empty()).unwrap_or_else(|| e["id"].as_str().unwrap_or("?"));
    let state = e["state"].as_str().map(|s| format!(" ({s})")).unwrap_or_default();
    format!("{} {}{}", title, st.dim(&format!("[{}]", e["id"].as_str().unwrap_or("?"))), state)
}

fn print_answer(st: &Style, r: &Value, show_context: bool) {
    println!("{}", r["answer"].as_str().unwrap_or_default());
    let tokens =
        r["usage"]["prompt_tokens"].as_u64().unwrap_or(0) + r["usage"]["completion_tokens"].as_u64().unwrap_or(0);
    let ents: Vec<&str> =
        r["entities"].as_array().map(|a| a.iter().filter_map(|e| e["id"].as_str()).collect()).unwrap_or_default();
    let mut footer = format!(
        "{} · intent {} · {} tokens · {:.1} ms",
        r["mode"].as_str().unwrap_or("?"),
        r["intent"].as_str().unwrap_or("?"),
        tokens,
        r["timings"]["total_ms"].as_f64().unwrap_or(0.0)
    );
    if let Some(saved) = r["efficiency"]["saved_tokens"].as_u64().filter(|s| *s > 0) {
        footer.push_str(&format!(" · ~{saved} tokens saved"));
    }
    if let Some(cost) = r["usage"]["cost_usd"].as_f64().filter(|c| *c > 0.0) {
        footer.push_str(&format!(" · ${cost:.5}"));
    }
    if !ents.is_empty() {
        footer.push_str(&format!(" · {}", ents.join(", ")));
    }
    println!("{}", st.dim(&footer));
    if let Some(note) = r["note"].as_str() {
        println!("{}", st.red(&format!("note: {note}")));
    }
    if show_context && let Some(facts) = r["context"]["facts"].as_array() {
        println!("\n{}", st.bold("Context sent to the model:"));
        for f in facts {
            println!(
                "  {} {}",
                st.dim(&format!("[{} {:.2}]", f["source"].as_str().unwrap_or(""), f["score"].as_f64().unwrap_or(0.0))),
                f["text"].as_str().unwrap_or("")
            );
        }
    }
}

// ── main ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let level = if matches!(cli.command, Command::Serve { .. }) { "info" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into()))
        .with_writer(std::io::stderr)
        .init();
    let st = Style { on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() };
    if let Err(e) = run(cli, &st).await {
        eprintln!("{} {e}", st.red("error:"));
        std::process::exit(1);
    }
}

async fn open_local(cli: &Cli) -> Result<(Arc<Intelli>, Config)> {
    let mut cfg = Config::discover(cli.config.as_deref())?;
    if let Some(ws) = &cli.workspace {
        cfg.engine.default_workspace = ws.clone();
    }
    let intelli = Intelli::open(cfg.clone()).await?;
    Ok((intelli, cfg))
}

async fn backend(cli: &Cli) -> Result<Backend> {
    match &cli.remote {
        Some(url) => Ok(Backend::Remote {
            http: reqwest::Client::new(),
            base: url.trim_end_matches('/').to_string(),
            token: cli.token.clone(),
        }),
        None => {
            let (intelli, cfg) = open_local(cli).await?;
            Ok(Backend::Local { intelli, principal: cfg.auth.default_principal })
        }
    }
}

async fn run(cli: Cli, st: &Style) -> Result<()> {
    match &cli.command {
        Command::Init { dir, force } => init(dir, *force, st),
        Command::Serve { listen } => {
            let (_, mut cfg) = open_local(&cli).await?;
            if let Some(l) = listen {
                cfg.grpc.listen = l.clone();
            }
            let intelli = Intelli::open(cfg.clone()).await?;
            eprintln!(
                "{} gRPC + HTTP on {} · storage {} · llm {} ({})",
                st.bold("onto-intelli"),
                cfg.grpc.listen,
                cfg.engine.storage,
                intelli.llm().name(),
                intelli.llm().model()
            );
            if let Some(why) = intelli.llm().unavailable() {
                eprintln!(
                    "{} LLM unavailable: {why} — deterministic answers and fact summaries still work",
                    st.red("!")
                );
            }
            onto_grpc::serve(intelli, async {
                let _ = tokio::signal::ctrl_c().await;
                eprintln!("\nshutting down");
            })
            .await
        }
        Command::Ask { question, mode, json: as_json, context, stream, no_cache, budget } => {
            let q = question.join(" ");
            if q.trim().is_empty() {
                return Err(Error::invalid("usage: onto ask <question>"));
            }
            let req = json!({
                "question": q, "mode": mode, "include_context": context, "no_cache": no_cache,
                "token_budget": budget, "workspace": cli.workspace,
            });
            let be = backend(&cli).await?;
            if *stream && let Backend::Local { intelli, principal } = &be {
                let ask: AskRequest = serde_json::from_value(req)?;
                let mut rx = intelli.ask_stream(principal.clone(), ask);
                while let Some(ev) = rx.recv().await {
                    match ev.kind.as_str() {
                        "delta" => {
                            print!("{}", ev.text);
                            let _ = std::io::stdout().flush();
                        }
                        "done" => {
                            println!();
                            if *as_json {
                                print_json(&ev.data);
                            } else {
                                println!(
                                    "{}",
                                    st.dim(&format!(
                                        "{} · {:.1} ms",
                                        ev.data["mode"].as_str().unwrap_or("?"),
                                        ev.data["timings"]["total_ms"].as_f64().unwrap_or(0.0)
                                    ))
                                );
                            }
                        }
                        "error" => return Err(Error::Other(ev.text)),
                        _ => {}
                    }
                }
                return Ok(());
            }
            let r = be.call("intelli.ask", req).await?;
            if *as_json {
                print_json(&r)
            } else {
                print_answer(st, &r, *context)
            }
            Ok(())
        }
        Command::Chat => chat(&cli, st).await,
        Command::Call { action, input } => {
            let be = backend(&cli).await?;
            print_json(&be.call(action, with_ws(read_input(input.clone())?, &cli.workspace)).await?);
            Ok(())
        }
        Command::Actions => {
            let be = backend(&cli).await?;
            let r = be.call("system.actions", json!({})).await?;
            for a in r["actions"].as_array().into_iter().flatten() {
                let w = if a["write"].as_bool() == Some(true) { "w" } else { " " };
                println!(
                    "{} {:<22} {}",
                    st.dim(w),
                    st.cyan(a["name"].as_str().unwrap_or("")),
                    a["description"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        Command::Seed { dataset } => {
            if dataset != "demo" {
                return Err(Error::invalid(format!("unknown dataset '{dataset}' (available: demo)")));
            }
            if cli.remote.is_some() {
                let be = backend(&cli).await?;
                let records: Vec<Value> =
                    seed::demo_mutations().into_iter().map(|m| serde_json::to_value(m).unwrap_or_default()).collect();
                let r = be.call("ingest.batch", json!({ "records": records, "workspace": cli.workspace.clone().unwrap_or_else(|| "acme".into()) })).await?;
                print_json(&r);
                return Ok(());
            }
            let (intelli, cfg) = open_local(&cli).await?;
            let ws = cli.workspace.clone().unwrap_or(cfg.engine.default_workspace);
            let report = seed::load_demo(&intelli, &ws).await?;
            println!(
                "Seeded workspace {} · {} entities · {} links · {} events · {} memory",
                st.bold(&ws),
                report.graph.entities.len(),
                report.graph.links,
                report.events,
                report.memories
            );
            println!("{}", st.dim("Try: onto ask \"Why is Project Alpha blocked?\" --context"));
            Ok(())
        }
        Command::Ingest { file, background, source } => {
            let text = if file == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else {
                std::fs::read_to_string(file)?
            };
            let mut records = Vec::new();
            for (n, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
                    continue;
                }
                records.push(
                    serde_json::from_str::<Value>(line).map_err(|e| Error::invalid(format!("line {}: {e}", n + 1)))?,
                );
            }
            let be = backend(&cli).await?;
            let total = records.len();
            for chunk in records.chunks(500) {
                be.call(
                    "ingest.batch",
                    json!({ "records": chunk, "async": background, "source": source, "workspace": cli.workspace }),
                )
                .await?;
            }
            if *background {
                be.call("ingest.flush", json!({})).await?;
            }
            println!("Ingested {total} records");
            Ok(())
        }
        Command::Show { id } => {
            let be = backend(&cli).await?;
            let e = be.call("entity.get", json!({ "id": id })).await?;
            println!("{}  {}", st.bold(&entity_line(st, &e)), st.dim(e["kind"].as_str().unwrap_or("")));
            if let Some(props) = e["props"].as_object() {
                for (k, v) in props {
                    let v = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                    println!("  {} {}", st.dim(&format!("{k}:")), v);
                }
            }
            if let Some(p) = e.get("provenance").filter(|p| p.is_object()) {
                println!("  {} {}", st.dim("provenance:"), p);
            }
            let n = be.call("graph.neighbors", json!({ "id": id })).await?;
            let list = n["neighbors"].as_array().cloned().unwrap_or_default();
            for (i, item) in list.iter().enumerate() {
                let branch = if i + 1 == list.len() { "└─" } else { "├─" };
                println!(
                    "  {branch} {} → {}",
                    st.cyan(item["rel"].as_str().unwrap_or("")),
                    entity_line(st, &item["entity"])
                );
            }
            let inferred = be.call("graph.infer", json!({ "id": id })).await?;
            for inf in inferred["inferred"].as_array().into_iter().flatten() {
                println!(
                    "  {} {} → {}",
                    st.dim("⇢"),
                    st.dim(inf["rel"].as_str().unwrap_or("")),
                    inf["to"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        Command::Deps { id, down, impact, cause, plan } => {
            let be = backend(&cli).await?;
            if *cause {
                let r = be.call("graph.root_cause", json!({ "id": id })).await?;
                let chains = r["chains"].as_array().cloned().unwrap_or_default();
                if chains.is_empty() {
                    println!("No entity in an alert state is reachable from {id}.");
                }
                for c in chains {
                    println!("{} {}", st.red("●"), c["text"].as_str().unwrap_or(""));
                }
                return Ok(());
            }
            if *plan {
                let r = be.call("graph.critical_path", json!({ "id": id })).await?;
                println!("{} ({} days)", st.bold("Critical path"), r["cost"]);
                for n in r["path"].as_array().into_iter().flatten() {
                    println!(
                        "  → {}",
                        entity_line(st, &json!({ "id": n["id"], "title": n["title"], "state": n["state"] }))
                    );
                }
                let order: Vec<&str> =
                    r["order"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                println!("{} {}", st.dim("Order:"), order.join(" → "));
                if let Some(c) = r["cyclic"].as_array().filter(|c| !c.is_empty()) {
                    println!("{} {:?}", st.red("Cycle:"), c);
                }
                return Ok(());
            }
            let r = if *impact {
                be.call("graph.impact", json!({ "id": id })).await?
            } else {
                be.call(
                    "graph.dependencies",
                    json!({ "id": id, "direction": if *down { "downstream" } else { "upstream" } }),
                )
                .await?
            };
            let nodes = r["nodes"].as_array().cloned().unwrap_or_default();
            if nodes.is_empty() {
                println!("Nothing found.");
            }
            for n in nodes {
                let depth = n["depth"].as_u64().unwrap_or(1) as usize;
                let mark = if n["open"].as_bool() == Some(true) { "○" } else { "●" };
                println!(
                    "{}{} {}",
                    "  ".repeat(depth),
                    mark,
                    entity_line(st, &json!({ "id": n["id"], "title": n["title"], "state": n["state"] }))
                );
            }
            Ok(())
        }
        Command::Schema { sub: Some(sub), json: as_json } => {
            let be = backend(&cli).await?;
            match sub {
                SchemaCmd::Profile => {
                    print_json(&be.call("schema.profile", json!({ "workspace": cli.workspace })).await?)
                }
                SchemaCmd::Suggest => {
                    let r = be.call("schema.suggest", json!({ "workspace": cli.workspace })).await?;
                    print!("{}", r["toml"].as_str().unwrap_or_default());
                }
                SchemaCmd::Status => print_json(&be.call("schema.status", json!({})).await?),
                SchemaCmd::Plan { file } => {
                    let plan = be.call("schema.plan", read_change_set(file)?).await?;
                    if *as_json { print_json(&plan) } else { print_plan(st, &plan) }
                }
                SchemaCmd::Apply { file, dry_run } => {
                    let mut set = read_change_set(file)?;
                    if *dry_run {
                        set["dry_run"] = json!(true);
                        let plan = be.call("schema.apply", set).await?;
                        if *as_json { print_json(&plan) } else { print_plan(st, &plan) }
                    } else {
                        let r = be.call("schema.apply", set).await?;
                        if *as_json {
                            print_json(&r)
                        } else {
                            print_plan(st, &r["plan"]);
                            println!("{} schema v{}", st.bold("applied"), r["version"]["version"]);
                        }
                    }
                }
                SchemaCmd::History { limit } => {
                    let r = be.call("schema.history", json!({ "limit": limit })).await?;
                    if *as_json {
                        print_json(&r)
                    } else {
                        let active = r["active"].as_u64();
                        for v in r["versions"].as_array().into_iter().flatten() {
                            print_version_line(st, v, active);
                        }
                    }
                }
                SchemaCmd::Show { version } => {
                    let v = be.call("schema.version", json!({ "version": version })).await?;
                    if *as_json {
                        print_json(&v)
                    } else {
                        print_version_line(st, &v, None);
                        for c in v["changes"].as_array().into_iter().flatten() {
                            println!("    {} {}", c["op"].as_str().unwrap_or(""), st.dim(&c.to_string()));
                        }
                        for e in v["diff"]["entries"].as_array().into_iter().flatten() {
                            let fields = e["fields"]
                                .as_array()
                                .map(|f| f.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
                            println!(
                                "    {} {}.{} {}",
                                e["change"].as_str().unwrap_or(""),
                                e["section"].as_str().unwrap_or(""),
                                e["name"].as_str().unwrap_or(""),
                                st.dim(&fields.unwrap_or_default())
                            );
                        }
                    }
                }
                SchemaCmd::Diff { from, to } => {
                    let d = be.call("schema.diff", json!({ "from": from, "to": to })).await?;
                    if *as_json {
                        print_json(&d)
                    } else {
                        for e in d["entries"].as_array().into_iter().flatten() {
                            println!(
                                "{:<9} {}.{}",
                                e["change"].as_str().unwrap_or(""),
                                e["section"].as_str().unwrap_or(""),
                                e["name"].as_str().unwrap_or("")
                            );
                        }
                    }
                }
                SchemaCmd::Revert { version, dry_run } | SchemaCmd::Undo { version, dry_run } => {
                    let action = if matches!(sub, SchemaCmd::Revert { .. }) { "schema.revert" } else { "schema.undo" };
                    let r = be.call(action, json!({ "version": version, "dry_run": dry_run })).await?;
                    if *as_json {
                        print_json(&r)
                    } else if let Some(plan) = r.get("plan") {
                        print_plan(st, plan)
                    } else {
                        print_plan(st, &r["applied"]["plan"]);
                        println!("{} schema v{}", st.bold("applied"), r["applied"]["version"]["version"]);
                    }
                }
            }
            Ok(())
        }
        Command::Do { action: None, .. } => {
            let be = backend(&cli).await?;
            let r = be.call("action.list", json!({})).await?;
            for a in r["action_types"].as_array().into_iter().flatten() {
                let params: Vec<String> = a["parameters"]
                    .as_object()
                    .map(|m| {
                        m.iter().map(|(k, v)| if v["required"] == true { k.clone() } else { format!("{k}?") }).collect()
                    })
                    .unwrap_or_default();
                println!(
                    "{:<12} {:<38} {}",
                    st.cyan(a["name"].as_str().unwrap_or("")),
                    params.join(" "),
                    st.dim(a["description"].as_str().unwrap_or(""))
                );
            }
            Ok(())
        }
        Command::Do { action: Some(action), params, confirm, dry_run } => {
            let be = backend(&cli).await?;
            let input = json!({
                "action": action, "params": parse_params(params)?, "confirm": confirm,
                "dry_run": dry_run, "workspace": cli.workspace,
            });
            print_receipt(st, &be.call("action.apply", input).await?);
            Ok(())
        }
        Command::Undo { receipt, force } => {
            let be = backend(&cli).await?;
            print_receipt(st, &be.call("action.undo", json!({ "receipt": receipt, "force": force })).await?);
            Ok(())
        }
        Command::History { id } => {
            let be = backend(&cli).await?;
            let r = be.call("entity.history", json!({ "id": id })).await?;
            for h in r["history"].as_array().into_iter().flatten() {
                let changed: Vec<&str> =
                    h["changed"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                println!(
                    "{} rev {:<3} {:<10} {}{}",
                    st.dim(h["at"].as_str().unwrap_or("")),
                    h["revision"],
                    h["actor"].as_str().unwrap_or(""),
                    changed.join(", "),
                    h["state"].as_str().map(|s| format!("  [{s}]")).unwrap_or_default()
                );
            }
            Ok(())
        }
        Command::Changes { days, within } => {
            let be = backend(&cli).await?;
            let r = be
                .call("graph.changes", json!({ "since_days": days, "within": within, "workspace": cli.workspace }))
                .await?;
            let list = r["changes"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("No changes in the last {days} day(s).");
            }
            for c in list {
                let changed_fields: Vec<&str> =
                    c["changed"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let what = if c["created"] == true && !changed_fields.is_empty() {
                    format!("created, then changed {}", changed_fields.join(", "))
                } else if c["created"] == true {
                    "created".to_string()
                } else if c["deleted"] == true {
                    "deleted".to_string()
                } else if c["from_state"] != c["to_state"] && !c["to_state"].is_null() {
                    format!("{} → {}", c["from_state"].as_str().unwrap_or("∅"), c["to_state"].as_str().unwrap_or("∅"))
                } else {
                    let f: Vec<&str> = c["changed"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    format!("updated {}", f.join(", "))
                };
                println!("{} {}", entity_line(st, &json!({ "id": c["id"], "title": c["title"] })), st.dim(&what));
            }
            Ok(())
        }
        Command::Metric { name, by, kind, measure, within, days } => {
            let be = backend(&cli).await?;
            if name.is_none() && kind.is_none() && measure.is_none() {
                let r = be.call("metric.list", json!({})).await?;
                for m in r["metrics"].as_array().into_iter().flatten() {
                    let cert = if m["certified"] == true { "✓" } else { " " };
                    println!(
                        "{cert} {:<26} {}",
                        st.cyan(m["name"].as_str().unwrap_or("")),
                        st.dim(m["description"].as_str().unwrap_or(""))
                    );
                }
                return Ok(());
            }
            let q = json!({
                "metric": name, "group_by": by, "kind": kind, "measure": measure,
                "within": within, "window_days": days, "workspace": cli.workspace,
            });
            let r = be.call("metric.query", q).await?;
            println!(
                "{} = {} {}{}",
                st.bold(r["metric"].as_str().unwrap_or("")),
                r["value"],
                r["unit"].as_str().unwrap_or(""),
                r["window_days"].as_u64().map(|d| format!(" (last {d} days)")).unwrap_or_default()
            );
            for row in r["rows"].as_array().into_iter().flatten() {
                let g: Vec<&str> = row["group"]
                    .as_object()
                    .map(|m| m.values().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if !g.is_empty() {
                    println!("  {:<30} {}", g.join(" / "), row["value"]);
                }
            }
            Ok(())
        }
        Command::Quality { kind } => {
            let be = backend(&cli).await?;
            let r = be.call("quality.report", json!({ "kind": kind, "workspace": cli.workspace })).await?;
            println!("{} {}/100 across {} entities", st.bold("Data quality"), r["score"], r["entities"]);
            for (k, q) in r["by_kind"].as_object().into_iter().flatten() {
                println!("  {:<14} {:>5}  ({} of {} with issues)", k, q["score"], q["with_issues"], q["entities"]);
            }
            for v in r["violations"].as_array().into_iter().flatten().take(25) {
                let sev = v["severity"].as_str().unwrap_or("");
                let tag = if sev == "error" { st.red(sev) } else { st.dim(sev) };
                println!("  {tag:<9} {:<18} {}", v["rule"].as_str().unwrap_or(""), v["message"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        Command::Dupes { kind, threshold } => {
            let be = backend(&cli).await?;
            let r = be
                .call("entity.duplicates", json!({ "kind": kind, "threshold": threshold, "workspace": cli.workspace }))
                .await?;
            let pairs = r["pairs"].as_array().cloned().unwrap_or_default();
            if pairs.is_empty() {
                println!("No likely duplicates.");
            }
            for d in pairs {
                let reasons: Vec<&str> =
                    d["reasons"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                println!(
                    "{:.2}  {} ≈ {}  {}",
                    d["score"].as_f64().unwrap_or(0.0),
                    d["a"].as_str().unwrap_or(""),
                    d["b"].as_str().unwrap_or(""),
                    st.dim(&reasons.join(", "))
                );
            }
            Ok(())
        }
        Command::Merge { keep, duplicate } => {
            let be = backend(&cli).await?;
            let r = be
                .call("entity.merge", json!({ "keep": keep, "merge": duplicate, "workspace": cli.workspace }))
                .await?;
            println!(
                "Merged {} into {} ({} links moved); the old id now redirects.",
                r["merged"].as_str().unwrap_or_default(),
                r["kept"].as_str().unwrap_or_default(),
                r["links_moved"]
            );
            Ok(())
        }
        Command::Whatif { id, state } => {
            let be = backend(&cli).await?;
            let q = format!("What if {id} were {state}?");
            let r =
                be.call("intelli.ask", json!({ "question": q, "workspace": cli.workspace, "no_cache": true })).await?;
            print_answer(st, &r, false);
            Ok(())
        }
        Command::Experts { topic } => {
            let be = backend(&cli).await?;
            let r = be.call("people.experts", json!({ "topic": topic.join(" "), "workspace": cli.workspace })).await?;
            let list = r["experts"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("Nobody is connected to that topic yet.");
            }
            for e in list {
                let reasons: Vec<&str> =
                    e["reasons"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                println!(
                    "{:>6.2}  {}  {}",
                    e["score"].as_f64().unwrap_or(0.0),
                    entity_line(st, &e),
                    st.dim(&reasons.join("; "))
                );
            }
            Ok(())
        }
        Command::Export { format, start, depth, no_schema } => {
            let be = backend(&cli).await?;
            let r = be
                .call(
                    "export.graph",
                    json!({ "format": format, "start": start, "depth": depth, "include_schema": !no_schema, "workspace": cli.workspace }),
                )
                .await?;
            print!("{}", r["content"].as_str().unwrap_or_default());
            Ok(())
        }
        Command::Mcp => {
            let (intelli, cfg) = open_local(&cli).await?;
            let server =
                onto_intelli::mcp::McpServer::new(intelli, cfg.auth.default_principal.clone(), cfg.mcp.allow_writes);
            let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
            let mut out = tokio::io::stdout();
            while let Some(line) = lines.next_line().await? {
                if line.trim().is_empty() {
                    continue;
                }
                let reply = match serde_json::from_str::<Value>(&line) {
                    Ok(msg) => server.handle(msg).await,
                    Err(e) => Some(
                        json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("parse error: {e}") } }),
                    ),
                };
                if let Some(r) = reply {
                    out.write_all(format!("{r}\n").as_bytes()).await?;
                    out.flush().await?;
                }
            }
            Ok(())
        }
        Command::Eval { file, json: as_json } => {
            let be = backend(&cli).await?;
            let input = match file {
                Some(f) => json!({ "toml": std::fs::read_to_string(f)? }),
                None => json!({}),
            };
            let r = be.call("eval.run", input).await?;
            if *as_json {
                print_json(&r);
                return Ok(());
            }
            for c in r["cases"].as_array().into_iter().flatten() {
                let mark = if c["passed"] == true { st.bold("PASS") } else { st.red("FAIL") };
                println!(
                    "{mark} {:<40} {}",
                    c["question"].as_str().unwrap_or(""),
                    st.dim(&format!("{} · {} tokens · {} ms", c["mode"].as_str().unwrap_or(""), c["tokens"], c["ms"]))
                );
                for f in c["failures"].as_array().into_iter().flatten() {
                    println!("     {}", st.red(f.as_str().unwrap_or("")));
                }
            }
            println!(
                "\n{}/{} passed ({}%) · {}% deterministic · avg {} ms · {} prompt tokens",
                r["passed"], r["total"], r["pass_rate"], r["deterministic_share"], r["avg_ms"], r["prompt_tokens"]
            );
            if r["failed"].as_u64().unwrap_or(0) > 0 {
                std::process::exit(2);
            }
            Ok(())
        }
        Command::Schema { json: as_json, sub: None } => {
            // The live schema of the store (or server), not just the config files.
            let be = backend(&cli).await?;
            let mut doc = be.call("schema.get", json!({})).await?;
            let version = doc.as_object_mut().and_then(|m| m.remove("schema_version"));
            if *as_json {
                print_json(&doc)
            } else {
                let schema = onto_intelli::Schema::from_json(doc)?;
                if let Some(v) = version {
                    println!("{}", st.dim(&format!("live schema version {v}")));
                }
                print!("{}", schema.describe())
            }
            Ok(())
        }
        Command::Protos => {
            let cfg = Config::discover(cli.config.as_deref())?;
            let table = onto_grpc::RouteTable::build(&cfg)?;
            println!("{} routes ({} services)", table.routes.len(), table.pool.services().len());
            for r in table.describe() {
                let tag = if r.builtin { st.dim("builtin") } else { st.cyan("custom ") };
                println!(
                    "{tag} {:<58} → {:<20} {}",
                    r.path,
                    r.action,
                    st.dim(&format!("{} → {} [{}]", r.input, r.output, r.streaming))
                );
            }
            Ok(())
        }
        Command::Stats => {
            let be = backend(&cli).await?;
            print_json(&be.call("system.stats", json!({})).await?);
            Ok(())
        }
    }
}

fn init(dir: &Path, force: bool, st: &Style) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let cfg_path = dir.join("onto-intelli.toml");
    if cfg_path.exists() && !force {
        return Err(Error::Conflict(format!("{} exists (use --force to overwrite)", cfg_path.display())));
    }
    std::fs::write(&cfg_path, EXAMPLE_CONFIG)?;
    let proto = dir.join("protos").join(EXAMPLE_PROTO_PATH);
    std::fs::create_dir_all(proto.parent().expect("proto has a parent"))?;
    if !proto.exists() || force {
        std::fs::write(&proto, EXAMPLE_PROTO)?;
    }
    std::fs::create_dir_all(dir.join(".onto"))?;
    println!("Created {} and {}", st.bold(&cfg_path.display().to_string()), proto.display());
    println!("\nNext:\n  onto seed demo\n  onto ask \"Who owns Project Alpha?\"\n  onto serve");
    Ok(())
}

async fn chat(cli: &Cli, st: &Style) -> Result<()> {
    let be = backend(cli).await?;
    let mut conversation = "new".to_string();
    println!("{}", st.dim("onto chat — ask anything about your workspace. 'new' starts over, 'exit' quits."));
    let stdin = std::io::stdin();
    loop {
        print!("{} ", st.cyan("you ›"));
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let q = line.trim();
        match q {
            "" => continue,
            "exit" | "quit" | ":q" => break,
            "new" => {
                conversation = "new".into();
                println!("{}", st.dim("(new conversation)"));
                continue;
            }
            _ => {}
        }
        match be
            .call("intelli.ask", json!({ "question": q, "conversation_id": conversation, "workspace": cli.workspace }))
            .await
        {
            Ok(r) => {
                if let Some(c) = r["conversation_id"].as_str() {
                    conversation = c.to_string();
                }
                print!("{} ", st.bold("onto ›"));
                print_answer(st, &r, false);
            }
            Err(e) => println!("{} {e}", st.red("error:")),
        }
    }
    Ok(())
}
