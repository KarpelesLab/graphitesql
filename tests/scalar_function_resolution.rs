//! SQLite resolves and arity-checks every scalar function call at prepare time:
//! an unknown name is `no such function: NAME` and a wrong argument count is
//! `wrong number of arguments to function NAME()`, both raised *before* the query
//! runs. graphite's tree-walker noticed only at row-evaluation time — i.e. never
//! over an empty (or fully filtered) table, where it silently produced no rows,
//! and the experimental VDBE fast path compiled a known call without re-checking
//! its arity. Both gaps are now closed by `reject_unresolved_functions_in_select`,
//! run ahead of execution on every clause. A missing column still wins (`no such
//! column: c`), since column resolution happens first on every path.
//!
//! Only functions whose arity is identical in the pinned ASCII-only sqlite3 the CI
//! oracle uses are exercised here — `lower`/`upper`/`substr` take an optional
//! locale/extra argument under a *locally* ICU-enabled sqlite, which is not
//! differentiable (see the project's ICU note), so they are deliberately omitted.
//! Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// First non-caret line of combined stdout/stderr, error-prefix stripped.
fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().next().unwrap_or("").trim_end().to_string();
    if !line.is_empty() {
        return line;
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
fn scalar_function_resolution_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let s = "CREATE TABLE t(a,b);";
    for sql in [
        // Wrong-arity scalar call over an EMPTY table — every clause position. The
        // expression is never evaluated (zero rows), yet it is still rejected.
        &format!("{s} SELECT abs(a,b) FROM t"),
        &format!("{s} SELECT length(a,b) FROM t"),
        &format!("{s} SELECT hex(a,b) FROM t"),
        &format!("{s} SELECT typeof(a,b) FROM t"),
        &format!("{s} SELECT round(a,b,a) FROM t"),
        &format!("{s} SELECT 1+abs(a,b) FROM t"),
        &format!("{s} SELECT a FROM t WHERE abs(a,b)"),
        &format!("{s} SELECT a FROM t GROUP BY abs(a,b)"),
        &format!("{s} SELECT a FROM t ORDER BY abs(a,b)"),
        &format!("{s} SELECT a FROM t GROUP BY a HAVING count(*)>0 AND abs(a,b)"),
        // Nested wrong-arity call.
        &format!("{s} SELECT abs(abs(a,b)) FROM t"),
        // Unknown function over an EMPTY table — every clause position.
        &format!("{s} SELECT nope(a) FROM t"),
        &format!("{s} SELECT nope(a,b) FROM t"),
        &format!("{s} SELECT a FROM t WHERE nope(a)"),
        &format!("{s} SELECT a FROM t GROUP BY nope(a)"),
        &format!("{s} SELECT a FROM t ORDER BY nope(a)"),
        // Alongside a window function (the window-dispatch path) — still rejected.
        &format!("{s} SELECT row_number() OVER (), abs(a,b) FROM t"),
        &format!("{s} SELECT sum(a) OVER (), nope(b) FROM t"),
        // A missing column inside the call still wins over the (here valid) arity.
        &format!("{s} SELECT abs(c) FROM t"),
        &format!("{s} SELECT nope(c) FROM t"),
        // Valid calls still run — empty and non-empty.
        &format!("{s} SELECT abs(a), length(b) FROM t"),
        &format!("{s} INSERT INTO t VALUES(-1,'XY'); SELECT abs(a), length(b), hex(a) FROM t"),
        // No-FROM (a row is produced) — these matched even before the fix.
        "SELECT abs(1,2)",
        "SELECT nope()",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
