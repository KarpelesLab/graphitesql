//! Qualified (`table.column`) references inside DDL expressions. SQLite resolves
//! a `table.col` that names the object being defined, but a generated-column or
//! index *key* expression then rejects the dotted form (`the "." operator
//! prohibited in generated columns` / `… in index expressions`) even though the
//! bare column resolves, while a CHECK constraint and a partial-index `WHERE`
//! accept the same dotted reference. Any other qualifier (or a correctly-
//! qualified but unknown column) is reported as `no such column: q.c`. graphite
//! previously matched only on the bare column name (or silently accepted the
//! dotted form). Matched to the `sqlite3` CLI (3.50.4).

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn generated_column_rejects_dotted_self_reference() {
    let mut c = Connection::open_memory().unwrap();
    assert!(
        c.execute("CREATE TABLE t(a, b AS (t.a))")
            .unwrap_err()
            .to_string()
            .contains("the \".\" operator prohibited in generated columns")
    );
    // Nested inside a function call still counts as a resolved dotted reference.
    let mut c = Connection::open_memory().unwrap();
    assert!(
        c.execute("CREATE TABLE t(a, b AS (abs(t.a)))")
            .unwrap_err()
            .to_string()
            .contains("the \".\" operator prohibited in generated columns")
    );
    // A wrong qualifier / unknown column is a "no such column", reported qualified.
    let mut c = Connection::open_memory().unwrap();
    assert!(
        c.execute("CREATE TABLE t(a, b AS (other.a))")
            .unwrap_err()
            .to_string()
            .contains("no such column: other.a")
    );
    let mut c = Connection::open_memory().unwrap();
    assert!(
        c.execute("CREATE TABLE t(a, b AS (t.nope))")
            .unwrap_err()
            .to_string()
            .contains("no such column: t.nope")
    );
    // A bare resolved reference is fine.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b AS (a + 1))").unwrap();
}

#[test]
fn check_accepts_dotted_self_reference() {
    let mut c = Connection::open_memory().unwrap();
    // CHECK tolerates the dotted form that a generated column rejects.
    c.execute("CREATE TABLE t(a, b, CHECK(t.a > 0))").unwrap();
    // But a foreign qualifier is still an unknown column.
    let mut c = Connection::open_memory().unwrap();
    assert!(
        c.execute("CREATE TABLE t(a, CHECK(other.a > 0))")
            .unwrap_err()
            .to_string()
            .contains("no such column: other.a")
    );
}

#[test]
fn index_key_rejects_dotted_self_reference() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    assert!(
        c.execute("CREATE INDEX i ON t(t.a)")
            .unwrap_err()
            .to_string()
            .contains("the \".\" operator prohibited in index expressions")
    );
    assert!(
        c.execute("CREATE INDEX i ON t(abs(t.a))")
            .unwrap_err()
            .to_string()
            .contains("the \".\" operator prohibited in index expressions")
    );
    // Foreign / unknown qualifier → no such column (qualified).
    assert!(
        c.execute("CREATE INDEX i ON t(other.a)")
            .unwrap_err()
            .to_string()
            .contains("no such column: other.a")
    );
    assert!(
        c.execute("CREATE INDEX i ON t(t.nope)")
            .unwrap_err()
            .to_string()
            .contains("no such column: t.nope")
    );
    // A legitimate expression index still builds.
    c.execute("CREATE INDEX ok ON t(abs(a), b)").unwrap();
}

#[test]
fn partial_where_accepts_dotted_self_reference() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    // The predicate tolerates the dotted form, like a CHECK.
    c.execute("CREATE INDEX i ON t(a) WHERE t.b > 0").unwrap();
    assert!(
        c.execute("CREATE INDEX j ON t(a) WHERE other.b > 0")
            .unwrap_err()
            .to_string()
            .contains("no such column: other.b")
    );
    assert!(
        c.execute("CREATE INDEX k ON t(a) WHERE t.nope > 0")
            .unwrap_err()
            .to_string()
            .contains("no such column: t.nope")
    );
}

#[test]
fn matches_sqlite_cli() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let g = env!("CARGO_BIN_EXE_graphitesql");
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !stdout.trim().is_empty() {
            return stdout.trim_end().to_string();
        }
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .find(|l| !l.trim_start().starts_with('^'))
            .unwrap_or("")
            .trim_start_matches("Error: in prepare, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("error: ")
            .to_string()
    };
    for sql in [
        // generated columns
        "CREATE TABLE t(a, b AS (t.a))",
        "CREATE TABLE t(a, b AS (abs(t.a)))",
        "CREATE TABLE t(a, b AS (other.a))",
        "CREATE TABLE t(a, b AS (t.nope))",
        "CREATE TABLE t(a, b AS (a))",
        // CHECK
        "CREATE TABLE t(a, b, CHECK(t.a > 0))",
        "CREATE TABLE t(a, CHECK(other.a > 0))",
        "CREATE TABLE t(a, CHECK(t.nope > 0))",
        // index key expressions
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(t.a)",
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(abs(t.a))",
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(other.a)",
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(t.nope)",
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(rowid)",
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(a+b)",
        // partial WHERE
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(a) WHERE t.b > 0",
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(a) WHERE other.b > 0",
        "CREATE TABLE t(a,b); CREATE INDEX i ON t(a) WHERE t.nope > 0",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
