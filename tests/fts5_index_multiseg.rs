//! Roadmap D2b (multi-segment): the bare-term / boolean / prefix `MATCH` routes
//! now read an FTS5 index with MORE THAN ONE height-0 segment, not just the single
//! bulk-rebuilt segment graphite's own writer produces. A stock `sqlite3` FTS5
//! table accumulates several segments after many inserts (in separate
//! transactions, until `'optimize'`/merge), so these tests build such a file with
//! `sqlite3` itself, confirm the structure record really lists >1 segment, and
//! assert graphite's index-routed `MATCH` returns exactly the rowid set `sqlite3`'s
//! own reader returns.
//!
//! For pure-insert histories the per-segment doclists are UNIONed (each docid lives
//! in one segment). When the file also has deletes/updates — a tombstone or a docid
//! shadowed across segments — the merge BAILS and graphite falls back to the
//! `_content` scan, which still computes the correct live set; a deletes-present
//! case asserts that correctness holds either way.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::{Connection, Value};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("gsql-d2bms-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(format!("idx-{}.db", SEQ.fetch_add(1, Ordering::Relaxed)));
    let p = p.to_string_lossy().into_owned();
    cleanup(&p);
    p
}

/// Remove the db file and any sidecar `-wal`/`-journal` for a clean slate.
fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run one `sqlite3` statement against the file, asserting success.
fn sqlite_exec(path: &str, sql: &str) {
    let o = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
    assert!(
        o.status.success(),
        "sqlite3 failed for {sql:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

/// sqlite3's MATCH over the file, as a sorted, `,`-joined string of rowids.
fn sqlite_match(path: &str, query: &str) -> String {
    let q = format!("SELECT rowid FROM t WHERE t MATCH '{query}' ORDER BY rowid;");
    let o = Command::new("sqlite3").arg(path).arg(&q).output().unwrap();
    assert!(
        o.status.success(),
        "sqlite3 failed for {q:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let mut v: Vec<i64> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().unwrap())
        .collect();
    v.sort_unstable();
    join(&v)
}

/// graphite's MATCH over the file, as a sorted, `,`-joined string of rowids.
fn graphite_match(c: &Connection, query: &str) -> String {
    let sql = format!("SELECT rowid FROM t WHERE t MATCH '{query}' ORDER BY rowid");
    let mut v: Vec<i64> = c
        .query(&sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref other => panic!("non-integer rowid: {other:?}"),
        })
        .collect();
    v.sort_unstable();
    join(&v)
}

fn join(v: &[i64]) -> String {
    v.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The number of segments the structure record (`%_data` id 10) lists, decoded with
/// `sqlite3` itself via a tiny shell of `hex()` is awkward; instead we read the
/// `nSegment` count straight out of the blob bytes. Layout: 4-byte cookie, then
/// varints `nLevel`, `nSegment`, …. We only need `nSegment`.
fn segment_count(path: &str) -> u64 {
    let o = Command::new("sqlite3")
        .arg(path)
        .arg("SELECT hex(block) FROM t_data WHERE id=10;")
        .output()
        .unwrap();
    assert!(o.status.success());
    let hex = String::from_utf8_lossy(&o.stdout);
    let hex = hex.trim();
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    // Skip the 4-byte cookie, read nLevel (varint) then nSegment (varint).
    let mut pos = 4usize;
    let _n_level = read_varint(&bytes, &mut pos);
    read_varint(&bytes, &mut pos)
}

/// Minimal sqlite varint decoder for the structure-record header.
fn read_varint(b: &[u8], pos: &mut usize) -> u64 {
    let mut val: u64 = 0;
    for i in 0..9 {
        let c = b[*pos];
        *pos += 1;
        if i == 8 {
            return (val << 8) | c as u64;
        }
        val = (val << 7) | (c & 0x7f) as u64;
        if c & 0x80 == 0 {
            return val;
        }
    }
    val
}

/// Build a multi-segment FTS5 table with `sqlite3`: each INSERT is its own
/// transaction (one shell invocation) so a fresh level-0 segment is flushed per
/// insert and they are NOT merged to one. No `'optimize'` is run.
fn build_multiseg(path: &str, docs: &[(i64, &str)]) {
    sqlite_exec(path, "CREATE VIRTUAL TABLE t USING fts5(body);");
    for (rowid, body) in docs {
        sqlite_exec(
            path,
            &format!("INSERT INTO t(rowid, body) VALUES ({rowid}, '{body}');"),
        );
    }
}

/// A corpus of distinct, pure-insert documents. "alpha"/"beta" recur across many
/// docs; "fox"/"hen" are sparse; "appliance"/"apple"/"apply" share the "app"
/// prefix; "zebra" never appears.
const DOCS: &[(i64, &str)] = &[
    (1, "alpha beta apple"),
    (2, "alpha gamma apply"),
    (3, "beta delta appliance"),
    (4, "alpha beta gamma"),
    (5, "fox runs fast"),
    (6, "alpha hen house"),
    (7, "beta beta beta"),
    (8, "gamma delta apple"),
    (9, "alpha fox apply"),
    (10, "hen and beta"),
    (11, "alpha apple appliance"),
    (12, "gamma gamma gamma"),
    (13, "beta fox hen"),
    (14, "alpha beta delta"),
    (15, "apply now apple later"),
];

#[test]
fn multiseg_pure_insert_bare_boolean_prefix_match_equals_sqlite() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    build_multiseg(&path, DOCS);

    // The whole point: sqlite must have left MORE THAN ONE segment in the index.
    let nseg = segment_count(&path);
    assert!(
        nseg > 1,
        "expected a multi-segment index, sqlite left nSegment={nseg}"
    );

    let c = Connection::open(&path).unwrap();
    let queries = [
        // bare terms (single-occurrence, multi-doc, sparse, absent)
        "alpha",
        "beta",
        "fox",
        "hen",
        "zebra",
        // boolean trees of bare terms
        "alpha AND beta",
        "alpha OR fox",
        "alpha AND beta AND gamma",
        "alpha OR beta OR hen",
        "alpha NOT beta",
        "(alpha OR fox) AND beta",
        "beta NOT fox",
        // prefix terms
        "app*",
        "appl*",
        "alph*",
        "ga*",
        "zz*",
    ];
    for q in queries {
        let g = graphite_match(&c, q);
        let s = sqlite_match(&path, q);
        assert_eq!(g, s, "query {q:?}: graphite {g:?} != sqlite {s:?}");
    }
    cleanup(&path);
}

#[test]
fn multiseg_with_deletes_and_updates_still_correct() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    build_multiseg(&path, DOCS);
    // Now introduce deletes and an update, each in its own transaction, so the
    // index gains a tombstone/shadowing segment. graphite's merge will BAIL on the
    // tombstone (or the docid shared across segments) and fall back to the
    // `_content` scan — which must still return sqlite's exact live set.
    sqlite_exec(&path, "DELETE FROM t WHERE rowid = 1;"); // drop "alpha beta apple"
    sqlite_exec(&path, "DELETE FROM t WHERE rowid = 7;"); // drop "beta beta beta"
    sqlite_exec(
        &path,
        "UPDATE t SET body = 'alpha replaced text' WHERE rowid = 9;",
    ); // 9 loses "fox"/"apply", gains nothing new

    let nseg = segment_count(&path);
    assert!(
        nseg > 1,
        "expected a multi-segment index after edits, nSegment={nseg}"
    );

    let c = Connection::open(&path).unwrap();
    let queries = [
        "alpha",
        "beta",
        "fox",
        "apple",
        "apply",
        "hen",
        "alpha AND beta",
        "alpha OR fox",
        "app*",
        "appl*",
    ];
    for q in queries {
        let g = graphite_match(&c, q);
        let s = sqlite_match(&path, q);
        assert_eq!(
            g, s,
            "deletes/updates query {q:?}: graphite {g:?} != sqlite {s:?}"
        );
    }
    cleanup(&path);
}

#[test]
fn multiseg_column_scoped_match_equals_sqlite() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    // Two columns so column-scoped `col : term` / `col : pre*` routes are exercised
    // across multiple segments.
    sqlite_exec(&path, "CREATE VIRTUAL TABLE t USING fts5(title, body);");
    let docs: &[(i64, &str, &str)] = &[
        (1, "alpha title", "beta body apple"),
        (2, "beta title", "alpha body apply"),
        (3, "gamma alpha", "delta appliance"),
        (4, "fox title", "alpha beta body"),
        (5, "hen alpha", "fox runs apple"),
        (6, "beta gamma", "alpha hen apply"),
        (7, "alpha alpha", "beta beta apple"),
        (8, "delta title", "gamma alpha appliance"),
    ];
    for (rowid, title, body) in docs {
        sqlite_exec(
            &path,
            &format!("INSERT INTO t(rowid, title, body) VALUES ({rowid}, '{title}', '{body}');"),
        );
    }
    let nseg = segment_count(&path);
    assert!(nseg > 1, "expected multi-segment, nSegment={nseg}");

    let c = Connection::open(&path).unwrap();
    let queries = [
        "title : alpha",
        "body : alpha",
        "title : beta",
        "body : apple",
        "title : alph*",
        "body : app*",
    ];
    for q in queries {
        let g = graphite_match(&c, q);
        let s = sqlite_match(&path, q);
        assert_eq!(g, s, "column query {q:?}: graphite {g:?} != sqlite {s:?}");
    }
    cleanup(&path);
}
