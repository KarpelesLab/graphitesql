//! Phase 9: the WAL *write* path (`PRAGMA journal_mode=WAL`).
//!
//! graphitesql appends committed pages as WAL frames to the `-wal` file. The
//! decisive gates: the real `sqlite3` CLI reads a graphitesql-written WAL
//! database (both before and after checkpoint) and `integrity_check` passes,
//! and graphitesql reads its own WAL writes back (including after reopen).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-walw-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

fn cleanup(path: &str) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{path}{suffix}"));
    }
}

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite3_run(path: &str, sql: &str) -> String {
    let out = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn ints(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            ref o => panic!("not int: {o:?}"),
        })
        .collect()
}

#[test]
fn journal_mode_pragma_reports_wal() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_path("mode.db");
    cleanup(&path);
    let mut c = Connection::create(&path).unwrap();
    assert_eq!(
        c.query("PRAGMA journal_mode").unwrap().rows[0][0],
        Value::Text("delete".into())
    );
    c.execute("PRAGMA journal_mode = WAL").unwrap();
    assert_eq!(
        c.query("PRAGMA journal_mode").unwrap().rows[0][0],
        Value::Text("wal".into())
    );
    drop(c);
    cleanup(&path);
}

#[test]
fn sqlite_reads_uncheckpointed_wal() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_path("uncp.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("PRAGMA journal_mode = WAL").unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
            .unwrap();
        for i in 1..=50 {
            c.execute(&format!("INSERT INTO t(v) VALUES ({})", i * 2))
                .unwrap();
        }
        c.execute("DELETE FROM t WHERE id % 4 = 0").unwrap();
        // No checkpoint: the data lives only in the -wal file.
    }
    // A -wal file must exist with content.
    assert!(
        std::fs::metadata(format!("{path}-wal"))
            .map(|m| m.len() > 32)
            .unwrap_or(false),
        "expected a non-empty -wal file"
    );
    // The real sqlite3 reads the uncheckpointed WAL database.
    assert_eq!(sqlite3_run(&path, "PRAGMA integrity_check;"), "ok");
    let want = sqlite3_run(&path, "SELECT count(*), sum(v) FROM t;");
    let got = {
        let c = Connection::open_readonly(&path).unwrap();
        let r = c.query("SELECT count(*), sum(v) FROM t").unwrap();
        format!(
            "{}|{}",
            match &r.rows[0][0] {
                Value::Integer(i) => *i,
                _ => panic!(),
            },
            match &r.rows[0][1] {
                Value::Integer(i) => *i,
                _ => panic!(),
            },
        )
    };
    assert_eq!(got, want);
    cleanup(&path);
}

#[test]
fn checkpoint_then_sqlite_reads_main() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_path("cp.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("PRAGMA journal_mode = WAL").unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, s TEXT)")
            .unwrap();
        for i in 1..=20 {
            c.execute(&format!("INSERT INTO t(s) VALUES ('row{i}')"))
                .unwrap();
        }
        c.execute("PRAGMA wal_checkpoint").unwrap();
    }
    // After checkpoint the main file holds the data; sqlite reads it.
    assert_eq!(sqlite3_run(&path, "PRAGMA integrity_check;"), "ok");
    assert_eq!(sqlite3_run(&path, "SELECT count(*) FROM t;"), "20");
    cleanup(&path);
}

#[test]
fn reopen_reads_wal() {
    let path = temp_path("reopen.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("PRAGMA journal_mode = WAL").unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
            .unwrap();
        c.execute("INSERT INTO t(v) VALUES (10),(20),(30)").unwrap();
        // Close without checkpoint.
    }
    // Reopen: graphitesql must load the -wal and see the rows.
    let mut c = Connection::open(&path).unwrap();
    assert_eq!(ints(&c, "SELECT count(*) FROM t"), vec![3]);
    assert_eq!(ints(&c, "SELECT sum(v) FROM t"), vec![60]);
    // And further writes (still WAL mode) work and persist.
    c.execute("INSERT INTO t(v) VALUES (40)").unwrap();
    assert_eq!(ints(&c, "SELECT sum(v) FROM t"), vec![100]);
    cleanup(&path);
}

#[test]
fn graphite_reads_sqlite_written_wal_after_our_open() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // sqlite writes a WAL database; graphitesql reads it read-only.
    let path = temp_path("sqlitewal.db");
    cleanup(&path);
    sqlite3_run(
        &path,
        "PRAGMA journal_mode=WAL; CREATE TABLE t(id INTEGER PRIMARY KEY, v INT); \
         INSERT INTO t(v) VALUES (1),(2),(3),(4);",
    );
    let c = Connection::open_readonly(&path).unwrap();
    assert_eq!(ints(&c, "SELECT sum(v) FROM t"), vec![10]);
    cleanup(&path);
}
