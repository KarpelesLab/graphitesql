//! A PRAGMA argument is a single name / string / number — never a dotted
//! (schema-qualified) name. SQLite rejects `PRAGMA table_info(main.t)` with
//! `near ".": syntax error`; graphite used to parse the argument as a full
//! expression and accept `main.t`. The schema-qualified *pragma* form
//! (`PRAGMA main.table_info(t)`) is unaffected. Verified vs sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::Connection;

#[test]
fn dotted_pragma_argument_is_a_syntax_error() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    for sql in [
        "PRAGMA table_info(main.t)",
        "PRAGMA table_info(temp.t)",
        "PRAGMA index_list(x.y)",
    ] {
        let e = c.query(sql).unwrap_err().to_string();
        assert!(e.contains("near \".\": syntax error"), "{sql} => {e}");
    }
}

#[test]
fn valid_pragma_argument_forms_still_work() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER, b TEXT)").unwrap();
    // Bare name, quoted name, and string argument.
    assert_eq!(c.query("PRAGMA table_info(t)").unwrap().rows.len(), 2);
    assert_eq!(c.query("PRAGMA table_info(\"t\")").unwrap().rows.len(), 2);
    assert_eq!(c.query("PRAGMA table_info('t')").unwrap().rows.len(), 2);
    // Schema-qualified pragma (dot before the pragma NAME) is fine.
    assert_eq!(c.query("PRAGMA main.table_info(t)").unwrap().rows.len(), 2);
    // Numeric / keyword setting values.
    c.execute("PRAGMA user_version=42").unwrap();
    c.execute("PRAGMA foreign_keys=ON").unwrap();
}
