//! Byte-exact port of SQLite's floating-point decimal decoder and renderer
//! (`sqlite3FpDecode` + `dekkerMul2` + the `etFLOAT`/`etEXP`/`etGENERIC` arm of
//! `sqlite3_str_vappendf`, from the pinned 3.50.4 amalgamation).
//!
//! SQLite renders a `double` to decimal by scaling it into `[1e17, 1e19)` with
//! Dekker-style double-double multiplication (so the 18–19 leading significant
//! digits are computed exactly), reading off the digits, then rounding to the
//! requested precision. This differs from a naive "print the exact f64 decimal
//! expansion then cap" approach: the double-double normalization truncates to
//! ~18-20 significant digits, so intermediate values round differently.
//!
//! We need this for two byte-exact-vs-sqlite surfaces: `quote()` of a real
//! (`%!0.15g`, falling back to `%!0.20e` when 15 digits do not round-trip) and
//! the `printf` `!` (alt-form-2) conversions.

use alloc::string::String;
use alloc::vec::Vec;

/// The `%f`/`%e`/`%g` conversion kind.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum XType {
    /// `%f` — fixed notation.
    Float,
    /// `%e` — exponential notation.
    Exp,
    /// `%g` — the shorter of fixed/exponential.
    Generic,
}

/// The decoded significant digits of a finite, non-zero `double`, plus where the
/// decimal point sits. Mirrors SQLite's `struct FpDecode`.
struct FpDecode {
    sign: u8,        // b'+' or b'-'
    is_special: u8,  // 0 normal, 1 Infinity, 2 NaN
    n: usize,        // number of significant digits in `digits`
    i_dp: i32,       // decimal-point location (digits before the point)
    digits: Vec<u8>, // the `n` significant ASCII digits, most significant first
}

/// Dekker double-double multiply: `x *= (y + yy)` keeping a two-limb result in
/// `x[0]` (high) and `x[1]` (low). A direct port of `dekkerMul2`. Rust `f64`
/// arithmetic is IEEE-754 binary64 with no implicit fused-multiply-add or
/// extended precision, which is exactly the "truncate every intermediate to
/// binary64" behaviour the C code forces with `volatile`.
fn dekker_mul2(x: &mut [f64; 2], y: f64, yy: f64) {
    // `bb` is an optimization barrier that stands in for C's `volatile`: it forces
    // every intermediate to be rounded to binary64 and prevents the backend from
    // contracting `a*b + c*d` into a fused-multiply-add. Dekker's exact two-product
    // depends on each sub-product being separately rounded, so a stray FMA would
    // corrupt the low limb (visible only at extreme exponents needing several
    // scaling steps).
    use core::hint::black_box as bb;
    let mut m = x[0].to_bits();
    m &= 0xfffffffffc000000u64;
    let hx = f64::from_bits(m);
    let tx = bb(x[0] - hx);
    m = y.to_bits();
    m &= 0xfffffffffc000000u64;
    let hy = f64::from_bits(m);
    let ty = bb(y - hy);
    let p = bb(hx * hy);
    let q = bb(bb(hx * ty) + bb(tx * hy));
    let c = bb(p + q);
    let mut cc = bb(bb(bb(p - c) + q) + bb(tx * ty));
    cc = bb(bb(bb(x[0] * yy) + bb(x[1] * y)) + cc);
    x[0] = c + cc;
    x[1] = c - x[0];
    x[1] += cc;
}

