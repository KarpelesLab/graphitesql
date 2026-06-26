//! The `sqlite_` name prefix is reserved for SQLite's internal catalog objects.
//! A user `CREATE TABLE/INDEX/VIEW/TRIGGER/VIRTUAL TABLE` — or an
//! `ALTER … RENAME TO` — naming a new object with that prefix is rejected with
//! `object name reserved for internal use: NAME` (case-insensitive on the
//! prefix, the name echoed exactly as written). graphite previously created
//! such objects silently. The internal catalogs (`sqlite_sequence`,
//! `sqlite_stat1`) are still created on demand by AUTOINCREMENT / ANALYZE.
//! Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn reserved_prefix_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    for sql in [
        "CREATE TABLE sqlite_foo(a)",
        "CREATE TABLE SQLITE_FOO(a)",
        "CREATE TABLE \"sqlite_foo\"(a)",
        "CREATE INDEX sqlite_i ON t(a)",
        "CREATE VIEW sqlite_v AS SELECT 1",
        "CREATE TRIGGER sqlite_tr AFTER INSERT ON t BEGIN SELECT 1; END",
        "CREATE TABLE sqlite_c AS SELECT 1",
        "ALTER TABLE t RENAME TO sqlite_t",
    ] {
        let e = c.execute(sql).unwrap_err().to_string();
        assert!(
            e.contains("object name reserved for internal use:"),
            "expected reserved-name error for {sql}, got: {e}"
        );
    }
    // Names that merely start with "sqlite" (no underscore) are fine.
    c.execute("CREATE TABLE sqlite(x)").unwrap();
    c.execute("CREATE TABLE sqlitex(y)").unwrap();
    // The internal catalog is still created implicitly.
    c.execute("CREATE TABLE seq(a INTEGER PRIMARY KEY AUTOINCREMENT)")
        .unwrap();
    c.execute("INSERT INTO seq DEFAULT VALUES").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM sqlite_master WHERE name='sqlite_sequence'")
            .unwrap()
            .rows[0][0],
        graphitesql::Value::Integer(1)
    );
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
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().next().unwrap_or("");
        // Normalize the CLI's wrapper prefixes / library message prefix.
        line.trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string()
    };
    for sql in [
        "CREATE TABLE sqlite_foo(a)",
        "CREATE TABLE SQLITE_FOO(a)",
        "CREATE INDEX sqlite_i ON t(a)",
        "CREATE VIEW sqlite_v AS SELECT 1",
        "CREATE VIRTUAL TABLE sqlite_vt USING fts5(a)",
        "CREATE TABLE sqlite(a)",
        "CREATE TABLE sqlitex(a)",
    ] {
        let setup = format!("CREATE TABLE t(a);{sql}");
        assert_eq!(run("sqlite3", &setup), run(g, &setup), "for {sql}");
    }
}
