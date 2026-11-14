//! Write transaction benchmark: the cost of writing queries, and read latency while a
//! large write runs.
//!
//! ```text
//! cargo run --release -p graphbuff --example txbench [rows]
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use graphbuff::{BoxedStore, Database, PersistentOptions, StorageMode, params};

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn time(db: &Database<BoxedStore>, q: &str) -> f64 {
    let t = Instant::now();
    db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    ms(t.elapsed())
}

fn bench(name: &str, open: &dyn Fn() -> Database<BoxedStore>, rows: i64) {
    println!("## {name}, {rows} rows");
    let db = open();
    db.execute("CREATE INDEX FOR (p:P) ON (p.id)").unwrap();
    println!("create          {:9.1} ms", time(&db, &format!("UNWIND range(1, {rows}) AS i CREATE (:P {{id: i}})")));
    println!("set all         {:9.1} ms", time(&db, "MATCH (p:P) SET p.v = p.id * 2"));
    println!(
        "merge (indexed) {:9.1} ms",
        time(&db, &format!("UNWIND range(1, {}) AS i MERGE (:P {{id: i * 2}})", rows / 10))
    );
    println!(
        "link chain      {:9.1} ms",
        time(&db, &format!("MATCH (a:P), (b:P) WHERE b.id = a.id + 1 AND a.id <= {} CREATE (a)-[:NEXT]->(b)", rows / 10))
    );
    let t = Instant::now();
    for i in 0..2000 {
        db.execute_with_params("MATCH (p:P {id: $i}) SET p.w = $i", &params! { "i" => i })
            .unwrap();
    }
    println!("2000 small sets {:9.1} ms", ms(t.elapsed()));
    println!("detach delete   {:9.1} ms", time(&db, "MATCH (p:P) DETACH DELETE p"));

    // Read latency while a large write runs.
    let db = Arc::new(db);
    db.execute("UNWIND range(1, 1000) AS i CREATE (:Small {i: i})").unwrap();
    let writing = Arc::new(AtomicBool::new(true));
    let writer = {
        let (db, writing) = (db.clone(), writing.clone());
        thread::spawn(move || {
            let t = Instant::now();
            db.execute(&format!("UNWIND range(1, {rows}) AS i CREATE (:Big {{i: i}})")).unwrap();
            writing.store(false, Ordering::Release);
            ms(t.elapsed())
        })
    };
    let mut lat = Vec::new();
    while writing.load(Ordering::Acquire) {
        let t = Instant::now();
        db.execute("MATCH (s:Small) RETURN count(s)").unwrap();
        lat.push(ms(t.elapsed()));
    }
    let write = writer.join().unwrap();
    lat.sort_by(f64::total_cmp);
    let p = |q: f64| lat.get(((lat.len() - 1) as f64 * q) as usize).copied().unwrap_or(0.0);
    println!(
        "reads during a {write:.0} ms write: {} reads, median {:.3} ms, max {:.1} ms",
        lat.len(),
        p(0.5),
        p(1.0)
    );
}

fn main() {
    let rows: i64 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(200_000);
    bench("memory", &|| Database::open(StorageMode::InMemory).unwrap(), rows);
    let dir = std::env::temp_dir().join(format!("graphbuff-txbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    bench(
        "persistent",
        &|| Database::open(StorageMode::Persistent(PersistentOptions::new(&dir))).unwrap(),
        rows / 10,
    );
    let _ = std::fs::remove_dir_all(&dir);
}
