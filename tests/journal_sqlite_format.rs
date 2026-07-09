//! Roadmap C7a/C7b: the SQLite-format rollback journal.
//!
//! graphite writes its rollback journal in SQLite's documented on-disk byte
//! layout (the `0xd9d505f9 20a163d7` magic, the record-count / nonce / initial
//! page-count / sector-size / page-size header padded to a sector, then one
//! `pgno + page + checksum` record per saved page). These tests verify the
//! format three ways:
//!
//! 1. **Header byte layout** — decode a journal graphite just wrote and assert
//!    every field matches the spec, including the per-page checksum algorithm.
//! 2. **Self round-trip** — simulate a crash by *retaining* the journal that a
//!    commit would otherwise have cleared, reopen, and confirm graphite rolls
//!    back to the pre-transaction state with `PRAGMA integrity_check = ok`.
//! 3. **Cross-recovery vs `sqlite3`** — a hot journal graphite leaves behind must
//!    let the real `sqlite3` recover the database (integrity ok), and graphite
//!    must recover a hot journal that `sqlite3` left behind. Skipped gracefully
//!    when no `sqlite3` is on `PATH` (following `tests/auto_vacuum_truncate.rs`).

#![cfg(feature = "std")]

use graphitesql::vfs::std_file::StdVfs;
use graphitesql::vfs::{File, LockLevel, OpenFlags, Vfs};
use graphitesql::{Connection, Value};
use std::cell::Cell;
use std::process::Command;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// A VFS that can leave a "hot" rollback journal behind, simulating a crash.
//
// It delegates everything to StdVfs, but wraps the `-journal` file so that, when
// "retain" mode is armed, the `truncate(0)` that graphite's commit issues to
// clear the journal is suppressed. The journal that the commit already wrote and
// synced (in SQLite format) therefore survives on disk as a hot journal, exactly
// as if the process had crashed after syncing the database but before deleting
// the journal. (To a rollback-journal database those two crash points are
// indistinguishable: both must roll the transaction back.)
// ---------------------------------------------------------------------------

struct RetainVfs {
    inner: StdVfs,
    retain: Rc<Cell<bool>>,
}

impl Vfs for RetainVfs {
    fn open(&self, path: &str, flags: OpenFlags) -> graphitesql::Result<Box<dyn File>> {
        let f = self.inner.open(path, flags)?;
        if path.ends_with("-journal") {
            Ok(Box::new(RetainFile {
                inner: f,
                retain: Rc::clone(&self.retain),
            }))
        } else {
            Ok(f)
        }
    }
    fn delete(&self, path: &str) -> graphitesql::Result<()> {
        // Suppress deletion of the journal while retaining (delete-mode commit).
        if path.ends_with("-journal") && self.retain.get() {
            return Ok(());
        }
        self.inner.delete(path)
    }
    fn exists(&self, path: &str) -> graphitesql::Result<bool> {
        self.inner.exists(path)
    }
}

struct RetainFile {
    inner: Box<dyn File>,
    retain: Rc<Cell<bool>>,
}

