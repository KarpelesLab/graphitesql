//! EXPLAIN QUERY PLAN reports a seek through an implicit `sqlite_autoindex_*`
//! index (a non-integer PRIMARY KEY or a UNIQUE column), matching the sqlite3
//! 3.50.4 CLI — previously these read as `SCAN` because the EQP skipped indexes
//! without a CREATE statement, even though the executor already seeked them.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn detail(c: &Connection, sql: &str) -> String {
    let r = c.query(sql).unwrap();
    r.rows
        .iter()
        .map(|row| match row.last() {
            Some(Value::Text(s)) => String::from(s.as_str()),
            other => panic!("detail not text: {other:?}"),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn autoindex_seeks_report_search() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE pk(a TEXT PRIMARY KEY, b)").unwrap();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM pk WHERE a='x'"),
        "SEARCH pk USING INDEX sqlite_autoindex_pk_1 (a=?)"
    );

    c.execute("CREATE TABLE u(a, b UNIQUE)").unwrap();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM u WHERE b=5"),
        "SEARCH u USING INDEX sqlite_autoindex_u_1 (b=?)"
    );

    // Composite PRIMARY KEY autoindex (covering, since the query only needs the
    // key columns).
    c.execute("CREATE TABLE ck(a, b, PRIMARY KEY(a,b))")
        .unwrap();
    assert_eq!(
        detail(
            &c,
            "EXPLAIN QUERY PLAN SELECT a,b FROM ck WHERE a=1 AND b=2"
        ),
        "SEARCH ck USING COVERING INDEX sqlite_autoindex_ck_1 (a=? AND b=?)"
    );
}

#[test]
fn explicit_index_and_scan_unchanged() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("CREATE INDEX i ON t(a)").unwrap();
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t WHERE a=1"),
        "SEARCH t USING INDEX i (a=?)"
    );
    assert_eq!(
        detail(&c, "EXPLAIN QUERY PLAN SELECT * FROM t WHERE b=1"),
        "SCAN t"
    );
}
