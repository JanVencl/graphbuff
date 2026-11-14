//! Compressed sparse row (CSR) export of a store's adjacency.
//!
//! [`Csr::build`] reads any [`GraphStore`] once and lays its relationships out as flat
//! arrays, the form GPU kernels and graph algorithms work on:
//!
//! * nodes get dense indexes `0..n` in ascending [`NodeId`] order,
//! * relationship types get dense ids `0..t` in the order of
//!   [`GraphStore::relationship_types`] (types with no relationships are left out),
//! * per type and per direction ([`Direction::Outgoing`], [`Direction::Incoming`]) a
//!   [`CsrAdjacency`] holds `offsets` (`n + 1` entries), and per entry the node index on
//!   the other end (`targets`) and the relationship id (`rels`). The entries of node `i`
//!   are `offsets[i]..offsets[i + 1]`, sorted by target and then by relationship id.
//!
//! The export is read-only and does not follow later writes: build it from a
//! [`snapshot`](GraphStore::snapshot) and rebuild it after commits
//! (`graphbuff::Database::csr` caches one per committed state).
//!
//! Indexes are `u32`, as GPU kernels prefer: a store with more than `u32::MAX - 1`
//! nodes, or a type with more than `u32::MAX` relationships, is `Unsupported`.

use std::collections::HashMap;

use crate::{Direction, Error, Expansion, GraphStore, NodeId, RelId, Result};

/// A compressed sparse row view of a store's relationships, per type and direction.
#[derive(Clone, Debug, Default)]
pub struct Csr {
    /// Distinct for every [`Csr::build`] (clones share it; `0` for the empty default), so
    /// a backend can cache device copies by it. Not part of equality.
    id: u64,
    nodes: Vec<NodeId>,
    index: NodeIndex,
    types: Vec<String>,
    out: Vec<CsrAdjacency>,
    inc: Vec<CsrAdjacency>,
    relationships: usize,
}

impl PartialEq for Csr {
    fn eq(&self, o: &Csr) -> bool {
        (&self.nodes, &self.index, &self.types, &self.out, &self.inc, self.relationships)
            == (&o.nodes, &o.index, &o.types, &o.out, &o.inc, o.relationships)
    }
}

impl Eq for Csr {}

fn next_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The relationships of one type in one direction, in compressed sparse row form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CsrAdjacency {
    offsets: Vec<u32>,
    targets: Vec<u32>,
    rels: Vec<RelId>,
}

/// Node id → dense index: a direct table while ids are compact, a hash map otherwise.
#[derive(Clone, Debug, PartialEq, Eq)]
enum NodeIndex {
    Table(Vec<u32>),
    Map(HashMap<NodeId, u32>),
}

impl Default for NodeIndex {
    fn default() -> Self {
        NodeIndex::Table(Vec::new())
    }
}

const ABSENT: u32 = u32::MAX;

impl NodeIndex {
    /// Indexes `nodes` (ascending): a table when the largest id is at most about four
    /// times the node count, so deleted ids cost little memory.
    fn new(nodes: &[NodeId]) -> NodeIndex {
        let max = nodes.last().map_or(0, |n| n.0);
        if max <= 4 * nodes.len() as u64 + 1024 {
            let mut table = vec![ABSENT; max as usize + 1];
            for (i, n) in nodes.iter().enumerate() {
                table[n.0 as usize] = i as u32;
            }
            NodeIndex::Table(table)
        } else {
            NodeIndex::Map(nodes.iter().enumerate().map(|(i, n)| (*n, i as u32)).collect())
        }
    }

    fn get(&self, id: NodeId) -> Option<u32> {
        match self {
            NodeIndex::Table(t) => {
                usize::try_from(id.0).ok().and_then(|i| t.get(i)).copied().filter(|&i| i != ABSENT)
            }
            NodeIndex::Map(m) => m.get(&id).copied(),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            NodeIndex::Table(t) => t.len() * 4,
            // Key, value and about one control byte per bucket, at the map's capacity.
            NodeIndex::Map(m) => m.capacity() * 13,
        }
    }
}

/// One relationship read from the store: source index, target index, id.
struct Edge {
    src: u32,
    dst: u32,
    rel: RelId,
}

/// Up to this many relationship types, [`Csr::build`] reads the adjacency once per type.
const TYPED_PASSES: usize = 4;

