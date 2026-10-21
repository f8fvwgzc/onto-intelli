//! The ontology builder as a service: draft object, link and action types
//! from a crawled data source, let the LLM refine the draft and answer
//! modeling questions, then apply the draft or file it as a proposal.
//!
//! The draft itself is deterministic ([`onto_core::modeling`]). The model
//! only suggests edits to it; every suggestion is applied to a copy and
//! kept only when the result still passes `schema.plan`. Applying follows
//! schema governance: people with an apply role apply, agents propose.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use onto_core::modeling::{ActionKind, DraftAction, DraftOptions, LinkVia, OntologyDraft, plural, snake, title_case};
use onto_core::query::FindSpec;
use onto_core::schema::{Category, PropType};
use onto_core::{ActionContext, Error, Ontology, Principal, ProposalInput, Result, one_or_many};

use crate::answer::Direct;
use crate::intelli::Intelli;
use crate::llm::{ChatMessage, LlmRequest};

/// Suggestions read from one model reply, at most.
const MAX_SUGGESTIONS: usize = 50;

const REFINE_SYSTEM: &str = "You refine a draft ontology generated from a database catalog. \
Suggest the changes a careful data modeler would make: clearer object, link and action names, \
labels, descriptions, property type corrections, link inverse names, and transition actions for \
states. Reply with one JSON object and nothing else:\n\
{\"suggestions\": [{\"target\": \"object:<name> | property:<object>.<property> | link:<name> | action:<name>\", \
\"field\": \"name | label | description | type | values | inverse | category | risk | transition\", \
\"from\": <current value>, \"to\": <new value>, \"why\": \"<one sentence>\"}]}\n\
Rules: names are snake_case ([a-z][a-z0-9_]*). Property types are string, text, int, float, bool, \
timestamp, enum (give its values with field \"values\"), json or list. Categories are thing, actor, \
container, artifact or knowledge. Risk is low, medium or high. A transition's \"to\" is one of the \
object's states. Only suggest what the catalog supports; return {\"suggestions\": []} when the draft is fine.";

const ADVISE_SYSTEM: &str = "You are an ontology modeling advisor. A draft ontology was generated \
from a database catalog: object types backed by tables, link types from foreign keys and join \
tables, and action types for the edits people make. Answer the question using only the draft and \
the facts given. Name the object, link and action types you mean, say which table or column each \
comes from, and explain the trade-offs briefly. Be concise.";

// ── inputs ───────────────────────────────────────────────────────────────

/// Draft options as the actions take them. The negative flags (`no_sync`,
/// `no_actions`, `no_default_exclude`) are the gRPC spelling, since proto3
/// drops `false` values.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct DraftIn {
    /// Connector name; empty picks the source named in the question, or the
    /// only crawled source.
    #[serde(default, alias = "connector")]
    pub source: Option<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub tables: Vec<String>,
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
    /// Exclude nothing unless `exclude` lists it.
    #[serde(default)]
    pub no_default_exclude: bool,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub sync: Option<bool>,
    #[serde(default)]
    pub no_sync: bool,
    #[serde(default)]
    pub actions: Option<bool>,
    #[serde(default)]
    pub no_actions: bool,
    #[serde(default)]
    pub views: bool,
}

