//! The reasoner: forward chaining over the visible graph, with one recorded
//! derivation per derived fact and a certificate an independent checker can
//! re-derive line by line.
//!
//! * **An inference is not an assertion.** Derived facts are never written to
//!   the stored graph. They live in a [`Closure`], are labelled `derived`, and
//!   carry the rule and the premises that produced them.
//! * **One derivation per fact, premises earlier.** Every derived fact has
//!   exactly one recorded [`Derivation`] (rule, conclusion, premises in the
//!   rule's body order), and every premise is asserted or concluded by an
//!   earlier line. Evaluation runs in rounds and appends a round's new
//!   conclusions only at its end, sorted by their text, which guarantees it.
//! * **Built-in versus supplied rules.** The schema's own semantics (kind
//!   inheritance, interfaces, transitive and symmetric link types) are the
//!   fixed [`BUILTIN_RULES`] table. Schema `[[rules]]` are supplied rules,
//!   named `schema/<name>`: assumptions an organization or an industry pack
//!   adds, which a certificate carries but does not establish. The engine only
//!   ever says `derived`; verdicts on a certificate belong to the checker.
//! * **Refuse rather than lie.** A term that cannot be spelled safely (a TAB,
//!   LF or CR anywhere, a `>` inside an entity id, a relation named like a
//!   keyword) is left out and reported in [`ReasonStats::refusals`]; such a
//!   closure yields no certificate.
//! * **A partial run says so.** When `max_rounds` or `max_derived` stops the
//!   run, `fixpoint_reached` is false and the closure is a lower bound; every
//!   report built on it carries the flag.
//! * **A clean run is not a consistency result.** [`ReasonStats::not_evaluated`]
//!   lists the schema constructs the reasoner does not evaluate, so silence is
//!   never read as "valid".
//! * **Determinism.** Facts are sorted before interning, rules run in a fixed
//!   order, and no hash-map iteration reaches the output: the same graph,
//!   schema and scope give the same bytes.
//!
//! Terms are spelled as in the `oi-cert/1` format: `<id>` for entities,
//! `#name` for kinds, interfaces and derived classes, bare names for
//! relations and keywords, `prop.<name>` for property predicates, compact JSON
//! for literal values and `?name` for rule variables.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::graph::Graph;
use crate::live_schema::valid_name;
use crate::model::Entity;
use crate::policy::{self, Principal};
use crate::schema::{Dir, Rule, Schema};
use crate::time::{fmt_ts, now_ms};
use crate::{Error, Ontology, Result};

/// Certificate format written by [`Closure::certificate`].
pub const FORMAT: &str = "oi-cert/1";

/// Words with a fixed meaning in facts. A relation with one of these names
/// cannot be reasoned over and is refused.
pub const KEYWORDS: &[&str] = &["a", "extends", "implements", "state", "transitive", "symmetric"];

/// The built-in rule table, byte for byte as it appears first in `rules.tsv`.
pub const BUILTIN_RULES: &str = concat!(
    "builtin/type_extends\t?x\ta\t?p\t?x\ta\t?k\t?k\textends\t?p\n",
    "builtin/extends_transitive\t?k\textends\t?q\t?k\textends\t?p\t?p\textends\t?q\n",
    "builtin/type_implements\t?x\ta\t?i\t?x\ta\t?k\t?k\timplements\t?i\n",
    "builtin/transitive\t?x\t?r\t?z\t?r\ta\ttransitive\t?x\t?r\t?y\t?y\t?r\t?z\n",
    "builtin/symmetric\t?y\t?r\t?x\t?r\ta\tsymmetric\t?x\t?r\t?y\n",
);

/// Name prefix of rules compiled from schema `[[rules]]`.
pub const SUPPLIED_PREFIX: &str = "schema/";

/// Schema constructs the reasoner does not evaluate. A closure says nothing
/// about whether the graph satisfies them.
pub const NOT_EVALUATED: &[&str] = &[
    "cardinality",
    "required_links",
    "link_endpoints",
    "property_types",
    "shapes",
    "metrics",
    "derived_properties",
    "quality_constraints",
    "action_preconditions",
];

/// What a certificate's manifest says about its own fields.
pub const MANIFEST_MEANS: &str = "derived fields are recomputed by the checker; attested fields are claims of the engine that wrote this certificate";

const BINDING_MEANS: &str = "A digest binds a certificate to bytes, not to a state of the world. same_graph means the \
     store, read now under this scope, yields exactly the certificate's asserted facts; it says nothing about the \
     store when the certificate was written, nor about facts outside the scope.";

/// Refusals listed (all of them are counted).
const MAX_REFUSALS_LISTED: usize = 20;
/// Matches a query scans before it stops; an answer cut there says so.
const QUERY_SCAN_CAP: usize = 100_000;
/// Closures kept per ontology.
const CACHE_SIZE: usize = 4;

// ── facts and rules ──────────────────────────────────────────────────────

/// A triple of spelled terms.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Fact {
    pub s: String,
    pub p: String,
    pub o: String,
}

impl Fact {
    pub fn new(s: impl Into<String>, p: impl Into<String>, o: impl Into<String>) -> Self {
        Self { s: s.into(), p: p.into(), o: o.into() }
    }

    /// `s<TAB>p<TAB>o`, without a line end.
    pub fn tsv(&self) -> String {
        format!("{}\t{}\t{}", self.s, self.p, self.o)
    }
}

impl fmt::Display for Fact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", self.s, self.p, self.o)
    }
}

/// How one derived fact was concluded: a rule and its premises, in the
/// rule's body order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Derivation {
    pub rule: String,
    pub conclusion: Fact,
    pub premises: Vec<Fact>,
}

/// Whether a fact is in a closure, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Asserted,
    /// Concluded by `derivations.tsv` line `line` (1-based).
    Derived {
        line: usize,
    },
}

/// A term the reasoner would not spell, and why.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Refusal {
    /// The offending text, escaped and shortened.
    pub term: String,
    pub reason: String,
}

/// One rule as spelled terms: `?name` variables and constant terms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleLine {
    pub name: String,
    pub head: [String; 3],
    pub body: Vec<[String; 3]>,
}

impl RuleLine {
    /// The `rules.tsv` line, without a line end.
    pub fn tsv(&self) -> String {
        let mut parts = vec![self.name.as_str()];
        parts.extend(self.head.iter().map(String::as_str));
        for atom in &self.body {
            parts.extend(atom.iter().map(String::as_str));
        }
        parts.join("\t")
    }

    pub fn is_builtin(&self) -> bool {
        self.name.starts_with("builtin/")
    }

    fn parse(line: &str) -> Option<RuleLine> {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 7 || !(f.len() - 1).is_multiple_of(3) {
            return None;
        }
        let triple = |i: usize| [f[i].to_string(), f[i + 1].to_string(), f[i + 2].to_string()];
        Some(RuleLine { name: f[0].to_string(), head: triple(1), body: (4..f.len()).step_by(3).map(triple).collect() })
    }
}

/// The built-in table as rule lines.
pub fn builtin_rules() -> Vec<RuleLine> {
    BUILTIN_RULES.lines().filter_map(RuleLine::parse).collect()
}

/// Classes and relations that schema rules derive (beyond the declared
/// kinds, interfaces and link types).
#[derive(Clone, Debug, Default)]
pub struct Vocabulary {
    pub classes: BTreeSet<String>,
    pub relations: BTreeSet<String>,
}

/// The derived vocabulary of `schema`'s rules.
pub fn vocabulary(schema: &Schema) -> Vocabulary {
    let mut v = Vocabulary::default();
    for r in &schema.rules {
        if r.is_horn() {
            match (r.then.get(1).and_then(Value::as_str), r.then.get(2).and_then(Value::as_str)) {
                (Some("a"), Some(class)) if !class.starts_with('?') => {
                    v.classes.insert(class.to_string());
                }
                (Some(rel), _) if rel != "a" && !rel.starts_with(['?', '^']) && schema.resolve_rel(rel).is_none() => {
                    v.relations.insert(rel.to_string());
                }
                _ => {}
            }
        } else if !r.infer.is_empty() && schema.resolve_rel(&r.infer).is_none() {
            v.relations.insert(r.infer.trim_start_matches('^').to_string());
        }
    }
    v
}

// ── spelling ─────────────────────────────────────────────────────────────

fn unsafe_text(t: &str) -> Option<&'static str> {
    if t.is_empty() {
        Some("empty")
    } else if t.contains('\t') {
        Some("contains a TAB")
    } else if t.contains('\n') {
        Some("contains a line feed")
    } else if t.contains('\r') {
        Some("contains a carriage return")
    } else {
        None
    }
}

/// `<id>`, refusing ids that cannot be spelled.
pub fn spell_entity(id: &str) -> std::result::Result<String, String> {
    if let Some(why) = unsafe_text(id) {
        return Err(format!("entity id {why}"));
    }
    if id.contains('>') {
        return Err("entity id contains '>'".into());
    }
    Ok(format!("<{id}>"))
}

