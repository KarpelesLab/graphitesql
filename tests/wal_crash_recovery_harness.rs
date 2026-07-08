//! Roadmap §6 crash-recovery — the **WAL durability path**.
//!
//! The rollback-journal crash harness (`tests/crash_recovery_harness.rs`) covers
//! DELETE-mode journaling. WAL mode is a *different* durability path and is
//! exercised here. graphite commits a WAL transaction by appending frames to the
//! `-wal` file and then issuing a single `sync` (see `WritePager::commit_wal`):
//!
//!   1. **append frames** — one `write_all_at` per dirty page; the *last* frame of
//!      the commit carries the post-commit db-size (the "commit marker");
//!   2. **sync the `-wal`** — after which the transaction is durable.
//!
//! A checkpoint (`PRAGMA wal_checkpoint`, see `WritePager::checkpoint`) folds the
//! committed frames back into the main file and then resets the `-wal`:
//!
//!   1. **write each frame's page into the main db**, then **truncate** the main
//!      file to the committed size and **sync** it;
//!   2. **truncate the `-wal` to 0** and **sync** it (the reset).
//!
//! A crash at *any* instant must leave a state the next open recovers to a
//! consistent database — the transaction is either fully visible or not at all,
//! never torn — and `PRAGMA integrity_check` is `ok`. Recovery in WAL mode is by
//! *replay*: on reopen graphite re-reads the `-wal` up to the last valid **commit
//! frame** (salt + running-checksum validated) and overlays it on the main file.
//!
//! This file builds a [`FaultVfs`] (the same shape as the rollback harness) that
//! makes a chosen file "die" at a chosen I/O op — every later call to that file
//! then errors, freezing the on-disk bytes exactly as a power-loss would, with an
//! optional *torn* leading-half write. We drive a WAL commit (and a checkpoint)
//! through it so it crashes at each phase boundary, reopen with a clean `StdVfs`,
//! and assert:
//!   (a) `integrity_check = ok`;
//!   (b) the visible rows equal EITHER the pre- OR the post-transaction state
//!       (consistent, never a torn blend);
//!   (c) where the crash point makes the outcome determinate (no durable commit
//!       frame ⇒ rolled back; a committed `-wal` ⇒ replayed), the exact state.
//! Where `sqlite3` is on `PATH` we additionally have it open the recovered file
//! and confirm `integrity_check = ok` and row agreement, skipping when absent.

#![cfg(feature = "std")]

use graphitesql::vfs::std_file::StdVfs;
use graphitesql::vfs::{File, LockLevel, OpenFlags, Vfs};
use graphitesql::{Connection, Value};
use std::cell::Cell;
use std::process::Command;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Which file an injected fault targets, and which operation trips it.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    /// The main database file (path == the db path, no suffix).
    Db,
    /// The write-ahead log (`<path>-wal`).
    Wal,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Write,
    Truncate,
    Sync,
}

/// A shared fault plan: "the Nth `op` on a `target` file fails, after which that
/// file is dead (every later op on it errors)". `None` arms no fault.
#[derive(Clone)]
struct Plan {
    target: Target,
    op: Op,
    /// 1-based ordinal of the matching op that trips the fault.
    nth: u32,
    /// How the trip manifests: a hard error (the usual crash) or, for a write, a
    /// *short* write that lands the leading half of the buffer then dies — a torn
    /// frame/sector.
    torn: bool,
}

/// Mutable run-state shared between the VFS and its files.
#[derive(Default)]
struct State {
    /// Count of matching ops seen so far (across all handles to the target).
    seen: Cell<u32>,
    /// Set once the fault has tripped: the target file is now dead.
    dead: Cell<bool>,
}

// ---------------------------------------------------------------------------
// The fault-injecting VFS.
// ---------------------------------------------------------------------------

struct FaultVfs {
    inner: StdVfs,
    plan: Option<Plan>,
    state: Rc<State>,
    /// The bare database path, so we can classify a handle as Db vs Wal.
    db_path: String,
}

impl FaultVfs {
    fn new(db_path: &str, plan: Option<Plan>) -> FaultVfs {
        FaultVfs {
            inner: StdVfs::new(),
            plan,
            state: Rc::new(State::default()),
            db_path: db_path.to_string(),
        }
    }

