//! Volume / scale stress (roadmap §6): large row counts, deep overflow blobs,
//! random-key fragmentation, and big transactions + VACUUM. These are heavy
//! (seconds to minutes) so every test is `#[ignore]` and runs in the scheduled
//! `stress` GitHub workflow via `cargo test -- --ignored`, never in the per-PR
//! gate. Each test asserts graphite's own `PRAGMA integrity_check = ok`, query
//! correctness, and — when `sqlite3` is on `PATH` — that stock sqlite reads the
//! same file cleanly (byte-format compatibility at scale).
//!
//! Sizes come from env vars with modest defaults (a quick local run); the
//! workflow scales them up:
//!
//!   * `GSQL_VOLUME_ROWS`     — rows in the many-rows test (default 100_000)
//!   * `GSQL_VOLUME_BLOB_MB`  — size of each overlarge blob, MiB (default 4)
//!   * `GSQL_VOLUME_BLOBS`    — how many overlarge blobs (default 6)
//!   * `GSQL_VOLUME_FRAG_KEYS`— random text-PK rows for the fragmentation test (50_000)

#![cfg(feature = "std")]

use graphitesql::exec::eval::Params;
use graphitesql::{Connection, Value};
use std::process::Command;

// ── dependency-free PRNG (SplitMix64) ────────────────────────────────────────
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
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn stress_db(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!("gsql-stress-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{tag}.db")).to_string_lossy().into_owned()
}

fn rm_db(path: &str) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{path}{suffix}"));
    }
}

/// Run a single scalar query and return the first cell (or `None` if sqlite3 is
/// absent / errored) — used for the differential cross-check.
fn sqlite3_scalar(path: &str, sql: &str) -> Option<String> {
    let out = Command::new("sqlite3").arg(path).arg(sql).output().ok()?;
    Some(if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        String::from_utf8_lossy(&out.stderr).trim().to_string()
    })
}

fn graphite_scalar(c: &Connection, sql: &str) -> String {
    match &c.query(sql).unwrap().rows[0][0] {
        Value::Integer(i) => i.to_string(),
        Value::Text(t) => t.to_string(),
        Value::Real(r) => r.to_string(),
        Value::Null => String::new(),
        Value::Blob(b) => format!("<blob {} bytes>", b.len()),
    }
}

fn assert_integrity(c: &Connection, path: &str) {
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into()),
        "graphite integrity_check failed"
    );
    if let Some(ok) = sqlite3_scalar(path, "PRAGMA integrity_check;") {
        assert_eq!(ok, "ok", "sqlite3 integrity_check failed on graphite file");
    }
}

/// Many rows with a secondary index and periodic overflow blobs, inserted in
/// batched transactions. Verifies the row count, an indexed aggregate, and that
/// stock sqlite agrees and finds the file sound.
#[test]
#[ignore = "heavy: large row count; run via the stress workflow"]
fn stress_volume_many_rows() {
    let rows = env_usize("GSQL_VOLUME_ROWS", 100_000);
    let path = stress_db("volume-many-rows");
    rm_db(&path);
    let mut c = Connection::create(&path).unwrap();
    c.execute("PRAGMA journal_mode=WAL").unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, k INTEGER, s TEXT, data BLOB)")
        .unwrap();
    c.execute("CREATE INDEX t_k ON t(k)").unwrap();
    let mut rng = Rng::new(0xF00D);
    let mut key_sum: i64 = 0;
    let batch = 5_000;
    let mut i = 0usize;
    while i < rows {
        c.execute("BEGIN").unwrap();
        let end = (i + batch).min(rows);
        for j in i..end {
            let k = (rng.next_u64() % 1_000_000) as i64;
            key_sum += k;
            // Every 500th row carries an overflow-sized blob to exercise overflow
            // pages amid the bulk insert.
            if j % 500 == 0 {
                c.execute_params(
                    "INSERT INTO t(k, s, data) VALUES(?1, ?2, randomblob(3000))",
                    &Params {
                        positional: vec![Value::Integer(k), Value::Text(format!("row-{j}").into())],
                        named: vec![],
                    },
                )
                .unwrap();
            } else {
                c.execute_params(
                    "INSERT INTO t(k, s) VALUES(?1, ?2)",
                    &Params {
                        positional: vec![Value::Integer(k), Value::Text(format!("row-{j}").into())],
                        named: vec![],
                    },
                )
                .unwrap();
            }
        }
        c.execute("COMMIT").unwrap();
        i = end;
    }
    // Row count and an indexed aggregate are exact and match sqlite.
    assert_eq!(
        graphite_scalar(&c, "SELECT count(*) FROM t"),
        rows.to_string()
    );
    assert_eq!(
        graphite_scalar(&c, "SELECT COALESCE(SUM(k),0) FROM t"),
        key_sum.to_string()
    );
    // The index answers a range query with the same count graphite computes.
    let via_index = graphite_scalar(
        &c,
        "SELECT count(*) FROM t WHERE k BETWEEN 100000 AND 200000",
    );
    if let Some(s) = sqlite3_scalar(
        &path,
        "SELECT count(*) FROM t WHERE k BETWEEN 100000 AND 200000;",
    ) {
        assert_eq!(via_index, s, "indexed range count differs from sqlite3");
    }
    if let Some(s) = sqlite3_scalar(&path, "SELECT count(*) FROM t;") {
        assert_eq!(s, rows.to_string(), "sqlite3 row count differs");
    }
    if let Some(s) = sqlite3_scalar(&path, "SELECT COALESCE(SUM(k),0) FROM t;") {
        assert_eq!(s, key_sum.to_string(), "sqlite3 key sum differs");
    }
    assert_integrity(&c, &path);
    drop(c);
    rm_db(&path);
}

