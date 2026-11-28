//! Sparse vector indexes: lexical weights (bge-m3's sparse output, `vector.embedSparse`),
//! stored as a list of tokens and a list of their weights, found through an inverted index
//! (a list of entities per token) and scored as `vector.sparseScore` scores them.

use std::collections::{BTreeMap, HashMap};

use graphbuff_core::{Changes, Error, GraphStore, IndexDef, IndexEntity, Result, Value};

use crate::filter_column::{FilterColumn, Predicate, Selection};
use crate::procedures::ProcedureContext;
use crate::vector::{
    FREE, Method, Nearest, VectorIndex, best_k, covered_value, filter_columns, filters_at, for_each_entity, near_the_cut,
    property, read_filters, sparse_dot,
};

/// The weights of a query `value` (`None` for null): a map of token to finite number.
pub(crate) fn query_weights(what: &str, value: &Value) -> Result<Option<BTreeMap<String, f64>>> {
    match value {
        Value::Null => Ok(None),
        Value::Map(m) => m
            .iter()
            .map(|(k, x)| match x {
                Value::Int(i) => Ok((k.clone(), *i as f64)),
                Value::Float(f) if f.is_finite() => Ok((k.clone(), *f)),
                o => Err(Error::Type(format!("{what}: token weights must be finite numbers, not {}", o.type_name()))),
            })
            .collect::<Result<_>>()
            .map(Some),
        o => Err(Error::Type(format!(
            "{what}: a sparse vector index is searched with a MAP of token to weight (as vector.embedSparse makes it), not {}",
            o.type_name()
        ))),
    }
}

/// The weights stored as `tokens` and `weights`, as `vector.sparseScore(q, tokens, weights)`
/// reads them (a token repeated takes its last weight); `None` if they are not two lists
/// of the same length, of strings and of finite numbers.
fn stored_weights(tokens: &Value, weights: &Value) -> Option<BTreeMap<String, f64>> {
    let (Value::List(t), Value::List(w)) = (tokens, weights) else { return None };
    if t.len() != w.len() {
        return None;
    }
    t.iter()
        .zip(w)
        .map(|(t, w)| match (t, w) {
            (Value::String(t), Value::Int(i)) => Some((t.clone(), *i as f64)),
            (Value::String(t), Value::Float(f)) if f.is_finite() => Some((t.clone(), *f)),
            _ => None,
        })
        .collect()
}

/// Whether `a` and `b` share a token that neither weights 0: the entities a search finds.
fn shares_token(a: &BTreeMap<String, f64>, b: &BTreeMap<String, f64>) -> bool {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    small.iter().any(|(t, x)| *x != 0.0 && large.get(t).is_some_and(|y| *y != 0.0))
}

/// The stored weights of entity `id` (whose tokens property is `tokens`), if it is indexed.
fn entity_weights(store: &dyn GraphStore, def: &IndexDef, id: u64, tokens: &Value) -> Result<Option<BTreeMap<String, f64>>> {
    let Some(weights) = property(store, def, id, &def.properties[1])? else { return Ok(None) };
    Ok(stored_weights(tokens, &weights).filter(|w| w.values().any(|x| *x != 0.0)))
}

/// The inverted index of a sparse vector index in one committed state. Each indexed entity
/// has a slot; a changed entity's slot is given up and it takes a new one, and once given
/// up slots outnumber the others the slots are numbered anew.
#[derive(Clone)]
pub(crate) struct SparseIndex {
    def: IndexDef,
    /// The entity of each slot; [`FREE`] for one given up.
    ids: Vec<u64>,
    slot_of: HashMap<u64, u32>,
    /// The number of each token.
    terms: HashMap<String, u32>,
    /// By token: the slots that have it (in order) with its weight there, rounded to f32
    /// (±`f32::MAX` at most).
    postings: Vec<Vec<(u32, f32)>>,
    /// By token: the largest magnitude of its weights (or more), before rounding.
    max_weight: Vec<f64>,
    /// The filter properties' values by slot (`null` for one given up).
    filters: Vec<FilterColumn>,
    /// Slots given up.
    dead: usize,
}

impl SparseIndex {
    pub fn def(&self) -> &IndexDef {
        &self.def
    }

    fn new(def: &IndexDef) -> SparseIndex {
        SparseIndex {
            def: def.clone(),
            ids: Vec::new(),
            slot_of: HashMap::new(),
            terms: HashMap::new(),
            postings: Vec::new(),
            max_weight: Vec::new(),
            filters: filter_columns(def),
            dead: 0,
        }
    }

