//! One test body for every `ControlStore`: `MemoryControl` always, and
//! `PgControl` on its own schema (`onto_control_test_<random>`, dropped at the
//! end, also on failure) when `DATABASE_URL` or `ONTO_TEST_DATABASE_URL` is set.

use std::sync::Arc;

use serde_json::json;

use onto_control::keys::sha256_hex;
use onto_control::store::{ControlStore, MemoryControl};
use onto_control::*;
use onto_core::sqlx;
use onto_core::time::now_ms;
use onto_core::{Error, Role};

fn database_url() -> Option<String> {
    ["ONTO_TEST_DATABASE_URL", "DATABASE_URL"].iter().find_map(|v| std::env::var(v).ok().filter(|u| !u.is_empty()))
}

/// The URL holds the password: what the control store reports leaves it out.
#[tokio::test]
async fn a_bad_control_store_url_is_reported_without_its_password() {
    let url = "postgres://bob:s3cret-pw@db.example:notaport/x";
    let m = PgControl::connect(url, "control").await.err().expect("the port is no number").to_string();
    assert!(m.contains("control store: connecting to PostgreSQL"), "{m}");
    assert!(!m.contains("s3cret-pw"), "{m}");
}

/// Drops the schema when the test ends, whichever way it ends.
struct DropSchema {
    url: String,
    schema: String,
}

impl Drop for DropSchema {
    fn drop(&mut self) {
        let (url, schema) = (self.url.clone(), self.schema.clone());
        // Drop runs inside the test's runtime: use a thread with its own.
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap();
                sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE")))
                    .execute(&pool)
                    .await
                    .unwrap();
            });
        })
        .join();
    }
}

fn test_schema() -> String {
    format!("onto_control_test_{}", new_ref())
}

fn login(provider: &str, subject: &str, email: Option<&str>, verified: bool) -> LoginUpsert {
    LoginUpsert {
        provider: provider.into(),
        subject: subject.into(),
        email: email.map(str::to_string),
        email_verified: verified,
        name: subject.to_uppercase(),
        avatar_url: None,
        claims: json!({ "name": subject }),
        now: now_ms(),
    }
}

