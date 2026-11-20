//! A small JSON reader and writer (RFC 8259), and the JSON form of query results and
//! errors: the one the HTTP API of `graphbuff-server` sends.
//!
//! Parsing produces [`Json`]; numbers keep whether they were written as integers, so
//! `1` becomes a Cypher INTEGER and `1.0` a FLOAT. Writing goes straight to a `String`.

use std::collections::BTreeMap;
use std::fmt::Write;

use crate::{
    Error, NodeData, PathData, PlanDescription, Point, QueryResult, QueryStats, RelData, ResultValue, Value,
};

/// Nesting limit for arrays and objects, so hostile input cannot overflow the stack.
const MAX_DEPTH: usize = 128;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    pub fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "boolean",
            Json::Int(_) | Json::Float(_) => "number",
            Json::String(_) => "string",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }
}

/// A parse error with the byte offset where it was detected.
#[derive(Debug, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid JSON at byte {}: {}", self.offset, self.message)
    }
}

pub fn parse(text: &str) -> Result<Json, ParseError> {
    let mut p = Parser {
        s: text.as_bytes(),
        pos: 0,
    };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.pos != p.s.len() {
        return Err(p.err("unexpected data after the value"));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            offset: self.pos,
        }
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.s.get(self.pos) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn expect(&mut self, lit: &str) -> Result<(), ParseError> {
        if self.s[self.pos..].starts_with(lit.as_bytes()) {
            self.pos += lit.len();
            Ok(())
        } else {
            Err(self.err(format!("expected `{lit}`")))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting is too deep"));
        }
        match self.peek() {
            None => Err(self.err("unexpected end of input")),
            Some(b'n') => self.expect("null").map(|_| Json::Null),
            Some(b't') => self.expect("true").map(|_| Json::Bool(true)),
            Some(b'f') => self.expect("false").map(|_| Json::Bool(false)),
            Some(b'"') => self.string().map(Json::String),
            Some(b'[') => {
                self.pos += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                loop {
                    self.ws();
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(Json::Array(items));
                        }
                        _ => return Err(self.err("expected `,` or `]`")),
                    }
                }
            }
            Some(b'{') => {
                self.pos += 1;
                let mut map = BTreeMap::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.pos += 1;
                    return Ok(Json::Object(map));
                }
                loop {
                    self.ws();
                    if self.peek() != Some(b'"') {
                        return Err(self.err("expected a string key"));
                    }
                    let k = self.string()?;
                    self.ws();
                    self.expect(":")?;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    map.insert(k, v);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b'}') => {
                            self.pos += 1;
                            return Ok(Json::Object(map));
                        }
                        _ => return Err(self.err("expected `,` or `}`")),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.pos;
        let digits = |p: &mut Self| {
            let s = p.pos;
            while let Some(b'0'..=b'9') = p.peek() {
                p.pos += 1;
            }
            p.pos - s
        };
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let int_start = self.pos;
        let n = digits(self);
        if n == 0 || (n > 1 && self.s[int_start] == b'0') {
            return Err(self.err("invalid number"));
        }
        let mut integral = true;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            integral = false;
            if digits(self) == 0 {
                return Err(self.err("expected digits after `.`"));
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.pos += 1;
            integral = false;
            if let Some(b'+' | b'-') = self.peek() {
                self.pos += 1;
            }
            if digits(self) == 0 {
                return Err(self.err("expected digits in the exponent"));
            }
        }
        let text = std::str::from_utf8(&self.s[start..self.pos]).expect("ASCII");
        if integral && let Ok(i) = text.parse::<i64>() {
            return Ok(Json::Int(i));
        }
        // Fractions, exponents, and integers beyond 64 bits.
        text.parse::<f64>()
            .map(Json::Float)
            .map_err(|_| self.err("invalid number"))
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let h = self
            .s
            .get(self.pos..self.pos + 4)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or_else(|| self.err("invalid \\u escape"))?;
        self.pos += 4;
        Ok(h)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // The input came from a &str and we stopped at ASCII bytes, so this is UTF-8.
            out.push_str(std::str::from_utf8(&self.s[start..self.pos]).expect("UTF-8"));
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let c = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.pos += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi) {
                                // A surrogate pair: `\uD83D\uDE00`.
                                self.expect("\\u")?;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(self.err("invalid low surrogate"));
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            out.push(
                                char::from_u32(code)
                                    .ok_or_else(|| self.err("invalid code point"))?,
                            );
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                Some(_) => return Err(self.err("control character in string")),
            }
        }
    }
}

