//! Differential testing of the date/time and printf functions against `sqlite3`.
//!
//! These are deterministic (no `'now'`) so the same expression must produce
//! byte-identical output in graphitesql and the real `sqlite3` CLI. Skipped if
//! the CLI is unavailable.

#![cfg(feature = "std")]

use graphitesql::{Connection, Value};
use std::process::Command;

fn sqlite_available() -> bool {
    Command::new("sqlite3").arg("--version").output().is_ok()
}

fn sqlite_eval(expr: &str) -> Option<String> {
    let out = Command::new("sqlite3")
        .arg(":memory:")
        .arg(format!("SELECT {expr};"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn render(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Integer(i) => i.to_string(),
        Value::Text(s) => s.clone(),
        // Match how sqlite3 prints doubles: whole values get a trailing `.0`.
        Value::Real(r) => {
            if *r == r.trunc() && r.is_finite() && r.abs() < 1e15 {
                format!("{r:.1}")
            } else {
                format!("{r}")
            }
        }
        Value::Blob(b) => b.iter().map(|x| format!("{x:02x}")).collect(),
    }
}

#[test]
fn datetime_against_sqlite3() {
    if !sqlite_available() {
        eprintln!("sqlite3 CLI not found; skipping datetime differential");
        return;
    }
    let conn = Connection::open_memory().unwrap();

    let exprs = [
        // date / time / datetime with explicit values
        "date('2000-01-01')",
        "date('2024-02-29')",
        "time('2000-01-01 13:14:15')",
        "datetime('2000-01-01 13:14:15')",
        "date('2000-01-01T13:14:15')",
        "datetime('1999-12-31 23:59:59')",
        // modifiers
        "date('2000-01-01','+1 day')",
        "date('2000-01-01','-1 day')",
        "date('2000-01-31','+1 month')",
        "date('2000-12-31','+1 month')",
        "date('2000-01-01','+1 year')",
        "date('2004-02-29','+1 year')",
        "datetime('2000-01-01 23:30:00','+1 hour')",
        "datetime('2000-01-01 00:00:30','+90 seconds')",
        "datetime('2000-01-01 00:30:00','+45 minutes')",
        "date('2000-06-15','start of month')",
        "date('2000-06-15','start of year')",
        "datetime('2000-06-15 12:34:56','start of day')",
        "date('2000-01-01','+1 month','+1 day')",
        "date('2000-03-01','-1 day')",
        "date('2000-01-01','weekday 0')",
        "date('2000-01-01','weekday 6')",
        "date('2000-01-01','weekday 1')",
        // unixepoch / julianday
        "datetime(0,'unixepoch')",
        "datetime(1000000000,'unixepoch')",
        "unixepoch('2009-02-13 23:31:30')",
        "unixepoch('1970-01-01')",
        "julianday('2000-01-01')",
        "julianday('2000-01-01 12:00:00')",
        "date(2451545,'-0.5 days')",
        // strftime
        "strftime('%Y-%m-%d','2000-01-02')",
        "strftime('%H:%M:%S','2000-01-02 03:04:05')",
        "strftime('%Y/%m/%d %H:%M','2023-07-19 08:09:10')",
        "strftime('%j','2000-03-01')",
        "strftime('%j','2000-01-01')",
        "strftime('%w','2000-01-01')",
        "strftime('%w','2000-01-02')",
        "strftime('%s','1970-01-01 00:01:00')",
        "strftime('%d/%m/%Y','2012-12-25')",
        "strftime('%H','2000-01-01 00:00:00')",
        "strftime('%M minutes','2000-01-01 12:30:00')",
        "strftime('%%literal%%','2000-01-01')",
        // %J renders the Julian day at 16 significant digits (%.16g), more than
        // julianday()'s default 15 — integer at noon, fractional otherwise.
        "strftime('%J','2024-06-15 12:34:56')",
        "strftime('%J','2024-06-15 12:34:56.789')",
        "strftime('%J','2024-06-15 12:00:00')",
        "strftime('%J','1999-12-31 23:59:59')",
        "strftime('%J','2024-01-01')",
        // NULL / invalid
        "date(NULL)",
        "date('not a date')",
        "time('garbage')",
        // printf / format
        "printf('%d',42)",
        "printf('%05d',42)",
        "printf('%-5d|',7)",
        "printf('%+d',7)",
        "printf('%x',255)",
        "printf('%X',255)",
        "printf('%o',64)",
        "printf('%.2f',3.14159)",
        "printf('%8.2f',3.14159)",
        "printf('%s-%s','a','b')",
        "printf('%.3s','abcdef')",
        "printf('%5s|','ab')",
        "printf('%-5s|','ab')",
        "printf('%d%%',50)",
        "printf('%c%c%c',104,105,33)",
        "printf('%q','it''s')",
        "printf('%Q','it''s')",
        "printf('no args')",
        "format('%d and %d',1,2)",
        "printf('%i',-15)",
        "printf('%6.2f',-3.5)",
    ];

    let mut failures = Vec::new();
    let mut passed = 0;
    let mut total = 0;
    for e in exprs {
        let Some(want) = sqlite_eval(e) else { continue };
        total += 1;
        match conn.query(&format!("SELECT {e}")) {
            Ok(r) => {
                let got = render(&r.rows[0][0]);
                if got == want {
                    passed += 1;
                } else {
                    failures.push(format!(
                        "  {e}\n    sqlite:   {want:?}\n    graphite: {got:?}"
                    ));
                }
            }
            Err(err) => failures.push(format!("  {e}\n    graphite error: {err}")),
        }
    }
    eprintln!("datetime differential: {passed}/{total} matched sqlite3");
    assert!(
        failures.is_empty(),
        "{} datetime/printf expressions diverged:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
