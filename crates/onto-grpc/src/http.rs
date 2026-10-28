//! JSON over HTTP/1.1 on the gRPC port, for curl, browsers and scripts.
//!
//! * `GET  /livez`                liveness, without credentials (the process answers)
//! * `GET  /readyz`               readiness, without credentials (the store answers)
//! * `GET  /healthz`              the server's health, for an authenticated caller
//! * `GET  /v1/actions`           action catalogue
//! * `GET  /v1/stats`             statistics
//! * `POST /v1/call/{action}`     run any action with a JSON body
//! * `POST /v1/ask`               ask a question
//! * `POST /v1/ask/stream`        ask with Server-Sent Events
//! * `GET  /v1/schema`            active schema (`?version=` for an older one)
//! * `GET  /v1/schema/status`     version, mode, governance
//! * `POST /v1/schema/plan`       dry run of a change set (impact, migration)
//! * `POST /v1/schema/apply`      apply a change set as a new version
//! * `GET  /v1/schema/history`    versions (`?limit=`)
//! * `GET  /v1/schema/versions/{v}`  one version with its document and diff
//! * `GET  /v1/schema/diff`       `?from=&to=`
//! * `GET  /v1/schema/as_of`      `?ts=` or `?days_ago=`
//! * `POST /v1/schema/revert`     `{version}`; `/v1/schema/undo` likewise
//! * `GET  /v1/schema/proposals`  proposals (`?status=&limit=`); `POST` files one
//! * `GET  /v1/schema/proposals/{id}`  one proposal with a fresh preview
//! * `POST /v1/schema/proposals/{id}/{approve|reject|comment|withdraw|revise}`
//! * `GET  /v1/execution/graphs`  nexc graphs; `POST` creates one (starter vocabulary)
//! * `GET|PUT /v1/execution/graphs/{graph}/vocabulary`  node and relation types
//! * `POST /v1/ontology/draft`   draft object, link and action types from a source
//! * `POST /v1/ontology/refine`  the draft refined by the LLM
//! * `POST /v1/ontology/advise`  modeling advice `{question, source?}`
//! * `POST /v1/ontology/apply`   apply a draft, or `{mode: "propose"}` file it
//! * `GET  /v1/objects/types`    object types with counts (`?workspace=`)
//! * `POST /v1/objects/explore`  Object Explorer: filters, facets, statistics
//! * `POST /v1/reason/query`    match triple patterns over asserted and derived facts
//! * `POST /v1/reason/certify`  a reasoning certificate with the onto-cert checker's report
//! * `POST /v1/reason/verify`   verdicts on claims `{claims, workspace?}`
//! * `POST /v1/shapes/validate` shape report (`{shapes?, workspace?, inferred?, limit?}`)
//! * `GET  /v1/schema/lint`     lint findings (`?checks=a,b&include_dismissed=true`)
//! * `GET  /v1/packs`           industry packs; `GET /v1/packs/{id}` one pack with its lint
//! * `POST /v1/packs/{id}/plan` the schema plan of installing a pack
//! * `POST /v1/packs/{id}/install`  install `{mode?, sample?, workspace?, message?}` (agents propose)
//! * `GET  /v1/changes`         one page of the change feed (`?after=` or `?from=earliest|now`, `limit`,
//!   comma-separated `kinds`, `ops`, `workspaces`, `ids`, `rels`, `props`, `sources`, `fields`,
//!   `include_migrations`)
//! * `GET  /v1/changes/head`    the feed's cursor, head, floor and whether capture is on
//! * `GET|POST /v1/changes/stream`  the change feed as Server-Sent Events: the filter keys of
//!   `GET /v1/changes` as a query (GET) or a JSON body (POST), `after` or `from=now|earliest`
//!   (default now); `Last-Event-ID` wins over `after`. Events `change`, `heartbeat`, then one
//!   `reset` or `end`.
//! * `POST /v1/changes/capture` `{on}`: record changes or not (admins of every workspace)
//! * `GET  /v1/webhooks`        webhook endpoints; `POST` registers one `{name, url, secret?, filter?,
//!   principal?, disabled?}` (admins who dominate its principal; a generated secret is returned once)
//! * `PATCH|DELETE /v1/webhooks/{name}`  change or remove an endpoint
//! * `GET  /v1/webhooks/{name}/deliveries`  its deliveries (`?states=`, `since`, `limit`, `id` for one
//!   delivery with its attempts)
//! * `POST /v1/webhooks/{name}/{test|rotate_secret|replay|resume}`  a test delivery, a secret rotation,
//!   a replay `{ids | since, states?}` (ids kept), or resuming a paused endpoint
//! * `POST /v1/webhooks/kill`   `{on | off}`: the tenant's webhook kill switch
//! * `GET  /v1/automations`     automations with their status (readers); `POST /v1/automations/kill`
//!   `{on | off}` is the tenant's automation kill switch
//! * `GET  /v1/automations/{name}`  one automation: definition, status, enablement, closure
//! * `GET  /v1/automations/{name}/runs`  its runs (`?state=`, `since`, `limit`), newest first;
//!   `GET .../runs/{id}` one run; `POST .../runs/{id}/rerun` runs a skipped or failed one again
//! * `POST /v1/automations/{name}/{test|enable|disable}`  a dry run `{cursor | id}` (writes nothing),
//!   enabling (approvers who dominate its run_as; it binds to its closure and expires), disabling.
//!   A run commits its action, receipt, outbox rows and run row together; webhook effects are
//!   enqueued once and delivered at least once, and external effects are not reversible.
//! * `GET  /v1/writeback/targets`  writeback targets; `POST` registers one `{name, base_url,
//!   routes: {name: path}, workspaces?, secret?, generate_secret?}` (admins of every workspace it
//!   serves). Writeback is not reversible; action.undo does not undo the external write.
//! * `DELETE /v1/writeback/targets/{name}`  remove a target; `POST .../{name}/test` `{route?}`
//!   checks it without sending anything
//! * `GET  /v1/outbox`          outbound deliveries (`?target=`, `states`, `source`, `receipt`, `run`,
//!   `since`, `limit`) for targets the caller may manage. Each change is enqueued once per endpoint
//!   and delivered at least once; receivers dedupe on the event id.
//! * `POST /v1/auth/session`      mint a short-lived browser session for the caller,
//!   or (impersonating tokens) for one of its users: `{act_as, roles, workspaces, entity, tenant, ttl_secs}`.
//!   A session minting a session cannot outlive itself and belongs to the same login (`sid`).
//!   `DELETE` signs out: it revokes the presented session's login, every session minted in it.
//! * `POST /v1/auth/session/revoke`  `{jti}`: a config token revokes a session it minted, and every
//!   session minted from that one (a `sid` works too), whatever the tenant
//! * `GET  /v1/auth/whoami`       the caller's principal, tenant and capabilities (and, for
//!   console sessions, `console: {user, role, org}`)
//! * `GET  /v1/admin/tenants`     operators: open tenants, their estimated memory, outbound counters
//!   (outbox rows per state, change lag, gaps, runs, paused counts) and the limits
//! * `/v1/traces`, `/v1/metrics`, `/v1/sessions`: observability (see [`crate::observe`])
//! * `POST /mcp`: MCP over HTTP (see [`crate::mcp_http`])
//!
//! Requests may send `x-onto-session-id` and a W3C `traceparent`; responses
//! of traced calls carry `x-onto-trace-id`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use futures::{Stream, StreamExt};
use serde_json::{Value, json};

