//! The `soundex(X)` scalar function, matched to the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};

fn t(c: &Connection, sql: &str) -> String {
    match c.query(sql).unwrap().rows.remove(0).remove(0) {
        Value::Text(s) => String::from(s.as_str()),
        other => panic!("expected text from {sql}, got {other:?}"),
    }
}

#[test]
fn soundex_matches_sqlite() {
    let c = Connection::open_memory().unwrap();
    // Oracle values observed from sqlite3 3.50.4.
    for (input, want) in [
        ("Robert", "R163"),
        ("Rupert", "R163"),
        ("Tymczak", "T522"),
        ("Pfister", "P236"),
        ("Ashcraft", "A226"),
        ("Euler", "E460"),
        ("Gauss", "G200"),
        ("a", "A000"),
        ("H", "H000"),
        ("  abc", "A120"), // leading non-letters skipped
    ] {
        assert_eq!(
            t(&c, &format!("SELECT soundex('{input}')")),
            want,
            "soundex({input})"
        );
    }
    // No letters (incl. NULL and a number) yields "?000" — soundex does not
    // propagate NULL.
    assert_eq!(t(&c, "SELECT soundex('')"), "?000");
    assert_eq!(t(&c, "SELECT soundex('123')"), "?000");
    assert_eq!(t(&c, "SELECT soundex(NULL)"), "?000");
    assert_eq!(t(&c, "SELECT soundex(12)"), "?000");
}
