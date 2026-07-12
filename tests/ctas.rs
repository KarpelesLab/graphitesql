//! Track A: `CREATE TABLE … AS SELECT …`. Verified against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => String::from(s.as_str()),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

fn rows_str(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn basic_ctas() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE src(a, b)").unwrap();
    c.execute("INSERT INTO src VALUES (1,'x'),(2,'y'),(3,'z')")
        .unwrap();
    c.execute("CREATE TABLE dst AS SELECT a, b FROM src WHERE a >= 2")
        .unwrap();
    assert_eq!(rows_str(&c, "SELECT a, b FROM dst ORDER BY a"), "2|y\n3|z");
    // Column names come from the SELECT.
    let r = c.query("SELECT * FROM dst LIMIT 0").unwrap();
    assert_eq!(r.columns, vec!["a", "b"]);
    // Aliased/expression columns.
    c.execute("CREATE TABLE agg AS SELECT count(*) AS n, sum(a) AS s FROM src")
        .unwrap();
    assert_eq!(rows_str(&c, "SELECT n, s FROM agg"), "3|6");
}

#[test]
fn against_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE src(a INT, b TEXT, c REAL);\
                 INSERT INTO src VALUES (1,'aa',1.5),(2,'bb',2.5),(3,'cc',3.5);\
                 CREATE TABLE t1 AS SELECT a, b FROM src WHERE a > 1;\
                 CREATE TABLE t2 AS SELECT a*10 AS x, upper(b) AS y FROM src;\
                 CREATE TABLE t3 AS SELECT b, count(*) AS n FROM src GROUP BY (a > 1)";
    let queries = [
        "SELECT a, b FROM t1 ORDER BY a",
        "SELECT x, y FROM t2 ORDER BY x",
        "SELECT count(*) FROM t3",
    ];

    let path = std::env::temp_dir().join(format!("gsql-ctas-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(setup)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = rows_str(&g, q);
        if got != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    let _ = std::fs::remove_file(&path);
    assert!(
        failures.is_empty(),
        "{} CTAS queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The generated CTAS schema text: each column inherits a canonical declared type
/// from the query's output affinity (a direct column ref keeps its source column's
/// affinity — `INT`/`TEXT`/`REAL`/`NUM`, BLOB/none/expression → no type), and the
/// column list is laid out on one line for ≤5 columns but one indented column per
/// line for ≥6 — byte-for-byte like sqlite. The inherited type also gives the new
/// table the right affinity.
#[test]
fn ctas_column_types_and_layout_match_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE s(a INTEGER,b TEXT,c REAL,d BLOB,e NUMERIC,f VARCHAR(10),g);\
                 INSERT INTO s DEFAULT VALUES;\
                 CREATE TABLE t_star AS SELECT * FROM s;\
                 CREATE TABLE t_expr AS SELECT a,b,a+1 AS x,upper(b) AS y,CAST(a AS TEXT) z FROM s;\
                 CREATE TABLE t_small AS SELECT a,b,c FROM s;\
                 CREATE TABLE t_five AS SELECT a,b,c,e,f FROM s;\
                 CREATE VIEW v AS SELECT a,b FROM s;\
                 CREATE TABLE t_view AS SELECT * FROM v;\
                 CREATE TABLE t_lit AS SELECT 1 AS p,'q' AS q,3.5 AS r";
    let names = ["t_star", "t_expr", "t_small", "t_five", "t_view", "t_lit"];

    let path = std::env::temp_dir().join(format!("gsql-ctas-schema-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(setup)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let mut failures = Vec::new();
    for n in names {
        let q = format!("SELECT sql FROM sqlite_master WHERE name='{n}'");
        let want = {
            let o = Command::new("sqlite3").arg(&path).arg(&q).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = rows_str(&g, &q);
        if got != want {
            failures.push(format!(
                "  {n}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    // Affinity of an inherited-type column: an INT column coerces inserted text.
    let aff_q = "CREATE TABLE si(a INTEGER);INSERT INTO si VALUES(5);\
                 CREATE TABLE di AS SELECT a FROM si;INSERT INTO di VALUES('42');\
                 SELECT typeof(a) FROM di WHERE a=42;";
    let want_aff = {
        let o = Command::new("sqlite3")
            .arg(":memory:")
            .arg(aff_q)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    };
    let mut g2 = Connection::open_memory().unwrap();
    for s in aff_q.split(';') {
        if !s.trim().is_empty() {
            let _ = g2.execute(s);
        }
    }
    let got_aff = rows_str(&g2, "SELECT typeof(a) FROM di WHERE a=42");
    if got_aff != want_aff {
        failures.push(format!(
            "  affinity\n    sqlite:   {want_aff:?}\n    graphite: {got_aff:?}"
        ));
    }

    let _ = std::fs::remove_file(&path);
    assert!(
        failures.is_empty(),
        "{} CTAS schema/affinity cases diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn ctas_auto_renames_duplicate_columns() {
    // `CREATE TABLE … AS SELECT` auto-renames duplicate output column names (the
    // 2nd `a` → `a:1`, 3rd → `a:2`) rather than erroring like an explicit column
    // list — matching sqlite. Names compare case-insensitively (original case
    // kept).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t AS SELECT 1 AS a, 2 AS a, 3 AS a, 4 AS b, 5 AS b")
        .unwrap();
    assert_eq!(
        c.query("SELECT * FROM t").unwrap().columns,
        ["a", "a:1", "a:2", "b", "b:1"]
    );
    let r = c.query("SELECT * FROM t").unwrap();
    assert_eq!(
        r.rows[0],
        vec![
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(3),
            Value::Integer(4),
            Value::Integer(5)
        ]
    );
    // Case-insensitive: the second `A` (different case) is still a duplicate.
    let mut c2 = Connection::open_memory().unwrap();
    c2.execute("CREATE TABLE u AS SELECT 1 AS a, 2 AS A")
        .unwrap();
    assert_eq!(c2.query("SELECT * FROM u").unwrap().columns, ["a", "A:1"]);
    // A plain (non-CTAS) duplicate column list is still rejected.
    assert!(c2.execute("CREATE TABLE w(a, a)").is_err());
}
