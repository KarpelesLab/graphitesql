//! Differential test for the VDBE live single-table scan over a `WITHOUT ROWID`
//! table (B5b-2). Such a table used to bail from the live-scan path to the
//! materialized one; it now streams its rows from an `IndexCursor` over the
//! index-organized b-tree (primary-key order). The result — values *and* row
//! order — must match the real `sqlite3` CLI across projection / WHERE /
//! ORDER BY / LIMIT / aggregate / DISTINCT / correlated-subquery shapes.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3(setup: &[&str], query: &str) -> String {
    let mut script = String::new();
    for s in setup {
        script.push_str(s);
        script.push_str(";\n");
    }
    script.push_str(query);
    script.push(';');
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(&script)
        .output()
        .unwrap();
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
                    Value::Text(s) => String::from(s.as_str()),
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
fn without_rowid_live_scan_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }

    // A WITHOUT ROWID table with a text PK, a generated column, and rows inserted
    // out of primary-key order (so the b-tree scan order is observably different
    // from insertion order).
    let setup = [
        "CREATE TABLE wr(k TEXT PRIMARY KEY, v INTEGER, g INTEGER GENERATED ALWAYS AS (v*2)) WITHOUT ROWID",
        "INSERT INTO wr(k,v) VALUES('banana',3),('apple',1),('cherry',2),('date',2)",
    ];

    let mut g = Connection::open_memory().unwrap();
    for s in &setup {
        g.execute(s).unwrap();
    }

    let queries = [
        "SELECT k, v, g FROM wr",                       // full scan, PK order
        "SELECT v, k FROM wr WHERE v >= 2",             // projection + WHERE
        "SELECT k FROM wr ORDER BY v DESC, k",          // ORDER BY (tie on v)
        "SELECT k FROM wr LIMIT 2 OFFSET 1",            // LIMIT/OFFSET over PK order
        "SELECT count(*), sum(v), min(k), max(k) FROM wr", // aggregates
        "SELECT DISTINCT v FROM wr ORDER BY 1",         // DISTINCT
        "SELECT k || ':' || g FROM wr",                 // expression projection
        // correlated subquery over the WITHOUT ROWID scan (B5c-2 on the new source)
        "SELECT k, (SELECT count(*) FROM wr b WHERE b.v < wr.v) FROM wr",
        "SELECT k FROM wr WHERE v = (SELECT max(v) FROM wr)",
    ];

    for q in queries {
        let want = sqlite3(&setup, q);
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "WITHOUT ROWID scan diverged: {q}");
    }
}
