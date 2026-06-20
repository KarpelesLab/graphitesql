//! Track C (multi-schema): the database registry and `ATTACH`/`DETACH`.
//! Built up piece by piece (C1: `PRAGMA database_list`).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

#[test]
fn database_list_reports_main() {
    // In-memory main has an empty file path.
    let c = Connection::open_memory().unwrap();
    let r = c.query("PRAGMA database_list").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Integer(0),
            Value::Text("main".into()),
            Value::Text("".into())
        ]]
    );

    // A file-backed main reports its path.
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-attach-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(a)").unwrap();
    }
    let c = Connection::open(&path).unwrap();
    let r = c.query("PRAGMA database_list").unwrap();
    assert_eq!(r.rows[0][1], Value::Text("main".into()));
    assert_eq!(r.rows[0][2], Value::Text(path.clone()));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn attach_and_detach_in_memory() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("ATTACH DATABASE '' AS aux2").unwrap();
    // Attached databases start at seq 2 (seq 1 is reserved for temp).
    let r = c.query("PRAGMA database_list").unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![
                Value::Integer(0),
                Value::Text("main".into()),
                Value::Text("".into())
            ],
            vec![
                Value::Integer(2),
                Value::Text("aux".into()),
                Value::Text("".into())
            ],
            vec![
                Value::Integer(3),
                Value::Text("aux2".into()),
                Value::Text("".into())
            ],
        ]
    );

    // Duplicate / reserved names are rejected.
    assert!(c.execute("ATTACH ':memory:' AS aux").is_err());
    assert!(c.execute("ATTACH ':memory:' AS main").is_err());
    assert!(c.execute("ATTACH ':memory:' AS temp").is_err());

    // DETACH removes it; main/temp and unknown names are rejected.
    c.execute("DETACH aux").unwrap();
    assert!(c.execute("DETACH main").is_err());
    assert!(c.execute("DETACH nope").is_err());
    let r = c.query("PRAGMA database_list").unwrap();
    assert_eq!(r.rows.len(), 2); // main + aux2
    assert_eq!(r.rows[1][1], Value::Text("aux2".into()));
}

#[test]
fn schema_qualified_read_main() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES(1,'x'),(2,'y')").unwrap();
    // `main.t` resolves to the main database.
    let r = c.query("SELECT a, b FROM main.t ORDER BY a").unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][1], Value::Text("x".into()));
    // A table-qualified alias works too.
    assert_eq!(
        c.query("SELECT m.b FROM main.t AS m WHERE m.a = 2")
            .unwrap()
            .rows[0][0],
        Value::Text("y".into())
    );
    // An unknown database qualifier is a clear error (not silent).
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    assert!(c.query("SELECT * FROM zzz.t").is_err());
}

#[test]
fn cross_database_join() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    c.execute("INSERT INTO u VALUES(1,'alice'),(2,'bob'),(3,'carol')")
        .unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("CREATE TABLE aux.o(oid INTEGER PRIMARY KEY, uid INT, amt INT)")
        .unwrap();
    c.execute("INSERT INTO aux.o VALUES(10,1,100),(11,1,200),(12,2,50)")
        .unwrap();

    // INNER join across databases, with 3-part column names (`aux.o.amt`).
    let r = c
        .query("SELECT u.name, aux.o.amt FROM u JOIN aux.o ON u.id = aux.o.uid ORDER BY aux.o.oid")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Text("alice".into()), Value::Integer(100)],
            vec![Value::Text("alice".into()), Value::Integer(200)],
            vec![Value::Text("bob".into()), Value::Integer(50)],
        ]
    );

    // LEFT join: carol (id 3) has no orders, so a NULL-extended row.
    let r = c
        .query(
            "SELECT u.name, o.amt FROM u LEFT JOIN aux.o AS o ON u.id = o.uid ORDER BY u.id, o.amt",
        )
        .unwrap();
    assert_eq!(
        r.rows.last().unwrap(),
        &vec![Value::Text("carol".into()), Value::Null]
    );

    // Both sources qualified, with aliases.
    assert_eq!(
        c.query("SELECT count(*) FROM main.u AS u JOIN aux.o AS o ON u.id=o.uid")
            .unwrap()
            .rows[0][0],
        Value::Integer(3)
    );

    // A temp table joins against a main table for unqualified names.
    c.execute("CREATE TEMP TABLE labels(uid INT, tag TEXT)")
        .unwrap();
    c.execute("INSERT INTO labels VALUES(1,'vip'),(2,'std')")
        .unwrap();
    let r = c
        .query("SELECT u.name, labels.tag FROM u JOIN labels ON u.id = labels.uid ORDER BY u.id")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Text("alice".into()), Value::Text("vip".into())],
            vec![Value::Text("bob".into()), Value::Text("std".into())],
        ]
    );
}

