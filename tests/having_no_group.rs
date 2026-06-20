//! HAVING without GROUP BY: allowed when the query is aggregated (the whole
//! result is one group), rejected on a non-aggregate query — matching SQLite.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT)").unwrap();
    c.execute("INSERT INTO t VALUES(1),(2),(3)").unwrap();
    c
}

#[test]
fn having_without_group_by_over_aggregate() {
    let c = setup();
    // The whole table is one group; HAVING filters that group.
    assert_eq!(
        c.query("SELECT count(*) FROM t HAVING count(*) > 1")
            .unwrap()
            .rows[0][0],
        Value::Integer(3)
    );
    assert!(c
        .query("SELECT count(*) FROM t HAVING count(*) > 5")
        .unwrap()
        .rows
        .is_empty());
    assert_eq!(
        c.query("SELECT sum(a) FROM t HAVING max(a) = 3")
            .unwrap()
            .rows[0][0],
        Value::Integer(6)
    );
}

#[test]
fn having_on_non_aggregate_query_is_rejected() {
    let c = setup();
    assert!(c.query("SELECT a FROM t HAVING 1 = 1").is_err());
}

#[test]
fn group_by_having_still_works() {
    let mut c = setup();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    let r = c
        .query("SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Integer(1), Value::Integer(2)]]);
}
