//! Roadmap D3a: the built-in `rtree` virtual-table module (on top of the W1/W2
//! writable+persistent vtab infrastructure). Functionally correct spatial index —
//! rows persist in the backing table and queries are answered by scan + the
//! re-applied WHERE. Coordinates are stored as 32-bit floats (min rounded down,
//! max rounded up) and the id as an integer, byte-for-byte like sqlite3 3.50.4.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

#[test]
fn spatial_filter_and_rowid_alias() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, minX, maxX, minY, maxY)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0,1, 0,1), (2, 5,6, 5,6), (3, 0.5,2, 0.5,2)")
        .unwrap();

    // An overlap query returns the boxes that intersect the search window.
    assert_eq!(
        rows(
            &c,
            "SELECT id FROM r WHERE minX <= 1.5 AND maxX >= 0.5 ORDER BY id"
        ),
        [vec![Value::Integer(1)], vec![Value::Integer(3)]]
    );
    // The first column is the rowid.
    assert_eq!(
        rows(&c, "SELECT rowid, id FROM r WHERE id = 2"),
        [vec![Value::Integer(2), Value::Integer(2)]]
    );
    // Integer-valued coordinates read back as REAL.
    assert_eq!(
        rows(&c, "SELECT minX, maxX FROM r WHERE id = 1"),
        [vec![Value::Real(0.0), Value::Real(1.0)]]
    );
}

#[test]
fn spatial_pushdown_prunes_but_keeps_every_match() {
    // A multi-level node tree (hundreds of entries) exercises the spatial
    // pushdown: the node walk prunes interior subtrees whose MBR can't satisfy
    // the coordinate constraints. The pruned result must still equal the exact
    // brute-force match set (pruning is an optimization, never a filter), across
    // range, open-ended, equality, and empty queries — and combined with a
    // non-spatial predicate that run_core re-applies.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, minX, maxX, minY, maxY)")
        .unwrap();
    // One bulk INSERT (a single rebuild) so building the tree is cheap; the box
    // for id i is the point (i, i) in X with a spread-out Y so the tree branches.
    let n = 300;
    let mut sql = String::from("INSERT INTO r VALUES ");
    for i in 1..=n {
        if i > 1 {
            sql.push(',');
        }
        let y = (i * 7) % 100;
        sql.push_str(&format!("({i},{i},{i},{y},{y})"));
    }
    c.execute(&sql).unwrap();

    let ids = |q: &str| -> Vec<i64> {
        rows(&c, q)
            .iter()
            .map(|r| match r[0] {
                Value::Integer(v) => v,
                ref o => panic!("not int: {o:?}"),
            })
            .collect()
    };
    // Range on X: minX>=50 AND maxX<=60 → points 50..=60 (minX==maxX==i).
    assert_eq!(
        ids("SELECT id FROM r WHERE minX>=50 AND maxX<=60 ORDER BY id"),
        (50..=60).collect::<Vec<_>>()
    );
    // Open-ended upper / lower bounds.
    assert_eq!(
        ids("SELECT id FROM r WHERE maxX<=5 ORDER BY id"),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(
        ids("SELECT id FROM r WHERE minX>=296 ORDER BY id"),
        vec![296, 297, 298, 299, 300]
    );
    // Equality on a coordinate.
    assert_eq!(
        ids("SELECT id FROM r WHERE minX=137 ORDER BY id"),
        vec![137]
    );
    // A 2-D window combining X and Y constraints, checked against brute force.
    let expect_xy: Vec<i64> = (1..=n)
        .filter(|&i| (40..=60).contains(&i) && ((i * 7) % 100) <= 30)
        .collect();
    assert_eq!(
        ids("SELECT id FROM r WHERE minX>=40 AND maxX<=60 AND maxY<=30 ORDER BY id"),
        expect_xy
    );
    // A constraint that prunes everything returns nothing.
    assert!(ids("SELECT id FROM r WHERE minX>=10000").is_empty());
    // Pruning composes with a non-spatial predicate (re-applied by run_core).
    assert_eq!(
        ids("SELECT id FROM r WHERE minX>=50 AND maxX<=60 AND id%2=0 ORDER BY id"),
        vec![50, 52, 54, 56, 58, 60]
    );
}

#[test]
fn coordinates_round_to_f32_like_sqlite() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, lo, hi)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (10, 0.1, 0.3)").unwrap();
    // min `0.1` rounds DOWN to the f32 below it, max `0.3` rounds UP — the exact
    // values sqlite3 3.50.4 stores and returns.
    assert_eq!(
        rows(&c, "SELECT lo, hi FROM r"),
        [vec![
            Value::Real(0.09999998658895493),
            Value::Real(0.30000001192092896),
        ]]
    );
}

