//! `UPDATE t SET (c1, c2, …) = (SELECT …)` — row-value-subquery assignment. The
//! subquery is evaluated once per target row (correlated allowed); its first
//! row's columns are assigned to the listed columns (no row → NULLs). Verified
//! differentially against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "<N>".into(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

/// Build `setup` + run `mutate`, then read `read` back, in both engines.
fn check(setup: &[&str], mutate: &str, read: &str) {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // graphite.
    let mut c = Connection::open_memory().unwrap();
    for s in setup {
        c.execute(s).unwrap();
    }
    c.execute(mutate).unwrap();
    let got = c
        .query(read)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("~");

    // sqlite (unique path per call — tests run in parallel).
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let db = std::env::temp_dir().join(format!("gsql-updrs-{}-{n}.db", std::process::id()));
    let db = db.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&db);
    for s in setup {
        let o = Command::new("sqlite3").arg(&db).arg(s).output().unwrap();
        assert!(o.status.success());
    }
    Command::new("sqlite3")
        .arg(&db)
        .arg(mutate)
        .output()
        .unwrap();
    let o = Command::new("sqlite3").arg(&db).arg(read).output().unwrap();
    let want = String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| {
            l.split('|')
                .map(|x| if x.is_empty() { "<N>" } else { x })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("~");
    let _ = std::fs::remove_file(&db);

    assert_eq!(got, want, "diverged for: {mutate}");
}

#[test]
fn multi_column_constant_subquery() {
    check(
        &[
            "CREATE TABLE t(a,b,c)",
            "INSERT INTO t VALUES(1,2,3),(4,5,6)",
        ],
        "UPDATE t SET (a,b)=(SELECT 90,80)",
        "SELECT a,b,c FROM t ORDER BY c",
    );
}

#[test]
fn correlated_single_column_subquery() {
    check(
        &[
            "CREATE TABLE t(a,b)",
            "CREATE TABLE u(k,v)",
            "INSERT INTO t VALUES(1,0),(2,0)",
            "INSERT INTO u VALUES(1,'x'),(2,'y')",
        ],
        "UPDATE t SET (b)=(SELECT v FROM u WHERE k=t.a)",
        "SELECT a,b FROM t ORDER BY a",
    );
}

#[test]
fn empty_subquery_assigns_nulls() {
    check(
        &["CREATE TABLE t(a,b)", "INSERT INTO t VALUES(1,2)"],
        "UPDATE t SET (a,b)=(SELECT 9,8 WHERE 0)",
        "SELECT a,b FROM t",
    );
}

#[test]
fn row_subquery_mixed_with_scalar_assignment() {
    check(
        &["CREATE TABLE t(a,b,c)", "INSERT INTO t VALUES(1,2,3)"],
        "UPDATE t SET (a,c)=(SELECT 7,8), b=99",
        "SELECT a,b,c FROM t",
    );
}

#[test]
fn row_subquery_swap_is_simultaneous() {
    // `(a,b)=(SELECT b,a)` swaps, evaluated against the original row.
    check(
        &["CREATE TABLE t(a,b)", "INSERT INTO t VALUES(1,2),(3,4)"],
        "UPDATE t SET (a,b)=(SELECT b,a)",
        "SELECT a,b FROM t ORDER BY b",
    );
}

#[test]
fn row_subquery_on_without_rowid() {
    check(
        &[
            "CREATE TABLE t(a PRIMARY KEY,b) WITHOUT ROWID",
            "INSERT INTO t VALUES('k',1),('m',2)",
        ],
        "UPDATE t SET (b)=(SELECT 42) WHERE a='k'",
        "SELECT a,b FROM t ORDER BY a",
    );
}

#[test]
fn row_subquery_with_where() {
    check(
        &[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, a, b)",
            "INSERT INTO t VALUES(1,10,20),(2,30,40)",
        ],
        "UPDATE t SET (a,b)=(SELECT a*2, b*2) WHERE id=2",
        "SELECT id,a,b FROM t ORDER BY id",
    );
}
