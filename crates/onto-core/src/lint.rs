//! Schema lint: fixed heuristics over a schema's definitions (and, given a
//! graph, over how much of the schema the graph uses), with reviewer
//! feedback kept per finding.
//!
//! * **A lint report is a measurement.** A finding says that the schema
//!   matches a heuristic worth a look. It is not a proof that the schema is
//!   wrong, and an empty report is not a proof that it is right: lint neither
//!   checks the graph against the schema (shapes and the quality report do)
//!   nor reasons over it (the reasoner does). Every report says so in `means`.
//! * **Stable ids.** A finding's id is the first 12 hex digits of
//!   `sha256(check + "\0" + target)`, so it survives edits elsewhere in the
//!   schema and feedback can refer to it across versions.
//! * **Never hide silently.** Dismissed findings leave the report only by
//!   default and are counted in `suppressed`; accepted ones stay and carry
//!   `accepted: true`. Checks that need a graph and had none are listed in
//!   `not_run`.
//! * **Determinism.** Findings are sorted by severity, check and target, and
//!   no hash-map iteration reaches the output.
//!
//! The checks, their severities and what they flag are the [`CHECKS`] table.
//! Lint reads any [`Schema`] value, including one that never went through
//! `Schema::compile`; checks that overlap compile-time validation
//! (`shape_reference`, `unknown_rule_class`) report every problem at once
//! instead of the first.
//!
//! Feedback (`accept` or `dismiss`, with an optional note) is stored in the
//! KV namespace [`NS_FEEDBACK`] keyed by finding id, needs write access and
//! is audited as `schema.lint_feedback`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::engine::{ActionContext, Extra};
use crate::graph::Graph;
use crate::model::Entity;
use crate::policy::{self, Access, Principal};
use crate::reason::{self, RuleLine, Vocabulary};
use crate::schema::{Cardinality, LinkType, PropType, PropertyDef, Schema};
use crate::shape_model::{Shape, ShapePath};
use crate::time::now_ms;
use crate::{Error, Ontology, Result};

/// KV namespace of lint feedback, keyed by finding id.
pub const NS_FEEDBACK: &str = "lint.feedback";

/// Audited event action of [`Ontology::lint_feedback`].
pub const FEEDBACK_ACTION: &str = "schema.lint_feedback";

/// What a lint report is (the `means` field of every report).
pub const MEANS: &str = "A lint report is a measurement: fixed heuristics over the schema's definitions and, \
with a graph, over how much of the schema the visible graph uses. A finding is a suggestion to review, not a \
proof that the schema is wrong; no finding is not a proof that it is right. Lint does not validate the graph \
against the schema (shapes and the quality report do) and does not reason over it (the reasoner does).";

/// A type with more direct subtypes than this is reported by `flat_hierarchy`.
pub const FLAT_HIERARCHY_LIMIT: usize = 8;

/// Longest feedback note accepted, in characters.
pub const MAX_NOTE_CHARS: usize = 2000;

/// How serious a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The schema says something it cannot mean (a cycle, an impossible list).
    Error,
    /// The schema is legal but likely not what was meant.
    Warning,
    /// A gap worth filling (documentation, unused vocabulary).
    Info,
}

/// One lint check: its name, the severity of its findings, whether it reads
/// the graph, and what it flags.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct CheckInfo {
    pub name: &'static str,
    /// The severity of its findings (`keyword_collision` reports inverse
    /// names one step lower, as warnings).
    pub severity: Severity,
    pub needs_graph: bool,
    pub about: &'static str,
}

const fn check(name: &'static str, severity: Severity, needs_graph: bool, about: &'static str) -> CheckInfo {
    CheckInfo { name, severity, needs_graph, about }
}

/// Every lint check, in the order it runs.
pub const CHECKS: &[CheckInfo] = &[
    check(
        "missing_description",
        Severity::Info,
        false,
        "An object type, link type, rule, shape or action type has no description.",
    ),
    check(
        "missing_inverse",
        Severity::Info,
        false,
        "A link type that is not symmetric has no inverse name, so its reverse reads as ^rel.",
    ),
    check(
        "unconstrained_link",
        Severity::Warning,
        false,
        "A link type has no from list or no to list, so it accepts any kind at that end.",
    ),
    check(
        "orphan_type",
        Severity::Info,
        false,
        "An object type no link type names, that extends and implements nothing, that nothing extends and \
         that no table backs.",
    ),
    check("extends_cycle", Severity::Error, false, "Object types extend each other in a cycle."),
    check(
        "dead_rule",
        Severity::Warning,
        false,
        "A rule can never fire: some variable (or constant) must be of kinds no object type satisfies at once, \
         taking subtypes, interfaces and empty-means-any endpoint lists into account.",
    ),
    check(
        "recursive_rule",
        Severity::Info,
        false,
        "A rule's head is reachable from its own body through rules. Allowed (the reasoner runs it to a \
         fixpoint), worth knowing.",
    ),
    check(
        "keyword_collision",
        Severity::Error,
        false,
        "A link type or a derived relation is named like a reserved keyword (a, extends, implements, state, \
         transitive, symmetric) or reads as another kind of term, so the reasoner refuses it. An inverse name \
         that does is a warning: the reasoner still reasons over the link type, but atoms read the name as the \
         inverse, never as the keyword.",
    ),
    check(
        "transitive_cardinality_one",
        Severity::Warning,
        false,
        "A transitive link type has cardinality one, which its own closure breaks.",
    ),
    check(
        "symmetric_endpoints",
        Severity::Warning,
        false,
        "A symmetric link type whose from and to lists differ, so a reversed link may not fit them.",
    ),
    check(
        "state_lists",
        Severity::Error,
        false,
        "Terminal or alert states that are not among the type's states (when it lists states).",
    ),
    check("empty_enum", Severity::Error, false, "An enum property or parameter with no values: nothing fits it."),
    check(
        "flat_hierarchy",
        Severity::Info,
        false,
        "An object type with more than 8 direct subtypes; an intermediate level may help.",
    ),
    check(
        "shape_reference",
        Severity::Error,
        false,
        "A shape names an unknown target, relation, property, class or shape, has an invalid pattern, or has \
         bounds that contradict each other.",
    ),
    check("unused_interface", Severity::Info, false, "An interface no object type implements."),
    check("duplicate_label", Severity::Warning, false, "Two object types (or two link types) share one label."),
    check(
        "unknown_rule_class",
        Severity::Error,
        false,
        "A rule's when uses `a <name>` where the name is neither a type, an interface nor a class some rule \
         derives.",
    ),
    check("unused_type", Severity::Info, true, "No visible object is of this type or one of its subtypes."),
    check("unused_link_type", Severity::Info, true, "No visible link of this type exists."),
];

