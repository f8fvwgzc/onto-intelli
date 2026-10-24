//! Industry packs: schema layers for one industry, installable on top of
//! `builtin:enterprise`, and the small marketplace that lists, shows, plans
//! and installs them.
//!
//! A pack is a TOML schema layer that starts with a `[pack]` table
//! ([`PackMeta`]: id, title, industry, version, requires, standards) plus a
//! sample data set of one [`EntityInput`] per line. Every pack is also a
//! built-in schema package, so `extend = ["builtin:<id>"]` loads it from
//! configuration; at runtime it is installed as a live schema change.
//!
//! * **A pack is assumptions, not facts.** Its `[[rules]]` are supplied rules:
//!   what they derive is labelled with the rule and never written to the
//!   graph, and a certificate that cites them earns
//!   `entailed_under_supplied_rules`, never `entailed`. The standards a pack
//!   lists are vocabulary it borrows; a pack never claims compliance.
//! * **Installing is a governed schema change.** [`change_set`] computes the
//!   puts that turn the current schema into the current schema with the pack
//!   layered on top. [`Ontology::pack_plan`] previews it like any change set,
//!   and [`Ontology::pack_install`] applies it (roles in
//!   `governance.apply_roles`) or files it as a proposal. Installing a pack
//!   that is already installed changes nothing.
//! * **Requirements are checked, not assumed.** A pack names the packages it
//!   layers on (`requires`); every object type of each must exist in the
//!   current schema, or the pack is refused with what is missing.
//! * **Samples load after their schema.** A sample is validated against the
//!   planned schema before anything is written, loads only into existing
//!   fields it does not overwrite (`fill_only`), and contains exactly one
//!   deliberate shape violation that the pack's `sample_note` documents.
//! * **Determinism.** Packs, names and listings come out sorted or in
//!   registry order.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::action_types::ActionType;
use crate::engine::{ActionContext, entity_id};
use crate::live_schema::{SchemaChangeSet, SchemaPlan, SchemaVersionInfo, apply_changes, changes_between};
use crate::metrics::MetricDef;
use crate::model::{EntityInput, LinkInput, Mutation};
use crate::policy::{Access, Principal};
use crate::proposals::{ProposalInput, ProposalView};
use crate::schema::{Glossary, InterfaceType, LinkType, ObjectType, PACKAGES, Rule, Schema};
use crate::shape_model::Shape;
use crate::time::now_ms;
use crate::{Error, Ontology, Result};

/// Manufacturing operations: plants, lines, parts and BOMs, production orders, lots, quality.
pub const MANUFACTURING_SCHEMA: &str = include_str!("../schemas/packs/manufacturing.toml");
/// Sample data for [`MANUFACTURING_SCHEMA`].
pub const MANUFACTURING_SAMPLE: &str = include_str!("../schemas/packs/manufacturing.sample.jsonl");

/// Supply chain and sourcing: suppliers, sites, regions, purchase orders, disruptions.
pub const SUPPLY_CHAIN_SCHEMA: &str = include_str!("../schemas/packs/supply-chain.toml");
/// Sample data for [`SUPPLY_CHAIN_SCHEMA`].
pub const SUPPLY_CHAIN_SAMPLE: &str = include_str!("../schemas/packs/supply-chain.sample.jsonl");

/// Retail and omnichannel commerce: stores, SKUs, stock, shoppers, orders, promotions, recalls.
pub const RETAIL_SCHEMA: &str = include_str!("../schemas/packs/retail.toml");
/// Sample data for [`RETAIL_SCHEMA`].
pub const RETAIL_SAMPLE: &str = include_str!("../schemas/packs/retail.sample.jsonl");

/// Logistics and transportation: carriers, hubs, shipments, legs, handling units, customs.
pub const LOGISTICS_SCHEMA: &str = include_str!("../schemas/packs/logistics.toml");
/// Sample data for [`LOGISTICS_SCHEMA`].
pub const LOGISTICS_SAMPLE: &str = include_str!("../schemas/packs/logistics.sample.jsonl");

