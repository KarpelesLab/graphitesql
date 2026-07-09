//! ROADMAP C9c — a **shared wal-index** lets multiple in-process `Connection`s
//! read a WAL-mode database coherently.
//!
//! Two (or more) `Connection`s share one VFS path. In WAL mode the newest
//! committed version of a page may live in the `-wal` file; a coherent
//! page→latest-frame map (the shared wal-index) makes every connection resolve
//! the correct latest committed frame, while a reader inside an open read
//! transaction keeps a stable snapshot across a concurrent writer's commits
//! (repeatable read, matching `wal.c`'s per-reader `mxFrame`).
//!
//! These are multi-connection *semantics* checks over the process-local shared
//! VFS (no `sqlite3` oracle needed): what each reader sees is the property under
//! test. `PRAGMA integrity_check` stays clean throughout.

#![cfg(feature = "std")]

use graphitesql::vfs::memory::MemoryVfs;
use graphitesql::{Connection, Value};

fn scalar_i64(c: &Connection, sql: &str) -> i64 {
    let r = c.query(sql).unwrap();
    match &r.rows[0][0] {
        Value::Integer(i) => *i,
        other => panic!("expected integer, got {other:?}"),
    }
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

/// A WAL-mode db seeded with `t(id INTEGER PRIMARY KEY, v)` holding one row,
/// plus two more connections over the same shared VFS path.
fn wal_three_conns() -> (MemoryVfs, Connection, Connection, Connection) {
    let vfs = MemoryVfs::new();
    {
        let mut c = Connection::create_vfs(&vfs, "db", 4096).unwrap();
        c.execute("PRAGMA journal_mode=WAL").unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
            .unwrap();
        c.execute("INSERT INTO t(v) VALUES (1)").unwrap();
    }
    let a = Connection::open_vfs(&vfs, "db").unwrap();
    let b = Connection::open_vfs(&vfs, "db").unwrap();
    let c = Connection::open_vfs(&vfs, "db").unwrap();
    (vfs, a, b, c)
}

/// B sees A's committed WAL data on B's next statement (the core C9c property:
/// the wal-index is shared, so a sibling's commit is immediately visible).
#[test]
fn sibling_sees_committed_wal_frames() {
    let (_vfs, mut a, b, _c) = wal_three_conns();
    // Both start with the one seeded row.
    assert_eq!(scalar_i64(&a, "SELECT count(*) FROM t"), 1);
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM t"), 1);

    // A appends WAL frames.
    a.execute("INSERT INTO t(v) VALUES (2),(3),(4)").unwrap();

    // B's next statement sees them without reopening.
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM t"), 4);
    assert_eq!(scalar_i64(&b, "SELECT sum(v) FROM t"), 10);
    assert_eq!(
        b.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );

    // A further commit is likewise visible.
    a.execute("INSERT INTO t(v) VALUES (5)").unwrap();
    assert_eq!(scalar_i64(&b, "SELECT sum(v) FROM t"), 15);
}

/// A reader that holds an open read transaction keeps its snapshot while a writer
/// commits more WAL frames (repeatable read = per-reader pinned `mxFrame`).
#[test]
fn open_reader_keeps_snapshot_across_writer_commits() {
    let (_vfs, mut a, mut b, _c) = wal_three_conns();

    // B opens a read transaction and reads: its snapshot is pinned at 1 row.
    b.execute("BEGIN").unwrap();
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM t"), 1);

    // A commits more WAL frames while B's read txn is open.
    a.execute("INSERT INTO t(v) VALUES (2),(3)").unwrap();

    // B still sees its pinned snapshot (1 row) — repeatable read.
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM t"), 1);
    assert_eq!(scalar_i64(&b, "SELECT sum(v) FROM t"), 1);

    // After B ends its transaction, its next statement sees A's writes.
    b.execute("COMMIT").unwrap();
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM t"), 3);
    assert_eq!(scalar_i64(&b, "SELECT sum(v) FROM t"), 6);
}

/// Many pages/frames: a large multi-commit workload from one connection is fully
/// and coherently visible to a sibling, and integrity_check stays clean.
#[test]
fn many_frames_visible_to_sibling() {
    let (_vfs, mut a, b, _c) = wal_three_conns();
    for i in 2..=200 {
        a.execute(&format!("INSERT INTO t(v) VALUES ({i})"))
            .unwrap();
    }
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM t"), 200);
    // sum(1..=200) = 20100
    assert_eq!(scalar_i64(&b, "SELECT sum(v) FROM t"), 20100);
    assert_eq!(
        b.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
    // The single-connection path gives identical results.
    assert_eq!(scalar_i64(&a, "SELECT count(*) FROM t"), 200);
    assert_eq!(scalar_i64(&a, "SELECT sum(v) FROM t"), 20100);
}

/// Checkpoint interaction: after a checkpoint folds the WAL into the main file, a
/// sibling still reads correctly, and further WAL writes remain coherent.
#[test]
fn readers_correct_across_checkpoint() {
    let (_vfs, mut a, b, mut c) = wal_three_conns();
    a.execute("INSERT INTO t(v) VALUES (2),(3),(4)").unwrap();
    assert_eq!(scalar_i64(&b, "SELECT sum(v) FROM t"), 10);

    // C checkpoints (no sibling holds an open read txn, so the WAL resets).
    c.execute("PRAGMA wal_checkpoint").unwrap();

    // B and A still read the checkpointed data correctly from the main file.
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM t"), 4);
    assert_eq!(scalar_i64(&a, "SELECT sum(v) FROM t"), 10);
    assert_eq!(
        b.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );

    // New WAL writes after the checkpoint are visible to siblings again.
    a.execute("INSERT INTO t(v) VALUES (10)").unwrap();
    assert_eq!(scalar_i64(&b, "SELECT sum(v) FROM t"), 20);
    assert_eq!(scalar_i64(&c, "SELECT count(*) FROM t"), 5);
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

/// A pinned reader must not lose its frames to a concurrent checkpoint: the
/// checkpoint still backfills the main file, but the WAL log stays intact so the
/// open reader keeps resolving its snapshot; results are identical to reading
/// without the checkpoint.
#[test]
fn checkpoint_does_not_strand_an_open_reader() {
    let (_vfs, mut a, mut b, mut c) = wal_three_conns();
    a.execute("INSERT INTO t(v) VALUES (2),(3)").unwrap(); // 3 rows, sum 6

    // B pins a snapshot at 3 rows.
    b.execute("BEGIN").unwrap();
    assert_eq!(scalar_i64(&b, "SELECT sum(v) FROM t"), 6);

    // A writes more, then C checkpoints while B's reader is pinned.
    a.execute("INSERT INTO t(v) VALUES (4),(5)").unwrap(); // 5 rows, sum 15
    c.execute("PRAGMA wal_checkpoint").unwrap();

    // B still sees exactly its pinned snapshot.
    assert_eq!(scalar_i64(&b, "SELECT sum(v) FROM t"), 6);
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM t"), 3);
    b.execute("COMMIT").unwrap();

    // After ending its txn, B sees the full current state.
    assert_eq!(scalar_i64(&b, "SELECT sum(v) FROM t"), 15);
    assert_eq!(
        b.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

/// A fresh connection opened *after* a sibling has already committed WAL frames
/// (in the same process, sharing the VFS) adopts the shared index and reads the
/// latest data — it does not have to re-scan or miss frames.
#[test]
fn late_opened_connection_adopts_shared_index() {
    let vfs = MemoryVfs::new();
    {
        let mut c = Connection::create_vfs(&vfs, "db", 4096).unwrap();
        c.execute("PRAGMA journal_mode=WAL").unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
            .unwrap();
    }
    let mut a = Connection::open_vfs(&vfs, "db").unwrap();
    a.execute("INSERT INTO t(v) VALUES (7),(8),(9)").unwrap();

    // Open a brand-new connection now; it must see A's uncheckpointed frames.
    let d = Connection::open_vfs(&vfs, "db").unwrap();
    assert_eq!(ints(&d, "SELECT sum(v) FROM t"), vec![24]);
    assert_eq!(
        d.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}
