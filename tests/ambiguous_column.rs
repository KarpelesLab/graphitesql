//! Ambiguous unqualified column names. SQLite rejects a column reference that
//! matches columns from two different FROM sources ("ambiguous column name");
//! a NATURAL/USING join coalesces its shared column, and a qualifier (table or
//! alias) disambiguates. graphite must match — and must NOT over-reject a valid
//! reference.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn setup() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE x(a, b)").unwrap();
    c.execute("CREATE TABLE y(a, c)").unwrap();
    c.execute("INSERT INTO x VALUES(1, 10)").unwrap();
    c.execute("INSERT INTO y VALUES(2, 20)").unwrap();
    c
}

#[test]
fn ambiguous_bare_reference_is_rejected() {
    let c = setup();
    // `a` exists in both x and y.
    assert!(c.query("SELECT a FROM x, y").is_err());
    assert!(c.query("SELECT a FROM x JOIN y ON x.a = y.a").is_err());
    assert!(c.query("SELECT 1 FROM x, y WHERE a = 1").is_err());
    assert!(c.query("SELECT 1 FROM x, y ORDER BY a").is_err());
    assert!(c.query("SELECT count(a) FROM x, y").is_err());
    // ...consumed only by GROUP BY (the VDBE grouped path must defer this too).
    assert!(c.query("SELECT a FROM x, y GROUP BY a").is_err());
    // ...consumed only by HAVING.
    assert!(c
        .query("SELECT b, c FROM x, y GROUP BY b, c HAVING a > 0")
        .is_err());
}

#[test]
fn unaliased_self_join_is_ambiguous() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE z(a, b)").unwrap();
    // Same table twice with no alias: even a qualifier or `*` cannot tell the
    // two `z` sources apart.
    assert!(c.query("SELECT a FROM z, z").is_err());
    assert!(c.query("SELECT z.a FROM z, z").is_err());
    assert!(c.query("SELECT * FROM z, z").is_err());
    // Distinct aliases disambiguate every form.
    assert!(c.query("SELECT * FROM z AS p, z AS q").is_ok());
    assert!(c.query("SELECT p.a FROM z AS p, z AS q").is_ok());
    assert!(c.query("SELECT a FROM z AS p, z AS q").is_err());
}

#[test]
fn coalesced_and_qualified_references_are_not_ambiguous() {
    let c = setup();
    // A USING/NATURAL join coalesces the shared column to one logical column.
    assert_eq!(
        c.query("SELECT a FROM x JOIN y USING(a)")
            .unwrap()
            .rows
            .len(),
        0 // no row matches (1 != 2), but the query is valid
    );
    assert!(c.query("SELECT a FROM x NATURAL JOIN y").is_ok());
    // A table/alias qualifier resolves the reference.
    assert_eq!(
        c.query("SELECT x.a FROM x, y ORDER BY x.a").unwrap().rows[0][0],
        Value::Integer(1)
    );
    // A column unique to one side is unambiguous.
    assert_eq!(
        c.query("SELECT b FROM x, y").unwrap().rows[0][0],
        Value::Integer(10)
    );
}

#[test]
fn shared_name_with_distinct_qualifiers_over_a_join_is_fine() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(id, name)").unwrap();
    c.execute("CREATE TABLE b(id, aid, val)").unwrap();
    c.execute("INSERT INTO a VALUES(1, 'x'), (2, 'y')").unwrap();
    c.execute("INSERT INTO b VALUES(10, 1, 100), (11, 2, 200)")
        .unwrap();
    // `id` exists in both, but `*` over distinct tables and qualified refs are OK.
    assert!(c.query("SELECT * FROM a JOIN b ON a.id = b.aid").is_ok());
    assert_eq!(
        c.query("SELECT a.id, b.val FROM a JOIN b ON a.id = b.aid ORDER BY a.id")
            .unwrap()
            .rows
            .len(),
        2
    );
    // Aliased self-join referenced by alias is fine.
    assert!(c
        .query("SELECT t1.id FROM a t1 JOIN a t2 ON t1.id = t2.id")
        .is_ok());
}