use onto_intelli::{AskRequest, Error, Hub, Intelli, Principal};

use crate::auth::{Auth, AuthError, Caller, Credential, guard_call, workspace_header};

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub auth: Arc<Auth>,
}

pub fn router(hub: Arc<Hub>, auth: Arc<Auth>) -> axum::Router {
    axum::Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/healthz", get(health))
        .route("/v1/actions", get(actions))
        .route("/v1/stats", get(stats))
        .route("/v1/call/{action}", post(call))
        .route("/v1/ask", post(ask))
        .route("/v1/ask/stream", post(ask_stream))
        .route("/v1/schema", get(schema_get))
        .route("/v1/schema/status", get(schema_status))
        .route("/v1/schema/plan", post(schema_plan))
        .route("/v1/schema/apply", post(schema_apply))
        .route("/v1/schema/history", get(schema_history))
        .route("/v1/schema/versions/{version}", get(schema_version))
        .route("/v1/schema/diff", get(schema_diff))
        .route("/v1/schema/as_of", get(schema_as_of))
        .route("/v1/schema/revert", post(schema_revert))
        .route("/v1/schema/undo", post(schema_undo))
        .route("/v1/schema/proposals", get(proposals_list).post(proposals_create))
        .route("/v1/schema/proposals/{id}", get(proposal_get))
        .route("/v1/schema/proposals/{id}/{verb}", post(proposal_verb))
        .route("/v1/auth/session", post(session).delete(session_logout))
        .route("/v1/auth/session/revoke", post(session_revoke))
        .route("/v1/auth/whoami", get(whoami))
        .route("/v1/admin/tenants", get(admin_tenants))
        .route("/v1/execution/graphs", get(exec_graphs).post(exec_init))
        .route("/v1/execution/graphs/{graph}/vocabulary", get(exec_vocab).put(exec_set_vocab))
        .route("/v1/catalog/connectors", get(connectors_list).post(connector_add))
        .route("/v1/catalog/connectors/{name}", delete(connector_remove))
        .route("/v1/catalog/connectors/{name}/crawl", post(connector_crawl))
        .route("/v1/catalog/crawl", post(crawl_all))
        .route("/v1/catalog/find", get(catalog_find))
        .route("/v1/catalog/join", get(catalog_join))
        .route("/v1/catalog/metric", get(catalog_metric))
        .route("/v1/catalog/rank/{id}", get(catalog_rank))
        .route("/v1/catalog/top", get(catalog_top))
        .route("/v1/catalog/rerank", post(catalog_rerank))
        .route("/v1/catalog/facts", get(facts_list))
        .route("/v1/catalog/facts/{id}/{verb}", post(fact_verb))
        .route("/v1/ontology/draft", post(ontology_draft))
        .route("/v1/ontology/refine", post(ontology_refine))
        .route("/v1/ontology/advise", post(ontology_advise))
        .route("/v1/ontology/apply", post(ontology_apply))
        .route("/v1/objects/types", get(object_types))
        .route("/v1/objects/explore", post(objects_explore))
        .route("/v1/reason/query", post(reason_query))
        .route("/v1/reason/certify", post(reason_certify))
        .route("/v1/reason/verify", post(reason_verify))
        .route("/v1/shapes/validate", post(shapes_validate))
        .route("/v1/schema/lint", get(schema_lint))
        .route("/v1/packs", get(packs_list))
        .route("/v1/packs/{id}", get(pack_show))
        .route("/v1/packs/{id}/{verb}", post(pack_verb))
        .route("/v1/changes", get(changes_poll))
        .route("/v1/changes/head", get(changes_head))
        .route("/v1/changes/capture", post(changes_capture))
        .route("/v1/changes/stream", get(changes_stream_get).post(changes_stream_post))
        .route("/v1/webhooks", get(webhooks_list).post(webhook_add))
        .route("/v1/webhooks/kill", post(webhook_kill))
        .route("/v1/webhooks/{name}", patch(webhook_update).delete(webhook_remove))
        .route("/v1/webhooks/{name}/deliveries", get(webhook_deliveries))
        .route("/v1/webhooks/{name}/{verb}", post(webhook_verb))
        .route("/v1/outbox", get(outbox_list))
        .route("/v1/writeback/targets", get(writeback_list).post(writeback_add))
        .route("/v1/writeback/targets/{name}", delete(writeback_remove))
        .route("/v1/writeback/targets/{name}/test", post(writeback_test))
        .route("/v1/automations", get(automations_list))
        .route("/v1/automations/kill", get(automation_get_kill).post(automation_kill))
        .route("/v1/automations/{name}", get(automation_get))
        .route("/v1/automations/{name}/runs", get(automation_runs))
        .route("/v1/automations/{name}/runs/{id}", get(automation_run))
        .route("/v1/automations/{name}/runs/{id}/rerun", post(automation_rerun))
        .route("/v1/automations/{name}/{verb}", post(automation_verb))
        .merge(crate::observe::routes())
        .merge(crate::mcp_http::routes())
        .with_state(AppState { hub, auth })
}

