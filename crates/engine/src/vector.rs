//! Vectors: `vector.similarity.cosine` / `vector.similarity.euclidean`, and the similarity
//! measures vector indexes score with.
//!
//! A vector is a list of numbers. Scores are normalized to [0, 1] as Neo4j does:
//! cosine `(1 + cos) / 2`, euclidean `1 / (1 + d²)`, so a higher score is always more
//! similar.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::Path;

use graphbuff_core::{
    Changes, Error, GraphStore, HnswConfig, IndexDef, IndexEntity, IndexKind, Kernels, MAX_VECTOR_DIMENSIONS, NodeId,
    RelId, Result, Value, VectorRows, VectorScores, VectorSimilarity as Similarity, vector_row_score,
};
use graphbuff_cypher::ast::IndexType;

use crate::filter_column::{FilterColumn, Predicate, Selection, filter_value};
use crate::hnsw::{Graph, Loaded, Metric};
use crate::index_cache::{Cached, IndexCache};
use crate::procedures::{ProcedureContext, ProcedureRows};

/// The kind of index `CREATE [VECTOR | FULLTEXT | POINT] INDEX … OPTIONS {…}` asks for. A vector
/// index needs `indexConfig: {`vector.dimensions`: n}` and may name
/// `vector.similarity_function` (`'cosine'`, the default, or `'euclidean'`); it is
/// approximate (HNSW) with `vector.hnsw.enabled: true` or when `vector.hnsw.m` or
/// `vector.hnsw.ef_construction` is given (defaults 16 and 100); a full-text
/// index takes `fulltext.analyzer` and `fulltext.eventually_consistent` (see
/// [`fulltext_kind`](crate::fulltext::fulltext_kind)), a point index the `spatial.*`
/// bounds (see [`point_kind`](crate::point_index::point_kind)); `indexProvider` is accepted and
/// ignored.
pub fn index_kind(kind: IndexType, options: &BTreeMap<String, Value>) -> Result<IndexKind> {
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
    if kind == IndexType::Fulltext {
        return crate::fulltext::fulltext_kind(&config);
    }
    if kind == IndexType::Point {
        return crate::point_index::point_kind(&config);
    }
    if kind == IndexType::Text {
        return crate::text_index::text_kind(&config);
    }
    if kind == IndexType::Lookup {
        return crate::text_index::lookup_kind(&config);
    }
    if kind == IndexType::Range {
        if let Some(k) = config.keys().next() {
            return Err(Error::Semantic(format!("unknown index setting `{k}` for a range index")));
        }
        return Ok(IndexKind::Range);
    }
    let (mut dimensions, mut similarity) = (None, Similarity::Cosine);
    let (mut enabled, mut m, mut ef) = (None, None, None);
    let hnsw_int = |k: &str, v: &Value, min: u16, max: u16| -> Result<u16> {
        match v {
            Value::Int(n) if (i64::from(min)..=i64::from(max)).contains(n) => Ok(*n as u16),
            o => Err(Error::Semantic(format!("`{k}` needs an integer from {min} to {max}, not {o}"))),
        }
    };
    for (k, v) in &config {
        match (k.as_str(), v) {
            ("vector.hnsw.enabled", Value::Bool(b)) => enabled = Some(*b),
            ("vector.hnsw.enabled", o) => {
                return Err(Error::Type(format!("`vector.hnsw.enabled` needs a BOOLEAN, not {}", o.type_name())));
            }
            ("vector.hnsw.m", v) => m = Some(hnsw_int(k, v, 2, HnswConfig::MAX_M)?),
            ("vector.hnsw.ef_construction", v) => {
                ef = Some(hnsw_int(k, v, 1, HnswConfig::MAX_EF_CONSTRUCTION)?);
            }
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
            // Recorded on the definition: see `embedding_model`.
            ("vector.embedding_model", _) => {}
            (k, _) if k.starts_with("vector.") => {
                return Err(Error::Unsupported(format!("vector index setting `{k}` is not supported")));
            }
            _ => return Err(Error::Semantic(format!("unknown index setting `{k}` for a vector index"))),
        }
    }
    let dimensions = dimensions.ok_or_else(|| {
        Error::Semantic("a vector index needs OPTIONS {indexConfig: {`vector.dimensions`: n}}".into())
    })?;
    let hnsw = match enabled {
        Some(false) if m.is_some() || ef.is_some() => {
            return Err(Error::Semantic(
                "`vector.hnsw.m` and `vector.hnsw.ef_construction` need `vector.hnsw.enabled: true`".into(),
            ));
        }
        Some(false) => None,
        None if m.is_none() && ef.is_none() => None,
        _ => Some(HnswConfig {
            m: m.unwrap_or(HnswConfig::DEFAULT.m),
            ef_construction: ef.unwrap_or(HnswConfig::DEFAULT.ef_construction),
        }),
    };
    Ok(IndexKind::Vector { dimensions, similarity, hnsw })
}

/// The `vector.embedding_model` of a vector index's `OPTIONS`: the model that makes its
/// vectors, used by search to embed query text.
pub fn embedding_model(options: &BTreeMap<String, Value>) -> Result<Option<String>> {
    let Some(Value::Map(config)) = options.get("indexConfig") else { return Ok(None) };
    match config.get("vector.embedding_model") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(m)) if !m.trim().is_empty() => Ok(Some(m.clone())),
        Some(o) => Err(Error::Type(format!("`vector.embedding_model` needs a model name (a STRING), not {o}"))),
    }
}

/// A `SHOW INDEXES` / `db.indexes()` row: name, entity type, label or type, properties,
/// index type and options.
pub fn index_row(d: IndexDef) -> Vec<Value> {
    let options = match d.kind {
        IndexKind::Range => BTreeMap::new(),
        IndexKind::Point | IndexKind::Text | IndexKind::Lookup => {
            BTreeMap::from([("indexConfig".to_string(), Value::Map(BTreeMap::new()))])
        }
        IndexKind::Vector { dimensions, similarity, hnsw } => {
            let mut config = BTreeMap::from([
                ("vector.dimensions".to_string(), Value::Int(i64::from(dimensions))),
                ("vector.similarity_function".to_string(), Value::String(similarity.name().into())),
                ("vector.hnsw.enabled".to_string(), Value::Bool(hnsw.is_some())),
            ]);
            if let Some(m) = &d.embedding_model {
                config.insert("vector.embedding_model".into(), Value::String(m.clone()));
            }
            if let Some(h) = hnsw {
                config.insert("vector.hnsw.m".into(), Value::Int(i64::from(h.m)));
                config.insert("vector.hnsw.ef_construction".into(), Value::Int(i64::from(h.ef_construction)));
            }
            BTreeMap::from([("indexConfig".to_string(), Value::Map(config))])
        }
        IndexKind::Fulltext { analyzer } => BTreeMap::from([(
            "indexConfig".to_string(),
            Value::Map(BTreeMap::from([
                ("fulltext.analyzer".to_string(), Value::String(analyzer.name().into())),
                ("fulltext.eventually_consistent".to_string(), Value::Bool(false)),
            ])),
        )]),
    };
    // A full-text index on several labels shows them as `A|B`; a lookup index has none.
    let (labels, properties) = if d.is_lookup() {
        (Value::Null, Value::Null)
    } else {
        (
            Value::String(d.labels().collect::<Vec<_>>().join("|")),
            Value::List(d.properties.into_iter().map(Value::String).collect()),
        )
    };
    vec![
        Value::String(d.name),
        Value::String(d.entity.text().into()),
        labels,
        properties,
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
        IndexKind::Vector { dimensions, similarity, .. } => (dimensions, similarity),
        IndexKind::Range | IndexKind::Fulltext { .. } | IndexKind::Point | IndexKind::Text | IndexKind::Lookup => {
            unreachable!("a vector index")
        }
    }
}

