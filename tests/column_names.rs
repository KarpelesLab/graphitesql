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

#[test]
fn table_wildcard_over_join_names_only_that_table() {
    // `t.*` over a join must name (and project) ONLY that table's columns, not
    // every column of the join — a bare `*` lists all. (Regression: the column
    // names previously listed all join columns while the data had only `t`'s.)
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, g TEXT)")
        .unwrap();
    c.execute("CREATE TABLE u(t_id INT, w INT)").unwrap();
    c.execute("INSERT INTO t(g) VALUES('a'),('b')").unwrap();
    c.execute("INSERT INTO u VALUES(1,10),(2,20)").unwrap();
    for &use_vdbe in &[true, false] {
        c.set_use_vdbe(use_vdbe);
        let r = c
            .query("SELECT t.* FROM t JOIN u ON u.t_id = t.id")
            .unwrap();
        assert_eq!(r.columns, ["id", "g"], "use_vdbe={use_vdbe}");
        assert!(
            r.rows.iter().all(|row| row.len() == 2),
            "use_vdbe={use_vdbe}"
        );
        // `u.*` likewise; a bare `*` lists everything.
        assert_eq!(
            c.query("SELECT u.* FROM t JOIN u ON u.t_id = t.id")
                .unwrap()
                .columns,
            ["t_id", "w"]
        );
        assert_eq!(
            c.query("SELECT * FROM t JOIN u ON u.t_id = t.id")
                .unwrap()
                .columns,
            ["id", "g", "t_id", "w"]
        );
    }
    c.set_use_vdbe(true);
}

#[test]
fn window_function_column_uses_source_text() {
    // A window-function output column is named after its verbatim source text
    // (matching sqlite), not the internal `__winN` rewrite placeholder. An alias
    // still wins.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT)")
        .unwrap();
    assert_eq!(
        names(&c, "SELECT sum(a) OVER () FROM t"),
        ["sum(a) OVER ()"]
    );
    assert_eq!(
        names(&c, "SELECT row_number() OVER (ORDER BY a) FROM t"),
        ["row_number() OVER (ORDER BY a)"]
    );
    assert_eq!(
        names(&c, "SELECT a, rank() OVER (ORDER BY a) FROM t"),
        ["a", "rank() OVER (ORDER BY a)"]
    );
    assert_eq!(names(&c, "SELECT sum(a) OVER () AS s FROM t"), ["s"]);
    assert_eq!(
        names(&c, "SELECT lag(a) OVER (ORDER BY a), a FROM t"),
        ["lag(a) OVER (ORDER BY a)", "a"]
    );
}