impl FpDecode {
    /// Port of `sqlite3FpDecode`. `i_round`: if > 0, round to that many
    /// significant digits; if <= 0, round to `-i_round` digits after the decimal
    /// point. `mx_round` caps the significant-digit count (16 normally, 26 under
    /// the `!` alt-form-2 flag).
    // The scaling thresholds and Dekker error terms are copied verbatim from
    // SQLite's source so they can be audited against it; several carry more
    // decimal digits than an `f64` distinguishes (they round to the intended
    // value), which is exactly what `clippy::excessive_precision` flags.
    #[allow(clippy::excessive_precision)]
    fn decode(mut r: f64, mut i_round: i32, mx_round: i32) -> FpDecode {
        let mut zbuf = [0u8; 24];
        let mut exp: i32 = 0;

        let sign;
        if r < 0.0 {
            sign = b'-';
            r = -r;
        } else if r == 0.0 {
            return FpDecode {
                sign: b'+',
                is_special: 0,
                n: 1,
                i_dp: 1,
                digits: alloc::vec![b'0'],
            };
        } else {
            sign = b'+';
        }

        let v0 = r.to_bits();
        let e = v0 >> 52;
        if (e & 0x7ff) == 0x7ff {
            return FpDecode {
                sign,
                is_special: if v0 != 0x7ff0000000000000 { 2 } else { 1 },
                n: 0,
                i_dp: 0,
                digits: Vec::new(),
            };
        }

        // Scale r into [1e17, 1e19) via double-double multiply, tracking `exp`.
        let mut rr = [r, 0.0f64];
        if rr[0] > 9.223372036854774784e18 {
            while rr[0] > 9.223372036854774784e118 {
                exp += 100;
                dekker_mul2(&mut rr, 1.0e-100, -1.99918998026028836196e-117);
            }
            while rr[0] > 9.223372036854774784e28 {
                exp += 10;
                dekker_mul2(&mut rr, 1.0e-10, -3.6432197315497741579e-27);
            }
            while rr[0] > 9.223372036854774784e18 {
                exp += 1;
                dekker_mul2(&mut rr, 1.0e-01, -5.5511151231257827021e-18);
            }
        } else {
            while rr[0] < 9.223372036854774784e-83 {
                exp -= 100;
                dekker_mul2(&mut rr, 1.0e100, -1.5902891109759918046e83);
            }
            while rr[0] < 9.223372036854774784e7 {
                exp -= 10;
                dekker_mul2(&mut rr, 1.0e10, 0.0);
            }
            while rr[0] < 9.22337203685477478e17 {
                exp -= 1;
                dekker_mul2(&mut rr, 1.0e01, 0.0);
            }
        }
        let mut v: u64 = if rr[1] < 0.0 {
            (rr[0] as u64) - ((-rr[1]) as u64)
        } else {
            (rr[0] as u64) + (rr[1] as u64)
        };

        // Extract significant digits into the back of zbuf.
        let mut i: i32 = zbuf.len() as i32 - 1;
        while v != 0 {
            zbuf[i as usize] = (v % 10) as u8 + b'0';
            i -= 1;
            v /= 10;
        }
        let mut n = zbuf.len() as i32 - 1 - i;
        let mut i_dp = n + exp;

        if i_round <= 0 {
            i_round = i_dp - i_round;
            if i_round == 0 && zbuf[(i + 1) as usize] >= b'5' {
                i_round = 1;
                zbuf[i as usize] = b'0';
                i -= 1;
                n += 1;
                i_dp += 1;
            }
        }
        if i_round > 0 && (i_round < n || n > mx_round) {
            // z points at the first significant digit, zbuf[i+1..].
            if i_round > mx_round {
                i_round = mx_round;
            }
            n = i_round;
            let zbase = (i + 1) as usize;
            if zbuf[zbase + i_round as usize] >= b'5' {
                let mut j = i_round - 1;
                loop {
                    zbuf[zbase + j as usize] += 1;
                    if zbuf[zbase + j as usize] <= b'9' {
                        break;
                    }
                    zbuf[zbase + j as usize] = b'0';
                    if j == 0 {
                        zbuf[i as usize] = b'1';
                        i -= 1;
                        n += 1;
                        i_dp += 1;
                        break;
                    } else {
                        j -= 1;
                    }
                }
            }
        }

        // Collect the n digits starting at zbuf[i+1], stripping trailing zeros.
        let zbase = (i + 1) as usize;
        let mut digits: Vec<u8> = zbuf[zbase..zbase + n as usize].to_vec();
        while digits.len() > 1 && *digits.last().unwrap() == b'0' {
            digits.pop();
        }

        FpDecode {
            sign,
            is_special: 0,
            n: digits.len(),
            i_dp,
            digits,
        }
    }
}

