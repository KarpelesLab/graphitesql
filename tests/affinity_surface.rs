//! Differential sweep of type-affinity / CAST / coercion corners that the
//! result-only corpus under-covers: pre-comparison affinity for `IN` and
//! `BETWEEN` (the left operand's affinity is pushed onto bare literal
//! operands), rowid-alias INTEGER affinity, storage affinity, and CAST edges.
//!
//! Every expectation below was captured from the pinned sqlite3 3.50.4 oracle;
//! the test also re-derives each value from the live `sqlite3` CLI (when present)
//! so a future SQLite change is caught too.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn have_sqlite3() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Run a one-shot script in the sqlite3 CLI and return its trimmed stdout.
fn sqlite3(setup: &str, query: &str) -> String {
    let sql = format!("{setup} {query};");
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim_end().to_string()
}

/// Run the same script through graphitesql and render its single column like the
/// sqlite3 CLI's default "list" mode (one row per line, NULL as empty).
fn graphite(setup: &str, query: &str) -> String {
    let mut c = Connection::open_memory().unwrap();
    for stmt in setup.split(';') {
        if !stmt.trim().is_empty() {
            c.execute(stmt).unwrap();
        }
    }
    let r = c.query(query).unwrap();
    r.rows
        .iter()
        .map(|row| row.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Text(s) => String::from(s.as_str()),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
    }
}

/// Assert graphite == a hard-coded sqlite value, and (when the CLI is present)
/// that the live oracle still agrees with that pinned value.
fn check(setup: &str, query: &str, expect: &str) {
    let got = graphite(setup, query);
    assert_eq!(got, expect, "graphite diverged for `{query}`");
    if have_sqlite3() {
        let oracle = sqlite3(setup, query);
        assert_eq!(
            oracle, expect,
            "pinned value stale vs live sqlite3 for `{query}`"
        );
    }
}

/// Pre-comparison affinity for `IN`: the left operand's NUMERIC/INTEGER/REAL
/// affinity is applied to bare text list elements (and TEXT affinity is applied
/// the other way), exactly as for `=`. A typeless (BLOB/NONE) column does not
/// coerce a text literal.
#[test]
fn in_list_applies_comparison_affinity() {
    let s = "CREATE TABLE t(i INTEGER, txt TEXT, n NUMERIC, b BLOB, none_col);\
             INSERT INTO t VALUES(10,'10',10,10,10);";
    check(s, "SELECT i IN ('10','20') FROM t", "1");
    check(s, "SELECT i IN ('10.0') FROM t", "1");
    check(s, "SELECT n IN ('10') FROM t", "1");
    check(s, "SELECT txt IN (10,20) FROM t", "1");
    check(s, "SELECT txt IN ('10') FROM t", "1");
    // A typeless column does NOT text-coerce, so '10' (text) != 10 (int): false.
    check(s, "SELECT none_col IN ('10') FROM t", "0");
    // A real BLOB literal never numerically matches a text/number list element.
    check(s, "SELECT b IN ('10') FROM t", "0");
    check(s, "SELECT i NOT IN ('5','15') FROM t", "1");
    // Per-element COLLATE on a single-element list still applies; on a
    // multi-element list it is ignored (left operand's collation wins).
    check("", "SELECT 'a' IN ('A' COLLATE NOCASE)", "1");
    check("", "SELECT 'a' IN ('x','A' COLLATE NOCASE)", "0");
}

/// Pre-comparison affinity for `BETWEEN`: each of the two implied comparisons
/// (`x >= lo`, `x <= hi`) pushes the left operand's affinity onto a bare bound.
#[test]
fn between_applies_comparison_affinity() {
    let s = "CREATE TABLE t(i INTEGER, txt TEXT);\
             INSERT INTO t VALUES(10,'10');";
    check(s, "SELECT i BETWEEN '5' AND '15' FROM t", "1");
    check(s, "SELECT i BETWEEN '9' AND '100' FROM t", "1");
    check(s, "SELECT i BETWEEN 9.5 AND 10.5 FROM t", "1");
    // TEXT column vs numeric bounds: the literals are text-coerced, so the
    // textual '10' is NOT between '5' and '15' as strings ('1' < '5').
    check(s, "SELECT txt BETWEEN 5 AND 15 FROM t", "0");
    check(s, "SELECT txt BETWEEN 9 AND 100 FROM t", "0");
}

