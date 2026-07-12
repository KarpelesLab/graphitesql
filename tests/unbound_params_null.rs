//! An unbound SQL parameter reads as NULL, exactly as SQLite's bind API does (a
//! statement stepped without a value bound to a `?`/`?N`/`:name`/`@name`/`$name`
//! sees NULL there). graphite previously raised an "unbound parameter" error.
//! Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

#[test]
fn unbound_parameters_read_as_null() {
    let c = Connection::open_memory().unwrap();
    // Positional, numbered, anonymous and named — all unbound — are NULL, and a
    // NULL parameter propagates through arithmetic (`?1 + 5` is NULL).
    for q in [
        "SELECT ?1, ?2, ?1 + 5",
        "SELECT ?, ?, ?",
        "SELECT :x, @y, $z",
        "SELECT typeof(?1), ifnull(?1, 'was-null')",
    ] {
        let rows = c.query(q).unwrap_or_else(|e| panic!("{q}: {e}")).rows;
        for cell in rows.into_iter().flatten() {
            // Only NULL, or a value derived from NULL (typeof→'null', ifnull→its
            // default) — never an error.
            assert!(
                matches!(&cell, Value::Null | Value::Text(_)),
                "unexpected non-null-derived cell {cell:?} for `{q}`"
            );
        }
    }
    // typeof(?1) is 'null'.
    assert_eq!(
        c.query("SELECT typeof(?1)").unwrap().rows,
        vec![vec![Value::Text("null".into())]],
    );
}

#[test]
fn unbound_parameters_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = Connection::open_memory().unwrap();
    for q in [
        "SELECT ?1, ?2",
        "SELECT :x, @y, $z",
        "SELECT ?1 + 5, typeof(?2), coalesce(?3, 'd')",
    ] {
        // graphite's rendering (NULL -> empty, matching sqlite -ascii).
        let got: Vec<Vec<String>> = c
            .query(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| match v {
                        Value::Null => String::new(),
                        Value::Integer(i) => i.to_string(),
                        Value::Text(s) => String::from(s.as_str()),
                        Value::Real(x) => graphitesql::exec::eval::format_real(*x),
                        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                    })
                    .collect()
            })
            .collect();
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg("-ascii")
            .arg(format!("{q};"))
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let expected: Vec<Vec<String>> = text
            .split('\u{1e}')
            .filter(|r| !r.is_empty())
            .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
            .collect();
        assert_eq!(got, expected, "diverged on {q}");
    }
}
