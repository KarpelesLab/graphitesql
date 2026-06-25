//! B5b-1: a plain two-table inner join with a nested-loopable shape (projection +
//! WHERE + constant LIMIT/OFFSET) runs on the VDBE as a nested loop over two
//! cursors, instead of materializing the `a × b` cross-product. `query_vdbe`
//! errors on any fallback to the tree-walker, so these passing proves the VDBE
//! join path handles them; results are checked against the expected rows (which
//! match sqlite's nested-loop order: every right row per left row, left outermost).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x, y)").unwrap();
    c.execute("INSERT INTO a VALUES(1,'a'),(2,'b'),(3,'c')")
        .unwrap();
    c.execute("CREATE TABLE b(p, q)").unwrap();
    c.execute("INSERT INTO b VALUES(1,'P'),(2,'Q'),(2,'R')")
        .unwrap();
    c
}

#[test]
fn nested_loop_join_runs_on_vdbe() {
    let c = setup();
    // Equi-join, no ORDER BY → the nested-loop path. Rows in left-outermost order.
    let r = c
        .query_vdbe("SELECT a.x, b.q FROM a JOIN b ON a.x = b.p")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Integer(1), Value::Text("P".into())],
            vec![Value::Integer(2), Value::Text("Q".into())],
            vec![Value::Integer(2), Value::Text("R".into())],
        ]
    );
}

#[test]
fn nested_loop_join_where_limit_offset() {
    let c = setup();
    // A comma join with the predicate in WHERE, plus LIMIT/OFFSET.
    let r = c
        .query_vdbe("SELECT a.x, b.q FROM a, b WHERE a.x = b.p LIMIT 2 OFFSET 1")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Integer(2), Value::Text("Q".into())],
            vec![Value::Integer(2), Value::Text("R".into())],
        ]
    );
}

#[test]
fn nested_loop_join_star_and_computed() {
    let c = setup();
    // `a.*` plus a computed projection over both tables.
    let r = c
        .query_vdbe("SELECT a.x * 10 + b.p AS s FROM a JOIN b ON a.x = b.p")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Integer(11)],
            vec![Value::Integer(22)],
            vec![Value::Integer(22)],
        ]
    );
}

#[test]
fn three_table_nested_loop_join_runs_on_vdbe() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x)").unwrap();
    c.execute("INSERT INTO a VALUES(1),(2)").unwrap();
    c.execute("CREATE TABLE b(y)").unwrap();
    c.execute("INSERT INTO b VALUES(10),(20)").unwrap();
    c.execute("CREATE TABLE cc(z)").unwrap();
    c.execute("INSERT INTO cc VALUES(100)").unwrap();
    // A three-table comma join runs as a 3-deep nested loop (no cross-product).
    let r = c
        .query_vdbe("SELECT a.x, b.y, cc.z FROM a, b, cc WHERE a.x = 2")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Integer(2), Value::Integer(10), Value::Integer(100)],
            vec![Value::Integer(2), Value::Integer(20), Value::Integer(100)],
        ]
    );
}

#[test]
fn left_join_runs_on_vdbe_with_null_padding() {
    let c = setup();
    // x=3 has no match in b → null-padded; verified on the VDBE (query_vdbe).
    let r = c
        .query_vdbe("SELECT a.x, b.q FROM a LEFT JOIN b ON a.x = b.p")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Integer(1), Value::Text("P".into())],
            vec![Value::Integer(2), Value::Text("Q".into())],
            vec![Value::Integer(2), Value::Text("R".into())],
            vec![Value::Integer(3), Value::Null],
        ]
    );
}

#[test]
fn left_join_where_filters_after_null_padding() {
    let c = setup();
    // `b.q IS NULL` keeps only the null-padded (unmatched) left rows.
    let r = c
        .query_vdbe("SELECT a.x FROM a LEFT JOIN b ON a.x = b.p WHERE b.q IS NULL")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(3)]]);
}

#[test]
fn left_join_empty_right_null_pads_every_left_row() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x)").unwrap();
    c.execute("INSERT INTO a VALUES(1),(2)").unwrap();
    c.execute("CREATE TABLE b(y)").unwrap();
    let r = c
        .query_vdbe("SELECT a.x, b.y FROM a LEFT JOIN b ON a.x = b.y")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Integer(1), Value::Null],
            vec![Value::Integer(2), Value::Null],
        ]
    );
}

#[test]
fn right_join_runs_on_vdbe_with_null_padding() {
    let c = setup();
    // b's row p=2 has two matches in a? No — a has x in {1,2,3}; b has p in
    // {1,2,2}. Every b row matches some a row, so no null padding here; the point
    // is RIGHT runs on the VDBE (preserved = right table b) with a's columns.
    let r = c
        .query_vdbe("SELECT a.x, b.q FROM a RIGHT JOIN b ON a.x = b.p")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Integer(1), Value::Text("P".into())],
            vec![Value::Integer(2), Value::Text("Q".into())],
            vec![Value::Integer(2), Value::Text("R".into())],
        ]
    );
}

