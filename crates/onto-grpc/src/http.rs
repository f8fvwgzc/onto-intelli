//! JSON over HTTP/1.1 on the gRPC port, for curl, browsers and scripts.
//!
//! * `GET  /healthz`              liveness
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
//! * `POST /v1/auth/session`      mint a short-lived browser session for the caller,
//!   or (impersonating tokens) for one of its users: `{act_as, roles, workspaces, entity, tenant, ttl_secs}`
//! * `GET  /v1/auth/whoami`       the caller's principal, tenant and capabilities

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

use crate::auth::{Auth, AuthError, workspace_header};

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub auth: Arc<Auth>,
}

pub fn router(hub: Arc<Hub>, auth: Arc<Auth>) -> axum::Router {
    axum::Router::new()
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
        .route("/v1/auth/session", post(session))
        .route("/v1/auth/whoami", get(whoami))
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
        .with_state(AppState { hub, auth })
}

pub fn error_response(e: &Error) -> Response {
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

/// Authenticate and pick the tenant's ontology.
pub async fn caller(state: &AppState, headers: &HeaderMap) -> Result<(Principal, Arc<Intelli>), Box<Response>> {
    let c = state.auth.authenticate(headers).map_err(|e| {
        let (status, code) = match &e {
            AuthError::Unauthenticated(_) => (StatusCode::UNAUTHORIZED, "unauthenticated"),
            AuthError::Forbidden(_) => (StatusCode::FORBIDDEN, "permission_denied"),
        };
        Box::new((status, Json(json!({ "error": { "code": code, "message": e.to_string() } }))).into_response())
    })?;
    let intelli = state.hub.resolve(c.tenant.as_deref()).await.map_err(|e| Box::new(error_response(&e)))?;
    Ok((c.principal, intelli))
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
    let (p, intelli) = match caller(state, headers).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    match intelli.call(&p, action, with_workspace(input, headers)).await {
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
    let (principal, intelli) = match caller(&s, &h).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let _ = intelli;
    let tenant =
        s.auth.authenticate(&h).ok().and_then(|c| c.tenant).or_else(|| Some(s.hub.default_tenant().to_string()));
    let (token, exp) = s.auth.issue_session(principal.clone(), tenant.clone(), input.ttl_secs.unwrap_or(3600));
    Json(json!({ "token": token, "expires_at": exp, "principal": principal, "tenant": tenant })).into_response()
}

async fn whoami(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let (p, intelli) = match caller(&s, &headers).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let onto = intelli.ontology();
    let gov = &onto.options().governance;
    let tenant =
        s.auth.authenticate(&headers).ok().and_then(|c| c.tenant).unwrap_or_else(|| s.hub.default_tenant().to_string());
    let tenants = if p.is_privileged() { s.hub.known_tenants().unwrap_or_default() } else { vec![tenant.clone()] };
    let cfg = intelli.config();
    Json(json!({
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
            "apply_schema": gov.can_apply(&p) && gov.live_edits,
            "approve": gov.can_approve(&p),
            "propose": gov.can_propose(&p),
        },
        "governance": gov,
        "ui": { "title": cfg.ui.title },
    }))
    .into_response()
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
    let (p, intelli) = match caller(&s, &headers).await {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let req: AskRequest = match serde_json::from_value(with_workspace(body, &headers)) {
        Ok(r) => r,
        Err(e) => return error_response(&Error::invalid(e)),
    };
    let rx = intelli.ask_stream(p, req);
    let events: std::pin::Pin<Box<dyn Stream<Item = Result<SseEvent, Infallible>> + Send>> =
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx).map(|ev| {
            Ok(SseEvent::default().event(ev.kind.clone()).data(serde_json::to_string(&ev).unwrap_or_default()))
        }));
    Sse::new(events).keep_alive(KeepAlive::default()).into_response()
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