    fn classify(&self, path: &str) -> Option<Target> {
        if path == self.db_path {
            Some(Target::Db)
        } else if path == format!("{}-wal", self.db_path) {
            Some(Target::Wal)
        } else {
            None
        }
    }
}

impl Vfs for FaultVfs {
    fn open(&self, path: &str, flags: OpenFlags) -> graphitesql::Result<Box<dyn File>> {
        let f = self.inner.open(path, flags)?;
        match (self.classify(path), &self.plan) {
            (Some(target), Some(plan)) if plan.target == target => Ok(Box::new(FaultFile {
                inner: f,
                plan: plan.clone(),
                state: Rc::clone(&self.state),
            })),
            _ => Ok(f),
        }
    }
    fn delete(&self, path: &str) -> graphitesql::Result<()> {
        // graphite never deletes the -wal (it truncates it to reset), so there is
        // no hot-file-to-preserve quirk here; pass deletes straight through.
        self.inner.delete(path)
    }
    fn exists(&self, path: &str) -> graphitesql::Result<bool> {
        self.inner.exists(path)
    }
}

struct FaultFile {
    inner: Box<dyn File>,
    plan: Plan,
    state: Rc<State>,
}

impl FaultFile {
    /// Account for one op of kind `op`. Returns `Err` once this op (or a previous
    /// one) should crash the file.
    fn gate(&self, op: Op) -> graphitesql::Result<()> {
        if self.state.dead.get() {
            // The crash already happened; nothing more reaches the disk.
            return Err(graphitesql::Error::Io(
                "fault-injected crash (file dead)".into(),
            ));
        }
        if op == self.plan.op {
            let n = self.state.seen.get() + 1;
            self.state.seen.set(n);
            if n == self.plan.nth {
                self.state.dead.set(true);
                return Err(graphitesql::Error::Io("fault-injected crash".into()));
            }
        }
        Ok(())
    }
}

impl File for FaultFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> graphitesql::Result<()> {
        if self.state.dead.get() {
            return Err(graphitesql::Error::Io(
                "fault-injected crash (file dead)".into(),
            ));
        }
        self.inner.read_exact_at(buf, offset)
    }
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> graphitesql::Result<()> {
        // For a *torn* write we first land the leading half, then die — modelling
        // a frame/sector that was half-flushed when the power dropped.
        if !self.state.dead.get() && self.plan.op == Op::Write {
            let n = self.state.seen.get() + 1;
            if n == self.plan.nth && self.plan.torn && buf.len() > 1 {
                let half = buf.len() / 2;
                self.inner.write_all_at(&buf[..half], offset)?;
            }
        }
        self.gate(Op::Write)?;
        self.inner.write_all_at(buf, offset)
    }
    fn truncate(&mut self, size: u64) -> graphitesql::Result<()> {
        self.gate(Op::Truncate)?;
        self.inner.truncate(size)
    }
    fn sync(&mut self) -> graphitesql::Result<()> {
        self.gate(Op::Sync)?;
        self.inner.sync()
    }
    fn size(&self) -> graphitesql::Result<u64> {
        if self.state.dead.get() {
            return Err(graphitesql::Error::Io(
                "fault-injected crash (file dead)".into(),
            ));
        }
        self.inner.size()
    }
    fn lock(&mut self, level: LockLevel) -> graphitesql::Result<()> {
        // Locking is process-local bookkeeping, not disk state: let it through even
        // after the crash so the writer's teardown does not panic.
        self.inner.lock(level)
    }
    fn unlock(&mut self, level: LockLevel) -> graphitesql::Result<()> {
        self.inner.unlock(level)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn have_sqlite3() -> bool {
    Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

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

/// A unique per-PID scratch directory; cleaned up on drop (incl. -wal/-shm/-journal
/// sidecars, since they live inside the directory).
struct Scratch {
    dir: std::path::PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir()
            .join(format!("gsql-walh-{}", std::process::id()))
            .join(tag);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch { dir }
    }
    fn path(&self, name: &str) -> String {
        self.dir.join(name).to_str().unwrap().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Whether the connection currently operates in the WAL write path. graphite's
/// `PRAGMA journal_mode` reports `wal` only when the live backend is in WAL mode
/// (a header marked WAL but an empty `-wal` reopens as a rollback-journal db and
/// reports `delete`), so this is the public probe for "the WAL path is active".
fn wal_active(conn: &Connection) -> bool {
    rows(conn, "PRAGMA journal_mode")
        .first()
        .and_then(|r| r.first())
        .map(|m| m.eq_ignore_ascii_case("wal"))
        .unwrap_or(false)
}

/// Run a graphite query and return rows as `Vec<Vec<String>>`.
fn rows(conn: &Connection, sql: &str) -> Vec<Vec<String>> {
    let res = conn.query(sql).unwrap();
    res.rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    Value::Integer(i) => i.to_string(),
                    Value::Real(f) => f.to_string(),
                    Value::Text(s) => s.to_string(),
                    Value::Blob(b) => format!("blob:{}", b.len()),
                })
                .collect()
        })
        .collect()
}

