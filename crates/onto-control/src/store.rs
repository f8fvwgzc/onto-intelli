//! The control store: one trait, a PostgreSQL implementation ([`crate::pg`])
//! and [`MemoryControl`] for tests and `engine.storage = "memory"`.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;

use onto_core::time::{new_id, now_ms};
use onto_core::{Error, Result};

use crate::keys::{random_token, sha256_hex};
use crate::model::*;

/// Everything the console keeps outside the tenants' schemas.
#[async_trait]
pub trait ControlStore: Send + Sync {
    // ── users and identities ──────────────────────────────────────────────
    async fn user(&self, id: &str) -> Result<Option<User>>;
    /// Every user, oldest first (the operator CLI).
    async fn users(&self) -> Result<Vec<User>>;
    async fn identity(&self, provider: &str, subject: &str) -> Result<Option<Identity>>;
    /// Find or create the user of an identity and record the login, in one
    /// transaction. An existing identity keeps its user; its claims and
    /// `last_login_at` are updated.
    async fn upsert_login(&self, login: LoginUpsert) -> Result<User>;
    /// Refuse the user's console sessions issued before `at`.
    async fn sign_out_everywhere(&self, user: &str, at: i64) -> Result<()>;

    // ── organizations and members ─────────────────────────────────────────
    /// A new organization with `owner` as its owner. A taken slug is a conflict.
    async fn create_org(&self, name: &str, slug: &str, owner: &str) -> Result<Org>;
    /// By id, or else by slug.
    async fn org(&self, id_or_slug: &str) -> Result<Option<Org>>;
    async fn rename_org(&self, org: &str, name: &str) -> Result<Org>;
    /// Only an organization without active projects; deleted projects and
    /// their keys go with it.
    async fn delete_org(&self, org: &str) -> Result<()>;
    async fn orgs_of(&self, user: &str) -> Result<Vec<(Org, OrgRole)>>;
    async fn role_of(&self, org: &str, user: &str) -> Result<Option<OrgRole>>;
    async fn members(&self, org: &str) -> Result<Vec<MemberView>>;
    /// Add a member, or change an existing member's role (keeps ≥ 1 owner).
    async fn add_member(&self, org: &str, user: &str, role: OrgRole) -> Result<()>;
    /// Change a member's role; demoting the last owner is a conflict.
    async fn set_role(&self, org: &str, user: &str, role: OrgRole) -> Result<()>;
    /// Removing the last owner is a conflict.
    async fn remove_member(&self, org: &str, user: &str) -> Result<()>;
    /// A new invitation and its code (shown once; only its hash is stored).
    async fn invite(&self, inv: NewInvitation) -> Result<(Invitation, String)>;
    /// Pending (not accepted) invitations of an organization, newest first.
    async fn invitations(&self, org: &str) -> Result<Vec<Invitation>>;
    /// A pending, unexpired invitation by its code, with its organization.
    async fn invitation_by_code(&self, code: &str) -> Result<Option<(Invitation, Org)>>;
    /// A pending, unexpired invitation exists for this verified email
    /// (case-insensitive) or with this code hash (SHA-256 hex of the code).
    async fn invitation_pending(&self, verified_email: Option<&str>, code_hash: Option<&str>) -> Result<bool>;
    /// Accept every pending, unexpired invitation for the user's verified
    /// email, and the one whose code hash is `code_hash` (whatever its
    /// email). Returns the organizations joined. Each invitation is accepted
    /// once.
    async fn accept_invitations(&self, user: &User, code_hash: Option<&str>) -> Result<Vec<String>>;
    async fn revoke_invitation(&self, org: &str, id: &str) -> Result<()>;

