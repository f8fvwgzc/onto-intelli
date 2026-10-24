//! The assurance commands of the `onto` binary.
//!
//! `onto cert` is offline (the onto-cert library on files) and is tested in
//! full here: on the checker's own fixtures and on a certificate the engine
//! writes for the demo data, accepted and then forged. Commands that call
//! catalogue actions run against an in-memory store (one process each) or,
//! for the end-to-end `pack install --sample` → `reason certify --out` →
//! `cert check` run, a PostgreSQL tenant of their own (skipped without
//! `DATABASE_URL`). An action the engine does not dispatch yet makes its
//! test print a note and return.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use onto_intelli::config::Config;
use onto_intelli::core::reason::ReasonOptions;
use onto_intelli::{Intelli, Principal, seed};
use serde_json::Value;

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| panic!("{e}: {}\n{}", self.stdout, self.stderr))
    }
}

/// Run `onto` with `args`, a clean environment for its own variables and
/// `env` on top.
fn onto_env(args: &[&str], env: &[(&str, &str)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_onto"));
    cmd.args(args).env("NO_COLOR", "1").stdin(Stdio::null());
    for v in ["ONTO_CONFIG", "ONTO_REMOTE", "ONTO_TOKEN", "ONTO_TENANT", "ONTO_MCP_PROFILE"] {
        cmd.env_remove(v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run onto");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn onto(args: &[&str]) -> Out {
    onto_env(args, &[])
}

/// A new empty directory under the system temp dir.
fn scratch(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "onto-cli-{name}-{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../onto-cert/tests/fixtures")
}

/// What `onto-cert check <dir>` prints and returns, from the library.
fn library_report(dir: &Path) -> (String, i32) {
    let (report, code) = match onto_cert::CertFiles::from_dir(dir) {
        Ok(files) => {
            let r = onto_cert::check(&files);
            (r.to_json(), r.exit_code())
        }
        Err(e) => {
            (serde_json::json!({"ok": false, "error": "unreadable", "file": e.file, "reason": e.error.to_string()}), 2)
        }
    };
    (format!("{}\n", serde_json::to_string_pretty(&report).unwrap()), code)
}

/// A config file for an in-memory store (each `onto` process starts empty).
fn memory_config() -> PathBuf {
    let dir = scratch("cfg");
    let path = dir.join("onto-intelli.toml");
    std::fs::write(&path, "[engine]\nstorage = \"memory\"\ndefault_workspace = \"acme\"\n").unwrap();
    path
}

/// False (with a note) when the engine does not dispatch the action yet.
fn dispatched(o: &Out, action: &str) -> bool {
    if o.stderr.contains("see system.actions") {
        eprintln!("skipped: {action} is not dispatched by this engine yet (run again after the I1 merge)");
        return false;
    }
    true
}

// ── onto cert (offline) ───────────────────────────────────────────────

#[test]
fn cert_check_prints_the_onto_cert_report_and_exit_code_for_every_fixture() {
    let mut seen = 0;
    for entry in std::fs::read_dir(fixtures()).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        let out = onto(&["cert", "check", dir.to_str().unwrap()]);
        let (expected, code) = library_report(&dir);
        assert_eq!(out.stdout, expected, "{}", dir.display());
        assert_eq!(out.code, code, "{}: {}", dir.display(), out.stdout);
        seen += 1;
    }
    assert!(seen >= 10, "fixtures found: {seen}");

    let out = onto(&["cert", "check", fixtures().join("coffee").to_str().unwrap()]);
    let r = out.json();
    assert_eq!((out.code, r["verdict"].as_str()), (0, Some("entailed")), "{r}");
    let out = onto(&["cert", "check", fixtures().join("coffee-forged").to_str().unwrap()]);
    let r = out.json();
    assert_eq!(
        (out.code, r["first_rejected"].as_u64(), r["reason"].as_str()),
        (1, Some(3), Some("conclusion_mismatch"))
    );
    let out = onto(&["cert", "check", fixtures().join("supplied-chain").to_str().unwrap(), "--json"]);
    assert_eq!((out.code, out.json()["verdict"].as_str()), (0, Some("entailed_under_supplied_rules")));
}

#[test]
fn cert_check_of_a_missing_directory_exits_2() {
    let missing = scratch("missing").join("nothing-here");
    let out = onto(&["cert", "check", missing.to_str().unwrap()]);
    assert_eq!(out.code, 2, "{}", out.stdout);
    let r = out.json();
    assert_eq!(
        (r["ok"].as_bool(), r["error"].as_str(), r["file"].as_str()),
        (Some(false), Some("unreadable"), Some("rules.tsv"))
    );
}

#[test]
fn cert_table_prints_the_builtin_rules() {
    let out = onto(&["cert", "table"]);
    assert_eq!((out.code, out.stdout.as_str()), (0, onto_cert::BUILTIN_RULES));
    let out = onto(&["cert", "table", "--json"]);
    let r = out.json();
    assert_eq!(r["rules"].as_array().map(Vec::len), Some(5), "{r}");
    assert_eq!(r["rules"][3]["name"], "builtin/transitive");
    assert_eq!(r["checker"], format!("onto-cert {}", onto_cert::VERSION));
}

/// A certificate the engine writes for the demo data passes `onto cert
/// check` with the relative verdict (the enterprise schema carries supplied
/// chain rules), and a forged derivation line is refused, by line and rule.
#[tokio::test]
async fn engine_certificate_is_accepted_and_a_forged_line_refused() {
    let mut cfg = Config::in_memory();
    cfg.engine.default_workspace = "acme".into();
    let i = Intelli::open(cfg).await.unwrap();
    seed::load_demo(&i, "acme").await.unwrap();
    let cert = i.ontology().reason_certificate(&Principal::admin("lead"), &ReasonOptions::default()).unwrap();
    let dir = scratch("engine-cert");
    cert.write_to(&dir).unwrap();

    let out = onto(&["cert", "check", dir.to_str().unwrap()]);
    let r = out.json();
    assert_eq!(out.code, 0, "{r}");
    assert_eq!(r["verdict"], "entailed_under_supplied_rules", "{r}");
    assert!(r["rules_cited"]["supplied"].as_u64().unwrap() > 0, "{r}");
    assert!(r["derivations"].as_u64().unwrap() > 0, "{r}");

    // Forge the first derivation line's conclusion object; drop the
    // manifest so the forgery is judged by re-derivation, not by digest.
    let forged = scratch("engine-forged");
    cert.write_to(&forged).unwrap();
    std::fs::remove_file(forged.join("manifest.json")).unwrap();
    let mut lines: Vec<String> = cert.derivations_tsv.lines().map(str::to_string).collect();
    let mut fields: Vec<String> = lines[0].split('\t').map(str::to_string).collect();
    fields[3] = "<forged:object>".into();
    let rule = fields[0].clone();
    lines[0] = fields.join("\t");
    std::fs::write(forged.join("derivations.tsv"), format!("{}\n", lines.join("\n"))).unwrap();
    let out = onto(&["cert", "check", forged.to_str().unwrap()]);
    let r = out.json();
    assert_eq!(out.code, 1, "{r}");
    assert_eq!(r["ok"], false);
    assert_eq!(r["first_rejected"], 1, "{r}");
    assert_eq!(r["rule"], rule.as_str(), "{r}");
    assert_eq!(r["reason"], "conclusion_mismatch", "{r}");

    // The same edit with the manifest kept is caught by its digest.
    let tampered = scratch("engine-tampered");
    cert.write_to(&tampered).unwrap();
    std::fs::write(tampered.join("derivations.tsv"), format!("{}\n", lines.join("\n"))).unwrap();
    let out = onto(&["cert", "check", tampered.to_str().unwrap()]);
    let r = out.json();
    assert_eq!(
        (out.code, r["reason"].as_str(), r["field"].as_str()),
        (1, Some("digest_mismatch"), Some("derivations_sha256")),
        "{r}"
    );
}

// ── onto mcp --profile ────────────────────────────────────────────────

#[test]
fn mcp_profile_flag_beats_the_environment_and_unknown_names_are_refused() {
    let cfg = memory_config();
    let cfg = cfg.to_str().unwrap();
    let out = onto(&["--config", cfg, "mcp", "--profile", "no_such_profile"]);
    assert_ne!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.contains("no_such_profile") && out.stderr.contains("catalog"), "{}", out.stderr);

    let out = onto_env(&["--config", cfg, "mcp"], &[("ONTO_MCP_PROFILE", "no_such_profile")]);
    assert_ne!(out.code, 0, "the environment names the profile when the flag is absent: {}", out.stderr);

    // The flag wins over the environment; with stdin closed the server exits cleanly.
    let out = onto_env(&["--config", cfg, "mcp", "--profile", "full"], &[("ONTO_MCP_PROFILE", "no_such_profile")]);
    assert_eq!(out.code, 0, "{}", out.stderr);
}