/// `#name` for a kind, interface or derived class.
pub fn spell_class(name: &str) -> std::result::Result<String, String> {
    match unsafe_text(name) {
        Some(why) => Err(format!("class name {why}")),
        None => Ok(format!("#{name}")),
    }
}

/// A relation, bare. Keywords and names that would read as another kind of
/// term are refused.
pub fn spell_relation(name: &str) -> std::result::Result<String, String> {
    if let Some(why) = unsafe_text(name) {
        return Err(format!("relation name {why}"));
    }
    if KEYWORDS.contains(&name) {
        return Err(format!("relation name '{name}' is a reserved keyword"));
    }
    if name.starts_with(['?', '<', '#', '"', '^']) || name.starts_with("prop.") {
        return Err(format!("relation name '{name}' reads as another kind of term"));
    }
    Ok(name.to_string())
}

fn spell_prop(name: &str) -> std::result::Result<String, String> {
    match unsafe_text(name) {
        Some(why) => Err(format!("property name {why}")),
        None => Ok(format!("prop.{name}")),
    }
}

/// Compact JSON; never refused (JSON escapes control characters).
fn spell_literal(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

fn shown(t: &str) -> String {
    let s = format!("{t:?}");
    if s.chars().count() > 120 { format!("{}…", s.chars().take(119).collect::<String>()) } else { s }
}

// ── rule compilation ─────────────────────────────────────────────────────

/// Why a rule or an atom did not compile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    /// The rule is wrong: schema compile rejects it.
    Invalid(String),
    /// The rule is well formed but names a term that cannot be spelled (a
    /// reserved keyword, an unsafe character). Schema compile accepts it; the
    /// reasoner refuses it and reports it under `refused`.
    Refused(String),
}

impl CompileError {
    fn context(self, at: impl fmt::Display) -> Self {
        match self {
            CompileError::Invalid(m) => CompileError::Invalid(format!("{at}: {m}")),
            CompileError::Refused(m) => CompileError::Refused(format!("{at}: {m}")),
        }
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::Invalid(m) | CompileError::Refused(m) => f.write_str(m),
        }
    }
}

impl From<String> for CompileError {
    fn from(m: String) -> Self {
        CompileError::Invalid(m)
    }
}

impl From<&str> for CompileError {
    fn from(m: &str) -> Self {
        CompileError::Invalid(m.to_string())
    }
}

type Compiled<T> = std::result::Result<T, CompileError>;

/// A spelling failure is a refusal, not a schema error.
fn spelled(r: std::result::Result<String, String>) -> Compiled<String> {
    r.map_err(CompileError::Refused)
}

fn is_var(t: &str) -> bool {
    t.strip_prefix('?').is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

fn var_of(v: &Value) -> Compiled<Option<String>> {
    match v.as_str() {
        Some(s) if s.starts_with('?') => {
            if is_var(s) {
                Ok(Some(s.to_string()))
            } else {
                Err(format!("bad variable {s:?} (use ?name with letters, digits and _)").into())
            }
        }
        _ => Ok(None),
    }
}

/// A subject or a relation's object: a variable or an entity id.
fn node_term(v: &Value, what: &str) -> Compiled<String> {
    if let Some(var) = var_of(v)? {
        return Ok(var);
    }
    match v.as_str() {
        Some(id) if id.contains(':') => spelled(spell_entity(id)),
        _ => Err(format!("{what} must be a ?variable or an entity id (kind:key), got {v}").into()),
    }
}

/// Compile one `[subject, predicate, object]` atom of a rule, a rule head
/// (`head`) or a query into spelled terms. Inverse relation names and `^rel`
/// become the canonical relation with subject and object swapped.
pub fn compile_atom(schema: &Schema, vocab: &Vocabulary, atom: &[Value], head: bool) -> Compiled<[String; 3]> {
    if atom.len() != 3 {
        return Err(format!("an atom has three parts [subject, predicate, object], got {}", atom.len()).into());
    }
    let Some(pred) = atom[1].as_str() else {
        return Err(format!("predicate must be a string, got {}", atom[1]).into());
    };
    let subject = node_term(&atom[0], "subject")?;
    if pred.starts_with('?') {
        if head {
            return Err("then needs a fixed predicate (a or a relation)".into());
        }
        let p = var_of(&atom[1])?.unwrap_or_default();
        return Ok([subject, p, node_term(&atom[2], "object")?]);
    }
    if pred == "a" {
        if let Some(var) = var_of(&atom[2])? {
            return Ok([subject, "a".into(), var]);
        }
        let Some(class) = atom[2].as_str() else {
            return Err(format!("the object of a must be a class name, got {}", atom[2]).into());
        };
        if head {
            if !valid_name(class) {
                return Err(format!("derived class name '{class}' must match [a-z][a-z0-9_]*").into());
            }
        } else if !schema.is_type_or_interface(class) && !vocab.classes.contains(class) {
            return Err(format!("unknown class {class} (not a kind, an interface or a class a rule derives)").into());
        }
        return Ok([subject, "a".into(), spelled(spell_class(class))?]);
    }
    if pred == "state" || pred.starts_with("prop.") {
        if head {
            return Err("rules derive types and relations, not property values".into());
        }
        let p = if pred == "state" {
            "state".to_string()
        } else {
            let name = &pred["prop.".len()..];
            let known = schema.object_types.values().any(|t| t.properties.contains_key(name))
                || schema.interfaces.values().any(|i| i.properties.contains_key(name));
            if !known {
                return Err(format!("unknown property {name}").into());
            }
            spelled(spell_prop(name))?
        };
        let o = match var_of(&atom[2])? {
            Some(var) => var,
            None => match &atom[2] {
                v @ (Value::String(_) | Value::Number(_) | Value::Bool(_)) => spell_literal(v),
                v => return Err(format!("the value of {pred} must be a ?variable or a scalar, got {v}").into()),
            },
        };
        return Ok([subject, p, o]);
    }
    let object = node_term(&atom[2], "object")?;
    let (base, swap) = match schema.resolve_rel(pred) {
        Some((base, dir)) => (base, dir == Dir::In),
        None => {
            let name = pred.strip_prefix('^').unwrap_or(pred);
            if vocab.relations.contains(name) {
                (name.to_string(), name != pred)
            } else {
                return Err(format!("unknown relation {pred}").into());
            }
        }
    };
    if head && !schema.link_types.contains_key(&base) && !valid_name(&base) {
        return Err(format!("derived relation name '{base}' must match [a-z][a-z0-9_]*").into());
    }
    let base = spelled(spell_relation(&base))?;
    Ok(if swap { [object, base, subject] } else { [subject, base, object] })
}

/// Compile a schema rule (either form) to its rule line `schema/<name>`.
/// [`CompileError::Invalid`] is a schema error; [`CompileError::Refused`]
/// means the rule is well formed but the reasoner cannot spell one of its
/// terms (for example a head relation named like a keyword).
pub fn compile_rule(schema: &Schema, vocab: &Vocabulary, rule: &Rule) -> Compiled<RuleLine> {
    if let Some(why) = unsafe_text(&rule.name) {
        return Err(CompileError::Refused(format!("rule name {why}")));
    }
    let name = format!("{SUPPLIED_PREFIX}{}", rule.name);
    if rule.is_horn() {
        if rule.when.is_empty() {
            return Err("when needs at least one atom".into());
        }
        if rule.then.len() != 3 {
            return Err("then must be one atom [subject, predicate, object]".into());
        }
        let mut body = Vec::with_capacity(rule.when.len());
        for (i, atom) in rule.when.iter().enumerate() {
            body.push(compile_atom(schema, vocab, atom, false).map_err(|e| e.context(format_args!("when[{i}]")))?);
        }
        let head = compile_atom(schema, vocab, &rule.then, true).map_err(|e| e.context("then"))?;
        let bound: BTreeSet<&str> = body.iter().flatten().map(String::as_str).filter(|t| is_var(t)).collect();
        if let Some(v) = head.iter().find(|t| is_var(t) && !bound.contains(t.as_str())) {
            return Err(format!("head variable {v} does not occur in when").into());
        }
        return Ok(RuleLine { name, head, body });
    }
    if rule.chain.is_empty() {
        return Err("empty chain".into());
    }
    let var = |i: usize| format!("?x{i}");
    let mut body = Vec::with_capacity(rule.chain.len());
    for (i, step) in rule.chain.iter().enumerate() {
        let (base, dir) = match schema.resolve_rel(step) {
            Some(r) => r,
            None => {
                let name = step.strip_prefix('^').unwrap_or(step);
                if !vocab.relations.contains(name) {
                    return Err(format!("unknown relation {step}").into());
                }
                (name.to_string(), if name == step { Dir::Out } else { Dir::In })
            }
        };
        let base = spelled(spell_relation(&base))?;
        body.push(if dir == Dir::In { [var(i + 1), base, var(i)] } else { [var(i), base, var(i + 1)] });
    }
    let n = rule.chain.len();
    let head = match schema.resolve_rel(&rule.infer) {
        Some((base, Dir::In)) => [var(n), spelled(spell_relation(&base))?, var(0)],
        Some((base, _)) => [var(0), spelled(spell_relation(&base))?, var(n)],
        // A derived relation: `^foo` derives foo from the end back to the start.
        None => match rule.infer.strip_prefix('^') {
            Some(base) => [var(n), spelled(spell_relation(base))?, var(0)],
            None => [var(0), spelled(spell_relation(&rule.infer))?, var(n)],
        },
    };
    Ok(RuleLine { name, head, body })
}

/// The rule table of `schema`: the built-in lines, then every schema rule
/// that compiles (in schema order), plus the rules refused and why.
pub fn rule_table(schema: &Schema) -> (Vec<RuleLine>, Vec<Refusal>) {
    let vocab = vocabulary(schema);
    let mut lines = builtin_rules();
    let mut refused = Vec::new();
    for r in &schema.rules {
        match compile_rule(schema, &vocab, r) {
            Ok(l) => lines.push(l),
            Err(e) => refused.push(Refusal { term: shown(&r.name), reason: format!("rule {}: {e}", r.name) }),
        }
    }
    (lines, refused)
}

/// Parse query atoms written like rule `when` atoms.
pub fn parse_atoms(schema: &Schema, atoms: &[Vec<Value>]) -> Result<Vec<AtomPattern>> {
    let vocab = vocabulary(schema);
    atoms
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let [s, p, o] =
                compile_atom(schema, &vocab, a, false).map_err(|e| Error::invalid(format!("atom {i}: {e}")))?;
            Ok(AtomPattern { s, p, o })
        })
        .collect()
}

