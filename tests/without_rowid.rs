//! Phase 9: WITHOUT ROWID tables (PK-clustered index b-tree storage).
//!
//! The decisive gate is real `sqlite3`'s `PRAGMA integrity_check` on a database
//! graphitesql wrote, plus round-trip agreement on the rows.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-wr-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite3_run(path: &str, sql: &str) -> String {
    let out = Command::new("sqlite3").arg(path).arg(sql).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn ints(c: &Connection, sql: &str) -> Vec<i64> {
    c.query(sql)
        .unwrap()
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Integer(v) => v,
            ref o => panic!("not int: {o:?}"),
        })
        .collect()
}

#[test]
fn basic_crud_in_memory() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(k TEXT PRIMARY KEY, v INT) WITHOUT ROWID")
        .unwrap();
    c.execute("INSERT INTO t VALUES ('c',3),('a',1),('b',2)")
        .unwrap();
    // Stored in PK order regardless of insert order.
    let r = c.query("SELECT k FROM t").unwrap();
    let ks: Vec<String> = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Text(s) => s.clone(),
            o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(ks, vec!["a", "b", "c"]);

    // Duplicate PK rejected.
    assert!(c.execute("INSERT INTO t VALUES ('a', 9)").is_err());

    // Update and delete.
    c.execute("UPDATE t SET v = v * 10 WHERE k = 'b'").unwrap();
    assert_eq!(ints(&c, "SELECT v FROM t WHERE k = 'b'"), vec![20]);
    c.execute("DELETE FROM t WHERE k = 'a'").unwrap();
    assert_eq!(ints(&c, "SELECT count(*) FROM t"), vec![2]);

    // INSERT OR REPLACE on the PK.
    c.execute("INSERT OR REPLACE INTO t VALUES ('b', 99)")
        .unwrap();
    assert_eq!(ints(&c, "SELECT v FROM t WHERE k = 'b'"), vec![99]);
}

#[test]
fn composite_pk() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE m(a INT, b INT, note TEXT, PRIMARY KEY(a,b)) WITHOUT ROWID")
        .unwrap();
    c.execute("INSERT INTO m VALUES (2,1,'x'),(1,2,'y'),(1,1,'z')")
        .unwrap();
    // Ordered by (a,b).
    let r = c.query("SELECT a,b FROM m").unwrap();
    let pairs: Vec<(i64, i64)> = r
        .rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Integer(a), Value::Integer(b)) => (*a, *b),
            _ => panic!(),
        })
        .collect();
    assert_eq!(pairs, vec![(1, 1), (1, 2), (2, 1)]);
    // Same (a,b) rejected; different b allowed.
    assert!(c.execute("INSERT INTO m VALUES (1,1,'dup')").is_err());
    c.execute("INSERT INTO m VALUES (1,3,'ok')").unwrap();
    assert_eq!(ints(&c, "SELECT count(*) FROM m"), vec![4]);
}

#[test]
fn integrity_and_roundtrip_vs_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_path("wr.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(k TEXT PRIMARY KEY, n INT, s TEXT) WITHOUT ROWID")
            .unwrap();
        for i in 1..=60i64 {
            c.execute(&format!(
                "INSERT INTO t VALUES ('k{:03}', {}, 'v{}')",
                (60 - i),
                i,
                i % 5
            ))
            .unwrap();
        }
        c.execute("DELETE FROM t WHERE n % 7 = 0").unwrap();
        c.execute("UPDATE t SET s = 'upd' WHERE n < 10").unwrap();
    }
    // The decisive gate.
    assert_eq!(sqlite3_run(&path, "PRAGMA integrity_check;"), "ok");
    // It really is a WITHOUT ROWID table: its sole "index" is the PK itself.
    assert_eq!(
        sqlite3_run(&path, "SELECT origin FROM pragma_index_list('t');"),
        "pk"
    );
    // Row sets agree.
    let want = sqlite3_run(&path, "SELECT k,n,s FROM t ORDER BY k;");
    let got = {
        let c = Connection::open_readonly(&path).unwrap();
        let r = c.query("SELECT k,n,s FROM t ORDER BY k").unwrap();
        r.rows
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
    };
    assert_eq!(got, want);
    cleanup(&path);
}

