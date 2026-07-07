//! Differential testing of the FTS5 braced column-set MATCH filter
//! `{c0 c1 …}:phrase` (and its negated `-{…}:` / `-col:` forms), matching
//! sqlite3 3.50.4. `{c0 c1}:` restricts the following phrase/sub-expression to the
//! listed columns (a set); `{a}:` is exactly the single-column `a:` form. `-{…}:`
//! restricts to the COMPLEMENT of the set. An unknown column name in a filter is a
//! query error, as in sqlite. Filters compose with `AND`/`OR`/`NOT`/`NEAR`/prefix
//! `*` and bind to a parenthesised sub-expression. Checked against sqlite3.

#![cfg(all(feature = "std", feature = "fts5"))]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3(path: &str, sql: &str) -> String {
    let o = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

fn render(result: &graphitesql::QueryResult) -> String {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    Value::Null => String::new(),
                    Value::Integer(i) => i.to_string(),
                    Value::Text(s) => s.clone(),
                    Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                    Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const SETUP: &str = "CREATE VIRTUAL TABLE ft USING fts5(a,b,c);\
    INSERT INTO ft VALUES('quick fox','lazy dog','red car'),\
                         ('slow fox','quick dog','blue car'),\
                         ('big fox','small dog','quick car');";

#[test]
fn braced_column_filter_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-fts5colfilter-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    sqlite3(&path, SETUP);
    let mut g = Connection::open_memory().unwrap();
    for s in SETUP.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let queries = [
        // Single-column braced form equals the bare `col:` form.
        "SELECT rowid FROM ft WHERE ft MATCH '{a}:quick' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH 'a:quick' ORDER BY rowid",
        // Multi-column sets (an OR over the listed columns).
        "SELECT rowid FROM ft WHERE ft MATCH '{a b}:quick' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '{a c}:quick' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '{b c}:quick' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '{a b c}:quick' ORDER BY rowid",
        // Negated column set: the COMPLEMENT of the listed columns.
        "SELECT rowid FROM ft WHERE ft MATCH '-{a}:quick' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '-{a b}:quick' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '-{b c}:quick' ORDER BY rowid",
        // Negated single-column form.
        "SELECT rowid FROM ft WHERE ft MATCH '-a:quick' ORDER BY rowid",
        // Case-insensitive column names.
        "SELECT rowid FROM ft WHERE ft MATCH '{A B}:quick' ORDER BY rowid",
        // Whitespace around the braces and colon.
        "SELECT rowid FROM ft WHERE ft MATCH '{a  b} : quick' ORDER BY rowid",
        // Composition with OR / AND / NOT.
        "SELECT rowid FROM ft WHERE ft MATCH '{a b}:quick OR {c}:car' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '{a}:quick AND {c}:red' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '{a b c}:fox NOT {a}:quick' ORDER BY rowid",
        // A braced filter over a parenthesised sub-expression (pushed onto every
        // term within it).
        "SELECT rowid FROM ft WHERE ft MATCH '{a b}:(quick OR fox)' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '-{a}:(quick OR car)' ORDER BY rowid",
        // Nested filters intersect (the inner and outer must both admit a column).
        "SELECT rowid FROM ft WHERE ft MATCH '{a b}:(c:quick)' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '{a b c}:(a:quick)' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '-{a}:(b:quick)' ORDER BY rowid",
        // Prefix `*` under a braced filter.
        "SELECT rowid FROM ft WHERE ft MATCH '{a}:qui*' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '{a b}:qui*' ORDER BY rowid",
        // A braced filter over a NEAR group.
        "SELECT rowid FROM ft WHERE ft MATCH '{a}:NEAR(quick fox)' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH '{b}:NEAR(quick fox)' ORDER BY rowid",
        // The existing bare `col:` filters (must stay green).
        "SELECT rowid FROM ft WHERE ft MATCH 'a:quick OR c:quick' ORDER BY rowid",
        "SELECT rowid FROM ft WHERE ft MATCH 'b:dog' ORDER BY rowid",
    ];
    for q in queries {
        let want = sqlite3(&path, q);
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "mismatch for `{q}`");
    }
    let _ = std::fs::remove_file(&path);
}