#[test]
fn cross_database_without_rowid_read() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("CREATE TABLE aux.k(a TEXT, b INT, PRIMARY KEY(a)) WITHOUT ROWID")
        .unwrap();
    c.execute("INSERT INTO aux.k VALUES('x',1),('a',2),('m',3)")
        .unwrap();
    // Sole-source read walks the clustered index in PK order.
    let r = c.query("SELECT a, b FROM aux.k ORDER BY a").unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Text("a".into()), Value::Integer(2)],
            vec![Value::Text("m".into()), Value::Integer(3)],
            vec![Value::Text("x".into()), Value::Integer(1)],
        ]
    );
    // A WITHOUT ROWID attached table as a join source.
    c.execute("CREATE TABLE main.t(a TEXT, n INT)").unwrap();
    c.execute("INSERT INTO main.t VALUES('a',10),('x',20)")
        .unwrap();
    let r = c
        .query("SELECT t.a, aux.k.b, t.n FROM t JOIN aux.k ON t.a=aux.k.a ORDER BY t.a")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![
                Value::Text("a".into()),
                Value::Integer(2),
                Value::Integer(10)
            ],
            vec![
                Value::Text("x".into()),
                Value::Integer(1),
                Value::Integer(20)
            ],
        ]
    );
}

#[test]
fn cross_database_create_index() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("CREATE TABLE aux.t(a INT, b TEXT)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(1,'x'),(2,'y'),(1,'z')")
        .unwrap();
    c.execute("CREATE INDEX aux.idx_a ON t(a)").unwrap();
    c.execute("CREATE UNIQUE INDEX aux.idx_b ON t(b)").unwrap();

    // The index serves an equality seek and its UNIQUE constraint is enforced.
    let r = c.query("SELECT b FROM aux.t WHERE a=1 ORDER BY b").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::Text("x".into())], vec![Value::Text("z".into())]]
    );
    assert!(c.execute("INSERT INTO aux.t VALUES(9,'x')").is_err());

    // The indexes live in aux's catalog, none leak into main.
    let names: Vec<_> = c
        .query("SELECT name FROM aux.sqlite_master WHERE type='index' ORDER BY name")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(
        names,
        vec![Value::Text("idx_a".into()), Value::Text("idx_b".into())]
    );
    assert_eq!(
        c.query("SELECT count(*) FROM main.sqlite_master WHERE type='index'")
            .unwrap()
            .rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn cross_database_create_index_file_cross_engine() {
    let sqlite = std::process::Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_ok();
    if !sqlite {
        return;
    }
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-attach-idx-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("ATTACH '{path}' AS d")).unwrap();
        c.execute("CREATE TABLE d.t(a INT, b TEXT)").unwrap();
        c.execute("INSERT INTO d.t VALUES(1,'x'),(2,'y')").unwrap();
        c.execute("CREATE INDEX d.idx_a ON t(a)").unwrap();
    }
    // sqlite3 reads the file: integrity ok (index pages valid) and the index is
    // stored bare-named so sqlite can use it.
    let out = std::process::Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check; SELECT b FROM t WHERE a=2;")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("ok"), "integrity: {s}");
    assert!(s.contains('y'), "seek: {s}");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn cross_database_transaction() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE m(a)").unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("CREATE TABLE aux.t(a)").unwrap();

    // A transaction spanning both databases commits atomically.
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO m VALUES(1)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(10)").unwrap();
    c.execute("COMMIT").unwrap();
    assert_eq!(
        c.query("SELECT a FROM m").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("SELECT a FROM aux.t").unwrap().rows[0][0],
        Value::Integer(10)
    );

    // ROLLBACK discards the attached database's changes too (not just main's).
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO m VALUES(2)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(20)").unwrap();
    c.execute("ROLLBACK").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM m").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("SELECT count(*) FROM aux.t").unwrap().rows[0][0],
        Value::Integer(1) // only the committed 10 remains
    );

    // DDL in the attached database is rolled back as well.
    c.execute("BEGIN").unwrap();
    c.execute("CREATE TABLE aux.tmp2(x)").unwrap();
    c.execute("ROLLBACK").unwrap();
    let names: Vec<_> = c
        .query("SELECT name FROM aux.sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(names, vec![Value::Text("t".into())]);
}

