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

/// π.
pub const PI: f64 = core::f64::consts::PI;
/// natural log of 2.
const LN2: f64 = core::f64::consts::LN_2;
/// natural log of 10.
const LN10: f64 = core::f64::consts::LN_10;

/// Smallest integer `>= x`.
pub fn ceil(x: f64) -> f64 {
    -floor(-x)
}

/// Square root via Newton–Raphson after reducing the mantissa into `[1, 4)`.
/// Full `f64` precision (the iteration is self-correcting).
pub fn sqrt(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 || !x.is_finite() {
        return x; // ±0 or +∞
    }
    // Reduce x = m * 4^e with m in [1, 4), so sqrt(x) = sqrt(m) * 2^e.
    let mut m = x;
    let mut e: i32 = 0;
    while m >= 4.0 {
        m /= 4.0;
        e += 1;
    }
    while m < 1.0 {
        m *= 4.0;
        e -= 1;
    }
    let mut y = m; // converges from any positive seed
    for _ in 0..40 {
        let ny = 0.5 * (y + m / y);
        if ny == y {
            break;
        }
        y = ny;
    }
    // Newton can land one ULP off; nudge `y` to the correctly-rounded root by
    // comparing the exact residual y² − m (via an error-free product) against
    // those of its neighbors.
    y = round_sqrt(m, y);
    y * powi(2.0, e)
}

/// Veltkamp split of a finite `f64` into hi+lo with non-overlapping mantissas.
fn split(a: f64) -> (f64, f64) {
    let c = 134_217_729.0 * a; // 2^27 + 1
    let hi = c - (c - a);
    (hi, a - hi)
}

/// Error-free product: returns `(p, e)` with `p = fl(a*b)` and `a*b = p + e`.
fn two_prod(a: f64, b: f64) -> (f64, f64) {
    let p = a * b;
    let (ah, al) = split(a);
    let (bh, bl) = split(b);
    let e = ((ah * bh - p) + ah * bl + al * bh) + al * bl;
    (p, e)
}

/// Pick the correctly-rounded square root among `y` and its f64 neighbors.
fn round_sqrt(m: f64, y: f64) -> f64 {
    let up = f64::from_bits(y.to_bits() + 1);
    let down = f64::from_bits(y.to_bits().wrapping_sub(1));
    // Candidates bracket the true root; choose the one with the smallest
    // magnitude residual (ties resolved toward even via bit parity is overkill
    // here — adjacent doubles rarely tie for sqrt).
    let mut best = y;
    let mut best_err = abs_residual(m, y);
    for &c in &[down, up] {
        if c > 0.0 && c.is_finite() {
            let err = abs_residual(m, c);
            if err < best_err {
                best_err = err;
                best = c;
            }
        }
    }
    best
}

/// |y² − m| computed with the error-free product (extended precision).
fn abs_residual(m: f64, y: f64) -> f64 {
    let (p, e) = two_prod(y, y);
    abs((p - m) + e)
}

/// e^x via range reduction `x = k·ln2 + r` and a Taylor series on `r`.
pub fn exp(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x == f64::INFINITY {
        return x;
    }
    if x == f64::NEG_INFINITY {
        return 0.0;
    }
    let k = round(x / LN2);
    let r = x - k * LN2; // |r| <= ln2/2 ≈ 0.347
                         // Taylor: sum r^n / n!
    let mut term = 1.0;
    let mut sum = 1.0;
    for n in 1..20 {
        term *= r / n as f64;
        sum += term;
    }
    sum * powi(2.0, k as i32)
}

/// Natural logarithm via mantissa reduction and the `atanh` series.
pub fn ln(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x == f64::INFINITY {
        return x;
    }
    // Reduce x = m * 2^e with m in [√0.5, √2), so the series argument is small.
    let mut m = x;
    let mut e: i32 = 0;
    while m >= core::f64::consts::SQRT_2 {
        m /= 2.0;
        e += 1;
    }
    while m < core::f64::consts::FRAC_1_SQRT_2 {
        m *= 2.0;
        e -= 1;
    }
    // ln(m) = 2·(s + s³/3 + s⁵/5 + …), s = (m-1)/(m+1), |s| < 0.18.
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let mut term = s;
    let mut sum = s;
    for k in 1..30 {
        term *= s2;
        sum += term / (2 * k + 1) as f64;
    }
    2.0 * sum + e as f64 * LN2
}

