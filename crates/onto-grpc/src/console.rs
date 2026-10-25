//! The console API (`/v1/console/…`): sign-in through OpenID Connect and
//! without an IdP (bootstrap token, development mode), sessions, organizations, members, invitations,
//! the audit log, projects and API keys. JSON over HTTP for the browser;
//! every route but sign-in and invitation lookup needs a console session,
//! and every role rule of docs/CONTROL_PLANE.md section 5 is enforced here.
//!
//! * `GET  /v1/console/auth/providers`   `{providers, bootstrap, dev_mode, signup}` (public)
//! * `GET  /v1/console/auth/oidc/{id}/start?binding=&return_to=&invite=`  302 to the IdP (public)
//! * `GET  /v1/console/auth/oidc/callback`  the IdP's redirect; 302 to `/ui/#/auth/callback?ticket=` (public)
//! * `POST /v1/console/auth/exchange`    `{ticket, binding}` → `{token, expires_at, user}` (public)
//! * `POST /v1/console/auth/token`       bootstrap admin sign-in (`authorization: Bearer $ONTO_ADMIN_TOKEN`)
//! * `POST /v1/console/auth/dev`         development sign-in (dev mode only)
//! * `POST /v1/console/auth/refresh`     a fresh session
//! * `POST /v1/console/auth/logout`      `{everywhere?}`
//! * `GET  /v1/console/me`               user, organizations and projects with roles
//! * `GET  /v1/console/invitations/{code}`  organization and role of an invitation (public)
//! * `POST /v1/console/orgs`             `{name}`; `GET/PATCH/DELETE /v1/console/orgs/{org}`
//! * `GET  /v1/console/orgs/{org}/members`; `PATCH/DELETE …/members/{user}`
//! * `GET/POST /v1/console/orgs/{org}/invitations`; `DELETE …/invitations/{id}`
//! * `GET  /v1/console/orgs/{org}/audit?before=&limit=`
//! * `GET/POST /v1/console/orgs/{org}/projects`; `GET/PATCH/DELETE /v1/console/projects/{ref}`
//! * `GET/POST /v1/console/projects/{ref}/keys`; `POST …/keys/{id}/rotate`; `DELETE …/keys/{id}`
//! * `GET/PUT/DELETE /v1/console/projects/{ref}/llm`; `POST …/llm/test`
//! * `GET  /v1/console/projects/{ref}/usage?from=&to=`
//! * `GET  /v1/console/projects/{ref}/connect`  URLs and key prefixes for the Connect page

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use serde::Deserialize;
use serde_json::{Value, json};

use onto_control::{
    AuditEntry, Control, KeyRequest, KeyScope, LlmUpdate, NewInvitation, Org, OrgRole, Project, ProjectPatch,
    StartRequest, User, check_name, keys, slugify,
};
use onto_intelli::core::time::now_ms;
use onto_intelli::{Error, Hub, seed};

use crate::auth::{Auth, AuthError, presented};
use crate::http::{auth_error_response, error_response};

/// Request bodies on console routes.
pub const BODY_LIMIT: usize = 64 * 1024;

#[derive(Clone)]
struct ConsoleState {
    hub: Arc<Hub>,
    auth: Arc<Auth>,
    control: Arc<Control>,
}

pub fn router(hub: Arc<Hub>, auth: Arc<Auth>, control: Arc<Control>) -> axum::Router {
    axum::Router::new()
        .route("/v1/console/auth/providers", get(providers))
        .route("/v1/console/auth/oidc/{id}/start", get(oidc_start))
        .route("/v1/console/auth/oidc/callback", get(oidc_callback))
        .route("/v1/console/auth/exchange", post(exchange))
        .route("/v1/console/auth/token", post(sign_in_token))
        .route("/v1/console/auth/dev", post(sign_in_dev))
        .route("/v1/console/auth/refresh", post(refresh))
        .route("/v1/console/auth/logout", post(logout))
        .route("/v1/console/me", get(me))
        .route("/v1/console/invitations/{code}", get(invitation_info))
        .route("/v1/console/orgs", post(org_create))
        .route("/v1/console/orgs/{org}", get(org_get).patch(org_patch).delete(org_delete))
        .route("/v1/console/orgs/{org}/members", get(members_list))
        .route("/v1/console/orgs/{org}/members/{user}", patch(member_patch).delete(member_delete))
        .route("/v1/console/orgs/{org}/invitations", get(invitations_list).post(invitation_create))
        .route("/v1/console/orgs/{org}/invitations/{id}", delete(invitation_revoke))
        .route("/v1/console/orgs/{org}/audit", get(audit_log))
        .route("/v1/console/orgs/{org}/projects", get(projects_list).post(project_create))
        .route("/v1/console/projects/{ref}", get(project_get).patch(project_patch).delete(project_delete))
        .route("/v1/console/projects/{ref}/keys", get(keys_list).post(key_create))
        .route("/v1/console/projects/{ref}/keys/{id}/rotate", post(key_rotate))
        .route("/v1/console/projects/{ref}/keys/{id}", delete(key_revoke))
        .route("/v1/console/projects/{ref}/llm", get(llm_get).put(llm_put).delete(llm_delete))
        .route("/v1/console/projects/{ref}/llm/test", post(llm_test))
        .route("/v1/console/projects/{ref}/usage", get(usage_get))
        .route("/v1/console/projects/{ref}/connect", get(connect_get))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(ConsoleState { hub, auth, control })
}

