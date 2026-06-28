//! `ALTER TABLE … RENAME TO` must rewrite the renamed table's name everywhere a
//! dependent trigger reaches it — including references buried in a subquery
//! inside the trigger's `WHEN` guard, or inside a body statement's
//! `WHERE`/`SET`/`VALUES`/`ON CONFLICT` subquery, even when the trigger is
//! attached to (and its body otherwise targets) a *different* table.
//!
//! graphite's "is this trigger affected?" check (`trigger_uses_table`) only
//! inspected the `ON` table and each body statement's direct target, so a
//! trigger whose *only* reference to the renamed table sat in a `WHEN` subquery
//! — or in a body `WHERE … IN (SELECT … FROM <old>)` — was skipped entirely,
//! leaving stale `FROM <old>` text that no longer matched SQLite. The rewrite
//! itself is a whole-text token pass, so once the trigger is recognized as
//! affected every reference (WHEN + body) is renamed; the bug was purely in the
//! detection. Now the `WHEN` clause and every nested body subquery are probed.
//!
//! Verified against the sqlite3 3.50.4 CLI.

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

#[test]
fn rename_rewrites_subquery_refs_in_when_and_body() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // WHEN guard: EXISTS subquery over the renamed table, body untouched.
        "CREATE TABLE t(a); CREATE TABLE u(b); \
         CREATE TRIGGER tr BEFORE INSERT ON u WHEN EXISTS(SELECT 1 FROM t) BEGIN \
           SELECT 1; END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // WHEN guard: scalar subquery comparison over the renamed table.
        "CREATE TABLE t(a); CREATE TABLE u(b); \
         CREATE TRIGGER tr AFTER INSERT ON u WHEN (SELECT count(*) FROM t)>0 BEGIN \
           SELECT 1; END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Body UPDATE whose WHERE has an `IN (SELECT … FROM <old>)` subquery.
        "CREATE TABLE t(a); CREATE TABLE u(b); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           UPDATE u SET b=1 WHERE b IN (SELECT a FROM t); END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Body DELETE whose WHERE has an `IN (SELECT … FROM <old>)` subquery.
        "CREATE TABLE t(a); CREATE TABLE u(b); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           DELETE FROM u WHERE b IN (SELECT a FROM t); END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Body INSERT whose VALUES carries a scalar subquery over the renamed table.
        "CREATE TABLE t(a); CREATE TABLE u(b); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           INSERT INTO u VALUES((SELECT a FROM t LIMIT 1)); END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Body UPDATE whose SET expression is a scalar subquery over the renamed table.
        "CREATE TABLE t(a); CREATE TABLE u(b); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           UPDATE u SET b=(SELECT a FROM t LIMIT 1); END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Body INSERT … WITH x AS (SELECT … FROM <old>) — a body CTE source.
        "CREATE TABLE t(a); CREATE TABLE u(b); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           INSERT INTO u WITH x AS (SELECT a FROM t) SELECT a FROM x; END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Upsert DO UPDATE … WHERE with a subquery over the renamed table.
        "CREATE TABLE t(a); CREATE TABLE u(b UNIQUE); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN \
           INSERT INTO u VALUES(1) ON CONFLICT(b) DO UPDATE SET b=2 \
             WHERE b IN (SELECT a FROM t); END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Regression guard: a trigger that never touches the renamed table is
        // left byte-for-byte unchanged (no spurious rewrite).
        "CREATE TABLE t(a); CREATE TABLE u(b); CREATE TABLE w(c); \
         CREATE TRIGGER tr AFTER INSERT ON u WHEN EXISTS(SELECT 1 FROM w) BEGIN \
           INSERT INTO w VALUES(1); END; \
         ALTER TABLE t RENAME TO t2; \
         SELECT sql FROM sqlite_schema WHERE name='tr'",
        // Round trip: the trigger still fires correctly after the rename.
        "CREATE TABLE t(a); CREATE TABLE u(b); \
         INSERT INTO t VALUES(7); \
         CREATE TRIGGER tr AFTER INSERT ON u WHEN (SELECT count(*) FROM t)>0 BEGIN \
           UPDATE u SET b=b+(SELECT a FROM t LIMIT 1); END; \
         ALTER TABLE t RENAME TO t2; \
         INSERT INTO u VALUES(1); \
         SELECT b FROM u",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
