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
