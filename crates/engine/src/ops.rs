//! Cypher value semantics: three-valued equality, comparison, ordering and arithmetic.

use std::cmp::Ordering;

use graphbuff_core::{Error, Result, Temporal as T, Value, format_float};

/// Cypher `=`: `None` means the result is `null`.
pub fn cypher_eq(a: &Value, b: &Value) -> Option<bool> {
    use Value::*;
    match (a, b) {
        (Null, _) | (_, Null) => None,
        (Int(x), Int(y)) => Some(x == y),
        (Temporal(x), Temporal(y)) => Some(x.cypher_eq(y)),
        (Int(_) | Float(_), Int(_) | Float(_)) => Some(a.as_float() == b.as_float()),
        (List(x), List(y)) => {
            if x.len() != y.len() {
                return Some(false);
            }
            let mut unknown = false;
            for (p, q) in x.iter().zip(y) {
                match cypher_eq(p, q) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown { None } else { Some(true) }
        }
        (Map(x), Map(y)) => {
            if x.len() != y.len() || x.keys().ne(y.keys()) {
                return Some(false);
            }
            let mut unknown = false;
            for (p, q) in x.values().zip(y.values()) {
                match cypher_eq(p, q) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown { None } else { Some(true) }
        }
        _ => Some(a == b),
    }
}

/// Cypher `<`, `>`, ...: `None` when the values are not comparable (result is `null`).
pub fn cypher_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => Some(x.cmp(y)),
        (Int(_) | Float(_), Int(_) | Float(_)) => a.as_float()?.partial_cmp(&b.as_float()?),
        (String(x), String(y)) => Some(x.cmp(y)),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Temporal(x), Temporal(y)) => x.cypher_cmp(y),
        (List(x), List(y)) => {
            for (p, q) in x.iter().zip(y) {
                match cypher_cmp(p, q)? {
                    Ordering::Equal => continue,
                    o => return Some(o),
                }
            }
            Some(x.len().cmp(&y.len()))
        }
        _ => None,
    }
}

fn order_rank(v: &Value) -> u8 {
    match v {
        Value::Map(_) => 0,
        Value::Node(_) => 1,
        Value::Rel(_) => 2,
        Value::List(_) => 3,
        Value::Path(_) => 4,
        Value::Point(_) => 5,
        Value::Temporal(_) => 6,
        Value::String(_) => 7,
        Value::Bool(_) => 8,
        Value::Int(_) | Value::Float(_) => 9,
        Value::Null => 10,
    }
}

/// Total order used by `ORDER BY`, `min` and `max` (nulls sort last when ascending).
pub fn order_cmp(a: &Value, b: &Value) -> Ordering {
    use Value::*;
    let (ra, rb) = (order_rank(a), order_rank(b));
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (a, b) {
        (Int(x), Int(y)) => x.cmp(y),
        (Int(_) | Float(_), Int(_) | Float(_)) => {
            let (x, y) = (a.as_float().unwrap_or(0.0), b.as_float().unwrap_or(0.0));
            x.partial_cmp(&y)
                .unwrap_or_else(|| x.is_nan().cmp(&y.is_nan()))
        }
        (String(x), String(y)) => x.cmp(y),
        (Bool(x), Bool(y)) => x.cmp(y),
        (List(x), List(y)) => {
            for (p, q) in x.iter().zip(y) {
                match order_cmp(p, q) {
                    Ordering::Equal => continue,
                    o => return o,
                }
            }
            x.len().cmp(&y.len())
        }
        (Map(x), Map(y)) => {
            for ((ka, va), (kb, vb)) in x.iter().zip(y) {
                match ka.cmp(kb).then_with(|| order_cmp(va, vb)) {
                    Ordering::Equal => continue,
                    o => return o,
                }
            }
            x.len().cmp(&y.len())
        }
        (Node(x), Node(y)) => x.cmp(y),
        (Rel(x), Rel(y)) => x.cmp(y),
        (Path(x), Path(y)) => x.nodes.cmp(&y.nodes).then_with(|| x.rels.cmp(&y.rels)),
        (Temporal(x), Temporal(y)) => x.order_cmp(y),
        (Point(x), Point(y)) => x.order_cmp(y),
        _ => Ordering::Equal,
    }
}

/// String form used by `toString()` and string concatenation.
pub fn to_plain_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Float(f) => format_float(*f),
        other => other.to_string(),
    }
}

fn type_err(op: &str, a: &Value, b: &Value) -> Error {
    Error::Type(format!(
        "cannot apply '{op}' to {} and {}",
        a.type_name(),
        b.type_name()
    ))
}

fn overflow() -> Error {
    Error::Runtime("integer overflow".into())
}

