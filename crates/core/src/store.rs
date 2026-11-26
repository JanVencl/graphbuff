use std::collections::BTreeMap;
use std::ops::Bound;

use crate::codec::{Decoder, Encoder};
use crate::{Error, NodeId, RelId, Result, Value};

/// Direction of a relationship relative to a node.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

impl Direction {
    pub fn reverse(self) -> Direction {
        match self {
            Direction::Outgoing => Direction::Incoming,
            Direction::Incoming => Direction::Outgoing,
            Direction::Both => Direction::Both,
        }
    }
}

/// One step of an expansion: a relationship and the node on its other end.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Expansion {
    pub rel: RelId,
    pub other: NodeId,
}

/// The storage abstraction the query engine executes against.
///
/// Implementations: [`crate::MemoryGraph`], [`crate::HybridGraph`] and
/// [`crate::PersistentGraph`]. GPU-backed stores would implement the same trait,
/// so the Cypher layer stays unchanged.
///
/// Names (labels, relationship types, property keys) cross this boundary as strings;
/// implementations are free to intern them internally.
///
/// ## Reads
/// Reads of entities, scans and traversals return `Result`, because a disk-backed store
/// may fail to read a page (an I/O error or a corrupt file). In-memory stores always
/// return `Ok`. The catalog methods (`node_count`, `labels`, `label_count`, `indexes`, …)
/// are infallible: every store answers them from metadata it keeps in memory.
///
/// Reading an entity that does not exist is not an error: it yields `false`, `None` or
/// an empty collection.
///
/// ## Transactions
/// The engine calls [`begin`](GraphStore::begin) before a query that writes and then
/// exactly one of [`commit`](GraphStore::commit) or [`rollback`](GraphStore::rollback).
/// A rollback must restore the graph to its state at `begin`.
///
/// `commit` may fail (for example when a durable store cannot write its log). A store
/// whose `commit` returns an error must already have rolled the transaction back, so
/// that the caller never sees a half-committed state.
///
/// ## Lifecycle
/// Opening a store is store-specific (see [`crate::open_store`]). Durable stores flush on
/// [`checkpoint`](GraphStore::checkpoint) and when dropped; in-memory stores ignore both.
pub trait GraphStore {
    // ---- writes -------------------------------------------------------------------

    fn create_node(&mut self, labels: &[String], props: BTreeMap<String, Value>) -> Result<NodeId>;

    fn create_relationship(
        &mut self,
        start: NodeId,
        end: NodeId,
        rel_type: &str,
        props: BTreeMap<String, Value>,
    ) -> Result<RelId>;

    /// Deletes a node. Without `detach`, fails if the node still has relationships.
    /// Returns `false` if the node did not exist (e.g. already deleted).
    fn delete_node(&mut self, id: NodeId, detach: bool) -> Result<bool>;

    /// Returns `false` if the relationship did not exist.
    fn delete_relationship(&mut self, id: RelId) -> Result<bool>;

    /// Sets a property; setting `Value::Null` removes it.
    fn set_node_property(&mut self, id: NodeId, key: &str, value: Value) -> Result<()>;

    /// Sets a property; setting `Value::Null` removes it.
    fn set_relationship_property(&mut self, id: RelId, key: &str, value: Value) -> Result<()>;

    /// Returns `true` if the label was newly added.
    fn add_label(&mut self, id: NodeId, label: &str) -> Result<bool>;

    /// Returns `true` if the label was present and removed.
    fn remove_label(&mut self, id: NodeId, label: &str) -> Result<bool>;

    // ---- entity reads ---------------------------------------------------------------

    fn node_exists(&self, id: NodeId) -> Result<bool>;
    fn relationship_exists(&self, id: RelId) -> Result<bool>;

    fn node_labels(&self, id: NodeId) -> Result<Vec<String>>;
    fn node_has_label(&self, id: NodeId, label: &str) -> Result<bool>;
    fn node_property(&self, id: NodeId, key: &str) -> Result<Option<Value>>;
    fn node_properties(&self, id: NodeId) -> Result<BTreeMap<String, Value>>;

    fn relationship_type(&self, id: RelId) -> Result<Option<String>>;
    /// `(start, end)` of the relationship.
    fn relationship_endpoints(&self, id: RelId) -> Result<Option<(NodeId, NodeId)>>;
    fn relationship_property(&self, id: RelId, key: &str) -> Result<Option<Value>>;
    fn relationship_properties(&self, id: RelId) -> Result<BTreeMap<String, Value>>;

    // ---- scans & traversal ------------------------------------------------------------

    /// All node ids, in ascending order.
    fn all_nodes(&self) -> Scan<'_, NodeId>;
    /// Node ids carrying `label`, in ascending order.
    fn nodes_with_label(&self, label: &str) -> Scan<'_, NodeId>;
    /// Number of nodes carrying `label` (used by the planner to pick scan order).
    fn label_count(&self, label: &str) -> usize;
    /// All relationship ids, in ascending order.
    fn all_relationships(&self) -> Scan<'_, RelId>;

    /// Relationships attached to `node` in `dir`, optionally restricted to `types`
    /// (empty slice = any type). With [`Direction::Both`], self-loops appear once.
    ///
    /// The result is lazy: a disk store reads the adjacency list in batches, so a caller
    /// that stops early (`LIMIT`, a failed match further down) reads only what it needs.
    /// A missing node yields nothing; a failed read shows up as an `Err` item.
    fn expand(&self, node: NodeId, dir: Direction, types: &[String]) -> Adjacency<'_>;

    /// [`expand`](GraphStore::expand), collected.
    fn expand_all(&self, node: NodeId, dir: Direction, types: &[String]) -> Result<Vec<Expansion>> {
        self.expand(node, dir, types).collect()
    }

    /// Number of relationships [`expand`](GraphStore::expand) would yield, without
    /// collecting them. Stores answer common cases (one direction, any type) from counts
    /// they keep per node.
    fn degree(&self, node: NodeId, dir: Direction, types: &[String]) -> Result<usize> {
        self.expand(node, dir, types).try_fold(0, |n, e| e.map(|_| n + 1))
    }

    // ---- catalog ----------------------------------------------------------------------

    fn node_count(&self) -> usize;
    fn relationship_count(&self) -> usize;
    /// Labels currently in use.
    fn labels(&self) -> Vec<String>;
    /// Relationship types currently in use.
    fn relationship_types(&self) -> Vec<String>;
    /// Number of relationships of `rel_type` (used by the planner's estimates). Stores
    /// that do not count per type answer with all relationships, an upper bound.
    fn relationship_type_count(&self, rel_type: &str) -> usize {
        let _ = rel_type;
        self.relationship_count()
    }
    /// Property keys ever used.
    fn property_keys(&self) -> Vec<String>;

    // ---- property indexes -------------------------------------------------------------
    //
    // Indexes are optional: the defaults describe a store without index support, and the
    // engine falls back to label scans. Index changes are transactional like data changes.
    //
    // An index covers the nodes with a label, or the relationships of a type, that have
    // every one of its properties. Single-property indexes answer equality and range
    // seeks; composite indexes answer equality seeks on all of their properties. Seeks
    // return entity ids as `u64` (a `NodeId` or `RelId` by the index's entity).

    /// Creates an index and fills it from existing data. Fails if an index with the same
    /// name, or on the same entity, label or type and properties, exists
    /// (see [`IndexDef::check_new`]).
    fn create_index(&mut self, def: IndexDef) -> Result<()> {
        let _ = def;
        Err(Error::Unsupported(
            "this store does not support indexes".into(),
        ))
    }

    /// Returns `false` if no index has that name.
    fn drop_index(&mut self, name: &str) -> Result<bool> {
        let _ = name;
        Ok(false)
    }

