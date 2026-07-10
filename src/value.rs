//! The dynamic value model and SQLite "serial types".
//!
//! Every cell in a SQLite table or index stores a *record*: a header of serial
//! type codes (varints) followed by the concatenated value bodies. This module
//! models the five SQLite storage classes ([`Value`]) and the serial-type codes
//! ([`SerialType`]) that describe how each value is laid out on disk.
//!
//! Record (de)serialization itself lives in the `format` module and builds on
//! these types; keeping the value model here lets the rest of the engine reason
//! about values without pulling in disk-format details.

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

/// A value's storage class, owning its data.
///
/// These are the five SQLite storage classes. Note that SQLite stores `BOOLEAN`
/// as integers and has no separate date/time class — those are conventions on
/// top of these five.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// A signed 64-bit integer.
    Integer(i64),
    /// An IEEE-754 double.
    Real(f64),
    /// A UTF-8 text value. (SQLite also supports UTF-16; graphitesql stores
    /// text as UTF-8 internally and converts at the boundary.)
    Text(String),
    /// A binary blob.
    Blob(Vec<u8>),
}

/// A borrowed view of a [`Value`], used on hot decode paths to avoid copying.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueRef<'a> {
    /// SQL `NULL`.
    Null,
    /// A signed 64-bit integer.
    Integer(i64),
    /// An IEEE-754 double.
    Real(f64),
    /// Borrowed UTF-8 text.
    Text(&'a str),
    /// Borrowed binary blob.
    Blob(&'a [u8]),
}

impl ValueRef<'_> {
    /// Copy this borrowed value into an owned [`Value`].
    pub fn to_owned(&self) -> Value {
        match *self {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(i) => Value::Integer(i),
            ValueRef::Real(r) => Value::Real(r),
            ValueRef::Text(s) => Value::Text(String::from(s)),
            ValueRef::Blob(b) => Value::Blob(Vec::from(b)),
        }
    }
}

/// A text collating sequence. `BINARY` (the default) compares bytes; `NOCASE`
/// folds ASCII letters; `RTRIM` ignores trailing spaces. `Custom` is an
/// application-registered sequence (see [`Connection::register_collation`](crate::Connection::register_collation)), identified by a
/// small id into a process-global registry so this enum stays `Copy`/`Send`/`Sync`.
/// Collations only affect text-vs-text comparison; storage-class ordering is
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Collation {
    /// `BINARY` — `memcmp` on the UTF-8 bytes.
    #[default]
    Binary,
    /// `NOCASE` — ASCII case-insensitive.
    NoCase,
    /// `RTRIM` — like `BINARY` but trailing spaces are ignored.
    RTrim,
    /// A user-registered collating sequence, by registry id. Only ever
    /// constructed on `std` builds (via [`Connection::register_collation`](crate::Connection::register_collation)).
    Custom(u32),
}

impl Collation {
    /// Parse a *built-in* collation name (`BINARY`/`NOCASE`/`RTRIM`,
    /// case-insensitive). Does not resolve custom collations — use the
    /// crate-internal `resolve_collation_name` for that.
    pub fn parse(name: &str) -> Option<Collation> {
        match name.to_ascii_lowercase().as_str() {
            "binary" => Some(Collation::Binary),
            "nocase" => Some(Collation::NoCase),
            "rtrim" => Some(Collation::RTrim),
            _ => None,
        }
    }
}

/// Resolve a collation name to a [`Collation`], including application-registered
/// custom collations. Returns `None` for an unknown name (the caller reports the
/// usual `no such collation sequence` error). Built-ins resolve on every build;
/// custom names resolve only on `std` builds.
pub fn resolve_collation_name(name: &str) -> Option<Collation> {
    if let Some(c) = Collation::parse(name) {
        return Some(c);
    }
    #[cfg(feature = "std")]
    {
        registry::resolve_name(name).map(Collation::Custom)
    }
    #[cfg(not(feature = "std"))]
    {
        None
    }
}

