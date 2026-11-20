//! Stage-1 storage: a fully in-memory property graph.
//!
//! Layout:
//! * nodes and relationships live in slab vectors indexed by id (ids are never reused),
//! * each node keeps its relationships grouped by type, with outgoing and incoming
//!   lists per group; each entry holds the relationship id and the node on the other end,
//!   so an expansion never looks up the relationship record (see [`TypeGroup`]),
//! * labels, relationship types and property keys are interned to `u32` tokens,
//! * a label index maps each label to the sorted set of nodes carrying it,
//! * optional property indexes map `(label, key, value)` to the nodes carrying them.
//!
//! All of it is kept in copy-on-write containers ([`cow`]), so cloning the graph is
//! cheap and shares everything: [`GraphStore::snapshot`] hands out such clones to readers,
//! and a transaction keeps one of the state at `begin` to roll back to.

mod cow;
mod snapshot;

pub(crate) use snapshot::SNAPSHOT_VERSION;

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use std::cmp::Ordering;
use std::ops::Bound;

use crate::store::{
    Adjacency, ChangeLog, Changes, ConstraintDef, Direction, Expansion, GraphStore, IndexDef, IndexEntity, IndexStats, Scan,
    Snapshot, ValueRange,
};
use cow::{CowVec, PMap, PSet, PVec};
use crate::{Error, NodeId, RelId, Result, Value};

pub(crate) type Token = u32;
/// The properties of a record: a small map sorted by key token. Records have few
/// properties, so a vector beats a hash map in size, lookups and above all copying,
/// which every first change of a record after a snapshot does.
#[derive(Debug, Clone, Default)]
pub(crate) struct Props(CowVec<(Token, Value)>);

impl Props {
    pub(crate) fn with_capacity(n: usize) -> Self {
        Props(Vec::with_capacity(n).into())
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    fn find(&self, k: Token) -> std::result::Result<usize, usize> {
        self.0.binary_search_by_key(&k, |(t, _)| *t)
    }

    pub(crate) fn get(&self, k: &Token) -> Option<&Value> {
        self.find(*k).ok().map(|i| &self.0[i].1)
    }

    pub(crate) fn insert(&mut self, k: Token, v: Value) -> Option<Value> {
        match self.find(k) {
            Ok(i) => Some(std::mem::replace(&mut self.0.make_mut()[i].1, v)),
            Err(i) => {
                self.0.make_mut().insert(i, (k, v));
                None
            }
        }
    }

    pub(crate) fn remove(&mut self, k: &Token) -> Option<Value> {
        let i = self.find(*k).ok()?;
        Some(self.0.make_mut().remove(i).1)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Token, &Value)> {
        self.0.iter().map(|(k, v)| (k, v))
    }
}

impl<'a> IntoIterator for &'a Props {
    type Item = (&'a Token, &'a Value);
    type IntoIter = std::iter::Map<std::slice::Iter<'a, (Token, Value)>, fn(&'a (Token, Value)) -> (&'a Token, &'a Value)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|(k, v)| (k, v))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Interner {
    pub(crate) names: Vec<String>,
    ids: HashMap<String, Token>,
}

impl Interner {
    pub(crate) fn get(&self, name: &str) -> Option<Token> {
        self.ids.get(name).copied()
    }

    pub(crate) fn intern(&mut self, name: &str) -> Token {
        if let Some(&t) = self.ids.get(name) {
            return t;
        }
        let t = self.names.len() as Token;
        self.names.push(name.to_string());
        self.ids.insert(name.to_string(), t);
        t
    }

    pub(crate) fn name(&self, t: Token) -> &str {
        &self.names[t as usize]
    }
}

/// Interns `name` in a shared interner, copying it only for a new name.
fn intern(interner: &mut Arc<Interner>, name: &str) -> Token {
    match interner.get(name) {
        Some(t) => t,
        None => Arc::make_mut(interner).intern(name),
    }
}

/// A node. Each part is shared copy-on-write, so copying a record (as the first change
/// after a snapshot does) costs three reference counts, and a change copies only the
/// part it changes.
#[derive(Debug, Clone, Default)]
struct NodeRecord {
    labels: CowVec<Token>,
    props: Props,
    /// Relationships grouped by type, sorted by type token. Groups are never empty.
    adj: CowVec<TypeGroup>,
}

/// One relationship seen from one of its endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AdjEntry {
    rel: RelId,
    other: NodeId,
}

/// Marks a removed entry; `other` of a live entry is a real node id.
const DEAD: NodeId = NodeId(u64::MAX);

/// A list of adjacency entries sorted by relationship id, copy-on-write so that a snapshot
/// of a node with a million relationships costs nothing and a later write to it copies
/// only a few small blocks.
///
/// Relationship ids only grow, so a new relationship is appended. Removal leaves a
/// tombstone (found by binary search) and the list is compacted once at least half of it
/// is dead, which keeps deleting all relationships of a node with a million of them
/// linear instead of quadratic, in any order.
#[derive(Debug, Clone, Default)]
struct AdjList {
    entries: PVec<AdjEntry>,
    dead: usize,
}

impl AdjList {
    fn len(&self) -> usize {
        self.entries.len() - self.dead
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn iter(&self) -> impl Iterator<Item = AdjEntry> + '_ {
        self.entries.iter().copied().filter(|e| e.other != DEAD)
    }

    /// Position of `rel` if it has an entry (live or dead).
    fn find(&self, rel: RelId) -> Option<usize> {
        let i = self.entries.partition_point(|x| x.rel < rel);
        self.entries.get(i).filter(|x| x.rel == rel).map(|_| i)
    }

    fn insert(&mut self, e: AdjEntry) {
        if self.entries.last().is_none_or(|l| l.rel < e.rel) {
            self.entries.push(e);
            return;
        }
        if let Some(i) = self.find(e.rel) {
            let slot = self.entries.get_mut(i).expect("found");
            debug_assert_eq!(slot.other, DEAD, "relationship linked twice");
            if slot.other == DEAD {
                self.dead -= 1;
            }
            slot.other = e.other;
            return;
        }
        // Not reached by the store (relationship ids only grow); kept for completeness.
        let mut all: Vec<AdjEntry> = self.entries.iter().copied().collect();
        let i = all.partition_point(|x| x.rel < e.rel);
        all.insert(i, e);
        self.entries = all.into_iter().collect();
    }

    fn remove(&mut self, rel: RelId) {
        // The newest entry (as when deleting newest first) needs no search or tombstone.
        if self.entries.last().is_some_and(|l| l.rel == rel) {
            self.entries.pop();
            while self.entries.last().is_some_and(|l| l.other == DEAD) {
                self.entries.pop();
                self.dead -= 1;
            }
            return;
        }
        let Some(i) = self.find(rel) else {
            return;
        };
        let slot = self.entries.get_mut(i).expect("found");
        if slot.other == DEAD {
            return;
        }
        slot.other = DEAD;
        self.dead += 1;
        // Trailing tombstones can go right away; the rest once they are the majority.
        while self.entries.last().is_some_and(|l| l.other == DEAD) {
            self.entries.pop();
            self.dead -= 1;
        }
        if self.dead > 16 && self.dead * 2 > self.entries.len() {
            self.entries = self.iter().collect();
            self.dead = 0;
        }
    }
}

/// The relationships of one type attached to a node.
#[derive(Debug, Clone, Default)]
struct TypeGroup {
    rel_type: Token,
    out: AdjList,
    inc: AdjList,
    /// Self-loops among `out` (each also appears in `inc`).
    loops: usize,
}

impl TypeGroup {
    fn is_empty(&self) -> bool {
        self.out.is_empty() && self.inc.is_empty()
    }

