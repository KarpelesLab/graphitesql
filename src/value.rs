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
        assert_eq!(SerialType::for_value(&Value::Integer(i64::MAX)), SerialType(6));
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
