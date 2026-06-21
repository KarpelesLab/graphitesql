//! `Connection::execute_batch` runs a `;`-separated multi-statement script
//! (roadmap A7), respecting string literals, comments, and `BEGIN…END` /
//! `CASE…END` blocks so a `;` inside one does not split a statement.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

#[test]
fn runs_multiple_statements_in_order() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(a, b);
         INSERT INTO t VALUES (1, 10);
         INSERT INTO t VALUES (2, 20);
         UPDATE t SET b = b + 1 WHERE a = 1;",
    )
    .unwrap();
    assert_eq!(
        c.query("SELECT a, b FROM t ORDER BY a").unwrap().rows,
        [
            vec![Value::Integer(1), Value::Integer(11)],
            vec![Value::Integer(2), Value::Integer(20)],
        ]
    );
}

#[test]
fn semicolons_inside_strings_and_comments_do_not_split() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(s);
         -- a comment; with a semicolon
         INSERT INTO t VALUES ('a;b;c'); /* block ; comment */
         INSERT INTO t VALUES ('plain');",
    )
    .unwrap();
    assert_eq!(
        c.query("SELECT s FROM t ORDER BY s").unwrap().rows,
        [
            vec![Value::Text("a;b;c".into())],
            vec![Value::Text("plain".into())],
        ]
    );
}

#[test]
fn trigger_body_with_inner_semicolons_is_one_statement() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE src(x);
         CREATE TABLE log(x);
         CREATE TRIGGER trg AFTER INSERT ON src BEGIN
             INSERT INTO log VALUES (NEW.x);
             UPDATE log SET x = x + 100 WHERE x = NEW.x;
         END;
         INSERT INTO src VALUES (5);",
    )
    .unwrap();
    assert_eq!(
        c.query("SELECT x FROM log").unwrap().rows,
        [vec![Value::Integer(105)]]
    );
}

#[test]
fn case_block_and_trailing_select_are_handled() {
    let mut c = Connection::open_memory().unwrap();
    // A CASE expression contains no bare `;`, but exercise the END bookkeeping;
    // a trailing SELECT in the script runs and is discarded without error.
    c.execute_batch(
        "CREATE TABLE t(n);
         INSERT INTO t VALUES (1), (2), (3);
         SELECT CASE WHEN n > 1 THEN 'big' ELSE 'small' END FROM t;",
    )
    .unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(3)
    );
}

#[test]
fn explicit_transaction_in_script_commits() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(a);
         BEGIN;
         INSERT INTO t VALUES (1);
         INSERT INTO t VALUES (2);
         COMMIT;",
    )
    .unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(2)
    );
}

#[test]
fn stops_at_first_error() {
    let mut c = Connection::open_memory().unwrap();
    let err = c.execute_batch(
        "CREATE TABLE t(a);
         INSERT INTO t VALUES (1);
         INSERT INTO nonexistent VALUES (2);
         INSERT INTO t VALUES (3);",
    );
    assert!(err.is_err());
    // The first insert committed; the statement after the failure did not run.
    assert_eq!(
        c.query("SELECT a FROM t").unwrap().rows,
        [vec![Value::Integer(1)]]
    );
}

#[test]
fn trailing_comment_or_empty_segments_are_ignored() {
    let mut c = Connection::open_memory().unwrap();
    // Empty statements between semicolons and a trailing comment are no-ops.
    c.execute_batch("CREATE TABLE t(a);; INSERT INTO t VALUES (1); -- done")
        .unwrap();
    assert_eq!(
        c.query("SELECT a FROM t").unwrap().rows,
        [vec![Value::Integer(1)]]
    );
}