    fn degree(&self, dir: Direction) -> usize {
        match dir {
            Direction::Outgoing => self.out.len(),
            Direction::Incoming => self.inc.len(),
            Direction::Both => self.out.len() + self.inc.len() - self.loops,
        }
    }
}

impl NodeRecord {
    fn has_relationships(&self) -> bool {
        !self.adj.is_empty()
    }

    fn group_mut(&mut self, t: Token) -> &mut TypeGroup {
        let i = match self.adj.binary_search_by_key(&t, |g| g.rel_type) {
            Ok(i) => i,
            Err(i) => {
                self.adj.make_mut().insert(
                    i,
                    TypeGroup {
                        rel_type: t,
                        ..TypeGroup::default()
                    },
                );
                i
            }
        };
        &mut self.adj.make_mut()[i]
    }

    fn link(&mut self, t: Token, rel: RelId, other: NodeId, outgoing: bool, is_loop: bool) {
        let g = self.group_mut(t);
        let e = AdjEntry { rel, other };
        if outgoing {
            g.out.insert(e);
            if is_loop {
                g.loops += 1;
            }
        } else {
            g.inc.insert(e);
        }
    }

    fn unlink(&mut self, t: Token, rel: RelId, outgoing: bool, is_loop: bool) {
        let Ok(i) = self.adj.binary_search_by_key(&t, |g| g.rel_type) else {
            return;
        };
        let g = &mut self.adj.make_mut()[i];
        if outgoing {
            g.out.remove(rel);
            if is_loop {
                g.loops -= 1;
            }
        } else {
            g.inc.remove(rel);
        }
        if g.is_empty() {
            self.adj.make_mut().remove(i);
        }
    }

    /// The groups whose type is in `wanted` (`None` = all).
    fn groups<'a>(
        &'a self,
        wanted: &Option<Vec<Token>>,
    ) -> impl Iterator<Item = &'a TypeGroup> {
        self.adj
            .iter()
            .filter(move |g| wanted.as_ref().is_none_or(|t| t.contains(&g.rel_type)))
    }
}

#[derive(Debug, Clone)]
struct RelRecord {
    start: NodeId,
    end: NodeId,
    rel_type: Token,
    props: Props,
}

/// A property index on `(:label).keys` (or on relationships of a type).
#[derive(Debug, Clone)]
struct PropIndex {
    def: IndexDef,
    /// Label token (node index) or relationship type token.
    label: Token,
    keys: Vec<Token>,
    entries: IndexEntries,
}

impl PropIndex {
    fn is_node(&self) -> bool {
        self.def.entity == IndexEntity::Node
    }

    /// The entry key of an entity with `props`: the normalized value (one property) or a
    /// list of them (composite); `None` if a property is missing.
    fn key_of(&self, props: &Props) -> Option<Value> {
        if let [k] = self.keys[..] {
            return props.get(&k).map(index_key);
        }
        let mut values = Vec::with_capacity(self.keys.len());
        for k in &self.keys {
            values.push(index_key(props.get(k)?));
        }
        Some(Value::List(values))
    }
}

/// The entry key of a seek for `values`, normalized as [`PropIndex::key_of`] does; `None`
/// if one is null (nothing matches).
fn seek_key(values: &[Value]) -> Option<Value> {
    if values.iter().any(Value::is_null) {
        return None;
    }
    Some(match values {
        [v] => index_key(v),
        _ => Value::List(values.iter().map(index_key).collect()),
    })
}

/// `f64` under `total_cmp`, so it can be a `BTreeSet` key.
#[derive(Debug, Clone, Copy)]
struct OrdF64(f64);

impl PartialEq for OrdF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0).is_eq()
    }
}

impl Eq for OrdF64 {}

impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Sort key of an indexed number or string, for range seeks. Numbers sort by their value
/// as `f64` (exact ties broken by type and bits), so very large integers may sort next to
/// a neighbour; range seeks include such neighbours and callers re-check.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum OrdKey {
    Num(OrdF64, u8, u64),
    Str(String),
}

fn ord_key(v: &Value) -> Option<OrdKey> {
    match v {
        Value::Int(i) => Some(OrdKey::Num(OrdF64(*i as f64), 0, *i as u64)),
        Value::Float(f) => Some(OrdKey::Num(OrdF64(*f), 1, f.to_bits())),
        Value::String(s) => Some(OrdKey::Str(s.clone())),
        _ => None,
    }
}

fn approx(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => 0.0,
    }
}

/// The entities with one indexed value. Unique values are common, and need no set.
#[derive(Debug, Clone)]
enum Ids {
    One(u64),
    Many(PSet<u64>),
}

impl Ids {
    /// Returns whether `id` was new.
    fn insert(&mut self, id: u64) -> bool {
        match self {
            Ids::One(x) if *x == id => false,
            Ids::One(x) => {
                let mut set = PSet::default();
                set.insert(*x);
                set.insert(id);
                *self = Ids::Many(set);
                true
            }
            Ids::Many(set) => set.insert(id),
        }
    }

    /// Returns whether `id` was present, and whether the entry is now empty.
    fn remove(&mut self, id: u64) -> (bool, bool) {
        match self {
            Ids::One(x) => (*x == id, *x == id),
            Ids::Many(set) => {
                let removed = set.remove(&id);
                (removed, set.is_empty())
            }
        }
    }

    fn to_vec(&self) -> Vec<u64> {
        match self {
            Ids::One(x) => vec![*x],
            Ids::Many(set) => set.iter().copied().collect(),
        }
    }
}

/// The entries of a property index: by normalized value for equality seeks, and numbers
/// and strings in order for range seeks.
#[derive(Debug, Clone, Default)]
pub(crate) struct IndexEntries {
    /// Values by hash; a bucket holds the values sharing a hash.
    eq: PMap<u64, Vec<(Value, Ids)>>,
    ordered: PSet<(OrdKey, u64)>,
    /// Entries (indexed entities).
    count: usize,
    /// Distinct values.
    distinct: usize,
}

