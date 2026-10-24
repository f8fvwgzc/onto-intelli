//! Shape reports: SHACL-style validation of the graph against the schema's
//! [`Shape`]s, a structural dry run of shape definitions, and induction of a
//! shape from the data.
//!
//! * **A report is a measurement, not a proof.** [`validate`] measures the
//!   graph as one principal can read it (and, with `inferred`, the reasoner's
//!   closure of it). Its `means` field says so.
//! * **Three answers, not two.** `conforms` means every selected shape with a
//!   target selected at least one focus node, no constraint was skipped, and
//!   none produced a result; `violates` means some constraint produced a
//!   result (of any severity, as in SHACL); `undetermined` means the question
//!   could not be answered: nothing produced a result but a constraint was
//!   skipped or a selected shape's target selected nothing, or the closure is a
//!   lower bound (its results are then listed but decide nothing). Silence is
//!   never read as "valid".
//! * **Whitelist what is implemented.** Keys a property shape carries that this
//!   module does not evaluate (`PropertyShape::unknown`) are reported as
//!   skipped, which makes a report without results undetermined. Annotation
//!   keys (`name`, `description`, `label`, `comment`, `order`, `group`,
//!   `default_value`) never constrain anything and are not skipped: a false
//!   undetermined is not free either.
//! * **A selected-but-absent node is a violation.** An id in `target_ids` that
//!   does not exist, or that the principal cannot read, yields a
//!   `target_missing` result rather than an empty target.
//! * **An induced shape is a hypothesis with evidence per line.** [`induce`]
//!   describes the data as it is, not what it should be: every constraint
//!   carries a sentence saying what was counted, and the entities it came from
//!   satisfy it by construction, so it constrains the next object, not the
//!   existing ones.
//! * **Determinism.** Focus nodes, values and results are sorted; no hash-map
//!   iteration reaches the output.
//!
//! Scope: focus nodes and the objects a path reaches are the entities the
//! principal can read, and with a workspace only that workspace's entities, so
//! a link to an object outside the scope is not counted (the reasoner reads
//! the graph the same way).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::graph::Graph;
use crate::live_schema::{SchemaChange, SchemaChangeSet, valid_name};
use crate::model::{Entity, one_or_many};
use crate::policy::{self, Principal};
use crate::reason::{self, Closure, ReasonOptions};
use crate::schema::{Dir, PropType, Schema, pattern_regex};
use crate::shape_model::{PropertyShape, Shape, ShapePath, ShapeSeverity};
use crate::{Error, Ontology, Result};

/// What a shape report is.
pub const REPORT_MEANS: &str = "A shape report is a measurement of the graph as this principal can read it (with \
     inferred, of the reasoner's closure too), not a proof. conforms: every selected shape with a target selected at least one \
     focus node, no constraint was skipped, and none produced a result. violates: some constraint produced a result, \
     of any severity. undetermined: no result, but a constraint was skipped or a selected shape's target selected \
     nothing (see unmatched_targets); or the closure is a lower bound, and then any results are listed but decide \
     nothing.";

/// What an induced shape is.
pub const INDUCE_MEANS: &str = "An induced shape describes the data as it is, not what it should be. Each constraint \
     carries the counts it was read from; the entities it came from satisfy it by construction, so it constrains the \
     next object, not the existing ones. Review it before saving it.";

/// What a shape check is.
pub const CHECK_MEANS: &str = "A structural dry run: every name a shape references is resolved against the schema \
     and every key is one this engine evaluates. ok says the shapes can be saved and fully evaluated; it says nothing \
     about whether the data conforms.";

/// What a shape delta is.
pub const DELTA_MEANS: &str = "Results that appear and disappear between two shape reports. Both are measurements; \
     when either was truncated by its limit the delta is partial (complete = false).";

/// `node` constraints nest at most this deep; deeper checks are skipped.
pub const MAX_NODE_DEPTH: usize = 5;

/// Results listed by default (all of them are counted).
pub const DEFAULT_LIMIT: usize = 1000;

/// Property shape keys that annotate rather than constrain (SHACL's
/// non-validating properties). They are never reported as skipped.
pub const ANNOTATION_KEYS: &[&str] = &["name", "description", "label", "comment", "order", "group", "default_value"];

// ── report types ─────────────────────────────────────────────────────────

/// Which shapes to evaluate, in which scope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShapesQuery {
    /// Evaluate only these shapes (all when empty). Shapes they reach through
    /// `node` are evaluated as needed either way.
    #[serde(deserialize_with = "one_or_many")]
    pub shapes: Vec<String>,
    /// Only entities of this workspace.
    pub workspace: Option<String>,
    /// Also read the reasoner's closure: derived classes select targets and
    /// satisfy `class`, derived relations are followed by paths.
    pub inferred: bool,
    /// Results listed (all are counted).
    pub limit: usize,
    /// Draft shapes evaluated as if they were in the schema (replacing a
    /// schema shape of the same name). Nothing is saved.
    pub drafts: BTreeMap<String, Shape>,
}

impl Default for ShapesQuery {
    fn default() -> Self {
        Self { shapes: Vec::new(), workspace: None, inferred: false, limit: DEFAULT_LIMIT, drafts: BTreeMap::new() }
    }
}

/// The three answers of a shape report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeVerdict {
    Conforms,
    Violates,
    Undetermined,
}

/// One constraint that did not hold for one focus node (and value).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShapeResult {
    pub shape: String,
    /// The property shape id, `<shape>/closed` or `<shape>/target`.
    pub constraint_id: String,
    /// `min_count`, `max_count`, `class`, `datatype`, `in`, `has_value`,
    /// `pattern`, `min_length`, `max_length`, `min_value`, `max_value`,
    /// `node`, `closed` or `target_missing`.
    pub component: String,
    /// The focus node's id.
    pub focus: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The offending value (an entity id for relation paths).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    pub severity: ShapeSeverity,
    pub message: String,
}

/// A constraint, target or shape that was not evaluated, and why.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SkippedConstraint {
    /// The shape (`*` for the whole report).
    pub shape: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint_id: Option<String>,
    /// The key that was not evaluated (`target`, `path`, `class`, `node`,
    /// `pattern`, an unsupported key, `shape`, `closure`) or `deactivated`.
    pub key: String,
    pub reason: String,
}

impl SkippedConstraint {
    /// Deactivated shapes are skipped on purpose and do not make a report
    /// undetermined.
    pub fn is_deactivation(&self) -> bool {
        self.key == "deactivated"
    }
}

/// Results per severity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeverityCounts {
    pub violation: usize,
    pub warning: usize,
    pub info: usize,
}

/// The outcome of [`validate`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShapeReport {
    pub verdict: ShapeVerdict,
    /// `Some(true)` conforms, `Some(false)` violates, `None` undetermined.
    pub conforms: Option<bool>,
    /// Sorted by shape, focus and constraint id; at most `limit`.
    pub results: Vec<ShapeResult>,
    /// Every result, listed or not.
    pub total_results: usize,
    /// True when `results` was cut at the limit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Focus nodes per evaluated shape that declares a target.
    pub focus_by_shape: BTreeMap<String, usize>,
    pub focus_nodes: usize,
    /// Shapes whose targets selected nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmatched_targets: Vec<String>,
    /// What was not evaluated, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedConstraint>,
    pub counts: SeverityCounts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub inferred: bool,
    /// With `inferred`: false when the closure is a lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixpoint_reached: Option<bool>,
    pub means: String,
}

