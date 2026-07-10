//! `PRAGMA table_info` (and `table_xinfo`) reports a column's declared type using
//! SQLite's canonical *standard* type-name spelling: a type that is exactly one of
//! `ANY` / `BLOB` / `INT` / `INTEGER` / `REAL` / `TEXT` (case-insensitive) is
//! reported UPPERCASE — so `InTeGeR`, `integer`, and bare `  int  ` all show
//! `INTEGER`/`INT`. Any other type text is preserved verbatim: a length spec
//! (`VARCHAR(5)`, `INT(3)`), a non-standard name (`mediumint`, `numeric`,
//! `double`), and — because SQLite compares the stored type string without trimming
//! — a quoted type whose content carries interior whitespace (`"integer "`).
//!
//! (This mirrors SQLite's `sqlite3StdType` string sharing; the stored schema `sql`
//! text still preserves the type exactly as written.) Verified vs sqlite3 3.50.4.

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
fn table_info_canonicalizes_standard_type_names() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        "CREATE TABLE t(a InTeGeR); PRAGMA table_info(t)",
        "CREATE TABLE t(a integer, b tExT, c ReAl, d BloB, e any, f InT); PRAGMA table_info(t)",
        // Bare surrounding whitespace: the type token excludes it, so it canonicalizes.
        "CREATE TABLE t(a   int   , b TEXT ); PRAGMA table_info(t)",
        // Non-standard / length-spec types are preserved verbatim.
        "CREATE TABLE t(a numeric, b varchar(5), c int(3), d mediumint, e double); PRAGMA table_info(t)",
        // Quoted type with interior whitespace is preserved; without, it canonicalizes.
        "CREATE TABLE t(a \"integer \"); PRAGMA table_info(t)",
        "CREATE TABLE t(a \"integer\", b [int], c `text`); PRAGMA table_info(t)",
        // STRICT tables and table_xinfo share the same reporting.
        "CREATE TABLE t(a int, b text) STRICT; PRAGMA table_info(t)",
        "CREATE TABLE t(a InT, b c, PRIMARY KEY(a)); PRAGMA table_xinfo(t)",
        // The stored schema text keeps the original casing.
        "CREATE TABLE t(a InTeGeR); SELECT sql FROM sqlite_master",
        // Untyped columns stay empty.
        "CREATE TABLE t(a, b); PRAGMA table_info(t)",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }
}
