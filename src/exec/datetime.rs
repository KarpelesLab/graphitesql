//! Date/time functions and `printf`/`format`.
//!
//! This is a faithful, dependency-free port of the core of SQLite's `date.c`.
//! Time values are represented internally as an integer Julian Day number scaled
//! to milliseconds (`ijd = JD * 86_400_000`), exactly as upstream does, so the
//! arithmetic — and therefore the formatted output — matches `sqlite3`.
//!
//! Everything is computed in **UTC**. The `localtime`/`utc` modifiers require a
//! timezone database, which graphitesql intentionally does not depend on; they
//! are treated as no-ops (UTC). All other behavior mirrors SQLite, and the
//! results are verified differentially against the real `sqlite3` CLI.
//!
//! The current time (`'now'`, the no-argument forms) needs a clock, which a
//! `no_std` crate has no portable access to; those forms return `NULL` rather
//! than a wrong answer. With the `std` feature a real clock is wired in.

use super::eval;
use crate::util::float;
use crate::value::Value;
use alloc::string::String;
use alloc::vec::Vec;

/// A parsed date/time, mirroring SQLite's `DateTime` struct.
#[derive(Clone, Copy, Default)]
struct DateTime {
    ijd: i64, // Julian day number * 86_400_000 (ms)
    y: i32,   // year
    m: i32,   // month (1-12)
    d: i32,   // day (1-31)
    h: i32,   // hour (0-23)
    min: i32, // minute (0-59)
    s: f64,   // seconds, including fractional part
    tz: i32,  // timezone offset in minutes
    valid_jd: bool,
    valid_ymd: bool,
    valid_hms: bool,
    valid_tz: bool,
    raw_s: bool, // the value came in as a bare number (for `unixepoch`)
}

impl DateTime {
    fn clear_ymd_hms_tz(&mut self) {
        self.valid_ymd = false;
        self.valid_hms = false;
        self.valid_tz = false;
    }

    /// Port of `computeJD`: derive `ijd` from Y/M/D (+ H/M/S/TZ).
    fn compute_jd(&mut self) {
        if self.valid_jd {
            return;
        }
        let (mut year, mut month, day) = if self.valid_ymd {
            (self.y, self.m, self.d)
        } else {
            (2000, 1, 1)
        };
        if month <= 2 {
            year -= 1;
            month += 12;
        }
        let a = year / 100;
        let b = 2 - a + a / 4;
        let x1 = 36525 * (year + 4716) / 100;
        let x2 = 306001 * (month + 1) / 10000;
        self.ijd = (((x1 + x2 + day + b) as f64 - 1524.5) * 86_400_000.0) as i64;
        self.valid_jd = true;
        if self.valid_hms {
            self.ijd += self.h as i64 * 3_600_000
                + self.min as i64 * 60_000
                + (self.s * 1000.0 + 0.5) as i64;
            if self.valid_tz {
                self.ijd -= self.tz as i64 * 60_000;
                self.valid_ymd = false;
                self.valid_hms = false;
                self.valid_tz = false;
            }
        }
    }

    /// Port of `computeYMD`.
    fn compute_ymd(&mut self) {
        if self.valid_ymd {
            return;
        }
        if !self.valid_jd {
            self.y = 2000;
            self.m = 1;
            self.d = 1;
        } else {
            let z = ((self.ijd + 43_200_000) / 86_400_000) as i32;
            let mut a = ((z as f64 - 1_867_216.25) / 36_524.25) as i32;
            a = z + 1 + a - a / 4;
            let b = a + 1524;
            let c = ((b as f64 - 122.1) / 365.25) as i32;
            let d = 36525 * (c & 32767) / 100;
            let e = ((b - d) as f64 / 30.6001) as i32;
            let x1 = (30.6001 * e as f64) as i32;
            self.d = b - d - x1;
            self.m = if e < 14 { e - 1 } else { e - 13 };
            self.y = if self.m > 2 { c - 4716 } else { c - 4715 };
        }
        self.valid_ymd = true;
    }

