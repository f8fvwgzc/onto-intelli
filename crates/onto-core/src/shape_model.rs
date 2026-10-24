//! Shapes: declarative conditions on the objects of a target (a kind, an
//! interface, a derived class, a list of ids, or the subjects or objects of
//! a relation), in the style of SHACL node and property shapes.
//!
//! This module holds the schema model only: the definitions, their merge by
//! name and their compile-time validation. Evaluating shapes against the
//! graph is a separate report. A shape report is a measurement of the graph,
//! not a proof; constraints this model does not understand are kept in
//! `unknown` and reported as undetermined rather than silently passed.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::schema::{PropType, Schema, pattern_regex};

/// How serious a shape violation is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeSeverity {
    #[default]
    Violation,
    Warning,
    Info,
}

impl ShapeSeverity {
    fn is_default(&self) -> bool {
        *self == ShapeSeverity::Violation
    }
}

/// A node shape: which objects it targets and what must hold for each.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Shape {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Objects of this kind, interface or derived class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// These objects, by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_ids: Vec<String>,
    /// Objects that are the source of this relation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_subjects_of: Option<String>,
    /// Objects that are the target of this relation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_objects_of: Option<String>,
    #[serde(default, skip_serializing_if = "ShapeSeverity::is_default")]
    pub severity: ShapeSeverity,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// No properties outside the declared paths (and `ignored_properties`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub closed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_properties: Vec<String>,
    /// Kept in the schema but not evaluated.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deactivated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<PropertyShape>,
}

/// Conditions on the values reached from a focus object through `path`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PropertyShape {
    /// Stable id; `<shape>/<index>` when not given.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// A relation (canonical, inverse name or `^rel`), a sequence of
    /// relations (`assigned_to.member_of`), `prop.<name>`, `state` or `title`.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_count: Option<usize>,
    /// Linked objects must be of this kind, interface or derived class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datatype: Option<PropType>,
    /// Values must be one of these.
    #[serde(rename = "in", default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<Value>,
    /// Some value must equal this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_value: Option<Value>,
    /// Strings must match this regular expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_value: Option<f64>,
    /// Linked objects must conform to this shape (nested at most 5 deep).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<ShapeSeverity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Keys this model does not support; their constraints are skipped and
    /// reported as undetermined.
    #[serde(flatten)]
    pub unknown: BTreeMap<String, Value>,
}

/// Relation steps a shape path may have.
pub const MAX_PATH_STEPS: usize = 16;

/// What a property shape path walks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShapePath {
    Title,
    State,
    Prop(String),
    /// Relation steps as written (canonical, inverse names or `^rel`).
    Relations(Vec<String>),
}

impl ShapePath {
    pub fn parse(path: &str) -> ShapePath {
        match path {
            "title" => ShapePath::Title,
            "state" => ShapePath::State,
            p => match p.strip_prefix("prop.") {
                Some(name) => ShapePath::Prop(name.to_string()),
                None => ShapePath::Relations(p.split('.').map(str::to_string).collect()),
            },
        }
    }
}

impl Shape {
    /// Default ids for property shapes that have none, `<shape>/<index>`.
    pub(crate) fn normalize(&mut self, name: &str) {
        for (i, p) in self.properties.iter_mut().enumerate() {
            if p.id.is_empty() {
                p.id = format!("{name}/{i}");
            }
        }
    }

    /// Every name the shape references must be known to `schema`.
    /// `classes` and `relations` are the classes and relations rules derive.
    pub(crate) fn validate(
        &self,
        schema: &Schema,
        classes: &BTreeSet<String>,
        relations: &BTreeSet<String>,
    ) -> std::result::Result<(), String> {
        let class_ok = |c: &str| schema.is_type_or_interface(c) || classes.contains(c);
        let rel_ok = |r: &str| schema.resolve_rel(r).is_some() || relations.contains(r.strip_prefix('^').unwrap_or(r));
        if let Some(t) = &self.target
            && !class_ok(t)
        {
            return Err(format!("unknown target {t}"));
        }
        for r in self.target_subjects_of.iter().chain(&self.target_objects_of) {
            if !rel_ok(r) {
                return Err(format!("unknown relation {r}"));
            }
        }
        if self.target_ids.iter().any(|id| id.is_empty()) {
            return Err("target_ids may not contain an empty id".into());
        }
        let mut ids = BTreeSet::new();
        for p in &self.properties {
            let at = if p.id.is_empty() { p.path.clone() } else { p.id.clone() };
            if !p.id.is_empty() && !ids.insert(p.id.as_str()) {
                return Err(format!("duplicate property shape id {}", p.id));
            }
            match ShapePath::parse(&p.path) {
                ShapePath::Title | ShapePath::State => {}
                ShapePath::Prop(name) => {
                    let known = schema.object_types.values().any(|t| t.properties.contains_key(&name))
                        || schema.interfaces.values().any(|i| i.properties.contains_key(&name));
                    if !known {
                        return Err(format!("{at}: unknown property {name}"));
                    }
                }
                ShapePath::Relations(steps) => {
                    if steps.len() > MAX_PATH_STEPS {
                        return Err(format!(
                            "{at}: path {} has {} steps, more than {MAX_PATH_STEPS}",
                            p.path,
                            steps.len()
                        ));
                    }
                    for s in &steps {
                        if s.is_empty() || !rel_ok(s) {
                            return Err(format!("{at}: unknown relation {s} in path {}", p.path));
                        }
                    }
                }
            }
            if let Some(c) = &p.class
                && !class_ok(c)
            {
                return Err(format!("{at}: unknown class {c}"));
            }
            if let Some(n) = &p.node
                && !schema.shapes.contains_key(n)
            {
                return Err(format!("{at}: unknown shape {n}"));
            }
            if let Some(pat) = &p.pattern {
                pattern_regex(pat).map_err(|e| format!("{at}: {e}"))?;
            }
            if let (Some(lo), Some(hi)) = (p.min_count, p.max_count)
                && lo > hi
            {
                return Err(format!("{at}: min_count {lo} exceeds max_count {hi}"));
            }
        }
        Ok(())
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_parse() {
        assert_eq!(ShapePath::parse("state"), ShapePath::State);
        assert_eq!(ShapePath::parse("prop.priority"), ShapePath::Prop("priority".into()));
        assert_eq!(
            ShapePath::parse("assigned_to.member_of"),
            ShapePath::Relations(vec!["assigned_to".into(), "member_of".into()])
        );
    }

    #[test]
    fn unknown_keys_are_kept() {
        let p: PropertyShape =
            serde_json::from_value(serde_json::json!({ "path": "title", "qualified_min_count": 1, "in": ["a"] }))
                .unwrap();
        assert_eq!(p.values, vec![Value::from("a")]);
        assert_eq!(p.unknown["qualified_min_count"], 1);
        let back = serde_json::to_value(&p).unwrap();
        assert_eq!(back["qualified_min_count"], 1);
        assert_eq!(back["in"][0], "a");
    }
}
