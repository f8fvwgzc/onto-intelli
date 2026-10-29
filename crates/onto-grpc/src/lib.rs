//! # onto-grpc
//!
//! One port serves:
//! * the built-in `onto_intelli.v1` gRPC API,
//! * **your own proto files**, compiled at startup and mapped onto engine
//!   actions with `[[grpc.routes]]` (no code generation, no rebuild),
//! * gRPC reflection (grpcurl, Postman) and `grpc.health.v1`,
//! * JSON over HTTP/1.1 (`POST /v1/call/{action}`, `/v1/ask`, SSE streaming),
//! * MCP over HTTP (`POST /mcp`, the Streamable HTTP transport with JSON responses),
//! * traces, sessions and metrics (`/v1/traces`, `/v1/sessions`, `/v1/metrics`,
//!   `ObservabilityService`), with `traceparent` and `x-onto-session-id` on every call,
//! * with a control plane ([`ServeOptions::control`]), the console API
//!   (`/v1/console/…`), project API keys, and each project's own LLM
//!   provider, limits and usage meter (installed as the hub's tenant hooks).
//!
//! A typed Rust client for the built-in API is in [`pb`].

pub mod auth;
pub mod console;
pub mod dynamic;
pub mod http;
pub mod mcp_http;
pub mod observe;
pub mod routes;
pub mod template;
pub mod ui;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tonic::server::NamedService;

use onto_control::Control;
use onto_intelli::{Error, Hub, Intelli, Result};

pub use routes::{BUILTIN_DESCRIPTOR, RouteInfo, RouteTable};

/// Generated client and message types for `onto_intelli.v1`.
pub mod pb {
    #![allow(clippy::all, missing_docs)]
    tonic::include_proto!("onto_intelli.v1");
}

/// The built-in proto source, for clients in other languages.
pub const BUILTIN_PROTO: &str = include_str!("../proto/onto_intelli/v1/onto_intelli.proto");

fn named<S: NamedService>(_: &S) -> String {
    format!("/{}/{{*rest}}", S::NAME)
}

/// How to serve.
#[derive(Clone, Debug, Default)]
pub struct ServeOptions {
    /// Development mode: no authentication, every caller is `auth.dev_principal`
    /// (only on loopback addresses unless `ONTO_DEV_ALLOW_REMOTE=1`).
    pub dev: bool,
    /// The control plane (`[console] enabled`): console routes, console
    /// sessions and project API keys.
    pub control: Option<Arc<Control>>,
}

/// The KV namespace of revoked session and login ids, in the default tenant.
pub const REVOKED_NS: &str = "auth.revoked";

/// Revoked session and login ids in the default tenant's KV store, whatever
/// the sessions' tenants, kept until the sessions would have expired: every
/// replica sees them. A revocation for every tenant is keyed by the id; one
/// for the sessions of some tenants (a service token limited to them) by
/// `<id>@<tenant>`, once per tenant. Expired rows are deleted at most every
/// [`PRUNE_EVERY_MS`], after a revoke that finds them due, on a task of
/// their own and [`PRUNE_BATCH`] rows at a time.
///
/// Session authentication in every tenant therefore reads the default
/// tenant. That is kept on purpose: every tenant lives in the same database,
/// so the default tenant is as reachable as the session's own; a failed read
/// refuses the session (503) rather than accepting it; and reads are cached
/// ([`auth::REVOCATION_TTL`]): two primary-key reads (the id, the id for
/// the session's tenant) per session id at most every 10 seconds.
struct HubRevocations {
    hub: std::sync::Weak<Hub>,
    /// When expired rows were last deleted (Unix ms).
    pruned: std::sync::atomic::AtomicI64,
}

/// How often a revoke deletes the expired revocations.
const PRUNE_EVERY_MS: i64 = 10 * 60 * 1000;

/// Expired revocations deleted per statement.
const PRUNE_BATCH: usize = 1_000;

