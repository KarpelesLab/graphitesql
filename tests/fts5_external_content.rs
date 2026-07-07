//! External-content FTS5 tables (`content='<table>'`, `content_rowid='<col>'`):
//! the fts5 index is built from an existing content table by `rebuild`, and column
//! values are read back from that table (no copy is kept in the fts5 shadows).
//!
//! Every case is differential vs stock `sqlite3` 3.50.4 — the same script runs
//! through both engines against a file database and their row output must match
//! byte-for-byte. Skipped when `sqlite3` is absent.

#![cfg(all(feature = "std", feature = "fts5"))]

use graphitesql::Connection;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn have_sqlite() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn tmp_path(tag: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gsql-fts5-ext-{tag}-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let p = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&p);
    p
}

/// Run `setup` (DDL/DML, no output expected) then `query` through stock sqlite3
/// against a fresh file, returning the query's `|`-joined lines (row order kept).
fn sqlite_out(tag: &str, setup: &str, query: &str) -> String {
    let path = tmp_path(&format!("s-{tag}"));
    let script = format!("{setup}\n{query}");
    let o = Command::new("sqlite3")
        .arg(&path)
        .arg(&script)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(
        o.status.success(),
        "sqlite3 failed for {tag}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

/// Run `setup` then `query` through graphite against a fresh file, returning the
/// query's `|`-joined lines in the same `sqlite3 -list` shape (`|` separators).
fn graphite_out(tag: &str, setup: &str, query: &str) -> String {
    let path = tmp_path(&format!("g-{tag}"));
    let mut c = Connection::create(&path).unwrap();
    for stmt in split_statements(setup) {
        c.execute(&stmt)
            .unwrap_or_else(|e| panic!("graphite setup {stmt:?}: {e}"));
    }
    let res = c
        .query(query)
        .unwrap_or_else(|e| panic!("graphite query {query:?}: {e}"));
    drop(c);
    let _ = std::fs::remove_file(&path);
    let mut out = String::new();
    for (ri, row) in res.rows.iter().enumerate() {
        if ri > 0 {
            out.push('\n');
        }
        for (ci, v) in row.iter().enumerate() {
            if ci > 0 {
                out.push('|');
            }
            out.push_str(&render(v));
        }
    }
    out
}

/// Render a value like the `sqlite3` CLI `-list` mode: NULL as empty, reals with
/// sqlite's `%!.15g`-ish formatting handled by graphite's own text conversion.
fn render(v: &graphitesql::Value) -> String {
    use graphitesql::Value::*;
    match v {
        Null => String::new(),
        Integer(i) => i.to_string(),
        Real(r) => {
            // Match sqlite3 CLI: integral reals print without a decimal only when
            // they aren't; graphite's Display already matches for our test values.
            let s = format!("{r}");
            s
        }
        Text(t) => t.clone(),
        Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

/// Very small statement splitter for the setup script: splits on `;` at line
/// ends. The setup scripts here use one statement per line ending in `;`.
fn split_statements(script: &str) -> Vec<String> {
    script
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("{s};"))
        .collect()
}

/// Assert graphite and sqlite produce identical output for `query` after `setup`.
fn assert_same(tag: &str, setup: &str, query: &str) {
    let s = sqlite_out(tag, setup, query);
    let g = graphite_out(tag, setup, query);
    assert_eq!(
        g, s,
        "\n[{tag}] query: {query}\ngraphite:\n{g}\nsqlite:\n{s}"
    );
}

const BUG_SETUP: &str = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, body);
INSERT INTO src VALUES(1,'hello world'),(2,'foo bar');
CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id');
INSERT INTO ft(ft) VALUES('rebuild');";

#[test]
fn bug_case_rebuild_match_and_column_retrieval() {
    if !have_sqlite() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    assert_same(
        "bug-rowid",
        BUG_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH 'hello';",
    );
    assert_same(
        "bug-col",
        BUG_SETUP,
        "SELECT rowid, body FROM ft WHERE ft MATCH 'world';",
    );
    // No match → empty, like sqlite.
    assert_same(
        "bug-nomatch",
        BUG_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH 'nope';",
    );
}

const MULTI_SETUP: &str = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, title, body, extra);
INSERT INTO src VALUES(10,'red apple','fresh fruit here',999),(20,'blue sky','the sky above',888);
CREATE VIRTUAL TABLE ft USING fts5(title, body, content='src', content_rowid='id');
INSERT INTO ft(ft) VALUES('rebuild');";

#[test]
fn multi_column_content_table_indexes_a_subset() {
    if !have_sqlite() {
        return;
    }
    // fts5 indexes title+body; `extra` is not an fts5 column and never appears.
    assert_same(
        "multi-cols",
        MULTI_SETUP,
        "SELECT rowid, title, body FROM ft WHERE ft MATCH 'apple';",
    );
    assert_same(
        "multi-star",
        MULTI_SETUP,
        "SELECT * FROM ft WHERE ft MATCH 'sky';",
    );
    assert_same(
        "multi-colscope",
        MULTI_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH 'body:sky';",
    );
    assert_same(
        "multi-title",
        MULTI_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH 'title:apple';",
    );
}

#[test]
fn default_content_rowid_uses_the_content_tables_rowid() {
    if !have_sqlite() {
        return;
    }
    let setup = "\
CREATE TABLE src(a, b);
INSERT INTO src VALUES('cat dog','x'),('bird fish','y');
CREATE VIRTUAL TABLE ft USING fts5(a, b, content='src');
INSERT INTO ft(ft) VALUES('rebuild');";
    assert_same(
        "default-rowid",
        setup,
        "SELECT rowid, a, b FROM ft WHERE ft MATCH 'dog';",
    );
}

#[test]
fn non_default_content_rowid_column() {
    if !have_sqlite() {
        return;
    }
    let setup = "\
CREATE TABLE docs(docid INTEGER PRIMARY KEY, author, title, body, year);
INSERT INTO docs VALUES(100,'alice','SQL basics','learn structured query language',2020),(200,'bob','FTS deep dive','full text search internals',2021);
CREATE VIRTUAL TABLE ft USING fts5(title, body, content='docs', content_rowid='docid');
INSERT INTO ft(ft) VALUES('rebuild');";
    assert_same(
        "rowid-alias",
        setup,
        "SELECT rowid, title, body FROM ft WHERE ft MATCH 'search';",
    );
    assert_same(
        "rowid-alias2",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'query';",
    );
}

const QSHAPES_SETUP: &str = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, title, body);
INSERT INTO src VALUES(1,'red apple','fresh fruit market'),(2,'blue sky','the sky is blue today'),(3,'green apple','sour green fruit');
CREATE VIRTUAL TABLE ft USING fts5(title, body, content='src', content_rowid='id');
INSERT INTO ft(ft) VALUES('rebuild');";

