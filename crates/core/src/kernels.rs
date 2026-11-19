//! Batched kernels over a [`Csr`] export.
//!
//! The kernels work on dense node indexes (`u32`, as in [`Csr`]) and flat arrays, the form
//! a GPU backend takes. [`Kernels`] is the interface a backend implements; [`CpuKernels`]
//! is the reference implementation (and the fallback when no GPU is present), splitting
//! independent work over threads.
//!
//! * [`Kernels::expand`]: a batch of rows (one source node each) to the rows of their
//!   relationships, in the order [`Csr::expand`] gives.
//! * [`Kernels::propagate`]: one hop of walk counts (how many walks end at each node), the
//!   whole-graph form of a multi-hop `count(*)`. Walks may use a relationship twice;
//!   Cypher paths may not, so the planner may use it only where the two agree.
//! * [`Kernels::bfs`]: hop distances from a set of sources, up to a depth.
//! * [`Kernels::filter`]: a comparison of a numeric property [`Column`] with a constant,
//!   with Cypher's three-valued result (a [`Mask`]); masks combine with `AND`, `OR`, `NOT`.
//! * [`Kernels::aggregate`]: `count`, `sum`, `min`, `max`, `avg` of a column over rows,
//!   equal to the engine's aggregate functions value for value (rows are taken in order).
//!   [`Kernels::aggregate_counts`] does the same over walk counts.
//! * [`Kernels::top_n`]: `ORDER BY column [DESC] LIMIT n` over rows.
//!
//! Columns hold integers and floats only: building one over a property that has other
//! values is `Unsupported`, and the caller runs the query on the row engine instead.

use std::cmp::Ordering;
use std::ops::Range;

use crate::store::VectorSimilarity;
use crate::{Csr, Direction, Error, GraphStore, RelId, Result, Value};

// ---------------------------------------------------------------------------------------
// Bit sets and three-valued masks
// ---------------------------------------------------------------------------------------

/// A fixed-size bit set, one bit per node index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bits {
    words: Vec<u64>,
    len: usize,
}

impl Bits {
    /// `len` bits, all clear.
    pub fn new(len: usize) -> Bits {
        Bits { words: vec![0; len.div_ceil(64)], len }
    }

    /// `len` bits, all set.
    pub fn ones(len: usize) -> Bits {
        Bits::from_words(vec![u64::MAX; len.div_ceil(64)], len)
    }

    fn from_words(words: Vec<u64>, len: usize) -> Bits {
        debug_assert_eq!(words.len(), len.div_ceil(64));
        let mut b = Bits { words, len };
        if !len.is_multiple_of(64)
            && let Some(w) = b.words.last_mut()
        {
            *w &= (1u64 << (len % 64)) - 1;
        }
        b
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, i: u32) -> bool {
        let i = i as usize;
        i < self.len && (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    pub fn set(&mut self, i: u32, on: bool) {
        let i = i as usize;
        assert!(i < self.len, "bit {i} out of range (length {})", self.len);
        let bit = 1u64 << (i % 64);
        if on {
            self.words[i / 64] |= bit;
        } else {
            self.words[i / 64] &= !bit;
        }
    }

    /// The number of set bits.
    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// The set bits, ascending.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(k, &w)| {
            let mut w = w;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let b = w.trailing_zeros();
                w &= w - 1;
                Some(k as u32 * 64 + b)
            })
        })
    }

    /// The underlying words (bit `i` is bit `i % 64` of word `i / 64`).
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    fn zip(&self, other: &Bits, f: impl Fn(u64, u64) -> u64) -> Bits {
        assert_eq!(self.len, other.len, "bit sets of different lengths");
        Bits::from_words(self.words.iter().zip(&other.words).map(|(&a, &b)| f(a, b)).collect(), self.len)
    }
}

/// A three-valued (`true`, `false`, `null`) result per node index, as Cypher predicates
/// give. `true` bits are a subset of the known bits; a node passes a `WHERE` only when its
/// result is `true`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mask {
    value: Bits,
    known: Bits,
}

impl Mask {
    /// Every node `true` (`Some(true)`), `false` or `null` (`None`).
    pub fn all(len: usize, v: Option<bool>) -> Mask {
        match v {
            Some(true) => Mask { value: Bits::ones(len), known: Bits::ones(len) },
            Some(false) => Mask { value: Bits::new(len), known: Bits::ones(len) },
            None => Mask { value: Bits::new(len), known: Bits::new(len) },
        }
    }

    /// A two-valued mask: set bits `true`, the rest `false` (a label test, say).
    pub fn from_bits(bits: Bits) -> Mask {
        let known = Bits::ones(bits.len);
        Mask { value: bits, known }
    }

    pub fn len(&self) -> usize {
        self.value.len
    }

    pub fn is_empty(&self) -> bool {
        self.value.len == 0
    }

    /// The result for node index `i` (`None` is `null`).
    pub fn get(&self, i: u32) -> Option<bool> {
        self.known.get(i).then(|| self.value.get(i))
    }

    /// Whether node index `i`'s result is `true`.
    pub fn passes(&self, i: u32) -> bool {
        self.value.get(i)
    }

    /// The nodes whose result is `true`.
    pub fn passing(&self) -> &Bits {
        &self.value
    }

    /// The nodes whose result is not `null`.
    pub fn known(&self) -> &Bits {
        &self.known
    }

    fn falses(&self) -> Bits {
        self.known.zip(&self.value, |k, v| k & !v)
    }

    /// Cypher `AND` (Kleene logic).
    pub fn and(&self, other: &Mask) -> Mask {
        let value = self.value.zip(&other.value, |a, b| a & b);
        let known = value.zip(&self.falses().zip(&other.falses(), |a, b| a | b), |a, b| a | b);
        Mask { value, known }
    }

    /// Cypher `OR` (Kleene logic).
    pub fn or(&self, other: &Mask) -> Mask {
        let value = self.value.zip(&other.value, |a, b| a | b);
        let known = value.zip(&self.falses().zip(&other.falses(), |a, b| a & b), |a, b| a | b);
        Mask { value, known }
    }

    /// Cypher `NOT`: `null` stays `null`.
    pub fn not(&self) -> Mask {
        Mask { value: self.falses(), known: self.known.clone() }
    }
}

/// The nodes with `label`, as a bit set over `csr`'s node indexes.
pub fn label_mask<S: GraphStore + ?Sized>(store: &S, csr: &Csr, label: &str) -> Result<Bits> {
    let mut bits = Bits::new(csr.node_count());
    for n in store.nodes_with_label(label) {
        if let Some(i) = csr.node_index(n?) {
            bits.set(i, true);
        }
    }
    Ok(bits)
}

// ---------------------------------------------------------------------------------------
// Numeric property columns
// ---------------------------------------------------------------------------------------