    /// Port of `computeHMS`.
    fn compute_hms(&mut self) {
        if self.valid_hms {
            return;
        }
        self.compute_jd();
        let mut s = ((self.ijd + 43_200_000) % 86_400_000) as i32;
        self.s = s as f64 / 1000.0;
        s = self.s as i32;
        self.s -= s as f64;
        self.h = s / 3600;
        s -= self.h * 3600;
        self.min = s / 60;
        self.s += (s - self.min * 60) as f64;
        self.valid_hms = true;
    }

    fn compute_ymd_hms(&mut self) {
        self.compute_ymd();
        self.compute_hms();
    }
}

/// Set the date from a bare number, treated as a Julian day (port of
/// `setRawDateNumber`).
fn set_raw_date_number(p: &mut DateTime, r: f64) {
    p.s = r;
    p.raw_s = true;
    if (0.0..5_373_484.5).contains(&r) {
        p.ijd = (r * 86_400_000.0 + 0.5) as i64;
        p.valid_jd = true;
    }
}

/// Parse `YYYY-MM-DD[ T]HH:MM[:SS[.SSS]][tz]`, or just a date, into `p`.
fn parse_yyyy_mm_dd(z: &str, p: &mut DateTime) -> bool {
    let bytes = z.as_bytes();
    let mut i = 0;
    let neg = if bytes.first() == Some(&b'-') {
        i = 1;
        true
    } else {
        false
    };
    // year: up to 4 digits
    let (year, ni) = read_int(bytes, i, 4);
    let Some(year) = year else { return false };
    i = ni;
    if bytes.get(i) != Some(&b'-') {
        return false;
    }
    i += 1;
    let (month, ni) = read_int(bytes, i, 2);
    let Some(month) = month else { return false };
    i = ni;
    if bytes.get(i) != Some(&b'-') {
        return false;
    }
    i += 1;
    let (day, ni) = read_int(bytes, i, 2);
    let Some(day) = day else { return false };
    i = ni;
    // optional time, after a separator
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'T' || bytes[i] == b't') {
        i += 1;
    }
    if i < bytes.len() {
        if !parse_hh_mm_ss(&z[i..], p) {
            return false;
        }
    } else {
        p.valid_hms = false;
        p.valid_tz = false;
    }
    p.valid_jd = false;
    p.valid_ymd = true;
    p.y = if neg { -year } else { year };
    p.m = month;
    p.d = day;
    true
}

/// Parse `HH:MM[:SS[.SSS]][tz]` into `p`.
fn parse_hh_mm_ss(z: &str, p: &mut DateTime) -> bool {
    let bytes = z.as_bytes();
    let mut i = 0;
    let (h, ni) = read_int(bytes, i, 2);
    let Some(h) = h else { return false };
    i = ni;
    if bytes.get(i) != Some(&b':') {
        return false;
    }
    i += 1;
    let (min, ni) = read_int(bytes, i, 2);
    let Some(min) = min else { return false };
    i = ni;
    let mut sec = 0.0;
    if bytes.get(i) == Some(&b':') {
        i += 1;
        let (s, ni) = read_int(bytes, i, 2);
        let Some(s) = s else { return false };
        i = ni;
        sec = s as f64;
        if bytes.get(i) == Some(&b'.') {
            i += 1;
            let start = i;
            let mut scale = 1.0;
            let mut frac = 0.0;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                scale *= 10.0;
                frac = frac * 10.0 + (bytes[i] - b'0') as f64;
                i += 1;
            }
            if i == start {
                return false;
            }
            sec += frac / scale;
        }
    }
    p.valid_jd = false;
    p.valid_hms = true;
    p.h = h;
    p.min = min;
    p.s = sec;
    // optional timezone
    parse_timezone(z, i, p)
}

