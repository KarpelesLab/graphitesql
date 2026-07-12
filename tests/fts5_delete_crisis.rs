//! Roadmap D2e — DELETE-triggered CRISIS MERGE via sqlite's tombstone-reconciling
//! merge. When enough autocommit deletes/inserts push a level to 16 segments,
//! sqlite's `fts5IndexCrisismerge` merges that level into ONE segment at the next
//! level. When the merge collapses the WHOLE index into the single oldest level
//! (`bOldest`), `fts5IndexMergeLevel`'s key-annihilation
//! (`if( pSegIter->nPos==0 && (bOldest || pSegIter->bDel==0) ) continue;`) drops
//! every DELETE marker together with the older posting it shadows — so the merged
//! segment is exactly a clean rebuild over the LIVE (post-delete) corpus, with no
//! tombstone entries and no deleted rowids.
//!
//! graphite services this incrementally through the same crisis-merge path as the
//! pure-insert case (rebuilding the merged segment from the live corpus), instead
//! of falling back to a bulk single-segment rebuild. This test drives the SAME
//! autocommit insert/delete sequences that cross the 16-segment threshold through
//! graphite and stock `sqlite3` (3.50.4, FTS5) and asserts the raw
//! `%_data` / `%_idx` / `%_docsize` bytes — including the STRUCTURE record — are
//! byte-identical, sqlite's `integrity-check` accepts graphite's file, and a MATCH
//! returns the same rows (deleted rows gone). Skipped when `sqlite3` with FTS5 is
//! not on PATH.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-delcrisis-{}-{}-{}.db",
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

