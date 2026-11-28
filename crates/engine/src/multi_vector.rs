//! Multi-vector indexes: a vector per token (bge-m3's ColBERT output, `vector.embedMulti`),
//! stored one after another in one flat list, scored as `vector.maxSim` scores them. An
//! exact index scores every token vector against every query vector; an approximate one
//! takes candidates from a graph (HNSW) of all token vectors, the nearest few of each
//! query vector, and scores those entities.

use std::collections::{BTreeMap, HashMap, HashSet};

use graphbuff_core::{
    Changes, Error, GraphStore, IndexDef, IndexEntity, IndexKind, Result, Value, VectorRows, VectorScores,
    VectorSimilarity as Similarity, vector_row_score,
};

use crate::filter_column::{FilterColumn, Predicate, Selection};
use crate::hnsw::{Bounds, Graph, Metric};
use crate::procedures::ProcedureContext;
use crate::vector::{
    FREE, Method, Nearest, SUBSET_ROWS, SUBSET_SHARE, VectorIndex, best_k, covered_value, filter_columns, for_each_entity,
    max_sim_of, near_the_cut, property, read_filters, search_width, vector_arg,
};

/// [`MultiIndex`]'s owner of a row or graph node not in use.
const NONE: u32 = u32::MAX;

/// The query vectors of `value` (`None` for null): a list of vectors of the index's
/// `dimensions`, as `vector.embedMulti` makes it, or all of them in one flat list.
pub(crate) fn query_vectors(what: &str, def: &IndexDef, dimensions: u32, value: &Value) -> Result<Option<Vec<Vec<f64>>>> {
    let d = dimensions as usize;
    let vectors: Vec<Vec<f64>> = match value {
        Value::Null => return Ok(None),
        Value::List(l) if l.first().is_some_and(|x| matches!(x, Value::List(_))) => {
            l.iter().map(|v| vector_arg(what, v).map(Option::unwrap_or_default)).collect::<Result<_>>()?
        }
        Value::List(l) if !l.is_empty() => {
            let all = vector_arg(what, value)?.unwrap_or_default();
            if !all.len().is_multiple_of(d) {
                return Err(Error::Semantic(format!(
                    "{what}: index `{}` has {d} dimensions; a flat list of {} numbers is not vectors of them",
                    def.name,
                    all.len()
                )));
            }
            all.chunks_exact(d).map(<[f64]>::to_vec).collect()
        }
        Value::List(_) => return Err(Error::Semantic(format!("{what}: a multi-vector query needs at least one vector"))),
        o => {
            return Err(Error::Type(format!(
                "{what}: a multi-vector index is searched with a LIST<LIST<FLOAT>> (as vector.embedMulti makes it), not {}",
                o.type_name()
            )));
        }
    };
    if let Some(v) = vectors.iter().find(|v| v.len() != d) {
        return Err(Error::Semantic(format!(
            "{what}: index `{}` has {d} dimensions, a query vector {}",
            def.name,
            v.len()
        )));
    }
    Ok(Some(vectors))
}

/// The numbers of a stored value of `dimensions`-vectors one after another, or `None` if
/// the index skips it: not a non-empty list of finite numbers whose length is a multiple.
fn stored_vectors(value: &Value, dimensions: usize) -> Option<Vec<f64>> {
    let Value::List(items) = value else { return None };
    if items.is_empty() || !items.len().is_multiple_of(dimensions) {
        return None;
    }
    items
        .iter()
        .map(|x| match x {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) if f.is_finite() => Some(*f),
            _ => None,
        })
        .collect()
}

fn dimensions(def: &IndexDef) -> usize {
    let IndexKind::MultiVector { dimensions, .. } = def.kind else { unreachable!("a multi-vector index") };
    dimensions as usize
}

/// The exact score `vector.maxSim(q, value)`, if the index has `value`.
fn exact(q: &[Vec<f64>], value: &Value, d: usize) -> Option<f64> {
    stored_vectors(value, d).map(|v| max_sim_of(q, v.chunks_exact(d)))
}

