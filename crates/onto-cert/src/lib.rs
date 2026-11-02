//! onto-cert: the independent checker for onto-intelli reasoning certificates
//! (format `oi-cert/1`).
//!
//! The onto-intelli engine derives facts from a typed property graph and can
//! write a certificate: `rules.tsv` (the rule table), `asserted.tsv` (the
//! facts the run started from), `derivations.tsv` (one line per derived fact:
//! the rule, the conclusion and the premises in the rule's body order) and
//! `manifest.json` (digests and counts of those files, plus the engine's
//! claims about the run). [`check`] reads those files and returns a
//! [`Report`].
//!
//! What a successful check proves: every conclusion in `derivations.tsv`
//! follows from the facts in `asserted.tsv` under the rules it cites. The
//! checker re-derives each line by pattern alone, without search: it unifies
//! each body atom of the cited rule with the premise in the same position
//! under one consistent binding, instantiates the head and compares it with
//! the conclusion, and requires every premise to be asserted or the
//! conclusion of an EARLIER line. The first line that fails stops the check
//! and is named with its rule and the reason.
//!
//! The five built-in rules (kind inheritance, transitive `extends`,
//! interfaces, transitive and symmetric relations) are a fixed table
//! compiled into this crate ([`BUILTIN_RULES`]). A rule named `builtin/...`
//! whose line differs from that table by one byte is rejected. A certificate
//! that cites only built-in rules earns [`Verdict::Entailed`]. One that cites
//! any other rule earns [`Verdict::EntailedUnderSuppliedRules`]: a supplied
//! rule is an assumption the certificate carries, not a fact it establishes.
//!
//! What it does not prove: that `asserted.tsv` is the graph the engine read
//! (comparing it with the store is the engine's binding check), that the
//! supplied rules are true, that the derivations are complete (the closure
//! may be a lower bound), or anything in the manifest's `attested` block,
//! which holds the engine's claims. The manifest's `derived` digests and
//! counts are recomputed here, so they bind the certificate to its bytes.
//!
//! This crate shares no code with the engine. It depends on std, `sha2` and
//! `serde_json` only and reimplements the format from its specification, so
//! a bug in the engine's reasoner cannot also hide in its checker. The
//! verdict words `entailed`, `entailed_under_supplied_rules` and the
//! [`Accepted`] value belong to this crate: an [`Accepted`] holds a private
//! token, so only [`check`] can make one, and reports are written to JSON
//! but never read back from it.

mod parse;

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

pub use parse::{ASSERTED_FILE, DERIVATIONS_FILE, MANIFEST_FILE, RULES_FILE};

use parse::{Triple, is_var};

/// The certificate format this checker reads.
pub const FORMAT: &str = "oi-cert/1";

/// The checker's version, reported as `onto-cert <version>`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The built-in rule table, one rule per line in the `rules.tsv` format. The
/// engine writes these five lines first, in this order, byte for byte.
pub const BUILTIN_RULES: &str = "\
builtin/type_extends\t?x\ta\t?p\t?x\ta\t?k\t?k\textends\t?p
builtin/extends_transitive\t?k\textends\t?q\t?k\textends\t?p\t?p\textends\t?q
builtin/type_implements\t?x\ta\t?i\t?x\ta\t?k\t?k\timplements\t?i
builtin/transitive\t?x\t?r\t?z\t?r\ta\ttransitive\t?x\t?r\t?y\t?y\t?r\t?z
builtin/symmetric\t?y\t?r\t?x\t?r\ta\tsymmetric\t?x\t?r\t?y
";

/// The name prefix reserved for the built-in rules.
pub const BUILTIN_PREFIX: &str = "builtin/";

const MEANS_ENTAILED: &str = "every derived fact follows from asserted.tsv under the built-in rules alone, re-derived \
    line by line by pattern without search; this does not show that asserted.tsv is the graph the engine read";
const MEANS_SUPPLIED: &str = "every derived fact follows from asserted.tsv under the cited rules, re-derived line by \
    line by pattern without search; the supplied (non-built-in) rules are assumptions the certificate carries, not \
    facts it establishes; this does not show that asserted.tsv is the graph the engine read";