/// A bare rowid alias has INTEGER affinity for comparison, so `rowid`/`oid`/
/// `_rowid_` and an `INTEGER PRIMARY KEY` numerically coerce text operands in
/// `=`, `<`, `IN`, and `BETWEEN` — including on the index-driven seek path,
/// whose superset is re-filtered through the same affinity-aware comparison.
#[test]
fn rowid_alias_has_integer_affinity() {
    let s = "CREATE TABLE r(a); INSERT INTO r VALUES(1),(2),(3);";
    check(s, "SELECT a FROM r WHERE rowid = '2'", "2");
    check(s, "SELECT a FROM r WHERE rowid < '3' ORDER BY a", "1\n2");
    check(
        s,
        "SELECT a FROM r WHERE rowid IN ('1','3') ORDER BY a",
        "1\n3",
    );
    check(
        s,
        "SELECT a FROM r WHERE rowid BETWEEN '2' AND '3' ORDER BY a",
        "2\n3",
    );
    check(
        s,
        "SELECT rowid, rowid IN ('1','3') FROM r",
        "1|1\n2|0\n3|1",
    );

    let p = "CREATE TABLE p(id INTEGER PRIMARY KEY, v);\
             INSERT INTO p VALUES(1,'a'),(2,'b'),(3,'c');";
    check(
        p,
        "SELECT v FROM p WHERE id IN ('1','3') ORDER BY id",
        "a\nc",
    );
    check(
        p,
        "SELECT v FROM p WHERE id BETWEEN '2' AND '3' ORDER BY id",
        "b\nc",
    );
    check(p, "SELECT v FROM p WHERE id = '2'", "b");
}

/// Column-vs-literal comparison affinity in both SELECT and WHERE context
/// (already correct; pinned here as a regression guard).
#[test]
fn comparison_affinity_column_vs_literal() {
    let s = "CREATE TABLE t(i INTEGER, txt TEXT, n NUMERIC, r REAL, b BLOB, none_col);\
             INSERT INTO t VALUES(10,'10',10,10.0,x'3130',10);";
    check(s, "SELECT i < '9' FROM t", "0");
    check(s, "SELECT txt < 9 FROM t", "1");
    check(s, "SELECT n = '10' FROM t", "1");
    check(s, "SELECT r > '5' FROM t", "1");
    check(s, "SELECT none_col = '10' FROM t", "0");
    check(s, "SELECT none_col = 10 FROM t", "1");
    check(s, "SELECT b = '10' FROM t", "0");
    check(s, "SELECT b = x'3130' FROM t", "1");
    // Bare literal pairs (no affinity on either side): no coercion.
    check("", "SELECT '10' < 9", "0");
    check("", "SELECT 10 < '9'", "1");
    check("", "SELECT '10' = 10", "0");
}

