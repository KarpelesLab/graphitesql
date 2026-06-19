//! Phase 9: scalar subqueries and `IN (SELECT …)` (uncorrelated).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT)")
        .unwrap();
    c.execute("INSERT INTO t(a) VALUES (10), (20), (30), (40)")
        .unwrap();
    c.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, a INT)")
        .unwrap();
    c.execute("INSERT INTO u(a) VALUES (20), (40)").unwrap();
    c
}

#[test]
fn in_select() {
    let c = setup();
    let r = c
        .query("SELECT a FROM t WHERE a IN (SELECT a FROM u) ORDER BY a")
        .unwrap();
    let got: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![20, 40]);

    let r = c
        .query("SELECT count(*) FROM t WHERE a NOT IN (SELECT a FROM u)")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(2)); // 10 and 30
}

#[test]
fn scalar_subquery() {
    let c = setup();
    // As a standalone value.
    let r = c.query("SELECT (SELECT max(a) FROM t)").unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(40));

    // In a projection and in a predicate.
    let r = c
        .query("SELECT a, (SELECT count(*) FROM u) AS uc FROM t WHERE a = 30")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(30));
    assert_eq!(r.rows[0][1], Value::Integer(2));

    let r = c
        .query("SELECT a FROM t WHERE a > (SELECT min(a) FROM u) ORDER BY a")
        .unwrap();
    let got: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![30, 40]); // a > 20
}

#[test]
fn scalar_subquery_no_rows_is_null() {
    let c = setup();
    let r = c.query("SELECT (SELECT a FROM u WHERE a = 999)").unwrap();
    assert_eq!(r.rows[0][0], Value::Null);
}

// ---- correlated subqueries & EXISTS -----------------------------------------

fn ints(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            ref o => panic!("not int: {o:?}"),
        })
        .collect()
}

#[test]
fn correlated_exists() {
    let c = setup();
    // t rows that also appear in u.
    let got = ints(
        &c,
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.a = t.a) ORDER BY a",
    );
    assert_eq!(got, vec![20, 40]);
}

#[test]
fn correlated_not_exists() {
    let c = setup();
    let got = ints(
        &c,
        "SELECT a FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.a = t.a) ORDER BY a",
    );
    assert_eq!(got, vec![10, 30]);
}

#[test]
fn correlated_scalar_in_projection() {
    let c = setup();
    // Count of u rows less than each t.a.
    let r = c
        .query("SELECT t.a, (SELECT count(*) FROM u WHERE u.a < t.a) FROM t ORDER BY t.a")
        .unwrap();
    let got: Vec<(i64, i64)> = r
        .rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Integer(a), Value::Integer(b)) => (*a, *b),
            _ => panic!(),
        })
        .collect();
    // u = {20,40}: for 10->0, 20->0, 30->1, 40->1
    assert_eq!(got, vec![(10, 0), (20, 0), (30, 1), (40, 1)]);
}

#[test]
fn correlated_in_where_predicate() {
    let c = setup();
    // t.a greater than the matching-or-any u.a (correlated comparison).
    let got = ints(
        &c,
        "SELECT a FROM t WHERE a > (SELECT min(u.a) FROM u WHERE u.a >= t.a) - 1 ORDER BY a",
    );
    // For each t.a: min u.a >= t.a, minus 1. 10->min(20,40)=20-1=19, 10>19? no.
    // 20->20-1=19, 20>19 yes. 30->40-1=39,30>39 no. 40->40-1=39,40>39 yes.
    assert_eq!(got, vec![20, 40]);
}

#[test]
fn correlated_against_sqlite3() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Build identical data in sqlite and graphitesql, run a battery of correlated
    // queries, compare output.
    let setup_sql = "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, g INT);\
        CREATE TABLE u(id INTEGER PRIMARY KEY, t_id INT, w INT);";
    let mut inserts = String::new();
    for i in 1..=12i64 {
        inserts += &format!(
            "INSERT INTO t(id,a,g) VALUES ({i},{},{});",
            i * 3 % 7,
            i % 3
        );
    }
    for i in 1..=18i64 {
        inserts += &format!(
            "INSERT INTO u(id,t_id,w) VALUES ({i},{},{});",
            i % 12 + 1,
            i % 5
        );
    }

    let path = std::env::temp_dir().join(format!("gsql-corr-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(format!("{setup_sql}{inserts}"))
        .output()
        .unwrap();
    assert!(out.status.success());

    let mut g = Connection::open_memory().unwrap();
    for s in setup_sql.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }
    for s in inserts.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let queries = [
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id) ORDER BY id",
        "SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id) ORDER BY id",
        "SELECT id, (SELECT count(*) FROM u WHERE u.t_id = t.id) FROM t ORDER BY id",
        "SELECT id, (SELECT sum(w) FROM u WHERE u.t_id = t.id) FROM t ORDER BY id",
        "SELECT id FROM t WHERE (SELECT count(*) FROM u WHERE u.t_id = t.id) > 1 ORDER BY id",
        "SELECT t.id, t.a FROM t WHERE t.a > (SELECT avg(w) FROM u WHERE u.t_id = t.id) ORDER BY t.id",
        "SELECT count(*) FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id AND u.w = t.g)",
    ];
    for q in queries {
        let want = {
            let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let r = g.query(q).unwrap();
        let got = r
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| match v {
                        Value::Null => String::new(),
                        Value::Integer(i) => i.to_string(),
                        Value::Real(r) => format!("{r}"),
                        Value::Text(s) => s.clone(),
                        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(got, want, "query: {q}");
    }
    let _ = std::fs::remove_file(&path);
}
