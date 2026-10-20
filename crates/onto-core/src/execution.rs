//! nexc-engine's execution vocabulary (`builtin:nexc-execution`) read and
//! written in nexc's own shape: a graph's node types and relation types.
//!
//! The data lives as ordinary entities (`execution_graph`, `node_type`,
//! `relation_type`, linked `defined_in`), so it is versioned, audited,
//! permission-checked, searchable and visible on the workspace map. These
//! helpers only translate.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::engine::ActionContext;
use crate::model::{EntityInput, EntityRef, Mutation, MutationResult};
use crate::policy::Principal;
use crate::{Error, Ontology, Result};

pub const GRAPH_KIND: &str = "execution_graph";
pub const NODE_TYPE_KIND: &str = "node_type";
pub const RELATION_TYPE_KIND: &str = "relation_type";
pub const DEFINED_IN: &str = "defined_in";

/// A type of node (same fields as nexc's `NodeType`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeTypeDef {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub default_role: String,
    #[serde(default = "default_executor")]
    pub default_executor: String,
    #[serde(default)]
    pub stage: i64,
    #[serde(default)]
    pub produces_artifact: bool,
    #[serde(default)]
    pub allow_code_exec: bool,
}

fn default_executor() -> String {
    "llm".into()
}

/// A type of relation (same fields as nexc's `RelationType`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RelationTypeDef {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub blocking: bool,
}

/// A graph's vocabulary (same shape as nexc's `Ontology`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Vocabulary {
    #[serde(default)]
    pub node_types: Vec<NodeTypeDef>,
    #[serde(default)]
    pub relation_types: Vec<RelationTypeDef>,
}

/// Result of replacing a vocabulary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VocabularyChange {
    pub graph: String,
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    pub revision: u64,
    pub vocabulary: Vocabulary,
}

impl Vocabulary {
    /// The vocabulary a new nexc graph starts with.
    pub fn starter() -> Self {
        let node =
            |key: &str, label: &str, description: &str, color: &str, icon: &str, role: &str, stage: i64| NodeTypeDef {
                key: key.into(),
                label: label.into(),
                description: description.into(),
                color: color.into(),
                icon: icon.into(),
                default_role: role.into(),
                default_executor: "llm".into(),
                stage,
                produces_artifact: false,
                allow_code_exec: false,
            };
        let relation = |key: &str, label: &str, description: &str, blocking: bool| RelationTypeDef {
            key: key.into(),
            label: label.into(),
            description: description.into(),
            blocking,
        };
        Vocabulary {
            node_types: vec![
                node("topic", "Topic", "A subject or idea that frames the work.", "#6366f1", "hash", "planner", 0),
                node(
                    "research",
                    "Research",
                    "Finding and summarising information.",
                    "#0ea5e9",
                    "book-open",
                    "researcher",
                    1,
                ),
                node("task", "Task", "A concrete step to carry out.", "#10b981", "list-todo", "engineer", 2),
                NodeTypeDef {
                    allow_code_exec: true,
                    ..node("code", "Code", "Writing or changing source code.", "#f59e0b", "code", "engineer", 2)
                },
                NodeTypeDef {
                    produces_artifact: true,
                    ..node("document", "Document", "A written deliverable.", "#a855f7", "file-text", "writer", 3)
                },
                NodeTypeDef {
                    produces_artifact: true,
                    ..node("output", "Output", "The final deliverable of the graph.", "#ef4444", "flag", "writer", 4)
                },
            ],
            relation_types: vec![
                relation(
                    "depends_on",
                    "Depends on",
                    "The source must finish before the target runs; its output is the target's context.",
                    true,
                ),
                relation("relates_to", "Relates to", "The two nodes are about the same thing; no ordering.", false),
            ],
        }
    }
}

/// `execution_graph:{key}` for a key or an id.
pub fn graph_id(graph: &str) -> String {
    if graph.starts_with("execution_graph:") { graph.to_string() } else { format!("{GRAPH_KIND}:{graph}") }
}

fn graph_key(graph: &str) -> &str {
    graph.strip_prefix("execution_graph:").unwrap_or(graph)
}

