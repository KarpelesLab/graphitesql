//! `PRAGMA journal_size_limit` reports the journal-shrink cap in bytes, with a
//! default of -1 ("no limit"). graphite returned no row at all for the getter;
//! it now stores and reports the value like sqlite, clamping any negative value
//! to -1. graphite does not actually honor the cap (its journal handling differs)
//! — the pragma is advisory, exactly as `analysis_limit`/`busy_timeout` are.
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn int(c: &Connection, sql: &str) -> i64 {
    let r = c.query(sql).unwrap();
    assert_eq!(r.rows.len(), 1, "expected one row for {sql:?}");
    match &r.rows[0][0] {
        Value::Integer(n) => *n,
        other => panic!("expected integer for {sql:?}, got {other:?}"),
    }
}

#[test]
fn default_is_minus_one() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(int(&c, "PRAGMA journal_size_limit"), -1);
}

#[test]
fn set_value_round_trips() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA journal_size_limit=4096").unwrap();
    assert_eq!(int(&c, "PRAGMA journal_size_limit"), 4096);
    c.execute("PRAGMA journal_size_limit=0").unwrap();
    assert_eq!(int(&c, "PRAGMA journal_size_limit"), 0);
}

#[test]
fn negative_clamps_to_minus_one() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA journal_size_limit=4096").unwrap();
    c.execute("PRAGMA journal_size_limit=-2").unwrap();
    assert_eq!(int(&c, "PRAGMA journal_size_limit"), -1);
}

#[test]
fn setter_via_query_echoes_the_value() {
    // Through the library query path the setter echoes the resulting value,
    // like sqlite (the CLI routes a `=` setter through execute(), which yields
    // no row — a separate, pre-existing convention shared by all stored pragmas).
    let c = Connection::open_memory().unwrap();
    assert_eq!(int(&c, "PRAGMA journal_size_limit=8192"), 8192);
    assert_eq!(int(&c, "PRAGMA journal_size_limit=-9"), -1);
}

#[test]
fn matches_sqlite_cli_getter() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    // The getter form is byte-identical. (A `=` setter's echo is dropped by the
    // CLI's setter routing — a separate, pre-existing convention — so a
    // set-then-get CLI sequence is intentionally not compared here.)
    let cases = ["PRAGMA journal_size_limit"];
    for sql in cases {
        let s = Command::new("sqlite3")
            .arg(":memory:")
            .arg(sql)
            .output()
            .unwrap();
        let gg = Command::new(g).arg(":memory:").arg(sql).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&s.stdout),
            String::from_utf8_lossy(&gg.stdout),
            "for {sql:?}"
        );
    }
}