/// Results that appear and disappear between two reports (for plans).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShapeDelta {
    pub before: ShapeVerdict,
    pub after: ShapeVerdict,
    pub new_results: Vec<ShapeResult>,
    pub resolved: Vec<ShapeResult>,
    /// False when either report was truncated (the delta is then partial).
    pub complete: bool,
    pub means: String,
}

/// What [`check`] found wrong with a shape definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeIssueKind {
    /// A target or class that is neither an object type, an interface nor a
    /// class some rule derives.
    MissingKind,
    /// A relation that is neither a link type (canonical, inverse or `^rel`)
    /// nor one some rule derives.
    MissingRelation,
    /// A `prop.<name>` path naming a property no type declares (or, for a
    /// shape targeting one type or interface, that its objects cannot have).
    MissingProperty,
    /// A `node` naming no shape.
    MissingShape,
    /// A key this engine does not evaluate (it would be skipped).
    UnknownKey,
    /// A `pattern` that is not a valid regular expression.
    BadPattern,
    /// A lower bound above its upper bound.
    BadBounds,
    /// Two property shapes with one id.
    DuplicateId,
    /// An empty id in `target_ids`.
    BadTargetId,
    /// A new shape name that is not `[a-z][a-z0-9_]*` (at most 64 characters).
    BadName,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ShapeIssue {
    pub shape: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint_id: Option<String>,
    pub issue: ShapeIssueKind,
    pub detail: String,
}

/// The outcome of [`check`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShapeCheck {
    pub ok: bool,
    pub shapes: usize,
    /// Property shapes checked.
    pub constraints: usize,
    pub issues: Vec<ShapeIssue>,
    pub means: String,
}

/// Options of [`induce`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InduceOptions {
    /// Only entities of this workspace.
    pub workspace: Option<String>,
    /// Name of the induced shape (`<kind>_shape` when unset).
    pub name: Option<String>,
}

/// The counts one induced constraint was read from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// `<constraint id> <component>`, e.g. `issue_shape/belongs_to min_count`.
    pub constraint: String,
    pub text: String,
}

/// The outcome of [`induce`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InducedShape {
    pub kind: String,
    pub name: String,
    /// Entities the shape was read from.
    pub entities: usize,
    pub shape: Shape,
    pub evidence: Vec<Evidence>,
    /// What was observed but left out, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// One `put_shape` change, ready for a schema plan or proposal.
    pub change_set: SchemaChangeSet,
    pub means: String,
}

// ── closure view ─────────────────────────────────────────────────────────

/// The closure's class and relation facts over entities, indexed for the
/// lookups shapes make.
#[derive(Default)]
struct View {
    classes: HashMap<String, BTreeSet<String>>,
    members: HashMap<String, BTreeSet<String>>,
    out: HashMap<(String, String), BTreeSet<String>>,
    inc: HashMap<(String, String), BTreeSet<String>>,
    subjects: HashMap<String, BTreeSet<String>>,
    objects: HashMap<String, BTreeSet<String>>,
}

fn entity_term(t: &str) -> Option<&str> {
    t.strip_prefix('<')?.strip_suffix('>')
}

impl View {
    fn of(c: &Closure) -> View {
        let mut v = View::default();
        let facts = c.asserted().iter().chain(c.derived().iter().map(|d| &d.conclusion));
        for f in facts {
            let Some(s) = entity_term(&f.s) else { continue };
            if f.p == "a" {
                if let Some(class) = f.o.strip_prefix('#') {
                    v.classes.entry(s.to_string()).or_default().insert(class.to_string());
                    v.members.entry(class.to_string()).or_default().insert(s.to_string());
                }
                continue;
            }
            let Some(o) = entity_term(&f.o) else { continue };
            if reason::KEYWORDS.contains(&f.p.as_str()) || f.p.starts_with("prop.") {
                continue;
            }
            v.out.entry((s.to_string(), f.p.clone())).or_default().insert(o.to_string());
            v.inc.entry((o.to_string(), f.p.clone())).or_default().insert(s.to_string());
            v.subjects.entry(f.p.clone()).or_default().insert(s.to_string());
            v.objects.entry(f.p.clone()).or_default().insert(o.to_string());
        }
        v
    }

    fn neighbors(&self, id: &str, rel: &str, dir: Dir, out: &mut BTreeSet<String>) {
        let key = (id.to_string(), rel.to_string());
        if matches!(dir, Dir::Out | Dir::Both)
            && let Some(ids) = self.out.get(&key)
        {
            out.extend(ids.iter().cloned());
        }
        if matches!(dir, Dir::In | Dir::Both)
            && let Some(ids) = self.inc.get(&key)
        {
            out.extend(ids.iter().cloned());
        }
    }
}

// ── evaluation ───────────────────────────────────────────────────────────

/// One relation step of a path, resolved.
#[derive(Clone, Debug)]
struct StepRef {
    rel: String,
    dir: Dir,
    /// A relation only rules derive (followed in the closure only).
    derived: bool,
}

enum PathPlan {
    Title,
    State,
    Prop(String),
    Steps(Vec<StepRef>),
}

/// The values a path reaches from one focus node, deduplicated and sorted.
struct PathValues {
    /// Entity ids (relation paths) rather than literals.
    nodes: bool,
    values: Vec<Value>,
}

struct Eval<'a> {
    schema: &'a Schema,
    g: &'a Graph,
    shapes: &'a BTreeMap<String, Shape>,
    scope: &'a dyn Fn(&Entity) -> bool,
    view: Option<&'a View>,
    vocab: reason::Vocabulary,
    skipped: BTreeSet<SkippedConstraint>,
}

impl<'a> Eval<'a> {
    fn skip(&mut self, shape: &str, constraint_id: Option<&str>, key: &str, reason: impl Into<String>) {
        self.skipped.insert(SkippedConstraint {
            shape: shape.to_string(),
            constraint_id: constraint_id.map(str::to_string),
            key: key.to_string(),
            reason: reason.into(),
        });
    }

    fn inferred(&self) -> bool {
        self.view.is_some()
    }