/// Parse a trailing timezone (`Z`, `+HH:MM`, `-HH:MM`) starting at byte `i`.
fn parse_timezone(z: &str, mut i: usize, p: &mut DateTime) -> bool {
    let bytes = z.as_bytes();
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    p.tz = 0;
    p.valid_tz = false;
    if i >= bytes.len() {
        return true;
    }
    let sign = match bytes[i] {
        b'Z' | b'z' => {
            i += 1;
            p.valid_tz = true;
            return i == bytes.len();
        }
        b'+' => 1,
        b'-' => -1,
        _ => return false,
    };
    i += 1;
    let (th, ni) = read_int(bytes, i, 2);
    let Some(th) = th else { return false };
    i = ni;
    if bytes.get(i) != Some(&b':') {
        return false;
    }
    i += 1;
    let (tm, ni) = read_int(bytes, i, 2);
    let Some(tm) = tm else { return false };
    i = ni;
    p.tz = sign * (th * 60 + tm);
    p.valid_tz = true;
    i == bytes.len()
}

/// Read up to `maxlen` ASCII digits as an int. Returns the value and the new
/// index; `None` if no digit was present.
fn read_int(bytes: &[u8], start: usize, maxlen: usize) -> (Option<i32>, usize) {
    let mut i = start;
    let mut val: i32 = 0;
    let mut seen = false;
    while i < bytes.len() && i < start + maxlen && bytes[i].is_ascii_digit() {
        val = val * 10 + (bytes[i] - b'0') as i32;
        seen = true;
        i += 1;
    }
    if seen {
        (Some(val), i)
    } else {
        (None, start)
    }
}

/// Parse a `Value` into a `DateTime` (port of `parseDateOrTime`). Returns `None`
/// on `NULL` / unparseable input or the unsupported `'now'`.
fn parse_value(v: &Value) -> Option<DateTime> {
    let mut p = DateTime::default();
    match v {
        Value::Null => None,
        Value::Integer(i) => {
            set_raw_date_number(&mut p, *i as f64);
            Some(p)
        }
        Value::Real(r) => {
            set_raw_date_number(&mut p, *r);
            Some(p)
        }
        Value::Text(s) => {
            let z = s.trim();
            if parse_yyyy_mm_dd(z, &mut p) || parse_hh_mm_ss(z, &mut p) {
                Some(p)
            } else if z.eq_ignore_ascii_case("now") {
                set_to_now(&mut p).then_some(p)
            } else if let Some(r) = parse_float(z) {
                set_raw_date_number(&mut p, r);
                Some(p)
            } else {
                None
            }
        }
        Value::Blob(_) => None,
    }
}

/// Set `p` to the current UTC time. Requires a clock, available only with the
/// `std` feature; returns `false` (=> NULL) otherwise.
fn set_to_now(p: &mut DateTime) -> bool {
    match current_ijd() {
        Some(ijd) => {
            p.clear_ymd_hms_tz();
            p.ijd = ijd;
            p.valid_jd = true;
            p.raw_s = false;
            true
        }
        None => false,
    }
}

#[cfg(feature = "std")]
fn current_ijd() -> Option<i64> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some(ms + 210_866_760_000_000)
}

#[cfg(not(feature = "std"))]
fn current_ijd() -> Option<i64> {
    None
}

