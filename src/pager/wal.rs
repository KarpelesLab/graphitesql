//! Read support for the SQLite Write-Ahead Log (WAL).
//!
//! When a database is in WAL mode, the newest committed version of a page may
//! live in the `-wal` file rather than the main database file. To read such a
//! database correctly we parse the WAL, validate it, and overlay its frames on
//! top of the main file. The WAL format and its checksum algorithm are specified
//! in the file-format docs ("Write-Ahead Log Format") and `wal.c`.
//!
//! WAL layout:
//!
//! * **32-byte header**: magic (`0x377f0682`/`0x377f0683` — LSB selects checksum
//!   byte order), format version, page size, checkpoint sequence, two salt
//!   values, and the header checksum.
//! * **frames**: a 24-byte header (page number; db size in pages for a *commit*
//!   frame else 0; the two salts; the running checksum) followed by one page of
//!   data.
//!
//! A frame is valid only if its salts match the header's and the running
//! checksum (seeded from the previous frame, or the header checksum for the
//! first frame) matches. We honor frames up to the last valid **commit** frame;
//! the page→frame map keeps the most recent frame per page within that range.
//!
//! This is the **read** half of WAL. Writing WAL (and the `-shm` wal-index) is
//! tracked for later; reading is what compatibility with WAL-mode databases
//! requires.

use super::{Page, PageSource};
use crate::error::{Error, Result};
use crate::format::DatabaseHeader;
use crate::format::header::HEADER_LEN;
use crate::vfs::File;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
#[cfg(not(feature = "std"))]
use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
#[cfg(not(feature = "std"))]
use core::cell::RefCell;

// NOTE: the shared wal-index stores frame bytes as plain `Vec<u8>` (not `Rc`) so
// the whole `WalIndex` stays `Send + Sync` for the std VFS's `Arc<Mutex<..>>`
// variant without pulling in `alloc::sync::Arc` (which requires target atomics
// the default `no_std` build must not assume). Frame reads clone the bytes, the
// same as the existing per-connection WAL path already does.

const WAL_HEADER_LEN: usize = 32;
const FRAME_HEADER_LEN: usize = 24;
const WAL_MAGIC_BE: u32 = 0x377f_0682; // base magic; LSB carries the endianness

/// Whether this build's native byte order is big-endian — `SQLITE_BIGENDIAN` in
/// `wal.c`. A fresh WAL header stamps `WAL_MAGIC | SQLITE_BIGENDIAN` and computes
/// checksums in native word order (`walChecksumBytes(1, …)`), so graphite does
/// the same: little-endian checksums (magic `…82`) on little-endian targets,
/// big-endian (magic `…83`) on big-endian ones.
pub(crate) const NATIVE_BIG_ENDIAN: bool = cfg!(target_endian = "big");

/// A parsed, validated WAL overlaid on a main database file, presented as a
/// [`PageSource`] that returns WAL frames where they exist and main-file pages
/// otherwise.
pub struct WalReader {
    main: Box<dyn File>,
    header: DatabaseHeader,
    page_size: usize,
    /// Database size in pages at the snapshot (from the last commit frame, or
    /// the main file if the WAL has no committed frames).
    db_size: u32,
    /// Page number → page bytes, for pages whose newest version is in the WAL.
    frames: BTreeMap<u32, Vec<u8>>,
}

