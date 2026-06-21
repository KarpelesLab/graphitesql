//! An unaliased result column is named after its verbatim source text, matching
//! SQLite — `SELECT a+b` yields a column literally named `a+b`, and the original
//! whitespace is preserved (`a  +  b`). A bare column reference uses the column
//! name (`t.a` → `a`); an `AS` alias always wins. Previously graphite produced
//! simplified labels like `expr` / `upper`.

#![cfg(feature = "std")]

use graphitesql::Connection;

fn names(c: &Connection, sql: &str) -> Vec<String> {
    c.query(sql).unwrap().columns
}

#[test]
fn unaliased_expression_uses_verbatim_source_span() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT, b TEXT)").unwrap();

    assert_eq!(names(&c, "SELECT a+b FROM t"), ["a+b"]);
    // Whitespace inside the expression is preserved exactly.
    assert_eq!(names(&c, "SELECT a  +  b FROM t"), ["a  +  b"]);
    assert_eq!(
        names(&c, "SELECT a*2, upper(b), 5, a||b FROM t"),
        ["a*2", "upper(b)", "5", "a||b"]
    );
    assert_eq!(
        names(&c, "SELECT CASE WHEN a>0 THEN 1 ELSE 0 END FROM t"),
        ["CASE WHEN a>0 THEN 1 ELSE 0 END"]
    );
    assert_eq!(names(&c, "SELECT count(*) FROM t"), ["count(*)"]);
}

#[test]
fn column_references_and_aliases() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT, b TEXT)").unwrap();
    // A bare / qualified column reference is named after the column, not the span.
    assert_eq!(names(&c, "SELECT a, t.b FROM t"), ["a", "b"]);
    // An AS alias (or bare-word alias) wins.
    assert_eq!(names(&c, "SELECT a+b AS s, a x FROM t"), ["s", "x"]);
    // A wildcard expands to the underlying column names.
    assert_eq!(names(&c, "SELECT * FROM t"), ["a", "b"]);
}
