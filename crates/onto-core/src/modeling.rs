//! Modeling: a draft ontology from a crawled data source, the way an
//! ontology builder starts one (Palantir Foundry: object types backed by
//! datasets, link types from keys, action types for the edits people make).
//!
//! [`draft`] reads the catalog entities a connector wrote for one source
//! (tables, columns, foreign keys, learned join paths) and proposes:
//!
//! * an **object type** per table, keyed by its primary key, with typed
//!   properties, a title column, a category and states from a status enum;
//! * a **link type** per foreign key, per join table (many-to-many) and per
//!   learned join that no constraint declares;
//! * **action types** to create and edit objects, move them between states
//!   and change their links.
//!
//! Every decision is deterministic and carries its reason. The draft is a
//! proposal: its [`SchemaChangeSet`] goes through `schema.plan` and then
//! `schema.apply` or a proposal, like any other schema change. Nothing here
//! reads row data or writes anything.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::action_types::{
    ActionType, Check, CheckKind, CreateEffect, Effect, LinkEffect, ParamDef, ParamType, PatchEffect, Risk,
};
use crate::authority::Scores;
use crate::graph::Graph;
use crate::live_schema::{SchemaChange, SchemaChangeSet};
use crate::model::Entity;
use crate::policy::{self, Access, Principal};
use crate::quality::Constraints;
use crate::schema::{
    Backing, Cardinality, Category, Dir, LinkBacking, LinkType, ObjectType, PropType, PropertyDef, Schema,
};
use crate::{Error, Ontology, Result};

/// Tables left out unless [`DraftOptions::exclude`] says otherwise.
pub const DEFAULT_EXCLUDE: &[&str] = &["staging.*", "*_tmp", "*_old", "*_bak", "*_backup"];

/// Kinds the data catalog uses; a table never takes one of these names.
pub const CATALOG_KINDS: &[&str] = &["data_source", "db_schema", "table", "view", "column", "query", "learned_fact"];

/// Object properties with a meaning of their own (access control).
const RESERVED_PROPS: &[&str] = &["visibility", "acl"];

/// Template names action types provide themselves.
const RESERVED_PARAMS: &[&str] = &["actor", "now", "created", "new_id", "item", "target"];

/// Learned joins weaker than this do not become link types.
pub const MIN_LEARNED_CONFIDENCE: f64 = 0.6;

/// Edit actions take at most this many properties.
const MAX_EDIT_PROPS: usize = 8;

fn yes() -> bool {
    true
}

/// What to draft.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DraftOptions {
    /// The connector (catalog source key) whose tables are modeled.
    pub source: String,
    /// `schema.table` names or globs (`sales.*`, `*orders`); empty means all.
    #[serde(default)]
    pub tables: Vec<String>,
    /// Globs of tables to leave out; [`DEFAULT_EXCLUDE`] when unset, nothing
    /// when an empty list.
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
    /// Put before every object type name (`crm` → `crm_customer`).
    #[serde(default)]
    pub prefix: Option<String>,
    /// Materialize rows as objects once the draft is applied.
    #[serde(default = "yes")]
    pub sync: bool,
    /// Generate action types.
    #[serde(default = "yes")]
    pub actions: bool,
    /// Model views too (they have no constraints: an `id` column is their key).
    #[serde(default)]
    pub views: bool,
}

impl Default for DraftOptions {
    fn default() -> Self {
        Self {
            source: String::new(),
            tables: Vec::new(),
            exclude: None,
            prefix: None,
            sync: true,
            actions: true,
            views: false,
        }
    }
}

impl DraftOptions {
    pub fn for_source(source: impl Into<String>) -> Self {
        Self { source: source.into(), ..Default::default() }
    }

    fn excluded_by(&self, full: &str, name: &str) -> Option<String> {
        let defaults: Vec<String> = DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect();
        let list = self.exclude.as_ref().unwrap_or(&defaults);
        list.iter().find(|p| table_glob(p, full, name)).cloned()
    }

    fn selected(&self, full: &str, name: &str) -> bool {
        self.tables.is_empty() || self.tables.iter().any(|p| table_glob(p, full, name))
    }
}

