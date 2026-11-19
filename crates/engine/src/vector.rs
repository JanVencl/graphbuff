//! Vectors: `vector.similarity.cosine` / `vector.similarity.euclidean`, and the similarity
//! measures vector indexes score with.
//!
//! A vector is a list of numbers. Scores are normalized to [0, 1] as Neo4j does:
//! cosine `(1 + cos) / 2`, euclidean `1 / (1 + d²)`, so a higher score is always more
//! similar.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Weak};

use graphbuff_core::{
    Error, GraphStore, IndexDef, IndexEntity, IndexKind, MAX_VECTOR_DIMENSIONS, NodeId, RelId, Result, Snapshot, Value,
    VectorSimilarity as Similarity,
};

use crate::procedures::{ProcedureContext, ProcedureRows};

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

// ---- the flat (exact) vector index ----------------------------------------------------------

/// The vector of an indexed entity: `value` if it is a list of exactly `dimensions`
/// finite numbers (with a non-zero norm for cosine); anything else is not indexed.
fn indexed_vector(value: &Value, dimensions: u32, similarity: Similarity) -> Option<Vec<f64>> {
    let Value::List(items) = value else { return None };
    if items.len() != dimensions as usize {
        return None;
    }
    let v: Vec<f64> = items
        .iter()
        .map(|x| match x {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) if f.is_finite() => Some(*f),
            _ => None,
        })
        .collect::<Option<_>>()?;
    if similarity == Similarity::Cosine && v.iter().all(|x| *x == 0.0) {
        return None;
    }
    Some(v)
}

/// The dimensions and similarity of a vector index.
fn vector_params(def: &IndexDef) -> (u32, Similarity) {
    match def.kind {
        IndexKind::Vector { dimensions, similarity } => (dimensions, similarity),
        IndexKind::Range => unreachable!("a vector index"),
    }
}

/// Calls `f` with the id and indexed property value of every entity `def` covers in
/// `store` (nodes with the label, or relationships of the type), in id order.
fn for_each_entity(
    store: &dyn GraphStore,
    def: &IndexDef,
    interrupt: &dyn Fn() -> Result<()>,
    mut f: impl FnMut(u64, Value),
) -> Result<()> {
    let key = &def.properties[0];
    let mut n = 0u32;
    let mut tick = || {
        n = n.wrapping_add(1);
        if n.is_multiple_of(4096) { interrupt() } else { Ok(()) }
    };
    match def.entity {
        IndexEntity::Node => {
            for id in store.nodes_with_label(&def.label) {
                let id = id?;
                tick()?;
                if let Some(v) = store.node_property(id, key)? {
                    f(id.0, v);
                }
            }
        }
        IndexEntity::Relationship => {
            for id in store.all_relationships() {
                let id = id?;
                tick()?;
                if store.relationship_type(id)?.as_deref() == Some(def.label.as_str())
                    && let Some(v) = store.relationship_property(id, key)?
                {
                    f(id.0, v);
                }
            }
        }
    }
    Ok(())
}

/// A vector index's vectors in one committed state: one f32 row per indexed entity, in
/// id order (unit length for cosine).
pub(crate) struct Matrix {
    def: IndexDef,
    ids: Vec<u64>,
    data: Vec<f32>,
    /// The largest row norm (euclidean), bounding the f32 rounding error of a score.
    max_norm: f64,
}

