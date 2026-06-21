//! Differential sweep of BLOB handling, `quote()`, `hex`/`unhex`, the RTRIM
//! collation, and text/blob edge behaviour against the `sqlite3` CLI (the
//! ground-truth oracle). Blobs are pinned through `quote(...)`/`hex(...)` and
//! storage classes through `typeof(...)` so the comparison is exact.
//!
//! Hard-coded assertions cover the specific divergences this corpus fixed:
//!
//!   * SQLite's string functions (`trim`/`upper`/`lower`/`replace`/`substr`/
//!     `soundex`/`unhex`) coerce a BLOB argument as a NUL-terminated C string,
//!     so an embedded `NUL` byte truncates the coerced text — `trim(X'00…')`
//!     is `''`, not `'   A   '`. (A genuine TEXT value keeps its embedded NULs:
//!     this engine's TEXT model is NUL-preserving — see `tests/json5.rs` — so
//!     the divergence is confined to the blob path.)
//!   * `unhex(X, Y)` accepts the ignore characters of `Y` only at byte
//!     boundaries, never inside a hex pair (`unhex('A BCD',' ')` is `NULL`).
//!   * `char()` substitutes U+FFFD for an out-of-range code point (negative or
//!     `> U+10FFFF`) instead of dropping the character.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

/// Render a graphite value the way the `sqlite3` CLI prints it in list mode.
/// Blobs are always wrapped in `quote(...)`/`hex(...)` in the probes, so a raw
/// blob never reaches here.
fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => graphitesql::exec::eval::format_real(*r),
        Value::Text(t) => t.clone(),
        Value::Blob(b) => format!("BLOB{b:?}"),
    }
}

/// Run `SELECT <expr>` in both engines; return `(graphite, sqlite)`. A failure
/// on either side collapses to `"<ERR>"`.
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

/// Expressions that must agree byte-for-byte with the sqlite3 CLI. Every blob
/// result is pinned through `quote(...)` or `hex(...)`.
const EXPRS: &[&str] = &[
    // --- quote() of every storage class -------------------------------------
    "quote(NULL)",
    "quote(0)",
    "quote(-9223372036854775808)",
    "quote(123456789012345)",
    "quote(0.0)",
    "quote(-0.0)",
    "quote(1.0)",
    "quote(3.14)",
    "quote(0.1)",
    "quote(1e16)",
    "quote(1e-7)",
    "quote(1e300)",
    "quote(9e999)",  // +Inf -> 9.0e+999
    "quote(-9e999)", // -Inf -> -9.0e+999
    "quote('a''b')",
    "quote('line1' || char(10) || 'line2')",
    "quote(X'')",
    "quote(X'00FF')",
    "quote(X'27')", // a byte that is a quote in ASCII stays hex in a blob literal
    "quote('plain')",
    // --- hex() --------------------------------------------------------------
    "hex('abc')",
    "hex(X'00ff')",
    "hex(255)",
    "hex(-5)",
    "hex(3.5)",
    "hex(-1.5)",
    "hex(NULL)",
    "hex(zeroblob(3))",
    // --- unhex() ------------------------------------------------------------
    "quote(unhex('414243'))",
    "quote(unhex('aAbB'))",
    "quote(unhex('41424'))", // odd length -> NULL
    "quote(unhex('zz'))",    // illegal chars -> NULL
    "quote(unhex(''))",      // empty -> X''
    "quote(unhex(NULL))",
    "quote(unhex('41 42 43', ' '))", // 2-arg ignore set
    "quote(unhex('41', ''))",        // empty ignore set
    "quote(unhex('xyz', 'xyz'))",    // ignore everything -> X''
    "quote(unhex('4 1', ' '))",      // odd after stripping -> NULL
    // --- char() / unicode() -------------------------------------------------
    "hex(char(65, 0, 66))",
    "hex(char(0))",
    "hex(char(0x1F600))", // astral
    "hex(char(65, -1, 66))",
    "hex(char(65, 0x110000, 66))",
    "hex(char(-1))",
    "hex(char(0x110000))",
    "unicode('abc')",
    "unicode('é')",
    "typeof(unicode(''))", // empty -> NULL
    "unicode(X'41')",
    // --- length vs octet_length ---------------------------------------------
    "length(X'00ff00')",
    "octet_length(X'00ff00')",
    "length('héllo')",
    "octet_length('héllo')",
    "length(X'410042')", // blob: byte count, keeps NUL -> 3
    "octet_length(X'410042')",
    "length(12345)",
    "octet_length(1.5)",
    // --- instr (blob & multibyte) -------------------------------------------
    "instr(X'0102030405', X'0304')",
    "instr('héllo', 'llo')",
    "instr(X'410042', 'B')",
    // --- substr (blob byte-indexed; text char-indexed) ----------------------
    "quote(substr(X'0102030405', 2, 2))",
    "quote(substr(X'0041004200', 1, 5))", // blob keeps embedded NUL bytes
    "substr('héllo', 2, 2)",
    // --- replace / trim of a BLOB (NUL-terminated C-string coercion) --------
    "typeof(replace(X'0102', X'01', X'09'))", // always returns text
    "quote(replace(X'00410042', X'41', X'58'))",
    "quote(trim(X'00200041002000'))",
    "quote(trim(X'410042'))",
    "quote(upper(X'610062'))",
    "quote(lower(X'410042'))",
    // --- zeroblob -----------------------------------------------------------
    "quote(zeroblob(0))",
    "length(zeroblob(5))",
    "quote(zeroblob(-5))", // negative clamps to empty
    "length(zeroblob(-5))",
    "typeof(zeroblob(0))",
    // --- RTRIM collation (expression-level COLLATE operator) ----------------
    "'abc' = 'abc   ' COLLATE RTRIM",
    "'abc   ' = 'abc' COLLATE RTRIM",
    "('abc' COLLATE RTRIM) = 'abc '",
    "'abc ' < 'abc' COLLATE RTRIM",
    "'abc' < 'abd' COLLATE RTRIM",
    // --- blob comparison & total ordering -----------------------------------
    "X'01' < X'02'",
    "X'0102' < X'02'",
    "X'' < X'00'",
    "X'01' = X'01'",
    "X'02' < X'0102'",
    "CASE WHEN X'01' < 'a' THEN 'blob<text' ELSE 'no' END",
    "CASE WHEN 5 < X'01' THEN 'num<blob' ELSE 'no' END",
    "CASE WHEN NULL < X'00' THEN 'y' ELSE 'no' END",
];

