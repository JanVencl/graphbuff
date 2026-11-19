//! Vectors: `vector.similarity.cosine` / `vector.similarity.euclidean`, and the similarity
//! measures vector indexes score with.
//!
//! A vector is a list of numbers. Scores are normalized to [0, 1] as Neo4j does:
//! cosine `(1 + cos) / 2`, euclidean `1 / (1 + d²)`, so a higher score is always more
//! similar.

use std::collections::BTreeMap;

use graphbuff_core::{Error, IndexDef, IndexKind, MAX_VECTOR_DIMENSIONS, Result, Value, VectorSimilarity as Similarity};

/// The kind of index `CREATE [VECTOR] INDEX … OPTIONS {…}` asks for. A vector index needs
/// `indexConfig: {`vector.dimensions`: n}` and may name `vector.similarity_function`
/// (`'cosine'`, the default, or `'euclidean'`); `indexProvider` is accepted and ignored.
pub fn index_kind(vector: bool, options: &BTreeMap<String, Value>) -> Result<IndexKind> {
    let mut config = BTreeMap::new();
    for (k, v) in options {
        match (k.as_str(), v) {
            ("indexProvider", Value::String(_) | Value::Null) => {}
            ("indexConfig", Value::Map(m)) => config = m.clone(),
            ("indexConfig", Value::Null) => {}
            ("indexProvider" | "indexConfig", o) => {
                return Err(Error::Type(format!("index option `{k}` does not accept {}", o.type_name())));
            }
            _ => return Err(Error::Semantic(format!("unknown index option `{k}`"))),
        }
    }
    if !vector {
        if let Some(k) = config.keys().next() {
            return Err(Error::Semantic(format!("unknown index setting `{k}` for a range index")));
        }
        return Ok(IndexKind::Range);
    }
    let (mut dimensions, mut similarity) = (None, Similarity::Cosine);
    for (k, v) in &config {
        match (k.as_str(), v) {
            ("vector.dimensions", Value::Int(n)) if (1..=i64::from(MAX_VECTOR_DIMENSIONS)).contains(n) => {
                dimensions = Some(*n as u32);
            }
            ("vector.dimensions", o) => {
                return Err(Error::Semantic(format!(
                    "`vector.dimensions` needs an integer from 1 to {MAX_VECTOR_DIMENSIONS}, not {}",
                    o
                )));
            }
            ("vector.similarity_function", Value::String(f)) => {
                similarity = Similarity::parse(f).ok_or_else(|| {
                    Error::Semantic(format!(
                        "`vector.similarity_function` is 'cosine' or 'euclidean', not '{f}'"
                    ))
                })?;
            }
            ("vector.similarity_function", o) => {
                return Err(Error::Type(format!("`vector.similarity_function` does not accept {}", o.type_name())));
            }
            (k, _) if k.starts_with("vector.") => {
                return Err(Error::Unsupported(format!("vector index setting `{k}` is not supported")));
            }
            _ => return Err(Error::Semantic(format!("unknown index setting `{k}` for a vector index"))),
        }
    }
    let dimensions = dimensions.ok_or_else(|| {
        Error::Semantic("a vector index needs OPTIONS {indexConfig: {`vector.dimensions`: n}}".into())
    })?;
    Ok(IndexKind::Vector { dimensions, similarity })
}

/// A `SHOW INDEXES` / `db.indexes()` row: name, entity type, label or type, properties,
/// index type and options.
pub fn index_row(d: IndexDef) -> Vec<Value> {
    let options = match d.kind {
        IndexKind::Range => BTreeMap::new(),
        IndexKind::Vector { dimensions, similarity } => BTreeMap::from([(
            "indexConfig".to_string(),
            Value::Map(BTreeMap::from([
                ("vector.dimensions".to_string(), Value::Int(i64::from(dimensions))),
                ("vector.similarity_function".to_string(), Value::String(similarity.name().into())),
            ])),
        )]),
    };
    vec![
        Value::String(d.name),
        Value::String(d.entity.text().into()),
        Value::String(d.label),
        Value::List(d.properties.into_iter().map(Value::String).collect()),
        Value::String(d.kind.text().into()),
        Value::Map(options),
    ]
}

/// The numbers of a vector argument of `function`: `Ok(None)` for null, an error for
/// anything but a non-empty list of numbers.
pub fn vector_arg(function: &str, v: &Value) -> Result<Option<Vec<f64>>> {
    let items = match v {
        Value::Null => return Ok(None),
        Value::List(items) => items,
        o => {
            return Err(Error::Type(format!(
                "{function}() needs a LIST<INTEGER | FLOAT> as a vector, not {}",
                o.type_name()
            )));
        }
    };
    if items.is_empty() {
        return Err(Error::Semantic(format!("{function}(): a vector needs at least one element")));
    }
    items
        .iter()
        .map(|x| match x {
            Value::Int(i) => Ok(*i as f64),
            Value::Float(f) if f.is_finite() => Ok(*f),
            Value::Float(_) => Err(Error::Semantic(format!("{function}(): vector elements must be finite"))),
            o => Err(Error::Type(format!(
                "{function}(): vector elements must be INTEGER or FLOAT, not {}",
                o.type_name()
            ))),
        })
        .collect::<Result<Vec<f64>>>()
        .map(Some)
}

/// `vector.similarity.cosine(a, b)` and `vector.similarity.euclidean(a, b)` (`name`
/// lowercased); `None` if `name` is not one of them.
pub fn vector_function(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let (shown, similarity) = match name {
        "vector.similarity.cosine" => ("vector.similarity.cosine", Similarity::Cosine),
        "vector.similarity.euclidean" => ("vector.similarity.euclidean", Similarity::Euclidean),
        n if n.starts_with("vector.") => {
            return Err(Error::Unsupported(format!("function `{name}` is not supported")));
        }
        _ => return Ok(None),
    };
    if args.len() != 2 {
        return Err(Error::Semantic(format!("{shown}() expects 2 argument(s), got {}", args.len())));
    }
    let (Some(a), Some(b)) = (vector_arg(shown, &args[0])?, vector_arg(shown, &args[1])?) else {
        return Ok(Some(Value::Null));
    };
    if a.len() != b.len() {
        return Err(Error::Semantic(format!(
            "{shown}(): the vectors have different dimensions ({} and {})",
            a.len(),
            b.len()
        )));
    }
    Ok(Some(similarity.score(&a, &b).map_or(Value::Null, Value::Float)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn scores_are_normalized() {
        let c = Similarity::Cosine;
        assert!(close(c.score(&[1.0, 0.0], &[2.0, 0.0]).unwrap(), 1.0));
        assert!(close(c.score(&[1.0, 0.0], &[0.0, 3.0]).unwrap(), 0.5));
        assert!(close(c.score(&[1.0, 0.0], &[-1.0, 0.0]).unwrap(), 0.0));
        assert_eq!(c.score(&[0.0, 0.0], &[1.0, 0.0]), None);
        let e = Similarity::Euclidean;
        assert!(close(e.score(&[1.0, 2.0], &[1.0, 2.0]).unwrap(), 1.0));
        assert!(close(e.score(&[0.0, 0.0], &[3.0, 4.0]).unwrap(), 1.0 / 26.0));
    }
}