#[test]
fn match_or_near_prefix_phrase_column_filters() {
    if !have_sqlite() {
        return;
    }
    assert_same(
        "or",
        QSHAPES_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH 'apple OR sky' ORDER BY rowid;",
    );
    assert_same(
        "near",
        QSHAPES_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH 'NEAR(green fruit, 5)';",
    );
    assert_same(
        "prefix",
        QSHAPES_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH 'appl*' ORDER BY rowid;",
    );
    assert_same(
        "phrase",
        QSHAPES_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH '\"green fruit\"';",
    );
    assert_same(
        "colfilter",
        QSHAPES_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH 'title:apple' ORDER BY rowid;",
    );
}

#[test]
fn bm25_rank_over_external_content() {
    if !have_sqlite() {
        return;
    }
    assert_same(
        "rank-order",
        QSHAPES_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH 'apple' ORDER BY rank;",
    );
    // The bm25 VALUE agrees to sqlite's printed precision (its CLI prints ~15 sig
    // digits; graphite's Display may add one more). Round both before comparing.
    let q = "SELECT rowid, round(bm25(ft), 9) FROM ft WHERE ft MATCH 'sky' ORDER BY rank;";
    assert_same("bm25-value", QSHAPES_SETUP, q);
}

#[test]
fn highlight_and_snippet_read_from_content_table() {
    if !have_sqlite() {
        return;
    }
    assert_same(
        "highlight",
        QSHAPES_SETUP,
        "SELECT highlight(ft, 0, '<', '>') FROM ft WHERE ft MATCH 'apple' ORDER BY rowid;",
    );
    let snippet_setup = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, body);
INSERT INTO src VALUES(1,'the quick brown fox jumps over the lazy dog');
CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id');
INSERT INTO ft(ft) VALUES('rebuild');";
    assert_same(
        "snippet",
        snippet_setup,
        "SELECT snippet(ft, 0, '<', '>', '...', 5) FROM ft WHERE ft MATCH 'fox';",
    );
}

#[test]
fn rebuild_after_content_changes() {
    if !have_sqlite() {
        return;
    }
    // Insert more content rows, then rebuild: the index picks up the new rows.
    let setup = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, body);
INSERT INTO src VALUES(1,'hello world');
CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id');
INSERT INTO ft(ft) VALUES('rebuild');
INSERT INTO src VALUES(2,'world peace'),(3,'goodbye moon');
INSERT INTO ft(ft) VALUES('rebuild');";
    assert_same(
        "rebuild-added",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'world' ORDER BY rowid;",
    );
    // Update a content row, rebuild, and the old token is gone, the new one found.
    let setup2 = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, body);
