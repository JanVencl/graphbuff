//! The filter properties of a vector index in memory: per property, one value for each row
//! (or slot) of the index, so that a filter is checked without reading the store.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use graphbuff_core::{Result, Temporal, Value};

use crate::ops::cypher_cmp;

/// The code of `null`, and of a row not in use.
const NULL: u32 = 0;

/// What a filter column keeps of `value`: numbers, strings, booleans and temporal values;
/// anything else counts as `null`.
pub(crate) fn filter_value(value: Option<Value>) -> Value {
    match value {
        Some(v @ (Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::String(_) | Value::Temporal(_))) => v,
        _ => Value::Null,
    }
}

/// The values of one filter property, dictionary-encoded: a code per row, and the distinct
/// values. A value no row has any more gives its code back, so the dictionary is never
/// larger than the rows.
#[derive(Clone, Debug)]
pub(crate) struct FilterColumn {
    /// The code of each row's value; [`NULL`] for none.
    codes: Vec<u32>,
    /// The value of each code (`null` for [`NULL`] and the free ones).
    values: Vec<Value>,
    /// How many rows have each code (not counted for [`NULL`]).
    uses: Vec<u32>,
    code_of: HashMap<Value, u32>,
    /// The codes not in use.
    free: Vec<u32>,
}

impl FilterColumn {
    pub fn new() -> FilterColumn {
        FilterColumn {
            codes: Vec::new(),
            values: vec![Value::Null],
            uses: vec![0],
            code_of: HashMap::new(),
            free: Vec::new(),
        }
    }

    /// Adds a row with `value` (one [`filter_value`] gives).
    pub fn push(&mut self, value: &Value) {
        let code = self.acquire(value);
        self.codes.push(code);
    }

    /// Adds rows of `null` up to `rows` rows.
    pub fn pad(&mut self, rows: usize) {
        debug_assert!(rows >= self.codes.len());
        self.codes.resize(rows, NULL);
    }

    /// The value of `row`.
    pub fn get(&self, row: u32) -> &Value {
        &self.values[self.codes[row as usize] as usize]
    }

    /// Sets the value of `row`.
    pub fn set(&mut self, row: u32, value: &Value) {
        if self.get(row) == value {
            return;
        }
        // The old value first, so that the new one can take its code.
        let old = std::mem::replace(&mut self.codes[row as usize], NULL);
        self.release(old);
        self.codes[row as usize] = self.acquire(value);
    }

    /// The code of `value`, counting one more row that has it.
    fn acquire(&mut self, value: &Value) -> u32 {
        if value.is_null() {
            return NULL;
        }
        if let Some(&code) = self.code_of.get(value) {
            self.uses[code as usize] += 1;
            return code;
        }
        let code = match self.free.pop() {
            Some(code) => {
                self.values[code as usize] = value.clone();
                self.uses[code as usize] = 1;
                code
            }
            None => {
                self.values.push(value.clone());
                self.uses.push(1);
                (self.values.len() - 1) as u32
            }
        };
        self.code_of.insert(value.clone(), code);
        code
    }

    /// Counts one row less that has `code`.
    fn release(&mut self, code: u32) {
        if code == NULL {
            return;
        }
        let uses = &mut self.uses[code as usize];
        *uses -= 1;
        if *uses == 0 {
            let value = std::mem::take(&mut self.values[code as usize]);
            self.code_of.remove(&value);
            self.free.push(code);
        }
    }
}

// ---- predicates ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Compare {
    Lt,
    Le,
    Gt,
    Ge,
}

/// A condition on one filter property: an entity passes if Cypher's `property = value`,
/// `property IN values` or `property < value` (…) is true (not false or `null`).
#[derive(Clone, Debug)]
pub(crate) struct Predicate {
    /// Which of the index's filter properties.
    pub column: usize,
    test: Test,
}

#[derive(Clone, Debug)]
enum Test {
    OneOf(OneOf),
    Compare(Compare, Value),
}

impl Predicate {
    pub fn equal(column: usize, value: Value) -> Predicate {
        Predicate::one_of(column, vec![value])
    }

    pub fn one_of(column: usize, values: Vec<Value>) -> Predicate {
        Predicate { column, test: Test::OneOf(OneOf::new(values)) }
    }

    pub fn compare(column: usize, op: Compare, value: Value) -> Predicate {
        Predicate { column, test: Test::Compare(op, value) }
    }

    /// Whether a property with `value` (one [`filter_value`] gives) passes.
    pub fn passes(&self, value: &Value) -> bool {
        match &self.test {
            Test::OneOf(set) => set.contains(value),
            Test::Compare(op, with) => cypher_cmp(value, with).is_some_and(|o| match op {
                Compare::Lt => o == Ordering::Less,
                Compare::Le => o != Ordering::Greater,
                Compare::Gt => o == Ordering::Greater,
                Compare::Ge => o != Ordering::Less,
            }),
        }
    }
}

/// The bits of `f`, the same for both zeros.
fn float_bits(f: f64) -> u64 {
    if f == 0.0 { 0 } else { f.to_bits() }
}