    fn visible(&self, id: &str) -> Option<&'a Entity> {
        self.g.get(id).filter(|e| (self.scope)(e))
    }

    /// Resolve one relation step: a link type (canonical, inverse name or
    /// `^rel`) or a relation some rule derives (`^` walks it backwards).
    fn step(&self, s: &str) -> std::result::Result<StepRef, String> {
        if let Some((rel, dir)) = self.schema.resolve_rel(s) {
            return Ok(StepRef { rel, dir, derived: false });
        }
        let (base, dir) = match s.strip_prefix('^') {
            Some(b) => (b, Dir::In),
            None => (s, Dir::Out),
        };
        if self.vocab.relations.contains(base) {
            if !self.inferred() {
                return Err(format!("{base} is a relation rules derive; validate with inferred to follow it"));
            }
            return Ok(StepRef { rel: base.to_string(), dir, derived: true });
        }
        Err(format!("unknown relation {s}"))
    }

    fn plan(&self, path: &str) -> std::result::Result<PathPlan, String> {
        Ok(match ShapePath::parse(path) {
            ShapePath::Title => PathPlan::Title,
            ShapePath::State => PathPlan::State,
            ShapePath::Prop(p) if p.is_empty() => return Err("empty property name".into()),
            ShapePath::Prop(p) => PathPlan::Prop(p),
            ShapePath::Relations(steps) => {
                let mut out = Vec::with_capacity(steps.len());
                for s in &steps {
                    if s.is_empty() {
                        return Err(format!("empty step in path {path}"));
                    }
                    out.push(self.step(s)?);
                }
                PathPlan::Steps(out)
            }
        })
    }

    fn walk(&self, plan: &PathPlan, focus: &Entity) -> PathValues {
        match plan {
            PathPlan::Title => PathValues {
                nodes: false,
                values: if focus.title.is_empty() { Vec::new() } else { vec![Value::String(focus.title.clone())] },
            },
            PathPlan::State => {
                PathValues { nodes: false, values: focus.state.iter().map(|s| Value::String(s.clone())).collect() }
            }
            PathPlan::Prop(p) => PathValues {
                nodes: false,
                values: focus.props.get(p).filter(|v| !v.is_null()).cloned().into_iter().collect(),
            },
            PathPlan::Steps(steps) => {
                let mut frontier: BTreeSet<String> = BTreeSet::from([focus.id.clone()]);
                for st in steps {
                    let mut next = BTreeSet::new();
                    for id in &frontier {
                        if !st.derived {
                            for n in self.g.neighbors(id, &st.rel, st.dir) {
                                if self.visible(n).is_some() {
                                    next.insert(n.to_string());
                                }
                            }
                        }
                        if let Some(v) = self.view {
                            v.neighbors(id, &st.rel, st.dir, &mut next);
                        }
                    }
                    frontier = next;
                }
                PathValues { nodes: true, values: frontier.into_iter().map(Value::String).collect() }
            }
        }
    }

    /// Can `c` be checked as a class here? Kinds and interfaces always,
    /// derived classes only with the closure.
    fn class_check(&self, c: &str) -> std::result::Result<(), String> {
        if self.schema.is_type_or_interface(c) {
            Ok(())
        } else if self.vocab.classes.contains(c) {
            if self.inferred() {
                Ok(())
            } else {
                Err(format!("{c} is a class rules derive; validate with inferred to check it"))
            }
        } else {
            Err(format!("unknown class {c}"))
        }
    }

    /// Is entity `id` an instance of `c`: its kind, an `extends` ancestor or an
    /// implemented interface, or (with the closure) any class it has there.
    fn is_instance(&self, id: &str, c: &str) -> bool {
        self.g.get(id).is_some_and(|e| self.schema.kind_is(&e.kind, c))
            || self.view.is_some_and(|v| v.classes.get(id).is_some_and(|cs| cs.contains(c)))
    }

    /// The focus nodes of a shape's targets, and the `target_ids` that are
    /// missing or out of scope.
    fn targets(&mut self, name: &str, shape: &Shape) -> (BTreeSet<String>, Vec<String>) {
        let mut focus = BTreeSet::new();
        if let Some(t) = &shape.target {
            let known = self.schema.is_type_or_interface(t);
            if known {
                let kinds: Vec<String> =
                    self.g.kinds().map(|(k, _)| k.clone()).filter(|k| self.schema.kind_is(k, t)).collect();
                for k in &kinds {
                    focus.extend(self.g.of_kind(k).filter(|e| (self.scope)(e)).map(|e| e.id.clone()));
                }
            }
            match (self.view, known || self.vocab.classes.contains(t)) {
                (Some(v), true) => focus.extend(v.members.get(t).into_iter().flatten().cloned()),
                (None, true) if !known => {
                    self.skip(name, None, "target", format!("{t} is a class rules derive; validate with inferred"))
                }
                (_, false) => self.skip(name, None, "target", format!("unknown target {t}")),
                _ => {}
            }
        }
        let mut missing = Vec::new();
        for id in &shape.target_ids {
            if self.visible(id).is_some() {
                focus.insert(id.clone());
            } else {
                missing.push(id.clone());
            }
        }
        for (rel, subjects) in
            shape.target_subjects_of.iter().map(|r| (r, true)).chain(shape.target_objects_of.iter().map(|r| (r, false)))
        {
            let st = match self.step(rel) {
                Ok(st) => st,
                Err(why) => {
                    self.skip(name, None, "target", why);
                    continue;
                }
            };
            // Subjects of an inverse step are the stored objects.
            let (from_end, to_end) = match st.dir {
                Dir::Out => (subjects, !subjects),
                Dir::In => (!subjects, subjects),
                Dir::Both => (true, true),
            };
            if !st.derived {
                for l in self.g.links() {
                    if l.rel != st.rel || self.visible(&l.from).is_none() || self.visible(&l.to).is_none() {
                        continue;
                    }
                    if from_end {
                        focus.insert(l.from.clone());
                    }
                    if to_end {
                        focus.insert(l.to.clone());
                    }
                }
            }
            if let Some(v) = self.view {
                if from_end {
                    focus.extend(v.subjects.get(&st.rel).into_iter().flatten().cloned());
                }
                if to_end {
                    focus.extend(v.objects.get(&st.rel).into_iter().flatten().cloned());
                }
            }
        }
        (focus, missing)
    }

    /// Record the keys of `shape` that are not evaluated.
    fn static_skips(&mut self, name: &str, shape: &Shape) {
        if shape.deactivated {
            self.skip(name, None, "deactivated", "the shape is deactivated");
            return;
        }
        for ps in &shape.properties {
            if is_deactivated(ps) {
                self.skip(name, Some(&ps.id), "deactivated", "the property shape is deactivated");
                continue;
            }
            for k in unsupported_keys(ps) {
                self.skip(name, Some(&ps.id), k, format!("{k} is not a constraint this engine evaluates"));
            }
        }
    }

    /// Every result of `shape` for one focus node.
    fn focus_results(
        &mut self,
        name: &str,
        shape: &'a Shape,
        focus: &Entity,
        depth: usize,
        stack: &mut Vec<(String, String)>,
    ) -> Vec<ShapeResult> {
        let mut out = Vec::new();
        if shape.deactivated {
            self.skip(name, None, "deactivated", "the shape is deactivated");
            return out;
        }
        for ps in &shape.properties {
            self.property(name, shape, ps, focus, depth, stack, &mut out);
        }
        if shape.closed {
            let mut allowed: BTreeSet<&str> =
                shape.ignored_properties.iter().map(|p| p.strip_prefix("prop.").unwrap_or(p)).collect();
            for ps in &shape.properties {
                if let ShapePath::Prop(_) = ShapePath::parse(&ps.path) {
                    allowed.insert(ps.path.strip_prefix("prop.").unwrap_or(&ps.path));
                }
            }
            let mut keys: Vec<(&String, &Value)> = focus.props.iter().filter(|(_, v)| !v.is_null()).collect();
            keys.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in keys {
                if !allowed.contains(k.as_str()) {
                    out.push(ShapeResult {
                        shape: name.to_string(),
                        constraint_id: format!("{name}/closed"),
                        component: "closed".into(),
                        focus: focus.id.clone(),
                        path: Some(format!("prop.{k}")),
                        value: Some(v.clone()),
                        severity: shape.severity,
                        message: pick(None, &shape.message, || {
                            format!("property {k} is not allowed: shape {name} is closed")
                        }),
                    });
                }
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn property(
        &mut self,
        name: &str,
        shape: &'a Shape,
        ps: &'a PropertyShape,
        focus: &Entity,
        depth: usize,
        stack: &mut Vec<(String, String)>,
        out: &mut Vec<ShapeResult>,
    ) {
        if is_deactivated(ps) {
            self.skip(name, Some(&ps.id), "deactivated", "the property shape is deactivated");
            return;
        }
        for k in unsupported_keys(ps) {
            self.skip(name, Some(&ps.id), k, format!("{k} is not a constraint this engine evaluates"));
        }
        let plan = match self.plan(&ps.path) {
            Ok(p) => p,
            Err(why) => {
                self.skip(name, Some(&ps.id), "path", why);
                return;
            }
        };
        let PathValues { nodes, values } = self.walk(&plan, focus);
        let severity = ps.severity.unwrap_or(shape.severity);
        let mut push = |component: &str, value: Option<Value>, default: String| {
            out.push(ShapeResult {
                shape: name.to_string(),
                constraint_id: ps.id.clone(),
                component: component.to_string(),
                focus: focus.id.clone(),
                path: Some(ps.path.clone()),
                value,
                severity,
                message: pick(ps.message.as_deref(), &shape.message, || default),
            });
        };
        let path = &ps.path;
        let n = values.len();
        if let Some(m) = ps.min_count
            && n < m
        {
            push("min_count", None, format!("{n} {path} value(s), fewer than min_count {m}"));
        }
        if let Some(m) = ps.max_count
            && n > m
        {
            push("max_count", None, format!("{n} {path} value(s), more than max_count {m}"));
        }
        if let Some(c) = &ps.class {
            match self.class_check(c) {
                Err(why) => self.skip(name, Some(&ps.id), "class", why),
                Ok(()) => {
                    for v in &values {
                        match v.as_str().filter(|_| nodes) {
                            Some(id) if self.is_instance(id, c) => {}
                            Some(id) => push("class", Some(v.clone()), format!("{id} via {path} is not a {c}")),
                            None => push("class", Some(v.clone()), format!("{path} value {v} is not an object")),
                        }
                    }
                }
            }
        }
        if let Some(ty) = ps.datatype {
            for v in &values {
                if nodes {
                    push("datatype", Some(v.clone()), format!("{path} reaches an object, not a {}", type_name(ty)));
                } else if !datatype_ok(ty, v) {
                    push("datatype", Some(v.clone()), format!("{path} value {v} is not a {}", type_name(ty)));
                }
            }
        }
        if !ps.values.is_empty() {
            for v in &values {
                if !ps.values.iter().any(|x| same(x, v)) {
                    push("in", Some(v.clone()), format!("{path} value {v} is not one of {}", list(&ps.values)));
                }
            }
        }
        if let Some(h) = &ps.has_value
            && !values.iter().any(|v| same(h, v))
        {
            push("has_value", Some(h.clone()), format!("{path} has no value {h}"));
        }
        if let Some(pat) = &ps.pattern {
            match pattern_regex(pat) {
                Err(why) => self.skip(name, Some(&ps.id), "pattern", why),
                Ok(re) => {
                    for v in &values {
                        if !re.is_match(&text_of(v)) {
                            push("pattern", Some(v.clone()), format!("{path} value {v} does not match {pat}"));
                        }
                    }
                }
            }
        }
        for v in &values {
            let len = text_of(v).chars().count();
            if let Some(m) = ps.min_length
                && len < m
            {
                push("min_length", Some(v.clone()), format!("{path} value {v} is shorter than {m} characters"));
            }
            if let Some(m) = ps.max_length
                && len > m
            {
                push("max_length", Some(v.clone()), format!("{path} value {v} is longer than {m} characters"));
            }
        }
        if ps.min_value.is_some() || ps.max_value.is_some() {
            for v in &values {
                let x = if nodes { None } else { v.as_f64() };
                let Some(x) = x else {
                    let component = if ps.min_value.is_some() { "min_value" } else { "max_value" };
                    push(component, Some(v.clone()), format!("{path} value {v} is not a number"));
                    continue;
                };
                if let Some(m) = ps.min_value
                    && x < m
                {
                    push("min_value", Some(v.clone()), format!("{path} value {v} is below {m}"));
                }
                if let Some(m) = ps.max_value
                    && x > m
                {
                    push("max_value", Some(v.clone()), format!("{path} value {v} is above {m}"));
                }
            }
        }
        if let Some(node) = &ps.node {
            let shapes = self.shapes;
            let Some(inner) = shapes.get(node) else {
                self.skip(name, Some(&ps.id), "node", format!("unknown shape {node}"));
                return;
            };
            let mut node_results = Vec::new();
            for v in &values {
                let Some(id) = v.as_str().filter(|_| nodes) else {
                    node_results.push((v.clone(), format!("{path} value {v} is not an object")));
                    continue;
                };
                if depth + 1 > MAX_NODE_DEPTH {
                    self.skip(
                        name,
                        Some(&ps.id),
                        "node",
                        format!("node shapes nest deeper than {MAX_NODE_DEPTH}; the check stops there"),
                    );
                    continue;
                }
                // A node already being checked against this shape is assumed to
                // conform (recursion is cut, not failed).
                if stack.iter().any(|(s, e)| s == node && e == id) {
                    continue;
                }
                let Some(e) = self.g.get(id) else { continue };
                self.static_skips(node, inner);
                stack.push((node.clone(), id.to_string()));
                let nested = self.focus_results(node, inner, e, depth + 1, stack);
                stack.pop();
                if let Some(first) = nested.first() {
                    node_results.push((
                        v.clone(),
                        format!(
                            "{id} via {path} does not conform to shape {node} ({} result(s); first: {})",
                            nested.len(),
                            first.message
                        ),
                    ));
                }
            }
            let mut push = |value: Value, default: String| {
                out.push(ShapeResult {
                    shape: name.to_string(),
                    constraint_id: ps.id.clone(),
                    component: "node".into(),
                    focus: focus.id.clone(),
                    path: Some(ps.path.clone()),
                    value: Some(value),
                    severity,
                    message: pick(ps.message.as_deref(), &shape.message, || default),
                });
            };
            for (v, msg) in node_results {
                push(v, msg);
            }
        }
    }
}

fn is_deactivated(ps: &PropertyShape) -> bool {
    ps.unknown.get("deactivated").and_then(Value::as_bool).unwrap_or(false)
}

/// Keys of `ps` that would constrain but are not evaluated.
fn unsupported_keys(ps: &PropertyShape) -> impl Iterator<Item = &str> {
    ps.unknown
        .iter()
        .filter(|(k, v)| !ANNOTATION_KEYS.contains(&k.as_str()) && !(k.as_str() == "deactivated" && v.is_boolean()))
        .map(|(k, _)| k.as_str())
}

/// A message for a result: the property shape's, the shape's, or a default.
fn pick(own: Option<&str>, shape: &str, default: impl FnOnce() -> String) -> String {
    match own.filter(|m| !m.is_empty()) {
        Some(m) => m.to_string(),
        None if !shape.is_empty() => shape.to_string(),
        None => default(),
    }
}

/// Equal JSON values, with numbers compared by value (`1` equals `1.0`).
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) if a.is_number() && b.is_number() => x == y,
        _ => a == b,
    }
}

