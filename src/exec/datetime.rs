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

/// The largest Julian-day value (in ms) SQLite will represent: the end of year
/// 9999. A computation that pushes `ijd` past this (or below 0) yields SQL NULL,
/// mirroring SQLite's `validJulianDay`.
const JD_MAX: i64 = 464_269_060_799_999;

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
    raw_s: bool,    // the value came in as a bare number (for `unixepoch`)
    subsec: bool,   // a `subsec`/`subsecond` modifier was applied (render ms)
    n_floor: i32,   // days the day-of-month overflowed (for the `floor` modifier)
    is_error: bool, // an unrepresentable computation occurred (permanently NULL)
}

impl DateTime {
    /// Port of `datetimeError`: mark the value permanently invalid. SQLite
    /// `memset`s the struct and sets `isError`; once set nothing clears it, so
    /// every render yields NULL even if a later modifier rebuilds an in-range
    /// `ijd` from the reset (year-0) fields. This is why an out-of-range input
    /// (a raw Julian-day number past year 9999, e.g. a Unix timestamp used
    /// without `unixepoch`) stays NULL through any modifier chain.
    fn datetime_error(&mut self) {
        *self = DateTime {
            is_error: true,
            ..DateTime::default()
        };
    }

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
        // SQLite (`computeJD`) flags an out-of-range year as an error, which
        // ultimately makes the value NULL; we mark `ijd` out of bounds so the
        // `validJulianDay` check in `is_date` rejects it. This also keeps the
        // intermediate products below from overflowing.
        if !(-4713..=9999).contains(&year) || self.raw_s {
            self.datetime_error();
            return;
        }
        if month <= 2 {
            year -= 1;
            month += 12;
        }
        // Widen to i64: the products stay well within range for the bounded year
        // above, but a transient pre-check year (before the modifier renormalizes
        // it) could otherwise overflow i32.
        let year = year as i64;
        let month = month as i64;
        let a = year / 100;
        let b = 2 - a + a / 4;
        let x1 = 36525 * (year + 4716) / 100;
        let x2 = 306001 * (month + 1) / 10000;
        self.ijd = (((x1 + x2 + day as i64 + b) as f64 - 1524.5) * 86_400_000.0) as i64;
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

    /// Port of `computeFloor`: from the current Y/M/D, count how many days the
    /// day-of-month overflows the month's length. Stored in `n_floor` so the
    /// `floor` modifier can roll the date back to the end of the month.
    fn compute_floor(&mut self) {
        let m = self.m;
        let d = self.d;
        self.n_floor = if d <= 28 {
            0
        } else if (1 << m) & 0x15aa != 0 {
            // Months with 31 days (and the low bits SQLite masks): no overflow.
            0
        } else if m != 2 {
            i32::from(d == 31)
        } else if self.y % 4 != 0 || (self.y % 100 == 0 && self.y % 400 != 0) {
            d - 28
        } else {
            d - 29
        };
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
        } else if !(0..=JD_MAX).contains(&self.ijd) {
            // A valid-but-out-of-range JD (e.g. arithmetic ran past year 9999)
            // is a permanent error, exactly as SQLite's `computeYMD` treats
            // `!validJulianDay(iJD)`.
            self.datetime_error();
            return;
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
        // Once the HMS has been derived from the JD, the value is no longer a
        // "raw number in the seconds field": clear `raw_s` so a later
        // `compute_jd` (after a modifier drops `valid_jd`) rebuilds from Y/M/D
        // instead of tripping the `raw_s` guard and invalidating to NULL. This
        // mirrors SQLite's `computeHMS`, which clears `p->rawS`.
        self.raw_s = false;
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
    // year: exactly 4 digits (SQLite `40f`); month/day exactly 2.
    let (year, ni) = read_exact(bytes, i, 4);
    let Some(year) = year else { return false };
    i = ni;
    if bytes.get(i) != Some(&b'-') {
        return false;
    }
    i += 1;
    let (month, ni) = read_exact(bytes, i, 2);
    let Some(month) = month else { return false };
    i = ni;
    if bytes.get(i) != Some(&b'-') {
        return false;
    }
    i += 1;
    let (day, ni) = read_exact(bytes, i, 2);
    let Some(day) = day else { return false };
    i = ni;
    // SQLite validates the month (1-12) and day (1-31); a day that overflows the
    // month (e.g. Feb 30) is accepted here and normalized later via the Julian-day
    // round-trip. Out-of-range components make the whole value NULL.
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return false;
    }
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
    p.compute_floor();
    true
}