pub fn error_response(e: &Error) -> Response {
    let status = match e {
        Error::NotFound(_) => StatusCode::NOT_FOUND,
        Error::Invalid(_) => StatusCode::BAD_REQUEST,
        Error::Denied(_) => StatusCode::FORBIDDEN,
        Error::Conflict(_) => StatusCode::CONFLICT,
        Error::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        Error::Limited(_) => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut resp = (status, Json(json!({ "error": { "code": e.code(), "message": e.to_string() } }))).into_response();
    if let Some(secs) = e.retry_after_secs()
        && let Ok(v) = axum::http::HeaderValue::from_str(&secs.to_string())
    {
        resp.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
    }
    resp
}

/// The response for a refused request: 401, 403 or 503.
pub fn auth_error_response(e: &AuthError) -> Response {
    let (status, code) = match e {
        AuthError::Unauthenticated(_) => (StatusCode::UNAUTHORIZED, "unauthenticated"),
        AuthError::Forbidden(_) => (StatusCode::FORBIDDEN, "permission_denied"),
        AuthError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    };
    (status, Json(json!({ "error": { "code": code, "message": e.to_string() } }))).into_response()
}

/// Authenticate and pick the tenant's ontology.
pub async fn caller(state: &AppState, headers: &HeaderMap) -> Result<(Principal, Arc<Intelli>), Box<Response>> {
    caller_full(state, headers).await.map(|(c, i)| (c.principal, i))
}

/// Authenticate and pick the tenant's ontology, keeping the whole caller.
pub async fn caller_full(state: &AppState, headers: &HeaderMap) -> Result<(Caller, Arc<Intelli>), Box<Response>> {
    let c = state.auth.authenticate(headers).await.map_err(|e| Box::new(auth_error_response(&e)))?;
    let intelli = state.hub.resolve(c.tenant.as_deref()).await.map_err(|e| Box::new(error_response(&e)))?;
    Ok((c, intelli))
}

fn with_workspace(mut input: Value, headers: &HeaderMap) -> Value {
    if let (Value::Object(m), Some(ws)) = (&mut input, workspace_header(headers))
        && !m.contains_key("workspace")
    {
        m.insert("workspace".into(), Value::String(ws));
    }
    input
}

/// Run an action for the caller, traced with the request's trace context.
pub(crate) async fn run(state: &AppState, headers: &HeaderMap, action: &str, input: Value) -> Response {
    let (c, intelli) = match caller_full(state, headers).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let mut input = with_workspace(input, headers);
    if let Err(e) = guard_call(&c.credential, &c.principal, action, &mut input, state.auth.allow_private_urls()).await {
        return error_response(&e);
    }
    let ctx = crate::observe::trace_ctx(headers, &c, "http");
    let t = intelli.call_traced(&c.principal, action, input, ctx).await;
    let resp = match t.result {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(&e),
    };
    with_trace_id(resp, t.trace_id.as_deref())
}

/// Add `x-onto-trace-id` when the call was traced.
fn with_trace_id(mut resp: Response, trace_id: Option<&str>) -> Response {
    if let Some(v) = trace_id.and_then(|id| axum::http::HeaderValue::from_str(id).ok()) {
        resp.headers_mut().insert(crate::observe::TRACE_ID, v);
    }
    resp
}

/// The process answers. No credentials, no details.
async fn livez() -> Response {
    Json(json!({ "status": "ok" })).into_response()
}

/// The store answers (one query), so the server can serve requests. No
/// credentials and nothing about the data: a probe may call it.
async fn readyz(State(s): State<AppState>) -> Response {
    match s.hub.ready().await {
        Ok(()) => Json(json!({ "status": "ready" })).into_response(),
        Err(e) => {
            tracing::warn!("readiness: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "status": "unavailable" }))).into_response()
        }
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

/// Query string as a JSON object; numeric values become numbers.
fn query_json(q: HashMap<String, String>) -> Value {
    fn scalar(v: String) -> Value {
        if let Ok(n) = v.parse::<i64>() {
            return Value::from(n);
        }
        if let Ok(f) = v.parse::<f64>() {
            return Value::from(f);
        }
        match v.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(v),
        }
    }
    Value::Object(q.into_iter().map(|(k, v)| (k, scalar(v))).collect())
}

fn body_or_empty(body: Option<Json<Value>>) -> Value {
    body.map(|Json(v)| v).unwrap_or_else(|| json!({}))
}

async fn schema_get(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "schema.get", query_json(q)).await
}

async fn schema_status(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "schema.status", json!({})).await
}

async fn schema_plan(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "schema.plan", body_or_empty(body)).await
}