/// Build a known database, switch it to WAL mode, and warm the `-wal` with one
/// committed frame so that on the *next* open graphite sees committed WAL frames
/// and operates in the WAL write path (a header-only WAL with no frames reopens
/// as a plain rollback-journal database). Returns the pre-transaction rows.
fn build_wal_db(db: &str, page_size: u32) -> Vec<Vec<String>> {
    let vfs = StdVfs::new();
    let mut c = Connection::create_vfs(&vfs, db, page_size).unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES (1,'one'),(2,'two'),(3,'three')")
        .unwrap();
    c.execute("PRAGMA journal_mode=WAL").unwrap();
    // Warm-up WAL commit: leaves committed frames in the -wal so the reopen below
    // (and in the crash tests) lands in WAL mode. A no-op-looking UPDATE still
    // dirties and re-journals the touched pages.
    c.execute("UPDATE t SET b=b").unwrap();
    let before = rows(&c, "SELECT a,b FROM t ORDER BY a");
    drop(c);
    before
}

/// The DML every crash test runs inside one WAL transaction. Chosen so the commit
/// appends several frames and changes visibly.
const TXN: &str = "UPDATE t SET b='MUTATED'; INSERT INTO t VALUES (4,'four'),(5,'five')";

/// The post-commit rows the transaction would produce if it committed cleanly.
fn expected_after() -> Vec<Vec<String>> {
    vec![
        vec!["1".into(), "MUTATED".into()],
        vec!["2".into(), "MUTATED".into()],
        vec!["3".into(), "MUTATED".into()],
        vec!["4".into(), "four".into()],
        vec!["5".into(), "five".into()],
    ]
}

/// Open `db` in WAL mode through a `FaultVfs` armed with `plan`, run `TXN` inside
/// one transaction, expecting the injected crash to fail the commit. Asserts the
/// commit failed (so the crash actually landed where intended).
fn crash_during_wal_commit(db: &str, plan: Plan) {
    let vfs = FaultVfs::new(db, Some(plan));
    let mut conn = Connection::open_vfs(&vfs, db).unwrap();
    assert!(wal_active(&conn), "test db should reopen in WAL mode");
    let r = conn.execute_batch(&format!("BEGIN;\n{TXN};\nCOMMIT;"));
    assert!(
        r.is_err(),
        "the injected fault should have failed the WAL commit, got Ok"
    );
    drop(conn);
}

/// Open `db` in WAL mode, run `TXN` (which must succeed), then drive a CHECKPOINT
/// through a `FaultVfs` armed with `plan` (expected to fail). The committed WAL is
/// already durable before the checkpoint, so recovery must yield the post-state.
fn crash_during_checkpoint(db: &str, plan: Plan) {
    // First, a clean WAL commit through an unfaulted VFS so the -wal holds the
    // committed transaction.
    {
        let vfs = FaultVfs::new(db, None);
        let mut conn = Connection::open_vfs(&vfs, db).unwrap();
        assert!(wal_active(&conn), "test db should reopen in WAL mode");
        conn.execute_batch(&format!("BEGIN;\n{TXN};\nCOMMIT;"))
            .unwrap();
        drop(conn);
    }
    // Now reopen with the fault armed and crash mid-checkpoint.
    let vfs = FaultVfs::new(db, Some(plan));
    let mut conn = Connection::open_vfs(&vfs, db).unwrap();
    assert!(wal_active(&conn), "checkpoint test db is WAL mode");
    let r = conn.execute("PRAGMA wal_checkpoint");
    assert!(
        r.is_err(),
        "the injected fault should have failed the checkpoint, got Ok"
    );
    drop(conn);
}