#[test]
fn cross_database_transaction_file_durability() {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-attach-tx-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    // A committed cross-database transaction lands durably in the attached file.
    {
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("ATTACH '{path}' AS d")).unwrap();
        c.execute("CREATE TABLE d.t(a)").unwrap();
        c.execute("BEGIN").unwrap();
        c.execute("INSERT INTO d.t VALUES(1),(2)").unwrap();
        c.execute("COMMIT").unwrap();
        // A subsequent rolled-back transaction leaves no trace.
        c.execute("BEGIN").unwrap();
        c.execute("INSERT INTO d.t VALUES(3)").unwrap();
        c.execute("ROLLBACK").unwrap();
    }
    // Reopen the file: exactly the committed rows are present.
    {
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("ATTACH '{path}' AS d")).unwrap();
        assert_eq!(
            c.query("SELECT count(*) FROM d.t").unwrap().rows[0][0],
            Value::Integer(2)
        );
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn cross_database_savepoint() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("CREATE TABLE aux.t(a)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(1)").unwrap();

    // ROLLBACK TO reverts the attached database to the savepoint.
    c.execute("BEGIN").unwrap();
    c.execute("INSERT INTO aux.t VALUES(2)").unwrap();
    c.execute("SAVEPOINT sp").unwrap();
    c.execute("INSERT INTO aux.t VALUES(3)").unwrap();
    c.execute("ROLLBACK TO sp").unwrap();
    c.execute("COMMIT").unwrap();
    assert_eq!(
        c.query("SELECT a FROM aux.t ORDER BY a").unwrap().rows,
        vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );

    // RELEASE keeps the staged changes in the attached database.
    c.execute("BEGIN").unwrap();
    c.execute("SAVEPOINT s2").unwrap();
    c.execute("INSERT INTO aux.t VALUES(9)").unwrap();
    c.execute("RELEASE s2").unwrap();
    c.execute("COMMIT").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM aux.t").unwrap().rows[0][0],
        Value::Integer(3)
    );

    // DDL in the attached database is reverted by ROLLBACK TO as well.
    c.execute("BEGIN").unwrap();
    c.execute("SAVEPOINT s3").unwrap();
    c.execute("CREATE TABLE aux.z(x)").unwrap();
    c.execute("ROLLBACK TO s3").unwrap();
    c.execute("COMMIT").unwrap();
    let names: Vec<_> = c
        .query("SELECT name FROM aux.sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(names, vec![Value::Text("t".into())]);
}

