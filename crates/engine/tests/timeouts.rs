//! Query timeouts and cancellation: `QueryOptions`, `CancelHandle`.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use graphbuff::{
    BoxedStore, CancelHandle, Database, DatabaseManager, Error, HybridOptions, PersistentOptions, QueryOptions,
    ResultValue, StorageMode,
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

type Opener = Box<dyn Fn() -> Database<BoxedStore>>;

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

fn count<S: graphbuff::GraphStore>(db: &Database<S>, q: &str) -> i64 {
    match &db.execute(q).unwrap().rows[0][0] {
        ResultValue::Int(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

/// A billion rows over 1000 nodes: never finishes in a test.
const SLOW: &str = "MATCH (a:N), (b:N), (c:N) RETURN count(*)";

fn setup<S: graphbuff::GraphStore>(db: &Database<S>) {
    db.execute("UNWIND range(1, 1000) AS i CREATE (:N {i: i})").unwrap();
}

fn timeout(ms: u64) -> QueryOptions {
    QueryOptions::default().with_timeout(Duration::from_millis(ms))
}

/// How long a stopped query may take past its limit before the test fails; generous,
/// for slow CI machines.
const SLACK: Duration = Duration::from_secs(5);

#[test]
fn read_query_times_out() {
    for (_dir, open) in stores("read") {
        let db = open();
        setup(&db);
        let started = Instant::now();
        let r = db.execute_with_options(SLOW, &Default::default(), &timeout(100));
        let took = started.elapsed();
        match r {
            Err(Error::Timeout(m)) => assert!(m.contains("100 ms"), "{m}"),
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert!(took >= Duration::from_millis(100) && took < SLACK, "{took:?}");
        // Without a limit, ordinary queries are unaffected.
        assert_eq!(count(&db, "MATCH (n:N) RETURN count(n)"), 1000);
        let r = db.execute_with_options("MATCH (n:N) RETURN count(n)", &Default::default(), &timeout(60_000));
        assert!(r.is_ok());
    }
}

#[test]
fn write_query_times_out_and_rolls_back() {
    for (_dir, open) in stores("write") {
        let db = open();
        setup(&db);
        // Stops while matching.
        let r = db.execute_with_options("MATCH (a:N), (b:N), (c:N) CREATE (:X)", &Default::default(), &timeout(50));
        assert!(matches!(r, Err(Error::Timeout(_))), "{r:?}");
        // Stops while creating: the match is quick, the million creates are not.
        let r = db.execute_with_options("MATCH (a:N), (b:N) CREATE (:X)", &Default::default(), &timeout(20));
        assert!(matches!(r, Err(Error::Timeout(_))), "{r:?}");
        let r = db.execute_with_options("MATCH (a:N), (b:N) SET a.v = b.i", &Default::default(), &timeout(20));
        assert!(matches!(r, Err(Error::Timeout(_))), "{r:?}");
        assert_eq!(count(&db, "MATCH (x:X) RETURN count(x)"), 0);
        assert_eq!(count(&db, "MATCH (n:N) WHERE n.v IS NOT NULL RETURN count(n)"), 0);
        // The database takes writers again.
        db.execute("CREATE (:X)").unwrap();
        assert_eq!(count(&db, "MATCH (x:X) RETURN count(x)"), 1);
    }
}

#[test]
fn variable_length_and_shortest_paths_time_out() {
    let db = Database::in_memory();
    // A complete directed graph on 12 nodes: its trails are countless.
    db.execute("UNWIND range(1, 12) AS i CREATE (:K {i: i})").unwrap();
    db.execute("MATCH (a:K), (b:K) WHERE a <> b CREATE (a)-[:E]->(b)").unwrap();
    for q in [
        "MATCH p = (a:K)-[:E*]->(b:K) RETURN count(p)",
        "MATCH (a:K {i: 1}), (b:K {i: 2}) MATCH p = allShortestPaths((a)-[:E*]-(b)) \
         WHERE all(r IN relationships(p) WHERE r.missing IS NULL) AND length(p) > 30 RETURN count(p)",
    ] {
        let started = Instant::now();
        let r = db.execute_with_options(q, &Default::default(), &timeout(100));
        assert!(matches!(r, Err(Error::Timeout(_))), "{q}: {r:?}");
        assert!(started.elapsed() < SLACK, "{q}: {:?}", started.elapsed());
    }
}

#[test]
fn cancel_from_another_thread() {
    for (_dir, open) in stores("cancel") {
        let db = open();
        setup(&db);
        let cancel = CancelHandle::new();
        let options = QueryOptions::default().with_cancel(cancel.clone());
        let (r, took) = thread::scope(|s| {
            let worker = s.spawn(|| {
                let started = Instant::now();
                let r = db.execute_with_options("MATCH (a:N), (b:N), (c:N) CREATE (:X)", &Default::default(), &options);
                (r, started.elapsed())
            });
            thread::sleep(Duration::from_millis(100));
            assert!(!worker.is_finished());
            cancel.cancel();
            worker.join().unwrap()
        });
        assert!(matches!(r, Err(Error::Cancelled(_))), "{r:?}");
        assert!(took < SLACK, "{took:?}");
        assert_eq!(count(&db, "MATCH (x:X) RETURN count(x)"), 0);

        // A cancelled handle stops later queries at once, before they change anything.
        let r = db.execute_with_options("CREATE (:X)", &Default::default(), &options);
        assert!(matches!(r, Err(Error::Cancelled(_))), "{r:?}");
        let r = db.execute_with_options("RETURN 1", &Default::default(), &options);
        assert!(matches!(r, Err(Error::Cancelled(_))), "{r:?}");
        assert_eq!(count(&db, "MATCH (x:X) RETURN count(x)"), 0);
    }
}

#[test]
fn statement_timeout_rolls_the_transaction_back() {
    for (_dir, open) in stores("tx") {
        let db = open();
        setup(&db);
        let mut tx = db.begin().unwrap();
        tx.execute("CREATE (:Y)").unwrap();
        let r = tx.execute_with_options(SLOW, &Default::default(), &timeout(50));
        assert!(matches!(r, Err(Error::Timeout(_))), "{r:?}");
        assert!(!tx.is_open());
        assert!(tx.execute("RETURN 1").is_err());
        drop(tx);
        assert_eq!(count(&db, "MATCH (y:Y) RETURN count(y)"), 0);

        // A statement within its limit leaves the transaction open.
        let mut tx = db.begin().unwrap();
        tx.execute_with_options("CREATE (:Y)", &Default::default(), &timeout(60_000)).unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&db, "MATCH (y:Y) RETURN count(y)"), 1);
    }
}

#[test]
fn waiting_for_a_writer_times_out() {
    let db = Database::in_memory();
    thread::scope(|s| {
        let (opened, wait_open) = std::sync::mpsc::channel();
        let (done, wait_done) = std::sync::mpsc::channel::<()>();
        let db = &db;
        let holder = s.spawn(move || {
            let mut tx = db.begin().unwrap();
            tx.execute("CREATE (:Held)").unwrap();
            opened.send(()).unwrap();
            wait_done.recv().unwrap();
            tx.commit().unwrap();
        });
        wait_open.recv().unwrap();

        let started = Instant::now();
        let r = db.execute_with_options("CREATE (:W)", &Default::default(), &timeout(100));
        assert!(matches!(r, Err(Error::Timeout(_))), "{r:?}");
        assert!(started.elapsed() >= Duration::from_millis(100) && started.elapsed() < SLACK);
        let r = db.begin_with_options(&timeout(50));
        assert!(matches!(r, Err(Error::Timeout(_))), "{r:?}");

        // Cancelling stops the wait too.
        let cancel = CancelHandle::new();
        let options = QueryOptions::default().with_cancel(cancel.clone());
        let waiter = s.spawn(move || db.execute_with_options("CREATE (:W)", &Default::default(), &options));
        thread::sleep(Duration::from_millis(50));
        cancel.cancel();
        let r = waiter.join().unwrap();
        assert!(matches!(r, Err(Error::Cancelled(_))), "{r:?}");

        // Reads do not wait.
        let r = db.execute_with_options("MATCH (n) RETURN count(n)", &Default::default(), &timeout(1000));
        assert!(r.is_ok());
        done.send(()).unwrap();
        holder.join().unwrap();
    });
    assert_eq!(db.node_count(), 1);
    // With the writer gone, a limited writer gets the lock.
    db.execute_with_options("CREATE (:W)", &Default::default(), &timeout(1000)).unwrap();
    assert_eq!(db.node_count(), 2);
}

#[test]
fn sessions_pass_the_options_on() {
    let mgr = DatabaseManager::in_memory();
    let mut session = mgr.session();
    session.execute("UNWIND range(1, 1000) AS i CREATE (:N)").unwrap();
    let r = session.execute_with_options(SLOW, &Default::default(), &timeout(50));
    assert!(matches!(r, Err(Error::Timeout(_))), "{r:?}");
    // Database commands are not limited.
    let cancel = CancelHandle::new();
    cancel.cancel();
    let options = QueryOptions::default().with_cancel(cancel);
    session.execute_with_options("CREATE DATABASE other", &Default::default(), &options).unwrap();
    let r = session.execute_with_options("USE other RETURN 1", &Default::default(), &options);
    assert!(matches!(r, Err(Error::Cancelled(_))), "{r:?}");
}
