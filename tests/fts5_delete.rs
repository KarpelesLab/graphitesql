//! Roadmap D2e — INCREMENTAL DELETE (and DELETE-then-INSERT of UPDATE) via
//! sqlite's TOMBSTONE mechanism: instead of rebuilding one compacted segment on a
//! DELETE/UPDATE, graphite appends ONE new level-0 segment whose entries are the
//! deleted document's terms written as DELETE markers (a poslist encoded
//! `size2 = 1` — content length 0 with the low delete-flag bit set), plus (for an
//! UPDATE) the new document's insert postings merged into the same term stream.
//! This is a byte-for-byte port of sqlite's `fts5FlushOneHash` delete path.
//!
//! This test drives the SAME autocommit DELETE/UPDATE sequences through graphite
//! and stock `sqlite3` (3.50.4, FTS5) and asserts the raw `%_data` / `%_idx` /
//! `%_docsize` bytes — including the STRUCTURE record — are identical, sqlite's
//! `integrity-check` accepts graphite's file, and a MATCH returns the same rows
//! (deleted rows gone). Skipped when `sqlite3` with FTS5 is not on PATH.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-del-{}-{}-{}.db",
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
fn assert_seq_identical(tag: &str, ddl: &str, stmts: &[&str], probe_terms: &[&str]) {
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
}

const DDL1: &str = "CREATE VIRTUAL TABLE ft USING fts5(x)";

#[test]
fn delete_one_row_appends_tombstone_segment() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical(
        "del-one",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'alpha beta')",
            "INSERT INTO ft(rowid,x) VALUES(2,'beta gamma')",
            "INSERT INTO ft(rowid,x) VALUES(3,'gamma delta')",
            "DELETE FROM ft WHERE rowid=2",
        ],
        &["alpha", "beta", "gamma", "delta"],
    );
}

#[test]
fn delete_many_rows_one_statement() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical(
        "del-many",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'alpha beta')",
            "INSERT INTO ft(rowid,x) VALUES(2,'beta gamma')",
            "INSERT INTO ft(rowid,x) VALUES(3,'gamma delta')",
            "INSERT INTO ft(rowid,x) VALUES(4,'delta epsilon')",
            "DELETE FROM ft WHERE rowid IN (2,4)",
        ],
        &["alpha", "beta", "gamma", "delta", "epsilon"],
    );
}

#[test]
fn delete_then_insert_new_rowid() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical(
        "del-then-ins",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'alpha beta')",
            "INSERT INTO ft(rowid,x) VALUES(2,'beta gamma')",
            "DELETE FROM ft WHERE rowid=1",
            "INSERT INTO ft(rowid,x) VALUES(3,'zeta eta')",
        ],
        &["alpha", "beta", "gamma", "zeta", "eta"],
    );
}

#[test]
fn update_one_row_delete_then_insert() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical(
        "upd-one",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'alpha beta')",
            "INSERT INTO ft(rowid,x) VALUES(2,'beta gamma')",
            "INSERT INTO ft(rowid,x) VALUES(3,'gamma delta')",
            "UPDATE ft SET x='zeta eta' WHERE rowid=2",
        ],
        &["alpha", "beta", "gamma", "delta", "zeta", "eta"],
    );
}

#[test]
fn update_row_sharing_a_term() {
    // The new text keeps a term the old text had ('beta'): the shared term must be
    // an INSERT posting (live positions), not a tombstone.
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical(
        "upd-shared",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'alpha beta gamma')",
            "UPDATE ft SET x='beta delta' WHERE rowid=1",
        ],
        &["alpha", "beta", "gamma", "delta"],
    );
}

#[test]
fn delete_multicolumn() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical(
        "del-multicol",
        "CREATE VIRTUAL TABLE ft USING fts5(a, b)",
        &[
            "INSERT INTO ft(rowid,a,b) VALUES(1,'alpha beta','one two')",
            "INSERT INTO ft(rowid,a,b) VALUES(2,'gamma delta','three four')",
            "DELETE FROM ft WHERE rowid=1",
        ],
        &["alpha", "two", "gamma", "four"],
    );
}

#[test]
fn insert_delete_insert_sequence() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical(
        "ins-del-ins",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'alpha beta')",
            "INSERT INTO ft(rowid,x) VALUES(2,'gamma delta')",
            "DELETE FROM ft WHERE rowid=1",
            "INSERT INTO ft(rowid,x) VALUES(3,'epsilon zeta')",
            "DELETE FROM ft WHERE rowid=2",
        ],
        &["alpha", "gamma", "epsilon", "zeta", "delta"],
    );
}

