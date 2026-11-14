//! Explicit transactions: `Database::begin`, then statements, then commit or rollback.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use graphbuff::{
    BoxedStore, Database, Error, HybridOptions, PersistentOptions, ResultValue, StorageMode, params,
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn count<S: graphbuff::GraphStore>(db: &Database<S>, q: &str) -> i64 {
    match &db.execute(q).unwrap().rows[0][0] {
        ResultValue::Int(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn tx_count(tx: &mut graphbuff::Transaction<'_, impl graphbuff::GraphStore>, q: &str) -> i64 {
    match &tx.execute(q).unwrap().rows[0][0] {
        ResultValue::Int(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

/// Opens a database (again) in a storage mode.
type Opener = Box<dyn Fn() -> Database<BoxedStore>>;

/// Each storage mode, opened in its own directory.
fn stores(name: &str) -> Vec<(TempDir, Opener)> {
    let mut out: Vec<(TempDir, Opener)> = Vec::new();
    let d = TempDir::new(&format!("{name}-mem"));
    out.push((d, Box::new(|| Database::open(StorageMode::InMemory).unwrap())));
    let d = TempDir::new(&format!("{name}-hybrid"));
    let p = d.0.clone();
    out.push((d, Box::new(move || Database::open(StorageMode::Hybrid(HybridOptions::new(&p))).unwrap())));
    let d = TempDir::new(&format!("{name}-persistent"));
    let p = d.0.clone();
    out.push((
        d,
        Box::new(move || Database::open(StorageMode::Persistent(PersistentOptions::new(&p))).unwrap()),
    ));
    out
}

#[test]
fn statements_see_each_other_and_commit_together() {
    for (_dir, open) in stores("commit") {
        let db = open();
        db.execute("CREATE (:A {n: 1})").unwrap();
        let mut tx = db.begin().unwrap();
        tx.execute("CREATE (:A {n: 2})").unwrap();
        tx.execute_with_params("MATCH (a:A) SET a.n = a.n * $k", &params! { "k" => 10 })
            .unwrap();
        // Reads in the transaction see its changes; reads outside see the last commit.
        assert_eq!(tx_count(&mut tx, "MATCH (a:A) RETURN sum(a.n)"), 30);
        assert_eq!(count(&db, "MATCH (a:A) RETURN sum(a.n)"), 1);
        assert_eq!(db.node_count(), 1);
        let plan = tx.execute("EXPLAIN MATCH (a:A) RETURN a").unwrap();
        assert!(plan.plan.is_some() && plan.rows.is_empty());
        let profiled = tx.execute("PROFILE MATCH (a:A) RETURN count(a)").unwrap();
        assert!(profiled.plan.is_some());
        tx.commit().unwrap();
        assert_eq!(count(&db, "MATCH (a:A) RETURN sum(a.n)"), 30);

        // Durable: a reopened store has the committed transaction.
        drop(db);
        let db = open();
        if db.storage_description().starts_with("in-memory") {
            continue;
        }
        assert_eq!(count(&db, "MATCH (a:A) RETURN sum(a.n)"), 30);
    }
}

#[test]
fn rollback_and_drop_discard_everything() {
    for (_dir, open) in stores("rollback") {
        let db = open();
        db.execute("CREATE INDEX FOR (a:A) ON (a.n)").unwrap();
        db.execute("CREATE (:A {n: 1})").unwrap();
        let mut tx = db.begin().unwrap();
        tx.execute("CREATE (:A {n: 2})").unwrap();
        tx.execute("MATCH (a:A {n: 1}) DETACH DELETE a").unwrap();
        tx.rollback();
        {
            let mut tx = db.begin().unwrap();
            tx.execute("CREATE (:A {n: 3})-[:R]->(:B)").unwrap();
            assert_eq!(tx_count(&mut tx, "MATCH (a:A {n: 3}) RETURN count(a)"), 1);
            // Dropped without a commit.
        }
        assert_eq!(count(&db, "MATCH (n) RETURN count(n)"), 1);
        assert_eq!(count(&db, "MATCH (a:A {n: 1}) RETURN count(a)"), 1);
        drop(db);
        let db = open();
        if db.storage_description().starts_with("in-memory") {
            continue;
        }
        assert_eq!(count(&db, "MATCH (n) RETURN count(n)"), 1);
    }
}

#[test]
fn a_failed_statement_rolls_the_transaction_back() {
    let db = Database::in_memory();
    db.execute("CREATE CONSTRAINT FOR (u:U) REQUIRE u.id IS UNIQUE").unwrap();
    db.execute("CREATE (:U {id: 1})").unwrap();

    let mut tx = db.begin().unwrap();
    tx.execute("CREATE (:U {id: 2})").unwrap();
    // A syntax error changes nothing: the transaction stays open.
    assert!(matches!(tx.execute("MATCH (n RETURN n"), Err(Error::Syntax { .. })));
    assert!(tx.is_open());
    // A failing statement ends it.
    let err = tx.execute("CREATE (:U {id: 1})").unwrap_err();
    assert!(matches!(err, Error::Constraint(_)), "{err}");
    assert!(!tx.is_open());
    let later = tx.execute("CREATE (:U {id: 3})").unwrap_err().to_string();
    assert!(later.contains("rolled back"), "{later}");
    assert!(tx.commit().is_err());
    assert_eq!(count(&db, "MATCH (u:U) RETURN count(u)"), 1);

    // The database is usable again.
    let mut tx = db.begin().unwrap();
    tx.execute("UNWIND [2, 3] AS i CREATE (:U {id: i})").unwrap();
    assert!(tx.execute("RETURN 1 / 0").is_err());
    assert_eq!(count(&db, "MATCH (u:U) RETURN count(u)"), 1);
    db.execute("CREATE (:U {id: 2})").unwrap();
    assert_eq!(count(&db, "MATCH (u:U) RETURN count(u)"), 2);
}

#[test]
fn schema_commands_in_a_transaction() {
    let db = Database::in_memory();
    let mut tx = db.begin().unwrap();
    tx.execute("UNWIND range(1, 100) AS i CREATE (:P {id: i})").unwrap();
    tx.execute("CREATE INDEX FOR (p:P) ON (p.id)").unwrap();
    assert_eq!(tx_count(&mut tx, "MATCH (p:P {id: 42}) RETURN count(p)"), 1);
    tx.commit().unwrap();
    assert!(db.explain("MATCH (p:P {id: 42}) RETURN p").unwrap().contains("NodeIndexSeek"));

    let name = db.read(|s| graphbuff::GraphStore::indexes(s)[0].name.clone());
    let mut tx = db.begin().unwrap();
    tx.execute(&format!("DROP INDEX {name}")).unwrap();
    assert!(!tx.execute("EXPLAIN MATCH (p:P {id: 42}) RETURN p").unwrap().plan.unwrap().to_string().contains("NodeIndexSeek"));
    drop(tx);
    assert_eq!(db.read(|s| graphbuff::GraphStore::indexes(s).len()), 1);
}

#[test]
fn the_owning_thread_cannot_wait_for_its_own_transaction() {
    let db = Database::in_memory();
    let mut tx = db.begin().unwrap();
    tx.execute("CREATE (:A)").unwrap();
    // Reads go to the last commit; writes and a second transaction would deadlock.
    assert_eq!(count(&db, "MATCH (a:A) RETURN count(a)"), 0);
    let msg = db.execute("CREATE (:B)").unwrap_err().to_string();
    assert!(msg.contains("open transaction"), "{msg}");
    assert!(db.begin().is_err());
    assert!(db.checkpoint().is_err());
    tx.commit().unwrap();
    db.execute("CREATE (:B)").unwrap();
    assert_eq!(db.node_count(), 2);
}

#[test]
fn other_writers_wait_for_the_transaction() {
    let db = Arc::new(Database::in_memory());
    db.execute("CREATE (:Counter {n: 0})").unwrap();
    let mut tx = db.begin().unwrap();
    tx.execute("MATCH (c:Counter) SET c.n = c.n + 10").unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let (started, wait) = mpsc::channel();
    let writer = {
        let (db, done) = (db.clone(), done.clone());
        thread::spawn(move || {
            started.send(()).unwrap();
            db.execute("MATCH (c:Counter) SET c.n = c.n * 2").unwrap();
            done.store(true, Ordering::Release);
        })
    };
    wait.recv().unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(!done.load(Ordering::Acquire), "a writer ran inside an open transaction");
    // Readers do not wait.
    assert_eq!(count(&*db, "MATCH (c:Counter) RETURN c.n"), 0);
    tx.commit().unwrap();
    writer.join().unwrap();
    assert_eq!(count(&*db, "MATCH (c:Counter) RETURN c.n"), 20);
}
