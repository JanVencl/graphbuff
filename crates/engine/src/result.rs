//! Query results handed back to callers.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use graphbuff_core::{GraphStore, NodeId, Point, RelId, Result, Temporal, Value, format_float, write_quoted};

/// A node with its labels and properties, captured when the query finished.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeData {
    pub id: NodeId,
    pub labels: Vec<String>,
    pub properties: BTreeMap<String, Value>,
}

/// A relationship with its type, endpoints and properties.
#[derive(Debug, Clone, PartialEq)]
pub struct RelData {
    pub id: RelId,
    pub rel_type: String,
    pub start: NodeId,
    pub end: NodeId,
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathData {
    pub nodes: Vec<NodeData>,
    pub relationships: Vec<RelData>,
}

/// A value in a query result. Unlike [`Value`], graph entities are fully materialized.
#[derive(Debug, Clone, PartialEq)]
pub enum ResultValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<ResultValue>),
    Map(BTreeMap<String, ResultValue>),
    Node(NodeData),
    Relationship(RelData),
    Path(PathData),
    /// A date, time, date-time or duration; shown in its ISO 8601 form.
    Temporal(Temporal),
    /// A point; shown as `point({x: …, y: …, crs: …})`.
    Point(Point),
}

impl ResultValue {
    pub(crate) fn materialize(v: Value, store: &dyn GraphStore) -> Result<ResultValue> {
        Ok(match v {
            Value::Null => ResultValue::Null,
            Value::Bool(b) => ResultValue::Bool(b),
            Value::Int(i) => ResultValue::Int(i),
            Value::Float(f) => ResultValue::Float(f),
            Value::String(s) => ResultValue::String(s),
            Value::List(items) => ResultValue::List(
                items
                    .into_iter()
                    .map(|v| Self::materialize(v, store))
                    .collect::<Result<_>>()?,
            ),
            Value::Map(m) => ResultValue::Map(
                m.into_iter()
                    .map(|(k, v)| Ok((k, Self::materialize(v, store)?)))
                    .collect::<Result<_>>()?,
            ),
            Value::Node(id) => ResultValue::Node(node_data(id, store)?),
            Value::Rel(id) => ResultValue::Relationship(rel_data(id, store)?),
            Value::Temporal(t) => ResultValue::Temporal(t),
            Value::Point(p) => ResultValue::Point(p),
            Value::Path(p) => ResultValue::Path(PathData {
                nodes: p
                    .nodes
                    .into_iter()
                    .map(|n| node_data(n, store))
                    .collect::<Result<_>>()?,
                relationships: p
                    .rels
                    .into_iter()
                    .map(|r| rel_data(r, store))
                    .collect::<Result<_>>()?,
            }),
        })
    }