// ---- writing ------------------------------------------------------------------------------

/// Appends `s` as a JSON string literal.
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Appends a float. Integral values keep a `.0` so that readers see a floating-point
/// number. JSON has no NaN or infinities; they are written as the strings `"NaN"`,
/// `"Infinity"` and `"-Infinity"`.
pub fn write_float(out: &mut String, f: f64) {
    if f.is_nan() {
        out.push_str("\"NaN\"");
    } else if f.is_infinite() {
        out.push_str(if f > 0.0 { "\"Infinity\"" } else { "\"-Infinity\"" });
    } else {
        // `{:?}` prints the shortest representation that round-trips, with `.0` or an
        // exponent, both valid JSON.
        let _ = write!(out, "{f:?}");
    }
}

// ---- query parameters ---------------------------------------------------------------------

/// The Cypher value a JSON value stands for, as a query parameter.
pub fn to_value(j: Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(b),
        Json::Int(i) => Value::Int(i),
        Json::Float(f) => Value::Float(f),
        Json::String(s) => Value::String(s),
        Json::Array(items) => Value::List(items.into_iter().map(to_value).collect()),
        Json::Object(m) => Value::Map(m.into_iter().map(|(k, v)| (k, to_value(v))).collect()),
    }
}

// ---- errors -------------------------------------------------------------------------------

/// The name of the kind of `e`: the `"code"` of its JSON form.
pub fn error_code(e: &Error) -> &'static str {
    match e {
        Error::Syntax { .. } => "SyntaxError",
        Error::Semantic(_) => "SemanticError",
        Error::Type(_) => "TypeError",
        Error::Runtime(_) => "RuntimeError",
        Error::Unsupported(_) => "Unsupported",
        Error::Constraint(_) => "ConstraintError",
        Error::Storage(_) => "StorageError",
        Error::TransactionTooLarge(_) => "TransactionTooLarge",
        Error::Timeout(_) => "Timeout",
        Error::Cancelled(_) => "Cancelled",
        Error::MemoryLimit(_) => "MemoryLimit",
        Error::Forbidden(_) => "Forbidden",
    }
}

