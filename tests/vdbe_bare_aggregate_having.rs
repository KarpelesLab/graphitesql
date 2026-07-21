//! Differential test for `HAVING` on a non-grouped aggregate query on the VDBE.
//! A bare-aggregate `SELECT` with a `HAVING` used to bail to the tree-walker;
//! the aggregate compiler now filters its single output row against the HAVING
//! predicate. An aggregate appearing in the projection binds to its finalized
//! slot; one appearing ONLY in HAVING is folded into an extra slot and bound
//! too, so `SELECT sum(a) FROM t HAVING count(*) > 1` also runs on the VDBE. A
//! HAVING term that is not a foldable aggregate over this scan (e.g. a bare
//! column) still defers, which the tree-walker evaluates. Either way the result
//! must match the real `sqlite3` CLI.

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
        assert_eq!(got, want, "bare-aggregate HAVING diverged: {q}");
    }
}

#[test]
fn bare_aggregate_having_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }

    let setup = [
        "CREATE TABLE t(a INTEGER, b TEXT)",
        "INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'z')",
    ];
    check(
        &setup,
        &[
            "SELECT count(*) FROM t HAVING count(*) > 5", // filtered out -> no row
            "SELECT count(*) FROM t HAVING count(*) > 2", // 3
            "SELECT sum(a) FROM t HAVING sum(a) >= 6",    // 6
            "SELECT max(a) FROM t HAVING max(a) > 2",     // 3
            "SELECT count(*), sum(a) FROM t HAVING count(*) >= 3", // multi-agg projection
            "SELECT count(*) FROM t HAVING count(*) BETWEEN 1 AND 3", // 3
            // HAVING aggregate NOT in the projection -> folded into an extra slot
            "SELECT count(*) FROM t HAVING sum(a) > 100", // no row
            "SELECT count(*) FROM t HAVING sum(a) = 6",   // 3
            "SELECT sum(a) FROM t HAVING count(*) > 1",   // 6
            "SELECT max(a) FROM t HAVING sum(a) > 3",     // 3
            "SELECT max(a), min(a) FROM t HAVING count(*) = 3 AND sum(a) > 5", // 3|1
            "SELECT sum(a) FROM t HAVING count(DISTINCT b) >= 3", // 6
        ],
    );

    // Empty table: the aggregate row exists (count 0) and HAVING filters it.
    check(
        &["CREATE TABLE e(a INTEGER)"],
        &[
            "SELECT count(*) FROM e HAVING count(*) > 0", // no row
            "SELECT count(*) FROM e HAVING count(*) = 0", // 0
            "SELECT sum(a) FROM e HAVING count(*) = 0",   // NULL row
            "SELECT sum(a) FROM e HAVING count(*) > 0",   // no row (HAVING-only agg)
        ],
    );
}

#[test]
fn having_only_aggregate_runs_on_vdbe() {
    // A HAVING that references an aggregate NOT present in the projection folds that
    // aggregate into an extra slot, so these run on the VDBE (proven by `query_vdbe`,
    // which errors on any fallback) and match the tree-walker.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INTEGER, b TEXT)").unwrap();
    c.execute("INSERT INTO t VALUES(1,'x'),(2,'y'),(3,'z')")
        .unwrap();
    for q in [
        "SELECT sum(a) FROM t HAVING count(*) > 1",
        "SELECT max(a) FROM t HAVING sum(a) > 100",
        "SELECT max(a), min(a) FROM t HAVING count(*) = 3 AND sum(a) > 5",
        "SELECT sum(a) FROM t HAVING count(DISTINCT b) >= 3",
    ] {
        let got = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE to run {q}: {e}"));
        assert_eq!(
            got.rows,
            c.query(q).unwrap().rows,
            "VDBE vs tree-walker on {q}"
        );
    }
}
