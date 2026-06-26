//! `substr(X, NULL [, …])` returns NULL — a NULL start position propagates like
//! any other NULL argument (the length argument already did). graphite
//! previously coerced a NULL start to 0 and returned the whole string. Matched
//! to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn null_start_yields_null() {
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT substr('abc', NULL)",
        "SELECT substr('abc', NULL, 2)",
        "SELECT substr(x'010203', NULL)",
    ] {
        assert_eq!(c.query(sql).unwrap().rows[0][0], Value::Null, "for {sql}");
    }
    // A real start still slices.
    assert_eq!(
        c.query("SELECT substr('abcdef', 2, 3)").unwrap().rows[0][0],
        Value::Text("bcd".into())
    );
    assert_eq!(
        c.query("SELECT substr('abc', -1)").unwrap().rows[0][0],
        Value::Text("c".into())
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
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    };
    for sql in [
        "SELECT quote(substr('abc', NULL))",
        "SELECT quote(substr('abc', NULL, 2))",
        "SELECT quote(substr('abc', 1, NULL))",
        "SELECT quote(substr('abcdef', 2, 3))",
        "SELECT typeof(substr('abc', NULL))",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
