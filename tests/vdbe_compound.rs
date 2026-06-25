//! B5c-3: compound SELECTs (`UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT`) run
//! on the VDBE — each constituent SELECT is executed through the VDBE and the
//! row-sets are combined with SQLite's set semantics (dedup under the left
//! SELECT's collations, post-dedup sort, overall ORDER BY/LIMIT/OFFSET).
//! `query_vdbe` errors on any fallback to the tree-walker, so these passing
//! proves the VDBE compound path handled them. Results match sqlite 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x)").unwrap();
    c.execute("INSERT INTO a VALUES(1),(2),(3),(3)").unwrap();
    c.execute("CREATE TABLE b(x)").unwrap();
    c.execute("INSERT INTO b VALUES(2),(3),(4)").unwrap();
    c
}

fn ints(rows: &graphitesql::QueryResult) -> Vec<i64> {
    rows.rows
        .iter()
        .map(|r| match &r[0] {
            Value::Integer(n) => *n,
            other => panic!("expected integer, got {other:?}"),
        })
        .collect()
}

#[test]
fn union_dedups_and_sorts_on_vdbe() {
    let c = setup();
    // UNION removes duplicates and (absent ORDER BY) emits in sorted order.
    let r = c
        .query_vdbe("SELECT x FROM a UNION SELECT x FROM b")
        .unwrap();
    assert_eq!(ints(&r), vec![1, 2, 3, 4]);
}

#[test]
fn union_all_concatenates_on_vdbe() {
    let c = setup();
    // UNION ALL preserves every row and the per-arm order (a then b).
    let r = c
        .query_vdbe("SELECT x FROM a UNION ALL SELECT x FROM b")
        .unwrap();
    assert_eq!(ints(&r), vec![1, 2, 3, 3, 2, 3, 4]);
}

#[test]
fn intersect_on_vdbe() {
    let c = setup();
    let r = c
        .query_vdbe("SELECT x FROM a INTERSECT SELECT x FROM b")
        .unwrap();
    assert_eq!(ints(&r), vec![2, 3]);
}

#[test]
fn except_on_vdbe() {
    let c = setup();
    let r = c
        .query_vdbe("SELECT x FROM a EXCEPT SELECT x FROM b")
        .unwrap();
    assert_eq!(ints(&r), vec![1]);
}

#[test]
fn union_with_order_by_and_limit_on_vdbe() {
    let c = setup();
    // An explicit ORDER BY/LIMIT applies to the whole compound.
    let r = c
        .query_vdbe("SELECT x FROM a UNION SELECT x FROM b ORDER BY x DESC LIMIT 2")
        .unwrap();
    assert_eq!(ints(&r), vec![4, 3]);
}

#[test]
fn const_int_real_union_keeps_later_repr_on_vdbe() {
    let c = Connection::open_memory().unwrap();
    // FROM-less constant arms: 1 and 1.0 are duplicates; SQLite keeps the later
    // representation (1.0).
    let r = c.query_vdbe("SELECT 1 UNION SELECT 1.0").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Real(1.0)]]);
}

#[test]
fn three_arm_left_associative_on_vdbe() {
    let c = Connection::open_memory().unwrap();
    // (1 UNION ALL 2) EXCEPT 1 == {2}.
    let r = c
        .query_vdbe("SELECT 1 UNION ALL SELECT 2 EXCEPT SELECT 1")
        .unwrap();
    assert_eq!(ints(&r), vec![2]);
}

#[test]
fn union_over_filtered_arms_on_vdbe() {
    let c = setup();
    // Each arm carries its own WHERE; the VDBE runs both, then unions.
    let r = c
        .query_vdbe("SELECT x FROM a WHERE x > 1 UNION SELECT x FROM b WHERE x < 4")
        .unwrap();
    assert_eq!(ints(&r), vec![2, 3]);
}

#[test]
fn column_count_mismatch_errors_on_vdbe() {
    let c = setup();
    // SELECTs on either side of a compound operator must project equal column
    // counts; SQLite (and graphite) reject a mismatch.
    let err = c.query_vdbe("SELECT x FROM a UNION SELECT x, x FROM b");
    assert!(err.is_err());
}
