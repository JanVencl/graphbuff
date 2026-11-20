//! Bulk loading: fills a new, empty store without the redo log.
//!
//! A [`BulkLoader`] writes records through the page cache with no transaction open, so
//! there are no before-images, no log records and no transaction size cap: pages go to
//! the data files when they are evicted, and at [`BulkLoader::finish`]. Relationships are
//! linked (and nodes made dense) by the same code as ordinary writes. The label index and
//! the property indexes are not maintained per entity: [`finish`](BulkLoader::finish)
//! builds them bottom-up from sorted keys, with full pages.
//!
//! Nothing is durable until `finish` returns. While loading, the directory holds an
//! `IMPORTING` marker, and [`PersistentGraph::open`] refuses a directory that still has
//! one: an import that failed or crashed must be deleted and run again.

use super::*;

/// Marker file of an import that has not finished.
pub(super) const IMPORT_MARKER: &str = "IMPORTING";

/// Fills a new persistent store directly (see the module docs). Created by
/// [`PersistentGraph::bulk_loader`].
pub struct BulkLoader {
    g: Option<PersistentGraph>,
    /// Node ids per label token, in id order (the label index, built at the end).
    labels: Vec<Vec<u64>>,
    /// Property indexes to build at the end.
    indexes: Vec<IndexDef>,
    /// An earlier call failed: the store may be half-written.
    failed: bool,
}

fn not_new(dir: &Path) -> Error {
    Error::Storage(format!(
        "{} already holds a store; bulk import needs a new directory",
        dir.display()
    ))
}

impl PersistentGraph {
    /// Starts a bulk import into `opts.dir`, which must not hold a store yet (it may be
    /// missing or empty). See [`BulkLoader`].
    pub fn bulk_loader(opts: PersistentOptions) -> Result<BulkLoader> {
        let dir = opts.dir.clone();
        if dir.join(META_FILE).exists() || dir.join(IMPORT_MARKER).exists() {
            return Err(not_new(&dir));
        }
        let g = PersistentGraph::open(opts)?;
        if g.meta.next_node != 0 || g.meta.next_rel != 0 {
            return Err(not_new(&dir));
        }
        let marker = dir.join(IMPORT_MARKER);
        File::create(&marker).map_err(io(&marker, "creating"))?;
        sync_dir(&dir)?;
        Ok(BulkLoader {
            g: Some(g),
            labels: Vec::new(),
            indexes: Vec::new(),
            failed: false,
        })
    }
}