impl WalReader {
    /// Open `main` overlaid with the WAL bytes from `wal`. If the WAL is empty or
    /// invalid, the main file is used alone.
    pub fn open(main: Box<dyn File>, wal: &mut dyn File) -> Result<WalReader> {
        let wal_size = wal.size()?;
        let mut frames = BTreeMap::new();
        let mut commit_db_size = 0u32;
        let mut wal_page_size = 0usize;

        if wal_size >= WAL_HEADER_LEN as u64 {
            let mut hdr = [0u8; WAL_HEADER_LEN];
            wal.read_exact_at(&mut hdr, 0)?;
            let magic = be32(&hdr, 0);
            if magic & 0xFFFF_FFFE != WAL_MAGIC_BE {
                return Err(Error::Corrupt("bad WAL magic".into()));
            }
            // The magic's least-significant bit selects the checksum byte order:
            // 1 = big-endian, 0 = little-endian (per `wal.c`'s `bigEndCksum`).
            let big_endian = (magic & 1) == 1;
            let page_size = be32(&hdr, 8) as usize;
            if page_size < 512 || !page_size.is_power_of_two() {
                return Err(Error::Corrupt("bad WAL page size".into()));
            }
            wal_page_size = page_size;
            let salt = &hdr[16..24];

            // Validate the header checksum (over the first 24 bytes, seed 0,0).
            let (h0, h1) = checksum(big_endian, 0, 0, &hdr[0..24]);
            if h0 != be32(&hdr, 24) || h1 != be32(&hdr, 28) {
                return Err(Error::Corrupt("WAL header checksum mismatch".into()));
            }

            // Walk frames, keeping a running checksum; record committed frames.
            let frame_len = FRAME_HEADER_LEN + page_size;
            let (mut s0, mut s1) = (h0, h1);
            let mut off = WAL_HEADER_LEN as u64;
            // Tentative frames since the last commit (applied atomically on commit).
            let mut pending: Vec<(u32, Vec<u8>)> = Vec::new();
            while off + frame_len as u64 <= wal_size {
                let mut fhdr = [0u8; FRAME_HEADER_LEN];
                wal.read_exact_at(&mut fhdr, off)?;
                let mut page = vec![0u8; page_size];
                wal.read_exact_at(&mut page, off + FRAME_HEADER_LEN as u64)?;

                // Salts must match the header, else the WAL was restarted here.
                if fhdr[8..16] != *salt {
                    break;
                }
                // Running checksum over frame-header[0..8] then the page data.
                let (c0, c1) = checksum(big_endian, s0, s1, &fhdr[0..8]);
                let (c0, c1) = checksum(big_endian, c0, c1, &page);
                if c0 != be32(&fhdr, 16) || c1 != be32(&fhdr, 20) {
                    break; // corrupt/torn frame: stop here
                }
                s0 = c0;
                s1 = c1;

                let page_no = be32(&fhdr, 0);
                let db_size = be32(&fhdr, 4);
                pending.push((page_no, page));
                if db_size != 0 {
                    // Commit frame: publish all pending frames atomically.
                    for (p, data) in pending.drain(..) {
                        frames.insert(p, data);
                    }
                    commit_db_size = db_size;
                }
                off += frame_len as u64;
            }
        }

        // Determine the authoritative page 1 (it may itself be in the WAL).
        let page1 = match frames.get(&1) {
            Some(p) => p.clone(),
            None => {
                let mut buf = vec![0u8; HEADER_LEN.max(512)];
                let want = main.size()?.min(buf.len() as u64) as usize;
                buf.truncate(want);
                main.read_exact_at(&mut buf, 0)?;
                buf
            }
        };
        let header = DatabaseHeader::parse(&page1)?;
        let page_size = if wal_page_size != 0 {
            wal_page_size
        } else {
            header.page_size as usize
        };

        let db_size = if commit_db_size != 0 {
            commit_db_size
        } else {
            (main.size()? / page_size as u64) as u32
        };

        Ok(WalReader {
            main,
            header,
            page_size,
            db_size,
            frames,
        })
    }
}

impl PageSource for WalReader {
    fn page(&self, number: u32) -> Result<Page> {
        if number == 0 || number > self.db_size {
            return Err(Error::Corrupt(alloc::format!(
                "page {number} out of range 1..={}",
                self.db_size
            )));
        }
        if let Some(data) = self.frames.get(&number) {
            return Ok(Page::from_bytes(number, data.clone()));
        }
        let mut buf = vec![0u8; self.page_size];
        self.main
            .read_exact_at(&mut buf, (number as u64 - 1) * self.page_size as u64)?;
        Ok(Page::from_bytes(number, buf))
    }
    fn header(&self) -> &DatabaseHeader {
        &self.header
    }
    fn usable_size(&self) -> usize {
        self.header.usable_size() as usize
    }
    fn page_count(&self) -> u32 {
        self.db_size
    }
}