/// Render `r` the way SQLite's `printf` renders a `%f`/`%e`/`%g` conversion with
/// the given `precision`, alt-form-2 (`!`) and alt-form (`#`) flags. This is the
/// core shared by `quote()` and the `printf` bang path; it does not apply field
/// width, zero-padding, thousands separators, or a forced `+`/space prefix
/// (callers that need those handle them separately).
pub fn format(r: f64, precision: i32, xtype: XType, altform2: bool, altform: bool) -> String {
    printf_float(r, precision, xtype, altform2, altform, false, false)
}

/// The full `etFLOAT`/`etEXP`/`etGENERIC` conversion arm of SQLite's
/// `sqlite3_str_vappendf`, minus field-width handling (the caller pads):
/// [`format`] plus the `,` flag (`cthousand` — thousands separators woven into
/// the pre-decimal digits) and the `0` flag's effect on the *non-finite*
/// renderings (`zeropad`: NaN prints `null` instead of `NaN`, and an infinity
/// is rendered as the numeric form `9` with decimal-point position 1000 — a
/// 1000-digit number under `%f`, `9.0…e+999` under `%e`/`%g` — instead of
/// `Inf`). A negative value carries its `-` sign; the caller applies any
/// `+`/space prefix and zero-pads to the field width afterwards, exactly like
/// the C code's trailing `flag_zeropad` block.
pub fn printf_float(
    r: f64,
    mut precision: i32,
    mut xtype: XType,
    altform2: bool,
    altform: bool,
    cthousand: bool,
    zeropad: bool,
) -> String {
    let i_round = match xtype {
        XType::Float => -precision,
        XType::Generic => {
            if precision == 0 {
                precision = 1;
            }
            precision
        }
        XType::Exp => precision + 1,
    };
    let mut s = FpDecode::decode(r, i_round, if altform2 { 26 } else { 16 });

    if s.is_special != 0 {
        if s.is_special == 2 {
            // NaN: never signed; under the `0` flag SQLite prints "null".
            return String::from(if zeropad { "null" } else { "NaN" });
        } else if zeropad {
            // An infinity under the `0` flag renders numerically as the digit 9
            // with the decimal point at position 1000 (so ~1e999, the largest
            // value SQLite's text-to-float parsing round-trips to infinity).
            s.digits = alloc::vec![b'9'];
            s.i_dp = 1000;
            s.n = 1;
        } else {
            let mut out = String::new();
            if s.sign == b'-' {
                out.push('-');
            }
            out.push_str("Inf");
            return out;
        }
    }

    let mut out = String::new();
    if s.sign == b'-' {
        out.push('-');
    }

    let exp = s.i_dp - 1;
    let flag_rtz;
    if xtype == XType::Generic {
        precision -= 1;
        flag_rtz = !altform;
        if exp < -4 || exp > precision {
            xtype = XType::Exp;
        } else {
            precision -= exp;
            xtype = XType::Float;
        }
    } else {
        flag_rtz = altform2;
    }
    let mut e2 = if xtype == XType::Exp { 0 } else { s.i_dp - 1 };

    let flag_dp = precision > 0 || altform || altform2;
    let mut j = 0usize;
    let digit = |j: &mut usize| -> char {
        let c = if *j < s.n { s.digits[*j] } else { b'0' };
        *j += 1;
        c as char
    };

    // Digits before the decimal point (with the `,` flag, a thousands
    // separator after each digit that has a multiple-of-3 count remaining).
    if e2 < 0 {
        out.push('0');
    } else {
        while e2 >= 0 {
            out.push(digit(&mut j));
            if cthousand && e2 % 3 == 0 && e2 > 1 {
                out.push(',');
            }
            e2 -= 1;
        }
    }
    // The decimal point.
    if flag_dp {
        out.push('.');
    }
    // Leading zeros after the point, before the first significant digit.
    e2 += 1;
    while e2 < 0 && precision > 0 {
        out.push('0');
        precision -= 1;
        e2 += 1;
    }
    // Significant digits after the point.
    while precision > 0 {
        out.push(digit(&mut j));
        precision -= 1;
    }
    // Remove trailing zeros and a bare trailing '.'.
    if flag_rtz && flag_dp {
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            if altform2 {
                out.push('0');
            } else {
                out.pop();
            }
        }
    }
    // The "eNNN" suffix.
    if xtype == XType::Exp {
        let mut e = s.i_dp - 1;
        out.push('e');
        if e < 0 {
            out.push('-');
            e = -e;
        } else {
            out.push('+');
        }
        if e >= 100 {
            out.push((b'0' + (e / 100) as u8) as char);
            e %= 100;
        }
        out.push((b'0' + (e / 10) as u8) as char);
        out.push((b'0' + (e % 10) as u8) as char);
    }
    out
}

