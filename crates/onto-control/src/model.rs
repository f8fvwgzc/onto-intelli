//! Rows of the control store. Times are Unix milliseconds; ids carry a type
//! prefix (`usr_`, `org_`, `prj_`, `key_`, `inv_`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use onto_core::{Error, Result, Role};

/// A person who signs in to the console.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: String,
    pub avatar_url: Option<String>,
    pub created_at: i64,
    pub last_login_at: Option<i64>,
    /// Console sessions issued before this are refused (sign out everywhere).
    pub sessions_valid_after: i64,
    pub disabled: bool,
}

/// One IdP identity `(provider, subject)` of a user.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    /// A `console.oidc` id, `local` (bootstrap admin) or `dev`.
    pub provider: String,
    pub subject: String,
    pub user_id: String,
    pub email: Option<String>,
    /// Profile claims at the last login (name, picture, email_verified).
    pub claims: Value,
    pub created_at: i64,
    pub last_login_at: i64,
}

/// What a sign-in tells the store: the identity and its profile.
#[derive(Clone, Debug, Default)]
pub struct LoginUpsert {
    pub provider: String,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: String,
    pub avatar_url: Option<String>,
    pub claims: Value,
    pub now: i64,
}

/// An organization (a team).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Org {
    pub id: String,
    /// `[a-z0-9-]`, 3 to 40 characters, unique.
    pub slug: String,
    pub name: String,
    pub created_at: i64,
    pub created_by: String,
}

/// A member's role in an organization, highest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrgRole {
    Owner,
    Admin,
    Developer,
    Viewer,
}

impl OrgRole {
    pub const ALL: [OrgRole; 4] = [OrgRole::Owner, OrgRole::Admin, OrgRole::Developer, OrgRole::Viewer];

    pub fn as_str(self) -> &'static str {
        match self {
            OrgRole::Owner => "owner",
            OrgRole::Admin => "admin",
            OrgRole::Developer => "developer",
            OrgRole::Viewer => "viewer",
        }
    }

    pub fn parse(s: &str) -> Result<OrgRole> {
        match s.trim().to_ascii_lowercase().as_str() {
            "owner" => Ok(OrgRole::Owner),
            "admin" => Ok(OrgRole::Admin),
            "developer" => Ok(OrgRole::Developer),
            "viewer" => Ok(OrgRole::Viewer),
            other => Err(Error::invalid(format!("unknown role '{other}' (owner, admin, developer, viewer)"))),
        }
    }

    fn rank(self) -> u8 {
        match self {
            OrgRole::Owner => 3,
            OrgRole::Admin => 2,
            OrgRole::Developer => 1,
            OrgRole::Viewer => 0,
        }
    }

    /// This role has at least the rights of `other`.
    pub fn at_least(self, other: OrgRole) -> bool {
        self.rank() >= other.rank()
    }

    /// The data-plane roles of a console member in a project (section 5 of
    /// docs/CONTROL_PLANE.md): owners and admins are `admin`, developers
    /// `member` + `developer`, viewers `viewer` + `observer`.
    pub fn principal_roles(self) -> Vec<Role> {
        match self {
            OrgRole::Owner | OrgRole::Admin => vec![Role::Admin],
            OrgRole::Developer => vec![Role::Member, Role::Custom("developer".into())],
            OrgRole::Viewer => vec![Role::Viewer, Role::Custom("observer".into())],
        }
    }
}

impl std::fmt::Display for OrgRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub org_id: String,
    pub user_id: String,
    pub role: OrgRole,
    pub created_at: i64,
}

/// A member with the parts of the user the members page shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemberView {
    pub user_id: String,
    pub email: Option<String>,
    pub name: String,
    pub avatar_url: Option<String>,
    pub role: OrgRole,
    pub joined_at: i64,
}

/// An invitation to an organization. The code itself is never stored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Invitation {
    pub id: String,
    pub org_id: String,
    pub email: String,
    pub role: OrgRole,
    #[serde(skip)]
    pub code_hash: String,
    pub invited_by: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub accepted_at: Option<i64>,
    pub accepted_by: Option<String>,
}

/// Invitations are valid this long.
pub const INVITATION_TTL_MS: i64 = 7 * 24 * 3600 * 1000;

#[derive(Clone, Debug)]
pub struct NewInvitation {
    pub org_id: String,
    pub email: String,
    /// `admin`, `developer` or `viewer` (owners are made, not invited).
    pub role: OrgRole,
    pub invited_by: String,
    pub now: i64,
}