/// The registry: `(id, schema TOML, sample JSONL)`, in listing order. Every
/// entry is also in [`PACKAGES`] under the same id.
pub const REGISTRY: &[(&str, &str, &str)] = &[
    ("manufacturing", MANUFACTURING_SCHEMA, MANUFACTURING_SAMPLE),
    ("supply-chain", SUPPLY_CHAIN_SCHEMA, SUPPLY_CHAIN_SAMPLE),
    ("retail", RETAIL_SCHEMA, RETAIL_SAMPLE),
    ("logistics", LOGISTICS_SCHEMA, LOGISTICS_SAMPLE),
];

/// What a pack's own words mean, for every report that shows one.
const MEANS: &str = "a pack is definitions plus supplied rules: what its rules derive is an assumption the pack \
                     carries, not a fact it establishes; the standards it lists are vocabulary it borrows, not \
                     compliance it claims";

/// The `[pack]` table: what the file is, never part of the schema itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackMeta {
    /// `[a-z][a-z0-9-]*`, the built-in package name (`builtin:<id>`).
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub industry: String,
    /// `MAJOR.MINOR.PATCH`.
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Packages this one layers on; their object types must exist first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Public standards whose vocabulary the pack borrows (never a compliance claim).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub standards: Vec<String>,
    /// What the sample data shows, including its one deliberate shape violation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sample_note: String,
}

/// A registered pack: its metadata, schema layer and sample data.
#[derive(Clone, Debug)]
pub struct Pack {
    pub meta: PackMeta,
    pub toml: &'static str,
    pub sample: &'static str,
}

/// How many definitions a pack layer carries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackCounts {
    pub object_types: usize,
    pub link_types: usize,
    pub interfaces: usize,
    pub rules: usize,
    pub shapes: usize,
    pub metrics: usize,
    pub action_types: usize,
    pub glossary_terms: usize,
    pub sample_entities: usize,
}

/// One pack in a listing, measured against a current schema.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackInfo {
    #[serde(flatten)]
    pub meta: PackMeta,
    pub counts: PackCounts,
    /// Every definition the pack carries is already in the current schema.
    pub installed: bool,
    /// Schema changes an install would make (0 when installed).
    pub pending_changes: usize,
    /// Required packages whose object types the current schema lacks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_requires: Vec<String>,
    /// Why the pack cannot be layered on the current schema, when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

/// A named definition, as a pack shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackItem {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// A link type, as a pack shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackLink {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub from: Vec<String>,
    pub to: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub transitive: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dependency: bool,
}

/// A supplied rule, as a pack shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackRule {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// `chain` or `horn`.
    pub form: String,
    /// `uses_component = has_bom > bom_component` or `?s a high_risk <= ...`.
    pub text: String,
}

/// Everything a pack defines, for review before installing it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackDetail {
    #[serde(flatten)]
    pub meta: PackMeta,
    pub counts: PackCounts,
    pub object_types: Vec<PackItem>,
    pub link_types: Vec<PackLink>,
    pub interfaces: Vec<PackItem>,
    pub rules: Vec<PackRule>,
    /// Classes the rules derive (`high_risk_supplier`), sorted.
    pub derived_classes: Vec<String>,
    /// Relations the rules derive (`uses_component`), sorted.
    pub derived_relations: Vec<String>,
    pub shapes: Vec<PackItem>,
    pub metrics: Vec<PackItem>,
    pub action_types: Vec<PackItem>,
    /// `section.word` → target, for every glossary term.
    pub glossary: BTreeMap<String, String>,
    /// The pack's schema layer as written.
    pub toml: String,
    pub means: String,
}

/// A name two packages both define (an installable pack must not share any).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameCollision {
    /// `names` (types, relations, inverses, interfaces, rules, shapes,
    /// metrics, action types and derived terms share one namespace) or
    /// `glossary.kinds|relations|dimensions`.
    pub namespace: String,
    pub name: String,
    pub packages: Vec<String>,
}

/// Apply the schema change directly or file it as a proposal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallMode {
    #[default]
    Apply,
    Propose,
}

