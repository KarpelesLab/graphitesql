//! Roadmap C6b-1 (storage slice): create an empty auto-vacuum database in the
//! pager layer and prove the real `sqlite3` opens it cleanly.
//!
//! These tests use the pager API directly (no SQL surface) to write a brand-new
//! page-1-only database with the auto-vacuum header fields SQLite expects, then
//! shell out to the pinned `sqlite3` oracle to confirm interoperability. The
//! sqlite3-dependent assertions are skipped gracefully when `sqlite3` is absent,
//! following the pattern in `tests/attach.rs`.

#![cfg(feature = "std")]

use graphitesql::pager::{AutoVacuum, WritePager};
use graphitesql::vfs::{OpenFlags, Vfs, std_file::StdVfs};
use std::process::Command;

/// Whether a usable `sqlite3` CLI is on PATH (skip the oracle checks otherwise).
fn have_sqlite3() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `sqlite3 <path> <sql>` and return trimmed stdout, asserting success.
fn sqlite3(path: &str, sql: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(path)
        .arg(sql)
        .output()
        .expect("spawn sqlite3");
    assert!(
        out.status.success(),
        "sqlite3 {sql:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Create an empty database in `mode` at `path` using only the pager layer.
fn create_empty(path: &str, mode: AutoVacuum) {
    let _ = std::fs::remove_file(path);
    let vfs = StdVfs::new();
    let file = vfs.open(path, OpenFlags::READ_WRITE_CREATE).unwrap();
    let journal = vfs
        .open(&format!("{path}-journal"), OpenFlags::READ_WRITE_CREATE)
        .unwrap();
    let mut wp = WritePager::create_auto_vacuum(file, Some(journal), None, 4096, mode).unwrap();
    wp.commit().unwrap();
    // graphite re-reads its own file and reports the mode via the header.
    drop(wp);
    let file = vfs.open(path, OpenFlags::READ_WRITE).unwrap();
    let wp = WritePager::open(file, None).unwrap();
    assert_eq!(wp.auto_vacuum(), mode, "graphite re-read mode mismatch");
}

fn run_mode(mode: AutoVacuum, expected_pragma: &str) {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "graphitesql-av-{:?}-{}.db",
        mode,
        std::process::id()
    ));
    let path = p.to_string_lossy().into_owned();

    create_empty(&path, mode);

    if have_sqlite3() {
        // PRAGMA auto_vacuum reports 0 / 1 / 2.
        assert_eq!(
            sqlite3(&path, "PRAGMA auto_vacuum;"),
            expected_pragma,
            "auto_vacuum pragma for {mode:?}"
        );
        // The file is structurally sound.
        assert_eq!(
            sqlite3(&path, "PRAGMA integrity_check;"),
            "ok",
            "integrity_check for {mode:?}"
        );
        // sqlite can still use the db: create a table, insert, count.
        assert_eq!(
            sqlite3(
                &path,
                "CREATE TABLE t(a); INSERT INTO t VALUES(1); SELECT count(*) FROM t;"
            ),
            "1",
            "round-trip usage for {mode:?}"
        );
    }

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn auto_vacuum_full_db_opens_in_sqlite() {
    run_mode(AutoVacuum::Full, "1");
}

#[test]
fn auto_vacuum_incremental_db_opens_in_sqlite() {
    run_mode(AutoVacuum::Incremental, "2");
}

#[test]
fn auto_vacuum_none_db_opens_in_sqlite() {
    run_mode(AutoVacuum::None, "0");
}
