//! Phase 9: window functions (`f(…) OVER (PARTITION BY … ORDER BY …)`).
//!
//! Verified differentially against the real `sqlite3` CLI where available.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        // Use graphitesql's own canonical real formatting (matches sqlite's %.15g).
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn render_rows(r: &graphitesql::QueryResult) -> String {
    r.rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn row_number_basic() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, g INT, v INT)")
        .unwrap();
    for (g, v) in [(1, 10), (1, 30), (1, 20), (2, 5), (2, 15)] {
        c.execute(&format!("INSERT INTO t(g,v) VALUES ({g},{v})"))
            .unwrap();
    }
    let r = c
        .query("SELECT v, row_number() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY g, v")
        .unwrap();
    let got: Vec<(i64, i64)> = r
        .rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Integer(a), Value::Integer(b)) => (*a, *b),
            _ => panic!("{row:?}"),
        })
        .collect();
    assert_eq!(got, vec![(10, 1), (20, 2), (30, 3), (5, 1), (15, 2)]);
}

#[test]
fn running_sum() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    for v in [1, 2, 3, 4] {
        c.execute(&format!("INSERT INTO t(v) VALUES ({v})"))
            .unwrap();
    }
    let r = c
        .query("SELECT v, sum(v) OVER (ORDER BY v) FROM t ORDER BY v")
        .unwrap();
    let got: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match &row[1] {
            Value::Integer(b) => *b,
            o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(got, vec![1, 3, 6, 10]); // running sum
}

/// The big differential battery against sqlite3.
#[test]
fn window_against_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, g INT, v INT, s TEXT);";
    let mut inserts = String::new();
    // Deterministic data with partitions, ties, and some NULLs.
    let data = [
        (1, 10, "a"),
        (1, 30, "b"),
        (1, 30, "c"),
        (1, 20, "d"),
        (2, 5, "e"),
        (2, 15, "f"),
        (2, 15, "g"),
        (3, 7, "h"),
        (3, 7, "i"),
        (3, 1, "j"),
    ];
    for (i, (g, v, s)) in data.iter().enumerate() {
        inserts += &format!("INSERT INTO t(id,g,v,s) VALUES ({},{g},{v},'{s}');", i + 1);
    }

    let path = std::env::temp_dir().join(format!("gsql-win-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(format!("{setup}{inserts}"))
        .output()
        .unwrap();
    assert!(out.status.success());

    let mut g = Connection::open_memory().unwrap();
    g.execute(setup.trim_end_matches(';')).unwrap();
    for s in inserts.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let queries = [
        "SELECT id, row_number() OVER (ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, row_number() OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, rank() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
        "SELECT id, dense_rank() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY id",
        "SELECT id, sum(v) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, sum(v) OVER (PARTITION BY g) FROM t ORDER BY id",
        "SELECT id, count(*) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, avg(v) OVER (PARTITION BY g) FROM t ORDER BY id",
        "SELECT id, min(v) OVER (PARTITION BY g), max(v) OVER (PARTITION BY g) FROM t ORDER BY id",
        "SELECT id, lag(v) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, lead(v) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, lag(v, 2, -1) OVER (ORDER BY id) FROM t ORDER BY id",
        "SELECT id, first_value(v) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, last_value(v) OVER (PARTITION BY g ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, nth_value(v, 2) OVER (PARTITION BY g ORDER BY id) FROM t ORDER BY id",
        "SELECT id, ntile(3) OVER (ORDER BY id) FROM t ORDER BY id",
        "SELECT id, ntile(4) OVER (ORDER BY v, id) FROM t ORDER BY id",
        "SELECT id, row_number() OVER (ORDER BY v DESC, id) FROM t ORDER BY id",
        "SELECT id, sum(v) OVER (ORDER BY v, id) + 100 FROM t ORDER BY id",
        "SELECT g, v, rank() OVER (PARTITION BY g ORDER BY v) FROM t ORDER BY g, v, id",
        // Explicit ROWS frames.
        "SELECT id, sum(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        "SELECT id, sum(v) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, avg(v) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, sum(v) OVER (PARTITION BY g ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
        "SELECT id, last_value(v) OVER (ORDER BY id ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM t ORDER BY id",
        "SELECT id, first_value(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, max(v) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM t ORDER BY id",
        "SELECT id, count(*) OVER (ORDER BY id ROWS 2 PRECEDING) FROM t ORDER BY id",
        // RANGE / GROUPS over the ordering key (peer-based).
        "SELECT id, sum(v) OVER (ORDER BY v RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
        "SELECT id, sum(v) OVER (ORDER BY v GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id",
    ];

    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        match g.query(q) {
            Ok(r) => {
                let got = render_rows(&r);
                if got != want {
                    failures.push(format!(
                        "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
                    ));
                }
            }
            Err(e) => failures.push(format!("  {q}\n    graphite error: {e}")),
        }
    }
    let _ = std::fs::remove_file(&path);
    assert!(
        failures.is_empty(),
        "{} window queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn window_over_aggregate() {
    // SQLite applies window functions AFTER GROUP BY: a window operates on the
    // grouped rows, and an aggregate inside a window argument/spec is the group's
    // aggregate. Verified against the tree-walker invariants and sqlite3.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(g TEXT, v INT)").unwrap();
    c.execute("INSERT INTO t VALUES ('a',1),('a',2),('b',3),('b',4),('c',10)")
        .unwrap();
    let cases = [
        "SELECT g, sum(v), sum(sum(v)) OVER () FROM t GROUP BY g ORDER BY g",
        "SELECT g, sum(v), avg(sum(v)) OVER () FROM t GROUP BY g ORDER BY g",
        "SELECT g, count(*) OVER () FROM t GROUP BY g ORDER BY g",
        "SELECT g, sum(v), row_number() OVER (ORDER BY sum(v) DESC) FROM t GROUP BY g ORDER BY g",
        "SELECT g, sum(v), rank() OVER (ORDER BY sum(v)) FROM t GROUP BY g ORDER BY sum(v)",
        "SELECT g, sum(v) - sum(sum(v)) OVER () AS d FROM t GROUP BY g ORDER BY g",
        "SELECT g, sum(v), sum(sum(v)) OVER (ORDER BY g) AS running FROM t GROUP BY g ORDER BY g",
        "SELECT g, max(v), max(v) - min(min(v)) OVER () FROM t GROUP BY g ORDER BY g",
    ];
    if std::process::Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    for q in cases {
        let want = {
            let o = std::process::Command::new("sqlite3")
                .arg(":memory:")
                .arg(format!(
                    "CREATE TABLE t(g TEXT, v INT); \
                     INSERT INTO t VALUES ('a',1),('a',2),('b',3),('b',4),('c',10); {q};"
                ))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = c
            .query(q)
            .unwrap()
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| match v {
                        Value::Null => String::new(),
                        Value::Integer(i) => i.to_string(),
                        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                        Value::Text(s) => s.clone(),
                        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(got, want, "window-over-aggregate diverged on {q}");
    }
}