fn value_hash(v: &Value) -> u64 {
    // The default hasher with fixed keys: the same in every process.
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

impl IndexEntries {
    /// `v` is already normalized (`index_key`).
    pub(crate) fn insert(&mut self, v: Value, id: u64) {
        if let Some(k) = ord_key(&v) {
            self.ordered.insert((k, id));
        }
        let h = value_hash(&v);
        let Some(bucket) = self.eq.get_mut(&h) else {
            self.eq.insert(h, vec![(v, Ids::One(id))]);
            self.distinct += 1;
            self.count += 1;
            return;
        };
        match bucket.iter().position(|(x, _)| *x == v) {
            Some(i) => {
                if bucket[i].1.insert(id) {
                    self.count += 1;
                }
            }
            None => {
                bucket.push((v, Ids::One(id)));
                self.distinct += 1;
                self.count += 1;
            }
        }
    }

    pub(crate) fn remove(&mut self, v: &Value, id: u64) {
        if let Some(k) = ord_key(v) {
            self.ordered.remove(&(k, id));
        }
        let h = value_hash(v);
        let Some(bucket) = self.eq.get_mut(&h) else {
            return;
        };
        let Some(i) = bucket.iter().position(|(x, _)| x == v) else {
            return;
        };
        let (removed, empty) = bucket[i].1.remove(id);
        if removed {
            self.count -= 1;
        }
        if empty {
            bucket.remove(i);
            self.distinct -= 1;
            if bucket.is_empty() {
                self.eq.remove(&h);
            }
        }
    }

    pub(crate) fn get(&self, v: &Value) -> Vec<u64> {
        self.eq
            .get(&value_hash(v))
            .and_then(|b| b.iter().find(|(x, _)| x == v))
            .map(|(_, ids)| ids.to_vec())
            .unwrap_or_default()
    }

    fn range(&self, range: &ValueRange) -> Vec<u64> {
        type Key = (OrdKey, u64);
        let bounds: (Bound<Key>, Bound<Key>) = match range {
            ValueRange::Prefix(p) => {
                let mut ids: Vec<u64> = self
                    .ordered
                    .range(Bound::Included(&(OrdKey::Str(p.clone()), 0)))
                    .take_while(|(k, _)| matches!(k, OrdKey::Str(s) if s.starts_with(p.as_str())))
                    .map(|(_, id)| *id)
                    .collect();
                ids.sort_unstable();
                return ids;
            }
            ValueRange::Between { lower, upper } => {
                let value = |b: &Bound<Value>| match b {
                    Bound::Included(v) | Bound::Excluded(v) => Some(v.clone()),
                    Bound::Unbounded => None,
                };
                let (lo, hi) = (value(lower), value(upper));
                // Bounds are taken as inclusive: the result may include values equal to an
                // exclusive bound, which callers filter out.
                match range.numeric() {
                    None => return Vec::new(),
                    Some(true) => (
                        lo.map_or(Bound::Unbounded, |v| {
                            Bound::Included((OrdKey::Num(OrdF64(approx(&v)), 0, 0), 0))
                        }),
                        hi.map_or(
                            Bound::Excluded((OrdKey::Str(String::new()), 0)),
                            |v| Bound::Included((OrdKey::Num(OrdF64(approx(&v)), u8::MAX, u64::MAX), u64::MAX)),
                        ),
                    ),
                    Some(false) => {
                        let s = |v: Value| match v {
                            Value::String(s) => s,
                            _ => String::new(),
                        };
                        (
                            Bound::Included((OrdKey::Str(lo.map(s).unwrap_or_default()), 0)),
                            hi.map_or(Bound::Unbounded, |v| {
                                Bound::Included((OrdKey::Str(s(v)), u64::MAX))
                            }),
                        )
                    }
                }
            }
        };
        // An empty range (lower above upper) would make `BTreeSet::range` panic.
        let start = match &bounds.0 {
            Bound::Included(k) | Bound::Excluded(k) => Some(k),
            Bound::Unbounded => None,
        };
        let end = match &bounds.1 {
            Bound::Included(k) | Bound::Excluded(k) => Some(k),
            Bound::Unbounded => None,
        };
        if let (Some(a), Some(b)) = (start, end)
            && (a > b || (a == b && matches!(bounds.1, Bound::Excluded(_))))
        {
            return Vec::new();
        }
        let (lower, upper) = bounds;
        let mut ids: Vec<u64> = self
            .ordered
            .range(lower.as_ref())
            .take_while(|k| match &upper {
                Bound::Included(u) => *k <= u,
                Bound::Excluded(u) => *k < u,
                Bound::Unbounded => true,
            })
            .map(|(_, id)| *id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

/// Normalizes a value so that Cypher-equal values share an index entry (`1` and `1.0`).
pub(crate) fn index_key(v: &Value) -> Value {
    match v {
        Value::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15 => {
            Value::Int(*f as i64)
        }
        Value::List(items) => Value::List(items.iter().map(index_key).collect()),
        // Zoned values at the same instant are equal: index them in UTC.
        Value::Temporal(t) => Value::Temporal(t.canonical()),
        // Points keep no negative zeros (see `Point::new`): Cypher-equal points encode alike.
        other => other.clone(),
    }
}

/// In-memory implementation of [`GraphStore`].
///
/// Cloning is cheap (the clone shares all data, copy-on-write) and gives the committed
/// state: a clone taken inside a transaction has none of its changes.
#[derive(Debug, Default)]
pub struct MemoryGraph {
    nodes: PVec<Option<NodeRecord>>,
    rels: PVec<Option<RelRecord>>,
    node_count: usize,
    rel_count: usize,
    labels: Arc<Interner>,
    rel_types: Arc<Interner>,
    keys: Arc<Interner>,
    label_index: HashMap<Token, PSet<NodeId>>,
    /// Live relationships per type token.
    type_counts: HashMap<Token, usize>,
    indexes: Vec<PropIndex>,
    /// Vector and full-text index definitions (the engine keeps their contents).
    engine_indexes: Vec<IndexDef>,
    constraints: Vec<ConstraintDef>,
    /// The state at `begin`, while a transaction is open; rollback returns to it.
    saved: Option<Box<MemoryGraph>>,
    /// The entities written since [`GraphStore::take_changes`] (not part of a clone).
    changes: ChangeLog,
}

impl Clone for MemoryGraph {
    fn clone(&self) -> Self {
        if let Some(saved) = &self.saved {
            return (**saved).clone();
        }
        MemoryGraph {
            nodes: self.nodes.clone(),
            rels: self.rels.clone(),
            node_count: self.node_count,
            rel_count: self.rel_count,
            labels: self.labels.clone(),
            rel_types: self.rel_types.clone(),
            keys: self.keys.clone(),
            label_index: self.label_index.clone(),
            type_counts: self.type_counts.clone(),
            indexes: self.indexes.clone(),
            engine_indexes: self.engine_indexes.clone(),
            constraints: self.constraints.clone(),
            saved: None,
            changes: ChangeLog::default(),
        }
    }
}

pub(crate) fn missing_node(id: NodeId) -> Error {
    Error::Runtime(format!("node {id} does not exist"))
}

pub(crate) fn missing_rel(id: RelId) -> Error {
    Error::Runtime(format!("relationship {id} does not exist"))
}

pub(crate) fn check_storable(key: &str, v: &Value) -> Result<()> {
    if v.is_storable_property() {
        Ok(())
    } else {
        Err(Error::Type(format!(
            "property values can only be of primitive types or lists of primitives, got {} for key '{key}'",
            v.type_name()
        )))
    }
}

/// A live record, in a slab node made unshared so it can be changed.
fn record_mut<T: Clone>(slab: &mut PVec<Option<T>>, i: u64) -> Option<&mut T> {
    slab.get_mut(i as usize)?.as_mut()
}

/// Adds a relationship to the adjacency of both its endpoints.
fn link_into(nodes: &mut PVec<Option<NodeRecord>>, id: RelId, rec: &RelRecord) {
    let is_loop = rec.start == rec.end;
    if let Some(n) = record_mut(nodes, rec.start.0) {
        n.link(rec.rel_type, id, rec.end, true, is_loop);
    }
    if let Some(n) = record_mut(nodes, rec.end.0) {
        n.link(rec.rel_type, id, rec.start, false, is_loop);
    }
}

impl MemoryGraph {
    pub fn new() -> Self {
        Self::default()
    }

    fn node(&self, id: NodeId) -> Option<&NodeRecord> {
        self.nodes.get(id.0 as usize)?.as_ref()
    }

    fn node_mut(&mut self, id: NodeId) -> Option<&mut NodeRecord> {
        record_mut(&mut self.nodes, id.0)
    }

    fn find_index(&self, entity: IndexEntity, label: &str, properties: &[String]) -> Option<&PropIndex> {
        self.indexes.iter().find(|ix| ix.def.covers(entity, label, properties))
    }

    /// An index for `def`, filled from the current data (names are interned).
    fn build_index(&mut self, def: IndexDef) -> PropIndex {
        let label = match def.entity {
            IndexEntity::Node => intern(&mut self.labels, &def.label),
            IndexEntity::Relationship => intern(&mut self.rel_types, &def.label),
        };
        let keys = def.properties.iter().map(|p| intern(&mut self.keys, p)).collect();
        let mut ix = PropIndex {
            def,
            label,
            keys,
            entries: IndexEntries::default(),
        };
        if ix.is_node() {
            for &id in self.label_index.get(&label).into_iter().flat_map(|s| s.iter()) {
                if let Some(v) = self.node(id).and_then(|n| ix.key_of(&n.props)) {
                    ix.entries.insert(v, id.0);
                }
            }
        } else {
            for (i, r) in self.rels.iter().enumerate() {
                if let Some(r) = r.as_ref().filter(|r| r.rel_type == label)
                    && let Some(v) = ix.key_of(&r.props)
                {
                    ix.entries.insert(v, i as u64);
                }
            }
        }
        ix
    }

    fn rel(&self, id: RelId) -> Option<&RelRecord> {
        self.rels.get(id.0 as usize)?.as_ref()
    }

    fn rel_mut(&mut self, id: RelId) -> Option<&mut RelRecord> {
        record_mut(&mut self.rels, id.0)
    }

    fn intern_props(&mut self, props: BTreeMap<String, Value>) -> Result<Props> {
        let mut out = Props::with_capacity(props.len());
        for (k, v) in props {
            check_storable(&k, &v)?;
            if !v.is_null() {
                out.insert(intern(&mut self.keys, &k), v);
            }
        }
        Ok(out)
    }

    fn export_props(&self, props: &Props) -> BTreeMap<String, Value> {
        props
            .iter()
            .map(|(k, v)| (self.keys.name(*k).to_string(), v.clone()))
            .collect()
    }

    fn unlink_rel(&mut self, id: RelId, rec: &RelRecord) {
        if let Some(c) = self.type_counts.get_mut(&rec.rel_type) {
            *c -= 1;
            if *c == 0 {
                self.type_counts.remove(&rec.rel_type);
            }
        }
        let is_loop = rec.start == rec.end;
        if let Some(n) = self.node_mut(rec.start) {
            n.unlink(rec.rel_type, id, true, is_loop);
        }
        if let Some(n) = self.node_mut(rec.end) {
            n.unlink(rec.rel_type, id, false, is_loop);
        }
    }

    fn link_rel(&mut self, id: RelId, rec: &RelRecord) {
        *self.type_counts.entry(rec.rel_type).or_default() += 1;
        link_into(&mut self.nodes, id, rec);
    }

    /// Resolves an expansion's type filter: `Some(None)` = any type, `None` = none of the
    /// requested types exist, so nothing can match.
    fn type_filter(&self, types: &[String]) -> Option<Option<Vec<Token>>> {
        if types.is_empty() {
            return Some(None);
        }
        let t: Vec<Token> = types.iter().filter_map(|t| self.rel_types.get(t)).collect();
        (!t.is_empty()).then_some(Some(t))
    }

    /// Adds (`insert`) or removes the node's entries in every node index matching the
    /// optional label / key filters, based on the node's current labels and properties.
    fn index_touch(&mut self, id: NodeId, label: Option<Token>, key: Option<Token>, insert: bool) {
        if self.indexes.is_empty() {
            return;
        }
        let Some(node) = self.node(id) else { return };
        let mut changes = Vec::new();
        for (i, ix) in self.indexes.iter().enumerate() {
            if !ix.is_node()
                || label.is_some_and(|l| l != ix.label)
                || key.is_some_and(|k| !ix.keys.contains(&k))
                || !node.labels.contains(&ix.label)
            {
                continue;
            }
            if let Some(v) = ix.key_of(&node.props) {
                changes.push((i, v));
            }
        }
        self.apply_index_changes(changes, id.0, insert);
    }

    /// Like [`index_touch`](Self::index_touch), for the relationship indexes of the
    /// relationship's type.
    fn rel_index_touch(&mut self, id: RelId, key: Option<Token>, insert: bool) {
        if self.indexes.is_empty() {
            return;
        }
        let Some(rel) = self.rel(id) else { return };
        let mut changes = Vec::new();
        for (i, ix) in self.indexes.iter().enumerate() {
            if ix.is_node() || ix.label != rel.rel_type || key.is_some_and(|k| !ix.keys.contains(&k)) {
                continue;
            }
            if let Some(v) = ix.key_of(&rel.props) {
                changes.push((i, v));
            }
        }
        self.apply_index_changes(changes, id.0, insert);
    }

    fn apply_index_changes(&mut self, changes: Vec<(usize, Value)>, id: u64, insert: bool) {
        for (i, v) in changes {
            let entries = &mut self.indexes[i].entries;
            if insert {
                entries.insert(v, id);
            } else {
                entries.remove(&v, id);
            }
        }
    }
}

impl GraphStore for MemoryGraph {
    fn create_node(&mut self, labels: &[String], props: BTreeMap<String, Value>) -> Result<NodeId> {
        let props = self.intern_props(props)?;
        let mut tokens: Vec<Token> = Vec::with_capacity(labels.len());
        for l in labels {
            let t = intern(&mut self.labels, l);
            if !tokens.contains(&t) {
                tokens.push(t);
            }
        }
        let id = NodeId(self.nodes.len() as u64);
        for &t in &tokens {
            self.label_index.entry(t).or_default().insert(id);
        }
        self.nodes.push(Some(NodeRecord {
            labels: tokens.into(),
            props,
            ..Default::default()
        }));
        self.node_count += 1;
        self.index_touch(id, None, None, true);
        self.changes.node(id);
        Ok(id)
    }

    fn create_relationship(
        &mut self,
        start: NodeId,
        end: NodeId,
        rel_type: &str,
        props: BTreeMap<String, Value>,
    ) -> Result<RelId> {
        if self.node(start).is_none() {
            return Err(missing_node(start));
        }
        if self.node(end).is_none() {
            return Err(missing_node(end));
        }
        let props = self.intern_props(props)?;
        let rel_type = intern(&mut self.rel_types, rel_type);
        let id = RelId(self.rels.len() as u64);
        let rec = RelRecord {
            start,
            end,
            rel_type,
            props,
        };
        self.link_rel(id, &rec);
        self.rels.push(Some(rec));
        self.rel_count += 1;
        self.rel_index_touch(id, None, true);
        self.changes.rel(id);
        Ok(id)
    }

    fn delete_node(&mut self, id: NodeId, detach: bool) -> Result<bool> {
        let Some(node) = self.node(id) else {
            return Ok(false);
        };
        if node.has_relationships() {
            if !detach {
                return Err(Error::Constraint(format!(
                    "cannot delete node {id} because it still has relationships; delete them first or use DETACH DELETE"
                )));
            }
            let mut attached: Vec<RelId> = node
                .adj
                .iter()
                .flat_map(|g| g.out.iter().chain(g.inc.iter()))
                .map(|e| e.rel)
                .collect();
            // Newest first: each removal then takes the tail of this node's lists, and a
            // rollback (which replays the log backwards) re-appends them in order.
            attached.sort_unstable_by(|a, b| b.cmp(a));
            attached.dedup();
            for r in attached {
                self.delete_relationship(r)?;
            }
        }
        self.index_touch(id, None, None, false);
        let rec = self
            .nodes
            .get_mut(id.0 as usize)
            .and_then(Option::take)
            .expect("node checked above");
        for l in rec.labels.iter() {
            if let Some(set) = self.label_index.get_mut(l) {
                set.remove(&id);
            }
        }
        self.node_count -= 1;
        self.changes.node(id);
        Ok(true)
    }

    fn delete_relationship(&mut self, id: RelId) -> Result<bool> {
        self.rel_index_touch(id, None, false);
        let Some(rec) = self.rels.get_mut(id.0 as usize).and_then(Option::take) else {
            return Ok(false);
        };
        self.unlink_rel(id, &rec);
        self.rel_count -= 1;
        self.changes.rel(id);
        Ok(true)
    }

    fn set_node_property(&mut self, id: NodeId, key: &str, value: Value) -> Result<()> {
        check_storable(key, &value)?;
        if self.node(id).is_none() {
            return Err(missing_node(id));
        }
        let k = intern(&mut self.keys, key);
        self.index_touch(id, None, Some(k), false);
        let node = self.node_mut(id).expect("checked above");
        if value.is_null() {
            node.props.remove(&k);
        } else {
            node.props.insert(k, value);
        }
        self.index_touch(id, None, Some(k), true);
        self.changes.node(id);
        Ok(())
    }

    fn set_relationship_property(&mut self, id: RelId, key: &str, value: Value) -> Result<()> {
        check_storable(key, &value)?;
        if self.rel(id).is_none() {
            return Err(missing_rel(id));
        }
        let k = intern(&mut self.keys, key);
        self.rel_index_touch(id, Some(k), false);
        let rel = self.rel_mut(id).expect("checked above");
        if value.is_null() {
            rel.props.remove(&k);
        } else {
            rel.props.insert(k, value);
        }
        self.rel_index_touch(id, Some(k), true);
        self.changes.rel(id);
        Ok(())
    }

    fn add_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        if self.node(id).is_none() {
            return Err(missing_node(id));
        }
        let t = intern(&mut self.labels, label);
        let node = self.node_mut(id).expect("checked above");
        if node.labels.contains(&t) {
            return Ok(false);
        }
        node.labels.make_mut().push(t);
        self.label_index.entry(t).or_default().insert(id);
        self.index_touch(id, Some(t), None, true);
        self.changes.node(id);
        Ok(true)
    }

    fn remove_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        let Some(node) = self.node(id) else {
            return Err(missing_node(id));
        };
        let Some(t) = self.labels.get(label) else {
            return Ok(false);
        };
        if !node.labels.contains(&t) {
            return Ok(false);
        }
        self.index_touch(id, Some(t), None, false);
        let node = self.node_mut(id).expect("checked above");
        node.labels.make_mut().retain(|l| *l != t);
        if let Some(set) = self.label_index.get_mut(&t) {
            set.remove(&id);
        }
        self.changes.node(id);
        Ok(true)
    }

    fn node_exists(&self, id: NodeId) -> Result<bool> {
        Ok(self.node(id).is_some())
    }

    fn relationship_exists(&self, id: RelId) -> Result<bool> {
        Ok(self.rel(id).is_some())
    }

    fn node_labels(&self, id: NodeId) -> Result<Vec<String>> {
        Ok(self
            .node(id)
            .map(|n| {
                n.labels
                    .iter()
                    .map(|l| self.labels.name(*l).to_string())
                    .collect()
            })
            .unwrap_or_default())
    }

    fn node_has_label(&self, id: NodeId, label: &str) -> Result<bool> {
        Ok(match (self.labels.get(label), self.node(id)) {
            (Some(t), Some(n)) => n.labels.contains(&t),
            _ => false,
        })
    }

    fn node_property(&self, id: NodeId, key: &str) -> Result<Option<Value>> {
        Ok(self
            .keys
            .get(key)
            .and_then(|k| self.node(id)?.props.get(&k).cloned()))
    }

    fn node_properties(&self, id: NodeId) -> Result<BTreeMap<String, Value>> {
        Ok(self
            .node(id)
            .map(|n| self.export_props(&n.props))
            .unwrap_or_default())
    }

    fn relationship_type(&self, id: RelId) -> Result<Option<String>> {
        Ok(self
            .rel(id)
            .map(|r| self.rel_types.name(r.rel_type).to_string()))
    }

    fn relationship_endpoints(&self, id: RelId) -> Result<Option<(NodeId, NodeId)>> {
        Ok(self.rel(id).map(|r| (r.start, r.end)))
    }

    fn relationship_property(&self, id: RelId, key: &str) -> Result<Option<Value>> {
        Ok(self
            .keys
            .get(key)
            .and_then(|k| self.rel(id)?.props.get(&k).cloned()))
    }

    fn relationship_properties(&self, id: RelId) -> Result<BTreeMap<String, Value>> {
        Ok(self
            .rel(id)
            .map(|r| self.export_props(&r.props))
            .unwrap_or_default())
    }

    fn all_nodes(&self) -> Scan<'_, NodeId> {
        Box::new(
            self.nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.is_some())
                .map(|(i, _)| Ok(NodeId(i as u64))),
        )
    }

    fn nodes_with_label(&self, label: &str) -> Scan<'_, NodeId> {
        match self.labels.get(label).and_then(|t| self.label_index.get(&t)) {
            Some(set) => Box::new(set.iter().copied().map(Ok)),
            None => Box::new(std::iter::empty()),
        }
    }

    fn label_count(&self, label: &str) -> usize {
        self.labels
            .get(label)
            .and_then(|t| self.label_index.get(&t))
            .map_or(0, PSet::len)
    }

    fn all_relationships(&self) -> Scan<'_, RelId> {
        Box::new(
            self.rels
                .iter()
                .enumerate()
                .filter(|(_, r)| r.is_some())
                .map(|(i, _)| Ok(RelId(i as u64))),
        )
    }

    fn expand(&self, node: NodeId, dir: Direction, types: &[String]) -> Adjacency<'_> {
        let (Some(n), Some(wanted)) = (self.node(node), self.type_filter(types)) else {
            return Box::new(std::iter::empty());
        };
        // Outgoing entries of every matching group, then incoming ones; within a group in
        // creation order. A node has few groups, so collecting them is cheap.
        let groups: Vec<&TypeGroup> = n.groups(&wanted).collect();
        let out = matches!(dir, Direction::Outgoing | Direction::Both).then(|| groups.clone());
        let inc = matches!(dir, Direction::Incoming | Direction::Both).then_some(groups);
        let both = dir == Direction::Both;
        let out = out.into_iter().flatten().flat_map(|g| g.out.iter());
        let inc = inc
            .into_iter()
            .flatten()
            .flat_map(|g| g.inc.iter())
            // With Both, a self-loop was already reported as outgoing.
            .filter(move |e| !(both && e.other == node));
        Box::new(out.chain(inc).map(|e| {
            Ok(Expansion {
                rel: e.rel,
                other: e.other,
            })
        }))
    }

    fn degree(&self, node: NodeId, dir: Direction, types: &[String]) -> Result<usize> {
        let (Some(n), Some(wanted)) = (self.node(node), self.type_filter(types)) else {
            return Ok(0);
        };
        Ok(n.groups(&wanted).map(|g| g.degree(dir)).sum())
    }

    fn node_count(&self) -> usize {
        self.node_count
    }

    fn relationship_count(&self) -> usize {
        self.rel_count
    }

    fn labels(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .label_index
            .iter()
            .filter(|(_, set)| !set.is_empty())
            .map(|(t, _)| self.labels.name(*t).to_string())
            .collect();
        out.sort();
        out
    }

    fn relationship_types(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .type_counts
            .keys()
            .map(|t| self.rel_types.name(*t).to_string())
            .collect();
        out.sort();
        out
    }

    fn relationship_type_count(&self, rel_type: &str) -> usize {
        self.rel_types
            .get(rel_type)
            .and_then(|t| self.type_counts.get(&t))
            .copied()
            .unwrap_or(0)
    }

    fn schema_index_stats(&self, entity: IndexEntity, label: &str, properties: &[String]) -> Option<IndexStats> {
        let ix = self.find_index(entity, label, properties)?;
        Some(IndexStats {
            entries: ix.entries.count,
            distinct: ix.entries.distinct,
        })
    }

    fn property_keys(&self) -> Vec<String> {
        let mut out = self.keys.names.clone();
        out.sort();
        out
    }

    fn create_index(&mut self, def: IndexDef) -> Result<()> {
        def.check_new(&self.indexes())?;
        if def.is_vector() || def.is_fulltext() {
            self.engine_indexes.push(def);
            return Ok(());
        }
        let ix = self.build_index(def);
        self.indexes.push(ix);
        Ok(())
    }

    fn drop_index(&mut self, name: &str) -> Result<bool> {
        if let Some(pos) = self.engine_indexes.iter().position(|d| d.name == name) {
            self.engine_indexes.remove(pos);
            return Ok(true);
        }
        let Some(pos) = self.indexes.iter().position(|ix| ix.def.name == name) else {
            return Ok(false);
        };
        self.indexes.remove(pos);
        Ok(true)
    }

    fn indexes(&self) -> Vec<IndexDef> {
        let mut out: Vec<IndexDef> = self.indexes.iter().map(|ix| ix.def.clone()).collect();
        out.extend(self.engine_indexes.iter().cloned());
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn create_constraint(&mut self, def: ConstraintDef) -> Result<()> {
        def.check_new(&self.constraints)?;
        self.constraints.push(def);
        Ok(())
    }

    fn drop_constraint(&mut self, name: &str) -> Result<bool> {
        let Some(pos) = self.constraints.iter().position(|c| c.name == name) else {
            return Ok(false);
        };
        self.constraints.remove(pos);
        Ok(true)
    }

    fn constraints(&self) -> Vec<ConstraintDef> {
        let mut out = self.constraints.clone();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn schema_index_seek(
        &self,
        entity: IndexEntity,
        label: &str,
        properties: &[String],
        values: &[Value],
    ) -> Result<Option<Vec<u64>>> {
        let Some(ix) = self.find_index(entity, label, properties) else {
            return Ok(None);
        };
        if values.len() != properties.len() {
            return Err(Error::Runtime("an index seek needs one value per property".into()));
        }
        Ok(Some(match seek_key(values) {
            Some(k) => ix.entries.get(&k),
            None => Vec::new(),
        }))
    }

    fn schema_index_range(
        &self,
        entity: IndexEntity,
        label: &str,
        property: &str,
        range: &ValueRange,
    ) -> Result<Option<Vec<u64>>> {
        let Some(ix) = self.find_index(entity, label, &[property.to_string()]) else {
            return Ok(None);
        };
        Ok(Some(ix.entries.range(range)))
    }

    fn snapshot(&self) -> Option<Snapshot> {
        Some(Arc::new(self.clone()))
    }

    fn backup(&self, dest: &std::path::Path) -> Result<crate::BackupInfo> {
        // Inside a transaction the committed state is the one saved at `begin`.
        let committed = self.saved.as_deref().unwrap_or(self);
        crate::hybrid::backup_memory(committed, 0, dest)
    }

    fn begin(&mut self) -> Result<()> {
        self.saved = Some(Box::new(self.clone()));
        self.changes.begin();
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.saved = None;
        self.changes.commit();
        Ok(())
    }

    fn rollback(&mut self) {
        if let Some(saved) = self.saved.take() {
            let mut changes = std::mem::take(&mut self.changes);
            changes.rollback();
            *self = *saved;
            self.changes = changes;
        }
    }

    fn take_changes(&mut self) -> Changes {
        self.changes.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labelled(g: &MemoryGraph, label: &str) -> Vec<NodeId> {
        g.nodes_with_label(label).collect::<Result<_>>().unwrap()
    }

    fn props(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn create_and_expand() {
        let mut g = MemoryGraph::new();
        let a = g
            .create_node(&["Person".into()], props(&[("name", "a".into())]))
            .unwrap();
        let b = g.create_node(&["Person".into()], props(&[])).unwrap();
        let r = g.create_relationship(a, b, "KNOWS", props(&[])).unwrap();
        assert_eq!(
            g.expand_all(a, Direction::Outgoing, &[]).unwrap(),
            vec![Expansion { rel: r, other: b }]
        );
        assert_eq!(
            g.expand_all(b, Direction::Incoming, &["KNOWS".into()]).unwrap(),
            vec![Expansion { rel: r, other: a }]
        );
        assert!(
            g.expand_all(a, Direction::Outgoing, &["LIKES".into()]).unwrap()
                .is_empty()
        );
        assert_eq!(labelled(&g, "Person"), vec![a, b]);
        assert_eq!(g.node_property(a, "name").unwrap(), Some("a".into()));
    }

    #[test]
    fn self_loop_reported_once_for_both() {
        let mut g = MemoryGraph::new();
        let a = g.create_node(&[], props(&[])).unwrap();
        g.create_relationship(a, a, "SELF", props(&[])).unwrap();
        assert_eq!(g.expand_all(a, Direction::Both, &[]).unwrap().len(), 1);
        assert_eq!(g.degree(a, Direction::Both, &[]).unwrap(), 1);
        assert_eq!(g.degree(a, Direction::Outgoing, &[]).unwrap(), 1);
        assert_eq!(g.expand_all(a, Direction::Incoming, &[]).unwrap().len(), 1);
    }

    #[test]
    fn delete_requires_detach() {
        let mut g = MemoryGraph::new();
        let a = g.create_node(&[], props(&[])).unwrap();
        let b = g.create_node(&[], props(&[])).unwrap();
        g.create_relationship(a, b, "R", props(&[])).unwrap();
        assert!(matches!(g.delete_node(a, false), Err(Error::Constraint(_))));
        assert!(g.delete_node(a, true).unwrap());
        assert_eq!(g.relationship_count(), 0);
        assert!(g.expand_all(b, Direction::Both, &[]).unwrap().is_empty());
    }

    /// Everything a reader sees of a graph, in a canonical form.
    fn observe(g: &dyn GraphStore) -> String {
        let mut out = format!("{} {} {:?} {:?}\n", g.node_count(), g.relationship_count(), g.labels(), g.relationship_types());
        for n in g.all_nodes() {
            let n = n.unwrap();
            let mut adj: Vec<(u64, u64)> = g
                .expand(n, Direction::Both, &[])
                .map(|e| e.map(|e| (e.rel.0, e.other.0)).unwrap())
                .collect();
            adj.sort_unstable();
            out += &format!("{n} {:?} {:?} {adj:?}\n", g.node_labels(n).unwrap(), g.node_properties(n).unwrap());
        }
        for l in ["A", "B"] {
            out += &format!("{l} {:?}\n", labelled_dyn(g, l));
        }
        for v in 0..5 {
            out += &format!("{v} {:?}\n", g.index_seek("A", "x", &Value::Int(v)).unwrap());
        }
        out
    }

    fn labelled_dyn(g: &dyn GraphStore, label: &str) -> Vec<NodeId> {
        g.nodes_with_label(label).collect::<Result<_>>().unwrap()
    }

    #[test]
    fn snapshots_are_unaffected_by_later_writes() {
        let mut g = MemoryGraph::new();
        g.create_index(IndexDef::node("ax", "A", "x")).unwrap();
        // Enough nodes and a big enough hub for multi-level copy-on-write structures.
        let hub = g.create_node(&["B".into()], props(&[])).unwrap();
        let mut nodes = Vec::new();
        for i in 0..2000 {
            let n = g.create_node(&["A".into()], props(&[("x", Value::Int(i % 5))])).unwrap();
            g.create_relationship(hub, n, "R", props(&[("w", Value::Int(i))])).unwrap();
            nodes.push(n);
        }
        let first = g.snapshot().unwrap();
        let seen = observe(&*first);

        g.begin().unwrap();
        for (i, &n) in nodes.iter().enumerate() {
            match i % 4 {
                0 => g.set_node_property(n, "x", Value::Int(9)).unwrap(),
                1 => {
                    g.remove_label(n, "A").unwrap();
                }
                2 => {
                    g.delete_node(n, true).unwrap();
                }
                _ => {
                    let m = g.create_node(&["A".into()], props(&[("x", Value::Int(1))])).unwrap();
                    g.create_relationship(m, n, "S", props(&[])).unwrap();
                }
            }
        }
        // A snapshot taken inside a transaction shows the last commit.
        let during = g.snapshot().unwrap();
        assert_eq!(observe(&*during), seen);
        g.commit().unwrap();
        let second = g.snapshot().unwrap();
        let changed = observe(&*second);
        assert_ne!(changed, seen);

        g.begin().unwrap();
        g.delete_node(hub, true).unwrap();
        g.rollback();
        g.begin().unwrap();
        g.delete_node(hub, true).unwrap();
        g.commit().unwrap();

        assert_eq!(observe(&*first), seen);
        assert_eq!(observe(&*during), seen);
        assert_eq!(observe(&*second), changed);
        assert_ne!(observe(&g), changed);
    }

    #[test]
    fn rollback_restores_everything() {
        let mut g = MemoryGraph::new();
        let a = g
            .create_node(&["A".into()], props(&[("x", 1.into())]))
            .unwrap();
        let b = g.create_node(&["B".into()], props(&[])).unwrap();
        let r = g
            .create_relationship(a, b, "R", props(&[("w", 2.into())]))
            .unwrap();

        g.begin().unwrap();
        g.set_node_property(a, "x", 10.into()).unwrap();
        g.set_node_property(a, "y", "new".into()).unwrap();
        g.add_label(a, "Extra").unwrap();
        g.remove_label(b, "B").unwrap();
        let c = g.create_node(&["C".into()], props(&[])).unwrap();
        g.create_relationship(c, a, "R2", props(&[])).unwrap();
        g.delete_node(b, true).unwrap();
        g.rollback();

        assert_eq!(g.node_count(), 2);
        assert_eq!(g.relationship_count(), 1);
        assert_eq!(g.node_property(a, "x").unwrap(), Some(1.into()));
        assert_eq!(g.node_property(a, "y").unwrap(), None);
        assert_eq!(g.node_labels(a).unwrap(), vec!["A".to_string()]);
        assert_eq!(labelled(&g, "B"), vec![b]);
        assert!(labelled(&g, "C").is_empty());
        assert!(labelled(&g, "Extra").is_empty());
        assert_eq!(
            g.expand_all(a, Direction::Both, &[]).unwrap(),
            vec![Expansion { rel: r, other: b }]
        );
        assert_eq!(g.relationship_property(r, "w").unwrap(), Some(2.into()));
    }

    fn idx(name: &str, label: &str, prop: &str) -> IndexDef {
        IndexDef::node(name, label, prop)
    }

    #[test]
    fn property_index_follows_every_mutation() {
        let mut g = MemoryGraph::new();
        let a = g
            .create_node(&["P".into()], props(&[("k", 1.into())]))
            .unwrap();
        g.create_index(idx("p_k", "P", "k")).unwrap();
        assert!(g.create_index(idx("again", "P", "k")).is_err());
        let seek = |g: &MemoryGraph, v: Value| g.index_seek("P", "k", &v).unwrap().unwrap();

        // Existing data is indexed; integral floats match ints.
        assert_eq!(seek(&g, 1.into()), vec![a]);
        assert_eq!(seek(&g, Value::Float(1.0)), vec![a]);

        let b = g
            .create_node(&["P".into()], props(&[("k", 2.into())]))
            .unwrap();
        assert_eq!(seek(&g, 2.into()), vec![b]);
        g.set_node_property(b, "k", 1.into()).unwrap();
        assert_eq!(seek(&g, 1.into()), vec![a, b]);
        assert!(seek(&g, 2.into()).is_empty());
        g.remove_label(b, "P").unwrap();
        assert_eq!(seek(&g, 1.into()), vec![a]);
        g.add_label(b, "P").unwrap();
        assert_eq!(seek(&g, 1.into()), vec![a, b]);
        g.delete_node(a, false).unwrap();
        assert_eq!(seek(&g, 1.into()), vec![b]);
        assert!(g.index_seek("P", "other", &1.into()).unwrap().is_none());
    }

    #[test]
    fn property_index_rolls_back() {
        let mut g = MemoryGraph::new();
        let a = g
            .create_node(&["P".into()], props(&[("k", 1.into())]))
            .unwrap();
        g.create_index(idx("p_k", "P", "k")).unwrap();
        g.begin().unwrap();
        g.set_node_property(a, "k", 5.into()).unwrap();
        g.create_node(&["P".into()], props(&[("k", 1.into())]))
            .unwrap();
        g.remove_label(a, "P").unwrap();
        g.create_index(idx("p_x", "P", "x")).unwrap();
        g.drop_index("p_k").unwrap();
        g.rollback();
        assert_eq!(g.indexes(), vec![idx("p_k", "P", "k")]);
        assert_eq!(g.index_seek("P", "k", &1.into()).unwrap().unwrap(), vec![a]);
        assert!(g.index_seek("P", "k", &5.into()).unwrap().unwrap().is_empty());
    }

    #[test]
    fn relationship_and_composite_indexes_follow_writes_and_roll_back() {
        let mut g = MemoryGraph::new();
        let a = g.create_node(&["P".into()], props(&[("x", 1.into()), ("y", 2.into())])).unwrap();
        let b = g.create_node(&["P".into()], props(&[("x", 1.into())])).unwrap();
        let r = g.create_relationship(a, b, "R", props(&[("w", 5.into())])).unwrap();
        g.create_index(IndexDef::relationship("r_w", "R", "w")).unwrap();
        let xy = ["x".to_string(), "y".to_string()];
        g.create_index(IndexDef {
            name: "p_xy".into(),
            entity: IndexEntity::Node,
            label: "P".into(),
            properties: xy.to_vec(),
            extra_labels: Vec::new(),
            kind: Default::default(),
        })
        .unwrap();
        let w = ["w".to_string()];
        let rel_seek = |g: &MemoryGraph, v: i64| {
            g.schema_index_seek(IndexEntity::Relationship, "R", &w, &[v.into()]).unwrap().unwrap()
        };
        let pair_seek = |g: &MemoryGraph, x: i64, y: i64| {
            g.schema_index_seek(IndexEntity::Node, "P", &xy, &[x.into(), y.into()]).unwrap().unwrap()
        };
        assert_eq!(rel_seek(&g, 5), vec![r.0]);
        assert_eq!(pair_seek(&g, 1, 2), vec![a.0]);
        assert!(pair_seek(&g, 1, 0).is_empty());
        // A node index does not answer for relationships, nor a composite for one property.
        assert!(g.index_seek("R", "w", &5.into()).unwrap().is_none());
        assert!(g.index_seek("P", "x", &1.into()).unwrap().is_none());
        assert_eq!(
            g.schema_index_stats(IndexEntity::Node, "P", &xy),
            Some(IndexStats { entries: 1, distinct: 1 })
        );

        g.begin().unwrap();
        g.set_relationship_property(r, "w", 6.into()).unwrap();
        g.set_node_property(b, "y", 2.into()).unwrap();
        let r2 = g.create_relationship(b, a, "R", props(&[("w", 6.into())])).unwrap();
        assert_eq!(rel_seek(&g, 6), vec![r.0, r2.0]);
        assert_eq!(pair_seek(&g, 1, 2), vec![a.0, b.0]);
        g.delete_relationship(r).unwrap();
        g.delete_node(a, true).unwrap();
        assert_eq!(rel_seek(&g, 6), Vec::<u64>::new());
        assert_eq!(pair_seek(&g, 1, 2), vec![b.0]);
        g.rollback();
        assert_eq!(rel_seek(&g, 5), vec![r.0]);
        assert!(rel_seek(&g, 6).is_empty());
        assert_eq!(pair_seek(&g, 1, 2), vec![a.0]);
    }

    #[test]
    fn rejects_map_properties() {
        let mut g = MemoryGraph::new();
        let bad = props(&[("m", Value::Map(BTreeMap::new()))]);
        assert!(matches!(g.create_node(&[], bad), Err(Error::Type(_))));
    }

    fn live(list: &AdjList) -> Vec<u64> {
        list.iter().map(|e| e.rel.0).collect()
    }

    #[test]
    fn adj_list_tombstones_and_compaction() {
        let entry = |i: u64| AdjEntry {
            rel: RelId(i),
            other: NodeId(i + 1000),
        };
        let mut l = AdjList::default();
        for i in 0..100 {
            l.insert(entry(i));
        }
        // Remove the even ids, front to back: tombstones, exactly half, not yet compacted.
        for i in (0..100).step_by(2) {
            l.remove(RelId(i));
            assert_eq!(l.len(), 100 - (i as usize / 2 + 1));
        }
        assert_eq!(live(&l), (1..100).step_by(2).collect::<Vec<_>>());
        assert_eq!((l.entries.len(), l.dead), (100, 50));
        // Re-inserting a removed id revives its tombstone.
        l.insert(entry(10));
        assert_eq!((l.entries.len(), l.dead), (100, 49));
        // Removing twice or removing an unknown id is a no-op.
        l.remove(RelId(0));
        l.remove(RelId(5000));
        assert_eq!(l.len(), 51);
        // Past half dead (on removing 1), the list is compacted; 3 is a new tombstone.
        for i in [10, 1, 3] {
            l.remove(RelId(i));
        }
        assert_eq!((l.entries.len(), l.dead), (49, 1));
        assert_eq!(l.len(), 48);
        // Re-inserting (a rollback) restores sorted order after a compaction too.
        for i in (0..50).rev().map(|i| i * 2).chain([1, 3]) {
            l.insert(entry(i));
        }
        assert_eq!(live(&l), (0..100).collect::<Vec<_>>());
        assert!(l.iter().all(|e| e.other.0 == e.rel.0 + 1000));
        // Removing from the tail drops entries instead of leaving tombstones.
        for i in (0..100).rev() {
            l.remove(RelId(i));
        }
        assert!(l.is_empty());
        assert!(l.entries.is_empty());
    }

    #[test]
    fn expansion_is_grouped_by_type() {
        let mut g = MemoryGraph::new();
        let a = g.create_node(&[], props(&[])).unwrap();
        let b = g.create_node(&[], props(&[])).unwrap();
        let k1 = g.create_relationship(a, b, "KNOWS", props(&[])).unwrap();
        let l1 = g.create_relationship(a, b, "LIKES", props(&[])).unwrap();
        let k2 = g.create_relationship(b, a, "KNOWS", props(&[])).unwrap();
        let s = g.create_relationship(a, a, "LIKES", props(&[])).unwrap();
        let rels = |g: &MemoryGraph, dir, types: &[&str]| -> Vec<RelId> {
            let types: Vec<String> = types.iter().map(|t| t.to_string()).collect();
            g.expand_all(a, dir, &types).unwrap().iter().map(|e| e.rel).collect()
        };
        assert_eq!(rels(&g, Direction::Outgoing, &["KNOWS"]), vec![k1]);
        assert_eq!(rels(&g, Direction::Incoming, &["KNOWS"]), vec![k2]);
        assert_eq!(rels(&g, Direction::Outgoing, &["LIKES"]), vec![l1, s]);
        assert_eq!(rels(&g, Direction::Both, &["LIKES"]), vec![l1, s]);
        assert_eq!(rels(&g, Direction::Both, &["LIKES", "KNOWS", "NOPE"]).len(), 4);
        assert!(rels(&g, Direction::Both, &["NOPE"]).is_empty());
        // Outgoing of every type first, then incoming; by type, then creation order.
        assert_eq!(rels(&g, Direction::Both, &[]), vec![k1, l1, s, k2]);

        let deg = |g: &MemoryGraph, dir, t: &[&str]| {
            let t: Vec<String> = t.iter().map(|x| x.to_string()).collect();
            g.degree(a, dir, &t).unwrap()
        };
        assert_eq!(deg(&g, Direction::Both, &[]), 4);
        assert_eq!(deg(&g, Direction::Outgoing, &[]), 3);
        assert_eq!(deg(&g, Direction::Incoming, &[]), 2);
        assert_eq!(deg(&g, Direction::Both, &["LIKES"]), 2);
        assert_eq!(deg(&g, Direction::Incoming, &["LIKES"]), 1);
        assert_eq!(deg(&g, Direction::Both, &["NOPE"]), 0);

        // Deleting the last relationship of a type removes its group.
        g.delete_relationship(k1).unwrap();
        g.delete_relationship(k2).unwrap();
        assert_eq!(g.node(a).unwrap().adj.len(), 1);
        assert_eq!(rels(&g, Direction::Both, &[]), vec![l1, s]);
    }

    #[test]
    fn rollback_of_mass_deletes_restores_adjacency() {
        let mut g = MemoryGraph::new();
        let hub = g.create_node(&[], props(&[])).unwrap();
        let mut others = Vec::new();
        for i in 0..500 {
            let n = g.create_node(&[], props(&[])).unwrap();
            let t = if i % 3 == 0 { "A" } else { "B" };
            if i % 2 == 0 {
                g.create_relationship(hub, n, t, props(&[])).unwrap();
            } else {
                g.create_relationship(n, hub, t, props(&[])).unwrap();
            }
            others.push(n);
        }
        g.create_relationship(hub, hub, "A", props(&[])).unwrap();
        let before = g.expand_all(hub, Direction::Both, &[]).unwrap();
        let degree = g.degree(hub, Direction::Both, &[]).unwrap();
        assert_eq!(degree, 501);

        // Relationship deletes oldest first (tombstones and compactions), then detach.
        g.begin().unwrap();
        let ids: Vec<RelId> = before.iter().map(|e| e.rel).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        for r in sorted.iter().take(300) {
            g.delete_relationship(*r).unwrap();
        }
        assert_eq!(g.degree(hub, Direction::Both, &[]).unwrap(), 201);
        g.delete_node(hub, true).unwrap();
        g.rollback();

        assert_eq!(g.expand_all(hub, Direction::Both, &[]).unwrap(), before);
        assert_eq!(g.degree(hub, Direction::Both, &[]).unwrap(), degree);
        for n in others {
            assert_eq!(g.degree(n, Direction::Both, &[]).unwrap(), 1);
        }

        // The same, committed: everything is gone, on both ends.
        g.begin().unwrap();
        g.delete_node(hub, true).unwrap();
        g.commit().unwrap();
        assert_eq!(g.relationship_count(), 0);
        assert!(g.nodes.iter().flatten().all(|n| n.adj.is_empty()));
    }
}
