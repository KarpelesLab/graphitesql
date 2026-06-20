//! Track A: row-value expressions — `(a,b) = (c,d)`, lexicographic ordering,
//! and `(a,b) IN ((…),(…))`. Verified differentially against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(), // sqlite CLI prints NULL as empty

        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn one(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
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
fn comparisons() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(one(&c, "SELECT (1,2) = (1,2)"), Value::Integer(1));
    assert_eq!(one(&c, "SELECT (1,2) = (1,3)"), Value::Integer(0));
    assert_eq!(one(&c, "SELECT (1,2) <> (1,3)"), Value::Integer(1));
    assert_eq!(one(&c, "SELECT (1,2) < (1,3)"), Value::Integer(1));
    assert_eq!(one(&c, "SELECT (1,2) < (1,2)"), Value::Integer(0));
    assert_eq!(one(&c, "SELECT (1,2) <= (1,2)"), Value::Integer(1));
    assert_eq!(one(&c, "SELECT (2,1) > (1,9)"), Value::Integer(1));
    assert_eq!(one(&c, "SELECT (1,2,3) >= (1,2,3)"), Value::Integer(1));
    // NULL element makes an otherwise-undecided comparison NULL.
    assert_eq!(one(&c, "SELECT (1,NULL) = (1,2)"), Value::Null);
    assert_eq!(one(&c, "SELECT (1,NULL) < (1,2)"), Value::Null);
    // ...but a decisive earlier element still resolves.
    assert_eq!(one(&c, "SELECT (1,NULL) < (2,5)"), Value::Integer(1));
    assert_eq!(one(&c, "SELECT (2,NULL) = (1,2)"), Value::Integer(0));
}

#[test]
fn in_list() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(one(&c, "SELECT (1,2) IN ((1,2),(3,4))"), Value::Integer(1));
    assert_eq!(one(&c, "SELECT (1,5) IN ((1,2),(3,4))"), Value::Integer(0));
    assert_eq!(
        one(&c, "SELECT (1,2) NOT IN ((1,2),(3,4))"),
        Value::Integer(0)
    );
    assert_eq!(
        one(&c, "SELECT (9,9) NOT IN ((1,2),(3,4))"),
        Value::Integer(1)
    );
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE t(a,b);\
                 INSERT INTO t VALUES (1,1),(1,2),(2,1),(2,2),(3,NULL)";
    let queries = [
        "SELECT count(*) FROM t WHERE (a,b) = (1,2)",
        "SELECT count(*) FROM t WHERE (a,b) > (1,2)",
        "SELECT a,b FROM t WHERE (a,b) >= (2,1) ORDER BY a,b",
        "SELECT count(*) FROM t WHERE (a,b) IN ((1,1),(2,2),(9,9))",
        "SELECT a,b FROM t WHERE (a,b) < (2,2) ORDER BY a,b",
        "SELECT a FROM t WHERE (a,b) = (3,NULL)",
    ];
    let path = std::env::temp_dir().join(format!("gsql-rv-{}.db", std::process::id()));
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
        "{} row-value queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
