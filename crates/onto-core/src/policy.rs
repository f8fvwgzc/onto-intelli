//! Principals, roles, workspace isolation and entity visibility.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::graph::Graph;
use crate::model::{Entity, Id};
use crate::schema::Dir;
use crate::{Error, Result};

/// A role held by a principal. The built-in roles carry fixed meaning; any
/// other name (`schema_admin`, `approver`, ...) is a custom role that only
/// grants what configuration ties to it (read access, approving proposals).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    /// Read only.
    Viewer,
    /// Read and write.
    Member,
    /// Automation: read and write. Agents may propose schema changes but never
    /// apply or approve them.
    Agent,
    /// Everything inside its workspaces, including restricted entities.
    Admin,
    /// Internal engine operations.
    System,
    /// A role named in configuration.
    Custom(String),
}

impl Role {
    pub fn as_str(&self) -> &str {
        match self {
            Role::Viewer => "viewer",
            Role::Member => "member",
            Role::Agent => "agent",
            Role::Admin => "admin",
            Role::System => "system",
            Role::Custom(name) => name,
        }
    }

    /// Parse a role name; unknown names become custom roles.
    pub fn parse(name: &str) -> Result<Role> {
        let n = name.trim().to_ascii_lowercase();
        Ok(match n.as_str() {
            "viewer" => Role::Viewer,
            "member" => Role::Member,
            "agent" => Role::Agent,
            "admin" => Role::Admin,
            "system" => Role::System,
            "" => return Err(Error::invalid("empty role name")),
            other => {
                if !other.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
                    || other.len() > 64
                {
                    return Err(Error::invalid(format!("invalid role name '{name}'")));
                }
                Role::Custom(other.to_string())
            }
        })
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Role {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        Role::parse(&name).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Delete,
    Admin,
}

/// Who is asking. Every read and write is evaluated against a principal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Principal {
    pub id: String,
    #[serde(default)]
    pub roles: Vec<Role>,
    /// Workspaces this principal may touch; `"*"` means all.
    #[serde(default = "all_workspaces")]
    pub workspaces: Vec<String>,
    /// The ontology entity representing this principal (`person:kenji`), used
    /// for "my issues" questions and restricted-entity ACLs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<Id>,
    /// The service that asserted this principal (impersonation), recorded
    /// as the source of its audit events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// The cascade a trusted service says this request belongs to
    /// (`x-onto-cause-depth` and `x-onto-cause-run` from a config token,
    /// docs/design/EVENTS.md C.6): contexts built for it carry it, so its
    /// changes keep the depth and a loop through another service still
    /// meets the depth limit. Never read from or written to JSON.
    #[serde(skip)]
    pub cause: Option<crate::changes::RunCause>,
}

fn all_workspaces() -> Vec<String> {
    vec!["*".into()]
}

impl Principal {
    pub fn new(id: impl Into<String>, roles: Vec<Role>, workspaces: Vec<String>) -> Self {
        Self { id: id.into(), roles, workspaces, entity: None, via: None, cause: None }
    }

    pub fn system() -> Self {
        Self::new("system", vec![Role::System], all_workspaces())
    }

    pub fn admin(id: impl Into<String>) -> Self {
        Self::new(id, vec![Role::Admin], all_workspaces())
    }

    pub fn member(id: impl Into<String>, workspace: impl Into<String>) -> Self {
        Self::new(id, vec![Role::Member], vec![workspace.into()])
    }

    pub fn viewer(id: impl Into<String>, workspace: impl Into<String>) -> Self {
        Self::new(id, vec![Role::Viewer], vec![workspace.into()])
    }

    /// Build from role and workspace names (`["member", "approver"]`).
    pub fn named(id: impl Into<String>, roles: &[&str], workspaces: &[&str]) -> Result<Self> {
        let roles = roles.iter().map(|r| Role::parse(r)).collect::<Result<Vec<_>>>()?;
        Ok(Self::new(id, roles, workspaces.iter().map(|w| w.to_string()).collect()))
    }

    pub fn with_entity(mut self, entity: impl Into<Id>) -> Self {
        self.entity = Some(entity.into());
        self
    }

    pub fn has(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }

    pub fn has_role(&self, role: &Role) -> bool {
        self.roles.contains(role)
    }

    /// Holds any of `roles` (by name).
    pub fn has_any(&self, roles: &[Role]) -> bool {
        roles.iter().any(|r| self.roles.contains(r))
    }

    /// Automation principals (agents) may propose but never approve or apply
    /// schema changes, whatever other roles they hold.
    pub fn is_agent(&self) -> bool {
        self.has(Role::Agent)
    }

    pub fn is_privileged(&self) -> bool {
        self.has(Role::Admin) || self.has(Role::System)
    }