    pub fn is_null(&self) -> bool {
        matches!(self, ResultValue::Null)
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            ResultValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            ResultValue::Float(f) => Some(*f),
            ResultValue::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            ResultValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ResultValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_temporal(&self) -> Option<&Temporal> {
        match self {
            ResultValue::Temporal(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_point(&self) -> Option<&Point> {
        match self {
            ResultValue::Point(p) => Some(p),
            _ => None,
        }
    }

    pub fn as_node(&self) -> Option<&NodeData> {
        match self {
            ResultValue::Node(n) => Some(n),
            _ => None,
        }
    }

    pub fn as_relationship(&self) -> Option<&RelData> {
        match self {
            ResultValue::Relationship(r) => Some(r),
            _ => None,
        }
    }
}

fn node_data(id: NodeId, store: &dyn GraphStore) -> Result<NodeData> {
    Ok(NodeData {
        id,
        labels: store.node_labels(id)?,
        properties: store.node_properties(id)?,
    })
}

fn rel_data(id: RelId, store: &dyn GraphStore) -> Result<RelData> {
    let (start, end) = store
        .relationship_endpoints(id)?
        .unwrap_or((NodeId(u64::MAX), NodeId(u64::MAX)));
    Ok(RelData {
        id,
        rel_type: store.relationship_type(id)?.unwrap_or_default(),
        start,
        end,
        properties: store.relationship_properties(id)?,
    })
}

fn write_props(f: &mut fmt::Formatter<'_>, props: &BTreeMap<String, Value>) -> fmt::Result {
    if props.is_empty() {
        return Ok(());
    }
    write!(f, " {{")?;
    for (i, (k, v)) in props.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{k}: {v}")?;
    }
    write!(f, "}}")
}

impl fmt::Display for NodeData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(#{}", self.id)?;
        for l in &self.labels {
            write!(f, ":{l}")?;
        }
        write_props(f, &self.properties)?;
        write!(f, ")")
    }
}

impl fmt::Display for RelData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[#{}:{}", self.id, self.rel_type)?;
        write_props(f, &self.properties)?;
        write!(f, "]")
    }
}

impl fmt::Display for PathData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, n) in self.nodes.iter().enumerate() {
            if i > 0 {
                let r = &self.relationships[i - 1];
                if r.start == n.id && r.end != n.id {
                    write!(f, "<-{r}-")?;
                } else {
                    write!(f, "-{r}->")?;
                }
            }
            write!(f, "{n}")?;
        }
        Ok(())
    }
}

impl fmt::Display for ResultValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResultValue::Null => write!(f, "null"),
            ResultValue::Bool(b) => write!(f, "{b}"),
            ResultValue::Int(i) => write!(f, "{i}"),
            ResultValue::Float(x) => write!(f, "{}", format_float(*x)),
            ResultValue::String(s) => write_quoted(f, s),
            ResultValue::List(items) => {
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            ResultValue::Map(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
            ResultValue::Node(n) => write!(f, "{n}"),
            ResultValue::Relationship(r) => write!(f, "{r}"),
            ResultValue::Path(p) => write!(f, "{p}"),
            ResultValue::Temporal(t) => write!(f, "{t}"),
            ResultValue::Point(p) => write!(f, "{p}"),
        }
    }
}

/// Counters describing what a query changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub nodes_created: usize,
    pub nodes_deleted: usize,
    pub relationships_created: usize,
    pub relationships_deleted: usize,
    pub properties_set: usize,
    pub labels_added: usize,
    pub labels_removed: usize,
    pub indexes_added: usize,
    pub indexes_removed: usize,
    pub constraints_added: usize,
    pub constraints_removed: usize,
    /// Databases created, replaced or dropped.
    pub system_updates: usize,
}

impl QueryStats {
    pub fn contains_updates(&self) -> bool {
        *self != QueryStats::default()
    }
}

impl fmt::Display for QueryStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = [
            (self.nodes_created, "nodes created"),
            (self.nodes_deleted, "nodes deleted"),
            (self.relationships_created, "relationships created"),
            (self.relationships_deleted, "relationships deleted"),
            (self.properties_set, "properties set"),
            (self.labels_added, "labels added"),
            (self.labels_removed, "labels removed"),
            (self.indexes_added, "indexes added"),
            (self.indexes_removed, "indexes removed"),
            (self.constraints_added, "constraints added"),
            (self.constraints_removed, "constraints removed"),
            (self.system_updates, "system updates"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
        if parts.is_empty() {
            write!(f, "no changes")
        } else {
            write!(f, "{}", parts.join(", "))
        }
    }
}

/// The outcome of a query: column names, rows, and update statistics.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<ResultValue>>,
    pub stats: QueryStats,
    /// The plan, for `EXPLAIN` (estimates only; the query did not run) and `PROFILE`
    /// (with the rows and time each operator actually took).
    pub plan: Option<PlanDescription>,
}

/// An execution plan as an operator tree, flattened root first: each line's inputs are
/// the lines right below it at `depth + 1`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanDescription {
    pub lines: Vec<PlanLine>,
    /// Whether the query ran (`PROFILE`), so that `rows` and `time` are measured.
    pub profiled: bool,
}