impl Csr {
    /// Reads every node and relationship of `store` into a new export. Call it on a
    /// snapshot (or a store no one writes) so that the result is consistent.
    pub fn build<S: GraphStore + ?Sized>(store: &S) -> Result<Csr> {
        let mut nodes = Vec::with_capacity(store.node_count());
        for n in store.all_nodes() {
            nodes.push(n?);
        }
        nodes.sort_unstable();
        nodes.dedup();
        if nodes.len() >= ABSENT as usize {
            return Err(Error::Unsupported(format!(
                "a CSR export holds at most {} nodes, the store has {}",
                ABSENT - 1,
                nodes.len()
            )));
        }
        let index = NodeIndex::new(&nodes);

        // Every relationship is read once, from the outgoing list of its start node. With
        // few types, one pass per type needs no type lookups; with many, one untyped pass
        // avoids walking each node's relationships once per type.
        let types: Vec<String> = store
            .relationship_types()
            .into_iter()
            .filter(|t| store.relationship_type_count(t) > 0)
            .collect();
        let other = |e: Expansion| {
            index.get(e.other).ok_or_else(|| {
                Error::Storage(format!(
                    "relationship {} ends at node {}, which the node scan did not return",
                    e.rel.0, e.other.0
                ))
            })
        };
        let mut edges: Vec<Vec<Edge>> = types.iter().map(|_| Vec::new()).collect();
        if types.len() <= TYPED_PASSES {
            for (ty, name) in types.iter().enumerate() {
                let filter = std::slice::from_ref(name);
                for (src, &node) in nodes.iter().enumerate() {
                    for e in store.expand(node, Direction::Outgoing, filter) {
                        let e = e?;
                        edges[ty].push(Edge { src: src as u32, dst: other(e)?, rel: e.rel });
                    }
                }
            }
        } else {
            let ids: HashMap<&str, usize> = types.iter().enumerate().map(|(i, t)| (t.as_str(), i)).collect();
            for (src, &node) in nodes.iter().enumerate() {
                for e in store.expand(node, Direction::Outgoing, &[]) {
                    let e = e?;
                    let name = store.relationship_type(e.rel)?;
                    let Some(&ty) = name.as_deref().and_then(|t| ids.get(t)) else {
                        return Err(Error::Storage(format!(
                            "relationship {} has type {name:?}, which the store does not count",
                            e.rel.0
                        )));
                    };
                    edges[ty].push(Edge { src: src as u32, dst: other(e)?, rel: e.rel });
                }
            }
        }
        let total: usize = edges.iter().map(Vec::len).sum();
        if total != store.relationship_count() {
            return Err(Error::Storage(format!(
                "the adjacency lists hold {total} relationships, the store counts {}",
                store.relationship_count()
            )));
        }

        // Edges arrive grouped by source in ascending order: sorting each row by target
        // and relationship orders the outgoing layout, and a stable counting sort on the
        // target gives the incoming one.
        let n = nodes.len();
        let mut out = Vec::with_capacity(types.len());
        let mut inc = Vec::with_capacity(types.len());
        for (name, mut list) in types.iter().zip(edges) {
            if list.len() > u32::MAX as usize {
                return Err(Error::Unsupported(format!(
                    "a CSR export holds at most {} relationships per type, `{name}` has {}",
                    u32::MAX,
                    list.len()
                )));
            }
            for row in list.chunk_by_mut(|a, b| a.src == b.src) {
                if row.len() > 1 {
                    row.sort_unstable_by_key(|e| (e.dst, e.rel));
                }
            }
            out.push(CsrAdjacency::from_sorted(n, list.iter().map(|e| (e.src, e.dst, e.rel))));
            inc.push(CsrAdjacency::transpose(n, &list));
        }

        Ok(Csr { id: next_id(), nodes, index, types, out, inc, relationships: total })
    }

    /// An id distinct for every export [`build`](Self::build) made in this process (a
    /// clone keeps it): device-side caches key on it.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Nodes in the export.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Relationships in the export (each counted once).
    pub fn relationship_count(&self) -> usize {
        self.relationships
    }

    /// The node with dense index `index`.
    pub fn node_id(&self, index: u32) -> NodeId {
        self.nodes[index as usize]
    }

    /// The dense index of a node, or `None` if the export does not hold it.
    pub fn node_index(&self, id: NodeId) -> Option<u32> {
        self.index.get(id)
    }

    /// All nodes, by dense index.
    pub fn nodes(&self) -> &[NodeId] {
        &self.nodes
    }

