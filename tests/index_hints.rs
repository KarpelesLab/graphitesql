//! Track A: `INDEXED BY` / `NOT INDEXED` query hints. The hints steer the
//! planner; results must be identical to an unhinted query.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, k INT, v TEXT)")
        .unwrap();
    c.execute("CREATE INDEX ik ON t(k)").unwrap();
    for i in 0..20 {
        c.execute(&format!(
            "INSERT INTO t(id,k,v) VALUES ({i},{},'r{i}')",
            i % 4
        ))
        .unwrap();
    }
    c
}

fn count(c: &Connection, sql: &str) -> i64 {
    match c.query(sql).unwrap().rows[0][0] {
        Value::Integer(i) => i,
        _ => panic!("not an int"),
    }
}

#[test]
fn not_indexed_still_correct() {
    let c = setup();
    // The index would normally be used; NOT INDEXED forces a scan, same result.
    assert_eq!(
        count(&c, "SELECT count(*) FROM t NOT INDEXED WHERE k = 2"),
        5
    );
    assert_eq!(
        count(&c, "SELECT count(*) FROM t NOT INDEXED WHERE id = 7"),
        1
    );
}

#[test]
fn indexed_by_uses_named_index() {
    let c = setup();
    assert_eq!(
        count(&c, "SELECT count(*) FROM t INDEXED BY ik WHERE k = 1"),
        5
    );
    // The hinted index must exist.
    assert!(
        c.query("SELECT * FROM t INDEXED BY no_such_index WHERE k = 1")
            .is_err()
    );
}

#[test]
fn hints_match_unhinted() {
    let c = setup();
    let plain = count(&c, "SELECT count(*) FROM t WHERE k = 3");
    assert_eq!(
        count(&c, "SELECT count(*) FROM t NOT INDEXED WHERE k = 3"),
        plain
    );
    assert_eq!(
        count(&c, "SELECT count(*) FROM t INDEXED BY ik WHERE k = 3"),
        plain
    );
}
