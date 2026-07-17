//! `PRAGMA wal_checkpoint(mode)` wired through the executor (`exec/mod.rs`), as
//! opposed to the raw pager API exercised by `wal_checkpoint_modes.rs`:
//!
//! * a `PRAGMA wal_checkpoint(TRUNCATE)` run through `Connection::execute` on a
//!   WAL database actually runs the mode-selected checkpoint — it folds the
//!   committed frames into the main file and zeroes the `-wal`, matching what a
//!   pinned `sqlite3 3.50.4` leaves behind;
//! * the default (no-argument) mode is PASSIVE (`pragma.c`), which backfills but
//!   keeps the `-wal` bytes in place;
//! * a non-WAL database reports the `(0, -1, -1)` triple from the read path, and
//!   the argument form is accepted there too.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-wcpe-{}-{name}", std::process::id()));
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

fn wal_size(path: &str) -> u64 {
    std::fs::metadata(format!("{path}-wal"))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// `PRAGMA wal_checkpoint(TRUNCATE)` via `execute` zeroes the `-wal` file and
/// keeps the data readable (`integrity_check` clean, the same as sqlite).
#[test]
fn execute_truncate_checkpoint_zeroes_wal() {
    let path = temp_path("truncate.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("PRAGMA journal_mode = WAL").unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
            .unwrap();
        for i in 1..=30 {
            c.execute(&format!("INSERT INTO t(v) VALUES ({})", i * 3))
                .unwrap();
        }
        assert!(wal_size(&path) > 32, "expected frames in the -wal");

        c.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        assert_eq!(wal_size(&path), 0, "TRUNCATE must zero the -wal file");

        // Data survives the checkpoint.
        assert_eq!(
            c.query("SELECT count(*), sum(v) FROM t").unwrap().rows[0],
            vec![Value::Integer(30), Value::Integer(1395)]
        );
        assert_eq!(
            c.query("PRAGMA integrity_check").unwrap().rows[0][0],
            Value::Text("ok".into())
        );
    }
    // The pinned real sqlite3 agrees the file is intact after our checkpoint.
    if sqlite3_available() {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check; SELECT count(*) FROM t;")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok\n30");
    }
    cleanup(&path);
}

/// The default (no-argument) checkpoint is PASSIVE: it backfills the main file
/// but leaves the `-wal` bytes in place (unlike TRUNCATE).
#[test]
fn execute_default_checkpoint_is_passive() {
    let path = temp_path("passive.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("PRAGMA journal_mode = WAL").unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
            .unwrap();
        for i in 1..=10 {
            c.execute(&format!("INSERT INTO t(v) VALUES ({i})"))
                .unwrap();
        }
        let before = wal_size(&path);
        assert!(before > 32);
        c.execute("PRAGMA wal_checkpoint").unwrap();
        assert_eq!(
            wal_size(&path),
            before,
            "PASSIVE (default) leaves the -wal bytes in place"
        );
        assert_eq!(
            c.query("PRAGMA integrity_check").unwrap().rows[0][0],
            Value::Text("ok".into())
        );
    }
    cleanup(&path);
}

/// On a non-WAL (rollback-journal / in-memory) database the read path reports
/// the fixed `(0, -1, -1)` triple, for both the bare and the argument form —
/// exactly like sqlite's `PRAGMA wal_checkpoint` on a non-WAL database.
#[test]
fn non_wal_query_reports_minus_one() {
    let c = Connection::open_memory().unwrap();
    let r = c.query("PRAGMA wal_checkpoint").unwrap();
    assert_eq!(r.columns, vec!["busy", "log", "checkpointed"]);
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Integer(0),
            Value::Integer(-1),
            Value::Integer(-1)
        ]]
    );
    assert_eq!(
        c.query("PRAGMA wal_checkpoint(TRUNCATE)").unwrap().rows,
        r.rows
    );
}