// ── responses ─────────────────────────────────────────────────────────────

enum Fail {
    Auth(AuthError),
    Engine(Error),
    /// 429 with `retry-after` (seconds).
    Limited(String, u64),
}

impl From<AuthError> for Fail {
    fn from(e: AuthError) -> Self {
        Fail::Auth(e)
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Engine(e)
    }
}

type Out = std::result::Result<Value, Fail>;

fn reply(r: Out) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(Fail::Auth(e)) => auth_error_response(&e),
        Err(Fail::Engine(e)) => error_response(&e),
        Err(Fail::Limited(msg, retry)) => {
            let mut resp = (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": { "code": "resource_exhausted", "message": msg } })),
            )
                .into_response();
            if let Ok(v) = HeaderValue::from_str(&retry.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
            resp
        }
    }
}

fn denied(msg: impl Into<String>) -> Fail {
    Fail::Auth(AuthError::Forbidden(msg.into()))
}

/// The body as `T`; a missing body is `{}`.
fn body<T: serde::de::DeserializeOwned>(b: Option<Json<Value>>) -> std::result::Result<T, Fail> {
    let v = b.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    serde_json::from_value(v).map_err(|e| Fail::Engine(Error::invalid(e)))
}

/// A serializable value with `role` added.
fn with_role<T: serde::Serialize>(v: &T, role: OrgRole) -> Value {
    let mut out = serde_json::to_value(v).unwrap_or(Value::Null);
    if let Value::Object(m) = &mut out {
        m.insert("role".into(), json!(role));
    }
    out
}

fn user_json(u: &User) -> Value {
    json!({ "id": u.id, "email": u.email, "email_verified": u.email_verified, "name": u.name,
            "avatar_url": u.avatar_url, "created_at": u.created_at, "last_login_at": u.last_login_at })
}

// ── access ────────────────────────────────────────────────────────────────

impl ConsoleState {
    async fn user(&self, h: &HeaderMap) -> std::result::Result<User, Fail> {
        Ok(self.auth.authenticate_console(h).await?.0)
    }

    /// The organization and the user's role there, which must be at least `min`.
    async fn org(&self, user: &User, org: &str, min: OrgRole) -> std::result::Result<(Org, OrgRole), Fail> {
        let o = self.control.store().org(org).await?.ok_or_else(|| Error::not_found(format!("organization {org}")))?;
        let role = self
            .control
            .store()
            .role_of(&o.id, &user.id)
            .await?
            .ok_or_else(|| denied(format!("not a member of organization {org}")))?;
        if !role.at_least(min) {
            return Err(denied(format!("needs the {min} role in organization {} (you are {role})", o.slug)));
        }
        Ok((o, role))
    }

    /// An active project and the user's role in its organization (at least `min`).
    async fn project(&self, user: &User, r: &str, min: OrgRole) -> std::result::Result<(Project, OrgRole), Fail> {
        let p = self
            .control
            .store()
            .project_by_ref(r)
            .await?
            .filter(Project::active)
            .ok_or_else(|| Error::not_found(format!("project {r}")))?;
        let role = self
            .control
            .store()
            .role_of(&p.org_id, &user.id)
            .await?
            .ok_or_else(|| denied(format!("not a member of project {r}")))?;
        if !role.at_least(min) {
            return Err(denied(format!("needs the {min} role in project {r} (you are {role})")));
        }
        Ok((p, role))
    }

    fn session(&self, user: &User) -> Value {
        let (token, exp) = self.auth.issue_console_session(&user.id, self.control.config().session_ttl_secs);
        json!({ "token": token, "expires_at": exp, "user": user_json(user) })
    }

