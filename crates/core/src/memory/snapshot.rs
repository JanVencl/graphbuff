//! Serializing a whole [`MemoryGraph`] and loading it back.
//!
//! The snapshot stores the interner tables, the live node and relationship records with
//! their ids, the slab lengths (so ids are never reused after a reload) and the property
//! index and constraint definitions. Adjacency lists, the label index and index entries
//! are derived data and are rebuilt on load.

use std::collections::HashMap;
use std::sync::Arc;

use super::cow::{PSet, PVec};
use super::{Interner, MemoryGraph, NodeRecord, Props, RelRecord, Token};
use crate::codec::{Decoder, Encoder};
use crate::{ConstraintDef, GraphStore, IndexDef, NodeId, RelId, Result};

/// Snapshot layout version written by [`MemoryGraph::encode_snapshot`]: 2 added the
/// constraint definitions at the end, 3 full index definitions (relationship and
/// composite indexes), 4 temporal property values (laid out as 3), 5 points (likewise),
/// 6 vector index definitions at the end, 7 full-text index definitions after them, 8
/// point index definitions after those, 9 text and lookup index definitions after those.
pub(crate) const SNAPSHOT_VERSION: u8 = 9;

impl Interner {
    pub(crate) fn from_names(names: Vec<String>, d: &Decoder) -> Result<Self> {
        let mut ids = HashMap::with_capacity(names.len());
        for (i, n) in names.iter().enumerate() {
            if ids.insert(n.clone(), i as Token).is_some() {
                return Err(d.corrupt(format!("duplicate interned name `{n}`")));
            }
        }
        Ok(Interner { names, ids })
    }
}

fn encode_props(e: &mut Encoder, props: &Props) {
    e.len(props.len());
    for (k, v) in props {
        e.u32(*k);
        e.value(v);
    }
}

fn decode_token(d: &mut Decoder, table: &Interner, what: &str) -> Result<Token> {
    let t = d.u32()?;
    if (t as usize) < table.names.len() {
        Ok(t)
    } else {
        Err(d.corrupt(format!("{what} token {t} is out of range")))
    }
}

fn decode_props(d: &mut Decoder, keys: &Interner) -> Result<Props> {
    let n = d.len(5)?;
    let mut props = Props::with_capacity(n);
    for _ in 0..n {
        let k = decode_token(d, keys, "property key")?;
        let v = d.value()?;
        if !v.is_storable_property() || v.is_null() {
            return Err(d.corrupt(format!("invalid stored {} property", v.type_name())));
        }
        props.insert(k, v);
    }
    Ok(props)
}

/// Reads an id that must be below `bound` and above `prev` (records are stored in order).
fn decode_id(d: &mut Decoder, bound: usize, prev: Option<u64>) -> Result<u64> {
    let id = d.u64()?;
    if id as usize >= bound || prev.is_some_and(|p| id <= p) {
        return Err(d.corrupt(format!("record id {id} is out of order or out of range")));
    }
    Ok(id)
}

impl MemoryGraph {
    /// Appends the graph's committed state to `e`. Must not be called mid-transaction.
    pub(crate) fn encode_snapshot(&self, e: &mut Encoder) {
        debug_assert!(self.saved.is_none(), "snapshot taken inside a transaction");
        e.strings(&self.labels.names);
        e.strings(&self.rel_types.names);
        e.strings(&self.keys.names);

        e.len(self.nodes.len());
        e.len(self.node_count);
        for (i, n) in self.nodes.iter().enumerate() {
            let Some(n) = n else { continue };
            e.u64(i as u64);
            e.len(n.labels.len());
            for l in n.labels.iter() {
                e.u32(*l);
            }
            encode_props(e, &n.props);
        }

        e.len(self.rels.len());
        e.len(self.rel_count);
        for (i, r) in self.rels.iter().enumerate() {
            let Some(r) = r else { continue };
            e.u64(i as u64);
            e.u64(r.start.0);
            e.u64(r.end.0);
            e.u32(r.rel_type);
            encode_props(e, &r.props);
        }

        e.len(self.indexes.len());
        for ix in &self.indexes {
            ix.def.encode(e);
        }

        ConstraintDef::encode_list(&self.constraints, e);
        IndexDef::encode_vector_list(&self.engine_indexes, e);
        IndexDef::encode_fulltext_list(&self.engine_indexes, e);
        IndexDef::encode_point_list(&self.engine_indexes, e);
        IndexDef::encode_tagged_list(&self.engine_indexes, e);
    }

