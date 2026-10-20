//! Interoperability exports: Mermaid diagrams of subgraphs and RDF Turtle of
//! the schema (OWL classes, object/datatype properties, inverses) and the data
//! (with PROV-O provenance).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde_json::Value;

use crate::graph::Graph;
use crate::model::Entity;
use crate::query::Edge;
use crate::schema::Schema;

const BASE: &str = "https://onto-intelli.dev/";

fn mermaid_id(id: &str) -> String {
    let s: String = id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("n_{s}")
}

fn mermaid_text(s: &str) -> String {
    s.replace('"', "#quot;").replace(['[', ']', '{', '}', '<', '>', '|'], " ")
}

/// `graph LR` diagram; entities in alert states are highlighted.
pub fn mermaid(schema: &Schema, nodes: &[Entity], edges: &[Edge]) -> String {
    let mut out = String::from("graph LR\n");
    let mut alerts = Vec::new();
    for e in nodes {
        let state = e.state.as_ref().map(|s| format!(" · {s}")).unwrap_or_default();
        let _ = writeln!(
            out,
            "  {}[\"{}<br/>{}{}\"]",
            mermaid_id(&e.id),
            mermaid_text(e.label()),
            e.kind,
            mermaid_text(&state)
        );
        if schema.is_alert(&e.kind, e.state.as_deref()) {
            alerts.push(mermaid_id(&e.id));
        }
    }
    for x in edges {
        let _ = writeln!(out, "  {} -->|{}| {}", mermaid_id(&x.from), mermaid_text(&x.rel), mermaid_id(&x.to));
    }
    if !alerts.is_empty() {
        out.push_str("  classDef alert stroke:#d33,stroke-width:3px;\n");
        let _ = writeln!(out, "  class {} alert", alerts.join(","));
    }
    out
}

fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b':') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn iri(space: &str, name: &str) -> String {
    format!("<{BASE}{space}/{}>", pct(name))
}

fn lit(s: &str) -> String {
    let escaped =
        s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t");
    format!("\"{escaped}\"")
}

fn value_literal(v: &Value) -> Vec<String> {
    match v {
        Value::Null => Vec::new(),
        Value::Bool(b) => vec![format!("\"{b}\"^^xsd:boolean")],
        Value::Number(n) if n.is_i64() || n.is_u64() => vec![format!("\"{n}\"^^xsd:integer")],
        Value::Number(n) => vec![format!("\"{n}\"^^xsd:decimal")],
        Value::String(s) => vec![lit(s)],
        Value::Array(a) => a.iter().flat_map(value_literal).collect(),
        Value::Object(_) => vec![lit(&v.to_string())],
    }
}

