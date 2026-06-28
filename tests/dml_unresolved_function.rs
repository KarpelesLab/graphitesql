//! An unknown or wrong-arity *scalar* function in a DELETE/UPDATE `SET` value,
//! `WHERE` predicate, or `RETURNING` clause is a prepare-time error in SQLite
//! (`no such function: NAME` / `wrong number of arguments to function NAME()`).
//! graphite resolved these lazily, so it silently accepted them over an empty
//! or fully-filtered table where no row ever evaluates the call; it now rejects
//! them at prepare time. Column existence is still resolved first (`RETURNING
//! nope(zzz)` → `no such column: zzz`), and a misused aggregate/window keeps its
//! own wording (`nope(sum(a))` → `misuse of aggregate function sum()`), so the
//! existence check only fires when nothing else did. Verified against the
//! sqlite3 3.50.4 CLI.

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
fn dml_unresolved_function_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let s = "CREATE TABLE t(a,b);";
    for sql in [
        // Unknown function — raised at prepare time even though the table is empty.
        &format!("{s} DELETE FROM t RETURNING nope(a)"),
        &format!("{s} UPDATE t SET a=1 RETURNING nope(a)"),
        &format!("{s} UPDATE t SET a=nope(a)"),
        &format!("{s} UPDATE t SET a=1 WHERE nope(a)"),
        &format!("{s} DELETE FROM t WHERE nope(a)"),
        &format!("{s} DELETE FROM t RETURNING nope(a, b)"),
        // Wrong argument count to a known builtin.
        &format!("{s} DELETE FROM t RETURNING abs(a, b)"),
        &format!("{s} UPDATE t SET a=abs(a, b)"),
        &format!("{s} DELETE FROM t WHERE abs(a, b)"),
        // Nested inside another expression — the inner unknown wins.
        &format!("{s} DELETE FROM t RETURNING 1 + nope(a)"),
        &format!("{s} UPDATE t SET a=abs(nope(a))"),
        // Column existence is resolved before function existence.
        &format!("{s} DELETE FROM t RETURNING nope(zzz)"),
        &format!("{s} UPDATE t SET a=nope(zzz)"),
        // A misused aggregate keeps its own wording (existence check is last).
        &format!("{s} UPDATE t SET a=nope(sum(a))"),
        &format!("{s} DELETE FROM t RETURNING nope(count(*))"),
        // Valid scalar calls still run, empty and non-empty.
        &format!("{s} DELETE FROM t RETURNING abs(a)"),
        &format!("{s} UPDATE t SET a=abs(a) WHERE a>0"),
        &format!("{s} INSERT INTO t VALUES(-5,6); UPDATE t SET a=abs(a) RETURNING a"),
        &format!("{s} INSERT INTO t VALUES(1,2); DELETE FROM t WHERE a>0 RETURNING a, abs(b)"),
        // INSERT … RETURNING already errored eagerly (a row is always produced);
        // confirm it still matches.
        &format!("{s} INSERT INTO t VALUES(1,2) RETURNING nope(a)"),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
