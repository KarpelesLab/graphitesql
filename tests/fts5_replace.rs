//! Regression: `INSERT OR REPLACE` / `REPLACE` onto an EXISTING fts5 rowid must
//! tombstone the OLD document's terms before writing the new document's postings.
//!
//! An `INSERT OR REPLACE` that lands on an existing rowid is a delete-of-old +
//! insert-of-new — exactly like an `UPDATE`. The replace-conflict branch used to
//! overwrite `%_content` but skip the inverted-index maintenance, leaving stale
//! postings: stock `sqlite3` then rejected the file with "malformed inverted index
//! for FTS5 table …", while graphite's own `integrity_check` wrongly said `ok`.
//!
//! This drives the SAME autocommit/transaction REPLACE sequences through graphite
//! and stock `sqlite3` (3.50.4, FTS5) and asserts: (a) sqlite's `quick_check` /
//! FTS5 `integrity-check` accept graphite's file, (b) MATCH returns the same rows
//! (old terms gone, new terms present), and — for the shapes graphite reproduces
//! byte-for-byte — (c) the raw `%_data`/`%_idx`/`%_docsize` bytes are identical to
//! sqlite's own `INSERT OR REPLACE` result. Skipped when `sqlite3` with FTS5 is
//! not on PATH.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-repl-{}-{}-{}.db",
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

/// sqlite's `PRAGMA quick_check` must report exactly `ok` (a malformed inverted
/// index shows up here as "malformed inverted index for FTS5 table main.ft").
fn sqlite_quick_check_ok(path: &str) -> bool {
    sqlite_raw(path, "PRAGMA quick_check;") == "ok"
}

/// sqlite's FTS5 `integrity-check` command must accept the file (no error).
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

fn graphite_match(c: &Connection, term: &str) -> Vec<i64> {
    c.query(&format!(
        "SELECT rowid FROM ft WHERE ft MATCH '{term}' ORDER BY rowid"
    ))
    .unwrap()
    .rows
    .iter()
    .map(|r| match &r[0] {
        graphitesql::Value::Integer(i) => *i,
        _ => -1,
    })
    .collect()
}

fn sqlite_match(path: &str, term: &str) -> Vec<i64> {
    sqlite_raw(
        path,
        &format!("SELECT rowid FROM ft WHERE ft MATCH '{term}' ORDER BY rowid;"),
    )
    .lines()
    .filter(|l| !l.is_empty())
    .map(|l| l.parse().unwrap())
    .collect()
}

/// Run the sequence through both engines. Assert sqlite `quick_check`/FTS5
/// `integrity-check` accept graphite's file, graphite's own `integrity_check` is
/// `ok`, MATCH rows agree for every probe term, and (when `byte_exact`) the raw
/// shadow-table bytes are identical to sqlite's.
fn assert_replace_valid(
    tag: &str,
    ddl: &str,
    stmts: &[&str],
    probe_terms: &[&str],
    byte_exact: bool,
) {
    let g = tmp_path(&format!("{tag}-g"));
    let s = tmp_path(&format!("{tag}-s"));

    let mut c = Connection::create(&g).unwrap();
    c.execute(ddl).unwrap();
    for st in stmts {
        c.execute(st).unwrap();
    }
    // Replay every statement in ONE sqlite3 process so an explicit BEGIN/COMMIT
    // spans them (each `sqlite_raw` call is otherwise its own transaction).
    let mut script = format!("{ddl};\n");
    for st in stmts {
        script.push_str(st);
        script.push_str(";\n");
    }
    sqlite_raw(&s, &script);

    // Graphite's own integrity_check must be clean.
    let gic = c.query("PRAGMA integrity_check").unwrap();
    let gic_ok = gic.rows.len() == 1
        && matches!(&gic.rows[0][0], graphitesql::Value::Text(t) if t.as_str() == "ok");
    assert!(gic_ok, "graphite integrity_check not ok for {tag}: {gic:?}");

    // Stock sqlite must accept the file graphite wrote.
    assert!(
        sqlite_quick_check_ok(&g),
        "sqlite quick_check rejected graphite's file for {tag}"
    );
    assert!(
        sqlite_integrity_ok(&g),
        "sqlite FTS5 integrity-check rejected graphite's file for {tag}"
    );

    // MATCH parity: old terms gone, new terms present.
    for term in probe_terms {
        assert_eq!(
            graphite_match(&c, term),
            sqlite_match(&s, term),
            "MATCH '{term}' rows diverge for {tag}"
        );
    }

    if byte_exact {
        assert_eq!(
            dump_shadows_graphite(&c),
            dump_shadows_sqlite(&s),
            "shadow-table bytes diverge for {tag}"
        );
    }
}

