//! Graph profile: what the populated data actually looks like (which
//! kind -relation-> kind patterns exist, how often, which properties and states
//! occur). Used to ground LLM prompts in real patterns rather than the whole
//! schema, and to propose schema extensions for data the schema does not
//! describe yet (ontology drift).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::graph::Graph;
use crate::model::Entity;
use crate::schema::Schema;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Pattern {
    pub from: String,
    pub rel: String,
    pub to: String,
    pub count: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PropStats {
    pub count: usize,
    pub types: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GraphProfile {
    pub entities: usize,
    pub links: usize,
    pub kinds: BTreeMap<String, usize>,
    pub patterns: Vec<Pattern>,
    pub props: BTreeMap<String, BTreeMap<String, PropStats>>,
    pub states: BTreeMap<String, BTreeMap<String, usize>>,
    pub unknown_kinds: Vec<String>,
    pub unknown_relations: Vec<String>,
    /// `kind.property` pairs not declared in the schema.
    pub unknown_props: Vec<String>,
}

/// Properties the engine itself understands (policy and fact confidence).
pub const SYSTEM_PROPS: &[&str] = &["visibility", "acl", "confidence", "verified"];

fn value_type(v: &Value) -> &'static str {
    match v {
        Value::String(s) if s.chars().count() > 120 => "text",
        Value::String(_) => "string",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "float",
        Value::Bool(_) => "bool",
        Value::Array(_) => "list",
        Value::Object(_) => "json",
        Value::Null => "null",
    }
}

pub fn profile(
    schema: &Schema,
    g: &Graph,
    workspace: Option<&str>,
    can_read: &dyn Fn(&Entity) -> bool,
) -> GraphProfile {
    let mut p = GraphProfile::default();
    let mut patterns: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    let mut unknown_rels = BTreeSet::new();
    let mut unknown_props = BTreeSet::new();
    for e in g.entities().filter(|e| workspace.is_none_or(|w| e.workspace == w) && can_read(e)) {
        p.entities += 1;
        *p.kinds.entry(e.kind.clone()).or_default() += 1;
        if let Some(s) = &e.state {
            *p.states.entry(e.kind.clone()).or_default().entry(s.clone()).or_default() += 1;
        }
        for (k, v) in &e.props {
            let st = p.props.entry(e.kind.clone()).or_default().entry(k.clone()).or_default();
            st.count += 1;
            st.types.insert(value_type(v).to_string());
            if schema.object_type(&e.kind).is_some()
                && schema.prop_def(&e.kind, k).is_none()
                && !SYSTEM_PROPS.contains(&k.as_str())
            {
                unknown_props.insert(format!("{}.{k}", e.kind));
            }
        }
        for (rel, to) in g.out_edges(&e.id) {
            let Some(t) = g.get(to).filter(|t| can_read(t)) else { continue };
            p.links += 1;
            *patterns.entry((e.kind.clone(), rel.clone(), t.kind.clone())).or_default() += 1;
            if schema.link_type(rel).is_none() {
                unknown_rels.insert(rel.clone());
            }
        }
    }
    p.unknown_kinds = p.kinds.keys().filter(|k| schema.object_type(k).is_none()).cloned().collect();
    p.unknown_relations = unknown_rels.into_iter().collect();
    p.unknown_props = unknown_props.into_iter().collect();
    p.patterns = patterns.into_iter().map(|((from, rel, to), count)| Pattern { from, rel, to, count }).collect();
    p.patterns
        .sort_by(|a, b| b.count.cmp(&a.count).then_with(|| (&a.from, &a.rel, &a.to).cmp(&(&b.from, &b.rel, &b.to))));
    p
}

/// Compact, count-free pattern list for prompts: changes only when a new
/// pattern appears, so it does not churn provider prompt caches.
pub fn pattern_summary(schema: &Schema, p: &GraphProfile, max: usize) -> String {
    let mut lines: Vec<String> = p
        .patterns
        .iter()
        .take(max)
        .map(|x| {
            let inv = schema
                .link_type(&x.rel)
                .and_then(|lt| lt.inverse.clone())
                .map(|i| format!(" ({i})"))
                .unwrap_or_default();
            format!("{} -{}-> {}{inv}", x.from, x.rel, x.to)
        })
        .collect();
    lines.sort();
    let kinds: Vec<String> = p
        .kinds
        .keys()
        .map(|k| match schema.object_type(k).filter(|t| !t.states.is_empty()) {
            Some(t) => format!("{k}[{}]", t.states.join("|")),
            None => k.clone(),
        })
        .collect();
    format!("Kinds in use: {}\nRelations in use: {}", kinds.join(", "), lines.join("; "))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SchemaSuggestion {
    /// A schema layer to review and add under `[ontology] extend`.
    pub toml: String,
    pub kinds: Vec<String>,
    pub relations: Vec<String>,
    pub properties: Vec<String>,
    /// Learned facts (join paths, metrics, synonyms) waiting for a person
    /// to certify or reject them, most confident first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub learned: Vec<String>,
}

fn toml_type(types: &BTreeSet<String>) -> &'static str {
    let t: Vec<&str> = types.iter().map(String::as_str).filter(|t| *t != "null").collect();
    match t.as_slice() {
        ["int"] => "int",
        ["float"] | ["float", "int"] | ["int", "float"] => "float",
        ["bool"] => "bool",
        ["list"] => "list",
        ["text"] | ["string", "text"] | ["text", "string"] => "text",
        ["string"] => "string",
        _ => "json",
    }
}

/// Propose schema additions for kinds, relations and properties seen in the
/// data but missing from the schema.
pub fn suggest(schema: &Schema, p: &GraphProfile) -> SchemaSuggestion {
    let mut out =
        String::from("# Suggested by onto-intelli from observed data. Review, then add under [ontology] extend.\n");
    let mut s = SchemaSuggestion::default();
    for kind in &p.unknown_kinds {
        let _ = writeln!(out, "\n[object_types.{kind}]\ndescription = \"Observed {} entities\"", p.kinds[kind]);
        if let Some(states) = p.states.get(kind) {
            let list: Vec<String> = states.keys().map(|x| format!("\"{x}\"")).collect();
            let _ = writeln!(out, "states = [{}]", list.join(", "));
        }
        for (prop, st) in p.props.get(kind).into_iter().flatten() {
            let _ = writeln!(out, "properties.{prop} = {{ type = \"{}\" }}", toml_type(&st.types));
        }
        s.kinds.push(kind.clone());
    }
    let mut by_kind: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for kp in &p.unknown_props {
        if let Some((kind, prop)) = kp.split_once('.') {
            by_kind.entry(kind.to_string()).or_default().push(prop.to_string());
        }
    }
    for (kind, props) in by_kind {
        let _ = writeln!(out, "\n[object_types.{kind}.properties]");
        for prop in props {
            let types = p.props.get(&kind).and_then(|m| m.get(&prop)).map(|st| toml_type(&st.types)).unwrap_or("json");
            let _ = writeln!(out, "{prop} = {{ type = \"{types}\" }}");
            s.properties.push(format!("{kind}.{prop}"));
        }
    }
    for rel in &p.unknown_relations {
        let from: BTreeSet<&str> = p.patterns.iter().filter(|x| &x.rel == rel).map(|x| x.from.as_str()).collect();
        let to: BTreeSet<&str> = p.patterns.iter().filter(|x| &x.rel == rel).map(|x| x.to.as_str()).collect();
        let known = |set: &BTreeSet<&str>| {
            set.iter().all(|k| schema.is_type_or_interface(k) || p.unknown_kinds.iter().any(|u| u == k))
        };
        let q = |set: &BTreeSet<&str>| set.iter().map(|k| format!("\"{k}\"")).collect::<Vec<_>>().join(", ");
        let _ = writeln!(out, "\n[link_types.{rel}]");
        if known(&from) {
            let _ = writeln!(out, "from = [{}]", q(&from));
        }
        if known(&to) {
            let _ = writeln!(out, "to = [{}]", q(&to));
        }
        let _ = writeln!(out, "# inverse = \"...\"   # name for walking it backwards");
        s.relations.push(rel.clone());
    }
    if s.kinds.is_empty() && s.relations.is_empty() && s.properties.is_empty() {
        out.push_str("\n# The schema already covers everything in the data.\n");
    }
    s.toml = out;
    s
}

impl crate::Ontology {
    /// Populated-pattern profile of what the caller can see.
    pub fn profile(&self, p: &crate::Principal, workspace: Option<&str>) -> crate::Result<GraphProfile> {
        if !p.allows(crate::Access::Read) {
            return Err(crate::Error::Denied(format!("{} has no read role", p.id)));
        }
        Ok(self.read(|g, schema| {
            let can = |e: &Entity| crate::policy::visible(p, e, g);
            profile(schema, g, workspace, &can)
        }))
    }

    /// Schema additions proposed from data the schema does not describe yet.
    pub fn suggest_schema(&self, p: &crate::Principal, workspace: Option<&str>) -> crate::Result<SchemaSuggestion> {
        let prof = self.profile(p, workspace)?;
        let mut s = suggest(&self.schema(), &prof);
        let mut facts = self.learned_facts(p, None, Some("suggested"), None, 200)?;
        facts.retain(|f| {
            workspace.is_none() || self.get(p, &f.id).is_ok_and(|e| Some(e.workspace.as_str()) == workspace)
        });
        facts.sort_by(|a, b| b.confidence.total_cmp(&a.confidence).then_with(|| a.id.cmp(&b.id)));
        s.learned = facts
            .iter()
            .take(25)
            .map(|f| {
                format!(
                    "{} {} (confidence {:.2}): onto learned --certify {}",
                    f.fact_type, f.statement, f.confidence, f.id
                )
            })
            .collect();
        Ok(s)
    }
}