fn parse_float(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Apply one modifier string (port of the common cases of `parseModifier`).
/// Returns `false` if the modifier is unrecognized/invalid.
fn apply_modifier(p: &mut DateTime, m: &str) -> bool {
    let m = m.trim();
    let lower = m.to_ascii_lowercase();
    match lower.as_str() {
        // Timezone modifiers: no tz database => treat as UTC no-ops.
        "utc" | "localtime" => true,
        "unixepoch" => {
            if p.raw_s {
                let r = p.s * 1000.0 + 210_866_760_000_000.0;
                if (0.0..464_269_060_800_000.0).contains(&r) {
                    p.clear_ymd_hms_tz();
                    p.ijd = (r + 0.5) as i64;
                    p.valid_jd = true;
                    p.raw_s = false;
                    return true;
                }
            }
            false
        }
        "julianday" => {
            // Force the raw number to be interpreted as a Julian day (default).
            if p.raw_s {
                p.raw_s = false;
            }
            true
        }
        "auto" => {
            if p.raw_s {
                // < 5373484.5 days => already a JD; otherwise a unix timestamp.
                if p.s >= 0.0 && p.s < 5_373_484.5 {
                    p.raw_s = false;
                } else {
                    return apply_modifier(p, "unixepoch");
                }
            }
            true
        }
        "start of day" => {
            p.compute_ymd_hms();
            p.s = 0.0;
            p.min = 0;
            p.h = 0;
            p.valid_jd = false;
            p.valid_hms = true;
            p.valid_tz = false;
            p.compute_jd();
            true
        }
        "start of month" => {
            p.compute_ymd();
            p.d = 1;
            p.s = 0.0;
            p.min = 0;
            p.h = 0;
            p.valid_jd = false;
            p.valid_hms = true;
            p.valid_tz = false;
            p.compute_jd();
            true
        }
        "start of year" => {
            p.compute_ymd();
            p.m = 1;
            p.d = 1;
            p.s = 0.0;
            p.min = 0;
            p.h = 0;
            p.valid_jd = false;
            p.valid_hms = true;
            p.valid_tz = false;
            p.compute_jd();
            true
        }
        "subsec" | "subsecond" => true,
        _ => apply_numeric_modifier(p, m, &lower),
    }
}

/// Handle `±N units`, `weekday N`, and `±HH:MM[:SS]` modifiers.
fn apply_numeric_modifier(p: &mut DateTime, orig: &str, lower: &str) -> bool {
    if let Some(rest) = lower.strip_prefix("weekday ") {
        if let Some(n) = rest
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|n| (0..=6).contains(n))
        {
            p.compute_ymd_hms();
            p.compute_jd();
            let cur = (p.ijd + 129_600_000) / 86_400_000 % 7; // 0 = Sunday
            let mut delta = n - cur;
            if delta < 0 {
                delta += 7;
            }
            p.ijd += delta * 86_400_000;
            p.clear_ymd_hms_tz();
            return true;
        }
        return false;
    }

    // Parse a leading signed number.
    let bytes = orig.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let num_start = 0;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    if i == 0 || (i == 1 && !bytes[0].is_ascii_digit()) {
        return false;
    }
    // `±HH:MM[:SS]` time-shift form.
    if bytes.get(i) == Some(&b':') {
        return apply_time_shift(p, orig);
    }
    let Some(r) = parse_float(&orig[num_start..i]) else {
        return false;
    };
    let unit = orig[i..].trim().to_ascii_lowercase();
    let rounder = if r < 0.0 { -0.5 } else { 0.5 };
    match unit.as_str() {
        "day" | "days" => {
            p.compute_jd();
            p.ijd += (r * 86_400_000.0 + rounder) as i64;
            p.clear_ymd_hms_tz();
        }
        "hour" | "hours" => {
            p.compute_jd();
            p.ijd += (r * 3_600_000.0 + rounder) as i64;
            p.clear_ymd_hms_tz();
        }
        "minute" | "minutes" => {
            p.compute_jd();
            p.ijd += (r * 60_000.0 + rounder) as i64;
            p.clear_ymd_hms_tz();
        }
        "second" | "seconds" => {
            p.compute_jd();
            p.ijd += (r * 1000.0 + rounder) as i64;
            p.clear_ymd_hms_tz();
        }
        "month" | "months" => {
            p.compute_ymd_hms();
            p.m += r as i32;
            let x = if p.m > 0 {
                (p.m - 1) / 12
            } else {
                (p.m - 12) / 12
            };
            p.y += x;
            p.m -= x * 12;
            p.valid_jd = false;
            p.compute_jd();
            let frac = r - float::trunc(r);
            if frac != 0.0 {
                p.ijd += (frac * 30.0 * 86_400_000.0 + rounder) as i64;
            }
            // Renormalize an overflowed day (e.g. Feb 31 -> Mar 2) from the JD.
            p.clear_ymd_hms_tz();
        }
        "year" | "years" => {
            p.compute_ymd_hms();
            p.y += r as i32;
            p.valid_jd = false;
            p.compute_jd();
            let frac = r - float::trunc(r);
            if frac != 0.0 {
                p.ijd += (frac * 365.0 * 86_400_000.0 + rounder) as i64;
            }
            p.clear_ymd_hms_tz();
        }
        _ => return false,
    }
    true
}

