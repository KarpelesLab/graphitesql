//! Hot-journal recovery must run under the `pagerSharedLock` interlock
//! (`pager.c` `hasHotJournal`), never unlocked at open:
//!
//! * a rollback journal whose owner still holds a `RESERVED`-or-stronger lock
//!   belongs to a transaction *in progress* — a second connection opening the
//!   file must **not** play it back (doing so would destroy the writer's
//!   committed baseline);
//! * opening against a writer holding `EXCLUSIVE` (mid-commit) is `Busy`, not a
//!   torn read;
//! * a journal with a **zeroed first byte** (journal_mode=PERSIST leftovers) is
//!   not hot: it must be ignored *without* escalating to the `EXCLUSIVE` lock,
//!   so a concurrent reader does not turn a harmless open into `Busy`;
//! * a journal next to a zero-length database is a stale remnant and is
//!   discarded, not played back;
//! * with no other lock holders, a genuinely hot journal is still recovered
//!   exactly as before.

use graphitesql::pager::WritePager;
use graphitesql::vfs::memory::MemoryVfs;
use graphitesql::vfs::{LockLevel, OpenFlags, Vfs};

/// The 8-byte magic that opens every SQLite-format rollback journal.
const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];
const SECTOR: u64 = 512;
const PAGE: usize = 4096;

/// SQLite's journal page checksum: nonce + the byte at every 200th offset from
/// the end.
fn page_checksum(nonce: u32, page: &[u8]) -> u32 {
    let mut cksum = nonce;
    let mut x = page.len() as isize - 200;
    while x >= 0 {
        cksum = cksum.wrapping_add(page[x as usize] as u32);
        x -= 200;
    }
    cksum
}

/// Build a committed single-page database at `db` in `vfs` and return page 1's
/// committed bytes.
fn build_db(vfs: &MemoryVfs, db: &str) -> Vec<u8> {
    let file = vfs.open(db, OpenFlags::READ_WRITE_CREATE).unwrap();
    let journal = vfs
        .open(&format!("{db}-journal"), OpenFlags::READ_WRITE_CREATE)
        .unwrap();
    let mut wp = WritePager::create(file, Some(journal), PAGE as u32).unwrap();
    wp.commit().unwrap();
    drop(wp);
    let f = vfs.open(db, OpenFlags::READ_ONLY).unwrap();
    let mut p1 = vec![0u8; PAGE];
    f.read_exact_at(&mut p1, 0).unwrap();
    p1
}

/// Write a valid, hot, single-record journal that would restore `orig_p1` as
/// page 1 of a 1-page database.
fn craft_hot_journal(vfs: &MemoryVfs, db: &str, orig_p1: &[u8]) {
    let mut j = vfs
        .open(&format!("{db}-journal"), OpenFlags::READ_WRITE_CREATE)
        .unwrap();
    j.truncate(0).unwrap();
    let nonce = 0x1234_5678u32;
    let mut hdr = vec![0u8; SECTOR as usize];
    hdr[0..8].copy_from_slice(&JOURNAL_MAGIC);
    hdr[8..12].copy_from_slice(&1u32.to_be_bytes()); // one record
    hdr[12..16].copy_from_slice(&nonce.to_be_bytes());
    hdr[16..20].copy_from_slice(&1u32.to_be_bytes()); // original page count
    hdr[20..24].copy_from_slice(&(SECTOR as u32).to_be_bytes());
    hdr[24..28].copy_from_slice(&(PAGE as u32).to_be_bytes());
    j.write_all_at(&hdr, 0).unwrap();
    let mut off = SECTOR;
    j.write_all_at(&1u32.to_be_bytes(), off).unwrap();
    off += 4;
    j.write_all_at(orig_p1, off).unwrap();
    off += PAGE as u64;
    j.write_all_at(&page_checksum(nonce, orig_p1).to_be_bytes(), off)
        .unwrap();
}

/// Scribble on a page-body byte of page 1 (outside the parsed header) so a
/// replayed journal is detectable without making the file unparseable.
fn corrupt_page1_body(vfs: &MemoryVfs, db: &str) {
    let mut f = vfs.open(db, OpenFlags::READ_WRITE).unwrap();
    f.write_all_at(&[0xEE; 8], 4000).unwrap();
}

fn page1(vfs: &MemoryVfs, db: &str) -> Vec<u8> {
    let f = vfs.open(db, OpenFlags::READ_ONLY).unwrap();
    let mut p1 = vec![0u8; PAGE];
    f.read_exact_at(&mut p1, 0).unwrap();
    p1
}

fn journal_size(vfs: &MemoryVfs, db: &str) -> u64 {
    vfs.open(&format!("{db}-journal"), OpenFlags::READ_ONLY)
        .unwrap()
        .size()
        .unwrap()
}

