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
        // `x IS [NOT] TRUE|FALSE` is a truthiness test (now compiled, not a
        // fallback): NULL is neither true nor false; text/real coerce numerically.
        "SELECT 1 IS TRUE, 0 IS TRUE, NULL IS TRUE, 2 IS TRUE, 'x' IS TRUE, 2.5 IS TRUE",
        "SELECT 1 IS FALSE, 0 IS FALSE, NULL IS FALSE, 0.0 IS FALSE, '' IS FALSE",
        "SELECT 1 IS NOT TRUE, 0 IS NOT TRUE, NULL IS NOT TRUE, NULL IS NOT FALSE",
        "SELECT TRUE IS 1, TRUE IS FALSE, (1 < 2) IS TRUE, (1 > 2) IS FALSE",
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
        // Constant LIMIT/OFFSET *expressions* fold and compile (not just bare
        // literals): negative LIMIT = unlimited, arithmetic, parens, CAST.
        "SELECT a FROM t ORDER BY a LIMIT -1",
        "SELECT a FROM t ORDER BY a LIMIT 1+1",
        "SELECT a FROM t ORDER BY a LIMIT (3)",
        "SELECT a FROM t ORDER BY a LIMIT 10/2 OFFSET 1+1",
        "SELECT a FROM t ORDER BY a LIMIT 2 OFFSET (1)",
        "SELECT a FROM t ORDER BY a LIMIT CAST('2' AS INT)",
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
        // `t.*` projection expands to all columns (by table name and alias).
        "SELECT t.* FROM t",
        "SELECT t.*, a + 1 FROM t WHERE a > 1",
        "SELECT x.* FROM t x",
        "SELECT x.* FROM t AS x ORDER BY a DESC",
        // Qualified single-table column references (by name and by alias).
        "SELECT t.a, t.b FROM t WHERE t.a > 1 ORDER BY t.id",
        "SELECT x.a FROM t x WHERE x.b = 'y' ORDER BY x.a",
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
fn two_table_join_matches_tree_walker() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE emp(eid INTEGER PRIMARY KEY, name TEXT, dept INT)")
        .unwrap();
    c.execute("CREATE TABLE dep(did INTEGER PRIMARY KEY, dname TEXT)")
        .unwrap();
    c.execute("INSERT INTO emp(name,dept) VALUES ('a',1),('b',2),('c',1),('d',9)")
        .unwrap();
    c.execute("INSERT INTO dep(dname) VALUES ('eng'),('sales')")
        .unwrap();
    // A third table so 3- and 4-way inner joins are exercised through the VDBE.
    c.execute("CREATE TABLE loc(lid INTEGER PRIMARY KEY, dept INT, city TEXT)")
        .unwrap();
    c.execute("INSERT INTO loc(dept,city) VALUES (1,'NYC'),(2,'LA'),(1,'SF')")
        .unwrap();
    // An inner join is a filtered cross-product; the VDBE path matches the
    // tree-walker for explicit JOIN ... ON, comma joins, CROSS, projections,
    // WHERE, ORDER BY, LIMIT, and aggregates over the join — for any number of
    // tables (a single `query_vdbe` call would error if the VDBE fell back).
    for q in [
        "SELECT name, dname FROM emp JOIN dep ON dept = did",
        "SELECT name, dname FROM emp, dep WHERE dept = did",
        "SELECT name, dname FROM emp CROSS JOIN dep",
        "SELECT name, dname FROM emp INNER JOIN dep ON dept = did ORDER BY name",
        "SELECT name, dname FROM emp JOIN dep ON dept = did WHERE name <> 'a'",
        "SELECT name FROM emp JOIN dep ON dept = did ORDER BY dname, name",
        "SELECT count(*) FROM emp JOIN dep ON dept = did",
        "SELECT dname, count(*) FROM emp JOIN dep ON dept = did GROUP BY dname",
        "SELECT name || ':' || dname FROM emp JOIN dep ON dept = did",
        "SELECT * FROM emp JOIN dep ON dept = did",
        "SELECT name, dname FROM emp JOIN dep ON dept = did LIMIT 2",
        // 3- and 4-table inner joins (explicit ON, comma, CROSS), raw and ordered.
        "SELECT emp.name, dep.dname, loc.city FROM emp JOIN dep ON emp.dept = dep.did \
         JOIN loc ON loc.dept = emp.dept",
        "SELECT emp.name, dep.dname, loc.city FROM emp JOIN dep ON emp.dept = dep.did \
         JOIN loc ON loc.dept = emp.dept ORDER BY emp.name, loc.city",
        "SELECT emp.name, loc.city FROM emp, dep, loc \
         WHERE emp.dept = dep.did AND loc.dept = emp.dept",
        "SELECT count(*) FROM emp JOIN dep ON emp.dept = dep.did JOIN loc ON loc.dept = emp.dept",
        "SELECT emp.name FROM emp CROSS JOIN dep CROSS JOIN loc ORDER BY emp.name LIMIT 5",
        "SELECT emp.name, dep.dname, loc.city FROM emp JOIN dep ON emp.dept = dep.did \
         JOIN loc ON loc.dept = emp.dept JOIN dep d2 ON d2.did = emp.dept",
    ] {
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
        assert_eq!(got, want, "VDBE join vs tree-walker diverged on {q}");
    }

    // Direct differential check against sqlite3 for the join (ordered output).
    if Command::new("sqlite3").arg("--version").output().is_ok() {
        let path = std::env::temp_dir().join(format!("gsql-vjoin-{}.db", std::process::id()));
        let path = path.to_string_lossy().into_owned();
        let _ = std::fs::remove_file(&path);
        let setup = "CREATE TABLE emp(eid INTEGER PRIMARY KEY, name TEXT, dept INT);\
                     CREATE TABLE dep(did INTEGER PRIMARY KEY, dname TEXT);\
                     INSERT INTO emp(name,dept) VALUES ('a',1),('b',2),('c',1),('d',9);\
                     INSERT INTO dep(dname) VALUES ('eng'),('sales')";
        Command::new("sqlite3")
            .arg(&path)
            .arg(setup)
            .output()
            .unwrap();
        for q in [
            "SELECT name, dname FROM emp JOIN dep ON dept = did ORDER BY name",
            "SELECT count(*) FROM emp JOIN dep ON dept = did",
            "SELECT name||'/'||dname FROM emp, dep WHERE dept = did ORDER BY 1",
        ] {
            let want = {
                let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
                String::from_utf8_lossy(&o.stdout).trim_end().to_string()
            };
            let got = c
                .query_vdbe(q)
                .unwrap()
                .rows
                .iter()
                .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(got, want, "VDBE join vs sqlite3 diverged on {q}");
        }
        let _ = std::fs::remove_file(&path);
    }

    // Shared column names: a *qualified* reference disambiguates them, so a join
    // of two tables that both have `id` now works and matches the tree-walker.
    c.execute("CREATE TABLE p(id INTEGER PRIMARY KEY, v)")
        .unwrap();
    c.execute("CREATE TABLE q(id INTEGER PRIMARY KEY, w)")
        .unwrap();
    c.execute("INSERT INTO p(id,v) VALUES (1,'a'),(2,'b')")
        .unwrap();
    c.execute("INSERT INTO q(id,w) VALUES (1,'x'),(2,'y')")
        .unwrap();
    for q in [
        "SELECT v, w FROM p JOIN q ON p.id = q.id",
        "SELECT p.id, v, w FROM p JOIN q ON p.id = q.id ORDER BY p.id",
        "SELECT v FROM p JOIN q ON p.id = q.id WHERE q.id = 1",
    ] {
        assert_eq!(
            c.query_vdbe(q).unwrap().rows,
            c.query(q).unwrap().rows,
            "qualified join diverged on {q}"
        );
    }
    // A *bare* reference to a shared name is ambiguous: the VDBE bails so the
    // tree-walker resolves or rejects it identically.
    assert!(c
        .query_vdbe("SELECT id FROM p JOIN q ON p.id = q.id")
        .is_err());
    // LEFT joins are now handled (the router NULL-extends unmatched left rows);
    // the result matches the tree-walker. See `left_join_matches_tree_walker_and_sqlite3`
    // for the fuller battery. A RIGHT/FULL join still bails (NULL-extension of the
    // other side is not modeled).
    let q = "SELECT name, dname FROM emp LEFT JOIN dep ON dept = did ORDER BY name";
    assert_eq!(
        c.query_vdbe(q).unwrap().rows,
        c.query(q).unwrap().rows,
        "VDBE LEFT JOIN diverged on {q}"
    );
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
fn vdbe_routing_handles_compound_via_per_arm() {
    // With routing enabled, `query()` accelerates each arm of a compound query
    // through the VDBE while the tree-walker performs the set combination; the
    // result is identical to running entirely on the tree-walker.
    let mut vdbe = Connection::open_memory().unwrap();
    let mut plain = Connection::open_memory().unwrap();
    for c in [&mut vdbe, &mut plain] {
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT)")
            .unwrap();
        c.execute("INSERT INTO t(a) VALUES (3),(1),(2),(1),(4)")
            .unwrap();
    }
    vdbe.set_use_vdbe(true);
    for q in [
        "SELECT a FROM t WHERE a > 1 UNION ALL SELECT a FROM t WHERE a = 1",
        "SELECT a FROM t UNION SELECT a FROM t ORDER BY a",
        "SELECT a FROM t WHERE a >= 2 INTERSECT SELECT a FROM t WHERE a <= 3 ORDER BY a",
        "SELECT a FROM t EXCEPT SELECT a FROM t WHERE a = 1 ORDER BY a",
        "SELECT a FROM t WHERE a < 3 UNION ALL SELECT a*10 FROM t WHERE a > 2 ORDER BY 1",
    ] {
        assert_eq!(
            vdbe.query(q).unwrap().rows,
            plain.query(q).unwrap().rows,
            "compound routing diverged on {q}"
        );
    }
}

