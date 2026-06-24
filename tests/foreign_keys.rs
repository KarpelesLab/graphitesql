//! Phase 9: foreign-key enforcement (gated on `PRAGMA foreign_keys = ON`).

#![cfg(feature = "std")]

use graphitesql::{Connection, Error, Value};

#[test]
fn fk_applies_parent_key_affinity() {
    // SQLite applies the parent key column's affinity to the child value: a text
    // child '1' satisfies an INTEGER parent key 1 (and a non-numeric 'x' does
    // not). Both the child→parent existence check and the parent-change child
    // matching honor it.
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE p(id INTEGER PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO p VALUES (1)").unwrap();
    c.execute("CREATE TABLE c(pid REFERENCES p ON DELETE CASCADE)")
        .unwrap();
    // text '1' satisfies the INTEGER parent key (existence check, child→parent).
    c.execute("INSERT INTO c VALUES ('1')").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM c").unwrap().rows[0][0],
        Value::Integer(1)
    );
    // a non-numeric text value cannot match an INTEGER parent key.
    assert!(matches!(
        c.execute("INSERT INTO c VALUES ('x')"),
        Err(Error::Constraint(_))
    ));
    // parent change matches the text child via the same affinity → CASCADE delete.
    c.execute("DELETE FROM p WHERE id=1").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM c").unwrap().rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn fk_uses_parent_key_collation() {
    // A foreign-key comparison uses the parent key column's collation: a NOCASE
    // parent key matches the child case-insensitively (the child's own collation
    // is not used). CASCADE on the parent honors it too.
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE p(id TEXT COLLATE NOCASE PRIMARY KEY)")
        .unwrap();
    c.execute("INSERT INTO p VALUES ('A')").unwrap();
    c.execute("CREATE TABLE c(pid TEXT REFERENCES p ON DELETE CASCADE)")
        .unwrap();
    // 'a' matches the NOCASE parent key 'A'.
    c.execute("INSERT INTO c VALUES ('a')").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM c").unwrap().rows[0][0],
        Value::Integer(1)
    );
    // a different letter still does not match.
    assert!(matches!(
        c.execute("INSERT INTO c VALUES ('b')"),
        Err(Error::Constraint(_))
    ));
    // CASCADE matches the child under the parent collation.
    c.execute("DELETE FROM p WHERE id='A'").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM c").unwrap().rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn fk_text_parent_matches_integer_child() {
    // The reverse affinity direction: a TEXT parent key, an integer child value.
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE p(id TEXT PRIMARY KEY)").unwrap();
    c.execute("INSERT INTO p VALUES ('1')").unwrap();
    c.execute("CREATE TABLE c(pid REFERENCES p)").unwrap();
    c.execute("INSERT INTO c VALUES (1)").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM c").unwrap().rows[0][0],
        Value::Integer(1)
    );
}

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

#[test]
fn composite_key() {
    // A multi-column FK: orphan rejected, valid accepted, ON DELETE CASCADE
    // removes only the rows matching the full (a,b) key.
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE p(a INT, b INT, PRIMARY KEY(a,b))")
        .unwrap();
    c.execute(
        "CREATE TABLE c(id INTEGER PRIMARY KEY, a INT, b INT, \
         FOREIGN KEY(a,b) REFERENCES p(a,b) ON DELETE CASCADE)",
    )
    .unwrap();
    c.execute("INSERT INTO p VALUES (1,1),(1,2),(2,2)").unwrap();
    c.execute("INSERT INTO c VALUES (10,1,1),(11,1,2),(12,2,2)")
        .unwrap();
    // No parent (9,9): rejected.
    let err = c.execute("INSERT INTO c VALUES (13,9,9)");
    assert!(matches!(err, Err(Error::Constraint(_))), "got {err:?}");
    // Deleting (1,1) cascades only to child 10.
    c.execute("DELETE FROM p WHERE a = 1 AND b = 1").unwrap();
    let r = c.query("SELECT id FROM c ORDER BY id").unwrap();
    let ids: Vec<_> = r.rows.iter().map(|row| row[0].clone()).collect();
    assert_eq!(
        ids,
        vec![
            graphitesql::Value::Integer(11),
            graphitesql::Value::Integer(12)
        ]
    );
}

#[test]
fn self_referential_cascade() {
    // A self-referential FK with recursive ON DELETE CASCADE: deleting the
    // root must cascade through the whole tree and terminate.
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute(
        "CREATE TABLE node(id INTEGER PRIMARY KEY, \
         parent INT REFERENCES node(id) ON DELETE CASCADE)",
    )
    .unwrap();
    c.execute("INSERT INTO node VALUES (1,NULL),(2,1),(3,2),(4,1)")
        .unwrap();
    // A self-referential orphan is still rejected.
    let err = c.execute("INSERT INTO node VALUES (5,99)");
    assert!(matches!(err, Err(Error::Constraint(_))), "got {err:?}");
    // Deleting 1 cascades to 2 and 4, then 2 cascades to 3 — table empties.
    c.execute("DELETE FROM node WHERE id = 1").unwrap();
    let r = c.query("SELECT id FROM node ORDER BY id").unwrap();
    assert!(r.rows.is_empty(), "expected empty, got {:?}", r.rows);
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

/// A `DEFERRABLE INITIALLY DEFERRED` child connection.
fn deferred_parent_child() -> Connection {
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE parent(id INTEGER PRIMARY KEY)")
        .unwrap();
    c.execute(
        "CREATE TABLE child(id INTEGER PRIMARY KEY, \
         pid INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)",
    )
    .unwrap();
    c
}

#[test]
fn deferred_fk_tolerates_temporary_violation_then_commits() {
    let mut c = deferred_parent_child();
    c.execute("BEGIN").unwrap();
    // The parent (100) does not exist yet — allowed because the check is deferred.
    c.execute("INSERT INTO child VALUES(1, 100)").unwrap();
    // Satisfy the constraint before committing.
    c.execute("INSERT INTO parent VALUES(100)").unwrap();
    c.execute("COMMIT").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM child").unwrap().rows[0][0],
        graphitesql::Value::Integer(1)
    );
}

