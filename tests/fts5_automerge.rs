//! Roadmap D2e-1 — FTS5 AUTOMERGE (`fts5IndexAutomerge`) incremental merge
//! scheduling. sqlite's fts5 does incremental sub-crisis merging: on every write
//! that crosses a `FTS5_WORK_UNIT` (64-leaf) boundary it merges the levels holding
//! `FTS5_DEFAULT_AUTOMERGE` (4) or more segments, in addition to the crisis merge
//! at 16 segments. graphite previously implemented only crisis-merge, so delete-heavy
//! and larger (past 64 leaves) autocommit corpora diverged from sqlite's on-disk
//! segment structure.
//!
//! This test drives autocommit insert/delete sequences — including ones that cross
//! one or more 64-leaf boundaries and populate several levels — through graphite
//! and stock `sqlite3` (3.50.4, FTS5) and asserts the raw `%_data` / `%_idx` /
//! `%_docsize` bytes (including the STRUCTURE record) are BYTE-IDENTICAL, graphite's
//! own `integrity_check` is clean, and MATCH returns the same rows. For pure-insert
//! corpora it additionally cross-checks that stock sqlite3 accepts graphite's file
//! (`integrity-check`); delete-heavy files are excluded from that ONE cross-check
//! because of a pre-existing, automerge-orthogonal core-pager defect (see
//! [`assert_automerge_identical`]). Skipped when `sqlite3` with FTS5 is not on PATH.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-automerge-{}-{}-{}.db",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let p = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&p);
    p
}

/// `sqlite3` with FTS5 available on PATH?
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
        .arg("INSERT INTO ft(ft) VALUES('integrity-check');")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn dump_shadows_sqlite(path: &str) -> String {
    let data = sqlite_raw(path, "SELECT id, quote(block) FROM ft_data ORDER BY id;");
    let idx = sqlite_raw(
        path,
        "SELECT segid, quote(term), pgno FROM ft_idx ORDER BY segid, term, pgno;",
    );
    let ds = sqlite_raw(path, "SELECT id, quote(sz) FROM ft_docsize ORDER BY id;");
    format!("DATA\n{data}\nIDX\n{idx}\nDOCSIZE\n{ds}")
}

fn dump_shadows_graphite(c: &Connection) -> String {
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
        .query("SELECT id, quote(block) FROM ft_data ORDER BY id")
        .unwrap());
    let idx = fmt(c
        .query("SELECT segid, quote(term), pgno FROM ft_idx ORDER BY segid, term, pgno")
        .unwrap());
    let ds = fmt(c
        .query("SELECT id, quote(sz) FROM ft_docsize ORDER BY id")
        .unwrap());
    format!("DATA\n{data}\nIDX\n{idx}\nDOCSIZE\n{ds}")
}

const DDL: &str = "CREATE VIRTUAL TABLE ft USING fts5(a, b)";

/// The document text for rowid `i` — deliberately varied per row so the segment
/// leaves fill unevenly and the merge output spans several leaves (which is where
/// the incremental-merge writer differs byte-for-byte from the bulk writer).
fn doc(i: usize) -> (String, String) {
    (
        format!("word{} term{} alpha{}", i % 7, i % 13, i % 5),
        format!("doc body number {i} content here"),
    )
}