impl File for RetainFile {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> graphitesql::Result<()> {
        self.inner.read_exact_at(buf, offset)
    }
    fn write_all_at(&mut self, buf: &[u8], offset: u64) -> graphitesql::Result<()> {
        self.inner.write_all_at(buf, offset)
    }
    fn truncate(&mut self, size: u64) -> graphitesql::Result<()> {
        // The "crash": ignore the commit's attempt to clear the journal.
        if size == 0 && self.retain.get() {
            return Ok(());
        }
        self.inner.truncate(size)
    }
    fn sync(&mut self) -> graphitesql::Result<()> {
        self.inner.sync()
    }
    fn size(&self) -> graphitesql::Result<u64> {
        self.inner.size()
    }
    fn lock(&self, level: LockLevel) -> graphitesql::Result<()> {
        self.inner.lock(level)
    }
    fn unlock(&self, level: LockLevel) -> graphitesql::Result<()> {
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

/// A unique per-PID scratch directory; cleaned up by the caller.
struct Scratch {
    dir: std::path::PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("gsql-c7-{}-{}", std::process::id(), tag));
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

const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn journal_page_checksum(nonce: u32, page: &[u8]) -> u32 {
    let mut cksum = nonce;
    let mut x = page.len() as isize - 200;
    while x >= 0 {
        cksum = cksum.wrapping_add(page[x as usize] as u32);
        x -= 200;
    }
    cksum
}

/// Run `body` against a connection whose journal is retained (a hot journal is
/// left on disk afterwards). Returns nothing; the connection is dropped inside.
fn with_retained_journal(vfs: &RetainVfs, retain: &Rc<Cell<bool>>, path: &str, sql: &str) {
    let mut conn = Connection::open_vfs(vfs, path).unwrap();
    // Run all mutations inside one explicit transaction so the single commit
    // (whose journal we then retain) rolls back the whole change set.
    retain.set(true);
    conn.execute_batch(&format!("BEGIN;\n{sql};\nCOMMIT;"))
        .unwrap();
    retain.set(false);
    drop(conn);
}

// ---------------------------------------------------------------------------
// 1. Header byte layout
// ---------------------------------------------------------------------------

#[test]
fn journal_header_byte_layout_matches_spec() {
    let scratch = Scratch::new("hdr");
    let db = scratch.path("t.db");
    let retain = Rc::new(Cell::new(false));
    let vfs = RetainVfs {
        inner: StdVfs::new(),
        retain: Rc::clone(&retain),
    };

    // Build a small db, then run a transaction whose journal we keep.
    {
        let mut c = Connection::create_vfs(&vfs, &db, 4096).unwrap();
        c.execute("CREATE TABLE t(a INTEGER, b TEXT)").unwrap();
        c.execute("INSERT INTO t VALUES (1,'one'),(2,'two')")
            .unwrap();
    }
    let page_size = std::fs::metadata(&db).unwrap().len();
    assert!(page_size >= 4096);

    with_retained_journal(&vfs, &retain, &db, "UPDATE t SET b = 'CHANGED' WHERE a = 1");

    // Decode the retained journal and check every header field.
    let jbytes = std::fs::read(format!("{db}-journal")).unwrap();
    assert!(jbytes.len() >= 512, "journal smaller than one sector");
    assert_eq!(&jbytes[0..8], &JOURNAL_MAGIC, "magic");
    let nrec = be32(&jbytes, 8);
    let nonce = be32(&jbytes, 12);
    let init_pages = be32(&jbytes, 16);
    let sector = be32(&jbytes, 20);
    let pgsz = be32(&jbytes, 24);
    assert_eq!(sector, 512, "sector size");
    assert_eq!(pgsz, 4096, "page size");
    assert!(nrec >= 1, "at least one page record");
    assert!(init_pages >= 1, "initial page count recorded");
    // The header is padded to one sector; bytes 28..512 are zero.
    assert!(
        jbytes[28..512].iter().all(|&x| x == 0),
        "header sector padding"
    );

    // The journal must hold exactly nrec records of (4 + pgsz + 4) bytes.
    let rec_len = 4 + pgsz as usize + 4;
    assert_eq!(
        jbytes.len(),
        512 + nrec as usize * rec_len,
        "journal length = header sector + nrec records"
    );

    // Validate the first record's checksum against the documented algorithm.
    let rec0 = &jbytes[512..512 + rec_len];
    let pgno = be32(rec0, 0);
    assert!(pgno >= 1, "page number in record");
    let page = &rec0[4..4 + pgsz as usize];
    let stored = be32(rec0, 4 + pgsz as usize);
    assert_eq!(
        stored,
        journal_page_checksum(nonce, page),
        "page checksum matches nonce-seeded sparse sum"
    );
}

// ---------------------------------------------------------------------------
// 2. Self round-trip: graphite recovers its own hot journal
// ---------------------------------------------------------------------------

#[test]
fn graphite_recovers_its_own_hot_journal() {
    let scratch = Scratch::new("self");
    let db = scratch.path("t.db");
    let retain = Rc::new(Cell::new(false));
    let vfs = RetainVfs {
        inner: StdVfs::new(),
        retain: Rc::clone(&retain),
    };

    {
        let mut c = Connection::create_vfs(&vfs, &db, 4096).unwrap();
        c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
            .unwrap();
        c.execute("INSERT INTO t VALUES (1,'one'),(2,'two'),(3,'three')")
            .unwrap();
    }

    // The pre-crash state we expect to roll back to.
    let before = {
        let c = Connection::open_readonly_vfs(&vfs, &db).unwrap();
        rows(&c, "SELECT a,b FROM t ORDER BY a")
    };

    // Crash mid-commit (journal retained).
    with_retained_journal(
        &vfs,
        &retain,
        &db,
        "UPDATE t SET b='MUTATED'; INSERT INTO t VALUES (4,'four')",
    );
    assert!(
        std::fs::metadata(format!("{db}-journal"))
            .map(|m| m.len() >= 512)
            .unwrap_or(false),
        "a hot journal should remain on disk"
    );

    // Reopen: recovery rolls the database back to `before`.
    {
        let c = Connection::open_vfs(&vfs, &db).unwrap();
        let after = rows(&c, "SELECT a,b FROM t ORDER BY a");
        assert_eq!(after, before, "rolled back to pre-transaction state");
        let ok = rows(&c, "PRAGMA integrity_check");
        assert_eq!(ok, vec![vec!["ok".to_string()]]);
    }
}

// ---------------------------------------------------------------------------
// 3. Cross-recovery against the real sqlite3
// ---------------------------------------------------------------------------

#[test]
fn sqlite3_recovers_graphite_hot_journal() {
    if !have_sqlite3() {
        eprintln!("skipping: sqlite3 not on PATH");
        return;
    }
    let scratch = Scratch::new("g2s");
    let db = scratch.path("t.db");
    let retain = Rc::new(Cell::new(false));
    let vfs = RetainVfs {
        inner: StdVfs::new(),
        retain: Rc::clone(&retain),
    };

    {
        let mut c = Connection::create_vfs(&vfs, &db, 4096).unwrap();
        c.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT)")
            .unwrap();
        c.execute("INSERT INTO t VALUES (1,'alpha'),(2,'beta'),(3,'gamma')")
            .unwrap();
    }
    let before = sqlite3(&db, "SELECT a||'='||b FROM t ORDER BY a");

    // Leave a hot journal behind from graphite.
    with_retained_journal(
        &vfs,
        &retain,
        &db,
        "UPDATE t SET b='zzz'; INSERT INTO t VALUES (4,'delta')",
    );
    assert!(std::path::Path::new(&format!("{db}-journal")).exists());

    // The real sqlite3, opening the db, must detect & roll back the hot journal.
    let after = sqlite3(&db, "SELECT a||'='||b FROM t ORDER BY a");
    assert_eq!(after, before, "sqlite3 rolled graphite's hot journal back");
    assert_eq!(sqlite3(&db, "PRAGMA integrity_check"), "ok");
    // sqlite3 should have removed the journal during recovery.
    assert!(
        !std::path::Path::new(&format!("{db}-journal")).exists(),
        "sqlite3 cleared the hot journal after recovery"
    );
}

#[test]
fn graphite_recovers_sqlite3_hot_journal() {
    if !have_sqlite3() {
        eprintln!("skipping: sqlite3 not on PATH");
        return;
    }
    let scratch = Scratch::new("s2g");
    let db = scratch.path("t.db");

    // Build a committed database with sqlite3 itself.
    sqlite3(
        &db,
        "PRAGMA page_size=4096; \
         CREATE TABLE t(a INTEGER PRIMARY KEY, b TEXT); \
         INSERT INTO t VALUES (1,'one'),(2,'two'),(3,'three');",
    );
    let before = sqlite3(&db, "SELECT a||'='||b FROM t ORDER BY a");

    // Produce a genuinely-hot, sqlite3-authored journal by forcing a cache spill
    // inside an open transaction and then SIGKILLing sqlite3 mid-transaction. A
    // 1-page cache plus a large multi-page UPDATE makes sqlite3 journal the
    // originals, sync the journal, and write dirty pages back into the main db
    // *before* COMMIT. Killing it then leaves the main db partially mutated and a
    // valid (non-zeroed) hot journal on disk describing how to undo it.
    let jpath = format!("{db}-journal");
    let script = "PRAGMA cache_size=1;\n\
         PRAGMA journal_mode=delete;\n\
         BEGIN IMMEDIATE;\n\
         INSERT INTO t(a,b) SELECT a, hex(randomblob(2000)) FROM t;\n\
         INSERT INTO t(a,b) SELECT a+1000, hex(randomblob(2000)) FROM t;\n\
         INSERT INTO t(a,b) SELECT a+100000, hex(randomblob(2000)) FROM t;\n\
         UPDATE t SET b = hex(randomblob(3000));\n\
         SELECT 'spilled';\n";
    let mut child = std::process::Command::new("sqlite3")
        .arg(&db)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn sqlite3");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
    }
    // Give sqlite3 a moment to spill, then kill it before COMMIT.
    std::thread::sleep(std::time::Duration::from_millis(400));
    let _ = child.kill();
    let _ = child.wait();