impl DraftIn {
    fn options(&self, source: String) -> DraftOptions {
        let exclude = match (&self.exclude, self.no_default_exclude) {
            (Some(list), _) if !list.is_empty() => Some(list.clone()),
            (_, true) => Some(Vec::new()),
            (other, false) => other.clone(),
        };
        DraftOptions {
            source,
            tables: self.tables.clone(),
            exclude,
            prefix: self.prefix.clone().filter(|p| !p.trim().is_empty()),
            sync: self.sync.unwrap_or(!self.no_sync),
            actions: self.actions.unwrap_or(!self.no_actions),
            views: self.views,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RefineIn {
    #[serde(flatten)]
    pub draft: DraftIn,
    /// What the model should pay attention to ("use the business names").
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AdviseIn {
    pub question: String,
    #[serde(default, alias = "connector")]
    pub source: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ApplyDraftIn {
    #[serde(flatten)]
    pub options: DraftIn,
    /// A reviewed draft (from `ontology.draft` or `ontology.refine`, possibly
    /// edited); its change set is rebuilt from its parts. Drafted afresh
    /// from the options when absent.
    #[serde(default)]
    pub draft: Option<OntologyDraft>,
    /// `apply` (default) or `propose`.
    #[serde(default)]
    pub mode: Option<String>,
    /// gRPC spelling of `mode: propose`.
    #[serde(default)]
    pub propose: bool,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub rationale: Option<String>,
    /// Do not crawl the source after applying.
    #[serde(default)]
    pub no_crawl: bool,
    /// Apply nothing: return the draft with its change set rebuilt from its
    /// (edited) parts, for `schema.plan`.
    #[serde(default)]
    pub preview: bool,
    /// Where a proposal comes from (`mcp`, `ui`); `ontology.apply_draft` by default.
    #[serde(default)]
    pub proposal_source: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
}

// ── outputs ──────────────────────────────────────────────────────────────

/// One change the model suggests to the draft.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Suggestion {
    /// `object:<name>`, `property:<object>.<property>`, `link:<name>` or `action:<name>`.
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub field: String,
    #[serde(default)]
    pub from: Value,
    #[serde(default)]
    pub to: Value,
    #[serde(default)]
    pub why: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Rejected {
    pub suggestion: Suggestion,
    pub error: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Refined {
    pub draft: OntologyDraft,
    pub suggestions: Vec<Suggestion>,
    /// Suggestions kept in the draft.
    pub applied: usize,
    pub rejected: Vec<Rejected>,
    /// The provider that made the suggestions (`none` when no model ran).
    pub llm: String,
    pub tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Advice {
    pub answer: String,
    /// `deterministic` (from the draft) or `llm`.
    pub mode: String,
    pub facts: Vec<String>,
    pub source: String,
    pub tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

// ── sources ──────────────────────────────────────────────────────────────

fn words(text: &str) -> String {
    let t: String =
        text.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { ' ' }).collect();
    format!(" {} ", t.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// The data source to model: the one given, else the one `text` names (by
/// key or title), else the only crawled source `p` can see.
pub fn resolve_source(onto: &Ontology, p: &Principal, given: Option<&str>, text: &str) -> Result<String> {
    if let Some(s) = given.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(s.strip_prefix("data_source:").unwrap_or(s).to_string());
    }
    let spec = FindSpec { kind: Some("data_source".into()), limit: 1_000, ..Default::default() };
    let (sources, _) = onto.find(p, &spec)?;
    let keys: Vec<(String, String)> = sources
        .iter()
        .filter_map(|e| e.id.strip_prefix("data_source:").map(|k| (k.to_string(), e.title.to_lowercase())))
        .collect();
    let q = words(text);
    let named: Vec<&String> = keys
        .iter()
        .filter(|(k, title)| {
            q.contains(&format!(" {} ", k.to_lowercase()))
                || q.contains(&words(&k.replace(['-', '_'], " ")))
                || (!title.trim().is_empty() && q.contains(&words(title)))
        })
        .map(|(k, _)| k)
        .collect();
    match (named.as_slice(), keys.as_slice()) {
        ([one], _) => Ok((*one).clone()),
        (_, [(only, _)]) => Ok(only.clone()),
        (_, []) => {
            Err(Error::not_found("a crawled data source: connect one (onto connect postgres <url>) and crawl it first"))
        }
        _ => Err(Error::invalid(format!(
            "several data sources ({}): name the one to model",
            keys.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

// ── deterministic advice ─────────────────────────────────────────────────

fn name_of<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn has_any(q: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| q.contains(n))
}

fn list(items: &[String]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        2 => format!("{} and {}", items[0], items[1]),
        n => format!("{}, and {}", items[..n - 1].join(", "), items[n - 1]),
    }
}

/// How a link is backed, in words.
fn link_basis(d: &OntologyDraft, l: &onto_core::modeling::DraftLink) -> String {
    match l.via {
        LinkVia::ForeignKey => {
            let table = d.object(&l.from).map(|o| o.table.clone()).unwrap_or_default();
            format!("foreign key {table}.{}", l.column.as_deref().unwrap_or("?"))
        }
        LinkVia::JoinTable => format!("join table {}", l.table.as_deref().unwrap_or("?")),
        LinkVia::Learned => format!("learned join, confidence {:.2}", l.confidence),
    }
}

/// Is the question about one of these topics? Object types are the default.
#[derive(Default)]
struct Topics {
    objects: bool,
    links: bool,
    actions: bool,
    properties: bool,
}

fn topics(q: &str) -> Topics {
    let t = Topics {
        links: has_any(q, &[" link", " relationship", " relation", " connect", " foreign key", " join", " reference"]),
        actions: has_any(q, &[" action", " transition", " workflow", " operation", " edit ", " edits "]),
        properties: has_any(
            q,
            &[" column", " property", " properties", " field", " fields", " data type", " attribute", " typed"],
        ),
        objects: has_any(q, &[" object type", " object types", " objects ", " entity", " entities", " tables"]),
    };
    if !t.links && !t.actions && !t.properties { Topics { objects: true, ..t } } else { t }
}

/// The object types a question mentions (by name, plural, label or table).
fn focus(d: &OntologyDraft, q: &str) -> Vec<String> {
    d.objects
        .iter()
        .filter(|o| {
            let table = o.table.rsplit('.').next().unwrap_or_default();
            [o.name.clone(), plural(&o.name), o.label.to_lowercase(), table.to_string(), o.table.clone()]
                .iter()
                .any(|w| !w.is_empty() && q.contains(&words(w)))
        })
        .map(|o| o.name.clone())
        .collect()
}

/// Answer a modeling question from a draft, without a model. The answer
/// names the object, link, action or property types the question is about,
/// with the tables and columns behind them; the facts carry the reasons.
pub fn advise_from_draft(d: &OntologyDraft, question: &str) -> Direct {
    let q = words(question);
    let t = topics(&q);
    let focus = focus(d, &q);
    let about = |name: &str| focus.is_empty() || focus.iter().any(|f| f == name);
    let mut parts: Vec<String> = Vec::new();
    let mut facts: Vec<String> = Vec::new();

    if d.objects.is_empty() {
        let why = if d.notes.is_empty() { "no table has a primary key".to_string() } else { d.notes.join("; ") };
        parts.push(format!("I cannot draft object types from {}: {why}.", d.source));
    }
    if t.objects && !d.objects.is_empty() {
        let shown: Vec<String> = d
            .objects
            .iter()
            .filter(|o| about(&o.name))
            .map(|o| {
                let mut s = format!(
                    "{} (from {}, key {}, title {}, {}",
                    o.name,
                    o.table,
                    o.key.join(", "),
                    o.title.as_deref().unwrap_or("none"),
                    name_of(&o.category)
                );
                if !o.states.is_empty() {
                    s.push_str(&format!(", states {}", o.states.join("|")));
                }
                s.push(')');
                s
            })
            .collect();
        parts.push(format!(
            "From {} I would create {} object type{}: {}.",
            d.source,
            shown.len(),
            if shown.len() == 1 { "" } else { "s" },
            list(&shown)
        ));
        if focus.is_empty() && !d.skipped.is_empty() {
            let skipped: Vec<String> = d.skipped.iter().map(|s| format!("{} ({})", s.table, s.reason)).collect();
            parts.push(format!("Not modeled: {}.", list(&skipped)));
        }
        for o in d.objects.iter().filter(|o| about(&o.name)) {
            facts.push(format!("object type {} from {}: {}", o.name, o.table, o.reasons.join("; ")));
        }
    }
    if t.properties {
        for o in d.objects.iter().filter(|o| about(&o.name)) {
            let props: Vec<String> = o
                .properties
                .iter()
                .map(|p| {
                    let mut s = format!("{} {} (column {} {})", p.name, name_of(&p.ty), p.column, p.data_type);
                    if p.ty == PropType::Enum && !p.values.is_empty() {
                        s.push_str(&format!(" values {}", p.values.join("|")));
                    }
                    if p.required {
                        s.push_str(" required");
                    }
                    if p.unique {
                        s.push_str(" unique");
                    }
                    s
                })
                .collect();
            if props.is_empty() {
                parts.push(format!("{} has no properties besides its key and links.", o.name));
            } else {
                parts.push(format!("{} properties: {}.", o.name, props.join("; ")));
            }
            for p in &o.properties {
                facts.push(format!("{}.{} is {}: {}", o.name, p.name, name_of(&p.ty), p.reason));
            }
        }
    }
    if t.links {
        let links: Vec<String> = d
            .links
            .iter()
            .filter(|l| about(&l.from) || about(&l.to))
            .map(|l| {
                format!(
                    "{} → {} as {} ({}, {}{})",
                    l.from,
                    l.to,
                    l.name,
                    name_of(&l.cardinality),
                    link_basis(d, l),
                    if l.inverse.is_empty() { String::new() } else { format!(", inverse {}", l.inverse) }
                )
            })
            .collect();
        if links.is_empty() {
            let who = if focus.is_empty() { "the drafted tables".to_string() } else { list(&focus) };
            parts.push(format!(
                "No link types: no foreign key or learned join connects {who} to another drafted table."
            ));
        } else {
            parts.push(format!("Link types: {}.", links.join("; ")));
        }
        for l in d.links.iter().filter(|l| about(&l.from) || about(&l.to)) {
            facts.push(format!("link type {}: {}", l.name, l.reason));
        }
    }
    if t.actions {
        let actions: Vec<String> = d
            .actions
            .iter()
            .filter(|a| about(&a.object))
            .map(|a| format!("{} ({} {}, risk {})", a.name, name_of(&a.kind), a.object, name_of(&a.risk)))
            .collect();
        if actions.is_empty() {
            parts.push("No action types are drafted for these object types.".into());
        } else {
            parts.push(format!("Action types: {}.", actions.join("; ")));
        }
        for a in d.actions.iter().filter(|a| about(&a.object)) {
            facts.push(format!("action type {}: {}", a.name, a.reason));
        }
    }
    if !t.links && !d.links.is_empty() && !t.properties && !t.actions {
        parts.push(format!(
            "The draft also has {} link type{} and {} action type{}.",
            d.links.len(),
            if d.links.len() == 1 { "" } else { "s" },
            d.actions.len(),
            if d.actions.len() == 1 { "" } else { "s" }
        ));
    }
    if !d.objects.is_empty() {
        parts.push(format!("Review it with onto model draft {0}; apply it with onto model apply {0}.", d.source));
    }
    Direct { answer: parts.join(" "), facts }
}

/// The deterministic answer to a modeling question ("What object types can
/// we create from the warehouse?"), from a draft of the source it names.
pub fn model_answer(onto: &Ontology, p: &Principal, question: &str) -> Option<Direct> {
    let source = resolve_source(onto, p, None, question).ok()?;
    let d = onto.draft_ontology(p, &DraftOptions::for_source(source)).ok()?;
    Some(advise_from_draft(&d, question))
}

/// A compact description of the draft for the model.
pub fn draft_summary(d: &OntologyDraft) -> String {
    let mut s = format!("Data source: {}\nObject types:\n", d.source);
    for o in &d.objects {
        s.push_str(&format!(
            "- {} \"{}\" (table {}, key {}, title {}, category {}){}\n",
            o.name,
            o.label,
            o.table,
            o.key.join(", "),
            o.title.as_deref().unwrap_or("none"),
            name_of(&o.category),
            if o.description.is_empty() { String::new() } else { format!(": {}", o.description) }
        ));
        if !o.states.is_empty() {
            s.push_str(&format!("  states ({}): {}\n", o.state.as_deref().unwrap_or("state"), o.states.join(", ")));
        }
        for p in &o.properties {
            s.push_str(&format!(
                "  property {}: {} from column {} {}{}{}\n",
                p.name,
                name_of(&p.ty),
                p.column,
                p.data_type,
                if p.values.is_empty() { String::new() } else { format!(" values {}", p.values.join("|")) },
                if p.description.is_empty() { String::new() } else { format!(" \"{}\"", p.description) }
            ));
        }
    }
    if !d.links.is_empty() {
        s.push_str("Link types:\n");
        for l in &d.links {
            s.push_str(&format!(
                "- {}: {} -> {} ({}, {}), inverse {}\n",
                l.name,
                l.from,
                l.to,
                name_of(&l.cardinality),
                link_basis(d, l),
                if l.inverse.is_empty() { "none" } else { &l.inverse }
            ));
        }
    }
    if !d.actions.is_empty() {
        s.push_str("Action types:\n");
        for a in &d.actions {
            s.push_str(&format!("- {} ({} on {}, risk {})\n", a.name, name_of(&a.kind), a.object, name_of(&a.risk)));
        }
    }
    if !d.skipped.is_empty() {
        s.push_str("Not modeled:\n");
        for k in &d.skipped {
            s.push_str(&format!("- {}: {}\n", k.table, k.reason));
        }
    }
    s
}

// ── suggestions ──────────────────────────────────────────────────────────

/// The first JSON object (or array) in a model reply: fenced, quoted in
/// prose, or bare.
pub fn first_json(text: &str) -> Option<Value> {
    let bytes = text.as_bytes();
    let mut start = 0;
    while let Some(off) = text[start..].find(['{', '[']) {
        let begin = start + off;
        let (open, close) = if bytes[begin] == b'{' { (b'{', b'}') } else { (b'[', b']') };
        let (mut depth, mut in_str, mut escaped) = (0usize, false, false);
        for (i, &b) in bytes.iter().enumerate().skip(begin) {
            if in_str {
                match b {
                    _ if escaped => escaped = false,
                    b'\\' => escaped = true,
                    b'"' => in_str = false,
                    _ => {}
                }
                continue;
            }
            match b {
                b'"' => in_str = true,
                _ if b == open => depth += 1,
                _ if b == close => {
                    depth -= 1;
                    if depth == 0 {
                        if let Ok(v) = serde_json::from_str::<Value>(&text[begin..=i]) {
                            return Some(v);
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
        start = begin + 1;
    }
    None
}

/// Suggestions from a model reply; malformed entries are left out.
pub fn parse_suggestions(text: &str) -> Vec<Suggestion> {
    let Some(v) = first_json(text) else { return Vec::new() };
    let items = match v {
        Value::Array(a) => a,
        Value::Object(mut m) => match m.remove("suggestions") {
            Some(Value::Array(a)) => a,
            _ => vec![Value::Object(m)],
        },
        _ => Vec::new(),
    };
    items
        .into_iter()
        .filter_map(|i| serde_json::from_value::<Suggestion>(i).ok())
        .filter(|s| !s.target.trim().is_empty() && !s.field.trim().is_empty())
        .take(MAX_SUGGESTIONS)
        .collect()
}

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn valid_name(s: &str) -> std::result::Result<String, String> {
    let n = snake(s);
    if n.is_empty() || !n.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(format!("'{s}' is not a valid name ([a-z][a-z0-9_]*)"));
    }
    Ok(n)
}

fn prop_type(s: &str) -> std::result::Result<PropType, String> {
    let t = match s.trim().to_lowercase().as_str() {
        "integer" | "bigint" | "smallint" | "long" => "int".to_string(),
        "number" | "decimal" | "numeric" | "double" | "real" | "money" => "float".to_string(),
        "boolean" => "bool".to_string(),
        "date" | "datetime" | "time" | "timestamptz" => "timestamp".to_string(),
        "varchar" | "uuid" | "char" => "string".to_string(),
        "jsonb" | "object" => "json".to_string(),
        "array" => "list".to_string(),
        other => other.to_string(),
    };
    serde_json::from_value(json!(t)).map_err(|_| format!("unknown property type '{s}'"))
}

fn values_of(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(text_of).collect(),
        Value::String(s) => s.split([',', '|']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => Vec::new(),
    }
}

/// Apply one suggestion to the draft's parts (the change set is rebuilt by
/// the caller). `schema` guards names already used outside the draft;
/// `catalog` is the deterministic draft, whose properties say what the
/// database guarantees: a column's value list (enum or CHECK) and whether it
/// is NOT NULL. A suggestion that the rows would not meet (invented enum
/// values, `required` on a nullable column) is refused, since rows that do
/// not fit the schema never become objects.
pub fn apply_suggestion(
    d: &mut OntologyDraft,
    s: &Suggestion,
    schema: &onto_core::Schema,
    catalog: &OntologyDraft,
) -> std::result::Result<(), String> {
    let field = s.field.trim().to_lowercase();
    let (kind, name) = match s.target.trim().split_once(':') {
        Some((k, n)) => (k.trim().to_lowercase(), n.trim().to_string()),
        None => {
            let n = s.target.trim().to_string();
            let k = if d.object(&n).is_some() {
                "object"
            } else if d.links.iter().any(|l| l.name == n) {
                "link"
            } else if d.actions.iter().any(|a| a.name == n) {
                "action"
            } else if n.contains('.') {
                "property"
            } else {
                return Err(format!("no object, link or action type named {n} in the draft"));
            };
            (k.to_string(), n)
        }
    };
    let to_text = || text_of(&s.to).ok_or_else(|| format!("{field} needs a value"));
    match kind.as_str() {
        "object" | "object_type" => {
            let idx = d.objects.iter().position(|o| o.name == name).ok_or(format!("no object type {name}"))?;
            match field.as_str() {
                "name" => {
                    let new = valid_name(&to_text()?)?;
                    if new == name {
                        return Ok(());
                    }
                    if d.object(&new).is_some() {
                        return Err(format!("the draft already has an object type {new}"));
                    }
                    let table = d.objects[idx].table.clone();
                    if let Some(t) = schema.object_types.get(&new)
                        && !t.backing.as_ref().is_some_and(|b| b.source == d.source && b.table == table)
                    {
                        return Err(format!("{new} is an existing object type; the draft must not replace it"));
                    }
                    let o = &mut d.objects[idx];
                    if o.label == title_case(&name) {
                        o.label = title_case(&new);
                    }
                    o.name = new.clone();
                    for l in &mut d.links {
                        if l.from == name {
                            l.from = new.clone();
                        }
                        if l.to == name {
                            l.to = new.clone();
                        }
                    }
                    let draft_actions: Vec<String> = d.actions.iter().map(|a| a.name.clone()).collect();
                    for a in d.actions.iter_mut().filter(|a| a.object == name) {
                        a.object = new.clone();
                        if let Some(stem) = a.name.strip_suffix(&format!("_{name}")) {
                            let renamed = format!("{stem}_{new}");
                            let taken = draft_actions.contains(&renamed) || schema.action_types.contains_key(&renamed);
                            if !taken {
                                a.name = renamed;
                            }
                        }
                    }
                }
                "label" => d.objects[idx].label = to_text()?,
                "description" => d.objects[idx].description = to_text()?,
                "category" => {
                    d.objects[idx].category = serde_json::from_value::<Category>(json!(to_text()?.to_lowercase()))
                        .map_err(|_| "category is thing, actor, container, artifact or knowledge".to_string())?;
                }
                "transition" | "transitions" | "add_transition" => {
                    let (state, given) = match &s.to {
                        Value::Object(m) => (
                            m.get("state").and_then(text_of).ok_or("a transition needs a state")?,
                            m.get("name").and_then(text_of),
                        ),
                        v => (text_of(v).ok_or("a transition needs a state")?, None),
                    };
                    let o = &d.objects[idx];
                    if !o.states.contains(&state) {
                        return Err(format!("{state} is not a state of {name} ({})", o.states.join(", ")));
                    }
                    if d.actions.iter().any(|a| {
                        a.object == name && a.kind == ActionKind::Transition && a.state.as_deref() == Some(&state)
                    }) {
                        return Err(format!("{name} already has a transition to {state}"));
                    }
                    let action = valid_name(&given.unwrap_or_else(|| format!("mark_{name}_{state}")))?;
                    if d.actions.iter().any(|a| a.name == action) || schema.action_types.contains_key(&action) {
                        return Err(format!("an action type {action} exists already"));
                    }
                    let label = o.label.to_lowercase();
                    let terminal = o.terminal_states.contains(&state);
                    d.actions.push(DraftAction {
                        name: action,
                        object: name.clone(),
                        kind: ActionKind::Transition,
                        risk: if terminal {
                            onto_core::action_types::Risk::Medium
                        } else {
                            onto_core::action_types::Risk::Low
                        },
                        description: format!("Move a {label} to {state}."),
                        phrases: vec![format!("mark {label} {state}"), format!("set {label} to {state}")],
                        state: Some(state.clone()),
                        reason: if s.why.is_empty() { "suggested by the model".into() } else { s.why.clone() },
                        ..Default::default()
                    });
                }
                other => {
                    return Err(format!(
                        "object types take name, label, description, category or transition, not {other}"
                    ));
                }
            }
        }
        "property" | "prop" | "column" => {
            let (obj, prop) =
                name.split_once('.').ok_or(format!("property target {name} must be <object>.<property>"))?;
            let o = d.objects.iter_mut().find(|o| o.name == obj).ok_or(format!("no object type {obj}"))?;
            let idx = o
                .properties
                .iter()
                .position(|p| p.name == prop || p.column == prop)
                .ok_or(format!("{obj} has no property {prop}"))?;
            let column = o.properties[idx].column.clone();
            let db = catalog
                .objects
                .iter()
                .find(|c| c.table == o.table)
                .and_then(|c| c.properties.iter().find(|p| p.column == column));
            match field.as_str() {
                "name" => {
                    let new = valid_name(&to_text()?)?;
                    if o.properties.iter().enumerate().any(|(i, p)| i != idx && p.name == new) {
                        return Err(format!("{obj} already has a property {new}"));
                    }
                    if matches!(new.as_str(), "visibility" | "acl") {
                        return Err(format!("{new} is reserved for access control"));
                    }
                    o.properties[idx].name = new;
                }
                "type" | "ty" | "data_type" => {
                    let ty = prop_type(&to_text()?)?;
                    let p = &mut o.properties[idx];
                    if ty == PropType::Enum && p.values.is_empty() {
                        return Err("an enum property needs values (field values)".into());
                    }
                    p.ty = ty;
                    p.reason =
                        format!("{} (refined: {})", p.reason, if s.why.is_empty() { "suggested" } else { &s.why });
                }
                "values" | "enum" | "enum_values" => {
                    let values = values_of(&s.to);
                    if values.is_empty() {
                        return Err("values needs a list".into());
                    }
                    let allowed = db.map(|p| p.values.as_slice()).unwrap_or_default();
                    if allowed.is_empty() {
                        return Err(format!(
                            "column {column} has no value list in the database (enum or CHECK): rows may hold any value, so it is not an enum"
                        ));
                    }
                    let missing: Vec<&str> =
                        allowed.iter().filter(|v| !values.contains(v)).map(String::as_str).collect();
                    if !missing.is_empty() {
                        return Err(format!(
                            "values must keep every value column {column} allows; missing {}",
                            missing.join(", ")
                        ));
                    }
                    let p = &mut o.properties[idx];
                    p.values = values;
                    p.ty = PropType::Enum;
                }
                "description" => o.properties[idx].description = to_text()?,
                "required" => {
                    let required = s.to.as_bool().ok_or("required is true or false")?;
                    if required && !db.is_some_and(|p| p.required) {
                        return Err(format!(
                            "column {column} is nullable in the database: rows without a value would not become objects"
                        ));
                    }
                    o.properties[idx].required = required;
                }
                "searchable" => o.properties[idx].searchable = s.to.as_bool().ok_or("searchable is true or false")?,
                other => {
                    return Err(format!(
                        "properties take name, type, values, description, required or searchable, not {other}"
                    ));
                }
            }
        }
        "link" | "link_type" => {
            let idx = d.links.iter().position(|l| l.name == name).ok_or(format!("no link type {name}"))?;
            match field.as_str() {
                "name" => {
                    let new = valid_name(&to_text()?)?;
                    if new == name {
                        return Ok(());
                    }
                    if d.links.iter().any(|l| l.name == new || l.inverse == new) || d.object(&new).is_some() {
                        return Err(format!("{new} is used in the draft already"));
                    }
                    if let Some(lt) = schema.link_types.get(&new) {
                        let ours = lt.from.iter().any(|f| d.object(f).is_some());
                        if !ours {
                            return Err(format!("{new} is an existing link type; the draft must not replace it"));
                        }
                    }
                    d.links[idx].name = new.clone();
                    let draft_actions: Vec<String> = d.actions.iter().map(|a| a.name.clone()).collect();
                    for a in d.actions.iter_mut().filter(|a| a.link.as_deref() == Some(name.as_str())) {
                        a.link = Some(new.clone());
                        let renamed = format!("set_{new}");
                        if a.name == format!("set_{name}")
                            && !draft_actions.contains(&renamed)
                            && !schema.action_types.contains_key(&renamed)
                        {
                            a.name = renamed;
                        }
                    }
                }
                "inverse" => {
                    let new = valid_name(&to_text()?)?;
                    if d.links.iter().enumerate().any(|(i, l)| i != idx && (l.name == new || l.inverse == new)) {
                        return Err(format!("{new} is used in the draft already"));
                    }
                    d.links[idx].inverse = new;
                }
                "label" => d.links[idx].label = to_text()?,
                "description" => d.links[idx].description = to_text()?,
                other => return Err(format!("link types take name, inverse, label or description, not {other}")),
            }
        }
        "action" | "action_type" => {
            let idx = d.actions.iter().position(|a| a.name == name).ok_or(format!("no action type {name}"))?;
            match field.as_str() {
                "name" => {
                    let new = valid_name(&to_text()?)?;
                    if new == name {
                        return Ok(());
                    }
                    if d.actions.iter().any(|a| a.name == new) || schema.action_types.contains_key(&new) {
                        return Err(format!("an action type {new} exists already"));
                    }
                    d.actions[idx].name = new;
                }
                "description" => d.actions[idx].description = to_text()?,
                "risk" => {
                    d.actions[idx].risk = serde_json::from_value(json!(to_text()?.to_lowercase()))
                        .map_err(|_| "risk is low, medium or high".to_string())?;
                }
                other => return Err(format!("action types take name, description or risk, not {other}")),
            }
        }
        other => return Err(format!("unknown target kind {other} (object, property, link, action)")),
    }
    Ok(())
}

/// The plan errors in `after` that `before` does not have.
fn new_errors<'a>(before: &[String], after: &'a [String]) -> Vec<&'a String> {
    after.iter().filter(|e| !before.contains(e)).collect()
}

// ── the service ──────────────────────────────────────────────────────────

impl Intelli {
    /// A real model is configured (not the offline mock, not switched off).
    fn modeling_llm(&self) -> bool {
        !matches!(self.llm().name(), "mock" | "none") && self.llm().unavailable().is_none()
    }

    /// `ontology.draft`: a deterministic draft (no model, no tokens).
    pub fn draft_ontology(&self, p: &Principal, input: &DraftIn) -> Result<OntologyDraft> {
        let source = resolve_source(self.ontology(), p, input.source.as_deref(), "")?;
        self.ontology().draft_ontology(p, &input.options(source))
    }

    /// `ontology.refine`: the draft, improved by the model's suggestions that
    /// keep it a valid schema change.
    pub async fn refine_ontology(&self, p: &Principal, input: RefineIn) -> Result<Refined> {
        let mut draft = self.draft_ontology(p, &input.draft)?;
        let none = |mut draft: OntologyDraft, note: String| Refined {
            draft: {
                draft.notes.push(note);
                draft
            },
            suggestions: Vec::new(),
            applied: 0,
            rejected: Vec::new(),
            llm: "none".into(),
            tokens: 0,
        };
        if !self.modeling_llm() {
            let why = match self.llm().unavailable() {
                Some(reason) => format!("the language model is unavailable ({reason})"),
                None => format!("no language model is configured (llm provider {})", self.llm().name()),
            };
            return Ok(none(draft, format!("{why}: this is the deterministic draft, unrefined")));
        }
        if draft.objects.is_empty() {
            return Ok(none(draft, "nothing to refine: the draft has no object types".into()));
        }
        let mut user = String::from("Draft ontology:\n");
        user.push_str(&draft_summary(&draft));
        if let Some(i) = input.instructions.as_deref().map(str::trim).filter(|i| !i.is_empty()) {
            user.push_str(&format!("\nInstructions from the modeler: {i}\n"));
        }
        user.push_str("\nReply with the JSON object only.");
        let req = LlmRequest {
            system: REFINE_SYSTEM.into(),
            messages: vec![ChatMessage::user(user)],
            max_tokens: self.config().llm.max_output_tokens,
        };
        let (resp, cached) = match self.complete(&req, None, false).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(none(draft, format!("the language model failed ({e}): this is the deterministic draft")));
            }
        };
        let tokens = if cached { 0 } else { resp.usage.total() };
        let suggestions = parse_suggestions(&resp.text);
        if suggestions.is_empty() {
            draft.notes.push("the model's reply held no usable suggestions".into());
        }
        let schema = self.ontology().schema();
        let catalog = draft.clone();
        // A suggestion is kept when it adds no plan error to the draft as it
        // stands after the suggestions kept before it.
        let mut errors: Vec<String> = self.ontology().schema_plan(p, &draft.changes)?.errors;
        let mut applied = 0;
        let mut rejected = Vec::new();
        for s in &suggestions {
            let mut next = draft.clone();
            if let Err(error) = apply_suggestion(&mut next, s, &schema, &catalog) {
                rejected.push(Rejected { suggestion: s.clone(), error });
                continue;
            }
            next.rebuild();
            let plan = self.ontology().schema_plan(p, &next.changes)?;
            let new = new_errors(&errors, &plan.errors);
            if new.is_empty() {
                errors = plan.errors.clone();
                draft = next;
                applied += 1;
            } else {
                rejected.push(Rejected {
                    suggestion: s.clone(),
                    error: new.iter().map(|e| e.as_str()).collect::<Vec<_>>().join("; "),
                });
            }
        }
        Ok(Refined { draft, suggestions, applied, rejected, llm: self.llm().name().to_string(), tokens })
    }

    /// `ontology.advise`: modeling advice grounded in the draft.
    pub async fn advise_ontology(&self, p: &Principal, input: AdviseIn) -> Result<Advice> {
        if input.question.trim().is_empty() {
            return Err(Error::invalid("ontology.advise needs a question"));
        }
        let source = resolve_source(self.ontology(), p, input.source.as_deref(), &input.question)?;
        let draft = self.ontology().draft_ontology(p, &DraftOptions::for_source(source.clone()))?;
        let direct = advise_from_draft(&draft, &input.question);
        let q = words(&input.question);
        let reasoning = has_any(
            &q,
            &[
                " why ",
                " how should ",
                " recommend",
                " better ",
                " best ",
                " explain ",
                " trade",
                " instead ",
                " how would ",
            ],
        );
        let deterministic = |note: Option<String>| Advice {
            answer: direct.answer.clone(),
            mode: "deterministic".into(),
            facts: direct.facts.clone(),
            source: source.clone(),
            tokens: 0,
            note,
        };
        if !reasoning || !self.modeling_llm() {
            return Ok(deterministic(None));
        }
        let mut user = format!("Draft ontology:\n{}\nFacts:\n", draft_summary(&draft));
        for f in &direct.facts {
            user.push_str(&format!("- {f}\n"));
        }
        user.push_str(&format!("\nQuestion: {}", input.question.trim()));
        let req = LlmRequest {
            system: ADVISE_SYSTEM.into(),
            messages: vec![ChatMessage::user(user)],
            max_tokens: self.config().llm.max_output_tokens,
        };
        match self.complete(&req, None, false).await {
            Ok((resp, cached)) if !resp.text.trim().is_empty() => Ok(Advice {
                answer: resp.text,
                mode: "llm".into(),
                facts: direct.facts.clone(),
                source: source.clone(),
                tokens: if cached { 0 } else { resp.usage.total() },
                note: None,
            }),
            Ok(_) => Ok(deterministic(Some("the model returned no text".into()))),
            Err(e) => Ok(deterministic(Some(format!("the language model failed: {e}")))),
        }
    }

    /// `ontology.apply_draft`: apply the draft as a new schema version (apply
    /// roles), or file it as a proposal (anyone who may propose; the only
    /// way for agents).
    pub async fn apply_ontology_draft(&self, p: &Principal, input: ApplyDraftIn) -> Result<Value> {
        let mut draft = match input.draft.clone() {
            Some(mut d) => {
                if d.source.trim().is_empty() {
                    d.source = resolve_source(self.ontology(), p, input.options.source.as_deref(), "")?;
                }
                d
            }
            None => self.draft_ontology(p, &input.options)?,
        };
        if input.options.no_sync || input.options.sync == Some(false) {
            for o in &mut draft.objects {
                o.sync = false;
            }
        }
        draft.rebuild();
        if input.preview {
            return Ok(json!({ "mode": "preview", "source": draft.source, "draft": draft }));
        }
        if draft.changes.changes.is_empty() {
            return Err(Error::invalid(format!(
                "nothing to apply: the draft of {} has no object types{}",
                draft.source,
                if draft.notes.is_empty() { String::new() } else { format!(" ({})", draft.notes.join("; ")) }
            )));
        }
        let propose = input.propose || input.mode.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("propose"));
        if let Some(m) = input.mode.as_deref().filter(|m| !matches!(m.to_lowercase().as_str(), "apply" | "propose")) {
            return Err(Error::invalid(format!("mode '{m}': use apply or propose")));
        }
        let counts = json!({
            "objects": draft.objects.len(),
            "links": draft.links.len(),
            "actions": draft.actions.len(),
        });
        let ws = input.workspace.clone().filter(|w| !w.is_empty()).unwrap_or_else(|| self.default_workspace().into());
        let source = match &p.via {
            Some(service) => format!("api via {service}"),
            None => "api".to_string(),
        };
        let ctx = ActionContext::new(p.clone(), ws).with_source(source).with_intent("ontology.apply_draft");
        if propose {
            let title = input
                .title
                .clone()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| format!("Ontology for data source {}", draft.source));
            let rationale = input.rationale.clone().filter(|r| !r.trim().is_empty()).unwrap_or_else(|| {
                let names: Vec<String> = draft.objects.iter().map(|o| format!("{} ({})", o.name, o.table)).collect();
                format!(
                    "Drafted from the catalog of {}: object types {}; {} link types, {} action types.",
                    draft.source,
                    list(&names),
                    draft.links.len(),
                    draft.actions.len()
                )
            });
            let proposal = self.ontology().propose_schema(
                &ctx,
                ProposalInput {
                    title,
                    rationale,
                    change_set: draft.changes.clone(),
                    source: Some(input.proposal_source.clone().unwrap_or_else(|| "ontology.apply_draft".into())),
                },
            )?;
            return Ok(json!({ "mode": "proposed", "source": draft.source, "counts": counts, "proposal": proposal }));
        }
        let applied = match self.ontology().schema_apply(&ctx, draft.changes.clone()) {
            Ok(a) => a,
            Err(Error::Denied(m)) => {
                return Err(Error::Denied(format!(
                    "{m}; file the draft as a proposal instead (mode=propose, onto model apply --propose, MCP propose_ontology)"
                )));
            }
            Err(e) => return Err(e),
        };
        let mut out = json!({
            "mode": "applied",
            "source": draft.source,
            "counts": counts,
            "version": applied.version,
            "revision": applied.revision,
        });
        let synced = draft.objects.iter().any(|o| o.sync);
        if synced && !input.no_crawl {
            if self.connector_spec(&draft.source).is_ok() {
                match self.crawl(p, Some(&draft.source)).await {
                    Ok(reports) => out["crawl"] = serde_json::to_value(&reports)?,
                    Err(e) => {
                        out["hint"] =
                            json!(format!("rows were not synced ({e}); run onto crawl {} to sync them", draft.source))
                    }
                }
            } else {
                out["hint"] = json!(format!(
                    "no connector named {0} is registered here, so no rows were read; register it (onto connect) and run onto crawl {0} to sync objects",
                    draft.source
                ));
            }
        } else if synced {
            out["hint"] = json!(format!("run onto crawl {} to sync rows into objects", draft.source));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suggestion_is_kept_only_when_it_adds_no_error() {
        let e = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // The draft had two errors; a kept suggestion fixed both. A later one
        // that brings two others back is refused, though the count is the
        // first draft's.
        assert!(new_errors(&e(&["a", "b"]), &e(&[])).is_empty());
        assert_eq!(new_errors(&e(&[]), &e(&["c", "d"])), ["c", "d"]);
        assert_eq!(new_errors(&e(&["a", "b"]), &e(&["a", "c"])), ["c"], "one fixed, one new");
        assert!(new_errors(&e(&["a", "b"]), &e(&["b"])).is_empty());
    }

    #[test]
    fn suggestions_must_fit_the_rows() {
        use onto_core::modeling::{DraftObject, DraftProperty};
        let prop = |name: &str, values: &[&str], required: bool| DraftProperty {
            name: name.into(),
            column: name.into(),
            ty: if values.is_empty() { PropType::String } else { PropType::Enum },
            values: values.iter().map(|v| v.to_string()).collect(),
            required,
            ..Default::default()
        };
        let catalog = OntologyDraft {
            source: "shop".into(),
            objects: vec![DraftObject {
                name: "customer".into(),
                table: "sales.customers".into(),
                properties: vec![
                    prop("tier", &["gold", "silver"], false),
                    prop("region", &[], false),
                    prop("email", &[], true),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let schema = onto_core::Schema::default();
        let suggest = |target: &str, field: &str, to: Value| {
            let mut d = catalog.clone();
            let s = Suggestion { target: target.into(), field: field.into(), to, ..Default::default() };
            apply_suggestion(&mut d, &s, &schema, &catalog).map(|_| d)
        };
        let invented = suggest("property:customer.region", "values", json!(["north", "south"])).unwrap_err();
        assert!(invented.contains("no value list"), "{invented}");
        let fewer = suggest("property:customer.tier", "values", json!(["gold"])).unwrap_err();
        assert!(fewer.contains("missing silver"), "{fewer}");
        let more = suggest("property:customer.tier", "values", json!(["gold", "silver", "platinum"])).unwrap();
        assert_eq!(more.objects[0].properties[0].values, ["gold", "silver", "platinum"]);
        let nullable = suggest("property:customer.region", "required", json!(true)).unwrap_err();
        assert!(nullable.contains("nullable"), "{nullable}");
        assert!(suggest("property:customer.email", "required", json!(true)).is_ok(), "NOT NULL");
        assert!(suggest("property:customer.email", "required", json!(false)).is_ok(), "loosening is fine");
        // A renamed property is still found by its column.
        let mut d = catalog.clone();
        let rename = Suggestion {
            target: "property:customer.tier".into(),
            field: "name".into(),
            to: json!("level"),
            ..Default::default()
        };
        apply_suggestion(&mut d, &rename, &schema, &catalog).unwrap();
        let s = Suggestion {
            target: "property:customer.level".into(),
            field: "values".into(),
            to: json!(["gold"]),
            ..Default::default()
        };
        assert!(apply_suggestion(&mut d, &s, &schema, &catalog).is_err());
    }
}
