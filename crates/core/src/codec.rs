//! Little-endian binary encoding shared by snapshots and the write-ahead log.
//!
//! The format is deliberately simple: fixed-width integers, length-prefixed strings and
//! sequences, and one tag byte per [`Value`]. Every decode is bounds-checked and reports
//! [`Error::Storage`] instead of panicking, because the input comes from disk.

use std::collections::BTreeMap;

use crate::spatial::{Crs, Point};
use crate::{Error, NodeId, Path, RelId, Result, Temporal, Value};

#[derive(Default)]
pub(crate) struct Encoder {
    pub buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn len(&mut self, n: usize) {
        self.u64(n as u64);
    }

    pub fn bool(&mut self, v: bool) {
        self.u8(v as u8);
    }

    pub fn str(&mut self, s: &str) {
        self.len(s.len());
        self.buf.extend_from_slice(s.as_bytes());
    }

    pub fn strings(&mut self, items: &[String]) {
        self.len(items.len());
        for s in items {
            self.str(s);
        }
    }

    pub fn props(&mut self, props: &BTreeMap<String, Value>) {
        self.len(props.len());
        for (k, v) in props {
            self.str(k);
            self.value(v);
        }
    }

    pub fn value(&mut self, v: &Value) {
        match v {
            Value::Null => self.u8(0),
            Value::Bool(b) => {
                self.u8(1);
                self.bool(*b);
            }
            Value::Int(i) => {
                self.u8(2);
                self.u64(*i as u64);
            }
            Value::Float(f) => {
                self.u8(3);
                self.u64(f.to_bits());
            }
            Value::String(s) => {
                self.u8(4);
                self.str(s);
            }
            Value::List(items) => {
                self.u8(5);
                self.len(items.len());
                for i in items {
                    self.value(i);
                }
            }
            Value::Map(m) => {
                self.u8(6);
                self.props(m);
            }
            Value::Node(id) => {
                self.u8(7);
                self.u64(id.0);
            }
            Value::Rel(id) => {
                self.u8(8);
                self.u64(id.0);
            }
            Value::Path(p) => {
                self.u8(9);
                self.len(p.nodes.len());
                for n in &p.nodes {
                    self.u64(n.0);
                }
                self.len(p.rels.len());
                for r in &p.rels {
                    self.u64(r.0);
                }
            }
            Value::Temporal(t) => {
                self.u8(10);
                match *t {
                    Temporal::Date(d) => {
                        self.u8(1);
                        self.u64(d as u64);
                    }
                    Temporal::LocalTime(n) => {
                        self.u8(2);
                        self.u64(n as u64);
                    }
                    Temporal::Time { nanos, offset } => {
                        self.u8(3);
                        self.u64(nanos as u64);
                        self.u32(offset as u32);
                    }
                    Temporal::LocalDateTime { days, nanos } => {
                        self.u8(4);
                        self.u64(days as u64);
                        self.u64(nanos as u64);
                    }
                    Temporal::DateTime { days, nanos, offset } => {
                        self.u8(5);
                        self.u64(days as u64);
                        self.u64(nanos as u64);
                        self.u32(offset as u32);
                    }
                    Temporal::Duration(d) => {
                        self.u8(6);
                        self.u64(d.months as u64);
                        self.u64(d.days as u64);
                        self.u64(d.seconds as u64);
                        self.u32(d.nanos as u32);
                    }
                }
            }
            Value::Point(p) => {
                self.u8(11);
                self.u8(p.crs().code());
                for c in p.coords() {
                    self.u64(c.to_bits());
                }
            }
        }
    }
}

