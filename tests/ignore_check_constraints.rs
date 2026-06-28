//! `PRAGMA ignore_check_constraints = ON` makes INSERT/UPDATE skip CHECK
//! enforcement (column-level, table-level, and named CONSTRAINT … CHECK alike),
//! so a row that would violate a CHECK is stored unchanged. NOT NULL, UNIQUE, and
//! foreign keys are unaffected — those are enforced independently. Turning the
//! pragma back off re-enforces CHECK, and the get form reads the live flag.
//! graphite previously parsed the pragma but always enforced CHECK. Verified
//! against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn col0(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}

#[test]
fn skips_check_on_insert_and_update() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a CHECK(a > 0), b, CHECK(b < 10))")
        .unwrap();
    c.execute("PRAGMA ignore_check_constraints = ON").unwrap();
    // Both the column-level and table-level CHECK are ignored.
    c.execute("INSERT INTO t VALUES (-5, 99)").unwrap();
    assert_eq!(col0(&c, "SELECT a FROM t"), Value::Integer(-5));
    // UPDATE into a violating value is allowed too.
    c.execute("UPDATE t SET a = -100").unwrap();
    assert_eq!(col0(&c, "SELECT a FROM t"), Value::Integer(-100));
}

#[test]
fn does_not_affect_not_null_or_unique() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a NOT NULL, b UNIQUE)").unwrap();
    c.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    c.execute("PRAGMA ignore_check_constraints = 1").unwrap();
    // Constraint errors render bare (no `error: ` framing).
    assert_eq!(
        c.execute("INSERT INTO t VALUES (NULL, 2)")
            .unwrap_err()
            .to_string(),
        "NOT NULL constraint failed: t.a"
    );
    assert_eq!(
        c.execute("INSERT INTO t VALUES (3, 1)")
            .unwrap_err()
            .to_string(),
        "UNIQUE constraint failed: t.b"
    );
}

#[test]
fn toggling_off_re_enforces_and_get_form_reflects_state() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a CHECK(a > 0))").unwrap();
    assert_eq!(
        col0(&c, "PRAGMA ignore_check_constraints"),
        Value::Integer(0)
    );
    c.execute("PRAGMA ignore_check_constraints = 1").unwrap();
    assert_eq!(
        col0(&c, "PRAGMA ignore_check_constraints"),
        Value::Integer(1)
    );
    c.execute("INSERT INTO t VALUES (-1)").unwrap();
    // Back off: CHECK is enforced again.
    c.execute("PRAGMA ignore_check_constraints = 0").unwrap();
    assert_eq!(
        col0(&c, "PRAGMA ignore_check_constraints"),
        Value::Integer(0)
    );
    assert_eq!(
        c.execute("INSERT INTO t VALUES (-2)")
            .unwrap_err()
            .to_string(),
        "CHECK constraint failed: a > 0"
    );
}

#[test]
fn matches_sqlite_cli() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    };
    for sql in [
        "CREATE TABLE t(a CHECK(a>0)); PRAGMA ignore_check_constraints=ON; INSERT INTO t VALUES(-5); SELECT a FROM t;",
        "CREATE TABLE t(a CHECK(a>0)); INSERT INTO t VALUES(5); PRAGMA ignore_check_constraints=1; UPDATE t SET a=-1; SELECT a FROM t;",
        "CREATE TABLE t(a,b,CHECK(a<b)); PRAGMA ignore_check_constraints=1; INSERT INTO t VALUES(9,1); SELECT * FROM t;",
        "CREATE TABLE t(a, CONSTRAINT pos CHECK(a>0)); PRAGMA ignore_check_constraints=1; INSERT INTO t VALUES(-1); SELECT a FROM t;",
        "PRAGMA ignore_check_constraints;",
        "PRAGMA ignore_check_constraints=1; PRAGMA ignore_check_constraints;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
