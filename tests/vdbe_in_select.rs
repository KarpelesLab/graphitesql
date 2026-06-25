//! B5c (VDBE depth): a non-correlated `IN (SELECT col)` candidate set is
//! materialized and folded to `IN (list)` by the router's fold pre-pass, so the
//! query runs on the VDBE (`query_vdbe` errors on fallback). A *computed*
//! (NONE-affinity) candidate folds to a plain list; a *bare-column* candidate
//! additionally carries its column's affinity (`candidate_affinity`) so the VDBE
//! reproduces SQLite's `combine(left_aff, col_aff)` comparison — without it the
//! fold would wrongly apply the left operand's affinity. Results match sqlite.
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
fn bare_column_in_select_runs_on_vdbe() {
    // A bare-column candidate (x, not computed) now folds WITH its column affinity
    // carried in `candidate_affinity`, so it runs on the VDBE and matches query().
    let c = setup();
    // Untyped column x → NONE affinity; left literal 2 (NONE) → no coercion needed.
    assert_eq!(
        vdbe1(&c, "SELECT 2 IN (SELECT x FROM t)"),
        Value::Integer(1)
    );
    assert_eq!(
        vdbe1(&c, "SELECT 9 IN (SELECT x FROM t)"),
        Value::Integer(0)
    );
    assert_eq!(
        vdbe1(&c, "SELECT 9 NOT IN (SELECT x FROM t)"),
        Value::Integer(1)
    );
    // Tree-walker agrees (it never sees the fold, but must give the same answer).
    assert_eq!(
        c.query("SELECT 2 IN (SELECT x FROM t)").unwrap().rows[0][0],
        Value::Integer(1)
    );
}

/// The crux of B5c-1: `combine(left_aff, candidate_col_aff)` must hold on the
/// VDBE. A bare-column candidate carries its column affinity, so the routed
/// result is byte-identical to the tree-walker (and sqlite) for every affinity
/// combo — including the cases where folding to a plain `IN (list)` would diverge
/// (TEXT-left × untyped-candidate must NOT coerce → no match).
#[test]
fn bare_column_in_select_affinity_combos_match_tree_walker() {
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "CREATE TABLE li(x INTEGER)",
        "INSERT INTO li VALUES(1)",
        "CREATE TABLE lt(x TEXT)",
        "INSERT INTO lt VALUES('1')",
        "CREATE TABLE ln(x)",
        "INSERT INTO ln VALUES(1)",
        "CREATE TABLE lr(x REAL)",
        "INSERT INTO lr VALUES(1)",
        "CREATE TABLE ct(y TEXT)",
        "INSERT INTO ct VALUES('1')",
        "CREATE TABLE ci(y INTEGER)",
        "INSERT INTO ci VALUES(1)",
        "CREATE TABLE cn(y)",
        "INSERT INTO cn VALUES(1)",
        "CREATE TABLE cf(y TEXT)",
        "INSERT INTO cf VALUES('1.0')",
    ] {
        c.execute(s).unwrap();
    }
    // (query, expected count) — the exact shapes the sqlite3 oracle pins down.
    let cases = [
        ("SELECT count(*) FROM ln WHERE x IN (SELECT y FROM ct)", 0), // none/text
        ("SELECT count(*) FROM lt WHERE x IN (SELECT y FROM cn)", 0), // text/none
        ("SELECT count(*) FROM li WHERE x IN (SELECT y FROM ct)", 1), // int/text
        ("SELECT count(*) FROM ln WHERE x IN (SELECT y FROM cn)", 1), // none/none
        ("SELECT count(*) FROM lt WHERE x IN (SELECT y FROM ci)", 1), // text/int
        ("SELECT count(*) FROM li WHERE x IN (SELECT y FROM ci)", 1), // int/int
        ("SELECT count(*) FROM lr WHERE x IN (SELECT y FROM ct)", 1), // real/text
        ("SELECT count(*) FROM li WHERE x IN (SELECT y FROM cf)", 1), // int/'1.0'
        (
            "SELECT count(*) FROM li WHERE x NOT IN (SELECT y FROM ct)",
            0,
        ), // NOT IN
    ];
    for (q, want) in cases {
        // Must RUN on the VDBE (the bare-column fold succeeded, no fallback).
        let v = c.query_vdbe(q).unwrap().rows[0][0].clone();
        assert_eq!(v, Value::Integer(want), "VDBE result diverged: {q}");
        // ...and equal the tree-walker.
        assert_eq!(
            c.query(q).unwrap().rows[0][0],
            Value::Integer(want),
            "tree-walker diverged: {q}"
        );
    }
}
