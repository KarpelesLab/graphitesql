//! An unknown or wrong-arity **scalar function call inside an expression-position
//! subquery** (`(SELECT …)`, `EXISTS (…)`, `… IN (SELECT …)`) must be rejected at
//! prepare time, like sqlite — even over an empty / fully-filtered outer table where
//! the row-evaluated tree-walker never reaches the call.
//! `reject_unresolved_functions_in_select` walks the outer expressions but never
//! descends into a nested subquery body, so such a call was previously accepted.
//!
//! `Executor::reject_unresolved_functions_in_subqueries` closes the gap: it collects
//! each subquery the outer expressions carry and checks its scalar calls — but only
//! when the subquery body is *column-clean* against its own `FROM` plus the outer
//! scope. That gate preserves sqlite's precedence: a `no such column` it reports
//! first is never masked by a function error (`SELECT (SELECT nope(zzz))` is a
//! missing-column case — now caught eagerly by `validate_subquery_body_columns`).
//! A subquery it cannot fully verify — compound or further-nested — is left alone,
//! never a false positive. Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

const SETUP: &str = "\
    CREATE TABLE t(a INTEGER, b TEXT);\n\
    CREATE TABLE u(c INTEGER);\n";

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        let s = stmt.trim();
        if !s.is_empty() {
            c.execute(s).unwrap();
        }
    }
    c
}

fn err(c: &Connection, sql: &str) -> String {
    let e = c.query(sql).unwrap_err().to_string();
    e.strip_prefix("error: ").unwrap_or(&e).to_string()
}

#[test]
fn rejects_unknown_function_in_subquery() {
    let c = conn();
    // Tables empty on purpose: the row-evaluated call is never reached.
    assert_eq!(
        err(&c, "SELECT (SELECT nope(a)) FROM t"),
        "no such function: nope"
    );
    assert_eq!(
        err(&c, "SELECT (SELECT nope(b) FROM u) FROM t"),
        "no such function: nope"
    );
    assert_eq!(
        err(&c, "SELECT a FROM t WHERE a IN (SELECT nope(c) FROM u)"),
        "no such function: nope"
    );
    assert_eq!(
        err(&c, "SELECT a FROM t WHERE EXISTS (SELECT nope(c) FROM u)"),
        "no such function: nope"
    );
}

#[test]
fn rejects_wrong_arity_in_subquery() {
    let c = conn();
    assert_eq!(
        err(&c, "SELECT (SELECT abs(1, 2)) FROM t"),
        "wrong number of arguments to function abs()"
    );
    // A wrong-arity call buried in a correlated, column-clean subquery WHERE.
    assert_eq!(
        err(
            &c,
            "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE substr(b) = c)"
        ),
        "wrong number of arguments to function substr()"
    );
}

#[test]
fn does_not_reject_valid_or_unverifiable_subqueries() {
    let c = conn();
    // Valid scalar calls inside a subquery are not rejected.
    assert!(c.query("SELECT (SELECT abs(c) FROM u) FROM t").is_ok());
    assert!(c.query("SELECT (SELECT upper(b)) FROM t").is_ok());
    assert!(c
        .query("SELECT a FROM t WHERE a IN (SELECT abs(c) FROM u)")
        .is_ok());
    // An aggregate in the subquery is not a scalar call (no false "no such function").
    assert!(c.query("SELECT (SELECT max(c) FROM u) FROM t").is_ok());
    // A subquery whose own column does not resolve is a `no such column`, NOT a
    // function error: sqlite reports the missing column first, and graphite now does
    // too (eagerly, via `validate_subquery_body_columns`), so this must NOT surface
    // as `no such function`.
    assert_eq!(
        err(&c, "SELECT (SELECT nope(zzz)) FROM t"),
        "no such column: zzz"
    );
    // A compound subquery body cannot be verified clean, so it is left alone too.
    assert!(c
        .query("SELECT a FROM t WHERE a IN (SELECT nope(c) FROM u UNION SELECT c FROM u)")
        .is_ok());
}

#[test]
fn matches_sqlite_cli() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
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
    for tail in [
        // Rejected at prepare time by both engines.
        "SELECT (SELECT nope(a)) FROM t;",
        "SELECT (SELECT nope(b) FROM u) FROM t;",
        "SELECT a FROM t WHERE a IN (SELECT nope(c) FROM u);",
        "SELECT a FROM t WHERE EXISTS (SELECT nope(c) FROM u);",
        "SELECT (SELECT abs(1,2)) FROM t;",
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE substr(b)=c);",
        // Accepted by both (empty result, no error).
        "SELECT (SELECT abs(c) FROM u) FROM t;",
        "SELECT (SELECT upper(b)) FROM t;",
        "SELECT a FROM t WHERE a IN (SELECT abs(c) FROM u);",
        "SELECT (SELECT max(c) FROM u) FROM t;",
    ] {
        let sql = format!("{SETUP} {tail}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {tail}");
    }
}
