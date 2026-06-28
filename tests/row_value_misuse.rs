//! A row value `(a, b, …)` used where a single value is required, and a
//! comparison or `BETWEEN` whose operands disagree in row arity, are both
//! `row value misused` in SQLite — raised at prepare time, before the query
//! runs. graphite evaluated the misuse per row (the row-value arm of the scalar
//! evaluator, and the operand-arity checks on `=`/`IS`/`BETWEEN`), so over an
//! empty (or fully filtered) table where no row was ever evaluated it silently
//! accepted the statement. It now reports the same error at prepare time, on
//! both the SELECT and the UPDATE/DELETE paths.
//!
//! An `IN` list whose elements disagree in arity with the left-hand side is the
//! closely related `IN(...) element has N terms - expected M` (a row element
//! under a scalar LHS is `row value misused` instead) — also raised at prepare
//! time, where graphite evaluated it per row and could even return a wrong
//! result when an earlier list element happened to match.
//!
//! Equal-arity row comparisons (`(a,b) = (1,2)`, `(a,b) BETWEEN (1,2) AND
//! (3,4)`) stay valid. A multi-column subquery in a plain scalar position is a
//! different message (`sub-select returns N columns - expected 1`) and is not
//! asserted here. Column resolution still wins when the operands are clean; a
//! comparison against a subquery carrying an unresolved column is left to its
//! existing behaviour. Verified against the sqlite3 3.50.4 CLI.

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
fn row_value_misuse_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let s = "CREATE TABLE t(a,b);";
    for sql in [
        // A bare row value in a scalar position — rejected even though the table
        // is empty and the expression never evaluates.
        &format!("{s} SELECT (1, 2) FROM t"),
        &format!("{s} SELECT ((1, 2)) FROM t"),
        &format!("{s} SELECT a + (1, 2) FROM t"),
        &format!("{s} SELECT abs((1, 2)) FROM t"),
        &format!("{s} SELECT * FROM t WHERE (1, 2)"),
        &format!("{s} SELECT 1 FROM t ORDER BY (1, 2)"),
        // A comparison / BETWEEN with mismatched operand arity.
        &format!("{s} SELECT * FROM t WHERE a = (1, 2)"),
        &format!("{s} SELECT * FROM t WHERE (a) = (1, 2)"),
        &format!("{s} SELECT * FROM t WHERE (a, b) = (1, 2, 3)"),
        &format!("{s} SELECT * FROM t WHERE (a, b, a) = (1, 2)"),
        &format!("{s} SELECT * FROM t WHERE (a, b) < (1, 2, 3)"),
        &format!("{s} SELECT * FROM t WHERE (a, b) BETWEEN (1, 2, 3) AND (4, 5, 6)"),
        // A multi-column subquery as a comparison operand is `row value misused`
        // too (not the scalar-arity message, which is for non-comparison contexts).
        &format!("{s} SELECT * FROM t WHERE a > (SELECT 1, 2)"),
        &format!("{s} SELECT * FROM t WHERE a IS (SELECT 1, 2)"),
        &format!("{s} SELECT * FROM t WHERE (a, b) = (SELECT a FROM t)"),
        &format!("{s} SELECT * FROM t WHERE a BETWEEN (SELECT 1, 2) AND 3"),
        // A row value nested inside a valid row comparison's element.
        &format!("{s} SELECT * FROM t WHERE (a, (1, 2)) = (1, 2)"),
        &format!("{s} SELECT * FROM t WHERE (a + (1, 2), b) = (1, 2)"),
        // An `IN` list element whose arity disagrees with the LHS.
        &format!("{s} SELECT * FROM t WHERE (a, b) IN ((1, 2), (3))"),
        &format!("{s} SELECT * FROM t WHERE (a, b) IN (1, 2)"),
        &format!("{s} SELECT * FROM t WHERE (a, b) IN ((1, 2, 3))"),
        &format!("{s} SELECT * FROM t WHERE (a, b, a) IN ((1, 2))"),
        &format!("{s} SELECT * FROM t WHERE a IN ((1, 2))"),
        &format!("{s} SELECT * FROM t WHERE a IN (1, (2, 3))"),
        // An earlier matching element used to mask the bad one over a non-empty
        // table (graphite returned a row); now rejected at prepare time.
        &format!("{s} INSERT INTO t VALUES(1,2); SELECT * FROM t WHERE (a, b) IN ((1, 2), (3))"),
        // The UPDATE/DELETE SET and WHERE paths reject it the same way.
        &format!("{s} DELETE FROM t WHERE a = (1, 2)"),
        &format!("{s} DELETE FROM t WHERE (a, b) IN (1, 2)"),
        &format!("{s} UPDATE t SET a = 1 WHERE a = (SELECT 1, 2)"),
        &format!("{s} UPDATE t SET a = (1, 2)"),
        // Equal-arity row comparisons stay valid — no error, empty result.
        &format!("{s} SELECT * FROM t WHERE (a, b) = (1, 2)"),
        &format!("{s} SELECT * FROM t WHERE (a, b) < (3, 4)"),
        &format!("{s} SELECT * FROM t WHERE ((a, b)) = (1, 2)"),
        &format!("{s} SELECT * FROM t WHERE (a, b) IN ((1, 2), (3, 4))"),
        &format!("{s} SELECT * FROM t WHERE a IN (1, 2, 3)"),
        &format!("{s} SELECT * FROM t WHERE (a, b) BETWEEN (1, 2) AND (3, 4)"),
        &format!("{s} SELECT * FROM t WHERE (a, b) = (SELECT a, b FROM t LIMIT 1)"),
        // And a valid row comparison over a non-empty table still runs.
        &format!("{s} INSERT INTO t VALUES(1,2),(3,4); SELECT * FROM t WHERE (a, b) = (1, 2)"),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