/// The text of a certificate's files. `manifest` is `None` when there is no
/// `manifest.json`; the digest check is then skipped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CertFiles {
    pub rules: String,
    pub asserted: String,
    pub derivations: String,
    pub manifest: Option<String>,
}

/// A certificate file that could not be read.
#[derive(Debug)]
pub struct ReadError {
    pub file: &'static str,
    pub error: std::io::Error,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "could not read {}", self.file)
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl CertFiles {
    /// Reads `rules.tsv`, `asserted.tsv`, `derivations.tsv` and, when it
    /// exists, `manifest.json` from a directory.
    pub fn from_dir(dir: &Path) -> Result<CertFiles, ReadError> {
        let read =
            |file: &'static str| std::fs::read_to_string(dir.join(file)).map_err(|error| ReadError { file, error });
        let manifest = match std::fs::read_to_string(dir.join(MANIFEST_FILE)) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(ReadError { file: MANIFEST_FILE, error }),
        };
        Ok(CertFiles {
            rules: read(RULES_FILE)?,
            asserted: read(ASSERTED_FILE)?,
            derivations: read(DERIVATIONS_FILE)?,
            manifest,
        })
    }
}

/// The checker's verdict on an accepted certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Every cited rule is a built-in rule.
    Entailed,
    /// At least one cited rule is a supplied rule: the conclusions hold if the supplied rules do.
    EntailedUnderSuppliedRules,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Entailed => "entailed",
            Verdict::EntailedUnderSuppliedRules => "entailed_under_supplied_rules",
        }
    }
}

/// Why a well-formed certificate was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    /// A rule named `builtin/...` differs from the compiled-in table line.
    ForgedBuiltinRule,
    /// A digest or count in the manifest differs from the recomputed value.
    DigestMismatch,
    /// A derivation cites a rule that `rules.tsv` does not define.
    UnknownRule,
    /// A derivation has a different number of premises than the rule has body atoms.
    Arity,
    /// A premise does not match its body atom under the binding so far.
    PremiseMismatch,
    /// The instantiated head differs from the conclusion.
    ConclusionMismatch,
    /// A premise is neither asserted nor concluded by an earlier line.
    PremiseNotKnown,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::ForgedBuiltinRule => "forged_builtin_rule",
            Reason::DigestMismatch => "digest_mismatch",
            Reason::UnknownRule => "unknown_rule",
            Reason::Arity => "arity",
            Reason::PremiseMismatch => "premise_mismatch",
            Reason::ConclusionMismatch => "conclusion_mismatch",
            Reason::PremiseNotKnown => "premise_not_known",
        }
    }
}

/// Proof that a value was produced by [`check`]. It has no public constructor.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Checked;

/// An accepted certificate. Only [`check`] can construct one: the private
/// token field makes a struct literal outside this crate fail to compile.
///
/// ```compile_fail
/// let forged = onto_cert::Accepted {
///     verdict: onto_cert::Verdict::Entailed,
///     asserted: 0,
///     derivations: 0,
///     builtin_cited: 0,
///     supplied_cited: 0,
///     rules_sha256: String::new(),
///     checked: unreachable!(),
/// };
/// ```
///
/// Every field is private and read through a getter, so a value that
/// [`check`] returned cannot be edited into a stronger verdict or bound to
/// another certificate's rules:
///
/// ```compile_fail
/// fn upgrade(mut a: onto_cert::Accepted) -> onto_cert::Accepted {
///     a.verdict = onto_cert::Verdict::Entailed;
///     a
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    verdict: Verdict,
    asserted: usize,
    derivations: usize,
    builtin_cited: usize,
    supplied_cited: usize,
    rules_sha256: String,
    checked: Checked,
}

impl Accepted {
    /// The checker's verdict.
    pub fn verdict(&self) -> Verdict {
        self.verdict
    }

    /// Lines in `asserted.tsv`.
    pub fn asserted(&self) -> usize {
        self.asserted
    }

    /// Lines in `derivations.tsv`.
    pub fn derivations(&self) -> usize {
        self.derivations
    }

    /// Distinct built-in rules cited by some derivation.
    pub fn builtin_cited(&self) -> usize {
        self.builtin_cited
    }

