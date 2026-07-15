//! Roadmap D2e-2 — INCREMENTAL writes inside an EXPLICIT transaction: an fts5
//! table written inside `BEGIN … COMMIT` (or `SAVEPOINT … RELEASE`) flushes the
//! whole transaction's postings as ONE level-0 segment at commit — matching
//! sqlite, which accumulates them in an in-memory hash and writes them at
//! `xSync`/`xCommit` — instead of rebuilding the index after every statement.
//!
//! This drives the SAME transactional scripts through graphite (one `Connection`,
//! statements batched so the transaction stays open) and stock `sqlite3` (3.50.4,
//! FTS5, one invocation) and asserts:
//!
//! * BYTE-IDENTICAL `%_data`/`%_idx`/`%_docsize` for the pure-insert shapes
//!   (single txn, multi-txn, mixed autocommit+txn, and a SAVEPOINT script whose
//!   nested savepoint boundary flushes an intermediate segment exactly as
//!   sqlite's `xSavepoint` does);
//! * in-transaction `MATCH` visibility (an uncommitted INSERT is visible to a
//!   later `MATCH` in the same transaction; an uncommitted DELETE hides its row);
//! * `ROLLBACK` discards the transaction's writes entirely (index untouched);
//! * delete/update-containing transactions stay integrity-clean and MATCH-correct
//!   (they flush as a single consolidated rebuild — correct, not byte-identical to
//!   sqlite's incremental tombstone segments; asserted for integrity + MATCH only).
//!
//! Skipped when `sqlite3` with FTS5 is not on PATH.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::{Connection, QueryResult, Value};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-tx-{}-{}-{}.db",
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
        .arg("INSERT INTO f(f) VALUES('integrity-check');")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Dump the three shadow tables as byte-comparable lines.
fn dump_shadows_sqlite(path: &str) -> String {
    let data = sqlite_raw(path, "SELECT id, quote(block) FROM f_data ORDER BY id;");
    let idx = sqlite_raw(
        path,
        "SELECT segid, quote(term), pgno FROM f_idx ORDER BY segid, term, pgno;",
    );
    let ds = sqlite_raw(path, "SELECT id, quote(sz) FROM f_docsize ORDER BY id;");
    format!("DATA\n{data}\nIDX\n{idx}\nDOCSIZE\n{ds}")
}

/// The text of a single scalar result cell (e.g. `PRAGMA integrity_check`).
fn scalar_text(c: &Connection, q: &str) -> String {
    match &c.query(q).unwrap().rows[0][0] {
        Value::Text(t) => String::from(t.as_str()),
        Value::Integer(i) => i.to_string(),
        Value::Null => String::new(),
        other => format!("{other:?}"),
    }
}

