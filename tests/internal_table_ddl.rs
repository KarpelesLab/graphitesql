//! SQLite forbids structural DDL on any table whose name begins with `sqlite_`
//! — the schema catalog (`sqlite_master` / `sqlite_schema`) and the other
//! internal bookkeeping tables (`sqlite_sequence`, …). `ALTER`, `DROP TABLE`
//! and `CREATE INDEX` on such a table report `table <name> may not be
//! {altered,dropped,indexed}`; the message normalises the catalog aliases to
//! `sqlite_master` (and the temp catalog to `sqlite_temp_master`) but otherwise
//! uses the table's stored name. The protection outranks `IF EXISTS` for a
//! table that exists, but a *missing* `sqlite_`-prefixed name still reports
//! `no such table`.
//!
//! graphite previously reported `no such table` for the catalog (which it
//! doesn't expose as a droppable table) and — worse — silently *performed* the
//! rename/drop/index on `sqlite_sequence`. Direct DML (`INSERT`/`DELETE`) on
//! `sqlite_sequence` stays allowed, as in SQLite.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// First non-caret output line, with the CLI error-prefixes peeled off so the
/// two shells are directly comparable.
fn run(bin: &str, sql: &str) -> String {
    let o = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    for line in s.lines() {
        let mut t = line.trim_end();
        if t.trim_start().starts_with('^') {
            continue;
        }
        // graphite: "Error: error: …"; sqlite: "Error: in prepare, …" /
        // "Error: stepping, …"; sqlite multi-stmt: "Parse error near line N: …".
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

const AUTOINC: &str = "CREATE TABLE t(a INTEGER PRIMARY KEY AUTOINCREMENT);";

#[test]
fn internal_table_ddl_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let ai = AUTOINC;
    let cases: &[String] = &[
        // The schema catalog, every alias spelling and every ALTER action.
        "ALTER TABLE sqlite_master RENAME TO x".into(),
        "ALTER TABLE sqlite_master ADD COLUMN z".into(),
        "ALTER TABLE sqlite_master RENAME COLUMN name TO nm".into(),
        "ALTER TABLE sqlite_master DROP COLUMN name".into(),
        "ALTER TABLE sqlite_schema RENAME TO x".into(),
        "ALTER TABLE SQLITE_MASTER RENAME TO x".into(),
        "ALTER TABLE main.sqlite_master RENAME TO x".into(),
        "DROP TABLE sqlite_master".into(),
        "DROP TABLE IF EXISTS sqlite_master".into(),
        "CREATE INDEX i ON sqlite_master(name)".into(),
        // A missing internal table is still `no such table` (IF EXISTS suppresses).
        "DROP TABLE IF EXISTS sqlite_stat1".into(),
        // sqlite_sequence, once it exists, is likewise protected from DDL …
        format!("{ai} ALTER TABLE sqlite_sequence RENAME TO x"),
        format!("{ai} ALTER TABLE sqlite_sequence ADD COLUMN z"),
        format!("{ai} DROP TABLE sqlite_sequence"),
        format!("{ai} DROP TABLE IF EXISTS sqlite_sequence"),
        format!("{ai} CREATE INDEX i ON sqlite_sequence(name)"),
        format!("{ai} CREATE UNIQUE INDEX i ON sqlite_sequence(seq)"),
        // … but ordinary DML on it is allowed.
        format!("{ai} DELETE FROM sqlite_sequence"),
        format!("{ai} INSERT INTO sqlite_sequence VALUES('t',5)"),
        // The temp catalog reports under its own canonical name.
        "CREATE TEMP TABLE tt(a); ALTER TABLE sqlite_temp_master RENAME TO x".into(),
    ];
    for sql in cases {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}

#[test]
fn ordinary_table_ddl_still_works() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // A user table that merely *contains* "sqlite" (not the reserved prefix) is
    // unaffected, and normal DDL on a plain table still succeeds silently.
    for sql in [
        "CREATE TABLE notsqlite_x(a); ALTER TABLE notsqlite_x RENAME TO y",
        "CREATE TABLE u(a,b); DROP TABLE u",
        "CREATE TABLE v(a,b); CREATE INDEX vi ON v(a)",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
