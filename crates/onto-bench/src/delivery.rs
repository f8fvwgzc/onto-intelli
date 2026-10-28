//! Webhook delivery to a local receiver (docs/design/EVENTS.md section 12):
//! for each endpoint count, a fresh tenant with that many endpoints on one
//! HTTP receiver on 127.0.0.1, each subscribed to `entity.create` of tasks,
//! so every new task is delivered once per endpoint. Two runs per count:
//!
//! - idle: one task per commit, waiting until every endpoint has received
//!   it before the next commit; the latency of one delivery with nothing
//!   queued.
//! - burst: `--rows` deliveries' worth of tasks written in commits of
//!   [`BATCH`] back to back; the latency of each delivery while the backlog
//!   drains, and the rows per second from the first commit to the last
//!   delivery received.
//!
//! Latency is from the moment the commit returns to the moment the receiver
//! has read the whole request. Deliveries are sent with the default
//! `[webhooks]` limits (4 requests in flight per endpoint, 8 per tenant)
//! unless `--in-flight-endpoint` or `--in-flight-tenant` say otherwise.
//! The receiver answers 200 with an empty body and checks nothing (no
//! signature verification): it stands in for a fast receiver.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use onto_intelli::config::AllowConfig;
use onto_intelli::core::store::{PgOptions, PgShared, Store};
use onto_intelli::core::{EntityInput, Mutation, Principal};
use onto_intelli::{ActionContext, Config, Intelli};

use super::{Args, Stat};

/// Tasks per commit in the burst run.
pub(crate) const BATCH: usize = 100;
/// Commits before the idle run is timed (connections, plan caches).
const WARMUP: usize = 10;
/// The signing secret the endpoints use (the Standard Webhooks test vector;
/// the receiver does not verify it).
const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

// ── the receiver ───────────────────────────────────────────────────────

/// One request the receiver read: its path, the CloudEvent `subject` (the
/// entity id) and when its body was complete.
#[derive(Clone, Debug)]
pub(crate) struct Receipt {
    pub(crate) path: String,
    pub(crate) subject: String,
    pub(crate) at: Instant,
}

#[derive(Default)]
struct Shared {
    receipts: Mutex<Vec<Receipt>>,
    count: AtomicUsize,
    notify: Notify,
}

/// A minimal HTTP/1.1 receiver on 127.0.0.1: keep-alive, `Content-Length`
/// bodies, every request answered 200 with an empty body.
pub(crate) struct Receiver {
    shared: Arc<Shared>,
    port: u16,
    accept: JoinHandle<()>,
}

impl Receiver {
    pub(crate) async fn start() -> std::io::Result<Receiver> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let _ = stream.set_nodelay(true);
                tokio::spawn(serve(stream, s.clone()));
            }
        });
        Ok(Receiver { shared, port, accept })
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Requests read so far.
    pub(crate) fn count(&self) -> usize {
        self.shared.count.load(Ordering::Acquire)
    }

    /// Wait until at least `n` requests were read, `limit` at most.
    pub(crate) async fn wait_for(&self, n: usize, limit: Duration) -> Result<(), String> {
        let deadline = Instant::now() + limit;
        loop {
            // Registered before the check, so no wake-up is lost between them.
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.count() >= n {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "the receiver read {} of {n} requests in {:.0} s",
                    self.count(),
                    limit.as_secs_f64()
                ));
            }
            let _ = tokio::time::timeout(left, notified).await;
        }
    }

    /// Take the receipts so far and start counting from zero.
    pub(crate) fn take(&self) -> Vec<Receipt> {
        let mut r = self.shared.receipts.lock().expect("receipts lock");
        self.shared.count.store(0, Ordering::Release);
        std::mem::take(&mut *r)
    }

    /// Stop accepting connections (open ones end when their client closes).
    pub(crate) fn stop(&self) {
        self.accept.abort();
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
const BAD: &[u8] = b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

/// A complete request at the start of a buffer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Request {
    /// Bytes of head and body together.
    pub(crate) len: usize,
    pub(crate) path: String,
    pub(crate) body: Range<usize>,
}

