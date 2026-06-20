//! Track A: collations (`BINARY`/`NOCASE`/`RTRIM`) in comparisons, `ORDER BY`,
//! `GROUP BY`, `DISTINCT`, and indexes. Verified differentially against sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn rows_str(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn nocase_comparison_and_order() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT COLLATE NOCASE)")
        .unwrap();
    c.execute("INSERT INTO t(n) VALUES ('Bob'),('alice'),('CAROL'),('bob')")
        .unwrap();
    // Column collation makes `=` case-insensitive.
    assert_eq!(
        c.query("SELECT count(*) FROM t WHERE n = 'BOB'")
            .unwrap()
            .rows[0][0],
        Value::Integer(2)
    );
    // ORDER BY uses the column's NOCASE collation.
    assert_eq!(
        rows_str(&c, "SELECT n FROM t ORDER BY n, id"),
        "alice\nBob\nbob\nCAROL"
    );
    // Explicit COLLATE on a binary column.
    let mut c2 = Connection::open_memory().unwrap();
    c2.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT)")
        .unwrap();
    c2.execute("INSERT INTO t(n) VALUES ('Bob'),('alice')")
        .unwrap();
    assert_eq!(
        c2.query("SELECT count(*) FROM t WHERE n = 'bob' COLLATE NOCASE")
            .unwrap()
            .rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        rows_str(&c2, "SELECT n FROM t ORDER BY n COLLATE NOCASE"),
        "alice\nBob"
    );
}

#[test]
fn rtrim_collation() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(n TEXT)").unwrap();
    c.execute("INSERT INTO t VALUES ('hi'),('hi   '),('ho')")
        .unwrap();
    // RTRIM ignores trailing spaces.
    assert_eq!(
        c.query("SELECT count(*) FROM t WHERE n = 'hi' COLLATE RTRIM")
            .unwrap()
            .rows[0][0],
        Value::Integer(2)
    );
}

#[test]
fn distinct_and_group_under_nocase() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(n TEXT COLLATE NOCASE, v INT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('A',1),('a',2),('B',3)")
        .unwrap();
    // DISTINCT collapses 'A'/'a' under NOCASE.
    assert_eq!(
        c.query("SELECT count(DISTINCT n) FROM t").unwrap().rows[0][0],
        Value::Integer(2)
    );
}

#[test]
fn nocase_index_integrity_and_lookup() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-collidx-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        // NOCASE via the column, plus an explicit-COLLATE index, plus a UNIQUE
        // NOCASE column (whose auto-index must order NOCASE).
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, n TEXT COLLATE NOCASE, e TEXT UNIQUE COLLATE NOCASE)")
            .unwrap();
        c.execute("CREATE INDEX iexp ON t(n COLLATE NOCASE)")
            .unwrap();
        let names = ["Apple", "banana", "CHERRY", "apple2", "Banana2"];
        for (i, w) in names.iter().enumerate() {
            c.execute(&format!("INSERT INTO t(n,e) VALUES ('{w}', 'e{i}')"))
                .unwrap();
        }
        // A duplicate UNIQUE NOCASE value is rejected ('APPLE' vs 'Apple'... use e).
        assert!(c.execute("INSERT INTO t(n,e) VALUES ('x','E0')").is_err());
    }
    // The NOCASE indexes must be consistent with the table per real sqlite3.
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check;")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
    // Index-driven NOCASE equality lookup finds the case-variant row.
    let c = Connection::open_readonly(&path).unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t WHERE n = 'APPLE'")
            .unwrap()
            .rows[0][0],
        Value::Integer(1)
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn collation_against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let stmts = [
        "CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT COLLATE NOCASE, b TEXT)",
        "INSERT INTO t(a,b) VALUES ('Apple','Apple'),('apple','apple'),('BANANA','BANANA'),('banana','banana'),('cherry','cherry')",
    ];
    let queries = [
        "SELECT a FROM t ORDER BY a, id",
        "SELECT b FROM t ORDER BY b, id",
        "SELECT b FROM t ORDER BY b COLLATE NOCASE, id",
        "SELECT count(*) FROM t WHERE a = 'APPLE'",
        "SELECT count(*) FROM t WHERE b = 'APPLE'",
        "SELECT count(*) FROM t WHERE b = 'APPLE' COLLATE NOCASE",
        "SELECT count(DISTINCT a) FROM t",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY a",
        "SELECT id FROM t WHERE a < 'b' ORDER BY id",
    ];

    let path = std::env::temp_dir().join(format!("gsql-coll-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(stmts.join(";"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut g = Connection::open_memory().unwrap();
    for s in stmts {
        g.execute(s).unwrap();
    }

    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = rows_str(&g, q);
        if got != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    let _ = std::fs::remove_file(&path);
    assert!(
        failures.is_empty(),
        "{} collation queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn in_operator_uses_left_collation() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT COLLATE NOCASE, b TEXT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES('ABC','xyz'),('abc','XYZ'),('Abc','def')")
        .unwrap();
    // `a IN ('ABC')` matches case-insensitively via column a's NOCASE collation.
    assert_eq!(
        rows_str(&c, "SELECT a FROM t WHERE a IN ('ABC') ORDER BY b"),
        "abc\nAbc\nABC"
    );
    // A BINARY column matches only the exact case.
    assert_eq!(rows_str(&c, "SELECT b FROM t WHERE b IN ('xyz')"), "xyz");
    // An explicit COLLATE on the left applies too.
    assert_eq!(
        rows_str(
            &c,
            "SELECT b FROM t WHERE b COLLATE NOCASE IN ('xyz') ORDER BY a"
        ),
        "xyz\nXYZ"
    );
}

#[test]
fn min_max_use_argument_collation() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT COLLATE NOCASE, b TEXT)")
        .unwrap();
    c.execute("INSERT INTO t VALUES('ABC','xyz'),('abc','XYZ'),('Abc','def')")
        .unwrap();
    // Under NOCASE all three a-values are equal, so min and max both return the
    // first ('ABC'), matching SQLite.
    assert_eq!(rows_str(&c, "SELECT max(a), min(a) FROM t"), "ABC|ABC");
    // A BINARY column compares by code point.
    assert_eq!(rows_str(&c, "SELECT max(b), min(b) FROM t"), "xyz|XYZ");
}

#[test]
fn between_and_case_use_collation() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT COLLATE NOCASE)").unwrap();
    c.execute("INSERT INTO t VALUES('Apple'),('BANANA'),('cherry')")
        .unwrap();
    // BETWEEN compares under the column's NOCASE collation.
    assert_eq!(
        rows_str(&c, "SELECT a FROM t WHERE a BETWEEN 'a' AND 'c' ORDER BY a"),
        "Apple\nBANANA"
    );
    // CASE x WHEN y compares under x's collation.
    assert_eq!(
        rows_str(
            &c,
            "SELECT CASE a WHEN 'apple' THEN 'fruit' ELSE 'other' END FROM t WHERE a='Apple'"
        ),
        "fruit"
    );
    // A plain (BINARY) operand is unaffected.
    assert_eq!(
        rows_str(&c, "SELECT CASE 'x' WHEN 'X' THEN 1 ELSE 0 END"),
        "0"
    );
}