/// The text a `pattern` or length constraint reads: a string as is, any
/// other value as compact JSON.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Order of scalar JSON types in an induced `in` list: booleans, numbers,
/// strings, anything else.
fn scalar_rank(v: &Value) -> u8 {
    match v {
        Value::Bool(_) => 0,
        Value::Number(_) => 1,
        Value::String(_) => 2,
        _ => 3,
    }
}

/// A total order on scalar values, numbers by value.
fn cmp_scalar(a: &Value, b: &Value) -> std::cmp::Ordering {
    scalar_rank(a).cmp(&scalar_rank(b)).then_with(|| match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Number(x), Value::Number(y)) => x.as_f64().unwrap_or(0.0).total_cmp(&y.as_f64().unwrap_or(0.0)),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        _ => a.to_string().cmp(&b.to_string()),
    })
}

fn list(values: &[Value]) -> String {
    values.iter().map(text_of).collect::<Vec<_>>().join("|")
}

fn type_name(ty: PropType) -> String {
    format!("{ty:?}").to_lowercase()
}

/// Does a literal fit a datatype? The JSON shape is checked (`enum` and
/// `text` are strings, `timestamp` an integer or a string); values of an enum
/// are checked by `in`, not here.
pub fn datatype_ok(ty: PropType, v: &Value) -> bool {
    match ty {
        PropType::String | PropType::Text | PropType::Enum => v.is_string(),
        PropType::Int => v.is_i64() || v.is_u64(),
        PropType::Float => v.is_number(),
        PropType::Bool => v.is_boolean(),
        PropType::Timestamp => v.is_i64() || v.is_string(),
        PropType::List => v.is_array(),
        PropType::Json => true,
    }
}

