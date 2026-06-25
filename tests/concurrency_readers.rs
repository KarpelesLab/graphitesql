//! Track C — C9a: reader `SHARED`-lock sharing.
//!
//! SQLite's locking model lets *many* readers hold `SHARED` at once while a
//! writer is excluded for as long as any reader holds it. graphitesql models
//! this process-locally in [`graphitesql::vfs::LockState`], shared per path by
//! every open handle within the process (the `StdVfs` per-path registry and the
//! `MemoryVfs` per-file `Rc<RefCell<LockState>>`).
//!
//! These tests drive the public VFS API the way a pager would — multiple
//! `File` handles over the *same* path — and assert the four C9a properties:
//!   (a) two (or more) readers both hold `SHARED` simultaneously and read
//!       correctly through their independent handles;
//!   (b) a writer attempting to upgrade to `EXCLUSIVE` while a reader still
//!       holds `SHARED` is rejected `Busy`;
//!   (c) once the reader drains, the writer's upgrade succeeds;
//!   (d) `RESERVED` can be taken while a reader holds `SHARED` (a writer
//!       announces intent while readers continue).
//!
//! The same `LockState` backs both VFSs, so the std-file path and the in-memory
//! path are exercised identically through their respective handle types.

#![cfg(feature = "std")]

use graphitesql::vfs::memory::MemoryVfs;
use graphitesql::vfs::std_file::StdVfs;
use graphitesql::vfs::{File, LockLevel, OpenFlags, Vfs};
use graphitesql::{Connection, Error, Value};

/// Per-PID scratch dir so parallel test binaries never collide or delete each
/// other's files.
fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("gsql-c9a-{}", std::process::id()));
    std::fs::create_dir_all(&p).expect("create scratch dir");
    p.push(name);
    p.to_string_lossy().into_owned()
}

/// Drive the four C9a properties over three independent handles to one path.
/// `open` yields a fresh `File` over the shared (per-path) `LockState`.
fn assert_reader_sharing(mut open: impl FnMut() -> Box<dyn File>) {
    // Seed some bytes so the readers have something to read back.
    {
        let mut w = open();
        w.lock(LockLevel::Shared).unwrap();
        w.lock(LockLevel::Reserved).unwrap();
        w.lock(LockLevel::Exclusive).unwrap();
        w.write_all_at(b"graphite", 0).unwrap();
        w.unlock(LockLevel::Unlocked).unwrap();
    }

    let mut r1 = open();
    let mut r2 = open();
    let mut writer = open();

    // (a) Two readers both take SHARED at the same time and read correctly.
    r1.lock(LockLevel::Shared).unwrap();
    r2.lock(LockLevel::Shared).unwrap();
    let mut b1 = [0u8; 8];
    let mut b2 = [0u8; 8];
    r1.read_exact_at(&mut b1, 0).unwrap();
    r2.read_exact_at(&mut b2, 0).unwrap();
    assert_eq!(&b1, b"graphite");
    assert_eq!(&b2, b"graphite");

    // (d) A writer announces intent with RESERVED while both readers still hold
    // SHARED — readers are not disturbed.
    writer.lock(LockLevel::Shared).unwrap();
    writer.lock(LockLevel::Reserved).unwrap();
    // Readers can still read after RESERVED is taken.
    r1.read_exact_at(&mut b1, 0).unwrap();
    assert_eq!(&b1, b"graphite");

    // (b) The writer cannot go EXCLUSIVE while *other* readers hold SHARED.
    assert!(
        matches!(writer.lock(LockLevel::Exclusive), Err(Error::Busy)),
        "EXCLUSIVE must be refused while readers hold SHARED",
    );

    // Drop one reader — the other still holds SHARED, so EXCLUSIVE is still out.
    r1.unlock(LockLevel::Unlocked).unwrap();
    assert!(
        matches!(writer.lock(LockLevel::Exclusive), Err(Error::Busy)),
        "EXCLUSIVE must stay refused while any other reader holds SHARED",
    );

    // (c) Once the last foreign reader drains, the writer's upgrade succeeds.
    r2.unlock(LockLevel::Unlocked).unwrap();
    writer.lock(LockLevel::Exclusive).unwrap();
    writer.write_all_at(b"SQLITE!!", 0).unwrap();
    writer.unlock(LockLevel::Unlocked).unwrap();

    // The exclusive write landed and is visible to a fresh reader.
    let mut fresh = open();
    fresh.lock(LockLevel::Shared).unwrap();
    let mut b3 = [0u8; 8];
    fresh.read_exact_at(&mut b3, 0).unwrap();
    assert_eq!(&b3, b"SQLITE!!");
}

#[test]
fn memory_vfs_readers_share_shared_lock() {
    let vfs = MemoryVfs::new();
    // Pre-create the file so READ_WRITE (no create) handles can open it.
    vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
    assert_reader_sharing(|| vfs.open("db", OpenFlags::READ_WRITE).unwrap());
}

#[test]
fn std_vfs_readers_share_shared_lock() {
    let vfs = StdVfs::new();
    let path = temp_path("readers.db");
    let _ = vfs.delete(&path);
    // Create the backing file once; subsequent handles open it READ_WRITE.
    vfs.open(&path, OpenFlags::READ_WRITE_CREATE).unwrap();
    assert_reader_sharing(|| vfs.open(&path, OpenFlags::READ_WRITE).unwrap());
    let _ = vfs.delete(&path);
}

