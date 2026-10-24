//! Schema alignment: which object types and link types of one schema
//! correspond to which of another, how sure that opinion is, and whether a
//! claimed mapping survives a comparison of the two hierarchies.
//!
//! * **A score is an opinion.** [`align`] fuses several signals into one
//!   score per pair: names (Jaro-Winkler, token Jaccard, glossary synonyms),
//!   properties, link types, parents and states for object types; names and
//!   endpoints for link types. The report says so in its `means` field:
//!   scores are opinions, and a mapping should be certified before anything
//!   relies on it.
//! * **Absence of evidence is not disagreement.** A signal neither side has
//!   data for (two kinds without states) is left out and the remaining
//!   weights are renormalized, so it neither helps nor hurts. Each row says
//!   which share of the total weight it was scored on (`evidence_coverage`).
//! * **Names alone justify at most a close match.** `exact_match` also needs
//!   structural agreement (shared properties or link types); a pair scored
//!   on names only is `semapv:LexicalMatching`, any other
//!   `semapv:CompositeMatching`.
//! * **Ties are never resolved silently.** Matching is greedy and one-to-one
//!   over the pairs sorted by score; a pair with a competitor within
//!   `tie_epsilon` is `borderline` and names the competitors in `tied_with`.
//!   So is a pair that rests on a similar spelling alone: `accepted` needs
//!   structure, or names that are the same word (own names or a glossary).
//! * **Certification can only refute.** [`certify`] compares the ancestor
//!   sets (`extends` chain and implemented interfaces) of both sides of each
//!   claimed mapping, carried through the mapping itself. It compares
//!   positions in hierarchies, not meanings: it keeps, weakens or leaves a
//!   claim uncertified, and never strengthens one.
//! * **Proposals, not writes.** The report carries an SSSOM TSV and a
//!   [`SchemaChangeSet`] of glossary terms for the accepted matches; nothing
//!   is applied here.
//! * **Determinism.** Every collection is ordered by name and every sort has
//!   a total tie-break, so the same two schemas give the same bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::live_schema::{GlossarySection, SchemaChange, SchemaChangeSet};
use crate::policy::{Access, Principal};
use crate::schema::{self, Dir, PropType, Schema};
use crate::{Error, Ontology, Result};

/// Object-type signal weights (renormalized over the signals that are defined).
pub const W_NAME: f64 = 0.35;
pub const W_PROPERTIES: f64 = 0.25;
pub const W_LINKS: f64 = 0.20;
pub const W_PARENTS: f64 = 0.10;
pub const W_STATES: f64 = 0.10;
/// Link-type signal weights.
pub const W_LINK_NAME: f64 = 0.5;
pub const W_LINK_ENDPOINTS: f64 = 0.5;

/// A pair scoring at least this (with structural agreement) is an `exact_match`.
pub const EXACT_MIN: f64 = 0.85;
/// Structural agreement an `exact_match` needs: property or link Jaccard
/// (object types), endpoint agreement (link types).
pub const STRUCTURE_MIN: f64 = 0.5;
/// A pair scoring at least this is a `close_match` (below: `related_match`).
pub const CLOSE_MIN: f64 = 0.6;
/// A pair scoring at least this, without a tie, is `accepted`.
pub const ACCEPT_MIN: f64 = 0.75;
/// Default for [`AlignOptions::min_score`]: weaker pairs are not candidates.
pub const DEFAULT_MIN_SCORE: f64 = 0.5;
/// Default for [`AlignOptions::tie_epsilon`].
pub const DEFAULT_TIE_EPSILON: f64 = 0.02;
/// Default for [`AlignOptions::alternatives`].
pub const DEFAULT_ALTERNATIVES: usize = 3;
/// Largest TOML source [`resolve_source`] accepts.
pub const MAX_SOURCE_BYTES: usize = 4 << 20;
/// Most object-type (or link-type) pairs one alignment scores.
pub const MAX_PAIRS: usize = 1_000_000;

/// What an alignment report is.
pub const MEANS: &str = "scores are opinions; certify a mapping before you rely on it";

/// What a certification report is.
pub const CERTIFY_MEANS: &str = "certification compares positions in the two hierarchies (extends chains and \
     implemented interfaces) through the mapping itself, not meanings: it can keep, weaken or leave a claimed match \
     uncertified, never strengthen it";

const SKOS: &str = "http://www.w3.org/2004/02/skos/core#";
const SEMAPV: &str = "https://w3id.org/semapv/vocab/";
const BASE: &str = "https://onto-intelli.dev/";

fn is_false(b: &bool) -> bool {
    !*b
}

// ── public types ─────────────────────────────────────────────────────────

/// A SKOS mapping predicate. `S broad_match T` means T is broader than S
/// (SKOS direction); `S narrow_match T` means T is narrower.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchPredicate {
    #[serde(alias = "skos:exactMatch", alias = "exactMatch")]
    ExactMatch,
    #[serde(alias = "skos:closeMatch", alias = "closeMatch")]
    CloseMatch,
    #[serde(alias = "skos:broadMatch", alias = "broadMatch")]
    BroadMatch,
    #[serde(alias = "skos:narrowMatch", alias = "narrowMatch")]
    NarrowMatch,
    #[serde(alias = "skos:relatedMatch", alias = "relatedMatch")]
    RelatedMatch,
}

impl MatchPredicate {
    const ALL: [MatchPredicate; 5] = [
        MatchPredicate::ExactMatch,
        MatchPredicate::CloseMatch,
        MatchPredicate::BroadMatch,
        MatchPredicate::NarrowMatch,
        MatchPredicate::RelatedMatch,
    ];

    /// `exact_match`, ...
    pub fn as_str(self) -> &'static str {
        match self {
            MatchPredicate::ExactMatch => "exact_match",
            MatchPredicate::CloseMatch => "close_match",
            MatchPredicate::BroadMatch => "broad_match",
            MatchPredicate::NarrowMatch => "narrow_match",
            MatchPredicate::RelatedMatch => "related_match",
        }
    }

    /// The SKOS local name: `exactMatch`, ...
    pub fn local_name(self) -> &'static str {
        match self {
            MatchPredicate::ExactMatch => "exactMatch",
            MatchPredicate::CloseMatch => "closeMatch",
            MatchPredicate::BroadMatch => "broadMatch",
            MatchPredicate::NarrowMatch => "narrowMatch",
            MatchPredicate::RelatedMatch => "relatedMatch",
        }
    }

    /// The SSSOM `predicate_id`: `skos:exactMatch`, ...
    pub fn curie(self) -> String {
        format!("skos:{}", self.local_name())
    }

    /// Parse `exact_match`, `exactMatch`, `skos:exactMatch` or the full SKOS IRI.
    pub fn parse(text: &str) -> Option<Self> {
        let t = text.trim().trim_start_matches('<').trim_end_matches('>');
        let local = t.strip_prefix(SKOS).or_else(|| t.strip_prefix("skos:")).unwrap_or(t);
        Self::ALL.into_iter().find(|p| p.as_str() == local || p.local_name() == local)
    }
}

/// How far an alignment row may be relied on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    /// Scored at least [`ACCEPT_MIN`] with no competitor within `tie_epsilon`,
    /// on more than a similar name (structure, or names that are the same word).
    Accepted,
    /// Scored below [`ACCEPT_MIN`], tied, or rests on a similar name alone:
    /// a reviewer decides.
    Borderline,
}

/// Knobs of [`align`]; every field has a default.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AlignOptions {
    /// Pairs scoring below this are not candidates.
    pub min_score: f64,
    /// Competing scores within this distance of a match make it a tie.
    pub tie_epsilon: f64,
    /// Source object types to align (empty: all).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_kinds: Vec<String>,
    /// Source link types to align (empty: all).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_links: Vec<String>,
    /// Runner-up candidates listed per row, for review.
    pub alternatives: usize,
}

impl Default for AlignOptions {
    fn default() -> Self {
        Self {
            min_score: DEFAULT_MIN_SCORE,
            tie_epsilon: DEFAULT_TIE_EPSILON,
            source_kinds: Vec::new(),
            source_links: Vec::new(),
            alternatives: DEFAULT_ALTERNATIVES,
        }
    }
}

/// The signal values behind one score, each in `[0, 1]`. `None` means
/// neither side had data for it: it was left out, not counted as zero.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Signals {
    /// Best name similarity (1.0 when a glossary makes them the same word).
    pub name: f64,
    /// A glossary synonym equates the two names.
    #[serde(default, skip_serializing_if = "is_false")]
    pub glossary: bool,
    /// Property-name Jaccard, weighted 80/20 with the share of shared names
    /// whose types agree (object types).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<f64>,
    /// Plain property-name Jaccard (object types).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property_jaccard: Option<f64>,
    /// Jaccard of the link types whose `from` or `to` names the kind, a
    /// supertype or an interface of it, matched by name or inverse (object types).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<f64>,
    /// Name similarity of the ancestors and interfaces (object types).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parents: Option<f64>,
    /// State Jaccard (object types).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub states: Option<f64>,
    /// Agreement of `from`/`to` kinds mapped through the accepted kind matches (link types).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<f64>,
}

/// A scored pair, as a competitor or alternative of a row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub source: String,
    pub target: String,
    pub score: f64,
}

