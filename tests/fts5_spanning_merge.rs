//! Roadmap D2e-3 — FTS5 incremental-merge SPANNING (doclist-index) byte-parity.
//!
//! When several level-0 segments that all carry one very common term are
//! automerged/crisismerged, the merged segment's doclist for that term spills
//! across many continuation leaves and grows a doclist-index (`dlidx`) page — a
//! "spanning" segment. sqlite's incremental-merge writer (`fts5IndexMergeLevel`)
//! packs those spanning leaves differently from the bulk `fts5FlushOneHash`
//! writer: it appends each position-list's SIZE varint DIRECTLY to the leaf buffer
//! (no page-full check, so the leaf may momentarily exceed pgsz) and streams only
//! the position DATA through `fts5WriteAppendPoslistData`, whose spill loop still
//! flushes when the page is exactly (or over) full. graphite previously streamed
//! the whole poslist (size+content) through the spill loop and skipped the
//! exactly-full flush, so its merged spanning leaves diverged from sqlite's by one
//! leaf boundary even though the file stayed correct.
//!
//! This test drives autocommit inserts of a shared term through graphite and stock
//! `sqlite3` (3.50.4, FTS5), asserts the raw `%_data`/`%_idx` bytes are
//! BYTE-IDENTICAL, that a doclist-index page is actually present (so the spanning
//! path is exercised), that graphite's `integrity_check` is clean and sqlite
//! accepts graphite's file, and that MATCH agrees. Skipped when `sqlite3` with
//! FTS5 is not on PATH.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-spanmerge-{}-{}-{}.db",
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