    /// May read traces, sessions and metrics: `admin`, `system`, or the
    /// custom roles `developer` and `observer` (console developers and viewers).
    pub fn can_observe(&self) -> bool {
        self.is_privileged()
            || self.roles.iter().any(|r| matches!(r, Role::Custom(n) if n == "developer" || n == "observer"))
    }

    pub fn in_workspace(&self, ws: &str) -> bool {
        self.workspaces.iter().any(|w| w == "*" || w == ws)
    }

    pub fn allows(&self, access: Access) -> bool {
        if self.is_privileged() {
            return true;
        }
        match access {
            Access::Read => !self.roles.is_empty(),
            Access::Write | Access::Delete => self.has(Role::Member) || self.has(Role::Agent),
            Access::Admin => false,
        }
    }

    /// A stable fingerprint of what this principal can see (cache keys).
    pub fn scope_key(&self) -> String {
        let mut roles: Vec<String> = self.roles.iter().map(|r| r.to_string()).collect();
        roles.sort();
        format!("{}|{}|{}", self.id, roles.join(","), self.workspaces.join(","))
    }
}

/// Principal ids that belong to the engine's own service principals. No
/// credential may act as them (`x-onto-act-as`), and no configured token may
/// carry them: otherwise a caller could write records attributed to an
/// automation, a webhook or a connector, and read or undo what they wrote.
/// A prefix ending in `:` reserves every id that starts with it; `system`
/// reserves `system` itself and `system:<anything>`.
pub const RESERVED_ID_PREFIXES: &[&str] = &["automation:", "webhook:", "connector:", "system"];

/// Whether `id` is a reserved service principal id ([`RESERVED_ID_PREFIXES`]),
/// compared case-insensitively and ignoring surrounding whitespace.
pub fn is_reserved_id(id: &str) -> bool {
    let id = id.trim().to_ascii_lowercase();
    RESERVED_ID_PREFIXES.iter().any(|p| {
        if p.ends_with(':') {
            id.starts_with(p)
        } else {
            id == *p || id.strip_prefix(p).is_some_and(|rest| rest.starts_with(':'))
        }
    })
}

/// What a configured principal may do, without its id: the roles and
/// workspaces a webhook endpoint, writeback target or automation acts with
/// (`run_as = { roles = [...], workspaces = [...] }`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopePrincipal {
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub workspaces: Vec<String>,
}

impl ScopePrincipal {
    pub fn new(roles: Vec<Role>, workspaces: Vec<String>) -> Self {
        Self { roles, workspaces }
    }

    /// The principal `id` acting with this scope.
    pub fn principal(&self, id: impl Into<String>) -> Principal {
        Principal::new(id, self.roles.clone(), self.workspaces.clone())
    }
}

/// Whether `a` may configure something that acts as `b`: `a` can already do
/// everything `b` can. Required on every mutating outbound operation
/// (webhook endpoints, writeback targets, automation enablement), so a
/// narrower admin cannot redirect a broader principal's output.
///
/// - `a` is not an agent, and `b` holds neither `system` nor `agent`.
/// - Every workspace of `b` is one of `a`'s (`b` holding `*` needs `a` to hold `*`).
/// - `a` admin (or system) in those workspaces covers every role of `b`.
///   Otherwise `viewer` in `b` needs read access, `member` write access,
///   every custom role of `b` must be held by `a`, and `admin` in `b` needs
///   `a` admin.
pub fn dominates(a: &Principal, b: &ScopePrincipal) -> bool {
    if a.is_agent() || b.roles.iter().any(|r| matches!(r, Role::System | Role::Agent)) {
        return false;
    }
    let a_all = a.workspaces.iter().any(|w| w == "*");
    for w in &b.workspaces {
        let covered = if w == "*" { a_all } else { a.in_workspace(w) };
        if !covered {
            return false;
        }
    }
    if a.is_privileged() {
        return true;
    }
    b.roles.iter().all(|r| match r {
        Role::Viewer => a.allows(Access::Read),
        Role::Member => a.allows(Access::Write),
        Role::Admin | Role::System | Role::Agent => false,
        Role::Custom(_) => a.has_role(r),
    })
}

/// Authorize `access` in `ws` (and on `entity`, when given).
pub fn check(p: &Principal, access: Access, ws: &str, entity: Option<&Entity>, graph: &Graph) -> Result<()> {
    if !p.in_workspace(ws) {
        return Err(Error::Denied(format!("{} has no access to workspace {ws}", p.id)));
    }
    if !p.allows(access) {
        return Err(Error::Denied(format!("{} may not {:?} in {ws}", p.id, access).to_lowercase()));
    }
    if let Some(e) = entity
        && !visible(p, e, graph)
    {
        return Err(Error::Denied(format!("{} is restricted", e.id)));
    }
    Ok(())
}

/// Link types [`visible`] follows to grant access to restricted entities: a
/// `member_of` link to a team named in an entity's `acl` makes it visible.
pub const ACL_LINK_TYPES: &[&str] = &["member_of"];

