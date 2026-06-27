//! SQLite resolves the scalar functions named in a CHECK or generated-column
//! expression at CREATE time: an unknown function is `no such function: NAME`
//! and a wrong argument count is `wrong number of arguments to function NAME()`,
//! both reported *before* the table is created. graphite used to accept such a
//! table and only fail (or silently misbehave) when a row was evaluated. It now
//! dry-resolves each call at CREATE, matching sqlite. Verified against the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The error message from running `sql` (a DDL statement), with the in-process
/// `error: ` prefix stripped.
fn err(sql: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    let e = c.execute(sql).unwrap_err().to_string();
    e.trim_start_matches("error: ").to_string()
}

#[test]
fn unknown_function_in_table_check_is_rejected() {
    assert_eq!(
        err("CREATE TABLE t(a, CHECK(unknownfn(a)))"),
        "no such function: unknownfn"
    );
}

#[test]
fn unknown_function_in_column_check_is_rejected() {
    assert_eq!(
        err("CREATE TABLE t(a CHECK(unknownfn(a)))"),
        "no such function: unknownfn"
    );
}

#[test]
fn unknown_function_in_generated_column_is_rejected() {
    assert_eq!(
        err("CREATE TABLE t(a, b GENERATED ALWAYS AS (unknownfn(a)))"),
        "no such function: unknownfn"
    );
}

#[test]
fn unknown_function_nested_deep_is_rejected() {
    assert_eq!(
        err("CREATE TABLE t(a, CHECK(CASE WHEN nope(a) THEN 1 ELSE 0 END))"),
        "no such function: nope"
    );
    assert_eq!(
        err("CREATE TABLE t(a, CHECK(a + nope(a) > 0))"),
        "no such function: nope"
    );
}

#[test]
fn wrong_argument_count_is_rejected() {
    assert_eq!(
        err("CREATE TABLE t(a, CHECK(abs()))"),
        "wrong number of arguments to function abs()"
    );
    assert_eq!(
        err("CREATE TABLE t(a, CHECK(abs(a, a)))"),
        "wrong number of arguments to function abs()"
    );
    assert_eq!(
        err("CREATE TABLE t(a, b GENERATED ALWAYS AS (abs()))"),
        "wrong number of arguments to function abs()"
    );
}

#[test]
fn known_functions_are_accepted_and_still_run() {
    // A valid CHECK using a real function is created, and the constraint still
    // applies at INSERT time (the dry-run leaves no trace).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, CHECK(abs(a) < 10))").unwrap();
    c.execute("INSERT INTO t VALUES (-3)").unwrap();
    let e = c
        .execute("INSERT INTO t VALUES (-20)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("CHECK constraint failed"), "got: {e}");
}

#[test]
fn nondeterministic_function_in_check_is_accepted() {
    // sqlite permits random() in a CHECK (it forbids it only in generated columns
    // and index expressions). The dry-run must not leak its RNG advance.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, CHECK(random() <> a))")
        .unwrap();
    c.execute("CREATE TABLE u(a, CHECK(date('now') IS NOT NULL))")
        .unwrap();
}

#[test]
fn aggregate_in_check_keeps_its_own_message() {
    // The aggregate-misuse check runs first and owns this wording.
    assert_eq!(
        err("CREATE TABLE t(a, CHECK(count(a) > 0))"),
        "misuse of aggregate function count()"
    );
}

#[test]
fn unknown_column_inside_unknown_function_reports_the_column() {
    // sqlite resolves an argument (a child) before the enclosing call, so the bad
    // column wins here. graphite's separate column check runs first, matching.
    assert_eq!(
        err("CREATE TABLE t(a, CHECK(unknownfn(zzz)))"),
        "no such column: zzz"
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let norm = |out: &[u8]| -> String {
        // Keep only the first line and strip the trailing ` (NN)` error code, so
        // the comparison is on the message itself (the CLI frames errors slightly
        // differently than the library does).
        let s = String::from_utf8_lossy(out);
        let first = s.lines().next().unwrap_or("").to_string();
        first
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    let strip_code = |s: String| -> String {
        // Drop a trailing " (NN)" sqlite error code if present.
        match s.rfind(" (") {
            Some(i) if s.ends_with(')') => s[..i].to_string(),
            _ => s,
        }
    };
    let cases = [
        "CREATE TABLE t(a, CHECK(unknownfn(a)))",
        "CREATE TABLE t(a, b GENERATED ALWAYS AS (unknownfn(a)))",
        "CREATE TABLE t(a, CHECK(abs(a, a)))",
        "CREATE TABLE t(a, CHECK(length()))",
        "CREATE TABLE t(a, CHECK(abs(a) < 10))",
        "CREATE TABLE t(a, CHECK(unknownfn(zzz)))",
    ];
    for sql in cases {
        let s = Command::new("sqlite3")
            .arg(":memory:")
            .arg(sql)
            .output()
            .unwrap();
        let gg = Command::new(g).arg(":memory:").arg(sql).output().unwrap();
        assert_eq!(
            strip_code(norm(&s.stdout) + &norm(&s.stderr)),
            strip_code(norm(&gg.stdout) + &norm(&gg.stderr)),
            "mismatch for {sql:?}"
        );
    }
}
