//! Saved HNSW graphs, end to end on durable stores: they spare the build after a restart,
//! whatever happened to the store since they were saved.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff_core::{BoxedStore, HybridOptions, PersistentOptions, StorageMode, SyncPolicy, Value};

use crate::vector::VectorIndex;
use crate::{Database, DatabaseManager, ExistingDatabase, StorageKind};

type Db = Database<BoxedStore>;

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("graphbuff-hnswfile-{}-{n}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Dir(dir)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const KINDS: [&str; 2] = ["hybrid", "persistent"];

fn open(kind: &str, dir: &Path) -> Db {
    Database::open(match kind {
        "hybrid" => StorageMode::Hybrid(HybridOptions::new(dir).sync(SyncPolicy::OsBuffered)),
        _ => StorageMode::Persistent(PersistentOptions::new(dir).sync(SyncPolicy::OsBuffered)),
    })
    .unwrap()
}

fn run(db: &Db, q: &str) -> Vec<String> {
    let r = db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    r.rows.iter().map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")).collect()
}

/// The graph files in `dir`.
fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("hnsw-"))
        .collect();
    names.sort();
    names
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_name() != "LOCK" {
            fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

const DIMS: usize = 8;

/// Pseudo-random numbers in [-1, 1].
struct Rng(u64);

impl Rng {
    fn vector(&mut self) -> Vec<f64> {
        (0..DIMS)
            .map(|_| {
                self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((self.0 >> 40) as f64 / (1u64 << 24) as f64 * 2000.0).round() / 1000.0 - 1.0
            })
            .collect()
    }

    fn literal(&mut self) -> String {
        format!("{:?}", self.vector())
    }

    /// `n` vectors as a Cypher list.
    fn list(&mut self, n: usize) -> String {
        format!("[{}]", (0..n).map(|_| self.literal()).collect::<Vec<_>>().join(", "))
    }
}

fn create_index(db: &Db, name: &str, label: &str, m: u32) {
    run(
        db,
        &format!(
            "CREATE VECTOR INDEX {name} FOR (n:{label}) ON n.e WITH [n.t] OPTIONS {{indexConfig: \
             {{`vector.dimensions`: {DIMS}, `vector.hnsw.m`: {m}, `vector.hnsw.ef_construction`: 64}}}}"
        ),
    );
}

/// `n` nodes `(:V {k, e, t})`, with an approximate index `v` on them and an exact one, on
/// the same property through label `X`.
fn fill(db: &Db, n: usize, rng: &mut Rng) {
    create_index(db, "v", "V", 8);
    run(db, &format!("CREATE VECTOR INDEX exact FOR (n:X) ON n.e WITH [n.t] OPTIONS {{indexConfig: {{`vector.dimensions`: {DIMS}}}}}"));
    run(db, &format!("UNWIND {} AS e WITH collect(e) AS es UNWIND range(0, size(es) - 1) AS k CREATE (:V:X {{k: k, e: es[k], t: k % 10}})", rng.list(n)));
}

/// The vectors inserted into and taken out of the graph of `v` since it was loaded, or,
/// when it was built, all of them.
fn edits(db: &Db) -> u64 {
    use crate::index_cache::Cached;
    let e = db.vector_cache.entry("v").expect("used by a query");
    assert!(matches!(&*e, VectorIndex::Approximate(_)));
    e.edits()
}

/// Queries the approximate index and compares it with the exact one: the scores must be
/// exact, filtered results all pass, and at least `least` of the true neighbours be found.
fn check(db: &Db, what: &str, least: f64) {
    let mut rng = Rng(4242);
    let (mut hits, mut total) = (0, 0);
    for _ in 0..25 {
        let q = rng.literal();
        let nearest = |index: &str| {
            run(db, &format!("CALL db.index.vector.queryNodes('{index}', 10, {q}) YIELD node, score RETURN id(node), score"))
        };
        let (got, want) = (nearest("v"), nearest("exact"));
        assert_eq!(got.len(), want.len(), "{what}");
        assert_eq!(got.iter().collect::<HashSet<_>>().len(), got.len(), "{what}: a node twice");
        hits += got.iter().filter(|g| want.contains(g)).count();
        total += want.len();
        // Through the filter columns: exactly the exact index's results, since few pass.
        let filtered = |index: &str| {
            run(db, &format!("MATCH (n) SEARCH n IN (VECTOR INDEX {index} FOR {q} WHERE n.t = 3 LIMIT 5) SCORE AS s RETURN id(n), s"))
        };
        assert_eq!(filtered("v"), filtered("exact"), "{what}: filtered");
    }
    assert!(hits as f64 >= least * total as f64, "{what}: {hits} of {total} found");
}

#[test]
fn a_restart_does_not_build_again() {
    for kind in KINDS {
        let dir = Dir::new(kind);
        let db = open(kind, &dir.0);
        fill(&db, 2000, &mut Rng(1));
        assert!(files(&dir.0).is_empty(), "{kind}: no query yet");
        check(&db, "built", 0.95);
        assert_eq!(edits(&db), 2000, "{kind}");
        // Saved by the query that built it.
        assert_eq!(files(&dir.0).len(), 1, "{kind}");
        let q = "CALL db.index.vector.queryNodes('v', 50, [1, 0, 0, 0, 0, 0, 0, 0.5]) YIELD node, score RETURN node.k, score";
        let before = run(&db, q);
        drop(db);
        for round in 0..2 {
            let db = open(kind, &dir.0);
            assert_eq!(run(&db, q), before, "{kind}, {round}: the same graph gives the same results");
            assert_eq!(edits(&db), 0, "{kind}, {round}: loaded");
            check(&db, "loaded", 0.95);
        }
    }
}

#[test]
fn later_commits_are_applied() {
    for kind in KINDS {
        let dir = Dir::new(kind);
        let mut rng = Rng(2);
        let db = open(kind, &dir.0);
        fill(&db, 2000, &mut rng);
        check(&db, "built", 0.95);
        let saved = fs::read(dir.0.join(&files(&dir.0)[0])).unwrap();

        // Changes of every kind: 20 new, 20 gone, 20 changed, 20 out of the index, and
        // filter values.
        run(&db, &format!("UNWIND {} AS e CREATE (:V:X {{k: 5000, e: e, t: 3}})", rng.list(20)));
        run(&db, "MATCH (n:V) WHERE n.k < 20 DETACH DELETE n");
        run(&db, &format!("WITH {} AS es MATCH (n:V) WHERE 100 <= n.k < 120 SET n.e = es[n.k - 100]", rng.list(20)));
        run(&db, "MATCH (n:V) WHERE 200 <= n.k < 220 REMOVE n:V:X");
        run(&db, "MATCH (n:V) WHERE n.k % 100 = 50 SET n.t = 3");

        // The process stops without closing the database: the graph saved is the first one.
        let crash = Dir::new("crash");
        run(&db, "MATCH (n:V) WHERE n.k = 300 SET n.x = 1");
        db.checkpoint().unwrap();
        copy_dir(&dir.0, &crash.0);
        fs::write(crash.0.join(&files(&dir.0)[0]), &saved).unwrap();
        let after = open(kind, &crash.0);
        check(&after, "after a crash", 0.93);
        // 20 new, 20 gone, 20 changed (out and in again), 20 out of the index.
        assert_eq!(edits(&after), 20 + 20 + 40 + 20, "{kind}");
        drop(after);
        // Closing saved what was inserted.
        let after = open(kind, &crash.0);
        check(&after, "after a crash and a close", 0.93);
        assert_eq!(edits(&after), 0, "{kind}");

        // The graph kept up with the commits and is saved on closing.
        check(&db, "kept up", 0.93);
        drop(db);
        let db = open(kind, &dir.0);
        check(&db, "reopened", 0.93);
        assert_eq!(edits(&db), 0, "{kind}");
    }
}

#[test]
fn a_graph_newer_than_its_store() {
    // Commits that a crash lost, or a backup put in the place of the store.
    for kind in KINDS {
        let (dir, earlier) = (Dir::new(kind), Dir::new("earlier"));
        let mut rng = Rng(3);
        let db = open(kind, &dir.0);
        fill(&db, 1500, &mut rng);
        db.checkpoint().unwrap();
        copy_dir(&dir.0, &earlier.0);
        run(&db, &format!("UNWIND {} AS e CREATE (:V:X {{k: 5000, e: e, t: 3}})", rng.list(100)));
        run(&db, &format!("WITH {} AS es MATCH (n:V) WHERE n.k < 50 SET n.e = es[n.k]", rng.list(50)));
        run(&db, "MATCH (n:V) WHERE 1000 <= n.k < 1030 DETACH DELETE n");
        check(&db, "the store", 0.95);
        drop(db);
        assert!(files(&earlier.0).is_empty());
        let name = &files(&dir.0)[0];
        fs::copy(dir.0.join(name), earlier.0.join(name)).unwrap();
        let db = open(kind, &earlier.0);
        check(&db, "the earlier store", 0.93);
        // 100 that never were, 50 of other vectors (out and in), 30 that are there after all.
        assert_eq!(edits(&db), 100 + 2 * 50 + 30, "{kind}");
    }
}

#[test]
fn graphs_that_cannot_be_used_are_built() {
    for kind in KINDS {
        let dir = Dir::new(kind);
        let db = open(kind, &dir.0);
        fill(&db, 1200, &mut Rng(4));
        check(&db, "built", 0.95);
        drop(db);
        let path = dir.0.join(&files(&dir.0)[0]);
        let good = fs::read(&path).unwrap();
        let damaged = {
            let mut d = good.clone();
            let middle = d.len() / 2;
            d[middle] ^= 4;
            d
        };
        for (what, bytes) in [("cut", &good[..good.len() / 3]), ("empty", &[]), ("damaged", &damaged[..]), ("text", b"hello, graph")] {
            fs::write(&path, bytes).unwrap();
            let db = open(kind, &dir.0);
            check(&db, what, 0.95);
            assert_eq!(edits(&db), 1200, "{kind}: {what}");
            drop(db);
            // And saved anew.
            assert_eq!(fs::read(&path).unwrap(), good, "{kind}: {what}");
        }
        // Too many other vectors.
        let db = open(kind, &dir.0);
        run(&db, &format!("WITH {} AS es MATCH (n:V) WHERE n.k < 400 SET n.e = es[n.k]", Rng(5).list(400)));
        drop(db);
        fs::write(&path, &good).unwrap();
        let db = open(kind, &dir.0);
        check(&db, "other vectors", 0.95);
        assert_eq!(edits(&db), 1200, "{kind}: other vectors");
        drop(db);
        // A folder in its place: built, and not saved.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let db = open(kind, &dir.0);
        check(&db, "a folder", 0.95);
        drop(db);
        assert!(path.is_dir(), "{kind}");
    }
}

#[test]
fn files_come_and_go_with_their_indexes() {
    for kind in KINDS {
        let dir = Dir::new(kind);
        let mut rng = Rng(6);
        let db = open(kind, &dir.0);
        fill(&db, 1100, &mut rng);
        check(&db, "built", 0.95);
        let first = files(&dir.0);
        assert_eq!(first.len(), 1);
        // Dropped in a transaction that is rolled back: kept.
        let mut tx = db.begin().unwrap();
        tx.execute("DROP INDEX v").unwrap();
        drop(tx);
        assert_eq!(files(&dir.0), first, "{kind}");
        run(&db, "DROP INDEX v");
        assert!(files(&dir.0).is_empty(), "{kind}");
        // Another one of its name.
        create_index(&db, "v", "V", 4);
        check(&db, "again", 0.9);
        assert_eq!(edits(&db), 1100);
        let second = fs::read(dir.0.join(&first[0])).unwrap();
        drop(db);

        // Dropped without its contents in memory, and recreated: the graph of the other
        // definition is not used.
        let db = open(kind, &dir.0);
        run(&db, "DROP INDEX v");
        create_index(&db, "v", "V", 8);
        fs::write(dir.0.join(&first[0]), &second).unwrap();
        check(&db, "recreated", 0.95);
        assert_eq!(edits(&db), 1100, "{kind}");
        run(&db, "DROP INDEX v");
        drop(db);

        // What is left of indexes that are gone goes when the store is opened; other files
        // stay.
        for name in [&first[0][..], "hnsw-0123456789abcdef.tmp", "hnsw-notes.graph", "hnsw-0123456789abcdef.txt", "hnsw-.graph"] {
            fs::write(dir.0.join(name), b"x").unwrap();
        }
        let db = open(kind, &dir.0);
        assert_eq!(files(&dir.0), ["hnsw-.graph", "hnsw-0123456789abcdef.txt", "hnsw-notes.graph"], "{kind}");
        assert_eq!(run(&db, "MATCH (n:V) RETURN count(n)"), ["1100"]);
    }
}

#[test]
fn when_graphs_are_saved() {
    for kind in KINDS {
        let dir = Dir::new(kind);
        let mut rng = Rng(7);
        let db = open(kind, &dir.0);
        fill(&db, 900, &mut rng);
        check(&db, "few", 0.95);
        db.checkpoint().unwrap();
        assert!(files(&dir.0).is_empty(), "{kind}: not worth it");

        run(&db, &format!("UNWIND {} AS e CREATE (:V:X {{k: 1, e: e, t: 3}})", rng.list(200)));
        check(&db, "more", 0.95);
        assert!(files(&dir.0).is_empty(), "{kind}: only 200 more");
        db.checkpoint().unwrap();
        let path = dir.0.join(&files(&dir.0)[0]);
        let first = fs::read(&path).unwrap();

        // Nothing changed, or only what the file does not hold: not written again.
        run(&db, "MATCH (n:V) WHERE n.k = 1 SET n.t = 4, n.y = 1");
        check(&db, "filter values", 0.95);
        fs::remove_file(&path).unwrap();
        db.checkpoint().unwrap();
        assert!(!path.exists(), "{kind}");
        fs::write(&path, &first).unwrap();

        // Enough changes: by the query that applies them.
        run(&db, &format!("UNWIND {} AS e CREATE (:V:X {{k: 2, e: e, t: 3}})", rng.list(1000)));
        assert_eq!(fs::read(&path).unwrap(), first, "{kind}");
        check(&db, "many more", 0.95);
        let second = fs::read(&path).unwrap();
        assert_ne!(second, first, "{kind}");
        // A few: on a checkpoint, and on closing.
        run(&db, "MATCH (n:V) WHERE n.k = 7 DETACH DELETE n");
        check(&db, "one less", 0.95);
        assert_eq!(fs::read(&path).unwrap(), second, "{kind}");
        db.checkpoint().unwrap();
        let third = fs::read(&path).unwrap();
        assert_ne!(third, second, "{kind}");
        run(&db, "MATCH (n:V) WHERE n.k = 8 DETACH DELETE n");
        check(&db, "another", 0.95);
        drop(db);
        assert_ne!(fs::read(&path).unwrap(), third, "{kind}");
        let db = open(kind, &dir.0);
        check(&db, "reopened", 0.95);
        assert_eq!(edits(&db), 0, "{kind}");
        // Down to a few again: the file goes.
        run(&db, "MATCH (n:V) WHERE n.k > 1 DETACH DELETE n");
        check(&db, "few again", 0.9);
        drop(db);
        assert!(files(&dir.0).is_empty(), "{kind}");
    }
}

/// A graph that was built again, or whose contents were forgotten and loaded, has been
/// changed less than the one before it, and is saved all the same.
#[test]
fn new_contents_are_saved() {
    let dir = Dir::new("new-contents");
    let mut rng = Rng(13);
    let db = open("hybrid", &dir.0);
    fill(&db, 1100, &mut rng);
    check(&db, "built", 0.95);
    for round in 0..3 {
        // Every vector replaced: each time, the graph is built again or nearly.
        run(&db, &format!("WITH {} AS es MATCH (n:V) SET n.e = es[n.k]", rng.list(1100)));
        check(&db, "replaced", 0.9);
        run(&db, &format!("WITH {} AS es MATCH (n:V) WHERE n.k < 30 SET n.e = es[n.k]", rng.list(30)));
        check(&db, "some replaced", 0.9);
        db.checkpoint().unwrap();
        let copy = Dir::new("new-contents-copy");
        copy_dir(&dir.0, &copy.0);
        let after = open("hybrid", &copy.0);
        check(&after, "copied", 0.9);
        assert_eq!(edits(&after), 0, "round {round}");
    }
    // Forgotten, as after a commit of very many changes.
    run(&db, &format!("UNWIND {} AS e CREATE (:V:X {{k: 5000, e: e, t: 3}})", rng.list(40)));
    db.vector_cache.published(&db.snapshot().unwrap(), graphbuff_core::Changes::unknown());
    check(&db, "loaded again", 0.9);
    assert_eq!(edits(&db), 40);
    drop(db);
    let db = open("hybrid", &dir.0);
    check(&db, "reopened", 0.9);
    assert_eq!(edits(&db), 0);
}

#[test]
fn relationships_euclidean_and_memory() {
    for kind in ["hybrid", "persistent", "memory"] {
        let dir = Dir::new(kind);
        let open_it = || if kind == "memory" { Database::open(StorageMode::InMemory).unwrap() } else { open(kind, &dir.0) };
        let db = open_it();
        run(
            &db,
            &format!(
                "CREATE VECTOR INDEX v FOR ()-[r:R]-() ON (r.e) OPTIONS {{indexConfig: {{`vector.dimensions`: {DIMS}, \
                 `vector.hnsw.enabled`: true, `vector.similarity_function`: 'euclidean'}}}}"
            ),
        );
        run(&db, &format!("CREATE (a:A) WITH a UNWIND {} AS e CREATE (a)-[:R {{e: e}}]->(:B)", Rng(8).list(1300)));
        let q = "CALL db.index.vector.queryRelationships('v', 20, [0, 0, 0, 0, 0, 0, 0, 0]) YIELD relationship, score RETURN id(relationship), score";
        let before = run(&db, q);
        assert_eq!(before.len(), 20);
        db.checkpoint().unwrap();
        if kind == "memory" {
            assert!(!dir.0.exists());
            continue;
        }
        drop(db);
        assert_eq!(files(&dir.0).len(), 1);
        let db = open_it();
        assert_eq!(run(&db, q), before, "{kind}");
        assert_eq!(edits(&db), 0, "{kind}");
    }
}

/// Queries running while others commit and the graph is saved.
#[test]
fn concurrent_queries_and_commits() {
    let dir = Dir::new("concurrent");
    let db = Arc::new(open("hybrid", &dir.0));
    fill(&db, 1200, &mut Rng(9));
    std::thread::scope(|s| {
        for t in 0..3 {
            let db = &db;
            s.spawn(move || {
                let mut rng = Rng(100 + t);
                for _ in 0..40 {
                    let q = rng.literal();
                    let r = run(db, &format!("CALL db.index.vector.queryNodes('v', 5, {q}) YIELD score RETURN score"));
                    assert_eq!(r.len(), 5);
                }
            });
        }
        let db = &db;
        s.spawn(move || {
            let mut rng = Rng(200);
            for i in 0..12 {
                run(db, &format!("UNWIND {} AS e CREATE (:V:X {{k: 9000, e: e, t: 3}})", rng.list(150)));
                if i % 4 == 3 {
                    db.checkpoint().unwrap();
                }
            }
        });
    });
    check(&db, "afterwards", 0.95);
    drop(db);
    let db = open("hybrid", &dir.0);
    check(&db, "reopened", 0.95);
    assert_eq!(edits(&db), 0);
}

#[test]
fn managed_databases() {
    let dir = Dir::new("managed");
    let backup = Dir::new("backup");
    let root = dir.0.join("databases");
    let mgr = DatabaseManager::open(&dir.0).unwrap();
    for (name, storage) in [("h", StorageKind::hybrid()), ("p", StorageKind::persistent())] {
        mgr.create_database(name, Some(storage), ExistingDatabase::Fail).unwrap();
        let db = mgr.database(name).unwrap();
        fill(&db, 1200, &mut Rng(10));
        check(&db, name, 0.95);
        assert_eq!(files(&root.join(name)).len(), 1, "{name}");

        // A backup has no graph; a database restored from it builds one.
        let to = backup.0.join(name);
        mgr.backup_database(name, &to).unwrap();
        assert!(files(&to).is_empty(), "{name}");
        mgr.restore_database(&format!("{name}2"), &to, false).unwrap();
        let restored = mgr.database(&format!("{name}2")).unwrap();
        check(&restored, "restored", 0.95);
        assert_eq!(edits(&restored), 1200, "{name}");
        drop(restored);

        // Closing a database that is dropped leaves nothing behind.
        run(&db, "MATCH (n:V) WHERE n.k = 1 DETACH DELETE n");
        check(&db, "changed", 0.95);
        drop(db);
        mgr.drop_database(name, false).unwrap();
        assert!(!root.join(name).exists(), "{name}");
        // Another one of its name starts anew.
        mgr.create_database(name, Some(StorageKind::hybrid()), ExistingDatabase::Fail).unwrap();
        let db = mgr.database(name).unwrap();
        assert!(files(&root.join(name)).is_empty(), "{name}");
        fill(&db, 1100, &mut Rng(11));
        check(&db, "anew", 0.95);
        assert_eq!(edits(&db), 1100, "{name}");
    }
}

/// Parameters reach an index the same way; a query with a timeout that stops a load or a
/// build leaves nothing half done.
#[test]
fn interrupted_queries() {
    let dir = Dir::new("interrupted");
    let db = open("hybrid", &dir.0);
    fill(&db, 1500, &mut Rng(12));
    let cancel = crate::CancelHandle::new();
    cancel.cancel();
    let options = crate::QueryOptions::default().with_cancel(cancel);
    let q = "CALL db.index.vector.queryNodes('v', 5, $q) YIELD score RETURN score";
    let params: BTreeMap<String, Value> = [("q".to_string(), Value::List(vec![Value::Float(0.5); DIMS]))].into();
    assert!(db.execute_with_options(q, &params, &options).is_err());
    assert!(files(&dir.0).is_empty());
    check(&db, "built", 0.95);
    drop(db);
    let db = open("hybrid", &dir.0);
    assert!(db.execute_with_options(q, &params, &options).is_err());
    assert_eq!(files(&dir.0).len(), 1);
    check(&db, "loaded", 0.95);
    assert_eq!(edits(&db), 0);
}

/// The first query after a restart, with and without the saved graph.
#[test]
#[ignore = "prints measurements"]
fn restart_time() {
    use graphbuff_core::GraphStore;
    let (n, dims) = (100_000, 128);
    for kind in KINDS {
        let dir = Dir::new(kind);
        let mut seed = 3u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 40) as f64 / (1u64 << 24) as f64 * 2.0 - 1.0
        };
        let centers: Vec<Vec<f64>> = (0..1000).map(|_| (0..dims).map(|_| next()).collect()).collect();
        let mut store = match kind {
            "hybrid" => graphbuff_core::open_store(&StorageMode::Hybrid(HybridOptions::new(&dir.0).sync(SyncPolicy::OsBuffered))),
            _ => graphbuff_core::open_store(&StorageMode::Persistent(
                PersistentOptions::new(&dir.0).sync(SyncPolicy::OsBuffered).max_transaction_bytes(0),
            )),
        }
        .unwrap();
        for batch in 0..n / 5000 {
            store.begin().unwrap();
            for i in 0..5000 {
                let c = &centers[(batch * 5000 + i) % 1000];
                let e = Value::List(c.iter().map(|x| Value::Float(x + 0.3 * next())).collect());
                store.create_node(&["V".into()], BTreeMap::from([("e".to_string(), e)])).unwrap();
            }
            store.commit().unwrap();
        }
        store.checkpoint().unwrap();
        drop(store);
        let q = format!("CALL db.index.vector.queryNodes('v', 10, {:?}) YIELD score RETURN score", centers[0]);
        let first = |what: &str| {
            let db = open(kind, &dir.0);
            if what == "built" {
                run(&db, &format!("CREATE VECTOR INDEX v FOR (n:V) ON n.e OPTIONS {{indexConfig: {{`vector.dimensions`: {dims}, `vector.hnsw.enabled`: true}}}}"));
            }
            let t = std::time::Instant::now();
            let r = run(&db, &q);
            let seconds = t.elapsed().as_secs_f64();
            println!("{kind}: first query, {what}: {seconds:.2} s, edits {}, file {:.1} MiB", edits(&db), fs::metadata(dir.0.join(&files(&dir.0)[0])).unwrap().len() as f64 / (1 << 20) as f64);
            let t = std::time::Instant::now();
            drop(db);
            println!("{kind}: closing {:.2} s", t.elapsed().as_secs_f64());
            r
        };
        let a = first("built");
        assert_eq!(a, first("loaded"));
        let path = dir.0.join(&files(&dir.0)[0]);
        fs::remove_file(path).unwrap();
        assert_eq!(a, first("without its file"));
    }
}