/// Storage affinity: on INSERT the value is coerced to the column's affinity;
/// `typeof()`/`quote()` pin the resulting storage class exactly. (Already
/// correct; pinned as a regression guard.)
#[test]
fn storage_affinity_on_insert() {
    let s = "CREATE TABLE t(i INTEGER, n NUMERIC, r REAL, b BLOB, x);";
    let cols = "typeof(i)||':'||quote(i), typeof(n)||':'||quote(n), \
                typeof(r)||':'||quote(r), typeof(b)||':'||quote(b), \
                typeof(x)||':'||quote(x)";
    // '123' -> INTEGER stores 123 (int), NUMERIC 123 (int), REAL 123.0, BLOB/none keep text.
    check(
        &format!("{s} INSERT INTO t VALUES('123','123','123','123','123');"),
        &format!("SELECT {cols} FROM t"),
        "integer:123|integer:123|real:123.0|text:'123'|text:'123'",
    );
    // '2e2' fully-numeric text reduces to 200 under INTEGER/NUMERIC, 200.0 under REAL.
    check(
        &format!("{s} INSERT INTO t VALUES('2e2','2e2','2e2','2e2','2e2');"),
        &format!("SELECT {cols} FROM t"),
        "integer:200|integer:200|real:200.0|text:'2e2'|text:'2e2'",
    );
    // '12abc' is not fully numeric: kept as text under every affinity.
    check(
        &format!("{s} INSERT INTO t VALUES('12abc','12abc','12abc','12abc','12abc');"),
        &format!("SELECT {cols} FROM t"),
        "text:'12abc'|text:'12abc'|text:'12abc'|text:'12abc'|text:'12abc'",
    );
    // A 12.0 real reduces to int under INTEGER/NUMERIC, stays real under REAL/BLOB/none.
    check(
        &format!("{s} INSERT INTO t VALUES(12.0,12.0,12.0,12.0,12.0);"),
        &format!("SELECT {cols} FROM t"),
        "integer:12|integer:12|real:12.0|real:12.0|real:12.0",
    );
}

/// CAST edges: text->INTEGER prefix/overflow saturation, real->INTEGER
/// truncation+saturation, NUMERIC integer-reduction, blob reinterpretation, and
/// CAST of NULL. (Already correct; pinned as a regression guard, and a guard
/// against i64-extreme panics.)
#[test]
fn cast_edges() {
    check("", "SELECT CAST('  42  ' AS INTEGER)", "42");
    check("", "SELECT CAST('0x10' AS INTEGER)", "0");
    check("", "SELECT CAST('2e2' AS INTEGER)", "2");
    check(
        "",
        "SELECT CAST('99999999999999999999' AS INTEGER)",
        "9223372036854775807",
    );
    check(
        "",
        "SELECT CAST('-99999999999999999999' AS INTEGER)",
        "-9223372036854775808",
    );
    // real->int truncates toward zero and saturates out of range (no panic).
    check("", "SELECT CAST(3.9 AS INTEGER)", "3");
    check("", "SELECT CAST(-3.9 AS INTEGER)", "-3");
    check("", "SELECT CAST(1e300 AS INTEGER)", "9223372036854775807");
    check("", "SELECT CAST(-1e300 AS INTEGER)", "-9223372036854775808");
    check(
        "",
        "SELECT CAST(9223372036854775807.0 AS INTEGER)",
        "9223372036854775807",
    );
    // NUMERIC reduces integral-real *text* to int, but keeps a real value as real.
    check("", "SELECT typeof(CAST('3.0' AS NUMERIC))", "integer");
    check("", "SELECT typeof(CAST(3.0 AS NUMERIC))", "real");
    check("", "SELECT CAST('abc' AS NUMERIC)", "0");
    // blob reinterpreted as text then parsed.
    check("", "SELECT CAST(x'3132' AS INTEGER)", "12");
    check("", "SELECT quote(CAST(65 AS BLOB))", "X'3635'");
    // CAST of NULL is NULL of the target's storage class -> typeof null.
    check("", "SELECT typeof(CAST(NULL AS INTEGER))", "null");
    check("", "SELECT quote(CAST(NULL AS BLOB))", "NULL");
}

/// Arithmetic / unary coercion and i64-overflow promotion to real, plus
/// division/modulo by zero (NULL) — guards against panics at the i64 extremes.
#[test]
fn arithmetic_coercion_and_overflow() {
    check("", "SELECT '10abc'+5", "15");
    check("", "SELECT '  7  '+1", "8");
    check("", "SELECT 'abc'+1", "1");
    check("", "SELECT -'5abc'", "-5");
    check("", "SELECT typeof(9223372036854775807+1)", "real");
    check("", "SELECT typeof(-9223372036854775808 * -1)", "real");
    check("", "SELECT 10/0", "");
    check("", "SELECT 10%0", "");
    check("", "SELECT 7.5%2", "1.0");
    check("", "SELECT 9223372036854775807 % -1", "0");
    check(
        "",
        "SELECT 9223372036854775807 / -1",
        "-9223372036854775807",
    );
}
