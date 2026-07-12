//! Roadmap D2e — INCREMENTAL MULTI-SEGMENT writes: graphite appends a new
//! level-0 segment per autocommit write transaction (and crisis-merges at 16
//! segments), byte-identical to sqlite's `fts5FlushOneHash`/`fts5IndexCrisismerge`
//! — instead of rebuilding one compacted segment on every write.
//!
//! sqlite flushes the in-memory hash to a NEW segment (fresh segid, level 0) at
//! each transaction commit; once a level reaches 16 segments it crisis-merges them
//! into one larger segment at the next level. So after N separate INSERT
//! transactions sqlite holds N level-0 segments (until 16 triggers a merge to one
//! level-1 segment). This test drives the SAME per-statement (autocommit) insert
//! sequences through graphite and stock `sqlite3` (3.50.4, FTS5) and asserts the
//! raw `%_data` / `%_idx` / `%_docsize` bytes — including the STRUCTURE record —
//! are identical, sqlite's `integrity-check` accepts graphite's file, and a MATCH
//! returns the same rows. Skipped when `sqlite3` with FTS5 is not on PATH.
//!
//! The single-transaction bulk case (one segment on both sides) is pinned by
//! `tests/fts5_multiterm.rs`; here every insert is its OWN autocommit transaction.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-ms-{}-{}-{}.db",
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

/// Dump the three shadow tables as `id|quote(block)` lines (byte-comparable).
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

/// Apply the SAME sequence of one-INSERT-per-transaction (autocommit) writes to a
/// graphite db and a sqlite db, then assert byte-identical shadow tables,
/// integrity-check-clean, and matching MATCH results for every doc's first token.
fn assert_seq_identical(tag: &str, docs: &[&str]) {
    let g = tmp_path(&format!("{tag}-g"));
    let s = tmp_path(&format!("{tag}-s"));

    let mut c = Connection::create(&g).unwrap();
    c.execute("CREATE VIRTUAL TABLE ft USING fts5(x)").unwrap();
    for d in docs {
        // Each execute autocommits → its own transaction → one appended segment.
        c.execute(&format!("INSERT INTO ft(x) VALUES('{d}')"))
            .unwrap();
    }
    // sqlite: one statement per invocation → one transaction each.
    sqlite_raw(&s, "CREATE VIRTUAL TABLE ft USING fts5(x);");
    for d in docs {
        sqlite_raw(&s, &format!("INSERT INTO ft(x) VALUES('{d}');"));
    }

    let gs = dump_shadows_graphite(&c);
    let ss = dump_shadows_sqlite(&s);
    assert_eq!(gs, ss, "shadow-table bytes diverge for {tag}");

    assert!(
        sqlite_integrity_ok(&g),
        "sqlite integrity-check rejected graphite's file for {tag}"
    );

    // MATCH parity: query each distinct first token and compare rowid sets.
    for d in docs {
        let term = d.split_whitespace().next().unwrap_or("");
        if term.is_empty() {
            continue;
        }
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
}

#[test]
fn multiseg_two_inserts_append_second_segment() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical("two", &["alpha beta", "gamma delta"]);
}

#[test]
fn multiseg_five_inserts_five_level0_segments() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical(
        "five",
        &[
            "alpha beta",
            "gamma delta",
            "epsilon zeta",
            "eta theta",
            "iota kappa",
        ],
    );
}

#[test]
fn multiseg_below_crisis_fifteen_segments() {
    if !have_fts5_sqlite() {
        return;
    }
    let docs: Vec<String> = (1..=15).map(|i| format!("word{i} tok{i}")).collect();
    let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
    assert_seq_identical("fifteen", &refs);
}

#[test]
fn multiseg_crisis_merge_at_sixteen() {
    if !have_fts5_sqlite() {
        return;
    }
    let docs: Vec<String> = (1..=16).map(|i| format!("word{i} tok{i}")).collect();
    let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
    assert_seq_identical("sixteen", &refs);
}

#[test]
fn multiseg_crisis_then_more_appends() {
    if !have_fts5_sqlite() {
        return;
    }
    // 16 → crisis-merge to one level-1 seg, then 4 more level-0 appends
    // (with the merged segment promoted back down alongside them).
    let docs: Vec<String> = (1..=20).map(|i| format!("word{i} tok{i}")).collect();
    let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
    assert_seq_identical("twenty", &refs);
}

#[test]
fn multiseg_second_crisis_and_beyond() {
    if !have_fts5_sqlite() {
        return;
    }
    // 33 inserts: crisis at 16 (→ level-1 seg), promote-down cascade, a SECOND
    // crisis around 31, then more appends. Any state graphite cannot match
    // byte-for-byte must fall back to the bulk rebuild (still integrity-clean and
    // MATCH-correct), which this asserts alongside the byte compare.
    let docs: Vec<String> = (1..=33).map(|i| format!("word{i} tok{i}")).collect();
    let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
    assert_seq_identical("thirtythree", &refs);
}

#[test]
fn multiseg_multicolumn_appends() {
    if !have_fts5_sqlite() {
        return;
    }
    let g = tmp_path("mc-g");
    let s = tmp_path("mc-s");
    let mut c = Connection::create(&g).unwrap();
    c.execute("CREATE VIRTUAL TABLE ft USING fts5(a, b)")
        .unwrap();
    sqlite_raw(&s, "CREATE VIRTUAL TABLE ft USING fts5(a, b);");
    let rows = [
        ("alpha one", "beta two"),
        ("gamma three", "delta four"),
        ("epsilon five", "zeta six"),
    ];
    for (a, b) in rows {
        c.execute(&format!("INSERT INTO ft(a, b) VALUES('{a}', '{b}')"))
            .unwrap();
        sqlite_raw(&s, &format!("INSERT INTO ft(a, b) VALUES('{a}', '{b}');"));
    }
    assert_eq!(
        dump_shadows_graphite(&c),
        dump_shadows_sqlite(&s),
        "multi-column shadow bytes diverge"
    );
    assert!(sqlite_integrity_ok(&g), "integrity-check rejected multicol");
}

/// A single autocommit INSERT still writes exactly ONE segment (segid 1) —
/// identical to the bulk case; the incremental path must not regress it.
#[test]
fn multiseg_single_insert_is_one_segment() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical("one", &["alpha beta gamma"]);
}
