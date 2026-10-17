//! Dynamic ontology schema: object types, link types, inference rules and
//! analysis settings, loaded from TOML at runtime and merged in layers.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{Error, Result};

/// The built-in Enterprise Execution ontology.
pub const ENTERPRISE_SCHEMA: &str = include_str!("../schemas/enterprise.toml");

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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
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
    /// Provenance edge (derived_from, generated_by).
    #[serde(default, skip_serializing_if = "is_false")]
    pub provenance: bool,
    /// Relevance weight used when expanding a context graph (0..1].
    #[serde(default = "default_weight")]
    pub weight: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub chain: Vec<String>,
    pub infer: String,
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

    /// `builtin:enterprise`, `builtin:empty`, or a path to a TOML schema file.
    pub fn load(source: &str) -> Result<Self> {
        match source {
            "builtin:enterprise" | "enterprise" => Ok(Self::enterprise()),
            "builtin:empty" | "empty" => Ok(Self::empty()),
            path => Self::from_file(path),
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
    /// replaced by name, rules are appended, `[analysis]` replaces when present.
    pub fn extend_toml(&mut self, text: &str) -> Result<()> {
        let file: SchemaFile = toml::from_str(text)?;
        if let Some(name) = file.name {
            self.name = name;
        }
        if let Some(version) = file.version {
            self.version = version;
        }
        self.object_types.extend(file.object_types);
        self.link_types.extend(file.link_types);
        for rule in file.rules {
            self.rules.retain(|r| r.name != rule.name);
            self.rules.push(rule);
        }
        if let Some(analysis) = file.analysis {
            self.analysis = analysis;
        }
        self.finish()
    }

    pub fn extend_file(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("reading schema {}: {e}", path.display())))?;
        self.extend_toml(&text)
    }

    fn finish(&mut self) -> Result<()> {
        for (name, ty) in self.object_types.iter_mut() {
            ty.name = name.clone();
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
                if !self.object_types.contains_key(kind) {
                    return Err(Error::Config(format!("link type {name} references unknown type {kind}")));
                }
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
        for rule in &self.rules {
            if rule.chain.is_empty() {
                return Err(Error::Config(format!("rule {} has an empty chain", rule.name)));
            }
            for step in &rule.chain {
                if self.resolve_rel(step).is_none() {
                    return Err(Error::Config(format!("rule {} uses unknown relation {step}", rule.name)));
                }
            }
        }
        Ok(())
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

    /// Is `kind` the same as, or a subtype of, `ancestor`?
    pub fn kind_is(&self, kind: &str, ancestor: &str) -> bool {
        let mut cur = Some(kind);
        let mut hops = 0;
        while let Some(k) = cur {
            if k == ancestor {
                return true;
            }
            hops += 1;
            if hops > 16 {
                break;
            }
            cur = self.object_types.get(k).and_then(|t| t.extends.as_deref());
        }
        false
    }

    pub fn prop_def(&self, kind: &str, prop: &str) -> Option<&PropertyDef> {
        let mut cur = self.object_types.get(kind);
        let mut hops = 0;
        while let Some(ty) = cur {
            if let Some(def) = ty.properties.get(prop) {
                return Some(def);
            }
            hops += 1;
            if hops > 16 {
                break;
            }
            cur = ty.extends.as_deref().and_then(|p| self.object_types.get(p));
        }
        None
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
            let ok = match def.ty {
                PropType::String | PropType::Text => value.is_string(),
                PropType::Enum => value.as_str().is_some_and(|v| def.values.iter().any(|x| x == v)),
                PropType::Int => value.is_i64() || value.is_u64(),
                PropType::Float => value.is_number(),
                PropType::Bool => value.is_boolean(),
                PropType::Timestamp => value.is_i64() || value.is_string(),
                PropType::List => value.is_array(),
                PropType::Json => true,
            };
            if !ok {
                let expected = if def.ty == PropType::Enum {
                    format!("one of {}", def.values.join("|"))
                } else {
                    format!("{:?}", def.ty).to_lowercase()
                };
                return Err(Error::invalid(format!("{kind}.{name} must be {expected}, got {value}")));
            }
        }
        if creating {
            let mut cur = Some(ty);
            while let Some(t) = cur {
                for (name, def) in &t.properties {
                    if def.required && props.get(name).is_none_or(Value::is_null) {
                        return Err(Error::invalid(format!("{kind}.{name} is required")));
                    }
                }
                cur = t.extends.as_deref().and_then(|p| self.object_types.get(p));
            }
        }
        Ok(())
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
            let rules: Vec<String> = self.rules.iter().map(|r| format!("{}={}", r.infer, r.chain.join(">"))).collect();
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
            let _ = writeln!(out);
        }
        if !self.rules.is_empty() {
            let _ = writeln!(out, "\nRules:");
            for r in &self.rules {
                let _ = writeln!(out, "  {} = {}", r.infer, r.chain.join(" > "));
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
    fn rejects_bad_rules() {
        let err = Schema::from_toml("[[rules]]\nname='x'\nchain=['nope']\ninfer='y'").unwrap_err();
        assert!(err.to_string().contains("unknown relation"));
    }
}
