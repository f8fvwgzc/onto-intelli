//! Library checks against the fixture certificates and a property test over
//! random small graphs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use onto_cert::{BUILTIN_RULES, CertFiles, Reason, Verdict, check};

fn fixture(name: &str) -> CertFiles {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    CertFiles::from_dir(&dir).unwrap_or_else(|e| panic!("{name}: {e}: {}", e.error))
}

/// `Display` names the file and `source()` holds the cause, so a reporter that
/// walks the chain prints each of them once.
#[test]
fn an_unreadable_certificate_reports_the_file_and_then_the_cause() {
    use std::error::Error;
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/no-such-certificate");
    let e = CertFiles::from_dir(&dir).expect_err("the directory does not exist");
    assert_eq!(e.to_string(), "could not read rules.tsv");
    let cause = e.source().expect("the cause is the source").to_string();
    assert_eq!(cause, e.error.to_string());
    assert!(!e.to_string().contains(&cause), "{e}");
}

#[test]
fn coffee_is_entailed() {
    let f = fixture("coffee");
    assert!(f.manifest.is_some());
    let r = check(&f);
    assert_eq!(r.verdict(), Some(Verdict::Entailed), "{}", r.to_json());
    let a = r.accepted().unwrap();
    assert_eq!((a.asserted(), a.derivations(), a.builtin_cited(), a.supplied_cited()), (3, 3, 2, 0));
    // The three conclusions of the example.
    for fact in ["<cup:my>\ta\t#coffee", "<cup:my>\ta\t#drink", "#espresso\textends\t#drink"] {
        assert!(f.derivations.lines().any(|l| l.split('\t').skip(1).take(3).collect::<Vec<_>>().join("\t") == fact));
    }
}

#[test]
fn coffee_forged_to_beer_is_rejected_at_its_line() {
    let r = check(&fixture("coffee-forged"));
    let x = r.rejected().unwrap_or_else(|| panic!("{}", r.to_json()));
    assert_eq!(x.first_rejected, Some(3));
    assert_eq!(x.reason, Reason::ConclusionMismatch);
    assert_eq!(x.rule.as_deref(), Some("builtin/type_extends"));
    assert_eq!(x.conclusion.as_deref(), Some("<cup:my> a #beer"));
    assert_eq!(r.exit_code(), 1);
}

#[test]
fn transitive_chain_is_entailed() {
    let r = check(&fixture("transitive-chain"));
    assert_eq!(r.verdict(), Some(Verdict::Entailed), "{}", r.to_json());
    assert_eq!(r.to_json()["rules_cited"]["builtin"], 1);
}

#[test]
fn interfaces_and_symmetric_relations_are_entailed() {
    let r = check(&fixture("interfaces-symmetric"));
    assert_eq!(r.verdict(), Some(Verdict::Entailed), "{}", r.to_json());
    assert_eq!(r.to_json()["rules_cited"]["builtin"], 2);
}

#[test]
fn supplied_chain_rule_is_entailed_under_supplied_rules() {
    let r = check(&fixture("supplied-chain"));
    assert_eq!(r.verdict(), Some(Verdict::EntailedUnderSuppliedRules), "{}", r.to_json());
    let j = r.to_json();
    assert_eq!(j["verdict"], "entailed_under_supplied_rules");
    assert_eq!(j["rules_cited"], serde_json::json!({"builtin": 1, "supplied": 1}));
}

#[test]
fn horn_rule_with_a_literal_is_entailed_under_supplied_rules() {
    let r = check(&fixture("horn-literal"));
    assert_eq!(r.verdict(), Some(Verdict::EntailedUnderSuppliedRules), "{}", r.to_json());
}

#[test]
fn digest_and_builtin_forgeries_are_rejected() {
    let x = check(&fixture("digest-mismatch")).rejected().cloned().unwrap();
    assert_eq!((x.reason, x.field.as_deref()), (Reason::DigestMismatch, Some("asserted_sha256")));
    let x = check(&fixture("forged-builtin")).rejected().cloned().unwrap();
    assert_eq!((x.reason, x.rule.as_deref()), (Reason::ForgedBuiltinRule, Some("builtin/transitive")));
}

#[test]
fn malformed_fixtures_exit_2() {
    for (name, file) in [
        ("malformed-rules", "rules.tsv"),
        ("malformed-asserted", "asserted.tsv"),
        ("malformed-derivations", "derivations.tsv"),
        ("malformed-manifest", "manifest.json"),
    ] {
        let r = check(&fixture(name));
        assert_eq!(r.exit_code(), 2, "{name}: {}", r.to_json());
        assert_eq!(r.malformed().unwrap().file, file, "{name}");
    }
}

// ---------------------------------------------------------------------------
// Property test: random small graphs, a reference closure written here, and
// one-byte forgeries of conclusions.

type Fact = [String; 3];

/// One rule of the built-in table, read from its text.
struct TestRule {
    name: String,
    head: [String; 3],
    body: Vec<[String; 3]>,
}

fn table() -> Vec<TestRule> {
    BUILTIN_RULES
        .lines()
        .map(|l| {
            let f: Vec<String> = l.split('\t').map(str::to_string).collect();
            let tri = |c: &[String]| [c[0].clone(), c[1].clone(), c[2].clone()];
            TestRule { name: f[0].clone(), head: tri(&f[1..4]), body: f[4..].chunks(3).map(tri).collect() }
        })
        .collect()
}

