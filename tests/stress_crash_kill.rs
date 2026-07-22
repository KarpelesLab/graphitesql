//! Process-kill crash-resilience stress (roadmap §6 durability, heavy variant).
//!
//! Unlike the deterministic in-process fault-injection harnesses
//! (`crash_recovery_harness.rs` / `wal_crash_recovery_harness.rs`, which run in
//! the normal gate), this suite spawns **real writer processes**, lets them commit
//! a stream of transactions against a file database, and **SIGKILLs** them at a
//! random instant — then reopens the file and asserts it recovered to a
//! consistent state. It exercises the end-to-end durability path (including the
//! on-disk journal/WAL, `fsync` ordering, and — crucially — **multiple processes
//! writing the same database** through the cross-process OS lock, the exact shape
//! that caused the production stale-header corruption now guarded by the pager's
//! `refresh_foreign_state`).
//!
//! Every test is `#[ignore]` (too slow/heavy for the per-PR gate); they run in the
//! scheduled `stress` GitHub workflow via `cargo test -- --ignored`. Sizes come
//! from env vars (small defaults for a quick local run; the workflow scales them
//! up for its ~15-minute budget):
//!
//!   * `GSQL_STRESS_ITERS`   — kill/recover iterations per test (default 8)
//!   * `GSQL_STRESS_WRITERS` — concurrent writer processes, multi-process tests (4)
//!   * `GSQL_STRESS_ACCTS`   — accounts per owner (40)
//!   * `GSQL_STRESS_BLOB_MAX`— max churn-blob size in bytes (200_000, forces overflow)
//!
//! ## The invariant
//! Each writer owns a disjoint set of `acct` rows and only ever *transfers* balance
//! between its own two accounts inside one transaction, so `SUM(bal)` over an
//! owner's rows is invariant no matter where a crash lands. After a kill we reopen
//! and require, for every owner: its balance sum equals the seeded constant (the
//! committed transactions were atomic — none torn), `PRAGMA integrity_check = ok`,
//! and — when `sqlite3` is on `PATH` — stock sqlite agrees on both.
//!
//! ## Writer mechanism (re-exec self)
//! There is no separate writer binary. The `stress_writer_child` test *is* the
//! writer: a driver spawns this same test executable targeting only that test with
//! `GSQL_STRESS_CHILD=1` set, and that process enters an infinite write loop until
//! killed. Run normally (no `GSQL_STRESS_CHILD`) the child test is a no-op, so a
//! plain `--ignored` run never hangs.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const INIT_BAL: i64 = 1000;

// ── small dependency-free PRNG (SplitMix64) ──────────────────────────────────
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A wall-clock-ish seed without a rand dependency (nanos since UNIX epoch).
fn time_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678)
}

// ───────────────────────────────────────────────────────────────────────────
// The writer child: runs only when GSQL_STRESS_CHILD is set. It transfers
// balance between its owner's accounts and churns overflow blobs, committing one
// transaction per iteration, forever (until SIGKILLed by the driver).
// ───────────────────────────────────────────────────────────────────────────
#[test]
#[ignore = "spawned as a child by the crash-kill drivers; no-op otherwise"]
fn stress_writer_child() {
    if std::env::var("GSQL_STRESS_CHILD").is_err() {
        return; // normal `--ignored` run: do nothing (never hangs).
    }
    let db = std::env::var("GSQL_DB").expect("GSQL_DB");
    let journal = std::env::var("GSQL_JOURNAL").unwrap_or_else(|_| "delete".into());
    let owner: i64 = std::env::var("GSQL_WRITER_ID")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let seed: u64 = std::env::var("GSQL_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(time_seed);
    let blob_max = env_usize("GSQL_STRESS_BLOB_MAX", 200_000);

    let mut rng = Rng::new(seed);
    let mut conn = Connection::open(&db).expect("open db");
    // Best-effort mode setup; the first writer that gets the lock sets it, the
    // rest are harmless no-ops. synchronous=FULL maximizes the fsync pressure a
    // kill can interrupt.
    let _ = conn.execute(&format!("PRAGMA journal_mode={journal}"));
    let _ = conn.execute("PRAGMA synchronous=FULL");
    let _ = conn.execute("PRAGMA busy_timeout=2000");

    // This owner's account ids. graphite holds a whole-file Exclusive lock for the
    // duration of each write transaction (it can't express SQLite's byte-range
    // RESERVED lock that keeps readers during a write), so with several writers
    // hammering, this initial read can be starved past `busy_timeout` and return
    // `Busy`. A correct multi-process client retries on `Busy` — exactly as the
    // transaction loop below does — rather than treating it as fatal.
    let sql = format!("SELECT id FROM acct WHERE owner={owner} ORDER BY id");
    let rows = loop {
        match conn.query(&sql) {
            Ok(r) => break r.rows,
            Err(graphitesql::Error::Busy) => {
                std::thread::sleep(Duration::from_millis(1 + rng.below(10)));
                continue;
            }
            Err(e) => panic!("select acct: {e:?}"),
        }
    };
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            _ => panic!("acct id not integer"),
        })
        .collect();
    assert!(ids.len() >= 2, "owner {owner} needs >= 2 accounts");

    let mut committed: u64 = 0;
    loop {
        let a = ids[rng.below(ids.len() as u64) as usize];
        let mut b = ids[rng.below(ids.len() as u64) as usize];
        if b == a {
            b = ids[(a as usize + 1) % ids.len()];
        }
        let amt = 1 + rng.below(50) as i64;
        // One atomic transaction: move `amt` a->b (sum preserved), and with some
        // probability churn a blob (insert an overflow-sized blob or delete one),
        // stressing overflow pages + the freelist under concurrent kills.
        if run_txn(&mut conn, &mut rng, owner, a, b, amt, blob_max).is_err() {
            // Lock contention / transient: retry after a beat.
            std::thread::sleep(Duration::from_millis(2 + rng.below(8)));
            continue;
        }
        committed += 1;
        if committed.is_multiple_of(32) {
            println!("committed {committed}");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    }
}