/// More than two readers coexist: SHARED is a counted lock, not a single owner.
#[test]
fn many_readers_coexist_and_block_exclusive() {
    let vfs = MemoryVfs::new();
    vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();

    let mut readers: Vec<Box<dyn File>> = (0..5)
        .map(|_| vfs.open("db", OpenFlags::READ_WRITE).unwrap())
        .collect();
    for r in &mut readers {
        r.lock(LockLevel::Shared).unwrap();
    }

    // A would-be writer can take SHARED+RESERVED alongside the five readers, but
    // EXCLUSIVE is refused until every one of them drains.
    let mut writer = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    writer.lock(LockLevel::Shared).unwrap();
    writer.lock(LockLevel::Reserved).unwrap();

    for r in readers.iter_mut() {
        assert!(
            matches!(writer.lock(LockLevel::Exclusive), Err(Error::Busy)),
            "EXCLUSIVE must wait for all readers to drain",
        );
        r.unlock(LockLevel::Unlocked).unwrap();
    }
    // All readers gone (only the writer's own SHARED remains) → upgrade wins.
    writer.lock(LockLevel::Exclusive).unwrap();
}

/// A second writer-intent (`RESERVED`) is refused while one is held, even though
/// readers may still come and go — mirrors SQLite's single write-intent rule.
#[test]
fn reserved_is_exclusive_among_writers_but_readers_continue() {
    let vfs = MemoryVfs::new();
    vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();

    let mut a = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    let mut b = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    let mut reader = vfs.open("db", OpenFlags::READ_WRITE).unwrap();

    a.lock(LockLevel::Shared).unwrap();
    a.lock(LockLevel::Reserved).unwrap();

    // A brand-new reader is still admitted while RESERVED is held.
    reader.lock(LockLevel::Shared).unwrap();

    // But a second RESERVED is refused.
    b.lock(LockLevel::Shared).unwrap();
    assert!(
        matches!(b.lock(LockLevel::Reserved), Err(Error::Busy)),
        "only one RESERVED at a time",
    );

    // After A releases, B may take RESERVED.
    a.unlock(LockLevel::Unlocked).unwrap();
    b.lock(LockLevel::Reserved).unwrap();
}

// ---------------------------------------------------------------------------
// End-to-end through the public `Connection` API.
//
// graphitesql's pager only grabs `SHARED` transiently on the path to the write
// lock; pure reads are served without holding a persistent read lock (a foreign
// writer is excluded by the `RESERVED`/`EXCLUSIVE` handshake instead). So at the
// `Connection` layer the observable C9a guarantee is: *concurrent readers never
// block each other or a single writer's reads*, while writers still serialize.
// These tests pin that behaviour down so a future change to the read-lock policy
// can't silently start BUSY-ing concurrent readers.
// ---------------------------------------------------------------------------

/// Two connections, both with an open transaction over the same file, read the
/// same committed rows correctly and concurrently — neither blocks the other.
#[test]
fn two_connections_read_concurrently_without_blocking() {
    let vfs = MemoryVfs::new();
    let mut a = Connection::create_vfs(&vfs, "db", 4096).unwrap();
    a.execute("CREATE TABLE t(x)").unwrap();
    a.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();

    let mut b = Connection::open_vfs(&vfs, "db").unwrap();

    // Both open read transactions and interleave their reads.
    a.execute("BEGIN").unwrap();
    b.execute("BEGIN").unwrap();

    let qa = a.query("SELECT x FROM t ORDER BY x").unwrap().rows;
    let qb = b.query("SELECT x FROM t ORDER BY x").unwrap().rows;
    let expect = vec![
        vec![Value::Integer(1)],
        vec![Value::Integer(2)],
        vec![Value::Integer(3)],
    ];
    assert_eq!(qa, expect, "reader A sees all committed rows");
    assert_eq!(qb, expect, "reader B sees all committed rows concurrently");

    // A second read on each connection, still inside the open txns, still works.
    assert_eq!(
        a.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(3)
    );
    assert_eq!(
        b.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(3)
    );

    a.execute("COMMIT").unwrap();
    b.execute("COMMIT").unwrap();
}

/// A reader keeps reading while another connection holds the write-intent
/// (`RESERVED`) lock — readers are not excluded by `RESERVED`, only a second
/// *writer* is. (The writer-vs-writer BUSY is covered by `concurrency.rs`.)
#[test]
fn reader_unaffected_by_a_concurrent_writers_reserved_lock() {
    let vfs = MemoryVfs::new();
    let mut a = Connection::create_vfs(&vfs, "db", 4096).unwrap();
    a.execute("CREATE TABLE t(x)").unwrap();
    a.execute("INSERT INTO t VALUES (10)").unwrap();

    let reader = Connection::open_vfs(&vfs, "db").unwrap();

    // A enters a write transaction → holds RESERVED.
    a.execute("BEGIN").unwrap();
    a.execute("INSERT INTO t VALUES (20)").unwrap();

    // The reader, on its own connection, still reads the committed snapshot
    // (the uncommitted row 20 is not yet flushed) without any BUSY.
    let rows = reader.query("SELECT x FROM t ORDER BY x").unwrap().rows;
    assert_eq!(rows, vec![vec![Value::Integer(10)]]);

    // After A commits and releases, the reader sees both rows.
    a.execute("COMMIT").unwrap();
    let rows = reader.query("SELECT x FROM t ORDER BY x").unwrap().rows;
    assert_eq!(
        rows,
        vec![vec![Value::Integer(10)], vec![Value::Integer(20)]]
    );
}