    fn indexes(&self) -> Vec<IndexDef> {
        Vec::new()
    }

    /// Size of the index on exactly these properties (in this order) of `label` (a label
    /// or a relationship type), or `None` if there is none.
    fn schema_index_stats(&self, entity: IndexEntity, label: &str, properties: &[String]) -> Option<IndexStats> {
        let _ = (entity, label, properties);
        None
    }

    /// Entities whose properties equal `values` (Cypher equality, one value per property),
    /// in ascending id order, or `None` if there is no index on exactly these properties.
    /// A null value matches nothing.
    fn schema_index_seek(
        &self,
        entity: IndexEntity,
        label: &str,
        properties: &[String],
        values: &[Value],
    ) -> Result<Option<Vec<u64>>> {
        let _ = (entity, label, properties, values);
        Ok(None)
    }

    /// Entities whose `property` lies in `range`, in ascending id order, or `None` if there
    /// is no single-property index on it. Every entity whose value lies in the range is
    /// included; a store may include others too (with values close to a bound, or long
    /// strings it indexes by a prefix), so callers re-check the predicate.
    fn schema_index_range(
        &self,
        entity: IndexEntity,
        label: &str,
        property: &str,
        range: &ValueRange,
    ) -> Result<Option<Vec<u64>>> {
        let _ = (entity, label, property, range);
        Ok(None)
    }

    /// Whether there is a single-property node index on `(:label).property`.
    fn has_index(&self, label: &str, property: &str) -> bool {
        self.index_stats(label, property).is_some()
    }

    /// Size of the single-property node index on `(:label).property`, if any.
    fn index_stats(&self, label: &str, property: &str) -> Option<IndexStats> {
        self.schema_index_stats(IndexEntity::Node, label, &[property.to_string()])
    }

    // ---- constraints ------------------------------------------------------------------
    //
    // A store only keeps constraint definitions (transactionally, and durably if the store
    // is durable). Checking data against them is the query engine's job.

    /// Records a constraint. Fails if one with the same name, or the same kind, label and
    /// properties, exists (see [`ConstraintDef::check_new`]).
    fn create_constraint(&mut self, def: ConstraintDef) -> Result<()> {
        let _ = def;
        Err(Error::Unsupported(
            "this store does not support constraints".into(),
        ))
    }

    /// Returns `false` if no constraint has that name.
    fn drop_constraint(&mut self, name: &str) -> Result<bool> {
        let _ = name;
        Ok(false)
    }

    /// Every constraint, by name.
    fn constraints(&self) -> Vec<ConstraintDef> {
        Vec::new()
    }

    /// Nodes with `label` whose `property` equals `value`, from the single-property node
    /// index ([`schema_index_seek`](GraphStore::schema_index_seek)).
    fn index_seek(&self, label: &str, property: &str, value: &Value) -> Result<Option<Vec<NodeId>>> {
        let ids = self.schema_index_seek(
            IndexEntity::Node,
            label,
            &[property.to_string()],
            std::slice::from_ref(value),
        )?;
        Ok(ids.map(|ids| ids.into_iter().map(NodeId).collect()))
    }

    /// Nodes with `label` whose `property` lies in `range`, from the single-property node
    /// index ([`schema_index_range`](GraphStore::schema_index_range)).
    fn index_range(&self, label: &str, property: &str, range: &ValueRange) -> Result<Option<Vec<NodeId>>> {
        let ids = self.schema_index_range(IndexEntity::Node, label, property, range)?;
        Ok(ids.map(|ids| ids.into_iter().map(NodeId).collect()))
    }

    // ---- transactions -----------------------------------------------------------------

    /// A read-only view of the last committed state that later writes do not change, or
    /// `None` if the store cannot provide one. Taking it must be cheap: the engine takes a
    /// snapshot after every commit and runs read queries on it, so they neither wait for
    /// nor see a transaction in progress. The view's write methods are never called.
    fn snapshot(&self) -> Option<Snapshot> {
        None
    }

    fn begin(&mut self) -> Result<()>;
    /// Makes the open transaction permanent. On error the transaction has been rolled back.
    fn commit(&mut self) -> Result<()>;
    fn rollback(&mut self);

    /// The entities that the commits since the last call created, deleted or changed
    /// (labels, type, properties), for caches of the committed state kept outside the
    /// store: the engine's vector index matrices. It may name more entities than changed
    /// (ones a rolled-back transaction touched, say), never fewer; [`Changes::unknown`]
    /// if the store cannot tell. Called between transactions.
    fn take_changes(&mut self) -> Changes {
        Changes::unknown()
    }

    // ---- lifecycle --------------------------------------------------------------------

    /// Compacts durable state (e.g. writes a snapshot and truncates the write-ahead log).
    /// Must not be called while a transaction is open. A no-op for volatile stores.
    fn checkpoint(&mut self) -> Result<()> {
        Ok(())
    }

    /// A short human-readable description of the storage, e.g. `in-memory`.
    fn describe(&self) -> String {
        "in-memory".into()
    }

    /// The directory of the store's files, if it has any. The engine keeps files of its own
    /// there (saved HNSW graphs), which the store ignores and which can be deleted.
    fn data_dir(&self) -> Option<&std::path::Path> {
        None
    }

    // ---- maintenance ------------------------------------------------------------------

    /// Writes a copy of the last committed state into `dest`, a new or empty directory,
    /// as a store that opens with the same kind of storage (a persistent store's backup
    /// is a persistent store; an in-memory or hybrid store's backup is a hybrid store).
    /// Called on a [`snapshot`](GraphStore::snapshot), it runs while writers commit
    /// (an online backup). See [`crate::maintenance`].
    fn backup(&self, dest: &std::path::Path) -> Result<crate::BackupInfo> {
        let _ = dest;
        Err(Error::Unsupported(format!("{} storage cannot be backed up", self.describe())))
    }

    /// Checks the store's files for internal consistency: records, adjacency lists,
    /// counts, indexes and constraints. Called on a [`snapshot`](GraphStore::snapshot),
    /// it runs while writers commit.
    fn check(&self) -> Result<crate::CheckReport> {
        Err(Error::Unsupported(format!(
            "consistency checks are for persistent storage, not {}",
            self.describe()
        )))
    }
}

/// A lazily evaluated scan of entity ids. Disk-backed stores read it in batches, so a
/// scan of a huge label never sits in memory at once and can stop early. A failed read
/// shows up as an `Err` item; callers stop at the first one.
///
/// The scan borrows the store immutably, so the store cannot change while it runs.
pub type Scan<'a, T> = Box<dyn Iterator<Item = Result<T>> + 'a>;

/// A lazily evaluated expansion: the relationships of one node, with their other ends.
/// Like [`Scan`], it borrows the store immutably and reports a failed read as an `Err`.
pub type Adjacency<'a> = Box<dyn Iterator<Item = Result<Expansion>> + 'a>;

/// The entities changed by a store's commits ([`GraphStore::take_changes`]): node and
/// relationship ids, possibly repeated, or `unknown`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub nodes: Vec<u64>,
    pub rels: Vec<u64>,
    /// The store cannot tell what changed (it does not keep track, or too much did).
    pub unknown: bool,
}

impl Changes {
    /// More ids than a store records before it gives up and reports [`Changes::unknown`]:
    /// past this a cache does better to rebuild than to apply the changes one by one.
    pub const LIMIT: usize = 1 << 20;

    pub fn unknown() -> Changes {
        Changes { unknown: true, ..Changes::default() }
    }

    /// The number of ids recorded.
    pub fn len(&self) -> usize {
        self.nodes.len() + self.rels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0 && !self.unknown
    }

    /// Adds the changes of later commits.
    pub fn extend(&mut self, later: Changes) {
        if self.unknown || later.unknown || self.len() + later.len() > Changes::LIMIT {
            *self = Changes::unknown();
        } else {
            self.nodes.extend(later.nodes);
            self.rels.extend(later.rels);
        }
    }

