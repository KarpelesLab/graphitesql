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
fn order_by_rowid_skips_temp_btree() {
    // ORDER BY the rowid / INTEGER PRIMARY KEY is already satisfied by the table
    // scan, so no temp b-tree is used (matching sqlite's plain SCAN) — for both
    // ASC and DESC, and via the `rowid` alias.
    let c = setup();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t ORDER BY id"),
        ["SCAN t"]
    );
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t ORDER BY id DESC"),
        ["SCAN t"]
    );
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t ORDER BY rowid"),
        ["SCAN t"]
    );
    // The optimisation must not fire when anything reshapes the rows: a WHERE
    // could pick a different access path, and a non-rowid key still needs a sort.
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a=1 ORDER BY id"
        ),
        [
            "SEARCH t USING INDEX it_a (a=?)",
            "USE TEMP B-TREE FOR ORDER BY"
        ]
    );
    // A non-indexed column still needs a sort.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t ORDER BY s"),
        ["SCAN t", "USE TEMP B-TREE FOR ORDER BY"]
    );
}

#[test]
fn order_by_secondary_index_uses_index() {
    // ORDER BY a full-index column scans that index in key order instead of
    // sorting. `USING INDEX` when the table is fetched, `USING COVERING INDEX`
    // when the index holds every referenced column — matching sqlite.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, c INT, extra TEXT)")
        .unwrap();
    c.execute("CREATE INDEX ic ON t(c)").unwrap();
    for (id, cv) in [(1, 30), (2, 10), (3, 20), (4, 10)] {
        c.execute(&format!("INSERT INTO t VALUES({id},{cv},'x')"))
            .unwrap();
    }
    // `extra` is not in the index, so SELECT * fetches the table: USING INDEX.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t ORDER BY c"),
        ["SCAN t USING INDEX ic"]
    );
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t ORDER BY c DESC"),
        ["SCAN t USING INDEX ic"]
    );
    // Referencing only indexed/rowid columns is covered: USING COVERING INDEX.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT c FROM t ORDER BY c"),
        ["SCAN t USING COVERING INDEX ic"]
    );
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT id, c FROM t ORDER BY c"),
        ["SCAN t USING COVERING INDEX ic"]
    );
    // A partial index must NOT be used (it omits rows), so the sort stays.
    c.execute("CREATE TABLE p(id INTEGER PRIMARY KEY, c INT)")
        .unwrap();
    c.execute("CREATE INDEX ip ON p(c) WHERE c > 0").unwrap();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM p ORDER BY c"),
        ["SCAN p", "USE TEMP B-TREE FOR ORDER BY"]
    );
}

#[test]
fn covering_index_scan_returns_correct_values() {
    // The covering path builds rows from index records (indexed cols + rowid)
    // without touching the table b-tree; values must be exact, incl. SELECT *
    // when the table is only rowid + indexed columns.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(k INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    c.execute("CREATE INDEX iu ON u(name)").unwrap();
    c.execute("INSERT INTO u VALUES(1,'banana'),(2,'apple'),(3,'cherry')")
        .unwrap();
    // SELECT * is covered (k = rowid, name = indexed); rows built from the index.
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM u ORDER BY name"),
        ["SCAN u USING COVERING INDEX iu"]
    );
    let rows = c.query("SELECT k, name FROM u ORDER BY name").unwrap().rows;
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(2), Value::Text("apple".into())],
            vec![Value::Integer(1), Value::Text("banana".into())],
            vec![Value::Integer(3), Value::Text("cherry".into())],
        ]
    );
}

#[test]
fn order_by_secondary_index_returns_correct_order() {
    // Result order must exactly match a sort: NULLs first (ASC) / last (DESC),
    // ties in rowid order, and NOCASE honored when the index collation matches.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, c INT)")
        .unwrap();
    c.execute("CREATE INDEX ic ON t(c)").unwrap();
    c.execute("INSERT INTO t VALUES(1,30),(2,10),(3,NULL),(4,10),(5,20),(6,NULL)")
        .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        c.query(sql)
            .unwrap()
            .rows
            .iter()
            .map(|r| match r[0] {
                Value::Integer(n) => n,
                _ => panic!(),
            })
            .collect()
    };
    assert_eq!(ids("SELECT a FROM t ORDER BY c"), [3, 6, 2, 4, 5, 1]);
    assert_eq!(ids("SELECT a FROM t ORDER BY c DESC"), [1, 5, 4, 2, 6, 3]);
    assert_eq!(ids("SELECT a FROM t ORDER BY c LIMIT 3"), [3, 6, 2]);

    let mut u = Connection::open_memory().unwrap();
    u.execute("CREATE TABLE u(k INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE)")
        .unwrap();
    u.execute("CREATE INDEX iu ON u(name)").unwrap();
    u.execute("INSERT INTO u VALUES(1,'banana'),(2,'Apple'),(3,'cherry'),(4,'apple')")
        .unwrap();
    let names: Vec<String> = u
        .query("SELECT name FROM u ORDER BY name")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            _ => panic!(),
        })
        .collect();
    assert_eq!(names, ["Apple", "apple", "banana", "cherry"]);
}

