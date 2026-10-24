//! MCP over HTTP (docs/CONTROL_PLANE.md section 10): the Model Context
//! Protocol's Streamable HTTP transport with JSON responses only.
//!
//! * `POST /mcp`  one JSON-RPC message or a batch, answered as JSON; a body of
//!   notifications only is `202 Accepted` with no body.
//! * `GET  /mcp`  `405`: the server opens no server-initiated stream.
//!
//! A request is authenticated like any data-plane request (an `agent`-scope
//! API key is the intended credential) and addresses the credential's
//! project. The messages go to [`McpServer::handle`], which is transport
//! independent. Tools that write are offered only when `[mcp] allow_writes`
//! is on and the principal may write; the tool profile is `ONTO_MCP_PROFILE`,
//! else `[mcp] profile` (see [`onto_intelli::mcp::MCP_PROFILES`]). No MCP session state is kept, so
//! `Mcp-Session-Id` is not needed; `Mcp-Protocol-Version`, when sent, must be
//! one of [`onto_intelli::mcp::SUPPORTED`]. Calls are traced with the source
//! `mcp`, the request's `x-onto-session-id` and `traceparent`, and the API
//! key's prefix.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use onto_intelli::core::Access;
use onto_intelli::mcp::{McpServer, SUPPORTED};

use crate::http::{AppState, caller_full};

/// The request header naming the client's protocol version.
pub const PROTOCOL_VERSION: &str = "mcp-protocol-version";

pub(crate) fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/mcp", post(mcp).get(no_stream).delete(no_stream))
}

/// A JSON-RPC error that answers no particular request.
fn rpc_error(status: StatusCode, code: i64, message: &str) -> Response {
    (status, Json(json!({ "jsonrpc": "2.0", "id": Value::Null, "error": { "code": code, "message": message } })))
        .into_response()
}

async fn no_stream() -> Response {
    let mut resp = rpc_error(
        StatusCode::METHOD_NOT_ALLOWED,
        -32000,
        "this server sends no server-initiated stream; POST JSON-RPC messages to /mcp",
    );
    resp.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST"));
    resp
}

async fn mcp(State(s): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(v) = headers.get(PROTOCOL_VERSION) {
        let v = v.to_str().unwrap_or_default().trim();
        if !SUPPORTED.contains(&v) {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                -32600,
                &format!("unsupported MCP protocol version '{v}' (supported: {})", SUPPORTED.join(", ")),
            );
        }
    }
    let (caller, intelli) = match caller_full(&s, &headers).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return rpc_error(StatusCode::BAD_REQUEST, -32700, &format!("parse error: {e}")),
    };
    let allow_writes = intelli.config().mcp.allow_writes && caller.principal.allows(Access::Write);
    let ctx = crate::observe::trace_ctx(&headers, &caller, "mcp");
    // The tool profile: ONTO_MCP_PROFILE, else `[mcp] profile` (there is no
    // CLI flag on this transport); an unknown one refuses every request.
    let profile = intelli.config().mcp.effective_profile(None);
    let server = match McpServer::new(intelli, caller.principal, allow_writes).with_profile(&profile) {
        Ok(s) => s.with_trace_ctx(ctx),
        Err(e) => return rpc_error(StatusCode::INTERNAL_SERVER_ERROR, -32603, &e.to_string()),
    };
    match msg {
        Value::Array(batch) => {
            if batch.is_empty() {
                return rpc_error(StatusCode::BAD_REQUEST, -32600, "an empty batch");
            }
            let mut replies = Vec::new();
            for m in batch {
                if !m.is_object() {
                    replies.push(json!({ "jsonrpc": "2.0", "id": Value::Null,
                                         "error": { "code": -32600, "message": "invalid request" } }));
                } else if let Some(r) = server.handle(m).await {
                    replies.push(r);
                }
            }
            if replies.is_empty() { accepted() } else { Json(Value::Array(replies)).into_response() }
        }
        m @ Value::Object(_) => match server.handle(m).await {
            Some(r) => Json(r).into_response(),
            None => accepted(),
        },
        _ => rpc_error(StatusCode::BAD_REQUEST, -32600, "invalid request: a JSON-RPC object or a batch"),
    }
}

/// Notifications and responses from the client: nothing to answer.
fn accepted() -> Response {
    StatusCode::ACCEPTED.into_response()
}