    /// Distinct supplied rules cited by some derivation.
    pub fn supplied_cited(&self) -> usize {
        self.supplied_cited
    }

    /// SHA-256 of the `rules.tsv` bytes, lowercase hex.
    pub fn rules_sha256(&self) -> &str {
        &self.rules_sha256
    }
}

/// A rejected certificate: the first failure found, in check order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected {
    pub reason: Reason,
    /// Lines in `asserted.tsv`.
    pub asserted: usize,
    /// Lines in `derivations.tsv`.
    pub derivations: usize,
    /// The 1-based derivation line that failed, for derivation checks.
    pub first_rejected: Option<usize>,
    /// The rule that was forged or cited by the failing line.
    pub rule: Option<String>,
    /// The 1-based premise position, for `premise_mismatch` and `premise_not_known`.
    pub premise: Option<usize>,
    /// The failing line's conclusion, spelled `s p o`.
    pub conclusion: Option<String>,
    /// The failing line's premises, spelled `s p o`.
    pub premises: Vec<String>,
    /// The file and line, for a forged built-in rule.
    pub file: Option<&'static str>,
    pub line: Option<usize>,
    /// The manifest field, its value and the recomputed value, for `digest_mismatch`.
    pub field: Option<String>,
    pub expected: Option<Value>,
    pub actual: Option<Value>,
}

/// A certificate that could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Malformed {
    pub file: &'static str,
    /// 1-based line; 0 when the problem is not tied to a line.
    pub line: usize,
    pub reason: String,
}

/// The three outcomes of a check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted(Accepted),
    Rejected(Rejected),
    Malformed(Malformed),
}

/// The result of [`check`]. It is written to JSON with [`Report::to_json`]
/// and has no way to be read back, nor to be built outside this crate.
///
/// The design asks for `Report` to be `Serialize` (never `Deserialize`).
/// This crate's dependency rule allows std, `sha2` and `serde_json` only, and
/// `serde_json` does not re-export the `serde::Serialize` trait, so `Report`
/// implements no serde trait. Callers serialize through [`Report::to_json`]:
/// to embed a report in a `#[derive(Serialize)]` response, store the
/// [`serde_json::Value`] it returns.
///
/// ```compile_fail
/// let m = onto_cert::Malformed { file: "rules.tsv", line: 0, reason: String::new() };
/// let r = onto_cert::Report { outcome: onto_cert::Outcome::Malformed(m) };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    outcome: Outcome,
}

impl Report {
    pub fn outcome(&self) -> &Outcome {
        &self.outcome
    }

    /// The accepted certificate, if the check succeeded.
    pub fn accepted(&self) -> Option<&Accepted> {
        match &self.outcome {
            Outcome::Accepted(a) => Some(a),
            _ => None,
        }
    }

    /// The rejection, if the certificate was well formed but did not check.
    pub fn rejected(&self) -> Option<&Rejected> {
        match &self.outcome {
            Outcome::Rejected(r) => Some(r),
            _ => None,
        }
    }

    /// The parse failure, if the certificate was malformed.
    pub fn malformed(&self) -> Option<&Malformed> {
        match &self.outcome {
            Outcome::Malformed(m) => Some(m),
            _ => None,
        }
    }

    pub fn verdict(&self) -> Option<Verdict> {
        self.accepted().map(|a| a.verdict)
    }

    /// 0 accepted, 1 rejected, 2 malformed.
    pub fn exit_code(&self) -> i32 {
        match self.outcome {
            Outcome::Accepted(_) => 0,
            Outcome::Rejected(_) => 1,
            Outcome::Malformed(_) => 2,
        }
    }

