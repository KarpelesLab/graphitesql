//! `ALTER TABLE … RENAME COLUMN` must NOT rename a name inside a foreign key's
//! *parent* column list — `REFERENCES other(col)` names the parent table's
//! column, not this table's — while a self-referencing FK (`REFERENCES
//! <thistable>(col)`) and the FK's own *child* column list (`FOREIGN KEY(col)`)
//! still rename, exactly as SQLite does.
//!
//! graphite rewrote the renamed column in the table's stored CREATE text with a
//! bare-token pass that renamed *every* matching identifier, so a child table
//! with `b REFERENCES other(a)` wrongly became `REFERENCES other(aa)` when its
//! own column `a` was renamed — diverging the stored `sqlite_schema.sql` and
//! silently corrupting the foreign-key target. The fix marks the tokens inside
//! each `REFERENCES <name>( … )` group whose `<name>` is not the renamed table
//! and leaves them intact.
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
fn rename_column_leaves_fk_parent_columns_intact() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Column-level `REFERENCES other(a)`: the parent's `a` stays put while the
        // child's own column `a` renames.
        "CREATE TABLE other(a); CREATE TABLE t(a, b REFERENCES other(a)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Table-level `FOREIGN KEY(a) REFERENCES other(a)`: the child list `(a)`
        // renames; the parent list `other(a)` does not.
        "CREATE TABLE other(a); CREATE TABLE t(a, b, FOREIGN KEY(a) REFERENCES other(a)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Composite FK: only the child side of the rename moves.
        "CREATE TABLE other(a,c); CREATE TABLE t(a,c, FOREIGN KEY(a,c) REFERENCES other(a,c)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Self-FK `REFERENCES t(a)`: the parent IS this table, so it DOES rename.
        "CREATE TABLE t(a PRIMARY KEY, b REFERENCES t(a)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Self-FK via a table-level `FOREIGN KEY(b) REFERENCES t(a)`.
        "CREATE TABLE t(a PRIMARY KEY, b, FOREIGN KEY(b) REFERENCES t(a)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // A no-column-list `REFERENCES other` is unaffected.
        "CREATE TABLE other(a); CREATE TABLE t(a, b REFERENCES other); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // A quoted parent + quoted parent column stays intact.
        "CREATE TABLE other(a); CREATE TABLE t(a, b REFERENCES \"other\"(\"a\")); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // A self-qualified CHECK alongside an FK to another table: the CHECK's
        // `t.a` renames, the FK parent `other(a)` does not.
        "CREATE TABLE other(a); CREATE TABLE t(a, b REFERENCES other(a), CHECK(t.a>0)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Renaming the *child* FK column itself leaves the parent column intact.
        "CREATE TABLE other(a); CREATE TABLE t(a, b REFERENCES other(a)); \
         ALTER TABLE t RENAME COLUMN b TO bb; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
        // Functional round trip: the FK still enforces after the rename.
        "CREATE TABLE other(a PRIMARY KEY); INSERT INTO other VALUES(1); \
         CREATE TABLE t(a REFERENCES other(a)); \
         ALTER TABLE t RENAME COLUMN a TO aa; \
         SELECT sql FROM sqlite_schema WHERE name='t'",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
