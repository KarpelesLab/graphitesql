//! Calling a built-in scalar function with the wrong number of arguments reports
//! SQLite's *universal* message body, `wrong number of arguments to function
//! NAME()` — with no "(want N, got M)" suffix and no per-function custom wording
//! ("takes 2 or 3 arguments"). Verified against sqlite3 3.50.4, whose every
//! built-in uses exactly this phrasing.

#![cfg(feature = "std")]

use graphitesql::Connection;

fn msg(c: &Connection, sql: &str) -> String {
    c.query(sql)
        .unwrap_err()
        .to_string()
        .trim_start_matches("error: ")
        .to_string()
}

#[test]
fn scalar_arity_message_matches_sqlite() {
    let c = Connection::open_memory().unwrap();
    // (call, function name reported). Covers the formerly-divergent sites: the
    // "(want N, got M)" suffix group (abs/length/coalesce/zeroblob/trim/concat…)
    // and the custom-text group (substr/round/instr/replace/like/unhex/json_*).
    for (sql, name) in [
        ("SELECT abs(1,2)", "abs"),
        ("SELECT length()", "length"),
        ("SELECT coalesce(1)", "coalesce"),
        ("SELECT zeroblob()", "zeroblob"),
        ("SELECT trim('a','b','c')", "trim"),
        ("SELECT ltrim('a','b','c')", "ltrim"),
        ("SELECT concat()", "concat"),
        ("SELECT concat_ws('x')", "concat_ws"),
        ("SELECT iif(1)", "iif"),
        ("SELECT substr('a')", "substr"),
        ("SELECT round(1,2,3)", "round"),
        ("SELECT instr('a')", "instr"),
        ("SELECT replace('a','b')", "replace"),
        ("SELECT like('a')", "like"),
        ("SELECT unhex('a','b','c')", "unhex"),
        ("SELECT json_pretty()", "json_pretty"),
        ("SELECT json_type('a','b','c')", "json_type"),
        ("SELECT json_array_length('a','b','c')", "json_array_length"),
    ] {
        assert_eq!(
            msg(&c, sql),
            format!("wrong number of arguments to function {name}()"),
            "{sql}"
        );
    }
}