    /// Apply a project's changed settings to its open tenant on this replica
    /// (other replicas poll `settings_version`). A failure is logged: the
    /// change is stored, and the next poll retries.
    async fn refresh(&self, p: &Project) {
        if let Err(e) = self.hub.refresh_tenant(&p.r#ref).await {
            tracing::warn!(project = %p.r#ref, "refreshing tenant settings: {e}");
        }
    }

    /// Tenants the backend already holds, adopted on bootstrap and dev sign-in.
    fn known_tenants(&self) -> Vec<String> {
        match self.hub.known_tenants() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("listing tenants to adopt: {e}");
                Vec::new()
            }
        }
    }
}

// ── sign-in and sessions ──────────────────────────────────────────────────

async fn providers(State(s): State<ConsoleState>) -> Response {
    let providers: Vec<Value> =
        s.control.oidc_providers().into_iter().map(|(id, name)| json!({ "id": id, "name": name })).collect();
    Json(json!({
        "providers": providers,
        "bootstrap": s.control.bootstrap_enabled(),
        "dev_mode": s.auth.dev_mode(),
        "signup": s.control.config().allow_signup,
    }))
    .into_response()
}

/// A 302 that no cache keeps and that sends no `Referer` onwards.
fn found(location: &str) -> Response {
    let mut resp = StatusCode::FOUND.into_response();
    let h = resp.headers_mut();
    match HeaderValue::from_str(location) {
        Ok(v) => {
            h.insert(header::LOCATION, v);
        }
        Err(_) => return error_response(&Error::Other("redirect location is not a valid header".into())),
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

/// The client of a request, for rate limits: the TCP peer, or, when the
/// peer is not a public address (a reverse proxy on loopback or the private
/// network), the last `X-Forwarded-For` entry, which that proxy appended.
/// A client on the internet cannot choose it.
pub(crate) fn client_addr(peer: Option<std::net::IpAddr>, headers: &HeaderMap) -> Option<std::net::IpAddr> {
    let forwarded = || {
        headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .next_back()
            .and_then(|a| a.trim().parse().ok())
    };
    match peer {
        Some(ip) if onto_control::net::public_ip(ip) => Some(ip),
        Some(ip) => forwarded().or(Some(ip)),
        None => forwarded(),
    }
}

async fn oidc_start(
    State(s): State<ConsoleState>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    conn: Option<Extension<tonic::transport::server::TcpConnectInfo>>,
    headers: HeaderMap,
) -> Response {
    let peer = conn.and_then(|Extension(c)| c.remote_addr).map(|a| a.ip());
    if !s.control.allow_oidc_start(client_addr(peer, &headers)) {
        return reply(Err(Fail::Limited(
            format!(
                "{} sign-in starts per minute from one address on this server; retry shortly",
                onto_control::signin::STARTS_PER_MINUTE
            ),
            60,
        )));
    }
    let req = StartRequest {
        binding: q.get("binding").cloned().unwrap_or_default(),
        return_to: q.get("return_to").cloned(),
        invite: q.get("invite").cloned(),
    };
    match s.control.oidc_start(&id, req).await {
        Ok(url) => found(&url),
        Err(e) => reply(Err(e.into())),
    }
}

async fn oidc_callback(State(s): State<ConsoleState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let hub = s.hub.clone();
    let open = move |r: String| {
        let hub = hub.clone();
        // The schema only: a sign-up does not keep a tenant open (it opens on first use).
        Box::pin(async move { hub.provision(&r).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = onto_intelli::Result<()>> + Send>>
    };
    found(&s.control.oidc_callback(&q, &open).await)
}

#[derive(Deserialize)]
struct ExchangeIn {
    ticket: String,
    binding: String,
}

async fn exchange(State(s): State<ConsoleState>, b: Option<Json<Value>>) -> Response {
    reply(
        async {
            let input: ExchangeIn = body(b)?;
            let user = match s.control.exchange_ticket(&input.ticket, &input.binding).await {
                Ok(u) => u,
                Err(Error::Denied(m)) => return Err(AuthError::Unauthenticated(m).into()),
                Err(e) => return Err(e.into()),
            };
            Ok(s.session(&user))
        }
        .await,
    )
}

async fn sign_in_token(State(s): State<ConsoleState>, h: HeaderMap) -> Response {
    reply(
        async {
            if !s.control.bootstrap_enabled() {
                return Err(Error::not_found("bootstrap sign-in (set the console.bootstrap_token_env variable)").into());
            }
            let tok = presented(&h).unwrap_or_default();
            if !s.control.check_bootstrap(tok) {
                return Err(AuthError::Unauthenticated("invalid bootstrap token".into()).into());
            }
            let signed = s.control.sign_in_local(&s.known_tenants()).await?;
            let mut out = s.session(&signed.user);
            out["org"] = json!(signed.org);
            Ok(out)
        }
        .await,
    )
}

async fn sign_in_dev(State(s): State<ConsoleState>) -> Response {
    reply(
        async {
            if !s.auth.dev_mode() {
                return Err(Error::not_found("development sign-in (only with onto serve --dev)").into());
            }
            let signed = s.control.sign_in_dev(&s.known_tenants()).await?;
            let mut out = s.session(&signed.user);
            out["org"] = json!(signed.org);
            Ok(out)
        }
        .await,
    )
}

async fn refresh(State(s): State<ConsoleState>, h: HeaderMap) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            Ok(s.session(&user))
        }
        .await,
    )
}

#[derive(Deserialize, Default)]
struct LogoutIn {
    #[serde(default)]
    everywhere: bool,
}

async fn logout(State(s): State<ConsoleState>, h: HeaderMap, b: Option<Json<Value>>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let input: LogoutIn = body(b)?;
            if input.everywhere {
                s.control.store().sign_out_everywhere(&user.id, now_ms()).await?;
                s.control.evict_user(&user.id);
                s.control.audit(AuditEntry::new(&user.id, "auth.logout_everywhere").target(&user.id)).await?;
            }
            Ok(json!({ "ok": true, "everywhere": input.everywhere }))
        }
        .await,
    )
}

