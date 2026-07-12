//! Differential test for a constant-`GROUP BY` aggregate query on the VDBE. A
//! `GROUP BY <constant>` (a non-positional row-constant key such as `1+1`, `'x'`,
//! `NULL`) forms a *single* group over a non-empty table and *no* rows over an
//! empty one — unlike a bare aggregate, which always emits one row. With an
//! all-aggregate projection this now runs on the VDBE via `compile_aggregate_select`
//! with a hidden `count(*)` non-empty gate. A bare positional `GROUP BY 1` (group
//! by output column) and a non-aggregate projection still take the grouped path.
//! Every result must match the real `sqlite3` CLI.

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

fn check(setup: &[&str], queries: &[&str]) {
    let mut g = Connection::open_memory().unwrap();
    for s in setup {
        g.execute(s).unwrap();
    }
    for q in queries {
        let want = sqlite3(setup, q);
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "constant GROUP BY diverged: {q}");
    }
}

#[test]
fn constant_group_by_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }

    let non_empty = [
        "CREATE TABLE t(a INTEGER)",
        "INSERT INTO t VALUES(5),(2),(5),(7)",
    ];
    check(
        &non_empty,
        &[
            "SELECT count(*) FROM t GROUP BY 1+1",         // one group -> 4
            "SELECT sum(a), count(*) FROM t GROUP BY 'x'", // one group
            "SELECT max(a), min(a) FROM t GROUP BY NULL",  // one group
            "SELECT count(*) FROM t GROUP BY (2 * 3)",     // parenthesized const
            "SELECT count(*) FROM t GROUP BY 1+1 HAVING count(*) > 5", // filtered -> none
            "SELECT count(*) FROM t GROUP BY 1+1 HAVING count(*) = 4", // 4
            // A bare positional `GROUP BY 1` groups by output column a (NOT one group).
            "SELECT a, count(*) FROM t GROUP BY 1 ORDER BY a",
        ],
    );

    // Over an EMPTY table a constant GROUP BY yields NO rows (a bare aggregate
    // would emit one row with count 0) — the non-empty gate must suppress it.
    check(
        &["CREATE TABLE e(a INTEGER)"],
        &[
            "SELECT count(*) FROM e GROUP BY 1+1",         // no rows
            "SELECT sum(a), count(*) FROM e GROUP BY 'x'", // no rows
            "SELECT count(*) FROM e",                      // bare aggregate: one row, 0
        ],
    );
}