    /// Relationship type names, by type id.
    pub fn types(&self) -> &[String] {
        &self.types
    }

    /// The id of a relationship type, or `None` if no relationship has it.
    pub fn type_id(&self, name: &str) -> Option<u32> {
        self.types.iter().position(|t| t == name).map(|i| i as u32)
    }

    /// The relationships of a type in one direction. `Direction::Both` is not a stored
    /// layout: it panics, like an out-of-range type id.
    pub fn adjacency(&self, ty: u32, dir: Direction) -> &CsrAdjacency {
        match dir {
            Direction::Outgoing => &self.out[ty as usize],
            Direction::Incoming => &self.inc[ty as usize],
            Direction::Both => panic!("a CSR adjacency is either outgoing or incoming"),
        }
    }

    /// The relationships of a node, like [`GraphStore::expand`]: of the given types
    /// (empty = any), with self-loops once under `Direction::Both`. Ordered by type id,
    /// then (for `Both`) outgoing before incoming, then target and relationship id.
    /// Types the export does not hold, and nodes it does not hold, have none.
    pub fn expand(&self, node: NodeId, dir: Direction, types: &[String]) -> Vec<Expansion> {
        let mut out = Vec::new();
        let Some(i) = self.node_index(node) else {
            return out;
        };
        for t in self.type_ids(types) {
            if matches!(dir, Direction::Outgoing | Direction::Both) {
                let a = &self.out[t as usize];
                let r = a.range(i);
                out.extend(r.map(|k| Expansion { rel: a.rels[k], other: self.nodes[a.targets[k] as usize] }));
            }
            if matches!(dir, Direction::Incoming | Direction::Both) {
                let a = &self.inc[t as usize];
                for k in a.range(i) {
                    if dir == Direction::Both && a.targets[k] == i {
                        continue; // a self-loop, already reported as outgoing
                    }
                    out.push(Expansion { rel: a.rels[k], other: self.nodes[a.targets[k] as usize] });
                }
            }
        }
        out
    }

    /// The number of relationships [`expand`](Self::expand) returns, without building
    /// them.
    pub fn degree(&self, node: NodeId, dir: Direction, types: &[String]) -> usize {
        let Some(i) = self.node_index(node) else {
            return 0;
        };
        let mut d = 0;
        for t in self.type_ids(types) {
            let (o, n) = (&self.out[t as usize], &self.inc[t as usize]);
            d += match dir {
                Direction::Outgoing => o.degree(i),
                Direction::Incoming => n.degree(i),
                Direction::Both => o.degree(i) + n.degree(i) - o.neighbors(i).iter().filter(|&&x| x == i).count(),
            };
        }
        d
    }