/// What the index keeps of an entity: its vectors (rounded to f32, one after another) and
/// its filter values.
struct Doc {
    vectors: Vec<f32>,
    filters: Vec<Value>,
}

fn read_doc(store: &dyn GraphStore, def: &IndexDef, id: u64, value: &Value) -> Result<Option<Doc>> {
    let Some(v) = stored_vectors(value, dimensions(def)) else { return Ok(None) };
    Ok(Some(Doc { vectors: v.iter().map(|x| *x as f32).collect(), filters: read_filters(store, def, id)? }))
}

/// Where the token vectors are kept.
#[derive(Clone)]
enum Tokens {
    /// One row each, plus rows not in use (zeros), written in place as commits change them.
    Flat {
        rows: VectorRows,
        /// The slot of each row; [`NONE`] for one not in use.
        owner: Vec<u32>,
        free: Vec<u32>,
    },
    /// A graph of their directions (unit length), with their norms.
    Graph {
        graph: Graph,
        /// The slot of each node.
        owner: Vec<u32>,
        norms: Vec<f32>,
        /// Whether a slot has a zero vector (not in the graph).
        zero: Vec<bool>,
        max_norm: f64,
    },
}

/// A multi-vector index of one committed state. Each indexed entity has a slot and its
/// vectors (rows or graph nodes); a changed entity gives up its slot and takes a new one.
/// Once given up slots outnumber the others (or deleted graph nodes the live ones), the
/// index is built again.
#[derive(Clone)]
pub(crate) struct MultiIndex {
    def: IndexDef,
    /// The entity of each slot; [`FREE`] for one given up.
    ids: Vec<u64>,
    slot_of: HashMap<u64, u32>,
    /// The rows or graph nodes of each slot.
    vectors_of: Vec<Vec<u32>>,
    filters: Vec<FilterColumn>,
    dead: usize,
    tokens: Tokens,
}

impl MultiIndex {
    pub fn def(&self) -> &IndexDef {
        &self.def
    }