    pub fn build(store: &dyn GraphStore, def: &IndexDef, interrupt: &dyn Fn() -> Result<()>) -> Result<SparseIndex> {
        let mut ix = SparseIndex::new(def);
        for_each_entity(store, def, interrupt, |id, tokens| {
            if let Some(w) = entity_weights(store, def, id, &tokens)? {
                ix.add(id, &w, &read_filters(store, def, id)?);
            }
            Ok(())
        })?;
        Ok(ix)
    }

    /// Gives entity `id` a new slot with `weights` (those not 0) and filter values `filters`.
    fn add(&mut self, id: u64, weights: &BTreeMap<String, f64>, filters: &[Value]) {
        let slot = self.ids.len() as u32;
        self.ids.push(id);
        self.slot_of.insert(id, slot);
        for (column, value) in self.filters.iter_mut().zip(filters) {
            column.push(value);
        }
        for (token, &w) in weights {
            if w == 0.0 {
                continue;
            }
            let term = match self.terms.get(token) {
                Some(&t) => t,
                None => {
                    let t = self.postings.len() as u32;
                    self.terms.insert(token.clone(), t);
                    self.postings.push(Vec::new());
                    self.max_weight.push(0.0);
                    t
                }
            } as usize;
            self.postings[term].push((slot, w.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32));
            self.max_weight[term] = self.max_weight[term].max(w.abs());
        }
    }

    pub fn apply(&mut self, store: &dyn GraphStore, changes: &Changes, interrupt: &dyn Fn() -> Result<()>) -> Result<()> {
        let changed = match self.def.entity {
            IndexEntity::Node => &changes.nodes,
            IndexEntity::Relationship => &changes.rels,
        };
        for (n, &id) in changed.iter().enumerate() {
            if n % 1024 == 1023 {
                interrupt()?;
            }
            if let Some(slot) = self.slot_of.remove(&id) {
                self.ids[slot as usize] = FREE;
                for column in &mut self.filters {
                    column.set(slot, &Value::Null);
                }
                self.dead += 1;
            }
            let def = &self.def;
            if let Some(tokens) = covered_value(store, def, id, &def.properties[0])?
                && let Some(w) = entity_weights(store, def, id, &tokens)?
            {
                let filters = read_filters(store, def, id)?;
                self.add(id, &w, &filters);
            }
        }
        if self.dead > 64 && self.dead > self.slot_of.len() {
            self.renumber();
        }
        Ok(())
    }

    /// Numbers the slots in use anew, in order, without those given up.
    fn renumber(&mut self) {
        let mut new_slot = vec![u32::MAX; self.ids.len()];
        let mut ids = Vec::with_capacity(self.slot_of.len());
        let mut filters = filter_columns(&self.def);
        for (slot, &id) in self.ids.iter().enumerate() {
            if id != FREE {
                new_slot[slot] = ids.len() as u32;
                ids.push(id);
                for (column, value) in filters.iter_mut().zip(filters_at(&self.filters, slot as u32)) {
                    column.push(&value);
                }
            }
        }
        for list in &mut self.postings {
            list.retain_mut(|(slot, _)| {
                *slot = new_slot[*slot as usize];
                *slot != u32::MAX
            });
        }
        self.slot_of = ids.iter().enumerate().map(|(slot, &id)| (id, slot as u32)).collect();
        self.ids = ids;
        self.filters = filters;
        self.dead = 0;
    }