/// Apply a `±HH:MM[:SS]` time-shift modifier.
fn apply_time_shift(p: &mut DateTime, orig: &str) -> bool {
    let bytes = orig.as_bytes();
    let sign = match bytes.first() {
        Some(b'+') => 1.0,
        Some(b'-') => -1.0,
        _ => return false,
    };
    let mut tmp = DateTime::default();
    if !parse_hh_mm_ss(&orig[1..], &mut tmp) {
        return false;
    }
    let ms = tmp.h as i64 * 3_600_000 + tmp.min as i64 * 60_000 + (tmp.s * 1000.0 + 0.5) as i64;
    p.compute_jd();
    p.ijd += (sign as i64) * ms;
    p.clear_ymd_hms_tz();
    true
}

/// Parse the `(timevalue, modifier, ...)` argument list into a finished
/// `DateTime`. Returns `None` if any part is NULL/invalid.
fn is_date(args: &[Value]) -> Option<DateTime> {
    // No time value => current time ("now"), matching `date()`/`time()`/etc.
    let mut p = match args.first() {
        Some(v) => parse_value(v)?,
        None => {
            let mut p = DateTime::default();
            if !set_to_now(&mut p) {
                return None;
            }
            p
        }
    };
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    for m in rest {
        let Value::Text(ms) = m else { return None };
        if !apply_modifier(&mut p, ms) {
            return None;
        }
    }
    p.compute_jd();
    Some(p)
}

// ---- output formatting ------------------------------------------------------

fn fmt_date(p: &mut DateTime) -> String {
    p.compute_ymd();
    if p.y < 0 || p.y > 9999 {
        alloc::format!("{:+05}-{:02}-{:02}", p.y, p.m, p.d)
    } else {
        alloc::format!("{:04}-{:02}-{:02}", p.y, p.m, p.d)
    }
}

fn fmt_time(p: &mut DateTime) -> String {
    p.compute_hms();
    alloc::format!("{:02}:{:02}:{:02}", p.h, p.min, p.s as i32)
}

/// `date(...)` -> `YYYY-MM-DD`.
pub fn date(args: &[Value]) -> Value {
    match is_date(args) {
        Some(mut p) => Value::Text(fmt_date(&mut p)),
        None => Value::Null,
    }
}

/// `time(...)` -> `HH:MM:SS`.
pub fn time(args: &[Value]) -> Value {
    match is_date(args) {
        Some(mut p) => Value::Text(fmt_time(&mut p)),
        None => Value::Null,
    }
}

/// `datetime(...)` -> `YYYY-MM-DD HH:MM:SS`.
pub fn datetime(args: &[Value]) -> Value {
    match is_date(args) {
        Some(mut p) => Value::Text(alloc::format!("{} {}", fmt_date(&mut p), fmt_time(&mut p))),
        None => Value::Null,
    }
}

/// `julianday(...)` -> floating-point Julian day number.
pub fn julianday(args: &[Value]) -> Value {
    match is_date(args) {
        Some(p) => Value::Real(p.ijd as f64 / 86_400_000.0),
        None => Value::Null,
    }
}

/// `unixepoch(...)` -> integer seconds since 1970 (no fractional modifier).
pub fn unixepoch(args: &[Value]) -> Value {
    match is_date(args) {
        Some(p) => Value::Integer((p.ijd - 210_866_760_000_000) / 1000),
        None => Value::Null,
    }
}

/// `strftime(format, timevalue, modifier, ...)`.
pub fn strftime(args: &[Value]) -> Value {
    if args.len() < 2 {
        return Value::Null;
    }
    let Value::Text(fmt) = &args[0] else {
        return Value::Null;
    };
    let Some(mut p) = is_date(&args[1..]) else {
        return Value::Null;
    };
    Value::Text(render_strftime(fmt, &mut p))
}

