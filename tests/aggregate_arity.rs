//! Aggregate functions called with too few arguments error cleanly instead of
//! panicking (`group_concat()` used to index `args[0]` out of bounds).

#![cfg(feature = "std")]

use graphitesql::Connection;

#[test]
fn zero_arg_aggregates_error_not_panic() {
    let c = Connection::open_memory().unwrap();
    // Each of these is "wrong number of arguments" in sqlite — and must not panic.
    for sql in [
        "SELECT group_concat()",
        "SELECT count()",
        "SELECT sum()",
        "SELECT avg()",
        "SELECT max()",
        "SELECT min()",
        "SELECT total()",
    ] {
        assert!(c.query(sql).is_err(), "{sql} should error");
    }
}

#[test]
fn arity_errors_in_grouped_context() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3)").unwrap();
    // The aggregate path (over real rows) also rejects the short call.
    assert!(c.query("SELECT group_concat() FROM t").is_err());
    assert!(c.query("SELECT json_group_object('k') FROM t").is_err());
    // Valid calls still work.
    assert_eq!(
        c.query("SELECT group_concat(x,'-') FROM t").unwrap().rows[0][0],
        graphitesql::Value::Text("1-2-3".into())
    );
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        graphitesql::Value::Integer(3)
    );
}
