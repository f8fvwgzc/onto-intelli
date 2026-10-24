//! The assurance actions: the reasoner and its certificates, explanations,
//! shapes, schema lint and locks, industry packs, schema alignment and sheet
//! induction, as JSON-in/JSON-out arms of the action table (see
//! [`crate::actions::ACTIONS`]).
//!
//! The engine (onto-core) derives and measures; it never says `entailed` or
//! `checked`. Only `reason.certify`'s `check` and `reason.check_certificate`
//! carry checker words, and both come from the independent `onto-cert`
//! library, which shares no code with the engine. Derived facts are never
//! written into the graph, and every report says what it means in `means`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{Value, json};

use onto_core::align::{AlignOptions, AlignSource, Mapping};
use onto_core::explain::{Claim, JustifyOptions, Semiring, SliceOptions};
use onto_core::induce::{self, InduceOptions, SheetMapping};
use onto_core::lint::{self, LintDecision, LintQuery};
use onto_core::packs::{self, InstallMode, PackInstallOptions};
use onto_core::reason::ReasonOptions;
use onto_core::shape_model::Shape;
use onto_core::shapes::{ShapeInduceOptions, ShapesQuery};
use onto_core::{Access, Entity, Error, Ontology, Principal, Result};

use crate::actions::{parse, to_json};
use crate::intelli::Intelli;

/// Assurance actions that only read the graph (and the schema): they also
/// run inside what-if scenarios on a fork (see `scenario.run`).
pub const GRAPH_READS: &[&str] = &[
    "reason.run",
    "reason.query",
    "reason.explain",
    "reason.provenance",
    "reason.certify",
    "reason.binding",
    "reason.verify",
    "reason.slice_check",
    "shapes.validate",
    "shapes.induce",
    "shapes.check",
    "model.induce",
];

/// The other assurance actions: certificate checks without a graph, lint
/// (it reads stored feedback), locks, packs, alignment and sheet ingest.
pub const CALLS: &[&str] = &[
    "reason.check_certificate",
    "schema.lint",
    "schema.lint_feedback",
    "schema.locks",
    "schema.lock",
    "schema.unlock",
    "schema.align",
    "schema.align_certify",
    "pack.list",
    "pack.show",
    "pack.plan",
    "pack.install",
    "ingest.sheet",
];

/// What a `reason.run` report means.
pub const RUN_MEANS: &str = "Derived facts are the engine's opinion under the rule table: they are labelled \
     derived, never written into the graph, and each carries the rule and premises that produced it. Certify them \
     (reason.certify) for a proof an independent checker accepts or refuses. A clean run is not a consistency \
     result: not_evaluated lists the schema constructs the reasoner did not evaluate. With fixpoint_reached false \
     the closure is a lower bound.";

/// Sample derivations listed by `reason.run` unless the input says otherwise.
const RUN_SAMPLES: usize = 20;
/// Derivations `reason.run` lists at most.
const MAX_RUN_SAMPLES: usize = 1000;
/// Bindings `reason.query` returns at most.
const MAX_QUERY_LIMIT: usize = 1000;
/// Lost facts `reason.slice_check` lists at most.
const MAX_SLICE_LIMIT: usize = 1000;
/// Justifications `reason.explain` searches for at most.
const MAX_JUSTIFICATIONS: usize = 100;
/// What a certificate of a closure that stopped at a limit means.
const CERTIFY_PARTIAL: &str = "the closure stopped at a limit before its fixpoint: every listed derivation still \
     checks, but they are a lower bound of what the rules derive";
/// Bindings returned by `reason.query` unless the input says otherwise.
const QUERY_LIMIT: usize = 50;
/// New lint findings `pack.show` lists.
const PACK_LINT_SAMPLES: usize = 50;

#[derive(Deserialize, Default)]
struct RunIn {
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    max_derived: Option<usize>,
    #[serde(default)]
    samples: Option<usize>,
}

#[derive(Deserialize)]
struct QueryIn {
    pattern: Vec<Vec<Value>>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct ExplainIn {
    fact: Value,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    max: Option<usize>,
}

#[derive(Deserialize)]
struct ProvenanceIn {
    fact: Value,
    #[serde(default)]
    semiring: Option<Semiring>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize, Default)]
