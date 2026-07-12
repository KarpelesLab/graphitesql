//! Track A: `ORDER BY … NULLS FIRST/LAST` and `IS [NOT] DISTINCT FROM`.
//! Verified differentially against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => String::from(s.as_str()),
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

#[test]
fn nulls_first_last() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v)")
        .unwrap();
    c.execute("INSERT INTO t(id,v) VALUES (1,3),(2,NULL),(3,1),(4,NULL),(5,2)")
        .unwrap();
    // Default ASC → NULLs first.
    assert_eq!(
        rows_str(&c, "SELECT v FROM t ORDER BY v, id"),
        "NULL\nNULL\n1\n2\n3"
    );
    // Default DESC → NULLs last.
    assert_eq!(
        rows_str(&c, "SELECT v FROM t ORDER BY v DESC, id"),
        "3\n2\n1\nNULL\nNULL"
    );
    // Explicit NULLS LAST under ASC.
    assert_eq!(
        rows_str(&c, "SELECT v FROM t ORDER BY v ASC NULLS LAST, id"),
        "1\n2\n3\nNULL\nNULL"
    );
    // Explicit NULLS FIRST under DESC.
    assert_eq!(
        rows_str(&c, "SELECT v FROM t ORDER BY v DESC NULLS FIRST, id"),
        "NULL\nNULL\n3\n2\n1"
    );
}

#[test]
fn is_distinct_from() {
    let c = Connection::open_memory().unwrap();
    // IS NOT DISTINCT FROM is null-aware equality; IS DISTINCT FROM its negation.
    let q = |sql: &str| -> i64 {
        match c.query(sql).unwrap().rows[0][0] {
            Value::Integer(i) => i,
            _ => panic!("not an int"),
        }
    };
    assert_eq!(q("SELECT 1 IS NOT DISTINCT FROM 1"), 1);
    assert_eq!(q("SELECT 1 IS NOT DISTINCT FROM 2"), 0);
    assert_eq!(q("SELECT NULL IS NOT DISTINCT FROM NULL"), 1);
    assert_eq!(q("SELECT NULL IS NOT DISTINCT FROM 1"), 0);
    assert_eq!(q("SELECT 1 IS DISTINCT FROM NULL"), 1);
    assert_eq!(q("SELECT NULL IS DISTINCT FROM NULL"), 0);
    assert_eq!(q("SELECT 1 IS DISTINCT FROM 1"), 0);
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, a, b TEXT);\
                 INSERT INTO t(id,a,b) VALUES \
                 (1,3,'x'),(2,NULL,'y'),(3,1,NULL),(4,NULL,'x'),(5,2,'z')";
    let queries = [
        "SELECT id FROM t ORDER BY a, id",
        "SELECT id FROM t ORDER BY a DESC, id",
        "SELECT id FROM t ORDER BY a ASC NULLS LAST, id",
        "SELECT id FROM t ORDER BY a DESC NULLS FIRST, id",
        "SELECT id FROM t ORDER BY b NULLS FIRST, id",
        "SELECT id FROM t ORDER BY b NULLS LAST, id",
        "SELECT count(*) FROM t WHERE a IS NOT DISTINCT FROM NULL",
        "SELECT count(*) FROM t WHERE a IS DISTINCT FROM NULL",
        "SELECT id FROM t WHERE b IS NOT DISTINCT FROM 'x' ORDER BY id",
    ];

    let path = std::env::temp_dir().join(format!("gsql-od-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(setup)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = rows_str(&g, q);
        if got != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    let _ = std::fs::remove_file(&path);
    assert!(
        failures.is_empty(),
        "{} queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
