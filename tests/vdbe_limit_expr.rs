//! VDBE track: non-constant `LIMIT`/`OFFSET`. Previously the VDBE compiler only
//! accepted a constant integer LIMIT/OFFSET and deferred everything else to the
//! tree-walker. It now compiles the LIMIT/OFFSET expression into a register and
//! coerces it with `Op::MustBeInt` (SQLite's `computeLimitRegisters`), so an
//! arithmetic / string / function / folded-subquery count runs on the VDBE.
//! `query_vdbe` FORCES the VDBE and errors on fallback, so these assertions prove
//! the VDBE — not the tree-walker — produced the result.
#![cfg(feature = "std")]
use graphitesql::{Connection, Error, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x INT)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3),(4),(5)")
        .unwrap();
    c
}

fn ints(c: &Connection, sql: &str) -> Vec<i64> {
    c.query_vdbe(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref o => panic!("not int: {o:?}"),
        })
        .collect()
}

#[test]
fn non_constant_limit_offset_run_on_vdbe() {
    let c = setup();
    // Arithmetic, string-coerced, and function LIMIT.
    assert_eq!(ints(&c, "SELECT x FROM t ORDER BY x LIMIT 2+1"), [1, 2, 3]);
    assert_eq!(ints(&c, "SELECT x FROM t ORDER BY x LIMIT '3'"), [1, 2, 3]);
    assert_eq!(ints(&c, "SELECT x FROM t ORDER BY x LIMIT abs(-2)"), [1, 2]);
    // A whole real coerces; negative is unlimited; zero is empty.
    assert_eq!(
        ints(&c, "SELECT x FROM t ORDER BY x LIMIT 3.0e0"),
        [1, 2, 3]
    );
    assert_eq!(
        ints(&c, "SELECT x FROM t ORDER BY x LIMIT -1"),
        [1, 2, 3, 4, 5]
    );
    assert!(ints(&c, "SELECT x FROM t LIMIT 0").is_empty());
    // OFFSET expressions (a negative offset skips nothing).
    assert_eq!(
        ints(&c, "SELECT x FROM t ORDER BY x LIMIT 2 OFFSET 1+1"),
        [3, 4]
    );
    assert_eq!(
        ints(&c, "SELECT x FROM t ORDER BY x LIMIT 2 OFFSET '1'"),
        [2, 3]
    );
    assert_eq!(
        ints(&c, "SELECT x FROM t ORDER BY x LIMIT 2 OFFSET -5"),
        [1, 2]
    );
    // ORDER BY DESC with an expression LIMIT + OFFSET.
    assert_eq!(
        ints(&c, "SELECT x FROM t ORDER BY x DESC LIMIT 2+1 OFFSET 1"),
        [4, 3, 2]
    );
    // A folded non-correlated scalar subquery LIMIT.
    assert_eq!(
        ints(&c, "SELECT x FROM t ORDER BY x LIMIT (SELECT 2)"),
        [1, 2]
    );
    // DISTINCT + expression LIMIT.
    assert_eq!(
        ints(&c, "SELECT DISTINCT x FROM t ORDER BY x LIMIT 1*2"),
        [1, 2]
    );
}

#[test]
fn non_integer_limit_offset_is_datatype_mismatch_on_vdbe() {
    let c = setup();
    // A fractional real, NULL, non-numeric text, and a NULL OFFSET are all
    // SQLite's `datatype mismatch` (SQLITE_MISMATCH), raised by `Op::MustBeInt`.
    for sql in [
        "SELECT x FROM t LIMIT 2.9",
        "SELECT x FROM t LIMIT NULL",
        "SELECT x FROM t LIMIT 'abc'",
        "SELECT x FROM t LIMIT 2 OFFSET NULL",
    ] {
        match c.query_vdbe(sql).unwrap_err() {
            Error::Error(m) => assert_eq!(m, "datatype mismatch", "for `{sql}`"),
            other => panic!("`{sql}`: expected datatype mismatch, got {other:?}"),
        }
    }
}