    /// Rebuilds a graph from data written by [`encode_snapshot`](Self::encode_snapshot),
    /// in layout `version` (1 to [`SNAPSHOT_VERSION`]).
    pub(crate) fn decode_snapshot(d: &mut Decoder, version: u8) -> Result<MemoryGraph> {
        Self::decode_snapshot_limited(d, version, usize::MAX)
    }

    /// [`decode_snapshot`](Self::decode_snapshot), failing when the snapshot asks for
    /// more than `max_slots` node or relationship slots (the fuzz target's bound).
    ///
    /// The slot counts come from the file and are allocated before the records are read
    /// (deleted ids keep their slots), so a corrupt count must fail as an error, not abort
    /// the process on an allocation it cannot make.
    pub(crate) fn decode_snapshot_limited(d: &mut Decoder, version: u8, max_slots: usize) -> Result<MemoryGraph> {
        let labels = Interner::from_names(d.strings()?, d)?;
        let rel_types = Interner::from_names(d.strings()?, d)?;
        let keys = Interner::from_names(d.strings()?, d)?;

        let slab = d.u64()? as usize;
        let node_count = d.len(8 + 8 + 8)?;
        if node_count > slab {
            return Err(d.corrupt("more live nodes than node slots"));
        }
        let mut nodes: Vec<Option<NodeRecord>> = slots(d, slab, max_slots, "node")?;
        nodes.resize_with(slab, || None);
        let mut label_index: HashMap<Token, PSet<NodeId>> = HashMap::new();
        let mut prev = None;
        for _ in 0..node_count {
            let id = decode_id(d, slab, prev)?;
            prev = Some(id);
            let n = d.len(4)?;
            let mut node_labels = Vec::with_capacity(n);
            for _ in 0..n {
                let l = decode_token(d, &labels, "label")?;
                if !node_labels.contains(&l) {
                    node_labels.push(l);
                }
                label_index.entry(l).or_default().insert(NodeId(id));
            }
            let props = decode_props(d, &keys)?;
            nodes[id as usize] = Some(NodeRecord {
                labels: node_labels.into(),
                props,
                ..Default::default()
            });
        }

        let slab = d.u64()? as usize;
        let rel_count = d.len(8 * 3 + 4 + 8)?;
        if rel_count > slab {
            return Err(d.corrupt("more live relationships than relationship slots"));
        }
        let mut nodes: PVec<Option<NodeRecord>> = nodes.into_iter().collect();
        let mut rels: Vec<Option<RelRecord>> = slots(d, slab, max_slots, "relationship")?;
        rels.resize_with(slab, || None);
        let mut prev = None;
        for _ in 0..rel_count {
            let id = decode_id(d, slab, prev)?;
            prev = Some(id);
            let (start, end) = (NodeId(d.u64()?), NodeId(d.u64()?));
            for n in [start, end] {
                if nodes.get(n.0 as usize).is_none_or(Option::is_none) {
                    return Err(d.corrupt(format!("relationship {id} points at missing node {n}")));
                }
            }
            let rel_type = decode_token(d, &rel_types, "relationship type")?;
            let props = decode_props(d, &keys)?;
            let rec = RelRecord {
                start,
                end,
                rel_type,
                props,
            };
            super::link_into(&mut nodes, RelId(id), &rec);
            rels[id as usize] = Some(rec);
        }

        let mut type_counts: HashMap<Token, usize> = HashMap::new();
        for r in rels.iter().flatten() {
            *type_counts.entry(r.rel_type).or_default() += 1;
        }
        let mut g = MemoryGraph {
            nodes,
            rels: rels.into_iter().collect(),
            node_count,
            rel_count,
            labels: Arc::new(labels),
            rel_types: Arc::new(rel_types),
            keys: Arc::new(keys),
            label_index,
            type_counts,
            indexes: Vec::new(),
            engine_indexes: Vec::new(),
            constraints: Vec::new(),
            saved: None,
            changes: Default::default(),
        };

        let n = d.len(8 + 4 + 4)?;
        for _ in 0..n {
            let def = if version >= 3 {
                IndexDef::decode(d)?
            } else {
                let name = d.str()?;
                let label = decode_token(d, &g.labels, "label")?;
                let key = decode_token(d, &g.keys, "property key")?;
                IndexDef::node(name, g.labels.name(label), g.keys.name(key))
            };
            let existing: Vec<IndexDef> = g.indexes.iter().map(|ix| ix.def.clone()).collect();
            def.check_new(&existing)
                .map_err(|e| d.corrupt(format!("bad index definition: {e}")))?;
            let ix = g.build_index(def);
            g.indexes.push(ix);
        }
        if version >= 2 {
            g.constraints = ConstraintDef::decode_list(d)?;
        }
        if version >= 6 {
            let mut defs = IndexDef::decode_vector_list(d)?;
            if version >= 7 {
                defs.extend(IndexDef::decode_fulltext_list(d)?);
            }
            if version >= 8 {
                defs.extend(IndexDef::decode_point_list(d)?);
            }
            if version >= 9 {
                defs.extend(IndexDef::decode_tagged_list(d)?);
            }
            for def in defs {
                def.check_new(&g.indexes()).map_err(|e| d.corrupt(format!("bad index definition: {e}")))?;
                g.engine_indexes.push(def);
            }
        }
        Ok(g)
    }
}