#[test]
fn secondary_index_integrity() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_path("wr-idx.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(k TEXT PRIMARY KEY, v INT) WITHOUT ROWID")
            .unwrap();
        c.execute("CREATE INDEX iv ON t(v)").unwrap();
        for i in 1..=30i64 {
            c.execute(&format!(
                "INSERT INTO t VALUES ('k{:02}', {})",
                30 - i,
                i % 6
            ))
            .unwrap();
        }
        c.execute("DELETE FROM t WHERE v = 0").unwrap();
        c.execute("UPDATE t SET v = v + 100 WHERE k = 'k05'")
            .unwrap();
    }
    // The secondary index b-tree on a WITHOUT ROWID table (keyed by (v, k))
    // must be consistent with the table per real sqlite3.
    assert_eq!(sqlite3_run(&path, "PRAGMA integrity_check;"), "ok");
    // And it's used by sqlite to answer a lookup correctly.
    let want = sqlite3_run(&path, "SELECT count(*) FROM t WHERE v = 3;");
    let got = {
        let c = Connection::open_readonly(&path).unwrap();
        match c.query("SELECT count(*) FROM t WHERE v = 3").unwrap().rows[0][0] {
            Value::Integer(n) => n.to_string(),
            _ => panic!(),
        }
    };
    assert_eq!(got, want);
    cleanup(&path);
}

#[test]
fn unique_constraint_on_wr() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_path("wr-uniq.db");
    cleanup(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(k TEXT PRIMARY KEY, email TEXT UNIQUE, n INT) WITHOUT ROWID")
            .unwrap();
        c.execute("INSERT INTO t VALUES ('a','a@x',1),('b','b@x',2)")
            .unwrap();
        // Duplicate UNIQUE email is rejected.
        assert!(c.execute("INSERT INTO t VALUES ('c','a@x',3)").is_err());
        // NULL email is allowed and distinct.
        c.execute("INSERT INTO t VALUES ('d',NULL,4),('e',NULL,5)")
            .unwrap();
        // Update into a conflicting email is rejected.
        assert!(c.execute("UPDATE t SET email='b@x' WHERE k='a'").is_err());
    }
    // The implied sqlite_autoindex for the UNIQUE column must be consistent.
    assert_eq!(sqlite3_run(&path, "PRAGMA integrity_check;"), "ok");
    assert_eq!(
        sqlite3_run(
            &path,
            "SELECT name FROM sqlite_master WHERE type='index' ORDER BY name;"
        ),
        "sqlite_autoindex_t_2"
    );
    // sqlite enforces the UNIQUE via the index we wrote.
    assert!(!Command::new("sqlite3")
        .arg(&path)
        .arg("INSERT INTO t VALUES ('z','a@x',9);")
        .output()
        .unwrap()
        .status
        .success());
    cleanup(&path);
}

/// graphitesql must also read a WITHOUT ROWID database written by real sqlite3.
#[test]
fn reads_sqlite_written_without_rowid() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_path("wr-sqlite.db");
    cleanup(&path);
    sqlite3_run(
        &path,
        "CREATE TABLE t(a INT, b INT, c TEXT, PRIMARY KEY(b)) WITHOUT ROWID;\
         INSERT INTO t VALUES (11,22,'x'),(44,55,'y'),(33,10,'z');",
    );
    let c = Connection::open_readonly(&path).unwrap();
    let r = c.query("SELECT a,b,c FROM t ORDER BY b").unwrap();
    // Ordered by b: 10,22,55 -> rows (33,10,z),(11,22,x),(44,55,y)
    assert_eq!(r.rows[0][0], Value::Integer(33));
    assert_eq!(r.rows[0][1], Value::Integer(10));
    assert_eq!(r.rows[0][2], Value::Text("z".into()));
    assert_eq!(r.rows[2][1], Value::Integer(55));
    cleanup(&path);
}
