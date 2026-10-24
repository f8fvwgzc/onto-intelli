//! Dynamic ontology schema: object types, link types, inference rules,
//! shapes and analysis settings, loaded from TOML at runtime and merged in
//! layers.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::action_types::ActionType;
use crate::metrics::{DerivedDef, MetricDef};
use crate::quality::Constraints;
use crate::shape_model::Shape;
use crate::{Error, Result};

/// The built-in Enterprise Execution ontology.
pub const ENTERPRISE_SCHEMA: &str = include_str!("../schemas/enterprise.toml");

/// nexc-engine's execution vocabulary: graphs, node types, relation types.
pub const NEXC_EXECUTION_SCHEMA: &str = include_str!("../schemas/nexc-execution.toml");

/// A data estate: data sources, schemas, tables, views, columns, queries and
/// learned facts, written by connectors (layered on enterprise by default).
pub const DATA_CATALOG_SCHEMA: &str = include_str!("../schemas/data-catalog.toml");

/// A repository's packages, their internal dependencies and owners, written
/// by the repository connector (layered after data-catalog by default).
pub const CODE_SCHEMA: &str = include_str!("../schemas/code.toml");

/// Built-in schema packages, layered with `extend = ["builtin:<name>"]`.
/// The industry packs of [`crate::packs::REGISTRY`] are layers on enterprise.
pub const PACKAGES: &[(&str, &str)] = &[
    ("enterprise", ENTERPRISE_SCHEMA),
    ("nexc-execution", NEXC_EXECUTION_SCHEMA),
    ("data-catalog", DATA_CATALOG_SCHEMA),
    ("code", CODE_SCHEMA),
    ("manufacturing", crate::packs::MANUFACTURING_SCHEMA),
    ("supply-chain", crate::packs::SUPPLY_CHAIN_SCHEMA),
    ("retail", crate::packs::RETAIL_SCHEMA),
    ("logistics", crate::packs::LOGISTICS_SCHEMA),
];

