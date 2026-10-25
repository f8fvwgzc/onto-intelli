//! `onto` — the onto-intelli command line.

mod assure;

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use onto_intelli::config::{Config, EXAMPLE_CONFIG, EXAMPLE_PROTO, EXAMPLE_PROTO_PATH};
use onto_intelli::trace::TraceCtx;
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
    /// Tenant (default: engine.postgres.tenant); each tenant is its own ontology.
    #[arg(long, global = true, env = "ONTO_TENANT")]
    tenant: Option<String>,
    /// PostgreSQL URL (default: engine.database_url, then $ONTO_DATABASE_URL, then $DATABASE_URL).
    #[arg(long, global = true, value_name = "URL")]
    database_url: Option<String>,
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
        /// Development mode: no authentication, every caller is auth.dev_principal
        /// (loopback addresses only).
        #[arg(long)]
        dev: bool,
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
        /// Plan only, with the derived facts the edits add and remove (consequences).
        #[arg(long)]
        consequences: bool,
        #[arg(long)]
        json: bool,
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
    Mcp {
        /// Tool profile: full (default), catalog, authoring, reasoning, governance,
        /// retrieval or evaluation. Precedence: this flag, $ONTO_MCP_PROFILE, `[mcp] profile`.
        #[arg(long, env = "ONTO_MCP_PROFILE")]
        profile: Option<String>,
    },
    /// Run competency questions (default: the built-in demo suite).
    Eval {
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Register a data source and crawl it: metadata only, never rows.
    ///
    /// postgres: `onto connect postgres env:WAREHOUSE_URL --name warehouse`.
    /// repo: `onto connect repo <path | git URL>` maps a monorepo's packages,
    /// internal dependencies and CODEOWNERS (manifests only, never source files).
    Connect {
        /// Kind of source: postgres or repo.
        kind: String,
        /// postgres: postgres://user:pass@host/db, or env:VAR to read it at crawl time (keeps the secret out of
        /// the store). repo: a directory, or a git URL (https://…, ssh://…, git@host:path).
        url: String,
        /// Connector name, part of entity ids (default: the database or repository name).
        #[arg(long)]
        name: Option<String>,
        /// postgres: schemas to crawl, `*` wildcards (repeatable; default: all).
        #[arg(long = "schema")]
        schemas: Vec<String>,
        /// postgres: schemas to skip (repeatable; default: onto_*). repo: directories to skip, gitignore-style.
        #[arg(long = "exclude")]
        exclude: Vec<String>,
        /// repo: only packages under these directories, gitignore-style (repeatable; default: all).
        #[arg(long = "include")]
        include: Vec<String>,
        /// repo: branch to clone (git URLs; default: the remote's default branch).
        #[arg(long)]
        branch: Option<String>,
        /// Background crawl interval: 15m, 1h, 900s, or 0 for on demand only (default: 15m, repo 1h).
        #[arg(long)]
        every: Option<String>,
        /// postgres: turn table grants into entity ACLs (see --grant).
        #[arg(long)]
        map_grants: bool,
        /// postgres: role=principal, whom a database role's grants map to (repeatable), e.g. analyst=team:analytics.
        #[arg(long = "grant")]
        grants: Vec<String>,
        /// postgres: role=owner, who owns what a database role owns (repeatable), e.g. finance_owner=team:finance.
        /// repo: owner=id, a CODEOWNERS owner mapped to an existing id (repeatable), e.g. @acme/payments=team:payments.
        #[arg(long = "owner")]
        owners: Vec<String>,
        #[arg(long)]
        title: Option<String>,
        /// Register only; crawl later (`onto crawl`).
        #[arg(long)]
        no_crawl: bool,
    },
    /// List connectors, or remove one.
    Connectors {
        #[arg(long)]
        remove: Option<String>,
        /// With --remove: also delete what it crawled.
        #[arg(long)]
        purge: bool,
    },
    /// Crawl a connector now (every connector without a name).
    Crawl {
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// OntoRank: an entity's authority and what it is made of, or the top assets.
    Rank {
        id: Option<String>,
        /// How many of the top assets to list.
        #[arg(long, default_value_t = 15)]
        top: usize,
        /// Kinds to list (repeatable).
        #[arg(long)]
        kind: Vec<String>,
        /// Recompute first.
        #[arg(long)]
        recompute: bool,
        #[arg(long)]
        json: bool,
    },
    /// Learned facts (join paths, metric definitions, synonyms); certify or reject one.
    Learned {
        #[arg(long = "type")]
        fact_type: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        certify: Option<String>,
        #[arg(long)]
        reject: Option<String>,
        #[arg(long)]
        note: Option<String>,
        /// Infer again from the catalog.
        #[arg(long)]
        relearn: bool,
    },
    /// Build an ontology from a crawled data source: draft object, link and
    /// action types, refine them with the LLM, ask for advice, apply or propose.
    Model {
        #[command(subcommand)]
        sub: ModelCmd,
    },
    /// Object Explorer: object types with counts, or the objects of one type.
    Explore {
        /// Object type (or interface); none lists the types.
        kind: Option<String>,
        /// Conditions (repeatable): prop=value, prop!=value, prop=a,b (any of), prop>n, prop>=n,
        /// prop<n, prop<=n, prop~text (contains), prop? (set), !prop (missing), link:rel=id.
        #[arg(long = "filter")]
        filters: Vec<String>,
        #[arg(long)]
        search: Option<String>,
        /// prop ascending, -prop descending.
        #[arg(long)]
        sort: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Control plane administration: opens the control store directly (operator).
    Console {
        #[command(subcommand)]
        sub: ConsoleCmd,
    },
    /// Reasoning: closure, pattern queries, explanations, claim checks, certificates.
    ///
    /// Derived facts are the engine's opinion until a certificate passes the
    /// independent checker (`onto reason certify --out DIR`, `onto cert check DIR`).
    Reason {
        #[command(subcommand)]
        sub: assure::ReasonCmd,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Check reasoning certificates offline with the onto-cert library (no store, no server).
    Cert {
        #[command(subcommand)]
        sub: assure::CertCmd,
    },
    /// Shapes: validate the graph (exit 0 conforms, 1 violates, 3 undetermined,
    /// 4 the command failed), induce a shape from data, check shape definitions.
    Shapes {
        #[command(subcommand)]
        sub: assure::ShapesCmd,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Lint the schema (a measurement); dismiss or accept findings. Exits 1 when an
    /// error finding is shown (dismissed findings listed with --all do not count),
    /// 4 when the command fails.
    Lint(assure::LintArgs),
    /// Lock a schema definition: changes that delete, rename or break it are refused (admins).
    Lock(assure::LockArgs),
    /// Release a schema lock (admins).
    Unlock(assure::UnlockArgs),
    /// Active schema locks.
    Locks {
        #[arg(long)]
        json: bool,
    },
    /// Industry packs: list, show, plan and install (or propose) them.
    Pack {
        #[command(subcommand)]
        sub: assure::PackCmd,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Align a pack or a schema TOML with the current schema (scores are an
    /// opinion); write SSSOM, or certify mappings against both hierarchies.
    Align(assure::AlignArgs),
    /// One sheet in, one ontology out: induce an object type, links and a shape
    /// from a CSV file (read locally), or ingest its rows with --ingest.
    Induce(assure::InduceArgs),
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
    /// File a change set as a proposal for review.
    Propose {
        file: PathBuf,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        rationale: String,
    },
    /// Schema change proposals.
    Proposals {
        /// open, applied, rejected, failed or withdrawn.
        #[arg(long)]
        status: Option<String>,
    },
    /// One proposal: rationale, impact, reviews (fresh preview when stale).
    Proposal { id: String },
    /// Approve a proposal; the last needed approval applies it.
    Approve {
        id: String,
        #[arg(long, default_value = "")]
        comment: String,
    },
    /// Reject a proposal.
    Reject {
        id: String,
        #[arg(long, default_value = "")]
        comment: String,
    },
    /// Withdraw an open or failed proposal.
    Withdraw { id: String },
}

#[derive(Subcommand)]
enum ConsoleCmd {
    /// Create the `default` organization with the local administrator as its
    /// owner, and adopt existing tenants as its projects.
    Bootstrap,
    /// Users with their organizations and roles.
    Users,
    /// Make a user an owner of an organization (id or slug).
    AddOwner { user: String, org: String },
    /// A project's API keys, or create one with --create (its secret is printed once).
    Keys {
        project_ref: String,
        /// Name of a new key.
        #[arg(long, value_name = "NAME")]
        create: Option<String>,
        /// Scopes of the new key (repeatable): read, write, agent, admin. The
        /// global --workspace limits it to one workspace (default: all).
        #[arg(long = "scope")]
        scopes: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Rewrap every stored project LLM key under ONTO_MASTER_KEY (set the
    /// old key as ONTO_MASTER_KEY_PREVIOUS); reports the rows it changed.
    RotateMasterKey {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ModelCmd {
    /// Draft object, link and action types from a source's catalog, with the reasons.
    Draft {
        /// Connector name (default: the only crawled source).
        source: Option<String>,
        #[command(flatten)]
        opts: DraftArgs,
        #[arg(long)]
        json: bool,
    },
    /// The draft refined by the LLM; suggestions that break the schema are rejected.
    Refine {
        source: Option<String>,
        #[command(flatten)]
        opts: DraftArgs,
        /// What the model should pay attention to.
        #[arg(long)]
        instructions: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Modeling advice grounded in the draft.
    Advise {
        question: Vec<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Apply the draft as a new schema version (apply roles), or file it as a proposal.
    Apply {
        source: Option<String>,
        #[command(flatten)]
        opts: DraftArgs,
        /// File a proposal for review instead of applying.
        #[arg(long)]
        propose: bool,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        rationale: Option<String>,
        /// Do not crawl the source after applying.
        #[arg(long)]
        no_crawl: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args)]
struct DraftArgs {
    /// Tables to model, schema.table or globs (repeatable; default: all).
    #[arg(long = "table")]
    tables: Vec<String>,
    /// Tables to leave out, globs (repeatable; default: staging.*, *_tmp, *_old, *_bak, *_backup).
    #[arg(long)]
    exclude: Vec<String>,
    /// Put before every object type name.
    #[arg(long)]
    prefix: Option<String>,
    /// Do not materialize rows as objects.
    #[arg(long)]
    no_sync: bool,
    /// No action types.
    #[arg(long)]
    no_actions: bool,
    /// Model views too.
    #[arg(long)]
    views: bool,
}

impl DraftArgs {
    fn input(&self, source: &Option<String>) -> Value {
        let mut v = json!({
            "source": source,
            "tables": self.tables,
            "prefix": self.prefix,
            "no_sync": self.no_sync,
            "no_actions": self.no_actions,
            "views": self.views,
        });
        if !self.exclude.is_empty() {
            v["exclude"] = json!(self.exclude);
        }
        v
    }
}

fn s_of(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

/// "1 link type", "3 link types".
fn n_of(n: u64, what: &str) -> String {
    if n == 1 {
        return format!("{n} {what}");
    }
    // entity → entities, but key → keys.
    match what.strip_suffix('y') {
        Some(stem) if !stem.ends_with(['a', 'e', 'i', 'o', 'u']) => format!("{n} {stem}ies"),
        _ => format!("{n} {what}s"),
    }
}

fn print_draft(st: &Style, d: &Value) {
    let count = |k: &str| d[k].as_array().map(Vec::len).unwrap_or(0);
    println!(
        "Draft from {}: {} · {} · {}",
        st.bold(s_of(&d["source"])),
        n_of(count("objects") as u64, "object type"),
        n_of(count("links") as u64, "link type"),
        n_of(count("actions") as u64, "action type")
    );
    for o in d["objects"].as_array().into_iter().flatten() {
        let key: Vec<&str> = o["key"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let mut head = format!(
            "key {} · title {} · {}",
            key.join(", "),
            o["title"].as_str().unwrap_or("none"),
            s_of(&o["category"])
        );
        if let Some(states) = o["states"].as_array().filter(|a| !a.is_empty()) {
            let states: Vec<&str> = states.iter().filter_map(Value::as_str).collect();
            head.push_str(&format!(" · states {}", states.join("|")));
        }
        println!(
            "\n{} {} ← {}  {}",
            st.bold(s_of(&o["name"])),
            s_of(&o["label"]),
            s_of(&o["table"]),
            st.dim(&format!("{head} · confidence {:.2}", o["confidence"].as_f64().unwrap_or(0.0)))
        );
        for p in o["properties"].as_array().into_iter().flatten() {
            let mut flags = Vec::new();
            for f in ["required", "unique", "searchable"] {
                if p[f] == true {
                    flags.push(f);
                }
            }
            println!(
                "    {:<22} {:<10} {:<20} {}",
                s_of(&p["name"]),
                st.cyan(s_of(&p["ty"])),
                s_of(&p["data_type"]),
                st.dim(&format!("{} {}", flags.join(" "), s_of(&p["reason"])))
            );
        }
        for r in o["reasons"].as_array().into_iter().flatten() {
            println!("    {}", st.dim(&format!("· {}", s_of(r))));
        }
    }
    if count("links") > 0 {
        println!("\n{}", st.bold("Link types"));
        for l in d["links"].as_array().into_iter().flatten() {
            println!(
                "  {:<20} {} → {} ({}, {}) inverse {}  {}",
                s_of(&l["name"]),
                s_of(&l["from"]),
                s_of(&l["to"]),
                s_of(&l["cardinality"]),
                s_of(&l["via"]).replace('_', " "),
                l["inverse"].as_str().filter(|i| !i.is_empty()).unwrap_or("none"),
                st.dim(s_of(&l["reason"]))
            );
        }
    }
    if count("actions") > 0 {
        println!("\n{}", st.bold("Action types"));
        for a in d["actions"].as_array().into_iter().flatten() {
            println!(
                "  {:<26} {:<10} {:<14} risk {:<6}  {}",
                s_of(&a["name"]),
                s_of(&a["kind"]),
                s_of(&a["object"]),
                s_of(&a["risk"]),
                st.dim(s_of(&a["reason"]))
            );
        }
    }
    if count("skipped") > 0 {
        println!("\n{}", st.bold("Not modeled"));
        for k in d["skipped"].as_array().into_iter().flatten() {
            println!("  {:<32} {}", s_of(&k["table"]), st.dim(s_of(&k["reason"])));
        }
    }
    for n in d["notes"].as_array().into_iter().flatten() {
        println!("{} {}", st.dim("note:"), s_of(n));
    }
}

/// `--filter` → an explorer filter: `prop=value`, `prop!=v`, `prop=a,b`,
/// `prop>n`, `prop>=n`, `prop<n`, `prop<=n`, `prop~text`, `prop?`, `!prop`.
fn parse_filter(f: &str) -> Result<Value> {
    let value = |v: &str| serde_json::from_str::<Value>(v).unwrap_or_else(|_| json!(v));
    if let Some(prop) = f.strip_prefix('!').filter(|p| !p.contains(['=', '<', '>', '~'])) {
        return Ok(json!({ "prop": prop, "op": "missing" }));
    }
    if let Some(prop) = f.strip_suffix('?') {
        return Ok(json!({ "prop": prop, "op": "exists" }));
    }
    for (sep, op) in
        [("!=", "ne"), (">=", "gte"), ("<=", "lte"), ("=", "eq"), (">", "gt"), ("<", "lt"), ("~", "contains")]
    {
        if let Some((prop, v)) = f.split_once(sep).filter(|(p, _)| !p.is_empty()) {
            if op == "eq" && v.contains(',') && !v.starts_with('[') {
                let list: Vec<Value> = v.split(',').map(|x| value(x.trim())).collect();
                return Ok(json!({ "prop": prop, "op": "in", "value": list }));
            }
            return Ok(json!({ "prop": prop, "op": op, "value": value(v) }));
        }
    }
    Err(Error::invalid(format!("--filter '{f}': use prop=value, prop>n, prop~text, prop? or !prop")))
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
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
    // As `SchemaPlan::applicable()`: a lock violation not overridden refuses
    // the apply whether or not any data changes.
    let locks = plan["locked_violations"].as_array().cloned().unwrap_or_default();
    let locked = locks.iter().any(|v| v["overridden"] != true);
    let data = if plan["breaking"].as_bool().unwrap_or(false) {
        st.cyan("breaking, migrated")
    } else {
        st.bold("non-breaking")
    };
    let verdict = if !valid {
        st.red("invalid")
    } else if !needs.is_empty() {
        st.red("needs migration")
    } else if locked {
        st.red("locked: apply refused")
    } else if !locks.is_empty() {
        format!("{}, {data}", st.cyan("locked (overridden)"))
    } else {
        data
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
    assure::print_risk(st, plan);
    assure::print_semantics(st, plan);
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
    let line =
        [mark, r["action"].as_str().unwrap_or("").to_string(), r["id"].as_str().map(|i| st.dim(i)).unwrap_or_default()];
    println!("{}", line.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" "));
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
    assure::print_semantics(st, plan);
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
    Remote { http: reqwest::Client, base: String, token: Option<String>, tenant: Option<String> },
}

impl Backend {
    async fn call(&self, action: &str, input: Value) -> Result<Value> {
        match self {
            Backend::Local { intelli, principal } => {
                let t = intelli.call_traced(principal, action, input, TraceCtx::from_source("cli")).await;
                // The process may exit right after: write its trace now.
                if t.trace_id.is_some() || action == "eval.run" {
                    intelli.flush_traces().await;
                }
                t.result
            }
            Backend::Remote { http, base, token, tenant } => {
                let mut req = http.post(format!("{base}/v1/call/{action}")).json(&input);
                if let Some(t) = token {
                    req = req.bearer_auth(t);
                }
                if let Some(t) = tenant {
                    req = req.header("x-onto-tenant", t);
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
                        Some("conflict") => Error::Conflict(msg),
                        Some("resource_exhausted") => Error::Limited(msg),
                        Some("unauthenticated") => Error::Denied(format!("{msg} (pass --token or set ONTO_TOKEN)")),
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

/// A connector name from a URL's database ("postgres://…/analytics" → analytics).
fn connector_name(url: &str) -> String {
    let db = url
        .rsplit_once('/')
        .map(|(_, d)| d.split('?').next().unwrap_or_default())
        .filter(|_| url.contains("://"))
        .unwrap_or_default();
    let name: String = db
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .take(40)
        .collect();
    if name.is_empty() { "warehouse".into() } else { name }
}

/// A repository source as the server should store it: git URLs as given,
/// local directories made absolute (a path for a remote server must be
/// absolute already: it names a directory on that server).
fn repo_source(src: &str, remote: bool) -> Result<String> {
    let src = src.trim();
    if onto_intelli::connectors::repo::looks_like_url(src) {
        return Ok(src.to_string());
    }
    let path = std::path::Path::new(src);
    if remote {
        return if path.is_absolute() {
            Ok(src.to_string())
        } else {
            Err(Error::invalid(format!("'{src}': with --remote, give the repository's absolute path on the server")))
        };
    }
    let abs = path
        .canonicalize()
        .map_err(|e| Error::invalid(format!("repository '{src}': {e} (give a directory or a git URL)")))?;
    if !abs.is_dir() {
        return Err(Error::invalid(format!("repository '{src}' is not a directory")));
    }
    Ok(abs.to_string_lossy().to_string())
}

/// "15m" → 900, "1h" → 3600, "90s"/"90" → 90, "0" → 0.
fn parse_every(s: &str) -> Result<u64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().map_err(|_| Error::invalid(format!("--every '{s}': use 15m, 1h, 900s or 0")))?;
    Ok(match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return Err(Error::invalid(format!("--every '{s}': use 15m, 1h, 900s or 0"))),
    })
}

fn print_crawl(st: &Style, c: &Value) {
    if let Some(r) = c["repo"].as_object() {
        let eco: Vec<String> = r
            .get("ecosystems")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| format!("{v} {k}")).collect())
            .unwrap_or_default();
        println!(
            "Crawled {} in {} ms{}: {} packages{}, {} dependencies, {} owners · {} written, {} unchanged, {} removed",
            st.bold(c["connector"].as_str().unwrap_or_default()),
            c["ms"],
            r.get("commit")
                .and_then(Value::as_str)
                .map(|c| format!(" at {}", &c[..c.len().min(12)]))
                .unwrap_or_default(),
            r.get("packages").cloned().unwrap_or_default(),
            if eco.is_empty() { String::new() } else { format!(" ({})", eco.join(", ")) },
            r.get("dependencies").cloned().unwrap_or_default(),
            r.get("owners").cloned().unwrap_or_default(),
            c["upserted"],
            c["unchanged"],
            c["deleted"],
        );
        for n in c["notes"].as_array().cloned().unwrap_or_default() {
            println!("  {} {}", st.dim("note:"), n.as_str().unwrap_or_default());
        }
        return;
    }
    println!(
        "Crawled {} in {} ms: {} schemas, {} tables, {} views, {} columns, {} queries · {} written, {} unchanged, {} removed · learned {} · ranked {}",
        st.bold(c["connector"].as_str().unwrap_or_default()),
        c["ms"],
        c["schemas"],
        c["tables"],
        c["views"],
        c["columns"],
        c["queries"],
        c["upserted"],
        c["unchanged"],
        c["deleted"],
        c["learned"]["written"],
        c["ranked"]
    );
    if let Some(objects) = c["objects"].as_object() {
        for (ty, o) in objects {
            println!(
                "  {} {} rows · {} written, {} unchanged, {} removed{}",
                st.cyan(ty),
                o["rows"],
                o["upserted"],
                o["unchanged"],
                o["deleted"],
                if o["truncated"] == true { " · truncated at max_rows" } else { "" }
            );
        }
    }
    for n in c["notes"].as_array().cloned().unwrap_or_default() {
        println!("  {} {}", st.dim("note:"), n.as_str().unwrap_or_default());
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

/// The exit code when `command` fails. Commands whose exit code is a verdict
/// (shapes validate/check, lint, reason certify, cert) use a code no verdict
/// uses, so a script can tell a failed run from a negative verdict.
fn failure_exit_code(command: &Command) -> i32 {
    let verdict = match command {
        Command::Shapes { sub, .. } => {
            matches!(sub, assure::ShapesCmd::Validate { .. } | assure::ShapesCmd::Check { .. })
        }
        Command::Reason { sub, .. } => matches!(sub, assure::ReasonCmd::Certify { .. }),
        Command::Lint(_) | Command::Cert { .. } => true,
        _ => false,
    };
    if verdict { assure::EXIT_FAILED } else { 1 }
}

/// Windows gives the main thread 1 MiB of stack (Linux and macOS 8 MiB), and
/// clap's parser for the whole command tree overflows it in a debug build,
/// so the command runs on a thread of its own with 8 MiB.
fn main() {
    let main = std::thread::Builder::new()
        .name("main".into())
        .stack_size(8 << 20)
        .spawn(|| tokio::runtime::Runtime::new().expect("starting the tokio runtime").block_on(cli_main()))
        .expect("starting the main thread");
    if main.join().is_err() {
        // The panic message is printed; exit as a panicking main would.
        std::process::exit(101);
    }
}

async fn cli_main() {
    quiet_broken_pipe();
    let cli = Cli::parse();
    let level = if matches!(cli.command, Command::Serve { .. }) { "info" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into()))
        .with_writer(std::io::stderr)
        .init();
    let st = Style { on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() };
    let failed = failure_exit_code(&cli.command);
    if let Err(e) = run(cli, &st).await {
        eprintln!("{} {e}", st.red("error:"));
        std::process::exit(failed);
    }
}

/// A reader that closes the pipe early (`onto ... | head -1`) ends the
/// command quietly, as a SIGPIPE would, instead of a panic from `println!`.
fn quiet_broken_pipe() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or_default();
        // "Broken pipe" on Unix; Windows says ERROR_NO_DATA (232) or ERROR_BROKEN_PIPE (109).
        let closed = ["Broken pipe", "(os error 232)", "(os error 109)"].iter().any(|m| msg.contains(m));
        if msg.contains("failed printing to stdout") && closed {
            // 128 + SIGPIPE, what a shell reports for a process the signal ends.
            std::process::exit(141);
        }
        default(info);
    }));
}

fn local_config(cli: &Cli) -> Result<Config> {
    let mut cfg = Config::discover(cli.config.as_deref())?;
    if let Some(ws) = &cli.workspace {
        cfg.engine.default_workspace = ws.clone();
    }
    if let Some(url) = &cli.database_url {
        cfg.engine.database_url = Some(url.clone());
    }
    Ok(cfg)
}

/// The local store (the `--tenant`'s, when given).
async fn open_local(cli: &Cli) -> Result<(Arc<Intelli>, Config)> {
    let cfg = local_config(cli)?;
    let intelli = match &cli.tenant {
        Some(t) => onto_intelli::Hub::open(cfg.clone()).await?.tenant(t).await?,
        None => Intelli::open(cfg.clone()).await?,
    };
    Ok((intelli, cfg))
}

async fn backend(cli: &Cli) -> Result<Backend> {
    match &cli.remote {
        Some(url) => Ok(Backend::Remote {
            http: reqwest::Client::new(),
            base: url.trim_end_matches('/').to_string(),
            token: cli.token.clone(),
            tenant: cli.tenant.clone(),
        }),
        None => {
            let (intelli, cfg) = open_local(cli).await?;
            Ok(Backend::Local { intelli, principal: cfg.local.principal })
        }
    }
}

async fn run(cli: Cli, st: &Style) -> Result<()> {
    match &cli.command {
        Command::Init { dir, force } => init(dir, *force, st),
        Command::Serve { listen, dev } => {
            let mut cfg = local_config(&cli)?;
            if let Some(l) = listen {
                cfg.grpc.listen = l.clone();
            }
            if let Some(t) = &cli.tenant {
                cfg.engine.postgres.tenant = t.clone();
            }
            let hub = onto_intelli::Hub::open(cfg.clone()).await?;
            // Open the default tenant now: configuration problems show at startup.
            let intelli = hub.tenant(hub.default_tenant()).await?;
            let control = if cfg.console.enabled { Some(open_control(&cfg).await?) } else { None };
            let mut auth = if *dev || cfg.auth.mode == "dev" {
                st.red("DEVELOPMENT MODE, no authentication")
            } else {
                format!("token auth ({} tokens)", cfg.auth.tokens.len())
            };
            if let Some(c) = &control {
                c.spawn_maintenance();
                let mut paths = Vec::new();
                if c.bootstrap_enabled() {
                    paths.push("bootstrap token".to_string());
                }
                if !cfg.console.oidc.is_empty() {
                    paths.push(format!("{} OIDC providers", cfg.console.oidc.len()));
                }
                if *dev || cfg.auth.mode == "dev" {
                    paths.push("dev".into());
                }
                auth.push_str(&format!(
                    " · console ({}; keys {}; LLM keys {})",
                    paths.join(", "),
                    if c.can_create_keys() { "on" } else { "off: set ONTO_KEY_PEPPER" },
                    if c.can_store_llm_keys() { "on" } else { "off: set ONTO_MASTER_KEY" }
                ));
            }
            eprintln!(
                "{} gRPC + HTTP on {} · storage {} · {} · tenant {}{} · llm {} ({})",
                st.bold("onto-intelli"),
                cfg.grpc.listen,
                cfg.engine.storage,
                auth,
                hub.default_tenant(),
                if hub.multi_tenant() { " (multi-tenant)" } else { "" },
                intelli.llm().name(),
                intelli.llm().model()
            );
            if let Some(why) = intelli.llm().unavailable() {
                eprintln!(
                    "{} LLM unavailable: {why} — deterministic answers and fact summaries still work",
                    st.red("!")
                );
            }
            onto_grpc::serve_hub(hub, onto_grpc::ServeOptions { dev: *dev, control }, async {
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
                let (mut rx, _) =
                    intelli.try_ask_stream_traced(principal.clone(), ask, TraceCtx::from_source("cli"))?;
                while let Some(ev) = rx.recv().await {
                    if ev.is_terminal() {
                        intelli.flush_traces().await;
                    }
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
            if let Some(w) = &report.warehouse {
                println!(
                    "Catalogued the demo warehouse · {} tables · {} views · {} columns · {} queries · {} learned facts",
                    w.tables, w.views, w.columns, w.queries, w.learned.written
                );
            }
            println!("{}", st.dim("Try: onto ask \"Why is Project Alpha blocked?\" --context"));
            println!(
                "{}",
                st.dim("     onto ask \"Which table holds revenue?\" · onto rank table:warehouse.sales.orders")
            );
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
                    let learned = r["learned"].as_array().cloned().unwrap_or_default();
                    if !learned.is_empty() {
                        println!("\n# Learned from the catalog, waiting for review ({}):", learned.len());
                        for l in learned {
                            println!("#   {}", l.as_str().unwrap_or_default());
                        }
                    }
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
                SchemaCmd::Propose { file, title, rationale } => {
                    let mut input = read_change_set(file)?;
                    input["title"] = json!(title);
                    input["rationale"] = json!(rationale);
                    input["source"] = json!("cli");
                    let r = be.call("schema.propose", input).await?;
                    if *as_json {
                        print_json(&r)
                    } else {
                        println!(
                            "{} {} {}",
                            st.bold("proposed"),
                            r["id"].as_str().unwrap_or(""),
                            r["title"].as_str().unwrap_or("")
                        );
                        print_plan(st, &r["plan"]);
                    }
                }
                SchemaCmd::Proposals { status } => {
                    let r = be.call("schema.proposals", json!({ "status": status })).await?;
                    if *as_json {
                        print_json(&r)
                    } else {
                        for p in r["proposals"].as_array().into_iter().flatten() {
                            let mark = match p["status"].as_str().unwrap_or("") {
                                "open" => st.cyan("open"),
                                "applied" => st.bold("applied"),
                                other => st.dim(other),
                            };
                            println!(
                                "{} {:<9} {} {} {}",
                                p["id"].as_str().unwrap_or(""),
                                mark,
                                p["title"].as_str().unwrap_or(""),
                                st.dim(&format!(
                                    "by {}{} · {}/{} approvals",
                                    p["proposer"]["id"].as_str().unwrap_or(""),
                                    if p["proposer"]["agent"].as_bool().unwrap_or(false) { " (agent)" } else { "" },
                                    p["approvals"],
                                    p["required_approvals"]
                                )),
                                if p["plan"]["breaking"].as_bool().unwrap_or(false) {
                                    st.red("breaking")
                                } else {
                                    String::new()
                                }
                            );
                        }
                    }
                }
                SchemaCmd::Proposal { id } => {
                    let r = be.call("schema.proposal", json!({ "id": id })).await?;
                    if *as_json {
                        print_json(&r)
                    } else {
                        println!(
                            "{} {} [{}]",
                            r["id"].as_str().unwrap_or(""),
                            st.bold(r["title"].as_str().unwrap_or("")),
                            r["status"].as_str().unwrap_or("")
                        );
                        println!("{}", r["rationale"].as_str().unwrap_or(""));
                        let plan = if r["preview"].is_object() { &r["preview"] } else { &r["plan"] };
                        if r["stale"].as_bool().unwrap_or(false) {
                            println!(
                                "{}",
                                st.cyan(&format!("schema moved on (v{}); preview against it:", r["active_version"]))
                            );
                        }
                        print_plan(st, plan);
                        for rv in r["reviews"].as_array().into_iter().flatten() {
                            println!(
                                "  {} {} {}",
                                rv["decision"].as_str().unwrap_or(""),
                                rv["by"].as_str().unwrap_or(""),
                                st.dim(rv["comment"].as_str().unwrap_or(""))
                            );
                        }
                    }
                }
                SchemaCmd::Approve { id, comment } | SchemaCmd::Reject { id, comment } => {
                    let action =
                        if matches!(sub, SchemaCmd::Approve { .. }) { "schema.approve" } else { "schema.reject" };
                    let r = be.call(action, json!({ "id": id, "comment": comment })).await?;
                    if *as_json {
                        print_json(&r)
                    } else {
                        println!("{} {}", st.bold(r["proposal"]["status"].as_str().unwrap_or("")), id);
                        if let Some(v) = r["applied"]["version"]["version"].as_u64() {
                            println!("applied as schema v{v}");
                        }
                    }
                }
                SchemaCmd::Withdraw { id } => {
                    let r = be.call("schema.withdraw", json!({ "id": id })).await?;
                    if *as_json { print_json(&r) } else { println!("{} {}", st.bold("withdrawn"), id) }
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
        Command::Do { action: Some(action), params, confirm, dry_run, consequences, json: as_json } => {
            let be = backend(&cli).await?;
            if *consequences {
                let input = json!({
                    "action": action, "params": parse_params(params)?, "consequences": true,
                    "workspace": cli.workspace,
                });
                let plan = be.call("action.plan", input).await?;
                if *as_json {
                    print_json(&plan);
                } else {
                    print_receipt(st, &json!({ "status": "planned", "action": action, "plan": plan }));
                }
                return Ok(());
            }
            let input = json!({
                "action": action, "params": parse_params(params)?, "confirm": confirm,
                "dry_run": dry_run, "workspace": cli.workspace,
            });
            let r = be.call("action.apply", input).await?;
            if *as_json {
                print_json(&r)
            } else {
                print_receipt(st, &r)
            }
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
        Command::Connect {
            kind,
            url,
            name,
            schemas,
            exclude,
            include,
            branch,
            every,
            map_grants,
            grants,
            owners,
            title,
            no_crawl,
        } => {
            let be = backend(&cli).await?;
            let repo = kind == "repo";
            let every = every.clone().unwrap_or_else(|| if repo { "1h".into() } else { "15m".into() });
            let pairs = |list: &[String], what: &str| -> Result<serde_json::Map<String, Value>> {
                list.iter()
                    .map(|kv| {
                        kv.split_once('=')
                            .map(|(k, v)| (k.to_string(), json!(v)))
                            .ok_or_else(|| Error::invalid(format!("--{what} takes key=id, got '{kv}'")))
                    })
                    .collect()
            };
            let url = if repo { repo_source(url, cli.remote.is_some())? } else { url.clone() };
            let name = name.clone().unwrap_or_else(|| {
                if repo { onto_intelli::connectors::repo::name_from(&url) } else { connector_name(&url) }
            });
            let mut spec = json!({
                "name": name,
                "kind": kind,
                "url": url,
                "workspace": cli.workspace,
                "every_secs": parse_every(&every)?,
                "title": title.clone().unwrap_or_default(),
            });
            if repo {
                let mut r = json!({ "owners": pairs(owners, "owner")? });
                if let Some(b) = branch {
                    r["branch"] = json!(b);
                }
                if !include.is_empty() {
                    r["include"] = json!(include);
                }
                if !exclude.is_empty() {
                    r["exclude"] = json!(exclude);
                }
                spec["repo"] = r;
            } else {
                if branch.is_some() || !include.is_empty() {
                    return Err(Error::invalid("--branch and --include apply to repo connectors"));
                }
                spec["map_grants"] = json!(*map_grants);
                spec["grant_map"] = json!(pairs(grants, "grant")?);
                spec["owner_map"] = json!(pairs(owners, "owner")?);
                if !schemas.is_empty() {
                    spec["schemas"] = json!(schemas);
                }
                if !exclude.is_empty() {
                    spec["exclude_schemas"] = json!(exclude);
                }
            }
            let saved = be.call("connector.add", spec).await?;
            println!(
                "Connected {} ({} {}), {}",
                st.bold(saved["name"].as_str().unwrap_or_default()),
                saved["kind"].as_str().unwrap_or_default(),
                saved["url"].as_str().unwrap_or_default(),
                if saved["every_secs"] == 0 {
                    "crawled on demand only".to_string()
                } else {
                    format!("crawled every {every}")
                }
            );
            if !no_crawl {
                let r = be.call("connector.crawl", json!({ "name": name })).await?;
                for c in r["crawls"].as_array().cloned().unwrap_or_default() {
                    print_crawl(st, &c);
                }
            }
            Ok(())
        }
        Command::Connectors { remove, purge } => {
            let be = backend(&cli).await?;
            if let Some(name) = remove {
                let r = be.call("connector.remove", json!({ "name": name, "purge": purge })).await?;
                println!("Removed {name} ({} crawled entities deleted)", r["deleted"]);
                return Ok(());
            }
            let r = be.call("connector.list", json!({})).await?;
            let list = r["connectors"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("No connectors. Add one: onto connect postgres env:WAREHOUSE_URL --name warehouse");
            }
            for c in list {
                let s = &c["spec"];
                let last = c["last_crawl_at"].as_i64().filter(|t| *t > 0).map(onto_intelli::core::time::fmt_ts);
                println!(
                    "{}  {} {}  every {}s · {} assets · last crawl {}{}",
                    st.bold(s["name"].as_str().unwrap_or_default()),
                    s["kind"].as_str().unwrap_or_default(),
                    s["url"].as_str().unwrap_or_default(),
                    s["every_secs"],
                    c["assets"],
                    last.unwrap_or_else(|| "never".into()),
                    c["last_error"].as_str().map(|e| format!(" · {}", st.red(e))).unwrap_or_default()
                );
            }
            Ok(())
        }
        Command::Crawl { name, json: as_json } => {
            let be = backend(&cli).await?;
            let r = be.call("connector.crawl", json!({ "name": name })).await?;
            if *as_json {
                print_json(&r);
            } else {
                let crawls = r["crawls"].as_array().cloned().unwrap_or_default();
                if crawls.is_empty() {
                    println!("No connectors to crawl. Add one: onto connect postgres <url>");
                }
                for c in crawls {
                    print_crawl(st, &c);
                }
            }
            Ok(())
        }
        Command::Rank { id, top, kind, recompute, json: as_json } => {
            let be = backend(&cli).await?;
            if *recompute {
                let r = be.call("catalog.rerank", json!({})).await?;
                println!("{}", st.dim(&format!("ranked {} entities", r["ranked"])));
            }
            match id {
                Some(id) => {
                    let a = be.call("catalog.rank", json!({ "id": id })).await?;
                    if *as_json {
                        print_json(&a);
                        return Ok(());
                    }
                    println!(
                        "{}  authority {}  rank {}",
                        st.bold(a["id"].as_str().unwrap_or_default()),
                        st.bold(&format!("{:.2}", a["score"].as_f64().unwrap_or(0.0))),
                        a["rank"]
                    );
                    let ev = &a["evidence"];
                    let rows = [
                        ("centrality", ev.get("pagerank").map(|v| format!("PageRank {v}"))),
                        ("usage", ev.get("usage").map(|v| format!("{v} scans and calls"))),
                        ("freshness", ev.get("age_days").map(|v| format!("{v} days since activity"))),
                        ("owner", ev.get("owner").and_then(Value::as_str).map(str::to_string)),
                        (
                            "documentation",
                            ev.get("documented_columns")
                                .and_then(Value::as_f64)
                                .map(|v| format!("{:.0}% of columns documented", v * 100.0)),
                        ),
                        (
                            "certification",
                            ev.get("certified_by").and_then(Value::as_str).map(|b| format!("by {b}")).or_else(|| {
                                ev.get("certified_fact").and_then(Value::as_str).map(|f| format!("subject of {f}"))
                            }),
                        ),
                    ];
                    for (name, why) in rows {
                        let v = a["components"][name].as_f64().unwrap_or(0.0);
                        println!("  {name:<14} {v:.2}  {}", st.dim(&why.unwrap_or_default()));
                    }
                }
                None => {
                    let r = be
                        .call("catalog.top", json!({ "kinds": kind, "limit": top, "workspace": cli.workspace }))
                        .await?;
                    if *as_json {
                        print_json(&r);
                        return Ok(());
                    }
                    let list = r["top"].as_array().cloned().unwrap_or_default();
                    if list.is_empty() {
                        println!("Nothing ranked yet: crawl a connector, or onto rank --recompute");
                    }
                    for a in list {
                        println!(
                            "{:>5.2}  {}",
                            a["score"].as_f64().unwrap_or(0.0),
                            a["id"].as_str().unwrap_or_default()
                        );
                    }
                }
            }
            Ok(())
        }
        Command::Learned { fact_type, state, certify, reject, note, relearn } => {
            let be = backend(&cli).await?;
            if *relearn {
                let r = be.call("learned.run", json!({ "workspace": cli.workspace })).await?;
                println!(
                    "{}",
                    st.dim(&format!(
                        "{} join paths, {} metrics, {} synonyms · {} written",
                        r["join_paths"], r["metrics"], r["synonyms"], r["written"]
                    ))
                );
            }
            for (id, action, verb) in
                [(certify, "learned.certify", "Certified"), (reject, "learned.reject", "Rejected")]
            {
                if let Some(id) = id {
                    let r = be.call(action, json!({ "id": id, "note": note })).await?;
                    println!("{verb} {}", r["fact"]["statement"].as_str().unwrap_or(id));
                    return Ok(());
                }
            }
            let r = be.call("learned.list", json!({ "fact_type": fact_type, "state": state })).await?;
            let list = r["facts"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("No learned facts yet: crawl a connector first.");
            }
            for f in list {
                let state = f["state"].as_str().unwrap_or_default();
                let tag = if state == "certified" { st.bold("certified") } else { st.dim(state) };
                println!(
                    "{tag:<10} {:.2}  {:<9} {}  {}",
                    f["confidence"].as_f64().unwrap_or(0.0),
                    f["fact_type"].as_str().unwrap_or_default(),
                    f["statement"].as_str().unwrap_or_default(),
                    st.dim(f["id"].as_str().unwrap_or_default())
                );
            }
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
        Command::Mcp { profile } => {
            let (intelli, cfg) = open_local(&cli).await?;
            let profile = onto_intelli::mcp::resolve_profile(profile.as_deref(), &cfg.mcp.profile);
            let server = onto_intelli::mcp::McpServer::new(intelli, cfg.mcp.principal.clone(), cfg.mcp.allow_writes)
                .with_profile(&profile)?;
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
        Command::Model { sub } => model(&cli, st, sub).await,
        Command::Explore { kind, filters, search, sort, limit, offset, json: as_json } => {
            let be = backend(&cli).await?;
            let Some(kind) = kind else {
                let r = be.call("objects.types", json!({ "workspace": cli.workspace })).await?;
                if *as_json {
                    print_json(&r);
                    return Ok(());
                }
                let types = r["types"].as_array().cloned().unwrap_or_default();
                if types.is_empty() {
                    println!("No object types yet.");
                }
                for t in types {
                    let backing = t["backing"]["table"]
                        .as_str()
                        .map(|tb| format!(" ← {}.{tb}", s_of(&t["backing"]["source"])))
                        .unwrap_or_default();
                    println!(
                        "{:<24} {:>7}  {}",
                        st.bold(s_of(&t["name"])),
                        t["count"],
                        st.dim(&format!("{} · {}{backing}", s_of(&t["label"]), s_of(&t["category"])))
                    );
                }
                return Ok(());
            };
            let filters = filters.iter().map(|f| parse_filter(f)).collect::<Result<Vec<_>>>()?;
            let r = be
                .call(
                    "objects.explore",
                    json!({ "kind": kind, "filters": filters, "search": search, "sort": sort, "limit": limit, "offset": offset, "workspace": cli.workspace }),
                )
                .await?;
            if *as_json {
                print_json(&r);
                return Ok(());
            }
            let objects = r["objects"].as_array().cloned().unwrap_or_default();
            println!(
                "{} {} {}",
                st.bold(&format!("{}", r["total"])),
                s_of(&r["label"]),
                st.dim(&format!("objects (showing {} from {offset})", objects.len()))
            );
            let columns: Vec<String> = r["columns"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["name"].as_str())
                .filter(|c| !matches!(*c, "title" | "created_at" | "updated_at"))
                .take(5)
                .map(str::to_string)
                .collect();
            for o in &objects {
                let is_ts = |name: &str| {
                    r["columns"].as_array().into_iter().flatten().any(|c| c["name"] == name && c["ty"] == "timestamp")
                };
                let props: Vec<String> = columns
                    .iter()
                    .filter_map(|c| {
                        o["props"].get(c).map(|v| match v.as_i64().filter(|_| is_ts(c)) {
                            Some(ms) => format!("{c}={}", onto_intelli::core::time::fmt_ts(ms)),
                            None => format!("{c}={}", cell(v)),
                        })
                    })
                    .collect();
                let line = if s_of(&o["title"]).is_empty() {
                    let state = o["state"].as_str().map(|x| format!(" ({x})")).unwrap_or_default();
                    format!("{}{state}", s_of(&o["id"]))
                } else {
                    entity_line(st, o)
                };
                println!("  {line}  {}", st.dim(&props.join(" ")));
            }
            for (name, values) in r["facets"].as_object().into_iter().flatten() {
                let vals: Vec<String> = values
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|v| format!("{} {}", cell(&v["value"]), v["count"]))
                    .collect();
                println!("{} {}", st.cyan(&format!("{name}:")), vals.join(" · "));
            }
            let timestamp = |name: &str| {
                r["columns"].as_array().into_iter().flatten().any(|c| c["name"] == name && c["ty"] == "timestamp")
            };
            for (name, s) in r["stats"].as_object().into_iter().flatten() {
                let (min, max) = (s["min"].as_f64().unwrap_or(0.0), s["max"].as_f64().unwrap_or(0.0));
                let (min, max) = if timestamp(name) {
                    let ts = |x: f64| onto_intelli::core::time::fmt_ts(x as i64);
                    (ts(min), ts(max))
                } else {
                    (format!("{min}"), format!("{max}"))
                };
                let mean = s["mean"].as_f64().map(|m| format!(" · mean {m:.2}")).unwrap_or_default();
                println!(
                    "{} {} · min {min} · max {max}{mean}",
                    st.cyan(&format!("{name}:")),
                    n_of(s["count"].as_u64().unwrap_or(0), "value")
                );
            }
            let actions: Vec<&str> = r["actions"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if !actions.is_empty() {
                println!("{}", st.dim(&format!("actions: {}", actions.join(", "))));
            }
            Ok(())
        }
        Command::Console { sub } => console(&cli, st, sub).await,
        Command::Reason { sub, json: as_json } => assure::reason(&cli, st, sub, *as_json).await,
        Command::Cert { sub } => assure::cert(sub),
        Command::Shapes { sub, json: as_json } => assure::shapes(&cli, st, sub, *as_json).await,
        Command::Lint(a) => assure::lint(&cli, st, a).await,
        Command::Lock(a) => assure::lock(&cli, st, a).await,
        Command::Unlock(a) => assure::unlock(&cli, st, a).await,
        Command::Locks { json: as_json } => assure::locks(&cli, st, *as_json).await,
        Command::Pack { sub, json: as_json } => assure::pack(&cli, st, sub, *as_json).await,
        Command::Align(a) => assure::align(&cli, st, a).await,
        Command::Induce(a) => assure::induce(&cli, st, a).await,
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

/// The control plane `onto serve` uses: the memory store with
/// `engine.storage = "memory"`, else the PostgreSQL schema `console.schema`.
async fn open_control(cfg: &Config) -> Result<Arc<onto_control::Control>> {
    let opts = onto_control::ControlOptions::from_env(&cfg.console)?;
    let control = match cfg.engine.storage.as_str() {
        "memory" => onto_control::Control::memory(cfg.console.clone(), opts)?,
        _ => onto_control::Control::postgres(&cfg.engine.resolve_database_url()?, cfg.console.clone(), opts).await?,
    };
    Ok(Arc::new(control))
}

/// `onto console …`: the operator opens the control store directly.
async fn console(cli: &Cli, st: &Style, sub: &ConsoleCmd) -> Result<()> {
    let cfg = local_config(cli)?;
    if cfg.engine.storage == "memory" {
        return Err(Error::Config(
            "with engine.storage = \"memory\" the control store lives inside `onto serve` only".into(),
        ));
    }
    let control = open_control(&cfg).await?;
    let store = control.store();
    match sub {
        ConsoleCmd::Bootstrap => {
            let hub = onto_intelli::Hub::open(cfg.clone()).await?;
            let tenants = hub.known_tenants()?;
            let signed = control.sign_in_local(&tenants).await?;
            println!(
                "Organization {} ({}) · owner {} ({})",
                st.bold(&signed.org.slug),
                signed.org.id,
                signed.user.name,
                signed.user.id
            );
            if signed.adopted.is_empty() {
                println!("{}", st.dim("No new tenants to adopt."));
            }
            for p in &signed.adopted {
                println!("  adopted tenant {} as project {}", st.cyan(&p.r#ref), p.id);
            }
            if !control.bootstrap_enabled() {
                println!(
                    "{}",
                    st.dim(&format!(
                        "Sign in to the console by setting {} (at least 32 characters) for `onto serve`.",
                        cfg.console.bootstrap_token_env.as_deref().unwrap_or("console.bootstrap_token_env")
                    ))
                );
            }
            Ok(())
        }
        ConsoleCmd::Users => {
            for u in store.users().await? {
                let orgs: Vec<String> =
                    store.orgs_of(&u.id).await?.into_iter().map(|(o, r)| format!("{} ({r})", o.slug)).collect();
                println!(
                    "{} {:<24} {:<28} {}{}",
                    st.cyan(&u.id),
                    u.name,
                    u.email.as_deref().unwrap_or("-"),
                    orgs.join(", "),
                    if u.disabled { st.red(" disabled") } else { String::new() }
                );
            }
            Ok(())
        }
        ConsoleCmd::AddOwner { user, org } => {
            let o = store.org(org).await?.ok_or_else(|| Error::not_found(format!("organization {org}")))?;
            store.user(user).await?.ok_or_else(|| Error::not_found(format!("user {user}")))?;
            store.add_member(&o.id, user, onto_control::OrgRole::Owner).await?;
            control
                .audit(onto_control::AuditEntry::new("system", "member.role").org(&o.id).target(user).detail(json!({
                    "to": "owner", "via": "onto console add-owner",
                })))
                .await?;
            println!("{user} is an owner of {}", st.bold(&o.slug));
            Ok(())
        }
        ConsoleCmd::Keys { project_ref, create, scopes, json: as_json } => {
            let p = store
                .project_by_ref(project_ref)
                .await?
                .filter(onto_control::Project::active)
                .ok_or_else(|| Error::not_found(format!("project {project_ref}")))?;
            if let Some(name) = create {
                let scopes = if scopes.is_empty() { vec!["read".to_string()] } else { scopes.clone() };
                let req = onto_control::KeyRequest {
                    name: name.clone(),
                    scopes: scopes.iter().map(|s| onto_control::KeyScope::parse(s)).collect::<Result<_>>()?,
                    workspaces: cli.workspace.clone().map(|w| vec![w]),
                    ..Default::default()
                };
                let (key, secret) = control.create_key(&p, req, "system").await?;
                if *as_json {
                    print_json(&json!({ "key": key, "secret": secret }));
                } else {
                    println!("Created {} ({}) in project {}", st.bold(&key.prefix), key.name, p.r#ref);
                    println!("{secret}");
                    println!("{}", st.dim("The secret is shown once; store it now."));
                }
                return Ok(());
            }
            let keys = store.keys(&p.id).await?;
            if *as_json {
                print_json(&json!({ "keys": keys }));
                return Ok(());
            }
            for k in keys {
                let scopes: Vec<&str> = k.scopes.iter().map(|s| s.as_str()).collect();
                let state = if k.revoked_at.is_some() {
                    st.red("revoked")
                } else if !k.usable(onto_intelli::core::time::now_ms()) {
                    st.red("expired")
                } else {
                    "active".to_string()
                };
                println!(
                    "{} {:<24} {:<22} {:<8} {}",
                    st.cyan(&k.prefix),
                    k.name,
                    scopes.join(","),
                    state,
                    st.dim(&k.id)
                );
            }
            Ok(())
        }
        ConsoleCmd::RotateMasterKey { json: as_json } => {
            let report = control.rotate_master_key().await?;
            if *as_json {
                print_json(&json!(report));
            } else {
                println!(
                    "Rewrapped {} LLM keys under the current ONTO_MASTER_KEY; {} already were.",
                    st.bold(&report.rewrapped.to_string()),
                    report.current
                );
                for (project, why) in &report.failed {
                    println!("  {} {project}: {why}", st.red("failed"));
                }
                if report.failed.is_empty() && report.rewrapped > 0 {
                    println!(
                        "{}",
                        st.dim("ONTO_MASTER_KEY_PREVIOUS can be removed once every replica runs on the new key.")
                    );
                }
            }
            if report.failed.is_empty() {
                Ok(())
            } else {
                Err(Error::Invalid(format!("{} keys did not decrypt", report.failed.len())))
            }
        }
    }
}

async fn model(cli: &Cli, st: &Style, sub: &ModelCmd) -> Result<()> {
    let be = backend(cli).await?;
    match sub {
        ModelCmd::Draft { source, opts, json: as_json } => {
            let d = be.call("ontology.draft", opts.input(source)).await?;
            if *as_json {
                print_json(&d);
            } else {
                print_draft(st, &d);
                if d["objects"].as_array().is_some_and(|a| !a.is_empty()) {
                    println!(
                        "\n{}",
                        st.dim(&format!(
                            "Next: onto model apply {0}   (or --propose to file it for review)",
                            s_of(&d["source"])
                        ))
                    );
                }
            }
        }
        ModelCmd::Refine { source, opts, instructions, json: as_json } => {
            let mut input = opts.input(source);
            input["instructions"] = json!(instructions);
            let r = be.call("ontology.refine", input).await?;
            if *as_json {
                print_json(&r);
                return Ok(());
            }
            print_draft(st, &r["draft"]);
            println!(
                "\n{} {} of {} suggestions applied · {} · {} tokens",
                st.bold("Refined:"),
                r["applied"],
                r["suggestions"].as_array().map(Vec::len).unwrap_or(0),
                s_of(&r["llm"]),
                r["tokens"]
            );
            for x in r["suggestions"].as_array().into_iter().flatten() {
                println!(
                    "  {} {} {} → {}  {}",
                    st.cyan(s_of(&x["target"])),
                    s_of(&x["field"]),
                    cell(&x["from"]),
                    cell(&x["to"]),
                    st.dim(s_of(&x["why"]))
                );
            }
            for x in r["rejected"].as_array().into_iter().flatten() {
                println!(
                    "  {} {} {}: {}",
                    st.red("rejected"),
                    s_of(&x["suggestion"]["target"]),
                    s_of(&x["suggestion"]["field"]),
                    s_of(&x["error"])
                );
            }
        }
        ModelCmd::Advise { question, source, json: as_json } => {
            let q = question.join(" ");
            if q.trim().is_empty() {
                return Err(Error::invalid("usage: onto model advise \"<question>\" [--source s]"));
            }
            let r = be.call("ontology.advise", json!({ "question": q, "source": source })).await?;
            if *as_json {
                print_json(&r);
                return Ok(());
            }
            println!("{}", s_of(&r["answer"]));
            for f in r["facts"].as_array().into_iter().flatten() {
                println!("  {}", st.dim(&format!("· {}", s_of(f))));
            }
            println!(
                "{}",
                st.dim(&format!("{} · source {} · {} tokens", s_of(&r["mode"]), s_of(&r["source"]), r["tokens"]))
            );
            if let Some(n) = r["note"].as_str() {
                println!("{}", st.red(&format!("note: {n}")));
            }
        }
        ModelCmd::Apply { source, opts, propose, title, rationale, no_crawl, json: as_json } => {
            let mut input = opts.input(source);
            input["mode"] = json!(if *propose { "propose" } else { "apply" });
            input["title"] = json!(title);
            input["rationale"] = json!(rationale);
            input["no_crawl"] = json!(no_crawl);
            input["workspace"] = json!(cli.workspace);
            let r = be.call("ontology.apply_draft", input).await?;
            if *as_json {
                print_json(&r);
                return Ok(());
            }
            let c = &r["counts"];
            let n = |k: &str, what: &str| n_of(c[k].as_u64().unwrap_or(0), what);
            let what = format!(
                "{}, {}, {}",
                n("objects", "object type"),
                n("links", "link type"),
                n("actions", "action type")
            );
            if r["mode"] == "proposed" {
                println!(
                    "Proposed {} ({what}) as {}; an approver applies it: onto schema approve {}",
                    s_of(&r["source"]),
                    st.bold(s_of(&r["proposal"]["id"])),
                    s_of(&r["proposal"]["id"])
                );
            } else {
                println!(
                    "Applied {} as schema {} ({what})",
                    st.bold(s_of(&r["source"])),
                    st.bold(&format!("v{}", r["version"]["version"]))
                );
                for c in r["crawl"].as_array().into_iter().flatten() {
                    print_crawl(st, c);
                }
                if let Some(h) = r["hint"].as_str() {
                    println!("{}", st.dim(h));
                }
            }
        }
    }
    Ok(())
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
    println!("Created {} and {}", st.bold(&cfg_path.display().to_string()), proto.display());
    println!(
        "\nNext:\n  make db-up   # or any PostgreSQL 16+ with pgvector\n  export ONTO_DATABASE_URL={}\n  onto seed demo\n  onto ask \"Who owns Project Alpha?\"\n  export ONTO_TOKEN=$(openssl rand -hex 24) && onto serve\n  (or `onto serve --dev` locally: no auth, loopback only)",
        onto_intelli::config::DEV_DATABASE_URL
    );
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

#[cfg(test)]
mod tests {
    #[test]
    fn command_line_definition_is_consistent() {
        use clap::CommandFactory;
        super::Cli::command().debug_assert();
    }

    #[test]
    fn counts_take_english_plurals() {
        assert_eq!(super::n_of(4, "entity"), "4 entities");
        assert_eq!(super::n_of(1, "entity"), "1 entity");
        assert_eq!(super::n_of(2, "key"), "2 keys");
        assert_eq!(super::n_of(3, "property shape"), "3 property shapes");
    }
}
