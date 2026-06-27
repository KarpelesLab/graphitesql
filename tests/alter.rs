//! Phase 9: ALTER TABLE (ADD COLUMN, RENAME TO).

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn temp_path(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("graphitesql-alter-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

#[test]
fn add_column_applies_default_to_existing_rows() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a TEXT)")
        .unwrap();
    c.execute("INSERT INTO t(a) VALUES ('x'),('y')").unwrap();

    // Add a column with a default; pre-existing rows must read the default.
    c.execute("ALTER TABLE t ADD COLUMN n INT DEFAULT 42")
        .unwrap();
    c.execute("ALTER TABLE t ADD COLUMN note TEXT").unwrap(); // default NULL

    let r = c.query("SELECT id, a, n, note FROM t ORDER BY id").unwrap();
    assert_eq!(r.columns, vec!["id", "a", "n", "note"]);
    assert_eq!(r.rows[0][2], Value::Integer(42)); // default for old row
    assert_eq!(r.rows[0][3], Value::Null);

    // New rows can populate the added columns.
    c.execute("INSERT INTO t(a, n, note) VALUES ('z', 7, 'hi')")
        .unwrap();
    let r = c.query("SELECT n, note FROM t WHERE a = 'z'").unwrap();
    assert_eq!(r.rows[0][0], Value::Integer(7));
    assert_eq!(r.rows[0][1], Value::Text("hi".into()));
}

#[test]
fn rename_column_updates_table_and_index() {
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let path = temp_path("renamecol.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, old_name TEXT)")
            .unwrap();
        c.execute("CREATE INDEX i ON t(old_name)").unwrap();
        c.execute("INSERT INTO t(old_name) VALUES ('x'),('y')")
            .unwrap();

        c.execute("ALTER TABLE t RENAME COLUMN old_name TO new_name")
            .unwrap();

        // Old name is gone; data is intact under the new name.
        assert!(c.query("SELECT old_name FROM t").is_err());
        let r = c.query("SELECT new_name FROM t ORDER BY new_name").unwrap();
        assert_eq!(r.rows[0][0], Value::Text("x".into()));
        assert_eq!(r.rows[1][0], Value::Text("y".into()));
    }
    if sqlite {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check; SELECT new_name FROM t ORDER BY new_name;")
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.contains("ok"), "integrity: {s}");
        assert!(s.contains('x') && s.contains('y'));
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn add_column_appends_verbatim_to_schema_text() {
    // ALTER … ADD COLUMN appends the column's verbatim text before the column
    // list's closing paren, preserving the original definition — like sqlite.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT NOT NULL, b)").unwrap();
    c.execute("ALTER TABLE t ADD COLUMN c TEXT DEFAULT 'hi' CHECK(c<>'')")
        .unwrap();
    let sql = match &c
        .query("SELECT sql FROM sqlite_master WHERE type='table'")
        .unwrap()
        .rows[0][0]
    {
        Value::Text(s) => s.clone(),
        o => panic!("not text: {o:?}"),
    };
    assert_eq!(
        sql,
        "CREATE TABLE t(a INT NOT NULL, b, c TEXT DEFAULT 'hi' CHECK(c<>''))"
    );
}

#[test]
fn add_column_inserts_before_table_constraints() {
    // When the table has trailing table-level constraints, the new column goes
    // after the last column but *before* the first constraint — exactly where
    // sqlite places it (`t(a, b, c, CHECK(a>0))`, not `t(a, b, CHECK(a>0), c)`).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b, CHECK(a>0))").unwrap();
    c.execute("ALTER TABLE t ADD COLUMN c").unwrap();
    let sql = match &c
        .query("SELECT sql FROM sqlite_master WHERE type='table'")
        .unwrap()
        .rows[0][0]
    {
        Value::Text(s) => s.clone(),
        o => panic!("not text: {o:?}"),
    };
    assert_eq!(sql, "CREATE TABLE t(a, b, c, CHECK(a>0))");
}