/// One operator of a [`PlanDescription`].
#[derive(Debug, Clone, PartialEq)]
pub struct PlanLine {
    pub depth: usize,
    /// The operator and its arguments, such as `Expand(All) (a)-[:KNOWS]->(b)`.
    pub operator: String,
    /// The planner's estimate of the rows this match step yields per input row.
    pub estimated_rows: Option<f64>,
    /// `PROFILE`: rows the operator produced.
    pub rows: Option<u64>,
    /// `PROFILE`: time spent in the operator itself, without its inputs.
    pub time: Option<Duration>,
}

impl fmt::Display for PlanDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for l in &self.lines {
            write!(f, "{}{}", "  ".repeat(l.depth), l.operator)?;
            if let Some(e) = l.estimated_rows {
                write!(f, " (~{} rows)", estimate_text(e))?;
            }
            if self.profiled {
                match (l.rows, l.time) {
                    (Some(r), Some(t)) => write!(f, " [{r} row{}, {:.3} ms]", plural(r), t.as_secs_f64() * 1e3)?,
                    (Some(r), None) => write!(f, " [{r} row{}]", plural(r))?,
                    _ => {}
                }
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

/// An estimated row count: whole numbers from 10 up, one decimal below.
pub(crate) fn estimate_text(rows: f64) -> String {
    if rows >= 10.0 {
        format!("{rows:.0}")
    } else {
        format!("{rows:.1}")
    }
}

/// Receives a query's result as it is produced (see [`Database::execute_streaming`]).
///
/// [`Database::execute_streaming`]: crate::Database::execute_streaming
pub trait RowSink {
    /// The column names, once, before any row.
    fn columns(&mut self, columns: &[String]) -> Result<()>;
    /// The next row. An error stops the query, which then fails with it.
    fn row(&mut self, row: Vec<ResultValue>) -> Result<()>;
}

impl QueryResult {
    /// Hands the columns and rows to `sink`, returning the result without its rows.
    pub fn feed(mut self, sink: &mut dyn RowSink) -> Result<QueryResult> {
        sink.columns(&self.columns)?;
        for row in std::mem::take(&mut self.rows) {
            sink.row(row)?;
        }
        Ok(self)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Index of a column by name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }

    /// The value at `row` in column `column`.
    pub fn get(&self, row: usize, column: &str) -> Option<&ResultValue> {
        self.rows.get(row)?.get(self.column_index(column)?)
    }

    /// All values of one column.
    pub fn column(&self, name: &str) -> Vec<&ResultValue> {
        match self.column_index(name) {
            Some(i) => self.rows.iter().map(|r| &r[i]).collect(),
            None => Vec::new(),
        }
    }

    /// Renders the result as an ASCII table.
    pub fn to_table(&self) -> String {
        if self.columns.is_empty() {
            return String::new();
        }
        let cells: Vec<Vec<String>> = self
            .rows
            .iter()
            .map(|r| r.iter().map(|v| v.to_string()).collect())
            .collect();
        let mut widths: Vec<usize> = self.columns.iter().map(|c| c.chars().count()).collect();
        for row in &cells {
            for (i, c) in row.iter().enumerate() {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
        let sep = format!(
            "+{}+",
            widths
                .iter()
                .map(|w| "-".repeat(w + 2))
                .collect::<Vec<_>>()
                .join("+")
        );
        let line = |vals: &[String]| {
            let inner: Vec<String> = vals
                .iter()
                .zip(&widths)
                .map(|(v, w)| format!(" {v}{} ", " ".repeat(w - v.chars().count())))
                .collect();
            format!("|{}|", inner.join("|"))
        };
        let mut out = vec![sep.clone(), line(&self.columns), sep.clone()];
        for row in &cells {
            out.push(line(row));
        }
        out.push(sep);
        out.join("\n")
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}
