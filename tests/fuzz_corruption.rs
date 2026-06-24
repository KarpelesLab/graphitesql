//! Corruption-robustness ("fuzz-style") tests for the readers (roadmap §6).
//!
//! There is no fuzzer dependency (zero-dep crate), so these are ordinary
//! deterministic `#[test]`s that synthesize a large, systematically-enumerated
//! set of *malformed* database files and assert that opening them and running a
//! few queries never **panics**. The reader is allowed to return `Ok` *or*
//! `Err` — what it must never do is index out of bounds, overflow an integer, or
//! otherwise abort the process. For structurally-impossible files we also assert
//! `.is_err()`.
//!
//! Because the crate is `#![forbid(unsafe_code)]` and has no `catch_unwind`-free
//! way to observe a panic in `no_std`, the panic detection here uses
//! `std::panic::catch_unwind` (these tests are `std`-only). A caught panic is
//! turned into a test failure naming the offending corruption, which is what
//! drove the reader fixes that accompany these tests.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Every temp file this test process creates lives under a single per-PID
/// directory (`<tmp>/gsql-fuzz-<pid>/`), so cleanup is one `rm -rf` and the
/// `-journal`/`-wal` sidecars can never escape into the shared temp root. The
/// directory is created on first use.
fn fuzz_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("gsql-fuzz-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Remove a database file together with its `-journal` / `-wal` / `-shm`
/// sidecars (writing a corrupt file can leave a rollback journal or WAL behind).
fn rm_db(p: &str) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{p}{suffix}"));
    }
}

/// Build one valid graphitesql database (with an index and an overflowing blob,
/// so the corruption space touches index pages and overflow chains) and return
/// its raw bytes.
fn build_base() -> Vec<u8> {
    let uniq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = fuzz_dir().join(format!("base-{uniq}.db"));
    let p = path.to_string_lossy().into_owned();
    rm_db(&p);
    {
        let mut c = Connection::create(&p).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, k INT, v TEXT)")
            .unwrap();
        c.execute("CREATE INDEX ik ON t(k)").unwrap();
        for i in 0..50 {
            c.execute(&format!(
                "INSERT INTO t(k,v) VALUES ({}, 'val{}')",
                i % 7,
                i
            ))
            .unwrap();
        }
        c.execute("CREATE TABLE big(b BLOB)").unwrap();
        let big = "x".repeat(20000);
        c.execute(&format!("INSERT INTO big VALUES ('{big}')"))
            .unwrap();
    }
    let data = std::fs::read(&p).unwrap();
    rm_db(&p);
    data
}

/// Write `bytes` to a fresh file, then open it and run several queries inside
/// `catch_unwind`. Returns `true` if `Connection::open` succeeded (the queries'
/// individual results are intentionally ignored — we only care that nothing
/// panicked). Panics (failing the test) if any reader path unwinds.
fn open_no_panic(bytes: &[u8], tag: &str) -> bool {
    let uniq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = fuzz_dir().join(format!("{uniq}-{}.db", tag.replace(['/', ' '], "_")));
    let p = path.to_string_lossy().into_owned();
    rm_db(&p);
    {
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
    }
    let pc = p.clone();
    let r = catch_unwind(AssertUnwindSafe(move || -> bool {
        match Connection::open(&pc) {
            Ok(c) => {
                let _ = c.query("SELECT * FROM t");
                let _ = c.query("SELECT * FROM t WHERE k = 3");
                let _ = c.query("SELECT * FROM t WHERE id = 10");
                let _ = c.query("SELECT * FROM big");
                let _ = c.query("SELECT count(*) FROM t");
                let _ = c.query("SELECT name FROM sqlite_schema");
                let _ = c.query("PRAGMA integrity_check");
                true
            }
            Err(_) => false,
        }
    }));
    rm_db(&p);
    match r {
        Ok(opened) => opened,
        Err(_) => panic!("reader PANICKED on corrupted input (tag={tag})"),
    }
}

fn page_size_of(base: &[u8]) -> usize {
    let raw = u16::from_be_bytes([base[16], base[17]]) as usize;
    if raw <= 1 {
        65536
    } else {
        raw
    }
}

#[test]
fn truncation_at_many_lengths() {
    let base = build_base();
    // Every length from 0 up to a few pages in: 0 bytes, 50, 99, 100, half a
    // page, page boundaries ± 1, etc. are all covered by the dense sweep.
    let cap = base.len().min(9000);
    for len in 0..cap {
        let opened = open_no_panic(&base[..len], &format!("trunc-{len}"));
        // Anything shorter than the 100-byte header cannot be a database.
        if len < 100 {
            assert!(!opened, "a {len}-byte file must not open as a database");
        }
    }
}