    /// Sorts the ids and drops repeats.
    pub fn normalize(&mut self) {
        for ids in [&mut self.nodes, &mut self.rels] {
            ids.sort_unstable();
            ids.dedup();
        }
    }
}

/// What a store records for [`GraphStore::take_changes`]: the ids its writes touch, in
/// order, with the position at `begin` so that a rollback can forget the transaction's.
#[derive(Clone, Debug, Default)]
pub(crate) struct ChangeLog {
    changes: Changes,
    /// Lengths of the lists and the `unknown` flag at `begin`.
    mark: Option<(usize, usize, bool)>,
}

impl ChangeLog {
    pub fn node(&mut self, id: NodeId) {
        if !self.changes.unknown {
            self.changes.nodes.push(id.0);
            self.check();
        }
    }

    pub fn rel(&mut self, id: RelId) {
        if !self.changes.unknown {
            self.changes.rels.push(id.0);
            self.check();
        }
    }

    /// Gives up past [`Changes::LIMIT`] ids, so that a store nobody takes changes from
    /// does not grow a log without end.
    fn check(&mut self) {
        if self.changes.len() > Changes::LIMIT {
            self.changes = Changes::unknown();
        }
    }

    pub fn begin(&mut self) {
        let c = &self.changes;
        self.mark = Some((c.nodes.len(), c.rels.len(), c.unknown));
    }

    pub fn commit(&mut self) {
        self.mark = None;
    }

    pub fn rollback(&mut self) {
        // A log that gave up inside the transaction stays given up: it no longer has
        // the ids from before.
        if let Some((nodes, rels, unknown)) = self.mark.take()
            && !self.changes.unknown
        {
            self.changes.nodes.truncate(nodes);
            self.changes.rels.truncate(rels);
            self.changes.unknown = unknown;
        }
    }

    pub fn take(&mut self) -> Changes {
        self.mark = None;
        std::mem::take(&mut self.changes)
    }
}

/// A type-erased store, used when the storage mode is chosen at runtime.
pub type BoxedStore = Box<dyn GraphStore + Send + Sync>;

/// A read-only view of a store's committed state ([`GraphStore::snapshot`]).
pub type Snapshot = std::sync::Arc<dyn GraphStore + Send + Sync>;

/// Forwards every method (including the ones with defaults) to the boxed store.
impl<T: GraphStore + ?Sized> GraphStore for Box<T> {
    fn create_node(&mut self, labels: &[String], props: BTreeMap<String, Value>) -> Result<NodeId> {
        (**self).create_node(labels, props)
    }
    fn create_relationship(
        &mut self,
        start: NodeId,
        end: NodeId,
        rel_type: &str,
        props: BTreeMap<String, Value>,
    ) -> Result<RelId> {
        (**self).create_relationship(start, end, rel_type, props)
    }
    fn delete_node(&mut self, id: NodeId, detach: bool) -> Result<bool> {
        (**self).delete_node(id, detach)
    }
    fn delete_relationship(&mut self, id: RelId) -> Result<bool> {
        (**self).delete_relationship(id)
    }
    fn set_node_property(&mut self, id: NodeId, key: &str, value: Value) -> Result<()> {
        (**self).set_node_property(id, key, value)
    }
    fn set_relationship_property(&mut self, id: RelId, key: &str, value: Value) -> Result<()> {
        (**self).set_relationship_property(id, key, value)
    }
    fn add_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        (**self).add_label(id, label)
    }
    fn remove_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        (**self).remove_label(id, label)
    }
    fn node_exists(&self, id: NodeId) -> Result<bool> {
        (**self).node_exists(id)
    }
    fn relationship_exists(&self, id: RelId) -> Result<bool> {
        (**self).relationship_exists(id)
    }
    fn node_labels(&self, id: NodeId) -> Result<Vec<String>> {
        (**self).node_labels(id)
    }
    fn node_has_label(&self, id: NodeId, label: &str) -> Result<bool> {
        (**self).node_has_label(id, label)
    }
    fn node_property(&self, id: NodeId, key: &str) -> Result<Option<Value>> {
        (**self).node_property(id, key)
    }
    fn node_properties(&self, id: NodeId) -> Result<BTreeMap<String, Value>> {
        (**self).node_properties(id)
    }
    fn relationship_type(&self, id: RelId) -> Result<Option<String>> {
        (**self).relationship_type(id)
    }
    fn relationship_endpoints(&self, id: RelId) -> Result<Option<(NodeId, NodeId)>> {
        (**self).relationship_endpoints(id)
    }
    fn relationship_property(&self, id: RelId, key: &str) -> Result<Option<Value>> {
        (**self).relationship_property(id, key)
    }
    fn relationship_properties(&self, id: RelId) -> Result<BTreeMap<String, Value>> {
        (**self).relationship_properties(id)
    }
    fn all_nodes(&self) -> Scan<'_, NodeId> {
        (**self).all_nodes()
    }
    fn nodes_with_label(&self, label: &str) -> Scan<'_, NodeId> {
        (**self).nodes_with_label(label)
    }
    fn label_count(&self, label: &str) -> usize {
        (**self).label_count(label)
    }
    fn all_relationships(&self) -> Scan<'_, RelId> {
        (**self).all_relationships()
    }
    fn expand(&self, node: NodeId, dir: Direction, types: &[String]) -> Adjacency<'_> {
        (**self).expand(node, dir, types)
    }
    fn expand_all(&self, node: NodeId, dir: Direction, types: &[String]) -> Result<Vec<Expansion>> {
        (**self).expand_all(node, dir, types)
    }
    fn degree(&self, node: NodeId, dir: Direction, types: &[String]) -> Result<usize> {
        (**self).degree(node, dir, types)
    }
    fn node_count(&self) -> usize {
        (**self).node_count()
    }
    fn relationship_count(&self) -> usize {
        (**self).relationship_count()
    }
    fn labels(&self) -> Vec<String> {
        (**self).labels()
    }
    fn relationship_types(&self) -> Vec<String> {
        (**self).relationship_types()
    }
    fn relationship_type_count(&self, rel_type: &str) -> usize {
        (**self).relationship_type_count(rel_type)
    }
    fn property_keys(&self) -> Vec<String> {
        (**self).property_keys()
    }
    fn create_index(&mut self, def: IndexDef) -> Result<()> {
        (**self).create_index(def)
    }
    fn drop_index(&mut self, name: &str) -> Result<bool> {
        (**self).drop_index(name)
    }
    fn indexes(&self) -> Vec<IndexDef> {
        (**self).indexes()
    }
    fn schema_index_stats(&self, entity: IndexEntity, label: &str, properties: &[String]) -> Option<IndexStats> {
        (**self).schema_index_stats(entity, label, properties)
    }
    fn schema_index_seek(
        &self,
        entity: IndexEntity,
        label: &str,
        properties: &[String],
        values: &[Value],
    ) -> Result<Option<Vec<u64>>> {
        (**self).schema_index_seek(entity, label, properties, values)
    }
    fn schema_index_range(
        &self,
        entity: IndexEntity,
        label: &str,
        property: &str,
        range: &ValueRange,
    ) -> Result<Option<Vec<u64>>> {
        (**self).schema_index_range(entity, label, property, range)
    }
    fn create_constraint(&mut self, def: ConstraintDef) -> Result<()> {
        (**self).create_constraint(def)
    }
    fn drop_constraint(&mut self, name: &str) -> Result<bool> {
        (**self).drop_constraint(name)
    }
    fn constraints(&self) -> Vec<ConstraintDef> {
        (**self).constraints()
    }
    fn snapshot(&self) -> Option<Snapshot> {
        (**self).snapshot()
    }
    fn begin(&mut self) -> Result<()> {
        (**self).begin()
    }
    fn commit(&mut self) -> Result<()> {
        (**self).commit()
    }
    fn rollback(&mut self) {
        (**self).rollback()
    }
    fn take_changes(&mut self) -> Changes {
        (**self).take_changes()
    }
    fn checkpoint(&mut self) -> Result<()> {
        (**self).checkpoint()
    }
    fn describe(&self) -> String {
        (**self).describe()
    }
    fn data_dir(&self) -> Option<&std::path::Path> {
        (**self).data_dir()
    }
    fn backup(&self, dest: &std::path::Path) -> Result<crate::BackupInfo> {
        (**self).backup(dest)
    }
    fn check(&self) -> Result<crate::CheckReport> {
        (**self).check()
    }
}

