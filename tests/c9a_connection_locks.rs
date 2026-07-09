//! Track C — C9a wired at the `Connection` layer: an open read transaction
//! blocks a concurrent writer's commit, with SQLite's **DEFERRED** semantics.
//!
//! The pager primitive (`WritePager::begin_read_txn`/`end_read_txn`) is exercised
//! directly in `tests/pager_read_locks.rs`. Here we drive it through real SQL —
//! `BEGIN` / `SELECT` / `INSERT` / `COMMIT` on two `Connection`s that share one
//! `MemoryVfs` path (two pagers over one `LockState`, exactly as two connections
//! in one process). The semantics asserted, matching `sqlite3 3.50.4`:
//!
//!   * two connections may each hold an open read transaction at once;
//!   * while connection A holds an open read transaction (`BEGIN; SELECT`),
//!     connection B's write **commit** returns `SQLITE_BUSY`; after A `COMMIT`s,
//!     B's commit succeeds;
//!   * **DEFERRED (critical):** `BEGIN` alone takes no lock — the read lock is
//!     acquired lazily at the *first read* inside the transaction. Two bare
//!     `BEGIN`s (no read yet) do not block a writer; if the lock were taken at
//!     `BEGIN` this would wrongly BUSY. This test guards that regression.
//!   * autocommit `SELECT`s (no open transaction) never block a writer.
//!
//! This is a graphite-vs-SQLite *semantics* check (process-local locking, no CLI
//! oracle needed): the behaviors above are what `pager.c`'s DEFERRED transaction
//! model produces.

#![cfg(feature = "std")]

use graphitesql::vfs::memory::MemoryVfs;
use graphitesql::{Connection, Error, Value};