fn str_of(m: &Map<String, Value>, k: &str) -> String {
    m.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn node_props(t: &NodeTypeDef) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("key".into(), json!(t.key));
    m.insert("label".into(), json!(t.label));
    m.insert("description".into(), json!(t.description));
    if !t.color.is_empty() {
        m.insert("color".into(), json!(t.color));
    }
    if !t.icon.is_empty() {
        m.insert("icon".into(), json!(t.icon));
    }
    m.insert("default_role".into(), json!(t.default_role));
    m.insert("default_executor".into(), json!(t.default_executor));
    m.insert("stage".into(), json!(t.stage));
    m.insert("produces_artifact".into(), json!(t.produces_artifact));
    m.insert("allow_code_exec".into(), json!(t.allow_code_exec));
    m
}

fn relation_props(t: &RelationTypeDef) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("key".into(), json!(t.key));
    m.insert("label".into(), json!(t.label));
    m.insert("description".into(), json!(t.description));
    m.insert("blocking".into(), json!(t.blocking));
    m
}

impl Ontology {
    fn require_execution_schema(&self) -> Result<()> {
        let s = self.schema();
        for k in [GRAPH_KIND, NODE_TYPE_KIND, RELATION_TYPE_KIND] {
            if !s.object_types.contains_key(k) {
                return Err(Error::Config(format!(
                    "the execution vocabulary needs the nexc-execution package (ontology.extend = [\"builtin:nexc-execution\"]); {k} is not defined"
                )));
            }
        }
        Ok(())
    }

    /// A graph's node types and relation types, in nexc's shape (sorted by
    /// stage then key, relation types by key).
    pub fn execution_vocabulary(&self, p: &Principal, graph: &str) -> Result<Vocabulary> {
        self.require_execution_schema()?;
        let gid = graph_id(graph);
        self.get(p, &gid)?;
        let mut v = Vocabulary::default();
        for (rel, e) in self.neighbors(p, &gid, Some("defines"))? {
            if rel != "defines" {
                continue;
            }
            let m = &e.props;
            match e.kind.as_str() {
                NODE_TYPE_KIND => v.node_types.push(NodeTypeDef {
                    key: str_of(m, "key"),
                    label: str_of(m, "label"),
                    description: str_of(m, "description"),
                    color: str_of(m, "color"),
                    icon: str_of(m, "icon"),
                    default_role: str_of(m, "default_role"),
                    default_executor: m.get("default_executor").and_then(Value::as_str).unwrap_or("llm").to_string(),
                    stage: m.get("stage").and_then(Value::as_i64).unwrap_or(0),
                    produces_artifact: m.get("produces_artifact").and_then(Value::as_bool).unwrap_or(false),
                    allow_code_exec: m.get("allow_code_exec").and_then(Value::as_bool).unwrap_or(false),
                }),
                RELATION_TYPE_KIND => v.relation_types.push(RelationTypeDef {
                    key: str_of(m, "key"),
                    label: str_of(m, "label"),
                    description: str_of(m, "description"),
                    blocking: m.get("blocking").and_then(Value::as_bool).unwrap_or(false),
                }),
                _ => {}
            }
        }
        v.node_types.sort_by(|a, b| (a.stage, &a.key).cmp(&(b.stage, &b.key)));
        v.relation_types.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(v)
    }

    /// Create a graph (optionally with the starter vocabulary).
    pub fn init_execution_graph(
        &self,
        ctx: &ActionContext,
        graph: &str,
        title: &str,
        vocabulary: Option<Vocabulary>,
    ) -> Result<VocabularyChange> {
        self.require_execution_schema()?;
        let gid = graph_id(graph);
        let mut muts = vec![Mutation::Upsert(EntityInput {
            id: Some(gid.clone()),
            kind: GRAPH_KIND.into(),
            title: Some(if title.is_empty() { graph_key(graph).to_string() } else { title.to_string() }),
            ..Default::default()
        })];
        let vocab = vocabulary.unwrap_or_default();
        muts.extend(self.vocabulary_mutations(&gid, &vocab));
        let r = self.apply(ctx, muts)?;
        Ok(VocabularyChange {
            graph: gid.clone(),
            created: vocab
                .node_types
                .iter()
                .map(|t| t.key.clone())
                .chain(vocab.relation_types.iter().map(|t| t.key.clone()))
                .collect(),
            updated: Vec::new(),
            removed: Vec::new(),
            revision: r.revision,
            vocabulary: self.execution_vocabulary(&ctx.principal, &gid)?,
        })
    }