/// How long past its expiry a revocation is kept before it is deleted: a
/// replica whose clock is ahead must not delete rows the others still
/// read as live (reads skip expired rows anyway).
const PRUNE_MARGIN_MS: i64 = 60 * 60 * 1000;

/// Delete expired revocations with `purge(limit)`, [`PRUNE_BATCH`] rows a
/// statement, until a statement deletes fewer; how many statements ran.
fn purge_in_batches(purge: impl Fn(usize) -> Result<u64>) -> usize {
    let mut statements = 0;
    loop {
        statements += 1;
        match purge(PRUNE_BATCH) {
            Ok(n) if n as usize >= PRUNE_BATCH => continue,
            Ok(_) => return statements,
            Err(e) => {
                tracing::warn!("deleting expired session revocations: {e}");
                return statements;
            }
        }
    }
}

/// Record `key` revoked until `exp` (Unix seconds), never shortening a
/// revocation already there: revoking an id without an expiry again, after
/// the limit was lowered, keeps the later of the two.
fn write_revocation(store: &dyn onto_intelli::core::store::Store, key: &str, exp: u64) -> Result<()> {
    let kept = store.kv_get(REVOKED_NS, key)?.and_then(|v| v.get("expires_at").and_then(serde_json::Value::as_u64));
    let exp = kept.map_or(exp, |kept| kept.max(exp));
    let until = i64::try_from(exp).unwrap_or(i64::MAX / 1000).saturating_mul(1000);
    store.kv_put(REVOKED_NS, key, &serde_json::json!({ "expires_at": exp }), Some(until))
}

impl HubRevocations {
    fn new(hub: &Arc<Hub>) -> Self {
        Self { hub: Arc::downgrade(hub), pruned: std::sync::atomic::AtomicI64::new(0) }
    }

    /// The default tenant's: one place for every tenant's sessions, so a
    /// revocation never depends on the tenant the caller happened to name.
    async fn store(&self) -> Result<Arc<dyn onto_intelli::core::store::Store>> {
        let hub = self.hub.upgrade().ok_or_else(|| Error::Unavailable("the server is shutting down".into()))?;
        let intelli = hub.resolve(None).await?;
        Ok(intelli.ontology().store().clone())
    }

    /// Whether this revoke deletes the expired rows: once per [`PRUNE_EVERY_MS`].
    fn prune_due(&self) -> bool {
        use std::sync::atomic::Ordering;
        let now = onto_intelli::core::time::now_ms();
        let last = self.pruned.load(Ordering::Relaxed);
        now - last >= PRUNE_EVERY_MS
            && self.pruned.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok()
    }
}

impl auth::Revocations for HubRevocations {
    fn revoke<'a>(
        &'a self,
        id: &'a str,
        tenants: Option<&'a [String]>,
        exp: u64,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let store = self.store().await?;
            let keys: Vec<String> = match tenants {
                None => vec![id.to_string()],
                Some(tenants) => tenants.iter().map(|t| auth::scoped_id(id, Some(t))).collect(),
            };
            let written = store.clone();
            tokio::task::spawn_blocking(move || keys.iter().try_for_each(|key| write_revocation(&*written, key, exp)))
                .await
                .map_err(|e| Error::Other(format!("revoking a session: {e}")))??;
            // Off the request: the revoke has answered by the time this runs.
            if self.prune_due() {
                let before = onto_intelli::core::time::now_ms() - PRUNE_MARGIN_MS;
                tokio::task::spawn_blocking(move || {
                    purge_in_batches(|limit| store.kv_purge_expired(REVOKED_NS, before, limit))
                });
            }
            Ok(())
        })
    }

    fn is_revoked<'a>(
        &'a self,
        id: &'a str,
        tenant: Option<&'a str>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<bool>> + Send + 'a>> {
        Box::pin(async move {
            let store = self.store().await?;
            let (id, scoped) = (id.to_string(), auth::scoped_id(id, tenant));
            // Two primary-key reads: the id, and the id for this tenant.
            tokio::task::spawn_blocking(move || {
                Ok(store.kv_get(REVOKED_NS, &id)?.is_some() || store.kv_get(REVOKED_NS, &scoped)?.is_some())
            })
            .await
            .map_err(|e| Error::Other(format!("reading revocations: {e}")))?
        })
    }
}

