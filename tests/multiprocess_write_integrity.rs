//! Concurrent writers in **separate processes** must never corrupt a shared
//! database file. graphitesql serialises cross-process writers with an OS lock, but
//! it used to acquire that lock *lazily* at the first page write — so a write
//! statement's b-tree navigation reads happened unlocked. A foreign process
//! committing between a writer's navigation and its first page write was then
//! clobbered (the writer staged its change over the page images it read *before*
//! the foreign commit), silently corrupting the b-tree: orphaned pages, rows out of
//! rowid order, table/index desync. The fix takes the write lock (and refreshes the
//! foreign-committed state) at the *start* of a write statement, before navigation,
//! matching SQLite's RESERVED-at-write-start.
//!
//! This test spawns several **child processes** (self-exec) that each hammer a
//! shared on-disk database with the pattern that surfaced the bug: an `UPSERT` into
//! a `TEXT PRIMARY KEY` table (a unique auto-index) plus a `DELETE`+`INSERT` burst
//! into a secondary-indexed table, wrapped in per-row transactions, over an
//! overlapping key pool so the writers contend on the same b-tree pages. After they
//! finish, `PRAGMA integrity_check` must report `ok`. Before the fix this failed
//! within a few runs; after it, the writers serialise correctly and the file stays
//! consistent.

#![cfg(feature = "std")]

use graphitesql::{Connection, Error, Value};
use std::time::Duration;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
fn nproc() -> usize {
    env_usize("GSQL_MP_WRITERS", 5)
}
fn txns() -> usize {
    env_usize("GSQL_MP_TXNS", 250)
}
fn pool() -> usize {
    env_usize("GSQL_MP_POOL", 80)
}

/// A tiny deterministic PRNG so the child needs no external crates.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const SCHEMA: &str = "\
    CREATE TABLE IF NOT EXISTS frag(uid TEXT PRIMARY KEY, job TEXT, n INTEGER, at TEXT);\
    CREATE INDEX IF NOT EXISTS frag_job ON frag(job);\
    CREATE TABLE IF NOT EXISTS hit(job TEXT, uid TEXT, k INTEGER, v TEXT);\
    CREATE INDEX IF NOT EXISTS hit_job ON hit(job);\
    CREATE INDEX IF NOT EXISTS hit_uid ON hit(uid);";

/// One writer child: `TXNS` transactions of upsert-fragment + delete/insert-hits,
/// retrying on `Busy` (a correct multi-process client). Runs only when re-executed
/// as a child (env flag set); a normal `--ignored` run is a no-op.
fn writer_child() {
    let db = std::env::var("GSQL_MP_DB").expect("GSQL_MP_DB");
    let id: u64 = std::env::var("GSQL_MP_ID")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut rng = Rng(0x9E3779B97F4A7C15 ^ (id.wrapping_mul(0x1000)));
    // `open` reads page 1, which can be `Busy` against a foreign writer's lock — and
    // `busy_timeout` can't be set until after opening — so retry the open itself.
    let mut c = loop {
        match Connection::open(&db) {
            Ok(c) => break c,
            Err(Error::Busy) => std::thread::sleep(Duration::from_millis(1 + rng.next() % 8)),
            Err(e) => panic!("child open: {e:?}"),
        }
    };
    let _ = c.execute("PRAGMA busy_timeout=4000");

    // Retry a statement on Busy (graphite surfaces Busy immediately for writers).
    let run = |c: &mut Connection, sql: &str, rng: &mut Rng| loop {
        match c.execute(sql) {
            Ok(_) => break,
            Err(Error::Busy) => std::thread::sleep(Duration::from_millis(1 + rng.next() % 8)),
            Err(e) => panic!("child stmt failed: {e:?} on {sql}"),
        }
    };

    for i in 0..txns() {
        let uid = format!("uid-{:04}", (id as usize * 13 + i) % pool());
        let job = format!("job-{}", i % 20);
        run(&mut c, "BEGIN", &mut rng);
        run(
            &mut c,
            &format!(
                "INSERT INTO frag(uid,job,n,at) VALUES('{uid}','{job}',{i},datetime('now')) \
                 ON CONFLICT(uid) DO UPDATE SET job='{job}',n={i},at=datetime('now')"
            ),
            &mut rng,
        );
        run(
            &mut c,
            &format!("DELETE FROM hit WHERE uid='{uid}'"),
            &mut rng,
        );
        for h in 0..(i % 6) {
            run(
                &mut c,
                &format!(
                    "INSERT INTO hit(job,uid,k,v) VALUES('{job}','{uid}',{},'val-{i}-{h}-padpadpadpad')",
                    i * 100 + h
                ),
                &mut rng,
            );
        }
        run(&mut c, "COMMIT", &mut rng);
    }
    // Best-effort marker so the parent knows this child made real progress.
    println!("done {id}");
}

#[test]
#[ignore = "spawned as a writer child; no-op otherwise"]
fn multiprocess_writer_child() {
    if std::env::var("GSQL_MP_CHILD").is_ok() {
        writer_child();
    }
}

#[test]
#[ignore = "heavy: spawns several writer processes hammering one db (fsync-per-txn); \
            run via the stress workflow or `--ignored`. Env: GSQL_MP_WRITERS/TXNS/POOL."]
fn concurrent_process_writes_keep_the_db_consistent() {
    let dir = std::env::temp_dir();
    let db = dir
        .join(format!("gsql-mp-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned();
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db}{suffix}"));
    }

    // Seed the schema (one process, no contention).
    {
        let mut c = Connection::create(&db).expect("create db");
        c.execute_batch(SCHEMA).expect("schema");
    }

    // Spawn the writer children.
    let exe = std::env::current_exe().expect("current_exe");
    let mut kids: Vec<std::process::Child> = (0..nproc())
        .map(|id| {
            std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "multiprocess_writer_child",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("GSQL_MP_CHILD", "1")
                .env("GSQL_MP_DB", &db)
                .env("GSQL_MP_ID", id.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn writer child")
        })
        .collect();

    // Wait for all children; capture any panic output.
    let mut failures = Vec::new();
    for (id, k) in kids.iter_mut().enumerate() {
        let out = k.wait_with_output_ref();
        if !out.0 {
            failures.push(format!("child {id} exited non-zero: {}", out.1));
        }
    }
    for suffix in ["-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db}{suffix}"));
    }

    // The whole point: after concurrent multi-process writes, the file is intact.
    let integrity = {
        let c = Connection::open(&db).expect("reopen for integrity");
        c.query("PRAGMA integrity_check")
            .expect("integrity_check")
            .rows
    };
    let _ = std::fs::remove_file(&db);

    assert!(
        failures.is_empty(),
        "writer child(ren) failed (a Busy/corruption during the run): {failures:?}"
    );
    assert_eq!(
        integrity,
        vec![vec![Value::Text("ok".into())]],
        "integrity_check found corruption after concurrent multi-process writes: {integrity:?}"
    );
}

/// Small helper: wait for a child and return (success, captured stderr).
trait WaitOutput {
    fn wait_with_output_ref(&mut self) -> (bool, String);
}
impl WaitOutput for std::process::Child {
    fn wait_with_output_ref(&mut self) -> (bool, String) {
        use std::io::Read;
        let mut err = String::new();
        if let Some(mut s) = self.stderr.take() {
            let _ = s.read_to_string(&mut err);
        }
        let ok = self.wait().map(|s| s.success()).unwrap_or(false);
        (ok, err)
    }
}