#[inline]
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// A writer's continuation state for the shared `-wal`: `(next append offset,
/// running checksum, salts, page size)`. Returned by [`WalIndex::writer_state`].
pub type WriterState = (u64, (u32, u32), [u8; 8], u32);

/// One committed WAL frame in the shared wal-index log: the page it rewrites, the
/// database size in pages recorded by the commit this frame belongs to (0 for a
/// non-commit frame within a multi-page transaction), and the salts under which
/// it was written. Frames are appended to the log in commit order; a reader
/// resolving a page consults them newest-first up to its pinned `mx_frame`.
#[derive(Clone)]
struct IndexFrame {
    page_no: u32,
    /// The post-commit db size in pages, tagged on the **last** frame of each
    /// committed transaction (0 on the others). Non-zero marks a commit boundary.
    commit_db_size: u32,
    data: Vec<u8>,
}

/// The process-local **wal-index**: a coherent, shared page→latest-frame map for
/// one WAL-mode database file, mirroring the role of SQLite's `-shm` wal-index
/// (`wal.c`) but as transient in-process state (no byte-compat with `-shm` is
/// needed — the wal-index is regenerable).
///
/// A single instance is shared by every in-process `Connection`'s pager over the
/// same file (keyed by path in the VFS, exactly like the [`LockState`] registry).
/// The writer, under the write-intent lock, appends its committed frames here and
/// bumps [`mx_frame`](WalIndex::mx_frame); a reader snapshots `mx_frame` at
/// read-transaction start and resolves every page against frames ≤ that snapshot,
/// so it sees a stable view (repeatable read) even while a writer commits more.
///
/// [`LockState`]: crate::vfs::LockState
pub struct WalIndex {
    /// Committed frames in append (commit) order. `log[i]` is frame index `i+1`
    /// (frame indices are 1-based, matching `wal.c`'s `iFrame`).
    log: Vec<IndexFrame>,
    /// The number of valid committed frames — the reader's snapshot upper bound
    /// and the writer's high-water mark (`WalIndexHdr.mxFrame`).
    mx_frame: u32,
    /// The salts currently in force. Rewritten when the WAL is reset
    /// (checkpoint/VACUUM/mode switch); a change invalidates any older log.
    salt: [u8; 8],
    /// Byte offset in the `-wal` file just past the last committed frame — where
    /// the next writer appends. Kept here so a second writer picks up where the
    /// first left off without re-scanning the file.
    next_offset: u64,
    /// Running checksum after the last committed frame (seed for the next append).
    cksum: (u32, u32),
    /// The page size of frames in this WAL, or 0 before the first frame.
    page_size: u32,
    /// The number of frames already copied back into the main database file by a
    /// checkpoint — `WalCkptInfo.nBackfill` in `wal.c`. Only a checkpoint
    /// increases it; a WAL reset/restart returns it to 0. Frames ≤ this mark are
    /// durable in the main file, so once it reaches [`mx_frame`](Self::mx_frame)
    /// the next writer may restart the log from the beginning (`walRestartLog`).
    n_backfill: u32,
    /// The checkpoint sequence number stamped into the WAL header (`Wal.nCkpt`,
    /// WAL header bytes 12–15). Incremented by every log restart
    /// ([`restart_hdr`](Self::restart_hdr)), exactly like `walRestartHdr`.
    ckpt_seq: u32,
    /// Whether frame checksums in the current WAL use big-endian word order (the
    /// header magic's least-significant bit, `WalIndexHdr.bigEndCksum`). Seeded
    /// from the on-disk header when attaching to an existing WAL; reset to the
    /// *native* byte order whenever a fresh header is written, matching
    /// `sqlite3WalFrames` (`pWal->hdr.bigEndCksum = SQLITE_BIGENDIAN`).
    big_end_cksum: bool,
    /// A monotonic generation bumped on every WAL reset (checkpoint/VACUUM/mode
    /// switch). A reader whose pinned generation differs knows its snapshot is
    /// stale and must re-read the header.
    generation: u64,
    /// Pinned reader marks: one entry per in-flight read transaction that has
    /// snapshotted this index, holding its pinned `mx_frame`. A checkpoint may only
    /// **reset** the WAL (truncate the log, so frames become unreachable) once no
    /// reader is pinned below the checkpointed high-water mark — otherwise a reader
    /// mid-transaction would lose the frames its snapshot still needs. Mirrors
    /// `wal.c`'s `aReadMark[]`. Entries are added at read-txn start and removed at
    /// read-txn end.
    readers: Vec<u32>,
}

