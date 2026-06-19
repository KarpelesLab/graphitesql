//! Phase 9: foreign-key enforcement (gated on `PRAGMA foreign_keys = ON`).

#![cfg(feature = "std")]

use graphitesql::{Connection, Error};

fn parent_child() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE parent(id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    c.execute("CREATE TABLE child(id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id), v INT)")
        .unwrap();
    c.execute("INSERT INTO parent(id,name) VALUES (1,'a'),(2,'b'),(3,'c')")
        .unwrap();
    c
}

#[test]
fn pragma_toggle() {
    let mut c = Connection::open_memory().unwrap();
    // Off by default, matching SQLite.
    assert_eq!(
        c.query("PRAGMA foreign_keys").unwrap().rows[0][0],
        graphitesql::Value::Integer(0)
    );
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    assert_eq!(
        c.query("PRAGMA foreign_keys").unwrap().rows[0][0],
        graphitesql::Value::Integer(1)
    );
    c.execute("PRAGMA foreign_keys = OFF").unwrap();
    assert_eq!(
        c.query("PRAGMA foreign_keys").unwrap().rows[0][0],
        graphitesql::Value::Integer(0)
    );
}

#[test]
fn insert_requires_parent() {
    let mut c = parent_child();
    // Valid parent reference.
    c.execute("INSERT INTO child(id,pid,v) VALUES (1,1,10)")
        .unwrap();
    // NULL key is allowed.
    c.execute("INSERT INTO child(id,pid,v) VALUES (2,NULL,20)")
        .unwrap();
    // Missing parent is rejected.
    let err = c.execute("INSERT INTO child(id,pid,v) VALUES (3,99,30)");
    assert!(matches!(err, Err(Error::Constraint(_))), "got {err:?}");
}

#[test]
fn disabled_allows_orphan() {
    let mut c = parent_child();
    c.execute("PRAGMA foreign_keys = OFF").unwrap();
    // With enforcement off, an orphan insert succeeds.
    c.execute("INSERT INTO child(id,pid,v) VALUES (3,99,30)")
        .unwrap();
}

#[test]
fn delete_restrict_default() {
    let mut c = parent_child();
    c.execute("INSERT INTO child(id,pid,v) VALUES (1,1,10)")
        .unwrap();
    // NO ACTION (default): deleting a referenced parent fails.
    let err = c.execute("DELETE FROM parent WHERE id = 1");
    assert!(matches!(err, Err(Error::Constraint(_))), "got {err:?}");
    // An unreferenced parent can be deleted.
    c.execute("DELETE FROM parent WHERE id = 2").unwrap();
}

#[test]
fn delete_cascade() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE parent(id INTEGER PRIMARY KEY)")
        .unwrap();
    c.execute(
        "CREATE TABLE child(id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE CASCADE)",
    )
    .unwrap();
    c.execute("INSERT INTO parent VALUES (1),(2)").unwrap();
    c.execute("INSERT INTO child VALUES (10,1),(11,1),(12,2)")
        .unwrap();
    c.execute("DELETE FROM parent WHERE id = 1").unwrap();
    // Only child 12 (pid=2) survives.
    let r = c.query("SELECT id FROM child ORDER BY id").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], graphitesql::Value::Integer(12));
}

#[test]
fn delete_set_null() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE parent(id INTEGER PRIMARY KEY)")
        .unwrap();
    c.execute(
        "CREATE TABLE child(id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE SET NULL)",
    )
    .unwrap();
    c.execute("INSERT INTO parent VALUES (1)").unwrap();
    c.execute("INSERT INTO child VALUES (10,1)").unwrap();
    c.execute("DELETE FROM parent WHERE id = 1").unwrap();
    let r = c.query("SELECT pid FROM child WHERE id = 10").unwrap();
    assert_eq!(r.rows[0][0], graphitesql::Value::Null);
}

#[test]
fn update_cascade() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE parent(id INTEGER PRIMARY KEY)")
        .unwrap();
    c.execute(
        "CREATE TABLE child(id INTEGER PRIMARY KEY, pid INT REFERENCES parent(id) ON UPDATE CASCADE)",
    )
    .unwrap();
    c.execute("INSERT INTO parent VALUES (1)").unwrap();
    c.execute("INSERT INTO child VALUES (10,1)").unwrap();
    c.execute("UPDATE parent SET id = 5 WHERE id = 1").unwrap();
    let r = c.query("SELECT pid FROM child WHERE id = 10").unwrap();
    assert_eq!(r.rows[0][0], graphitesql::Value::Integer(5));
}

/// Differential battery against sqlite3 with foreign_keys ON.
#[test]
fn foreign_keys_against_sqlite3() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let schema = "PRAGMA foreign_keys=ON;\
        CREATE TABLE p(id INTEGER PRIMARY KEY, n TEXT);\
        CREATE TABLE c(id INTEGER PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE CASCADE ON UPDATE CASCADE, v INT);\
        INSERT INTO p VALUES (1,'x'),(2,'y'),(3,'z');\
        INSERT INTO c VALUES (10,1,100),(11,1,101),(12,2,102),(13,NULL,103);";

    // Apply a script to both engines, then compare a query's output.
    let run_sqlite = |ops: &str, q: &str| -> String {
        let path = std::env::temp_dir().join(format!("gsql-fk-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let full = format!("{schema}{ops}");
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg(&full)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg(format!("PRAGMA foreign_keys=ON;{q}"))
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    };
    let run_graphite = |ops: &str, q: &str| -> String {
        let mut g = Connection::open_memory().unwrap();
        for s in schema.split(';') {
            if !s.trim().is_empty() {
                g.execute(s).unwrap();
            }
        }
        for s in ops.split(';') {
            if !s.trim().is_empty() {
                g.execute(s).unwrap();
            }
        }
        let r = g.query(q).unwrap();
        r.rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| match v {
                        graphitesql::Value::Null => String::new(),
                        graphitesql::Value::Integer(i) => i.to_string(),
                        graphitesql::Value::Text(s) => s.clone(),
                        graphitesql::Value::Real(r) => graphitesql::exec::eval::format_real(*r),
                        graphitesql::Value::Blob(b) => {
                            b.iter().map(|x| format!("{x:02x}")).collect()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let cases = [
        (
            "DELETE FROM p WHERE id=1;",
            "SELECT id,pid FROM c ORDER BY id",
        ),
        (
            "UPDATE p SET id=9 WHERE id=2;",
            "SELECT id,pid FROM c ORDER BY id",
        ),
        ("DELETE FROM p WHERE id=3;", "SELECT id FROM p ORDER BY id"),
    ];
    for (ops, q) in cases {
        assert_eq!(run_graphite(ops, q), run_sqlite(ops, q), "ops: {ops}");
    }
}
