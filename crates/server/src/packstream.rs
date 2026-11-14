//! PackStream, the binary encoding of Bolt messages and values.
//!
//! A value is a null, boolean, 64-bit integer, 64-bit float, byte array, string, list,
//! map (string keys) or structure (a one-byte tag and up to 15 fields). Integers use the
//! smallest of the tiny, 8, 16, 32 and 64-bit forms; sizes are big-endian.

use std::collections::BTreeMap;
use std::fmt;

/// Deepest nesting accepted when decoding, so that hostile input cannot overflow the stack.
const MAX_DEPTH: usize = 128;

/// A PackStream value.
#[derive(Clone, Debug, PartialEq)]
pub enum PackValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Bytes(Vec<u8>),
    String(String),
    List(Vec<PackValue>),
    Map(BTreeMap<String, PackValue>),
    Struct { tag: u8, fields: Vec<PackValue> },
}

impl PackValue {
    /// A short name of the value's type, for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            PackValue::Null => "null",
            PackValue::Bool(_) => "boolean",
            PackValue::Int(_) => "integer",
            PackValue::Float(_) => "float",
            PackValue::Bytes(_) => "bytes",
            PackValue::String(_) => "string",
            PackValue::List(_) => "list",
            PackValue::Map(_) => "map",
            PackValue::Struct { .. } => "structure",
        }
    }

    /// A map from `(key, value)` pairs.
    pub fn map<K: Into<String>>(entries: impl IntoIterator<Item = (K, PackValue)>) -> PackValue {
        PackValue::Map(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    pub fn str(s: impl Into<String>) -> PackValue {
        PackValue::String(s.into())
    }
}

/// Malformed PackStream input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

// ---- encoding -------------------------------------------------------------------------------

/// Appends `v` to `out`.
pub fn encode(out: &mut Vec<u8>, v: &PackValue) {
    match v {
        PackValue::Null => out.push(0xC0),
        PackValue::Bool(b) => out.push(if *b { 0xC3 } else { 0xC2 }),
        PackValue::Int(i) => encode_int(out, *i),
        PackValue::Float(f) => {
            out.push(0xC1);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        PackValue::Bytes(b) => {
            sized(out, b.len(), None, [0xCC, 0xCD, 0xCE]);
            out.extend_from_slice(b);
        }
        PackValue::String(s) => encode_str(out, s),
        PackValue::List(items) => {
            sized(out, items.len(), Some(0x90), [0xD4, 0xD5, 0xD6]);
            for x in items {
                encode(out, x);
            }
        }
        PackValue::Map(m) => {
            sized(out, m.len(), Some(0xA0), [0xD8, 0xD9, 0xDA]);
            for (k, x) in m {
                encode_str(out, k);
                encode(out, x);
            }
        }
        PackValue::Struct { tag, fields } => {
            debug_assert!(fields.len() < 16, "a structure has at most 15 fields");
            out.push(0xB0 | fields.len() as u8);
            out.push(*tag);
            for x in fields {
                encode(out, x);
            }
        }
    }
}

pub fn encode_int(out: &mut Vec<u8>, i: i64) {
    if (-16..128).contains(&i) {
        out.push(i as u8);
    } else if let Ok(v) = i8::try_from(i) {
        out.push(0xC8);
        out.push(v as u8);
    } else if let Ok(v) = i16::try_from(i) {
        out.push(0xC9);
        out.extend_from_slice(&v.to_be_bytes());
    } else if let Ok(v) = i32::try_from(i) {
        out.push(0xCA);
        out.extend_from_slice(&v.to_be_bytes());
    } else {
        out.push(0xCB);
        out.extend_from_slice(&i.to_be_bytes());
    }
}

pub fn encode_str(out: &mut Vec<u8>, s: &str) {
    sized(out, s.len(), Some(0x80), [0xD0, 0xD1, 0xD2]);
    out.extend_from_slice(s.as_bytes());
}

/// A size marker: `tiny | len` below 16 (when the type has a tiny form), else one of the
/// 8, 16 and 32-bit markers followed by the size.
fn sized(out: &mut Vec<u8>, len: usize, tiny: Option<u8>, markers: [u8; 3]) {
    match tiny {
        Some(t) if len < 16 => out.push(t | len as u8),
        _ if len <= u8::MAX as usize => {
            out.push(markers[0]);
            out.push(len as u8);
        }
        _ if len <= u16::MAX as usize => {
            out.push(markers[1]);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            out.push(markers[2]);
            out.extend_from_slice(&(len as u32).to_be_bytes());
        }
    }
}

// ---- decoding -------------------------------------------------------------------------------

/// Decodes exactly one value from `bytes`; trailing bytes are an error.
pub fn decode(bytes: &[u8]) -> Result<PackValue, DecodeError> {
    let mut r = Reader { b: bytes, pos: 0 };
    let v = r.value(0)?;
    if r.pos != bytes.len() {
        return Err(DecodeError(format!("{} bytes after the value", bytes.len() - r.pos)));
    }
    Ok(v)
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.b.len() - self.pos < n {
            return Err(DecodeError("the value ends early".into()));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn be<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }

    /// A size, which cannot exceed the bytes left (each item takes at least one byte).
    fn len(&mut self, width: u8) -> Result<usize, DecodeError> {
        let n = match width {
            1 => self.byte()? as usize,
            2 => u16::from_be_bytes(self.be()?) as usize,
            _ => u32::from_be_bytes(self.be()?) as usize,
        };
        if n > self.b.len() - self.pos {
            return Err(DecodeError("a size runs past the end of the message".into()));
        }
        Ok(n)
    }

    fn string(&mut self, n: usize) -> Result<String, DecodeError> {
        let s = self.take(n)?;
        std::str::from_utf8(s)
            .map(str::to_string)
            .map_err(|_| DecodeError("a string is not UTF-8".into()))
    }

    fn list(&mut self, n: usize, depth: usize) -> Result<PackValue, DecodeError> {
        let mut items = Vec::with_capacity(n);
        for _ in 0..n {
            items.push(self.value(depth + 1)?);
        }
        Ok(PackValue::List(items))
    }

    fn map(&mut self, n: usize, depth: usize) -> Result<PackValue, DecodeError> {
        let mut m = BTreeMap::new();
        for _ in 0..n {
            let PackValue::String(k) = self.value(depth + 1)? else {
                return Err(DecodeError("a map key is not a string".into()));
            };
            let v = self.value(depth + 1)?;
            m.insert(k, v);
        }
        Ok(PackValue::Map(m))
    }

    fn value(&mut self, depth: usize) -> Result<PackValue, DecodeError> {
        if depth > MAX_DEPTH {
            return Err(DecodeError(format!("values nest deeper than {MAX_DEPTH} levels")));
        }
        let m = self.byte()?;
        Ok(match m {
            0x00..=0x7F => PackValue::Int(m as i64),
            0xF0..=0xFF => PackValue::Int(m as i8 as i64),
            0x80..=0x8F => PackValue::String(self.string((m & 0x0F) as usize)?),
            0x90..=0x9F => self.list((m & 0x0F) as usize, depth)?,
            0xA0..=0xAF => self.map((m & 0x0F) as usize, depth)?,
            0xB0..=0xBF => {
                let tag = self.byte()?;
                let n = (m & 0x0F) as usize;
                let mut fields = Vec::with_capacity(n);
                for _ in 0..n {
                    fields.push(self.value(depth + 1)?);
                }
                PackValue::Struct { tag, fields }
            }
            0xC0 => PackValue::Null,
            0xC1 => PackValue::Float(f64::from_bits(u64::from_be_bytes(self.be()?))),
            0xC2 => PackValue::Bool(false),
            0xC3 => PackValue::Bool(true),
            0xC8 => PackValue::Int(i8::from_be_bytes(self.be()?) as i64),
            0xC9 => PackValue::Int(i16::from_be_bytes(self.be()?) as i64),
            0xCA => PackValue::Int(i32::from_be_bytes(self.be()?) as i64),
            0xCB => PackValue::Int(i64::from_be_bytes(self.be()?)),
            0xCC..=0xCE => {
                let n = self.len(1 << (m - 0xCC))?;
                PackValue::Bytes(self.take(n)?.to_vec())
            }
            0xD0..=0xD2 => {
                let n = self.len(1 << (m - 0xD0))?;
                PackValue::String(self.string(n)?)
            }
            0xD4..=0xD6 => {
                let n = self.len(1 << (m - 0xD4))?;
                self.list(n, depth)?
            }
            0xD8..=0xDA => {
                let n = self.len(1 << (m - 0xD8))?;
                self.map(n, depth)?
            }
            _ => return Err(DecodeError(format!("unknown marker 0x{m:02X}"))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(v: PackValue) -> Vec<u8> {
        let mut out = Vec::new();
        encode(&mut out, &v);
        assert_eq!(decode(&out).unwrap(), v, "{out:02X?}");
        out
    }

    #[test]
    fn integers_use_the_smallest_form() {
        assert_eq!(round(PackValue::Int(1)), [0x01]);
        assert_eq!(round(PackValue::Int(-16)), [0xF0]);
        assert_eq!(round(PackValue::Int(-17)), [0xC8, 0xEF]);
        assert_eq!(round(PackValue::Int(127)), [0x7F]);
        assert_eq!(round(PackValue::Int(128)), [0xC9, 0x00, 0x80]);
        assert_eq!(round(PackValue::Int(-129)), [0xC9, 0xFF, 0x7F]);
        assert_eq!(round(PackValue::Int(40_000)).len(), 5);
        assert_eq!(round(PackValue::Int(i64::MIN)).len(), 9);
    }

    #[test]
    fn values_round_trip() {
        round(PackValue::Null);
        round(PackValue::Bool(true));
        round(PackValue::Float(-1.5));
        round(PackValue::Bytes(vec![1, 2, 3]));
        assert_eq!(round(PackValue::str("A")), [0x81, 0x41]);
        round(PackValue::str("x".repeat(300)));
        round(PackValue::List((0..20).map(PackValue::Int).collect()));
        round(PackValue::map([("a", PackValue::Int(1)), ("b", PackValue::List(vec![]))]));
        round(PackValue::map((0..70_000).map(|i| (i.to_string(), PackValue::Null))));
        round(PackValue::Struct {
            tag: 0x4E,
            fields: vec![PackValue::Int(1), PackValue::List(vec![]), PackValue::map::<&str>([])],
        });
    }

    #[test]
    fn malformed_input_is_an_error() {
        assert!(decode(&[0xD2, 0xFF, 0xFF, 0xFF, 0xFF]).is_err()); // size past the end
        assert!(decode(&[0xA1, 0x01, 0x01]).is_err()); // integer key
        assert!(decode(&[0x82, 0xFF, 0xFE]).is_err()); // not UTF-8
        assert!(decode(&[0xC7]).is_err());
        assert!(decode(&[0x01, 0x01]).is_err()); // trailing byte
        assert!(decode(&vec![0x91; 1000]).is_err()); // too deep
    }
}
