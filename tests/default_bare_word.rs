//! An unparenthesized bare-word column `DEFAULT` (`DEFAULT abc`) is valid in
//! SQLite: the identifier is taken as a STRING literal (`DEFAULT abc` stores
//! `'abc'`), and the schema reprints it verbatim. graphite used to reject it as
//! "default value of column is not constant" (it parsed the word as a column
//! reference). A *parenthesized* `DEFAULT (abc)` remains a real expression and is
//! still rejected as non-constant. Byte-verified against sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

#[test]
fn bare_word_default_is_a_string_literal() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch("CREATE TABLE t(a DEFAULT abc, b DEFAULT xyz_123, k INT);")
        .unwrap();
    c.execute("INSERT INTO t(k) VALUES(1)").unwrap();
    let row = &c.query("SELECT a, b FROM t").unwrap().rows[0];
    assert_eq!(row[0], Value::Text("abc".into()));
    assert_eq!(row[1], Value::Text("xyz_123".into()));
}

#[test]
fn bare_word_default_reprints_verbatim() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a DEFAULT abc)").unwrap();
    assert_eq!(
        c.query("SELECT sql FROM sqlite_master WHERE name='t'")
            .unwrap()
            .rows[0][0],
        Value::Text("CREATE TABLE t(a DEFAULT abc)".into())
    );
}

#[test]
fn parenthesized_column_default_still_rejected() {
    let mut c = Connection::open_memory().unwrap();
    // `DEFAULT (abc)` is an expression (a column ref) → non-constant, rejected.
    assert!(c.execute("CREATE TABLE t(a DEFAULT (abc))").is_err());
}

#[test]
fn other_default_forms_unaffected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE t(a DEFAULT 'lit', b DEFAULT 42, c DEFAULT NULL,
                        d DEFAULT TRUE, e DEFAULT (abs(-5)));",
    )
    .unwrap();
    c.execute("INSERT INTO t DEFAULT VALUES").unwrap();
    let row = &c.query("SELECT a, b, c, d, e FROM t").unwrap().rows[0];
    assert_eq!(row[0], Value::Text("lit".into()));
    assert_eq!(row[1], Value::Integer(42));
    assert_eq!(row[2], Value::Null);
    assert_eq!(row[3], Value::Integer(1));
    assert_eq!(row[4], Value::Integer(5));
}
