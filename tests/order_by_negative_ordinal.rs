//! A negative integer literal used as an `ORDER BY` ordinal (`ORDER BY -1`) — and
//! any other wrapped positional form SQLite reads as a column position (a unary
//! `+`/`-`, parenthesis, or `COLLATE`) — must be reported as an out-of-range
//! ordinal at prepare time, exactly as SQLite does: `Nth ORDER BY term out of range
//! - should be between 1 and M`.
//!
//! The tree-walker's `check_positional_terms` already resolved `-1` (via
//! `positional_int`, which folds a unary minus) and rejected it. The opt-in VDBE
//! fast path, however, only treated a *bare positive* integer literal as an
//! ordinal; a `-1` parses as `Unary{Negate, Integer(1)}`, fell through to the
//! general-expression arm, and was compiled as a sort by the constant value −1 (a
//! no-op) — so the query silently succeeded where SQLite errors. The VDBE now
//! defers every positional ordinal it does not itself accelerate (an in-range bare
//! positive integer) to the tree-walker, which owns the exact range error.
//! Verified against sqlite3 3.50.4, with the VDBE both on (default) and off.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

const SETUP: &str = "CREATE TABLE t(a INTEGER, b TEXT); CREATE TABLE u(c INTEGER, d TEXT);";

fn conn(use_vdbe: bool) -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.set_use_vdbe(use_vdbe);
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
fn rejects_negative_order_by_ordinal() {
    // Both engines must reject identically — this is the whole point of the fix.
    for &vdbe in &[true, false] {
        let c = conn(vdbe);
        assert_eq!(
            err(&c, "SELECT a FROM t ORDER BY -1"),
            "1st ORDER BY term out of range - should be between 1 and 1",
            "vdbe={vdbe}"
        );
        assert_eq!(
            err(&c, "SELECT a, b FROM t ORDER BY -1"),
            "1st ORDER BY term out of range - should be between 1 and 2",
            "vdbe={vdbe}"
        );
        // The reported ordinal is the offending term's position within the clause.
        assert_eq!(
            err(&c, "SELECT a, b FROM t ORDER BY 1, -1"),
            "2nd ORDER BY term out of range - should be between 1 and 2",
            "vdbe={vdbe}"
        );
        assert_eq!(
            err(&c, "SELECT a FROM t ORDER BY a, -1"),
            "2nd ORDER BY term out of range - should be between 1 and 1",
            "vdbe={vdbe}"
        );
        // `*` width is counted after expansion.
        assert_eq!(
            err(&c, "SELECT * FROM t ORDER BY -1"),
            "1st ORDER BY term out of range - should be between 1 and 2",
            "vdbe={vdbe}"
        );
        // A trailing direction does not change the ordinal resolution.
        assert_eq!(
            err(&c, "SELECT a FROM t ORDER BY -1 DESC"),
            "1st ORDER BY term out of range - should be between 1 and 1",
            "vdbe={vdbe}"
        );
    }
}

#[test]
fn valid_and_wrapped_ordinals_still_resolve() {
    for &vdbe in &[true, false] {
        let c = conn(vdbe);
        // In-range bare and wrapped positive ordinals are accepted.
        assert!(c.query("SELECT a FROM t ORDER BY 1").is_ok(), "vdbe={vdbe}");
        assert!(
            c.query("SELECT a, b FROM t ORDER BY 2").is_ok(),
            "vdbe={vdbe}"
        );
        // A unary `+` and a parenthesis are positional too: `+2`/`(2)` over two
        // columns resolve to column 2 (not a constant sort).
        assert!(
            c.query("SELECT a, b FROM t ORDER BY +2").is_ok(),
            "vdbe={vdbe}"
        );
        assert!(
            c.query("SELECT a, b FROM t ORDER BY (2)").is_ok(),
            "vdbe={vdbe}"
        );
        // A non-integer ordinal expression (`1.5`) is a constant sort, not a
        // position — accepted, like SQLite.
        assert!(
            c.query("SELECT a FROM t ORDER BY 1.5").is_ok(),
            "vdbe={vdbe}"
        );
        // A bare column reference still resolves.
        assert!(c.query("SELECT a FROM t ORDER BY a").is_ok(), "vdbe={vdbe}");
    }
}

#[test]
fn wrapped_out_of_range_ordinal_rejected() {
    for &vdbe in &[true, false] {
        let c = conn(vdbe);
        // `+2`/`(2)` with a single output column are out of range.
        assert_eq!(
            err(&c, "SELECT a FROM t ORDER BY +2"),
            "1st ORDER BY term out of range - should be between 1 and 1",
            "vdbe={vdbe}"
        );
        assert_eq!(
            err(&c, "SELECT a FROM t ORDER BY (2)"),
            "1st ORDER BY term out of range - should be between 1 and 1",
            "vdbe={vdbe}"
        );
    }
}

#[test]
fn matches_sqlite_cli() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        let e = String::from_utf8_lossy(&out.stderr);
        let line = format!("{s}{e}")
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Parse error: ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string();
        if line.is_empty() {
            "<ok>".to_string()
        } else {
            line
        }
    };
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for tail in [
        "SELECT a FROM t ORDER BY -1;",
        "SELECT a, b FROM t ORDER BY -1;",
        "SELECT a, b FROM t ORDER BY 1, -1;",
        "SELECT a FROM t ORDER BY a, -1;",
        "SELECT * FROM t ORDER BY -1;",
        "SELECT a FROM t ORDER BY -1 DESC;",
        "SELECT a, b FROM t UNION SELECT c, d FROM u ORDER BY -1;",
        "SELECT a FROM t ORDER BY +2;",
        "SELECT a, b FROM t ORDER BY (2);",
        // GROUP BY shares the rule (and was already handled): both engines reject.
        "SELECT a FROM t GROUP BY -1;",
        "SELECT a, count(*) FROM t GROUP BY -1;",
        // Valid forms succeed in both.
        "SELECT a, b FROM t ORDER BY 2;",
        "SELECT a, b FROM t ORDER BY +2;",
        "SELECT a FROM t ORDER BY 1.5;",
        "SELECT a FROM t ORDER BY a;",
    ] {
        let sql = format!("{SETUP} {tail}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {tail}");
    }
}
