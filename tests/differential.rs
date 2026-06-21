//! Differential testing against the real `sqlite3`.
//!
//! Builds the same dataset in graphitesql and in `sqlite3`, then runs a large,
//! generated corpus of `SELECT`s through both and asserts identical output. This
//! is how we push toward broad SQLite compatibility: any divergence is a bug.
//!
//! Skipped automatically if the `sqlite3` CLI is unavailable. Output is compared
//! in SQLite's default list mode (`|`-separated, NULL = empty). The dataset uses
//! only integers and text so value formatting is unambiguous (no float-printing
//! differences).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

const SETUP: &str = "
CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b INT, g INT, s TEXT);
CREATE TABLE u(id INTEGER PRIMARY KEY, t_id INT, w INT);
CREATE INDEX it_a ON t(a);
CREATE INDEX it_g ON t(g);
CREATE INDEX it_gb ON t(g, b);
CREATE INDEX it_s ON t(s);
CREATE INDEX iu_tid ON u(t_id);
";

fn dataset_inserts() -> Vec<String> {
    let mut out = Vec::new();
    // 60 rows with deterministic, varied values (some NULLs in s and b).
    for id in 1..=60i64 {
        let a = id;
        let b = if id % 11 == 0 {
            "NULL".to_string()
        } else {
            (id * 7 % 13 - 6).to_string()
        };
        let g = id % 5;
        let s = if id % 9 == 0 {
            "NULL".to_string()
        } else {
            format!("'str{}'", id % 7)
        };
        out.push(format!(
            "INSERT INTO t(id,a,b,g,s) VALUES ({id},{a},{b},{g},{s});"
        ));
    }
    for id in 1..=40i64 {
        out.push(format!(
            "INSERT INTO u(id,t_id,w) VALUES ({id},{},{});",
            id % 60 + 1,
            id * 3 % 17
        ));
    }
    out
}

fn sqlite_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Render a graphitesql result the way `sqlite3` prints list mode.
fn render(result: &graphitesql::QueryResult) -> String {
    let mut lines = Vec::new();
    for row in &result.rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| match v {
                Value::Null => String::new(),
                Value::Integer(i) => i.to_string(),
                Value::Text(s) => s.clone(),
                // graphitesql's canonical real formatting (matches sqlite's %.15g).
                Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
            })
            .collect();
        lines.push(cells.join("|"));
    }
    lines.join("\n")
}