/// A journal whose owner still holds RESERVED is *live*, not hot: a second
/// connection's open must leave both the journal and the database untouched.
#[test]
fn journal_under_reserved_lock_is_not_recovered() {
    let vfs = MemoryVfs::new();
    let orig = build_db(&vfs, "db");
    craft_hot_journal(&vfs, "db", &orig);
    corrupt_page1_body(&vfs, "db");
    let jsize = journal_size(&vfs, "db");

    // The "writer": a handle holding RESERVED, as a connection mid-transaction
    // (journal written, database not yet fully updated) would.
    let guard = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    guard.lock(LockLevel::Shared).unwrap();
    guard.lock(LockLevel::Reserved).unwrap();

    let file = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    let journal = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
    let wp = WritePager::open(file, Some(journal)).unwrap();
    // Not replayed: the scribbled body byte is still there and the journal kept.
    assert_eq!(&wp.read_page(1).unwrap()[4000..4008], &[0xEE; 8]);
    drop(wp);
    assert_eq!(
        journal_size(&vfs, "db"),
        jsize,
        "journal must be left alone"
    );
    assert_ne!(page1(&vfs, "db"), orig, "no rollback under a live RESERVED");

    // The writer finishes (drops its lock): the next open replays the journal.
    guard.unlock(LockLevel::Unlocked).unwrap();
    drop(guard);
    let file = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    let journal = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
    let wp = WritePager::open(file, Some(journal)).unwrap();
    assert_eq!(wp.read_page(1).unwrap(), orig, "hot journal now replayed");
    drop(wp);
    assert_eq!(
        journal_size(&vfs, "db"),
        0,
        "journal cleared after playback"
    );
}

/// Opening against a writer mid-commit (EXCLUSIVE held) with a journal on disk
/// is `Busy` — never a read of half-committed state.
#[test]
fn open_against_exclusive_writer_is_busy() {
    let vfs = MemoryVfs::new();
    let orig = build_db(&vfs, "db");
    craft_hot_journal(&vfs, "db", &orig);

    let guard = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    guard.lock(LockLevel::Shared).unwrap();
    guard.lock(LockLevel::Exclusive).unwrap();

    let file = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    let journal = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
    assert!(matches!(
        WritePager::open(file, Some(journal)),
        Err(graphitesql::Error::Busy)
    ));
}

/// A hot journal blocked by a concurrent *reader* (SHARED held, so the
/// EXCLUSIVE needed for playback cannot be taken) surfaces as `Busy` rather
/// than an unlocked rollback under the reader's feet.
#[test]
fn hot_journal_with_concurrent_reader_is_busy() {
    let vfs = MemoryVfs::new();
    let orig = build_db(&vfs, "db");
    craft_hot_journal(&vfs, "db", &orig);

    let guard = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    guard.lock(LockLevel::Shared).unwrap();

    let file = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    let journal = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
    assert!(matches!(
        WritePager::open(file, Some(journal)),
        Err(graphitesql::Error::Busy)
    ));
}

/// A journal whose first byte is zero (a PERSIST-mode leftover) is not hot: the
/// open must succeed *without* escalating to EXCLUSIVE, even while a concurrent
/// reader holds SHARED.
#[test]
fn zeroed_journal_is_ignored_without_escalation() {
    let vfs = MemoryVfs::new();
    let orig = build_db(&vfs, "db");
    {
        let mut j = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
        j.truncate(0).unwrap();
        j.write_all_at(&[0u8; SECTOR as usize], 0).unwrap(); // zeroed header
    }

    let guard = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    guard.lock(LockLevel::Shared).unwrap();

    let file = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    let journal = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
    let wp = WritePager::open(file, Some(journal)).unwrap();
    assert_eq!(wp.read_page(1).unwrap(), orig);
}

/// A journal next to a zero-length database is a stale remnant: it is
/// discarded, never played back (`hasHotJournal`'s `nPage==0` branch).
#[test]
fn journal_next_to_empty_database_is_discarded() {
    let vfs = MemoryVfs::new();
    // A zero-length "database" plus a plausible journal.
    vfs.open("db", OpenFlags::READ_WRITE_CREATE).unwrap();
    let fake = vec![0xABu8; PAGE];
    craft_hot_journal(&vfs, "db", &fake);
    assert!(journal_size(&vfs, "db") > 0);

    let file = vfs.open("db", OpenFlags::READ_WRITE).unwrap();
    let journal = vfs.open("db-journal", OpenFlags::READ_WRITE).unwrap();
    // The open itself still fails (a zero-length file is not a database), but
    // the stale journal must be gone and the database untouched.
    assert!(WritePager::open(file, Some(journal)).is_err());
    assert_eq!(journal_size(&vfs, "db"), 0, "stale journal discarded");
    let f = vfs.open("db", OpenFlags::READ_ONLY).unwrap();
    assert_eq!(f.size().unwrap(), 0, "empty database left empty");
}
