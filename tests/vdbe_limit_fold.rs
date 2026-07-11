//! The VDBE folds a `LIMIT`/`OFFSET` that is a constant expression of
//! deterministic, stateless scalar functions (`abs`, `round`, `length`, …), not
//! just an integer literal. This is a Track-B coverage step: it only moves the
//! query onto the VDBE when the fold is provably safe; a clock/random/state
//! function, or an argument referencing a column, bails to the tree-walker. The
//! result is always identical to the tree-walker's (and to SQLite's), which is
//! what this checks.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn ints(conn: &Connection, sql: &str) -> Vec<i64> {
    conn.query(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref other => panic!("non-integer: {other:?}"),
        })
        .collect()
}

fn setup() -> Connection {
    let mut conn = Connection::open_memory().unwrap();
    conn.execute("CREATE TABLE t(v INTEGER)").unwrap();
    conn.execute("INSERT INTO t VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10)")
        .unwrap();
    conn
}

#[test]
fn limit_with_pure_function() {
    let conn = setup();
    assert_eq!(
        ints(&conn, "SELECT v FROM t ORDER BY v LIMIT abs(-3)"),
        [1, 2, 3]
    );
    assert_eq!(
        ints(&conn, "SELECT v FROM t ORDER BY v LIMIT round(2.9)"),
        [1, 2, 3]
    );
    assert_eq!(
        ints(&conn, "SELECT v FROM t ORDER BY v LIMIT length('abcd')"),
        [1, 2, 3, 4]
    );
}

#[test]
fn offset_with_pure_function() {
    let conn = setup();
    assert_eq!(
        ints(
            &conn,
            "SELECT v FROM t ORDER BY v LIMIT length('abcd') OFFSET length('ab')"
        ),
        [3, 4, 5, 6]
    );
}

#[test]
fn mixed_arithmetic_and_function() {
    let conn = setup();
    // (2*2) + coalesce(NULL,1) = 5
    assert_eq!(
        ints(
            &conn,
            "SELECT v FROM t ORDER BY v LIMIT (2*2)+coalesce(NULL,1)"
        ),
        [1, 2, 3, 4, 5]
    );
}

#[test]
fn non_foldable_limit_still_correct() {
    // A clock function in LIMIT must not be folded (it would diverge from the
    // tree-walker); it bails and produces the same datatype-mismatch error SQLite
    // gives, rather than a silently-wrong row count.
    let conn = setup();
    let err = conn
        .query("SELECT v FROM t LIMIT datetime('now')")
        .unwrap_err();
    assert!(
        err.to_string().contains("datatype mismatch"),
        "unexpected: {err}"
    );
}

#[test]
fn limit_via_vdbe_matches_tree_walker() {
    // Explicit parity: the VDBE result must equal the tree-walker's for the same
    // folded LIMIT.
    let conn = setup();
    let via_vdbe = conn
        .query("SELECT v FROM t ORDER BY v LIMIT abs(-4)")
        .unwrap();
    let via_tree = conn
        .query_vdbe("SELECT v FROM t ORDER BY v LIMIT abs(-4)")
        .unwrap();
    assert_eq!(via_vdbe.rows, via_tree.rows);
    assert_eq!(via_vdbe.rows.len(), 4);
}