// ── asserted facts ───────────────────────────────────────────────────────

/// What the supplied rules read beyond kinds and links.
struct Needs {
    state: bool,
    props: BTreeSet<String>,
}

fn needs(lines: &[RuleLine]) -> Needs {
    let mut n = Needs { state: false, props: BTreeSet::new() };
    for atom in lines.iter().filter(|l| !l.is_builtin()).flat_map(|l| l.body.iter()) {
        let p = atom[1].as_str();
        if p == "state" || is_var(p) {
            n.state = true;
        }
        if let Some(name) = p.strip_prefix("prop.") {
            n.props.insert(name.to_string());
        }
    }
    n
}

struct Collected {
    facts: Vec<Fact>,
    refusals: BTreeSet<Refusal>,
}

/// The asserted facts of section 1 of the format: schema facts always,
/// entity facts for visible entities in scope, links between them.
fn collect(
    schema: &Schema,
    g: &Graph,
    workspace: Option<&str>,
    lines: &[RuleLine],
    can_read: &dyn Fn(&Entity) -> bool,
) -> Collected {
    let need = needs(lines);
    let mut facts = Vec::new();
    let mut refusals = BTreeSet::new();
    let mut refuse = |term: &str, reason: String| {
        refusals.insert(Refusal { term: shown(term), reason });
    };
    for (name, ty) in &schema.object_types {
        let parents = ty.extends.iter().map(|p| ("extends", p)).chain(ty.implements.iter().map(|i| ("implements", i)));
        for (kw, parent) in parents {
            match (spell_class(name), spell_class(parent)) {
                (Ok(c), Ok(p)) => facts.push(Fact::new(c, kw, p)),
                (Err(e), _) => refuse(name, e),
                (_, Err(e)) => refuse(parent, e),
            }
        }
    }
    for (name, lt) in &schema.link_types {
        for (on, kw) in [(lt.transitive, "transitive"), (lt.symmetric, "symmetric")] {
            if on {
                match spell_relation(name) {
                    Ok(r) => facts.push(Fact::new(r, "a", kw)),
                    Err(e) => refuse(name, e),
                }
            }
        }
    }
    let mut visible: HashMap<&str, String> = HashMap::new();
    for e in g.entities() {
        if workspace.is_some_and(|w| w != e.workspace) || !can_read(e) {
            continue;
        }
        let id = match spell_entity(&e.id) {
            Ok(id) => id,
            Err(why) => {
                refuse(&e.id, why);
                continue;
            }
        };
        match spell_class(&e.kind) {
            Ok(k) => facts.push(Fact::new(id.clone(), "a", k)),
            Err(why) => refuse(&e.kind, why),
        }
        if need.state
            && let Some(s) = &e.state
        {
            facts.push(Fact::new(id.clone(), "state", spell_literal(&Value::String(s.clone()))));
        }
        for p in &need.props {
            if let Some(v) = e.props.get(p).filter(|v| !v.is_null()) {
                facts.push(Fact::new(id.clone(), format!("prop.{p}"), spell_literal(v)));
            }
        }
        visible.insert(e.id.as_str(), id);
    }
    for l in g.links() {
        let (Some(from), Some(to)) = (visible.get(l.from.as_str()), visible.get(l.to.as_str())) else { continue };
        match spell_relation(&l.rel) {
            Ok(rel) => facts.push(Fact::new(from.clone(), rel, to.clone())),
            Err(why) => refuse(&l.rel, why),
        }
    }
    facts.sort();
    facts.dedup();
    Collected { facts, refusals }
}

/// `asserted.tsv` for a sorted, unique list of facts.
fn facts_tsv(facts: &[Fact]) -> String {
    let mut out = String::new();
    for f in facts {
        out.push_str(&f.tsv());
        out.push('\n');
    }
    out
}

// ── evaluation ───────────────────────────────────────────────────────────

/// Interned triple.
type Triple = [u32; 3];

#[derive(Clone, Copy, Debug)]
enum Tm {
    Var(usize),
    Con(u32),
}

struct CRule {
    head: [Tm; 3],
    body: Vec<[Tm; 3]>,
    vars: Vec<String>,
    /// Join order of the body for each body position matched against the delta.
    plans: Vec<Vec<usize>>,
}

#[derive(Default)]
struct Interner {
    ids: HashMap<String, u32>,
    terms: Vec<String>,
}

impl Interner {
    fn id(&mut self, t: &str) -> u32 {
        if let Some(&i) = self.ids.get(t) {
            return i;
        }
        let i = self.terms.len() as u32;
        self.terms.push(t.to_string());
        self.ids.insert(t.to_string(), i);
        i
    }

    fn triple(&mut self, f: &Fact) -> Triple {
        [self.id(&f.s), self.id(&f.p), self.id(&f.o)]
    }
}

/// Atom terms against the variables of one rule or query.
fn compile_terms(
    atom: &[String; 3],
    vars: &mut Vec<String>,
    terms: &mut dyn FnMut(&str) -> Option<u32>,
) -> Option<[Tm; 3]> {
    let mut out = [Tm::Con(0); 3];
    for (i, t) in atom.iter().enumerate() {
        out[i] = if is_var(t) {
            let v = match vars.iter().position(|x| x == t) {
                Some(v) => v,
                None => {
                    vars.push(t.clone());
                    vars.len() - 1
                }
            };
            Tm::Var(v)
        } else {
            Tm::Con(terms(t)?)
        };
    }
    Some(out)
}

/// Greedy join order: start with `first` (when given), then repeatedly the
/// atom with the most known positions (subject and object count double).
fn plan_order(body: &[[Tm; 3]], first: Option<usize>, nvars: usize) -> Vec<usize> {
    let mut bound = vec![false; nvars];
    let mut used = vec![false; body.len()];
    let mut order = Vec::with_capacity(body.len());
    let take = |j: usize, bound: &mut Vec<bool>, used: &mut Vec<bool>, order: &mut Vec<usize>| {
        used[j] = true;
        order.push(j);
        for t in body[j] {
            if let Tm::Var(v) = t {
                bound[v] = true;
            }
        }
    };
    if let Some(i) = first {
        take(i, &mut bound, &mut used, &mut order);
    }
    while order.len() < body.len() {
        let score = |j: usize| -> usize {
            body[j]
                .iter()
                .enumerate()
                .map(|(pos, t)| {
                    let known = match t {
                        Tm::Con(_) => true,
                        Tm::Var(v) => bound[*v],
                    };
                    if !known {
                        0
                    } else if pos == 1 {
                        1
                    } else {
                        2
                    }
                })
                .sum()
        };
        let best = (0..body.len()).filter(|j| !used[*j]).max_by_key(|j| (score(*j), std::cmp::Reverse(*j)));
        let Some(j) = best else { break };
        take(j, &mut bound, &mut used, &mut order);
    }
    order
}

/// Facts with lookups by predicate, subject, object and pairs. Every list
/// holds fact indices in ascending order.
#[derive(Default)]
struct Index {
    facts: Vec<Triple>,
    set: HashMap<Triple, u32>,
    by_p: HashMap<u32, Vec<u32>>,
    by_s: HashMap<u32, Vec<u32>>,
    by_o: HashMap<u32, Vec<u32>>,
    by_ps: HashMap<(u32, u32), Vec<u32>>,
    by_po: HashMap<(u32, u32), Vec<u32>>,
}

