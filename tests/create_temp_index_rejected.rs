//! `TEMP`/`TEMPORARY` is only valid before `TABLE`, `VIEW`, or `TRIGGER`.
//! SQLite rejects `CREATE TEMP INDEX` and `CREATE TEMP VIRTUAL TABLE` as
//! `near "<token>": syntax error`; graphite used to accept `CREATE TEMP INDEX`
//! (placing the index in the temp schema) and `CREATE TEMP VIRTUAL TABLE`.
//! The valid `TEMP TABLE/VIEW/TRIGGER` and plain `[UNIQUE] INDEX` /
//! `VIRTUAL TABLE` forms are unaffected. Verified vs sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::Connection;

#[test]
fn temp_index_is_a_syntax_error() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    for sql in [
        "CREATE TEMP INDEX ix ON t(a)",
        "CREATE TEMPORARY INDEX ix ON t(a)",
    ] {
        let e = c.execute(sql).unwrap_err().to_string();
        assert!(e.contains("near \"INDEX\": syntax error"), "{sql} => {e}");
    }
}

#[test]
fn temp_virtual_table_is_a_syntax_error() {
    let mut c = Connection::open_memory().unwrap();
    let e = c
        .execute("CREATE TEMP VIRTUAL TABLE v USING fts5(x)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("near \"VIRTUAL\": syntax error"), "{e}");
}

#[test]
fn valid_create_forms_still_work() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("CREATE INDEX ix1 ON t(a)").unwrap();
    c.execute("CREATE UNIQUE INDEX ix2 ON t(a)").unwrap();
    c.execute("CREATE TEMP TABLE tt(a)").unwrap();
    c.execute("CREATE TEMP VIEW vv AS SELECT 1").unwrap();
    c.execute("CREATE TEMP TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END")
        .unwrap();
    c.execute("CREATE VIRTUAL TABLE v USING fts5(x)").unwrap();
}
