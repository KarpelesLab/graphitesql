//! `ALTER TABLE … DROP COLUMN` parity with SQLite's refusal rules and messages.
//!
//! SQLite rejects a column-level PRIMARY KEY / UNIQUE (and an INTEGER PRIMARY
//! KEY, or any column named by a table-level PRIMARY KEY) outright; everything
//! else is reported by regenerating the schema without the column and
//! re-parsing it, so a table CHECK, a generated column, a table UNIQUE, a table
//! FOREIGN KEY, or an explicit index that *references* the dropped column yields
//! `error in {table|index} … after drop column: …`, while a constraint that
//! does not reference it drops cleanly. graphite used to refuse every one of
//! these unconditionally (rejecting valid drops) and emitted its own ad-hoc
//! messages. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn err(c: &mut Connection, sql: &str) -> String {
    c.execute(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn structural_columns_are_refused_with_sqlite_messages() {
    let cases: &[(&str, &str)] = &[
        (
            "CREATE TABLE t(a PRIMARY KEY, b)",
            "cannot drop PRIMARY KEY column: \"a\"",
        ),
        (
            "CREATE TABLE t(a PRIMARY KEY)",
            "cannot drop PRIMARY KEY column: \"a\"",
        ),
        (
            "CREATE TABLE t(a INTEGER PRIMARY KEY, b)",
            "cannot drop PRIMARY KEY column: \"a\"",
        ),
        (
            "CREATE TABLE t(a, b, PRIMARY KEY(a, b))",
            "cannot drop PRIMARY KEY column: \"a\"",
        ),
        (
            "CREATE TABLE t(a UNIQUE, b)",
            "cannot drop UNIQUE column: \"a\"",
        ),
        (
            "CREATE TABLE t(a UNIQUE)",
            "cannot drop UNIQUE column: \"a\"",
        ),
        (
            "CREATE TABLE t(a)",
            "cannot drop column \"a\": no other columns exist",
        ),
    ];
    for (ddl, msg) in cases {
        let mut c = Connection::open_memory().unwrap();
        c.execute(ddl).unwrap();
        assert_eq!(
            err(&mut c, "ALTER TABLE t DROP COLUMN a"),
            *msg,
            "for {ddl}"
        );
    }
}

#[test]
fn references_to_the_dropped_column_report_reparse_errors() {
    let cases: &[(&str, &str)] = &[
        (
            "CREATE TABLE t(a, b, CHECK(a > 0))",
            "error in table t after drop column: no such column: a",
        ),
        (
            "CREATE TABLE t(a, b AS (a + 1), c)",
            "error in table t after drop column: no such column: a",
        ),
        (
            "CREATE TABLE t(a, b, UNIQUE(a))",
            "error in table t after drop column: no such column: a",
        ),
        (
            "CREATE TABLE t(a, b, c, FOREIGN KEY(a) REFERENCES p(x))",
            "error in table t after drop column: unknown column \"a\" in foreign key definition",
        ),
    ];
    for (ddl, msg) in cases {
        let mut c = Connection::open_memory().unwrap();
        c.execute("CREATE TABLE p(x PRIMARY KEY)").unwrap();
        c.execute(ddl).unwrap();
        assert_eq!(
            err(&mut c, "ALTER TABLE t DROP COLUMN a"),
            *msg,
            "for {ddl}"
        );
    }
}

#[test]
fn an_index_referencing_the_dropped_column_names_the_index() {
    for (idx, _) in [
        ("CREATE INDEX i ON t(a)", ""),
        ("CREATE INDEX i ON t(a + b)", ""),
        ("CREATE INDEX i ON t(b) WHERE a > 0", ""),
    ] {
        let mut c = Connection::open_memory().unwrap();
        c.execute("CREATE TABLE t(a, b)").unwrap();
        c.execute(idx).unwrap();
        assert_eq!(
            err(&mut c, "ALTER TABLE t DROP COLUMN a"),
            "error in index i after drop column: no such column: a",
            "for {idx}"
        );
    }
}

#[test]
fn a_constraint_not_referencing_the_column_drops_cleanly() {
    // Each of these names `a` in neither a CHECK, generated expr, index, nor FK,
    // so the drop succeeds (graphite used to refuse them all).
    for ddl in [
        "CREATE TABLE t(a, b, CHECK(b > 0))",
        "CREATE TABLE t(a, b, c AS (b + 1))",
        "CREATE TABLE t(a CHECK(a > 0), b)", // the dropped column's own CHECK
        "CREATE TABLE t(a REFERENCES p, b)", // the dropped column's own FK
        "CREATE TABLE t(a COLLATE NOCASE, b)",
    ] {
        let mut c = Connection::open_memory().unwrap();
        c.execute("CREATE TABLE p(x PRIMARY KEY)").unwrap();
        c.execute(ddl).unwrap();
        c.execute("ALTER TABLE t DROP COLUMN a")
            .unwrap_or_else(|e| panic!("{ddl}: {e}"));
        // The column is gone and the table still reads.
        c.query("SELECT b FROM t").unwrap();
    }
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
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    let p = "CREATE TABLE p(x PRIMARY KEY);";
    for sql in [
        "CREATE TABLE t(a PRIMARY KEY, b); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a PRIMARY KEY); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a INTEGER PRIMARY KEY, b); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a, b, PRIMARY KEY(a,b)); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a UNIQUE, b); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a UNIQUE); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a, b, CHECK(a>0)); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a, b AS (a+1), c); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a, b, UNIQUE(a)); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a, b, FOREIGN KEY(a) REFERENCES p(x)); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a, b); CREATE INDEX i ON t(a); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a, b); CREATE INDEX i ON t(a+b); ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a, b); CREATE INDEX i ON t(b) WHERE a>0; ALTER TABLE t DROP COLUMN a;",
        "CREATE TABLE t(a, b, CHECK(b>0)); ALTER TABLE t DROP COLUMN a; SELECT 'ok';",
        "CREATE TABLE t(a CHECK(a>0), b); ALTER TABLE t DROP COLUMN a; SELECT 'ok';",
        "CREATE TABLE t(a REFERENCES p, b); ALTER TABLE t DROP COLUMN a; SELECT 'ok';",
        "CREATE TABLE t(a COLLATE NOCASE, b); ALTER TABLE t DROP COLUMN a; SELECT 'ok';",
    ] {
        let full = format!("{p}{sql}");
        assert_eq!(run("sqlite3", &full), run(g, &full), "for {sql}");
    }
}