pub(crate) struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Names the file being decoded, for error messages.
    what: &'a str,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8], what: &'a str) -> Self {
        Decoder { buf, pos: 0, what }
    }

    pub fn corrupt(&self, msg: impl std::fmt::Display) -> Error {
        Error::Storage(format!(
            "{} is corrupt at byte {}: {msg}",
            self.what, self.pos
        ))
    }

    pub fn is_empty(&self) -> bool {
        self.pos == self.buf.len()
    }

    /// The next `n` raw bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() - self.pos < n {
            return Err(self.corrupt("unexpected end of data"));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// A sequence length. Each element takes at least `min_size` bytes, so a length that
    /// cannot fit in the remaining input is rejected before anything is allocated.
    pub fn len(&mut self, min_size: usize) -> Result<usize> {
        let n = self.u64()?;
        let remaining = (self.buf.len() - self.pos) as u64;
        if n.saturating_mul(min_size.max(1) as u64) > remaining {
            return Err(self.corrupt(format!("length {n} exceeds the remaining data")));
        }
        Ok(n as usize)
    }

    pub fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(self.corrupt(format!("invalid boolean {b}"))),
        }
    }

    pub fn str(&mut self) -> Result<String> {
        let n = self.len(1)?;
        let bytes = self.take(n)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| self.corrupt("invalid UTF-8"))
    }

    pub fn strings(&mut self) -> Result<Vec<String>> {
        let n = self.len(8)?;
        (0..n).map(|_| self.str()).collect()
    }

    pub fn props(&mut self) -> Result<BTreeMap<String, Value>> {
        let n = self.len(9)?;
        let mut out = BTreeMap::new();
        for _ in 0..n {
            let k = self.str()?;
            let v = self.value()?;
            out.insert(k, v);
        }
        Ok(out)
    }

    fn temporal(&mut self) -> Result<Temporal> {
        use crate::temporal::{Duration, MAX_YEAR, NANOS_PER_DAY, NANOS_PER_SECOND, civil_from_days};
        let days = |d: &Self, v: u64| -> Result<i64> {
            let v = v as i64;
            if civil_from_days(v).0.abs() > MAX_YEAR {
                return Err(d.corrupt(format!("date out of range ({v} days)")));
            }
            Ok(v)
        };
        let nanos = |d: &Self, v: u64| -> Result<i64> {
            let v = v as i64;
            if (0..NANOS_PER_DAY).contains(&v) {
                Ok(v)
            } else {
                Err(d.corrupt(format!("time of day out of range ({v} ns)")))
            }
        };
        let offset = |d: &Self, v: u32| -> Result<i32> {
            let o = v as i32;
            if o.abs() <= 18 * 3600 {
                Ok(o)
            } else {
                Err(d.corrupt(format!("UTC offset out of range ({o} s)")))
            }
        };
        Ok(match self.u8()? {
            1 => {
                let d = self.u64()?;
                Temporal::Date(days(self, d)?)
            }
            2 => {
                let n = self.u64()?;
                Temporal::LocalTime(nanos(self, n)?)
            }
            3 => {
                let (n, o) = (self.u64()?, self.u32()?);
                Temporal::Time {
                    nanos: nanos(self, n)?,
                    offset: offset(self, o)?,
                }
            }
            4 => {
                let (d, n) = (self.u64()?, self.u64()?);
                Temporal::LocalDateTime {
                    days: days(self, d)?,
                    nanos: nanos(self, n)?,
                }
            }
            5 => {
                let (d, n, o) = (self.u64()?, self.u64()?, self.u32()?);
                Temporal::DateTime {
                    days: days(self, d)?,
                    nanos: nanos(self, n)?,
                    offset: offset(self, o)?,
                }
            }
            6 => {
                let (months, d, seconds, n) = (self.u64()?, self.u64()?, self.u64()?, self.u32()?);
                if n as i64 >= NANOS_PER_SECOND {
                    return Err(self.corrupt(format!("duration nanoseconds out of range ({n})")));
                }
                Temporal::Duration(Duration {
                    months: months as i64,
                    days: d as i64,
                    seconds: seconds as i64,
                    nanos: n as i32,
                })
            }
            k => return Err(self.corrupt(format!("unknown temporal kind {k}"))),
        })
    }

    pub fn value(&mut self) -> Result<Value> {
        Ok(match self.u8()? {
            0 => Value::Null,
            1 => Value::Bool(self.bool()?),
            2 => Value::Int(self.u64()? as i64),
            3 => Value::Float(f64::from_bits(self.u64()?)),
            4 => Value::String(self.str()?),
            5 => {
                let n = self.len(1)?;
                Value::List((0..n).map(|_| self.value()).collect::<Result<_>>()?)
            }
            6 => Value::Map(self.props()?),
            7 => Value::Node(NodeId(self.u64()?)),
            8 => Value::Rel(RelId(self.u64()?)),
            9 => {
                let n = self.len(8)?;
                let nodes = (0..n)
                    .map(|_| self.u64().map(NodeId))
                    .collect::<Result<_>>()?;
                let n = self.len(8)?;
                let rels = (0..n)
                    .map(|_| self.u64().map(RelId))
                    .collect::<Result<_>>()?;
                Value::Path(Path { nodes, rels })
            }
            10 => Value::Temporal(self.temporal()?),
            11 => {
                let code = self.u8()?;
                let crs = Crs::from_code(code)
                    .ok_or_else(|| self.corrupt(format!("unknown coordinate reference system {code}")))?;
                let mut coords = [0.0; 3];
                for c in &mut coords[..crs.dims()] {
                    *c = f64::from_bits(self.u64()?);
                }
                let p = Point::new(crs, &coords[..crs.dims()])
                    .map_err(|e| self.corrupt(format!("invalid point: {e}")))?;
                Value::Point(p)
            }
            t => return Err(self.corrupt(format!("unknown value tag {t}"))),
        })
    }
}

/// CRC-32 (IEEE 802.3, reflected, polynomial 0xEDB88320).
pub(crate) fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            table[i] = c;
            i += 1;
        }
        table
    };
    let mut crc = !0u32;
    for &b in data {
        crc = TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn values_round_trip() {
        let mut m = BTreeMap::new();
        m.insert("k".to_string(), Value::List(vec![1.into(), "x".into()]));
        let values = vec![
            Value::Null,
            true.into(),
            (-7i64).into(),
            Value::Float(-0.5),
            "héllo".into(),
            Value::Map(m),
            Value::Node(NodeId(3)),
            Value::Rel(RelId(4)),
            Value::Path(Path {
                nodes: vec![NodeId(1), NodeId(2)],
                rels: vec![RelId(9)],
            }),
        ];
        let mut e = Encoder::new();
        for v in &values {
            e.value(v);
        }
        let mut d = Decoder::new(&e.buf, "test");
        for v in &values {
            assert_eq!(&d.value().unwrap(), v);
        }
        assert!(d.is_empty());
    }

    #[test]
    fn truncated_input_is_an_error() {
        let mut e = Encoder::new();
        e.str("hello");
        let mut d = Decoder::new(&e.buf[..6], "test");
        assert!(matches!(d.str(), Err(Error::Storage(_))));
    }
}
