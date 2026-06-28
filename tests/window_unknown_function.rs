//! A function carrying `OVER (…)` that is neither a built-in window function nor
//! an aggregate is rejected at prepare time. SQLite resolves the *name* before it
//! classifies the `OVER`, so the two faults are reported in that order: an unknown
//! name is `no such function: NAME`, while a known scalar misused as a window is
//! `NAME() may not be used as a window function`. graphite previously emitted the
//! window-misuse wording for an unknown name too (`nope() OVER ()` → "nope() may
//! not be used as a window function"). Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;

/// Run `sql` and return the error message with the library's `error: ` framing
/// stripped, so it compares to sqlite's bare text.
fn err(c: &Connection, sql: &str) -> String {
    let e = c.query(sql).unwrap_err().to_string();
    e.strip_prefix("error: ").unwrap_or(&e).to_string()
}

#[test]
fn unknown_windowed_function_is_no_such_function() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    // Unknown name: existence is checked first, regardless of arity or clause.
    assert_eq!(
        err(&c, "SELECT nope() OVER () FROM t"),
        "no such function: nope"
    );
    assert_eq!(
        err(&c, "SELECT nope(a, a) OVER () FROM t"),
        "no such function: nope"
    );
    assert_eq!(
        err(&c, "SELECT nope() OVER (PARTITION BY a) FROM t"),
        "no such function: nope"
    );
    assert_eq!(
        err(&c, "SELECT a FROM t ORDER BY nope() OVER ()"),
        "no such function: nope"
    );
}

#[test]
fn known_scalar_windowed_is_window_misuse() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    // A *known* scalar (any arity) stays the window-misuse error.
    for (sql, name) in [
        ("SELECT abs(a) OVER () FROM t", "abs"),
        ("SELECT abs(a, a) OVER () FROM t", "abs"),
        ("SELECT abs() OVER () FROM t", "abs"),
        ("SELECT max(a, a) OVER () FROM t", "max"),
        ("SELECT coalesce(a, a) OVER () FROM t", "coalesce"),
    ] {
        assert_eq!(
            err(&c, sql),
            format!("{name}() may not be used as a window function"),
            "for {sql}"
        );
    }
}

#[test]
fn valid_windowed_calls_still_run() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES (1)").unwrap();
    // Aggregates and built-in window functions remain valid with OVER.
    assert!(c.query("SELECT sum(a) OVER () FROM t").is_ok());
    assert!(c.query("SELECT row_number() OVER () FROM t").is_ok());
}

#[test]
fn matches_sqlite_cli() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // sqlite's one-shot CLI appends a caret annotation line to a prepare error;
    // keep only the first line (the message itself), as the differential corpus does.
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        let e = String::from_utf8_lossy(&out.stderr);
        format!("{s}{e}")
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Parse error: ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string()
    };
    for sql in [
        "CREATE TABLE t(a); SELECT nope() OVER () FROM t;",
        "CREATE TABLE t(a); SELECT nope(a,a) OVER () FROM t;",
        "CREATE TABLE t(a); SELECT a FROM t ORDER BY nope() OVER ();",
        "CREATE TABLE t(a); SELECT abs(a) OVER () FROM t;",
        "CREATE TABLE t(a); SELECT max(a,a) OVER () FROM t;",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT sum(a) OVER () FROM t;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
