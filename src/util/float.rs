//! Pure-`core` floating-point helpers.
//!
//! `f64` methods like `trunc`, `floor`, `round`, `abs`, and `powi` live in
//! `std` (they bottom out in `libm`), so they are unavailable under `#![no_std]`
//! without an external dependency. graphitesql forbids dependencies, so we
//! implement the handful we need here, in safe `core` arithmetic. These cover
//! the magnitudes SQLite cares about and handle NaN/∞ defensively.

/// Absolute value.
pub fn abs(x: f64) -> f64 {
    if x < 0.0 { -x } else { x }
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
    if t > x { t - 1.0 } else { t }
}

/// Round half away from zero (SQLite's `round()` rule).
pub fn round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let bump = if x < 0.0 { -0.5 } else { 0.5 };
    trunc(x + bump)
}

/// IEEE floating remainder: `x - n·y` where `n = trunc(x/y)`, carrying the sign
/// of `x` (matching C `fmod`, which is what SQLite's `mod()` uses).
///
/// The naive `x - trunc(x/y)·y` is wrong whenever `x/y` overflows (e.g.
/// `mod(1e308, 1e-300)` would yield `±∞` instead of the true remainder), so this
/// reduces `|x|` by repeatedly subtracting `y` scaled up by powers of two — an
/// exact, overflow-free long division that reproduces glibc's result bit-for-bit.
pub fn fmod(x: f64, y: f64) -> f64 {
    if y == 0.0 || !x.is_finite() || y.is_nan() {
        return f64::NAN;
    }
    if y.is_infinite() {
        return x;
    }
    let sign = x < 0.0;
    let mut a = abs(x);
    let b = abs(y);
    if a < b {
        return x; // remainder is x itself
    }
    // Subtract the largest `b·2^k <= a` repeatedly. Doubling `b` is exact, and we
    // stop before `scaled + scaled` would exceed `a`, so nothing overflows.
    while a >= b {
        let mut scaled = b;
        while scaled + scaled <= a {
            scaled += scaled;
        }
        a -= scaled;
    }
    if sign { -a } else { a }
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
/// 1/ln2, for `exp`'s range reduction.
const INV_LN2: f64 = core::f64::consts::LOG2_E;
/// High part of ln2 (low 27 mantissa bits cleared) for accurate reduction.
const LN2_HI: f64 = 0.693_147_167_563_438_4;
/// Low part: `LN2 - LN2_HI`, so `LN2_HI + LN2_LO == LN2` to full precision.
const LN2_LO: f64 = 1.299_650_689_388_988_9e-8;

/// Scale `x` by `2^k`, preserving subnormals and overflow the way IEEE `ldexp`
/// does. A single `x * 2^k` would flush an overflowing or underflowing power to
/// ±∞ / 0 before the multiply; splitting `k` keeps the gradual-underflow tail
/// (so `exp(-745)` lands on the smallest subnormal, matching SQLite) and lets a
/// genuine overflow become ±∞ rather than NaN.
fn ldexp(x: f64, k: i32) -> f64 {
    if k > 1023 {
        let mut y = x * powi(2.0, 1023);
        let mut rem = k - 1023;
        while rem > 1023 {
            y *= powi(2.0, 1023);
            rem -= 1023;
        }
        y * powi(2.0, rem)
    } else if k < -1022 {
        let mut y = x * powi(2.0, -1022);
        let mut rem = k + 1022;
        while rem < -1022 {
            y *= powi(2.0, -1022);
            rem += 1022;
        }
        y * powi(2.0, rem)
    } else {
        x * powi(2.0, k)
    }
}

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
    // Scale back to full magnitude, then take one Heron step *at that scale* using
    // an error-free residual `y² − x`. Iterating on the reduced `m` and merely
    // scaling leaves `y` up to several ulps off at the result's true magnitude
    // (one ulp at the reduced scale is a different absolute size), and a ±1-ulp
    // nudge can't recover that. The corrected `y` is then within one ulp, so the
    // final `round_sqrt` against `x` lands on the correctly-rounded root.
    let mut full = y * powi(2.0, e);
    let (p, err) = two_prod(full, full);
    let resid = (p - x) + err; // full² − x to extra precision
    full -= resid / (2.0 * full);
    round_sqrt(x, full)
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

/// Pick the correctly-rounded square root of `x` among `y` and its f64 neighbors.
fn round_sqrt(x: f64, y: f64) -> f64 {
    let up = f64::from_bits(y.to_bits() + 1);
    let down = f64::from_bits(y.to_bits().wrapping_sub(1));
    // Candidates bracket the true root; choose the one with the smallest
    // magnitude residual (ties resolved toward even via bit parity is overkill
    // here — adjacent doubles rarely tie for sqrt).
    let mut best = y;
    let mut best_err = abs_residual(x, y);
    for &c in &[down, up] {
        if c > 0.0 && c.is_finite() {
            let err = abs_residual(x, c);
            if err < best_err {
                best_err = err;
                best = c;
            }
        }
    }
    best
}

/// |y² − x| computed with the error-free product (extended precision).
fn abs_residual(x: f64, y: f64) -> f64 {
    let (p, e) = two_prod(y, y);
    abs((p - x) + e)
}

/// e^x via range reduction `x = k·ln2 + r` and a Taylor series on `r`.
///
/// `k = round(x/ln2)` leaves `|r| <= ln2/2 ≈ 0.347`. The reduction subtracts
/// `ln2` in two halves (`LN2_HI + LN2_LO`) so the cancellation in `x - k·ln2`
/// stays accurate even when `k` is large (≈1023 near the overflow edge); a
/// single rounded `ln2` would lose ~1 ulp there. The kernel sums `exp(r) - 1`
/// (so the leading `1.0` is added once, last, rather than swamping the tail),
/// then `ldexp` applies `2^k` while preserving subnormals/overflow.
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
    // Past the finite range of `exp` the result is `+∞` / `0`; short-circuit so the
    // huge `k` can't overflow the `i32` exponent in the scaling below.
    // `exp(x)` overflows for x > ~709.78 and underflows to 0 for x < ~-745.13.
    if x > 709.8 {
        return f64::INFINITY;
    }
    if x < -745.2 {
        return 0.0;
    }
    let k = round(x * INV_LN2);
    let r = (x - k * LN2_HI) - k * LN2_LO; // |r| <= ln2/2 ≈ 0.347
    // Taylor for exp(r) - 1: sum_{n>=1} r^n / n!
    let mut term = r;
    let mut sum = r;
    for n in 2..18 {
        term *= r / n as f64;
        sum += term;
    }
    ldexp(1.0 + sum, k as i32)
}

