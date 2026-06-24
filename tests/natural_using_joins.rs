//! NATURAL and USING joins: equality on common/named columns, with those
//! columns coalesced into a single output column. Verified against sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE e(id INT, name TEXT, dept INT)")
        .unwrap();
    c.execute("CREATE TABLE d(dept INT, dname TEXT)").unwrap();
    c.execute("INSERT INTO e VALUES(1,'a',10),(2,'b',20),(3,'c',99)")
        .unwrap();
    c.execute("INSERT INTO d VALUES(10,'X'),(20,'Y')").unwrap();
    c
}

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

#[test]
fn using_natural_join_applies_key_affinity() {
    // A NATURAL/USING join's coalesce-key equality applies each side's column
    // affinity, like an `ON l = r` equality — so a cross-type key matches
    // (INTEGER 1 = TEXT '1'), while a non-numeric text still does not.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(k INTEGER, x)").unwrap();
    c.execute("INSERT INTO a VALUES(1, 10)").unwrap();
    c.execute("CREATE TABLE b(k TEXT, y)").unwrap();
    c.execute("INSERT INTO b VALUES('1', 20)").unwrap();
    assert_eq!(
        rows(&c, "SELECT count(*) FROM a JOIN b USING(k)")[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        rows(&c, "SELECT count(*) FROM a NATURAL JOIN b")[0][0],
        Value::Integer(1)
    );
    // The coalesced key takes the left value.
    assert_eq!(
        rows(&c, "SELECT k FROM a JOIN b USING(k)")[0][0],
        Value::Integer(1)
    );
    // A non-numeric text key cannot match the INTEGER key.
    c.execute("INSERT INTO b VALUES('z', 30)").unwrap();
    assert_eq!(
        rows(&c, "SELECT count(*) FROM a JOIN b USING(k)")[0][0],
        Value::Integer(1)
    );
}

#[test]
fn natural_join_coalesces_common_column() {
    let c = setup();
    // SELECT * keeps the common column (dept) once, in its left position:
    // (id, name, dept, dname).
    assert_eq!(
        rows(&c, "SELECT * FROM e NATURAL JOIN d ORDER BY id"),
        vec![
            vec![
                Value::Integer(1),
                Value::Text("a".into()),
                Value::Integer(10),
                Value::Text("X".into())
            ],
            vec![
                Value::Integer(2),
                Value::Text("b".into()),
                Value::Integer(20),
                Value::Text("Y".into())
            ],
        ]
    );
    // The coalesced column is referenceable unqualified.
    assert_eq!(
        rows(&c, "SELECT dept FROM e NATURAL JOIN d ORDER BY dept"),
        vec![vec![Value::Integer(10)], vec![Value::Integer(20)]]
    );
}

#[test]
fn using_join_matches_natural() {
    let c = setup();
    assert_eq!(
        rows(&c, "SELECT * FROM e JOIN d USING(dept) ORDER BY id"),
        rows(&c, "SELECT * FROM e NATURAL JOIN d ORDER BY id")
    );
    // A USING column missing from one side is an error.
    assert!(c.query("SELECT * FROM e JOIN d USING(nope)").is_err());
}

#[test]
fn natural_left_join_coalesces_from_left_when_unmatched() {
    let c = setup();
    // dept 99 has no match in d; the coalesced dept keeps the left value, dname NULL.
    assert_eq!(
        rows(
            &c,
            "SELECT id, dept, dname FROM e NATURAL LEFT JOIN d ORDER BY id"
        ),
        vec![
            vec![
                Value::Integer(1),
                Value::Integer(10),
                Value::Text("X".into())
            ],
            vec![
                Value::Integer(2),
                Value::Integer(20),
                Value::Text("Y".into())
            ],
            vec![Value::Integer(3), Value::Integer(99), Value::Null],
        ]
    );
}

#[test]
fn natural_self_join_is_not_a_cross_join() {
    // Regression: NATURAL was parsed as a table alias, silently producing a
    // cross join. A self natural join matches each row only with itself.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT, b INT)").unwrap();
    c.execute("INSERT INTO t VALUES(1,10),(2,20),(1,99)")
        .unwrap();
    assert_eq!(
        rows(&c, "SELECT count(*) FROM t NATURAL JOIN t")[0][0],
        Value::Integer(3)
    );
}

#[test]
fn natural_join_no_common_column_is_cross_join() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE p(x INT)").unwrap();
    c.execute("CREATE TABLE q(y INT)").unwrap();
    c.execute("INSERT INTO p VALUES(1),(2)").unwrap();
    c.execute("INSERT INTO q VALUES(3),(4)").unwrap();
    assert_eq!(
        rows(&c, "SELECT count(*) FROM p NATURAL JOIN q")[0][0],
        Value::Integer(4)
    );
}