/// Calls `f` with the id and indexed property value of every entity `def` covers in
/// `store` (nodes with the label, or relationships of the type), in id order.
fn for_each_entity(
    store: &dyn GraphStore,
    def: &IndexDef,
    interrupt: &dyn Fn() -> Result<()>,
    mut f: impl FnMut(u64, Value) -> Result<()>,
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
                    f(id.0, v)?;
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
                    f(id.0, v)?;
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

/// What a vector index keeps of an entity: its row, and its value of each filter property.
struct Entry {
    row: Vec<f32>,
    filters: Vec<Value>,
}

impl Entry {
    /// The entry of entity `id`, whose vector property is `value`, or `None` if the index
    /// skips it.
    fn read(store: &dyn GraphStore, def: &IndexDef, id: u64, value: &Value) -> Result<Option<Entry>> {
        let (dimensions, similarity) = vector_params(def);
        let Some(row) = matrix_row(value, dimensions, similarity) else { return Ok(None) };
        Ok(Some(Entry { row, filters: read_filters(store, def, id)? }))
    }
}

/// Property `key` of entity `id` of `def`.
fn property(store: &dyn GraphStore, def: &IndexDef, id: u64, key: &str) -> Result<Option<Value>> {
    match def.entity {
        IndexEntity::Node => store.node_property(NodeId(id), key),
        IndexEntity::Relationship => store.relationship_property(RelId(id), key),
    }
}

/// The values entity `id` has for the filter properties of `def`.
fn read_filters(store: &dyn GraphStore, def: &IndexDef, id: u64) -> Result<Vec<Value>> {
    def.filter_properties().iter().map(|key| Ok(filter_value(property(store, def, id, key)?))).collect()
}

/// The entries of every entity `def` covers in `store`, in id order.
fn entries(store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<Vec<(u64, Entry)>> {
    let mut out = Vec::new();
    for_each_entity(store, def, interrupt, |id, value| {
        if let Some(e) = Entry::read(store, def, id, &value)? {
            out.push((id, e));
        }
        Ok(())
    })?;
    Ok(out)
}

/// Empty columns for the filter properties of `def`.
fn filter_columns(def: &IndexDef) -> Vec<FilterColumn> {
    def.filter_properties().iter().map(|_| FilterColumn::new()).collect()
}

/// The values of `row` in `columns`.
fn filters_at(columns: &[FilterColumn], row: u32) -> Vec<Value> {
    columns.iter().map(|c| c.get(row).clone()).collect()
}

/// The entry entity `id` has in `store` for index `def`, or `None` if the index skips it
/// (or it is gone).
fn current_entry(store: &dyn GraphStore, def: &IndexDef, id: u64) -> Result<Option<Entry>> {
    let key = &def.properties[0];
    let value = match def.entity {
        IndexEntity::Node => {
            let n = NodeId(id);
            if store.node_exists(n)? && store.node_has_label(n, &def.label)? {
                store.node_property(n, key)?
            } else {
                None
            }
        }
        IndexEntity::Relationship => {
            let r = RelId(id);
            if store.relationship_exists(r)? && store.relationship_type(r)?.as_deref() == Some(def.label.as_str()) {
                store.relationship_property(r, key)?
            } else {
                None
            }
        }
    };
    match value {
        Some(v) => Entry::read(store, def, id, &v),
        None => Ok(None),
    }
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
    /// The filter properties' values by row (`null` for a row not in use).
    filters: Vec<FilterColumn>,
}

impl Matrix {
    fn build(store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<Matrix> {
        Ok(Matrix::layout(def, entries(store, def, interrupt)?))
    }

    /// The rows of `entries`, then rows not in use (an eighth more, at least 16).
    fn layout(def: &IndexDef, entries: Vec<(u64, Entry)>) -> Matrix {
        let (dimensions, _) = vector_params(def);
        let d = dimensions as usize;
        let n = entries.len();
        let capacity = n + (n / 8).max(16);
        let mut ids = Vec::with_capacity(capacity);
        let mut row_of = HashMap::with_capacity(n);
        let mut data = Vec::with_capacity(capacity * d);
        let mut filters = filter_columns(def);
        for (i, (id, entry)) in entries.into_iter().enumerate() {
            ids.push(id);
            row_of.insert(id, i as u32);
            data.extend(entry.row);
            for (column, value) in filters.iter_mut().zip(&entry.filters) {
                column.push(value);
            }
        }
        ids.resize(capacity, FREE);
        data.resize(capacity * d, 0.0);
        for column in &mut filters {
            column.pad(capacity);
        }
        Matrix {
            def: def.clone(),
            ids,
            row_of,
            free: (n as u32..capacity as u32).rev().collect(),
            rows: VectorRows::new(d, data),
            filters,
        }
    }

    /// The vector of row `r`.
    fn row(&self, r: u32) -> &[f32] {
        let (d, r) = (self.rows.dims(), r as usize);
        &self.rows.data()[r * d..(r + 1) * d]
    }

    /// Sets the filter values of row `r`.
    fn set_filters(&mut self, r: u32, values: &[Value]) {
        for (column, value) in self.filters.iter_mut().zip(values) {
            column.set(r, value);
        }
    }

    /// Brings the matrix to `store`, a later committed state, given (a superset of) the
    /// entities changed since: their rows are rewritten, freed or taken from the free
    /// ones; the row of an unchanged vector stays, whatever its filter values. Laid out
    /// anew when the free rows run out or most rows are free.
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
            let entry = current_entry(store, &self.def, id)?;
            if entry.is_some() || self.row_of.contains_key(&id) {
                updates.push((id, entry));
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
        for (id, entry) in &updates {
            if entry.is_none()
                && let Some(r) = self.row_of.remove(id)
            {
                self.ids[r as usize] = FREE;
                self.free.push(r);
                writes.insert(r, vec![0.0; d]);
                for column in &mut self.filters {
                    column.set(r, &Value::Null);
                }
            }
        }
        for (id, entry) in updates {
            let Some(Entry { row, filters }) = entry else { continue };
            let r = match self.row_of.get(&id) {
                Some(&r) => r,
                None => {
                    let r = self.free.pop().expect("free rows counted above");
                    self.ids[r as usize] = id;
                    self.row_of.insert(id, r);
                    r
                }
            };
            self.set_filters(r, &filters);
            if writes.contains_key(&r) || self.row(r) != row {
                writes.insert(r, row);
            }
        }
        if writes.is_empty() {
            return Ok(());
        }
        let at: Vec<u32> = writes.keys().copied().collect();
        let data: Vec<f32> = writes.into_values().flatten().collect();
        self.rows.patch(&at, &data);
        Ok(())
    }

    /// Lays the matrix out anew with `updates` applied.
    fn relayout(&mut self, updates: Vec<(u64, Option<Entry>)>) {
        let updated: HashMap<u64, Option<Entry>> = updates.into_iter().collect();
        let mut entries: Vec<(u64, Entry)> = Vec::with_capacity(self.row_of.len() + updated.len());
        for (i, &id) in self.ids.iter().enumerate() {
            if id != FREE && !updated.contains_key(&id) {
                let r = i as u32;
                entries.push((id, Entry { row: self.row(r).to_vec(), filters: filters_at(&self.filters, r) }));
            }
        }
        entries.extend(updated.into_iter().filter_map(|(id, entry)| entry.map(|e| (id, e))));
        entries.sort_unstable_by_key(|e| e.0);
        *self = Matrix::layout(&self.def, entries);
    }

    /// Approximate scores of every row against `q`, from `kernels` (the GPU, when the
    /// database has one), and a bound on how far one may be from the exact score of the
    /// stored vector. Rows not in use are scored too; [`Matrix::ids`] tells them apart.
    fn approximate(&self, q: &[f64], kernels: &dyn Kernels) -> (Vec<f64>, f64) {
        let (_, similarity) = vector_params(&self.def);
        let query = RowQuery::new(similarity, q);
        let s = kernels.vector_scores(&self.rows, &query.vector, similarity);
        let bound = s.error_bound(similarity, self.rows.max_norm(), query.norm);
        (s.values.iter().map(|&v| query.score(v)).collect(), bound)
    }
}

/// A query vector as rows are compared with it (unit length for cosine).
struct RowQuery {
    similarity: Similarity,
    vector: Vec<f64>,
    /// The norm of `vector`.
    norm: f64,
}

impl RowQuery {
    fn new(similarity: Similarity, q: &[f64]) -> RowQuery {
        let norm = q.iter().map(|x| x * x).sum::<f64>().sqrt();
        match similarity {
            Similarity::Cosine => RowQuery { similarity, vector: q.iter().map(|x| x / norm).collect(), norm: 1.0 },
            Similarity::Euclidean => RowQuery { similarity, vector: q.to_vec(), norm },
        }
    }

    /// The score of a row for which the kernels computed `v`. For cosine, `(1 + dot) / 2`
    /// moves by half as much as the dot product; for euclidean, `1 / (1 + d²)` moves by at
    /// most as much as d².
    fn score(&self, v: f64) -> f64 {
        match self.similarity {
            Similarity::Cosine => (1.0 + v.clamp(-1.0, 1.0)) / 2.0,
            Similarity::Euclidean => 1.0 / (1.0 + v.max(0.0)),
        }
    }

    /// Approximate scores of `rows` (with their entities), and a bound on how far one may
    /// be from the exact score of the stored vector.
    fn scores<'a>(
        &self,
        rows: impl Iterator<Item = (u64, &'a [f32])>,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<(Vec<(f64, u64)>, f64)> {
        let mut out = Vec::new();
        let mut max_norm = 1.0f64;
        for (n, (id, row)) in rows.enumerate() {
            if n % 4096 == 4095 {
                interrupt()?;
            }
            let v = vector_row_score(row, &self.vector, self.similarity);
            if self.similarity == Similarity::Euclidean {
                max_norm = max_norm.max(self.norm + v.max(0.0).sqrt());
            }
            out.push((self.score(v), id));
        }
        let bound = VectorScores { values: Vec::new(), f32_terms: 0 }.error_bound(self.similarity, max_norm, self.norm);
        Ok((out, bound))
    }
}

/// An approximate vector index of one committed state: an HNSW graph of its vectors
/// (unit length for cosine), one slot per indexed entity. A changed entity's old slot is
/// deleted and its vector inserted anew; the graph is built again once deleted slots
/// outnumber live ones.
#[derive(Clone)]
pub(crate) struct Approximate {
    def: IndexDef,
    graph: Graph,
    /// The entity of each slot.
    ids: Vec<u64>,
    /// The live slot of each entity.
    slot_of: HashMap<u64, u32>,
    /// The filter properties' values by slot (`null` for a deleted one).
    filters: Vec<FilterColumn>,
    /// Vectors inserted into and taken out of the graph, since it was loaded if it was.
    edits: u64,
}

impl Approximate {
    fn new(def: &IndexDef) -> Approximate {
        let (dimensions, similarity) = vector_params(def);
        let IndexKind::Vector { hnsw: Some(h), .. } = def.kind else { unreachable!("an approximate index") };
        let metric = match similarity {
            Similarity::Cosine => Metric::Cosine,
            Similarity::Euclidean => Metric::Euclidean,
        };
        Approximate {
            def: def.clone(),
            graph: Graph::new(dimensions as usize, metric, usize::from(h.m), usize::from(h.ef_construction)),
            ids: Vec::new(),
            slot_of: HashMap::new(),
            filters: filter_columns(def),
            edits: 0,
        }
    }

    /// The graph of `entries`, inserted in order.
    fn with(def: &IndexDef, entries: Vec<(u64, Entry)>, interrupt: &dyn Fn() -> Result<()>) -> Result<Approximate> {
        let mut a = Approximate::new(def);
        for (n, (id, entry)) in entries.into_iter().enumerate() {
            if n % 256 == 255 {
                interrupt()?;
            }
            a.insert(id, &entry);
        }
        Ok(a)
    }

    fn insert(&mut self, id: u64, entry: &Entry) {
        let slot = self.graph.insert(&entry.row);
        self.edits += 1;
        debug_assert_eq!(slot as usize, self.ids.len());
        self.ids.push(id);
        self.slot_of.insert(id, slot);
        for (column, value) in self.filters.iter_mut().zip(&entry.filters) {
            column.push(value);
        }
    }

    fn build(store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<Approximate> {
        Approximate::with(def, entries(store, def, interrupt)?, interrupt)
    }

    fn apply(&mut self, store: &dyn GraphStore, changes: &Changes, interrupt: &dyn Fn() -> Result<()>) -> Result<()> {
        let changed = match self.def.entity {
            IndexEntity::Node => &changes.nodes,
            IndexEntity::Relationship => &changes.rels,
        };
        for (n, &id) in changed.iter().enumerate() {
            if n % 256 == 255 {
                interrupt()?;
            }
            let entry = current_entry(store, &self.def, id)?;
            if let Some(slot) = self.slot_of.get(&id).copied() {
                if let Some(e) = &entry
                    && e.row == self.graph.vector(slot)
                {
                    // The same vector: at most its filter values changed.
                    for (column, value) in self.filters.iter_mut().zip(&e.filters) {
                        column.set(slot, value);
                    }
                    continue;
                }
                self.graph.remove(slot);
                self.edits += 1;
                self.slot_of.remove(&id);
                for column in &mut self.filters {
                    column.set(slot, &Value::Null);
                }
            }
            if let Some(entry) = entry {
                self.insert(id, &entry);
            }
        }
        if self.graph.removed() > 64 && self.graph.removed() > self.graph.live() {
            let mut entries: Vec<(u64, Entry)> = self
                .slot_of
                .iter()
                .map(|(&id, &slot)| {
                    (id, Entry { row: self.graph.vector(slot).to_vec(), filters: filters_at(&self.filters, slot) })
                })
                .collect();
            entries.sort_unstable_by_key(|e| e.0);
            let edits = self.edits;
            *self = Approximate::with(&self.def, entries, interrupt)?;
            self.edits += edits;
        }
        Ok(())
    }

    /// What a saved graph of index `def` must be of. The filter properties are not part of
    /// it: their values are read from the store.
    fn key(def: &IndexDef) -> Vec<u8> {
        let (dimensions, similarity) = vector_params(def);
        let IndexKind::Vector { hnsw: Some(h), .. } = def.kind else { unreachable!("an approximate index") };
        let mut key = format!("{}|{dimensions}|{}|{}|{}", def.entity.text(), similarity.name(), h.m, h.ef_construction);
        for s in [&def.name, &def.label, &def.properties[0]] {
            key += &format!("|{}:{s}", s.len());
        }
        key.into_bytes()
    }

    /// The contents of index `def` in `store`, with the graph saved in `file`: vectors it
    /// does not have are inserted. `None` if there is no graph of the index, or too much
    /// of it is not of this state.
    fn load(
        file: &Path,
        store: &dyn GraphStore,
        def: &IndexDef,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<Option<Approximate>> {
        if !file.is_file() {
            return Ok(None);
        }
        let entries = entries(store, def, interrupt)?;
        let at: HashMap<u64, usize> = entries.iter().enumerate().map(|(i, (id, _))| (*id, i)).collect();
        let mut a = Approximate::new(def);
        let empty = std::mem::replace(&mut a.graph, Graph::new(1, Metric::Cosine, 2, 1));
        let row = |id: u64| at.get(&id).map(|&i| &entries[i].1.row[..]);
        let Some(Loaded { graph, ids, dropped }) = Graph::load(file, &Approximate::key(def), empty, row) else {
            return Ok(None);
        };
        if dropped > 64 && dropped > graph.live() / LOAD_DROPPED_SHARE || graph.removed() > 64 && graph.removed() > graph.live() {
            return Ok(None);
        }
        a.graph = graph;
        a.edits = dropped as u64;
        for (slot, id) in ids.iter().enumerate() {
            let values = if a.graph.is_live(slot as u32) {
                a.slot_of.insert(*id, slot as u32);
                Some(&entries[at[id]].1.filters)
            } else {
                None
            };
            for (i, column) in a.filters.iter_mut().enumerate() {
                column.push(values.map_or(&Value::Null, |v| &v[i]));
            }
        }
        a.ids = ids;
        for (n, (id, entry)) in entries.iter().enumerate() {
            if n % 256 == 255 {
                interrupt()?;
            }
            if !a.slot_of.contains_key(id) {
                a.insert(*id, entry);
            }
        }
        Ok(Some(a))
    }

    /// The query vector as the graph's vectors are kept (unit length for cosine).
    fn query(&self, q: &[f64]) -> Vec<f32> {
        let (_, similarity) = vector_params(&self.def);
        let scale = match similarity {
            Similarity::Cosine => 1.0 / q.iter().map(|x| x * x).sum::<f64>().sqrt(),
            Similarity::Euclidean => 1.0,
        };
        q.iter().map(|x| (x * scale) as f32).collect()
    }

    /// The entities of the (up to) `n` vectors the graph finds closest to `q`.
    fn candidates(&self, q: &[f64], n: usize, interrupt: &dyn Fn() -> Result<()>) -> Result<Vec<u64>> {
        let bounds = crate::hnsw::Bounds { budget: usize::MAX, interrupt };
        let found = self.graph.search_where(&self.query(q), n, n, |_| true, bounds)?.unwrap_or_default();
        Ok(found.into_iter().map(|(_, slot)| self.ids[slot as usize]).collect())
    }

    /// The entities of the (up to) `n` selected vectors the graph finds closest to `q`, at
    /// least `min(k, selected.count())`; `None` if scoring all the selected ones takes
    /// less work, or the search turned out to take as much or to find too few.
    fn candidates_where(
        &self,
        q: &[f64],
        k: usize,
        n: usize,
        selected: &Selection,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<Option<Vec<u64>>> {
        let matches = selected.count();
        if matches <= n || filtered_search_cost(n, self.graph.slots(), matches) >= matches as f64 {
            return Ok(None);
        }
        let bounds = crate::hnsw::Bounds { budget: matches, interrupt };
        let found = self.graph.search_where(&self.query(q), n, n, |slot| selected.contains(slot), bounds)?;
        Ok(found
            .filter(|found| found.len() >= k.min(matches))
            .map(|found| found.into_iter().map(|(_, slot)| self.ids[slot as usize]).collect()))
    }
}

/// About how many distances a graph search that keeps `ef` candidates measures when
/// `matches` of the `slots` vectors pass its filter: it looks at all vectors around the
/// query to find the ones that pass.
fn filtered_search_cost(ef: usize, slots: usize, matches: usize) -> f64 {
    FILTERED_SEARCH_FACTOR * ef as f64 * slots as f64 / matches.max(1) as f64
}

/// The distances a graph search measures per candidate it keeps (`filtered_search_cost_measured`):
/// about 4 to 6 on clustered vectors and 10 to 26 on uniformly random ones, when a quarter
/// or more pass. With 8, the search chosen was the faster one, or close to it, for every
/// size and share measured (graph and scan take the same time near one in ten of 100,000
/// clustered 128-dimension vectors).
const FILTERED_SEARCH_FACTOR: f64 = 8.0;

/// An exact index scores only the selected rows, on the calling thread, if they are at
/// most this many or at most one in [`SUBSET_SHARE`]: otherwise all rows, with the
/// database's kernels.
const SUBSET_ROWS: usize = 1024;
const SUBSET_SHARE: usize = 8;

/// A saved graph is not used if more than one in this many of its vectors are no longer
/// those of their entities: the search of a graph suffers from taking out many.
const LOAD_DROPPED_SHARE: usize = 8;

/// Graphs of fewer vectors are not saved: building one takes a fraction of a second.
const MIN_SAVED: usize = 1000;

/// A graph is saved while queries run once this many of its vectors (and one in
/// [`SAVE_SHARE`]) were inserted or taken out since.
const SAVE_EDITS: u64 = 1000;
const SAVE_SHARE: u64 = 10;

/// Whether `name` is a file that graphs are saved in, or written to before.
pub(crate) fn is_graph_file(name: &str) -> bool {
    name.strip_prefix("hnsw-").is_some_and(|rest| {
        rest.strip_suffix(".graph").or(rest.strip_suffix(".tmp")).is_some_and(|hash| {
            hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())
        })
    })
}

/// The in-memory contents of a vector index of one committed state.
#[derive(Clone)]
pub(crate) enum VectorIndex {
    /// Exact: every vector is scored.
    Flat(Matrix),
    Approximate(Approximate),
}

impl Cached for VectorIndex {
    fn def(&self) -> &IndexDef {
        match self {
            VectorIndex::Flat(m) => &m.def,
            VectorIndex::Approximate(a) => &a.def,
        }
    }

    fn build(store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<VectorIndex> {
        Ok(match def.kind {
            IndexKind::Vector { hnsw: Some(_), .. } => VectorIndex::Approximate(Approximate::build(store, def, interrupt)?),
            _ => VectorIndex::Flat(Matrix::build(store, def, interrupt)?),
        })
    }

    fn apply(&mut self, store: &dyn GraphStore, changes: &Changes, interrupt: &dyn Fn() -> Result<()>) -> Result<()> {
        match self {
            VectorIndex::Flat(m) => m.apply(store, changes, interrupt),
            VectorIndex::Approximate(a) => a.apply(store, changes, interrupt),
        }
    }

    /// `hnsw-<hash of the index's name>.graph` for an approximate index.
    fn file_name(def: &IndexDef) -> Option<String> {
        let IndexKind::Vector { hnsw: Some(_), .. } = def.kind else { return None };
        // FNV-1a. (Another index with the same hash would only make both graphs be built:
        // the file says which index it is of.)
        let hash = def.name.bytes().fold(0xCBF2_9CE4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3));
        Some(format!("hnsw-{hash:016x}.graph"))
    }

    fn load(
        file: &Path,
        store: &dyn GraphStore,
        def: &IndexDef,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<Option<VectorIndex>> {
        Ok(Approximate::load(file, store, def, interrupt)?.map(VectorIndex::Approximate))
    }

    fn save(&self, file: &Path) -> io::Result<()> {
        match self {
            VectorIndex::Approximate(a) if a.graph.live() >= MIN_SAVED => {
                a.graph.save(file, &Approximate::key(&a.def), &a.ids)
            }
            VectorIndex::Approximate(_) => match std::fs::remove_file(file) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
            VectorIndex::Flat(_) => Ok(()),
        }
    }

    fn edits(&self) -> u64 {
        match self {
            VectorIndex::Flat(_) => 0,
            VectorIndex::Approximate(a) => a.edits,
        }
    }

    fn save_due(&self, unsaved: u64) -> bool {
        match self {
            VectorIndex::Flat(_) => false,
            VectorIndex::Approximate(a) => unsaved >= SAVE_EDITS.max(a.graph.live() as u64 / SAVE_SHARE),
        }
    }
}

/// The vector indexes of a committed state.
pub(crate) type VectorCache = IndexCache<VectorIndex>;

/// How many candidates an approximate search for `k` neighbours keeps (`ef`, four times
/// `k` and at least 200: recall@10 0.99 on clustered 128-dimension vectors): those are
/// scored exactly, and the best `k` returned.
fn search_width(k: usize) -> usize {
    k.saturating_mul(4).max(200)
}

/// `db.create.setNodeVectorProperty(node, key, vector)` (`entity` Node) and
/// `db.create.setRelationshipVectorProperty(relationship, key, vector)`: sets the property
/// to `vector`, which must be a non-empty list of finite numbers (stored as floats). A
/// `null` entity does nothing; a `null` vector removes the property.
pub fn set_vector_property(ctx: &ProcedureContext<'_>, entity: IndexEntity, args: &[Value]) -> Result<ProcedureRows> {
    let procedure = match entity {
        IndexEntity::Node => "db.create.setNodeVectorProperty",
        IndexEntity::Relationship => "db.create.setRelationshipVectorProperty",
    };
    let Value::String(key) = &args[1] else {
        return Err(Error::Type(format!("{procedure} needs a property key, got {}", args[1].type_name())));
    };
    let value = match &args[2] {
        Value::Null => Value::Null,
        Value::List(items) if !items.is_empty() && items.len() <= MAX_VECTOR_DIMENSIONS as usize => Value::List(
            items
                .iter()
                .map(|x| match x {
                    Value::Int(i) => Ok(Value::Float(*i as f64)),
                    Value::Float(f) if f.is_finite() => Ok(Value::Float(*f)),
                    other => Err(Error::Type(format!(
                        "{procedure} needs a vector of finite numbers, got an element {other}"
                    ))),
                })
                .collect::<Result<_>>()?,
        ),
        Value::List(items) => {
            return Err(Error::Runtime(format!(
                "{procedure} needs a vector of 1 to {MAX_VECTOR_DIMENSIONS} numbers, got {}",
                items.len()
            )));
        }
        other => return Err(Error::Type(format!("{procedure} needs a vector (a list of numbers), got {}", other.type_name()))),
    };
    ctx.set_property(&args[0], key, value)?;
    Ok(Vec::new())
}

/// `db.index.vector.queryNodes(indexName, k, vector)` (`entity` Node) and
/// `db.index.vector.queryRelationships(…)`: the `k` indexed entities most similar to
/// `vector`, as `[entity, score]` rows, best first (ties by id). The scores and order are
/// those of `vector.similarity.*` over the stored vectors. Exact, except for an
/// approximate (HNSW) index on a committed state: its graph proposes
/// [`search_width`]`(k)` candidates, which are scored exactly, so a true neighbour the
/// search misses can be absent.
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
    let q = query_vector(procedure, &def, &args[2])?
        .ok_or_else(|| Error::Type(format!("{procedure}: the query vector is null")))?;
    Ok(nearest(ctx, &def, &q, k, &[])?
        .0
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

/// How a search of a vector index found its results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    /// Through the graph of an approximate index.
    Graph,
    /// By scoring every vector that passes the filter.
    Matches,
    /// By scoring all vectors of the index.
    All,
    /// By reading the graph, inside a transaction.
    Store,
}

impl Method {
    pub const ALL: [Method; 4] = [Method::Graph, Method::Matches, Method::All, Method::Store];

    fn text(self) -> &'static str {
        match self {
            Method::Graph => "HNSW graph",
            Method::Matches => "matching vectors",
            Method::All => "all vectors",
            Method::Store => "scan of the store",
        }
    }
}

/// The scores and ids a search found, best first, and how, if there was anything to search.
pub(crate) type Nearest = (Vec<(f64, u64)>, Option<Method>);

/// How many searches used each method, for a `PROFILE` line: ` by HNSW graph`, or
/// ` by HNSW graph (3), matching vectors (1)`. Empty without any.
pub(crate) fn methods_text(searches: [u64; Method::ALL.len()]) -> String {
    let used: Vec<(Method, u64)> = Method::ALL.into_iter().zip(searches).filter(|(_, n)| *n > 0).collect();
    match &used[..] {
        [] => String::new(),
        [(method, _)] => format!(" by {}", method.text()),
        several => {
            let parts: Vec<String> = several.iter().map(|(m, n)| format!("{} ({n})", m.text())).collect();
            format!(" by {}", parts.join(", "))
        }
    }
}

/// The vector `value` to search index `def` with (`None` for null): of the index's
/// dimensions, and not zero for cosine. `what` searches, for errors.
pub(crate) fn query_vector(what: &str, def: &IndexDef, value: &Value) -> Result<Option<Vec<f64>>> {
    let (dimensions, similarity) = vector_params(def);
    let Some(q) = vector_arg(what, value)? else { return Ok(None) };
    if q.len() != dimensions as usize {
        return Err(Error::Semantic(format!(
            "{what}: index `{}` has {dimensions} dimensions, the query vector {}",
            def.name,
            q.len()
        )));
    }
    if similarity == Similarity::Cosine && q.iter().all(|x| *x == 0.0) {
        return Err(Error::Semantic(format!("{what}: a zero vector has no cosine similarity")));
    }
    Ok(Some(q))
}

/// The `k` entities of vector index `def` most similar to `q` (of the index's dimensions,
/// not zero for cosine) among those whose filter properties pass every predicate of
/// `filter`: their scores and ids, best first (ties by id). All that pass, if they are
/// fewer. Exact, except for an approximate index on a committed state when its graph is
/// searched; even then the scores are exact and `k` entities are returned if as many pass
/// a (non-empty) filter. Also how they were found, if there was anything to search.
pub(crate) fn nearest(
    ctx: &ProcedureContext<'_>,
    def: &IndexDef,
    q: &[f64],
    k: usize,
    filter: &[Predicate],
) -> Result<Nearest> {
    if k == 0 {
        return Ok((Vec::new(), None));
    }
    let (dimensions, similarity) = vector_params(def);
    let interrupt = || ctx.check_interrupt();
    let store = ctx.store();
    let exact = |value: &Value| indexed_vector(value, dimensions, similarity).and_then(|v| similarity.score(&v, q));
    // The exact scores of `ids`, from the stored vectors.
    let rescore = |ids: &mut dyn Iterator<Item = u64>| -> Result<Vec<(f64, u64)>> {
        let mut out = Vec::new();
        for (n, id) in ids.enumerate() {
            if n % 256 == 255 {
                interrupt()?;
            }
            if let Some(score) = property(store, def, id, &def.properties[0])?.as_ref().and_then(exact) {
                out.push((score, id));
            }
        }
        Ok(out)
    };
    // The exact scores of those of `approx` that may be among the best `k`, when each of
    // its scores is within `tolerance` of the exact one.
    let near_the_cut = |(approx, tolerance): (Vec<(f64, u64)>, f64)| -> Result<Vec<(f64, u64)>> {
        let cut = if approx.len() <= k {
            f64::NEG_INFINITY
        } else {
            let mut sorted: Vec<f64> = approx.iter().map(|a| a.0).collect();
            let i = sorted.len() - k;
            *sorted.select_nth_unstable_by(i, f64::total_cmp).1 - tolerance
        };
        rescore(&mut approx.iter().filter(|(s, _)| *s >= cut).map(|(_, id)| *id))
    };

    let (mut best, method) = match ctx.vectors() {
        // A committed state: the cached index, then exact scores near the cut.
        Some((cache, snapshot)) => {
            let index = cache.get(snapshot, def, &interrupt)?;
            let columns = match &*index {
                VectorIndex::Flat(m) => &m.filters,
                VectorIndex::Approximate(a) => &a.filters,
            };
            let selected = match filter {
                [] => None,
                filter => Some(Selection::new(columns, filter, &interrupt)?),
            };
            let query = RowQuery::new(similarity, q);
            match (&*index, selected) {
                (VectorIndex::Flat(m), Some(s)) if s.count() <= SUBSET_ROWS.max(m.ids.len() / SUBSET_SHARE) => {
                    let approx = query.scores(s.iter().map(|r| (m.ids[r as usize], m.row(r))), &interrupt)?;
                    (near_the_cut(approx)?, Method::Matches)
                }
                (VectorIndex::Flat(m), selected) => {
                    let (all, tolerance) = m.approximate(q, &*ctx.kernels());
                    let in_use = |r: usize| match &selected {
                        Some(s) => s.contains(r as u32),
                        None => m.ids[r] != FREE,
                    };
                    let approx = all.into_iter().enumerate().filter(|(r, _)| in_use(*r)).map(|(r, s)| (s, m.ids[r]));
                    (near_the_cut((approx.collect(), tolerance))?, Method::All)
                }
                (VectorIndex::Approximate(a), None) => {
                    let ids = a.candidates(q, search_width(k), &interrupt)?;
                    (rescore(&mut ids.into_iter())?, Method::Graph)
                }
                (VectorIndex::Approximate(a), Some(s)) => {
                    match a.candidates_where(q, k, search_width(k), &s, &interrupt)? {
                        Some(ids) => (rescore(&mut ids.into_iter())?, Method::Graph),
                        None => {
                            let rows = s.iter().map(|r| (a.ids[r as usize], a.graph.vector(r)));
                            (near_the_cut(query.scores(rows, &interrupt)?)?, Method::Matches)
                        }
                    }
                }
            }
        }
        // Inside a transaction (or writing): every entity, as the query sees it.
        None => {
            let mut out = Vec::new();
            for_each_entity(store, def, &interrupt, |id, value| {
                if let Some(score) = exact(&value) {
                    let values = if filter.is_empty() { Vec::new() } else { read_filters(store, def, id)? };
                    if filter.iter().all(|p| p.passes(&values[p.column])) {
                        out.push((score, id));
                    }
                }
                Ok(())
            })?;
            (out, Method::Store)
        }
    };
    best.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    best.truncate(k);
    Ok((best, Some(method)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(e: &VectorIndex) -> &Matrix {
        match e {
            VectorIndex::Flat(m) => m,
            VectorIndex::Approximate(_) => panic!("an exact index"),
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    /// The filter values of every entity index `name` holds, by id (as text), and the index.
    fn filters_by_id(db: &crate::Database, name: &str) -> (BTreeMap<u64, String>, std::sync::Arc<VectorIndex>) {
        let e = db.vector_cache.entry(name).expect("built");
        let (rows, columns) = match &*e {
            VectorIndex::Flat(m) => (&m.row_of, &m.filters),
            VectorIndex::Approximate(a) => (&a.slot_of, &a.filters),
        };
        let shown = rows
            .iter()
            .map(|(&id, &r)| {
                let values: Vec<String> = filters_at(columns, r).iter().map(|v| v.to_string()).collect();
                (id, values.join(" "))
            })
            .collect();
        (shown, e)
    }

    /// What the index `name` should hold, read from the graph.
    fn expected_filters(db: &crate::Database) -> BTreeMap<u64, String> {
        let r = db
            .execute("MATCH (n:V) WHERE size(n.e) = 2 RETURN id(n), n.tenant, n.year")
            .unwrap();
        r.rows
            .iter()
            .map(|row| {
                let shown = |v: &crate::ResultValue| match v.to_string() {
                    s if s.starts_with('[') => "null".to_string(),
                    s => s,
                };
                (row[0].to_string().parse().unwrap(), format!("{} {}", shown(&row[1]), shown(&row[2])))
            })
            .collect()
    }

    #[test]
    fn filter_columns_follow_commits() {
        for hnsw in [false, true] {
            let db = crate::Database::in_memory();
            let q = "CALL db.index.vector.queryNodes('v', 1, [1, 0]) YIELD node RETURN node.k";
            db.execute(&format!(
                "CREATE VECTOR INDEX v FOR (n:V) ON n.e WITH [n.tenant, n.year] \
                 OPTIONS {{indexConfig: {{`vector.dimensions`: 2, `vector.hnsw.enabled`: {hnsw}}}}}"
            ))
            .unwrap();
            db.execute(
                "UNWIND range(1, 100) AS i CREATE (:V {k: i, e: [0.1 * i, 1], tenant: 't' + toString(i % 7), year: 2000 + i % 5})",
            )
            .unwrap();
            let check = |what: &str| {
                db.execute(q).unwrap();
                let (got, e) = filters_by_id(&db, "v");
                assert_eq!(got, expected_filters(&db), "hnsw {hnsw}: {what}");
                e
            };
            let built = check("built");

            // Only filter values change: neither the rows nor the graph do.
            db.execute("MATCH (n:V) WHERE n.k <= 30 SET n.tenant = 'moved', n.year = null").unwrap();
            let after = check("filter values changed");
            match (&*built, &*after) {
                (VectorIndex::Flat(a), VectorIndex::Flat(b)) => assert_eq!(a.rows.id(), b.rows.id()),
                (VectorIndex::Approximate(a), VectorIndex::Approximate(b)) => {
                    assert_eq!((a.graph.slots(), b.graph.slots(), b.graph.removed()), (100, 100, 0));
                }
                _ => panic!("the kind of index changed"),
            }
            drop((built, after));

            for (what, change) in [
                ("values that are not kept", "MATCH (n:V) WHERE n.k = 50 SET n.tenant = [1, 2]"),
                ("vector and filters", "MATCH (n:V) WHERE n.k = 51 SET n.e = [9, 9], n.year = 1.5"),
                ("entities gone", "MATCH (n:V) WHERE n.k % 10 = 0 DETACH DELETE n"),
                ("entities that left the index", "MATCH (n:V) WHERE n.k % 10 = 1 REMOVE n:V"),
                ("vectors no longer indexed", "MATCH (n:V) WHERE n.k % 10 = 2 SET n.e = [1]"),
                ("a few new ones", "UNWIND range(101, 110) AS i CREATE (:V {k: i, e: [1, i], tenant: date('2026-01-01')})"),
                ("many new ones", "UNWIND range(200, 400) AS i CREATE (:V {k: i, e: [i, 1], year: i % 2 = 0})"),
                ("most vectors replaced", "MATCH (n:V) WHERE size(n.e) = 2 SET n.e = [n.e[1], n.e[0] + 1]"),
                ("again", "MATCH (n:V) WHERE size(n.e) = 2 SET n.e = [n.e[1], n.e[0] + 1], n.tenant = n.k"),
                ("most gone", "MATCH (n:V) WHERE n.k > 120 DETACH DELETE n"),
            ] {
                db.execute(change).unwrap_or_else(|e| panic!("{change}: {e}"));
                check(what);
            }
        }
    }

    #[test]
    fn relationship_filter_columns() {
        let db = crate::Database::in_memory();
        db.execute(
            "CREATE VECTOR INDEX v FOR ()-[r:NEAR]-() ON r.e WITH [r.since] OPTIONS {indexConfig: {`vector.dimensions`: 2}}",
        )
        .unwrap();
        db.execute("CREATE (a)-[:NEAR {e: [1, 0], since: 1}]->(b), (b)-[:NEAR {e: [0, 1]}]->(a), (a)-[:FAR {e: [0, 1], since: 3}]->(b)")
            .unwrap();
        let q = "CALL db.index.vector.queryRelationships('v', 1, [1, 0]) YIELD score RETURN score";
        db.execute(q).unwrap();
        let values = |db: &crate::Database| filters_by_id(db, "v").0.into_values().collect::<Vec<_>>();
        assert_eq!(values(&db), ["1", "null"]);
        db.execute("MATCH ()-[r:NEAR]->() SET r.since = coalesce(r.since, 1) + 1").unwrap();
        db.execute(q).unwrap();
        assert_eq!(values(&db), ["2", "2"]);
    }

    /// `test.nearest(index, k, q, filter)`: a filtered search, where `filter` is a list of
    /// `[filter property's number, operator, value]`.
    fn with_filtered_search() -> crate::Database {
        use crate::filter_column::Compare;
        let db = crate::Database::in_memory();
        db.register_procedure(
            crate::ProcedureSignature::new("test.nearest", ["index", "k", "q", "filter"], ["id", "score"]),
            |ctx, args| {
                let [Value::String(name), Value::Int(k), q, Value::List(filter)] = args else { panic!("{args:?}") };
                let def = ctx.store().indexes().into_iter().find(|d| d.name == *name).expect("an index");
                let filter: Vec<Predicate> = filter
                    .iter()
                    .map(|p| {
                        let Value::List(p) = p else { panic!("{p:?}") };
                        let [Value::Int(column), Value::String(op), v] = &p[..] else { panic!("{p:?}") };
                        let (c, v) = (*column as usize, v.clone());
                        match (op.as_str(), v) {
                            ("=", v) => Predicate::equal(c, v),
                            ("IN", Value::List(list)) => Predicate::one_of(c, list),
                            ("<", v) => Predicate::compare(c, Compare::Lt, v),
                            ("<=", v) => Predicate::compare(c, Compare::Le, v),
                            (">", v) => Predicate::compare(c, Compare::Gt, v),
                            (">=", v) => Predicate::compare(c, Compare::Ge, v),
                            (op, v) => panic!("{op} {v:?}"),
                        }
                    })
                    .collect();
                let q = vector_arg("test", q)?.expect("a vector");
                Ok(nearest(ctx, &def, &q, *k as usize, &filter)?
                    .0
                    .into_iter()
                    .map(|(score, id)| vec![Value::Int(id as i64), Value::Float(score)])
                    .collect())
            },
        )
        .unwrap();
        db
    }

    fn shown(r: crate::QueryResult) -> Vec<String> {
        r.rows.iter().map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")).collect()
    }

    /// The queries of a filtered search, through the index and as a scan of the graph.
    fn filtered(index: &str, function: &str, k: usize, q: &str, filter: &[(usize, &str, &str)]) -> (String, String) {
        let names = ["tenant", "year"];
        let list: Vec<String> = filter.iter().map(|(c, op, v)| format!("[{c}, '{op}', {v}]")).collect();
        let conditions: Vec<String> = filter.iter().map(|(c, op, v)| format!("n.{} {op} {v}", names[*c])).collect();
        (
            format!("CALL test.nearest('{index}', {k}, {q}, [{}]) YIELD id, score RETURN id, score", list.join(", ")),
            format!(
                "MATCH (n:V) WHERE {} WITH n, vector.similarity.{function}(n.e, {q}) AS score \
                 RETURN id(n), score ORDER BY score DESC, id(n) LIMIT {k}",
                conditions.join(" AND ")
            ),
        )
    }

    const FILTERS: [&[(usize, &str, &str)]; 11] = [
        &[(0, "=", "3")],
        &[(0, "=", "3.0")],
        &[(0, "=", "'none'")],
        &[(0, "=", "null")],
        &[(0, "IN", "[1, 2, null, 'x']")],
        &[(0, "IN", "[]")],
        &[(1, ">=", "date('2020-06-01')")],
        &[(1, ">", "date('2000-01-01')"), (1, "<=", "date('2021-01-01')")],
        &[(0, "<", "2"), (1, "<", "date('2019-01-01')")],
        &[(1, "<", "2020")],
        &[(0, ">=", "0")],
    ];

    #[test]
    fn filtered_exact_search() {
        for function in ["cosine", "euclidean"] {
            let db = with_filtered_search();
            db.execute(&format!(
                "CREATE VECTOR INDEX v FOR (n:V) ON n.e WITH [n.tenant, n.year] OPTIONS {{indexConfig: \
                 {{`vector.dimensions`: 3, `vector.similarity_function`: '{function}'}}}}"
            ))
            .unwrap();
            // Coarse coordinates, so that many scores are equal.
            db.execute(
                "UNWIND range(1, 4000) AS i CREATE (n:V {e: [i * 7 % 5 - 2, i * 13 % 7 - 3.0, i * 31 % 3 + 1]}) \
                 SET n.tenant = CASE i % 11 WHEN 0 THEN null WHEN 1 THEN 'x' WHEN 2 THEN 1.0 ELSE i % 5 END, \
                 n.year = CASE i % 9 WHEN 0 THEN i ELSE date({year: 2015 + i % 8, month: 1 + i % 12}) END",
            )
            .unwrap();
            let check = |what: &str| {
                for filter in FILTERS {
                    for k in [1, 10, 300, 5000] {
                        let (search, scan) = filtered("v", function, k, "[1, 0.5, -2]", filter);
                        assert_eq!(shown(db.execute(&search).unwrap()), shown(db.execute(&scan).unwrap()), "{what}: {search}");
                    }
                }
            };
            check("built");
            db.execute("MATCH (n:V) WHERE id(n) % 3 = 0 SET n.tenant = 3, n.year = date('2020-07-01')").unwrap();
            db.execute("MATCH (n:V) WHERE id(n) % 10 = 0 DETACH DELETE n").unwrap();
            check("after commits");
            // Inside a transaction, its own writes count.
            let mut tx = db.begin().unwrap();
            tx.execute("MATCH (n:V) WHERE id(n) % 4 = 1 SET n.tenant = 1").unwrap();
            for filter in FILTERS {
                let (search, scan) = filtered("v", function, 10, "[1, 0.5, -2]", filter);
                assert_eq!(shown(tx.execute(&search).unwrap()), shown(tx.execute(&scan).unwrap()), "in a transaction: {search}");
            }
        }
    }

    #[test]
    fn filtered_approximate_search() {
        let db = with_filtered_search();
        let dims = 16;
        db.execute(&format!(
            "CREATE VECTOR INDEX v FOR (n:V) ON n.e WITH [n.tenant, n.year] OPTIONS {{indexConfig: \
             {{`vector.dimensions`: {dims}, `vector.hnsw.enabled`: true}}}}"
        ))
        .unwrap();
        let mut seed = 7u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 40) as f64 / (1u64 << 24) as f64 * 2.0 - 1.0
        };
        // Forty clusters. `tenant` is independent of the vectors, `year` is the cluster: a
        // filter on it selects vectors far from (or only near) the query.
        let n = 6000;
        let centers: Vec<Vec<f64>> = (0..40).map(|_| (0..dims).map(|_| next()).collect()).collect();
        let mut vector = |c: usize| -> Vec<String> { centers[c].iter().map(|x| format!("{:.4}", x + 0.3 * next())).collect() };
        let values: Vec<String> =
            (0..n).map(|i| format!("{{e: [{}], tenant: {}, year: {}}}", vector(i % 40).join(", "), i / 40 % 100, i % 40)).collect();
        db.execute(&format!("UNWIND [{}] AS p CREATE (n:V) SET n = p", values.join(", "))).unwrap();
        let queries: Vec<String> = (0..10).map(|c| format!("[{}]", vector(c).join(", "))).collect();

        let mut through_graph = 0;
        for (filter, least) in [
            (&[(0, "=", "5")][..], 0.95),
            (&[(0, "<", "10")], 0.95),
            (&[(0, "<", "50")], 0.95),
            (&[(0, "<", "90")], 0.95),
            (&[(1, "=", "3")], 0.95),
            (&[(1, "=", "39")], 0.95),
            (&[(1, "<", "20")], 0.95),
            (&[(1, ">=", "20")], 0.95),
            (&[(1, ">=", "4"), (0, "<", "95")], 0.95),
            (&[(0, "=", "1000")], 1.0),
        ] {
            for k in [1, 10, 100] {
                let (mut hits, mut total) = (0, 0);
                for q in &queries {
                    let (search, scan) = filtered("v", "cosine", k, q, filter);
                    let (got, want) = (shown(db.execute(&search).unwrap()), shown(db.execute(&scan).unwrap()));
                    assert_eq!(got.len(), want.len(), "{filter:?}, k = {k}");
                    total += want.len();
                    // Entity and score: the scores are exact.
                    hits += got.iter().filter(|r| want.contains(r)).count();
                }
                assert!(hits as f64 >= least * total as f64, "{filter:?}, k = {k}: {hits} of {total}");
            }
            let entry = db.vector_cache.entry("v").unwrap();
            let VectorIndex::Approximate(a) = &*entry else { panic!("an approximate index") };
            let predicates: Vec<Predicate> = filter
                .iter()
                .map(|(c, op, v)| {
                    use crate::filter_column::Compare;
                    let v = Value::Int(v.parse().unwrap());
                    match *op {
                        "=" => Predicate::equal(*c, v),
                        "<" => Predicate::compare(*c, Compare::Lt, v),
                        _ => Predicate::compare(*c, Compare::Ge, v),
                    }
                })
                .collect();
            let s = Selection::new(&a.filters, &predicates, &|| Ok(())).unwrap();
            let q: Vec<f64> = queries[0].trim_matches(['[', ']']).split(", ").map(|x| x.parse().unwrap()).collect();
            // Few matches are always scored one by one.
            let graph = a.candidates_where(&q, 10, search_width(10), &s, &|| Ok(())).unwrap().is_some();
            assert!(!graph || s.count() * 4 > n, "{filter:?}");
            through_graph += usize::from(graph);
        }
        assert!(through_graph >= 2, "the graph was searched for {through_graph} filters");

        // A graph search that reaches fewer than `k` of the matches does not answer.
        let entry = db.vector_cache.entry("v").unwrap();
        let VectorIndex::Approximate(a) = &*entry else { panic!("an approximate index") };
        let mut a = a.clone();
        let all = Selection::new(&a.filters, &[Predicate::one_of(0, (0..100).map(Value::Int).collect())], &|| Ok(())).unwrap();
        let q = vec![1.0; dims];
        let found = |a: &Approximate, k| a.candidates_where(&q, k, 600, &all, &|| Ok(())).unwrap().map(|c| c.len());
        assert_eq!(found(&a, 150), Some(600));
        a.graph.cut_off(100);
        assert_eq!(found(&a, 150), None);
        assert!(found(&a, 1).is_some_and(|n| (1..=100).contains(&n)));
    }

    /// A query cancelled while it searches a graph stops there.
    #[test]
    fn graph_searches_are_interrupted() {
        let db = crate::Database::in_memory();
        db.execute("CREATE VECTOR INDEX v FOR (n:V) ON n.e WITH [n.p] OPTIONS {indexConfig: {`vector.dimensions`: 4, `vector.hnsw.enabled`: true}}")
            .unwrap();
        db.execute("UNWIND range(1, 5000) AS i CREATE (:V {p: i % 10, e: [sin(i), cos(i * 3), sin(i * 7), cos(i * 11)]})").unwrap();
        let cancel = crate::CancelHandle::new();
        let (handle, errors) = (cancel.clone(), std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        let seen = errors.clone();
        db.register_procedure(crate::ProcedureSignature::new("test.stopped", [""; 0], ["x"]), move |ctx, _| {
            let def = ctx.store().indexes().remove(0);
            let search = || nearest(ctx, &def, &[1.0, 0.0, 0.0, 0.0], 10, &[]);
            assert_eq!(search()?.1, Some(Method::Graph));
            handle.cancel();
            seen.lock().unwrap().push(search().err());
            Ok(Vec::new())
        })
        .unwrap();
        let options = crate::QueryOptions::default().with_cancel(cancel);
        let _ = db.execute_with_options("CALL test.stopped()", &Default::default(), &options);
        assert!(matches!(errors.lock().unwrap()[..], [Some(Error::Cancelled(_))]), "{errors:?}");
        // A filtered one too.
        let entry = db.vector_cache.entry("v").unwrap();
        let VectorIndex::Approximate(a) = &*entry else { panic!("an approximate index") };
        let s = Selection::new(&a.filters, &[Predicate::one_of(0, (0..9).map(Value::Int).collect())], &|| Ok(())).unwrap();
        let search = |interrupt: &dyn Fn() -> Result<()>| a.candidates_where(&[1.0, 0.0, 0.0, 0.0], 10, 200, &s, interrupt);
        assert!(search(&|| Ok(())).unwrap().is_some());
        assert!(search(&|| Err(Error::Cancelled("stopped".into()))).is_err());
    }

    /// How the distances a filtered graph search measures grow: see
    /// [`FILTERED_SEARCH_FACTOR`].
    #[test]
    #[ignore = "prints measurements"]
    fn filtered_search_cost_measured() {
        for (n, dims, clusters) in [(20_000, 32, 0), (20_000, 32, 200), (50_000, 64, 500), (100_000, 128, 1000), (50_000, 384, 500), (50_000, 384, 0)] {
            let mut seed = 11u64;
            let mut next = move || {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (seed >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
            };
            let centers: Vec<Vec<f32>> = (0..clusters).map(|_| (0..dims).map(|_| next()).collect()).collect();
            let mut vector = |i: usize| -> Vec<f32> {
                let spread = if clusters == 0 { 1.0 } else { 0.3 };
                let v: Vec<f32> = (0..dims)
                    .map(|d| centers.get(i % clusters.max(1)).map_or(0.0, |c| c[d]) + spread * next())
                    .collect();
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                v.iter().map(|x| x / norm).collect()
            };
            let mut g = Graph::new(dims, Metric::Cosine, 16, 100);
            for i in 0..n {
                let v = vector(i);
                g.insert(&v);
            }
            let queries: Vec<Vec<f32>> = (0..10).map(&mut vector).collect();
            for every in [1u32, 2, 4, 10, 30, 100] {
                let mut factors: Vec<f64> = queries
                    .iter()
                    .map(|q| {
                        let (mut lo, mut hi) = (0, 2 * n);
                        while hi - lo > 1 {
                            let mid = (lo + hi) / 2;
                            // Slots in a scrambled order, so that clusters do not line up.
                            let passes = |s: u32| (s.wrapping_mul(2_654_435_761) >> 8).is_multiple_of(every);
                            let bounds = crate::hnsw::Bounds { budget: mid, interrupt: &|| Ok(()) };
                            if g.search_where(q, 10, 200, passes, bounds).unwrap().is_some() { hi = mid } else { lo = mid }
                        }
                        hi as f64 / (200.0 * f64::from(every))
                    })
                    .collect();
                factors.sort_by(f64::total_cmp);
                // The time of the graph search, and of scoring every vector that passes.
                let passes = |s: u32| (s.wrapping_mul(2_654_435_761) >> 8).is_multiple_of(every);
                let t = std::time::Instant::now();
                for q in &queries {
                    g.search_where(q, 10, 200, passes, crate::hnsw::Bounds { budget: usize::MAX, interrupt: &|| Ok(()) }).unwrap();
                }
                let graph = t.elapsed().as_secs_f64() * 1e5;
                let t = std::time::Instant::now();
                for q in &queries {
                    let q: Vec<f64> = q.iter().map(|x| f64::from(*x)).collect();
                    let rows = (0..n as u32).filter(|s| passes(*s)).map(|s| (u64::from(s), g.vector(s)));
                    RowQuery::new(Similarity::Cosine, &q).scores(rows, &|| Ok(())).unwrap();
                }
                let scan = t.elapsed().as_secs_f64() * 1e5;
                let matches = n as f64 / f64::from(every);
                println!(
                    "{n} x {dims}, {clusters} clusters, 1 in {every}: factor {:.1} to {:.1}, median {:.1}; graph {graph:.0} us, scan {scan:.0} us, {}",
                    factors[0],
                    factors[9],
                    factors[5],
                    if filtered_search_cost(200, n, matches as usize) >= matches { "scan chosen" } else { "graph chosen" },
                );
            }
        }
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
        let first = db.vector_cache.entry("v").map(|e| flat(&e).rows.clone()).expect("built");
        assert_eq!(first.len(), 100 + 16);
        // Two commits, then a query: the rows they changed are written in place.
        db.execute("MATCH (n:V {k: 7}) SET n.e = [1, 0]").unwrap();
        db.execute("CREATE (:V {k: 0, e: [1, 0.1]})").unwrap();
        let r = db.execute(q).unwrap();
        assert_eq!(r.rows.iter().map(|r| r[0].to_string()).collect::<Vec<_>>(), ["7", "0"]);
        let patched = db.vector_cache.entry("v").map(|e| flat(&e).rows.clone()).expect("kept");
        assert_eq!(patched.len(), first.len());
        let since: Vec<(u64, Vec<u32>)> = patched.changed_since().map(|(id, rows)| (id, rows.to_vec())).collect();
        assert_eq!(since, [(first.id(), vec![6, 100])]);
        // A commit that changes no indexed entity leaves the rows as they are.
        db.execute("CREATE (:W {e: [1, 0]})").unwrap();
        db.execute(q).unwrap();
        assert_eq!(db.vector_cache.entry("v").map(|e| flat(&e).rows.clone()).unwrap().id(), patched.id());
        // More new entities than free rows: laid out anew.
        db.execute("UNWIND range(1, 40) AS i CREATE (:V {k: 1000 + i, e: [0, 1]})").unwrap();
        db.execute(q).unwrap();
        let grown = db.vector_cache.entry("v").map(|e| flat(&e).rows.clone()).unwrap();
        assert_eq!(grown.len(), 141 + 141 / 8);
        assert_eq!(grown.changed_since().count(), 0);
        // A dropped index's matrix goes with it.
        db.execute("DROP INDEX v").unwrap();
        assert!(db.vector_cache.entry("v").map(|e| flat(&e).rows.clone()).is_none());
    }
}