/// `{"error": {"code": "...", "message": "..."}}`.
pub fn encode_error_message(code: &str, message: &str) -> String {
    let mut body = String::from(r#"{"error":{"code":"#);
    write_str(&mut body, code);
    body.push_str(r#","message":"#);
    write_str(&mut body, message);
    body.push_str("}}");
    body
}

/// An engine error as `{"error": {"code": "...", "message": "..."}}`; a syntax error also
/// has `"offset"`, the byte of the query text where it was detected.
pub fn encode_error(e: &Error) -> String {
    match e {
        Error::Syntax { message, offset } => {
            let mut body = String::from(r#"{"error":{"code":"SyntaxError","message":"#);
            write_str(&mut body, message);
            let _ = write!(body, r#","offset":{offset}}}}}"#);
            body
        }
        Error::Semantic(m)
        | Error::Type(m)
        | Error::Runtime(m)
        | Error::Unsupported(m)
        | Error::Constraint(m)
        | Error::Storage(m)
        | Error::TransactionTooLarge(m)
        | Error::Timeout(m)
        | Error::Cancelled(m)
        | Error::MemoryLimit(m)
        | Error::Forbidden(m) => encode_error_message(error_code(e), m),
    }
}

// ---- results ------------------------------------------------------------------------------

/// A query result as `{"columns": [...], "rows": [[...]], "stats": {...}}`, with
/// `"plan"` for `EXPLAIN` and `PROFILE`.
pub fn encode_result(r: &QueryResult) -> String {
    let mut out = String::with_capacity(64 + r.rows.len() * 32);
    write_result_head(&mut out, &r.columns);
    for (i, row) in r.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_row(&mut out, row);
    }
    write_result_tail(&mut out, &r.stats, r.plan.as_ref());
    out
}

/// The start of [`encode_result`]'s object, up to the opening of `rows`; then come the
/// rows ([`write_row`], separated by commas) and [`write_result_tail`]. For writing a
/// result as its rows are produced.
pub fn write_result_head(out: &mut String, columns: &[String]) {
    out.push_str(r#"{"columns":["#);
    for (i, c) in columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_str(out, c);
    }
    out.push_str(r#"],"rows":["#);
}

/// One row as a JSON array.
pub fn write_row(out: &mut String, row: &[ResultValue]) {
    out.push('[');
    for (j, v) in row.iter().enumerate() {
        if j > 0 {
            out.push(',');
        }
        write_result_value(out, v);
    }
    out.push(']');
}

/// The end of [`encode_result`]'s object, from the end of `rows`.
pub fn write_result_tail(out: &mut String, stats: &QueryStats, plan: Option<&PlanDescription>) {
    out.push_str(r#"],"stats":"#);
    write_stats(out, stats);
    if let Some(plan) = plan {
        out.push_str(r#","plan":"#);
        write_plan(out, plan);
    }
    out.push('}');
}

/// `{"profiled": bool, "lines": [{"depth", "operator", "estimatedRows", "rows", "timeMs"}]}`;
/// numbers that were not estimated or measured are `null`.
fn write_plan(out: &mut String, plan: &PlanDescription) {
    let _ = write!(out, r#"{{"profiled":{},"lines":["#, plan.profiled);
    let num = |out: &mut String, v: Option<f64>| match v {
        Some(v) if v.is_finite() => write_float(out, v),
        _ => out.push_str("null"),
    };
    for (i, l) in plan.lines.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, r#"{{"depth":{},"operator":"#, l.depth);
        write_str(out, &l.operator);
        out.push_str(r#","estimatedRows":"#);
        num(out, l.estimated_rows);
        out.push_str(r#","rows":"#);
        match l.rows {
            Some(r) => {
                let _ = write!(out, "{r}");
            }
            None => out.push_str("null"),
        }
        out.push_str(r#","timeMs":"#);
        num(out, l.time.map(|t| t.as_secs_f64() * 1e3));
        out.push('}');
    }
    out.push_str("]}");
}

fn write_stats(out: &mut String, s: &QueryStats) {
    let _ = write!(
        out,
        concat!(
            r#"{{"nodesCreated":{},"nodesDeleted":{},"relationshipsCreated":{},"#,
            r#""relationshipsDeleted":{},"propertiesSet":{},"labelsAdded":{},"#,
            r#""labelsRemoved":{},"indexesAdded":{},"indexesRemoved":{},"#,
            r#""constraintsAdded":{},"constraintsRemoved":{},"#,
            r#""systemUpdates":{},"containsUpdates":{}}}"#
        ),
        s.nodes_created,
        s.nodes_deleted,
        s.relationships_created,
        s.relationships_deleted,
        s.properties_set,
        s.labels_added,
        s.labels_removed,
        s.indexes_added,
        s.indexes_removed,
        s.constraints_added,
        s.constraints_removed,
        s.system_updates,
        s.contains_updates()
    );
}

/// Property values (scalars and lists) as plain JSON.
fn write_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Float(f) => write_float(out, *f),
        Value::String(s) => write_str(out, s),
        Value::List(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, x);
            }
            out.push(']');
        }
        Value::Map(m) => write_props(out, m),
        // Not storable as properties; never part of a materialized result's properties.
        Value::Node(id) => {
            let _ = write!(out, r#"{{"$type":"node","id":{id}}}"#);
        }
        Value::Rel(id) => {
            let _ = write!(out, r#"{{"$type":"relationship","id":{id}}}"#);
        }
        Value::Path(_) => out.push_str("null"),
        Value::Temporal(t) => write_str(out, &t.to_string()),
        Value::Point(p) => write_point(out, p),
    }
}

fn write_props(out: &mut String, m: &BTreeMap<String, Value>) {
    out.push('{');
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_str(out, k);
        out.push(':');
        write_value(out, v);
    }
    out.push('}');
}

fn write_node(out: &mut String, n: &NodeData) {
    let _ = write!(out, r#"{{"$type":"node","id":{},"labels":["#, n.id);
    for (i, l) in n.labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_str(out, l);
    }
    out.push_str(r#"],"properties":"#);
    write_props(out, &n.properties);
    out.push('}');
}

fn write_rel(out: &mut String, r: &RelData) {
    let _ = write!(out, r#"{{"$type":"relationship","id":{},"type":"#, r.id);
    write_str(out, &r.rel_type);
    let _ = write!(out, r#","start":{},"end":{},"properties":"#, r.start, r.end);
    write_props(out, &r.properties);
    out.push('}');
}

fn write_path(out: &mut String, p: &PathData) {
    out.push_str(r#"{"$type":"path","nodes":["#);
    for (i, n) in p.nodes.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_node(out, n);
    }
    out.push_str(r#"],"relationships":["#);
    for (i, r) in p.relationships.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_rel(out, r);
    }
    out.push_str("]}");
}

fn write_result_value(out: &mut String, v: &ResultValue) {
    match v {
        ResultValue::Null => out.push_str("null"),
        ResultValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        ResultValue::Int(i) => {
            let _ = write!(out, "{i}");
        }
        ResultValue::Float(f) => write_float(out, *f),
        ResultValue::String(s) => write_str(out, s),
        ResultValue::List(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_result_value(out, x);
            }
            out.push(']');
        }
        ResultValue::Map(m) => {
            out.push('{');
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(out, k);
                out.push(':');
                write_result_value(out, v);
            }
            out.push('}');
        }
        ResultValue::Node(n) => write_node(out, n),
        ResultValue::Relationship(r) => write_rel(out, r),
        ResultValue::Path(p) => write_path(out, p),
        ResultValue::Temporal(t) => write_str(out, &t.to_string()),
        ResultValue::Point(p) => write_point(out, p),
    }
}

/// A point as `{"$type":"point","crs":"cartesian","srid":7203,"x":1.0,"y":2.0}` (and `"z"`).
fn write_point(out: &mut String, p: &Point) {
    out.push_str(r#"{"$type":"point","crs":"#);
    write_str(out, p.crs().name());
    let _ = write!(out, r#","srid":{}"#, p.crs().srid());
    for (name, c) in ["x", "y", "z"].iter().zip(p.coords()) {
        let _ = write!(out, r#","{name}":"#);
        write_float(out, *c);
    }
    out.push('}');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_kinds() {
        let v = parse(r#" {"a": [1, -2, 3.5, 1e3, true, false, null], "s": "x\"\n\u00e9\ud83d\ude00", "o": {}} "#)
            .unwrap();
        let Json::Object(m) = v else { panic!() };
        assert_eq!(
            m["a"],
            Json::Array(vec![
                Json::Int(1),
                Json::Int(-2),
                Json::Float(3.5),
                Json::Float(1000.0),
                Json::Bool(true),
                Json::Bool(false),
                Json::Null
            ])
        );
        assert_eq!(m["s"], Json::String("x\"\né😀".into()));
        assert_eq!(m["o"], Json::Object(BTreeMap::new()));
    }

    #[test]
    fn big_integers_become_floats() {
        assert_eq!(parse("9223372036854775807").unwrap(), Json::Int(i64::MAX));
        assert_eq!(
            parse("9223372036854775808").unwrap(),
            Json::Float(9223372036854775808.0)
        );
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "", "{", "[1,]", "{\"a\" 1}", "01", "1.", "\"abc", "nul", "[1] x", "\"\u{1}\"",
            "\"\\ud83d\"", "-", "{1: 2}",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let deep = "[".repeat(1000) + &"]".repeat(1000);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn writes_strings_and_floats() {
        let mut s = String::new();
        write_str(&mut s, "a\"b\\c\n\u{1}é");
        assert_eq!(s, r#""a\"b\\c\n\u0001é""#);
        let mut s = String::new();
        for f in [1.0, -0.5, 1e300, f64::NAN, f64::NEG_INFINITY] {
            write_float(&mut s, f);
            s.push(' ');
        }
        assert_eq!(s, r#"1.0 -0.5 1e300 "NaN" "-Infinity" "#);
        // Round trip.
        assert_eq!(parse("1e300").unwrap(), Json::Float(1e300));
    }
}
