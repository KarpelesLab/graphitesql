//! Direct-DML write path for external-content and contentless FTS5 tables:
//! `INSERT INTO ft(rowid, <cols>)` adds a document's postings, the special
//! `INSERT INTO ft(ft, rowid, <cols>) VALUES('delete', …)` command subtracts them,
//! `'delete-all'` clears the index, and a contentless table (`content=''`) reads
//! its columns back as NULL (with `highlight`/`snippet` also NULL) while `MATCH`
//! still works off the inverted index.
//!
//! Every case is differential vs stock `sqlite3` 3.50.4 (same script through both
//! engines against a file database; ROW output must match byte-for-byte). The
//! on-disk cases additionally have `sqlite3` MATCH and `PRAGMA integrity_check` a
//! graphite-written database. Skipped when `sqlite3` is absent.

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
        "gsql-fts5-clw-{tag}-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let p = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&p);
    p
}

/// Run `setup` (DDL/DML, no output) then `query` through stock sqlite3 against a
/// fresh file, returning the query's `|`-joined lines (row order kept).
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
/// query's `|`-joined lines in the same `sqlite3 -list` shape.
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
    render_rows(&res.rows)
}

fn render_rows(rows: &[Vec<graphitesql::Value>]) -> String {
    let mut out = String::new();
    for (ri, row) in rows.iter().enumerate() {
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

fn render(v: &graphitesql::Value) -> String {
    use graphitesql::Value::*;
    match v {
        Null => String::new(),
        Integer(i) => i.to_string(),
        Real(r) => format!("{r}"),
        Text(t) => String::from(t.as_str()),
        Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

/// Split a `;`-terminated setup script into individual statements.
fn split_statements(script: &str) -> Vec<String> {
    script
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("{s};"))
        .collect()
}

/// Assert graphite and sqlite produce identical `query` output after `setup`.
fn assert_same(tag: &str, setup: &str, query: &str) {
    let s = sqlite_out(tag, setup, query);
    let g = graphite_out(tag, setup, query);
    assert_eq!(
        g, s,
        "\n[{tag}] query: {query}\ngraphite:\n{g}\nsqlite:\n{s}"
    );
}

// ---------------------------------------------------------------------------
// Gap 1: direct INSERT on an external-content table indexes the supplied text.
// ---------------------------------------------------------------------------

const EXT_SETUP: &str = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, body);
INSERT INTO src VALUES(1,'hello');
CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id');
INSERT INTO ft(ft) VALUES('rebuild');
INSERT INTO src VALUES(2,'world');
INSERT INTO ft(rowid, body) VALUES(2,'world');";

#[test]
fn external_direct_insert_then_match() {
    if !have_sqlite() {
        return;
    }
    assert_same(
        "ext-ins-world",
        EXT_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH 'world';",
    );
    assert_same(
        "ext-ins-hello",
        EXT_SETUP,
        "SELECT rowid FROM ft WHERE ft MATCH 'hello';",
    );
    // The supplied text is indexed even when it diverges from the content table.
    let setup = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, body);
CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id');
INSERT INTO src VALUES(2,'world');
INSERT INTO ft(rowid, body) VALUES(2,'zzz');";
    assert_same(
        "ext-supplied-zzz",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'zzz';",
    );
    assert_same(
        "ext-supplied-noworld",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'world';",
    );
}

// ---------------------------------------------------------------------------
// Gap 2: the `'delete'` command (contentless & external content).
// ---------------------------------------------------------------------------

#[test]
fn delete_command_contentless() {
    if !have_sqlite() {
        return;
    }
    let setup = "\
CREATE VIRTUAL TABLE ft USING fts5(x, content='');
INSERT INTO ft(rowid, x) VALUES(1,'a b'),(2,'c d');
INSERT INTO ft(ft, rowid, x) VALUES('delete', 1, 'a b');";
    assert_same(
        "del-cl-a",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'a';",
    );
    assert_same(
        "del-cl-b",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'b';",
    );
    assert_same(
        "del-cl-c",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'c';",
    );
}

#[test]
fn delete_command_external() {
    if !have_sqlite() {
        return;
    }
    let setup = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, body);
INSERT INTO src VALUES(1,'a b'),(2,'c d');
CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id');
INSERT INTO ft(ft) VALUES('rebuild');
INSERT INTO ft(ft, rowid, body) VALUES('delete', 1, 'a b');";
    assert_same(
        "del-ext-a",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'a';",
    );
    assert_same(
        "del-ext-c",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'c';",
    );
}

