//! `GROUP BY <n>` groups by the n-th output column (not the constant n), and
//! `generate_series(..,0)` treats a zero step as 1 — both matched to the sqlite3
//! CLI. (Tests use ORDER BY so the row order is deterministic; SQLite's grouped
//! output order without ORDER BY is unspecified and graphite may differ.)

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

#[test]
fn group_by_position_resolves_to_column() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(1),(2),(3),(4)")
        .unwrap();
    // GROUP BY 1 groups by `a` (per-value counts), not by the constant 1 (which
    // would collapse to a single group of 5).
    assert_eq!(
        rows(&c, "SELECT a, count(*) FROM t GROUP BY 1 ORDER BY 1"),
        vec![
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(2), Value::Integer(1)],
            vec![Value::Integer(3), Value::Integer(1)],
            vec![Value::Integer(4), Value::Integer(1)],
        ]
    );
    // The position names the output *expression*, so an expression column works.
    assert_eq!(
        rows(&c, "SELECT a%2 p, count(*) FROM t GROUP BY 1 ORDER BY 1"),
        vec![
            vec![Value::Integer(0), Value::Integer(2)],
            vec![Value::Integer(1), Value::Integer(3)],
        ]
    );
    // GROUP BY 1 and GROUP BY a agree.
    assert_eq!(
        rows(&c, "SELECT a, count(*) FROM t GROUP BY 1 ORDER BY 1"),
        rows(&c, "SELECT a, count(*) FROM t GROUP BY a ORDER BY a")
    );
}

#[test]
fn generate_series_zero_step_is_one() {
    let c = Connection::open_memory().unwrap();
    // A zero step behaves like step 1.
    assert_eq!(
        rows(&c, "SELECT count(*) FROM generate_series(0,5,0)")[0][0],
        Value::Integer(6)
    );
    assert_eq!(
        rows(
            &c,
            "SELECT value FROM generate_series(0,3,0) ORDER BY value"
        ),
        vec![
            vec![Value::Integer(0)],
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)],
        ]
    );
    // Descending range with a zero (→ positive) step yields nothing.
    assert_eq!(
        rows(&c, "SELECT count(*) FROM generate_series(10,1,0)")[0][0],
        Value::Integer(0)
    );
}