/// Build the complete router (gRPC + reflection + health + JSON) for one ontology.
pub async fn router(intelli: Arc<Intelli>) -> Result<(axum::Router, Arc<RouteTable>)> {
    router_hub(Hub::single(intelli), &ServeOptions::default()).await
}

/// The router for a hub of tenants (`x-onto-tenant` picks one).
pub async fn router_hub(hub: Arc<Hub>, opts: &ServeOptions) -> Result<(axum::Router, Arc<RouteTable>)> {
    let cfg = hub.config().clone();
    let table = Arc::new(RouteTable::build(&cfg)?);
    let auth = Arc::new(auth::Auth::from_config_with(&cfg.auth, opts.dev, opts.control.clone())?);
    auth.set_revocations(Arc::new(HubRevocations::new(&hub)));
    let mut router = axum::Router::new();
    if cfg.grpc.reflection {
        let builder = || {
            let b = tonic_reflection::server::Builder::configure()
                .register_file_descriptor_set(table.file_descriptor_set());
            if cfg.grpc.health {
                b.register_encoded_file_descriptor_set(tonic_health::pb::FILE_DESCRIPTOR_SET)
            } else {
                b
            }
        };
        let v1 = builder().build_v1().map_err(|e| Error::Config(format!("reflection: {e}")))?;
        let v1alpha = builder().build_v1alpha().map_err(|e| Error::Config(format!("reflection: {e}")))?;
        router = router.route_service(&named(&v1), v1).route_service(&named(&v1alpha), v1alpha);
    }
    if cfg.grpc.health {
        let (reporter, health) = tonic_health::server::health_reporter();
        reporter.set_service_status("", tonic_health::ServingStatus::Serving).await;
        for svc in table.pool.services() {
            reporter.set_service_status(svc.full_name(), tonic_health::ServingStatus::Serving).await;
        }
        router = router.route_service(&named(&health), health);
    }
    if let Some(control) = &opts.control {
        // Projects get their own provider, limits and usage meter.
        onto_control::hooks::install(control, &hub, auth.dev_mode()).await;
    }
    if cfg.grpc.http {
        router = router.merge(http::router(hub.clone(), auth.clone()));
        if let Some(control) = &opts.control {
            router = router.merge(console::router(hub.clone(), auth.clone(), control.clone()));
        }
        if cfg.ui.enabled {
            router = router.merge(ui::router(&cfg.ui));
        }
    }
    let router = router.fallback_service(dynamic::DynamicGrpc { table: table.clone(), hub, auth });
    Ok((router, table))
}

/// Serve one ontology until `shutdown` resolves.
pub async fn serve(intelli: Arc<Intelli>, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
    serve_hub(Hub::single(intelli), ServeOptions::default(), shutdown).await
}

/// Serve a hub of tenants until `shutdown` resolves.
pub async fn serve_hub(
    hub: Arc<Hub>,
    opts: ServeOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener = bind(&hub.config().grpc.listen).await?;
    serve_hub_on(hub, opts, listener, shutdown).await
}

/// Bind `grpc.listen` (an address such as `127.0.0.1:50051`).
pub async fn bind(listen: &str) -> Result<tokio::net::TcpListener> {
    let addr: SocketAddr = listen.parse().map_err(|e| Error::Config(format!("grpc.listen '{listen}': {e}")))?;
    tokio::net::TcpListener::bind(addr).await.map_err(|e| Error::Unavailable(format!("bind {addr}: {e}")))
}