#[test]
fn rename_column_preserves_schema_text() {
    // ALTER … RENAME COLUMN edits the column name in the stored CREATE text in
    // place (renaming it inside CHECK/generated expressions and dependent index
    // definitions too), reproducing the new name exactly as written — bare for a
    // bare word, double-quoted for a quoted identifier — like sqlite.
    let sql_after = |create: &[&str], rename: &str, ty: &str| -> String {
        let mut c = Connection::open_memory().unwrap();
        for s in create {
            c.execute(s).unwrap();
        }
        c.execute(rename).unwrap();
        match &c
            .query(&format!("SELECT sql FROM sqlite_master WHERE type='{ty}'"))
            .unwrap()
            .rows[0][0]
        {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        }
    };
    // Renamed inside a CHECK expression, bare new name.
    assert_eq!(
        sql_after(
            &["CREATE TABLE t(aa INT, b, CHECK(aa>0))"],
            "ALTER TABLE t RENAME COLUMN aa TO yy",
            "table",
        ),
        "CREATE TABLE t(yy INT, b, CHECK(yy>0))"
    );
    // A quoted new name stays double-quoted.
    assert_eq!(
        sql_after(
            &["CREATE TABLE t(a, b)"],
            "ALTER TABLE t RENAME COLUMN a TO \"x y\"",
            "table",
        ),
        "CREATE TABLE t(\"x y\", b)"
    );
    // A dependent index is repointed in place too.
    assert_eq!(
        sql_after(
            &["CREATE TABLE t(a, b)", "CREATE INDEX i ON t(a, b)"],
            "ALTER TABLE t RENAME COLUMN a TO x",
            "index",
        ),
        "CREATE INDEX i ON t(x, b)"
    );
}

#[test]
fn drop_column_preserves_schema_text() {
    // ALTER … DROP COLUMN removes the column (and one adjacent comma) from the
    // stored CREATE text in place, preserving the others verbatim — like sqlite.
    let sql_after = |create: &str, drop: &str| -> String {
        let mut c = Connection::open_memory().unwrap();
        c.execute(create).unwrap();
        c.execute(drop).unwrap();
        match &c
            .query("SELECT sql FROM sqlite_master WHERE type='table'")
            .unwrap()
            .rows[0][0]
        {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        }
    };
    // Middle, last, and first columns; formatting preserved.
    assert_eq!(
        sql_after(
            "CREATE TABLE t(a INT, b TEXT DEFAULT 'x', c)",
            "ALTER TABLE t DROP COLUMN b"
        ),
        "CREATE TABLE t(a INT, c)"
    );
    assert_eq!(
        sql_after("CREATE TABLE t(a, b, c)", "ALTER TABLE t DROP COLUMN c"),
        "CREATE TABLE t(a, b)"
    );
    assert_eq!(
        sql_after("CREATE TABLE t(a, b, c)", "ALTER TABLE t DROP COLUMN a"),
        "CREATE TABLE t(b, c)"
    );
}

#[test]
fn rename_table_preserves_schema_text() {
    // ALTER … RENAME TO edits the table name in the stored CREATE text in place
    // (quoting only the new name), preserving the original column formatting —
    // and repoints a dependent index's `ON` clause the same way — exactly like
    // sqlite, rather than reprinting the whole definition from the AST.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT NOT NULL, b TEXT DEFAULT 'x', CHECK(a>0))")
        .unwrap();
    c.execute("CREATE INDEX i ON t(a COLLATE NOCASE, b)")
        .unwrap();
    c.execute("ALTER TABLE t RENAME TO t2").unwrap();

    let sql = |ty: &str| -> String {
        match &c
            .query(&format!("SELECT sql FROM sqlite_master WHERE type='{ty}'"))
            .unwrap()
            .rows[0][0]
        {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        }
    };
    assert_eq!(
        sql("table"),
        "CREATE TABLE \"t2\"(a INT NOT NULL, b TEXT DEFAULT 'x', CHECK(a>0))"
    );
    assert_eq!(
        sql("index"),
        "CREATE INDEX i ON \"t2\"(a COLLATE NOCASE, b)"
    );
}

