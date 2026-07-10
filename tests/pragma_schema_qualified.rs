//! Schema-qualified introspection PRAGMAs: `PRAGMA <db>.table_info(t)` and the
//! other metadata pragmas honour the `<db>.` qualifier, targeting `main`, `temp`,
//! or an attached database (matching SQLite). The qualifier was previously dropped
//! by the parser, so a qualified pragma always inspected the active (main) schema —
//! returning empty for a table that lives only in an attached/temp database.
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

fn strip(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim_start().starts_with("PRAGMA") && !l.contains("error here"))
        .map(|l| {
            l.strip_prefix("Error: in prepare, ")
                .or_else(|| l.strip_prefix("Error: error: "))
                .or_else(|| l.strip_prefix("Error: "))
                .unwrap_or(l)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn schema_qualified_introspection_pragmas_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let cases = [
        // Attached database.
        "ATTACH ':memory:' AS aux; CREATE TABLE aux.t(a INT, b); PRAGMA aux.table_info(t)",
        "ATTACH ':memory:' AS aux; CREATE TABLE aux.t(a,b,PRIMARY KEY(a)) WITHOUT ROWID; \
         PRAGMA aux.table_xinfo(t)",
        "ATTACH ':memory:' AS aux; CREATE TABLE aux.t(a); CREATE INDEX aux.i ON t(a); \
         PRAGMA aux.index_list(t)",
        "ATTACH ':memory:' AS aux; CREATE TABLE aux.t(a,b); CREATE INDEX aux.i ON t(a,b); \
         PRAGMA aux.index_info(i)",
        "ATTACH ':memory:' AS aux; CREATE TABLE aux.t(a,b); CREATE INDEX aux.i ON t(a DESC); \
         PRAGMA aux.index_xinfo(i)",
        "ATTACH ':memory:' AS aux; CREATE TABLE aux.p(id PRIMARY KEY); \
         CREATE TABLE aux.c(x REFERENCES p(id)); PRAGMA aux.foreign_key_list(c)",
        // The WITHOUT ROWID PK auto-index resolves in the attached schema too.
        "ATTACH ':memory:' AS aux; CREATE TABLE aux.t(a,b,PRIMARY KEY(a)) WITHOUT ROWID; \
         PRAGMA aux.index_info('sqlite_autoindex_t_1')",
        // main / temp qualifiers.
        "CREATE TABLE t(a INT); PRAGMA main.table_info(t)",
        "CREATE TEMP TABLE t(a INT); PRAGMA temp.table_info(t)",
        // The pragma TVF's 2nd argument is the schema.
        "ATTACH ':memory:' AS aux; CREATE TABLE aux.t(a INT, b); \
         SELECT * FROM pragma_table_info('t','aux')",
        // A same-named table in two databases resolves to the qualified one.
        "CREATE TABLE t(a INT); ATTACH ':memory:' AS aux; CREATE TABLE aux.t(x TEXT, y); \
         PRAGMA main.table_info(t); PRAGMA aux.table_info(t)",
        // The unqualified form is unchanged.
        "CREATE TABLE t(a INT); PRAGMA table_info(t)",
    ];
    for sql in cases {
        assert_eq!(out("sqlite3", sql), out(g, sql), "for {sql}");
    }

    // An unknown database name errors `unknown database <name>` (prefix stripped).
    for sql in ["PRAGMA nope.table_info(t)", "PRAGMA nope.index_list(t)"] {
        let s = strip(&out("sqlite3", sql));
        let gr = strip(&out(g, sql));
        assert!(
            s.contains("unknown database nope"),
            "sqlite should reject {sql}: {s}"
        );
        assert_eq!(s, gr, "for {sql}");
    }
}
