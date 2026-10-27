//! `onto watch`: one page of the change feed through `changes.poll`
//! (`--once`), or the feed followed live, in process or from a server over
//! SSE. The in-memory runs are one process each (an empty store with its
//! schema bootstrap); the PostgreSQL run seeds a tenant of its own in one
//! process and reads its changes from others (skipped without
//! `DATABASE_URL`); the remote run follows a server started in the test.

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

fn scratch(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "onto-cli-watch-{name}-{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn memory_config() -> PathBuf {
    let path = scratch("cfg").join("onto-intelli.toml");
    std::fs::write(&path, "[engine]\nstorage = \"memory\"\ndefault_workspace = \"acme\"\n").unwrap();
    path
}

#[test]
fn watch_once_prints_a_page_and_the_cursor_to_resume_from() {
    let cfg = memory_config();
    let cfg = cfg.to_str().unwrap();
    let out = onto_env(&["--config", cfg, "watch", "--once", "--json"], &[]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let page = out.json();
    assert_eq!(page["expired"], false, "{page}");
    assert!(page["next"].as_str().is_some_and(|n| n.contains('.')), "{page}");

    let out = onto_env(&["--config", cfg, "watch", "--once", "--from", "now"], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("No changes you may see"), "{}", out.stdout);
    assert!(out.stdout.contains("next:") && out.stdout.contains("resume with --after"), "{}", out.stdout);

    // Live: the schema bootstrap's record from the start, then stop after one.
    let out = onto_env(&["--config", cfg, "watch", "--from", "earliest", "--count", "1", "--json"], &[]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let line: Value = serde_json::from_str(out.stdout.trim()).unwrap_or_else(|e| panic!("{e}: {}", out.stdout));
    assert_eq!(line["op"], "schema.change", "{line}");
    let out = onto_env(&["--config", cfg, "watch", "--once", "--count", "1"], &[]);
    assert_eq!(out.code, 2, "--count is for live following: {}", out.stderr);
    // Bad input is refused by the engine or by the parser.
    let out = onto_env(&["--config", cfg, "watch", "--once", "--after", "soon"], &[]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("<seq>.<n>"), "{}", out.stderr);
    let out = onto_env(&["--config", cfg, "watch", "--once", "--after", "1.0", "--from", "now"], &[]);
    assert_eq!(out.code, 2, "clap refuses --after with --from: {}", out.stderr);
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
fn watch_once_reads_what_another_process_wrote_on_postgres() {
    let Some(url) = database_url() else { return };
    let tenant = format!("cli_watch_{}", &onto_intelli::core::time::new_id("x")[2..12]).to_lowercase();
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

    let out = onto_env(&["--config", cfg, "watch", "--once", "--from", "now", "--json"], &env);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let start = out.json()["next"].as_str().unwrap().to_string();
    let out = onto_env(&["--config", cfg, "seed"], &env);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);

    let out = onto_env(
        &[
            "--config",
            cfg,
            "watch",
            "--once",
            "--after",
            &start,
            "--limit",
            "1000",
            "--kinds",
            "project",
            "--ops",
            "entity.create",
            "--json",
        ],
        &env,
    );
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let page = out.json();
    let ids: Vec<&str> = page["changes"].as_array().unwrap().iter().filter_map(|c| c["id"].as_str()).collect();
    assert!(ids.contains(&"project:alpha"), "{page}");
    assert!(page["changes"][0]["entity"].is_object(), "full documents by default: {page}");

    let text = onto_env(
        &[
            "--config",
            cfg,
            "watch",
            "--once",
            "--after",
            &start,
            "--limit",
            "1000",
            "--kinds",
            "project",
            "--ops",
            "entity.create",
        ],
        &env,
    );
    assert_eq!(text.code, 0, "{}", text.stderr);
    assert!(text.stdout.contains("entity.create") && text.stdout.contains("project:alpha"), "{}", text.stdout);
    let next = page["next"].as_str().unwrap();
    assert!(text.stdout.contains(&format!("next: {next}")), "{}", text.stdout);
}

/// `onto --remote … watch` follows a server's SSE stream: records written
/// after it starts, in order, until `--count`.
#[test]
fn watch_follows_a_server_live() {
    const TOKEN: &str = "operator-token-for-watch-tests";
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (base, _stop) = rt.block_on(async {
        let mut cfg = onto_intelli::Config::in_memory();
        cfg.engine.default_workspace = "acme".into();
        cfg.auth.tokens = vec![onto_intelli::config::TokenConfig {
            token: Some(TOKEN.into()),
            token_env: None,
            principal: onto_intelli::Principal::admin("ops"),
            impersonate: false,
            impersonate_roles: Vec::new(),
            tenants: vec!["*".into()],
        }];
        let hub = onto_intelli::Hub::open(cfg).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(onto_grpc::serve_hub_on(hub, onto_grpc::ServeOptions::default(), listener, async {
            let _ = rx.await;
        }));
        (format!("http://{addr}"), tx)
    });
    let watcher = {
        let base = base.clone();
        std::thread::spawn(move || {
            onto_env(
                &[
                    "--remote",
                    &base,
                    "--token",
                    TOKEN,
                    "watch",
                    "--kinds",
                    "task",
                    "--ops",
                    "entity.create",
                    "--count",
                    "2",
                    "--json",
                ],
                &[],
            )
        })
    };
    // Write until the watcher has seen two creates (it may connect after the first writes).
    rt.block_on(async {
        let client = reqwest::Client::new();
        for n in 0..200 {
            if watcher.is_finished() {
                break;
            }
            let r = client
                .post(format!("{base}/v1/call/entity.upsert"))
                .bearer_auth(TOKEN)
                .json(&serde_json::json!({ "id": format!("task:w{n}"), "title": "watched" }))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });
    let out = watcher.join().unwrap();
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let lines: Vec<Value> = out.stdout.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2, "{}", out.stdout);
    assert!(lines.iter().all(|l| l["op"] == "entity.create" && l["kind"] == "task"), "{}", out.stdout);
    let cursor = |l: &Value| -> (u64, u32) {
        let (a, b) = l["cursor"].as_str().unwrap().split_once('.').unwrap();
        (a.parse().unwrap(), b.parse().unwrap())
    };
    assert!(cursor(&lines[0]) < cursor(&lines[1]), "in cursor order: {}", out.stdout);
}