/// One proposed correspondence.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Match {
    pub source: String,
    pub source_label: String,
    pub target: String,
    pub target_label: String,
    pub predicate: MatchPredicate,
    pub status: MatchStatus,
    /// Weighted score over the defined signals (3 decimals).
    pub score: f64,
    pub signals: Signals,
    /// Share of the total signal weight this score rests on (3 decimals).
    pub evidence_coverage: f64,
    /// `semapv:LexicalMatching` (names only) or `semapv:CompositeMatching`.
    pub justification: String,
    /// One sentence per signal, and why the predicate and status were chosen.
    pub evidence: Vec<String>,
    /// Competing pairs within `tie_epsilon` (the row is then borderline).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tied_with: Vec<Candidate>,
    /// The next-best targets for this source, for review.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<Candidate>,
}

/// A definition no row covers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unmatched {
    /// `object_types` or `link_types`.
    pub section: String,
    pub name: String,
    pub label: String,
    /// The best-scoring counterpart, whatever its score, for review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nearest: Option<Candidate>,
}

/// Row counts of an [`AlignReport`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlignCounts {
    pub accepted: usize,
    pub borderline: usize,
    pub unmatched_source: usize,
    pub unmatched_target: usize,
}

/// The thresholds a report was computed with.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Thresholds {
    pub exact: f64,
    pub structure: f64,
    pub close: f64,
    pub accept: f64,
    pub min_score: f64,
    pub tie_epsilon: f64,
}

/// The result of [`align`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlignReport {
    pub source_schema: String,
    pub target_schema: String,
    /// Object-type rows, sorted by source then target.
    pub kinds: Vec<Match>,
    /// Link-type rows, sorted by source then target.
    pub links: Vec<Match>,
    pub unmatched_source: Vec<Unmatched>,
    pub unmatched_target: Vec<Unmatched>,
    pub counts: AlignCounts,
    pub thresholds: Thresholds,
    /// Every row (kinds, then links) as SSSOM TSV.
    pub sssom_tsv: String,
    /// Glossary terms (source word → target name) for accepted exact and
    /// close matches whose names differ. A proposal: nothing is applied.
    pub change_set: SchemaChangeSet,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub means: String,
}

/// A claimed correspondence between a source term and a target term (object
/// types or interfaces).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    pub source: String,
    pub target: String,
    pub predicate: MatchPredicate,
}

/// What certification concluded about one mapping row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Certification {
    /// The ancestor sets carried through the mapping do not refute the claim.
    Upheld,
    /// As upheld, but some ancestors are not carried by the mapping (see `gaps`).
    UpheldWithGaps,
    /// The hierarchies refute the claim: `predicate` is the weaker one they allow.
    Downgraded,
    /// Nothing could be compared: the claim is kept as written, never upgraded.
    Uncertified,
}

impl Certification {
    /// `upheld`, `upheld_with_gaps`, `downgraded` or `uncertified`.
    pub fn as_str(self) -> &'static str {
        match self {
            Certification::Upheld => "upheld",
            Certification::UpheldWithGaps => "upheld_with_gaps",
            Certification::Downgraded => "downgraded",
            Certification::Uncertified => "uncertified",
        }
    }
}

/// Which side of a mapping a gap is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Source,
    Target,
}

/// An ancestor the mapping does not carry: a source ancestor no row maps, or
/// a target ancestor no row maps onto.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Gap {
    pub side: Side,
    pub ancestor: String,
}

/// One certified mapping row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CertifiedMapping {
    pub source: String,
    pub target: String,
    /// The predicate as claimed.
    pub claimed: MatchPredicate,
    /// The predicate certification keeps (the claimed one unless downgraded).
    pub predicate: MatchPredicate,
    pub certification: Certification,
    /// Why, in one sentence naming the sets compared.
    pub reason: String,
    /// The source term's ancestors (extends chain and interfaces).
    pub source_ancestors: Vec<String>,
    /// The target term's ancestors.
    pub target_ancestors: Vec<String>,
    /// Source ancestors translated through the mapping (target names).
    pub carried: Vec<String>,
    /// Target ancestors the mapping maps something onto.
    pub comparable: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<Gap>,
}

/// Row counts of a [`CertifyReport`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertifyCounts {
    pub upheld: usize,
    pub upheld_with_gaps: usize,
    pub downgraded: usize,
    pub uncertified: usize,
}

/// The result of [`certify_report`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CertifyReport {
    pub rows: Vec<CertifiedMapping>,
    pub counts: CertifyCounts,
    /// Source object types (in scope) that no mapping row names.
    pub unmapped_source: Vec<String>,
    /// Target object types that no mapping row names.
    pub unmapped_target: Vec<String>,
    /// The rows with their certified predicates as SSSOM TSV.
    pub sssom_tsv: String,
    pub means: String,
}

/// Where the schema to align comes from.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlignSource {
    /// A built-in package or registered pack, by id (`manufacturing` or `builtin:manufacturing`).
    Pack(String),
    /// A schema TOML document.
    Toml(String),
}

/// A source schema ready to align: the compiled schema and, when it was
/// layered on required packages, the definitions its own file declares.
#[derive(Clone, Debug)]
pub struct ResolvedSource {
    pub schema: Schema,
    /// Packages layered beneath the file (from `[pack].requires`, or
    /// `enterprise` for a layer that does not compile alone).
    pub layered_on: Vec<String>,
    /// Object types the file declares (`None`: the file is the whole schema).
    pub kinds: Option<BTreeSet<String>>,
    /// Link types the file declares.
    pub links: Option<BTreeSet<String>>,
}

// ── names ────────────────────────────────────────────────────────────────

/// A name in comparable forms.
#[derive(Clone, Debug)]
struct Name {
    /// As written (for evidence).
    raw: String,
    /// Lowercase words joined by single spaces (Jaro-Winkler input).
    text: String,
    /// Stemmed words (token Jaccard input).
    tokens: BTreeSet<String>,
    /// Stemmed words joined by `_` (equality).
    key: String,
    /// Came from a glossary, not from the definition itself.
    synonym: bool,
}

/// Words of a name: split at non-alphanumerics and lower-to-upper camelCase
/// boundaries, lowercased.
fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// A light singular: `companies` → `company`, `tickets` → `ticket`.
fn stem(w: &str) -> String {
    if let Some(s) = w.strip_suffix("ies")
        && s.len() >= 2
    {
        return format!("{s}y");
    }
    if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
        return w[..w.len() - 1].to_string();
    }
    w.to_string()
}

fn name(raw: &str, synonym: bool) -> Name {
    let words = split_words(raw);
    let stems: Vec<String> = words.iter().map(|w| stem(w)).collect();
    Name {
        raw: raw.to_string(),
        text: words.join(" "),
        key: stems.join("_"),
        tokens: stems.into_iter().collect(),
        synonym,
    }
}

/// The comparable key of a property, state or link name.
fn key(raw: &str) -> String {
    split_words(raw).iter().map(|w| stem(w)).collect::<Vec<_>>().join("_")
}