/// The table entry of a check.
pub fn check_info(name: &str) -> Option<&'static CheckInfo> {
    CHECKS.iter().find(|c| c.name == name)
}

/// `sha256(check + "\0" + target)`, first 12 hex digits.
pub fn finding_id(check: &str, target: &str) -> String {
    let mut h = Sha256::new();
    h.update(check.as_bytes());
    h.update([0u8]);
    h.update(target.as_bytes());
    h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// One lint finding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// See [`finding_id`].
    pub id: String,
    pub check: String,
    pub severity: Severity,
    /// What the finding is about: `object_type:<name>`, `link_type:<name>`,
    /// `interface:<name>`, `rule:<name>`, `shape:<name>`,
    /// `shape:<name>/<property shape>`, `action_type:<name>`, a property
    /// as `<section>:<name>.<property>`, or a list of those.
    pub target: String,
    pub message: String,
    /// What would make the finding go away.
    pub fix: String,
    /// A reviewer accepted it (it stays in the report).
    #[serde(default, skip_serializing_if = "is_false")]
    pub accepted: bool,
    /// A reviewer dismissed it (shown only with `include_dismissed`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub dismissed: bool,
    /// The reviewer's note, with either decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Findings in a report, by severity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintCounts {
    pub error: usize,
    pub warning: usize,
    pub info: usize,
}

/// A selected check that did not run, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotRun {
    pub check: String,
    pub reason: String,
}

/// The result of a lint run (a measurement; see [`MEANS`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LintReport {
    /// Sorted by severity, then check, then target.
    pub findings: Vec<Finding>,
    /// Of `findings`, by severity.
    pub counts: LintCounts,
    /// Dismissed findings left out of `findings`.
    #[serde(default)]
    pub suppressed: usize,
    /// Checks that ran, in [`CHECKS`] order.
    pub checks_run: Vec<String>,
    /// Selected checks that did not run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_run: Vec<NotRun>,
    /// Version of the schema linted, when it came from an ontology.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u64>,
    pub means: String,
}

/// A reviewer's decision on a finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LintDecision {
    /// The finding is right; it stays in reports, marked `accepted`.
    Accept,
    /// The finding does not apply; reports hide it and count it in `suppressed`.
    Dismiss,
}

/// Stored feedback on one finding (the latest decision wins).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintFeedback {
    pub id: String,
    pub check: String,
    pub target: String,
    pub decision: LintDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Principal id of the reviewer.
    pub by: String,
    /// Milliseconds since the epoch.
    pub at: i64,
}

/// Input of [`Ontology::schema_lint`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintQuery {
    /// Run only these checks (all when absent or empty).
    #[serde(default)]
    pub checks: Option<Vec<String>>,
    /// Keep dismissed findings in the report (marked `dismissed`).
    #[serde(default)]
    pub include_dismissed: bool,
}

// ── running ──────────────────────────────────────────────────────────────

/// Run every check. Graph checks run when `g` is given and count only what
/// `can_read` admits.
pub fn lint(schema: &Schema, g: Option<&Graph>, can_read: &dyn Fn(&Entity) -> bool) -> LintReport {
    run(schema, g, can_read, CHECKS.iter().map(|c| c.name).collect())
}

/// Run the named checks (all when `checks` is `None` or empty). An unknown
/// check name is an error that lists the known ones.
pub fn lint_checks(
    schema: &Schema,
    g: Option<&Graph>,
    can_read: &dyn Fn(&Entity) -> bool,
    checks: Option<&[String]>,
) -> Result<LintReport> {
    let selected = match checks {
        None | Some([]) => CHECKS.iter().map(|c| c.name).collect(),
        Some(names) => {
            let mut sel = BTreeSet::new();
            for n in names {
                match check_info(n.trim()) {
                    Some(c) => {
                        sel.insert(c.name);
                    }
                    None => {
                        let known: Vec<&str> = CHECKS.iter().map(|c| c.name).collect();
                        return Err(Error::invalid(format!("unknown lint check {n:?} (known: {})", known.join(", "))));
                    }
                }
            }
            sel
        }
    };
    Ok(run(schema, g, can_read, selected))
}

fn run(
    schema: &Schema,
    g: Option<&Graph>,
    can_read: &dyn Fn(&Entity) -> bool,
    selected: BTreeSet<&'static str>,
) -> LintReport {
    let mut l = Linter { schema, vocab: reason::vocabulary(schema), selected: &selected, findings: Vec::new() };
    l.missing_description();
    l.missing_inverse();
    l.unconstrained_link();
    l.orphan_type();
    l.extends_cycle();
    l.dead_rule();
    l.recursive_rule();
    l.keyword_collision();
    l.transitive_cardinality_one();
    l.symmetric_endpoints();
    l.state_lists();
    l.empty_enum();
    l.flat_hierarchy();
    l.shape_reference();
    l.unused_interface();
    l.duplicate_label();
    l.unknown_rule_class();
    if let Some(g) = g {
        l.unused(g, can_read);
    }
    let mut findings = l.findings;
    findings.sort_by(|a, b| (a.severity, &a.check, &a.target).cmp(&(b.severity, &b.check, &b.target)));
    // A target is reported once per check (duplicate definitions in an
    // uncompiled schema could otherwise repeat it).
    let mut seen = BTreeSet::new();
    findings.retain(|f| seen.insert(f.id.clone()));
    let mut checks_run = Vec::new();
    let mut not_run = Vec::new();
    for c in CHECKS.iter().filter(|c| selected.contains(c.name)) {
        if c.needs_graph && g.is_none() {
            not_run.push(NotRun { check: c.name.into(), reason: "needs a graph".into() });
        } else {
            checks_run.push(c.name.to_string());
        }
    }
    LintReport {
        counts: count(&findings),
        findings,
        suppressed: 0,
        checks_run,
        not_run,
        schema_version: None,
        means: MEANS.into(),
    }
}

fn count(findings: &[Finding]) -> LintCounts {
    let mut c = LintCounts::default();
    for f in findings {
        match f.severity {
            Severity::Error => c.error += 1,
            Severity::Warning => c.warning += 1,
            Severity::Info => c.info += 1,
        }
    }
    c
}

/// Mark findings with their feedback: accepted ones carry `accepted`,
/// dismissed ones are removed and counted in `suppressed` unless
/// `include_dismissed`, in which case they stay, marked `dismissed`.
pub fn apply_feedback(
    mut report: LintReport,
    feedback: &BTreeMap<String, LintFeedback>,
    include_dismissed: bool,
) -> LintReport {
    let mut kept = Vec::with_capacity(report.findings.len());
    for mut f in std::mem::take(&mut report.findings) {
        if let Some(fb) = feedback.get(&f.id) {
            f.note = fb.note.clone();
            match fb.decision {
                LintDecision::Accept => f.accepted = true,
                LintDecision::Dismiss => {
                    f.dismissed = true;
                    if !include_dismissed {
                        report.suppressed += 1;
                        continue;
                    }
                }
            }
        }
        kept.push(f);
    }
    report.counts = count(&kept);
    report.findings = kept;
    report
}