/// Base-10 logarithm.
pub fn log10(x: f64) -> f64 {
    ln(x) / LN10
}

/// Base-2 logarithm.
pub fn log2(x: f64) -> f64 {
    ln(x) / LN2
}

/// `base^exp` for real arguments (SQLite `pow`/`power` semantics for the common
/// cases): exact integer powers when `exp` is integral, else `exp(exp·ln base)`.
pub fn pow(base: f64, y: f64) -> f64 {
    if y == 0.0 || base == 1.0 {
        return 1.0;
    }
    if base.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    // Integral exponent: exact (and handles negative bases).
    if y == trunc(y) && abs(y) <= 1024.0 {
        return powi(base, y as i32);
    }
    if base < 0.0 {
        return f64::NAN; // non-integer power of a negative number
    }
    if base == 0.0 {
        return 0.0;
    }
    exp(y * ln(base))
}

/// Sine. Argument reduced modulo π/2 with a Taylor kernel on `[-π/4, π/4]`.
pub fn sin(x: f64) -> f64 {
    if !x.is_finite() {
        return f64::NAN;
    }
    let (k, r) = reduce_quarter_pi(x);
    match k & 3 {
        0 => sin_kernel(r),
        1 => cos_kernel(r),
        2 => -sin_kernel(r),
        _ => -cos_kernel(r),
    }
}

/// Cosine.
pub fn cos(x: f64) -> f64 {
    if !x.is_finite() {
        return f64::NAN;
    }
    let (k, r) = reduce_quarter_pi(x);
    match k & 3 {
        0 => cos_kernel(r),
        1 => -sin_kernel(r),
        2 => -cos_kernel(r),
        _ => sin_kernel(r),
    }
}

/// Tangent.
pub fn tan(x: f64) -> f64 {
    sin(x) / cos(x)
}

/// Reduce `x` to `k·(π/2) + r` with `|r| <= π/4`, returning `(k, r)`.
fn reduce_quarter_pi(x: f64) -> (i64, f64) {
    let half_pi = PI / 2.0;
    let k = round(x / half_pi);
    let r = x - k * half_pi;
    (k as i64, r)
}

fn sin_kernel(r: f64) -> f64 {
    // r - r³/3! + r⁵/5! - …
    let r2 = r * r;
    let mut term = r;
    let mut sum = r;
    for n in 1..12 {
        term *= -r2 / ((2 * n) as f64 * (2 * n + 1) as f64);
        sum += term;
    }
    sum
}

fn cos_kernel(r: f64) -> f64 {
    // 1 - r²/2! + r⁴/4! - …
    let r2 = r * r;
    let mut term = 1.0;
    let mut sum = 1.0;
    for n in 1..12 {
        term *= -r2 / ((2 * n - 1) as f64 * (2 * n) as f64);
        sum += term;
    }
    sum
}

/// Arctangent, via argument halving `atan(x)=2·atan(x/(1+√(1+x²)))` to shrink
/// `|x|` below ~0.1, then a Taylor series.
pub fn atan(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x == f64::INFINITY {
        return PI / 2.0;
    }
    if x == f64::NEG_INFINITY {
        return -PI / 2.0;
    }
    let neg = x < 0.0;
    let mut a = abs(x);
    let mut halvings = 0u32;
    while a > 0.1 {
        a = a / (1.0 + sqrt(1.0 + a * a));
        halvings += 1;
    }
    // Taylor: a - a³/3 + a⁵/5 - …
    let a2 = a * a;
    let mut term = a;
    let mut sum = a;
    for k in 1..30 {
        term *= -a2;
        sum += term / (2 * k + 1) as f64;
    }
    let mut result = sum * powi(2.0, halvings as i32);
    if neg {
        result = -result;
    }
    result
}

/// Two-argument arctangent with quadrant resolution.
pub fn atan2(y: f64, x: f64) -> f64 {
    if x > 0.0 {
        atan(y / x)
    } else if x < 0.0 {
        if y >= 0.0 {
            atan(y / x) + PI
        } else {
            atan(y / x) - PI
        }
    } else {
        // x == 0
        if y > 0.0 {
            PI / 2.0
        } else if y < 0.0 {
            -PI / 2.0
        } else {
            0.0
        }
    }
}

