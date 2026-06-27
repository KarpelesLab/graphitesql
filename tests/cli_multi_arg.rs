//! The `sqlite3` CLI runs each trailing command-line argument as an independent
//! SQL batch: `sqlite3 db "SELECT 1" "SELECT 2"` executes both, even though
//! neither argument ends in `;`. graphite's CLI previously joined the arguments
//! into one string with a single space, so two complete-but-unterminated
//! statements were spliced together (`…VALUES(1),(2)` + `SELECT …` →
//! `…(2)SELECT …`) and reported as a syntax error. Verified against the sqlite3
//! 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run `bin :memory: <args…>` and return stdout (trimmed) or the first non-caret
/// error line with the CLI framing stripped.
fn run(bin: &str, args: &[&str]) -> String {
    let out = Command::new(bin)
        .arg(":memory:")
        .args(args)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        return stdout.trim_end().to_string();
    }
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .find(|l| !l.trim_start().starts_with('^'))
        .unwrap_or("")
        .trim_start_matches("Error: in prepare, ")
        .trim_start_matches("Error: stepping, ")
        .trim_start_matches("Error: ")
        .trim_start_matches("SQL error: ")
        .trim_start_matches("error: ")
        .trim_end()
        .to_string()
}

#[test]
fn each_argument_runs_as_its_own_batch() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Neither argument is `;`-terminated, yet both must run.
    assert_eq!(
        run(
            g,
            &[
                "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2)",
                "SELECT a FROM t"
            ]
        ),
        "1\n2"
    );
    // Three separate arguments, each a single statement.
    assert_eq!(
        run(
            g,
            &[
                "CREATE TABLE t(a)",
                "INSERT INTO t VALUES(7)",
                "SELECT a FROM t"
            ]
        ),
        "7"
    );
    // A single combined argument with internal `;` still works as before.
    assert_eq!(
        run(
            g,
            &["CREATE TABLE t(a); INSERT INTO t VALUES(9); SELECT a FROM t;"]
        ),
        "9"
    );
}

#[test]
fn first_failing_argument_aborts_the_rest() {
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // The first argument errors, so the second never produces output.
    assert_eq!(run(g, &["SELECT", "SELECT 1"]), "incomplete input");
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases: &[&[&str]] = &[
        &[
            "CREATE TABLE t(a); INSERT INTO t VALUES(1),(2)",
            "SELECT a FROM t",
        ],
        &[
            "CREATE TABLE t(a)",
            "INSERT INTO t VALUES(7)",
            "SELECT a FROM t",
        ],
        &["SELECT 1", "SELECT 2"],
        &["CREATE TABLE t(a); INSERT INTO t VALUES(9); SELECT a FROM t;"],
        &["SELECT", "SELECT 1"],
    ];
    for args in cases {
        assert_eq!(run("sqlite3", args), run(g, args), "for {args:?}");
    }
}
