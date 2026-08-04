//! Arbitrary-precision decimal arithmetic, a faithful `no_std` port of SQLite's
//! `ext/misc/decimal.c` (the `decimal` loadable extension).
//!
//! The engine keeps a number as a sign, a most-significant-first array of
//! base-ten digits, and a count of how many of those digits sit to the right of
//! the decimal point. All arithmetic (`add`, `sub`, `mul`, `cmp`) is exact — no
//! floating point is involved for text/integer inputs — and rendering
//! reproduces SQLite's canonical decimal text byte-for-byte. Float and 8-byte
//! blob inputs are expanded to their exact decimal value the same way SQLite
//! does, via [`Decimal::from_double`].
//!
//! Reference: SQLite 3.50.4 `ext/misc/decimal.c`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// A base-ten arbitrary-precision decimal.
///
/// `digits` holds the significand most-significant digit first (each element is
/// `0..=9`); `n_frac` is how many trailing entries lie to the right of the
/// decimal point.
#[derive(Clone, Debug)]
pub struct Decimal {
    /// `true` when the value is negative.
    sign: bool,
    /// Digits, most significant first, each `0..=9`.
    digits: Vec<u8>,
    /// Number of digits to the right of the decimal point.
    n_frac: i64,
}

/// SQLite's `IsSpace`, i.e. C `isspace`: space, tab, LF, VT, FF, CR. (Rust's
/// `is_ascii_whitespace` omits the vertical tab `0x0b`, so this is spelled out.)
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

impl Decimal {
    /// Parse a decimal from raw text bytes, mirroring `decimalNewFromText`.
    ///
    /// Accepts optional leading whitespace, an optional `+`/`-` sign, decimal
    /// digits with an optional single `.`, and an optional `e`/`E` exponent.
    /// Any other character is silently skipped (as in SQLite). Never fails: an
    /// empty or all-zero input parses to zero.
    pub fn from_bytes(bytes: &[u8]) -> Decimal {
        let n = bytes.len();
        let mut sign = false;
        let mut digits: Vec<u8> = Vec::with_capacity(n + 1);
        // While scanning, `n_frac` first holds the marker `pos_of_dot + 1`
        // (0 == no dot), then is rewritten to the true fractional-digit count.
        let mut n_frac: i64 = 0;
        let mut i_exp: i64 = 0;
        let mut i = 0usize;

        while i < n && is_space(bytes[i]) {
            i += 1;
        }
        if i < n && bytes[i] == b'-' {
            sign = true;
            i += 1;
        } else if i < n && bytes[i] == b'+' {
            i += 1;
        }
        while i < n && bytes[i] == b'0' {
            i += 1;
        }
        while i < n {
            let c = bytes[i];
            if c.is_ascii_digit() {
                digits.push(c - b'0');
            } else if c == b'.' {
                n_frac = digits.len() as i64 + 1;
            } else if c == b'e' || c == b'E' {
                let mut j = i + 1;
                let mut neg = false;
                if j >= n {
                    break;
                }
                if bytes[j] == b'-' {
                    neg = true;
                    j += 1;
                } else if bytes[j] == b'+' {
                    j += 1;
                }
                while j < n && i_exp < 1_000_000 {
                    if bytes[j].is_ascii_digit() {
                        i_exp = i_exp * 10 + (bytes[j] - b'0') as i64;
                    }
                    j += 1;
                }
                if neg {
                    i_exp = -i_exp;
                }
                break;
            }
            i += 1;
        }

        if n_frac != 0 {
            n_frac = digits.len() as i64 - (n_frac - 1);
        }

        if i_exp > 0 {
            if n_frac > 0 {
                if i_exp <= n_frac {
                    n_frac -= i_exp;
                    i_exp = 0;
                } else {
                    i_exp -= n_frac;
                    n_frac = 0;
                }
            }
            if i_exp > 0 {
                let new_len = digits.len() + i_exp as usize;
                digits.resize(new_len, 0);
            }
        } else if i_exp < 0 {
            i_exp = -i_exp;
            let n_extra = digits.len() as i64 - n_frac - 1;
            if n_extra != 0 {
                if n_extra >= i_exp {
                    n_frac += i_exp;
                    i_exp = 0;
                } else {
                    i_exp -= n_extra;
                    n_frac = digits.len() as i64 - 1;
                }
            }
            if i_exp > 0 {
                let mut newd = vec![0u8; i_exp as usize];
                newd.extend_from_slice(&digits);
                digits = newd;
                n_frac += i_exp;
            }
        }

        Decimal {
            sign,
            digits,
            n_frac,
        }
    }

    /// Parse a decimal from a `&str`. Convenience wrapper over [`from_bytes`].
    ///
    /// [`from_bytes`]: Decimal::from_bytes
    pub fn parse(s: &str) -> Decimal {
        Decimal::from_bytes(s.as_bytes())
    }