fn jaro(a: &[char], b: &[char]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let range = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut a_hit = vec![false; a.len()];
    let mut b_hit = vec![false; b.len()];
    let mut matches = 0usize;
    for (i, ca) in a.iter().enumerate() {
        let lo = i.saturating_sub(range);
        let hi = (i + range + 1).min(b.len());
        for j in lo..hi {
            if !b_hit[j] && b[j] == *ca {
                a_hit[i] = true;
                b_hit[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let mut transpositions = 0usize;
    let mut k = 0usize;
    for (i, ca) in a.iter().enumerate() {
        if a_hit[i] {
            while !b_hit[k] {
                k += 1;
            }
            if *ca != b[k] {
                transpositions += 1;
            }
            k += 1;
        }
    }
    let m = matches as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64 / 2.0) / m) / 3.0
}

/// Jaro-Winkler similarity (prefix scale 0.1, prefix up to 4 characters).
pub fn jaro_winkler(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let j = jaro(&a, &b);
    let prefix = a.iter().zip(&b).take(4).take_while(|(x, y)| x == y).count();
    j + prefix as f64 * 0.1 * (1.0 - j)
}

fn jaccard<T: Ord>(a: &BTreeSet<T>, b: &BTreeSet<T>) -> f64 {
    let union = a.union(b).count();
    if union == 0 { 0.0 } else { a.intersection(b).count() as f64 / union as f64 }
}

/// Similarity of two names: the larger of Jaro-Winkler and token Jaccard.
fn name_sim(a: &Name, b: &Name) -> f64 {
    jaro_winkler(&a.text, &b.text).max(jaccard(&a.tokens, &b.tokens))
}

/// The best name comparison between two lists of names.
#[derive(Clone, Debug, Default)]
struct NameHit {
    value: f64,
    glossary: bool,
    a: String,
    b: String,
}

/// Own names that are the same word win (lexical 1.0); then a glossary
/// synonym on either side that is the same word as a name on the other
/// (1.0, marked `glossary`); then the best fuzzy similarity.
fn name_signal(a: &[Name], b: &[Name]) -> NameHit {
    let hit = |x: &Name, y: &Name, value: f64, glossary: bool| NameHit {
        value,
        glossary,
        a: x.raw.clone(),
        b: y.raw.clone(),
    };
    let same = |x: &Name, y: &Name| !x.key.is_empty() && x.key == y.key;
    for x in a.iter().filter(|x| !x.synonym) {
        if let Some(y) = b.iter().find(|y| !y.synonym && same(x, y)) {
            return hit(x, y, 1.0, false);
        }
    }
    for x in a {
        if let Some(y) = b.iter().find(|y| same(x, y) && (x.synonym || y.synonym)) {
            return hit(x, y, 1.0, true);
        }
    }
    let mut best = NameHit::default();
    for x in a {
        for y in b {
            let v = name_sim(x, y);
            if v > best.value {
                best = hit(x, y, v, false);
            }
        }
    }
    best
}

fn push_name(out: &mut Vec<Name>, raw: &str, synonym: bool) {
    if raw.is_empty() {
        return;
    }
    let n = name(raw, synonym);
    if !out.iter().any(|x| x.key == n.key && x.text == n.text) {
        out.push(n);
    }
}

/// Name, label and the glossary words for an object type or interface.
fn kind_names(schema: &Schema, kind: &str) -> Vec<Name> {
    let mut out = Vec::new();
    push_name(&mut out, kind, false);
    if let Some(t) = schema.object_types.get(kind) {
        push_name(&mut out, &t.label, false);
    }
    for (word, k) in &schema.glossary.kinds {
        if k == kind {
            push_name(&mut out, word, true);
        }
    }
    out
}

// ── features ─────────────────────────────────────────────────────────────

struct KindFeat {
    name: String,
    label: String,
    names: Vec<Name>,
    /// Property key → type (own, inherited and from interfaces).
    props: BTreeMap<String, PropType>,
    /// Name keys of each link type touching the kind.
    links: Vec<BTreeSet<String>>,
    /// Names of each ancestor and interface.
    parents: Vec<Vec<Name>>,
    states: BTreeSet<String>,
}

fn kind_feat(schema: &Schema, kind: &str) -> KindFeat {
    let ty = &schema.object_types[kind];
    let chain = schema.type_chain(kind);
    let mut props = BTreeMap::new();
    for t in &chain {
        for (p, d) in &t.properties {
            props.entry(key(p)).or_insert(d.ty);
        }
    }
    let mut parent_names: Vec<String> = chain.iter().skip(1).map(|t| t.name.clone()).collect();
    for t in &chain {
        for i in &t.implements {
            if let Some(iface) = schema.interfaces.get(i) {
                for (p, d) in &iface.properties {
                    props.entry(key(p)).or_insert(d.ty);
                }
            }
            if !parent_names.contains(i) {
                parent_names.push(i.clone());
            }
        }
    }
    parent_names.sort();
    let links = schema
        .link_types
        .values()
        .filter(|lt| lt.from.iter().chain(&lt.to).any(|x| schema.kind_is(kind, x)))
        .map(|lt| {
            let mut keys = BTreeSet::from([key(&lt.name)]);
            if let Some(inv) = &lt.inverse {
                keys.insert(key(inv));
            }
            if !lt.label.is_empty() {
                keys.insert(key(&lt.label));
            }
            keys
        })
        .collect();
    KindFeat {
        name: kind.to_string(),
        label: display(&ty.label, kind),
        names: kind_names(schema, kind),
        props,
        links,
        parents: parent_names.iter().map(|p| kind_names(schema, p)).collect(),
        states: ty.states.iter().map(|s| key(s)).collect(),
    }
}

fn display(label: &str, name: &str) -> String {
    if label.is_empty() { name.to_string() } else { label.to_string() }
}

struct LinkFeat {
    name: String,
    label: String,
    names: Vec<Name>,
    /// The inverse name and the glossary words for it.
    inverse: Vec<Name>,
    from: Vec<String>,
    to: Vec<String>,
}

fn link_feat(schema: &Schema, rel: &str) -> LinkFeat {
    let lt = &schema.link_types[rel];
    let mut names = Vec::new();
    push_name(&mut names, rel, false);
    push_name(&mut names, &lt.label, false);
    let mut inverse = Vec::new();
    if let Some(inv) = &lt.inverse {
        push_name(&mut inverse, inv, false);
    }
    for (word, target) in &schema.glossary.relations {
        match schema.resolve_rel(target) {
            Some((base, Dir::In)) if base == rel => push_name(&mut inverse, word, true),
            Some((base, _)) if base == rel => push_name(&mut names, word, true),
            _ => {}
        }
    }
    LinkFeat {
        name: rel.to_string(),
        label: display(&lt.label, rel),
        names,
        inverse,
        from: lt.from.clone(),
        to: lt.to.clone(),
    }
}

// ── scoring ──────────────────────────────────────────────────────────────

/// Everything behind one object-type score (numbers only; evidence text is
/// rendered for reported rows).
#[derive(Clone, Debug, Default)]
struct KindDetail {
    name: NameHit,
    /// (shared, union, types agreeing among shared)
    props: (usize, usize, usize),
    /// (matched, union)
    links: (usize, usize),
    /// (shared, union)
    states: (usize, usize),
    signals: Signals,
    score: f64,
    coverage: f64,
}

/// Greedy one-to-one matching of items whose key sets intersect:
/// `(matched, |a| + |b| - matched)`.
fn soft_match(a: &[BTreeSet<String>], b: &[BTreeSet<String>]) -> (usize, usize) {
    let mut used = vec![false; b.len()];
    let mut matched = 0;
    for x in a {
        if let Some(j) = (0..b.len()).find(|&j| !used[j] && !x.is_disjoint(&b[j])) {
            used[j] = true;
            matched += 1;
        }
    }
    (matched, a.len() + b.len() - matched)
}

fn parents_signal(a: &[Vec<Name>], b: &[Vec<Name>]) -> Option<f64> {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => None,
        (true, false) | (false, true) => Some(0.0),
        _ => {
            let best = |x: &[Name], ys: &[Vec<Name>]| ys.iter().map(|y| name_signal(x, y).value).fold(0.0, f64::max);
            let total: f64 = a.iter().map(|x| best(x, b)).sum::<f64>() + b.iter().map(|y| best(y, a)).sum::<f64>();
            Some(total / (a.len() + b.len()) as f64)
        }
    }
}

/// Weighted mean over the defined signals, and the share of weight defined.
fn fuse(parts: &[(f64, Option<f64>)]) -> (f64, f64) {
    let total: f64 = parts.iter().map(|(w, _)| w).sum();
    let (mut num, mut den) = (0.0, 0.0);
    for (w, v) in parts {
        if let Some(v) = v {
            num += w * v;
            den += w;
        }
    }
    if den == 0.0 { (0.0, 0.0) } else { (num / den, den / total) }
}

fn score_kind(s: &KindFeat, t: &KindFeat) -> KindDetail {
    let name = name_signal(&s.names, &t.names);
    let mut d = KindDetail { name: name.clone(), ..Default::default() };
    let (sp, tp): (BTreeSet<&String>, BTreeSet<&String>) = (s.props.keys().collect(), t.props.keys().collect());
    let (properties, property_jaccard) = if sp.is_empty() && tp.is_empty() {
        (None, None)
    } else {
        let shared: Vec<&String> = sp.intersection(&tp).copied().collect();
        let union = sp.union(&tp).count();
        let typed = shared.iter().filter(|p| s.props[**p] == t.props[**p]).count();
        d.props = (shared.len(), union, typed);
        let j = shared.len() as f64 / union as f64;
        let agree = if shared.is_empty() { 0.0 } else { typed as f64 / shared.len() as f64 };
        (Some(j * (0.8 + 0.2 * agree)), Some(j))
    };
    let links = if s.links.is_empty() && t.links.is_empty() {
        None
    } else {
        d.links = soft_match(&s.links, &t.links);
        Some(d.links.0 as f64 / d.links.1 as f64)
    };
    let parents = parents_signal(&s.parents, &t.parents);
    let states = if s.states.is_empty() && t.states.is_empty() {
        None
    } else {
        d.states = (s.states.intersection(&t.states).count(), s.states.union(&t.states).count());
        Some(d.states.0 as f64 / d.states.1 as f64)
    };
    let name_value = if name.glossary { 1.0 } else { name.value };
    let (score, coverage) = fuse(&[
        (W_NAME, Some(name_value)),
        (W_PROPERTIES, properties),
        (W_LINKS, links),
        (W_PARENTS, parents),
        (W_STATES, states),
    ]);
    d.signals = Signals {
        name: name_value,
        glossary: name.glossary,
        properties,
        property_jaccard,
        links,
        parents,
        states,
        endpoints: None,
    };
    d.score = score;
    d.coverage = coverage;
    d
}

#[derive(Clone, Debug, Default)]
struct LinkDetail {
    name: NameHit,
    /// Endpoint agreement of `from` and of `to`.
    ends: (Option<f64>, Option<f64>),
    signals: Signals,
    score: f64,
    coverage: f64,
}

/// Agreement of one end of a link type: source kinds mapped into the
/// target, compared with the target's kinds (a kind agrees with its
/// subtypes and supertypes). "Any" against specific kinds is half agreement;
/// source kinds nothing maps leave the end undefined.
fn end_agreement(src: &[String], tgt: &[String], map: &dyn Fn(&str) -> Option<String>, target: &Schema) -> Option<f64> {
    match (src.is_empty(), tgt.is_empty()) {
        (true, true) => None,
        (true, false) | (false, true) => Some(0.5),
        _ => {
            let mapped: BTreeSet<String> = src.iter().filter_map(|k| map(k)).collect();
            if mapped.is_empty() {
                return None;
            }
            let related = |a: &str, b: &str| target.kind_is(a, b) || target.kind_is(b, a);
            let hit_m = mapped.iter().filter(|m| tgt.iter().any(|t| related(m, t))).count();
            let hit_t = tgt.iter().filter(|t| mapped.iter().any(|m| related(m, t))).count();
            Some((hit_m + hit_t) as f64 / (mapped.len() + tgt.len()) as f64)
        }
    }
}

fn score_link(s: &LinkFeat, t: &LinkFeat, map: &dyn Fn(&str) -> Option<String>, target: &Schema) -> LinkDetail {
    let mut name = name_signal(&s.names, &t.names);
    // Inverse names count only when they are the same word: fuzzy scores of
    // short `x_by` / `x_of` names are mostly the shared suffix.
    if name.value < 1.0 && !s.inverse.is_empty() && !t.inverse.is_empty() {
        let inv = name_signal(&s.inverse, &t.inverse);
        if inv.value >= 1.0 {
            name = inv;
        }
    }
    let ends = (end_agreement(&s.from, &t.from, map, target), end_agreement(&s.to, &t.to, map, target));
    let endpoints = match ends {
        (Some(a), Some(b)) => Some((a + b) / 2.0),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    let name_value = if name.glossary { 1.0 } else { name.value };
    let (score, coverage) = fuse(&[(W_LINK_NAME, Some(name_value)), (W_LINK_ENDPOINTS, endpoints)]);
    LinkDetail {
        signals: Signals { name: name_value, glossary: name.glossary, endpoints, ..Default::default() },
        name,
        ends,
        score,
        coverage,
    }
}

// ── matching ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct Scored {
    s: usize,
    t: usize,
    score: f64,
}

/// Candidates above the floor, plus each side's best pair at any score.
struct Scores {
    pairs: Vec<Scored>,
    best_s: Vec<Option<Scored>>,
    best_t: Vec<Option<Scored>>,
}

fn better(a: &Scored, b: &Option<Scored>) -> bool {
    // Pairs are scored in (s, t) order, so `>` keeps the first of equals.
    b.is_none_or(|b| a.score > b.score)
}

fn collect_scores(n_s: usize, n_t: usize, min: f64, mut score: impl FnMut(usize, usize) -> f64) -> Scores {
    let mut out = Scores { pairs: Vec::new(), best_s: vec![None; n_s], best_t: vec![None; n_t] };
    for s in 0..n_s {
        for t in 0..n_t {
            let p = Scored { s, t, score: score(s, t) };
            if better(&p, &out.best_s[s]) {
                out.best_s[s] = Some(p);
            }
            if better(&p, &out.best_t[t]) {
                out.best_t[t] = Some(p);
            }
            if p.score >= min {
                out.pairs.push(p);
            }
        }
    }
    out.pairs.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.s.cmp(&b.s)).then(a.t.cmp(&b.t)));
    out
}