/// Options of [`Ontology::pack_install`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PackInstallOptions {
    pub mode: InstallMode,
    /// Load the pack's sample entities into the context's workspace once its
    /// schema is in force.
    pub sample: bool,
    /// Why (the change set message and the proposal rationale).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What an install did.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackInstalled {
    pub pack: String,
    pub pack_version: String,
    pub mode: InstallMode,
    /// The schema already held every definition: nothing was applied or proposed.
    pub already_installed: bool,
    /// Schema changes applied or proposed.
    pub changes: usize,
    /// The schema version the install created (apply mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<SchemaVersionInfo>,
    /// The proposal it filed (propose mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<ProposalView>,
    pub sample_entities: usize,
    pub sample_links: usize,
    /// Graph revision after the last write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// The sections of a schema file, parsed without compiling them.
#[derive(Debug, Default, Deserialize)]
struct Layer {
    #[serde(default)]
    pack: Option<PackMeta>,
    #[serde(default)]
    object_types: BTreeMap<String, ObjectType>,
    #[serde(default)]
    link_types: BTreeMap<String, LinkType>,
    #[serde(default)]
    interfaces: BTreeMap<String, InterfaceType>,
    #[serde(default)]
    rules: Vec<Rule>,
    #[serde(default)]
    shapes: BTreeMap<String, Shape>,
    #[serde(default)]
    metrics: BTreeMap<String, MetricDef>,
    #[serde(default)]
    action_types: BTreeMap<String, ActionType>,
    #[serde(default)]
    glossary: Glossary,
}

impl Layer {
    fn parse(what: &str, text: &str) -> Result<Layer> {
        toml::from_str(text).map_err(|e| Error::Config(format!("{what}: {e}")))
    }

    fn counts(&self) -> PackCounts {
        PackCounts {
            object_types: self.object_types.len(),
            link_types: self.link_types.len(),
            interfaces: self.interfaces.len(),
            rules: self.rules.len(),
            shapes: self.shapes.len(),
            metrics: self.metrics.len(),
            action_types: self.action_types.len(),
            glossary_terms: self.glossary.kinds.len() + self.glossary.relations.len() + self.glossary.dimensions.len(),
            sample_entities: 0,
        }
    }

    /// Relations the layer declares: link type names and their inverses.
    fn declared_relations(&self) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = self.link_types.keys().cloned().collect();
        out.extend(self.link_types.values().filter_map(|lt| lt.inverse.clone()));
        out
    }

    /// Classes and relations the layer's rules derive (beyond what it declares).
    fn derived(&self) -> (BTreeSet<String>, BTreeSet<String>) {
        let declared = self.declared_relations();
        let (mut classes, mut relations) = (BTreeSet::new(), BTreeSet::new());
        for r in &self.rules {
            let head = if r.is_horn() {
                match (r.then.get(1).and_then(|v| v.as_str()), r.then.get(2).and_then(|v| v.as_str())) {
                    (Some("a"), Some(class)) => {
                        if !class.starts_with('?') {
                            classes.insert(class.to_string());
                        }
                        None
                    }
                    (Some(rel), _) => Some(rel),
                    _ => None,
                }
            } else {
                Some(r.infer.as_str())
            };
            if let Some(rel) = head.map(|h| h.trim_start_matches('^'))
                && !rel.is_empty()
                && !rel.starts_with('?')
                && !declared.contains(rel)
            {
                relations.insert(rel.to_string());
            }
        }
        (classes, relations)
    }

    /// Every name the layer introduces in the shared namespace.
    fn names(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        out.extend(self.object_types.keys().cloned());
        out.extend(self.declared_relations());
        out.extend(self.interfaces.keys().cloned());
        out.extend(self.rules.iter().map(|r| r.name.clone()));
        out.extend(self.shapes.keys().cloned());
        out.extend(self.metrics.keys().cloned());
        out.extend(self.action_types.keys().cloned());
        let (classes, relations) = self.derived();
        out.extend(classes);
        out.extend(relations);
        out
    }
}

