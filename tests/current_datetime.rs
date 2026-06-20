//! `CURRENT_DATE` / `CURRENT_TIME` / `CURRENT_TIMESTAMP` keywords, equivalent to
//! `date`/`time`/`datetime('now')` (UTC). Clock-based, so checked for equivalence
//! and shape rather than an exact constant.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn text(c: &Connection, sql: &str) -> String {
    match &c.query(sql).unwrap().rows[0][0] {
        Value::Text(s) => s.clone(),
        other => panic!("not text: {other:?}"),
    }
}

#[test]
fn current_keywords_match_now_functions() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        c.query("SELECT CURRENT_DATE = date('now')").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("SELECT CURRENT_TIMESTAMP = datetime('now')")
            .unwrap()
            .rows[0][0],
        Value::Integer(1)
    );
    // Shapes.
    assert_eq!(text(&c, "SELECT CURRENT_DATE").len(), 10); // YYYY-MM-DD
    assert_eq!(text(&c, "SELECT CURRENT_TIME").len(), 8); // HH:MM:SS
    assert_eq!(text(&c, "SELECT CURRENT_TIMESTAMP").len(), 19); // YYYY-MM-DD HH:MM:SS
}

#[test]
fn current_keywords_as_column_defaults() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, d DEFAULT CURRENT_DATE, ts DEFAULT CURRENT_TIMESTAMP)")
        .unwrap();
    c.execute("INSERT INTO t DEFAULT VALUES").unwrap();
    assert_eq!(
        c.query("SELECT d = date('now'), ts = datetime('now') FROM t")
            .unwrap()
            .rows[0],
        vec![Value::Integer(1), Value::Integer(1)]
    );
}

#[test]
fn quoted_identifier_is_still_a_column() {
    // A double-quoted `"current_date"` is an identifier (column), not the keyword.
    let c = Connection::open_memory().unwrap();
    assert!(c.query("SELECT \"current_date\"").is_err());
}