fn fmt_result(r: QueryResult) -> String {
    r.rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Integer(i) => i.to_string(),
                    Value::Text(t) => String::from(t.as_str()),
                    Value::Null => String::new(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn dump_shadows_graphite(c: &Connection) -> String {
    let data = fmt_result(
        c.query("SELECT id, quote(block) FROM f_data ORDER BY id")
            .unwrap(),
    );
    let idx = fmt_result(
        c.query("SELECT segid, quote(term), pgno FROM f_idx ORDER BY segid, term, pgno")
            .unwrap(),
    );
    let ds = fmt_result(
        c.query("SELECT id, quote(sz) FROM f_docsize ORDER BY id")
            .unwrap(),
    );
    format!("DATA\n{data}\nIDX\n{idx}\nDOCSIZE\n{ds}")
}

/// Run `script` through a fresh graphite db and a fresh sqlite db, then assert
/// the shadow-table bytes are identical and sqlite accepts graphite's file.
fn assert_bytes_identical(tag: &str, script: &str) {
    if !have_fts5_sqlite() {
        eprintln!("skip {tag}: no fts5 sqlite3");
        return;
    }
    let g = tmp_path(&format!("{tag}-g"));
    let s = tmp_path(&format!("{tag}-s"));

    let mut c = Connection::create(&g).unwrap();
    c.execute_batch(script).unwrap();
    sqlite_raw(&s, script);

    assert_eq!(
        dump_shadows_graphite(&c),
        dump_shadows_sqlite(&s),
        "shadow-table bytes diverge for {tag}"
    );
    assert!(
        sqlite_integrity_ok(&g),
        "sqlite integrity-check rejected graphite's file for {tag}"
    );
    assert_eq!(
        scalar_text(&c, "PRAGMA integrity_check"),
        "ok",
        "graphite integrity_check failed for {tag}"
    );
}

/// A single BEGIN/COMMIT wrapping N inserts must produce ONE level-0 segment,
/// byte-identical to sqlite (which flushes its hash as one segment at commit).
#[test]
fn txn_inserts_flush_as_one_segment() {
    let mut sql = String::from("CREATE VIRTUAL TABLE f USING fts5(a);\nBEGIN;\n");
    for i in 0..20 {
        sql.push_str(&format!(
            "INSERT INTO f VALUES('doc {i} the quick brown fox jumps over the lazy dog');\n"
        ));
    }
    sql.push_str("COMMIT;\n");
    assert_bytes_identical("txn20", &sql);
}

/// Several separate transactions each append their own level-0 segment.
#[test]
fn multiple_transactions_each_one_segment() {
    let mut sql = String::from("CREATE VIRTUAL TABLE f USING fts5(a);\n");
    for t in 0..4 {
        sql.push_str("BEGIN;\n");
        for i in 0..5 {
            sql.push_str(&format!(
                "INSERT INTO f VALUES('batch {t} row {i} lorem ipsum');\n"
            ));
        }
        sql.push_str("COMMIT;\n");
    }
    assert_bytes_identical("multitxn", &sql);
}

/// Autocommit inserts interleaved with an explicit transaction stay byte-identical.
#[test]
fn mixed_autocommit_and_transaction() {
    let sql = "CREATE VIRTUAL TABLE f USING fts5(a);\n\
        INSERT INTO f VALUES('auto one alpha');\n\
        INSERT INTO f VALUES('auto two beta');\n\
        BEGIN;\n\
        INSERT INTO f VALUES('txn three gamma');\n\
        INSERT INTO f VALUES('txn four delta');\n\
        INSERT INTO f VALUES('txn five epsilon');\n\
        COMMIT;\n\
        INSERT INTO f VALUES('auto six zeta');\n";
    assert_bytes_identical("mixed", sql);
}

/// A larger single transaction that crosses the automerge/crisis thresholds still
/// matches sqlite byte-for-byte.
#[test]
fn large_transaction_crosses_merges() {
    let mut sql = String::from("CREATE VIRTUAL TABLE f USING fts5(a);\nBEGIN;\n");
    for i in 0..600 {
        sql.push_str(&format!(
            "INSERT INTO f VALUES('t{} w{} common shared frequent rare{}');\n",
            i % 17,
            i % 11,
            i
        ));
    }
    sql.push_str("COMMIT;\n");
    assert_bytes_identical("txn600", &sql);
}

/// A SAVEPOINT script: the nested savepoint boundary flushes the pending inserts
/// as their own segment (sqlite's `xSavepoint`), a ROLLBACK TO discards only the
/// writes after it, and the outer RELEASE flushes the rest — byte-identical.
#[test]
fn savepoint_boundary_flush() {
    let sql = "CREATE VIRTUAL TABLE f USING fts5(a);\n\
        SAVEPOINT s1;\n\
        INSERT INTO f VALUES('keep one');\n\
        INSERT INTO f VALUES('keep two');\n\
        SAVEPOINT s2;\n\
        INSERT INTO f VALUES('drop three');\n\
        ROLLBACK TO s2;\n\
        INSERT INTO f VALUES('keep four');\n\
        RELEASE s1;\n";
    assert_bytes_identical("savepoint", sql);
}

/// A prefix-index table inside a transaction also flushes as one segment.
#[test]
fn prefix_index_transaction() {
    let sql = "CREATE VIRTUAL TABLE f USING fts5(a, prefix='2 3');\n\
        BEGIN;\n\
        INSERT INTO f VALUES('running quickly always onward');\n\
        INSERT INTO f VALUES('runner runs run ran');\n\
        INSERT INTO f VALUES('walking talking stalking balking');\n\
        COMMIT;\n";
    assert_bytes_identical("prefix", sql);
}

/// In-transaction `MATCH` visibility: an uncommitted INSERT is visible to a
/// `MATCH` later in the same transaction, and an uncommitted DELETE hides its row.
#[test]
fn in_transaction_match_visibility() {
    let g = tmp_path("vis");
    let mut c = Connection::create(&g).unwrap();
    c.execute("CREATE VIRTUAL TABLE f USING fts5(a)").unwrap();
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO f VALUES('hello world')").unwrap();
    c.execute("INSERT INTO f VALUES('goodbye moon')").unwrap();

    let ids = |c: &Connection, q: &str| -> Vec<i64> {
        c.query(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| match &r[0] {
                Value::Integer(i) => *i,
                _ => -1,
            })
            .collect()
    };

    // Uncommitted rows are visible mid-transaction.
    assert_eq!(
        ids(&c, "SELECT rowid FROM f WHERE f MATCH 'hello'"),
        vec![1]
    );
    assert_eq!(ids(&c, "SELECT rowid FROM f WHERE f MATCH 'moon'"), vec![2]);
    assert!(ids(&c, "SELECT rowid FROM f WHERE f MATCH 'absent'").is_empty());

    // An uncommitted DELETE hides its row immediately.
    c.execute("DELETE FROM f WHERE rowid=1").unwrap();
    assert!(ids(&c, "SELECT rowid FROM f WHERE f MATCH 'hello'").is_empty());
    assert_eq!(ids(&c, "SELECT rowid FROM f WHERE f MATCH 'moon'"), vec![2]);

    c.execute("COMMIT").unwrap();

    // After commit the index serves the same result and stays integrity-clean.
    assert!(ids(&c, "SELECT rowid FROM f WHERE f MATCH 'hello'").is_empty());
    assert_eq!(ids(&c, "SELECT rowid FROM f WHERE f MATCH 'moon'"), vec![2]);
    assert_eq!(scalar_text(&c, "PRAGMA integrity_check"), "ok");
}

/// `ROLLBACK` discards the transaction's writes entirely — the index is left
/// exactly as it was before `BEGIN` (here: empty, no leaf segments).
#[test]
fn rollback_discards_transaction() {
    let g = tmp_path("rb");
    let mut c = Connection::create(&g).unwrap();
    c.execute("CREATE VIRTUAL TABLE f USING fts5(a)").unwrap();
    c.execute("INSERT INTO f VALUES('committed alpha')")
        .unwrap();
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO f VALUES('rolled back beta')")
        .unwrap();
    c.execute("INSERT INTO f VALUES('rolled back gamma')")
        .unwrap();
    c.execute("ROLLBACK").unwrap();

    // The rolled-back rows are gone from content and index.
    let count = |q: &str| -> i64 {
        match &c.query(q).unwrap().rows[0][0] {
            Value::Integer(i) => *i,
            _ => -1,
        }
    };
    assert_eq!(count("SELECT count(*) FROM f"), 1);
    assert_eq!(count("SELECT count(*) FROM f WHERE f MATCH 'beta'"), 0);
    assert_eq!(count("SELECT count(*) FROM f WHERE f MATCH 'alpha'"), 1);
    assert_eq!(scalar_text(&c, "PRAGMA integrity_check"), "ok");
}

/// A transaction that DELETEs or UPDATEs a previously-committed document is NOT
/// byte-identical to sqlite (it consolidates to one rebuild rather than sqlite's
/// incremental tombstone segments), but it MUST stay integrity-clean, be readable
/// by stock sqlite, and return the same `MATCH` rows as sqlite.
#[test]
fn delete_update_txn_correct_and_integrity_clean() {
    if !have_fts5_sqlite() {
        eprintln!("skip: no fts5 sqlite3");
        return;
    }
    let script = "CREATE VIRTUAL TABLE f USING fts5(a);\n\
        INSERT INTO f VALUES('original alpha shared');\n\
        INSERT INTO f VALUES('original beta shared');\n\
        INSERT INTO f VALUES('original gamma shared');\n\
        BEGIN;\n\
        UPDATE f SET a='changed alpha now shared' WHERE rowid=1;\n\
        DELETE FROM f WHERE rowid=2;\n\
        INSERT INTO f VALUES('added delta shared');\n\
        COMMIT;\n";

    let g = tmp_path("du-g");
    let s = tmp_path("du-s");
    let mut c = Connection::create(&g).unwrap();
    c.execute_batch(script).unwrap();
    sqlite_raw(&s, script);

    // graphite's own integrity check + sqlite's must both accept the file.
    assert_eq!(scalar_text(&c, "PRAGMA integrity_check"), "ok");
    assert!(
        sqlite_integrity_ok(&g),
        "sqlite integrity-check rejected graphite's delete/update-txn file"
    );

    // MATCH parity across the interesting terms.
    for term in ["shared", "changed", "original", "delta", "beta", "alpha"] {
        let gr = fmt_result(
            c.query(&format!(
                "SELECT rowid FROM f WHERE f MATCH '{term}' ORDER BY rowid"
            ))
            .unwrap(),
        );
        let sr = sqlite_raw(
            &s,
            &format!("SELECT rowid FROM f WHERE f MATCH '{term}' ORDER BY rowid;"),
        );
        assert_eq!(gr, sr, "MATCH '{term}' rows diverge (delete/update txn)");
    }
}
