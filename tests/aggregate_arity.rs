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

#[test]
fn too_many_args_aggregates_and_window_funcs_error() {
    // sqlite rejects extra arguments to a builtin aggregate or a ranking/value
    // window function ("wrong number of arguments"); graphite used to ignore them.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(v)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3)").unwrap();
    for sql in [
        // Aggregates that take exactly one argument.
        "SELECT sum(1, 2)",
        "SELECT total(1, 2)",
        "SELECT avg(1, 2)",
        "SELECT count(1, 2)",
        "SELECT sum(v, v) FROM t",
        // Ranking window functions take no argument; value ones a fixed count.
        "SELECT row_number(1) OVER () FROM t",
        "SELECT rank(1) OVER () FROM t",
        "SELECT dense_rank(1) OVER () FROM t",
        "SELECT cume_dist(1) OVER () FROM t",
        "SELECT ntile() OVER () FROM t",
        "SELECT ntile(2, 3) OVER () FROM t",
        "SELECT lag() OVER () FROM t",
        "SELECT lag(v, 1, 0, 9) OVER () FROM t",
        "SELECT first_value() OVER () FROM t",
        "SELECT nth_value(v) OVER () FROM t",
    ] {
        assert!(
            c.query(sql).is_err(),
            "{sql} should error (wrong arg count)"
        );
    }
    // The valid argument counts still work.
    for sql in [
        "SELECT sum(v), avg(v), count(*), count(v), group_concat(v), group_concat(v, '-') FROM t",
        "SELECT row_number() OVER (ORDER BY v), ntile(2) OVER (ORDER BY v), \
         lag(v) OVER (ORDER BY v), lag(v, 1, 0) OVER (ORDER BY v), \
         nth_value(v, 2) OVER (ORDER BY v) FROM t",
    ] {
        assert!(c.query(sql).is_ok(), "{sql} should succeed");
    }
}