#[test]
fn rename_table_repoints_foreign_keys() {
    // ALTER … RENAME TO updates `REFERENCES <old>` in OTHER tables' foreign keys
    // (and any self-reference), repointing only the target token — references to
    // other tables, and a like-named column, are left intact — exactly like sqlite.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE a(id INTEGER PRIMARY KEY)").unwrap();
    c.execute("CREATE TABLE p(id INTEGER PRIMARY KEY, par REFERENCES p(id))")
        .unwrap();
    // `c` references both `a` and `p`; column `p` shares the old table's name.
    c.execute("CREATE TABLE c(p TEXT, x REFERENCES a(id), y REFERENCES p(id))")
        .unwrap();
    c.execute("ALTER TABLE p RENAME TO p2").unwrap();

    let sql = |name: &str| -> String {
        match &c
            .query(&format!(
                "SELECT sql FROM sqlite_master WHERE name='{name}'"
            ))
            .unwrap()
            .rows[0][0]
        {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        }
    };
    // Self-reference repointed; the table name quoted.
    assert_eq!(
        sql("p2"),
        "CREATE TABLE \"p2\"(id INTEGER PRIMARY KEY, par REFERENCES \"p2\"(id))"
    );
    // Only the `p` FK target is rewritten: the `a` reference and the `p` column stay.
    assert_eq!(
        sql("c"),
        "CREATE TABLE c(p TEXT, x REFERENCES a(id), y REFERENCES \"p2\"(id))"
    );

    // The repointed foreign key is still enforced under the new name.
    c.execute("PRAGMA foreign_keys=ON").unwrap();
    c.execute("INSERT INTO p2(id) VALUES(1)").unwrap();
    c.execute("INSERT INTO c(y) VALUES(1)").unwrap(); // valid parent
    assert!(c.execute("INSERT INTO c(y) VALUES(99)").is_err()); // orphan rejected
}

#[test]
fn rename_table_updates_catalog_and_indexes() {
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let path = temp_path("rename.db");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE old(id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        c.execute("CREATE INDEX idx_v ON old(v)").unwrap();
        c.execute("INSERT INTO old(v) VALUES ('a'),('b'),('c')")
            .unwrap();

        c.execute("ALTER TABLE old RENAME TO renamed").unwrap();

        // Old name gone, new name works.
        assert!(c.schema().table("old").is_none());
        assert!(c.schema().table("renamed").is_some());
        let r = c.query("SELECT count(*) FROM renamed").unwrap();
        assert_eq!(r.rows[0][0], Value::Integer(3));
        // The index now belongs to the renamed table.
        assert_eq!(c.schema().index("idx_v").unwrap().tbl_name, "renamed");
    }
    if sqlite {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check; SELECT count(*) FROM renamed;")
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.contains("ok"), "integrity: {s}");
        assert!(s.contains('3'));
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn drop_column_rewrites_rows_and_keeps_integrity() {
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let path = temp_path("dropcol.db");
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INT, b TEXT, c INT)")
            .unwrap();
        c.execute("CREATE INDEX it_c ON t(c)").unwrap();
        c.execute("INSERT INTO t(a,b,c) VALUES (1,'x',10),(2,'y',20),(3,'z',30)")
            .unwrap();

        c.execute("ALTER TABLE t DROP COLUMN b").unwrap();

        let r = c.query("SELECT * FROM t ORDER BY id").unwrap();
        assert_eq!(r.columns, ["id", "a", "c"]);
        assert_eq!(r.rows.len(), 3);
        assert_eq!(
            r.rows[0],
            [Value::Integer(1), Value::Integer(1), Value::Integer(10)]
        );
        // The index on the surviving column still works (and its position shifted).
        let q = c.query("SELECT id FROM t WHERE c = 20").unwrap();
        assert_eq!(q.rows[0][0], Value::Integer(2));

        // Structural columns cannot be dropped.
        assert!(c.execute("ALTER TABLE t DROP COLUMN id").is_err()); // PRIMARY KEY
        assert!(c.execute("ALTER TABLE t DROP COLUMN c").is_err()); // indexed
        assert!(c.execute("ALTER TABLE t DROP COLUMN nope").is_err()); // missing
    }
    if sqlite {
        let out = Command::new("sqlite3")
            .arg(&path)
            .arg("PRAGMA integrity_check; SELECT a,c FROM t ORDER BY id;")
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(s.contains("ok"), "integrity: {s}");
        assert!(s.contains("2|20"));
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-journal"));
}

#[test]
fn create_temp_table_works() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TEMP TABLE t1(x INT)").unwrap();
    c.execute("CREATE TEMPORARY TABLE t2(y TEXT)").unwrap();
    c.execute("INSERT INTO t1 VALUES (1),(2)").unwrap();
    c.execute("INSERT INTO t2 VALUES ('a')").unwrap();
    assert_eq!(
        c.query("SELECT count(*) FROM t1").unwrap().rows[0][0],
        Value::Integer(2)
    );
    assert_eq!(
        c.query("SELECT y FROM t2").unwrap().rows[0][0],
        Value::Text("a".into())
    );
}