fn render_strftime(fmt: &str, p: &mut DateTime) -> String {
    p.compute_ymd_hms();
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('d') => out.push_str(&alloc::format!("{:02}", p.d)),
            Some('e') => out.push_str(&alloc::format!("{:2}", p.d)),
            Some('f') => {
                let sec = p.s;
                out.push_str(&alloc::format!("{:06.3}", sec));
            }
            Some('F') => out.push_str(&fmt_date(p)),
            Some('H') => out.push_str(&alloc::format!("{:02}", p.h)),
            Some('I') => {
                let h12 = ((p.h + 11) % 12) + 1;
                out.push_str(&alloc::format!("{:02}", h12));
            }
            Some('j') => {
                let mut y0 = *p;
                y0.valid_jd = false;
                y0.m = 1;
                y0.d = 1;
                y0.h = 0;
                y0.min = 0;
                y0.s = 0.0;
                y0.valid_ymd = true;
                y0.valid_hms = true;
                y0.compute_jd();
                let nday = ((p.ijd - y0.ijd + 43_200_000) / 86_400_000) + 1;
                out.push_str(&alloc::format!("{:03}", nday));
            }
            Some('J') => out.push_str(&eval::format_real(p.ijd as f64 / 86_400_000.0)),
            Some('k') => out.push_str(&alloc::format!("{:2}", p.h)),
            Some('l') => {
                let h12 = ((p.h + 11) % 12) + 1;
                out.push_str(&alloc::format!("{:2}", h12));
            }
            Some('m') => out.push_str(&alloc::format!("{:02}", p.m)),
            Some('M') => out.push_str(&alloc::format!("{:02}", p.min)),
            Some('p') => out.push_str(if p.h >= 12 { "PM" } else { "AM" }),
            Some('P') => out.push_str(if p.h >= 12 { "pm" } else { "am" }),
            Some('R') => out.push_str(&alloc::format!("{:02}:{:02}", p.h, p.min)),
            Some('s') => {
                let secs = (p.ijd - 210_866_760_000_000) / 1000;
                out.push_str(&alloc::format!("{}", secs));
            }
            Some('S') => out.push_str(&alloc::format!("{:02}", p.s as i32)),
            Some('T') => out.push_str(&alloc::format!("{:02}:{:02}:{:02}", p.h, p.min, p.s as i32)),
            Some('u') => {
                let mut wd = ((p.ijd + 129_600_000) / 86_400_000 % 7) as i32; // 0=Sun
                if wd == 0 {
                    wd = 7;
                }
                out.push_str(&alloc::format!("{}", wd));
            }
            Some('w') => {
                let wd = (p.ijd + 129_600_000) / 86_400_000 % 7; // 0=Sun
                out.push_str(&alloc::format!("{}", wd));
            }
            Some('W') => {
                let mut y0 = *p;
                y0.valid_jd = false;
                y0.m = 1;
                y0.d = 1;
                y0.h = 0;
                y0.min = 0;
                y0.s = 0.0;
                y0.valid_ymd = true;
                y0.valid_hms = true;
                y0.compute_jd();
                let nday = (p.ijd - y0.ijd + 43_200_000) / 86_400_000;
                let wd = (p.ijd + 129_600_000) / 86_400_000 % 7;
                let wn = (nday + 7 - (if wd != 0 { wd - 1 } else { 6 })) / 7;
                out.push_str(&alloc::format!("{:02}", wn));
            }
            Some('Y') => out.push_str(&alloc::format!("{:04}", p.y)),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

// ---- printf / format --------------------------------------------------------

/// SQLite's `printf`/`format`: a subset of C `printf` conversions sufficient for
/// the common cases (`%d %i %u %s %c %x %X %o %f %e %g %% %q %Q %w`), with width,
/// left-justify, zero-pad, `+`, space, and precision flags.
pub fn printf(args: &[Value]) -> Value {
    if args.is_empty() {
        return Value::Null;
    }
    let Value::Text(fmt) = &args[0] else {
        return Value::Null;
    };
    let mut out = String::new();
    let mut arg_idx = 1usize;
    let bytes: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c != '%' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        // flags
        let mut left = false;
        let mut zero = false;
        let mut plus = false;
        let mut space = false;
        let mut alt = false;
        loop {
            match bytes.get(i) {
                Some('-') => left = true,
                Some('0') => zero = true,
                Some('+') => plus = true,
                Some(' ') => space = true,
                Some('#') => alt = true,
                Some(',') | Some('!') => {} // accepted, ignored
                _ => break,
            }
            i += 1;
        }
        // width
        let mut width = 0usize;
        let mut has_width = false;
        while let Some(d) = bytes.get(i).filter(|c| c.is_ascii_digit()) {
            width = width * 10 + (*d as u8 - b'0') as usize;
            has_width = true;
            i += 1;
        }
        // precision
        let mut prec: Option<usize> = None;
        if bytes.get(i) == Some(&'.') {
            i += 1;
            let mut p = 0usize;
            while let Some(d) = bytes.get(i).filter(|c| c.is_ascii_digit()) {
                p = p * 10 + (*d as u8 - b'0') as usize;
                i += 1;
            }
            prec = Some(p);
        }
        let Some(&conv) = bytes.get(i) else { break };
        i += 1;
        let _ = has_width;
        let next = |idx: &mut usize| -> Value {
            let v = args.get(*idx).cloned().unwrap_or(Value::Null);
            *idx += 1;
            v
        };
        let body = match conv {
            'd' | 'i' => {
                let n = eval::to_i64(&next(&mut arg_idx));
                let mut s = if n < 0 {
                    alloc::format!("{}", n.unsigned_abs())
                } else {
                    alloc::format!("{n}")
                };
                if n >= 0 {
                    if plus {
                        s.insert(0, '+');
                    } else if space {
                        s.insert(0, ' ');
                    }
                } else {
                    s.insert(0, '-');
                }
                s
            }
            'u' => alloc::format!("{}", eval::to_i64(&next(&mut arg_idx)) as u64),
            'x' => alloc::format!("{:x}", eval::to_i64(&next(&mut arg_idx)) as u64),
            'X' => alloc::format!("{:X}", eval::to_i64(&next(&mut arg_idx)) as u64),
            'o' => alloc::format!("{:o}", eval::to_i64(&next(&mut arg_idx)) as u64),
            'c' => {
                // SQLite's %c emits the first character of the argument's text
                // (e.g. 104 -> "104" -> '1'), not the code point.
                let v = next(&mut arg_idx);
                eval::to_text(&v)
                    .chars()
                    .next()
                    .map(String::from)
                    .unwrap_or_default()
            }
            'f' => {
                let f = eval::to_f64(&next(&mut arg_idx));
                alloc::format!("{:.*}", prec.unwrap_or(6), f)
            }
            'e' => {
                let f = eval::to_f64(&next(&mut arg_idx));
                fmt_exp(f, prec.unwrap_or(6), false)
            }
            'E' => {
                let f = eval::to_f64(&next(&mut arg_idx));
                fmt_exp(f, prec.unwrap_or(6), true)
            }
            'g' | 'G' => {
                let f = eval::to_f64(&next(&mut arg_idx));
                eval::format_real(f)
            }
            's' => {
                let v = next(&mut arg_idx);
                let mut s = eval::to_text(&v);
                if let Some(pr) = prec {
                    s = s.chars().take(pr).collect();
                }
                s
            }
            'q' => {
                let v = next(&mut arg_idx);
                eval::to_text(&v).replace('\'', "''")
            }
            'Q' => {
                let v = next(&mut arg_idx);
                match v {
                    Value::Null => String::from("NULL"),
                    other => alloc::format!("'{}'", eval::to_text(&other).replace('\'', "''")),
                }
            }
            'w' => {
                let v = next(&mut arg_idx);
                eval::to_text(&v).replace('"', "\"\"")
            }
            _ => {
                // Unknown conversion: emit verbatim.
                out.push('%');
                out.push(conv);
                continue;
            }
        };
        let _ = alt;
        // apply width/justification
        let len = body.chars().count();
        if width > len {
            let pad = width - len;
            if left {
                out.push_str(&body);
                for _ in 0..pad {
                    out.push(' ');
                }
            } else if zero && matches!(conv, 'd' | 'i' | 'u' | 'x' | 'X' | 'o' | 'f' | 'e' | 'E') {
                // zero-pad after any sign.
                let (sign, rest) = match body.strip_prefix(['-', '+', ' ']) {
                    Some(r) => (&body[..1], r),
                    None => ("", body.as_str()),
                };
                out.push_str(sign);
                for _ in 0..pad {
                    out.push('0');
                }
                out.push_str(rest);
            } else {
                for _ in 0..pad {
                    out.push(' ');
                }
                out.push_str(&body);
            }
        } else {
            out.push_str(&body);
        }
    }
    Value::Text(out)
}