fn run_txn(
    conn: &mut Connection,
    rng: &mut Rng,
    owner: i64,
    a: i64,
    b: i64,
    amt: i64,
    blob_max: usize,
) -> Result<(), String> {
    let exec = |c: &mut Connection, sql: &str| -> Result<(), String> {
        c.execute(sql).map(|_| ()).map_err(|e| format!("{e}"))
    };
    exec(conn, "BEGIN IMMEDIATE")?;
    let body = (|| -> Result<(), String> {
        exec(conn, &format!("UPDATE acct SET bal=bal-{amt} WHERE id={a}"))?;
        exec(conn, &format!("UPDATE acct SET bal=bal+{amt} WHERE id={b}"))?;
        match rng.below(4) {
            0 => {
                let sz = 1 + rng.below(blob_max as u64);
                exec(
                    conn,
                    &format!("INSERT INTO blobs(owner,data) VALUES({owner}, randomblob({sz}))"),
                )?;
            }
            1 => {
                // Delete one of this owner's blobs (if any).
                exec(
                    conn,
                    &format!(
                        "DELETE FROM blobs WHERE id IN (SELECT id FROM blobs WHERE owner={owner} LIMIT 1)"
                    ),
                )?;
            }
            _ => {}
        }
        Ok(())
    })();
    match body {
        Ok(()) => exec(conn, "COMMIT"),
        Err(e) => {
            let _ = conn.execute("ROLLBACK");
            Err(e)
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Driver helpers
// ───────────────────────────────────────────────────────────────────────────

/// A unique scratch db path under a per-PID stress directory.
fn stress_db(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!("gsql-stress-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{tag}-{}.db", time_seed()))
        .to_string_lossy()
        .into_owned()
}

fn rm_db(path: &str) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{path}{suffix}"));
    }
}

/// Create + seed the accounts and blobs tables: `writers` owners, `accts` rows
/// each, every account starting at `INIT_BAL`. Returns the per-owner expected
/// balance sum.
fn seed_db(path: &str, journal: &str, writers: usize, accts: usize) -> i64 {
    rm_db(path);
    let mut c = Connection::create(path).expect("create db");
    c.execute(&format!("PRAGMA journal_mode={journal}"))
        .expect("journal_mode");
    c.execute("CREATE TABLE acct(id INTEGER PRIMARY KEY, owner INTEGER, bal INTEGER)")
        .unwrap();
    c.execute("CREATE INDEX acct_owner ON acct(owner)").unwrap();
    c.execute("CREATE TABLE blobs(id INTEGER PRIMARY KEY, owner INTEGER, data BLOB)")
        .unwrap();
    c.execute("BEGIN").unwrap();
    for owner in 0..writers {
        for _ in 0..accts {
            c.execute(&format!(
                "INSERT INTO acct(owner,bal) VALUES({owner},{INIT_BAL})"
            ))
            .unwrap();
        }
    }
    c.execute("COMMIT").unwrap();
    (accts as i64) * INIT_BAL
}

/// Spawn one writer child (this test binary, re-invoked to run only
/// `stress_writer_child`), piping its stdout so the driver can wait for progress.
fn spawn_writer(db: &str, journal: &str, owner: usize, seed: u64) -> Child {
    let exe = std::env::current_exe().expect("current_exe");
    Command::new(exe)
        .args([
            "--exact",
            "stress_writer_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GSQL_STRESS_CHILD", "1")
        .env("GSQL_DB", db)
        .env("GSQL_JOURNAL", journal)
        .env("GSQL_WRITER_ID", owner.to_string())
        .env("GSQL_SEED", seed.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn writer child")
}

/// Block until the child prints its first progress line (a transaction
/// committed) or a deadline passes. Returns true if progress was seen.
fn wait_for_progress(child: &mut Child, within: Duration) -> bool {
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => return false,
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            if line.starts_with("committed") {
                let _ = tx.send(());
                break;
            }
            line.clear();
        }
    });
    rx.recv_timeout(within).is_ok()
}

