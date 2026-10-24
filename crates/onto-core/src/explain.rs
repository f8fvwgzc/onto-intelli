//! Explanations over a reasoning closure: why a fact holds, how far it can be
//! trusted, whether a claim is backed by the graph, and whether a slice of the
//! graph keeps what the whole graph entails.
//!
//! * **Justifications** ([`justify`]) are minimal sets of asserted facts from
//!   which the closure's rule table derives a fact. Schema facts
//!   (`#incident extends #issue`, `depends_on a transitive`) are asserted facts
//!   and appear in justifications like any other. One justification comes from
//!   deletion-based minimization of a witness; further ones from Reiter's
//!   hitting-set tree, bounded by `max_justifications` and `max_steps`. Three
//!   words, three claims: `sufficient` (an independent evaluator re-derives the
//!   fact from exactly these facts), `minimal` (it fails to once any one of them
//!   is removed, each re-derived) and `complete` (every minimal justification
//!   was enumerated; only claimed when no bound stopped the search and the
//!   closure reached its fixpoint).
//! * **Provenance** ([`provenance`]) evaluates a fact's support in a semiring:
//!   `why` (the minimal witness sets), `trust` (the best derivation's weakest
//!   premise, weights from each entity's `confidence` property) and `count`
//!   (the number of derivation trees, a lower bound when the support has a
//!   cycle).
//! * **Verification** ([`verify`]) checks claims written in schema vocabulary
//!   (an LLM's answer, an analyst's note) against the graph and the closure:
//!   `asserted`, `derived` (with the derivation down to asserted leaves),
//!   `contradicted` (a stored single-valued fact says otherwise),
//!   `not_entailed` or `outside_vocabulary`. A claimed reasoning step (rule and
//!   premises) gets a label of its own, so an invented premise is told apart
//!   from a misapplied rule. A reasoning trace is a list of such steps: each
//!   is labelled against the graph, so a step resting on an earlier invented
//!   conclusion is caught too.
//! * **Slices** ([`slice_check`]) compare the closure of the whole visible
//!   graph with the closure of the facts whose entities all lie in a slice:
//!   which goals the slice preserves and which it loses, with the premises the
//!   loss is missing. Coverage is a proxy; preservation is the property.
//!
//! Nothing here writes. An inference is not an assertion: every report labels
//! what is asserted and what is derived, and none claims completeness for a
//! closure that stopped short of its fixpoint. The engine's words are
//! `derived` and `sound_step`; certificate verdicts belong to the checker.
//!
//! The support of a fact is enumerated by matching every rule's body against
//! the closure (backwards from the fact), so it includes rule instances that
//! re-derive asserted facts, which [`ReasonOptions::capture_all`] does not
//! record. Any closure works; `capture_all` is not needed.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::graph::Graph;
use crate::model::{Entity, Id};
use crate::policy::{self, Principal};
use crate::reason::{
    self, AtomPattern, Closure, Derivation, Fact, ReasonOptions, RuleLine, SUPPLIED_PREFIX, Status, Vocabulary,
};
use crate::schema::{Cardinality, Dir, Schema};
use crate::{Error, Ontology, Result};

/// What a justification report claims.
pub const JUSTIFY_MEANS: &str = "A justification is a set of asserted facts (schema facts included) from which the \
     closure's rule table derives the fact. sufficient: an independent evaluator re-derives the fact from exactly \
     these facts. minimal: it no longer does once any one of them is removed (each re-derived). complete: every \
     minimal justification was enumerated, claimed only when no bound stopped the search and the closure reached its \
     fixpoint. Supplied rules (schema/...) are assumptions the derivation rests on, not facts it establishes.";

const WHY_MEANS: &str = "why-provenance: the minimal sets of asserted facts the fact can be derived from (see \
     justifications); exact when the enumeration was complete.";
const TRUST_MEANS: &str = "trust-provenance: the best derivation's weakest premise. An asserted fact weighs its \
     entity's confidence property when it lies in [0, 1] (a link: the link's own confidence, else the lower of its \
     endpoints'), else 1.0; schema facts weigh 1.0. A confidence is an opinion recorded in the graph; this value \
     propagates it, it does not measure anything.";
const COUNT_MEANS: &str = "count-provenance: the number of distinct derivation trees (an asserted fact is one tree \
     of its own). With a cycle in the support there are infinitely many trees; the value then counts only trees \
     whose facts become derivable in strictly increasing order, a lower bound, and exact is false.";

/// What a verification report claims.
pub const VERIFY_MEANS: &str = "Each claim is compared with the graph and its closure under the rule table. \
     asserted and derived are lookups (derived carries its derivation down to asserted leaves); contradicted reads a \
     stored single-valued fact that says otherwise (a state, a property value, a cardinality-one link, an entity's \
     kind); not_entailed is absence from this closure under its rules, not falsity; outside_vocabulary means the \
     claim uses relations, classes, properties or entities this schema and this reader do not have, or an entity \
     whose id the reasoner refuses to spell (its reason says so). A step label \
     says whether the claimed rule and premises are a sound step over the graph; it is the engine's check, not a \
     certificate verdict.";

/// What a slice report claims.
pub const SLICE_MEANS: &str = "Coverage is a proxy; preservation is the property. preserved: the slice's own \
     closure (facts whose entities all lie in the slice, plus schema facts) entails the goal as the full visible \
     graph does. lost: the full graph entails it and the slice does not; missing_premises are the asserted leaves of \
     the full derivation the slice lacks. slice_only would mean the slice entails what the whole does not, which \
     monotone rules make impossible: it is reported loudly when it happens. coverage is the share of asserted facts \
     touching slice entities that the slice keeps.";

/// Join steps (candidate facts examined) the independent evaluator may take
/// while verifying the justifications of one fact; past it,
/// `sufficient`/`minimal` stay false (not established) and the report says
/// why.
const VERIFY_BUDGET: usize = 5_000_000;
/// Propagation steps the hitting-set search may take over the support cone.
const SEARCH_WORK: usize = 50_000_000;
/// Facts listed per sample (missing premises, unknown ids, alarms).
const SAMPLE: usize = 20;

type Bind = BTreeMap<String, String>;

// ── shared term helpers ──────────────────────────────────────────────────

fn is_var(t: &str) -> bool {
    t.len() > 1 && t.starts_with('?')
}

/// Bind the variables of `atom` to `f`, consistently with `b`.
fn unify(atom: &[String; 3], f: &Fact, b: &Bind) -> Option<Bind> {
    let mut out = b.clone();
    for (t, v) in atom.iter().zip([&f.s, &f.p, &f.o]) {
        if is_var(t) {
            match out.get(t) {
                Some(bound) if bound != v => return None,
                Some(_) => {}
                None => {
                    out.insert(t.clone(), v.clone());
                }
            }
        } else if t != v {
            return None;
        }
    }
    Some(out)
}

fn ground_term(t: &str, b: &Bind) -> String {
    if is_var(t) { b.get(t).cloned().unwrap_or_else(|| t.to_string()) } else { t.to_string() }
}

fn ground(atom: &[String; 3], b: &Bind) -> Fact {
    Fact::new(ground_term(&atom[0], b), ground_term(&atom[1], b), ground_term(&atom[2], b))
}

/// The id inside an entity term `<id>`.
fn entity_of(term: &str) -> Option<&str> {
    term.strip_prefix('<').and_then(|t| t.strip_suffix('>'))
}

fn entity_terms(f: &Fact) -> impl Iterator<Item = &str> {
    [f.s.as_str(), f.o.as_str()].into_iter().filter_map(entity_of)
}

fn is_store_value(f: &Fact) -> bool {
    f.p == "state" || f.p.starts_with("prop.")
}

/// Is `id` an entity this closure reads (visible and in scope)?
fn visible_in(closure: &Closure, g: &Graph, id: &str) -> Option<String> {
    let e = g.get(id)?;
    let term = reason::spell_entity(id).ok()?;
    let kind = reason::spell_class(&e.kind).ok()?;
    (closure.contains(&Fact::new(term.clone(), "a", kind)) == Some(Status::Asserted)).then_some(term)
}

/// A `state` or `prop.<name>` fact read from the store, for an entity the
/// closure reads. The closure holds such facts only when a rule reads them;
/// the store always does, and rules never derive them.
fn store_holds(closure: &Closure, g: &Graph, f: &Fact) -> bool {
    let Some(id) = entity_of(&f.s) else { return false };
    if visible_in(closure, g, id).is_none() {
        return false;
    }
    stored_value(g, f).is_some_and(|v| same_value(&v, &f.o))
}

/// The stored `state` or `prop.<name>` value of the entity a store-value
/// fact is about.
fn stored_value(g: &Graph, f: &Fact) -> Option<Value> {
    let e = g.get(entity_of(&f.s)?)?;
    if f.p == "state" {
        e.state.as_ref().map(|s| Value::String(s.clone()))
    } else {
        f.p.strip_prefix("prop.").and_then(|name| e.props.get(name)).filter(|v| !v.is_null()).cloned()
    }
}

/// Is the spelled literal `literal` the stored value `v`? Numbers compare by
/// value (`3` is `3.0`), everything else by its spelling.
fn same_value(v: &Value, literal: &str) -> bool {
    if serde_json::to_string(v).is_ok_and(|t| t == literal) {
        return true;
    }
    match (v.as_f64(), serde_json::from_str::<Value>(literal).ok().as_ref().and_then(Value::as_f64)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Whether a fact is asserted, derived or neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Entailment {
    Asserted,
    Derived,
    NotEntailed,
}

/// The status of `f` in `closure`, reading `state`/`prop.*` values from the
/// store when the closure does not hold them.
fn held(closure: &Closure, g: &Graph, f: &Fact) -> Entailment {
    match closure.contains(f) {
        Some(Status::Asserted) => Entailment::Asserted,
        Some(Status::Derived { .. }) => Entailment::Derived,
        None if is_store_value(f) && store_holds(closure, g, f) => Entailment::Asserted,
        None => Entailment::NotEntailed,
    }
}

/// The recorded derivation of `f` unfolded to asserted leaves: the lines in
/// certificate order (premises before conclusions), the leaves, the rules.
fn unfold(closure: &Closure, f: &Fact) -> (Vec<Derivation>, Vec<Fact>, Vec<String>) {
    let mut lines = BTreeSet::new();
    let mut leaves = BTreeSet::new();
    let mut seen = HashSet::new();
    let mut stack = vec![f.clone()];
    while let Some(x) = stack.pop() {
        if !seen.insert(x.clone()) {
            continue;
        }
        match closure.contains(&x) {
            Some(Status::Derived { line }) => {
                lines.insert(line);
                if let Some(d) = closure.derived().get(line - 1) {
                    stack.extend(d.premises.iter().cloned());
                }
            }
            _ => {
                leaves.insert(x);
            }
        }
    }
    let lines: Vec<Derivation> = lines.into_iter().filter_map(|l| closure.derived().get(l - 1).cloned()).collect();
    let rules: BTreeSet<String> = lines.iter().map(|d| d.rule.clone()).collect();
    (lines, leaves.into_iter().collect(), rules.into_iter().collect())
}

// ── independent evaluator ────────────────────────────────────────────────

/// A slot not bound yet.
const UNBOUND: u32 = u32::MAX;

type Triple = [u32; 3];

/// A multiplicative hasher for the evaluator's interned keys: they are small
/// integers the evaluator made, never attacker-chosen strings.
#[derive(Default, Clone, Copy)]
struct Fx(u64);

impl Fx {
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl std::hash::Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for c in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            self.add(u64::from_le_bytes(w));
        }
    }
    fn write_u32(&mut self, n: u32) {
        self.add(u64::from(n));
    }
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }
}