#[test]
fn graphite_reads_sqlite_written_index_with_deletes() {
    // Confirm graphite READS a sqlite-written index that contains a tombstone
    // segment (a MATCH over the deleted term returns no rows, others intact).
    if !have_fts5_sqlite() {
        return;
    }
    let s = tmp_path("read-sqlite");
    sqlite_raw(&s, "CREATE VIRTUAL TABLE ft USING fts5(x);");
    sqlite_raw(&s, "INSERT INTO ft(rowid,x) VALUES(1,'alpha beta');");
    sqlite_raw(&s, "INSERT INTO ft(rowid,x) VALUES(2,'beta gamma');");
    sqlite_raw(&s, "INSERT INTO ft(rowid,x) VALUES(3,'gamma delta');");
    sqlite_raw(&s, "DELETE FROM ft WHERE rowid=2;");

    let c = Connection::open(&s).unwrap();
    let rows = |q: &str| -> Vec<i64> {
        c.query(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| match &r[0] {
                graphitesql::Value::Integer(i) => *i,
                _ => -1,
            })
            .collect()
    };
    // 'beta' was in rows 1 and 2; row 2 is tombstoned → only row 1 remains.
    assert_eq!(
        rows("SELECT rowid FROM ft WHERE ft MATCH 'beta' ORDER BY rowid"),
        vec![1]
    );
    assert_eq!(
        rows("SELECT rowid FROM ft WHERE ft MATCH 'gamma' ORDER BY rowid"),
        vec![3]
    );
    assert_eq!(
        rows("SELECT rowid FROM ft WHERE ft MATCH 'alpha' ORDER BY rowid"),
        vec![1]
    );
}

#[test]
fn delete_all_rows() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical(
        "del-all",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'alpha beta')",
            "INSERT INTO ft(rowid,x) VALUES(2,'beta gamma')",
            "DELETE FROM ft",
        ],
        &["alpha", "beta", "gamma"],
    );
}

#[test]
fn deletes_across_several_segments_byte_identical() {
    // Multiple separate INSERT transactions leave several level-0 segments; a
    // DELETE then appends one more tombstone segment. Stays under the 16-segment
    // crisis threshold, so every step is byte-identical.
    if !have_fts5_sqlite() {
        return;
    }
    assert_seq_identical(
        "del-multi-seg",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'alpha one')",
            "INSERT INTO ft(rowid,x) VALUES(2,'beta two')",
            "INSERT INTO ft(rowid,x) VALUES(3,'gamma three')",
            "INSERT INTO ft(rowid,x) VALUES(4,'delta four')",
            "DELETE FROM ft WHERE rowid=2",
            "DELETE FROM ft WHERE rowid=3",
        ],
        &["alpha", "beta", "gamma", "delta", "one", "four"],
    );
}

#[test]
fn many_deletes_fall_back_but_stay_correct() {
    // Enough separate delete transactions to cross sqlite's 16-segment crisis
    // threshold. graphite falls back to the (correct) bulk rebuild there rather
    // than porting tombstone-reconciling crisis merges, so the bytes need not
    // match sqlite — but the index must stay query-correct and sqlite's
    // integrity-check must accept it.
    if !have_fts5_sqlite() {
        return;
    }
    let g = tmp_path("many-del-g");
    let mut c = Connection::create(&g).unwrap();
    c.execute(DDL1).unwrap();
    for i in 1..=20 {
        c.execute(&format!(
            "INSERT INTO ft(rowid,x) VALUES({i},'tok{i} common')"
        ))
        .unwrap();
    }
    for i in 1..=18 {
        c.execute(&format!("DELETE FROM ft WHERE rowid={i}"))
            .unwrap();
    }
    // Only rows 19 and 20 survive.
    let rows = |q: &str| -> Vec<i64> {
        c.query(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| match &r[0] {
                graphitesql::Value::Integer(i) => *i,
                _ => -1,
            })
            .collect()
    };
    assert_eq!(
        rows("SELECT rowid FROM ft WHERE ft MATCH 'common' ORDER BY rowid"),
        vec![19, 20]
    );
    assert!(rows("SELECT rowid FROM ft WHERE ft MATCH 'tok5'").is_empty());
    assert_eq!(
        rows("SELECT rowid FROM ft WHERE ft MATCH 'tok19'"),
        vec![19]
    );
    assert!(
        sqlite_integrity_ok(&g),
        "sqlite integrity-check rejected graphite's fallback file"
    );
}