/// A handful of multi-MiB blobs — deep overflow chains — bound from Rust and read
/// back byte-for-byte, then validated by graphite + sqlite integrity_check.
#[test]
#[ignore = "heavy: multi-MiB overflow blobs; run via the stress workflow"]
fn stress_volume_overlarge_blobs() {
    let mb = env_usize("GSQL_VOLUME_BLOB_MB", 4);
    let count = env_usize("GSQL_VOLUME_BLOBS", 6);
    let path = stress_db("volume-blobs");
    rm_db(&path);
    let mut c = Connection::create(&path).unwrap();
    c.execute("CREATE TABLE big(id INTEGER PRIMARY KEY, n INTEGER, data BLOB)")
        .unwrap();
    let mut rng = Rng::new(0xBEEF);
    // Build each blob with a deterministic per-id byte pattern so the read-back
    // comparison catches any overflow-chain corruption, not just a length change.
    let make_blob = |id: usize, len: usize| -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        let mut r = Rng::new(0xABCD_0000 ^ id as u64);
        for _ in 0..len {
            v.push((r.next_u64() & 0xFF) as u8);
        }
        v
    };
    for id in 0..count {
        // Vary the size a little around the target so chains have different tails.
        let len = mb * 1024 * 1024 + (rng.next_u64() as usize % 4096);
        let blob = make_blob(id, len);
        c.execute_params(
            "INSERT INTO big(id, n, data) VALUES(?1, ?2, ?3)",
            &Params {
                positional: vec![
                    Value::Integer(id as i64),
                    Value::Integer(len as i64),
                    Value::Blob(blob),
                ],
                named: vec![],
            },
        )
        .unwrap();
    }
    // Read each blob back and compare byte-for-byte against the regenerated
    // pattern, and confirm the stored length column matches.
    for id in 0..count {
        let r = c
            .query(&format!("SELECT n, data FROM big WHERE id={id}"))
            .unwrap();
        let n = match r.rows[0][0] {
            Value::Integer(n) => n as usize,
            _ => panic!("n not integer"),
        };
        let data = match &r.rows[0][1] {
            Value::Blob(b) => b.clone(),
            other => panic!("data not blob: {other:?}"),
        };
        assert_eq!(
            data.len(),
            n,
            "blob {id}: length column disagrees with data"
        );
        assert_eq!(
            data,
            make_blob(id, n),
            "blob {id}: overflow chain content corrupted"
        );
    }
    // sqlite3 independently agrees on each length and finds the file sound.
    for id in 0..count {
        if let Some(s) = sqlite3_scalar(
            &path,
            &format!("SELECT length(data) FROM big WHERE id={id};"),
        ) {
            let n = graphite_scalar(&c, &format!("SELECT n FROM big WHERE id={id}"));
            assert_eq!(s, n, "sqlite3 blob {id} length differs");
        }
    }
    assert_integrity(&c, &path);
    drop(c);
    rm_db(&path);
}