enum Cand<'a> {
    One(Option<u32>),
    List(&'a [u32]),
    All,
}

impl Index {
    fn push(&mut self, f: Triple) -> u32 {
        let i = self.facts.len() as u32;
        self.facts.push(f);
        self.set.insert(f, i);
        self.by_p.entry(f[1]).or_default().push(i);
        self.by_s.entry(f[0]).or_default().push(i);
        self.by_o.entry(f[2]).or_default().push(i);
        self.by_ps.entry((f[1], f[0])).or_default().push(i);
        self.by_po.entry((f[1], f[2])).or_default().push(i);
        i
    }

    fn candidates(&self, s: Option<u32>, p: Option<u32>, o: Option<u32>) -> Cand<'_> {
        fn list(l: Option<&Vec<u32>>) -> Cand<'_> {
            Cand::List(l.map(Vec::as_slice).unwrap_or(&[]))
        }
        match (s, p, o) {
            (Some(s), Some(p), Some(o)) => Cand::One(self.set.get(&[s, p, o]).copied()),
            (Some(s), Some(p), None) => list(self.by_ps.get(&(p, s))),
            (None, Some(p), Some(o)) => list(self.by_po.get(&(p, o))),
            (Some(s), None, _) => list(self.by_s.get(&s)),
            (None, None, Some(o)) => list(self.by_o.get(&o)),
            (None, Some(p), None) => list(self.by_p.get(&p)),
            (None, None, None) => Cand::All,
        }
    }
}

/// Receives each binding and the fact matched per atom; returns false to stop.
type Emit<'e> = dyn FnMut(&[Option<u32>], &[u32]) -> bool + 'e;

/// One join over `atoms` in `order`; the first atom ranges over fact indices
/// `first`, the others over `rest`.
struct Join<'a> {
    ix: &'a Index,
    atoms: &'a [[Tm; 3]],
    order: &'a [usize],
    first: (u32, u32),
    rest: (u32, u32),
}

impl Join<'_> {
    /// Calls `emit` with every binding and the matched fact per atom; stops
    /// (returning false) when `emit` returns false.
    fn run(&self, k: usize, binds: &mut [Option<u32>], matched: &mut [u32], emit: &mut Emit<'_>) -> bool {
        if k == self.order.len() {
            return emit(binds, matched);
        }
        let ai = self.order[k];
        let atom = self.atoms[ai];
        let val = |t: Tm, binds: &[Option<u32>]| match t {
            Tm::Con(c) => Some(c),
            Tm::Var(v) => binds[v],
        };
        let (lo, hi) = if k == 0 { self.first } else { self.rest };
        let mut attempt = |idx: u32, binds: &mut [Option<u32>], matched: &mut [u32]| -> bool {
            let f = self.ix.facts[idx as usize];
            let mut newly = [usize::MAX; 3];
            let mut ok = true;
            for pos in 0..3 {
                match atom[pos] {
                    Tm::Con(c) => ok = f[pos] == c,
                    Tm::Var(v) => match binds[v] {
                        Some(b) => ok = b == f[pos],
                        None => {
                            binds[v] = Some(f[pos]);
                            newly[pos] = v;
                        }
                    },
                }
                if !ok {
                    break;
                }
            }
            let go = !ok || {
                matched[ai] = idx;
                self.run(k + 1, binds, matched, emit)
            };
            for v in newly.into_iter().filter(|v| *v != usize::MAX) {
                binds[v] = None;
            }
            go
        };
        match self.ix.candidates(val(atom[0], binds), val(atom[1], binds), val(atom[2], binds)) {
            Cand::One(Some(idx)) if (lo..hi).contains(&idx) => attempt(idx, binds, matched),
            Cand::One(_) => true,
            Cand::List(l) => {
                let a = l.partition_point(|&i| i < lo);
                let b = l.partition_point(|&i| i < hi);
                l[a..b].iter().all(|&idx| attempt(idx, binds, matched))
            }
            Cand::All => (lo..hi).all(|idx| attempt(idx, binds, matched)),
        }
    }
}

fn instantiate(head: &[Tm; 3], binds: &[Option<u32>]) -> Triple {
    let v = |t: Tm| match t {
        Tm::Con(c) => c,
        Tm::Var(v) => binds[v].unwrap_or(u32::MAX),
    };
    [v(head[0]), v(head[1]), v(head[2])]
}

/// Limits and scope of one reasoning run.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasonOptions {
    /// Only entities of this workspace (schema facts always).
    pub workspace: Option<String>,
    /// Stop after this many derived facts (the closure is then a lower bound).
    pub max_derived: usize,
    /// Stop after this many rounds (the closure is then a lower bound).
    pub max_rounds: usize,
    /// Record every rule instance (a derivation DAG) for explanations.
    pub capture_all: bool,
    /// At most this many instances are recorded with `capture_all`.
    pub max_instances: usize,
}

impl Default for ReasonOptions {
    fn default() -> Self {
        Self {
            workspace: None,
            max_derived: 1_000_000,
            max_rounds: 10_000,
            capture_all: false,
            max_instances: 2_000_000,
        }
    }
}

/// Counts and flags of one run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReasonStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub asserted: usize,
    pub derived: usize,
    /// Lines in the rule table (built-in and supplied).
    pub rules: usize,
    /// Derived facts per rule (every rule of the table, zero included).
    pub by_rule: BTreeMap<String, usize>,
    pub rounds: usize,
    /// False when a limit stopped the run: the closure is a lower bound.
    pub fixpoint_reached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
    /// Terms and rules left out because they cannot be spelled safely.
    pub refused: usize,
    /// The first refusals (at most 20), sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refusals: Vec<Refusal>,
    /// Schema constructs this run did not evaluate.
    pub not_evaluated: Vec<String>,
    /// Rule instances recorded (with `capture_all`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instances: Option<usize>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub instances_truncated: bool,
}

/// A query atom of spelled terms (`?name` for variables).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtomPattern {
    pub s: String,
    pub p: String,
    pub o: String,
}

/// Variable name (without `?`) → spelled term.
pub type Binding = BTreeMap<String, String>;

/// Claims of the engine that wrote a certificate.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Attested {
    pub engine: String,
    pub created_at: String,
    pub workspace: Option<String>,
    pub schema_version: u64,
    pub graph_revision: u64,
    pub principal: String,
    pub fixpoint_reached: bool,
    pub rounds: usize,
}

/// The four files of an `oi-cert/1` certificate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Certificate {
    pub rules_tsv: String,
    pub asserted_tsv: String,
    pub derivations_tsv: String,
    pub manifest_json: String,
}

impl Certificate {
    /// `(file name, contents)` in a fixed order.
    pub fn files(&self) -> [(&'static str, &str); 4] {
        [
            ("rules.tsv", &self.rules_tsv),
            ("asserted.tsv", &self.asserted_tsv),
            ("derivations.tsv", &self.derivations_tsv),
            ("manifest.json", &self.manifest_json),
        ]
    }

    /// Write the files into `dir` (created when missing).
    pub fn write_to(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).map_err(|e| Error::Storage(format!("{}: {e}", dir.display())))?;
        for (name, text) in self.files() {
            let path = dir.join(name);
            std::fs::write(&path, text).map_err(|e| Error::Storage(format!("{}: {e}", path.display())))?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: String,
    derived: ManifestDerived,
    attested: Attested,
    means: String,
}

#[derive(Serialize, Deserialize)]
struct ManifestDerived {
    asserted_sha256: String,
    rules_sha256: String,
    derivations_sha256: String,
    asserted: usize,
    rules: usize,
    derivations: usize,
}

/// Hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// A fact one closure has and another has not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddedFact {
    pub fact: Fact,
    /// The rule that derived it (none when asserted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub premises: Vec<Fact>,
}

/// Facts that appear and disappear from one closure to another.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosureDiff {
    pub added: Vec<AddedFact>,
    pub removed: Vec<Fact>,
    /// False when either closure is a lower bound (then so is this diff).
    pub fixpoint_reached: bool,
}

impl ClosureDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// Is a certificate's `asserted.tsv` what the store yields now?
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingVerdict {
    SameGraph,
    DifferentGraph,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingReport {
    pub verdict: BindingVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub certificate_facts: usize,
    pub store_facts: usize,
    pub shared: usize,
    pub only_in_certificate: usize,
    pub only_in_store: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample_only_in_certificate: Vec<Fact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample_only_in_store: Vec<Fact>,
    /// sha256 of the given `asserted.tsv` bytes.
    pub certificate_sha256: String,
    /// sha256 of `asserted.tsv` as the store yields it now.
    pub store_sha256: String,
    /// Do the given bytes match the manifest's `asserted_sha256` (when given)?
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_digest_matches: Option<bool>,
    /// Terms the store holds that could not be spelled.
    pub refused: usize,
    pub means: String,
}

/// Bindings of a query over a closure.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryAnswer {
    pub bindings: Vec<Binding>,
    /// False when the closure is a lower bound (more bindings may exist).
    pub fixpoint_reached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

struct Instance {
    rule: u16,
    conclusion: Triple,
    premises: Vec<u32>,
}

/// The asserted facts in scope plus everything the rule table derives from
/// them, each derived fact with one recorded derivation.
pub struct Closure {
    rules: Vec<RuleLine>,
    terms: Interner,
    ix: Index,
    asserted: Vec<Fact>,
    derived: Vec<Derivation>,
    instances: Vec<Instance>,
    instances_by: HashMap<Triple, Vec<u32>>,
    stats: ReasonStats,
}

impl fmt::Debug for Closure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Closure").field("stats", &self.stats).finish_non_exhaustive()
    }
}