async fn schema_apply(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "schema.apply", body_or_empty(body)).await
}

async fn schema_history(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "schema.history", query_json(q)).await
}

async fn schema_version(State(s): State<AppState>, Path(version): Path<u64>, headers: HeaderMap) -> Response {
    run(&s, &headers, "schema.version", json!({ "version": version })).await
}

async fn schema_diff(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "schema.diff", query_json(q)).await
}

async fn schema_as_of(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "schema.as_of", query_json(q)).await
}

async fn schema_revert(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "schema.revert", body_or_empty(body)).await
}

async fn schema_undo(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "schema.undo", body_or_empty(body)).await
}

async fn proposals_list(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "schema.proposals", query_json(q)).await
}

async fn proposals_create(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "schema.propose", body_or_empty(body)).await
}

async fn proposal_get(State(s): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    run(&s, &headers, "schema.proposal", json!({ "id": id })).await
}

async fn proposal_verb(
    State(s): State<AppState>,
    Path((id, verb)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let action = match verb.as_str() {
        "approve" => "schema.approve",
        "reject" => "schema.reject",
        "comment" => "schema.comment",
        "withdraw" => "schema.withdraw",
        "revise" => "schema.revise",
        other => return error_response(&Error::not_found(format!("proposal action '{other}'"))),
    };
    let mut input = body_or_empty(body);
    if let Value::Object(m) = &mut input {
        m.insert("id".into(), Value::String(id));
    }
    run(&s, &headers, action, input).await
}

#[derive(serde::Deserialize, Default)]
struct SessionIn {
    #[serde(default)]
    act_as: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    workspaces: Vec<String>,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    tenant: Option<String>,
    #[serde(default)]
    ttl_secs: Option<u64>,
}

async fn session(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let input: SessionIn = match serde_json::from_value(body_or_empty(body)) {
        Ok(i) => i,
        Err(e) => return error_response(&Error::invalid(format!("session: {e}"))),
    };
    // The body names the principal the same way the x-onto-act-* headers do,
    // so the same limits apply (impersonate_roles, workspaces, tenants).
    let mut h = headers.clone();
    let mut set = |name: &'static str, v: String| {
        if let Ok(val) = v.parse() {
            h.insert(name, val);
        }
    };
    if let Some(who) = input.act_as {
        set(crate::auth::ACT_AS, who);
        if !input.roles.is_empty() {
            set(crate::auth::ACT_ROLES, input.roles.join(","));
        }
        if !input.workspaces.is_empty() {
            set(crate::auth::ACT_WORKSPACES, input.workspaces.join(","));
        }
        if let Some(e) = input.entity {
            set(crate::auth::ACT_ENTITY, e);
        }
    }
    if let Some(t) = input.tenant {
        set(crate::auth::TENANT, t);
    }
    let (c, _intelli) = match caller_full(&s, &h).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let ttl = input.ttl_secs.unwrap_or(3600);
    let principal = c.principal.clone();
    // A session minting a session cannot outlive itself (renewal ends when
    // the first session would have) and stays in its login, so revoking the
    // first one revokes this one too.
    let parent = c.session.as_ref();
    let (token, minted, tenant) = match &c.credential {
        Credential::Console { .. } => {
            return error_response(&Error::Denied(
                "console sessions cannot mint sessions; use the console session itself".into(),
            ));
        }
        // The key's delegate: refused once the key is revoked.
        Credential::ApiKey { prefix, project, .. } => {
            let (t, m) = s.auth.issue_key_session_from(principal.clone(), prefix, project, ttl, parent);
            (t, m, Some(project.clone()))
        }
        _ => {
            let tenant = c.tenant.clone().or_else(|| Some(s.hub.default_tenant().to_string()));
            let (t, m) = s.auth.issue_session_from(principal.clone(), tenant.clone(), ttl, parent);
            (t, m, tenant)
        }
    };
    Json(json!({
        "token": token,
        "expires_at": minted.exp,
        "jti": minted.jti,
        "sid": minted.sid,
        "principal": principal,
        "tenant": tenant,
    }))
    .into_response()
}