/// Reopen the recovered database and assert consistency: every owner's balance
/// sum equals the seeded constant (atomic recovery, no torn transaction) and
/// `integrity_check` is `ok`. Cross-checks with `sqlite3` when it is on `PATH`.
fn assert_recovered(path: &str, writers: usize, expected_sum: i64) {
    let c = Connection::open(path).expect("reopen recovered db");
    let ic = c.query("PRAGMA integrity_check").unwrap();
    assert_eq!(
        ic.rows[0][0],
        Value::Text("ok".into()),
        "graphite integrity_check failed after kill: {:?}",
        ic.rows
    );
    for owner in 0..writers {
        let sum = c
            .query(&format!(
                "SELECT COALESCE(SUM(bal),0) FROM acct WHERE owner={owner}"
            ))
            .unwrap();
        assert_eq!(
            sum.rows[0][0],
            Value::Integer(expected_sum),
            "owner {owner}: balance sum drifted (torn transaction) after kill"
        );
    }
    // Optional differential: stock sqlite3 must also read a clean, consistent file.
    if let Some(out) = sqlite3(path, "PRAGMA integrity_check;") {
        assert_eq!(
            out.trim(),
            "ok",
            "sqlite3 integrity_check failed after kill"
        );
        for owner in 0..writers {
            let s = sqlite3(
                path,
                &format!("SELECT COALESCE(SUM(bal),0) FROM acct WHERE owner={owner};"),
            )
            .unwrap_or_default();
            assert_eq!(
                s.trim(),
                expected_sum.to_string(),
                "sqlite3 sees owner {owner} sum drift after kill"
            );
        }
    }
}

/// Run a query through the `sqlite3` CLI if present; `None` when it is not on
/// `PATH` (the test still asserts graphite's own recovery).
fn sqlite3(path: &str, sql: &str) -> Option<String> {
    let out = Command::new("sqlite3").arg(path).arg(sql).output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        // A non-zero exit (e.g. "database disk image is malformed") is a real
        // failure the caller should see.
        Some(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// One kill/recover cycle over `writers` concurrent processes: spawn them, wait
/// until each has committed at least once, let them run a randomized extra beat,
/// SIGKILL them all, then assert the file recovered consistently.
fn kill_cycle(db: &str, journal: &str, writers: usize, expected_sum: i64, seed: u64) {
    let mut rng = Rng::new(seed);
    let mut kids: Vec<Child> = (0..writers)
        .map(|o| spawn_writer(db, journal, o, seed.wrapping_add(o as u64 * 0x1000)))
        .collect();
    // Ensure every writer is actually committing before we kill (otherwise the
    // test would trivially pass on an untouched file).
    for k in &mut kids {
        assert!(
            wait_for_progress(k, Duration::from_secs(20)),
            "writer never reported progress (open/commit failed?)"
        );
    }
    // Let the writers race a little, then kill them at staggered instants.
    std::thread::sleep(Duration::from_millis(10 + rng.below(60)));
    for k in &mut kids {
        std::thread::sleep(Duration::from_millis(rng.below(15)));
        let _ = k.kill(); // SIGKILL on Unix — no cleanup, hardest crash.
    }
    for mut k in kids {
        let _ = k.wait();
    }
    assert_recovered(db, writers, expected_sum);
}

// ───────────────────────────────────────────────────────────────────────────
// The drivers
// ───────────────────────────────────────────────────────────────────────────

fn run_kill_stress(journal: &str, writers: usize) {
    let iters = env_usize("GSQL_STRESS_ITERS", 8);
    let accts = env_usize("GSQL_STRESS_ACCTS", 40);
    let db = stress_db(&format!("kill-{journal}-{writers}"));
    let expected = seed_db(&db, journal, writers, accts);
    let base = time_seed();
    for i in 0..iters {
        kill_cycle(
            &db,
            journal,
            writers,
            expected,
            base.wrapping_add(i as u64 * 0x9E37),
        );
    }
    rm_db(&db);
}

#[test]
#[ignore = "heavy: spawns and SIGKILLs writer processes; run via the stress workflow"]
fn crash_kill_single_writer_delete() {
    run_kill_stress("delete", 1);
}

#[test]
#[ignore = "heavy: spawns and SIGKILLs writer processes; run via the stress workflow"]
fn crash_kill_single_writer_wal() {
    run_kill_stress("wal", 1);
}

#[test]
#[ignore = "heavy: multiple processes writing one db, then SIGKILLed; stress workflow"]
fn crash_kill_multiprocess_delete() {
    run_kill_stress("delete", env_usize("GSQL_STRESS_WRITERS", 4));
}

#[test]
#[ignore = "heavy: multiple processes writing one db, then SIGKILLed; stress workflow"]
fn crash_kill_multiprocess_wal() {
    run_kill_stress("wal", env_usize("GSQL_STRESS_WRITERS", 4));
}