    /// The type ids of `types` (empty = every type), ascending and without repeats.
    /// Names the export does not hold are left out, so they match nothing.
    pub fn type_ids(&self, types: &[String]) -> Vec<u32> {
        if types.is_empty() {
            return (0..self.types.len() as u32).collect();
        }
        let mut ids: Vec<u32> = types.iter().filter_map(|t| self.type_id(t)).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Approximate heap memory the export holds, in bytes.
    pub fn memory_bytes(&self) -> usize {
        let adj: usize = self.out.iter().chain(&self.inc).map(CsrAdjacency::memory_bytes).sum();
        self.nodes.len() * 8
            + self.index.bytes()
            + self.types.iter().map(|t| t.len() + 24).sum::<usize>()
            + adj
    }
}

impl CsrAdjacency {
    /// From `(source, target, rel)` entries sorted by source, then target and rel.
    fn from_sorted(n: usize, entries: impl ExactSizeIterator<Item = (u32, u32, RelId)>) -> CsrAdjacency {
        let mut offsets = vec![0u32; n + 1];
        let mut targets = Vec::with_capacity(entries.len());
        let mut rels = Vec::with_capacity(entries.len());
        for (src, dst, rel) in entries {
            offsets[src as usize + 1] += 1;
            targets.push(dst);
            rels.push(rel);
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        CsrAdjacency { offsets, targets, rels }
    }

    /// The incoming layout of edges sorted by (source, target, rel): a stable counting
    /// sort on the target keeps each row sorted by source, then rel.
    fn transpose(n: usize, edges: &[Edge]) -> CsrAdjacency {
        let mut offsets = vec![0u32; n + 1];
        for e in edges {
            offsets[e.dst as usize + 1] += 1;
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        let mut next: Vec<u32> = offsets[..n].to_vec();
        let mut targets = vec![0u32; edges.len()];
        let mut rels = vec![RelId(0); edges.len()];
        for e in edges {
            let k = &mut next[e.dst as usize];
            targets[*k as usize] = e.src;
            rels[*k as usize] = e.rel;
            *k += 1;
        }
        CsrAdjacency { offsets, targets, rels }
    }

    /// Row offsets: the entries of node `i` are `offsets[i]..offsets[i + 1]`.
    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    /// The dense index of the node on the other end, per entry.
    pub fn targets(&self) -> &[u32] {
        &self.targets
    }

    /// The relationship, per entry.
    pub fn rels(&self) -> &[RelId] {
        &self.rels
    }

    /// Entries in this layout.
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// The entries of node index `i`.
    pub(crate) fn range(&self, i: u32) -> std::ops::Range<usize> {
        self.offsets[i as usize] as usize..self.offsets[i as usize + 1] as usize
    }

    /// The other ends of node index `i`'s relationships, sorted.
    pub fn neighbors(&self, i: u32) -> &[u32] {
        &self.targets[self.range(i)]
    }

    /// The relationships of node index `i`, in the order of [`neighbors`](Self::neighbors).
    pub fn rels_of(&self, i: u32) -> &[RelId] {
        &self.rels[self.range(i)]
    }

    /// The number of entries of node index `i`.
    pub fn degree(&self, i: u32) -> usize {
        (self.offsets[i as usize + 1] - self.offsets[i as usize]) as usize
    }

    fn memory_bytes(&self) -> usize {
        self.offsets.len() * 4 + self.targets.len() * 4 + self.rels.len() * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryGraph;
    use std::collections::BTreeMap;

    fn rel(g: &mut MemoryGraph, a: NodeId, b: NodeId, t: &str) -> RelId {
        g.create_relationship(a, b, t, BTreeMap::new()).unwrap()
    }

    #[test]
    fn layout() {
        let mut g = MemoryGraph::new();
        let n: Vec<NodeId> = (0..4).map(|_| g.create_node(&[], BTreeMap::new()).unwrap()).collect();
        let r1 = rel(&mut g, n[0], n[2], "A");
        let r2 = rel(&mut g, n[0], n[1], "A");
        let r3 = rel(&mut g, n[1], n[1], "B");
        let r4 = rel(&mut g, n[3], n[1], "A");
        let csr = Csr::build(&g).unwrap();
        assert_eq!(csr.node_count(), 4);
        assert_eq!(csr.relationship_count(), 4);
        assert_eq!(csr.types(), ["A", "B"]);
        let a = csr.adjacency(0, Direction::Outgoing);
        assert_eq!(a.offsets(), [0, 2, 2, 2, 3]);
        assert_eq!(a.targets(), [1, 2, 1]);
        assert_eq!(a.rels(), [r2, r1, r4]);
        let a = csr.adjacency(0, Direction::Incoming);
        assert_eq!(a.offsets(), [0, 0, 2, 3, 3]);
        assert_eq!(a.targets(), [0, 3, 0]);
        assert_eq!(a.rels(), [r2, r4, r1]);
        assert_eq!(csr.degree(n[1], Direction::Both, &[]), 3);
        assert_eq!(csr.degree(n[1], Direction::Both, &["B".into()]), 1);
        assert_eq!(
            csr.expand(n[1], Direction::Both, &["B".into(), "C".into()]),
            [Expansion { rel: r3, other: n[1] }]
        );
        assert!(csr.expand(NodeId(99), Direction::Both, &[]).is_empty());
    }

    #[test]
    fn sparse_ids_use_a_map() {
        let mut g = MemoryGraph::new();
        let a = g.create_node(&[], BTreeMap::new()).unwrap();
        let mut last = a;
        for _ in 0..5000 {
            last = g.create_node(&[], BTreeMap::new()).unwrap();
            if last != a {
                g.delete_node(last, false).unwrap();
            }
        }
        let b = g.create_node(&[], BTreeMap::new()).unwrap();
        let r = rel(&mut g, b, a, "T");
        let csr = Csr::build(&g).unwrap();
        assert!(matches!(csr.index, NodeIndex::Map(_)));
        assert_eq!(csr.node_index(b), Some(1));
        assert_eq!(csr.node_index(last), None);
        assert_eq!(csr.expand(a, Direction::Incoming, &[]), [Expansion { rel: r, other: b }]);
    }
}