/// A pair chosen by greedy one-to-one matching, with its competitors.
struct Chosen {
    pair: Scored,
    tied: Vec<Scored>,
    alternatives: Vec<Scored>,
}

fn assign(sc: &Scores, n_s: usize, n_t: usize, eps: f64, alternatives: usize) -> Vec<Chosen> {
    let (mut s_used, mut t_used) = (vec![false; n_s], vec![false; n_t]);
    let mut by_s: Vec<Vec<usize>> = vec![Vec::new(); n_s];
    let mut by_t: Vec<Vec<usize>> = vec![Vec::new(); n_t];
    for (i, p) in sc.pairs.iter().enumerate() {
        by_s[p.s].push(i);
        by_t[p.t].push(i);
    }
    let mut out = Vec::new();
    for (i, p) in sc.pairs.iter().enumerate() {
        if s_used[p.s] || t_used[p.t] {
            continue;
        }
        s_used[p.s] = true;
        t_used[p.t] = true;
        let mut tied: Vec<usize> = by_s[p.s]
            .iter()
            .chain(&by_t[p.t])
            .copied()
            .filter(|&j| j != i && (sc.pairs[j].score - p.score).abs() <= eps)
            .collect();
        tied.sort();
        tied.dedup();
        let alts = by_s[p.s].iter().copied().filter(|&j| j != i).take(alternatives).map(|j| sc.pairs[j]).collect();
        out.push(Chosen { pair: *p, tied: tied.into_iter().map(|j| sc.pairs[j]).collect(), alternatives: alts });
    }
    out
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Accepted needs [`ACCEPT_MIN`], no tie, and more than a fuzzy name: a
/// row scored on names alone is accepted only when they are the same word.
fn status(score: f64, tied: bool, sig: &Signals) -> MatchStatus {
    if score >= ACCEPT_MIN && !tied && !fuzzy_name_only(sig) { MatchStatus::Accepted } else { MatchStatus::Borderline }
}

fn structural(sig: &Signals) -> bool {
    [sig.properties, sig.links, sig.parents, sig.states, sig.endpoints].iter().any(Option::is_some)
}

fn fuzzy_name_only(sig: &Signals) -> bool {
    !structural(sig) && sig.name < 1.0
}

/// `semapv:LexicalMatching` when only the name was compared, else
/// `semapv:CompositeMatching`.
fn justification(sig: &Signals) -> String {
    if structural(sig) { "semapv:CompositeMatching".into() } else { "semapv:LexicalMatching".into() }
}

fn status_sentence(score: f64, tied: &[Candidate], eps: f64, sig: &Signals) -> String {
    if !tied.is_empty() {
        let names: Vec<String> =
            tied.iter().map(|c| format!("{} -> {} ({:.3})", c.source, c.target, c.score)).collect();
        format!("borderline: tied within {eps} with {}", names.join(", "))
    } else if score < ACCEPT_MIN {
        format!("borderline: score {score:.3} is below {ACCEPT_MIN}")
    } else if fuzzy_name_only(sig) {
        "borderline: scored on a similar name alone, with no structure to compare".into()
    } else {
        format!("accepted: score {score:.3} >= {ACCEPT_MIN} and no competitor within {eps}")
    }
}

fn name_sentence(hit: &NameHit) -> String {
    if hit.glossary {
        format!("name: '{}' and '{}' are the same word through a glossary (1.00)", hit.a, hit.b)
    } else {
        format!("name: '{}' vs '{}' (similarity {:.2})", hit.a, hit.b, hit.value)
    }
}

fn kind_evidence(d: &KindDetail, s: &KindFeat, t: &KindFeat) -> Vec<String> {
    let mut ev = vec![name_sentence(&d.name)];
    ev.push(match d.signals.properties {
        None => "properties: not compared (neither declares one)".into(),
        Some(v) => format!(
            "properties: {} of {} names shared (jaccard {:.2}), types agree on {} of {} ({v:.2})",
            d.props.0,
            d.props.1,
            d.signals.property_jaccard.unwrap_or(0.0),
            d.props.2,
            d.props.0
        ),
    });
    ev.push(match d.signals.links {
        None => "links: not compared (no link type touches either)".into(),
        Some(v) => format!("links: {} of {} link types correspond by name ({v:.2})", d.links.0, d.links.1),
    });
    let parents = |f: &KindFeat| f.parents.iter().map(|p| p[0].raw.clone()).collect::<Vec<_>>().join(", ");
    ev.push(match d.signals.parents {
        None => "parents: not compared (neither extends or implements anything)".into(),
        Some(v) => format!("parents: [{}] vs [{}] ({v:.2})", parents(s), parents(t)),
    });
    ev.push(match d.signals.states {
        None => "states: not compared (neither declares states)".into(),
        Some(v) => format!("states: {} of {} shared ({v:.2})", d.states.0, d.states.1),
    });
    ev
}

fn kind_predicate(score: f64, sig: &Signals) -> (MatchPredicate, String) {
    let structure = sig.property_jaccard.unwrap_or(0.0).max(sig.links.unwrap_or(0.0));
    if score >= EXACT_MIN && structure >= STRUCTURE_MIN {
        (
            MatchPredicate::ExactMatch,
            format!(
                "exact_match: score {score:.3} >= {EXACT_MIN} and property or link jaccard {structure:.2} >= {STRUCTURE_MIN}"
            ),
        )
    } else if score >= CLOSE_MIN {
        let why = if score >= EXACT_MIN && sig.property_jaccard.is_none() && sig.links.is_none() {
            format!("close_match: score {score:.3} without properties or link types to compare")
        } else if score >= EXACT_MIN {
            format!("close_match: score {score:.3} but property and link jaccard {structure:.2} < {STRUCTURE_MIN}")
        } else {
            format!("close_match: score {score:.3} >= {CLOSE_MIN}")
        };
        (MatchPredicate::CloseMatch, why)
    } else {
        (MatchPredicate::RelatedMatch, format!("related_match: score {score:.3} < {CLOSE_MIN}"))
    }
}

fn link_predicate(score: f64, sig: &Signals) -> (MatchPredicate, String) {
    let ends = sig.endpoints.unwrap_or(0.0);
    if score >= EXACT_MIN && sig.endpoints.is_some() && ends >= STRUCTURE_MIN {
        (
            MatchPredicate::ExactMatch,
            format!("exact_match: score {score:.3} >= {EXACT_MIN} and endpoint agreement {ends:.2} >= {STRUCTURE_MIN}"),
        )
    } else if score >= CLOSE_MIN {
        let why = if score >= EXACT_MIN {
            match sig.endpoints {
                None => format!("close_match: score {score:.3} on names alone (endpoints could not be compared)"),
                Some(e) => format!("close_match: score {score:.3} but endpoint agreement {e:.2} < {STRUCTURE_MIN}"),
            }
        } else {
            format!("close_match: score {score:.3} >= {CLOSE_MIN}")
        };
        (MatchPredicate::CloseMatch, why)
    } else {
        (MatchPredicate::RelatedMatch, format!("related_match: score {score:.3} < {CLOSE_MIN}"))
    }
}

fn candidates(pairs: &[Scored], s_names: &[String], t_names: &[String]) -> Vec<Candidate> {
    pairs
        .iter()
        .map(|p| Candidate { source: s_names[p.s].clone(), target: t_names[p.t].clone(), score: round3(p.score) })
        .collect()
}

// ── align ────────────────────────────────────────────────────────────────

/// Align `source` onto `target`: scored object-type and link-type
/// correspondences, unmatched definitions, SSSOM TSV and a glossary change
/// set for the target. Pure: neither schema is changed.
pub fn align(source: &Schema, target: &Schema, opts: &AlignOptions) -> AlignReport {
    let eps = opts.tie_epsilon.max(0.0);
    let in_scope = |list: &[String], name: &str| list.is_empty() || list.iter().any(|n| n == name);
    let s_kinds: Vec<String> =
        source.object_types.keys().filter(|k| in_scope(&opts.source_kinds, k)).cloned().collect();
    let t_kinds: Vec<String> = target.object_types.keys().cloned().collect();
    let sf: Vec<KindFeat> = s_kinds.iter().map(|k| kind_feat(source, k)).collect();
    let tf: Vec<KindFeat> = t_kinds.iter().map(|k| kind_feat(target, k)).collect();

    // Object types.
    let ks = collect_scores(sf.len(), tf.len(), opts.min_score, |s, t| score_kind(&sf[s], &tf[t]).score);
    let chosen = assign(&ks, sf.len(), tf.len(), eps, opts.alternatives);
    let primary_of_s: BTreeMap<usize, usize> = chosen.iter().map(|c| (c.pair.s, c.pair.t)).collect();
    let primary_of_t: BTreeMap<usize, usize> = chosen.iter().map(|c| (c.pair.t, c.pair.s)).collect();
    let mut kinds = Vec::new();
    let kind_row = |pair: Scored, tied: &[Scored], alts: &[Scored], hierarchy: Option<(MatchPredicate, String)>| {
        let d = score_kind(&sf[pair.s], &tf[pair.t]);
        // Thresholds apply to the reported (rounded) numbers, so the words
        // and the numbers in a row never disagree.
        let (score, signals) = (round3(pair.score), round_signals(d.signals.clone()));
        let tied = candidates(tied, &s_kinds, &t_kinds);
        let (predicate, why) = hierarchy.unwrap_or_else(|| kind_predicate(score, &signals));
        let mut evidence = kind_evidence(&d, &sf[pair.s], &tf[pair.t]);
        evidence.push(why);
        evidence.push(status_sentence(score, &tied, eps, &signals));
        Match {
            source: sf[pair.s].name.clone(),
            source_label: sf[pair.s].label.clone(),
            target: tf[pair.t].name.clone(),
            target_label: tf[pair.t].label.clone(),
            predicate,
            status: status(score, !tied.is_empty(), &signals),
            score,
            evidence_coverage: round3(d.coverage),
            justification: justification(&signals),
            signals,
            evidence,
            tied_with: tied,
            alternatives: candidates(alts, &s_kinds, &t_kinds),
        }
    };
    for c in &chosen {
        kinds.push(kind_row(c.pair, &c.tied, &c.alternatives, None));
    }
    // Hierarchy fallback ("the best target is the parent or child of the
    // matched one"): an unmatched source whose best target went to one of
    // its ancestors is narrower than that target (broad_match), to one of
    // its descendants broader (narrow_match); symmetrically for an unmatched
    // target whose best source is matched to its parent or child.
    let ties_of = |p: &Scored| -> Vec<Scored> {
        ks.pairs
            .iter()
            .filter(|q| (q.s == p.s || q.t == p.t) && !(q.s == p.s && q.t == p.t) && (q.score - p.score).abs() <= eps)
            .copied()
            .collect()
    };
    let alts_of = |p: &Scored| -> Vec<Scored> {
        ks.pairs.iter().filter(|q| q.s == p.s && q.t != p.t).take(opts.alternatives).copied().collect()
    };
    for (s, best) in ks.best_s.iter().enumerate() {
        let Some(best) = best.filter(|b| b.score >= opts.min_score) else { continue };
        if primary_of_s.contains_key(&s) {
            continue;
        }
        let Some(&owner) = primary_of_t.get(&best.t) else { continue };
        let (sname, owner_name, tname) = (&s_kinds[s], &s_kinds[owner], &t_kinds[best.t]);
        let hierarchy = if source.kind_is(sname, owner_name) {
            Some((
                MatchPredicate::BroadMatch,
                format!(
                    "broad_match: {tname} is matched to {owner_name}, an ancestor of {sname}, so {tname} is broader"
                ),
            ))
        } else if source.kind_is(owner_name, sname) {
            Some((
                MatchPredicate::NarrowMatch,
                format!(
                    "narrow_match: {tname} is matched to {owner_name}, a descendant of {sname}, so {tname} is narrower"
                ),
            ))
        } else {
            None
        };
        if let Some(h) = hierarchy {
            kinds.push(kind_row(best, &ties_of(&best), &alts_of(&best), Some(h)));
        }
    }
    for (t, best) in ks.best_t.iter().enumerate() {
        let Some(best) = best.filter(|b| b.score >= opts.min_score) else { continue };
        if primary_of_t.contains_key(&t) {
            continue;
        }
        let Some(&matched) = primary_of_s.get(&best.s) else { continue };
        let (tname, other, sname) = (&t_kinds[t], &t_kinds[matched], &s_kinds[best.s]);
        let hierarchy = if target.kind_is(tname, other) {
            Some((
                MatchPredicate::NarrowMatch,
                format!("narrow_match: {sname} is matched to {other} and {tname} extends it, so {tname} is narrower"),
            ))
        } else if target.kind_is(other, tname) {
            Some((
                MatchPredicate::BroadMatch,
                format!("broad_match: {sname} is matched to {other}, a descendant of {tname}, so {tname} is broader"),
            ))
        } else {
            None
        };
        if let Some(h) = hierarchy {
            kinds.push(kind_row(best, &ties_of(&best), &alts_of(&best), Some(h)));
        }
    }
    kinds.sort_by(|a, b| a.source.cmp(&b.source).then(a.target.cmp(&b.target)));

    // Link types: endpoints are mapped through the accepted kind matches;
    // definitions outside the aligned set (interfaces, the layers a pack
    // sits on) map to the target definition of the same name.
    let kind_map: BTreeMap<String, String> = kinds
        .iter()
        .filter(|m| {
            m.status == MatchStatus::Accepted
                && matches!(m.predicate, MatchPredicate::ExactMatch | MatchPredicate::CloseMatch)
        })
        .map(|m| (m.source.clone(), m.target.clone()))
        .collect();
    let aligned: BTreeSet<&str> = s_kinds.iter().map(String::as_str).collect();
    let map = |k: &str| -> Option<String> {
        if let Some(t) = kind_map.get(k) {
            return Some(t.clone());
        }
        (!aligned.contains(k) && target.is_type_or_interface(k)).then(|| k.to_string())
    };
    let s_links: Vec<String> = source.link_types.keys().filter(|k| in_scope(&opts.source_links, k)).cloned().collect();
    let t_links: Vec<String> = target.link_types.keys().cloned().collect();
    let slf: Vec<LinkFeat> = s_links.iter().map(|k| link_feat(source, k)).collect();
    let tlf: Vec<LinkFeat> = t_links.iter().map(|k| link_feat(target, k)).collect();
    let ls =
        collect_scores(slf.len(), tlf.len(), opts.min_score, |s, t| score_link(&slf[s], &tlf[t], &map, target).score);
    let lchosen = assign(&ls, slf.len(), tlf.len(), eps, opts.alternatives);
    let mut links: Vec<Match> = lchosen
        .iter()
        .map(|c| {
            let d = score_link(&slf[c.pair.s], &tlf[c.pair.t], &map, target);
            let (score, signals) = (round3(c.pair.score), round_signals(d.signals.clone()));
            let tied = candidates(&c.tied, &s_links, &t_links);
            let (predicate, why) = link_predicate(score, &signals);
            let (s, t) = (&slf[c.pair.s], &tlf[c.pair.t]);
            let kinds = |k: &[String]| if k.is_empty() { "any".to_string() } else { format!("[{}]", k.join(", ")) };
            let end = |v: Option<f64>, a: &[String], b: &[String]| {
                let mapped = a.iter().filter(|k| map(k).is_some()).count();
                match v {
                    None if !a.is_empty() && mapped == 0 => format!("not compared (no kind of {} is mapped)", kinds(a)),
                    None => "not compared".to_string(),
                    Some(v) if mapped < a.len() && mapped > 0 => {
                        format!("{} vs {} ({v:.2}; {mapped} of {} source kinds mapped)", kinds(a), kinds(b), a.len())
                    }
                    Some(v) => format!("{} vs {} ({v:.2})", kinds(a), kinds(b)),
                }
            };
            let evidence = vec![
                name_sentence(&d.name),
                format!("from: {}", end(d.ends.0, &s.from, &t.from)),
                format!("to: {}", end(d.ends.1, &s.to, &t.to)),
                why,
                status_sentence(score, &tied, eps, &signals),
            ];
            Match {
                source: s.name.clone(),
                source_label: s.label.clone(),
                target: t.name.clone(),
                target_label: t.label.clone(),
                predicate,
                status: status(score, !tied.is_empty(), &signals),
                score,
                evidence_coverage: round3(d.coverage),
                justification: justification(&signals),
                signals,
                evidence,
                tied_with: tied,
                alternatives: candidates(&c.alternatives, &s_links, &t_links),
            }
        })
        .collect();
    links.sort_by(|a, b| a.source.cmp(&b.source).then(a.target.cmp(&b.target)));

    // Unmatched definitions, with their nearest counterpart.
    let unmatched = |section: &str,
                     names: &[String],
                     labels: &dyn Fn(usize) -> String,
                     covered: &BTreeSet<&str>,
                     best: &[Option<Scored>],
                     source_side: bool,
                     other: &[String]| {
        names
            .iter()
            .enumerate()
            .filter(|(_, n)| !covered.contains(n.as_str()))
            .map(|(i, n)| Unmatched {
                section: section.to_string(),
                name: n.clone(),
                label: labels(i),
                nearest: best[i].map(|p| {
                    let (s, t) =
                        if source_side { (n.clone(), other[p.t].clone()) } else { (other[p.s].clone(), n.clone()) };
                    Candidate { source: s, target: t, score: round3(p.score) }
                }),
            })
            .collect::<Vec<_>>()
    };
    let covered_s: BTreeSet<&str> = kinds.iter().map(|m| m.source.as_str()).collect();
    let covered_t: BTreeSet<&str> = kinds.iter().map(|m| m.target.as_str()).collect();
    let covered_ls: BTreeSet<&str> = links.iter().map(|m| m.source.as_str()).collect();
    let covered_lt: BTreeSet<&str> = links.iter().map(|m| m.target.as_str()).collect();
    let mut unmatched_source =
        unmatched("object_types", &s_kinds, &|i| sf[i].label.clone(), &covered_s, &ks.best_s, true, &t_kinds);
    unmatched_source.extend(unmatched(
        "link_types",
        &s_links,
        &|i| slf[i].label.clone(),
        &covered_ls,
        &ls.best_s,
        true,
        &t_links,
    ));
    let mut unmatched_target =
        unmatched("object_types", &t_kinds, &|i| tf[i].label.clone(), &covered_t, &ks.best_t, false, &s_kinds);
    unmatched_target.extend(unmatched(
        "link_types",
        &t_links,
        &|i| tlf[i].label.clone(),
        &covered_lt,
        &ls.best_t,
        false,
        &s_links,
    ));

    let (change_set, warnings) = glossary_changes(source, target, &kinds, &links);
    let all = kinds.iter().chain(&links);
    let counts = AlignCounts {
        accepted: all.clone().filter(|m| m.status == MatchStatus::Accepted).count(),
        borderline: all.filter(|m| m.status == MatchStatus::Borderline).count(),
        unmatched_source: unmatched_source.len(),
        unmatched_target: unmatched_target.len(),
    };
    let sssom_tsv = sssom(source, target, &align_rows(&kinds, &links));
    AlignReport {
        source_schema: source.name.clone(),
        target_schema: target.name.clone(),
        kinds,
        links,
        unmatched_source,
        unmatched_target,
        counts,
        thresholds: Thresholds {
            exact: EXACT_MIN,
            structure: STRUCTURE_MIN,
            close: CLOSE_MIN,
            accept: ACCEPT_MIN,
            min_score: opts.min_score,
            tie_epsilon: eps,
        },
        sssom_tsv,
        change_set,
        warnings,
        means: MEANS.into(),
    }
}

fn round_signals(s: Signals) -> Signals {
    let r = |v: Option<f64>| v.map(round3);
    Signals {
        name: round3(s.name),
        glossary: s.glossary,
        properties: r(s.properties),
        property_jaccard: r(s.property_jaccard),
        links: r(s.links),
        parents: r(s.parents),
        states: r(s.states),
        endpoints: r(s.endpoints),
    }
}

/// Glossary terms for accepted exact and close matches whose names differ:
/// people who say the source word mean the target definition. Words the
/// target already defines, or already maps elsewhere, are not proposed.
fn glossary_changes(
    source: &Schema,
    target: &Schema,
    kinds: &[Match],
    links: &[Match],
) -> (SchemaChangeSet, Vec<String>) {
    let usable = |m: &&Match| {
        m.status == MatchStatus::Accepted
            && matches!(m.predicate, MatchPredicate::ExactMatch | MatchPredicate::CloseMatch)
            && m.source != m.target
    };
    let mut changes = Vec::new();
    let mut warnings = Vec::new();
    for m in kinds.iter().filter(usable) {
        let word = m.source.to_lowercase();
        if target.is_type_or_interface(&word) {
            warnings.push(format!(
                "{word} is already a kind of {}: a glossary term {word} -> {} would be shadowed, not proposed",
                target.name, m.target
            ));
        } else {
            match target.glossary.kinds.get(&word) {
                Some(k) if *k == m.target => {}
                Some(k) => warnings.push(format!(
                    "the glossary of {} already maps {word} to {k}; {word} -> {} not proposed",
                    target.name, m.target
                )),
                None => changes.push(SchemaChange::PutGlossaryTerm {
                    section: GlossarySection::Kinds,
                    word,
                    target: m.target.clone(),
                }),
            }
        }
    }
    for m in links.iter().filter(usable) {
        let word = m.source.to_lowercase();
        if target.resolve_rel(&word).is_some() {
            warnings.push(format!(
                "{word} is already a relation of {}: a glossary term {word} -> {} would be shadowed, not proposed",
                target.name, m.target
            ));
        } else {
            match target.glossary.relations.get(&word) {
                Some(r) if target.resolve_rel(r).is_some_and(|(base, _)| base == m.target) => {}
                Some(r) => warnings.push(format!(
                    "the glossary of {} already maps {word} to {r}; {word} -> {} not proposed",
                    target.name, m.target
                )),
                None => changes.push(SchemaChange::PutGlossaryTerm {
                    section: GlossarySection::Relations,
                    word,
                    target: m.target.clone(),
                }),
            }
        }
    }
    let message = if changes.is_empty() {
        String::new()
    } else {
        format!(
            "glossary terms from aligning {} onto {} ({} accepted matches)",
            source.name,
            target.name,
            changes.len()
        )
    };
    (SchemaChangeSet { changes, message, ..Default::default() }, warnings)
}

// ── certification ────────────────────────────────────────────────────────

/// Ancestors of an object type (extends chain and the interfaces of every
/// type on it); an interface has none. `None` for an unknown term.
fn ancestors(schema: &Schema, term: &str) -> Option<BTreeSet<String>> {
    if schema.is_interface(term) {
        return Some(BTreeSet::new());
    }
    schema.object_types.get(term)?;
    let mut out = BTreeSet::new();
    for (i, t) in schema.type_chain(term).into_iter().enumerate() {
        if i > 0 {
            out.insert(t.name.clone());
        }
        out.extend(t.implements.iter().cloned());
    }
    out.remove(term);
    Some(out)
}

/// How the carried source ancestors (T) relate to the comparable target ancestors (O).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Position {
    Same,
    /// T ⊋ O: the source sits deeper, so the target is broader.
    SourceDeeper,
    /// O ⊋ T: the target sits deeper, so the target is narrower.
    TargetDeeper,
    Incomparable,
}