/// Run the reasoner over the entities `can_read` admits (and, with a
/// workspace, only that workspace's).
pub fn closure(schema: &Schema, g: &Graph, opts: &ReasonOptions, can_read: &dyn Fn(&Entity) -> bool) -> Closure {
    let (rules, refused_rules) = rule_table(schema);
    let collected = collect(schema, g, opts.workspace.as_deref(), &rules, can_read);
    let mut refusals = collected.refusals;
    refusals.extend(refused_rules);
    run(rules, collected.facts, refusals, opts)
}

fn run(rules: Vec<RuleLine>, asserted: Vec<Fact>, refusals: BTreeSet<Refusal>, opts: &ReasonOptions) -> Closure {
    let mut terms = Interner::default();
    let mut ix = Index::default();
    for f in &asserted {
        let t = terms.triple(f);
        ix.push(t);
    }
    let asserted_n = ix.facts.len() as u32;
    let mut crules = Vec::with_capacity(rules.len());
    for line in &rules {
        let mut vars = Vec::new();
        let mut intern = |t: &str| Some(terms.id(t));
        let body: Vec<[Tm; 3]> = line.body.iter().filter_map(|a| compile_terms(a, &mut vars, &mut intern)).collect();
        let head = compile_terms(&line.head, &mut vars, &mut intern).unwrap_or([Tm::Con(0); 3]);
        let plans = (0..body.len()).map(|i| plan_order(&body, Some(i), vars.len())).collect();
        crules.push(CRule { head, body, vars, plans });
    }

    let mut derivs: Vec<(u16, u32, Vec<u32>)> = Vec::new();
    let mut instances: Vec<Instance> = Vec::new();
    let mut seen_instances: HashSet<(u16, Vec<u32>)> = HashSet::new();
    let mut instances_truncated = false;
    let mut truncated = None;
    let mut rounds = 0;
    let mut delta = (0u32, asserted_n);
    while delta.0 < delta.1 {
        if rounds >= opts.max_rounds {
            truncated = Some(format!("stopped after max_rounds = {}", opts.max_rounds));
            break;
        }
        rounds += 1;
        let end = delta.1;
        let mut fresh: Vec<(Triple, u16, Vec<u32>)> = Vec::new();
        let mut fresh_set: HashSet<Triple> = HashSet::new();
        for (ri, rule) in crules.iter().enumerate() {
            let ri = ri as u16;
            let mut binds = vec![None; rule.vars.len()];
            let mut matched = vec![0u32; rule.body.len()];
            for plan in &rule.plans {
                let join = Join { ix: &ix, atoms: &rule.body, order: plan, first: (delta.0, end), rest: (0, end) };
                join.run(0, &mut binds, &mut matched, &mut |b, m| {
                    let c = instantiate(&rule.head, b);
                    let known = ix.set.get(&c).copied();
                    if opts.capture_all
                        && !instances_truncated
                        && known.is_none_or(|i| i >= asserted_n)
                        && seen_instances.insert((ri, m.to_vec()))
                    {
                        if instances.len() >= opts.max_instances {
                            instances_truncated = true;
                        } else {
                            instances.push(Instance { rule: ri, conclusion: c, premises: m.to_vec() });
                        }
                    }
                    if known.is_none() && fresh_set.insert(c) {
                        fresh.push((c, ri, m.to_vec()));
                    }
                    true
                });
            }
        }
        if fresh.is_empty() {
            break;
        }
        let text = |t: &Triple| [&terms.terms[t[0] as usize], &terms.terms[t[1] as usize], &terms.terms[t[2] as usize]];
        fresh.sort_by(|a, b| text(&a.0).cmp(&text(&b.0)));
        let room = opts.max_derived.saturating_sub(derivs.len());
        if fresh.len() > room {
            fresh.truncate(room);
            truncated = Some(format!("stopped at max_derived = {}", opts.max_derived));
        }
        for (c, ri, premises) in fresh {
            let idx = ix.push(c);
            derivs.push((ri, idx, premises));
        }
        delta = (end, ix.facts.len() as u32);
        if truncated.is_some() {
            break;
        }
    }

    let fact = |i: u32, terms: &Interner, ix: &Index| {
        let t = ix.facts[i as usize];
        Fact::new(&terms.terms[t[0] as usize], &terms.terms[t[1] as usize], &terms.terms[t[2] as usize])
    };
    let derived: Vec<Derivation> = derivs
        .iter()
        .map(|(ri, idx, premises)| Derivation {
            rule: rules[*ri as usize].name.clone(),
            conclusion: fact(*idx, &terms, &ix),
            premises: premises.iter().map(|p| fact(*p, &terms, &ix)).collect(),
        })
        .collect();
    let mut by_rule: BTreeMap<String, usize> = rules.iter().map(|r| (r.name.clone(), 0)).collect();
    for d in &derived {
        *by_rule.entry(d.rule.clone()).or_default() += 1;
    }
    let mut instances_by: HashMap<Triple, Vec<u32>> = HashMap::new();
    for (i, inst) in instances.iter().enumerate() {
        instances_by.entry(inst.conclusion).or_default().push(i as u32);
    }
    let refused = refusals.len();
    let stats = ReasonStats {
        workspace: opts.workspace.clone(),
        asserted: asserted.len(),
        derived: derived.len(),
        rules: rules.len(),
        by_rule,
        rounds,
        fixpoint_reached: truncated.is_none(),
        truncated,
        refused,
        refusals: refusals.into_iter().take(MAX_REFUSALS_LISTED).collect(),
        not_evaluated: NOT_EVALUATED.iter().map(|s| s.to_string()).collect(),
        instances: opts.capture_all.then_some(instances.len()),
        instances_truncated,
    };
    Closure { rules, terms, ix, asserted, derived, instances, instances_by, stats }
}

impl Closure {
    /// The asserted facts, sorted.
    pub fn asserted(&self) -> &[Fact] {
        &self.asserted
    }

    /// The derivations in certificate order.
    pub fn derived(&self) -> &[Derivation] {
        &self.derived
    }

    /// The rule table this closure was computed with.
    pub fn rules(&self) -> &[RuleLine] {
        &self.rules
    }

    pub fn stats(&self) -> ReasonStats {
        self.stats.clone()
    }

    pub fn fixpoint_reached(&self) -> bool {
        self.stats.fixpoint_reached
    }

    fn triple_of(&self, f: &Fact) -> Option<Triple> {
        Some([*self.terms.ids.get(&f.s)?, *self.terms.ids.get(&f.p)?, *self.terms.ids.get(&f.o)?])
    }

    /// Is `f` asserted, derived (with its line), or absent?
    pub fn contains(&self, f: &Fact) -> Option<Status> {
        let i = *self.ix.set.get(&self.triple_of(f)?)? as usize;
        let n = self.asserted.len();
        Some(if i < n { Status::Asserted } else { Status::Derived { line: i - n + 1 } })
    }

    /// The recorded derivation of a derived fact.
    pub fn derivation(&self, f: &Fact) -> Option<&Derivation> {
        match self.contains(f)? {
            Status::Derived { line } => self.derived.get(line - 1),
            Status::Asserted => None,
        }
    }

    /// Every recorded rule instance concluding `f` (only with `capture_all`),
    /// sorted by rule and premises.
    pub fn instances_of(&self, f: &Fact) -> Vec<Derivation> {
        let Some(t) = self.triple_of(f) else { return Vec::new() };
        let all = self.asserted.len() + self.derived.len();
        let fact = |i: u32| {
            let t = self.ix.facts[i as usize];
            Fact::new(
                &self.terms.terms[t[0] as usize],
                &self.terms.terms[t[1] as usize],
                &self.terms.terms[t[2] as usize],
            )
        };
        let mut out: Vec<Derivation> = self
            .instances_by
            .get(&t)
            .into_iter()
            .flatten()
            .map(|&i| &self.instances[i as usize])
            .filter(|inst| inst.premises.iter().all(|p| (*p as usize) < all))
            .map(|inst| Derivation {
                rule: self.rules[inst.rule as usize].name.clone(),
                conclusion: f.clone(),
                premises: inst.premises.iter().map(|p| fact(*p)).collect(),
            })
            .collect();
        out.sort_by(|a, b| (&a.rule, &a.premises).cmp(&(&b.rule, &b.premises)));
        out
    }

