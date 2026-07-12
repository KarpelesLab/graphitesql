//! Track B / B7b regression coverage: with the VDBE now the **default** `SELECT`
//! engine, these lock in the correctness fixes that flipping the default
//! surfaced. Each runs through `Connection::query()` (VDBE-first, tree-walker
//! fallback) and is compared byte-for-byte against `sqlite3`.

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

fn graphite(c: &Connection, sql: &str) -> String {
    // `trim_end` mirrors how the sqlite reference output is captured (its CLI
    // drops a trailing blank line from a final NULL-only row).
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

#[test]
fn vdbe_default_matches_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT, c REAL);\
                 INSERT INTO t VALUES(1,5,'x',1.5),(2,3,'y',2.5),(3,5,'x',3.5),(4,NULL,'z',NULL);\
                 CREATE INDEX ic ON t(c);\
                 CREATE TABLE u(uid INTEGER PRIMARY KEY, tid INT, w INT);\
                 INSERT INTO u VALUES(1,1,10),(2,1,20),(3,2,30)";
    let path = std::env::temp_dir().join(format!("gsql-vdbedef-{}.db", std::process::id()));
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

    // Each query is answered by the VDBE engine (the default) or falls back; the
    // result must be byte-identical to sqlite3 either way. The comments name the
    // B7b fix each shape exercises.
    let queries = [
        // blob `||` concatenation: bytes joined, blob when not valid UTF-8.
        "SELECT hex(x'00' || x'ff'), hex('a' || x'00' || 'b')",
        // negating i64::MIN promotes to a real.
        "SELECT -(-9223372036854775808), typeof(-(-9223372036854775808))",
        // ORDER BY whose order comes from an index scan (ties/NULLs follow it).
        "SELECT a FROM t ORDER BY c",
        "SELECT a FROM t ORDER BY c DESC",
        // bare aggregate, GROUP BY, HAVING (with/without GROUP BY).
        "SELECT count(*), sum(a), min(c), max(c) FROM t",
        "SELECT b, count(*) FROM t GROUP BY b ORDER BY b",
        "SELECT b, sum(a) FROM t GROUP BY b HAVING sum(a) > 3 ORDER BY b",
        "SELECT count(*) FROM t HAVING count(*) > 0",
        // a bare non-grouped column in a grouped query (representative-row).
        "SELECT b, count(*), a FROM t GROUP BY b ORDER BY b",
        // joins (inner + comma) and a correlated subquery.
        "SELECT t.id, u.w FROM t JOIN u ON t.id = u.tid ORDER BY t.id, u.w",
        "SELECT id, (SELECT sum(w) FROM u WHERE tid = t.id) FROM t ORDER BY id",
        // compound (each arm VDBE-routed, tree-walker combines).
        "SELECT a FROM t WHERE a IS NOT NULL UNION SELECT w FROM u ORDER BY 1",
        // DISTINCT, IN, BETWEEN, CASE, coalesce.
        "SELECT DISTINCT a FROM t ORDER BY a",
        "SELECT id FROM t WHERE b IN ('x','y') ORDER BY id",
        "SELECT coalesce(a,-1), CASE WHEN c IS NULL THEN 'n' ELSE 'y' END FROM t ORDER BY id",
    ];
    let mut failures = Vec::new();
    for q in queries {
        let want = {
            let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = graphite(&g, q);
        if got != want {
            failures.push(format!(
                "  {q}\n    sqlite:   {want:?}\n    graphite: {got:?}"
            ));
        }
    }
    let _ = std::fs::remove_file(&path);
    assert!(
        failures.is_empty(),
        "{} VDBE-default queries diverged from sqlite3:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn vdbe_default_rejects_like_sqlite3() {
    // Shapes the VDBE must defer to the tree-walker so the error is still raised
    // (rather than the VDBE silently accepting them): an out-of-range positional
    // ORDER BY, and a HAVING on a non-aggregate query.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT, b INT)").unwrap();
    c.execute("INSERT INTO t VALUES(1,2),(3,4)").unwrap();
    assert!(c.query("SELECT a FROM t ORDER BY 5").is_err());
    assert!(c.query("SELECT a FROM t HAVING a > 0").is_err());
}