/// Only admins (and the system) create or delete links that grant access:
/// otherwise any member could join a team named in a restricted entity's
/// `acl` and read it.
pub fn check_acl_link(p: &Principal, rel: &str) -> Result<()> {
    if ACL_LINK_TYPES.contains(&rel) && !p.is_privileged() {
        return Err(Error::Denied(format!(
            "{} may not change {rel} links: they grant access to restricted entities (admins only)",
            p.id
        )));
    }
    Ok(())
}

/// Entities with `visibility = "restricted"` are only visible to principals
/// (or their teams) listed in the `acl` property, and to admins.
pub fn visible(p: &Principal, e: &Entity, graph: &Graph) -> bool {
    if !p.in_workspace(&e.workspace) {
        return false;
    }
    if p.is_privileged() {
        return true;
    }
    match e.prop_str("visibility") {
        Some("restricted") | Some("private") => {
            let acl: Vec<&str> = e
                .props
                .get("acl")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let me = p.entity.as_deref().unwrap_or(&p.id);
            if acl.contains(&me) || acl.contains(&p.id.as_str()) {
                return true;
            }
            ACL_LINK_TYPES.iter().any(|rel| graph.neighbors(me, rel, Dir::Out).iter().any(|team| acl.contains(team)))
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observers_read_traces() {
        assert!(Principal::admin("a").can_observe());
        assert!(Principal::system().can_observe());
        assert!(Principal::named("d", &["member", "developer"], &["*"]).unwrap().can_observe());
        assert!(Principal::named("v", &["viewer", "observer"], &["*"]).unwrap().can_observe());
        assert!(!Principal::member("m", "*").can_observe());
        assert!(!Principal::new("k", vec![Role::Agent], vec!["*".into()]).can_observe());
    }

    #[test]
    fn reserved_ids() {
        for id in [
            "automation:payroll",
            "webhook:hook1",
            "connector:crm",
            "system",
            "System",
            " SYSTEM ",
            "system:wake",
            "Automation:X",
            "WEBHOOK:",
        ] {
            assert!(is_reserved_id(id), "{id}");
        }
        for id in ["user:kenji", "key:onto_sk_ab", "systems-team", "systemd", "nexc-backend", "automations", "ops"] {
            assert!(!is_reserved_id(id), "{id}");
        }
    }

    fn scope(roles: &[&str], ws: &[&str]) -> ScopePrincipal {
        let p = Principal::named("x", roles, ws).unwrap();
        ScopePrincipal::new(p.roles, p.workspaces)
    }

    #[test]
    fn dominance() {
        let admin_all = Principal::admin("a");
        let admin_ws1 = Principal::new("a1", vec![Role::Admin], vec!["ws1".into()]);
        let member_ws1 = Principal::member("m", "ws1");
        let viewer_ws1 = Principal::viewer("v", "ws1");
        let triager = Principal::named("t", &["member", "triager"], &["ws1", "ws2"]).unwrap();
        let agent_admin = Principal::new("g", vec![Role::Admin, Role::Agent], vec!["*".into()]);

        // A scoped admin cannot configure something acting over every workspace.
        assert!(dominates(&admin_all, &scope(&["admin"], &["*"])));
        assert!(!dominates(&admin_ws1, &scope(&["viewer"], &["*"])));
        assert!(dominates(&admin_ws1, &scope(&["admin", "member", "triager"], &["ws1"])));
        assert!(!dominates(&admin_ws1, &scope(&["viewer"], &["ws1", "ws2"])));
        // Built-in roles by access; admin in b needs admin in a.
        assert!(dominates(&member_ws1, &scope(&["viewer", "member"], &["ws1"])));
        assert!(!dominates(&member_ws1, &scope(&["admin"], &["ws1"])));
        assert!(dominates(&viewer_ws1, &scope(&["viewer"], &["ws1"])));
        assert!(!dominates(&viewer_ws1, &scope(&["member"], &["ws1"])));
        // Custom roles must be held.
        assert!(dominates(&triager, &scope(&["member", "triager"], &["ws2"])));
        assert!(!dominates(&member_ws1, &scope(&["triager"], &["ws1"])));
        assert!(!dominates(&triager, &scope(&["approver"], &["ws1"])));
        // Agents never dominate; nobody dominates system or agent scopes.
        assert!(!dominates(&agent_admin, &scope(&["viewer"], &["ws1"])));
        assert!(!dominates(&admin_all, &scope(&["agent"], &["ws1"])));
        assert!(!dominates(&Principal::system(), &scope(&["system"], &["*"])));
        assert!(dominates(&Principal::system(), &scope(&["admin"], &["*"])));
        // The scope's own principal.
        let p = scope(&["member"], &["ws1"]).principal("automation:triage");
        assert_eq!((p.id.as_str(), p.workspaces.as_slice()), ("automation:triage", ["ws1".to_string()].as_slice()));
    }
}
