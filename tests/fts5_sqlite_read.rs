//! Roadmap D2e-M2: a graphite-written FTS5 table is readable — and `MATCH`-able —
//! by stock `sqlite3`. graphite now stores FTS5 in sqlite's five shadow tables
//! (`_content`/`_docsize`/`_config`/`_idx`/`_data`) with a byte-compatible segment
//! index, so the file round-trips: sqlite opens it, returns the documents, runs
//! full-text `MATCH` queries against graphite's index, and passes integrity-check.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_path() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-m2b-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let p = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&p);
    p
}

/// Run a query through stock sqlite3 against the file and return its sorted,
/// `|`-joined output (asserting success).
fn sqlite_run(path: &str, q: &str) -> String {
    let o = Command::new("sqlite3").arg(path).arg(q).output().unwrap();
    assert!(
        o.status.success(),
        "sqlite3 failed for {q:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let mut v: Vec<String> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    v.sort();
    v.join("|")
}

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

#[test]
fn sqlite_reads_and_matches_graphite_written_fts5() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-fts5-m2b-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
            .unwrap();
        c.execute(
            "INSERT INTO t(rowid, title, body) VALUES \
             (1,'hello world','the quick brown fox'),\
             (2,'goodbye moon','a lazy dog runs'),\
             (3,'hello again','the fox and the dog')",
        )
        .unwrap();
    } // drop flushes the file

    let sqlite = |q: &str| {
        let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
        assert!(
            o.status.success(),
            "sqlite3 failed for {q:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let mut v: Vec<String> = String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        v.sort();
        v.join("|")
    };

    // sqlite reads the documents from graphite's `_content` (columns joined by
    // '|', rows sorted then joined by '|').
    assert_eq!(
        sqlite("SELECT rowid, title FROM t ORDER BY rowid"),
        "1|hello world|2|goodbye moon|3|hello again"
    );
    // sqlite answers full-text MATCH against graphite's segment index.
    assert_eq!(
        sqlite("SELECT rowid FROM t WHERE t MATCH 'fox' ORDER BY rowid"),
        "1|3"
    );
    assert_eq!(
        sqlite("SELECT rowid FROM t WHERE t MATCH 'dog' ORDER BY rowid"),
        "2|3"
    );
    assert_eq!(
        sqlite("SELECT rowid FROM t WHERE t MATCH 'hello' ORDER BY rowid"),
        "1|3"
    );
    // A column filter and a phrase.
    assert_eq!(
        sqlite("SELECT rowid FROM t WHERE t MATCH 'title:goodbye'"),
        "2"
    );
    assert_eq!(
        sqlite("SELECT rowid FROM t WHERE t MATCH '\"quick brown\"'"),
        "1"
    );
    // The FTS5 internal integrity-check and the database integrity-check both pass.
    let chk = Command::new("sqlite3")
        .arg(&path)
        .arg("INSERT INTO t(t) VALUES('integrity-check');")
        .output()
        .unwrap();
    assert!(
        chk.status.success(),
        "fts5 integrity-check failed: {}",
        String::from_utf8_lossy(&chk.stderr)
    );
    assert_eq!(sqlite("PRAGMA integrity_check"), "ok");

    let _ = std::fs::remove_file(&path);
}

/// A larger table whose segment spans multiple leaf pages: sqlite must use
/// graphite's `%_idx` to find terms across leaves. Each doc has a shared term
/// ("common") plus a unique one ("wordNNNN").
#[test]
fn sqlite_matches_multi_leaf_graphite_fts5() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
            .unwrap();
        let mut sql = String::from("INSERT INTO t(rowid, body) VALUES ");
        for i in 1..=400 {
            if i > 1 {
                sql.push(',');
            }
            sql.push_str(&format!("({i},'common word{i:04}')"));
        }
        c.execute(&sql).unwrap();
    }
    // "common" is in every doc → 400 hits across many leaves.
    assert_eq!(
        sqlite_run(&path, "SELECT count(*) FROM t WHERE t MATCH 'common'"),
        "400"
    );
    // A unique term on (likely) a non-first leaf resolves via %_idx.
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'word0377'"),
        "377"
    );
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'word0001'"),
        "1"
    );
    assert_eq!(sqlite_run(&path, "PRAGMA integrity_check"), "ok");
    let chk = Command::new("sqlite3")
        .arg(&path)
        .arg("INSERT INTO t(t) VALUES('integrity-check');")
        .output()
        .unwrap();
    assert!(
        chk.status.success(),
        "fts5 integrity-check: {}",
        String::from_utf8_lossy(&chk.stderr)
    );
    let _ = std::fs::remove_file(&path);
}