type FxBuild = std::hash::BuildHasherDefault<Fx>;

#[derive(Clone, Copy)]
enum Term {
    Var(usize),
    Const(u32),
}

/// A rule line with its terms interned and its variables numbered.
struct Compiled {
    head: [Term; 3],
    body: Vec<[Term; 3]>,
    vars: usize,
}

/// Interned facts with lookups by predicate and by predicate plus subject or
/// object, each list in insertion order (so ascending, and a range of
/// insertion indices is a binary search away).
#[derive(Default)]
struct Store {
    facts: Vec<Triple>,
    set: HashSet<Triple, FxBuild>,
    by_p: HashMap<u32, Vec<u32>, FxBuild>,
    by_ps: HashMap<(u32, u32), Vec<u32>, FxBuild>,
    by_po: HashMap<(u32, u32), Vec<u32>, FxBuild>,
}

/// The indices of a lookup list that fall in a range of insertion indices.
enum Cands<'a> {
    List(std::slice::Iter<'a, u32>),
    Range(std::ops::Range<u32>),
}

impl Iterator for Cands<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        match self {
            Cands::List(i) => i.next().copied(),
            Cands::Range(r) => r.next(),
        }
    }
}

impl Store {
    fn insert(&mut self, t: Triple) -> bool {
        if !self.set.insert(t) {
            return false;
        }
        let i = self.facts.len() as u32;
        self.by_p.entry(t[1]).or_default().push(i);
        self.by_ps.entry((t[1], t[0])).or_default().push(i);
        self.by_po.entry((t[1], t[2])).or_default().push(i);
        self.facts.push(t);
        true
    }

    /// Facts inserted in `range` that may match `pat` (`UNBOUND` where the
    /// pattern is open).
    fn candidates(&self, pat: Triple, range: (u32, u32)) -> Cands<'_> {
        fn clip(l: Option<&Vec<u32>>, range: (u32, u32)) -> Cands<'_> {
            let l = l.map(Vec::as_slice).unwrap_or_default();
            let a = l.partition_point(|i| *i < range.0);
            let b = l.partition_point(|i| *i < range.1).max(a);
            Cands::List(l[a..b].iter())
        }
        match (pat[0] != UNBOUND, pat[1] != UNBOUND, pat[2] != UNBOUND) {
            (true, true, _) => clip(self.by_ps.get(&(pat[1], pat[0])), range),
            (_, true, true) => clip(self.by_po.get(&(pat[1], pat[2])), range),
            (_, true, _) => clip(self.by_p.get(&pat[1]), range),
            _ => Cands::Range(range.0..range.1.max(range.0)),
        }
    }
}

/// How a join ended early.
enum Halt {
    Goal,
    Budget,
}

/// One semi-naive join of a rule body: atom `order[k]` ranges over
/// `ranges[k]`.
struct Join<'a> {
    st: &'a Store,
    rule: &'a Compiled,
    order: Vec<usize>,
    ranges: Vec<(u32, u32)>,
    slots: Vec<u32>,
    goal: Triple,
    budget: &'a mut usize,
    out: &'a mut Vec<Triple>,
}

impl Join<'_> {
    fn value(&self, t: Term) -> u32 {
        match t {
            Term::Const(c) => c,
            Term::Var(v) => self.slots[v],
        }
    }

    fn go(&mut self, k: usize) -> std::result::Result<(), Halt> {
        let Some(&ai) = self.order.get(k) else {
            let h = self.rule.head;
            let t = [self.value(h[0]), self.value(h[1]), self.value(h[2])];
            if t.contains(&UNBOUND) {
                return Ok(());
            }
            if t == self.goal {
                return Err(Halt::Goal);
            }
            if !self.st.set.contains(&t) {
                self.out.push(t);
            }
            return Ok(());
        };
        let atom = self.rule.body[ai];
        let pat = [self.value(atom[0]), self.value(atom[1]), self.value(atom[2])];
        let st = self.st;
        for i in st.candidates(pat, self.ranges[k]) {
            // Every candidate examined is join work taken from the budget.
            if *self.budget == 0 {
                return Err(Halt::Budget);
            }
            *self.budget -= 1;
            let fact = st.facts[i as usize];
            let mut bound = [0usize; 3];
            let mut n = 0;
            let mut ok = true;
            for (term, v) in atom.iter().zip(fact) {
                match *term {
                    Term::Const(c) if c != v => ok = false,
                    Term::Const(_) => {}
                    Term::Var(s) if self.slots[s] == UNBOUND => {
                        self.slots[s] = v;
                        bound[n] = s;
                        n += 1;
                    }
                    Term::Var(s) if self.slots[s] != v => ok = false,
                    Term::Var(_) => {}
                }
                if !ok {
                    break;
                }
            }
            let res = if ok { self.go(k + 1) } else { Ok(()) };
            for s in &bound[..n] {
                self.slots[*s] = UNBOUND;
            }
            res?;
        }
        Ok(())
    }
}

/// A semi-naive evaluator over interned spelled facts, independent of the
/// reasoner's own evaluation, so a justification is checked by a second
/// implementation.
struct Evaluator {
    ids: HashMap<String, u32>,
    rules: Vec<Compiled>,
}

impl Evaluator {
    fn new(rules: &[RuleLine]) -> Self {
        let mut ev = Evaluator { ids: HashMap::new(), rules: Vec::new() };
        for r in rules {
            let mut vars: HashMap<String, usize> = HashMap::new();
            let mut term = |t: &str, ev: &mut Evaluator| -> Term {
                if is_var(t) {
                    let n = vars.len();
                    Term::Var(*vars.entry(t.to_string()).or_insert(n))
                } else {
                    Term::Const(ev.id(t))
                }
            };
            let mut atom = |a: &[String; 3], ev: &mut Evaluator| [term(&a[0], ev), term(&a[1], ev), term(&a[2], ev)];
            let body: Vec<[Term; 3]> = r.body.iter().map(|a| atom(a, &mut ev)).collect();
            let head = atom(&r.head, &mut ev);
            ev.rules.push(Compiled { head, body, vars: vars.len() });
        }
        ev
    }

    fn id(&mut self, t: &str) -> u32 {
        if let Some(&i) = self.ids.get(t) {
            return i;
        }
        let i = self.ids.len() as u32;
        self.ids.insert(t.to_string(), i);
        i
    }

    fn triple(&mut self, f: &Fact) -> Triple {
        [self.id(&f.s), self.id(&f.p), self.id(&f.o)]
    }

    /// Does the rule table derive `goal` from exactly `base`? Every
    /// candidate fact a join examines is taken from `budget`; `None` when the
    /// budget runs out first.
    fn entails(&mut self, base: &BTreeSet<Fact>, goal: &Fact, budget: &mut usize) -> Option<bool> {
        let mut st = Store::default();
        for f in base {
            let t = self.triple(f);
            st.insert(t);
        }
        let goal = self.triple(goal);
        if st.set.contains(&goal) {
            return Some(true);
        }
        let mut delta = (0u32, st.facts.len() as u32);
        while delta.0 < delta.1 {
            let mut fresh: Vec<Triple> = Vec::new();
            for rule in &self.rules {
                // Atom i ranges over the delta, the atoms before it over the
                // facts older than the delta and those after it over all:
                // each new combination is joined once.
                for i in 0..rule.body.len() {
                    let order: Vec<usize> =
                        std::iter::once(i).chain((0..rule.body.len()).filter(|j| *j != i)).collect();
                    let ranges = order
                        .iter()
                        .map(|j| match j.cmp(&i) {
                            Ordering::Equal => delta,
                            Ordering::Less => (0, delta.0),
                            Ordering::Greater => (0, delta.1),
                        })
                        .collect();
                    let mut join = Join {
                        st: &st,
                        rule,
                        order,
                        ranges,
                        slots: vec![UNBOUND; rule.vars],
                        goal,
                        budget: &mut *budget,
                        out: &mut fresh,
                    };
                    match join.go(0) {
                        Ok(()) => {}
                        Err(Halt::Goal) => return Some(true),
                        Err(Halt::Budget) => return None,
                    }
                }
            }
            let end = st.facts.len() as u32;
            for t in fresh {
                st.insert(t);
            }
            delta = (end, st.facts.len() as u32);
        }
        Some(false)
    }
}

/// [`Evaluator::entails`] under a fresh evaluator.
#[cfg(test)]
fn entails(rules: &[RuleLine], base: &BTreeSet<Fact>, goal: &Fact, budget: &mut usize) -> Option<bool> {
    Evaluator::new(rules).entails(base, goal, budget)
}

// ── the support cone of one fact ─────────────────────────────────────────

/// One rule instance in the support of a fact.
struct Inst {
    rule: usize,
    conclusion: usize,
    premises: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sup {
    /// In the base set (asserted).
    Base,
    /// First derived by this instance.
    By(usize),
}

/// Every fact the goal can rest on and every rule instance among them,
/// found by unifying each rule head with a fact and matching the body
/// against the closure.
struct Cone {
    facts: Vec<Fact>,
    asserted: Vec<bool>,
    insts: Vec<Inst>,
    /// Instances using a fact as a premise (one entry per occurrence).
    uses: Vec<Vec<usize>>,
    /// Instances concluding a fact.
    concluding: Vec<Vec<usize>>,
    /// False when a budget or a cut query left instances out.
    complete: bool,
    stopped: Option<String>,
    /// Some instance concludes a fact from premises that include the fact
    /// itself (`a next a, a next b ⊢ a next b`): left out of the instances
    /// (a fact never supports itself), yet a cycle that makes the number of
    /// derivation trees infinite.
    self_support: bool,
    /// Propagation work spent so far (facts and premise uses visited).
    work: std::cell::Cell<usize>,
}

impl Cone {
    fn build(closure: &Closure, goal: &Fact, max_instances: usize) -> Cone {
        let mut cone = Cone {
            facts: Vec::new(),
            asserted: Vec::new(),
            insts: Vec::new(),
            uses: Vec::new(),
            concluding: Vec::new(),
            complete: true,
            stopped: None,
            self_support: false,
            work: std::cell::Cell::new(0),
        };
        let mut ids: HashMap<Fact, usize> = HashMap::new();
        let mut node = |cone: &mut Cone, f: Fact| -> usize {
            if let Some(&i) = ids.get(&f) {
                return i;
            }
            let i = cone.facts.len();
            cone.asserted.push(closure.contains(&f) == Some(Status::Asserted));
            cone.facts.push(f.clone());
            cone.uses.push(Vec::new());
            cone.concluding.push(Vec::new());
            ids.insert(f, i);
            i
        };
        if closure.contains(goal).is_none() {
            return cone;
        }
        node(&mut cone, goal.clone());
        let base_cut = closure.stats().truncated;
        let rules = closure.rules();
        let mut seen: HashSet<(usize, Vec<usize>)> = HashSet::new();
        let mut next = 0;
        'facts: while next < cone.facts.len() {
            let fi = next;
            next += 1;
            let f = cone.facts[fi].clone();
            for (ri, line) in rules.iter().enumerate() {
                let Some(b) = unify(&line.head, &f, &Bind::new()) else { continue };
                let atoms: Vec<AtomPattern> = line
                    .body
                    .iter()
                    .map(|a| {
                        let g = ground(a, &b);
                        AtomPattern { s: g.s, p: g.p, o: g.o }
                    })
                    .collect();
                let room = max_instances.saturating_sub(cone.insts.len());
                let answer = match closure.query_answer(&atoms, room.saturating_add(1)) {
                    Ok(a) => a,
                    Err(e) => {
                        cone.stop(format!("premises of {f} under {} could not be matched: {e}", line.name));
                        continue;
                    }
                };
                if answer.truncated != base_cut {
                    cone.stop(format!("matching the premises of {f} under {} was cut", line.name));
                }
                if answer.bindings.len() > room {
                    cone.stop(format!("the support passed max_instances = {max_instances}"));
                }
                for binding in answer.bindings.iter().take(room) {
                    let mut full = b.clone();
                    for (k, v) in binding {
                        full.insert(format!("?{k}"), v.clone());
                    }
                    let premises: Vec<Fact> = line.body.iter().map(|a| ground(a, &full)).collect();
                    // A fact never supports itself, but such an instance is a
                    // cycle in the support (every premise is in the closure).
                    if premises.contains(&f) {
                        cone.self_support = true;
                        continue;
                    }
                    let pidx: Vec<usize> = premises.into_iter().map(|p| node(&mut cone, p)).collect();
                    if seen.insert((ri, pidx.clone())) {
                        let ii = cone.insts.len();
                        for &p in &pidx {
                            cone.uses[p].push(ii);
                        }
                        cone.concluding[fi].push(ii);
                        cone.insts.push(Inst { rule: ri, conclusion: fi, premises: pidx });
                    }
                }
                if cone.insts.len() >= max_instances && !cone.complete {
                    break 'facts;
                }
            }
        }
        cone
    }