// ── the checks ───────────────────────────────────────────────────────────

struct Linter<'a> {
    schema: &'a Schema,
    vocab: Vocabulary,
    selected: &'a BTreeSet<&'static str>,
    findings: Vec<Finding>,
}

/// What one atom of a rule requires of a term.
enum Need {
    /// Of one of these kinds (or interfaces).
    Kinds(Vec<String>),
    /// In this state.
    State(String),
}

impl Need {
    fn admits(&self, schema: &Schema, kind: &str) -> bool {
        match self {
            Need::Kinds(ks) => ks.iter().any(|k| schema.kind_is(kind, k)),
            Need::State(s) => {
                schema.object_types.get(kind).is_none_or(|t| t.states.is_empty() || t.states.iter().any(|x| x == s))
            }
        }
    }
}

/// What a rule head derives, for recursion: a class or a relation.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Head {
    Class(String),
    Rel(String),
}

impl<'a> Linter<'a> {
    fn on(&self, check: &str) -> bool {
        self.selected.contains(check)
    }

    fn push(&mut self, check: &'static str, target: String, message: String, fix: String) {
        let severity = check_info(check).map(|c| c.severity).unwrap_or(Severity::Info);
        self.push_as(check, severity, target, message, fix);
    }

    /// A finding less severe than its check's usual severity.
    fn push_as(&mut self, check: &'static str, severity: Severity, target: String, message: String, fix: String) {
        self.findings.push(Finding {
            id: finding_id(check, &target),
            check: check.into(),
            severity,
            target,
            message,
            fix,
            accepted: false,
            dismissed: false,
            note: None,
        });
    }

    /// Rules that compile, with their schema index.
    fn compiled_rules(&self) -> Vec<(usize, RuleLine)> {
        let s = self.schema;
        s.rules
            .iter()
            .enumerate()
            .filter_map(|(i, r)| reason::compile_rule(s, &self.vocab, r).ok().map(|l| (i, l)))
            .collect()
    }

    fn missing_description(&mut self) {
        const C: &str = "missing_description";
        if !self.on(C) {
            return;
        }
        let s = self.schema;
        let blank = |d: &str| d.trim().is_empty();
        let mut found: Vec<(String, String, String)> = Vec::new();
        for (name, t) in &s.object_types {
            if blank(&t.description) {
                found.push((
                    format!("object_type:{name}"),
                    format!("object type {name} has no description"),
                    format!("add description = \"...\" to [object_types.{name}]: what one {name} is, in a sentence"),
                ));
            }
        }
        for (name, lt) in &s.link_types {
            if blank(&lt.description) {
                found.push((
                    format!("link_type:{name}"),
                    format!("link type {name} has no description"),
                    format!(
                        "add description = \"...\" to [link_types.{name}]: what a {name} link between two objects means"
                    ),
                ));
            }
        }
        for r in &s.rules {
            if blank(&r.description) {
                found.push((
                    format!("rule:{}", r.name),
                    format!("rule {} has no description", r.name),
                    format!(
                        "add description = \"...\" to the [[rules]] entry {}: what it concludes and why that holds",
                        r.name
                    ),
                ));
            }
        }
        for (name, sh) in &s.shapes {
            if blank(&sh.description) {
                found.push((
                    format!("shape:{name}"),
                    format!("shape {name} has no description"),
                    format!("add description = \"...\" to [shapes.{name}]: the business condition it states"),
                ));
            }
        }
        for (name, a) in &s.action_types {
            if blank(&a.description) {
                found.push((
                    format!("action_type:{name}"),
                    format!("action type {name} has no description"),
                    format!("add description = \"...\" to [action_types.{name}]: what running it does"),
                ));
            }
        }
        for (t, m, f) in found {
            self.push(C, t, m, f);
        }
    }

    fn missing_inverse(&mut self) {
        const C: &str = "missing_inverse";
        if !self.on(C) {
            return;
        }
        let missing: Vec<String> = self
            .schema
            .link_types
            .iter()
            .filter(|(_, lt)| !lt.symmetric && lt.inverse.as_deref().is_none_or(|i| i.trim().is_empty()))
            .map(|(n, _)| n.clone())
            .collect();
        for name in missing {
            self.push(
                C,
                format!("link_type:{name}"),
                format!("link type {name} has no inverse name, so its reverse direction reads as ^{name}"),
                format!("add inverse = \"...\" to [link_types.{name}]: the relation as read from the target's side"),
            );
        }
    }

    fn unconstrained_link(&mut self) {
        const C: &str = "unconstrained_link";
        if !self.on(C) {
            return;
        }
        let mut found = Vec::new();
        for (name, lt) in &self.schema.link_types {
            let open = match (lt.from.is_empty(), lt.to.is_empty()) {
                (true, true) => "any source kind and any target kind",
                (true, false) => "any source kind",
                (false, true) => "any target kind",
                (false, false) => continue,
            };
            found.push((name.clone(), open));
        }
        for (name, open) in found {
            self.push(
                C,
                format!("link_type:{name}"),
                format!("link type {name} accepts {open}"),
                format!("list the kinds it connects in from = [...] and to = [...] of [link_types.{name}]"),
            );
        }
    }

    fn orphan_type(&mut self) {
        const C: &str = "orphan_type";
        if !self.on(C) {
            return;
        }
        let s = self.schema;
        let named: BTreeSet<&str> =
            s.link_types.values().flat_map(|lt| lt.from.iter().chain(&lt.to)).map(String::as_str).collect();
        let parents: BTreeSet<&str> = s.object_types.values().filter_map(|t| t.extends.as_deref()).collect();
        let orphans: Vec<String> = s
            .object_types
            .iter()
            .filter(|(name, t)| {
                t.extends.is_none()
                    && t.implements.is_empty()
                    && t.backing.is_none()
                    && !named.contains(name.as_str())
                    && !parents.contains(name.as_str())
            })
            .map(|(n, _)| n.clone())
            .collect();
        for name in orphans {
            self.push(
                C,
                format!("object_type:{name}"),
                format!(
                    "object type {name} is not connected: no link type names it, it extends nothing, implements no \
                     interface, nothing extends it and no table backs it"
                ),
                "connect it with a link type (from/to), place it in the hierarchy (extends/implements), or remove it"
                    .into(),
            );
        }
    }

