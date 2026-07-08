//! The "ambiguous column name" error names the offending column exactly as the
//! reference was *written*: a bare `column`, a `table.column`, or a three-part
//! `schema.table.column`. graphite previously stripped any qualifier and always
//! reported the bare column name (`SELECT t.a FROM t, t` said `a`, not `t.a`).
//!
//! (A `*`/`t.*` wildcard over an unaliased self-join is also ambiguous; SQLite
//! reports it with the database-qualified expansion `main.t.a` / `temp.t.a`.
//! graphite reports `t.a` there — getting the schema prefix needs the db name
//! threaded onto `ColumnInfo`, which it does not yet carry, so that sub-case is
//! left for later and not asserted here.) Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a,b)").unwrap();
    c
}

#[test]
fn explicit_qualifier_is_echoed_in_the_message() {
    let c = conn();
    // Bare reference → bare name.
    assert!(
        c.query("SELECT a FROM t, t")
            .unwrap_err()
            .to_string()
            .contains("ambiguous column name: a")
    );
    // `table.column` reference → `t.a`.
    assert!(
        c.query("SELECT t.a FROM t, t")
            .unwrap_err()
            .to_string()
            .contains("ambiguous column name: t.a")
    );
    // `schema.table.column` reference → `main.t.a`.
    assert!(
        c.query("SELECT main.t.a FROM t, t")
            .unwrap_err()
            .to_string()
            .contains("ambiguous column name: main.t.a")
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
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_start_matches("stepping, ")
            .trim_end()
            .to_string()
    };
    const SETUP: &str = "CREATE TABLE t(a,b); CREATE TABLE u(a,c);";
    for sql in [
        "SELECT a FROM t, t",
        "SELECT t.a FROM t, t",
        "SELECT main.t.a FROM t, t",
        "SELECT a FROM t, u",
        "SELECT a FROM t t1, t t2",
        // Non-ambiguous controls (no error either side).
        "SELECT t.a, t.b FROM t",
        "SELECT a FROM t AS p, t AS q WHERE p.a = q.a",
    ] {
        let full = format!("{SETUP} {sql}");
        assert_eq!(run("sqlite3", &full), run(g, &full), "for {sql}");
    }
}
