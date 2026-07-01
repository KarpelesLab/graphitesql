//! Covering-index reads on WHERE-driven index seeks (roadmap B2b, seek case).
//!
//! When the index chosen for an equality/range/IN seek already holds every
//! column the query references (indexed columns + rowid), graphitesql reads
//! straight from the index and `EXPLAIN QUERY PLAN` reports `USING COVERING
//! INDEX`. These tests pin both the plan string and that the covering rows equal
//! the table-fetch path (including NULLs), mirroring sqlite3 3.50.4.
#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

/// The single `EXPLAIN QUERY PLAN` detail string for `sql`.
fn plan(conn: &Connection, sql: &str) -> String {
    let r = conn
        .query(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("explain query plan");
    assert_eq!(r.rows.len(), 1, "expected one plan node for: {sql}");
    match &r.rows[0][3] {
        Value::Text(s) => s.clone(),
        other => panic!("plan detail not text: {other:?}"),
    }
}

/// All rows of `sql`.
fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    conn.query(sql).expect("query").rows
}

fn setup() -> Connection {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b, c)")
        .unwrap();
    conn.execute("CREATE INDEX ic ON t(c)").unwrap();
    conn.execute("INSERT INTO t VALUES(1,10,5),(2,20,5),(3,30,9),(4,NULL,5),(5,50,NULL)")
        .unwrap();
    conn
}

fn ints(mut got: Vec<Vec<Value>>) -> Vec<i64> {
    got.sort_by_key(|r| match r[0] {
        Value::Integer(i) => i,
        _ => i64::MAX,
    });
    got.into_iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            _ => panic!("not integer"),
        })
        .collect()
}

#[test]
fn equality_seek_covered_reports_covering_and_correct_rows() {
    let conn = setup();
    // c is the seek column and the only result column -> covered.
    assert_eq!(
        plan(&conn, "SELECT c FROM t WHERE c=5"),
        "SEARCH t USING COVERING INDEX ic (c=?)"
    );
    let got = rows(&conn, "SELECT c FROM t WHERE c=5");
    assert_eq!(ints(got), vec![5, 5, 5]);
}

#[test]
fn rowid_projection_is_covered() {
    let conn = setup();
    // a is the INTEGER PRIMARY KEY (rowid), present in every index record.
    assert_eq!(
        plan(&conn, "SELECT a, c FROM t WHERE c=5"),
        "SEARCH t USING COVERING INDEX ic (c=?)"
    );
    assert_eq!(
        plan(&conn, "SELECT a FROM t WHERE c=5"),
        "SEARCH t USING COVERING INDEX ic (c=?)"
    );
    assert_eq!(
        ints(rows(&conn, "SELECT a FROM t WHERE c=5")),
        vec![1, 2, 4]
    );
}

#[test]
fn non_covered_projection_uses_plain_index() {
    let conn = setup();
    // b is not in the index -> must fetch from the table.
    assert_eq!(
        plan(&conn, "SELECT b FROM t WHERE c=5"),
        "SEARCH t USING INDEX ic (c=?)"
    );
    // Result still correct (one matching row has b NULL).
    let got = rows(&conn, "SELECT b FROM t WHERE c=5 AND a=4");
    assert_eq!(got, vec![vec![Value::Null]]);
}

#[test]
fn residual_where_column_not_in_index_is_not_covered() {
    let conn = setup();
    // c is the seek column but b (also in WHERE) is not in the index.
    assert_eq!(
        plan(&conn, "SELECT c FROM t WHERE c=5 AND b>0"),
        "SEARCH t USING INDEX ic (c=?)"
    );
    // ...whereas a residual WHERE on a (rowid) stays covered, and the rowid range
    // bounds the index's implicit trailing key (matching SQLite — see B9g).
    assert_eq!(
        plan(&conn, "SELECT c FROM t WHERE c=5 AND a>1"),
        "SEARCH t USING COVERING INDEX ic (c=? AND rowid>?)"
    );
}

#[test]
fn covering_rows_equal_table_fetch_with_nulls() {
    let conn = setup();
    // Covered path (SELECT a,c) vs the table-fetch path (SELECT a,c,b dropping b)
    // must agree row-for-row, NULLs included.
    let covered = rows(&conn, "SELECT a, c FROM t WHERE c=5 ORDER BY a");
    let table_path: Vec<Vec<Value>> = rows(&conn, "SELECT a, c, b FROM t WHERE c=5 ORDER BY a")
        .into_iter()
        .map(|mut r| {
            r.truncate(2);
            r
        })
        .collect();
    assert_eq!(covered, table_path);
}

#[test]
fn range_seek_covered() {
    let conn = setup();
    // Range on the indexed column c, projecting only c (+rowid) -> covered.
    assert_eq!(
        plan(&conn, "SELECT c FROM t WHERE c>4 AND c<9"),
        "SEARCH t USING COVERING INDEX ic (c>? AND c<?)"
    );
    assert_eq!(
        ints(rows(&conn, "SELECT c FROM t WHERE c>4 AND c<9")),
        vec![5, 5, 5]
    );
    // Non-covered range still reports plain INDEX.
    assert_eq!(
        plan(&conn, "SELECT b FROM t WHERE c>4 AND c<9"),
        "SEARCH t USING INDEX ic (c>? AND c<?)"
    );
}

#[test]
fn in_seek_covered() {
    let conn = setup();
    assert_eq!(
        plan(&conn, "SELECT a, c FROM t WHERE c IN (5, 9)"),
        "SEARCH t USING COVERING INDEX ic (c=?)"
    );
    assert_eq!(
        ints(rows(&conn, "SELECT a FROM t WHERE c IN (5,9)")),
        vec![1, 2, 3, 4]
    );
    // Non-covered IN keeps plain INDEX.
    assert_eq!(
        plan(&conn, "SELECT b FROM t WHERE c IN (5,9)"),
        "SEARCH t USING INDEX ic (c=?)"
    );
}

#[test]
fn generated_column_table_not_covered() {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE TABLE g(a INTEGER PRIMARY KEY, c, d AS (c*2))")
        .unwrap();
    conn.execute("CREATE INDEX gic ON g(c)").unwrap();
    conn.execute("INSERT INTO g(a,c) VALUES(1,5),(2,5),(3,9)")
        .unwrap();
    // A generated column on the table disables covering (conservative guard).
    assert_eq!(
        plan(&conn, "SELECT c FROM g WHERE c=5"),
        "SEARCH g USING INDEX gic (c=?)"
    );
    assert_eq!(ints(rows(&conn, "SELECT c FROM g WHERE c=5")), vec![5, 5]);
}
