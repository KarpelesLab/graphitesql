//! `trim`/`ltrim`/`rtrim` with a NULL second argument (the set of characters to
//! strip) return NULL, like any other NULL argument — sqlite propagates the
//! NULL rather than treating it as an empty trim-set. graphite previously
//! returned the input string unchanged. The one-arg forms and a NULL *subject*
//! were already NULL. Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn null_trim_set_yields_null() {
    let c = Connection::open_memory().unwrap();
    for sql in [
        "SELECT ltrim('  x', NULL)",
        "SELECT rtrim('x  ', NULL)",
        "SELECT trim('  x  ', NULL)",
    ] {
        assert_eq!(c.query(sql).unwrap().rows[0][0], Value::Null, "for {sql}");
    }
    // A non-NULL set still trims, and the empty set is a no-op (not NULL).
    assert_eq!(
        c.query("SELECT ltrim('xxabc', 'x')").unwrap().rows[0][0],
        Value::Text("abc".into())
    );
    assert_eq!(
        c.query("SELECT ltrim('abc', '')").unwrap().rows[0][0],
        Value::Text("abc".into())
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let run = |sql: &str| -> String {
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg(sql)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    };
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run_g = |sql: &str| -> String {
        let out = Command::new(g).arg(":memory:").arg(sql).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    };
    for sql in [
        "SELECT quote(ltrim('  x', NULL))",
        "SELECT quote(rtrim('x  ', NULL))",
        "SELECT quote(trim('  x  ', NULL))",
        "SELECT typeof(trim('x', NULL))",
        "SELECT quote(trim('--a--', '-'))",
    ] {
        assert_eq!(run(sql), run_g(sql), "for {sql}");
    }
}
