//! # onto-grpc
//!
//! One port serves:
//! * the built-in `onto_intelli.v1` gRPC API,
//! * **your own proto files**, compiled at startup and mapped onto engine
//!   actions with `[[grpc.routes]]` (no code generation, no rebuild),
//! * gRPC reflection (grpcurl, Postman) and `grpc.health.v1`,
//! * JSON over HTTP/1.1 (`POST /v1/call/{action}`, `/v1/ask`, SSE streaming).
//!
//! A typed Rust client for the built-in API is in [`pb`].

pub mod auth;
pub mod dynamic;
pub mod http;
pub mod routes;
pub mod template;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tonic::server::NamedService;

use onto_intelli::{Error, Intelli, Result};

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

/// Build the complete router (gRPC + reflection + health + JSON).
pub async fn router(intelli: Arc<Intelli>) -> Result<(axum::Router, Arc<RouteTable>)> {
    let cfg = intelli.config().clone();
    let table = Arc::new(RouteTable::build(&cfg)?);
    let auth = Arc::new(auth::Auth::from_config(&cfg.auth)?);
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
    if cfg.grpc.http {
        router = router.merge(http::router(intelli.clone(), auth.clone()));
    }
    let router = router.fallback_service(dynamic::DynamicGrpc { table: table.clone(), intelli, auth });
    Ok((router, table))
}

/// Serve until `shutdown` resolves.
pub async fn serve(intelli: Arc<Intelli>, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
    let listen = intelli.config().grpc.listen.clone();
    let addr: SocketAddr = listen.parse().map_err(|e| Error::Config(format!("grpc.listen '{listen}': {e}")))?;
    let listener =
        tokio::net::TcpListener::bind(addr).await.map_err(|e| Error::Unavailable(format!("bind {addr}: {e}")))?;
    serve_on(intelli, listener, shutdown).await
}

/// Serve on an already-bound listener (tests, port 0, socket activation).
pub async fn serve_on(
    intelli: Arc<Intelli>,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let (router, table) = router(intelli).await?;
    let addr = listener.local_addr().ok();
    tracing::info!(?addr, routes = table.routes.len(), "onto-intelli listening (gRPC + HTTP/JSON)");
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tonic::transport::Server::builder()
        .accept_http1(true)
        .add_routes(tonic::service::Routes::from(router))
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await
        .map_err(|e| Error::Unavailable(format!("server: {e}")))
}