#[test]
fn cross_database_view() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("CREATE TABLE aux.t(a INT, b TEXT)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(1,'x'),(2,'y'),(3,'z')")
        .unwrap();
    c.execute("CREATE TABLE aux.u(a INT, n INT)").unwrap();
    c.execute("INSERT INTO aux.u VALUES(2,20),(3,30)").unwrap();

    // A view whose body resolves its (unqualified) tables in the attached db.
    c.execute("CREATE VIEW aux.v AS SELECT a, b FROM t WHERE a > 1")
        .unwrap();
    assert_eq!(
        c.query("SELECT * FROM aux.v ORDER BY a").unwrap().rows,
        vec![
            vec![Value::Integer(2), Value::Text("y".into())],
            vec![Value::Integer(3), Value::Text("z".into())],
        ]
    );
    // Stored bare-named.
    assert_eq!(
        c.query("SELECT sql FROM aux.sqlite_master WHERE name='v'")
            .unwrap()
            .rows[0][0],
        Value::Text("CREATE VIEW v AS SELECT a, b FROM t WHERE a > 1".into())
    );

    // A view body containing a join (both tables in the attached db).
    c.execute("CREATE VIEW aux.j AS SELECT t.b, u.n FROM t JOIN u ON t.a=u.a")
        .unwrap();
    assert_eq!(
        c.query("SELECT * FROM aux.j ORDER BY n").unwrap().rows,
        vec![
            vec![Value::Text("y".into()), Value::Integer(20)],
            vec![Value::Text("z".into()), Value::Integer(30)],
        ]
    );

    // A subquery in the view body also resolves in the attached db.
    c.execute("CREATE VIEW aux.s AS SELECT b FROM t WHERE a IN (SELECT a FROM u)")
        .unwrap();
    assert_eq!(
        c.query("SELECT * FROM aux.s ORDER BY b").unwrap().rows,
        vec![vec![Value::Text("y".into())], vec![Value::Text("z".into())]]
    );

    // The cross-db view used as a join source from a main table.
    c.execute("CREATE TABLE main.m(a INT, tag TEXT)").unwrap();
    c.execute("INSERT INTO main.m VALUES(2,'two'),(3,'three')")
        .unwrap();
    assert_eq!(
        c.query("SELECT m.tag, aux.v.b FROM m JOIN aux.v ON m.a=aux.v.a ORDER BY m.a")
            .unwrap()
            .rows,
        vec![
            vec![Value::Text("two".into()), Value::Text("y".into())],
            vec![Value::Text("three".into()), Value::Text("z".into())],
        ]
    );

    // A plain/TEMP view (unqualified) still lives in main and reads fine.
    c.execute("CREATE TEMP VIEW tv AS SELECT 42").unwrap();
    assert_eq!(
        c.query("SELECT * FROM tv").unwrap().rows[0][0],
        Value::Integer(42)
    );
}