/// Arcsine, `asin(x) = atan(x / √(1−x²))` with endpoint handling.
pub fn asin(x: f64) -> f64 {
    if x.is_nan() || !(-1.0..=1.0).contains(&x) {
        return f64::NAN;
    }
    if x == 1.0 {
        return PI / 2.0;
    }
    if x == -1.0 {
        return -PI / 2.0;
    }
    atan(x / sqrt(1.0 - x * x))
}

/// Arccosine.
pub fn acos(x: f64) -> f64 {
    if x.is_nan() || !(-1.0..=1.0).contains(&x) {
        return f64::NAN;
    }
    PI / 2.0 - asin(x)
}

/// Hyperbolic sine.
pub fn sinh(x: f64) -> f64 {
    let e = exp(x);
    (e - 1.0 / e) / 2.0
}

/// Hyperbolic cosine.
pub fn cosh(x: f64) -> f64 {
    let e = exp(x);
    (e + 1.0 / e) / 2.0
}

/// Hyperbolic tangent.
pub fn tanh(x: f64) -> f64 {
    if x > 20.0 {
        return 1.0;
    }
    if x < -20.0 {
        return -1.0;
    }
    let e2 = exp(2.0 * x);
    (e2 - 1.0) / (e2 + 1.0)
}

/// Inverse hyperbolic sine.
pub fn asinh(x: f64) -> f64 {
    ln(x + sqrt(x * x + 1.0))
}

/// Inverse hyperbolic cosine.
pub fn acosh(x: f64) -> f64 {
    if x < 1.0 {
        return f64::NAN;
    }
    ln(x + sqrt(x * x - 1.0))
}

/// Inverse hyperbolic tangent.
pub fn atanh(x: f64) -> f64 {
    if x <= -1.0 || x >= 1.0 {
        return if x == 1.0 {
            f64::INFINITY
        } else if x == -1.0 {
            f64::NEG_INFINITY
        } else {
            f64::NAN
        };
    }
    0.5 * ln((1.0 + x) / (1.0 - x))
}

/// Degrees from radians.
pub fn degrees(x: f64) -> f64 {
    x * 180.0 / PI
}

/// Radians from degrees.
pub fn radians(x: f64) -> f64 {
    x * PI / 180.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() <= 1e-10 * (1.0 + b.abs()), "{a} vs {b}");
    }

    #[test]
    #[allow(clippy::approx_constant)] // literal reference values for known angles
    fn transcendental() {
        close(sqrt(2.0), core::f64::consts::SQRT_2);
        close(sqrt(1e300), 1e150);
        close(exp(1.0), core::f64::consts::E);
        close(ln(core::f64::consts::E), 1.0);
        close(ln(1000.0), 6.907_755_278_982_137);
        close(log10(1000.0), 3.0);
        close(log2(1024.0), 10.0);
        close(pow(2.0, 0.5), core::f64::consts::SQRT_2);
        close(pow(9.0, 0.5), 3.0);
        close(sin(1.0), 0.841_470_984_807_896_5);
        close(cos(1.0), 0.540_302_305_868_139_8);
        close(tan(1.0), 1.557_407_724_654_902_3);
        close(sin(10.0), -0.544_021_110_889_369_8);
        close(atan(1.0), PI / 4.0);
        close(atan2(1.0, 1.0), PI / 4.0);
        close(asin(0.5), 0.523_598_775_598_298_9);
        close(acos(0.5), 1.047_197_551_196_597_7);
        close(sinh(1.0), 1.175_201_193_643_801_4);
        close(cosh(1.0), 1.543_080_634_815_243_7);
        close(tanh(0.5), 0.462_117_157_260_009_8);
        close(asinh(1.0), 0.881_373_587_019_543);
        close(acosh(2.0), 1.316_957_896_924_816_7);
        close(atanh(0.5), 0.549_306_144_334_054_8);
        close(degrees(PI), 180.0);
        close(radians(180.0), PI);
        close(ceil(2.1), 3.0);
        close(ceil(-2.1), -2.0);
    }

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