// ── catalogue actions on an in-memory store ───────────────────────────

#[test]
fn lint_pack_list_and_reason_run_print_reports() {
    let cfg = memory_config();
    let cfg = cfg.to_str().unwrap();

    let out = onto(&["--config", cfg, "lint", "--json"]);
    if dispatched(&out, "schema.lint") {
        let r = out.json();
        assert!(r["findings"].is_array() && r["means"].is_string(), "{r}");
        let errors = r["findings"].as_array().unwrap().iter().filter(|f| f["severity"] == "error").count();
        assert_eq!(out.code, if errors > 0 { 1 } else { 0 }, "{r}");
        let styled = onto(&["--config", cfg, "lint"]);
        assert_eq!(styled.code, out.code, "{}", styled.stderr);
        assert!(styled.stdout.contains("suppressed"), "{}", styled.stdout);
    }

    let out = onto(&["--config", cfg, "pack", "list", "--json"]);
    if dispatched(&out, "pack.list") {
        let r = out.json();
        let ids: Vec<&str> = r["items"].as_array().unwrap().iter().filter_map(|p| p["id"].as_str()).collect();
        assert!(ids.contains(&"manufacturing"), "{r}");
        let styled = onto(&["--config", cfg, "pack", "list"]);
        assert_eq!(styled.code, 0, "{}", styled.stderr);
        assert!(styled.stdout.contains("manufacturing"), "{}", styled.stdout);
    }

    let out = onto(&["--config", cfg, "reason", "run", "--json"]);
    if dispatched(&out, "reason.run") {
        assert_eq!(out.code, 0, "{}", out.stderr);
        let styled = onto(&["--config", cfg, "reason", "run"]);
        assert!(styled.stdout.contains("not evaluated"), "{}", styled.stdout);
        assert!(styled.stdout.contains("a clean run is not a consistency result"), "{}", styled.stdout);
    }

    let out = onto(&["--config", cfg, "pack", "plan", "manufacturing"]);
    if dispatched(&out, "pack.plan") {
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert!(out.stdout.contains("risk "), "{}", out.stdout);
    }
}