impl WalIndex {
    /// A fresh, empty wal-index (no committed frames).
    fn new() -> WalIndex {
        WalIndex {
            log: Vec::new(),
            mx_frame: 0,
            salt: [0; 8],
            next_offset: 0,
            cksum: (0, 0),
            page_size: 0,
            n_backfill: 0,
            ckpt_seq: 0,
            big_end_cksum: NATIVE_BIG_ENDIAN,
            generation: 0,
            readers: Vec::new(),
        }
    }

    /// The current high-water mark: the latest committed frame index. A reader
    /// snapshots this at read-transaction start.
    pub fn mx_frame(&self) -> u32 {
        self.mx_frame
    }

    /// The current WAL generation (bumped on every reset).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The number of frames already backfilled into the main database file
    /// (`WalCkptInfo.nBackfill`).
    pub fn n_backfill(&self) -> u32 {
        self.n_backfill
    }

    /// Record that a checkpoint has copied every frame up to `n` into the main
    /// database file. Only ever advances (a reset returns it to 0), mirroring
    /// `walCheckpoint`'s `AtomicStore(&pInfo->nBackfill, mxSafeFrame)`.
    pub fn set_n_backfill(&mut self, n: u32) {
        if n > self.n_backfill {
            self.n_backfill = n;
        }
    }

    /// The checkpoint sequence number for the next WAL header (`Wal.nCkpt`).
    pub fn ckpt_seq(&self) -> u32 {
        self.ckpt_seq
    }

    /// The salts currently in force (WAL header bytes 16–23).
    pub fn salt(&self) -> [u8; 8] {
        self.salt
    }

    /// Whether the current WAL's checksums use big-endian word order.
    pub fn big_end_cksum(&self) -> bool {
        self.big_end_cksum
    }

    /// Restart the log so the next writer overwrites the WAL file from the
    /// beginning — the port of `wal.c`'s `walRestartHdr`: bump the checkpoint
    /// sequence, increment salt-1 as a big-endian integer, replace salt-2, and
    /// zero `mxFrame`/`nBackfill`. Callers must have confirmed every frame is
    /// backfilled and no reader still resolves pages from the log (sqlite blocks
    /// on the read-slot locks; graphite's callers check the pinned reader marks).
    ///
    /// sqlite draws salt-2 from `sqlite3_randomness`; graphite has no RNG
    /// (`no_std`, zero deps), so it remixes the previous salt-2 with an xorshift
    /// step — different every restart, which is all recovery needs (stale frames
    /// from the previous generation must fail the salt comparison).
    pub fn restart_hdr(&mut self) {
        self.ckpt_seq = self.ckpt_seq.wrapping_add(1);
        let s1 = u32::from_be_bytes([self.salt[0], self.salt[1], self.salt[2], self.salt[3]])
            .wrapping_add(1);
        self.salt[0..4].copy_from_slice(&s1.to_be_bytes());
        let mut s2 = u32::from_be_bytes([self.salt[4], self.salt[5], self.salt[6], self.salt[7]]);
        s2 ^= s2 << 13;
        s2 ^= s2 >> 17;
        s2 ^= s2 << 5;
        s2 = s2.wrapping_add(0x9E37_79B9);
        self.salt[4..8].copy_from_slice(&s2.to_be_bytes());
        self.log.clear();
        self.mx_frame = 0;
        self.n_backfill = 0;
        self.next_offset = 0;
        self.cksum = (0, 0);
        self.big_end_cksum = NATIVE_BIG_ENDIAN;
        self.generation = self.generation.wrapping_add(1);
    }