/// Whether an index covers nodes (by label) or relationships (by type).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IndexEntity {
    Node,
    Relationship,
}

impl IndexEntity {
    /// `NODE` or `RELATIONSHIP`, as `SHOW INDEXES` lists it.
    pub fn text(self) -> &'static str {
        match self {
            IndexEntity::Node => "NODE",
            IndexEntity::Relationship => "RELATIONSHIP",
        }
    }
}

/// Most properties a composite index may have (its keys must fit a B+tree key).
pub const MAX_INDEX_PROPERTIES: usize = 16;

/// Most dimensions a vector index may have.
pub const MAX_VECTOR_DIMENSIONS: u32 = 4096;

/// Set in the similarity byte of an encoded vector index that is approximate (HNSW).
const HNSW_FLAG: u8 = 0x80;
/// Set in the similarity byte of a vector index that records its embedding model; the
/// model's name follows the HNSW settings.
const MODEL_FLAG: u8 = 0x40;

/// How vectors are compared. Scores are normalized to [0, 1] as Neo4j does, higher
/// meaning more similar: cosine `(1 + cos) / 2`, euclidean `1 / (1 + d²)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VectorSimilarity {
    Cosine,
    Euclidean,
}

impl VectorSimilarity {
    /// `'cosine'` or `'euclidean'` (any case), as in index options.
    pub fn parse(name: &str) -> Option<VectorSimilarity> {
        match name.to_ascii_lowercase().as_str() {
            "cosine" => Some(VectorSimilarity::Cosine),
            "euclidean" => Some(VectorSimilarity::Euclidean),
            _ => None,
        }
    }

    /// `COSINE` or `EUCLIDEAN`, as `SHOW INDEXES` shows it.
    pub fn name(self) -> &'static str {
        match self {
            VectorSimilarity::Cosine => "COSINE",
            VectorSimilarity::Euclidean => "EUCLIDEAN",
        }
    }

    /// The normalized score of `a` and `b` (same length); `None` for cosine with a zero
    /// vector.
    pub fn score(self, a: &[f64], b: &[f64]) -> Option<f64> {
        match self {
            VectorSimilarity::Cosine => {
                let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
                for (x, y) in a.iter().zip(b) {
                    dot += x * y;
                    na += x * x;
                    nb += y * y;
                }
                if na == 0.0 || nb == 0.0 {
                    return None;
                }
                let cos = (dot / (na * nb).sqrt()).clamp(-1.0, 1.0);
                Some((1.0 + cos) / 2.0)
            }
            VectorSimilarity::Euclidean => {
                let d2: f64 = a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum();
                Some(1.0 / (1.0 + d2))
            }
        }
    }
}

/// How a full-text index splits text into terms (Lucene's analyzers, as Neo4j names
/// them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Analyzer {
    /// Words (Unicode word boundaries), lowercased.
    #[default]
    StandardNoStopWords,
    /// Words, lowercased, without English stop words.
    Standard,
    /// Words, lowercased, without English stop words and possessive `'s`, stemmed
    /// (Porter).
    English,
    /// Runs of letters, lowercased.
    Simple,
    /// Runs of letters, lowercased, without English stop words.
    Stop,
    /// Runs of non-whitespace characters, as written.
    Whitespace,
    /// The whole value as one term, as written.
    Keyword,
}

impl Analyzer {
    pub const ALL: [Analyzer; 7] = [
        Analyzer::StandardNoStopWords,
        Analyzer::Standard,
        Analyzer::English,
        Analyzer::Simple,
        Analyzer::Stop,
        Analyzer::Whitespace,
        Analyzer::Keyword,
    ];

    /// Its name in index options, such as `standard-no-stop-words`.
    pub fn name(self) -> &'static str {
        match self {
            Analyzer::StandardNoStopWords => "standard-no-stop-words",
            Analyzer::Standard => "standard",
            Analyzer::English => "english",
            Analyzer::Simple => "simple",
            Analyzer::Stop => "stop",
            Analyzer::Whitespace => "whitespace",
            Analyzer::Keyword => "keyword",
        }
    }

    /// The analyzer named `name` (any case).
    pub fn parse(name: &str) -> Option<Analyzer> {
        Analyzer::ALL.into_iter().find(|a| a.name().eq_ignore_ascii_case(name))
    }

    fn code(self) -> u8 {
        Analyzer::ALL.iter().position(|a| *a == self).expect("listed") as u8 + 1
    }

    fn from_code(code: u8) -> Option<Analyzer> {
        Analyzer::ALL.get(usize::from(code).checked_sub(1)?).copied()
    }
}

/// The settings of an approximate vector index: a graph of the vectors (HNSW) searched
/// instead of every vector. `m` is the number of neighbours a vector keeps per layer
/// (twice that on the bottom layer), `ef_construction` how many candidates an insertion
/// considers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HnswConfig {
    pub m: u16,
    pub ef_construction: u16,
}

impl HnswConfig {
    /// Neo4j's defaults.
    pub const DEFAULT: HnswConfig = HnswConfig { m: 16, ef_construction: 100 };
    /// The largest `m` (Neo4j's).
    pub const MAX_M: u16 = 512;
    /// The largest `ef_construction` (Neo4j's).
    pub const MAX_EF_CONSTRUCTION: u16 = 3200;

    pub fn is_valid(self) -> bool {
        (2..=Self::MAX_M).contains(&self.m) && (1..=Self::MAX_EF_CONSTRUCTION).contains(&self.ef_construction)
    }
}

/// What an index is: a property index answering seeks, a vector index answering
/// nearest-neighbour queries over one list-of-numbers property, a full-text index
/// answering text queries over string properties, or a point index answering distance
/// and bounding-box seeks over one point property.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum IndexKind {
    #[default]
    Range,
    Vector {
        dimensions: u32,
        similarity: VectorSimilarity,
        /// Approximate search (HNSW); `None`: exact search over every vector.
        hnsw: Option<HnswConfig>,
    },
    Fulltext {
        analyzer: Analyzer,
    },
    Point,
    /// A string index (trigrams) on one property, for `STARTS WITH`, `ENDS WITH` and
    /// `CONTAINS`.
    Text,
    /// A token lookup index: the labels of all nodes, or the types of all relationships.
    /// No label or properties.
    Lookup,
}

impl IndexKind {
    /// `RANGE`, `VECTOR`, `FULLTEXT`, `POINT`, `TEXT` or `LOOKUP`, as `SHOW INDEXES` lists it.
    pub fn text(self) -> &'static str {
        match self {
            IndexKind::Range => "RANGE",
            IndexKind::Vector { .. } => "VECTOR",
            IndexKind::Fulltext { .. } => "FULLTEXT",
            IndexKind::Point => "POINT",
            IndexKind::Text => "TEXT",
            IndexKind::Lookup => "LOOKUP",
        }
    }
}

