//! Storage modes: durability, recovery and checkpoints of hybrid storage.
//! Persistent storage is tested in `persistent.rs`.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{
    BoxedStore, Database, Error, GraphStore, HybridGraph, HybridOptions, StorageMode, SyncPolicy,
};

/// A fresh directory under the system temp dir, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-test-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        TempDir(dir)
    }

    fn opts(&self) -> HybridOptions {
        HybridOptions::new(&self.0)
    }

    fn open(&self) -> Database<BoxedStore> {
        Database::open(StorageMode::Hybrid(self.opts())).unwrap()
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

const SETUP: &[&str] = &[
    "CREATE (:Person {name: 'Ann', age: 31})-[:KNOWS {since: 2020}]->(:Person {name: 'Bob', tags: ['x', 'y']})",
    "CREATE INDEX person_name FOR (p:Person) ON (p.name)",
    "CREATE (:Person {name: 'Cid'}), (:Temp)",
    "MATCH (t:Temp) DELETE t",
    "MATCH (p:Person {name: 'Ann'}) SET p.age = 32, p:Admin",
    "MATCH (p:Person {name: 'Cid'}) REMOVE p:Person",
];

fn assert_setup_state<S: GraphStore>(db: &Database<S>) {
    assert_eq!(db.node_count(), 3);
    assert_eq!(db.relationship_count(), 1);
    assert_eq!(
        run(
            db,
            "MATCH (a)-[k:KNOWS]->(b) RETURN a.name, a.age, labels(a), k.since, b.tags"
        ),
        vec![r#""Ann", 32, ["Person", "Admin"], 2020, ["x", "y"]"#]
    );
    assert_eq!(
        run(db, "MATCH (p:Person) RETURN p.name ORDER BY p.name"),
        vec![r#""Ann""#, r#""Bob""#]
    );
    let indexes = db.read(|s| s.indexes());
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "person_name");
    assert!(
        db.read(|s| s.index_seek("Person", "name", &"Bob".into()))
            .unwrap()
            .is_some()
    );
}

#[test]
fn in_memory_mode_via_open() {
    let db = Database::open(StorageMode::InMemory).unwrap();
    for q in SETUP {
        run(&db, q);
    }
    assert_setup_state(&db);
    assert_eq!(db.storage_description(), "in-memory");
}

#[test]
fn hybrid_survives_reopen() {
    let dir = TempDir::new("reopen");
    {
        let db = dir.open();
        for q in SETUP {
            run(&db, q);
        }
        assert_setup_state(&db);
    }
    let db = dir.open();
    assert_setup_state(&db);
    // Ids are not reused after recovery: the deleted :Temp node's id stays retired.
    let max_before = run(&db, "MATCH (n) RETURN max(id(n))")[0]
        .parse::<i64>()
        .unwrap();
    let new_id = run(&db, "CREATE (n:New) RETURN id(n)")[0]
        .parse::<i64>()
        .unwrap();
    assert_eq!(new_id, max_before + 2);
}

#[test]
fn failed_queries_are_not_logged() {
    let dir = TempDir::new("rollback");
    {
        let db = dir.open();
        run(&db, "CREATE (:Keep)");
        let err = db.execute("CREATE (:Lost) WITH 1 AS one RETURN one / 0");
        assert!(err.is_err());
        assert_eq!(db.node_count(), 1);
    }
    let db = dir.open();
    assert_eq!(run(&db, "MATCH (n) RETURN labels(n)"), vec![r#"["Keep"]"#]);
    // The next id after recovery matches what the live store would have handed out.
    assert_eq!(run(&db, "CREATE (n) RETURN id(n)"), vec!["1"]);
}

#[test]
fn checkpoint_compacts_log_and_keeps_data() {
    let dir = TempDir::new("checkpoint");
    {
        let db = dir.open();
        for q in SETUP {
            run(&db, q);
        }
        let wal_before = fs::metadata(dir.wal()).unwrap().len();
        db.checkpoint().unwrap();
        let wal_after = fs::metadata(dir.wal()).unwrap().len();
        assert!(wal_after < wal_before);
        assert_eq!(wal_after, 8, "log holds only its header after a checkpoint");
        run(&db, "CREATE (:AfterCheckpoint)");
    }
    let db = dir.open();
    assert_eq!(db.node_count(), 4);
    assert_eq!(
        run(&db, "MATCH (n:AfterCheckpoint) RETURN count(n)"),
        vec!["1"]
    );
    assert_eq!(db.read(|s| s.indexes()).len(), 1);
}

#[test]
fn automatic_checkpoint() {
    let dir = TempDir::new("autockpt");
    {
        let store = HybridGraph::open(dir.opts().checkpoint_after_bytes(256)).unwrap();
        let db = Database::with_store(store);
        for i in 0..50 {
            run(
                &db,
                &format!("CREATE (:N {{i: {i}, pad: 'xxxxxxxxxxxxxxxxxxxxxxxx'}})"),
            );
        }
        assert!(db.read(|s| s.wal_size()) <= 256 + 200);
        assert!(dir.0.join("graph.snapshot").exists());
    }
    let db = dir.open();
    assert_eq!(
        run(&db, "MATCH (n:N) RETURN count(n), sum(n.i)"),
        vec!["50, 1225"]
    );
}

#[test]
fn torn_log_tail_is_discarded() {
    let dir = TempDir::new("torn");
    {
        let db = dir.open();
        run(&db, "CREATE (:A)");
        run(&db, "CREATE (:B)");
    }
    let good_len = fs::metadata(dir.wal()).unwrap().len();
    // Simulate a crash halfway through writing the next record.
    let mut f = OpenOptions::new().append(true).open(dir.wal()).unwrap();
    f.write_all(&[200, 0, 0, 0, 1, 2, 3, 4, 9, 9]).unwrap();
    drop(f);

    let db = dir.open();
    assert_eq!(db.node_count(), 2);
    assert_eq!(fs::metadata(dir.wal()).unwrap().len(), good_len);
    run(&db, "CREATE (:C)");
    drop(db);
    assert_eq!(dir.open().node_count(), 3);
}

#[test]
fn corrupt_record_checksum_stops_replay_there() {
    let dir = TempDir::new("crc");
    {
        let db = dir.open();
        run(&db, "CREATE (:A)");
        run(&db, "CREATE (:B)");
    }
    // Flip the last byte of the log, inside the second record's payload.
    let mut bytes = fs::read(dir.wal()).unwrap();
    *bytes.last_mut().unwrap() ^= 0xFF;
    fs::write(dir.wal(), &bytes).unwrap();
    let db = dir.open();
    assert_eq!(run(&db, "MATCH (n) RETURN labels(n)"), vec![r#"["A"]"#]);
}

#[test]
fn log_records_covered_by_snapshot_are_skipped() {
    let dir = TempDir::new("skip");
    {
        let db = dir.open();
        run(&db, "CREATE (:A)");
        run(&db, "CREATE (:B)");
        let old_log = fs::read(dir.wal()).unwrap();
        db.checkpoint().unwrap();
        drop(db);
        // Crash between replacing the snapshot and emptying the log.
        fs::write(dir.wal(), old_log).unwrap();
    }
    let db = dir.open();
    assert_eq!(db.node_count(), 2);
    run(&db, "CREATE (:C)");
    drop(db);
    assert_eq!(dir.open().node_count(), 3);
}

#[test]
fn corrupt_snapshot_is_an_error() {
    let dir = TempDir::new("badsnap");
    {
        let db = dir.open();
        run(&db, "CREATE (:A)");
        db.checkpoint().unwrap();
    }
    let path = dir.0.join("graph.snapshot");
    let mut bytes = fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    fs::write(&path, bytes).unwrap();
    assert!(matches!(
        Database::open(StorageMode::Hybrid(dir.opts())),
        Err(Error::Storage(_))
    ));
}

#[test]
fn directory_is_locked_while_open() {
    let dir = TempDir::new("lock");
    let db = dir.open();
    assert!(matches!(
        Database::open(StorageMode::Hybrid(dir.opts())),
        Err(Error::Storage(_))
    ));
    drop(db);
    dir.open();
}

#[test]
fn direct_store_writes_autocommit() {
    let dir = TempDir::new("direct");
    {
        let mut g = HybridGraph::open(dir.opts().sync(SyncPolicy::OsBuffered)).unwrap();
        let mut props = BTreeMap::new();
        props.insert("k".to_string(), 1.into());
        let a = g.create_node(&["X".into()], props).unwrap();
        g.set_node_property(a, "k", 2.into()).unwrap();
        // A failing write outside a transaction leaves nothing behind.
        assert!(
            g.set_node_property(a, "m", graphbuff::Value::Map(BTreeMap::new()))
                .is_err()
        );
        assert_eq!(g.last_sequence(), 2);
    }
    let g = HybridGraph::open(dir.opts()).unwrap();
    assert_eq!(g.node_count(), 1);
    assert_eq!(
        g.node_property(graphbuff::NodeId(0), "k").unwrap(),
        Some(2.into())
    );
}
