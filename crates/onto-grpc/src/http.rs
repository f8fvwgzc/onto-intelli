//! JSON over HTTP/1.1 on the gRPC port, for curl, browsers and scripts.
//!
//! * `GET  /healthz`              liveness
//! * `GET  /v1/actions`           action catalogue
//! * `GET  /v1/stats`             statistics
//! * `POST /v1/call/{action}`     run any action with a JSON body
//! * `POST /v1/ask`               ask a question
//! * `POST /v1/ask/stream`        ask with Server-Sent Events

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{Stream, StreamExt};
use serde_json::{Value, json};

use onto_intelli::{AskRequest, Error, Intelli, Principal};

use crate::auth::{Auth, workspace_header};

#[derive(Clone)]
struct AppState {
    intelli: Arc<Intelli>,
    auth: Arc<Auth>,
}

pub fn router(intelli: Arc<Intelli>, auth: Arc<Auth>) -> axum::Router {
    axum::Router::new()
        .route("/healthz", get(health))
        .route("/v1/actions", get(actions))
        .route("/v1/stats", get(stats))
        .route("/v1/call/{action}", post(call))
        .route("/v1/ask", post(ask))
        .route("/v1/ask/stream", post(ask_stream))
        .with_state(AppState { intelli, auth })
}

fn error_response(e: &Error) -> Response {
    let status = match e {
        Error::NotFound(_) => StatusCode::NOT_FOUND,
        Error::Invalid(_) => StatusCode::BAD_REQUEST,
        Error::Denied(_) => StatusCode::FORBIDDEN,
        Error::Conflict(_) => StatusCode::CONFLICT,
        Error::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({ "error": { "code": e.code(), "message": e.to_string() } }))).into_response()
}

fn principal(state: &AppState, headers: &HeaderMap) -> Result<Principal, Box<Response>> {
    state.auth.principal(headers).map_err(|msg| {
        Box::new(
            (StatusCode::UNAUTHORIZED, Json(json!({ "error": { "code": "unauthenticated", "message": msg } })))
                .into_response(),
        )
    })
}

fn with_workspace(mut input: Value, headers: &HeaderMap) -> Value {
    if let (Value::Object(m), Some(ws)) = (&mut input, workspace_header(headers))
        && !m.contains_key("workspace")
    {
        m.insert("workspace".into(), Value::String(ws));
    }
    input
}

async fn run(state: &AppState, headers: &HeaderMap, action: &str, input: Value) -> Response {
    let p = match principal(state, headers) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match state.intelli.call(&p, action, with_workspace(input, headers)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn health(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "system.health", json!({})).await
}

async fn actions(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "system.actions", json!({})).await
}

async fn stats(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "system.stats", json!({})).await
}

async fn call(
    State(s): State<AppState>,
    Path(action): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    run(&s, &headers, &action, body.map(|Json(v)| v).unwrap_or(Value::Null)).await
}

async fn ask(State(s): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    run(&s, &headers, "intelli.ask", body).await
}

async fn ask_stream(State(s): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let p = match principal(&s, &headers) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let req: AskRequest = match serde_json::from_value(with_workspace(body, &headers)) {
        Ok(r) => r,
        Err(e) => return error_response(&Error::invalid(e)),
    };
    let rx = s.intelli.ask_stream(p, req);
    let events: std::pin::Pin<Box<dyn Stream<Item = Result<SseEvent, Infallible>> + Send>> =
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx).map(|ev| {
            Ok(SseEvent::default().event(ev.kind.clone()).data(serde_json::to_string(&ev).unwrap_or_default()))
        }));
    Sse::new(events).keep_alive(KeepAlive::default()).into_response()
}
