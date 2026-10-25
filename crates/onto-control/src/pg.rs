//! The control store on PostgreSQL: one schema (`console.schema`, default
//! `onto_control`) with its own versioned migrations, on a small pool of its
//! own created on the server's runtime, so every query is a plain `async`
//! sqlx call.

use std::time::Duration;

use async_trait::async_trait;
use onto_core::sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use onto_core::sqlx::{self, AssertSqlSafe, Row};
use serde_json::Value;

use onto_core::time::{new_id, now_ms};
use onto_core::{Error, Result, Role};

use crate::keys::sha256_hex;
use crate::model::*;
use crate::store::{ControlStore, invitation_code, merge_settings, usage_horizon};

/// Control-store migrations embedded in the binary, applied in order.
/// `{schema}` is the quoted control schema. Never edit a released
/// migration; add the next one.
pub const MIGRATIONS: &[(i32, &str, &str)] = &[
    (1, "control", include_str!("../migrations/0001_control.sql")),
    (2, "llm_usage", include_str!("../migrations/0002_llm_usage.sql")),
    (3, "audit_no_delete", include_str!("../migrations/0003_audit_no_delete.sql")),
];

/// Connections of the control pool.
pub const POOL_SIZE: u32 = 4;

fn sql(s: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(s)
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `[a-z_][a-z0-9_]*`, at most 63 bytes.
pub fn valid_schema(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).is_some_and(|c| c == "23505")
}

fn conflict_or(e: sqlx::Error, what: impl FnOnce() -> String) -> Error {
    if unique_violation(&e) { Error::Conflict(what()) } else { e.into() }
}

fn json_of(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|e| Error::Storage(format!("corrupt control row: {e}")))
}

const USER_COLS: &str =
    "id, email, email_verified, name, avatar_url, created_at, last_login_at, sessions_valid_after, disabled";
const PROJECT_COLS: &str =
    "id, org_id, ref, name, status, settings::text AS settings, settings_version, created_at, created_by, deleted_at";
const KEY_COLS: &str = "id, project_id, name, prefix, hash, scopes, workspaces, impersonate, impersonate_roles, \
                        created_at, created_by, expires_at, revoked_at, revoked_by, last_used_at, rotated_from";
const INVITATION_COLS: &str =
    "id, org_id, email, role, code_hash, invited_by, created_at, expires_at, accepted_at, accepted_by";
const ORG_COLS: &str = "id, slug, name, created_at, created_by";
const LLM_COLS: &str = "project_id, provider, model, base_url, api_key_ct, api_key_last4, max_output_tokens, \
                        temperature, effort, timeout_secs, pricing::text AS pricing, headers::text AS headers, \
                        monthly_budget_usd, monthly_token_budget, asks_per_minute, updated_at, updated_by";
const USAGE_COLS: &str = "project_id, day::text AS day, provider, model, asks, llm_calls, deterministic, \
                          input_tokens, output_tokens, cached_tokens, cost_usd, errors";

fn user_of(r: &PgRow) -> Result<User> {
    Ok(User {
        id: r.try_get("id")?,
        email: r.try_get("email")?,
        email_verified: r.try_get("email_verified")?,
        name: r.try_get("name")?,
        avatar_url: r.try_get("avatar_url")?,
        created_at: r.try_get("created_at")?,
        last_login_at: r.try_get("last_login_at")?,
        sessions_valid_after: r.try_get("sessions_valid_after")?,
        disabled: r.try_get("disabled")?,
    })
}

fn org_of(r: &PgRow) -> Result<Org> {
    Ok(Org {
        id: r.try_get("id")?,
        slug: r.try_get("slug")?,
        name: r.try_get("name")?,
        created_at: r.try_get("created_at")?,
        created_by: r.try_get("created_by")?,
    })
}

fn role_of_text(s: &str) -> Result<OrgRole> {
    OrgRole::parse(s).map_err(|e| Error::Storage(format!("corrupt control row: {e}")))
}

fn project_of(r: &PgRow) -> Result<Project> {
    let settings: String = r.try_get("settings")?;
    Ok(Project {
        id: r.try_get("id")?,
        org_id: r.try_get("org_id")?,
        r#ref: r.try_get("ref")?,
        name: r.try_get("name")?,
        status: r.try_get("status")?,
        settings: json_of(&settings)?,
        settings_version: r.try_get("settings_version")?,
        created_at: r.try_get("created_at")?,
        created_by: r.try_get("created_by")?,
        deleted_at: r.try_get("deleted_at")?,
    })
}

fn key_of(r: &PgRow) -> Result<ApiKey> {
    let scopes: Vec<String> = r.try_get("scopes")?;
    let roles: Vec<String> = r.try_get("impersonate_roles")?;
    let corrupt = |e: Error| Error::Storage(format!("corrupt control row: {e}"));
    Ok(ApiKey {
        id: r.try_get("id")?,
        project_id: r.try_get("project_id")?,
        name: r.try_get("name")?,
        prefix: r.try_get("prefix")?,
        hash: r.try_get("hash")?,
        scopes: scopes.iter().map(|s| KeyScope::parse(s)).collect::<Result<_>>().map_err(corrupt)?,
        workspaces: r.try_get("workspaces")?,
        impersonate: r.try_get("impersonate")?,
        impersonate_roles: roles.iter().map(|s| Role::parse(s)).collect::<Result<_>>().map_err(corrupt)?,
        created_at: r.try_get("created_at")?,
        created_by: r.try_get("created_by")?,
        expires_at: r.try_get("expires_at")?,
        revoked_at: r.try_get("revoked_at")?,
        revoked_by: r.try_get("revoked_by")?,
        last_used_at: r.try_get("last_used_at")?,
        rotated_from: r.try_get("rotated_from")?,
    })
}

fn invitation_of(r: &PgRow) -> Result<Invitation> {
    let role: String = r.try_get("role")?;
    Ok(Invitation {
        id: r.try_get("id")?,
        org_id: r.try_get("org_id")?,
        email: r.try_get("email")?,
        role: role_of_text(&role)?,
        code_hash: r.try_get("code_hash")?,
        invited_by: r.try_get("invited_by")?,
        created_at: r.try_get("created_at")?,
        expires_at: r.try_get("expires_at")?,
        accepted_at: r.try_get("accepted_at")?,
        accepted_by: r.try_get("accepted_by")?,
    })
}

