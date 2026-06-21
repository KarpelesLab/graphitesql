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

// --- json_each / json_tree path argument (roadmap: real bug fix) ---

fn pairs(c: &Connection, sql: &str) -> Vec<(String, String)> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| (render(&r[0]), render(&r[1])))
        .collect()
}

#[test]
fn json_each_navigates_a_path_argument() {
    let c = Connection::open_memory().unwrap();
    // An array at the path iterates its elements (keys = indices).
    assert_eq!(
        pairs(
            &c,
            "SELECT key, value FROM json_each('{\"a\":[10,20]}', '$.a')"
        ),
        [("0".into(), "10".into()), ("1".into(), "20".into())]
    );
    // An object at the path iterates its members.
    assert_eq!(
        pairs(
            &c,
            "SELECT key, value FROM json_each('{\"a\":{\"b\":5,\"c\":6}}', '$.a')"
        ),
        [("b".into(), "5".into()), ("c".into(), "6".into())]
    );
    // A scalar at the path is a single row with a NULL key.
    assert_eq!(
        pairs(&c, "SELECT key, value FROM json_each('{\"a\":7}', '$.a')"),
        [(String::new(), "7".into())]
    );
    // fullkey/path are rooted at the navigated path.
    assert_eq!(
        pairs(
            &c,
            "SELECT fullkey, path FROM json_each('{\"a\":[10,20]}', '$.a')"
        ),
        [
            ("$.a[0]".into(), "$.a".into()),
            ("$.a[1]".into(), "$.a".into())
        ]
    );
    // A path that does not resolve yields no rows.
    assert!(c
        .query("SELECT * FROM json_each('{\"a\":1}', '$.zzz')")
        .unwrap()
        .rows
        .is_empty());
}

#[test]
fn json_tree_roots_at_a_path_argument() {
    let c = Connection::open_memory().unwrap();
    let r = c
        .query("SELECT key, value, type, fullkey, path FROM json_tree('{\"x\":[1,2]}', '$.x')")
        .unwrap();
    // Root row: key = the path's last component, fullkey = the path, path = its parent.
    assert_eq!(render(&r.rows[0][0]), "x");
    assert_eq!(render(&r.rows[0][2]), "array");
    assert_eq!(render(&r.rows[0][3]), "$.x");
    assert_eq!(render(&r.rows[0][4]), "$");
    // Children are walked under the path.
    assert_eq!(render(&r.rows[1][3]), "$.x[0]");
    assert_eq!(render(&r.rows[1][1]), "1");
    assert_eq!(render(&r.rows[2][3]), "$.x[1]");
}

#[test]
fn explain_query_plan_over_a_virtual_table() {
    // Previously EXPLAIN QUERY PLAN over a virtual table errored ("schema sql is
    // not CREATE TABLE"); it now renders sqlite's VIRTUAL TABLE node shape.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE s USING series(1, 5)")
        .unwrap();
    let detail = |sql: &str| -> String {
        match c.query(sql).unwrap().rows.last().unwrap().last().unwrap() {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        }
    };
    assert_eq!(
        detail("EXPLAIN QUERY PLAN SELECT * FROM s"),
        "SCAN s VIRTUAL TABLE INDEX 0:"
    );
    // A pushed constraint shows in the index string (graphite's own plan number).
    assert!(detail("EXPLAIN QUERY PLAN SELECT * FROM s WHERE value > 2")
        .starts_with("SCAN s VIRTUAL TABLE INDEX "));
}

#[test]
fn pragma_table_info_over_a_virtual_table() {
    // table_info over a vtab previously errored; it now lists the module's columns
    // (cid, name; type/notnull/dflt/pk are empty/0 — the safe module interface
    // carries no per-column type info).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE s USING series(1, 5)")
        .unwrap();
    let r = c.query("PRAGMA table_info(s)").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Integer(0)); // cid
    assert_eq!(r.rows[0][1], Value::Text("value".into())); // name
}
