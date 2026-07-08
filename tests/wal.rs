//! Phase 8: read a real WAL-mode database.
//!
//! `tests/fixtures/wal.db` is a database whose table and rows exist *only* in
//! the uncheckpointed `tests/fixtures/wal.db-wal` companion (produced by the
//! real SQLite via a live connection snapshot). Reading it correctly requires
//! parsing and overlaying the WAL — the main file alone has no `t` table.

#![cfg(feature = "std")]

use graphitesql::pager::{PageSource, WalReader};
use graphitesql::vfs::{OpenFlags, Vfs, std_file::StdVfs};
use graphitesql::{Connection, Value};

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn wal_reader_overlays_frames() {
    // Low-level: the WAL must contribute the schema page and the table pages.
    let vfs = StdVfs::new();
    let main = vfs.open(&fixture("wal.db"), OpenFlags::READ_ONLY).unwrap();
    let mut wal = vfs
        .open(&fixture("wal.db-wal"), OpenFlags::READ_ONLY)
        .unwrap();
    let reader = WalReader::open(main, wal.as_mut()).unwrap();
    // The committed snapshot grew the database beyond the 1-page main file.
    assert!(reader.page_count() >= 2, "wal should extend the db");
    assert_eq!(reader.header().page_size, 4096);
}

#[test]
fn query_wal_mode_database() {
    // High-level: open read-only; the WAL is detected and overlaid automatically.
    let c = Connection::open_readonly(&fixture("wal.db")).unwrap();

    // The table only exists via the WAL.
    assert!(c.schema().table("t").is_some(), "table t must be visible");

    let r = c.query("SELECT a, b, c FROM t ORDER BY a").unwrap();
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[0][0], Value::Integer(1));
    assert_eq!(r.rows[0][1], Value::Text("one".into()));
    assert_eq!(r.rows[2][1], Value::Text("three".into()));

    let agg = c.query("SELECT count(*), sum(a) FROM t").unwrap();
    assert_eq!(agg.rows[0][0], Value::Integer(3));
    assert_eq!(agg.rows[0][1], Value::Integer(6));
}
