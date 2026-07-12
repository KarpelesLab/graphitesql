//! SQLite's "bare column" rule: a grouped query with exactly one min()/max()
//! takes bare (non-aggregate, non-grouped) columns from the row achieving that
//! extreme. Checked against sqlite3. (Queries with two min/max are undefined and
//! excluded.)

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

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

#[test]
fn bare_column_minmax_matches_sqlite3() {
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-mm-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, g INT, v INT, name TEXT);\
        INSERT INTO t(g,v,name) VALUES \
          (1,10,'a'),(1,30,'b'),(1,20,'c'),(2,5,'d'),(2,50,'e'),(3,NULL,'f');";
    {
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg(setup)
            .output()
            .unwrap();
        assert!(o.status.success());
    }
    let mut g = Connection::open_memory().unwrap();
    for s in setup.split(';') {
        if !s.trim().is_empty() {
            g.execute(s).unwrap();
        }
    }
    let queries = [
        "SELECT g, max(v), name FROM t GROUP BY g ORDER BY g",
        "SELECT g, min(v), name FROM t GROUP BY g ORDER BY g",
        "SELECT max(v), name FROM t",
        "SELECT min(v), name FROM t",
        "SELECT max(v), count(*), name FROM t",
        "SELECT max(v)+1, name FROM t",
        "SELECT g, max(v) AS m, name, id FROM t GROUP BY g ORDER BY g",
        "SELECT name, v FROM t GROUP BY g HAVING max(v)=v ORDER BY g",
        "SELECT g, min(v), name FROM t WHERE v IS NOT NULL GROUP BY g ORDER BY g",
        // HAVING may reference SELECT-output aliases.
        "SELECT g, sum(v) s FROM t GROUP BY g HAVING s > 30 ORDER BY g",
        "SELECT g, count(*) c FROM t GROUP BY g HAVING c >= 2 ORDER BY g",
        "SELECT g AS grp, max(v) m FROM t GROUP BY g HAVING m > 20 AND grp < 3 ORDER BY g",
    ];
    for q in queries {
        let want = {
            let o = Command::new("sqlite3")
                .arg(&path)
                .arg(format!("{q};"))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        let got = render(&g.query(q).unwrap());
        assert_eq!(got, want, "min/max bare-column diverged: {q}");
    }
    let _ = std::fs::remove_file(&path);
}