/// An empty vector with room for `n` slots, or a corruption error if that is more than
/// `max` or does not fit in memory.
fn slots<T>(d: &Decoder, n: usize, max: usize, what: &str) -> Result<Vec<T>> {
    let mut v = Vec::new();
    if n > max || v.try_reserve_exact(n).is_err() {
        return Err(d.corrupt(format!("{n} {what} slots do not fit in memory")));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{Direction, GraphStore, Value};

    #[test]
    fn snapshot_round_trip() {
        let mut g = MemoryGraph::new();
        let mut p = BTreeMap::new();
        p.insert("name".to_string(), Value::from("Ann"));
        let a = g
            .create_node(&["Person".into(), "Admin".into()], p)
            .unwrap();
        let b = g.create_node(&["Person".into()], BTreeMap::new()).unwrap();
        let gone = g.create_node(&[], BTreeMap::new()).unwrap();
        let r = g
            .create_relationship(a, b, "KNOWS", BTreeMap::new())
            .unwrap();
        g.set_relationship_property(r, "since", 2020.into())
            .unwrap();
        g.create_index(IndexDef::node("pn", "Person", "name")).unwrap();
        g.create_index(IndexDef::relationship("ks", "KNOWS", "since")).unwrap();
        g.create_index(IndexDef {
            name: "pnk".into(),
            entity: crate::IndexEntity::Node,
            label: "Person".into(),
            properties: vec!["name".into(), "kind".into()],
            extra_labels: Vec::new(),
            kind: Default::default(),
        })
        .unwrap();
        g.delete_node(gone, false).unwrap();

        let mut e = Encoder::new();
        g.encode_snapshot(&mut e);
        let mut d = Decoder::new(&e.buf, "snapshot");
        let mut h = MemoryGraph::decode_snapshot(&mut d, SNAPSHOT_VERSION).unwrap();
        assert!(d.is_empty());

        assert_eq!(h.node_count(), 2);
        assert_eq!(
            h.node_labels(a).unwrap(),
            vec!["Person".to_string(), "Admin".to_string()]
        );
        assert_eq!(h.relationship_property(r, "since").unwrap(), Some(2020.into()));
        assert_eq!(h.expand_all(b, Direction::Incoming, &[]).unwrap().len(), 1);
        assert_eq!(h.index_seek("Person", "name", &"Ann".into()).unwrap(), Some(vec![a]));
        assert_eq!(h.indexes(), g.indexes());
        let since = ["since".to_string()];
        assert_eq!(
            h.schema_index_seek(crate::IndexEntity::Relationship, "KNOWS", &since, &[2020.into()])
                .unwrap(),
            Some(vec![r.0])
        );
        assert!(!h.node_exists(gone).unwrap());
        // Ids keep increasing after a reload: the deleted slot is not reused.
        let c = h.create_node(&[], BTreeMap::new()).unwrap();
        assert!(c > gone);
    }

    /// Found by the `storage` fuzz target: a node slot count of about 2^40 made the
    /// decoder abort on the allocation instead of failing.
    #[test]
    fn huge_slot_count_is_an_error() {
        let body = [0x02, 0x09, 0x05, 0x00, 0x01, 0x18, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x08, 0xb3, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mut d = Decoder::new(&body, "snapshot");
        d.u64().unwrap();
        let e = MemoryGraph::decode_snapshot(&mut d, SNAPSHOT_VERSION).err().expect("an error");
        assert!(e.to_string().contains("slots do not fit"), "{e}");
    }
}