async fn me(State(s): State<ConsoleState>, h: HeaderMap) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let orgs: Vec<Value> =
                s.control.store().orgs_of(&user.id).await?.iter().map(|(o, r)| with_role(o, *r)).collect();
            let projects: Vec<Value> =
                s.control.store().projects_of(&user.id).await?.iter().map(|(p, r)| with_role(p, *r)).collect();
            Ok(json!({ "user": user_json(&user), "orgs": orgs, "projects": projects }))
        }
        .await,
    )
}

async fn invitation_info(State(s): State<ConsoleState>, Path(code): Path<String>) -> Response {
    reply(
        async {
            let (inv, org) =
                s.control.store().invitation_by_code(&code).await?.ok_or_else(|| Error::not_found("invitation"))?;
            Ok(json!({
                "org": { "id": org.id, "name": org.name, "slug": org.slug },
                "role": inv.role, "email": inv.email, "expires_at": inv.expires_at,
            }))
        }
        .await,
    )
}

// ── organizations and members ─────────────────────────────────────────────

#[derive(Deserialize)]
struct NameIn {
    name: String,
}

async fn org_create(State(s): State<ConsoleState>, h: HeaderMap, b: Option<Json<Value>>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let input: NameIn = body(b)?;
            let name = check_name("organization", &input.name)?;
            let base = slugify(&name);
            let mut last = None;
            for _ in 0..5 {
                let slug = format!("{base}-{}", keys::random_alnum(4));
                match s.control.store().create_org(&name, &slug, &user.id).await {
                    Ok(org) => {
                        s.control.evict_user(&user.id);
                        s.control
                            .audit(AuditEntry::new(&user.id, "org.create").org(&org.id).target(&org.id).detail(json!({
                                "name": org.name, "slug": org.slug,
                            })))
                            .await?;
                        return Ok(with_role(&org, OrgRole::Owner));
                    }
                    Err(Error::Conflict(m)) => last = Some(m),
                    Err(e) => return Err(e.into()),
                }
            }
            Err(Error::Conflict(last.unwrap_or_default()).into())
        }
        .await,
    )
}

async fn org_get(State(s): State<ConsoleState>, h: HeaderMap, Path(org): Path<String>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (o, role) = s.org(&user, &org, OrgRole::Viewer).await?;
            Ok(with_role(&o, role))
        }
        .await,
    )
}

async fn org_patch(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path(org): Path<String>,
    b: Option<Json<Value>>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (o, role) = s.org(&user, &org, OrgRole::Owner).await?;
            let input: NameIn = body(b)?;
            let name = check_name("organization", &input.name)?;
            let renamed = s.control.store().rename_org(&o.id, &name).await?;
            s.control
                .audit(
                    AuditEntry::new(&user.id, "org.rename")
                        .org(&o.id)
                        .target(&o.id)
                        .detail(json!({ "from": o.name, "to": name })),
                )
                .await?;
            Ok(with_role(&renamed, role))
        }
        .await,
    )
}

