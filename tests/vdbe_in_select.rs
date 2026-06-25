//! B5c (VDBE depth): a non-correlated `IN (SELECT <computed>)` candidate set is
//! materialized and folded to `IN (list)` by the router's fold pre-pass, so the
//! query runs on the VDBE (`query_vdbe` errors on fallback). Only computed (NONE-
//! affinity) candidate columns are folded — a bare-column candidate keeps its
//! column affinity and is left for the tree-walker. Results match sqlite.
#![cfg(feature = "std")]
use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3)").unwrap();
    c
}
fn vdbe1(c: &Connection, sql: &str) -> Value {
    c.query_vdbe(sql).unwrap().rows[0][0].clone()
}

#[test]
fn computed_in_select_runs_on_vdbe() {
    let c = setup();
    // foldable: computed candidate column → runs on VDBE, matches sqlite
    assert_eq!(
        vdbe1(&c, "SELECT 2 IN (SELECT x+0 FROM t)"),
        Value::Integer(1)
    );
    assert_eq!(
        vdbe1(&c, "SELECT 9 IN (SELECT x+0 FROM t)"),
        Value::Integer(0)
    );
    assert_eq!(
        vdbe1(&c, "SELECT 5 NOT IN (SELECT x*10 FROM t)"),
        Value::Integer(1)
    );
    // empty candidate set → IN is false, NOT IN is true (even structurally)
    assert_eq!(
        vdbe1(&c, "SELECT 1 IN (SELECT x+0 FROM t WHERE 0)"),
        Value::Integer(0)
    );
    assert_eq!(
        vdbe1(&c, "SELECT 1 NOT IN (SELECT x+0 FROM t WHERE 0)"),
        Value::Integer(1)
    );
    // in WHERE position, as a filter
    let rows = c
        .query_vdbe("SELECT x FROM t WHERE x IN (SELECT x+0 FROM t WHERE x<>2) ORDER BY x")
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(3)]]);
}

#[test]
fn bare_column_in_select_still_defers() {
    // A bare-column candidate (x, not computed) keeps its column affinity, so it
    // must NOT be folded — query_vdbe should error (fall back to the tree-walker).
    let c = setup();
    assert!(
        c.query_vdbe("SELECT 2 IN (SELECT x FROM t)").is_err(),
        "bare-column IN(SELECT) must defer from the VDBE, not fold"
    );
    // ...but plain query() (tree-walker) still answers it correctly.
    assert_eq!(
        c.query("SELECT 2 IN (SELECT x FROM t)").unwrap().rows[0][0],
        Value::Integer(1)
    );
}
