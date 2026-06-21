//! A single-table query on a WITHOUT ROWID table with a leading-PRIMARY-KEY
//! equality seeks the clustered b-tree instead of scanning — `SEARCH w USING
//! PRIMARY KEY (…)` in EXPLAIN QUERY PLAN, matching sqlite3 3.50.4 — and returns
//! the same rows. A non-leading-PK predicate stays a SCAN.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn detail(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| match row.last() {
            Some(Value::Text(s)) => s.clone(),
            other => panic!("detail not text: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn val(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}

#[test]
fn leading_pk_equality_seeks_the_primary_key() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE w(id PRIMARY KEY, n) WITHOUT ROWID")
        .unwrap();
    c.execute("INSERT INTO w VALUES (1,'a'),(2,'b'),(3,'c')")
        .unwrap();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM w WHERE id=2"),
        "SEARCH w USING PRIMARY KEY (id=?)"
    );
    assert_eq!(
        val(&c, "SELECT n FROM w WHERE id=2"),
        Value::Text("b".into())
    );
    // A missing key returns nothing; a present one, exactly its row.
    assert!(c
        .query("SELECT n FROM w WHERE id=9")
        .unwrap()
        .rows
        .is_empty());
}

#[test]
fn composite_pk_prefix_and_full() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE w(a, b, c, PRIMARY KEY(a, b)) WITHOUT ROWID")
        .unwrap();
    c.execute("INSERT INTO w VALUES (1,2,10),(1,3,20),(2,2,30)")
        .unwrap();
    // Full key.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT c FROM w WHERE a=1 AND b=3"),
        "SEARCH w USING PRIMARY KEY (a=? AND b=?)"
    );
    assert_eq!(
        val(&c, "SELECT c FROM w WHERE a=1 AND b=3"),
        Value::Integer(20)
    );
    // Leading-column prefix only.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT c FROM w WHERE a=1"),
        "SEARCH w USING PRIMARY KEY (a=?)"
    );
    assert_eq!(
        c.query("SELECT c FROM w WHERE a=1 ORDER BY b")
            .unwrap()
            .rows,
        [vec![Value::Integer(10)], vec![Value::Integer(20)]]
    );
}

#[test]
fn a_non_leading_predicate_still_scans() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE w(a, b, c, PRIMARY KEY(a, b)) WITHOUT ROWID")
        .unwrap();
    c.execute("INSERT INTO w VALUES (1,2,10),(2,2,30)").unwrap();
    // `b` is not the leading PK column: no seek.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM w WHERE b=2"),
        "SCAN w"
    );
    // ...but the result is still correct.
    assert_eq!(
        c.query("SELECT c FROM w WHERE b=2 ORDER BY a")
            .unwrap()
            .rows,
        [vec![Value::Integer(10)], vec![Value::Integer(30)]]
    );
}

#[test]
fn text_and_collated_primary_keys() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE w(k TEXT COLLATE NOCASE PRIMARY KEY, n) WITHOUT ROWID")
        .unwrap();
    c.execute("INSERT INTO w VALUES ('Abc', 1), ('xyz', 2)")
        .unwrap();
    // The PK's NOCASE collation drives the seek comparison.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT n FROM w WHERE k='abc'"),
        "SEARCH w USING PRIMARY KEY (k=?)"
    );
    assert_eq!(val(&c, "SELECT n FROM w WHERE k='ABC'"), Value::Integer(1));
}