/// Natural logarithm via mantissa reduction and the `atanh` series.
///
/// Non-positive inputs are a *domain error*: `ln(x)` for `x <= 0` returns `NaN`
/// (which the dispatch layer maps to SQL `NULL`, matching SQLite's `ln(0)` and
/// `ln(-1)`), rather than the mathematical `-∞` limit at zero.
pub fn ln(x: f64) -> f64 {
    if x.is_nan() || x <= 0.0 {
        return f64::NAN;
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
    let integral = y == trunc(y);
    // Integral exponent within i32: exact via binary exponentiation (also handles
    // negative bases and the `0^negative` pole, which yields ±∞).
    if integral && abs(y) <= 1024.0 {
        return powi(base, y as i32);
    }
    // Half-integer powers of a non-negative base go through the correctly-rounded
    // `sqrt`, which is ~1 ulp better than `exp(y·ln base)` for `x**0.5` (so the
    // 15-significant-digit rendering matches SQLite, e.g. `pow(2,0.5)`).
    if base >= 0.0 {
        if y == 0.5 {
            return sqrt(base);
        }
        if y == -0.5 && base != 0.0 {
            // `sqrt(1/b)` is ~1 ulp closer than `1/sqrt(b)` (SQLite agrees).
            return sqrt(1.0 / base);
        }
    }
    if base < 0.0 {
        // A negative base to a *non*-integer power is a domain error (NaN/NULL);
        // an integral power beyond the `powi` range still has a well-defined sign
        // from the exponent's parity, e.g. `pow(-2, 2000) = +Inf`,
        // `pow(-2, 1025) = -Inf`.
        if !integral {
            return f64::NAN;
        }
        let mag = exp(y * ln(-base));
        // Even exponent ⇒ positive, odd ⇒ negative. `y` is integral and huge, so
        // halving it stays exact; its low bit is the parity.
        return if fmod(y, 2.0) == 0.0 { mag } else { -mag };
    }
    if base == 0.0 {
        // `0^positive = 0`; `0^negative` is a pole ⇒ `+∞` (matching SQLite, which
        // reports `pow(0, -0.5) = Inf`). Integral negative `y` already went
        // through the `powi` pole above; this covers the fractional case.
        return if y < 0.0 { f64::INFINITY } else { 0.0 };
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

/// π/2 split into three doubles (`PIO2_1` has its low 32 mantissa bits cleared)
/// so that `k·(π/2)` can be subtracted from `x` with almost no rounding loss
/// during the Cody–Waite range reduction below.
const PIO2_1: f64 = 1.570796012878418;
const PIO2_2: f64 = 3.139164164167596e-7;
const PIO2_3: f64 = 6.223372171896613e-14;

/// Reduce `x` to `k·(π/2) + r` with `|r| <= π/4`, returning `(k, r)`.
///
/// Subtracting a single rounded `π/2` loses one bit of `r` per bit of `k`, which
/// wrecks the last few significant digits of `sin`/`cos` for arguments more than
/// a few multiples of π. Instead this uses Cody–Waite reduction with π/2 split
/// into `PIO2_1 + PIO2_2 + PIO2_3`, and forms `k·PIO2_1` as an error-free product
/// so the leading cancellation `x − k·PIO2_1` carries no rounding error.
fn reduce_quarter_pi(x: f64) -> (i64, f64) {
    let k = round(x * (2.0 / PI));
    let (p1, e1) = two_prod(k, PIO2_1);
    let r = ((x - p1) - e1) - k * PIO2_2;
    let r = r - k * PIO2_3;
    (k as i64, r)
}

fn sin_kernel(r: f64) -> f64 {
    // r - r³/3! + r⁵/5! - …, summed with Kahan compensation to recover the
    // low-order bits the running sum would otherwise drop.
    let r2 = r * r;
    let mut term = r;
    let mut sum = r;
    let mut c = 0.0;
    for n in 1..13 {
        term *= -r2 / ((2 * n) as f64 * (2 * n + 1) as f64);
        let y = term - c;
        let t = sum + y;
        c = (t - sum) - y;
        sum = t;
    }
    sum
}

fn cos_kernel(r: f64) -> f64 {
    // 1 - r²/2! + r⁴/4! - …, Kahan-compensated.
    let r2 = r * r;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut c = 0.0;
    for n in 1..13 {
        term *= -r2 / ((2 * n - 1) as f64 * (2 * n) as f64);
        let y = term - c;
        let t = sum + y;
        c = (t - sum) - y;
        sum = t;
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
    // For any |x| ≳ 2⁵³ the true `atan(x)` is within half a ULP of ±π/2, so it
    // rounds to exactly ±π/2. The half-angle reduction below squares `a`, which
    // overflows to +∞ once |x| ≳ 1.3e154 (`a·a = +∞` ⇒ `a/(1+√(1+∞)) = 0`,
    // collapsing the whole reduction to 0). Short-circuit that range directly.
    if a > 1e154 {
        return if neg { -PI / 2.0 } else { PI / 2.0 };
    }
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
///
/// Follows IEEE signed-zero conventions (matching SQLite/`atan2(3)`): the sign
/// bits of `y` *and* `x` — including `-0.0` — select the branch, so e.g.
/// `atan2(-0.0, -1) = -π` and `atan2(0.0, -0.0) = +π`.
pub fn atan2(y: f64, x: f64) -> f64 {
    // The ±π branch on the negative-x axis is driven by sign *bits*, since
    // `-0.0 < 0.0` is false yet `-0.0` lies on the negative side.
    let y_neg = y.is_sign_negative();
    let x_neg = x.is_sign_negative();
    if x > 0.0 {
        atan(y / x)
    } else if x < 0.0 {
        if y_neg {
            atan(y / x) - PI
        } else {
            atan(y / x) + PI
        }
    } else if y > 0.0 {
        PI / 2.0
    } else if y < 0.0 {
        -PI / 2.0
    } else {
        // y is ±0 and x is ±0. The sign of x's zero selects the axis (−0 ⇒ ±π),
        // the sign of y's zero selects the sign of the result.
        match (y_neg, x_neg) {
            (false, false) => 0.0,
            (true, false) => -0.0,
            (false, true) => PI,
            (true, true) => -PI,
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

/// Degrees from radians. Grouped as `x · (180/π)` to match SQLite bit-for-bit.
pub fn degrees(x: f64) -> f64 {
    x * (180.0 / PI)
}

/// Radians from degrees. Grouped as `x · (π/180)` to match SQLite bit-for-bit
/// (a left-to-right `x·π/180` rounds the intermediate `x·π` and can differ in
/// the last digit).
pub fn radians(x: f64) -> f64 {
    x * (PI / 180.0)
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

    #[test]
    fn fmod_is_overflow_free() {
        // Normal cases keep C-`fmod` semantics (sign of the dividend).
        assert_eq!(fmod(7.5, 2.0), 1.5);
        assert_eq!(fmod(-7.5, 2.0), -1.5);
        assert_eq!(fmod(7.5, -2.0), 1.5);
        assert_eq!(fmod(1e10, 3.0), 1.0);
        assert_eq!(fmod(3.0, 7.0), 3.0);
        // A division `x/y` that would overflow no longer poisons the result to
        // `±∞`; the true (glibc-equal) remainder is returned.
        assert_eq!(fmod(1e308, 1e-300), 1e308 % 1e-300);
        assert!(fmod(1e308, 1e-300).is_finite());
        // Domain: zero divisor and non-finite dividend are NaN (⇒ SQL NULL).
        assert!(fmod(5.0, 0.0).is_nan());
        assert!(fmod(f64::INFINITY, 2.0).is_nan());
        assert_eq!(fmod(2.0, f64::INFINITY), 2.0);
    }

    #[test]
    fn exp_overflow_underflow() {
        // A large argument overflows to +∞ (SQLite renders this as `Inf`), the
        // mirror underflows to the gradual-underflow tail rather than NaN.
        assert!(exp(710.0).is_infinite() && exp(710.0) > 0.0);
        assert!(exp(1000.0).is_infinite());
        // A huge argument must not overflow the internal i32 exponent cast.
        assert!(exp(1e308).is_infinite());
        assert_eq!(exp(-1000.0), 0.0);
        assert_eq!(exp(-1e308), 0.0);
        // `exp(-745)` lands on the smallest positive subnormal (matches SQLite's
        // `4.94065645841247e-324`); `exp(-746)` flushes to exactly 0.
        assert_eq!(exp(-745.0), f64::from_bits(1));
        assert_eq!(exp(-746.0), 0.0);
        // Edge of the finite range stays finite and accurate.
        assert!(exp(709.0).is_finite());
        close(exp(709.0), 8.218_407_461_554_972e307);
        // Improved accuracy: these now round to SQLite's 15-significant-digit
        // output (previously ~1 ulp low).
        close(exp(1.0), core::f64::consts::E);
    }

    #[test]
    fn ln_domain_is_nan() {
        // `ln(x <= 0)` is a domain error (NaN ⇒ SQL NULL), not the `-∞` limit, so
        // the dispatch layer reports NULL exactly like SQLite's `ln(0)`/`ln(-1)`.
        assert!(ln(0.0).is_nan());
        assert!(ln(-1.0).is_nan());
        assert!(log2(0.0).is_nan());
        assert!(log10(0.0).is_nan());
    }

    #[test]
    fn pow_edges() {
        // Poles and overflow yield ±∞; a negative base keeps its parity sign even
        // beyond the exact-`powi` range.
        assert!(pow(0.0, -1.0).is_infinite() && pow(0.0, -1.0) > 0.0);
        assert!(pow(0.0, -0.5).is_infinite() && pow(0.0, -0.5) > 0.0);
        assert_eq!(pow(0.0, 0.5), 0.0);
        assert!(pow(2.0, 2000.0).is_infinite());
        assert!(pow(-2.0, 2000.0).is_infinite() && pow(-2.0, 2000.0) > 0.0);
        assert!(pow(-2.0, 1025.0).is_infinite() && pow(-2.0, 1025.0) < 0.0);
        // Non-integer power of a negative base is a domain error.
        assert!(pow(-8.0, 1.0 / 3.0).is_nan());
        // `x**0.5` routed through the correctly-rounded sqrt.
        assert_eq!(pow(2.0, 0.5), sqrt(2.0));
        assert_eq!(pow(0.5, -0.5), sqrt(2.0));
        assert_eq!(pow(0.0, 0.0), 1.0);
    }

    #[test]
    fn atan2_signed_zero() {
        // IEEE signed-zero conventions: the sign bits of both arguments select
        // the branch (matches SQLite / atan2(3)).
        close(atan2(-0.0, -1.0), -PI);
        close(atan2(0.0, -1.0), PI);
        close(atan2(0.0, -0.0), PI);
        close(atan2(-0.0, -0.0), -PI);
        // On the +x axis the result is ±0 (rendered as `0.0` either way).
        assert_eq!(atan2(-0.0, 1.0), 0.0);
        assert_eq!(atan2(0.0, 1.0), 0.0);
    }

    #[test]
    fn sqrt_correctly_rounded() {
        // A case the previous reduced-scale rounding missed by one ulp.
        assert_eq!(sqrt(5.740_547_787_712_544e29), 757_664_027_634_448.5);
        assert_eq!(sqrt(4.0), 2.0);
        assert_eq!(sqrt(0.0), 0.0);
        assert!(sqrt(-1.0).is_nan());
    }
}