/// An index on one or more properties of the nodes with a label, or of the
/// relationships of a type: a property (range) index, a vector index, or a full-text
/// index (which may cover several labels or types).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexDef {
    pub name: String,
    pub entity: IndexEntity,
    /// The label (node index) or relationship type (relationship index).
    pub label: String,
    /// Further labels or types of a full-text index; empty for other indexes.
    pub extra_labels: Vec<String>,
    /// One property, or several for a composite index (in order). A vector index has its
    /// vector property first, then its filter properties.
    pub properties: Vec<String>,
    pub kind: IndexKind,
    /// The embedding model that made a vector index's vectors (`vector.embedding_model`),
    /// if it was given: search uses it to embed query text. Always `None` for other kinds.
    pub embedding_model: Option<String>,
}

impl IndexDef {
    /// A single-property node index.
    pub fn node(name: impl Into<String>, label: impl Into<String>, property: impl Into<String>) -> IndexDef {
        IndexDef {
            name: name.into(),
            entity: IndexEntity::Node,
            label: label.into(),
            extra_labels: Vec::new(),
            properties: vec![property.into()],
            kind: IndexKind::Range,
            embedding_model: None,
        }
    }

    /// A vector index on one property of the nodes with `label`, or of the relationships
    /// of a type (`entity`).
    pub fn vector(
        name: impl Into<String>,
        entity: IndexEntity,
        label: impl Into<String>,
        property: impl Into<String>,
        dimensions: u32,
        similarity: VectorSimilarity,
    ) -> IndexDef {
        IndexDef {
            name: name.into(),
            entity,
            label: label.into(),
            extra_labels: Vec::new(),
            properties: vec![property.into()],
            kind: IndexKind::Vector { dimensions, similarity, hnsw: None },
            embedding_model: None,
        }
    }

    /// The same vector index, searched approximately with `hnsw`.
    pub fn with_hnsw(mut self, hnsw: HnswConfig) -> IndexDef {
        if let IndexKind::Vector { hnsw: h, .. } = &mut self.kind {
            *h = Some(hnsw);
        }
        self
    }

    /// The same vector index, recording `model` as the embedding model of its vectors.
    pub fn with_model(mut self, model: impl Into<String>) -> IndexDef {
        self.embedding_model = Some(model.into());
        self
    }

    /// The same vector index with `filters` as its filter properties.
    pub fn with_filters(mut self, filters: impl IntoIterator<Item = String>) -> IndexDef {
        self.properties.extend(filters);
        self
    }

    /// The filter properties of a vector index: kept next to its vectors, for filters
    /// applied during a search. Empty for other indexes.
    pub fn filter_properties(&self) -> &[String] {
        if self.is_vector() { self.properties.get(1..).unwrap_or_default() } else { &[] }
    }

    /// The properties that tell indexes of one kind apart: all but the filter properties
    /// of a vector index.
    fn key_properties(&self) -> &[String] {
        &self.properties[..self.properties.len() - self.filter_properties().len()]
    }

    /// A full-text index on `properties` of the nodes with any of `labels`, or of the
    /// relationships of any of the types (`entity`).
    pub fn fulltext(
        name: impl Into<String>,
        entity: IndexEntity,
        labels: Vec<String>,
        properties: Vec<String>,
        analyzer: Analyzer,
    ) -> IndexDef {
        let mut labels = labels.into_iter();
        IndexDef {
            name: name.into(),
            entity,
            label: labels.next().unwrap_or_default(),
            extra_labels: labels.collect(),
            properties,
            kind: IndexKind::Fulltext { analyzer },
            embedding_model: None,
        }
    }

    pub fn is_vector(&self) -> bool {
        matches!(self.kind, IndexKind::Vector { .. })
    }

    pub fn is_fulltext(&self) -> bool {
        matches!(self.kind, IndexKind::Fulltext { .. })
    }

    /// A point index on one property of the nodes with `label`, or of the relationships
    /// of a type (`entity`).
    pub fn point(
        name: impl Into<String>,
        entity: IndexEntity,
        label: impl Into<String>,
        property: impl Into<String>,
    ) -> IndexDef {
        IndexDef {
            name: name.into(),
            entity,
            label: label.into(),
            extra_labels: Vec::new(),
            properties: vec![property.into()],
            kind: IndexKind::Point,
            embedding_model: None,
        }
    }

    pub fn is_point(&self) -> bool {
        self.kind == IndexKind::Point
    }

    /// A text index on one property of the nodes with `label`, or of the relationships
    /// of a type (`entity`).
    pub fn text(
        name: impl Into<String>,
        entity: IndexEntity,
        label: impl Into<String>,
        property: impl Into<String>,
    ) -> IndexDef {
        IndexDef { kind: IndexKind::Text, ..IndexDef::point(name, entity, label, property) }
    }

    pub fn is_text(&self) -> bool {
        self.kind == IndexKind::Text
    }

    /// The token lookup index of the nodes' labels or the relationships' types.
    pub fn lookup(name: impl Into<String>, entity: IndexEntity) -> IndexDef {
        IndexDef {
            name: name.into(),
            entity,
            label: String::new(),
            extra_labels: Vec::new(),
            properties: Vec::new(),
            kind: IndexKind::Lookup,
            embedding_model: None,
        }
    }

    pub fn is_lookup(&self) -> bool {
        self.kind == IndexKind::Lookup
    }

    /// Whether the engine keeps the index's contents (vector, full-text, point and text
    /// indexes; a lookup index has none of its own): the store keeps only its definition.
    pub fn is_engine_kept(&self) -> bool {
        !matches!(self.kind, IndexKind::Range)
    }