    /// Every fact, asserted then derived.
    fn all_facts(&self) -> impl Iterator<Item = &Fact> {
        self.asserted.iter().chain(self.derived.iter().map(|d| &d.conclusion))
    }

    /// Basic graph pattern matching over the closure: every binding of the
    /// atoms' variables (without `?`), sorted, at most `limit`. A query whose
    /// matches exceed the scan cap returns the smallest bindings among those
    /// scanned; [`Closure::query_answer`] reports that cut.
    pub fn query(&self, atoms: &[AtomPattern], limit: usize) -> Vec<Binding> {
        self.scan(atoms, limit).0
    }

    /// [`Closure::query`] with the closure's flags, refusing atoms that read
    /// facts the closure does not hold: `state` and `prop.<name>` facts are
    /// collected only when some rule reads them, so such an atom would answer
    /// "nothing matches" for values the store has.
    pub fn query_answer(&self, atoms: &[AtomPattern], limit: usize) -> Result<QueryAnswer> {
        let need = needs(&self.rules);
        for (i, a) in atoms.iter().enumerate() {
            let held = match a.p.as_str() {
                "state" => need.state,
                p => p.strip_prefix("prop.").is_none_or(|name| need.props.contains(name)),
            };
            if !held {
                return Err(Error::invalid(format!(
                    "atom {i}: the closure holds no {} facts because no rule reads them; read property values from the store",
                    a.p
                )));
            }
        }
        let (bindings, cut) = self.scan(atoms, limit);
        let mut truncated = self.stats.truncated.clone();
        if cut {
            let note =
                format!("query stopped after {QUERY_SCAN_CAP} matches; bindings are the smallest of those scanned");
            truncated = Some(match truncated {
                Some(t) => format!("{t}; {note}"),
                None => note,
            });
        }
        Ok(QueryAnswer { bindings, fixpoint_reached: self.stats.fixpoint_reached, truncated })
    }

    /// The `limit` smallest bindings, and whether the scan hit its cap.
    fn scan(&self, atoms: &[AtomPattern], limit: usize) -> (Vec<Binding>, bool) {
        let mut vars = Vec::new();
        let mut body = Vec::with_capacity(atoms.len());
        for a in atoms {
            let triple = [a.s.clone(), a.p.clone(), a.o.clone()];
            let mut lookup = |t: &str| self.terms.ids.get(t).copied();
            match compile_terms(&triple, &mut vars, &mut lookup) {
                Some(t) => body.push(t),
                // A constant the closure never mentions matches nothing.
                None => return (Vec::new(), false),
            }
        }
        if body.is_empty() || limit == 0 {
            return (Vec::new(), false);
        }
        let order = plan_order(&body, None, vars.len());
        let all = self.ix.facts.len() as u32;
        let join = Join { ix: &self.ix, atoms: &body, order: &order, first: (0, all), rest: (0, all) };
        // Keep only the `limit` smallest, so the answer does not depend on
        // the join order.
        let mut found: BTreeSet<Binding> = BTreeSet::new();
        let (mut scanned, mut cut) = (0usize, false);
        let mut binds = vec![None; vars.len()];
        let mut matched = vec![0u32; body.len()];
        join.run(0, &mut binds, &mut matched, &mut |b, _| {
            if scanned == QUERY_SCAN_CAP {
                cut = true;
                return false;
            }
            scanned += 1;
            let binding: Binding = vars
                .iter()
                .zip(b)
                .filter_map(|(v, t)| Some((v[1..].to_string(), self.terms.terms[(*t)? as usize].clone())))
                .collect();
            found.insert(binding);
            if found.len() > limit {
                found.pop_last();
            }
            true
        });
        (found.into_iter().collect(), cut)
    }

    /// The certificate of this closure. Refused when any term or rule was
    /// refused. `fixpoint_reached` and `rounds` are taken from the closure.
    pub fn certificate(&self, mut attested: Attested) -> Result<Certificate> {
        if self.stats.refused > 0 {
            let listed: Vec<String> =
                self.stats.refusals.iter().map(|r| format!("{} ({})", r.term, r.reason)).collect();
            return Err(Error::Invalid(format!(
                "certificate refused: {} term(s) cannot be spelled safely: {}",
                self.stats.refused,
                listed.join("; ")
            )));
        }
        let mut rules_tsv = String::new();
        for r in &self.rules {
            rules_tsv.push_str(&r.tsv());
            rules_tsv.push('\n');
        }
        let asserted_tsv = facts_tsv(&self.asserted);
        let mut derivations_tsv = String::new();
        for d in &self.derived {
            derivations_tsv.push_str(&d.rule);
            for f in std::iter::once(&d.conclusion).chain(&d.premises) {
                derivations_tsv.push('\t');
                derivations_tsv.push_str(&f.tsv());
            }
            derivations_tsv.push('\n');
        }
        attested.fixpoint_reached = self.stats.fixpoint_reached;
        attested.rounds = self.stats.rounds;
        let manifest = Manifest {
            format: FORMAT.into(),
            derived: ManifestDerived {
                asserted_sha256: sha256_hex(asserted_tsv.as_bytes()),
                rules_sha256: sha256_hex(rules_tsv.as_bytes()),
                derivations_sha256: sha256_hex(derivations_tsv.as_bytes()),
                asserted: self.asserted.len(),
                rules: self.rules.len(),
                derivations: self.derived.len(),
            },
            attested,
            means: MANIFEST_MEANS.into(),
        };
        let mut manifest_json =
            serde_json::to_string_pretty(&manifest).map_err(|e| Error::Other(format!("manifest: {e}")))?;
        manifest_json.push('\n');
        Ok(Certificate { rules_tsv, asserted_tsv, derivations_tsv, manifest_json })
    }

    /// Facts `other` has and this closure has not (with `other`'s rule and
    /// premises), and facts this closure has and `other` has not.
    pub fn diff(&self, other: &Closure) -> ClosureDiff {
        let mut added: Vec<AddedFact> = other
            .all_facts()
            .filter(|f| self.contains(f).is_none())
            .map(|f| match other.derivation(f) {
                Some(d) => AddedFact { fact: f.clone(), rule: Some(d.rule.clone()), premises: d.premises.clone() },
                None => AddedFact { fact: f.clone(), rule: None, premises: Vec::new() },
            })
            .collect();
        added.sort_by(|a, b| a.fact.cmp(&b.fact));
        let mut removed: Vec<Fact> = self.all_facts().filter(|f| other.contains(f).is_none()).cloned().collect();
        removed.sort();
        ClosureDiff { added, removed, fixpoint_reached: self.fixpoint_reached() && other.fixpoint_reached() }
    }
}

/// Parse an `asserted.tsv` file.
pub fn parse_asserted(text: &str) -> Result<Vec<Fact>> {
    let mut out = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 3 || f.iter().any(|t| t.is_empty()) {
            return Err(Error::invalid(format!(
                "asserted.tsv line {}: expected 3 non-empty fields, got {}",
                i + 1,
                f.len()
            )));
        }
        out.push(Fact::new(f[0], f[1], f[2]));
    }
    Ok(out)
}

/// Compare a certificate's asserted facts with what the store yields now
/// under the same scope (the workspace named by the manifest, if any).
pub fn binding(
    schema: &Schema,
    g: &Graph,
    can_read: &dyn Fn(&Entity) -> bool,
    asserted_tsv: &str,
    manifest_json: Option<&str>,
) -> Result<BindingReport> {
    let cert = parse_asserted(asserted_tsv)?;
    let manifest: Option<Value> = manifest_json
        .map(|m| serde_json::from_str(m).map_err(|e| Error::invalid(format!("manifest.json: {e}"))))
        .transpose()?;
    let workspace =
        manifest.as_ref().and_then(|m| m.pointer("/attested/workspace")).and_then(Value::as_str).map(str::to_string);
    let certificate_sha256 = sha256_hex(asserted_tsv.as_bytes());
    let manifest_digest_matches = manifest
        .as_ref()
        .and_then(|m| m.pointer("/derived/asserted_sha256"))
        .and_then(Value::as_str)
        .map(|d| d == certificate_sha256);
    let (rules, _) = rule_table(schema);
    let now = collect(schema, g, workspace.as_deref(), &rules, can_read);
    let cert_set: BTreeSet<&Fact> = cert.iter().collect();
    let store_set: BTreeSet<&Fact> = now.facts.iter().collect();
    let only_cert: Vec<&Fact> = cert_set.difference(&store_set).copied().collect();
    let only_store: Vec<&Fact> = store_set.difference(&cert_set).copied().collect();
    let same = only_cert.is_empty() && only_store.is_empty();
    Ok(BindingReport {
        verdict: if same { BindingVerdict::SameGraph } else { BindingVerdict::DifferentGraph },
        workspace,
        certificate_facts: cert_set.len(),
        store_facts: store_set.len(),
        shared: cert_set.intersection(&store_set).count(),
        only_in_certificate: only_cert.len(),
        only_in_store: only_store.len(),
        sample_only_in_certificate: only_cert.iter().take(MAX_REFUSALS_LISTED).map(|f| (*f).clone()).collect(),
        sample_only_in_store: only_store.iter().take(MAX_REFUSALS_LISTED).map(|f| (*f).clone()).collect(),
        certificate_sha256,
        store_sha256: sha256_hex(facts_tsv(&now.facts).as_bytes()),
        manifest_digest_matches,
        refused: now.refusals.len(),
        means: BINDING_MEANS.into(),
    })
}