    fn extends_cycle(&mut self) {
        const C: &str = "extends_cycle";
        if !self.on(C) {
            return;
        }
        let types = &self.schema.object_types;
        let mut cycles: BTreeSet<Vec<String>> = BTreeSet::new();
        for start in types.keys() {
            let mut path: Vec<&str> = Vec::new();
            let mut cur = Some(start.as_str());
            while let Some(k) = cur {
                if let Some(i) = path.iter().position(|p| *p == k) {
                    let mut cycle: Vec<String> = path[i..].iter().map(|s| s.to_string()).collect();
                    let min = cycle.iter().enumerate().min_by(|a, b| a.1.cmp(b.1)).map(|(i, _)| i).unwrap_or(0);
                    cycle.rotate_left(min);
                    cycles.insert(cycle);
                    break;
                }
                if path.len() > types.len() {
                    break;
                }
                path.push(k);
                cur = types.get(k).and_then(|t| t.extends.as_deref());
            }
        }
        for cycle in cycles {
            let mut walk = cycle.clone();
            walk.push(cycle[0].clone());
            self.push(
                C,
                format!("object_type:{}", cycle[0]),
                format!("object types extend each other in a cycle: {}", walk.join(" extends ")),
                format!("break the cycle: one of [{}] must extend something outside it, or nothing", cycle.join(", ")),
            );
        }
    }