/// The single-column integer scalar of a one-row, one-column result.
fn scalar_i64(c: &Connection, sql: &str) -> i64 {
    let r = c.query(sql).unwrap();
    match &r.rows[0][0] {
        Value::Integer(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

/// Two connections over one shared in-memory VFS path, with a seeded `t(x)`
/// table holding one row. The database is in the default rollback-journal mode
/// (not WAL), so a writer's commit upgrades to `EXCLUSIVE` and a held reader
/// makes it BUSY — the locking behavior under test.
fn two_conns() -> (MemoryVfs, Connection, Connection) {
    let vfs = MemoryVfs::new();
    {
        let mut c = Connection::create_vfs(&vfs, "db", 4096).unwrap();
        c.execute("CREATE TABLE t(x)").unwrap();
        c.execute("INSERT INTO t VALUES (1)").unwrap();
    }
    let a = Connection::open_vfs(&vfs, "db").unwrap();
    let b = Connection::open_vfs(&vfs, "db").unwrap();
    (vfs, a, b)
}

/// Two connections can each hold an open read transaction at once, and both read
/// the committed data (SHARED is a counted lock; readers never block readers).
#[test]
fn two_open_read_txns_coexist() {
    let (_vfs, mut a, mut b) = two_conns();

    a.execute("BEGIN").unwrap();
    assert_eq!(a.query("SELECT x FROM t").unwrap().rows.len(), 1);

    b.execute("BEGIN").unwrap();
    assert_eq!(b.query("SELECT x FROM t").unwrap().rows.len(), 1);

    // Both readers still see the row while the other's read txn is open.
    assert_eq!(scalar_i64(&a, "SELECT x FROM t"), 1);
    assert_eq!(scalar_i64(&b, "SELECT x FROM t"), 1);

    a.execute("COMMIT").unwrap();
    b.execute("COMMIT").unwrap();
}

/// While A holds an open read transaction (`BEGIN; SELECT`), B's write commit
/// BUSYs; after A `COMMIT`s (releasing its persistent SHARED lock), B succeeds.
#[test]
fn open_reader_blocks_a_writers_commit_until_it_commits() {
    let (_vfs, mut a, mut b) = two_conns();

    // A opens a read transaction and actually reads: the persistent SHARED lock
    // is now held (DEFERRED — taken at the first read, not at BEGIN).
    a.execute("BEGIN").unwrap();
    assert_eq!(a.query("SELECT x FROM t").unwrap().rows.len(), 1);

    // B stages a write and tries to commit. The commit upgrades to EXCLUSIVE,
    // which A's SHARED lock blocks: BUSY.
    b.execute("BEGIN").unwrap();
    b.execute("INSERT INTO t VALUES (2)").unwrap();
    assert!(
        matches!(b.execute("COMMIT"), Err(Error::Busy)),
        "B's commit must BUSY while A holds an open read transaction",
    );

    // A ends its read transaction, releasing the SHARED lock.
    a.execute("COMMIT").unwrap();

    // Now B's commit can upgrade to EXCLUSIVE and succeed.
    b.execute("COMMIT").unwrap();

    // The write landed and is visible to a fresh read.
    assert_eq!(scalar_i64(&a, "SELECT count(*) FROM t"), 2);
}

/// DEFERRED semantics (the critical regression guard): two connections both do a
/// bare `BEGIN` with **no read yet**. One then INSERTs and COMMITs — it must
/// SUCCEED, because an empty (never-read) transaction holds no lock. If the read
/// lock were taken eagerly at `BEGIN`, the other's empty txn would wrongly BUSY
/// this writer. This matches SQLite: `BEGIN` is DEFERRED.
#[test]
fn bare_begin_takes_no_lock_writer_succeeds() {
    let (_vfs, mut a, mut b) = two_conns();

    // Both open a transaction but neither reads.
    a.execute("BEGIN").unwrap();
    b.execute("BEGIN").unwrap();

    // B writes and commits: no reader holds a lock, so this succeeds.
    b.execute("INSERT INTO t VALUES (2)").unwrap();
    b.execute("COMMIT").unwrap();

    // A never read, so its still-open transaction never took a lock; end it.
    a.execute("COMMIT").unwrap();

    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM t"), 2);
}

/// A read that first happens *after* a bare `BEGIN` acquires the lock lazily:
/// the writer that could commit before the read now BUSYs once A has read.
#[test]
fn lock_is_taken_at_first_read_not_at_begin() {
    let (_vfs, mut a, mut b) = two_conns();

    // A begins but does not read yet: no lock. A concurrent staged write could
    // still commit at this instant (proven by the bare-begin test); here we
    // instead let A perform its first read, which *now* takes the SHARED lock.
    a.execute("BEGIN").unwrap();
    assert_eq!(a.query("SELECT x FROM t").unwrap().rows.len(), 1);

    // With A's lock now held, B's commit BUSYs.
    b.execute("BEGIN").unwrap();
    b.execute("INSERT INTO t VALUES (2)").unwrap();
    assert!(
        matches!(b.execute("COMMIT"), Err(Error::Busy)),
        "after A's first read the lock is held, so B must BUSY",
    );

    a.execute("COMMIT").unwrap();
    b.execute("COMMIT").unwrap();
}

/// Autocommit `SELECT`s (no open transaction) take no persistent lock, so they
/// never block a concurrent writer.
#[test]
fn autocommit_reads_do_not_block_a_writer() {
    let (_vfs, a, mut b) = two_conns();

    // A reads outside any transaction (autocommit): no persistent lock.
    assert_eq!(a.query("SELECT x FROM t").unwrap().rows.len(), 1);
    assert_eq!(a.query("SELECT x FROM t").unwrap().rows.len(), 1);

    // B can write and commit freely.
    b.execute("BEGIN").unwrap();
    b.execute("INSERT INTO t VALUES (2)").unwrap();
    b.execute("COMMIT").unwrap();

    assert_eq!(scalar_i64(&a, "SELECT count(*) FROM t"), 2);
}

/// A read-only explicit transaction that ends with `ROLLBACK` (rather than
/// `COMMIT`) must also release the persistent SHARED lock, so a later writer
/// proceeds.
#[test]
fn rollback_releases_the_read_lock() {
    let (_vfs, mut a, mut b) = two_conns();

    a.execute("BEGIN").unwrap();
    assert_eq!(a.query("SELECT x FROM t").unwrap().rows.len(), 1);

    b.execute("BEGIN").unwrap();
    b.execute("INSERT INTO t VALUES (2)").unwrap();
    assert!(matches!(b.execute("COMMIT"), Err(Error::Busy)));

    // A rolls back its read-only transaction; the SHARED lock is released.
    a.execute("ROLLBACK").unwrap();

    b.execute("COMMIT").unwrap();
    assert_eq!(scalar_i64(&b, "SELECT count(*) FROM t"), 2);
}
