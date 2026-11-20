//! Per-query memory limits (`QueryOptions::max_memory`), counted by the counting allocator.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::accounting::{self, CountingAllocator};
use graphbuff::{BoxedStore, Database, Error, QueryOptions, ResultValue, RowSink, StorageMode};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const MIB: u64 = 1 << 20;

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-mem-{}-{}-{name}",
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

fn limited(db: &Database<BoxedStore>, q: &str, mib: u64) -> graphbuff::Result<graphbuff::QueryResult> {
    db.execute_with_options(q, &BTreeMap::new(), &QueryOptions::default().with_max_memory(mib * MIB))
}

fn int(db: &Database<BoxedStore>, q: &str) -> i64 {
    match &db.execute(q).unwrap().rows[0][0] {
        ResultValue::Int(i) => *i,
        other => panic!("{q}: {other:?}"),
    }
}

#[test]
fn counting_is_on() {
    assert!(accounting::is_counting());
    let before = accounting::thread_bytes();
    let v = vec![0u8; 10 << 20];
    assert!(accounting::thread_bytes() - before >= 10 << 20);
    drop(v);
    assert!(accounting::thread_bytes() - before < 1 << 20);
    // Exempt allocations are not counted.
    let kept = accounting::exempt(|| vec![0u8; 10 << 20]);
    assert!(accounting::thread_bytes() - before < 1 << 20);
    drop(kept);
}

#[test]
fn a_query_over_its_limit_stops() {
    let db = Database::open(StorageMode::InMemory).unwrap();
    let big = "UNWIND range(1, 3000000) AS i RETURN collect(i) AS all";
    let e = limited(&db, big, 32).unwrap_err();
    assert!(matches!(e, Error::MemoryLimit(_)), "{e}");
    assert!(e.to_string().contains("32 MiB"), "{e}");
    // Within the limit, and without one, it runs.
    assert_eq!(limited(&db, "UNWIND range(1, 1000) AS i RETURN sum(i)", 32).unwrap().rows.len(), 1);
    assert_eq!(db.execute(big).unwrap().rows.len(), 1);
}

#[test]
fn a_large_result_counts() {
    let db = Database::open(StorageMode::InMemory).unwrap();
    let e = limited(&db, "UNWIND range(1, 2000000) AS i RETURN i, toString(i) AS s", 32).unwrap_err();
    assert!(matches!(e, Error::MemoryLimit(_)), "{e}");
}

#[test]
fn a_writing_query_over_its_limit_is_rolled_back() {
    let db = Database::open(StorageMode::InMemory).unwrap();
    let e = limited(&db, "UNWIND range(1, 1000000) AS i CREATE (:N {i: i, s: 'some text'})", 16).unwrap_err();
    assert!(matches!(e, Error::MemoryLimit(_)), "{e}");
    assert_eq!(int(&db, "MATCH (n:N) RETURN count(n)"), 0);
    // In a transaction too, and the transaction ends.
    let mut tx = db.begin().unwrap();
    let options = QueryOptions::default().with_max_memory(16 * MIB);
    let e = tx
        .execute_with_options("UNWIND range(1, 3000000) AS i RETURN collect(i)", &BTreeMap::new(), &options)
        .unwrap_err();
    assert!(matches!(e, Error::MemoryLimit(_)), "{e}");
    drop(tx);
    // Batches committed before the limit is reached stay committed.
    let q = "UNWIND range(1, 200000) AS i CALL { WITH i CREATE (:B {i: i}) RETURN i AS j } IN TRANSACTIONS OF 1000 ROWS
             RETURN count(j)";
    let e = limited(&db, q, 16).unwrap_err();
    assert!(matches!(e, Error::MemoryLimit(_)), "{e}");
    let made = int(&db, "MATCH (b:B) RETURN count(b)");
    assert!(made > 0 && made < 200000 && made % 1000 == 0, "{made}");
}