#[test]
fn delete_command_partial_old_text_matches_sqlite() {
    if !have_sqlite() {
        return;
    }
    // A partial `'delete'` (subset of the doc's tokens) subtracts exactly those
    // tokens' postings; the rest of the doc stays matchable — like sqlite.
    let setup = "\
CREATE VIRTUAL TABLE ft USING fts5(x, content='');
INSERT INTO ft(rowid, x) VALUES(1,'a b c');
INSERT INTO ft(ft, rowid, x) VALUES('delete', 1, 'a');";
    assert_same(
        "del-partial-a",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'a';",
    );
    assert_same(
        "del-partial-b",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'b';",
    );
    assert_same(
        "del-partial-c",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'c';",
    );
}

#[test]
fn delete_all_command_contentless_and_external() {
    if !have_sqlite() {
        return;
    }
    let cl = "\
CREATE VIRTUAL TABLE ft USING fts5(x, content='');
INSERT INTO ft(rowid, x) VALUES(1,'a'),(2,'b');
INSERT INTO ft(ft) VALUES('delete-all');";
    assert_same(
        "delall-cl",
        cl,
        "SELECT rowid FROM ft WHERE ft MATCH 'a OR b';",
    );
    let ext = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, body);
INSERT INTO src VALUES(1,'a'),(2,'b');
CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id');
INSERT INTO ft(ft) VALUES('rebuild');
INSERT INTO ft(ft) VALUES('delete-all');";
    assert_same(
        "delall-ext",
        ext,
        "SELECT rowid FROM ft WHERE ft MATCH 'a OR b';",
    );
}

#[test]
fn additive_insert_same_rowid_unions_terms() {
    if !have_sqlite() {
        return;
    }
    // Re-inserting the same rowid adds the new tokens (union of terms); each term's
    // positions come from the latest insert containing it — matching sqlite.
    let setup = "\
CREATE VIRTUAL TABLE ft USING fts5(x, content='');
INSERT INTO ft(rowid, x) VALUES(1,'a b');
INSERT INTO ft(rowid, x) VALUES(1,'c');";
    assert_same(
        "add-a",
        setup,
        "SELECT count(*) FROM ft WHERE ft MATCH 'a';",
    );
    assert_same(
        "add-b",
        setup,
        "SELECT count(*) FROM ft WHERE ft MATCH 'b';",
    );
    assert_same(
        "add-c",
        setup,
        "SELECT count(*) FROM ft WHERE ft MATCH 'c';",
    );
}

// ---------------------------------------------------------------------------
// Gap 3: contentless `SELECT <col>` → NULL; highlight/snippet → NULL; MATCH ok.
// ---------------------------------------------------------------------------

#[test]
fn contentless_select_column_is_null() {
    if !have_sqlite() {
        return;
    }
    let setup = "\
CREATE VIRTUAL TABLE ft USING fts5(x, content='');
INSERT INTO ft(rowid, x) VALUES(1,'hello world');";
    assert_same("cl-col", setup, "SELECT quote(x) FROM ft;");
    assert_same(
        "cl-star",
        setup,
        "SELECT quote(x) FROM ft WHERE ft MATCH 'hello';",
    );
    assert_same(
        "cl-rowid",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'world';",
    );
    assert_same(
        "cl-highlight",
        setup,
        "SELECT quote(highlight(ft,0,'[',']')) FROM ft WHERE ft MATCH 'hello';",
    );
    assert_same(
        "cl-snippet",
        setup,
        "SELECT quote(snippet(ft,0,'[',']','...',5)) FROM ft WHERE ft MATCH 'hello';",
    );
}