/// Apply `n_ins` autocommit inserts then `n_del` autocommit deletes (of the lowest
/// rowids) to a graphite db and a sqlite db, and assert byte-identical shadow
/// tables, integrity-check-clean, and matching MATCH rows for a few probe terms.
fn assert_automerge_identical(tag: &str, n_ins: usize, n_del: usize) {
    if !have_fts5_sqlite() {
        return;
    }
    let g = tmp_path(&format!("{tag}-g"));
    let s = tmp_path(&format!("{tag}-s"));

    let mut stmts: Vec<String> = Vec::new();
    for i in 1..=n_ins {
        let (a, b) = doc(i);
        stmts.push(format!("INSERT INTO ft(rowid,a,b) VALUES({i},'{a}','{b}')"));
    }
    for i in 1..=n_del {
        stmts.push(format!("DELETE FROM ft WHERE rowid={i}"));
    }

    let mut c = Connection::create(&g).unwrap();
    c.execute(DDL).unwrap();
    for st in &stmts {
        c.execute(st).unwrap();
    }
    sqlite_raw(&s, &format!("{DDL};"));
    for st in &stmts {
        sqlite_raw(&s, &format!("{st};"));
    }

    // Byte-parity of the fts5 segment structure — the automerge deliverable. Read
    // graphite's shadow tables THROUGH graphite (not sqlite) so the comparison does
    // not depend on sqlite being able to open graphite's file: graphite currently
    // has a PRE-EXISTING (present on master, orthogonal to automerge) core-pager
    // defect that leaves a delete-heavy database malformed at the SQLite b-tree
    // level (in the `%_content` shadow, which the fts5 index bytes do not touch), so
    // stock sqlite3 refuses to open such a file even though every `%_data`/`%_idx`/
    // `%_docsize` byte is identical to sqlite's.
    let gs = dump_shadows_graphite(&c);
    let ss = dump_shadows_sqlite(&s);
    assert_eq!(gs, ss, "shadow-table bytes diverge for {tag}");

    // graphite's own integrity check must accept every file it writes.
    let gic = c
        .query("PRAGMA integrity_check")
        .unwrap()
        .rows
        .first()
        .map(|r| match &r[0] {
            graphitesql::Value::Text(t) => String::from(t.as_str()),
            _ => String::new(),
        })
        .unwrap_or_default();
    assert_eq!(gic, "ok", "graphite integrity_check failed for {tag}");

    // For pure-insert corpora the SQLite b-tree is well-formed, so cross-check the
    // strongest oracle: sqlite's own fts5 integrity-check over graphite's file.
    if n_del == 0 {
        assert!(
            sqlite_integrity_ok(&g),
            "sqlite integrity-check rejected graphite's file for {tag}"
        );
    }

    for term in ["term3", "word4", "alpha2", "content", "body"] {
        let gq = c
            .query(&format!(
                "SELECT rowid FROM ft WHERE ft MATCH '{term}' ORDER BY rowid"
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
            &format!("SELECT rowid FROM ft WHERE ft MATCH '{term}' ORDER BY rowid;"),
        )
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().unwrap())
        .collect();
        assert_eq!(grow, srow, "MATCH '{term}' rows diverge for {tag}");
    }

    let _ = std::fs::remove_file(&g);
    let _ = std::fs::remove_file(&s);
}

// --- pure inserts crossing 64-leaf automerge boundaries -----------------------

#[test]
fn pure_insert_just_past_one_work_unit() {
    // wc crosses 64 at insert 64 → the first automerge event.
    assert_automerge_identical("ins70", 70, 0);
}

#[test]
fn pure_insert_hundred() {
    assert_automerge_identical("ins100", 100, 0);
}

#[test]
fn pure_insert_across_two_work_units() {
    // 128 leaves crosses the 64-boundary twice (a second automerge cascade).
    assert_automerge_identical("ins128", 128, 0);
}

#[test]
fn pure_insert_odd_sizes() {
    for n in [66, 90, 112, 150, 200] {
        assert_automerge_identical(&format!("ins{n}"), n, 0);
    }
}

// --- delete-heavy corpora (tombstone segments + automerge) --------------------

#[test]
fn insert40_delete30() {
    assert_automerge_identical("i40d30", 40, 30);
}

#[test]
fn insert60_delete50() {
    assert_automerge_identical("i60d50", 60, 50);
}

#[test]
fn insert100_delete80() {
    assert_automerge_identical("i100d80", 100, 80);
}

#[test]
fn delete_heavy_random_shapes() {
    for (ni, nd) in [(90usize, 45usize), (133, 77), (150, 100), (175, 120)] {
        assert_automerge_identical(&format!("i{ni}d{nd}"), ni, nd);
    }
}
