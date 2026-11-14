//! Online backup, restore and consistency checks (`Database::backup`, `Database::check`,
//! `DatabaseManager::backup_database` / `restore_database` / `check_database`).

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use graphbuff::{BACKUP_MARKER, Database, DatabaseManager, Error, StorageKind, StorageMode};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-backup-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        TempDir(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn int(db: &Database<graphbuff::BoxedStore>, q: &str) -> i64 {
    let r = db.execute(q).unwrap_or_else(|e| panic!("`{q}` failed: {e}"));
    r.rows[0][0].as_int().unwrap()
}

fn persistent_manager(t: &TempDir) -> DatabaseManager {
    DatabaseManager::open_with(t.join("data"), StorageKind::persistent()).unwrap()
}

#[test]
fn backup_while_a_writer_commits() {
    let t = TempDir::new("online");
    let mgr = Arc::new(persistent_manager(&t));
    let db = mgr.database("graphbuff").unwrap();
    db.execute("CREATE INDEX FOR (n:N) ON (n.i)").unwrap();
    db.execute("UNWIND range(1, 2000) AS i CREATE (:N {i: i})-[:R]->(:M {i: i})").unwrap();

    // Each commit adds a pair joined by a relationship, so every committed state has
    // twice as many nodes as relationships.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (db, stop) = (db.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut i = 10_000;
            while !stop.load(Ordering::Relaxed) {
                db.execute(&format!("CREATE (:N {{i: {i}}})-[:R]->(:M {{i: {i}}})")).unwrap();
                i += 1;
            }
            i - 10_000
        })
    };
    let mut backups = Vec::new();
    for k in 0..3 {
        let dest = t.join(&format!("backup{k}"));
        let info = mgr.backup_database("graphbuff", &dest).unwrap();
        assert_eq!(info.format, "persistent");
        assert_eq!(info.nodes, 2 * info.relationships);
        backups.push((dest, info));
    }
    // A check runs online too.
    let report = mgr.check_database("graphbuff").unwrap();
    stop.store(true, Ordering::Relaxed);
    let written = writer.join().unwrap();
    assert!(report.is_consistent(), "{report}");
    assert!(written > 0);

    for (k, (dest, info)) in backups.iter().enumerate() {
        let name = format!("restored{k}");
        let restored = mgr.restore_database(&name, dest, false).unwrap();
        assert_eq!(restored.storage.name(), "persistent");
        assert_eq!(restored.nodes as u64, info.nodes);
        let r = mgr.database(&name).unwrap();
        assert_eq!(int(&r, "MATCH (n:N) RETURN count(n)") as u64, info.relationships);
        assert_eq!(int(&r, "MATCH (:N)-[:R]->(m:M) RETURN count(m)") as u64, info.relationships);
        // The index came along and is used.
        assert!(r.explain("MATCH (n:N {i: 7}) RETURN n").unwrap().contains("NodeIndexSeek"));
        assert_eq!(int(&r, "MATCH (n:N {i: 7}) RETURN count(n)"), 1);
        assert!(r.check().unwrap().is_consistent());
        // The backup stays usable after a restore.
        assert!(dest.join("meta").exists());
    }
}

#[test]
fn backup_leaves_out_an_open_transaction() {
    let t = TempDir::new("tx");
    let mgr = persistent_manager(&t);
    let db = mgr.database("graphbuff").unwrap();
    db.execute("CREATE (:A {x: 1})").unwrap();
    let mut tx = db.begin().unwrap();
    tx.execute("CREATE (:A {x: 2})").unwrap();
    // From another thread, as a server would: this thread holds the transaction.
    let dest = t.join("b");
    let info = std::thread::scope(|s| s.spawn(|| db.backup(&dest).unwrap()).join().unwrap());
    tx.commit().unwrap();
    assert_eq!(info.nodes, 1);
    mgr.restore_database("copy", &dest, false).unwrap();
    assert_eq!(int(&mgr.database("copy").unwrap(), "MATCH (a:A) RETURN sum(a.x)"), 1);
}