#[test]
fn blob_quote_surface_matches_sqlite3() {
    if !sqlite3_available() {
        eprintln!("sqlite3 not available; skipping differential sweep");
        return;
    }
    let c = Connection::open_memory().unwrap();
    let mut mismatches = Vec::new();
    for expr in EXPRS {
        let (g, s) = both(&c, expr);
        if g != s {
            mismatches.push(format!(
                "  {expr}\n    graphite = {g:?}\n    sqlite3  = {s:?}"
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "differential divergences:\n{}",
        mismatches.join("\n")
    );
}

/// Total-ordering sort: `quote(...)` each value so blobs and the NULL/number/
/// text/blob storage-class ordering are pinned exactly.
#[test]
fn blob_total_ordering_matches_sqlite3() {
    if !sqlite3_available() {
        return;
    }
    let c = Connection::open_memory().unwrap();
    let q = "SELECT group_concat(quote(x)) FROM \
        (SELECT 1 AS x UNION ALL SELECT 2.5 UNION ALL SELECT 'a' \
         UNION ALL SELECT X'00' UNION ALL SELECT X'0102' UNION ALL SELECT NULL ORDER BY x)";
    let g = render(&c.query(q).unwrap().rows[0][0]);
    let o = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("{q};"))
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout).trim_end().to_string();
    assert_eq!(g, s, "blob/number/text/null total ordering");
    // Pin the observed sqlite oracle value too, so a regression is obvious even
    // if the CLI is missing.
    assert_eq!(g, "NULL,1,2.5,'a',X'00',X'0102'");
}

/// Hard-pinned assertions for the fixed divergences (independent of the CLI).
#[test]
fn fixed_divergences_pinned() {
    let c = Connection::open_memory().unwrap();
    let q = |sql: &str| render(&c.query(sql).unwrap().rows[0][0]);

    // A BLOB argument to the string functions is coerced as a NUL-terminated C
    // string, so an embedded NUL byte truncates it (matching SQLite). A genuine
    // TEXT value, in contrast, keeps its embedded NULs throughout this engine
    // (see `tests/json5.rs`), so the divergence is confined to the blob path.
    assert_eq!(q("SELECT quote(trim(X'00200041002000'))"), "''");
    assert_eq!(q("SELECT quote(replace(X'00410042', X'41', X'58'))"), "''");
    assert_eq!(q("SELECT quote(upper(X'610062'))"), "'A'");
    assert_eq!(q("SELECT quote(lower(X'410042'))"), "'a'");
    // A blob is byte-indexed by substr and keeps every byte (including NULs).
    assert_eq!(
        q("SELECT quote(substr(X'0041004200', 1, 5))"),
        "X'0041004200'"
    );

    // char() substitutes U+FFFD (hex EFBFBD) for out-of-range code points.
    assert_eq!(q("SELECT hex(char(65, -1, 66))"), "41EFBFBD42");
    assert_eq!(q("SELECT hex(char(0x110000))"), "EFBFBD");
    // Valid code points (including NUL and astral) still pass through.
    assert_eq!(q("SELECT hex(char(65, 0, 66))"), "410042");
    assert_eq!(q("SELECT hex(char(0x1F600))"), "F09F9880");

    // quote() of the infinities and negative zero.
    assert_eq!(q("SELECT quote(9e999)"), "9.0e+999");
    assert_eq!(q("SELECT quote(-9e999)"), "-9.0e+999");
    assert_eq!(q("SELECT quote(-0.0)"), "0.0");

    // 2-arg unhex: ignore chars are valid only at byte boundaries, never inside
    // a hex pair (`unhex('AB CD',' ')` -> X'ABCD'; `unhex('A BCD',' ')` -> NULL).
    assert_eq!(q("SELECT quote(unhex('AB CD', ' '))"), "X'ABCD'");
    assert_eq!(q("SELECT quote(unhex(' AB CD ', ' '))"), "X'ABCD'");
    assert_eq!(q("SELECT quote(unhex('A BCD', ' '))"), "NULL");
    assert_eq!(q("SELECT quote(unhex('4-142', '-'))"), "NULL");
    assert_eq!(q("SELECT quote(unhex('41-42-43', '-'))"), "X'414243'");
    // A BLOB hex argument is read as a NUL-terminated C string (the bytes after
    // an embedded NUL are dropped), matching SQLite.
    assert_eq!(q("SELECT quote(unhex(X'3431003432'))"), "X'41'");
}
