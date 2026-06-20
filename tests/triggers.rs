//! Phase 9: row triggers (`CREATE TRIGGER … BEGIN … END`).
//!
//! Verified differentially against the real `sqlite3` CLI where available.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

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
fn after_insert_logs() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("CREATE TABLE log(id INTEGER PRIMARY KEY, what TEXT, v INT)")
        .unwrap();
    c.execute(
        "CREATE TRIGGER trg AFTER INSERT ON t BEGIN \
            INSERT INTO log(what, v) VALUES ('ins', NEW.v); \
         END",
    )
    .unwrap();
    c.execute("INSERT INTO t(v) VALUES (10),(20),(30)").unwrap();
    assert_eq!(ints(&c, "SELECT v FROM log ORDER BY id"), vec![10, 20, 30]);
    assert_eq!(
        c.query("SELECT what FROM log LIMIT 1").unwrap().rows[0][0],
        Value::Text("ins".into())
    );
}

#[test]
fn after_update_old_new() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("CREATE TABLE audit(id INTEGER PRIMARY KEY, oldv INT, newv INT)")
        .unwrap();
    c.execute(
        "CREATE TRIGGER trg AFTER UPDATE ON t BEGIN \
            INSERT INTO audit(oldv, newv) VALUES (OLD.v, NEW.v); \
         END",
    )
    .unwrap();
    c.execute("INSERT INTO t(v) VALUES (1),(2)").unwrap();
    c.execute("UPDATE t SET v = v * 10").unwrap();
    let r = c.query("SELECT oldv, newv FROM audit ORDER BY id").unwrap();
    let got: Vec<(i64, i64)> = r
        .rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Integer(a), Value::Integer(b)) => (*a, *b),
            _ => panic!(),
        })
        .collect();
    assert_eq!(got, vec![(1, 10), (2, 20)]);
}

#[test]
fn after_delete_old() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("CREATE TABLE dead(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("CREATE TRIGGER trg AFTER DELETE ON t BEGIN INSERT INTO dead(v) VALUES (OLD.v); END")
        .unwrap();
    c.execute("INSERT INTO t(v) VALUES (5),(6),(7)").unwrap();
    c.execute("DELETE FROM t WHERE v > 5").unwrap();
    assert_eq!(ints(&c, "SELECT v FROM dead ORDER BY v"), vec![6, 7]);
}

#[test]
fn when_clause() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("CREATE TABLE big(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute(
        "CREATE TRIGGER trg AFTER INSERT ON t WHEN NEW.v >= 100 \
         BEGIN INSERT INTO big(v) VALUES (NEW.v); END",
    )
    .unwrap();
    c.execute("INSERT INTO t(v) VALUES (10),(200),(50),(300)")
        .unwrap();
    assert_eq!(ints(&c, "SELECT v FROM big ORDER BY v"), vec![200, 300]);
}

#[test]
fn trigger_updates_other_table() {
    // A common pattern: maintain a running total in a summary table.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE item(id INTEGER PRIMARY KEY, qty INT)")
        .unwrap();
    c.execute("CREATE TABLE summary(id INTEGER PRIMARY KEY, total INT)")
        .unwrap();
    c.execute("INSERT INTO summary(id, total) VALUES (1, 0)")
        .unwrap();
    c.execute(
        "CREATE TRIGGER trg AFTER INSERT ON item BEGIN \
            UPDATE summary SET total = total + NEW.qty WHERE id = 1; \
         END",
    )
    .unwrap();
    c.execute("INSERT INTO item(qty) VALUES (3),(4),(5)")
        .unwrap();
    assert_eq!(ints(&c, "SELECT total FROM summary"), vec![12]);
}

#[test]
fn drop_trigger_stops_firing() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("CREATE TABLE log(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("CREATE TRIGGER trg AFTER INSERT ON t BEGIN INSERT INTO log(v) VALUES (NEW.v); END")
        .unwrap();
    c.execute("INSERT INTO t(v) VALUES (1)").unwrap();
    c.execute("DROP TRIGGER trg").unwrap();
    c.execute("INSERT INTO t(v) VALUES (2)").unwrap();
    assert_eq!(ints(&c, "SELECT v FROM log ORDER BY v"), vec![1]);
}

