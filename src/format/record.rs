//! The SQLite record format: how a row (or index key) is serialized to bytes.
//!
//! A record is a *header* followed by a *body* (file-format spec, "Record
//! Format"):
//!
//! ```text
//! ┌───────────────┬───────────────────────────┐
//! │ header        │ body                      │
//! │ varint hdrlen │ serialtype varints…       │ value bodies…
//! └───────────────┴───────────────────────────┘
//! ```
//!
//! The header begins with a varint giving the total header length in bytes
//! (including that varint), then one serial-type varint per column. The body is
//! the concatenation of each column's bytes, laid out exactly as its serial type
//! dictates (see [`SerialType`]). Integers are big-endian and sign-extended.

use crate::error::{Error, Result};
use crate::format::TextEncoding;
use crate::util::varint;
use crate::value::{SerialType, Value};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Decode a complete record into its column values.
///
/// Serialize column `values` into a record (header of serial types + bodies),
/// exactly as SQLite lays out a row or index key. Integers use the narrowest
/// lossless serial type; this is the inverse of [`decode_record`].
pub fn encode_record(values: &[Value]) -> Vec<u8> {
    let serials: Vec<SerialType> = values.iter().map(SerialType::for_value).collect();

    // Body of the header: the serial-type varints.
    let mut serial_bytes = Vec::new();
    let mut buf = [0u8; varint::MAX_LEN];
    for s in &serials {
        let n = varint::encode(s.0, &mut buf);
        serial_bytes.extend_from_slice(&buf[..n]);
    }
    // The header length varint includes itself; iterate to a fixed point because
    // changing the length can change the varint's own width.
    let mut header_len = serial_bytes.len() + 1;
    loop {
        let n = varint::len(header_len as u64);
        if n + serial_bytes.len() == header_len {
            break;
        }
        header_len = n + serial_bytes.len();
    }

    let mut out = Vec::new();
    let n = varint::encode(header_len as u64, &mut buf);
    out.extend_from_slice(&buf[..n]);
    out.extend_from_slice(&serial_bytes);

    for (v, s) in values.iter().zip(&serials) {
        match v {
            Value::Null | Value::Integer(0) | Value::Integer(1) => {}
            Value::Integer(i) => {
                let len = s.content_len().unwrap_or(0);
                let be = i.to_be_bytes();
                out.extend_from_slice(&be[8 - len..]);
            }
            Value::Real(r) => out.extend_from_slice(&r.to_be_bytes()),
            Value::Text(t) => out.extend_from_slice(t.as_bytes()),
            Value::Blob(b) => out.extend_from_slice(b),
        }
    }
    out
}

/// `encoding` selects how TEXT bodies are interpreted (UTF-8 or UTF-16).
pub fn decode_record(bytes: &[u8], encoding: TextEncoding) -> Result<Vec<Value>> {
    let (header_len, n) = varint::decode(bytes)
        .ok_or_else(|| Error::Corrupt("truncated record header length".into()))?;
    let header_len = header_len as usize;
    if header_len > bytes.len() {
        return Err(Error::Corrupt(format!(
            "record header length {header_len} exceeds record size {}",
            bytes.len()
        )));
    }

    let mut values = Vec::new();
    let mut hdr = n; // cursor within the header region
    let mut body = header_len; // cursor within the body region
    while hdr < header_len {
        let (raw, used) = varint::decode(&bytes[hdr..])
            .ok_or_else(|| Error::Corrupt("truncated serial type".into()))?;
        hdr += used;
        let serial = SerialType(raw);
        let len = serial
            .content_len()
            .ok_or_else(|| Error::Corrupt(format!("reserved serial type {raw}")))?;
        if body + len > bytes.len() {
            return Err(Error::Corrupt(
                "record body shorter than header implies".into(),
            ));
        }
        let value = decode_value(serial, &bytes[body..body + len], encoding)?;
        body += len;
        values.push(value);
    }
    Ok(values)
}

/// Decode a single column body of the given serial type.
fn decode_value(serial: SerialType, body: &[u8], encoding: TextEncoding) -> Result<Value> {
    Ok(match serial.0 {
        0 => Value::Null,
        1 => Value::Integer(i64::from(body[0] as i8)),
        2 => Value::Integer(i64::from(i16::from_be_bytes([body[0], body[1]]))),
        3 => Value::Integer(sign_extend(body, 3)),
        4 => Value::Integer(i64::from(i32::from_be_bytes([
            body[0], body[1], body[2], body[3],
        ]))),
        5 => Value::Integer(sign_extend(body, 6)),
        6 => Value::Integer(i64::from_be_bytes([
            body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
        ])),
        7 => Value::Real(f64::from_be_bytes([
            body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
        ])),
        8 => Value::Integer(0),
        9 => Value::Integer(1),
        n if n >= 12 && n.is_multiple_of(2) => Value::Blob(Vec::from(body)),
        _ => Value::Text(decode_text(body, encoding)?),
    })
}

/// Sign-extend a big-endian integer of `nbytes` (3 or 6) bytes to `i64`.
fn sign_extend(body: &[u8], nbytes: usize) -> i64 {
    let mut v: u64 = 0;
    for &b in &body[..nbytes] {
        v = (v << 8) | u64::from(b);
    }
    let shift = 64 - (nbytes * 8);
    // Shift left then arithmetic-shift right to propagate the sign bit.
    ((v << shift) as i64) >> shift
}

fn decode_text(body: &[u8], encoding: TextEncoding) -> Result<String> {
    match encoding {
        TextEncoding::Utf8 => String::from_utf8(Vec::from(body))
            .map_err(|_| Error::Corrupt("invalid UTF-8 text".into())),
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            if !body.len().is_multiple_of(2) {
                return Err(Error::Corrupt("odd-length UTF-16 text".into()));
            }
            let units = body.chunks_exact(2).map(|c| match encoding {
                TextEncoding::Utf16Be => u16::from_be_bytes([c[0], c[1]]),
                _ => u16::from_le_bytes([c[0], c[1]]),
            });
            char::decode_utf16(units)
                .collect::<core::result::Result<String, _>>()
                .map_err(|_| Error::Corrupt("invalid UTF-16 text".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn round_trip_mixed_values() {
        let values = vec![
            Value::Null,
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(42),
            Value::Integer(-1),
            Value::Integer(300),
            Value::Integer(-70000),
            Value::Integer(i64::MIN),
            Value::Integer(i64::MAX),
            Value::Real(2.5),
            Value::Text(String::from("hello, graphite")),
            Value::Blob(vec![0u8, 1, 2, 255, 254]),
        ];
        let bytes = encode_record(&values);
        let decoded = decode_record(&bytes, TextEncoding::Utf8).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn three_and_six_byte_ints_sign_extend() {
        for v in [
            Value::Integer(-0x80_0000),
            Value::Integer(0x7f_ffff),
            Value::Integer(-0x8000_0000_0000),
            Value::Integer(0x7fff_ffff_ffff),
        ] {
            let bytes = encode_record(core::slice::from_ref(&v));
            assert_eq!(decode_record(&bytes, TextEncoding::Utf8).unwrap(), vec![v]);
        }
    }

    #[test]
    fn rejects_truncated_body() {
        // Header claims one 8-byte integer (serial 6) but body is empty.
        let bytes = [0x02, 0x06];
        assert!(decode_record(&bytes, TextEncoding::Utf8).is_err());
    }

    #[test]
    fn empty_record_decodes_to_no_columns() {
        // header_len = 1 (just the length varint), no serial types.
        let decoded = decode_record(&[0x01], TextEncoding::Utf8).unwrap();
        assert!(decoded.is_empty());
    }
}