fn has_target(s: &Shape) -> bool {
    s.target.is_some() || !s.target_ids.is_empty() || s.target_subjects_of.is_some() || s.target_objects_of.is_some()
}

/// The schema's shapes overlaid with drafts, every property shape with an id.
fn effective_shapes(schema: &Schema, drafts: &BTreeMap<String, Shape>) -> BTreeMap<String, Shape> {
    let mut all = schema.shapes.clone();
    for (name, s) in drafts {
        all.insert(name.clone(), s.clone());
    }
    for (name, s) in all.iter_mut() {
        s.normalize(name);
    }
    all
}

/// Evaluate shapes over the entities `can_read` admits (and, with a
/// workspace, only that workspace's).
///
/// With `q.inferred` the closure is read too; when `closure` is `None` one is
/// computed here under the same scope. A given closure must have been
/// computed under that scope.
pub fn validate(
    schema: &Schema,
    g: &Graph,
    closure: Option<&Closure>,
    q: &ShapesQuery,
    can_read: &dyn Fn(&Entity) -> bool,
) -> ShapeReport {
    let shapes = effective_shapes(schema, &q.drafts);
    let ws = q.workspace.as_deref();
    let scope = |e: &Entity| ws.is_none_or(|w| e.workspace == w) && can_read(e);
    let computed;
    let closure = match (q.inferred, closure) {
        (false, _) => None,
        (true, Some(c)) => Some(c),
        (true, None) => {
            let opts = ReasonOptions { workspace: q.workspace.clone(), ..ReasonOptions::default() };
            computed = reason::closure(schema, g, &opts, can_read);
            Some(&computed)
        }
    };
    let view = closure.map(View::of);
    let mut ev = Eval {
        schema,
        g,
        shapes: &shapes,
        scope: &scope,
        view: view.as_ref(),
        vocab: reason::vocabulary(schema),
        skipped: BTreeSet::new(),
    };
    if let Some(c) = closure {
        let refused = c.stats().refused;
        if refused > 0 {
            ev.skip(
                "*",
                None,
                "closure",
                format!("{refused} term(s) could not be spelled; facts about them are missing from the closure"),
            );
        }
    }
    let mut selected: Vec<&String> = Vec::new();
    if q.shapes.is_empty() {
        selected.extend(shapes.keys());
    } else {
        let wanted: BTreeSet<&String> = q.shapes.iter().collect();
        for name in wanted {
            match shapes.get_key_value(name) {
                Some((k, _)) => selected.push(k),
                None => ev.skip(name, None, "shape", format!("unknown shape {name}")),
            }
        }
    }
    let mut results = Vec::new();
    let mut focus_by_shape = BTreeMap::new();
    let mut unmatched = Vec::new();
    for name in selected {
        let shape = &shapes[name];
        ev.static_skips(name, shape);
        if shape.deactivated || !has_target(shape) {
            continue;
        }
        let (focus, missing) = ev.targets(name, shape);
        focus_by_shape.insert(name.clone(), focus.len());
        if focus.is_empty() && missing.is_empty() {
            unmatched.push(name.clone());
        }
        for id in missing {
            results.push(ShapeResult {
                shape: name.clone(),
                constraint_id: format!("{name}/target"),
                component: "target_missing".into(),
                focus: id.clone(),
                path: None,
                value: None,
                severity: shape.severity,
                message: pick(None, &shape.message, || format!("target {id} does not exist or is not visible")),
            });
        }
        for id in &focus {
            let Some(e) = g.get(id) else { continue };
            let mut stack = vec![(name.clone(), id.clone())];
            results.extend(ev.focus_results(name, shape, e, 0, &mut stack));
        }
    }
    results.sort_by(|a, b| {
        (&a.shape, &a.focus, &a.constraint_id, &a.component)
            .cmp(&(&b.shape, &b.focus, &b.constraint_id, &b.component))
            .then_with(|| a.value.as_ref().map(text_of).cmp(&b.value.as_ref().map(text_of)))
    });
    let mut counts = SeverityCounts::default();
    for r in &results {
        match r.severity {
            ShapeSeverity::Violation => counts.violation += 1,
            ShapeSeverity::Warning => counts.warning += 1,
            ShapeSeverity::Info => counts.info += 1,
        }
    }
    let total_results = results.len();
    let truncated = total_results > q.limit;
    results.truncate(q.limit);
    let focus_nodes = focus_by_shape.values().sum();
    let skipped: Vec<SkippedConstraint> = ev.skipped.into_iter().collect();
    let fixpoint_reached = closure.map(Closure::fixpoint_reached);
    // One selected shape whose target matched nothing leaves part of the
    // question unanswered, however many other focus nodes there were.
    let undetermined = skipped.iter().any(|s| !s.is_deactivation()) || focus_nodes == 0 || !unmatched.is_empty();
    // A lower-bound closure can both hide and invent results (a class or a
    // min_count the missing facts would satisfy), so it answers nothing.
    let verdict = if fixpoint_reached == Some(false) {
        ShapeVerdict::Undetermined
    } else if total_results > 0 {
        ShapeVerdict::Violates
    } else if undetermined {
        ShapeVerdict::Undetermined
    } else {
        ShapeVerdict::Conforms
    };
    ShapeReport {
        verdict,
        conforms: match verdict {
            ShapeVerdict::Conforms => Some(true),
            ShapeVerdict::Violates => Some(false),
            ShapeVerdict::Undetermined => None,
        },
        results,
        total_results,
        truncated,
        focus_by_shape,
        focus_nodes,
        unmatched_targets: unmatched,
        skipped,
        counts,
        workspace: q.workspace.clone(),
        inferred: q.inferred,
        fixpoint_reached,
        means: REPORT_MEANS.into(),
    }
}

