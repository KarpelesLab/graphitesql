//! Pure-`core` floating-point helpers.
//!
//! `f64` methods like `trunc`, `floor`, `round`, `abs`, and `powi` live in
//! `std` (they bottom out in `libm`), so they are unavailable under `#![no_std]`
//! without an external dependency. graphitesql forbids dependencies, so we
//! implement the handful we need here, in safe `core` arithmetic. These cover
//! the magnitudes SQLite cares about and handle NaN/∞ defensively.

/// Absolute value.
pub fn abs(x: f64) -> f64 {
    if x < 0.0 {
        -x
    } else {
        x
    }
}

/// Truncate toward zero.
pub fn trunc(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    // Any |x| >= 2^52 is already integral for f64.
    if abs(x) >= 4_503_599_627_370_496.0 {
        return x;
    }
    (x as i64) as f64
}

/// Largest integer `<= x`.
pub fn floor(x: f64) -> f64 {
    let t = trunc(x);
    if t > x {
        t - 1.0
    } else {
        t
    }
}

/// Round half away from zero (SQLite's `round()` rule).
pub fn round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let bump = if x < 0.0 { -0.5 } else { 0.5 };
    trunc(x + bump)
}

/// Floating remainder `x - trunc(x / y) * y` (avoids the `%` intrinsic).
pub fn fmod(x: f64, y: f64) -> f64 {
    if y == 0.0 || !x.is_finite() {
        return f64::NAN;
    }
    x - trunc(x / y) * y
}

/// Integer power `base^exp` for small non-negative `exp` (used by `round(x, n)`).
pub fn powi(base: f64, exp: i32) -> f64 {
    if exp < 0 {
        return 1.0 / powi(base, -exp);
    }
    let mut acc = 1.0;
    let mut b = base;
    let mut e = exp as u32;
    while e > 0 {
        if e & 1 == 1 {
            acc *= b;
        }
        b *= b;
        e >>= 1;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert_eq!(abs(-3.5), 3.5);
        assert_eq!(trunc(3.9), 3.0);
        assert_eq!(trunc(-3.9), -3.0);
        assert_eq!(floor(-3.1), -4.0);
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -3.0);
        assert_eq!(round(2.4), 2.0);
        assert_eq!(powi(10.0, 3), 1000.0);
        assert_eq!(powi(2.0, 10), 1024.0);
        assert_eq!(fmod(7.5, 2.0), 1.5);
    }
}