    // ── projects ──────────────────────────────────────────────────────────
    /// A new project with a random ref (the tenant id).
    async fn create_project(&self, org: &str, name: &str, by: &str) -> Result<Project>;
    /// An existing tenant as a project of `org` (`ref` = tenant id, named
    /// after it); `None` when that ref is a project already.
    async fn adopt_project(&self, org: &str, tenant: &str, by: &str) -> Result<Option<Project>>;
    async fn project(&self, id: &str) -> Result<Option<Project>>;
    async fn project_by_ref(&self, r: &str) -> Result<Option<Project>>;
    /// Active projects of an organization, oldest first.
    async fn projects(&self, org: &str) -> Result<Vec<Project>>;
    /// Active projects of every organization the user belongs to.
    async fn projects_of(&self, user: &str) -> Result<Vec<(Project, OrgRole)>>;
    /// Rename and merge settings (a `null` value removes one); bumps
    /// `settings_version` when settings change.
    async fn update_project(&self, r: &str, patch: ProjectPatch, by: &str) -> Result<Project>;
    /// Mark an active project deleted.
    async fn delete_project(&self, r: &str, by: &str) -> Result<()>;
    /// The user's role in an active project.
    async fn membership(&self, user: &str, project_ref: &str) -> Result<Option<(Project, OrgRole)>>;

    // ── API keys ──────────────────────────────────────────────────────────
    /// Insert a key; the caller computed prefix and hash. A taken prefix is a conflict.
    async fn create_key(&self, k: NewKey) -> Result<ApiKey>;
    async fn key(&self, id: &str) -> Result<Option<ApiKey>>;
    async fn key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>>;
    /// Every key of a project (revoked ones too), newest first.
    async fn keys(&self, project_id: &str) -> Result<Vec<ApiKey>>;
    async fn revoke_key(&self, id: &str, by: &str, at: i64) -> Result<()>;
    /// Let a key expire at `at` (the grace period of a rotation).
    async fn expire_key(&self, id: &str, at: i64) -> Result<()>;
    /// Batched `last_used_at` writes (never moves backwards).
    async fn touch_keys(&self, used: &[(String, i64)]) -> Result<()>;

    // ── OIDC state ────────────────────────────────────────────────────────
    async fn put_login(&self, l: PendingLogin) -> Result<()>;
    /// Single use: the row is deleted; `None` when unknown or expired.
    async fn take_login(&self, state_hash: &str, now: i64) -> Result<Option<PendingLogin>>;
    async fn put_ticket(&self, t: LoginTicket) -> Result<()>;
    async fn take_ticket(&self, ticket_hash: &str, now: i64) -> Result<Option<LoginTicket>>;
    /// Delete expired pending logins and tickets, and usage rows older than
    /// [`USAGE_RETENTION_DAYS`]; returns how many.
    async fn sweep(&self, now: i64) -> Result<u64>;

    // ── LLM settings and usage ────────────────────────────────────────────
    async fn llm(&self, project_id: &str) -> Result<Option<ProjectLlm>>;
    /// Every project's LLM settings (the operator's master key rotation).
    async fn llms(&self) -> Result<Vec<ProjectLlm>>;
    /// Insert or replace a project's LLM settings and bump the project's
    /// `settings_version`, in one transaction.
    async fn put_llm(&self, s: ProjectLlm) -> Result<()>;
    /// Back to the server's provider; bumps `settings_version` when there
    /// were settings. Returns whether there were.
    async fn delete_llm(&self, project_id: &str) -> Result<bool>;
    /// Replace the sealed key `old` with `new` (a rewrap); false when the
    /// stored key is no longer `old`. Leaves `settings_version` alone.
    async fn replace_llm_key(&self, project_id: &str, old: &Sealed, new: &Sealed) -> Result<bool>;
    /// Add each delta's counters to its `(project, day, provider, model)`
    /// row (`INSERT … ON CONFLICT DO UPDATE` adding to the counters).
    async fn add_usage(&self, rows: &[UsageDelta]) -> Result<()>;
    /// Rows with `from_day <= day <= to_day` (`YYYY-MM-DD`), by day, provider, model.
    async fn usage(&self, project_id: &str, from_day: &str, to_day: &str) -> Result<Vec<UsageDay>>;
    /// `(ref, settings_version)` of every active project (the 30 s poll).
    async fn settings_versions(&self) -> Result<Vec<(String, i64)>>;

    // ── audit ─────────────────────────────────────────────────────────────
    async fn audit(&self, e: AuditEntry) -> Result<()>;
    /// An organization's entries, newest first, with `seq < before`.
    async fn audit_log(&self, org: &str, before: Option<i64>, limit: usize) -> Result<Vec<AuditEntry>>;
}

/// Usage rows are kept this many days.
pub const USAGE_RETENTION_DAYS: i64 = 400;