#[test]
fn update_and_delete() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0,10), (2, 20,30)")
        .unwrap();
    c.execute("UPDATE r SET b = 100 WHERE id = 1").unwrap();
    assert_eq!(
        rows(&c, "SELECT id, b FROM r WHERE id = 1"),
        [vec![Value::Integer(1), Value::Real(100.0)]]
    );
    c.execute("DELETE FROM r WHERE id = 2").unwrap();
    assert_eq!(
        rows(&c, "SELECT id FROM r ORDER BY id"),
        [vec![Value::Integer(1)]]
    );
}

#[test]
fn rejects_min_greater_than_max_and_bad_arity() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    // min > max is rejected (like sqlite's "rtree constraint failed").
    assert!(c.execute("INSERT INTO r VALUES (1, 5, 2)").is_err());
    // An even column count (no id + 2N coordinates) is rejected.
    assert!(c
        .execute("CREATE VIRTUAL TABLE bad USING rtree(id, a)")
        .is_err());
}

#[test]
fn rows_persist_in_the_node_store() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (7, 1, 2)").unwrap();
    // The row round-trips through the persistent node store, and the storage is
    // SQLite's byte-compatible `_node`/`_rowid`/`_parent` shadow tables (not the
    // generic `_data` backing).
    assert_eq!(
        rows(&c, "SELECT id, a, b FROM r"),
        [vec![Value::Integer(7), Value::Real(1.0), Value::Real(2.0)]]
    );
    assert!(c.query("SELECT data FROM r_node WHERE nodeno=1").is_ok());
    assert!(c.query("SELECT nodeno FROM r_rowid WHERE rowid=7").is_ok());
    assert!(c.query("SELECT * FROM r_data").is_err());
}

#[test]
fn alter_and_index_on_a_virtual_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0, 5), (2, 10, 15)")
        .unwrap();

    // ADD COLUMN / CREATE INDEX on a vtab are rejected (matching sqlite), not the
    // old confusing "schema sql is not CREATE TABLE".
    assert!(c.execute("ALTER TABLE r ADD COLUMN z").is_err());
    assert!(c.execute("CREATE INDEX i ON r(a)").is_err());

    // RENAME works: the vtab and its node shadow tables are all renamed, and the
    // rows survive.
    c.execute("ALTER TABLE r RENAME TO r2").unwrap();
    assert_eq!(
        c.query("SELECT id, a, b FROM r2 ORDER BY id").unwrap().rows,
        [
            vec![Value::Integer(1), Value::Real(0.0), Value::Real(5.0)],
            vec![Value::Integer(2), Value::Real(10.0), Value::Real(15.0)],
        ]
    );
    // The old name is gone; the shadow tables moved too.
    assert!(c.query("SELECT * FROM r").is_err());
    assert!(c.query("SELECT * FROM r2_node").is_ok());
    assert!(c.query("SELECT * FROM r_node").is_err());
}

#[test]
fn drop_removes_the_backing_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0, 5)").unwrap();
    assert!(c.query("SELECT * FROM r_node").is_ok());
    c.execute("DROP TABLE r").unwrap();
    // Both the vtab and its shadow tables are gone.
    assert!(c.query("SELECT * FROM r").is_err());
    assert!(c.query("SELECT * FROM r_node").is_err());
    assert!(c.query("SELECT * FROM r_rowid").is_err());
    assert_eq!(
        c.query("SELECT count(*) FROM sqlite_master").unwrap().rows[0][0],
        Value::Integer(0)
    );
}

#[test]
fn integrity_check_passes_with_a_virtual_table() {
    // integrity_check used to error on a vtab (it has no b-tree of its own);
    // it now skips the vtab and still validates the regular tables + the
    // `<name>_data` backing table.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x INTEGER PRIMARY KEY, y)")
        .unwrap();
    c.execute("CREATE INDEX iy ON t(y)").unwrap();
    c.execute("INSERT INTO t VALUES (1,'a'),(2,'b')").unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0, 5)").unwrap();
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

