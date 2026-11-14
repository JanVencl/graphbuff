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
    fn checkpoint(&mut self) -> Result<()> {
        (**self).checkpoint()
    }
    fn describe(&self) -> String {
        (**self).describe()
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

/// A property index: on one or more properties of the nodes with a label, or of the
/// relationships of a type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexDef {
    pub name: String,
    pub entity: IndexEntity,
    /// The label (node index) or relationship type (relationship index).
    pub label: String,
    /// One property, or several for a composite index (in order).
    pub properties: Vec<String>,
}

impl IndexDef {
    /// A single-property node index.
    pub fn node(name: impl Into<String>, label: impl Into<String>, property: impl Into<String>) -> IndexDef {
        IndexDef {
            name: name.into(),
            entity: IndexEntity::Node,
            label: label.into(),
            properties: vec![property.into()],
        }
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
            properties: vec![property.into()],
        }
    }

    /// Whether `self` indexes exactly these properties of `label` (a label or type).
    pub fn covers(&self, entity: IndexEntity, label: &str, properties: &[String]) -> bool {
        self.entity == entity && self.label == label && self.properties == properties
    }

    /// The indexed schema in Cypher style: `:Label(a, b)` or `[:TYPE](a)`.
    pub fn target(&self) -> String {
        let props = self.properties.join(", ");
        match self.entity {
            IndexEntity::Node => format!(":{}({props})", self.label),
            IndexEntity::Relationship => format!("[:{}]({props})", self.label),
        }
    }

    /// Checks that `self` is well formed and does not clash with `existing`.
    pub fn check_new(&self, existing: &[IndexDef]) -> Result<()> {
        if self.properties.is_empty() {
            return Err(Error::Semantic("an index needs at least one property".into()));
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
            if ix.covers(self.entity, &self.label, &self.properties) {
                return Err(Error::Constraint(format!(
                    "an index on {} already exists",
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
            properties,
        })
    }

    /// Decodes a single-property node index as older formats wrote it: name, label,
    /// property.
    pub(crate) fn decode_v1(d: &mut Decoder) -> Result<IndexDef> {
        let name = d.str()?;
        let label = d.str()?;
        Ok(IndexDef::node(name, label, d.str()?))
    }
}

/// A schema constraint on the nodes with a label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstraintDef {
    pub name: String,
    pub kind: ConstraintKind,
    pub label: String,
    /// One property for `NotNull`; one or more for `Unique` and `NodeKey`.
    pub properties: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstraintKind {
    /// `IS UNIQUE`: no two nodes with the label have equal values for all of the
    /// properties (nodes missing one of them are not compared).
    Unique,
    /// `IS NOT NULL`: every node with the label has the property.
    NotNull,
    /// `IS NODE KEY`: every node with the label has all of the properties, and their
    /// values are unique.
    NodeKey,
}

impl ConstraintKind {
    /// The Cypher spelling: `UNIQUE`, `NOT NULL` or `NODE KEY`.
    pub fn text(self) -> &'static str {
        match self {
            ConstraintKind::Unique => "UNIQUE",
            ConstraintKind::NotNull => "NOT NULL",
            ConstraintKind::NodeKey => "NODE KEY",
        }
    }

    fn tag(self) -> u8 {
        match self {
            ConstraintKind::Unique => 1,
            ConstraintKind::NotNull => 2,
            ConstraintKind::NodeKey => 3,
        }
    }
}

impl ConstraintDef {
    /// Whether nodes need all of the properties (`NOT NULL`, `NODE KEY`).
    pub fn requires_properties(&self) -> bool {
        matches!(self.kind, ConstraintKind::NotNull | ConstraintKind::NodeKey)
    }

    /// Whether value combinations must be unique (`UNIQUE`, `NODE KEY`).
    pub fn requires_unique(&self) -> bool {
        matches!(self.kind, ConstraintKind::Unique | ConstraintKind::NodeKey)
    }

    /// Checks that `self` is well formed and does not clash with `existing`.
    pub fn check_new(&self, existing: &[ConstraintDef]) -> Result<()> {
        if self.properties.is_empty() {
            return Err(Error::Semantic("a constraint needs at least one property".into()));
        }
        if self.kind == ConstraintKind::NotNull && self.properties.len() > 1 {
            return Err(Error::Semantic(
                "a NOT NULL constraint takes exactly one property".into(),
            ));
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
            if c.kind == self.kind && c.label == self.label && c.properties == self.properties {
                return Err(Error::Constraint(format!(
                    "an equivalent constraint `{}` already exists",
                    c.name
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn encode(&self, e: &mut Encoder) {
        e.str(&self.name);
        e.u8(self.kind.tag());
        e.str(&self.label);
        e.strings(&self.properties);
    }

    pub(crate) fn decode(d: &mut Decoder) -> Result<ConstraintDef> {
        let name = d.str()?;
        let kind = match d.u8()? {
            1 => ConstraintKind::Unique,
            2 => ConstraintKind::NotNull,
            3 => ConstraintKind::NodeKey,
            t => return Err(d.corrupt(format!("unknown constraint kind {t}"))),
        };
        Ok(ConstraintDef {
            name,
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