fn corpus() -> Vec<String> {
    let mut q = Vec::new();
    let cols = ["a", "b", "g", "id"];
    let ops = ["=", "<>", "<", "<=", ">", ">="];
    let lits = ["-6", "-3", "0", "1", "5", "7", "13", "30", "60"];

    // 1) Single-predicate filters (rows and counts).
    for col in cols {
        for op in ops {
            for lit in lits {
                q.push(format!(
                    "SELECT id FROM t WHERE {col} {op} {lit} ORDER BY id;"
                ));
                q.push(format!("SELECT count(*) FROM t WHERE {col} {op} {lit};"));
            }
        }
    }
    // 1b) Two-column predicates across several pairs.
    for (c1, c2) in [("a", "b"), ("a", "g"), ("b", "g"), ("id", "b")] {
        for op1 in ops {
            for op2 in ops {
                for lit in ["0", "5", "20"] {
                    q.push(format!(
                        "SELECT id FROM t WHERE {c1} {op1} {lit} AND {c2} {op2} {lit} ORDER BY id;"
                    ));
                    q.push(format!(
                        "SELECT count(*) FROM t WHERE {c1} {op1} {lit} OR {c2} {op2} {lit};"
                    ));
                }
            }
        }
    }
    // 1c) Text predicates.
    for op in ["=", "<>", "<", "<=", ">", ">="] {
        for n in 0..7 {
            q.push(format!(
                "SELECT id FROM t WHERE s {op} 'str{n}' ORDER BY id;"
            ));
        }
    }
    // 1d) ORDER BY each column, both directions, with limits.
    for col in cols {
        for dir in ["ASC", "DESC"] {
            for k in [1, 5, 20] {
                q.push(format!(
                    "SELECT id, {col} FROM t ORDER BY {col} {dir}, id LIMIT {k};"
                ));
            }
        }
    }
    // 1e) Aggregates over a filter, grouped.
    for op in ops {
        for lit in ["0", "10", "30"] {
            q.push(format!(
                "SELECT g, count(*), sum(a), min(b), max(b) FROM t WHERE a {op} {lit} GROUP BY g ORDER BY g;"
            ));
        }
    }
    // 2) Boolean combinations.
    for op1 in ops {
        for op2 in ops {
            q.push(format!(
                "SELECT id FROM t WHERE a {op1} 20 AND b {op2} 0 ORDER BY id;"
            ));
            q.push(format!(
                "SELECT id FROM t WHERE a {op1} 20 OR b {op2} 0 ORDER BY id;"
            ));
        }
    }
    // 3) Arithmetic & expression projections.
    for expr in [
        "a+b", "a-b", "a*b", "a/3", "a%3", "a*2-b", "(a+b)*2", "-a", "a&b", "a|b", "a<<1", "a>>1",
    ] {
        q.push(format!(
            "SELECT id, {expr} FROM t WHERE b IS NOT NULL ORDER BY id;"
        ));
    }
    // 4) NULL handling.
    q.push("SELECT id FROM t WHERE b IS NULL ORDER BY id;".into());
    q.push("SELECT id FROM t WHERE s IS NOT NULL ORDER BY id;".into());
    q.push("SELECT id, coalesce(s, 'none') FROM t ORDER BY id;".into());
    q.push("SELECT id, b IS NULL FROM t ORDER BY id;".into());
    // 5) Aggregates + GROUP BY/HAVING.
    q.push("SELECT count(*), sum(a), min(b), max(b), min(a), max(a) FROM t;".into());
    q.push("SELECT g, count(*), sum(a), min(b), max(b) FROM t GROUP BY g ORDER BY g;".into());
    q.push("SELECT g, count(*) FROM t GROUP BY g HAVING count(*) > 11 ORDER BY g;".into());
    q.push("SELECT g, sum(a) FROM t WHERE a > 10 GROUP BY g ORDER BY g;".into());
    q.push("SELECT count(DISTINCT g), count(DISTINCT s) FROM t;".into());
    // 6) Functions.
    for f in [
        "length(s)",
        "upper(s)",
        "lower(s)",
        "substr(s,1,3)",
        "abs(b)",
        "instr(s,'r')",
        "replace(s,'str','X')",
        "trim(s)",
        "typeof(b)",
        "sign(b)",
        "max(a,b)",
        "min(a,b)",
        "nullif(g,0)",
        "ifnull(s,'?')",
        "octet_length(s)",
        "octet_length(a)",
        "glob('str*', s)",
        "glob('STR*', s)",
        "glob('str?', s)",
    ] {
        q.push(format!("SELECT id, {f} FROM t ORDER BY id;"));
    }
    // 7) LIKE / GLOB.
    for pat in ["'str%'", "'%2'", "'str_'", "'STR1'"] {
        q.push(format!("SELECT id FROM t WHERE s LIKE {pat} ORDER BY id;"));
    }
    for pat in ["'str*'", "'*1'", "'str?'", "'[s]tr*'"] {
        q.push(format!("SELECT id FROM t WHERE s GLOB {pat} ORDER BY id;"));
    }
    // 8) ORDER BY / LIMIT / OFFSET / DISTINCT.
    q.push("SELECT id FROM t ORDER BY b, id;".into());
    q.push("SELECT id FROM t ORDER BY b DESC, id ASC;".into());
    q.push("SELECT DISTINCT g FROM t ORDER BY g;".into());
    q.push("SELECT DISTINCT s FROM t ORDER BY s;".into());
    for n in [1, 3, 7, 25] {
        q.push(format!("SELECT id FROM t ORDER BY id LIMIT {n};"));
        q.push(format!("SELECT id FROM t ORDER BY id LIMIT {n} OFFSET 5;"));
    }
    // 9) IN / BETWEEN / CASE.
    q.push("SELECT id FROM t WHERE g IN (1,3) ORDER BY id;".into());
    q.push("SELECT id FROM t WHERE a NOT IN (1,2,3,4,5) ORDER BY id;".into());
    q.push("SELECT id FROM t WHERE a BETWEEN 10 AND 20 ORDER BY id;".into());
    q.push(
        "SELECT id, CASE WHEN g=0 THEN 'z' WHEN g=1 THEN 'o' ELSE 'm' END FROM t ORDER BY id;"
            .into(),
    );
    // 10) Joins.
    q.push("SELECT t.id, u.w FROM t JOIN u ON t.id = u.t_id ORDER BY t.id, u.w;".into());
    q.push("SELECT t.g, count(*) FROM t JOIN u ON t.id = u.t_id GROUP BY t.g ORDER BY t.g;".into());
    q.push(
        "SELECT t.id FROM t LEFT JOIN u ON t.id = u.t_id WHERE u.w IS NULL ORDER BY t.id;".into(),
    );
    // 11) Subqueries.
    q.push("SELECT id FROM t WHERE id IN (SELECT t_id FROM u) ORDER BY id;".into());
    q.push("SELECT count(*) FROM t WHERE a > (SELECT avg(w) FROM u);".into());
    // 12) CTE.
    q.push("WITH hi AS (SELECT id FROM t WHERE a > 40) SELECT count(*) FROM hi;".into());

    // 13) NULL-aware aggregates and edge cases.
    q.push("SELECT count(b), count(*), count(s) FROM t;".into());
    q.push("SELECT sum(b), min(b), max(b) FROM t;".into()); // b has NULLs
    q.push("SELECT g, count(b), count(s) FROM t GROUP BY g ORDER BY g;".into());
    q.push("SELECT id, a || '-' || s FROM t ORDER BY id;".into()); // NULL s -> NULL
    q.push("SELECT id, b % 3 FROM t WHERE b IS NOT NULL ORDER BY id;".into());
    q.push("SELECT id, -b FROM t WHERE b IS NOT NULL ORDER BY id;".into());
    // 14) CAST.
    q.push("SELECT id, CAST(s AS INTEGER) FROM t ORDER BY id;".into());
    q.push("SELECT CAST(avg(a) AS INTEGER) FROM t;".into());
    q.push("SELECT id, CAST(b AS TEXT) FROM t WHERE b IS NOT NULL ORDER BY id;".into());
    // 15) Multi-column GROUP BY / DISTINCT / HAVING-on-aggregate.
    q.push("SELECT g, a%2, count(*) FROM t GROUP BY g, a%2 ORDER BY g, a%2;".into());
    q.push("SELECT DISTINCT g, a%2 FROM t ORDER BY g, a%2;".into());
    q.push("SELECT g, sum(a) FROM t GROUP BY g HAVING sum(a) > 300 ORDER BY g;".into());
    // 16) Nested functions and predicates.
    q.push("SELECT id, upper(substr(s,1,3)) FROM t WHERE s IS NOT NULL ORDER BY id;".into());
    q.push("SELECT id FROM t WHERE s IS NULL OR s LIKE 'str_' ORDER BY id;".into());
    q.push("SELECT id, length(s), abs(b) FROM t ORDER BY id;".into());
    q.push("SELECT id FROM t WHERE NOT (a > 30) ORDER BY id;".into());
    q.push("SELECT id FROM t WHERE (g = 1 OR g = 2) AND a < 30 ORDER BY id;".into());
    // 17) ORDER BY expression / by output position.
    q.push("SELECT id, a%5 FROM t ORDER BY 2, 1 LIMIT 15;".into());
    q.push("SELECT id FROM t ORDER BY a%7, id;".into());

    // 18) Type affinity in comparisons (column type vs literal type).
    for (col, lit) in [
        ("a", "'5'"),
        ("a", "'10'"),
        ("g", "'1'"),
        ("id", "'30'"),
        ("s", "1"),
        ("s", "'str3'"),
        ("b", "'0'"),
    ] {
        for op in ops {
            q.push(format!(
                "SELECT id FROM t WHERE {col} {op} {lit} ORDER BY id;"
            ));
        }
    }

    // 19) Scalar edge cases (integer division/modulo signs, substr bounds,
    // NULL-propagating functions, concat coercion, constant WHERE, nested CASE).
    for e in [
        "7/2",
        "-7/2",
        "7/-2",
        "-7/-2",
        "7%3",
        "-7%3",
        "7%-3",
        "substr('hello',2)",
        "substr('hello',2,2)",
        "substr('hello',-2)",
        "substr('hello',-2,1)",
        "substr('hello',0,3)",
        "substr('hello',10)",
        "length('')",
        "length('abc')",
        "abs(-5)",
        "abs(5)",
        "1 || 2",
        "'x' || 3",
        "min(3,1,2)",
        "max(3,1,2)",
        "coalesce(NULL, NULL, 3)",
        "nullif(5,5)",
        "nullif(5,6)",
        "CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' END",
        "CASE WHEN 0 THEN 'a' END",
        "5 IN (1,2,5)",
        "5 NOT IN (1,2)",
        "NULL IN (1,2)",
        "2 BETWEEN 1 AND 3",
        "4 BETWEEN 1 AND 3",
    ] {
        q.push(format!("SELECT {e};"));
    }
    for w in ["1", "0", "NULL", "1=1", "1=0", "'a'='a'"] {
        q.push(format!("SELECT count(*) FROM t WHERE {w};"));
    }
    // 20) NULL-propagating functions over a column with NULLs.
    for f in ["length(s)", "upper(s)", "substr(s,2)", "abs(b)", "s || '!'"] {
        q.push(format!("SELECT id, {f} FROM t ORDER BY id;"));
    }
    // 21) Aggregates over empty/filtered sets.
    q.push("SELECT count(*), sum(a), min(a), max(a) FROM t WHERE a > 1000;".into());
    q.push("SELECT g, count(*) FROM t WHERE a > 1000 GROUP BY g;".into());

    // 22) group_concat (row order = scan order), GLOB (case-sensitive),
    // self/3-way joins, DISTINCT on expressions, deeper nesting.
    q.push("SELECT g, group_concat(id) FROM t GROUP BY g ORDER BY g;".into());
    q.push(
        "SELECT g, group_concat(s, ',') FROM t WHERE s IS NOT NULL GROUP BY g ORDER BY g;".into(),
    );
    for pat in ["'str*'", "'*2'", "'str?'", "'STR1'", "'str[12]'"] {
        q.push(format!("SELECT id FROM t WHERE s GLOB {pat} ORDER BY id;"));
    }
    q.push("SELECT DISTINCT a%3, g FROM t ORDER BY 1, 2;".into());
    q.push("SELECT x.id, y.id FROM t x JOIN t y ON x.g = y.g AND x.id < y.id WHERE x.id < 5 ORDER BY x.id, y.id;".into());
    q.push("SELECT t.id, u.w FROM t JOIN u ON t.id = u.t_id JOIN t t2 ON t2.id = u.w ORDER BY t.id, u.w;".into());
    q.push("SELECT id, upper(substr(replace(s,'str','q'),1,2)) FROM t WHERE s IS NOT NULL ORDER BY id;".into());
    q.push("SELECT id FROM t WHERE (a+b) > 20 AND b IS NOT NULL ORDER BY id;".into());
    q.push("SELECT id FROM t WHERE abs(b) = 6 ORDER BY id;".into());
    q.push("SELECT count(*) FROM t WHERE s LIKE 'STR%';".into());
    q.push("SELECT id, coalesce(b, -100) FROM t ORDER BY coalesce(b,-100), id;".into());

    // 23) Compound queries: UNION / UNION ALL / INTERSECT / EXCEPT.
    q.push("SELECT g FROM t WHERE a < 20 UNION SELECT g FROM t WHERE a > 40 ORDER BY g;".into());
    q.push("SELECT a FROM t WHERE g = 1 UNION ALL SELECT a FROM t WHERE g = 2 ORDER BY a;".into());
    q.push(
        "SELECT g FROM t WHERE a < 30 INTERSECT SELECT g FROM t WHERE a > 10 ORDER BY g;".into(),
    );
    q.push("SELECT g FROM t EXCEPT SELECT g FROM t WHERE a < 30 ORDER BY g;".into());
    q.push("SELECT id FROM t WHERE g = 0 UNION SELECT id FROM t WHERE g = 1 UNION SELECT id FROM t WHERE g = 2 ORDER BY id;".into());
    q.push("SELECT a FROM t UNION SELECT t_id FROM u ORDER BY a LIMIT 10;".into());
    q.push(
        "SELECT g, count(*) FROM t GROUP BY g UNION ALL SELECT 99, sum(a) FROM t ORDER BY 1;"
            .into(),
    );
    // Compound dedup keeps the last occurrence's representation across types.
    q.push("SELECT 1 UNION SELECT 1.0;".into());
    q.push("SELECT 1.0 UNION SELECT 1;".into());
    q.push("SELECT 5 UNION SELECT 5.5 UNION SELECT 5.0 ORDER BY 1;".into());
    q.push("SELECT a FROM t WHERE a<3 UNION SELECT a*1.0 FROM t WHERE a<3 ORDER BY 1;".into());
    // IS TRUE/FALSE truthiness and abs() of text (REAL result).
    for e in [
        "2 IS TRUE",
        "0 IS TRUE",
        "NULL IS TRUE",
        "2 IS FALSE",
        "0 IS FALSE",
        "2 IS NOT TRUE",
        "NULL IS NOT FALSE",
        "-5 IS TRUE",
        "'' IS TRUE",
        "abs('5')",
        "abs('5xy')",
        "abs(-5)",
        "abs('-3.2abc')",
        "abs(5.5)",
        "printf('%*d', 5, 3)",
        "printf('%.*f', 2, 3.14159)",
        "printf('%-*d|', 4, 7)",
        "printf('%*d', -5, 3)",
        "sign('abc')",
        "sign('5')",
        "sign('-3.2')",
        "quote(unhex('48-49', '-'))",
        "quote(unhex('48 49', ' '))",
    ] {
        q.push(format!("SELECT {e};"));
    }

    // 24) Window functions over the dataset (rank/aggregates/offset + frames).
    q.push("SELECT id, row_number() OVER (ORDER BY a, id) FROM t ORDER BY id;".into());
    q.push("SELECT id, rank() OVER (PARTITION BY g ORDER BY b) FROM t ORDER BY id;".into());
    q.push("SELECT id, dense_rank() OVER (PARTITION BY g ORDER BY b) FROM t ORDER BY id;".into());
    q.push("SELECT id, sum(a) OVER (PARTITION BY g ORDER BY id) FROM t ORDER BY id;".into());
    q.push("SELECT id, avg(a) OVER (PARTITION BY g) FROM t ORDER BY id;".into());
    q.push("SELECT id, count(*) OVER (PARTITION BY g) FROM t ORDER BY id;".into());
    q.push("SELECT id, lag(a) OVER (ORDER BY id) FROM t ORDER BY id;".into());
    q.push("SELECT id, lead(a, 2, -1) OVER (ORDER BY id) FROM t ORDER BY id;".into());
    q.push(
        "SELECT id, sum(a) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t ORDER BY id;"
            .into(),
    );
    q.push("SELECT id, ntile(4) OVER (ORDER BY id) FROM t ORDER BY id;".into());

    // 25) Derived tables and correlated subqueries / EXISTS over the dataset.
    q.push("SELECT count(*) FROM (SELECT a FROM t WHERE a > 20);".into());
    q.push(
        "SELECT sub.g, sub.c FROM (SELECT g, count(*) AS c FROM t GROUP BY g) AS sub ORDER BY sub.g;"
            .into(),
    );
    q.push(
        "SELECT t.id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id) ORDER BY t.id;"
            .into(),
    );
    q.push(
        "SELECT t.id, (SELECT count(*) FROM u WHERE u.t_id = t.id) FROM t ORDER BY t.id;".into(),
    );
    q.push(
        "SELECT id FROM t WHERE a > (SELECT avg(w) FROM u WHERE u.t_id = t.id) ORDER BY id;".into(),
    );

    // 26) Real-valued expressions (now that formatting matches sqlite's %.15g).
    q.push("SELECT id, a * 1.0 / 3 FROM t WHERE id <= 6 ORDER BY id;".into());
    q.push("SELECT avg(a), avg(b) FROM t;".into());
    q.push("SELECT g, avg(a) FROM t GROUP BY g ORDER BY g;".into());

    // 25) Planner seek paths over indexed columns (a, g, s) and the rowid:
    // OR-of-seekables (same/different index), IN, multi-range, AND-in-OR. Each is
    // also checked for `count(*)` to exercise the aggregate path over the seek.
    let or_preds = [
        "a = 10 OR a = 20",
        "a = 10 OR g = 1",
        "a < 5 OR a > 55",
        "a IN (3, 7, 11) OR g = 2",
        "id = 4 OR a = 30",
        "g = 0 OR g = 2 OR a > 50",
        "(a = 10 AND g = 0) OR a = 40",
        "s = 'str1' OR s = 'str3'",
        "id < 5 OR id > 25",
        "a BETWEEN 10 AND 20 OR a BETWEEN 40 AND 50",
    ];
    for p in or_preds {
        q.push(format!("SELECT id FROM t WHERE {p} ORDER BY id;"));
        q.push(format!("SELECT count(*) FROM t WHERE {p};"));
    }
    // Range + IN seeks over the rowid and indexed columns, with extra predicates
    // that must survive the superset re-filtering.
    for p in [
        "a >= 20 AND a <= 40 AND b IS NOT NULL",
        "id IN (1, 5, 9, 13) AND g = 0",
        "g IN (0, 1) AND a > 25",
        "a > 30 AND s LIKE 'str%'",
        "id BETWEEN 5 AND 15 AND a % 2 = 0",
    ] {
        q.push(format!("SELECT id FROM t WHERE {p} ORDER BY id;"));
    }

    // 26) Scalar-expression edge cases (mixed-type arithmetic and coercion,
    // string-function bounds, rounding, CAST corners) evaluated as constants.
    for e in [
        // Mixed-type arithmetic: text operands coerce to numbers (or 0).
        "3 + '4'",
        "'10' - 2",
        "'3.5' * 2",
        "'abc' + 1",
        "2 + '5xyz'",
        "10 / 4",
        "10 / 4.0",
        "10 % 3",
        "-10 % 3",
        "10 % -3",
        "5 / 0",
        "5 % 0",
        "5.0 / 0",
        // Comparison coercions between literals.
        "'2' < '10'",
        "2 < 10",
        "'2' < 10",
        "'a' < 'b'",
        "1 = 1.0",
        "'1' = 1",
        // round / abs / sign edge cases.
        "round(2.5)",
        "round(3.5)",
        "round(-2.5)",
        "round(2.345, 2)",
        "round(2.345, -1)",
        "abs(-9223372036854775807)",
        "sign(-3.2)",
        "sign(0)",
        // substr bounds.
        "substr('hello', 0)",
        "substr('hello', -3, 2)",
        "substr('hello', 2, -1)",
        "substr('hello', 3, 100)",
        "substr('', 1, 1)",
        // instr / replace / trim / upper-lower corners.
        "instr('hello', '')",
        "instr('', 'x')",
        "instr('banana', 'na')",
        "replace('aaa', 'a', 'bb')",
        "replace('abc', '', 'x')",
        "trim('  x  ')",
        "trim('xxhixx', 'x')",
        "ltrim('--y', '-')",
        "rtrim('z--', '-')",
        // hex / quote / typeof / char / unicode.
        "hex('AB')",
        "hex(255)",
        "quote('a''b')",
        "quote(NULL)",
        "quote(3.5)",
        "typeof(1)",
        "typeof(1.0)",
        "typeof('x')",
        "typeof(NULL)",
        "typeof(x'00')",
        "char(72, 105)",
        "unicode('A')",
        // CAST corners.
        "CAST('12abc' AS INTEGER)",
        "CAST('3.9' AS INTEGER)",
        "CAST(3.9 AS INTEGER)",
        "CAST('  42  ' AS INTEGER)",
        "CAST('xyz' AS REAL)",
        "CAST(65 AS TEXT)",
        "CAST(x'4142' AS TEXT)",
        "CAST('1e3' AS REAL)",
        // coalesce / nullif / iif / boolean-ish.
        "coalesce(NULL, 2.0, 3)",
        "nullif('a', 'a')",
        "iif(1 < 2, 'y', 'n')",
        "1 AND 2",
        "0 OR '3'",
        "NOT 'abc'",
        "NOT ''",
        // printf / format.
        "printf('%d-%s', 5, 'x')",
        "printf('%.2f', 3.14159)",
        "printf('%5d', 42)",
        "format('%x', 255)",
        // JSONB: the binary encoding (via hex) and round-trips through json().
        "hex(jsonb('null'))",
        "hex(jsonb('42'))",
        "hex(jsonb('-7'))",
        "hex(jsonb('\"hi\"'))",
        "hex(jsonb('\"a\\\"b\"'))",
        "hex(jsonb('[1,2,3]'))",
        "hex(jsonb('{\"a\":1,\"bb\":[2,3]}'))",
        "hex(jsonb_array(1, 2, 3))",
        "hex(jsonb_object('a', 1, 'b', 2))",
        "json(jsonb('{\"a\":1,\"b\":[2,3,\"x\"]}'))",
        "json(jsonb_set('{\"a\":1}', '$.b', 9))",
        "json(jsonb_remove('{\"a\":1,\"b\":2}', '$.a'))",
        "json(jsonb_patch('{\"a\":1}', '{\"b\":2}'))",
        "json_extract(jsonb('{\"a\":10,\"b\":[20,30]}'), '$.b[1]')",
        "json_type(jsonb('[1,2,3]'))",
        "json_array_length(jsonb('[1,2,3,4]'))",
        "json(jsonb_object('a', 1, 'b', jsonb_array(2, 3)))",
        // JSON preserves a number's verbatim source text in text output.
        "json('1e2')",
        "json('1E10')",
        "json('-0.0')",
        "json('[1e2, 1.50, 100, 2.0, 1.5e3]')",
        "json('{\"a\":1e2,\"b\":2.50}')",
        "hex(jsonb('1e10'))",
        "hex(jsonb('1.50'))",
        "json(jsonb('[1e2,2.50]'))",
        // ...but an extracted scalar is the canonical SQL value.
        "json_extract('[1e2]', '$[0]')",
    ] {
        q.push(format!("SELECT {e};"));
    }

    // 27) Date/time functions with fixed (deterministic) inputs.
    for e in [
        "date('2024-02-29')",
        "date('2024-01-31', '+1 month')",
        "date('2024-03-31', '-1 month')",
        "date('2024-02-29', '+1 year')",
        "time('2024-01-01 13:45:30')",
        "datetime('2024-06-15 08:30:00', '+90 minutes')",
        "datetime('2024-06-15', '+1 day', '-2 hours')",
        "strftime('%Y-%m-%d %H:%M', '2024-06-15 08:30:00')",
        "strftime('%j', '2024-12-31')",
        "strftime('%j', '2024-03-09 14:05:07')",
        "strftime('%j', '2024-01-01')",
        "strftime('%w', '2024-06-16')",
        // ISO 8601 week-date specifiers, including year-boundary cases.
        "strftime('%G-W%V-%u', '2024-03-09')",
        "strftime('%G-W%V-%u', '2021-01-01')",
        "strftime('%G-W%V-%u', '2020-12-31')",
        "strftime('%G-W%V-%u', '2016-01-01')",
        "strftime('%g %V %G', '2026-01-01')",
        "julianday('2000-01-01')",
        "date('2024-06-15', 'start of month')",
        "date('2024-06-15', 'weekday 0')",
        "strftime('%s', '1970-01-02 00:00:00')",
        "datetime(0, 'unixepoch')",
        "datetime(86400, 'unixepoch')",
    ] {
        q.push(format!("SELECT {e};"));
    }
    // 28) Date/time validation and overflow normalization (invalid -> NULL;
    // day-overflow normalized via the Julian-day round-trip; hour 24 preserved).
    for e in [
        "quote(date('2024-02-30'))",
        "quote(date('2024-02-31'))",
        "quote(date('2024-04-31'))",
        "quote(date('2024-13-01'))",
        "quote(date('2024-00-15'))",
        "quote(date('2024-01-32'))",
        "quote(time('25:00'))",
        "quote(time('23:60'))",
        "quote(time('23:59:60'))",
        "quote(time('24:00:00'))",
        "quote(time('24:00:30'))",
        "quote(datetime('2024-02-30 12:00'))",
        "quote(datetime('2024-02-30 25:00'))",
        "quote(strftime('%H:%M:%S','2024-06-15 25:61:00'))",
        "quote(datetime('2024-02-29 23:59:59','+1 second'))",
    ] {
        q.push(format!("SELECT {e};"));
    }
    // 29) Numeric-literal digit separators (SQLite 3.46+) and hex literals.
    for e in [
        "1_000",
        "1_000_000",
        "0xFF_FF",
        "1_000.0",
        "1_0e3",
        "0x1F",
        "0xABCDEF",
        "1_000 + 2_000",
        "1_2.3_4",
    ] {
        q.push(format!("SELECT {e};"));
    }
    // 30) Negative LIMIT means "no limit" (OFFSET still applies).
    for lo in [
        "LIMIT -1",
        "LIMIT -5",
        "LIMIT -1 OFFSET 3",
        "LIMIT -1 OFFSET 100",
        "LIMIT 0",
    ] {
        q.push(format!("SELECT id FROM t ORDER BY id {lo};"));
    }
    q.push(
        "SELECT id FROM t WHERE g=1 UNION SELECT id FROM t WHERE g=2 ORDER BY id LIMIT -1;".into(),
    );
    // 31) printf/format specifiers: %g notation, half-away %f rounding, sign flags.
    for e in [
        "printf('%g',1000000)",
        "printf('%g',100000)",
        "printf('%g',0.0001)",
        "printf('%g',0.00001)",
        "printf('%.3g',3.14159)",
        "printf('%g',1234.5)",
        "printf('%G',123456789)",
        "printf('%.0f',2.5)",
        "printf('%.0f',0.5)",
        "printf('%+.2f',3.5)",
        "printf('% d',5)",
        "printf('%+d',5)",
        "printf('%.2f',2.675)",
        "printf('%08.3f',3.14)",
        "printf('%5.2f',3.14159)",
        "printf('%e',12345.678)",
        "printf('%g',0.1)",
    ] {
        q.push(format!("SELECT {e};"));
    }
    // 32) CAST to sized / multi-word type names (the size is ignored; affinity
    // comes from the type keyword).
    for e in [
        "CAST(3.7 AS VARCHAR(10))",
        "CAST(3.7 AS DECIMAL(10,2))",
        "CAST('5' AS INT8)",
        "CAST(123 AS CHARACTER(5))",
        "CAST(5 AS UNSIGNED BIG INT)",
        "CAST('3.0' AS DECIMAL(10,2))",
        "typeof(CAST(5.5 AS DECIMAL(4,1)))",
        "typeof(CAST('7' AS NUMERIC(3)))",
    ] {
        q.push(format!("SELECT {e};"));
    }

    q
}