/// Sign out: the presented session's login is revoked, so the session, the
/// one it was minted from and every other one minted in that login.
async fn session_logout(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let c = match s.auth.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return auth_error_response(&e),
    };
    let Some((login, exp)) = c.session.as_ref().and_then(|si| Some((si.login()?.to_string(), si.exp))) else {
        return error_response(&Error::invalid("DELETE /v1/auth/session revokes a session token: present one"));
    };
    match s.auth.revoke_session(&login, exp).await {
        Ok(()) => Json(json!({ "revoked": login })).into_response(),
        Err(e) => error_response(&e),
    }
}

#[derive(serde::Deserialize)]
struct RevokeIn {
    /// A session's `jti` (which is also the `sid` of the sessions minted
    /// from it), or a `sid`.
    jti: String,
    /// Unix seconds the session expires at (from the mint response). The id
    /// stays revoked at least as long as any session could live anyway.
    #[serde(default)]
    expires_at: Option<u64>,
}

/// A service (a config token) revokes a session it minted for a user, and
/// the sessions minted from it. Session ids name no tenant: the revocation
/// holds whatever tenant the request names.
async fn session_revoke(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let c = match s.auth.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return auth_error_response(&e),
    };
    if !matches!(c.credential, Credential::Config | Credential::Dev) {
        return error_response(&Error::Denied("only a service token ([[auth.tokens]]) revokes sessions by id".into()));
    }
    let input: RevokeIn = match serde_json::from_value(body_or_empty(body)) {
        Ok(i) => i,
        Err(e) => return error_response(&Error::invalid(format!("revoke: {e}"))),
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let exp = input.expires_at.unwrap_or(now + s.hub.config().auth.session_max_ttl_secs);
    match s.auth.revoke_session(&input.jti, exp).await {
        Ok(()) => Json(json!({ "revoked": input.jti })).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn whoami(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let (c, intelli) = match caller_full(&s, &headers).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let p = &c.principal;
    let onto = intelli.ontology();
    let gov = &onto.options().governance;
    let tenant = c.tenant.clone().unwrap_or_else(|| s.hub.default_tenant().to_string());
    // Project credentials see their own project only.
    let tenants = if p.is_privileged() && !c.credential.is_project() {
        s.hub.known_tenants().unwrap_or_default()
    } else {
        vec![tenant.clone()]
    };
    let console = match &c.credential {
        Credential::Console { user, role, org } => json!({ "user": user, "role": role, "org": org }),
        _ => Value::Null,
    };
    let cfg = intelli.config();
    Json(json!({
        "console": console,
        "credential": credential_kind(&c.credential),
        "principal": p,
        "tenant": tenant,
        "tenants": tenants,
        "multi_tenant": s.hub.multi_tenant(),
        "default_workspace": cfg.engine.default_workspace,
        "dev_mode": s.auth.dev_mode(),
        "version": onto_intelli::VERSION,
        "schema_version": onto.schema_version(),
        "capabilities": {
            "read": p.allows(onto_intelli::core::Access::Read),
            "write": p.allows(onto_intelli::core::Access::Write),
            "apply_schema": gov.can_apply(p) && gov.live_edits,
            "approve": gov.can_approve(p),
            "propose": gov.can_propose(p),
            "observe": p.can_observe(),
            "configure_outbound": p.is_privileged() && !p.is_agent(),
            "automate": !p.is_agent() && (gov.can_approve(p) || p.is_privileged()),
        },
        "governance": gov,
        "ui": { "title": cfg.ui.title },
    }))
    .into_response()
}

/// Open tenants with their estimated memory and outbound counters: an
/// operator's view of the whole process, so only a privileged principal
/// acting for itself on a credential for every tenant (a config token with
/// `tenants = ["*"]`, or dev mode) may read it. Tokens limited to some
/// tenants, sessions bound to one, project credentials (API keys, console
/// sessions), callers acting for someone else and non-admins are refused.
async fn admin_tenants(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let c = match s.auth.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return auth_error_response(&e),
    };
    if !c.is_operator() {
        return error_response(&Error::Denied(format!(
            "{} may not list the server's tenants (operators only: an admin token for every tenant)",
            c.principal.id
        )));
    }
    let e = &s.hub.config().engine;
    // The outbound counters read each tenant's store (several round trips
    // per tenant), a bounded number of tenants at a time.
    let tenants = s.hub.tenant_report().await;
    Json(json!({
        "open": tenants.len(),
        "subscribers_process": onto_intelli::changes::ChangeHub::process_subscribers(),
        "estimated_bytes": tenants.iter().map(|t| t.estimated_bytes).sum::<u64>(),
        "listening_stores": s.hub.listening_stores(),
        "limits": {
            "max_open_tenants": e.max_open_tenants,
            "tenant_idle_secs": e.tenant_idle_secs,
            "tenant_memory_budget_mb": e.tenant_memory_budget_mb,
        },
        "tenants": tenants,
    }))
    .into_response()
}

fn credential_kind(c: &Credential) -> &'static str {
    match c {
        Credential::Config => "token",
        Credential::ApiKey { .. } => "api_key",
        Credential::Console { .. } => "console",
        Credential::Session => "session",
        Credential::Dev => "dev",
    }
}

