//! `min()`/`max()` called with *zero* arguments matches neither the one-argument
//! aggregate form nor the (>=2)-argument scalar form, so SQLite rejects it at
//! prepare time with `wrong number of arguments to function NAME()`. graphite's
//! arity validator treats `min`/`max` as aggregates only at one argument, so the
//! bare zero-arg call slipped past and was caught only lazily — i.e. never, over
//! an empty (or fully filtered) table, where it silently produced no rows. It is
//! now rejected up front in every clause. The windowed form (`max() OVER ()`) is
//! a *different* error — `min`/`max` may not be window functions at all — and is
//! left to that check. Verified against the sqlite3 3.50.4 CLI.

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
fn min_max_zero_arg_arity_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let s = "CREATE TABLE t(a);";
    for sql in [
        // Zero-arg min()/max() over an EMPTY table — every clause position.
        &format!("{s} SELECT max() FROM t"),
        &format!("{s} SELECT min() FROM t"),
        &format!("{s} SELECT 1+max() FROM t"),
        &format!("{s} SELECT a FROM t WHERE max()"),
        &format!("{s} SELECT a FROM t WHERE min()>0"),
        &format!("{s} SELECT a FROM t GROUP BY max()"),
        &format!("{s} SELECT a FROM t ORDER BY max()"),
        &format!("{s} SELECT a FROM t GROUP BY a HAVING max()"),
        &format!("{s} SELECT a FROM t GROUP BY a HAVING count(*)>0 AND max()"),
        // No FROM clause (a row is produced, so this matched even before the fix).
        "SELECT max()",
        "SELECT min()",
        // The windowed form is a distinct error (min/max are not window functions).
        &format!("{s} SELECT max() OVER () FROM t"),
        &format!("{s} SELECT min() OVER () FROM t"),
        // Valid min/max — aggregate (1 arg) and scalar (>=2 args) — still run.
        &format!("{s} SELECT max(a) FROM t"),
        &format!("{s} SELECT min(a) FROM t"),
        "SELECT max(1,2)",
        "SELECT min(3,1,2)",
        &format!("{s} INSERT INTO t VALUES(1),(2); SELECT max(a), min(a) FROM t"),
        &format!("{s} INSERT INTO t VALUES(5); SELECT max(a) OVER () FROM t"),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
