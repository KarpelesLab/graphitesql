//! The percentile aggregate family — a `no_std` port of SQLite's
//! `ext/misc/percentile.c` (the loadable `percentile` extension).
//!
//! Four related aggregates share one implementation, differing only in the
//! fraction they target and whether they interpolate:
//!
//! | function          | args | `mxFrac` | interpolates |
//! |-------------------|------|----------|--------------|
//! | `median`          | 1    | —        | yes (fraction fixed at 0.5) |
//! | `percentile`      | 2    | 100      | yes          |
//! | `percentile_cont` | 2    | 1        | yes          |
//! | `percentile_disc` | 2    | 1        | no (returns an actual input) |
//!
//! Each collects the non-NULL numeric inputs, sorts them ascending, and reads
//! off the value at fractional position `P·(N−1)`. The *continuous* forms
//! linearly interpolate between the two straddling samples; the *discrete* form
//! returns the lower sample verbatim. The result is always a REAL (or NULL for
//! an empty input) — even `percentile_disc`, which returns one of the inputs.
//!
//! This module holds only the pure sort-and-compute math; the SQL-level value
//! collection, numeric-type validation, and error reporting live in the
//! executor (`exec::eval::PercentileAcc`), which drives both aggregate engines.
//!
//! Reference: SQLite 3.50.4 `ext/misc/percentile.c`.

/// Which member of the percentile family an aggregate call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PercentileKind {
    /// `median(Y)` — equivalent to `percentile_cont(Y, 0.5)`.
    Median,
    /// `percentile(Y, P)` — `P` on a 0..100 scale.
    Percentile,
    /// `percentile_cont(Y, P)` — `P` on a 0..1 scale, interpolating.
    PercentileCont,
    /// `percentile_disc(Y, P)` — `P` on a 0..1 scale, no interpolation.
    PercentileDisc,
}

impl PercentileKind {
    /// Recognize one of the four percentile-family names (case-insensitively),
    /// or `None` for any other function name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "median" => Some(PercentileKind::Median),
            "percentile" => Some(PercentileKind::Percentile),
            "percentile_cont" => Some(PercentileKind::PercentileCont),
            "percentile_disc" => Some(PercentileKind::PercentileDisc),
            _ => None,
        }
    }

    /// The lower-case function name, for error messages.
    pub fn name(self) -> &'static str {
        match self {
            PercentileKind::Median => "median",
            PercentileKind::Percentile => "percentile",
            PercentileKind::PercentileCont => "percentile_cont",
            PercentileKind::PercentileDisc => "percentile_disc",
        }
    }

    /// The declared argument count: `median` takes 1, the rest take 2.
    pub fn n_arg(self) -> usize {
        match self {
            PercentileKind::Median => 1,
            _ => 2,
        }
    }

    /// The scale the fraction argument is divided by: 100 for `percentile`,
    /// 1 for the rest (`median` never reads a fraction argument).
    pub fn mx_frac(self) -> f64 {
        match self {
            PercentileKind::Percentile => 100.0,
            _ => 1.0,
        }
    }

    /// Whether this is the *discrete* form (returns an actual input, no
    /// interpolation).
    pub fn is_discrete(self) -> bool {
        matches!(self, PercentileKind::PercentileDisc)
    }
}

/// Compute the percentile value from `a`, the non-empty ascending-sorted array
/// of non-NULL inputs, at fraction `rpct` in `0.0..=1.0`.
///
/// `discrete` selects `percentile_disc`'s no-interpolation behavior. Mirrors the
/// final block of `percentComputeFinal` in `ext/misc/percentile.c`.
///
/// # Panics
/// Panics if `a` is empty; callers return NULL for an empty group before
/// reaching here.
pub fn compute(a: &[f64], rpct: f64, discrete: bool) -> f64 {
    let n = a.len();
    // Fractional index into the sorted array: rPct * (N-1).
    let ix = rpct * (n - 1) as f64;
    // Floor of ix. rPct is in [0,1] and N>=1, so ix is in [0, N-1] and the
    // truncating cast is a true floor.
    let i1 = ix as usize;
    if discrete {
        a[i1]
    } else {
        // The upper straddling sample: the same index when ix is integral or we
        // are already at the last element, otherwise the next one.
        let i2 = if ix == i1 as f64 || i1 == n - 1 {
            i1
        } else {
            i1 + 1
        };
        a[i1] + (a[i2] - a[i1]) * (ix - i1 as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_even() {
        // {1,2,3,4} median = 2.5 (interpolated between a[1] and a[2]).
        assert_eq!(compute(&[1.0, 2.0, 3.0, 4.0], 0.5, false), 2.5);
    }

    #[test]
    fn median_odd() {
        // {1,2,3} median = 2.0 (exact middle sample).
        assert_eq!(compute(&[1.0, 2.0, 3.0], 0.5, false), 2.0);
    }

    #[test]
    fn endpoints() {
        let a = [10.0, 20.0, 30.0, 40.0];
        assert_eq!(compute(&a, 0.0, false), 10.0);
        assert_eq!(compute(&a, 1.0, false), 40.0);
    }

    #[test]
    fn discrete_takes_lower_sample() {
        // percentile_disc(_, 0.5) over {1,2,3,4}: ix=1.5, floor=1 -> a[1]=2.
        assert_eq!(compute(&[1.0, 2.0, 3.0, 4.0], 0.5, true), 2.0);
        // The continuous form interpolates to 2.5 for the same input.
        assert_eq!(compute(&[1.0, 2.0, 3.0, 4.0], 0.5, false), 2.5);
    }

    #[test]
    fn single_value() {
        assert_eq!(compute(&[7.0], 0.5, false), 7.0);
        assert_eq!(compute(&[7.0], 1.0, true), 7.0);
    }

    #[test]
    fn quartiles() {
        // {1,2,3,4,5}: 25th pct ix=1.0 -> 2.0; 75th ix=3.0 -> 4.0.
        let a = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(compute(&a, 0.25, false), 2.0);
        assert_eq!(compute(&a, 0.75, false), 4.0);
    }
}