#[test]
fn shared_caches_are_not_the_querys() {
    let dir = TempDir::new("persistent");
    let db = Database::open(StorageMode::persistent(&dir.0)).unwrap();
    db.execute("UNWIND range(1, 100000) AS i CREATE (:P {i: i, s: 'a fairly long string property ' + toString(i)})-[:R]->(:Q)")
        .unwrap();
    drop(db);
    let db = Database::open(StorageMode::persistent(&dir.0)).unwrap();
    // Reading every page fills the page cache (more than the limit); a filter streams.
    let r = limited(&db, "MATCH (p:P) WHERE p.s ENDS WITH ' 99999' RETURN p.i", 4).unwrap();
    assert_eq!(r.rows, [[ResultValue::Int(99999)]]);
    // The kernel cache (a CSR export of the graph) is built by the query that needs it.
    let r = limited(&db, "MATCH (:P)-[:R]->(q) RETURN count(q) AS n", 4).unwrap();
    assert_eq!(r.rows[0][0], ResultValue::Int(100000));
    // Aggregation keeps its groups, not its input rows.
    let r = limited(&db, "MATCH (p:P) RETURN count(p.s) AS n", 4).unwrap();
    assert_eq!(r.rows[0][0], ResultValue::Int(100000));
    // Sorting holds its input rows: that is the query's memory.
    let e = limited(&db, "MATCH (p:P) WITH p.s AS s ORDER BY s RETURN count(s)", 4).unwrap_err();
    assert!(matches!(e, Error::MemoryLimit(_)), "{e}");
}

/// Counts the rows it is handed, keeping none.
#[derive(Default)]
struct Counter {
    columns: Vec<String>,
    rows: usize,
    stop_after: Option<usize>,
}

impl RowSink for Counter {
    fn columns(&mut self, columns: &[String]) -> graphbuff::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }

    fn row(&mut self, _: Vec<ResultValue>) -> graphbuff::Result<()> {
        self.rows += 1;
        if self.stop_after == Some(self.rows) {
            return Err(Error::Cancelled("enough".into()));
        }
        Ok(())
    }
}

#[test]
fn a_streamed_result_is_not_held() {
    let dir = TempDir::new("streamed");
    let db = Database::open(StorageMode::persistent(&dir.0)).unwrap();
    let q = "UNWIND range(1, 1000) AS i UNWIND range(1, 2000) AS j RETURN i, toString(j) AS s";
    let options = QueryOptions::default().with_max_memory(32 * MIB);
    // Collected, the rows are over the limit; streamed, they pass through.
    assert!(matches!(limited(&db, q, 32), Err(Error::MemoryLimit(_))));
    let mut sink = Counter::default();
    let r = db.execute_streaming(q, &BTreeMap::new(), &options, &mut sink).unwrap();
    assert_eq!((sink.columns, sink.rows, r.rows.len()), (vec!["i".to_string(), "s".to_string()], 2000000, 0));
    // UNION drops repeats across its parts as they stream.
    let mut sink = Counter::default();
    db.execute_streaming("UNWIND [1, 2, 2] AS x RETURN x UNION UNWIND [2, 3] AS x RETURN x", &BTreeMap::new(), &options, &mut sink)
        .unwrap();
    assert_eq!(sink.rows, 3);
    // An error from the sink stops the query.
    let mut sink = Counter { stop_after: Some(10), ..Counter::default() };
    let e = db.execute_streaming(q, &BTreeMap::new(), &options, &mut sink).unwrap_err();
    assert!(matches!(e, Error::Cancelled(_)), "{e}");
    assert_eq!(sink.rows, 10);
    // A writing query commits, then hands its rows over.
    let mut sink = Counter::default();
    let r = db
        .execute_streaming("UNWIND range(1, 5) AS i CREATE (n:W {i: i}) RETURN n", &BTreeMap::new(), &options, &mut sink)
        .unwrap();
    assert_eq!((sink.rows, r.stats.nodes_created), (5, 5));
    assert_eq!(int(&db, "MATCH (n:W) RETURN count(n)"), 5);
}
