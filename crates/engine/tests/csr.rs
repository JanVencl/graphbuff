//! CSR adjacency export: the export matches the store's own expansions on every
//! kind of storage, and `Database::csr` follows commits.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{BoxedStore, Csr, Database, Direction, GraphStore, Snapshot, StorageMode};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-csrtest-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn exec<S: GraphStore>(db: &Database<S>, q: &str) {
    db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}"));
}

/// A random multigraph with self-loops, a dense node, three types and some deletes.
fn load<S: GraphStore>(db: &Database<S>) {
    exec(db, "UNWIND range(0, 299) AS i CREATE (:N {id: i})");
    exec(db, "MATCH (n:N {id: 0}) SET n:Hub");
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = |m: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % m
    };
    let mut rows = Vec::new();
    for _ in 0..1500 {
        let a = if next(5) == 0 { 0 } else { next(300) };
        let b = if next(10) == 0 { a } else { next(300) };
        let t = ["A", "B", "C"][next(3) as usize];
        rows.push(format!("[{a},{b},'{t}']"));
    }
    for t in ["A", "B", "C"] {
        exec(
            db,
            &format!(
                "UNWIND [{}] AS r WITH r WHERE r[2] = '{t}' \
                 MATCH (a:N {{id: r[0]}}), (b:N {{id: r[1]}}) CREATE (a)-[:{t}]->(b)",
                rows.join(",")
            ),
        );
    }
    exec(db, "MATCH (n:N) WHERE n.id % 37 = 5 DETACH DELETE n");
    exec(db, "MATCH ()-[r:B]->() WHERE id(r) % 11 = 3 DELETE r");
    exec(db, "CREATE (:Lonely)");
}

fn sorted(mut v: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    v.sort_unstable();
    v
}

/// Every node, direction and type filter gives the same relationships as the store.
fn compare(store: &dyn GraphStore, csr: &Csr) {
    assert_eq!(csr.node_count(), store.node_count());
    assert_eq!(csr.relationship_count(), store.relationship_count());
    let mut types: Vec<String> = csr.types().to_vec();
    types.sort();
    let mut store_types: Vec<String> = store
        .relationship_types()
        .into_iter()
        .filter(|t| store.relationship_type_count(t) > 0)
        .collect();
    store_types.sort();
    assert_eq!(types, store_types);
    let filters: Vec<Vec<String>> = vec![
        vec![],
        vec!["A".into()],
        vec!["B".into(), "C".into()],
        vec!["C".into(), "Missing".into()],
    ];
    for n in store.all_nodes() {
        let n = n.unwrap();
        assert!(csr.node_index(n).is_some());
        for dir in [Direction::Outgoing, Direction::Incoming, Direction::Both] {
            for f in &filters {
                let want = sorted(
                    store.expand(n, dir, f).map(|e| e.unwrap()).map(|e| (e.rel.0, e.other.0)).collect(),
                );
                let got = sorted(csr.expand(n, dir, f).iter().map(|e| (e.rel.0, e.other.0)).collect());
                assert_eq!(got, want, "node {n:?} {dir:?} {f:?}");
                assert_eq!(csr.degree(n, dir, f), want.len());
            }
        }
    }
    // Rows are sorted by target, then relationship.
    for t in 0..csr.types().len() as u32 {
        for dir in [Direction::Outgoing, Direction::Incoming] {
            let a = csr.adjacency(t, dir);
            assert_eq!(a.offsets().len(), csr.node_count() + 1);
            assert_eq!(*a.offsets().last().unwrap() as usize, a.len());
            for i in 0..csr.node_count() as u32 {
                let row: Vec<_> = a.neighbors(i).iter().zip(a.rels_of(i)).collect();
                assert!(row.windows(2).all(|w| w[0] <= w[1]), "row {i} of type {t} {dir:?}");
            }
        }
    }
}

fn check_store(db: &Database<BoxedStore>) {
    load(db);
    let csr = db.csr().unwrap();
    let snapshot: Snapshot = db.snapshot().expect("all three stores take snapshots");
    compare(&*snapshot, &csr);
    assert!(csr.memory_bytes() > 0);
}

#[test]
fn matches_memory_store() {
    check_store(&Database::open(StorageMode::InMemory).unwrap());
}

#[test]
fn matches_hybrid_store() {
    let dir = TempDir::new("hybrid");
    check_store(&Database::open(StorageMode::hybrid(&dir.0)).unwrap());
}

#[test]
fn matches_persistent_store() {
    let dir = TempDir::new("persistent");
    let db = Database::open(StorageMode::persistent(&dir.0)).unwrap();
    check_store(&db);
    // The densified hub is exported like any other node.
    let hub = db.execute("MATCH (h:Hub) RETURN id(h) AS id").unwrap();
    assert_eq!(hub.rows.len(), 1);
}

#[test]
fn empty_store() {
    let db = Database::in_memory();
    let csr = db.csr().unwrap();
    assert_eq!(csr.node_count(), 0);
    assert_eq!(csr.relationship_count(), 0);
    assert!(csr.types().is_empty());
}

#[test]
fn follows_commits() {
    let db = Database::in_memory();
    exec(&db, "CREATE (a:P)-[:KNOWS]->(b:P)");
    let first = db.csr().unwrap();
    assert!(Arc::ptr_eq(&first, &db.csr().unwrap()), "cached until the next commit");
    // A read-only query commits nothing.
    exec(&db, "MATCH (n) RETURN count(n)");
    assert!(Arc::ptr_eq(&first, &db.csr().unwrap()));

    exec(&db, "MATCH (a:P), (b:P) WHERE id(a) < id(b) CREATE (b)-[:LIKES]->(a)");
    let second = db.csr().unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(first.relationship_count(), 1);
    assert_eq!(second.relationship_count(), 2);
    assert_eq!(second.types(), ["KNOWS", "LIKES"]);

    // An open transaction's changes are not exported; its commit is.
    let mut tx = db.begin().unwrap();
    tx.execute("CREATE (:P)-[:KNOWS]->(:P)").unwrap();
    let during = std::thread::scope(|s| s.spawn(|| db.csr().unwrap()).join().unwrap());
    assert!(Arc::ptr_eq(&during, &second));
    tx.commit().unwrap();
    assert_eq!(db.csr().unwrap().relationship_count(), 3);

    // Exports taken earlier stay valid.
    assert_eq!(first.node_count(), 2);
    let a = first.node_index(first.nodes()[0]).unwrap();
    assert_eq!(first.adjacency(0, Direction::Outgoing).degree(a), 1);
}

/// More types than the per-type passes take: one untyped pass with type lookups.
#[test]
fn many_types() {
    let dir = TempDir::new("many");
    for db in [
        Database::open(StorageMode::InMemory).unwrap(),
        Database::open(StorageMode::persistent(&dir.0)).unwrap(),
    ] {
        exec(&db, "UNWIND range(0, 99) AS i CREATE (:N {id: i})");
        for (k, t) in ["T0", "T1", "T2", "T3", "T4", "T5", "C"].iter().enumerate() {
            exec(
                &db,
                &format!(
                    "MATCH (a:N), (b:N) WHERE (a.id * 7 + b.id) % 23 = 0 AND (a.id + b.id) % 7 = {k}                      CREATE (a)-[:{t}]->(b)"
                ),
            );
        }
        exec(&db, "MATCH ()-[r:T3]->() DELETE r");
        let csr = db.csr().unwrap();
        assert_eq!(csr.types().len(), 6, "{:?}", csr.types());
        compare(&*db.snapshot().unwrap(), &csr);
    }
}