/// Serve on an already-bound listener (tests, port 0, socket activation).
pub async fn serve_on(
    intelli: Arc<Intelli>,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    serve_hub_on(Hub::single(intelli), ServeOptions::default(), listener, shutdown).await
}

pub async fn serve_hub_on(
    hub: Arc<Hub>,
    opts: ServeOptions,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    prepare(hub, opts, listener).await?.run(shutdown).await
}

/// A server whose configuration was checked (authentication, the console's
/// startup rules, development mode on loopback only, routes and protos) on
/// a bound port, not yet serving. `onto serve` prepares before it prints
/// its banner, so a configuration error never follows a "listening" line.
pub struct Prepared {
    hub: Arc<Hub>,
    router: axum::Router,
    table: Arc<RouteTable>,
    listener: tokio::net::TcpListener,
}

/// How long [`Prepared::run_graceful`] waits for cut connections to end.
const CUT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

mod conn {
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::TcpStream;
    use tonic::transport::server::{Connected, TcpConnectInfo};

    type Cut = Pin<Box<dyn Future<Output = ()> + Send>>;

    /// An accepted connection that ends when its cut signal turns `true`:
    /// every read and write after that fails, so the connection task
    /// serving it (and the request it was handling) ends.
    pub(crate) struct Cuttable {
        io: TcpStream,
        cut: Option<Cut>,
        was_cut: bool,
    }

    impl Cuttable {
        pub(crate) fn new(io: TcpStream, cut: Option<tokio::sync::watch::Receiver<bool>>) -> Self {
            let cut = cut.map(|mut rx| -> Cut {
                Box::pin(async move {
                    // A dropped sender (the server is gone) cuts too.
                    let _ = rx.wait_for(|c| *c).await;
                })
            });
            Self { io, cut, was_cut: false }
        }

        fn check(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
            if let Some(f) = &mut self.cut
                && f.as_mut().poll(cx).is_ready()
            {
                self.cut = None;
                self.was_cut = true;
            }
            if self.was_cut {
                Err(io::Error::new(io::ErrorKind::ConnectionAborted, "cut at the shutdown deadline"))
            } else {
                Ok(())
            }
        }
    }

    impl Connected for Cuttable {
        type ConnectInfo = TcpConnectInfo;

        fn connect_info(&self) -> TcpConnectInfo {
            self.io.connect_info()
        }
    }

    impl AsyncRead for Cuttable {
        fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            if let Err(e) = self.check(cx) {
                return Poll::Ready(Err(e));
            }
            Pin::new(&mut self.io).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Cuttable {
        fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            if let Err(e) = self.check(cx) {
                return Poll::Ready(Err(e));
            }
            Pin::new(&mut self.io).poll_write(cx, buf)
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[io::IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            if let Err(e) = self.check(cx) {
                return Poll::Ready(Err(e));
            }
            Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
        }

        fn is_write_vectored(&self) -> bool {
            self.io.is_write_vectored()
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            if let Err(e) = self.check(cx) {
                return Poll::Ready(Err(e));
            }
            Pin::new(&mut self.io).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_shutdown(cx)
        }
    }
}

/// Check everything that can fail at startup and build the router.
pub async fn prepare(hub: Arc<Hub>, opts: ServeOptions, listener: tokio::net::TcpListener) -> Result<Prepared> {
    let dev = opts.dev || hub.config().auth.mode == "dev";
    let addr = listener.local_addr().ok();
    if dev {
        let loopback = addr.is_some_and(|a| a.ip().is_loopback());
        if !loopback && std::env::var("ONTO_DEV_ALLOW_REMOTE").as_deref() != Ok("1") {
            return Err(Error::Config(format!(
                "development mode (no authentication) only listens on loopback addresses, not {addr:?}; \
                 configure [[auth.tokens]] instead"
            )));
        }
        tracing::warn!("DEVELOPMENT MODE: no authentication, every caller is {}", hub.config().auth.dev_principal.id);
    }
    check_console(hub.config(), opts.control.as_deref(), dev)?;
    let (router, table) = router_hub(hub.clone(), &opts).await?;
    Ok(Prepared { hub, router, table, listener })
}

impl Prepared {
    /// The bound address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.listener.local_addr().ok()
    }