/// The first day kept by [`ControlStore::sweep`] at `now` (`YYYY-MM-DD`).
pub fn usage_horizon(now: i64) -> String {
    onto_core::time::fmt_date(now - USAGE_RETENTION_DAYS * 86_400_000)
}

/// Merge `patch` into the object `base`; a `null` value removes the key.
pub fn merge_settings(base: &Value, patch: &Value) -> Result<Value> {
    let Value::Object(p) = patch else {
        return Err(Error::invalid("settings must be an object"));
    };
    let mut out = base.as_object().cloned().unwrap_or_default();
    for (k, v) in p {
        if v.is_null() {
            out.remove(k);
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Object(out))
}

/// A new invitation code and its stored hash.
pub fn invitation_code() -> (String, String) {
    let code = random_token(24);
    let hash = sha256_hex(code.as_bytes());
    (code, hash)
}

fn same_email(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

// ── in memory ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct State {
    users: BTreeMap<String, User>,
    identities: BTreeMap<(String, String), Identity>,
    orgs: BTreeMap<String, Org>,
    members: BTreeMap<(String, String), Member>,
    invitations: BTreeMap<String, Invitation>,
    projects: BTreeMap<String, Project>,
    keys: BTreeMap<String, ApiKey>,
    logins: HashMap<String, PendingLogin>,
    tickets: HashMap<String, LoginTicket>,
    audit: Vec<AuditEntry>,
    llm: BTreeMap<String, ProjectLlm>,
    usage: BTreeMap<(String, String, String, String), UsageDay>,
}

impl State {
    fn owners(&self, org: &str) -> usize {
        self.members.values().filter(|m| m.org_id == org && m.role == OrgRole::Owner).count()
    }

    fn keeps_an_owner(&self, org: &str, user: &str, new_role: Option<OrgRole>) -> Result<()> {
        let was_owner =
            self.members.get(&(org.to_string(), user.to_string())).is_some_and(|m| m.role == OrgRole::Owner);
        if was_owner && new_role != Some(OrgRole::Owner) && self.owners(org) <= 1 {
            return Err(Error::Conflict("an organization keeps at least one owner".into()));
        }
        Ok(())
    }

    fn project_by_ref(&self, r: &str) -> Option<&Project> {
        self.projects.values().find(|p| p.r#ref == r)
    }

    fn bump(&mut self, project_id: &str) {
        if let Some(p) = self.projects.get_mut(project_id) {
            p.settings_version += 1;
        }
    }
}

/// The control store in memory: tests, demos and `engine.storage = "memory"`.
#[derive(Default)]
pub struct MemoryControl {
    s: Mutex<State>,
}

impl MemoryControl {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ControlStore for MemoryControl {
    async fn user(&self, id: &str) -> Result<Option<User>> {
        Ok(self.s.lock().users.get(id).cloned())
    }

    async fn users(&self) -> Result<Vec<User>> {
        let mut v: Vec<User> = self.s.lock().users.values().cloned().collect();
        v.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(v)
    }

    async fn identity(&self, provider: &str, subject: &str) -> Result<Option<Identity>> {
        Ok(self.s.lock().identities.get(&(provider.to_string(), subject.to_string())).cloned())
    }

    async fn upsert_login(&self, l: LoginUpsert) -> Result<User> {
        let mut s = self.s.lock();
        let key = (l.provider.clone(), l.subject.clone());
        if let Some(id) = s.identities.get_mut(&key) {
            id.last_login_at = l.now;
            id.claims = l.claims.clone();
            id.email = l.email.clone();
            let uid = id.user_id.clone();
            let u = s.users.get_mut(&uid).ok_or_else(|| Error::Storage(format!("identity without user {uid}")))?;
            u.last_login_at = Some(l.now);
            if l.email.is_some() {
                u.email = l.email.clone();
                u.email_verified = l.email_verified;
            }
            if !l.name.is_empty() {
                u.name = l.name.clone();
            }
            if l.avatar_url.is_some() {
                u.avatar_url = l.avatar_url.clone();
            }
            return Ok(u.clone());
        }
        let user = User {
            id: new_id("usr"),
            email: l.email.clone(),
            email_verified: l.email_verified,
            name: l.name.clone(),
            avatar_url: l.avatar_url.clone(),
            created_at: l.now,
            last_login_at: Some(l.now),
            sessions_valid_after: 0,
            disabled: false,
        };
        s.users.insert(user.id.clone(), user.clone());
        s.identities.insert(
            key,
            Identity {
                provider: l.provider,
                subject: l.subject,
                user_id: user.id.clone(),
                email: l.email,
                claims: l.claims,
                created_at: l.now,
                last_login_at: l.now,
            },
        );
        Ok(user)
    }

    async fn sign_out_everywhere(&self, user: &str, at: i64) -> Result<()> {
        let mut s = self.s.lock();
        let u = s.users.get_mut(user).ok_or_else(|| Error::not_found(format!("user {user}")))?;
        u.sessions_valid_after = at;
        Ok(())
    }

    async fn create_org(&self, name: &str, slug: &str, owner: &str) -> Result<Org> {
        if !valid_slug(slug) {
            return Err(Error::invalid(format!("invalid organization slug '{slug}'")));
        }
        let mut s = self.s.lock();
        if s.orgs.values().any(|o| o.slug == slug) {
            return Err(Error::Conflict(format!("organization slug '{slug}' is taken")));
        }
        if !s.users.contains_key(owner) {
            return Err(Error::not_found(format!("user {owner}")));
        }
        let now = now_ms();
        let org =
            Org { id: new_id("org"), slug: slug.into(), name: name.into(), created_at: now, created_by: owner.into() };
        s.orgs.insert(org.id.clone(), org.clone());
        s.members.insert(
            (org.id.clone(), owner.to_string()),
            Member { org_id: org.id.clone(), user_id: owner.into(), role: OrgRole::Owner, created_at: now },
        );
        Ok(org)
    }

    async fn org(&self, id_or_slug: &str) -> Result<Option<Org>> {
        let s = self.s.lock();
        Ok(s.orgs.get(id_or_slug).or_else(|| s.orgs.values().find(|o| o.slug == id_or_slug)).cloned())
    }

    async fn rename_org(&self, org: &str, name: &str) -> Result<Org> {
        let mut s = self.s.lock();
        let o = s.orgs.get_mut(org).ok_or_else(|| Error::not_found(format!("organization {org}")))?;
        o.name = name.to_string();
        Ok(o.clone())
    }

    async fn delete_org(&self, org: &str) -> Result<()> {
        let mut s = self.s.lock();
        if !s.orgs.contains_key(org) {
            return Err(Error::not_found(format!("organization {org}")));
        }
        if s.projects.values().any(|p| p.org_id == org && p.active()) {
            return Err(Error::Conflict("delete the organization's projects first".into()));
        }
        let gone: Vec<String> = s.projects.values().filter(|p| p.org_id == org).map(|p| p.id.clone()).collect();
        s.keys.retain(|_, k| !gone.contains(&k.project_id));
        s.llm.retain(|p, _| !gone.contains(p));
        s.usage.retain(|(p, ..), _| !gone.contains(p));
        s.projects.retain(|_, p| p.org_id != org);
        s.members.retain(|(o, _), _| o != org);
        s.invitations.retain(|_, i| i.org_id != org);
        s.orgs.remove(org);
        Ok(())
    }

    async fn orgs_of(&self, user: &str) -> Result<Vec<(Org, OrgRole)>> {
        let s = self.s.lock();
        let mut v: Vec<(Org, OrgRole)> = s
            .members
            .values()
            .filter(|m| m.user_id == user)
            .filter_map(|m| s.orgs.get(&m.org_id).map(|o| (o.clone(), m.role)))
            .collect();
        v.sort_by(|a, b| (a.0.created_at, &a.0.id).cmp(&(b.0.created_at, &b.0.id)));
        Ok(v)
    }

    async fn role_of(&self, org: &str, user: &str) -> Result<Option<OrgRole>> {
        Ok(self.s.lock().members.get(&(org.to_string(), user.to_string())).map(|m| m.role))
    }

    async fn members(&self, org: &str) -> Result<Vec<MemberView>> {
        let s = self.s.lock();
        let mut v: Vec<MemberView> = s
            .members
            .values()
            .filter(|m| m.org_id == org)
            .filter_map(|m| {
                s.users.get(&m.user_id).map(|u| MemberView {
                    user_id: u.id.clone(),
                    email: u.email.clone(),
                    name: u.name.clone(),
                    avatar_url: u.avatar_url.clone(),
                    role: m.role,
                    joined_at: m.created_at,
                })
            })
            .collect();
        v.sort_by(|a, b| (a.joined_at, &a.user_id).cmp(&(b.joined_at, &b.user_id)));
        Ok(v)
    }

    async fn add_member(&self, org: &str, user: &str, role: OrgRole) -> Result<()> {
        let mut s = self.s.lock();
        if !s.orgs.contains_key(org) {
            return Err(Error::not_found(format!("organization {org}")));
        }
        if !s.users.contains_key(user) {
            return Err(Error::not_found(format!("user {user}")));
        }
        s.keeps_an_owner(org, user, Some(role))?;
        let key = (org.to_string(), user.to_string());
        match s.members.get_mut(&key) {
            Some(m) => m.role = role,
            None => {
                s.members.insert(key, Member { org_id: org.into(), user_id: user.into(), role, created_at: now_ms() });
            }
        }
        Ok(())
    }

    async fn set_role(&self, org: &str, user: &str, role: OrgRole) -> Result<()> {
        let mut s = self.s.lock();
        let key = (org.to_string(), user.to_string());
        if !s.members.contains_key(&key) {
            return Err(Error::not_found(format!("member {user} of {org}")));
        }
        s.keeps_an_owner(org, user, Some(role))?;
        if let Some(m) = s.members.get_mut(&key) {
            m.role = role;
        }
        Ok(())
    }

    async fn remove_member(&self, org: &str, user: &str) -> Result<()> {
        let mut s = self.s.lock();
        let key = (org.to_string(), user.to_string());
        if !s.members.contains_key(&key) {
            return Err(Error::not_found(format!("member {user} of {org}")));
        }
        s.keeps_an_owner(org, user, None)?;
        s.members.remove(&key);
        Ok(())
    }

    async fn invite(&self, inv: NewInvitation) -> Result<(Invitation, String)> {
        if inv.role == OrgRole::Owner {
            return Err(Error::invalid("invitations are for admins, developers and viewers"));
        }
        let mut s = self.s.lock();
        if !s.orgs.contains_key(&inv.org_id) {
            return Err(Error::not_found(format!("organization {}", inv.org_id)));
        }
        let (code, code_hash) = invitation_code();
        let i = Invitation {
            id: new_id("inv"),
            org_id: inv.org_id,
            email: inv.email.trim().to_string(),
            role: inv.role,
            code_hash,
            invited_by: inv.invited_by,
            created_at: inv.now,
            expires_at: inv.now + INVITATION_TTL_MS,
            accepted_at: None,
            accepted_by: None,
        };
        s.invitations.insert(i.id.clone(), i.clone());
        Ok((i, code))
    }

    async fn invitations(&self, org: &str) -> Result<Vec<Invitation>> {
        let s = self.s.lock();
        let mut v: Vec<Invitation> =
            s.invitations.values().filter(|i| i.org_id == org && i.accepted_at.is_none()).cloned().collect();
        v.sort_by(|a, b| (b.created_at, &b.id).cmp(&(a.created_at, &a.id)));
        Ok(v)
    }

    async fn invitation_by_code(&self, code: &str) -> Result<Option<(Invitation, Org)>> {
        let hash = sha256_hex(code.as_bytes());
        let now = now_ms();
        let s = self.s.lock();
        Ok(s.invitations
            .values()
            .find(|i| i.code_hash == hash && i.accepted_at.is_none() && i.expires_at > now)
            .and_then(|i| s.orgs.get(&i.org_id).map(|o| (i.clone(), o.clone()))))
    }

    async fn invitation_pending(&self, verified_email: Option<&str>, code_hash: Option<&str>) -> Result<bool> {
        let now = now_ms();
        let s = self.s.lock();
        Ok(s.invitations.values().any(|i| {
            i.accepted_at.is_none()
                && i.expires_at > now
                && (code_hash == Some(i.code_hash.as_str()) || verified_email.is_some_and(|e| same_email(e, &i.email)))
        }))
    }

    async fn accept_invitations(&self, user: &User, code_hash: Option<&str>) -> Result<Vec<String>> {
        let hash = code_hash;
        let now = now_ms();
        let mut s = self.s.lock();
        let ids: Vec<String> = s
            .invitations
            .values()
            .filter(|i| i.accepted_at.is_none() && i.expires_at > now)
            .filter(|i| {
                hash == Some(i.code_hash.as_str())
                    || (user.email_verified && user.email.as_deref().is_some_and(|e| same_email(e, &i.email)))
            })
            .map(|i| i.id.clone())
            .collect();
        let mut joined = Vec::new();
        for id in ids {
            let Some(i) = s.invitations.get_mut(&id) else { continue };
            i.accepted_at = Some(now);
            i.accepted_by = Some(user.id.clone());
            let (org, role) = (i.org_id.clone(), i.role);
            s.members.entry((org.clone(), user.id.clone())).or_insert(Member {
                org_id: org.clone(),
                user_id: user.id.clone(),
                role,
                created_at: now,
            });
            if !joined.contains(&org) {
                joined.push(org);
            }
        }
        Ok(joined)
    }

    async fn revoke_invitation(&self, org: &str, id: &str) -> Result<()> {
        let mut s = self.s.lock();
        match s.invitations.get(id) {
            Some(i) if i.org_id == org && i.accepted_at.is_none() => {
                s.invitations.remove(id);
                Ok(())
            }
            _ => Err(Error::not_found(format!("invitation {id}"))),
        }
    }

    async fn create_project(&self, org: &str, name: &str, by: &str) -> Result<Project> {
        let mut s = self.s.lock();
        if !s.orgs.contains_key(org) {
            return Err(Error::not_found(format!("organization {org}")));
        }
        let mut r = crate::keys::new_ref();
        while s.project_by_ref(&r).is_some() {
            r = crate::keys::new_ref();
        }
        let p = new_project(org, &r, name, by);
        s.projects.insert(p.id.clone(), p.clone());
        Ok(p)
    }

    async fn adopt_project(&self, org: &str, tenant: &str, by: &str) -> Result<Option<Project>> {
        let mut s = self.s.lock();
        if !s.orgs.contains_key(org) {
            return Err(Error::not_found(format!("organization {org}")));
        }
        if s.project_by_ref(tenant).is_some() {
            return Ok(None);
        }
        let p = new_project(org, tenant, tenant, by);
        s.projects.insert(p.id.clone(), p.clone());
        Ok(Some(p))
    }

    async fn project(&self, id: &str) -> Result<Option<Project>> {
        Ok(self.s.lock().projects.get(id).cloned())
    }

    async fn project_by_ref(&self, r: &str) -> Result<Option<Project>> {
        Ok(self.s.lock().project_by_ref(r).cloned())
    }

    async fn projects(&self, org: &str) -> Result<Vec<Project>> {
        let s = self.s.lock();
        let mut v: Vec<Project> = s.projects.values().filter(|p| p.org_id == org && p.active()).cloned().collect();
        v.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(v)
    }

    async fn projects_of(&self, user: &str) -> Result<Vec<(Project, OrgRole)>> {
        let s = self.s.lock();
        let mut v: Vec<(Project, OrgRole)> = s
            .projects
            .values()
            .filter(|p| p.active())
            .filter_map(|p| s.members.get(&(p.org_id.clone(), user.to_string())).map(|m| (p.clone(), m.role)))
            .collect();
        v.sort_by(|a, b| (a.0.created_at, &a.0.id).cmp(&(b.0.created_at, &b.0.id)));
        Ok(v)
    }

    async fn update_project(&self, r: &str, patch: ProjectPatch, _by: &str) -> Result<Project> {
        let mut s = self.s.lock();
        let p = s
            .projects
            .values_mut()
            .find(|p| p.r#ref == r && p.active())
            .ok_or_else(|| Error::not_found(format!("project {r}")))?;
        if let Some(n) = patch.name {
            p.name = n;
        }
        if let Some(set) = patch.settings {
            let merged = merge_settings(&p.settings, &set)?;
            if merged != p.settings {
                p.settings = merged;
                p.settings_version += 1;
            }
        }
        Ok(p.clone())
    }

    async fn delete_project(&self, r: &str, _by: &str) -> Result<()> {
        let mut s = self.s.lock();
        let p = s
            .projects
            .values_mut()
            .find(|p| p.r#ref == r && p.active())
            .ok_or_else(|| Error::not_found(format!("project {r}")))?;
        p.status = "deleted".into();
        p.deleted_at = Some(now_ms());
        Ok(())
    }

    async fn membership(&self, user: &str, project_ref: &str) -> Result<Option<(Project, OrgRole)>> {
        let s = self.s.lock();
        Ok(s.project_by_ref(project_ref)
            .filter(|p| p.active())
            .and_then(|p| s.members.get(&(p.org_id.clone(), user.to_string())).map(|m| (p.clone(), m.role))))
    }

    async fn create_key(&self, k: NewKey) -> Result<ApiKey> {
        let mut s = self.s.lock();
        if s.keys.values().any(|x| x.prefix == k.prefix) {
            return Err(Error::Conflict(format!("key prefix {} is taken", k.prefix)));
        }
        if !s.projects.contains_key(&k.project_id) {
            return Err(Error::not_found(format!("project {}", k.project_id)));
        }
        let key = new_key(k);
        s.keys.insert(key.id.clone(), key.clone());
        Ok(key)
    }

    async fn key(&self, id: &str) -> Result<Option<ApiKey>> {
        Ok(self.s.lock().keys.get(id).cloned())
    }

    async fn key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>> {
        Ok(self.s.lock().keys.values().find(|k| k.prefix == prefix).cloned())
    }

    async fn keys(&self, project_id: &str) -> Result<Vec<ApiKey>> {
        let s = self.s.lock();
        let mut v: Vec<ApiKey> = s.keys.values().filter(|k| k.project_id == project_id).cloned().collect();
        v.sort_by(|a, b| (b.created_at, &b.id).cmp(&(a.created_at, &a.id)));
        Ok(v)
    }

    async fn revoke_key(&self, id: &str, by: &str, at: i64) -> Result<()> {
        let mut s = self.s.lock();
        let k = s.keys.get_mut(id).ok_or_else(|| Error::not_found(format!("key {id}")))?;
        if k.revoked_at.is_none() {
            k.revoked_at = Some(at);
            k.revoked_by = Some(by.to_string());
        }
        Ok(())
    }

    async fn expire_key(&self, id: &str, at: i64) -> Result<()> {
        let mut s = self.s.lock();
        let k = s.keys.get_mut(id).ok_or_else(|| Error::not_found(format!("key {id}")))?;
        k.expires_at = Some(k.expires_at.map_or(at, |e| e.min(at)));
        Ok(())
    }

    async fn touch_keys(&self, used: &[(String, i64)]) -> Result<()> {
        let mut s = self.s.lock();
        for (id, at) in used {
            if let Some(k) = s.keys.get_mut(id) {
                k.last_used_at = Some(k.last_used_at.map_or(*at, |l| l.max(*at)));
            }
        }
        Ok(())
    }

    async fn put_login(&self, l: PendingLogin) -> Result<()> {
        self.s.lock().logins.insert(l.state_hash.clone(), l);
        Ok(())
    }

    async fn take_login(&self, state_hash: &str, now: i64) -> Result<Option<PendingLogin>> {
        Ok(self.s.lock().logins.remove(state_hash).filter(|l| l.expires_at > now))
    }

    async fn put_ticket(&self, t: LoginTicket) -> Result<()> {
        self.s.lock().tickets.insert(t.ticket_hash.clone(), t);
        Ok(())
    }

    async fn take_ticket(&self, ticket_hash: &str, now: i64) -> Result<Option<LoginTicket>> {
        Ok(self.s.lock().tickets.remove(ticket_hash).filter(|t| t.expires_at > now))
    }

    async fn sweep(&self, now: i64) -> Result<u64> {
        let mut s = self.s.lock();
        let before = s.logins.len() + s.tickets.len();
        s.logins.retain(|_, l| l.expires_at > now);
        s.tickets.retain(|_, t| t.expires_at > now);
        let swept = before - s.logins.len() - s.tickets.len();
        let horizon = usage_horizon(now);
        let rows = s.usage.len();
        s.usage.retain(|(_, day, ..), _| *day >= horizon);
        Ok((swept + rows - s.usage.len()) as u64)
    }

    async fn llm(&self, project_id: &str) -> Result<Option<ProjectLlm>> {
        Ok(self.s.lock().llm.get(project_id).cloned())
    }

    async fn llms(&self) -> Result<Vec<ProjectLlm>> {
        Ok(self.s.lock().llm.values().cloned().collect())
    }

    async fn put_llm(&self, l: ProjectLlm) -> Result<()> {
        let mut s = self.s.lock();
        if !s.projects.contains_key(&l.project_id) {
            return Err(Error::not_found(format!("project {}", l.project_id)));
        }
        let id = l.project_id.clone();
        s.llm.insert(id.clone(), l);
        s.bump(&id);
        Ok(())
    }

    async fn delete_llm(&self, project_id: &str) -> Result<bool> {
        let mut s = self.s.lock();
        let had = s.llm.remove(project_id).is_some();
        if had {
            s.bump(project_id);
        }
        Ok(had)
    }

    async fn replace_llm_key(&self, project_id: &str, old: &Sealed, new: &Sealed) -> Result<bool> {
        let mut s = self.s.lock();
        match s.llm.get_mut(project_id) {
            Some(l) if l.api_key_ct.as_ref() == Some(old) => {
                l.api_key_ct = Some(new.clone());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn add_usage(&self, rows: &[UsageDelta]) -> Result<()> {
        let mut s = self.s.lock();
        for r in rows {
            if !s.projects.contains_key(&r.project_id) {
                continue;
            }
            let key = (r.project_id.clone(), r.day.clone(), r.provider.clone(), r.model.clone());
            s.usage
                .entry(key)
                .or_insert_with(|| UsageDay {
                    project_id: r.project_id.clone(),
                    day: r.day.clone(),
                    provider: r.provider.clone(),
                    model: r.model.clone(),
                    ..Default::default()
                })
                .add(r);
        }
        Ok(())
    }

    async fn usage(&self, project_id: &str, from_day: &str, to_day: &str) -> Result<Vec<UsageDay>> {
        let s = self.s.lock();
        Ok(s.usage
            .values()
            .filter(|u| u.project_id == project_id && u.day.as_str() >= from_day && u.day.as_str() <= to_day)
            .cloned()
            .collect())
    }

    async fn settings_versions(&self) -> Result<Vec<(String, i64)>> {
        let s = self.s.lock();
        Ok(s.projects.values().filter(|p| p.active()).map(|p| (p.r#ref.clone(), p.settings_version)).collect())
    }

    async fn audit(&self, mut e: AuditEntry) -> Result<()> {
        let mut s = self.s.lock();
        e.seq = s.audit.len() as i64 + 1;
        s.audit.push(e);
        Ok(())
    }

    async fn audit_log(&self, org: &str, before: Option<i64>, limit: usize) -> Result<Vec<AuditEntry>> {
        let s = self.s.lock();
        Ok(s.audit
            .iter()
            .rev()
            .filter(|e| e.org_id.as_deref() == Some(org) && before.is_none_or(|b| e.seq < b))
            .take(limit)
            .cloned()
            .collect())
    }
}

pub(crate) fn new_project(org: &str, r: &str, name: &str, by: &str) -> Project {
    Project {
        id: new_id("prj"),
        org_id: org.into(),
        r#ref: r.into(),
        name: name.into(),
        status: "active".into(),
        settings: Value::Object(Default::default()),
        settings_version: 1,
        created_at: now_ms(),
        created_by: by.into(),
        deleted_at: None,
    }
}

pub(crate) fn new_key(k: NewKey) -> ApiKey {
    ApiKey {
        id: new_id("key"),
        project_id: k.project_id,
        name: k.name,
        prefix: k.prefix,
        hash: k.hash,
        scopes: k.scopes,
        workspaces: k.workspaces,
        impersonate: k.impersonate,
        impersonate_roles: k.impersonate_roles,
        created_at: k.created_at,
        created_by: k.created_by,
        expires_at: k.expires_at,
        revoked_at: None,
        revoked_by: None,
        last_used_at: None,
        rotated_from: k.rotated_from,
    }
}
