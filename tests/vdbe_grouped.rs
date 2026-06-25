//! Track B (roadmap B6): the VDBE grouped path now also compiles `HAVING` and
//! `ORDER BY` over the grouped output (ordering by an aggregate, a grouping
//! column, or an output ordinal/alias), plus `LIMIT`/`OFFSET`. Each query is run
//! through both `query` (the tree-walker oracle) and `query_vdbe`; the rows must
//! be identical. A spot-check against the real `sqlite3` CLI guards the oracle.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

const GROUPED: &[&str] = &[
    // HAVING over an aggregate.
    "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1",
    "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) >= 2",
    "SELECT a, sum(b) FROM t GROUP BY a HAVING sum(b) >= 5",
    "SELECT a, sum(b) FROM t GROUP BY a HAVING sum(b) > 100", // filters all out
    // HAVING referencing a grouping column.
    "SELECT a, count(*) FROM t GROUP BY a HAVING a > 1",
    "SELECT a, count(*) FROM t GROUP BY a HAVING a > 1 AND count(*) >= 1",
    // HAVING referencing an output alias (resolves to a grouping column).
    "SELECT a, count(*) AS n FROM t GROUP BY a HAVING n > 1",
    // ORDER BY an aggregate.
    "SELECT a, count(*) FROM t GROUP BY a ORDER BY count(*) DESC",
    "SELECT a, count(*) FROM t GROUP BY a ORDER BY count(*) ASC, a",
    "SELECT a, sum(b) FROM t GROUP BY a ORDER BY sum(b) DESC",
    // ORDER BY a grouping column.
    "SELECT a, count(*) FROM t GROUP BY a ORDER BY a",
    "SELECT a, count(*) FROM t GROUP BY a ORDER BY a DESC",
    // ORDER BY an output ordinal / alias.
    "SELECT a, count(*) FROM t GROUP BY a ORDER BY 2 DESC, 1",
    "SELECT a, count(*) AS n FROM t GROUP BY a ORDER BY n DESC",
    // Combinations: HAVING + ORDER BY + LIMIT/OFFSET.
    "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) >= 1 ORDER BY count(*) DESC, a",
    "SELECT a, count(*) FROM t GROUP BY a HAVING a >= 1 ORDER BY a DESC LIMIT 2",
    "SELECT a, count(*) FROM t GROUP BY a ORDER BY count(*) DESC LIMIT 1",
    "SELECT a, count(*) FROM t GROUP BY a ORDER BY a LIMIT 2 OFFSET 1",
    "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1 ORDER BY a DESC LIMIT 1 OFFSET 0",
    "SELECT a, count(*) FROM t GROUP BY a ORDER BY a LIMIT 0",
    // GROUP BY two columns with HAVING + ORDER BY.
    "SELECT a, b, count(*) FROM t GROUP BY a, b HAVING count(*) >= 1 ORDER BY count(*) DESC, a, b",
    // Aggregate-only output with HAVING on a grouping column.
    "SELECT count(*) FROM t GROUP BY a HAVING a > 1 ORDER BY a",
    // Expression over a grouping column / aggregate in the projection + ORDER BY.
    "SELECT a, count(*) * 2 FROM t GROUP BY a ORDER BY count(*) * 2 DESC, a",
    // WHERE + GROUP BY + HAVING + ORDER BY together.
    "SELECT a, count(*) FROM t WHERE b >= 0 GROUP BY a HAVING count(*) >= 1 ORDER BY a DESC",
    // HAVING / ORDER BY over an aggregate that is NOT in the projection.
    "SELECT a FROM t GROUP BY a HAVING sum(b) >= 5 ORDER BY a",
    "SELECT a, count(*) FROM t GROUP BY a ORDER BY sum(b) DESC, a",
    "SELECT a FROM t GROUP BY a HAVING max(b) > 3 ORDER BY min(b) DESC, a",
    // Positional GROUP BY (a bare integer ordinal names an output column):
    // `GROUP BY 1` groups by the first result column, like SQLite.
    "SELECT a, count(*) FROM t GROUP BY 1",
    "SELECT a, count(*) FROM t GROUP BY 1 ORDER BY 2 DESC, 1",
    "SELECT a, count(*) FROM t GROUP BY 1 ORDER BY 1 DESC LIMIT 2",
    "SELECT a, b, count(*) FROM t GROUP BY 1, 2 ORDER BY 1, 2",
    "SELECT a, count(*) FROM t GROUP BY 1 HAVING count(*) >= 2 ORDER BY 1",
    "SELECT a, count(*) AS n FROM t GROUP BY 1 ORDER BY n DESC, 1",
    // A positional term repeated / mixed with a named grouping column.
    "SELECT a, b, count(*) FROM t GROUP BY a, 2 ORDER BY 1, 2",
];