/// Reopen `db` with a clean `StdVfs`, assert integrity is ok, and return the
/// recovered rows (WAL replay happens transparently on open).
fn reopen_clean(db: &str) -> Vec<Vec<String>> {
    let vfs = StdVfs::new();
    let c = Connection::open_vfs(&vfs, db).unwrap();
    assert_eq!(
        rows(&c, "PRAGMA integrity_check"),
        vec![vec!["ok".to_string()]],
        "recovered database fails integrity_check"
    );
    rows(&c, "SELECT a,b FROM t ORDER BY a")
}

/// Reopen `db`, fold any committed `-wal` into the main file via a clean
/// checkpoint, integrity-check, and return the recovered rows. Used before the
/// `sqlite3` cross-check so the real CLI reads the committed state from the main
/// file regardless of WAL/`-shm` interop.
fn reopen_finalize(db: &str) -> Vec<Vec<String>> {
    let vfs = StdVfs::new();

    {
        let mut c = Connection::open_vfs(&vfs, db).unwrap();
        assert_eq!(
            rows(&c, "PRAGMA integrity_check"),
            vec![vec!["ok".to_string()]],
            "recovered database fails integrity_check"
        );
        let got = rows(&c, "SELECT a,b FROM t ORDER BY a");
        // Fold the WAL back into the main file so sqlite3 reads it without needing
        // to replay graphite's WAL (and so the -wal can be removed).
        let _ = c.execute("PRAGMA wal_checkpoint");
        got
    }
}

/// Assert the recovered rows are a *consistent* snapshot: exactly the pre- or
/// exactly the post-transaction state, never a torn blend of the two.
fn assert_consistent(got: &[Vec<String>], before: &[Vec<String>], after: &[Vec<String>]) {
    let is_before = got == before;
    let is_after = got == after;
    assert!(
        is_before || is_after,
        "torn state after recovery:\n got    = {got:?}\n before = {before:?}\n after  = {after:?}"
    );
}

/// Cross-check the recovered file with `sqlite3` (integrity + row agreement),
/// skipping when sqlite3 is unavailable. The caller must have finalized the WAL
/// (folded it into the main file and removed/emptied the `-wal`) first.
fn cross_check_sqlite3(db: &str, recovered: &[Vec<String>]) {
    if !have_sqlite3() {
        eprintln!("skipping sqlite3 cross-check: not on PATH");
        return;
    }
    // Remove any leftover empty -wal/-shm so sqlite3 reads purely the main file.
    let _ = std::fs::remove_file(format!("{db}-wal"));
    let _ = std::fs::remove_file(format!("{db}-shm"));
    assert_eq!(
        sqlite3(db, "PRAGMA integrity_check"),
        "ok",
        "sqlite3 disagrees: recovered db fails integrity_check"
    );
    let s = sqlite3(db, "SELECT a||'|'||b FROM t ORDER BY a");
    let flat: Vec<String> = recovered
        .iter()
        .map(|r| format!("{}|{}", r[0], r[1]))
        .collect();
    assert_eq!(
        s,
        flat.join("\n"),
        "sqlite3 reads different rows than graphite from the recovered db"
    );
}

// ===========================================================================
// WAL COMMIT injection points
// ===========================================================================

// ---------------------------------------------------------------------------
// A — crash on the FIRST WAL frame write (before the commit frame, before sync).
// No durable commit frame for this txn exists, so replay stops at the previous
// commit: determinate ROLL BACK to the pre-transaction state.
// ---------------------------------------------------------------------------

