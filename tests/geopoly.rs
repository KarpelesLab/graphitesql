//! The scalar geometry library of SQLite's `geopoly` extension (the scalar
//! functions and the `geopoly_group_bbox` aggregate — not the virtual table).
//! Every function is verified byte-for-byte against the `sqlite3` CLI.

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
        Value::Text(s) => s.clone(),
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

/// Run each query through graphite (via `Connection`) and the real `sqlite3`
/// CLI, asserting byte-equality of the rendered result rows.
fn diff_against_sqlite3(setup: &[&str], queries: &[&str]) {
    if !sqlite3_available() {
        eprintln!("sqlite3 not found; skipping");
        return;
    }
    // A unique db path per call: the tests run in parallel within one binary, so
    // a shared `process::id()` path would race (one test's cleanup clobbers
    // another's file). An atomic counter disambiguates.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("gsql-geopoly-{}-{n}.db", std::process::id()));
    let path = path.to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    let mut g = Connection::open_memory().unwrap();
    for s in setup {
        let out = Command::new("sqlite3").arg(&path).arg(s).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        g.execute(s).unwrap();
    }

    let mut failures = Vec::new();
    for &q in queries {
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
        "{} geopoly queries diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn json_blob_area_roundtrip() {
    diff_against_sqlite3(
        &[],
        &[
            // json render (closed ring), including negative/fractional coords
            "SELECT geopoly_json('[[0,0],[4,0],[4,3],[0,3],[0,0]]')",
            "SELECT geopoly_json('[[0.5,0.25],[1,0],[1,1],[0.5,0.25]]')",
            "SELECT geopoly_json('[[-1.5,-2.5],[3,0],[0,4],[-1.5,-2.5]]')",
            // blob hex (the exact on-disk byte format)
            "SELECT hex(geopoly_blob('[[0,0],[1,0],[1,1],[0,0]]'))",
            "SELECT hex(geopoly_blob('[[0,0],[4,0],[4,3],[0,3],[0,0]]'))",
            // json -> blob -> json round-trips (BLOB input path)
            "SELECT geopoly_json(geopoly_blob('[[0,0],[4,0],[4,3],[0,3],[0,0]]'))",
            "SELECT geopoly_area(geopoly_blob('[[0,0],[4,0],[4,3],[0,0]]'))",
            // area: CCW positive, CW negative, fractional
            "SELECT geopoly_area('[[0,0],[4,0],[4,3],[0,0]]')",
            "SELECT geopoly_area('[[0,0],[4,3],[4,0],[0,0]]')",
            "SELECT geopoly_area('[[0.0,0.0],[4.5,0.0],[4.5,3.5],[0.0,3.5],[0.0,0.0]]')",
            "SELECT typeof(geopoly_area('[[0,0],[4,0],[4,3],[0,0]]'))",
        ],
    );
}

#[test]
fn invalid_input_is_null() {
    diff_against_sqlite3(
        &[],
        &[
            "SELECT geopoly_json('not json')",
            "SELECT geopoly_area(NULL)",
            "SELECT geopoly_area('[[0,0],[1,1]]')", // <4 pairs
            "SELECT geopoly_json('[[0,0],[2,0],[2,2],[0,2]]')", // unclosed ring
            "SELECT geopoly_json('[[0,0],[1,0],[1,1]]')", // too few
            "SELECT geopoly_json('[[00,0],[1,0],[1,1],[0,0]]')", // leading zero
            "SELECT geopoly_json('[[0,0],[1,0],[1,1],[0,0]] extra')", // trailing junk
            "SELECT geopoly_json(x'00')",           // bad blob
            "SELECT geopoly_area(12345)",           // integer input
            // lenient acceptances that must round-trip
            "SELECT geopoly_json('  [ [0,0] , [1,0] , [1,1] , [0,0] ]  ')",
            "SELECT geopoly_json('[[0,0],[1,0],[1,1],[0,0],]')",
            "SELECT geopoly_json('[[0,0,9],[1,0,9],[1,1,9],[0,0,9]]')",
            "SELECT geopoly_area('[[.5,0],[1,0],[1,1],[.5,0]]')",
            "SELECT geopoly_json('[[0,0],[1,0],[1,1],[0,1.5e2]]')",
        ],
    );
}

#[test]
fn bbox_ccw() {
    diff_against_sqlite3(
        &[],
        &[
            "SELECT geopoly_json(geopoly_bbox('[[0,0],[4,0],[4,3],[0,3],[0,0]]'))",
            "SELECT hex(geopoly_bbox('[[1,1],[5,2],[3,7],[-1,4],[1,1]]'))",
            // ccw: a CCW input is unchanged; a CW input is reversed
            "SELECT geopoly_json(geopoly_ccw('[[0,0],[4,0],[4,3],[0,0]]'))",
            "SELECT geopoly_json(geopoly_ccw('[[0,0],[4,3],[4,0],[0,0]]'))",
            "SELECT hex(geopoly_ccw('[[0,0],[4,3],[4,0],[0,3],[0,0]]'))",
        ],
    );
}

#[test]
fn regular() {
    diff_against_sqlite3(
        &[],
        &[
            "SELECT geopoly_json(geopoly_regular(0,0,10,3))",
            "SELECT geopoly_json(geopoly_regular(0,0,10,4))",
            "SELECT geopoly_json(geopoly_regular(0,0,10,6))",
            "SELECT geopoly_json(geopoly_regular(5,5,2,4))",
            "SELECT geopoly_json(geopoly_regular(-3.5,2.25,7,8))",
            "SELECT hex(geopoly_regular(1.5,-2.5,3.7,13))",
            "SELECT hex(geopoly_regular(0,0,1e6,60))",
            // bounds / NULL behaviour
            "SELECT geopoly_regular(0,0,10,2)", // n<3 -> NULL
            "SELECT geopoly_regular(0,0,0,4)",  // r<=0 -> NULL
            "SELECT geopoly_regular(0,0,-1,4)", // r<0 -> NULL
            "SELECT geopoly_regular(0,0,10,1001) IS NULL", // n>1000 clamps (not null)
            "SELECT length(geopoly_blob(geopoly_regular(0,0,10,1001)))",
            "SELECT length(geopoly_blob(geopoly_regular(0,0,10,1000)))",
        ],
    );
}

#[test]
fn contains_point() {
    diff_against_sqlite3(
        &[],
        &[
            // strictly inside -> 2
            "SELECT geopoly_contains_point('[[0,0],[4,0],[4,4],[0,4],[0,0]]', 2, 2)",
            // on a vertex / edge -> 1
            "SELECT geopoly_contains_point('[[0,0],[4,0],[4,4],[0,4],[0,0]]', 0, 0)",
            "SELECT geopoly_contains_point('[[0,0],[4,0],[4,4],[0,4],[0,0]]', 2, 0)",
            "SELECT geopoly_contains_point('[[0,0],[4,0],[4,4],[0,4],[0,0]]', 4, 2)",
            "SELECT geopoly_contains_point('[[0,0],[4,0],[4,4],[0,4],[0,0]]', 0, 2)",
            // outside -> 0
            "SELECT geopoly_contains_point('[[0,0],[4,0],[4,4],[0,4],[0,0]]', 5, 5)",
            "SELECT geopoly_contains_point('[[0,0],[4,0],[4,4],[0,4],[0,0]]', -1, 2)",
            // string coordinates coerce like sqlite
            "SELECT geopoly_contains_point('[[0,0],[4,0],[4,4],[0,4],[0,0]]', '2', '2')",
            // invalid polygon -> NULL
            "SELECT geopoly_contains_point('bad', 1, 1)",
        ],
    );
}

#[test]
fn overlap_within() {
    let a = "[[0,0],[3,0],[3,3],[0,3],[0,0]]";
    let inner = "[[1,1],[2,1],[2,2],[1,2],[1,1]]";
    let touch = "[[3,0],[6,0],[6,3],[3,3],[3,0]]";
    let cross = "[[1,1],[4,1],[4,4],[1,4],[1,1]]";
    let apart = "[[10,10],[11,10],[11,11],[10,11],[10,10]]";
    let queries = [
        // all five overlap codes
        format!("SELECT geopoly_overlap('{a}','{apart}')"), // 0 disjoint
        format!("SELECT geopoly_overlap('{a}','{touch}')"), // 0 edge-touch only
        format!("SELECT geopoly_overlap('{a}','{cross}')"), // 1 crossing
        format!("SELECT geopoly_overlap('{inner}','{a}')"), // 2 P1 in P2
        format!("SELECT geopoly_overlap('{a}','{inner}')"), // 3 P2 in P1
        format!("SELECT geopoly_overlap('{a}','{a}')"),     // 4 identical
        // within codes: 2 equal, 1 strictly within, 0 not
        format!("SELECT geopoly_within('{a}','{a}')"),
        format!("SELECT geopoly_within('{a}','{inner}')"),
        format!("SELECT geopoly_within('{inner}','{a}')"),
        format!("SELECT geopoly_within('{a}','{apart}')"),
        // NULL propagation
        format!("SELECT geopoly_overlap(NULL,'{a}')"),
        format!("SELECT geopoly_within('nope','{a}')"),
    ];
    let refs: Vec<&str> = queries.iter().map(String::as_str).collect();
    diff_against_sqlite3(&[], &refs);
}

#[test]
fn svg_xform() {
    diff_against_sqlite3(
        &[],
        &[
            "SELECT geopoly_svg('[[0,0],[1,0],[1,1],[0,0]]','fill=\"red\"')",
            "SELECT geopoly_svg('[[0,0],[1,0],[1,1],[0,0]]','fill=\"red\"','stroke=\"blue\"')",
            "SELECT geopoly_svg('[[0.5,0.25],[1,0],[1,1],[0.5,0.25]]')",
            "SELECT geopoly_svg('[[0,0],[1,0],[1,1],[0,0]]', NULL, 'a=1')",
            // xform: translate, scale, rotate-ish
            "SELECT geopoly_json(geopoly_xform('[[0,0],[1,0],[1,1],[0,0]]',1,0,0,1,5,10))",
            "SELECT geopoly_json(geopoly_xform('[[0,0],[1,0],[1,1],[0,0]]',2,0,0,2,0,0))",
            "SELECT geopoly_json(geopoly_xform('[[0,0],[1,0],[1,1],[0,0]]',0.1,0,0,0.1,0,0))",
            "SELECT hex(geopoly_xform('[[0,0],[4,0],[4,3],[0,3],[0,0]]',0,-1,1,0,0,0))",
            "SELECT geopoly_xform(NULL,1,0,0,1,0,0)",
        ],
    );
}

#[test]
fn group_bbox_aggregate() {
    diff_against_sqlite3(
        &["CREATE TABLE t(g)", "INSERT INTO t VALUES('[[0,0],[1,0],[1,1],[0,1],[0,0]]'),('[[5,5],[6,5],[6,6],[5,6],[5,5]]')"],
        &[
            // union of the two bboxes -> the enclosing rectangle
            "SELECT geopoly_json(geopoly_group_bbox(g)) FROM t",
            // works on blob inputs too
            "SELECT geopoly_json(geopoly_group_bbox(geopoly_blob(g))) FROM t",
            // grouped
            "SELECT geopoly_json(geopoly_group_bbox(g)) FROM t GROUP BY (g LIKE '[[5%')",
        ],
    );
    // NULL / all-invalid groups
    diff_against_sqlite3(
        &[
            "CREATE TABLE u(g)",
            "INSERT INTO u VALUES(NULL),('garbage')",
        ],
        &[
            // A group of a NULL and a non-`[` text: NULL is skipped, the text
            // contributes a zero bbox (SQLite's rc-OK-but-no-polygon quirk).
            "SELECT hex(geopoly_group_bbox(g)) FROM u",
            "SELECT geopoly_group_bbox(g) IS NULL FROM u WHERE 0", // empty -> NULL
        ],
    );
}

/// A pure-`Connection` smoke test (independent of the CLI): the documented
/// return codes and the blob byte format.
#[test]
fn connection_smoke() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        rows_str(&c, "SELECT geopoly_area('[[0,0],[4,0],[4,3],[0,0]]')"),
        "6.0"
    );
    assert_eq!(
        rows_str(&c, "SELECT geopoly_area('[[0,0],[4,3],[4,0],[0,0]]')"),
        "-6.0"
    );
    assert_eq!(
        rows_str(&c, "SELECT hex(geopoly_blob('[[0,0],[1,0],[1,1],[0,0]]'))"),
        "0100000300000000000000000000803F000000000000803F0000803F",
    );
    // contains_point: 2 inside, 1 boundary, 0 outside
    assert_eq!(
        rows_str(
            &c,
            "SELECT geopoly_contains_point('[[0,0],[4,0],[4,4],[0,4],[0,0]]',2,2)"
        ),
        "2"
    );
    assert_eq!(
        rows_str(
            &c,
            "SELECT geopoly_contains_point('[[0,0],[4,0],[4,4],[0,4],[0,0]]',2,0)"
        ),
        "1"
    );
    assert_eq!(
        rows_str(
            &c,
            "SELECT geopoly_contains_point('[[0,0],[4,0],[4,4],[0,4],[0,0]]',9,9)"
        ),
        "0"
    );
    // overlap identical -> 4, within equal -> 2
    let a = "[[0,0],[3,0],[3,3],[0,3],[0,0]]";
    assert_eq!(
        rows_str(&c, &format!("SELECT geopoly_overlap('{a}','{a}')")),
        "4"
    );
    assert_eq!(
        rows_str(&c, &format!("SELECT geopoly_within('{a}','{a}')")),
        "2"
    );
    // invalid input -> NULL
    assert_eq!(rows_str(&c, "SELECT geopoly_area('bad')"), "");
}