#[test]
fn corrupted_header_fields() {
    let base = build_base();
    // Single-byte mutations across the whole 100-byte header.
    for pos in 0..100usize {
        for val in [0u8, 1, 7, 0x55, 0x80, 0xff] {
            let mut b = base.clone();
            b[pos] = val;
            open_no_panic(&b, &format!("hdr-{pos}-{val}"));
        }
    }
    // Page size: 0, 1, 7, 3, 100, non-power-of-two, 0xFFFF, etc. All but the
    // sentinel/valid ones are structurally impossible and must error.
    for ps in [0u16, 1, 3, 7, 100, 0x1ff, 0x201, 3000, 4097, 0xffff] {
        let mut b = base.clone();
        b[16..18].copy_from_slice(&ps.to_be_bytes());
        let opened = open_no_panic(&b, &format!("ps-{ps}"));
        let valid = ps == 1 || (ps >= 512 && ps.is_power_of_two());
        if !valid {
            assert!(!opened, "page size {ps} must be rejected");
        }
    }
    // Reserved-space byte (offset 20): the full range, including absurd values.
    for r in 0u8..=255 {
        let mut b = base.clone();
        b[20] = r;
        open_no_panic(&b, &format!("resv-{r}"));
    }
    // Invalid text-encoding byte (offset 56..60); only 1/2/3 are valid.
    for enc in [0u32, 4, 0xff, 0xffff_ffff] {
        let mut b = base.clone();
        b[56..60].copy_from_slice(&enc.to_be_bytes());
        let opened = open_no_panic(&b, &format!("enc-{enc}"));
        assert!(!opened, "text encoding {enc} must be rejected");
    }
    // 32-bit header pointers/counts wildly wrong: in-header page count, freelist
    // trunk, freelist count, largest-root-page (auto-vacuum).
    for off in [28u32, 32, 36, 52] {
        for val in [1u32, 2, 0x7fff_ffff, 0xffff_ffff] {
            let mut b = base.clone();
            b[off as usize..off as usize + 4].copy_from_slice(&val.to_be_bytes());
            open_no_panic(&b, &format!("h32-{off}-{val}"));
        }
    }
    // Bad magic: never opens.
    let mut bad_magic = base.clone();
    bad_magic[0] = b'X';
    assert!(!open_no_panic(&bad_magic, "bad-magic"));
}

#[test]
fn corrupted_btree_pages() {
    let base = build_base();
    let ps = page_size_of(&base);
    // Dense single-byte corruption across the b-tree pages (page 1 body and the
    // following pages): exercises page-type bytes, cell counts, the cell-pointer
    // array, and cell contents.
    let end = base.len().min(ps * 3);
    for pos in 100..end {
        for val in [0u8, 0xff, 0x02, 0x05, 0x0a, 0x0d] {
            let mut b = base.clone();
            b[pos] = val;
            open_no_panic(&b, &format!("pg-{pos}-{val}"));
        }
    }

    // Targeted structural attacks on each page's header & cell-pointer array:
    // bogus page-type byte, absurd cell counts, and cell pointers that point
    // past the page / to byte 0 / overlap.
    let npages = (base.len() / ps).min(4);
    for pg in 0..npages {
        let pstart = pg * ps;
        let body = if pg == 0 { 100 } else { 0 };

        // Page-type byte -> every value (most are invalid).
        for ty in 0u8..=255 {
            let mut b = base.clone();
            b[pstart + body] = ty;
            open_no_panic(&b, &format!("ptype-{pg}-{ty}"));
        }

        // Cell count -> huge / off-by-one.
        for nc in [0xffffu16, 0x7fff, 0x00ff, 1] {
            let mut b = base.clone();
            b[pstart + body + 3..pstart + body + 5].copy_from_slice(&nc.to_be_bytes());
            open_no_panic(&b, &format!("ncells-{pg}-{nc}"));
        }

        // Cell-pointer array entries -> offsets past the page, into the header,
        // to zero, overlapping. Hit the first 24 slots.
        for k in 0..24usize {
            for off in [
                0xffffu16,
                0x0000,
                (ps as u16).wrapping_sub(1),
                1,
                8,
                body as u16,
            ] {
                let mut b = base.clone();
                let at = pstart + body + 8 + 2 * k;
                if at + 2 <= b.len() {
                    b[at..at + 2].copy_from_slice(&off.to_be_bytes());
                    open_no_panic(&b, &format!("cellptr-{pg}-{k}-{off}"));
                }
            }
        }
    }
}

