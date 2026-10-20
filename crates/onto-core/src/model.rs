//! Things (entities, links, provenance) and events.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

/// Entity id in `kind:key` form, e.g. `issue:421` or `person:kenji`.
pub type Id = String;

/// A thing in the ontology.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: Id,
    pub kind: String,
    pub workspace: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub props: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub revision: u64,
}

impl Entity {
    /// The part after `kind:`.
    pub fn key(&self) -> &str {
        self.id.split_once(':').map(|(_, k)| k).unwrap_or(&self.id)
    }

    /// Human label: the title, or the id when untitled.
    pub fn label(&self) -> &str {
        if self.title.is_empty() { &self.id } else { &self.title }
    }

    pub fn prop_str(&self, name: &str) -> Option<&str> {
        self.props.get(name).and_then(Value::as_str)
    }
}

/// Where a thing came from: uploaded, fetched, generated, derived.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// `upload`, `github`, `ai_generation`, `saas`, `manual`, `ingest`, ...
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived_from: Vec<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_by: Option<Id>,
    /// Event or execution id that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
    /// When it was produced (for connectors: when the source was crawled).
    #[serde(default)]
    pub at: i64,
    /// The connector that crawled it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
}

/// Create-or-update request for an entity.
///
/// The id is taken from `id`, else built as `kind:key`, else generated.
/// `{"id": "issue:421"}` alone is enough: the kind is read from the prefix.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EntityInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    #[serde(default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub props: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// Replace props instead of merging them into the existing entity.
    #[serde(default)]
    pub replace: bool,
    /// Only fill fields that are empty on an existing entity (connectors use
    /// this so a webhook never overwrites curated titles or properties).
    #[serde(default)]
    pub fill_only: bool,
    /// Convenience: links from this entity created in the same batch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkSpec>,
}

impl EntityInput {
    pub fn new(kind: impl Into<String>, key: impl Into<String>, title: impl Into<String>) -> Self {
        Self { kind: kind.into(), key: Some(key.into()), title: Some(title.into()), ..Default::default() }
    }

    pub fn state(mut self, state: impl Into<String>) -> Self {
        self.state = Some(state.into());
        self
    }

    pub fn prop(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.props.insert(name.into(), value.into());
        self
    }

    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    pub fn link(mut self, rel: impl Into<String>, to: impl Into<Id>) -> Self {
        self.links.push(LinkSpec { rel: rel.into(), to: to.into(), props: Map::new() });
        self
    }

    pub fn provenance(mut self, provenance: Provenance) -> Self {
        self.provenance = Some(provenance);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkSpec {
    pub rel: String,
    pub to: Id,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub props: Map<String, Value>,
}

/// A stored relation, always in its canonical (declared) direction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub from: Id,
    pub rel: String,
    pub to: Id,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub props: Map<String, Value>,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub created_at: i64,
}

/// Link request. `rel` may be an inverse name (`owned_by`); it is stored canonically.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkInput {
    pub from: Id,
    pub rel: String,
    pub to: Id,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub props: Map<String, Value>,
}

impl LinkInput {
    pub fn new(from: impl Into<Id>, rel: impl Into<String>, to: impl Into<Id>) -> Self {
        Self { from: from.into(), rel: rel.into(), to: to.into(), props: Map::new() }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    #[default]
    Success,
    Failure,
    Denied,
    Pending,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
            Outcome::Denied => "denied",
            Outcome::Pending => "pending",
        }
    }
}

/// Something that happened. Lives in the event store, not in the graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub ts: i64,
    pub workspace: String,
    pub actor: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default)]
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived_from: Vec<Id>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
}

/// Request to record an event from outside (webhooks, UI actions, agents).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EventInput {
    pub action: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub target: Option<Id>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub outcome: Outcome,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub derived_from: Vec<Id>,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub ts: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EventFilter {
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub target: Option<Id>,
    /// Matches `action` exactly or as a dotted prefix (`entity.` matches `entity.create`).
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub until: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One change. Serialized with an `op` tag so JSONL ingestion reads naturally:
/// `{"op":"upsert","kind":"issue","key":"421","title":"Payment timeout"}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Mutation {
    #[serde(alias = "upsert_entity")]
    Upsert(EntityInput),
    #[serde(alias = "patch_entity")]
    Patch(EntityPatch),
    #[serde(alias = "delete_entity")]
    Delete(EntityRef),
    #[serde(alias = "link_create")]
    Link(LinkInput),
    #[serde(alias = "link_delete")]
    Unlink(LinkInput),
    Event(EventInput),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EntityPatch {
    pub id: Id,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub props: Map<String, Value>,
    #[serde(default)]
    pub unset: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EntityRef {
    pub id: Id,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MutationResult {
    pub applied: usize,
    pub entities: Vec<Id>,
    pub links: usize,
    pub events: Vec<String>,
    pub revision: u64,
}

/// Accepts `"x"` or `["x", "y"]`.
pub fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => Vec::new(),
        Some(OneOrMany::One(s)) if s.is_empty() => Vec::new(),
        Some(OneOrMany::One(s)) => vec![s],
        Some(OneOrMany::Many(v)) => v,
    })
}