/// The first request in `buf`: `None` while it is incomplete, an error for
/// a request this receiver does not read (no `Content-Length`, chunked).
pub(crate) fn parse(buf: &[u8]) -> Option<Result<Request, String>> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..end]);
    let mut lines = head.split("\r\n");
    let path = match lines.next().map(|l| l.split(' ').collect::<Vec<_>>()) {
        Some(parts) if parts.len() == 3 => parts[1].to_string(),
        _ => return Some(Err("a request line is METHOD PATH VERSION".into())),
    };
    let mut len = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let k = k.trim().to_ascii_lowercase();
        if k == "transfer-encoding" {
            return Some(Err("chunked bodies are not read".into()));
        }
        if k == "content-length" {
            match v.trim().parse::<usize>() {
                Ok(n) => len = Some(n),
                Err(_) => return Some(Err("content-length is not a number".into())),
            }
        }
    }
    let start = end + 4;
    let body = start..start + len.unwrap_or(0);
    if buf.len() < body.end {
        return None;
    }
    Some(Ok(Request { len: body.end, path, body }))
}

async fn serve(mut stream: TcpStream, shared: Arc<Shared>) {
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        match parse(&buf) {
            Some(Ok(req)) => {
                let at = Instant::now();
                let subject = serde_json::from_slice::<Value>(&buf[req.body.clone()])
                    .ok()
                    .and_then(|v| v.get("subject").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default();
                shared.receipts.lock().expect("receipts lock").push(Receipt { path: req.path, subject, at });
                shared.count.fetch_add(1, Ordering::AcqRel);
                shared.notify.notify_waiters();
                buf.drain(..req.len);
                if stream.write_all(OK).await.is_err() {
                    return;
                }
            }
            Some(Err(_)) => {
                let _ = stream.write_all(BAD).await;
                return;
            }
            None => match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            },
        }
    }
}

/// The run (`'l'` idle, `'b'` burst) and number of the task an entity id
/// names: `task:l12` is `('l', 12)`.
pub(crate) fn task_index(subject: &str) -> Option<(char, usize)> {
    let key = subject.rsplit(':').next()?;
    let mut chars = key.chars();
    let run = chars.next()?;
    let n = chars.as_str().parse().ok()?;
    Some((run, n))
}

// ── the scenario ───────────────────────────────────────────────────────

