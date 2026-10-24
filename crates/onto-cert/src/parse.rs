//! Parsing of the certificate files of format `oi-cert/1`.
//!
//! This module reads text and returns borrowed terms. It checks only the
//! shape of each line: field counts, empty terms, variable names, head
//! variables that do not occur in the body and duplicate rule names. It gives
//! no meaning to a term beyond "variable" (starts with `?`) or "constant".
//! A failure is a [`Malformed`] value naming the file and the 1-based line.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::Malformed;

/// File name of the rule table.
pub const RULES_FILE: &str = "rules.tsv";
/// File name of the asserted facts.
pub const ASSERTED_FILE: &str = "asserted.tsv";
/// File name of the derivation lines.
pub const DERIVATIONS_FILE: &str = "derivations.tsv";
/// File name of the manifest.
pub const MANIFEST_FILE: &str = "manifest.json";

/// A triple of spelled terms: subject, predicate, object.
pub(crate) type Triple<'a> = [&'a str; 3];

/// One line of `rules.tsv`.
#[derive(Debug)]
pub(crate) struct Rule<'a> {
    pub name: &'a str,
    pub line: usize,
    /// The whole line without its newline, for the byte comparison of built-in rules.
    pub text: &'a str,
    pub head: Triple<'a>,
    pub body: Vec<Triple<'a>>,
}

/// One line of `derivations.tsv`.
#[derive(Debug)]
pub(crate) struct Step<'a> {
    pub rule: &'a str,
    pub conclusion: Triple<'a>,
    pub premises: Vec<Triple<'a>>,
}

/// The `derived` block of `manifest.json`: values the checker recomputes.
#[derive(Debug)]
pub(crate) struct Manifest {
    pub asserted_sha256: String,
    pub rules_sha256: String,
    pub derivations_sha256: String,
    pub asserted: u64,
    pub rules: u64,
    pub derivations: u64,
}

fn malformed(file: &'static str, line: usize, reason: impl Into<String>) -> Malformed {
    Malformed { file, line, reason: reason.into() }
}

/// The lines of a file with their 1-based numbers. Every line ends with `\n`;
/// a last line without one is accepted. An empty file has no lines.
fn lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    let take = if text.is_empty() { 0 } else { usize::MAX };
    body.split('\n').take(take).enumerate().map(|(i, l)| (i + 1, l))
}

/// Splits a line on TAB and checks that no field is empty or holds a CR.
fn fields<'a>(file: &'static str, n: usize, line: &'a str) -> Result<Vec<&'a str>, Malformed> {
    if line.is_empty() {
        return Err(malformed(file, n, "empty line"));
    }
    let fs: Vec<&str> = line.split('\t').collect();
    for (k, f) in fs.iter().enumerate() {
        if f.is_empty() {
            return Err(malformed(file, n, format!("empty term in field {}", k + 1)));
        }
        if f.contains('\r') {
            return Err(malformed(file, n, format!("carriage return in field {}", k + 1)));
        }
    }
    Ok(fs)
}

pub(crate) fn is_var(t: &str) -> bool {
    t.starts_with('?')
}

/// A term of an asserted fact, a conclusion or a premise: never a variable.
fn ground(file: &'static str, n: usize, t: &str) -> Result<(), Malformed> {
    if is_var(t) {
        return Err(malformed(file, n, format!("rule variable `{t}` in a ground fact")));
    }
    Ok(())
}

fn triple<'a>(fs: &[&'a str]) -> Triple<'a> {
    [fs[0], fs[1], fs[2]]
}

/// Parses `asserted.tsv`: `s<TAB>p<TAB>o` per line.
pub(crate) fn asserted(text: &str) -> Result<Vec<Triple<'_>>, Malformed> {
    let f = ASSERTED_FILE;
    let mut out = Vec::new();
    for (n, line) in lines(text) {
        let fs = fields(f, n, line)?;
        if fs.len() != 3 {
            return Err(malformed(f, n, format!("expected 3 fields (subject, predicate, object), found {}", fs.len())));
        }
        for t in &fs {
            ground(f, n, t)?;
        }
        out.push(triple(&fs));
    }
    Ok(out)
}

/// Parses `rules.tsv`: `name`, the head atom, then at least one body atom.
pub(crate) fn rules(text: &str) -> Result<Vec<Rule<'_>>, Malformed> {
    let f = RULES_FILE;
    let mut out: Vec<Rule> = Vec::new();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (n, line) in lines(text) {
        let fs = fields(f, n, line)?;
        if fs.len() < 7 || (fs.len() - 1) % 3 != 0 {
            return Err(malformed(
                f,
                n,
                format!("expected a name, a head atom and at least one body atom (1 + 3k fields), found {}", fs.len()),
            ));
        }
        for t in &fs[1..] {
            if *t == "?" {
                return Err(malformed(f, n, "variable without a name"));
            }
        }
        let name = fs[0];
        if let Some(first) = seen.insert(name, n) {
            return Err(malformed(f, n, format!("duplicate rule name `{name}` (first on line {first})")));
        }
        let head = triple(&fs[1..4]);
        let body: Vec<Triple> = fs[4..].chunks(3).map(triple).collect();
        for t in head {
            if is_var(t) && !body.iter().any(|a| a.contains(&t)) {
                return Err(malformed(f, n, format!("head variable `{t}` does not occur in the body")));
            }
        }
        out.push(Rule { name, line: n, text: line, head, body });
    }
    Ok(out)
}