async fn exercise(s: Arc<dyn ControlStore>) {
    // Users and identities.
    let alice = s.upsert_login(login("clerk", "alice", Some("Alice@Example.com"), true)).await.unwrap();
    assert!(alice.id.starts_with("usr_"));
    let again = s.upsert_login(login("clerk", "alice", Some("alice@example.com"), true)).await.unwrap();
    assert_eq!(again.id, alice.id, "an identity keeps its user");
    let id = s.identity("clerk", "alice").await.unwrap().unwrap();
    assert_eq!((id.user_id.as_str(), id.claims["name"].as_str()), (alice.id.as_str(), Some("alice")));
    let other_idp = s.upsert_login(login("google", "alice", Some("alice@example.com"), true)).await.unwrap();
    assert_ne!(other_idp.id, alice.id, "accounts are not linked across providers by email");
    let bob = s.upsert_login(login("clerk", "bob", Some("bob@example.com"), false)).await.unwrap();
    let carol = s.upsert_login(login("clerk", "carol", Some("carol@example.com"), true)).await.unwrap();
    assert_eq!(s.users().await.unwrap().len(), 4);
    s.sign_out_everywhere(&alice.id, 1234).await.unwrap();
    assert_eq!(s.user(&alice.id).await.unwrap().unwrap().sessions_valid_after, 1234);
    assert!(s.user("usr_none").await.unwrap().is_none());

    // Organizations and members; the last owner stays.
    let acme = s.create_org("Acme", "acme-1234", &alice.id).await.unwrap();
    assert!(matches!(s.create_org("Acme 2", "acme-1234", &bob.id).await, Err(Error::Conflict(_))));
    assert!(s.create_org("Bad", "Bad Slug", &bob.id).await.is_err());
    assert_eq!(s.org("acme-1234").await.unwrap().unwrap().id, acme.id);
    assert_eq!(s.org(&acme.id).await.unwrap().unwrap().slug, "acme-1234");
    assert_eq!(s.rename_org(&acme.id, "Acme Inc").await.unwrap().name, "Acme Inc");
    assert_eq!(s.role_of(&acme.id, &alice.id).await.unwrap(), Some(OrgRole::Owner));
    assert!(matches!(s.set_role(&acme.id, &alice.id, OrgRole::Admin).await, Err(Error::Conflict(_))));
    assert!(matches!(s.remove_member(&acme.id, &alice.id).await, Err(Error::Conflict(_))));
    s.add_member(&acme.id, &bob.id, OrgRole::Developer).await.unwrap();
    s.set_role(&acme.id, &bob.id, OrgRole::Owner).await.unwrap();
    s.set_role(&acme.id, &alice.id, OrgRole::Admin).await.unwrap();
    assert!(matches!(s.remove_member(&acme.id, &bob.id).await, Err(Error::Conflict(_))), "bob is the last owner");
    s.set_role(&acme.id, &alice.id, OrgRole::Owner).await.unwrap();
    s.remove_member(&acme.id, &bob.id).await.unwrap();
    assert!(matches!(s.set_role(&acme.id, &bob.id, OrgRole::Viewer).await, Err(Error::NotFound(_))));
    let members = s.members(&acme.id).await.unwrap();
    assert_eq!(
        members.iter().map(|m| (m.user_id.as_str(), m.role)).collect::<Vec<_>>(),
        vec![(alice.id.as_str(), OrgRole::Owner)]
    );
    assert_eq!(s.orgs_of(&alice.id).await.unwrap()[0].0.id, acme.id);

    // Invitations: by verified email or by code, unexpired, once.
    let now = now_ms();
    let new_inv = |email: &str, role: OrgRole, now: i64| NewInvitation {
        org_id: acme.id.clone(),
        email: email.into(),
        role,
        invited_by: alice.id.clone(),
        now,
    };
    assert!(s.invite(new_inv("x@example.com", OrgRole::Owner, now)).await.is_err(), "owners are not invited");
    let (by_email, code1) = s.invite(new_inv("CAROL@example.com", OrgRole::Developer, now)).await.unwrap();
    let (_for_bob, _) = s.invite(new_inv("bob@example.com", OrgRole::Viewer, now)).await.unwrap();
    let (_old, old_code) = s.invite(new_inv("carol@example.com", OrgRole::Admin, now - 8 * 86_400_000)).await.unwrap();
    let (by_code, code2) = s.invite(new_inv("someone-else@example.com", OrgRole::Viewer, now)).await.unwrap();
    assert_eq!(s.invitations(&acme.id).await.unwrap().len(), 4);
    let (shown, org) = s.invitation_by_code(&code1).await.unwrap().unwrap();
    assert_eq!((shown.id.as_str(), org.name.as_str()), (by_email.id.as_str(), "Acme Inc"));
    assert!(s.invitation_by_code(&old_code).await.unwrap().is_none(), "expired");
    assert!(s.invitation_by_code("nope").await.unwrap().is_none());
    // What a closed sign-up checks: a pending invitation by email or code.
    assert!(s.invitation_pending(Some("Carol@Example.com"), None).await.unwrap());
    assert!(s.invitation_pending(None, Some(&sha256_hex(code2.as_bytes()))).await.unwrap());
    assert!(!s.invitation_pending(Some("nobody@example.com"), Some(&sha256_hex(b"nope"))).await.unwrap());
    assert!(!s.invitation_pending(None, Some(&sha256_hex(old_code.as_bytes()))).await.unwrap(), "expired");
    assert!(!s.invitation_pending(None, None).await.unwrap());
    // Bob's email is not verified: his invitation is not accepted by email.
    assert!(s.accept_invitations(&bob, None).await.unwrap().is_empty());
    // Carol's is: the unexpired one; the expired admin invitation is not.
    assert_eq!(s.accept_invitations(&carol, None).await.unwrap(), vec![acme.id.clone()]);
    assert_eq!(s.role_of(&acme.id, &carol.id).await.unwrap(), Some(OrgRole::Developer));
    assert!(s.invitation_by_code(&code1).await.unwrap().is_none(), "accepted once");
    assert!(s.accept_invitations(&carol, Some(&sha256_hex(code1.as_bytes()))).await.unwrap().is_empty(), "single use");
    // A code proves the link whatever the email.
    assert_eq!(s.accept_invitations(&bob, Some(&sha256_hex(code2.as_bytes()))).await.unwrap(), vec![acme.id.clone()]);
    assert_eq!(s.role_of(&acme.id, &bob.id).await.unwrap(), Some(OrgRole::Viewer));
    assert!(s.accept_invitations(&other_idp, Some(&sha256_hex(code2.as_bytes()))).await.unwrap().is_empty());
    assert!(s.invitations(&acme.id).await.unwrap().iter().all(|i| i.id != by_code.id));
    let pending: Vec<String> = s.invitations(&acme.id).await.unwrap().into_iter().map(|i| i.id).collect();
    for id in &pending {
        s.revoke_invitation(&acme.id, id).await.unwrap();
    }
    assert!(s.invitations(&acme.id).await.unwrap().is_empty());
    assert!(matches!(s.revoke_invitation(&acme.id, &by_code.id).await, Err(Error::NotFound(_))));

    // Projects.
    let p = s.create_project(&acme.id, "Payments", &alice.id).await.unwrap();
    assert_eq!(p.r#ref.len(), 20);
    assert!(p.r#ref.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
    assert!(p.active());
    let adopted = s.adopt_project(&acme.id, "legacy_tenant", &alice.id).await.unwrap().unwrap();
    assert_eq!((adopted.r#ref.as_str(), adopted.name.as_str()), ("legacy_tenant", "legacy_tenant"));
    assert!(s.adopt_project(&acme.id, "legacy_tenant", &alice.id).await.unwrap().is_none());
    assert_eq!(s.projects(&acme.id).await.unwrap().len(), 2);
    assert_eq!(s.project(&p.id).await.unwrap().unwrap().r#ref, p.r#ref);
    assert_eq!(s.project_by_ref(&p.r#ref).await.unwrap().unwrap().id, p.id);
    let patched = s
        .update_project(
            &p.r#ref,
            ProjectPatch { name: Some("Payments API".into()), settings: Some(json!({ "default_workspace": "pay" })) },
            &alice.id,
        )
        .await
        .unwrap();
    assert_eq!((patched.name.as_str(), patched.settings_version), ("Payments API", 2));
    let same = s
        .update_project(
            &p.r#ref,
            ProjectPatch { settings: Some(json!({ "default_workspace": "pay" })), ..Default::default() },
            &alice.id,
        )
        .await
        .unwrap();
    assert_eq!(same.settings_version, 2, "no change, no bump");
    let removed = s
        .update_project(
            &p.r#ref,
            ProjectPatch { settings: Some(json!({ "default_workspace": null })), ..Default::default() },
            &alice.id,
        )
        .await
        .unwrap();
    assert_eq!((removed.settings.clone(), removed.settings_version), (json!({}), 3));
    let (mp, role) = s.membership(&carol.id, &p.r#ref).await.unwrap().unwrap();
    assert_eq!((mp.id.as_str(), role), (p.id.as_str(), OrgRole::Developer));
    assert!(s.membership(&other_idp.id, &p.r#ref).await.unwrap().is_none(), "not a member");
    assert_eq!(s.projects_of(&carol.id).await.unwrap().len(), 2);

    // API keys.
    let new_key = |prefix: &str| NewKey {
        project_id: p.id.clone(),
        name: "ci".into(),
        prefix: prefix.into(),
        hash: "ab".repeat(32),
        scopes: vec![KeyScope::Read, KeyScope::Agent],
        workspaces: vec!["pay".into()],
        impersonate: true,
        impersonate_roles: vec![Role::Viewer],
        created_by: carol.id.clone(),
        created_at: now_ms(),
        expires_at: None,
        rotated_from: None,
    };
    let k = s.create_key(new_key("onto_sk_aaaaaaaa")).await.unwrap();
    assert!(matches!(s.create_key(new_key("onto_sk_aaaaaaaa")).await, Err(Error::Conflict(_))));
    let got = s.key_by_prefix("onto_sk_aaaaaaaa").await.unwrap().unwrap();
    assert_eq!(got, k);
    assert_eq!(got.scopes, vec![KeyScope::Read, KeyScope::Agent]);
    assert_eq!(got.impersonate_roles, vec![Role::Viewer]);
    assert!(s.key_by_prefix("onto_sk_zzzzzzzz").await.unwrap().is_none());
    let mut rotated = new_key("onto_sk_bbbbbbbb");
    rotated.rotated_from = Some(k.id.clone());
    let k2 = s.create_key(rotated).await.unwrap();
    assert_eq!(s.keys(&p.id).await.unwrap().len(), 2);
    s.expire_key(&k.id, 5_000).await.unwrap();
    s.expire_key(&k.id, 9_000).await.unwrap();
    assert_eq!(s.key(&k.id).await.unwrap().unwrap().expires_at, Some(5_000), "expiry only moves earlier");
    s.revoke_key(&k2.id, &alice.id, 7_000).await.unwrap();
    s.revoke_key(&k2.id, &bob.id, 8_000).await.unwrap();
    let r = s.key(&k2.id).await.unwrap().unwrap();
    assert_eq!((r.revoked_at, r.revoked_by.as_deref()), (Some(7_000), Some(alice.id.as_str())));
    assert!(!r.usable(now_ms()));
    s.touch_keys(&[(k.id.clone(), 100), (k2.id.clone(), 200)]).await.unwrap();
    s.touch_keys(&[(k.id.clone(), 50)]).await.unwrap();
    assert_eq!(s.key(&k.id).await.unwrap().unwrap().last_used_at, Some(100), "never backwards");
    assert!(matches!(s.revoke_key("key_none", "x", 1).await, Err(Error::NotFound(_))));

    // LLM settings: put replaces and bumps settings_version, delete goes back.
    let v0 = s.project(&p.id).await.unwrap().unwrap().settings_version;
    assert!(s.llm(&p.id).await.unwrap().is_none());
    let llm = ProjectLlm {
        project_id: p.id.clone(),
        provider: "openai-compatible".into(),
        model: "m1".into(),
        base_url: Some("https://llm.example.com/v1".into()),
        api_key_ct: Some(Sealed(vec![1, 2, 3])),
        api_key_last4: Some("abcd".into()),
        max_output_tokens: 512,
        temperature: Some(0.5),
        effort: None,
        timeout_secs: 30,
        pricing: onto_intelli::config::Pricing {
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
            cached_input_per_mtok: 0.1,
        },
        headers: [("x-team".to_string(), "core".to_string())].into(),
        monthly_budget_usd: Some(50.0),
        monthly_token_budget: Some(1_000_000),
        asks_per_minute: Some(60),
        updated_at: 7,
        updated_by: alice.id.clone(),
    };
    s.put_llm(llm.clone()).await.unwrap();
    assert_eq!(s.llm(&p.id).await.unwrap(), Some(llm.clone()));
    assert_eq!(s.project(&p.id).await.unwrap().unwrap().settings_version, v0 + 1);
    let changed = ProjectLlm { model: "m2".into(), api_key_ct: None, api_key_last4: None, ..llm.clone() };
    s.put_llm(changed.clone()).await.unwrap();
    assert_eq!(s.llm(&p.id).await.unwrap(), Some(changed));
    assert_eq!(s.llms().await.unwrap().len(), 1);
    assert!(s.settings_versions().await.unwrap().contains(&(p.r#ref.clone(), v0 + 2)));
    assert!(matches!(
        s.put_llm(ProjectLlm { project_id: "prj_none".into(), ..llm.clone() }).await,
        Err(Error::NotFound(_))
    ));
    // A rewrap replaces only the key it was computed from.
    s.put_llm(llm.clone()).await.unwrap();
    assert!(!s.replace_llm_key(&p.id, &Sealed(vec![9]), &Sealed(vec![4])).await.unwrap());
    assert!(s.replace_llm_key(&p.id, &Sealed(vec![1, 2, 3]), &Sealed(vec![4, 5])).await.unwrap());
    assert_eq!(s.llm(&p.id).await.unwrap().unwrap().api_key_ct, Some(Sealed(vec![4, 5])));
    let v3 = s.project(&p.id).await.unwrap().unwrap().settings_version;
    assert_eq!(v3, v0 + 3, "a rewrap leaves settings_version alone");
    assert!(s.delete_llm(&p.id).await.unwrap());
    assert!(!s.delete_llm(&p.id).await.unwrap());
    assert_eq!(s.project(&p.id).await.unwrap().unwrap().settings_version, v3 + 1, "bumped once");
    s.put_llm(llm.clone()).await.unwrap();

    // Usage adds to the counters of its (project, day, provider, model) row.
    let day = |d: &str, model: &str, cost: f64| UsageDelta {
        project_id: p.id.clone(),
        day: d.into(),
        provider: "openai".into(),
        model: model.into(),
        asks: 1,
        llm_calls: 1,
        deterministic: 0,
        input_tokens: 100,
        output_tokens: 10,
        cached_tokens: 5,
        cost_usd: cost,
        errors: 0,
    };
    s.add_usage(&[day("2026-10-01", "a", 0.25), day("2026-10-01", "a", 0.25), day("2026-10-02", "b", 1.0)])
        .await
        .unwrap();
    s.add_usage(&[
        day("2026-10-01", "a", 0.5),
        UsageDelta { project_id: "prj_gone".into(), ..day("2026-10-01", "a", 9.0) },
    ])
    .await
    .unwrap();
    let rows = s.usage(&p.id, "2026-10-01", "2026-10-31").await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].day.as_str(), rows[0].model.as_str()), ("2026-10-01", "a"));
    assert_eq!((rows[0].asks, rows[0].input_tokens, rows[0].cached_tokens), (3, 300, 15));
    assert!((rows[0].cost_usd - 1.0).abs() < 1e-9);
    assert_eq!(s.usage(&p.id, "2026-10-02", "2026-10-02").await.unwrap().len(), 1);
    assert!(s.usage(&p.id, "2026-09-01", "2026-09-30").await.unwrap().is_empty());
    // Rows older than 400 days are swept.
    s.add_usage(&[day("2020-01-01", "old", 1.0)]).await.unwrap();
    assert!(s.sweep(now_ms()).await.unwrap() >= 1);
    assert!(s.usage(&p.id, "2019-01-01", "2020-12-31").await.unwrap().is_empty());

    // Deleted projects disappear from membership and listings.
    s.delete_project(&p.r#ref, &alice.id).await.unwrap();
    assert!(s.membership(&carol.id, &p.r#ref).await.unwrap().is_none());
    assert_eq!(s.projects(&acme.id).await.unwrap().len(), 1);
    assert_eq!(s.project_by_ref(&p.r#ref).await.unwrap().unwrap().status, "deleted");
    assert!(matches!(s.delete_project(&p.r#ref, &alice.id).await, Err(Error::NotFound(_))));
    assert!(matches!(s.delete_org(&acme.id).await, Err(Error::Conflict(_))), "an active project is left");
    s.delete_project("legacy_tenant", &alice.id).await.unwrap();

    // OIDC state: single use, expiry honoured.
    let pending = |state: &str, exp: i64| PendingLogin {
        state_hash: state.into(),
        provider: "clerk".into(),
        nonce: "n".into(),
        code_verifier: "v".into(),
        binding_hash: "b".into(),
        invite_code_hash: Some("i".into()),
        return_to: "#/".into(),
        created_at: 1,
        expires_at: exp,
    };
    s.put_login(pending("s1", 10_000)).await.unwrap();
    assert_eq!(s.take_login("s1", 5_000).await.unwrap(), Some(pending("s1", 10_000)));
    assert!(s.take_login("s1", 5_000).await.unwrap().is_none(), "single use");
    s.put_login(pending("s2", 10_000)).await.unwrap();
    assert!(s.take_login("s2", 10_001).await.unwrap().is_none(), "expired");
    let ticket = |t: &str, exp: i64| LoginTicket {
        ticket_hash: t.into(),
        user_id: alice.id.clone(),
        binding_hash: "b".into(),
        expires_at: exp,
    };
    s.put_ticket(ticket("t1", 10_000)).await.unwrap();
    assert_eq!(s.take_ticket("t1", 1).await.unwrap(), Some(ticket("t1", 10_000)));
    assert!(s.take_ticket("t1", 1).await.unwrap().is_none());
    s.put_login(pending("s3", 10_000)).await.unwrap();
    s.put_ticket(ticket("t2", 20_000)).await.unwrap();
    assert_eq!(s.sweep(15_000).await.unwrap(), 1);
    assert_eq!(s.sweep(25_000).await.unwrap(), 1);

    // Audit log, newest first, paged by seq.
    for i in 0..5 {
        s.audit(AuditEntry::new(&alice.id, "test.event").org(&acme.id).detail(json!({ "i": i }))).await.unwrap();
    }
    s.audit(AuditEntry::new("system", "elsewhere").org("org_other")).await.unwrap();
    let page = s.audit_log(&acme.id, None, 3).await.unwrap();
    assert_eq!(page.iter().map(|e| e.detail["i"].as_i64().unwrap()).collect::<Vec<_>>(), vec![4, 3, 2]);
    let next = s.audit_log(&acme.id, Some(page[2].seq), 10).await.unwrap();
    assert_eq!(next.iter().map(|e| e.detail["i"].as_i64().unwrap()).collect::<Vec<_>>(), vec![1, 0]);

    // An organization without active projects can go; its deleted projects go with it.
    s.delete_org(&acme.id).await.unwrap();
    assert!(s.org(&acme.id).await.unwrap().is_none());
    assert!(s.project_by_ref(&p.r#ref).await.unwrap().is_none());
    assert!(s.key(&k.id).await.unwrap().is_none());
    assert!(s.llm(&p.id).await.unwrap().is_none(), "LLM settings go with their project");
    assert!(s.usage(&p.id, "2026-01-01", "2026-12-31").await.unwrap().is_empty());
    assert!(s.orgs_of(&alice.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn memory_control_store() {
    exercise(Arc::new(MemoryControl::new())).await;
}

#[tokio::test]
async fn postgres_control_store() {
    let Some(url) = database_url() else {
        eprintln!("DATABASE_URL not set: skipping the PostgreSQL control store test");
        return;
    };
    let schema = test_schema();
    let _guard = DropSchema { url: url.clone(), schema: schema.clone() };
    let pg = PgControl::connect(&url, &schema).await.unwrap();
    exercise(Arc::new(pg)).await;
}

#[tokio::test]
async fn postgres_control_migrations() {
    let Some(url) = database_url() else { return };
    let schema = test_schema();
    let _guard = DropSchema { url: url.clone(), schema: schema.clone() };
    let pg = PgControl::connect(&url, &schema).await.unwrap();
    // Applied once and recorded; connecting again applies nothing.
    let first = pg.migrations().await.unwrap();
    assert_eq!(
        first.iter().map(|m| (m.0, m.1.as_str())).collect::<Vec<_>>(),
        vec![(1, "control"), (2, "llm_usage"), (3, "audit_no_delete")]
    );
    let again = PgControl::connect(&url, &schema).await.unwrap();
    assert_eq!(again.migrations().await.unwrap(), first);
    // The audit log is append-only.
    pg.audit(AuditEntry::new("system", "x").org("org_1")).await.unwrap();
    let err = sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE \"{schema}\".audit_log SET actor = 'mallory'")))
        .execute(pg.pool())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("append-only"), "{err}");
    // DELETE and TRUNCATE are refused too: nobody erases what they did.
    for stmt in ["DELETE FROM \"{s}\".audit_log", "TRUNCATE \"{s}\".audit_log"] {
        let err = sqlx::query(sqlx::AssertSqlSafe(stmt.replace("{s}", &schema))).execute(pg.pool()).await.unwrap_err();
        assert!(err.to_string().contains("append-only"), "{stmt}: {err}");
    }
    assert_eq!(pg.audit_log("org_1", None, 10).await.unwrap().len(), 1);
    // A schema newer than this build is refused.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO \"{schema}\".schema_migrations (version, name, applied_at) VALUES (999, 'future', 0)"
    )))
    .execute(pg.pool())
    .await
    .unwrap();
    let err = PgControl::connect(&url, &schema).await.err().unwrap();
    assert!(err.to_string().contains("newer than this build"), "{err}");
    assert!(PgControl::connect(&url, "Bad-Schema").await.is_err());
}