    fn stop(&mut self, why: String) {
        self.complete = false;
        self.stopped.get_or_insert(why);
    }

    const GOAL: usize = 0;

    fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    /// Forward propagation from the asserted facts `base` admits: which facts
    /// become derivable and by which instance first (FIFO order, so a
    /// derivable fact's first instance only uses facts derivable before it).
    /// Stops early once `stop_at` is derivable.
    fn propagate(&self, base: &dyn Fn(usize) -> bool, stop_at: Option<usize>) -> Vec<Option<Sup>> {
        let mut sup = vec![None; self.facts.len()];
        let mut missing: Vec<usize> = self.insts.iter().map(|i| i.premises.len()).collect();
        let mut queue = VecDeque::new();
        for (v, s) in sup.iter_mut().enumerate() {
            if self.asserted[v] && base(v) {
                *s = Some(Sup::Base);
                queue.push_back(v);
            }
        }
        let mut work = 0;
        while let Some(v) = queue.pop_front() {
            if stop_at == Some(v) {
                break;
            }
            work += 1 + self.uses[v].len();
            for &ii in &self.uses[v] {
                missing[ii] -= 1;
                if missing[ii] == 0 {
                    let c = self.insts[ii].conclusion;
                    if sup[c].is_none() {
                        sup[c] = Some(Sup::By(ii));
                        queue.push_back(c);
                    }
                }
            }
        }
        self.work.set(self.work.get().saturating_add(work));
        sup
    }

    /// Asserted leaves and instances (premises first) of the first-found
    /// derivation of `goal` in `sup`.
    fn witness(&self, sup: &[Option<Sup>], goal: usize) -> (BTreeSet<usize>, Vec<usize>) {
        let mut leaves = BTreeSet::new();
        let mut order = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![(goal, false)];
        while let Some((v, expanded)) = stack.pop() {
            if expanded {
                if let Some(Sup::By(ii)) = sup[v] {
                    order.push(ii);
                }
                continue;
            }
            if !seen.insert(v) {
                continue;
            }
            match sup[v] {
                Some(Sup::Base) => {
                    leaves.insert(v);
                }
                Some(Sup::By(ii)) => {
                    stack.push((v, true));
                    for &p in self.insts[ii].premises.iter().rev() {
                        if !seen.contains(&p) {
                            stack.push((p, false));
                        }
                    }
                }
                None => {}
            }
        }
        (leaves, order)
    }

    fn derivable_from(&self, set: &BTreeSet<usize>, goal: usize) -> bool {
        self.propagate(&|v| set.contains(&v), Some(goal))[goal].is_some()
    }

    /// Deletion-based minimization: drop each fact (in fact order) that the
    /// goal does not need given the others. Stops, with `true`, once the
    /// cone's propagation work reaches `limit`: the set is then sufficient
    /// but perhaps not minimal.
    fn minimize(
        &self,
        mut set: BTreeSet<usize>,
        goal: usize,
        steps: &mut usize,
        limit: usize,
    ) -> (BTreeSet<usize>, bool) {
        let mut order: Vec<usize> = set.iter().copied().collect();
        order.sort_by(|a, b| self.facts[*a].cmp(&self.facts[*b]));
        for v in order {
            if self.work.get() >= limit {
                return (set, true);
            }
            set.remove(&v);
            *steps += 1;
            if !self.derivable_from(&set, goal) {
                set.insert(v);
            }
        }
        (set, false)
    }

    /// Facts the goal's support reaches (through concluding instances).
    fn reach(&self, goal: usize) -> Vec<bool> {
        let mut reach = vec![false; self.facts.len()];
        let mut stack = vec![goal];
        while let Some(v) = stack.pop() {
            if std::mem::replace(&mut reach[v], true) {
                continue;
            }
            for &ii in &self.concluding[v] {
                stack.extend(self.insts[ii].premises.iter().copied().filter(|p| !reach[*p]));
            }
        }
        reach
    }

    /// The reached facts in an order where every fact comes after the
    /// premises of its admitted instances, or `None` when they form a cycle.
    fn topo(&self, reach: &[bool], admit: &dyn Fn(usize) -> bool) -> Option<Vec<usize>> {
        let mut indeg = vec![0usize; self.facts.len()];
        for (v, _) in reach.iter().enumerate().filter(|(_, r)| **r) {
            indeg[v] =
                self.concluding[v].iter().filter(|ii| admit(**ii)).map(|ii| self.insts[*ii].premises.len()).sum();
        }
        let mut ready: VecDeque<usize> = (0..self.facts.len()).filter(|v| reach[*v] && indeg[*v] == 0).collect();
        let mut order = Vec::new();
        while let Some(v) = ready.pop_front() {
            order.push(v);
            for &ii in &self.uses[v] {
                let c = self.insts[ii].conclusion;
                if reach[c] && admit(ii) {
                    indeg[c] -= 1;
                    if indeg[c] == 0 {
                        ready.push_back(c);
                    }
                }
            }
        }
        (order.len() == reach.iter().filter(|r| **r).count()).then_some(order)
    }

    /// The round at which each fact first becomes derivable from all the
    /// asserted facts (asserted: 0).
    fn levels(&self) -> Vec<Option<usize>> {
        let sup = self.propagate(&|_| true, None);
        let mut level = vec![None; self.facts.len()];
        // Each fact's first instance only uses facts derivable before it, so
        // repeated passes over those instances assign every level.
        let mut fired: Vec<(usize, usize)> = Vec::new();
        for (v, s) in sup.iter().enumerate() {
            match s {
                Some(Sup::Base) => level[v] = Some(0),
                Some(Sup::By(ii)) => fired.push((*ii, v)),
                None => {}
            }
        }
        let mut changed = true;
        while changed {
            changed = false;
            for &(ii, v) in &fired {
                if level[v].is_some() {
                    continue;
                }
                let ps: Option<Vec<usize>> = self.insts[ii].premises.iter().map(|p| level[*p]).collect();
                if let Some(ps) = ps {
                    level[v] = Some(1 + ps.into_iter().max().unwrap_or(0));
                    changed = true;
                }
            }
        }
        level
    }
}

// ── justifications ───────────────────────────────────────────────────────

/// Bounds of a justification search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JustifyOptions {
    /// Stop after this many justifications.
    pub max_justifications: usize,
    /// Stop after this many oracle calls and hitting-set nodes (it never
    /// cuts the first justification's minimization; `max_work` does).
    pub max_steps: usize,
    /// Rule instances collected for the support of the fact.
    pub max_instances: usize,
    /// Propagation work (facts and premise uses visited) the search may
    /// spend; a first justification whose minimization it cuts is reported
    /// as found (sufficient, minimality left to the evaluator).
    pub max_work: usize,
    /// Join steps (candidate facts examined) the independent evaluator may
    /// spend verifying sufficiency and minimality.
    pub verify_budget: usize,
}

impl Default for JustifyOptions {
    fn default() -> Self {
        Self {
            max_justifications: 10,
            max_steps: 10_000,
            max_instances: 100_000,
            max_work: SEARCH_WORK,
            verify_budget: VERIFY_BUDGET,
        }
    }
}

/// One minimal set of asserted facts the fact can be derived from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Justification {
    /// Asserted facts, sorted (schema facts included).
    pub facts: Vec<Fact>,
    /// Supplied rules (`schema/...`) the derivation from these facts uses.
    pub rules: Vec<String>,
    /// Built-in rules it uses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builtin_rules: Vec<String>,
    /// One derivation from exactly these facts, premises before conclusions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proof: Vec<Derivation>,
    /// The independent evaluator re-derives the fact from exactly these facts.
    pub sufficient: bool,
    /// Removing any one fact makes the evaluator fail to derive it.
    pub minimal: bool,
}

/// The justifications of one fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Justifications {
    pub fact: Fact,
    pub status: Entailment,
    pub justifications: Vec<Justification>,
    /// Every minimal justification is listed.
    pub complete: bool,
    pub fixpoint_reached: bool,
    /// What stopped the search short (a bound, a partial closure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
    /// Oracle calls and hitting-set nodes used.
    pub steps: usize,
    /// Set when some `sufficient`/`minimal` could not be checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
    pub means: String,
}

impl Justifications {
    /// A store value (`state`, `prop.*`) the closure does not hold: its own
    /// and only justification, since rules never derive property values.
    fn store_value(fact: Fact, fixpoint_reached: bool) -> Self {
        Justifications {
            justifications: vec![Justification {
                facts: vec![fact.clone()],
                rules: Vec::new(),
                builtin_rules: Vec::new(),
                proof: Vec::new(),
                sufficient: true,
                minimal: true,
            }],
            fact,
            status: Entailment::Asserted,
            complete: true,
            fixpoint_reached,
            stopped: None,
            steps: 0,
            verification: None,
            means: JUSTIFY_MEANS.into(),
        }
    }
}

