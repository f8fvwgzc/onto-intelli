//! The `onto-cert` command: checks an onto-intelli reasoning certificate.
//!
//! `onto-cert check <dir>` reads the certificate files from a directory,
//! prints the JSON report on stdout and exits 0 (accepted), 1 (rejected) or
//! 2 (malformed or unreadable). `onto-cert table` prints the compiled-in
//! built-in rule table and `onto-cert --version` the checker's version.

use std::path::Path;
use std::process::ExitCode;

use onto_cert::{BUILTIN_RULES, CertFiles, VERSION, check};

const USAGE: &str = "\
usage: onto-cert check <dir>   check the certificate in <dir> (rules.tsv, asserted.tsv,
                               derivations.tsv, optional manifest.json); prints a JSON report,
                               exits 0 accepted, 1 rejected, 2 malformed
       onto-cert table         print the built-in rule table
       onto-cert --version     print the checker's version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["check", dir] => {
            let (report, code) = match CertFiles::from_dir(Path::new(dir)) {
                Ok(files) => {
                    let r = check(&files);
                    (r.to_json(), r.exit_code())
                }
                Err(e) => (
                    serde_json::json!({"ok": false, "error": "unreadable", "file": e.file, "reason": e.error.to_string()}),
                    2,
                ),
            };
            println!("{}", serde_json::to_string_pretty(&report).expect("a JSON value serializes"));
            ExitCode::from(code as u8)
        }
        ["--version"] => {
            println!("onto-cert {VERSION}");
            ExitCode::SUCCESS
        }
        ["table"] => {
            print!("{BUILTIN_RULES}");
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
