//! `PRAGMA table_list` — one row per table/view across all databases plus each
//! database's synthetic schema table. Verified as an unordered row set against
//! the `sqlite3` CLI (sqlite emits rows in unspecified hash order).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

/// Render a graphite result as a sorted set of `schema|name|type|ncol|wr|strict`.
fn graphite_set(c: &Connection, sql: &str) -> Vec<String> {
    let mut v: Vec<String> = c
        .query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| {
            let cell = |i: usize| match &r[i] {
                Value::Text(s) => s.clone(),
                Value::Integer(n) => n.to_string(),
                other => format!("{other:?}"),
            };
            format!(
                "{}|{}|{}|{}|{}|{}",
                cell(0),
                cell(1),
                cell(2),
                cell(3),
                cell(4),
                cell(5)
            )
        })
        .collect();
    v.sort();
    v
}

fn sqlite_set(setup_and_query: &str) -> Option<Vec<String>> {
    let out = std::process::Command::new("sqlite3")
        .arg(":memory:")
        .arg(setup_and_query)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut v: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    v.sort();
    Some(v)
}

#[test]
fn table_list_matches_sqlite() {
    // Build the same schema in graphite and sqlite3, then compare the row set.
    let ddl = "CREATE TABLE t(a,b); \
               CREATE VIEW v AS SELECT 1 AS x, 2 AS y; \
               CREATE TABLE w(k PRIMARY KEY, val) WITHOUT ROWID; \
               CREATE TABLE s(x INTEGER) STRICT; \
               ATTACH ':memory:' AS aux; \
               CREATE TABLE aux.u(b,c,d);";

    let mut c = Connection::open_memory().unwrap();
    for stmt in ddl.split(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let got = graphite_set(&c, "PRAGMA table_list");

    // A few invariants hold regardless of whether sqlite3 is installed.
    assert!(got.contains(&"main|t|table|2|0|0".to_string()));
    assert!(got.contains(&"main|v|view|2|0|0".to_string())); // view column count
    assert!(got.contains(&"main|w|table|2|1|0".to_string())); // WITHOUT ROWID -> wr=1
    assert!(got.contains(&"main|s|table|1|0|1".to_string())); // STRICT -> strict=1
    assert!(got.contains(&"aux|u|table|3|0|0".to_string()));
    assert!(got.contains(&"main|sqlite_schema|table|5|0|0".to_string()));
    assert!(got.contains(&"temp|sqlite_temp_schema|table|5|0|0".to_string()));
    assert!(got.contains(&"aux|sqlite_schema|table|5|0|0".to_string()));

    // Exact set-equality with sqlite3 when available.
    if let Some(want) = sqlite_set(&format!("{ddl} PRAGMA table_list;")) {
        assert_eq!(got, want, "graphite table_list set != sqlite3");
    }
}

#[test]
fn collation_list_reports_builtins() {
    let c = Connection::open_memory().unwrap();
    let names: Vec<String> = c
        .query("PRAGMA collation_list")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match &r[1] {
            Value::Text(s) => s.clone(),
            other => format!("{other:?}"),
        })
        .collect();
    // graphite implements exactly the three built-in collating sequences, listed
    // in sqlite's order (`BINARY`, `NOCASE`, `RTRIM`) with sequential ids.
    assert_eq!(names, vec!["BINARY", "NOCASE", "RTRIM"]);
    let seqs: Vec<i64> = c
        .query("PRAGMA collation_list")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Integer(n) => n,
            ref other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(seqs, vec![0, 1, 2]);
}

#[test]
fn table_list_filtered_by_name() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a,b)").unwrap();
    c.execute("CREATE TABLE other(z)").unwrap();
    // `PRAGMA table_list('t')` returns only the named object, no schema rows.
    let got = graphite_set(&c, "PRAGMA table_list('t')");
    assert_eq!(got, vec!["main|t|table|2|0|0".to_string()]);
}

#[test]
fn pragma_table_valued_function_without_parens() {
    // `FROM pragma_<name>` (no parentheses) is the 0-arg table-valued form.
    let mut c = Connection::open_memory().unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    let names: Vec<_> = c
        .query("SELECT name FROM pragma_database_list ORDER BY seq")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(
        names,
        vec![Value::Text("main".into()), Value::Text("aux".into())]
    );
    // A real table of the same name still wins (no shadowing).
    c.execute("CREATE TABLE pragma_x(z)").unwrap();
    c.execute("INSERT INTO pragma_x VALUES(7)").unwrap();
    assert_eq!(
        c.query("SELECT z FROM pragma_x").unwrap().rows[0][0],
        Value::Integer(7)
    );
}
