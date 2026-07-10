//! A-alter-2: `ALTER TABLE … RENAME COLUMN` that would leave a dependent view
//! unresolvable is rejected *and rolled back*, matching SQLite. SQLite applies the
//! rename, re-validates every dependent, and on failure rolls the whole statement
//! back and errors `error in view NAME after rename: <detail>`. graphite now does
//! the same via a writer savepoint around the rewrite + a post-rename probe of each
//! dependent view. A valid rename (even one graphite propagates through niche
//! shapes like an alias collision) is *not* rejected.
//!
//! Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Combined stdout+stderr of a one-shot run, trimmed.
fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s.trim_end().to_string()
}

/// Strip each CLI's error-line prefix so the shared `error in view …` message
/// compares equal (the prefixes are the CLI-2 residual).
fn strip(s: &str) -> String {
    s.lines()
        .map(|l| {
            l.strip_prefix("Error: stepping, ")
                .or_else(|| l.strip_prefix("Error: error: "))
                .or_else(|| l.strip_prefix("Runtime error near line 1: "))
                .or_else(|| l.strip_prefix("Error: "))
                .unwrap_or(l)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn breaking_rename_is_rejected_and_rolled_back() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");

    // Each rename breaks a dependent view; both engines reject with the same detail
    // and roll back (the table column keeps its old name). We dump the whole schema
    // so the rejected-and-unchanged table + view are compared too.
    let cases = [
        // USING(col) join whose column vanishes → "cannot join using …".
        "CREATE TABLE t(a,b); CREATE TABLE u(a,c); \
         CREATE VIEW v AS SELECT * FROM t JOIN u USING(a); \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // Derived table in main position projecting the renamed column, consumed →
        // "no such column: a".
        "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT a FROM (SELECT a FROM t); \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // CTE whose renamed output column is consumed by the outer query.
        "CREATE TABLE t(a,b); CREATE VIEW v AS WITH x AS (SELECT a FROM t) SELECT a FROM x; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
    ];
    for sql in cases {
        let s = strip(&out("sqlite3", sql));
        let gr = strip(&out(g, sql));
        assert!(
            s.contains("after rename:"),
            "sqlite should reject: {sql}\n{s}"
        );
        // The rolled-back schema (table col unchanged) + the identical error line.
        assert_eq!(s, gr, "reject/rollback mismatch for {sql}");
    }
}

#[test]
fn valid_rename_is_accepted() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // These renames all resolve after propagation and must NOT be rejected — the
    // full schema dump (renamed table + rewritten view) must match sqlite exactly.
    let cases = [
        "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT a FROM t; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // Alias collision — span-precise rewrite keeps the alias, renames the ref.
        "CREATE TABLE t(a,b); CREATE VIEW v AS SELECT b AS a, a FROM t; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // NATURAL JOIN degrades to a cross-join (still resolves) — not a break.
        "CREATE TABLE t(a,b); CREATE TABLE u(a,c); CREATE VIEW v AS SELECT * FROM t NATURAL JOIN u; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // Cross-source subquery graphite rewrites.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); \
         CREATE VIEW v AS SELECT c FROM u WHERE c IN (SELECT a FROM t); \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
    ];
    for sql in cases {
        assert_eq!(
            out("sqlite3", sql),
            out(g, sql),
            "valid rename mismatch for {sql}"
        );
    }
}