#[test]
fn cross_database_view_file_cross_engine() {
    let sqlite = std::process::Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_ok();
    if !sqlite {
        return;
    }
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-attach-view-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("ATTACH '{path}' AS d")).unwrap();
        c.execute("CREATE TABLE d.t(a INT, b TEXT)").unwrap();
        c.execute("INSERT INTO d.t VALUES(1,'x'),(2,'y')").unwrap();
        c.execute("CREATE VIEW d.v AS SELECT b FROM t WHERE a=2")
            .unwrap();
    }
    // graphite re-reads the view from the file...
    {
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("ATTACH '{path}' AS d")).unwrap();
        assert_eq!(
            c.query("SELECT * FROM d.v").unwrap().rows[0][0],
            Value::Text("y".into())
        );
    }
    // ...and so does sqlite3 (bare-named view SQL, integrity ok).
    let out = std::process::Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check; SELECT b FROM v;")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("ok"), "integrity: {s}");
    assert!(s.contains('y'), "view under sqlite3: {s}");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn cross_database_create_trigger() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("CREATE TABLE aux.t(a INT, b TEXT)").unwrap();
    c.execute("CREATE TABLE aux.log(msg TEXT)").unwrap();
    // A trigger on an attached table, with `NEW.col` in its body.
    c.execute(
        "CREATE TRIGGER aux.tr AFTER INSERT ON t BEGIN INSERT INTO log(msg) VALUES(NEW.b); END",
    )
    .unwrap();
    c.execute("INSERT INTO aux.t VALUES(3,'z')").unwrap();
    // It fires against the attached database.
    assert_eq!(
        c.query("SELECT msg FROM aux.log").unwrap().rows[0][0],
        Value::Text("z".into())
    );
    // Stored bare-named (the `aux.` qualifier stripped, but `NEW.b` untouched).
    assert_eq!(
        c.query("SELECT sql FROM aux.sqlite_master WHERE name='tr'")
            .unwrap()
            .rows[0][0],
        Value::Text(
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log(msg) VALUES(NEW.b); END"
                .into()
        )
    );
    // Nothing leaked into main.
    assert_eq!(
        c.query("SELECT count(*) FROM main.sqlite_master")
            .unwrap()
            .rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn cross_database_create_trigger_file_cross_engine() {
    let sqlite = std::process::Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_ok();
    if !sqlite {
        return;
    }
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-attach-trig-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("ATTACH '{path}' AS d")).unwrap();
        c.execute("CREATE TABLE d.t(a INT, b TEXT)").unwrap();
        c.execute("CREATE TABLE d.log(msg TEXT)").unwrap();
        c.execute(
            "CREATE TRIGGER d.tr AFTER INSERT ON t BEGIN INSERT INTO log(msg) VALUES(NEW.b); END",
        )
        .unwrap();
    }
    // sqlite3 reads the file (bare-named trigger SQL), and the trigger it parsed
    // fires on its own insert.
    let out = std::process::Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check; INSERT INTO t VALUES(1,'hi'); SELECT msg FROM log;")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("ok"), "integrity: {s}");
    assert!(s.contains("hi"), "trigger fire under sqlite3: {s}");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn cross_database_alter() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();
    c.execute("CREATE TABLE aux.t(a INT, b TEXT)").unwrap();
    c.execute("INSERT INTO aux.t VALUES(1,'x')").unwrap();

    // ADD / RENAME COLUMN / RENAME TABLE all target the attached database.
    c.execute("ALTER TABLE aux.t ADD COLUMN c INT DEFAULT 7")
        .unwrap();
    c.execute("ALTER TABLE aux.t RENAME COLUMN b TO bb")
        .unwrap();
    c.execute("ALTER TABLE aux.t RENAME TO t2").unwrap();
    let r = c.query("SELECT a, bb, c FROM aux.t2").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![
            Value::Integer(1),
            Value::Text("x".into()),
            Value::Integer(7)
        ]]
    );

    // A same-named main table is untouched by ALTERs against aux.
    c.execute("CREATE TABLE main.t(z)").unwrap();
    c.execute("ALTER TABLE main.t ADD COLUMN w").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM aux.sqlite_master WHERE name='t'")
            .unwrap()
            .rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn cross_database_alter_file_cross_engine() {
    let sqlite = std::process::Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_ok();
    if !sqlite {
        return;
    }
    let mut p = std::env::temp_dir();
    p.push(format!(
        "graphitesql-attach-alter-{}.db",
        std::process::id()
    ));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    {
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("ATTACH '{path}' AS d")).unwrap();
        c.execute("CREATE TABLE d.t(a INT, b TEXT)").unwrap();
        c.execute("INSERT INTO d.t VALUES(1,'x')").unwrap();
        c.execute("ALTER TABLE d.t ADD COLUMN c INT DEFAULT 9")
            .unwrap();
        c.execute("ALTER TABLE d.t RENAME COLUMN b TO bb").unwrap();
    }
    // sqlite3 reads the ALTERed file: integrity ok and the new schema is visible.
    let out = std::process::Command::new("sqlite3")
        .arg(&path)
        .arg("PRAGMA integrity_check; SELECT a||'|'||bb||'|'||c FROM t;")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("ok"), "integrity: {s}");
    assert!(s.contains("1|x|9"), "row: {s}");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn cross_database_create_read_write() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(99)").unwrap();
    c.execute("ATTACH ':memory:' AS aux").unwrap();

    // CREATE / INSERT into the attached database, then read it back.
    c.execute("CREATE TABLE aux.t(id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    assert_eq!(
        c.execute("INSERT INTO aux.t VALUES(1,'alice'),(2,'bob')")
            .unwrap(),
        2
    );
    let r = c.query("SELECT id, name FROM aux.t ORDER BY id").unwrap();
    assert_eq!(r.rows[0][1], Value::Text("alice".into()));
    assert_eq!(r.rows[1][1], Value::Text("bob".into()));

    // The two databases are isolated: main.t still has its own single row, and
    // each catalog lists only its own table.
    assert_eq!(
        c.query("SELECT a FROM t").unwrap().rows[0][0],
        Value::Integer(99)
    );
    assert_eq!(
        c.query("SELECT count(*) FROM main.t").unwrap().rows[0][0],
        Value::Integer(1)
    );
    assert_eq!(
        c.query("SELECT count(*) FROM aux.sqlite_master")
            .unwrap()
            .rows[0][0],
        Value::Integer(1)
    );

    // UPDATE / DELETE / DROP against the attached database.
    c.execute("UPDATE aux.t SET name='ALICE' WHERE id=1")
        .unwrap();
    c.execute("DELETE FROM aux.t WHERE id=2").unwrap();
    let r = c.query("SELECT id, name FROM aux.t").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::Integer(1), Value::Text("ALICE".into())]]
    );
    c.execute("DROP TABLE aux.t").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM aux.sqlite_master")
            .unwrap()
            .rows[0][0],
        Value::Integer(0)
    );
    // main.t is untouched by the DROP in aux.
    assert_eq!(
        c.query("SELECT count(*) FROM main.sqlite_master")
            .unwrap()
            .rows[0][0],
        Value::Integer(1)
    );
}