/// Is `id` a well-formed pack id (`[a-z][a-z0-9-]*`, at most 64 characters)?
fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    id.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `MAJOR.MINOR.PATCH`, digits only.
fn valid_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Parse and validate the `[pack]` table of a pack's TOML.
pub fn parse_meta(toml: &str) -> Result<PackMeta> {
    let layer = Layer::parse("pack", toml)?;
    let meta = layer.pack.ok_or_else(|| Error::Config("pack: no [pack] table".into()))?;
    let at = |m: String| Error::Config(format!("pack {}: {m}", meta.id));
    if !valid_id(&meta.id) {
        return Err(Error::Config(format!("pack id '{}' must match [a-z][a-z0-9-]* (at most 64 characters)", meta.id)));
    }
    if meta.title.trim().is_empty() {
        return Err(at("needs a title".into()));
    }
    if !valid_version(&meta.version) {
        return Err(at(format!("version '{}' must be MAJOR.MINOR.PATCH", meta.version)));
    }
    if let Some(r) = meta.requires.iter().find(|r| r.trim().is_empty() || **r == meta.id) {
        return Err(at(format!("cannot require '{r}'")));
    }
    Ok(meta)
}

/// Every registered pack, in registry order.
pub fn packs() -> Vec<Pack> {
    REGISTRY
        .iter()
        .map(|(id, toml, sample)| Pack {
            meta: parse_meta(toml).unwrap_or_else(|e| panic!("registered pack {id} has a valid [pack] table: {e}")),
            toml,
            sample,
        })
        .collect()
}

/// One registered pack by id.
pub fn find(id: &str) -> Result<Pack> {
    packs().into_iter().find(|p| p.meta.id == id).ok_or_else(|| {
        let ids: Vec<&str> = REGISTRY.iter().map(|(i, _, _)| *i).collect();
        Error::not_found(format!("pack '{id}' (available: {})", ids.join(", ")))
    })
}

fn package_toml(name: &str) -> Option<&'static str> {
    PACKAGES.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// Required packages (`requires`) whose object types `current` lacks, as
/// `"<package>: <type>, <type>"` lines (`"<package>: unknown package"`).
pub fn missing_requires(current: &Schema, meta: &PackMeta) -> Vec<String> {
    let mut out = Vec::new();
    for r in &meta.requires {
        let Some(text) = package_toml(r) else {
            out.push(format!("{r}: unknown package"));
            continue;
        };
        let missing: Vec<String> = match Layer::parse(r, text) {
            Ok(layer) => layer.object_types.into_keys().filter(|k| !current.object_types.contains_key(k)).collect(),
            Err(e) => vec![e.to_string()],
        };
        if !missing.is_empty() {
            out.push(format!("{r}: {}", missing.join(", ")));
        }
    }
    out
}

/// Refuse a pack whose required packages are not in `current`.
pub fn check_requires(current: &Schema, meta: &PackMeta) -> Result<()> {
    let missing = missing_requires(current, meta);
    if missing.is_empty() {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "pack {} requires {}; the current schema lacks object types of {}",
        meta.id,
        meta.requires.join(", "),
        missing.join("; ")
    )))
}

/// `current` with the pack layered on top (requirements checked).
pub fn with_pack(current: &Schema, id: &str) -> Result<Schema> {
    let pack = find(id)?;
    check_requires(current, &pack.meta)?;
    let mut s = current.clone();
    s.extend_toml(pack.toml).map_err(|e| match e {
        Error::Config(m) => Error::Invalid(format!("pack {id} does not layer on the current schema: {m}")),
        other => other,
    })?;
    Ok(s)
}

/// The pack on top of the packages it requires, alone (empty schema first,
/// then each required package in order, then the pack).
pub fn standalone(id: &str) -> Result<Schema> {
    let pack = find(id)?;
    let mut s = Schema::empty();
    for r in &pack.meta.requires {
        let text = package_toml(r).ok_or_else(|| Error::Config(format!("pack {id} requires unknown package {r}")))?;
        s.extend_toml(text)?;
    }
    s.extend_toml(pack.toml)?;
    Ok(s)
}