    /// Serve until `shutdown` resolves, then until open connections close.
    /// The hub is left as it is (another server may share it).
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        self.serve(shutdown, None).await
    }

    /// [`run`](Self::run), with every accepted connection tied to `cut`:
    /// once it turns `true`, the connections still open end (tonic serves
    /// each one in its own task, which dropping the serve future leaves
    /// running).
    async fn serve(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
        cut: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<()> {
        use tokio_stream::StreamExt as _;
        let addr = self.listener.local_addr().ok();
        tracing::info!(?addr, routes = self.table.routes.len(), "onto-intelli listening (gRPC + HTTP/JSON)");
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(self.listener)
            .map(move |io| io.map(|io| conn::Cuttable::new(io, cut.clone())));
        tonic::transport::Server::builder()
            .accept_http1(true)
            .add_routes(tonic::service::Routes::from(self.router))
            .serve_with_incoming_shutdown(incoming, shutdown)
            .await
            .map_err(|e| Error::Unavailable(format!("server: {e}")))
    }

    /// Serve until `shutdown` resolves, then shut down gracefully within
    /// `grace`: stop accepting connections, close the hub's tenants (change
    /// streams end with their final frame, workers start no new round, no
    /// tenant opens; open tenants keep serving the requests in flight), let
    /// open requests and the workers' current rounds finish, cut the
    /// connections still open at the deadline (their requests end
    /// unanswered), and release the leases, claimed outbox rows and wake
    /// scanner lock this process holds (fenced: only the current holder at
    /// its epoch), so a replacement process takes over at once. Use it when
    /// this server owns the hub.
    pub async fn run_graceful(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
        grace: std::time::Duration,
    ) -> Result<onto_intelli::Released> {
        let hub = self.hub.clone();
        let at: Arc<std::sync::OnceLock<tokio::time::Instant>> = Arc::default();
        let fired = Arc::new(tokio::sync::Notify::new());
        let signal = {
            let (hub, at, fired) = (hub.clone(), at.clone(), fired.clone());
            async move {
                shutdown.await;
                let _ = at.set(tokio::time::Instant::now());
                tracing::info!(
                    grace_secs = grace.as_secs(),
                    "shutting down: no new connections, streams end, workers stop"
                );
                hub.begin_shutdown();
                fired.notify_one();
            }
        };
        let (cut, cuts) = tokio::sync::watch::channel(false);
        let mut serve = Box::pin(self.serve(signal, Some(cuts)));
        let finished = tokio::select! {
            r = &mut serve => Some(r),
            _ = async {
                fired.notified().await;
                let start = at.get().copied().unwrap_or_else(tokio::time::Instant::now);
                tokio::time::sleep_until(start + grace).await;
            } => None,
        };
        let drained = match finished {
            Some(r) => r,
            None => {
                // Connections still open at the deadline are cut here: their
                // reads and writes fail, so each connection task ends; the
                // server returns once the last one has.
                tracing::warn!("connections still open after {grace:?}: cutting them");
                let _ = cut.send(true);
                if tokio::time::timeout(CUT_WAIT, &mut serve).await.is_err() {
                    tracing::warn!("connections still open {CUT_WAIT:?} after the cut");
                }
                Ok(())
            }
        };
        drop(serve);
        // The server may have stopped on its own (an error): stop the rest too.
        let deadline = at.get().map_or_else(|| tokio::time::Instant::now() + grace, |t| *t + grace);
        let released = hub.finish_shutdown(deadline).await;
        tracing::info!(
            leases = released.leases,
            claims = released.claims,
            crawls = released.crawls,
            cut = released.cut,
            failed = released.failed,
            "released what this process held"
        );
        drained.map(|()| released)
    }
}

