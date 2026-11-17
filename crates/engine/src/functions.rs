//! Built-in scalar functions.

use std::cell::Cell;
use std::collections::BTreeMap;

use graphbuff_core::temporal::{BetweenUnit, NANOS_PER_SECOND, Temporal, TemporalKind};
use graphbuff_core::{Error, Point, Result, Value};
use graphbuff_cypher::ast::IS_NORMALIZED;

use crate::context::Executor;
use crate::ops::{cypher_eq, order_cmp, to_plain_string};
use crate::unicode::{NormalForm, is_normalized, normalize};

fn arity(name: &str, args: &[Value], min: usize, max: usize) -> Result<()> {
    if args.len() < min || args.len() > max {
        let expected = if min == max {
            format!("{min}")
        } else {
            format!("{min} to {max}")
        };
        return Err(Error::Semantic(format!(
            "{name}() expects {expected} argument(s), got {}",
            args.len()
        )));
    }
    Ok(())
}

fn wrong_type(name: &str, v: &Value) -> Error {
    Error::Type(format!("{name}() does not accept {}", v.type_name()))
}

fn int_arg(name: &str, v: &Value) -> Result<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        Value::Float(f) if f.fract() == 0.0 => Ok(*f as i64),
        o => Err(wrong_type(name, o)),
    }
}

fn num_fn(name: &str, v: &Value, f: impl Fn(f64) -> f64) -> Result<Value> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Int(_) | Value::Float(_) => Ok(Value::Float(f(v.as_float().unwrap()))),
        o => Err(wrong_type(name, o)),
    }
}

fn str_fn(name: &str, v: &Value, f: impl Fn(&str) -> String) -> Result<Value> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::String(s) => Ok(Value::String(f(s))),
        o => Err(wrong_type(name, o)),
    }
}

thread_local! {
    static RNG: Cell<u64> = Cell::new(
        graphbuff_core::clock::unix_time().map(|d| d.as_nanos() as u64).unwrap_or(0x9E37_79B9_7F4A_7C15) | 1,
    );
}

/// xorshift64* — good enough for `rand()`, no dependency needed.
fn next_random() -> f64 {
    RNG.with(|s| {
        let mut x = s.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        s.set(x);
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    })
}