#[test]
fn wal_crash_first_frame_rolls_back() {
    let scratch = Scratch::new("wal_first_frame");
    let db = scratch.path("t.db");
    let before = build_wal_db(&db, 4096);
    let after = expected_after();

    crash_during_wal_commit(
        &db,
        Plan {
            target: Target::Wal,
            op: Op::Write,
            nth: 1,
            torn: false,
        },
    );

    let got = reopen_clean(&db);
    assert_eq!(
        got, before,
        "a crash before the commit frame is durable must not show the txn"
    );
    assert_consistent(&got, &before, &after);
    let finalized = reopen_finalize(&db);
    assert_eq!(finalized, before);
    cross_check_sqlite3(&db, &finalized);
}

// ---------------------------------------------------------------------------
// A' — a TORN first-frame write: the leading half lands, then the file dies.
// Still no valid commit frame ⇒ replay rolls back to the pre-state, and the
// partial frame's broken checksum must not corrupt the replay.
// ---------------------------------------------------------------------------

#[test]
fn wal_crash_torn_first_frame_rolls_back() {
    let scratch = Scratch::new("wal_torn_first");
    let db = scratch.path("t.db");
    let before = build_wal_db(&db, 4096);
    let after = expected_after();

    crash_during_wal_commit(
        &db,
        Plan {
            target: Target::Wal,
            op: Op::Write,
            nth: 1,
            torn: true,
        },
    );

    let got = reopen_clean(&db);
    assert_eq!(got, before, "a torn first frame must roll back");
    assert_consistent(&got, &before, &after);
    let finalized = reopen_finalize(&db);
    assert_eq!(finalized, before);
    cross_check_sqlite3(&db, &finalized);
}

// ---------------------------------------------------------------------------
// B — crash on the COMMIT frame write (the 2nd/last frame of the commit dies).
// The earlier frame(s) are on disk but the commit marker never lands, so replay
// stops at the previous commit: determinate ROLL BACK.
// ---------------------------------------------------------------------------

#[test]
fn wal_crash_commit_frame_write_rolls_back() {
    let scratch = Scratch::new("wal_commit_frame");
    let db = scratch.path("t.db");
    let before = build_wal_db(&db, 4096);
    let after = expected_after();

    crash_during_wal_commit(
        &db,
        Plan {
            target: Target::Wal,
            op: Op::Write,
            nth: 2,
            torn: false,
        },
    );

    let got = reopen_clean(&db);
    assert_eq!(
        got, before,
        "no durable commit frame ⇒ the partial txn rolls back"
    );
    assert_consistent(&got, &before, &after);
    let finalized = reopen_finalize(&db);
    assert_eq!(finalized, before);
    cross_check_sqlite3(&db, &finalized);
}

// ---------------------------------------------------------------------------
// B' — a TORN commit-frame write: half the final frame lands, then death. Its
// running checksum cannot validate, so replay must reject it and roll back.
// ---------------------------------------------------------------------------

#[test]
fn wal_crash_torn_commit_frame_rolls_back() {
    let scratch = Scratch::new("wal_torn_commit");
    let db = scratch.path("t.db");
    let before = build_wal_db(&db, 4096);
    let after = expected_after();

    crash_during_wal_commit(
        &db,
        Plan {
            target: Target::Wal,
            op: Op::Write,
            nth: 2,
            torn: true,
        },
    );

    let got = reopen_clean(&db);
    assert_eq!(got, before, "a torn commit frame must roll back");
    assert_consistent(&got, &before, &after);
    let finalized = reopen_finalize(&db);
    assert_eq!(finalized, before);
    cross_check_sqlite3(&db, &finalized);
}

// ---------------------------------------------------------------------------
// C — crash on the WAL `sync` (all frames, including the commit frame, were
// physically written by `write_all_at`, but the flush that makes them durable
// never returned). This is the genuinely AMBIGUOUS power-loss case: the bytes
// may or may not have reached the platter. Our FaultFile freezes whatever was
// written, so here the frames are present and checksum-valid ⇒ recovery REPLAYS
// them (the post-state). Either outcome is consistent; we assert consistency and,
// since the bytes are present, the post-state specifically.
// ---------------------------------------------------------------------------