fn llm_of(r: &PgRow) -> Result<ProjectLlm> {
    let pricing: String = r.try_get("pricing")?;
    let headers: String = r.try_get("headers")?;
    let ct: Option<Vec<u8>> = r.try_get("api_key_ct")?;
    let corrupt = |e: serde_json::Error| Error::Storage(format!("corrupt control row: {e}"));
    let max: i32 = r.try_get("max_output_tokens")?;
    let timeout: i32 = r.try_get("timeout_secs")?;
    Ok(ProjectLlm {
        project_id: r.try_get("project_id")?,
        provider: r.try_get("provider")?,
        model: r.try_get("model")?,
        base_url: r.try_get("base_url")?,
        api_key_ct: ct.map(Sealed),
        api_key_last4: r.try_get("api_key_last4")?,
        max_output_tokens: max.max(1) as u32,
        temperature: r.try_get("temperature")?,
        effort: r.try_get("effort")?,
        timeout_secs: timeout.max(1) as u32,
        pricing: serde_json::from_str(&pricing).map_err(corrupt)?,
        headers: serde_json::from_str(&headers).map_err(corrupt)?,
        monthly_budget_usd: r.try_get("monthly_budget_usd")?,
        monthly_token_budget: r.try_get("monthly_token_budget")?,
        asks_per_minute: r.try_get("asks_per_minute")?,
        updated_at: r.try_get("updated_at")?,
        updated_by: r.try_get("updated_by")?,
    })
}

fn usage_of(r: &PgRow) -> Result<UsageDay> {
    Ok(UsageDay {
        project_id: r.try_get("project_id")?,
        day: r.try_get("day")?,
        provider: r.try_get("provider")?,
        model: r.try_get("model")?,
        asks: r.try_get("asks")?,
        llm_calls: r.try_get("llm_calls")?,
        deterministic: r.try_get("deterministic")?,
        input_tokens: r.try_get("input_tokens")?,
        output_tokens: r.try_get("output_tokens")?,
        cached_tokens: r.try_get("cached_tokens")?,
        cost_usd: r.try_get("cost_usd")?,
        errors: r.try_get("errors")?,
    })
}

fn audit_of(r: &PgRow) -> Result<AuditEntry> {
    let detail: String = r.try_get("detail")?;
    Ok(AuditEntry {
        seq: r.try_get("seq")?,
        ts: r.try_get("ts")?,
        org_id: r.try_get("org_id")?,
        project_id: r.try_get("project_id")?,
        actor: r.try_get("actor")?,
        action: r.try_get("action")?,
        target: r.try_get("target")?,
        detail: json_of(&detail)?,
    })
}

/// The control store on PostgreSQL.
pub struct PgControl {
    pool: PgPool,
    /// Unquoted schema name.
    schema: String,
    /// Quoted, for statements.
    q: String,
}

impl PgControl {
    /// Connect (a pool of [`POOL_SIZE`] connections on the current runtime)
    /// and apply the control migrations.
    pub async fn connect(url: &str, schema: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(POOL_SIZE)
            .acquire_timeout(Duration::from_secs(10))
            .connect(url)
            .await
            .map_err(|e| Error::Unavailable(format!("control store: connecting to PostgreSQL: {e}")))?;
        Self::with_pool(pool, schema).await
    }

    /// Use an existing pool; applies the control migrations.
    pub async fn with_pool(pool: PgPool, schema: &str) -> Result<Self> {
        if !valid_schema(schema) {
            return Err(Error::Config(format!(
                "console.schema '{schema}' must be a lowercase PostgreSQL name ([a-z_][a-z0-9_]*, at most 63 bytes)"
            )));
        }
        let me = Self { pool, schema: schema.to_string(), q: quote_ident(schema) };
        me.migrate().await?;
        Ok(me)
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    fn t(&self, table: &str) -> String {
        format!("{}.{table}", self.q)
    }

    /// Apply the migrations this schema has not had, under an advisory lock
    /// (like tenant migrations). A schema newer than this build is refused.
    pub async fn migrate(&self) -> Result<()> {
        let s = &self.q;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))").bind(&self.schema).execute(&mut *tx).await?;
        sqlx::raw_sql(sql(format!(
            "CREATE SCHEMA IF NOT EXISTS {s}; CREATE TABLE IF NOT EXISTS {s}.schema_migrations \
             (version INT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)"
        )))
        .execute(&mut *tx)
        .await?;
        let applied: Vec<i32> =
            sqlx::query_scalar(sql(format!("SELECT version FROM {s}.schema_migrations"))).fetch_all(&mut *tx).await?;
        let known = MIGRATIONS.last().map(|m| m.0).unwrap_or(0);
        if let Some(&newest) = applied.iter().max()
            && newest > known
        {
            return Err(Error::Config(format!(
                "control schema {s} is at migration {newest}, newer than this build (up to {known}): upgrade onto-intelli"
            )));
        }
        for (version, name, body) in MIGRATIONS {
            if applied.contains(version) {
                continue;
            }
            sqlx::raw_sql(sql(body.replace("{schema}", s)))
                .execute(&mut *tx)
                .await
                .map_err(|e| Error::Storage(format!("control migration {version:04}_{name} on {s}: {e}")))?;
            sqlx::query(sql(format!(
                "INSERT INTO {s}.schema_migrations (version, name, applied_at) VALUES ($1, $2, $3)"
            )))
            .bind(version)
            .bind(name)
            .bind(now_ms())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Applied migrations, oldest first: (version, name, applied_at ms).
    pub async fn migrations(&self) -> Result<Vec<(i32, String, i64)>> {
        Ok(sqlx::query_as(sql(format!(
            "SELECT version, name, applied_at FROM {} ORDER BY version",
            self.t("schema_migrations")
        )))
        .fetch_all(&self.pool)
        .await?)
    }

    /// Roles of an organization's members, locked for the transaction.
    async fn lock_roles(
        &self,
        tx: &mut sqlx::transaction::Transaction<'static, sqlx::Postgres>,
        org: &str,
    ) -> Result<Vec<(String, OrgRole)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(sql(format!(
            "SELECT user_id, role FROM {} WHERE org_id = $1 FOR UPDATE",
            self.t("members")
        )))
        .bind(org)
        .fetch_all(&mut **tx)
        .await?;
        rows.into_iter().map(|(u, r)| Ok((u, role_of_text(&r)?))).collect()
    }
}