impl Executor<'_> {
    /// Whether `name` is a function (not an aggregate): calling it with no arguments
    /// fails with anything but "unknown function". Functions without arguments only read
    /// the clock or the random generator.
    pub(crate) fn function_exists(&self, name: &str) -> bool {
        !matches!(self.call_function(name, Vec::new()), Err(Error::Semantic(m)) if m.starts_with("unknown function"))
    }

    pub fn call_function(&self, name: &str, args: Vec<Value>) -> Result<Value> {
        let a0 = args.first().cloned().unwrap_or_default();
        let store = self.store();
        if let Some(v) = self.temporal_function(name, &args)? {
            return Ok(v);
        }
        if let Some(v) = spatial_function(name, &args)? {
            return Ok(v);
        }
        if let Some(v) = extra_function(name, &args)? {
            return Ok(v);
        }
        match name {
            // ---- graph -------------------------------------------------------------
            "id" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::Node(id) => Ok(Value::Int(id.0 as i64)),
                    Value::Rel(id) => Ok(Value::Int(id.0 as i64)),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "elementid" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::Node(id) => Ok(Value::String(format!("n:{id}"))),
                    Value::Rel(id) => Ok(Value::String(format!("r:{id}"))),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "labels" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::Node(id) => Ok(Value::from(store.node_labels(id)?)),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "type" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::Rel(id) => Ok(store
                        .relationship_type(id)?
                        .map_or(Value::Null, Value::String)),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "keys" | "properties" => {
                arity(name, &args, 1, 1)?;
                let props: BTreeMap<String, Value> = match a0 {
                    Value::Null => return Ok(Value::Null),
                    Value::Node(id) => store.node_properties(id)?,
                    Value::Rel(id) => store.relationship_properties(id)?,
                    Value::Map(m) => m,
                    o => return Err(wrong_type(name, &o)),
                };
                Ok(if name == "keys" {
                    Value::List(props.into_keys().map(Value::String).collect())
                } else {
                    Value::Map(props)
                })
            }
            "startnode" | "endnode" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::Rel(id) => Ok(store
                        .relationship_endpoints(id)?
                        .map_or(Value::Null, |(s, e)| {
                            Value::Node(if name == "startnode" { s } else { e })
                        })),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "nodes" | "relationships" | "rels" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::Path(p) => Ok(if name == "nodes" {
                        Value::List(p.nodes.into_iter().map(Value::Node).collect())
                    } else {
                        Value::List(p.rels.into_iter().map(Value::Rel).collect())
                    }),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "degree" | "indegree" | "outdegree" => {
                arity(name, &args, 1, 1)?;
                let dir = match name {
                    "indegree" => graphbuff_core::Direction::Incoming,
                    "outdegree" => graphbuff_core::Direction::Outgoing,
                    _ => graphbuff_core::Direction::Both,
                };
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::Node(id) => Ok(Value::Int(store.degree(id, dir, &[])? as i64)),
                    o => Err(wrong_type(name, &o)),
                }
            }

            // ---- lists ---------------------------------------------------------------
            "size" | "length" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::List(l) => Ok(Value::Int(l.len() as i64)),
                    Value::String(s) => Ok(Value::Int(s.chars().count() as i64)),
                    Value::Path(p) => Ok(Value::Int(p.rels.len() as i64)),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "head" | "last" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::List(l) => Ok(if name == "head" { l.first() } else { l.last() }
                        .cloned()
                        .unwrap_or_default()),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "tail" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::List(l) => Ok(Value::List(l.into_iter().skip(1).collect())),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "range" => {
                arity(name, &args, 2, 3)?;
                // Only integers: `range(0.0, 1)` fails like `range(true, 1)`.
                if let Some(f) = args.iter().find(|a| matches!(a, Value::Float(_))) {
                    return Err(wrong_type(name, f));
                }
                let start = int_arg(name, &args[0])?;
                let end = int_arg(name, &args[1])?;
                let step = args
                    .get(2)
                    .map(|s| int_arg(name, s))
                    .transpose()?
                    .unwrap_or(1);
                if step == 0 {
                    return Err(Error::Runtime("range() step must not be zero".into()));
                }
                let mut out = Vec::new();
                let mut i = start;
                while (step > 0 && i <= end) || (step < 0 && i >= end) {
                    out.push(Value::Int(i));
                    if out.len() > 10_000_000 {
                        return Err(Error::Runtime(
                            "range() would produce more than 10 million elements".into(),
                        ));
                    }
                    i = match i.checked_add(step) {
                        Some(n) => n,
                        None => break,
                    };
                }
                Ok(Value::List(out))
            }
            "reverse" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::List(mut l) => {
                        l.reverse();
                        Ok(Value::List(l))
                    }
                    Value::String(s) => Ok(Value::String(s.chars().rev().collect())),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "coalesce" => Ok(args.into_iter().find(|v| !v.is_null()).unwrap_or_default()),
            "isempty" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::List(l) => Ok(Value::Bool(l.is_empty())),
                    Value::String(s) => Ok(Value::Bool(s.is_empty())),
                    Value::Map(m) => Ok(Value::Bool(m.is_empty())),
                    o => Err(wrong_type(name, &o)),
                }
            }

            // ---- strings -------------------------------------------------------------
            "toupper" | "upper" => str_fn(name, &a0, str::to_uppercase),
            "tolower" | "lower" => str_fn(name, &a0, str::to_lowercase),
            "replace" => {
                arity(name, &args, 3, 3)?;
                match (&args[0], &args[1], &args[2]) {
                    (Value::String(s), Value::String(from), Value::String(to)) => {
                        Ok(Value::String(s.replace(from.as_str(), to)))
                    }
                    _ if args.iter().any(Value::is_null) => Ok(Value::Null),
                    _ => Err(Error::Type("replace() expects three strings".into())),
                }
            }
            "substring" => {
                arity(name, &args, 2, 3)?;
                if args.iter().any(Value::is_null) {
                    return Ok(Value::Null);
                }
                let Value::String(s) = &args[0] else {
                    return Err(wrong_type(name, &args[0]));
                };
                let start = int_arg(name, &args[1])?.max(0) as usize;
                let chars = s.chars().skip(start);
                Ok(Value::String(match args.get(2) {
                    Some(len) => chars.take(int_arg(name, len)?.max(0) as usize).collect(),
                    None => chars.collect(),
                }))
            }
            "left" | "right" => {
                arity(name, &args, 2, 2)?;
                if args[0].is_null() {
                    return Ok(Value::Null);
                }
                let Value::String(s) = &args[0] else {
                    return Err(wrong_type(name, &args[0]));
                };
                let n = int_arg(name, &args[1])?.max(0) as usize;
                let count = s.chars().count();
                Ok(Value::String(if name == "left" {
                    s.chars().take(n).collect()
                } else {
                    s.chars().skip(count.saturating_sub(n)).collect()
                }))
            }
            "split" => {
                arity(name, &args, 2, 2)?;
                match (&args[0], &args[1]) {
                    (Value::String(s), Value::String(d)) => Ok(Value::List(
                        s.split(d.as_str())
                            .map(|p| Value::String(p.to_string()))
                            .collect(),
                    )),
                    _ if args.iter().any(Value::is_null) => Ok(Value::Null),
                    _ => Err(Error::Type("split() expects two strings".into())),
                }
            }

            // ---- conversion ----------------------------------------------------------
            "tostring" => {
                arity(name, &args, 1, 1)?;
                match &a0 {
                    Value::Null => Ok(Value::Null),
                    Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Temporal(_) | Value::Point(_) => {
                        Ok(Value::String(to_plain_string(&a0)))
                    }
                    o => Err(wrong_type(name, o)),
                }
            }
            "tointeger" | "tointegerornull" => {
                arity(name, &args, 1, 1)?;
                Ok(match a0 {
                    Value::Int(i) => Value::Int(i),
                    Value::Float(f) if f.is_finite() => Value::Int(f.trunc() as i64),
                    Value::Float(_) => Value::Null,
                    Value::Bool(b) => Value::Int(b as i64),
                    Value::String(s) => {
                        let s = s.trim();
                        s.parse::<i64>()
                            .map(Value::Int)
                            .or_else(|_| s.parse::<f64>().map(|f| Value::Int(f.trunc() as i64)))
                            .unwrap_or(Value::Null)
                    }
                    Value::Null => Value::Null,
                    // The `…OrNull` forms turn any other type into null.
                    _ if name.ends_with("ornull") => Value::Null,
                    o => return Err(wrong_type(name, &o)),
                })
            }
            "tofloat" | "tofloatornull" => {
                arity(name, &args, 1, 1)?;
                Ok(match a0 {
                    Value::Int(i) => Value::Float(i as f64),
                    Value::Float(f) => Value::Float(f),
                    Value::String(s) => s
                        .trim()
                        .parse::<f64>()
                        .map(Value::Float)
                        .unwrap_or(Value::Null),
                    Value::Null => Value::Null,
                    // The `…OrNull` forms turn any other type into null.
                    _ if name.ends_with("ornull") => Value::Null,
                    o => return Err(wrong_type(name, &o)),
                })
            }
            "toboolean" | "tobooleanornull" => {
                arity(name, &args, 1, 1)?;
                Ok(match a0 {
                    Value::Bool(b) => Value::Bool(b),
                    Value::Int(i) => Value::Bool(i != 0),
                    Value::String(s) if s.trim().eq_ignore_ascii_case("true") => Value::Bool(true),
                    Value::String(s) if s.trim().eq_ignore_ascii_case("false") => {
                        Value::Bool(false)
                    }
                    Value::String(_) => Value::Null,
                    Value::Null => Value::Null,
                    // The `…OrNull` forms turn any other type into null.
                    _ if name.ends_with("ornull") => Value::Null,
                    o => return Err(wrong_type(name, &o)),
                })
            }

            // ---- math ----------------------------------------------------------------
            "abs" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::Int(i) => i
                        .checked_abs()
                        .map(Value::Int)
                        .ok_or_else(|| Error::Runtime("integer overflow".into())),
                    Value::Float(f) => Ok(Value::Float(f.abs())),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "sign" => {
                arity(name, &args, 1, 1)?;
                match a0 {
                    Value::Null => Ok(Value::Null),
                    Value::Int(i) => Ok(Value::Int(i.signum())),
                    Value::Float(f) => Ok(Value::Int(if f > 0.0 {
                        1
                    } else if f < 0.0 {
                        -1
                    } else {
                        0
                    })),
                    o => Err(wrong_type(name, &o)),
                }
            }
            "ceil" => num_fn(name, &a0, f64::ceil),
            "floor" => num_fn(name, &a0, f64::floor),
            "round" if args.len() > 1 => {
                arity(name, &args, 2, 3)?;
                if args.iter().any(Value::is_null) {
                    return Ok(Value::Null);
                }
                let x = args[0].as_float().ok_or_else(|| wrong_type(name, &args[0]))?;
                let precision = int_arg(name, &args[1])?;
                let mode = match args.get(2) {
                    None => RoundMode::HalfUp,
                    Some(Value::String(m)) => RoundMode::parse(m)?,
                    Some(o) => return Err(wrong_type(name, o)),
                };
                Ok(Value::Float(round_decimal(x, precision, mode)))
            }
            "round" => num_fn(name, &a0, f64::round),
            "sqrt" => num_fn(name, &a0, f64::sqrt),
            "exp" => num_fn(name, &a0, f64::exp),
            "log" => num_fn(name, &a0, f64::ln),
            "log10" => num_fn(name, &a0, f64::log10),
            "sin" => num_fn(name, &a0, f64::sin),
            "cos" => num_fn(name, &a0, f64::cos),
            "tan" => num_fn(name, &a0, f64::tan),
            "pi" => Ok(Value::Float(std::f64::consts::PI)),
            "e" => Ok(Value::Float(std::f64::consts::E)),
            "rand" => Ok(Value::Float(next_random())),
            "timestamp" => Ok(Value::Int(
                graphbuff_core::clock::unix_time().map(|d| d.as_millis() as i64).unwrap_or(0),
            )),

            _ => Err(Error::Semantic(format!("unknown function '{name}'"))),
        }
    }
}

