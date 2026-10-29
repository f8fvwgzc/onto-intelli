//! `onto automation list|runs|enable|disable`: in process on an empty
//! in-memory store, and across processes on PostgreSQL (skipped without
//! `DATABASE_URL`): an automation defined by one process (`onto call
//! schema.apply`) is enabled by the next, listed active by a third and
//! disabled by a fourth. One-off commands run no automation consumer (`onto
//! serve` does), so a change made by a command starts no run there.

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
        "onto-cli-automation-{name}-{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const DEFINE: &str = r#"{"changes": [{"op": "put_automation", "name": "resolve_blocked", "def": {
    "run_as": {"roles": ["member"], "workspaces": ["acme"]},
    "trigger": {"on": "change", "kinds": ["issue"], "ops": ["entity.update"], "state": {"to": ["blocked"]}},
    "effects": [{"action": {"type": "resolve", "params": {"issue": "{trigger.id}"}}}]}}]}"#;

#[test]
fn automation_commands_in_process() {
    let cfg = scratch("cfg").join("onto-intelli.toml");
    std::fs::write(&cfg, "[engine]\nstorage = \"memory\"\ndefault_workspace = \"acme\"\n").unwrap();
    let cfg = cfg.to_str().unwrap();
    let out = onto_env(&["--config", cfg, "automation", "list"], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("No automations"), "{}", out.stdout);
    let out = onto_env(&["--config", cfg, "automation", "list", "--json"], &[]);
    assert_eq!(out.json()["kill"], false);
    let out = onto_env(&["--config", cfg, "automation", "runs"], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("No runs"), "{}", out.stdout);
    let out = onto_env(&["--config", cfg, "automation", "enable", "nope"], &[]);
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("automation nope"), "{}", out.stderr);
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
fn automations_are_enabled_across_processes_on_postgres() {
    let Some(url) = database_url() else { return };
    let tenant = format!("cli_auto_{}", &onto_intelli::core::time::new_id("x")[2..12]).to_lowercase();
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
    let out = onto_env(&["--config", cfg, "call", "schema.apply", DEFINE], &env);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let out = onto_env(&["--config", cfg, "automation", "enable", "resolve_blocked"], &env);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    assert!(
        out.stdout.contains("Enabled resolve_blocked") && out.stdout.contains("expires after 30 days"),
        "{}",
        out.stdout
    );
    let out = onto_env(&["--config", cfg, "automation", "list"], &env);
    assert!(out.stdout.contains("resolve_blocked  active"), "{}", out.stdout);
    let out = onto_env(&["--config", cfg, "automation", "list", "--json"], &env);
    assert_eq!(out.json()["items"][0]["enabled_by"], "local");
    // A command runs no consumer: the change waits for `onto serve`.
    let upsert = r#"{"id": "issue:1", "title": "One", "state": "blocked"}"#;
    let out = onto_env(&["--config", cfg, "call", "entity.upsert", upsert], &env);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let out = onto_env(&["--config", cfg, "automation", "runs", "resolve_blocked"], &env);
    assert!(out.stdout.contains("No runs"), "{}", out.stdout);
    let out = onto_env(&["--config", cfg, "automation", "disable", "resolve_blocked"], &env);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let out = onto_env(&["--config", cfg, "automation", "list"], &env);
    assert!(out.stdout.contains("inactive (not_enabled)"), "{}", out.stdout);
}