fn list(set: &BTreeSet<String>) -> String {
    format!("[{}]", set.iter().cloned().collect::<Vec<_>>().join(", "))
}

/// Check each claimed mapping against the two hierarchies, translating the
/// source term's ancestors through the mapping itself (exact and close rows;
/// an exact row wins over a close one for the same source term, otherwise
/// the first row does). A claimed `exact_match` is kept only when the
/// carried ancestors equal the target's comparable ancestors; otherwise it
/// becomes `broad_match` (the source sits deeper), `narrow_match` (the
/// target does) or `close_match` (neither contains the other). A claimed
/// `broad_match` or `narrow_match` contradicted by the hierarchies becomes
/// `close_match`; `close_match` and `related_match` claim no position and are
/// kept. A row with nothing to compare is `uncertified` and keeps its claim.
pub fn certify(source: &Schema, target: &Schema, mappings: &[Mapping]) -> Vec<CertifiedMapping> {
    let mut fwd: BTreeMap<&str, (&str, MatchPredicate)> = BTreeMap::new();
    for m in mappings {
        if !matches!(m.predicate, MatchPredicate::ExactMatch | MatchPredicate::CloseMatch) {
            continue;
        }
        match fwd.get(m.source.as_str()) {
            Some((_, MatchPredicate::ExactMatch)) => {}
            Some(_) if m.predicate == MatchPredicate::CloseMatch => {}
            _ => {
                fwd.insert(m.source.as_str(), (m.target.as_str(), m.predicate));
            }
        }
    }
    let images: BTreeSet<String> = fwd.values().map(|(t, _)| t.to_string()).collect();
    mappings.iter().map(|m| certify_one(source, target, m, &fwd, &images)).collect()
}