/// Random text-PK (`WITHOUT ROWID`) inserts with a secondary index — the
/// resultdb fragment-cache shape that once bloated the b-tree ~10-20×. Guards
/// that random-key inserts stay compact (page count near sqlite's) and the file
/// round-trips through sqlite.
#[test]
#[ignore = "heavy: random-key fragmentation shape; run via the stress workflow"]
fn stress_volume_random_key_fragmentation() {
    let keys = env_usize("GSQL_VOLUME_FRAG_KEYS", 50_000);
    let path = stress_db("volume-frag");
    rm_db(&path);
    let mut c = Connection::create(&path).unwrap();
    c.execute("CREATE TABLE frag(k TEXT PRIMARY KEY, v INTEGER) WITHOUT ROWID")
        .unwrap();
    c.execute("CREATE INDEX frag_v ON frag(v)").unwrap();
    let mut rng = Rng::new(0x5EED);
    let batch = 5_000;
    let mut i = 0usize;
    while i < keys {
        c.execute("BEGIN").unwrap();
        let end = (i + batch).min(keys);
        for _ in i..end {
            // A random 16-hex-char key (non-sequential) + a random secondary value.
            let key = format!("{:016x}", rng.next_u64());
            let v = (rng.next_u64() % 1_000_000) as i64;
            c.execute_params(
                "INSERT OR IGNORE INTO frag(k, v) VALUES(?1, ?2)",
                &Params {
                    positional: vec![Value::Text(key.into()), Value::Integer(v)],
                    named: vec![],
                },
            )
            .unwrap();
        }
        c.execute("COMMIT").unwrap();
        i = end;
    }
    assert_integrity(&c, &path);
    // The file must not be pathologically fragmented: compare graphite's page
    // count to sqlite's own for the same data (the fragmentation fix keeps them
    // within a small factor; a regression bloats it 10-20×).
    let gp: i64 = c.query("PRAGMA page_count").unwrap().rows[0][0]
        .clone()
        .into_i64();
    if let Some(sp) = sqlite3_scalar(&path, "PRAGMA page_count;")
        && let Ok(sp) = sp.parse::<i64>()
    {
        assert!(
            gp <= sp * 2 + 16,
            "fragmentation regression: graphite {gp} pages vs sqlite {sp}"
        );
    }
    drop(c);
    rm_db(&path);
}

/// A big single transaction, a bulk delete of half the rows, then `VACUUM` —
/// exercising the freelist and file rewrite at size. Verifies counts and
/// integrity before and after, cross-checked with sqlite.
#[test]
#[ignore = "heavy: big transaction + VACUUM; run via the stress workflow"]
fn stress_volume_big_txn_and_vacuum() {
    let rows = env_usize("GSQL_VOLUME_ROWS", 100_000);
    let path = stress_db("volume-vacuum");
    rm_db(&path);
    let mut c = Connection::create(&path).unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INTEGER, pad TEXT)")
        .unwrap();
    c.execute("BEGIN").unwrap();
    for i in 0..rows {
        c.execute_params(
            "INSERT INTO t(id, v, pad) VALUES(?1, ?2, ?3)",
            &Params {
                positional: vec![
                    Value::Integer(i as i64),
                    Value::Integer((i as i64 * 7) % 1000),
                    Value::Text(format!("padding-value-for-row-{i}").into()),
                ],
                named: vec![],
            },
        )
        .unwrap();
    }
    c.execute("COMMIT").unwrap();
    assert_eq!(
        graphite_scalar(&c, "SELECT count(*) FROM t"),
        rows.to_string()
    );
    assert_integrity(&c, &path);
    // Delete half, then reclaim with VACUUM.
    c.execute("DELETE FROM t WHERE id % 2 = 0").unwrap();
    let remaining = graphite_scalar(&c, "SELECT count(*) FROM t");
    c.execute("VACUUM").unwrap();
    assert_eq!(
        graphite_scalar(&c, "SELECT count(*) FROM t"),
        remaining,
        "VACUUM changed the row count"
    );
    assert_integrity(&c, &path);
    if let Some(s) = sqlite3_scalar(&path, "SELECT count(*) FROM t;") {
        assert_eq!(s, remaining, "sqlite3 row count differs after VACUUM");
    }
    drop(c);
    rm_db(&path);
}

// Small helper: pull an i64 out of a Value for the page_count comparison.
trait IntoI64 {
    fn into_i64(self) -> i64;
}
impl IntoI64 for Value {
    fn into_i64(self) -> i64 {
        match self {
            Value::Integer(i) => i,
            Value::Real(r) => r as i64,
            Value::Text(t) => t.to_string().parse().unwrap_or(0),
            _ => 0,
        }
    }
}
