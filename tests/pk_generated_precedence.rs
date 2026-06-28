//! When a CREATE TABLE has both a duplicate PRIMARY KEY *and* a generated
//! column in a PRIMARY KEY, SQLite reports one error or the other based on the
//! *source order* of the PRIMARY KEY declarations — it processes them
//! sequentially (`sqlite3AddPrimaryKey`), so the first declared PK decides:
//!
//!   * a **generated** first PK → `generated columns cannot be part of the
//!     PRIMARY KEY`;
//!   * a **non-generated** first PK followed by any second PK → `table "X" has
//!     more than one primary key`.
//!
//! graphite used to fire the generated-column error eagerly from its per-column
//! loop, so `t(a PRIMARY KEY, b AS (a) PRIMARY KEY)` reported the generated
//! error where SQLite reports "more than one primary key". Column-level PKs
//! precede table-level ones in source order.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn run(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    for line in s.lines() {
        let mut t = line.trim_end();
        if t.trim_start().starts_with('^') {
            continue;
        }
        for prefix in [
            "Error: ",
            "in prepare, ",
            "stepping, ",
            "SQL error: ",
            "error: ",
        ] {
            t = t.strip_prefix(prefix).unwrap_or(t);
        }
        if let Some(rest) = t.strip_prefix("Parse error") {
            t = rest.split_once(": ").map_or(rest, |(_, m)| m);
        }
        if let Some(rest) = t.strip_prefix("Runtime error") {
            t = rest.split_once(": ").map_or(rest, |(_, m)| m);
        }
        return t.to_string();
    }
    String::new()
}

#[test]
fn pk_generated_precedence_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Non-generated first PK + a second (generated) PK → "more than one".
        "CREATE TABLE t(a PRIMARY KEY, b AS (a) PRIMARY KEY)",
        "CREATE TABLE t(a PRIMARY KEY, b AS (a), PRIMARY KEY(b))",
        // Generated first PK → generated-column error wins.
        "CREATE TABLE t(a AS (b) PRIMARY KEY, b)",
        "CREATE TABLE t(a AS (b), b, PRIMARY KEY(a))",
        "CREATE TABLE t(a, b AS (a), PRIMARY KEY(a,b))",
        "CREATE TABLE t(a AS (b) PRIMARY KEY)",
        "CREATE TABLE t(a AS (b) PRIMARY KEY, b PRIMARY KEY)",
        // Two non-generated PKs → "more than one" (no generated columns at all).
        "CREATE TABLE t(a PRIMARY KEY, b PRIMARY KEY)",
        "CREATE TABLE t(a PRIMARY KEY, b, PRIMARY KEY(b))",
        // Valid: single PK, generated non-PK column.
        "CREATE TABLE t(a PRIMARY KEY, b)",
        "CREATE TABLE t(a, b AS (a))",
        "CREATE TABLE t(a PRIMARY KEY, b AS (a))",
    ];
    for sql in cases {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
