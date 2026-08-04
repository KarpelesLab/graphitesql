//! The SQLite `ieee754` extension's decompose/recompose arithmetic — a faithful
//! port of `ext/misc/ieee754.c` (3.50.4). `ieee754(X)` splits a double into an
//! exact `mantissa · 2^exponent`; `ieee754(M, E)` reassembles it. The integer
//! arithmetic (including the signed shifts that give `-0.0` its `(1, -3071)`
//! form) mirrors the C exactly so the results byte-match sqlite.

/// Decompose `r` into `(mantissa, exponent)` such that `r == mantissa · 2^exponent`
/// (the values `ieee754(X)` prints and that `ieee754_mantissa`/`ieee754_exponent`
/// return). Ported from `ieee754func`'s one-argument branch.
pub fn parts(r_in: f64) -> (i64, i32) {
    // Only a genuinely-negative value is folded to positive; `-0.0` (which is not
    // `< 0.0`) keeps its sign bit and flows through the general path below,
    // reproducing sqlite's `ieee754(-0.0) = 'ieee754(1,-3071)'`.
    let is_neg = r_in < 0.0;
    let r = if is_neg { -r_in } else { r_in };
    // `memcpy(&a, &r, 8)` with a signed 64-bit `a`: the bit pattern, read as i64
    // (so a set sign bit — e.g. `-0.0` — makes `a` negative and `a >> 52`
    // sign-extends, exactly as the C relies on).
    let a = r.to_bits() as i64;
    if a == 0 {
        return (0, -1075);
    }
    let mut e: i64 = a >> 52; // arithmetic shift, matching `sqlite3_int64`
    let mut m: i64 = a & ((1i64 << 52) - 1);
    if e == 0 {
        m <<= 1;
    } else {
        m |= 1i64 << 52;
    }
    while e < 1075 && m > 0 && (m & 1) == 0 {
        m >>= 1;
        e += 1;
    }
    if is_neg {
        m = -m;
    }
    (m, (e - 1075) as i32)
}

/// Reassemble `mantissa · 2^exponent` into the nearest double, ported from
/// `ieee754func`'s two-argument branch. `None` mirrors the C's bare `return`
/// (no result → SQL NULL) for `mantissa == i64::MIN`.
pub fn compose(m_in: i64, e_in: i64) -> Option<f64> {
    let mut m = m_in;
    // Ticket 22dea1cfdb9151e4: clamp the exponent.
    let mut e = e_in.clamp(-10000, 10000);
    let mut is_neg = false;
    if m < 0 {
        is_neg = true;
        m = -m;
        if m < 0 {
            return None; // i64::MIN: negation overflowed
        }
    } else if m == 0 && e > -1000 && e < 1000 {
        return Some(0.0);
    }
    while (m >> 32) & 0xffe0_0000 != 0 {
        m >>= 1;
        e += 1;
    }
    while m != 0 && ((m >> 32) & 0xfff0_0000) == 0 {
        m <<= 1;
        e -= 1;
    }
    e += 1075;
    if e <= 0 {
        // Subnormal.
        if 1 - e >= 64 {
            m = 0;
        } else {
            m >>= 1 - e;
        }
        e = 0;
    } else if e > 0x7ff {
        e = 0x7ff;
    }
    let mut a = m & ((1i64 << 52) - 1);
    a |= e << 52;
    if is_neg {
        a |= 1i64 << 63; // (sqlite3_uint64)1 << 63
    }
    Some(f64::from_bits(a as u64))
}

/// Reconstruct a double from an 8-byte big-endian blob (`ieee754_from_blob`), or
/// `None` for a wrong-length input (the C sets no result → NULL).
pub fn from_blob(bytes: &[u8]) -> Option<f64> {
    let arr: [u8; 8] = bytes.try_into().ok()?;
    Some(f64::from_bits(u64::from_be_bytes(arr)))
}

/// The 8-byte big-endian blob of a double (`ieee754_to_blob`).
pub fn to_blob(r: f64) -> [u8; 8] {
    r.to_bits().to_be_bytes()
}

/// `ieee754_inc(r, n)`: the double whose bit pattern is `r`'s plus `n` (wrapping),
/// i.e. `n` representable steps along the number line. Ported from `ieee754inc`.
pub fn inc(r: f64, n: i64) -> f64 {
    f64::from_bits(r.to_bits().wrapping_add(n as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompose_matches_sqlite() {
        assert_eq!(parts(45.25), (181, -2));
        assert_eq!(parts(2.0), (2, 0));
        assert_eq!(parts(0.0), (0, -1075));
        assert_eq!(parts(-0.0), (1, -3071));
        assert_eq!(parts(1e308), (5010420900022432, 971));
    }

    #[test]
    fn recompose_matches_sqlite() {
        assert_eq!(compose(181, -2), Some(45.25));
        assert_eq!(compose(2, 0), Some(2.0));
        assert_eq!(compose(1, 0), Some(1.0));
    }

    #[test]
    fn blob_roundtrip() {
        assert_eq!(to_blob(1.0), [0x3f, 0xf0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(from_blob(&to_blob(45.25)), Some(45.25));
        assert_eq!(from_blob(&[0, 1, 2]), None);
    }
}