    /// Expand so the value has at least `n_digit` total digits and `n_frac`
    /// fractional digits, padding with zeros. Mirrors `decimal_expand`.
    fn expand(&mut self, n_digit: i64, n_frac: i64) {
        let cur_digit = self.digits.len() as i64;
        let n_add_frac = n_frac - self.n_frac;
        let n_add_sig = (n_digit - cur_digit) - n_add_frac;
        if n_add_frac == 0 && n_add_sig == 0 {
            return;
        }
        if n_add_sig > 0 {
            let mut newd = vec![0u8; n_add_sig as usize];
            newd.extend_from_slice(&self.digits);
            self.digits = newd;
        }
        if n_add_frac > 0 {
            let new_len = self.digits.len() + n_add_frac as usize;
            self.digits.resize(new_len, 0);
            self.n_frac += n_add_frac;
        }
    }

    /// `self := self + other`. Mirrors `decimal_add`; both operands may become
    /// denormalized in the process (which is why `other` is taken by value).
    pub fn add(&mut self, mut other: Decimal) {
        let mut n_sig = self.digits.len() as i64 - self.n_frac;
        if n_sig > 0 && self.digits[0] == 0 {
            n_sig -= 1;
        }
        let b_sig = other.digits.len() as i64 - other.n_frac;
        if n_sig < b_sig {
            n_sig = b_sig;
        }
        let mut n_frac = self.n_frac;
        if n_frac < other.n_frac {
            n_frac = other.n_frac;
        }
        let n_digit = n_sig + n_frac + 1;
        self.expand(n_digit, n_frac);
        other.expand(n_digit, n_frac);
        let len = self.digits.len();

        if self.sign == other.sign {
            let mut carry = 0i32;
            for i in (0..len).rev() {
                let x = self.digits[i] as i32 + other.digits[i] as i32 + carry;
                if x >= 10 {
                    carry = 1;
                    self.digits[i] = (x - 10) as u8;
                } else {
                    carry = 0;
                    self.digits[i] = x as u8;
                }
            }
        } else {
            // Subtract the smaller magnitude from the larger. `memcmp` over the
            // now equal-length digit arrays decides which is larger.
            let flip = cmp_digits(&self.digits, &other.digits) < 0;
            if flip {
                self.sign = !self.sign;
            }
            let mut borrow = 0i32;
            for i in (0..len).rev() {
                let (av, bv) = if flip {
                    (other.digits[i] as i32, self.digits[i] as i32)
                } else {
                    (self.digits[i] as i32, other.digits[i] as i32)
                };
                let x = av - bv - borrow;
                if x < 0 {
                    self.digits[i] = (x + 10) as u8;
                    borrow = 1;
                } else {
                    self.digits[i] = x as u8;
                    borrow = 0;
                }
            }
        }
    }

    /// `self := self - other`. Implemented as `self + (-other)`, exactly as
    /// SQLite's `decimalSubFunc` flips the sign before calling `decimal_add`.
    pub fn sub(&mut self, mut other: Decimal) {
        other.sign = !other.sign;
        self.add(other);
    }

    /// `self := self * other`. Mirrors `decimalMul`.
    ///
    /// Retains all significant digits after the decimal point, trimming
    /// trailing fractional zeros only down to the larger of the two inputs'
    /// fractional counts.
    pub fn mul(&mut self, other: &Decimal) {
        let na = self.digits.len();
        let nb = other.digits.len();
        let mut acc = vec![0u8; na + nb + 2];
        let min_frac = self.n_frac.min(other.n_frac);
        for i in (0..na).rev() {
            let f = self.digits[i] as i32;
            let mut carry = 0i32;
            let mut k: i64 = i as i64 + nb as i64 - 1 + 3;
            for j in (0..nb).rev() {
                let x = acc[k as usize] as i32 + f * other.digits[j] as i32 + carry;
                acc[k as usize] = (x % 10) as u8;
                carry = x / 10;
                k -= 1;
            }
            // k is now i+2.
            let x = acc[k as usize] as i32 + carry;
            acc[k as usize] = (x % 10) as u8;
            acc[(k - 1) as usize] = (acc[(k - 1) as usize] as i32 + x / 10) as u8;
        }
        self.digits = acc;
        self.n_frac += other.n_frac;
        self.sign ^= other.sign;
        while self.n_frac > min_frac && *self.digits.last().unwrap() == 0 {
            self.n_frac -= 1;
            self.digits.pop();
        }
    }