/// The schema changes that install the pack on `current`: puts for every
/// definition the pack adds or changes, nothing for what is already there.
/// Empty when the pack is installed.
pub fn change_set(current: &Schema, id: &str) -> Result<SchemaChangeSet> {
    let pack = find(id)?;
    let target = with_pack(current, id)?;
    Ok(SchemaChangeSet {
        changes: changes_between(current, &target),
        message: format!("install pack {} {}: {}", pack.meta.id, pack.meta.version, pack.meta.title),
        ..Default::default()
    })
}

/// The registered packs measured against `current`.
pub fn list(current: &Schema) -> Vec<PackInfo> {
    packs()
        .into_iter()
        .map(|p| {
            let mut counts = Layer::parse(&p.meta.id, p.toml).map(|l| l.counts()).unwrap_or_default();
            counts.sample_entities = sample_lines(p.sample).count();
            let missing = missing_requires(current, &p.meta);
            let (pending, blocked) = if missing.is_empty() {
                match change_set(current, &p.meta.id) {
                    Ok(cs) => (cs.changes.len(), None),
                    Err(e) => (0, Some(e.to_string())),
                }
            } else {
                (0, Some(format!("requires {}", missing.join("; "))))
            };
            PackInfo {
                installed: blocked.is_none() && pending == 0,
                pending_changes: pending,
                missing_requires: missing,
                blocked,
                counts,
                meta: p.meta,
            }
        })
        .collect()
}

/// Everything a pack defines, read from its layer.
pub fn show(id: &str) -> Result<PackDetail> {
    let pack = find(id)?;
    let layer = Layer::parse(id, pack.toml)?;
    let mut counts = layer.counts();
    counts.sample_entities = sample_lines(pack.sample).count();
    let item = |name: &String, label: &str, description: &str| PackItem {
        name: name.clone(),
        label: label.to_string(),
        description: description.to_string(),
    };
    let (classes, relations) = layer.derived();
    let mut glossary = BTreeMap::new();
    for (section, terms) in [
        ("kinds", &layer.glossary.kinds),
        ("relations", &layer.glossary.relations),
        ("dimensions", &layer.glossary.dimensions),
    ] {
        for (word, target) in terms {
            glossary.insert(format!("{section}.{word}"), target.clone());
        }
    }
    Ok(PackDetail {
        counts,
        object_types: layer.object_types.iter().map(|(n, t)| item(n, &t.label, &t.description)).collect(),
        link_types: layer
            .link_types
            .iter()
            .map(|(n, lt)| PackLink {
                name: n.clone(),
                description: lt.description.clone(),
                from: lt.from.clone(),
                to: lt.to.clone(),
                inverse: lt.inverse.clone(),
                transitive: lt.transitive,
                dependency: lt.dependency,
            })
            .collect(),
        interfaces: layer.interfaces.iter().map(|(n, i)| item(n, "", &i.description)).collect(),
        rules: layer
            .rules
            .iter()
            .map(|r| PackRule {
                name: r.name.clone(),
                description: r.description.clone(),
                form: if r.is_horn() { "horn" } else { "chain" }.into(),
                text: r.describe(),
            })
            .collect(),
        derived_classes: classes.into_iter().collect(),
        derived_relations: relations.into_iter().collect(),
        shapes: layer.shapes.iter().map(|(n, s)| item(n, "", &s.description)).collect(),
        metrics: layer.metrics.iter().map(|(n, m)| item(n, "", &m.description)).collect(),
        action_types: layer.action_types.iter().map(|(n, a)| item(n, "", &a.description)).collect(),
        glossary,
        toml: pack.toml.to_string(),
        means: MEANS.into(),
        meta: pack.meta,
    })
}

/// Non-empty lines of a JSONL sample, numbered from 1.
fn sample_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines().enumerate().map(|(i, l)| (i + 1, l.trim())).filter(|(_, l)| !l.is_empty())
}

