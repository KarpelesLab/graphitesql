//! Track A: the `VALUES` query — as a standalone statement and a table source.
//! Verified against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn rows_str(c: &Connection, sql: &str) -> String {
    use graphitesql::Value;
    c.query(sql)
        .unwrap()
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

#[test]
fn standalone_and_columns() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(rows_str(&c, "VALUES (1,2),(3,4)"), "1|2\n3|4");
    // SQLite names the columns column1, column2, …
    let r = c.query("VALUES (1,2,3)").unwrap();
    assert_eq!(r.columns, vec!["column1", "column2", "column3"]);
    // Duplicates are preserved (UNION ALL semantics).
    assert_eq!(rows_str(&c, "VALUES (1),(1),(2)"), "1\n1\n2");
}

#[test]
fn as_table_source() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        rows_str(
            &c,
            "SELECT column1 FROM (VALUES (10,'a'),(20,'b')) ORDER BY column1 DESC"
        ),
        "20\n10"
    );
    assert_eq!(
        rows_str(&c, "SELECT sum(column1) FROM (VALUES (1),(2),(3))"),
        "6"
    );
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let queries = [
        "VALUES (1,2),(3,4),(5,6)",
        "SELECT * FROM (VALUES (1),(2),(3)) WHERE column1 > 1",
        "SELECT column2 FROM (VALUES (1,'x'),(2,'y')) ORDER BY column1",
        "SELECT count(*) FROM (VALUES (1),(2),(3),(4))",
        "VALUES ('a', 1.5), ('b', 2.5)",
    ];
    let c = Connection::open_memory().unwrap();
    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(q)
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = rows_str(&c, q);
        if got != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} VALUES queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn multi_row_values_as_compound_operand() {
    // A multi-row VALUES on the *right* of a compound operator must contribute
    // all its rows (regression: only the first row was kept).
    let c = Connection::open_memory().unwrap();
    assert_eq!(rows_str(&c, "VALUES(1),(2) UNION VALUES(2),(3)"), "1\n2\n3");
    assert_eq!(
        rows_str(&c, "VALUES(1),(2) UNION ALL VALUES(3),(4)"),
        "1\n2\n3\n4"
    );
    assert_eq!(rows_str(&c, "SELECT 1 UNION VALUES(2),(3)"), "1\n2\n3");
    assert_eq!(
        rows_str(&c, "VALUES(1),(2),(3) INTERSECT VALUES(2),(3)"),
        "2\n3"
    );
    assert_eq!(rows_str(&c, "VALUES(1),(2),(3) EXCEPT VALUES(2)"), "1\n3");
    // Chained compounds keep left-associative semantics.
    assert_eq!(
        rows_str(&c, "SELECT 1 UNION SELECT 2 UNION ALL SELECT 2"),
        "1\n2\n2"
    );
}
