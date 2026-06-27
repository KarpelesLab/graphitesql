//! LIMIT / OFFSET apply SQLite's `OP_MustBeInt`: the value must be an integer
//! (or a losslessly integer-valued real / clean numeric text), else a
//! "datatype mismatch" error. graphite previously truncated lossily and treated
//! NULL as zero. Matched to the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

#[test]
fn non_integer_limit_offset_errors() {
    let c = Connection::open_memory().unwrap();
    // These all raise "datatype mismatch" in sqlite — and must error here too,
    // not silently truncate / treat NULL as 0.
    for sql in [
        "SELECT 1 LIMIT 1.9",
        "SELECT 1 LIMIT '2abc'",
        "SELECT 1 LIMIT 'abc'",
        "SELECT 1 LIMIT NULL",
        "SELECT 1 LIMIT 5 OFFSET NULL",
        "SELECT 1 LIMIT 5 OFFSET 1.5",
        "SELECT 1 LIMIT x'32'",
        "SELECT 1 LIMIT 3.0/2",
    ] {
        assert!(c.query(sql).is_err(), "{sql} should be a datatype mismatch");
    }
}

#[test]
fn non_integer_limit_offset_errors_over_a_table_scan() {
    // A `SELECT … FROM t` over a real table compiles to the VDBE scan path,
    // which folds a constant LIMIT/OFFSET separately from the constant-row
    // `SELECT 1` path above. That folder previously truncated lossily (1.9 → 1)
    // and parsed text/NULL leniently, so the table-scan path silently accepted
    // what sqlite rejects. It must now bail to the interpreter and raise the
    // same "datatype mismatch".
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3)").unwrap();
    for sql in [
        "SELECT a FROM t LIMIT 1.9",
        "SELECT a FROM t LIMIT '2abc'",
        "SELECT a FROM t LIMIT 'abc'",
        "SELECT a FROM t LIMIT NULL",
        "SELECT a FROM t LIMIT x'32'",
        "SELECT a FROM t LIMIT 2 OFFSET 1.5",
        "SELECT a FROM t LIMIT 2 OFFSET NULL",
        "SELECT a FROM t LIMIT 2 OFFSET 'x'",
    ] {
        assert!(c.query(sql).is_err(), "{sql} should be a datatype mismatch");
    }
}

#[test]
fn integer_valued_limits_work() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3),(4),(5)")
        .unwrap();
    let g = |sql: &str| -> Vec<i64> {
        c.query(sql)
            .unwrap()
            .rows
            .into_iter()
            .map(|mut r| match r.remove(0) {
                Value::Integer(i) => i,
                other => panic!("{other:?}"),
            })
            .collect()
    };
    // Integer-valued reals and clean numeric text are accepted.
    assert_eq!(g("SELECT a FROM t ORDER BY a LIMIT 2"), vec![1, 2]);
    assert_eq!(g("SELECT a FROM t ORDER BY a LIMIT 2.0"), vec![1, 2]);
    assert_eq!(g("SELECT a FROM t ORDER BY a LIMIT '3'"), vec![1, 2, 3]);
    assert_eq!(g("SELECT a FROM t ORDER BY a LIMIT 2+1"), vec![1, 2, 3]);
    assert_eq!(g("SELECT a FROM t ORDER BY a LIMIT 1.0"), vec![1]);
    // Negative LIMIT means no limit; OFFSET still applies.
    assert_eq!(
        g("SELECT a FROM t ORDER BY a LIMIT -1 OFFSET 3"),
        vec![4, 5]
    );
    assert_eq!(
        g("SELECT a FROM t ORDER BY a LIMIT 2 OFFSET '1'"),
        vec![2, 3]
    );
}