async fn exec_graphs(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "execution.graphs", query_json(q)).await
}

async fn exec_init(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "execution.init_graph", body_or_empty(body)).await
}

async fn exec_vocab(State(s): State<AppState>, Path(graph): Path<String>, headers: HeaderMap) -> Response {
    run(&s, &headers, "execution.vocabulary", json!({ "graph": graph })).await
}

async fn exec_set_vocab(
    State(s): State<AppState>,
    Path(graph): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let mut input = body_or_empty(body);
    if let Value::Object(m) = &mut input {
        m.insert("graph".into(), Value::String(graph));
    }
    run(&s, &headers, "execution.set_vocabulary", input).await
}

async fn ask(State(s): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    run(&s, &headers, "intelli.ask", body).await
}

async fn ask_stream(State(s): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let (c, intelli) = match caller_full(&s, &headers).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let req: AskRequest = match serde_json::from_value(with_workspace(body, &headers)) {
        Ok(r) => r,
        Err(e) => return error_response(&Error::invalid(e)),
    };
    let ctx = crate::observe::trace_ctx(&headers, &c, "http");
    let (rx, trace_id) = match intelli.try_ask_stream_traced(c.principal, req, ctx) {
        Ok(rx) => rx,
        Err(e) => return error_response(&e),
    };
    let events: std::pin::Pin<Box<dyn Stream<Item = Result<SseEvent, Infallible>> + Send>> =
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx).map(|ev| {
            Ok(SseEvent::default().event(ev.kind.clone()).data(serde_json::to_string(&ev).unwrap_or_default()))
        }));
    with_trace_id(Sse::new(events).keep_alive(KeepAlive::default()).into_response(), trace_id.as_deref())
}

// ── data catalog ─────────────────────────────────────────────────────────

async fn connectors_list(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "connector.list", json!({})).await
}

async fn connector_add(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "connector.add", body_or_empty(body)).await
}

async fn connector_remove(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let purge = q.get("purge").is_some_and(|v| v == "true" || v == "1");
    run(&s, &headers, "connector.remove", json!({ "name": name, "purge": purge })).await
}

async fn connector_crawl(State(s): State<AppState>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    run(&s, &headers, "connector.crawl", json!({ "name": name })).await
}

async fn crawl_all(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "connector.crawl", json!({})).await
}

/// Query strings stay strings here (a term like "2024" is text).
fn text_query(q: HashMap<String, String>) -> Value {
    Value::Object(
        q.into_iter()
            .map(|(k, v)| match (k.as_str(), v.parse::<i64>()) {
                ("limit", Ok(n)) => (k, Value::from(n)),
                ("kinds", _) => (k, Value::from(v.split(',').map(str::to_string).collect::<Vec<_>>())),
                _ => (k, Value::String(v)),
            })
            .collect(),
    )
}

async fn catalog_find(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "catalog.find", text_query(q)).await
}

async fn catalog_join(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "catalog.join", text_query(q)).await
}

async fn catalog_metric(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "catalog.metric", text_query(q)).await
}

async fn catalog_rank(State(s): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    run(&s, &headers, "catalog.rank", json!({ "id": id })).await
}

async fn catalog_top(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "catalog.top", text_query(q)).await
}

async fn catalog_rerank(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "catalog.rerank", body_or_empty(body)).await
}

async fn facts_list(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "learned.list", text_query(q)).await
}

async fn fact_verb(
    State(s): State<AppState>,
    Path((id, verb)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let action = match verb.as_str() {
        "certify" => "learned.certify",
        "reject" => "learned.reject",
        other => {
            return error_response(&Error::not_found(format!("fact verb '{other}' (certify, reject)")));
        }
    };
    let mut input = body_or_empty(body);
    if let Value::Object(m) = &mut input {
        m.insert("id".into(), Value::String(id));
    }
    run(&s, &headers, action, input).await
}

async fn ontology_draft(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "ontology.draft", body_or_empty(body)).await
}

async fn ontology_refine(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "ontology.refine", body_or_empty(body)).await
}

async fn ontology_advise(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "ontology.advise", body_or_empty(body)).await
}

async fn ontology_apply(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "ontology.apply_draft", body_or_empty(body)).await
}