#[test]
fn delete_then_reinsert() {
    if !have_sqlite() {
        return;
    }
    let setup = "\
CREATE VIRTUAL TABLE ft USING fts5(x, content='');
INSERT INTO ft(rowid, x) VALUES(1,'apple');
INSERT INTO ft(ft, rowid, x) VALUES('delete', 1, 'apple');
INSERT INTO ft(rowid, x) VALUES(1,'banana');";
    assert_same(
        "dr-apple",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'apple';",
    );
    assert_same(
        "dr-banana",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'banana';",
    );
}

#[test]
fn contentless_multi_column_and_query_shapes() {
    if !have_sqlite() {
        return;
    }
    let setup = "\
CREATE VIRTUAL TABLE ft USING fts5(a, b, content='');
INSERT INTO ft(rowid, a, b) VALUES(1,'red apple','fresh fruit'),(2,'blue sky','the sky is blue');";
    assert_same(
        "cl-mc-phrase",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH '\"fresh fruit\"';",
    );
    assert_same(
        "cl-mc-colscope",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'b:sky';",
    );
    assert_same(
        "cl-mc-and",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'red AND apple';",
    );
    assert_same(
        "cl-mc-prefix",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'app*';",
    );
    assert_same(
        "cl-mc-cols-null",
        setup,
        "SELECT quote(a), quote(b) FROM ft ORDER BY rowid;",
    );
}

// ---------------------------------------------------------------------------
// External DELETE / UPDATE (via WHERE) — supported; contentless rejects them.
// ---------------------------------------------------------------------------

#[test]
fn external_delete_via_where() {
    if !have_sqlite() {
        return;
    }
    let setup = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, body);
INSERT INTO src VALUES(1,'a b'),(2,'c');
CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id');
INSERT INTO ft(ft) VALUES('rebuild');
DELETE FROM ft WHERE rowid=1;";
    assert_same(
        "ext-del-a",
        setup,
        "SELECT count(*) FROM ft WHERE ft MATCH 'a';",
    );
    assert_same(
        "ext-del-c",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'c';",
    );
}

#[test]
fn external_update_via_where() {
    if !have_sqlite() {
        return;
    }
    let setup = "\
CREATE TABLE src(id INTEGER PRIMARY KEY, body);
INSERT INTO src VALUES(1,'old');
CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id');
INSERT INTO ft(ft) VALUES('rebuild');
UPDATE ft SET body='new' WHERE rowid=1;";
    assert_same(
        "ext-upd-old",
        setup,
        "SELECT count(*) FROM ft WHERE ft MATCH 'old';",
    );
    assert_same(
        "ext-upd-new",
        setup,
        "SELECT rowid FROM ft WHERE ft MATCH 'new';",
    );
}

