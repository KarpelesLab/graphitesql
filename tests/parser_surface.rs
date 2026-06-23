//! Parser-surface coverage: SQL spellings the `sqlite3` CLI accepts that
//! graphite previously rejected at the parse layer, but which desugar to
//! already-supported AST / execution. Each closed gap is checked
//! differentially against `sqlite3`; the genuinely-deferred forms (which would
//! need executor work) are asserted to still fail to parse so the boundary is
//! explicit.

#![cfg(feature = "std")]

use graphitesql::sql::parser::parse;
use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
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

const SETUP: &str = "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT);\
    CREATE TABLE u(id INTEGER PRIMARY KEY, b INT);\
    INSERT INTO t(a) VALUES (10),(20),(30);\
    INSERT INTO u(b) VALUES (100),(200);";

/// Build a fresh graphite connection and a fresh sqlite db file with `SETUP`.
/// The path is unique per call so parallel tests do not collide.
fn both() -> (Connection, String) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("gsql-psurf-{}-{n}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let o = Command::new("sqlite3")
        .arg(&path)
        .arg(SETUP)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let mut g = Connection::open_memory().unwrap();
    for s in SETUP.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }
    (g, path)
}

fn sqlite_query(path: &str, sql: &str) -> String {
    let o = Command::new("sqlite3")
        .arg(path)
        .arg(format!("{sql};"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

/// `SELECT … FROM (…)` with redundant parentheses around a single
/// table-or-subquery: `(t)`, `((t))`, `(t alias)`, `((SELECT …))`,
/// `((SELECT …) alias)`. SQLite treats these parens as transparent.
#[test]
fn redundant_from_parens_match_sqlite() {
    if !sqlite_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let (g, path) = both();
    let queries = [
        "SELECT a FROM (t) ORDER BY a",
        "SELECT a FROM ((t)) ORDER BY a",
        "SELECT z.a FROM (t) AS z ORDER BY z.a",
        "SELECT a FROM ((t) z) ORDER BY a",
        "SELECT x FROM ((SELECT 7 AS x))",
        "SELECT y.x FROM ((SELECT 7 AS x) y)",
        "SELECT * FROM (((SELECT 42)))",
        "SELECT a FROM (t NOT INDEXED) ORDER BY a",
    ];
    for q in queries {
        let want = sqlite_query(&path, q);
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "diverged: {q}");
    }
    let _ = std::fs::remove_file(&path);
}

/// `PRAGMA [schema.]name` — a schema-qualified pragma name (the qualifier is
/// accepted and the bare name applied, as pragmas are connection-scoped).
#[test]
fn schema_qualified_pragma_match_sqlite() {
    if !sqlite_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let (g, path) = both();
    for q in [
        "PRAGMA main.user_version",
        "PRAGMA main.table_info(t)",
        "PRAGMA main.index_list(t)",
    ] {
        let want = sqlite_query(&path, q);
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "pragma diverged: {q}");
    }
    let _ = std::fs::remove_file(&path);
}

/// `WITH … INSERT … SELECT` — a CTE clause prefixing an `INSERT … SELECT`. The
/// CTE is materialized while the inserted SELECT runs.
#[test]
fn with_prefixed_insert_select_match_sqlite() {
    if !sqlite_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let (mut g, path) = both();
    let stmt =
        "WITH src(v) AS (SELECT 100 UNION ALL SELECT 200) INSERT INTO t(a) SELECT v FROM src";
    {
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg(format!("{stmt};"))
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    g.execute(stmt).unwrap();
    let q = "SELECT a FROM t ORDER BY a";
    assert_eq!(render(&g.query(q).unwrap()), sqlite_query(&path, q));

    // A RECURSIVE CTE prefixing INSERT … SELECT also parses + runs.
    let mut g2 = Connection::open_memory().unwrap();
    g2.execute("CREATE TABLE n(x INT)").unwrap();
    g2.execute(
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<5) \
         INSERT INTO n(x) SELECT x FROM c",
    )
    .unwrap();
    let got = render(&g2.query("SELECT x FROM n ORDER BY x").unwrap());
    assert_eq!(got, "1\n2\n3\n4\n5");

    let _ = std::fs::remove_file(&path);
}

/// Forms that genuinely need executor work are still rejected at parse time, so
/// they surface as a clean error rather than silently mis-executing. These are
/// the documented deferred gaps.
#[test]
fn deferred_forms_still_rejected() {
    // Parenthesized *join groups* in FROM need a nested-join AST/executor.
    assert!(parse("SELECT * FROM (t JOIN u)").is_err());
    assert!(parse("SELECT * FROM (t, u)").is_err());
    assert!(parse("SELECT * FROM (t JOIN u) AS j").is_err());
    // `WITH … INSERT … VALUES` is still rejected: a CTE may only prefix a query
    // body, an `INSERT … SELECT`, or (now) an UPDATE/DELETE — not `VALUES`.
    assert!(parse("WITH x AS (SELECT 1) INSERT INTO t(a) VALUES (1)").is_err());
}

/// `offset` and `end` are usable as bare (unqualified) column names in
/// expression position, like SQLite — they only *end* an expression (as the
/// `LIMIT … OFFSET` / `CASE … END` clause keywords) rather than being barred
/// from starting one. The genuinely-reserved clause keywords stay rejected.
#[test]
fn offset_and_end_are_usable_column_names() {
    if !sqlite_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let mut g = Connection::open_memory().unwrap();
    g.execute("CREATE TABLE k(\"offset\" INT, \"end\" INT)")
        .unwrap();
    g.execute("INSERT INTO k VALUES (3, 7), (1, 9)").unwrap();

    let path = std::env::temp_dir().join(format!("gsql-kw-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let o = Command::new("sqlite3")
        .arg(&path)
        .arg("CREATE TABLE k(\"offset\" INT, \"end\" INT); INSERT INTO k VALUES (3,7),(1,9);")
        .output()
        .unwrap();
    assert!(o.status.success());

    // Bare keyword column refs parse and evaluate identically to sqlite.
    for q in [
        "SELECT offset FROM k ORDER BY offset",
        "SELECT end FROM k ORDER BY end",
        "SELECT offset + end FROM k ORDER BY offset",
        "SELECT offset FROM k ORDER BY offset LIMIT 1 OFFSET 1",
    ] {
        let want = sqlite_query(&path, q);
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "diverged: {q}");
    }
    let _ = std::fs::remove_file(&path);

    // Genuinely-reserved clause keywords remain rejected as bare column names.
    for q in [
        "SELECT order FROM k",
        "SELECT limit FROM k",
        "SELECT table FROM k",
        "SELECT where FROM k",
        "SELECT from FROM k",
    ] {
        assert!(parse(q).is_err(), "should still reject: {q}");
    }
}

/// Sanity: the redundant-paren handling does not change how a real subquery or
/// a row-value expression parses (no regression in the overloaded `(`).
#[test]
fn paren_handling_no_regression() {
    // Derived table with alias still works.
    assert!(parse("SELECT * FROM (SELECT 1) x").is_ok());
    // Row-value expression in a WHERE is unaffected.
    assert!(parse("SELECT * FROM t WHERE (id, a) = (1, 10)").is_ok());
    // Plain table reference unaffected.
    assert!(parse("SELECT * FROM t").is_ok());
}
