//! `PRAGMA query_only = ON` puts the connection in read-only mode: any statement
//! that would open a write transaction — INSERT/UPDATE/DELETE, every
//! CREATE/DROP/ALTER, VACUUM, and ANALYZE (which writes `sqlite_stat1`) — fails
//! with `attempt to write a readonly database`, while SELECT, PRAGMA,
//! ATTACH/DETACH, and read-only transaction/savepoint control pass through.
//! Turning the pragma back off restores writes. graphite previously parsed the
//! pragma but ignored it, so writes went through unchecked. Verified against the
//! sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

const READONLY: &str = "attempt to write a readonly database";

/// Library-level error text for a statement, with the `error: ` framing stripped
/// so it reads as SQLite's bare `errmsg` (what the CLI prints after its prefix).
fn err(c: &mut Connection, sql: &str) -> String {
    let msg = if sql.trim_start().to_ascii_uppercase().starts_with("SELECT") {
        c.query(sql).unwrap_err().to_string()
    } else {
        c.execute(sql).unwrap_err().to_string()
    };
    msg.strip_prefix("error: ").unwrap_or(&msg).to_string()
}

#[test]
fn blocks_every_write_statement() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES (1)").unwrap();
    c.execute("PRAGMA query_only = ON").unwrap();
    for sql in [
        "INSERT INTO t VALUES (2)",
        "UPDATE t SET a = 9",
        "DELETE FROM t",
        "CREATE TABLE u(a)",
        "CREATE INDEX i ON t(a)",
        "CREATE VIEW v AS SELECT * FROM t",
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END",
        "DROP TABLE t",
        "ALTER TABLE t ADD COLUMN b",
        "ALTER TABLE t RENAME TO t2",
        "VACUUM",
        "ANALYZE",
        // A write to a TEMP table is blocked too (query_only covers every schema).
        "CREATE TEMP TABLE tmp(a)",
    ] {
        assert_eq!(err(&mut c, sql), READONLY, "for {sql}");
    }
    // The table is untouched — the blocked writes never ran.
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(1)
    );
}

#[test]
fn reads_and_readonly_control_still_work() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES (1), (2)").unwrap();
    c.execute("PRAGMA query_only = 1").unwrap();
    // SELECT, a read-only transaction, ATTACH, and SAVEPOINT control are allowed.
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(2)
    );
    c.execute("BEGIN").unwrap();
    assert_eq!(
        c.query("SELECT a FROM t ORDER BY a").unwrap().rows[0][0],
        Value::Integer(1)
    );
    c.execute("COMMIT").unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("SAVEPOINT s1").unwrap();
    c.execute("RELEASE s1").unwrap();
}

#[test]
fn toggling_off_restores_writes_and_get_form_reflects_state() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    // Default off → get form reads 0.
    assert_eq!(
        c.query("PRAGMA query_only").unwrap().rows[0][0],
        Value::Integer(0)
    );
    c.execute("PRAGMA query_only = ON").unwrap();
    assert_eq!(
        c.query("PRAGMA query_only").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(err(&mut c, "INSERT INTO t VALUES (1)"), READONLY);
    // Back off: writes go through again.
    c.execute("PRAGMA query_only = OFF").unwrap();
    assert_eq!(
        c.query("PRAGMA query_only").unwrap().rows[0][0],
        Value::Integer(0)
    );
    c.execute("INSERT INTO t VALUES (1)").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(1)
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
    // Strip each CLI's error framing down to the bare message: graphite prints
    // `error: <msg>`, the stock CLI `Runtime error near line N: <msg>` (or
    // `stepping, <msg>`), so a normalized comparison isolates the message text.
    let norm = |out: &str| -> String {
        out.trim_end()
            .lines()
            .map(|line| {
                let line = line.trim();
                for p in ["Error: in prepare, ", "Error: ", "error: ", "stepping, "] {
                    if let Some(r) = line.strip_prefix(p) {
                        return r.to_string();
                    }
                }
                if let Some(rest) = line.strip_prefix("Runtime error near line ")
                    && let Some(idx) = rest.find(": ")
                {
                    return rest[idx + 2..].to_string();
                }
                line.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let run = |bin: &str, sql: &str| -> String {
        let out = Command::new(bin).arg(":memory:").arg(sql).output().unwrap();
        norm(&String::from_utf8_lossy(&out.stdout))
    };
    for sql in [
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); PRAGMA query_only=ON; INSERT INTO t VALUES(2);",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); PRAGMA query_only=ON; UPDATE t SET a=2;",
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); PRAGMA query_only=ON; DELETE FROM t;",
        "PRAGMA query_only=ON; CREATE TABLE t(a);",
        "CREATE TABLE t(a); PRAGMA query_only=ON; DROP TABLE t;",
        "CREATE TABLE t(a); PRAGMA query_only=ON; CREATE INDEX i ON t(a);",
        "CREATE TABLE t(a); PRAGMA query_only=ON; ALTER TABLE t ADD COLUMN b;",
        "PRAGMA query_only=ON; VACUUM;",
        "CREATE TABLE t(a); PRAGMA query_only=ON; ANALYZE;",
        // Reads and the get form pass through.
        "CREATE TABLE t(a); INSERT INTO t VALUES(1); PRAGMA query_only=ON; SELECT a FROM t;",
        "CREATE TABLE t(a); PRAGMA query_only=ON; PRAGMA query_only=OFF; INSERT INTO t VALUES(1); SELECT a FROM t;",
        "PRAGMA query_only=1; PRAGMA query_only;",
        "PRAGMA query_only;",
    ] {
        assert_eq!(run("sqlite3", sql), run(g, sql), "for {sql}");
    }
}