impl BulkLoader {
    /// Runs `f` on the store's pager and metadata with no transaction open.
    fn direct<T>(&mut self, f: impl FnOnce(&mut Tx) -> Result<T>) -> Result<T> {
        if self.failed {
            return Err(Error::Storage("an earlier bulk import step failed".into()));
        }
        let g = self.g.as_mut().expect("present until finished");
        let dense_threshold = g.opts.dense_threshold;
        let result = {
            let mut p = g.pager.lock_for_write();
            f(&mut Tx {
                p: &mut p,
                m: &mut g.meta,
                dense_threshold,
            })
        };
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    /// The store being filled, for reads.
    pub fn store(&self) -> &PersistentGraph {
        self.g.as_ref().expect("present until finished")
    }

    /// Adds a node; ids are given out in order from 0.
    pub fn create_node(&mut self, labels: &[String], props: BTreeMap<String, Value>) -> Result<NodeId> {
        let mut pending = std::mem::take(&mut self.labels);
        let created = self.direct(|t| {
            let props = t.intern_props(props)?;
            let mut tokens: Vec<Token> = Vec::with_capacity(labels.len());
            for l in labels {
                let tok = t.m.intern_label(l);
                if !tokens.contains(&tok) {
                    tokens.push(tok);
                }
            }
            let id = t.m.next_node;
            let nd = NodeData {
                labels: tokens,
                props,
            };
            let rec = NodeRec {
                blob: t.write_blob(0, &encode_node(&nd))?,
                ..NodeRec::default()
            };
            t.write_node_rec(id, Some(&rec))?;
            t.m.next_node += 1;
            t.m.node_count += 1;
            for &tok in &nd.labels {
                t.m.label_counts[tok as usize] += 1;
                if pending.len() <= tok as usize {
                    pending.resize_with(tok as usize + 1, Vec::new);
                }
                pending[tok as usize].push(id);
            }
            Ok(NodeId(id))
        });
        self.labels = pending;
        created
    }

    /// Adds a relationship between two nodes created earlier.
    pub fn create_relationship(
        &mut self,
        start: NodeId,
        end: NodeId,
        rel_type: &str,
        props: BTreeMap<String, Value>,
    ) -> Result<RelId> {
        self.direct(|t| t.create_rel(start, end, rel_type, props))
    }

    /// Adds a property index, built by [`finish`](Self::finish) over everything loaded.
    pub fn create_index(&mut self, def: IndexDef) -> Result<()> {
        def.check_new(&self.indexes)?;
        self.indexes.push(def);
        Ok(())
    }

    /// Builds the label and property indexes, writes everything to disk and removes the
    /// import marker. The store can then be opened as usual.
    pub fn finish(mut self) -> Result<()> {
        let labels = std::mem::take(&mut self.labels);
        let indexes = std::mem::take(&mut self.indexes);
        self.direct(|t| {
            // Keys `label | node` in label then node order: already sorted.
            let keys = labels
                .iter()
                .enumerate()
                .flat_map(|(l, nodes)| nodes.iter().map(move |&n| label_key(l as Token, n).to_vec()));
            t.m.label_tree = btree::build(t.p, &mut t.m.index_pages, keys)?;
            for def in indexes {
                if def.is_vector() || def.is_fulltext() {
                    t.m.engine_indexes.push(def);
                    continue;
                }
                build_index(t, def, &labels)?;
            }
            Ok(())
        })?;
        let mut g = self.g.take().expect("present until finished");
        g.meta.seq += 1;
        if let Err(e) = g.checkpoint() {
            g.abandoned = true;
            return Err(e);
        }
        let dir = g.opts.dir.clone();
        drop(g);
        let marker = dir.join(IMPORT_MARKER);
        fs::remove_file(&marker).map_err(io(&marker, "removing"))?;
        sync_dir(&dir)
    }
}

impl Drop for BulkLoader {
    /// An unfinished import is left as it is, marker included; nothing more is flushed.
    fn drop(&mut self) {
        if let Some(mut g) = self.g.take() {
            g.abandoned = true;
        }
    }
}

/// Fills a new property index from the loaded entities.
fn build_index(t: &mut Tx, def: IndexDef, labels: &[Vec<u64>]) -> Result<()> {
    let entity = def.entity;
    let label = match entity {
        IndexEntity::Node => t.m.intern_label(&def.label),
        IndexEntity::Relationship => t.m.intern_type(&def.label),
    };
    let keys = def.properties.iter().map(|p| t.m.keys.intern(p)).collect();
    let mut ix = IndexMeta {
        def,
        label,
        keys,
        root: 0,
        entries: 0,
        distinct: 0,
    };
    let mut entries: Vec<Vec<u8>> = Vec::new();
    match entity {
        IndexEntity::Node => {
            for &id in labels.get(label as usize).map(Vec::as_slice).unwrap_or(&[]) {
                let Some(rec) = t.rd().node(NodeId(id))? else {
                    return Err(corrupt(format!("loaded node {id} is missing")));
                };
                let props = t.rd().node_data(&rec)?.props;
                entries.extend(ix_entry(&ix, &props, id));
            }
        }
        IndexEntity::Relationship => {
            for id in 0..t.m.next_rel {
                let Some(rec) = t.rd().rel(RelId(id))? else { continue };
                if rec.rel_type == label {
                    let props = t.rd().rel_props(&rec)?;
                    entries.extend(ix_entry(&ix, &props, id));
                }
            }
        }
    }
    entries.sort_unstable();
    let value = |k: &[u8]| k[..k.len() - 8].to_vec();
    ix.entries = entries.len() as u64;
    ix.distinct = entries
        .iter()
        .enumerate()
        .filter(|&(i, k)| i == 0 || value(&entries[i - 1]) != value(k))
        .count() as u64;
    ix.root = btree::build(t.p, &mut t.m.index_pages, entries)?;
    t.m.indexes.push(ix);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("graphbuff-bulk-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            Dir(d)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    type NodeSpec = (Vec<String>, BTreeMap<String, Value>);
    type RelSpec = (u64, u64, &'static str, BTreeMap<String, Value>);

    /// A deterministic graph: hubs (dense), self-loops, two types, long string values
    /// (lossy index keys) and shared values.
    fn spec(n: u64) -> (Vec<NodeSpec>, Vec<RelSpec>) {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut rnd = move |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        let mut nodes = Vec::new();
        for i in 0..n {
            let mut labels = vec!["Person".to_string()];
            if i % 3 == 0 {
                labels.push("Admin".to_string());
            }
            if i % 7 == 0 {
                labels = Vec::new();
            }
            let mut props = BTreeMap::new();
            props.insert("id".to_string(), Value::Int(i as i64));
            props.insert("group".to_string(), Value::Int((i % 10) as i64));
            if i % 5 == 0 {
                props.insert("bio".to_string(), Value::String(format!("{}{i}", "x".repeat(400))));
            }
            nodes.push((labels, props));
        }
        let mut rels = Vec::new();
        for i in 0..n * 3 {
            let start = if i % 4 == 0 { rnd(3) } else { rnd(n) };
            let end = if i % 50 == 0 { start } else { rnd(n) };
            let t = if i % 3 == 0 { "LIKES" } else { "KNOWS" };
            let mut props = BTreeMap::new();
            if i % 2 == 0 {
                props.insert("w".to_string(), Value::Int(rnd(100) as i64));
            }
            rels.push((start, end, t, props));
        }
        (nodes, rels)
    }

    fn defs() -> Vec<IndexDef> {
        vec![
            IndexDef::node("by_id", "Person", "id"),
            IndexDef::node("by_bio", "Person", "bio"),
            IndexDef {
                name: "by_pair".into(),
                entity: IndexEntity::Node,
                label: "Admin".into(),
                properties: vec!["group".into(), "id".into()],
                extra_labels: Vec::new(),
                kind: Default::default(),
            },
            IndexDef::relationship("by_w", "KNOWS", "w"),
        ]
    }

    fn opts(dir: &Path) -> PersistentOptions {
        PersistentOptions::new(dir).dense_threshold(20).cache_bytes(1 << 20)
    }

    fn by_hand(dir: &Path, nodes: &[NodeSpec], rels: &[RelSpec]) -> PersistentGraph {
        let mut g = PersistentGraph::open(opts(dir)).unwrap();
        g.begin().unwrap();
        for (l, p) in nodes {
            g.create_node(l, p.clone()).unwrap();
        }
        for (a, b, t, p) in rels {
            g.create_relationship(NodeId(*a), NodeId(*b), t, p.clone()).unwrap();
        }
        // After the data, so that type tokens are given out in the same order.
        for d in defs() {
            g.create_index(d).unwrap();
        }
        g.commit().unwrap();
        g
    }

    fn bulk(dir: &Path, nodes: &[NodeSpec], rels: &[RelSpec]) {
        let mut b = PersistentGraph::bulk_loader(opts(dir)).unwrap();
        for (l, p) in nodes {
            b.create_node(l, p.clone()).unwrap();
        }
        for (a, e, t, p) in rels {
            b.create_relationship(NodeId(*a), NodeId(*e), t, p.clone()).unwrap();
        }
        for d in defs() {
            b.create_index(d).unwrap();
        }
        b.finish().unwrap();
    }

    fn dense(g: &PersistentGraph, n: NodeId) -> bool {
        g.reader(|r| Ok(r.node(n)?.unwrap().dense)).unwrap()
    }

    fn probe(g: &PersistentGraph) -> Vec<Option<Vec<u64>>> {
        let mut out = Vec::new();
        for v in [0i64, 5, 21, 300, 1999] {
            out.push(g.schema_index_seek(IndexEntity::Node, "Person", &["id".into()], &[Value::Int(v)]).unwrap());
            let pair = [Value::Int(v % 10), Value::Int(v)];
            out.push(g.schema_index_seek(IndexEntity::Node, "Admin", &["group".into(), "id".into()], &pair).unwrap());
            let w = g.schema_index_seek(IndexEntity::Relationship, "KNOWS", &["w".into()], &[Value::Int(v % 100)]);
            out.push(w.unwrap().map(|mut v| {
                v.sort();
                v
            }));
            let bio = Value::String(format!("{}{v}", "x".repeat(400)));
            out.push(g.schema_index_seek(IndexEntity::Node, "Person", &["bio".into()], &[bio]).unwrap());
        }
        let range = ValueRange::Between {
            lower: Bound::Included(Value::Int(100)),
            upper: Bound::Excluded(Value::Int(1200)),
        };
        out.push(g.schema_index_range(IndexEntity::Node, "Person", "id", &range).unwrap());
        out
    }

    fn same(a: &PersistentGraph, b: &PersistentGraph, nodes: usize, rels: usize) {
        assert_eq!(a.node_count(), b.node_count());
        assert_eq!(a.relationship_count(), b.relationship_count());
        assert_eq!(a.labels(), b.labels());
        assert_eq!(a.relationship_types(), b.relationship_types());
        for l in ["Person", "Admin"] {
            let x: Vec<_> = a.nodes_with_label(l).map(|n| n.unwrap()).collect();
            let y: Vec<_> = b.nodes_with_label(l).map(|n| n.unwrap()).collect();
            assert_eq!(x, y, "label {l}");
            assert_eq!(a.label_count(l), b.label_count(l));
        }
        for i in 0..nodes as u64 {
            let n = NodeId(i);
            assert_eq!(a.node_labels(n).unwrap(), b.node_labels(n).unwrap());
            assert_eq!(a.node_properties(n).unwrap(), b.node_properties(n).unwrap());
            assert_eq!(dense(a, n), dense(b, n), "dense flag of {i}");
            for d in [Direction::Outgoing, Direction::Incoming, Direction::Both] {
                for t in [vec![], vec!["KNOWS".to_string()]] {
                    let x = a.expand_all(n, d, &t).unwrap();
                    let y = b.expand_all(n, d, &t).unwrap();
                    assert_eq!(x, y, "expand {i} {d:?} {t:?}");
                    assert_eq!(a.degree(n, d, &t).unwrap(), b.degree(n, d, &t).unwrap());
                }
            }
        }
        for i in 0..rels as u64 {
            let r = RelId(i);
            assert_eq!(a.relationship_endpoints(r).unwrap(), b.relationship_endpoints(r).unwrap());
            assert_eq!(a.relationship_properties(r).unwrap(), b.relationship_properties(r).unwrap());
        }
        for d in defs() {
            let x = a.schema_index_stats(d.entity, &d.label, &d.properties).unwrap();
            let y = b.schema_index_stats(d.entity, &d.label, &d.properties).unwrap();
            assert_eq!((x.entries, x.distinct), (y.entries, y.distinct), "{}", d.name);
        }
        assert_eq!(probe(a), probe(b));
    }

    #[test]
    fn bulk_load_matches_ordinary_writes() {
        let (nodes, rels) = spec(2000);
        let (d1, d2) = (Dir::new("hand"), Dir::new("bulk"));
        let a = by_hand(&d1.0, &nodes, &rels);
        bulk(&d2.0, &nodes, &rels);
        let mut b = PersistentGraph::open(opts(&d2.0)).unwrap();
        assert!(dense(&b, NodeId(0)), "hubs become dense");
        same(&a, &b, nodes.len(), rels.len());

        // The loaded store takes ordinary writes, into the built trees too.
        drop(a);
        let mut a = PersistentGraph::open(opts(&d1.0)).unwrap();
        for g in [&mut a, &mut b] {
            g.begin().unwrap();
            for i in 0..500 {
                let mut p = BTreeMap::new();
                p.insert("id".to_string(), Value::Int(10_000 + i));
                p.insert("group".to_string(), Value::Int(i % 10));
                let n = g.create_node(&["Person".into(), "Admin".into()], p).unwrap();
                g.create_relationship(n, NodeId(1), "KNOWS", BTreeMap::new()).unwrap();
            }
            g.delete_node(NodeId(2), true).unwrap();
            g.commit().unwrap();
        }
        let ids = |g: &PersistentGraph| g.nodes_with_label("Admin").map(|n| n.unwrap()).collect::<Vec<_>>();
        assert_eq!(ids(&a), ids(&b));
        assert_eq!(a.node_count(), b.node_count());
        let seek = |g: &PersistentGraph| {
            g.schema_index_seek(IndexEntity::Node, "Person", &["id".into()], &[Value::Int(10_250)]).unwrap()
        };
        assert_eq!(seek(&b), Some(vec![2250]));
        assert_eq!(seek(&a), seek(&b));
    }

    #[test]
    fn empty_load_and_unfinished_load() {
        let d = Dir::new("empty");
        PersistentGraph::bulk_loader(opts(&d.0)).unwrap().finish().unwrap();
        let g = PersistentGraph::open(opts(&d.0)).unwrap();
        assert_eq!(g.node_count(), 0);
        drop(g);
        // A directory with a store is refused.
        assert!(PersistentGraph::bulk_loader(opts(&d.0)).is_err());

        let d = Dir::new("unfinished");
        let mut b = PersistentGraph::bulk_loader(opts(&d.0)).unwrap();
        b.create_node(&["A".into()], BTreeMap::new()).unwrap();
        drop(b);
        let e = PersistentGraph::open(opts(&d.0)).unwrap_err();
        assert!(e.to_string().contains("did not finish"), "{e}");
        assert!(PersistentGraph::bulk_loader(opts(&d.0)).is_err());
    }

    #[test]
    fn built_trees_have_several_levels() {
        let d = Dir::new("levels");
        let mut b = PersistentGraph::bulk_loader(opts(&d.0)).unwrap();
        for i in 0..60_000i64 {
            let mut p = BTreeMap::new();
            p.insert("k".to_string(), Value::String(format!("{:0>100}", i % 20_000)));
            b.create_node(&["L".into()], p).unwrap();
        }
        b.create_index(IndexDef::node("ix", "L", "k")).unwrap();
        b.finish().unwrap();
        let g = PersistentGraph::open(opts(&d.0)).unwrap();
        let all: Vec<_> = g.nodes_with_label("L").map(|n| n.unwrap().0).collect();
        assert_eq!(all, (0..60_000).collect::<Vec<_>>());
        let stats = g.schema_index_stats(IndexEntity::Node, "L", &["k".into()]).unwrap();
        assert_eq!((stats.entries, stats.distinct), (60_000, 20_000));
        for v in [0u64, 1, 9_999, 19_999] {
            let key = Value::String(format!("{v:0>100}"));
            let hits = g.schema_index_seek(IndexEntity::Node, "L", &["k".into()], &[key]).unwrap().unwrap();
            assert_eq!(hits, vec![v, v + 20_000, v + 40_000]);
        }
        // Values below 10,000 start with 96 zeros.
        let prefix = ValueRange::Prefix("0".repeat(96));
        let hits = g.schema_index_range(IndexEntity::Node, "L", "k", &prefix).unwrap().unwrap();
        assert_eq!(hits.len(), 30_000);
    }
}
