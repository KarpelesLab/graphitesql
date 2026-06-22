//! Track B: the VDBE bytecode IR spike. For constant `SELECT` projections, the
//! compiled-and-interpreted program must produce the same rows as the
//! tree-walking executor and as `sqlite3`.

#![cfg(feature = "std")]

use graphitesql::exec::vdbe;
use graphitesql::sql::{ast::Statement, parse_one};
use graphitesql::{Connection, Value};
use std::process::Command;

fn vdbe_run(sql: &str) -> Vec<Vec<Value>> {
    let Statement::Select(sel) = parse_one(sql).unwrap() else {
        panic!("not a select")
    };
    let prog = vdbe::compile_const_select(&sel).unwrap();
    vdbe::run(&prog).unwrap()
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

#[test]
fn vdbe_matches_tree_walker() {
    let c = Connection::open_memory().unwrap();
    let queries = [
        "SELECT 1 + 2 * 3",
        "SELECT 10 - 4, 8 / 2, 17 % 5",
        "SELECT 'a' || 'b' || 'c'",
        "SELECT -5, 3.5, 2 + 3.5",
        "SELECT (1 + 2) * (3 + 4)",
        "SELECT 100 / 7, 100.0 / 7",
        "SELECT 1 < 2, 2 <= 2, 3 > 4, 5 = 5, 5 <> 6",
        "SELECT 1 AND 1, 1 AND 0, 0 OR 1, 0 OR 0",
        "SELECT NOT 0, NOT 1, NULL IS NULL, 1 IS NOT NULL",
        "SELECT (1 < 2) AND (3 < 4), (1 > 2) OR (5 = 5)",
        "SELECT CASE WHEN 1 > 2 THEN 'a' WHEN 3 > 2 THEN 'b' ELSE 'c' END",
        "SELECT CASE 5 WHEN 1 THEN 'one' WHEN 5 THEN 'five' ELSE '?' END",
        "SELECT CASE WHEN 0 THEN 1 END, CASE WHEN 1 THEN 2 ELSE 3 END",
        "SELECT CAST(3.9 AS INTEGER), CAST('42' AS INTEGER), CAST(5 AS TEXT)",
        "SELECT CAST('3.14' AS REAL), CAST(7 AS REAL)",
        "SELECT x'01ff', x''",
        "SELECT CASE WHEN 1 THEN x'aa' ELSE x'bb' END",
        "SELECT x'00' IS NULL, x'01' IS NOT NULL",
        "SELECT 12 & 10, 12 | 10, 1 << 4, 256 >> 2",
        "SELECT ~0, ~5, ~-1, +7, +(3 * 2)",
        "SELECT (5 & 3) | 8, ~(1 << 3)",
        "SELECT 6 & NULL, NULL | 1, ~NULL",
        "SELECT 1 IS 1, 1 IS 2, NULL IS NULL, 1 IS NULL, NULL IS 1",
        "SELECT 1 IS NOT 2, NULL IS NOT NULL, 'a' IS 'a', 'a' IS NOT 'b'",
        "SELECT 5 BETWEEN 1 AND 10, 5 BETWEEN 6 AND 10, 5 BETWEEN 5 AND 5",
        "SELECT 5 NOT BETWEEN 1 AND 10, 5 NOT BETWEEN 6 AND 10",
        "SELECT 'm' BETWEEN 'a' AND 'z', NULL BETWEEN 1 AND 10",
        "SELECT 'abc' LIKE 'a%', 'abc' LIKE 'A_C', 'abc' LIKE 'x%'",
        "SELECT 'abc' GLOB 'a*', 'abc' GLOB 'A*', 'abc' GLOB '?b?'",
        "SELECT NULL LIKE 'a%', 'abc' NOT LIKE 'a%'",
        "SELECT 2 IN (1,2,3), 5 IN (1,2,3), 2 NOT IN (1,2,3)",
        "SELECT 1 IN (2,3,NULL), 1 IN (1,NULL), NULL IN (1,2)",
        "SELECT 'b' IN ('a','b','c'), 9 IN ()",
        "SELECT '{\"a\":5}' -> '$.a', '{\"a\":5}' ->> '$.a'",
        "SELECT '[1,2,3]' -> 1, '[1,2,3]' ->> 2, '{\"x\":null}' ->> '$.x'",
        // Pure scalar function calls (deferred to eval_scalar).
        "SELECT abs(-7), abs(3.5), length('hello'), upper('abc'), lower('ABC')",
        "SELECT substr('graphite', 2, 4), replace('a-b-c', '-', '+'), instr('abc','b')",
        "SELECT hex('AB'), quote('it''s'), char(72, 105), unicode('A')",
        "SELECT round(3.14159, 2), sign(-5), abs(length(trim('  x  ')))",
        "SELECT typeof(1), typeof('a'), typeof(2.5), typeof(NULL), typeof(x'00')",
        "SELECT coalesce(NULL, NULL, 3), ifnull(NULL, 'd'), nullif(5, 5), nullif(5, 6)",
        "SELECT max(3, 1, 2), min(3, 1, 2), abs(-2) + length('ab')",
        "SELECT json_type('[1,2]'), json_array_length('[1,2,3]'), json_valid('{')",
        "SELECT upper(substr('hello world', 7)), length(quote('x'))",
    ];
    for q in queries {
        let walker = c.query(q).unwrap().rows;
        let vdbe = vdbe_run(q);
        assert_eq!(vdbe, walker, "VDBE vs tree-walker diverged on {q}");
    }
}

#[test]
fn vdbe_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let queries = [
        "SELECT 1 + 2 * 3",
        "SELECT 'x' || 'y'",
        "SELECT 10 - 4, 8 / 2",
        "SELECT (2 + 3) * 4",
        "SELECT 12 & 10, 12 | 10, 1 << 4, 256 >> 2",
        "SELECT ~0, ~5, +7, (5 & 3) | 8",
        "SELECT 1 IS 1, 1 IS NOT 2, NULL IS NULL",
        "SELECT 5 BETWEEN 1 AND 10, 5 NOT BETWEEN 6 AND 10",
        "SELECT 'abc' LIKE 'a%', 'abc' GLOB '?b?'",
        "SELECT 2 IN (1,2,3), 5 NOT IN (1,2,3)",
        "SELECT abs(-7), length('hello'), upper('abc'), substr('graphite',2,4)",
        "SELECT round(3.14159,2), coalesce(NULL,3), typeof(2.5), max(3,1,2)",
    ];
    for q in queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(q)
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = vdbe_run(q)
            .iter()
            .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(got, want, "VDBE vs sqlite3 diverged on {q}");
    }
}

