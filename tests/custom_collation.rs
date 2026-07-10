//! User-registered custom collating sequences (`Connection::register_collation`,
//! the equivalent of `sqlite3_create_collation`).
//!
//! The `sqlite3` CLI oracle cannot register a custom collation, so the
//! differential check here is indirect: a custom collation *defined to equal*
//! `NOCASE` must produce exactly the ordering SQLite's built-in `NOCASE` does.
//! Everything the engine writes must still pass `PRAGMA integrity_check`.

#![cfg(feature = "std")]

use core::cmp::Ordering;
use graphitesql::Connection;

fn rows(conn: &Connection, sql: &str) -> Vec<String> {
    conn.query(sql)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match &r[0] {
            graphitesql::Value::Text(s) => s.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn order_by_custom_collation() {
    let mut conn = Connection::open_memory().unwrap();
    // Reverse of BINARY.
    conn.register_collation("rev", |x: &str, y: &str| x.cmp(y).reverse());

    conn.execute("CREATE TABLE t(s TEXT)").unwrap();
    conn.execute("INSERT INTO t VALUES ('apple'),('banana'),('cherry')")
        .unwrap();

    let got = rows(&conn, "SELECT s FROM t ORDER BY s COLLATE rev");
    assert_eq!(got, ["cherry", "banana", "apple"]);
}

#[test]
fn custom_collation_equals_builtin_nocase() {
    let mut conn = Connection::open_memory().unwrap();
    conn.register_collation("mynocase", |x: &str, y: &str| {
        x.to_ascii_uppercase().cmp(&y.to_ascii_uppercase())
    });
    conn.execute("CREATE TABLE t(s TEXT)").unwrap();
    conn.execute("INSERT INTO t VALUES ('Banana'),('apple'),('Cherry')")
        .unwrap();

    let custom = rows(&conn, "SELECT s FROM t ORDER BY s COLLATE mynocase");
    let builtin = rows(&conn, "SELECT s FROM t ORDER BY s COLLATE nocase");
    assert_eq!(custom, builtin);
}

#[test]
fn custom_collation_in_column_and_unique_index() {
    let mut conn = Connection::open_memory().unwrap();
    conn.register_collation("ci", |x: &str, y: &str| {
        x.to_ascii_uppercase().cmp(&y.to_ascii_uppercase())
    });
    conn.execute("CREATE TABLE t(k TEXT COLLATE ci)").unwrap();
    conn.execute("CREATE UNIQUE INDEX t_k ON t(k)").unwrap();
    conn.execute("INSERT INTO t VALUES ('Hello')").unwrap();

    // A case-variant duplicate must violate the custom-collation UNIQUE index.
    assert!(conn.execute("INSERT INTO t VALUES ('HELLO')").is_err());
    // A genuinely distinct key inserts fine.
    conn.execute("INSERT INTO t VALUES ('World')").unwrap();

    // Equality under the column collation is case-insensitive.
    let n = conn
        .query("SELECT count(*) FROM t WHERE k = 'hello'")
        .unwrap();
    assert_eq!(n.rows[0][0], graphitesql::Value::Integer(1));

    // Anything the engine wrote (incl. the custom-collation index) is well-formed.
    let ic = conn.query("PRAGMA integrity_check").unwrap();
    assert_eq!(ic.rows[0][0], graphitesql::Value::Text("ok".into()));
}

#[test]
fn unknown_collation_still_errors() {
    let conn = Connection::open_memory().unwrap();
    let e = conn
        .query("SELECT 1 ORDER BY 1 COLLATE definitely_not_registered")
        .unwrap_err();
    assert!(
        e.to_string().contains("no such collation sequence"),
        "unexpected error: {e}"
    );
}

#[test]
fn reregistration_replaces() {
    let mut conn = Connection::open_memory().unwrap();
    conn.register_collation("swap", |x: &str, y: &str| x.cmp(y));
    conn.register_collation("swap", |x: &str, y: &str| x.cmp(y).reverse());
    conn.execute("CREATE TABLE t(s TEXT)").unwrap();
    conn.execute("INSERT INTO t VALUES ('a'),('b')").unwrap();
    let got = rows(&conn, "SELECT s FROM t ORDER BY s COLLATE swap");
    assert_eq!(got, ["b", "a"]);
}

// Keep the `Ordering` import meaningful even if closures elide it.
const _: fn(&str, &str) -> Ordering = |a, b| a.cmp(b);
