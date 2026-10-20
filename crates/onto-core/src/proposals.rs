//! Schema change proposals: propose → review (diff and impact preview) →
//! approve or reject by a configured role → apply atomically.
//!
//! Anyone allowed to propose (agents included) files a change set with a
//! rationale. Approvers (`governance.approve_roles`, never agents, never the
//! proposer unless `allow_self_approval`) approve or reject. When a proposal
//! collects `governance.required_approvals`, it is applied in the same
//! transaction that records it as applied. Every step is audited as a
//! `schema.proposal.*` event.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::engine::{ActionContext, Extra};
use crate::live_schema::{SchemaChangeSet, SchemaPlan, VersionOrigin};
use crate::model::Id;
use crate::policy::{Principal, Role};
use crate::time::{fmt_ts, new_id, now_ms};
use crate::{Error, Ontology, Result};

/// KV namespace holding one proposal per key (its id).
pub const NS_PROPOSALS: &str = "schema.proposal";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Waiting for review.
    Open,
    /// Approved and applied as a schema version.
    Applied,
    Rejected,
    /// Approved, but the change no longer applied (the schema moved on, or
    /// it needs a migration); revise it to reopen.
    Failed,
    Withdrawn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    Reject,
    Comment,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proposer {
    pub id: String,
    #[serde(default)]
    pub roles: Vec<Role>,
    /// Filed by automation (a principal with the `agent` role).
    #[serde(default)]
    pub agent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<Id>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Review {
    pub by: String,
    pub decision: Decision,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub comment: String,
    /// The schema version the reviewer previewed against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_version: Option<u64>,
    pub ts: i64,
    pub at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaProposal {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub rationale: String,
    pub change_set: SchemaChangeSet,
    pub proposer: Proposer,
    pub status: ProposalStatus,
    /// Schema version the proposal was planned against.
    pub base_version: u64,
    /// Impact report computed when filed (or revised).
    pub plan: SchemaPlan,
    #[serde(default)]
    pub reviews: Vec<Review>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Where it came from (`mcp`, `api`, `ui`, an assistant's name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Workspace its audit events are recorded in.
    #[serde(default)]
    pub workspace: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub revision: u32,
}

impl SchemaProposal {
    pub fn approvals(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for r in &self.reviews {
            if r.decision == Decision::Approve && !out.contains(&r.by.as_str()) {
                out.push(&r.by);
            }
        }
        out
    }
}

/// A proposal as read, with a fresh preview when the schema has moved on.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProposalView {
    #[serde(flatten)]
    pub proposal: SchemaProposal,
    pub approvals: usize,
    pub required_approvals: usize,
    /// The active schema version differs from the one it was planned against.
    pub stale: bool,
    /// Plan against the active schema (open proposals only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<SchemaPlan>,
    pub active_version: u64,
}

/// What a proposer files.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProposalInput {
    pub title: String,
    #[serde(default)]
    pub rationale: String,
    #[serde(flatten)]
    pub change_set: SchemaChangeSet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Result of a review: the proposal afterwards, and the version when it applied.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewOutcome {
    pub proposal: ProposalView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<crate::live_schema::SchemaApplied>,
}

fn proposer_of(p: &Principal) -> Proposer {
    Proposer { id: p.id.clone(), roles: p.roles.clone(), agent: p.is_agent(), entity: p.entity.clone() }
}

impl Ontology {
    fn load_proposal(&self, id: &str) -> Result<SchemaProposal> {
        let v =
            self.store().kv_get(NS_PROPOSALS, id)?.ok_or_else(|| Error::not_found(format!("schema proposal {id}")))?;
        serde_json::from_value(v).map_err(|e| Error::Storage(format!("proposal {id}: {e}")))
    }

    fn view(&self, prop: SchemaProposal, p: &Principal) -> Result<ProposalView> {
        let active = self.schema_version();
        let stale = prop.base_version != active;
        let preview = if prop.status == ProposalStatus::Open && stale {
            Some(self.schema_plan(p, &prop.change_set)?)
        } else {
            None
        };
        Ok(ProposalView {
            approvals: prop.approvals().len(),
            required_approvals: self.options().governance.required_approvals.max(1),
            stale,
            preview,
            active_version: active,
            proposal: prop,
        })
    }

    /// Persist a proposal and its audit event atomically.
    fn save_proposal(&self, ctx: &ActionContext, prop: &SchemaProposal, action: &str, payload: Value) -> Result<()> {
        let event = self.event(ctx, &prop.workspace, action, None, now_ms(), payload, Vec::new());
        let extra = Extra {
            kv: vec![(NS_PROPOSALS.into(), prop.id.clone(), serde_json::to_value(prop)?)],
            events: vec![event],
            ..Default::default()
        };
        let mut g = self.write_lock();
        self.commit_locked(&mut g, ctx, Vec::new(), extra)?;
        Ok(())
    }

    fn deny(&self, ctx: &ActionContext, action: &str, reason: String) -> Error {
        let mut c = ctx.clone();
        c.intent = Some(action.to_string());
        self.audit_denied(&c, &reason, now_ms());
        Error::Denied(reason)
    }

    /// File a schema change proposal. Invalid change sets are refused with
    /// their errors; breaking ones are accepted and show what they need.
    pub fn propose_schema(&self, ctx: &ActionContext, input: ProposalInput) -> Result<ProposalView> {
        let gov = &self.options().governance;
        if !gov.can_propose(&ctx.principal) {
            return Err(self.deny(
                ctx,
                "schema.propose",
                format!("{} may not propose schema changes", ctx.principal.id),
            ));
        }
        if input.title.trim().is_empty() {
            return Err(Error::invalid("a proposal needs a title"));
        }
        let plan = self.schema_plan(&ctx.principal, &input.change_set)?;
        if !plan.valid {
            return Err(Error::Invalid(format!("the proposed change does not validate: {}", plan.errors.join("; "))));
        }
        let now = now_ms();
        let prop = SchemaProposal {
            id: new_id("prop"),
            title: input.title.trim().to_string(),
            rationale: input.rationale,
            base_version: plan.base_version,
            change_set: input.change_set,
            proposer: proposer_of(&ctx.principal),
            status: ProposalStatus::Open,
            plan,
            reviews: Vec::new(),
            applied_version: None,
            error: None,
            source: input.source.or_else(|| ctx.source.clone()),
            workspace: ctx.workspace.clone(),
            created_at: now,
            updated_at: now,
            revision: 1,
        };
        self.save_proposal(
            ctx,
            &prop,
            "schema.proposal.create",
            json!({
                "proposal": prop.id,
                "title": prop.title,
                "agent": prop.proposer.agent,
                "breaking": prop.plan.breaking,
                "needs_migration": prop.plan.needs_migration.len(),
                "changes": prop.change_set.changes.iter().map(|c| format!("{} {}", c.op(), c.target())).collect::<Vec<_>>(),
            }),
        )?;
        self.view(prop, &ctx.principal)
    }

    /// Proposals, newest first, optionally by status.
    pub fn list_proposals(
        &self,
        p: &Principal,
        status: Option<ProposalStatus>,
        limit: usize,
    ) -> Result<Vec<ProposalView>> {
        if !p.allows(crate::policy::Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let mut all: Vec<SchemaProposal> = self
            .store()
            .kv_scan(NS_PROPOSALS, "")?
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_value::<SchemaProposal>(v).ok())
            .filter(|x| status.is_none_or(|s| x.status == s))
            .collect();
        all.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
        all.truncate(limit);
        let active = self.schema_version();
        let required = self.options().governance.required_approvals.max(1);
        // Listings skip re-planning; `get_proposal` returns a fresh preview.
        Ok(all
            .into_iter()
            .map(|x| ProposalView {
                approvals: x.approvals().len(),
                required_approvals: required,
                stale: x.base_version != active,
                preview: None,
                active_version: active,
                proposal: x,
            })
            .collect())
    }

    /// One proposal with a fresh preview when the schema has moved on.
    pub fn get_proposal(&self, p: &Principal, id: &str) -> Result<ProposalView> {
        if !p.allows(crate::policy::Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let prop = self.load_proposal(id)?;
        self.view(prop, p)
    }

    /// Approve, reject or comment. The approval that reaches
    /// `required_approvals` applies the change set atomically.
    pub fn review_proposal(
        &self,
        ctx: &ActionContext,
        id: &str,
        decision: Decision,
        comment: &str,
        seen_version: Option<u64>,
    ) -> Result<ReviewOutcome> {
        let gov = self.options().governance.clone();
        let mut prop = self.load_proposal(id)?;
        let p = &ctx.principal;
        let action = match decision {
            Decision::Approve => "schema.proposal.approve",
            Decision::Reject => "schema.proposal.reject",
            Decision::Comment => "schema.proposal.comment",
        };
        if decision == Decision::Comment {
            if !p.allows(crate::policy::Access::Read) {
                return Err(Error::Denied(format!("{} has no read role", p.id)));
            }
        } else {
            if !gov.can_approve(p) {
                let reason = if p.is_agent() {
                    format!("{} is an agent: agents may propose schema changes but not review them", p.id)
                } else {
                    format!(
                        "{} may not review schema proposals (needs one of: {})",
                        p.id,
                        gov.approve_roles.iter().map(Role::as_str).collect::<Vec<_>>().join(", ")
                    )
                };
                return Err(self.deny(ctx, action, reason));
            }
            if decision == Decision::Approve && !gov.allow_self_approval && p.id == prop.proposer.id {
                return Err(self.deny(ctx, action, format!("{} cannot approve their own proposal", p.id)));
            }
            if prop.status != ProposalStatus::Open {
                return Err(Error::Conflict(format!("proposal {id} is {:?}, not open", prop.status).to_lowercase()));
            }
            if let Some(seen) = seen_version {
                let active = self.schema_version();
                if seen != active {
                    return Err(Error::Conflict(format!(
                        "the schema moved from version {seen} to {active} since you reviewed; review the fresh preview"
                    )));
                }
            }
        }
        let now = now_ms();
        if decision == Decision::Approve && prop.approvals().contains(&p.id.as_str()) {
            return Err(Error::Conflict(format!("{} already approved {id}", p.id)));
        }
        prop.reviews.push(Review {
            by: p.id.clone(),
            decision,
            comment: comment.to_string(),
            seen_version,
            ts: now,
            at: fmt_ts(now),
        });
        prop.updated_at = now;
        let payload = json!({ "proposal": prop.id, "title": prop.title, "comment": comment });
        match decision {
            Decision::Comment => {
                self.save_proposal(ctx, &prop, action, payload)?;
                Ok(ReviewOutcome { proposal: self.view(prop, p)?, applied: None })
            }
            Decision::Reject => {
                prop.status = ProposalStatus::Rejected;
                self.save_proposal(ctx, &prop, action, payload)?;
                Ok(ReviewOutcome { proposal: self.view(prop, p)?, applied: None })
            }
            Decision::Approve => {
                let required = gov.required_approvals.max(1);
                if prop.approvals().len() < required {
                    self.save_proposal(ctx, &prop, action, payload)?;
                    return Ok(ReviewOutcome { proposal: self.view(prop, p)?, applied: None });
                }
                // Enough approvals: apply, recording the proposal as applied in
                // the same transaction as the schema version.
                let approve_event = self.event(ctx, &prop.workspace, action, None, now, payload, Vec::new());
                let mut set = prop.change_set.clone();
                if set.message.is_empty() {
                    set.message = prop.title.clone();
                }
                let origin = VersionOrigin {
                    source: "proposal",
                    proposal: Some(prop.id.clone()),
                    kv: Vec::new(),
                    config: None,
                    events: vec![approve_event],
                    proposal_record: Some(Box::new(prop.clone())),
                };
                match self.schema_commit(ctx, set, origin) {
                    Ok(applied) => {
                        let prop = self.load_proposal(id)?;
                        Ok(ReviewOutcome { proposal: self.view(prop, p)?, applied: Some(applied) })
                    }
                    Err(e) => {
                        prop.status = ProposalStatus::Failed;
                        prop.error = Some(e.to_string());
                        self.save_proposal(
                            ctx,
                            &prop,
                            "schema.proposal.fail",
                            json!({ "proposal": prop.id, "error": e.to_string() }),
                        )?;
                        Err(e)
                    }
                }
            }
        }
    }

    /// The proposer (or an approver) withdraws an open proposal.
    pub fn withdraw_proposal(&self, ctx: &ActionContext, id: &str) -> Result<ProposalView> {
        let mut prop = self.load_proposal(id)?;
        let p = &ctx.principal;
        if p.id != prop.proposer.id && !self.options().governance.can_approve(p) {
            return Err(self.deny(ctx, "schema.proposal.withdraw", format!("{} cannot withdraw {id}", p.id)));
        }
        if !matches!(prop.status, ProposalStatus::Open | ProposalStatus::Failed) {
            return Err(Error::Conflict(format!("proposal {id} is {:?}", prop.status).to_lowercase()));
        }
        prop.status = ProposalStatus::Withdrawn;
        prop.updated_at = now_ms();
        self.save_proposal(ctx, &prop, "schema.proposal.withdraw", json!({ "proposal": prop.id }))?;
        self.view(prop, p)
    }

    /// Replace the change set of an open or failed proposal; approvals reset.
    pub fn revise_proposal(&self, ctx: &ActionContext, id: &str, input: ProposalInput) -> Result<ProposalView> {
        let mut prop = self.load_proposal(id)?;
        let p = &ctx.principal;
        if p.id != prop.proposer.id && !self.options().governance.can_approve(p) {
            return Err(self.deny(ctx, "schema.proposal.revise", format!("{} cannot revise {id}", p.id)));
        }
        if !matches!(prop.status, ProposalStatus::Open | ProposalStatus::Failed) {
            return Err(Error::Conflict(format!("proposal {id} is {:?}", prop.status).to_lowercase()));
        }
        let plan = self.schema_plan(p, &input.change_set)?;
        if !plan.valid {
            return Err(Error::Invalid(format!("the revised change does not validate: {}", plan.errors.join("; "))));
        }
        if !input.title.trim().is_empty() {
            prop.title = input.title.trim().to_string();
        }
        if !input.rationale.is_empty() {
            prop.rationale = input.rationale;
        }
        prop.change_set = input.change_set;
        prop.base_version = plan.base_version;
        prop.plan = plan;
        prop.status = ProposalStatus::Open;
        prop.error = None;
        prop.revision += 1;
        prop.updated_at = now_ms();
        // Approvals were for the previous change set.
        prop.reviews.retain(|r| r.decision != Decision::Approve);
        self.save_proposal(
            ctx,
            &prop,
            "schema.proposal.revise",
            json!({ "proposal": prop.id, "revision": prop.revision, "by": p.id }),
        )?;
        self.view(prop, p)
    }
}