/// A set of values by Cypher's `=`: an integer equals the floats its conversion to a float
/// equals, NaN equals nothing, and temporal values equal across offsets where `=` says so.
#[derive(Clone, Debug, Default)]
struct OneOf {
    ints: HashSet<i64>,
    /// The floats, and the integers as floats, by [`float_bits`].
    floats: HashSet<u64>,
    ints_as_floats: HashSet<u64>,
    temporals: Vec<Temporal>,
    /// Strings and booleans.
    others: HashSet<Value>,
}

impl OneOf {
    fn new(values: Vec<Value>) -> OneOf {
        let mut set = OneOf::default();
        for v in values {
            match v {
                Value::Int(i) => {
                    set.ints.insert(i);
                    set.ints_as_floats.insert(float_bits(i as f64));
                }
                Value::Float(f) if !f.is_nan() => {
                    set.floats.insert(float_bits(f));
                }
                Value::Temporal(t) => set.temporals.push(t),
                Value::String(_) | Value::Bool(_) => {
                    set.others.insert(v);
                }
                // Equal to no value a filter column keeps.
                _ => {}
            }
        }
        set
    }

    fn contains(&self, value: &Value) -> bool {
        match value {
            Value::Int(i) => self.ints.contains(i) || self.floats.contains(&float_bits(*i as f64)),
            Value::Float(f) if !f.is_nan() => {
                let bits = float_bits(*f);
                self.floats.contains(&bits) || self.ints_as_floats.contains(&bits)
            }
            Value::Temporal(t) => self.temporals.iter().any(|x| x.cypher_eq(t)),
            Value::String(_) | Value::Bool(_) => self.others.contains(value),
            _ => false,
        }
    }
}

/// The rows (or slots) of an index that pass a filter.
pub(crate) struct Selection {
    words: Vec<u64>,
    count: usize,
}

impl Selection {
    /// The rows of `columns` that pass all of `predicates` (at least one). Rows not in use
    /// have `null` in every column, so they never do.
    pub fn new(columns: &[FilterColumn], predicates: &[Predicate], interrupt: &dyn Fn() -> Result<()>) -> Result<Selection> {
        debug_assert!(!predicates.is_empty());
        let mut words: Option<Vec<u64>> = None;
        for p in predicates {
            let column = &columns[p.column];
            // Each distinct value is tested once.
            let mut passes = vec![false; column.values.len()];
            for (n, (pass, value)) in passes.iter_mut().zip(&column.values).enumerate() {
                if n % 4096 == 4095 {
                    interrupt()?;
                }
                *pass = !value.is_null() && p.passes(value);
            }
            interrupt()?;
            let mut mask = column.codes.chunks(64).map(|codes| {
                codes.iter().enumerate().fold(0u64, |w, (i, &c)| w | (u64::from(passes[c as usize]) << i))
            });
            match &mut words {
                None => words = Some(mask.collect()),
                Some(words) => words.iter_mut().for_each(|w| *w &= mask.next().expect("columns of one length")),
            }
        }
        let words = words.unwrap_or_default();
        let count = words.iter().map(|w| w.count_ones() as usize).sum();
        Ok(Selection { words, count })
    }

    /// The number of rows selected.
    pub fn count(&self) -> usize {
        self.count
    }

    pub fn contains(&self, row: u32) -> bool {
        self.words[row as usize / 64] >> (row % 64) & 1 == 1
    }