    /// The pages a checkpoint may copy into the main database file, honoring the
    /// backfill window: for every page touched by a frame in
    /// (`n_backfill`, `mx_frame`], take its **newest** such frame, and emit the
    /// page only if that frame is ≤ `mx_safe`. This is `walCheckpoint`'s
    /// iterator loop with its `iFrame<=nBackfill || iFrame>mxSafeFrame` skip: a
    /// page whose newest frame lies beyond the safe mark is deferred entirely (a
    /// later checkpoint picks it up), never written at an older version.
    pub fn backfill_pages(&self, mx_safe: u32) -> Vec<(u32, Vec<u8>)> {
        let lo = self.n_backfill as usize;
        let hi = (self.mx_frame as usize).min(self.log.len());
        if lo >= hi {
            return Vec::new();
        }
        // Newest frame index (0-based into `log`) per page within (lo, hi].
        let mut newest: BTreeMap<u32, usize> = BTreeMap::new();
        for (i, frame) in self.log[lo..hi].iter().enumerate() {
            newest.insert(frame.page_no, lo + i);
        }
        newest
            .into_iter()
            .filter(|&(_, i)| (i as u32 + 1) <= mx_safe)
            .map(|(page_no, i)| (page_no, self.log[i].data.clone()))
            .collect()
    }

    /// The salts, append offset, checksum, and page size a writer needs to
    /// continue appending to the shared `-wal`. Returns `None` if empty (the
    /// writer then starts a fresh WAL header with its own salts).
    pub fn writer_state(&self) -> Option<WriterState> {
        if self.mx_frame == 0 {
            None
        } else {
            Some((self.next_offset, self.cksum, self.salt, self.page_size))
        }
    }

    /// Whether the shared index currently holds no committed frames.
    pub fn is_empty(&self) -> bool {
        self.mx_frame == 0
    }

    /// Resolve the newest committed version of `page_no` visible at snapshot
    /// `snapshot_mx` (a value obtained from [`mx_frame`](Self::mx_frame) at
    /// read-transaction start). Searches frames newest-first, ignoring frames past
    /// the snapshot; returns the page bytes, or `None` if the page is not in the
    /// WAL up to that snapshot (the reader then falls back to the main file).
    pub fn find_frame(&self, page_no: u32, snapshot_mx: u32) -> Option<Vec<u8>> {
        let hi = (snapshot_mx as usize).min(self.log.len());
        for frame in self.log[..hi].iter().rev() {
            if frame.page_no == page_no {
                return Some(frame.data.clone());
            }
        }
        None
    }

    /// The db size in pages of the snapshot at `snapshot_mx`: the `commit_db_size`
    /// of the newest commit frame at or before the snapshot. `None` if no commit
    /// is visible (the reader then uses the main file's size).
    pub fn snapshot_db_size(&self, snapshot_mx: u32) -> Option<u32> {
        let hi = (snapshot_mx as usize).min(self.log.len());
        for frame in self.log[..hi].iter().rev() {
            if frame.commit_db_size != 0 {
                return Some(frame.commit_db_size);
            }
        }
        None
    }

