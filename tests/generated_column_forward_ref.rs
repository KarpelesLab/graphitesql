//! A generated column's expression may reference another column declared *later*
//! in the table — including another generated column. SQLite resolves these
//! forward references topologically, so `b AS (c+1), c AS (a+1)` yields the same
//! values regardless of declaration order. graphite used to evaluate generated
//! columns in declaration order, so a forward-referenced generated column was
//! still NULL when its dependent was computed.
//!
//! A *cycle* among generated columns is rejected at CREATE (before any row is
//! inserted) with `generated column loop on "X"`, where `X` is the column whose
//! expression closes the cycle (generated columns visited in declaration order).
//! graphite used to accept the cyclic table and silently produce NULLs.
//! Verified vs sqlite3 3.50.4.
#![cfg(feature = "std")]

use std::process::Command;

fn run(bin: &str, sql: &str) -> String {
    let out = Command::new(bin)
        .arg(":memory:")
        .arg(sql)
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if !line.is_empty() {
            return line.to_string();
        }
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr.lines() {
        if line.starts_with('^') {
            continue;
        }
        let s = line
            .strip_prefix("Error: in prepare, ")
            .or_else(|| line.strip_prefix("Error: stepping, "))
            .or_else(|| line.strip_prefix("Error: SQL error: "))
            .or_else(|| line.strip_prefix("Error: "))
            .unwrap_or(line);
        let s = s.strip_prefix("error: ").unwrap_or(s);
        let s = s.rsplit_once(" (").map_or(s, |(head, tail)| {
            if tail
                .trim_end_matches(')')
                .chars()
                .all(|c| c.is_ascii_digit())
            {
                head
            } else {
                s
            }
        });
        return s.to_string();
    }
    String::new()
}

fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn same(sql: &str) {
    let g = run(env!("CARGO_BIN_EXE_graphitesql"), sql);
    let s = run("sqlite3", sql);
    assert_eq!(g, s, "mismatch for SQL: {sql}");
}

#[test]
fn forward_reference_resolves_virtual() {
    if !sqlite3_available() {
        return;
    }
    // b references c (declared later), c references a — both resolve.
    same(
        "CREATE TABLE t(a, b AS (c+1), c AS (a+1)); INSERT INTO t(a) VALUES(1); SELECT b,c FROM t;",
    );
    // A three-deep forward chain.
    same(
        "CREATE TABLE t(a, b AS (c+1), c AS (d+1), d AS (a+1)); \
         INSERT INTO t(a) VALUES(1); SELECT b,c,d FROM t;",
    );
    // The same dependency referenced twice in one expression.
    same(
        "CREATE TABLE t(a, b AS (c+c), c AS (a+1)); INSERT INTO t(a) VALUES(3); SELECT b,c FROM t;",
    );
    // A generated column may forward-reference a *plain* column too.
    same("CREATE TABLE t(a, b AS (z+1), z); INSERT INTO t(a,z) VALUES(1,5); SELECT b FROM t;");
    // Backward references (declaration order) keep working.
    same(
        "CREATE TABLE t(a, c AS (a+1), b AS (c+1)); INSERT INTO t(a) VALUES(5); SELECT b,c FROM t;",
    );
}

#[test]
fn forward_reference_resolves_stored() {
    if !sqlite3_available() {
        return;
    }
    // STORED generated columns (materialized on the write path) resolve forward
    // references too, and integrity_check passes on the persisted record.
    same(
        "CREATE TABLE t(a, b AS (c+1) STORED, c AS (a+1) STORED); \
         INSERT INTO t(a) VALUES(1); SELECT b,c FROM t;",
    );
    same(
        "CREATE TABLE t(a, b AS (c+1) STORED, c AS (a+1) STORED); \
         INSERT INTO t(a) VALUES(7); PRAGMA integrity_check;",
    );
    // A STORED column depending on a VIRTUAL column declared later.
    same(
        "CREATE TABLE t(a, b AS (c+1) STORED, c AS (a*2)); \
         INSERT INTO t(a) VALUES(4); SELECT b,c FROM t;",
    );
    // Mixed stored/virtual deeper chain.
    same(
        "CREATE TABLE t(a, b AS (c*2), c AS (d+1) STORED, d AS (a+10)); \
         INSERT INTO t(a) VALUES(1); SELECT a,b,c,d FROM t;",
    );
}

#[test]
fn cycle_rejected_at_create_with_named_column() {
    if !sqlite3_available() {
        return;
    }
    // Self-loop.
    same("CREATE TABLE t(a, b AS (b+1));");
    // Two-cycle: the column closing the cycle (second visited) is named.
    same("CREATE TABLE t(a, b AS (c+1), c AS (b+1));");
    same("CREATE TABLE t(a, c AS (b+1), b AS (c+1));");
    // Three-cycle, in two declaration orders.
    same("CREATE TABLE t(a, b AS (c+1), c AS (d+1), d AS (b+1));");
    same("CREATE TABLE t(a, d AS (b+1), b AS (c+1), c AS (d+1));");
    // The cycle reference embedded in a larger expression.
    same("CREATE TABLE t(a, b AS (a+c), c AS (a+b));");
    same("CREATE TABLE t(a, x AS (y+a), y AS (x*2));");
    // The error is raised at CREATE even when an INSERT follows.
    same("CREATE TABLE t(a, b AS (b+1)); INSERT INTO t(a) VALUES(1);");
}
