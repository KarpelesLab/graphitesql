//! The rowid pseudo-column and its aliases (`rowid`, `_rowid_`, `oid`), both
//! bare and table-qualified (`t.rowid`), matched against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn ints(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            _ => panic!("not int: {sql}"),
        })
        .collect()
}

#[test]
fn qualified_and_bare_rowid_aliases() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT)").unwrap();
    c.execute("INSERT INTO t VALUES('x'),('y'),('z')").unwrap();

    // Bare aliases.
    assert_eq!(
        ints(&c, "SELECT rowid FROM t ORDER BY rowid"),
        vec![1, 2, 3]
    );
    assert_eq!(
        ints(&c, "SELECT _rowid_ FROM t ORDER BY _rowid_"),
        vec![1, 2, 3]
    );
    assert_eq!(ints(&c, "SELECT oid FROM t ORDER BY oid"), vec![1, 2, 3]);

    // Table-qualified aliases (previously "no such column: rowid").
    assert_eq!(
        ints(&c, "SELECT t.rowid FROM t ORDER BY t.rowid"),
        vec![1, 2, 3]
    );
    assert_eq!(
        ints(&c, "SELECT t._rowid_ FROM t ORDER BY t.rowid"),
        vec![1, 2, 3]
    );
    assert_eq!(
        ints(&c, "SELECT t.oid FROM t ORDER BY t.oid"),
        vec![1, 2, 3]
    );

    // In WHERE / with the data column alongside.
    let r = c
        .query("SELECT t.rowid, a FROM t WHERE t.rowid = 2")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(2));
    assert_eq!(r.rows[0][1], Value::Text("y".into()));
}

#[test]
fn aliased_table_qualifier() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT)").unwrap();
    c.execute("INSERT INTO t VALUES('x'),('y')").unwrap();
    // The qualifier may be a table alias.
    assert_eq!(
        ints(&c, "SELECT x.rowid FROM t AS x ORDER BY x.rowid"),
        vec![1, 2]
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let setup = "CREATE TABLE t(a TEXT); INSERT INTO t VALUES('x'),('y'),('z');";
    let query = "SELECT t.rowid || ':' || a FROM t ORDER BY t.rowid";
    let want = {
        let o = Command::new("sqlite3")
            .arg(":memory:")
            .arg(format!("{setup} {query};"))
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    let mut c = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            c.execute(s).unwrap();
        }
    }
    let got: Vec<String> = c
        .query(query)
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r[0] {
            Value::Text(t) => String::from(t.as_str()),
            _ => String::new(),
        })
        .collect();
    assert_eq!(got.join("\n"), want);
}
