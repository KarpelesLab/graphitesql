//! Roadmap C9d: the `Connection` threading model.
//!
//! The C9d goal was to make a `Connection` `Send` (movable across threads) so it
//! could be handed to a thread pool. That turned out to be **unreachable** with a
//! clean, non-breaking change: a `Connection` stores its file as a single erased
//! `Box<dyn File>`, and the always-available in-memory VFS handle (`MemoryFile`,
//! which backs `:memory:` and must work in `no_std`/wasm) is deliberately
//! `Rc`/`RefCell`-based and `!Send`. Because the same concrete `Connection` type
//! carries both the `Send` `StdFile` and the `!Send` `MemoryFile`, the type is
//! `Send` only if *every* `File` impl is — which would require either editing the
//! intentionally single-threaded `MemoryFile` or making `Connection` generic over
//! the VFS (a large, pervasive refactor). See the `Connection` doc comment for the
//! full, itemized blocker list.
//!
//! graphite therefore ships the roadmap-blessed fallback: a **documented
//! per-thread model**. A `Connection` is thread-confined (used by one thread at a
//! time; neither `Send` nor `Sync`). Real multi-threaded use gives **each thread
//! its own `Connection`** over the same file path — the built-in `StdVfs`
//! coordinates cross-`Connection` access to one file through a process-local lock
//! manager and a shared wal-index. This test exercises exactly that model.
//!
//! The `!Send`-ness is intentional and load-bearing, so we do *not* assert
//! `Connection: Send` here (it would be a false green). That `Connection` is
//! deliberately *not* `Send` is enforced at compile time by a `compile_fail`
//! doc-test on the `Connection` type itself (see `src/exec/mod.rs`).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::sync::{Arc, Barrier};
use std::thread;

fn tmp_path(tag: &str) -> String {
    std::env::temp_dir()
        .join(format!("gsql-c9d-{}-{}.db", tag, std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// The usable concurrency model for **writers**: N worker threads, each owning a
/// `Connection` to its **own** database file, running concurrently. This is the
/// robust thread-pool pattern — every worker opens, writes, and reads entirely
/// within its own thread, so nothing `!Send` crosses a thread boundary and no two
/// connections contend for one file's write lock. (graphite's cross-`Connection`
/// write coordination to a *single* file is process-local and single-writer;
/// concurrent writers to one file is a separate roadmap concern, so the
/// per-thread model gives each writer its own file.)
#[test]
fn per_thread_connections_write_concurrently() {
    const WORKERS: i64 = 4;
    const PER_WORKER: i64 = 50;
    let barrier = Arc::new(Barrier::new(WORKERS as usize));

    let handles: Vec<_> = (0..WORKERS)
        .map(|w| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let path = tmp_path(&format!("writer{w}"));
                let _ = std::fs::remove_file(&path);
                let mut conn = Connection::create(&path).unwrap();
                conn.execute("CREATE TABLE t(n INTEGER)").unwrap();
                // Start all workers writing at once, to actually overlap.
                barrier.wait();
                for n in 0..PER_WORKER {
                    conn.execute(&format!("INSERT INTO t VALUES ({n})"))
                        .unwrap();
                }
                let r = conn.query("SELECT count(*), sum(n) FROM t").unwrap();
                assert_eq!(r.rows[0][0], Value::Integer(PER_WORKER));
                assert_eq!(
                    r.rows[0][1],
                    Value::Integer(PER_WORKER * (PER_WORKER - 1) / 2)
                );
                let _ = std::fs::remove_file(&path);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

/// The usable concurrency model for **readers**: many threads, each with its own
/// `Connection`, reading the *same* on-disk database at once. Read concurrency to
/// one file is supported; every reader sees the committed data.
#[test]
fn per_thread_connections_read_shared_file() {
    let path = tmp_path("readers");
    let _ = std::fs::remove_file(&path);

    // Seed from the main thread, then close before readers open.
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(n INTEGER)").unwrap();
        for n in 0..100 {
            c.execute(&format!("INSERT INTO t VALUES ({n})")).unwrap();
        }
    }

    const READERS: usize = 6;
    let barrier = Arc::new(Barrier::new(READERS));
    let handles: Vec<_> = (0..READERS)
        .map(|_| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                // Each reader owns its own Connection, opened inside the thread.
                let conn = Connection::open(&path).unwrap();
                barrier.wait();
                for _ in 0..20 {
                    let r = conn.query("SELECT count(*), sum(n) FROM t").unwrap();
                    assert_eq!(r.rows[0][0], Value::Integer(100));
                    assert_eq!(r.rows[0][1], Value::Integer(4950));
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let _ = std::fs::remove_file(&path);
}

/// A `Connection` produced on one thread and then used to run DML + a query — the
/// straightforward "do work on this connection" flow — with the connection created
/// and consumed entirely within a single spawned worker (never moved while live).
#[test]
fn work_inside_a_spawned_thread() {
    let path = tmp_path("worker");
    let _ = std::fs::remove_file(&path);

    let path_for_thread = path.clone();
    let value = thread::spawn(move || {
        let mut conn = Connection::create(&path_for_thread).unwrap();
        conn.execute("CREATE TABLE kv(k TEXT, v INTEGER)").unwrap();
        conn.execute("INSERT INTO kv VALUES ('a', 1), ('b', 2), ('c', 3)")
            .unwrap();
        conn.execute("UPDATE kv SET v = v * 10 WHERE k = 'b'")
            .unwrap();
        let r = conn.query("SELECT sum(v) FROM kv").unwrap();
        match r.rows[0][0] {
            Value::Integer(n) => n,
            ref other => panic!("unexpected sum: {other:?}"),
        }
    })
    .join()
    .unwrap();

    // 1 + 20 + 3
    assert_eq!(value, 24);

    let _ = std::fs::remove_file(&path);
}