/// The name of a collation for schema/EXPLAIN reprinting (`BINARY`/`NOCASE`/
/// `RTRIM`, or a custom sequence's registered name).
pub fn collation_name(coll: Collation) -> alloc::string::String {
    use alloc::string::ToString;
    match coll {
        Collation::Binary => "BINARY".to_string(),
        Collation::NoCase => "NOCASE".to_string(),
        Collation::RTrim => "RTRIM".to_string(),
        Collation::Custom(id) => {
            #[cfg(feature = "std")]
            {
                registry::name_of(id).unwrap_or_else(|| "BINARY".to_string())
            }
            #[cfg(not(feature = "std"))]
            {
                let _ = id;
                "BINARY".to_string()
            }
        }
    }
}

/// Register (or replace) a custom collating sequence `name`, callable as
/// `COLLATE <name>` in SQL. `cmp` compares two text values. Requires `std`.
///
/// Re-registering an existing name replaces its comparison function. The
/// registry is process-global and its entries are never reclaimed, so register
/// each collation once at startup.
#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
pub fn register_collation<F>(name: &str, cmp: F) -> u32
where
    F: Fn(&str, &str) -> Ordering + Send + 'static,
{
    registry::register(name, alloc::boxed::Box::new(cmp))
}

/// The process-global custom-collation registry. `std`-only (needs a global
/// `Mutex`). Ids index `fns`; `by_name` maps a name to its id.
#[cfg(feature = "std")]
mod registry {
    use super::Ordering;
    use alloc::boxed::Box;
    use alloc::collections::BTreeMap;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use std::sync::{Mutex, OnceLock};

    type CollFn = Box<dyn Fn(&str, &str) -> Ordering + Send>;

    struct Registry {
        /// Lowercased name → id (collation names are case-insensitive).
        by_name: BTreeMap<String, u32>,
        /// id → original-case name (for schema/EXPLAIN reprinting).
        names: Vec<String>,
        /// id → comparison function.
        fns: Vec<CollFn>,
    }

    fn registry() -> &'static Mutex<Registry> {
        static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
        REG.get_or_init(|| {
            Mutex::new(Registry {
                by_name: BTreeMap::new(),
                names: Vec::new(),
                fns: Vec::new(),
            })
        })
    }

    pub(super) fn register(name: &str, f: CollFn) -> u32 {
        let mut reg = registry().lock().unwrap();
        let key = name.to_ascii_lowercase();
        if let Some(&id) = reg.by_name.get(&key) {
            reg.fns[id as usize] = f;
            reg.names[id as usize] = name.to_string();
            return id;
        }
        let id = reg.fns.len() as u32;
        reg.fns.push(f);
        reg.names.push(name.to_string());
        reg.by_name.insert(key, id);
        id
    }

    pub(super) fn resolve_name(name: &str) -> Option<u32> {
        registry()
            .lock()
            .unwrap()
            .by_name
            .get(&name.to_ascii_lowercase())
            .copied()
    }

    pub(super) fn name_of(id: u32) -> Option<String> {
        registry().lock().unwrap().names.get(id as usize).cloned()
    }

    pub(super) fn compare(id: u32, x: &str, y: &str) -> Ordering {
        let reg = registry().lock().unwrap();
        match reg.fns.get(id as usize) {
            Some(f) => f(x, y),
            // Unregistered id (cannot happen via the public API): fall back to BINARY.
            None => x.as_bytes().cmp(y.as_bytes()),
        }
    }
}

/// Compare two text strings under `coll`.
pub fn cmp_text(x: &str, y: &str, coll: Collation) -> Ordering {
    match coll {
        Collation::Binary => x.as_bytes().cmp(y.as_bytes()),
        Collation::NoCase => x
            .bytes()
            .map(|b| b.to_ascii_uppercase())
            .cmp(y.bytes().map(|b| b.to_ascii_uppercase())),
        Collation::RTrim => x
            .trim_end_matches(' ')
            .as_bytes()
            .cmp(y.trim_end_matches(' ').as_bytes()),
        Collation::Custom(_id) => {
            #[cfg(feature = "std")]
            {
                registry::compare(_id, x, y)
            }
            // In `no_std` builds a `Custom` collation can never be constructed
            // (there is no registry), so this arm is unreachable; fall back to
            // BINARY to keep the match exhaustive.
            #[cfg(not(feature = "std"))]
            {
                x.as_bytes().cmp(y.as_bytes())
            }
        }
    }
}