#[test]
fn explain_lists_vdbe_bytecode() {
    // Plain `EXPLAIN <select>` (B8) returns graphite's VDBE program as
    // (addr, opcode, detail) rows: sequential addresses, a recognizable opcode
    // stream, and a terminating Halt.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();

    let r = c.query("EXPLAIN SELECT 1 + 2").unwrap();
    assert_eq!(r.columns, vec!["addr", "opcode", "detail"]);
    assert!(!r.rows.is_empty());
    // Addresses are 0..n in order.
    for (i, row) in r.rows.iter().enumerate() {
        assert_eq!(row[0], Value::Integer(i as i64));
    }
    let opcodes = |q: &str| -> Vec<String> {
        c.query(q)
            .unwrap()
            .rows
            .iter()
            .map(|row| match &row[1] {
                Value::Text(s) => s.clone(),
                _ => String::new(),
            })
            .collect()
    };
    let consts = opcodes("EXPLAIN SELECT 1 + 2");
    assert!(consts.contains(&"Integer".to_string()));
    assert!(consts.contains(&"Arith".to_string()));
    assert!(consts.contains(&"ResultRow".to_string()));
    assert_eq!(consts.last().unwrap(), "Halt");

    // A table scan compiles to a Rewind/Column/Next loop.
    let scan = opcodes("EXPLAIN SELECT a FROM t WHERE a > 1");
    for op in ["Rewind", "Column", "Compare", "IfFalse", "Next", "Halt"] {
        assert!(
            scan.contains(&op.to_string()),
            "missing opcode {op} in {scan:?}"
        );
    }

    // A shape the VDBE cannot compile reports Unsupported (no bytecode).
    assert!(c.query("EXPLAIN SELECT a FROM t, t AS t2").is_err());
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

#[test]
fn collate_operator_matches_tree_walker() {
    // The explicit `COLLATE` operator now compiles through the VDBE (it used to
    // bail). The collation precedence mirrors sqlite: an explicit COLLATE on
    // either operand (left first) beats an implicit column collation (left first),
    // else BINARY; ORDER BY honors an explicit COLLATE or the column's own.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT, b TEXT COLLATE NOCASE)")
        .unwrap();
    c.execute("INSERT INTO t(a,b) VALUES ('A','A'),('a','a'),('B','b')")
        .unwrap();
    for q in [
        "SELECT a FROM t WHERE a = 'a' COLLATE NOCASE ORDER BY id",
        "SELECT a FROM t WHERE a COLLATE NOCASE = 'B'",
        "SELECT a FROM t WHERE b = 'a' ORDER BY id", // implicit NOCASE column
        "SELECT a FROM t WHERE b = 'a' COLLATE BINARY", // explicit overrides implicit
        "SELECT a FROM t WHERE b COLLATE BINARY = 'a'",
        "SELECT a FROM t ORDER BY a COLLATE NOCASE, id",
        "SELECT b FROM t ORDER BY b", // implicit NOCASE in ORDER BY
        "SELECT b FROM t ORDER BY b COLLATE BINARY",
        "SELECT a FROM t WHERE a < 'b' COLLATE NOCASE ORDER BY id",
    ] {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE COLLATE vs tree-walker diverged on {q}");
    }
}

#[test]
fn rowid_pseudo_column_matches_tree_walker() {
    // `rowid`/`_rowid_`/`oid` resolve to a hidden trailing slot on a single-table
    // scan, so they compile through the VDBE; `*` excludes the hidden slot. An
    // INTEGER PRIMARY KEY aliases the rowid (same value). A WITHOUT ROWID table
    // has no rowid, so such a reference falls back (the tree-walker errors).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a TEXT, b INT)").unwrap();
    c.execute("INSERT INTO t VALUES ('x',5),('y',1),('z',3)")
        .unwrap();
    // NB: `ORDER BY rowid` is deferred to the tree-walker (the scan already
    // satisfies it), so these order by a non-rowid column to stay on the VDBE.
    for q in [
        "SELECT rowid, a FROM t ORDER BY a",
        "SELECT rowid FROM t WHERE b > 2 ORDER BY a",
        "SELECT _rowid_, oid FROM t ORDER BY a",
        "SELECT a FROM t WHERE rowid = 2",
        "SELECT t.rowid, t.a FROM t ORDER BY t.a",
        "SELECT * FROM t ORDER BY a", // `*` excludes the hidden rowid slot
        "SELECT rowid * 2 AS r2 FROM t ORDER BY a",
    ] {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(got, want, "VDBE rowid vs tree-walker diverged on {q}");
    }

    // An INTEGER PRIMARY KEY column is the rowid alias (identical values).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, v)")
        .unwrap();
    c.execute("INSERT INTO u VALUES (10,'a'),(20,'b')").unwrap();
    assert_eq!(
        c.query_vdbe("SELECT rowid, id FROM u ORDER BY v")
            .unwrap()
            .rows,
        c.query("SELECT rowid, id FROM u ORDER BY v").unwrap().rows
    );

    // A WITHOUT ROWID table has no rowid: the VDBE bails (no hidden slot), so the
    // tree-walker handles it — and errors, like sqlite.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE w(k TEXT PRIMARY KEY, v) WITHOUT ROWID")
        .unwrap();
    assert!(c.query_vdbe("SELECT rowid FROM w").is_err());
}

#[test]
fn subquery_from_matches_tree_walker() {
    // A derived table (FROM subquery) over a single all-BINARY base table is
    // materialized and compiled through the VDBE: each output column inherits its
    // affinity from the resolved type and BINARY collation, so the outer query
    // compares exactly like the tree-walker.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT, b TEXT)").unwrap();
    c.execute("INSERT INTO t VALUES (3,'x'),(1,'y'),(2,'z')")
        .unwrap();
    for q in [
        "SELECT x FROM (SELECT a AS x FROM t WHERE a > 1) ORDER BY x",
        "SELECT * FROM (SELECT a, b FROM t) ORDER BY a",
        "SELECT sub.a, sub.b FROM (SELECT a, b FROM t WHERE a < 3) sub ORDER BY sub.a",
        "SELECT x FROM (SELECT a + 10 AS x FROM t) ORDER BY x",
        "SELECT a FROM (SELECT a FROM t) WHERE a = '2'", // INTEGER affinity inherited
        "SELECT x FROM (SELECT a AS x FROM t ORDER BY a LIMIT 2) ORDER BY x",
    ] {
        let got = c.query_vdbe(q).unwrap().rows;
        let want = c.query(q).unwrap().rows;
        assert_eq!(
            got, want,
            "VDBE subquery-FROM vs tree-walker diverged on {q}"
        );
    }

    // A subquery over a non-BINARY base column defers (its derived collation can't
    // be safely inherited); the tree-walker handles it.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(a TEXT COLLATE NOCASE)").unwrap();
    c.execute("INSERT INTO u VALUES ('A')").unwrap();
    assert!(c
        .query_vdbe("SELECT a FROM (SELECT a FROM u) WHERE a = 'a'")
        .is_err());
}

#[test]
fn noncorrelated_scalar_and_exists_subqueries_fold_on_vdbe() {
    // A non-correlated scalar or EXISTS subquery in a top-level expression is
    // folded to the constant it evaluates to, so the VDBE runs the rest of the
    // query (it has no cursor for a nested query). Results match the tree-walker.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, g TEXT, v INT)")
        .unwrap();
    c.execute("INSERT INTO t(g,v) VALUES ('a',10),('a',20),('b',5),('b',15),('b',25)")
        .unwrap();
    c.execute("CREATE TABLE u(w INT)").unwrap();
    c.execute("INSERT INTO u VALUES (100),(200),(300)").unwrap();
    for q in [
        "SELECT v FROM t WHERE v > (SELECT avg(v) FROM t) ORDER BY v",
        // (no ORDER BY on the rowid/IPK — that defers via a separate, pre-existing
        // "order satisfied by scan" rule, unrelated to subquery folding.)
        "SELECT g, (SELECT sum(w) FROM u) AS tot FROM t",
        "SELECT v FROM t WHERE EXISTS (SELECT 1 FROM u WHERE w > 250) ORDER BY v",
        "SELECT v FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE w > 9999) ORDER BY v",
        "SELECT count(*) FROM t WHERE v <= (SELECT max(v) FROM t WHERE g='a')",
        "SELECT v * (SELECT min(w) FROM u) FROM t",
        "SELECT v FROM t WHERE v > (SELECT min(w) FROM u) - 95 ORDER BY v",
    ] {
        // The query must compile and run on the VDBE (the fold makes it foldable)…
        let got = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE to handle {q}: {e}"))
            .rows;
        // …and match the tree-walker exactly.
        let want = {
            c.set_use_vdbe(false);
            let r = c.query(q).unwrap().rows;
            c.set_use_vdbe(true);
            r
        };
        assert_eq!(
            got, want,
            "VDBE folded-subquery vs tree-walker diverged on {q}"
        );
    }

    // A *correlated* subquery references the outer row, so it is NOT folded and
    // the VDBE defers to the tree-walker (which is still correct). A scalar
    // subquery projecting a bare column is likewise left alone (it would carry
    // that column's affinity, which a plain literal would not).
    assert!(c
        .query_vdbe("SELECT v FROM t a WHERE v > (SELECT avg(v) FROM t b WHERE b.g = a.g)")
        .is_err());
    assert!(c
        .query_vdbe("SELECT v FROM t WHERE g = (SELECT g FROM t WHERE v = 25)")
        .is_err());
}

#[test]
fn folded_subqueries_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, g TEXT, v INT);\
                 INSERT INTO t(g,v) VALUES ('a',10),('a',20),('b',5),('b',15),('b',25);\
                 CREATE TABLE u(w INT); INSERT INTO u VALUES(100),(200),(300);";
    let mut c = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            c.execute(s).unwrap();
        }
    }
    for q in [
        "SELECT v FROM t WHERE v > (SELECT avg(v) FROM t) ORDER BY v",
        "SELECT g, (SELECT sum(w) FROM u) FROM t ORDER BY id",
        "SELECT v FROM t WHERE EXISTS (SELECT 1 FROM u WHERE w > 250) ORDER BY v",
        "SELECT v FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE w > 9999) ORDER BY v",
        "SELECT v * (SELECT min(w) FROM u) FROM t ORDER BY id",
    ] {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(format!("{setup} {q};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = c
            .query(q)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(got, want, "folded-subquery vs sqlite3 diverged on {q}");
    }
}

#[test]
fn left_join_matches_tree_walker_and_sqlite3() {
    // A LEFT JOIN cannot be modeled as a filtered cross-product (unmatched left
    // rows must be NULL-extended), so the router builds the joined rows by a real
    // nested loop and the VDBE runs projection/WHERE/aggregates over them. Results
    // must match the tree-walker (forced via query_vdbe) and sqlite3.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, g TEXT)")
        .unwrap();
    c.execute("CREATE TABLE u(t_id INT, w INT)").unwrap();
    c.execute("CREATE TABLE x(w_id INT, n TEXT)").unwrap();
    c.execute("INSERT INTO t(g) VALUES('a'),('b'),('c')")
        .unwrap();
    c.execute("INSERT INTO u VALUES(1,10),(1,11),(3,30)")
        .unwrap();
    c.execute("INSERT INTO x VALUES(10,'ten'),(30,'thirty')")
        .unwrap();
    let qs = [
        "SELECT t.g, u.w FROM t LEFT JOIN u ON u.t_id=t.id ORDER BY t.id, u.w",
        "SELECT t.g, u.w FROM t LEFT JOIN u ON u.t_id=t.id WHERE u.w IS NULL ORDER BY t.id",
        "SELECT t.g, count(u.w) FROM t LEFT JOIN u ON u.t_id=t.id GROUP BY t.g ORDER BY t.g",
        "SELECT t.g, u.w FROM t LEFT JOIN u ON u.t_id=t.id AND u.w>10 ORDER BY t.id, u.w",
        "SELECT count(*) FROM t LEFT JOIN u ON u.t_id=t.id",
        "SELECT t.g, u.w, x.n FROM t LEFT JOIN u ON u.t_id=t.id LEFT JOIN x ON x.w_id=u.w \
         ORDER BY t.id, u.w",
        "SELECT t.g, u.w, x.n FROM t LEFT JOIN u ON u.t_id=t.id INNER JOIN x ON x.w_id=u.w \
         ORDER BY t.id",
    ];
    for q in qs {
        // The VDBE must actually handle it (not silently fall back).
        let got = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE to handle {q}: {e}"))
            .rows;
        let want = {
            c.set_use_vdbe(false);
            let r = c.query(q).unwrap().rows;
            c.set_use_vdbe(true);
            r
        };
        assert_eq!(got, want, "VDBE LEFT JOIN vs tree-walker diverged on {q}");
    }

    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, g TEXT);\
                 CREATE TABLE u(t_id INT, w INT); CREATE TABLE x(w_id INT, n TEXT);\
                 INSERT INTO t(g) VALUES('a'),('b'),('c');\
                 INSERT INTO u VALUES(1,10),(1,11),(3,30);\
                 INSERT INTO x VALUES(10,'ten'),(30,'thirty');";
    for q in qs {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(format!("{setup} {q};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = c
            .query(q)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(got, want, "LEFT JOIN vs sqlite3 diverged on {q}");
    }
}

#[test]
fn right_and_full_join_match_tree_walker_and_sqlite3() {
    // A single RIGHT/FULL outer join runs on the VDBE: the router emits the
    // left-driven matched pairs, (FULL only) null-extends unmatched left rows, and
    // appends every unmatched right row — matching sqlite's row order exactly.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(k INT, g TEXT)").unwrap();
    c.execute("CREATE TABLE u(k INT, w INT)").unwrap();
    c.execute("INSERT INTO t VALUES(1,'a'),(2,'b'),(3,'c')")
        .unwrap();
    c.execute("INSERT INTO u VALUES(1,10),(2,20),(1,11),(9,90)")
        .unwrap();
    let qs = [
        "SELECT t.g, u.w FROM t RIGHT JOIN u ON t.k=u.k",
        "SELECT t.g, u.w FROM t FULL JOIN u ON t.k=u.k",
        "SELECT t.g, u.w FROM t FULL OUTER JOIN u ON t.k=u.k",
        "SELECT count(*) FROM t RIGHT JOIN u ON t.k=u.k",
        "SELECT t.g, u.w FROM t RIGHT JOIN u ON t.k=u.k WHERE t.g IS NULL",
        "SELECT t.g, count(u.w) FROM t FULL JOIN u ON t.k=u.k GROUP BY t.g ORDER BY t.g",
    ];
    for q in qs {
        // No ORDER BY on most: the VDBE must reproduce sqlite's row order, so
        // compare against the tree-walker (forced via query_vdbe) without sorting.
        let got = c
            .query_vdbe(q)
            .unwrap_or_else(|e| panic!("expected VDBE to handle {q}: {e}"))
            .rows;
        let want = {
            c.set_use_vdbe(false);
            let r = c.query(q).unwrap().rows;
            c.set_use_vdbe(true);
            r
        };
        assert_eq!(got, want, "VDBE RIGHT/FULL vs tree-walker diverged on {q}");
    }

    if Command::new("sqlite3").arg("--version").output().is_err() {
        return;
    }
    let setup = "CREATE TABLE t(k INT, g TEXT); CREATE TABLE u(k INT, w INT);\
                 INSERT INTO t VALUES(1,'a'),(2,'b'),(3,'c');\
                 INSERT INTO u VALUES(1,10),(2,20),(1,11),(9,90);";
    for q in qs {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(format!("{setup} {q};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = c
            .query(q)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(got, want, "RIGHT/FULL JOIN vs sqlite3 diverged on {q}");
    }
}