/// Minimal sets of asserted facts from which `closure`'s rule table derives
/// `fact`, each verified by an independent re-derivation.
pub fn justify(closure: &Closure, fact: &Fact, opts: &JustifyOptions) -> Justifications {
    let (justs, cone, status, steps, stopped) = search(closure, fact, opts);
    let rules = closure.rules();
    let mut budget = opts.verify_budget;
    let mut eval = Evaluator::new(rules);
    let sets: Vec<BTreeSet<Fact>> =
        justs.iter().map(|set| set.iter().map(|v| cone.facts[*v].clone()).collect()).collect();
    // Sufficiency of every justification first (cheap: the evaluator stops
    // at the goal), then minimality, which needs a full fixpoint per fact.
    let sufficient: Vec<Option<bool>> = sets.iter().map(|facts| eval.entails(facts, fact, &mut budget)).collect();
    let minimal: Vec<Option<bool>> = sets
        .iter()
        .zip(&sufficient)
        .map(|(facts, ok)| {
            if *ok != Some(true) {
                return Some(false);
            }
            for f in facts {
                let mut less = facts.clone();
                less.remove(f);
                match eval.entails(&less, fact, &mut budget)? {
                    false => {}
                    true => return Some(false),
                }
            }
            Some(true)
        })
        .collect();
    let unverified = sufficient.iter().chain(&minimal).any(Option::is_none);
    let mut out: Vec<Justification> = justs
        .iter()
        .zip(sets)
        .zip(sufficient.iter().zip(&minimal))
        .map(|((set, facts), (sufficient, minimal))| {
            let sufficient = *sufficient == Some(true);
            let minimal = sufficient && *minimal == Some(true);
            let sup = cone.propagate(&|v| set.contains(&v), Some(Cone::GOAL));
            let (_, order) = cone.witness(&sup, Cone::GOAL);
            let mut supplied = BTreeSet::new();
            let mut builtin = BTreeSet::new();
            let proof: Vec<Derivation> = order
                .iter()
                .map(|ii| {
                    let inst = &cone.insts[*ii];
                    let name = rules[inst.rule].name.clone();
                    if name.starts_with(SUPPLIED_PREFIX) {
                        supplied.insert(name.clone());
                    } else {
                        builtin.insert(name.clone());
                    }
                    Derivation {
                        rule: name,
                        conclusion: cone.facts[inst.conclusion].clone(),
                        premises: inst.premises.iter().map(|p| cone.facts[*p].clone()).collect(),
                    }
                })
                .collect();
            Justification {
                facts: facts.into_iter().collect(),
                rules: supplied.into_iter().collect(),
                builtin_rules: builtin.into_iter().collect(),
                proof,
                sufficient,
                minimal,
            }
        })
        .collect();
    out.sort_by(|a, b| (a.facts.len(), &a.facts).cmp(&(b.facts.len(), &b.facts)));
    let fixpoint_reached = closure.fixpoint_reached();
    let stopped = stopped.or_else(|| (!fixpoint_reached).then(|| "the closure is a lower bound".to_string()));
    let verification = unverified.then(|| {
        format!(
            "the independent evaluator's budget (verify_budget = {} join steps) ran out: a false sufficient or \
             minimal means not established, not refuted",
            opts.verify_budget
        )
    });
    Justifications {
        verification,
        fact: fact.clone(),
        status,
        justifications: out,
        complete: stopped.is_none(),
        fixpoint_reached,
        stopped,
        steps,
        means: JUSTIFY_MEANS.into(),
    }
}

type Search = (Vec<BTreeSet<usize>>, Cone, Entailment, usize, Option<String>);

/// Reiter's hitting-set tree over the support cone, with deletion-based
/// minimization for each new justification.
fn search(closure: &Closure, fact: &Fact, opts: &JustifyOptions) -> Search {
    let status = match closure.contains(fact) {
        Some(Status::Asserted) => Entailment::Asserted,
        Some(Status::Derived { .. }) => Entailment::Derived,
        None => Entailment::NotEntailed,
    };
    let cone = Cone::build(closure, fact, opts.max_instances);
    let mut stopped = cone.stopped.clone();
    let mut steps = 0;
    let mut justs: Vec<BTreeSet<usize>> = Vec::new();
    if cone.is_empty() || opts.max_justifications == 0 {
        if !cone.is_empty() {
            stopped.get_or_insert_with(|| "max_justifications = 0".into());
        }
        return (justs, cone, status, steps, stopped);
    }
    let goal = Cone::GOAL;
    let out_of_work = || format!("stopped after max_work = {} propagation steps", opts.max_work);
    let first = |removed: &BTreeSet<usize>, steps: &mut usize| -> Option<(BTreeSet<usize>, bool)> {
        *steps += 1;
        let sup = cone.propagate(&|v| !removed.contains(&v), Some(goal));
        sup[goal]?;
        let (leaves, _) = cone.witness(&sup, goal);
        Some(cone.minimize(leaves, goal, steps, opts.max_work))
    };
    let Some((root, cut)) = first(&BTreeSet::new(), &mut steps) else {
        stopped.get_or_insert_with(|| "the support found does not derive the fact".into());
        return (justs, cone, status, steps, stopped);
    };
    // A root whose minimization was cut is still reported (sufficient; the
    // evaluator checks minimality); the search ends there.
    justs.push(root.clone());
    if cut {
        stopped.get_or_insert_with(out_of_work);
        return (justs, cone, status, steps, stopped);
    }
    let mut queue: VecDeque<BTreeSet<usize>> = VecDeque::new();
    let mut seen: HashSet<BTreeSet<usize>> = HashSet::new();
    let mut closed: Vec<BTreeSet<usize>> = Vec::new();
    let expand = |label: &BTreeSet<usize>, path: &BTreeSet<usize>, queue: &mut VecDeque<_>, seen: &mut HashSet<_>| {
        for v in label {
            let mut child = path.clone();
            child.insert(*v);
            if seen.insert(child.clone()) {
                queue.push_back(child);
            }
        }
    };
    expand(&root, &BTreeSet::new(), &mut queue, &mut seen);
    while let Some(path) = queue.pop_front() {
        // A superset of a path that removed every derivation removes them too.
        if closed.iter().any(|c| c.is_subset(&path)) {
            continue;
        }
        if steps >= opts.max_steps {
            stopped.get_or_insert_with(|| format!("stopped after max_steps = {}", opts.max_steps));
            break;
        }
        if cone.work.get() >= opts.max_work {
            stopped.get_or_insert_with(out_of_work);
            break;
        }
        steps += 1;
        // Reuse a justification the path does not hit.
        let label = match justs.iter().find(|j| j.is_disjoint(&path)) {
            Some(j) => j.clone(),
            None => {
                if justs.len() >= opts.max_justifications {
                    stopped.get_or_insert_with(|| {
                        format!("stopped after max_justifications = {}", opts.max_justifications)
                    });
                    break;
                }
                match first(&path, &mut steps) {
                    // Not minimized to the end: dropped, the search ends.
                    Some((_, true)) => {
                        stopped.get_or_insert_with(out_of_work);
                        break;
                    }
                    Some((j, false)) => {
                        justs.push(j.clone());
                        j
                    }
                    None => {
                        closed.push(path);
                        continue;
                    }
                }
            }
        };
        expand(&label, &path, &mut queue, &mut seen);
    }
    (justs, cone, status, steps, stopped)
}

// ── provenance ───────────────────────────────────────────────────────────

/// The semiring a fact's support is evaluated in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Semiring {
    /// The minimal witness sets.
    #[default]
    Why,
    /// Max over derivations of the min over premises, in [0, 1].
    Trust,
    /// The number of derivation trees.
    Count,
}

/// The value of a provenance evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProvenanceValue {
    Why(Vec<Vec<Fact>>),
    Count(u64),
    Trust(f64),
}

/// A fact's support evaluated in one semiring.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    pub fact: Fact,
    pub semiring: Semiring,
    pub status: Entailment,
    pub value: ProvenanceValue,
    /// The value is the semiring's value over the whole support, not a bound.
    pub exact: bool,
    /// Some fact in the support can be derived from itself.
    pub cycle_in_support: bool,
    pub fixpoint_reached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
    pub means: String,
}

impl Provenance {
    /// A store value (`state`, `prop.*`) the closure does not hold: asserted,
    /// its own single derivation tree.
    fn store_value(fact: Fact, semiring: Semiring, weight: f64, fixpoint_reached: bool) -> Self {
        let (value, means) = match semiring {
            Semiring::Why => (ProvenanceValue::Why(vec![vec![fact.clone()]]), WHY_MEANS),
            Semiring::Trust => (ProvenanceValue::Trust(weight), TRUST_MEANS),
            Semiring::Count => (ProvenanceValue::Count(1), COUNT_MEANS),
        };
        Provenance {
            fact,
            semiring,
            status: Entailment::Asserted,
            value,
            exact: true,
            cycle_in_support: false,
            fixpoint_reached,
            stopped: None,
            means: means.into(),
        }
    }
}

/// Weights of asserted facts for trust provenance: an entity fact weighs the
/// entity's `confidence` property, a link fact the link's own `confidence`,
/// else the lower of its endpoints'. Only weights other than 1.0 are listed;
/// values outside [0, 1] are ignored.
pub fn confidence_weights(g: &Graph, facts: &[Fact]) -> BTreeMap<Fact, f64> {
    let conf = |props: &serde_json::Map<String, Value>| {
        props.get("confidence").and_then(Value::as_f64).filter(|c| (0.0..=1.0).contains(c))
    };
    let entity = |id: &str| g.get(id).and_then(|e| conf(&e.props)).unwrap_or(1.0);
    let mut out = BTreeMap::new();
    for f in facts {
        let Some(s) = entity_of(&f.s) else { continue };
        let w = match entity_of(&f.o) {
            Some(o) => g.get_link(s, &f.p, o).and_then(|l| conf(&l.props)).unwrap_or_else(|| entity(s).min(entity(o))),
            None => entity(s),
        };
        if w != 1.0 {
            out.insert(f.clone(), w);
        }
    }
    out
}

/// [`provenance_with`] under the default justification bounds.
pub fn provenance(closure: &Closure, fact: &Fact, semiring: Semiring, weights: &BTreeMap<Fact, f64>) -> Provenance {
    provenance_with(closure, fact, semiring, weights, &JustifyOptions::default())
}

/// Evaluate `fact`'s support in `semiring`. Asserted facts missing from
/// `weights` weigh 1.0.
pub fn provenance_with(
    closure: &Closure,
    fact: &Fact,
    semiring: Semiring,
    weights: &BTreeMap<Fact, f64>,
    opts: &JustifyOptions,
) -> Provenance {
    let fixpoint_reached = closure.fixpoint_reached();
    if semiring == Semiring::Why {
        let j = justify(closure, fact, opts);
        let cone = Cone::build(closure, fact, opts.max_instances);
        let cycle = !cone.is_empty() && (cone.self_support || cone.topo(&cone.reach(Cone::GOAL), &|_| true).is_none());
        return Provenance {
            fact: fact.clone(),
            semiring,
            status: j.status,
            value: ProvenanceValue::Why(j.justifications.into_iter().map(|j| j.facts).collect()),
            exact: j.complete,
            cycle_in_support: cycle,
            fixpoint_reached,
            stopped: j.stopped,
            means: WHY_MEANS.into(),
        };
    }
    let status = match closure.contains(fact) {
        Some(Status::Asserted) => Entailment::Asserted,
        Some(Status::Derived { .. }) => Entailment::Derived,
        None => Entailment::NotEntailed,
    };
    let cone = Cone::build(closure, fact, opts.max_instances);
    let mut stopped = cone.stopped.clone();
    if !fixpoint_reached {
        stopped.get_or_insert_with(|| "the closure is a lower bound".into());
    }
    let bounded = cone.complete && fixpoint_reached;
    let (cycle, value, exact) = if cone.is_empty() {
        let v = if semiring == Semiring::Count { ProvenanceValue::Count(0) } else { ProvenanceValue::Trust(0.0) };
        (false, v, fixpoint_reached)
    } else {
        let reach = cone.reach(Cone::GOAL);
        let order = cone.topo(&reach, &|_| true);
        // The instances left out because they rest on their own conclusion
        // keep the count below the number of trees: a cycle too.
        let cycle = order.is_none() || cone.self_support;
        match semiring {
            Semiring::Trust => (cycle, ProvenanceValue::Trust(trust(&cone, weights)), bounded),
            _ => {
                let (count, saturated) = match order {
                    Some(order) => count_trees(&cone, &order, &|_| true),
                    None => {
                        // Only instances whose premises all become derivable
                        // strictly earlier: acyclic, so a finite lower bound.
                        let level = cone.levels();
                        let admit = |ii: usize| {
                            let inst = &cone.insts[ii];
                            match level[inst.conclusion] {
                                Some(l) => inst.premises.iter().all(|p| level[*p].is_some_and(|pl| pl < l)),
                                None => false,
                            }
                        };
                        let order = cone.topo(&reach, &admit).unwrap_or_default();
                        count_trees(&cone, &order, &admit)
                    }
                };
                if saturated {
                    stopped.get_or_insert_with(|| "the count passed u64::MAX".into());
                }
                (cycle, ProvenanceValue::Count(count), bounded && !cycle && !saturated)
            }
        }
    };
    Provenance {
        fact: fact.clone(),
        semiring,
        status,
        value,
        exact,
        cycle_in_support: cycle,
        fixpoint_reached,
        stopped,
        means: if semiring == Semiring::Trust { TRUST_MEANS } else { COUNT_MEANS }.into(),
    }
}

