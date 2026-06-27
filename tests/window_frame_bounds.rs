//! A window frame's two boundaries are constrained two ways, and SQLite reports
//! the two failure classes with different messages:
//!
//!   * **Grammar** — `UNBOUNDED FOLLOWING` is not a legal *start* bound and
//!     `UNBOUNDED PRECEDING` is not a legal *end* bound; each is a
//!     `near "FOLLOWING"/"PRECEDING": syntax error` pointing at the direction
//!     keyword (the bare `ROWS UNBOUNDED FOLLOWING` start form too).
//!   * **Semantics** — the start's bound category may not come after the end's:
//!     the three combos CURRENT/PRECEDING, FOLLOWING/PRECEDING and
//!     FOLLOWING/CURRENT are an `unsupported frame specification` (a real
//!     message, not a `near` syntax error). The numeric offset is not compared,
//!     so `2 PRECEDING AND 1 PRECEDING` / `2 FOLLOWING AND 1 FOLLOWING` are
//!     valid (empty) frames.
//!
//! Verified against the sqlite3 3.50.4 CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;

fn run(frame: &str) -> Result<(), String> {
    let mut c = Connection::open_memory().unwrap();
    c.execute("CREATE TABLE t(a)").unwrap();
    c.query(&format!("SELECT count(*) OVER (ORDER BY a {frame}) FROM t"))
        .map(|_| ())
        .map_err(|e| {
            e.to_string()
                .trim_start_matches("error: ")
                .trim_start_matches("SQL error: ")
                .to_string()
        })
}

fn ok(frame: &str) {
    run(frame).unwrap_or_else(|e| panic!("expected {frame:?} to parse, got: {e}"));
}

fn err(frame: &str, msg: &str) {
    assert_eq!(run(frame).unwrap_err(), msg, "{frame:?}");
}

const UNSUPPORTED: &str = "unsupported frame specification";

#[test]
fn start_after_end_is_unsupported() {
    err("ROWS BETWEEN CURRENT ROW AND 1 PRECEDING", UNSUPPORTED);
    err("ROWS BETWEEN 1 FOLLOWING AND 1 PRECEDING", UNSUPPORTED);
    err("ROWS BETWEEN 1 FOLLOWING AND CURRENT ROW", UNSUPPORTED);
    // All three frame modes apply the same rule.
    err("RANGE BETWEEN 1 FOLLOWING AND 1 PRECEDING", UNSUPPORTED);
    err("GROUPS BETWEEN 1 FOLLOWING AND CURRENT ROW", UNSUPPORTED);
}

#[test]
fn unbounded_following_start_is_a_syntax_error() {
    err(
        "ROWS BETWEEN UNBOUNDED FOLLOWING AND UNBOUNDED FOLLOWING",
        "near \"FOLLOWING\": syntax error",
    );
    // The grammar error outranks any ordering check.
    err(
        "ROWS BETWEEN UNBOUNDED FOLLOWING AND 1 PRECEDING",
        "near \"FOLLOWING\": syntax error",
    );
    // Bare start form.
    err(
        "ROWS UNBOUNDED FOLLOWING",
        "near \"FOLLOWING\": syntax error",
    );
}

#[test]
fn unbounded_preceding_end_is_a_syntax_error() {
    err(
        "ROWS BETWEEN CURRENT ROW AND UNBOUNDED PRECEDING",
        "near \"PRECEDING\": syntax error",
    );
    err(
        "ROWS BETWEEN 1 PRECEDING AND UNBOUNDED PRECEDING",
        "near \"PRECEDING\": syntax error",
    );
}

#[test]
fn valid_frames_still_parse() {
    ok("ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING");
    ok("ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW");
    ok("ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING");
    ok("ROWS BETWEEN CURRENT ROW AND CURRENT ROW");
    ok("ROWS UNBOUNDED PRECEDING");
    // Equal category, reversed offsets: a valid (empty) frame, not unsupported.
    ok("ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING");
    ok("ROWS BETWEEN 2 FOLLOWING AND 1 FOLLOWING");
}
