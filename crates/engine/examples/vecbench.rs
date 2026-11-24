//! Vector index benchmark: `db.index.vector.queryNodes` against the same question asked
//! with `ORDER BY vector.similarity.cosine(…) DESC LIMIT k`, with the scores on the CPU
//! (every core) and, with the `gpu` feature, on the GPU. Results are checked equal. Then
//! filtered searches (`SEARCH … WHERE d.p < share`), by the share of the vectors that pass,
//! against `MATCH … WHERE … ORDER BY … LIMIT k`; with `--hnsw 1` also through an approximate
//! index, with its recall.
//!
//! ```text
//! cargo run --release -p graphbuff --example vecbench
//! cargo run --release -p graphbuff --features gpu --example vecbench -- --docs 200000 --dims 768
//! ```
//!
//! The graph (in memory): `--docs` `(:Doc {emb})` nodes with random `--dims`-dimensional
//! vectors (`--clusters N`: around N random centers), and a cosine vector index on them.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use graphbuff::{CpuKernels, Database, GraphStore, Kernels, MemoryGraph, Result, Value};

const K: usize = 10;

fn main() -> Result<()> {
    let Args { docs, dims, reps, clusters, hnsw } = parse_args();
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
    let centers: Vec<Vec<f64>> =
        (0..clusters).map(|_| (0..dims).map(|_| unit(&mut rng) * 2.0 - 1.0).collect()).collect();
    let vector = |rng: &mut u64| -> Vec<Value> {
        let center = (!centers.is_empty()).then(|| &centers[(unit(rng) * clusters as f64) as usize]);
        let spread = if clusters == 0 { 1.0 } else { 0.3 };
        (0..dims)
            .map(|i| Value::Float(center.map_or(0.0, |c| c[i]) + spread * (unit(rng) * 2.0 - 1.0)))
            .collect()
    };
    let t = Instant::now();
    store.begin()?;
    for _ in 0..docs {
        let properties = BTreeMap::from([
            ("emb".to_string(), Value::List(vector(&mut rng))),
            // In steps of a tenth of a percent.
            ("p".to_string(), Value::Float((unit(&mut rng) * 1000.0).floor() / 10.0)),
        ]);
        store.create_node(&["Doc".into(), "Approx".into()], properties)?;
    }
    store.commit()?;
    let db = Database::with_store(store);
    let create = |name: &str, label: &str, hnsw: bool| {
        db.execute(&format!(
            "CREATE VECTOR INDEX {name} FOR (d:{label}) ON d.emb WITH [d.p] \
             OPTIONS {{indexConfig: {{`vector.dimensions`: {dims}, `vector.hnsw.enabled`: {hnsw}}}}}"
        ))
    };
    create("doc_emb", "Doc", false)?;
    println!(
        "{docs} docs x {dims} dims ({:.0} MiB of f32 vectors, {clusters} clusters), load {:.1?}, k = {K}, median of {reps}\n",
        (docs * dims * 4) as f64 / (1 << 20) as f64,
        t.elapsed()
    );

    let query = |rng: &mut u64| -> BTreeMap<String, Value> {
        BTreeMap::from([("q".to_string(), Value::List(vector(rng)))])
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

    if hnsw {
        create("approx_emb", "Approx", true)?;
        let t = Instant::now();
        ids(&via_index.replace("doc_emb", "approx_emb"), &params)?;
        println!("  {:<44} {:>10.1} s", "approximate index: build", t.elapsed().as_secs_f64());
    }
    println!("\nfiltered, by the share of the vectors that pass:");
    for share in ["0.1", "1", "10", "50", "90", "100"] {
        let filter = format!("d.p < {share}.0").replace("0.1.0", "0.1");
        let scan = format!(
            "MATCH (d:Doc) WHERE {filter} WITH d, vector.similarity.cosine(d.emb, $q) AS score \
             RETURN id(d) AS id, score ORDER BY score DESC, id LIMIT {K}"
        );
        let search = |index: &str| {
            format!(
                "MATCH (d) SEARCH d IN (VECTOR INDEX {index} FOR $q WHERE {filter} LIMIT {K}) SCORE AS score \
                 RETURN id(d) AS id, score"
            )
        };
        // The same queries for every way.
        let mut qrng = rng;
        let queries: Vec<_> = (0..reps).map(|_| query(&mut qrng)).collect();
        let timed = |q: &str| -> Result<(Duration, Vec<Vec<String>>)> {
            ids(q, &queries[0])?;
            let (mut results, mut i) = (Vec::new(), 0);
            let t = median(reps, || {
                let t = Instant::now();
                results.push(ids(q, &queries[i])?);
                i += 1;
                Ok(t.elapsed())
            })?;
            Ok((t, results))
        };
        let (t, exact) = timed(&scan)?;
        print!("  {share:>5} %  MATCH … WHERE … ORDER BY {:>8.2} ms", ms(t));
        for (name, kernels) in &backends {
            db.set_kernels(kernels.clone());
            let (t, found) = timed(&search("doc_emb"))?;
            assert_eq!(found, exact, "{name}, {share} %: search and scan disagree");
            print!("   SEARCH, {name} {:>7.2} ms", ms(t));
        }
        if hnsw {
            let (t, found) = timed(&search("approx_emb"))?;
            let hits: usize = found.iter().zip(&exact).map(|(f, e)| f.iter().filter(|x| e.contains(x)).count()).sum();
            let all: usize = exact.iter().map(Vec::len).sum();
            assert!(found.iter().zip(&exact).all(|(f, e)| f.len() == e.len()), "{share} %: too few found");
            print!("   SEARCH, HNSW {:>7.2} ms, recall {:.3}", ms(t), hits as f64 / all.max(1) as f64);
        }
        println!();
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

struct Args {
    docs: usize,
    dims: usize,
    reps: usize,
    clusters: usize,
    hnsw: bool,
}

fn parse_args() -> Args {
    let mut out = Args { docs: 100_000, dims: 384, reps: 5, clusters: 0, hnsw: false };
    let usage = || -> ! {
        eprintln!("usage: vecbench [--docs N] [--dims N] [--reps N] [--clusters N] [--hnsw 0|1]");
        std::process::exit(2)
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let v: usize = args.next().and_then(|v| v.replace('_', "").parse().ok()).unwrap_or_else(|| usage());
        match a.as_str() {
            "--docs" => out.docs = v.max(1),
            "--dims" => out.dims = v.clamp(1, 4096),
            "--reps" => out.reps = v.max(1),
            "--clusters" => out.clusters = v,
            "--hnsw" => out.hnsw = v != 0,
            _ => usage(),
        }
    }
    out
}
