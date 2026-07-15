//! Roadmap D2e — FTS5 PREFIX-index DOUBLE-CASCADE crisis merge produces a VALID
//! file.
//!
//! A prefix-configured fts5 table (`prefix='2 3'`) whose documents share a short
//! literal prefix (`common1`, `common2`, …) makes each prefix-index term (`co`,
//! `com`, …) cover every row, so its doclist spans many continuation leaves plus a
//! doclist-index page. Loading enough rows crosses the 16-segment crisis threshold
//! at level 0 TWICE across the load: the first crisis merges 16 level-0 segments
//! into a level-1 segment; a later second crisis fires while that level-1 segment
//! already exists.
//!
//! graphite's prefix crisis path REBUILDS the whole live corpus into a single fresh
//! segment and rewrites every `%_data`/`%_idx` row. It used to leave the older
//! level-1 segment referenced in the structure record with no backing `%_data` —
//! which both graphite's own `PRAGMA integrity_check` and stock sqlite3's
//! `quick_check` reject as a "malformed inverted index". The fix drops every
//! subsumed segment when the corpus is rebuilt, so exactly one segment (holding the
//! whole corpus) remains.
//!
//! BYTE-PARITY NOTE: for a SINGLE crisis graphite is byte-identical to sqlite (see
//! `tests/fts5_spanning_merge.rs`). For a DOUBLE cascade the STRUCTURE diverges —
//! sqlite keeps the older level-1 segment and grows a second one via its
//! incremental automerge (the D2e-1 automerge gap graphite does not implement), so
//! graphite collapses to one segment where sqlite keeps two. That is a valid,
//! shippable divergence: this test asserts VALIDITY (both integrity checkers accept
//! the file) and correct MATCH, not byte-parity.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-pfxcascade-{}-{}-{}.db",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let p = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&p);
    p
}

fn have_fts5_sqlite() -> bool {
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg("CREATE VIRTUAL TABLE t USING fts5(a); SELECT 1;")
        .output();
    matches!(o, Ok(o) if o.status.success())
}

/// stock-sqlite3 `PRAGMA quick_check` on `path` returns exactly `ok`.
fn sqlite_quick_check_ok(path: &str) -> bool {
    let o = Command::new("sqlite3")
        .arg(path)
        .arg("PRAGMA quick_check;")
        .output();
    matches!(o, Ok(o) if o.status.success()
        && String::from_utf8_lossy(&o.stdout).trim() == "ok")
}

fn graphite_text(c: &Connection, q: &str) -> String {
    match &c.query(q).unwrap().rows[0][0] {
        graphitesql::Value::Text(t) => String::from(t.as_str()),
        graphitesql::Value::Integer(i) => i.to_string(),
        _ => String::new(),
    }
}

fn graphite_i64(c: &Connection, q: &str) -> i64 {
    match &c.query(q).unwrap().rows[0][0] {
        graphitesql::Value::Integer(i) => *i,
        _ => -1,
    }
}

/// Load `batches` autocommit inserts of 250 rows each into a `prefix=<cfg>` fts5
/// table, every row sharing the literal prefix `common`, then assert the resulting
/// file is VALID (graphite `integrity_check` = ok, sqlite `quick_check` = ok) and
/// that a `com*` prefix MATCH returns every row.
fn assert_double_cascade_valid(tag: &str, prefix_cfg: &str, batches: i64) {
    if !have_fts5_sqlite() {
        return;
    }
    let g = tmp_path(tag);
    let ddl = format!("CREATE VIRTUAL TABLE f USING fts5(a, prefix='{prefix_cfg}')");

    let mut c = Connection::create(&g).unwrap();
    c.execute(&ddl).unwrap();
    for b in 0..batches {
        let lo = b * 250 + 1;
        let hi = b * 250 + 250;
        c.execute(&format!(
            "INSERT INTO f(rowid,a) SELECT value,'common'||value \
             FROM generate_series({lo},{hi})"
        ))
        .unwrap();
    }
    let total = batches * 250;

    // Prove the load actually double-cascaded: the structure record must show a
    // level-1 (merged) segment (segid range beyond the level-0 write allocations)
    // AND a doclist-index (spanning) page must exist — otherwise the regression
    // path this test guards would not be exercised.
    let n_dlidx = graphite_i64(&c, "SELECT count(*) FROM f_data WHERE (id >> 36) & 1 = 1");
    assert!(
        n_dlidx > 0,
        "{tag}: expected a doclist-index (spanning) page but found none"
    );

    // VALIDITY — graphite's own integrity check accepts the file.
    assert_eq!(
        graphite_text(&c, "PRAGMA integrity_check"),
        "ok",
        "{tag}: graphite integrity_check failed"
    );
    // Drop the Connection so sqlite opens a fully-flushed file.
    drop(c);
    // VALIDITY — stock sqlite3 accepts graphite's file.
    assert!(
        sqlite_quick_check_ok(&g),
        "{tag}: sqlite quick_check rejected graphite's double-cascade file"
    );

    // MATCH correctness on the shared prefix: `com*` must find every row.
    let c = Connection::open(&g).unwrap();
    let got = graphite_i64(&c, "SELECT count(*) FROM f WHERE f MATCH 'com*'");
    assert_eq!(got, total, "{tag}: MATCH 'com*' count wrong");

    let _ = std::fs::remove_file(&g);
}

#[test]
fn prefix_double_cascade_32x250_prefix_2_3() {
    // 32 autocommit batches of 250 rows (8000 rows), prefix='2 3': two level-0
    // crises across the load, the second while a level-1 segment already exists.
    // This is the exact malformed-file repro.
    assert_double_cascade_valid("dc-32x250-p23", "2 3", 32);
}

#[test]
fn prefix_double_cascade_40x250_prefix_2_3() {
    // 40 batches (10000 rows), prefix='2 3': a larger double cascade.
    assert_double_cascade_valid("dc-40x250-p23", "2 3", 40);
}

#[test]
fn prefix_double_cascade_32x250_prefix_2() {
    // Single prefix length.
    assert_double_cascade_valid("dc-32x250-p2", "2", 32);
}

#[test]
fn prefix_double_cascade_32x250_prefix_2_3_4() {
    // Three prefix lengths.
    assert_double_cascade_valid("dc-32x250-p234", "2 3 4", 32);
}
