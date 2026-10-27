//! The change feed's actions (docs/design/EVENTS.md, Part A): `changes.poll`
//! reads one page of records after a cursor, filtered for the caller and by
//! the call's filter; `changes.head` says where the feed is; `changes.capture`
//! switches recording for the tenant (admins of every workspace, never
//! agents).
//!
//! The records themselves are computed by onto-core in the commit that made
//! the change. A cursor older than the floor is reported `expired` with a
//! message instead of a page that silently starts later, and a commit that
//! was written without records arrives as a `gap` record. Consistent resync
//! is not offered: after an expiry or a gap, re-read with `entity.find`; the
//! result is not pinned to a cursor.

use serde::Deserialize;
use serde_json::{Value, json};

use onto_core::changes::{ChangeFilter, ChangeOp, ChangesMeta, Cursor, FieldMode, MAX_PAGE};
use onto_core::{Access, Error, Principal, Result, one_or_many};

use crate::actions::{parse, to_json};
use crate::intelli::Intelli;

/// The change feed actions, dispatched by [`Intelli::changes_call`].
pub const CALLS: &[&str] = &["changes.poll", "changes.head", "changes.capture"];

/// What an expired page says, besides `expired: true`.
pub const EXPIRED_MESSAGE: &str = "the cursor is older than the floor: records after it may be missing. Consistent \
     resync is not offered; re-read with entity.find (the result is not pinned to a cursor) and resume from the head.";

/// The filter keys of `changes.poll` (and, later, of streams): lists accept
/// one string or many; `exclude_migrations` is the negative form gRPC needs,
/// because proto3 drops a false `include_migrations`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FilterIn {
    #[serde(deserialize_with = "one_or_many")]
    pub kinds: Vec<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub ops: Vec<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub workspaces: Vec<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub ids: Vec<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub rels: Vec<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub props: Vec<String>,
    #[serde(deserialize_with = "one_or_many")]
    pub sources: Vec<String>,
    pub fields: Option<String>,
    pub include_migrations: Option<bool>,
    pub exclude_migrations: Option<bool>,
}

impl FilterIn {
    /// The engine filter; unknown ops and field modes, and contradictory
    /// migration flags, are refused with the valid values.
    pub fn into_filter(self) -> Result<ChangeFilter> {
        let ops = self.ops.iter().map(|o| o.trim().parse::<ChangeOp>()).collect::<Result<Vec<_>>>()?;
        let fields = match self.fields.as_deref().map(str::trim) {
            None | Some("") | Some("full") => FieldMode::Full,
            Some("ids") => FieldMode::Ids,
            Some(other) => return Err(Error::invalid(format!("fields '{other}' must be full or ids"))),
        };
        let include_migrations = match (self.include_migrations, self.exclude_migrations) {
            (Some(i), Some(e)) if i == e => {
                return Err(Error::invalid("include_migrations and exclude_migrations contradict each other"));
            }
            (Some(i), _) => i,
            (None, Some(e)) => !e,
            (None, None) => true,
        };
        let f = ChangeFilter {
            kinds: self.kinds,
            ops,
            workspaces: self.workspaces,
            ids: self.ids,
            rels: self.rels,
            props: self.props,
            sources: self.sources,
            include_migrations,
            fields,
        };
        f.check()?;
        Ok(f)
    }
}

/// Where a read starts when no cursor is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartAt {
    /// The oldest record kept (the floor's commit on).
    Earliest,
    /// The head: only what is committed afterwards.
    Now,
}

impl std::str::FromStr for StartAt {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "earliest" => Ok(StartAt::Earliest),
            "now" => Ok(StartAt::Now),
            other => Err(Error::invalid(format!("from '{other}' must be earliest or now"))),
        }
    }
}

#[derive(Deserialize)]
struct PollIn {
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(flatten)]
    filter: FilterIn,
}

#[derive(Deserialize)]
struct CaptureIn {
    #[serde(default)]
    on: Option<bool>,
    /// The negative form gRPC needs (proto3 drops `on: false`).
    #[serde(default)]
    off: Option<bool>,
    #[serde(default)]
    workspace: Option<String>,
}

impl CaptureIn {
    fn switch(&self) -> Result<bool> {
        match (self.on, self.off) {
            (Some(on), Some(off)) if on == off => {
                Err(Error::invalid("changes.capture: on and off contradict each other"))
            }
            (Some(on), _) => Ok(on),
            (None, Some(off)) => Ok(!off),
            (None, None) => Err(Error::invalid("changes.capture: give on: true or on: false (or off: true)")),
        }
    }
}