/// Results of `after` that `before` lacks (new) and the reverse (resolved).
pub fn delta(before: &ShapeReport, after: &ShapeReport) -> ShapeDelta {
    type Key = (String, String, String, String, Option<String>, Option<String>);
    let key = |r: &ShapeResult| -> Key {
        (
            r.shape.clone(),
            r.constraint_id.clone(),
            r.component.clone(),
            r.focus.clone(),
            r.path.clone(),
            r.value.as_ref().map(|v| v.to_string()),
        )
    };
    let b: BTreeSet<Key> = before.results.iter().map(key).collect();
    let a: BTreeSet<Key> = after.results.iter().map(key).collect();
    ShapeDelta {
        before: before.verdict,
        after: after.verdict,
        new_results: after.results.iter().filter(|r| !b.contains(&key(r))).cloned().collect(),
        resolved: before.results.iter().filter(|r| !a.contains(&key(r))).cloned().collect(),
        complete: !before.truncated && !after.truncated,
        means: DELTA_MEANS.into(),
    }
}

// ── check ────────────────────────────────────────────────────────────────

fn known_prop(schema: &Schema, name: &str) -> bool {
    schema.object_types.values().any(|t| t.properties.contains_key(name))
        || schema.interfaces.values().any(|i| i.properties.contains_key(name))
}

/// A structural dry run of shape definitions against `schema`: every
/// referenced kind, relation, property and shape exists, every key is
/// evaluated, every pattern compiles and every bound is ordered. Shapes named
/// in `shapes` replace the schema's of the same name; `node` may reference
/// either.
pub fn check(schema: &Schema, shapes: &BTreeMap<String, Shape>) -> ShapeCheck {
    let vocab = reason::vocabulary(schema);
    let class_ok = |c: &str| schema.is_type_or_interface(c) || vocab.classes.contains(c);
    let rel_ok =
        |r: &str| schema.resolve_rel(r).is_some() || vocab.relations.contains(r.strip_prefix('^').unwrap_or(r));
    let mut issues = BTreeSet::new();
    let mut constraints = 0;
    for (name, raw) in shapes {
        let mut shape = raw.clone();
        shape.normalize(name);
        let mut issue = |cid: Option<&str>, issue: ShapeIssueKind, detail: String| {
            issues.insert(ShapeIssue { shape: name.clone(), constraint_id: cid.map(str::to_string), issue, detail });
        };
        if !schema.shapes.contains_key(name) && !valid_name(name) {
            issue(None, ShapeIssueKind::BadName, format!("{name:?} is not a valid name ([a-z][a-z0-9_]*, at most 64)"));
        }
        if let Some(t) = &shape.target
            && !class_ok(t)
        {
            issue(
                None,
                ShapeIssueKind::MissingKind,
                format!("target {t} is not an object type, interface or derived class"),
            );
        }
        for r in shape.target_subjects_of.iter().chain(&shape.target_objects_of) {
            if !rel_ok(r) {
                issue(
                    None,
                    ShapeIssueKind::MissingRelation,
                    format!("target relation {r} is not a link type or derived relation"),
                );
            }
        }
        if shape.target_ids.iter().any(String::is_empty) {
            issue(None, ShapeIssueKind::BadTargetId, "target_ids contains an empty id".into());
        }
        // Kinds a `prop.*` path must fit, when the shape targets exactly one
        // declared type or interface.
        let only_kind =
            match (&shape.target, shape.target_ids.is_empty(), &shape.target_subjects_of, &shape.target_objects_of) {
                (Some(t), true, None, None) if schema.is_type_or_interface(t) => Some(t.clone()),
                _ => None,
            };
        let mut ids = BTreeSet::new();
        for (i, ps) in raw.properties.iter().enumerate() {
            constraints += 1;
            let cid = Some(shape.properties[i].id.as_str());
            // Compared after normalization, as compile does: an explicit id
            // can collide with another property's default `<shape>/<index>`.
            let nid = shape.properties[i].id.as_str();
            if !ids.insert(nid) {
                issue(cid, ShapeIssueKind::DuplicateId, format!("property shape id {nid} is used twice"));
            }
            match ShapePath::parse(&ps.path) {
                ShapePath::Title | ShapePath::State => {}
                ShapePath::Prop(p) => {
                    if !known_prop(schema, &p) {
                        issue(cid, ShapeIssueKind::MissingProperty, format!("no type declares property {p}"));
                    } else if let Some(t) = &only_kind {
                        let kinds = schema.concrete_kinds(t);
                        let iface = schema.interfaces.get(t).is_some_and(|i| i.properties.contains_key(&p));
                        if !iface && !kinds.is_empty() && !kinds.iter().any(|k| schema.prop_def(k, &p).is_some()) {
                            issue(cid, ShapeIssueKind::MissingProperty, format!("{t} objects have no property {p}"));
                        }
                    }
                }
                ShapePath::Relations(steps) => {
                    for s in &steps {
                        if s.is_empty() {
                            issue(cid, ShapeIssueKind::MissingRelation, format!("empty step in path {:?}", ps.path));
                        } else if !rel_ok(s) {
                            issue(
                                cid,
                                ShapeIssueKind::MissingRelation,
                                format!("{s} in path {} is not a link type or derived relation", ps.path),
                            );
                        }
                    }
                }
            }
            if let Some(c) = &ps.class
                && !class_ok(c)
            {
                issue(
                    cid,
                    ShapeIssueKind::MissingKind,
                    format!("class {c} is not an object type, interface or derived class"),
                );
            }
            if let Some(n) = &ps.node
                && !shapes.contains_key(n)
                && !schema.shapes.contains_key(n)
            {
                issue(cid, ShapeIssueKind::MissingShape, format!("node shape {n} does not exist"));
            }
            if let Some(p) = &ps.pattern
                && let Err(e) = pattern_regex(p)
            {
                issue(cid, ShapeIssueKind::BadPattern, e);
            }
            let bounds = [
                ("count", ps.min_count.map(|x| x as f64), ps.max_count.map(|x| x as f64)),
                ("length", ps.min_length.map(|x| x as f64), ps.max_length.map(|x| x as f64)),
                ("value", ps.min_value, ps.max_value),
            ];
            for (what, lo, hi) in bounds {
                if let (Some(lo), Some(hi)) = (lo, hi)
                    && lo > hi
                {
                    issue(cid, ShapeIssueKind::BadBounds, format!("min_{what} {lo} exceeds max_{what} {hi}"));
                }
            }
            for k in unsupported_keys(ps) {
                issue(
                    cid,
                    ShapeIssueKind::UnknownKey,
                    format!(
                        "{k} is not a constraint this engine evaluates; it would be skipped and leave the report undetermined"
                    ),
                );
            }
        }
    }
    let issues: Vec<ShapeIssue> = issues.into_iter().collect();
    ShapeCheck { ok: issues.is_empty(), shapes: shapes.len(), constraints, issues, means: CHECK_MEANS.into() }
}