/// The TOML of a built-in package.
pub fn builtin_package(name: &str) -> Result<&'static str> {
    PACKAGES.iter().find(|(n, _)| *n == name).map(|(_, t)| *t).ok_or_else(|| {
        let names: Vec<&str> = PACKAGES.iter().map(|(n, _)| *n).collect();
        Error::Config(format!("unknown built-in schema package '{name}' (available: {})", names.join(", ")))
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropType {
    #[default]
    String,
    Text,
    Int,
    Float,
    Bool,
    Timestamp,
    Enum,
    List,
    Json,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PropertyDef {
    #[serde(rename = "type", default)]
    pub ty: PropType,
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub searchable: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Strings must match this regular expression (anchor it with `^…$`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Strings may hold at most this many characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
}

/// Compiled `pattern`s, shared by every schema.
pub(crate) fn pattern_regex(p: &str) -> std::result::Result<std::sync::Arc<regex_lite::Regex>, String> {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex};
    static CACHE: LazyLock<Mutex<HashMap<String, Arc<regex_lite::Regex>>>> = LazyLock::new(Default::default);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = cache.get(p) {
        return Ok(r.clone());
    }
    let r = Arc::new(regex_lite::Regex::new(p).map_err(|e| format!("invalid pattern {p:?}: {e}"))?);
    if cache.len() < 1024 {
        cache.insert(p.to_string(), r.clone());
    }
    Ok(r)
}

impl PropertyDef {
    /// Does `value` fit this definition? `null` always fits (it means unset).
    pub fn accepts(&self, value: &Value) -> bool {
        if value.is_null() {
            return true;
        }
        if let Some(s) = value.as_str() {
            if self.max_length.is_some_and(|m| s.chars().count() > m) {
                return false;
            }
            if let Some(p) = &self.pattern
                && !pattern_regex(p).is_ok_and(|r| r.is_match(s))
            {
                return false;
            }
        }
        match self.ty {
            PropType::String | PropType::Text => value.is_string(),
            PropType::Enum => value.as_str().is_some_and(|v| self.values.iter().any(|x| x == v)),
            PropType::Int => value.is_i64() || value.is_u64(),
            PropType::Float => value.is_number(),
            PropType::Bool => value.is_boolean(),
            PropType::Timestamp => value.is_i64() || value.is_string(),
            PropType::List => value.is_array(),
            PropType::Json => true,
        }
    }

    /// What [`accepts`](Self::accepts) expects, for error messages.
    pub fn expected(&self) -> String {
        let mut out = if self.ty == PropType::Enum {
            format!("one of {}", self.values.join("|"))
        } else {
            format!("{:?}", self.ty).to_lowercase()
        };
        if let Some(p) = &self.pattern {
            out.push_str(&format!(" matching {p}"));
        }
        if let Some(m) = self.max_length {
            out.push_str(&format!(" of at most {m} characters"));
        }
        out
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    #[default]
    Thing,
    Actor,
    Container,
    Artifact,
    Knowledge,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObjectType {
    #[serde(default)]
    pub name: String,
    /// Display name (the name when empty).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// `#rrggbb` used by maps and editors.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color: String,
    /// Icon name for UIs (free-form; unknown names render as a dot).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub icon: String,
    #[serde(default)]
    pub category: Category,
    /// Parent type (is-a). Properties and link constraints are inherited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, PropertyDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<String>,
    /// States meaning "finished": excluded from open counts and remaining work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminal_states: Vec<String>,
    /// States meaning "something is wrong": candidates in root-cause analysis.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alert_states: Vec<String>,
    /// Interfaces this type implements (polymorphism across types).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implements: Vec<String>,
    /// Computed properties (aggregations over relations), evaluated on read.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub derived: BTreeMap<String, DerivedDef>,
    /// Data-quality rules checked by the quality report.
    #[serde(default, skip_serializing_if = "Constraints::is_empty")]
    pub constraints: Constraints,
    /// The table this type's objects come from (see [`Backing`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backing: Option<Backing>,
}

/// Default for [`Backing::max_rows`].
pub const DEFAULT_MAX_ROWS: usize = 100_000;

/// Where an object type's objects come from: a table of a connected data
/// source (Palantir: the backing datasource). Each row becomes one object
/// with id `{type}:{key}`, where the key is the values of the `key` columns
/// joined with `.`. Only the key and the mapped columns are ever read, and
/// only when `sync` is on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Backing {
    /// The connector (and catalog source) the table belongs to.
    pub source: String,
    /// `schema.table`.
    pub table: String,
    /// Primary key columns, in key order.
    pub key: Vec<String>,
    /// Materialize the table's rows as objects of this type.
    #[serde(default, skip_serializing_if = "is_false")]
    pub sync: bool,
    /// Column used as the object title (the key when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Column mapped to the object state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Property → column.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
    /// Link type (from this type) → foreign key column holding the target's key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub links: BTreeMap<String, String>,
    /// At most this many rows are synced ([`DEFAULT_MAX_ROWS`] when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<usize>,
}

impl Backing {
    pub fn max_rows(&self) -> usize {
        self.max_rows.unwrap_or(DEFAULT_MAX_ROWS)
    }
}

/// A many-to-many link type backed by a join table: each row links the
/// object keyed by column `from` to the object keyed by column `to`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkBacking {
    pub source: String,
    /// `schema.table`.
    pub table: String,
    /// Column holding the key of the link's source object.
    pub from: String,
    /// Column holding the key of the link's target object.
    pub to: String,
}

impl ObjectType {
    /// Layer an extension onto this type: properties, interfaces, derived
    /// properties and constraints are added; non-empty lists replace.
    pub fn merge(&mut self, other: ObjectType) {
        if !other.description.is_empty() {
            self.description = other.description;
        }
        if !other.label.is_empty() {
            self.label = other.label;
        }
        if !other.color.is_empty() {
            self.color = other.color;
        }
        if !other.icon.is_empty() {
            self.icon = other.icon;
        }
        if other.category != Category::Thing {
            self.category = other.category;
        }
        if other.extends.is_some() {
            self.extends = other.extends;
        }
        self.properties.extend(other.properties);
        if !other.states.is_empty() {
            self.states = other.states;
        }
        if !other.terminal_states.is_empty() {
            self.terminal_states = other.terminal_states;
        }
        if !other.alert_states.is_empty() {
            self.alert_states = other.alert_states;
        }
        for i in other.implements {
            if !self.implements.contains(&i) {
                self.implements.push(i);
            }
        }
        self.derived.extend(other.derived);
        self.constraints.merge(&other.constraints);
        if other.backing.is_some() {
            self.backing = other.backing;
        }
    }
}

/// A shape shared by several object types: common properties and relations.
/// `kind = "work_item"` in queries matches every implementing type.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InterfaceType {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Properties every implementer has (inherited when not redeclared).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, PropertyDef>,
    /// Relations every implementer must be allowed to use as the source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
}

/// Business vocabulary: words people use for kinds and relations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Glossary {
    #[serde(default)]
    pub kinds: BTreeMap<String, String>,
    #[serde(default)]
    pub relations: BTreeMap<String, String>,
    /// Named slices for metrics, e.g. `team = "assigned_to.member_of"`.
    #[serde(default)]
    pub dimensions: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    #[default]
    Many,
    /// At most one outgoing link of this type; linking again replaces it.
    One,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinkType {
    #[serde(default)]
    pub name: String,
    /// Display name (the name when empty).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Allowed source kinds; empty means any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from: Vec<String>,
    /// Allowed target kinds; empty means any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse: Option<String>,
    #[serde(default)]
    pub cardinality: Cardinality,
    /// Counts as a dependency edge (critical path, cycles, blockers).
    #[serde(default, skip_serializing_if = "is_false")]
    pub dependency: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub symmetric: bool,
    /// `a -r-> b -r-> c` implies `a -r-> c` (evaluated by the reasoner, never stored).
    #[serde(default, skip_serializing_if = "is_false")]
    pub transitive: bool,
    /// Provenance edge (derived_from, generated_by).
    #[serde(default, skip_serializing_if = "is_false")]
    pub provenance: bool,
    /// Relevance weight used when expanding a context graph (0..1].
    #[serde(default = "default_weight")]
    pub weight: f32,
    /// The join table a many-to-many link type comes from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backing: Option<LinkBacking>,
}

impl Default for LinkType {
    fn default() -> Self {
        Self {
            name: String::new(),
            label: String::new(),
            description: String::new(),
            from: Vec::new(),
            to: Vec::new(),
            inverse: None,
            cardinality: Cardinality::Many,
            dependency: false,
            symmetric: false,
            transitive: false,
            provenance: false,
            weight: default_weight(),
            backing: None,
        }
    }
}

/// A supplied inference rule, in one of two forms.
///
/// Chain form: `chain = ["assigned_to", "member_of"]`, `infer = "handled_by_team"`
/// derives `x handled_by_team z` from `x assigned_to y` and `y member_of z`.
///
/// Horn form: `when` is a list of atoms `[subject, predicate, object]` and
/// `then` one atom; `?name` is a variable. Predicates are `a` (is a kind,
/// interface or derived class), `state`, `prop.<name>`, a relation, or a
/// relation another rule derives. `then` derives a class (`a`) or a relation,
/// never a property value. Rules are assumptions the schema supplies: what they
/// derive is labelled with the rule and never written to the graph.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub infer: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when: Vec<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub then: Vec<Value>,
}

impl Rule {
    /// Written in the Horn form (`when`/`then`) rather than as a chain.
    pub fn is_horn(&self) -> bool {
        !self.when.is_empty() || !self.then.is_empty()
    }

    /// The relation (chain form) or `a <class>` / relation (Horn form) the rule derives.
    pub fn head_predicate(&self) -> Option<&str> {
        if self.is_horn() { self.then.get(1).and_then(Value::as_str) } else { Some(self.infer.as_str()) }
    }

    /// `handled_by_team = assigned_to > member_of`, or
    /// `?s a high_risk <= ?s a supplier, ?s located_in ?c` for the Horn form.
    pub fn describe(&self) -> String {
        if !self.is_horn() {
            return format!("{} = {}", self.infer, self.chain.join(" > "));
        }
        let atom = |a: &[Value]| -> String {
            a.iter()
                .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let body: Vec<String> = self.when.iter().map(|a| atom(a)).collect();
        format!("{} <= {}", atom(&self.then), body.join(", "))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Analysis {
    #[serde(default)]
    pub root_cause: Vec<String>,
    #[serde(default)]
    pub impact: Vec<String>,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub containment: Vec<String>,
    #[serde(default = "default_estimate")]
    pub estimate_property: String,
}

impl Default for Analysis {
    fn default() -> Self {
        Self {
            root_cause: Vec::new(),
            impact: Vec::new(),
            scope: Vec::new(),
            containment: Vec::new(),
            estimate_property: default_estimate(),
        }
    }
}

/// Traversal direction relative to the stored edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    Out,
    In,
    Both,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Schema {
    pub name: String,
    pub version: u32,
    pub object_types: BTreeMap<String, ObjectType>,
    pub link_types: BTreeMap<String, LinkType>,
    pub rules: Vec<Rule>,
    pub analysis: Analysis,
    pub interfaces: BTreeMap<String, InterfaceType>,
    pub glossary: Glossary,
    pub metrics: BTreeMap<String, MetricDef>,
    pub action_types: BTreeMap<String, ActionType>,
    /// Conditions on the objects of a target, by name (see [`Shape`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub shapes: BTreeMap<String, Shape>,
    /// inverse name -> canonical relation
    #[serde(skip)]
    inverses: BTreeMap<String, String>,
}

/// A schema file layer; every section is optional so files can extend each other.
#[derive(Debug, Default, Deserialize)]
struct SchemaFile {
    name: Option<String>,
    version: Option<u32>,
    #[serde(default)]
    object_types: BTreeMap<String, ObjectType>,
    #[serde(default)]
    link_types: BTreeMap<String, LinkType>,
    #[serde(default)]
    rules: Vec<Rule>,
    analysis: Option<Analysis>,
    #[serde(default)]
    interfaces: BTreeMap<String, InterfaceType>,
    glossary: Option<Glossary>,
    #[serde(default)]
    metrics: BTreeMap<String, MetricDef>,
    #[serde(default)]
    action_types: BTreeMap<String, ActionType>,
    #[serde(default)]
    shapes: BTreeMap<String, Shape>,
    /// Package metadata (`[pack]`, see [`crate::packs::PackMeta`]). It
    /// describes the file, never the schema, so it does not change the
    /// schema's name or version.
    #[serde(default)]
    #[allow(dead_code)]
    pack: Option<crate::packs::PackMeta>,
}

impl Schema {
    /// Empty schema: everything is allowed in non-strict mode.
    pub fn empty() -> Self {
        Self { name: "empty".into(), version: 1, ..Default::default() }
    }

    /// The built-in Enterprise Execution ontology.
    pub fn enterprise() -> Self {
        Self::from_toml(ENTERPRISE_SCHEMA).expect("built-in schema is valid")
    }

    /// `builtin:enterprise`, `builtin:empty`, `builtin:<package>`, or a path
    /// to a TOML schema file.
    pub fn load(source: &str) -> Result<Self> {
        match source {
            "builtin:enterprise" | "enterprise" => Ok(Self::enterprise()),
            "builtin:empty" | "empty" => Ok(Self::empty()),
            other => match other.strip_prefix("builtin:") {
                Some(name) => Self::from_toml(builtin_package(name)?),
                None => Self::from_file(other),
            },
        }
    }

    /// Layer a built-in package (`nexc-execution`, ...) or a TOML file.
    pub fn extend_source(&mut self, source: &str, resolve: impl Fn(&str) -> std::path::PathBuf) -> Result<()> {
        match source.strip_prefix("builtin:") {
            Some(name) => self.extend_toml(builtin_package(name)?),
            None => self.extend_file(resolve(source)),
        }
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading schema {}: {e}", path.display())))?;
        Self::from_toml(&text)
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let mut schema = Self::empty();
        schema.extend_toml(text)?;
        Ok(schema)
    }

    /// Layer another schema file on top: types and relations are added or
    /// replaced by name, rules are appended (or replaced by name), shapes are
    /// replaced by name, `[analysis]` replaces when present. A `[pack]` table
    /// is package metadata and leaves the schema untouched.
    pub fn extend_toml(&mut self, text: &str) -> Result<()> {
        let file: SchemaFile = toml::from_str(text)?;
        if let Some(name) = file.name {
            self.name = name;
        }
        if let Some(version) = file.version {
            self.version = version;
        }
        for (name, t) in file.object_types {
            match self.object_types.get_mut(&name) {
                Some(cur) => cur.merge(t),
                None => {
                    self.object_types.insert(name, t);
                }
            }
        }
        self.link_types.extend(file.link_types);
        for rule in file.rules {
            self.rules.retain(|r| r.name != rule.name);
            self.rules.push(rule);
        }
        if let Some(analysis) = file.analysis {
            self.analysis = analysis;
        }
        self.interfaces.extend(file.interfaces);
        if let Some(g) = file.glossary {
            self.glossary.kinds.extend(g.kinds);
            self.glossary.relations.extend(g.relations);
            self.glossary.dimensions.extend(g.dimensions);
        }
        self.metrics.extend(file.metrics);
        self.action_types.extend(file.action_types);
        self.shapes.extend(file.shapes);
        self.compile()
    }

    /// A schema document as JSON (the shape `schema.get` returns and the
    /// live schema API stores), validated.
    pub fn from_json(value: Value) -> Result<Self> {
        let mut schema: Schema =
            serde_json::from_value(value).map_err(|e| Error::Config(format!("schema document: {e}")))?;
        schema.compile()?;
        Ok(schema)
    }

    pub fn extend_file(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading schema {}: {e}", path.display())))?;
        self.extend_toml(&text)
    }

    /// Normalize names and validate every cross-reference. Called after any
    /// edit; the first problem found is returned.
    pub fn compile(&mut self) -> Result<()> {
        for (name, ty) in self.object_types.iter_mut() {
            ty.name = name.clone();
        }
        for (name, iface) in self.interfaces.iter_mut() {
            iface.name = name.clone();
        }
        for (name, m) in self.metrics.iter_mut() {
            m.name = name.clone();
        }
        for (name, a) in self.action_types.iter_mut() {
            a.name = name.clone();
        }
        for (name, shape) in self.shapes.iter_mut() {
            shape.normalize(name);
        }
        for name in self.interfaces.keys() {
            if self.object_types.contains_key(name) {
                return Err(Error::Config(format!("interface {name} collides with an object type")));
            }
        }
        self.inverses.clear();
        for (name, lt) in self.link_types.iter_mut() {
            lt.name = name.clone();
            if !(0.0..=1.0).contains(&lt.weight) || lt.weight == 0.0 {
                return Err(Error::Config(format!("link type {name}: weight must be in (0, 1]")));
            }
        }
        for (name, lt) in &self.link_types {
            for kind in lt.from.iter().chain(&lt.to) {
                if !self.is_type_or_interface(kind) {
                    return Err(Error::Config(format!("link type {name} references unknown type {kind}")));
                }
            }
            if lt.transitive && !lt.from.is_empty() && !lt.to.is_empty() && !self.kinds_overlap(&lt.from, &lt.to) {
                return Err(Error::Config(format!(
                    "link type {name} is transitive but its targets [{}] can never be its sources [{}]",
                    lt.to.join(", "),
                    lt.from.join(", ")
                )));
            }
            if let Some(inv) = &lt.inverse {
                if self.link_types.contains_key(inv) {
                    return Err(Error::Config(format!("inverse {inv} of {name} collides with a link type")));
                }
                if let Some(prev) = self.inverses.insert(inv.clone(), name.clone()) {
                    return Err(Error::Config(format!("inverse {inv} declared by both {prev} and {name}")));
                }
            }
        }
        for (name, ty) in &self.object_types {
            if let Some(parent) = &ty.extends
                && !self.object_types.contains_key(parent)
            {
                return Err(Error::Config(format!("type {name} extends unknown type {parent}")));
            }
        }
        let vocab = crate::reason::vocabulary(self);
        for rule in &self.rules {
            if rule.is_horn() {
                if !rule.chain.is_empty() || !rule.infer.is_empty() {
                    return Err(Error::Config(format!(
                        "rule {}: give either chain/infer or when/then, not both",
                        rule.name
                    )));
                }
                // A term the reasoner cannot spell (a head relation named like a
                // keyword) is not a schema error: the reasoner refuses the rule
                // and reports it, as it does for a chain rule's `infer`.
                if let Err(crate::reason::CompileError::Invalid(e)) = crate::reason::compile_rule(self, &vocab, rule) {
                    return Err(Error::Config(format!("rule {}: {e}", rule.name)));
                }
                continue;
            }
            if rule.chain.is_empty() {
                return Err(Error::Config(format!("rule {} has an empty chain", rule.name)));
            }
            if rule.infer.is_empty() {
                return Err(Error::Config(format!("rule {}: a chain needs infer (the derived relation)", rule.name)));
            }
            for step in &rule.chain {
                // A step may name a relation another rule derives.
                let derived = vocab.relations.contains(step.strip_prefix('^').unwrap_or(step));
                if self.resolve_rel(step).is_none() && !derived {
                    return Err(Error::Config(format!("rule {} uses unknown relation {step}", rule.name)));
                }
            }
        }
        for (name, shape) in &self.shapes {
            shape
                .validate(self, &vocab.classes, &vocab.relations)
                .map_err(|e| Error::Config(format!("shape {name}: {e}")))?;
        }
        self.check_interfaces()?;
        for (word, kind) in &self.glossary.kinds {
            if !self.is_type_or_interface(kind) {
                return Err(Error::Config(format!("glossary maps '{word}' to unknown type {kind}")));
            }
        }
        for (word, rel) in &self.glossary.relations {
            if self.resolve_rel(rel).is_none() {
                return Err(Error::Config(format!("glossary maps '{word}' to unknown relation {rel}")));
            }
        }
        for ty in self.object_types.values() {
            for (prop, def) in &ty.properties {
                if let Some(p) = &def.pattern {
                    pattern_regex(p).map_err(|e| Error::Config(format!("{}.{prop}: {e}", ty.name)))?;
                }
            }
            for (prop, d) in &ty.derived {
                d.validate(self).map_err(|e| Error::Config(format!("{}.derived.{prop}: {e}", ty.name)))?;
            }
            for rel in &ty.constraints.required_links {
                if self.resolve_rel(rel).is_none() {
                    return Err(Error::Config(format!("{}.constraints: unknown relation {rel}", ty.name)));
                }
            }
        }
        for m in self.metrics.values() {
            m.validate(self).map_err(|e| Error::Config(format!("metric {}: {e}", m.name)))?;
        }
        for a in self.action_types.values() {
            a.validate(self).map_err(|e| Error::Config(format!("action type {}: {e}", a.name)))?;
        }
        self.check_backings()
    }

    /// Backed properties must be properties of the type, backed links must
    /// be link types the type may use as the source.
    fn check_backings(&self) -> Result<()> {
        for ty in self.object_types.values() {
            let Some(b) = &ty.backing else { continue };
            let what = format!("{}.backing", ty.name);
            if b.source.is_empty() || b.table.is_empty() {
                return Err(Error::Config(format!("{what}: needs a source and a table")));
            }
            if b.key.is_empty() || b.key.iter().any(|k| k.is_empty()) {
                return Err(Error::Config(format!("{what}: needs at least one key column")));
            }
            for prop in b.properties.keys() {
                if self.prop_def(&ty.name, prop).is_none() {
                    return Err(Error::Config(format!("{what}: {prop} is not a property of {}", ty.name)));
                }
            }
            for rel in b.links.keys() {
                let Some(lt) = self.link_types.get(rel) else {
                    return Err(Error::Config(format!("{what}: unknown link type {rel}")));
                };
                if !lt.from.is_empty() && !lt.from.iter().any(|k| self.kind_is(&ty.name, k)) {
                    return Err(Error::Config(format!("{what}: {} may not use link type {rel}", ty.name)));
                }
            }
        }
        for lt in self.link_types.values() {
            if let Some(b) = &lt.backing
                && [&b.source, &b.table, &b.from, &b.to].iter().any(|v| v.is_empty())
            {
                return Err(Error::Config(format!("link type {}.backing: needs source, table, from and to", lt.name)));
            }
        }
        Ok(())
    }

    fn check_interfaces(&self) -> Result<()> {
        for ty in self.object_types.values() {
            for iname in &ty.implements {
                let Some(iface) = self.interfaces.get(iname) else {
                    return Err(Error::Config(format!("type {} implements unknown interface {iname}", ty.name)));
                };
                for (prop, def) in &iface.properties {
                    if let Some(own) = self.own_prop_def(&ty.name, prop)
                        && own.ty != def.ty
                    {
                        return Err(Error::Config(format!(
                            "type {} declares {prop} as {:?} but interface {iname} requires {:?}",
                            ty.name, own.ty, def.ty
                        )));
                    }
                }
                for rel in &iface.links {
                    let Some((base, _)) = self.resolve_rel(rel) else {
                        return Err(Error::Config(format!("interface {iname} requires unknown relation {rel}")));
                    };
                    let lt = &self.link_types[&base];
                    if !lt.from.is_empty() && !lt.from.iter().any(|k| self.kind_is(&ty.name, k)) {
                        return Err(Error::Config(format!(
                            "type {} implements {iname} but may not use relation {base}",
                            ty.name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Can some object be both of a kind in `a` and of a kind in `b`?
    fn kinds_overlap(&self, a: &[String], b: &[String]) -> bool {
        a.iter().any(|x| {
            b.iter().any(|y| {
                x == y
                    || self.kind_is(x, y)
                    || self.kind_is(y, x)
                    || self.object_types.keys().any(|k| self.kind_is(k, x) && self.kind_is(k, y))
            })
        })
    }

    pub fn is_interface(&self, name: &str) -> bool {
        self.interfaces.contains_key(name)
    }

    pub fn is_type_or_interface(&self, name: &str) -> bool {
        self.object_types.contains_key(name) || self.interfaces.contains_key(name)
    }

    /// Map a word from a question to a kind or interface: exact names,
    /// glossary synonyms and simple plurals ("tickets" → issue, "people" → person).
    pub fn canonical_kind(&self, word: &str) -> Option<String> {
        let w = word.trim().to_lowercase().replace([' ', '-'], "_");
        let lookup = |c: &str| -> Option<String> {
            if self.is_type_or_interface(c) {
                return Some(c.to_string());
            }
            self.glossary.kinds.get(c).cloned()
        };
        if let Some(k) = lookup(&w) {
            return Some(k);
        }
        let mut stems = Vec::new();
        if let Some(s) = w.strip_suffix("ies") {
            stems.push(format!("{s}y"));
        }
        if let Some(s) = w.strip_suffix("es") {
            stems.push(s.to_string());
        }
        if let Some(s) = w.strip_suffix('s') {
            stems.push(s.to_string());
        }
        stems.iter().find_map(|s| lookup(s))
    }

    /// Map a word to a relation via names, inverses and the glossary.
    pub fn canonical_rel(&self, word: &str) -> Option<String> {
        let w = word.trim().to_lowercase();
        if self.resolve_rel(&w).is_some() {
            return Some(w);
        }
        self.glossary.relations.get(&w).or_else(|| self.glossary.relations.get(w.trim_end_matches('s'))).cloned()
    }

    /// Concrete object types matching a kind or interface name.
    pub fn concrete_kinds(&self, name: &str) -> Vec<String> {
        self.object_types.keys().filter(|k| self.kind_is(k, name)).cloned().collect()
    }

    pub fn object_type(&self, kind: &str) -> Option<&ObjectType> {
        self.object_types.get(kind)
    }

    pub fn link_type(&self, rel: &str) -> Option<&LinkType> {
        self.link_types.get(rel)
    }

    /// Resolve a relation name to (canonical relation, direction).
    /// Accepts canonical names, declared inverse names and `^rel`.
    pub fn resolve_rel(&self, name: &str) -> Option<(String, Dir)> {
        if let Some(base) = name.strip_prefix('^') {
            return self
                .link_types
                .get(base)
                .map(|lt| (base.to_string(), if lt.symmetric { Dir::Both } else { Dir::In }));
        }
        if let Some(lt) = self.link_types.get(name) {
            return Some((name.to_string(), if lt.symmetric { Dir::Both } else { Dir::Out }));
        }
        self.inverses.get(name).map(|base| (base.clone(), Dir::In))
    }

    /// Like [`resolve_rel`](Self::resolve_rel) but undeclared names are taken
    /// literally, so non-strict graphs with ad-hoc relations stay traversable.
    pub fn resolve_rel_lenient(&self, name: &str) -> (String, Dir) {
        self.resolve_rel(name).unwrap_or_else(|| match name.strip_prefix('^') {
            Some(base) => (base.to_string(), Dir::In),
            None => (name.to_string(), Dir::Out),
        })
    }

    /// Name of a relation as seen when walking it in `dir`.
    pub fn display_rel(&self, base: &str, dir: Dir) -> String {
        match dir {
            Dir::Out | Dir::Both => base.to_string(),
            Dir::In => match self.link_types.get(base) {
                Some(lt) if lt.symmetric => base.to_string(),
                Some(LinkType { inverse: Some(inv), .. }) => inv.clone(),
                _ => format!("^{base}"),
            },
        }
    }

    pub fn weight(&self, base: &str) -> f32 {
        self.link_types.get(base).map(|lt| lt.weight).unwrap_or(0.5)
    }

    pub fn is_dependency(&self, base: &str) -> bool {
        self.link_types.get(base).is_some_and(|lt| lt.dependency)
    }

    pub fn dependency_rels(&self) -> Vec<String> {
        self.link_types.values().filter(|lt| lt.dependency).map(|lt| lt.name.clone()).collect()
    }

    /// Is `kind` the same as, a subtype of, or an implementer of `ancestor`
    /// (an object type or an interface)?
    pub fn kind_is(&self, kind: &str, ancestor: &str) -> bool {
        let mut cur = Some(kind);
        let mut hops = 0;
        while let Some(k) = cur {
            if k == ancestor {
                return true;
            }
            let ty = self.object_types.get(k);
            if ty.is_some_and(|t| t.implements.iter().any(|i| i == ancestor)) {
                return true;
            }
            hops += 1;
            if hops > 16 {
                break;
            }
            cur = ty.and_then(|t| t.extends.as_deref());
        }
        false
    }

    /// Property definition from the type, its ancestors, then its interfaces.
    pub fn prop_def(&self, kind: &str, prop: &str) -> Option<&PropertyDef> {
        self.own_prop_def(kind, prop).or_else(|| {
            self.type_chain(kind)
                .iter()
                .flat_map(|t| t.implements.iter())
                .filter_map(|i| self.interfaces.get(i))
                .find_map(|i| i.properties.get(prop))
        })
    }

    fn own_prop_def(&self, kind: &str, prop: &str) -> Option<&PropertyDef> {
        self.type_chain(kind).into_iter().find_map(|t| t.properties.get(prop))
    }

    /// The type followed by its `extends` ancestors.
    pub fn type_chain(&self, kind: &str) -> Vec<&ObjectType> {
        let mut out = Vec::new();
        let mut cur = self.object_types.get(kind);
        while let Some(t) = cur {
            if out.len() > 16 {
                break;
            }
            out.push(t);
            cur = t.extends.as_deref().and_then(|p| self.object_types.get(p));
        }
        out
    }

    /// Derived property definitions for a kind (including ancestors).
    pub fn derived_defs(&self, kind: &str) -> Vec<(&String, &DerivedDef)> {
        let mut seen = std::collections::BTreeSet::new();
        self.type_chain(kind).into_iter().flat_map(|t| t.derived.iter()).filter(|(k, _)| seen.insert(*k)).collect()
    }

    /// Constraints for a kind, merged over ancestors.
    pub fn constraints(&self, kind: &str) -> Constraints {
        let mut c = Constraints::default();
        for t in self.type_chain(kind).into_iter().rev() {
            c.merge(&t.constraints);
        }
        c
    }

    pub fn category(&self, kind: &str) -> Category {
        self.object_types.get(kind).map(|t| t.category).unwrap_or_default()
    }

    /// Not in a terminal state (no state counts as open).
    pub fn is_open(&self, kind: &str, state: Option<&str>) -> bool {
        match (self.object_types.get(kind), state) {
            (Some(ty), Some(s)) => !ty.terminal_states.iter().any(|t| t == s),
            _ => true,
        }
    }

    pub fn is_alert(&self, kind: &str, state: Option<&str>) -> bool {
        match (self.object_types.get(kind), state) {
            (Some(ty), Some(s)) => ty.alert_states.iter().any(|t| t == s),
            _ => false,
        }
    }

    pub fn validate_entity(
        &self,
        kind: &str,
        state: Option<&str>,
        props: &Map<String, Value>,
        creating: bool,
        strict: bool,
    ) -> Result<()> {
        let Some(ty) = self.object_types.get(kind) else {
            return if strict {
                Err(Error::invalid(format!("unknown entity kind '{kind}' (strict schema)")))
            } else {
                Ok(())
            };
        };
        if let Some(s) = state
            && !ty.states.is_empty()
            && !ty.states.iter().any(|x| x == s)
        {
            return Err(Error::invalid(format!(
                "state '{s}' is not valid for {kind}; expected one of: {}",
                ty.states.join(", ")
            )));
        }
        for (name, value) in props {
            if value.is_null() {
                continue;
            }
            let Some(def) = self.prop_def(kind, name) else {
                if strict {
                    return Err(Error::invalid(format!("unknown property {kind}.{name} (strict schema)")));
                }
                continue;
            };
            if !def.accepts(value) {
                return Err(Error::invalid(format!("{kind}.{name} must be {}, got {value}", def.expected())));
            }
        }
        if creating {
            for (name, def) in self.all_props(kind) {
                if def.required && props.get(name).is_none_or(Value::is_null) {
                    return Err(Error::invalid(format!("{kind}.{name} is required")));
                }
            }
        }
        let _ = ty;
        Ok(())
    }

    /// Every property definition that applies to a kind (type, ancestors, interfaces).
    pub fn all_props(&self, kind: &str) -> Vec<(&String, &PropertyDef)> {
        let mut seen = std::collections::BTreeSet::new();
        let chain = self.type_chain(kind);
        let own = chain.iter().flat_map(|t| t.properties.iter());
        let ifaces = chain
            .iter()
            .flat_map(|t| t.implements.iter())
            .filter_map(|i| self.interfaces.get(i))
            .flat_map(|i| i.properties.iter());
        own.chain(ifaces).filter(|(k, _)| seen.insert(*k)).collect()
    }

    pub fn validate_link(&self, from_kind: &str, base: &str, to_kind: &str, strict: bool) -> Result<()> {
        let Some(lt) = self.link_types.get(base) else {
            return if strict {
                Err(Error::invalid(format!("unknown relation '{base}' (strict schema)")))
            } else {
                Ok(())
            };
        };
        let allowed = |list: &[String], kind: &str| list.is_empty() || list.iter().any(|k| self.kind_is(kind, k));
        let fits = allowed(&lt.from, from_kind) && allowed(&lt.to, to_kind);
        let fits_reversed = lt.symmetric && allowed(&lt.from, to_kind) && allowed(&lt.to, from_kind);
        if fits || fits_reversed {
            Ok(())
        } else {
            Err(Error::invalid(format!(
                "{from_kind} -{base}-> {to_kind} is not allowed (from: [{}], to: [{}])",
                lt.from.join(", "),
                lt.to.join(", ")
            )))
        }
    }

    /// Compact, stable description for LLM system prompts. Stable text keeps
    /// provider-side prompt caches warm across requests.
    pub fn summary(&self) -> String {
        let mut out = String::from("Types: ");
        let types: Vec<String> = self
            .object_types
            .values()
            .map(|t| if t.states.is_empty() { t.name.clone() } else { format!("{}[{}]", t.name, t.states.join("|")) })
            .collect();
        out.push_str(&types.join(", "));
        out.push_str("\nRelations (inverse in parentheses): ");
        let rels: Vec<String> = self
            .link_types
            .values()
            .map(|lt| match &lt.inverse {
                Some(inv) => format!("{}({inv})", lt.name),
                None => lt.name.clone(),
            })
            .collect();
        out.push_str(&rels.join(", "));
        if !self.rules.is_empty() {
            out.push_str("\nDerived: ");
            let rules: Vec<String> = self
                .rules
                .iter()
                .map(|r| if r.is_horn() { r.describe() } else { format!("{}={}", r.infer, r.chain.join(">")) })
                .collect();
            out.push_str(&rules.join(", "));
        }
        out
    }

    /// Human-readable schema listing for `onto schema`.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{} v{}", self.name, self.version);
        let _ = writeln!(out, "\nObject types:");
        for t in self.object_types.values() {
            let _ = write!(out, "  {:<13} {:?}", t.name, t.category);
            if !t.states.is_empty() {
                let _ = write!(out, "  states: {}", t.states.join(" → "));
            }
            if let Some(b) = &t.backing {
                let _ = write!(out, "  backed by {}:{}", b.source, b.table);
                if b.sync {
                    let _ = write!(out, " (synced)");
                }
            }
            let _ = writeln!(out);
        }
        let _ = writeln!(out, "\nLink types:");
        for lt in self.link_types.values() {
            let side = |v: &[String]| if v.is_empty() { "*".to_string() } else { v.join("|") };
            let _ = write!(out, "  {} -{}-> {}", side(&lt.from), lt.name, side(&lt.to));
            if let Some(inv) = &lt.inverse {
                let _ = write!(out, "   (inverse: {inv})");
            }
            if lt.dependency {
                let _ = write!(out, "   [dependency]");
            }
            if lt.transitive {
                let _ = write!(out, "   [transitive]");
            }
            if lt.symmetric {
                let _ = write!(out, "   [symmetric]");
            }
            if let Some(b) = &lt.backing {
                let _ = write!(out, "   backed by {}:{}", b.source, b.table);
            }
            let _ = writeln!(out);
        }
        if !self.rules.is_empty() {
            let _ = writeln!(out, "\nRules:");
            for r in &self.rules {
                let _ = writeln!(out, "  {}", r.describe());
            }
        }
        if !self.interfaces.is_empty() {
            let _ = writeln!(out, "\nInterfaces:");
            for i in self.interfaces.values() {
                let _ = writeln!(out, "  {:<15} implemented by {}", i.name, self.concrete_kinds(&i.name).join(", "));
            }
        }
        if !self.action_types.is_empty() {
            let _ = writeln!(out, "\nAction types:");
            for a in self.action_types.values() {
                let params: Vec<String> = a.parameters.keys().cloned().collect();
                let _ = writeln!(out, "  {:<15} ({}) risk {:?}", a.name, params.join(", "), a.risk);
            }
        }
        if !self.shapes.is_empty() {
            let _ = writeln!(out, "\nShapes:");
            for (name, sh) in &self.shapes {
                let target = sh.target.as_deref().unwrap_or("(ids or relation)");
                let _ = writeln!(out, "  {name:<24} {target}, {} property shapes", sh.properties.len());
            }
        }
        if !self.metrics.is_empty() {
            let _ = writeln!(out, "\nMetrics:");
            for m in self.metrics.values() {
                let _ = writeln!(out, "  {:<24} {:?} {}", m.name, m.measure, m.kind.as_deref().unwrap_or(""));
            }
        }
        out
    }
}

fn default_weight() -> f32 {
    1.0
}

fn default_estimate() -> String {
    "estimate".into()
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtin_schema_loads_and_resolves_inverses() {
        let s = Schema::enterprise();
        assert!(s.object_types.contains_key("issue"));
        assert_eq!(s.resolve_rel("owned_by"), Some(("owns".into(), Dir::In)));
        assert_eq!(s.resolve_rel("^owns"), Some(("owns".into(), Dir::In)));
        assert_eq!(s.resolve_rel("owns"), Some(("owns".into(), Dir::Out)));
        assert_eq!(s.resolve_rel("relates_to"), Some(("relates_to".into(), Dir::Both)));
        assert_eq!(s.display_rel("assigned_to", Dir::In), "works_on");
        assert!(s.is_dependency("blocked_by"));
        assert!(!s.is_open("task", Some("done")));
        assert!(s.is_alert("deployment", Some("failed")));
    }

    #[test]
    fn validates_states_props_and_links() {
        let s = Schema::enterprise();
        let mut props = Map::new();
        props.insert("priority".into(), json!("urgent"));
        assert!(s.validate_entity("issue", Some("open"), &props, true, false).is_err());
        props.insert("priority".into(), json!("high"));
        assert!(s.validate_entity("issue", Some("open"), &props, true, false).is_ok());
        assert!(s.validate_entity("issue", Some("sleeping"), &Map::new(), true, false).is_err());
        assert!(s.validate_entity("spaceship", None, &Map::new(), true, false).is_ok());
        assert!(s.validate_entity("spaceship", None, &Map::new(), true, true).is_err());
        assert!(s.validate_link("issue", "assigned_to", "person", false).is_ok());
        assert!(s.validate_link("person", "assigned_to", "issue", false).is_err());
    }

    #[test]
    fn extension_layers_add_types_and_relations() {
        let mut s = Schema::enterprise();
        s.extend_toml(
            r#"
            [object_types.incident]
            extends = "issue"
            properties.sev = { type = "int" }
            [link_types.pages]
            from = ["incident"]
            to = ["person"]
            inverse = "paged_by"
            "#,
        )
        .unwrap();
        assert!(s.kind_is("incident", "issue"));
        assert!(s.prop_def("incident", "priority").is_some());
        assert!(s.validate_link("incident", "assigned_to", "person", true).is_ok());
        assert_eq!(s.resolve_rel("paged_by"), Some(("pages".into(), Dir::In)));
    }

    #[test]
    fn interfaces_glossary_and_extensions_validate() {
        let s = Schema::enterprise();
        assert!(s.kind_is("task", "work_item"));
        assert!(s.kind_is("issue", "work_item"));
        assert!(!s.kind_is("person", "work_item"));
        assert!(s.prop_def("task", "priority").is_some(), "interface properties are inherited");
        assert_eq!(s.canonical_kind("tickets").as_deref(), Some("issue"));
        assert_eq!(s.canonical_kind("People").as_deref(), Some("person"));
        assert_eq!(s.canonical_kind("work items").as_deref(), Some("work_item"));
        assert_eq!(s.canonical_rel("owner").as_deref(), Some("owned_by"));
        assert!(s.concrete_kinds("work_item").contains(&"milestone".to_string()));
        let bad = Schema::from_toml("[object_types.a]\nimplements = ['nope']\n").unwrap_err();
        assert!(bad.to_string().contains("unknown interface"));
        let mut s2 = Schema::enterprise();
        let err = s2
            .extend_toml("[object_types.bug]\nimplements = ['work_item']\nproperties.priority = { type = 'int' }\n")
            .unwrap_err();
        assert!(err.to_string().contains("interface work_item requires"));
    }

    #[test]
    fn backings_round_trip_merge_and_validate() {
        let text = r#"
            [object_types.order]
            properties.amount = { type = "float" }
            states = ["placed", "paid"]
            backing = { source = "wh", table = "sales.orders", key = ["id"], sync = true, state = "status", properties = { amount = "amount" }, links = { customer = "customer_id" } }
            [object_types.customer]
            backing = { source = "wh", table = "sales.customers", key = ["id"] }
            [object_types.tag]
            [link_types.customer]
            from = ["order"]
            to = ["customer"]
            inverse = "orders"
            cardinality = "one"
            [link_types.tags]
            from = ["order"]
            to = ["tag"]
            backing = { source = "wh", table = "sales.order_tags", from = "order_id", to = "tag_id" }
        "#;
        let s = Schema::from_toml(text).unwrap();
        let b = s.object_types["order"].backing.clone().unwrap();
        assert!(b.sync);
        assert_eq!(b.max_rows(), DEFAULT_MAX_ROWS);
        assert_eq!(b.links["customer"], "customer_id");
        assert_eq!(s.link_types["tags"].backing.as_ref().unwrap().to, "tag_id");
        assert!(s.describe().contains("backed by wh:sales.orders (synced)"));

        // JSON round trip (the live schema stores documents as JSON).
        let doc = serde_json::to_value(&s).unwrap();
        assert_eq!(
            doc["object_types"]["customer"]["backing"],
            json!({ "source": "wh", "table": "sales.customers", "key": ["id"] })
        );
        assert!(doc["object_types"]["tag"].get("backing").is_none(), "unbacked types stay as they were");
        let back = Schema::from_json(doc).unwrap();
        assert_eq!(back.object_types["order"].backing, Some(b.clone()));

        // A later layer replaces the backing; one without keeps it.
        let mut layered = s.clone();
        layered.extend_toml("[object_types.order]\ndescription = \"Orders\"\n").unwrap();
        assert_eq!(layered.object_types["order"].backing, Some(b));
        layered
            .extend_toml("[object_types.order]\nbacking = { source = \"wh2\", table = \"s.o\", key = [\"id\"] }\n")
            .unwrap();
        assert_eq!(layered.object_types["order"].backing.as_ref().unwrap().source, "wh2");

        let broken = |edit: &dyn Fn(&mut Backing)| {
            let mut s = Schema::from_toml(text).unwrap();
            edit(s.object_types.get_mut("order").unwrap().backing.as_mut().unwrap());
            s.compile().unwrap_err().to_string()
        };
        assert!(
            broken(&|b| {
                b.properties.insert("nope".into(), "x".into());
            })
            .contains("nope is not a property")
        );
        assert!(
            broken(&|b| {
                b.links.insert("tags_of".into(), "x".into());
            })
            .contains("unknown link type")
        );
        assert!(broken(&|b| b.key.clear()).contains("key column"));
        let mut wrong_side = Schema::from_toml(text).unwrap();
        wrong_side
            .object_types
            .get_mut("customer")
            .unwrap()
            .backing
            .as_mut()
            .unwrap()
            .links
            .insert("customer".into(), "x".into());
        assert!(wrong_side.compile().unwrap_err().to_string().contains("may not use link type customer"));
    }

    #[test]
    fn rejects_bad_rules() {
        let err = Schema::from_toml("[[rules]]\nname='x'\nchain=['nope']\ninfer='y'").unwrap_err();
        assert!(err.to_string().contains("unknown relation"));
    }

    fn with_rule(rule: &str) -> Result<Schema> {
        let mut s = Schema::enterprise();
        s.extend_toml(&format!("[[rules]]\n{rule}"))?;
        Ok(s)
    }

    #[test]
    fn horn_rules_validate() {
        let s = with_rule(
            r#"name = "urgent"
            description = "Triaged high-priority issues are urgent."
            when = [["?i", "a", "issue"], ["?i", "state", "triaged"], ["?i", "prop.priority", "high"], ["?i", "owned_by", "?t"]]
            then = ["?i", "a", "urgent_issue"]"#,
        )
        .unwrap();
        let r = s.rules.iter().find(|r| r.name == "urgent").unwrap();
        assert!(r.is_horn());
        assert!(r.describe().starts_with("?i a urgent_issue <= ?i a issue"), "{}", r.describe());
        // A later rule may use the derived class and the chain rules' relations.
        let mut s2 = s.clone();
        s2.extend_toml(
            "[[rules]]\nname = 'on_call'\nwhen = [['?i', 'a', 'urgent_issue'], ['?i', 'handled_by_team', '?t']]\nthen = ['?t', 'a', 'on_call_team']\n",
        )
        .unwrap();
        // Chain steps may name a derived relation.
        s2.extend_toml("[[rules]]\nname = 'team_scope'\nchain = ['handled_by_team', 'owns']\ninfer = 'team_scope'\n")
            .unwrap();
        // JSON round trip keeps the Horn form and leaves chain rules as they were.
        let doc = serde_json::to_value(&s2).unwrap();
        let chain = doc["rules"].as_array().unwrap().iter().find(|r| r["name"] == "responsible_team").unwrap();
        assert!(chain.get("when").is_none() && chain["chain"].is_array());
        Schema::from_json(doc).unwrap();

        let err = |rule: &str| with_rule(rule).unwrap_err().to_string();
        let e = err("name = 'r'\nwhen = [['?x', 'located_in', '?c']]\nthen = ['?x', 'a', 'risky']");
        assert!(e.contains("rule r:") && e.contains("unknown relation located_in"), "{e}");
        let e = err("name = 'r'\nwhen = [['?x', 'a', 'issue']]\nthen = ['?x', 'handled_by_team', '?y']");
        assert!(e.contains("head variable ?y does not occur in when"), "{e}");
        let e = err("name = 'r'\nwhen = [['?x', 'a', 'issue']]\nthen = ['?x', 'state', 'urgent']");
        assert!(e.contains("rules derive types and relations, not property values"), "{e}");
        let e = err("name = 'r'\nwhen = [['?x', 'a', 'spaceship']]\nthen = ['?x', 'a', 'risky']");
        assert!(e.contains("unknown class spaceship"), "{e}");
        let e = err("name = 'r'\nwhen = [['?x', 'prop.warp', 9]]\nthen = ['?x', 'a', 'risky']");
        assert!(e.contains("unknown property warp"), "{e}");
        let e =
            err("name = 'r'\nchain = ['owns']\ninfer = 'x'\nwhen = [['?x', 'a', 'issue']]\nthen = ['?x', 'a', 'y']");
        assert!(e.contains("not both"), "{e}");
        let e = err("name = 'r'\nwhen = [['?x', 'a', 'issue']]\nthen = ['?x', 'a', 'Bad Class']");
        assert!(e.contains("must match"), "{e}");
        let e = err("name = 'r'\nwhen = [['?x', 'a']]\nthen = ['?x', 'a', 'y']");
        assert!(e.contains("three parts"), "{e}");
        let e = err("name = 'r'\nchain = ['owns']");
        assert!(e.contains("needs infer"), "{e}");
    }

    #[test]
    fn transitive_link_types_must_chain() {
        let s = Schema::from_toml(
            "[object_types.a]\n[object_types.b]\nextends = 'a'\n[link_types.r]\nfrom = ['a']\nto = ['b']\ntransitive = true\n",
        )
        .unwrap();
        assert!(s.link_types["r"].transitive);
        assert!(s.describe().contains("[transitive]"));
        let err = Schema::from_toml(
            "[object_types.a]\n[object_types.b]\n[link_types.r]\nfrom = ['a']\nto = ['b']\ntransitive = true\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("is transitive"), "{err}");
        // Not transitive: not serialized, so existing documents keep their bytes.
        let doc = serde_json::to_value(Schema::enterprise()).unwrap();
        assert!(doc["link_types"]["owns"].get("transitive").is_none());
        assert!(doc.get("shapes").is_none());
    }

    #[test]
    fn pack_tables_leave_name_and_version_alone() {
        let mut s = Schema::enterprise();
        let (name, version) = (s.name.clone(), s.version);
        s.extend_toml(
            "[pack]\nid = 'supply'\ntitle = 'Supply chain'\nversion = '1.2.0'\nname = 'other'\n[object_types.supplier]\n",
        )
        .unwrap();
        assert_eq!((s.name.as_str(), s.version), (name.as_str(), version));
        assert!(s.object_types.contains_key("supplier"));
    }

    #[test]
    fn shapes_merge_and_validate() {
        let mut s = Schema::enterprise();
        s.extend_toml(
            r#"
            [shapes.issue_shape]
            target = "issue"
            message = "Issues need an owner project."
            properties = [
                { path = "belongs_to", min_count = 1, class = "project" },
                { path = "prop.priority", in = ["low", "high"], id = "prio" },
                { path = "assigned_to.member_of", max_count = 3, severity = "warning" },
            ]
            "#,
        )
        .unwrap();
        let sh = &s.shapes["issue_shape"];
        assert_eq!(sh.properties[0].id, "issue_shape/0");
        assert_eq!(sh.properties[1].id, "prio");
        assert_eq!(sh.properties[2].severity, Some(crate::shape_model::ShapeSeverity::Warning));
        // A later layer replaces a shape by name.
        s.extend_toml("[shapes.issue_shape]\ntarget = 'task'\n").unwrap();
        assert!(s.shapes["issue_shape"].properties.is_empty());
        let back = Schema::from_json(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back.shapes, s.shapes);

        let bad = |text: &str| Schema::enterprise().extend_toml(text).unwrap_err().to_string();
        assert!(bad("[shapes.x]\ntarget = 'spaceship'\n").contains("shape x: unknown target spaceship"));
        assert!(bad("[shapes.x]\ntarget_subjects_of = 'nope'\n").contains("unknown relation nope"));
        assert!(bad("[shapes.x]\nproperties = [{ path = 'owns.nope' }]\n").contains("unknown relation nope"));
        assert!(bad("[shapes.x]\nproperties = [{ path = 'prop.nope' }]\n").contains("unknown property nope"));
        assert!(bad("[shapes.x]\nproperties = [{ path = 'owns', node = 'y' }]\n").contains("unknown shape y"));
        assert!(bad("[shapes.x]\nproperties = [{ path = 'title', pattern = '(' }]\n").contains("invalid pattern"));
    }
}
