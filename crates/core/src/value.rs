use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::spatial::Point;
use crate::temporal::Temporal;

/// Identifier of a node. Ids are never reused within the lifetime of a store.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

/// Identifier of a relationship. Ids are never reused within the lifetime of a store.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelId(pub u64);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for RelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A path through the graph: `nodes.len() == rels.len() + 1`, and `rels[i]` connects
/// `nodes[i]` with `nodes[i + 1]` (in either direction).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Path {
    pub nodes: Vec<NodeId>,
    pub rels: Vec<RelId>,
}

/// A dynamically typed Cypher value.
///
/// Graph entities are carried by id; their labels and properties live in the store.
/// Equality and hashing here are *structural* (used for grouping and `DISTINCT`);
/// Cypher's three-valued `=` operator is implemented by the engine.
#[derive(Clone, Debug, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Node(NodeId),
    Rel(RelId),
    Path(Path),
    /// A date, time, date-time or duration.
    Temporal(Temporal),
    /// A 2D or 3D point.
    Point(Point),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Cypher name of the value's type, used in error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Bool(_) => "BOOLEAN",
            Value::Int(_) => "INTEGER",
            Value::Float(_) => "FLOAT",
            Value::String(_) => "STRING",
            Value::List(_) => "LIST",
            Value::Map(_) => "MAP",
            Value::Node(_) => "NODE",
            Value::Rel(_) => "RELATIONSHIP",
            Value::Path(_) => "PATH",
            Value::Temporal(t) => t.type_name(),
            Value::Point(_) => "POINT",
        }
    }

    /// Whether this value may be stored as a property: scalars (temporal values
    /// included), or lists of scalars. `Null` is accepted too — storing null means
    /// "remove the property".
    pub fn is_storable_property(&self) -> bool {
        match self {
            Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::String(_)
            | Value::Temporal(_)
            | Value::Point(_) => true,
            Value::List(items) => items.iter().all(|v| {
                matches!(
                    v,
                    Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::String(_)
                        | Value::Temporal(_)
                        | Value::Point(_)
                )
            }),
            _ => false,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

fn canonical_float_bits(f: f64) -> u64 {
    if f == 0.0 {
        0.0f64.to_bits()
    } else if f.is_nan() {
        f64::NAN.to_bits()
    } else {
        f.to_bits()
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        use Value::*;
        match (self, other) {
            (Null, Null) => true,
            (Bool(a), Bool(b)) => a == b,
            (Int(a), Int(b)) => a == b,
            (Float(a), Float(b)) => canonical_float_bits(*a) == canonical_float_bits(*b),
            (String(a), String(b)) => a == b,
            (List(a), List(b)) => a == b,
            (Map(a), Map(b)) => a == b,
            (Node(a), Node(b)) => a == b,
            (Rel(a), Rel(b)) => a == b,
            (Path(a), Path(b)) => a == b,
            (Temporal(a), Temporal(b)) => a == b,
            (Point(a), Point(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Value::Null => {}
            Value::Bool(b) => b.hash(state),
            Value::Int(i) => i.hash(state),
            Value::Float(f) => canonical_float_bits(*f).hash(state),
            Value::String(s) => s.hash(state),
            Value::List(l) => l.hash(state),
            Value::Map(m) => m.hash(state),
            Value::Node(n) => n.hash(state),
            Value::Rel(r) => r.hash(state),
            Value::Path(p) => p.hash(state),
            Value::Temporal(t) => t.hash(state),
            Value::Point(p) => p.hash(state),
        }
    }
}

/// Formats a float so it always reads as a float (`1.0`, not `1`).
pub fn format_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
        format!("{f:.1}")
    } else if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        if f > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else {
        format!("{f}")
    }
}

/// Writes `s` as a quoted Cypher string literal.
pub fn write_quoted(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    write!(f, "\"")?;
    for c in s.chars() {
        match c {
            '"' => write!(f, "\\\"")?,
            '\\' => write!(f, "\\\\")?,
            '\n' => write!(f, "\\n")?,
            '\t' => write!(f, "\\t")?,
            c => write!(f, "{c}")?,
        }
    }
    write!(f, "\"")
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{}", format_float(*x)),
            Value::String(s) => write_quoted(f, s),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Value::Map(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
            Value::Node(id) => write!(f, "Node({id})"),
            Value::Rel(id) => write!(f, "Rel({id})"),
            Value::Path(p) => {
                write!(f, "Path(")?;
                for (i, n) in p.nodes.iter().enumerate() {
                    if i > 0 {
                        write!(f, "-[{}]-", p.rels[i - 1])?;
                    }
                    write!(f, "({n})")?;
                }
                write!(f, ")")
            }
            Value::Temporal(t) => write!(f, "{t}"),
            Value::Point(p) => write!(f, "{p}"),
        }
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Int(v as i64)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::String(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::String(v)
    }
}
impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(v: Vec<T>) -> Self {
        Value::List(v.into_iter().map(Into::into).collect())
    }
}
impl From<NodeId> for Value {
    fn from(v: NodeId) -> Self {
        Value::Node(v)
    }
}
impl From<Temporal> for Value {
    fn from(v: Temporal) -> Self {
        Value::Temporal(v)
    }
}
impl From<Point> for Value {
    fn from(v: Point) -> Self {
        Value::Point(v)
    }
}
impl From<RelId> for Value {
    fn from(v: RelId) -> Self {
        Value::Rel(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_values() {
        assert_eq!(Value::Float(1.0).to_string(), "1.0");
        assert_eq!(Value::Float(2.5).to_string(), "2.5");
        assert_eq!(Value::from(vec![1i64, 2]).to_string(), "[1, 2]");
        assert_eq!(Value::from("a\"b").to_string(), "\"a\\\"b\"");
    }

    #[test]
    fn storable_properties() {
        assert!(Value::from(vec![1i64, 2]).is_storable_property());
        assert!(!Value::Map(BTreeMap::new()).is_storable_property());
        assert!(!Value::List(vec![Value::List(vec![])]).is_storable_property());
        assert!(!Value::Node(NodeId(0)).is_storable_property());
    }
}