/// A proposed ontology for one data source. Read as input, every field is
/// optional: a reviewed draft may be given as its objects, links and actions
/// alone (its change set is rebuilt from them, and an empty source is
/// resolved by the caller).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OntologyDraft {
    #[serde(default)]
    pub source: String,
    /// The schema changes that create it: object types, then link types,
    /// then action types. Rebuilt from the parts by [`rebuild`](Self::rebuild).
    #[serde(default)]
    pub changes: SchemaChangeSet,
    #[serde(default)]
    pub objects: Vec<DraftObject>,
    #[serde(default)]
    pub links: Vec<DraftLink>,
    #[serde(default)]
    pub actions: Vec<DraftAction>,
    #[serde(default)]
    pub skipped: Vec<Skipped>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// A table that did not become an object type, and why.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Skipped {
    pub table: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DraftObject {
    /// Object type name.
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// `schema.table`.
    pub table: String,
    /// The catalog entity (`table:{source}.{schema}.{table}`).
    pub table_id: String,
    pub category: Category,
    /// Key columns: object ids are `{name}:{v1}.{v2}`.
    pub key: Vec<String>,
    /// A single text-like key column people type (create actions ask for it).
    #[serde(default)]
    pub natural_key: bool,
    /// Title column.
    pub title: Option<String>,
    /// Column mapped to the object state.
    pub state: Option<String>,
    pub states: Vec<String>,
    #[serde(default)]
    pub terminal_states: Vec<String>,
    #[serde(default)]
    pub alert_states: Vec<String>,
    pub properties: Vec<DraftProperty>,
    /// Materialize rows as objects.
    #[serde(default)]
    pub sync: bool,
    pub confidence: f64,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DraftProperty {
    pub name: String,
    pub column: String,
    /// The column's database type.
    #[serde(default)]
    pub data_type: String,
    pub ty: PropType,
    pub required: bool,
    pub values: Vec<String>,
    pub searchable: bool,
    pub unique: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    pub description: String,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkVia {
    #[default]
    ForeignKey,
    JoinTable,
    Learned,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DraftLink {
    pub name: String,
    pub inverse: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// Source object type.
    pub from: String,
    /// Target object type.
    pub to: String,
    pub cardinality: Cardinality,
    pub via: LinkVia,
    /// The column holding the target's key: the foreign key column of the
    /// source table, or the join table's column for the source.
    pub column: Option<String>,
    /// The join table's column for the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_column: Option<String>,
    /// The join table (`schema.table`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    pub confidence: f64,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    #[default]
    Create,
    Edit,
    Transition,
    Relink,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DraftAction {
    pub name: String,
    /// The object type it acts on.
    pub object: String,
    pub kind: ActionKind,
    pub risk: Risk,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub phrases: Vec<String>,
    /// Transition: the state it moves to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Relink: the link type it sets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    pub reason: String,
}

// ── names ────────────────────────────────────────────────────────────────

/// `pattern` against `schema.table`; a pattern without a dot also matches
/// the bare table name.
fn table_glob(pattern: &str, full: &str, name: &str) -> bool {
    glob(pattern, full) || (!pattern.contains('.') && glob(pattern, name))
}

/// `*` matches any run of characters; case-insensitive.
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) =
        (pattern.to_lowercase().chars().collect(), text.to_lowercase().chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// A valid schema name (`[a-z][a-z0-9_]*`, at most 64 characters).
pub fn snake(s: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_ascii_uppercase() && prev_lower {
            out.push('_');
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
    }
    let mut out = out.trim_matches('_').to_string();
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    if out.is_empty() {
        out = "item".into();
    }
    if !out.starts_with(|c: char| c.is_ascii_lowercase()) {
        out = format!("t_{out}");
    }
    out.truncate(64);
    out.trim_end_matches('_').to_string()
}

const IRREGULAR: &[(&str, &str)] = &[
    ("people", "person"),
    ("persons", "person"),
    ("men", "man"),
    ("women", "woman"),
    ("children", "child"),
    ("mice", "mouse"),
    ("geese", "goose"),
    ("feet", "foot"),
    ("teeth", "tooth"),
    ("criteria", "criterion"),
    ("indices", "index"),
    ("matrices", "matrix"),
    ("vertices", "vertex"),
    ("analyses", "analysis"),
    ("movies", "movie"),
];

const INVARIANT: &[&str] = &[
    "data",
    "metadata",
    "media",
    "staff",
    "news",
    "series",
    "species",
    "information",
    "equipment",
    "feedback",
    "sheep",
    "fish",
    "inventory",
];

/// Singulars ending in `-che`: their plural `-ches` loses only the `s`.
const CHE_WORDS: &[&str] = &[
    "cache",
    "niche",
    "avalanche",
    "cliche",
    "moustache",
    "mustache",
    "panache",
    "psyche",
    "creche",
    "quiche",
    "tranche",
];

/// Singulars ending in `-use`: their plural `-uses` loses only the `s`
/// (`-ause` words such as cause and clause are recognised by their ending).
const USE_WORDS: &[&str] =
    &["use", "misuse", "excuse", "fuse", "ruse", "muse", "abuse", "refuse", "recluse", "accuse", "peruse"];

/// Singulars ending in `-as`: their plural `-ases` loses `es`.
const AS_WORDS: &[&str] = &["alias", "canvas", "atlas", "bias", "gas", "pancreas"];

fn singular_word(w: &str) -> String {
    if INVARIANT.contains(&w) {
        return w.to_string();
    }
    if let Some((_, s)) = IRREGULAR.iter().find(|(p, _)| *p == w) {
        return s.to_string();
    }
    if w.len() > 3 && w.ends_with("ies") {
        return format!("{}y", &w[..w.len() - 3]);
    }
    if w.ends_with("ouses") {
        return w[..w.len() - 1].to_string();
    }
    if w.len() > 4 && w.ends_with("es") {
        // `drop_s` keeps the `e` of the singular, `drop_es` drops it.
        let (drop_s, drop_es) = (&w[..w.len() - 1], &w[..w.len() - 2]);
        if w.ends_with("ches") {
            return if CHE_WORDS.contains(&drop_s) { drop_s } else { drop_es }.to_string();
        }
        if w.ends_with("uses") {
            let keeps_e = drop_es.ends_with("aus") || USE_WORDS.contains(&drop_s);
            return if keeps_e { drop_s } else { drop_es }.to_string();
        }
        if w.ends_with("ases") {
            return if AS_WORDS.contains(&drop_es) { drop_es } else { drop_s }.to_string();
        }
    }
    for suffix in ["sses", "shes", "xes", "zzes"] {
        if w.len() > suffix.len() && w.ends_with(suffix) {
            return w[..w.len() - 2].to_string();
        }
    }
    if w.ends_with("ss") || w.ends_with("us") || w.ends_with("is") {
        return w.to_string();
    }
    match w.strip_suffix('s') {
        Some(s) if s.len() > 1 => s.to_string(),
        _ => w.to_string(),
    }
}

fn plural_word(w: &str) -> String {
    if INVARIANT.contains(&w) {
        return w.to_string();
    }
    if let Some((p, _)) = IRREGULAR.iter().find(|(_, s)| *s == w) {
        return p.to_string();
    }
    let consonant_y =
        w.len() > 1 && w.ends_with('y') && !matches!(w.as_bytes()[w.len() - 2], b'a' | b'e' | b'i' | b'o' | b'u');
    if consonant_y {
        return format!("{}ies", &w[..w.len() - 1]);
    }
    if ["s", "x", "z", "ch", "sh"].iter().any(|s| w.ends_with(s)) {
        return format!("{w}es");
    }
    format!("{w}s")
}

/// The singular of a snake_case name: only the last word changes
/// (`order_items` → `order_item`, `categories` → `category`).
pub fn singular(name: &str) -> String {
    match name.rsplit_once('_') {
        Some((head, last)) => format!("{head}_{}", singular_word(last)),
        None => singular_word(name),
    }
}

/// The plural of a snake_case name (`order_item` → `order_items`).
pub fn plural(name: &str) -> String {
    match name.rsplit_once('_') {
        Some((head, last)) => format!("{head}_{}", plural_word(last)),
        None => plural_word(name),
    }
}

/// `order_item` → `Order Item`.
pub fn title_case(name: &str) -> String {
    name.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `customer_id` → `customer`.
fn fk_stem(column: &str) -> String {
    let c = snake(column);
    for suffix in ["_id", "_fk", "_key"] {
        if let Some(s) = c.strip_suffix(suffix)
            && !s.is_empty()
        {
            return s.to_string();
        }
    }
    c
}

/// `base`, else `base_2`, `base_3`, … whichever is free.
fn free_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2..).map(|i| format!("{}_{i}", truncate(base, 60))).find(|n| !taken(n)).unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    let mut s = s.to_string();
    s.truncate(max);
    s.trim_end_matches('_').to_string()
}

// ── types ────────────────────────────────────────────────────────────────

const PROSE: &[&str] =
    &["description", "notes", "note", "body", "comment", "comments", "content", "summary", "details", "bio"];

const TERMINAL: &[&str] = &[
    "done",
    "closed",
    "cancel",
    "complete",
    "shipped",
    "delivered",
    "resolved",
    "archived",
    "paid",
    "refunded",
    "inactive",
    "deleted",
    "rejected",
    "expired",
];

const ALERT: &[&str] = &["fail", "block", "overdue", "error", "declined", "past_due", "suspended", "on_hold"];

/// Words that negate the state word after them (`not_paid`, `non_billable`).
const NEGATIONS: &[&str] = &["not", "non", "no", "un", "in"];

/// Does state value `v` say one of `words`? Words match whole words of the
/// value (split at `_`, `-` and spaces), the last one also as a word start
/// (`cancel` matches `cancelled`), so `unpaid`, `incomplete` and `unblocked`
/// match nothing; a word after a negation (`not_paid`) does not count.
fn state_is(v: &str, words: &[&str]) -> bool {
    let lower = v.to_lowercase();
    let tokens: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty()).collect();
    words.iter().any(|w| {
        let parts: Vec<&str> = w.split('_').collect();
        let Some((last, head)) = parts.split_last() else { return false };
        (0..tokens.len()).any(|i| {
            let Some(window) = tokens.get(i..i + parts.len()) else { return false };
            window[..head.len()] == *head
                && window[head.len()].starts_with(last)
                && (i == 0 || !NEGATIONS.contains(&tokens[i - 1]))
        })
    })
}

/// State value → the verb of the action that moves an object there.
const VERBS: &[(&str, &str)] = &[
    ("cancelled", "cancel"),
    ("canceled", "cancel"),
    ("shipped", "ship"),
    ("delivered", "deliver"),
    ("paid", "pay"),
    ("refunded", "refund"),
    ("closed", "close"),
    ("archived", "archive"),
    ("approved", "approve"),
    ("rejected", "reject"),
    ("completed", "complete"),
    ("resolved", "resolve"),
    ("suspended", "suspend"),
];

const PEOPLE: &[&str] = &["user", "customer", "employee", "person", "member", "contact", "staff", "author"];
const CONTAINERS: &[&str] =
    &["organization", "company", "team", "project", "department", "store", "warehouse", "account"];

fn base_type(data_type: &str) -> String {
    let t = data_type.trim().to_lowercase();
    let t = t.split('(').next().unwrap_or_default().trim().to_string();
    t.rsplit('.').next().unwrap_or_default().trim_matches('"').to_string()
}

fn is_textual(data_type: &str) -> bool {
    let t = base_type(data_type);
    matches!(
        t.as_str(),
        "text" | "varchar" | "character varying" | "char" | "character" | "bpchar" | "uuid" | "citext" | "name"
    )
}

fn is_timestamp(data_type: &str) -> bool {
    let t = base_type(data_type);
    t == "date" || t.starts_with("timestamp") || t.starts_with("time")
}

/// The property type for a column, and why.
fn prop_type(column: &str, data_type: &str, enum_values: &[String]) -> (PropType, String) {
    let raw = data_type.trim().to_lowercase();
    if raw.ends_with("[]") {
        return (PropType::List, format!("{data_type} is an array → list"));
    }
    if !enum_values.is_empty() {
        return (PropType::Enum, format!("{data_type} allows {} values → enum", enum_values.len()));
    }
    let t = base_type(data_type);
    let ty = match t.as_str() {
        "smallint" | "integer" | "int" | "int2" | "int4" | "int8" | "bigint" | "serial" | "bigserial"
        | "smallserial" | "serial4" | "serial8" => PropType::Int,
        "numeric" | "decimal" | "real" | "double precision" | "float4" | "float8" | "money" | "float" => {
            PropType::Float
        }
        "boolean" | "bool" => PropType::Bool,
        "json" | "jsonb" => PropType::Json,
        _ if is_timestamp(data_type) => PropType::Timestamp,
        _ if is_textual(data_type) => {
            let c = snake(column);
            if PROSE.iter().any(|w| c == *w || c.ends_with(&format!("_{w}"))) {
                return (PropType::Text, format!("{data_type} with a prose name → text"));
            }
            PropType::String
        }
        _ => return (PropType::String, format!("{data_type} has no closer type → string")),
    };
    (ty, format!("{data_type} → {}", format!("{ty:?}").to_lowercase()))
}

fn param_type(ty: PropType) -> Option<ParamType> {
    Some(match ty {
        PropType::String | PropType::Timestamp => ParamType::String,
        PropType::Text => ParamType::Text,
        PropType::Int => ParamType::Int,
        PropType::Float => ParamType::Float,
        PropType::Bool => ParamType::Bool,
        PropType::Enum => ParamType::Enum,
        PropType::List | PropType::Json => return None,
    })
}

// ── reading the catalog ──────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
struct Col {
    name: String,
    data_type: String,
    nullable: bool,
    position: i64,
    primary_key: bool,
    unique: bool,
    enum_values: Vec<String>,
    description: String,
}

/// A single-column foreign key of a table.
#[derive(Clone, Debug)]
struct Fk {
    column: String,
    constraint: String,
    /// Target table entity id.
    to_table: String,
    to_column: String,
}

#[derive(Clone, Debug, Default)]
struct Tbl {
    id: String,
    schema: String,
    name: String,
    full: String,
    is_view: bool,
    description: String,
    columns: Vec<Col>,
    fks: Vec<Fk>,
    /// Foreign keys over several columns (not drafted as links).
    composite_fks: Vec<String>,
}

impl Tbl {
    fn col(&self, name: &str) -> Option<&Col> {
        self.columns.iter().find(|c| c.name == name)
    }

    fn pk(&self) -> Vec<String> {
        self.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect()
    }
}

fn read_table(g: &Graph, e: &Entity, visible: &dyn Fn(&Entity) -> bool) -> Tbl {
    let name = e.prop_str("name").unwrap_or_else(|| e.key().rsplit('.').next().unwrap_or_default()).to_string();
    let schema = e.prop_str("schema_name").unwrap_or_default().to_string();
    let mut t = Tbl {
        id: e.id.clone(),
        full: if schema.is_empty() { name.clone() } else { format!("{schema}.{name}") },
        schema,
        name,
        is_view: e.kind == "view",
        description: e.prop_str("description").unwrap_or_default().trim().to_string(),
        ..Default::default()
    };
    let mut composite: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();
    for cid in g.neighbors(&e.id, "contains", Dir::Out) {
        let Some(c) = g.get(cid).filter(|c| c.kind == "column" && visible(c)) else { continue };
        let col = Col {
            name: c.prop_str("name").unwrap_or_else(|| c.key().rsplit('.').next().unwrap_or_default()).to_string(),
            data_type: c.prop_str("data_type").unwrap_or_default().to_string(),
            nullable: c.props.get("nullable").and_then(Value::as_bool).unwrap_or(true),
            position: c.props.get("position").and_then(Value::as_i64).unwrap_or(i64::MAX),
            primary_key: c.props.get("primary_key").and_then(Value::as_bool).unwrap_or(false),
            unique: c.props.get("unique").and_then(Value::as_bool).unwrap_or(false),
            enum_values: c
                .props
                .get("enum_values")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
            description: c.prop_str("description").unwrap_or_default().trim().to_string(),
        };
        for (rel, to) in g.out_edges(cid) {
            if rel != "references" {
                continue;
            }
            let Some(target) = g.get(to).filter(|x| x.kind == "column") else { continue };
            let Some(to_table) = g.neighbors(to, "contains", Dir::In).into_iter().find(|t| t.starts_with("table:"))
            else {
                continue;
            };
            let constraint = g
                .get_link(cid, rel, to)
                .and_then(|l| l.props.get("constraint"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let to_column = target.prop_str("name").unwrap_or_default().to_string();
            composite.entry((constraint, to_table.to_string())).or_default().push((col.name.clone(), to_column));
        }
        t.columns.push(col);
    }
    t.columns.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.name.cmp(&b.name)));
    for ((constraint, to_table), cols) in composite {
        if cols.len() == 1 {
            let (column, to_column) = cols.into_iter().next().unwrap_or_default();
            t.fks.push(Fk { column, constraint, to_table, to_column });
        } else {
            t.composite_fks.push(format!(
                "{constraint} ({})",
                cols.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    let positions: HashMap<String, i64> = t.columns.iter().map(|c| (c.name.clone(), c.position)).collect();
    let position = |c: &str| positions.get(c).copied().unwrap_or(i64::MAX);
    t.fks.sort_by(|a, b| position(&a.column).cmp(&position(&b.column)).then_with(|| a.constraint.cmp(&b.constraint)));
    t
}

/// A learned join path between two tables: `a.x = b.y`.
#[derive(Clone, Debug)]
struct LearnedJoin {
    id: String,
    a: String,
    a_col: String,
    b: String,
    b_col: String,
    confidence: f64,
    state: String,
}

fn learned_joins(g: &Graph, tables: &HashMap<String, Tbl>, visible: &dyn Fn(&Entity) -> bool) -> Vec<LearnedJoin> {
    let mut out = Vec::new();
    for f in g.of_kind("learned_fact") {
        if f.prop_str("fact_type") != Some("join_path") || f.state.as_deref() == Some("rejected") || !visible(f) {
            continue;
        }
        let ends = g.neighbors(&f.id, "concerns", Dir::Out);
        let [x, y] = ends.as_slice() else { continue };
        let (Some(tx), Some(ty)) = (tables.get(*x), tables.get(*y)) else { continue };
        let expr = f.prop_str("expression").unwrap_or_default();
        if expr.to_lowercase().contains(" and ") {
            continue;
        }
        let Some((l, r)) = expr.split_once('=') else { continue };
        let side = |s: &str| s.trim().rsplit_once('.').map(|(t, c)| (t.trim().to_string(), c.trim().to_string()));
        let (Some((lt, lc)), Some((rt, rc))) = (side(l), side(r)) else { continue };
        let (a_col, b_col) = if lt == tx.name && rt == ty.name {
            (lc, rc)
        } else if lt == ty.name && rt == tx.name {
            (rc, lc)
        } else {
            continue;
        };
        out.push(LearnedJoin {
            id: f.id.clone(),
            a: tx.id.clone(),
            a_col,
            b: ty.id.clone(),
            b_col,
            confidence: f.props.get("confidence").and_then(Value::as_f64).unwrap_or(0.0),
            state: f.state.clone().unwrap_or_else(|| "suggested".into()),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

// ── the draft ────────────────────────────────────────────────────────────

/// Draft an ontology for the tables of `opts.source` in the catalog.
/// `scores` (OntoRank) orders the object types and nudges their confidence;
/// pass empty scores when none are computed.
pub fn draft(g: &Graph, schema: &Schema, scores: &Scores, opts: &DraftOptions) -> OntologyDraft {
    draft_visible(g, schema, scores, opts, &|_| true)
}

/// Is `ty` an object type an earlier draft of this source created for `table`?
fn backed_by(ty: &ObjectType, source: &str, table: &str) -> bool {
    ty.backing.as_ref().is_some_and(|b| b.source == source && b.table == table)
}

/// The name and inverse of the link type an earlier draft made for `column`
/// of `table` (a foreign key or learned join from object type `from` to `to`).
fn earlier_column_link(
    schema: &Schema,
    source: &str,
    from: &str,
    table: &str,
    column: &str,
    to: &str,
) -> Option<(String, String)> {
    let ty = schema.object_types.get(from).filter(|ty| backed_by(ty, source, table))?;
    ty.backing.as_ref()?.links.iter().find_map(|(rel, c)| {
        let lt = schema.link_types.get(rel)?;
        (c == column && lt.backing.is_none() && lt.to.len() == 1 && lt.to[0] == to)
            .then(|| (rel.clone(), lt.inverse.clone().unwrap_or_default()))
    })
}

/// The name and inverse of the link type an earlier draft made for the
/// join table `table` (`from_column` → `to_column`).
fn earlier_join_link(
    schema: &Schema,
    source: &str,
    table: &str,
    from_column: &str,
    to_column: &str,
) -> Option<(String, String)> {
    schema
        .link_types
        .values()
        .find(|lt| {
            lt.backing
                .as_ref()
                .is_some_and(|b| b.source == source && b.table == table && b.from == from_column && b.to == to_column)
        })
        .map(|lt| (lt.name.clone(), lt.inverse.clone().unwrap_or_default()))
}

fn draft_visible(
    g: &Graph,
    schema: &Schema,
    scores: &Scores,
    opts: &DraftOptions,
    visible: &dyn Fn(&Entity) -> bool,
) -> OntologyDraft {
    let source = opts.source.trim().to_string();
    let mut d = OntologyDraft { source: source.clone(), ..Default::default() };
    if !g.get(&crate::catalog::source_id(&source)).is_some_and(visible) {
        d.notes.push(format!("no crawled data source named {source}: connect it and crawl it first"));
        d.rebuild();
        return d;
    }

    // Tables of the source, filtered.
    let prefix_t = format!("table:{source}.");
    let prefix_v = format!("view:{source}.");
    let mut tables: HashMap<String, Tbl> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut candidates: Vec<&Entity> = g
        .of_kind("table")
        .chain(g.of_kind("view"))
        .filter(|e| e.id.starts_with(&prefix_t) || e.id.starts_with(&prefix_v))
        .filter(|e| visible(e))
        .collect();
    candidates.sort_by(|a, b| a.id.cmp(&b.id));
    for e in candidates {
        let t = read_table(g, e, visible);
        if !opts.selected(&t.full, &t.name) {
            continue;
        }
        if let Some(p) = opts.excluded_by(&t.full, &t.name) {
            d.skipped.push(Skipped { table: t.full.clone(), reason: format!("excluded by {p}") });
            continue;
        }
        if t.is_view && !opts.views {
            d.skipped.push(Skipped { table: t.full.clone(), reason: "a view (draft with views to model it)".into() });
            continue;
        }
        if t.columns.is_empty() {
            d.skipped.push(Skipped { table: t.full.clone(), reason: "no columns visible".into() });
            continue;
        }
        order.push(t.id.clone());
        tables.insert(t.id.clone(), t);
    }
    // Most authoritative first.
    let score = |id: &str| scores.get(id).copied().unwrap_or(0.0);
    order.sort_by(|a, b| score(b).total_cmp(&score(a)).then_with(|| a.cmp(b)));

    // Join tables become many-to-many link types, the rest object types.
    let mut joins: Vec<String> = Vec::new();
    let mut keyed: Vec<(String, Vec<String>, String, f64)> = Vec::new();
    for id in &order {
        let t = &tables[id];
        if is_join_table(t) {
            joins.push(id.clone());
            continue;
        }
        let pk = t.pk();
        if !pk.is_empty() {
            let why = format!("key {} (primary key)", pk.join(", "));
            keyed.push((id.clone(), pk, why, 0.9));
            continue;
        }
        let unique = t.columns.iter().filter(|c| c.unique).min_by_key(|c| (c.nullable, c.position));
        if let Some(c) = unique {
            let why = format!("key {} (no primary key; a unique column)", c.name);
            keyed.push((id.clone(), vec![c.name.clone()], why, 0.75));
            continue;
        }
        if t.is_view {
            let guess = [format!("{}_id", singular(&snake(&t.name))), "id".to_string()];
            if let Some(c) = guess.iter().find_map(|n| t.col(n)) {
                let why = format!("key {} (a view has no constraints; its id column)", c.name);
                keyed.push((id.clone(), vec![c.name.clone()], why, 0.6));
                continue;
            }
        }
        d.skipped.push(Skipped { table: t.full.clone(), reason: "no primary key".into() });
    }

    // Object type names. A table that already backs an object type of this
    // source keeps that type's name (renamed, prefixed or refined earlier),
    // so a re-draft updates the type instead of adding a second one.
    let mut names: HashMap<String, String> = HashMap::new(); // table id → type name
    let mut used: BTreeSet<String> = BTreeSet::new();
    for (id, _, _, _) in &keyed {
        let t = &tables[id];
        if let Some(ty) = schema.object_types.values().find(|ty| backed_by(ty, &source, &t.full)) {
            used.insert(ty.name.clone());
            names.insert(id.clone(), ty.name.clone());
        }
    }
    for (id, _, _, _) in &keyed {
        if names.contains_key(id) {
            continue;
        }
        let t = &tables[id];
        let mut base = singular(&snake(&t.name));
        if let Some(p) = opts.prefix.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            let p = snake(p);
            base = truncate(&format!("{p}_{base}"), 64);
        }
        let taken_elsewhere = |n: &str| {
            CATALOG_KINDS.contains(&n)
                || schema.interfaces.contains_key(n)
                || schema.object_types.get(n).is_some_and(|ty| !backed_by(ty, &source, &t.full))
        };
        let mut name = base.clone();
        if taken_elsewhere(&name) {
            name = truncate(&format!("{}_{base}", snake(&source)), 64);
            d.notes.push(format!("{} would be {base}, which exists already: named {name}", t.full));
        }
        if used.contains(&name) {
            let other = truncate(&format!("{}_{name}", snake(&t.schema)), 64);
            d.notes.push(format!("{} would be {name}, which another table takes: named {other}", t.full));
            name = other;
        }
        let name = free_name(&name, |n| used.contains(n) || taken_elsewhere(n));
        used.insert(name.clone());
        names.insert(id.clone(), name);
    }

    // Our object types from an earlier draft.
    let ours: BTreeSet<&str> =
        names.values().map(String::as_str).filter(|n| schema.object_types.contains_key(*n)).collect();
    // Relation names already in the schema. A link an earlier draft of this
    // source made for the same column or join table keeps its name and
    // inverse ([`earlier_link`]); every other link avoids them all.
    let mut rels_taken: BTreeSet<String> = BTreeSet::new();
    for lt in schema.link_types.values() {
        rels_taken.insert(lt.name.clone());
        rels_taken.extend(lt.inverse.clone());
    }

    // Links: foreign keys first (their columns are not properties).
    let mut fk_links: Vec<(String, Fk, String)> = Vec::new(); // (from table id, fk, target type)
    let mut link_columns: BTreeSet<(String, String)> = BTreeSet::new();
    for (id, _, _, _) in &keyed {
        let t = &tables[id];
        for fk in &t.fks {
            let Some(target) = names.get(&fk.to_table) else {
                // Name the target only when it is one of the tables read here
                // (a table hidden from the principal stays unnamed).
                let what = tables.get(&fk.to_table).map(|x| x.full.as_str()).unwrap_or("a table");
                d.notes.push(format!(
                    "{}.{} references {what}, which is not drafted: kept as a property",
                    t.full, fk.column
                ));
                continue;
            };
            let target_key = keyed.iter().find(|(tid, _, _, _)| tid == &fk.to_table).map(|(_, k, _, _)| k);
            if target_key.is_none_or(|k| k.len() != 1 || k[0] != fk.to_column) {
                d.notes.push(format!(
                    "{}.{} references {}.{}, not its key: kept as a property",
                    t.full, fk.column, tables[&fk.to_table].full, fk.to_column
                ));
                continue;
            }
            link_columns.insert((id.clone(), fk.column.clone()));
            fk_links.push((id.clone(), fk.clone(), target.clone()));
        }
        for c in &t.composite_fks {
            d.notes.push(format!("{}: composite foreign key {c} is not drafted as a link", t.full));
        }
    }
    let stems: Vec<String> = fk_links.iter().map(|(_, fk, _)| fk_stem(&fk.column)).collect();
    let inverse_bases: Vec<String> = fk_links.iter().map(|(id, _, _)| plural(&names[id])).collect();
    for (i, (id, fk, target)) in fk_links.iter().enumerate() {
        let t = &tables[id];
        let from = &names[id];
        let stem = &stems[i];
        let (name, inverse) = match earlier_column_link(schema, &source, from, &t.full, &fk.column, target) {
            Some(kept) => kept,
            None => {
                let twice = stems.iter().filter(|s| *s == stem).count() > 1;
                let mut name = stem.clone();
                if twice || rels_taken.contains(&name) {
                    name = truncate(&format!("{from}_{stem}"), 64);
                }
                let name = free_name(&name, |n| rels_taken.contains(n));
                rels_taken.insert(name.clone());
                let base_inv = inverse_bases[i].clone();
                let mut inverse = base_inv.clone();
                if inverse_bases.iter().filter(|b| **b == base_inv).count() > 1 || rels_taken.contains(&inverse) {
                    inverse = truncate(&format!("{stem}_{base_inv}"), 64);
                }
                let inverse = free_name(&inverse, |n| rels_taken.contains(n));
                rels_taken.insert(inverse.clone());
                (name, inverse)
            }
        };
        let to_label = title_case(target);
        d.links.push(DraftLink {
            label: title_case(&name),
            description: format!(
                "The {} a {} refers to ({}.{} → {}.{}).",
                to_label.to_lowercase(),
                title_case(from).to_lowercase(),
                t.full,
                fk.column,
                tables[&fk.to_table].full,
                fk.to_column
            ),
            name,
            inverse,
            from: from.clone(),
            to: target.clone(),
            cardinality: Cardinality::One,
            via: LinkVia::ForeignKey,
            column: Some(fk.column.clone()),
            target_column: None,
            table: None,
            confidence: 0.95,
            reason: format!(
                "foreign key {} on {}.{} → {}.{}",
                if fk.constraint.is_empty() { "(unnamed)" } else { &fk.constraint },
                t.full,
                fk.column,
                tables[&fk.to_table].full,
                fk.to_column
            ),
        });
    }

    // Join tables.
    for id in &joins {
        let t = &tables[id];
        let (a, b) = (&t.fks[0], &t.fks[1]);
        let (Some(from), Some(to)) = (names.get(&a.to_table), names.get(&b.to_table)) else {
            d.skipped.push(Skipped {
                table: t.full.clone(),
                reason: "a join table between tables that are not drafted".into(),
            });
            continue;
        };
        // Each foreign key must reference the single key of its object type,
        // or the rows name objects that do not exist.
        let off_key = [a, b].into_iter().find(|fk| {
            keyed
                .iter()
                .find(|(tid, _, _, _)| tid == &fk.to_table)
                .is_none_or(|(_, k, _, _)| k.len() != 1 || k[0] != fk.to_column)
        });
        if let Some(fk) = off_key {
            d.skipped.push(Skipped {
                table: t.full.clone(),
                reason: format!(
                    "a join table whose {} references {}.{}, not its key",
                    fk.column, tables[&fk.to_table].full, fk.to_column
                ),
            });
            continue;
        }
        let (stem_a, stem_b) = (fk_stem(&a.column), fk_stem(&b.column));
        let (name, inverse) = match earlier_join_link(schema, &source, &t.full, &a.column, &b.column) {
            Some(kept) => kept,
            None => {
                let mut name = plural(&stem_b);
                if rels_taken.contains(&name) {
                    name = truncate(&format!("{stem_a}_{name}"), 64);
                }
                let name = free_name(&name, |n| rels_taken.contains(n));
                rels_taken.insert(name.clone());
                let mut inverse = plural(&stem_a);
                if rels_taken.contains(&inverse) {
                    inverse = truncate(&format!("{stem_b}_{inverse}"), 64);
                }
                let inverse = free_name(&inverse, |n| rels_taken.contains(n));
                rels_taken.insert(inverse.clone());
                (name, inverse)
            }
        };
        d.links.push(DraftLink {
            label: title_case(&name),
            description: if t.description.is_empty() {
                format!("{} and {} related through {}.", title_case(from), title_case(to), t.full)
            } else {
                t.description.clone()
            },
            name,
            inverse,
            from: from.clone(),
            to: to.clone(),
            cardinality: Cardinality::Many,
            via: LinkVia::JoinTable,
            column: Some(a.column.clone()),
            target_column: Some(b.column.clone()),
            table: Some(t.full.clone()),
            confidence: 0.9,
            reason: format!(
                "join table {}: two foreign keys ({} → {}, {} → {}) and nothing else but keys and timestamps",
                t.full, a.column, from, b.column, to
            ),
        });
    }

    // Learned joins no constraint declares.
    for j in learned_joins(g, &tables, visible) {
        if j.confidence < MIN_LEARNED_CONFIDENCE {
            continue;
        }
        let is_key = |tid: &str, c: &str| keyed.iter().any(|(x, k, _, _)| x == tid && k.len() == 1 && k[0] == c);
        // The side that joins on the other side's single key holds the
        // reference; joins key to key or non-key to non-key have no direction.
        let (from_t, col, to_t, to_col) = match (is_key(&j.a, &j.a_col), is_key(&j.b, &j.b_col)) {
            (false, true) => (j.a.clone(), j.a_col.clone(), j.b.clone(), j.b_col.clone()),
            (true, false) => (j.b.clone(), j.b_col.clone(), j.a.clone(), j.a_col.clone()),
            _ => continue,
        };
        let (Some(from), Some(to)) = (names.get(&from_t), names.get(&to_t)) else { continue };
        let covered = tables[&from_t].fks.iter().any(|f| f.column == col)
            || tables[&from_t].fks.iter().any(|f| f.to_table == to_t)
            || tables[&to_t].fks.iter().any(|f| f.to_table == from_t);
        if covered || tables[&from_t].col(&col).is_none() {
            continue;
        }
        let (name, inverse) = match earlier_column_link(schema, &source, from, &tables[&from_t].full, &col, to) {
            Some(kept) => kept,
            None => {
                let stem = fk_stem(&col);
                let mut name = stem.clone();
                if rels_taken.contains(&name) {
                    name = truncate(&format!("{from}_{stem}"), 64);
                }
                let name = free_name(&name, |n| rels_taken.contains(n));
                rels_taken.insert(name.clone());
                let mut inverse = plural(from);
                if rels_taken.contains(&inverse) {
                    inverse = truncate(&format!("{stem}_{inverse}"), 64);
                }
                let inverse = free_name(&inverse, |n| rels_taken.contains(n));
                rels_taken.insert(inverse.clone());
                (name, inverse)
            }
        };
        link_columns.insert((from_t.clone(), col.clone()));
        d.links.push(DraftLink {
            label: title_case(&name),
            description: format!(
                "The {} a {} refers to ({}.{col} = {}.{to_col}, learned from queries).",
                title_case(to).to_lowercase(),
                title_case(from).to_lowercase(),
                tables[&from_t].full,
                tables[&to_t].full
            ),
            name,
            inverse,
            from: from.clone(),
            to: to.clone(),
            cardinality: Cardinality::One,
            via: LinkVia::Learned,
            column: Some(col.clone()),
            target_column: None,
            table: None,
            confidence: j.confidence,
            reason: format!(
                "learned join {} ({}, confidence {:.2}): {}.{col} = {}.{to_col} with no foreign key",
                j.id, j.state, j.confidence, tables[&from_t].full, tables[&to_t].full
            ),
        });
    }

    // Object types.
    let referenced: HashMap<&str, usize> = {
        let mut m: HashMap<&str, BTreeSet<&str>> = HashMap::new();
        for t in tables.values() {
            for fk in &t.fks {
                if fk.to_table != t.id {
                    m.entry(fk.to_table.as_str()).or_default().insert(t.id.as_str());
                }
            }
        }
        m.into_iter().map(|(k, v)| (k, v.len())).collect()
    };
    for (id, key, key_reason, base_conf) in &keyed {
        let t = &tables[id];
        let name = names[id].clone();
        let mut reasons = vec![format!("{} → object type {name}", t.full), key_reason.clone()];
        let sing = singular(&snake(&t.name));
        let last = sing.rsplit('_').next().unwrap_or(&sing).to_string();
        let has_email = t.columns.iter().any(|c| snake(&c.name).contains("email"));
        let refs = referenced.get(id.as_str()).copied().unwrap_or(0);
        let category = if PEOPLE.contains(&last.as_str()) || (last == "account" && has_email) {
            reasons.push(format!("category actor: {} holds people", t.name));
            Category::Actor
        } else if refs >= 2 || CONTAINERS.contains(&last.as_str()) {
            reasons.push(if refs >= 2 {
                format!("category container: {refs} other tables reference it")
            } else {
                format!("category container: {} groups other things", t.name)
            });
            Category::Container
        } else {
            Category::Thing
        };

        // State column: a status-like column with a value list.
        let state_col = t
            .columns
            .iter()
            .find(|c| matches!(snake(&c.name).as_str(), "status" | "state" | "stage") && !c.enum_values.is_empty())
            .filter(|c| !key.contains(&c.name));
        let (states, terminal, alert) = match state_col {
            Some(c) => {
                let terminal: Vec<String> = c.enum_values.iter().filter(|v| state_is(v, TERMINAL)).cloned().collect();
                let alert: Vec<String> =
                    c.enum_values.iter().filter(|v| state_is(v, ALERT) && !terminal.contains(v)).cloned().collect();
                reasons.push(format!("states from {}: {}", c.name, c.enum_values.join(", ")));
                (c.enum_values.clone(), terminal, alert)
            }
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        for c in t.columns.iter().filter(|c| matches!(snake(&c.name).as_str(), "status" | "state" | "stage")) {
            if c.enum_values.is_empty() {
                reasons.push(format!("{} has no value list (enum or CHECK): kept as a property", c.name));
            }
        }

        // Title column.
        let linked = |c: &str| link_columns.contains(&(id.clone(), c.to_string()));
        let pick = |pred: &dyn Fn(&str) -> bool| {
            t.columns.iter().find(|c| !linked(&c.name) && pred(&snake(&c.name))).map(|c| c.name.clone())
        };
        let title = ["name", "title", "label", "display_name", "full_name"]
            .iter()
            .find_map(|w| pick(&|c| c == *w))
            .or_else(|| pick(&|c| c.ends_with("_name")))
            .or_else(|| ["email", "code", "sku", "slug"].iter().find_map(|w| pick(&|c| c == *w)));
        if let Some(c) = &title {
            reasons.push(format!("title column {c}"));
        }

        // Properties.
        let mut properties: Vec<DraftProperty> = Vec::new();
        let mut prop_names: BTreeSet<String> = BTreeSet::new();
        for c in &t.columns {
            if key.contains(&c.name) || linked(&c.name) || state_col.is_some_and(|s| s.name == c.name) {
                continue;
            }
            let mut pname = snake(&c.name);
            if RESERVED_PROPS.contains(&pname.as_str()) {
                pname = format!("{pname}_value");
            }
            let pname = free_name(&pname, |n| prop_names.contains(n));
            prop_names.insert(pname.clone());
            let (ty, mut reason) = prop_type(&c.name, &c.data_type, &c.enum_values);
            let is_title = title.as_deref() == Some(c.name.as_str());
            let required = !c.nullable;
            if required {
                reason.push_str("; NOT NULL → required");
            }
            if c.unique {
                reason.push_str("; unique");
            }
            let pattern = (ty == PropType::String && pname.contains("email")).then(|| r"^[^@\s]+@[^@\s]+$".to_string());
            if pattern.is_some() {
                reason.push_str("; email pattern");
            }
            if is_title {
                reason.push_str("; title column");
            }
            properties.push(DraftProperty {
                name: pname,
                column: c.name.clone(),
                data_type: c.data_type.clone(),
                ty,
                required,
                values: if ty == PropType::Enum { c.enum_values.clone() } else { Vec::new() },
                searchable: is_title || ty == PropType::Text,
                unique: c.unique,
                pattern,
                description: c.description.clone(),
                reason,
            });
        }
        let natural_key = key.len() == 1 && t.col(&key[0]).is_some_and(|c| is_textual(&c.data_type));
        if natural_key {
            reasons.push(format!("{} is a natural key people type", key[0]));
        }
        let s = f64::from(score(id));
        if s > 0.0 {
            reasons.push(format!("OntoRank {s:.2}"));
        }
        d.objects.push(DraftObject {
            label: title_case(&sing),
            description: if t.description.is_empty() {
                format!("A row of {} in {source}.", t.full)
            } else {
                t.description.clone()
            },
            table: t.full.clone(),
            table_id: t.id.clone(),
            category,
            key: key.clone(),
            natural_key,
            title,
            state: state_col.map(|c| c.name.clone()),
            states,
            terminal_states: terminal,
            alert_states: alert,
            properties,
            sync: opts.sync,
            confidence: round2(base_conf + (s.clamp(0.0, 1.0) * 0.09)),
            reasons,
            name,
        });
    }

    if opts.actions {
        draft_actions(&mut d, schema, &ours);
    }
    if d.objects.is_empty() {
        d.notes.push(format!("no table of {source} could become an object type"));
    }
    d.rebuild();
    d
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Is `t` only the many-to-many relation between two other tables?
fn is_join_table(t: &Tbl) -> bool {
    if t.is_view || t.fks.len() != 2 || t.fks[0].column == t.fks[1].column {
        return false;
    }
    let fk_cols: BTreeSet<&str> = t.fks.iter().map(|f| f.column.as_str()).collect();
    let pk: BTreeSet<&str> = t.columns.iter().filter(|c| c.primary_key).map(|c| c.name.as_str()).collect();
    let rest_is_timestamps = t
        .columns
        .iter()
        .filter(|c| !fk_cols.contains(c.name.as_str()) && !c.primary_key)
        .all(|c| is_timestamp(&c.data_type));
    if pk == fk_cols {
        return rest_is_timestamps;
    }
    let surrogate = pk.is_empty() || (pk.len() == 1 && pk.iter().all(|c| !fk_cols.contains(c)));
    surrogate && rest_is_timestamps
}

fn draft_actions(d: &mut OntologyDraft, schema: &Schema, ours: &BTreeSet<&str>) {
    let source = snake(&d.source);
    let our_action = |a: &ActionType| {
        a.parameters.values().any(|p| p.kinds.iter().any(|k| ours.contains(k.as_str())))
            || a.effects.iter().any(|e| e.create.as_ref().is_some_and(|c| ours.contains(c.kind.as_str())))
    };
    let mut taken: BTreeSet<String> =
        schema.action_types.values().filter(|a| !our_action(a)).map(|a| a.name.clone()).collect();
    let mut out: Vec<DraftAction> = Vec::new();
    let mut add = |out: &mut Vec<DraftAction>, mut a: DraftAction| {
        let base = a.name.clone();
        if taken.contains(&base) {
            a.name = truncate(&format!("{source}_{base}"), 64);
            a.reason.push_str(&format!(" (named {} because {base} exists)", a.name));
        }
        a.name = free_name(&a.name, |n| taken.contains(n));
        taken.insert(a.name.clone());
        out.push(a);
    };
    for o in &d.objects {
        let l = o.label.to_lowercase();
        let blocked: Vec<&str> = o
            .properties
            .iter()
            .filter(|p| p.required && param_type(p.ty).is_none() && o.title.as_deref() != Some(&p.column))
            .map(|p| p.name.as_str())
            .collect();
        if blocked.is_empty() {
            add(
                &mut out,
                DraftAction {
                    name: format!("create_{}", o.name),
                    object: o.name.clone(),
                    kind: ActionKind::Create,
                    risk: Risk::Low,
                    description: format!("Create a {l} ({}).", o.table),
                    phrases: vec![format!("create {l}"), format!("add {l}"), format!("new {l}")],
                    reason: "every object type can be created; required properties become parameters".into(),
                    ..Default::default()
                },
            );
        } else {
            d.notes.push(format!(
                "no create action for {}: required {} cannot be action parameters",
                o.name,
                blocked.join(", ")
            ));
        }
        if o.properties.iter().any(|p| param_type(p.ty).is_some() && p.ty != PropType::Timestamp) {
            add(
                &mut out,
                DraftAction {
                    name: format!("edit_{}", o.name),
                    object: o.name.clone(),
                    kind: ActionKind::Edit,
                    risk: Risk::Low,
                    description: format!("Edit the properties of a {l}."),
                    phrases: vec![format!("edit {l}"), format!("update {l}")],
                    reason: format!("edits up to {MAX_EDIT_PROPS} scalar properties"),
                    ..Default::default()
                },
            );
        }
        for v in &o.states {
            let Some((_, verb)) = VERBS.iter().find(|(s, _)| v.eq_ignore_ascii_case(s)) else { continue };
            let terminal = o.terminal_states.contains(v);
            add(
                &mut out,
                DraftAction {
                    name: format!("{verb}_{}", o.name),
                    object: o.name.clone(),
                    kind: ActionKind::Transition,
                    risk: if terminal { Risk::Medium } else { Risk::Low },
                    description: format!("Mark a {l} as {v}."),
                    phrases: vec![format!("{verb} {l}"), (*verb).to_string(), format!("mark {v}")],
                    state: Some(v.clone()),
                    reason: format!(
                        "state {v} of {}{}",
                        o.state.as_deref().unwrap_or("state"),
                        if terminal { " (final: medium risk)" } else { "" }
                    ),
                    ..Default::default()
                },
            );
        }
    }
    for lk in &d.links {
        if !matches!(lk.via, LinkVia::ForeignKey) {
            continue;
        }
        let ll = lk.label.to_lowercase();
        add(
            &mut out,
            DraftAction {
                name: truncate(&format!("set_{}", lk.name), 64),
                object: lk.from.clone(),
                kind: ActionKind::Relink,
                risk: Risk::Low,
                description: format!(
                    "Set the {ll} of a {} (replaces the current one).",
                    title_case(&lk.from).to_lowercase()
                ),
                phrases: vec![format!("set {ll}"), format!("change {ll}")],
                link: Some(lk.name.clone()),
                reason: format!("{} has one {ll} ({})", lk.from, lk.column.as_deref().unwrap_or_default()),
                ..Default::default()
            },
        );
    }
    d.actions = out;
}

// ── schema changes from the parts ────────────────────────────────────────

impl OntologyDraft {
    /// Recompute [`changes`](Self::changes) from objects, links and actions
    /// (after renaming or removing parts of the draft).
    pub fn rebuild(&mut self) {
        self.changes = self.schema_changes();
    }

    /// The change set that creates this draft: one `put_object_type` per
    /// object, then one `put_link_type` per link, then one
    /// `put_action_type` per action.
    pub fn schema_changes(&self) -> SchemaChangeSet {
        let mut changes = Vec::new();
        for o in &self.objects {
            changes.push(SchemaChange::PutObjectType {
                name: o.name.clone(),
                def: self.object_type(o),
                migrations: Vec::new(),
            });
        }
        for l in &self.links {
            changes.push(SchemaChange::PutLinkType {
                name: l.name.clone(),
                def: self.link_type(l),
                migrations: Vec::new(),
            });
        }
        for a in &self.actions {
            if let Some(def) = self.action_type(a) {
                changes.push(SchemaChange::PutActionType { name: a.name.clone(), def });
            }
        }
        SchemaChangeSet {
            changes,
            message: format!("ontology drafted from data source {}", self.source),
            ..Default::default()
        }
    }

    pub fn object(&self, name: &str) -> Option<&DraftObject> {
        self.objects.iter().find(|o| o.name == name)
    }

    fn object_type(&self, o: &DraftObject) -> ObjectType {
        let properties: BTreeMap<String, PropertyDef> = o
            .properties
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    PropertyDef {
                        ty: p.ty,
                        required: p.required,
                        values: if p.ty == PropType::Enum { p.values.clone() } else { Vec::new() },
                        searchable: p.searchable,
                        description: p.description.clone(),
                        pattern: p.pattern.clone(),
                        max_length: None,
                    },
                )
            })
            .collect();
        let links: BTreeMap<String, String> = self
            .links
            .iter()
            .filter(|l| l.from == o.name && l.table.is_none())
            .filter_map(|l| l.column.clone().map(|c| (l.name.clone(), c)))
            .collect();
        ObjectType {
            name: o.name.clone(),
            label: o.label.clone(),
            description: o.description.clone(),
            category: o.category,
            properties,
            states: o.states.clone(),
            terminal_states: o.terminal_states.clone(),
            alert_states: o.alert_states.clone(),
            constraints: Constraints {
                unique: o.properties.iter().filter(|p| p.unique).map(|p| p.name.clone()).collect(),
                ..Default::default()
            },
            backing: Some(Backing {
                source: self.source.clone(),
                table: o.table.clone(),
                key: o.key.clone(),
                sync: o.sync,
                title: o.title.clone(),
                state: o.state.clone(),
                properties: o.properties.iter().map(|p| (p.name.clone(), p.column.clone())).collect(),
                links,
                max_rows: None,
            }),
            ..Default::default()
        }
    }

    fn link_type(&self, l: &DraftLink) -> LinkType {
        LinkType {
            name: l.name.clone(),
            label: l.label.clone(),
            description: l.description.clone(),
            from: vec![l.from.clone()],
            to: vec![l.to.clone()],
            inverse: (!l.inverse.is_empty()).then(|| l.inverse.clone()),
            cardinality: l.cardinality,
            weight: 0.8,
            backing: match (&l.table, &l.column, &l.target_column) {
                (Some(table), Some(from), Some(to)) => Some(LinkBacking {
                    source: self.source.clone(),
                    table: table.clone(),
                    from: from.clone(),
                    to: to.clone(),
                }),
                _ => None,
            },
            ..Default::default()
        }
    }

    fn action_type(&self, a: &DraftAction) -> Option<ActionType> {
        let o = self.object(&a.object)?;
        let item = || ParamDef {
            ty: ParamType::Entity,
            kinds: vec![o.name.clone()],
            required: true,
            description: format!("The {}", o.label.to_lowercase()),
            ..Default::default()
        };
        let param_of = |p: &DraftProperty, required: bool| -> Option<ParamDef> {
            Some(ParamDef {
                ty: param_type(p.ty)?,
                values: if p.ty == PropType::Enum { p.values.clone() } else { Vec::new() },
                required,
                description: if p.description.is_empty() { title_case(&p.name) } else { p.description.clone() },
                ..Default::default()
            })
        };
        let pname = |n: &str| if RESERVED_PARAMS.contains(&n) { format!("{n}_value") } else { n.to_string() };
        let tpl = |n: &str| Value::String(format!("{{{n}}}"));
        let mut at = ActionType {
            name: a.name.clone(),
            description: a.description.clone(),
            phrases: a.phrases.clone(),
            risk: a.risk,
            ..Default::default()
        };
        match a.kind {
            ActionKind::Create => {
                let key_param = o.natural_key.then(|| pname(&snake(&o.key[0])));
                if let Some(k) = &key_param {
                    at.parameters.insert(
                        k.clone(),
                        ParamDef {
                            ty: ParamType::String,
                            required: true,
                            description: format!("Key ({})", o.key[0]),
                            ..Default::default()
                        },
                    );
                }
                let title_prop = o.properties.iter().find(|p| o.title.as_deref() == Some(p.column.as_str()));
                let mut props = Map::new();
                let mut title = None;
                if let Some(p) = title_prop
                    && let Some(def) = param_of(p, true)
                {
                    let n = pname(&p.name);
                    at.parameters.insert(n.clone(), def);
                    props.insert(p.name.clone(), tpl(&n));
                    title = Some(format!("{{{n}}}"));
                } else if o.title.is_some() && o.title == o.key.first().cloned() {
                    title = key_param.as_ref().map(|k| format!("{{{k}}}"));
                }
                for p in o.properties.iter().filter(|p| p.required) {
                    if props.contains_key(&p.name) {
                        continue;
                    }
                    let Some(def) = param_of(p, true) else { continue };
                    let n = pname(&p.name);
                    at.parameters.insert(n.clone(), def);
                    props.insert(p.name.clone(), tpl(&n));
                }
                at.effects.push(Effect {
                    create: Some(CreateEffect {
                        kind: o.name.clone(),
                        key: Some(match &key_param {
                            Some(k) => format!("{{{k}}}"),
                            None => "{new_id}".into(),
                        }),
                        title,
                        state: o.states.first().cloned(),
                        props,
                        links: Vec::new(),
                    }),
                    ..Default::default()
                });
            }
            ActionKind::Edit => {
                at.parameters.insert("item".into(), item());
                let mut props = Map::new();
                let mut title = None;
                for p in o
                    .properties
                    .iter()
                    .filter(|p| param_type(p.ty).is_some() && p.ty != PropType::Timestamp)
                    .take(MAX_EDIT_PROPS)
                {
                    let Some(def) = param_of(p, false) else { continue };
                    let n = pname(&p.name);
                    at.parameters.insert(n.clone(), def);
                    props.insert(p.name.clone(), tpl(&n));
                    if o.title.as_deref() == Some(p.column.as_str()) {
                        title = Some(format!("{{{n}}}"));
                    }
                }
                at.effects.push(Effect {
                    patch: Some(PatchEffect { id: "{item}".into(), title, state: None, props }),
                    ..Default::default()
                });
            }
            ActionKind::Transition => {
                let state = a.state.clone()?;
                at.parameters.insert("item".into(), item());
                at.preconditions.push(Check {
                    check: CheckKind::StateAllowed,
                    param: Some("item".into()),
                    rel: None,
                    to: None,
                    prop: None,
                    value: Some(json!(state)),
                    values: Vec::new(),
                    other: None,
                    message: None,
                });
                at.effects.push(Effect {
                    patch: Some(PatchEffect {
                        id: "{item}".into(),
                        title: None,
                        state: Some(state),
                        props: Map::new(),
                    }),
                    ..Default::default()
                });
            }
            ActionKind::Relink => {
                let link = self.links.iter().find(|l| Some(&l.name) == a.link.as_ref())?;
                at.parameters.insert("item".into(), item());
                at.parameters.insert(
                    "target".into(),
                    ParamDef {
                        ty: ParamType::Entity,
                        kinds: vec![link.to.clone()],
                        required: true,
                        description: format!("The new {}", link.label.to_lowercase()),
                        cue: vec!["to".into()],
                        ..Default::default()
                    },
                );
                at.effects.push(Effect {
                    link: Some(LinkEffect {
                        from: "{item}".into(),
                        rel: link.name.clone(),
                        to: "{target}".into(),
                        props: Map::new(),
                    }),
                    replace: true,
                    ..Default::default()
                });
            }
        }
        Some(at)
    }
}

impl Ontology {
    /// Draft an ontology from the crawled catalog of `opts.source`, as `p`
    /// sees it (tables and columns hidden from `p` are left out). Reads only.
    pub fn draft_ontology(&self, p: &Principal, opts: &DraftOptions) -> Result<OntologyDraft> {
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        if opts.source.trim().is_empty() {
            return Err(Error::invalid("a draft needs a source (the connector name)"));
        }
        let scores = self.authority_scores();
        let schema = self.schema();
        let g = self.inner_graph_read();
        let src = crate::catalog::source_id(opts.source.trim());
        if !g.get(&src).is_some_and(|e| policy::visible(p, e, &g)) {
            return Err(Error::not_found(src));
        }
        Ok(draft_visible(&g, &schema, &scores, opts, &|e| policy::visible(p, e, &g)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        self, CatalogSnapshot, ColumnInfo, ForeignKey, MapOptions, RelKind, RelationInfo, SchemaInfo, SourceInfo,
    };
    use crate::engine::ActionContext;

    #[test]
    fn names_singulars_plurals_and_globs() {
        for (p, s) in [
            ("orders", "order"),
            ("order_items", "order_item"),
            ("categories", "category"),
            ("addresses", "address"),
            ("statuses", "status"),
            ("people", "person"),
            ("data", "data"),
            ("warehouses", "warehouse"),
            ("boxes", "box"),
            ("branches", "branch"),
            ("status", "status"),
            ("courses", "course"),
            ("caches", "cache"),
            ("niches", "niche"),
            ("matches", "match"),
            ("coaches", "coach"),
            ("causes", "cause"),
            ("clauses", "clause"),
            ("excuses", "excuse"),
            ("uses", "use"),
            ("buses", "bus"),
            ("bonuses", "bonus"),
            ("aliases", "alias"),
            ("canvases", "canvas"),
            ("cases", "case"),
            ("purchases", "purchase"),
            ("databases", "database"),
            ("houses", "house"),
        ] {
            assert_eq!(singular(p), s, "{p}");
        }
        for (s, p) in [("order", "orders"), ("category", "categories"), ("address", "addresses"), ("person", "people")]
        {
            assert_eq!(plural(s), p, "{s}");
        }
        assert_eq!(title_case("order_item"), "Order Item");
        assert_eq!(snake("OrderItems"), "order_items");
        assert_eq!(snake("2fa codes"), "t_2fa_codes");
        assert_eq!(fk_stem("customer_id"), "customer");
        assert_eq!(fk_stem("id"), "id");
        assert!(table_glob("staging.*", "staging.revenue_tmp", "revenue_tmp"));
        assert!(table_glob("*_tmp", "staging.revenue_tmp", "revenue_tmp"));
        assert!(table_glob("orders", "sales.orders", "orders"));
        assert!(!table_glob("sales.orders", "archive.orders", "orders"));
    }

    #[test]
    fn terminal_and_alert_states_match_whole_words() {
        let terminal = |vs: &[&'static str]| vs.iter().filter(|v| state_is(v, TERMINAL)).copied().collect::<Vec<_>>();
        assert_eq!(
            terminal(&["draft", "unpaid", "paid", "incomplete", "not_paid", "Partially Paid"]),
            ["paid", "Partially Paid"]
        );
        assert_eq!(
            terminal(&["cancelled", "canceled", "completed", "closed_won", "undelivered", "unresolved"]),
            ["cancelled", "canceled", "completed", "closed_won"]
        );
        let alert = |vs: &[&'static str]| vs.iter().filter(|v| state_is(v, ALERT)).copied().collect::<Vec<_>>();
        assert_eq!(
            alert(&["blocked", "unblocked", "failed", "past_due", "on-hold", "error", "holding"]),
            ["blocked", "failed", "past_due", "on-hold", "error"]
        );
    }

    #[test]
    fn column_types_map_to_property_types() {
        let t = |name: &str, dt: &str| prop_type(name, dt, &[]).0;
        assert_eq!(t("qty", "integer"), PropType::Int);
        assert_eq!(t("amount", "numeric(12,2)"), PropType::Float);
        assert_eq!(t("active", "boolean"), PropType::Bool);
        assert_eq!(t("created_at", "timestamp with time zone"), PropType::Timestamp);
        assert_eq!(t("day", "date"), PropType::Timestamp);
        assert_eq!(t("meta", "jsonb"), PropType::Json);
        assert_eq!(t("tags", "text[]"), PropType::List);
        assert_eq!(t("region", "character varying(40)"), PropType::String);
        assert_eq!(t("notes", "text"), PropType::Text);
        assert_eq!(t("order_description", "text"), PropType::Text);
        assert_eq!(t("addr", "inet"), PropType::String);
        assert_eq!(prop_type("status", "order_status", &["a".into()]).0, PropType::Enum);
    }

    fn col(name: &str, data_type: &str, comment: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
            comment: comment.into(),
            ..Default::default()
        }
    }

    fn pk(name: &str, comment: &str) -> ColumnInfo {
        ColumnInfo { primary_key: true, nullable: false, unique: true, ..col(name, "bigint", comment) }
    }

    fn fk(name: &str, from: &str, column: &str, to: &str) -> ForeignKey {
        ForeignKey {
            name: name.into(),
            from_schema: "sales".into(),
            from_table: from.into(),
            from_columns: vec![column.into()],
            to_schema: "sales".into(),
            to_table: to.into(),
            to_columns: vec!["id".into()],
        }
    }

    /// The demo warehouse's shape, plus an enum status, a unique email, a
    /// composite key on order_items, a tag join table and a team table whose
    /// name collides with the enterprise `team` type.
    fn warehouse() -> CatalogSnapshot {
        let rel = |schema: &str, name: &str, comment: &str, columns: Vec<ColumnInfo>| RelationInfo {
            schema: schema.into(),
            name: name.into(),
            comment: comment.into(),
            columns,
            public_read: true,
            ..Default::default()
        };
        let statuses = ["placed", "paid", "shipped", "cancelled", "refunded", "on_hold"];
        CatalogSnapshot {
            source: SourceInfo { key: "warehouse".into(), engine: "postgres".into(), ..Default::default() },
            schemas: ["sales", "finance", "staging"]
                .iter()
                .map(|s| SchemaInfo { name: (*s).into(), ..Default::default() })
                .collect(),
            relations: vec![
                rel(
                    "sales",
                    "customers",
                    "One row per customer account",
                    vec![
                        pk("id", "Customer id"),
                        col("cust_name", "text", "Customer name"),
                        ColumnInfo { unique: true, nullable: false, ..col("email", "citext", "Login email") },
                        col("region", "text", "Sales region"),
                        col("created_at", "timestamptz", "When the account was opened"),
                    ],
                ),
                rel(
                    "sales",
                    "orders",
                    "One row per order placed in the shop",
                    vec![
                        pk("id", "Order id"),
                        col("customer_id", "bigint", "Customer who placed the order"),
                        col("amount", "numeric(12,2)", "Order total in EUR, also known as GMV"),
                        ColumnInfo {
                            enum_values: statuses.iter().map(|s| s.to_string()).collect(),
                            nullable: false,
                            ..col("status", "sales.order_status", "Where the order is")
                        },
                        col("notes", "text", ""),
                        col("created_at", "timestamptz", "When the order was placed"),
                    ],
                ),
                rel(
                    "sales",
                    "order_items",
                    "Line items of each order",
                    vec![
                        ColumnInfo { primary_key: true, nullable: false, ..col("order_id", "bigint", "") },
                        ColumnInfo { primary_key: true, nullable: false, ..col("product_sku", "text", "") },
                        col("qty", "integer", ""),
                        col("unit_price", "numeric(12,2)", "Price per unit in EUR"),
                    ],
                ),
                rel("sales", "tags", "", vec![pk("id", ""), col("label", "text", "")]),
                rel(
                    "sales",
                    "order_tags",
                    "",
                    vec![
                        ColumnInfo { primary_key: true, nullable: false, ..col("order_id", "bigint", "") },
                        ColumnInfo { primary_key: true, nullable: false, ..col("tag_id", "bigint", "") },
                        col("tagged_at", "timestamptz", ""),
                    ],
                ),
                rel("sales", "teams", "Sales teams", vec![pk("id", ""), col("name", "text", "")]),
                rel("sales", "audit_log", "", vec![col("at", "timestamptz", ""), col("what", "text", "")]),
                RelationInfo {
                    kind: RelKind::View,
                    ..rel(
                        "finance",
                        "revenue_recognized",
                        "",
                        vec![col("day", "timestamptz", ""), col("revenue", "numeric", "")],
                    )
                },
                rel("staging", "revenue_tmp", "", vec![col("day", "date", ""), col("rev", "numeric", "")]),
            ],
            foreign_keys: vec![
                fk("orders_customer_id_fkey", "orders", "customer_id", "customers"),
                fk("order_items_order_id_fkey", "order_items", "order_id", "orders"),
                fk("order_tags_order_id_fkey", "order_tags", "order_id", "orders"),
                fk("order_tags_tag_id_fkey", "order_tags", "tag_id", "tags"),
            ],
            ..Default::default()
        }
    }

    fn world() -> Ontology {
        let mut schema = Schema::enterprise();
        schema.extend_toml(crate::schema::builtin_package("data-catalog").unwrap()).unwrap();
        let o = Ontology::in_memory(schema);
        let opts = MapOptions { workspace: "acme".into(), connector: "warehouse".into(), ..Default::default() };
        let plan = catalog::plan_changes(
            catalog::map_snapshot(&warehouse(), &opts),
            &Default::default(),
            &crate::graph::Graph::default(),
            catalog::MANAGED_RELS,
        );
        o.apply(&ActionContext::new(Principal::admin("crawler"), "acme"), plan.mutations).unwrap();
        o
    }

    fn admin() -> Principal {
        Principal::admin("ada")
    }

    #[test]
    fn drafts_types_keys_titles_and_properties_from_the_warehouse() {
        let o = world();
        let d = o.draft_ontology(&admin(), &DraftOptions::for_source("warehouse")).unwrap();
        let names: BTreeSet<&str> = d.objects.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(
            names,
            BTreeSet::from(["customer", "order", "order_item", "tag", "warehouse_team"]),
            "{:#?}",
            d.skipped
        );
        let customer = d.object("customer").unwrap();
        assert_eq!(customer.label, "Customer");
        assert_eq!(customer.key, vec!["id"]);
        assert_eq!(customer.title.as_deref(), Some("cust_name"));
        assert_eq!(customer.category, Category::Actor);
        assert_eq!(customer.description, "One row per customer account");
        let email = customer.properties.iter().find(|p| p.name == "email").unwrap();
        assert!(email.unique && email.required && email.pattern.is_some(), "{email:?}");
        let created = customer.properties.iter().find(|p| p.name == "created_at").unwrap();
        assert_eq!(created.ty, PropType::Timestamp);
        assert!(customer.properties.iter().find(|p| p.name == "cust_name").unwrap().searchable);
        assert!(!customer.properties.iter().any(|p| p.name == "id"), "key columns are not properties");

        let order = d.object("order").unwrap();
        assert_eq!(order.category, Category::Container, "orders are referenced by order_items and order_tags");
        assert_eq!(order.state.as_deref(), Some("status"));
        assert_eq!(order.states, ["placed", "paid", "shipped", "cancelled", "refunded", "on_hold"]);
        assert_eq!(order.terminal_states, ["paid", "shipped", "cancelled", "refunded"]);
        assert_eq!(order.alert_states, ["on_hold"]);
        let props: BTreeMap<&str, PropType> = order.properties.iter().map(|p| (p.name.as_str(), p.ty)).collect();
        assert_eq!(
            props,
            BTreeMap::from([
                ("amount", PropType::Float),
                ("notes", PropType::Text),
                ("created_at", PropType::Timestamp)
            ]),
            "the foreign key is a link and the status the state"
        );
        assert_eq!(order.title, None);

        let item = d.object("order_item").unwrap();
        assert_eq!(item.key, vec!["order_id", "product_sku"]);
        assert_eq!(item.label, "Order Item");
        let qty = item.properties.iter().find(|p| p.name == "qty").unwrap();
        assert_eq!(qty.ty, PropType::Int);

        // Collisions: the enterprise kind keeps its name.
        let team = d.object("warehouse_team").unwrap();
        assert_eq!(team.label, "Team");
        assert_eq!(team.category, Category::Container);
        assert!(d.notes.iter().any(|n| n.contains("sales.teams") && n.contains("warehouse_team")), "{:?}", d.notes);

        // Skipped: no key, a view, staging.
        let skipped: BTreeMap<&str, &str> = d.skipped.iter().map(|s| (s.table.as_str(), s.reason.as_str())).collect();
        assert_eq!(skipped["sales.audit_log"], "no primary key");
        assert!(skipped["finance.revenue_recognized"].contains("view"));
        assert!(skipped["staging.revenue_tmp"].contains("staging.*"));
        assert!(!skipped.contains_key("sales.order_tags"), "a join table becomes a link");
    }

    #[test]
    fn drafts_links_inverses_join_tables_and_actions() {
        let o = world();
        let d = o.draft_ontology(&admin(), &DraftOptions::for_source("warehouse")).unwrap();
        let link = |n: &str| d.links.iter().find(|l| l.name == n).unwrap_or_else(|| panic!("{n}: {:#?}", d.links));
        let c = link("customer");
        assert_eq!((c.from.as_str(), c.to.as_str(), c.inverse.as_str()), ("order", "customer", "orders"));
        assert_eq!(
            (c.cardinality, c.via, c.column.as_deref()),
            (Cardinality::One, LinkVia::ForeignKey, Some("customer_id"))
        );
        let ord = link("order");
        assert_eq!((ord.from.as_str(), ord.to.as_str(), ord.inverse.as_str()), ("order_item", "order", "order_items"));
        let tags = link("tags");
        assert_eq!((tags.from.as_str(), tags.to.as_str(), tags.inverse.as_str()), ("order", "tag", "tag_orders"));
        assert_eq!((tags.cardinality, tags.via), (Cardinality::Many, LinkVia::JoinTable));
        assert_eq!(tags.table.as_deref(), Some("sales.order_tags"));

        let actions: BTreeMap<&str, (&str, ActionKind, Risk)> =
            d.actions.iter().map(|a| (a.name.as_str(), (a.object.as_str(), a.kind, a.risk))).collect();
        assert_eq!(actions["create_order"], ("order", ActionKind::Create, Risk::Low));
        assert_eq!(actions["edit_customer"], ("customer", ActionKind::Edit, Risk::Low));
        assert_eq!(actions["cancel_order"], ("order", ActionKind::Transition, Risk::Medium));
        assert_eq!(actions["ship_order"].2, Risk::Medium);
        assert_eq!(actions["refund_order"].2, Risk::Medium);
        assert_eq!(actions["set_customer"], ("order", ActionKind::Relink, Risk::Low));
        assert!(!actions.contains_key("set_tags"), "only foreign key links get a relink action");

        // The change set: objects with backings, links, then actions.
        let ops: Vec<&str> = d.changes.changes.iter().map(|c| c.op()).collect();
        let first_link = ops.iter().position(|o| *o == "put_link_type").unwrap();
        let first_action = ops.iter().position(|o| *o == "put_action_type").unwrap();
        assert!(ops[..first_link].iter().all(|o| *o == "put_object_type"));
        assert!(ops[first_link..first_action].iter().all(|o| *o == "put_link_type"));
        let def = |name: &str| {
            d.changes.changes.iter().find_map(|c| match c {
                SchemaChange::PutObjectType { name: n, def, .. } if n == name => Some(def.clone()),
                _ => None,
            })
        };
        let order = def("order").unwrap();
        let b = order.backing.unwrap();
        assert_eq!(
            (b.source.as_str(), b.table.as_str(), b.key.clone()),
            ("warehouse", "sales.orders", vec!["id".to_string()])
        );
        assert_eq!(b.state.as_deref(), Some("status"));
        assert!(b.sync);
        assert_eq!(b.links, BTreeMap::from([("customer".to_string(), "customer_id".to_string())]));
        assert_eq!(b.properties["amount"], "amount");
        assert_eq!(def("customer").unwrap().constraints.unique, vec!["email"]);
        let tag_link = d.changes.changes.iter().find_map(|c| match c {
            SchemaChange::PutLinkType { name, def, .. } if name == "tags" => def.backing.clone(),
            _ => None,
        });
        assert_eq!(
            tag_link,
            Some(LinkBacking {
                source: "warehouse".into(),
                table: "sales.order_tags".into(),
                from: "order_id".into(),
                to: "tag_id".into()
            })
        );

        // The draft is a valid schema change on enterprise + data-catalog.
        let plan = o.schema_plan(&admin(), &d.changes).unwrap();
        assert!(plan.valid, "{:?}", plan.errors);
        assert!(!plan.breaking, "{:?}", plan.needs_migration);
    }

    #[test]
    fn generated_actions_run_after_the_draft_is_applied() {
        let o = world();
        let d = o.draft_ontology(&admin(), &DraftOptions::for_source("warehouse")).unwrap();
        let ctx = ActionContext::new(admin(), "acme");
        o.schema_apply(&ctx, d.changes.clone()).unwrap();
        assert!(o.schema().object_types["order"].backing.is_some());
        let opts = crate::action_types::ApplyOptions { confirm: true, ..Default::default() };
        let attempt = |name: &str, params: Value| o.apply_action(&ctx, name, params.as_object().unwrap(), opts);
        let run = |name: &str, params: Value| attempt(name, params).unwrap();
        let created = |r: &crate::action_types::ActionReceipt, kind: &str| {
            assert_eq!(r.status, "applied", "{r:?}");
            r.result.as_ref().unwrap().entities.iter().find(|id| id.starts_with(kind)).cloned().unwrap()
        };
        let customer =
            created(&run("create_customer", json!({ "cust_name": "Ada", "email": "ada@acme.test" })), "customer:");
        assert_eq!(o.get(&admin(), &customer).unwrap().title, "Ada");
        let bad = attempt("create_customer", json!({ "cust_name": "Bo", "email": "not an email" }));
        assert!(bad.is_err_and(|e| e.to_string().contains("email")), "the email pattern holds");
        let order = created(&run("create_order", json!({})), "order:");
        assert_eq!(o.get(&admin(), &order).unwrap().state.as_deref(), Some("placed"));
        assert_eq!(run("set_customer", json!({ "item": order, "target": customer })).status, "applied");
        assert_eq!(run("edit_order", json!({ "item": order, "amount": 12.5 })).status, "applied");
        let cancel = run("cancel_order", json!({ "item": order }));
        assert_eq!(cancel.status, "applied", "{cancel:?}");
        let e = o.get(&admin(), &order).unwrap();
        assert_eq!(e.state.as_deref(), Some("cancelled"));
        assert_eq!(e.props["amount"], json!(12.5));
        let linked =
            o.read(|g, _| g.neighbors(&order, "customer", Dir::Out).iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert_eq!(linked, vec![customer]);

        // A second draft of the same source reuses its names.
        let again = o.draft_ontology(&admin(), &DraftOptions::for_source("warehouse")).unwrap();
        assert!(again.object("order").is_some() && again.links.iter().any(|l| l.name == "customer"));
        assert!(again.actions.iter().any(|a| a.name == "create_order"));
        let plan = o.schema_plan(&admin(), &again.changes).unwrap();
        assert!(plan.valid, "{:?}", plan.errors);
    }

    fn types_backing(o: &Ontology, table: &str) -> Vec<String> {
        o.schema()
            .object_types
            .values()
            .filter(|t| t.backing.as_ref().is_some_and(|b| b.table == table))
            .map(|t| t.name.clone())
            .collect()
    }

    fn new_object_types(o: &Ontology, d: &OntologyDraft) -> Vec<String> {
        let schema = o.schema();
        d.changes
            .changes
            .iter()
            .filter_map(|c| match c {
                SchemaChange::PutObjectType { name, .. } if !schema.object_types.contains_key(name) => {
                    Some(name.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_redraft_keeps_the_names_of_types_and_links_it_made_before() {
        let o = world();
        let ctx = ActionContext::new(admin(), "acme");
        // Applied with a prefix, then drafted again without one.
        let crm = DraftOptions { prefix: Some("crm".into()), ..DraftOptions::for_source("warehouse") };
        let first = o.draft_ontology(&admin(), &crm).unwrap();
        assert!(first.object("crm_order").is_some() && first.object("crm_customer").is_some());
        o.schema_apply(&ctx, first.changes.clone()).unwrap();
        let again = o.draft_ontology(&admin(), &DraftOptions::for_source("warehouse")).unwrap();
        assert!(new_object_types(&o, &again).is_empty(), "{:?}", new_object_types(&o, &again));
        assert!(again.object("crm_order").is_some() && again.object("order").is_none());
        let link_names =
            |d: &OntologyDraft| d.links.iter().map(|l| (l.name.clone(), l.inverse.clone())).collect::<Vec<_>>();
        assert_eq!(link_names(&again), link_names(&first), "links keep their names and inverses");
        let plan = o.schema_plan(&admin(), &again.changes).unwrap();
        assert!(plan.diff.is_empty(), "the same ontology again: {:?}", plan.diff);
        assert_eq!(types_backing(&o, "sales.orders"), ["crm_order"], "one object type per table");

        // A type renamed in the builder (customer → buyer) stays renamed.
        let o = world();
        let mut d = o.draft_ontology(&admin(), &DraftOptions::for_source("warehouse")).unwrap();
        for x in d.objects.iter_mut().filter(|x| x.name == "customer") {
            x.name = "buyer".into();
        }
        for l in d.links.iter_mut().filter(|l| l.to == "customer") {
            l.to = "buyer".into();
        }
        for a in d.actions.iter_mut().filter(|a| a.object == "customer") {
            a.object = "buyer".into();
            a.name = a.name.replace("_customer", "_buyer");
        }
        d.rebuild();
        o.schema_apply(&ctx, d.changes.clone()).unwrap();
        let again = o.draft_ontology(&admin(), &DraftOptions::for_source("warehouse")).unwrap();
        assert!(new_object_types(&o, &again).is_empty(), "{:?}", new_object_types(&o, &again));
        assert!(again.object("buyer").is_some() && again.object("customer").is_none());
        let fk = again.links.iter().find(|l| l.column.as_deref() == Some("customer_id")).unwrap();
        assert_eq!((fk.name.as_str(), fk.to.as_str()), ("customer", "buyer"), "not customer_2");
        let plan = o.schema_plan(&admin(), &again.changes).unwrap();
        let added = plan.diff.entries.iter().filter(|e| e.change == crate::live_schema::DiffKind::Added).count();
        assert!(plan.valid && added == 0, "{:?} {:?}", plan.errors, plan.diff);
        assert_eq!(types_backing(&o, "sales.customers"), ["buyer"]);
    }

    #[test]
    fn two_types_may_not_sync_one_table() {
        let o = world();
        let d = o.draft_ontology(&admin(), &DraftOptions::for_source("warehouse")).unwrap();
        let mut changes = d.changes.clone();
        let SchemaChange::PutObjectType { def, .. } = changes
            .changes
            .iter()
            .find(|c| matches!(c, SchemaChange::PutObjectType { name, .. } if name == "order"))
            .unwrap()
        else {
            unreachable!()
        };
        let mut copy = def.clone();
        copy.name = "order_copy".into();
        copy.backing.as_mut().unwrap().links.clear();
        changes.changes.push(SchemaChange::PutObjectType {
            name: "order_copy".into(),
            def: copy,
            migrations: Vec::new(),
        });
        let plan = o.schema_plan(&admin(), &changes).unwrap();
        assert!(!plan.valid);
        assert!(
            plan.errors.iter().any(|e| e.contains("order, order_copy") && e.contains("sales.orders")),
            "{:?}",
            plan.errors
        );
        // Without sync, a second backing reads nothing: allowed.
        if let Some(SchemaChange::PutObjectType { def, .. }) = changes.changes.last_mut() {
            def.backing.as_mut().unwrap().sync = false;
        }
        assert!(o.schema_plan(&admin(), &changes).unwrap().valid);
    }

    #[test]
    fn join_tables_must_reference_the_keys_they_join() {
        let schema = {
            let mut s = Schema::enterprise();
            s.extend_toml(crate::schema::builtin_package("data-catalog").unwrap()).unwrap();
            s
        };
        let o = Ontology::in_memory(schema);
        let rel = |name: &str, columns: Vec<ColumnInfo>| RelationInfo {
            schema: "sales".into(),
            name: name.into(),
            columns,
            public_read: true,
            ..Default::default()
        };
        let to = |from: &str, column: &str, table: &str, target: &str| ForeignKey {
            name: format!("{from}_{column}_fkey"),
            from_schema: "sales".into(),
            from_table: from.into(),
            from_columns: vec![column.into()],
            to_schema: "sales".into(),
            to_table: table.into(),
            to_columns: vec![target.into()],
        };
        let snap = CatalogSnapshot {
            source: SourceInfo { key: "shop".into(), engine: "postgres".into(), ..Default::default() },
            schemas: vec![SchemaInfo { name: "sales".into(), ..Default::default() }],
            relations: vec![
                rel("orders", vec![pk("id", "")]),
                rel("products", vec![pk("id", ""), ColumnInfo { unique: true, ..col("sku", "text", "") }]),
                rel("tags", vec![pk("id", "")]),
                rel(
                    "order_products",
                    vec![
                        ColumnInfo { primary_key: true, nullable: false, ..col("order_id", "bigint", "") },
                        ColumnInfo { primary_key: true, nullable: false, ..col("product_sku", "text", "") },
                    ],
                ),
                rel(
                    "order_tags",
                    vec![
                        ColumnInfo { primary_key: true, nullable: false, ..col("order_id", "bigint", "") },
                        ColumnInfo { primary_key: true, nullable: false, ..col("tag_id", "bigint", "") },
                    ],
                ),
            ],
            foreign_keys: vec![
                to("order_products", "order_id", "orders", "id"),
                to("order_products", "product_sku", "products", "sku"),
                to("order_tags", "order_id", "orders", "id"),
                to("order_tags", "tag_id", "tags", "id"),
            ],
            ..Default::default()
        };
        let opts = MapOptions { workspace: "acme".into(), connector: "shop".into(), ..Default::default() };
        let plan = catalog::plan_changes(
            catalog::map_snapshot(&snap, &opts),
            &Default::default(),
            &crate::graph::Graph::default(),
            catalog::MANAGED_RELS,
        );
        o.apply(&ActionContext::new(Principal::admin("crawler"), "acme"), plan.mutations).unwrap();
        let d = o.draft_ontology(&admin(), &DraftOptions::for_source("shop")).unwrap();
        let joins: Vec<&str> =
            d.links.iter().filter(|l| l.via == LinkVia::JoinTable).filter_map(|l| l.table.as_deref()).collect();
        assert_eq!(joins, ["sales.order_tags"], "{:#?}", d.links);
        let skipped = d.skipped.iter().find(|s| s.table == "sales.order_products").unwrap();
        assert!(skipped.reason.contains("product_sku") && skipped.reason.contains("not its key"), "{skipped:?}");
    }

    #[test]
    fn options_prefixes_filters_and_learned_links() {
        let o = world();
        let opts = DraftOptions {
            tables: vec!["sales.orders".into(), "sales.customers".into()],
            prefix: Some("shop".into()),
            actions: false,
            sync: false,
            ..DraftOptions::for_source("warehouse")
        };
        let d = o.draft_ontology(&admin(), &opts).unwrap();
        let names: Vec<&str> = d.objects.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"shop_order") && names.contains(&"shop_customer"), "{names:?}");
        assert!(d.actions.is_empty() && d.objects.iter().all(|x| !x.sync));
        assert_eq!(d.links.len(), 1);
        assert!(o.schema_plan(&admin(), &d.changes).unwrap().valid);

        // Nothing excluded: the staging table has no key and is skipped for that.
        let all = DraftOptions { exclude: Some(Vec::new()), ..DraftOptions::for_source("warehouse") };
        let d = o.draft_ontology(&admin(), &all).unwrap();
        assert!(d.skipped.iter().any(|s| s.table == "staging.revenue_tmp" && s.reason == "no primary key"));

        // Views on request, keyed by an id column when they have one.
        let views = DraftOptions { views: true, ..DraftOptions::for_source("warehouse") };
        let d = o.draft_ontology(&admin(), &views).unwrap();
        assert!(d.skipped.iter().any(|s| s.table == "finance.revenue_recognized" && s.reason == "no primary key"));

        // A learned join with no foreign key becomes a link.
        let ctx = ActionContext::new(admin(), "acme");
        o.apply(
            &ctx,
            vec![crate::model::Mutation::Upsert(
                crate::model::EntityInput::new("learned_fact", "join_path.x1", "order_items joins tags")
                    .state("suggested")
                    .prop("fact_type", "join_path")
                    .prop("expression", "order_items.product_sku = tags.label")
                    .prop("confidence", 0.4)
                    .link("concerns", "table:warehouse.sales.order_items")
                    .link("concerns", "table:warehouse.sales.tags"),
            )],
        )
        .unwrap();
        let weak = o.draft_ontology(&admin(), &DraftOptions::for_source("warehouse")).unwrap();
        assert!(weak.links.iter().all(|l| l.via != LinkVia::Learned), "below the confidence floor");
        o.apply(
            &ctx,
            vec![crate::model::Mutation::Upsert(
                crate::model::EntityInput::new("learned_fact", "join_path.x2", "order_items joins customers")
                    .state("certified")
                    .prop("fact_type", "join_path")
                    .prop("expression", "order_items.qty = customers.id")
                    .prop("confidence", 0.8)
                    .link("concerns", "table:warehouse.sales.order_items")
                    .link("concerns", "table:warehouse.sales.customers"),
            )],
        )
        .unwrap();
        let d = o.draft_ontology(&admin(), &DraftOptions::for_source("warehouse")).unwrap();
        let l = d.links.iter().find(|l| l.via == LinkVia::Learned).unwrap();
        assert_eq!(
            (l.name.as_str(), l.from.as_str(), l.to.as_str(), l.confidence),
            ("qty", "order_item", "customer", 0.8)
        );
        assert!(o.schema_plan(&admin(), &d.changes).unwrap().valid);
    }

    #[test]
    fn drafts_follow_what_the_principal_may_see() {
        let o = world();
        assert!(o.draft_ontology(&admin(), &DraftOptions::for_source("nope")).is_err());
        let ctx = ActionContext::new(admin(), "acme");
        o.apply(
            &ctx,
            vec![crate::model::Mutation::Patch(crate::model::EntityPatch {
                id: "table:warehouse.sales.orders".into(),
                props: json!({ "visibility": "restricted", "acl": ["team:finance"] }).as_object().cloned().unwrap(),
                ..Default::default()
            })],
        )
        .unwrap();
        let member = Principal::named("user:bo", &["member"], &["*"]).unwrap();
        let d = o.draft_ontology(&member, &DraftOptions::for_source("warehouse")).unwrap();
        assert!(d.object("order").is_none());
        assert!(d.objects.iter().all(|x| x.table != "sales.orders"));
        assert!(d.object("customer").is_some());
        // Draft from the pure function sees everything.
        let full = o.read(|g, s| draft(g, s, &Scores::default(), &DraftOptions::for_source("warehouse")));
        assert!(full.object("order").is_some());
    }
}
