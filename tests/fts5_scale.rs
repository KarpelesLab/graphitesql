//! Roadmap D2e-encoder: graphite-written FTS5 stays byte/integrity-correct AT
//! SCALE, specifically once a single term's doclist spills across enough leaf
//! pages to require a DOCLIST-INDEX (`dlidx`) b-tree — the piece the common-case
//! writer used to omit (which made sqlite's `integrity-check` reject the file).
//!
//! Each test builds a corpus with graphite, then cross-checks against stock
//! `sqlite3` (3.50.4, FTS5): sqlite must read graphite's file, pass
//! `INSERT INTO ft(ft) VALUES('integrity-check')`, and return the same `MATCH`
//! rows; where the layout is deterministic, the raw `%_data`/`%_idx` bytes are
//! compared directly. Skipped when `sqlite3` (with FTS5) is not on PATH.
//!
//! Sizes are kept modest (graphite's per-row content write is superlinear), but
//! the rowids are spaced far apart so a term's doclist reaches five leaves — and
//! thus a doclist-index — with only a few thousand documents.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::{Connection, Value};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-scale-{}-{}-{}.db",
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

/// Sorted, `\n`-joined output of `q` on `path` via sqlite3.
fn sqlite_sorted(path: &str, q: &str) -> String {
    let mut v: Vec<String> = sqlite_raw(path, q).lines().map(str::to_string).collect();
    v.sort();
    v.join("\n")
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

/// `VALUES` list of `n` single-token ('shared') documents at widely-spaced
/// rowids, so the doclist's rowid deltas are multi-byte and it reaches five
/// leaf pages (→ a doclist-index) with only `n` documents.
fn spaced_values(n: usize) -> String {
    let mut s = String::new();
    for i in 1..=n {
        if i > 1 {
            s.push(',');
        }
        s.push_str(&format!("({},'shared')", (i as i64) * 100_000_000));
    }
    s
}

/// Build the same corpus with graphite (`g`) and with sqlite+optimize (`s`).
fn build_pair(g: &str, s: &str, values: &str) {
    {
        let mut c = Connection::create(g).unwrap();
        c.execute("CREATE VIRTUAL TABLE ft USING fts5(body)")
            .unwrap();
        // One statement → one rebuild (graphite bulk-rebuilds a single segment).
        c.execute("BEGIN").ok();
        c.execute(&format!("INSERT INTO ft(rowid,body) VALUES {values}"))
            .unwrap();
        c.execute("COMMIT").ok();
    }
    Command::new("sqlite3")
        .arg(s)
        .arg("CREATE VIRTUAL TABLE ft USING fts5(body);")
        .output()
        .unwrap();
    Command::new("sqlite3")
        .arg(s)
        .arg(format!("INSERT INTO ft(rowid,body) VALUES {values};"))
        .output()
        .unwrap();
    // Compact sqlite to a single segment so its layout matches graphite's
    // always-one-segment rebuild.
    Command::new("sqlite3")
        .arg(s)
        .arg("INSERT INTO ft(ft) VALUES('optimize');")
        .output()
        .unwrap();
}

/// A single term whose doclist spans five leaves gets a doclist-index. sqlite
/// must accept graphite's file (integrity-check) and, because the single-segment
/// layout is deterministic here, the raw `%_data` and `%_idx` bytes are IDENTICAL
/// to sqlite's own.
#[test]
fn dlidx_corpus_is_byte_identical_and_integrity_clean() {
    if !have_fts5_sqlite() {
        eprintln!("sqlite3 with FTS5 not found; skipping");
        return;
    }
    let g = tmp_path("dlidx-g");
    let s = tmp_path("dlidx-s");
    let vals = spaced_values(3000);
    build_pair(&g, &s, &vals);

    // graphite actually emitted a doclist-index page (dli bit 1<<36 set).
    let g_dlidx = sqlite_raw(&g, "SELECT count(*) FROM ft_data WHERE (id & (1<<36))<>0;");
    assert_eq!(g_dlidx, "1", "graphite must emit exactly one dlidx page");
    let g_leaves = sqlite_raw(
        &g,
        "SELECT count(*) FROM ft_data WHERE id>10 AND (id & (1<<36))=0;",
    );
    assert_eq!(g_leaves, "5", "expected a five-leaf single-term doclist");

    // sqlite's FTS5 integrity-check accepts graphite's file.
    assert!(
        sqlite_integrity_ok(&g),
        "sqlite integrity-check rejected graphite's dlidx file"
    );
    assert_eq!(sqlite_raw(&g, "PRAGMA integrity_check;"), "ok");

    // Byte-for-byte identical shadow tables.
    let data = "SELECT id||':'||quote(block) FROM ft_data ORDER BY id;";
    assert_eq!(
        sqlite_raw(&g, data),
        sqlite_raw(&s, data),
        "%_data bytes diverge from sqlite"
    );
    let idx = "SELECT segid||':'||quote(term)||':'||pgno FROM ft_idx ORDER BY segid, term;";
    assert_eq!(
        sqlite_raw(&g, idx),
        sqlite_raw(&s, idx),
        "%_idx bytes diverge from sqlite"
    );

    // And MATCH over graphite's file returns every rowid.
    assert_eq!(
        sqlite_raw(
            &g,
            "SELECT count(*), sum(rowid) FROM ft WHERE ft MATCH 'shared';"
        ),
        format!(
            "3000|{}",
            (1..=3000i64).map(|i| i * 100_000_000).sum::<i64>()
        )
    );

    let _ = std::fs::remove_file(&g);
    let _ = std::fs::remove_file(&s);
}

/// After deleting half the documents from a dlidx-sized table, graphite rebuilds
/// a fresh (still single) segment. sqlite must accept it and return exactly the
/// surviving rows for a MATCH.
#[test]
fn delete_half_stays_integrity_clean() {
    if !have_fts5_sqlite() {
        eprintln!("sqlite3 with FTS5 not found; skipping");
        return;
    }
    let g = tmp_path("del-g");
    {
        let mut c = Connection::create(&g).unwrap();
        c.execute("CREATE VIRTUAL TABLE ft USING fts5(body)")
            .unwrap();
        c.execute("BEGIN").ok();
        c.execute(&format!(
            "INSERT INTO ft(rowid,body) VALUES {}",
            spaced_values(3000)
        ))
        .unwrap();
        c.execute("COMMIT").ok();
        // Delete the even-indexed documents (rowids that are multiples of 2e8).
        c.execute("DELETE FROM ft WHERE (rowid/100000000) % 2 = 0")
            .unwrap();
    }
    assert!(
        sqlite_integrity_ok(&g),
        "sqlite integrity-check rejected graphite's post-DELETE file"
    );
    assert_eq!(sqlite_raw(&g, "PRAGMA integrity_check;"), "ok");
    // 1500 odd-indexed rows survive; all still MATCH 'shared'.
    assert_eq!(
        sqlite_raw(&g, "SELECT count(*) FROM ft WHERE ft MATCH 'shared';"),
        "1500"
    );
    // Spot-check: a deleted rowid is gone, a surviving one remains.
    assert_eq!(
        sqlite_raw(
            &g,
            "SELECT count(*) FROM ft WHERE ft MATCH 'shared' AND rowid=200000000;"
        ),
        "0"
    );
    assert_eq!(
        sqlite_raw(
            &g,
            "SELECT count(*) FROM ft WHERE ft MATCH 'shared' AND rowid=100000000;"
        ),
        "1"
    );
    let _ = std::fs::remove_file(&g);
}

/// `INSERT INTO ft(ft) VALUES('optimize')` / `('merge', N)` are accepted and
/// leave graphite's already-single-segment index unchanged and integrity-clean.
#[test]
fn optimize_and_merge_are_noops_on_dlidx_index() {
    if !have_fts5_sqlite() {
        eprintln!("sqlite3 with FTS5 not found; skipping");
        return;
    }
    let g = tmp_path("opt-g");
    let before;
    {
        let mut c = Connection::create(&g).unwrap();
        c.execute("CREATE VIRTUAL TABLE ft USING fts5(body)")
            .unwrap();
        c.execute("BEGIN").ok();
        c.execute(&format!(
            "INSERT INTO ft(rowid,body) VALUES {}",
            spaced_values(3000)
        ))
        .unwrap();
        c.execute("COMMIT").ok();
        before = sqlite_raw(&g, "SELECT id||':'||quote(block) FROM ft_data ORDER BY id;");
        // These maintenance commands are no-ops for a single compacted segment.
        c.execute("INSERT INTO ft(ft) VALUES('optimize')").unwrap();
        c.execute("INSERT INTO ft(ft, rank) VALUES('merge', 4)")
            .unwrap();
    }
    let after = sqlite_raw(&g, "SELECT id||':'||quote(block) FROM ft_data ORDER BY id;");
    assert_eq!(before, after, "optimize/merge changed the single segment");
    assert!(sqlite_integrity_ok(&g));
    let _ = std::fs::remove_file(&g);
}

/// Regression on the READ side: a dlidx-sized table written by sqlite is read
/// back correctly by graphite (a term whose doclist has a doclist-index).
#[test]
fn graphite_reads_sqlite_dlidx_file() {
    if !have_fts5_sqlite() {
        eprintln!("sqlite3 with FTS5 not found; skipping");
        return;
    }
    let s = tmp_path("read-s");
    Command::new("sqlite3")
        .arg(&s)
        .arg("CREATE VIRTUAL TABLE ft USING fts5(body);")
        .output()
        .unwrap();
    Command::new("sqlite3")
        .arg(&s)
        .arg(format!(
            "INSERT INTO ft(rowid,body) VALUES {};",
            spaced_values(3000)
        ))
        .output()
        .unwrap();
    Command::new("sqlite3")
        .arg(&s)
        .arg("INSERT INTO ft(ft) VALUES('optimize');")
        .output()
        .unwrap();
    // sqlite really wrote a doclist-index.
    assert_eq!(
        sqlite_raw(&s, "SELECT count(*) FROM ft_data WHERE (id & (1<<36))<>0;"),
        "1"
    );

    // graphite opens sqlite's file and answers MATCH from its index.
    let c = Connection::open(&s).unwrap();
    let rows = c
        .query("SELECT count(*) FROM ft WHERE ft MATCH 'shared'")
        .unwrap();
    let count = match rows.rows.first().and_then(|r| r.first()) {
        Some(Value::Integer(i)) => *i,
        other => panic!("expected an integer count, got {other:?}"),
    };
    assert_eq!(count, 3000, "graphite miscounted sqlite's dlidx doclist");

    // Cross-check the SAME query in sqlite.
    assert_eq!(
        sqlite_sorted(&s, "SELECT count(*) FROM ft WHERE ft MATCH 'shared'"),
        "3000"
    );
    let _ = std::fs::remove_file(&s);
}
