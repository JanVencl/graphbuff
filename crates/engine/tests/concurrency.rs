//! Readers and one writer at the same time: readers see only committed states, and are
//! not held up while a writing query runs.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

use graphbuff::{BoxedStore, Database, HybridOptions, PersistentOptions, ResultValue, StorageMode, params};

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

fn int(v: &ResultValue) -> i64 {
    match v {
        ResultValue::Int(i) => *i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

const ACCOUNTS: i64 = 40;
const BALANCE: i64 = 100;

/// Many readers check invariants that every committed state keeps and every partly
/// applied write breaks, while one writer moves money between accounts and creates and
/// deletes nodes in pairs.
fn no_torn_reads(db: Database<BoxedStore>) {
    db.execute(&format!(
        "UNWIND range(1, {ACCOUNTS}) AS i CREATE (:Account {{id: i, balance: {BALANCE}}})"
    ))
    .unwrap();
    db.execute("CREATE INDEX FOR (a:Account) ON (a.id)").unwrap();
    let db = Arc::new(db);
    let done = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (db, done, reads) = (db.clone(), done.clone(), reads.clone());
            thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    let r = db
                        .execute(
                            "MATCH (a:Account) WITH sum(a.balance) AS total, count(a) AS accounts \
                             OPTIONAL MATCH (p:Pair) \
                             RETURN total, accounts, count(p) AS pairs, \
                                    count(DISTINCT p.g) AS groups",
                        )
                        .unwrap();
                    let row = &r.rows[0];
                    assert_eq!(int(&row[0]), ACCOUNTS * BALANCE, "money appeared or vanished");
                    assert_eq!(int(&row[1]), ACCOUNTS);
                    assert_eq!(int(&row[2]), 2 * int(&row[3]), "a pair was half written");
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    // Let the readers get going first.
    while reads.load(Ordering::Relaxed) < 8 {
        thread::yield_now();
    }
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut rand = move |n: i64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as i64
    };
    for i in 0..300 {
        match i % 3 {
            0 | 1 => {
                let (a, b) = (1 + rand(ACCOUNTS), 1 + rand(ACCOUNTS));
                db.execute_with_params(
                    "MATCH (a:Account {id: $a}), (b:Account {id: $b}) WHERE a <> b \
                     SET a.balance = a.balance - $x, b.balance = b.balance + $x",
                    &params! { "a" => a, "b" => b, "x" => 1 + rand(20) },
                )
                .unwrap();
            }
            _ => {
                db.execute_with_params("CREATE (:Pair {g: $g}), (:Pair {g: $g})", &params! { "g" => i })
                    .unwrap();
                if i % 2 == 0 {
                    db.execute_with_params(
                        "MATCH (p:Pair) WHERE p.g = $g DETACH DELETE p",
                        &params! { "g" => i - 3 },
                    )
                    .unwrap();
                }
                // A failing write must leave nothing behind either.
                assert!(
                    db.execute("MATCH (a:Account {id: 1}) SET a.balance = a.balance - 1 CREATE (:Pair {g: -1}) WITH a RETURN 1 / 0")
                        .is_err()
                );
            }
        }
    }
    done.store(true, Ordering::Release);
    for r in readers {
        r.join().unwrap();
    }
    assert!(reads.load(Ordering::Relaxed) > 0);
}

#[test]
fn memory_readers_never_see_a_torn_state() {
    no_torn_reads(Database::open(StorageMode::InMemory).unwrap());
}

#[test]
fn hybrid_readers_never_see_a_torn_state() {
    let dir = TempDir::new("torn-hybrid");
    no_torn_reads(Database::open(StorageMode::Hybrid(HybridOptions::new(&dir.0))).unwrap());
}

#[test]
fn persistent_readers_never_see_a_torn_state() {
    let dir = TempDir::new("torn-persistent");
    no_torn_reads(Database::open(StorageMode::Persistent(PersistentOptions::new(&dir.0))).unwrap());
}

/// Readers keep answering while a large write runs, and see none of it until it commits.
fn reads_during_a_large_write(db: Database<BoxedStore>, rows: i64) {
    db.execute("UNWIND range(1, 100) AS i CREATE (:Small {i: i})").unwrap();
    let db = Arc::new(db);
    let start = Arc::new(Barrier::new(2));

    let writer = {
        let (db, start) = (db.clone(), start.clone());
        thread::spawn(move || {
            start.wait();
            let t = Instant::now();
            db.execute_with_params(
                "UNWIND range(1, $n) AS i CREATE (:Big {i: i})",
                &params! { "n" => rows },
            )
            .unwrap();
            (t, t.elapsed())
        })
    };

    start.wait();
    // When each read that saw none of the write finished.
    let mut before_commit = Vec::new();
    loop {
        let r = db
            .execute("MATCH (s:Small) WITH count(s) AS small OPTIONAL MATCH (b:Big) RETURN small, count(b) AS big")
            .unwrap();
        let (small, big) = (int(&r.rows[0][0]), int(&r.rows[0][1]));
        assert_eq!(small, 100);
        assert!(big == 0 || big == rows, "saw {big} of {rows} new nodes");
        if big == rows {
            break;
        }
        before_commit.push(Instant::now());
    }
    let (started, took) = writer.join().unwrap();
    // A reader that waited for the writer could finish only before the write began.
    let late = before_commit.iter().filter(|t| **t > started + took / 2).count();
    assert!(
        late >= 3,
        "{late} of {} reads finished in the second half of a {took:?} write",
        before_commit.len()
    );
}

#[test]
fn memory_reads_continue_during_a_large_write() {
    reads_during_a_large_write(Database::open(StorageMode::InMemory).unwrap(), 200_000);
}

#[test]
fn persistent_reads_continue_during_a_large_write() {
    let dir = TempDir::new("large-persistent");
    reads_during_a_large_write(
        Database::open(StorageMode::Persistent(PersistentOptions::new(&dir.0))).unwrap(),
        20_000,
    );
}

#[test]
fn hybrid_reads_continue_during_a_large_write() {
    let dir = TempDir::new("large-hybrid");
    reads_during_a_large_write(
        Database::open(StorageMode::Hybrid(HybridOptions::new(&dir.0))).unwrap(),
        100_000,
    );
}