/// The pack's sample entities, in file order. Lines are workspace-agnostic:
/// one naming a workspace, or lacking a kind and a key, is refused.
pub fn sample(id: &str) -> Result<Vec<EntityInput>> {
    let pack = find(id)?;
    let mut out = Vec::new();
    for (n, line) in sample_lines(pack.sample) {
        let at = |m: String| Error::Config(format!("pack {id} sample line {n}: {m}"));
        let e: EntityInput = serde_json::from_str(line).map_err(|e| at(e.to_string()))?;
        if e.workspace.is_some() {
            return Err(at("samples are workspace-agnostic: remove `workspace`".into()));
        }
        if e.key.as_deref().is_none_or(str::is_empty) && e.id.as_deref().is_none_or(str::is_empty) {
            return Err(at("needs a key (or an id) so reloading it updates rather than duplicates".into()));
        }
        entity_id(&e).map_err(|e| at(e.to_string()))?;
        out.push(e);
    }
    Ok(out)
}

/// Mutations that load `entities`: every upsert first, then every link, so
/// a line may link to an entity a later line defines.
pub fn sample_mutations(entities: Vec<EntityInput>) -> Vec<Mutation> {
    let mut upserts = Vec::with_capacity(entities.len());
    let mut links = Vec::new();
    for mut e in entities {
        let from = match entity_id(&e) {
            Ok((id, _)) => id,
            Err(_) => format!("{}:{}", e.kind, e.key.clone().unwrap_or_default()),
        };
        for l in std::mem::take(&mut e.links) {
            links.push(Mutation::Link(LinkInput { from: from.clone(), rel: l.rel, to: l.to, props: l.props }));
        }
        upserts.push(Mutation::Upsert(e));
    }
    upserts.extend(links);
    upserts
}

/// Names that a registered pack shares with another registered pack or with
/// a non-pack built-in package (enterprise, data-catalog, code, ...), sorted.
/// Packs must not share names: installing two of them, or one over a base
/// package, would merge their definitions by name.
pub fn collisions() -> Vec<NameCollision> {
    let pack_ids: Vec<&str> = REGISTRY.iter().map(|(id, _, _)| *id).collect();
    let mut layers: Vec<(&str, &str)> = PACKAGES.iter().filter(|(n, _)| !pack_ids.contains(n)).copied().collect();
    layers.extend(REGISTRY.iter().map(|(id, toml, _)| (*id, *toml)));
    collisions_in(&layers, &pack_ids)
}

/// Names shared by two of `layers` (`(package, TOML)`) where at least one of
/// them is in `packs`; the other layers may share names among themselves
/// (base packages restate each other's relations on purpose). A layer that
/// does not parse is reported under the namespace `unparsable`.
pub fn collisions_in(layers: &[(&str, &str)], packs: &[&str]) -> Vec<NameCollision> {
    let mut owners: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut out = Vec::new();
    for (pkg, text) in layers {
        let layer = match Layer::parse(pkg, text) {
            Ok(l) => l,
            Err(e) => {
                out.push(NameCollision {
                    namespace: "unparsable".into(),
                    name: e.to_string(),
                    packages: vec![pkg.to_string()],
                });
                continue;
            }
        };
        let mut add = |ns: &str, name: &str| {
            owners.entry((ns.to_string(), name.to_string())).or_default().insert(pkg.to_string());
        };
        for n in layer.names() {
            add("names", &n);
        }
        for (ns, terms) in [
            ("glossary.kinds", &layer.glossary.kinds),
            ("glossary.relations", &layer.glossary.relations),
            ("glossary.dimensions", &layer.glossary.dimensions),
        ] {
            for w in terms.keys() {
                add(ns, &w.trim().to_lowercase());
            }
        }
    }
    out.extend(
        owners
            .into_iter()
            .filter(|(_, pkgs)| pkgs.len() > 1 && pkgs.iter().any(|p| packs.contains(&p.as_str())))
            .map(|((namespace, name), pkgs)| NameCollision { namespace, name, packages: pkgs.into_iter().collect() }),
    );
    out
}

impl Ontology {
    fn pack_reader(&self, p: &Principal) -> Result<()> {
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        Ok(())
    }

    /// The registered packs measured against the schema in force.
    pub fn pack_list(&self, p: &Principal) -> Result<Vec<PackInfo>> {
        self.pack_reader(p)?;
        Ok(list(&self.schema()))
    }

