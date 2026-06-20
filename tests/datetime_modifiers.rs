//! Differential sweep of date/time value parsing and modifiers against the
//! `sqlite3` 3.50.4 CLI (the ground-truth oracle).
//!
//! Every case below pairs a graphite expression with the exact output observed
//! from the real `sqlite3` CLI. The test asserts graphite matches that hard-coded
//! oracle value, and — when the CLI is available — re-derives the oracle live so a
//! future SQLite change can't silently drift the expectations.
//!
//! Deliberately excluded: the `localtime`/`utc` timezone modifiers (and
//! `'now','localtime'`), which on an ICU/tz-aware build convert against the host
//! timezone and are therefore non-deterministic. graphite treats them as UTC
//! no-ops by design, a documented, intentional divergence.

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

/// `(expression, expected sqlite3 3.50.4 output)`. An empty string is SQL NULL.
const CASES: &[(&str, &str)] = &[
    // ---- fractional unit offsets ----
    (
        "datetime('2000-01-01 00:00:00','+1.5 days')",
        "2000-01-02 12:00:00",
    ),
    (
        "datetime('2000-01-01 00:00:00','+0.25 seconds')",
        "2000-01-01 00:00:00",
    ),
    (
        "datetime('2000-01-01 00:00:00','+1.5 hours')",
        "2000-01-01 01:30:00",
    ),
    (
        "datetime('2000-01-01 12:00:00','-1.5 days')",
        "1999-12-31 00:00:00",
    ),
    (
        "datetime('2000-01-01 00:00:00','+1.5 months')",
        "2000-02-16 00:00:00",
    ),
    (
        "datetime('2000-01-01 00:00:00','+2.5 years')",
        "2002-07-02 12:00:00",
    ),
    ("date('2000-01-01','+1.9 days')", "2000-01-02"),
    ("date('2000-01-01','+1.1 days')", "2000-01-02"),
    ("date('2000-01-01','-1.9 days')", "1999-12-30"),
    // ---- calendar (month/year) clamping ----
    ("date('2024-01-31','+1 month')", "2024-03-02"),
    ("date('2024-02-29','+1 year')", "2025-03-01"),
    ("date('2023-02-28','+1 year')", "2024-02-28"),
    ("date('2024-01-31','+1 month','+1 month')", "2024-04-02"),
    ("date('2020-02-29','+4 years')", "2024-02-29"),
    ("date('2000-01-31','+13 months')", "2001-03-03"),
    ("date('2024-03-31','-1 month')", "2024-03-02"),
    // ---- floor / ceiling overflow resolution ----
    ("date('2024-01-31','+1 month','floor')", "2024-02-29"),
    ("date('2024-01-31','+1 month','ceiling')", "2024-03-02"),
    ("date('2000-01-01','floor')", "2000-01-01"),
    ("date('2000-01-01','ceiling')", "2000-01-01"),
    ("date('2023-01-31','+1 month','floor')", "2023-02-28"),
    ("date('2024-12-31','+2 months','floor')", "2025-02-28"),
    // ---- start of {day,month,year} ----
    (
        "datetime('2024-03-15 12:34:56','start of day')",
        "2024-03-15 00:00:00",
    ),
    (
        "datetime('2024-03-15 12:34:56','start of month')",
        "2024-03-01 00:00:00",
    ),
    (
        "datetime('2024-03-15 12:34:56','start of year')",
        "2024-01-01 00:00:00",
    ),
    // chained: last day of month
    (
        "date('2024-02-15','start of month','+1 month','-1 day')",
        "2024-02-29",
    ),
    (
        "date('2024-12-15','start of month','+1 month','-1 day')",
        "2024-12-31",
    ),
    (
        "date('2023-12-10','start of month','+1 month','-1 day')",
        "2023-12-31",
    ),
    (
        "datetime('2024-06-15 12:00:00','start of year','+6 months')",
        "2024-07-01 00:00:00",
    ),
    // ---- weekday N (each N, and already-on-day) ----
    ("date('2024-03-13','weekday 0')", "2024-03-17"),
    ("date('2024-03-13','weekday 1')", "2024-03-18"),
    ("date('2024-03-13','weekday 2')", "2024-03-19"),
    ("date('2024-03-13','weekday 3')", "2024-03-13"), // already Wednesday
    ("date('2024-03-13','weekday 4')", "2024-03-14"),
    ("date('2024-03-13','weekday 5')", "2024-03-15"),
    ("date('2024-03-13','weekday 6')", "2024-03-16"),
    (
        "datetime('2024-03-13 12:00:00','weekday 3')",
        "2024-03-13 12:00:00",
    ),
    ("date('2000-01-01','weekday 3.0')", "2000-01-05"), // integer-valued float
    ("date('2000-01-01','weekday +1')", "2000-01-03"),
    ("date('2000-01-01','weekday 3.5')", ""), // non-integer -> NULL
    ("date('2000-01-01','weekday 7')", ""),   // out of range -> NULL
    ("date('2024-03-13','weekday 1','weekday 5')", "2024-03-22"),
    // ---- ±HH:MM[:SS] time-shift modifiers ----
    (
        "datetime('2000-01-01 12:00:00','+05:00')",
        "2000-01-01 17:00:00",
    ),
    (
        "datetime('2000-01-01 12:00:00','-05:00')",
        "2000-01-01 07:00:00",
    ),
    (
        "datetime('2000-01-01 12:00:00','+05:30:15')",
        "2000-01-01 17:30:15",
    ),
    // ---- (+|-)YYYY-MM-DD[ HH:MM] calendar-offset modifier ----
    ("date('2000-01-01','+0001-00-00')", "2001-01-01"),
    ("date('2000-01-31','+0000-01-00')", "2000-03-02"),
    ("date('2000-01-15','+0001-02-03')", "2001-03-18"),
    (
        "datetime('2000-01-15 00:00:00','+0001-02-03 04:05')",
        "2001-03-18 04:05:00",
    ),
    ("date('2000-03-15','-0000-01-00')", "2000-02-15"),
    ("date('2024-02-29','+0001-00-00')", "2025-03-01"),
    (
        "datetime('2020-06-15 12:00:00','-0000-00-00 13:00')",
        "2020-06-14 23:00:00",
    ),
    ("date('2000-01-01','+0000-13-00')", ""), // MM must be 0-11 -> NULL
    ("date('2000-01-01','+0000-00-31')", ""), // DD must be 0-30 -> NULL
    ("date('2000-01-01','+12345-00-00')", ""), // 5-digit year field -> NULL
    // ---- input formats ----
    ("datetime('2000-01-01')", "2000-01-01 00:00:00"),
    ("datetime('2000-01-01 13:14')", "2000-01-01 13:14:00"),
    ("datetime('2000-01-01 13:14:15')", "2000-01-01 13:14:15"),
    ("datetime('2000-01-01 13:14:15.678')", "2000-01-01 13:14:15"),
    ("datetime('2000-01-01T13:14:15')", "2000-01-01 13:14:15"),
    ("datetime('2000-01-01 13:14:15Z')", "2000-01-01 13:14:15"),
    (
        "datetime('2000-01-01 13:14:15+05:00')",
        "2000-01-01 08:14:15",
    ),
    (
        "datetime('2000-01-01 13:14:15-05:00')",
        "2000-01-01 18:14:15",
    ),
    (
        "datetime('2000-01-01 13:14:15.5+05:00')",
        "2000-01-01 08:14:15",
    ),
    ("time('13:14')", "13:14:00"),
    ("time('13:14:15')", "13:14:15"),
    ("time('13:14:15.678')", "13:14:15"),
    ("datetime('13:14')", "2000-01-01 13:14:00"),
    ("datetime('13:14:15')", "2000-01-01 13:14:15"),
    ("time('12:00:00+05:00')", "07:00:00"),
    ("time('12:00Z')", "12:00:00"),
    // separators: any run of space/T between date and time
    ("datetime('2000-01-01  12:00:00')", "2000-01-01 12:00:00"),
    ("datetime('2000-01-01TT12:00:00')", "2000-01-01 12:00:00"),
    // trailing whitespace on a value is tolerated; leading whitespace is not
    ("date('2000-01-01 ')", "2000-01-01"),
    ("date(' 2000-01-01')", ""),
    ("date(' 2000-01-01 ')", ""),
    ("time(' 12:00:00')", ""),
    // ---- 24:00:00 clock preservation (not rolled into next day) ----
    ("datetime('2000-01-01 24:00:00')", "2000-01-01 24:00:00"),
    ("datetime('2000-01-01 24:30:00')", "2000-01-01 24:30:00"),
    ("date('2000-01-01 24:00:00')", "2000-01-01"),
    ("datetime('2024-02-30 24:00:00')", "2024-03-02 24:00:00"),
    (
        "datetime('2000-01-01 24:00:00','+0 seconds')",
        "2000-01-02 00:00:00",
    ),
    (
        "datetime('2000-01-01 24:00:00','+1 hour')",
        "2000-01-02 01:00:00",
    ),
    // ---- exact-digit-count parsing ----
    ("date('-001-01-01')", ""), // year needs exactly 4 digits
    ("date('001-01-01')", ""),
    ("date('-0001-01-01')", "-0001-01-01"),
    ("date('0001-01-01')", "0001-01-01"),
    ("date('2000-1-01')", ""), // month needs exactly 2 digits
    ("date('2000-01-1')", ""), // day needs exactly 2 digits
    ("time('1:00:00')", ""),
    ("time('12:5:00')", ""),
    ("time('12:05:5')", ""),
    ("datetime('2000-01-01 12:00:00+5:00')", ""),
    ("datetime('2000-01-01 12:00:00+05:0')", ""),
    ("date('99999-01-01')", ""),
    ("date('10000-01-01')", ""),
    // ---- out-of-range components -> NULL ----
    ("date('2024-13-01')", ""),
    ("date('2024-00-10')", ""),
    ("date('2024-02-30')", "2024-03-01"), // overflow normalized (no modifier)
    ("datetime('2024-02-30 12:00:00')", "2024-03-01 12:00:00"),
    ("time('25:00:00')", ""),
    ("time('24:00:00')", "24:00:00"),
    ("time('12:60:00')", ""),
    ("time('12:00:60')", ""),
    ("time('12:00:00+15:00')", ""), // tz hour max 14
    ("time('12:00:00+14:00')", "22:00:00"),
    // ---- julianday / unixepoch / auto number interpretation ----
    ("julianday('2000-01-01')", "2451544.5"),
    ("julianday('2000-01-01 12:00:00')", "2451545.0"),
    ("julianday('2000-01-01 06:00:00')", "2451544.75"),
    ("datetime(2451545.0)", "2000-01-01 12:00:00"),
    ("datetime(2451545)", "2000-01-01 12:00:00"),
    ("datetime(0)", "-4713-11-24 12:00:00"),
    ("datetime(0,'unixepoch')", "1970-01-01 00:00:00"),
    ("datetime(1234567890,'unixepoch')", "2009-02-13 23:31:30"),
    ("unixepoch('2000-01-01')", "946684800"),
    ("unixepoch('2038-01-19 03:14:07')", "2147483647"),
    ("datetime('2451545','auto')", "2000-01-01 12:00:00"),
    ("datetime('1234567890','auto')", "2009-02-13 23:31:30"),
    ("datetime(1234567890,'auto')", "2009-02-13 23:31:30"),
    (
        "datetime('1000000000.5','unixepoch','subsec')",
        "2001-09-09 01:46:40.500",
    ),
    // out-of-JD-range raw numbers
    ("datetime(5373484)", "9999-12-31 12:00:00"),
    ("datetime(5373485)", ""),
    ("datetime(-1)", ""),
    ("datetime(5000000000)", ""),
    // auto/julianday/unixepoch only valid as the FIRST modifier
    ("datetime(2440587.5,'+1 day','auto')", ""),
    ("datetime('2000-01-01','+1 day','auto')", ""),
    ("datetime(2440587.5,'auto','auto')", ""),
    // ---- whitespace rules on modifiers (surrounding whitespace -> NULL) ----
    ("date('2000-01-01','+1 day')", "2000-01-02"),
    ("date('2000-01-01','+1  day')", "2000-01-02"), // internal ws ok
    ("date('2000-01-01','+1   day')", "2000-01-02"),
    ("date('2000-01-01','+1day')", ""), // no separator -> NULL
    ("date('2000-01-01',' +1 day')", ""),
    ("date('2000-01-01','+1 day ')", ""),
    ("date('2000-01-01','  +1 day  ')", ""),
    ("date('2000-01-01','+ 1 days')", ""),
    ("date('2000-01-01',' start of month')", ""),
    ("date('2000-01-01','start of month ')", ""),
    ("date('2000-01-01','unixepoch ')", ""),
    ("date('2000-01-01',' weekday 0')", ""),
    ("date('2000-01-01','weekday 0 ')", "2000-01-02"), // trailing ws after N ok
    // ---- mixed-case modifiers (case-insensitive) ----
    ("date('2000-01-01','+1 Day')", "2000-01-02"),
    ("date('2000-01-01','Start Of Month')", "2000-01-01"),
    ("date('2000-01-01','WEEKDAY 1')", "2000-01-03"),
    // ---- timediff ----
    (
        "timediff('2024-03-01','2024-01-01')",
        "+0000-02-00 00:00:00.000",
    ),
    (
        "timediff('2024-01-01','2024-03-01')",
        "-0000-02-00 00:00:00.000",
    ),
    // ---- year 9999 ceiling ----
    ("datetime('9999-12-31 23:59:59')", "9999-12-31 23:59:59"),
    ("datetime('9999-12-31 23:59:59','+1 second')", ""),
    ("datetime('9999-12-31 24:00:00')", ""),
    ("date('9999-12-31','+1 day')", ""),
    // ---- overflow / malformed input must not panic ----
    ("date('')", ""),
    ("date('-')", ""),
    ("date('2000-01-01','+99999999999999999999 days')", ""),
    ("date('2000-01-01','+1e10 days')", ""),
    ("date('2000-01-01','+')", ""),
    ("date('2000-01-01',':')", ""),
    ("date('2000-01-01','weekday x')", ""),
];

#[test]
fn modifiers_and_parsing_against_sqlite3() {
    let conn = Connection::open_memory().unwrap();
    let live = sqlite_available();
    if !live {
        eprintln!("sqlite3 CLI not found; asserting against recorded oracle only");
    }

    let mut failures = Vec::new();
    for (expr, oracle) in CASES {
        // When the CLI is present, re-derive the oracle so the recorded value
        // can't silently drift from the real sqlite3.
        if live {
            if let Some(live_val) = sqlite_eval(expr) {
                assert_eq!(
                    &live_val, oracle,
                    "recorded oracle for `{expr}` is stale: sqlite3 now says {live_val:?}, \
                     test expects {oracle:?}"
                );
            }
        }
        match conn.query(&format!("SELECT {expr}")) {
            Ok(r) => {
                let got = render(&r.rows[0][0]);
                if got != *oracle {
                    failures.push(format!(
                        "  {expr}\n    sqlite:   {oracle:?}\n    graphite: {got:?}"
                    ));
                }
            }
            Err(err) => failures.push(format!("  {expr}\n    graphite error: {err}")),
        }
    }
    assert!(
        failures.is_empty(),
        "{} date/time expressions diverged from sqlite3:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
