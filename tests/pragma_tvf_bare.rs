//! A bare pragma table-valued function (`FROM pragma_table_info WHERE arg='t'`)
//! takes its pragma argument from an equality constraint on the hidden `arg`
//! column, mirroring SQLite's eponymous pragma vtab. Every pragma TVF — bare or
//! called — also exposes hidden `arg`/`schema` columns echoing those values, and
//! omits them from `*` expansion. Verified against sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn rows_str(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One-shot sqlite3 CLI run over the same setup+query, joined like `rows_str`.
fn sqlite3_rows(setup: &str, sql: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{setup}{sql}"))
        .output()
        .expect("run sqlite3");
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

const SETUP: &str = "CREATE TABLE t(a INT, b TEXT REFERENCES u(z)); \
                     CREATE TABLE u(z PRIMARY KEY); CREATE INDEX ix ON t(a);";

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    c
}

#[test]
fn bare_pragma_tvf_driven_by_where_arg() {
    let c = conn();
    // The headline idiom: a bare pragma TVF driven by `WHERE arg=…`.
    assert_eq!(
        rows_str(&c, "SELECT name, type FROM pragma_table_info WHERE arg='t'"),
        "a|INT\nb|TEXT"
    );
    // Other argument-taking pragma TVFs work bare too.
    assert_eq!(
        rows_str(&c, "SELECT name FROM pragma_index_list WHERE arg='t'"),
        "ix"
    );
    assert_eq!(
        rows_str(&c, "SELECT name FROM pragma_index_info WHERE arg='ix'"),
        "a"
    );
    assert_eq!(
        rows_str(
            &c,
            "SELECT \"table\", \"from\", \"to\" FROM pragma_foreign_key_list WHERE arg='t'"
        ),
        "u|b|z"
    );
}

#[test]
fn hidden_arg_and_schema_columns_echo_and_hide() {
    let c = conn();
    // `arg`/`schema` are selectable (echoing the driving values) but excluded
    // from `*` — both in the bare form and the called form.
    assert_eq!(
        rows_str(&c, "SELECT arg, name FROM pragma_table_info WHERE arg='t'"),
        "t|a\nt|b"
    );
    assert_eq!(
        rows_str(
            &c,
            "SELECT name FROM pragma_table_info WHERE arg='t' AND schema='main'"
        ),
        "a\nb"
    );
    // `*` yields only the visible table_info columns (no arg/schema).
    assert_eq!(
        rows_str(&c, "SELECT * FROM pragma_table_info WHERE arg='t'"),
        "0|a|INT|0||0\n1|b|TEXT|0||0"
    );
    // The called form echoes its positional `arg` and a NULL `schema`.
    assert_eq!(
        rows_str(&c, "SELECT arg FROM pragma_table_info('t')"),
        "t\nt"
    );
    assert_eq!(
        rows_str(&c, "SELECT schema FROM pragma_table_info('t')"),
        "\n"
    );
    // A called pragma TVF can still be filtered on the hidden `arg`.
    assert_eq!(
        rows_str(&c, "SELECT name FROM pragma_table_info('t') WHERE arg='t'"),
        "a\nb"
    );
}

#[test]
fn bare_without_a_constraint_or_with_a_nonequality_yields_no_rows() {
    let c = conn();
    // No `arg` constraint names no object → empty (not an error), like the
    // argument-less `PRAGMA table_info`.
    assert!(c
        .query("SELECT name FROM pragma_table_info")
        .unwrap()
        .rows
        .is_empty());
    // A non-equality constraint does not drive the pragma → empty.
    assert!(c
        .query("SELECT name FROM pragma_table_info WHERE arg LIKE 't'")
        .unwrap()
        .rows
        .is_empty());
    // An unknown table name → empty.
    assert!(c
        .query("SELECT name FROM pragma_table_info WHERE arg='nonesuch'")
        .unwrap()
        .rows
        .is_empty());
}

#[test]
fn differential_against_sqlite3() {
    if !sqlite3_available() {
        return;
    }
    let c = conn();
    for sql in [
        "SELECT name, type FROM pragma_table_info WHERE arg='t'",
        "SELECT arg, name FROM pragma_table_info WHERE arg='t'",
        "SELECT name FROM pragma_table_info WHERE arg='t' AND schema='main'",
        "SELECT * FROM pragma_table_info WHERE arg='t'",
        "SELECT name FROM pragma_table_info",
        "SELECT name FROM pragma_index_list WHERE arg='t'",
        "SELECT name FROM pragma_index_info WHERE arg='ix'",
        "SELECT \"table\", \"from\", \"to\" FROM pragma_foreign_key_list WHERE arg='t'",
        "SELECT name FROM pragma_table_info('t') WHERE arg='t'",
        "SELECT arg FROM pragma_table_info('t')",
        "SELECT name FROM pragma_table_info WHERE arg='t' ORDER BY cid DESC",
        "SELECT name FROM pragma_table_info WHERE arg LIKE 't'",
        "SELECT count(*) FROM pragma_table_info WHERE arg='nonesuch'",
    ] {
        assert_eq!(
            rows_str(&c, sql),
            sqlite3_rows(SETUP, sql),
            "mismatch: {sql}"
        );
    }
}