    /// Everything a pack defines.
    pub fn pack_show(&self, p: &Principal, id: &str) -> Result<PackDetail> {
        self.pack_reader(p)?;
        show(id)
    }

    /// Dry run of installing a pack: the schema plan of its change set. An
    /// installed pack yields a valid plan with no changes and a warning.
    pub fn pack_plan(&self, p: &Principal, id: &str) -> Result<SchemaPlan> {
        self.pack_reader(p)?;
        let active = self.active_schema();
        let set = change_set(&active.schema, id)?;
        if set.changes.is_empty() {
            return Ok(installed_plan(active.version, id));
        }
        self.schema_plan(p, &set)
    }

    /// Install a pack: apply its change set (`governance.apply_roles`; agents
    /// never) or file it as a proposal, then, with `sample`, load its sample
    /// entities into `ctx.workspace`. The sample is validated against the
    /// planned schema before anything is written. A sample needs the pack's
    /// schema in force, so `sample` with `propose` is refused until the
    /// proposal is applied (install again then: the schema part is a no-op).
    pub fn pack_install(&self, ctx: &ActionContext, id: &str, opts: PackInstallOptions) -> Result<PackInstalled> {
        let ctx = &ctx.clone().with_intent("pack.install");
        self.pack_reader(&ctx.principal)?;
        let pack = find(id)?;
        let active = self.active_schema();
        let current = active.schema;
        let mut set = change_set(&current, id)?;
        if let Some(m) = opts.message.as_ref().filter(|m| !m.trim().is_empty()) {
            set.message = m.trim().to_string();
        }
        let mut out = PackInstalled {
            pack: pack.meta.id.clone(),
            pack_version: pack.meta.version.clone(),
            mode: opts.mode,
            already_installed: set.changes.is_empty(),
            changes: set.changes.len(),
            version: None,
            proposal: None,
            sample_entities: 0,
            sample_links: 0,
            revision: None,
        };
        // Refuse what the caller may not do before any work is staged.
        if !out.already_installed
            && opts.mode == InstallMode::Apply
            && !self.options().governance.can_apply(&ctx.principal)
        {
            let why = format!(
                "{} may not apply schema changes, so it may not install pack {id} directly: propose instead \
                 (mode = propose)",
                ctx.principal.id
            );
            self.audit_denied(ctx, &why, now_ms());
            return Err(Error::Denied(why));
        }
        let sample_muts = if opts.sample {
            if opts.mode == InstallMode::Propose && !out.already_installed {
                return Err(Error::invalid(format!(
                    "the sample of pack {id} needs its schema in force: propose without sample, and install again \
                     with sample once the proposal is applied"
                )));
            }
            if !ctx.principal.allows(Access::Write) {
                let why = format!("{} may not write the sample of pack {id}", ctx.principal.id);
                self.audit_denied(ctx, &why, now_ms());
                return Err(Error::Denied(why));
            }
            let muts = sample_mutations(
                sample(id)?.into_iter().map(|e| EntityInput { fill_only: true, ..e }).collect::<Vec<_>>(),
            );
            let planned = if out.already_installed {
                (*current).clone()
            } else {
                apply_changes(&current, &set.changes)
                    .map_err(|e| Error::Invalid(format!("pack {id} does not install: {}", e.join("; "))))?
            };
            {
                let g = self.inner_graph_read();
                self.stage_only(&g, &planned, ctx, muts.clone())
                    .map_err(|e| prefix(e, &format!("the sample of pack {id} does not load")))?;
            }
            Some(muts)
        } else {
            None
        };
        if !out.already_installed {
            match opts.mode {
                InstallMode::Apply => {
                    // The change set was computed from this version: refuse if
                    // another change lands first rather than apply a stale diff.
                    set.expected_version = Some(active.version);
                    let applied = self.schema_apply(ctx, set)?;
                    out.revision = Some(applied.revision);
                    out.version = Some(applied.version);
                }
                InstallMode::Propose => {
                    let rationale = opts.message.clone().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| {
                        format!("{} (requires {})", pack.meta.description, pack.meta.requires.join(", "))
                    });
                    let input = ProposalInput {
                        title: format!("Install pack {} {}", pack.meta.title, pack.meta.version),
                        rationale,
                        change_set: set,
                        source: None,
                    };
                    out.proposal = Some(self.propose_schema(ctx, input)?);
                }
            }
        }
        if let Some(muts) = sample_muts {
            out.sample_links = muts.iter().filter(|m| matches!(m, Mutation::Link(_))).count();
            out.sample_entities = muts.len() - out.sample_links;
            let res = self.apply(ctx, muts).map_err(|e| {
                let at = match &out.version {
                    Some(v) => format!("pack {id} was installed as schema version {}, but its sample", v.version),
                    None => format!("the sample of pack {id}"),
                };
                prefix(e, &format!("{at} did not load"))
            })?;
            out.revision = Some(res.revision);
        }
        Ok(out)
    }
}

