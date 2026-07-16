//! NOCASE collation folds ASCII letters to *lower* case (SQLite's
//! `sqlite3UpperToLower[]` / `nocaseCollatingFunc`), so keys mixing letters
//! with the punctuation bytes `[ \ ] ^ _ ` ` (0x5B..=0x60) order the same as
//! sqlite, and NOCASE indexes written by graphite are byte-consistent with
//! sqlite's (integrity/quick_check clean, indexed lookups return sqlite's
//! rows). Verified differentially against sqlite3 3.50.4.

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

/// Keys that mix mixed-case words, the 0x5B..=0x60 punctuation bytes, and
/// digits — precisely where an upper-fold vs lower-fold NOCASE would diverge.
const KEYS: &[&str] = &[
    "Apple", "apple", "ZEBRA", "zebra", "[bracket", "]close", "^caret", "_under", "`tick",
    "\\back", "0zero", "9nine", "Ant", "ant", "[", "]", "_", "`", "A", "a", "Z", "z", "0", "9",
];

#[test]
fn nocase_order_matches_sqlite() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let mut setup = vec!["CREATE TABLE t(id INTEGER PRIMARY KEY, x TEXT)".to_string()];
    for (i, k) in KEYS.iter().enumerate() {
        // Single-quote escaping: none of KEYS contain a quote.
        setup.push(format!("INSERT INTO t(id,x) VALUES ({}, '{}')", i, k));
    }

    let path = std::env::temp_dir().join(format!("gsql-nocaseord-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let out = Command::new("sqlite3")
        .arg(&path)
        .arg(setup.join(";"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut g = Connection::open_memory().unwrap();
    for s in &setup {
        g.execute(s).unwrap();
    }

    // Tie-break on id so the row order is fully determined on both engines.
    let queries = [
        "SELECT x FROM t ORDER BY x COLLATE NOCASE, id",
        "SELECT id FROM t WHERE x < 'a' COLLATE NOCASE ORDER BY id",
        "SELECT id FROM t WHERE x >= '[' COLLATE NOCASE AND x < 'b' COLLATE NOCASE ORDER BY id",
        "SELECT count(DISTINCT x COLLATE NOCASE) FROM t",
        "SELECT x FROM t WHERE x = 'apple' COLLATE NOCASE ORDER BY id",
    ];
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
        "{} NOCASE-order queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn nocase_index_written_by_graphite_is_sqlite_consistent() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-nocaseidx-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        // A NOCASE PRIMARY KEY (its index orders NOCASE) plus a secondary
        // NOCASE index over a column full of the punctuation-byte keys.
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(k TEXT COLLATE NOCASE PRIMARY KEY, v TEXT)")
            .unwrap();
        c.execute("CREATE INDEX iv ON t(v COLLATE NOCASE)").unwrap();
        // Deduplicate so the NOCASE PK does not collide (case-insensitively).
        let uniq = [
            "Apple", "ZEBRA", "[bracket", "]close", "^caret", "_under", "`tick", "0zero", "Ant",
            "[", "_",
        ];
        for (i, k) in uniq.iter().enumerate() {
            c.execute(&format!("INSERT INTO t(k,v) VALUES ('{k}', 'V{i}')"))
                .unwrap();
        }
        // graphite's own integrity_check must pass.
        let ic = c.query("PRAGMA integrity_check").unwrap();
        assert_eq!(render(&ic.rows[0][0]), "ok");
    }
    // Real sqlite3 must agree the file (incl. both NOCASE indexes) is sound.
    for pragma in ["PRAGMA integrity_check;", "PRAGMA quick_check;"] {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg(pragma)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "ok",
            "{pragma} failed"
        );
    }
    // An indexed NOCASE range on graphite returns exactly sqlite's rows.
    let q = "SELECT k FROM t WHERE k >= '[' AND k < 'b' ORDER BY k";
    let want = {
        let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    };
    let c = Connection::open_readonly(&path).unwrap();
    assert_eq!(rows_str(&c, q), want);
    // Case-variant equality still finds the row through the NOCASE PK index.
    assert_eq!(
        c.query("SELECT count(*) FROM t WHERE k = 'APPLE'")
            .unwrap()
            .rows[0][0],
        Value::Integer(1)
    );
    let _ = std::fs::remove_file(&path);
}
