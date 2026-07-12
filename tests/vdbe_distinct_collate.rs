//! Row-level `SELECT DISTINCT` dedups each output column under its resolved
//! collation. The single-table scan path and the nested-loop join path thread the
//! per-column collation into `DistinctCheck` (resolved with the same
//! `explicit_collation`/`col_collation` logic as an ORDER BY key), so a column with
//! a *declared* `NOCASE`/`RTRIM` collation — and an explicit `COLLATE BINARY` —
//! runs on the VDBE. An explicit *non-BINARY* `COLLATE` on a projection still
//! defers to the tree-walker via the `projections_have_explicit_collation` guard
//! (the grouped/aggregate DISTINCT paths likewise still defer any non-BINARY
//! collation). Verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn conn() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT, b INT)").unwrap();
    c.execute("INSERT INTO t VALUES ('x',1),('X',1),('y',2)")
        .unwrap();
    c
}

#[test]
fn distinct_with_explicit_collate_defers_but_is_correct() {
    let c = conn();
    // Explicit non-BINARY collation in the projection: the VDBE must defer.
    for q in [
        "SELECT DISTINCT a COLLATE NOCASE FROM t",
        "SELECT DISTINCT a COLLATE NOCASE, b FROM t",
        "SELECT DISTINCT (a COLLATE NOCASE) FROM t",
    ] {
        assert!(c.query_vdbe(q).is_err(), "expected VDBE to defer on {q}");
    }
    // The default path (tree-walker fallback) collapses 'x'=='X' under NOCASE.
    assert_eq!(
        c.query("SELECT DISTINCT a COLLATE NOCASE FROM t ORDER BY 1")
            .unwrap()
            .rows,
        vec![vec![Value::Text("x".into())], vec![Value::Text("y".into())]],
    );

    // Explicit COLLATE BINARY is the default comparison and stays on the VDBE:
    // 'x' and 'X' remain distinct.
    let r = c
        .query_vdbe("SELECT DISTINCT a COLLATE BINARY FROM t ORDER BY 1")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Text("X".into())],
            vec![Value::Text("x".into())],
            vec![Value::Text("y".into())],
        ],
    );
}

#[test]
fn distinct_over_declared_collation_column_runs_on_vdbe() {
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT COLLATE NOCASE, b INT, c TEXT COLLATE RTRIM)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('A',1,'x '),('a',2,'x'),('B',3,'y'),('a',4,'x  ')")
        .unwrap();
    let setup = "CREATE TABLE t(a TEXT COLLATE NOCASE, b INT, c TEXT COLLATE RTRIM);\n\
                 INSERT INTO t VALUES ('A',1,'x '),('a',2,'x'),('B',3,'y'),('a',4,'x  ');\n";
    for q in [
        "SELECT DISTINCT a FROM t ORDER BY 1", // NOCASE-declared column
        "SELECT DISTINCT c FROM t ORDER BY 1", // RTRIM-declared column
        "SELECT DISTINCT a, c FROM t ORDER BY 1, 2", // two collated columns
        "SELECT DISTINCT a COLLATE BINARY FROM t ORDER BY 1", // explicit BINARY override
    ] {
        // The declared-collation column now dedups on the VDBE (no longer defers).
        let r = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE to run {q}: {e}"));
        if !sqlite {
            continue;
        }
        let got: Vec<Vec<String>> = r
            .rows
            .iter()
            .map(|row| {
                row.iter()
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
            .arg(format!("{setup}{q};"))
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

#[test]
fn join_distinct_over_collation_columns_runs_on_vdbe() {
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT COLLATE NOCASE, x INT)")
        .unwrap();
    c.execute("CREATE TABLE u(b TEXT COLLATE NOCASE, y INT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('A',1),('a',2),('B',3)")
        .unwrap();
    c.execute("INSERT INTO u VALUES ('a',10),('c',20)").unwrap();
    let setup = "CREATE TABLE t(a TEXT COLLATE NOCASE, x INT);\n\
                 CREATE TABLE u(b TEXT COLLATE NOCASE, y INT);\n\
                 INSERT INTO t VALUES ('A',1),('a',2),('B',3);\n\
                 INSERT INTO u VALUES ('a',10),('c',20);\n";
    for q in [
        "SELECT DISTINCT t.a FROM t JOIN u ON t.a=u.b ORDER BY 1",
        "SELECT DISTINCT t.a, u.b FROM t, u ORDER BY 1, 2",
        "SELECT DISTINCT t.a COLLATE BINARY FROM t, u ORDER BY 1",
    ] {
        // A nested-loop join DISTINCT over NOCASE columns now runs on the VDBE.
        let r = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE to run {q}: {e}"));
        if !sqlite {
            continue;
        }
        let got: Vec<Vec<String>> = r
            .rows
            .iter()
            .map(|row| {
                row.iter()
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
            .arg(format!("{setup}{q};"))
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

#[test]
fn distinct_with_explicit_collate_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = conn();
    let setup = "CREATE TABLE t(a TEXT, b INT);\nINSERT INTO t VALUES ('x',1),('X',1),('y',2);\n";
    for q in [
        "SELECT DISTINCT a COLLATE NOCASE FROM t ORDER BY 1",
        "SELECT DISTINCT a COLLATE NOCASE, b FROM t ORDER BY 1, 2",
        "SELECT DISTINCT a COLLATE BINARY FROM t ORDER BY 1",
    ] {
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
                        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                    })
                    .collect()
            })
            .collect();
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg("-ascii")
            .arg(format!("{setup}{q};"))
            .output()
            .unwrap();
        assert!(out.status.success(), "sqlite3 failed on {q}");
        let text = String::from_utf8(out.stdout).unwrap();
        let expected: Vec<Vec<String>> = text
            .split('\u{1e}')
            .filter(|r| !r.is_empty())
            .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
            .collect();
        assert_eq!(got, expected, "diverged on {q}");
    }
}