    /// The report as a JSON object.
    pub fn to_json(&self) -> Value {
        match &self.outcome {
            Outcome::Accepted(a) => json!({
                "ok": true,
                "verdict": a.verdict.as_str(),
                "asserted": a.asserted,
                "derivations": a.derivations,
                "rules_cited": {"builtin": a.builtin_cited, "supplied": a.supplied_cited},
                "rules_sha256": a.rules_sha256,
                "checker": format!("onto-cert {VERSION}"),
                "means": match a.verdict {
                    Verdict::Entailed => MEANS_ENTAILED,
                    Verdict::EntailedUnderSuppliedRules => MEANS_SUPPLIED,
                },
            }),
            Outcome::Rejected(r) => {
                let mut m = Map::new();
                m.insert("ok".into(), false.into());
                m.insert("reason".into(), r.reason.as_str().into());
                m.insert("asserted".into(), r.asserted.into());
                m.insert("derivations".into(), r.derivations.into());
                let mut opt = |k: &str, v: Option<Value>| {
                    if let Some(v) = v {
                        m.insert(k.into(), v);
                    }
                };
                opt("first_rejected", r.first_rejected.map(Value::from));
                opt("rule", r.rule.clone().map(Value::from));
                opt("premise", r.premise.map(Value::from));
                opt("conclusion", r.conclusion.clone().map(Value::from));
                opt("premises", r.first_rejected.map(|_| r.premises.clone().into()));
                opt("file", r.file.map(Value::from));
                opt("line", r.line.map(Value::from));
                opt("field", r.field.clone().map(Value::from));
                opt("expected", r.expected.clone());
                opt("actual", r.actual.clone());
                Value::Object(m)
            }
            Outcome::Malformed(e) => json!({
                "ok": false,
                "error": "malformed",
                "file": e.file,
                "line": e.line,
                "reason": e.reason,
            }),
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The compiled-in line of a built-in rule, by name.
fn builtin_line(name: &str) -> Option<&'static str> {
    BUILTIN_RULES.lines().find(|l| l.split('\t').next() == Some(name))
}

fn spell(t: &Triple) -> String {
    t.join(" ")
}

/// Unifies a rule atom with a ground fact, extending `binding`. Constants
/// must be equal; a variable already bound must be bound to the same term.
fn unify<'a>(atom: &Triple<'a>, fact: &Triple<'a>, binding: &mut Vec<(&'a str, &'a str)>) -> bool {
    for (t, v) in atom.iter().zip(fact) {
        if is_var(t) {
            match binding.iter().find(|(n, _)| n == t) {
                Some((_, b)) if b != v => return false,
                Some(_) => {}
                None => binding.push((t, v)),
            }
        } else if t != v {
            return false;
        }
    }
    true
}

/// Checks a certificate. See the crate documentation for what an accepted
/// certificate proves and what it does not.
///
/// Order: (1) parse every file, (2) compare each `builtin/...` rule with the
/// compiled-in table, (3) recompute the manifest's digests and counts when a
/// manifest is given, (4) re-derive each derivation line in order. The first
/// failure decides the report.
pub fn check(files: &CertFiles) -> Report {
    let outcome = match run(files) {
        Ok(a) => Outcome::Accepted(a),
        Err(o) => *o,
    };
    Report { outcome }
}

fn malformed(m: Malformed) -> Box<Outcome> {
    Box::new(Outcome::Malformed(m))
}

/// The check itself; the error is the failing outcome, boxed because it is large.
fn run(files: &CertFiles) -> Result<Accepted, Box<Outcome>> {
    // 1. Parse.
    let rules = parse::rules(&files.rules).map_err(malformed)?;
    let asserted = parse::asserted(&files.asserted).map_err(malformed)?;
    let steps = parse::derivations(&files.derivations).map_err(malformed)?;
    let manifest = files.manifest.as_deref().map(parse::manifest).transpose().map_err(malformed)?;
    let (n_asserted, n_steps) = (asserted.len(), steps.len());
    let reject = |reason: Reason| Rejected {
        reason,
        asserted: n_asserted,
        derivations: n_steps,
        first_rejected: None,
        rule: None,
        premise: None,
        conclusion: None,
        premises: Vec::new(),
        file: None,
        line: None,
        field: None,
        expected: None,
        actual: None,
    };

    // 2. Built-in rules are byte-identical to the compiled-in table.
    for r in rules.iter().filter(|r| r.name.starts_with(BUILTIN_PREFIX)) {
        if builtin_line(r.name) != Some(r.text) {
            return Err(Box::new(Outcome::Rejected(Rejected {
                rule: Some(r.name.to_string()),
                file: Some(RULES_FILE),
                line: Some(r.line),
                ..reject(Reason::ForgedBuiltinRule)
            })));
        }
    }

    // 3. The manifest's derived block matches the bytes.
    let rules_sha256 = sha256_hex(files.rules.as_bytes());
    if let Some(m) = &manifest {
        let digests = [
            ("asserted_sha256", &m.asserted_sha256, sha256_hex(files.asserted.as_bytes())),
            ("rules_sha256", &m.rules_sha256, rules_sha256.clone()),
            ("derivations_sha256", &m.derivations_sha256, sha256_hex(files.derivations.as_bytes())),
        ];
        let counts = [
            ("asserted", m.asserted, n_asserted as u64),
            ("rules", m.rules, rules.len() as u64),
            ("derivations", m.derivations, n_steps as u64),
        ];
        let mismatch = digests
            .into_iter()
            .find(|(_, want, got)| *want != got)
            .map(|(k, want, got)| (k, Value::from(want.as_str()), Value::from(got)))
            .or_else(|| counts.into_iter().find(|(_, want, got)| want != got).map(|(k, w, g)| (k, w.into(), g.into())));
        if let Some((field, expected, actual)) = mismatch {
            return Err(Box::new(Outcome::Rejected(Rejected {
                field: Some(field.to_string()),
                expected: Some(expected),
                actual: Some(actual),
                ..reject(Reason::DigestMismatch)
            })));
        }
    }

    // 4. Re-derive each line; its premises must already be known.
    let mut known: HashSet<Triple> = asserted.iter().copied().collect();
    let mut cited: BTreeSet<&str> = BTreeSet::new();
    for (i, s) in steps.iter().enumerate() {
        let fail = |reason: Reason, premise: Option<usize>| {
            Box::new(Outcome::Rejected(Rejected {
                first_rejected: Some(i + 1),
                rule: Some(s.rule.to_string()),
                premise,
                conclusion: Some(spell(&s.conclusion)),
                premises: s.premises.iter().map(spell).collect(),
                ..reject(reason)
            }))
        };
        let Some(rule) = rules.iter().find(|r| r.name == s.rule) else { return Err(fail(Reason::UnknownRule, None)) };
        if rule.body.len() != s.premises.len() {
            return Err(fail(Reason::Arity, None));
        }
        let mut binding = Vec::new();
        for (j, (atom, fact)) in rule.body.iter().zip(&s.premises).enumerate() {
            if !unify(atom, fact, &mut binding) {
                return Err(fail(Reason::PremiseMismatch, Some(j + 1)));
            }
        }
        let head =
            rule.head.map(|t| if is_var(t) { binding.iter().find(|(n, _)| *n == t).map_or(t, |b| b.1) } else { t });
        if head != s.conclusion {
            return Err(fail(Reason::ConclusionMismatch, None));
        }
        if let Some(j) = s.premises.iter().position(|p| !known.contains(p)) {
            return Err(fail(Reason::PremiseNotKnown, Some(j + 1)));
        }
        known.insert(s.conclusion);
        cited.insert(s.rule);
    }

    let builtin_cited = cited.iter().filter(|n| n.starts_with(BUILTIN_PREFIX)).count();
    let supplied_cited = cited.len() - builtin_cited;
    let verdict = if supplied_cited == 0 { Verdict::Entailed } else { Verdict::EntailedUnderSuppliedRules };
    Ok(Accepted {
        verdict,
        asserted: n_asserted,
        derivations: n_steps,
        builtin_cited,
        supplied_cited,
        rules_sha256,
        checked: Checked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASSERTED: &str = "#coffee\textends\t#drink\n#espresso\textends\t#coffee\n<cup:my>\ta\t#espresso\n";
    const DERIVED: &str = "\
builtin/extends_transitive\t#espresso\textends\t#drink\t#espresso\textends\t#coffee\t#coffee\textends\t#drink
builtin/type_extends\t<cup:my>\ta\t#coffee\t<cup:my>\ta\t#espresso\t#espresso\textends\t#coffee
builtin/type_extends\t<cup:my>\ta\t#drink\t<cup:my>\ta\t#coffee\t#coffee\textends\t#drink
";

    fn files(derivations: &str) -> CertFiles {
        CertFiles {
            rules: BUILTIN_RULES.into(),
            asserted: ASSERTED.into(),
            derivations: derivations.into(),
            manifest: None,
        }
    }

    fn manifest_for(f: &CertFiles) -> String {
        json!({
            "format": FORMAT,
            "derived": {
                "asserted_sha256": sha256_hex(f.asserted.as_bytes()),
                "rules_sha256": sha256_hex(f.rules.as_bytes()),
                "derivations_sha256": sha256_hex(f.derivations.as_bytes()),
                "asserted": parse::asserted(&f.asserted).unwrap().len(),
                "rules": parse::rules(&f.rules).unwrap().len(),
                "derivations": parse::derivations(&f.derivations).unwrap().len(),
            },
            "attested": {"engine": "onto-intelli 0.2.0"},
        })
        .to_string()
    }

    fn rejected(r: &Report) -> &Rejected {
        r.rejected().unwrap_or_else(|| panic!("expected a rejection: {}", r.to_json()))
    }

    #[test]
    fn builtin_table_is_five_well_formed_lines() {
        let rules = parse::rules(BUILTIN_RULES).unwrap();
        let names: Vec<&str> = rules.iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            [
                "builtin/type_extends",
                "builtin/extends_transitive",
                "builtin/type_implements",
                "builtin/transitive",
                "builtin/symmetric"
            ]
        );
        assert!(BUILTIN_RULES.ends_with('\n') && !BUILTIN_RULES.contains("\n\n") && !BUILTIN_RULES.contains(' '));
        assert_eq!(rules[3].body.len(), 3);
        assert!(rules.iter().all(|r| builtin_line(r.name) == Some(r.text)));
    }

    #[test]
    fn accepts_a_good_certificate() {
        let mut f = files(DERIVED);
        f.manifest = Some(manifest_for(&f));
        let r = check(&f);
        assert_eq!(r.exit_code(), 0, "{}", r.to_json());
        let a = r.accepted().unwrap();
        assert_eq!(
            (a.verdict, a.asserted, a.derivations, a.builtin_cited, a.supplied_cited),
            (Verdict::Entailed, 3, 3, 2, 0)
        );
        let j = r.to_json();
        assert_eq!(j["verdict"], "entailed");
        assert_eq!(j["rules_sha256"], sha256_hex(BUILTIN_RULES.as_bytes()));
        assert_eq!(j["checker"], format!("onto-cert {VERSION}"));
    }

    #[test]
    fn no_derivations_is_vacuously_entailed() {
        let r = check(&files(""));
        assert_eq!(r.verdict(), Some(Verdict::Entailed));
        assert_eq!(r.to_json()["derivations"], 0);
    }

    #[test]
    fn rejects_an_appended_forged_conclusion() {
        let forged = format!(
            "{DERIVED}builtin/type_extends\t<cup:my>\ta\t#beer\t<cup:my>\ta\t#coffee\t#coffee\textends\t#drink\n"
        );
        let r = check(&files(&forged));
        let x = rejected(&r);
        assert_eq!(
            (x.reason, x.first_rejected, x.rule.as_deref()),
            (Reason::ConclusionMismatch, Some(4), Some("builtin/type_extends"))
        );
        let j = r.to_json();
        assert_eq!(j["conclusion"], "<cup:my> a #beer");
        assert_eq!(j["premises"], json!(["<cup:my> a #coffee", "#coffee extends #drink"]));
        assert_eq!(r.exit_code(), 1);
    }

    #[test]
    fn rejects_a_premise_outside_the_graph() {
        let forged =
            format!("{DERIVED}builtin/type_extends\t<cup:my>\ta\t#beer\t<cup:my>\ta\t#drink\t#drink\textends\t#beer\n");
        let x = check(&files(&forged)).rejected().cloned().unwrap();
        assert_eq!((x.reason, x.first_rejected, x.premise), (Reason::PremiseNotKnown, Some(4), Some(2)));
    }

    #[test]
    fn rejects_a_reordered_premise() {
        let swapped = DERIVED.replace(
            "<cup:my>\ta\t#espresso\t#espresso\textends\t#coffee",
            "#espresso\textends\t#coffee\t<cup:my>\ta\t#espresso",
        );
        let x = check(&files(&swapped)).rejected().cloned().unwrap();
        assert_eq!((x.reason, x.first_rejected, x.premise), (Reason::PremiseMismatch, Some(2), Some(1)));
    }

    #[test]
    fn rejects_an_inconsistent_binding() {
        // ?k is bound to #espresso by premise 1 and must be #espresso in premise 2.
        let bad = "builtin/type_extends\t<cup:my>\ta\t#drink\t<cup:my>\ta\t#espresso\t#coffee\textends\t#drink\n";
        let x = check(&files(bad)).rejected().cloned().unwrap();
        assert_eq!((x.reason, x.premise), (Reason::PremiseMismatch, Some(2)));
    }

    #[test]
    fn rejects_a_forged_builtin_rule() {
        // A symmetric rule that drops its `?r a symmetric` guard.
        let mut f = files(DERIVED);
        f.rules = BUILTIN_RULES.replace(
            "builtin/symmetric\t?y\t?r\t?x\t?r\ta\tsymmetric\t?x\t?r\t?y",
            "builtin/symmetric\t?y\t?r\t?x\t?x\t?r\t?y",
        );
        let x = check(&f).rejected().cloned().unwrap();
        assert_eq!(
            (x.reason, x.rule.as_deref(), x.line),
            (Reason::ForgedBuiltinRule, Some("builtin/symmetric"), Some(5))
        );
        f.rules = format!("{BUILTIN_RULES}builtin/everything\t?x\ta\t?y\t?x\ta\t?y\n");
        assert_eq!(rejected(&check(&f)).reason, Reason::ForgedBuiltinRule);
    }

    #[test]
    fn rejects_a_self_citing_line() {
        let selfish =
            "builtin/extends_transitive\t#coffee\textends\t#drink\t#coffee\textends\t#drink\t#drink\textends\t#drink\n";
        let mut f = files(selfish);
        f.asserted = "#drink\textends\t#drink\n".into();
        let x = check(&f).rejected().cloned().unwrap();
        assert_eq!((x.reason, x.first_rejected, x.premise), (Reason::PremiseNotKnown, Some(1), Some(1)));
    }

    #[test]
    fn rejects_a_premise_concluded_by_a_later_line() {
        let lines: Vec<&str> = DERIVED.lines().collect();
        let reordered = format!("{}\n{}\n{}\n", lines[0], lines[2], lines[1]);
        let x = check(&files(&reordered)).rejected().cloned().unwrap();
        assert_eq!((x.reason, x.first_rejected, x.premise), (Reason::PremiseNotKnown, Some(2), Some(1)));
    }

    #[test]
    fn rejects_unknown_rules_and_wrong_arity() {
        let x = check(&files("schema/nope\ta\tp\tb\ta\tq\tb\n")).rejected().cloned().unwrap();
        assert_eq!(x.reason, Reason::UnknownRule);
        let x =
            check(&files("builtin/type_extends\t<cup:my>\ta\t#coffee\t<cup:my>\ta\t#espresso\n")).rejected().cloned();
        assert_eq!(x.unwrap().reason, Reason::Arity);
    }

    #[test]
    fn rejects_a_digest_mismatch() {
        let mut f = files(DERIVED);
        f.manifest = Some(manifest_for(&f));
        f.asserted.push_str("<cup:other>\ta\t#espresso\n");
        let r = check(&f);
        let x = rejected(&r);
        assert_eq!((x.reason, x.field.as_deref()), (Reason::DigestMismatch, Some("asserted_sha256")));
        assert_eq!(r.to_json()["field"], "asserted_sha256");

        // Right digests, wrong count.
        let mut f = files(DERIVED);
        let mut m: Value = serde_json::from_str(&manifest_for(&f)).unwrap();
        m["derived"]["derivations"] = 4.into();
        f.manifest = Some(m.to_string());
        let x = check(&f).rejected().cloned().unwrap();
        assert_eq!((x.field.as_deref(), x.expected, x.actual), (Some("derivations"), Some(4.into()), Some(3.into())));
    }

    #[test]
    fn a_cited_supplied_rule_earns_the_relative_verdict() {
        let rule = "schema/blocks_release\t?x0\tblocks_release\t?x2\t?x0\tdepends_on\t?x1\t?x1\tpart_of\t?x2\n";
        let f = CertFiles {
            rules: format!("{BUILTIN_RULES}{rule}"),
            asserted: "<issue:1>\tdepends_on\t<issue:2>\n<issue:2>\tpart_of\t<release:r1>\n".into(),
            derivations: "schema/blocks_release\t<issue:1>\tblocks_release\t<release:r1>\t<issue:1>\tdepends_on\t<issue:2>\t<issue:2>\tpart_of\t<release:r1>\n".into(),
            manifest: None,
        };
        let r = check(&f);
        assert_eq!(r.verdict(), Some(Verdict::EntailedUnderSuppliedRules), "{}", r.to_json());
        let j = r.to_json();
        assert_eq!(j["rules_cited"], json!({"builtin": 0, "supplied": 1}));
        assert!(j["means"].as_str().unwrap().contains("assumptions"));

        // A supplied rule that is present but never cited does not weaken the verdict.
        let mut g = files(DERIVED);
        g.rules = format!("{BUILTIN_RULES}{rule}");
        assert_eq!(check(&g).verdict(), Some(Verdict::Entailed));
    }

    #[test]
    fn malformed_inputs_name_file_and_line() {
        let mut f = files("builtin/type_extends\t<cup:my>\ta\n");
        let r = check(&f);
        assert_eq!(r.exit_code(), 2);
        let j = r.to_json();
        assert_eq!(
            (j["error"].as_str(), j["file"].as_str(), j["line"].as_u64()),
            (Some("malformed"), Some(DERIVATIONS_FILE), Some(1))
        );
        f.derivations = DERIVED.into();
        f.manifest = Some("not json".into());
        assert_eq!(check(&f).malformed().unwrap().file, MANIFEST_FILE);
        f.manifest = None;
        f.rules = format!("{BUILTIN_RULES}{BUILTIN_RULES}");
        assert!(check(&f).malformed().unwrap().reason.contains("duplicate"));
    }

    #[test]
    fn a_missing_final_newline_is_accepted() {
        let mut f = files(DERIVED.trim_end());
        f.asserted = ASSERTED.trim_end().into();
        f.rules = BUILTIN_RULES.trim_end().into();
        assert_eq!(check(&f).verdict(), Some(Verdict::Entailed));
    }

    /// The checker must stay independent of the engine: no onto-* crate and no
    /// path dependency in any dependency table (normal, dev, build or
    /// target-specific), and only sha2 and serde_json under `[dependencies]`.
    #[test]
    fn no_engine_dependency() {
        let manifest = include_str!("../Cargo.toml");
        let workspace = include_str!("../../../Cargo.toml");
        let workspace = &workspace[workspace.find("[workspace.dependencies]").expect("workspace dependency table")..];
        let mut section = "";
        let mut deps = 0;
        for line in manifest.lines().map(str::trim) {
            if line.starts_with('[') {
                section = line;
            }
            let dep_table = section.ends_with("dependencies]") || section.contains("dependencies.");
            if !dep_table || line.is_empty() || line.starts_with('#') {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                !lower.contains("onto-") && !lower.contains("onto_") && !compact.contains("path="),
                "onto-cert must not depend on the engine: {section} {line}"
            );
            if section == "[dependencies]" && line != section {
                deps += 1;
            }
            // A `name.workspace = true` entry inherits its source from the
            // workspace table, so that definition must not be a path either.
            if compact.contains("workspace=true") {
                let name = compact.split(['.', '=']).next().unwrap_or_default();
                let def = workspace
                    .lines()
                    .map(str::trim)
                    .find(|l| l.split(['.', '=', ' ']).next() == Some(name))
                    .unwrap_or_else(|| panic!("{name} is not in [workspace.dependencies]"));
                assert!(!def.replace(' ', "").contains("path="), "{name} is a path dependency: {def}");
            }
        }
        assert_eq!(deps, 2, "only sha2 and serde_json");
    }
}