fn keeps_an_owner(roles: &[(String, OrgRole)], user: &str, new_role: Option<OrgRole>) -> Result<()> {
    let owners = roles.iter().filter(|(_, r)| *r == OrgRole::Owner).count();
    let was_owner = roles.iter().any(|(u, r)| u == user && *r == OrgRole::Owner);
    if was_owner && new_role != Some(OrgRole::Owner) && owners <= 1 {
        return Err(Error::Conflict("an organization keeps at least one owner".into()));
    }
    Ok(())
}

#[async_trait]
impl ControlStore for PgControl {
    async fn user(&self, id: &str) -> Result<Option<User>> {
        let row = sqlx::query(sql(format!("SELECT {USER_COLS} FROM {} WHERE id = $1", self.t("users"))))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(user_of).transpose()
    }

    async fn users(&self) -> Result<Vec<User>> {
        let rows = sqlx::query(sql(format!("SELECT {USER_COLS} FROM {} ORDER BY created_at, id", self.t("users"))))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(user_of).collect()
    }

    async fn identity(&self, provider: &str, subject: &str) -> Result<Option<Identity>> {
        let row = sqlx::query(sql(format!(
            "SELECT provider, subject, user_id, email, claims::text AS claims, created_at, last_login_at \
             FROM {} WHERE provider = $1 AND subject = $2",
            self.t("identities")
        )))
        .bind(provider)
        .bind(subject)
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };
        let claims: String = r.try_get("claims")?;
        Ok(Some(Identity {
            provider: r.try_get("provider")?,
            subject: r.try_get("subject")?,
            user_id: r.try_get("user_id")?,
            email: r.try_get("email")?,
            claims: json_of(&claims)?,
            created_at: r.try_get("created_at")?,
            last_login_at: r.try_get("last_login_at")?,
        }))
    }

    async fn upsert_login(&self, l: LoginUpsert) -> Result<User> {
        let (users, ids) = (self.t("users"), self.t("identities"));
        let claims = serde_json::to_string(&l.claims)?;
        let mut tx = self.pool.begin().await?;
        // Two first logins of one identity at once would both create a user.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("{}:login:{}:{}", self.schema, l.provider, l.subject))
            .execute(&mut *tx)
            .await?;
        let existing: Option<String> =
            sqlx::query_scalar(sql(format!("SELECT user_id FROM {ids} WHERE provider = $1 AND subject = $2")))
                .bind(&l.provider)
                .bind(&l.subject)
                .fetch_optional(&mut *tx)
                .await?;
        let uid = match existing {
            Some(uid) => {
                sqlx::query(sql(format!(
                    "UPDATE {ids} SET last_login_at = $3, claims = $4::jsonb, email = $5 \
                     WHERE provider = $1 AND subject = $2"
                )))
                .bind(&l.provider)
                .bind(&l.subject)
                .bind(l.now)
                .bind(&claims)
                .bind(&l.email)
                .execute(&mut *tx)
                .await?;
                sqlx::query(sql(format!(
                    "UPDATE {users} SET last_login_at = $2, \
                     email = COALESCE($3, email), \
                     email_verified = CASE WHEN $3::text IS NULL THEN email_verified ELSE $4 END, \
                     name = CASE WHEN $5 = '' THEN name ELSE $5 END, \
                     avatar_url = COALESCE($6, avatar_url) WHERE id = $1"
                )))
                .bind(&uid)
                .bind(l.now)
                .bind(&l.email)
                .bind(l.email_verified)
                .bind(&l.name)
                .bind(&l.avatar_url)
                .execute(&mut *tx)
                .await?;
                uid
            }
            None => {
                let uid = new_id("usr");
                sqlx::query(sql(format!(
                    "INSERT INTO {users} (id, email, email_verified, name, avatar_url, created_at, last_login_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $6)"
                )))
                .bind(&uid)
                .bind(&l.email)
                .bind(l.email_verified)
                .bind(&l.name)
                .bind(&l.avatar_url)
                .bind(l.now)
                .execute(&mut *tx)
                .await?;
                sqlx::query(sql(format!(
                    "INSERT INTO {ids} (provider, subject, user_id, email, claims, created_at, last_login_at) \
                     VALUES ($1, $2, $3, $4, $5::jsonb, $6, $6)"
                )))
                .bind(&l.provider)
                .bind(&l.subject)
                .bind(&uid)
                .bind(&l.email)
                .bind(&claims)
                .bind(l.now)
                .execute(&mut *tx)
                .await?;
                uid
            }
        };
        let row = sqlx::query(sql(format!("SELECT {USER_COLS} FROM {users} WHERE id = $1")))
            .bind(&uid)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        user_of(&row)
    }

    async fn sign_out_everywhere(&self, user: &str, at: i64) -> Result<()> {
        let n = sqlx::query(sql(format!("UPDATE {} SET sessions_valid_after = $2 WHERE id = $1", self.t("users"))))
            .bind(user)
            .bind(at)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::not_found(format!("user {user}")));
        }
        Ok(())
    }

    async fn create_org(&self, name: &str, slug: &str, owner: &str) -> Result<Org> {
        if !valid_slug(slug) {
            return Err(Error::invalid(format!("invalid organization slug '{slug}'")));
        }
        let now = now_ms();
        let org =
            Org { id: new_id("org"), slug: slug.into(), name: name.into(), created_at: now, created_by: owner.into() };
        let mut tx = self.pool.begin().await?;
        sqlx::query(sql(format!(
            "INSERT INTO {} (id, slug, name, created_at, created_by) VALUES ($1, $2, $3, $4, $5)",
            self.t("orgs")
        )))
        .bind(&org.id)
        .bind(&org.slug)
        .bind(&org.name)
        .bind(now)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .map_err(|e| conflict_or(e, || format!("organization slug '{slug}' is taken")))?;
        sqlx::query(sql(format!(
            "INSERT INTO {} (org_id, user_id, role, created_at) VALUES ($1, $2, 'owner', $3)",
            self.t("members")
        )))
        .bind(&org.id)
        .bind(owner)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(c) if c == "23503" => Error::not_found(format!("user {owner}")),
            _ => e.into(),
        })?;
        tx.commit().await?;
        Ok(org)
    }

    async fn org(&self, id_or_slug: &str) -> Result<Option<Org>> {
        let row = sqlx::query(sql(format!(
            "SELECT {ORG_COLS} FROM {} WHERE id = $1 OR slug = $1 ORDER BY (id = $1) DESC LIMIT 1",
            self.t("orgs")
        )))
        .bind(id_or_slug)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(org_of).transpose()
    }

    async fn rename_org(&self, org: &str, name: &str) -> Result<Org> {
        let row =
            sqlx::query(sql(format!("UPDATE {} SET name = $2 WHERE id = $1 RETURNING {ORG_COLS}", self.t("orgs"))))
                .bind(org)
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        row.as_ref().map(org_of).transpose()?.ok_or_else(|| Error::not_found(format!("organization {org}")))
    }

    async fn delete_org(&self, org: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let found: Option<String> =
            sqlx::query_scalar(sql(format!("SELECT id FROM {} WHERE id = $1 FOR UPDATE", self.t("orgs"))))
                .bind(org)
                .fetch_optional(&mut *tx)
                .await?;
        if found.is_none() {
            return Err(Error::not_found(format!("organization {org}")));
        }
        let active: i64 = sqlx::query_scalar(sql(format!(
            "SELECT count(*) FROM {} WHERE org_id = $1 AND status = 'active'",
            self.t("projects")
        )))
        .bind(org)
        .fetch_one(&mut *tx)
        .await?;
        if active > 0 {
            return Err(Error::Conflict("delete the organization's projects first".into()));
        }
        // Keys go with their projects (ON DELETE CASCADE), members and
        // invitations with the organization.
        sqlx::query(sql(format!("DELETE FROM {} WHERE org_id = $1", self.t("projects"))))
            .bind(org)
            .execute(&mut *tx)
            .await?;
        sqlx::query(sql(format!("DELETE FROM {} WHERE id = $1", self.t("orgs")))).bind(org).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn orgs_of(&self, user: &str) -> Result<Vec<(Org, OrgRole)>> {
        let rows = sqlx::query(sql(format!(
            "SELECT o.id, o.slug, o.name, o.created_at, o.created_by, m.role FROM {} o JOIN {} m ON m.org_id = o.id \
             WHERE m.user_id = $1 ORDER BY o.created_at, o.id",
            self.t("orgs"),
            self.t("members")
        )))
        .bind(user)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let role: String = r.try_get("role")?;
                Ok((org_of(r)?, role_of_text(&role)?))
            })
            .collect()
    }

    async fn role_of(&self, org: &str, user: &str) -> Result<Option<OrgRole>> {
        let role: Option<String> = sqlx::query_scalar(sql(format!(
            "SELECT role FROM {} WHERE org_id = $1 AND user_id = $2",
            self.t("members")
        )))
        .bind(org)
        .bind(user)
        .fetch_optional(&self.pool)
        .await?;
        role.as_deref().map(role_of_text).transpose()
    }

    async fn members(&self, org: &str) -> Result<Vec<MemberView>> {
        let rows = sqlx::query(sql(format!(
            "SELECT u.id, u.email, u.name, u.avatar_url, m.role, m.created_at FROM {} m JOIN {} u ON u.id = m.user_id \
             WHERE m.org_id = $1 ORDER BY m.created_at, u.id",
            self.t("members"),
            self.t("users")
        )))
        .bind(org)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let role: String = r.try_get("role")?;
                Ok(MemberView {
                    user_id: r.try_get("id")?,
                    email: r.try_get("email")?,
                    name: r.try_get("name")?,
                    avatar_url: r.try_get("avatar_url")?,
                    role: role_of_text(&role)?,
                    joined_at: r.try_get("created_at")?,
                })
            })
            .collect()
    }

    async fn add_member(&self, org: &str, user: &str, role: OrgRole) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let roles = self.lock_roles(&mut tx, org).await?;
        keeps_an_owner(&roles, user, Some(role))?;
        sqlx::query(sql(format!(
            "INSERT INTO {} (org_id, user_id, role, created_at) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (org_id, user_id) DO UPDATE SET role = EXCLUDED.role",
            self.t("members")
        )))
        .bind(org)
        .bind(user)
        .bind(role.as_str())
        .bind(now_ms())
        .execute(&mut *tx)
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(c) if c == "23503" => Error::not_found(format!("organization {org} or user {user}")),
            _ => e.into(),
        })?;
        tx.commit().await?;
        Ok(())
    }

    async fn set_role(&self, org: &str, user: &str, role: OrgRole) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let roles = self.lock_roles(&mut tx, org).await?;
        if !roles.iter().any(|(u, _)| u == user) {
            return Err(Error::not_found(format!("member {user} of {org}")));
        }
        keeps_an_owner(&roles, user, Some(role))?;
        sqlx::query(sql(format!("UPDATE {} SET role = $3 WHERE org_id = $1 AND user_id = $2", self.t("members"))))
            .bind(org)
            .bind(user)
            .bind(role.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn remove_member(&self, org: &str, user: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let roles = self.lock_roles(&mut tx, org).await?;
        if !roles.iter().any(|(u, _)| u == user) {
            return Err(Error::not_found(format!("member {user} of {org}")));
        }
        keeps_an_owner(&roles, user, None)?;
        sqlx::query(sql(format!("DELETE FROM {} WHERE org_id = $1 AND user_id = $2", self.t("members"))))
            .bind(org)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn invite(&self, inv: NewInvitation) -> Result<(Invitation, String)> {
        if inv.role == OrgRole::Owner {
            return Err(Error::invalid("invitations are for admins, developers and viewers"));
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
        sqlx::query(sql(format!(
            "INSERT INTO {} (id, org_id, email, role, code_hash, invited_by, created_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            self.t("invitations")
        )))
        .bind(&i.id)
        .bind(&i.org_id)
        .bind(&i.email)
        .bind(i.role.as_str())
        .bind(&i.code_hash)
        .bind(&i.invited_by)
        .bind(i.created_at)
        .bind(i.expires_at)
        .execute(&self.pool)
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(c) if c == "23503" => Error::not_found(format!("organization {}", i.org_id)),
            _ => e.into(),
        })?;
        Ok((i, code))
    }

    async fn invitations(&self, org: &str) -> Result<Vec<Invitation>> {
        let rows = sqlx::query(sql(format!(
            "SELECT {INVITATION_COLS} FROM {} WHERE org_id = $1 AND accepted_at IS NULL ORDER BY created_at DESC, id DESC",
            self.t("invitations")
        )))
        .bind(org)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(invitation_of).collect()
    }

    async fn invitation_by_code(&self, code: &str) -> Result<Option<(Invitation, Org)>> {
        let row = sqlx::query(sql(format!(
            "SELECT {INVITATION_COLS} FROM {} WHERE code_hash = $1 AND accepted_at IS NULL AND expires_at > $2",
            self.t("invitations")
        )))
        .bind(sha256_hex(code.as_bytes()))
        .bind(now_ms())
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };
        let inv = invitation_of(&r)?;
        Ok(self.org(&inv.org_id).await?.map(|o| (inv, o)))
    }

    async fn invitation_pending(&self, verified_email: Option<&str>, code_hash: Option<&str>) -> Result<bool> {
        let row: Option<(i32,)> = sqlx::query_as(sql(format!(
            "SELECT 1 FROM {} WHERE accepted_at IS NULL AND expires_at > $1 \
               AND (code_hash = $2 OR ($3::text IS NOT NULL AND lower(email) = lower($3))) LIMIT 1",
            self.t("invitations")
        )))
        .bind(now_ms())
        .bind(code_hash)
        .bind(verified_email)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn accept_invitations(&self, user: &User, code_hash: Option<&str>) -> Result<Vec<String>> {
        let now = now_ms();
        let email = if user.email_verified { user.email.clone() } else { None };
        let hash = code_hash;
        let mut tx = self.pool.begin().await?;
        // Accept by marking, so each invitation is accepted exactly once.
        let rows: Vec<(String, String)> = sqlx::query_as(sql(format!(
            "UPDATE {} SET accepted_at = $1, accepted_by = $2 \
             WHERE accepted_at IS NULL AND expires_at > $1 \
               AND (code_hash = $3 OR ($4::text IS NOT NULL AND lower(email) = lower($4))) \
             RETURNING org_id, role",
            self.t("invitations")
        )))
        .bind(now)
        .bind(&user.id)
        .bind(hash)
        .bind(&email)
        .fetch_all(&mut *tx)
        .await?;
        let mut joined: Vec<String> = Vec::new();
        for (org, role) in rows {
            sqlx::query(sql(format!(
                "INSERT INTO {} (org_id, user_id, role, created_at) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (org_id, user_id) DO NOTHING",
                self.t("members")
            )))
            .bind(&org)
            .bind(&user.id)
            .bind(&role)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            if !joined.contains(&org) {
                joined.push(org);
            }
        }
        tx.commit().await?;
        Ok(joined)
    }

    async fn revoke_invitation(&self, org: &str, id: &str) -> Result<()> {
        let n = sqlx::query(sql(format!(
            "DELETE FROM {} WHERE id = $1 AND org_id = $2 AND accepted_at IS NULL",
            self.t("invitations")
        )))
        .bind(id)
        .bind(org)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(Error::not_found(format!("invitation {id}")));
        }
        Ok(())
    }

    async fn create_project(&self, org: &str, name: &str, by: &str) -> Result<Project> {
        for _ in 0..3 {
            let p = crate::store::new_project(org, &crate::keys::new_ref(), name, by);
            match self.insert_project(&p).await {
                Err(Error::Conflict(_)) => continue,
                other => return other.map(|_| p),
            }
        }
        Err(Error::Conflict("could not pick a unique project ref".into()))
    }

    async fn adopt_project(&self, org: &str, tenant: &str, by: &str) -> Result<Option<Project>> {
        let p = crate::store::new_project(org, tenant, tenant, by);
        match self.insert_project(&p).await {
            Ok(()) => Ok(Some(p)),
            Err(Error::Conflict(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn project(&self, id: &str) -> Result<Option<Project>> {
        let row = sqlx::query(sql(format!("SELECT {PROJECT_COLS} FROM {} WHERE id = $1", self.t("projects"))))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(project_of).transpose()
    }

    async fn project_by_ref(&self, r: &str) -> Result<Option<Project>> {
        let row = sqlx::query(sql(format!("SELECT {PROJECT_COLS} FROM {} WHERE ref = $1", self.t("projects"))))
            .bind(r)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(project_of).transpose()
    }

    async fn projects(&self, org: &str) -> Result<Vec<Project>> {
        let rows = sqlx::query(sql(format!(
            "SELECT {PROJECT_COLS} FROM {} WHERE org_id = $1 AND status = 'active' ORDER BY created_at, id",
            self.t("projects")
        )))
        .bind(org)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(project_of).collect()
    }

    async fn projects_of(&self, user: &str) -> Result<Vec<(Project, OrgRole)>> {
        let rows = sqlx::query(sql(format!(
            "SELECT p.id, p.org_id, p.ref, p.name, p.status, p.settings::text AS settings, p.settings_version, \
             p.created_at, p.created_by, p.deleted_at, m.role FROM {} p JOIN {} m ON m.org_id = p.org_id \
             WHERE m.user_id = $1 AND p.status = 'active' ORDER BY p.created_at, p.id",
            self.t("projects"),
            self.t("members")
        )))
        .bind(user)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let role: String = r.try_get("role")?;
                Ok((project_of(r)?, role_of_text(&role)?))
            })
            .collect()
    }

    async fn update_project(&self, r: &str, patch: ProjectPatch, _by: &str) -> Result<Project> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(sql(format!(
            "SELECT {PROJECT_COLS} FROM {} WHERE ref = $1 AND status = 'active' FOR UPDATE",
            self.t("projects")
        )))
        .bind(r)
        .fetch_optional(&mut *tx)
        .await?;
        let mut p =
            row.as_ref().map(project_of).transpose()?.ok_or_else(|| Error::not_found(format!("project {r}")))?;
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
        sqlx::query(sql(format!(
            "UPDATE {} SET name = $2, settings = $3::jsonb, settings_version = $4 WHERE id = $1",
            self.t("projects")
        )))
        .bind(&p.id)
        .bind(&p.name)
        .bind(serde_json::to_string(&p.settings)?)
        .bind(p.settings_version)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(p)
    }

    async fn delete_project(&self, r: &str, _by: &str) -> Result<()> {
        let n = sqlx::query(sql(format!(
            "UPDATE {} SET status = 'deleted', deleted_at = $2 WHERE ref = $1 AND status = 'active'",
            self.t("projects")
        )))
        .bind(r)
        .bind(now_ms())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(Error::not_found(format!("project {r}")));
        }
        Ok(())
    }

    async fn membership(&self, user: &str, project_ref: &str) -> Result<Option<(Project, OrgRole)>> {
        let row = sqlx::query(sql(format!(
            "SELECT p.id, p.org_id, p.ref, p.name, p.status, p.settings::text AS settings, p.settings_version, \
             p.created_at, p.created_by, p.deleted_at, m.role FROM {} p JOIN {} m ON m.org_id = p.org_id \
             WHERE p.ref = $1 AND m.user_id = $2 AND p.status = 'active'",
            self.t("projects"),
            self.t("members")
        )))
        .bind(project_ref)
        .bind(user)
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };
        let role: String = r.try_get("role")?;
        Ok(Some((project_of(&r)?, role_of_text(&role)?)))
    }

    async fn create_key(&self, k: NewKey) -> Result<ApiKey> {
        let key = crate::store::new_key(k);
        let scopes: Vec<&str> = key.scopes.iter().map(|s| s.as_str()).collect();
        let roles: Vec<&str> = key.impersonate_roles.iter().map(Role::as_str).collect();
        sqlx::query(sql(format!(
            "INSERT INTO {} (id, project_id, name, prefix, hash, scopes, workspaces, impersonate, impersonate_roles, \
             created_at, created_by, expires_at, rotated_from) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
            self.t("api_keys")
        )))
        .bind(&key.id)
        .bind(&key.project_id)
        .bind(&key.name)
        .bind(&key.prefix)
        .bind(&key.hash)
        .bind(&scopes)
        .bind(&key.workspaces)
        .bind(key.impersonate)
        .bind(&roles)
        .bind(key.created_at)
        .bind(&key.created_by)
        .bind(key.expires_at)
        .bind(&key.rotated_from)
        .execute(&self.pool)
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(c) if c == "23505" => Error::Conflict(format!("key prefix {} is taken", key.prefix)),
            Some(c) if c == "23503" => Error::not_found(format!("project {}", key.project_id)),
            _ => e.into(),
        })?;
        Ok(key)
    }

    async fn key(&self, id: &str) -> Result<Option<ApiKey>> {
        let row = sqlx::query(sql(format!("SELECT {KEY_COLS} FROM {} WHERE id = $1", self.t("api_keys"))))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(key_of).transpose()
    }

    async fn key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>> {
        let row = sqlx::query(sql(format!("SELECT {KEY_COLS} FROM {} WHERE prefix = $1", self.t("api_keys"))))
            .bind(prefix)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(key_of).transpose()
    }

    async fn keys(&self, project_id: &str) -> Result<Vec<ApiKey>> {
        let rows = sqlx::query(sql(format!(
            "SELECT {KEY_COLS} FROM {} WHERE project_id = $1 ORDER BY created_at DESC, id DESC",
            self.t("api_keys")
        )))
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(key_of).collect()
    }

    async fn revoke_key(&self, id: &str, by: &str, at: i64) -> Result<()> {
        let n = sqlx::query(sql(format!(
            "UPDATE {} SET revoked_at = COALESCE(revoked_at, $3), revoked_by = COALESCE(revoked_by, $2) WHERE id = $1",
            self.t("api_keys")
        )))
        .bind(id)
        .bind(by)
        .bind(at)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(Error::not_found(format!("key {id}")));
        }
        Ok(())
    }

    async fn set_key_hash(&self, id: &str, old: &str, new: &str) -> Result<bool> {
        let n = sqlx::query(sql(format!("UPDATE {} SET hash = $3 WHERE id = $1 AND hash = $2", self.t("api_keys"))))
            .bind(id)
            .bind(old)
            .bind(new)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n == 1)
    }

    async fn expire_key(&self, id: &str, at: i64) -> Result<()> {
        let n = sqlx::query(sql(format!(
            "UPDATE {} SET expires_at = LEAST(COALESCE(expires_at, $2), $2) WHERE id = $1",
            self.t("api_keys")
        )))
        .bind(id)
        .bind(at)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(Error::not_found(format!("key {id}")));
        }
        Ok(())
    }

    async fn touch_keys(&self, used: &[(String, i64)]) -> Result<()> {
        if used.is_empty() {
            return Ok(());
        }
        let (ids, ats): (Vec<String>, Vec<i64>) = used.iter().cloned().unzip();
        sqlx::query(sql(format!(
            "UPDATE {} k SET last_used_at = GREATEST(COALESCE(k.last_used_at, 0), u.at) \
             FROM UNNEST($1::text[], $2::bigint[]) AS u(id, at) WHERE k.id = u.id",
            self.t("api_keys")
        )))
        .bind(&ids)
        .bind(&ats)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn put_login(&self, l: PendingLogin) -> Result<()> {
        sqlx::query(sql(format!(
            "INSERT INTO {} (state_hash, provider, nonce, code_verifier, binding_hash, invite_code_hash, return_to, \
             created_at, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            self.t("oidc_logins")
        )))
        .bind(&l.state_hash)
        .bind(&l.provider)
        .bind(&l.nonce)
        .bind(&l.code_verifier)
        .bind(&l.binding_hash)
        .bind(&l.invite_code_hash)
        .bind(&l.return_to)
        .bind(l.created_at)
        .bind(l.expires_at)
        .execute(&self.pool)
        .await
        .map_err(|e| conflict_or(e, || "pending login exists".into()))?;
        Ok(())
    }

    async fn take_login(&self, state_hash: &str, now: i64) -> Result<Option<PendingLogin>> {
        let row = sqlx::query(sql(format!(
            "DELETE FROM {} WHERE state_hash = $1 RETURNING state_hash, provider, nonce, code_verifier, binding_hash, \
             invite_code_hash, return_to, created_at, expires_at",
            self.t("oidc_logins")
        )))
        .bind(state_hash)
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };
        let l = PendingLogin {
            state_hash: r.try_get("state_hash")?,
            provider: r.try_get("provider")?,
            nonce: r.try_get("nonce")?,
            code_verifier: r.try_get("code_verifier")?,
            binding_hash: r.try_get("binding_hash")?,
            invite_code_hash: r.try_get("invite_code_hash")?,
            return_to: r.try_get("return_to")?,
            created_at: r.try_get("created_at")?,
            expires_at: r.try_get("expires_at")?,
        };
        Ok(Some(l).filter(|l| l.expires_at > now))
    }

    async fn put_ticket(&self, t: LoginTicket) -> Result<()> {
        sqlx::query(sql(format!(
            "INSERT INTO {} (ticket_hash, user_id, binding_hash, expires_at) VALUES ($1, $2, $3, $4)",
            self.t("login_tickets")
        )))
        .bind(&t.ticket_hash)
        .bind(&t.user_id)
        .bind(&t.binding_hash)
        .bind(t.expires_at)
        .execute(&self.pool)
        .await
        .map_err(|e| conflict_or(e, || "ticket exists".into()))?;
        Ok(())
    }

    async fn take_ticket(&self, ticket_hash: &str, now: i64) -> Result<Option<LoginTicket>> {
        let row = sqlx::query(sql(format!(
            "DELETE FROM {} WHERE ticket_hash = $1 RETURNING ticket_hash, user_id, binding_hash, expires_at",
            self.t("login_tickets")
        )))
        .bind(ticket_hash)
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else { return Ok(None) };
        let t = LoginTicket {
            ticket_hash: r.try_get("ticket_hash")?,
            user_id: r.try_get("user_id")?,
            binding_hash: r.try_get("binding_hash")?,
            expires_at: r.try_get("expires_at")?,
        };
        Ok(Some(t).filter(|t| t.expires_at > now))
    }

    async fn sweep(&self, now: i64) -> Result<u64> {
        let a = sqlx::query(sql(format!("DELETE FROM {} WHERE expires_at <= $1", self.t("oidc_logins"))))
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let b = sqlx::query(sql(format!("DELETE FROM {} WHERE expires_at <= $1", self.t("login_tickets"))))
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let c = sqlx::query(sql(format!("DELETE FROM {} WHERE day < $1::date", self.t("usage_daily"))))
            .bind(usage_horizon(now))
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(a + b + c)
    }

    async fn llm(&self, project_id: &str) -> Result<Option<ProjectLlm>> {
        let row = sqlx::query(sql(format!("SELECT {LLM_COLS} FROM {} WHERE project_id = $1", self.t("project_llm"))))
            .bind(project_id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(llm_of).transpose()
    }

    async fn llms(&self) -> Result<Vec<ProjectLlm>> {
        let rows = sqlx::query(sql(format!("SELECT {LLM_COLS} FROM {} ORDER BY project_id", self.t("project_llm"))))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(llm_of).collect()
    }

    async fn put_llm(&self, l: ProjectLlm) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(sql(format!(
            "INSERT INTO {} (project_id, provider, model, base_url, api_key_ct, api_key_last4, max_output_tokens, \
             temperature, effort, timeout_secs, pricing, headers, monthly_budget_usd, monthly_token_budget, \
             asks_per_minute, updated_at, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::jsonb, $12::jsonb, $13, $14, $15, $16, $17) \
             ON CONFLICT (project_id) DO UPDATE SET provider = EXCLUDED.provider, model = EXCLUDED.model, \
             base_url = EXCLUDED.base_url, api_key_ct = EXCLUDED.api_key_ct, api_key_last4 = EXCLUDED.api_key_last4, \
             max_output_tokens = EXCLUDED.max_output_tokens, temperature = EXCLUDED.temperature, \
             effort = EXCLUDED.effort, timeout_secs = EXCLUDED.timeout_secs, pricing = EXCLUDED.pricing, \
             headers = EXCLUDED.headers, monthly_budget_usd = EXCLUDED.monthly_budget_usd, \
             monthly_token_budget = EXCLUDED.monthly_token_budget, asks_per_minute = EXCLUDED.asks_per_minute, \
             updated_at = EXCLUDED.updated_at, updated_by = EXCLUDED.updated_by",
            self.t("project_llm")
        )))
        .bind(&l.project_id)
        .bind(&l.provider)
        .bind(&l.model)
        .bind(&l.base_url)
        .bind(l.api_key_ct.as_ref().map(|c| c.0.clone()))
        .bind(&l.api_key_last4)
        .bind(l.max_output_tokens.min(i32::MAX as u32) as i32)
        .bind(l.temperature)
        .bind(&l.effort)
        .bind(l.timeout_secs.min(i32::MAX as u32) as i32)
        .bind(serde_json::to_string(&l.pricing)?)
        .bind(serde_json::to_string(&l.headers)?)
        .bind(l.monthly_budget_usd)
        .bind(l.monthly_token_budget)
        .bind(l.asks_per_minute)
        .bind(l.updated_at)
        .bind(&l.updated_by)
        .execute(&mut *tx)
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(c) if c == "23503" => Error::not_found(format!("project {}", l.project_id)),
            _ => e.into(),
        })?;
        self.bump(&mut tx, &l.project_id).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_llm(&self, project_id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let n = sqlx::query(sql(format!("DELETE FROM {} WHERE project_id = $1", self.t("project_llm"))))
            .bind(project_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n > 0 {
            self.bump(&mut tx, project_id).await?;
        }
        tx.commit().await?;
        Ok(n > 0)
    }

    async fn replace_llm_key(&self, project_id: &str, old: &Sealed, new: &Sealed) -> Result<bool> {
        let n = sqlx::query(sql(format!(
            "UPDATE {} SET api_key_ct = $3 WHERE project_id = $1 AND api_key_ct = $2",
            self.t("project_llm")
        )))
        .bind(project_id)
        .bind(&old.0)
        .bind(&new.0)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n > 0)
    }

    async fn add_usage(&self, rows: &[UsageDelta]) -> Result<()> {
        // One row per key: ON CONFLICT cannot update a row twice in one statement.
        let mut merged: Vec<UsageDay> = Vec::new();
        for r in rows {
            match merged.iter_mut().find(|m| {
                (&m.project_id, &m.day, &m.provider, &m.model) == (&r.project_id, &r.day, &r.provider, &r.model)
            }) {
                Some(m) => m.add(r),
                None => merged.push(r.clone()),
            }
        }
        let rows = &merged[..];
        if rows.is_empty() {
            return Ok(());
        }
        let col = |f: fn(&UsageDay) -> i64| rows.iter().map(f).collect::<Vec<i64>>();
        let text = |f: fn(&UsageDay) -> &str| rows.iter().map(|r| f(r).to_string()).collect::<Vec<String>>();
        // Rows of projects deleted meanwhile are skipped (the join), not an error.
        sqlx::query(sql(format!(
            "INSERT INTO {usage} AS u (project_id, day, provider, model, asks, llm_calls, deterministic, \
             input_tokens, output_tokens, cached_tokens, cost_usd, errors) \
             SELECT d.project_id, d.day::date, d.provider, d.model, d.asks, d.llm_calls, d.deterministic, \
                    d.input_tokens, d.output_tokens, d.cached_tokens, d.cost_usd, d.errors \
             FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::bigint[], $6::bigint[], $7::bigint[], \
                         $8::bigint[], $9::bigint[], $10::bigint[], $11::float8[], $12::bigint[]) \
                  AS d(project_id, day, provider, model, asks, llm_calls, deterministic, input_tokens, \
                       output_tokens, cached_tokens, cost_usd, errors) \
             JOIN {projects} p ON p.id = d.project_id \
             ON CONFLICT (project_id, day, provider, model) DO UPDATE SET \
             asks = u.asks + EXCLUDED.asks, llm_calls = u.llm_calls + EXCLUDED.llm_calls, \
             deterministic = u.deterministic + EXCLUDED.deterministic, \
             input_tokens = u.input_tokens + EXCLUDED.input_tokens, \
             output_tokens = u.output_tokens + EXCLUDED.output_tokens, \
             cached_tokens = u.cached_tokens + EXCLUDED.cached_tokens, \
             cost_usd = u.cost_usd + EXCLUDED.cost_usd, errors = u.errors + EXCLUDED.errors",
            usage = self.t("usage_daily"),
            projects = self.t("projects"),
        )))
        .bind(text(|r| &r.project_id))
        .bind(text(|r| &r.day))
        .bind(text(|r| &r.provider))
        .bind(text(|r| &r.model))
        .bind(col(|r| r.asks))
        .bind(col(|r| r.llm_calls))
        .bind(col(|r| r.deterministic))
        .bind(col(|r| r.input_tokens))
        .bind(col(|r| r.output_tokens))
        .bind(col(|r| r.cached_tokens))
        .bind(rows.iter().map(|r| r.cost_usd).collect::<Vec<f64>>())
        .bind(col(|r| r.errors))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn usage(&self, project_id: &str, from_day: &str, to_day: &str) -> Result<Vec<UsageDay>> {
        let rows = sqlx::query(sql(format!(
            "SELECT {USAGE_COLS} FROM {} WHERE project_id = $1 AND day BETWEEN $2::date AND $3::date \
             ORDER BY day, provider, model",
            self.t("usage_daily")
        )))
        .bind(project_id)
        .bind(from_day)
        .bind(to_day)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(usage_of).collect()
    }

    async fn settings_versions(&self) -> Result<Vec<(String, i64)>> {
        Ok(sqlx::query_as(sql(format!(
            "SELECT ref, settings_version FROM {} WHERE status = 'active'",
            self.t("projects")
        )))
        .fetch_all(&self.pool)
        .await?)
    }

    async fn audit(&self, e: AuditEntry) -> Result<()> {
        sqlx::query(sql(format!(
            "INSERT INTO {} (ts, org_id, project_id, actor, action, target, detail) \
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
            self.t("audit_log")
        )))
        .bind(e.ts)
        .bind(&e.org_id)
        .bind(&e.project_id)
        .bind(&e.actor)
        .bind(&e.action)
        .bind(&e.target)
        .bind(serde_json::to_string(&e.detail)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn audit_log(&self, org: &str, before: Option<i64>, limit: usize) -> Result<Vec<AuditEntry>> {
        let rows = sqlx::query(sql(format!(
            "SELECT seq, ts, org_id, project_id, actor, action, target, detail::text AS detail FROM {} \
             WHERE org_id = $1 AND ($2::bigint IS NULL OR seq < $2) ORDER BY seq DESC LIMIT $3",
            self.t("audit_log")
        )))
        .bind(org)
        .bind(before)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(audit_of).collect()
    }
}

impl PgControl {
    async fn bump(
        &self,
        tx: &mut sqlx::transaction::Transaction<'static, sqlx::Postgres>,
        project_id: &str,
    ) -> Result<()> {
        sqlx::query(sql(format!(
            "UPDATE {} SET settings_version = settings_version + 1 WHERE id = $1",
            self.t("projects")
        )))
        .bind(project_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn insert_project(&self, p: &Project) -> Result<()> {
        sqlx::query(sql(format!(
            "INSERT INTO {} (id, org_id, ref, name, status, settings, settings_version, created_at, created_by) \
             VALUES ($1, $2, $3, $4, 'active', '{{}}'::jsonb, 1, $5, $6)",
            self.t("projects")
        )))
        .bind(&p.id)
        .bind(&p.org_id)
        .bind(&p.r#ref)
        .bind(&p.name)
        .bind(p.created_at)
        .bind(&p.created_by)
        .execute(&self.pool)
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(c) if c == "23505" => Error::Conflict(format!("project ref {} is taken", p.r#ref)),
            Some(c) if c == "23503" => Error::not_found(format!("organization {}", p.org_id)),
            _ => e.into(),
        })?;
        Ok(())
    }
}
