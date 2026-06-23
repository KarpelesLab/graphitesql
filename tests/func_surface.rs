//! Differential sweep of the scalar-function surface against the `sqlite3` CLI
//! (the ground-truth oracle), plus hard-coded assertions for the specific
//! divergences this corpus fixed:
//!
//!   * `NULLIF` now honours an explicit `COLLATE` on either operand.
//!   * `substr`/`substring` is a faithful port of SQLite's `substrFunc`,
//!     including saturating arithmetic for pathological i64 indices.
//!   * `round(x, NULL)` is NULL; a negative-zero result normalises to `0.0`.
//!   * `abs(-0.0)` is `0.0`, not `-0.0`.
//!   * `coalesce`/`concat`/`concat_ws`/`min`/`max` argument-count errors match.
//!   * the `like(...)` function form rejects a non-single-character ESCAPE.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Render a graphite value the way the `sqlite3` CLI prints it in its default
/// list mode: NULL is empty, integers/reals/text verbatim. (Blob comparisons
/// are done by wrapping the expression in `hex(...)`, so blobs never reach
/// here.)
fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Text(t) => t.clone(),
        Value::Blob(b) => format!("BLOB{b:?}"),
    }
}

/// Run `SELECT <expr>` in both engines; return `(graphite, sqlite)` rendered as
/// the CLI would. Errors collapse to the sentinel `"<ERR>"` on both sides so a
/// shared "this is an error" expectation can be asserted without matching exact
/// wording.
fn both(c: &Connection, expr: &str) -> (String, String) {
    let q = format!("SELECT {expr}");
    let g = match c.query(&q) {
        Ok(rs) => render(&rs.rows[0][0]),
        Err(_) => "<ERR>".to_string(),
    };
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{q};"))
        .output()
        .unwrap();
    let s = if o.status.success() && o.stderr.is_empty() {
        String::from_utf8_lossy(&o.stdout).trim_end().to_string()
    } else {
        "<ERR>".to_string()
    };
    (g, s)
}

