//! Vectors: `vector.similarity.cosine` / `vector.similarity.euclidean`, and the similarity
//! measures vector indexes score with.
//!
//! A vector is a list of numbers. Scores are normalized to [0, 1] as Neo4j does:
//! cosine `(1 + cos) / 2`, euclidean `1 / (1 + d²)`, so a higher score is always more
//! similar.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Weak};

use graphbuff_core::{
    Changes, Error, GraphStore, IndexDef, IndexEntity, IndexKind, Kernels, MAX_VECTOR_DIMENSIONS, NodeId, RelId, Result, Snapshot,
    Value, VectorRows, VectorSimilarity as Similarity,
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

/// The f32 row of a vector `value` as a [`Matrix`] keeps it (unit length for cosine), or
/// `None` if the index skips it.
fn matrix_row(value: &Value, dimensions: u32, similarity: Similarity) -> Option<Vec<f32>> {
    let v = indexed_vector(value, dimensions, similarity)?;
    let scale = match similarity {
        Similarity::Cosine => 1.0 / v.iter().map(|x| x * x).sum::<f64>().sqrt(),
        Similarity::Euclidean => 1.0,
    };
    Some(v.iter().map(|x| (x * scale) as f32).collect())
}

/// [`Matrix::ids`] of a row not in use.
const FREE: u64 = u64::MAX;

/// A vector index's vectors in one committed state: one f32 row per indexed entity (unit
/// length for cosine), in no particular order, plus rows not in use (all zeros), so that
/// a commit's changes are written into rows in place ([`Matrix::apply`]) and the number
/// of rows (the layout of a GPU copy) seldom changes.
#[derive(Clone)]
pub(crate) struct Matrix {
    def: IndexDef,
    /// The entity of each row; [`FREE`] for a row not in use.
    ids: Vec<u64>,
    /// The row of each entity.
    row_of: HashMap<u64, u32>,
    /// The rows not in use.
    free: Vec<u32>,
    rows: VectorRows,
}

impl Matrix {
    fn build(store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<Matrix> {
        let (dimensions, similarity) = vector_params(def);
        let mut entries = Vec::new();
        for_each_entity(store, def, interrupt, |id, value| {
            if let Some(row) = matrix_row(&value, dimensions, similarity) {
                entries.push((id, row));
            }
        })?;
        Ok(Matrix::layout(def, entries))
    }

    /// The rows of `entries`, then rows not in use (an eighth more, at least 16).
    fn layout(def: &IndexDef, entries: Vec<(u64, Vec<f32>)>) -> Matrix {
        let (dimensions, _) = vector_params(def);
        let d = dimensions as usize;
        let n = entries.len();
        let capacity = n + (n / 8).max(16);
        let mut ids = Vec::with_capacity(capacity);
        let mut row_of = HashMap::with_capacity(n);
        let mut data = Vec::with_capacity(capacity * d);
        for (i, (id, row)) in entries.into_iter().enumerate() {
            ids.push(id);
            row_of.insert(id, i as u32);
            data.extend(row);
        }
        ids.resize(capacity, FREE);
        data.resize(capacity * d, 0.0);
        Matrix {
            def: def.clone(),
            ids,
            row_of,
            free: (n as u32..capacity as u32).rev().collect(),
            rows: VectorRows::new(d, data),
        }
    }

    /// The row entity `id` has in `store`, or `None` if the index skips it (or it is gone).
    fn current(&self, store: &dyn GraphStore, id: u64) -> Result<Option<Vec<f32>>> {
        let (dimensions, similarity) = vector_params(&self.def);
        let key = &self.def.properties[0];
        let value = match self.def.entity {
            IndexEntity::Node => {
                let n = NodeId(id);
                if store.node_exists(n)? && store.node_has_label(n, &self.def.label)? {
                    store.node_property(n, key)?
                } else {
                    None
                }
            }
            IndexEntity::Relationship => {
                let r = RelId(id);
                if store.relationship_exists(r)?
                    && store.relationship_type(r)?.as_deref() == Some(self.def.label.as_str())
                {
                    store.relationship_property(r, key)?
                } else {
                    None
                }
            }
        };
        Ok(value.and_then(|v| matrix_row(&v, dimensions, similarity)))
    }

    /// Brings the matrix to `store`, a later committed state, given (a superset of) the
    /// entities changed since: their rows are rewritten, freed or taken from the free
    /// ones. Laid out anew when the free rows run out or most rows are free.
    fn apply(&mut self, store: &dyn GraphStore, changes: &Changes, interrupt: &dyn Fn() -> Result<()>) -> Result<()> {
        let changed = match self.def.entity {
            IndexEntity::Node => &changes.nodes,
            IndexEntity::Relationship => &changes.rels,
        };
        let mut updates = Vec::new();
        for (n, &id) in changed.iter().enumerate() {
            if n % 4096 == 4095 {
                interrupt()?;
            }
            let row = self.current(store, id)?;
            if row.is_some() || self.row_of.contains_key(&id) {
                updates.push((id, row));
            }
        }
        if updates.is_empty() {
            return Ok(());
        }
        let removed = updates.iter().filter(|(id, r)| r.is_none() && self.row_of.contains_key(id)).count();
        let added = updates.iter().filter(|(id, r)| r.is_some() && !self.row_of.contains_key(id)).count();
        let live = self.row_of.len() + added - removed;
        if added > self.free.len() + removed || (self.ids.len() > 64 && live < self.ids.len() / 2) {
            self.relayout(updates);
            return Ok(());
        }
        let d = self.rows.dims();
        let mut writes: BTreeMap<u32, Vec<f32>> = BTreeMap::new();
        // Free rows first, so that new entities can take them.
        for (id, row) in &updates {
            if row.is_none()
                && let Some(r) = self.row_of.remove(id)
            {
                self.ids[r as usize] = FREE;
                self.free.push(r);
                writes.insert(r, vec![0.0; d]);
            }
        }
        for (id, row) in updates {
            let Some(row) = row else { continue };
            let r = match self.row_of.get(&id) {
                Some(&r) => r,
                None => {
                    let r = self.free.pop().expect("free rows counted above");
                    self.ids[r as usize] = id;
                    self.row_of.insert(id, r);
                    r
                }
            };
            writes.insert(r, row);
        }
        let at: Vec<u32> = writes.keys().copied().collect();
        let data: Vec<f32> = writes.into_values().flatten().collect();
        self.rows.patch(&at, &data);
        Ok(())
    }

    /// Lays the matrix out anew with `updates` applied.
    fn relayout(&mut self, updates: Vec<(u64, Option<Vec<f32>>)>) {
        let d = self.rows.dims();
        let updated: HashMap<u64, Option<Vec<f32>>> = updates.into_iter().collect();
        let mut entries: Vec<(u64, Vec<f32>)> = Vec::with_capacity(self.row_of.len() + updated.len());
        for (i, &id) in self.ids.iter().enumerate() {
            if id != FREE && !updated.contains_key(&id) {
                entries.push((id, self.rows.data()[i * d..(i + 1) * d].to_vec()));
            }
        }
        entries.extend(updated.into_iter().filter_map(|(id, row)| row.map(|r| (id, r))));
        entries.sort_unstable_by_key(|e| e.0);
        *self = Matrix::layout(&self.def, entries);
    }

    /// Approximate scores of every row against `q`, from `kernels` (the GPU, when the
    /// database has one), and a bound on how far one may be from the exact score of the
    /// stored vector. Rows not in use are scored too; [`Matrix::ids`] tells them apart.
    fn approximate(&self, q: &[f64], kernels: &dyn Kernels) -> (Vec<f64>, f64) {
        let (_, similarity) = vector_params(&self.def);
        let qn = q.iter().map(|x| x * x).sum::<f64>().sqrt();
        match similarity {
            Similarity::Cosine => {
                let unit: Vec<f64> = q.iter().map(|x| x / qn).collect();
                let s = kernels.vector_scores(&self.rows, &unit, similarity);
                // The score is (1 + dot) / 2: it moves by half as much as the dot product.
                let bound = s.error_bound(similarity, 1.0, 1.0);
                (s.values.iter().map(|dot| (1.0 + dot.clamp(-1.0, 1.0)) / 2.0).collect(), bound)
            }
            Similarity::Euclidean => {
                let s = kernels.vector_scores(&self.rows, q, similarity);
                // 1 / (1 + d²) moves by at most as much as d².
                let bound = s.error_bound(similarity, self.rows.max_norm(), qn);
                (s.values.iter().map(|d2| 1.0 / (1.0 + d2.max(0.0))).collect(), bound)
            }
        }
    }
}

type WeakSnapshot = Weak<dyn GraphStore + Send + Sync>;

fn is(w: &WeakSnapshot, snapshot: &Snapshot) -> bool {
    std::ptr::addr_eq(w.as_ptr(), Arc::as_ptr(snapshot))
}

/// The vector index matrices of one committed state, kept up to date with later commits:
/// [`Database`](crate::Database) reports each commit's changes ([`VectorCache::published`]),
/// and the first query on a later state applies them to the matrices instead of building
/// them again.
#[derive(Default)]
pub(crate) struct VectorCache {
    state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    /// The last committed state published.
    latest: Option<WeakSnapshot>,
    /// The state the matrices are of.
    base: Option<WeakSnapshot>,
    matrices: HashMap<String, Arc<Matrix>>,
    /// The states published after `base`, oldest first, with what each commit changed.
    log: Vec<(WeakSnapshot, Changes)>,
    /// The ids in `log`.
    logged: usize,
}

impl CacheState {
    /// Forgets the matrices (and the log, of no use without them).
    fn clear(&mut self) {
        self.base = None;
        self.matrices.clear();
        self.log.clear();
        self.logged = 0;
    }

    /// Makes the matrices those of `snapshot`, entry `at` of the log, applying the
    /// changes of the commits up to it.
    fn advance(&mut self, snapshot: &Snapshot, at: usize, interrupt: &dyn Fn() -> Result<()>) -> Result<()> {
        let mut changes = Changes::default();
        for (_, c) in self.log.drain(..=at) {
            changes.extend(c);
        }
        self.logged = self.log.iter().map(|(_, c)| c.len()).sum();
        self.base = Some(Arc::downgrade(snapshot));
        if changes.unknown {
            self.matrices.clear();
            return Ok(());
        }
        changes.normalize();
        let names: Vec<String> = self.matrices.keys().cloned().collect();
        for name in names {
            let m = self.matrices.remove(&name).expect("listed above");
            // In place, unless a query still reads it.
            let mut m = Arc::try_unwrap(m).unwrap_or_else(|m| (*m).clone());
            m.apply(&**snapshot, &changes, interrupt)?;
            self.matrices.insert(name, Arc::new(m));
        }
        Ok(())
    }
}

impl VectorCache {
    /// Records a commit: `snapshot` is the new committed state, `changes` what the store
    /// says changed since the state published before it.
    pub fn published(&self, snapshot: &Snapshot, changes: Changes) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.latest = Some(Arc::downgrade(snapshot));
        if !st.matrices.is_empty() {
            // Drop the matrices of indexes that are gone.
            let defs = snapshot.indexes();
            st.matrices.retain(|_, m| defs.contains(&m.def));
        }
        if st.matrices.is_empty() || changes.unknown || st.logged + changes.len() > Changes::LIMIT {
            // Nothing to keep up to date, or more changes than are worth applying.
            st.clear();
            return;
        }
        st.logged += changes.len();
        st.log.push((Arc::downgrade(snapshot), changes));
    }

    /// The matrix of index `def` in `snapshot`.
    fn matrix(&self, snapshot: &Snapshot, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<Arc<Matrix>> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.base.as_ref().is_some_and(|b| is(b, snapshot)) {
            if let Some(at) = st.log.iter().position(|(w, _)| is(w, snapshot)) {
                if let Err(e) = st.advance(snapshot, at, interrupt) {
                    st.clear();
                    return Err(e);
                }
            } else if st.latest.as_ref().is_some_and(|l| is(l, snapshot)) {
                st.clear();
                st.base = Some(Arc::downgrade(snapshot));
            } else {
                // An earlier state, read by a query that began before the last commits.
                drop(st);
                return Ok(Arc::new(Matrix::build(&**snapshot, def, interrupt)?));
            }
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

    /// The rows of the matrix of index `name`, if one is kept: for tests.
    #[cfg(test)]
    fn rows(&self, name: &str) -> Option<VectorRows> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.matrices.get(name).map(|m| m.rows.clone())
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
            let (all, tolerance) = m.approximate(&q, &*ctx.kernels());
            let approx: Vec<(f64, u64)> =
                all.into_iter().zip(&m.ids).filter(|(_, id)| **id != FREE).map(|(s, id)| (s, *id)).collect();
            if approx.is_empty() {
                return Ok(Vec::new());
            }
            let cut = if approx.len() <= k {
                f64::NEG_INFINITY
            } else {
                let mut sorted: Vec<f64> = approx.iter().map(|a| a.0).collect();
                let i = sorted.len() - k;
                *sorted.select_nth_unstable_by(i, f64::total_cmp).1 - tolerance
            };
            let mut out = Vec::new();
            for &(s, id) in &approx {
                if s < cut {
                    continue;
                }
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

    #[test]
    fn commits_patch_the_matrix() {
        let db = crate::Database::in_memory();
        let q = "CALL db.index.vector.queryNodes('v', 2, [1, 0]) YIELD node RETURN node.k";
        db.execute("CREATE VECTOR INDEX v FOR (n:V) ON n.e OPTIONS {indexConfig: {`vector.dimensions`: 2}}").unwrap();
        db.execute("UNWIND range(1, 100) AS i CREATE (:V {k: i, e: [0, 1]})").unwrap();
        db.execute(q).unwrap();
        let first = db.vector_cache.rows("v").expect("built");
        assert_eq!(first.len(), 100 + 16);
        // Two commits, then a query: the rows they changed are written in place.
        db.execute("MATCH (n:V {k: 7}) SET n.e = [1, 0]").unwrap();
        db.execute("CREATE (:V {k: 0, e: [1, 0.1]})").unwrap();
        let r = db.execute(q).unwrap();
        assert_eq!(r.rows.iter().map(|r| r[0].to_string()).collect::<Vec<_>>(), ["7", "0"]);
        let patched = db.vector_cache.rows("v").expect("kept");
        assert_eq!(patched.len(), first.len());
        let since: Vec<(u64, Vec<u32>)> = patched.changed_since().map(|(id, rows)| (id, rows.to_vec())).collect();
        assert_eq!(since, [(first.id(), vec![6, 100])]);
        // A commit that changes no indexed entity leaves the rows as they are.
        db.execute("CREATE (:W {e: [1, 0]})").unwrap();
        db.execute(q).unwrap();
        assert_eq!(db.vector_cache.rows("v").unwrap().id(), patched.id());
        // More new entities than free rows: laid out anew.
        db.execute("UNWIND range(1, 40) AS i CREATE (:V {k: 1000 + i, e: [0, 1]})").unwrap();
        db.execute(q).unwrap();
        let grown = db.vector_cache.rows("v").unwrap();
        assert_eq!(grown.len(), 141 + 141 / 8);
        assert_eq!(grown.changed_since().count(), 0);
        // A dropped index's matrix goes with it.
        db.execute("DROP INDEX v").unwrap();
        assert!(db.vector_cache.rows("v").is_none());
    }
}