#[test]
fn vacuum_preserves_a_persistent_virtual_table() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(x)").unwrap();
    c.execute("INSERT INTO t VALUES (1),(2)").unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0, 5), (2, 3, 8)")
        .unwrap();
    c.execute("VACUUM").unwrap();
    // Both the regular table and the vtab (via its backing table) survive.
    assert_eq!(
        rows(&c, "SELECT x FROM t ORDER BY x"),
        [vec![Value::Integer(1)], vec![Value::Integer(2)]]
    );
    assert_eq!(
        rows(&c, "SELECT id, a, b FROM r ORDER BY id"),
        [
            vec![Value::Integer(1), Value::Real(0.0), Value::Real(5.0)],
            vec![Value::Integer(2), Value::Real(3.0), Value::Real(8.0)],
        ]
    );
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
}

#[test]
fn explain_query_plan_matches_sqlite_rtree_index() {
    // The reported `idxNum:idxStr` matches sqlite's rtree xBestIndex: a coordinate
    // range is `INDEX 2:<op><col>…` (D=`>=`, B=`<=`, A=`=`, C=`<`, E=`>`; column
    // digit is 0-based among the coordinates), an `id =` lookup is `INDEX 1:`, and
    // a bare scan is `INDEX 2:`.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, minX, maxX, minY, maxY)")
        .unwrap();
    let plan = |sql: &str| match c.query(sql).unwrap().rows.last().unwrap().last() {
        Some(Value::Text(s)) => s.clone(),
        o => panic!("not text: {o:?}"),
    };
    assert_eq!(
        plan("EXPLAIN QUERY PLAN SELECT id FROM r WHERE minX>=0 AND maxX<=3"),
        "SCAN r VIRTUAL TABLE INDEX 2:D0B1"
    );
    assert_eq!(
        plan(
            "EXPLAIN QUERY PLAN SELECT id FROM r WHERE minX>=0 AND minY>=0 AND maxX<=3 AND maxY<=3"
        ),
        "SCAN r VIRTUAL TABLE INDEX 2:D0D2B1B3"
    );
    assert_eq!(
        plan("EXPLAIN QUERY PLAN SELECT id FROM r WHERE id = 2"),
        "SCAN r VIRTUAL TABLE INDEX 1:"
    );
    assert_eq!(
        plan("EXPLAIN QUERY PLAN SELECT id FROM r"),
        "SCAN r VIRTUAL TABLE INDEX 2:"
    );
}

#[test]
fn foreign_key_list_on_a_vtab_is_empty() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, a, b)")
        .unwrap();
    assert!(c
        .query("PRAGMA foreign_key_list(r)")
        .unwrap()
        .rows
        .is_empty());
}

#[test]
fn table_info_reports_rtree_column_types() {
    // The rtree module declares typed columns (id INT, coords REAL), so table_info
    // matches sqlite byte-for-byte.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, minX, maxX)")
        .unwrap();
    let r = c.query("PRAGMA table_info(r)").unwrap();
    let ty = |i: usize| match &r.rows[i][2] {
        Value::Text(s) => s.clone(),
        o => panic!("not text: {o:?}"),
    };
    assert_eq!(ty(0), "INT");
    assert_eq!(ty(1), "REAL");
    assert_eq!(ty(2), "REAL");
}

#[test]
fn auxiliary_columns_store_and_query() {
    // A `+name` column declares auxiliary (non-spatial) data: stored verbatim,
    // retrievable, and usable in WHERE (via the re-applied filter), byte-for-byte
    // like sqlite3. Its declared type is dropped (empty type in table_info).
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, minX, maxX, +label TEXT, +n)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0,10, 'alpha', 42), (2, 20,30, 'beta', 7)")
        .unwrap();
    // Auxiliary values come back alongside coordinates.
    assert_eq!(
        rows(&c, "SELECT id, label, n FROM r ORDER BY id"),
        [
            vec![
                Value::Integer(1),
                Value::Text("alpha".into()),
                Value::Integer(42)
            ],
            vec![
                Value::Integer(2),
                Value::Text("beta".into()),
                Value::Integer(7)
            ],
        ]
    );
    // A spatial filter still works with auxiliary columns present.
    assert_eq!(
        rows(&c, "SELECT label FROM r WHERE minX <= 5"),
        [vec![Value::Text("alpha".into())]]
    );
    // An auxiliary column is filterable too.
    assert_eq!(
        rows(&c, "SELECT id FROM r WHERE n = 7"),
        [vec![Value::Integer(2)]]
    );
    // The auxiliary column's declared type is not retained.
    let info = c.query("PRAGMA table_info(r)").unwrap();
    assert_eq!(info.rows[3][1], Value::Text("label".into()));
    assert_eq!(info.rows[3][2], Value::Text("".into()));
}