/// Trees per fact in topological order: one for an asserted fact plus, per
/// admitted instance, the product of its premises' counts.
fn count_trees(cone: &Cone, order: &[usize], admit: &dyn Fn(usize) -> bool) -> (u64, bool) {
    let mut val = vec![0u64; cone.facts.len()];
    let mut saturated = false;
    for &v in order {
        let mut n: u64 = u64::from(cone.asserted[v]);
        for &ii in cone.concluding[v].iter().filter(|ii| admit(**ii)) {
            let mut prod: u64 = 1;
            for &p in &cone.insts[ii].premises {
                prod = prod.checked_mul(val[p]).unwrap_or_else(|| {
                    saturated = true;
                    u64::MAX
                });
            }
            n = n.checked_add(prod).unwrap_or_else(|| {
                saturated = true;
                u64::MAX
            });
        }
        val[v] = n;
    }
    (val[Cone::GOAL], saturated)
}

/// Max-min widest derivation (Knuth's generalization of Dijkstra: `min` never
/// exceeds its arguments, so a fact is final when it leaves the heap).
fn trust(cone: &Cone, weights: &BTreeMap<Fact, f64>) -> f64 {
    #[derive(PartialEq)]
    struct Key(f64, usize);
    impl Eq for Key {}
    impl PartialOrd for Key {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Key {
        fn cmp(&self, other: &Self) -> Ordering {
            self.0.total_cmp(&other.0).then_with(|| other.1.cmp(&self.1))
        }
    }
    let n = cone.facts.len();
    let mut best = vec![f64::NEG_INFINITY; n];
    let mut done = vec![false; n];
    let mut missing: Vec<usize> = cone.insts.iter().map(|i| i.premises.len()).collect();
    let mut heap = BinaryHeap::new();
    for v in (0..n).filter(|v| cone.asserted[*v]) {
        let w = weights.get(&cone.facts[v]).copied().filter(|w| (0.0..=1.0).contains(w)).unwrap_or(1.0);
        best[v] = w;
        heap.push(Key(w, v));
    }
    while let Some(Key(w, v)) = heap.pop() {
        if done[v] || w < best[v] {
            continue;
        }
        done[v] = true;
        if v == Cone::GOAL {
            break;
        }
        for &ii in &cone.uses[v] {
            missing[ii] -= 1;
            if missing[ii] == 0 {
                let inst = &cone.insts[ii];
                let m = inst.premises.iter().map(|p| best[*p]).fold(f64::INFINITY, f64::min);
                if !done[inst.conclusion] && m > best[inst.conclusion] {
                    best[inst.conclusion] = m;
                    heap.push(Key(m, inst.conclusion));
                }
            }
        }
    }
    if best[Cone::GOAL].is_finite() { best[Cone::GOAL] } else { 0.0 }
}

// ── claims ───────────────────────────────────────────────────────────────

/// A statement in schema vocabulary: `{subject, relation, object}` with
/// entity ids, relation names (canonical, inverse or `^rel`, or a relation
/// a rule derives), `a` with a kind, interface or derived class, `state`, or
/// `prop.<name>` with a JSON value. Schema statements work too:
/// `{"subject": "incident", "relation": "extends", "object": "issue"}` and
/// `{"subject": "depends_on", "relation": "a", "object": "transitive"}`.
/// With `rule` and/or `premises` it is a reasoning step to check.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub subject: String,
    pub relation: String,
    pub object: Value,
    /// The rule the step claims to apply (`responsible_team`,
    /// `schema/responsible_team` or `builtin/transitive`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// The premises the step claims (their own `rule`/`premises` are ignored).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub premises: Vec<Claim>,
}

impl Claim {
    pub fn new(subject: impl Into<String>, relation: impl Into<String>, object: impl Into<Value>) -> Self {
        Self { subject: subject.into(), relation: relation.into(), object: object.into(), rule: None, premises: vec![] }
    }

    /// The same claim as a reasoning step.
    pub fn step(mut self, rule: Option<&str>, premises: Vec<Claim>) -> Self {
        self.rule = rule.map(str::to_string);
        self.premises = premises;
        self
    }
}

/// How a claim was read.
enum Form {
    /// A link type (with its cardinality) or a derived relation.
    Relation {
        link: Option<(Cardinality, bool)>,
    },
    /// `a <class>`; `object_type` when the class is a kind.
    Class {
        object_type: bool,
    },
    StoreValue,
    Schema,
}

struct Spelled {
    fact: Fact,
    form: Form,
}

fn strip_entity(t: &str) -> &str {
    entity_of(t).unwrap_or(t)
}

fn strip_class(t: &str) -> &str {
    t.strip_prefix('#').unwrap_or(t)
}

/// Spell a claim as a fact, or list the terms outside the vocabulary. With
/// `known`, entities must be ones `closure` reads.
fn spell_claim(
    schema: &Schema,
    vocab: &Vocabulary,
    g: &Graph,
    closure: &Closure,
    claim: &Claim,
    known: bool,
) -> std::result::Result<Spelled, Vec<String>> {
    let mut unknown = Vec::new();
    let rel = claim.relation.trim();
    let subject = strip_entity(claim.subject.trim());
    let object_str = claim.object.as_str().map(str::trim);
    let entity = |id: &str, unknown: &mut Vec<String>| -> Option<String> {
        // A stored entity whose id cannot be spelled is not unknown: the
        // reasoner refused it, and the claim says why.
        if let Err(why) = reason::spell_entity(id) {
            unknown.push(format!("entity {id} ({REFUSED_TERM}: {why})"));
            return None;
        }
        let spelled = if known { visible_in(closure, g, id) } else { reason::spell_entity(id).ok() };
        if spelled.is_none() || !id.contains(':') {
            unknown.push(format!("entity {id}"));
            return None;
        }
        spelled
    };
    // Schema statements.
    if (rel == "extends" || rel == "implements") && !subject.contains(':') {
        let object = object_str.map(strip_class).unwrap_or_default();
        for name in [subject, object] {
            if !schema.is_type_or_interface(name) {
                unknown.push(format!("class {name}"));
            }
        }
        if !unknown.is_empty() {
            return Err(unknown);
        }
        let (s, o) = (reason::spell_class(subject), reason::spell_class(object));
        return match (s, o) {
            (Ok(s), Ok(o)) => Ok(Spelled { fact: Fact::new(s, rel, o), form: Form::Schema }),
            _ => Err(vec![format!("class {subject} or {object} cannot be spelled")]),
        };
    }
    if rel == "a" && matches!(object_str, Some("transitive" | "symmetric")) && !subject.contains(':') {
        if !schema.link_types.contains_key(subject) {
            return Err(vec![format!("link type {subject}")]);
        }
        return match reason::spell_relation(subject) {
            Ok(r) => Ok(Spelled { fact: Fact::new(r, "a", object_str.unwrap_or_default()), form: Form::Schema }),
            Err(e) => Err(vec![format!("link type {subject} ({e})")]),
        };
    }
    let s = entity(subject, &mut unknown);
    if rel == "a" {
        let class = object_str.map(strip_class).unwrap_or_default();
        // A class some rule derives is open even when it is named like a kind.
        let object_type = schema.object_types.contains_key(class) && !vocab.classes.contains(class);
        if !schema.is_type_or_interface(class) && !vocab.classes.contains(class) {
            unknown.push(format!("class {}", if class.is_empty() { claim.object.to_string() } else { class.into() }));
        }
        let o = reason::spell_class(class).ok();
        return match (s, o, unknown.is_empty()) {
            (Some(s), Some(o), true) => Ok(Spelled { fact: Fact::new(s, "a", o), form: Form::Class { object_type } }),
            _ => Err(unknown),
        };
    }
    if rel == "state" || rel.starts_with("prop.") {
        if let Some(name) = rel.strip_prefix("prop.") {
            let declared = schema.object_types.values().any(|t| t.properties.contains_key(name))
                || schema.interfaces.values().any(|i| i.properties.contains_key(name));
            if !declared {
                unknown.push(format!("property {name}"));
            }
        } else if !claim.object.is_string() {
            unknown.push(format!("state {} (a state is a string)", claim.object));
        }
        let literal = serde_json::to_string(&claim.object).unwrap_or_default();
        return match (s, unknown.is_empty()) {
            (Some(s), true) => Ok(Spelled { fact: Fact::new(s, rel, literal), form: Form::StoreValue }),
            _ => Err(unknown),
        };
    }
    let resolved = match schema.resolve_rel(rel) {
        Some((base, dir)) => {
            let lt = &schema.link_types[&base];
            Some((base, dir == Dir::In, Some((lt.cardinality, lt.symmetric))))
        }
        None => {
            let name = rel.strip_prefix('^').unwrap_or(rel);
            vocab.relations.contains(name).then(|| (name.to_string(), name != rel, None))
        }
    };
    let o = match object_str {
        Some(id) => entity(strip_entity(id), &mut unknown),
        None => {
            unknown.push(format!("object {} (a relation's object is an entity id)", claim.object));
            None
        }
    };
    let Some((base, swap, link)) = resolved else {
        unknown.insert(0, format!("relation {rel}"));
        return Err(unknown);
    };
    let base = match reason::spell_relation(&base) {
        Ok(b) => b,
        Err(e) => {
            unknown.insert(0, format!("relation {rel} ({e})"));
            return Err(unknown);
        }
    };
    match (s, o, unknown.is_empty()) {
        (Some(s), Some(o), true) => {
            let fact = if swap { Fact::new(o, base, s) } else { Fact::new(s, base, o) };
            Ok(Spelled { fact, form: Form::Relation { link } })
        }
        _ => Err(unknown),
    }
}

/// The fact a claim spells, over entities `closure` reads; terms outside the
/// vocabulary (or entities outside the reader's scope) are an
/// [`Error::Invalid`].
pub fn claim_fact(schema: &Schema, g: &Graph, closure: &Closure, claim: &Claim) -> Result<Fact> {
    let vocab = reason::vocabulary(schema);
    spell_claim(schema, &vocab, g, closure, claim, true)
        .map(|s| s.fact)
        .map_err(|u| Error::invalid(format!("outside vocabulary: {}", u.join(", "))))
}

/// What a claim is, judged against the graph and the closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Asserted,
    Derived,
    Contradicted,
    NotEntailed,
    OutsideVocabulary,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Asserted => "asserted",
            Verdict::Derived => "derived",
            Verdict::Contradicted => "contradicted",
            Verdict::NotEntailed => "not_entailed",
            Verdict::OutsideVocabulary => "outside_vocabulary",
        }
    }
}