fn certify_one(
    source: &Schema,
    target: &Schema,
    m: &Mapping,
    fwd: &BTreeMap<&str, (&str, MatchPredicate)>,
    images: &BTreeSet<String>,
) -> CertifiedMapping {
    let mut row = CertifiedMapping {
        source: m.source.clone(),
        target: m.target.clone(),
        claimed: m.predicate,
        predicate: m.predicate,
        certification: Certification::Uncertified,
        reason: String::new(),
        source_ancestors: Vec::new(),
        target_ancestors: Vec::new(),
        carried: Vec::new(),
        comparable: Vec::new(),
        gaps: Vec::new(),
    };
    let unknown = |schema: &Schema, term: &str, side: &str| {
        if schema.link_types.contains_key(term) {
            format!("{term} is a link type of the {side} schema: link types have no hierarchy to compare")
        } else {
            format!("{term} is not an object type or interface of the {side} schema")
        }
    };
    let (Some(s_anc), Some(o_anc)) = (ancestors(source, &m.source), ancestors(target, &m.target)) else {
        row.reason = if ancestors(source, &m.source).is_none() {
            unknown(source, &m.source, "source")
        } else {
            unknown(target, &m.target, "target")
        };
        return row;
    };
    row.source_ancestors = s_anc.iter().cloned().collect();
    row.target_ancestors = o_anc.iter().cloned().collect();
    let mut carried = BTreeSet::new();
    for c in &s_anc {
        match fwd.get(c.as_str()) {
            Some((t, _)) => {
                carried.insert(t.to_string());
            }
            None => row.gaps.push(Gap { side: Side::Source, ancestor: c.clone() }),
        }
    }
    let comparable: BTreeSet<String> = o_anc.iter().filter(|a| images.contains(*a)).cloned().collect();
    for a in o_anc.iter().filter(|a| !images.contains(*a)) {
        row.gaps.push(Gap { side: Side::Target, ancestor: a.clone() });
    }
    row.carried = carried.iter().cloned().collect();
    row.comparable = comparable.iter().cloned().collect();
    if carried.is_empty() && comparable.is_empty() {
        row.reason = if s_anc.is_empty() && o_anc.is_empty() {
            "nothing to compare: neither side has an ancestor".into()
        } else {
            format!(
                "nothing to compare: no row maps a source ancestor {} or onto a target ancestor {}",
                list(&s_anc),
                list(&o_anc)
            )
        };
        return row;
    }
    let position = if carried == comparable {
        Position::Same
    } else if carried.is_superset(&comparable) {
        Position::SourceDeeper
    } else if comparable.is_superset(&carried) {
        Position::TargetDeeper
    } else {
        Position::Incomparable
    };
    let sets =
        format!("carried source ancestors {} vs comparable target ancestors {}", list(&carried), list(&comparable));
    let (kept, why) = match (m.predicate, position) {
        (MatchPredicate::ExactMatch, Position::Same) => (m.predicate, format!("exact_match upheld: {sets} are equal")),
        (MatchPredicate::ExactMatch, Position::SourceDeeper) => (
            MatchPredicate::BroadMatch,
            format!("exact_match refuted: {sets}; the source sits deeper, so {} is broader", m.target),
        ),
        (MatchPredicate::ExactMatch, Position::TargetDeeper) => (
            MatchPredicate::NarrowMatch,
            format!("exact_match refuted: {sets}; the target sits deeper, so {} is narrower", m.target),
        ),
        (MatchPredicate::ExactMatch, Position::Incomparable) => {
            (MatchPredicate::CloseMatch, format!("exact_match refuted: {sets}; neither contains the other"))
        }
        (MatchPredicate::BroadMatch, Position::Same | Position::SourceDeeper)
        | (MatchPredicate::NarrowMatch, Position::Same | Position::TargetDeeper) => {
            (m.predicate, format!("{} not refuted: {sets}", m.predicate.as_str()))
        }
        (MatchPredicate::BroadMatch | MatchPredicate::NarrowMatch, _) => (
            MatchPredicate::CloseMatch,
            format!("{} refuted: {sets} put the target on the other side or apart", m.predicate.as_str()),
        ),
        (MatchPredicate::CloseMatch | MatchPredicate::RelatedMatch, _) => {
            (m.predicate, format!("{} claims no position: kept ({sets})", m.predicate.as_str()))
        }
    };
    row.predicate = kept;
    row.certification = if kept != m.predicate {
        Certification::Downgraded
    } else if row.gaps.is_empty() {
        Certification::Upheld
    } else {
        Certification::UpheldWithGaps
    };
    row.reason = if row.certification == Certification::UpheldWithGaps {
        let names: Vec<&str> = row.gaps.iter().map(|g| g.ancestor.as_str()).collect();
        format!("{why}; the mapping does not carry {}", names.join(", "))
    } else {
        why
    };
    row
}

