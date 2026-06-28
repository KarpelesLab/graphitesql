//! An `expr IN (SELECT …)` whose subquery returns a different number of columns
//! than the left-hand side expects is a prepare-time error in SQLite
//! (`sub-select returns N columns - expected M`), raised before the query runs.
//! graphite resolved the `IN` lazily, per row, so it silently accepted the
//! mismatch over an empty (or fully filtered) table where no row ever evaluates
//! the predicate; it now reports the same arity error at prepare time, on both
//! the SELECT and the UPDATE/DELETE paths.
//!
//! Column resolution still wins: the check fires only when every column the
//! subquery and the LHS reference resolves, so a `no such column` (SQLite's
//! first error) is never masked by an arity report. A subquery with an
//! unresolved column is therefore left to its existing (lazy) behaviour and is
//! not asserted here. Verified against the sqlite3 3.50.4 CLI.

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
fn in_select_arity_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let s = "CREATE TABLE t(a,b);";
    for sql in [
        // Scalar LHS, wider subquery — rejected even though the table is empty.
        &format!("{s} SELECT * FROM t WHERE a IN (SELECT a, b FROM t)"),
        &format!("{s} SELECT * FROM t WHERE a NOT IN (SELECT a, a FROM t)"),
        // Row-value LHS, narrower subquery — the other direction.
        &format!("{s} SELECT * FROM t WHERE (a,b) IN (SELECT a FROM t)"),
        // A constant-row subquery has a structural width too.
        &format!("{s} SELECT * FROM t WHERE a IN (SELECT 1, 2)"),
        // `SELECT *` expands to the FROM width.
        &format!("{s} SELECT * FROM t WHERE a IN (SELECT * FROM t)"),
        // A correlated subquery is still arity-checked at prepare time.
        &format!("{s} SELECT * FROM t WHERE a IN (SELECT a, b FROM t WHERE b=t.a)"),
        // In a HAVING predicate (an aggregate-context clause).
        &format!("{s} SELECT a FROM t GROUP BY a HAVING count(*) IN (SELECT a, b FROM t)"),
        // The UPDATE/DELETE WHERE and SET paths reject it the same way.
        &format!("{s} DELETE FROM t WHERE a IN (SELECT a, b FROM t)"),
        &format!("{s} UPDATE t SET a=1 WHERE a IN (SELECT a, b FROM t)"),
        &format!("{s} UPDATE t SET a=(b IN (SELECT a, b FROM t))"),
        // Matching widths are valid — no error, empty result.
        &format!("{s} SELECT * FROM t WHERE (a,b) IN (SELECT a, b FROM t)"),
        &format!("{s} SELECT * FROM t WHERE a IN (SELECT b FROM t)"),
        // And a matching width over a non-empty table still runs.
        &format!("{s} INSERT INTO t VALUES(1,2); SELECT * FROM t WHERE a IN (SELECT a FROM t)"),
        &format!(
            "{s} INSERT INTO t VALUES(1,2); SELECT * FROM t WHERE (a,b) IN (SELECT a, b FROM t)"
        ),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