/// Trims the characters of `chars` (default: whitespace) from the start and/or end.
fn trim_fn(name: &str, args: &[Value], start: bool, end: bool) -> Result<Value> {
    arity(name, args, 1, 2)?;
    if args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let Value::String(s) = &args[0] else {
        return Err(wrong_type(name, &args[0]));
    };
    let set: Option<Vec<char>> = match args.get(1) {
        None => None,
        Some(Value::String(c)) => Some(c.chars().collect()),
        Some(o) => return Err(wrong_type(name, o)),
    };
    let strip = |c: char| match &set {
        None => c.is_whitespace(),
        Some(set) => set.contains(&c),
    };
    let mut out = s.as_str();
    if start {
        out = out.trim_start_matches(strip);
    }
    if end {
        out = out.trim_end_matches(strip);
    }
    Ok(Value::String(out.to_string()))
}

/// `toString` for the values it accepts; `None` for the others.
fn to_string_or_none(v: &Value) -> Option<Value> {
    match v {
        Value::Null => Some(Value::Null),
        Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Temporal(_) | Value::Point(_) => {
            Some(Value::String(to_plain_string(v)))
        }
        _ => None,
    }
}

/// The `…OrNull` conversions, per list element.
fn convert(kind: &str, v: &Value) -> Value {
    match (kind, v) {
        (_, Value::Null) => Value::Null,
        ("integer", Value::Int(i)) => Value::Int(*i),
        ("integer", Value::Float(f)) if f.is_finite() => Value::Int(f.trunc() as i64),
        ("integer", Value::Bool(b)) => Value::Int(*b as i64),
        ("integer", Value::String(s)) => {
            let s = s.trim();
            s.parse::<i64>()
                .map(Value::Int)
                .or_else(|_| s.parse::<f64>().map(|f| Value::Int(f.trunc() as i64)))
                .unwrap_or(Value::Null)
        }
        ("float", Value::Int(i)) => Value::Float(*i as f64),
        ("float", Value::Float(f)) => Value::Float(*f),
        ("float", Value::String(s)) => s.trim().parse::<f64>().map(Value::Float).unwrap_or(Value::Null),
        ("boolean", Value::Bool(b)) => Value::Bool(*b),
        ("boolean", Value::Int(i)) => Value::Bool(*i != 0),
        ("boolean", Value::String(s)) if s.trim().eq_ignore_ascii_case("true") => Value::Bool(true),
        ("boolean", Value::String(s)) if s.trim().eq_ignore_ascii_case("false") => Value::Bool(false),
        ("string", v) => to_string_or_none(v).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// A list argument; `None` for null.
fn list_arg(name: &str, v: &Value) -> Result<Option<Vec<Value>>> {
    match v {
        Value::Null => Ok(None),
        Value::List(l) => Ok(Some(l.clone())),
        o => Err(wrong_type(name, o)),
    }
}

/// Cypher's name for the type of `v`, as `valueType()` returns it.
fn value_type(v: &Value) -> String {
    fn base(v: &Value) -> String {
        use graphbuff_core::temporal::TemporalKind as K;
        match v {
            Value::Null => "NULL".into(),
            Value::Bool(_) => "BOOLEAN".into(),
            Value::Int(_) => "INTEGER".into(),
            Value::Float(_) => "FLOAT".into(),
            Value::String(_) => "STRING".into(),
            Value::Map(_) => "MAP".into(),
            Value::Node(_) => "NODE".into(),
            Value::Rel(_) => "RELATIONSHIP".into(),
            Value::Path(_) => "PATH".into(),
            Value::Point(_) => "POINT".into(),
            Value::Temporal(t) => match t.kind() {
                K::Date => "DATE",
                K::LocalTime => "LOCAL TIME",
                K::Time => "ZONED TIME",
                K::LocalDateTime => "LOCAL DATETIME",
                K::DateTime => "ZONED DATETIME",
                K::Duration => "DURATION",
            }
            .into(),
            Value::List(items) => {
                let mut types: Vec<String> = Vec::new();
                for t in items.iter().filter(|i| !i.is_null()).map(base) {
                    if !types.contains(&t) {
                        types.push(t);
                    }
                }
                let nullable = items.iter().any(Value::is_null);
                let inner = if items.is_empty() {
                    "NOTHING".to_string()
                } else if types.is_empty() {
                    "NULL".to_string()
                } else {
                    let suffix = if nullable { "" } else { " NOT NULL" };
                    types.iter().map(|t| format!("{t}{suffix}")).collect::<Vec<_>>().join(" | ")
                };
                format!("LIST<{inner}>")
            }
        }
    }
    match v {
        Value::Null => "NULL".into(),
        v => format!("{} NOT NULL", base(v)),
    }
}

/// A random (version 4) UUID.
fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(6) {
        let r = (next_random() * (1u64 << 48) as f64) as u64;
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = (r >> (8 * i)) as u8;
        }
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// Rounding modes of `round(x, precision, mode)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RoundMode {
    Up,
    Down,
    Ceiling,
    Floor,
    HalfUp,
    HalfDown,
    HalfEven,
}

impl RoundMode {
    fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_uppercase().as_str() {
            "UP" => Self::Up,
            "DOWN" => Self::Down,
            "CEILING" => Self::Ceiling,
            "FLOOR" => Self::Floor,
            "HALF_UP" => Self::HalfUp,
            "HALF_DOWN" => Self::HalfDown,
            "HALF_EVEN" => Self::HalfEven,
            _ => {
                return Err(Error::Runtime(format!(
                    "unknown rounding mode `{s}` (UP, DOWN, CEILING, FLOOR, HALF_UP, HALF_DOWN or HALF_EVEN)"
                )));
            }
        })
    }
}

