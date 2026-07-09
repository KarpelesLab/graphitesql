//! Track C — C9a: the pager's **persistent** read-lock policy.
//!
//! SQLite holds `PAGER_SHARED` for the whole of an open read transaction, not
//! just transiently per page (see `pager.c` `sqlite3PagerBegin` /
//! `pager_wait_on_lock`). That is what makes a reader *visible* to a concurrent
//! writer: while any connection holds an open read transaction, another
//! connection's commit-time upgrade to `EXCLUSIVE` returns `SQLITE_BUSY`; when
//! the reader ends its transaction the writer proceeds. Readers still coexist
//! (`SHARED` is a counted lock).
//!
//! graphitesql's [`WritePager`] models this process-locally: two pagers over the
//! same [`MemoryVfs`] path share one `LockState`, exactly as two connections in
//! one process would. These tests drive the pager API directly —
//! [`WritePager::begin_read_txn`] / [`WritePager::end_read_txn`] open and close
//! the persistent read lock — and assert the C9a semantics:
//!   (1) two readers hold the persistent `SHARED` lock at once and both read;
//!   (2) while a reader holds it, a writer's `commit` (which upgrades to
//!       `EXCLUSIVE`) BUSYs;
//!   (3) a writer may still take `RESERVED` while the reader holds `SHARED`;
//!   (4) once the reader ends its txn, the writer's commit succeeds.
//!
//! NOTE: exposing this at the `Connection` layer needs the exec layer to call
//! `begin_read_txn`/`end_read_txn` at `BEGIN`/`COMMIT`/`ROLLBACK` (that file is
//! owned by another track); these tests pin the pager mechanism the hook drives.

#![cfg(feature = "std")]

use graphitesql::Error;
use graphitesql::pager::WritePager;
use graphitesql::vfs::memory::MemoryVfs;
use graphitesql::vfs::{File, OpenFlags, Vfs};
use std::boxed::Box;

/// Open a fresh `File` handle over the shared per-path `LockState`.
fn open_file(vfs: &MemoryVfs, path: &str) -> Box<dyn File> {
    vfs.open(path, OpenFlags::READ_WRITE).unwrap()
}

/// Create a real, committed 1-page database in `vfs` at `path` and stage one
/// user-visible byte on page 1 so readers have something to read back.
fn seed_db(vfs: &MemoryVfs, path: &str) {
    vfs.open(path, OpenFlags::READ_WRITE_CREATE).unwrap();
    let file = open_file(vfs, path);
    let mut wp = WritePager::create(file, None, 4096).unwrap();
    // Stamp a marker into page 1's body (after the 100-byte header) and commit.
    let mut page1 = wp.read_page(1).unwrap();
    page1[200] = 0xAB;
    wp.write_page(1, page1).unwrap();
    wp.commit().unwrap();
}

/// Open a pager over an already-created database file.
fn open_pager(vfs: &MemoryVfs, path: &str) -> WritePager {
    WritePager::open(open_file(vfs, path), None).unwrap()
}

/// (1) Two readers both hold the persistent `SHARED` lock and read concurrently,
/// and (2) a third connection's write commit BUSYs until they drain (4).
#[test]
fn readers_hold_shared_and_block_a_writers_commit() {
    let vfs = MemoryVfs::new();
    seed_db(&vfs, "db");

    let mut r1 = open_pager(&vfs, "db");
    let mut r2 = open_pager(&vfs, "db");
    let mut w = open_pager(&vfs, "db");

    // (1) Both readers open a read transaction: each takes the persistent SHARED
    // lock. They coexist (SHARED is counted) and both read the committed marker.
    r1.begin_read_txn().unwrap();
    r2.begin_read_txn().unwrap();
    assert!(r1.in_read_txn() && r2.in_read_txn());
    assert_eq!(r1.read_page(1).unwrap()[200], 0xAB);
    assert_eq!(r2.read_page(1).unwrap()[200], 0xAB);

    // (3) The writer stages a change — taking RESERVED is fine while readers hold
    // SHARED (RESERVED coexists with readers).
    let mut p = w.read_page(1).unwrap();
    p[201] = 0xCD;
    w.write_page(1, p).unwrap();

    // (2) The writer's commit upgrades to EXCLUSIVE, which cannot be granted while
    // either reader still holds SHARED → BUSY.
    assert!(
        matches!(w.commit(), Err(Error::Busy)),
        "commit must BUSY while readers hold the persistent SHARED lock",
    );

    // One reader ends its txn; the other still holds SHARED → still BUSY.
    r1.end_read_txn();
    assert!(!r1.in_read_txn());
    assert!(
        matches!(w.commit(), Err(Error::Busy)),
        "commit must stay BUSY while any reader holds SHARED",
    );

    // (4) The last reader ends its txn; the writer's commit now succeeds.
    r2.end_read_txn();
    w.commit().unwrap();

    // The write landed and is visible to a fresh reader.
    let fresh = open_pager(&vfs, "db");
    assert_eq!(fresh.read_page(1).unwrap()[201], 0xCD);
}