    /// Approximate scores of the entities (in `selected`, if given) that share a token with
    /// `q`, and a bound on how far one may be from the exact score.
    fn scores(
        &self,
        q: &BTreeMap<String, f64>,
        selected: Option<&Selection>,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<(Vec<(f64, u64)>, f64)> {
        let mut acc: HashMap<u32, f64> = HashMap::new();
        // Rounding a weight to f32 moves it by at most 2^-24 of itself; the f64 sums add
        // far less. Twice that, for margin.
        let (mut bound, mut seen) = (0.0, 0usize);
        for (token, &qw) in q {
            let Some(&term) = self.terms.get(token).filter(|_| qw != 0.0) else { continue };
            let max = self.max_weight[term as usize];
            bound += qw.abs() * if max > f64::from(f32::MAX) { f64::INFINITY } else { max };
            for &(slot, w) in &self.postings[term as usize] {
                seen += 1;
                if seen.is_multiple_of(4096) {
                    interrupt()?;
                }
                if self.ids[slot as usize] == FREE || selected.is_some_and(|s| !s.contains(slot)) {
                    continue;
                }
                *acc.entry(slot).or_insert(0.0) += qw * f64::from(w);
            }
        }
        let tolerance = 2.0 * bound * (2f64.powi(-24) + 2f64.powi(-40));
        Ok((acc.into_iter().map(|(slot, s)| (s, self.ids[slot as usize])).collect(), tolerance))
    }
}

/// [`nearest`](crate::vector::nearest) of a sparse index: the `k` entities that share a
/// token with `q` (neither weight 0) and score highest, as `vector.sparseScore` scores them.
pub(crate) fn nearest(
    ctx: &ProcedureContext<'_>,
    def: &IndexDef,
    q: &BTreeMap<String, f64>,
    k: usize,
    filter: &[Predicate],
) -> Result<Nearest> {
    let interrupt = || ctx.check_interrupt();
    let store = ctx.store();
    let Some((cache, snapshot)) = ctx.vectors() else {
        // Inside a transaction (or writing): every entity, as the query sees it.
        let mut out = Vec::new();
        for_each_entity(store, def, &interrupt, |id, tokens| {
            if let Some(w) = entity_weights(store, def, id, &tokens)?
                && shares_token(q, &w)
            {
                let values = if filter.is_empty() { Vec::new() } else { read_filters(store, def, id)? };
                if filter.iter().all(|p| p.passes(&values[p.column])) {
                    out.push((sparse_dot(q, &w), id));
                }
            }
            Ok(())
        })?;
        return Ok((best_k(out, k), Some(Method::Store)));
    };
    let index = cache.get(snapshot, def, &interrupt)?;
    let VectorIndex::Sparse(ix) = &*index else { unreachable!("a sparse index") };
    let selected = match filter {
        [] => None,
        filter => Some(Selection::new(&ix.filters, filter, &interrupt)?),
    };
    let (approx, tolerance) = ix.scores(q, selected.as_ref(), &interrupt)?;
    // The exact scores of those near the cut, from the stored weights.
    let mut best = Vec::new();
    for (n, id) in near_the_cut(&approx, tolerance, k).enumerate() {
        if n % 256 == 255 {
            interrupt()?;
        }
        if let Some(tokens) = property(store, def, id, &def.properties[0])?
            && let Some(w) = entity_weights(store, def, id, &tokens)?
        {
            best.push((sparse_dot(q, &w), id));
        }
    }
    Ok((best_k(best, k), Some(Method::Postings)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(db: &crate::Database, name: &str) -> SparseIndex {
        match &*db.vector_cache.entry(name).expect("built") {
            VectorIndex::Sparse(s) => s.clone(),
            _ => panic!("a sparse index"),
        }
    }

    #[test]
    fn slots_follow_commits() {
        let db = crate::Database::in_memory();
        db.execute("CREATE VECTOR INDEX s FOR (d:D) ON (d.t, d.w) WITH [d.g] OPTIONS {indexConfig: {`vector.type`: 'sparse'}}")
            .unwrap();
        db.execute("UNWIND range(1, 200) AS i CREATE (:D {k: i, t: ['a', 'b' + (i % 7)], w: [0.5, i], g: i % 3})").unwrap();
        let q = "CALL db.index.vector.queryNodes('s', 3, {a: 1.0}) YIELD node RETURN node.k";
        db.execute(q).unwrap();
        let ix = index(&db, "s");
        assert_eq!((ix.ids.len(), ix.dead, ix.terms.len()), (200, 0, 8));
        // Changed and removed entities give up their slots; past half, numbered anew.
        db.execute("MATCH (d:D) WHERE d.k <= 50 SET d.w = [1, 2]").unwrap();
        db.execute("MATCH (d:D) WHERE d.k > 190 DETACH DELETE d").unwrap();
        db.execute(q).unwrap();
        let ix = index(&db, "s");
        assert_eq!((ix.ids.len(), ix.dead, ix.slot_of.len()), (250, 60, 190));
        db.execute("MATCH (d:D) WHERE d.k > 50 SET d.t = ['x', 'y'], d.w = [1, 0]").unwrap();
        db.execute(q).unwrap();
        let ix = index(&db, "s");
        assert_eq!((ix.ids.len(), ix.dead, ix.slot_of.len()), (190, 0, 190));
        assert!(ix.postings.iter().flatten().all(|(slot, _)| (*slot as usize) < ix.ids.len()));
        let g: Vec<String> = (0..190).map(|s| filters_at(&ix.filters, s)[0].to_string()).collect();
        let want: Vec<String> = ix.ids.iter().map(|id| {
            let r = db.execute(&format!("MATCH (d) WHERE id(d) = {id} RETURN d.g")).unwrap();
            r.rows[0][0].to_string()
        }).collect();
        assert_eq!(g, want);
        // Weights 0 are not kept, and an entity of only those is not indexed.
        assert!(!ix.terms.contains_key("y"));
        db.execute("CREATE (:D {k: 0, t: ['z'], w: [0]})").unwrap();
        db.execute(q).unwrap();
        assert_eq!(index(&db, "s").slot_of.len(), 190);
    }
}