/// Rounds `x` to `precision` decimal places, on its shortest decimal form (so
/// `round(2.675, 2)` is 2.68, as written, not 2.67 as the binary value would give).
fn round_decimal(x: f64, precision: i64, mode: RoundMode) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let neg = x < 0.0;
    // `d.ddddde±x`: the digits, and how many of them come before the decimal point.
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').expect("scientific notation");
    let digits: Vec<u8> = mantissa.bytes().filter(u8::is_ascii_digit).map(|b| b - b'0').collect();
    let point = exp.parse::<i64>().expect("an exponent") + 1;
    let keep = point.saturating_add(precision);
    if keep >= digits.len() as i64 {
        return x; // nothing to round away
    }
    let k = keep.max(0) as usize;
    let rest = &digits[k..];
    // The first discarded digit (0 if the rounding position lies left of every digit).
    let first = if keep >= 0 { rest[0] } else { 0 };
    let after_first_nonzero = if keep >= 0 { rest[1..].iter().any(|&d| d != 0) } else { true };
    let nonzero = rest.iter().any(|&d| d != 0);
    let above_half = first > 5 || (first == 5 && after_first_nonzero);
    let half = first == 5 && !after_first_nonzero;
    let last_odd = k > 0 && digits[k - 1] % 2 == 1;
    let up = match mode {
        RoundMode::Up => nonzero,
        RoundMode::Down => false,
        RoundMode::Ceiling => nonzero && !neg,
        RoundMode::Floor => nonzero && neg,
        RoundMode::HalfUp => above_half || half,
        RoundMode::HalfDown => above_half,
        RoundMode::HalfEven => above_half || (half && last_odd),
    };
    let mut kept: String = digits[..k].iter().map(|d| char::from(b'0' + d)).collect();
    if kept.is_empty() {
        kept.push('0');
    }
    let mut m: u128 = kept.parse().expect("at most 17 digits");
    if up {
        m += 1;
    }
    let magnitude: f64 = format!("{m}e{}", -precision).parse().unwrap_or(0.0);
    if neg { -magnitude } else { magnitude }
}

