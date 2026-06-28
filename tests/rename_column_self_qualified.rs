//! `ALTER TABLE … RENAME COLUMN` must propagate into the table's *own* schema
//! text not only for bare references (`CHECK(a > 0)`) but also for
//! `<table>.col`-qualified self-references — a CHECK written `CHECK(t.a > 0)`,
//! a column-level `CHECK(t.a > 0)`, and a partial index's `WHERE t.col > 0` —
//! exactly as SQLite rewrites them.
//!
//! graphite rewrote the renamed column in the stored CREATE text with a token
//! pass that skipped every `x.col` (after-`.`) token to avoid renaming an
//! unrelated `other.col`. But in a single-table definition the only possible
//! qualifier *is* the table itself, so a `t.a` reference was left stale. The
//! fix rewrites the table's own definition (and an index over it) with the
//! table name as an accepted qualifier, while preserving each occurrence's
//! original double-quoting like SQLite does.
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
fn rename_column_propagates_into_self_qualified_refs() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Table-level CHECK with a `t.col` self-qualified reference.
        "CREATE TABLE t(a, b, CHECK(t.a > 0)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Column-level CHECK with a `t.col` self-qualified reference.
        "CREATE TABLE t(a CHECK(t.a>0), b); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Mixed bare + `t.col` in one CHECK: both must be renamed.
        "CREATE TABLE t(a, b, CHECK(a > 0 AND t.a < 100)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Partial index over this table whose WHERE uses `t.col`.
        "CREATE TABLE t(a, b); CREATE INDEX ix ON t(a) WHERE t.a > 0; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='ix'",
        // Quote preservation: a double-quoted `"t"."a"` stays double-quoted.
        "CREATE TABLE t(a, b, CHECK(\"t\".\"a\" > 0)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Quote preservation: a quoted occurrence stays quoted even when the
        // new name is typed bare, while the bare column-list entry stays bare.
        "CREATE TABLE t(a, b, CHECK(\"a\">0)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Quote preservation: a bare occurrence is quoted when the *new* name is
        // typed double-quoted (so the bare column-list entry becomes quoted too).
        "CREATE TABLE t(a, b, CHECK(\"a\">0)); \
         ALTER TABLE t RENAME COLUMN \"a\" TO \"aa\"; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // A quoted column-list entry with a bare CHECK occurrence: each keeps
        // its own quoting.
        "CREATE TABLE t(\"a\", b, CHECK(a>0)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Bare-only CHECK (control): unchanged behavior, still matches.
        "CREATE TABLE t(a, b, CHECK(a > 0)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Regression guard: a same-named column/index on an *unrelated* table is
        // left untouched when a different table's column is renamed.
        "CREATE TABLE t(a); CREATE TABLE u(a); CREATE INDEX ix ON u(a) WHERE u.a>0; \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='ix'",
        // Functional round trip: the CHECK still enforces after the rename.
        "CREATE TABLE t(a, b, CHECK(t.a > 0)); INSERT INTO t VALUES(5, 1); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'; SELECT count(*) FROM t",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