#[test]
fn non_recursive_by_default() {
    // With recursive_triggers OFF (the default), a trigger that writes the same
    // table does not re-fire itself.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("CREATE TABLE log(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute(
        "CREATE TRIGGER trg AFTER INSERT ON t BEGIN \
            INSERT INTO log(v) VALUES (NEW.v); \
            INSERT INTO t(v) VALUES (NEW.v + 1); \
         END",
    )
    .unwrap();
    // Inserting one row logs exactly one row (the nested INSERT does not re-fire).
    c.execute("INSERT INTO t(v) VALUES (1)").unwrap();
    assert_eq!(ints(&c, "SELECT count(*) FROM log"), vec![1]);
    // Both the original and the trigger's inserted row exist in t.
    assert_eq!(ints(&c, "SELECT count(*) FROM t"), vec![2]);
}

#[test]
fn recursive_triggers_pragma() {
    let mut c = Connection::open_memory().unwrap();
    assert_eq!(
        c.query("PRAGMA recursive_triggers").unwrap().rows[0][0],
        Value::Integer(0)
    );
    c.execute("PRAGMA recursive_triggers = ON").unwrap();
    assert_eq!(
        c.query("PRAGMA recursive_triggers").unwrap().rows[0][0],
        Value::Integer(1)
    );
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    c.execute("CREATE TABLE log(id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    // Recurse until v reaches a bound, logging each level.
    c.execute(
        "CREATE TRIGGER trg AFTER INSERT ON t WHEN NEW.v < 5 BEGIN \
            INSERT INTO log(v) VALUES (NEW.v); \
            INSERT INTO t(v) VALUES (NEW.v + 1); \
         END",
    )
    .unwrap();
    c.execute("INSERT INTO t(v) VALUES (1)").unwrap();
    // Levels v=1,2,3,4 each log (v=5 fails the WHEN guard).
    assert_eq!(ints(&c, "SELECT v FROM log ORDER BY v"), vec![1, 2, 3, 4]);
}

#[test]
fn instead_of_makes_view_writable() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE base(id INTEGER PRIMARY KEY, v INT, hidden INT)")
        .unwrap();
    c.execute("INSERT INTO base(id,v,hidden) VALUES (1,10,100),(2,20,200)")
        .unwrap();
    c.execute("CREATE VIEW vw AS SELECT id, v FROM base")
        .unwrap();

    // Without an INSTEAD OF trigger, the view is not writable.
    assert!(c.execute("INSERT INTO vw(id,v) VALUES (3,30)").is_err());

    c.execute(
        "CREATE TRIGGER vi INSTEAD OF INSERT ON vw BEGIN \
            INSERT INTO base(id,v,hidden) VALUES (NEW.id, NEW.v, NEW.v * 10); \
         END",
    )
    .unwrap();
    c.execute(
        "CREATE TRIGGER vu INSTEAD OF UPDATE ON vw BEGIN \
            UPDATE base SET v = NEW.v WHERE id = OLD.id; \
         END",
    )
    .unwrap();
    c.execute(
        "CREATE TRIGGER vd INSTEAD OF DELETE ON vw BEGIN \
            DELETE FROM base WHERE id = OLD.id; \
         END",
    )
    .unwrap();

    // INSERT through the view.
    c.execute("INSERT INTO vw(id,v) VALUES (3,30)").unwrap();
    assert_eq!(ints(&c, "SELECT hidden FROM base WHERE id = 3"), vec![300]);

    // UPDATE through the view.
    c.execute("UPDATE vw SET v = 99 WHERE id = 1").unwrap();
    assert_eq!(ints(&c, "SELECT v FROM base WHERE id = 1"), vec![99]);

    // DELETE through the view.
    c.execute("DELETE FROM vw WHERE id = 2").unwrap();
    assert_eq!(ints(&c, "SELECT count(*) FROM base WHERE id = 2"), vec![0]);
}