    // We need a *hot* journal (exists, non-empty, valid header). If this sqlite3
    // build/timing did not leave one, skip rather than assert a false negative.
    let hot = std::fs::read(&jpath).ok().filter(|j| {
        j.len() >= 512 && j[0..8] == JOURNAL_MAGIC && {
            // header not zeroed-out (persist invalidation) and a sane page size
            let pgsz = be32(j, 24);
            pgsz == 4096
        }
    });
    let Some(_) = hot else {
        eprintln!("skipping s->g leg: sqlite3 left no hot journal (timing/build)");
        return;
    };

    // graphite, opening the database, must detect the sqlite3-authored hot
    // journal and roll the interrupted transaction all the way back.
    let vfs = StdVfs::new();
    {
        let c = Connection::open_vfs(&vfs, &db).unwrap();
        let after = rows(&c, "SELECT a||'='||b FROM t ORDER BY a");
        let after_flat: Vec<String> = after.into_iter().map(|r| r[0].clone()).collect();
        assert_eq!(
            after_flat.join("\n"),
            before,
            "graphite rolled sqlite3's hot journal back to the committed state"
        );
        assert_eq!(
            rows(&c, "PRAGMA integrity_check"),
            vec![vec!["ok".to_string()]]
        );
    }
    // Cross-check the recovered db with sqlite3 too.
    assert_eq!(sqlite3(&db, "PRAGMA integrity_check"), "ok");
    assert_eq!(
        sqlite3(&db, "SELECT a||'='||b FROM t ORDER BY a"),
        before,
        "sqlite3 agrees the db is back to the committed state"
    );
}

// ---------------------------------------------------------------------------

/// Run a query through graphite and return rows as strings.
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
