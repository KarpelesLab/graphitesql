//! The `fts5vocab` virtual table: a read-only view over an FTS5 table's
//! vocabulary in its `row`, `col`, and `instance` forms. Verified differentially
//! against the `sqlite3` CLI — fts5vocab output depends only on document content
//! and tokenization, so the same logical corpus is built in both engines.

#![cfg(all(feature = "std", feature = "fts5"))]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn rows(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|r| r.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build `script` in both engines and compare `query` (which should carry its
/// own ORDER BY so row order is deterministic).
fn check(script: &str, query: &str) {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let want = {
        let o = Command::new("sqlite3")
            .arg(":memory:")
            .arg(format!("{script}; {query};"))
            .output()
            .unwrap();
        assert!(o.status.success(), "sqlite failed: {o:?}");
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    };
    let mut c = Connection::open_memory().unwrap();
    for s in script.split(';') {
        if !s.trim().is_empty() {
            c.execute(s.trim()).unwrap();
        }
    }
    let got = rows(&c, query);
    assert_eq!(
        got, want,
        "fts5vocab diverged\nscript: {script}\nquery: {query}"
    );
}

const CORPUS: &str = "CREATE VIRTUAL TABLE ft USING fts5(a, b); \
     INSERT INTO ft VALUES('hello world hello','foo bar'),('world of bar','hello again')";

#[test]
fn fts5vocab_row() {
    check(
        &format!("{CORPUS}; CREATE VIRTUAL TABLE v USING fts5vocab(ft, 'row')"),
        "SELECT term, doc, cnt FROM v ORDER BY term",
    );
}

#[test]
fn fts5vocab_col() {
    check(
        &format!("{CORPUS}; CREATE VIRTUAL TABLE v USING fts5vocab(ft, 'col')"),
        "SELECT term, col, doc, cnt FROM v ORDER BY term, col",
    );
}

#[test]
fn fts5vocab_instance() {
    check(
        &format!("{CORPUS}; CREATE VIRTUAL TABLE v USING fts5vocab(ft, 'instance')"),
        "SELECT term, doc, col, offset FROM v ORDER BY term, doc, col, offset",
    );
}

#[test]
fn fts5vocab_aggregate_query() {
    // A vocab table composes with ordinary SQL: total tokens, distinct terms.
    check(
        &format!("{CORPUS}; CREATE VIRTUAL TABLE v USING fts5vocab(ft, 'row')"),
        "SELECT sum(cnt), count(*) FROM v",
    );
    check(
        &format!("{CORPUS}; CREATE VIRTUAL TABLE v USING fts5vocab(ft, 'row')"),
        "SELECT term FROM v WHERE cnt >= 2 ORDER BY term",
    );
}

#[test]
fn fts5vocab_three_columns_and_repeats() {
    let script = "CREATE VIRTUAL TABLE docs USING fts5(title, body, tags); \
         INSERT INTO docs VALUES('the cat','a cat sat on the mat','animal pet'), \
                                ('the dog','the dog ran','animal'), \
                                ('birds','many birds sing sing sing','animal wild'); \
         CREATE VIRTUAL TABLE vr USING fts5vocab(docs, 'row'); \
         CREATE VIRTUAL TABLE vc USING fts5vocab(docs, 'col')";
    check(script, "SELECT term, doc, cnt FROM vr ORDER BY term");
    check(
        script,
        "SELECT term, col, doc, cnt FROM vc ORDER BY term, col",
    );
}

#[test]
fn fts5vocab_persists_across_reopen() {
    // The vocab table is recomputed from the corpus on every open (no storage of
    // its own); reopening a file and querying it works.
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-fts5vocab-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE VIRTUAL TABLE ft USING fts5(a, b)")
            .unwrap();
        c.execute("INSERT INTO ft VALUES('alpha beta','beta gamma')")
            .unwrap();
        c.execute("CREATE VIRTUAL TABLE v USING fts5vocab(ft, 'row')")
            .unwrap();
    }
    let c = Connection::open(&path).unwrap();
    assert_eq!(
        rows(&c, "SELECT term, doc, cnt FROM v ORDER BY term"),
        "alpha|1|1\nbeta|1|2\ngamma|1|1"
    );
    let _ = std::fs::remove_file(&path);
}
