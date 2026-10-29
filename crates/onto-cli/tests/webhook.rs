//! `onto webhook list|add|rm`: in process on an empty in-memory store (one
//! process per command), and across processes on PostgreSQL (skipped
//! without `DATABASE_URL`): an endpoint added by one process is listed by
//! the next and removed by a third.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

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

fn onto_env(args: &[&str], env: &[(&str, &str)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_onto"));
    cmd.args(args).env("NO_COLOR", "1").stdin(Stdio::null());
    for v in ["ONTO_CONFIG", "ONTO_REMOTE", "ONTO_TOKEN", "ONTO_TENANT", "ONTO_MCP_PROFILE", "ONTO_MASTER_KEY"] {
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

fn scratch(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "onto-cli-webhook-{name}-{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn webhook_add_list_and_rm_in_process() {
    let cfg = scratch("cfg").join("onto-intelli.toml");
    std::fs::write(&cfg, "[engine]\nstorage = \"memory\"\ndefault_workspace = \"acme\"\n").unwrap();
    let cfg = cfg.to_str().unwrap();
    let out = onto_env(&["--config", cfg, "webhook", "list"], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("No webhook endpoints"), "{}", out.stdout);

    let args = [
        "--config",
        cfg,
        "webhook",
        "add",
        "ops",
        "https://hooks.example.com/in",
        "--secret",
        "env:ONTO_CLI_TEST_HOOK_SECRET",
        "--kind",
        "issue",
        "--role",
        "viewer",
        "--sees",
        "acme",
        "--json",
    ];
    let out = onto_env(&args, &[]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let r = out.json();
    assert_eq!(r["endpoint"]["filter"]["kinds"], serde_json::json!(["issue"]), "{r}");
    assert_eq!(r["endpoint"]["secret"], "env:ONTO_CLI_TEST_HOOK_SECRET");
    assert!(r["note"].as_str().unwrap().contains("delivered at least once"), "{r}");

    // Without a master key a generated secret cannot be sealed: the error says what to do.
    let out = onto_env(&["--config", cfg, "webhook", "add", "gen", "https://hooks.example.com/in"], &[]);
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("env:VAR"), "{}", out.stderr);
    // An unknown endpoint is not found.
    let out = onto_env(&["--config", cfg, "webhook", "rm", "nope"], &[]);
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("not found") || out.stderr.contains("nope"), "{}", out.stderr);
}

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
fn webhook_endpoints_persist_across_processes_on_postgres() {
    let Some(url) = database_url() else { return };
    let tenant = format!("cli_hook_{}", &onto_intelli::core::time::new_id("x")[2..12]).to_lowercase();
    let _guard = TenantGuard { url: url.clone(), tenant: tenant.clone() };
    let cfg = scratch("pg").join("onto-intelli.toml");
    std::fs::write(
        &cfg,
        format!(
            "[engine]\nstorage = \"postgres\"\ndefault_workspace = \"acme\"\n\n[engine.postgres]\ntenant = \"{tenant}\"\nschema_prefix = \"onto_\"\nmax_connections = 2\n"
        ),
    )
    .unwrap();
    let cfg = cfg.to_str().unwrap();
    let env = [("DATABASE_URL", url.as_str())];
    let add = [
        "--config",
        cfg,
        "webhook",
        "add",
        "ops",
        "https://hooks.example.com/in?token=abc",
        "--secret",
        "env:ONTO_CLI_TEST_HOOK_SECRET",
        "--all",
    ];
    let out = onto_env(&add, &env);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    assert!(out.stdout.contains("Added ops"), "{}", out.stdout);
    let out = onto_env(&["--config", cfg, "webhook", "list"], &env);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("ops") && out.stdout.contains("all changes"), "{}", out.stdout);
    let out = onto_env(&["--config", cfg, "webhook", "list", "--json"], &env);
    assert_eq!(out.json()["items"][0]["filter"]["include_migrations"], true);
    let out = onto_env(&["--config", cfg, "webhook", "rm", "ops"], &env);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let out = onto_env(&["--config", cfg, "webhook", "list"], &env);
    assert!(out.stdout.contains("No webhook endpoints"), "{}", out.stdout);
}
