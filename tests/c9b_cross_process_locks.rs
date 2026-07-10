//! C9b — OS-level cross-process file locks (Rust 1.89 `std::fs::File::lock`).
//!
//! The `StdVfs` now drives one process-wide OS advisory lock off its per-path
//! aggregate lock state (`src/vfs/std_file.rs`): the process holds an OS *shared*
//! lock while only readers are active and an OS *exclusive* lock for any write
//! intent. That makes two separate OS processes over the same database file
//! serialize their writes, instead of the previous process-local-only coordination.
//!
//! This test holds an OS lock in the test process itself (via the same
//! `std::fs::File` primitive graphite uses) and spawns real `graphitesql` child
//! processes, asserting they see `SQLITE_BUSY` ("database is locked") exactly when
//! SQLite would. Unix-guarded: advisory `flock`-style semantics; on Windows the
//! std locks are mandatory and the timing model differs.

#![cfg(all(feature = "std", unix))]

use std::fs::File;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_graphitesql")
}

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-c9b-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

/// Run one SQL string through a fresh `graphitesql` process; return
/// `(success, combined stdout+stderr)`.
fn run(db: &str, sql: &str) -> (bool, String) {
    let out = Command::new(bin()).arg(db).arg(sql).output().unwrap();
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

fn cleanup(db: &str) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db}{suffix}"));
    }
}

fn is_locked(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("locked") || m.contains("busy")
}

#[test]
fn foreign_exclusive_lock_blocks_a_writer() {
    let db = temp_path("exwrite");
    cleanup(&db);
    let (ok, _) = run(&db, "CREATE TABLE t(a); INSERT INTO t VALUES(1)");
    assert!(ok, "setup failed");

    // Another process (this test) holds an exclusive OS lock on the file.
    let lockf = File::options().read(true).write(true).open(&db).unwrap();
    lockf.lock().unwrap();

    let (ok, msg) = run(&db, "INSERT INTO t VALUES(2)");
    assert!(
        !ok,
        "a write must fail while a foreign exclusive lock is held"
    );
    assert!(is_locked(&msg), "expected a locked/busy error, got: {msg}");

    // Releasing the foreign lock lets the write through.
    lockf.unlock().unwrap();
    let (ok, msg) = run(&db, "INSERT INTO t VALUES(2)");
    assert!(ok, "write must succeed once the lock is released: {msg}");

    let (ok, msg) = run(&db, "SELECT count(*) FROM t");
    assert!(ok && msg.trim() == "2", "expected 2 rows, got: {msg}");
    cleanup(&db);
}

#[test]
fn foreign_shared_lock_blocks_a_writer_but_not_a_reader() {
    let db = temp_path("shwrite");
    cleanup(&db);
    let (ok, _) = run(&db, "CREATE TABLE t(a); INSERT INTO t VALUES(1)");
    assert!(ok, "setup failed");

    // A foreign *shared* (reader) lock: writers must wait, readers coexist.
    let lockf = File::options().read(true).write(true).open(&db).unwrap();
    lockf.lock_shared().unwrap();

    let (ok, msg) = run(&db, "INSERT INTO t VALUES(2)");
    assert!(!ok, "a write must fail while a foreign shared lock is held");
    assert!(is_locked(&msg), "expected a locked/busy error, got: {msg}");

    // A concurrent reader coexists with the foreign shared lock.
    let (ok, msg) = run(&db, "SELECT count(*) FROM t");
    assert!(
        ok,
        "a read must succeed alongside a foreign shared lock: {msg}"
    );
    assert_eq!(msg.trim(), "1", "reader saw the pre-write state");

    lockf.unlock().unwrap();
    cleanup(&db);
}

#[test]
fn foreign_exclusive_lock_blocks_an_autocommit_reader() {
    // C9b-3: a bare autocommit SELECT takes a transient shared lock for the read, so
    // a foreign process mid-write (holding the exclusive lock) can't be read torn.
    let db = temp_path("acread");
    cleanup(&db);
    let (ok, _) = run(&db, "CREATE TABLE t(a); INSERT INTO t VALUES(1)");
    assert!(ok, "setup failed");

    let lockf = File::options().read(true).write(true).open(&db).unwrap();
    lockf.lock().unwrap();

    // No BEGIN — a plain autocommit read must still be blocked by the exclusive lock.
    let (ok, msg) = run(&db, "SELECT count(*) FROM t");
    assert!(
        !ok && is_locked(&msg),
        "an autocommit read must be blocked by a foreign exclusive lock, got ok={ok} msg={msg}"
    );

    // Once released, the read succeeds.
    lockf.unlock().unwrap();
    let (ok, msg) = run(&db, "SELECT count(*) FROM t");
    assert!(
        ok && msg.trim() == "1",
        "read must succeed after unlock: {msg}"
    );
    cleanup(&db);
}

#[test]
fn foreign_exclusive_lock_blocks_an_explicit_read_transaction() {
    let db = temp_path("exread");
    cleanup(&db);
    let (ok, _) = run(&db, "CREATE TABLE t(a); INSERT INTO t VALUES(1)");
    assert!(ok, "setup failed");

    let lockf = File::options().read(true).write(true).open(&db).unwrap();
    lockf.lock().unwrap();

    // An explicit read transaction takes a persistent shared lock (C9a), which the
    // foreign exclusive lock blocks.
    let out = Command::new(bin())
        .arg(&db)
        .arg("BEGIN; SELECT count(*) FROM t; COMMIT")
        .output()
        .unwrap();
    let msg =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);
    assert!(
        is_locked(&msg),
        "an explicit read txn must be blocked by a foreign exclusive lock, got: {msg}"
    );

    lockf.unlock().unwrap();
    cleanup(&db);
}