// ── engine wrappers ──────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Eq)]
struct CacheKey {
    revision: u64,
    schema_version: u64,
    principal: String,
    entity: Option<String>,
    opts: ReasonOptions,
}

/// The last few closures an ontology computed, keyed by graph revision,
/// schema version, principal scope and options.
#[derive(Default)]
pub(crate) struct ClosureCache {
    entries: Mutex<VecDeque<(CacheKey, Arc<Closure>)>>,
}

impl ClosureCache {
    fn get(&self, key: &CacheKey) -> Option<Arc<Closure>> {
        let mut e = self.entries.lock();
        let i = e.iter().position(|(k, _)| k == key)?;
        let hit = e.remove(i)?;
        let c = hit.1.clone();
        e.push_back(hit);
        Some(c)
    }

    fn put(&self, key: CacheKey, c: Arc<Closure>) {
        let mut e = self.entries.lock();
        e.retain(|(k, _)| k != &key);
        e.push_back((key, c));
        while e.len() > CACHE_SIZE {
            e.pop_front();
        }
    }
}

impl Ontology {
    fn reason_at(&self, p: &Principal, opts: &ReasonOptions) -> Result<(Arc<Closure>, u64, u64)> {
        if let Some(ws) = &opts.workspace
            && !p.in_workspace(ws)
        {
            return Err(Error::Denied(format!("{} has no access to workspace {ws}", p.id)));
        }
        let g = self.reader(p)?;
        let active = self.active_schema();
        let revision = self.revision();
        let key = CacheKey {
            revision,
            schema_version: active.version,
            principal: p.scope_key(),
            entity: p.entity.clone(),
            opts: opts.clone(),
        };
        if let Some(c) = self.reason_cache().get(&key) {
            return Ok((c, revision, active.version));
        }
        let can = |e: &Entity| policy::visible(p, e, &g);
        let c = Arc::new(closure(&active.schema, &g, opts, &can));
        self.reason_cache().put(key, c.clone());
        Ok((c, revision, active.version))
    }

    /// The closure of what `p` may read (cached for the last few scopes).
    pub fn reason(&self, p: &Principal, opts: &ReasonOptions) -> Result<Arc<Closure>> {
        Ok(self.reason_at(p, opts)?.0)
    }

    /// Query the closure with atoms written like rule `when` atoms, e.g.
    /// `[["?i", "handled_by_team", "team:payments"]]`.
    pub fn reason_query(
        &self,
        p: &Principal,
        opts: &ReasonOptions,
        atoms: &[Vec<Value>],
        limit: usize,
    ) -> Result<QueryAnswer> {
        let patterns = parse_atoms(&self.schema(), atoms)?;
        self.reason(p, opts)?.query_answer(&patterns, limit)
    }

    pub fn reason_stats(&self, p: &Principal, opts: &ReasonOptions) -> Result<ReasonStats> {
        Ok(self.reason(p, opts)?.stats())
    }

    /// A certificate of the closure, attested with this engine's state.
    pub fn reason_certificate(&self, p: &Principal, opts: &ReasonOptions) -> Result<Certificate> {
        let (c, revision, schema_version) = self.reason_at(p, opts)?;
        let stats = c.stats();
        c.certificate(Attested {
            engine: format!("onto-intelli {}", env!("CARGO_PKG_VERSION")),
            created_at: fmt_ts(now_ms()),
            workspace: opts.workspace.clone(),
            schema_version,
            graph_revision: revision,
            principal: p.id.clone(),
            fixpoint_reached: stats.fixpoint_reached,
            rounds: stats.rounds,
        })
    }