    pub fn build(store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<MultiIndex> {
        let mut docs = Vec::new();
        for_each_entity(store, def, interrupt, |id, value| {
            if let Some(doc) = read_doc(store, def, id, &value)? {
                docs.push((id, doc));
            }
            Ok(())
        })?;
        let d = dimensions(def);
        let tokens = match def.kind {
            IndexKind::MultiVector { hnsw: Some(h), .. } => Tokens::Graph {
                graph: Graph::new(d, Metric::Cosine, usize::from(h.m), usize::from(h.ef_construction)),
                owner: Vec::new(),
                norms: Vec::new(),
                zero: Vec::new(),
                max_norm: 0.0,
            },
            _ => {
                let n: usize = docs.iter().map(|(_, doc)| doc.vectors.len() / d).sum();
                flat_rows(d, n)
            }
        };
        let mut ix = MultiIndex {
            def: def.clone(),
            ids: Vec::new(),
            slot_of: HashMap::new(),
            vectors_of: Vec::new(),
            filters: filter_columns(def),
            dead: 0,
            tokens,
        };
        let mut writes = BTreeMap::new();
        for (n, (id, doc)) in docs.into_iter().enumerate() {
            if n % 256 == 255 {
                interrupt()?;
            }
            ix.add(id, doc, &mut writes);
        }
        ix.write(writes);
        Ok(ix)
    }

    /// The number of vectors in use.
    fn live_vectors(&self) -> usize {
        match &self.tokens {
            Tokens::Flat { rows, free, .. } => rows.len() - free.len(),
            Tokens::Graph { graph, .. } => graph.live(),
        }
    }

    /// Gives entity `id` a new slot with `doc`; rows to write go to `writes` (a flat
    /// index has enough free rows).
    fn add(&mut self, id: u64, doc: Doc, writes: &mut BTreeMap<u32, Vec<f32>>) {
        let slot = self.ids.len() as u32;
        self.ids.push(id);
        self.slot_of.insert(id, slot);
        for (column, value) in self.filters.iter_mut().zip(&doc.filters) {
            column.push(value);
        }
        let d = dimensions(&self.def);
        let mut mine = Vec::with_capacity(doc.vectors.len() / d);
        match &mut self.tokens {
            Tokens::Flat { owner, free, .. } => {
                for v in doc.vectors.chunks_exact(d) {
                    let r = free.pop().expect("free rows made beforehand");
                    owner[r as usize] = slot;
                    writes.insert(r, v.to_vec());
                    mine.push(r);
                }
            }
            Tokens::Graph { graph, owner, norms, zero, max_norm } => {
                zero.push(false);
                for v in doc.vectors.chunks_exact(d) {
                    let norm = v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
                    if norm == 0.0 {
                        zero[slot as usize] = true;
                        continue;
                    }
                    let unit: Vec<f32> = v.iter().map(|x| (f64::from(*x) / norm) as f32).collect();
                    let node = graph.insert(&unit);
                    debug_assert_eq!(node as usize, owner.len());
                    owner.push(slot);
                    norms.push(norm as f32);
                    *max_norm = max_norm.max(norm);
                    mine.push(node);
                }
            }
        }
        self.vectors_of.push(mine);
    }

    /// Gives up the slot of entity `id`, if it has one.
    fn remove(&mut self, id: u64, writes: &mut BTreeMap<u32, Vec<f32>>) {
        let Some(slot) = self.slot_of.remove(&id) else { return };
        self.ids[slot as usize] = FREE;
        for column in &mut self.filters {
            column.set(slot, &Value::Null);
        }
        self.dead += 1;
        let mine = std::mem::take(&mut self.vectors_of[slot as usize]);
        match &mut self.tokens {
            Tokens::Flat { rows, owner, free } => {
                for r in mine {
                    owner[r as usize] = NONE;
                    free.push(r);
                    writes.insert(r, vec![0.0; rows.dims()]);
                }
            }
            Tokens::Graph { graph, owner, zero, .. } => {
                zero[slot as usize] = false;
                for node in mine {
                    graph.remove(node);
                    owner[node as usize] = NONE;
                }
            }
        }
    }

    /// Writes `writes` into the rows of a flat index.
    fn write(&mut self, writes: BTreeMap<u32, Vec<f32>>) {
        if let Tokens::Flat { rows, .. } = &mut self.tokens
            && !writes.is_empty()
        {
            let at: Vec<u32> = writes.keys().copied().collect();
            let data: Vec<f32> = writes.into_values().flatten().collect();
            rows.patch(&at, &data);
        }
    }

    /// Lays the rows of a flat index out anew, with room for `more`.
    fn relayout(&mut self, more: usize) {
        let Tokens::Flat { rows, .. } = &self.tokens else { return };
        let d = rows.dims();
        let live: usize = self.vectors_of.iter().map(Vec::len).sum();
        let Tokens::Flat { rows: new_rows, mut owner, mut free } = flat_rows(d, live + more) else { unreachable!() };
        let mut data = new_rows.data().to_vec();
        let mut next = 0u32;
        for (slot, mine) in self.vectors_of.iter_mut().enumerate() {
            for r in mine.iter_mut() {
                let at = next as usize;
                data[at * d..(at + 1) * d].copy_from_slice(&rows.data()[*r as usize * d..(*r as usize + 1) * d]);
                owner[at] = slot as u32;
                *r = next;
                next += 1;
            }
        }
        free.retain(|r| *r >= next);
        self.tokens = Tokens::Flat { rows: VectorRows::new(d, data), owner, free };
    }

    pub fn apply(&mut self, store: &dyn GraphStore, changes: &Changes, interrupt: &dyn Fn() -> Result<()>) -> Result<()> {
        let changed = match self.def.entity {
            IndexEntity::Node => &changes.nodes,
            IndexEntity::Relationship => &changes.rels,
        };
        let mut updates = Vec::new();
        for (n, &id) in changed.iter().enumerate() {
            if n % 256 == 255 {
                interrupt()?;
            }
            let def = &self.def;
            let doc = match covered_value(store, def, id, &def.properties[0])? {
                Some(v) => read_doc(store, def, id, &v)?,
                None => None,
            };
            if doc.is_some() || self.slot_of.contains_key(&id) {
                updates.push((id, doc));
            }
        }
        if updates.is_empty() {
            return Ok(());
        }
        let mut writes = BTreeMap::new();
        for (id, _) in &updates {
            self.remove(*id, &mut writes);
        }
        let d = dimensions(&self.def);
        let wanted: usize = updates.iter().filter_map(|(_, doc)| doc.as_ref()).map(|doc| doc.vectors.len() / d).sum();
        if let Tokens::Flat { free, .. } = &self.tokens
            && free.len() < wanted
        {
            self.write(std::mem::take(&mut writes));
            self.relayout(wanted);
        }
        for (id, doc) in updates {
            if let Some(doc) = doc {
                self.add(id, doc, &mut writes);
            }
        }
        self.write(writes);
        let removed_nodes = match &self.tokens {
            Tokens::Graph { graph, .. } => graph.removed(),
            Tokens::Flat { .. } => 0,
        };
        if (self.dead > 64 && self.dead > self.slot_of.len()) || (removed_nodes > 64 && removed_nodes > self.live_vectors()) {
            *self = MultiIndex::build(store, &self.def, interrupt)?;
        }
        Ok(())
    }

    /// The approximate MaxSim of slot `slot` against `q` (from the f32 vectors kept).
    fn approx(&self, slot: u32, q: &[Vec<f64>]) -> f64 {
        let mine = &self.vectors_of[slot as usize];
        let total: f64 = q
            .iter()
            .map(|a| {
                let best = match &self.tokens {
                    Tokens::Flat { rows, .. } => {
                        let d = rows.dims();
                        mine.iter()
                            .map(|&r| vector_row_score(&rows.data()[r as usize * d..(r as usize + 1) * d], a, Similarity::Cosine))
                            .fold(f64::NEG_INFINITY, f64::max)
                    }
                    Tokens::Graph { graph, norms, zero, .. } => {
                        let start = if zero[slot as usize] { 0.0 } else { f64::NEG_INFINITY };
                        mine.iter()
                            .map(|&n| f64::from(norms[n as usize]) * vector_row_score(graph.vector(n), a, Similarity::Cosine))
                            .fold(start, f64::max)
                    }
                };
                best
            })
            .sum();
        total / q.len() as f64
    }

    /// A bound on how far an approximate MaxSim may be from the exact one. Rounding the
    /// vectors to f32 (for a graph, their directions and norms apart) moves each dot
    /// product by a few units of roundoff of `|r||q|`; MaxSim moves by at most the mean of
    /// those.
    fn tolerance(&self, q: &[Vec<f64>], f32_terms: u32) -> f64 {
        let (max_norm, factor) = match &self.tokens {
            Tokens::Flat { rows, .. } => (rows.max_norm(), 1.0),
            Tokens::Graph { max_norm, .. } => (*max_norm, 4.0),
        };
        let unit = VectorScores { values: Vec::new(), f32_terms }.error_bound(Similarity::Cosine, 1.0, 1.0);
        let norms: f64 = q.iter().map(|a| a.iter().map(|x| x * x).sum::<f64>().sqrt()).sum::<f64>() / q.len() as f64;
        factor * unit * max_norm.max(1e-300) * norms
    }

    /// Approximate scores of all slots in use (in `selected`, if given), from all rows of
    /// a flat index with the database's kernels.
    fn score_all(
        &self,
        q: &[Vec<f64>],
        selected: Option<&Selection>,
        kernels: &dyn graphbuff_core::Kernels,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<(Vec<(f64, u64)>, f64)> {
        let Tokens::Flat { rows, owner, .. } = &self.tokens else { unreachable!("a flat index") };
        let n = self.ids.len();
        let mut best = vec![f64::NEG_INFINITY; n * q.len()];
        let mut terms = 0;
        for (i, a) in q.iter().enumerate() {
            interrupt()?;
            let s = kernels.vector_scores(rows, a, Similarity::Cosine);
            terms = terms.max(s.f32_terms);
            for (r, v) in s.values.into_iter().enumerate() {
                let o = owner[r];
                if o != NONE {
                    let b = &mut best[o as usize * q.len() + i];
                    *b = b.max(v);
                }
            }
        }
        let scores = (0..n as u32)
            .filter(|&slot| self.ids[slot as usize] != FREE && selected.is_none_or(|s| s.contains(slot)))
            .map(|slot| {
                let row = &best[slot as usize * q.len()..(slot as usize + 1) * q.len()];
                (row.iter().sum::<f64>() / q.len() as f64, self.ids[slot as usize])
            })
            .collect();
        Ok((scores, self.tolerance(q, terms)))
    }

    /// Approximate scores of `slots`, on the calling thread.
    fn score_slots(
        &self,
        q: &[Vec<f64>],
        slots: impl Iterator<Item = u32>,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<(Vec<(f64, u64)>, f64)> {
        let mut out = Vec::new();
        for (n, slot) in slots.enumerate() {
            if n % 64 == 63 {
                interrupt()?;
            }
            if self.ids[slot as usize] != FREE {
                out.push((self.approx(slot, q), self.ids[slot as usize]));
            }
        }
        Ok((out, self.tolerance(q, 0)))
    }

    /// The slots of the vectors the graph finds nearest to each query vector (`width` of
    /// each), among those in `selected` if given.
    fn candidates(
        &self,
        q: &[Vec<f64>],
        width: usize,
        selected: Option<&Selection>,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<Vec<u32>> {
        let Tokens::Graph { graph, owner, zero, .. } = &self.tokens else { unreachable!("an approximate index") };
        let mut found = HashSet::new();
        for a in q {
            let norm = a.iter().map(|x| x * x).sum::<f64>().sqrt();
            if norm == 0.0 {
                continue;
            }
            let unit: Vec<f32> = a.iter().map(|x| (x / norm) as f32).collect();
            let accept = |node: u32| selected.is_none_or(|s| s.contains(owner[node as usize]));
            let near = graph.search_where(&unit, width, width, accept, Bounds { budget: usize::MAX, interrupt })?;
            found.extend(near.unwrap_or_default().into_iter().map(|(_, node)| owner[node as usize]));
        }
        // An entity with a zero vector scores at least 0 for every query vector.
        found.extend(
            zero.iter()
                .enumerate()
                .filter(|(slot, z)| **z && selected.is_none_or(|s| s.contains(*slot as u32)))
                .map(|(slot, _)| slot as u32),
        );
        Ok(found.into_iter().collect())
    }
}

/// Flat tokens of `n` rows of `d` in use, plus room (an eighth more, at least 16), all
/// free.
fn flat_rows(d: usize, n: usize) -> Tokens {
    let capacity = n + (n / 8).max(16);
    Tokens::Flat {
        rows: VectorRows::new(d, vec![0.0; capacity * d]),
        owner: vec![NONE; capacity],
        free: (0..capacity as u32).rev().collect(),
    }
}

/// [`nearest`](crate::vector::nearest) of a multi-vector index: the `k` entities with the
/// highest `vector.maxSim(q, …)`. Exact, except for an approximate index on a committed
/// state: the entities of the [`search_width`]`(k)` vectors its graph finds nearest to
/// each query vector are scored, so one whose vectors are each a little less near can be
/// missed.
pub(crate) fn nearest(
    ctx: &ProcedureContext<'_>,
    def: &IndexDef,
    q: &[Vec<f64>],
    k: usize,
    filter: &[Predicate],
) -> Result<Nearest> {
    let interrupt = || ctx.check_interrupt();
    let store = ctx.store();
    let d = dimensions(def);
    let Some((cache, snapshot)) = ctx.vectors() else {
        // Inside a transaction (or writing): every entity, as the query sees it.
        let mut out = Vec::new();
        for_each_entity(store, def, &interrupt, |id, value| {
            if let Some(score) = exact(q, &value, d) {
                let values = if filter.is_empty() { Vec::new() } else { read_filters(store, def, id)? };
                if filter.iter().all(|p| p.passes(&values[p.column])) {
                    out.push((score, id));
                }
            }
            Ok(())
        })?;
        return Ok((best_k(out, k), Some(Method::Store)));
    };
    let index = cache.get(snapshot, def, &interrupt)?;
    let VectorIndex::Multi(ix) = &*index else { unreachable!("a multi-vector index") };
    let selected = match filter {
        [] => None,
        filter => Some(Selection::new(&ix.filters, filter, &interrupt)?),
    };
    let few = |s: &Selection| {
        let vectors: usize = s.iter().map(|slot| ix.vectors_of[slot as usize].len()).sum();
        vectors <= SUBSET_ROWS.max(ix.live_vectors() / SUBSET_SHARE)
    };
    let ((approx, tolerance), method) = match (&ix.tokens, &selected) {
        (_, Some(s)) if few(s) => (ix.score_slots(q, s.iter(), &interrupt)?, Method::Matches),
        (Tokens::Flat { .. }, s) => (ix.score_all(q, s.as_ref(), &*ctx.kernels(), &interrupt)?, Method::All),
        (Tokens::Graph { .. }, s) => {
            let slots = ix.candidates(q, search_width(k), s.as_ref(), &interrupt)?;
            (ix.score_slots(q, slots.into_iter(), &interrupt)?, Method::Graph)
        }
    };
    // The exact scores of those near the cut, from the stored vectors.
    let mut best = Vec::new();
    for (n, id) in near_the_cut(&approx, tolerance, k).enumerate() {
        if n % 64 == 63 {
            interrupt()?;
        }
        if let Some(score) = property(store, def, id, &def.properties[0])?.and_then(|v| exact(q, &v, d)) {
            best.push((score, id));
        }
    }
    Ok((best_k(best, k), Some(method)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(db: &crate::Database, name: &str) -> MultiIndex {
        match &*db.vector_cache.entry(name).expect("built") {
            VectorIndex::Multi(m) => m.clone(),
            _ => panic!("a multi-vector index"),
        }
    }

    #[test]
    fn rows_follow_commits() {
        for hnsw in [false, true] {
            let db = crate::Database::in_memory();
            db.execute(&format!(
                "CREATE VECTOR INDEX m FOR (d:D) ON d.v OPTIONS {{indexConfig: {{`vector.type`: 'multi', \
                 `vector.dimensions`: 2, `vector.hnsw.enabled`: {hnsw}}}}}"
            ))
            .unwrap();
            db.execute("UNWIND range(1, 100) AS i CREATE (:D {k: i, v: [1, 0, 0, i]})").unwrap();
            let q = "CALL db.index.vector.queryNodes('m', 2, [[0, 1]]) YIELD node RETURN node.k";
            let top = |db: &crate::Database| {
                db.execute(q).unwrap().rows.iter().map(|r| r[0].to_string()).collect::<Vec<_>>()
            };
            assert_eq!(top(&db), ["100", "99"]);
            assert_eq!(index(&db, "m").live_vectors(), 200);
            // Vectors replaced, added and gone.
            db.execute("MATCH (d:D) WHERE d.k <= 40 SET d.v = d.v + [0, 1000 + d.k]").unwrap();
            db.execute("MATCH (d:D) WHERE d.k > 90 DETACH DELETE d").unwrap();
            assert_eq!(top(&db), ["40", "39"]);
            let ix = index(&db, "m");
            assert_eq!((ix.slot_of.len(), ix.live_vectors()), (90, 220));
            // Most entities changed: built again.
            db.execute("MATCH (d:D) WHERE d.k > 40 SET d.v = [0, -1]").unwrap();
            assert_eq!(top(&db), ["40", "39"]);
            let ix = index(&db, "m");
            assert_eq!((ix.dead, ix.slot_of.len(), ix.live_vectors()), (0, 90, 170));
            // Values the index skips.
            db.execute("CREATE (:D {k: 0, v: [1, 2, 3]}), (:D {k: -1, v: 'x'}), (:D {k: -2, v: []})").unwrap();
            assert_eq!(top(&db), ["40", "39"]);
            assert_eq!(index(&db, "m").slot_of.len(), 90);
        }
    }
}