/// Turtle document. `include_schema` emits the ontology (OWL), `entities`
/// the instance data (already permission-filtered by the caller).
pub fn turtle(schema: &Schema, g: &Graph, entities: &[&Entity], include_schema: bool) -> String {
    let mut out = String::new();
    out.push_str("@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .\n");
    out.push_str("@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n");
    out.push_str("@prefix owl: <http://www.w3.org/2002/07/owl#> .\n");
    out.push_str("@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n");
    out.push_str("@prefix prov: <http://www.w3.org/ns/prov#> .\n");
    let _ = writeln!(out, "@prefix onto: <{BASE}schema/> .\n");
    if include_schema {
        let _ = writeln!(
            out,
            "<{BASE}schema> a owl:Ontology ; rdfs:label {} ; owl:versionInfo \"{}\" .\n",
            lit(&schema.name),
            schema.version
        );
        for (name, iface) in &schema.interfaces {
            let _ = writeln!(
                out,
                "{} a owl:Class ; rdfs:label {} ; rdfs:comment {} .",
                iri("class", name),
                lit(name),
                lit(&format!("interface. {}", iface.description))
            );
        }
        let mut datatype_props: BTreeSet<String> = BTreeSet::new();
        for t in schema.object_types.values() {
            let _ = write!(out, "{} a owl:Class ; rdfs:label {}", iri("class", &t.name), lit(&t.name));
            if !t.description.is_empty() {
                let _ = write!(out, " ; rdfs:comment {}", lit(&t.description));
            }
            for parent in t.extends.iter().chain(t.implements.iter()) {
                let _ = write!(out, " ; rdfs:subClassOf {}", iri("class", parent));
            }
            out.push_str(" .\n");
            datatype_props.extend(t.properties.keys().cloned());
        }
        for iface in schema.interfaces.values() {
            datatype_props.extend(iface.properties.keys().cloned());
        }
        for p in &datatype_props {
            let _ = writeln!(out, "{} a owl:DatatypeProperty ; rdfs:label {} .", iri("property", p), lit(p));
        }
        out.push_str("onto:state a owl:DatatypeProperty ; rdfs:label \"state\" .\n");
        out.push_str("onto:workspace a owl:DatatypeProperty ; rdfs:label \"workspace\" .\n");
        for lt in schema.link_types.values() {
            let _ = write!(out, "{} a owl:ObjectProperty", iri("relation", &lt.name));
            if lt.symmetric {
                out.push_str(" , owl:SymmetricProperty");
            }
            if lt.dependency {
                out.push_str(" , owl:TransitiveProperty");
            }
            let _ = write!(out, " ; rdfs:label {}", lit(&lt.name));
            if !lt.description.is_empty() {
                let _ = write!(out, " ; rdfs:comment {}", lit(&lt.description));
            }
            if lt.from.len() == 1 {
                let _ = write!(out, " ; rdfs:domain {}", iri("class", &lt.from[0]));
            }
            if lt.to.len() == 1 {
                let _ = write!(out, " ; rdfs:range {}", iri("class", &lt.to[0]));
            }
            out.push_str(" .\n");
            if let Some(inv) = &lt.inverse {
                let _ = writeln!(
                    out,
                    "{} a owl:ObjectProperty ; rdfs:label {} ; owl:inverseOf {} .",
                    iri("relation", inv),
                    lit(inv),
                    iri("relation", &lt.name)
                );
            }
        }
        out.push('\n');
    }
    for e in entities {
        let _ = write!(
            out,
            "{} a {} ;\n  rdfs:label {} ;\n  onto:workspace {}",
            iri("entity", &e.id),
            iri("class", &e.kind),
            lit(e.label()),
            lit(&e.workspace)
        );
        if let Some(s) = &e.state {
            let _ = write!(out, " ;\n  onto:state {}", lit(s));
        }
        for (k, v) in &e.props {
            for l in value_literal(v) {
                let _ = write!(out, " ;\n  {} {l}", iri("property", k));
            }
        }
        for (rel, to) in g.out_edges(&e.id) {
            if entities.iter().any(|x| &x.id == to) {
                let _ = write!(out, " ;\n  {} {}", iri("relation", rel), iri("entity", to));
            }
        }
        if let Some(pv) = &e.provenance {
            for src in &pv.derived_from {
                let _ = write!(out, " ;\n  prov:wasDerivedFrom {}", iri("entity", src));
            }
            if let Some(gb) = &pv.generated_by {
                let _ = write!(out, " ;\n  prov:wasAttributedTo {}", iri("entity", gb));
            }
        }
        out.push_str(" .\n");
    }
    out
}

impl crate::Ontology {
    /// Mermaid diagram of a traversal (permission-filtered).
    pub fn export_mermaid(&self, p: &crate::Principal, spec: &crate::query::TraverseSpec) -> crate::Result<String> {
        let sub = self.traverse(p, spec)?;
        Ok(mermaid(&self.schema(), &sub.nodes, &sub.edges))
    }

    /// Turtle of the schema and/or the visible data (optionally a traversal only).
    pub fn export_turtle(
        &self,
        p: &crate::Principal,
        workspace: Option<&str>,
        include_schema: bool,
        include_data: bool,
        start: Option<&crate::query::TraverseSpec>,
    ) -> crate::Result<String> {
        let subset: Option<Vec<String>> = match start {
            Some(spec) => Some(self.traverse(p, spec)?.nodes.into_iter().map(|e| e.id).collect()),
            None => None,
        };
        if !p.allows(crate::Access::Read) {
            return Err(crate::Error::Denied(format!("{} has no read role", p.id)));
        }
        Ok(self.read(|g, schema| {
            let entities: Vec<&Entity> = if include_data {
                g.entities()
                    .filter(|e| workspace.is_none_or(|w| e.workspace == w))
                    .filter(|e| subset.as_ref().is_none_or(|s| s.contains(&e.id)))
                    .filter(|e| crate::policy::visible(p, e, g))
                    .collect()
            } else {
                Vec::new()
            };
            let mut entities = entities;
            entities.sort_by(|a, b| a.id.cmp(&b.id));
            turtle(schema, g, &entities, include_schema)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_iris_and_literals() {
        assert_eq!(pct("issue:T 1#x"), "issue:T%201%23x");
        assert_eq!(lit("a \"b\"\n"), "\"a \\\"b\\\"\\n\"");
        assert_eq!(mermaid_id("service:payment-api"), "n_service_payment_api");
    }
}
