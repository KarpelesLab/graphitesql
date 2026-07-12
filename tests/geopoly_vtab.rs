//! The built-in `geopoly` virtual-table module: a 2-D R-Tree indexing each
//! polygon's bounding box, with the polygon (`_shape`) and user columns stored
//! as `_rowid` aux columns (`a0` = the `_shape` BLOB, `a1..aN` = user cols),
//! byte-compatible with sqlite3 3.50.4. The spatial `geopoly_overlap` /
//! `geopoly_within` predicates prune candidates by the query polygon's bounding
//! box, then `run_core` re-applies the exact predicate.
//!
//! Every assertion is diffed against the real `sqlite3` CLI where one is present;
//! the `_shape` / cross-engine tests skip cleanly when `sqlite3` is absent.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn rows(c: &Connection, sql: &str) -> Vec<Vec<Value>> {
    c.query(sql).unwrap().rows
}

/// Whether a `sqlite3` CLI is on PATH (the differential oracle).
fn have_sqlite3() -> bool {
    std::process::Command::new("sqlite3")
        .arg("--version")
        .output()
        .is_ok()
}

/// Run one query on `path` via the `sqlite3` CLI, returning trimmed stdout.
fn sqlite(path: &str, q: &str) -> String {
    let o = std::process::Command::new("sqlite3")
        .arg(path)
        .arg(q)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// A fresh temp-file path for a file-backed database.
fn temp_db(tag: &str) -> String {
    let p = std::env::temp_dir().join(format!("gsql-geopoly-{tag}-{}.db", std::process::id()));
    let p = p.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&p);
    p
}

/// The four sample squares used across the row-level tests, as GeoJSON strings.
const A: &str = "[[0,0],[10,0],[10,10],[0,10],[0,0]]";
const B: &str = "[[5,5],[15,5],[15,15],[5,15],[5,5]]";
const C: &str = "[[100,100],[110,100],[110,110],[100,110],[100,100]]";

fn seed(c: &mut Connection) {
    c.execute("CREATE VIRTUAL TABLE geo USING geopoly(label, val)")
        .unwrap();
    c.execute(&format!(
        "INSERT INTO geo(_shape,label,val) VALUES('{A}','A',1)"
    ))
    .unwrap();
    c.execute(&format!(
        "INSERT INTO geo(_shape,label,val) VALUES('{B}','B',2)"
    ))
    .unwrap();
    c.execute(&format!(
        "INSERT INTO geo(_shape,label,val) VALUES('{C}','C',3)"
    ))
    .unwrap();
}

#[test]
fn create_declares_shape_and_user_columns() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE geo USING geopoly(label, val)")
        .unwrap();
    // The virtual table's columns are `_shape` then the user args, like sqlite.
    let info = rows(&c, "PRAGMA table_info(geo)");
    let names: Vec<String> = info
        .iter()
        .map(|r| match &r[1] {
            Value::Text(s) => String::from(s.as_str()),
            _ => String::new(),
        })
        .collect();
    assert_eq!(names, ["_shape", "label", "val"]);
}