#[test]
fn contentless_delete_and_update_are_rejected() {
    if !have_sqlite() {
        return;
    }
    // sqlite rejects `DELETE`/`UPDATE` on a contentless table; graphite errors too.
    let path = tmp_path("cl-reject");
    let mut c = Connection::create(&path).unwrap();
    c.execute("CREATE VIRTUAL TABLE ft USING fts5(x, content='');")
        .unwrap();
    c.execute("INSERT INTO ft(rowid, x) VALUES(1,'a b');")
        .unwrap();
    assert!(
        c.execute("DELETE FROM ft WHERE rowid=1;").is_err(),
        "contentless DELETE should be rejected"
    );
    assert!(
        c.execute("UPDATE ft SET x='c' WHERE rowid=1;").is_err(),
        "contentless UPDATE should be rejected"
    );
    drop(c);
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// On-disk byte compatibility: graphite writes, sqlite reads / integrity-checks.
// ---------------------------------------------------------------------------

fn sqlite_query_file(path: &str, query: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(path)
        .arg(query)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "sqlite3 read failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

#[test]
fn graphite_written_contentless_is_sqlite_readable() {
    if !have_sqlite() {
        return;
    }
    let path = tmp_path("bytecompat-cl");
    {
        let mut c = Connection::create(&path).unwrap();
        for stmt in [
            "CREATE VIRTUAL TABLE ft USING fts5(x, content='');",
            "INSERT INTO ft(rowid, x) VALUES(1,'alpha beta'),(2,'gamma delta'),(3,'beta gamma');",
            "INSERT INTO ft(ft, rowid, x) VALUES('delete', 2, 'gamma delta');",
            "INSERT INTO ft(rowid, x) VALUES(4,'delta epsilon');",
        ] {
            c.execute(stmt).unwrap();
        }
        drop(c);
    }
    // sqlite MATCHes the graphite-written contentless index.
    assert_eq!(
        sqlite_query_file(&path, "SELECT rowid FROM ft WHERE ft MATCH 'beta';"),
        "1\n3"
    );
    assert_eq!(
        sqlite_query_file(&path, "SELECT rowid FROM ft WHERE ft MATCH 'gamma';"),
        "3"
    );
    assert_eq!(
        sqlite_query_file(&path, "SELECT rowid FROM ft WHERE ft MATCH 'delta';"),
        "4"
    );
    // A contentless column reads back NULL under sqlite too.
    assert_eq!(
        sqlite_query_file(&path, "SELECT quote(x) FROM ft ORDER BY rowid;"),
        "NULL\nNULL\nNULL"
    );
    // PRAGMA integrity_check and the fts5 internal integrity-check both pass.
    assert_eq!(sqlite_query_file(&path, "PRAGMA integrity_check;"), "ok");
    assert_eq!(
        sqlite_query_file(
            &path,
            "INSERT INTO ft(ft) VALUES('integrity-check'); SELECT 'ok';"
        ),
        "ok"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn graphite_written_external_is_sqlite_readable() {
    if !have_sqlite() {
        return;
    }
    let path = tmp_path("bytecompat-ext");
    {
        let mut c = Connection::create(&path).unwrap();
        for stmt in [
            "CREATE TABLE src(id INTEGER PRIMARY KEY, body);",
            "INSERT INTO src VALUES(1,'hello world'),(2,'foo bar');",
            "CREATE VIRTUAL TABLE ft USING fts5(body, content='src', content_rowid='id');",
            "INSERT INTO ft(ft) VALUES('rebuild');",
            "INSERT INTO src VALUES(3,'baz qux');",
            "INSERT INTO ft(rowid, body) VALUES(3,'baz qux');",
            "INSERT INTO ft(ft, rowid, body) VALUES('delete', 2, 'foo bar');",
        ] {
            c.execute(stmt).unwrap();
        }
        drop(c);
    }
    assert_eq!(
        sqlite_query_file(&path, "SELECT rowid FROM ft WHERE ft MATCH 'baz';"),
        "3"
    );
    assert_eq!(
        sqlite_query_file(&path, "SELECT rowid FROM ft WHERE ft MATCH 'foo';"),
        ""
    );
    assert_eq!(
        sqlite_query_file(&path, "SELECT rowid FROM ft WHERE ft MATCH 'hello';"),
        "1"
    );
    assert_eq!(sqlite_query_file(&path, "PRAGMA integrity_check;"), "ok");
    assert_eq!(
        sqlite_query_file(
            &path,
            "INSERT INTO ft(ft) VALUES('integrity-check'); SELECT 'ok';"
        ),
        "ok"
    );
    let _ = std::fs::remove_file(&path);
}
