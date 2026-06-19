//! EXPLAIN QUERY PLAN tests.
//!
//! graphitesql executes with an iterator engine (no VDBE bytecode), so plain
//! `EXPLAIN` is unsupported, but `EXPLAIN QUERY PLAN` describes the actual scan
//! plan. The detail-string format matches SQLite's for single-table access.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn detail(conn: &Connection, sql: &str) -> Vec<String> {
    let r = conn.query(sql).unwrap();
    // Columns: id, parent, notused, detail
    assert_eq!(r.columns.len(), 4);
    r.rows
        .iter()
        .map(|row| match &row[3] {
            Value::Text(s) => s.clone(),
            other => panic!("detail not text: {other:?}"),
        })
        .collect()
}

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT, s TEXT)")
        .unwrap();
    c.execute("CREATE INDEX it_a ON t(a)").unwrap();
    c.execute("CREATE INDEX it_ab ON t(a, b)").unwrap();
    for i in 1..=20 {
        c.execute(&format!(
            "INSERT INTO t(id,a,b,s) VALUES ({i},{},{},'r{}')",
            i % 5,
            i % 3,
            i % 4
        ))
        .unwrap();
    }
    c
}

#[test]
fn full_scan() {
    let c = setup();
    assert_eq!(detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t"), ["SCAN t"]);
}

#[test]
fn search_by_rowid() {
    let c = setup();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t WHERE id = 5"),
        ["SEARCH t USING INTEGER PRIMARY KEY (rowid=?)"]
    );
}

#[test]
fn search_by_index() {
    let c = setup();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 2"),
        ["SEARCH t USING INDEX it_a (a=?)"]
    );
}

#[test]
fn search_by_composite_index() {
    let c = setup();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 2 AND b = 1"),
        ["SEARCH t USING INDEX it_ab (a=? AND b=?)"]
    );
}

#[test]
fn order_by_adds_temp_btree() {
    let c = setup();
    let d = detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t ORDER BY s");
    assert_eq!(d, ["SCAN t", "USE TEMP B-TREE FOR ORDER BY"]);
}

#[test]
fn aliased_table() {
    let c = setup();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t AS x WHERE x.a = 2"),
        ["SEARCH t AS x USING INDEX it_a (a=?)"]
    );
}

#[test]
fn plain_explain_unsupported() {
    let c = setup();
    assert!(c.query("EXPLAIN SELECT * FROM t").is_err());
}

#[test]
fn explain_delete() {
    let c = setup();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN DELETE FROM t WHERE id = 3"),
        ["SEARCH t USING INTEGER PRIMARY KEY (rowid=?)"]
    );
}

#[test]
fn rowid_lookup_returns_correct_row() {
    // The new rowid fast-path must still return correct results.
    let c = setup();
    let r = c.query("SELECT a, b FROM t WHERE id = 7").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::Integer(7 % 5));
    // Non-integer literal: id = 7.5 matches no row.
    let r = c.query("SELECT * FROM t WHERE id = 7.5").unwrap();
    assert_eq!(r.rows.len(), 0);
}
