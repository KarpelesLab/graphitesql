//! Roadmap D2: the built-in `fts5` virtual-table module. This first slice is the
//! *document store* — `CREATE VIRTUAL TABLE … USING fts5(col, …)` declares the
//! text columns and documents round-trip through the persistent `<name>_data`
//! backing table. The tokenizer and `MATCH` querying build on top of this.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

#[test]
fn stores_and_retrieves_documents() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    c.execute(
        "INSERT INTO t(title, body) VALUES \
         ('Hello','the quick brown fox'),('Bye','the lazy dog')",
    )
    .unwrap();
    // Documents come back in rowid order, with an implicit 1-based rowid.
    assert_eq!(
        rows(&c, "SELECT rowid, title, body FROM t ORDER BY rowid"),
        [
            vec![
                Value::Integer(1),
                text("Hello"),
                text("the quick brown fox")
            ],
            vec![Value::Integer(2), text("Bye"), text("the lazy dog")],
        ]
    );
    // `*` expands to the declared columns only (no hidden rowid).
    assert_eq!(
        rows(&c, "SELECT * FROM t WHERE rowid = 1"),
        [vec![text("Hello"), text("the quick brown fox")]]
    );
}

#[test]
fn insert_without_a_column_list() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(a, b)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('x','y'), ('p','q')")
        .unwrap();
    assert_eq!(
        rows(&c, "SELECT a, b FROM t ORDER BY rowid"),
        [vec![text("x"), text("y")], vec![text("p"), text("q")],]
    );
}

#[test]
fn update_and_delete_documents() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('one'),('two'),('three')")
        .unwrap();
    c.execute("UPDATE t SET body = 'TWO' WHERE rowid = 2")
        .unwrap();
    c.execute("DELETE FROM t WHERE rowid = 1").unwrap();
    assert_eq!(
        rows(&c, "SELECT rowid, body FROM t ORDER BY rowid"),
        [
            vec![Value::Integer(2), text("TWO")],
            vec![Value::Integer(3), text("three")],
        ]
    );
}

#[test]
fn table_info_columns_are_untyped() {
    // FTS5 declares its columns with no type — PRAGMA table_info reports an empty
    // type string, byte-for-byte like sqlite3.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    let r = c.query("PRAGMA table_info(t)").unwrap();
    assert_eq!(r.rows[0][1], text("title"));
    assert_eq!(r.rows[0][2], text("")); // empty type
    assert_eq!(r.rows[1][1], text("body"));
    assert_eq!(r.rows[1][2], text(""));
}

#[test]
fn config_options_are_ignored_only_columns_declared() {
    // A `tokenize = …` option arg is not a column; only `a` and `b` are declared.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(a, b, tokenize = 'porter')")
        .unwrap();
    assert_eq!(
        c.query("PRAGMA table_info(t)").unwrap().rows.len(),
        2,
        "the tokenize option must not become a column"
    );
    c.execute("INSERT INTO t VALUES ('hi','there')").unwrap();
    assert_eq!(
        rows(&c, "SELECT a, b FROM t"),
        [vec![text("hi"), text("there")]]
    );
}

#[test]
fn persists_and_passes_integrity_check() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('alpha'),('beta')")
        .unwrap();
    // The documents live in the real backing table.
    assert_eq!(
        rows(&c, "SELECT rowid, body FROM t_data ORDER BY rowid"),
        [
            vec![Value::Integer(1), text("alpha")],
            vec![Value::Integer(2), text("beta")],
        ]
    );
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        text("ok")
    );
}