#[test]
fn table_scan_matches_tree_walker() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT)")
        .unwrap();
    c.execute("INSERT INTO t(a,b) VALUES (3,'x'),(1,'y'),(2,'z'),(1,'y'),(2,'x')")
        .unwrap();
    // Plain projections, expressions over columns, and WHERE filters all run
    // through the VDBE and match the tree-walker.
    for q in [
        "SELECT a, b FROM t",
        "SELECT a * 2, b || '!' FROM t",
        "SELECT *, a + id FROM t",
        "SELECT CASE WHEN a > 1 THEN 'big' ELSE 'small' END FROM t",
        "SELECT a, b FROM t WHERE a > 1",
        "SELECT id FROM t WHERE b = 'y'",
        "SELECT a FROM t WHERE a >= 2 AND b <> 'z'",
        "SELECT a FROM t WHERE 0",
        "SELECT a FROM t LIMIT 2",
        "SELECT a FROM t LIMIT 0",
        "SELECT a FROM t LIMIT 100",
        "SELECT id FROM t WHERE a >= 1 LIMIT 1",
        "SELECT a FROM t LIMIT 1 OFFSET 1",
        "SELECT a FROM t LIMIT 2 OFFSET 1",
        "SELECT a FROM t LIMIT 5 OFFSET 2",
        "SELECT id FROM t WHERE a >= 1 LIMIT 1 OFFSET 1",
        "SELECT a FROM t ORDER BY a",
        "SELECT a, b FROM t ORDER BY a DESC",
        "SELECT a, b FROM t ORDER BY b",
        "SELECT a FROM t ORDER BY a DESC LIMIT 2",
        "SELECT a FROM t ORDER BY a LIMIT 2 OFFSET 1",
        "SELECT a, b FROM t WHERE a >= 1 ORDER BY a DESC",
        "SELECT a * 2 AS d FROM t ORDER BY d",
        "SELECT a, b FROM t ORDER BY 1 DESC",
        "SELECT a FROM t ORDER BY b DESC, a",
        "SELECT DISTINCT a FROM t",
        "SELECT DISTINCT a > 1 FROM t",
        "SELECT DISTINCT b FROM t ORDER BY b",
        "SELECT DISTINCT a FROM t WHERE a >= 1 LIMIT 2",
        "SELECT DISTINCT a FROM t ORDER BY a DESC LIMIT 1 OFFSET 1",
        "SELECT count(*) FROM t",
        "SELECT count(*), count(a), sum(a), avg(a) FROM t",
        "SELECT min(a), max(a), min(b), max(b) FROM t",
        "SELECT total(a), sum(a) FROM t WHERE a > 100",
        "SELECT count(*) FROM t WHERE a > 1",
        "SELECT sum(a * 2), avg(a + 1) FROM t",
        "SELECT group_concat(b) FROM t",
        "SELECT count(*), min(a) FROM t WHERE b = 'nope'",
        "SELECT a, count(*) FROM t GROUP BY a",
        "SELECT a, count(*), sum(a) FROM t GROUP BY a",
        "SELECT b, count(*) FROM t GROUP BY b",
        "SELECT a, b, count(*) FROM t GROUP BY a, b",
        "SELECT count(*), a FROM t GROUP BY a",
        "SELECT a, max(b), min(b) FROM t WHERE a >= 1 GROUP BY a",
        "SELECT a, group_concat(b) FROM t GROUP BY a",
        // Pure scalar functions over column values run through Op::Func.
        "SELECT upper(b), length(b), abs(a) FROM t",
        "SELECT a, substr(b, 1, 1), typeof(a) FROM t",
        "SELECT b FROM t WHERE length(b) = 1",
        "SELECT coalesce(b, 'none'), round(a * 1.5, 1) FROM t",
    ] {
        let mut got = c.query_vdbe(q).unwrap().rows;
        let mut want = c.query(q).unwrap().rows;
        // The tree-walker emits grouped output ordered by the GROUP BY keys (as
        // SQLite does); the VDBE spike emits groups in accumulation order. They
        // produce the same SET of rows, so for a grouped query with no explicit
        // ORDER BY compare order-insensitively. (Non-grouped and ORDER BY queries
        // are still compared exactly.)
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
    // Empty table: the Rewind loop emits nothing.
    c.execute("DELETE FROM t").unwrap();
    assert!(c.query_vdbe("SELECT a FROM t").unwrap().rows.is_empty());
}