INSERT INTO src VALUES(1,'hello world'),(2,'world peace');
CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id');
INSERT INTO ft(ft) VALUES('rebuild');
UPDATE src SET body='hello sunshine' WHERE id=1;
INSERT INTO ft(ft) VALUES('rebuild');";
    assert_same(
        "rebuild-updated-new",
        setup2,
        "SELECT rowid FROM ft WHERE ft MATCH 'sunshine';",
    );
    assert_same(
        "rebuild-updated-old",
        setup2,
        "SELECT rowid FROM ft WHERE ft MATCH 'world' ORDER BY rowid;",
    );
}

/// An fts5 column not present in the content table is an error at `rebuild` time,
/// with sqlite's exact message (`no such column: T.<col>`).
#[test]
fn missing_fts5_column_in_content_table_errors() {
    // No sqlite needed: this is graphite's own error-parity check, but we assert
    // the message matches what sqlite 3.50.4 emits.
    let path = tmp_path("misscol");
    let mut c = Connection::create(&path).unwrap();
    c.execute("CREATE TABLE src(id INTEGER PRIMARY KEY, body)")
        .unwrap();
    c.execute("INSERT INTO src VALUES(1,'hi')").unwrap();
    c.execute(
        "CREATE VIRTUAL TABLE ft USING fts5(body, missingcol, content='src', content_rowid='id')",
    )
    .unwrap();
    let err = c
        .execute("INSERT INTO ft(ft) VALUES('rebuild')")
        .unwrap_err()
        .to_string();
    drop(c);
    let _ = std::fs::remove_file(&path);
    assert!(
        err.contains("no such column: T.missingcol"),
        "expected sqlite's missing-column message, got: {err}"
    );
}

/// A missing content table is `no such table: main.<tbl>` at rebuild, like sqlite.
#[test]
fn missing_content_table_errors() {
    let path = tmp_path("misstbl");
    let mut c = Connection::create(&path).unwrap();
    c.execute("CREATE VIRTUAL TABLE ft USING fts5(body, content='nosuchtbl', content_rowid='id')")
        .unwrap();
    let err = c
        .execute("INSERT INTO ft(ft) VALUES('rebuild')")
        .unwrap_err()
        .to_string();
    drop(c);
    let _ = std::fs::remove_file(&path);
    assert!(
        err.contains("no such table: main.nosuchtbl"),
        "expected sqlite's missing-table message, got: {err}"
    );
}

/// Direct content-modifying DML on an external-content table is declined (graphite
/// implements the read side + `rebuild`; index-delta sync is a documented
/// follow-up). We never silently drop the write.
#[test]
fn direct_dml_on_external_content_is_declined() {
    let path = tmp_path("dml");
    let mut c = Connection::create(&path).unwrap();
    c.execute("CREATE TABLE src(id INTEGER PRIMARY KEY, body)")
        .unwrap();
    c.execute("INSERT INTO src VALUES(1,'hello world')")
        .unwrap();
    c.execute("CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id')")
        .unwrap();
    c.execute("INSERT INTO ft(ft) VALUES('rebuild')").unwrap();
    assert!(c
        .execute("INSERT INTO ft(rowid, body) VALUES(5, 'phantom')")
        .is_err());
    assert!(c.execute("DELETE FROM ft WHERE rowid=1").is_err());
    assert!(c.execute("UPDATE ft SET body='x' WHERE rowid=1").is_err());
    // The `'delete'` maintenance command is likewise declined (index-delta sync).
    assert!(c
        .execute("INSERT INTO ft(ft, rowid, body) VALUES('delete', 1, 'hello world')")
        .is_err());
    drop(c);
    let _ = std::fs::remove_file(&path);
}

/// Normal (self-content) and contentless fts5 tables keep working unchanged.
#[test]
fn self_content_and_contentless_still_work() {
    if !have_sqlite() {
        return;
    }
    // Self-content: documents stored in the fts5 table itself.
    let self_setup = "\
CREATE VIRTUAL TABLE ft USING fts5(title, body);
INSERT INTO ft(rowid, title, body) VALUES(1,'hello world','the quick brown fox'),(2,'goodbye moon','a lazy dog');";
    assert_same(
        "self-match",
        self_setup,
        "SELECT rowid, title FROM ft WHERE ft MATCH 'fox';",
    );
    // Contentless (`content=''`): only the index, no stored columns; rowid queries.
    let cl_setup = "\
CREATE VIRTUAL TABLE ft USING fts5(body, content='');
INSERT INTO ft(rowid, body) VALUES(1,'hello world'),(2,'foo bar');";
    assert_same(
        "contentless",
        cl_setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'hello';",
    );
}