    /// Reset the index to empty with new salts (checkpoint/VACUUM/mode switch).
    /// Bumps the generation so readers know their snapshots are stale. Does not
    /// touch pinned reader marks — the caller (`checkpoint`) only resets when it
    /// has confirmed no reader is pinned below the checkpointed mark.
    pub fn reset(&mut self, salt: [u8; 8]) {
        self.log.clear();
        self.mx_frame = 0;
        self.salt = salt;
        self.next_offset = 0;
        self.cksum = (0, 0);
        self.page_size = 0;
        self.n_backfill = 0;
        self.ckpt_seq = 0;
        self.big_end_cksum = NATIVE_BIG_ENDIAN;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Register a pinned reader at `mx_frame` (called at read-transaction start).
    pub fn register_reader(&mut self, mx_frame: u32) {
        self.readers.push(mx_frame);
    }

    /// Unregister one pinned reader at `mx_frame` (called at read-transaction end).
    /// Removes a single matching entry.
    pub fn unregister_reader(&mut self, mx_frame: u32) {
        if let Some(pos) = self.readers.iter().position(|&m| m == mx_frame) {
            self.readers.swap_remove(pos);
        }
    }

    /// The smallest `mx_frame` any pinned reader still needs, or `None` if no
    /// reader is currently pinned. A checkpoint may reset the WAL only when this is
    /// `None` (or ≥ the checkpointed mark), so no in-flight reader loses frames.
    pub fn min_reader_mark(&self) -> Option<u32> {
        self.readers.iter().copied().min()
    }

    /// The db size recorded by the last commit in the full log (0 if empty).
    pub fn full_db_size(&self) -> u32 {
        self.log
            .iter()
            .rev()
            .find(|f| f.commit_db_size != 0)
            .map(|f| f.commit_db_size)
            .unwrap_or(0)
    }
}

/// A cloneable handle to a [`WalIndex`] shared by every pager over one file.
///
/// The interior-mutability primitive differs by VFS — `Rc<RefCell>` for the
/// single-threaded in-memory VFS, an `Arc<Mutex>` for the thread-safe std VFS —
/// so this enum abstracts over both behind a uniform [`with`](Self::with) API. It
/// is obtained from a [`File`] via [`File::wal_index`] and lives in the pager's
/// WAL runtime; cloning it shares the same underlying index.
///
/// [`File`]: crate::vfs::File
/// [`File::wal_index`]: crate::vfs::File::wal_index
///
/// The variant is chosen by build configuration, not at runtime: under the `std`
/// feature every handle is the thread-safe `Arc<Mutex>` form, so a
/// [`SharedWalIndex`] (and the `StdFile` that stores one) stays `Send + Sync +
/// RefUnwindSafe`. Under pure `no_std` (where `alloc::sync::Arc` may be
/// unavailable for want of target atomics) it is the single-threaded `Rc<RefCell>`
/// form. The [`new`](Self::new) constructor picks the right one automatically.
#[derive(Clone)]
pub enum SharedWalIndex {
    /// The single-threaded (`no_std`) variant: a process-local `Rc<RefCell>`.
    #[cfg(not(feature = "std"))]
    Local(Rc<RefCell<WalIndex>>),
    /// The thread-safe (`std`) variant: a process-shareable `Arc<Mutex>`.
    #[cfg(feature = "std")]
    Shared(std::sync::Arc<std::sync::Mutex<WalIndex>>),
}

impl SharedWalIndex {
    /// Create a fresh, empty wal-index handle in the form matching the build:
    /// `Arc<Mutex>` under `std`, `Rc<RefCell>` under pure `no_std`.
    pub fn new() -> SharedWalIndex {
        #[cfg(feature = "std")]
        {
            SharedWalIndex::Shared(std::sync::Arc::new(std::sync::Mutex::new(WalIndex::new())))
        }
        #[cfg(not(feature = "std"))]
        {
            SharedWalIndex::Local(Rc::new(RefCell::new(WalIndex::new())))
        }
    }

    /// Run `f` with exclusive access to the underlying [`WalIndex`].
    pub fn with<R>(&self, f: impl FnOnce(&mut WalIndex) -> R) -> R {
        match self {
            #[cfg(not(feature = "std"))]
            SharedWalIndex::Local(rc) => f(&mut rc.borrow_mut()),
            #[cfg(feature = "std")]
            SharedWalIndex::Shared(arc) => f(&mut arc.lock().expect("wal-index mutex poisoned")),
        }
    }
}

impl Default for SharedWalIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// A reader's stable snapshot of the shared wal-index, captured at
/// read-transaction start: the pinned high-water mark and the generation it was
/// taken under. All page resolution during the transaction uses this fixed
/// `mx_frame`, so concurrent appends by a writer are invisible (repeatable read),
/// matching `wal.c`'s per-reader `mxFrame`.
#[derive(Clone, Copy)]
pub struct WalSnapshot {
    /// The pinned high-water mark; frames past this are not visible to the reader.
    pub mx_frame: u32,
    /// The generation this snapshot was taken under; a mismatch means the WAL was
    /// reset (checkpointed) and the snapshot must be refreshed.
    pub generation: u64,
}

impl WalIndex {
    /// Capture a fresh reader snapshot at the current high-water mark.
    pub fn snapshot(&self) -> WalSnapshot {
        WalSnapshot {
            mx_frame: self.mx_frame,
            generation: self.generation,
        }
    }

