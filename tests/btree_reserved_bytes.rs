//! Regression: honor the per-page **reserved bytes** (file header byte 20, the
//! "bytes of unused reserved space at the end of each page" that SQLite exposes
//! via `SQLITE_FCNTL_RESERVE_BYTES`).
//!
//! The b-tree serializers used to pack cell content down from the *full* page
//! size, ignoring the reserved region at the page tail. On a database with
//! `reserved > 0` that wrote cell bytes INTO the reserved area, producing a file
//! that real sqlite3 rejects (`… Extends off end of page` / `Offset … out of
//! range … usable`), i.e. `database disk image is malformed`. The fix lays cell
//! content out only within `usable = page_size - reserved` while the on-disk page
//! buffer stays the full page size (the reserved tail is left zero).
//!
//! graphitesql has no public API to set the reserved-byte count yet (there is no
//! `sqlite3_file_control(SQLITE_FCNTL_RESERVE_BYTES)` binding), so — exactly like
//! a real consumer that sets it on a freshly created, still-empty database — we
//! create an empty db, stamp header byte 20, and then build the schema and rows.
//!
//! Each case asserts:
//!   * graphitesql's own `PRAGMA integrity_check` is `ok`,
//!   * sqlite3's `PRAGMA integrity_check` is `ok` (when the CLI is available) —
//!     this is what catches any cell that reaches into the reserved region,
//!   * every row survives with its exact bytes.
//!
//! A final case proves the fix is a strict no-op at `reserved = 0`: a database
//! built through the reserved-bytes path with byte 20 = 0 is byte-for-byte
//! identical to a plain database built by the same statements.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::collections::BTreeMap;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn graphite_check(c: &Connection) -> String {
    match &c.query("PRAGMA integrity_check").unwrap().rows[0][0] {
        Value::Text(s) => String::from(s.as_str()),
        _ => "?".into(),
    }
}

/// Tiny deterministic PRNG (SplitMix64) so the workload is reproducible without a
/// dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn pick(&mut self, choices: &[usize]) -> usize {
        choices[(self.next() as usize) % choices.len()]
    }
}

/// A deterministic value of `len` bytes over a quote-free alphabet, so it embeds
/// directly in an SQL string literal and its exact bytes can be re-verified.
fn value(tag: u64, len: usize) -> String {
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut s = String::with_capacity(len);
    let mut x = tag | 1;
    for _ in 0..len {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.push(alphabet[(x >> 33) as usize % alphabet.len()] as char);
    }
    s
}

/// Set file-header byte 20 (reserved-bytes-per-page) on the file at `path`.
fn stamp_reserved(path: &str, reserved: u8) {
    let mut bytes = std::fs::read(path).unwrap();
    assert!(
        bytes.len() >= 100,
        "expected an initialized 100-byte header, got {} bytes",
        bytes.len()
    );
    bytes[20] = reserved;
    std::fs::write(path, bytes).unwrap();
}

fn read_reserved(path: &str) -> u8 {
    std::fs::read(path).unwrap()[20]
}

/// Create an empty database file (header only, no schema rows), so the reserved
/// byte can be stamped before any cell is written — mirroring a consumer that
/// sets reserved bytes on a fresh, empty db.
fn create_empty(path: &str) {
    let _ = std::fs::remove_file(path);
    let c = Connection::create(path).unwrap();
    // A read-only pragma is enough to force the initialized header to disk.
    let _ = c.query("PRAGMA user_version").unwrap();
    drop(c);
}

/// The deterministic schema + workload, replayed identically for every reserved
/// size (and for the no-op comparison). Fills a WITHOUT ROWID table and a rowid
/// table carrying a secondary index, with blob sizes spanning tiny, near-usable,
/// and overflow, plus deletes and updates. Returns the expected surviving rows:
/// `(cache: k -> v)` and `(t: id -> a)`.
#[allow(clippy::type_complexity)]
fn build(c: &mut Connection) -> (BTreeMap<String, String>, BTreeMap<i64, String>) {
    c.execute("CREATE TABLE cache(k TEXT PRIMARY KEY, v BLOB) WITHOUT ROWID")
        .unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, b BLOB)")
        .unwrap();
    c.execute("CREATE INDEX t_a ON t(a)").unwrap();

    // Sizes spanning tiny, near-usable (a 4096 page with reserved leaves ~4050
    // local before overflow), and clearly-overflowing payloads.
    let sizes = [1usize, 12, 80, 400, 900, 1500, 3000, 4050, 4080, 9000];
    let mut cache: BTreeMap<String, String> = BTreeMap::new();
    let mut trows: BTreeMap<i64, String> = BTreeMap::new();

    let mut rng = Rng(0xC0FFEE);
    let mut tag: u64 = 0;
    for i in 1..=400i64 {
        tag += 1;
        let k = value(tag, 6 + (rng.next() as usize % 20));
        let v = value(tag ^ 0x5555, rng.pick(&sizes));
        c.execute(&format!("INSERT OR REPLACE INTO cache VALUES('{k}','{v}')"))
            .unwrap();
        cache.insert(k, v);

        tag += 1;
        let a = value(tag, 2 + (rng.next() as usize % 38));
        let b = value(tag ^ 0xAAAA, rng.pick(&sizes));
        c.execute(&format!("INSERT INTO t(id,a,b) VALUES({i},'{a}','{b}')"))
            .unwrap();
        trows.insert(i, a);
        let _ = b;
    }

    // Interleave deletes and updates that reshape both trees and rebuild the
    // secondary index.
    for step in 0..80i64 {
        let del = 1 + (rng.next() as i64 % 400);
        c.execute(&format!("DELETE FROM t WHERE id={del}")).unwrap();
        trows.remove(&del);

        let upd = 1 + (rng.next() as i64 % 400);
        if trows.contains_key(&upd) {
            tag += 1;
            let a = value(tag, 4 + (rng.next() as usize % 40));
            c.execute(&format!("UPDATE t SET a='{a}' WHERE id={upd}"))
                .unwrap();
            trows.insert(upd, a);
        }

        // Grow/shrink an existing cache row (WITHOUT ROWID in-place rewrite).
        if let Some((k, _)) = cache.iter().nth(step as usize % cache.len().max(1)) {
            let k = k.clone();
            tag += 1;
            let v = value(tag, rng.pick(&sizes));
            c.execute(&format!("UPDATE cache SET v='{v}' WHERE k='{k}'"))
                .unwrap();
            cache.insert(k, v);
        }
    }

    (cache, trows)
}