#[test]
fn rtree_i32_stores_integer_coordinates() {
    // The `rtree_i32` variant stores coordinates as 32-bit integers (floats
    // truncate toward zero), with INT-typed columns, byte-for-byte like sqlite3.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree_i32(id, x0, x1)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 5,15), (2, 2.7,8.2), (3, -3.9,4.1)")
        .unwrap();
    // Float inputs truncate toward zero; the values come back as integers.
    assert_eq!(
        rows(&c, "SELECT id, x0, x1 FROM r ORDER BY id"),
        [
            vec![Value::Integer(1), Value::Integer(5), Value::Integer(15)],
            vec![Value::Integer(2), Value::Integer(2), Value::Integer(8)],
            vec![Value::Integer(3), Value::Integer(-3), Value::Integer(4)],
        ]
    );
    // A spatial filter works as for the float variant.
    assert_eq!(
        rows(&c, "SELECT id FROM r WHERE x0 <= 4 ORDER BY id"),
        [vec![Value::Integer(2)], vec![Value::Integer(3)]]
    );
    // Columns are declared INT (not REAL).
    let info = c.query("PRAGMA table_info(r)").unwrap();
    assert_eq!(info.rows[1][2], Value::Text("INT".into()));
}

#[test]
fn duplicate_id_is_rejected() {
    // The rtree `id` is the rowid alias, so inserting an existing id is a UNIQUE
    // conflict (errors / OR IGNORE skips / OR REPLACE overwrites), matching sqlite,
    // rather than silently overwriting the box.
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE r USING rtree(id, x0, x1)")
        .unwrap();
    c.execute("INSERT INTO r VALUES (1, 0, 5)").unwrap();
    // A plain duplicate id fails and leaves the original box in place.
    assert!(c.execute("INSERT INTO r VALUES (1, 9, 9)").is_err());
    assert_eq!(rows(&c, "SELECT x0 FROM r"), [vec![Value::Real(0.0)]]);
    // OR IGNORE skips it; OR REPLACE overwrites it.
    c.execute("INSERT OR IGNORE INTO r VALUES (1, 9, 9)")
        .unwrap();
    assert_eq!(rows(&c, "SELECT x0 FROM r"), [vec![Value::Real(0.0)]]);
    c.execute("INSERT OR REPLACE INTO r VALUES (1, 9, 9)")
        .unwrap();
    assert_eq!(rows(&c, "SELECT x0 FROM r"), [vec![Value::Real(9.0)]]);
    // A NULL/absent id still auto-assigns (no false conflict).
    c.execute("INSERT INTO r(x0, x1) VALUES (2, 7)").unwrap();
    assert_eq!(
        rows(&c, "SELECT count(*) FROM r"),
        [vec![Value::Integer(2)]]
    );
}

// ── D3c (M1): read SQLite's byte-compatible R-Tree on-disk format ────────────

