//! `onto` — the onto-intelli command line.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

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
    /// Print the ontology schema.
    Schema {
        #[arg(long)]
        json: bool,
    },
    /// Compile configured proto files and print the gRPC route table.
    Protos,
    /// Graph, memory, cache and token statistics.
    Stats,
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
        Command::Schema { json: as_json } => {
            let cfg = Config::discover(cli.config.as_deref())?;
            let schema = onto_intelli::intelli::load_schema(&cfg)?;
            if *as_json {
                print_json(&serde_json::to_value(&schema)?)
            } else {
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
