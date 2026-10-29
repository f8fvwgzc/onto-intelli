//! `onto serve` lifecycle, against the built binary on a free loopback port:
//! a configuration error (a reserved principal id) fails before anything
//! says it is listening; SIGTERM (Unix) shuts down gracefully: open change
//! streams get their final `end` frame, the process exits 0, and on
//! PostgreSQL (skipped without `DATABASE_URL`) the fan-out lease it held is
//! released, so a second `onto serve` takes it at once instead of after the
//! 60 s lease.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn scratch(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "onto-cli-serve-{name}-{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn onto(args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_onto"));
    cmd.args(args).env("NO_COLOR", "1").stdin(Stdio::null());
    for v in ["ONTO_CONFIG", "ONTO_REMOTE", "ONTO_TOKEN", "ONTO_TENANT", "ONTO_MCP_PROFILE", "RUST_LOG"] {
        cmd.env_remove(v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

/// A running `onto serve` whose stderr is collected line by line. Killed
/// when dropped (a failed test leaves no server behind).
struct Server {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(mut cmd: Command) -> Self {
        let mut child = cmd.stdout(Stdio::null()).stderr(Stdio::piped()).spawn().expect("run onto serve");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let (stderr, sink) = (child.stderr.take().unwrap(), lines.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(std::result::Result::ok) {
                sink.lock().unwrap().push(line);
            }
        });
        Self { child, lines }
    }

    fn stderr(&self) -> String {
        self.lines.lock().unwrap().join("\n")
    }

    /// Wait until stderr has a line containing `what` (or the process ends).
    #[cfg(unix)]
    fn wait_for(&mut self, what: &str, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if self.lines.lock().unwrap().iter().any(|l| l.contains(what)) {
                return true;
            }
            if self.child.try_wait().unwrap().is_some() {
                std::thread::sleep(Duration::from_millis(100));
                return self.lines.lock().unwrap().iter().any(|l| l.contains(what));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// The exit code once the process ended, within `secs`.
    fn exit(&mut self, secs: u64) -> Option<i32> {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Some(st) = self.child.try_wait().unwrap() {
                // Let the reader thread take the last lines.
                std::thread::sleep(Duration::from_millis(200));
                return Some(st.code().unwrap_or(-1));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    #[cfg(unix)]
    fn sigterm(&self) {
        let st = Command::new("kill").args(["-TERM", &self.child.id().to_string()]).status().expect("kill -TERM");
        assert!(st.success());
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn a_reserved_principal_id_fails_before_the_listening_banner() {
    let cfg = scratch("bad").join("onto-intelli.toml");
    std::fs::write(
        &cfg,
        "[engine]\nstorage = \"memory\"\n\n[[auth.tokens]]\ntoken = \"a-long-enough-token-0123456789\"\n\
         principal = { id = \"automation:payroll\", roles = [\"admin\"], workspaces = [\"*\"] }\n",
    )
    .unwrap();
    let port = free_port().to_string();
    let listen = format!("127.0.0.1:{port}");
    let mut s = Server::start(onto(&["--config", cfg.to_str().unwrap(), "serve", "--listen", &listen], &[]));
    let code = s.exit(60).expect("onto serve exits on a configuration error");
    let err = s.stderr();
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("reserved service principal id"), "{err}");
    assert!(!err.contains("listening"), "nothing says it listens: {err}");
    assert!(!err.contains("gRPC + HTTP on"), "no banner: {err}");
    // Nothing listens on the port either.
    assert!(std::net::TcpStream::connect(&listen).is_err());
}

/// Open a change stream and read it until the server closes it.
#[cfg(unix)]
fn stream(port: u16) -> std::thread::JoinHandle<String> {
    use std::io::{Read, Write};
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    write!(
        c,
        "GET /v1/changes/stream HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    std::thread::spawn(move || {
        let mut body = Vec::new();
        let _ = c.read_to_end(&mut body);
        String::from_utf8_lossy(&body).into_owned()
    })
}

#[cfg(unix)]
#[test]
fn sigterm_ends_streams_and_exits_cleanly() {
    let cfg = scratch("mem").join("onto-intelli.toml");
    std::fs::write(&cfg, "[engine]\nstorage = \"memory\"\n").unwrap();
    let port = free_port();
    let listen = format!("127.0.0.1:{port}");
    let mut s = Server::start(onto(&["--config", cfg.to_str().unwrap(), "serve", "--dev", "--listen", &listen], &[]));
    assert!(s.wait_for("listening", 120), "{}", s.stderr());
    let sse = stream(port);
    std::thread::sleep(Duration::from_millis(500));
    let asked = Instant::now();
    s.sigterm();
    let code = s.exit(30).expect("onto serve exits after SIGTERM");
    let err = s.stderr();
    assert_eq!(code, 0, "a clean exit: {err}");
    assert!(asked.elapsed() < Duration::from_secs(20), "well within the grace: {:?}", asked.elapsed());
    assert!(err.contains("shutting down") && err.contains("stopped · released"), "{err}");
    let body = sse.join().unwrap();
    assert!(body.contains("event: end") && body.contains("tenant_closed"), "the stream's final frame: {body}");
    assert!(body.contains("the server is shutting down"), "{body}");
}

#[cfg(unix)]
fn database_url() -> Option<String> {
    let url = ["ONTO_TEST_DATABASE_URL", "DATABASE_URL"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|u| !u.trim().is_empty()));
    if url.is_none() {
        eprintln!("skipped: set DATABASE_URL (or ONTO_TEST_DATABASE_URL) to run the PostgreSQL serve test");
    }
    url
}

/// Drops the tenant schema when the test ends, pass or fail.
#[cfg(unix)]
struct TenantGuard {
    url: String,
    tenant: String,
}

#[cfg(unix)]
impl Drop for TenantGuard {
    fn drop(&mut self) {
        if let Ok(store) = pg_store(&self.url, &self.tenant) {
            let _ = store.drop_tenant();
        }
    }
}

#[cfg(unix)]
fn pg_store(url: &str, tenant: &str) -> onto_intelli::Result<onto_intelli::core::store::PgStore> {
    use onto_intelli::core::store::{PgOptions, PgStore};
    PgStore::connect(url, tenant, PgOptions { schema_prefix: "onto_".into(), listen: false, ..Default::default() })
}

#[cfg(unix)]
#[test]
fn sigterm_releases_the_fan_out_lease_and_a_second_server_takes_it_at_once_on_postgres() {
    use onto_intelli::core::store::Store;
    use onto_intelli::webhooks::FANOUT_LEASE;

    let Some(url) = database_url() else { return };
    let tenant = format!("cli_life_{}", &onto_intelli::core::time::new_id("x")[2..12]).to_lowercase();
    let _guard = TenantGuard { url: url.clone(), tenant: tenant.clone() };
    let cfg = scratch("pg").join("onto-intelli.toml");
    std::fs::write(
        &cfg,
        format!(
            "[engine]\nstorage = \"postgres\"\ndefault_workspace = \"acme\"\n\n[engine.postgres]\ntenant = \"{tenant}\"\n\
             schema_prefix = \"onto_\"\nmax_connections = 4\n"
        ),
    )
    .unwrap();
    let cfg = cfg.to_str().unwrap();
    let env = [("DATABASE_URL", url.as_str()), ("ONTO_CLI_TEST_HOOK_SECRET", "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw")];
    // A change subscription: the serving process's fan-out takes the lease.
    let add = [
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
    ];
    let out = onto(&add, &env).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let store = pg_store(&url, &tenant).unwrap();
    let holder_now = || store.lease_get(FANOUT_LEASE).unwrap().filter(|l| l.held);

    let serve = |port: u16| {
        let listen = format!("127.0.0.1:{port}");
        Server::start(onto(&["--config", cfg, "serve", "--dev", "--listen", &listen], &env))
    };
    let mut first = serve(free_port());
    assert!(first.wait_for("listening", 120), "{}", first.stderr());
    let end = Instant::now() + Duration::from_secs(20);
    while holder_now().is_none() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
    }
    let lease = holder_now().unwrap_or_else(|| panic!("the first server fans out: {}", first.stderr()));
    first.sigterm();
    assert_eq!(first.exit(30), Some(0), "{}", first.stderr());
    assert!(first.stderr().contains("released 1 lease,"), "{}", first.stderr());
    assert!(holder_now().is_none(), "released on SIGTERM, not left to expire");

    let mut second = serve(free_port());
    assert!(second.wait_for("listening", 120), "{}", second.stderr());
    let started = Instant::now();
    while holder_now().is_none() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let next = holder_now().unwrap_or_else(|| panic!("the second server fans out: {}", second.stderr()));
    assert!(started.elapsed() < Duration::from_secs(10), "at once, not after the 60 s lease");
    assert!(next.holder != lease.holder && next.epoch == lease.epoch + 1, "{lease:?} then {next:?}");
    second.sigterm();
    assert_eq!(second.exit(30), Some(0), "{}", second.stderr());
}