    /// Does a certificate's `asserted.tsv` match what the store yields now
    /// for `p` (in the manifest's workspace, when a manifest is given)?
    pub fn reason_binding(
        &self,
        p: &Principal,
        asserted_tsv: &str,
        manifest_json: Option<&str>,
    ) -> Result<BindingReport> {
        let g = self.reader(p)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        let report = binding(&self.schema(), &g, &can, asserted_tsv, manifest_json)?;
        if let Some(ws) = &report.workspace
            && !p.in_workspace(ws)
        {
            return Err(Error::Denied(format!("{} has no access to workspace {ws}", p.id)));
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Link;
    use serde_json::json;

    fn entity(id: &str, ws: &str) -> Entity {
        Entity {
            id: id.into(),
            kind: id.split(':').next().unwrap_or_default().into(),
            workspace: ws.into(),
            title: id.into(),
            state: None,
            props: Default::default(),
            aliases: Vec::new(),
            provenance: None,
            created_at: 0,
            updated_at: 0,
            revision: 1,
        }
    }

    fn link(from: &str, rel: &str, to: &str) -> Link {
        Link {
            from: from.into(),
            rel: rel.into(),
            to: to.into(),
            props: Default::default(),
            workspace: "acme".into(),
            created_at: 0,
        }
    }

    fn all(_: &Entity) -> bool {
        true
    }

    fn schema() -> Schema {
        Schema::from_toml(
            r#"
            [object_types.node]
            [object_types.special]
            extends = "node"
            implements = ["thing"]
            [interfaces.thing]
            [link_types.next]
            from = ["node"]
            to = ["node"]
            transitive = true
            inverse = "prev"
            [link_types.near]
            symmetric = true
            "#,
        )
        .unwrap()
    }

    #[test]
    fn builtin_table_is_five_lines_in_order() {
        let b = builtin_rules();
        assert_eq!(b.len(), 5);
        assert_eq!(b[0].name, "builtin/type_extends");
        assert_eq!(b[3].body.len(), 3);
        let text: String = b.iter().map(|r| format!("{}\n", r.tsv())).collect();
        assert_eq!(text, BUILTIN_RULES);
    }

    #[test]
    fn spelling_refuses_unsafe_terms() {
        assert_eq!(spell_entity("issue:1").unwrap(), "<issue:1>");
        assert!(spell_entity("issue:a>b").is_err());
        assert!(spell_entity("issue:a\tb").is_err());
        assert!(spell_class("").is_err());
        assert!(spell_relation("state").is_err());
        assert!(spell_relation("?x").is_err());
        assert_eq!(spell_literal(&json!("high")), "\"high\"");
        assert_eq!(spell_literal(&json!("a\tb")), "\"a\\tb\"");
    }

    #[test]
    fn transitive_symmetric_and_types() {
        let s = schema();
        let mut g = Graph::default();
        for id in ["node:a", "node:b", "special:c", "node:d"] {
            g.put_entity(entity(id, "acme"));
        }
        g.put_link(link("node:a", "next", "node:b"));
        g.put_link(link("node:b", "next", "special:c"));
        g.put_link(link("special:c", "next", "node:d"));
        g.put_link(link("node:a", "near", "node:d"));
        let c = closure(&s, &g, &ReasonOptions::default(), &all);
        assert!(c.fixpoint_reached());
        let has = |s: &str, p: &str, o: &str| c.contains(&Fact::new(s, p, o));
        assert!(matches!(has("<node:a>", "next", "<node:d>"), Some(Status::Derived { .. })));
        assert!(matches!(has("<node:d>", "near", "<node:a>"), Some(Status::Derived { .. })));
        assert!(matches!(has("<special:c>", "a", "#node"), Some(Status::Derived { .. })));
        assert!(matches!(has("<special:c>", "a", "#thing"), Some(Status::Derived { .. })));
        assert_eq!(has("<special:c>", "a", "#special"), Some(Status::Asserted));
        assert_eq!(has("<node:d>", "next", "<node:a>"), None);
        let d = c.derivation(&Fact::new("<node:a>", "next", "<node:d>")).unwrap();
        assert_eq!(d.rule, "builtin/transitive");
        assert_eq!(d.premises[0], Fact::new("next", "a", "transitive"));
        // The premises of every line are asserted or concluded earlier.
        let mut known: HashSet<&Fact> = c.asserted().iter().collect();
        for d in c.derived() {
            assert!(d.premises.iter().all(|p| known.contains(p)), "{d:?}");
            known.insert(&d.conclusion);
        }
    }

    #[test]
    fn refused_terms_block_the_certificate_only() {
        let s = schema();
        let mut g = Graph::default();
        g.put_entity(entity("node:a", "acme"));
        g.put_entity(entity("node:b>c", "acme"));
        g.put_entity(entity("node:t\tab", "acme"));
        g.put_link(link("node:a", "state", "node:a"));
        let c = closure(&s, &g, &ReasonOptions::default(), &all);
        let st = c.stats();
        assert_eq!(st.refused, 3, "{:?}", st.refusals);
        assert!(c.contains(&Fact::new("<node:a>", "a", "#node")).is_some());
        let err = c.certificate(Attested::default()).unwrap_err().to_string();
        assert!(err.contains("certificate refused"), "{err}");
        assert!(err.contains("reserved keyword"), "{err}");
    }

    #[test]
    fn query_binds_variables() {
        let s = schema();
        let mut g = Graph::default();
        for id in ["node:a", "node:b", "node:c"] {
            g.put_entity(entity(id, "acme"));
        }
        g.put_link(link("node:a", "next", "node:b"));
        g.put_link(link("node:b", "next", "node:c"));
        let c = closure(&s, &g, &ReasonOptions::default(), &all);
        let atoms = parse_atoms(&s, &[vec![json!("?x"), json!("prev"), json!("node:a")]]).unwrap();
        assert_eq!(atoms[0], AtomPattern { s: "<node:a>".into(), p: "next".into(), o: "?x".into() });
        let b = c.query(&atoms, 10);
        let xs: Vec<&str> = b.iter().map(|b| b["x"].as_str()).collect();
        assert_eq!(xs, vec!["<node:b>", "<node:c>"]);
        assert_eq!(c.query(&atoms, 1).len(), 1);
        let none = parse_atoms(&s, &[vec![json!("?x"), json!("next"), json!("node:zzz")]]).unwrap();
        assert!(c.query(&none, 10).is_empty());
    }

    #[test]
    fn limits_make_a_lower_bound() {
        let s = schema();
        let mut g = Graph::default();
        for i in 0..20 {
            g.put_entity(entity(&format!("node:{i:02}"), "acme"));
        }
        for i in 0..19 {
            g.put_link(link(&format!("node:{i:02}"), "next", &format!("node:{:02}", i + 1)));
        }
        let full = closure(&s, &g, &ReasonOptions::default(), &all);
        assert!(full.fixpoint_reached());
        assert_eq!(full.stats().by_rule["builtin/transitive"], 19 * 18 / 2);
        let cut = closure(&s, &g, &ReasonOptions { max_derived: 10, ..Default::default() }, &all);
        assert!(!cut.fixpoint_reached());
        assert_eq!(cut.derived().len(), 10);
        assert!(cut.stats().truncated.unwrap().contains("max_derived"));
        let rounds = closure(&s, &g, &ReasonOptions { max_rounds: 1, ..Default::default() }, &all);
        assert!(!rounds.fixpoint_reached());
        assert!(rounds.certificate(Attested::default()).unwrap().manifest_json.contains("\"fixpoint_reached\": false"));
        let diff = cut.diff(&full);
        assert!(!diff.fixpoint_reached);
        assert_eq!(diff.added.len(), full.derived().len() - 10);
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn capture_all_records_every_instance() {
        let s = schema();
        let mut g = Graph::default();
        for id in ["node:a", "node:b", "node:c", "node:d"] {
            g.put_entity(entity(id, "acme"));
        }
        // Two ways to a -> d: through b and through c.
        g.put_link(link("node:a", "next", "node:b"));
        g.put_link(link("node:b", "next", "node:d"));
        g.put_link(link("node:a", "next", "node:c"));
        g.put_link(link("node:c", "next", "node:d"));
        let opts = ReasonOptions { capture_all: true, ..Default::default() };
        let c = closure(&s, &g, &opts, &all);
        let ad = Fact::new("<node:a>", "next", "<node:d>");
        assert_eq!(c.instances_of(&ad).len(), 2);
        assert_eq!(c.stats().instances, Some(2));
        let plain = closure(&s, &g, &ReasonOptions::default(), &all);
        assert!(plain.instances_of(&ad).is_empty());
    }

    #[test]
    fn query_answers_are_the_sorted_first_bindings() {
        let s = schema();
        let atoms = parse_atoms(
            &s,
            &[vec![json!("?y"), json!("a"), json!("node")], vec![json!("?x"), json!("a"), json!("node")]],
        )
        .unwrap();
        let graph = |n: usize| {
            let mut g = Graph::default();
            for i in 0..n {
                g.put_entity(entity(&format!("node:{i:03}"), "acme"));
            }
            g
        };
        // 200 * 200 matches: under the scan cap, so the answer is exact.
        let c = closure(&s, &graph(200), &ReasonOptions::default(), &all);
        let a = c.query_answer(&atoms, 300).unwrap();
        assert_eq!(a.truncated, None);
        assert_eq!(a.bindings.len(), 300);
        assert_eq!(a.bindings[250]["x"], "<node:001>");
        assert_eq!(a.bindings[250]["y"], "<node:050>");
        assert!(a.bindings.windows(2).all(|w| w[0] < w[1]));
        // 400 * 400 matches pass the cap: the answer says it was cut.
        let c = closure(&s, &graph(400), &ReasonOptions::default(), &all);
        let a = c.query_answer(&atoms, 300).unwrap();
        assert!(a.fixpoint_reached);
        assert!(a.truncated.as_deref().is_some_and(|t| t.contains("query stopped after 100000")), "{:?}", a.truncated);
        assert_eq!(a.bindings.len(), 300);
    }

    #[test]
    fn queries_refuse_property_facts_no_rule_reads() {
        let s = Schema::from_toml(
            r#"
            [object_types.node.properties.tier]
            type = "string"
            [object_types.node.properties.size]
            type = "string"
            [[rules]]
            name = "big"
            when = [["?n", "prop.size", "big"]]
            then = ["?n", "a", "big_node"]
            "#,
        )
        .unwrap();
        let mut e = entity("node:a", "acme");
        e.state = Some("open".into());
        e.props.insert("tier".into(), json!("gold"));
        e.props.insert("size".into(), json!("big"));
        let mut g = Graph::default();
        g.put_entity(e);
        let c = closure(&s, &g, &ReasonOptions::default(), &all);
        for atom in
            [vec![json!("?n"), json!("state"), json!("open")], vec![json!("?n"), json!("prop.tier"), json!("gold")]]
        {
            let p = parse_atoms(&s, &[atom]).unwrap();
            let err = c.query_answer(&p, 10).unwrap_err().to_string();
            assert!(err.contains("no rule reads them"), "{err}");
        }
        let read = parse_atoms(&s, &[vec![json!("?n"), json!("prop.size"), json!("big")]]).unwrap();
        assert_eq!(c.query_answer(&read, 10).unwrap().bindings.len(), 1);
    }

    #[test]
    fn chain_infer_with_caret_swaps_the_head() {
        let s = Schema::from_toml(
            r#"
            [object_types.node]
            [link_types.r]
            [link_types.q]
            [[rules]]
            name = "d"
            chain = ["r"]
            infer = "foo"
            [[rules]]
            name = "e"
            chain = ["r"]
            infer = "^foo"
            [[rules]]
            name = "f"
            chain = ["r"]
            infer = "^q"
            "#,
        )
        .unwrap();
        let (lines, refused) = rule_table(&s);
        assert!(refused.is_empty(), "{refused:?}");
        let tsv: Vec<String> = lines.iter().filter(|l| !l.is_builtin()).map(RuleLine::tsv).collect();
        assert_eq!(
            tsv,
            vec![
                "schema/d\t?x0\tfoo\t?x1\t?x0\tr\t?x1",
                "schema/e\t?x1\tfoo\t?x0\t?x0\tr\t?x1",
                "schema/f\t?x1\tq\t?x0\t?x0\tr\t?x1",
            ]
        );
    }

    #[test]
    fn keyword_heads_compile_and_are_refused_by_the_reasoner() {
        let s = Schema::from_toml(
            r#"
            [object_types.node]
            [link_types.r]
            [[rules]]
            name = "h"
            when = [["?x", "r", "?y"]]
            then = ["?x", "implements", "?y"]
            [[rules]]
            name = "c"
            chain = ["r"]
            infer = "implements"
            "#,
        )
        .expect("a keyword head is the reasoner's refusal, not a schema error");
        let (lines, refused) = rule_table(&s);
        assert_eq!(lines.len(), builtin_rules().len());
        let reasons: Vec<&str> = refused.iter().map(|r| r.reason.as_str()).collect();
        assert_eq!(
            reasons,
            vec![
                "rule h: then: relation name 'implements' is a reserved keyword",
                "rule c: relation name 'implements' is a reserved keyword",
            ]
        );
        // A malformed rule is still a schema error.
        let err = Schema::from_toml("[[rules]]\nname = 'b'\nwhen = [['?x', 'a', 'nope']]\nthen = ['?x', 'a', 'y']")
            .unwrap_err()
            .to_string();
        assert!(err.contains("rule b: when[0]: unknown class nope"), "{err}");
    }
}
