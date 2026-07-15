//! `PRAGMA integrity_check` detects a MALFORMED FTS5 inverted index — an index
//! whose decoded `(term, rowid, position)` tuples no longer match a fresh
//! re-tokenization of the `%_content` documents, exactly like sqlite's
//! `fts5IntegrityMethod` ("malformed inverted index for FTS5 table …").
//!
//! The overriding requirement is ZERO false positives: every valid FTS5 shape
//! must stay `ok`. The checker only reports a table it decoded with certainty
//! (a pure-insert index) and skips every shape it cannot resolve safely
//! (tombstone/update history, doclist-index spanning, external/contentless), so
//! these tests assert both halves: valid → `ok`, and a deliberately-desynced
//! index → detected.

#![cfg(feature = "std")]
#![cfg(feature = "fts5")]

use graphitesql::{Connection, Value};

fn check(c: &Connection) -> String {
    c.query("PRAGMA integrity_check;")
        .unwrap()
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Text(t) => t.to_string(),
            v => format!("{v:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_ok(c: &Connection, label: &str) {
    let r = check(c);
    assert_eq!(r, "ok", "{label}: integrity_check should be ok, got: {r}");
}

/// Every valid self-content and non-self-content FTS5 shape stays `ok` — no false
/// positive. Covers empty, single/multi-segment, in-transaction, delete-heavy,
/// INSERT OR REPLACE, UPDATE, prefix, multi-column, ascii/unicode tokenizers, and
/// the (skipped) external-content and contentless modes.
#[test]
fn valid_shapes_stay_ok() {
    // empty, then a single insert.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE e USING fts5(a);").unwrap();
    assert_ok(&c, "empty");
    c.execute("INSERT INTO e VALUES('one two three');").unwrap();
    assert_ok(&c, "single insert");

    // Multi-segment: many separate autocommit inserts (no merge).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE m USING fts5(a,b);")
        .unwrap();
    for i in 0..30 {
        c.execute(&format!(
            "INSERT INTO m VALUES('word{i} common alpha','beta gamma{i}');"
        ))
        .unwrap();
    }
    assert_ok(&c, "multi-segment");

    // In-transaction bulk insert.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE tx USING fts5(a);").unwrap();
    c.execute("BEGIN;").unwrap();
    for i in 0..50 {
        c.execute(&format!("INSERT INTO tx VALUES('doc {i} shared token');"))
            .unwrap();
    }
    c.execute("COMMIT;").unwrap();
    assert_ok(&c, "in-transaction");

    // Delete-heavy (tombstones present → conservatively skipped, never a false
    // positive).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE d USING fts5(a);").unwrap();
    for i in 0..40 {
        c.execute(&format!("INSERT INTO d VALUES('token{i} common');"))
            .unwrap();
    }
    for i in (0..40).step_by(2) {
        c.execute(&format!("DELETE FROM d WHERE rowid={};", i + 1))
            .unwrap();
    }
    assert_ok(&c, "delete-heavy");

    // INSERT OR REPLACE and UPDATE (delete+insert → tombstone → skipped).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING fts5(a);").unwrap();
    c.execute("INSERT INTO r(rowid,a) VALUES(1,'original text');")
        .unwrap();
    c.execute("INSERT OR REPLACE INTO r(rowid,a) VALUES(1,'replaced text');")
        .unwrap();
    assert_ok(&c, "insert-or-replace");
    c.execute("UPDATE r SET a='third revision' WHERE rowid=1;")
        .unwrap();
    assert_ok(&c, "update");

    // Prefix index.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE p USING fts5(a, prefix='2 3');")
        .unwrap();
    for i in 0..20 {
        c.execute(&format!("INSERT INTO p VALUES('prefixword{i} another');"))
            .unwrap();
    }
    assert_ok(&c, "prefix");

    // Multi-column.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE mc USING fts5(a,b,cc);")
        .unwrap();
    c.execute("INSERT INTO mc VALUES('alpha','beta','gamma delta');")
        .unwrap();
    c.execute("INSERT INTO mc VALUES('one','two three','four');")
        .unwrap();
    assert_ok(&c, "multi-column");

    // ascii tokenizer.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE ta USING fts5(a, tokenize='ascii');")
        .unwrap();
    c.execute("INSERT INTO ta VALUES('Hello WORLD Foo');")
        .unwrap();
    c.execute("INSERT INTO ta VALUES('bar baz qux');").unwrap();
    assert_ok(&c, "ascii tokenizer");

    // unicode tokenizer (default) with non-ascii text.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE tu USING fts5(a);").unwrap();
    c.execute("INSERT INTO tu VALUES('café déjà vu');").unwrap();
    c.execute("INSERT INTO tu VALUES('naïve résumé');").unwrap();
    assert_ok(&c, "unicode tokenizer");

    // Contentless (no local documents → skipped).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE cl USING fts5(a, content='');")
        .unwrap();
    c.execute("INSERT INTO cl(rowid,a) VALUES(1,'contentless doc');")
        .unwrap();
    assert_ok(&c, "contentless");

    // External content (documents live in another table → skipped).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE ext(id INTEGER PRIMARY KEY, body TEXT);")
        .unwrap();
    c.execute("INSERT INTO ext VALUES(1,'external body text');")
        .unwrap();
    c.execute("CREATE VIRTUAL TABLE ec USING fts5(body, content='ext', content_rowid='id');")
        .unwrap();
    c.execute("INSERT INTO ec(ec, rowid, body) VALUES('rebuild', 1, 'external body text');")
        .unwrap();
    assert_ok(&c, "external content");
}

/// A tampered index is DETECTED, while an untampered copy stays `ok`.
#[test]
fn tampered_index_is_detected() {
    // (1) Desync the DOCUMENTS from the index by editing the `%_content` shadow
    //     directly (bypassing the vtab). The index still decodes cleanly but no
    //     longer matches the re-tokenized content → detected (the checksum path,
    //     the shape the two historical stale-index bugs hit).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(a,b);")
        .unwrap();
    c.execute("INSERT INTO t VALUES('hello world','foo bar');")
        .unwrap();
    c.execute("INSERT INTO t VALUES('the quick brown','fox jumps');")
        .unwrap();
    assert_ok(&c, "before content tamper");
    c.execute("UPDATE t_content SET c0='completely different text' WHERE id=1;")
        .unwrap();
    assert!(
        check(&c).contains("malformed inverted index for FTS5 table main.t"),
        "content desync must be detected, got: {}",
        check(&c)
    );

    // (2) Remove a leaf page the structure record references (a structurally
    //     impossible file) → detected.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(a,b);")
        .unwrap();
    c.execute("INSERT INTO t VALUES('hello world','foo bar');")
        .unwrap();
    c.execute("INSERT INTO t VALUES('the quick brown','fox jumps');")
        .unwrap();
    c.execute("INSERT INTO t VALUES('lorem ipsum','dolor sit');")
        .unwrap();
    assert_ok(&c, "before leaf delete");
    c.execute("DELETE FROM t_data WHERE id=(SELECT max(id) FROM t_data);")
        .unwrap();
    assert!(
        check(&c).contains("malformed inverted index for FTS5 table main.t"),
        "missing leaf must be detected, got: {}",
        check(&c)
    );

    // Control: an untouched sibling table in the same database stays ok.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE keep USING fts5(a);")
        .unwrap();
    c.execute("INSERT INTO keep VALUES('this stays valid');")
        .unwrap();
    assert_ok(&c, "untampered control");
}

/// Cross-engine parity: a self-content FTS5 file WRITTEN BY stock `sqlite3` stays
/// `ok` under graphite's checker — proving graphite's re-tokenization and index
/// decode agree with sqlite's on positions/columns. Skipped when `sqlite3` is
/// absent (CI pins 3.50.4).
#[test]
fn sqlite_written_files_stay_ok() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping cross-engine parity check");
        return;
    }
    let tmp = std::env::temp_dir();
    let cases: &[(&str, &str)] = &[
        (
            "uni",
            "CREATE VIRTUAL TABLE t USING fts5(a,b);\
             INSERT INTO t VALUES('hello world foo','the quick brown fox');\
             INSERT INTO t VALUES('lorem ipsum dolor','sit amet consectetur');\
             INSERT INTO t VALUES('cafe deja vu','one two three four five');",
        ),
        (
            "ascii",
            "CREATE VIRTUAL TABLE t USING fts5(a, tokenize='ascii');\
             INSERT INTO t VALUES('Hello WORLD Foo');\
             INSERT INTO t VALUES('bar baz qux');",
        ),
        (
            "prefix",
            "CREATE VIRTUAL TABLE t USING fts5(a, prefix='2 3');\
             INSERT INTO t VALUES('prefixed words here');\
             INSERT INTO t VALUES('another set entirely');",
        ),
    ];
    for (tag, ddl) in cases {
        let path = tmp.join(format!("gsql-fts5-integ-{}-{tag}.db", std::process::id()));
        let path = path.to_string_lossy().into_owned();
        let _ = std::fs::remove_file(&path);
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg(format!("{ddl} PRAGMA integrity_check;"))
            .output()
            .unwrap();
        assert!(
            out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "ok",
            "sqlite3 build/check failed for {tag}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Also build a spanning (many-doc) index for one case via sqlite3.
        let c = Connection::open(&path).unwrap();
        assert_ok(&c, &format!("sqlite-written {tag}"));
        drop(c);
        let _ = std::fs::remove_file(&path);
    }

    // Spanning (doclist-index) shape, sqlite-written: must not false-positive
    // (graphite conservatively skips it, but the file must decode/scan cleanly).
    let path = tmp
        .join(format!("gsql-fts5-integ-{}-span.db", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    let mut ddl = String::from("CREATE VIRTUAL TABLE t USING fts5(a);\nBEGIN;\n");
    for i in 0..6000 {
        ddl.push_str(&format!("INSERT INTO t VALUES('common uniq{i}');\n"));
    }
    ddl.push_str("COMMIT;\nPRAGMA integrity_check;\n");
    // The DDL is far too long for a single argv entry — feed it on stdin.
    use std::io::Write;
    let mut child = Command::new("sqlite3")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(ddl.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "ok",
        "sqlite3 spanning build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let c = Connection::open(&path).unwrap();
    assert_ok(&c, "sqlite-written spanning");
    drop(c);
    let _ = std::fs::remove_file(&path);
}