#[test]
fn wal_crash_on_commit_sync_is_consistent() {
    let scratch = Scratch::new("wal_commit_sync");
    let db = scratch.path("t.db");
    let before = build_wal_db(&db, 4096);
    let after = expected_after();

    crash_during_wal_commit(
        &db,
        Plan {
            target: Target::Wal,
            op: Op::Sync,
            nth: 1,
            torn: false,
        },
    );

    let got = reopen_clean(&db);
    assert_consistent(&got, &before, &after);
    // The commit frame's bytes landed before the (failed) sync, so they replay.
    assert_eq!(
        got, after,
        "frames written before the failed sync replay as committed"
    );
    let finalized = reopen_finalize(&db);
    assert_eq!(finalized, after);
    cross_check_sqlite3(&db, &finalized);
}

// ===========================================================================
// CHECKPOINT injection points (the committed WAL is durable beforehand)
// ===========================================================================

// ---------------------------------------------------------------------------
// D — crash MIDWAY through a checkpoint writing pages back into the main db (the
// 1st main-file page write dies). The committed `-wal` is untouched (its reset
// truncate happens only after the main sync), so reopen REPLAYS the WAL over the
// partially-written main file: determinate POST-state, never torn.
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_crash_first_db_write_replays_wal() {
    let scratch = Scratch::new("ckpt_first_db");
    let db = scratch.path("t.db");
    let before = build_wal_db(&db, 4096);
    let after = expected_after();

    crash_during_checkpoint(
        &db,
        Plan {
            target: Target::Db,
            op: Op::Write,
            nth: 1,
            torn: false,
        },
    );

    let got = reopen_clean(&db);
    assert_eq!(
        got, after,
        "a partial checkpoint is masked by replaying the still-present WAL"
    );
    assert_consistent(&got, &before, &after);
    let finalized = reopen_finalize(&db);
    assert_eq!(finalized, after);
    cross_check_sqlite3(&db, &finalized);
}

// ---------------------------------------------------------------------------
// D' — a TORN checkpoint page write: half a main-db page lands, then death. The
// torn main page must be masked by the intact WAL frame on reopen.
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_crash_torn_db_write_replays_wal() {
    let scratch = Scratch::new("ckpt_torn_db");
    let db = scratch.path("t.db");
    let before = build_wal_db(&db, 4096);
    let after = expected_after();

    crash_during_checkpoint(
        &db,
        Plan {
            target: Target::Db,
            op: Op::Write,
            nth: 1,
            torn: true,
        },
    );

    let got = reopen_clean(&db);
    assert_eq!(got, after, "a torn checkpoint page is masked by the WAL");
    assert_consistent(&got, &before, &after);
    let finalized = reopen_finalize(&db);
    assert_eq!(finalized, after);
    cross_check_sqlite3(&db, &finalized);
}

// ---------------------------------------------------------------------------
// E — crash on the main-db `sync` of the checkpoint (pages written, flush did
// not return). The `-wal` is still intact, so reopen replays it: POST-state.
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_crash_on_db_sync_replays_wal() {
    let scratch = Scratch::new("ckpt_db_sync");
    let db = scratch.path("t.db");
    let before = build_wal_db(&db, 4096);
    let after = expected_after();

    crash_during_checkpoint(
        &db,
        Plan {
            target: Target::Db,
            op: Op::Sync,
            nth: 1,
            torn: false,
        },
    );

    let got = reopen_clean(&db);
    assert_eq!(
        got, after,
        "WAL still present ⇒ replay yields the post-state"
    );
    assert_consistent(&got, &before, &after);
    let finalized = reopen_finalize(&db);
    assert_eq!(finalized, after);
    cross_check_sqlite3(&db, &finalized);
}

// ---------------------------------------------------------------------------
// F — crash on the `-wal` RESET truncate at the end of a checkpoint. The main db
// already holds the fully-checkpointed, synced pages; the `-wal` reset never
// completed, so the (now-redundant) committed frames are still present. Replay
// re-applies the same bytes that are already in the main file ⇒ POST-state,
// consistent and idempotent.
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_crash_on_wal_reset_is_consistent() {
    let scratch = Scratch::new("ckpt_wal_reset");
    let db = scratch.path("t.db");
    let before = build_wal_db(&db, 4096);
    let after = expected_after();

    crash_during_checkpoint(
        &db,
        Plan {
            target: Target::Wal,
            op: Op::Truncate,
            nth: 1,
            torn: false,
        },
    );

    let got = reopen_clean(&db);
    assert_eq!(
        got, after,
        "checkpoint applied to main; redundant WAL replay is idempotent"
    );
    assert_consistent(&got, &before, &after);
    let finalized = reopen_finalize(&db);
    assert_eq!(finalized, after);
    cross_check_sqlite3(&db, &finalized);
}