    /// The labels (or types) the index covers: [`label`](Self::label), then the
    /// [`extra_labels`](Self::extra_labels) of a full-text index.
    pub fn labels(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.label.as_str()).chain(self.extra_labels.iter().map(String::as_str))
    }

    /// Whether `self` and `other` are indexes of the same kind (range, vector or
    /// full-text) on the same labels or types and properties: one makes the other
    /// redundant. The filter properties of vector indexes do not count.
    pub fn same_index(&self, other: &IndexDef) -> bool {
        let mut a: Vec<&str> = self.labels().collect();
        let mut b: Vec<&str> = other.labels().collect();
        a.sort_unstable();
        b.sort_unstable();
        std::mem::discriminant(&self.kind) == std::mem::discriminant(&other.kind)
            && self.entity == other.entity
            && a == b
            && self.key_properties() == other.key_properties()
    }

    /// A single-property relationship index.
    pub fn relationship(
        name: impl Into<String>,
        rel_type: impl Into<String>,
        property: impl Into<String>,
    ) -> IndexDef {
        IndexDef {
            name: name.into(),
            entity: IndexEntity::Relationship,
            label: rel_type.into(),
            extra_labels: Vec::new(),
            properties: vec![property.into()],
            kind: IndexKind::Range,
            embedding_model: None,
        }
    }

    /// Whether `self` is a property (range) index on exactly these properties of
    /// `label` (a label or type). Vector indexes cover nothing: they answer no seeks.
    pub fn covers(&self, entity: IndexEntity, label: &str, properties: &[String]) -> bool {
        self.kind == IndexKind::Range && self.same_schema(entity, label, properties)
    }

    /// Whether `self` is on exactly these properties of `label`, whatever its kind.
    pub fn same_schema(&self, entity: IndexEntity, label: &str, properties: &[String]) -> bool {
        self.entity == entity && self.label == label && self.properties == properties
    }

    /// The indexed schema in Cypher style: `:Label(a, b)` or `[:TYPE](a)` (`:A|B(a)` for a
    /// full-text index on several labels), without the filter properties of a vector index.
    pub fn target(&self) -> String {
        if self.is_lookup() {
            return match self.entity {
                IndexEntity::Node => "labels(n)".into(),
                IndexEntity::Relationship => "type(r)".into(),
            };
        }
        let props = self.key_properties().join(", ");
        let labels = self.labels().collect::<Vec<_>>().join("|");
        match self.entity {
            IndexEntity::Node => format!(":{labels}({props})"),
            IndexEntity::Relationship => format!("[:{labels}]({props})"),
        }
    }

    /// Checks that `self` is well formed and does not clash with `existing`.
    pub fn check_new(&self, existing: &[IndexDef]) -> Result<()> {
        if self.is_lookup() {
            if !self.label.is_empty() || !self.properties.is_empty() {
                return Err(Error::Semantic("a lookup index has no label, type or properties".into()));
            }
        } else if self.properties.is_empty() {
            return Err(Error::Semantic("an index needs at least one property".into()));
        }
        if let IndexKind::Vector { dimensions, hnsw, .. } = self.kind {
            if hnsw.is_some_and(|h| !h.is_valid()) {
                return Err(Error::Semantic(format!(
                    "HNSW needs `vector.hnsw.m` from 2 to {} and `vector.hnsw.ef_construction` from 1 to {}",
                    HnswConfig::MAX_M,
                    HnswConfig::MAX_EF_CONSTRUCTION
                )));
            }
            if dimensions == 0 || dimensions > MAX_VECTOR_DIMENSIONS {
                return Err(Error::Semantic(format!(
                    "a vector index needs 1 to {MAX_VECTOR_DIMENSIONS} dimensions, not {dimensions}"
                )));
            }
        }
        if (self.is_point() || self.is_text()) && self.properties.len() != 1 {
            return Err(Error::Semantic(format!("a {} index is on exactly one property", self.kind.text().to_lowercase())));
        }
        if let Some(m) = &self.embedding_model
            && (!self.is_vector() || m.trim().is_empty() || m.len() > 256)
        {
            return Err(Error::Semantic(if self.is_vector() {
                "`vector.embedding_model` needs a model name".into()
            } else {
                "only a vector index records an embedding model".into()
            }));
        }
        if !self.extra_labels.is_empty() && !self.is_fulltext() {
            return Err(Error::Semantic("only a full-text index covers several labels or types".into()));
        }
        for (i, l) in self.extra_labels.iter().enumerate() {
            if *l == self.label || self.extra_labels[..i].contains(l) {
                return Err(Error::Semantic(format!("`{l}` is repeated in the index")));
            }
        }
        if self.properties.len() > MAX_INDEX_PROPERTIES {
            return Err(Error::Semantic(format!(
                "an index can have at most {MAX_INDEX_PROPERTIES} properties"
            )));
        }
        for (i, p) in self.properties.iter().enumerate() {
            if self.properties[..i].contains(p) {
                return Err(Error::Semantic(format!(
                    "property `{p}` is repeated in the index"
                )));
            }
        }
        for ix in existing {
            if ix.name == self.name {
                return Err(Error::Constraint(format!(
                    "an index named `{}` already exists",
                    self.name
                )));
            }
            if ix.same_index(self) {
                return Err(Error::Constraint(format!(
                    "{} index on {} already exists",
                    match self.kind {
                        IndexKind::Range => "an",
                        IndexKind::Vector { .. } => "a vector",
                        IndexKind::Fulltext { .. } => "a full-text",
                        IndexKind::Point => "a point",
                        IndexKind::Text => "a text",
                        IndexKind::Lookup => "a lookup",
                    },
                    self.target()
                )));
            }
        }
        Ok(())
    }

    /// Encodes the definition (persistent format 6 and hybrid log 3 on).
    pub(crate) fn encode(&self, e: &mut Encoder) {
        e.str(&self.name);
        e.u8(match self.entity {
            IndexEntity::Node => 1,
            IndexEntity::Relationship => 2,
        });
        e.str(&self.label);
        e.strings(&self.properties);
    }

    pub(crate) fn decode(d: &mut Decoder) -> Result<IndexDef> {
        let name = d.str()?;
        let entity = match d.u8()? {
            1 => IndexEntity::Node,
            2 => IndexEntity::Relationship,
            t => return Err(d.corrupt(format!("unknown index entity {t}"))),
        };
        let label = d.str()?;
        let properties = d.strings()?;
        if properties.is_empty() || properties.len() > MAX_INDEX_PROPERTIES {
            return Err(d.corrupt(format!("index `{name}` has {} properties", properties.len())));
        }
        Ok(IndexDef {
            name,
            entity,
            label,
            extra_labels: Vec::new(),
            properties,
            kind: IndexKind::Range,
            embedding_model: None,
        })
    }

    /// Encodes a vector index: the definition as [`encode`](Self::encode) writes it, then
    /// its dimensions and similarity (persistent format 9, hybrid log op 14, memory
    /// snapshot 6). An approximate index sets [`HNSW_FLAG`] in the similarity byte and
    /// follows it with `m` and `ef_construction`; one that records its embedding model sets
    /// [`MODEL_FLAG`] and ends with the model's name. Filter properties follow the vector
    /// property in the definition's properties. 0.5 and older refuse an index with filter
    /// properties or a model.
    pub(crate) fn encode_vector(&self, e: &mut Encoder) {
        let IndexKind::Vector { dimensions, similarity, hnsw } = self.kind else {
            unreachable!("encode_vector of a range index")
        };
        self.encode(e);
        e.u32(dimensions);
        let code = match similarity {
            VectorSimilarity::Cosine => 1,
            VectorSimilarity::Euclidean => 2,
        };
        let model = if self.embedding_model.is_some() { MODEL_FLAG } else { 0 };
        match hnsw {
            None => e.u8(code | model),
            Some(h) => {
                e.u8(code | model | HNSW_FLAG);
                e.u32(u32::from(h.m));
                e.u32(u32::from(h.ef_construction));
            }
        }
        if let Some(m) = &self.embedding_model {
            e.str(m);
        }
    }

    pub(crate) fn decode_vector(d: &mut Decoder) -> Result<IndexDef> {
        let mut def = IndexDef::decode(d)?;
        let dimensions = d.u32()?;
        let code = d.u8()?;
        let similarity = match code & !(HNSW_FLAG | MODEL_FLAG) {
            1 => VectorSimilarity::Cosine,
            2 => VectorSimilarity::Euclidean,
            t => return Err(d.corrupt(format!("unknown vector similarity {t}"))),
        };
        let hnsw = if code & HNSW_FLAG != 0 {
            let (m, ef) = (d.u32()?, d.u32()?);
            let h = HnswConfig { m: u16::try_from(m).unwrap_or(0), ef_construction: u16::try_from(ef).unwrap_or(0) };
            if !h.is_valid() {
                return Err(d.corrupt(format!("bad HNSW settings of vector index `{}`", def.name)));
            }
            Some(h)
        } else {
            None
        };
        if code & MODEL_FLAG != 0 {
            def.embedding_model = Some(d.str()?);
        }
        if dimensions == 0 || dimensions > MAX_VECTOR_DIMENSIONS {
            return Err(d.corrupt(format!("bad vector index `{}`", def.name)));
        }
        def.kind = IndexKind::Vector { dimensions, similarity, hnsw };
        Ok(def)
    }

    /// Encodes the vector indexes among `defs`.
    pub(crate) fn encode_vector_list(defs: &[IndexDef], e: &mut Encoder) {
        let vectors: Vec<&IndexDef> = defs.iter().filter(|d| d.is_vector()).collect();
        e.len(vectors.len());
        for d in vectors {
            d.encode_vector(e);
        }
    }

    pub(crate) fn decode_vector_list(d: &mut Decoder) -> Result<Vec<IndexDef>> {
        let n = d.len(8)?;
        (0..n).map(|_| IndexDef::decode_vector(d)).collect()
    }

    /// Encodes a full-text index: the definition as [`encode`](Self::encode) writes it,
    /// then its further labels and its analyzer (persistent format 10, hybrid log op 15,
    /// memory snapshot 7).
    pub(crate) fn encode_fulltext(&self, e: &mut Encoder) {
        let IndexKind::Fulltext { analyzer } = self.kind else {
            unreachable!("encode_fulltext of another index")
        };
        self.encode(e);
        e.strings(&self.extra_labels);
        e.u8(analyzer.code());
    }

    pub(crate) fn decode_fulltext(d: &mut Decoder) -> Result<IndexDef> {
        let mut def = IndexDef::decode(d)?;
        def.extra_labels = d.strings()?;
        let code = d.u8()?;
        let analyzer = Analyzer::from_code(code).ok_or_else(|| d.corrupt(format!("unknown analyzer {code}")))?;
        def.kind = IndexKind::Fulltext { analyzer };
        Ok(def)
    }

    /// Encodes the full-text indexes among `defs`.
    pub(crate) fn encode_fulltext_list(defs: &[IndexDef], e: &mut Encoder) {
        let texts: Vec<&IndexDef> = defs.iter().filter(|d| d.is_fulltext()).collect();
        e.len(texts.len());
        for d in texts {
            d.encode_fulltext(e);
        }
    }

    pub(crate) fn decode_fulltext_list(d: &mut Decoder) -> Result<Vec<IndexDef>> {
        let n = d.len(8)?;
        (0..n).map(|_| IndexDef::decode_fulltext(d)).collect()
    }

    /// Encodes the point indexes among `defs`, each as [`encode`](Self::encode) writes it
    /// (persistent format 11, hybrid log op 16, memory snapshot 8).
    pub(crate) fn encode_point_list(defs: &[IndexDef], e: &mut Encoder) {
        let points: Vec<&IndexDef> = defs.iter().filter(|d| d.is_point()).collect();
        e.len(points.len());
        for d in points {
            d.encode(e);
        }
    }

    pub(crate) fn decode_point(d: &mut Decoder) -> Result<IndexDef> {
        let mut def = IndexDef::decode(d)?;
        if def.properties.len() != 1 {
            return Err(d.corrupt(format!("point index `{}` has {} properties", def.name, def.properties.len())));
        }
        def.kind = IndexKind::Point;
        Ok(def)
    }

    pub(crate) fn decode_point_list(d: &mut Decoder) -> Result<Vec<IndexDef>> {
        let n = d.len(8)?;
        (0..n).map(|_| IndexDef::decode_point(d)).collect()
    }

    /// Encodes the text and lookup indexes among `defs`, each as a kind tag (1 text,
    /// 2 lookup) and the definition's name, entity, label and properties (persistent
    /// format 12, hybrid log op 17, memory snapshot 9). Later kinds take further tags.
    pub(crate) fn encode_tagged_list(defs: &[IndexDef], e: &mut Encoder) {
        let tagged: Vec<&IndexDef> = defs.iter().filter(|d| d.is_text() || d.is_lookup()).collect();
        e.len(tagged.len());
        for d in tagged {
            d.encode_tagged(e);
        }
    }

    /// One definition of [`encode_tagged_list`](Self::encode_tagged_list).
    pub(crate) fn encode_tagged(&self, e: &mut Encoder) {
        e.u8(match self.kind {
            IndexKind::Text => 1,
            IndexKind::Lookup => 2,
            _ => unreachable!("encode_tagged of a {} index", self.kind.text()),
        });
        e.str(&self.name);
        e.u8(match self.entity {
            IndexEntity::Node => 1,
            IndexEntity::Relationship => 2,
        });
        e.str(&self.label);
        e.strings(&self.properties);
    }

    pub(crate) fn decode_tagged(d: &mut Decoder) -> Result<IndexDef> {
        let tag = d.u8()?;
        let name = d.str()?;
        let entity = match d.u8()? {
            1 => IndexEntity::Node,
            2 => IndexEntity::Relationship,
            t => return Err(d.corrupt(format!("unknown index entity {t}"))),
        };
        let label = d.str()?;
        let properties = d.strings()?;
        let def = match tag {
            1 if properties.len() == 1 => IndexDef::text(name, entity, label, properties[0].clone()),
            2 if properties.is_empty() && label.is_empty() => IndexDef::lookup(name, entity),
            1 | 2 => return Err(d.corrupt(format!("bad index `{name}`"))),
            t => return Err(d.corrupt(format!("unknown index kind {t}"))),
        };
        Ok(def)
    }

    pub(crate) fn decode_tagged_list(d: &mut Decoder) -> Result<Vec<IndexDef>> {
        let n = d.len(8)?;
        (0..n).map(|_| IndexDef::decode_tagged(d)).collect()
    }
}