impl Matrix {
    fn build(store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<Matrix> {
        let (dimensions, similarity) = vector_params(def);
        let mut m = Matrix {
            def: def.clone(),
            ids: Vec::new(),
            data: Vec::new(),
            max_norm: 0.0,
        };
        for_each_entity(store, def, interrupt, |id, value| {
            let Some(v) = indexed_vector(&value, dimensions, similarity) else { return };
            let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
            let scale = if similarity == Similarity::Cosine { 1.0 / norm } else { 1.0 };
            m.ids.push(id);
            m.data.extend(v.iter().map(|x| (x * scale) as f32));
            if similarity == Similarity::Euclidean {
                m.max_norm = m.max_norm.max(norm);
            }
        })?;
        Ok(m)
    }

    /// Approximate scores (f32 rows, f64 sums) of every row against `q`, and how far
    /// one may be from the exact score of the stored vector.
    fn approximate(&self, q: &[f64]) -> (Vec<f64>, f64) {
        let (dimensions, similarity) = vector_params(&self.def);
        let d = dimensions as usize;
        match similarity {
            Similarity::Cosine => {
                let qn = q.iter().map(|x| x * x).sum::<f64>().sqrt();
                let unit: Vec<f64> = q.iter().map(|x| x / qn).collect();
                let scores = self
                    .data
                    .chunks_exact(d)
                    .map(|row| {
                        let dot: f64 = row.iter().zip(&unit).map(|(r, u)| f64::from(*r) * u).sum();
                        (1.0 + dot.clamp(-1.0, 1.0)) / 2.0
                    })
                    .collect();
                // Rows rounded to f32 move a dot product of unit vectors by < 2^-23.
                (scores, 1e-6)
            }
            Similarity::Euclidean => {
                let scores = self
                    .data
                    .chunks_exact(d)
                    .map(|row| {
                        let d2: f64 = row.iter().zip(q).map(|(r, x)| (f64::from(*r) - x).powi(2)).sum();
                        1.0 / (1.0 + d2)
                    })
                    .collect();
                // |Δd²| <= 2^-23 |a - b| |b| for rows rounded to f32, and the score moves
                // by at most |Δd²|; 8 times that, plus a little.
                let qn = q.iter().map(|x| x * x).sum::<f64>().sqrt();
                (scores, 2f64.powi(-20) * (qn + self.max_norm) * self.max_norm + 1e-12)
            }
        }
    }
}

/// The vector index matrices of one committed state, built on first use after a commit.
#[derive(Default)]
pub(crate) struct VectorCache {
    state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    snapshot: Option<Weak<dyn GraphStore + Send + Sync>>,
    matrices: HashMap<String, Arc<Matrix>>,
}

impl VectorCache {
    /// The matrix of index `def` in `snapshot`.
    fn matrix(&self, snapshot: &Snapshot, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<Arc<Matrix>> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let current = st.snapshot.as_ref().is_some_and(|w| std::ptr::addr_eq(w.as_ptr(), Arc::as_ptr(snapshot)));
        if !current {
            *st = CacheState {
                snapshot: Some(Arc::downgrade(snapshot)),
                matrices: HashMap::new(),
            };
        }
        if let Some(m) = st.matrices.get(&def.name)
            && m.def == *def
        {
            return Ok(m.clone());
        }
        let m = Arc::new(Matrix::build(&**snapshot, def, interrupt)?);
        st.matrices.insert(def.name.clone(), m.clone());
        Ok(m)
    }
}

/// `db.index.vector.queryNodes(indexName, k, vector)` (`entity` Node) and
/// `db.index.vector.queryRelationships(…)`: the `k` indexed entities most similar to
/// `vector`, as `[entity, score]` rows, best first (ties by id). Exact: the scores and
/// order are those of `vector.similarity.*` over the stored vectors.
pub fn query_index(ctx: &ProcedureContext<'_>, entity: IndexEntity, args: &[Value]) -> Result<ProcedureRows> {
    let procedure = match entity {
        IndexEntity::Node => "db.index.vector.queryNodes",
        IndexEntity::Relationship => "db.index.vector.queryRelationships",
    };
    let name = match &args[0] {
        Value::String(s) => s.clone(),
        o => return Err(Error::Type(format!("{procedure}: indexName must be a STRING, not {}", o.type_name()))),
    };
    let k = match &args[1] {
        Value::Int(k) if *k >= 1 => *k as usize,
        Value::Int(k) => return Err(Error::Semantic(format!("{procedure}: numberOfNearestNeighbours must be at least 1, not {k}"))),
        o => {
            return Err(Error::Type(format!(
                "{procedure}: numberOfNearestNeighbours must be an INTEGER, not {}",
                o.type_name()
            )));
        }
    };
    let Some(def) = ctx.store().indexes().into_iter().find(|d| d.name == name && d.is_vector()) else {
        return Err(Error::Semantic(format!("{procedure}: there is no vector index named `{name}`")));
    };
    if def.entity != entity {
        let other = match def.entity {
            IndexEntity::Node => "db.index.vector.queryNodes",
            IndexEntity::Relationship => "db.index.vector.queryRelationships",
        };
        return Err(Error::Semantic(format!(
            "{procedure}: `{name}` indexes {}s; use {other}",
            def.entity.text().to_lowercase()
        )));
    }
    let (dimensions, similarity) = vector_params(&def);
    let q = vector_arg(procedure, &args[2])?
        .ok_or_else(|| Error::Type(format!("{procedure}: the query vector is null")))?;
    if q.len() != dimensions as usize {
        return Err(Error::Semantic(format!(
            "{procedure}: index `{name}` has {dimensions} dimensions, the query vector {}",
            q.len()
        )));
    }
    if similarity == Similarity::Cosine && q.iter().all(|x| *x == 0.0) {
        return Err(Error::Semantic(format!("{procedure}: a zero vector has no cosine similarity")));
    }
    let interrupt = || ctx.check_interrupt();
    let store = ctx.store();
    let exact = |value: &Value| indexed_vector(value, dimensions, similarity).and_then(|v| similarity.score(&v, &q));

    let mut best: Vec<(f64, u64)> = match ctx.vectors() {
        // A committed state: the cached matrix, then exact scores near the cut.
        Some((cache, snapshot)) => {
            let m = cache.matrix(snapshot, &def, &interrupt)?;
            let (approx, tolerance) = m.approximate(&q);
            if approx.is_empty() {
                return Ok(Vec::new());
            }
            let cut = if approx.len() <= k {
                f64::NEG_INFINITY
            } else {
                let mut sorted = approx.clone();
                let i = sorted.len() - k;
                *sorted.select_nth_unstable_by(i, f64::total_cmp).1 - tolerance
            };
            let mut out = Vec::new();
            for (i, s) in approx.iter().enumerate() {
                if *s < cut {
                    continue;
                }
                let id = m.ids[i];
                let value = match entity {
                    IndexEntity::Node => store.node_property(NodeId(id), &def.properties[0])?,
                    IndexEntity::Relationship => store.relationship_property(RelId(id), &def.properties[0])?,
                };
                if let Some(score) = value.as_ref().and_then(exact) {
                    out.push((score, id));
                }
            }
            out
        }
        // Inside a transaction (or writing): every entity, as the query sees it.
        None => {
            let mut out = Vec::new();
            for_each_entity(store, &def, &interrupt, |id, value| {
                if let Some(score) = exact(&value) {
                    out.push((score, id));
                }
            })?;
            out
        }
    };
    best.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    best.truncate(k);
    Ok(best
        .into_iter()
        .map(|(score, id)| {
            let e = match entity {
                IndexEntity::Node => Value::Node(NodeId(id)),
                IndexEntity::Relationship => Value::Rel(RelId(id)),
            };
            vec![e, Value::Float(score)]
        })
        .collect())
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