    /// Append one already-checksummed committed frame to the shared log, advancing
    /// the high-water mark, append offset, and running checksum. Called by the
    /// writer under the write-intent lock after the frame is durable in the
    /// `-wal` file.
    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &mut self,
        page_no: u32,
        commit_db_size: u32,
        salt: [u8; 8],
        data: Vec<u8>,
        next_offset: u64,
        cksum: (u32, u32),
        page_size: u32,
    ) {
        self.log.push(IndexFrame {
            page_no,
            commit_db_size,
            data,
        });
        self.mx_frame = self.log.len() as u32;
        self.salt = salt;
        self.next_offset = next_offset;
        self.cksum = cksum;
        self.page_size = page_size;
    }

    /// Seed the index from frames loaded off an existing `-wal` file on first open
    /// (only when the shared index is still empty). Idempotent races are avoided
    /// by the caller holding the index lock and checking emptiness first.
    /// `ckpt_seq` and `big_end_cksum` come from the parsed WAL header (bytes
    /// 12–15 and the magic's LSB); the backfill mark is unknown without a `-shm`
    /// wal-index, so it conservatively restarts at 0 (a checkpoint then re-copies
    /// frames that may already be in the main file — harmless, the bytes match).
    #[allow(clippy::too_many_arguments)]
    pub fn seed(
        &mut self,
        frames: Vec<(u32, u32, Vec<u8>)>,
        salt: [u8; 8],
        next_offset: u64,
        cksum: (u32, u32),
        page_size: u32,
        ckpt_seq: u32,
        big_end_cksum: bool,
    ) {
        self.log = frames
            .into_iter()
            .map(|(page_no, commit_db_size, data)| IndexFrame {
                page_no,
                commit_db_size,
                data,
            })
            .collect();
        self.mx_frame = self.log.len() as u32;
        self.salt = salt;
        self.next_offset = next_offset;
        self.cksum = cksum;
        self.page_size = page_size;
        self.n_backfill = 0;
        self.ckpt_seq = ckpt_seq;
        self.big_end_cksum = big_end_cksum;
    }
}

/// The WAL running checksum. Consumes `data` (length a multiple of 8) as pairs of
/// 32-bit words in the selected byte order, updating the (s0, s1) accumulators.
pub(crate) fn checksum(big_endian: bool, mut s0: u32, mut s1: u32, data: &[u8]) -> (u32, u32) {
    let read = |at: usize| -> u32 {
        if big_endian {
            u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
        } else {
            u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
        }
    };
    let mut i = 0;
    while i + 8 <= data.len() {
        s0 = s0.wrapping_add(read(i)).wrapping_add(s1);
        s1 = s1.wrapping_add(read(i + 4)).wrapping_add(s0);
        i += 8;
    }
    (s0, s1)
}

// Under `std`, `SharedWalIndex` is stored inside the public `StdFile`, which was
// `Send + Sync + RefUnwindSafe` in 0.1.0. The std variant is `Arc<Mutex<..>>`, so
// the handle must keep those auto-traits — this compile-time check guards against
// a future change (e.g. an `Rc` creeping in) silently regressing them.
#[cfg(all(test, feature = "std"))]
const _: fn() = || {
    fn assert<T: Send + Sync + std::panic::RefUnwindSafe>() {}
    assert::<SharedWalIndex>();
};