struct ScopeIn {
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct CertFilesIn {
    rules_tsv: String,
    asserted_tsv: String,
    derivations_tsv: String,
    #[serde(default)]
    manifest_json: Option<String>,
}

#[derive(Deserialize)]
struct BindingIn {
    asserted_tsv: String,
    #[serde(default)]
    manifest_json: Option<String>,
}

#[derive(Deserialize)]
struct VerifyIn {
    claims: Vec<Claim>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct SliceIn {
    entities: Vec<String>,
    #[serde(default)]
    goals: Option<Vec<Claim>>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct InduceShapeIn {
    kind: String,
    #[serde(flatten)]
    opts: ShapeInduceOptions,
}

#[derive(Deserialize)]
struct CheckShapesIn {
    shapes: BTreeMap<String, Shape>,
}

#[derive(Deserialize)]
struct LintFeedbackIn {
    id: String,
    decision: LintDecision,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct LockIn {
    section: String,
    name: String,
    reason: String,
}

#[derive(Deserialize)]
struct UnlockIn {
    section: String,
    name: String,
}

#[derive(Deserialize)]
struct AlignIn {
    #[serde(default)]
    pack: Option<String>,
    #[serde(default)]
    toml: Option<String>,
    /// `min_score` under its catalogue name.
    #[serde(default)]
    threshold: Option<f64>,
    #[serde(flatten)]
    opts: AlignOptions,
}

#[derive(Deserialize)]
struct AlignCertifyIn {
    #[serde(default)]
    pack: Option<String>,
    #[serde(default)]
    toml: Option<String>,
    mappings: Vec<Mapping>,
}

#[derive(Deserialize)]
struct PackIn {
    id: String,
}

#[derive(Deserialize)]
struct PackInstallIn {
    id: String,
    #[serde(flatten)]
    opts: PackInstallOptions,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct SheetIn {
    csv: String,
    #[serde(flatten)]
    opts: InduceOptions,
}

#[derive(Deserialize)]
struct IngestSheetIn {
    csv: String,
    #[serde(default)]
    mapping: Option<SheetMapping>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

fn scope(workspace: Option<String>) -> ReasonOptions {
    ReasonOptions { workspace: workspace.filter(|w| !w.is_empty()), ..ReasonOptions::default() }
}

/// A fact as `[s, p, o]` (the object a JSON value: an entity id, a name or a
/// literal) or as a claim `{subject, relation, object}`.
fn claim_of(fact: Value) -> Result<Claim> {
    match fact {
        Value::Array(mut v) if v.len() == 3 => {
            let o = v.pop().unwrap_or_default();
            let (Some(s), Some(r)) = (v[0].as_str(), v[1].as_str()) else {
                return Err(Error::invalid("fact: subject and relation must be strings"));
            };
            Ok(Claim::new(s, r, o))
        }
        Value::Array(v) => Err(Error::invalid(format!("fact: a triple has 3 terms, not {}", v.len()))),
        v @ Value::Object(_) => serde_json::from_value(v).map_err(|e| Error::invalid(format!("fact: {e}"))),
        _ => Err(Error::invalid("fact: give [subject, relation, object] or {subject, relation, object}")),
    }
}

fn read_check(p: &Principal) -> Result<()> {
    if p.allows(Access::Read) { Ok(()) } else { Err(Error::Denied(format!("{} has no read role", p.id))) }
}

/// Exactly one of `pack` / `toml`.
fn align_source(pack: Option<String>, toml: Option<String>) -> Result<AlignSource> {
    match (pack.filter(|s| !s.trim().is_empty()), toml.filter(|s| !s.trim().is_empty())) {
        (Some(p), None) => Ok(AlignSource::Pack(p)),
        (None, Some(t)) => Ok(AlignSource::Toml(t)),
        (Some(_), Some(_)) => Err(Error::invalid("align: give pack or toml, not both")),
        (None, None) => Err(Error::invalid("align: give pack (a pack id) or toml (a schema document)")),
    }
}

/// The checker's report on certificate files: the independent `onto-cert`
/// library, in process.
fn check_files(rules: &str, asserted: &str, derivations: &str, manifest: Option<&str>) -> Value {
    let files = onto_cert::CertFiles {
        rules: rules.to_string(),
        asserted: asserted.to_string(),
        derivations: derivations.to_string(),
        manifest: manifest.filter(|m| !m.trim().is_empty()).map(str::to_string),
    };
    onto_cert::check(&files).to_json()
}

/// The lint findings a pack brings (schema checks only: the graph is not
/// consulted): those of the pack on the packages it requires that those
/// packages alone do not have, so the counts are the pack's own whatever is
/// installed; plus how many findings are new when it is layered on
/// `current` (none once it is installed).
fn pack_lint(current: &onto_core::Schema, id: &str) -> Value {
    let all = |_: &Entity| true;
    let ids = |s: &onto_core::Schema| -> BTreeSet<String> {
        lint::lint(s, None, &all).findings.into_iter().map(|f| f.id).collect()
    };
    let requires = || -> Result<onto_core::Schema> {
        let mut s = onto_core::Schema::empty();
        for r in &packs::find(id)?.meta.requires {
            s.extend_toml(onto_core::schema::builtin_package(r)?)?;
        }
        Ok(s)
    };
    let (own, base) = match (packs::standalone(id), requires()) {
        (Ok(own), Ok(base)) => (own, base),
        (Err(e), _) | (_, Err(e)) => return json!({ "skipped": e.to_string(), "means": lint::MEANS }),
    };
    let base_ids = ids(&base);
    let report = lint::lint(&own, None, &all);
    let mine: Vec<_> = report.findings.iter().filter(|f| !base_ids.contains(&f.id)).collect();
    let mut counts = lint::LintCounts::default();
    for f in &mine {
        match f.severity {
            lint::Severity::Error => counts.error += 1,
            lint::Severity::Warning => counts.warning += 1,
            lint::Severity::Info => counts.info += 1,
        }
    }
    let new_on_current = match packs::with_pack(current, id) {
        Ok(layered) => {
            let before = ids(current);
            json!(ids(&layered).difference(&before).count())
        }
        Err(e) => json!({ "skipped": e.to_string() }),
    };
    json!({
        "counts": counts,
        "new_findings": mine.len(),
        "findings": mine.into_iter().take(PACK_LINT_SAMPLES).collect::<Vec<_>>(),
        "new_on_current": new_on_current,
        "checks_run": report.checks_run,
        "means": format!("{} Schema checks of the pack on the packages it requires; findings those packages have \
                          without the pack are not counted. new_on_current counts the findings layering the pack \
                          on the current schema adds (none once it is installed).", lint::MEANS),
    })
}

/// Run a read-only assurance action against any ontology (live or a fork).
pub fn graph_read(onto: &Ontology, p: &Principal, action: &str, input: Value) -> Result<Value> {
    match action {
        "reason.run" => {
            let i: RunIn = parse(action, input)?;
            let mut opts = scope(i.workspace);
            // A caller may lower the engine's bound, never raise it (and a
            // bound at the default shares the cached closure).
            if let Some(m) = i.max_derived.filter(|m| *m > 0) {
                opts.max_derived = m.min(opts.max_derived);
            }
            let c = onto.reason(p, &opts)?;
            let mut v = to_json(&c.stats())?;
            let samples: Vec<Value> = c
                .derived()
                .iter()
                .take(i.samples.unwrap_or(RUN_SAMPLES).min(MAX_RUN_SAMPLES))
                .map(|d| json!({ "fact": d.conclusion, "rule": d.rule, "premises": d.premises }))
                .collect();
            v["samples"] = Value::Array(samples);
            v["means"] = json!(RUN_MEANS);
            Ok(v)
        }
        "reason.query" => {
            let i: QueryIn = parse(action, input)?;
            if i.pattern.is_empty() {
                return Err(Error::invalid("reason.query: pattern needs at least one [s, p, o] atom"));
            }
            let limit = i.limit.unwrap_or(QUERY_LIMIT).min(MAX_QUERY_LIMIT);
            to_json(&onto.reason_query(p, &scope(i.workspace), &i.pattern, limit)?)
        }
        "reason.explain" => {
            let i: ExplainIn = parse(action, input)?;
            let claim = claim_of(i.fact)?;
            let mut bounds = JustifyOptions::default();
            if let Some(m) = i.max.filter(|m| *m > 0) {
                bounds.max_justifications = m.min(MAX_JUSTIFICATIONS);
            }
            to_json(&onto.reason_explain(p, &scope(i.workspace), &claim, &bounds)?)
        }
        "reason.provenance" => {
            let i: ProvenanceIn = parse(action, input)?;
            let claim = claim_of(i.fact)?;
            let semiring = i.semiring.unwrap_or_default();
            to_json(&onto.reason_provenance(p, &scope(i.workspace), &claim, semiring, &JustifyOptions::default())?)
        }
        "reason.certify" => {
            let i: ScopeIn = parse(action, input)?;
            let opts = scope(i.workspace);
            let cert = onto.reason_certificate(p, &opts)?;
            let check =
                check_files(&cert.rules_tsv, &cert.asserted_tsv, &cert.derivations_tsv, Some(&cert.manifest_json));
            // The checker does not read attested fields (by design); the
            // closure's own flag is carried next to its verdict instead.
            let stats = onto.reason_stats(p, &opts)?;
            let mut v = json!({
                "certificate": cert,
                "check": check,
                "fixpoint_reached": stats.fixpoint_reached,
                "rounds": stats.rounds,
            });
            if let Some(t) = stats.truncated {
                v["truncated"] = json!(t);
            }
            if !stats.fixpoint_reached {
                v["means"] = json!(CERTIFY_PARTIAL);
            }
            Ok(v)
        }
        "reason.binding" => {
            let i: BindingIn = parse(action, input)?;
            to_json(&onto.reason_binding(p, &i.asserted_tsv, i.manifest_json.as_deref().filter(|m| !m.is_empty()))?)
        }
        "reason.verify" => {
            let i: VerifyIn = parse(action, input)?;
            to_json(&onto.reason_verify(p, &scope(i.workspace), &i.claims)?)
        }
        "reason.slice_check" => {
            let i: SliceIn = parse(action, input)?;
            let mut opts = SliceOptions { reason: scope(i.workspace), ..SliceOptions::default() };
            if let Some(l) = i.limit.filter(|l| *l > 0) {
                opts.limit = l.min(MAX_SLICE_LIMIT);
            }
            let slice: BTreeSet<String> = i.entities.into_iter().map(|id| onto.resolve_id(&id)).collect();
            let goals = i.goals.filter(|g| !g.is_empty());
            to_json(&onto.reason_slice_check(p, &opts, &slice, goals.as_deref())?)
        }
        "shapes.validate" => {
            let q: ShapesQuery = parse(action, input)?;
            to_json(&onto.shapes_validate(p, &q)?)
        }
        "shapes.induce" => {
            let i: InduceShapeIn = parse(action, input)?;
            to_json(&onto.shapes_induce(p, &i.kind, &i.opts)?)
        }
        "shapes.check" => {
            let i: CheckShapesIn = parse(action, input)?;
            read_check(p)?;
            to_json(&onto.shapes_check(&i.shapes))
        }
        "model.induce" => {
            let i: SheetIn = parse(action, input)?;
            to_json(&onto.induce_sheet(p, &i.csv, &i.opts)?)
        }
        other => Err(Error::invalid(format!("{other} is not a read-only assurance action"))),
    }
}

impl Intelli {
    /// The assurance actions that are not pure graph reads (see [`CALLS`]).
    pub(crate) fn assurance_call(&self, p: &Principal, action: &str, input: Value) -> Result<Value> {
        let onto = self.ontology();
        match action {
            "reason.check_certificate" => {
                let i: CertFilesIn = parse(action, input)?;
                read_check(p)?;
                Ok(check_files(&i.rules_tsv, &i.asserted_tsv, &i.derivations_tsv, i.manifest_json.as_deref()))
            }
            "schema.lint" => {
                let q: LintQuery = parse(action, input)?;
                to_json(&onto.schema_lint(p, q)?)
            }
            "schema.lint_feedback" => {
                let i: LintFeedbackIn = parse(action, input)?;
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                to_json(&onto.lint_feedback(&ctx, &i.id, i.decision, i.note)?)
            }
            "schema.locks" => onto.schema_locks(p).map(|l| json!({ "items": l })),
            "schema.lock" => {
                let i: LockIn = parse(action, input)?;
                let ctx = self.ctx(p, None).with_intent(action);
                to_json(&onto.schema_lock(&ctx, &i.section, &i.name, &i.reason)?)
            }
            "schema.unlock" => {
                let i: UnlockIn = parse(action, input)?;
                let ctx = self.ctx(p, None).with_intent(action);
                match onto.schema_unlock(&ctx, &i.section, &i.name) {
                    Ok(lock) => Ok(json!({ "removed": true, "lock": lock })),
                    // Nothing held: releasing is a no-op, not an error.
                    Err(Error::NotFound(_)) => Ok(json!({ "removed": false })),
                    Err(e) => Err(e),
                }
            }
            "schema.align" => {
                let mut i: AlignIn = parse(action, input)?;
                if let Some(t) = i.threshold {
                    i.opts.min_score = t;
                }
                let source = align_source(i.pack, i.toml)?;
                to_json(&onto.schema_align(p, &source, &i.opts)?)
            }
            "schema.align_certify" => {
                let i: AlignCertifyIn = parse(action, input)?;
                let source = align_source(i.pack, i.toml)?;
                let r = onto.schema_align_certify(p, &source, &i.mappings)?;
                Ok(json!({
                    "items": r.rows,
                    "counts": r.counts,
                    "unmapped_source": r.unmapped_source,
                    "unmapped_target": r.unmapped_target,
                    "sssom_tsv": r.sssom_tsv,
                    "means": r.means,
                }))
            }
            "pack.list" => onto.pack_list(p).map(|l| json!({ "items": l })),
            "pack.show" => {
                let i: PackIn = parse(action, input)?;
                let mut v = to_json(&onto.pack_show(p, &i.id)?)?;
                v["lint"] = pack_lint(&onto.schema(), &i.id);
                Ok(v)
            }
            "pack.plan" => {
                let i: PackIn = parse(action, input)?;
                to_json(&onto.pack_plan(p, &i.id)?)
            }
            "pack.install" => {
                let mut i: PackInstallIn = parse(action, input)?;
                // Agents never apply schema: a pack from an agent is a
                // proposal, whatever the input says (as propose_ontology).
                let forced = p.is_agent() && i.opts.mode == InstallMode::Apply;
                if p.is_agent() {
                    i.opts.mode = InstallMode::Propose;
                }
                let ctx = self.ctx(p, i.workspace).with_intent(action);
                let mut v = to_json(&onto.pack_install(&ctx, &i.id, i.opts)?)?;
                if forced {
                    v["note"] = json!("agents propose schema changes: the install was filed as a proposal");
                }
                Ok(v)
            }
            "ingest.sheet" => {
                let i: IngestSheetIn = parse(action, input)?;
                let ws = self.ws_or_default(i.workspace);
                let induced = i.mapping.is_none();
                let mapping = match i.mapping {
                    Some(m) => m,
                    None => {
                        let opts =
                            InduceOptions { kind: i.kind, key: i.key, file: i.file, workspace: Some(ws.clone()) };
                        let ind = onto.induce_sheet(p, &i.csv, &opts)?;
                        // Induction always names a free kind (`task_2` when
                        // `task` exists), so rows go into the existing type
                        // the sheet designates, mapped column by column.
                        let schema = onto.schema();
                        if schema.object_types.contains_key(&ind.base_kind) {
                            induce::mapping_onto(&schema, &ind, &ind.base_kind)?
                        } else if ind.base_kind == ind.kind {
                            return Err(Error::invalid(format!(
                                "the sheet describes object type {}, which the schema does not define yet: apply \
                                 or propose the change_set of model.induce first, then ingest with its mapping",
                                ind.kind
                            )));
                        } else {
                            return Err(Error::invalid(format!(
                                "the sheet's rows are named {}, which is not an object type here (an interface, \
                                 derived class or kind in use): pass kind to name an object type, or a mapping",
                                ind.base_kind
                            )));
                        }
                    }
                };
                let ctx = self.ctx(p, Some(ws)).with_intent(action);
                let mut v = to_json(&onto.ingest_sheet(&ctx, &i.csv, &mapping, i.dry_run)?)?;
                v["mapping"] = to_json(&mapping)?;
                v["mapping_induced"] = json!(induced);
                Ok(v)
            }
            other => Err(Error::not_found(format!("action '{other}' (see system.actions)"))),
        }
    }
}