/// Format `%e` style: `d.dddde±dd`.
fn fmt_exp(f: f64, prec: usize, upper: bool) -> String {
    let s = alloc::format!("{:.*e}", prec, f);
    // Rust prints `1.5e2`; C/SQLite prints `1.5e+02`. Normalize the exponent.
    if let Some(pos) = s.find(['e', 'E']) {
        let (mantissa, exp) = s.split_at(pos);
        let exp = &exp[1..];
        let (sign, digits) = match exp.strip_prefix('-') {
            Some(d) => ('-', d),
            None => ('+', exp.strip_prefix('+').unwrap_or(exp)),
        };
        let e = if upper { 'E' } else { 'e' };
        alloc::format!("{mantissa}{e}{sign}{:0>2}", digits)
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn t(v: &str) -> Value {
        Value::Text(String::from(v))
    }

    #[test]
    fn basic_date() {
        assert_eq!(date(&[t("2000-01-01")]), t("2000-01-01"));
        assert_eq!(
            datetime(&[t("2000-01-01 12:34:56")]),
            t("2000-01-01 12:34:56")
        );
        assert_eq!(time(&[t("2000-01-01 12:34:56")]), t("12:34:56"));
    }

    #[test]
    fn modifiers() {
        assert_eq!(date(&[t("2000-01-01"), t("+1 day")]), t("2000-01-02"));
        assert_eq!(date(&[t("2000-01-31"), t("+1 month")]), t("2000-03-02"));
        assert_eq!(date(&[t("2000-01-01"), t("+1 year")]), t("2001-01-01"));
        assert_eq!(
            date(&[t("2000-01-15"), t("start of month")]),
            t("2000-01-01")
        );
    }

    #[test]
    fn unixepoch_modifier() {
        // 0 unix epoch = 1970-01-01.
        assert_eq!(
            datetime(&[Value::Integer(0), t("unixepoch")]),
            t("1970-01-01 00:00:00")
        );
        assert_eq!(unixepoch(&[t("1970-01-01 00:00:00")]), Value::Integer(0));
    }

    #[test]
    fn strftime_codes() {
        assert_eq!(strftime(&[t("%Y/%m/%d"), t("2000-01-02")]), t("2000/01/02"));
        assert_eq!(
            strftime(&[t("%H:%M:%S"), t("2000-01-02 03:04:05")]),
            t("03:04:05")
        );
    }

    #[test]
    fn printf_basic() {
        assert_eq!(
            printf(&[t("%d-%d"), Value::Integer(1), Value::Integer(2)]),
            t("1-2")
        );
        assert_eq!(printf(&[t("%05d"), Value::Integer(42)]), t("00042"));
        assert_eq!(printf(&[t("%.2f"), Value::Real(3.567)]), t("3.57"));
        assert_eq!(printf(&[t("%-5d|"), Value::Integer(7)]), t("7    |"));
        assert_eq!(printf(&[t("%x"), Value::Integer(255)]), t("ff"));
        assert_eq!(printf(&[t("%s and %s"), t("a"), t("b")]), t("a and b"));
    }

    #[cfg(feature = "std")]
    #[test]
    fn now_returns_current_date() {
        // With the std clock, date('now') is a valid YYYY-MM-DD in a sane range.
        let Value::Text(s) = date(&[t("now")]) else {
            panic!("date('now') should not be NULL with std");
        };
        assert_eq!(s.len(), 10, "got {s:?}");
        let year: i32 = s[..4].parse().unwrap();
        assert!(year >= 2024, "implausible year {year}");
        // No-arg form is equivalent.
        assert_eq!(date(&[]), date(&[t("now")]));
    }

    #[test]
    fn null_propagation() {
        assert_eq!(date(&[Value::Null]), Value::Null);
        assert_eq!(date(&[t("not a date")]), Value::Null);
        let _ = vec![1];
    }
}
