//! Rowid-seek join optimization (roadmap B1a).
//!
//! When a `JOIN`'s `ON` is a lone equi-join `outer.col = u.id` whose right side
//! is the inner table `u`'s INTEGER PRIMARY KEY (rowid), the inner row is fetched
//! by rowid per outer row instead of being materialized and nested-looped.
//! `EXPLAIN QUERY PLAN` then reads `SEARCH u USING INTEGER PRIMARY KEY (rowid=?)`
//! (matching SQLite) and the results are identical to the materialize path.

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

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, n)")
        .unwrap();
    c.execute("CREATE TABLE t(a, uid)").unwrap();
    c.execute("INSERT INTO u VALUES(1,'x'),(2,'y')").unwrap();
    // 30 has no match (uid 9); 40 is a NULL key; 50 is a non-integer key (2.5).
    c.execute("INSERT INTO t VALUES(10,1),(20,2),(30,9),(40,NULL),(50,2.5)")
        .unwrap();
    c
}

#[test]
fn inner_join_eqp_searches_by_rowid() {
    let c = setup();
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT t.a,u.n FROM t JOIN u ON t.uid=u.id"
        ),
        ["SCAN t", "SEARCH u USING INTEGER PRIMARY KEY (rowid=?)"]
    );
    // The mirror form `u.id = t.uid` is the same plan.
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT t.a,u.n FROM t JOIN u ON u.id=t.uid"
        ),
        ["SCAN t", "SEARCH u USING INTEGER PRIMARY KEY (rowid=?)"]
    );
}

#[test]
fn left_join_eqp_searches_by_rowid() {
    let c = setup();
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT t.a,u.n FROM t LEFT JOIN u ON t.uid=u.id"
        ),
        // The inner side of a LEFT join carries sqlite's ` LEFT-JOIN` suffix.
        [
            "SCAN t",
            "SEARCH u USING INTEGER PRIMARY KEY (rowid=?) LEFT-JOIN"
        ]
    );
}

#[test]
fn inner_join_drops_unmatched() {
    let c = setup();
    // 30/40/50 have no integer-rowid match: INNER drops them.
    assert_eq!(
        rows(
            &c,
            "SELECT t.a,u.n FROM t JOIN u ON t.uid=u.id ORDER BY t.a"
        ),
        [alloc_row(10, "x"), alloc_row(20, "y"),]
    );
}

#[test]
fn left_join_null_extends_unmatched() {
    let c = setup();
    // The three unmatched outer rows are NULL-extended (not dropped).
    assert_eq!(
        rows(
            &c,
            "SELECT t.a,u.n FROM t LEFT JOIN u ON t.uid=u.id ORDER BY t.a"
        ),
        [
            alloc_row(10, "x"),
            alloc_row(20, "y"),
            alloc_null(30),
            alloc_null(40),
            alloc_null(50),
        ]
    );
}

#[test]
fn coerced_keys_match_like_sqlite() {
    // A real/text join key that is numerically an integer still seeks the rowid;
    // a non-integer real (2.5) and a NULL never match.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, n)")
        .unwrap();
    c.execute("CREATE TABLE t(a, uid)").unwrap();
    c.execute("INSERT INTO u VALUES(1,'x'),(2,'y')").unwrap();
    c.execute(
        "INSERT INTO t VALUES(10,1),(20,1.0),(30,'1'),(40,'1.0'),(50,2.5),(60,NULL),(70,x'01')",
    )
    .unwrap();
    assert_eq!(
        rows(
            &c,
            "SELECT t.a,u.n FROM t JOIN u ON t.uid=u.id ORDER BY t.a"
        ),
        [
            alloc_row(10, "x"),
            alloc_row(20, "x"),
            alloc_row(30, "x"),
            alloc_row(40, "x"),
        ]
    );
}

#[test]
fn non_ipk_join_column_uses_an_automatic_index() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE v(id INTEGER PRIMARY KEY, k)")
        .unwrap();
    c.execute("CREATE TABLE t(a, uid)").unwrap();
    c.execute("INSERT INTO v VALUES(1,100),(2,200)").unwrap();
    c.execute("INSERT INTO t VALUES(10,100),(20,200)").unwrap();
    // Joining on a non-IPK column (`v.k`) is not a rowid seek; the executor
    // hash-joins it, reported (as sqlite does) as an automatic covering index.
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT t.a FROM t JOIN v ON t.uid=v.k"
        ),
        [
            "SCAN t",
            "BLOOM FILTER ON v (k=?)",
            "SEARCH v USING AUTOMATIC COVERING INDEX (k=?)",
        ]
    );
}

#[test]
fn without_rowid_inner_seeks_its_primary_key() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE w(id INTEGER PRIMARY KEY, n) WITHOUT ROWID")
        .unwrap();
    c.execute("CREATE TABLE t(a, uid)").unwrap();
    c.execute("INSERT INTO w VALUES(1,'x'),(2,'y')").unwrap();
    c.execute("INSERT INTO t VALUES(10,1),(20,2)").unwrap();
    // Joining a WITHOUT ROWID table on its leading PRIMARY KEY seeks the
    // clustered b-tree per outer row, matching sqlite.
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT t.a,w.n FROM t JOIN w ON t.uid=w.id"
        ),
        ["SCAN t", "SEARCH w USING PRIMARY KEY (id=?)"]
    );
    assert_eq!(
        rows(
            &c,
            "SELECT t.a,w.n FROM t JOIN w ON t.uid=w.id ORDER BY t.a"
        ),
        [alloc_row(10, "x"), alloc_row(20, "y")]
    );
}

fn alloc_row(a: i64, n: &str) -> Vec<Value> {
    vec![Value::Integer(a), Value::Text(n.to_string().into())]
}

fn alloc_null(a: i64) -> Vec<Value> {
    vec![Value::Integer(a), Value::Null]
}
