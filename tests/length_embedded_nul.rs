//! `length(X)` on a TEXT value counts characters up to (not including) the first
//! NUL, matching sqlite3 3.50.4 — `length('A'||char(0)||'B')` is 1, not 3. BLOB
//! length and `octet_length` are unaffected (they count every byte). The value
//! itself still stores the NUL (hex/comparison see all bytes); only `length`
//! and the C-string-style CLI rendering stop early.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn val(c: &Connection, sql: &str) -> Value {
    c.query(sql).unwrap().rows[0][0].clone()
}

#[test]
fn length_of_text_stops_at_first_nul() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        val(&c, "SELECT length('A'||char(0)||'B')"),
        Value::Integer(1)
    );
    assert_eq!(
        val(&c, "SELECT length(cast(x'410042' as text))"),
        Value::Integer(1)
    );
    assert_eq!(val(&c, "SELECT length(char(65,0,66))"), Value::Integer(1));
    // A leading NUL yields length 0.
    assert_eq!(val(&c, "SELECT length(char(0)||'x')"), Value::Integer(0));
    // No NUL: ordinary character count (incl. multi-byte).
    assert_eq!(val(&c, "SELECT length('hello')"), Value::Integer(5));
    assert_eq!(val(&c, "SELECT length('héllo')"), Value::Integer(5));
    // Numbers stringify without NULs, so they are unaffected.
    assert_eq!(val(&c, "SELECT length(12345)"), Value::Integer(5));
}

#[test]
fn octet_length_and_blob_length_count_every_byte() {
    let c = Connection::open_memory().unwrap();
    // octet_length counts all bytes, including past the NUL.
    assert_eq!(
        val(&c, "SELECT octet_length('A'||char(0)||'B')"),
        Value::Integer(3)
    );
    // A BLOB's length is its full byte count regardless of NULs.
    assert_eq!(val(&c, "SELECT length(x'410042')"), Value::Integer(3));
}

#[test]
fn the_nul_is_still_stored_in_the_value() {
    let c = Connection::open_memory().unwrap();
    // length stops early, but the bytes are all present: hex sees them and an
    // equality against the truncated prefix is false.
    assert_eq!(
        val(&c, "SELECT hex('A'||char(0)||'B')"),
        Value::Text("410042".into())
    );
    assert_eq!(
        val(&c, "SELECT ('A'||char(0)||'B') = 'A'"),
        Value::Integer(0)
    );
}