/// A number in a [`Column`]: Cypher keeps integers and floats apart.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }

    pub fn to_value(self) -> Value {
        match self {
            Num::Int(i) => Value::Int(i),
            Num::Float(f) => Value::Float(f),
        }
    }

    pub fn from_value(v: &Value) -> Option<Num> {
        match *v {
            Value::Int(i) => Some(Num::Int(i)),
            Value::Float(f) => Some(Num::Float(f)),
            _ => None,
        }
    }

    /// Cypher `=` on two numbers.
    fn cypher_eq(self, o: Num) -> bool {
        match (self, o) {
            (Num::Int(a), Num::Int(b)) => a == b,
            _ => self.as_f64() == o.as_f64(),
        }
    }

    /// Cypher `<` and friends: `None` (null) when a NaN is involved.
    fn cypher_cmp(self, o: Num) -> Option<Ordering> {
        match (self, o) {
            (Num::Int(a), Num::Int(b)) => Some(a.cmp(&b)),
            _ => self.as_f64().partial_cmp(&o.as_f64()),
        }
    }

    /// The `ORDER BY` order: numerically, NaN above every number.
    fn order(self, o: Num) -> Ordering {
        match (self, o) {
            (Num::Int(a), Num::Int(b)) => a.cmp(&b),
            _ => {
                let (x, y) = (self.as_f64(), o.as_f64());
                x.partial_cmp(&y).unwrap_or_else(|| x.is_nan().cmp(&y.is_nan()))
            }
        }
    }
}