async fn org_delete(State(s): State<ConsoleState>, h: HeaderMap, Path(org): Path<String>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (o, _) = s.org(&user, &org, OrgRole::Owner).await?;
            s.control.store().delete_org(&o.id).await?;
            s.control.evict_org(&o.id);
            s.control
                .audit(
                    AuditEntry::new(&user.id, "org.delete").org(&o.id).target(&o.id).detail(json!({ "slug": o.slug })),
                )
                .await?;
            Ok(json!({ "deleted": o.id }))
        }
        .await,
    )
}

async fn members_list(State(s): State<ConsoleState>, h: HeaderMap, Path(org): Path<String>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (o, _) = s.org(&user, &org, OrgRole::Viewer).await?;
            Ok(json!({ "members": s.control.store().members(&o.id).await? }))
        }
        .await,
    )
}

#[derive(Deserialize)]
struct RoleIn {
    role: OrgRole,
}

async fn member_patch(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path((org, member)): Path<(String, String)>,
    b: Option<Json<Value>>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (o, my_role) = s.org(&user, &org, OrgRole::Admin).await?;
            let input: RoleIn = body(b)?;
            let current = s
                .control
                .store()
                .role_of(&o.id, &member)
                .await?
                .ok_or_else(|| Error::not_found(format!("member {member}")))?;
            if (current == OrgRole::Owner || input.role == OrgRole::Owner) && my_role != OrgRole::Owner {
                return Err(denied("only owners make or demote owners"));
            }
            s.control.store().set_role(&o.id, &member, input.role).await?;
            s.control.evict_org(&o.id);
            s.control
                .audit(
                    AuditEntry::new(&user.id, "member.role")
                        .org(&o.id)
                        .target(&member)
                        .detail(json!({ "from": current, "to": input.role })),
                )
                .await?;
            Ok(json!({ "user_id": member, "role": input.role }))
        }
        .await,
    )
}

async fn member_delete(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path((org, member)): Path<(String, String)>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            // Anyone may leave; removing others takes an admin (an owner for owners).
            let min = if member == user.id { OrgRole::Viewer } else { OrgRole::Admin };
            let (o, my_role) = s.org(&user, &org, min).await?;
            let current = s
                .control
                .store()
                .role_of(&o.id, &member)
                .await?
                .ok_or_else(|| Error::not_found(format!("member {member}")))?;
            if member != user.id && current == OrgRole::Owner && my_role != OrgRole::Owner {
                return Err(denied("only owners remove owners"));
            }
            s.control.store().remove_member(&o.id, &member).await?;
            s.control.evict_org(&o.id);
            s.control
                .audit(
                    AuditEntry::new(&user.id, "member.remove")
                        .org(&o.id)
                        .target(&member)
                        .detail(json!({ "role": current })),
                )
                .await?;
            Ok(json!({ "removed": member }))
        }
        .await,
    )
}

// ── invitations ───────────────────────────────────────────────────────────

async fn invitations_list(State(s): State<ConsoleState>, h: HeaderMap, Path(org): Path<String>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (o, _) = s.org(&user, &org, OrgRole::Admin).await?;
            Ok(json!({ "invitations": s.control.store().invitations(&o.id).await? }))
        }
        .await,
    )
}

#[derive(Deserialize)]
struct InviteIn {
    email: String,
    role: OrgRole,
}

async fn invitation_create(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path(org): Path<String>,
    b: Option<Json<Value>>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (o, _) = s.org(&user, &org, OrgRole::Admin).await?;
            let input: InviteIn = body(b)?;
            let email = input.email.trim();
            if email.len() > 254 || email.chars().any(char::is_whitespace) || !email.contains('@') {
                return Err(Error::invalid("a valid email address is needed").into());
            }
            let (inv, code) = s
                .control
                .store()
                .invite(NewInvitation {
                    org_id: o.id.clone(),
                    email: email.to_string(),
                    role: input.role,
                    invited_by: user.id.clone(),
                    now: now_ms(),
                })
                .await?;
            s.control
                .audit(
                    AuditEntry::new(&user.id, "invitation.create")
                        .org(&o.id)
                        .target(&inv.id)
                        .detail(json!({ "email": inv.email, "role": inv.role })),
                )
                .await?;
            let base = s.control.config().public_url.trim_end_matches('/');
            Ok(json!({ "invitation": inv, "link": format!("{base}/ui/#/invite?code={code}") }))
        }
        .await,
    )
}

