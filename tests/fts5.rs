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
fn match_queries_tokens() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    c.execute(
        "INSERT INTO t VALUES \
         ('Hello World','the quick brown fox'),\
         ('Goodbye','the lazy dog'),\
         ('Mixed','Fox and Dog')",
    )
    .unwrap();

    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not an integer rowid"),
            })
            .collect::<Vec<_>>()
    };

    // `t MATCH x` searches across all columns; column refs scope to one column.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'fox' ORDER BY rowid"),
        [1, 3]
    );
    assert_eq!(
        ids("SELECT rowid FROM t WHERE body MATCH 'dog' ORDER BY rowid"),
        [2, 3]
    );
    assert_eq!(ids("SELECT rowid FROM t WHERE title MATCH 'hello'"), [1]);
    // Space-separated tokens are AND-ed; matching is case-insensitive.
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'quick fox'"), [1]);
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'FOX' ORDER BY rowid"),
        [1, 3]
    );
    // No match, and a column-scoped token that lives in a different column.
    assert!(ids("SELECT rowid FROM t WHERE t MATCH 'zebra'").is_empty());
    assert!(ids("SELECT rowid FROM t WHERE title MATCH 'fox'").is_empty());
}

#[test]
fn match_column_filter_syntax() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(title, body)")
        .unwrap();
    c.execute(
        "INSERT INTO t VALUES \
         ('Hello World','the quick brown fox'),\
         ('Goodbye','the lazy dog'),\
         ('Mixed Fox','and Dog')",
    )
    .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not an integer rowid"),
            })
            .collect::<Vec<_>>()
    };
    // `col:token` in a table-wide MATCH restricts the token to that column.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'body:dog' ORDER BY rowid"),
        [2, 3]
    );
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'title:fox'"), [3]);
    assert_eq!(ids("SELECT rowid FROM t WHERE t MATCH 'title:hello'"), [1]);
    // Column-filtered terms AND together across columns.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'body:dog title:mixed'"),
        [3]
    );
}

#[test]
fn match_phrase_and_prefix_queries() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute(
        "INSERT INTO t VALUES \
         ('the quick brown fox'),('the brown quick cat'),('quick foxes run')",
    )
    .unwrap();
    let ids = |sql: &str| -> Vec<i64> {
        rows(&c, sql)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(i) => i,
                _ => panic!("not an integer rowid"),
            })
            .collect::<Vec<_>>()
    };
    // A quoted phrase requires the tokens to be adjacent and in order.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH '\"quick brown\"'"),
        [1]
    );
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH '\"brown quick\"'"),
        [2]
    );
    // A `token*` prefix matches any token starting with it.
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'fox*' ORDER BY rowid"),
        [1, 3]
    );
    assert_eq!(
        ids("SELECT rowid FROM t WHERE t MATCH 'qu*' ORDER BY rowid"),
        [1, 2, 3]
    );
}

#[test]
fn match_against_null_pattern_is_null() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE t USING fts5(body)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('hello')").unwrap();
    // A NULL query matches nothing (NULL is not true in a WHERE clause).
    assert!(rows(&c, "SELECT rowid FROM t WHERE t MATCH NULL").is_empty());
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
