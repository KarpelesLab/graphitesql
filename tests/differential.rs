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
                Value::Real(r) => format!("{r}"),
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
    ] {
        q.push(format!("SELECT id, {f} FROM t ORDER BY id;"));
    }
    // 7) LIKE / GLOB.
    for pat in ["'str%'", "'%2'", "'str_'", "'STR1'"] {
        q.push(format!("SELECT id FROM t WHERE s LIKE {pat} ORDER BY id;"));
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
