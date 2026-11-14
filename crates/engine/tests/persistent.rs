//! Persistent (disk-resident, paged) storage: equivalence with the in-memory store,
//! durability, crash recovery and bounded memory.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{
    BoxedStore, Database, DatabaseManager, Direction, Error, GraphStore, IndexDef, MemoryGraph,
    NodeId, PersistentGraph, PersistentOptions, RelId, StorageMode, SyncPolicy, Value, ValueRange,
};

/// A fresh directory under the system temp dir, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-ptest-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        TempDir(dir)
    }

    fn opts(&self) -> PersistentOptions {
        PersistentOptions::new(&self.0)
    }

    /// The smallest cache (16 pages), so that tests exercise eviction.
    fn tiny(&self) -> PersistentOptions {
        self.opts().cache_bytes(0)
    }

    fn open(&self) -> Database<BoxedStore> {
        Database::open(StorageMode::Persistent(self.opts())).unwrap()
    }

    fn wal(&self) -> PathBuf {
        self.0.join("graph.wal")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run<S: GraphStore>(db: &Database<S>, q: &str) -> Vec<String> {
    db.execute(q)
        .unwrap_or_else(|e| panic!("`{q}` failed: {e}"))
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .collect()
}

// ---- Cypher-level equivalence -----------------------------------------------------------

const SCRIPT: &str = "
CREATE (ann:Person {name: 'Ann', age: 31, tags: ['a', 'b']}),
       (bob:Person {name: 'Bob', age: 42}),
       (cid:Person:Admin {name: 'Cid'}),
       (heat:Movie {title: 'Heat', year: 1995}),
       (ann)-[:KNOWS {since: 2020}]->(bob),
       (bob)-[:KNOWS]->(cid),
       (cid)-[:KNOWS]->(ann),
       (ann)-[:LIKES {stars: 5}]->(heat),
       (bob)-[:LIKES {stars: 3}]->(heat),
       (cid)-[:SELF]->(cid);
CREATE INDEX person_name FOR (p:Person) ON (p.name);
UNWIND range(1, 200) AS i CREATE (:Num {i: i, even: i % 2 = 0, s: 'n' + toString(i)});
MATCH (a:Num), (b:Num) WHERE b.i = a.i + 1 CREATE (a)-[:NEXT]->(b);
MATCH (n:Num) WHERE n.i % 10 = 0 SET n:Round, n.big = 'x' + toString(n.i);
MATCH (n:Num) WHERE n.i > 190 DETACH DELETE n;
MATCH (p:Person {name: 'Bob'}) SET p.age = 43, p.long = 'LLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLL' + 'LLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLL' + 'LLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLL' + 'LLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLL' + 'LLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLL' + 'LLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLL';
CREATE INDEX person_long FOR (p:Person) ON (p.long);
MATCH (p:Person {name: 'Cid'}) REMOVE p:Admin SET p.age = null;
MATCH (:Person {name: 'Ann'})-[l:LIKES]->() DELETE l;
";

const QUERIES: &[&str] = &[
    "MATCH (n) RETURN count(n)",
    "MATCH ()-[r]->() RETURN count(r)",
    "MATCH (p:Person) RETURN p.name, p.age, p.tags, labels(p) ORDER BY p.name",
    "MATCH (a)-[k:KNOWS]->(b) RETURN a.name, k.since, b.name ORDER BY a.name",
    "MATCH (a:Person)-[:KNOWS*1..3]->(b) RETURN a.name, count(DISTINCT b) ORDER BY a.name",
    "MATCH (n:Num) RETURN count(n), sum(n.i), min(n.i), max(n.i)",
    "MATCH (n:Round) RETURN n.i, n.big ORDER BY n.i",
    "MATCH (a:Num)-[:NEXT]->(b:Num) RETURN count(*), sum(b.i - a.i)",
    "MATCH (n:Num {i: 50})<-[:NEXT*2]-(m) RETURN m.i",
    "MATCH (p:Person {name: 'Bob'}) RETURN p.age, size(p.long)",
    "MATCH (p:Person) WHERE p.long STARTS WITH 'LLL' RETURN p.name",
    "MATCH (c)-[s:SELF]-(c) RETURN c.name, type(s)",
    "MATCH (m:Movie)<-[l:LIKES]-(p) RETURN m.title, p.name, l.stars",
    "MATCH (n) RETURN labels(n) AS l, count(*) ORDER BY l",
    "MATCH (n:Person) RETURN n.name, degree(n) AS deg ORDER BY n.name",
];

#[test]
fn cypher_results_match_in_memory_store() {
    let dir = TempDir::new("cypher");
    let mem = Database::in_memory();
    let disk = Database::with_store(PersistentGraph::open(dir.tiny()).unwrap());
    for stmt in graphbuff::cypher::split_statements(SCRIPT).unwrap() {
        run(&mem, stmt);
        run(&disk, stmt);
    }
    for q in QUERIES {
        assert_eq!(run(&disk, q), run(&mem, q), "{q}");
    }
    // A value too long to index inline: the lossy index key is re-checked.
    let long = mem.read(|s| s.node_property(NodeId(1), "long")).unwrap().unwrap();
    assert!(matches!(&long, Value::String(l) if l.len() > 256));
    assert_eq!(
        disk.read(|s| s.index_seek("Person", "long", &long)).unwrap(),
        Some(vec![NodeId(1)])
    );
    // Same answers after a reopen.
    drop(disk);
    let disk = Database::with_store(PersistentGraph::open(dir.tiny()).unwrap());
    for q in QUERIES {
        assert_eq!(run(&disk, q), run(&mem, q), "after reopen: {q}");
    }
    assert_eq!(disk.read(|s| s.indexes()), mem.read(|s| s.indexes()));
}

// ---- store-level differential test ------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

const LABELS: &[&str] = &["A", "B", "C"];
const TYPES: &[&str] = &["R", "S"];
const KEYS: &[&str] = &["k0", "k1", "k2", "k3"];

fn value_pool() -> Vec<Value> {
    vec![
        Value::Int(0),
        Value::Int(1),
        Value::Int(2),
        Value::Float(1.0),
        Value::Float(2.5),
        Value::String("a".into()),
        Value::String("b".into()),
        Value::String("x".repeat(300)),
        Value::String(format!("{}y", "x".repeat(300))),
        Value::String("z".repeat(1000)),
        Value::List(vec![Value::Int(1), Value::Int(2)]),
        Value::Bool(true),
    ]
}

fn random_props(rng: &mut Rng, pool: &[Value]) -> BTreeMap<String, Value> {
    let mut props = BTreeMap::new();
    for k in KEYS {
        if rng.chance(40) {
            props.insert(k.to_string(), pool[rng.below(pool.len() as u64) as usize].clone());
        }
    }
    props
}

/// A canonical dump of everything observable through the store API. Orders that the two
/// stores may legitimately differ in after a rollback (labels of a node, adjacency order)
/// are sorted.
fn dump(g: &dyn GraphStore, pool: &[Value]) -> String {
    try_dump(g, pool).unwrap_or_else(|e| panic!("read failed: {e}"))
}

fn try_dump(g: &dyn GraphStore, pool: &[Value]) -> graphbuff::Result<String> {
    use std::fmt::Write;
    let mut s = String::new();
    writeln!(s, "nodes={} rels={}", g.node_count(), g.relationship_count()).unwrap();
    writeln!(s, "labels={:?} types={:?}", g.labels(), g.relationship_types()).unwrap();
    for n in g.all_nodes() {
        let n = n?;
        let mut labels = g.node_labels(n)?;
        labels.sort();
        let mut exp = Vec::new();
        for d in [Direction::Outgoing, Direction::Incoming, Direction::Both] {
            let mut e: Vec<_> = g
                .expand_all(n, d, &[])?
                .iter()
                .map(|x| (x.rel.0, x.other.0))
                .collect();
            e.sort();
            let degree = g.degree(n, d, &[])?;
            assert_eq!(degree, e.len(), "degree of n{n} {d:?}");
            exp.push((e, degree));
            // Typed: one type, several types (one of them unknown), only unknown types.
            for types in [&["R".to_string()][..], &["S".into(), "nope".into()], &["nope".into()]] {
                let mut t: Vec<_> = g
                    .expand_all(n, d, types)?
                    .iter()
                    .map(|x| (x.rel.0, x.other.0))
                    .collect();
                t.sort();
                let degree = g.degree(n, d, types)?;
                assert_eq!(degree, t.len(), "degree of n{n} {d:?} {types:?}");
                exp.push((t, degree));
            }
        }
        let props = g.node_properties(n)?;
        writeln!(s, "n{n} {labels:?} {props:?} {exp:?}").unwrap();
        for k in KEYS {
            assert_eq!(g.node_property(n, k)?, props.get(*k).cloned());
        }
        for l in LABELS {
            assert_eq!(g.node_has_label(n, l)?, labels.iter().any(|x| x == l));
        }
        assert!(g.node_exists(n)?);
    }
    for r in g.all_relationships() {
        let r = r?;
        writeln!(
            s,
            "r{r} {:?} {:?} {:?}",
            g.relationship_type(r)?,
            g.relationship_endpoints(r)?,
            g.relationship_properties(r)?
        )
        .unwrap();
        for k in KEYS {
            assert_eq!(
                g.relationship_property(r, k)?,
                g.relationship_properties(r)?.get(*k).cloned()
            );
        }
        assert!(g.relationship_exists(r)?);
    }
    for l in LABELS {
        let ids = g.nodes_with_label(l).collect::<graphbuff::Result<Vec<_>>>()?;
        assert_eq!(ids.len(), g.label_count(l));
        writeln!(s, "label {l}: {ids:?}").unwrap();
    }
    writeln!(s, "indexes {:?}", g.indexes()).unwrap();
    for t in TYPES {
        writeln!(s, "type {t}: {}", g.relationship_type_count(t)).unwrap();
    }
    for ix in g.indexes() {
        let stats = g.index_stats(&ix.label, &ix.properties[0]).expect("the index exists");
        // Entries are the labelled nodes with the property; check, then dump the counts.
        let mut entries = 0;
        let mut values: Vec<Value> = Vec::new();
        for n in g.nodes_with_label(&ix.label) {
            if let Some(v) = g.node_property(n?, &ix.properties[0])? {
                entries += 1;
                let v = match v {
                    Value::Float(f) if f.fract() == 0.0 => Value::Int(f as i64),
                    v => v,
                };
                if !values.contains(&v) {
                    values.push(v);
                }
            }
        }
        assert_eq!((stats.entries, stats.distinct), (entries, values.len()), "{}", ix.name);
        writeln!(s, "stats {}: {stats:?}", ix.name).unwrap();
    }
    for ix in g.indexes() {
        for v in pool {
            writeln!(
                s,
                "seek {} {v:?}: {:?}",
                ix.name,
                g.index_seek(&ix.label, &ix.properties[0], v)?
            )
            .unwrap();
        }
        // Range seeks may return extra candidates (callers re-check), so check that each
        // store returns every node in the range and only labelled nodes with the
        // property, and dump the exact answer.
        let labelled: Vec<NodeId> = g.nodes_with_label(&ix.label).collect::<graphbuff::Result<_>>()?;
        for range in test_ranges() {
            let got = g
                .index_range(&ix.label, &ix.properties[0], &range)?
                .expect("the index exists");
            assert!(got.windows(2).all(|w| w[0] < w[1]), "{range:?}: not ascending");
            let mut exact = Vec::new();
            for &n in &labelled {
                if let Some(v) = g.node_property(n, &ix.properties[0])?
                    && in_range(&v, &range)
                {
                    exact.push(n);
                }
            }
            for n in &exact {
                assert!(got.contains(n), "{} {range:?}: missing n{n}", ix.name);
            }
            for n in &got {
                assert!(labelled.contains(n), "{} {range:?}: n{n} is not labelled", ix.name);
            }
            writeln!(s, "range {} {range:?}: {exact:?}", ix.name).unwrap();
        }
    }
    Ok(s)
}

fn test_ranges() -> Vec<ValueRange> {
    use std::ops::Bound::{Excluded, Included, Unbounded};
    let long = "x".repeat(300);
    vec![
        ValueRange::Between { lower: Included(Value::Int(1)), upper: Excluded(Value::Float(2.5)) },
        ValueRange::Between { lower: Excluded(Value::Int(1)), upper: Unbounded },
        ValueRange::Between { lower: Unbounded, upper: Included(Value::Float(1.0)) },
        ValueRange::Between { lower: Included(Value::Int(3)), upper: Included(Value::Int(1)) },
        ValueRange::Between { lower: Included("a".into()), upper: Excluded("y".into()) },
        ValueRange::Between { lower: Excluded(Value::String(long.clone())), upper: Unbounded },
        ValueRange::Between { lower: Unbounded, upper: Included(Value::String(long.clone())) },
        ValueRange::Between { lower: Included(Value::Int(1)), upper: Included("b".into()) },
        ValueRange::Prefix("x".into()),
        ValueRange::Prefix(format!("{long}y")),
        ValueRange::Prefix(String::new()),
    ]
}

/// Cypher comparison semantics for the value pool: numbers with numbers, strings with
/// strings, anything else never in range.
fn in_range(v: &Value, range: &ValueRange) -> bool {
    use std::cmp::Ordering;
    use std::ops::Bound;
    let cmp = |a: &Value, b: &Value| -> Option<Ordering> {
        let num = |x: &Value| match x {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        };
        match (a, b) {
            (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
            _ => num(a)?.partial_cmp(&num(b)?),
        }
    };
    match range {
        ValueRange::Prefix(p) => matches!(v, Value::String(s) if s.starts_with(p.as_str())),
        ValueRange::Between { lower, upper } => {
            let ok = |b: &Bound<Value>, want: Ordering| match b {
                Bound::Unbounded => true,
                Bound::Included(x) => cmp(v, x).is_some_and(|o| o == want || o.is_eq()),
                Bound::Excluded(x) => cmp(v, x) == Some(want),
            };
            let kind_ok = [lower, upper].iter().any(|b| !matches!(b, Bound::Unbounded));
            kind_ok && ok(lower, Ordering::Greater) && ok(upper, Ordering::Less)
        }
    }
}

fn outcome<T: std::fmt::Debug>(r: graphbuff::Result<T>) -> String {
    match r {
        Ok(v) => format!("ok {v:?}"),
        Err(e) => format!(
            "err {}",
            match e {
                Error::Constraint(_) => "constraint",
                Error::Runtime(_) => "runtime",
                Error::Type(_) => "type",
                _ => "other",
            }
        ),
    }
}

/// Applies one random operation to both stores and checks they agree on the outcome.
fn random_op(rng: &mut Rng, pool: &[Value], mem: &mut MemoryGraph, disk: &mut PersistentGraph) {
    let nodes: Vec<NodeId> = mem.all_nodes().map(Result::unwrap).collect();
    let rels: Vec<RelId> = mem.all_relationships().map(Result::unwrap).collect();
    // Mostly existing entities, sometimes a deleted or never-created id.
    let node = |rng: &mut Rng| match nodes.len() {
        0 => NodeId(rng.below(5)),
        n if rng.chance(90) => nodes[rng.below(n as u64) as usize],
        _ => NodeId(rng.below(nodes.len() as u64 + 5)),
    };
    let rel = |rng: &mut Rng| match rels.len() {
        0 => RelId(rng.below(5)),
        n if rng.chance(90) => rels[rng.below(n as u64) as usize],
        _ => RelId(rng.below(rels.len() as u64 + 5)),
    };
    let pick = |rng: &mut Rng, items: &[&str]| items[rng.below(items.len() as u64) as usize].to_string();
    let (a, b) = match rng.below(100) {
        0..25 => {
            let labels: Vec<String> = LABELS
                .iter()
                .filter(|_| rng.chance(40))
                .map(|l| l.to_string())
                .collect();
            let props = random_props(rng, pool);
            (
                outcome(mem.create_node(&labels, props.clone())),
                outcome(disk.create_node(&labels, props)),
            )
        }
        25..45 => {
            let (s, e) = (node(rng), node(rng));
            let e = if rng.chance(10) { s } else { e };
            let t = pick(rng, TYPES);
            let props = random_props(rng, pool);
            (
                outcome(mem.create_relationship(s, e, &t, props.clone())),
                outcome(disk.create_relationship(s, e, &t, props)),
            )
        }
        45..52 => {
            let r = rel(rng);
            (
                outcome(mem.delete_relationship(r)),
                outcome(disk.delete_relationship(r)),
            )
        }
        52..58 => {
            let (n, detach) = (node(rng), rng.chance(60));
            (
                outcome(mem.delete_node(n, detach)),
                outcome(disk.delete_node(n, detach)),
            )
        }
        58..75 => {
            let (n, k) = (node(rng), pick(rng, KEYS));
            let v = if rng.chance(20) {
                Value::Null
            } else if rng.chance(5) {
                Value::Map(BTreeMap::new())
            } else {
                pool[rng.below(pool.len() as u64) as usize].clone()
            };
            (
                outcome(mem.set_node_property(n, &k, v.clone())),
                outcome(disk.set_node_property(n, &k, v)),
            )
        }
        75..83 => {
            let (r, k) = (rel(rng), pick(rng, KEYS));
            let v = if rng.chance(20) {
                Value::Null
            } else {
                pool[rng.below(pool.len() as u64) as usize].clone()
            };
            (
                outcome(mem.set_relationship_property(r, &k, v.clone())),
                outcome(disk.set_relationship_property(r, &k, v)),
            )
        }
        83..90 => {
            let (n, l) = (node(rng), pick(rng, LABELS));
            (outcome(mem.add_label(n, &l)), outcome(disk.add_label(n, &l)))
        }
        90..97 => {
            let (n, l) = (node(rng), pick(rng, LABELS));
            (
                outcome(mem.remove_label(n, &l)),
                outcome(disk.remove_label(n, &l)),
            )
        }
        _ => {
            let defs = [("ia", "A", "k0"), ("ib", "B", "k1"), ("ic", "C", "k2")];
            let (name, label, prop) = defs[rng.below(3) as usize];
            if rng.chance(50) {
                let def = IndexDef::node(name, label, prop);
                (
                    outcome(mem.create_index(def.clone())),
                    outcome(disk.create_index(def)),
                )
            } else {
                (outcome(mem.drop_index(name)), outcome(disk.drop_index(name)))
            }
        }
    };
    assert_eq!(a, b);
}

/// Runs a random workload against both stores. `dense_threshold` 0 makes every node with
/// a relationship dense; small values make nodes turn dense in the middle of transactions
/// (which then commit, roll back, or crash).
fn differential(seed: u64, sync: SyncPolicy, dense_threshold: u32) {
    let dir = TempDir::new("diff");
    let opts = dir
        .tiny()
        .sync(sync)
        .checkpoint_after_bytes(512 << 10)
        .dense_threshold(dense_threshold);
    let pool = value_pool();
    let mut rng = Rng(seed);
    let mut mem = MemoryGraph::new();
    let mut disk = PersistentGraph::open(opts.clone()).unwrap();

    for round in 0..300 {
        mem.begin().unwrap();
        disk.begin().unwrap();
        for _ in 0..rng.below(8) + 1 {
            random_op(&mut rng, &pool, &mut mem, &mut disk);
        }
        if rng.chance(25) {
            mem.rollback();
            disk.rollback();
        } else {
            mem.commit().unwrap();
            disk.commit().unwrap();
        }
        // Writes outside a transaction commit on their own.
        if rng.chance(10) {
            random_op(&mut rng, &pool, &mut mem, &mut disk);
        }
        match rng.below(40) {
            0 => disk.checkpoint().unwrap(),
            1 => {
                drop(disk);
                disk = PersistentGraph::open(opts.clone()).unwrap();
            }
            2 => {
                disk.abandon(); // crash
                disk = PersistentGraph::open(opts.clone()).unwrap();
            }
            _ => {}
        }
        assert!(disk.cached_pages() <= disk.cache_capacity());
        if round % 5 == 0 {
            assert_eq!(dump(&disk, &pool), dump(&mem, &pool), "round {round}");
        }
    }
    assert_eq!(dump(&disk, &pool), dump(&mem, &pool));
    // Every property key the disk store knows, the memory store knows too (the memory
    // store also keeps keys interned by rolled-back transactions).
    let mem_keys = mem.property_keys();
    assert!(disk.property_keys().iter().all(|k| mem_keys.contains(k)));
}

#[test]
fn random_workload_matches_memory_store() {
    differential(0x9E37_79B9_7F4A_7C15, SyncPolicy::EveryCommit, 50);
}

#[test]
fn random_workload_matches_memory_store_os_buffered() {
    differential(0xD1B5_4A32_D192_ED03, SyncPolicy::OsBuffered, 50);
}

#[test]
fn random_workload_with_dense_nodes_matches_memory_store() {
    differential(0xA076_1D64_78BD_642F, SyncPolicy::EveryCommit, 2);
}

#[test]
fn random_workload_all_dense_matches_memory_store() {
    differential(0xE703_7ED1_A0B4_28DB, SyncPolicy::OsBuffered, 0);
}

// ---- durability and recovery ------------------------------------------------------------

#[test]
fn survives_reopen_and_keeps_ids_retired() {
    let dir = TempDir::new("reopen");
    {
        let db = dir.open();
        run(&db, "CREATE (:A {x: 1}), (:Temp)");
        run(&db, "MATCH (t:Temp) DELETE t");
    }
    let db = dir.open();
    assert_eq!(run(&db, "MATCH (n) RETURN labels(n), n.x"), vec![r#"["A"], 1"#]);
    assert_eq!(run(&db, "CREATE (n) RETURN id(n)"), vec!["2"]);
    assert!(db.storage_description().starts_with("persistent:"));
}

#[test]
fn failed_queries_leave_no_trace() {
    let dir = TempDir::new("rollback");
    {
        let db = dir.open();
        run(&db, "CREATE (:Keep)");
        assert!(
            db.execute("CREATE (:Lost) WITH 1 AS one RETURN one / 0")
                .is_err()
        );
        assert_eq!(db.node_count(), 1);
        assert_eq!(run(&db, "CREATE (n) RETURN id(n)"), vec!["1"]);
    }
    let db = dir.open();
    assert_eq!(db.node_count(), 2);
    assert_eq!(run(&db, "MATCH (n:Lost) RETURN count(n)"), vec!["0"]);
}

#[test]
fn recovers_from_the_log_after_a_crash() {
    let dir = TempDir::new("crash");
    let mem = Database::in_memory();
    {
        let store = PersistentGraph::open(dir.tiny().checkpoint_after_bytes(0)).unwrap();
        let db = Database::with_store(store);
        for stmt in graphbuff::cypher::split_statements(SCRIPT).unwrap() {
            run(&db, stmt);
            run(&mem, stmt);
        }
        // Evictions have written some pages to the data files, but nothing was
        // checkpointed: `meta` does not exist yet.
        assert!(!dir.0.join("meta").exists());
        db.into_store().abandon();
    }
    let disk = Database::with_store(PersistentGraph::open(dir.tiny()).unwrap());
    for q in QUERIES {
        assert_eq!(run(&disk, q), run(&mem, q), "{q}");
    }
    assert_eq!(fs::metadata(dir.wal()).unwrap().len(), 8, "recovery checkpoints");
}

#[test]
fn torn_log_tail_is_discarded() {
    let dir = TempDir::new("torn");
    {
        let store = PersistentGraph::open(dir.opts()).unwrap();
        let db = Database::with_store(store);
        run(&db, "CREATE (:A)");
        run(&db, "CREATE (:B)");
        db.into_store().abandon();
    }
    // A crash halfway through writing the next record.
    let mut f = OpenOptions::new().append(true).open(dir.wal()).unwrap();
    f.write_all(&[200, 0, 0, 0, 1, 2, 3, 4, 9, 9]).unwrap();
    drop(f);
    let db = dir.open();
    assert_eq!(
        run(&db, "MATCH (n) RETURN labels(n) ORDER BY id(n)"),
        vec![r#"["A"]"#, r#"["B"]"#]
    );
}

#[test]
fn corrupt_record_stops_replay_there() {
    let dir = TempDir::new("crc");
    {
        let store = PersistentGraph::open(dir.opts()).unwrap();
        let db = Database::with_store(store);
        run(&db, "CREATE (:A)");
        run(&db, "CREATE (:B)");
        db.into_store().abandon();
    }
    let mut bytes = fs::read(dir.wal()).unwrap();
    *bytes.last_mut().unwrap() ^= 0xFF; // inside the second record's last page image
    fs::write(dir.wal(), &bytes).unwrap();
    let db = dir.open();
    assert_eq!(run(&db, "MATCH (n) RETURN labels(n)"), vec![r#"["A"]"#]);
}

#[test]
fn corrupt_metadata_is_an_error() {
    let dir = TempDir::new("badmeta");
    drop(dir.open().execute("CREATE (:A)").unwrap());
    let path = dir.0.join("meta");
    let mut bytes = fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    fs::write(&path, bytes).unwrap();
    assert!(matches!(
        Database::open(StorageMode::Persistent(dir.opts())),
        Err(Error::Storage(_))
    ));
}

#[test]
fn directory_is_locked_while_open() {
    let dir = TempDir::new("lock");
    let db = dir.open();
    assert!(matches!(
        Database::open(StorageMode::Persistent(dir.opts())),
        Err(Error::Storage(_))
    ));
    drop(db);
    dir.open();
}

#[test]
fn checkpoint_empties_the_log() {
    let dir = TempDir::new("ckpt");
    let db = dir.open();
    run(&db, "UNWIND range(1, 100) AS i CREATE (:N {i: i})");
    assert!(fs::metadata(dir.wal()).unwrap().len() > 8);
    db.checkpoint().unwrap();
    assert_eq!(fs::metadata(dir.wal()).unwrap().len(), 8);
    drop(db);
    assert_eq!(
        run(&dir.open(), "MATCH (n:N) RETURN count(n), sum(n.i)"),
        vec!["100, 5050"]
    );
}

// ---- larger than the cache --------------------------------------------------------------

#[test]
fn graph_larger_than_cache() {
    let dir = TempDir::new("large");
    let opts = dir.tiny().sync(SyncPolicy::OsBuffered);
    {
        let db = Database::with_store(PersistentGraph::open(opts.clone()).unwrap());
        run(&db, "CREATE INDEX n_i FOR (n:N) ON (n.i)");
        for chunk in 0..10 {
            run(
                &db,
                &format!(
                    "UNWIND range({}, {}) AS i CREATE (:N {{i: i, pad: 'pppppppppppppppppppppppppppppppppppppppp'}})",
                    chunk * 2000,
                    chunk * 2000 + 1999
                ),
            );
        }
        run(
            &db,
            "MATCH (a:N), (b:N) WHERE b.i = a.i + 1 AND a.i % 100 = 0 CREATE (a)-[:NEXT]->(b)",
        );
        let (cached, cap) = db.read(|s| (s.cached_pages(), s.cache_capacity()));
        assert!(cached <= cap, "{cached} pages cached, capacity {cap}");
    }
    let total: u64 = fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert!(total > 2 << 20, "data files hold {total} bytes");

    let db = Database::with_store(PersistentGraph::open(opts).unwrap());
    assert_eq!(
        run(&db, "MATCH (n:N) RETURN count(n), sum(n.i)"),
        vec!["20000, 199990000"]
    );
    assert_eq!(run(&db, "MATCH (n:N {i: 12345}) RETURN n.pad STARTS WITH 'p'"), vec!["true"]);
    assert_eq!(run(&db, "MATCH ()-[r:NEXT]->() RETURN count(r)"), vec!["200"]);
    assert!(db.read(|s| s.cached_pages() <= s.cache_capacity()));
}

// ---- multiple databases -----------------------------------------------------------------

#[test]
fn manager_creates_persistent_databases() {
    let dir = TempDir::new("mgr");
    {
        let mgr = DatabaseManager::open(&dir.0).unwrap();
        let mut s = mgr.session();
        s.execute("CREATE DATABASE big OPTIONS {storage: 'persistent', cacheBytes: 1048576, sync: 'os'}")
            .unwrap();
        assert!(
            s.execute("CREATE DATABASE bad OPTIONS {storage: 'hybrid', cacheBytes: 1}")
                .is_err()
        );
        s.execute("USE big").unwrap();
        s.execute("CREATE (:P {name: 'x'})").unwrap();
    }
    assert!(dir.0.join("databases/big/nodes.db").exists());
    let mgr = DatabaseManager::open(&dir.0).unwrap();
    let mut s = mgr.session();
    let r = s.execute("SHOW DATABASE big").unwrap();
    assert!(format!("{:?}", r.rows).contains("persistent"));
    s.execute("USE big").unwrap();
    let r = s.execute("MATCH (p:P) RETURN p.name").unwrap();
    assert_eq!(r.rows[0][0].to_string(), r#""x""#);
    let db = s.database().unwrap();
    assert!(db.storage_description().contains("1 MiB page cache"));
}

/// A longer run over many seeds: `cargo test --release -- --ignored`.
#[test]
#[ignore]
fn random_workload_many_seeds() {
    for seed in 1..=40u64 {
        let sync = if seed % 2 == 0 {
            SyncPolicy::EveryCommit
        } else {
            SyncPolicy::OsBuffered
        };
        let dense = [0, 1, 3, 50][seed as usize % 4];
        differential(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1, sync, dense);
    }
}

// ---- read errors ------------------------------------------------------------------------

#[test]
fn corrupt_data_fails_queries_instead_of_answering_wrongly() {
    let dir = TempDir::new("badblob");
    run(&dir.open(), "CREATE (:A {x: 1}), (:B {y: 2})"); // closing checkpoints
    // Node 0's blob is block 1 (bytes 128..256 of blobs.db): next u64, then length u32.
    // Claim a length the one-block chain cannot hold.
    let path = dir.0.join("blobs.db");
    let mut bytes = fs::read(&path).unwrap();
    bytes[136..140].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&path, bytes).unwrap();

    let db = dir.open();
    let read = db.execute("MATCH (n:A) RETURN n.x");
    assert!(matches!(read, Err(Error::Storage(_))), "{read:?}");
    // A write that has to read the damaged node fails and leaves nothing behind.
    let write = db.execute("CREATE (:C) WITH 1 AS one MATCH (n:A) SET n.z = 3");
    assert!(matches!(write, Err(Error::Storage(_))), "{write:?}");
    assert_eq!(db.node_count(), 2);
    // Undamaged data still reads fine.
    assert_eq!(run(&db, "MATCH (n:B) RETURN n.y"), vec!["2"]);
}

// ---- scans ------------------------------------------------------------------------------

#[test]
fn scans_are_lazy_and_allow_reads_in_between() {
    let dir = TempDir::new("scan");
    let mut g = PersistentGraph::open(dir.tiny()).unwrap();
    g.begin().unwrap();
    for i in 0..3000i64 {
        let labels: Vec<String> = if i % 3 == 0 { vec!["T".into()] } else { vec![] };
        let mut props = BTreeMap::new();
        props.insert("i".to_string(), Value::Int(i));
        g.create_node(&labels, props).unwrap();
    }
    g.delete_node(NodeId(3), false).unwrap();
    g.commit().unwrap();

    // Batches of the label index (512 keys) and of record pages (128 records) are
    // stitched together in order, and other reads can run while a scan is open.
    let mut seen = Vec::new();
    for id in g.nodes_with_label("T") {
        let id = id.unwrap();
        assert_eq!(g.node_property(id, "i").unwrap(), Some(Value::Int(id.0 as i64)));
        seen.push(id.0);
    }
    let expected: Vec<u64> = (0..3000).step_by(3).filter(|&i| i != 3).collect();
    assert_eq!(seen, expected);
    let all: Vec<u64> = g.all_nodes().map(|n| n.unwrap().0).collect();
    assert_eq!(all.len(), 2999);
    assert!(all.windows(2).all(|w| w[0] < w[1]) && !all.contains(&3));

    // Stopping early only reads what it needs.
    assert_eq!(
        g.all_nodes().take(2).map(|n| n.unwrap().0).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(g.nodes_with_label("Missing").count(), 0);
    assert_eq!(g.all_relationships().count(), 0);
}

#[test]
fn corrupt_index_page_fails_a_label_scan() {
    let dir = TempDir::new("badindex");
    run(&dir.open(), "CREATE (:A), (:A), (:B)");
    // The label index root is the first page of index.db; break its node kind byte.
    let path = dir.0.join("index.db");
    let mut bytes = fs::read(&path).unwrap();
    bytes[8192] = 9;
    fs::write(&path, bytes).unwrap();

    let db = dir.open();
    let r = db.execute("MATCH (n:A) RETURN count(n)");
    assert!(matches!(r, Err(Error::Storage(_))), "{r:?}");
    let scan: Vec<_> = db.read(|s| s.nodes_with_label("A").collect());
    assert!(matches!(scan.as_slice(), [Err(Error::Storage(_))]), "{scan:?}");
    // A full scan does not use the index and still works.
    assert_eq!(run(&db, "MATCH (n) RETURN count(n)"), vec!["3"]);
}

// ---- transaction size cap ---------------------------------------------------------------

#[test]
fn oversized_transactions_fail_cleanly() {
    let dir = TempDir::new("txcap");
    // A 128 KiB cache and a 1 MiB cap.
    let opts = dir.opts().cache_bytes(128 << 10).max_transaction_bytes(1 << 20);
    let db = Database::open(StorageMode::Persistent(opts)).unwrap();
    run(&db, "CREATE INDEX FOR (n:N) ON (n.i)");
    let big = "UNWIND range(1, 20000) AS i CREATE (:N {i: i, pad: 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'})";

    // One query.
    let r = db.execute(big);
    assert!(matches!(r, Err(Error::TransactionTooLarge(_))), "{r:?}");
    assert_eq!(db.node_count(), 0);

    // A statement in an explicit transaction: the whole transaction rolls back.
    let mut tx = db.begin().unwrap();
    tx.execute("CREATE (:Before)").unwrap();
    let r = tx.execute(big);
    assert!(matches!(r, Err(Error::TransactionTooLarge(_))), "{r:?}");
    assert!(!tx.is_open());
    drop(tx);
    assert_eq!(db.node_count(), 0);

    // Several small statements that add up past the cap fail on the one that crosses it.
    // (Each batch has new, increasing keys, so it touches only the end of the index.)
    let small = |batch: usize| {
        format!(
            "UNWIND range({}, {}) AS i CREATE (:N {{i: i, pad: 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'}})",
            batch * 1000 + 1,
            batch * 1000 + 1000
        )
    };
    let mut tx = db.begin().unwrap();
    let mut ok = 0;
    let failed = loop {
        match tx.execute(&small(ok)) {
            Ok(_) => ok += 1,
            Err(e) => break e,
        }
        assert!(ok < 100, "never hit the cap");
    };
    assert!(matches!(failed, Error::TransactionTooLarge(_)), "{failed}");
    assert!(ok >= 1, "a single small statement fits");
    drop(tx);
    assert_eq!(db.node_count(), 0);

    // The same work in batches commits, and the database is fine afterwards.
    for batch in 0..20 {
        run(&db, &small(batch));
    }
    assert_eq!(run(&db, "MATCH (n:N) RETURN count(n)"), vec!["20000"]);
    assert_eq!(run(&db, "MATCH (n:N {i: 19777}) RETURN count(n)"), vec!["1"]);
}
