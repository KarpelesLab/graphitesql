//! Phase 9: non-recursive common table expressions (`WITH`).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT)")
        .unwrap();
    c.execute("INSERT INTO t(a) VALUES (5), (15), (25), (35)")
        .unwrap();
    c
}

#[test]
fn with_clause_as_source() {
    let c = setup();
    let r = c
        .query("WITH big AS (SELECT a FROM t WHERE a > 10) SELECT count(*), sum(a) FROM big")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(3));
    assert_eq!(r.rows[0][1], Value::Integer(75)); // 15+25+35

    let r = c
        .query(
            "WITH big AS (SELECT a FROM t WHERE a > 10) SELECT a FROM big WHERE a < 30 ORDER BY a",
        )
        .unwrap();
    let got: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![15, 25]);
}

#[test]
fn with_explicit_columns() {
    let c = setup();
    let r = c
        .query("WITH r(x) AS (SELECT a FROM t) SELECT x FROM r ORDER BY x LIMIT 1")
        .unwrap();
    assert_eq!(r.columns, vec!["x"]);
    assert_eq!(r.rows[0][0], Value::Integer(5));
}

// ---- recursive CTEs ---------------------------------------------------------

fn ints(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            ref other => panic!("not int: {other:?}"),
        })
        .collect()
}

#[test]
fn recursive_counter() {
    let c = Connection::open_memory().unwrap();
    let got = ints(
        &c,
        "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt WHERE x < 10) \
         SELECT x FROM cnt",
    );
    assert_eq!(got, (1..=10).collect::<Vec<_>>());
}

#[test]
fn recursive_sum_and_aggregate() {
    let c = Connection::open_memory().unwrap();
    let got = ints(
        &c,
        "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt WHERE x < 100) \
         SELECT sum(x) FROM cnt",
    );
    assert_eq!(got, vec![5050]);
}

#[test]
fn recursive_with_outer_filter_and_order() {
    let c = Connection::open_memory().unwrap();
    let got = ints(
        &c,
        "WITH RECURSIVE cnt(x) AS (SELECT 0 UNION ALL SELECT x+2 FROM cnt WHERE x < 10) \
         SELECT x FROM cnt WHERE x > 2 ORDER BY x DESC",
    );
    assert_eq!(got, vec![10, 8, 6, 4]);
}

#[test]
fn recursive_union_distinct_terminates() {
    // UNION (distinct) over a bounded space stops once no new rows appear.
    let c = Connection::open_memory().unwrap();
    let got = ints(
        &c,
        "WITH RECURSIVE m(x) AS (SELECT 0 UNION SELECT (x+1)%5 FROM m) \
         SELECT x FROM m ORDER BY x",
    );
    assert_eq!(got, vec![0, 1, 2, 3, 4]);
}

#[test]
fn recursive_transitive_closure() {
    // Classic graph reachability over an edges table.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE edge(a INT, b INT)").unwrap();
    for (a, b) in [(1, 2), (2, 3), (3, 4), (1, 5)] {
        c.execute(&format!("INSERT INTO edge VALUES ({a},{b})"))
            .unwrap();
    }
    let got = ints(
        &c,
        "WITH RECURSIVE reach(n) AS (\
            SELECT 1 \
            UNION \
            SELECT edge.b FROM edge JOIN reach ON edge.a = reach.n) \
         SELECT n FROM reach ORDER BY n",
    );
    assert_eq!(got, vec![1, 2, 3, 4, 5]);
}

#[test]
fn recursive_against_sqlite3() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let queries = [
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<20) SELECT group_concat(x) FROM c",
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<20) SELECT sum(x), count(*), max(x) FROM c",
        "WITH RECURSIVE fib(a,b) AS (SELECT 0,1 UNION ALL SELECT b,a+b FROM fib WHERE b<200) SELECT a FROM fib",
        "WITH RECURSIVE c(x) AS (SELECT 2 UNION ALL SELECT x*2 FROM c WHERE x<1000) SELECT x FROM c ORDER BY x DESC",
    ];
    let c = Connection::open_memory().unwrap();
    for q in queries {
        let want = {
            let out = Command::new("sqlite3")
                .arg(":memory:")
                .arg(q)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim_end().to_string()
        };
        let r = c.query(q).unwrap();
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
}

#[test]
fn recursive_cte_limit_terminates_and_bounds() {
    let c = Connection::open_memory().unwrap();
    // LIMIT on the CTE body bounds the rows it produces and terminates what
    // would otherwise be an infinite recursion.
    assert_eq!(
        ints(
            &c,
            "WITH RECURSIVE cc(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cc LIMIT 5) SELECT x FROM cc"
        ),
        vec![1, 2, 3, 4, 5]
    );
    // LIMIT with OFFSET.
    assert_eq!(
        ints(
            &c,
            "WITH RECURSIVE cc(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cc LIMIT 3 OFFSET 2) SELECT x FROM cc"
        ),
        vec![3, 4, 5]
    );
    // LIMIT 0 yields nothing.
    assert!(ints(
        &c,
        "WITH RECURSIVE cc(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cc LIMIT 0) SELECT x FROM cc"
    )
    .is_empty());
    // A normally-terminating recursion is unaffected.
    assert_eq!(
        ints(
            &c,
            "WITH RECURSIVE cc(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cc WHERE x<4) SELECT x FROM cc"
        ),
        vec![1, 2, 3, 4]
    );
}
