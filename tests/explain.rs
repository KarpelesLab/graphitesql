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
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 2 AND b = 1"
        ),
        ["SEARCH t USING INDEX it_ab (a=? AND b=?)"]
    );
}

#[test]
fn search_by_range_and_in() {
    // A dedicated table with a single index per column, so the chosen index is
    // unambiguous (and matches what the executor seeks).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE r(id INTEGER PRIMARY KEY, a INT, b TEXT)")
        .unwrap();
    c.execute("CREATE INDEX r_a ON r(a)").unwrap();
    for i in 1..=20 {
        c.execute(&format!("INSERT INTO r(a,b) VALUES ({}, 'x{i}')", i % 7))
            .unwrap();
    }
    // Range bounds render as `>`/`<` regardless of inclusivity, like SQLite.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM r WHERE a > 3"),
        ["SEARCH r USING INDEX r_a (a>?)"]
    );
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM r WHERE a <= 3"),
        ["SEARCH r USING INDEX r_a (a<?)"]
    );
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM r WHERE a >= 2 AND a < 5"
        ),
        ["SEARCH r USING INDEX r_a (a>? AND a<?)"]
    );
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM r WHERE a BETWEEN 2 AND 5"
        ),
        ["SEARCH r USING INDEX r_a (a>? AND a<?)"]
    );
    // IN-list renders like equality.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM r WHERE a IN (1,2,3)"),
        ["SEARCH r USING INDEX r_a (a=?)"]
    );
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM r WHERE id IN (1,2,3)"),
        ["SEARCH r USING INTEGER PRIMARY KEY (rowid=?)"]
    );
    // Rowid ranges walk the table b-tree between integer bounds.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM r WHERE id > 5"),
        ["SEARCH r USING INTEGER PRIMARY KEY (rowid>?)"]
    );
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM r WHERE id >= 5 AND id < 9"
        ),
        ["SEARCH r USING INTEGER PRIMARY KEY (rowid>? AND rowid<?)"]
    );
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM r WHERE id < 9"),
        ["SEARCH r USING INTEGER PRIMARY KEY (rowid<?)"]
    );
}

#[test]
fn multi_index_or() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT)")
        .unwrap();
    c.execute("CREATE INDEX ta ON t(a)").unwrap();
    c.execute("CREATE INDEX tb ON t(b)").unwrap();
    // Top-level OR of two seekable disjuncts on different indexes.
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 1 OR b = 2"
        ),
        [
            "MULTI-INDEX OR",
            "INDEX 1",
            "SEARCH t USING INDEX ta (a=?)",
            "INDEX 2",
            "SEARCH t USING INDEX tb (b=?)",
        ]
    );
    // A non-seekable disjunct (no index on the rowid range's... here b+1) makes
    // the whole thing a plain scan.
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a = 1 OR b + 1 = 2"
        ),
        ["SCAN t"]
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