#[test]
fn shapes_validate_exit_code_follows_the_verdict() {
    let cfg = memory_config();
    let cfg = cfg.to_str().unwrap();
    let out = onto(&["--config", cfg, "shapes", "validate", "--json"]);
    if !dispatched(&out, "shapes.validate") {
        // A failed run is not a verdict: 4, never 1 (violates).
        assert_eq!(out.code, 4, "{}", out.stderr);
        return;
    }
    let r = out.json();
    let expected = match r["verdict"].as_str() {
        Some("conforms") => 0,
        Some("violates") => 1,
        _ => 3,
    };
    assert_eq!(out.code, expected, "{r}");
}

#[test]
fn reason_certify_writes_files_that_cert_check_accepts() {
    let cfg = memory_config();
    let cfg = cfg.to_str().unwrap();
    let dir = scratch("certify-out");
    let out = onto(&["--config", cfg, "reason", "certify", "--out", dir.to_str().unwrap()]);
    if !dispatched(&out, "reason.certify") {
        return;
    }
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    assert!(out.stdout.contains(dir.to_str().unwrap()), "{}", out.stdout);
    for f in ["rules.tsv", "asserted.tsv", "derivations.tsv", "manifest.json"] {
        assert!(dir.join(f).is_file(), "{f} written");
    }
    let check = onto(&["cert", "check", dir.to_str().unwrap()]);
    assert_eq!(check.code, 0, "{}", check.stdout);
    assert!(out.stdout.contains(check.json()["verdict"].as_str().unwrap()), "{}", out.stdout);
}

// ── end to end on PostgreSQL ──────────────────────────────────────────

fn database_url() -> Option<String> {
    let url = ["ONTO_TEST_DATABASE_URL", "DATABASE_URL"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|u| !u.trim().is_empty()));
    if url.is_none() {
        eprintln!("skipped: set DATABASE_URL (or ONTO_TEST_DATABASE_URL) to run the PostgreSQL CLI test");
    }
    url
}

