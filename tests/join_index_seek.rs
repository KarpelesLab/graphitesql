//! Index-seek join optimization (roadmap B1a², index case).
//!
//! When a `JOIN`'s `ON` is a lone equi-join `outer.col = u.k` whose right side
//! `u.k` is the leading column of a usable secondary index on the inner plain
//! base table, the matching inner rows are found by seeking that index per outer
//! row instead of materializing and nested-looping `u`. `EXPLAIN QUERY PLAN`
//! then reads `SEARCH u USING INDEX <name> (<col>=?)` (matching SQLite) and the
//! results are identical to the materialize path, including the duplicate-key
//! fan-out where one non-unique key matches several inner rows.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn detail(conn: &Connection, sql: &str) -> Vec<String> {
    let r = conn.query(sql).unwrap();
    assert_eq!(r.columns.len(), 4); // id, parent, notused, detail
    r.rows
        .iter()
        .map(|row| match &row[3] {
            Value::Text(s) => String::from(s.as_str()),
            other => panic!("detail not text: {other:?}"),
        })
        .collect()
}

fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    conn.query(sql).unwrap().rows
}

/// `u(id PK, k, n)` with a secondary index `iuk` on the non-PK column `k`. Two
/// rows share `k = 200`, so a join key of 200 fans out to both.
fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, k, n)")
        .unwrap();
    c.execute("CREATE INDEX iuk ON u(k)").unwrap();
    c.execute("CREATE TABLE t(a, uk)").unwrap();
    c.execute("INSERT INTO u VALUES(1,100,'x'),(2,200,'y'),(3,200,'z')")
        .unwrap();
    // 30 has no matching k; the join key 200 (row 20) matches BOTH 200 rows.
    c.execute("INSERT INTO t VALUES(10,100),(20,200),(30,999)")
        .unwrap();
    c
}

fn row(a: i64, n: &str) -> Vec<Value> {
    vec![Value::Integer(a), Value::Text(n.to_string().into())]
}

fn null_row(a: i64) -> Vec<Value> {
    vec![Value::Integer(a), Value::Null]
}

#[test]
fn inner_join_eqp_searches_by_index() {
    let c = setup();
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT t.a,u.n FROM t JOIN u ON t.uk=u.k"
        ),
        ["SCAN t", "SEARCH u USING INDEX iuk (k=?)"]
    );
    // The mirror form `u.k = t.uk` is the same plan.
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT t.a,u.n FROM t JOIN u ON u.k=t.uk"
        ),
        ["SCAN t", "SEARCH u USING INDEX iuk (k=?)"]
    );
}

#[test]
fn left_join_eqp_searches_by_index() {
    let c = setup();
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT t.a,u.n FROM t LEFT JOIN u ON t.uk=u.k"
        ),
        // The inner side of a LEFT join carries sqlite's ` LEFT-JOIN` suffix.
        ["SCAN t", "SEARCH u USING INDEX iuk (k=?) LEFT-JOIN"]
    );
}

#[test]
fn inner_join_fans_out_on_duplicate_key_and_drops_unmatched() {
    let c = setup();
    // 30 (uk=999) has no match: INNER drops it. 20 (uk=200) matches BOTH 200
    // rows, so it appears twice.
    assert_eq!(
        rows(
            &c,
            "SELECT t.a,u.n FROM t JOIN u ON t.uk=u.k ORDER BY t.a,u.n"
        ),
        [row(10, "x"), row(20, "y"), row(20, "z")]
    );
}

#[test]
fn left_join_null_extends_unmatched_keeps_fan_out() {
    let c = setup();
    // 30 -> NULL-extended; 20 still fans out to both 200 rows.
    assert_eq!(
        rows(
            &c,
            "SELECT t.a,u.n FROM t LEFT JOIN u ON t.uk=u.k ORDER BY t.a,u.n"
        ),
        [row(10, "x"), row(20, "y"), row(20, "z"), null_row(30)]
    );
}

#[test]
fn null_key_never_matches() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, k, n)")
        .unwrap();
    c.execute("CREATE INDEX iuk ON u(k)").unwrap();
    c.execute("CREATE TABLE t(a, uk)").unwrap();
    c.execute("INSERT INTO u VALUES(1,100,'x')").unwrap();
    // A NULL join key never equi-joins (even to an indexed NULL key row).
    c.execute("INSERT INTO u VALUES(2,NULL,'q')").unwrap();
    c.execute("INSERT INTO t VALUES(10,100),(20,NULL)").unwrap();
    // INNER: the NULL-key outer row (20) is dropped.
    assert_eq!(
        rows(&c, "SELECT t.a,u.n FROM t JOIN u ON t.uk=u.k ORDER BY t.a"),
        [row(10, "x")]
    );
    // LEFT: the NULL-key outer row (20) is NULL-extended, not matched to row 2.
    assert_eq!(
        rows(
            &c,
            "SELECT t.a,u.n FROM t LEFT JOIN u ON t.uk=u.k ORDER BY t.a"
        ),
        [row(10, "x"), null_row(20)]
    );
}

#[test]
fn non_indexed_join_column_uses_an_automatic_index() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, k, n)")
        .unwrap();
    // No index on `k`: the join column is neither rowid nor indexed, so the
    // executor builds a transient hash index — reported (as sqlite does) as a
    // BLOOM FILTER plus an AUTOMATIC COVERING INDEX seek.
    c.execute("CREATE TABLE t(a, uk)").unwrap();
    c.execute("INSERT INTO u VALUES(1,100,'x'),(2,200,'y')")
        .unwrap();
    c.execute("INSERT INTO t VALUES(10,100),(20,200)").unwrap();
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT t.a,u.n FROM t JOIN u ON t.uk=u.k"
        ),
        [
            "SCAN t",
            "BLOOM FILTER ON u (k=?)",
            "SEARCH u USING AUTOMATIC COVERING INDEX (k=?)",
        ]
    );
    // Results are still correct on the scan path.
    assert_eq!(
        rows(&c, "SELECT t.a,u.n FROM t JOIN u ON t.uk=u.k ORDER BY t.a"),
        [row(10, "x"), row(20, "y")]
    );
}

#[test]
fn nocase_index_seek_reevaluates_on_with_binary() {
    // The index on `u.k` is `COLLATE NOCASE`, so the seek finds a NOCASE superset
    // ('foo' -> 'Foo'). The full `ON` (`t.uk = u.k`) is then re-evaluated with the
    // left operand `t.uk`'s implicit BINARY collation, dropping the case-mismatch
    // candidates — byte-identical to sqlite, which returns the empty set here.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, k TEXT COLLATE NOCASE, n)")
        .unwrap();
    c.execute("CREATE INDEX iuk ON u(k)").unwrap();
    c.execute("CREATE TABLE t(a, uk TEXT)").unwrap();
    c.execute("INSERT INTO u VALUES(1,'Foo','x'),(2,'BAR','y')")
        .unwrap();
    c.execute("INSERT INTO t VALUES(10,'foo'),(20,'bar'),(30,'FOO')")
        .unwrap();
    // The seek path is chosen (EQP shows the index).
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT t.a,u.n FROM t JOIN u ON t.uk=u.k"
        ),
        ["SCAN t", "SEARCH u USING INDEX iuk (k=?)"]
    );
    let r = rows(
        &c,
        "SELECT t.a,u.n FROM t JOIN u ON t.uk=u.k ORDER BY t.a,u.n",
    );
    assert_eq!(r, Vec::<Vec<Value>>::new(), "got {r:?}");
}