async fn object_types(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "objects.types", text_query(q)).await
}

async fn objects_explore(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "objects.explore", body_or_empty(body)).await
}

// ── assurance ────────────────────────────────────────────────────────────

async fn reason_query(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "reason.query", body_or_empty(body)).await
}

async fn reason_certify(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "reason.certify", body_or_empty(body)).await
}

async fn reason_verify(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "reason.verify", body_or_empty(body)).await
}

async fn shapes_validate(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "shapes.validate", body_or_empty(body)).await
}

/// `?checks=a,b` (or repeated values joined by commas) and `?include_dismissed=true`.
async fn schema_lint(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut input = serde_json::Map::new();
    if let Some(c) = q.get("checks") {
        let checks: Vec<&str> = c.split(',').map(str::trim).filter(|c| !c.is_empty()).collect();
        input.insert("checks".into(), json!(checks));
    }
    if let Some(d) = q.get("include_dismissed") {
        input.insert("include_dismissed".into(), json!(matches!(d.as_str(), "true" | "1" | "yes")));
    }
    run(&s, &headers, "schema.lint", Value::Object(input)).await
}

/// Change feed query parameters as `changes.poll` input: list keys split on
/// commas, `limit` a number, the migration flags booleans, everything else
/// (cursors like `42.0` included) a string.
pub(crate) fn changes_query(q: HashMap<String, String>) -> Value {
    const LISTS: &[&str] = &["kinds", "ops", "workspaces", "ids", "rels", "props", "sources"];
    let mut input = serde_json::Map::new();
    for (k, v) in q {
        let value = match k.as_str() {
            k if LISTS.contains(&k) => {
                json!(v.split(',').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>())
            }
            "limit" => v.trim().parse::<u64>().map(Value::from).unwrap_or(Value::String(v)),
            "include_migrations" | "exclude_migrations" => match v.trim() {
                "true" | "1" | "yes" => Value::Bool(true),
                "false" | "0" | "no" => Value::Bool(false),
                _ => Value::String(v),
            },
            _ => Value::String(v),
        };
        input.insert(k, value);
    }
    Value::Object(input)
}

async fn changes_poll(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "changes.poll", changes_query(q)).await
}

async fn changes_head(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "changes.head", json!({})).await
}

async fn changes_capture(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "changes.capture", body_or_empty(body)).await
}

async fn changes_stream_get(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    changes_stream(s, headers, changes_query(q)).await
}

async fn changes_stream_post(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    changes_stream(s, headers, body_or_empty(body)).await
}

/// How long a client waits before reconnecting (the SSE `retry:` field).
const STREAM_RETRY: std::time::Duration = std::time::Duration::from_millis(2000);

/// The change feed as Server-Sent Events. A stream is not an action, so this
/// does what `run` and `dispatch` do before one: authenticate, guard project
/// credentials, and `Intelli::admit`. Every `changes.reauth_secs` the stream
/// authenticates these headers again and ends when the credential is revoked
/// or the principal changed.
async fn changes_stream(s: AppState, headers: HeaderMap, input: Value) -> Response {
    const ACTION: &str = "changes.stream";
    let (c, intelli) = match caller_full(&s, &headers).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let mut input = input;
    if let Err(e) = guard_call(&c.credential, &c.principal, ACTION, &mut input, s.auth.allow_private_urls()).await {
        return error_response(&e);
    }
    if let Err(e) = intelli.admit(&c.principal, ACTION, &input) {
        return error_response(&e);
    }
    let last_event_id = headers.get("last-event-id").and_then(|v| v.to_str().ok()).map(str::to_string);
    let (filter, start) = match onto_intelli::changes::stream_request(input, last_event_id.as_deref()) {
        Ok(r) => r,
        Err(e) => return error_response(&e),
    };
    let reauth: onto_intelli::changes::Reauth = {
        let (auth, headers) = (s.auth.clone(), headers.clone());
        Arc::new(move || {
            let (auth, headers) = (auth.clone(), headers.clone());
            Box::pin(async move {
                use onto_intelli::changes::ReauthError;
                match auth.authenticate(&headers).await {
                    Ok(c) => Ok(c.principal),
                    Err(AuthError::Unauthenticated(m)) => Err(ReauthError::Unauthenticated(m)),
                    Err(AuthError::Forbidden(m)) => Err(ReauthError::Denied(m)),
                    Err(AuthError::Unavailable(m)) => Err(ReauthError::Unavailable(m)),
                }
            })
        })
    };
    let sub = match intelli.subscribe_changes(c.principal, filter, start, Some(reauth)).await {
        Ok(sub) => sub,
        Err(e) => return error_response(&e),
    };
    // The stream holds the tenant weakly: an evicted tenant ends it.
    drop(intelli);
    let first = futures::stream::once(async { Ok(SseEvent::default().retry(STREAM_RETRY)) });
    let frames = sub.into_stream().map(|f| {
        let mut ev = SseEvent::default().event(f.event()).data(f.data().to_string());
        if let Some(id) = f.id() {
            ev = ev.id(id.to_string());
        }
        Ok::<_, Infallible>(ev)
    });
    let events: std::pin::Pin<Box<dyn Stream<Item = Result<SseEvent, Infallible>> + Send>> =
        Box::pin(first.chain(frames));
    Sse::new(events).into_response()
}

