//! `ALL` and `DISTINCT` are reserved quantifier keywords: valid only directly
//! after `SELECT`, or as the first token inside an aggregate call
//! (`count(DISTINCT a)`, `count(ALL a)` — `ALL` is the default). In any other,
//! expression-operand position SQLite rejects them as a syntax error pointing at
//! the keyword: `1 > ALL (SELECT 1)` → `near "ALL": syntax error`. Only one
//! quantifier is allowed, so `count(ALL DISTINCT a)` reports `near "DISTINCT"`.
//!
//! graphite previously (a) failed to accept `ALL` as an aggregate quantifier
//! (`count(ALL a)` → `near "a"`) and (b) mis-parsed an operand-position
//! `ALL`/`DISTINCT` as a column/function, so the error pointed at the wrong
//! token. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

const SETUP: &str = "CREATE TABLE t(a,b,c); INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(1,3,'z');";

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a,b,c)").unwrap();
    c.execute("INSERT INTO t VALUES(3,1,'x'),(1,2,'y'),(1,3,'z')")
        .unwrap();
    c
}

#[test]
fn all_quantifier_is_accepted_in_aggregates() {
    let c = conn();
    // `ALL` is the default — `count(ALL a)` counts every non-null `a` (3),
    // unlike `count(DISTINCT a)` (2).
    let one = |sql: &str| match c.query(sql).unwrap().rows[0][0] {
        Value::Integer(i) => i,
        ref v => panic!("expected int, got {v:?}"),
    };
    assert_eq!(one("SELECT count(ALL a) FROM t"), 3);
    assert_eq!(one("SELECT count(DISTINCT a) FROM t"), 2);
    assert_eq!(one("SELECT sum(ALL a) FROM t"), 5);
    assert_eq!(one("SELECT count(a) FROM t"), 3);
}

#[test]
fn quantifier_in_operand_position_is_rejected() {
    let c = conn();
    for sql in [
        "SELECT 1 > ALL (SELECT 1)",
        "SELECT a FROM t WHERE a > ALL (SELECT a FROM t)",
        "SELECT 1 = ALL (1,2)",
    ] {
        let e = c.query(sql).unwrap_err().to_string();
        assert!(
            e.contains(r#"near "ALL": syntax error"#),
            "for {sql}: {e:?}"
        );
    }
    for sql in [
        "SELECT 1 > DISTINCT (SELECT 1)",
        "SELECT 1 + DISTINCT",
        // Double quantifier `ALL DISTINCT` — the second keyword is the offender.
        "SELECT count(ALL DISTINCT a) FROM t",
    ] {
        let e = c.query(sql).unwrap_err().to_string();
        assert!(
            e.contains(r#"near "DISTINCT": syntax error"#),
            "for {sql}: {e:?}",
        );
    }
    // Double quantifier `DISTINCT ALL` — points at the second keyword `ALL`.
    let e = c
        .query("SELECT count(DISTINCT ALL a) FROM t")
        .unwrap_err()
        .to_string();
    assert!(e.contains(r#"near "ALL": syntax error"#), "{e:?}");
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
    for sql in [
        // Quantifiers that are accepted.
        "SELECT ALL 1",
        "SELECT ALL(1)",
        "SELECT ALL a FROM t",
        "SELECT DISTINCT 1",
        "SELECT DISTINCT a FROM t",
        "SELECT count(ALL a) FROM t",
        "SELECT count(DISTINCT a) FROM t",
        "SELECT sum(ALL a) FROM t",
        "SELECT group_concat(ALL a) FROM t",
        // Operand-position rejections (error points at the keyword).
        "SELECT 1 > ALL (SELECT 1)",
        "SELECT 1 > DISTINCT (SELECT 1)",
        "SELECT a FROM t WHERE a > ALL (SELECT a FROM t)",
        "SELECT 1 = ALL (1,2)",
        "SELECT 1 + DISTINCT",
        // Double quantifier.
        "SELECT count(ALL DISTINCT a) FROM t",
        "SELECT count(DISTINCT ALL a) FROM t",
    ] {
        let full = format!("{SETUP} {sql}");
        assert_eq!(run("sqlite3", &full), run(g, &full), "for {sql}");
    }
}