async fn invitation_revoke(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path((org, id)): Path<(String, String)>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (o, _) = s.org(&user, &org, OrgRole::Admin).await?;
            s.control.store().revoke_invitation(&o.id, &id).await?;
            s.control.audit(AuditEntry::new(&user.id, "invitation.revoke").org(&o.id).target(&id)).await?;
            Ok(json!({ "revoked": id }))
        }
        .await,
    )
}

async fn audit_log(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path(org): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (o, _) = s.org(&user, &org, OrgRole::Admin).await?;
            let before = q.get("before").and_then(|v| v.parse::<i64>().ok());
            let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(50).clamp(1, 200);
            let entries = s.control.store().audit_log(&o.id, before, limit).await?;
            let next = if entries.len() == limit { entries.last().map(|e| e.seq) } else { None };
            Ok(json!({ "entries": entries, "next": next }))
        }
        .await,
    )
}

// ── projects ──────────────────────────────────────────────────────────────

async fn projects_list(State(s): State<ConsoleState>, h: HeaderMap, Path(org): Path<String>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (o, role) = s.org(&user, &org, OrgRole::Viewer).await?;
            let projects: Vec<Value> =
                s.control.store().projects(&o.id).await?.iter().map(|p| with_role(p, role)).collect();
            Ok(json!({ "projects": projects }))
        }
        .await,
    )
}

#[derive(Deserialize)]
struct ProjectIn {
    name: String,
    #[serde(default)]
    seed: Option<String>,
}

async fn project_create(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path(org): Path<String>,
    b: Option<Json<Value>>,
) -> Response {
    reply(async {
        let user = s.user(&h).await?;
        let (o, role) = s.org(&user, &org, OrgRole::Admin).await?;
        let input: ProjectIn = body(b)?;
        let name = check_name("project", &input.name)?;
        if let Some(seed) = input.seed.as_deref().filter(|v| *v != "demo") {
            return Err(Error::invalid(format!("unknown seed '{seed}' (demo)")).into());
        }
        let max = s.control.config().max_projects_per_org;
        if max > 0 && s.control.store().projects(&o.id).await?.len() >= max {
            return Err(Error::Conflict(format!("organization {} has {max} projects, the most allowed", o.slug)).into());
        }
        let project = s.control.store().create_project(&o.id, &name, &user.id).await?;
        // A project is a tenant: open it now (runs the tenant migrations).
        let intelli = match s.hub.tenant(&project.r#ref).await {
            Ok(i) => i,
            Err(e) => {
                if let Err(e2) = s.control.store().delete_project(&project.r#ref, &user.id).await {
                    tracing::error!(project = %project.r#ref, "marking a project that failed to open deleted: {e2}");
                }
                return Err(e.into());
            }
        };
        let mut seeded = Value::Null;
        if input.seed.is_some() {
            let ws = intelli.default_workspace().to_string();
            let report = seed::load_demo(&intelli, &ws).await?;
            seeded = json!({ "dataset": "demo", "workspace": ws, "entities": report.graph.entities.len(),
                             "links": report.graph.links, "events": report.events });
        }
        s.control
            .audit(
                AuditEntry::new(&user.id, "project.create")
                    .project(&project)
                    .target(&project.r#ref)
                    .detail(json!({ "name": project.name, "seed": input.seed })),
            )
            .await?;
        let mut out = with_role(&project, role);
        out["seeded"] = seeded;
        Ok(out)
    }
    .await)
}

async fn project_get(State(s): State<ConsoleState>, h: HeaderMap, Path(r): Path<String>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, role) = s.project(&user, &r, OrgRole::Viewer).await?;
            Ok(with_role(&p, role))
        }
        .await,
    )
}

async fn project_patch(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path(r): Path<String>,
    b: Option<Json<Value>>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, role) = s.project(&user, &r, OrgRole::Admin).await?;
            let mut input: ProjectPatch = body(b)?;
            if let Some(n) = &input.name {
                input.name = Some(check_name("project", n)?);
            }
            if input.settings.as_ref().is_some_and(|v| !v.is_object()) {
                return Err(Error::invalid("settings must be an object").into());
            }
            if let Some(m) = input.settings.as_ref().and_then(Value::as_object) {
                onto_control::hooks::check_trace_settings(m)?;
            }
            let keys: Vec<String> = input
                .settings
                .as_ref()
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            let detail = json!({ "name": input.name, "settings": keys });
            let updated = s.control.store().update_project(&r, input, &user.id).await?;
            s.control
                .audit(AuditEntry::new(&user.id, "project.update").project(&p).target(&p.r#ref).detail(detail))
                .await?;
            if updated.settings_version != p.settings_version {
                s.refresh(&updated).await;
            }
            Ok(with_role(&updated, role))
        }
        .await,
    )
}

