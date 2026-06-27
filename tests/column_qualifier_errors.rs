//! A **three-part `schema.table.column`** reference is validated the way SQLite
//! validates it: the `schema.` qualifier must name the database the matched FROM
//! source actually lives in, otherwise the whole reference is `no such column:
//! schema.table.column` — *even when the named database exists elsewhere*. A
//! correct qualifier (`main.t.a` for a `main` table) resolves like the bare
//! `t.a`. Matched to the `sqlite3` CLI (3.50.4).
//!
//! This is statically enforced (at prepare time) for the common single-table /
//! simple-`ON`-join shape, so the error fires even when the table is empty or
//! every row is filtered out — and regardless of whether the VDBE fast path or
//! the tree-walker would run the query. A *correlated subquery body* that binds
//! its qualified reference to an enclosing FROM is still resolved lazily (the
//! documented nested-scope residual) and is intentionally not covered here.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

/// The library-level error for a SELECT (no CLI framing), `error: ` stripped.
fn err(setup: &[&str], sql: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for s in setup {
        c.execute(s).unwrap();
    }
    let msg = c.query(sql).unwrap_err().to_string();
    msg.strip_prefix("error: ").unwrap_or(&msg).to_string()
}

#[test]
fn wrong_schema_qualifier_on_a_column_is_no_such_column() {
    // `bad` names no database at all; `aux`/`temp` name no in-scope database
    // here. Each keeps the full three-part name in the message.
    for q in ["bad", "aux", "temp"] {
        assert_eq!(
            err(&["CREATE TABLE t(a,b)"], &alloc_sql(q)),
            format!("no such column: {q}.t.a"),
            "qualifier {q}"
        );
    }
}

/// `SELECT <q>.t.a FROM t` — a three-part column whose qualifier is `q`.
fn alloc_sql(q: &str) -> String {
    format!("SELECT {q}.t.a FROM t")
}

#[test]
fn wrong_qualifier_fires_in_every_clause() {
    // Projection, WHERE, ORDER BY and GROUP BY are all validated statically.
    let setup = &["CREATE TABLE t(a,b)"];
    assert_eq!(
        err(setup, "SELECT * FROM t WHERE bad.t.a = 1"),
        "no such column: bad.t.a"
    );
    assert_eq!(
        err(setup, "SELECT a FROM t ORDER BY bad.t.a"),
        "no such column: bad.t.a"
    );
    assert_eq!(
        err(setup, "SELECT a FROM t GROUP BY bad.t.a"),
        "no such column: bad.t.a"
    );
    assert_eq!(
        err(setup, "SELECT count(*) FROM t WHERE bad.t.a > 0"),
        "no such column: bad.t.a"
    );
}

#[test]
fn wrong_qualifier_fires_even_when_no_row_is_produced() {
    // Empty table: the error is a prepare-time check, not a per-row one.
    assert_eq!(
        err(&["CREATE TABLE t(a,b)"], "SELECT bad.t.a FROM t"),
        "no such column: bad.t.a"
    );
    // Every row filtered out: same.
    assert_eq!(
        err(
            &["CREATE TABLE t(a,b)", "INSERT INTO t VALUES(1,2)"],
            "SELECT bad.t.a FROM t WHERE a = 999"
        ),
        "no such column: bad.t.a"
    );
}

#[test]
fn an_alias_hides_the_underlying_table_name_in_the_qualifier() {
    // `FROM t AS x` exposes the source as `x`; a `t`-qualified column no longer
    // matches any source, so even a correct *database* part cannot save it.
    assert_eq!(
        err(&["CREATE TABLE t(a,b)"], "SELECT main.t.a FROM t AS x"),
        "no such column: main.t.a"
    );
}

#[test]
fn correct_schema_qualifier_resolves_like_the_bare_column() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a,b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,2)").unwrap();
    assert_eq!(
        c.query("SELECT main.t.a, main.t.b FROM t").unwrap().rows[0],
        vec![Value::Integer(1), Value::Integer(2)]
    );
    // And under a WHERE that also uses the three-part form.
    assert_eq!(
        c.query("SELECT main.t.a FROM t WHERE main.t.a = 1")
            .unwrap()
            .rows[0][0],
        Value::Integer(1)
    );
}

#[test]
fn temp_qualifier_resolves_against_a_temp_source() {
    // A genuinely-temp table accepts a `temp.` column qualifier and rejects a
    // `main.` one (the source lives in `temp`, shadowing main).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TEMP TABLE t(a,b)").unwrap();
    c.execute("INSERT INTO t VALUES(5,6)").unwrap();
    assert_eq!(
        c.query("SELECT temp.t.a FROM t").unwrap().rows[0][0],
        Value::Integer(5)
    );
    let msg = c.query("SELECT main.t.a FROM t").unwrap_err().to_string();
    assert_eq!(
        msg.strip_prefix("error: ").unwrap_or(&msg),
        "no such column: main.t.a"
    );
}
