//! Error-message-body parity with sqlite3 for two resolution failures. First, an
//! unknown *qualified* column keeps its qualifier (`no such column: t.c`), where
//! graphite previously dropped it (`no such column: c`). Second, a compound-query
//! ORDER BY term that names no output column reports `<ordinal> ORDER BY term does
//! not match any column in the result set`, where graphite previously raised a
//! "not yet implemented" error. Verified against sqlite3 3.50.4 (the CLI's "Parse
//! error"/"Runtime error" wrapper differs and is normalized by the corpus).

#![cfg(feature = "std")]

use graphitesql::Connection;

fn msg(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn unknown_qualified_column_keeps_qualifier() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1, 2)").unwrap();

    assert_eq!(msg(&c, "SELECT t.c FROM t"), "no such column: t.c");
    assert_eq!(
        msg(&c, "SELECT a FROM t WHERE t.zzz = 1"),
        "no such column: t.zzz"
    );
    assert_eq!(
        msg(&c, "SELECT a FROM t ORDER BY t.qqq"),
        "no such column: t.qqq"
    );
    // An unqualified reference is still reported by bare name.
    assert_eq!(msg(&c, "SELECT bad FROM t"), "no such column: bad");
}

#[test]
fn compound_order_by_non_matching_term() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        msg(&c, "SELECT 1 a UNION SELECT 2 a ORDER BY b"),
        "1st ORDER BY term does not match any column in the result set"
    );
    assert_eq!(
        msg(&c, "SELECT 1 a UNION ALL SELECT 2 ORDER BY zzz"),
        "1st ORDER BY term does not match any column in the result set"
    );
    // The ordinal tracks the offending term's position: here the second.
    assert_eq!(
        msg(&c, "SELECT 1 a UNION SELECT 2 ORDER BY a, b"),
        "2nd ORDER BY term does not match any column in the result set"
    );
    // A matching term (by alias or position) still orders correctly.
    assert!(c.query("SELECT 1 a UNION SELECT 2 ORDER BY a").is_ok());
    assert!(c.query("SELECT 1 a UNION SELECT 2 ORDER BY 1").is_ok());
}
