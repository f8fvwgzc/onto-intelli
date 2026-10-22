//! The console API against a real server on 127.0.0.1:0 (memory storage,
//! memory control store): bootstrap and development sign-in, the role rules
//! of every console route (table-driven), API keys on the data plane,
//! project deletion, sessions, invitations and the abuse limits.

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use onto_control::{Control, ControlOptions, OrgRole};
use onto_intelli::config::Config;
use onto_intelli::{Hub, seed};

const BOOTSTRAP: &str = "bootstrap-token-for-console-tests-0123456789";

struct Srv {
    base: String,
    http: reqwest::Client,
    control: Arc<Control>,
    hub: Arc<Hub>,
    _stop: oneshot::Sender<()>,
}

fn console_config() -> Config {
    let mut cfg = Config::in_memory();
    cfg.engine.tenancy = "multi".into();
    cfg.console.enabled = true;
    cfg.console.public_url = "http://127.0.0.1:50051".into();
    cfg
}

async fn start(dev: bool, bootstrap: Option<&str>) -> Srv {
    let cfg = console_config();
    let opts = ControlOptions { bootstrap_token: bootstrap.map(str::to_string), ..Default::default() };
    let control = Arc::new(Control::memory(cfg.console.clone(), opts).unwrap());
    let hub = Hub::open(cfg).await.unwrap();
    // A tenant that exists before the console: adopted as a project on sign-in.
    seed::load_demo(&hub.tenant("legacy").await.unwrap(), "default").await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let opts = onto_grpc::ServeOptions { dev, control: Some(control.clone()) };
    tokio::spawn(onto_grpc::serve_hub_on(hub.clone(), opts, listener, async {
        let _ = rx.await;
    }));
    let srv = Srv { base: format!("http://{addr}"), http: reqwest::Client::new(), control, hub, _stop: tx };
    // Wait until it answers.
    for _ in 0..50 {
        if srv.http.get(format!("{}/v1/console/auth/providers", srv.base)).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    srv
}

impl Srv {
    async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        tenant: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if let Some(t) = tenant {
            req = req.header("x-onto-tenant", t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    async fn get(&self, path: &str, token: &str) -> (u16, Value) {
        self.call(Method::GET, path, Some(token), None, None).await
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> (u16, Value) {
        self.call(Method::POST, path, Some(token), None, Some(body)).await
    }

    async fn sign_in(&self) -> (String, Value) {
        let (st, v) = self.call(Method::POST, "/v1/console/auth/token", Some(BOOTSTRAP), None, None).await;
        assert_eq!(st, 200, "{v}");
        (v["token"].as_str().unwrap().to_string(), v)
    }
}

#[tokio::test]
async fn bootstrap_and_dev_sign_in() {
    let srv = start(true, Some(BOOTSTRAP)).await;
    let (st, p) = srv.call(Method::GET, "/v1/console/auth/providers", None, None, None).await;
    assert_eq!(st, 200);
    assert_eq!(p, json!({ "providers": [], "bootstrap": true, "dev_mode": true, "signup": true }));
    // The bootstrap token, compared in constant time; a wrong one is 401.
    let (st, _) = srv.call(Method::POST, "/v1/console/auth/token", Some("wrong-token"), None, None).await;
    assert_eq!(st, 401);
    let (token, v) = srv.sign_in().await;
    assert!(token.starts_with("onto1."));
    assert_eq!(v["user"]["name"], "Administrator");
    assert_eq!(v["org"]["slug"], "default");
    // Existing tenants are projects of the default organization (ref = tenant id).
    let (st, me) = srv.get("/v1/console/me", &token).await;
    assert_eq!(st, 200, "{me}");
    assert_eq!(me["orgs"][0]["role"], "owner");
    let refs: Vec<&str> = me["projects"].as_array().unwrap().iter().map(|p| p["ref"].as_str().unwrap()).collect();
    assert_eq!(refs, vec!["legacy"]);
    // The console session reads that project's data with x-onto-tenant.
    let (st, e) = srv
        .call(Method::POST, "/v1/call/entity.get", Some(&token), Some("legacy"), Some(json!({ "id": "project:alpha" })))
        .await;
    assert_eq!(st, 200, "{e}");
    let (st, _) =
        srv.call(Method::POST, "/v1/call/entity.get", Some(&token), None, Some(json!({ "id": "project:alpha" }))).await;
    assert_eq!(st, 403, "a console session names its project");
    let (st, who) = srv.call(Method::GET, "/v1/auth/whoami", Some(&token), Some("legacy"), None).await;
    assert_eq!(st, 200);
    assert_eq!(who["console"]["role"], "owner");
    assert_eq!(who["console"]["org"], v["org"]["id"]);
    assert_eq!(who["credential"], "console");
    assert_eq!(who["tenants"], json!(["legacy"]));
    // Signing in again keeps the user and organization.
    let (_, again) = srv.sign_in().await;
    assert_eq!(again["user"]["id"], v["user"]["id"]);
    assert_eq!(again["org"]["id"], v["org"]["id"]);

    // Development sign-in: dev/dev, owner of `dev`; tenants already adopted stay where they are.
    let (st, d) = srv.call(Method::POST, "/v1/console/auth/dev", None, None, None).await;
    assert_eq!(st, 200, "{d}");
    assert_eq!(d["org"]["slug"], "dev");
    let dev = d["token"].as_str().unwrap();
    let (_, dme) = srv.get("/v1/console/me", dev).await;
    assert!(dme["projects"].as_array().unwrap().is_empty());
    let (st, _) = srv
        .call(Method::POST, "/v1/call/entity.get", Some(dev), Some("legacy"), Some(json!({ "id": "project:alpha" })))
        .await;
    assert_eq!(st, 403, "not a member");
    // Console routes need a console session.
    let (st, _) = srv.call(Method::GET, "/v1/console/me", None, None, None).await;
    assert_eq!(st, 401);

    // Refresh, then sign out everywhere: earlier sessions stop working.
    let (st, r) = srv.post("/v1/console/auth/refresh", &token, json!({})).await;
    assert_eq!(st, 200);
    let fresh = r["token"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (st, _) = srv.post("/v1/console/auth/logout", &fresh, json!({ "everywhere": true })).await;
    assert_eq!(st, 200);
    assert_eq!(srv.get("/v1/console/me", &token).await.0, 401);
    assert_eq!(srv.get("/v1/console/me", &fresh).await.0, 401);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (token, _) = srv.sign_in().await;
    assert_eq!(srv.get("/v1/console/me", &token).await.0, 200);
    // Console sessions do not mint other sessions.
    let (st, _) = srv.call(Method::POST, "/v1/auth/session", Some(&token), Some("legacy"), Some(json!({}))).await;
    assert_eq!(st, 403);
}

#[tokio::test]
async fn console_without_dev_mode_needs_no_config_tokens() {
    let srv = start(false, Some(BOOTSTRAP)).await;
    let (st, p) = srv.call(Method::GET, "/v1/console/auth/providers", None, None, None).await;
    assert_eq!((st, p["dev_mode"].clone()), (200, json!(false)));
    let (st, _) = srv.call(Method::POST, "/v1/console/auth/dev", None, None, None).await;
    assert_eq!(st, 404, "dev sign-in only in dev mode");
    let (st, _) = srv.call(Method::POST, "/v1/call/entity.get", None, Some("legacy"), Some(json!({}))).await;
    assert_eq!(st, 401, "no anonymous data-plane access");
    let (token, _) = srv.sign_in().await;
    let (st, _) = srv
        .call(Method::POST, "/v1/call/entity.get", Some(&token), Some("legacy"), Some(json!({ "id": "project:alpha" })))
        .await;
    assert_eq!(st, 200);
}

#[tokio::test]
async fn startup_rules() {
    let control = |cfg: &Config, bootstrap: Option<&str>| {
        let opts = ControlOptions { bootstrap_token: bootstrap.map(str::to_string), ..Default::default() };
        Control::memory(cfg.console.clone(), opts).unwrap()
    };
    let ok = console_config();
    assert!(onto_grpc::check_console(&ok, Some(&control(&ok, Some(BOOTSTRAP))), false).is_ok());
    assert!(onto_grpc::check_console(&ok, None, false).unwrap_err().to_string().contains("control store"));
    let mut single = console_config();
    single.engine.tenancy = "single".into();
    let err = onto_grpc::check_console(&single, Some(&control(&single, Some(BOOTSTRAP))), false).unwrap_err();
    assert!(err.to_string().contains("multi"), "{err}");
    let err = onto_grpc::check_console(&ok, Some(&control(&ok, None)), false).unwrap_err();
    assert!(err.to_string().contains("sign-in path"), "{err}");
    assert!(onto_grpc::check_console(&ok, Some(&control(&ok, None)), true).is_ok(), "dev mode is a sign-in path");
    let mut remote = console_config();
    remote.console.public_url = "http://onto.example.com".into();
    let err = onto_grpc::check_console(&remote, Some(&control(&remote, Some(BOOTSTRAP))), false).unwrap_err();
    assert!(err.to_string().contains("https"), "{err}");
    // serve_hub_on applies them.
    let hub = Hub::open(single.clone()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let opts = onto_grpc::ServeOptions { dev: false, control: Some(Arc::new(control(&single, Some(BOOTSTRAP)))) };
    assert!(onto_grpc::serve_hub_on(hub, opts, listener, async {}).await.is_err());
    // The control schema is never a tenant.
    let hub = Hub::open(console_config()).await.unwrap();
    assert!(hub.tenant("control").await.err().unwrap().to_string().contains("reserved"));
}

/// One console route, the lowest role allowed, and how to call it.
struct Case {
    name: &'static str,
    min: OrgRole,
    method: Method,
    path: String,
    body: Option<Value>,
    tenant: Option<String>,
}

#[tokio::test]
async fn every_console_route_enforces_its_role() {
    let srv = start(true, Some(BOOTSTRAP)).await;
    let (owner, signed) = srv.sign_in().await;
    let org = signed["org"]["id"].as_str().unwrap().to_string();
    let (st, d) = srv.call(Method::POST, "/v1/console/auth/dev", None, None, None).await;
    assert_eq!(st, 200);
    let dev = d["token"].as_str().unwrap().to_string();
    let dev_id = d["user"]["id"].as_str().unwrap().to_string();
    let owner_id = signed["user"]["id"].as_str().unwrap().to_string();

    // A project with demo data, created through the console.
    let (st, p) = srv
        .post(&format!("/v1/console/orgs/{org}/projects"), &owner, json!({ "name": "Payments", "seed": "demo" }))
        .await;
    assert_eq!(st, 200, "{p}");
    let r = p["ref"].as_str().unwrap().to_string();
    assert_eq!(r.len(), 20);
    assert!(p["seeded"]["entities"].as_u64().unwrap() > 0);
    let (st, e) = srv
        .call(Method::POST, "/v1/call/entity.get", Some(&owner), Some(&r), Some(json!({ "id": "project:alpha" })))
        .await;
    assert_eq!(st, 200, "{e}");
    let (st, _) =
        srv.post(&format!("/v1/console/orgs/{org}/projects"), &owner, json!({ "name": "x", "seed": "nope" })).await;
    assert_eq!(st, 400);

    // Not a member yet: refused everywhere.
    assert_eq!(srv.get(&format!("/v1/console/orgs/{org}"), &dev).await.0, 403);
    assert_eq!(srv.get(&format!("/v1/console/projects/{r}"), &dev).await.0, 403);

    // The dev user joins as a viewer (invitations are accepted at sign-in, stage 2).
    srv.control.store().add_member(&org, &dev_id, OrgRole::Viewer).await.unwrap();
    srv.control.evict_org(&org);

    // A key made by the owner, which a developer may not revoke.
    let (st, k) = srv
        .post(&format!("/v1/console/projects/{r}/keys"), &owner, json!({ "name": "owner-key", "scopes": ["read"] }))
        .await;
    assert_eq!(st, 200, "{k}");
    let owner_key = k["key"]["id"].as_str().unwrap().to_string();

    for (i, role) in [OrgRole::Viewer, OrgRole::Developer, OrgRole::Admin, OrgRole::Owner].into_iter().enumerate() {
        let (st, v) = srv
            .call(
                Method::PATCH,
                &format!("/v1/console/orgs/{org}/members/{dev_id}"),
                Some(&owner),
                None,
                Some(json!({ "role": role })),
            )
            .await;
        assert_eq!(st, 200, "{v}");
        // A project to delete and a key project, fresh for each role.
        let (_, scratch) = srv
            .post(&format!("/v1/console/orgs/{org}/projects"), &owner, json!({ "name": format!("scratch-{i}") }))
            .await;
        let scratch_ref = scratch["ref"].as_str().unwrap().to_string();
        let cases = vec![
            Case {
                name: "org get",
                min: OrgRole::Viewer,
                method: Method::GET,
                path: format!("/v1/console/orgs/{org}"),
                body: None,
                tenant: None,
            },
            Case {
                name: "members",
                min: OrgRole::Viewer,
                method: Method::GET,
                path: format!("/v1/console/orgs/{org}/members"),
                body: None,
                tenant: None,
            },
            Case {
                name: "projects",
                min: OrgRole::Viewer,
                method: Method::GET,
                path: format!("/v1/console/orgs/{org}/projects"),
                body: None,
                tenant: None,
            },
            Case {
                name: "project get",
                min: OrgRole::Viewer,
                method: Method::GET,
                path: format!("/v1/console/projects/{r}"),
                body: None,
                tenant: None,
            },
            Case {
                name: "keys list",
                min: OrgRole::Viewer,
                method: Method::GET,
                path: format!("/v1/console/projects/{r}/keys"),
                body: None,
                tenant: None,
            },
            Case {
                name: "data read",
                min: OrgRole::Viewer,
                method: Method::POST,
                path: "/v1/call/entity.get".into(),
                body: Some(json!({ "id": "project:alpha" })),
                tenant: Some(r.clone()),
            },
            Case {
                name: "data write",
                min: OrgRole::Developer,
                method: Method::POST,
                path: "/v1/call/entity.upsert".into(),
                body: Some(json!({ "id": format!("task:r{i}"), "title": "x" })),
                tenant: Some(r.clone()),
            },
            Case {
                name: "key create",
                min: OrgRole::Developer,
                method: Method::POST,
                path: format!("/v1/console/projects/{r}/keys"),
                body: Some(json!({ "name": format!("k{i}"), "scopes": ["read", "write"] })),
                tenant: None,
            },
            Case {
                name: "admin key",
                min: OrgRole::Admin,
                method: Method::POST,
                path: format!("/v1/console/projects/{r}/keys"),
                body: Some(json!({ "name": format!("a{i}"), "scopes": ["admin"] })),
                tenant: None,
            },
            Case {
                name: "impersonating key",
                min: OrgRole::Admin,
                method: Method::POST,
                path: format!("/v1/console/projects/{r}/keys"),
                body: Some(
                    json!({ "name": format!("i{i}"), "scopes": ["write"], "impersonate": true, "impersonate_roles": ["member"] }),
                ),
                tenant: None,
            },
            Case {
                name: "revoke others' key",
                min: OrgRole::Admin,
                method: Method::DELETE,
                path: format!("/v1/console/projects/{r}/keys/{owner_key}"),
                body: None,
                tenant: None,
            },
            Case {
                name: "project settings",
                min: OrgRole::Admin,
                method: Method::PATCH,
                path: format!("/v1/console/projects/{r}"),
                body: Some(json!({ "settings": { "round": i } })),
                tenant: None,
            },
            Case {
                name: "llm get",
                min: OrgRole::Viewer,
                method: Method::GET,
                path: format!("/v1/console/projects/{r}/llm"),
                body: None,
                tenant: None,
            },
            Case {
                name: "usage",
                min: OrgRole::Viewer,
                method: Method::GET,
                path: format!("/v1/console/projects/{r}/usage"),
                body: None,
                tenant: None,
            },
            Case {
                name: "connect",
                min: OrgRole::Viewer,
                method: Method::GET,
                path: format!("/v1/console/projects/{r}/connect"),
                body: None,
                tenant: None,
            },
            Case {
                name: "llm put",
                min: OrgRole::Admin,
                method: Method::PUT,
                path: format!("/v1/console/projects/{r}/llm"),
                body: Some(json!({ "provider": "none" })),
                tenant: None,
            },
            Case {
                name: "llm test",
                min: OrgRole::Developer,
                method: Method::POST,
                path: format!("/v1/console/projects/{r}/llm/test"),
                body: Some(json!({})),
                tenant: None,
            },
            Case {
                name: "llm delete",
                min: OrgRole::Admin,
                method: Method::DELETE,
                path: format!("/v1/console/projects/{r}/llm"),
                body: None,
                tenant: None,
            },
            Case {
                name: "project create",
                min: OrgRole::Admin,
                method: Method::POST,
                path: format!("/v1/console/orgs/{org}/projects"),
                body: Some(json!({ "name": format!("by-{role}") })),
                tenant: None,
            },
            Case {
                name: "invitations",
                min: OrgRole::Admin,
                method: Method::GET,
                path: format!("/v1/console/orgs/{org}/invitations"),
                body: None,
                tenant: None,
            },
            Case {
                name: "invite",
                min: OrgRole::Admin,
                method: Method::POST,
                path: format!("/v1/console/orgs/{org}/invitations"),
                body: Some(json!({ "email": format!("p{i}@example.com"), "role": "viewer" })),
                tenant: None,
            },
            Case {
                name: "audit",
                min: OrgRole::Admin,
                method: Method::GET,
                path: format!("/v1/console/orgs/{org}/audit"),
                body: None,
                tenant: None,
            },
            Case {
                name: "org rename",
                min: OrgRole::Owner,
                method: Method::PATCH,
                path: format!("/v1/console/orgs/{org}"),
                body: Some(json!({ "name": "Default" })),
                tenant: None,
            },
            Case {
                name: "project delete",
                min: OrgRole::Owner,
                method: Method::DELETE,
                path: format!("/v1/console/projects/{scratch_ref}"),
                body: Some(json!({ "confirm": format!("scratch-{i}") })),
                tenant: None,
            },
        ];
        for c in cases {
            let (st, v) = srv.call(c.method.clone(), &c.path, Some(&dev), c.tenant.as_deref(), c.body.clone()).await;
            let want = if role.at_least(c.min) { 200 } else { 403 };
            assert_eq!(st, want, "{} as {role}: {v}", c.name);
        }
        if role == OrgRole::Admin {
            // The owner key was revoked by the admin; make another for the owner round.
            let (_, k) = srv
                .post(
                    &format!("/v1/console/projects/{r}/keys"),
                    &owner,
                    json!({ "name": "owner-key-2", "scopes": ["read"] }),
                )
                .await;
            let id = k["key"]["id"].as_str().unwrap().to_string();
            let path = format!("/v1/console/projects/{r}/keys/{id}");
            assert_eq!(srv.call(Method::DELETE, &path, Some(&dev), None, None).await.0, 200, "admins revoke any key");
        }
    }
    // Admins (not owners) cannot touch owners; owners keep at least one owner.
    let path = format!("/v1/console/orgs/{org}/members/{owner_id}");
    srv.call(
        Method::PATCH,
        &format!("/v1/console/orgs/{org}/members/{dev_id}"),
        Some(&owner),
        None,
        Some(json!({ "role": "admin" })),
    )
    .await;
    assert_eq!(srv.call(Method::PATCH, &path, Some(&dev), None, Some(json!({ "role": "viewer" }))).await.0, 403);
    assert_eq!(srv.call(Method::DELETE, &path, Some(&dev), None, None).await.0, 403);
    let own = format!("/v1/console/orgs/{org}/members/{owner_id}");
    let (st, v) = srv.call(Method::PATCH, &own, Some(&owner), None, Some(json!({ "role": "admin" }))).await;
    assert_eq!(st, 409, "the last owner stays: {v}");
    assert_eq!(srv.call(Method::DELETE, &own, Some(&owner), None, None).await.0, 409);

    // Developers manage only their own keys (in a project of its own: key creations are rate limited).
    srv.call(
        Method::PATCH,
        &format!("/v1/console/orgs/{org}/members/{dev_id}"),
        Some(&owner),
        None,
        Some(json!({ "role": "developer" })),
    )
    .await;
    let (_, kp) = srv.post(&format!("/v1/console/orgs/{org}/projects"), &owner, json!({ "name": "Keys" })).await;
    let r = kp["ref"].as_str().unwrap().to_string();
    let (_, mine) =
        srv.post(&format!("/v1/console/projects/{r}/keys"), &dev, json!({ "name": "mine", "scopes": ["agent"] })).await;
    let mine = mine["key"]["id"].as_str().unwrap().to_string();
    let (_, theirs) = srv
        .post(&format!("/v1/console/projects/{r}/keys"), &owner, json!({ "name": "theirs", "scopes": ["agent"] }))
        .await;
    let theirs = theirs["key"]["id"].as_str().unwrap().to_string();
    let (st, rot) =
        srv.post(&format!("/v1/console/projects/{r}/keys/{mine}/rotate"), &dev, json!({ "grace_secs": 60 })).await;
    assert_eq!(st, 200, "{rot}");
    assert!(rot["secret"].as_str().unwrap().starts_with("onto_sk_"));
    assert_eq!(rot["key"]["rotated_from"], mine);
    assert_eq!(srv.post(&format!("/v1/console/projects/{r}/keys/{theirs}/rotate"), &dev, json!({})).await.0, 403);
    assert_eq!(
        srv.call(Method::DELETE, &format!("/v1/console/projects/{r}/keys/{theirs}"), Some(&dev), None, None).await.0,
        403
    );
    assert_eq!(
        srv.call(Method::DELETE, &format!("/v1/console/projects/{r}/keys/{mine}"), Some(&dev), None, None).await.0,
        200
    );
    let (st, _) =
        srv.post(&format!("/v1/console/projects/{r}/keys/{mine}/rotate"), &dev, json!({ "grace_secs": 86_401 })).await;
    assert_eq!(st, 400);

    // Keys never show their secret or hash again; the audit log has no secrets.
    let (_, list) = srv.get(&format!("/v1/console/projects/{r}/keys"), &owner).await;
    assert!(list["keys"].as_array().unwrap().iter().all(|k| k.get("hash").is_none() && k.get("secret").is_none()));
    let (st, audit) = srv.get(&format!("/v1/console/orgs/{org}/audit?limit=200"), &owner).await;
    assert_eq!(st, 200);
    let text = audit.to_string();
    assert!(!text.contains(rot["secret"].as_str().unwrap()));
    for action in [
        "key.create",
        "key.rotate",
        "key.revoke",
        "member.role",
        "project.create",
        "project.delete",
        "org.rename",
        "invitation.create",
        "auth.signin",
        "llm.update",
        "llm.delete",
    ] {
        assert!(text.contains(action), "audit has {action}");
    }
}

#[tokio::test]
async fn api_keys_on_the_data_plane() {
    let srv = start(true, Some(BOOTSTRAP)).await;
    let (owner, signed) = srv.sign_in().await;
    let org = signed["org"]["id"].as_str().unwrap().to_string();
    let (_, p) = srv
        .post(&format!("/v1/console/orgs/{org}/projects"), &owner, json!({ "name": "Search", "seed": "demo" }))
        .await;
    let r = p["ref"].as_str().unwrap().to_string();
    let (st, k) = srv
        .post(&format!("/v1/console/projects/{r}/keys"), &owner, json!({ "name": "svc", "scopes": ["read", "write"] }))
        .await;
    assert_eq!(st, 200, "{k}");
    let secret = k["secret"].as_str().unwrap().to_string();
    let prefix = k["key"]["prefix"].as_str().unwrap().to_string();
    assert!(secret.starts_with(&format!("{prefix}_")));

    // The key's project is its tenant; no header needed.
    let (st, e) = srv
        .call(Method::POST, "/v1/call/entity.get", Some(&secret), None, Some(json!({ "id": "project:alpha" })))
        .await;
    assert_eq!(st, 200, "{e}");
    let (st, who) = srv.call(Method::GET, "/v1/auth/whoami", Some(&secret), None, None).await;
    assert_eq!(st, 200);
    assert_eq!((who["tenant"].as_str(), who["credential"].as_str()), (Some(r.as_str()), Some("api_key")));
    assert_eq!(who["principal"]["id"], format!("key:{prefix}"));
    assert_eq!(who["principal"]["roles"], json!(["viewer", "member"]));
    assert_eq!(who["capabilities"]["observe"], false, "only admin keys read traces");
    let (st, _) = srv.call(Method::POST, "/v1/call/entity.get", Some(&secret), Some("legacy"), Some(json!({}))).await;
    assert_eq!(st, 403, "another project's tenant");
    // x-api-key works too; gRPC metadata the same way.
    let resp = srv
        .http
        .post(format!("{}/v1/call/entity.get", srv.base))
        .header("x-api-key", &secret)
        .json(&json!({ "id": "project:alpha" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ch = tonic::transport::Channel::from_shared(srv.base.clone()).unwrap().connect().await.unwrap();
    let mut onto = onto_grpc::pb::ontology_service_client::OntologyServiceClient::new(ch);
    let mut req = tonic::Request::new(onto_grpc::pb::EntityRef { id: "project:alpha".into() });
    req.metadata_mut().insert("authorization", format!("Bearer {secret}").parse().unwrap());
    assert_eq!(onto.get_entity(req).await.unwrap().into_inner().id, "project:alpha");

    // A session the key mints is bound to the key.
    let (st, sess) = srv.call(Method::POST, "/v1/auth/session", Some(&secret), None, Some(json!({}))).await;
    assert_eq!(st, 200, "{sess}");
    assert_eq!(sess["tenant"], r);
    let session = sess["token"].as_str().unwrap().to_string();
    assert_eq!(
        srv.call(Method::POST, "/v1/call/entity.get", Some(&session), None, Some(json!({ "id": "project:alpha" })))
            .await
            .0,
        200
    );

    // Project credentials cannot register connectors that read the server's environment or files.
    let (_, admin) = srv
        .post(&format!("/v1/console/projects/{r}/keys"), &owner, json!({ "name": "ops", "scopes": ["admin"] }))
        .await;
    let admin = admin["secret"].as_str().unwrap().to_string();
    for url in ["env:DATABASE_URL", "/var/lib/repo"] {
        let (st, v) = srv.post("/v1/catalog/connectors", &admin, json!({ "name": "leak", "url": url })).await;
        assert_eq!(st, 403, "{url}: {v}");
        let (st, _) = srv
            .call(
                Method::POST,
                "/v1/call/connector.add",
                Some(&owner),
                Some(&r),
                Some(json!({ "name": "leak", "url": url })),
            )
            .await;
        assert_eq!(st, 403, "console sessions too");
    }
    // PostgreSQL connectors: a TCP host with public addresses only (this
    // server's own database is refused), no socket, no host parameters.
    for url in [
        "postgres://onto:onto@127.0.0.1:5441/onto",
        "postgresql:///onto?host=/var/run/postgresql",
        "postgres://u@8.8.8.8/onto?hostaddr=127.0.0.1",
    ] {
        let (st, v) =
            srv.post("/v1/catalog/connectors", &admin, json!({ "name": "pg", "url": url, "every_secs": 0 })).await;
        assert_eq!(st, 403, "{url}: {v}");
    }
    let (st, v) = srv
        .post(
            "/v1/catalog/connectors",
            &admin,
            json!({ "name": "pg", "url": "postgres://ro@8.8.8.8/wh", "every_secs": 0, "restricted": false }),
        )
        .await;
    assert_eq!((st, &v["restricted"]), (200, &json!(true)), "marked restricted whatever the input said: {v}");
    // Repository connectors: a git URL must be https to a public address
    // (console.allow_private_urls is off here); a local path never.
    for url in ["https://127.0.0.1/acme/platform.git", "git@github.com:acme/platform.git", "/srv/platform"] {
        let spec = json!({ "name": "repo", "kind": "repo", "url": url, "every_secs": 0 });
        let (st, v) = srv.post("/v1/catalog/connectors", &admin, spec).await;
        assert_eq!(st, 403, "{url}: {v}");
    }
    let spec =
        json!({ "name": "platform", "kind": "repo", "url": "https://8.8.8.8/acme/platform.git", "every_secs": 0 });
    let (st, v) = srv.post("/v1/catalog/connectors", &admin, spec).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["kind"], "repo");

    // Unknown prefix and wrong secret: the same 401.
    let (_, unknown) = onto_control::keys::generate();
    let wrong = format!("{prefix}_{}", onto_control::keys::random_token(32));
    let a = srv.call(Method::POST, "/v1/call/entity.get", Some(&unknown), None, Some(json!({}))).await;
    let b = srv.call(Method::POST, "/v1/call/entity.get", Some(&wrong), None, Some(json!({}))).await;
    assert_eq!(a.0, 401);
    assert_eq!(a, b);

    // Revocation is immediate on this server, for the key and its sessions.
    let id = k["key"]["id"].as_str().unwrap();
    let (st, rev) =
        srv.call(Method::DELETE, &format!("/v1/console/projects/{r}/keys/{id}"), Some(&owner), None, None).await;
    assert_eq!(st, 200);
    assert!(rev["key"]["revoked_at"].is_number());
    assert_eq!(srv.call(Method::POST, "/v1/call/entity.get", Some(&secret), None, Some(json!({}))).await, a);
    assert_eq!(srv.call(Method::POST, "/v1/call/entity.get", Some(&session), None, Some(json!({}))).await.0, 401);

    // Rotation without grace: the old secret stops, the new one works.
    let (_, k2) =
        srv.post(&format!("/v1/console/projects/{r}/keys"), &owner, json!({ "name": "rot", "scopes": ["read"] })).await;
    let (old_id, old_secret) =
        (k2["key"]["id"].as_str().unwrap().to_string(), k2["secret"].as_str().unwrap().to_string());
    let (_, rot) = srv.post(&format!("/v1/console/projects/{r}/keys/{old_id}/rotate"), &owner, json!({})).await;
    let new_secret = rot["secret"].as_str().unwrap();
    assert_eq!(srv.call(Method::POST, "/v1/call/entity.get", Some(&old_secret), None, Some(json!({}))).await.0, 401);
    assert_eq!(
        srv.call(Method::POST, "/v1/call/entity.get", Some(new_secret), None, Some(json!({ "id": "project:alpha" })))
            .await
            .0,
        200
    );

    // Deleting the project needs its name; afterwards its keys and members are refused and its data is gone.
    let (st, _) = srv
        .call(
            Method::DELETE,
            &format!("/v1/console/projects/{r}"),
            Some(&owner),
            None,
            Some(json!({ "confirm": "nope" })),
        )
        .await;
    assert_eq!(st, 400);
    let (st, _) = srv
        .call(
            Method::DELETE,
            &format!("/v1/console/projects/{r}"),
            Some(&owner),
            None,
            Some(json!({ "confirm": "Search" })),
        )
        .await;
    assert_eq!(st, 200);
    assert_eq!(srv.call(Method::POST, "/v1/call/entity.get", Some(new_secret), None, Some(json!({}))).await.0, 401);
    assert_eq!(srv.call(Method::POST, "/v1/call/entity.get", Some(&owner), Some(&r), Some(json!({}))).await.0, 403);
    assert_eq!(srv.get(&format!("/v1/console/projects/{r}"), &owner).await.0, 404);
    assert!(!srv.hub.open_tenants().contains(&r), "evicted from the hub");

    // Ten key creations a minute per project, then 429 with retry-after.
    let (_, p2) = srv.post(&format!("/v1/console/orgs/{org}/projects"), &owner, json!({ "name": "Limits" })).await;
    let r2 = p2["ref"].as_str().unwrap().to_string();
    for i in 0..10 {
        let (st, _) = srv
            .post(
                &format!("/v1/console/projects/{r2}/keys"),
                &owner,
                json!({ "name": format!("k{i}"), "scopes": ["read"] }),
            )
            .await;
        assert_eq!(st, 200);
    }
    let resp = srv
        .http
        .post(format!("{}/v1/console/projects/{r2}/keys", srv.base))
        .bearer_auth(&owner)
        .json(&json!({ "name": "k10", "scopes": ["read"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
    assert!(resp.headers().get("retry-after").is_some());
}

#[tokio::test]
async fn organizations_invitations_and_limits() {
    let srv = start(true, Some(BOOTSTRAP)).await;
    let (owner, _) = srv.sign_in().await;
    // A new organization; its creator is the owner; slugs get a random suffix.
    let (st, o) = srv.post("/v1/console/orgs", &owner, json!({ "name": "Acme, Inc." })).await;
    assert_eq!(st, 200, "{o}");
    assert_eq!(o["role"], "owner");
    let slug = o["slug"].as_str().unwrap();
    assert!(slug.starts_with("acme-inc-") && slug.len() == "acme-inc-".len() + 4, "{slug}");
    let org = o["id"].as_str().unwrap().to_string();
    assert_eq!(srv.get(&format!("/v1/console/orgs/{slug}"), &owner).await.0, 200, "by slug too");
    assert_eq!(srv.post("/v1/console/orgs", &owner, json!({ "name": "" })).await.0, 400);

    // Invitations: a link with the public URL; the code shows the organization; revocation.
    let (st, inv) = srv
        .post(
            &format!("/v1/console/orgs/{org}/invitations"),
            &owner,
            json!({ "email": "dana@example.com", "role": "developer" }),
        )
        .await;
    assert_eq!(st, 200, "{inv}");
    let link = inv["link"].as_str().unwrap();
    assert!(link.starts_with("http://127.0.0.1:50051/ui/#/invite?code="), "{link}");
    let code = link.rsplit_once("code=").unwrap().1;
    let (st, info) = srv.call(Method::GET, &format!("/v1/console/invitations/{code}"), None, None, None).await;
    assert_eq!(st, 200);
    assert_eq!((info["org"]["name"].as_str(), info["role"].as_str()), (Some("Acme, Inc."), Some("developer")));
    let (st, _) = srv
        .post(
            &format!("/v1/console/orgs/{org}/invitations"),
            &owner,
            json!({ "email": "x@example.com", "role": "owner" }),
        )
        .await;
    assert_eq!(st, 400, "owners are not invited");
    assert_eq!(
        srv.post(&format!("/v1/console/orgs/{org}/invitations"), &owner, json!({ "email": "nope", "role": "viewer" }))
            .await
            .0,
        400
    );
    let id = inv["invitation"]["id"].as_str().unwrap();
    assert_eq!(
        srv.call(Method::DELETE, &format!("/v1/console/orgs/{org}/invitations/{id}"), Some(&owner), None, None).await.0,
        200
    );
    assert_eq!(srv.call(Method::GET, &format!("/v1/console/invitations/{code}"), None, None, None).await.0, 404);

    // An organization with projects cannot be deleted; without, it can.
    let (_, p) = srv.post(&format!("/v1/console/orgs/{org}/projects"), &owner, json!({ "name": "One" })).await;
    assert_eq!(srv.call(Method::DELETE, &format!("/v1/console/orgs/{org}"), Some(&owner), None, None).await.0, 409);
    let r = p["ref"].as_str().unwrap();
    assert_eq!(
        srv.call(
            Method::DELETE,
            &format!("/v1/console/projects/{r}"),
            Some(&owner),
            None,
            Some(json!({ "confirm": "One" }))
        )
        .await
        .0,
        200
    );
    assert_eq!(srv.call(Method::DELETE, &format!("/v1/console/orgs/{org}"), Some(&owner), None, None).await.0, 200);
    assert_eq!(srv.get(&format!("/v1/console/orgs/{org}"), &owner).await.0, 404);

    // Console request bodies are at most 64 KB.
    let big = "x".repeat(70 * 1024);
    let (st, _) = srv.post("/v1/console/orgs", &owner, json!({ "name": big })).await;
    assert_eq!(st, 413);
}