/// String, list, math and scalar functions (and `apoc`-style basics);
/// `None` if `name` is not one of them.
fn extra_function(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let a0 = args.first().cloned().unwrap_or_default();
    let trig = |f: fn(f64) -> f64| -> Result<Value> {
        arity(name, args, 1, 1)?;
        num_fn(name, &a0, f)
    };
    Ok(Some(match name {
        // ---- strings ---------------------------------------------------------------
        "trim" | "btrim" => trim_fn(name, args, true, true)?,
        "ltrim" => trim_fn(name, args, true, false)?,
        "rtrim" => trim_fn(name, args, false, true)?,
        "char_length" | "character_length" => {
            arity(name, args, 1, 1)?;
            match &a0 {
                Value::Null => Value::Null,
                Value::String(s) => Value::Int(s.chars().count() as i64),
                o => return Err(wrong_type(name, o)),
            }
        }
        "tostringornull" => {
            arity(name, args, 1, 1)?;
            to_string_or_none(&a0).unwrap_or(Value::Null)
        }
        "normalize" | IS_NORMALIZED => {
            arity(name, args, 1, 2)?;
            let form = match args.get(1) {
                None => NormalForm::Nfc,
                Some(Value::String(f)) => NormalForm::parse(f).ok_or_else(|| {
                    Error::Runtime(format!("unknown normal form {f:?}; use NFC, NFD, NFKC or NFKD"))
                })?,
                Some(o) => return Err(wrong_type(name, o)),
            };
            match &a0 {
                Value::Null => Value::Null,
                Value::String(s) if name == IS_NORMALIZED => Value::Bool(is_normalized(s, form)),
                Value::String(s) => Value::String(normalize(s, form)),
                o if name == IS_NORMALIZED => {
                    return Err(Error::Type(format!("IS NORMALIZED does not accept {}", o.type_name())));
                }
                o => return Err(wrong_type(name, o)),
            }
        }

        // ---- lists -----------------------------------------------------------------
        "tointegerlist" | "tofloatlist" | "tobooleanlist" | "tostringlist" => {
            arity(name, args, 1, 1)?;
            let kind = &name[2..name.len() - 4];
            match list_arg(name, &a0)? {
                None => Value::Null,
                Some(l) => Value::List(l.iter().map(|v| convert(kind, v)).collect()),
            }
        }

        // ---- scalar ----------------------------------------------------------------
        "nullif" => {
            arity(name, args, 2, 2)?;
            if cypher_eq(&args[0], &args[1]) == Some(true) { Value::Null } else { a0 }
        }
        "valuetype" => {
            arity(name, args, 1, 1)?;
            Value::String(value_type(&a0))
        }
        "randomuuid" => {
            arity(name, args, 0, 0)?;
            Value::String(random_uuid())
        }

        // ---- math ------------------------------------------------------------------
        "isnan" => {
            arity(name, args, 1, 1)?;
            match a0 {
                Value::Null => Value::Null,
                Value::Int(_) => Value::Bool(false),
                Value::Float(f) => Value::Bool(f.is_nan()),
                o => return Err(wrong_type(name, &o)),
            }
        }
        "acos" => trig(f64::acos)?,
        "asin" => trig(f64::asin)?,
        "atan" => trig(f64::atan)?,
        "cot" => trig(|x| 1.0 / x.tan())?,
        "degrees" => trig(f64::to_degrees)?,
        "radians" => trig(f64::to_radians)?,
        "haversin" => trig(|x| (1.0 - x.cos()) / 2.0)?,
        "atan2" => {
            arity(name, args, 2, 2)?;
            if args.iter().any(Value::is_null) {
                return Ok(Some(Value::Null));
            }
            let y = args[0].as_float().ok_or_else(|| wrong_type(name, &args[0]))?;
            let x = args[1].as_float().ok_or_else(|| wrong_type(name, &args[1]))?;
            Value::Float(y.atan2(x))
        }

        // ---- apoc-style basics -----------------------------------------------------
        "apoc.coll.sum" | "apoc.coll.avg" => {
            arity(name, args, 1, 1)?;
            let Some(l) = list_arg(name, &a0)? else {
                return Ok(Some(Value::Null));
            };
            let nums: Vec<f64> = l
                .iter()
                .filter(|v| !v.is_null())
                .map(|v| v.as_float().ok_or_else(|| wrong_type(name, v)))
                .collect::<Result<_>>()?;
            if name == "apoc.coll.sum" {
                Value::Float(nums.iter().sum())
            } else if nums.is_empty() {
                Value::Null
            } else {
                Value::Float(nums.iter().sum::<f64>() / nums.len() as f64)
            }
        }
        "apoc.coll.min" | "apoc.coll.max" => {
            arity(name, args, 1, 1)?;
            let Some(l) = list_arg(name, &a0)? else {
                return Ok(Some(Value::Null));
            };
            let it = l.into_iter().filter(|v| !v.is_null());
            let pick = if name == "apoc.coll.min" { it.min_by(order_cmp) } else { it.max_by(order_cmp) };
            pick.unwrap_or(Value::Null)
        }
        "apoc.coll.toset" => {
            arity(name, args, 1, 1)?;
            let Some(l) = list_arg(name, &a0)? else {
                return Ok(Some(Value::Null));
            };
            let mut out: Vec<Value> = Vec::new();
            for v in l {
                if !out.iter().any(|o| o == &v) {
                    out.push(v);
                }
            }
            Value::List(out)
        }
        "apoc.coll.sort" => {
            arity(name, args, 1, 1)?;
            let Some(mut l) = list_arg(name, &a0)? else {
                return Ok(Some(Value::Null));
            };
            l.sort_by(order_cmp);
            Value::List(l)
        }
        "apoc.coll.flatten" => {
            arity(name, args, 1, 2)?;
            let deep = matches!(args.get(1), Some(Value::Bool(true)));
            fn flatten(l: Vec<Value>, deep: bool, out: &mut Vec<Value>) {
                for v in l {
                    match v {
                        Value::List(inner) if deep => flatten(inner, deep, out),
                        Value::List(inner) => out.extend(inner),
                        v => out.push(v),
                    }
                }
            }
            let Some(l) = list_arg(name, &a0)? else {
                return Ok(Some(Value::Null));
            };
            let mut out = Vec::new();
            flatten(l, deep, &mut out);
            Value::List(out)
        }
        "apoc.coll.contains" | "apoc.coll.indexof" => {
            arity(name, args, 2, 2)?;
            let Some(l) = list_arg(name, &a0)? else {
                return Ok(Some(if name == "apoc.coll.contains" { Value::Bool(false) } else { Value::Int(-1) }));
            };
            let pos = l.iter().position(|v| cypher_eq(v, &args[1]) == Some(true));
            if name == "apoc.coll.contains" {
                Value::Bool(pos.is_some())
            } else {
                Value::Int(pos.map_or(-1, |p| p as i64))
            }
        }
        "apoc.text.join" => {
            arity(name, args, 2, 2)?;
            let (Some(l), Value::String(sep)) = (list_arg(name, &a0)?, &args[1]) else {
                return Ok(Some(Value::Null));
            };
            let parts: Vec<String> = l
                .iter()
                .filter(|v| !v.is_null())
                .map(|v| match v {
                    Value::String(s) => Ok(s.clone()),
                    o => Err(wrong_type(name, o)),
                })
                .collect::<Result<_>>()?;
            Value::String(parts.join(sep))
        }
        "apoc.map.merge" => {
            arity(name, args, 2, 2)?;
            let mut out = BTreeMap::new();
            for a in args {
                match a {
                    Value::Null => {}
                    Value::Map(m) => out.extend(m.clone()),
                    o => return Err(wrong_type(name, o)),
                }
            }
            Value::Map(out)
        }
        "apoc.map.frompairs" => {
            arity(name, args, 1, 1)?;
            let Some(l) = list_arg(name, &a0)? else {
                return Ok(Some(Value::Null));
            };
            let mut out = BTreeMap::new();
            for pair in l {
                match pair {
                    Value::List(kv) if kv.len() == 2 => match &kv[0] {
                        Value::String(k) => {
                            out.insert(k.clone(), kv[1].clone());
                        }
                        o => return Err(wrong_type(name, o)),
                    },
                    o => {
                        return Err(Error::Type(format!(
                            "apoc.map.fromPairs() expects [key, value] pairs, got {}",
                            o.type_name()
                        )));
                    }
                }
            }
            Value::Map(out)
        }
        n if n.starts_with("apoc.") => {
            return Err(Error::Unsupported(format!(
                "function `{name}` is not supported (only a few apoc-style basics are built in)"
            )));
        }
        _ => return Ok(None),
    }))
}

