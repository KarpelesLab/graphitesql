//! A-alter-2b: `ALTER TABLE … RENAME COLUMN` that would leave a dependent *trigger*
//! body unresolvable is rejected and rolled back, matching SQLite
//! (`error in trigger NAME after rename: <detail>`).
//!
//! graphite can't query a trigger the way it queries a view, so it resolves the
//! trigger's real body `SELECT` ASTs — the source of an `INSERT … SELECT` and a
//! body `SELECT` step — against the post-rename schema, with `NEW`/`OLD`/`RAISE`
//! neutralised, and rejects only when one fails with a genuine renamed-column
//! resolution error. Probing the real AST keeps the detail byte-identical to
//! SQLite. Breaks reachable only through an `UPDATE`/`DELETE`/`VALUES`/`WHEN`
//! expression subquery are a known residual (graphite still accepts the rename, as
//! it did for every trigger break before this) — never a *wrong* rejection.
//!
//! Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn out(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    s.trim_end().to_string()
}

/// Strip each CLI's error-line prefix so the shared message compares equal.
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
fn breaking_trigger_rename_is_rejected_and_rolled_back() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Each rename breaks the trigger body's INSERT…SELECT (or body SELECT) source;
    // both engines reject with the same detail and roll back. The trailing schema
    // dump proves the table column and trigger were rolled back untouched.
    let cases = [
        // Derived table projecting the renamed column, consumed → no such column.
        "CREATE TABLE t(a,b); CREATE TABLE log(x); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log SELECT a FROM (SELECT a FROM t); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // USING(col) join whose column vanishes → cannot join using.
        "CREATE TABLE t(a,b); CREATE TABLE u(a,c); CREATE TABLE log(x); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN INSERT INTO log SELECT b FROM t JOIN u USING(a); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // CTE inside the INSERT…SELECT source.
        "CREATE TABLE t(a,b); CREATE TABLE log(x); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log \
           SELECT a FROM (WITH z AS (SELECT a FROM t) SELECT a FROM z); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // Alias-qualified derived reference — the outer `s.a` dangles identically.
        "CREATE TABLE t(a,b); CREATE TABLE log(x); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log SELECT s.a FROM (SELECT a FROM t) s; END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // A body SELECT step (not an INSERT) with a broken derived source.
        "CREATE TABLE t(a,b); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT a FROM (SELECT a FROM t); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
    ];
    for sql in cases {
        let s = strip(&out("sqlite3", sql));
        let gr = strip(&out(g, sql));
        assert!(
            s.contains("after rename:"),
            "sqlite should reject: {sql}\n{s}"
        );
        assert_eq!(s, gr, "reject/rollback mismatch for {sql}");
    }
}

#[test]
fn valid_trigger_rename_is_accepted() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // Renames the trigger propagation fully handles (or that don't touch it): none
    // may be rejected. The full schema dump must match sqlite exactly.
    let cases = [
        // NEW.<col> reference — always rewritten, never a break.
        "CREATE TABLE t(a,b); CREATE TABLE log(x); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.a); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // A correlated subquery over the UPDATE target must resolve (not false-reject).
        "CREATE TABLE t(a,b); CREATE TABLE u(c,d); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN UPDATE u SET d=(SELECT max(a) FROM t WHERE t.b=u.c); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // Single-source INSERT…SELECT — propagated.
        "CREATE TABLE t(a,b); CREATE TABLE log(x); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log SELECT a FROM t; END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // A `NEW.<col>` inside a body-subquery WHERE — neutralised, not a break.
        "CREATE TABLE t(a,b); CREATE TABLE log(x); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log SELECT b FROM t WHERE a=NEW.a; END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // Renaming a column the trigger never references.
        "CREATE TABLE t(a,b); CREATE TABLE log(x); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log SELECT a FROM (SELECT a FROM t); END; \
         ALTER TABLE t RENAME COLUMN b TO bb; SELECT type,name,sql FROM sqlite_master",
        // A cross-source mixed-scope INSERT…SELECT the propagation rewrites.
        "CREATE TABLE t(a,b); CREATE TABLE u(c); CREATE TABLE log(x); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN INSERT INTO log SELECT a FROM t WHERE a IN (SELECT c FROM u); END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
        // RAISE body: neutralised, resolves, accepted.
        "CREATE TABLE t(a,b); \
         CREATE TRIGGER tr BEFORE INSERT ON t BEGIN SELECT CASE WHEN NEW.a<0 THEN RAISE(ABORT,'neg') END; END; \
         ALTER TABLE t RENAME COLUMN a TO aa; SELECT type,name,sql FROM sqlite_master",
    ];
    for sql in cases {
        assert_eq!(
            out("sqlite3", sql),
            out(g, sql),
            "valid trigger rename mismatch for {sql}"
        );
    }
}
