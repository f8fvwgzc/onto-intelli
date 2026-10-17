//! Principals, roles, workspace isolation and entity visibility.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::graph::Graph;
use crate::model::{Entity, Id};
use crate::schema::Dir;
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Read only.
    Viewer,
    /// Read and write.
    Member,
    /// Automation: read and write.
    Agent,
    /// Everything inside its workspaces, including restricted entities.
    Admin,
    /// Internal engine operations.
    System,
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
}

fn all_workspaces() -> Vec<String> {
    vec!["*".into()]
}

impl Principal {
    pub fn new(id: impl Into<String>, roles: Vec<Role>, workspaces: Vec<String>) -> Self {
        Self { id: id.into(), roles, workspaces, entity: None }
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

    pub fn with_entity(mut self, entity: impl Into<Id>) -> Self {
        self.entity = Some(entity.into());
        self
    }

    pub fn has(&self, role: Role) -> bool {
        self.roles.contains(&role)
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
        let mut roles: Vec<String> = self.roles.iter().map(|r| format!("{r:?}")).collect();
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