/// Expressions that must agree byte-for-byte with the sqlite3 CLI. Blob results
/// are wrapped in `hex(...)` so both engines emit comparable text.
const CORPUS: &[&str] = &[
    // ---- NULLIF collation -------------------------------------------------
    "NULLIF('a','A' COLLATE NOCASE)",
    "NULLIF('a' COLLATE NOCASE,'A')",
    "NULLIF('a','A')",
    "NULLIF('a','a')",
    "NULLIF('A','a' COLLATE NOCASE)",
    "NULLIF(1,1.0)",
    "NULLIF(1,'1')",
    "NULLIF('1',1)",
    "NULLIF(0.0,0)",
    "NULLIF('','')",
    "typeof(NULLIF(1,1))",
    // ---- substr / substring ----------------------------------------------
    "substr('hello',0)",
    "substr('hello',-2)",
    "substr('hello',-2,1)",
    "substr('hello',-10,3)",
    "substr('hello',2,-1)",
    "substr('hello',2,-3)",
    "substr('hello',0,2)",
    "substr('hello',-1,-1)",
    "substr('hello',10)",
    "substr('hello',2,100)",
    "substr('hello',2)",
    "substring('hello',2,2)",
    "substr('héllo',2,2)",
    "substr('hello',-9223372036854775808,3)",
    "substr('abcde',9223372036854775807,5)",
    "substr('abcde',9223372036854775807,-5)",
    "substr('abcde',-9223372036854775808,9223372036854775807)",
    "substr('abcde',1,9223372036854775807)",
    "substr('abcde',3,-9223372036854775808)",
    "substr('hello',1,0)",
    "substr('hello',-3,2)",
    "substr('hello',5,-2)",
    "substr('hello',6,-2)",
    "substr('',1,3)",
    "substr('hello',2,NULL)",
    "hex(substr(x'0102030405',2,2))",
    "hex(substr(x'0102030405',-2,2))",
    "hex(substr(x'0102030405',0,3))",
    "hex(substr(x'0102030405',2))",
    // ---- replace / instr / trim ------------------------------------------
    "replace('aaa','','X')",
    "replace('abcabc','bc','XY')",
    "replace('hello','l','')",
    "replace('aaa','a','aa')",
    "instr('hello','l')",
    "instr('hello','z')",
    "instr('hello','')",
    "instr('','')",
    "instr('café','é')",
    "instr('abc',2)",
    "instr(12345,34)",
    "trim('  hi  ')",
    "trim('xxhixx','x')",
    "ltrim('xxhixx','x')",
    "rtrim('xxhixx','x')",
    "trim('abchicba','abc')",
    "trim('hello','xyz')",
    "trim('xyzzy','')",
    "trim('ééhellóó','éó')",
    // ---- hex / unhex ------------------------------------------------------
    "hex('abc')",
    "hex(x'0102ff')",
    "hex(255)",
    "hex(unhex('414243'))",
    "hex(unhex('deadBEEF'))",
    "hex(unhex(''))",
    "hex(unhex('48 49',' '))",
    "unhex('xyz')",
    "unhex('4')",
    "unhex('abc')",
    "unhex('0x41')",
    // ---- char / unicode ---------------------------------------------------
    "char(72,105)",
    "char(65)",
    "hex(char(65,0,66))",
    "unicode('A')",
    "unicode('')",
    "unicode('😀')",
    "char(0x1F600)",
    // ---- round ------------------------------------------------------------
    "round(2.5)",
    "round(3.5)",
    "round(-2.5)",
    "round(-0.0)",
    "round(-0.4)",
    "round(-0.04,1)",
    "round(2.567,2)",
    "round(1234.5678,-2)",
    "round(2.675,2)",
    "round(2.5,NULL)",
    "round(NULL,2)",
    "typeof(round(-0.4))",
    // ---- quote ------------------------------------------------------------
    "quote('it''s')",
    "quote(123)",
    "quote(1.5)",
    "quote(NULL)",
    "quote('')",
    "quote(x'00ff')",
    // ---- abs / sign / typeof ---------------------------------------------
    "abs(-5)",
    "abs(-5.5)",
    "abs('abc')",
    "abs(-0.0)",
    "abs(0.0)",
    "sign(-3)",
    "sign(0)",
    "sign(3.5)",
    "sign('abc')",
    "sign(-0.0)",
    "typeof(1)",
    "typeof(1.5)",
    "typeof('x')",
    "typeof(x'00')",
    "typeof(NULL)",
    // ---- iif / coalesce / ifnull / min / max -----------------------------
    "coalesce(NULL,NULL,3)",
    "ifnull(NULL,5)",
    "ifnull('',5)",
    "iif(1,'y','n')",
    "iif(0,'y')",
    "iif(NULL,'y','n')",
    "min(3,1,2)",
    "max(3,1,2)",
    "min(1,NULL,2)",
    "max(1,NULL,2)",
    "min('b','a','c')",
    "max('2',10)",
    "min(1.0,1)",
    "min(1,1.0)",
    "max(1.0,1)",
    "max(1,1.0)",
    "min(2,1.0,1)",
    "max(1,2.0,2)",
    "typeof(min(1.0,1))",
    "typeof(max(1,1.0))",
    // ---- length / octet_length / zeroblob --------------------------------
    "length('héllo')",
    "length(x'010203')",
    "length(12345)",
    "length('')",
    "octet_length('héllo')",
    "octet_length(x'010203')",
    "hex(zeroblob(3))",
    "length(zeroblob(5))",
    // ---- concat / concat_ws ----------------------------------------------
    "concat('a','b','c')",
    "concat('a',NULL,'c')",
    "concat(1,2,3)",
    "concat('a')",
    "concat_ws('-','a','b','c')",
    "concat_ws('-','a',NULL,'c')",
    "concat_ws(NULL,'a','b')",
    "concat_ws('-','x')",
    // ---- like / glob (ASCII) ---------------------------------------------
    "'abc' LIKE 'a_c'",
    "'a%c' LIKE 'a\\%c' ESCAPE '\\'",
    "'a_c' LIKE 'a\\_c' ESCAPE '\\'",
    "'100%' LIKE '100\\%' ESCAPE '\\'",
    "like('a%c','abc')",
    "like('a\\%c','a%c','\\')",
    "glob('a*','abc')",
    "'abc' GLOB 'a[b]c'",
    "'a*c' GLOB 'a[*]c'",
    "'abc' GLOB 'a[!x-z]c'",
    // ---- argument-count / escape errors (both sides error) ---------------
    "coalesce(NULL)",
    "concat()",
    "concat_ws('-')",
    "min()",
    "max()",
    "nullif(1)",
    "round()",
    "like('a%c','abc','')",
    "like('a%c','abc','xy')",
    "like('a%c','abc',NULL)",
];