pub(crate) async fn delivery_bench(
    args: &Args,
    shared: &PgShared,
    opts: &PgOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let secret = std::env::temp_dir().join(format!("onto-bench-hook-secret-{}", std::process::id()));
    std::fs::write(&secret, SECRET)?;
    let rx = Receiver::start().await?;
    let mut stats: Vec<Stat> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut result = Ok(());
    for &n in &args.endpoints {
        if let Err(e) = one_count(args, shared, opts, &rx, &secret, n, &mut stats, &mut lines).await {
            result = Err(e);
            break;
        }
    }
    rx.stop();
    let _ = std::fs::remove_file(&secret);
    result?;
    let defaults = onto_intelli::config::WebhooksConfig::default();
    println!(
        "## Webhook delivery to a local receiver ({} idle commits, about {} rows per burst, commits of {BATCH}; \
         in flight: {} per endpoint, {} per tenant)\n",
        args.latency_commits,
        args.rows,
        args.in_flight_endpoint.unwrap_or(defaults.max_in_flight_per_endpoint),
        args.in_flight_tenant.unwrap_or(defaults.max_in_flight_per_tenant),
    );
    println!("| Operation | What | Runs | p50 ms | p99 ms | mean ms | max ms |\n|---|---|---:|---:|---:|---:|---:|");
    for s in &mut stats {
        println!("{}", s.row());
    }
    println!();
    for l in lines {
        println!("{l}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn one_count(
    args: &Args,
    shared: &PgShared,
    opts: &PgOptions,
    rx: &Receiver,
    secret: &std::path::Path,
    n: usize,
    stats: &mut Vec<Stat>,
    lines: &mut Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if n == 0 || n > 50 {
        return Err("--endpoints takes counts from 1 to 50 ([webhooks] max_endpoints)".into());
    }
    let tenant = format!("{}_deliver{n}", args.tenant);
    shared.tenant(&tenant, opts.clone())?.drop_tenant()?;
    let mut cfg = Config::in_memory();
    cfg.engine.storage = "postgres".into();
    cfg.engine.default_workspace = "bench".into();
    cfg.memory.enabled = false;
    cfg.cache.enabled = false;
    cfg.traces.enabled = false;
    cfg.connectors.background = false;
    cfg.automation.workers = false;
    cfg.webhooks.allow = vec![AllowConfig { host: "127.0.0.1".into(), cidrs: vec!["127.0.0.1/32".into()] }];
    if let Some(k) = args.in_flight_tenant {
        cfg.webhooks.max_in_flight_per_tenant = k;
    }
    if let Some(k) = args.in_flight_endpoint {
        cfg.webhooks.max_in_flight_per_endpoint = k;
    }
    let store: Arc<dyn Store> = Arc::new(shared.tenant(&tenant, opts.clone())?);
    let intelli = Intelli::open_with_store(cfg, store.clone(), onto_intelli::llm::build(&Default::default())?).await?;
    let out = measure(args, &intelli, store.as_ref(), rx, secret, n, stats, lines).await;
    // Stop fan-out and delivery (releasing their leases) before the drop.
    intelli.shutdown(Duration::from_secs(5)).await;
    drop(intelli);
    rx.take();
    if !args.keep {
        shared.tenant(&tenant, opts.clone())?.drop_tenant()?;
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn measure(
    args: &Args,
    intelli: &Arc<Intelli>,
    store: &dyn Store,
    rx: &Receiver,
    secret: &std::path::Path,
    n: usize,
    stats: &mut Vec<Stat>,
    lines: &mut Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let onto = intelli.ontology().clone();
    let p = Principal::admin("bench");
    let ctx = ActionContext::new(p.clone(), "bench");
    onto.set_capture(&ctx, true)?;
    for k in 0..n {
        intelli
            .call(
                &p,
                "webhook.add",
                json!({
                    "name": format!("ep{k}"),
                    "url": rx.url(&format!("/n{n}/ep{k}")),
                    "secret": format!("file:{}", secret.display()),
                    "filter": { "kinds": ["task"], "ops": ["entity.create"] },
                    "principal": { "roles": "admin", "workspaces": "*" },
                }),
            )
            .await?;
    }
    rx.take();

    // Idle: one task per commit, each delivered to every endpoint before
    // the next commit.
    let mut returned: Vec<Instant> = Vec::new();
    for i in 0..WARMUP + args.latency_commits {
        let (o, c) = (onto.clone(), ctx.clone());
        let input = EntityInput::new("task", format!("l{i}"), format!("delivery bench {i}"));
        tokio::task::spawn_blocking(move || o.upsert(&c, input)).await??;
        returned.push(Instant::now());
        rx.wait_for((i + 1) * n, Duration::from_secs(60)).await?;
    }
    let mut idle = Stat {
        name: "commit → receiver, idle",
        detail: format!("{n} endpoint(s), one task per commit, nothing queued"),
        samples: Vec::new(),
    };
    for r in rx.take() {
        if let Some(('l', i)) = task_index(&r.subject)
            && i >= WARMUP
        {
            idle.samples.push(r.at.duration_since(returned[i]).as_secs_f64() * 1000.0);
        }
    }
    stats.push(idle);

    // Burst: commits of BATCH back to back, then wait for the drain.
    let tasks = args.rows.div_ceil(n).max(BATCH);
    let rows = tasks * n;
    let started = Instant::now();
    let mut batch_returned: Vec<Instant> = Vec::new();
    let mut i = 0usize;
    while i < tasks {
        let muts: Vec<Mutation> = (i..(i + BATCH).min(tasks))
            .map(|k| Mutation::Upsert(EntityInput::new("task", format!("b{k}"), format!("delivery burst {k}"))))
            .collect();
        i += muts.len();
        let (o, c) = (onto.clone(), ctx.clone());
        tokio::task::spawn_blocking(move || o.apply(&c, muts)).await??;
        batch_returned.push(Instant::now());
    }
    let crawl = started.elapsed();
    rx.wait_for(rows, Duration::from_secs(1800)).await?;
    // The receiver has every request; the worker records the last outcomes
    // just after.
    loop {
        let open: u64 = store.outbox_open_counts()?.iter().map(|(_, c)| c).sum();
        if open == 0 {
            break;
        }
        if started.elapsed() > Duration::from_secs(1800) {
            return Err(format!("{open} outbox rows still open after 30 min").into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let settled = started.elapsed();
    let receipts = rx.take();
    let mut burst = Stat {
        name: "commit → receiver, burst",
        detail: format!("{n} endpoint(s), {tasks} tasks in commits of {BATCH}"),
        samples: Vec::new(),
    };
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut last = started;
    for r in &receipts {
        if let Some(('b', k)) = task_index(&r.subject) {
            burst.samples.push(r.at.duration_since(batch_returned[k / BATCH]).as_secs_f64() * 1000.0);
            last = last.max(r.at);
            seen.insert((r.path.clone(), r.subject.clone()));
        }
    }
    if seen.len() != rows {
        return Err(format!("{n} endpoints: {} distinct deliveries received, {rows} expected", seen.len()).into());
    }
    let drain = last.duration_since(started).as_secs_f64();
    lines.push(format!(
        "- {n} endpoint(s): {tasks} tasks ({rows} rows) committed in {:.1} s; the last row reached the receiver \
         {drain:.1} s after the first commit began: **{:.0} rows/s**; the outbox was empty after {:.1} s; \
         {} repeated deliveries.",
        crawl.as_secs_f64(),
        rows as f64 / drain,
        settled.as_secs_f64(),
        receipts.len() - seen.len(),
    ));
    stats.push(burst);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_requests_only() {
        let req = b"POST /n1/ep0 HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse(&req[..20]), None);
        assert_eq!(parse(&req[..req.len() - 1]), None);
        let r = parse(req).unwrap().unwrap();
        assert_eq!(r, Request { len: req.len(), path: "/n1/ep0".into(), body: req.len() - 5..req.len() });
        // A second request behind the first is left for the next parse.
        let two = [&req[..], &req[..]].concat();
        assert_eq!(parse(&two).unwrap().unwrap().len, req.len());
        // No body.
        let get = b"GET / HTTP/1.1\r\n\r\n";
        assert_eq!(parse(get).unwrap().unwrap().body, get.len()..get.len());
        assert!(parse(b"POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n").unwrap().is_err());
        assert!(parse(b"POST / HTTP/1.1\r\ncontent-length: x\r\n\r\n").unwrap().is_err());
        assert!(parse(b"nonsense\r\n\r\n").unwrap().is_err());
    }

    #[test]
    fn task_indexes() {
        assert_eq!(task_index("task:l12"), Some(('l', 12)));
        assert_eq!(task_index("task:b0"), Some(('b', 0)));
        assert_eq!(task_index("b7"), Some(('b', 7)));
        assert_eq!(task_index("task:x"), None);
        assert_eq!(task_index(""), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn receiver_reads_keep_alive_requests() {
        let rx = Receiver::start().await.unwrap();
        let mut s = TcpStream::connect(("127.0.0.1", rx.port)).await.unwrap();
        for k in 0..3 {
            let body = format!(r#"{{"subject":"task:b{k}"}}"#);
            let req = format!("POST /n1/ep0 HTTP/1.1\r\ncontent-length: {}\r\n\r\n{body}", body.len());
            s.write_all(req.as_bytes()).await.unwrap();
            let mut answer = vec![0u8; OK.len()];
            s.read_exact(&mut answer).await.unwrap();
            assert_eq!(answer, OK);
        }
        rx.wait_for(3, Duration::from_secs(5)).await.unwrap();
        let got = rx.take();
        assert_eq!(got.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(), ["task:b0", "task:b1", "task:b2"]);
        assert!(got.iter().all(|r| r.path == "/n1/ep0"));
        assert_eq!(rx.count(), 0);
        rx.stop();
    }
}