    /// Compare two decimals, returning `-1`, `0`, or `+1`. Mirrors
    /// `decimal_cmp`.
    ///
    /// Note this compares the *stored* representations, so trailing zeros make a
    /// value compare greater (`1.0` > `1`) — matching SQLite exactly.
    pub fn compare(&self, other: &Decimal) -> i32 {
        if self.sign != other.sign {
            return if self.sign { -1 } else { 1 };
        }
        // For two negatives, comparing magnitudes reverses the result, so swap.
        let (a, b) = if self.sign {
            (other, self)
        } else {
            (self, other)
        };
        let n_a_sig = a.digits.len() as i64 - a.n_frac;
        let n_b_sig = b.digits.len() as i64 - b.n_frac;
        if n_a_sig != n_b_sig {
            return sgn(n_a_sig - n_b_sig);
        }
        let n = a.digits.len().min(b.digits.len());
        let rc = cmp_digits(&a.digits[..n], &b.digits[..n]);
        if rc != 0 {
            return sgn(rc as i64);
        }
        sgn(a.digits.len() as i64 - b.digits.len() as i64)
    }

    /// Render the canonical decimal text, mirroring `decimal_result`.
    ///
    /// Leading integer zeros are stripped, a lone zero integer part is shown as
    /// `0`, negative zero is normalized to `0`, and every stored fractional
    /// digit is emitted after the point.
    pub fn to_decimal_string(&self) -> String {
        let n_digit = self.digits.len() as i64;
        let mut sign = self.sign;
        if n_digit == 0 || (n_digit == 1 && self.digits[0] == 0) {
            sign = false;
        }
        let mut z = String::with_capacity(self.digits.len() + 4);
        if sign {
            z.push('-');
        }
        let mut n = n_digit - self.n_frac; // count of integer-part digits
        if n <= 0 {
            z.push('0');
        }
        let mut j = 0i64;
        while n > 1 && self.digits[j as usize] == 0 {
            j += 1;
            n -= 1;
        }
        while n > 0 {
            z.push((self.digits[j as usize] + b'0') as char);
            j += 1;
            n -= 1;
        }
        if self.n_frac > 0 {
            z.push('.');
            loop {
                z.push((self.digits[j as usize] + b'0') as char);
                j += 1;
                if j >= n_digit {
                    break;
                }
            }
        }
        z
    }

    /// Build a decimal that is exactly `2**n`, mirroring `decimalPow2`.
    /// Returns `None` for `|n| > 20000` (SQLite's guard).
    fn from_pow2(mut n: i32) -> Option<Decimal> {
        if !(-20000..=20000).contains(&n) {
            return None;
        }
        let mut a = Decimal::parse("1.0");
        if n == 0 {
            return Some(a);
        }
        let mut x = if n > 0 {
            Decimal::parse("2.0")
        } else {
            n = -n;
            Decimal::parse("0.5")
        };
        loop {
            if n & 1 != 0 {
                a.mul(&x);
            }
            n >>= 1;
            if n == 0 {
                break;
            }
            let x2 = x.clone();
            x.mul(&x2);
        }
        Some(a)
    }

    /// Expand an IEEE-754 `f64` into its exact decimal value, mirroring
    /// `decimalFromDouble`. Returns `None` for a NaN or infinity.
    pub fn from_double(r: f64) -> Option<Decimal> {
        let is_neg = r < 0.0;
        let r = if is_neg { -r } else { r };
        let a = r.to_bits() as i64;
        let (m, e): (i64, i32);
        if a == 0 {
            m = 0;
            e = 0;
        } else {
            let mut ee = (a >> 52) as i32;
            let mut mm = a & (((1i64) << 52) - 1);
            if ee == 0 {
                mm <<= 1;
            } else {
                mm |= (1i64) << 52;
            }
            while ee < 1075 && mm > 0 && (mm & 1) == 0 {
                mm >>= 1;
                ee += 1;
            }
            if is_neg {
                mm = -mm;
            }
            ee -= 1075;
            if ee > 971 {
                return None; // NaN or Infinity
            }
            m = mm;
            e = ee;
        }
        // m is the integer significand, e the (base-2) exponent.
        let mut result = Decimal::parse(&int_to_string(m));
        if let Some(px) = Decimal::from_pow2(e) {
            result.mul(&px);
        }
        Some(result)
    }
}