pub fn add(a: Value, b: Value) -> Result<Value> {
    use Value::*;
    Ok(match (a, b) {
        (Null, _) | (_, Null) => Null,
        (Int(x), Int(y)) => Int(x.checked_add(y).ok_or_else(overflow)?),
        // A date or time plus a duration, or two durations.
        (Value::Temporal(t), Value::Temporal(T::Duration(d)))
        | (Value::Temporal(T::Duration(d)), Value::Temporal(t)) => Value::Temporal(t.plus(&d)?),
        (a @ (Int(_) | Float(_)), b @ (Int(_) | Float(_))) => {
            Float(a.as_float().unwrap() + b.as_float().unwrap())
        }
        (String(x), b @ (String(_) | Int(_) | Float(_) | Bool(_))) => {
            String(x + &to_plain_string(&b))
        }
        (a @ (Int(_) | Float(_) | Bool(_)), String(y)) => String(to_plain_string(&a) + &y),
        (List(mut x), List(y)) => {
            x.extend(y);
            List(x)
        }
        (List(mut x), b) => {
            x.push(b);
            List(x)
        }
        (a, List(mut y)) => {
            y.insert(0, a);
            List(y)
        }
        (a, b) => return Err(type_err("+", &a, &b)),
    })
}

pub fn arith(op: &str, a: Value, b: Value) -> Result<Value> {
    use Value::*;
    if a.is_null() || b.is_null() {
        return Ok(Null);
    }
    match (op, &a, &b) {
        ("-", Value::Temporal(t), Value::Temporal(T::Duration(d))) => {
            return Ok(Value::Temporal(t.plus(&d.negate()?)?));
        }
        ("*", Value::Temporal(T::Duration(d)), Int(_) | Float(_))
        | ("*", Int(_) | Float(_), Value::Temporal(T::Duration(d))) => {
            let k = if let Value::Temporal(_) = a { &b } else { &a };
            return Ok(Value::Temporal(T::Duration(d.scale(k.as_float().unwrap())?)));
        }
        ("/", Value::Temporal(T::Duration(d)), Int(_) | Float(_)) => {
            let k = b.as_float().unwrap();
            if k == 0.0 {
                return Err(Error::Runtime("division by zero".into()));
            }
            return Ok(Value::Temporal(T::Duration(d.scale(1.0 / k)?)));
        }
        _ => {}
    }
    match (&a, &b) {
        (Int(x), Int(y)) => {
            let (x, y) = (*x, *y);
            let r = match op {
                "-" => x.checked_sub(y),
                "*" => x.checked_mul(y),
                "/" => {
                    if y == 0 {
                        return Err(Error::Runtime("division by zero".into()));
                    }
                    x.checked_div(y)
                }
                "%" => {
                    if y == 0 {
                        return Err(Error::Runtime("division by zero".into()));
                    }
                    x.checked_rem(y)
                }
                "^" => return Ok(Float((x as f64).powf(y as f64))),
                _ => unreachable!("unknown arithmetic operator {op}"),
            };
            r.map(Int).ok_or_else(overflow)
        }
        (Int(_) | Float(_), Int(_) | Float(_)) => {
            let (x, y) = (a.as_float().unwrap(), b.as_float().unwrap());
            Ok(Float(match op {
                "-" => x - y,
                "*" => x * y,
                "/" => x / y,
                "%" => x % y,
                "^" => x.powf(y),
                _ => unreachable!("unknown arithmetic operator {op}"),
            }))
        }
        _ => Err(type_err(op, &a, &b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equality_is_three_valued() {
        assert_eq!(cypher_eq(&Value::Int(1), &Value::Float(1.0)), Some(true));
        assert_eq!(cypher_eq(&Value::Null, &Value::Int(1)), None);
        assert_eq!(
            cypher_eq(
                &Value::from(vec![1i64, 2]),
                &Value::List(vec![Value::Int(1), Value::Null])
            ),
            None
        );
        assert_eq!(
            cypher_eq(
                &Value::from(vec![1i64, 2]),
                &Value::List(vec![Value::Int(3), Value::Null])
            ),
            Some(false)
        );
        assert_eq!(cypher_eq(&Value::from("1"), &Value::Int(1)), Some(false));
    }

    #[test]
    fn ordering_puts_null_last() {
        let mut v = vec![
            Value::Null,
            Value::Int(2),
            Value::from("a"),
            Value::Float(1.5),
        ];
        v.sort_by(order_cmp);
        assert_eq!(
            v,
            vec![
                Value::from("a"),
                Value::Float(1.5),
                Value::Int(2),
                Value::Null
            ]
        );
    }

    #[test]
    fn arithmetic() {
        assert_eq!(
            add(Value::Int(1), Value::Float(0.5)).unwrap(),
            Value::Float(1.5)
        );
        assert_eq!(
            add(Value::from("a"), Value::Int(1)).unwrap(),
            Value::from("a1")
        );
        assert_eq!(
            arith("/", Value::Int(7), Value::Int(2)).unwrap(),
            Value::Int(3)
        );
        assert!(arith("/", Value::Int(1), Value::Int(0)).is_err());
        assert!(add(Value::Int(i64::MAX), Value::Int(1)).is_err());
    }
}