/// [`certify`] with counts, the definitions no row names, SSSOM and `means`.
/// `source_scope` limits `unmapped_source` (empty: every source object type).
pub fn certify_report(
    source: &Schema,
    target: &Schema,
    mappings: &[Mapping],
    source_scope: &[String],
) -> CertifyReport {
    let rows = certify(source, target, mappings);
    let mut counts = CertifyCounts::default();
    for r in &rows {
        match r.certification {
            Certification::Upheld => counts.upheld += 1,
            Certification::UpheldWithGaps => counts.upheld_with_gaps += 1,
            Certification::Downgraded => counts.downgraded += 1,
            Certification::Uncertified => counts.uncertified += 1,
        }
    }
    let named_s: BTreeSet<&str> = mappings.iter().map(|m| m.source.as_str()).collect();
    let named_t: BTreeSet<&str> = mappings.iter().map(|m| m.target.as_str()).collect();
    let unmapped_source = source
        .object_types
        .keys()
        .filter(|k| source_scope.is_empty() || source_scope.contains(k))
        .filter(|k| !named_s.contains(k.as_str()))
        .cloned()
        .collect();
    let unmapped_target = target.object_types.keys().filter(|k| !named_t.contains(k.as_str())).cloned().collect();
    let label = |schema: &Schema, n: &str| schema.object_types.get(n).map(|t| display(&t.label, n)).unwrap_or(n.into());
    let sssom_rows: Vec<SssomRow> = rows
        .iter()
        .map(|r| SssomRow {
            subject: format!("src:class/{}", curie_local(&r.source)),
            subject_label: label(source, &r.source),
            predicate: r.predicate,
            object: format!("tgt:class/{}", curie_local(&r.target)),
            object_label: label(target, &r.target),
            justification: "semapv:MappingReview".into(),
            confidence: None,
            comment: format!("{}; claimed {}: {}", r.certification.as_str(), r.claimed.curie(), r.reason),
        })
        .collect();
    CertifyReport {
        rows,
        counts,
        unmapped_source,
        unmapped_target,
        sssom_tsv: sssom(source, target, &sssom_rows),
        means: CERTIFY_MEANS.into(),
    }
}

// ── SSSOM ────────────────────────────────────────────────────────────────

struct SssomRow {
    subject: String,
    subject_label: String,
    predicate: MatchPredicate,
    object: String,
    object_label: String,
    justification: String,
    confidence: Option<f64>,
    comment: String,
}

/// The SSSOM column header.
pub const SSSOM_COLUMNS: &str = "subject_id\tsubject_label\tpredicate_id\tobject_id\tobject_label\t\
                                 mapping_justification\tconfidence\tcomment";

fn align_rows(kinds: &[Match], links: &[Match]) -> Vec<SssomRow> {
    let row = |space: &str, m: &Match| SssomRow {
        subject: format!("src:{space}/{}", curie_local(&m.source)),
        subject_label: m.source_label.clone(),
        predicate: m.predicate,
        object: format!("tgt:{space}/{}", curie_local(&m.target)),
        object_label: m.target_label.clone(),
        justification: m.justification.clone(),
        confidence: Some(m.score),
        comment: format!(
            "{}; {}",
            if m.status == MatchStatus::Accepted { "accepted" } else { "borderline" },
            m.evidence.join("; ")
        ),
    };
    kinds.iter().map(|m| row("class", m)).chain(links.iter().map(|m| row("relation", m))).collect()
}

fn curie_local(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// A TSV cell: TAB, LF and CR become spaces.
fn cell(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// The `src:` / `tgt:` expansion: the schema's name, plus the side when both
/// schemas share a name (so the two prefixes never expand alike).
fn set_base(name: &str, other: &str, side: &str) -> String {
    let n = curie_local(name);
    if name == other { format!("{BASE}schema/{n}/{side}/") } else { format!("{BASE}schema/{n}/") }
}

fn sssom(source: &Schema, target: &Schema, rows: &[SssomRow]) -> String {
    let mut out = String::new();
    out.push_str("#curie_map:\n");
    let _ = writeln!(out, "#  skos: {SKOS}");
    let _ = writeln!(out, "#  semapv: {SEMAPV}");
    let _ = writeln!(out, "#  src: {}", set_base(&source.name, &target.name, "source"));
    let _ = writeln!(out, "#  tgt: {}", set_base(&target.name, &source.name, "target"));
    let _ = writeln!(out, "#mapping_set_id: {BASE}align/{}/{}", curie_local(&source.name), curie_local(&target.name));
    out.push_str("#license: https://w3id.org/sssom/license/unspecified\n");
    let _ = writeln!(out, "#mapping_tool: onto-intelli {}", env!("CARGO_PKG_VERSION"));
    out.push_str(SSSOM_COLUMNS);
    out.push('\n');
    for r in rows {
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            r.subject,
            cell(&r.subject_label),
            r.predicate.curie(),
            r.object,
            cell(&r.object_label),
            r.justification,
            r.confidence.map(|c| format!("{c:.3}")).unwrap_or_default(),
            cell(&r.comment)
        );
    }
    out
}

/// The local name of an SSSOM id: `src:class/issue`, `<https://…/class/issue>`,
/// `issue` → `issue` (percent escapes decoded).
fn sssom_term(id: &str) -> String {
    let t = id.trim().trim_start_matches('<').trim_end_matches('>');
    let local = t.rsplit(['/', '#']).next().unwrap_or(t);
    let local = if local.contains(':') && !t.contains('/') { local.rsplit(':').next().unwrap_or(local) } else { local };
    let bytes = local.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(h) = local.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(h);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read the mapping rows of an SSSOM TSV (metadata lines start with `#`).
/// The header must name `subject_id`, `predicate_id` and `object_id`; a row
/// missing one of them, or with a predicate that is not a SKOS mapping
/// predicate, is refused with its line number rather than skipped.
pub fn parse_sssom(text: &str) -> Result<Vec<Mapping>> {
    let mut header: Option<(usize, usize, usize)> = None;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cells: Vec<&str> = line.split('\t').collect();
        let Some((s, p, o)) = header else {
            let col = |name: &str| cells.iter().position(|c| c.trim() == name);
            match (col("subject_id"), col("predicate_id"), col("object_id")) {
                (Some(s), Some(p), Some(o)) => header = Some((s, p, o)),
                _ => {
                    return Err(Error::invalid(format!(
                        "sssom line {n}: the header must name subject_id, predicate_id and object_id"
                    )));
                }
            }
            continue;
        };
        let get = |c: usize, what: &str| -> Result<&str> {
            cells
                .get(c)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| Error::invalid(format!("sssom line {n}: missing {what}")))
        };
        let (subject, predicate, object) = (get(s, "subject_id")?, get(p, "predicate_id")?, get(o, "object_id")?);
        let Some(predicate) = MatchPredicate::parse(predicate) else {
            return Err(Error::invalid(format!(
                "sssom line {n}: predicate {predicate} is not a SKOS mapping predicate \
                 (exactMatch, closeMatch, broadMatch, narrowMatch, relatedMatch)"
            )));
        };
        let (source, target) = (sssom_term(subject), sssom_term(object));
        if source.is_empty() || target.is_empty() {
            return Err(Error::invalid(format!("sssom line {n}: empty subject or object")));
        }
        out.push(Mapping { source, target, predicate });
    }
    if header.is_none() {
        return Err(Error::invalid("sssom: no header row (subject_id, predicate_id, object_id)"));
    }
    Ok(out)
}