/// A schema constraint on the nodes with a label or the relationships of a type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstraintDef {
    pub name: String,
    pub entity: IndexEntity,
    pub kind: ConstraintKind,
    /// The label (nodes) or relationship type (relationships).
    pub label: String,
    /// One property for `NotNull` and `Typed`; one or more for `Unique` and `Key`.
    pub properties: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstraintKind {
    /// `IS UNIQUE`: no two entities with the label (type) have equal values for all of
    /// the properties (entities missing one of them are not compared).
    Unique,
    /// `IS NOT NULL`: every entity with the label (type) has the property.
    NotNull,
    /// `IS NODE KEY` / `IS RELATIONSHIP KEY`: every entity has all of the properties,
    /// and their values are unique.
    Key,
    /// `IS :: <type>`: the property, where present, has one of these types.
    Typed(PropertyType),
}

impl ConstraintKind {
    /// The Cypher spelling for `entity`: `UNIQUE`, `NOT NULL`, `NODE KEY`,
    /// `RELATIONSHIP KEY` or `PROPERTY TYPE`.
    pub fn text(self, entity: IndexEntity) -> &'static str {
        match self {
            ConstraintKind::Unique => "UNIQUE",
            ConstraintKind::NotNull => "NOT NULL",
            ConstraintKind::Key if entity == IndexEntity::Node => "NODE KEY",
            ConstraintKind::Key => "RELATIONSHIP KEY",
            ConstraintKind::Typed(_) => "PROPERTY TYPE",
        }
    }

    fn tag(self) -> u8 {
        match self {
            ConstraintKind::Unique => 1,
            ConstraintKind::NotNull => 2,
            ConstraintKind::Key => 3,
            ConstraintKind::Typed(_) => 4,
        }
    }
}

/// The property types a `Typed` constraint allows: a union of scalar types and lists of
/// them (`LIST<INTEGER NOT NULL>`), as a bit set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PropertyType(u32);

impl PropertyType {
    /// The scalar types, by bit, in the order `SHOW CONSTRAINTS` lists them.
    pub const SCALARS: [&'static str; 11] = [
        "BOOLEAN",
        "STRING",
        "INTEGER",
        "FLOAT",
        "DATE",
        "LOCAL TIME",
        "ZONED TIME",
        "LOCAL DATETIME",
        "ZONED DATETIME",
        "DURATION",
        "POINT",
    ];
    const LIST: u32 = 16;

    /// The scalar type named `name` (one of [`SCALARS`](Self::SCALARS)), or a list of it.
    pub fn scalar(name: &str, list: bool) -> Option<PropertyType> {
        let i = Self::SCALARS.iter().position(|s| *s == name)? as u32;
        Some(PropertyType(1 << (i + if list { Self::LIST } else { 0 })))
    }

    pub fn union(self, other: PropertyType) -> PropertyType {
        PropertyType(self.0 | other.0)
    }