/// Drops the tenant schema when the test ends, pass or fail.
struct TenantGuard {
    url: String,
    tenant: String,
}

impl Drop for TenantGuard {
    fn drop(&mut self) {
        use onto_intelli::core::store::{PgOptions, PgStore};
        let opts = PgOptions { schema_prefix: "onto_".into(), listen: false, ..Default::default() };
        if let Ok(store) = PgStore::connect(&self.url, &self.tenant, opts) {
            let _ = store.drop_tenant();
        }
    }
}

#[test]
fn pack_install_sample_then_certify_then_cert_check_on_postgres() {
    let Some(url) = database_url() else { return };
    let tenant = format!("cli_assure_{}", &onto_intelli::core::time::new_id("x")[2..12]).to_lowercase();
    let _guard = TenantGuard { url: url.clone(), tenant: tenant.clone() };
    let dir = scratch("pg");
    let cfg = dir.join("onto-intelli.toml");
    std::fs::write(
        &cfg,
        format!(
            "[engine]\nstorage = \"postgres\"\ndefault_workspace = \"acme\"\n\n[engine.postgres]\ntenant = \"{tenant}\"\nschema_prefix = \"onto_\"\nmax_connections = 2\n"
        ),
    )
    .unwrap();
    let cfg = cfg.to_str().unwrap();
    let env = [("DATABASE_URL", url.as_str())];

    let out = onto_env(&["--config", cfg, "pack", "install", "manufacturing", "--sample", "--json"], &env);
    if !dispatched(&out, "pack.install") {
        return;
    }
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let r = out.json();
    assert!(r["sample_entities"].as_u64().unwrap_or(0) > 0, "{r}");

    let certs = dir.join("cert");
    let out = onto_env(&["--config", cfg, "reason", "certify", "--out", certs.to_str().unwrap()], &env);
    if !dispatched(&out, "reason.certify") {
        return;
    }
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    assert!(out.stdout.contains("entailed_under_supplied_rules"), "{}", out.stdout);

    let check = onto(&["cert", "check", certs.to_str().unwrap()]);
    let r = check.json();
    assert_eq!((check.code, r["verdict"].as_str()), (0, Some("entailed_under_supplied_rules")), "{r}");
    assert!(r["rules_cited"]["supplied"].as_u64().unwrap() > 0, "{r}");

    let out = onto_env(&["--config", cfg, "reason", "binding", certs.to_str().unwrap(), "--json"], &env);
    if dispatched(&out, "reason.binding") {
        assert_eq!(out.code, 0, "{}", out.stderr);
        assert_eq!(out.json()["verdict"], "same_graph", "{}", out.stdout);
    }
}

#[test]
fn a_closed_pipe_ends_the_command_quietly() {
    // `onto align --pack retail | head -1`: the reader goes away early.
    let cfg = memory_config();
    let mut child = Command::new(env!("CARGO_BIN_EXE_onto"))
        .args(["--config", cfg.to_str().unwrap(), "align", "--pack", "retail"])
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run onto");
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked") && !stderr.contains("Broken pipe"), "{stderr}");
    assert!(matches!(out.status.code(), Some(0 | 141)), "{:?} {stderr}", out.status);
}

#[test]
fn plan_consequences_say_asserted_or_derived() {
    let cfg = memory_config();
    let cfg = cfg.to_str().unwrap();
    let json = onto(&["--config", cfg, "pack", "plan", "manufacturing", "--json"]);
    if !dispatched(&json, "pack.plan") {
        return;
    }
    let plan = json.json();
    let c = &plan["consequences"];
    let added = c["added"].as_u64().unwrap();
    let asserted = c["by_rule"]["asserted"].as_u64().unwrap_or(0);
    let out = onto(&["--config", cfg, "pack", "plan", "manufacturing"]);
    let line = out.stdout.lines().find(|l| l.contains("consequences:")).unwrap_or_else(|| panic!("{}", out.stdout));
    let expected = match (asserted, added - asserted) {
        (0, d) => format!("+{d} derived"),
        (a, 0) => format!("+{a} asserted"),
        (a, d) => format!("+{added} added ({a} asserted, {d} derived)"),
    };
    assert!(line.contains(&expected), "{line} (expected {expected}) {c}");
}