#[test]
fn scalar_surface_matches_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let c = Connection::open_memory().unwrap();
    let mut diverged = Vec::new();
    for &e in CORPUS {
        let (g, s) = both(&c, e);
        if g != s {
            diverged.push(format!("{e}\n  graphite: [{g}]\n  sqlite:   [{s}]"));
        }
    }
    assert!(
        diverged.is_empty(),
        "scalar function divergences:\n{}",
        diverged.join("\n")
    );
}

/// Hard-coded oracle values for the specific bugs fixed, so the regression is
/// pinned even when the `sqlite3` CLI is unavailable. Each value was observed
/// from sqlite3 3.50.4.
#[test]
fn fixed_divergences_pinned() {
    let c = Connection::open_memory().unwrap();
    let null = |e: &str| {
        matches!(
            c.query(&format!("SELECT {e}")).unwrap().rows[0][0],
            Value::Null
        )
    };
    let text = |e: &str| match &c.query(&format!("SELECT {e}")).unwrap().rows[0][0] {
        Value::Text(t) => t.clone(),
        other => format!("{other:?}"),
    };
    let real = |e: &str| match c.query(&format!("SELECT {e}")).unwrap().rows[0][0] {
        Value::Real(r) => r,
        ref other => panic!("expected real, got {other:?}"),
    };

    // NULLIF honours an explicit COLLATE.
    assert!(null("NULLIF('a','A' COLLATE NOCASE)"));
    assert!(null("NULLIF('a' COLLATE NOCASE,'A')"));
    assert_eq!(text("NULLIF('a','A')"), "a");

    // round(x, NULL) -> NULL; -0.0 result normalised to 0.0.
    assert!(null("round(2.5,NULL)"));
    assert_eq!(real("round(-0.4)").to_bits(), 0.0f64.to_bits());
    assert_eq!(real("round(-0.0)").to_bits(), 0.0f64.to_bits());

    // abs(-0.0) -> 0.0 (positive zero).
    assert_eq!(real("abs(-0.0)").to_bits(), 0.0f64.to_bits());

    // substr saturating arithmetic (no panic) on i64 extremes.
    assert_eq!(text("substr('abcde',1,9223372036854775807)"), "abcde");
    assert_eq!(
        text("substr('abcde',-9223372036854775808,9223372036854775807)"),
        "abcd"
    );

    // Argument-count / escape errors.
    for bad in [
        "coalesce(NULL)",
        "concat()",
        "concat_ws('-')",
        "min()",
        "max()",
        "like('a%c','abc','')",
        "like('a%c','abc','xy')",
    ] {
        assert!(
            c.query(&format!("SELECT {bad}")).is_err(),
            "expected error for {bad}"
        );
    }
}

#[test]
fn single_arg_functions_error_on_zero_args_without_panicking() {
    // A zero-argument call to a one-argument scalar function must produce a clean
    // error, never a panic (these used to index `v[0]` on an empty vec). Verified
    // for every such builtin; sqlite likewise rejects them ("wrong number of
    // arguments"), so the differential corpus stays consistent.
    let c = Connection::open_memory().unwrap();
    let fns = [
        "typeof",
        "hex",
        "unicode",
        "abs",
        "length",
        "lower",
        "upper",
        "round",
        "sign",
        "ceil",
        "floor",
        "sqrt",
        "exp",
        "ln",
        "trim",
        "ltrim",
        "rtrim",
        "quote",
        "soundex",
        "json",
        "json_valid",
        "json_type",
        "json_quote",
        "unhex",
        "zeroblob",
        "randomblob",
        "likely",
        "unlikely",
        "octet_length",
        "json_array_length",
        "sin",
        "cos",
        "tan",
        "degrees",
        "radians",
        "unistr",
        "subtype",
    ];
    for f in fns {
        let q = format!("SELECT {f}()");
        // Must be an Err (not a panic, not Ok).
        assert!(
            c.query(&q).is_err(),
            "{f}() should error on zero args, not succeed"
        );
    }
    // The valid one-argument forms still work.
    assert_eq!(
        c.query("SELECT typeof(1), hex('AB'), unicode('Z')")
            .unwrap()
            .rows[0],
        vec![
            Value::Text("integer".into()),
            Value::Text("4142".into()),
            Value::Integer(90)
        ]
    );
}
