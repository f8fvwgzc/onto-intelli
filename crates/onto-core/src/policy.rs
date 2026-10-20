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
}

fn all_workspaces() -> Vec<String> {
    vec!["*".into()]
}

impl Principal {
    pub fn new(id: impl Into<String>, roles: Vec<Role>, workspaces: Vec<String>) -> Self {
        Self { id: id.into(), roles, workspaces, entity: None, via: None }
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
            graph.neighbors(me, "member_of", Dir::Out).iter().any(|team| acl.contains(team))
        }
        _ => true,
    }
}