/// Render a real exactly as SQLite's `quote()` does: `%!0.15g`, falling back to
/// `%!0.20e` when the 15-significant-digit form does not round-trip back to the
/// same `double`.
pub fn quote_real(r: f64) -> String {
    if r.is_nan() {
        // quote() of a NaN: SQLite prints the bare token (it is not valid SQL,
        // but matches the CLI). NaN cannot be a stored value in practice.
        return String::from("NaN");
    }
    if r.is_infinite() {
        // %!0.15g of an infinity renders "Inf"/"-Inf".
        return format(r, 15, XType::Generic, true, false);
    }
    let g = format(r, 15, XType::Generic, true, false);
    // Round-trip check: SQLite re-parses the 15-digit form with `sqlite3AtoF`
    // (correctly-rounded decimal-to-double) and keeps it only if it recovers the
    // exact same double. Rust's `f64` parser is likewise correctly rounded.
    if g.parse::<f64>() == Ok(r) {
        g
    } else {
        format(r, 20, XType::Exp, true, false)
    }
}

#[cfg(test)]
// The reference vectors are literal doubles captured from the sqlite3 CLI; some
// happen to sit near PI/E, which `clippy::approx_constant` would rewrite.
#[allow(clippy::approx_constant)]
mod tests {
    use super::*;

    /// Byte-exact values captured from the sqlite3 3.50.4 CLI (`SELECT
    /// quote(CAST(<x> AS REAL))`), covering the round-tripping `%!0.15g` path and
    /// the `%!0.20e` fallback.
    #[test]
    fn quote_real_matches_sqlite_reference() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0.0"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (100.0, "100.0"),
            (-2.5, "-2.5"),
            (2.0 / 3.0, "6.666666666666666296e-01"),
            (1.0 / 3.0, "3.333333333333333148e-01"),
            (3.141592653589793, "3.141592653589793116e+00"),
            (123456789012345.6, "1.234567890123455938e+14"),
            (9223372036854775808.0, "9.22337203685477581e+18"),
            (1e20, "1.0e+20"),
            (1e-20, "1.0e-20"),
        ];
        for &(r, want) in cases {
            assert_eq!(quote_real(r), want, "quote_real({r})");
        }
    }

    /// Whatever `quote_real` prints must reparse to the exact same double — that
    /// is the whole point of the `%!0.15g` -> `%!0.20e` fallback.
    #[test]
    fn quote_real_round_trips() {
        let vals = [
            0.1,
            2.0 / 3.0,
            3.141592653589793,
            2.718281828459045,
            1.0 / 7.0,
            123.456,
            9.87654321e-5,
            -4.2e30,
            1.7976931348623157e308,
            5e-324, // smallest subnormal
        ];
        for r in vals {
            let q = quote_real(r);
            assert_eq!(q.parse::<f64>(), Ok(r), "round-trip {r} via {q}");
        }
    }
}