/// An unknown column name inside a filter is a query error in both engines
/// (`no such column: NAME` at the library level; the sqlite3 CLI prefixes it with
/// `stepping,`). Assert both error and that graphite names the missing column.
#[test]
fn unknown_column_in_filter_errors() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-fts5colerr-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    sqlite3(&path, SETUP);
    let mut g = Connection::open_memory().unwrap();
    for s in SETUP.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    // (query, the column name graphite should report as missing)
    let cases = [
        ("SELECT rowid FROM ft WHERE ft MATCH '{z}:quick'", "z"),
        ("SELECT rowid FROM ft WHERE ft MATCH '{a z}:quick'", "z"),
        ("SELECT rowid FROM ft WHERE ft MATCH '-{z}:quick'", "z"),
        ("SELECT rowid FROM ft WHERE ft MATCH 'z:quick'", "z"),
    ];
    for (q, missing) in cases {
        // sqlite3 errors (its stderr carries `no such column`), producing no rows.
        let sqlite_out = {
            let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
            let err = String::from_utf8_lossy(&o.stderr);
            assert!(
                err.contains(&format!("no such column: {missing}")),
                "sqlite3 should report the missing column for `{q}`, got stderr: {err}"
            );
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        assert_eq!(sqlite_out, "", "sqlite3 should yield no rows for `{q}`");
        // graphite errors with the same library-level message.
        let err = g.query(q).expect_err(&format!("`{q}` should error"));
        assert!(
            err.to_string()
                .contains(&format!("no such column: {missing}")),
            "graphite error message for `{q}`: {err}"
        );
    }
    let _ = std::fs::remove_file(&path);
}

/// The unknown-column filter error fires even when the table is empty (sqlite
/// reports it at cursor-filter time, before any row is stepped), so graphite must
/// too — a `MATCH` filter naming a bad column is not silently empty.
#[test]
fn unknown_column_errors_on_empty_table() {
    let mut g = Connection::open_memory().unwrap();
    g.execute("CREATE VIRTUAL TABLE ft USING fts5(a,b,c)")
        .unwrap();
    let err = g
        .query("SELECT rowid FROM ft WHERE ft MATCH '{z}:quick'")
        .expect_err("should error even with no rows");
    assert!(
        err.to_string().contains("no such column: z"),
        "expected a no-such-column error, got: {err}"
    );
}

/// A chained/nested brace (`{a}:{b}:x`, `a:{b}:x`) and a braced set with no colon
/// (`{a b} quick`) or empty braces (`{}:x`) are syntax errors in sqlite; graphite
/// rejects them the same way (`fts5: syntax error …`).
#[test]
fn malformed_brace_is_a_syntax_error() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-fts5colsyn-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    sqlite3(&path, SETUP);
    let mut g = Connection::open_memory().unwrap();
    for s in SETUP.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let cases = [
        "SELECT rowid FROM ft WHERE ft MATCH '{a}:{b}:quick'",
        "SELECT rowid FROM ft WHERE ft MATCH 'a:{b}:quick'",
        "SELECT rowid FROM ft WHERE ft MATCH '{a b} quick'",
        "SELECT rowid FROM ft WHERE ft MATCH '{}:quick'",
    ];
    for q in cases {
        let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            err.contains("fts5: syntax error"),
            "sqlite3 should report a syntax error for `{q}`, got stderr: {err}"
        );
        let g_err = g.query(q).expect_err(&format!("`{q}` should error"));
        assert!(
            g_err.to_string().contains("fts5: syntax error"),
            "graphite should report a syntax error for `{q}`, got: {g_err}"
        );
    }
    let _ = std::fs::remove_file(&path);
}
