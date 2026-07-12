//! `SELECT count(*)` answered via a covering secondary index (roadmap B2b).
//!
//! `count(*)` is answered from a covering secondary index when that index's
//! estimated row is *strictly narrower* than the table's (SQLite's
//! `estimateTableWidth`/`estimateIndexWidth` cost model). `EXPLAIN QUERY PLAN`
//! then reports `SCAN t USING COVERING INDEX <name>`, choosing the narrowest
//! qualifying index (ties → the most-recently-created). An index no narrower than
//! the table — e.g. the sole non-key column indexed on a two-column table — is
//! not used, and the plan stays `SCAN t`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn detail(conn: &Connection, sql: &str) -> Vec<String> {
    let r = conn.query(sql).unwrap();
    assert_eq!(r.columns.len(), 4);
    r.rows
        .iter()
        .map(|row| match &row[3] {
            Value::Text(s) => String::from(s.as_str()),
            other => panic!("detail not text: {other:?}"),
        })
        .collect()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    let r = conn.query(sql).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].len(), 1);
    match &r.rows[0][0] {
        Value::Integer(n) => *n,
        other => panic!("count not integer: {other:?}"),
    }
}

#[test]
fn one_index_uses_covering_index_in_eqp() {
    // The index (b + rowid = 2 units) is narrower than the 3-column table, so it
    // is used to count. (On a two-column table it would tie the table and SCAN.)
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b, c)")
        .unwrap();
    c.execute("CREATE INDEX ib ON t(b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,10,100),(2,20,200)")
        .unwrap();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT count(*) FROM t"),
        ["SCAN t USING COVERING INDEX ib"]
    );
}

#[test]
fn index_no_narrower_than_table_keeps_plain_scan() {
    // On a two-column table, indexing the sole non-key column gives an index
    // (b + rowid = 2) exactly as wide as the table (a + b = 2), so SQLite scans
    // the table rather than the index.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    c.execute("CREATE INDEX ib ON t(b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,10),(2,20)").unwrap();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT count(*) FROM t"),
        ["SCAN t"]
    );
    assert_eq!(count(&c, "SELECT count(*) FROM t"), 2);
}

#[test]
fn one_index_count_is_correct() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    c.execute("CREATE INDEX ib ON t(b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,10),(2,20),(3,30)")
        .unwrap();
    assert_eq!(count(&c, "SELECT count(*) FROM t"), 3);
}

#[test]
fn no_index_keeps_plain_scan() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(1,10),(2,20)").unwrap();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT count(*) FROM t"),
        ["SCAN t"]
    );
    assert_eq!(count(&c, "SELECT count(*) FROM t"), 2);
}

#[test]
fn multiple_indexes_pick_cheapest_covering_index() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b, c)")
        .unwrap();
    c.execute("CREATE INDEX ib ON t(b)").unwrap();
    c.execute("CREATE INDEX ic ON t(c)").unwrap();
    c.execute("INSERT INTO t VALUES(1,10,100),(2,20,200)")
        .unwrap();
    // Both indexes are narrower than the table and equally wide, so SQLite counts
    // the most-recently-created one (ic), matching its cost-model tie-break.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT count(*) FROM t"),
        ["SCAN t USING COVERING INDEX ic"]
    );
    assert_eq!(count(&c, "SELECT count(*) FROM t"), 2);
}

#[test]
fn count_correct_after_delete() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b, c)")
        .unwrap();
    c.execute("CREATE INDEX ib ON t(b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,10,1),(2,20,2),(3,30,3),(4,40,4)")
        .unwrap();
    assert_eq!(count(&c, "SELECT count(*) FROM t"), 4);
    c.execute("DELETE FROM t WHERE a IN (2,3)").unwrap();
    // Still uses the covering index, and the count reflects the deletes.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT count(*) FROM t"),
        ["SCAN t USING COVERING INDEX ib"]
    );
    assert_eq!(count(&c, "SELECT count(*) FROM t"), 2);
}

#[test]
fn empty_table_counts_zero() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    c.execute("CREATE INDEX ib ON t(b)").unwrap();
    assert_eq!(count(&c, "SELECT count(*) FROM t"), 0);
}

#[test]
fn partial_index_does_not_cover_count() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    c.execute("CREATE INDEX ib ON t(b) WHERE b > 5").unwrap();
    c.execute("INSERT INTO t VALUES(1,1),(2,10)").unwrap();
    // A partial index does not have one entry per row => plain SCAN, full count.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT count(*) FROM t"),
        ["SCAN t"]
    );
    assert_eq!(count(&c, "SELECT count(*) FROM t"), 2);
}

#[test]
fn without_rowid_table_uses_plain_scan() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT PRIMARY KEY, b) WITHOUT ROWID")
        .unwrap();
    c.execute("CREATE INDEX ib ON t(b)").unwrap();
    c.execute("INSERT INTO t VALUES('x',1),('y',2)").unwrap();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT count(*) FROM t"),
        ["SCAN t"]
    );
    assert_eq!(count(&c, "SELECT count(*) FROM t"), 2);
}

#[test]
fn count_with_where_is_not_index_covered() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b)")
        .unwrap();
    c.execute("CREATE INDEX ib ON t(b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,10),(2,20),(3,10)")
        .unwrap();
    // A WHERE clause excludes the count-covering fast path.
    let plan = detail(&c, "EXPLAIN QUERY PLAN SELECT count(*) FROM t WHERE b = 10");
    assert_ne!(plan, ["SCAN t USING COVERING INDEX ib"]);
    assert_eq!(count(&c, "SELECT count(*) FROM t WHERE b = 10"), 2);
}
