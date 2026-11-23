//! The filter properties of a vector index in memory: per property, one value for each row
//! (or slot) of the index, so that a filter is checked without reading the store.

use std::collections::HashMap;

use graphbuff_core::Value;

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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn only_scalars_are_kept() {
        assert_eq!(filter_value(Some(Value::Int(3))), Value::Int(3));
        assert_eq!(filter_value(Some(Value::List(vec![Value::Int(3)]))), Value::Null);
        assert_eq!(filter_value(None), Value::Null);
    }
}