#[test]
fn temp_tables() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(99)").unwrap();

    // CREATE TEMP TABLE goes to the temp database, not main's catalog.
    c.execute("CREATE TEMP TABLE tmp(id INTEGER PRIMARY KEY, v TEXT)")
        .unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM sqlite_master").unwrap().rows[0][0],
        Value::Integer(1) // just `t`
    );
    // database_list shows temp at seq 1.
    let r = c.query("PRAGMA database_list").unwrap();
    assert_eq!(
        r.rows[1],
        vec![
            Value::Integer(1),
            Value::Text("temp".into()),
            Value::Text("".into())
        ]
    );
    // sqlite_temp_master lists temp objects.
    assert_eq!(
        c.query("SELECT name FROM sqlite_temp_master").unwrap().rows[0][0],
        Value::Text("tmp".into())
    );

    // Unqualified DML/reads resolve to the temp table.
    c.execute("INSERT INTO tmp VALUES(1,'a'),(2,'b')").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM tmp").unwrap().rows[0][0],
        Value::Integer(2)
    );
    c.execute("UPDATE tmp SET v='Z' WHERE id=1").unwrap();
    c.execute("DELETE FROM tmp WHERE id=2").unwrap();
    let r = c.query("SELECT id, v FROM tmp").unwrap();
    assert_eq!(
        r.rows,
        vec![vec![Value::Integer(1), Value::Text("Z".into())]]
    );

    // A temp table shadows a same-named main table for unqualified names.
    c.execute("CREATE TEMP TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    c.execute("INSERT INTO t VALUES(2)").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t").unwrap().rows[0][0],
        Value::Integer(2)
    );
    assert_eq!(
        c.query("SELECT count(*) FROM main.t").unwrap().rows[0][0],
        Value::Integer(1)
    );

    // DROP of the temp table leaves the main table intact.
    c.execute("DROP TABLE t").unwrap();
    assert_eq!(
        c.query("SELECT a FROM t").unwrap().rows[0][0],
        Value::Integer(99)
    );
}

#[test]
fn temp_tables_do_not_persist_to_a_file() {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-attach-temp-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE persist(a)").unwrap();
        c.execute("CREATE TEMP TABLE ephemeral(b)").unwrap();
        c.execute("INSERT INTO ephemeral VALUES(1)").unwrap();
    }
    // Reopening the file shows only the persistent table.
    let c = Connection::open(&path).unwrap();
    let names: Vec<_> = c
        .query("SELECT name FROM sqlite_master")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(names, vec![Value::Text("persist".into())]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn attach_file_database_cross_engine() {
    let sqlite = std::process::Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_ok();

    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-attach-file-{}.db", std::process::id()));
    let path = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    // Create + populate a brand-new file via ATTACH, then check sqlite3 reads it.
    {
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("ATTACH '{path}' AS d")).unwrap();
        c.execute("CREATE TABLE d.t(id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        c.execute("INSERT INTO d.t VALUES(1,'hello'),(2,'world')")
            .unwrap();
        // database_list reports the file path for the attachment.
        let r = c.query("PRAGMA database_list").unwrap();
        assert_eq!(r.rows[1][1], Value::Text("d".into()));
        assert_eq!(r.rows[1][2], Value::Text(path.clone()));
    }
    if sqlite {
        let out = std::process::Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check; SELECT id||':'||v FROM t ORDER BY id;")
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.contains("ok"), "integrity: {s}");
        assert!(s.contains("1:hello") && s.contains("2:world"), "rows: {s}");
    }

    // Re-attach the existing file and read it back; a further write round-trips.
    {
        let mut c = Connection::open_memory().unwrap();
        c.execute(&format!("ATTACH '{path}' AS d")).unwrap();
        assert_eq!(
            c.query("SELECT v FROM d.t WHERE id=1").unwrap().rows[0][0],
            Value::Text("hello".into())
        );
        c.execute("INSERT INTO d.t VALUES(3,'again')").unwrap();
    }
    if sqlite {
        let out = std::process::Command::new("sqlite3")
            .arg(&path)
            .arg("SELECT count(*) FROM t;")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "3");
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}