#[test]
fn deferred_fk_still_violated_fails_at_commit_and_keeps_tx_open() {
    let mut c = deferred_parent_child();
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO child VALUES(1, 100)").unwrap();
    // Still no parent 100 → COMMIT fails on the deferred constraint.
    assert!(matches!(c.execute("COMMIT"), Err(Error::Constraint(_))));
    // SQLite leaves the transaction active so it can be repaired and retried.
    c.execute("INSERT INTO parent VALUES(100)").unwrap();
    c.execute("COMMIT").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM child").unwrap().rows[0][0],
        graphitesql::Value::Integer(1)
    );
}

#[test]
fn deferred_fk_in_autocommit_is_checked_immediately() {
    // Outside an explicit transaction the statement is its own transaction, so
    // the implicit commit checks the deferred key at once and rolls the row back.
    let mut c = deferred_parent_child();
    assert!(c.execute("INSERT INTO child VALUES(1, 100)").is_err());
    assert_eq!(
        c.query("SELECT count(*) FROM child").unwrap().rows[0][0],
        graphitesql::Value::Integer(0)
    );
}

#[test]
fn non_deferred_fk_is_still_immediate() {
    // A plain (immediate) FK rejects at statement time even inside a transaction.
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE parent(id INTEGER PRIMARY KEY)")
        .unwrap();
    c.execute("CREATE TABLE child(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id))")
        .unwrap();
    c.execute("BEGIN").unwrap();
    assert!(c.execute("INSERT INTO child VALUES(1, 100)").is_err());
}

#[test]
fn initially_immediate_is_not_deferred() {
    // `DEFERRABLE INITIALLY IMMEDIATE` is checked immediately, like a plain FK.
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE parent(id INTEGER PRIMARY KEY)")
        .unwrap();
    c.execute(
        "CREATE TABLE child(id INTEGER PRIMARY KEY, \
         pid INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY IMMEDIATE)",
    )
    .unwrap();
    c.execute("BEGIN").unwrap();
    assert!(c.execute("INSERT INTO child VALUES(1, 100)").is_err());
}

#[test]
fn deferred_fk_disabled_when_pragma_off() {
    // With foreign_keys OFF a deferred FK is never enforced (immediate or at
    // commit) — byte-identical to the no-FK behavior.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE parent(id INTEGER PRIMARY KEY)")
        .unwrap();
    c.execute(
        "CREATE TABLE child(id INTEGER PRIMARY KEY, \
         pid INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)",
    )
    .unwrap();
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO child VALUES(1, 100)").unwrap();
    c.execute("COMMIT").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM child").unwrap().rows[0][0],
        graphitesql::Value::Integer(1)
    );
}

#[test]
fn insert_or_replace_fires_on_delete_actions() {
    use graphitesql::Value;
    // `INSERT OR REPLACE` deletes the conflicting parent row to make room; that
    // delete runs the child FK `ON DELETE` actions, exactly like sqlite. (DELETE
    // triggers do NOT fire — sqlite gates those on recursive_triggers.)
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE p(id INTEGER PRIMARY KEY)").unwrap();
    c.execute("CREATE TABLE c(pid REFERENCES p(id) ON DELETE CASCADE)")
        .unwrap();
    c.execute("INSERT INTO p VALUES (1),(2)").unwrap();
    c.execute("INSERT INTO c VALUES (1),(1),(2)").unwrap();
    // Replacing p(1) cascade-deletes its two children; p(2)'s child survives.
    c.execute("INSERT OR REPLACE INTO p VALUES (1)").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM c").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );

    // SET NULL variant.
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE p(id INTEGER PRIMARY KEY)").unwrap();
    c.execute("CREATE TABLE c(pid REFERENCES p(id) ON DELETE SET NULL)")
        .unwrap();
    c.execute("INSERT INTO p VALUES (1)").unwrap();
    c.execute("INSERT INTO c VALUES (1)").unwrap();
    c.execute("INSERT OR REPLACE INTO p VALUES (1)").unwrap();
    assert_eq!(
        c.query("SELECT pid FROM c").unwrap().rows[0][0],
        Value::Null
    );

    // RESTRICT blocks the replace (the child still references the row).
    let mut c = Connection::open_memory().unwrap();
    c.execute("PRAGMA foreign_keys = ON").unwrap();
    c.execute("CREATE TABLE p(id INTEGER PRIMARY KEY)").unwrap();
    c.execute("CREATE TABLE c(pid REFERENCES p(id) ON DELETE RESTRICT)")
        .unwrap();
    c.execute("INSERT INTO p VALUES (1)").unwrap();
    c.execute("INSERT INTO c VALUES (1)").unwrap();
    assert!(matches!(
        c.execute("INSERT OR REPLACE INTO p VALUES (1)"),
        Err(Error::Constraint(_))
    ));
}
