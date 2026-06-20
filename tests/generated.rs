//! Track A: generated columns (`… AS (expr) [STORED|VIRTUAL]`).
//!
//! Verified against `sqlite3`: VIRTUAL columns are computed on read and not
//! stored; STORED columns are materialized on write; writes to generated columns
//! are rejected; indexes over generated columns work; and a graphitesql-written
//! file passes `sqlite3 integrity_check`.

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

fn render_rows(r: &graphitesql::QueryResult) -> String {
    r.rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn virtual_and_stored() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT, b INT, v INT AS (a+b) VIRTUAL, s INT AS (a*b) STORED)")
        .unwrap();
    c.execute("INSERT INTO t(a,b) VALUES (3,4),(10,20)")
        .unwrap();
    let r = c.query("SELECT a,b,v,s FROM t ORDER BY a").unwrap();
    assert_eq!(render_rows(&r), "3|4|7|12\n10|20|30|200");

    // Cannot insert into a generated column.
    assert!(c.execute("INSERT INTO t(a,b,v) VALUES (1,2,3)").is_err());
    assert!(c.execute("INSERT INTO t(a,b,s) VALUES (1,2,3)").is_err());
    // Cannot update one either.
    assert!(c.execute("UPDATE t SET v = 99 WHERE a = 3").is_err());

    // Updating a base column recomputes the generated ones.
    c.execute("UPDATE t SET b = 5 WHERE a = 3").unwrap();
    let r = c.query("SELECT v, s FROM t WHERE a = 3").unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(8)); // 3+5
    assert_eq!(r.rows[0][1], Value::Integer(15)); // 3*5
}

#[test]
fn default_is_virtual() {
    // `AS (expr)` without STORED/VIRTUAL defaults to VIRTUAL.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x INT, y AS (x * 2))").unwrap();
    c.execute("INSERT INTO t(x) VALUES (21)").unwrap();
    assert_eq!(
        c.query("SELECT y FROM t").unwrap().rows[0][0],
        Value::Integer(42)
    );
}

#[test]
fn index_on_generated_column() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-gen-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(a INT, b INT, sum INT AS (a+b) STORED)")
            .unwrap();
        c.execute("CREATE INDEX isum ON t(sum)").unwrap();
        for i in 1..=50 {
            c.execute(&format!("INSERT INTO t(a,b) VALUES ({i}, {})", i * 2))
                .unwrap();
        }
    }
    // Real sqlite reads it, the stored generated column + its index are valid.
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check;")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
    // And the index answers a lookup correctly.
    let c = Connection::open_readonly(&path).unwrap();
    let r = c.query("SELECT a FROM t WHERE sum = 9").unwrap(); // a + 2a = 9 -> a=3
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Integer(3));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn generated_against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let stmts = [
        "CREATE TABLE t(a INT, b INT, c TEXT, v AS (a+b) VIRTUAL, s AS (b*b) STORED, t2 AS (c || '!') VIRTUAL)",
        "INSERT INTO t(a,b,c) VALUES (1,2,'x'),(5,5,'y'),(10,3,'z')",
        "UPDATE t SET a = a + 100 WHERE c = 'y'",
        "DELETE FROM t WHERE c = 'z'",
    ];
    let query = "SELECT a,b,c,v,s,t2 FROM t ORDER BY rowid";

    let path = std::env::temp_dir().join(format!("gsql-gen2-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let script = format!("{};{query};", stmts.join(";"));
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(&script)
        .output()
        .unwrap();
    let want = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    let _ = std::fs::remove_file(&path);

    let mut g = Connection::open_memory().unwrap();
    for s in stmts {
        g.execute(s).unwrap();
    }
    let got = render_rows(&g.query(query).unwrap());
    assert_eq!(got, want);
}

#[test]
fn table_must_have_a_non_generated_column() {
    // SQLite rejects a table whose every column is generated.
    let mut c = Connection::open_memory().unwrap();
    assert!(c
        .execute("CREATE TABLE t(x INT GENERATED ALWAYS AS (1) VIRTUAL)")
        .is_err());
    assert!(c
        .execute("CREATE TABLE t(a AS (1) STORED, b AS (2) STORED)")
        .is_err());
    // A single real column alongside generated ones is fine.
    assert!(c.execute("CREATE TABLE t(a, b AS (a + 1))").is_ok());
}