/// Every binding of `body[i..]` against `facts`, extending `b`.
fn matches(
    body: &[[String; 3]],
    facts: &BTreeSet<Fact>,
    b: &BTreeMap<String, String>,
    out: &mut Vec<(BTreeMap<String, String>, Vec<Fact>)>,
    used: &mut Vec<Fact>,
) {
    let Some((atom, rest)) = body.split_first() else {
        out.push((b.clone(), used.clone()));
        return;
    };
    'facts: for f in facts {
        let mut nb = b.clone();
        for (t, v) in atom.iter().zip(f) {
            if t.starts_with('?') {
                match nb.get(t) {
                    Some(x) if x != v => continue 'facts,
                    Some(_) => {}
                    None => {
                        nb.insert(t.clone(), v.clone());
                    }
                }
            } else if t != v {
                continue 'facts;
            }
        }
        used.push(f.clone());
        matches(rest, facts, &nb, out, used);
        used.pop();
    }
}

/// Naive rounds: every new conclusion of a round uses only facts known at the
/// start of the round, so the lines come out in a valid certificate order.
fn closure(asserted: &BTreeSet<Fact>, rules: &[TestRule]) -> Vec<(String, Fact, Vec<Fact>)> {
    let mut known = asserted.clone();
    let mut lines = Vec::new();
    loop {
        let mut round: BTreeMap<Fact, (String, Vec<Fact>)> = BTreeMap::new();
        for r in rules {
            let mut out = Vec::new();
            matches(&r.body, &known, &BTreeMap::new(), &mut out, &mut Vec::new());
            for (b, premises) in out {
                let c = r.head.clone().map(|t| if t.starts_with('?') { b[&t].clone() } else { t });
                if !known.contains(&c) {
                    round.entry(c).or_insert((r.name.clone(), premises));
                }
            }
        }
        if round.is_empty() {
            return lines;
        }
        for (c, (rule, premises)) in round {
            known.insert(c.clone());
            lines.push((rule, c, premises));
        }
    }
}

fn random_graph(rng: &mut fastrand::Rng) -> BTreeSet<Fact> {
    let f = |s: String, p: &str, o: String| [s, p.to_string(), o];
    let kinds = rng.usize(2..5);
    let ifaces = rng.usize(0..3);
    let rels = rng.usize(1..4);
    let ents = rng.usize(1..6);
    let mut g = BTreeSet::new();
    for _ in 0..rng.usize(0..kinds + 2) {
        g.insert(f(format!("#k{}", rng.usize(0..kinds)), "extends", format!("#k{}", rng.usize(0..kinds))));
    }
    for _ in 0..ifaces * 2 {
        g.insert(f(format!("#k{}", rng.usize(0..kinds)), "implements", format!("#i{}", rng.usize(0..ifaces))));
    }
    for r in 0..rels {
        if rng.bool() {
            g.insert(f(format!("r{r}"), "a", "transitive".into()));
        }
        if rng.bool() {
            g.insert(f(format!("r{r}"), "a", "symmetric".into()));
        }
    }
    for e in 0..ents {
        g.insert(f(format!("<e:{e}>"), "a", format!("#k{}", rng.usize(0..kinds))));
    }
    for _ in 0..rng.usize(0..ents * 2) {
        g.insert(f(
            format!("<e:{}>", rng.usize(0..ents)),
            &format!("r{}", rng.usize(0..rels)),
            format!("<e:{}>", rng.usize(0..ents)),
        ));
    }
    g
}

fn render(asserted: &BTreeSet<Fact>, lines: &[(String, Fact, Vec<Fact>)]) -> CertFiles {
    let asserted: String = asserted.iter().map(|f| format!("{}\n", f.join("\t"))).collect();
    let derivations: String = lines
        .iter()
        .map(|(rule, c, ps)| {
            let mut fs = vec![rule.clone()];
            fs.extend(c.iter().cloned());
            for p in ps {
                fs.extend(p.iter().cloned());
            }
            format!("{}\n", fs.join("\t"))
        })
        .collect();
    CertFiles { rules: BUILTIN_RULES.into(), asserted, derivations, manifest: None }
}

#[test]
fn random_closures_check_and_one_byte_forgeries_do_not() {
    let rules = table();
    let seed = std::env::var("ONTO_CERT_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x5eed_cafe);
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut forged = 0;
    for case in 0..200 {
        let g = random_graph(&mut rng);
        let lines = closure(&g, &rules);
        let files = render(&g, &lines);
        let r = check(&files);
        assert_eq!(
            r.verdict(),
            Some(Verdict::Entailed),
            "case {case} (seed {seed}): {}\n{}",
            r.to_json(),
            files.derivations
        );
        assert_eq!(r.accepted().unwrap().derivations(), lines.len());
        if lines.is_empty() {
            continue;
        }

        // Change one byte of one conclusion term to another letter or digit.
        let i = rng.usize(0..lines.len());
        let mut mutated = lines.clone();
        let term = &mut mutated[i].1[rng.usize(0..3)];
        let mut bytes = term.clone().into_bytes();
        let k = rng.usize(0..bytes.len());
        let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut b = alphabet[rng.usize(0..alphabet.len())];
        while b == bytes[k] {
            b = alphabet[rng.usize(0..alphabet.len())];
        }
        bytes[k] = b;
        *term = String::from_utf8(bytes).unwrap();
        let r = check(&render(&g, &mutated));
        let x = r.rejected().unwrap_or_else(|| panic!("case {case} (seed {seed}): forgery accepted: {}", r.to_json()));
        assert_eq!(x.first_rejected, Some(i + 1), "case {case} (seed {seed}): {}", r.to_json());
        assert_eq!(x.reason, Reason::ConclusionMismatch, "case {case} (seed {seed}): {}", r.to_json());
        forged += 1;
    }
    assert!(forged > 50, "only {forged} cases had derivations");
}
