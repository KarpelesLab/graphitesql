//! A leading `WITH` clause is in scope for *every* `INSERT` source, not only
//! `INSERT … SELECT`. graphite previously accepted `WITH … INSERT … SELECT` but
//! rejected `WITH … INSERT … VALUES(…)` / `DEFAULT VALUES` with a spurious
//! `near ";": syntax error` (and reported `incomplete input` instead of
//! `no such table` when the CTE name collided with the insert target).
//!
//! SQLite makes the CTE referenceable from a subquery inside the VALUES list,
//! e.g. `WITH c(n) AS (VALUES(5)) INSERT INTO t VALUES((SELECT n FROM c))`.
//! The CTE never shadows a real table as the insert *target*: `INSERT INTO c …`
//! is still `no such table: c`. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn with_before_insert_values_runs() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    // The CTE is in scope but unreferenced here — the plain VALUES still runs.
    c.execute("WITH c AS (SELECT 1 a) INSERT INTO t VALUES(2)")
        .unwrap();
    // …and referenced from a scalar subquery inside the VALUES list.
    c.execute("WITH c(n) AS (VALUES(5)) INSERT INTO t VALUES((SELECT n FROM c))")
        .unwrap();
    let rows = c.query("SELECT a FROM t ORDER BY a").unwrap();
    assert_eq!(rows.rows.len(), 2);
}

#[test]
fn cte_does_not_shadow_the_insert_target() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    let err = c
        .execute("WITH c AS (SELECT 1 a) INSERT INTO c VALUES(2)")
        .unwrap_err()
        .to_string();
    assert!(err.contains("no such table: c"), "got: {err}");
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_start_matches("stepping, ")
            .trim_end()
            .to_string()
    };
    for sql in [
        // WITH + plain VALUES.
        "CREATE TABLE t(a); WITH c AS (SELECT 1 a) INSERT INTO t VALUES(2); SELECT a FROM t;",
        // WITH + VALUES referencing the CTE via a scalar subquery.
        "CREATE TABLE t(a); WITH c(n) AS (VALUES(5)) INSERT INTO t VALUES((SELECT n FROM c)); SELECT a FROM t;",
        // WITH + DEFAULT VALUES.
        "CREATE TABLE t(a DEFAULT 7); WITH c AS (SELECT 1 a) INSERT INTO t DEFAULT VALUES; SELECT a FROM t;",
        // WITH + INSERT … SELECT (the form that always worked — regression guard).
        "CREATE TABLE t(a); WITH c(n) AS (VALUES(1),(2)) INSERT INTO t SELECT n FROM c; SELECT a FROM t ORDER BY a;",
        // The CTE name must not shadow a real table as the insert target.
        "CREATE TABLE t(a); WITH c AS (SELECT 1 a) INSERT INTO c VALUES(2);",
        // Multi-row VALUES with the CTE in scope but unused.
        "CREATE TABLE t(a); WITH c AS (SELECT 9 a) INSERT INTO t VALUES(1),(2),(3); SELECT count(*) FROM t;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
