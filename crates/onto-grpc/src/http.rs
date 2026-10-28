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
//! * `POST /v1/packs/{id}/install`  install `{mode?, sample?, workspace?, message?}` (agents propose,
//!   and write a sample only with `governance.agent_raw_writes`)
//! * `POST /v1/auth/session`      mint a short-lived browser session for the caller,
//!   or (impersonating tokens) for one of its users: `{act_as, roles, workspaces, entity, tenant, ttl_secs}`.
//!   A session minting a session cannot outlive itself and belongs to the same login (`sid`);
//!   a session minted from a session mints none (403).
//!   `DELETE` signs out: it revokes the presented session's login, every session minted in it.
//! * `POST /v1/auth/session/revoke`  `{jti}`: a config token revokes a session it minted, and every
//!   session minted from that one (a `sid` works too): whatever the tenant for a token of every
//!   tenant (`"*"`), in all of its own tenants for a token limited to some (the answer lists them;
//!   with several, the request still names one with `x-onto-tenant`)
//! * `GET  /v1/auth/whoami`       the caller's principal, tenant and capabilities (and, for
//!   console sessions, `console: {user, role, org}`)
//! * `GET  /v1/admin/tenants`     operators: open tenants, their estimated memory and the limits
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
use axum::routing::{delete, get, post};
use futures::{Stream, StreamExt};
use serde_json::{Value, json};

use onto_intelli::{AskRequest, Error, Hub, Intelli, Principal};

use crate::auth::{Auth, AuthError, Caller, Credential, guard_project_call, workspace_header};

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
    if let Err(e) = guard_project_call(&c.credential, action, &mut input, state.auth.allow_private_urls()).await {
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
    // first one revokes this one too. Only the first session of a login
    // mints: revoking any other session's id then leaves no session minted
    // from it behind.
    let parent = c.session.as_ref();
    if parent.is_some_and(crate::auth::SessionInfo::is_derived) {
        return error_response(&Error::Denied(
            "a session minted from a session cannot mint sessions; mint from the session it came from".into(),
        ));
    }
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
/// one it was minted from and every other one minted in that login. That
/// is the point of signing out (the user's login ends, not one copy of its
/// token), and it gives a derived session no power it lacks: it is the same
/// principal in the same tenant as the rest of its login.
async fn session_logout(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let c = match s.auth.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return auth_error_response(&e),
    };
    let Some((login, exp)) = c.session.as_ref().and_then(|si| Some((si.login()?.to_string(), si.exp))) else {
        return error_response(&Error::invalid("DELETE /v1/auth/session revokes a session token: present one"));
    };
    match s.auth.revoke_session(&login, None, exp).await {
        Ok(()) => Json(json!({ "revoked": login })).into_response(),
        Err(e) => error_response(&e),
    }
}

#[derive(serde::Deserialize)]
struct RevokeIn {
    /// A session's `jti` (which is also the `sid` of the sessions minted
    /// from it), or a `sid`.
    jti: String,
    /// Only for ids minted before ids carried their expiry: Unix seconds the
    /// session expires at (from the mint response). Such an id stays revoked
    /// at least as long as any session could live anyway.
    #[serde(default)]
    expires_at: Option<u64>,
}

/// A service (a config token) revokes a session it minted for a user, and
/// the sessions minted from it. Session ids name no tenant: a token for
/// every tenant revokes the id whatever tenant the request names; a token
/// limited to some tenants revokes it for the sessions of those tenants only.
async fn session_revoke(State(s): State<AppState>, headers: HeaderMap, body: Option<Json<Value>>) -> Response {
    let c = match s.auth.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return auth_error_response(&e),
    };
    let tenants = match &c.credential {
        Credential::Config { tenants } if !tenants.iter().any(|t| t == "*") => Some(tenants.clone()),
        Credential::Config { .. } | Credential::Dev => None,
        _ => {
            return error_response(&Error::Denied(
                "only a service token ([[auth.tokens]]) revokes sessions by id".into(),
            ));
        }
    };
    let input: RevokeIn = match serde_json::from_value(body_or_empty(body)) {
        Ok(i) => i,
        Err(e) => return error_response(&Error::invalid(format!("revoke: {e}"))),
    };
    // Without expires_at, revoke_session keeps an old-format id as long as
    // a session minted now could live (the limit as clamped).
    let exp = input.expires_at.unwrap_or(0);
    match s.auth.revoke_session(&input.jti, tenants.as_deref(), exp).await {
        Ok(()) => match tenants {
            Some(tenants) => Json(json!({ "revoked": input.jti, "tenants": tenants })).into_response(),
            None => Json(json!({ "revoked": input.jti })).into_response(),
        },
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
        },
        "governance": gov,
        "ui": { "title": cfg.ui.title },
    }))
    .into_response()
}

/// Open tenants with their estimated memory: an operator's view of the
/// whole process, so project credentials (API keys, console sessions) and
/// non-admins are refused.
async fn admin_tenants(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let c = match s.auth.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return auth_error_response(&e),
    };
    if c.credential.is_project() || !c.principal.is_privileged() {
        return error_response(&Error::Denied(format!(
            "{} may not list the server's tenants (operators only)",
            c.principal.id
        )));
    }
    let e = &s.hub.config().engine;
    let tenants = s.hub.tenant_stats();
    Json(json!({
        "open": tenants.len(),
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
        Credential::Config { .. } => "token",
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