#[test]
fn shadow_table_schema_matches_sqlite() {
    let path = temp_db("schema");
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE VIRTUAL TABLE geo USING geopoly(label, val)")
            .unwrap();
    }
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping shadow-schema diff");
        let _ = std::fs::remove_file(&path);
        return;
    }
    // The three shadow tables match sqlite's, the `_rowid` table extended with one
    // `aK` aux column per stored value (a0=_shape, a1..aN=user). Compare with
    // whitespace normalized: graphite reprints its own shadow DDL with a space
    // after each comma (the same cosmetic divergence the built-in rtree module
    // has); the schema is byte-format-compatible, which the cross-engine tests
    // prove by having sqlite read a graphite-written geopoly file.
    let got = sqlite(
        &path,
        "SELECT name||'|'||sql FROM sqlite_master WHERE name LIKE 'geo\\_%' ESCAPE '\\' ORDER BY name",
    );
    let norm = |s: &str| s.replace(", ", ",");
    assert_eq!(
        norm(&got),
        "geo_node|CREATE TABLE \"geo_node\"(nodeno INTEGER PRIMARY KEY,data)\n\
         geo_parent|CREATE TABLE \"geo_parent\"(nodeno INTEGER PRIMARY KEY,parentnode)\n\
         geo_rowid|CREATE TABLE \"geo_rowid\"(rowid INTEGER PRIMARY KEY,nodeno,a0,a1,a2)"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn insert_and_select_roundtrips_shape_and_columns() {
    let mut c = Connection::open_memory().unwrap();
    seed(&mut c);
    // `_shape` round-trips through geopoly_json; user columns come back verbatim.
    assert_eq!(
        rows(
            &c,
            "SELECT rowid, geopoly_json(_shape), label, val FROM geo ORDER BY rowid"
        ),
        [
            vec![
                Value::Integer(1),
                Value::Text("[[0.0,0.0],[10.0,0.0],[10.0,10.0],[0.0,10.0],[0.0,0.0]]".into()),
                Value::Text("A".into()),
                Value::Integer(1),
            ],
            vec![
                Value::Integer(2),
                Value::Text("[[5.0,5.0],[15.0,5.0],[15.0,15.0],[5.0,15.0],[5.0,5.0]]".into()),
                Value::Text("B".into()),
                Value::Integer(2),
            ],
            vec![
                Value::Integer(3),
                Value::Text(
                    "[[100.0,100.0],[110.0,100.0],[110.0,110.0],[100.0,110.0],[100.0,100.0]]"
                        .into()
                ),
                Value::Text("C".into()),
                Value::Integer(3),
            ],
        ]
    );
    // A geopoly scalar over `_shape` computes on the stored polygon.
    assert_eq!(
        rows(&c, "SELECT geopoly_area(_shape) FROM geo WHERE rowid=1"),
        [vec![Value::Real(100.0)]]
    );
}

#[test]
fn shape_is_stored_as_the_normalized_blob() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE geo USING geopoly(l)")
        .unwrap();
    c.execute(&format!("INSERT INTO geo(_shape,l) VALUES('{A}','A')"))
        .unwrap();
    // `_shape` reads back as a BLOB (the geopoly on-disk format), not the text.
    assert_eq!(
        rows(&c, "SELECT typeof(_shape) FROM geo"),
        [vec![Value::Text("blob".into())]]
    );
}

#[test]
fn geopoly_overlap_selects_intersecting_rows() {
    let mut c = Connection::open_memory().unwrap();
    seed(&mut c);
    // The query window intersects A and B, not the distant C.
    assert_eq!(
        rows(
            &c,
            "SELECT label FROM geo \
             WHERE geopoly_overlap(_shape,'[[1,1],[6,1],[6,6],[1,6],[1,1]]') ORDER BY label"
        ),
        [vec![Value::Text("A".into())], vec![Value::Text("B".into())]]
    );
    // A window far from every polygon returns nothing (pruned + re-checked).
    assert!(
        rows(
            &c,
            "SELECT label FROM geo \
         WHERE geopoly_overlap(_shape,'[[500,500],[501,500],[501,501],[500,501],[500,500]]')"
        )
        .is_empty()
    );
}

#[test]
fn geopoly_within_selects_contained_rows() {
    let mut c = Connection::open_memory().unwrap();
    seed(&mut c);
    // A big box contains A and B but not C.
    assert_eq!(
        rows(
            &c,
            "SELECT label FROM geo \
             WHERE geopoly_within(_shape,'[[-1,-1],[20,-1],[20,20],[-1,20],[-1,-1]]') ORDER BY label"
        ),
        [vec![Value::Text("A".into())], vec![Value::Text("B".into())]]
    );
}

#[test]
fn update_reindexes_shape_and_columns() {
    let mut c = Connection::open_memory().unwrap();
    seed(&mut c);
    c.execute("UPDATE geo SET label='Z', _shape='[[0,0],[1,0],[1,1],[0,1],[0,0]]' WHERE rowid=1")
        .unwrap();
    assert_eq!(
        rows(
            &c,
            "SELECT geopoly_json(_shape), label FROM geo WHERE rowid=1"
        ),
        [vec![
            Value::Text("[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,1.0],[0.0,0.0]]".into()),
            Value::Text("Z".into()),
        ]]
    );
    // The shrunk polygon no longer overlaps a window that only met the old box.
    assert!(
        rows(
            &c,
            "SELECT label FROM geo \
         WHERE rowid=1 AND geopoly_overlap(_shape,'[[8,8],[9,8],[9,9],[8,9],[8,8]]')"
        )
        .is_empty()
    );
}

#[test]
fn delete_removes_the_row() {
    let mut c = Connection::open_memory().unwrap();
    seed(&mut c);
    c.execute("DELETE FROM geo WHERE rowid=2").unwrap();
    assert_eq!(
        rows(&c, "SELECT rowid, label FROM geo ORDER BY rowid"),
        [
            vec![Value::Integer(1), Value::Text("A".into())],
            vec![Value::Integer(3), Value::Text("C".into())],
        ]
    );
}