/// `changes.head` and `changes.capture` answer with the feed's position.
fn head_json(m: &ChangesMeta) -> Value {
    json!({ "cursor": m.head, "head": m.head.seq, "floor": m.floor, "capture": m.capture })
}

fn read_check(p: &Principal) -> Result<()> {
    if !p.allows(Access::Read) {
        return Err(Error::Denied(format!("{} has no read role", p.id)));
    }
    Ok(())
}

impl Intelli {
    /// The change feed actions ([`CALLS`]). Synchronous: the store calls
    /// block, so the caller runs this off the async workers.
    pub(crate) fn changes_call(&self, p: &Principal, action: &str, input: Value) -> Result<Value> {
        let onto = self.ontology();
        match action {
            "changes.poll" => {
                let i: PollIn = parse(action, input)?;
                let f = i.filter.into_filter().map_err(|e| prefixed(action, e))?;
                let limit = i.limit.unwrap_or(self.config().changes.page).clamp(1, MAX_PAGE);
                let page = match (i.after.filter(|a| !a.trim().is_empty()), i.from.filter(|f| !f.trim().is_empty())) {
                    (Some(_), Some(_)) => {
                        return Err(Error::invalid("changes.poll: give after or from, not both"));
                    }
                    (Some(after), None) => {
                        let after: Cursor = after.parse().map_err(|e| prefixed(action, e))?;
                        onto.changes_page(p, after, &f, limit)?
                    }
                    (None, from) => {
                        match from.as_deref().map(str::parse::<StartAt>).transpose().map_err(|e| prefixed(action, e))? {
                            None | Some(StartAt::Earliest) => onto.changes_page_earliest(p, &f, limit)?,
                            Some(StartAt::Now) => {
                                read_check(p)?;
                                onto.changes_page(p, onto.change_head()?, &f, limit)?
                            }
                        }
                    }
                };
                let mut out = to_json(&page)?;
                if page.expired {
                    out["message"] = json!(EXPIRED_MESSAGE);
                }
                Ok(out)
            }
            "changes.head" => {
                read_check(p)?;
                Ok(head_json(&onto.store().changes_meta()?))
            }
            "changes.capture" => {
                let i: CaptureIn = parse(action, input)?;
                let on = i.switch()?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                onto.set_capture(&ctx, on)?;
                Ok(head_json(&onto.store().changes_meta()?))
            }
            other => Err(Error::invalid(format!("unknown action '{other}' (see system.actions)"))),
        }
    }
}

/// Name the action in an input error, as `parse` does.
fn prefixed(action: &str, e: Error) -> Error {
    match e {
        Error::Invalid(m) => Error::invalid(format!("{action}: {m}")),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(v: Value) -> Result<ChangeFilter> {
        serde_json::from_value::<FilterIn>(v).unwrap().into_filter()
    }

    #[test]
    fn filter_keys_take_one_or_many_and_refuse_unknown_values() {
        let f = filter(json!({ "kinds": "issue", "ops": ["entity.create", "link.delete"], "fields": "ids" })).unwrap();
        assert_eq!(f.kinds, vec!["issue"]);
        assert_eq!(f.ops, vec![ChangeOp::EntityCreate, ChangeOp::LinkDelete]);
        assert_eq!(f.fields, FieldMode::Ids);
        assert!(f.include_migrations, "migrations are included by default");
        assert!(!filter(json!({ "exclude_migrations": true })).unwrap().include_migrations);
        assert!(!filter(json!({ "include_migrations": false })).unwrap().include_migrations);
        let e = filter(json!({ "ops": ["entity.created"] })).unwrap_err();
        assert!(e.to_string().contains("entity.create,"), "{e}");
        assert!(filter(json!({ "fields": "some" })).unwrap_err().to_string().contains("full or ids"));
        assert!(filter(json!({ "include_migrations": true, "exclude_migrations": true })).is_err());
        let ids: Vec<String> = (0..1001).map(|n| format!("issue:{n}")).collect();
        assert!(filter(json!({ "ids": ids })).unwrap_err().to_string().contains("1000"));
    }

    #[test]
    fn capture_needs_one_clear_switch() {
        let c = |v: Value| serde_json::from_value::<CaptureIn>(v).unwrap().switch();
        assert!(c(json!({ "on": true })).unwrap());
        assert!(!c(json!({ "on": false })).unwrap());
        assert!(!c(json!({ "off": true })).unwrap());
        assert!(c(json!({})).is_err());
        assert!(c(json!({ "on": true, "off": true })).is_err());
    }
}