/// Run `q` through stock sqlite3 on `path`; assert success; return raw stdout.
fn sqlite_raw(path: &str, q: &str) -> String {
    let o = Command::new("sqlite3").arg(path).arg(q).output().unwrap();
    assert!(
        o.status.success(),
        "sqlite3 failed for {q:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

/// sqlite's FTS5 `integrity-check` must accept the file (no error).
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

/// Apply the SAME sequence of statements to a graphite db and a sqlite db, then
/// assert byte-identical shadow tables, integrity-check-clean, and matching MATCH
/// rowid sets for a set of probe terms.
fn assert_seq_identical(tag: &str, ddl: &str, stmts: &[String], probe_terms: &[&str]) {
    let g = tmp_path(&format!("{tag}-g"));
    let s = tmp_path(&format!("{tag}-s"));

    let mut c = Connection::create(&g).unwrap();
    c.execute(ddl).unwrap();
    for st in stmts {
        c.execute(st).unwrap();
    }
    sqlite_raw(&s, &format!("{ddl};"));
    for st in stmts {
        sqlite_raw(&s, &format!("{st};"));
    }

    let gs = dump_shadows_graphite(&c);
    let ss = dump_shadows_sqlite(&s);
    assert_eq!(gs, ss, "shadow-table bytes diverge for {tag}");

    assert!(
        sqlite_integrity_ok(&g),
        "sqlite integrity-check rejected graphite's file for {tag}"
    );

    for term in probe_terms {
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
        let srow_raw = sqlite_raw(
            &s,
            &format!("SELECT rowid FROM ft WHERE ft MATCH '{term}' ORDER BY rowid;"),
        );
        let srow: Vec<i64> = srow_raw
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.parse().unwrap())
            .collect();
        assert_eq!(grow, srow, "MATCH '{term}' rows diverge for {tag}");
    }

    let _ = std::fs::remove_file(&g);
    let _ = std::fs::remove_file(&s);
}

const DDL1: &str = "CREATE VIRTUAL TABLE ft USING fts5(x)";

/// 20 autocommit inserts (crisis-merge at 16 → level 1), then 18 autocommit
/// deletes. The 16th insert triggers an insert-crisis (all-live merge); the
/// deletes append tombstone segments until a later delete pushes level 0 back to
/// 16 segments and triggers a DELETE-crisis merge that must annihilate tombstones.
#[test]
fn insert20_then_delete18_crosses_crisis() {
    if !have_fts5_sqlite() {
        return;
    }
    let mut stmts: Vec<String> = Vec::new();
    for i in 1..=20 {
        stmts.push(format!(
            "INSERT INTO ft(rowid,x) VALUES({i},'word{i} common alpha')"
        ));
    }
    for i in 1..=18 {
        stmts.push(format!("DELETE FROM ft WHERE rowid={i}"));
    }
    assert_seq_identical(
        "ins20-del18",
        DDL1,
        &stmts,
        &["common", "alpha", "word19", "word20", "word1", "word10"],
    );
}

/// Interleaved insert/delete reaching 16 level-0 segments with tombstones in the
/// merged level.
#[test]
fn interleaved_insert_delete_reaches_crisis() {
    if !have_fts5_sqlite() {
        return;
    }
    let mut stmts: Vec<String> = Vec::new();
    // Seed 8 rows.
    for i in 1..=8 {
        stmts.push(format!(
            "INSERT INTO ft(rowid,x) VALUES({i},'seed{i} shared')"
        ));
    }
    // Now alternate insert new / delete old to churn level-0 segments across 16.
    for next in 9..25 {
        stmts.push(format!(
            "INSERT INTO ft(rowid,x) VALUES({next},'new{next} shared')"
        ));
        stmts.push(format!("DELETE FROM ft WHERE rowid={}", next - 8));
    }
    assert_seq_identical(
        "interleaved",
        DDL1,
        &stmts,
        &["shared", "seed1", "new24", "new9"],
    );
}

/// A three-column table with per-column terms; delete-crisis must annihilate
/// tombstones across all columns.
#[test]
fn multicol_delete_crisis() {
    if !have_fts5_sqlite() {
        return;
    }
    let ddl = "CREATE VIRTUAL TABLE ft USING fts5(a, b, c)";
    let mut stmts: Vec<String> = Vec::new();
    for i in 1..=20 {
        stmts.push(format!(
            "INSERT INTO ft(rowid,a,b,c) VALUES({i},'a{i} tag','b{i} tag','c{i} common')"
        ));
    }
    for i in 1..=17 {
        stmts.push(format!("DELETE FROM ft WHERE rowid={i}"));
    }
    assert_seq_identical(
        "multicol",
        ddl,
        &stmts,
        &["tag", "common", "a18", "b19", "c20"],
    );
}

/// A delete-crisis that deletes the LAST live row — the merged live corpus is
/// EMPTY. sqlite's `fts5IndexMergeLevel` writes the merged segment, sees
/// `pgnoLast==0`, and REMOVES it, leaving a zero-segment structure (with an
/// `nRow=0` averages record). graphite must produce the same empty structure, not
/// a phantom leafless segment. (Regression pin: this exact interleaving corrupted
/// the file before the empty-merge fix.)
#[test]
fn delete_crisis_to_empty() {
    if !have_fts5_sqlite() {
        return;
    }
    // Insert then delete in a churn so that the 16th appended segment is a
    // DELETE that removes the final remaining live row.
    let mut stmts: Vec<String> = Vec::new();
    let mut live: Vec<i64> = Vec::new();
    let mut nextid = 1i64;
    let mut segcount = 0; // appended level-0 segments so far this level
    // Build up to exactly the crisis boundary, ending on a delete-to-empty.
    while segcount < 15 {
        if live.len() >= 2 && segcount % 2 == 1 {
            let rid = live.remove(0);
            stmts.push(format!("DELETE FROM ft WHERE rowid={rid}"));
        } else {
            let rid = nextid;
            nextid += 1;
            stmts.push(format!(
                "INSERT INTO ft(rowid,x) VALUES({rid},'w{rid} shared')"
            ));
            live.push(rid);
        }
        segcount += 1;
    }
    // Delete every remaining live row so the 16th (crisis-triggering) segment
    // leaves the live corpus empty.
    while !live.is_empty() {
        let rid = live.remove(0);
        stmts.push(format!("DELETE FROM ft WHERE rowid={rid}"));
    }
    assert_seq_identical("del-to-empty", DDL1, &stmts, &["shared", "w1", "w9"]);
}