fn sqlite_raw(path: &str, q: &str) -> String {
    let o = Command::new("sqlite3").arg(path).arg(q).output().unwrap();
    assert!(
        o.status.success(),
        "sqlite3 failed for {q:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn sqlite_integrity_ok(path: &str) -> bool {
    Command::new("sqlite3")
        .arg(path)
        .arg("INSERT INTO f(f) VALUES('integrity-check');")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn dump_data_sqlite(path: &str) -> String {
    let data = sqlite_raw(path, "SELECT id, quote(block) FROM f_data ORDER BY id;");
    let idx = sqlite_raw(
        path,
        "SELECT segid, quote(term), pgno FROM f_idx ORDER BY segid, term, pgno;",
    );
    format!("DATA\n{data}\nIDX\n{idx}")
}

fn dump_data_graphite(c: &Connection) -> String {
    let fmt = |r: graphitesql::QueryResult| -> String {
        r.rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| match v {
                        graphitesql::Value::Integer(i) => i.to_string(),
                        graphitesql::Value::Text(t) => String::from(t.as_str()),
                        graphitesql::Value::Null => String::new(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let data = fmt(c
        .query("SELECT id, quote(block) FROM f_data ORDER BY id")
        .unwrap());
    let idx = fmt(c
        .query("SELECT segid, quote(term), pgno FROM f_idx ORDER BY segid, term, pgno")
        .unwrap());
    format!("DATA\n{data}\nIDX\n{idx}")
}

fn graphite_i64(c: &Connection, q: &str) -> i64 {
    match &c.query(q).unwrap().rows[0][0] {
        graphitesql::Value::Integer(i) => *i,
        _ => -1,
    }
}

/// Insert `batches` autocommit statements of `per` rows each (rowids contiguous),
/// every row carrying the shared term `common`, so the incremental merge produces
/// a spanning (doclist-index) segment. Assert byte-parity of the fts5 index.
fn assert_spanning_merge_identical(tag: &str, batches: i64, per: i64) {
    if !have_fts5_sqlite() {
        return;
    }
    let g = tmp_path(&format!("{tag}-g"));
    let s = tmp_path(&format!("{tag}-s"));

    let ddl = "CREATE VIRTUAL TABLE f USING fts5(a)";
    let mut stmts: Vec<String> = Vec::new();
    for b in 0..batches {
        let lo = b * per + 1;
        let hi = b * per + per;
        stmts.push(format!(
            "INSERT INTO f(rowid,a) SELECT value,'common u'||value \
             FROM generate_series({lo},{hi})"
        ));
    }

    let mut c = Connection::create(&g).unwrap();
    c.execute(ddl).unwrap();
    for st in &stmts {
        c.execute(st).unwrap();
    }
    sqlite_raw(&s, &format!("{ddl};"));
    for st in &stmts {
        sqlite_raw(&s, &format!("{st};"));
    }

    // The doclist-index rowid has bit 36 (dlidx) set: prove the corpus actually
    // built a spanning segment so this test exercises the merge dlidx path.
    let n_dlidx = graphite_i64(&c, "SELECT count(*) FROM f_data WHERE (id >> 36) & 1 = 1");
    assert!(
        n_dlidx > 0,
        "{tag}: expected a doclist-index (spanning) page but found none"
    );

    // Byte-parity of the fts5 segment structure — the D2e-3 deliverable. Read
    // graphite's shadows THROUGH graphite so the comparison does not depend on
    // sqlite opening graphite's file.
    let gs = dump_data_graphite(&c);
    let ss = dump_data_sqlite(&s);
    assert_eq!(gs, ss, "{tag}: spanning-merge segment bytes diverge");

    // graphite's own integrity check must accept the file, and — since this is a
    // pure-insert corpus — so must stock sqlite3's fts5 integrity-check.
    let gic = match &c.query("PRAGMA integrity_check").unwrap().rows[0][0] {
        graphitesql::Value::Text(t) => String::from(t.as_str()),
        _ => String::new(),
    };
    assert_eq!(gic, "ok", "{tag}: graphite integrity_check failed");
    assert!(
        sqlite_integrity_ok(&g),
        "{tag}: sqlite integrity-check rejected graphite's file"
    );

    // MATCH agreement on the spanning term and a rare one.
    for term in ["common", "u1", "u9999"] {
        let gq = c
            .query(&format!(
                "SELECT rowid FROM f WHERE f MATCH '{term}' ORDER BY rowid"
            ))
            .unwrap();
        let grow: Vec<i64> = gq
            .rows
            .iter()
            .map(|r| match &r[0] {
                graphitesql::Value::Integer(i) => *i,
                _ => -1,
            })
            .collect();
        let srow: Vec<i64> = sqlite_raw(
            &s,
            &format!("SELECT rowid FROM f WHERE f MATCH '{term}' ORDER BY rowid;"),
        )
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().unwrap())
        .collect();
        assert_eq!(grow, srow, "{tag}: MATCH '{term}' rows diverge");
    }

    let _ = std::fs::remove_file(&g);
    let _ = std::fs::remove_file(&s);
}

#[test]
fn spanning_merge_20x500_shared_term() {
    // 20 autocommit batches of 500 rows (10000 rows) sharing term `common`: the
    // level-0 segments crisismerge/automerge into a segment whose `common` doclist
    // spans many leaves and grows a doclist-index page.
    assert_spanning_merge_identical("span20x500", 20, 500);
}

#[test]
fn spanning_merge_16x500_shared_term() {
    // A different batch shape (16 batches of 500, 8000 rows) also crisismerges into
    // a spanning (doclist-index) segment, at a different set of leaf boundaries.
    assert_spanning_merge_identical("span16x500", 16, 500);
}

/// Like [`assert_spanning_merge_identical`] but for a PREFIX-configured table. A
/// prefix index's short-prefix term (e.g. `co` for `prefix='2'`) has a doclist
/// covering EVERY row, so it spans many continuation leaves and grows a
/// doclist-index page inside the incrementally (crisis)merged segment. That
/// spanning prefix term previously took the bulk (non-merge) leaf writer and split
/// its leaves at different byte offsets than sqlite's `fts5IndexMergeLevel`; the
/// crisis-merge now routes the whole prefix segment through the merge writer so the
/// bytes match. Single-crisis shapes only (a SECOND cascade crisis over multi-page
/// prefix segments is the separately-tracked D2e automerge-cascade gap).
fn assert_prefix_spanning_merge_identical(tag: &str, prefix: &str, batches: i64, per: i64) {
    if !have_fts5_sqlite() {
        return;
    }
    let g = tmp_path(&format!("{tag}-g"));
    let s = tmp_path(&format!("{tag}-s"));

    let ddl = format!("CREATE VIRTUAL TABLE f USING fts5(a, prefix='{prefix}')");
    let mut stmts: Vec<String> = Vec::new();
    for b in 0..batches {
        let lo = b * per + 1;
        let hi = b * per + per;
        stmts.push(format!(
            "INSERT INTO f(rowid,a) SELECT value,'common'||value \
             FROM generate_series({lo},{hi})"
        ));
    }

    let mut c = Connection::create(&g).unwrap();
    c.execute(&ddl).unwrap();
    for st in &stmts {
        c.execute(st).unwrap();
    }
    sqlite_raw(&s, &format!("{ddl};"));
    for st in &stmts {
        sqlite_raw(&s, &format!("{st};"));
    }

    // Prove a doclist-index (spanning) page exists so the merge dlidx path is hit.
    let n_dlidx = graphite_i64(&c, "SELECT count(*) FROM f_data WHERE (id >> 36) & 1 = 1");
    assert!(
        n_dlidx > 0,
        "{tag}: expected a doclist-index (spanning) page but found none"
    );

    // Byte-parity of the whole fts5 segment (main `'0'` + prefix `'1'`/`'2'`… term
    // streams), read through graphite so the comparison is engine-independent.
    let gs = dump_data_graphite(&c);
    let ss = dump_data_sqlite(&s);
    assert_eq!(gs, ss, "{tag}: prefix spanning-merge segment bytes diverge");

    let gic = match &c.query("PRAGMA integrity_check").unwrap().rows[0][0] {
        graphitesql::Value::Text(t) => String::from(t.as_str()),
        _ => String::new(),
    };
    assert_eq!(gic, "ok", "{tag}: graphite integrity_check failed");
    assert!(
        sqlite_integrity_ok(&g),
        "{tag}: sqlite integrity-check rejected graphite's file"
    );

    // MATCH agreement on the spanning bare term and its prefix queries.
    for term in ["common", "co*", "com*"] {
        let gq = c
            .query(&format!(
                "SELECT rowid FROM f WHERE f MATCH '{term}' ORDER BY rowid"
            ))
            .unwrap();
        let grow: Vec<i64> = gq
            .rows
            .iter()
            .map(|r| match &r[0] {
                graphitesql::Value::Integer(i) => *i,
                _ => -1,
            })
            .collect();
        let srow: Vec<i64> = sqlite_raw(
            &s,
            &format!("SELECT rowid FROM f WHERE f MATCH '{term}' ORDER BY rowid;"),
        )
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().unwrap())
        .collect();
        assert_eq!(grow, srow, "{tag}: MATCH '{term}' rows diverge");
    }

    let _ = std::fs::remove_file(&g);
    let _ = std::fs::remove_file(&s);
}

#[test]
fn prefix_spanning_merge_20x500_p23() {
    // The canonical repro: 20x500 shared-prefix rows, prefix='2 3'. Both the main
    // `'0'` and the two prefix indexes' spanning doclists must match sqlite byte-
    // for-byte through the crisis merge.
    assert_prefix_spanning_merge_identical("pspan20x500-p23", "2 3", 20, 500);
}

#[test]
fn prefix_spanning_merge_16x500_p2() {
    // A single prefix index (`prefix='2'`), single-crisis 16x500 (8000 rows).
    assert_prefix_spanning_merge_identical("pspan16x500-p2", "2", 16, 500);
}

#[test]
fn prefix_spanning_merge_16x500_p234() {
    // Three prefix indexes (`prefix='2 3 4'`), single-crisis 16x500 (8000 rows):
    // three spanning prefix streams plus the main stream in one merged segment.
    assert_prefix_spanning_merge_identical("pspan16x500-p234", "2 3 4", 16, 500);
}