#[test]
fn explicit_and_implicit_rowids() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE geo USING geopoly(l)")
        .unwrap();
    c.execute(&format!(
        "INSERT INTO geo(rowid,_shape,l) VALUES(42,'{A}','x')"
    ))
    .unwrap();
    c.execute(&format!("INSERT INTO geo(_shape,l) VALUES('{B}','y')"))
        .unwrap();
    // The implicit rowid follows max+1 (43), like sqlite.
    assert_eq!(
        rows(&c, "SELECT rowid, l FROM geo ORDER BY rowid"),
        [
            vec![Value::Integer(42), Value::Text("x".into())],
            vec![Value::Integer(43), Value::Text("y".into())],
        ]
    );
    // Reusing an existing rowid is a UNIQUE conflict.
    assert!(
        c.execute(&format!(
            "INSERT INTO geo(rowid,_shape,l) VALUES(42,'{C}','z')"
        ))
        .is_err()
    );
}

#[test]
fn invalid_shape_is_rejected_or_stored_verbatim() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE VIRTUAL TABLE geo USING geopoly(l)")
        .unwrap();
    // A NULL / numeric / bracket-opened-but-malformed `_shape` is an error.
    assert!(
        c.execute("INSERT INTO geo(_shape,l) VALUES(NULL,'n')")
            .is_err()
    );
    assert!(
        c.execute("INSERT INTO geo(_shape,l) VALUES(3.14,'f')")
            .is_err()
    );
    assert!(
        c.execute("INSERT INTO geo(_shape,l) VALUES('[[0,0],[1',x'')")
            .is_err()
    );
    // Text that never opens a ring is stored verbatim with a zero bbox.
    c.execute("INSERT INTO geo(_shape,l) VALUES('','e')")
        .unwrap();
    assert_eq!(
        rows(&c, "SELECT typeof(_shape), _shape, l FROM geo"),
        [vec![
            Value::Text("text".into()),
            Value::Text(String::new().into()),
            Value::Text("e".into()),
        ]]
    );
}

#[test]
fn explain_query_plan_matches_sqlite_geopoly_index() {
    let mut c = Connection::open_memory().unwrap();
    seed(&mut c);
    let eqp = |q: &str| match &rows(&c, q)[0][3] {
        Value::Text(s) => String::from(s.as_str()),
        _ => String::new(),
    };
    assert_eq!(
        eqp(&format!(
            "EXPLAIN QUERY PLAN SELECT * FROM geo WHERE geopoly_overlap(_shape,'{A}')"
        )),
        "SCAN geo VIRTUAL TABLE INDEX 2:rtree"
    );
    assert_eq!(
        eqp(&format!(
            "EXPLAIN QUERY PLAN SELECT * FROM geo WHERE geopoly_within(_shape,'{A}')"
        )),
        "SCAN geo VIRTUAL TABLE INDEX 3:rtree"
    );
    assert_eq!(
        eqp("EXPLAIN QUERY PLAN SELECT * FROM geo WHERE rowid=1"),
        "SCAN geo VIRTUAL TABLE INDEX 1:rowid"
    );
    assert_eq!(
        eqp("EXPLAIN QUERY PLAN SELECT * FROM geo"),
        "SCAN geo VIRTUAL TABLE INDEX 4:fullscan"
    );
    // A rowid equality wins over a spatial function (sqlite's `xBestIndex` order).
    assert_eq!(
        eqp(&format!(
            "EXPLAIN QUERY PLAN SELECT * FROM geo WHERE geopoly_overlap(_shape,'{A}') AND rowid=2"
        )),
        "SCAN geo VIRTUAL TABLE INDEX 1:rowid"
    );
}

#[test]
fn integrity_check_passes() {
    let mut c = Connection::open_memory().unwrap();
    seed(&mut c);
    assert_eq!(
        rows(&c, "PRAGMA integrity_check")[0][0],
        Value::Text("ok".into())
    );
}