#[derive(Deserialize)]
struct ConfirmIn {
    #[serde(default)]
    confirm: String,
}

async fn project_delete(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path(r): Path<String>,
    b: Option<Json<Value>>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, _) = s.project(&user, &r, OrgRole::Owner).await?;
            let input: ConfirmIn = body(b)?;
            if input.confirm != p.name {
                return Err(Error::invalid("to delete a project, send {\"confirm\": \"<the project name>\"}").into());
            }
            s.control.store().delete_project(&r, &user.id).await?;
            s.control.evict_project(&p);
            s.control
                .audit(
                    AuditEntry::new(&user.id, "project.delete")
                        .project(&p)
                        .target(&p.r#ref)
                        .detail(json!({ "name": p.name })),
                )
                .await?;
            // Evict the tenant and drop its schema with everything in it.
            s.hub.drop_tenant(&p.r#ref)?;
            Ok(json!({ "deleted": p.r#ref }))
        }
        .await,
    )
}

// ── API keys ──────────────────────────────────────────────────────────────

/// Keys with `admin` scope or impersonation need an admin.
fn privileged_key(scopes: &[KeyScope], impersonate: bool) -> bool {
    impersonate || scopes.contains(&KeyScope::Admin)
}

async fn keys_list(State(s): State<ConsoleState>, h: HeaderMap, Path(r): Path<String>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, _) = s.project(&user, &r, OrgRole::Viewer).await?;
            Ok(json!({ "keys": s.control.store().keys(&p.id).await? }))
        }
        .await,
    )
}

async fn key_create(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path(r): Path<String>,
    b: Option<Json<Value>>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, role) = s.project(&user, &r, OrgRole::Developer).await?;
            let req: KeyRequest = body(b)?;
            if privileged_key(&req.scopes, req.impersonate) && !role.at_least(OrgRole::Admin) {
                return Err(denied("keys with the admin scope or impersonation need the admin role"));
            }
            if !s.control.allow_key_creation(&p.id) {
                return Err(Fail::Limited("10 key creations per minute per project; retry shortly".into(), 60));
            }
            let (key, secret) = s.control.create_key(&p, req, &user.id).await?;
            Ok(json!({ "key": key, "secret": secret }))
        }
        .await,
    )
}

#[derive(Deserialize, Default)]
struct RotateIn {
    #[serde(default)]
    grace_secs: u64,
}

async fn key_rotate(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path((r, id)): Path<(String, String)>,
    b: Option<Json<Value>>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, role) = s.project(&user, &r, OrgRole::Developer).await?;
            let input: RotateIn = body(b)?;
            let key = s.control.project_key(&p, &id).await?;
            check_key_owner(&key, &user, role)?;
            if !s.control.allow_key_creation(&p.id) {
                return Err(Fail::Limited("10 key creations per minute per project; retry shortly".into(), 60));
            }
            let (key, secret) = s.control.rotate_key(&p, &id, input.grace_secs, &user.id).await?;
            Ok(json!({ "key": key, "secret": secret }))
        }
        .await,
    )
}

async fn key_revoke(State(s): State<ConsoleState>, h: HeaderMap, Path((r, id)): Path<(String, String)>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, role) = s.project(&user, &r, OrgRole::Developer).await?;
            let key = s.control.project_key(&p, &id).await?;
            check_key_owner(&key, &user, role)?;
            let key = s.control.revoke_key(&p, &id, &user.id).await?;
            Ok(json!({ "key": key }))
        }
        .await,
    )
}

/// Developers rotate and revoke only keys they created (and never admin or
/// impersonating keys); admins and owners any key.
fn check_key_owner(key: &onto_control::ApiKey, user: &User, role: OrgRole) -> std::result::Result<(), Fail> {
    if role.at_least(OrgRole::Admin) {
        return Ok(());
    }
    if key.created_by != user.id {
        return Err(denied("developers manage only the keys they created"));
    }
    if privileged_key(&key.scopes, key.impersonate) {
        return Err(denied("keys with the admin scope or impersonation need the admin role"));
    }
    Ok(())
}

// ── LLM settings and usage ────────────────────────────────────────────────

async fn llm_get(State(s): State<ConsoleState>, h: HeaderMap, Path(r): Path<String>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, _) = s.project(&user, &r, OrgRole::Viewer).await?;
            let mut v = s.control.llm_view(&p).await?;
            let d = s.hub.default_llm();
            v["server_default"] = json!({ "provider": d.name(), "model": d.model() });
            Ok(v)
        }
        .await,
    )
}