#[test]
fn differential_against_sqlite3() {
    if !sqlite_available() {
        eprintln!("sqlite3 CLI not found; skipping differential suite");
        return;
    }

    // Build the sqlite reference database.
    let mut sqlite_path = std::env::temp_dir();
    sqlite_path.push(format!("graphitesql-diff-{}.db", std::process::id()));
    let sqlite_path = sqlite_path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&sqlite_path);
    let mut script = String::from(SETUP);
    for ins in dataset_inserts() {
        script.push_str(&ins);
    }
    let out = Command::new("sqlite3")
        .arg(&sqlite_path)
        .arg(&script)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sqlite setup: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Build the identical graphitesql database.
    let mut gdb = Connection::open_memory().unwrap();
    for stmt in SETUP.split(';') {
        if !stmt.trim().is_empty() {
            gdb.execute(stmt).unwrap();
        }
    }
    for ins in dataset_inserts() {
        gdb.execute(ins.trim_end_matches(';')).unwrap();
    }

    let queries = corpus();
    let total = queries.len();
    let mut passed = 0;
    let mut failures = Vec::new();

    for sql in &queries {
        let want = {
            let out = Command::new("sqlite3")
                .arg(&sqlite_path)
                .arg(sql)
                .output()
                .unwrap();
            if !out.status.success() {
                // Skip cases the reference itself rejects (keeps the corpus honest).
                continue;
            }
            String::from_utf8_lossy(&out.stdout).trim_end().to_string()
        };
        match gdb.query(sql.trim_end_matches(';')) {
            Ok(r) => {
                let got = render(&r);
                if got.trim_end() == want {
                    passed += 1;
                } else {
                    failures.push(format!(
                        "SQL: {sql}\n  sqlite: {want:?}\n  graphite: {got:?}"
                    ));
                }
            }
            Err(e) => failures.push(format!("SQL: {sql}\n  graphite error: {e}")),
        }
    }

    let _ = std::fs::remove_file(&sqlite_path);

    eprintln!("differential: {passed}/{total} queries matched sqlite3");
    if !failures.is_empty() {
        let shown: Vec<String> = failures.iter().take(20).cloned().collect();
        panic!(
            "{} of {} differential queries diverged from sqlite3:\n{}",
            failures.len(),
            total,
            shown.join("\n")
        );
    }
}