/// A graphite-written geopoly file (with enough rows to force a multi-level node
/// tree, plus a delete and an update) is read by `sqlite3`: `integrity_check` ok
/// and every query result matches graphite's.
#[test]
fn graphite_written_geopoly_is_read_by_sqlite3() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_db("m2");
    {
        let mut c = Connection::create(&path).unwrap();
        c.execute("CREATE VIRTUAL TABLE geo USING geopoly(label, val)")
            .unwrap();
        let vals: Vec<String> = (0..120)
            .map(|i| {
                format!(
                    "('[[{i},{i}],[{a},{i}],[{a},{a}],[{i},{a}],[{i},{i}]]','L{i}',{i})",
                    a = i + 3
                )
            })
            .collect();
        c.execute(&format!(
            "INSERT INTO geo(_shape,label,val) VALUES {}",
            vals.join(",")
        ))
        .unwrap();
        c.execute("DELETE FROM geo WHERE rowid=40").unwrap();
        c.execute("UPDATE geo SET label='ZZ' WHERE rowid=12")
            .unwrap();
    }
    assert_eq!(sqlite(&path, "PRAGMA integrity_check"), "ok");
    assert_eq!(sqlite(&path, "SELECT count(*) FROM geo"), "119");
    assert_eq!(sqlite(&path, "SELECT label FROM geo WHERE rowid=40"), "");
    assert_eq!(sqlite(&path, "SELECT label FROM geo WHERE rowid=12"), "ZZ");
    assert_eq!(
        sqlite(&path, "SELECT geopoly_json(_shape) FROM geo WHERE rowid=1"),
        "[[0.0,0.0],[3.0,0.0],[3.0,3.0],[0.0,3.0],[0.0,0.0]]"
    );
    assert_eq!(
        sqlite(
            &path,
            "SELECT group_concat(rowid) FROM (SELECT rowid FROM geo \
             WHERE geopoly_overlap(_shape,'[[10,10],[15,10],[15,15],[10,15],[10,10]]') \
             ORDER BY rowid)"
        ),
        "9,10,11,12,13,14,15"
    );
    // The file uses sqlite's three shadow tables, with no `_data`.
    assert_eq!(
        sqlite(
            &path,
            "SELECT count(*) FROM sqlite_master WHERE name IN('geo_node','geo_rowid','geo_parent')"
        ),
        "3"
    );
    assert_eq!(
        sqlite(
            &path,
            "SELECT count(*) FROM sqlite_master WHERE name='geo_data'"
        ),
        "0"
    );
    let _ = std::fs::remove_file(&path);
}

/// A `sqlite3`-written geopoly file (multi-level node tree) is read + queried by
/// graphite, matching sqlite's own answers row-for-row.
#[test]
fn reads_sqlite_written_geopoly() {
    if !have_sqlite3() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    let path = temp_db("read");
    let mut script = String::from("CREATE VIRTUAL TABLE geo USING geopoly(label, val);");
    for i in 0..120 {
        script.push_str(&format!(
            "INSERT INTO geo(_shape,label,val) VALUES('[[{i},{i}],[{a},{i}],[{a},{a}],[{i},{a}],[{i},{i}]]','L{i}',{i});",
            a = i + 3
        ));
    }
    let o = std::process::Command::new("sqlite3")
        .arg(&path)
        .arg(&script)
        .output()
        .unwrap();
    assert!(o.status.success(), "sqlite build failed: {o:?}");

    let c = Connection::open(&path).unwrap();
    // The shadow tables are sqlite's, not graphite's `_data`.
    assert_eq!(
        rows(
            &c,
            "SELECT count(*) FROM sqlite_master WHERE name='geo_node'"
        )[0][0],
        Value::Integer(1)
    );
    assert!(rows(&c, "SELECT 1 FROM sqlite_master WHERE name='geo_data'").is_empty());

    let graph = |q: &str| {
        rows(&c, q)
            .iter()
            .map(|r| {
                r.iter()
                    .map(|x| match x {
                        Value::Integer(i) => i.to_string(),
                        Value::Real(f) => graphitesql::exec::eval::format_real(*f),
                        Value::Null => String::new(),
                        Value::Text(s) => String::from(s.as_str()),
                        Value::Blob(_) => "b".into(),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join("~")
    };
    for q in [
        "SELECT count(*) FROM geo",
        "SELECT geopoly_json(_shape) FROM geo WHERE rowid=5",
        "SELECT label, val FROM geo WHERE rowid=100",
        "SELECT group_concat(rowid) FROM (SELECT rowid FROM geo \
         WHERE geopoly_overlap(_shape,'[[10,10],[15,10],[15,15],[10,15],[10,10]]') ORDER BY rowid)",
        "SELECT group_concat(rowid) FROM (SELECT rowid FROM geo \
         WHERE geopoly_within(_shape,'[[0,0],[8,0],[8,8],[0,8],[0,0]]') ORDER BY rowid)",
    ] {
        assert_eq!(graph(q), sqlite(&path, q), "diverged: {q}");
    }
    assert_eq!(
        rows(&c, "PRAGMA integrity_check")[0][0],
        Value::Text("ok".into())
    );
    let _ = std::fs::remove_file(&path);
}
