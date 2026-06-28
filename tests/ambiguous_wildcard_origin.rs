//! When `*` (or `tbl.*`) expansion over an *unaliased self-join* surfaces a
//! column that two sources share by both name and qualifier, SQLite cannot tell
//! the copies apart and rejects the query — naming the column by its source's
//! origin: `<db>.<table>.<col>` for a real table (`main.t.a`, a temp table that
//! shadows it → `temp.t.a`, an attached `aux.t.a`), or `*.<alias>.<col>` for a
//! derived table / CTE that has no database (`*.x.a`). graphite previously
//! emitted just `<source>.<col>` (`t.a`, `x.a`); it now matches the origin
//! prefix. An *explicit* ambiguous reference (`SELECT a` / `SELECT x.a`) keeps
//! its bare/`table.col` spelling, unchanged.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// The `ambiguous column name: …` tail of the error, regardless of how the CLI
/// wraps it (prepare errors vs the parse-error `near line N:` prefix). For a
/// query that does not error, returns its first non-empty output line so the
/// valid cases still compare equal.
fn err_tail(bin: &str, sql: &str) -> String {
    let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for line in text.lines() {
        if let Some(pos) = line.find("ambiguous column name:") {
            return line[pos..].trim_end().to_string();
        }
    }
    text.lines()
        .map(str::trim_end)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

#[test]
fn wildcard_ambiguity_names_source_origin() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    for sql in [
        // Real table self-join → `main.t.col`.
        "CREATE TABLE t(a); SELECT * FROM t, t",
        "CREATE TABLE t(a,b); SELECT * FROM t, t",
        "CREATE TABLE t(a,b); SELECT t.* FROM t, t",
        // Temp table (shadows main) → `temp.t.col`.
        "CREATE TEMP TABLE t(a,b); SELECT * FROM t, t",
        "CREATE TABLE t(a,b); CREATE TEMP TABLE t(a,b); SELECT * FROM t, t",
        // CTE / subquery → `*.alias.col`.
        "WITH x AS (SELECT 1) SELECT * FROM x, x",
        "WITH x AS (SELECT 1 a) SELECT * FROM x, x",
        "CREATE TABLE t(a,b); WITH x AS (SELECT * FROM t) SELECT * FROM x, x",
        "SELECT * FROM (SELECT 1 a) x, (SELECT 2 a) x",
        // Explicit references keep their bare / table.col spelling.
        "WITH x AS (SELECT 1 a) SELECT a FROM x, x",
        "WITH x AS (SELECT 1 a) SELECT x.a FROM x, x",
        "CREATE TABLE t(a); SELECT a FROM t, t",
        "CREATE TABLE t(a); CREATE TABLE u(a); SELECT * FROM t,u WHERE a=1",
        // Aliased self-join is unambiguous — both run fine.
        "CREATE TABLE t(a); SELECT * FROM t t1, t t2",
    ] {
        assert_eq!(err_tail("sqlite3", sql), err_tail(g, sql), "for {sql}");
    }
}