async fn llm_put(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path(r): Path<String>,
    b: Option<Json<Value>>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, _) = s.project(&user, &r, OrgRole::Admin).await?;
            let update = LlmUpdate::from_json(b.map(|Json(v)| v).unwrap_or_else(|| json!({})))?;
            s.control.put_llm(&p, update, &user.id, s.auth.dev_mode()).await?;
            s.refresh(&p).await;
            let mut v = s.control.llm_view(&p).await?;
            let d = s.hub.default_llm();
            v["server_default"] = json!({ "provider": d.name(), "model": d.model() });
            Ok(v)
        }
        .await,
    )
}

async fn llm_delete(State(s): State<ConsoleState>, h: HeaderMap, Path(r): Path<String>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, _) = s.project(&user, &r, OrgRole::Admin).await?;
            let had = s.control.delete_llm(&p, &user.id).await?;
            if had {
                s.refresh(&p).await;
            }
            Ok(json!({ "deleted": had }))
        }
        .await,
    )
}

async fn llm_test(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path(r): Path<String>,
    b: Option<Json<Value>>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, _) = s.project(&user, &r, OrgRole::Developer).await?;
            if !s.control.allow_llm_test(&p.id) {
                return Err(Fail::Limited(
                    format!(
                        "{} LLM tests per minute per project; retry shortly",
                        onto_control::llm::LLM_TESTS_PER_MINUTE
                    ),
                    60,
                ));
            }
            let draft = match b.map(|Json(v)| v) {
                Some(v) if v.as_object().is_some_and(|m| !m.is_empty()) => Some(LlmUpdate::from_json(v)?),
                _ => None,
            };
            let out = s.control.test_llm(&p, draft, s.hub.default_llm(), s.auth.dev_mode()).await?;
            Ok(serde_json::to_value(out).map_err(Error::from)?)
        }
        .await,
    )
}

async fn usage_get(
    State(s): State<ConsoleState>,
    h: HeaderMap,
    Path(r): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, _) = s.project(&user, &r, OrgRole::Viewer).await?;
            Ok(s.control.usage_report(&p, q.get("from").map(String::as_str), q.get("to").map(String::as_str)).await?)
        }
        .await,
    )
}

/// `GET …/connect`: what the Connect page fills its snippets with. The data
/// plane, gRPC and MCP share the server's port, so every URL starts with
/// `console.public_url`; keys are the usable ones, by prefix (never a secret).
async fn connect_get(State(s): State<ConsoleState>, h: HeaderMap, Path(r): Path<String>) -> Response {
    reply(
        async {
            let user = s.user(&h).await?;
            let (p, _) = s.project(&user, &r, OrgRole::Viewer).await?;
            let now = now_ms();
            let keys: Vec<Value> = s
                .control
                .store()
                .keys(&p.id)
                .await?
                .iter()
                .filter(|k| k.usable(now))
                .map(|k| json!({ "prefix": k.prefix, "name": k.name, "scopes": k.scopes }))
                .collect();
            let base = s.control.config().public_url.trim_end_matches('/').to_string();
            Ok(json!({
                "api_url": base,
                "grpc_url": base,
                "mcp_url": format!("{base}/mcp"),
                "ref": p.r#ref,
                "keys": keys,
            }))
        }
        .await,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_of_a_request() {
        let h = |xff: &[&str]| {
            let mut m = HeaderMap::new();
            for v in xff {
                m.append("x-forwarded-for", HeaderValue::from_str(v).unwrap());
            }
            m
        };
        let ip = |s: &str| Some(s.parse::<std::net::IpAddr>().unwrap());
        // A public peer is the client, whatever it forwards.
        assert_eq!(client_addr(ip("8.8.8.8"), &h(&["9.9.9.9"])), ip("8.8.8.8"));
        // Behind a proxy on loopback or the private network: the entry it appended.
        assert_eq!(client_addr(ip("127.0.0.1"), &h(&["1.1.1.1, 8.8.8.8"])), ip("8.8.8.8"));
        assert_eq!(client_addr(ip("10.0.0.2"), &h(&["1.1.1.1", "8.8.4.4"])), ip("8.8.4.4"));
        assert_eq!(client_addr(ip("10.0.0.2"), &h(&[])), ip("10.0.0.2"));
        assert_eq!(client_addr(ip("10.0.0.2"), &h(&["garbage"])), ip("10.0.0.2"));
        assert_eq!(client_addr(None, &h(&[])), None);
    }
}