#[test]
fn whole_page_and_random_garbage() {
    let base = build_base();
    let ps = page_size_of(&base);
    let npages = base.len() / ps;

    // Fill each page with a fixed pattern (page 1 keeps its header).
    for pg in 0..npages {
        for pat in [0u8, 0xff, 0xaa, 0x55] {
            let mut b = base.clone();
            let start = if pg == 0 { 100 } else { pg * ps };
            let end = (pg + 1) * ps;
            for byte in &mut b[start..end] {
                *byte = pat;
            }
            open_no_panic(&b, &format!("fill-{pg}-{pat}"));
        }
    }

    // Whole-file pseudo-random garbage (a simple deterministic LCG) of many
    // lengths, half of them keeping a valid magic so parsing proceeds further.
    let mut state: u64 = 0x1234_5678_9abc_def0;
    for trial in 0..300 {
        let len = (state as usize % base.len()).max(100);
        let mut b = vec![0u8; len];
        for byte in &mut b {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (state >> 33) as u8;
        }
        if trial % 2 == 0 {
            b[0..16].copy_from_slice(b"SQLite format 3\0");
        }
        open_no_panic(&b, &format!("rand-{trial}"));
    }
}

/// Forge a minimal valid header for a 1-page database of arbitrary page size and
/// reserved space, with an empty table-leaf root on page 1. Lets us reach reader
/// code paths (e.g. the payload-overflow split) with a *small* usable size that
/// a real, 4 KiB-page database never produces.
fn forge_header(page_size: u32, reserved: u8) -> Vec<u8> {
    let mut page = vec![0u8; page_size as usize];
    page[0..16].copy_from_slice(b"SQLite format 3\0");
    let raw_ps: u16 = if page_size == 65536 {
        1
    } else {
        page_size as u16
    };
    page[16..18].copy_from_slice(&raw_ps.to_be_bytes());
    page[18] = 1;
    page[19] = 1;
    page[20] = reserved;
    page[21] = 64;
    page[22] = 32;
    page[23] = 32;
    page[28..32].copy_from_slice(&1u32.to_be_bytes()); // size in pages
    page[44..48].copy_from_slice(&4u32.to_be_bytes()); // schema format
    page[56..60].copy_from_slice(&1u32.to_be_bytes()); // utf-8
    page[92..96].copy_from_slice(&1u32.to_be_bytes()); // version-valid-for
    page[100] = 0x0d; // table-leaf root, 0 cells
    page
}

#[test]
fn forged_small_pages_and_overflow() {
    for &ps in &[512u32, 1024, 2048, 65536] {
        for &reserved in &[0u8, 1, 100, 200, 250, 255] {
            if reserved as u32 >= ps {
                continue;
            }
            let mut b = forge_header(ps, reserved);
            open_no_panic(&b, &format!("forge-{ps}-{reserved}"));

            // Bogus 9-byte varint payload lengths planted in the body, to drive
            // the payload-split / overflow math with attacker-controlled sizes.
            for at in [100usize, 108, 110, 116] {
                let mut bb = b.clone();
                if at + 9 <= bb.len() {
                    for byte in &mut bb[at..at + 8] {
                        *byte = 0xff;
                    }
                    bb[at + 8] = 0x7f;
                    open_no_panic(&bb, &format!("varint-{ps}-{reserved}-{at}"));
                }
            }

            // Claim one cell whose payload overflows onto a page past EOF.
            b[103..105].copy_from_slice(&1u16.to_be_bytes()); // num_cells = 1
            b[108..110].copy_from_slice(&111u16.to_be_bytes()); // cell ptr -> 111
            if 111 + 12 <= b.len() {
                b[111] = 0x81; // payload-length varint, continuation
                b[112] = 0x00; // -> 128 bytes
                b[113] = 0x01; // rowid varint = 1
                b[114..118].copy_from_slice(&0xffff_ffffu32.to_be_bytes()); // overflow page
                open_no_panic(&b, &format!("overflow-{ps}-{reserved}"));
            }
        }
    }
}

#[test]
fn corrupted_wal_sidecar() {
    let base = build_base();
    let ps = page_size_of(&base);
    let path = fuzz_dir().join(format!(
        "wal-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let p = path.to_string_lossy().into_owned();
    let wp = format!("{p}-wal");
    rm_db(&p);
    let _ = std::fs::remove_file(&wp);
    std::fs::write(&p, &base).unwrap();

    let mut state: u64 = 0xdead_beef_cafe_babe;
    for trial in 0..200 {
        let len = 32 + (state as usize % (4 * (ps + 24)));
        let mut w = vec![0u8; len];
        for byte in &mut w {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (state >> 40) as u8;
        }
        if trial % 3 == 0 {
            // Plausible WAL magic + page size so frame parsing proceeds.
            w[0..4].copy_from_slice(&0x377f_0682u32.to_be_bytes());
            w[8..12].copy_from_slice(&(ps as u32).to_be_bytes());
        }
        std::fs::write(&wp, &w).unwrap();
        let r = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(c) = Connection::open(&p) {
                let _ = c.query("SELECT * FROM t");
                let _ = c.query("PRAGMA integrity_check");
            }
        }));
        if r.is_err() {
            rm_db(&p);
            let _ = std::fs::remove_file(&wp);
            panic!("WAL reader PANICKED on corrupted sidecar (trial={trial})");
        }
    }
    rm_db(&p);
    let _ = std::fs::remove_file(&wp);
}