#[test]
fn memory_and_hybrid_databases_restore_as_hybrid() {
    let t = TempDir::new("hybrid");
    let mgr = DatabaseManager::open(t.join("data")).unwrap();
    mgr.session()
        .execute("CREATE DATABASE mem OPTIONS {storage: 'memory'}")
        .unwrap();
    let mem = mgr.database("mem").unwrap();
    mem.execute("CREATE (:P {name: 'Ann'})-[:KNOWS {since: date('2020-01-02')}]->(:P {name: 'Bob'})")
        .unwrap();
    let dest = t.join("b");
    let info = mgr.backup_database("mem", &dest).unwrap();
    assert_eq!((info.format, info.nodes, info.relationships), ("hybrid", 2, 1));
    assert!(matches!(mgr.check_database("mem"), Err(Error::Unsupported(_))));

    let restored = mgr.restore_database("copy", &dest, false).unwrap();
    assert_eq!(restored.storage.name(), "hybrid");
    let copy = mgr.database("copy").unwrap();
    let r = copy
        .execute("MATCH (a)-[k:KNOWS]->(b) RETURN a.name, toString(k.since), b.name")
        .unwrap();
    assert_eq!(r.rows[0][1].to_string(), "\"2020-01-02\"");
    // Durable: it survives reopening the manager.
    drop((copy, mem));
    drop(mgr);
    let mgr = DatabaseManager::open(t.join("data")).unwrap();
    assert_eq!(int(&mgr.database("copy").unwrap(), "MATCH (n) RETURN count(n)"), 2);
}

#[test]
fn restore_replaces_only_when_asked() {
    let t = TempDir::new("replace");
    let mgr = persistent_manager(&t);
    let db = mgr.database("graphbuff").unwrap();
    db.execute("CREATE (:Old), (:Old)").unwrap();
    let other = Database::open(StorageMode::persistent(t.join("other"))).unwrap();
    other.execute("CREATE (:New)").unwrap();
    other.backup(&t.join("b")).unwrap();
    drop(other);

    let e = mgr.restore_database("graphbuff", &t.join("b"), false).unwrap_err();
    assert!(e.to_string().contains("not empty"), "{e}");
    // A database still in use (held here) cannot be replaced.
    let e = mgr.restore_database("graphbuff", &t.join("b"), true).unwrap_err();
    assert!(e.to_string().contains("in use"), "{e}");
    assert_eq!(int(&db, "MATCH (n:Old) RETURN count(n)"), 2);
    drop(db);
    mgr.restore_database("graphbuff", &t.join("b"), true).unwrap();
    let db = mgr.database("graphbuff").unwrap();
    assert_eq!(int(&db, "MATCH (n:New) RETURN count(n)"), 1);
    assert_eq!(int(&db, "MATCH (n:Old) RETURN count(n)"), 0);

    // An empty database is replaced without overwrite.
    mgr.session().execute("CREATE DATABASE empty").unwrap();
    mgr.restore_database("empty", &t.join("b"), false).unwrap();
}

#[test]
fn bad_backups_are_refused() {
    let t = TempDir::new("bad");
    let mgr = persistent_manager(&t);
    mgr.database("graphbuff").unwrap().execute("CREATE (:Keep)").unwrap();

    fs::create_dir_all(t.join("nothing")).unwrap();
    let e = mgr.restore_database("x", &t.join("nothing"), false).unwrap_err();
    assert!(e.to_string().contains("not a graphbuff backup"), "{e}");

    mgr.backup_database("graphbuff", &t.join("b")).unwrap();
    fs::write(t.join("b").join(BACKUP_MARKER), b"").unwrap();
    let e = mgr.restore_database("graphbuff", &t.join("b"), true).unwrap_err();
    assert!(e.to_string().contains("did not finish"), "{e}");

    // A damaged backup fails its check and leaves the database alone.
    fs::remove_file(t.join("b").join(BACKUP_MARKER)).unwrap();
    let nodes = t.join("b").join("nodes.db");
    let mut bytes = fs::read(&nodes).unwrap();
    bytes[0] = 0; // the only node's in-use flag
    fs::write(&nodes, bytes).unwrap();
    let e = mgr.restore_database("graphbuff", &t.join("b"), true).unwrap_err();
    assert!(e.to_string().contains("not consistent"), "{e}");
    assert_eq!(int(&mgr.database("graphbuff").unwrap(), "MATCH (n:Keep) RETURN count(n)"), 1);
    assert!(mgr.databases().iter().all(|d| d.name == "graphbuff"));

    // A backup never overwrites anything.
    let e = mgr.backup_database("graphbuff", &t.join("b")).unwrap_err();
    assert!(e.to_string().contains("not empty"), "{e}");
}
