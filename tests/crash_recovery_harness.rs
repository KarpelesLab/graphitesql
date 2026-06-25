//! Roadmap C7-harness / §6 crash-recovery: a fault-injecting `Vfs` plus a
//! suite asserting graphite's core durability invariant.
//!
//! graphite commits a rollback-journal transaction in three ordered phases
//! (see `WritePager::commit`):
//!
//!   1. **write the journal** — save the originals of every page about to be
//!      overwritten, sync the records, publish the record count, sync again;
//!   2. **write the database** — overwrite each dirty page, truncate the file to
//!      the new size, sync the database file;
//!   3. **clear the journal** — `truncate(0)` + sync, after which the commit is
//!      durable and no hot journal remains.
//!
//! A crash at *any* instant must leave a state the next open can recover to a
//! consistent database: the transaction is either fully applied or fully rolled
//! back, never torn, and `PRAGMA integrity_check` is `ok`.
//!
//! This file builds a [`FaultVfs`] that delegates to [`StdVfs`] but can make a
//! chosen file "die" at a chosen I/O operation — every later call to that file
//! then fails, freezing the on-disk bytes exactly as a power-loss would. We
//! drive a transaction through it so it crashes at one of the three phase
//! boundaries (and mid-phase), reopen with a clean `StdVfs`, and assert:
//!   (a) `integrity_check = ok`;
//!   (b) the visible rows equal EITHER the pre- OR the post-transaction state
//!       (consistent, not a torn mix);
//!   (c) where the crash point makes the outcome determinate (any crash before
//!       the journal is cleared rolls back), the exact pre-transaction state.
//! Where `sqlite3` is on `PATH` we additionally have it open the recovered file
//! and confirm `integrity_check = ok`, skipping gracefully when absent.

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
    /// The rollback journal (`<path>-journal`).
    Journal,
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
    /// How the trip manifests: a hard error (the usual crash) or, for a write,
    /// a *short* write that lands the first half of the buffer then dies — a
    /// torn sector.
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
    /// The bare database path, so we can classify a handle as Db vs Journal.
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
        } else if path == format!("{}-journal", self.db_path) {
            Some(Target::Journal)
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
        // Once the journal is dead, a crash froze it on disk: refuse to delete it
        // so the next open sees the hot journal exactly as the crash left it.
        if self.classify(path) == Some(Target::Journal)
            && self.plan.as_ref().map(|p| p.target) == Some(Target::Journal)
            && self.state.dead.get()
        {
            return Ok(());
        }
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
        // a sector that was half-flushed when the power dropped.
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
        // Locking is process-local bookkeeping, not disk state: let it through
        // even after the crash so the writer's teardown does not panic.
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

/// A unique per-PID scratch directory; cleaned up on drop.
struct Scratch {
    dir: std::path::PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir()
            .join(format!("gsql-c7h-{}", std::process::id()))
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

/// Build a known database via a *clean* `StdVfs` and return its starting rows.
fn build_db(db: &str, page_size: u32) -> Vec<Vec<String>> {
    let vfs = StdVfs::new();
    let mut c = Connection::create_vfs(&vfs, db, page_size).unwrap();
    c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES (1,'one'),(2,'two'),(3,'three')")
        .unwrap();
    let before = rows(&c, "SELECT a,b FROM t ORDER BY a");
    drop(c);
    before
}

/// The DML every crash test runs inside one transaction. Chosen so the commit
/// touches several pages and changes visibly.
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

/// Drive `TXN` through a `FaultVfs` armed with `plan`; the commit is expected to
/// fail (the injected crash). Returns whether a hot journal remains on disk.
fn crash_during_commit(db: &str, plan: Plan) -> bool {
    let vfs = FaultVfs::new(db, Some(plan));
    {
        let mut conn = Connection::open_vfs(&vfs, db).unwrap();
        // The crash surfaces as an Err out of the batched commit. We do not care
        // *where* exactly the error pops; we care about the on-disk aftermath.
        let r = conn.execute_batch(&format!("BEGIN;\n{TXN};\nCOMMIT;"));
        assert!(
            r.is_err(),
            "the injected fault should have failed the commit, got Ok"
        );
        drop(conn);
    }
    std::fs::metadata(format!("{db}-journal"))
        .map(|m| m.len() >= 512)
        .unwrap_or(false)
}

/// Reopen `db` with a clean `StdVfs`, assert integrity is ok, and return the
/// recovered rows.
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
/// skipping when sqlite3 is unavailable.
fn cross_check_sqlite3(db: &str, recovered: &[Vec<String>]) {
    if !have_sqlite3() {
        eprintln!("skipping sqlite3 cross-check: not on PATH");
        return;
    }
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

// ---------------------------------------------------------------------------
// Injection point A — crash AFTER the journal is fully written & synced, on the
// FIRST database-page write. The db file is untouched, the journal is hot, so
// recovery is *determinate*: roll back to the pre-transaction state.
// ---------------------------------------------------------------------------

#[test]
fn crash_before_db_write_rolls_back() {
    let scratch = Scratch::new("before_db_write");
    let db = scratch.path("t.db");
    let before = build_db(&db, 4096);
    let after = expected_after();

    let hot = crash_during_commit(
        &db,
        Plan {
            target: Target::Db,
            op: Op::Write,
            nth: 1,
            torn: false,
        },
    );
    assert!(
        hot,
        "journal should be hot: it was written before the db write"
    );

    let got = reopen_clean(&db);
    assert_eq!(
        got, before,
        "a crash before any db page was written must roll fully back"
    );
    assert_consistent(&got, &before, &after);
    cross_check_sqlite3(&db, &got);
}

// ---------------------------------------------------------------------------
// Injection point B — crash MIDWAY through overwriting the db pages (the 2nd
// page write dies, and as the file is left partially mutated the journal stays
// hot). Reopen must replay the hot journal and restore the pre-state.
// ---------------------------------------------------------------------------

#[test]
fn crash_midway_db_write_replays_journal() {
    let scratch = Scratch::new("midway_db_write");
    let db = scratch.path("t.db");
    let before = build_db(&db, 4096);
    let after = expected_after();

    let hot = crash_during_commit(
        &db,
        Plan {
            target: Target::Db,
            op: Op::Write,
            nth: 2,
            torn: false,
        },
    );
    assert!(hot, "journal hot: crash happened before it was cleared");

    let got = reopen_clean(&db);
    assert_eq!(
        got, before,
        "a partial db write must be undone by replaying the hot journal"
    );
    assert_consistent(&got, &before, &after);
    cross_check_sqlite3(&db, &got);
}

// ---------------------------------------------------------------------------
// Injection point B' — a TORN db-page write: the first page write lands only
// its leading half, then dies. The journal is hot; recovery must overwrite the
// torn page from the journal original.
// ---------------------------------------------------------------------------

#[test]
fn crash_torn_db_write_replays_journal() {
    let scratch = Scratch::new("torn_db_write");
    let db = scratch.path("t.db");
    let before = build_db(&db, 4096);
    let after = expected_after();

    let hot = crash_during_commit(
        &db,
        Plan {
            target: Target::Db,
            op: Op::Write,
            nth: 1,
            torn: true,
        },
    );
    assert!(hot, "journal hot after a torn db write");

    let got = reopen_clean(&db);
    assert_eq!(
        got, before,
        "a torn db page must be restored from the journal"
    );
    assert_consistent(&got, &before, &after);
    cross_check_sqlite3(&db, &got);
}

// ---------------------------------------------------------------------------
// Injection point C — crash on the db `sync` (all page writes landed, but the
// flush that makes them durable never completed). The journal is still hot, so
// recovery rolls the (possibly buffered) changes back to the pre-state.
// ---------------------------------------------------------------------------

#[test]
fn crash_on_db_sync_rolls_back() {
    let scratch = Scratch::new("db_sync");
    let db = scratch.path("t.db");
    let before = build_db(&db, 4096);
    let after = expected_after();

    let hot = crash_during_commit(
        &db,
        Plan {
            target: Target::Db,
            op: Op::Sync,
            nth: 1,
            torn: false,
        },
    );
    assert!(
        hot,
        "journal hot: db sync failed before the journal was cleared"
    );

    let got = reopen_clean(&db);
    assert_consistent(&got, &before, &after);
    // The journal still describes the originals, so recovery rolls back.
    assert_eq!(
        got, before,
        "a crash before the journal is cleared rolls back to the pre-state"
    );
    cross_check_sqlite3(&db, &got);
}

// ---------------------------------------------------------------------------
// Injection point D — crash AFTER the db pages are written & synced (durable),
// but on the journal `truncate(0)` that would finalize the commit. The journal
// remains hot; the next open sees a hot journal over a fully-mutated db and must
// be CONSISTENT. Because the hot journal still holds the originals, recovery
// rolls the db back to the pre-state (committed-or-rolledback: rolled back).
// ---------------------------------------------------------------------------

#[test]
fn crash_on_journal_clear_is_consistent() {
    let scratch = Scratch::new("journal_clear");
    let db = scratch.path("t.db");
    let before = build_db(&db, 4096);
    let after = expected_after();

    // The journal is truncated TWICE per commit: first at the top of
    // `write_journal` (to clear any stale journal), then again at the very end
    // to finalize the commit. We want the *second* one — the finalizing clear —
    // so the records written in between survive on disk as a hot journal.
    let hot = crash_during_commit(
        &db,
        Plan {
            target: Target::Journal,
            op: Op::Truncate,
            nth: 2,
            torn: false,
        },
    );
    assert!(
        hot,
        "the journal-clear truncate was suppressed, so the journal stays hot"
    );

    let got = reopen_clean(&db);
    // Determinate: the hot journal's originals win, rolling the db back.
    assert_eq!(
        got, before,
        "hot journal over a written db rolls back to the originals"
    );
    assert_consistent(&got, &before, &after);
    cross_check_sqlite3(&db, &got);
}

// ---------------------------------------------------------------------------
// Control — a clean commit with NO fault: the new rows are present and the db is
// healthy. This pins that the harness's transaction really does change state, so
// the crash assertions above are meaningful.
// ---------------------------------------------------------------------------

#[test]
fn clean_commit_applies_and_is_healthy() {
    let scratch = Scratch::new("clean");
    let db = scratch.path("t.db");
    let _before = build_db(&db, 4096);
    let after = expected_after();

    {
        let vfs = FaultVfs::new(&db, None); // armed with no fault
        let mut conn = Connection::open_vfs(&vfs, &db).unwrap();
        conn.execute_batch(&format!("BEGIN;\n{TXN};\nCOMMIT;"))
            .unwrap();
        drop(conn);
    }
    // No hot journal should survive a clean commit.
    assert!(
        std::fs::metadata(format!("{db}-journal"))
            .map(|m| m.len() < 512)
            .unwrap_or(true),
        "a clean commit must clear the journal"
    );

    let got = reopen_clean(&db);
    assert_eq!(got, after, "clean commit applied the new rows");
    cross_check_sqlite3(&db, &got);
}

// ---------------------------------------------------------------------------
// A sweep across many db-write crash ordinals: whichever page write dies, and
// whether or not it is torn, the reopened db must be consistent and integrity
// ok. This catches any ordinal-specific recovery hole without hand-enumerating.
// ---------------------------------------------------------------------------

#[test]
fn sweep_db_write_crash_points_stay_consistent() {
    let before_template;
    {
        let scratch = Scratch::new("sweep_seed");
        let db = scratch.path("t.db");
        before_template = build_db(&db, 4096);
    }
    let after = expected_after();

    // The transaction dirties two on-disk pages, so a commit issues two db-page
    // `write_all_at`s (the file truncate/sync that follow are separate op kinds).
    // Sweep both write ordinals, each clean and torn.
    for nth in 1u32..=2 {
        for torn in [false, true] {
            let scratch = Scratch::new(&format!("sweep_{nth}_{torn}"));
            let db = scratch.path("t.db");
            let before = build_db(&db, 4096);
            assert_eq!(before, before_template, "stable seed across the sweep");

            crash_during_commit(
                &db,
                Plan {
                    target: Target::Db,
                    op: Op::Write,
                    nth,
                    torn,
                },
            );
            let got = reopen_clean(&db);
            assert_consistent(&got, &before, &after);
            // Any crash before the journal is cleared is determinate: rolled back.
            assert_eq!(
                got, before,
                "db-write crash nth={nth} torn={torn} should roll back"
            );
        }
    }
}
