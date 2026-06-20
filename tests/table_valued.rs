//! Track D: table-valued functions — `generate_series`. Verified against sqlite3.

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

#[test]
fn basic() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        rows_str(&c, "SELECT value FROM generate_series(1, 5)"),
        "1\n2\n3\n4\n5"
    );
    assert_eq!(
        rows_str(&c, "SELECT value FROM generate_series(0, 10, 3)"),
        "0\n3\n6\n9"
    );
    assert_eq!(
        rows_str(&c, "SELECT value FROM generate_series(5, 1, -2)"),
        "5\n3\n1"
    );
    // Aggregations and WHERE over the series.
    assert_eq!(
        rows_str(&c, "SELECT sum(value) FROM generate_series(1, 100)"),
        "5050"
    );
    assert_eq!(
        rows_str(
            &c,
            "SELECT count(*) FROM generate_series(1, 20) WHERE value % 2 = 0"
        ),
        "10"
    );
}

#[test]
fn join_with_series() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n)")
        .unwrap();
    c.execute("INSERT INTO t(n) VALUES (10),(20)").unwrap();
    // Cross join a table with a series.
    let got = rows_str(
        &c,
        "SELECT t.id, s.value FROM t, generate_series(1, 2) AS s ORDER BY t.id, s.value",
    );
    assert_eq!(got, "1|1\n1|2\n2|1\n2|2");
}

#[test]
fn json_each_and_tree() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let c = Connection::open_memory().unwrap();
    // Stable columns only — `id`/`parent` are SQLite-internal node offsets.
    let queries = [
        r#"SELECT key,value,type,atom,fullkey,path FROM json_each('{"a":1,"b":[7,8],"c":"x"}')"#,
        r#"SELECT key,value,type,fullkey,path FROM json_each('[10,20,30]')"#,
        r#"SELECT key,value,type,fullkey,path FROM json_tree('{"a":1,"b":[7,8]}')"#,
        r#"SELECT value FROM json_each('[1,2,3,4]') WHERE value > 2"#,
        r#"SELECT count(*), sum(value) FROM json_each('[5,10,15]')"#,
    ];
    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(q)
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = rows_str(&c, q);
        if got != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} json_each/json_tree queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let queries = [
        "SELECT value FROM generate_series(1, 8)",
        "SELECT value FROM generate_series(2, 20, 5)",
        "SELECT value FROM generate_series(10, 0, -3)",
        "SELECT sum(value), count(*), min(value), max(value) FROM generate_series(1, 50)",
        "SELECT value*value FROM generate_series(1, 5)",
    ];
    let c = Connection::open_memory().unwrap();
    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(q)
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = rows_str(&c, q);
        if got != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} generate_series queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
