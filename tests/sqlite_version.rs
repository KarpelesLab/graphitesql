//! `sqlite_version()` reports the SQLite release graphitesql tracks (and writes
//! into new file headers).

#![cfg(feature = "std")]

use graphitesql::{Connection, TARGET_SQLITE_VERSION, Value};

#[test]
fn sqlite_version_matches_target() {
    let c = Connection::open_memory().unwrap();
    assert_eq!(
        c.query("SELECT sqlite_version()").unwrap().rows[0][0],
        Value::Text(TARGET_SQLITE_VERSION.into())
    );
    // Usable in expressions, and a `major.minor.patch` shape.
    let v = match &c.query("SELECT sqlite_version()").unwrap().rows[0][0] {
        Value::Text(s) => String::from(s.as_str()),
        _ => panic!(),
    };
    assert_eq!(v.split('.').count(), 3);
    assert!(v.split('.').all(|p| p.parse::<u32>().is_ok()));
    // Wrong arity is an error.
    assert!(c.query("SELECT sqlite_version(1)").is_err());
}