    fn vocabulary_mutations(&self, gid: &str, v: &Vocabulary) -> Vec<Mutation> {
        let key = graph_key(gid);
        let mut muts = Vec::new();
        for t in &v.node_types {
            muts.push(Mutation::Upsert(
                EntityInput {
                    id: Some(format!("{NODE_TYPE_KIND}:{key}.{}", t.key)),
                    kind: NODE_TYPE_KIND.into(),
                    title: Some(t.label.clone()),
                    props: node_props(t),
                    replace: true,
                    ..Default::default()
                }
                .link(DEFINED_IN, gid.to_string()),
            ));
        }
        for t in &v.relation_types {
            muts.push(Mutation::Upsert(
                EntityInput {
                    id: Some(format!("{RELATION_TYPE_KIND}:{key}.{}", t.key)),
                    kind: RELATION_TYPE_KIND.into(),
                    title: Some(t.label.clone()),
                    props: relation_props(t),
                    replace: true,
                    ..Default::default()
                }
                .link(DEFINED_IN, gid.to_string()),
            ));
        }
        muts
    }

    /// Replace a graph's vocabulary in one atomic, audited write: types are
    /// created or updated, types not listed are removed. Keys must be unique.
    pub fn set_execution_vocabulary(
        &self,
        ctx: &ActionContext,
        graph: &str,
        vocabulary: Vocabulary,
    ) -> Result<VocabularyChange> {
        self.require_execution_schema()?;
        let gid = graph_id(graph);
        let current = self.execution_vocabulary(&ctx.principal, &gid)?;
        for (what, keys) in [
            ("node type", vocabulary.node_types.iter().map(|t| &t.key).collect::<Vec<_>>()),
            ("relation type", vocabulary.relation_types.iter().map(|t| &t.key).collect::<Vec<_>>()),
        ] {
            let mut seen = std::collections::HashSet::new();
            if let Some(dup) = keys.iter().find(|k| !seen.insert(**k)) {
                return Err(Error::invalid(format!("{what} key '{dup}' appears twice")));
            }
        }
        let mut created = Vec::new();
        let mut updated = Vec::new();
        let mut removed = Vec::new();
        for t in &vocabulary.node_types {
            match current.node_types.iter().find(|c| c.key == t.key) {
                None => created.push(t.key.clone()),
                Some(c) if c != t => updated.push(t.key.clone()),
                _ => {}
            }
        }
        for t in &vocabulary.relation_types {
            match current.relation_types.iter().find(|c| c.key == t.key) {
                None => created.push(t.key.clone()),
                Some(c) if c != t => updated.push(t.key.clone()),
                _ => {}
            }
        }
        let key = graph_key(&gid).to_string();
        let mut muts = self.vocabulary_mutations(&gid, &vocabulary);
        for c in &current.node_types {
            if !vocabulary.node_types.iter().any(|t| t.key == c.key) {
                removed.push(c.key.clone());
                muts.push(Mutation::Delete(EntityRef { id: format!("{NODE_TYPE_KIND}:{key}.{}", c.key) }));
            }
        }
        for c in &current.relation_types {
            if !vocabulary.relation_types.iter().any(|t| t.key == c.key) {
                removed.push(c.key.clone());
                muts.push(Mutation::Delete(EntityRef { id: format!("{RELATION_TYPE_KIND}:{key}.{}", c.key) }));
            }
        }
        let r: MutationResult = self.apply(ctx, muts)?;
        Ok(VocabularyChange {
            graph: gid.clone(),
            created,
            updated,
            removed,
            revision: r.revision,
            vocabulary: self.execution_vocabulary(&ctx.principal, &gid)?,
        })
    }
}