/// Outbox query parameters as input: `states` and `ids` split on commas,
/// `since`, `limit` numbers, everything else a string.
fn outbox_query(q: HashMap<String, String>) -> Value {
    let mut input = serde_json::Map::new();
    for (k, v) in q {
        let value = match k.as_str() {
            "states" | "ids" => json!(v.split(',').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>()),
            "since" | "limit" => v.trim().parse::<i64>().map(Value::from).unwrap_or(Value::String(v)),
            _ => Value::String(v),
        };
        input.insert(k, value);
    }
    Value::Object(input)
}

/// `body` with `name` set from the path.
fn named(body: Option<Json<Value>>, name: String) -> Value {
    let mut input = body_or_empty(body);
    if let Value::Object(m) = &mut input {
        m.insert("name".into(), Value::String(name));
    }
    input
}

async fn webhooks_list(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "webhook.list", json!({})).await
}

async fn webhook_add(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "webhook.add", body_or_empty(body)).await
}

async fn webhook_kill(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "webhook.kill", body_or_empty(body)).await
}

async fn webhook_update(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    run(&s, &headers, "webhook.update", named(body, name)).await
}

async fn webhook_remove(State(s): State<AppState>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    run(&s, &headers, "webhook.remove", json!({ "name": name })).await
}

async fn webhook_deliveries(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut input = outbox_query(q);
    input["name"] = Value::String(name);
    run(&s, &headers, "webhook.deliveries", input).await
}

async fn webhook_verb(
    State(s): State<AppState>,
    Path((name, verb)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let action = match verb.as_str() {
        "test" => "webhook.test",
        "rotate_secret" => "webhook.rotate_secret",
        "replay" => "webhook.replay",
        "resume" => "webhook.resume",
        other => {
            return error_response(&Error::not_found(format!(
                "webhook action '{other}' (test, rotate_secret, replay, resume)"
            )));
        }
    };
    run(&s, &headers, action, named(body, name)).await
}

async fn automations_list(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "automation.list", json!({})).await
}

async fn automation_get(State(s): State<AppState>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    run(&s, &headers, "automation.get", json!({ "name": name })).await
}

/// `GET /v1/automations/kill`: the automation named `kill` (the path's POST is the switch).
async fn automation_get_kill(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "automation.get", json!({ "name": "kill" })).await
}

async fn automation_kill(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "automation.kill", body_or_empty(body)).await
}

async fn automation_runs(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut input = serde_json::Map::new();
    for (k, v) in q {
        let value = match k.as_str() {
            "state" | "states" => json!(v.split(',').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>()),
            "since" | "limit" => v.trim().parse::<i64>().map(Value::from).unwrap_or(Value::String(v)),
            _ => Value::String(v),
        };
        input.insert(k, value);
    }
    input.insert("name".into(), Value::String(name));
    run(&s, &headers, "automation.runs", Value::Object(input)).await
}

async fn automation_run(
    State(s): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    run(&s, &headers, "automation.runs", json!({ "name": name, "id": id })).await
}

async fn automation_rerun(
    State(s): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let mut input = body_or_empty(body);
    if let Value::Object(m) = &mut input {
        m.insert("id".into(), Value::String(id));
        m.insert("automation".into(), Value::String(name));
    }
    run(&s, &headers, "automation.rerun", input).await
}

async fn automation_verb(
    State(s): State<AppState>,
    Path((name, verb)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let action = match verb.as_str() {
        "test" => "automation.test",
        "enable" => "automation.enable",
        "disable" => "automation.disable",
        other => {
            return error_response(&Error::not_found(format!("automation action '{other}' (test, enable, disable)")));
        }
    };
    run(&s, &headers, action, named(body, name)).await
}

async fn outbox_list(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    run(&s, &headers, "outbox.list", outbox_query(q)).await
}

async fn writeback_list(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "writeback.list", json!({})).await
}

async fn writeback_add(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    run(&s, &headers, "writeback.add", body_or_empty(body)).await
}

async fn writeback_remove(State(s): State<AppState>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    run(&s, &headers, "writeback.remove", json!({ "name": name })).await
}

async fn writeback_test(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    run(&s, &headers, "writeback.test", named(body, name)).await
}

async fn packs_list(State(s): State<AppState>, headers: HeaderMap) -> Response {
    run(&s, &headers, "pack.list", json!({})).await
}

async fn pack_show(State(s): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    run(&s, &headers, "pack.show", json!({ "id": id })).await
}

async fn pack_verb(
    State(s): State<AppState>,
    Path((id, verb)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let action = match verb.as_str() {
        "plan" => "pack.plan",
        "install" => "pack.install",
        other => return error_response(&Error::not_found(format!("pack action '{other}'"))),
    };
    let mut input = body_or_empty(body);
    if let Value::Object(m) = &mut input {
        m.insert("id".into(), Value::String(id));
    }
    run(&s, &headers, action, input).await
}
