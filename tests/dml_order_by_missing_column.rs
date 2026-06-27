//! `UPDATE … ORDER BY <col> LIMIT n` and `DELETE … ORDER BY <col> LIMIT n` (the
//! SQLite update/delete-limit extension) resolve the `ORDER BY` column at
//! prepare time, so a name that is not a column of the target table is `no such
//! column` even over an empty table. graphite validated only the `WHERE` and
//! `SET`-value expressions eagerly, leaving `ORDER BY` to lazy per-row
//! resolution — so a statement that matched no row silently accepted a bogus
//! sort column. `ORDER BY` in these statements takes no alias scope, so it is
//! checked exactly like `WHERE`. Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn err(c: &mut Connection, sql: &str) -> String {
    c.execute(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn missing_order_by_column_in_dml_is_rejected() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    for sql in [
        "UPDATE t SET a = 1 ORDER BY zzz LIMIT 1",
        "DELETE FROM t ORDER BY zzz LIMIT 1",
        "UPDATE t SET a = 1 WHERE b > 0 ORDER BY zzz LIMIT 1",
        "DELETE FROM t WHERE a > 0 ORDER BY zzz LIMIT 1",
    ] {
        assert_eq!(err(&mut c, sql), "no such column: zzz", "for {sql}");
    }
}

#[test]
fn valid_order_by_column_in_dml_still_works() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES (3, 1), (1, 2), (2, 3)")
        .unwrap();
    // A real column (and rowid) sorts fine; the LIMIT bounds the rows touched.
    c.execute("UPDATE t SET a = a + 10 ORDER BY b LIMIT 1")
        .unwrap();
    c.execute("DELETE FROM t ORDER BY a DESC LIMIT 1").unwrap();
    c.execute("DELETE FROM t ORDER BY rowid LIMIT 1").unwrap();
    let n = match c
        .query("SELECT count(*) FROM t")
        .unwrap()
        .rows
        .remove(0)
        .remove(0)
    {
        graphitesql::Value::Integer(n) => n,
        other => panic!("expected integer, got {other:?}"),
    };
    assert_eq!(n, 1);
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
            .trim_start_matches("Error: stepping, ")
            .trim_start_matches("Error: ")
            .trim_start_matches("SQL error: ")
            .trim_start_matches("error: ")
            .trim_end()
            .to_string()
    };
    let setup = "CREATE TABLE t(a, b); INSERT INTO t VALUES (3, 1), (1, 2), (2, 3);";
    for tail in [
        "UPDATE t SET a = 1 ORDER BY zzz LIMIT 1",
        "DELETE FROM t ORDER BY zzz LIMIT 1",
        "UPDATE t SET a = 1 WHERE b > 0 ORDER BY zzz LIMIT 1",
        "DELETE FROM t WHERE a > 0 ORDER BY zzz LIMIT 1",
        "UPDATE t SET a = a + 10 ORDER BY b LIMIT 1",
        "DELETE FROM t ORDER BY a DESC LIMIT 1",
    ] {
        let sql = format!("{setup}{tail}");
        assert_eq!(run("sqlite3", &sql), run(g, &sql), "for {tail}");
    }
}
