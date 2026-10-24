//! Assurance commands: reasoning and certificates, shapes, schema lint and
//! locks, industry packs, schema alignment and sheet induction.
//!
//! Every command calls a catalogue action through the backend (the local
//! store, or the server with `--remote`) and prints the report it returns,
//! except `onto cert`, which runs the `onto-cert` checker library on files
//! and never opens a store. Checker words (`entailed`,
//! `entailed_under_supplied_rules`) are printed only from a report that the
//! onto-cert library produced, in this process or in the engine; the
//! engine's own reports say `derived`.
//!
//! Commands whose report is a verdict exit with a code that scripts can
//! test: `cert check` 0/1/2 like `onto-cert`, `reason certify` non-zero when
//! the check fails, `shapes validate` 0/1/3 and `lint` 1 on an error finding.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde_json::{Value, json};

use onto_intelli::{Error, Result};

use super::{Cli, Style, backend, n_of, print_json, print_plan, read_input, s_of};

// ── command line ──────────────────────────────────────────────────────

#[derive(Subcommand)]
pub(crate) enum ReasonCmd {
    /// Compute the closure: asserted and derived facts, facts per rule, sample
    /// derivations, and the schema constructs the reasoner did not evaluate
    /// (a clean run is not a consistency result).
    Run {
        /// Stop after this many derived facts (the closure is then a lower bound).
        #[arg(long)]
        max_derived: Option<usize>,
        /// Sample derivations to show.
        #[arg(long, default_value_t = 20)]
        samples: usize,
    },
    /// Match a pattern over the closure, e.g. '[["?i","handled_by_team","?t"],["?t","a","team"]]'
    /// (a single atom may leave out the outer list; inline, @file or - for stdin).
    Query {
        pattern: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Why a fact holds: the minimal sets of asserted facts it is derived from,
    /// with the rules; with --semiring, its provenance (why, trust or count).
    Explain {
        /// Entity id, or a schema name for schema facts.
        subject: String,
        /// Link type, derived relation, `a`, `state`, `prop.<name>`, `extends`...
        relation: String,
        /// Entity id, name, or a JSON value for `state` and `prop.*`.
        object: String,
        /// why | trust | count: evaluate the fact's support in this semiring.
        #[arg(long)]
        semiring: Option<String>,
        /// At most this many justifications.
        #[arg(long)]
        max: Option<usize>,
    },
    /// Check claims (JSON: a list of {subject, relation, object, rule?, premises?};
    /// inline, @file or - for stdin): asserted, derived, contradicted,
    /// not_entailed or outside_vocabulary.
    Verify { claims: String },
    /// Does a slice of entities keep what the full graph entails?
    Slice {
        /// Entity ids in the slice.
        #[arg(required = true)]
        entities: Vec<String>,
        /// Claims the slice must keep (JSON list, inline or @file).
        #[arg(long)]
        goals: Option<String>,
    },
    /// Certify the closure: with --out, write rules.tsv, asserted.tsv,
    /// derivations.tsv and manifest.json into DIR and check them again here
    /// with the onto-cert library. Exit 0 accepted, 1 rejected, 2 malformed,
    /// 4 when the command fails (unknown action, denied, unreachable server).
    Certify {
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
    },
    /// Is a certificate's asserted.tsv (in DIR) what the store yields now?
    Binding { dir: PathBuf },
}

#[derive(Subcommand)]
pub(crate) enum CertCmd {
    /// Check the certificate in DIR offline (rules.tsv, asserted.tsv,
    /// derivations.tsv, optional manifest.json) and print the onto-cert JSON
    /// report. Exit 0 accepted, 1 rejected, 2 malformed or unreadable. No
    /// store or server is opened.
    Check {
        dir: PathBuf,
        /// The report is always JSON; accepted for symmetry with other commands.
        #[arg(long)]
        json: bool,
    },
    /// Print the built-in rule table compiled into the checker.
    Table {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ShapesCmd {
    /// Validate the graph against the schema's shapes (a measurement).
    ///
    /// Exit codes: 0 conforms, 1 violates, 3 undetermined (some constraint,
    /// target or shape was not evaluated, so the report cannot say "valid"),
    /// 4 the command failed (no verdict).
    Validate {
        /// Read the reasoner's closure: derived classes select targets and
        /// satisfy `class`, derived relations are followed by paths.
        #[arg(long)]
        inferred: bool,
        /// Evaluate only these shapes (repeatable; default: all).
        #[arg(long = "shape")]
        shapes: Vec<String>,
        /// Results listed (all are counted).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Induce a shape from the objects of a kind; prints it as a schema layer
    /// (TOML), with the counts behind each constraint.
    Induce { kind: String },
    /// Check shape definitions (TOML `[shapes.<name>]` tables, or JSON) against
    /// the current schema, without validating any data. Exit 0 ok, 1 issues
    /// found, 4 the command failed.
    Check { file: PathBuf },
}

#[derive(Subcommand)]
pub(crate) enum PackCmd {
    /// Industry packs: version, counts, standards, installed or not.
    List,
    /// Everything a pack defines: types, links, rules, shapes, metrics, actions.
    Show { id: String },
    /// The schema plan of installing a pack: risk, consequences, locks, renames.
    Plan { id: String },
    /// Install a pack (apply roles), or file it as a proposal with --propose.
    Install {
        id: String,
        /// File a schema proposal for review instead of applying.
        #[arg(long)]
        propose: bool,
        /// Also load the pack's sample data into the workspace.
        #[arg(long)]
        sample: bool,
        /// Why (the change set message and proposal rationale).
        #[arg(long)]
        message: Option<String>,
    },
}

#[derive(Args)]
pub(crate) struct LintArgs {
    /// Run only these checks (repeatable; default: all).
    #[arg(long = "check")]
    checks: Vec<String>,
    /// Also show dismissed findings.
    #[arg(long)]
    all: bool,
    /// Dismiss a finding by id (reports hide it and count it as suppressed).
    #[arg(long, value_name = "ID", conflicts_with = "accept")]
    dismiss: Option<String>,
    /// Accept a finding by id (it stays in reports, marked accepted).
    #[arg(long, value_name = "ID")]
    accept: Option<String>,
    /// Note recorded with --dismiss or --accept.
    #[arg(long)]
    note: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub(crate) struct LockArgs {
    /// object_types, link_types, interfaces, rules, metrics, action_types, shapes or properties.
    section: String,
    /// The definition (`issue`, `depends_on`, `issue.priority` for a property).
    name: String,
    /// Why it is locked (shown to whoever plans a change that touches it).
    #[arg(long)]
    reason: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub(crate) struct UnlockArgs {
    section: String,
    name: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub(crate) struct AlignArgs {
    /// A schema TOML file to align with the current schema.
    #[arg(required_unless_present = "pack", conflicts_with = "pack")]
    file: Option<PathBuf>,
    /// A pack (or built-in package) to align with the current schema.
    #[arg(long)]
    pack: Option<String>,
    /// Minimum score of a candidate.
    #[arg(long)]
    threshold: Option<f64>,
    /// Write the SSSOM TSV of the report to this file.
    #[arg(long, value_name = "OUT.tsv")]
    sssom: Option<PathBuf>,
    /// Certify these mappings (SSSOM TSV, or JSON [{source, target, predicate}])
    /// against both hierarchies instead of aligning.
    #[arg(long, value_name = "MAPPINGS")]
    certify: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub(crate) struct InduceArgs {
    /// A CSV file (read here; its text is sent).
    file: PathBuf,
    /// The object type name (default: from the file name or the key column).
    #[arg(long)]
    kind: Option<String>,
    /// The key column (default: the first filled, unique, key-like column).
    #[arg(long)]
    key: Option<String>,
    /// Write the rows into the workspace (the object type must exist: apply
    /// the induced change set first).
    #[arg(long)]
    ingest: bool,
    /// With --ingest: validate the rows without writing.
    #[arg(long, requires = "ingest")]
    dry_run: bool,
    /// With --ingest: a mapping file (JSON: {kind, key, title?, state?,
    /// properties: {column: property}, links: {column: {rel, target_kind}}})
    /// saying where each column goes, instead of matching columns by name.
    #[arg(long, value_name = "FILE", requires = "ingest")]
    mapping: Option<PathBuf>,
    /// Write the induced change set (JSON) to this file, for `onto schema plan|apply`.
    #[arg(long, value_name = "FILE", conflicts_with = "ingest")]
    change_set: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

// ── shared helpers ────────────────────────────────────────────────────

/// An action input without its null members: the engine reads a missing
/// key as its default, while a null can fail a non-optional field.
fn compact(mut v: Value) -> Value {
    if let Value::Object(m) = &mut v {
        m.retain(|_, x| !x.is_null());
    }
    v
}

/// Exit code of a verdict command (shapes, lint, reason certify, cert) whose
/// run failed before it reached a verdict: distinct from every verdict code
/// (0-3) and from clap's usage error (2).
pub(crate) const EXIT_FAILED: i32 = 4;

/// Exit with `code` when it is not 0 (after flushing what was printed).
fn finish(code: i32) -> Result<()> {
    if code != 0 {
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }
    Ok(())
}

/// A fact as `s p o`, from `{s, p, o}`, `[s, p, o]` or a string.
fn fact(v: &Value) -> String {
    match v {
        Value::Object(m) if m.contains_key("s") => format!("{} {} {}", s_of(&v["s"]), s_of(&v["p"]), s_of(&v["o"])),
        Value::Object(m) if m.contains_key("subject") => {
            format!("{} {} {}", s_of(&v["subject"]), s_of(&v["relation"]), cell(&v["object"]))
        }
        Value::Array(a) => a.iter().map(cell).collect::<Vec<_>>().join(" "),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn cell(v: &Value) -> String {
    super::cell(v)
}

fn len_of(v: &Value) -> usize {
    match v {
        Value::Array(a) => a.len(),
        Value::Number(n) => n.as_u64().unwrap_or(0) as usize,
        _ => 0,
    }
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().map(cell).collect()
}

/// A derivation line: `fact  by rule from p1, p2`.
fn derivation_line(st: &Style, d: &Value) -> String {
    let conclusion = if d["fact"].is_null() { &d["conclusion"] } else { &d["fact"] };
    let premises: Vec<String> = d["premises"].as_array().into_iter().flatten().map(fact).collect();
    let mut s = fact(conclusion);
    if let Some(rule) = d["rule"].as_str() {
        s.push_str(&st.dim(&format!("  by {rule}")));
    }
    if !premises.is_empty() {
        s.push_str(&st.dim(&format!(" from {}", premises.join(", "))));
    }
    s
}

fn print_means(st: &Style, r: &Value) {
    if let Some(m) = r["means"].as_str() {
        println!("{}", st.dim(&format!("means: {m}")));
    }
}

/// Text of a JSON argument: inline, `@file` or `-` (stdin).
fn json_arg(arg: &str) -> Result<Value> {
    read_input(Some(arg.to_string()))
}

/// A claim object (or a value parsed as one) as a list of claims.
fn claim_list(v: Value) -> Value {
    match v {
        Value::Array(_) => v,
        Value::Object(ref m) if m.contains_key("claims") => v["claims"].clone(),
        Value::Object(_) => json!([v]),
        other => other,
    }
}

/// A command-line object term: a JSON value when it reads as one (numbers,
/// booleans, quoted strings), else the text itself (ids and names).
fn object_term(s: &str) -> Value {
    match serde_json::from_str::<Value>(s) {
        Ok(v @ (Value::Number(_) | Value::Bool(_) | Value::String(_) | Value::Null)) => v,
        _ => json!(s),
    }
}

/// A pattern argument as a list of atoms (a single atom is wrapped).
fn pattern_atoms(v: Value) -> Result<Value> {
    let atoms = match &v {
        Value::Array(a) if a.first().is_some_and(Value::is_array) => v,
        Value::Array(a) if a.len() == 3 => json!([v]),
        Value::Object(m) if m.contains_key("pattern") => v["pattern"].clone(),
        _ => {
            return Err(Error::invalid("pattern: give a JSON list of [s, p, o] atoms, e.g. [[\"?x\",\"a\",\"team\"]]"));
        }
    };
    Ok(atoms)
}

// ── reason ────────────────────────────────────────────────────────────

pub(crate) async fn reason(cli: &Cli, st: &Style, sub: &ReasonCmd, as_json: bool) -> Result<()> {
    let ws = &cli.workspace;
    match sub {
        ReasonCmd::Run { max_derived, samples } => {
            let be = backend(cli).await?;
            let r = be
                .call("reason.run", compact(json!({ "workspace": ws, "max_derived": max_derived, "samples": samples })))
                .await?;
            if as_json {
                print_json(&r);
            } else {
                print_reason_run(st, &r);
            }
            Ok(())
        }
        ReasonCmd::Query { pattern, limit } => {
            let atoms = pattern_atoms(json_arg(pattern)?)?;
            let be = backend(cli).await?;
            let r =
                be.call("reason.query", compact(json!({ "pattern": atoms, "workspace": ws, "limit": limit }))).await?;
            if as_json {
                print_json(&r);
            } else {
                print_bindings(st, &r);
            }
            Ok(())
        }
        ReasonCmd::Explain { subject, relation, object, semiring, max } => {
            let claim = json!({ "subject": subject, "relation": relation, "object": object_term(object) });
            let be = backend(cli).await?;
            let r = match semiring {
                Some(sr) => {
                    be.call("reason.provenance", compact(json!({ "fact": claim, "semiring": sr, "workspace": ws })))
                        .await?
                }
                None => {
                    be.call("reason.explain", compact(json!({ "fact": claim, "workspace": ws, "max": max }))).await?
                }
            };
            if as_json {
                print_json(&r);
            } else if semiring.is_some() {
                print_provenance(st, &r);
            } else {
                print_justifications(st, &r);
            }
            Ok(())
        }
        ReasonCmd::Verify { claims } => {
            let claims = claim_list(json_arg(claims)?);
            let be = backend(cli).await?;
            let r = be.call("reason.verify", compact(json!({ "claims": claims, "workspace": ws }))).await?;
            if as_json {
                print_json(&r);
            } else {
                print_verdicts(st, &r);
            }
            Ok(())
        }
        ReasonCmd::Slice { entities, goals } => {
            let mut input = json!({ "entities": entities, "workspace": ws });
            if let Some(g) = goals {
                input["goals"] = claim_list(json_arg(g)?);
            }
            let be = backend(cli).await?;
            let r = be.call("reason.slice_check", compact(input)).await?;
            if as_json {
                print_json(&r);
            } else {
                print_slice(st, &r);
            }
            Ok(())
        }
        ReasonCmd::Certify { out } => {
            let be = backend(cli).await?;
            let r = be.call("reason.certify", compact(json!({ "workspace": ws }))).await?;
            certify(st, &r, out.as_deref(), as_json)
        }
        ReasonCmd::Binding { dir } => {
            let read = |f: &str| std::fs::read_to_string(dir.join(f));
            let asserted = read("asserted.tsv")
                .map_err(|e| Error::invalid(format!("{}: {e}", dir.join("asserted.tsv").display())))?;
            let manifest = read("manifest.json").ok();
            let be = backend(cli).await?;
            let r = be
                .call(
                    "reason.binding",
                    compact(json!({ "asserted_tsv": asserted, "manifest_json": manifest, "workspace": ws })),
                )
                .await?;
            if as_json {
                print_json(&r);
            } else {
                print_binding(st, &r);
            }
            Ok(())
        }
    }
}

fn print_reason_run(st: &Style, r: &Value) {
    let s = if r["stats"].is_object() { &r["stats"] } else { r };
    let fixpoint = s["fixpoint_reached"] == true;
    println!(
        "{} derived from {} asserted {}",
        st.bold(&n_of(s["derived"].as_u64().unwrap_or(0), "fact")),
        s["asserted"],
        st.dim(&format!(
            "({} · {} · {})",
            n_of(s["rules"].as_u64().unwrap_or(0), "rule"),
            n_of(s["rounds"].as_u64().unwrap_or(0), "round"),
            if fixpoint { "fixpoint reached".to_string() } else { "fixpoint NOT reached: a lower bound".to_string() }
        ))
    );
    if let Some(t) = s["truncated"].as_str() {
        println!("  {} {t}", st.red("truncated:"));
    }
    if let Some(by) = s["by_rule"].as_object() {
        for (rule, n) in by.iter().filter(|(_, n)| n.as_u64().unwrap_or(0) > 0) {
            println!("  {:<40} {n}", rule);
        }
    }
    if s["refused"].as_u64().unwrap_or(0) > 0 {
        println!("  {} {} terms or rules could not be spelled safely", st.red("refused:"), s["refused"]);
        for f in s["refusals"].as_array().into_iter().flatten() {
            println!("    {} {}", s_of(&f["term"]), st.dim(s_of(&f["reason"])));
        }
    }
    let not_eval = strs(&s["not_evaluated"]);
    if !not_eval.is_empty() {
        println!(
            "{} {}\n  {}",
            st.cyan("not evaluated:"),
            not_eval.join(", "),
            st.dim("a clean run is not a consistency result")
        );
    }
    let samples = r["samples"].as_array().cloned().unwrap_or_default();
    if !samples.is_empty() {
        println!("{}", st.bold("Sample derivations (derived, not asserted)"));
        for d in &samples {
            println!("  {}", derivation_line(st, d));
        }
    }
    print_means(st, r);
}

fn print_bindings(st: &Style, r: &Value) {
    let rows = r["bindings"].as_array().cloned().unwrap_or_default();
    let mut vars: Vec<String> = Vec::new();
    for b in &rows {
        for k in b.as_object().into_iter().flat_map(|m| m.keys()) {
            if !vars.contains(k) {
                vars.push(k.clone());
            }
        }
    }
    if !vars.is_empty() {
        println!("{}", st.bold(&vars.iter().map(|v| format!("?{v:<30}")).collect::<String>()));
    }
    for b in &rows {
        println!("{}", vars.iter().map(|v| format!(" {:<30}", cell(&b[v]))).collect::<String>());
    }
    let mut foot = n_of(rows.len() as u64, "binding");
    if let Some(t) = r["truncated"].as_str() {
        foot.push_str(&format!(" · truncated: {t}"));
    }
    if r["fixpoint_reached"] == false {
        foot.push_str(" · closure is a lower bound: more bindings may exist");
    }
    println!("{}", st.dim(&foot));
}

fn print_justifications(st: &Style, r: &Value) {
    println!("{} {}", fact(&r["fact"]), st.cyan(s_of(&r["status"])));
    let justs = r["justifications"].as_array().cloned().unwrap_or_default();
    for (i, j) in justs.iter().enumerate() {
        let mut rules = strs(&j["rules"]);
        rules.extend(strs(&j["builtin_rules"]));
        let checked = match (j["sufficient"] == true, j["minimal"] == true) {
            (true, true) => "sufficient, minimal",
            (true, false) => "sufficient, minimality not shown",
            _ => "not verified",
        };
        println!(
            "  {} {}",
            st.bold(&format!("#{}", i + 1)),
            st.dim(&format!(
                "{} · rules {} · {checked}",
                n_of(len_of(&j["facts"]) as u64, "asserted fact"),
                if rules.is_empty() { "none".into() } else { rules.join(", ") }
            ))
        );
        for f in j["facts"].as_array().into_iter().flatten() {
            println!("      {}", fact(f));
        }
        for d in j["proof"].as_array().into_iter().flatten() {
            println!("      {} {}", st.dim("⊢"), derivation_line(st, d));
        }
    }
    if justs.is_empty() {
        println!("  no justification: the rule table does not derive it from the visible graph");
    }
    let mut foot = Vec::new();
    if r["complete"] == false {
        foot.push("not every justification is listed".to_string());
    }
    if r["fixpoint_reached"] == false {
        foot.push("closure is a lower bound".into());
    }
    if let Some(s) = r["stopped"].as_str() {
        foot.push(format!("stopped: {s}"));
    }
    if let Some(s) = r["verification"].as_str() {
        foot.push(s.to_string());
    }
    if !foot.is_empty() {
        println!("{}", st.dim(&foot.join(" · ")));
    }
    print_means(st, r);
}

fn print_provenance(st: &Style, r: &Value) {
    println!(
        "{} {} {}",
        fact(&r["fact"]),
        st.cyan(s_of(&r["status"])),
        st.dim(&format!("semiring {}", s_of(&r["semiring"])))
    );
    match &r["value"] {
        Value::Array(sets) => {
            for (i, set) in sets.iter().enumerate() {
                let facts: Vec<String> = set.as_array().into_iter().flatten().map(fact).collect();
                println!("  {} {}", st.bold(&format!("#{}", i + 1)), facts.join(", "));
            }
        }
        v => println!("  value {}", cell(v)),
    }
    let mut foot = vec![if r["exact"] == true { "exact".to_string() } else { "a bound, not exact".to_string() }];
    if r["cycle_in_support"] == true {
        foot.push("cycle in support".into());
    }
    if r["fixpoint_reached"] == false {
        foot.push("closure is a lower bound".into());
    }
    if let Some(s) = r["stopped"].as_str() {
        foot.push(format!("stopped: {s}"));
    }
    println!("{}", st.dim(&foot.join(" · ")));
    print_means(st, r);
}

fn print_verdicts(st: &Style, r: &Value) {
    for v in r["verdicts"].as_array().into_iter().flatten() {
        let word = s_of(&v["verdict"]);
        let mark = match word {
            "asserted" | "derived" => st.bold(word),
            "contradicted" => st.red(word),
            _ => st.cyan(word),
        };
        println!("{mark:<20} {}  {}", fact(&v["claim"]), st.dim(s_of(&v["reason"])));
        if let Some(step) = v["step"].as_object() {
            println!(
                "    step {} {}",
                s_of(step.get("label").unwrap_or(&Value::Null)),
                st.dim(s_of(step.get("reason").unwrap_or(&Value::Null)))
            );
        }
        for d in v["derivation"].as_array().into_iter().flatten() {
            println!("    {} {}", st.dim("⊢"), derivation_line(st, d));
        }
        for c in v["conflicting"].as_array().into_iter().flatten() {
            println!("    {} {}", st.red("conflicts with"), fact(c));
        }
    }
    if let Some(c) = r["counts"].as_object() {
        let parts: Vec<String> = c.iter().map(|(k, n)| format!("{n} {k}")).collect();
        println!("{}", st.dim(&parts.join(" · ")));
    }
    if r["fixpoint_reached"] == false {
        println!("{}", st.dim("closure is a lower bound: not_entailed may mean not derived yet"));
    }
    print_means(st, r);
}

fn print_slice(st: &Style, r: &Value) {
    println!(
        "slice of {}: {} preserved, {} lost {}",
        n_of(r["slice"].as_u64().unwrap_or(0), "entity"),
        r["preserved"],
        r["lost"],
        st.dim(&format!(
            "(full graph {} asserted, {} derived · slice {} asserted, {} derived)",
            r["full_asserted"], r["full_derived"], r["slice_asserted"], r["slice_derived"]
        ))
    );
    for id in strs(&r["unknown_ids"]) {
        println!("  {} {id}", st.red("unknown id"));
    }
    for g in r["goals"].as_array().into_iter().flatten() {
        println!("  {:<18} {}", s_of(&g["status"]), fact(&g["claim"]));
        for m in g["missing_premises"].as_array().into_iter().flatten() {
            println!("      {} {}", st.dim("missing"), fact(m));
        }
    }
    for l in r["lost_in_slice_vocabulary"].as_array().into_iter().flatten() {
        println!("  {} {} {}", st.red("lost"), fact(&l["fact"]), st.dim(&format!("by {}", s_of(&l["rule"]))));
    }
    if r["truncated"] == true {
        let listed = r["lost_in_slice_vocabulary"].as_array().map_or(0, Vec::len);
        println!("  {}", st.dim(&format!("{listed} of {} lost facts listed", r["lost"])));
    }
    let alarm = r["slice_only"].as_array().cloned().unwrap_or_default();
    if !alarm.is_empty() {
        println!("{}", st.red("slice entails what the full graph does not (soundness alarm):"));
        for f in &alarm {
            println!("  {} {}", st.red("slice only"), fact(f));
        }
    }
    if r["fixpoint_reached"] == false {
        println!("{}", st.dim("closure is a lower bound: preserved and lost counts may change"));
    }
    print_means(st, r);
}

fn print_binding(st: &Style, r: &Value) {
    let verdict = s_of(&r["verdict"]);
    let word = if verdict == "same_graph" { st.bold(verdict) } else { st.red(verdict) };
    println!(
        "{word} {}",
        st.dim(&format!(
            "certificate {} facts · store {} · shared {} · only in certificate {} · only in store {}",
            r["certificate_facts"], r["store_facts"], r["shared"], r["only_in_certificate"], r["only_in_store"]
        ))
    );
    if let Some(m) = r["manifest_digest_matches"].as_bool() {
        println!(
            "  manifest asserted_sha256 {}",
            if m { "matches the file".to_string() } else { st.red("does NOT match the file") }
        );
    }
    for f in r["sample_only_in_certificate"].as_array().into_iter().flatten() {
        println!("  {} {}", st.dim("only in certificate"), fact(f));
    }
    for f in r["sample_only_in_store"].as_array().into_iter().flatten() {
        println!("  {} {}", st.dim("only in store"), fact(f));
    }
    print_means(st, r);
}

/// The certificate files of a `reason.certify` result, as onto-cert reads them.
fn cert_files(c: &Value) -> Result<onto_cert::CertFiles> {
    let text = |k: &str| {
        c[k].as_str().map(str::to_string).ok_or_else(|| Error::Other(format!("reason.certify returned no {k}")))
    };
    Ok(onto_cert::CertFiles {
        rules: text("rules_tsv")?,
        asserted: text("asserted_tsv")?,
        derivations: text("derivations_tsv")?,
        manifest: c["manifest_json"].as_str().map(str::to_string),
    })
}

/// Write the certificate (with `out`), check it again with the onto-cert
/// library in this process, print the checker's verdict and exit 1 when
/// either check rejects it (2 when the local check finds it malformed).
fn certify(st: &Style, r: &Value, out: Option<&Path>, as_json: bool) -> Result<()> {
    let files = cert_files(&r["certificate"])?;
    if let Some(dir) = out {
        std::fs::create_dir_all(dir).map_err(|e| Error::Storage(format!("{}: {e}", dir.display())))?;
        for (name, text) in [
            (onto_cert::RULES_FILE, &files.rules),
            (onto_cert::ASSERTED_FILE, &files.asserted),
            (onto_cert::DERIVATIONS_FILE, &files.derivations),
        ] {
            write_file(&dir.join(name), text)?;
        }
        if let Some(m) = &files.manifest {
            write_file(&dir.join(onto_cert::MANIFEST_FILE), m)?;
        }
    }
    // Checked here from the bytes written, independently of the engine's own check.
    let local = match out {
        Some(dir) => cert_report(dir),
        None => {
            let rep = onto_cert::check(&files);
            (rep.to_json(), rep.exit_code())
        }
    };
    let engine = &r["check"];
    let agree = engine["ok"] == local.0["ok"] && engine["verdict"] == local.0["verdict"];
    // The checker does not read attested fields: whether the closure reached
    // its fixpoint is the engine's claim, shown next to the verdict.
    let attested = files
        .manifest
        .as_deref()
        .and_then(|m| serde_json::from_str::<Value>(m).ok())
        .map(|m| m["attested"].clone())
        .unwrap_or_default();
    let partial = r["fixpoint_reached"] == false || attested["fixpoint_reached"] == false;
    if as_json {
        let mut v = json!({
            "check": local.0,
            "engine_check": engine,
            "checks_agree": agree,
            "fixpoint_reached": !partial,
        });
        if let Some(t) = r["truncated"].as_str() {
            v["truncated"] = json!(t);
        }
        match out {
            Some(dir) => v["out"] = json!(dir.display().to_string()),
            None => v["certificate"] = r["certificate"].clone(),
        }
        print_json(&v);
    } else {
        let rep = &local.0;
        if rep["ok"] == true {
            println!(
                "{} {}",
                st.bold(s_of(&rep["verdict"])),
                st.dim(&format!(
                    "{} asserted · {} derivations · rules cited: {} built-in, {} supplied · {}",
                    rep["asserted"],
                    rep["derivations"],
                    rep["rules_cited"]["builtin"],
                    rep["rules_cited"]["supplied"],
                    s_of(&rep["checker"])
                ))
            );
        } else {
            println!(
                "{} {}",
                st.red("rejected"),
                st.dim(&format!(
                    "{} at line {} ({}){}",
                    s_of(if rep["reason"].is_null() { &rep["error"] } else { &rep["reason"] }),
                    rep["first_rejected"].as_u64().or(rep["line"].as_u64()).unwrap_or(0),
                    s_of(if rep["rule"].is_null() { &rep["file"] } else { &rep["rule"] }),
                    rep["conclusion"].as_str().map(|c| format!(": {c}")).unwrap_or_default()
                ))
            );
        }
        if let Some(m) = rep["means"].as_str() {
            println!("{}", st.dim(&format!("means: {m}")));
        }
        if partial {
            let why = r["truncated"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("after {} rounds", attested["rounds"]));
            println!(
                "{} closure is a lower bound (stopped: {why}): the derivations check, but more may follow",
                st.red("note:")
            );
        }
        if let Some(dir) = out {
            println!("wrote {} (check again with `onto cert check {0}` or `onto-cert check {0}`)", dir.display());
        }
        if !agree {
            println!("{} the engine's check said {}", st.red("warning:"), engine);
        }
    }
    let engine_rejected = engine.is_object() && engine["ok"] != true;
    finish(if local.1 != 0 {
        local.1
    } else if engine_rejected {
        1
    } else {
        0
    })
}

fn write_file(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|e| Error::Storage(format!("{}: {e}", path.display())))
}

// ── cert (offline) ────────────────────────────────────────────────────

/// The onto-cert report on the certificate in `dir` and its exit code,
/// exactly as `onto-cert check <dir>` prints and returns them.
pub(crate) fn cert_report(dir: &Path) -> (Value, i32) {
    match onto_cert::CertFiles::from_dir(dir) {
        Ok(files) => {
            let r = onto_cert::check(&files);
            (r.to_json(), r.exit_code())
        }
        Err(e) => (json!({"ok": false, "error": "unreadable", "file": e.file, "reason": e.error.to_string()}), 2),
    }
}

pub(crate) fn cert(sub: &CertCmd) -> Result<()> {
    match sub {
        CertCmd::Check { dir, .. } => {
            let (report, code) = cert_report(dir);
            println!("{}", serde_json::to_string_pretty(&report)?);
            finish(code)
        }
        CertCmd::Table { json: as_json } => {
            if *as_json {
                let rules: Vec<Value> = onto_cert::BUILTIN_RULES
                    .lines()
                    .map(|l| json!({ "name": l.split('\t').next().unwrap_or_default(), "line": l }))
                    .collect();
                print_json(&json!({
                    "format": onto_cert::FORMAT,
                    "checker": format!("onto-cert {}", onto_cert::VERSION),
                    "rules": rules,
                }));
            } else {
                print!("{}", onto_cert::BUILTIN_RULES);
            }
            Ok(())
        }
    }
}

// ── shapes ────────────────────────────────────────────────────────────

/// Exit code of a shape report: 0 conforms, 1 violates, 3 undetermined.
pub(crate) fn shape_exit_code(verdict: &str) -> i32 {
    match verdict {
        "conforms" => 0,
        "violates" => 1,
        _ => 3,
    }
}

/// Shapes from a TOML (`[shapes.<name>]`, or the shapes at top level) or JSON file.
fn read_shapes(path: &Path) -> Result<Value> {
    let v = super::read_change_set(path)?;
    Ok(if v["shapes"].is_object() { v["shapes"].clone() } else { v })
}

pub(crate) async fn shapes(cli: &Cli, st: &Style, sub: &ShapesCmd, as_json: bool) -> Result<()> {
    let ws = &cli.workspace;
    match sub {
        ShapesCmd::Validate { inferred, shapes, limit } => {
            let be = backend(cli).await?;
            let r = be
                .call(
                    "shapes.validate",
                    compact(json!({ "shapes": shapes, "inferred": inferred, "limit": limit, "workspace": ws })),
                )
                .await?;
            if as_json {
                print_json(&r);
            } else {
                print_shape_report(st, &r);
            }
            finish(shape_exit_code(s_of(&r["verdict"])))
        }
        ShapesCmd::Induce { kind } => {
            let be = backend(cli).await?;
            let r = be.call("shapes.induce", compact(json!({ "kind": kind, "workspace": ws }))).await?;
            if as_json {
                print_json(&r);
                return Ok(());
            }
            println!(
                "# {} from {} {}",
                st.bold(s_of(&r["name"])),
                n_of(r["entities"].as_u64().unwrap_or(0), "entity"),
                st.dim(&format!("of kind {}", s_of(&r["kind"])))
            );
            for e in r["evidence"].as_array().into_iter().flatten() {
                println!("# {} {}", s_of(&e["constraint"]), st.dim(s_of(&e["text"])));
            }
            for n in r["notes"].as_array().into_iter().flatten() {
                println!("# note: {}", s_of(n));
            }
            let layer = json!({ "shapes": { s_of(&r["name"]): r["shape"] } });
            match toml::to_string(&layer) {
                Ok(t) => print!("\n{t}"),
                Err(_) => print_json(&layer),
            }
            if let Some(m) = r["means"].as_str() {
                println!("# means: {m}");
            }
            Ok(())
        }
        ShapesCmd::Check { file } => {
            let shapes = read_shapes(file)?;
            let be = backend(cli).await?;
            let r = be.call("shapes.check", compact(json!({ "shapes": shapes }))).await?;
            if as_json {
                print_json(&r);
            } else {
                println!(
                    "{} {}",
                    if r["ok"] == true { st.bold("ok") } else { st.red("issues") },
                    st.dim(&format!(
                        "{} · {} checked",
                        n_of(r["shapes"].as_u64().unwrap_or(0), "shape"),
                        n_of(r["constraints"].as_u64().unwrap_or(0), "constraint")
                    ))
                );
                for i in r["issues"].as_array().into_iter().flatten() {
                    println!(
                        "  {:<18} {}{} {}",
                        st.red(s_of(&i["issue"])),
                        s_of(&i["shape"]),
                        i["constraint_id"].as_str().map(|c| format!(" {c}")).unwrap_or_default(),
                        st.dim(s_of(&i["detail"]))
                    );
                }
                print_means(st, &r);
            }
            finish(if r["ok"] == true { 0 } else { 1 })
        }
    }
}

fn print_shape_report(st: &Style, r: &Value) {
    let verdict = s_of(&r["verdict"]);
    let word = match verdict {
        "conforms" => st.bold("conforms"),
        "violates" => st.red("violates"),
        _ => st.cyan("undetermined"),
    };
    let c = &r["counts"];
    println!(
        "{word} {}",
        st.dim(&format!(
            "{} focus nodes · {} violations, {} warnings, {} info{}{}",
            r["focus_nodes"],
            c["violation"],
            c["warning"],
            c["info"],
            if r["inferred"] == true { " · with inferred facts" } else { "" },
            if r["fixpoint_reached"] == false { " (closure is a lower bound)" } else { "" }
        ))
    );
    let results = r["results"].as_array().cloned().unwrap_or_default();
    if !results.is_empty() {
        println!("  {:<9} {:<24} {:<12} {:<24} {:<18} message", "severity", "shape", "component", "focus", "path");
    }
    for x in &results {
        let sev = s_of(&x["severity"]);
        let sev = if sev == "violation" { st.red(&format!("{sev:<9}")) } else { st.cyan(&format!("{sev:<9}")) };
        println!(
            "  {sev} {:<24} {:<12} {:<24} {:<18} {}{}",
            s_of(&x["shape"]),
            s_of(&x["component"]),
            s_of(&x["focus"]),
            s_of(&x["path"]),
            s_of(&x["message"]),
            if x["value"].is_null() { String::new() } else { st.dim(&format!(" (value {})", cell(&x["value"]))) }
        );
    }
    if r["truncated"] == true {
        println!("  {}", st.dim(&format!("{} of {} results listed", results.len(), r["total_results"])));
    }
    for t in strs(&r["unmatched_targets"]) {
        println!("  {} {t}", st.dim("no focus nodes:"));
    }
    let skipped: Vec<&Value> =
        r["skipped"].as_array().into_iter().flatten().filter(|s| s["key"] != "deactivated").collect();
    if !skipped.is_empty() {
        println!("{}", st.cyan("not evaluated (why the verdict can be undetermined):"));
        for s in skipped {
            println!(
                "  {} {} {}",
                s_of(&s["shape"]),
                s["constraint_id"].as_str().map(|c| format!("{c} ")).unwrap_or_default() + s_of(&s["key"]),
                st.dim(s_of(&s["reason"]))
            );
        }
    }
    print_means(st, r);
}

// ── lint and locks ────────────────────────────────────────────────────

/// Error findings a lint report shows (dismissed ones, listed with --all,
/// do not count: a reviewer said they do not apply).
pub(crate) fn lint_errors(r: &Value) -> usize {
    r["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["severity"] == "error" && f["dismissed"] != true)
        .count()
}

pub(crate) async fn lint(cli: &Cli, st: &Style, a: &LintArgs) -> Result<()> {
    let be = backend(cli).await?;
    if let Some((id, decision)) =
        a.dismiss.as_ref().map(|i| (i, "dismiss")).or_else(|| a.accept.as_ref().map(|i| (i, "accept")))
    {
        let r = be
            .call(
                "schema.lint_feedback",
                compact(json!({ "id": id, "decision": decision, "note": a.note, "workspace": cli.workspace })),
            )
            .await?;
        if a.json {
            print_json(&r);
        } else {
            println!(
                "{} {} {}",
                st.bold(if decision == "dismiss" { "dismissed" } else { "accepted" }),
                s_of(&r["id"]),
                st.dim(&format!("{} {} · by {}", s_of(&r["check"]), s_of(&r["target"]), s_of(&r["by"])))
            );
        }
        return Ok(());
    }
    if a.note.is_some() {
        return Err(Error::invalid("--note goes with --dismiss or --accept"));
    }
    let checks = if a.checks.is_empty() { Value::Null } else { json!(a.checks) };
    let r = be.call("schema.lint", compact(json!({ "checks": checks, "include_dismissed": a.all }))).await?;
    if a.json {
        print_json(&r);
    } else {
        for f in r["findings"].as_array().into_iter().flatten() {
            let sev = s_of(&f["severity"]);
            let word = match sev {
                "error" => st.red(&format!("{sev:<7}")),
                "warning" => st.cyan(&format!("{sev:<7}")),
                _ => st.dim(&format!("{sev:<7}")),
            };
            let mut marks = String::new();
            if f["accepted"] == true {
                marks.push_str(" [accepted]");
            }
            if f["dismissed"] == true {
                marks.push_str(" [dismissed]");
            }
            println!(
                "{word} {} {} {}{}",
                st.dim(s_of(&f["id"])),
                s_of(&f["check"]),
                s_of(&f["target"]),
                st.dim(&marks)
            );
            println!("        {}", s_of(&f["message"]));
            if let Some(fix) = f["fix"].as_str().filter(|s| !s.is_empty()) {
                println!("        {}", st.dim(&format!("fix: {fix}")));
            }
            if let Some(n) = f["note"].as_str() {
                println!("        {}", st.dim(&format!("note: {n}")));
            }
        }
        let c = &r["counts"];
        println!(
            "{}",
            st.dim(&format!(
                "{} errors, {} warnings, {} info · {} suppressed (dismissed) · {} checks ran",
                c["error"],
                c["warning"],
                c["info"],
                r["suppressed"].as_u64().unwrap_or(0),
                len_of(&r["checks_run"])
            ))
        );
        for n in r["not_run"].as_array().into_iter().flatten().chain(r["partial"].as_array().into_iter().flatten()) {
            println!("  {} {} {}", st.cyan("not run:"), s_of(&n["check"]), st.dim(s_of(&n["reason"])));
        }
        print_means(st, &r);
    }
    finish(if lint_errors(&r) > 0 { 1 } else { 0 })
}

fn lock_line(st: &Style, l: &Value) -> String {
    format!(
        "{} {} {}",
        st.cyan(s_of(&l["section"])),
        st.bold(s_of(&l["name"])),
        st.dim(&format!("locked by {} at {}: {}", s_of(&l["locked_by"]), s_of(&l["at"]), s_of(&l["reason"])))
    )
}

pub(crate) async fn lock(cli: &Cli, st: &Style, a: &LockArgs) -> Result<()> {
    let be = backend(cli).await?;
    let r =
        be.call("schema.lock", compact(json!({ "section": a.section, "name": a.name, "reason": a.reason }))).await?;
    if a.json {
        print_json(&r)
    } else {
        println!("{}", lock_line(st, &r))
    }
    Ok(())
}

pub(crate) async fn unlock(cli: &Cli, st: &Style, a: &UnlockArgs) -> Result<()> {
    let be = backend(cli).await?;
    let r = be.call("schema.unlock", compact(json!({ "section": a.section, "name": a.name }))).await?;
    if a.json {
        print_json(&r);
    } else if r["removed"] == true {
        println!("{} {} {}", st.bold("unlocked"), a.section, a.name);
    } else {
        println!("{} {} {}", st.dim("not locked:"), a.section, a.name);
    }
    Ok(())
}

pub(crate) async fn locks(cli: &Cli, st: &Style, as_json: bool) -> Result<()> {
    let be = backend(cli).await?;
    let r = be.call("schema.locks", compact(json!({}))).await?;
    if as_json {
        print_json(&r);
        return Ok(());
    }
    let items = r["items"].as_array().cloned().unwrap_or_default();
    for l in &items {
        println!("{}", lock_line(st, l));
    }
    if items.is_empty() {
        println!("{}", st.dim("no locks"));
    }
    Ok(())
}

// ── packs ─────────────────────────────────────────────────────────────

pub(crate) async fn pack(cli: &Cli, st: &Style, sub: &PackCmd, as_json: bool) -> Result<()> {
    let be = backend(cli).await?;
    match sub {
        PackCmd::List => {
            let r = be.call("pack.list", compact(json!({}))).await?;
            if as_json {
                print_json(&r);
                return Ok(());
            }
            for p in r["items"].as_array().into_iter().flatten() {
                let c = &p["counts"];
                let status = if p["installed"] == true {
                    st.bold("installed")
                } else if let Some(b) = p["blocked"].as_str() {
                    st.red(&format!("blocked: {b}"))
                } else {
                    st.dim(&format!("{} changes to install", p["pending_changes"]))
                };
                println!(
                    "{:<14} {:<8} {:<40} {}",
                    st.cyan(s_of(&p["id"])),
                    s_of(&p["version"]),
                    s_of(&p["title"]),
                    status
                );
                println!(
                    "    {}",
                    st.dim(&format!(
                        "{} · {} object types, {} link types, {} rules, {} shapes, {} metrics, {} actions, {} sample entities{}",
                        s_of(&p["industry"]),
                        c["object_types"],
                        c["link_types"],
                        c["rules"],
                        c["shapes"],
                        c["metrics"],
                        c["action_types"],
                        c["sample_entities"],
                        if len_of(&p["standards"]) > 0 {
                            format!(" · borrows from {}", strs(&p["standards"]).join(", "))
                        } else {
                            String::new()
                        }
                    ))
                );
                let missing = strs(&p["missing_requires"]);
                if !missing.is_empty() {
                    println!("    {} {}", st.red("requires"), missing.join(", "));
                }
            }
            Ok(())
        }
        PackCmd::Show { id } => {
            let r = be.call("pack.show", compact(json!({ "id": id }))).await?;
            if as_json {
                print_json(&r);
            } else {
                print_pack(st, &r);
            }
            Ok(())
        }
        PackCmd::Plan { id } => {
            let r = be.call("pack.plan", compact(json!({ "id": id }))).await?;
            if as_json {
                print_json(&r)
            } else {
                print_plan(st, &r)
            }
            Ok(())
        }
        PackCmd::Install { id, propose, sample, message } => {
            let input = json!({
                "id": id, "mode": if *propose { "propose" } else { "apply" }, "sample": sample,
                "message": message, "workspace": cli.workspace,
            });
            let r = be.call("pack.install", compact(input)).await?;
            if as_json {
                print_json(&r);
                return Ok(());
            }
            let what = format!("{} {}", s_of(&r["pack"]), s_of(&r["pack_version"]));
            if r["already_installed"] == true {
                println!("{} {what} {}", st.bold("installed"), st.dim("already: nothing to change"));
            } else if r["proposal"].is_object() {
                println!(
                    "{} {what} {}",
                    st.cyan("proposed"),
                    st.dim(&format!(
                        "{} · proposal {} (review with `onto schema proposal {}`)",
                        n_of(r["changes"].as_u64().unwrap_or(0), "schema change"),
                        s_of(&r["proposal"]["id"]),
                        s_of(&r["proposal"]["id"])
                    ))
                );
            } else {
                println!(
                    "{} {what} {}",
                    st.bold("installed"),
                    st.dim(&format!(
                        "{} · schema v{}",
                        n_of(r["changes"].as_u64().unwrap_or(0), "schema change"),
                        r["version"]["version"]
                    ))
                );
            }
            if r["sample_entities"].as_u64().unwrap_or(0) > 0 || r["sample_links"].as_u64().unwrap_or(0) > 0 {
                println!(
                    "  sample: {} entities, {} links{}",
                    r["sample_entities"],
                    r["sample_links"],
                    match r["sample_links_skipped"].as_u64().unwrap_or(0) {
                        0 => String::new(),
                        n => format!(" ({n} skipped: curated data kept)"),
                    }
                );
            }
            Ok(())
        }
    }
}

fn print_pack(st: &Style, r: &Value) {
    println!(
        "{} {} {}",
        st.bold(s_of(&r["title"])),
        st.dim(&format!("{} {}", s_of(&r["id"]), s_of(&r["version"]))),
        st.dim(s_of(&r["industry"]))
    );
    if let Some(d) = r["description"].as_str() {
        println!("{d}");
    }
    let requires = strs(&r["requires"]);
    if !requires.is_empty() {
        println!("{} {}", st.dim("requires"), requires.join(", "));
    }
    let standards = strs(&r["standards"]);
    if !standards.is_empty() {
        println!("{} {}", st.dim("borrows vocabulary from"), standards.join(", "));
    }
    let items = |title: &str, key: &str| {
        let list = r[key].as_array().cloned().unwrap_or_default();
        if list.is_empty() {
            return;
        }
        println!("\n{}", st.bold(title));
        for i in list {
            println!("  {:<28} {}", s_of(&i["name"]), st.dim(s_of(&i["description"])));
        }
    };
    items("Object types", "object_types");
    items("Interfaces", "interfaces");
    let links = r["link_types"].as_array().cloned().unwrap_or_default();
    if !links.is_empty() {
        println!("\n{}", st.bold("Link types"));
        for l in links {
            let mut flags = Vec::new();
            for f in ["transitive", "dependency"] {
                if l[f] == true {
                    flags.push(f);
                }
            }
            println!(
                "  {:<28} {} → {} {}",
                s_of(&l["name"]),
                strs(&l["from"]).join("|"),
                strs(&l["to"]).join("|"),
                st.dim(&flags.join(" "))
            );
        }
    }
    let rules = r["rules"].as_array().cloned().unwrap_or_default();
    if !rules.is_empty() {
        println!("\n{} {}", st.bold("Rules"), st.dim("(supplied: assumptions, not facts)"));
        for x in rules {
            println!("  {:<28} {} {}", s_of(&x["name"]), s_of(&x["text"]), st.dim(&format!("({})", s_of(&x["form"]))));
        }
    }
    items("Shapes", "shapes");
    items("Metrics", "metrics");
    items("Action types", "action_types");
    if let Some(note) = r["sample_note"].as_str() {
        println!("\n{} {note}", st.dim("sample:"));
    }
    let lint = if r["lint"].is_object() { &r["lint"] } else { &r["lint_summary"] };
    if lint.is_object() {
        let c = if lint["counts"].is_object() { &lint["counts"] } else { lint };
        println!(
            "\n{} {} errors, {} warnings, {} info {}",
            st.bold("Lint (the pack's own findings):"),
            c["error"],
            c["warning"],
            c["info"],
            st.dim("· a measurement")
        );
    }
    print_means(st, r);
}

// ── align ─────────────────────────────────────────────────────────────

/// Mappings to certify, from SSSOM TSV or JSON (`[{source, target, predicate}]`
/// or `{mappings: [...]}`).
fn read_mappings(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
    let trimmed = text.trim_start();
    if path.extension().is_some_and(|e| e == "json") || trimmed.starts_with('[') || trimmed.starts_with('{') {
        let v: Value = serde_json::from_str(&text)?;
        return Ok(if v["mappings"].is_array() { v["mappings"].clone() } else { v });
    }
    let rows = onto_intelli::core::align::parse_sssom(&text)?;
    Ok(serde_json::to_value(rows)?)
}

pub(crate) async fn align(cli: &Cli, st: &Style, a: &AlignArgs) -> Result<()> {
    let mut input = json!({});
    if let Some(p) = &a.pack {
        input["pack"] = json!(p);
    }
    if let Some(f) = &a.file {
        let text = std::fs::read_to_string(f).map_err(|e| Error::invalid(format!("{}: {e}", f.display())))?;
        input["toml"] = json!(text);
    }
    let be = backend(cli).await?;
    if let Some(m) = &a.certify {
        input["mappings"] = read_mappings(m)?;
        let r = be.call("schema.align_certify", compact(input)).await?;
        if let Some(out) = &a.sssom
            && let Some(tsv) = r["sssom_tsv"].as_str()
        {
            write_file(out, tsv)?;
        }
        if a.json {
            print_json(&r);
            return Ok(());
        }
        let rows = if r["items"].is_array() { &r["items"] } else { &r["rows"] };
        for x in rows.as_array().into_iter().flatten() {
            let word = s_of(&x["certification"]);
            let mark = match word {
                "upheld" => st.bold(word),
                "downgraded" => st.red(word),
                _ => st.cyan(word),
            };
            let pred = if x["claimed"] != x["predicate"] {
                format!("{} → {}", s_of(&x["claimed"]), s_of(&x["predicate"]))
            } else {
                s_of(&x["predicate"]).to_string()
            };
            println!("{mark:<18} {} {pred} {}  {}", s_of(&x["source"]), s_of(&x["target"]), st.dim(s_of(&x["reason"])));
        }
        print_means(st, &r);
        return Ok(());
    }
    if let Some(t) = a.threshold {
        input["threshold"] = json!(t);
    }
    let r = be.call("schema.align", compact(input)).await?;
    if let Some(out) = &a.sssom {
        write_file(out, s_of(&r["sssom_tsv"]))?;
    }
    if a.json {
        print_json(&r);
        return Ok(());
    }
    println!(
        "{} → {} {}",
        st.bold(s_of(&r["source_schema"])),
        st.bold(s_of(&r["target_schema"])),
        st.dim(&format!(
            "{} accepted, {} borderline, {} source and {} target unmatched",
            r["counts"]["accepted"],
            r["counts"]["borderline"],
            r["counts"]["unmatched_source"],
            r["counts"]["unmatched_target"]
        ))
    );
    for (title, key) in [("Object types", "kinds"), ("Link types", "links")] {
        let rows = r[key].as_array().cloned().unwrap_or_default();
        if rows.is_empty() {
            continue;
        }
        println!("\n{}", st.bold(title));
        for m in rows {
            let status = s_of(&m["status"]);
            let mark = if status == "accepted" {
                st.bold(&format!("{status:<10}"))
            } else {
                st.cyan(&format!("{status:<10}"))
            };
            println!(
                "  {mark} {:<24} {:<14} {:<24} {}",
                s_of(&m["source"]),
                s_of(&m["predicate"]),
                s_of(&m["target"]),
                st.dim(&format!("score {}", m["score"]))
            );
            for e in m["evidence"].as_array().into_iter().flatten() {
                println!("      {}", st.dim(s_of(e)));
            }
        }
    }
    for (title, key) in [("Unmatched source", "unmatched_source"), ("Unmatched target", "unmatched_target")] {
        let rows = r[key].as_array().cloned().unwrap_or_default();
        if !rows.is_empty() {
            let names: Vec<String> = rows.iter().map(|u| s_of(&u["name"]).to_string()).collect();
            println!("{} {}", st.dim(&format!("{title}:")), names.join(", "));
        }
    }
    for w in r["warnings"].as_array().into_iter().flatten() {
        println!("{} {}", st.cyan("warning"), s_of(w));
    }
    if let Some(out) = &a.sssom {
        println!("wrote {}", out.display());
    }
    print_means(st, &r);
    Ok(())
}

// ── induce ────────────────────────────────────────────────────────────

pub(crate) async fn induce(cli: &Cli, st: &Style, a: &InduceArgs) -> Result<()> {
    let csv = std::fs::read_to_string(&a.file).map_err(|e| Error::invalid(format!("{}: {e}", a.file.display())))?;
    let file = a.file.file_name().map(|f| f.to_string_lossy().to_string());
    let be = backend(cli).await?;
    if a.ingest {
        let mapping = match &a.mapping {
            Some(path) => {
                let text =
                    std::fs::read_to_string(path).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
                let v: Value = serde_json::from_str(&text)
                    .map_err(|e| Error::invalid(format!("{}: not JSON: {e}", path.display())))?;
                Some(v)
            }
            None => None,
        };
        let input = json!({
            "csv": csv, "kind": a.kind, "key": a.key, "file": file,
            "workspace": cli.workspace, "dry_run": a.dry_run, "mapping": mapping,
        });
        let r = be.call("ingest.sheet", compact(input)).await?;
        if a.json {
            print_json(&r);
            return Ok(());
        }
        println!(
            "{} {} {}",
            st.bold(if r["dry_run"] == true { "dry run" } else { "ingested" }),
            s_of(&r["kind"]),
            st.dim(&format!(
                "{} rows · {} objects · {} links{}",
                r["rows"],
                r["upserts"],
                r["links"],
                r["revision"].as_u64().map(|v| format!(" · revision {v}")).unwrap_or_default()
            ))
        );
        for f in r["findings"].as_array().into_iter().flatten() {
            println!(
                "  {} row {} {} {}",
                st.cyan(s_of(&f["issue"])),
                f["row"],
                f["column"].as_str().unwrap_or_default(),
                st.dim(s_of(&f["message"]))
            );
        }
        let total = r["findings_total"].as_u64().unwrap_or(0);
        if total as usize > len_of(&r["findings"]) {
            println!("  {}", st.dim(&format!("{total} findings in all")));
        }
        print_means(st, &r);
        return Ok(());
    }
    let r = be.call("model.induce", compact(json!({ "csv": csv, "kind": a.kind, "key": a.key, "file": file }))).await?;
    if let Some(out) = &a.change_set {
        write_file(out, &serde_json::to_string_pretty(&r["change_set"])?)?;
    }
    if a.json {
        print_json(&r);
        return Ok(());
    }
    println!(
        "{} {} {}",
        st.bold(s_of(&r["kind"])),
        st.dim(&format!("from {} rows", r["rows"])),
        st.dim(s_of(&r["kind_evidence"]))
    );
    for (what, c) in [("key", &r["key"]), ("title", &r["title"])] {
        println!("  {:<6} {} {}", what, c["column"].as_str().unwrap_or("none"), st.dim(s_of(&c["evidence"])));
    }
    for c in r["columns"].as_array().into_iter().flatten() {
        println!(
            "  {:<24} {:<9} {:<10} {}",
            s_of(&c["column"]),
            s_of(&c["role"]),
            st.cyan(s_of(&c["type"])),
            st.dim(&format!(
                "{}{} filled, {} distinct · {}",
                if c["required"] == true { "required · " } else { "" },
                c["filled"],
                c["distinct"],
                s_of(&c["evidence"])
            ))
        );
    }
    for l in r["links"].as_array().into_iter().flatten() {
        println!(
            "  {} {} → {} {}",
            st.bold("link"),
            s_of(&l["rel"]),
            s_of(&l["target_kind"]),
            st.dim(&format!("{}/{} cells match · {}", l["matched"], l["filled"], s_of(&l["evidence"])))
        );
    }
    for e in r["shape_evidence"].as_array().into_iter().flatten() {
        println!("  {} {} {}", st.dim("shape"), s_of(&e["constraint"]), st.dim(s_of(&e["text"])));
    }
    for w in r["warnings"].as_array().into_iter().flatten() {
        println!("{} {}", st.cyan("warning"), s_of(w));
    }
    let changes = len_of(&r["change_set"]["changes"]);
    match &a.change_set {
        Some(out) => println!(
            "wrote {} ({}): `onto schema plan {0}`, then `onto schema apply {0}`, then `onto induce {} --ingest`",
            out.display(),
            n_of(changes as u64, "change"),
            a.file.display()
        ),
        None => println!(
            "{}",
            st.dim(&format!("{} in the change set (save it with --change-set FILE)", n_of(changes as u64, "change")))
        ),
    }
    print_means(st, &r);
    Ok(())
}

// ── plans: the semantic fields ────────────────────────────────────────

/// The semantic parts of a schema or action plan: consequences, lock
/// violations, likely renames and the shape delta (absent parts print nothing).
pub(crate) fn print_semantics(st: &Style, plan: &Value) {
    let c = &plan["consequences"];
    if c.is_object() {
        if let Some(why) = c["skipped"].as_str() {
            println!("  {} {}", st.dim("consequences not computed:"), st.dim(why));
        } else {
            let verdict = c["verdict"].as_str().unwrap_or(if c["conservative"] == true {
                "conservative"
            } else {
                "undetermined"
            });
            let word = match verdict {
                "conservative" => st.bold("conservative"),
                "not_conservative" => {
                    st.red(&format!("not conservative ({} new over existing names)", c["new_over_existing_vocabulary"]))
                }
                other => st.cyan(other),
            };
            // Added facts are asserted (a new `extends`, a link type made
            // transitive) or derived; only the latter are the reasoner's.
            let added = c["added"].as_u64().unwrap_or(0);
            let asserted = c["by_rule"]["asserted"].as_u64().unwrap_or(0).min(added);
            let added_words = match (asserted, added - asserted) {
                (0, d) => format!("+{d} derived"),
                (a, 0) => format!("+{a} asserted"),
                (a, d) => format!("+{added} added ({a} asserted, {d} derived)"),
            };
            let mut notes = String::new();
            if c["fixpoint_reached"] == false {
                notes.push_str(" (a run stopped before its fixpoint)");
            }
            if let Some(n) = c["refused"].as_u64().filter(|n| *n > 0) {
                notes.push_str(&format!(" ({n} refused term(s): their facts are left out)"));
            }
            println!(
                "  consequences: {added_words}, -{} removed · {word}{}",
                c["removed"],
                if notes.is_empty() { notes } else { st.dim(&notes) }
            );
            if let Some(by) = c["by_rule"].as_object().filter(|m| !m.is_empty()) {
                let parts: Vec<String> = by.iter().map(|(r, n)| format!("{r} {n}")).collect();
                println!("      {}", st.dim(&parts.join(" · ")));
            }
            let samples = if len_of(&c["witnesses"]) > 0 { c["witnesses"].clone() } else { c["samples_added"].clone() };
            let samples = samples.as_array().cloned().unwrap_or_default();
            for d in samples.iter().take(5) {
                println!("      + {}", derivation_line(st, d));
            }
            if samples.len() > 5 {
                println!("      {}", st.dim(&format!("… {} more listed with --json", samples.len() - 5)));
            }
            for f in c["samples_removed"].as_array().into_iter().flatten().take(5) {
                println!("      - {}", fact(f));
            }
        }
    }
    for v in plan["locked_violations"].as_array().into_iter().flatten() {
        let tag = if v["overridden"] == true { st.cyan("locked (overridden)") } else { st.red("locked") };
        println!(
            "  {tag} {} {}: {} {}",
            s_of(&v["section"]),
            s_of(&v["name"]),
            s_of(&v["reason"]),
            st.dim(&format!(
                "(by {}{})",
                s_of(&v["locked_by"]),
                v["lock_reason"].as_str().map(|r| format!(": {r}")).unwrap_or_default()
            ))
        );
    }
    for r in plan["likely_renames"].as_array().into_iter().flatten() {
        println!(
            "  {} {} {} → {} {}",
            st.cyan("rename?"),
            s_of(&r["section"]),
            s_of(&r["from"]),
            s_of(&r["to"]),
            st.dim(&format!("score {}", r["score"]))
        );
        for e in r["evidence"].as_array().into_iter().flatten() {
            println!("      {}", st.dim(s_of(e)));
        }
    }
    let s = &plan["shapes"];
    if s.is_object() {
        let new = len_of(&s["new_results"]);
        let resolved = len_of(&s["resolved"]);
        let verdicts = match (s["before"].as_str(), s["after"].as_str()) {
            (Some(b), Some(a)) => format!(" · {b} → {a}"),
            _ => String::new(),
        };
        let word = if new > 0 { st.red(&format!("{new} new results")) } else { format!("{new} new results") };
        println!("  shapes: {word}, {resolved} resolved{verdicts}");
        if let Some(why) = s["skipped"].as_str() {
            println!("      {}", st.dim(why));
        }
        let samples = if s["samples"].is_array() { &s["samples"] } else { &s["new_results"] };
        for x in samples.as_array().into_iter().flatten().take(5) {
            println!(
                "      {} {} {} {}",
                s_of(&x["shape"]),
                s_of(&x["focus"]),
                s_of(&x["component"]),
                st.dim(s_of(&x["message"]))
            );
        }
    }
}

/// The risk word and its reasons (schema plans).
pub(crate) fn print_risk(st: &Style, plan: &Value) {
    let Some(risk) = plan["risk"].as_str() else { return };
    let word = match risk {
        "high" => st.red("risk high"),
        "medium" => st.cyan("risk medium"),
        _ => st.bold("risk low"),
    };
    let reasons = strs(&plan["risk_reasons"]);
    if reasons.is_empty() {
        println!("  {word}");
    } else {
        println!("  {word}: {}", reasons.join("; "));
    }
}

// ── mcp profiles ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_verdicts_map_to_exit_codes() {
        assert_eq!(shape_exit_code("conforms"), 0);
        assert_eq!(shape_exit_code("violates"), 1);
        assert_eq!(shape_exit_code("undetermined"), 3);
        assert_eq!(shape_exit_code(""), 3);
    }

    #[test]
    fn lint_exit_counts_only_shown_errors() {
        let r = json!({ "findings": [
            { "severity": "error", "dismissed": true },
            { "severity": "warning" },
        ]});
        assert_eq!(lint_errors(&r), 0);
        let r = json!({ "findings": [{ "severity": "error", "accepted": true }] });
        assert_eq!(lint_errors(&r), 1);
    }

    #[test]
    fn patterns_and_terms_parse() {
        assert_eq!(pattern_atoms(json!(["?x", "a", "team"])).unwrap(), json!([["?x", "a", "team"]]));
        assert_eq!(pattern_atoms(json!([["?x", "a", "team"]])).unwrap(), json!([["?x", "a", "team"]]));
        assert!(pattern_atoms(json!({"x": 1})).is_err());
        assert_eq!(object_term("42"), json!(42));
        assert_eq!(object_term("team:payments"), json!("team:payments"));
        assert_eq!(object_term("\"high\""), json!("high"));
        assert_eq!(
            claim_list(json!({"subject": "a", "relation": "b", "object": "c"})).as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(fact(&json!({"s": "<a>", "p": "r", "o": "<b>"})), "<a> r <b>");
    }
}