/// What a claimed reasoning step is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepLabel {
    /// The rule concludes the claim from the premises and every premise is
    /// entailed.
    SoundStep,
    /// The rule applies to the premises, but some premise is not entailed
    /// (invented).
    PremisesNotInGraph,
    /// The named rule is not in the rule table.
    RuleNotInTable,
    /// The rule cannot conclude the claim from these premises.
    RuleDoesNotConcludeThis,
    /// No rule in the table concludes a fact like the claim (it can only be
    /// asserted).
    NoRuleConcludesThisRelation,
}

/// The check of a claimed reasoning step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepCheck {
    pub label: StepLabel,
    /// The rule-table line the step was checked against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    pub reason: String,
    /// Premises not entailed by the closure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<Fact>,
    /// Premise terms outside the vocabulary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<String>,
}

/// The verdict on one claim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaimVerdict {
    pub claim: Claim,
    /// The claim as a spelled fact (absent when outside the vocabulary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact: Option<Fact>,
    pub verdict: Verdict,
    pub reason: String,
    /// Terms outside the vocabulary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<String>,
    /// For `derived`: the derivation lines, premises before conclusions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derivation: Vec<Derivation>,
    /// For `derived`: the asserted facts the derivation rests on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leaves: Vec<Fact>,
    /// For `derived`: the rules the derivation uses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    /// For `contradicted`: the stored facts that say otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicting: Vec<Fact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<StepCheck>,
}

/// The verdicts on a list of claims.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub verdicts: Vec<ClaimVerdict>,
    /// Claims per verdict.
    pub counts: BTreeMap<String, usize>,
    /// False when the closure is a lower bound: `not_entailed` may then be
    /// "not derived yet".
    pub fixpoint_reached: bool,
    /// Terms the closure refused to spell: claims naming them are
    /// `outside_vocabulary` with the refusal as the reason.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub refused: usize,
    pub means: String,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// How a claim's reason names a term the reasoner refused.
const REFUSED_TERM: &str = "refused by the reasoner";

/// Stored facts that contradict a claim that is not entailed.
fn conflicts(schema: &Schema, g: &Graph, closure: &Closure, sp: &Spelled) -> Vec<Fact> {
    let f = &sp.fact;
    match &sp.form {
        Form::StoreValue => stored_value(g, f)
            .filter(|v| !same_value(v, &f.o))
            .map(|v| Fact::new(f.s.clone(), f.p.clone(), serde_json::to_string(&v).unwrap_or_default()))
            .into_iter()
            .collect(),
        Form::Relation { link: Some((Cardinality::One, false)) } => {
            let atoms = [AtomPattern { s: f.s.clone(), p: f.p.clone(), o: "?z".into() }];
            closure
                .query(&atoms, SAMPLE)
                .into_iter()
                .filter_map(|b| b.get("z").cloned())
                .map(|z| Fact::new(f.s.clone(), f.p.clone(), z))
                .filter(|x| x.o != f.o && closure.contains(x) == Some(Status::Asserted))
                .collect()
        }
        Form::Class { object_type: true } => {
            // Kinds are closed: an entity has exactly one stored kind, and is
            // of the claimed kind only if its kind is it or extends it. Read
            // from the schema, not the closure, so a closure that stopped
            // short of deriving the extends chain never contradicts it.
            let Some(e) = entity_of(&f.s).and_then(|id| g.get(id)) else { return Vec::new() };
            if schema.kind_is(&e.kind, strip_class(&f.o)) {
                return Vec::new();
            }
            reason::spell_class(&e.kind).map(|k| Fact::new(f.s.clone(), "a", k)).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

fn find_rule<'r>(rules: &'r [RuleLine], name: &str) -> Option<&'r RuleLine> {
    let name = name.trim();
    rules
        .iter()
        .find(|r| r.name == name)
        .or_else(|| rules.iter().find(|r| r.name == format!("{SUPPLIED_PREFIX}{name}")))
        .or_else(|| rules.iter().find(|r| r.name == format!("builtin/{name}")))
}

/// Assign each claimed premise to a distinct body atom (in any order) under
/// one binding; the atoms left over must be schema-level facts that hold
/// (`?r a transitive`, `#leaf extends #step`), supplied rather than demanded.
fn match_body(
    closure: &Closure,
    g: &Graph,
    body: &[&[String; 3]],
    premises: &[Fact],
    used: &mut [bool],
    b: &Bind,
) -> bool {
    let Some(k) = used.iter().position(|u| !u) else {
        return body.iter().all(|a| {
            let x = ground(a, b);
            ![&x.s, &x.p, &x.o].iter().any(|t| is_var(t))
                && entity_terms(&x).next().is_none()
                && held(closure, g, &x) != Entailment::NotEntailed
        });
    };
    for (j, atom) in body.iter().enumerate() {
        if let Some(next) = unify(atom, &premises[k], b) {
            used[k] = true;
            let rest: Vec<&[String; 3]> = body.iter().enumerate().filter(|(i, _)| *i != j).map(|(_, a)| *a).collect();
            if match_body(closure, g, &rest, premises, used, &next) {
                return true;
            }
            used[k] = false;
        }
    }
    false
}

/// Schema-level body atoms of `r` that the head binding `b` grounds and
/// that do not hold (for `builtin/transitive` on a relation not declared
/// transitive: `rel a transitive`).
fn unmet_schema_atoms(closure: &Closure, g: &Graph, r: &RuleLine, b: &Bind) -> Vec<Fact> {
    r.body
        .iter()
        .map(|a| ground(a, b))
        .filter(|x| ![&x.s, &x.p, &x.o].iter().any(|t| is_var(t)) && entity_terms(x).next().is_none())
        .filter(|x| held(closure, g, x) == Entailment::NotEntailed)
        .collect()
}

/// Could `r` conclude facts like `fact`? Its head unifies with the fact and
/// the schema facts the unification grounds hold (so `builtin/transitive`,
/// whose head matches any fact, only counts for relations declared
/// transitive). Entity premises are not consulted: this is about the
/// relation, not the instance.
fn could_conclude(closure: &Closure, g: &Graph, r: &RuleLine, fact: &Fact) -> bool {
    unify(&r.head, fact, &Bind::new()).is_some_and(|b| unmet_schema_atoms(closure, g, r, &b).is_empty())
}

fn check_step(
    schema: &Schema,
    vocab: &Vocabulary,
    g: &Graph,
    closure: &Closure,
    fact: &Fact,
    claim: &Claim,
) -> Option<StepCheck> {
    if claim.rule.is_none() && claim.premises.is_empty() {
        return None;
    }
    let rules = closure.rules();
    let step = |label, rule: Option<&RuleLine>, reason: String| StepCheck {
        label,
        rule: rule.map(|r| r.name.clone()),
        reason,
        missing: Vec::new(),
        unknown: Vec::new(),
    };
    let concluding: Vec<&RuleLine> = rules.iter().filter(|r| could_conclude(closure, g, r, fact)).collect();
    let named = match &claim.rule {
        Some(name) => match find_rule(rules, name) {
            Some(r) => Some(r),
            None => {
                return Some(step(StepLabel::RuleNotInTable, None, format!("no rule named {name} in the rule table")));
            }
        },
        None => None,
    };
    if concluding.is_empty() {
        return Some(step(
            StepLabel::NoRuleConcludesThisRelation,
            named,
            format!("no rule in the table concludes a fact like {fact}: it can only be asserted"),
        ));
    }
    if let Some(r) = named
        && !concluding.iter().any(|c| c.name == r.name)
    {
        let reason = match unify(&r.head, fact, &Bind::new()) {
            Some(b) => {
                let unmet: Vec<String> = unmet_schema_atoms(closure, g, r, &b).iter().map(Fact::to_string).collect();
                format!("{} cannot conclude {fact}: {} does not hold", r.name, unmet.join(", "))
            }
            None => format!("{} concludes {}, not {fact}", r.name, r.head.join(" ")),
        };
        return Some(step(StepLabel::RuleDoesNotConcludeThis, named, reason));
    }
    // Premises are spelled without requiring their entities to exist, so an
    // invented premise still matches the rule and is reported as invented.
    let mut premises = Vec::new();
    let mut unknown = Vec::new();
    for p in &claim.premises {
        match spell_claim(schema, vocab, g, closure, p, false) {
            Ok(s) => premises.push(s.fact),
            Err(u) => unknown.extend(u),
        }
    }
    if !unknown.is_empty() {
        // A premise outside the vocabulary cannot be spelled, so no rule body
        // atom matches it: the rule does not conclude the claim from it.
        let mut s = step(
            StepLabel::RuleDoesNotConcludeThis,
            named,
            format!(
                "a premise uses terms outside the vocabulary ({}): no rule in the table can be applied to it",
                unknown.join(", ")
            ),
        );
        s.unknown = unknown;
        return Some(s);
    }
    let candidates: Vec<&RuleLine> = match named {
        Some(r) => vec![r],
        None => concluding,
    };
    let applies = candidates.into_iter().find(|r| {
        let body: Vec<&[String; 3]> = r.body.iter().collect();
        premises.len() <= body.len()
            && unify(&r.head, fact, &Bind::new())
                .is_some_and(|b| match_body(closure, g, &body, &premises, &mut vec![false; premises.len()], &b))
    });
    let Some(rule) = applies else {
        let reason = match named {
            Some(r) => format!(
                "{} needs {} premise(s) matching {} (schema premises that hold may be left out); the claimed \
                 premises do not instantiate it with this conclusion",
                r.name,
                r.body.len(),
                r.body.iter().map(|a| a.join(" ")).collect::<Vec<_>>().join(", ")
            ),
            None => "no rule in the table concludes this from these premises".into(),
        };
        return Some(step(StepLabel::RuleDoesNotConcludeThis, named, reason));
    };
    let missing: Vec<Fact> =
        premises.iter().filter(|p| held(closure, g, p) == Entailment::NotEntailed).cloned().collect();
    if missing.is_empty() {
        Some(step(
            StepLabel::SoundStep,
            Some(rule),
            format!(
                "{} concludes {fact} from the claimed premises, all asserted or derived in this closure",
                rule.name
            ),
        ))
    } else {
        let mut s = step(
            StepLabel::PremisesNotInGraph,
            Some(rule),
            format!(
                "{} applies, but {} premise(s) are neither asserted nor derived in this closure",
                rule.name,
                missing.len()
            ),
        );
        s.missing = missing;
        Some(s)
    }
}

/// Judge each claim against the graph and the closure (which must have been
/// computed over `g` under the reader's scope; entities outside it are
/// unknown).
pub fn verify(schema: &Schema, g: &Graph, closure: &Closure, claims: &[Claim]) -> Vec<ClaimVerdict> {
    let vocab = reason::vocabulary(schema);
    let partial = if closure.fixpoint_reached() { "" } else { " (the closure is a lower bound)" };
    claims
        .iter()
        .map(|claim| {
            let mut v = ClaimVerdict {
                claim: claim.clone(),
                fact: None,
                verdict: Verdict::NotEntailed,
                reason: String::new(),
                unknown: Vec::new(),
                derivation: Vec::new(),
                leaves: Vec::new(),
                rules: Vec::new(),
                conflicting: Vec::new(),
                step: None,
            };
            let sp = match spell_claim(schema, &vocab, g, closure, claim, true) {
                Ok(sp) => sp,
                Err(unknown) => {
                    v.verdict = Verdict::OutsideVocabulary;
                    v.reason = if unknown.iter().any(|u| u.contains(REFUSED_TERM)) {
                        format!("cannot be reasoned over: {}", unknown.join(", "))
                    } else {
                        format!("not in this schema or not visible: {}", unknown.join(", "))
                    };
                    v.unknown = unknown;
                    return v;
                }
            };
            let f = sp.fact.clone();
            v.step = check_step(schema, &vocab, g, closure, &f, claim);
            match held(closure, g, &f) {
                Entailment::Asserted => {
                    v.verdict = Verdict::Asserted;
                    v.reason = format!("{f} is asserted");
                }
                Entailment::Derived => {
                    let (lines, leaves, rules) = unfold(closure, &f);
                    v.verdict = Verdict::Derived;
                    v.reason =
                        format!("{f} is derived in {} step(s) from {} asserted fact(s)", lines.len(), leaves.len());
                    v.derivation = lines;
                    v.leaves = leaves;
                    v.rules = rules;
                }
                Entailment::NotEntailed => {
                    let conflicting = conflicts(schema, g, closure, &sp);
                    if conflicting.is_empty() {
                        v.reason = format!("{f} is neither asserted nor derived{partial}");
                    } else {
                        v.verdict = Verdict::Contradicted;
                        let shown: Vec<String> = conflicting.iter().map(Fact::to_string).collect();
                        v.reason = match sp.form {
                            Form::Relation { .. } => {
                                format!("{} has at most one {} link: {}", f.s, f.p, shown.join("; "))
                            }
                            Form::Class { .. } => format!("kinds are closed: {}", shown.join("; ")),
                            _ => format!("the stored value differs: {}", shown.join("; ")),
                        };
                        v.conflicting = conflicting;
                    }
                }
            }
            v.fact = Some(f);
            v
        })
        .collect()
}

fn verify_report(verdicts: Vec<ClaimVerdict>, closure: &Closure) -> VerifyReport {
    let mut counts = BTreeMap::new();
    for v in &verdicts {
        *counts.entry(v.verdict.as_str().to_string()).or_insert(0) += 1;
    }
    VerifyReport {
        verdicts,
        counts,
        fixpoint_reached: closure.fixpoint_reached(),
        refused: closure.stats().refused,
        means: VERIFY_MEANS.into(),
    }
}

// ── slices ───────────────────────────────────────────────────────────────

/// Scope and limits of a slice check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SliceOptions {
    #[serde(flatten)]
    pub reason: ReasonOptions,
    /// Lost facts listed (all are counted).
    pub limit: usize,
}

