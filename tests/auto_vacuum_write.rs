//! Roadmap C6b-2: graphite *writes* auto_vacuum databases, maintaining the
//! pointer-map (ptrmap) pages so the file stays structurally sound.
//!
//! These tests drive graphite via SQL to build an `auto_vacuum=FULL` database on
//! disk — enough rows to allocate many pages and cross at least one ptrmap-page
//! boundary, a large value to force an overflow chain, a secondary index, and
//! some deletes (to exercise frees) — then shell out to the pinned `sqlite3`
//! oracle to confirm `PRAGMA integrity_check` is `ok`, the mode is reported, and
//! the row count matches. The sqlite3 assertions are skipped gracefully when no
//! `sqlite3` is on PATH, following `tests/attach.rs`.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
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

fn tmp_path(tag: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-avw-{}-{}.db", tag, std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
    path
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

/// The headline acceptance test: a FULL auto_vacuum database built entirely by
/// graphite, with overflow, an index, many pages (crossing a ptrmap boundary),
/// and deletes, must pass sqlite3's integrity_check.
#[test]
fn full_auto_vacuum_roundtrips_through_sqlite() {
    let path = tmp_path("full");

    // Each row carries a ~4.5 KiB TEXT value, so every row spills onto an
    // overflow page. At the default 4096-byte page size each ptrmap page covers
    // 819 data pages, so >800 such rows pushes the file well past the first
    // ptrmap boundary (page 2) and across the second (page 822) and third — the
    // file ends up >2400 pages, exercising several ptrmap pages.
    let big: String = "Z".repeat(4500);
    let kept: i64 = {
        let mut c = Connection::create(&path).unwrap();
        c.execute("PRAGMA auto_vacuum=FULL").unwrap();
        assert_eq!(
            c.query("PRAGMA auto_vacuum").unwrap().rows[0][0],
            Value::Integer(1),
            "graphite should report FULL after setting it on an empty db"
        );
        c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
            .unwrap();
        for i in 1..=1000i64 {
            c.execute(&format!("INSERT INTO t VALUES({i}, '{big}')"))
                .unwrap();
        }
        // One extra-large value forcing a multi-page overflow chain.
        let huge: String = "Q".repeat(50_000);
        c.execute(&format!("INSERT INTO t VALUES(100000, '{huge}')"))
            .unwrap();
        // A secondary index over the (overflowing) text column.
        c.execute("CREATE INDEX ix ON t(b)").unwrap();
        // Delete a chunk of rows to exercise page frees mid-file.
        c.execute("DELETE FROM t WHERE a <= 200").unwrap();

        let kept = match c.query("SELECT count(*) FROM t").unwrap().rows[0][0] {
            Value::Integer(n) => n,
            ref v => panic!("count not an integer: {v:?}"),
        };
        // 1000 inserted + 1 huge - 200 deleted = 801.
        assert_eq!(kept, 801);
        kept
    };

    // graphite must re-read its own auto_vacuum file correctly.
    {
        let c = Connection::open(&path).unwrap();
        assert_eq!(
            c.query("PRAGMA auto_vacuum").unwrap().rows[0][0],
            Value::Integer(1)
        );
        assert_eq!(
            c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
            Value::Integer(kept)
        );
        // The big overflow row survives the round-trip with its full value.
        assert_eq!(
            c.query("SELECT length(b) FROM t WHERE a=100000")
                .unwrap()
                .rows[0][0],
            Value::Integer(50_000)
        );
        // graphite's own integrity check passes too.
        assert_eq!(
            c.query("PRAGMA integrity_check").unwrap().rows[0][0],
            Value::Text("ok".into())
        );
    }

    if have_sqlite3() {
        assert_eq!(
            sqlite3(&path, "PRAGMA integrity_check;"),
            "ok",
            "sqlite3 integrity_check on a graphite-written FULL auto_vacuum db"
        );
        assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "1");
        assert_eq!(sqlite3(&path, "SELECT count(*) FROM t;"), kept.to_string());
        // sqlite can read the overflow value and use the secondary index.
        assert_eq!(
            sqlite3(&path, "SELECT length(b) FROM t WHERE a=100000;"),
            "50000"
        );
        assert_eq!(sqlite3(&path, "SELECT count(*) FROM t WHERE b='ZZZ';"), "0");
        assert_eq!(
            sqlite3(
                &path,
                &format!("SELECT a FROM t WHERE b='{big}' AND a=777;")
            ),
            "777"
        );
    }

    cleanup(&path);
}

/// INCREMENTAL mode likewise stays sound through writes (frees keep FREEPAGE
/// ptrmap entries that integrity_check accepts).
#[test]
fn incremental_auto_vacuum_roundtrips_through_sqlite() {
    let path = tmp_path("incr");
    let kept: i64 = {
        let mut c = Connection::create(&path).unwrap();
        c.execute("PRAGMA auto_vacuum=INCREMENTAL").unwrap();
        assert_eq!(
            c.query("PRAGMA auto_vacuum").unwrap().rows[0][0],
            Value::Integer(2)
        );
        c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
            .unwrap();
        for i in 1..=800i64 {
            c.execute(&format!("INSERT INTO t VALUES({i}, 'row-{i}')"))
                .unwrap();
        }
        c.execute("CREATE INDEX ix ON t(b)").unwrap();
        c.execute("DELETE FROM t WHERE a % 5 = 0").unwrap();
        match c.query("SELECT count(*) FROM t").unwrap().rows[0][0] {
            Value::Integer(n) => n,
            ref v => panic!("count: {v:?}"),
        }
    };
    if have_sqlite3() {
        assert_eq!(sqlite3(&path, "PRAGMA integrity_check;"), "ok");
        assert_eq!(sqlite3(&path, "PRAGMA auto_vacuum;"), "2");
        assert_eq!(sqlite3(&path, "SELECT count(*) FROM t;"), kept.to_string());
    }
    cleanup(&path);
}

/// Setting auto_vacuum on a *non-empty* database is a no-op (matches sqlite),
/// and a default (NONE) database is never silently switched.
#[test]
fn auto_vacuum_pragma_is_noop_on_nonempty_db() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    // Now non-empty: requesting FULL does not change the mode.
    c.execute("PRAGMA auto_vacuum=FULL").unwrap();
    assert_eq!(
        c.query("PRAGMA auto_vacuum").unwrap().rows[0][0],
        Value::Integer(0)
    );
    // Still writable as a plain NONE database.
    c.execute("INSERT INTO t VALUES(2)").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(2)
    );
}