/// The temporal kind a constructor name (`date`, `localtime`, …) makes.
fn temporal_kind(name: &str) -> Option<TemporalKind> {
    Some(match name {
        "date" => TemporalKind::Date,
        "localtime" => TemporalKind::LocalTime,
        "time" => TemporalKind::Time,
        "localdatetime" => TemporalKind::LocalDateTime,
        "datetime" => TemporalKind::DateTime,
        "duration" => TemporalKind::Duration,
        _ => return None,
    })
}

impl Executor<'_> {
    /// Temporal constructors and functions; `None` if `name` is not one of them.
    fn temporal_function(&self, name: &str, args: &[Value]) -> Result<Option<Value>> {
        let (base, member) = match name.split_once('.') {
            Some((b, m)) => (b, Some(m)),
            None => (name, None),
        };
        let Some(kind) = temporal_kind(base) else {
            return Ok(None);
        };
        let temporal = |v: &Value| -> Result<Option<Temporal>> {
            match v {
                Value::Null => Ok(None),
                Value::Temporal(t) => Ok(Some(*t)),
                o => Err(wrong_type(name, o)),
            }
        };
        Ok(Some(match (kind, member) {
            (TemporalKind::Duration, None) => {
                arity(name, args, 1, 1)?;
                Temporal::construct(kind, args.first(), self.statement_time())?
            }
            (_, None) => {
                arity(name, args, 0, 1)?;
                Temporal::construct(kind, args.first(), self.statement_time())?
            }
            // The current time: one clock per statement; in a time zone if one is given.
            (k, Some("statement" | "transaction" | "realtime")) if k != TemporalKind::Duration => {
                arity(name, args, 0, 1)?;
                Temporal::now_in(kind, args.first(), self.statement_time())?
            }
            (k, Some("truncate")) if k != TemporalKind::Duration => {
                arity(name, args, 2, 3)?;
                let Some(t) = temporal(&args[1])? else {
                    return Ok(Some(Value::Null));
                };
                let unit = match &args[0] {
                    Value::Null => return Ok(Some(Value::Null)),
                    Value::String(u) => u.as_str(),
                    o => return Err(wrong_type(name, o)),
                };
                let empty = std::collections::BTreeMap::new();
                let fields = match args.get(2) {
                    None | Some(Value::Null) => &empty,
                    Some(Value::Map(m)) => m,
                    Some(o) => return Err(wrong_type(name, o)),
                };
                Value::Temporal(Temporal::truncate(k, unit, &t, fields)?)
            }
            (TemporalKind::DateTime, Some("fromepoch")) => {
                arity(name, args, 1, 2)?;
                if args.iter().any(Value::is_null) {
                    return Ok(Some(Value::Null));
                }
                let secs = int_arg(name, &args[0])? as i128;
                let nanos = args.get(1).map(|n| int_arg(name, n)).transpose()?.unwrap_or(0) as i128;
                Value::Temporal(Temporal::from_epoch_nanos(secs * NANOS_PER_SECOND as i128 + nanos)?)
            }
            (TemporalKind::DateTime, Some("fromepochmillis")) => {
                arity(name, args, 1, 1)?;
                if args[0].is_null() {
                    return Ok(Some(Value::Null));
                }
                let ms = int_arg(name, &args[0])? as i128;
                Value::Temporal(Temporal::from_epoch_nanos(ms * 1_000_000)?)
            }
            (TemporalKind::Duration, Some(m @ ("between" | "inmonths" | "indays" | "inseconds"))) => {
                arity(name, args, 2, 2)?;
                let (Some(a), Some(b)) = (temporal(&args[0])?, temporal(&args[1])?) else {
                    return Ok(Some(Value::Null));
                };
                let unit = match m {
                    "between" => BetweenUnit::All,
                    "inmonths" => BetweenUnit::Months,
                    "indays" => BetweenUnit::Days,
                    _ => BetweenUnit::Seconds,
                };
                Value::Temporal(Temporal::Duration(Temporal::between(&a, &b, unit)?))
            }
            _ => return Err(Error::Unsupported(format!("function `{name}` is not supported"))),
        }))
    }
}