impl Default for SliceOptions {
    fn default() -> Self {
        Self { reason: ReasonOptions::default(), limit: 100 }
    }
}

/// How a goal fares in a slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SliceStatus {
    Preserved,
    Lost,
    NotEntailed,
    /// The slice entails what the full graph does not: a soundness alarm.
    SliceOnly,
    OutsideVocabulary,
}

impl SliceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SliceStatus::Preserved => "preserved",
            SliceStatus::Lost => "lost",
            SliceStatus::NotEntailed => "not_entailed",
            SliceStatus::SliceOnly => "slice_only",
            SliceStatus::OutsideVocabulary => "outside_vocabulary",
        }
    }
}

/// One goal checked against a slice.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalCheck {
    pub claim: Claim,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact: Option<Fact>,
    pub status: SliceStatus,
    /// Asserted leaves of the full derivation the slice lacks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_premises: Vec<Fact>,
    /// Entities outside the slice those premises mention.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_entities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<String>,
}

/// A derived fact over slice entities that the slice loses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LostFact {
    pub fact: Fact,
    pub rule: String,
    pub missing_premises: Vec<Fact>,
    pub missing_entities: Vec<String>,
}

/// Does a slice of the graph keep what the whole visible graph entails?
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SliceReport {
    /// Entity ids in the slice.
    pub slice: usize,
    /// Slice ids that are absent, invisible or out of scope (sample).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown_ids: Vec<String>,
    pub full_asserted: usize,
    pub full_derived: usize,
    pub slice_asserted: usize,
    pub slice_derived: usize,
    /// Per goal (when goals are given).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub goals: Vec<GoalCheck>,
    /// Without goals: derived facts over slice entities the slice loses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lost_in_slice_vocabulary: Vec<LostFact>,
    /// Goals (or slice-vocabulary derived facts) preserved and lost.
    pub preserved: usize,
    pub lost: usize,
    /// Goals per status (when goals are given).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counts: BTreeMap<String, usize>,
    /// Share of asserted facts touching slice entities that the slice keeps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<f64>,
    /// Facts the slice entails and the full graph does not (sample; should
    /// never happen for monotone rules).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slice_only: Vec<Fact>,
    /// The lost list was capped at `limit`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    pub fixpoint_reached: bool,
    pub means: String,
}

fn missing_in(full: &Closure, sliced: &Closure, g: &Graph, slice: &BTreeSet<Id>, f: &Fact) -> (Vec<Fact>, Vec<String>) {
    let leaves = if full.contains(f) == Some(Status::Asserted) || is_store_value(f) {
        vec![f.clone()]
    } else {
        unfold(full, f).1
    };
    let missing: Vec<Fact> = leaves.into_iter().filter(|l| held(sliced, g, l) == Entailment::NotEntailed).collect();
    let entities: BTreeSet<String> =
        missing.iter().flat_map(entity_terms).filter(|id| !slice.contains(*id)).map(str::to_string).collect();
    (missing, entities.into_iter().collect())
}

/// Compare the closure of the visible graph with the closure of the facts
/// whose entities all lie in `slice` (schema facts always included).
pub fn slice_check(
    schema: &Schema,
    g: &Graph,
    slice: &BTreeSet<Id>,
    goals: Option<&[Claim]>,
    opts: &SliceOptions,
    can_read: &dyn Fn(&Entity) -> bool,
) -> SliceReport {
    let full = reason::closure(schema, g, &opts.reason, can_read);
    let in_slice = |e: &Entity| slice.contains(&e.id) && can_read(e);
    let sliced = reason::closure(schema, g, &opts.reason, &in_slice);
    let ws = opts.reason.workspace.as_deref();
    let unknown_ids: Vec<String> = slice
        .iter()
        .filter(|id| g.get(id).is_none_or(|e| !can_read(e) || ws.is_some_and(|w| w != e.workspace)))
        .take(SAMPLE)
        .cloned()
        .collect();
    let touching: Vec<&Fact> =
        full.asserted().iter().filter(|f| entity_terms(f).any(|id| slice.contains(id))).collect();
    let kept = touching.iter().filter(|f| sliced.contains(f).is_some()).count();
    let coverage = (!touching.is_empty()).then(|| kept as f64 / touching.len() as f64);
    let slice_only: Vec<Fact> = sliced
        .asserted()
        .iter()
        .chain(sliced.derived().iter().map(|d| &d.conclusion))
        .filter(|f| full.contains(f).is_none())
        .take(SAMPLE)
        .cloned()
        .collect();
    let mut report = SliceReport {
        slice: slice.len(),
        unknown_ids,
        full_asserted: full.asserted().len(),
        full_derived: full.derived().len(),
        slice_asserted: sliced.asserted().len(),
        slice_derived: sliced.derived().len(),
        goals: Vec::new(),
        lost_in_slice_vocabulary: Vec::new(),
        preserved: 0,
        lost: 0,
        counts: BTreeMap::new(),
        coverage,
        slice_only,
        truncated: false,
        fixpoint_reached: full.fixpoint_reached() && sliced.fixpoint_reached(),
        means: SLICE_MEANS.into(),
    };
    match goals {
        Some(goals) => {
            let vocab = reason::vocabulary(schema);
            for claim in goals {
                let mut check = GoalCheck {
                    claim: claim.clone(),
                    fact: None,
                    status: SliceStatus::NotEntailed,
                    missing_premises: Vec::new(),
                    missing_entities: Vec::new(),
                    unknown: Vec::new(),
                };
                match spell_claim(schema, &vocab, g, &full, claim, true) {
                    Err(unknown) => {
                        check.status = SliceStatus::OutsideVocabulary;
                        check.unknown = unknown;
                    }
                    Ok(sp) => {
                        let f = sp.fact;
                        let in_full = held(&full, g, &f) != Entailment::NotEntailed;
                        let in_slice = held(&sliced, g, &f) != Entailment::NotEntailed;
                        check.status = match (in_full, in_slice) {
                            (true, true) => SliceStatus::Preserved,
                            (true, false) => {
                                let (m, e) = missing_in(&full, &sliced, g, slice, &f);
                                check.missing_premises = m;
                                check.missing_entities = e;
                                SliceStatus::Lost
                            }
                            (false, false) => SliceStatus::NotEntailed,
                            (false, true) => SliceStatus::SliceOnly,
                        };
                        if check.status == SliceStatus::SliceOnly && !report.slice_only.contains(&f) {
                            report.slice_only.push(f.clone());
                        }
                        check.fact = Some(f);
                    }
                }
                *report.counts.entry(check.status.as_str().to_string()).or_insert(0) += 1;
                match check.status {
                    SliceStatus::Preserved => report.preserved += 1,
                    SliceStatus::Lost => report.lost += 1,
                    _ => {}
                }
                report.goals.push(check);
            }
        }
        None => {
            for d in full.derived() {
                let f = &d.conclusion;
                // Facts over slice entities only: schema-level facts
                // (`#a extends #c`) are in every slice and say nothing about it.
                if entity_terms(f).next().is_none() || !entity_terms(f).all(|id| slice.contains(id)) {
                    continue;
                }
                if sliced.contains(f).is_some() {
                    report.preserved += 1;
                    continue;
                }
                report.lost += 1;
                if report.lost_in_slice_vocabulary.len() >= opts.limit {
                    report.truncated = true;
                    continue;
                }
                let (missing_premises, missing_entities) = missing_in(&full, &sliced, g, slice, f);
                report.lost_in_slice_vocabulary.push(LostFact {
                    fact: f.clone(),
                    rule: d.rule.clone(),
                    missing_premises,
                    missing_entities,
                });
            }
        }
    }
    report
}

// ── engine wrappers ──────────────────────────────────────────────────────

impl Ontology {
    /// Run `f` over `p`'s closure and the graph it was computed from.
    fn with_closure<T>(
        &self,
        p: &Principal,
        opts: &ReasonOptions,
        f: impl FnOnce(&Schema, &Graph, &Closure) -> Result<T>,
    ) -> Result<T> {
        for _ in 0..3 {
            let before = self.revision();
            let c = self.reason(p, opts)?;
            let g = self.reader(p)?;
            // Writes bump the revision under the write lock: an unchanged
            // revision means the cached closure matches this graph.
            if self.revision() == before {
                return f(&self.schema(), &g, &c);
            }
        }
        // Busy writers: compute the closure under this read guard.
        check_scope(p, opts)?;
        let g = self.reader(p)?;
        let schema = self.schema();
        let can = |e: &Entity| policy::visible(p, e, &g);
        let c = reason::closure(&schema, &g, opts, &can);
        f(&schema, &g, &c)
    }

    /// The justifications of a claim (see [`justify`]).
    pub fn reason_explain(
        &self,
        p: &Principal,
        opts: &ReasonOptions,
        claim: &Claim,
        bounds: &JustifyOptions,
    ) -> Result<Justifications> {
        self.with_closure(p, opts, |schema, g, c| {
            let fact = claim_fact(schema, g, c, claim)?;
            if is_store_value(&fact) && c.contains(&fact).is_none() {
                return Ok(if store_holds(c, g, &fact) {
                    Justifications::store_value(fact, c.fixpoint_reached())
                } else {
                    justify(c, &fact, bounds)
                });
            }
            Ok(justify(c, &fact, bounds))
        })
    }

