//! `random()` and `randomblob(N)`. Both are non-deterministic, so the tests
//! assert structural properties (type, length, range, per-row variation) rather
//! than exact values, and check the determinism guard that SQLite enforces in
//! index expressions and generated columns. Behaviour verified against the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn val(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}

#[test]
fn random_returns_an_integer_that_varies_per_row() {
    let mut c = Connection::open_memory().unwrap();
    assert!(matches!(val(&c, "SELECT random()"), Value::Integer(_)));
    assert!(matches!(
        val(&c, "SELECT typeof(random())"),
        Value::Text(t) if t == "integer"
    ));

    // Across many rows, random() takes many distinct values (it is evaluated
    // per row, not folded to a constant).
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10)")
        .unwrap();
    assert_eq!(
        val(&c, "SELECT count(DISTINCT random()) FROM t"),
        Value::Integer(10)
    );
    // Two calls in one row also differ.
    assert_eq!(val(&c, "SELECT random()=random()"), Value::Integer(0));
}

#[test]
fn randomblob_length_and_clamping() {
    let c = Connection::open_memory().unwrap();
    assert!(matches!(
        val(&c, "SELECT typeof(randomblob(8))"),
        Value::Text(t) if t == "blob"
    ));
    assert_eq!(val(&c, "SELECT length(randomblob(16))"), Value::Integer(16));
    // N < 1 clamps to a single byte; non-numeric / NULL arguments coerce to 0
    // (then clamp), so they too yield one byte — not NULL.
    assert_eq!(val(&c, "SELECT length(randomblob(0))"), Value::Integer(1));
    assert_eq!(val(&c, "SELECT length(randomblob(-5))"), Value::Integer(1));
    assert_eq!(
        val(&c, "SELECT length(randomblob(NULL))"),
        Value::Integer(1)
    );
    assert_eq!(
        val(&c, "SELECT length(randomblob('abc'))"),
        Value::Integer(1)
    );
    // A real argument truncates toward zero.
    assert_eq!(val(&c, "SELECT length(randomblob(3.9))"), Value::Integer(3));
    // Past SQLITE_MAX_LENGTH (1e9) both randomblob and zeroblob error rather
    // than attempting a multi-gigabyte allocation.
    assert!(c.query("SELECT randomblob(2000000000)").is_err());
    assert!(c.query("SELECT zeroblob(2000000000)").is_err());
    assert_eq!(
        val(&c, "SELECT length(zeroblob(1000000000))"),
        Value::Integer(1_000_000_000)
    );
}

#[test]
fn nondeterministic_functions_are_rejected_where_sqlite_rejects_them() {
    let mut c = Connection::open_memory().unwrap();
    // Generated columns.
    assert!(c.execute("CREATE TABLE g(a, b AS (random()))").is_err());
    assert!(
        c.execute("CREATE TABLE g(a, b AS (randomblob(4)))")
            .is_err()
    );
    // Index expressions and partial-index predicates.
    c.execute("CREATE TABLE t(a)").unwrap();
    assert!(c.execute("CREATE INDEX i1 ON t(random())").is_err());
    assert!(
        c.execute("CREATE INDEX i2 ON t(a) WHERE random() > 0")
            .is_err()
    );
    // A deterministic expression index is still fine.
    c.execute("CREATE INDEX i3 ON t(a + 1)").unwrap();
}