/// Render an `i64` in base ten (SQLite formats the significand with `%lld`).
fn int_to_string(m: i64) -> String {
    let mut s = String::new();
    if m < 0 {
        s.push('-');
    }
    // Build magnitude digits; handle i64::MIN without overflow via i128.
    let mut mag: u64 = (m as i128).unsigned_abs() as u64;
    if mag == 0 {
        s.push('0');
        return s;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while mag > 0 {
        i -= 1;
        buf[i] = b'0' + (mag % 10) as u8;
        mag /= 10;
    }
    s.push_str(core::str::from_utf8(&buf[i..]).unwrap());
    s
}

/// C `memcmp` over two digit slices: the sign of the first differing element
/// within the shared prefix, else 0.
fn cmp_digits(a: &[u8], b: &[u8]) -> i32 {
    let n = a.len().min(b.len());
    for i in 0..n {
        if a[i] != b[i] {
            return a[i] as i32 - b[i] as i32;
        }
    }
    0
}

/// Reduce an integer to its sign: `-1`, `0`, or `+1`.
fn sgn(x: i64) -> i32 {
    match x.cmp(&0) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(a: &str, b: &str) -> String {
        let mut x = Decimal::parse(a);
        x.add(Decimal::parse(b));
        x.to_decimal_string()
    }
    fn sub(a: &str, b: &str) -> String {
        let mut x = Decimal::parse(a);
        x.sub(Decimal::parse(b));
        x.to_decimal_string()
    }
    fn mul(a: &str, b: &str) -> String {
        let mut x = Decimal::parse(a);
        x.mul(&Decimal::parse(b));
        x.to_decimal_string()
    }
    fn cmp(a: &str, b: &str) -> i32 {
        Decimal::parse(a).compare(&Decimal::parse(b))
    }
    fn dec(a: &str) -> String {
        Decimal::parse(a).to_decimal_string()
    }

    #[test]
    fn canonicalize() {
        // Trailing zeros are canonical (decimal_result emits every stored
        // fractional digit) — verified byte-for-byte against sqlite3 3.50.4.
        assert_eq!(dec("007.50"), "7.50");
        assert_eq!(dec("0"), "0");
        assert_eq!(dec("000"), "0");
        assert_eq!(dec("-0"), "0");
        assert_eq!(dec("0.5"), "0.5");
        assert_eq!(dec(".5"), "0.5");
        assert_eq!(dec("100"), "100");
        assert_eq!(dec("1e3"), "1000");
        assert_eq!(dec("1.5e2"), "150");
        assert_eq!(dec("1500e-2"), "15.00");
        assert_eq!(dec("-0.0"), "0.0");
        assert_eq!(dec("  -42"), "-42");
    }

    #[test]
    fn addition() {
        assert_eq!(add("1.1", "2.2"), "3.3");
        assert_eq!(add("0.1", "0.2"), "0.3");
        assert_eq!(add("1e3", "2"), "1002");
        assert_eq!(add("-5", "3"), "-2");
        assert_eq!(add("5", "-3"), "2");
        assert_eq!(add("-5", "-3"), "-8");
        assert_eq!(add("100", "0.001"), "100.001");
    }

    #[test]
    fn subtraction() {
        assert_eq!(sub("0", "0.0001"), "-0.0001");
        assert_eq!(sub("3.3", "1.1"), "2.2");
        assert_eq!(sub("1", "1"), "0");
        // sqlite keeps the negative sign on a multi-digit zero result (the
        // sign is only cleared when nDigit<=1), so this is "-0", not "0".
        assert_eq!(sub("-1", "-1"), "-0");
        assert_eq!(sub("10", "20"), "-10");
    }

    #[test]
    fn multiplication() {
        assert_eq!(mul("1.5", "2"), "3");
        assert_eq!(mul("1.1", "1.1"), "1.21");
        assert_eq!(mul("-2", "3"), "-6");
        assert_eq!(mul("-2", "-3"), "6");
        assert_eq!(mul("0.1", "0.1"), "0.01");
        assert_eq!(
            mul("99999999999999999999", "99999999999999999999"),
            "9999999999999999999800000000000000000001"
        );
    }

    #[test]
    fn comparison() {
        assert_eq!(cmp("10", "9"), 1);
        assert_eq!(cmp("9", "10"), -1);
        // decimal_cmp is a total order on the *stored* digit arrays, so extra
        // trailing zeros compare as greater — "1.0" > "1". Matches sqlite.
        assert_eq!(cmp("1.0", "1"), 1);
        assert_eq!(cmp("1.0", "1.0"), 0);
        assert_eq!(cmp("-5", "3"), -1);
        assert_eq!(cmp("-5", "-3"), -1);
        assert_eq!(cmp("0.10", "0.1"), 1);
        assert_eq!(cmp("100.00", "100"), 1);
    }

    #[test]
    fn from_double_exact() {
        // 0.5 and 0.25 are exact in binary.
        assert_eq!(
            Decimal::from_double(0.5).unwrap().to_decimal_string(),
            "0.5"
        );
        assert_eq!(
            Decimal::from_double(0.25).unwrap().to_decimal_string(),
            "0.25"
        );
        assert_eq!(Decimal::from_double(2.0).unwrap().to_decimal_string(), "2");
        assert_eq!(Decimal::from_double(0.0).unwrap().to_decimal_string(), "0");
    }
}