/// Like [`cmp_values`] but applying `coll` to text-vs-text comparison.
pub fn cmp_values_coll(a: &Value, b: &Value, coll: Collation) -> Ordering {
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => cmp_text(x, y, coll),
        _ => cmp_values(a, b),
    }
}

/// Compare two values in SQLite's total ordering: `NULL` < numbers < text <
/// blobs; numbers compared numerically, text by byte (the `BINARY` collation),
/// blobs by `memcmp`. This is the order used for index keys, `ORDER BY`, and
/// comparisons (collation refinements are layered on top elsewhere).
pub fn cmp_values(a: &Value, b: &Value) -> Ordering {
    fn class(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Integer(_) | Value::Real(_) => 1,
            Value::Text(_) => 2,
            Value::Blob(_) => 3,
        }
    }
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        // Two integers compare exactly as `i64`; coercing both through `f64`
        // (as this used to) collapses values above 2^53 — e.g. `10^16` and
        // `10^16 + 1` would wrongly read equal.
        (Value::Integer(x), Value::Integer(y)) => x.cmp(y),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        // A mixed integer/real comparison uses SQLite's exact algorithm, which
        // never loses the integer's low bits to a lossy `f64` round-trip.
        (Value::Integer(i), Value::Real(r)) => int_float_cmp(*i, *r),
        (Value::Real(r), Value::Integer(i)) => int_float_cmp(*i, *r).reverse(),
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Blob(x), Value::Blob(y)) => x.cmp(y),
        _ => class(a).cmp(&class(b)),
    }
}

/// Compare an `i64` with an `f64` exactly, mirroring SQLite's
/// `sqlite3IntFloatCompare` (the 8-byte-`double` branch). Returns the ordering
/// of `i` relative to `r`. The naive `i as f64` comparison loses precision once
/// `|i| > 2^53`; this truncates the real toward zero, compares integer parts
/// first, then disambiguates an equal-integer-part tie by the real's fraction.
fn int_float_cmp(i: i64, r: f64) -> Ordering {
    if r.is_nan() {
        // SQLite never stores a NaN (it becomes NULL); match the prior
        // `partial_cmp(..).unwrap_or(Equal)` fallback defensively.
        return Ordering::Equal;
    }
    // `r` entirely outside the `i64` range: any finite integer is on the near
    // side. (`2^63` is not representable as `i64`, so the upper bound is `>=`.)
    if r < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    if r >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    let y = r as i64; // truncates toward zero; exact since `r` is in range
    match i.cmp(&y) {
        Ordering::Equal => (i as f64).partial_cmp(&r).unwrap_or(Ordering::Equal),
        other => other,
    }
}

/// A SQLite record serial type code.
///
/// The mapping from code to meaning (file-format spec, "Serial Type Codes Of
/// The Record Format"):
///
/// | code | meaning | body bytes |
/// |------|---------|------------|
/// | 0 | NULL | 0 |
/// | 1 | int, big-endian | 1 |
/// | 2 | int | 2 |
/// | 3 | int | 3 |
/// | 4 | int | 4 |
/// | 5 | int | 6 |
/// | 6 | int | 8 |
/// | 7 | IEEE-754 float | 8 |
/// | 8 | integer 0 | 0 |
/// | 9 | integer 1 | 0 |
/// | 10, 11 | reserved | — |
/// | N≥12 even | BLOB | (N-12)/2 |
/// | N≥13 odd | TEXT | (N-13)/2 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialType(pub u64);