#[test]
fn order_by_rowid_returns_correct_order() {
    // The skipped sort must still yield rowid order (incl. negative rowids and
    // DESC via reverse).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES(3,'c'),(1,'a'),(2,'b'),(-5,'neg')")
        .unwrap();
    let col = |sql: &str| -> Vec<i64> {
        c.query(sql)
            .unwrap()
            .rows
            .iter()
            .map(|r| match r[0] {
                graphitesql::Value::Integer(n) => n,
                _ => panic!("not int"),
            })
            .collect()
    };
    assert_eq!(col("SELECT a FROM t ORDER BY a"), [-5, 1, 2, 3]);
    assert_eq!(col("SELECT a FROM t ORDER BY a DESC"), [3, 2, 1, -5]);
    assert_eq!(col("SELECT a FROM t ORDER BY rowid"), [-5, 1, 2, 3]);
    assert_eq!(col("SELECT a FROM t ORDER BY a LIMIT 2"), [-5, 1]);
    assert_eq!(col("SELECT a FROM t ORDER BY a DESC LIMIT 2"), [3, 2]);
}

#[test]
fn aliased_table() {
    let c = setup();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t AS x WHERE x.a = 2"),
        ["SEARCH x USING INDEX it_a (a=?)"]
    );
}

#[test]
fn plain_explain_lists_bytecode() {
    // Plain `EXPLAIN <select>` (B8) now compiles the query to graphite's VDBE
    // program and lists it as (addr, opcode, detail) rows.
    let c = setup();
    let r = c.query("EXPLAIN SELECT * FROM t").unwrap();
    assert_eq!(r.columns, ["addr", "opcode", "detail"]);
    assert!(!r.rows.is_empty());
    // A shape the VDBE cannot compile to a program still reports an error.
    assert!(c.query("EXPLAIN SELECT * FROM t, t AS t2").is_err());
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

#[test]
fn rowid_keyword_alias_seeks_like_sqlite() {
    // `rowid` / `_rowid_` / `oid` (the keyword aliases, not the IPK column name)
    // now seek the table b-tree directly — for an INTEGER PRIMARY KEY table AND a
    // plain rowid table — matching SQLite's EQP, instead of a full scan.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE ipk(id INTEGER PRIMARY KEY, a)")
        .unwrap();
    c.execute("INSERT INTO ipk VALUES(1,'x'),(2,'y'),(3,'z')")
        .unwrap();
    c.execute("CREATE TABLE imp(a, b)").unwrap();
    c.execute("INSERT INTO imp VALUES('p','q'),('r','s'),('t','u')")
        .unwrap();

    let seek = ["SEARCH ipk USING INTEGER PRIMARY KEY (rowid=?)"];
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM ipk WHERE rowid=1"),
        seek
    );
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM ipk WHERE rowid IN (1,2)"
        ),
        seek
    );
    let seek_imp = ["SEARCH imp USING INTEGER PRIMARY KEY (rowid=?)"];
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM imp WHERE rowid=2"),
        seek_imp
    );
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM imp WHERE _rowid_ IN (1,3)"
        ),
        seek_imp
    );

    // Results are correct (the rowid seek returns the right rows).
    let mut got: Vec<i64> = c
        .query("SELECT rowid FROM imp WHERE rowid IN (1,3)")
        .unwrap()
        .rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            _ => panic!(),
        })
        .collect();
    got.sort();
    assert_eq!(got, [1, 3]);

    // A real column literally named `rowid` shadows the keyword: no rowid seek.
    c.execute("CREATE TABLE shadow(rowid TEXT, b)").unwrap();
    c.execute("INSERT INTO shadow VALUES('k', 9)").unwrap();
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT * FROM shadow WHERE rowid='k'"
        ),
        ["SCAN shadow"]
    );
    assert_eq!(
        c.query("SELECT b FROM shadow WHERE rowid='k'")
            .unwrap()
            .rows[0][0],
        Value::Integer(9)
    );
}