/// Startup rules of the control plane: a control store when the console is
/// enabled, multi-tenancy (each project is a tenant), HTTP on, at least one
/// sign-in path (an OIDC provider, the bootstrap token or dev mode), and an
/// `https://` public URL unless its host is loopback.
pub fn check_console(cfg: &onto_intelli::Config, control: Option<&Control>, dev: bool) -> Result<()> {
    let Some(control) = control else {
        if cfg.console.enabled {
            return Err(Error::Config(
                "console.enabled = true needs a control store (ServeOptions.control; `onto serve` builds it)".into(),
            ));
        }
        return Ok(());
    };
    if cfg.engine.tenancy != "multi" {
        return Err(Error::Config("the console needs engine.tenancy = \"multi\": every project is a tenant".into()));
    }
    if !cfg.grpc.http {
        return Err(Error::Config("the console needs grpc.http = true (its API is HTTP/JSON)".into()));
    }
    let ccfg = control.config();
    if ccfg.oidc.is_empty() && !control.bootstrap_enabled() && !dev {
        return Err(Error::Config(format!(
            "the console has no sign-in path: add a [[console.oidc]] provider, set {} (at least 32 characters), \
             or run `onto serve --dev`",
            ccfg.bootstrap_token_env.as_deref().unwrap_or("console.bootstrap_token_env")
        )));
    }
    if !public_url_ok(&ccfg.public_url) {
        return Err(Error::Config(format!(
            "console.public_url '{}' must be https:// (http:// only for a loopback host)",
            ccfg.public_url
        )));
    }
    Ok(())
}

/// `https://…`, or `http://` with a loopback host.
fn public_url_ok(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("https://") {
        return !rest.is_empty();
    }
    let Some(rest) = url.strip_prefix("http://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    #[test]
    fn expired_revocations_are_deleted_in_batches_until_one_comes_back_short() {
        use std::cell::RefCell;
        let answers = RefCell::new(vec![Ok(3), Ok(super::PRUNE_BATCH as u64)]);
        assert_eq!(super::purge_in_batches(|_| answers.borrow_mut().pop().unwrap()), 2, "a full batch, then 3 rows");
        let failing = |_| Err(onto_intelli::Error::Unavailable("down".into()));
        assert_eq!(super::purge_in_batches(failing), 1, "an error stops it");
    }

    #[test]
    fn a_repeated_revocation_never_shortens_the_first() {
        use onto_intelli::core::store::{MemoryStore, Store};
        let store = MemoryStore::default();
        let now = onto_intelli::core::time::now_ms() as u64 / 1000;
        let expiry =
            |store: &MemoryStore| store.kv_get(super::REVOKED_NS, "old-id").unwrap().unwrap()["expires_at"].clone();
        // Revoked for 30 days, then again after the limit was lowered to an hour.
        super::write_revocation(&store, "old-id", now + 30 * 86_400).unwrap();
        super::write_revocation(&store, "old-id", now + 3_600).unwrap();
        assert_eq!(expiry(&store), now + 30 * 86_400);
        // A later expiry still extends it.
        super::write_revocation(&store, "old-id", now + 60 * 86_400).unwrap();
        assert_eq!(expiry(&store), now + 60 * 86_400);
    }

    #[test]
    fn public_urls() {
        for ok in ["https://onto.example.com", "http://127.0.0.1:50051", "http://localhost", "http://[::1]:8080/x"] {
            assert!(super::public_url_ok(ok), "{ok}");
        }
        for bad in ["http://onto.example.com", "http://10.0.0.1", "ftp://x", "https://", "onto.example.com"] {
            assert!(!super::public_url_ok(bad), "{bad}");
        }
    }
}