#[test]
fn grouped_having_orderby_matches_tree_walker() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT)")
        .unwrap();
    c.execute("INSERT INTO t(a,b) VALUES (3,10),(1,2),(2,5),(1,3),(2,7),(1,1)")
        .unwrap();
    for q in GROUPED {
        compare_vdbe_vs_tree(&c, q);
    }
    // Empty table: every grouped query yields no rows.
    c.execute("DELETE FROM t").unwrap();
    for q in GROUPED {
        compare_vdbe_vs_tree(&c, q);
    }
}

/// Compare the VDBE spike against the tree-walker. The tree-walker emits grouped
/// output ordered by the GROUP BY keys (like SQLite); the VDBE spike emits groups
/// in accumulation order. They produce the same SET of rows, so for a grouped
/// query with no explicit ORDER BY we compare order-insensitively. ORDER BY
/// queries are compared exactly.
fn compare_vdbe_vs_tree(c: &Connection, q: &str) {
    let mut got = c.query_vdbe(q).unwrap().rows;
    let mut want = c.query(q).unwrap().rows;
    let qu = q.to_ascii_uppercase();
    if qu.contains("GROUP BY") && !qu.contains("ORDER BY") {
        let rowcmp = |a: &Vec<graphitesql::Value>, b: &Vec<graphitesql::Value>| {
            for (x, y) in a.iter().zip(b.iter()) {
                let o = graphitesql::cmp_values(x, y);
                if o != core::cmp::Ordering::Equal {
                    return o;
                }
            }
            a.len().cmp(&b.len())
        };
        got.sort_by(rowcmp);
        want.sort_by(rowcmp);
    }
    assert_eq!(got, want, "VDBE vs tree-walker diverged on {q}");
}

#[test]
fn grouped_having_orderby_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT)")
        .unwrap();
    c.execute("INSERT INTO t(a,b) VALUES (3,10),(1,2),(2,5),(1,3),(2,7),(1,1)")
        .unwrap();
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT);\n\
         INSERT INTO t(a,b) VALUES (3,10),(1,2),(2,5),(1,3),(2,7),(1,1);\n";
    // Only compare against sqlite3 for queries with a fully deterministic row
    // order (a total ORDER BY). Without ORDER BY, graphite emits groups in
    // first-seen order whereas sqlite3 orders by the grouping key — a known,
    // intentional difference that the tree-walker oracle already covers.
    for q in GROUPED.iter().filter(|q| {
        let l = q.to_ascii_lowercase();
        l.contains("order by") && !l.contains("limit 0")
    }) {
        let vdbe: Vec<Vec<String>> = c
            .query_vdbe(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| r.iter().map(render).collect())
            .collect();
        let out = Command::new("sqlite3")
            .arg(":memory:")
            .arg("-ascii")
            .arg(format!("{setup}{q};"))
            .output()
            .unwrap();
        assert!(out.status.success(), "sqlite3 failed on {q}");
        let text = String::from_utf8(out.stdout).unwrap();
        // -ascii uses 0x1f between fields and 0x1e between records.
        let expected: Vec<Vec<String>> = text
            .split('\u{1e}')
            .filter(|r| !r.is_empty())
            .map(|r| r.split('\u{1f}').map(|f| f.to_string()).collect())
            .collect();
        assert_eq!(vdbe, expected, "VDBE vs sqlite3 diverged on {q}");
    }
}