// ===========================================================================
// Controls
// ===========================================================================

// ---------------------------------------------------------------------------
// Control 1 — a clean WAL commit with NO fault: the new rows are present, the db
// is healthy, and the committed data lives in the `-wal` (uncheckpointed). Pins
// that the harness's transaction really changes state in the WAL path.
// ---------------------------------------------------------------------------

#[test]
fn clean_wal_commit_applies_and_is_healthy() {
    let scratch = Scratch::new("clean_commit");
    let db = scratch.path("t.db");
    let _before = build_wal_db(&db, 4096);
    let after = expected_after();

    {
        let vfs = FaultVfs::new(&db, None); // armed with no fault
        let mut conn = Connection::open_vfs(&vfs, &db).unwrap();
        assert!(wal_active(&conn), "control db is WAL mode");
        conn.execute_batch(&format!("BEGIN;\n{TXN};\nCOMMIT;"))
            .unwrap();
        drop(conn);
    }
    // The committed data is in the -wal; the main file alone does not yet have it.
    assert!(
        std::fs::metadata(format!("{db}-wal"))
            .map(|m| m.len() > 32)
            .unwrap_or(false),
        "a WAL commit leaves frames in the -wal"
    );

    let got = reopen_clean(&db);
    assert_eq!(got, after, "clean WAL commit applied the new rows");
    let finalized = reopen_finalize(&db);
    assert_eq!(finalized, after);
    cross_check_sqlite3(&db, &finalized);
}

// ---------------------------------------------------------------------------
// Control 2 — a clean checkpoint with NO fault: the committed WAL is folded into
// the main file, the `-wal` is reset, and the post-state survives a reopen.
// ---------------------------------------------------------------------------

#[test]
fn clean_checkpoint_folds_into_main() {
    let scratch = Scratch::new("clean_ckpt");
    let db = scratch.path("t.db");
    let _before = build_wal_db(&db, 4096);
    let after = expected_after();

    {
        let vfs = FaultVfs::new(&db, None);
        let mut conn = Connection::open_vfs(&vfs, &db).unwrap();
        conn.execute_batch(&format!("BEGIN;\n{TXN};\nCOMMIT;"))
            .unwrap();
        conn.execute("PRAGMA wal_checkpoint").unwrap();
        drop(conn);
    }
    // After a clean checkpoint the -wal is reset to empty (header-only or 0).
    assert!(
        std::fs::metadata(format!("{db}-wal"))
            .map(|m| m.len() <= 32)
            .unwrap_or(true),
        "a clean checkpoint resets the -wal"
    );

    let got = reopen_clean(&db);
    assert_eq!(got, after, "checkpoint folded the txn into the main file");
    cross_check_sqlite3(&db, &got);
}

// ---------------------------------------------------------------------------
// Sweep — across every WAL-frame write ordinal and torn/clean, a crash mid-commit
// must reopen consistent and integrity-ok. The TXN dirties two pages, so a commit
// appends two frames; crash on either, clean or torn. Every pre-commit-frame crash
// is determinate: rolled back.
// ---------------------------------------------------------------------------

#[test]
fn sweep_wal_commit_crash_points_stay_consistent() {
    let after = expected_after();
    for nth in 1u32..=2 {
        for torn in [false, true] {
            let scratch = Scratch::new(&format!("sweep_{nth}_{torn}"));
            let db = scratch.path("t.db");
            let before = build_wal_db(&db, 4096);

            crash_during_wal_commit(
                &db,
                Plan {
                    target: Target::Wal,
                    op: Op::Write,
                    nth,
                    torn,
                },
            );
            let got = reopen_clean(&db);
            assert_consistent(&got, &before, &after);
            // No commit frame is durable for either ordinal ⇒ roll back.
            assert_eq!(
                got, before,
                "wal-commit crash nth={nth} torn={torn} should roll back"
            );
        }
    }
}