    /// A claim's provenance in `semiring`; trust weights come from the
    /// `confidence` properties of the entities and links `p` can read.
    pub fn reason_provenance(
        &self,
        p: &Principal,
        opts: &ReasonOptions,
        claim: &Claim,
        semiring: Semiring,
        bounds: &JustifyOptions,
    ) -> Result<Provenance> {
        self.with_closure(p, opts, |schema, g, c| {
            let fact = claim_fact(schema, g, c, claim)?;
            // A store value the closure does not hold (no rule reads it) is
            // read from the store, as `reason_explain` and `reason_verify`
            // do. One the store does not hold either has no derivation at
            // all (rules never conclude states or property values), so the
            // general path's exact 0 is the truth.
            if is_store_value(&fact) && c.contains(&fact).is_none() && store_holds(c, g, &fact) {
                let weight = confidence_weights(g, std::slice::from_ref(&fact)).get(&fact).copied().unwrap_or(1.0);
                return Ok(Provenance::store_value(fact, semiring, weight, c.fixpoint_reached()));
            }
            let weights =
                if semiring == Semiring::Trust { confidence_weights(g, c.asserted()) } else { BTreeMap::new() };
            Ok(provenance_with(c, &fact, semiring, &weights, bounds))
        })
    }

    /// Verdicts on claims (see [`verify`]).
    pub fn reason_verify(&self, p: &Principal, opts: &ReasonOptions, claims: &[Claim]) -> Result<VerifyReport> {
        self.with_closure(p, opts, |schema, g, c| Ok(verify_report(verify(schema, g, c, claims), c)))
    }

    /// Does `slice` keep what `p`'s visible graph entails (see [`slice_check`])?
    pub fn reason_slice_check(
        &self,
        p: &Principal,
        opts: &SliceOptions,
        slice: &BTreeSet<Id>,
        goals: Option<&[Claim]>,
    ) -> Result<SliceReport> {
        check_scope(p, &opts.reason)?;
        let g = self.reader(p)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        Ok(slice_check(&self.schema(), &g, slice, goals, opts, &can))
    }
}

fn check_scope(p: &Principal, opts: &ReasonOptions) -> Result<()> {
    match &opts.workspace {
        Some(ws) if !p.in_workspace(ws) => Err(Error::Denied(format!("{} has no access to workspace {ws}", p.id))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Link;
    use crate::reason::closure;
    use serde_json::json;

    fn entity(id: &str) -> Entity {
        Entity {
            id: id.into(),
            kind: id.split(':').next().unwrap_or_default().into(),
            workspace: "acme".into(),
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
            [link_types.next]
            transitive = true
            [link_types.near]
            symmetric = true
            "#,
        )
        .unwrap()
    }

    fn graph(nodes: &[&str], links: &[(&str, &str, &str)]) -> Graph {
        let mut g = Graph::default();
        for n in nodes {
            g.put_entity(entity(&format!("node:{n}")));
        }
        for (a, r, b) in links {
            g.put_link(link(&format!("node:{a}"), r, &format!("node:{b}")));
        }
        g
    }

    fn f(s: &str, p: &str, o: &str) -> Fact {
        Fact::new(s, p, o)
    }

    #[test]
    fn unify_binds_consistently() {
        let atom = ["?x".to_string(), "next".to_string(), "?x".to_string()];
        assert!(unify(&atom, &f("<a>", "next", "<a>"), &Bind::new()).is_some());
        assert!(unify(&atom, &f("<a>", "next", "<b>"), &Bind::new()).is_none());
        assert!(unify(&atom, &f("<a>", "prev", "<a>"), &Bind::new()).is_none());
    }

    #[test]
    fn the_independent_evaluator_agrees_with_the_reasoner() {
        let s = schema();
        let g = graph(
            &["a", "b", "c", "d"],
            &[("a", "next", "b"), ("b", "next", "c"), ("c", "next", "d"), ("a", "near", "d")],
        );
        let c = closure(&s, &g, &ReasonOptions::default(), &all);
        let base: BTreeSet<Fact> = c.asserted().iter().cloned().collect();
        for d in c.derived() {
            assert_eq!(
                entails(c.rules(), &base, &d.conclusion, &mut usize::MAX.clone()),
                Some(true),
                "{}",
                d.conclusion
            );
        }
        assert_eq!(entails(c.rules(), &base, &f("<node:d>", "next", "<node:a>"), &mut usize::MAX.clone()), Some(false));
        // A budget too small to reach the goal is no answer.
        let chain: BTreeSet<Fact> =
            c.asserted().iter().filter(|x| x.p == "next" || x.o == "transitive").cloned().collect();
        assert_eq!(entails(c.rules(), &chain, &f("<node:a>", "next", "<node:d>"), &mut 1), None);
    }

    #[test]
    fn the_cone_holds_instances_that_rederive_asserted_facts() {
        let s = schema();
        // a -> c is stored and also follows from a -> b -> c.
        let g = graph(&["a", "b", "c"], &[("a", "next", "b"), ("b", "next", "c"), ("a", "next", "c")]);
        let c = closure(&s, &g, &ReasonOptions::default(), &all);
        let ac = f("<node:a>", "next", "<node:c>");
        assert_eq!(c.contains(&ac), Some(Status::Asserted));
        let j = justify(&c, &ac, &JustifyOptions::default());
        assert!(j.complete, "{j:?}");
        let sets: Vec<Vec<Fact>> = j.justifications.iter().map(|j| j.facts.clone()).collect();
        assert_eq!(
            sets,
            vec![
                vec![ac.clone()],
                vec![
                    f("<node:a>", "next", "<node:b>"),
                    f("<node:b>", "next", "<node:c>"),
                    f("next", "a", "transitive")
                ],
            ]
        );
        assert!(j.justifications.iter().all(|j| j.sufficient && j.minimal));
    }

    #[test]
    fn cycles_are_detected_and_counted_as_a_lower_bound() {
        let s = schema();
        let g = graph(&["a", "b"], &[("a", "near", "b")]);
        let c = closure(&s, &g, &ReasonOptions::default(), &all);
        let ba = f("<node:b>", "near", "<node:a>");
        let p = provenance(&c, &ba, Semiring::Count, &BTreeMap::new());
        assert!(p.cycle_in_support);
        assert!(!p.exact);
        assert_eq!(p.value, ProvenanceValue::Count(1));
        let t = provenance(&c, &ba, Semiring::Trust, &BTreeMap::new());
        assert!(t.cycle_in_support && t.exact);
        assert_eq!(t.value, ProvenanceValue::Trust(1.0));
    }

    #[test]
    fn a_self_loop_makes_the_count_a_lower_bound() {
        let s = schema();
        let g = graph(&["a", "b"], &[("a", "next", "b"), ("b", "next", "b")]);
        let c = closure(&s, &g, &ReasonOptions::default(), &all);
        let ab = f("<node:a>", "next", "<node:b>");
        let p = provenance(&c, &ab, Semiring::Count, &BTreeMap::new());
        assert!(p.cycle_in_support && !p.exact, "{p:?}");
        assert_eq!(p.value, ProvenanceValue::Count(1));
        let w = provenance(&c, &ab, Semiring::Why, &BTreeMap::new());
        assert!(w.cycle_in_support && w.exact, "{w:?}");
    }

    #[test]
    fn provenance_values_serialize_untagged() {
        assert_eq!(serde_json::to_value(ProvenanceValue::Count(2)).unwrap(), json!(2));
        assert_eq!(serde_json::to_value(ProvenanceValue::Trust(0.5)).unwrap(), json!(0.5));
        let why = ProvenanceValue::Why(vec![vec![f("<a>", "a", "#b")]]);
        let back: ProvenanceValue = serde_json::from_value(serde_json::to_value(&why).unwrap()).unwrap();
        assert_eq!(back, why);
        let back: ProvenanceValue = serde_json::from_value(json!(1.0)).unwrap();
        assert_eq!(back, ProvenanceValue::Trust(1.0));
    }

    #[test]
    fn a_fact_outside_the_closure_has_no_justification() {
        let s = schema();
        let g = graph(&["a", "b"], &[("a", "next", "b")]);
        let c = closure(&s, &g, &ReasonOptions::default(), &all);
        let j = justify(&c, &f("<node:b>", "next", "<node:a>"), &JustifyOptions::default());
        assert_eq!(j.status, Entailment::NotEntailed);
        assert!(j.justifications.is_empty());
        assert!(j.complete);
        let p = provenance(&c, &f("<node:b>", "next", "<node:a>"), Semiring::Count, &BTreeMap::new());
        assert_eq!(p.value, ProvenanceValue::Count(0));
    }

    #[test]
    fn a_partial_closure_never_claims_completeness() {
        let s = schema();
        let g = graph(&["a", "b", "c", "d"], &[("a", "next", "b"), ("b", "next", "c"), ("c", "next", "d")]);
        let c = closure(&s, &g, &ReasonOptions { max_rounds: 1, ..Default::default() }, &all);
        assert!(!c.fixpoint_reached());
        let j = justify(&c, &f("<node:a>", "next", "<node:c>"), &JustifyOptions::default());
        assert_eq!(j.status, Entailment::Derived);
        assert!(!j.complete);
        assert!(!j.fixpoint_reached);
        assert_eq!(j.stopped.as_deref(), Some("the closure is a lower bound"));
    }

    #[test]
    fn random_graphs_give_sufficient_and_minimal_justifications() {
        let s = schema();
        let mut rng = fastrand::Rng::with_seed(7);
        for round in 0..25 {
            let n = 3 + rng.usize(..4);
            let names: Vec<String> = (0..n).map(|i| format!("n{i}")).collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let mut links = Vec::new();
            for _ in 0..rng.usize(2..7) {
                let (a, b) = (rng.usize(..n), rng.usize(..n));
                if a != b {
                    links.push((refs[a], if rng.bool() { "next" } else { "near" }, refs[b]));
                }
            }
            let g = graph(&refs, &links);
            let c = closure(&s, &g, &ReasonOptions::default(), &all);
            // The evaluator agrees with the reasoner on every pair.
            let base: BTreeSet<Fact> = c.asserted().iter().cloned().collect();
            for a in &refs {
                for b in &refs {
                    for rel in ["next", "near"] {
                        let x = f(&format!("<node:{a}>"), rel, &format!("<node:{b}>"));
                        let want = c.contains(&x).is_some();
                        assert_eq!(
                            entails(c.rules(), &base, &x, &mut usize::MAX.clone()),
                            Some(want),
                            "round {round}: {x}"
                        );
                    }
                }
            }
            for d in c.derived() {
                let j = justify(&c, &d.conclusion, &JustifyOptions::default());
                assert!(!j.justifications.is_empty(), "round {round}: {}", d.conclusion);
                for x in &j.justifications {
                    assert!(x.sufficient && x.minimal, "round {round}: {} {x:?}", d.conclusion);
                }
                // No justification contains another.
                for (i, a) in j.justifications.iter().enumerate() {
                    for b in &j.justifications[i + 1..] {
                        let (a, b): (BTreeSet<_>, BTreeSet<_>) = (a.facts.iter().collect(), b.facts.iter().collect());
                        assert!(!a.is_subset(&b) && !b.is_subset(&a), "round {round}");
                    }
                }
                // Each minimal justification is the leaf set of a tree of its
                // own, so an exact count is at least their number.
                let p = provenance(&c, &d.conclusion, Semiring::Count, &BTreeMap::new());
                let ProvenanceValue::Count(k) = p.value else { panic!("{p:?}") };
                assert!(k >= 1, "round {round}");
                if p.exact && j.complete {
                    assert!(k as usize >= j.justifications.len(), "round {round}: {k} < {}", j.justifications.len());
                }
            }
        }
    }
}