/// `ORDER BY` on optional numbers: nulls after every number.
fn order_opt(a: Option<Num>, b: Option<Num>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.order(b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// One numeric property of every node, by node index.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    present: Bits,
    data: ColumnData,
}

#[derive(Clone, Debug, PartialEq)]
enum ColumnData {
    Int(Vec<i64>),
    Float(Vec<f64>),
    /// Both kinds: `floats[i]` where `is_float` is set, `ints[i]` otherwise.
    Mixed { ints: Vec<i64>, floats: Vec<f64>, is_float: Bits },
}

impl Column {
    /// Reads property `key` of every node of `csr` from `store` (the state `csr` was built
    /// from). Values that are neither integers nor floats are `Unsupported`.
    pub fn build<S: GraphStore + ?Sized>(store: &S, csr: &Csr, key: &str) -> Result<Column> {
        let mut values = Vec::with_capacity(csr.node_count());
        for &id in csr.nodes() {
            values.push(match store.node_property(id, key)? {
                None | Some(Value::Null) => None,
                Some(v) => Some(Num::from_value(&v).ok_or_else(|| {
                    Error::Unsupported(format!(
                        "a property column holds numbers, but `{key}` of node {} is {}",
                        id.0,
                        v.type_name()
                    ))
                })?),
            });
        }
        Ok(Column::from_values(values))
    }

    /// A column from values by node index (`None` = no value).
    pub fn from_values(values: Vec<Option<Num>>) -> Column {
        let n = values.len();
        let mut present = Bits::new(n);
        let (mut ints, mut floats) = (false, false);
        for (i, v) in values.iter().enumerate() {
            match v {
                Some(Num::Int(_)) => ints = true,
                Some(Num::Float(_)) => floats = true,
                None => continue,
            }
            present.set(i as u32, true);
        }
        let data = match (ints, floats) {
            (_, false) => ColumnData::Int(
                values.iter().map(|v| if let Some(Num::Int(x)) = v { *x } else { 0 }).collect(),
            ),
            (false, true) => ColumnData::Float(
                values.iter().map(|v| if let Some(Num::Float(x)) = v { *x } else { 0.0 }).collect(),
            ),
            (true, true) => {
                let mut is_float = Bits::new(n);
                let mut iv = vec![0i64; n];
                let mut fv = vec![0f64; n];
                for (i, v) in values.iter().enumerate() {
                    match *v {
                        Some(Num::Int(x)) => iv[i] = x,
                        Some(Num::Float(x)) => {
                            fv[i] = x;
                            is_float.set(i as u32, true);
                        }
                        None => {}
                    }
                }
                ColumnData::Mixed { ints: iv, floats: fv, is_float }
            }
        };
        Column { present, data }
    }

    /// Nodes in the column.
    pub fn len(&self) -> usize {
        self.present.len
    }

    pub fn is_empty(&self) -> bool {
        self.present.len == 0
    }

    /// The value of node index `i`, or `None` if it has none.
    #[inline]
    pub fn get(&self, i: u32) -> Option<Num> {
        if !self.present.get(i) {
            return None;
        }
        let k = i as usize;
        Some(match &self.data {
            ColumnData::Int(v) => Num::Int(v[k]),
            ColumnData::Float(v) => Num::Float(v[k]),
            ColumnData::Mixed { ints, floats, is_float } => {
                if is_float.get(i) {
                    Num::Float(floats[k])
                } else {
                    Num::Int(ints[k])
                }
            }
        })
    }

    /// The nodes that have a value.
    pub fn present(&self) -> &Bits {
        &self.present
    }

    /// For a column of integers only (or no values): the largest absolute value, else
    /// `None`. Sums of fewer than `2^53 / bound` such values are exact in any order, as
    /// integers and as floats.
    pub fn int_bound(&self) -> Option<u64> {
        match &self.data {
            ColumnData::Int(v) => Some(self.present.iter().map(|i| v[i as usize].unsigned_abs()).max().unwrap_or(0)),
            ColumnData::Float(_) if self.present.count_ones() == 0 => Some(0),
            _ => None,
        }
    }

    /// Approximate heap memory, in bytes.
    pub fn memory_bytes(&self) -> usize {
        let n = self.present.len;
        n / 8
            + match &self.data {
                ColumnData::Int(_) | ColumnData::Float(_) => n * 8,
                ColumnData::Mixed { .. } => n * 16 + n / 8,
            }
    }
}

/// A comparison of a column with a constant: `column op constant`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    /// The operator with its operands swapped: `c op x` is `x op.flip() c`.
    pub fn flip(self) -> CmpOp {
        match self {
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::Le => CmpOp::Ge,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::Ge => CmpOp::Le,
            o => o,
        }
    }

    /// Cypher's result of `a op b`, for a number `a`.
    fn eval(self, a: Num, b: &Value) -> Option<bool> {
        let Some(b) = Num::from_value(b) else {
            // A number against a non-number: `=` is false, `<>` true, the rest null.
            return match self {
                CmpOp::Eq => Some(false),
                CmpOp::Ne => Some(true),
                _ => None,
            };
        };
        match self {
            CmpOp::Eq => Some(a.cypher_eq(b)),
            CmpOp::Ne => Some(!a.cypher_eq(b)),
            CmpOp::Lt => a.cypher_cmp(b).map(Ordering::is_lt),
            CmpOp::Le => a.cypher_cmp(b).map(Ordering::is_le),
            CmpOp::Gt => a.cypher_cmp(b).map(Ordering::is_gt),
            CmpOp::Ge => a.cypher_cmp(b).map(Ordering::is_ge),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Kernel results
// ---------------------------------------------------------------------------------------

/// The rows an [`expand`](Kernels::expand) produced: per output row, the input row it came
/// from, the node index on the other end and the relationship.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExpandBatch {
    pub rows: Vec<u32>,
    pub targets: Vec<u32>,
    pub rels: Vec<RelId>,
}

impl ExpandBatch {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn append(&mut self, mut other: ExpandBatch) {
        self.rows.append(&mut other.rows);
        self.targets.append(&mut other.targets);
        self.rels.append(&mut other.rels);
    }
}

/// `count(*)`, `count(x)`, `sum(x)`, `avg(x)`, `min(x)` and `max(x)` of a column over a
/// set of rows, as the engine's aggregate functions return them.
#[derive(Clone, Debug, PartialEq)]
pub struct Aggregates {
    /// `count(*)`: rows.
    pub rows: i64,
    /// `count(x)`: rows with a value.
    pub count: i64,
    /// `sum(x)`: an integer while every value is one and the sum fits, else a float.
    pub sum: Value,
    /// `avg(x)`: a float, `null` without values.
    pub avg: Value,
    /// `min(x)`: `null` without values.
    pub min: Value,
    /// `max(x)`: `null` without values.
    pub max: Value,
}

/// The engine's `count`/`sum`/`avg`/`min`/`max` accumulators, for numbers.
struct Acc {
    rows: i64,
    count: i64,
    int: i64,
    float: f64,
    is_float: bool,
    avg: f64,
    min: Option<Num>,
    max: Option<Num>,
}

impl Acc {
    fn new() -> Acc {
        Acc { rows: 0, count: 0, int: 0, float: 0.0, is_float: false, avg: 0.0, min: None, max: None }
    }

    #[inline]
    fn add(&mut self, v: Option<Num>) {
        self.rows += 1;
        let Some(v) = v else { return };
        self.count += 1;
        match v {
            Num::Int(i) => match self.int.checked_add(i) {
                Some(s) => self.int = s,
                None => {
                    self.float += i as f64;
                    self.is_float = true;
                }
            },
            Num::Float(f) => {
                self.float += f;
                self.is_float = true;
            }
        }
        self.avg += v.as_f64();
        self.minmax(v);
    }

    fn minmax(&mut self, v: Num) {
        if self.min.is_none_or(|m| v.order(m) == Ordering::Less) {
            self.min = Some(v);
        }
        if self.max.is_none_or(|m| v.order(m) == Ordering::Greater) {
            self.max = Some(v);
        }
    }

    fn finish(self) -> Aggregates {
        Aggregates {
            rows: self.rows,
            count: self.count,
            sum: if self.is_float { Value::Float(self.int as f64 + self.float) } else { Value::Int(self.int) },
            avg: if self.count == 0 { Value::Null } else { Value::Float(self.avg / self.count as f64) },
            min: self.min.map_or(Value::Null, Num::to_value),
            max: self.max.map_or(Value::Null, Num::to_value),
        }
    }
}

fn overflow(what: &str) -> Error {
    Error::Unsupported(format!("{what} overflows a 64-bit integer; aggregate the rows instead"))
}

// ---------------------------------------------------------------------------------------
// The kernel interface
// ---------------------------------------------------------------------------------------

/// The vectors of a vector index as rows of `f32`, for [`Kernels::vector_scores`]: unit
/// length for cosine, as stored for euclidean. Each value has its own [`id`](Self::id), so
/// that a backend can keep a copy (on a device) while it is in use; a [`patch`](Self::patch)
/// makes a new value that remembers which rows differ from the values before it, so that
/// such a copy can be brought up to date by writing only those.
#[derive(Clone, Debug)]
pub struct VectorRows {
    id: u64,
    dims: usize,
    data: Vec<f32>,
    /// At least the largest row norm.
    max_norm: f64,
    /// Earlier values this one was patched from, newest first, each with the rows (sorted)
    /// that differ from it.
    history: Vec<(u64, Vec<u32>)>,
}

/// How many earlier values a [`VectorRows`] remembers.
const VECTOR_HISTORY: usize = 4;

fn next_rows_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn row_norm(row: &[f32]) -> f64 {
    row.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt()
}

impl VectorRows {
    /// Rows of `dims` values each (`data.len()` a multiple of `dims`, `dims` > 0).
    pub fn new(dims: usize, data: Vec<f32>) -> VectorRows {
        assert!(dims > 0 && data.len().is_multiple_of(dims), "rows of {dims} values");
        let max_norm = data.chunks_exact(dims).map(row_norm).fold(0.0, f64::max);
        VectorRows { id: next_rows_id(), dims, data, max_norm, history: Vec::new() }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Sets the rows at `at` (sorted, distinct) to the rows of `data`, in order, making
    /// this a new value with a new [`id`](Self::id). The number of rows stays the same.
    pub fn patch(&mut self, at: &[u32], data: &[f32]) {
        let d = self.dims;
        assert_eq!(at.len() * d, data.len(), "rows of {d} values");
        assert!(at.windows(2).all(|w| w[0] < w[1]), "sorted rows");
        if at.is_empty() {
            return;
        }
        for (&i, row) in at.iter().zip(data.chunks_exact(d)) {
            let i = i as usize;
            self.data[i * d..(i + 1) * d].copy_from_slice(row);
            self.max_norm = self.max_norm.max(row_norm(row));
        }
        let half = self.len() / 2;
        let mut history = vec![(self.id, at.to_vec())];
        for (id, rows) in self.history.drain(..) {
            let mut merged = Vec::with_capacity(rows.len() + at.len());
            let (mut a, mut b) = (rows.iter().peekable(), at.iter().peekable());
            loop {
                let next = match (a.peek(), b.peek()) {
                    (Some(x), Some(y)) if x < y => a.next(),
                    (Some(x), Some(y)) if x > y => b.next(),
                    (Some(_), Some(_)) => {
                        b.next();
                        a.next()
                    }
                    (Some(_), None) => a.next(),
                    (None, Some(_)) => b.next(),
                    (None, None) => break,
                };
                merged.extend(next.copied());
            }
            // Past half the rows, a fresh copy costs about as much as the writes.
            if merged.len() > half || history.len() == VECTOR_HISTORY {
                break;
            }
            history.push((id, merged));
        }
        self.history = history;
        self.id = next_rows_id();
    }

    /// The earlier values this one was [patched](Self::patch) from, newest first, each
    /// with the rows (sorted) that differ from it; only the last few, while those rows
    /// are few.
    pub fn changed_since(&self) -> impl Iterator<Item = (u64, &[u32])> {
        self.history.iter().map(|(id, rows)| (*id, rows.as_slice()))
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    /// The number of rows.
    pub fn len(&self) -> usize {
        self.data.len() / self.dims
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn data(&self) -> &[f32] {
        &self.data
    }

    /// The largest row norm, or more (after a patch replaced the largest).
    pub fn max_norm(&self) -> f64 {
        self.max_norm
    }
}

/// What [`Kernels::vector_scores`] computes per row: for cosine the dot product with the
/// (unit) query, for euclidean the squared distance.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorScores {
    pub values: Vec<f64>,
    /// How many terms each value summed in `f32` (0: the sums are `f64`), which bounds its
    /// rounding error: see [`VectorScores::error_bound`].
    pub f32_terms: u32,
}

impl VectorScores {
    /// A bound on how far each value may be from the same computation in exact arithmetic
    /// on the original (unrounded) vectors, for rows of norm at most `max_norm` and a
    /// query of norm `query_norm` (1 for cosine). With unit roundoff u = 2^-24 and
    /// `t` terms summed in f32: rounding rows and the query to f32 moves a dot product by
    /// at most 2u·|r||q|, and summing adds t·u·Σ|rᵢqᵢ| (to first order); a squared
    /// distance moves by at most (t + 5)·u·(|r| + |q|)². The bound returned is four times
    /// that, for the higher-order terms and some margin.
    pub fn error_bound(&self, similarity: VectorSimilarity, max_norm: f64, query_norm: f64) -> f64 {
        let u = 2f64.powi(-24);
        let t = f64::from(self.f32_terms);
        match similarity {
            VectorSimilarity::Cosine => 4.0 * (t + 2.0) * u,
            VectorSimilarity::Euclidean => 4.0 * (t + 5.0) * u * (max_norm + query_norm).powi(2),
        }
    }
}

/// [`Kernels::vector_scores`] on the CPU over the rows in `range`, with `f64` sums.
fn vector_scores_cpu(rows: &VectorRows, query: &[f64], similarity: VectorSimilarity, range: Range<usize>) -> Vec<f64> {
    let d = rows.dims;
    rows.data[range.start * d..range.end * d]
        .chunks_exact(d)
        .map(|row| match similarity {
            VectorSimilarity::Cosine => row.iter().zip(query).map(|(r, q)| f64::from(*r) * q).sum(),
            VectorSimilarity::Euclidean => row.iter().zip(query).map(|(r, q)| (f64::from(*r) - q).powi(2)).sum(),
        })
        .collect()
}

/// One hop of a [`Kernels::walk`]: relationships of `types` in `dir`, then only the nodes
/// set in `mask` keep their counts (`None`: all do).
#[derive(Clone, Copy, Debug)]
pub struct Hop<'a> {
    pub dir: Direction,
    pub types: &'a [u32],
    pub mask: Option<&'a Bits>,
}

/// The kernels an execution backend provides. Node indexes and type ids are those of the
/// [`Csr`] passed in (type ids from [`Csr::type_ids`]; an empty list matches nothing),
/// and columns and masks are by the same node indexes.
pub trait Kernels: Send + Sync {
    /// A short name for plans (`"cpu"`, `"wgpu"`, …).
    fn name(&self) -> &str;

    /// Expands every source (one per input row) over the relationships of `types` in
    /// `dir`. Output rows are grouped by input row, in input order; within a row they come
    /// in [`Csr::expand`] order, with self-loops once under `Direction::Both`.
    fn expand(&self, csr: &Csr, sources: &[u32], dir: Direction, types: &[u32]) -> ExpandBatch;

    /// One hop of walk counts: `result[j]` is the sum of `counts[i]` over the
    /// relationships a hop in `dir` takes from `i` to `j` (self-loops once under
    /// `Direction::Both`). `Unsupported` if a count overflows `u64`.
    fn propagate(&self, csr: &Csr, counts: &[u64], dir: Direction, types: &[u32]) -> Result<Vec<u64>>;

    /// Walk counts over several hops: [`propagate`](Self::propagate) for each hop in
    /// turn, then the counts of nodes outside the hop's mask set to zero. A backend may
    /// keep the counts where it computes them between hops; the result is the same.
    fn walk(&self, csr: &Csr, counts: &[u64], hops: &[Hop<'_>]) -> Result<Vec<u64>> {
        let mut counts = counts.to_vec();
        for hop in hops {
            counts = self.propagate(csr, &counts, hop.dir, hop.types)?;
            if let Some(m) = hop.mask {
                for (x, c) in counts.iter_mut().enumerate() {
                    if !m.get(x as u32) {
                        *c = 0;
                    }
                }
            }
        }
        Ok(counts)
    }

    /// Hop distances from `sources` (distance 0) up to `max_depth`; `u32::MAX` for nodes
    /// not reached.
    fn bfs(&self, csr: &Csr, sources: &[u32], dir: Direction, types: &[u32], max_depth: u32) -> Vec<u32>;

    /// `column op value` for every node, three-valued as in Cypher: a node without a value,
    /// or a `null` constant, gives `null`.
    fn filter(&self, column: &Column, op: CmpOp, value: &Value) -> Mask;

    /// The positions in `rows` (node indexes) whose node passes `mask`, ascending.
    fn select(&self, rows: &[u32], mask: &Mask) -> Vec<u32>;

    /// Aggregates of `column` over `rows` (node indexes, repeats allowed), equal to the
    /// engine's aggregate functions fed the rows in order.
    fn aggregate(&self, column: &Column, rows: &[u32]) -> Aggregates;

    /// Aggregates of `column` over `counts[i]` rows of every node `i` (walk counts from
    /// [`propagate`](Self::propagate)). `count`, integer `sum`, `min` and `max` equal the
    /// row form; float `sum` and `avg` multiply instead of adding repeatedly, so their last
    /// bits may differ, and a tie between an integer and an equal float in `min` or `max`
    /// keeps the lower node index rather than the first row. `Unsupported` when an integer
    /// would overflow (the row form would switch to floats there).
    fn aggregate_counts(&self, column: &Column, counts: &[u64]) -> Result<Aggregates>;

    /// `ORDER BY column [DESC] LIMIT n` over `rows` (node indexes): the positions in `rows`
    /// of the first `n` rows, in order. Nulls sort last ascending and first descending;
    /// equal keys keep input order, as a stable sort does.
    fn top_n(&self, column: &Column, rows: &[u32], n: usize, descending: bool) -> Vec<usize>;

    /// PageRank over the relationships of `types`, followed in `dir` (`Both`: as
    /// undirected, self-loops once), as Neo4j GDS computes it: every score starts at
    /// `1 - damping` and becomes `(1 - damping) + damping * sum(score(u) / degree(u))` over
    /// the relationships reaching it; nodes without relationships pass nothing on.
    /// Stops after `max_iterations`, or once no score moved by more than `tolerance`.
    /// Returns the scores and the iterations run.
    fn page_rank(
        &self,
        csr: &Csr,
        types: &[u32],
        dir: Direction,
        damping: f64,
        max_iterations: u32,
        tolerance: f64,
    ) -> (Vec<f64>, u32);

    /// Weakly connected components over the relationships of `types` (direction
    /// ignored): per node, the smallest node index in its component.
    fn wcc(&self, csr: &Csr, types: &[u32]) -> Vec<u32>;

    /// Triangles through each node over the relationships of `types`, as an undirected
    /// simple graph: direction ignored, parallel relationships counted once, self-loops
    /// ignored.
    fn triangles(&self, csr: &Csr, types: &[u32]) -> Vec<u64>;

    /// Per row of `rows`: the dot product with `query` (cosine; `query` of unit length) or
    /// the squared distance to it (euclidean). Approximate within
    /// [`VectorScores::error_bound`]; callers needing exact scores re-score the rows near
    /// their cut. The default runs on the calling thread with `f64` sums.
    fn vector_scores(&self, rows: &VectorRows, query: &[f64], similarity: VectorSimilarity) -> VectorScores {
        VectorScores { values: vector_scores_cpu(rows, query, similarity, 0..rows.len()), f32_terms: 0 }
    }
}

/// The CPU implementation of [`Kernels`]: independent work (rows of an expand or select,
/// nodes of a propagate or filter) is split over threads; order-dependent work
/// (aggregates, which add in row order, and BFS levels) runs on the calling thread.
#[derive(Clone, Debug)]
pub struct CpuKernels {
    threads: usize,
}

impl Default for CpuKernels {
    fn default() -> Self {
        CpuKernels::new()
    }
}

/// Work items below which a kernel stays on the calling thread.
const PARALLEL_MIN: usize = 1 << 14;

impl CpuKernels {
    /// One thread per available core.
    pub fn new() -> CpuKernels {
        CpuKernels { threads: std::thread::available_parallelism().map_or(1, |n| n.get()) }
    }

    /// At most `threads` threads (at least one).
    pub fn with_threads(threads: usize) -> CpuKernels {
        CpuKernels { threads: threads.max(1) }
    }

    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Splits `0..n` into contiguous ranges: one below [`PARALLEL_MIN`], else up to one per
    /// thread, each at least a quarter of `PARALLEL_MIN`.
    fn ranges(&self, n: usize) -> Vec<Range<usize>> {
        let parts = if n < PARALLEL_MIN { 1 } else { self.threads.min(n / (PARALLEL_MIN / 4)).max(1) };
        let step = n.div_ceil(parts).max(1);
        (0..parts).map(|p| (p * step).min(n)..((p + 1) * step).min(n)).filter(|r| !r.is_empty()).collect()
    }

    /// Runs `f` on each range of `0..n`, in parallel when there are several; results in
    /// order.
    fn split<T: Send>(&self, n: usize, f: impl Fn(Range<usize>) -> T + Sync) -> Vec<T> {
        let ranges = self.ranges(n);
        if ranges.len() <= 1 {
            return ranges.into_iter().map(f).collect();
        }
        let f = &f;
        std::thread::scope(|s| {
            let handles: Vec<_> = ranges.into_iter().map(|r| s.spawn(move || f(r))).collect();
            handles.into_iter().map(|h| h.join().expect("kernel thread panicked")).collect()
        })
    }
}

/// Which layouts one hop in `dir` reads: (outgoing, incoming).
fn layouts(dir: Direction) -> (bool, bool) {
    (matches!(dir, Direction::Outgoing | Direction::Both), matches!(dir, Direction::Incoming | Direction::Both))
}

impl Kernels for CpuKernels {
    fn name(&self) -> &str {
        "cpu"
    }

    fn vector_scores(&self, rows: &VectorRows, query: &[f64], similarity: VectorSimilarity) -> VectorScores {
        // Split by rows, weighted by their length.
        let n = rows.len();
        let per = (PARALLEL_MIN / rows.dims.max(1)).max(1);
        let parts: Vec<Range<usize>> = (0..n.div_ceil(per)).map(|p| p * per..((p + 1) * per).min(n)).collect();
        let chunks = parts.len().div_ceil(self.threads).max(1);
        let values = if parts.len() <= 1 {
            vector_scores_cpu(rows, query, similarity, 0..n)
        } else {
            std::thread::scope(|s| {
                let handles: Vec<_> = parts
                    .chunks(chunks)
                    .map(|group| {
                        let range = group[0].start..group[group.len() - 1].end;
                        s.spawn(move || vector_scores_cpu(rows, query, similarity, range))
                    })
                    .collect();
                handles.into_iter().flat_map(|h| h.join().expect("kernel thread panicked")).collect()
            })
        };
        VectorScores { values, f32_terms: 0 }
    }

    fn expand(&self, csr: &Csr, sources: &[u32], dir: Direction, types: &[u32]) -> ExpandBatch {
        let (out, inc) = layouts(dir);
        let parts = self.split(sources.len(), |r| {
            let mut b = ExpandBatch::default();
            for row in r {
                let i = sources[row];
                for &t in types {
                    if out {
                        let a = csr.adjacency(t, Direction::Outgoing);
                        let k = a.range(i);
                        b.rows.extend(std::iter::repeat_n(row as u32, k.len()));
                        b.targets.extend_from_slice(&a.targets()[k.clone()]);
                        b.rels.extend_from_slice(&a.rels()[k]);
                    }
                    if inc {
                        let a = csr.adjacency(t, Direction::Incoming);
                        for k in a.range(i) {
                            let j = a.targets()[k];
                            if dir == Direction::Both && j == i {
                                continue; // a self-loop, already reported as outgoing
                            }
                            b.rows.push(row as u32);
                            b.targets.push(j);
                            b.rels.push(a.rels()[k]);
                        }
                    }
                }
            }
            b
        });
        let mut parts = parts.into_iter();
        let mut batch = parts.next().unwrap_or_default();
        for p in parts {
            batch.append(p);
        }
        batch
    }

    fn propagate(&self, csr: &Csr, counts: &[u64], dir: Direction, types: &[u32]) -> Result<Vec<u64>> {
        let n = csr.node_count();
        assert_eq!(counts.len(), n, "one count per node");
        // Pull: node j sums the counts of the nodes a hop in `dir` reaches it from, its
        // neighbours in the reverse direction. Every j is written by one thread.
        let (out, inc) = layouts(dir.reverse());
        let parts = self.split(n, |r| -> Option<Vec<u64>> {
            let mut res = Vec::with_capacity(r.len());
            for j in r {
                let j = j as u32;
                let mut sum = 0u64;
                for &t in types {
                    if out {
                        for &i in csr.adjacency(t, Direction::Outgoing).neighbors(j) {
                            sum = sum.checked_add(counts[i as usize])?;
                        }
                    }
                    if inc {
                        for &i in csr.adjacency(t, Direction::Incoming).neighbors(j) {
                            if dir == Direction::Both && i == j {
                                continue; // a self-loop, already counted as outgoing
                            }
                            sum = sum.checked_add(counts[i as usize])?;
                        }
                    }
                }
                res.push(sum);
            }
            Some(res)
        });
        let mut res = Vec::with_capacity(n);
        for p in parts {
            res.extend(p.ok_or_else(|| overflow("a walk count"))?);
        }
        Ok(res)
    }

    fn bfs(&self, csr: &Csr, sources: &[u32], dir: Direction, types: &[u32], max_depth: u32) -> Vec<u32> {
        let mut depth = vec![u32::MAX; csr.node_count()];
        let mut frontier = Vec::new();
        for &s in sources {
            if depth[s as usize] != 0 {
                depth[s as usize] = 0;
                frontier.push(s);
            }
        }
        let (out, inc) = layouts(dir);
        let mut d = 0;
        while !frontier.is_empty() && d < max_depth {
            d += 1;
            let mut next = Vec::new();
            for &i in &frontier {
                for &t in types {
                    let outs = if out { csr.adjacency(t, Direction::Outgoing).neighbors(i) } else { &[] };
                    let ins = if inc { csr.adjacency(t, Direction::Incoming).neighbors(i) } else { &[] };
                    for &j in outs.iter().chain(ins) {
                        if depth[j as usize] == u32::MAX {
                            depth[j as usize] = d;
                            next.push(j);
                        }
                    }
                }
            }
            frontier = next;
        }
        depth
    }

    fn filter(&self, column: &Column, op: CmpOp, value: &Value) -> Mask {
        let n = column.len();
        if value.is_null() {
            return Mask::all(n, None);
        }
        let parts = self.split(n.div_ceil(64), |r| {
            let mut vals = Vec::with_capacity(r.len());
            let mut known = Vec::with_capacity(r.len());
            for w in r {
                let (mut v, mut k) = (0u64, 0u64);
                let mut present = column.present.words[w];
                while present != 0 {
                    let b = present.trailing_zeros();
                    present &= present - 1;
                    let a = column.get((w * 64) as u32 + b).expect("present");
                    if let Some(x) = op.eval(a, value) {
                        k |= 1 << b;
                        v |= u64::from(x) << b;
                    }
                }
                vals.push(v);
                known.push(k);
            }
            (vals, known)
        });
        let (mut vals, mut known) = (Vec::new(), Vec::new());
        for (v, k) in parts {
            vals.extend(v);
            known.extend(k);
        }
        Mask { value: Bits::from_words(vals, n), known: Bits::from_words(known, n) }
    }

    fn select(&self, rows: &[u32], mask: &Mask) -> Vec<u32> {
        self.split(rows.len(), |r| r.filter(|&p| mask.passes(rows[p])).map(|p| p as u32).collect::<Vec<_>>())
            .concat()
    }

    fn aggregate(&self, column: &Column, rows: &[u32]) -> Aggregates {
        let mut acc = Acc::new();
        for &i in rows {
            acc.add(column.get(i));
        }
        acc.finish()
    }

    fn aggregate_counts(&self, column: &Column, counts: &[u64]) -> Result<Aggregates> {
        assert_eq!(counts.len(), column.len(), "one count per node");
        let mut acc = Acc::new();
        for (i, &c) in counts.iter().enumerate() {
            if c == 0 {
                continue;
            }
            let c = i64::try_from(c).map_err(|_| overflow("a row count"))?;
            acc.rows = acc.rows.checked_add(c).ok_or_else(|| overflow("count(*)"))?;
            let Some(v) = column.get(i as u32) else { continue };
            acc.count += c; // at most `rows`
            match v {
                Num::Int(x) => {
                    acc.int = x.checked_mul(c).and_then(|p| acc.int.checked_add(p)).ok_or_else(|| overflow("sum"))?;
                }
                Num::Float(f) => {
                    acc.float += f * c as f64;
                    acc.is_float = true;
                }
            }
            acc.avg += v.as_f64() * c as f64;
            acc.minmax(v);
        }
        Ok(acc.finish())
    }

    fn top_n(&self, column: &Column, rows: &[u32], n: usize, descending: bool) -> Vec<usize> {
        if n == 0 {
            return Vec::new();
        }
        let cmp = |a: &usize, b: &usize| {
            let o = order_opt(column.get(rows[*a]), column.get(rows[*b]));
            if descending { o.reverse() } else { o }.then(a.cmp(b))
        };
        let mut idx: Vec<usize> = (0..rows.len()).collect();
        if n < idx.len() {
            idx.select_nth_unstable_by(n - 1, cmp);
            idx.truncate(n);
        }
        idx.sort_unstable_by(cmp);
        idx
    }

    fn page_rank(
        &self,
        csr: &Csr,
        types: &[u32],
        dir: Direction,
        damping: f64,
        max_iterations: u32,
        tolerance: f64,
    ) -> (Vec<f64>, u32) {
        let n = csr.node_count();
        let base = 1.0 - damping;
        // Hops each node passes its score along.
        let degree: Vec<u32> = self
            .split(n, |r| {
                r.map(|i| types.iter().map(|&t| hop_degree(csr, t, dir, i as u32)).sum::<u32>())
                    .collect::<Vec<_>>()
            })
            .concat();
        let mut score = vec![base; n];
        let mut iterations = 0;
        // Pull: node j sums the shares of the nodes a hop in `dir` reaches it from, in a
        // fixed order, so results do not depend on the number of threads.
        let (out, inc) = layouts(dir.reverse());
        while iterations < max_iterations {
            iterations += 1;
            let share: Vec<f64> =
                score.iter().zip(&degree).map(|(&s, &d)| if d == 0 { 0.0 } else { s / f64::from(d) }).collect();
            let next: Vec<f64> = self
                .split(n, |r| {
                    r.map(|j| {
                        let j = j as u32;
                        let mut sum = 0.0;
                        for &t in types {
                            if out {
                                for &i in csr.adjacency(t, Direction::Outgoing).neighbors(j) {
                                    sum += share[i as usize];
                                }
                            }
                            if inc {
                                for &i in csr.adjacency(t, Direction::Incoming).neighbors(j) {
                                    if dir != Direction::Both || i != j {
                                        sum += share[i as usize];
                                    }
                                }
                            }
                        }
                        base + damping * sum
                    })
                    .collect::<Vec<_>>()
                })
                .concat();
            let moved = next.iter().zip(&score).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
            score = next;
            if moved <= tolerance {
                break;
            }
        }
        (score, iterations)
    }

    fn wcc(&self, csr: &Csr, types: &[u32]) -> Vec<u32> {
        // Union-find: every root is the smallest index of its set.
        let n = csr.node_count();
        let mut parent: Vec<u32> = (0..n as u32).collect();
        fn find(parent: &mut [u32], mut x: u32) -> u32 {
            while parent[x as usize] != x {
                let up = parent[parent[x as usize] as usize];
                parent[x as usize] = up; // path halving
                x = up;
            }
            x
        }
        for &t in types {
            let a = csr.adjacency(t, Direction::Outgoing);
            for x in 0..n as u32 {
                for &y in a.neighbors(x) {
                    let (rx, ry) = (find(&mut parent, x), find(&mut parent, y));
                    if rx != ry {
                        let (lo, hi) = if rx < ry { (rx, ry) } else { (ry, rx) };
                        parent[hi as usize] = lo;
                    }
                }
            }
        }
        (0..n as u32).map(|x| find(&mut parent, x)).collect()
    }

    fn triangles(&self, csr: &Csr, types: &[u32]) -> Vec<u64> {
        let n = csr.node_count();
        let (offsets, targets) = self.oriented_neighbours(csr, types);
        let forward = |u: usize| &targets[offsets[u]..offsets[u + 1]];
        let parts = self.split(n, |r| {
            let mut counts = vec![0u64; n];
            for u in r {
                let fu = forward(u);
                for &v in fu {
                    // Sorted intersection of the two forward lists.
                    let fv = forward(v as usize);
                    let (mut i, mut j) = (0, 0);
                    while i < fu.len() && j < fv.len() {
                        match fu[i].cmp(&fv[j]) {
                            Ordering::Less => i += 1,
                            Ordering::Greater => j += 1,
                            Ordering::Equal => {
                                counts[u] += 1;
                                counts[v as usize] += 1;
                                counts[fu[i] as usize] += 1;
                                i += 1;
                                j += 1;
                            }
                        }
                    }
                }
            }
            counts
        });
        let mut parts = parts.into_iter();
        let mut total = parts.next().unwrap_or_else(|| vec![0; n]);
        for p in parts {
            for (t, c) in total.iter_mut().zip(p) {
                *t += c;
            }
        }
        total
    }
}

impl CpuKernels {
    /// The undirected simple graph over the relationships of `types` (direction ignored,
    /// parallel relationships once, self-loops left out), with every edge kept only at its
    /// lower-ranked end (by degree, then index), in CSR form: `(offsets, targets)`, each
    /// row sorted. Every triangle is the intersection of a row with the row of one of its
    /// entries, exactly once.
    pub fn oriented_neighbours(&self, csr: &Csr, types: &[u32]) -> (Vec<usize>, Vec<u32>) {
        let n = csr.node_count();
        let neighbours: Vec<Vec<u32>> = self
            .split(n, |r| {
                r.map(|x| {
                    let x = x as u32;
                    let mut nb = Vec::new();
                    for &t in types {
                        nb.extend_from_slice(csr.adjacency(t, Direction::Outgoing).neighbors(x));
                        nb.extend_from_slice(csr.adjacency(t, Direction::Incoming).neighbors(x));
                    }
                    nb.retain(|&y| y != x);
                    nb.sort_unstable();
                    nb.dedup();
                    nb
                })
                .collect::<Vec<_>>()
            })
            .concat();
        let rank = |x: u32| (neighbours[x as usize].len(), x);
        let forward: Vec<Vec<u32>> = self
            .split(n, |r| {
                r.map(|x| neighbours[x].iter().copied().filter(|&y| rank(y) > rank(x as u32)).collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            })
            .concat();
        drop(neighbours);
        let mut offsets = Vec::with_capacity(n + 1);
        offsets.push(0);
        for f in &forward {
            offsets.push(offsets.last().unwrap() + f.len());
        }
        (offsets, forward.concat())
    }
}

/// The hops of type `t` from node `i` in `dir` (self-loops once under `Both`).
fn hop_degree(csr: &Csr, t: u32, dir: Direction, i: u32) -> u32 {
    let o = csr.adjacency(t, Direction::Outgoing).neighbors(i);
    let n = csr.adjacency(t, Direction::Incoming).degree(i) as u32;
    match dir {
        Direction::Outgoing => o.len() as u32,
        Direction::Incoming => n,
        Direction::Both => o.len() as u32 + n - o.iter().filter(|&&x| x == i).count() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemoryGraph, NodeId};
    use std::collections::BTreeMap;

    #[test]
    fn patched_vector_rows_remember_what_changed() {
        let mut rows = VectorRows::new(2, vec![0.0; 2 * 10]);
        let first = rows.id();
        rows.patch(&[0, 7], &[1.0, 0.0, 3.0, 4.0]);
        let second = rows.id();
        assert_ne!(first, second);
        assert_eq!(rows.max_norm(), 5.0);
        rows.patch(&[2, 7], &[0.0, 1.0, 0.0, 0.0]);
        rows.patch(&[], &[]);
        let third = rows.id();
        assert_eq!(&rows.data()[..6], &[1.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
        assert_eq!(&rows.data()[14..16], &[0.0, 0.0]);
        let since: Vec<(u64, Vec<u32>)> = rows.changed_since().map(|(id, at)| (id, at.to_vec())).collect();
        assert_eq!(since, [(second, vec![2, 7]), (first, vec![0, 2, 7])]);
        // Changes to more than half the rows since an earlier value are not worth writing.
        rows.patch(&[1, 3, 4, 5], &[0.0; 8]);
        let since: Vec<u64> = rows.changed_since().map(|(id, _)| id).collect();
        assert_eq!(since, [third]);
    }

    #[test]
    fn three_valued_masks() {
        // a = t t t f f f n n n, b = t f n t f n t f n.
        let a = Mask { value: Bits::from_words(vec![0b000_000_111], 9), known: Bits::from_words(vec![0b000_111_111], 9) };
        let b = Mask { value: Bits::from_words(vec![0b001_001_001], 9), known: Bits::from_words(vec![0b011_011_011], 9) };
        let get = |m: &Mask| (0..9).map(|i| m.get(i)).collect::<Vec<_>>();
        let (t, f, n) = (Some(true), Some(false), None);
        assert_eq!(get(&a.and(&b)), [t, f, n, f, f, f, n, f, n]);
        assert_eq!(get(&a.or(&b)), [t, t, t, t, f, n, t, n, n]);
        assert_eq!(get(&a.not()), [f, f, f, t, t, t, n, n, n]);
        assert_eq!(Bits::ones(70).count_ones(), 70);
        assert_eq!(Bits::ones(70).iter().last(), Some(69));
    }

    #[test]
    fn filter_compares_like_cypher() {
        let k = CpuKernels::with_threads(2);
        let col = Column::from_values(vec![
            Some(Num::Int(1)),
            Some(Num::Float(1.0)),
            Some(Num::Float(f64::NAN)),
            None,
            Some(Num::Int(i64::MAX)),
        ]);
        let get = |m: Mask| (0..5).map(|i| m.get(i)).collect::<Vec<_>>();
        let (t, f, n) = (Some(true), Some(false), None);
        assert_eq!(get(k.filter(&col, CmpOp::Eq, &Value::Int(1))), [t, t, f, n, f]);
        assert_eq!(get(k.filter(&col, CmpOp::Ne, &Value::Float(1.0))), [f, f, t, n, t]);
        assert_eq!(get(k.filter(&col, CmpOp::Gt, &Value::Int(0))), [t, t, n, n, t]);
        assert_eq!(get(k.filter(&col, CmpOp::Lt, &Value::Int(i64::MAX))), [t, t, n, n, f]);
        assert_eq!(get(k.filter(&col, CmpOp::Eq, &Value::from("1"))), [f, f, f, n, f]);
        assert_eq!(get(k.filter(&col, CmpOp::Le, &Value::from("1"))), [n, n, n, n, n]);
        assert_eq!(get(k.filter(&col, CmpOp::Eq, &Value::Null)), [n, n, n, n, n]);
    }

    #[test]
    fn aggregates_follow_the_engine() {
        let k = CpuKernels::new();
        let col = Column::from_values(vec![Some(Num::Int(i64::MAX)), Some(Num::Int(1)), None, Some(Num::Float(0.5))]);
        let a = k.aggregate(&col, &[0, 1, 2, 3, 1]);
        assert_eq!((a.rows, a.count), (5, 4));
        // The two 1s overflow the integer sum, so they are added as floats.
        assert_eq!(a.sum, Value::Float(i64::MAX as f64 + 2.5));
        assert_eq!((a.min, a.max), (Value::Float(0.5), Value::Int(i64::MAX)));
        let e = k.aggregate(&col, &[2]);
        assert_eq!((e.rows, e.count, e.sum, e.avg, e.min), (1, 0, Value::Int(0), Value::Null, Value::Null));
        assert!(k.aggregate_counts(&col, &[1, 1, 0, 0]).is_err());
        let c = k.aggregate_counts(&col, &[0, 3, 7, 2]).unwrap();
        assert_eq!((c.rows, c.count, c.sum, c.max), (12, 5, Value::Float(4.0), Value::Int(1)));
    }

    #[test]
    fn top_n_is_a_stable_sort_prefix() {
        let k = CpuKernels::new();
        let col = Column::from_values(vec![Some(Num::Int(3)), None, Some(Num::Float(f64::NAN)), Some(Num::Float(3.0))]);
        let rows = [0, 1, 2, 3, 0, 1];
        assert_eq!(k.top_n(&col, &rows, 10, false), [0, 3, 4, 2, 1, 5]);
        assert_eq!(k.top_n(&col, &rows, 3, false), [0, 3, 4]);
        assert_eq!(k.top_n(&col, &rows, 3, true), [1, 5, 2]);
        assert!(k.top_n(&col, &rows, 0, true).is_empty());
    }

    #[test]
    fn expand_propagate_bfs() {
        let mut g = MemoryGraph::new();
        let n: Vec<NodeId> = (0..4).map(|_| g.create_node(&[], BTreeMap::new()).unwrap()).collect();
        for (a, b) in [(0, 1), (0, 2), (1, 2), (2, 2), (2, 3)] {
            g.create_relationship(n[a], n[b], "T", BTreeMap::new()).unwrap();
        }
        let csr = Csr::build(&g).unwrap();
        let k = CpuKernels::new();
        let all = csr.type_ids(&[]);
        let b = k.expand(&csr, &[2, 0], Direction::Both, &all);
        let got: Vec<(u32, u32)> = b.rows.iter().zip(&b.targets).map(|(&r, &t)| (r, t)).collect();
        assert_eq!(got, [(0, 2), (0, 3), (0, 0), (0, 1), (1, 1), (1, 2)]);
        assert_eq!(k.propagate(&csr, &[1, 0, 0, 0], Direction::Outgoing, &all).unwrap(), [0, 1, 1, 0]);
        assert_eq!(k.propagate(&csr, &[0, 1, 1, 0], Direction::Outgoing, &all).unwrap(), [0, 0, 2, 1]);
        assert_eq!(k.propagate(&csr, &[0, 0, 1, 0], Direction::Both, &all).unwrap(), [1, 1, 1, 1]);
        assert_eq!(k.propagate(&csr, &[0, 0, 0, 1], Direction::Incoming, &all).unwrap(), [0, 0, 1, 0]);
        assert!(k.propagate(&csr, &[u64::MAX, u64::MAX, 0, 0], Direction::Outgoing, &all).is_err());
        assert_eq!(k.bfs(&csr, &[3], Direction::Incoming, &all, 9), [2, 2, 1, 0]);
        assert_eq!(k.bfs(&csr, &[0], Direction::Outgoing, &all, 1), [0, 1, 1, u32::MAX]);
        assert!(k.expand(&csr, &[0], Direction::Outgoing, &[]).is_empty());
    }

    #[test]
    fn algorithms() {
        // Triangles 0-1-2 (with a parallel 2 -> 0) and 1-2-3 (3 has a self-loop), a
        // separate pair 5 -> 4 and a lonely node 6.
        let mut g = MemoryGraph::new();
        let n: Vec<NodeId> = (0..7).map(|_| g.create_node(&[], BTreeMap::new()).unwrap()).collect();
        for (a, b) in [(0, 1), (1, 2), (2, 0), (2, 0), (3, 1), (2, 3), (3, 3), (5, 4)] {
            g.create_relationship(n[a], n[b], "T", BTreeMap::new()).unwrap();
        }
        let csr = Csr::build(&g).unwrap();
        let all = csr.type_ids(&[]);
        for k in [CpuKernels::with_threads(1), CpuKernels::with_threads(3)] {
            assert_eq!(k.wcc(&csr, &all), [0, 0, 0, 0, 4, 4, 6]);
            assert_eq!(k.triangles(&csr, &all), [1, 2, 2, 1, 0, 0, 0]);
            let (s, it) = k.page_rank(&csr, &all, Direction::Outgoing, 0.85, 1000, 1e-12);
            assert!(it < 1000);
            assert_eq!((s[5], s[6]), (1.0 - 0.85, 1.0 - 0.85));
            assert!((s[4] - (0.15 + 0.85 * 0.15)).abs() < 1e-12);
            let (one, it) = k.page_rank(&csr, &all, Direction::Outgoing, 0.85, 1, 0.0);
            assert_eq!(it, 1);
            // Node 0 gets 2 of node 2's 3 hops (two parallel 2 -> 0).
            assert!((one[0] - (0.15 + 0.85 * 0.15 * 2.0 / 3.0)).abs() < 1e-12);
        }
    }
}
