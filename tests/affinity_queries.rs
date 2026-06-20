//! Differential testing over a table with mixed column affinities (typeless,
//! NUMERIC, REAL) — the areas where storage/comparison coercion is subtle. Every
//! query is compared byte-for-byte with real sqlite3.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3(path: &str, sql: &str) -> String {
    let o = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
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
                    Value::Text(s) => s.clone(),
                    Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                    Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn mixed_affinity_queries_match_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-affq-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    // any=typeless (NONE), num=NUMERIC, rl=REAL, txt=TEXT.
    let setup = "CREATE TABLE m(id INTEGER PRIMARY KEY, any, num NUMERIC, rl REAL, txt TEXT);\
        INSERT INTO m(any,num,rl,txt) VALUES (1,'1',1,'1');\
        INSERT INTO m(any,num,rl,txt) VALUES ('2',2,'2.0',2);\
        INSERT INTO m(any,num,rl,txt) VALUES (3.0,'3.5',3,'3');\
        INSERT INTO m(any,num,rl,txt) VALUES ('4.0','4',4.5,'four');\
        INSERT INTO m(any,num,rl,txt) VALUES (5,5,5,5);\
        INSERT INTO m(any,num,rl,txt) VALUES ('x',NULL,NULL,NULL);\
        INSERT INTO m(any,num,rl,txt) VALUES (NULL,'10',10,'10');";
    sqlite3(&path, setup);
    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }

    let queries = [
        // WHERE comparisons across affinities and literals.
        "SELECT id FROM m WHERE any = 1 ORDER BY id",
        "SELECT id FROM m WHERE any = '1' ORDER BY id",
        "SELECT id FROM m WHERE num = 2 ORDER BY id",
        "SELECT id FROM m WHERE num = '2' ORDER BY id",
        "SELECT id FROM m WHERE rl = 2 ORDER BY id",
        "SELECT id FROM m WHERE txt = 2 ORDER BY id",
        "SELECT id FROM m WHERE txt = '2' ORDER BY id",
        "SELECT id FROM m WHERE any = txt ORDER BY id",
        "SELECT id FROM m WHERE num = rl ORDER BY id",
        "SELECT id FROM m WHERE any > 2 ORDER BY id",
        "SELECT id FROM m WHERE num > 2 ORDER BY id",
        "SELECT id FROM m WHERE txt > 2 ORDER BY id",
        // typeof / value projections.
        "SELECT id, typeof(any), typeof(num), typeof(rl), typeof(txt) FROM m ORDER BY id",
        "SELECT id, any, num, rl, txt FROM m ORDER BY id",
        // ORDER BY each (storage-class ordering).
        "SELECT id FROM m ORDER BY any, id",
        "SELECT id FROM m ORDER BY num, id",
        "SELECT id FROM m ORDER BY txt, id",
        // DISTINCT / GROUP BY / aggregates over mixed types.
        "SELECT DISTINCT num FROM m ORDER BY num",
        "SELECT num, count(*) FROM m GROUP BY num ORDER BY num",
        "SELECT sum(any), sum(num), sum(rl), avg(num), total(rl) FROM m",
        "SELECT max(any), min(any), max(txt), min(txt) FROM m",
        "SELECT id FROM m WHERE num IN (1,2,5) ORDER BY id",
        "SELECT id FROM m WHERE any IN ('2', 5) ORDER BY id",
        // arithmetic forcing numeric coercion of mixed storage.
        "SELECT id, any + 0, num * 2, txt + 1 FROM m WHERE num IS NOT NULL ORDER BY id",
    ];
    for q in queries {
        let want = sqlite3(&path, &format!("{q};"));
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "mixed-affinity query diverged: {q}");
    }
    let _ = std::fs::remove_file(&path);
}