// ── sources ──────────────────────────────────────────────────────────────

fn declared(doc: &toml::Table, section: &str) -> BTreeSet<String> {
    doc.get(section).and_then(|v| v.as_table()).map(|t| t.keys().cloned().collect()).unwrap_or_default()
}

/// Compile an [`AlignSource`]. A file whose `[pack].requires` names packages
/// is layered on them (in order); a file that does not compile alone is
/// layered on `enterprise`, the base every built-in layer extends. Either
/// way only the definitions the file itself declares are aligned.
pub fn resolve_source(source: &AlignSource) -> Result<ResolvedSource> {
    let text = match source {
        AlignSource::Pack(id) => match schema::builtin_package(id.strip_prefix("builtin:").unwrap_or(id)) {
            Ok(text) => text,
            Err(Error::Config(msg)) => return Err(Error::NotFound(msg)),
            Err(e) => return Err(e),
        },
        AlignSource::Toml(text) => {
            if text.len() > MAX_SOURCE_BYTES {
                return Err(Error::invalid(format!(
                    "align source: {} bytes of TOML, at most {MAX_SOURCE_BYTES}",
                    text.len()
                )));
            }
            text.as_str()
        }
    };
    let doc: toml::Table = toml::from_str(text).map_err(|e| Error::Config(format!("align source: {e}")))?;
    let requires: Vec<String> = doc
        .get("pack")
        .and_then(|p| p.get("requires"))
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| s.trim_start_matches("builtin:").to_string()).collect())
        .unwrap_or_default();
    let layered = |bases: &[String]| -> Result<ResolvedSource> {
        let mut schema = Schema::empty();
        for b in bases {
            schema.extend_toml(schema::builtin_package(b)?)?;
        }
        schema.extend_toml(text)?;
        Ok(ResolvedSource {
            schema,
            layered_on: bases.to_vec(),
            kinds: Some(declared(&doc, "object_types")),
            links: Some(declared(&doc, "link_types")),
        })
    };
    if !requires.is_empty() {
        return layered(&requires);
    }
    match Schema::from_toml(text) {
        Ok(schema) => Ok(ResolvedSource { schema, layered_on: Vec::new(), kinds: None, links: None }),
        Err(alone) => layered(&["enterprise".to_string()]).map_err(|_| alone),
    }
}

// ── engine ───────────────────────────────────────────────────────────────

fn read_check(p: &Principal) -> Result<()> {
    if p.allows(Access::Read) { Ok(()) } else { Err(Error::Denied(format!("{} has no read role", p.id))) }
}

fn scoped(opts: &AlignOptions, src: &ResolvedSource) -> AlignOptions {
    let mut opts = opts.clone();
    if opts.source_kinds.is_empty()
        && let Some(k) = &src.kinds
    {
        opts.source_kinds = k.iter().cloned().collect();
    }
    if opts.source_links.is_empty()
        && let Some(l) = &src.links
    {
        opts.source_links = l.iter().cloned().collect();
    }
    opts
}

impl Ontology {
    /// Align a pack or a TOML schema onto the schema in force. Read access
    /// is enough: nothing is written, and the glossary change set in the
    /// report is a proposal for `schema_plan` / `schema_apply`.
    pub fn schema_align(&self, p: &Principal, source: &AlignSource, opts: &AlignOptions) -> Result<AlignReport> {
        read_check(p)?;
        let src = resolve_source(source)?;
        let target = self.schema();
        let opts = scoped(opts, &src);
        let n = |list: &[String], all: usize| if list.is_empty() { all } else { list.len() };
        let kinds = n(&opts.source_kinds, src.schema.object_types.len()) * target.object_types.len();
        let links = n(&opts.source_links, src.schema.link_types.len()) * target.link_types.len();
        if kinds.max(links) > MAX_PAIRS {
            return Err(Error::invalid(format!(
                "align: {} pairs to score, at most {MAX_PAIRS}; narrow source_kinds or source_links",
                kinds.max(links)
            )));
        }
        Ok(align(&src.schema, &target, &opts))
    }

    /// Certify claimed mappings from a pack or a TOML schema onto the schema
    /// in force (see [`certify`]).
    pub fn schema_align_certify(
        &self,
        p: &Principal,
        source: &AlignSource,
        mappings: &[Mapping],
    ) -> Result<CertifyReport> {
        read_check(p)?;
        let src = resolve_source(source)?;
        let scope: Vec<String> = src.kinds.iter().flatten().cloned().collect();
        Ok(certify_report(&src.schema, &self.schema(), mappings, &scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaro_winkler_matches_reference_values() {
        assert!((jaro_winkler("martha", "marhta") - 0.9611).abs() < 1e-3);
        assert!((jaro_winkler("dixon", "dicksonx") - 0.8133).abs() < 1e-3);
        assert!((jaro_winkler("dwayne", "duane") - 0.84).abs() < 1e-3);
        assert_eq!(jaro_winkler("same", "same"), 1.0);
        assert_eq!(jaro_winkler("", "x"), 0.0);
        assert_eq!(jaro_winkler("abc", "xyz"), 0.0);
    }

    #[test]
    fn names_split_camel_case_and_separators() {
        assert_eq!(split_words("WorkOrder"), vec!["work", "order"]);
        assert_eq!(split_words("work_order-line item"), vec!["work", "order", "line", "item"]);
        assert_eq!(split_words("HTTPServer2"), vec!["httpserver2"]);
        assert_eq!(key("Companies"), "company");
        assert_eq!(key("tickets"), "ticket");
        assert_eq!(key("class"), "class");
        assert_eq!(key("bus"), "bus");
    }

    #[test]
    fn glossary_synonyms_make_the_same_word() {
        let a = vec![name("ticket", false)];
        let b = vec![name("issue", false), name("tickets", true)];
        let hit = name_signal(&a, &b);
        assert!(hit.glossary && hit.value == 1.0, "{hit:?}");
        // Two definitions' own names never count as a glossary hit.
        assert!(!name_signal(&[name("issue", false)], &[name("issue", false)]).glossary);
    }

    #[test]
    fn soft_match_is_one_to_one() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<BTreeSet<_>>();
        let a = vec![s(&["owns", "owned_by"]), s(&["member_of"])];
        let b = vec![s(&["owned_by"]), s(&["owns"])];
        assert_eq!(soft_match(&a, &b), (1, 3));
    }

    #[test]
    fn fuse_drops_undefined_signals() {
        let (score, coverage) = fuse(&[(0.5, Some(1.0)), (0.5, None)]);
        assert_eq!((score, coverage), (1.0, 0.5));
        assert_eq!(fuse(&[(0.5, None)]), (0.0, 0.0));
    }

    #[test]
    fn predicates_parse_in_every_spelling() {
        for (text, p) in [
            ("exact_match", MatchPredicate::ExactMatch),
            ("skos:closeMatch", MatchPredicate::CloseMatch),
            ("<http://www.w3.org/2004/02/skos/core#broadMatch>", MatchPredicate::BroadMatch),
            ("narrowMatch", MatchPredicate::NarrowMatch),
        ] {
            assert_eq!(MatchPredicate::parse(text), Some(p), "{text}");
        }
        assert_eq!(MatchPredicate::parse("owl:equivalentClass"), None);
        let p: MatchPredicate = serde_json::from_value(serde_json::json!("skos:relatedMatch")).unwrap();
        assert_eq!(p, MatchPredicate::RelatedMatch);
    }

    #[test]
    fn sssom_terms_take_the_local_name() {
        assert_eq!(sssom_term("src:class/work_order"), "work_order");
        assert_eq!(sssom_term("<https://onto-intelli.dev/schema/x/class/issue>"), "issue");
        assert_eq!(sssom_term("ex:issue"), "issue");
        assert_eq!(sssom_term("issue"), "issue");
        assert_eq!(sssom_term("src:class/a%20b"), "a b");
        assert_eq!(cell("a\tb\nc"), "a b c");
    }
}