/// Parse `HH:MM[:SS[.SSS]][tz]` into `p`.
fn parse_hh_mm_ss(z: &str, p: &mut DateTime) -> bool {
    let bytes = z.as_bytes();
    let mut i = 0;
    // SQLite `20c:20e`: hour 2 digits (0-24), minute 2 digits (0-59).
    let (h, ni) = read_exact(bytes, i, 2);
    let Some(h) = h else { return false };
    i = ni;
    if bytes.get(i) != Some(&b':') {
        return false;
    }
    i += 1;
    let (min, ni) = read_exact(bytes, i, 2);
    let Some(min) = min else { return false };
    i = ni;
    let mut sec = 0.0;
    if bytes.get(i) == Some(&b':') {
        i += 1;
        // `20e`: seconds 2 digits (0-59); the fractional `.FFF` is separate.
        let (s, ni) = read_exact(bytes, i, 2);
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
    // SQLite range-checks the clock fields: hour 0-24, minute 0-59, second in
    // [0, 60). Out-of-range makes the whole value NULL.
    if !(0..=24).contains(&h) || !(0..=59).contains(&min) || !(0.0..60.0).contains(&sec) {
        return false;
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
    // SQLite `20b:20e`: tz hour 2 digits (0-14), tz minute 2 digits (0-59).
    let (th, ni) = read_exact(bytes, i, 2);
    let Some(th) = th.filter(|h| (0..=14).contains(h)) else {
        return false;
    };
    i = ni;
    if bytes.get(i) != Some(&b':') {
        return false;
    }
    i += 1;
    let (tm, ni) = read_exact(bytes, i, 2);
    let Some(tm) = tm.filter(|m| (0..=59).contains(m)) else {
        return false;
    };
    i = ni;
    p.tz = sign * (th * 60 + tm);
    p.valid_tz = true;
    i == bytes.len()
}

/// Read **exactly** `len` ASCII digits as an int (port of SQLite's `getDigits`,
/// whose `40f`/`21a`/… specs read a fixed digit count). Returns the value and the
/// new index, or `None` if fewer than `len` digits are present. The `min`/`max`
/// bounds are checked by the caller.
fn read_exact(bytes: &[u8], start: usize, len: usize) -> (Option<i32>, usize) {
    let mut val: i32 = 0;
    for k in 0..len {
        match bytes.get(start + k) {
            Some(b) if b.is_ascii_digit() => val = val * 10 + (b - b'0') as i32,
            _ => return (None, start),
        }
    }
    (Some(val), start + len)
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
            // SQLite passes the raw text to the parsers (no trim): a date/time
            // shape with leading whitespace is rejected, while the numeric
            // fallback (`parse_float`) tolerates surrounding whitespace itself.
            let z = s.as_str();
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
/// `idx` is the 1-based position of the modifier in the argument list (the first
/// modifier is 1), used by SQLite to forbid `auto`/`julianday`/`unixepoch`
/// anywhere but the very first modifier. Returns `false` if the modifier is
/// unrecognized/invalid.
fn apply_modifier(p: &mut DateTime, m: &str, idx: usize) -> bool {
    // SQLite does NOT trim modifiers: it switches on the first byte and matches
    // the keyword exactly (`sqlite3_stricmp`), so any leading or trailing
    // whitespace makes the whole modifier — and the date — invalid. Only the
    // numeric `±N unit` and `weekday N` forms tolerate *internal* whitespace, and
    // they handle it themselves below.
    let lower = m.to_ascii_lowercase();
    match lower.as_str() {
        // Timezone modifiers: no tz database, so the conversion is a value no-op.
        // SQLite nonetheless recomputes the Julian day and clears the Y/M/D +
        // clock fields (`computeJD` then `clearYMD_HMS_TZ`, or the `utc` guess
        // loop's `memset`), which normalizes an out-of-range parsed field:
        // `2024-02-30` -> `2024-03-01`, `2024-06-15 24:00:00` -> the next day. A
        // bare value with no modifier keeps such fields verbatim, but a `utc`/
        // `localtime` modifier forces the normalization — so match that here.
        "utc" | "localtime" => {
            p.compute_jd();
            p.clear_ymd_hms_tz();
            true
        }
        "unixepoch" => {
            // Only valid as the first modifier, and only on a raw number.
            if idx > 1 || !p.raw_s {
                return false;
            }
            let r = p.s * 1000.0 + 210_866_760_000_000.0;
            if (0.0..464_269_060_800_000.0).contains(&r) {
                p.clear_ymd_hms_tz();
                p.ijd = (r + 0.5) as i64;
                p.valid_jd = true;
                p.raw_s = false;
                return true;
            }
            false
        }
        "julianday" => {
            // Force the raw number to be interpreted as a Julian day (default);
            // only valid as the first modifier.
            if idx > 1 {
                return false;
            }
            if p.raw_s {
                p.raw_s = false;
            }
            true
        }
        "auto" => {
            // Only valid as the first modifier.
            if idx > 1 {
                return false;
            }
            if p.raw_s {
                // < 5373484.5 days => already a JD; otherwise a unix timestamp.
                if p.s >= 0.0 && p.s < 5_373_484.5 {
                    p.raw_s = false;
                } else {
                    return apply_modifier(p, "unixepoch", idx);
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
            // `compute_ymd_hms` (not just `compute_ymd`) so `raw_s` is cleared
            // for a bare-number input, matching SQLite's `computeYMD_HMS` here.
            p.compute_ymd_hms();
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
            p.compute_ymd_hms();
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
        "ceiling" => {
            // Day-of-month overflow rolls forward (the default); a near no-op
            // that just finalizes the JD and clears the floor counter.
            p.compute_jd();
            p.clear_ymd_hms_tz();
            p.n_floor = 0;
            true
        }
        "floor" => {
            // Day-of-month overflow rolls *back* to the end of the month: undo
            // the `n_floor` days that the most recent parse / month-add overflowed.
            p.compute_jd();
            p.ijd -= p.n_floor as i64 * 86_400_000;
            p.clear_ymd_hms_tz();
            true
        }
        "subsec" | "subsecond" => {
            // Make `datetime()`/`time()` render milliseconds; a no-op for `date()`.
            p.subsec = true;
            true
        }
        _ => apply_numeric_modifier(p, m, &lower),
    }
}

/// Handle `±N units`, `weekday N`, and `±HH:MM[:SS]` modifiers.
fn apply_numeric_modifier(p: &mut DateTime, orig: &str, lower: &str) -> bool {
    // `weekday N`: exactly the prefix `"weekday "` (one space), then a number in
    // [0,7) that is integer-valued. SQLite's `sqlite3AtoF` tolerates a leading
    // `+`/whitespace and trailing whitespace, and accepts `3.0` (== 3) but not
    // `3.5`.
    if let Some(rest) = lower.strip_prefix("weekday ") {
        let Some(r) = parse_float(rest) else {
            return false;
        };
        if !(0.0..7.0).contains(&r) || r != float::trunc(r) {
            return false;
        }
        let n = r as i64;
        p.compute_ymd_hms();
        p.valid_jd = false;
        p.compute_jd();
        let mut z = (p.ijd + 129_600_000) / 86_400_000 % 7; // 0 = Sunday
        if z > n {
            z -= 7;
        }
        p.ijd += (n - z) * 86_400_000;
        p.clear_ymd_hms_tz();
        return true;
    }

    // The `(+|-)YYYY-MM-DD[ HH:MM]` calendar-offset form. Only attempted when the
    // text starts with a sign and has the right shape; falls through to the
    // `±N unit` / `±HH:MM` forms otherwise.
    if (orig.starts_with('+') || orig.starts_with('-')) && apply_ymd_offset(p, orig) {
        return true;
    }

    // Parse a leading signed number, stopping at the first space or colon (so an
    // embedded space — `+1 day` — splits number from unit, while `+1day` does
    // not and is rejected as a malformed unit).
    let bytes = orig.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
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
    let Some(r) = parse_float(&orig[..i]) else {
        return false;
    };
    // The unit name follows after optional internal spaces; SQLite skips the
    // whitespace, then requires the *rest* to be exactly the unit name (no
    // trailing whitespace) of length 3..=10 with an optional plural `s`.
    let unit_field = &orig[i..];
    let unit_trimmed = unit_field.trim_start();
    if unit_trimmed.len() == unit_field.len() && !unit_field.is_empty() {
        // No separating space at all (`+1day`): SQLite rejects this.
        return false;
    }
    let unit = unit_trimmed.to_ascii_lowercase();
    let rounder = if r < 0.0 { -0.5 } else { 0.5 };
    // SQLite rejects an out-of-range magnitude (`r > -rLimit && r < rLimit`)
    // before applying the transform, so e.g. `+1e10 days` is NULL rather than an
    // overflow. The limits mirror `aXformType[].rLimit` in `date.c`.
    let limit = match unit.as_str() {
        "second" | "seconds" => 4.6427e14_f64,
        "minute" | "minutes" => 7.7379e12,
        "hour" | "hours" => 1.2897e11,
        "day" | "days" => 5_373_485.0,
        "month" | "months" => 176_546.0,
        "year" | "years" => 14713.0,
        _ => return false,
    };
    if !(-limit < r && r < limit) {
        return false;
    }
    p.n_floor = 0;
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
            p.compute_floor();
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
            p.compute_floor();
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

/// Apply the `(+|-)YYYY-MM-DD[ HH:MM]` calendar-offset modifier (port of the
/// `z[n]=='-'` branch of SQLite's numeric `parseModifier`). Adds (or subtracts)
/// whole years, months (0-11) and days (0-30), with the optional ` HH:MM` tail
/// adding a time-of-day shift. Returns `false` if `orig` is not this shape.
fn apply_ymd_offset(p: &mut DateTime, orig: &str) -> bool {
    let bytes = orig.as_bytes();
    let sign = bytes[0];
    // Find the extent of the leading number, stopping at `:` or whitespace, and
    // detecting the embedded `-` separators that mark the YMD form.
    let mut n = 1;
    let mut dash_at = None;
    while n < bytes.len() {
        let c = bytes[n];
        if c == b':' || c == b' ' {
            break;
        }
        if c == b'-' {
            // `40f-21a-21d`: a `-` after a 4- or 5-digit year marks the YMD form.
            if (n == 5 || n == 6) && dash_at.is_none() {
                dash_at = Some(n);
            }
        }
        n += 1;
    }
    // Width of the year field: 4 (`+YYYY-`) or 5 (`+YYYYY-`); anything else is
    // not the calendar-offset form.
    let Some(dash) = dash_at else { return false };
    let year_w = dash - 1;
    if year_w != 4 && year_w != 5 {
        return false;
    }
    // Parse `YYYY-MM-DD` (each of MM/DD exactly two digits).
    let head = &orig[1..n];
    let parts: Vec<&str> = head.split('-').collect();
    if parts.len() != 3 || parts[1].len() != 2 || parts[2].len() != 2 {
        return false;
    }
    let (Ok(yy), Ok(mm), Ok(dd)) = (
        parts[0].parse::<i32>(),
        parts[1].parse::<i32>(),
        parts[2].parse::<i32>(),
    ) else {
        return false;
    };
    if mm >= 12 || dd >= 31 {
        return false;
    }
    p.compute_ymd_hms();
    p.valid_jd = false;
    let d_shift = if sign == b'-' {
        p.y -= yy;
        p.m -= mm;
        -dd
    } else {
        p.y += yy;
        p.m += mm;
        dd
    };
    let x = if p.m > 0 {
        (p.m - 1) / 12
    } else {
        (p.m - 12) / 12
    };
    p.y += x;
    p.m -= x * 12;
    p.compute_floor();
    p.compute_jd();
    p.valid_hms = false;
    p.valid_ymd = false;
    p.ijd += d_shift as i64 * 86_400_000;
    // Optional ` HH:MM` time-of-day tail.
    if n >= bytes.len() {
        return true;
    }
    if bytes[n] != b' ' {
        return false;
    }
    let mut tx = DateTime::default();
    if !parse_hh_mm_ss(&orig[n + 1..], &mut tx) {
        return false;
    }
    // Reuse the `±HH:MM` shift, with the sign of the whole modifier.
    let ms = tx.h as i64 * 3_600_000 + tx.min as i64 * 60_000 + (tx.s * 1000.0 + 0.5) as i64;
    p.compute_jd();
    if sign == b'-' {
        p.ijd -= ms;
    } else {
        p.ijd += ms;
    }
    p.clear_ymd_hms_tz();
    true
}

/// Apply a `±HH:MM[:SS]` time-shift modifier.
fn apply_time_shift(p: &mut DateTime, orig: &str) -> bool {
    let bytes = orig.as_bytes();
    // The sign is optional: SQLite's `parseModifier` reaches this branch for
    // `[+-]HH:MM[:SS[.FFF]]`, and a bare `HH:MM` (no sign) adds the time-of-day.
    let (sign, rest) = match bytes.first() {
        Some(b'+') => (1_i64, &orig[1..]),
        Some(b'-') => (-1_i64, &orig[1..]),
        _ => (1_i64, orig),
    };
    let mut tmp = DateTime::default();
    if !parse_hh_mm_ss(rest, &mut tmp) {
        return false;
    }
    // SQLite drops the whole-day part of the parsed time before applying it (it
    // extracts the time-of-day from a Julian day), so `24:00` adds nothing and
    // `24:30` adds 30 minutes.
    let ms = (tmp.h as i64 * 3_600_000 + tmp.min as i64 * 60_000 + (tmp.s * 1000.0 + 0.5) as i64)
        % 86_400_000;
    p.compute_jd();
    p.ijd += sign * ms;
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
    for (idx, m) in rest.iter().enumerate() {
        let Value::Text(ms) = m else { return None };
        // `idx + 1` is the 1-based modifier position, matching SQLite's argv index
        // (the first modifier is 1), which `auto`/`julianday`/`unixepoch` require.
        if !apply_modifier(&mut p, ms, idx + 1) {
            return None;
        }
    }
    p.compute_jd();
    // SQLite's date functions return NULL once a computation runs outside the
    // representable Julian-day window (year 0..=9999). This is a check on the
    // Julian day itself, not on Y/M/D: `datetime('9999-12-31 24:00:00')` is NULL
    // even though its stored day is 31, because the +24h pushes the JD past the
    // ceiling. Mirrors `validJulianDay` in SQLite's `date.c`. `is_error` is the
    // sticky `datetimeError` flag — a modifier chain may have rebuilt an in-range
    // `ijd` from the reset fields, but the value stays NULL.
    if p.is_error || !(0..=JD_MAX).contains(&p.ijd) {
        return None;
    }
    // SQLite only re-derives Y/M/D from the Julian day in the no-modifier case
    // when the parsed day overflows its month (`D > 28`), e.g.
    // `date('2024-02-30')` -> `2024-03-01`. A modifier path leaves YMD invalid
    // (the modifier clears it), so `compute_ymd` rebuilds it from the JD then.
    // Crucially, a valid in-range day that merely carries a `24:00:00` clock
    // (`datetime('2000-01-01 24:00:00')`) is *not* rolled into the next day — the
    // Y/M/D and the clock fields are both printed exactly as parsed.
    if args.len() == 1 && p.valid_ymd && p.d > 28 {
        p.valid_ymd = false;
    }
    p.compute_ymd();
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
    if p.subsec {
        // `subsec`/`subsecond`: render seconds as `SS.SSS` (e.g. `45.000`).
        alloc::format!("{:02}:{:02}:{:06.3}", p.h, p.min, p.s)
    } else {
        alloc::format!("{:02}:{:02}:{:02}", p.h, p.min, p.s as i32)
    }
}

/// `date(...)` -> `YYYY-MM-DD`.
pub fn date(args: &[Value]) -> Value {
    match is_date(args) {
        Some(mut p) => Value::Text(fmt_date(&mut p).into()),
        None => Value::Null,
    }
}

/// `time(...)` -> `HH:MM:SS`.
pub fn time(args: &[Value]) -> Value {
    match is_date(args) {
        Some(mut p) => Value::Text(fmt_time(&mut p).into()),
        None => Value::Null,
    }
}

/// `datetime(...)` -> `YYYY-MM-DD HH:MM:SS`.
pub fn datetime(args: &[Value]) -> Value {
    match is_date(args) {
        Some(mut p) => {
            Value::Text(alloc::format!("{} {}", fmt_date(&mut p), fmt_time(&mut p)).into())
        }
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

/// `unixepoch(...)` -> seconds since 1970. Integer by default; with a
/// `subsec`/`subsecond` modifier, a real carrying the fractional (millisecond)
/// part, matching SQLite.
pub fn unixepoch(args: &[Value]) -> Value {
    match is_date(args) {
        Some(p) => {
            let ms = p.ijd - 210_866_760_000_000;
            if p.subsec {
                Value::Real(ms as f64 / 1000.0)
            } else {
                Value::Integer(ms / 1000)
            }
        }
        None => Value::Null,
    }
}

/// `timediff(A, B)` -> the calendar time difference from B to A, formatted as
/// `(+|-)YYYY-MM-DD HH:MM:SS.SSS` (the amount of time to add to B to reach A).
///
/// Faithful port of SQLite's `timediffFunc`: parse both operands, choose the
/// sign by ordering them, then carry the calendar breakdown month-by-month so
/// that uneven month lengths and leap years come out exactly as upstream.
pub fn timediff(a: &Value, b: &Value) -> Value {
    // Both operands must parse as a single date/time value (no modifiers).
    let Some(mut d1) = parse_value(a) else {
        return Value::Null;
    };
    let Some(mut d2) = parse_value(b) else {
        return Value::Null;
    };
    // Like `isDate`, finish both operands to a valid Julian day before reading.
    d1.compute_jd();
    d2.compute_jd();
    d1.compute_ymd_hms();
    d2.compute_ymd_hms();

    // iJD of 0000-01-01 00:00:00, used to bias the residual so re-deriving Y/M/D
    // yields the day count (+1) and the clock fields directly.
    const BIAS: i64 = 148_699_540_800_000;
    let (sign, mut y, mut m);
    if d1.ijd >= d2.ijd {
        // d1 >= d2: carry d2 up toward d1 by whole years then whole months.
        sign = '+';
        y = d1.y - d2.y;
        if y != 0 {
            d2.y = d1.y;
            d2.valid_jd = false;
            d2.compute_jd();
        }
        m = d1.m - d2.m;
        if m < 0 {
            y -= 1;
            m += 12;
        }
        if m != 0 {
            d2.m = d1.m;
            d2.valid_jd = false;
            d2.compute_jd();
        }
        // Back off a month at a time until d2 no longer overshoots d1.
        while d1.ijd < d2.ijd {
            m -= 1;
            if m < 0 {
                m = 11;
                y -= 1;
            }
            d2.m -= 1;
            if d2.m < 1 {
                d2.m = 12;
                d2.y -= 1;
            }
            d2.valid_jd = false;
            d2.compute_jd();
        }
        d1.ijd -= d2.ijd;
        d1.ijd += BIAS;
    } else {
        // d1 < d2: pull d2 down toward d1 by whole years then whole months.
        sign = '-';
        y = d2.y - d1.y;
        if y != 0 {
            d2.y = d1.y;
            d2.valid_jd = false;
            d2.compute_jd();
        }
        m = d2.m - d1.m;
        if m < 0 {
            y -= 1;
            m += 12;
        }
        if m != 0 {
            d2.m = d1.m;
            d2.valid_jd = false;
            d2.compute_jd();
        }
        // Step d2 forward a month at a time until it no longer undershoots d1.
        while d1.ijd > d2.ijd {
            m -= 1;
            if m < 0 {
                m = 11;
                y -= 1;
            }
            d2.m += 1;
            if d2.m > 12 {
                d2.m = 1;
                d2.y += 1;
            }
            d2.valid_jd = false;
            d2.compute_jd();
        }
        d1.ijd = d2.ijd - d1.ijd;
        d1.ijd += BIAS;
    }

    d1.valid_ymd = false;
    d1.valid_hms = false;
    d1.valid_tz = false;
    d1.compute_ymd_hms();

    Value::Text(
        alloc::format!(
            "{}{:04}-{:02}-{:02} {:02}:{:02}:{:06.3}",
            sign,
            y,
            m,
            d1.d - 1,
            d1.h,
            d1.min,
            d1.s
        )
        .into(),
    )
}

/// `strftime(format, timevalue, modifier, ...)`.
pub fn strftime(args: &[Value]) -> Value {
    // Only the format is required: `strftime('%Y')` defaults the time-value to
    // 'now', exactly like `date()`/`time()`/`datetime()`. `is_date(&[])` (the
    // empty modifier slice when a single arg is given) supplies the "now" default.
    let Some(fmt) = args.first() else {
        return Value::Null;
    };
    // SQLite coerces a non-text format to text (`strftime(123)` -> "123",
    // `strftime(x'41')` -> "A"); only a NULL format yields NULL.
    if matches!(fmt, Value::Null) {
        return Value::Null;
    }
    let fmt = eval::to_text(fmt);
    let Some(mut p) = is_date(&args[1..]) else {
        return Value::Null;
    };
    match render_strftime(&fmt, &mut p) {
        Some(s) => Value::Text(s.into()),
        None => Value::Null,
    }
}

/// Render an `strftime` format. Returns `None` (SQL NULL) if the format contains
/// any specifier SQLite does not recognize — SQLite aborts the whole conversion
/// rather than emitting the unknown `%X` literally.
fn render_strftime(fmt: &str, p: &mut DateTime) -> Option<String> {
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
            Some('j') => out.push_str(&alloc::format!("{:03}", day_of_year(p))),
            // `%J` renders the Julian day with 16 significant digits (`%.16g`),
            // higher precision than julianday()'s default real formatting — a
            // SQLite quirk (e.g. `2460477.024259259`, but `2460477` at noon).
            Some('J') => out.push_str(&crate::util::fpdecode::format(
                p.ijd as f64 / 86_400_000.0,
                16,
                crate::util::fpdecode::XType::Generic,
                false,
                false,
            )),
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
                let total_ms = p.ijd - 210_866_760_000_000;
                let secs = total_ms / 1000;
                if p.subsec {
                    // The `subsec`/`subsecond` modifier renders %s with millisecond
                    // precision (`<secs>.mmm`), as SQLite does.
                    out.push_str(&alloc::format!("{}.{:03}", secs, (total_ms % 1000).abs()));
                } else {
                    out.push_str(&alloc::format!("{}", secs));
                }
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
            Some('U') => {
                // Week of year, Sunday as the first day (00-53): weeks before the
                // first Sunday are week 00. `(yday0 + 7 - wday_sun0) / 7`.
                let yday0 = day_of_year(p) - 1;
                let wday = (p.ijd + 129_600_000) / 86_400_000 % 7; // 0 = Sunday
                out.push_str(&alloc::format!("{:02}", (yday0 + 7 - wday) / 7));
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
            Some('G') => {
                let (iy, _) = iso_week_date(p);
                out.push_str(&alloc::format!("{iy:04}"));
            }
            Some('g') => {
                let (iy, _) = iso_week_date(p);
                out.push_str(&alloc::format!("{:02}", iy.rem_euclid(100)));
            }
            Some('V') => {
                let (_, iw) = iso_week_date(p);
                out.push_str(&alloc::format!("{iw:02}"));
            }
            Some('%') => out.push('%'),
            // An unrecognized specifier (or a trailing `%`) aborts the whole
            // conversion to NULL, matching SQLite.
            Some(_) | None => return None,
        }
    }
    Some(out)
}

/// Day of year (1-366) for `p`, normalizing to midnight.
fn day_of_year(p: &DateTime) -> i64 {
    let midnight = |m: i32, d: i32| {
        let mut x = *p;
        x.valid_jd = false;
        x.m = m;
        x.d = d;
        x.h = 0;
        x.min = 0;
        x.s = 0.0;
        x.valid_ymd = true;
        x.valid_hms = true;
        x.compute_jd();
        x.ijd
    };
    ((midnight(p.m, p.d) - midnight(1, 1) + 43_200_000) / 86_400_000) + 1
}

/// ISO weekday of `p`: Monday = 1 … Sunday = 7.
fn iso_weekday(p: &DateTime) -> i32 {
    let wd = ((p.ijd + 129_600_000) / 86_400_000 % 7) as i32; // 0 = Sunday
    if wd == 0 { 7 } else { wd }
}

/// ISO 8601 week-date `(year, week)` for `p`. Week 1 is the week (Mon–Sun)
/// containing the year's first Thursday; days before it belong to the last week
/// of the previous ISO year, and late-December days can belong to week 1 of the
/// next ISO year.
fn iso_week_date(p: &DateTime) -> (i32, i32) {
    // Whether ISO year `y` has 53 weeks (the standard "long year" predicate).
    let long_year = |y: i64| {
        let f =
            |y: i64| (y + y.div_euclid(4) - y.div_euclid(100) + y.div_euclid(400)).rem_euclid(7);
        f(y) == 4 || f(y - 1) == 3
    };
    let doy = day_of_year(p);
    let dow = iso_weekday(p) as i64;
    let week = (doy - dow + 10) / 7;
    let y = p.y as i64;
    let (iy, iw) = if week < 1 {
        (y - 1, if long_year(y - 1) { 53 } else { 52 })
    } else if week > if long_year(y) { 53 } else { 52 } {
        (y + 1, 1)
    } else {
        (y, week)
    };
    (iy as i32, iw as i32)
}

// ---- printf / format --------------------------------------------------------

/// SQLite's total-output ceiling (`SQLITE_MAX_LENGTH`, default 1e9). A field
/// whose width or precision would push the result to or past this returns the
/// empty string, exactly as SQLite's `sqlite3_str` does on `SQLITE_TOOBIG`.
const PRINTF_LIMIT: usize = 1_000_000_000;

/// SQLite caps the fractional precision of the floating-point conversions
/// (`%f`/`%e`/`%g`) at 1e8, independent of the 1e9 total-output ceiling: a
/// `%.999999999f` of `1.0` renders `"1."` + 1e8 zeros (length 100000002), not
/// ~1e9 digits. Clamping to the same value keeps graphite byte-identical *and*
/// stops it from trying to materialize a billion digits (which hangs).
const PRINTF_FLOAT_PRECISION_LIMIT: usize = 100_000_000;

/// Insert `,` thousands separators into the integer part of a formatted number,
/// preserving a leading sign (`+`/`-`/space) and any fractional `.xxx` tail.
fn group_thousands(s: &str) -> String {
    let (sign, rest) = match s.strip_prefix(['+', '-', ' ']) {
        Some(r) => (&s[..1], r),
        None => ("", s),
    };
    let dot = rest.find('.').unwrap_or(rest.len());
    let (int_part, frac) = rest.split_at(dot);
    let n = int_part.len();
    let mut grouped = String::with_capacity(n + n / 3);
    for (idx, ch) in int_part.chars().enumerate() {
        if idx > 0 && (n - idx) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    alloc::format!("{sign}{grouped}{frac}")
}

/// Count the characters in `bytes` with SQLite's lenient `SKIP_UTF8` stepping
/// (each byte that is not a UTF-8 continuation byte starts a character). For valid
/// UTF-8 / ASCII this equals `str::chars().count()`; for a non-UTF-8 `%s` argument
/// it counts leniently instead of going through a lossy decode.
fn printf_char_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| (b & 0xc0) != 0x80).count()
}

/// Truncate `bytes` to at most `n` characters (lenient `SKIP_UTF8` units), for the
/// precision of a `%s`/`%z` conversion over possibly-non-UTF-8 text.
fn printf_truncate_chars(bytes: &[u8], n: usize) -> alloc::vec::Vec<u8> {
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() && count < n {
        i += 1;
        while i < bytes.len() && (bytes[i] & 0xc0) == 0x80 {
            i += 1;
        }
        count += 1;
    }
    bytes[..i].to_vec()
}

/// Apply an optional character-precision to `bytes` (for the `%q`/`%Q`/`%w`
/// conversions, whose precision limits the number of *input* characters consumed
/// before escaping).
fn printf_prec_bytes(bytes: &[u8], prec: Option<usize>) -> alloc::vec::Vec<u8> {
    match prec {
        Some(p) => printf_truncate_chars(bytes, p),
        None => bytes.to_vec(),
    }
}

/// SQL-escape `bytes` by doubling every occurrence of `q` (SQLite's byte-oriented
/// `et_SQLESCAPE` — `%q`/`%Q` double `'`, `%w` doubles `"`). Operating on raw
/// bytes keeps a non-UTF-8 argument exact.
fn printf_escape_byte(bytes: &[u8], q: u8) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(bytes.len());
    for &b in bytes {
        if b == q {
            out.push(q);
        }
        out.push(b);
    }
    out
}

/// SQLite's `printf`/`format`, mirroring `sqlite3_str_vappendf`: the C
/// conversions `%d %i %u %f %e %E %g %G %x %X %o %s %c %%` plus SQLite's
/// extensions `%q %Q %w %z`, the flags `- + space 0 # ,` and `!`, numeric or
/// `*` width and precision (including integer minimum-digit precision and the
/// `#` alternate form).
///
/// The output is accumulated as bytes so a `%s`/`%z` argument whose text is not
/// valid UTF-8 is emitted verbatim rather than through a lossy decode; every
/// other conversion produces ASCII, so the valid-UTF-8 output is byte-identical.
pub fn printf(args: &[Value]) -> Value {
    if args.is_empty() {
        return Value::Null;
    }
    let Value::Text(fmt) = &args[0] else {
        return Value::Null;
    };
    let mut out: Vec<u8> = Vec::new();
    let mut arg_idx = 1usize;
    let bytes: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    // Set on any width/precision overflow; SQLite returns the empty string.
    let mut too_big = false;
    let mut cbuf = [0u8; 4];
    while i < bytes.len() {
        let c = bytes[i];
        if c != '%' {
            out.extend_from_slice(c.encode_utf8(&mut cbuf).as_bytes());
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            // A `%` that is the final character of the format string — with no
            // conversion specifier following — is emitted literally by SQLite
            // (`printf('%')` → "%", `printf('abc%')` → "abc%"). A `%` followed
            // by flags/width that then runs off the end (`%5`, `%-`) still
            // produces nothing, handled by the conversion path below.
            out.push(b'%');
            break;
        }
        if bytes[i] == '%' {
            out.push(b'%');
            i += 1;
            continue;
        }
        // flags
        let mut left = false;
        let mut zero = false;
        let mut plus = false;
        let mut space = false;
        let mut alt = false;
        let mut comma = false;
        let mut bang = false;
        loop {
            match bytes.get(i) {
                Some('-') => left = true,
                Some('0') => zero = true,
                Some('+') => plus = true,
                Some(' ') => space = true,
                Some('#') => alt = true,
                Some(',') => comma = true, // thousands grouping (SQLite extension)
                Some('!') => bang = true,  // alt-form-2: trim trailing zeros
                _ => break,
            }
            i += 1;
        }
        // width — a `*` takes it from the next argument (a negative value means
        // left-justify). A `,` here (after width) is not a flag and makes the
        // whole conversion fail in SQLite; we honour that below.
        let mut width = 0usize;
        if bytes.get(i) == Some(&'*') {
            i += 1;
            let wv = eval::to_int_value(&args.get(arg_idx).cloned().unwrap_or(Value::Null));
            arg_idx += 1;
            if wv < 0 {
                left = true;
                width = wv.unsigned_abs() as usize;
            } else {
                width = wv as usize;
            }
        } else {
            while let Some(d) = bytes.get(i).filter(|c| c.is_ascii_digit()) {
                width = width
                    .saturating_mul(10)
                    .saturating_add((*d as u8 - b'0') as usize);
                i += 1;
            }
        }
        // A comma following the width (e.g. `%12,d`) is rejected by SQLite — the
        // comma is only valid in the flags position, never after the width — so
        // the whole result becomes the empty string.
        if bytes.get(i) == Some(&',') {
            too_big = true; // reuse the "fail -> empty string" path
            break;
        }
        // precision — likewise `.*` takes it from the next argument.
        let mut prec: Option<usize> = None;
        if bytes.get(i) == Some(&'.') {
            i += 1;
            if bytes.get(i) == Some(&'*') {
                i += 1;
                let pv = eval::to_int_value(&args.get(arg_idx).cloned().unwrap_or(Value::Null));
                arg_idx += 1;
                // SQLite uses the *magnitude* of a `.*` precision — a negative
                // argument behaves like its absolute value (`%.*f` with -2 is
                // `%.2f`), not like an omitted precision (C) nor a clamp to 0.
                prec = Some(pv.unsigned_abs() as usize);
            } else {
                let mut p = 0usize;
                while let Some(d) = bytes.get(i).filter(|c| c.is_ascii_digit()) {
                    p = p
                        .saturating_mul(10)
                        .saturating_add((*d as u8 - b'0') as usize);
                    i += 1;
                }
                prec = Some(p);
            }
        }
        // The `l`/`ll` length modifiers are accepted and ignored (graphite
        // formats integers as i64 regardless), matching SQLite, which supports
        // `%ld`/`%lld` but not `%hd`.
        while bytes.get(i) == Some(&'l') {
            i += 1;
        }
        let Some(&conv) = bytes.get(i) else { break };
        i += 1;
        let next = |idx: &mut usize| -> Value {
            let v = args.get(*idx).cloned().unwrap_or(Value::Null);
            *idx += 1;
            v
        };
        // Bound the field: width/precision past SQLITE_MAX_LENGTH yield "".
        if width >= PRINTF_LIMIT || prec.is_some_and(|p| p >= PRINTF_LIMIT) {
            too_big = true;
            break;
        }
        // SQLite clamps the fractional precision of the float conversions at 1e8
        // (see `PRINTF_FLOAT_PRECISION_LIMIT`); a larger precision produces the
        // same bytes as 1e8 rather than ~1e9 digits. Only the float arms are
        // affected — integer/string precision keeps its meaning.
        let prec = match conv {
            'f' | 'e' | 'E' | 'g' | 'G' => prec.map(|p| p.min(PRINTF_FLOAT_PRECISION_LIMIT)),
            _ => prec,
        };
        // Whether this conversion zero-pads on the left of the numeric digits.
        let numeric = matches!(
            conv,
            'd' | 'i' | 'u' | 'x' | 'X' | 'o' | 'f' | 'e' | 'E' | 'g' | 'G'
        );
        // The string conversions emit the argument's raw text bytes (verbatim for
        // a non-UTF-8 value): %s/%z as-is, %q/%Q/%w SQL-escaped by doubling the
        // relevant quote byte over the raw bytes (SQLite's byte-oriented
        // et_SQLESCAPE), %c the first *character*'s bytes repeated. Every other
        // conversion produces ASCII, formatted as a `String` and turned to bytes.
        let body: Vec<u8> = if matches!(conv, 's' | 'z' | 'q' | 'Q' | 'w' | 'c') {
            let v = next(&mut arg_idx);
            match conv {
                's' | 'z' => {
                    let b = crate::exec::eval::text_bytes(&v);
                    match prec {
                        Some(pr) => printf_truncate_chars(&b, pr),
                        None => b,
                    }
                }
                'c' => {
                    // SQLite's %c emits the first *character* of the argument's text
                    // (e.g. 104 -> "104" -> '1'; for `'é'` the two bytes `C3 A9`),
                    // repeated `precision` times (min one); an empty/NULL argument
                    // yields a NUL byte. Take the first lenient UTF-8 unit's raw
                    // bytes so a multi-byte leading character is emitted whole.
                    let b = if matches!(v, Value::Null) {
                        alloc::vec::Vec::new()
                    } else {
                        crate::exec::eval::text_bytes(&v)
                    };
                    let first = if b.is_empty() {
                        alloc::vec![0u8]
                    } else {
                        printf_truncate_chars(&b, 1)
                    };
                    first.repeat(prec.unwrap_or(1).max(1))
                }
                // The precision limits the number of *input* characters consumed;
                // escaping is applied afterward so each quote still doubles.
                'q' => match v {
                    Value::Null => b"(NULL)".to_vec(),
                    other => printf_escape_byte(
                        &printf_prec_bytes(&crate::exec::eval::text_bytes(&other), prec),
                        b'\'',
                    ),
                },
                'Q' => match v {
                    // Like %q but always wrapped in single quotes (never counted).
                    Value::Null => b"NULL".to_vec(),
                    other => {
                        let esc = printf_escape_byte(
                            &printf_prec_bytes(&crate::exec::eval::text_bytes(&other), prec),
                            b'\'',
                        );
                        let mut out = alloc::vec::Vec::with_capacity(esc.len() + 2);
                        out.push(b'\'');
                        out.extend_from_slice(&esc);
                        out.push(b'\'');
                        out
                    }
                },
                'w' => match v {
                    Value::Null => b"(NULL)".to_vec(),
                    other => printf_escape_byte(
                        &printf_prec_bytes(&crate::exec::eval::text_bytes(&other), prec),
                        b'"',
                    ),
                },
                _ => unreachable!(),
            }
        } else {
            let s: String = match conv {
                'd' | 'i' => int_body(eval::to_int_value(&next(&mut arg_idx)), prec, plus, space),
                'u' => int_unsigned(
                    eval::to_int_value(&next(&mut arg_idx)) as u64,
                    10,
                    false,
                    prec,
                    alt,
                ),
                'x' => int_unsigned(
                    eval::to_int_value(&next(&mut arg_idx)) as u64,
                    16,
                    false,
                    prec,
                    alt,
                ),
                'X' => int_unsigned(
                    eval::to_int_value(&next(&mut arg_idx)) as u64,
                    16,
                    true,
                    prec,
                    alt,
                ),
                'o' => int_unsigned(
                    eval::to_int_value(&next(&mut arg_idx)) as u64,
                    8,
                    false,
                    prec,
                    alt,
                ),
                'f' | 'e' | 'E' | 'g' | 'G' => {
                    // Every float conversion renders through the port of
                    // SQLite's own `FpDecode` machinery (`printf.c`'s
                    // etFLOAT/etEXP/etGENERIC arm): 16 significant digits (26
                    // under the `!` flag), `#`/`!` decimal-point and
                    // trailing-zero handling, inline `,` grouping, and the `0`
                    // flag's numeric rendering of infinities.
                    let f = eval::to_f64(&next(&mut arg_idx));
                    let xtype = match conv {
                        'f' => crate::util::fpdecode::XType::Float,
                        'e' | 'E' => crate::util::fpdecode::XType::Exp,
                        _ => crate::util::fpdecode::XType::Generic,
                    };
                    if f.is_nan() {
                        // NaN is plain text ("null" under the `0` flag) that is
                        // never signed or zero-padded, only space-justified.
                        let body = if zero { "null" } else { "NaN" };
                        zero = false;
                        String::from(body)
                    } else {
                        let mut body = crate::util::fpdecode::printf_float(
                            f,
                            prec.unwrap_or(6) as i32,
                            xtype,
                            bang,
                            alt,
                            comma,
                            zero,
                        );
                        if matches!(conv, 'E' | 'G') {
                            body = body.replace('e', "E");
                        }
                        with_sign(body, plus, space)
                    }
                }
                // `%q`/`%Q`/`%w` are handled in the byte branch above.
                _ => {
                    // Unknown conversion (e.g. `%y`): SQLite discards the directive
                    // and stops formatting the rest of the string.
                    break;
                }
            };
            s.into_bytes()
        };
        // SQLite still honours the `0` flag for integers when a precision is
        // given (unlike C): `%05.3d` of 7 is `00007` — precision sets the minimum
        // digit count and the `0` flag then zero-pads to the field width.
        let zero_pad = zero && numeric;
        // The float conversions weave `,` separators in during rendering
        // (`printf_float`), like SQLite; only the integer conversions group
        // here, post-hoc.
        let grouped = comma && matches!(conv, 'd' | 'i');
        // The `,` flag groups the integer part in threes. With zero-padding the
        // grouping is applied *after* the zero-fill to the field width (so the
        // commas fall between the pad zeros and the result may exceed `width`);
        // otherwise it groups first and then space-pads.
        let body: Vec<u8> = if grouped && zero_pad && width > printf_char_count(&body) {
            // Zero-pad the magnitude (after any sign) up to the field width, then
            // group. `grouped` implies a numeric (ASCII) body, so read it as str.
            let bs = core::str::from_utf8(&body).unwrap_or("");
            let (sign, mag) = match bs.strip_prefix(['+', '-', ' ']) {
                Some(r) => (&bs[..1], r),
                None => ("", bs),
            };
            let pad = width - printf_char_count(&body);
            let padded = alloc::format!("{sign}{}{mag}", "0".repeat(pad));
            group_thousands(&padded).into_bytes()
        } else if grouped {
            group_thousands(core::str::from_utf8(&body).unwrap_or("")).into_bytes()
        } else {
            body
        };
        // Once comma-grouping has consumed the zero-padding above, fall back to
        // ordinary (space) justification for any residual width.
        let zero_pad = zero_pad && !grouped;
        // For integer conversions SQLite lets the `0` flag override `-` (so
        // `%-010d` of 7 is `0000000007`); for floats/strings, `-` wins. So an
        // integer zero-pad takes priority over left-justification.
        let is_int = matches!(conv, 'd' | 'i' | 'u' | 'x' | 'X' | 'o');
        let zero_wins = zero_pad && (is_int || !left);
        // For `#` zero-padding, the alternate-form prefix (`0x`/`0X`, or a
        // leading octal `0`) is emitted before the zero-fill and is *not* counted
        // toward the field width (matching C/SQLite).
        let alt_prefix_len = if alt && zero_pad {
            match conv {
                'x' | 'X' if body.starts_with(b"0x") || body.starts_with(b"0X") => 2,
                'o' if body.first() == Some(&b'0') => 1,
                _ => 0,
            }
        } else {
            0
        };
        // apply width/justification
        let len = printf_char_count(&body);
        // Zero-pad reaches `width` digits *after* the alt prefix; space-pad
        // counts the whole body.
        let effective = if zero_wins { len - alt_prefix_len } else { len };
        if width > effective {
            if (out.len() + width + alt_prefix_len) >= PRINTF_LIMIT {
                too_big = true;
                break;
            }
            let pad = width - effective;
            if zero_wins {
                // zero-pad after any sign and the alternate-form prefix (numeric,
                // so ASCII bytes).
                let (sign, rest): (&[u8], &[u8]) =
                    if matches!(body.first().copied(), Some(b'-' | b'+' | b' ')) {
                        (&body[..1], &body[1..])
                    } else {
                        (&[], &body[..])
                    };
                let (prefix, rest) = rest.split_at(alt_prefix_len.min(rest.len()));
                out.extend_from_slice(sign);
                out.extend_from_slice(prefix);
                out.resize(out.len() + pad, b'0');
                out.extend_from_slice(rest);
            } else if left {
                out.extend_from_slice(&body);
                out.resize(out.len() + pad, b' ');
            } else {
                out.resize(out.len() + pad, b' ');
                out.extend_from_slice(&body);
            }
        } else {
            out.extend_from_slice(&body);
        }
        if out.len() >= PRINTF_LIMIT {
            too_big = true;
            break;
        }
    }
    if too_big {
        return Value::Text(crate::value::Text::from_bytes(Vec::new()));
    }
    Value::Text(crate::value::Text::from_bytes(out))
}

/// Format a signed integer for `%d`/`%i`: apply a minimum-digit precision
/// (zero-padding the magnitude) and the sign flags.
fn int_body(n: i64, prec: Option<usize>, plus: bool, space: bool) -> String {
    let mag = n.unsigned_abs();
    let mut digits = alloc::format!("{mag}");
    // Precision 0 with value 0 still prints "0" in SQLite (unlike C, which emits
    // nothing); otherwise precision is the minimum number of digits.
    if let Some(p) = prec {
        while digits.len() < p {
            digits.insert(0, '0');
        }
    }
    let mut s = digits;
    if n < 0 {
        s.insert(0, '-');
    } else if plus {
        s.insert(0, '+');
    } else if space {
        s.insert(0, ' ');
    }
    s
}

/// Format an unsigned integer for `%u`/`%x`/`%X`/`%o`: minimum-digit precision
/// and, with `#`, the alternate-form prefix (`0x`/`0X` for hex, leading `0` for
/// octal — never on a zero value).
fn int_unsigned(n: u64, base: u32, upper: bool, prec: Option<usize>, alt: bool) -> String {
    let mut digits = match base {
        16 if upper => alloc::format!("{n:X}"),
        16 => alloc::format!("{n:x}"),
        8 => alloc::format!("{n:o}"),
        _ => alloc::format!("{n}"),
    };
    if let Some(p) = prec {
        while digits.len() < p {
            digits.insert(0, '0');
        }
    }
    if alt && n != 0 {
        match base {
            16 if upper => digits.insert_str(0, "0X"),
            16 => digits.insert_str(0, "0x"),
            // SQLite's octal `#` always prepends a single `0` (e.g. `%#.5o` of 8
            // is `000010`), regardless of any precision zero-padding.
            8 => digits.insert(0, '0'),
            _ => {}
        }
    }
    digits
}

/// Prepend the `+`/space sign flag to a formatted non-negative number (a leading
/// `-` is already present for negatives).
fn with_sign(mut s: String, plus: bool, space: bool) -> String {
    if !s.starts_with('-') {
        if plus {
            s.insert(0, '+');
        } else if space {
            s.insert(0, ' ');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn t(v: &str) -> Value {
        Value::Text(String::from(v).into())
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
    fn timediff_basic() {
        let td = |a: &str, b: &str| timediff(&t(a), &t(b));
        // amount to add to B to reach A
        assert_eq!(
            td("2020-01-02 00:00:00", "2020-01-01 00:00:00"),
            t("+0000-00-01 00:00:00.000")
        );
        assert_eq!(
            td("2024-03-01", "2024-02-01"),
            t("+0000-01-00 00:00:00.000")
        );
        // negative sign when A < B
        assert_eq!(
            td("2020-01-01", "2020-03-01"),
            t("-0000-02-00 00:00:00.000")
        );
        assert_eq!(
            td("2025-06-15", "2020-01-01"),
            t("+0005-05-14 00:00:00.000")
        );
        // sub-second
        assert_eq!(
            td("2020-01-01 12:30:45.5", "2020-01-01 12:00:00"),
            t("+0000-00-00 00:30:45.500")
        );
        // equal inputs
        assert_eq!(
            td("2020-01-01", "2020-01-01"),
            t("+0000-00-00 00:00:00.000")
        );
        // leap-day month boundary (the back-off loop)
        assert_eq!(
            td("2024-03-31", "2024-02-29"),
            t("+0000-01-02 00:00:00.000")
        );
        // forward/backward asymmetry around uneven months
        assert_eq!(
            td("2159-10-07", "2156-03-17"),
            t("+0003-06-20 00:00:00.000")
        );
        assert_eq!(
            td("2156-03-17", "2159-10-07"),
            t("-0003-06-21 00:00:00.000")
        );
        // invalid / NULL operands -> NULL
        assert_eq!(timediff(&t("not a date"), &t("2020-01-01")), Value::Null);
        assert_eq!(timediff(&Value::Null, &t("2020-01-01")), Value::Null);
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