impl SerialType {
    /// Number of bytes this serial type occupies in the record body, or `None`
    /// for the reserved codes 10 and 11.
    pub fn content_len(self) -> Option<usize> {
        Some(match self.0 {
            0 | 8 | 9 => 0,
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 4,
            5 => 6,
            6 | 7 => 8,
            10 | 11 => return None,
            n if n % 2 == 0 => ((n - 12) / 2) as usize,
            n => ((n - 13) / 2) as usize,
        })
    }

    /// The smallest serial type that can losslessly represent `value`.
    ///
    /// This matches SQLite's choice: small integers collapse to the 0/1 literals
    /// (codes 8/9) and otherwise to the narrowest of the 1/2/3/4/6/8-byte forms.
    pub fn for_value(value: &Value) -> SerialType {
        SerialType(match value {
            Value::Null => 0,
            Value::Integer(0) => 8,
            Value::Integer(1) => 9,
            Value::Integer(i) => {
                let i = *i;
                if (-0x80..=0x7f).contains(&i) {
                    1
                } else if (-0x8000..=0x7fff).contains(&i) {
                    2
                } else if (-0x80_0000..=0x7f_ffff).contains(&i) {
                    3
                } else if (-0x8000_0000..=0x7fff_ffff).contains(&i) {
                    4
                } else if (-0x8000_0000_0000..=0x7fff_ffff_ffff).contains(&i) {
                    5
                } else {
                    6
                }
            }
            Value::Real(_) => 7,
            Value::Blob(b) => 12 + 2 * b.len() as u64,
            Value::Text(s) => 13 + 2 * s.len() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    #[test]
    fn content_lengths() {
        assert_eq!(SerialType(0).content_len(), Some(0));
        assert_eq!(SerialType(1).content_len(), Some(1));
        assert_eq!(SerialType(5).content_len(), Some(6));
        assert_eq!(SerialType(6).content_len(), Some(8));
        assert_eq!(SerialType(7).content_len(), Some(8));
        assert_eq!(SerialType(8).content_len(), Some(0));
        assert_eq!(SerialType(9).content_len(), Some(0));
        assert_eq!(SerialType(10).content_len(), None);
        assert_eq!(SerialType(11).content_len(), None);
        // BLOB of 4 bytes -> 12 + 2*4 = 20.
        assert_eq!(SerialType(20).content_len(), Some(4));
        // TEXT of 5 bytes -> 13 + 2*5 = 23.
        assert_eq!(SerialType(23).content_len(), Some(5));
    }

    #[test]
    fn serial_type_selection_matches_sqlite() {
        assert_eq!(SerialType::for_value(&Value::Null), SerialType(0));
        assert_eq!(SerialType::for_value(&Value::Integer(0)), SerialType(8));
        assert_eq!(SerialType::for_value(&Value::Integer(1)), SerialType(9));
        assert_eq!(SerialType::for_value(&Value::Integer(2)), SerialType(1));
        assert_eq!(SerialType::for_value(&Value::Integer(127)), SerialType(1));
        assert_eq!(SerialType::for_value(&Value::Integer(128)), SerialType(2));
        assert_eq!(SerialType::for_value(&Value::Integer(-1)), SerialType(1));
        assert_eq!(
            SerialType::for_value(&Value::Integer(i64::MAX)),
            SerialType(6)
        );
        assert_eq!(SerialType::for_value(&Value::Real(1.5)), SerialType(7));
        assert_eq!(
            SerialType::for_value(&Value::Text("abc".to_string())),
            SerialType(19) // 13 + 2*3
        );
        assert_eq!(
            SerialType::for_value(&Value::Blob(vec![0u8; 4])),
            SerialType(20) // 12 + 2*4
        );
    }

    #[test]
    fn value_ref_round_trips() {
        assert_eq!(ValueRef::Integer(5).to_owned(), Value::Integer(5));
        assert_eq!(ValueRef::Text("x").to_owned(), Value::Text("x".to_string()));
    }
}