const DDL1: &str = "CREATE VIRTUAL TABLE ft USING fts5(x)";

#[test]
fn insert_or_replace_conflict_tombstones_old_terms() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_replace_valid(
        "ior-one",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'hello')",
            "INSERT OR REPLACE INTO ft(rowid,x) VALUES(1,'world')",
        ],
        &["hello", "world"],
        true,
    );
}

#[test]
fn replace_into_conflict_tombstones_old_terms() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_replace_valid(
        "repl-one",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'hello')",
            "REPLACE INTO ft(rowid,x) VALUES(1,'world')",
        ],
        &["hello", "world"],
        true,
    );
}

#[test]
fn repeated_replaces_same_rowid() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_replace_valid(
        "ior-repeat",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'one')",
            "INSERT OR REPLACE INTO ft(rowid,x) VALUES(1,'two')",
            "INSERT OR REPLACE INTO ft(rowid,x) VALUES(1,'three')",
            "INSERT OR REPLACE INTO ft(rowid,x) VALUES(1,'four')",
        ],
        &["one", "two", "three", "four"],
        true,
    );
}

#[test]
fn replace_in_transaction() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_replace_valid(
        "ior-txn",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'hello')",
            "BEGIN",
            "INSERT OR REPLACE INTO ft(rowid,x) VALUES(1,'world')",
            "COMMIT",
        ],
        &["hello", "world"],
        true,
    );
}

#[test]
fn replace_mixed_with_inserts_and_deletes() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_replace_valid(
        "ior-mixed",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'aaa'),(2,'bbb'),(3,'ccc')",
            "INSERT OR REPLACE INTO ft(rowid,x) VALUES(2,'zzz')",
            "INSERT INTO ft(rowid,x) VALUES(4,'ddd')",
            "DELETE FROM ft WHERE rowid=3",
        ],
        &["aaa", "bbb", "ccc", "ddd", "zzz"],
        true,
    );
}

#[test]
fn multi_row_mixed_new_and_replace_one_statement() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_replace_valid(
        "ior-multirow",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'alpha'),(2,'beta')",
            "INSERT OR REPLACE INTO ft(rowid,x) VALUES(1,'gamma'),(3,'delta')",
        ],
        &["alpha", "beta", "gamma", "delta"],
        true,
    );
}

#[test]
fn multi_column_replace() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_replace_valid(
        "ior-multicol",
        "CREATE VIRTUAL TABLE ft USING fts5(x, y)",
        &[
            "INSERT INTO ft(rowid,x,y) VALUES(1,'foo','bar')",
            "INSERT OR REPLACE INTO ft(rowid,x,y) VALUES(1,'baz','qux')",
        ],
        &["foo", "bar", "baz", "qux", "y:qux"],
        true,
    );
}

#[test]
fn replace_to_null_document() {
    if !have_fts5_sqlite() {
        return;
    }
    assert_replace_valid(
        "ior-null",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'hello')",
            "INSERT OR REPLACE INTO ft(rowid,x) VALUES(1,NULL)",
        ],
        &["hello"],
        true,
    );
}

#[test]
fn non_conflicting_or_replace_is_a_plain_insert() {
    if !have_fts5_sqlite() {
        return;
    }
    // A new rowid via OR REPLACE must stay byte-identical to a plain insert append.
    assert_replace_valid(
        "ior-nonconflict",
        DDL1,
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'hello')",
            "INSERT OR REPLACE INTO ft(rowid,x) VALUES(2,'world')",
        ],
        &["hello", "world"],
        true,
    );
}

#[test]
fn prefix_index_replace_valid_not_byte_checked() {
    if !have_fts5_sqlite() {
        return;
    }
    // Prefix tables take the consolidated rebuild path (a documented residual), so
    // the bytes may consolidate differently — but the file must still be valid and
    // MATCH must agree.
    assert_replace_valid(
        "ior-prefix",
        "CREATE VIRTUAL TABLE ft USING fts5(x, prefix='2 3')",
        &[
            "INSERT INTO ft(rowid,x) VALUES(1,'hello world')",
            "INSERT OR REPLACE INTO ft(rowid,x) VALUES(1,'goodbye planet')",
        ],
        &["hello", "world", "goodbye", "planet", "pla*", "wor*"],
        false,
    );
}