#[test]
fn triggers_against_sqlite3() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // Statements listed individually (trigger bodies contain semicolons, so a
    // naive split on ';' would break them).
    let statements = [
        "CREATE TABLE t(id INTEGER PRIMARY KEY, v INT, g INT)",
        "CREATE TABLE log(id INTEGER PRIMARY KEY, op TEXT, ov INT, nv INT)",
        "CREATE TRIGGER ti AFTER INSERT ON t BEGIN INSERT INTO log(op,ov,nv) VALUES('i',NULL,NEW.v); END",
        "CREATE TRIGGER tu AFTER UPDATE ON t WHEN NEW.v <> OLD.v BEGIN INSERT INTO log(op,ov,nv) VALUES('u',OLD.v,NEW.v); END",
        "CREATE TRIGGER td AFTER DELETE ON t BEGIN INSERT INTO log(op,ov,nv) VALUES('d',OLD.v,NULL); END",
        "INSERT INTO t(id,v,g) VALUES (1,10,0),(2,20,1),(3,30,0)",
        "UPDATE t SET v = v + 1 WHERE g = 0",
        "UPDATE t SET v = v WHERE id = 2",
        "DELETE FROM t WHERE id = 3",
    ];
    let query = "SELECT op, ov, nv FROM log ORDER BY id";
    let script: String = statements
        .iter()
        .map(|s| format!("{s};"))
        .collect::<Vec<_>>()
        .join("\n");

    let path = std::env::temp_dir().join(format!("gsql-trg-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let o = Command::new("sqlite3")
        .arg(&path)
        .arg(&script)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let want = {
        let o = Command::new("sqlite3")
            .arg(&path)
            .arg(query)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    };
    let _ = std::fs::remove_file(&path);

    let mut g = Connection::open_memory().unwrap();
    for s in statements {
        g.execute(s).unwrap();
    }
    let r = g.query(query).unwrap();
    let got = r
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
        .join("\n");
    assert_eq!(got, want);
}

#[test]
fn update_of_columns_restricts_firing() {
    // `UPDATE OF a` fires only when column a is in the SET list.
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "CREATE TABLE t(a, b, c)",
        "CREATE TABLE log(m)",
        "CREATE TRIGGER tr AFTER UPDATE OF a ON t BEGIN INSERT INTO log VALUES(1); END",
        "INSERT INTO t VALUES(1, 2, 3)",
    ] {
        c.execute(s).unwrap();
    }
    // Updating b only: trigger must NOT fire.
    c.execute("UPDATE t SET b = 9 WHERE a = 1").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM log").unwrap().rows[0][0],
        Value::Integer(0)
    );
    // Updating a: trigger fires.
    c.execute("UPDATE t SET a = 7 WHERE c = 3").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM log").unwrap().rows[0][0],
        Value::Integer(1)
    );
}

#[test]
fn update_of_multiple_columns_and_plain_update() {
    let mut c = Connection::open_memory().unwrap();
    for s in [
        "CREATE TABLE t(a, b, c)",
        "CREATE TABLE log(m)",
        "CREATE TRIGGER tof AFTER UPDATE OF a, c ON t BEGIN INSERT INTO log VALUES('of'); END",
        "CREATE TRIGGER tany AFTER UPDATE ON t BEGIN INSERT INTO log VALUES('any'); END",
        "INSERT INTO t VALUES(1, 2, 3)",
    ] {
        c.execute(s).unwrap();
    }
    // Update b only: the plain trigger fires, the OF(a,c) trigger does not.
    c.execute("UPDATE t SET b = 9").unwrap();
    let rows = c.query("SELECT m FROM log").unwrap();
    assert_eq!(rows.rows, vec![vec![Value::Text("any".into())]]);
    // Update c: both fire.
    c.execute("UPDATE t SET c = 8").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM log").unwrap().rows[0][0],
        Value::Integer(3)
    );
}