/// A reader that ends its transaction frees the lock immediately: a subsequent
/// writer commit succeeds with no lingering BUSY.
#[test]
fn ending_a_read_txn_releases_the_shared_lock() {
    let vfs = MemoryVfs::new();
    seed_db(&vfs, "db");

    let mut r = open_pager(&vfs, "db");
    let mut w = open_pager(&vfs, "db");

    r.begin_read_txn().unwrap();
    let mut p = w.read_page(1).unwrap();
    p[201] = 0x11;
    w.write_page(1, p).unwrap();
    assert!(matches!(w.commit(), Err(Error::Busy)));

    // Reader done → writer proceeds.
    r.end_read_txn();
    w.commit().unwrap();

    let fresh = open_pager(&vfs, "db");
    assert_eq!(fresh.read_page(1).unwrap()[201], 0x11);
}

/// `begin_read_txn` is idempotent and `end_read_txn` is a no-op when no read
/// transaction is open — neither strands a lock.
#[test]
fn begin_and_end_read_txn_are_idempotent() {
    let vfs = MemoryVfs::new();
    seed_db(&vfs, "db");

    let mut r = open_pager(&vfs, "db");
    // end before begin: no-op, no lock held afterwards.
    r.end_read_txn();
    assert!(!r.in_read_txn());

    // Two begins, then reads still work; a foreign writer is blocked until end.
    r.begin_read_txn().unwrap();
    r.begin_read_txn().unwrap();
    assert!(r.in_read_txn());
    assert_eq!(r.read_page(1).unwrap()[200], 0xAB);

    let mut w = open_pager(&vfs, "db");
    let mut p = w.read_page(1).unwrap();
    p[202] = 0x22;
    w.write_page(1, p).unwrap();
    assert!(matches!(w.commit(), Err(Error::Busy)));

    r.end_read_txn();
    // A second end is a harmless no-op.
    r.end_read_txn();
    w.commit().unwrap();
}

/// More than two readers coexist on the persistent lock and all of them must
/// drain before the writer's commit is admitted.
#[test]
fn many_persistent_readers_coexist_and_block_commit() {
    let vfs = MemoryVfs::new();
    seed_db(&vfs, "db");

    let mut readers: Vec<WritePager> = (0..5).map(|_| open_pager(&vfs, "db")).collect();
    for r in &mut readers {
        r.begin_read_txn().unwrap();
        assert_eq!(r.read_page(1).unwrap()[200], 0xAB);
    }

    let mut w = open_pager(&vfs, "db");
    let mut p = w.read_page(1).unwrap();
    p[203] = 0x33;
    w.write_page(1, p).unwrap();

    // Drain readers one at a time; the commit stays BUSY until the last one ends.
    for r in readers.iter_mut() {
        assert!(
            matches!(w.commit(), Err(Error::Busy)),
            "commit must wait for every reader to end its txn",
        );
        r.end_read_txn();
    }
    w.commit().unwrap();

    let fresh = open_pager(&vfs, "db");
    assert_eq!(fresh.read_page(1).unwrap()[203], 0x33);
}
