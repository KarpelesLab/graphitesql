//! Roadmap D2e — FTS5 PREFIX-index DOUBLE-CASCADE crisis merge is BYTE-IDENTICAL to
//! sqlite.
//!
//! A prefix-configured fts5 table (`prefix='2 3'`) whose documents share a short
//! literal prefix (`common1`, `common2`, …) makes each prefix-index term (`co`,
//! `com`, …) cover every row, so its doclist spans many continuation leaves plus a
//! doclist-index page. Loading enough rows crosses the 16-segment crisis threshold
//! at level 0 TWICE across the load: the first crisis merges 16 level-0 segments
//! into a level-1 segment; a later second crisis fires while that level-1 segment
//! already exists.
//!
//! graphite now services the prefix write path with the SAME faithful incremental
//! merge as the main index (`fts5IndexMergeLevel` port over the FULL keys): a
//! level-0 crisis merges ONLY that level's segments into a fresh segment at the next
//! level, leaving the earlier merged segment intact. So a double cascade yields TWO
//! level-1 segments — byte-for-byte identical to sqlite's own crisis merges (which
//! is what produces the two segments here; with each 250-row batch = one level-0
//! leaf, the work-unit automerge never fires, so both engines reach two level-1
//! segments purely through two crisis merges).
//!
//! This test asserts full BYTE-PARITY of `%_data` and `%_idx` against a stock
//! sqlite3 3.50.4 oracle loaded with the identical statements, plus validity (both
//! integrity checkers accept the file) and correct MATCH.

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

/// Run `sql` against a fresh stock-sqlite3 database at `path`.
fn sqlite_run(path: &str, sql: &str) {
    let o = Command::new("sqlite3")
        .arg(path)
        .arg(sql)
        .output()
        .expect("spawn sqlite3");
    assert!(
        o.status.success(),
        "sqlite3 load failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

/// One `col1|col2|…`-joined row per output row of `query` from stock sqlite3.
fn sqlite_rows(path: &str, query: &str) -> Vec<String> {
    let o = Command::new("sqlite3")
        .arg("-noheader")
        .arg(path)
        .arg(query)
        .output()
        .expect("spawn sqlite3");
    assert!(o.status.success());
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// The same `col1|col2|…` rows produced by graphite for `query`.
fn graphite_rows(c: &Connection, query: &str) -> Vec<String> {
    let rs = c.query(query).unwrap();
    rs.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    graphitesql::Value::Text(t) => String::from(t.as_str()),
                    graphitesql::Value::Integer(i) => i.to_string(),
                    graphitesql::Value::Real(r) => r.to_string(),
                    graphitesql::Value::Null => String::new(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
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
/// table, every row sharing the literal prefix `common`, into BOTH graphite and a
/// stock-sqlite3 oracle, then assert byte-parity of `%_data`/`%_idx`, validity, and
/// that a `com*` prefix MATCH returns every row.
fn assert_double_cascade_byte_identical(tag: &str, prefix_cfg: &str, batches: i64) {
    if !have_fts5_sqlite() {
        return;
    }
    let g = tmp_path(tag);
    let s = tmp_path(&format!("{tag}-oracle"));
    let ddl = format!("CREATE VIRTUAL TABLE f USING fts5(a, prefix='{prefix_cfg}')");

    // Build the identical load script (DDL + one autocommit INSERT per batch).
    let mut script = format!("{ddl};\n");
    for b in 0..batches {
        let lo = b * 250 + 1;
        let hi = b * 250 + 250;
        script.push_str(&format!(
            "INSERT INTO f(rowid,a) SELECT value,'common'||value \
             FROM generate_series({lo},{hi});\n"
        ));
    }

    // graphite.
    let mut c = Connection::create(&g).unwrap();
    for stmt in script.split(";\n").filter(|s| !s.trim().is_empty()) {
        c.execute(stmt).unwrap();
    }
    // sqlite oracle.
    sqlite_run(&s, &script);

    let total = batches * 250;

    // Prove the load actually double-cascaded: the file must hold at least two
    // distinct leaf segments (the two level-1 merges), not one collapsed rebuild —
    // exactly the shape sqlite keeps. (A single crisis leaves one segment.)
    let n_segids = graphite_i64(
        &c,
        "SELECT count(distinct id >> 37) FROM f_data WHERE id > 10",
    );
    assert!(
        n_segids >= 2,
        "{tag}: expected >= 2 leaf segments (double cascade) but found {n_segids}"
    );

    // BYTE-PARITY — the whole `%_data` segment store, structure record included.
    let g_data = graphite_rows(&c, "SELECT id, quote(block) FROM f_data ORDER BY id");
    let s_data = sqlite_rows(&s, "SELECT id, quote(block) FROM f_data ORDER BY id");
    assert_eq!(
        g_data.len(),
        s_data.len(),
        "{tag}: f_data row count differs (graphite {} vs sqlite {})",
        g_data.len(),
        s_data.len()
    );
    assert_eq!(g_data, s_data, "{tag}: f_data not byte-identical to sqlite");

    // BYTE-PARITY — the `%_idx` b-tree pointer rows.
    let g_idx = graphite_rows(
        &c,
        "SELECT segid, quote(term), pgno FROM f_idx ORDER BY segid, term, pgno",
    );
    let s_idx = sqlite_rows(
        &s,
        "SELECT segid, quote(term), pgno FROM f_idx ORDER BY segid, term, pgno",
    );
    assert_eq!(g_idx, s_idx, "{tag}: f_idx not byte-identical to sqlite");

    // VALIDITY — graphite's own integrity check accepts the file.
    assert_eq!(
        graphite_text(&c, "PRAGMA integrity_check"),
        "ok",
        "{tag}: graphite integrity_check failed"
    );
    // MATCH correctness on the shared prefix: `com*` must find every row.
    let got = graphite_i64(&c, "SELECT count(*) FROM f WHERE f MATCH 'com*'");
    assert_eq!(got, total, "{tag}: MATCH 'com*' count wrong");

    // Drop the Connection so sqlite opens a fully-flushed file.
    drop(c);
    // VALIDITY — stock sqlite3 accepts graphite's file.
    assert!(
        sqlite_quick_check_ok(&g),
        "{tag}: sqlite quick_check rejected graphite's double-cascade file"
    );

    let _ = std::fs::remove_file(&g);
    let _ = std::fs::remove_file(&s);
}

#[test]
fn prefix_double_cascade_32x250_prefix_2_3() {
    // 32 autocommit batches of 250 rows (8000 rows), prefix='2 3': two level-0
    // crises across the load, the second while a level-1 segment already exists.
    assert_double_cascade_byte_identical("dc-32x250-p23", "2 3", 32);
}

#[test]
fn prefix_double_cascade_40x250_prefix_2_3() {
    // 40 batches (10000 rows), prefix='2 3': a larger double cascade.
    assert_double_cascade_byte_identical("dc-40x250-p23", "2 3", 40);
}

#[test]
fn prefix_double_cascade_32x250_prefix_2() {
    // Single prefix length.
    assert_double_cascade_byte_identical("dc-32x250-p2", "2", 32);
}

#[test]
fn prefix_double_cascade_32x250_prefix_2_3_4() {
    // Three prefix lengths.
    assert_double_cascade_byte_identical("dc-32x250-p234", "2 3 4", 32);
}
