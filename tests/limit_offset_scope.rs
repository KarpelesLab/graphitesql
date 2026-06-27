//! A `LIMIT`/`OFFSET` expression is resolved by SQLite with **no table columns
//! in scope** — not even a correlated outer column — and that resolution runs
//! ahead of every other check in the statement. So any column reference inside a
//! `LIMIT`/`OFFSET` is `no such column: NAME`, and that error wins over an
//! aggregate misuse (`LIMIT sum(a)` → `no such column: a`, not `misuse of
//! aggregate function sum()`), over an unknown/wrong-arity function
//! (`LIMIT nope(a)` → `no such column: a`), and over a result-column / `WHERE`
//! resolution error elsewhere in the same statement. Only when the `LIMIT`
//! carries no column argument do those other errors surface (`LIMIT count(*)` →
//! `misuse of aggregate function count()`; `LIMIT nope()` → `no such function`).
//! A subquery limit (`LIMIT (SELECT …)`) has its own scope and is valid.
//!
//! graphite previously resolved the `LIMIT` lazily during evaluation, so the
//! aggregate's misuse (or a correlated outer column) was seen before the missing
//! column — silently accepting some statements SQLite rejects at prepare time.

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
fn limit_offset_column_scope_matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let s = "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(3,4),(5,6);";
    let c = "CREATE TABLE t(a,b); INSERT INTO t VALUES(1,2),(3,4); \
             CREATE TABLE t2(c); INSERT INTO t2 VALUES(9);";
    for sql in [
        // An aggregate wrapping a column resolves the missing column first — the
        // case graphite used to report as a `misuse` instead.
        &format!("{s} SELECT a FROM t LIMIT sum(a)"),
        &format!("{s} SELECT a FROM t LIMIT 1 OFFSET sum(b)"),
        &format!("{s} SELECT a FROM t LIMIT max(a,b)"),
        // An aggregate / unknown function with no column argument keeps its own
        // error (no column reference to resolve first).
        &format!("{s} SELECT a FROM t LIMIT count(*)"),
        &format!("{s} SELECT a FROM t LIMIT nope()"),
        // A scalar/unknown call wrapping a column, a bare column, an output alias,
        // and a qualified reference are all `no such column` — LIMIT sees no
        // column scope at all.
        &format!("{s} SELECT a FROM t LIMIT abs(a)"),
        &format!("{s} SELECT a FROM t LIMIT nope(a)"),
        &format!("{s} SELECT a FROM t LIMIT a"),
        &format!("{s} SELECT a AS x FROM t LIMIT x"),
        &format!("{s} SELECT a FROM t LIMIT t.a"),
        &format!("{s} SELECT a FROM t LIMIT 2*a+1"),
        &format!("{s} SELECT a FROM t LIMIT CAST(a AS INT)"),
        &format!("{s} SELECT a FROM t LIMIT coalesce(a,1)"),
        // The LIMIT column error precedes a result-column / WHERE resolution error.
        &format!("{s} SELECT nope(a) FROM t LIMIT sum(b)"),
        &format!("{s} SELECT zzz FROM t LIMIT sum(b)"),
        &format!("{s} SELECT a FROM t WHERE nope(a) LIMIT sum(b)"),
        &format!("{s} SELECT count(*) FROM t WHERE a>0 LIMIT sum(b)"),
        &format!("{s} SELECT a FROM t GROUP BY a LIMIT sum(b)"),
        // Even a *correlated* outer column is out of scope in a subquery's LIMIT.
        &format!("{c} SELECT (SELECT c FROM t2 LIMIT t.a) FROM t"),
        &format!("{c} SELECT a FROM t WHERE a IN (SELECT c FROM t2 LIMIT t.a)"),
        // A derived-table LIMIT over a column is rejected the same way.
        &format!("{s} SELECT a FROM (SELECT a FROM t LIMIT b)"),
        // Valid limits are unchanged: constants, arithmetic, and a subquery
        // (which has its own scope) all run.
        &format!("{s} SELECT a FROM t ORDER BY a LIMIT 2 OFFSET 1"),
        &format!("{s} SELECT a FROM t LIMIT 1+1"),
        &format!("{s} SELECT a FROM t LIMIT (SELECT count(*) FROM t)"),
        &format!(
            "{s} SELECT a FROM t LIMIT (SELECT max(a) FROM t) OFFSET (SELECT min(a)-1 FROM t)"
        ),
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