// ── induction ────────────────────────────────────────────────────────────

/// `1 issue`, `37 issues`.
fn count(n: usize, kind: &str) -> String {
    if n == 1 { format!("1 {kind}") } else { format!("{n} {}", crate::modeling::plural(kind)) }
}

/// The class every value kind shares: the kind itself, else the most specific
/// common `extends` ancestor, else the one interface they all implement.
fn common_class(schema: &Schema, kinds: &BTreeSet<String>) -> Option<(String, &'static str)> {
    let first = kinds.iter().next()?;
    if kinds.len() == 1 {
        return schema.object_types.contains_key(first).then(|| (first.clone(), "kind"));
    }
    let chain: Vec<String> = schema.type_chain(first).iter().map(|t| t.name.clone()).collect();
    if let Some(t) = chain.iter().find(|t| kinds.iter().all(|k| schema.kind_is(k, t))) {
        return Some((t.clone(), "ancestor"));
    }
    let shared: Vec<&String> =
        schema.interfaces.keys().filter(|i| kinds.iter().all(|k| schema.kind_is(k, i))).collect();
    match shared.as_slice() {
        [one] => Some(((*one).clone(), "interface")),
        _ => None,
    }
}

/// The datatype every value fits: the declared one when all fit it, else
/// the one their JSON says.
fn observed_type(declared: Option<PropType>, values: &[&Value]) -> Option<(PropType, bool)> {
    if let Some(ty) = declared.filter(|t| *t != PropType::Json)
        && values.iter().all(|v| datatype_ok(ty, v))
    {
        return Some((ty, true));
    }
    let all = |f: fn(&Value) -> bool| values.iter().all(|v| f(v));
    let ty = if all(Value::is_string) {
        PropType::String
    } else if all(|v| v.is_i64() || v.is_u64()) {
        PropType::Int
    } else if all(Value::is_number) {
        PropType::Float
    } else if all(Value::is_boolean) {
        PropType::Bool
    } else if all(Value::is_array) {
        PropType::List
    } else {
        return None;
    };
    Some((ty, false))
}

/// Induce a shape for `kind` (an object type or interface) from the entities
/// `can_read` admits: what every one of them has, what none has twice, the
/// class of what they link to, the datatypes and the small value sets they
/// use. Never a `pattern`, never value ranges: observations are evidence, not
/// rules.
pub fn induce(
    schema: &Schema,
    g: &Graph,
    kind: &str,
    opts: &InduceOptions,
    can_read: &dyn Fn(&Entity) -> bool,
) -> Result<InducedShape> {
    if !schema.is_type_or_interface(kind) {
        return Err(Error::not_found(format!("object type or interface {kind}")));
    }
    let name = opts.name.clone().unwrap_or_else(|| format!("{kind}_shape"));
    if !valid_name(&name) {
        return Err(Error::invalid(format!("shape name {name:?} is not [a-z][a-z0-9_]* (at most 64 characters)")));
    }
    let ws = opts.workspace.as_deref();
    let scope = |e: &Entity| ws.is_none_or(|w| e.workspace == w) && can_read(e);
    let empty = BTreeMap::new();
    let ev = Eval {
        schema,
        g,
        shapes: &empty,
        scope: &scope,
        view: None,
        vocab: reason::vocabulary(schema),
        skipped: BTreeSet::new(),
    };
    let mut kinds: Vec<&String> = g.kinds().map(|(k, _)| k).filter(|k| schema.kind_is(k, kind)).collect();
    kinds.sort();
    let mut population: Vec<&Entity> = kinds.iter().flat_map(|k| g.of_kind(k)).filter(|e| scope(e)).collect();
    population.sort_by(|a, b| a.id.cmp(&b.id));
    let n = population.len();
    let mut notes = Vec::new();

    // Candidate paths, in output order: title, state, properties, relations.
    let mut props = BTreeSet::new();
    let mut rels = BTreeSet::new();
    let mut undeclared_props = BTreeSet::new();
    let mut undeclared_rels = BTreeSet::new();
    let mut has_state = false;
    for e in &population {
        has_state |= e.state.is_some();
        for (k, v) in &e.props {
            if v.is_null() {
                continue;
            }
            if known_prop(schema, k) {
                props.insert(k.clone());
            } else {
                undeclared_props.insert(k.clone());
            }
        }
        for st in g.steps(&e.id) {
            if ev.visible(st.other).is_none() {
                continue;
            }
            if schema.link_types.contains_key(st.rel) {
                rels.insert(schema.display_rel(st.rel, st.dir));
            } else {
                undeclared_rels.insert(st.rel.to_string());
            }
        }
    }
    for p in &undeclared_props {
        notes.push(format!(
            "property {p} is not declared by any type: left out (a shape may only name declared properties)"
        ));
    }
    for r in &undeclared_rels {
        notes.push(format!("relation {r} is not a declared link type: left out"));
    }
    let mut paths: Vec<String> = vec!["title".into()];
    if has_state {
        paths.push("state".into());
    }
    paths.extend(props.iter().map(|p| format!("prop.{p}")));
    paths.extend(rels);

    let kinds_text = count(n, kind);
    let mut evidence = Vec::new();
    let mut properties = Vec::new();
    for path in paths {
        let Ok(plan) = ev.plan(&path) else { continue };
        let per: Vec<PathValues> = population.iter().map(|e| ev.walk(&plan, e)).collect();
        let relation = matches!(plan, PathPlan::Steps(_));
        let id = format!("{name}/{path}");
        let mut ps = PropertyShape { id: id.clone(), path: path.clone(), ..Default::default() };
        let mut say = |component: &str, text: String| {
            evidence.push(Evidence { constraint: format!("{id} {component}"), text });
        };
        let having = per.iter().filter(|p| !p.values.is_empty()).count();
        let most = per.iter().map(|p| p.values.len()).max().unwrap_or(0);
        let what = if relation { format!("a {path} link") } else { format!("a {path}") };
        if n > 0 && having == n {
            ps.min_count = Some(1);
            say("min_count", format!("{having} of {kinds_text} have {what}"));
        }
        if relation && having > 0 && most <= 1 && having * 2 >= n {
            ps.max_count = Some(1);
            say("max_count", format!("{having} of {kinds_text} have exactly one {path} link and none has more"));
        }
        let values: Vec<&Value> = per.iter().flat_map(|p| p.values.iter()).collect();
        if relation && !values.is_empty() {
            let linked: BTreeSet<String> =
                values.iter().filter_map(|v| v.as_str()).filter_map(|id| g.get(id)).map(|e| e.kind.clone()).collect();
            let objects: BTreeSet<&str> = values.iter().filter_map(|v| v.as_str()).collect();
            match common_class(schema, &linked) {
                Some((c, how)) => {
                    ps.class = Some(c.clone());
                    let how = match how {
                        "kind" => format!("are {}", crate::modeling::plural(&c)),
                        "ancestor" => format!("are kinds of {c}"),
                        _ => format!("implement {c}"),
                    };
                    say("class", format!("all {} objects reached by {path} {how}", objects.len()));
                }
                None => notes.push(format!(
                    "{path} reaches objects of kinds {} with no single declared class in common: no class constraint",
                    linked.iter().cloned().collect::<Vec<_>>().join(", ")
                )),
            }
        }
        if let PathPlan::Prop(p) = &plan
            && !values.is_empty()
        {
            let declared = if schema.is_interface(kind) {
                schema.interfaces.get(kind).and_then(|i| i.properties.get(p)).map(|d| d.ty)
            } else {
                schema.prop_def(kind, p).map(|d| d.ty)
            };
            match observed_type(declared, &values) {
                Some((ty, as_declared)) => {
                    ps.datatype = Some(ty);
                    let tail = if as_declared { " (as declared)" } else { "" };
                    say("datatype", format!("all {} {p} values are {}{tail}", values.len(), type_name(ty)));
                }
                None => notes.push(format!("{p} values have mixed JSON types: no datatype constraint")),
            }
        }
        let enumerable = matches!(plan, PathPlan::Prop(_) | PathPlan::State)
            && values.iter().all(|v| v.is_string() || v.is_number() || v.is_boolean());
        if enumerable && !values.is_empty() {
            // Distinct by the equality `in` is validated with ([`same`]), so 1
            // and "1" stay apart and every source value is listed.
            let mut distinct: Vec<Value> = Vec::new();
            for v in &values {
                if !distinct.iter().any(|d| same(d, v)) {
                    distinct.push((*v).clone());
                    if distinct.len() > 12 {
                        break;
                    }
                }
            }
            distinct.sort_by(cmp_scalar);
            let d = distinct.len();
            if (2..=12).contains(&d) && values.len() >= 5 * d {
                // Mixed JSON types are shown as JSON so 1 and "1" read apart.
                let mixed = distinct.iter().any(|v| scalar_rank(v) != scalar_rank(&distinct[0]));
                let names: Vec<String> =
                    distinct.iter().map(|v| if mixed { v.to_string() } else { text_of(v) }).collect();
                ps.values = distinct;
                say("in", format!("{} {path} values take {d} distinct values ({})", values.len(), names.join(", ")));
            }
        }
        let constrained = ps.min_count.is_some()
            || ps.max_count.is_some()
            || ps.class.is_some()
            || ps.datatype.is_some()
            || !ps.values.is_empty();
        if constrained {
            properties.push(ps);
        }
    }
    if n == 0 {
        notes.push(format!("no {kind} objects are visible in this scope: nothing to induce"));
    }
    if schema.shapes.contains_key(&name) {
        notes.push(format!("shape {name} exists: saving this change set replaces it"));
    }
    let shape = Shape {
        description: format!("Induced from {kinds_text}: describes the data as it is, not what it should be."),
        target: Some(kind.to_string()),
        properties,
        ..Default::default()
    };
    let change_set = SchemaChangeSet {
        changes: vec![SchemaChange::PutShape { name: name.clone(), def: shape.clone() }],
        message: format!("induce shape {name} from {kinds_text}"),
        ..Default::default()
    };
    Ok(InducedShape {
        kind: kind.to_string(),
        name,
        entities: n,
        shape,
        evidence,
        notes,
        change_set,
        means: INDUCE_MEANS.into(),
    })
}