    /// The rows selected, in order.
    pub fn iter(&self) -> impl Iterator<Item = u32> {
        self.words.iter().enumerate().flat_map(|(i, &word)| {
            let mut w = word;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let bit = w.trailing_zeros();
                w &= w - 1;
                Some(i as u32 * 64 + bit)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::cypher_eq;

    fn s(v: &str) -> Value {
        Value::String(v.into())
    }

    #[test]
    fn values_by_row() {
        let mut c = FilterColumn::new();
        for v in [s("a"), Value::Null, s("b"), s("a")] {
            c.push(&v);
        }
        c.pad(6);
        let got = |c: &FilterColumn| (0..c.codes.len() as u32).map(|r| c.get(r).clone()).collect::<Vec<_>>();
        assert_eq!(got(&c), [s("a"), Value::Null, s("b"), s("a"), Value::Null, Value::Null]);
        assert_eq!(c.code_of.len(), 2);
        // Integers and floats are different values here: the predicates compare them.
        c.set(4, &Value::Int(1));
        c.set(5, &Value::Float(1.0));
        assert_eq!(c.code_of.len(), 4);
        c.set(0, &Value::Null);
        c.set(2, &s("a"));
        assert_eq!(got(&c), [Value::Null, Value::Null, s("a"), s("a"), Value::Int(1), Value::Float(1.0)]);
    }

    #[test]
    fn unused_values_give_their_codes_back() {
        let mut c = FilterColumn::new();
        c.push(&Value::Int(0));
        c.push(&s("kept"));
        // A property that changes all the time does not grow the dictionary.
        for i in 1..1000 {
            c.set(0, &Value::Int(i));
        }
        assert_eq!(c.values.len(), 3);
        assert_eq!(c.code_of.len(), 2);
        assert_eq!((c.get(0), c.get(1)), (&Value::Int(999), &s("kept")));
        c.set(0, &Value::Float(f64::NAN));
        c.set(0, &Value::Float(f64::NAN));
        assert_eq!(c.code_of.len(), 2);
    }

    /// Values of every kind a column keeps, with the awkward ones.
    fn samples() -> Vec<Value> {
        let db = crate::Database::in_memory();
        let temporal = [
            "date('2024-02-29')",
            "date('2024-03-01')",
            "localtime('10:00')",
            "time('10:00+01:00')",
            "time('09:00Z')",
            "localdatetime('2024-02-29T10:00')",
            "datetime('2024-02-29T10:00+01:00')",
            "datetime('2024-02-29T09:00Z')",
            "datetime('2024-02-29T10:00[Europe/Prague]')",
            "duration('P1D')",
            "duration('PT24H')",
        ];
        let mut out: Vec<Value> = temporal
            .iter()
            .map(|t| {
                let q = format!("CREATE (n:T {{v: {t}}})");
                db.execute(&q).unwrap_or_else(|e| panic!("{q}: {e}"));
                let snapshot = db.snapshot().expect("a committed state");
                let id = snapshot.nodes_with_label("T").last().unwrap().unwrap();
                snapshot.node_property(id, "v").unwrap().unwrap()
            })
            .collect();
        out.extend([Value::Bool(true), Value::Bool(false), s(""), s("a"), s("B"), s("ä")]);
        out.extend([0, 1, -1, 2, i64::MAX, i64::MIN, (1 << 53) + 1, 1 << 53].map(Value::Int));
        out.extend([0.0, -0.0, 1.0, 1.5, 9_007_199_254_740_992.0, f64::NAN, f64::INFINITY, -1e300].map(Value::Float));
        out
    }

    #[test]
    fn predicates_are_cypher_comparisons() {
        let samples = samples();
        let lists = || samples.iter().cloned().chain([Value::Null, Value::List(vec![Value::Int(1)])]);
        for with in lists() {
            let equal = Predicate::equal(0, with.clone());
            for v in samples.iter().chain([&Value::Null]) {
                assert_eq!(equal.passes(v), cypher_eq(v, &with) == Some(true), "{v:?} = {with:?}");
                for (op, yes) in [
                    (Compare::Lt, &[Ordering::Less][..]),
                    (Compare::Le, &[Ordering::Less, Ordering::Equal]),
                    (Compare::Gt, &[Ordering::Greater]),
                    (Compare::Ge, &[Ordering::Greater, Ordering::Equal]),
                ] {
                    let want = cypher_cmp(v, &with).is_some_and(|o| yes.contains(&o));
                    assert_eq!(Predicate::compare(0, op, with.clone()).passes(v), want, "{v:?} {op:?} {with:?}");
                }
            }
        }
        for skip in 0..5 {
            let list: Vec<Value> = lists().skip(skip).step_by(3).collect();
            let set = Predicate::one_of(0, list.clone());
            for v in samples.iter().chain([&Value::Null]) {
                let want = list.iter().any(|x| cypher_eq(v, x) == Some(true));
                assert_eq!(set.passes(v), want, "{v:?} IN {list:?}");
            }
        }
        assert!(!Predicate::one_of(0, vec![]).passes(&Value::Int(1)));
    }

    #[test]
    fn selections() {
        let (mut a, mut b) = (FilterColumn::new(), FilterColumn::new());
        for i in 0..1000 {
            a.push(&Value::Int(i % 7));
            b.push(&if i % 3 == 0 { Value::Null } else { Value::Int(i) });
        }
        // Rows not in use, and rows put back out of use.
        a.pad(1100);
        b.pad(1100);
        for r in [7, 14] {
            a.set(r, &Value::Null);
            b.set(r, &Value::Null);
        }
        let columns = [a, b];
        for predicates in [
            vec![Predicate::equal(0, Value::Int(0))],
            vec![Predicate::one_of(0, vec![Value::Int(0), Value::Float(3.0)]), Predicate::compare(1, Compare::Ge, Value::Int(500))],
            vec![Predicate::equal(0, Value::Int(99))],
            vec![Predicate::compare(1, Compare::Lt, Value::Int(5000))],
        ] {
            let want: Vec<u32> =
                (0..1100).filter(|&r| predicates.iter().all(|p| p.passes(columns[p.column].get(r)))).collect();
            let got = Selection::new(&columns, &predicates, &|| Ok(())).unwrap();
            assert_eq!(got.iter().collect::<Vec<_>>(), want);
            assert_eq!(got.count(), want.len());
            assert!((0..1100).all(|r| got.contains(r) == want.contains(&r)));
        }
    }

    #[test]
    fn only_scalars_are_kept() {
        assert_eq!(filter_value(Some(Value::Int(3))), Value::Int(3));
        assert_eq!(filter_value(Some(Value::List(vec![Value::Int(3)]))), Value::Null);
        assert_eq!(filter_value(None), Value::Null);
    }
}