/// Parses `derivations.tsv`: rule name, conclusion, then the premises.
pub(crate) fn derivations(text: &str) -> Result<Vec<Step<'_>>, Malformed> {
    let f = DERIVATIONS_FILE;
    let mut out = Vec::new();
    for (n, line) in lines(text) {
        let fs = fields(f, n, line)?;
        if fs.len() < 4 || (fs.len() - 1) % 3 != 0 {
            return Err(malformed(
                f,
                n,
                format!("expected a rule name, a conclusion and premises (1 + 3k fields), found {}", fs.len()),
            ));
        }
        for t in &fs[1..] {
            ground(f, n, t)?;
        }
        let conclusion = triple(&fs[1..4]);
        let premises = fs[4..].chunks(3).map(triple).collect();
        out.push(Step { rule: fs[0], conclusion, premises });
    }
    Ok(out)
}

/// Parses `manifest.json` and reads its `derived` block. The `attested`
/// block is the engine's claim and is not read.
pub(crate) fn manifest(text: &str) -> Result<Manifest, Malformed> {
    let f = MANIFEST_FILE;
    let v: Value = serde_json::from_str(text).map_err(|e| malformed(f, e.line(), format!("invalid JSON: {e}")))?;
    match v.get("format").and_then(Value::as_str) {
        Some(crate::FORMAT) => {}
        Some(other) => {
            return Err(malformed(f, 0, format!("unsupported format `{other}`, expected {}", crate::FORMAT)));
        }
        None => return Err(malformed(f, 0, "missing string field `format`")),
    }
    let Some(d) = v.get("derived").filter(|d| d.is_object()) else {
        return Err(malformed(f, 0, "missing object field `derived`"));
    };
    let s = |k: &str| {
        d.get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| malformed(f, 0, format!("missing string field `derived.{k}`")))
    };
    let u = |k: &str| {
        d.get(k).and_then(Value::as_u64).ok_or_else(|| malformed(f, 0, format!("missing integer field `derived.{k}`")))
    };
    Ok(Manifest {
        asserted_sha256: s("asserted_sha256")?,
        rules_sha256: s("rules_sha256")?,
        derivations_sha256: s("derivations_sha256")?,
        asserted: u("asserted")?,
        rules: u("rules")?,
        derivations: u("derivations")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_accept_a_missing_final_newline() {
        assert_eq!(lines("").count(), 0);
        assert_eq!(lines("a\nb\n").collect::<Vec<_>>(), vec![(1, "a"), (2, "b")]);
        assert_eq!(lines("a\nb").collect::<Vec<_>>(), vec![(1, "a"), (2, "b")]);
        assert_eq!(lines("\n").collect::<Vec<_>>(), vec![(1, "")]);
    }

    #[test]
    fn asserted_needs_three_ground_terms() {
        assert_eq!(asserted("<a:1>\ta\t#k\n").unwrap(), vec![["<a:1>", "a", "#k"]]);
        assert_eq!(asserted("<a:1>\ta\n").unwrap_err().line, 1);
        assert!(asserted("x\ty\tz\n<a:1>\t\t#k\n").unwrap_err().reason.contains("empty term in field 2"));
        assert!(asserted("?x\ta\t#k\n").unwrap_err().reason.contains("variable"));
        assert!(asserted("x\ty\tz\r\n").unwrap_err().reason.contains("carriage return"));
        assert!(asserted("x\ty\tz\n\nx\ty\tw\n").unwrap_err().reason.contains("empty line"));
    }

    #[test]
    fn rules_check_shape_head_variables_and_names() {
        let ok = rules("r\t?x\tp\t?y\t?x\tq\t?y\n").unwrap();
        assert_eq!(ok[0].name, "r");
        assert_eq!(ok[0].body, vec![["?x", "q", "?y"]]);
        assert!(rules("r\t?x\tp\t?y\n").unwrap_err().reason.contains("found 4"));
        assert!(rules("r\t?x\tp\t?y\t?x\tq\t?y\t?z\n").is_err());
        let e = rules("r\t?x\tp\t?z\t?x\tq\t?y\n").unwrap_err();
        assert!(e.reason.contains("`?z` does not occur"), "{e:?}");
        let e = rules("r\t?x\tp\t?y\t?x\tq\t?y\nr\t?x\tp\t?y\t?x\tq\t?y\n").unwrap_err();
        assert_eq!((e.line, e.reason.contains("duplicate")), (2, true));
        assert!(rules("r\t?\tp\tc\t?\tq\tc\n").unwrap_err().reason.contains("without a name"));
    }

    #[test]
    fn derivations_need_ground_triples() {
        let s = derivations("r\ta\tp\tb\ta\tq\tb\n").unwrap();
        assert_eq!((s[0].rule, s[0].conclusion, s[0].premises.len()), ("r", ["a", "p", "b"], 1));
        assert_eq!(derivations("r\ta\tp\tb\n").unwrap()[0].premises.len(), 0);
        assert!(derivations("r\ta\tp\n").is_err());
        assert!(derivations("r\ta\tp\tb\ta\tq\n").is_err());
        assert!(derivations("r\ta\tp\t?b\ta\tq\tb\n").unwrap_err().reason.contains("variable"));
    }

    #[test]
    fn manifest_reads_the_derived_block() {
        let m = manifest(
            r#"{"format":"oi-cert/1","derived":{"asserted_sha256":"a","rules_sha256":"r","derivations_sha256":"d",
            "asserted":1,"rules":2,"derivations":3},"attested":{"engine":"anything"}}"#,
        )
        .unwrap();
        assert_eq!((m.asserted, m.rules, m.derivations, m.rules_sha256.as_str()), (1, 2, 3, "r"));
        assert!(manifest("{").is_err());
        assert!(manifest(r#"{"format":"oi-cert/2","derived":{}}"#).unwrap_err().reason.contains("unsupported"));
        assert!(manifest(r#"{"format":"oi-cert/1","derived":{"asserted_sha256":1}}"#).is_err());
    }
}