#[test]
fn table_scan_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-vdbe-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT);\
                 INSERT INTO t(a,b) VALUES (3,'x'),(1,'y'),(2,'z'),(1,'y'),(2,'x')";
    Command::new("sqlite3")
        .arg(&path)
        .arg(setup)
        .output()
        .unwrap();

    // Build the same data in a separate in-memory db for the VDBE run (do not
    // touch the sqlite3 reference file).
    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let queries = [
        "SELECT a FROM t",
        "SELECT a, b FROM t",
        "SELECT a + 1, b || b FROM t",
        "SELECT a, b FROM t ORDER BY a",
        "SELECT a, b FROM t ORDER BY b DESC",
        "SELECT a FROM t ORDER BY a DESC LIMIT 2 OFFSET 1",
        "SELECT DISTINCT a FROM t ORDER BY a",
        "SELECT count(*), sum(a), avg(a), min(a), max(b) FROM t",
        "SELECT count(*) FROM t WHERE a > 1",
    ];
    for q in queries {
        let want = {
            let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = g
            .query_vdbe(q)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(got, want, "VDBE vs sqlite3 diverged on {q}");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn falls_back_for_non_constant() {
    // Anything beyond a constant SELECT list is Unsupported, so the engine can
    // fall back to the tree-walker.
    for q in ["SELECT * FROM t", "SELECT 1 WHERE 1=1", "SELECT count(*)"] {
        let Statement::Select(sel) = parse_one(q).unwrap() else {
            panic!()
        };
        assert!(
            vdbe::compile_const_select(&sel).is_err(),
            "expected {q} to be unsupported by the spike"
        );
    }
}