/// `point(…)`, `point.distance(…)` and `point.withinBBox(…)`; `None` if `name` is not
/// one of them.
fn spatial_function(name: &str, args: &[Value]) -> Result<Option<Value>> {
    // A point argument: `None` for null.
    let point = |v: &Value| -> Result<Option<Point>> {
        match v {
            Value::Null => Ok(None),
            Value::Point(p) => Ok(Some(*p)),
            o => Err(wrong_type(name, o)),
        }
    };
    Ok(Some(match name {
        "point" => {
            arity(name, args, 1, 1)?;
            match &args[0] {
                Value::Null => Value::Null,
                Value::Map(m) => Point::from_map(m)?.map_or(Value::Null, Value::Point),
                o => return Err(wrong_type(name, o)),
            }
        }
        "point.distance" => {
            arity(name, args, 2, 2)?;
            match (point(&args[0])?, point(&args[1])?) {
                (Some(a), Some(b)) => a.distance(&b).map_or(Value::Null, Value::Float),
                _ => Value::Null,
            }
        }
        "point.withinbbox" => {
            arity(name, args, 3, 3)?;
            match (point(&args[0])?, point(&args[1])?, point(&args[2])?) {
                (Some(p), Some(lo), Some(hi)) => p.within_bbox(&lo, &hi).map_or(Value::Null, Value::Bool),
                _ => Value::Null,
            }
        }
        n if n.starts_with("point.") => {
            return Err(Error::Unsupported(format!("function `{name}` is not supported")));
        }
        _ => return Ok(None),
    }))
}
