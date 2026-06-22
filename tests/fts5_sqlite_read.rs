//! Roadmap D2e-M2: a graphite-written FTS5 table is readable — and `MATCH`-able —
//! by stock `sqlite3`. graphite now stores FTS5 in sqlite's five shadow tables
//! (`_content`/`_docsize`/`_config`/`_idx`/`_data`) with a byte-compatible segment
//! index, so the file round-trips: sqlite opens it, returns the documents, runs
//! full-text `MATCH` queries against graphite's index, and passes integrity-check.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::Connection;
use std::process::Command;

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
