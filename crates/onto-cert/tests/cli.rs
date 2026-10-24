//! The `onto-cert` binary: JSON on stdout and exit codes 0, 1 and 2.

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_onto-cert")).args(args).output().expect("run onto-cert")
}

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name).display().to_string()
}

/// Runs `onto-cert check <fixture>` and returns the exit code and the parsed report.
fn check(name: &str) -> (i32, Value) {
    let out = run(&["check", &fixture(name)]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let report: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{name}: {e}: {stdout}"));
    (out.status.code().expect("exit code"), report)
}

#[test]
fn coffee_exits_0_with_entailed() {
    let (code, r) = check("coffee");
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["ok"], true);
    assert_eq!(r["verdict"], "entailed");
    assert_eq!((r["asserted"].as_u64(), r["derivations"].as_u64()), (Some(3), Some(3)));
    assert_eq!(r["checker"], format!("onto-cert {}", env!("CARGO_PKG_VERSION")));
    assert_eq!(r["rules_sha256"].as_str().map(str::len), Some(64));
}

#[test]
fn forged_coffee_exits_1_naming_line_and_rule() {
    let (code, r) = check("coffee-forged");
    assert_eq!(code, 1, "{r}");
    assert_eq!(r["ok"], false);
    assert_eq!(r["first_rejected"], 3);
    assert_eq!(r["rule"], "builtin/type_extends");
    assert_eq!(r["reason"], "conclusion_mismatch", "{r}");
    assert_eq!(r["conclusion"], "<cup:my> a #beer");
    assert_eq!(r["premises"].as_array().map(Vec::len), Some(2));
}

#[test]
fn transitive_chain_exits_0() {
    let (code, r) = check("transitive-chain");
    assert_eq!((code, r["verdict"].as_str()), (0, Some("entailed")), "{r}");
}

#[test]
fn supplied_chain_exits_0_with_the_relative_verdict() {
    let (code, r) = check("supplied-chain");
    assert_eq!((code, r["verdict"].as_str()), (0, Some("entailed_under_supplied_rules")), "{r}");
    assert_eq!(r["rules_cited"]["supplied"], 1);
    assert!(r["means"].as_str().unwrap().contains("assumptions"), "{r}");
}

#[test]
fn digest_mismatch_and_forged_builtin_exit_1() {
    let (code, r) = check("digest-mismatch");
    assert_eq!(
        (code, r["reason"].as_str(), r["field"].as_str()),
        (1, Some("digest_mismatch"), Some("asserted_sha256"))
    );
    let (code, r) = check("forged-builtin");
    assert_eq!(
        (code, r["reason"].as_str(), r["rule"].as_str()),
        (1, Some("forged_builtin_rule"), Some("builtin/transitive"))
    );
}

#[test]
fn malformed_inputs_exit_2() {
    for (name, file) in [
        ("malformed-rules", "rules.tsv"),
        ("malformed-asserted", "asserted.tsv"),
        ("malformed-derivations", "derivations.tsv"),
        ("malformed-manifest", "manifest.json"),
    ] {
        let (code, r) = check(name);
        assert_eq!(code, 2, "{name}: {r}");
        assert_eq!(
            (r["ok"].as_bool(), r["error"].as_str(), r["file"].as_str()),
            (Some(false), Some("malformed"), Some(file))
        );
        assert!(r["line"].is_u64() && r["reason"].is_string(), "{r}");
    }
    let (code, r) = check("missing-derivations");
    assert_eq!((code, r["error"].as_str(), r["file"].as_str()), (2, Some("unreadable"), Some("derivations.tsv")));
    let (code, r) = check("no-such-dir");
    assert_eq!((code, r["error"].as_str()), (2, Some("unreadable")));
}

#[test]
fn version_and_table() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), format!("onto-cert {}\n", env!("CARGO_PKG_VERSION")));
    let out = run(&["table"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), onto_cert::BUILTIN_RULES);
}

#[test]
fn anything_else_prints_usage_and_exits_2() {
    for args in [&[][..], &["check"], &["check", "a", "b"], &["help"], &["--verbose"]] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(String::from_utf8(out.stderr).unwrap().starts_with("usage: onto-cert"), "{args:?}");
    }
}