    fn dead_rule(&mut self) {
        const C: &str = "dead_rule";
        if !self.on(C) {
            return;
        }
        let s = self.schema;
        let all: Vec<&str> = s.object_types.keys().map(String::as_str).collect();
        let mut found = Vec::new();
        for (ri, line) in self.compiled_rules() {
            let rule = &s.rules[ri];
            let at = |i: usize| {
                if rule.is_horn() {
                    format!("when[{i}]")
                } else {
                    format!("step {} {}", i + 1, rule.chain.get(i).map(String::as_str).unwrap_or("?"))
                }
            };
            // Every term with what the atoms require of it, in order of first use.
            let mut terms: Vec<(String, Vec<(Need, String)>)> = Vec::new();
            let mut need = |term: &str, n: Need, why: String| match terms.iter_mut().find(|(t, _)| t == term) {
                Some((_, ns)) => ns.push((n, why)),
                None => terms.push((term.to_string(), vec![(n, why)])),
            };
            for (i, [subj, pred, obj]) in line.body.iter().enumerate() {
                match pred.as_str() {
                    "a" => {
                        if let Some(class) = obj.strip_prefix('#')
                            && s.is_type_or_interface(class)
                        {
                            need(subj, Need::Kinds(vec![class.to_string()]), format!("is a {class} ({})", at(i)));
                        }
                    }
                    "state" => {
                        if let Ok(Value::String(st)) = serde_json::from_str::<Value>(obj) {
                            need(subj, Need::State(st.clone()), format!("is in state {st} ({})", at(i)));
                        }
                    }
                    p if p.starts_with('?') || p.starts_with("prop.") => {}
                    rel => {
                        let Some(lt) = s.link_types.get(rel) else { continue };
                        if lt.symmetric {
                            if !lt.from.is_empty() && !lt.to.is_empty() {
                                let mut ends: Vec<String> = lt.from.iter().chain(&lt.to).cloned().collect();
                                ends.sort();
                                ends.dedup();
                                let why = format!("is an end of {rel}, one of [{}] ({})", ends.join(", "), at(i));
                                need(subj, Need::Kinds(ends.clone()), why.clone());
                                need(obj, Need::Kinds(ends), why);
                            }
                        } else {
                            if !lt.from.is_empty() {
                                let why =
                                    format!("is the source of {rel}, one of [{}] ({})", lt.from.join(", "), at(i));
                                need(subj, Need::Kinds(lt.from.clone()), why);
                            }
                            if !lt.to.is_empty() {
                                let why = format!("is the target of {rel}, one of [{}] ({})", lt.to.join(", "), at(i));
                                need(obj, Need::Kinds(lt.to.clone()), why);
                            }
                        }
                    }
                }
            }
            for (term, needs) in &terms {
                // A variable may be any object type; a constant entity is of its id's kind.
                let (shown, mut kinds): (String, Vec<&str>) =
                    match term.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
                        Some(id) => {
                            let kind = id.split(':').next().unwrap_or(id);
                            (format!("{term} (of kind {kind})"), vec![kind])
                        }
                        None if term.starts_with('?') => (term.clone(), all.clone()),
                        None => continue,
                    };
                for (n, _) in needs {
                    kinds.retain(|k| n.admits(s, k));
                }
                if kinds.is_empty() {
                    let whys: Vec<&str> = needs.iter().map(|(_, w)| w.as_str()).collect();
                    found.push((
                        rule.name.clone(),
                        format!(
                            "rule {} can never fire: {shown} {}; no object type satisfies all of these",
                            rule.name,
                            whys.join(", and ")
                        ),
                    ));
                    break;
                }
            }
        }
        for (name, message) in found {
            self.push(
                C,
                format!("rule:{name}"),
                message,
                "correct the relations or classes the rule names (or the from/to lists of those relations), or \
                 remove the rule"
                    .into(),
            );
        }
    }

    fn recursive_rule(&mut self) {
        const C: &str = "recursive_rule";
        if !self.on(C) {
            return;
        }
        let s = self.schema;
        let lines = self.compiled_rules();
        let heads: Vec<Option<Head>> = lines
            .iter()
            .map(|(_, l)| match l.head[1].as_str() {
                "a" => l.head[2].strip_prefix('#').map(|c| Head::Class(c.to_string())),
                rel => Some(Head::Rel(rel.to_string())),
            })
            .collect();
        // Can body atom `atom` match a fact a rule with head `head` derives?
        let uses = |atom: &[String; 3], head: &Head| match (atom[1].as_str(), head) {
            (p, _) if p.starts_with('?') => true,
            ("a", Head::Class(c)) => {
                atom[2].starts_with('?') || atom[2].strip_prefix('#').is_some_and(|b| b == c || s.kind_is(c, b))
            }
            (p, Head::Rel(r)) => p != "a" && p == r,
            _ => false,
        };
        let edges: Vec<Vec<usize>> = lines
            .iter()
            .map(|(_, l)| {
                (0..lines.len())
                    .filter(|&j| heads[j].as_ref().is_some_and(|h| l.body.iter().any(|atom| uses(atom, h))))
                    .collect()
            })
            .collect();
        let mut found = Vec::new();
        for i in 0..lines.len() {
            // Breadth first from i's body, back to i.
            let mut prev: Vec<Option<usize>> = vec![None; lines.len()];
            let mut queue: VecDeque<usize> = VecDeque::new();
            let mut last = None;
            for &j in &edges[i] {
                if j == i {
                    last = Some(i);
                    break;
                }
                if prev[j].is_none() {
                    prev[j] = Some(i);
                    queue.push_back(j);
                }
            }
            while last.is_none()
                && let Some(k) = queue.pop_front()
            {
                for &j in &edges[k] {
                    if j == i {
                        last = Some(k);
                        break;
                    }
                    if prev[j].is_none() && j != i {
                        prev[j] = Some(k);
                        queue.push_back(j);
                    }
                }
            }
            let Some(mut k) = last else { continue };
            // `last` has an edge back to i; walk its predecessors back to i.
            let mut back = Vec::new();
            while k != i {
                back.push(k);
                k = prev[k].unwrap_or(i);
            }
            let mut path = vec![i];
            path.extend(back.into_iter().rev());
            path.push(i);
            let names: Vec<&str> = path.iter().map(|&x| s.rules[lines[x].0].name.as_str()).collect();
            let rule = &s.rules[lines[i].0];
            let derives = match &heads[i] {
                Some(Head::Class(c)) => format!("class {c}"),
                Some(Head::Rel(r)) => format!("relation {r}"),
                None => "a fact".into(),
            };
            found.push((
                rule.name.clone(),
                format!(
                    "rule {} derives {derives}, which its own body uses again through rules {}",
                    rule.name,
                    names.join(" -> ")
                ),
            ));
        }
        for (name, message) in found {
            self.push(
                C,
                format!("rule:{name}"),
                message,
                "nothing, if the recursion is intended (the reasoner runs it to a fixpoint); otherwise break the \
                 cycle between these rules"
                    .into(),
            );
        }
    }

    fn keyword_collision(&mut self) {
        const C: &str = "keyword_collision";
        if !self.on(C) {
            return;
        }
        let s = self.schema;
        let reserved = reason::KEYWORDS.join(", ");
        let mut found = Vec::new();
        for (name, lt) in &s.link_types {
            let own = reason::spell_relation(name).err();
            let inverse = lt.inverse.as_deref().and_then(|inv| reason::spell_relation(inv).err().map(|e| (inv, e)));
            let message = match (&own, &inverse) {
                (None, None) => continue,
                (Some(e), None) => format!("link type {name} cannot be reasoned over: {e}"),
                (Some(e), Some((_, ie))) => {
                    format!("link type {name} cannot be reasoned over: {e}; its inverse name too: {ie}")
                }
                // The reasoner spells the canonical name, so it reasons over
                // the link type; but a rule or query atom with the inverse
                // name means the inverse, never the keyword.
                (None, Some((inv, ie))) => format!(
                    "the inverse name {inv} of link type {name} is ambiguous: {ie}; in rule and query atoms \
                     `{inv}` means the inverse of {name}, so the reasoner's own `{inv}` facts cannot be asked for"
                ),
            };
            let severity = if own.is_some() { Severity::Error } else { Severity::Warning };
            found.push((
                severity,
                format!("link_type:{name}"),
                message,
                format!("rename it (reserved words: {reserved}; names may not start with ?, <, #, \", ^ or prop.)"),
            ));
        }
        for r in &s.rules {
            let derived = if r.is_horn() {
                match r.then.get(1).and_then(Value::as_str) {
                    Some(p) if p != "a" && !p.starts_with('?') && s.resolve_rel(p).is_none() => Some(p),
                    _ => None,
                }
            } else if !r.infer.is_empty() && s.resolve_rel(&r.infer).is_none() {
                Some(r.infer.as_str())
            } else {
                None
            };
            let Some(rel) = derived else { continue };
            let base = rel.strip_prefix('^').unwrap_or(rel);
            if let Err(e) = reason::spell_relation(base) {
                found.push((
                    Severity::Error,
                    format!("rule:{}", r.name),
                    format!("rule {} derives relation {base}, which the reasoner refuses: {e}", r.name),
                    format!("give the derived relation another name (reserved words: {reserved})"),
                ));
            }
        }
        for (sev, t, m, f) in found {
            self.push_as(C, sev, t, m, f);
        }
    }

    fn transitive_cardinality_one(&mut self) {
        const C: &str = "transitive_cardinality_one";
        if !self.on(C) {
            return;
        }
        let names: Vec<String> = self
            .schema
            .link_types
            .iter()
            .filter(|(_, lt)| lt.transitive && lt.cardinality == Cardinality::One)
            .map(|(n, _)| n.clone())
            .collect();
        for name in names {
            self.push(
                C,
                format!("link_type:{name}"),
                format!(
                    "link type {name} is transitive with cardinality one: from a -> b -> c its closure derives \
                     a -> c beside a -> b, so \"at most one\" cannot hold for derived links"
                ),
                format!("drop transitive or set cardinality = \"many\" on [link_types.{name}]"),
            );
        }
    }

    fn symmetric_endpoints(&mut self) {
        const C: &str = "symmetric_endpoints";
        if !self.on(C) {
            return;
        }
        let sorted = |v: &[String]| {
            let mut v = v.to_vec();
            v.sort();
            v.dedup();
            v
        };
        let mut found = Vec::new();
        for (name, lt) in &self.schema.link_types {
            if lt.symmetric && sorted(&lt.from) != sorted(&lt.to) {
                found.push((name.clone(), side(lt, true), side(lt, false)));
            }
        }
        for (name, from, to) in found {
            self.push(
                C,
                format!("link_type:{name}"),
                format!(
                    "symmetric link type {name} has different endpoint lists (from: {from}, to: {to}); its reverse \
                     b -> a needs both lists to admit both ends"
                ),
                format!("give from and to of [link_types.{name}] the same kinds"),
            );
        }
    }

    fn state_lists(&mut self) {
        const C: &str = "state_lists";
        if !self.on(C) {
            return;
        }
        let mut found = Vec::new();
        for (name, t) in &self.schema.object_types {
            if t.states.is_empty() {
                continue;
            }
            let missing =
                |list: &[String]| -> Vec<String> { list.iter().filter(|s| !t.states.contains(s)).cloned().collect() };
            let (term, alert) = (missing(&t.terminal_states), missing(&t.alert_states));
            if term.is_empty() && alert.is_empty() {
                continue;
            }
            let mut parts = Vec::new();
            if !term.is_empty() {
                parts.push(format!("terminal states [{}]", term.join(", ")));
            }
            if !alert.is_empty() {
                parts.push(format!("alert states [{}]", alert.join(", ")));
            }
            found.push((
                name.clone(),
                format!(
                    "object type {name} lists {} that are not among its states [{}], so no object can be in them",
                    parts.join(" and "),
                    t.states.join(", ")
                ),
            ));
        }
        for (name, message) in found {
            self.push(
                C,
                format!("object_type:{name}"),
                message,
                format!(
                    "add them to states, or remove them from terminal_states/alert_states of [object_types.{name}]"
                ),
            );
        }
    }

    fn empty_enum(&mut self) {
        const C: &str = "empty_enum";
        if !self.on(C) {
            return;
        }
        let s = self.schema;
        let empty = |d: &PropertyDef| d.ty == PropType::Enum && d.values.is_empty();
        let mut found = Vec::new();
        for (name, t) in &s.object_types {
            for (p, d) in &t.properties {
                if empty(d) {
                    found.push((format!("object_type:{name}.{p}"), format!("property {name}.{p}")));
                }
            }
        }
        for (name, i) in &s.interfaces {
            for (p, d) in &i.properties {
                if empty(d) {
                    found.push((format!("interface:{name}.{p}"), format!("interface property {name}.{p}")));
                }
            }
        }
        for (name, a) in &s.action_types {
            for (p, d) in &a.parameters {
                if d.ty == crate::action_types::ParamType::Enum && d.values.is_empty() {
                    found.push((format!("action_type:{name}.{p}"), format!("parameter {p} of action type {name}")));
                }
            }
        }
        for (target, what) in found {
            self.push(
                C,
                target,
                format!("{what} is an enum with no values, so no value fits it"),
                "list the allowed values in values = [...], or give it another type".into(),
            );
        }
    }

    fn flat_hierarchy(&mut self) {
        const C: &str = "flat_hierarchy";
        if !self.on(C) {
            return;
        }
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (name, t) in &self.schema.object_types {
            if let Some(p) = t.extends.as_deref() {
                children.entry(p).or_default().push(name);
            }
        }
        let found: Vec<(String, Vec<String>)> = children
            .into_iter()
            .filter(|(_, c)| c.len() > FLAT_HIERARCHY_LIMIT)
            .map(|(p, c)| (p.to_string(), c.into_iter().map(str::to_string).collect()))
            .collect();
        for (parent, kids) in found {
            self.push(
                C,
                format!("object_type:{parent}"),
                format!(
                    "object type {parent} has {} direct subtypes (more than {FLAT_HIERARCHY_LIMIT}): {}",
                    kids.len(),
                    kids.join(", ")
                ),
                format!("group related subtypes of {parent} under intermediate types, or use a property instead"),
            );
        }
    }

    fn shape_reference(&mut self) {
        const C: &str = "shape_reference";
        if !self.on(C) {
            return;
        }
        let mut found: Vec<(String, String, Vec<String>)> = Vec::new();
        for (name, sh) in &self.schema.shapes {
            for (target, problems) in self.shape_problems(name, sh) {
                found.push((name.clone(), target, problems));
            }
        }
        for (name, target, problems) in found {
            self.push(
                C,
                target,
                format!("shape {name}: {}", problems.join("; ")),
                format!("declare the missing names or correct [shapes.{name}]"),
            );
        }
    }

    /// Every problem of one shape, grouped by target (the shape, or one of
    /// its property shapes).
    fn shape_problems(&self, name: &str, sh: &Shape) -> Vec<(String, Vec<String>)> {
        let s = self.schema;
        let class_ok = |c: &str| s.is_type_or_interface(c) || self.vocab.classes.contains(c);
        let rel_ok =
            |r: &str| s.resolve_rel(r).is_some() || self.vocab.relations.contains(r.strip_prefix('^').unwrap_or(r));
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        let mut own = Vec::new();
        if let Some(t) = &sh.target
            && !class_ok(t)
        {
            own.push(format!("unknown target {t} (not a type, an interface or a class a rule derives)"));
        }
        for (what, r) in [("target_subjects_of", &sh.target_subjects_of), ("target_objects_of", &sh.target_objects_of)]
        {
            if let Some(r) = r
                && !rel_ok(r)
            {
                own.push(format!("{what}: unknown relation {r}"));
            }
        }
        if sh.target_ids.iter().any(|id| id.trim().is_empty()) {
            own.push("target_ids contains an empty id".into());
        }
        let mut ids = BTreeMap::new();
        for (i, p) in sh.properties.iter().enumerate() {
            let id = if p.id.is_empty() { format!("{name}/{i}") } else { p.id.clone() };
            if let Some(prev) = ids.insert(id.clone(), i) {
                own.push(format!("property shapes {prev} and {i} share the id {id}"));
            }
        }
        if !own.is_empty() {
            out.push((format!("shape:{name}"), own));
        }
        for (i, p) in sh.properties.iter().enumerate() {
            let id = if p.id.is_empty() { format!("{name}/{i}") } else { p.id.clone() };
            let mut probs = Vec::new();
            match ShapePath::parse(&p.path) {
                ShapePath::Title | ShapePath::State => {}
                ShapePath::Prop(prop) => {
                    let known = s.object_types.values().any(|t| t.properties.contains_key(&prop))
                        || s.interfaces.values().any(|i| i.properties.contains_key(&prop));
                    if !known {
                        probs.push(format!("path {}: unknown property {prop}", p.path));
                    }
                }
                ShapePath::Relations(steps) => {
                    for step in &steps {
                        if step.is_empty() || !rel_ok(step) {
                            probs.push(format!("path {}: unknown relation {step:?}", p.path));
                        }
                    }
                }
            }
            if let Some(c) = &p.class
                && !class_ok(c)
            {
                probs.push(format!("unknown class {c}"));
            }
            if let Some(n) = &p.node
                && !s.shapes.contains_key(n)
            {
                probs.push(format!("unknown node shape {n}"));
            }
            if let Some(pat) = &p.pattern
                && let Err(e) = crate::schema::pattern_regex(pat)
            {
                probs.push(e);
            }
            if let (Some(lo), Some(hi)) = (p.min_count, p.max_count)
                && lo > hi
            {
                probs.push(format!("min_count {lo} exceeds max_count {hi}"));
            }
            if let (Some(lo), Some(hi)) = (p.min_length, p.max_length)
                && lo > hi
            {
                probs.push(format!("min_length {lo} exceeds max_length {hi}"));
            }
            if let (Some(lo), Some(hi)) = (p.min_value, p.max_value)
                && lo > hi
            {
                probs.push(format!("min_value {lo} exceeds max_value {hi}"));
            }
            if !probs.is_empty() {
                let target = match id.strip_prefix(name).and_then(|r| r.strip_prefix('/')) {
                    Some(_) => format!("shape:{id}"),
                    None => format!("shape:{name}/{id}"),
                };
                out.push((target, probs.into_iter().map(|m| format!("property shape {id}: {m}")).collect()));
            }
        }
        out
    }

    fn unused_interface(&mut self) {
        const C: &str = "unused_interface";
        if !self.on(C) {
            return;
        }
        let s = self.schema;
        let unused: Vec<String> =
            s.interfaces.keys().filter(|i| !s.object_types.keys().any(|k| s.kind_is(k, i))).cloned().collect();
        for name in unused {
            self.push(
                C,
                format!("interface:{name}"),
                format!("interface {name} has no implementer, so `kind = \"{name}\"` matches nothing"),
                format!("add implements = [\"{name}\"] to the object types that share it, or remove it"),
            );
        }
    }

    fn duplicate_label(&mut self) {
        const C: &str = "duplicate_label";
        if !self.on(C) {
            return;
        }
        let s = self.schema;
        let norm = |label: &str, name: &str| {
            let l = if label.trim().is_empty() { name } else { label };
            l.to_lowercase().replace(['_', '-'], " ").split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let mut groups: BTreeMap<(&str, String), Vec<(&str, &str)>> = BTreeMap::new();
        for (name, t) in &s.object_types {
            groups.entry(("object_type", norm(&t.label, name))).or_default().push((name, display(&t.label, name)));
        }
        for (name, lt) in &s.link_types {
            groups.entry(("link_type", norm(&lt.label, name))).or_default().push((name, display(&lt.label, name)));
        }
        let mut found = Vec::new();
        for ((section, label), members) in groups {
            if members.len() < 2 {
                continue;
            }
            let targets: Vec<String> = members.iter().map(|(n, _)| format!("{section}:{n}")).collect();
            let shown: Vec<String> = members.iter().map(|(n, l)| format!("{n} (\"{l}\")")).collect();
            let what = if section == "object_type" { "object types" } else { "link types" };
            found.push((
                targets.join(", "),
                format!(
                    "{what} {} share the label \"{label}\", so people and models cannot tell them apart",
                    shown.join(", ")
                ),
            ));
        }
        for (target, message) in found {
            self.push(C, target, message, "give each a label that says how it differs".into());
        }
    }

    fn unknown_rule_class(&mut self) {
        const C: &str = "unknown_rule_class";
        if !self.on(C) {
            return;
        }
        let s = self.schema;
        let mut found = Vec::new();
        for r in &s.rules {
            let mut unknown: Vec<String> = Vec::new();
            for atom in &r.when {
                if atom.get(1).and_then(Value::as_str) != Some("a") {
                    continue;
                }
                let Some(class) = atom.get(2).and_then(Value::as_str) else { continue };
                if class.starts_with('?') || s.is_type_or_interface(class) || self.vocab.classes.contains(class) {
                    continue;
                }
                if !unknown.iter().any(|u| u == class) {
                    unknown.push(class.to_string());
                }
            }
            if !unknown.is_empty() {
                found.push((r.name.clone(), unknown));
            }
        }
        for (name, unknown) in found {
            self.push(
                C,
                format!("rule:{name}"),
                format!(
                    "rule {name} requires `a {}`, which is neither an object type, an interface nor a class any rule \
                     derives, so the rule can never fire",
                    unknown.join("`, `a ")
                ),
                "name an existing type or interface, or add the rule that derives the class".into(),
            );
        }
    }

    fn unused(&mut self, g: &Graph, can_read: &dyn Fn(&Entity) -> bool) {
        let s = self.schema;
        if self.on("unused_type") {
            let kinds: BTreeSet<&str> = g.entities().filter(|e| can_read(e)).map(|e| e.kind.as_str()).collect();
            let unused: Vec<String> =
                s.object_types.keys().filter(|t| !kinds.iter().any(|k| s.kind_is(k, t))).cloned().collect();
            for name in unused {
                self.push(
                    "unused_type",
                    format!("object_type:{name}"),
                    format!("no visible object is of type {name} or one of its subtypes"),
                    "nothing, if the data is still to come; otherwise remove the type or map a source to it".into(),
                );
            }
        }
        if self.on("unused_link_type") {
            let visible = |id: &str| g.get(id).is_some_and(can_read);
            let rels: BTreeSet<&str> =
                g.links().filter(|l| visible(&l.from) && visible(&l.to)).map(|l| l.rel.as_str()).collect();
            let unused: Vec<String> = s.link_types.keys().filter(|r| !rels.contains(r.as_str())).cloned().collect();
            for name in unused {
                self.push(
                    "unused_link_type",
                    format!("link_type:{name}"),
                    format!("no visible link of type {name} exists"),
                    "nothing, if the data is still to come; otherwise remove the link type or map a source to it"
                        .into(),
                );
            }
        }
    }
}