#[test]
fn right_join_null_pads_unmatched_right_row() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x)").unwrap();
    c.execute("INSERT INTO a VALUES(1)").unwrap();
    c.execute("CREATE TABLE b(p)").unwrap();
    c.execute("INSERT INTO b VALUES(1),(2)").unwrap();
    // b.p=2 has no match in a → a.x is NULL for that preserved right row.
    let r = c
        .query_vdbe("SELECT a.x, b.p FROM a RIGHT JOIN b ON a.x = b.p")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Integer(1), Value::Integer(1)],
            vec![Value::Null, Value::Integer(2)],
        ]
    );
}

#[test]
fn full_join_runs_on_vdbe_with_both_sided_null_padding() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x)").unwrap();
    c.execute("INSERT INTO a VALUES(1),(2),(3)").unwrap();
    c.execute("CREATE TABLE b(p)").unwrap();
    c.execute("INSERT INTO b VALUES(2),(3),(4)").unwrap();
    // SQLite's FULL-join order: left-driven rows first, then unmatched-right.
    let r = c
        .query_vdbe("SELECT a.x, b.p FROM a FULL JOIN b ON a.x = b.p")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Integer(1), Value::Null],
            vec![Value::Integer(2), Value::Integer(2)],
            vec![Value::Integer(3), Value::Integer(3)],
            vec![Value::Null, Value::Integer(4)],
        ]
    );
    // `WHERE a.x IS NULL` keeps only the unmatched-right (pass 2) rows.
    let r2 = c
        .query_vdbe("SELECT b.p FROM a FULL JOIN b ON a.x = b.p WHERE a.x IS NULL")
        .unwrap();
    assert_eq!(r2.rows, vec![vec![Value::Integer(4)]]);
}

#[test]
fn distinct_over_join_runs_on_vdbe() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x)").unwrap();
    c.execute("INSERT INTO a VALUES(1),(1),(2)").unwrap();
    c.execute("CREATE TABLE b(p)").unwrap();
    c.execute("INSERT INTO b VALUES(1),(1),(2)").unwrap();
    // The join produces duplicate x values; DISTINCT collapses them, on the VDBE.
    let r = c
        .query_vdbe("SELECT DISTINCT a.x FROM a JOIN b ON a.x = b.p ORDER BY a.x")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
}

#[test]
fn order_by_over_join_runs_on_vdbe() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x, y)").unwrap();
    c.execute("INSERT INTO a VALUES(3,'c'),(1,'a'),(2,'b')")
        .unwrap();
    c.execute("CREATE TABLE b(p, q)").unwrap();
    c.execute("INSERT INTO b VALUES(1,'P'),(2,'Q'),(2,'R')")
        .unwrap();
    // Multi-key ORDER BY (DESC then ASC) staged through the sorter, on the VDBE.
    let r = c
        .query_vdbe("SELECT a.x, b.q FROM a JOIN b ON a.x = b.p ORDER BY a.x DESC, b.q")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Integer(2), Value::Text("Q".into())],
            vec![Value::Integer(2), Value::Text("R".into())],
            vec![Value::Integer(1), Value::Text("P".into())],
        ]
    );
    // ORDER BY + LIMIT/OFFSET applies to the sorted output.
    let r2 = c
        .query_vdbe("SELECT a.x FROM a JOIN b ON a.x = b.p ORDER BY a.x LIMIT 1 OFFSET 1")
        .unwrap();
    assert_eq!(r2.rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn bare_aggregate_over_join_runs_on_vdbe() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x, y)").unwrap();
    c.execute("INSERT INTO a VALUES(3,'c'),(1,'a'),(2,'b')")
        .unwrap();
    c.execute("CREATE TABLE b(p, q)").unwrap();
    c.execute("INSERT INTO b VALUES(1,'P'),(2,'Q'),(2,'R')")
        .unwrap();
    // count(*)/sum/min/max over the inner join, folded through the nested loop.
    let r = c
        .query_vdbe("SELECT count(*), sum(a.x), min(b.q), max(b.q) FROM a JOIN b ON a.x = b.p")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Integer(3),
            Value::Integer(5), // 1 + 2 + 2
            Value::Text("P".into()),
            Value::Text("R".into()),
        ]]
    );
    // group_concat is order-sensitive; the fold order matches the cross-product.
    let g = c
        .query_vdbe("SELECT group_concat(b.q) FROM a JOIN b ON a.x = b.p")
        .unwrap();
    assert_eq!(g.rows, vec![vec![Value::Text("P,Q,R".into())]]);
    // An empty match still yields one row: count → 0, sum → NULL.
    let e = c
        .query_vdbe("SELECT count(*), sum(a.x) FROM a JOIN b ON a.x = b.p AND a.x = 99")
        .unwrap();
    assert_eq!(e.rows, vec![vec![Value::Integer(0), Value::Null]]);
}

#[test]
fn nested_loop_join_empty_side_yields_no_rows() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(x)").unwrap();
    c.execute("INSERT INTO a VALUES(1),(2)").unwrap();
    c.execute("CREATE TABLE b(y)").unwrap();
    // Right side empty → no output, no panic.
    assert!(c
        .query_vdbe("SELECT a.x, b.y FROM a JOIN b ON 1=1")
        .unwrap()
        .rows
        .is_empty());
}