    /// The bit of a scalar value's type, or `None` if it has none (null, maps, entities).
    fn bit(v: &Value) -> Option<u32> {
        use crate::temporal::Temporal;
        Some(match v {
            Value::Bool(_) => 0,
            Value::String(_) => 1,
            Value::Int(_) => 2,
            Value::Float(_) => 3,
            Value::Temporal(t) => match t {
                Temporal::Date(_) => 4,
                Temporal::LocalTime(_) => 5,
                Temporal::Time { .. } => 6,
                Temporal::LocalDateTime { .. } => 7,
                Temporal::DateTime { .. } => 8,
                Temporal::Duration(_) => 9,
            },
            Value::Point(_) => 10,
            _ => return None,
        })
    }

    /// Whether a property value has one of the types. An empty list has every list type;
    /// a list with nulls or mixed types has none.
    pub fn accepts(self, v: &Value) -> bool {
        match v {
            Value::List(items) => {
                let Some(first) = items.first() else {
                    return self.0 >> Self::LIST != 0;
                };
                let Some(bit) = Self::bit(first) else { return false };
                items.iter().all(|x| Self::bit(x) == Some(bit)) && self.0 & (1 << (bit + Self::LIST)) != 0
            }
            v => Self::bit(v).is_some_and(|b| self.0 & (1 << b) != 0),
        }
    }

    fn from_bits(bits: u32) -> Option<PropertyType> {
        let valid = (1u32 << Self::SCALARS.len()) - 1;
        let valid = valid | valid << Self::LIST;
        (bits != 0 && bits & !valid == 0).then_some(PropertyType(bits))
    }
}

impl std::fmt::Display for PropertyType {
    /// `INTEGER | FLOAT`, `LIST<STRING NOT NULL>`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for (shift, list) in [(0, false), (Self::LIST, true)] {
            for (i, name) in Self::SCALARS.iter().enumerate() {
                if self.0 & (1 << (i as u32 + shift)) == 0 {
                    continue;
                }
                if !first {
                    f.write_str(" | ")?;
                }
                first = false;
                if list {
                    write!(f, "LIST<{name} NOT NULL>")?;
                } else {
                    f.write_str(name)?;
                }
            }
        }
        Ok(())
    }
}

impl ConstraintDef {
    /// Whether entities need all of the properties (`NOT NULL`, `KEY`).
    pub fn requires_properties(&self) -> bool {
        matches!(self.kind, ConstraintKind::NotNull | ConstraintKind::Key)
    }

    /// Whether value combinations must be unique (`UNIQUE`, `KEY`).
    pub fn requires_unique(&self) -> bool {
        matches!(self.kind, ConstraintKind::Unique | ConstraintKind::Key)
    }

    /// Whether `other` constrains the same thing: the same kind (any property type for
    /// `Typed`), entity, label and properties.
    pub fn same_target(&self, other: &ConstraintDef) -> bool {
        std::mem::discriminant(&self.kind) == std::mem::discriminant(&other.kind)
            && self.entity == other.entity
            && self.label == other.label
            && self.properties == other.properties
    }

    /// Checks that `self` is well formed and does not clash with `existing`.
    pub fn check_new(&self, existing: &[ConstraintDef]) -> Result<()> {
        if self.properties.is_empty() {
            return Err(Error::Semantic("a constraint needs at least one property".into()));
        }
        if matches!(self.kind, ConstraintKind::NotNull | ConstraintKind::Typed(_)) && self.properties.len() > 1 {
            return Err(Error::Semantic(format!(
                "a {} constraint takes exactly one property",
                self.kind.text(self.entity)
            )));
        }
        for (i, p) in self.properties.iter().enumerate() {
            if self.properties[..i].contains(p) {
                return Err(Error::Semantic(format!(
                    "property `{p}` is repeated in the constraint"
                )));
            }
        }
        for c in existing {
            if c.name == self.name {
                return Err(Error::Constraint(format!(
                    "a constraint named `{}` already exists",
                    self.name
                )));
            }
            if c.same_target(self) {
                return Err(Error::Constraint(if c.kind == self.kind {
                    format!("an equivalent constraint `{}` already exists", c.name)
                } else {
                    format!("constraint `{}` already gives the property a type", c.name)
                }));
            }
        }
        Ok(())
    }

    /// Relationship constraints set this bit in the kind tag.
    const RELATIONSHIP: u8 = 0x10;

    pub(crate) fn encode(&self, e: &mut Encoder) {
        e.str(&self.name);
        let rel = if self.entity == IndexEntity::Relationship { Self::RELATIONSHIP } else { 0 };
        e.u8(self.kind.tag() | rel);
        if let ConstraintKind::Typed(t) = self.kind {
            e.u32(t.0);
        }
        e.str(&self.label);
        e.strings(&self.properties);
    }

    pub(crate) fn decode(d: &mut Decoder) -> Result<ConstraintDef> {
        let name = d.str()?;
        let tag = d.u8()?;
        let entity = if tag & Self::RELATIONSHIP != 0 { IndexEntity::Relationship } else { IndexEntity::Node };
        let kind = match tag & !Self::RELATIONSHIP {
            1 => ConstraintKind::Unique,
            2 => ConstraintKind::NotNull,
            3 => ConstraintKind::Key,
            4 => {
                let bits = d.u32()?;
                match PropertyType::from_bits(bits) {
                    Some(t) => ConstraintKind::Typed(t),
                    None => return Err(d.corrupt(format!("unknown property types {bits:#x}"))),
                }
            }
            t => return Err(d.corrupt(format!("unknown constraint kind {t}"))),
        };
        Ok(ConstraintDef {
            name,
            entity,
            kind,
            label: d.str()?,
            properties: d.strings()?,
        })
    }

    /// Reads a list written by [`encode_list`](Self::encode_list), checking names.
    pub(crate) fn decode_list(d: &mut Decoder) -> Result<Vec<ConstraintDef>> {
        let n = d.len(4)?;
        let mut out: Vec<ConstraintDef> = Vec::with_capacity(n);
        for _ in 0..n {
            let c = ConstraintDef::decode(d)?;
            if out.iter().any(|o| o.name == c.name) {
                return Err(d.corrupt(format!("duplicate constraint `{}`", c.name)));
            }
            out.push(c);
        }
        Ok(out)
    }

    pub(crate) fn encode_list(list: &[ConstraintDef], e: &mut Encoder) {
        e.len(list.len());
        for c in list {
            c.encode(e);
        }
    }
}

/// Property values an index range seek looks for ([`GraphStore::index_range`]).
#[derive(Clone, Debug, PartialEq)]
pub enum ValueRange {
    /// Numbers between two numbers, or strings between two strings (Cypher `<`, `<=`,
    /// `>`, `>=`). A missing bound is `Unbounded`; the bounds present are of one kind.
    Between { lower: Bound<Value>, upper: Bound<Value> },
    /// Strings that start with the prefix (Cypher `STARTS WITH`).
    Prefix(String),
}

impl ValueRange {
    /// The bounds of a `Between`, if both present ones are numbers (`Some(true)`) or
    /// strings (`Some(false)`); `None` if they mix kinds (nothing can match) or there are
    /// none.
    pub fn numeric(&self) -> Option<bool> {
        let ValueRange::Between { lower, upper } = self else {
            return Some(false);
        };
        let kind = |b: &Bound<Value>| match b {
            Bound::Included(v) | Bound::Excluded(v) => match v {
                Value::Int(_) | Value::Float(_) => Some(Some(true)),
                Value::String(_) => Some(Some(false)),
                _ => Some(None),
            },
            Bound::Unbounded => None,
        };
        match (kind(lower), kind(upper)) {
            (Some(a), Some(b)) if a == b => a,
            (Some(_), Some(_)) => None,
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => None,
        }
    }
}

/// Size of a property index, for the planner's estimates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Indexed entities (labelled nodes, or relationships of the type, that have every
    /// indexed property).
    pub entries: usize,
    /// Distinct indexed values or value combinations (`1` and `1.0` count once).
    pub distinct: usize,
}