/// The plan of installing a pack that is already installed.
fn installed_plan(version: u64, id: &str) -> SchemaPlan {
    SchemaPlan {
        base_version: version,
        valid: true,
        warnings: vec![format!("pack {id} is already installed: nothing to change")],
        ..Default::default()
    }
}

/// The same error kind with `context` in front of its message.
fn prefix(e: Error, context: &str) -> Error {
    match e {
        Error::Invalid(m) => Error::Invalid(format!("{context}: {m}")),
        Error::NotFound(m) => Error::Invalid(format!("{context}: {m} not found")),
        Error::Denied(m) => Error::Denied(format!("{context}: {m}")),
        Error::Conflict(m) => Error::Conflict(format!("{context}: {m}")),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_is_validated() {
        let ok = parse_meta("[pack]\nid = 'x-1'\ntitle = 'X'\nversion = '1.0.0'\n").unwrap();
        assert_eq!(ok.id, "x-1");
        let err = |t: &str| parse_meta(t).unwrap_err().to_string();
        assert!(err("[object_types.a]\n").contains("no [pack] table"));
        assert!(err("[pack]\nid = 'X'\ntitle = 'X'\nversion = '1.0.0'\n").contains("must match"));
        assert!(err("[pack]\nid = 'x'\nversion = '1.0.0'\n").contains("needs a title"));
        assert!(err("[pack]\nid = 'x'\ntitle = 'X'\nversion = '1.0'\n").contains("MAJOR.MINOR.PATCH"));
        assert!(err("[pack]\nid = 'x'\ntitle = 'X'\nversion = '1.0.0'\nrequires = ['x']\n").contains("cannot require"));
        assert!(err("[pack]\nid = 'x'\ntitle = 'X'\nversion = '1.0.0'\nrequires = 'enterprise'\n").contains("pack"));
    }

    #[test]
    fn layers_name_their_derived_vocabulary() {
        let layer = Layer::parse(
            "t",
            r#"
            [link_types.r]
            inverse = "r_of"
            [[rules]]
            name = "c"
            chain = ["r", "r"]
            infer = "rr"
            [[rules]]
            name = "h"
            when = [["?x", "r", "?y"]]
            then = ["?x", "a", "special"]
            [[rules]]
            name = "back"
            chain = ["r"]
            infer = "r_of"
            "#,
        )
        .unwrap();
        let (classes, relations) = layer.derived();
        assert_eq!(classes.into_iter().collect::<Vec<_>>(), ["special"]);
        assert_eq!(relations.into_iter().collect::<Vec<_>>(), ["rr"], "declared inverses are not derived names");
        assert!(layer.names().contains("r_of") && layer.names().contains("c"));
    }

    #[test]
    fn sample_mutations_put_links_last() {
        let e = EntityInput::new("issue", "1", "One").link("blocked_by", "issue:2");
        let muts = sample_mutations(vec![e, EntityInput::new("issue", "2", "Two")]);
        assert!(matches!(&muts[0], Mutation::Upsert(e) if e.links.is_empty()));
        assert!(matches!(&muts[1], Mutation::Upsert(_)));
        assert!(matches!(&muts[2], Mutation::Link(l) if l.from == "issue:1" && l.to == "issue:2"));
    }
}