/// A project is a Hub tenant: `ref` is the tenant id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub org_id: String,
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub name: String,
    /// `active` or `deleted`.
    pub status: String,
    pub settings: Value,
    pub settings_version: i64,
    pub created_at: i64,
    pub created_by: String,
    pub deleted_at: Option<i64>,
}

impl Project {
    pub fn active(&self) -> bool {
        self.status == "active"
    }
}

/// A change to a project: a new name, and settings merged into the old ones
/// (a `null` value removes a setting).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ProjectPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub settings: Option<Value>,
}

/// What an API key may do. Each scope maps to a data-plane role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyScope {
    Read,
    Write,
    Agent,
    Admin,
}

impl KeyScope {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyScope::Read => "read",
            KeyScope::Write => "write",
            KeyScope::Agent => "agent",
            KeyScope::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Result<KeyScope> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" => Ok(KeyScope::Read),
            "write" => Ok(KeyScope::Write),
            "agent" => Ok(KeyScope::Agent),
            "admin" => Ok(KeyScope::Admin),
            other => Err(Error::invalid(format!("unknown key scope '{other}' (read, write, agent, admin)"))),
        }
    }

    /// `read` → viewer, `write` → member, `agent` → agent, `admin` → admin.
    pub fn role(self) -> Role {
        match self {
            KeyScope::Read => Role::Viewer,
            KeyScope::Write => Role::Member,
            KeyScope::Agent => Role::Agent,
            KeyScope::Admin => Role::Admin,
        }
    }

    /// The roles of a key: the union of its scopes' roles, in scope order.
    pub fn roles(scopes: &[KeyScope]) -> Vec<Role> {
        let mut out: Vec<Role> = Vec::new();
        for s in scopes {
            let r = s.role();
            if !out.contains(&r) {
                out.push(r);
            }
        }
        out
    }
}

/// An API key as stored: the prefix is shown, the secret never is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub project_id: String,
    pub name: String,
    /// `onto_sk_` + 8 characters.
    pub prefix: String,
    /// Lowercase hex SHA-256(pepper || full key).
    #[serde(skip)]
    pub hash: String,
    pub scopes: Vec<KeyScope>,
    pub workspaces: Vec<String>,
    pub impersonate: bool,
    pub impersonate_roles: Vec<Role>,
    pub created_at: i64,
    pub created_by: String,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub revoked_by: Option<String>,
    pub last_used_at: Option<i64>,
    pub rotated_from: Option<String>,
}

impl ApiKey {
    /// Not revoked and not expired at `now`.
    pub fn usable(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|e| e > now)
    }
}

/// A key to insert; the caller computed the prefix and hash.
#[derive(Clone, Debug)]
pub struct NewKey {
    pub project_id: String,
    pub name: String,
    pub prefix: String,
    pub hash: String,
    pub scopes: Vec<KeyScope>,
    pub workspaces: Vec<String>,
    pub impersonate: bool,
    pub impersonate_roles: Vec<Role>,
    pub created_by: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub rotated_from: Option<String>,
}

/// A pending OIDC login (state → verifier, nonce). Single use, 10 minutes.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingLogin {
    pub state_hash: String,
    pub provider: String,
    pub nonce: String,
    pub code_verifier: String,
    pub binding_hash: String,
    pub invite_code_hash: Option<String>,
    pub return_to: String,
    pub created_at: i64,
    pub expires_at: i64,
}

/// A one-time ticket the OIDC callback hands to the console. Single use, 60 s.
#[derive(Clone, Debug, PartialEq)]
pub struct LoginTicket {
    pub ticket_hash: String,
    pub user_id: String,
    pub binding_hash: String,
    pub expires_at: i64,
}

/// A vault envelope (section 8.2 of docs/CONTROL_PLANE.md): ciphertext, but
/// still never shown.
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed(pub Vec<u8>);

impl std::fmt::Debug for Sealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sealed({} bytes)", self.0.len())
    }
}

/// The LLM providers a project may choose (`mock` only in dev mode).
pub const LLM_PROVIDERS: [&str; 6] = ["anthropic", "openai", "ollama", "openai-compatible", "none", "mock"];