/// graphite reads an R-Tree written by `sqlite3` in its native `<name>_node` /
/// `_rowid` / `_parent` shadow-table format (a b-tree of nodes), not graphite's
/// own `<name>_data` storage — so a sqlite-written R-Tree queries correctly.
#[test]
fn reads_sqlite_written_rtree_node_format() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-rtree-d3c-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    // Enough rows to force a multi-level node tree (interior nodes).
    let mut script =
        String::from("CREATE VIRTUAL TABLE rt USING rtree(id, minx, maxx, miny, maxy);");
    for i in 0..250 {
        script.push_str(&format!(
            "INSERT INTO rt VALUES({i},{}.5,{}.5,{}.0,{}.0);",
            i,
            i + 1,
            i * 2,
            i * 2 + 1
        ));
    }
    let o = Command::new("sqlite3")
        .arg(&path)
        .arg(&script)
        .output()
        .unwrap();
    assert!(o.status.success(), "sqlite build failed: {o:?}");

    let c = Connection::open(&path).unwrap();
    // The shadow tables are sqlite's, not graphite's `_data`.
    assert!(
        c.query("SELECT 1 FROM sqlite_master WHERE name='rt_node'")
            .unwrap()
            .rows
            .len()
            == 1
    );
    assert!(c
        .query("SELECT 1 FROM sqlite_master WHERE name='rt_data'")
        .unwrap()
        .rows
        .is_empty());

    let sqlite = |q: &str| {
        let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
        let mut v: Vec<String> = String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        v.sort();
        v.join("~")
    };
    let graph = |q: &str| {
        let mut v: Vec<String> = c
            .query(q)
            .unwrap()
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|x| match x {
                        Value::Integer(i) => i.to_string(),
                        Value::Real(f) => graphitesql::exec::eval::format_real(*f),
                        Value::Null => String::new(),
                        Value::Text(s) => s.clone(),
                        Value::Blob(_) => "b".into(),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect();
        v.sort();
        v.join("~")
    };
    for q in [
        "SELECT count(*) FROM rt",
        "SELECT id FROM rt WHERE minx>=100 AND minx<105 ORDER BY id",
        "SELECT id, minx, maxx, miny, maxy FROM rt WHERE id=137",
        "SELECT max(maxx), min(minx) FROM rt",
        "SELECT id FROM rt WHERE minx<2 ORDER BY id",
    ] {
        assert_eq!(graph(q), sqlite(q), "diverged: {q}");
    }
    // graphite reads it as a valid database.
    assert_eq!(
        c.query("PRAGMA integrity_check").unwrap().rows[0][0],
        Value::Text("ok".into())
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn reads_sqlite_written_rtree_i32_node_format() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-rtreei32-d3c-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    let o = Command::new("sqlite3")
        .arg(&path)
        .arg("CREATE VIRTUAL TABLE ri USING rtree_i32(id, x0, x1); INSERT INTO ri VALUES(1,-5,5),(2,10,20),(3,0,1);")
        .output()
        .unwrap();
    assert!(o.status.success(), "{o:?}");
    let c = Connection::open(&path).unwrap();
    let r = c.query("SELECT id, x0, x1 FROM ri ORDER BY id").unwrap();
    assert_eq!(r.rows.len(), 3);
    assert_eq!(
        r.rows[0],
        vec![Value::Integer(1), Value::Integer(-5), Value::Integer(5)]
    );
    assert_eq!(
        r.rows[1],
        vec![Value::Integer(2), Value::Integer(10), Value::Integer(20)]
    );
    let _ = std::fs::remove_file(&path);
}

// ── D3c (M2): graphite WRITES the byte-compatible R-Tree node format ─────────

/// A graphite-written R-Tree (no aux columns) now uses SQLite's node format, so
/// sqlite3 reads it with `rtreecheck` / `integrity_check` ok and correct queries
/// — across inserts that force a multi-level tree, plus delete and update.
#[test]
fn graphite_written_rtree_is_read_by_sqlite3() {
    use std::process::Command;
    if Command::new("sqlite3").arg("--version").output().is_err() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = std::env::temp_dir().join(format!("gsql-rtree-m2-{}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE VIRTUAL TABLE rt USING rtree(id, minx, maxx, miny, maxy)")
            .unwrap();
        // A single multi-row INSERT of >51 entries (one rebuild) forces a
        // multi-level node tree (interior root + several leaves).
        let vals = (0..150)
            .map(|i| format!("({i},{}.0,{}.0,{}.0,{}.0)", i, i + 1, i * 2, i * 2 + 1))
            .collect::<Vec<_>>()
            .join(",");
        c.execute(&format!("INSERT INTO rt VALUES {vals}")).unwrap();
        c.execute("DELETE FROM rt WHERE id=77").unwrap();
        c.execute("UPDATE rt SET maxx=999 WHERE id=12").unwrap();
    }
    let chk = |q: &str| {
        let o = Command::new("sqlite3").arg(&path).arg(q).output().unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    assert_eq!(chk("SELECT rtreecheck('rt')"), "ok", "rtreecheck");
    assert_eq!(chk("PRAGMA integrity_check"), "ok");
    assert_eq!(chk("SELECT count(*) FROM rt"), "149");
    assert_eq!(chk("SELECT id FROM rt WHERE id=77"), "");
    assert_eq!(chk("SELECT maxx FROM rt WHERE id=12"), "999.0");
    assert_eq!(
        chk("SELECT group_concat(id) FROM (SELECT id FROM rt WHERE minx>=40 AND minx<=43 ORDER BY id)"),
        "40,41,42,43"
    );
    // The file uses sqlite's three shadow tables, with no `_data`.
    assert_eq!(
        chk("SELECT count(*) FROM sqlite_master WHERE name IN('rt_node','rt_rowid','rt_parent')"),
        "3"
    );
    assert_eq!(
        chk("SELECT count(*) FROM sqlite_master WHERE name='rt_data'"),
        "0"
    );
    let _ = std::fs::remove_file(&path);
}
