//! Vector index benchmark: `db.index.vector.queryNodes` against the same question asked
//! with `ORDER BY vector.similarity.cosine(…) DESC LIMIT k`, with the scores on the CPU
//! (every core) and, with the `gpu` feature, on the GPU. Results are checked equal.
//!
//! ```text
//! cargo run --release -p graphbuff --example vecbench
//! cargo run --release -p graphbuff --features gpu --example vecbench -- --docs 200000 --dims 768
//! ```
//!
//! The graph (in memory): `--docs` `(:Doc {emb})` nodes with random `--dims`-dimensional
//! vectors, and a cosine vector index on them.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use graphbuff::{CpuKernels, Database, GraphStore, Kernels, MemoryGraph, Result, Value};

const K: usize = 10;

fn main() -> Result<()> {
    let (docs, dims, reps) = parse_args();
    #[allow(unused_mut)]
    let mut backends: Vec<(&str, Arc<dyn Kernels>)> = vec![("cpu", Arc::new(CpuKernels::new()))];
    #[cfg(feature = "gpu")]
    match graphbuff::WgpuKernels::new() {
        Ok(g) => {
            println!("GPU: {}", g.adapter());
            backends.push(("gpu", Arc::new(g.with_min_work(0))));
        }
        Err(e) => println!("no GPU: {e}"),
    }

    let mut store = MemoryGraph::new();
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let t = Instant::now();
    store.begin()?;
    for _ in 0..docs {
        let emb: Vec<Value> = (0..dims).map(|_| Value::Float(unit(&mut rng) * 2.0 - 1.0)).collect();
        store.create_node(&["Doc".into()], BTreeMap::from([("emb".to_string(), Value::List(emb))]))?;
    }
    store.commit()?;
    let db = Database::with_store(store);
    db.execute(&format!(
        "CREATE VECTOR INDEX doc_emb FOR (d:Doc) ON d.emb OPTIONS {{indexConfig: {{`vector.dimensions`: {dims}}}}}"
    ))?;
    println!(
        "{docs} docs x {dims} dims ({:.0} MiB of f32 vectors), load {:.1?}, k = {K}, median of {reps}\n",
        (docs * dims * 4) as f64 / (1 << 20) as f64,
        t.elapsed()
    );

    let query = |rng: &mut u64| -> BTreeMap<String, Value> {
        let q: Vec<Value> = (0..dims).map(|_| Value::Float(unit(rng) * 2.0 - 1.0)).collect();
        BTreeMap::from([("q".to_string(), Value::List(q))])
    };
    let via_index = format!(
        "CALL db.index.vector.queryNodes('doc_emb', {K}, $q) YIELD node, score RETURN id(node) AS id, score"
    );
    let via_scan = format!(
        "MATCH (d:Doc) WITH d, vector.similarity.cosine(d.emb, $q) AS score \
         RETURN id(d) AS id, score ORDER BY score DESC, id LIMIT {K}"
    );
    let ids = |q: &str, p: &BTreeMap<String, Value>| -> Result<Vec<String>> {
        Ok(db.execute_with_params(q, p)?.rows.iter().map(|r| format!("{} {}", r[0], r[1])).collect())
    };

    let params = query(&mut rng);
    let t = Instant::now();
    let scan = ids(&via_scan, &params)?;
    println!("  {:<44} {:>10.1} ms", "ORDER BY vector.similarity.cosine LIMIT k", ms(t.elapsed()));
    for (name, kernels) in &backends {
        db.set_kernels(kernels.clone());
        // A commit drops the cached vectors: the next query builds them.
        db.execute("CREATE (:Other)")?;
        let t = Instant::now();
        assert_eq!(ids(&via_index, &params)?, scan, "{name}: index and scan disagree");
        let first = t.elapsed();
        let mut qrng = rng;
        let each = median(reps, || {
            let p = query(&mut qrng);
            let t = Instant::now();
            ids(&via_index, &p)?;
            Ok(t.elapsed())
        })?;
        println!("  {:<44} {:>10.1} ms", format!("queryNodes, {name}: first after a commit"), ms(first));
        println!("  {:<44} {:>10.1} ms", format!("queryNodes, {name}: next ones"), ms(each));
    }
    Ok(())
}

fn unit(rng: &mut u64) -> f64 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    (*rng >> 11) as f64 / (1u64 << 53) as f64
}

fn median(reps: usize, mut f: impl FnMut() -> Result<Duration>) -> Result<Duration> {
    let mut times = (0..reps).map(|_| f()).collect::<Result<Vec<_>>>()?;
    times.sort();
    Ok(times[times.len() / 2])
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn parse_args() -> (usize, usize, usize) {
    let (mut docs, mut dims, mut reps) = (100_000, 384, 5);
    let usage = || -> ! {
        eprintln!("usage: vecbench [--docs N] [--dims N] [--reps N]");
        std::process::exit(2)
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let v: usize = args.next().and_then(|v| v.replace('_', "").parse().ok()).unwrap_or_else(|| usage());
        match a.as_str() {
            "--docs" => docs = v.max(1),
            "--dims" => dims = v.clamp(1, 4096),
            "--reps" => reps = v.max(1),
            _ => usage(),
        }
    }
    (docs, dims, reps)
}