/// A project's own LLM provider, budget and rate limit (`project_llm`).
/// The API key is stored only sealed; responses carry `has_api_key` and
/// `api_key_last4`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectLlm {
    pub project_id: String,
    /// `anthropic`, `openai`, `ollama`, `openai-compatible`, `none` (or `mock` in dev mode).
    pub provider: String,
    /// Empty: the provider's default.
    pub model: String,
    pub base_url: Option<String>,
    #[serde(skip)]
    pub api_key_ct: Option<Sealed>,
    pub api_key_last4: Option<String>,
    pub max_output_tokens: u32,
    pub temperature: Option<f32>,
    pub effort: Option<String>,
    pub timeout_secs: u32,
    /// USD per million tokens; all zero uses the provider's list prices where known.
    pub pricing: onto_intelli::config::Pricing,
    /// Extra request headers (never credentials).
    pub headers: std::collections::BTreeMap<String, String>,
    pub monthly_budget_usd: Option<f64>,
    pub monthly_token_budget: Option<i64>,
    pub asks_per_minute: Option<i32>,
    pub updated_at: i64,
    pub updated_by: String,
}

impl ProjectLlm {
    pub fn has_api_key(&self) -> bool {
        self.api_key_ct.is_some()
    }
}

/// Usage of one project on one UTC day with one provider and model
/// (`usage_daily`). As a delta it is added to the stored row.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageDay {
    pub project_id: String,
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    pub provider: String,
    pub model: String,
    pub asks: i64,
    pub llm_calls: i64,
    pub deterministic: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub cost_usd: f64,
    pub errors: i64,
}

/// What [`ControlStore::add_usage`](crate::ControlStore::add_usage) adds to `usage_daily`.
pub type UsageDelta = UsageDay;

impl UsageDay {
    /// Add another row's counters to this one.
    pub fn add(&mut self, o: &UsageDay) {
        self.asks += o.asks;
        self.llm_calls += o.llm_calls;
        self.deterministic += o.deterministic;
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.cached_tokens += o.cached_tokens;
        self.cost_usd += o.cost_usd;
        self.errors += o.errors;
    }
}

/// One control-plane change. Never holds secrets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Assigned by the store; 0 on a new entry.
    #[serde(default)]
    pub seq: i64,
    pub ts: i64,
    pub org_id: Option<String>,
    pub project_id: Option<String>,
    /// A user id, or `system`.
    pub actor: String,
    /// `key.create`, `key.revoke`, `member.role`, `project.delete`, ...
    pub action: String,
    pub target: Option<String>,
    pub detail: Value,
}

impl AuditEntry {
    pub fn new(actor: &str, action: &str) -> Self {
        AuditEntry {
            seq: 0,
            ts: onto_core::time::now_ms(),
            org_id: None,
            project_id: None,
            actor: actor.to_string(),
            action: action.to_string(),
            target: None,
            detail: Value::Object(Default::default()),
        }
    }

    pub fn org(mut self, org: &str) -> Self {
        self.org_id = Some(org.to_string());
        self
    }

    pub fn project(mut self, p: &Project) -> Self {
        self.org_id = Some(p.org_id.clone());
        self.project_id = Some(p.id.clone());
        self
    }

    pub fn target(mut self, t: &str) -> Self {
        self.target = Some(t.to_string());
        self
    }

    pub fn detail(mut self, d: Value) -> Self {
        self.detail = d;
        self
    }
}

/// Organization slugs: `[a-z0-9-]`, 3 to 40 characters, no leading or trailing `-`.
pub fn valid_slug(s: &str) -> bool {
    (3..=40).contains(&s.len())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// A slug from a name: lowercase letters and digits, runs of anything else
/// become one `-`, at most 30 characters (room for a suffix).
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 30 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "team".into() } else { out }
}

/// Names of organizations, projects and keys: 1 to 100 characters, no control characters.
pub fn check_name(what: &str, name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 100 || n.chars().any(char::is_control) {
        return Err(Error::invalid(format!("{what} name must have 1 to 100 printable characters")));
    }
    Ok(n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_and_scopes() {
        assert!(OrgRole::Owner.at_least(OrgRole::Admin));
        assert!(!OrgRole::Developer.at_least(OrgRole::Admin));
        assert_eq!(OrgRole::parse("Developer").unwrap(), OrgRole::Developer);
        assert!(OrgRole::parse("root").is_err());
        assert_eq!(OrgRole::Viewer.principal_roles(), vec![Role::Viewer, Role::Custom("observer".into())]);
        assert_eq!(
            KeyScope::roles(&[KeyScope::Read, KeyScope::Write, KeyScope::Agent, KeyScope::Admin, KeyScope::Read]),
            vec![Role::Viewer, Role::Member, Role::Agent, Role::Admin]
        );
        assert_eq!(serde_json::to_value(KeyScope::Agent).unwrap(), "agent");
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("  Acme, Inc. "), "acme-inc");
        assert_eq!(slugify("!!!"), "team");
        assert!(valid_slug("acme-inc"));
        assert!(!valid_slug("ab"));
        assert!(!valid_slug("-ab"));
        assert!(!valid_slug("Acme"));
    }
}