fn side(lt: &LinkType, from: bool) -> String {
    let v = if from { &lt.from } else { &lt.to };
    if v.is_empty() { "any".into() } else { format!("[{}]", v.join(", ")) }
}

fn display<'a>(label: &'a str, name: &'a str) -> &'a str {
    if label.trim().is_empty() { name } else { label }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn valid_id(id: &str) -> bool {
    id.len() == 12 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ── on the ontology ──────────────────────────────────────────────────────

impl Ontology {
    /// Lint the schema in force, with the graph checks over what `p` can
    /// see, and apply stored feedback (see [`apply_feedback`]).
    pub fn schema_lint(&self, p: &Principal, q: LintQuery) -> Result<LintReport> {
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        let feedback = self.feedback_map()?;
        let schema = self.schema();
        let version = self.schema_version();
        let g = self.reader(p)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        let mut report = lint_checks(&schema, Some(&g), &can, q.checks.as_deref())?;
        drop(g);
        report.schema_version = Some(version);
        Ok(apply_feedback(report, &feedback, q.include_dismissed))
    }

    /// Record a reviewer's decision on a current finding (the latest decision
    /// on an id wins). Needs write access; audited as `schema.lint_feedback`.
    /// An id that is not a finding of the current schema and visible graph is
    /// not found.
    pub fn lint_feedback(
        &self,
        ctx: &ActionContext,
        id: &str,
        decision: LintDecision,
        note: Option<String>,
    ) -> Result<LintFeedback> {
        let p = &ctx.principal;
        let denied = if !p.in_workspace(&ctx.workspace) {
            Some(format!("{} has no access to workspace {}", p.id, ctx.workspace))
        } else if !p.allows(Access::Write) {
            Some(format!("{} may not give lint feedback (needs write access)", p.id))
        } else {
            None
        };
        if let Some(reason) = denied {
            let mut c = ctx.clone();
            c.intent = Some(FEEDBACK_ACTION.into());
            self.audit_denied(&c, &reason, now_ms());
            return Err(Error::Denied(reason));
        }
        let id = id.trim().to_ascii_lowercase();
        if !valid_id(&id) {
            return Err(Error::invalid(format!("lint finding id {id:?} must be 12 hex digits")));
        }
        let note = note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
        if note.as_ref().is_some_and(|n| n.chars().count() > MAX_NOTE_CHARS) {
            return Err(Error::invalid(format!("a lint feedback note may hold at most {MAX_NOTE_CHARS} characters")));
        }
        let (_, fb) = self.transact(ctx, |o, g| {
            let schema = o.schema();
            let can = |e: &Entity| policy::visible(p, e, g);
            let report = lint(&schema, Some(g), &can);
            let Some(f) = report.findings.into_iter().find(|f| f.id == id) else {
                return Err(Error::not_found(format!("lint finding {id} (schema.lint lists the current ids)")));
            };
            let fb = LintFeedback {
                id: f.id,
                check: f.check,
                target: f.target,
                decision,
                note: note.clone(),
                by: p.id.clone(),
                at: now_ms(),
            };
            let payload = json!({
                "id": fb.id, "check": fb.check, "target": fb.target, "decision": fb.decision, "note": fb.note,
            });
            let extra = Extra {
                kv: vec![(NS_FEEDBACK.into(), fb.id.clone(), serde_json::to_value(&fb)?)],
                events: vec![o.event(ctx, &ctx.workspace, FEEDBACK_ACTION, None, fb.at, payload, Vec::new())],
                ..Default::default()
            };
            Ok((Vec::new(), extra, fb))
        })?;
        Ok(fb)
    }

    /// Every stored lint feedback entry, by finding id.
    pub fn lint_feedback_entries(&self, p: &Principal) -> Result<Vec<LintFeedback>> {
        if !p.allows(Access::Read) {
            return Err(Error::Denied(format!("{} has no read role", p.id)));
        }
        Ok(self.feedback_map()?.into_values().collect())
    }

    fn feedback_map(&self) -> Result<BTreeMap<String, LintFeedback>> {
        let mut out = BTreeMap::new();
        for (key, v) in self.store().kv_scan(NS_FEEDBACK, "")? {
            // An entry that does not parse hides nothing: its finding shows.
            if let Ok(fb) = serde_json::from_value::<LintFeedback>(v) {
                out.insert(key, fb);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &Entity) -> bool {
        true
    }

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = finding_id("missing_inverse", "link_type:owns");
        assert_eq!(a.len(), 12);
        assert!(valid_id(&a));
        assert_eq!(a, finding_id("missing_inverse", "link_type:owns"));
        assert_ne!(a, finding_id("missing_inverse", "link_type:own"));
        // The separator keeps "ab" + "c" apart from "a" + "bc".
        assert_ne!(finding_id("ab", "c"), finding_id("a", "bc"));
        let mut h = Sha256::new();
        h.update(b"missing_inverse\0link_type:owns");
        let full: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(a, full[..12]);
    }

    #[test]
    fn check_table_is_consistent() {
        let names: BTreeSet<&str> = CHECKS.iter().map(|c| c.name).collect();
        assert_eq!(names.len(), CHECKS.len(), "check names are unique");
        assert_eq!(CHECKS.len(), 19);
        for c in CHECKS {
            assert!(!c.about.is_empty() && c.about.ends_with('.'), "{} is documented", c.name);
        }
        let graph: Vec<&str> = CHECKS.iter().filter(|c| c.needs_graph).map(|c| c.name).collect();
        assert_eq!(graph, ["unused_type", "unused_link_type"]);
    }

    #[test]
    fn selection_and_not_run() {
        let s = Schema::enterprise();
        let r = lint(&s, None, &none);
        assert_eq!(r.checks_run.len(), CHECKS.len() - 2);
        assert_eq!(r.not_run.iter().map(|n| n.check.as_str()).collect::<Vec<_>>(), ["unused_type", "unused_link_type"]);
        assert_eq!(r.means, MEANS);
        let only = lint_checks(&s, None, &none, Some(&["missing_inverse".to_string()])).unwrap();
        assert_eq!(only.checks_run, ["missing_inverse"]);
        assert!(only.not_run.is_empty());
        assert!(only.findings.iter().all(|f| f.check == "missing_inverse"));
        let err = lint_checks(&s, None, &none, Some(&["nope".to_string()])).unwrap_err().to_string();
        assert!(err.contains("unknown lint check") && err.contains("dead_rule"), "{err}");
        assert_eq!(lint_checks(&s, None, &none, Some(&[])).unwrap().checks_run.len(), CHECKS.len() - 2);
    }

    #[test]
    fn report_is_sorted_and_counted() {
        let r = lint(&Schema::enterprise(), None, &none);
        let keys: Vec<_> = r.findings.iter().map(|f| (f.severity, f.check.clone(), f.target.clone())).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        let c = count(&r.findings);
        assert_eq!(c, r.counts);
        assert_eq!(c.error + c.warning + c.info, r.findings.len());
        // Same input, same bytes.
        let again = lint(&Schema::enterprise(), None, &none);
        assert_eq!(serde_json::to_string(&r).unwrap(), serde_json::to_string(&again).unwrap());
    }

    #[test]
    fn feedback_marks_and_suppresses() {
        let r = lint(&Schema::enterprise(), None, &none);
        assert!(r.findings.len() >= 2, "{:?}", r.findings);
        let (a, d) = (&r.findings[0], &r.findings[1]);
        let fb = |f: &Finding, decision| LintFeedback {
            id: f.id.clone(),
            check: f.check.clone(),
            target: f.target.clone(),
            decision,
            note: Some("seen".into()),
            by: "ada".into(),
            at: 1,
        };
        let map: BTreeMap<String, LintFeedback> =
            [(a.id.clone(), fb(a, LintDecision::Accept)), (d.id.clone(), fb(d, LintDecision::Dismiss))].into();
        let hidden = apply_feedback(r.clone(), &map, false);
        assert_eq!(hidden.suppressed, 1);
        assert_eq!(hidden.findings.len(), r.findings.len() - 1);
        assert!(hidden.findings.iter().all(|f| f.id != d.id));
        let acc = hidden.findings.iter().find(|f| f.id == a.id).unwrap();
        assert!(acc.accepted && acc.note.as_deref() == Some("seen"));
        assert_eq!(count(&hidden.findings), hidden.counts);
        let shown = apply_feedback(r.clone(), &map, true);
        assert_eq!(shown.suppressed, 0);
        assert!(shown.findings.iter().any(|f| f.id == d.id && f.dismissed));
        let json = serde_json::to_value(&shown.findings[0]).unwrap();
        assert!(json.get("dismissed").is_none() || json["dismissed"] == true);
    }
}