fn verify_rows(
    c: &Connection,
    cache: &BTreeMap<String, String>,
    trows: &BTreeMap<i64, String>,
    label: &str,
) {
    let got_cache = c.query("SELECT k, v FROM cache ORDER BY k").unwrap();
    assert_eq!(
        got_cache.rows.len(),
        cache.len(),
        "{label}: cache row count"
    );
    for (row, (k, v)) in got_cache.rows.iter().zip(cache.iter()) {
        let gk = text(&row[0]);
        let gv = text(&row[1]);
        assert_eq!(&gk, k, "{label}: cache key");
        assert_eq!(&gv, v, "{label}: cache value for {k}");
    }

    let got_t = c.query("SELECT id, a FROM t ORDER BY id").unwrap();
    assert_eq!(got_t.rows.len(), trows.len(), "{label}: t row count");
    for (row, (id, a)) in got_t.rows.iter().zip(trows.iter()) {
        let gid = match &row[0] {
            Value::Integer(i) => *i,
            other => panic!("{label}: id not integer: {other:?}"),
        };
        assert_eq!(gid, *id, "{label}: t id");
        assert_eq!(&text(&row[1]), a, "{label}: t.a for id {id}");
    }

    // The secondary index must return the same set through an index-driven query.
    let via_index = c.query("SELECT count(*) FROM t WHERE a >= '' ").unwrap();
    assert_eq!(
        via_index.rows[0][0],
        Value::Integer(trows.len() as i64),
        "{label}: index scan count"
    );
}

fn text(v: &Value) -> String {
    match v {
        Value::Text(s) => String::from(s.as_str()),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        other => panic!("expected text/blob, got {other:?}"),
    }
}

fn run_reserved_case(reserved: u8) {
    let path = std::env::temp_dir()
        .join(format!(
            "gsql-reserved-{reserved}-{}.db",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned();

    create_empty(&path);
    stamp_reserved(&path, reserved);

    let (cache, trows) = {
        let mut c = Connection::open(&path).unwrap();
        let rows = build(&mut c);
        assert_eq!(
            graphite_check(&c),
            "ok",
            "reserved={reserved}: graphite integrity_check"
        );
        verify_rows(&c, &rows.0, &rows.1, &format!("reserved={reserved}"));
        rows
    };

    // The reserved byte must survive every commit (page-1 header re-stamp).
    assert_eq!(
        read_reserved(&path),
        reserved,
        "reserved={reserved}: header byte 20 preserved"
    );

    // Reopen fresh (no cache) and re-verify the rows persisted on disk.
    {
        let c = Connection::open(&path).unwrap();
        assert_eq!(
            graphite_check(&c),
            "ok",
            "reserved={reserved}: integrity after reopen"
        );
        verify_rows(&c, &cache, &trows, &format!("reserved={reserved}-reopen"));
    }

    // The differential oracle: real sqlite3 must accept the file. Its
    // integrity_check is what catches a cell that reaches into the reserved tail.
    if sqlite3_available() {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check;")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "ok",
            "reserved={reserved}: sqlite3 integrity_check"
        );
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn reserved_1() {
    run_reserved_case(1);
}

#[test]
fn reserved_4() {
    run_reserved_case(4);
}

#[test]
fn reserved_8() {
    run_reserved_case(8);
}

#[test]
fn reserved_16() {
    run_reserved_case(16);
}

#[test]
fn reserved_32() {
    run_reserved_case(32);
}

/// reserved = 0 must be a strict no-op: a database built through the
/// reserved-bytes path with byte 20 = 0 is byte-for-byte identical to a plain
/// database built by the very same statements. Proves the fix does not perturb
/// any existing (default) database — `usable == page_size` when reserved is 0.
#[test]
fn reserved_zero_is_byte_identical_noop() {
    let plain = std::env::temp_dir()
        .join(format!("gsql-reserved-plain-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let stamped = std::env::temp_dir()
        .join(format!("gsql-reserved-zero-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&plain);

    // Route A: a plain database, reserved untouched (default 0).
    {
        let mut c = Connection::create(&plain).unwrap();
        let _ = build(&mut c);
        assert_eq!(graphite_check(&c), "ok", "plain integrity");
    }

    // Route B: identical statements, but through the reserved-bytes path with the
    // reserved byte explicitly stamped to 0 (a no-op stamp).
    create_empty(&stamped);
    stamp_reserved(&stamped, 0);
    {
        let mut c = Connection::open(&stamped).unwrap();
        let _ = build(&mut c);
        assert_eq!(graphite_check(&c), "ok", "stamped-zero integrity");
    }

    let a = std::fs::read(&plain).unwrap();
    let b = std::fs::read(&stamped).unwrap();
    assert_eq!(
        a, b,
        "reserved=0 output must be byte-identical to a plain database"
    );

    let _ = std::fs::remove_file(&plain);
    let _ = std::fs::remove_file(&stamped);
}