// ── Ontology ─────────────────────────────────────────────────────────────

impl Ontology {
    /// Validate the schema's shapes (and any drafts) over what `p` may read.
    pub fn shapes_validate(&self, p: &Principal, q: &ShapesQuery) -> Result<ShapeReport> {
        if let Some(ws) = &q.workspace
            && !p.in_workspace(ws)
        {
            return Err(Error::Denied(format!("{} has no access to workspace {ws}", p.id)));
        }
        let schema = self.schema();
        let known = |n: &String| schema.shapes.contains_key(n) || q.drafts.contains_key(n);
        if let Some(missing) = q.shapes.iter().find(|n| !known(n)) {
            return Err(Error::not_found(format!("shape {missing}")));
        }
        let g = self.reader(p)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        // The closure is computed under this same read guard, so it describes
        // exactly the graph the shapes are evaluated over.
        Ok(validate(&schema, &g, None, q, &can))
    }

    /// A structural dry run of shape definitions against the schema in force.
    pub fn shapes_check(&self, shapes: &BTreeMap<String, Shape>) -> ShapeCheck {
        check(&self.schema(), shapes)
    }

    /// Induce a shape for `kind` from what `p` may read.
    pub fn shapes_induce(&self, p: &Principal, kind: &str, opts: &InduceOptions) -> Result<InducedShape> {
        if let Some(ws) = &opts.workspace
            && !p.in_workspace(ws)
        {
            return Err(Error::Denied(format!("{} has no access to workspace {ws}", p.id)));
        }
        let schema = self.schema();
        let g = self.reader(p)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        induce(&schema, &g, kind, opts, &can)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_compare_by_value() {
        assert!(same(&json!(1), &json!(1.0)));
        assert!(!same(&json!("1"), &json!(1)));
        assert!(same(&json!("a"), &json!("a")));
    }

    #[test]
    fn datatypes_read_json_shapes() {
        assert!(datatype_ok(PropType::Int, &json!(3)));
        assert!(!datatype_ok(PropType::Int, &json!(3.5)));
        assert!(datatype_ok(PropType::Float, &json!(3)));
        assert!(datatype_ok(PropType::Enum, &json!("high")));
        assert!(!datatype_ok(PropType::Bool, &json!("true")));
        assert!(datatype_ok(PropType::List, &json!([1])));
    }

    #[test]
    fn annotations_and_deactivation_are_not_unsupported() {
        let ps: PropertyShape = serde_json::from_value(
            json!({ "path": "title", "name": "Title", "order": 1, "deactivated": false, "qualified_min_count": 1 }),
        )
        .unwrap();
        assert_eq!(unsupported_keys(&ps).collect::<Vec<_>>(), vec!["qualified_min_count"]);
        assert!(!is_deactivated(&ps));
        let off: PropertyShape = serde_json::from_value(json!({ "path": "title", "deactivated": true })).unwrap();
        assert!(is_deactivated(&off));
    }

    #[test]
    fn common_classes() {
        let s = Schema::enterprise();
        let set = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<BTreeSet<_>>();
        assert_eq!(common_class(&s, &set(&["project"])), Some(("project".into(), "kind")));
        assert_eq!(common_class(&s, &set(&["issue", "task"])), Some(("work_item".into(), "interface")));
        assert_eq!(common_class(&s, &set(&["issue", "person"])), None);
    }

    #[test]
    fn observed_types() {
        let (a, b, c) = (json!(1), json!(2.5), json!("x"));
        assert_eq!(observed_type(None, &[&a]), Some((PropType::Int, false)));
        assert_eq!(observed_type(None, &[&a, &b]), Some((PropType::Float, false)));
        assert_eq!(observed_type(Some(PropType::Enum), &[&c]), Some((PropType::Enum, true)));
        assert_eq!(observed_type(Some(PropType::Int), &[&c]), Some((PropType::String, false)));
        assert_eq!(observed_type(None, &[&a, &c]), None);
    }
}
