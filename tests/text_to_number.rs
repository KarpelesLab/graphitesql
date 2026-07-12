//! SQLite's text→number coercion reads a decimal numeric prefix and does NOT
//! recognise the word forms `inf`/`infinity`/`nan` (Rust's `f64::from_str`
//! does). A numeric *overflow* like `1e400` is still a valid number → ±Inf.
//! Matched against the `sqlite3` CLI.

#![cfg(feature = "std")]

use graphitesql::Connection;
use std::process::Command;

fn sqlite3_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite_scalar(sql: &str) -> String {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(sql)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn graphite_scalar(sql: &str) -> String {
    let c = Connection::open_memory().unwrap();
    let r = c.query(sql).unwrap();
    use graphitesql::Value::*;
    match &r.rows[0][0] {
        Null => String::new(),
        Integer(i) => i.to_string(),
        Real(x) => graphitesql::exec::eval::format_real(*x),
        Text(t) => String::from(t.as_str()),
        Blob(_) => "<blob>".into(),
    }
}

#[test]
fn inf_and_nan_words_are_not_numbers() {
    if !sqlite3_available() {
        eprintln!("sqlite3 CLI not found; skipping");
        return;
    }
    let exprs = [
        "cast('inf' as real)",
        "cast('infinity' as real)",
        "cast('nan' as real)",
        "cast('-inf' as real)",
        "cast('  -inf  ' as real)",
        "'inf' + 0",
        "abs('inf')",
        "sign('inf')",
        // Genuine numeric overflow is still ±Inf, not rejected.
        "cast('1e400' as real)",
        "cast('1e1000' as real)",
        "'1e400' + 0",
        // Ordinary decimal text still parses.
        "cast('.5e3' as real)",
        "cast(' 3.14 ' as real)",
        "'42abc' + 0",
    ];
    for e in exprs {
        let sql = format!("SELECT {e}");
        assert_eq!(
            graphite_scalar(&sql),
            sqlite_scalar(&format!("{sql};")),
            "text→number diverged for {e}"
        );
    }
}