#[test]
fn add_column_constraint_restrictions() {
    // UNIQUE and PRIMARY KEY columns can never be added.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    assert!(c.execute("ALTER TABLE t ADD COLUMN b UNIQUE").is_err());
    assert!(c.execute("ALTER TABLE t ADD COLUMN b PRIMARY KEY").is_err());

    // NOT NULL with a NULL default is allowed on an empty table...
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    assert!(c.execute("ALTER TABLE t ADD COLUMN b NOT NULL").is_ok());

    // ...but rejected once the table holds rows (they would get NULL).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(1)").unwrap();
    assert!(c.execute("ALTER TABLE t ADD COLUMN b NOT NULL").is_err());
    assert!(c
        .execute("ALTER TABLE t ADD COLUMN b NOT NULL DEFAULT NULL")
        .is_err());
    // A non-NULL default fills existing rows, so it is allowed.
    assert!(c
        .execute("ALTER TABLE t ADD COLUMN b NOT NULL DEFAULT 0")
        .is_ok());
    assert_eq!(
        c.query("SELECT b FROM t").unwrap().rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn rename_collisions_are_rejected() {
    // RENAME COLUMN onto an existing column name is a duplicate.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    assert!(c.execute("ALTER TABLE t RENAME COLUMN a TO b").is_err());
    assert!(c.execute("ALTER TABLE t RENAME COLUMN a TO c").is_ok());

    // RENAME TABLE onto an existing table, index, or itself is rejected.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("CREATE TABLE u(b)").unwrap();
    c.execute("CREATE INDEX ix ON t(a)").unwrap();
    assert!(c.execute("ALTER TABLE t RENAME TO u").is_err());
    assert!(c.execute("ALTER TABLE t RENAME TO ix").is_err());
    assert!(c.execute("ALTER TABLE t RENAME TO t").is_err());
    assert!(c.execute("ALTER TABLE t RENAME TO renamed").is_ok());
}

#[test]
fn rename_column_keeps_check_generated_and_default_working() {
    // After RENAME COLUMN, the table's own CHECK / generated / DEFAULT
    // expressions still refer to the column (by its new name), so the table
    // keeps working — matching sqlite, where it previously broke with
    // "no such column".
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b CHECK(b > a), c AS (a + 1), d DEFAULT 7)")
        .unwrap();
    c.execute("ALTER TABLE t RENAME COLUMN a TO x").unwrap();

    // CHECK still enforced under the new name.
    assert!(c.execute("INSERT INTO t(x, b) VALUES (5, 3)").is_err());
    c.execute("INSERT INTO t(x, b) VALUES (3, 5)").unwrap();
    // Generated column and default still compute.
    assert_eq!(
        c.query("SELECT x, b, c, d FROM t").unwrap().rows,
        [vec![
            Value::Integer(3),
            Value::Integer(5),
            Value::Integer(4),
            Value::Integer(7),
        ]]
    );

    // A table-level CHECK and a composite PK list are renamed too.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE u(a, b, CHECK(a < b), PRIMARY KEY(a, b))")
        .unwrap();
    c.execute("ALTER TABLE u RENAME COLUMN b TO y").unwrap();
    assert!(c.execute("INSERT INTO u VALUES (3, 1)").is_err()); // 3 < 1 false
    c.execute("INSERT INTO u VALUES (1, 3)").unwrap();
    assert!(c.execute("INSERT INTO u VALUES (1, 3)").is_err()); // PK(a,y) dup
}

#[test]
fn rename_table_rewrites_dependent_views() {
    // A view whose SELECT references a table keeps working after the table is
    // renamed: the stored body is repointed to the new name, matching sqlite.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a, b)").unwrap();
    c.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
    c.execute("CREATE VIEW v AS SELECT a, b FROM t WHERE b > 5")
        .unwrap();
    c.execute("CREATE VIEW vq AS SELECT t.a AS x, count(*) AS c FROM t GROUP BY t.a")
        .unwrap();
    // An unrelated view (no reference to t) must be left untouched.
    c.execute("CREATE VIEW other AS SELECT 1 AS z").unwrap();
    let other_sql_before = c
        .query("SELECT sql FROM sqlite_master WHERE name='other'")
        .unwrap()
        .rows[0][0]
        .clone();

    c.execute("ALTER TABLE t RENAME TO t2").unwrap();

    // The views still resolve and return the same rows.
    assert_eq!(
        c.query("SELECT * FROM v ORDER BY a").unwrap().rows,
        [
            vec![Value::Integer(1), Value::Integer(10)],
            vec![Value::Integer(2), Value::Integer(20)],
        ]
    );
    assert_eq!(
        c.query("SELECT * FROM vq ORDER BY x").unwrap().rows,
        [
            vec![Value::Integer(1), Value::Integer(1)],
            vec![Value::Integer(2), Value::Integer(1)],
        ]
    );
    // Stored bodies repoint the table (double-quoted, as sqlite does); the
    // count() function name is left intact, and the unrelated view is unchanged.
    let sql_v = c
        .query("SELECT sql FROM sqlite_master WHERE name='v'")
        .unwrap()
        .rows[0][0]
        .clone();
    assert_eq!(
        sql_v,
        Value::Text("CREATE VIEW v AS SELECT a, b FROM \"t2\" WHERE b > 5".into())
    );
    let sql_vq = c
        .query("SELECT sql FROM sqlite_master WHERE name='vq'")
        .unwrap()
        .rows[0][0]
        .clone();
    assert_eq!(
        sql_vq,
        Value::Text(
            "CREATE VIEW vq AS SELECT \"t2\".a AS x, count(*) AS c FROM \"t2\" GROUP BY \"t2\".a"
                .into()
        )
    );
    let other_sql_after = c
        .query("SELECT sql FROM sqlite_master WHERE name='other'")
        .unwrap()
        .rows[0][0]
        .clone();
    assert_eq!(other_sql_before, other_sql_after);

    // The result database still passes an integrity check.
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

#[test]
fn rename_table_named_like_a_function_spares_calls() {
    // A table named the same as a SQL function (`count`) is renamed only where it
    // is a table reference; the `count(*)` call is preserved.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE count(x)").unwrap();
    c.execute("INSERT INTO count VALUES (1), (2), (3)").unwrap();
    c.execute("CREATE VIEW cv AS SELECT count(*) AS n FROM count")
        .unwrap();
    c.execute("ALTER TABLE count RENAME TO tally").unwrap();
    assert_eq!(
        c.query("SELECT n FROM cv").unwrap().rows[0][0],
        Value::Integer(3)
    );
    assert_eq!(
        c.query("SELECT sql FROM sqlite_master WHERE name='cv'")
            .unwrap()
            .rows[0][0],
        Value::Text("CREATE VIEW cv AS SELECT count(*) AS n FROM \"tally\"".into())
    );
}

#[test]
fn rename_column_propagates_into_foreign_keys() {
    // Renaming a column referenced by another table's FOREIGN KEY rewrites the
    // `REFERENCES <parent>(col)` clause, byte-for-byte like sqlite3, while a child
    // column that happens to share the old name is left untouched.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE p(name)").unwrap();
    c.execute("CREATE TABLE c(name, pid REFERENCES p(name))")
        .unwrap();

    c.execute("ALTER TABLE p RENAME COLUMN name TO label")
        .unwrap();

    let sql = |name: &str| -> String {
        match &c
            .query(&format!(
                "SELECT sql FROM sqlite_master WHERE name='{name}'"
            ))
            .unwrap()
            .rows[0][0]
        {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        }
    };
    // The FK reference is rewritten; the child's own `name` column is not.
    assert_eq!(sql("c"), "CREATE TABLE c(name, pid REFERENCES p(label))");
    // The parent's own column is renamed (existing behavior).
    assert_eq!(sql("p"), "CREATE TABLE p(label)");
}

#[test]
fn rename_table_rewrites_dependent_trigger_bodies() {
    // ALTER TABLE … RENAME TO rewrites the renamed table's name inside trigger
    // bodies that reference it — whether the trigger is attached to it or to
    // another table — byte-for-byte like sqlite3, while an unrelated trigger is
    // left untouched.
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let after = |create: &[&str]| -> String {
        let mut c = Connection::open_memory().unwrap();
        for s in create {
            c.execute(s).unwrap();
        }
        c.execute("ALTER TABLE t RENAME TO t2").unwrap();
        match &c
            .query("SELECT sql FROM sqlite_master WHERE name='tr'")
            .unwrap()
            .rows[0][0]
        {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        }
    };
    // Trigger on another table whose body inserts into the renamed table.
    let body = after(&[
        "CREATE TABLE t(a)",
        "CREATE TABLE u(x)",
        "CREATE TRIGGER tr AFTER INSERT ON u BEGIN INSERT INTO t VALUES(NEW.x); END",
    ]);
    assert!(
        body.contains("\"t2\"") && !body.contains("INTO t "),
        "not rewritten: {body}"
    );
    if sqlite {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(
                    "CREATE TABLE t(a); CREATE TABLE u(x);\
                     CREATE TRIGGER tr AFTER INSERT ON u BEGIN INSERT INTO t VALUES(NEW.x); END;\
                     ALTER TABLE t RENAME TO t2;\
                     SELECT sql FROM sqlite_master WHERE name='tr';",
                )
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        assert_eq!(body, want);
    }
    // An unrelated trigger (touches only `u`) is unchanged.
    let unrelated = after(&[
        "CREATE TABLE t(a)",
        "CREATE TABLE u(x)",
        "CREATE TRIGGER tr AFTER INSERT ON u BEGIN INSERT INTO u VALUES(NEW.x); END",
    ]);
    assert_eq!(
        unrelated,
        "CREATE TRIGGER tr AFTER INSERT ON u BEGIN INSERT INTO u VALUES(NEW.x); END"
    );
}

#[test]
fn rename_column_propagates_into_trigger_bodies() {
    // RENAME COLUMN propagates into a trigger ON the renamed table: NEW.col /
    // OLD.col (and the WHEN clause) always bind to that table, so they are
    // rewritten byte-for-byte like sqlite3 even when the trigger body also
    // touches another table — while NEW/OLD of a trigger on a *different* table
    // (where they bind elsewhere) are left untouched.
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let after = |create: &str, alter: &str| -> String {
        let mut c = Connection::open_memory().unwrap();
        c.execute_batch(create).unwrap();
        c.execute(alter).unwrap();
        match &c
            .query("SELECT sql FROM sqlite_master WHERE name='tr'")
            .unwrap()
            .rows[0][0]
        {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        }
    };
    let want = |create: &str, alter: &str| -> String {
        let o = Command::new("sqlite3")
            .arg(":memory:")
            .arg(format!(
                "{create} {alter} SELECT sql FROM sqlite_master WHERE name='tr';"
            ))
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    };
    let alter = "ALTER TABLE t RENAME COLUMN a TO a2;";
    // (description, setup) — each trigger is on `t` and also writes to `l`.
    let cases = [
        "CREATE TABLE t(a,b); CREATE TABLE l(x); \
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO l(x) VALUES(NEW.a); END;",
        "CREATE TABLE t(a,b); CREATE TABLE l(x); \
         CREATE TRIGGER tr AFTER INSERT ON t WHEN NEW.a > 0 BEGIN INSERT INTO l(x) VALUES(1); END;",
        "CREATE TABLE t(a,b); CREATE TABLE l(x); \
         CREATE TRIGGER tr BEFORE DELETE ON t BEGIN INSERT INTO l(x) VALUES(OLD.a); END;",
    ];
    for setup in cases {
        let got = after(setup, alter);
        assert!(
            got.contains(".a2") && !got.contains(".a "),
            "NEW/OLD not rewritten: {got}"
        );
        if sqlite {
            assert_eq!(got, want(setup, alter), "diverged for: {setup}");
        }
    }

    // A trigger on a *different* table whose NEW.a binds to that table's own `a`
    // is left untouched (NEW does not refer to the renamed table here).
    let other = "CREATE TABLE t(a,b); CREATE TABLE u(a,c); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN UPDATE t SET b = NEW.a; END;";
    let got = after(other, alter);
    assert!(
        got.contains("NEW.a") && !got.contains("NEW.a2"),
        "wrongly rewritten: {got}"
    );
    if sqlite {
        assert_eq!(got, want(other, alter));
    }
}

#[test]
fn rename_column_propagates_into_cross_object_trigger_body() {
    // A trigger attached to ANOTHER table, whose body reads/writes ONLY the
    // renamed table, has every bare and `<table>.`-qualified reference to the
    // renamed column rewritten byte-for-byte like sqlite3 — while its own
    // NEW/OLD (which bind to the trigger's table) are left untouched. Conservative:
    // a body touching a second table is left unchanged (never corrupted).
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let after = |create: &str, alter: &str| -> String {
        let mut c = Connection::open_memory().unwrap();
        c.execute_batch(create).unwrap();
        c.execute(alter).unwrap();
        match &c
            .query("SELECT sql FROM sqlite_master WHERE name='tr'")
            .unwrap()
            .rows[0][0]
        {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        }
    };
    let want = |create: &str, alter: &str| -> String {
        let o = Command::new("sqlite3")
            .arg(":memory:")
            .arg(format!(
                "{create} {alter} SELECT sql FROM sqlite_master WHERE name='tr';"
            ))
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    };
    let alter = "ALTER TABLE t RENAME COLUMN a TO a2;";
    let cases = [
        // Body updates only `t`: bare `a` (SET target and expression) rewritten.
        "CREATE TABLE t(a,b); CREATE TABLE u(x); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN UPDATE t SET a = a + 1 WHERE a > 0; END;",
        // Body deletes from only `t`; OLD.x belongs to `u` and stays.
        "CREATE TABLE t(a,b); CREATE TABLE u(x); \
         CREATE TRIGGER tr BEFORE DELETE ON u BEGIN DELETE FROM t WHERE t.a = OLD.x; END;",
        // Body inserts into only `t`.
        "CREATE TABLE t(a,b); CREATE TABLE u(x); \
         CREATE TRIGGER tr AFTER INSERT ON u BEGIN INSERT INTO t(a,b) VALUES(NEW.x, 0); END;",
    ];
    for setup in cases {
        let got = after(setup, alter);
        if sqlite {
            assert_eq!(got, want(setup, alter), "diverged for: {setup}");
        }
    }

    // Conservative bail: a body that touches a SECOND table (INSERT INTO l SELECT
    // FROM t) is left unchanged rather than risk a wrong rewrite — so it does NOT
    // match sqlite here, but it is never corrupted (graphite keeps the old text).
    let two = "CREATE TABLE t(a,b); CREATE TABLE l(y); \
         CREATE TRIGGER tr AFTER INSERT ON l BEGIN INSERT INTO l(y) SELECT a FROM t; END;";
    let got = after(two, alter);
    assert!(
        got.contains("SELECT a FROM t"),
        "should be left intact: {got}"
    );
}

#[test]
fn rename_column_propagates_into_single_source_view() {
    // Renaming a column propagates into a view that draws from ONLY that table —
    // every reference (bare, table-qualified, and through an alias) is rewritten
    // byte-for-byte like sqlite3, and the view still queries correctly.
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let cases = [
        "CREATE VIEW v AS SELECT a, b FROM t",
        "CREATE VIEW v AS SELECT t.a, b FROM t WHERE t.a > 0",
        "CREATE VIEW v AS SELECT a AS x FROM t ORDER BY a",
    ];
    for view in cases {
        let ddl = format!("CREATE TABLE t(a INT, b INT); {view};");
        let alter = "ALTER TABLE t RENAME COLUMN a TO renamed;";
        let mut c = Connection::open_memory().unwrap();
        for s in ddl.split(';').chain(alter.split(';')) {
            if !s.trim().is_empty() {
                c.execute(s).unwrap();
            }
        }
        let got = match &c
            .query("SELECT sql FROM sqlite_master WHERE name='v'")
            .unwrap()
            .rows[0][0]
        {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        };
        assert!(
            !got.contains(" a ") && !got.contains(",a") && !got.contains(".a "),
            "view still references the old column: {got}"
        );
        if sqlite {
            let want = {
                let o = Command::new("sqlite3")
                    .arg(":memory:")
                    .arg(format!(
                        "{ddl} {alter} SELECT sql FROM sqlite_master WHERE name='v';"
                    ))
                    .output()
                    .unwrap();
                String::from_utf8_lossy(&o.stdout).trim_end().to_string()
            };
            assert_eq!(got, want, "view rewrite diverged for: {view}");
        }
    }
}

#[test]
fn rename_column_propagates_into_multi_source_view() {
    // A MULTI-source view (a join of base tables): a `<renamed-table>.col`
    // reference is always rewritten, and a bare `col` only when that name is
    // unique across the join's sources — byte-for-byte like sqlite3. (A-rn3.)
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let cases: &[(&str, &str)] = &[
        // Unique column `a`: bare + qualified refs all rewrite.
        (
            "CREATE TABLE t(a,b); CREATE TABLE u(c,d); \
             CREATE VIEW v AS SELECT a, c, t.b FROM t JOIN u ON a=c WHERE a>0;",
            "ALTER TABLE t RENAME COLUMN a TO a2;",
        ),
        // Ambiguous column `x` (in both tables): only the `t.x` qualified ref moves.
        (
            "CREATE TABLE t(x,b); CREATE TABLE u(x,d); \
             CREATE VIEW v AS SELECT t.x, u.x FROM t JOIN u ON t.b=u.d;",
            "ALTER TABLE t RENAME COLUMN x TO x2;",
        ),
        // Comma join, unique column.
        (
            "CREATE TABLE t(a,b); CREATE TABLE u(c,d); \
             CREATE VIEW v AS SELECT a,b,c,d FROM t,u WHERE b=d;",
            "ALTER TABLE t RENAME COLUMN a TO aa;",
        ),
    ];
    for (ddl, alter) in cases {
        let mut c = Connection::open_memory().unwrap();
        c.execute_batch(ddl).unwrap();
        c.execute(alter).unwrap();
        let got = match &c
            .query("SELECT sql FROM sqlite_master WHERE name='v'")
            .unwrap()
            .rows[0][0]
        {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        };
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(format!(
                    "{ddl} {alter} SELECT sql FROM sqlite_master WHERE name='v';"
                ))
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        assert_eq!(got, want, "multi-source view rewrite diverged for: {ddl}");
    }
}

#[test]
fn rename_column_propagates_into_single_source_trigger() {
    // Renaming a column propagates into a trigger ON that table whose body
    // references only it: NEW./OLD., table-qualified, and bare refs are all
    // rewritten byte-for-byte like sqlite3. A trigger that also writes another
    // table is left unchanged (uncorrupted) — the scope-aware remainder.
    let sqlite = Command::new("sqlite3").arg("--version").output().is_ok();
    let trig_sql = |create: &[&str]| -> String {
        let mut c = Connection::open_memory().unwrap();
        for s in create {
            c.execute(s).unwrap();
        }
        c.execute("ALTER TABLE t RENAME COLUMN a TO renamed")
            .unwrap();
        match &c
            .query("SELECT sql FROM sqlite_master WHERE name='tr'")
            .unwrap()
            .rows[0][0]
        {
            Value::Text(s) => s.clone(),
            o => panic!("not text: {o:?}"),
        }
    };
    let single = trig_sql(&[
        "CREATE TABLE t(a INT, b INT)",
        "CREATE TRIGGER tr AFTER UPDATE ON t WHEN NEW.a > 0 \
         BEGIN UPDATE t SET b = NEW.a WHERE a = OLD.a; END",
    ]);
    assert!(
        !single.contains("NEW.a ") && single.contains("NEW.renamed"),
        "trigger not propagated: {single}"
    );
    if sqlite {
        let want = {
            let o = Command::new("sqlite3")
                .arg(":memory:")
                .arg(
                    "CREATE TABLE t(a INT, b INT);\
                     CREATE TRIGGER tr AFTER UPDATE ON t WHEN NEW.a > 0 \
                     BEGIN UPDATE t SET b = NEW.a WHERE a = OLD.a; END;\
                     ALTER TABLE t RENAME COLUMN a TO renamed;\
                     SELECT sql FROM sqlite_master WHERE name='tr';",
                )
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).trim_end().to_string()
        };
        assert_eq!(single, want);
    }
    // A trigger that writes to another table is left unchanged (no corruption of
    // the other table's column).
    let multi = trig_sql(&[
        "CREATE TABLE t(a INT, b INT)",
        "CREATE TABLE log(a INT)",
        "CREATE TRIGGER tr AFTER INSERT ON t BEGIN INSERT INTO log VALUES(NEW.a); END",
    ]);
    assert!(
        multi.contains("INSERT INTO log"),
        "trigger corrupted: {multi}"
    );
}

#[test]
fn rename_column_does_not_corrupt_multi_table_view() {
    // A multi-table view needs scope-aware resolution (the A-rn3 remainder); the
    // safe path must NOT corrupt the other table's same-named column — it leaves
    // the view unchanged rather than blindly renaming `u.a`.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a INT, b INT)").unwrap();
    c.execute("CREATE TABLE u(a INT, c INT)").unwrap();
    c.execute("CREATE VIEW v AS SELECT t.a, u.a AS ua FROM t JOIN u ON t.b = u.c")
        .unwrap();
    c.execute("ALTER TABLE t RENAME COLUMN a TO renamed")
        .unwrap();
    let got = match &c
        .query("SELECT sql FROM sqlite_master WHERE name='v'")
        .unwrap()
        .rows[0][0]
    {
        Value::Text(s) => s.clone(),
        o => panic!("not text: {o:?}"),
    };
    // `u.a` (the other table's column) is never touched.
    assert!(got.contains("u.a AS ua"), "corrupted u.a: {got}");
}
