//! A built-in window-only function (`row_number`, `rank`, `lag`, …) exists
//! solely as a window function, so calling one without an `OVER` clause is
//! `misuse of window function NAME()` in SQLite. graphite's per-row evaluator
//! already reported this when a row was reached, but over an empty (or fully
//! filtered) table the call was never evaluated and the error was silently
//! skipped — so it is now raised at prepare time. A wrong argument count is
//! diagnosed first (`ntile()` → `wrong number of arguments to function
//! ntile()`). In a HAVING clause the misuse is reported only once HAVING itself
//! is legal: a non-aggregate query says `HAVING clause on a non-aggregate query`
//! first. Verified against the sqlite3 3.50.4 CLI.

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
fn window_function_without_over_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let s = "CREATE TABLE t(a,b);";
    for sql in [
        // Every window-only function, called as a scalar over an empty table.
        &format!("{s} SELECT row_number() FROM t"),
        &format!("{s} SELECT rank() FROM t"),
        &format!("{s} SELECT dense_rank() FROM t"),
        &format!("{s} SELECT percent_rank() FROM t"),
        &format!("{s} SELECT cume_dist() FROM t"),
        &format!("{s} SELECT ntile(2) FROM t"),
        &format!("{s} SELECT lag(a) FROM t"),
        &format!("{s} SELECT lead(a) FROM t"),
        &format!("{s} SELECT first_value(a) FROM t"),
        &format!("{s} SELECT last_value(a) FROM t"),
        &format!("{s} SELECT nth_value(a,1) FROM t"),
        // Wrong argument count is reported ahead of the misuse.
        &format!("{s} SELECT ntile() FROM t"),
        &format!("{s} SELECT row_number(1,2,3) FROM t"),
        &format!("{s} SELECT lag() FROM t"),
        &format!("{s} SELECT nth_value(a) FROM t"),
        // Nested inside another expression, and in non-result positions.
        &format!("{s} SELECT 1+row_number() FROM t"),
        &format!("{s} SELECT abs(rank()) FROM t"),
        &format!("{s} SELECT a FROM t WHERE row_number()"),
        &format!("{s} SELECT count(*) FROM t WHERE rank()>0"),
        &format!("{s} SELECT a FROM t GROUP BY row_number()"),
        &format!("{s} SELECT a FROM t ORDER BY row_number()"),
        // HAVING: misuse only on a genuine aggregate query …
        &format!("{s} SELECT a FROM t GROUP BY a HAVING rank()>0"),
        &format!("{s} SELECT count(*) FROM t HAVING rank()>0"),
        // … otherwise the HAVING-context error wins (also for an OVER window).
        &format!("{s} SELECT a FROM t HAVING rank()>0"),
        &format!("{s} SELECT a FROM t HAVING rank() OVER ()>0"),
        &format!("{s} SELECT a FROM t GROUP BY a HAVING rank() OVER ()>0"),
        // Correct usages still parse and run.
        &format!("{s} SELECT row_number() OVER () FROM t"),
        &format!("{s} INSERT INTO t VALUES(1,2); SELECT row_number() OVER (ORDER BY a) FROM t"),
        "SELECT row_number()",
        &format!("{s} SELECT * FROM (SELECT row_number() OVER () AS r FROM t)"),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
