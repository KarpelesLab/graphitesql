//! Non-UTF-8 text. SQLite's text is a byte string that need not be valid UTF-8:
//! `x'ff' || x'00'` and `CAST(<blob> AS TEXT)` yield a value whose storage class
//! is `text` even though the bytes are not valid UTF-8. graphitesql used to fall
//! back to a blob for those (a `String`-backed `Value::Text` couldn't hold them);
//! now `Value::Text` is byte-backed, so `typeof()` reports `text` like SQLite and
//! the exact bytes round-trip through storage.
//!
//! These are library-level assertions of well-defined SQLite semantics (no
//! `sqlite3` CLI): `||` always yields text, a blob cast to text keeps its bytes,
//! and `hex()` (which reads the raw bytes) is unchanged.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn one(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows.into_iter().next().unwrap()[0].clone()
}
fn text(c: &Connection, sql: &str) -> String {
    match one(c, sql) {
        Value::Text(t) => String::from(t.as_str()),
        v => panic!("expected text, got {v:?}"),
    }
}

#[test]
fn concat_of_non_utf8_is_text() {
    let c = Connection::open_memory().unwrap();
    // `||` always produces TEXT, even when the bytes are not valid UTF-8.
    assert_eq!(text(&c, "SELECT typeof(x'ff' || x'00')"), "text");
    assert_eq!(text(&c, "SELECT typeof('a' || x'ff' || 'b')"), "text");
    // A valid-UTF-8 concatenation is still text (unchanged).
    assert_eq!(text(&c, "SELECT typeof('a' || 'b')"), "text");
    // `hex()` reads the raw bytes — identical whether the class is text or blob.
    assert_eq!(text(&c, "SELECT hex(x'ff' || x'00')"), "FF00");
    assert_eq!(text(&c, "SELECT hex('a' || x'ff' || 'b')"), "61FF62");
}

#[test]
fn cast_blob_to_text_keeps_bytes() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(text(&c, "SELECT typeof(CAST(x'ff' AS TEXT))"), "text");
    assert_eq!(text(&c, "SELECT hex(CAST(x'ff' AS TEXT))"), "FF");
    // A valid-UTF-8 blob cast to text is the same text as before.
    assert_eq!(text(&c, "SELECT CAST(x'6162' AS TEXT)"), "ab");
}

#[test]
fn non_utf8_text_round_trips_through_storage() {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.execute("INSERT INTO t VALUES(x'ff' || x'00' || x'fe')")
        .unwrap();
    // Stored as text, read back as text with the exact bytes intact.
    assert_eq!(text(&c, "SELECT typeof(a) FROM t"), "text");
    assert_eq!(text(&c, "SELECT hex(a) FROM t"), "FF00FE");
    // A valid-UTF-8 text still round-trips normally.
    c.execute("INSERT INTO t VALUES('héllo')").unwrap();
    assert_eq!(
        text(
            &c,
            "SELECT a FROM t WHERE typeof(a)='text' AND hex(a)<>'FF00FE'"
        ),
        "héllo"
    );
}