/// The porter tokenizer: graphite stems tokens when indexing, so sqlite's
/// porter-stemmed MATCH finds them in a graphite-written table.
#[test]
fn sqlite_matches_porter_graphite_fts5() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE VIRTUAL TABLE t USING fts5(body, tokenize='porter')")
            .unwrap();
        c.execute(
            "INSERT INTO t(rowid, body) VALUES \
             (1,'the runners are running'),(2,'a connection was connected')",
        )
        .unwrap();
    }
    // "running"/"runners" and "run" all stem to "run".
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'run'"),
        "1"
    );
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'connect'"),
        "2"
    );
    assert_eq!(sqlite_run(&path, "PRAGMA integrity_check"), "ok");
    let _ = std::fs::remove_file(&path);
}

/// After UPDATE and DELETE the index is rebuilt, so sqlite sees the current
/// documents and MATCH reflects the edits.
#[test]
fn sqlite_reads_graphite_fts5_after_update_delete() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
            .unwrap();
        c.execute(
            "INSERT INTO t(rowid, body) VALUES (1,'alpha beta'),(2,'gamma delta'),(3,'epsilon')",
        )
        .unwrap();
        c.execute("UPDATE t SET body='zeta eta' WHERE rowid=2")
            .unwrap();
        c.execute("DELETE FROM t WHERE rowid=3").unwrap();
    }
    // Doc 2's old terms are gone, its new terms present; doc 3 is gone.
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'gamma'"),
        ""
    );
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'zeta'"),
        "2"
    );
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'epsilon'"),
        ""
    );
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'alpha'"),
        "1"
    );
    assert_eq!(
        sqlite_run(&path, "SELECT rowid, body FROM t ORDER BY rowid"),
        "1|alpha beta|2|zeta eta"
    );
    assert_eq!(sqlite_run(&path, "PRAGMA integrity_check"), "ok");
    let _ = std::fs::remove_file(&path);
}

/// Accented Latin text: graphite folds diacritics like sqlite's unicode61
/// default (`café`→`cafe`), so a graphite-written FTS5 table with accents is
/// integrity-clean and MATCHes correctly under stock sqlite3.
#[test]
fn sqlite_matches_accented_graphite_fts5() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
            .unwrap();
        c.execute(
            "INSERT INTO t(rowid, body) VALUES \
             (1,'café résumé'),(2,'naïve über'),(3,'Pâté à la française')",
        )
        .unwrap();
    }
    assert_eq!(sqlite_run(&path, "PRAGMA integrity_check"), "ok");
    // sqlite folds the query too, so the de-accented form matches.
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'cafe'"),
        "1"
    );
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'resume'"),
        "1"
    );
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'naive'"),
        "2"
    );
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'francaise'"),
        "3"
    );
    let _ = std::fs::remove_file(&path);
}

/// Latin Extended-A and Latin Extended Additional: graphite's full diacritic
/// table (derived from sqlite 3.50.4) folds Polish, Czech, Romanian, and
/// Vietnamese precomposed accents the same way unicode61's `remove_diacritics=1`
/// does, so a graphite-written index is integrity-clean and MATCHes the
/// de-accented query under stock sqlite3.
#[test]
fn sqlite_matches_extended_latin_graphite_fts5() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = tmp_path();
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
            .unwrap();
        c.execute(
            "INSERT INTO t(rowid, body) VALUES \
             (1,'zażółć gęślą jaźń'),(2,'Dvořák Antonín'),\
             (3,'București România'),(4,'Tiếng Việt Nam mạ lủ')",
        )
        .unwrap();
    }
    assert_eq!(sqlite_run(&path, "PRAGMA integrity_check"), "ok");
    // Polish ż/ó/ć/ę/ś/ą/ź fold to ASCII bases (gęślą→gesla, jaźń→jazn). The
    // stroke letter ł is NOT a diacritic, so unicode61 keeps it (zażółć→zazołc);
    // graphite keeps it identically.
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'gesla'"),
        "1"
    );
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'jazn'"),
        "1"
    );
    // Czech ř/á.
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'dvorak'"),
        "2"
    );
    // Romanian ș/â.
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'bucuresti'"),
        "3"
    );
    // Vietnamese single-mark ạ/ủ (Latin Extended Additional) fold to a/u; the
    // double-accented ế/ệ are kept verbatim by remove_diacritics=1 (so 'viet'
    // would NOT match) — graphite keeps them identically, so this stays in sync.
    assert_eq!(
        sqlite_run(&path, "SELECT rowid FROM t WHERE t MATCH 'ma lu'"),
        "4"
    );
    let _ = std::fs::remove_file(&path);
}
