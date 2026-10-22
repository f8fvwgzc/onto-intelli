//! Data-plane observability over HTTP (docs/CONTROL_PLANE.md section 10):
//! thin routes over the `trace.*` actions, for any credential whose
//! principal may observe (a console session with `x-onto-tenant`, an
//! admin-scope API key, an operator token).
//!
//! * `GET  /v1/traces?…`              `trace.list` (the query string as a `TraceQuery`)
//! * `GET  /v1/traces/{id}`           `trace.get`
//! * `POST /v1/traces/{id}/scores`    `trace.score`
//! * `GET  /v1/metrics?…`             `trace.metrics`
//! * `GET  /v1/sessions?…`            `trace.sessions`
//! * `GET  /v1/sessions/{id}?limit=`  `trace.session`
//!
//! Every data-plane request also carries trace context in ([`trace_ctx`]):
//! `x-onto-session-id`, a W3C `traceparent` (an invalid one is ignored),
//! the source (`http` or `grpc`) and the API key's prefix; responses of
//! traced calls carry `x-onto-trace-id`.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use serde_json::{Value, json};

use onto_intelli::trace::TraceCtx;

use crate::auth::{Caller, Credential};
use crate::http::{AppState, run};

/// The session id request header.
pub const SESSION_ID: &str = "x-onto-session-id";
/// The W3C Trace Context request header.
pub const TRACEPARENT: &str = "traceparent";
/// The response header naming the trace of a traced call.
pub const TRACE_ID: &str = "x-onto-trace-id";

/// What a request says about its trace: `x-onto-session-id`, `traceparent`,
/// where it came from and the API key's prefix.
pub fn trace_ctx(headers: &HeaderMap, caller: &Caller, source: &'static str) -> TraceCtx {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    let mut ctx = TraceCtx::from_source(source).with_traceparent(get(TRACEPARENT));
    ctx.session_id = get(SESSION_ID).map(str::to_string);
    if let Credential::ApiKey { prefix, .. } = &caller.credential {
        ctx.api_key = Some(prefix.clone());
    }
    ctx
}

pub(crate) fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v1/traces", get(list))
        .route("/v1/traces/{id}", get(trace_get))
        .route("/v1/traces/{id}/scores", post(score))
        .route("/v1/metrics", get(metrics))
        .route("/v1/sessions", get(sessions))
        .route("/v1/sessions/{id}", get(session))
}

/// The query string as an action input: `since`, `until`, `limit` and
/// `min_ms` are numbers, everything else stays text (`q=2024` is text).
fn query(q: HashMap<String, String>) -> Value {
    Value::Object(
        q.into_iter()
            .map(|(k, v)| {
                let n = match k.as_str() {
                    "since" | "until" | "limit" => v.parse::<i64>().ok().map(Value::from),
                    "min_ms" => v.parse::<f64>().ok().map(Value::from),
                    _ => None,
                };
                (k, n.unwrap_or(Value::String(v)))
            })
            .collect(),
    )
}

async fn list(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    run(&s, &headers, "trace.list", query(q)).await
}

async fn trace_get(State(s): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    run(&s, &headers, "trace.get", json!({ "id": id })).await
}

async fn score(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let mut input = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    if let Value::Object(m) = &mut input {
        m.insert("trace_id".into(), Value::String(id));
    }
    run(&s, &headers, "trace.score", input).await
}

async fn metrics(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    run(&s, &headers, "trace.metrics", query(q)).await
}

async fn sessions(State(s): State<AppState>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    run(&s, &headers, "trace.sessions", query(q)).await
}

async fn session(
    State(s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut input = query(q);
    if let Value::Object(m) = &mut input {
        m.insert("id".into(), Value::String(id));
    }
    run(&s, &headers, "trace.session", input).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_keep_text() {
        let q: HashMap<String, String> = [("since", "5"), ("limit", "x"), ("q", "2024"), ("min_ms", "1.5")]
            .map(|(k, v)| (k.into(), v.into()))
            .into();
        assert_eq!(query(q), json!({ "since": 5, "limit": "x", "q": "2024", "min_ms": 1.5 }));
    }
}
