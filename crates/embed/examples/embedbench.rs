//! Embedding throughput: texts/s and tokens/s by batch size and text length, on the CPU
//! and (with `--features gpu`) the GPU, plus Cypher end to end (`vector.embedBatch`
//! against per-row `vector.embed`).
//!
//! ```text
//! cargo run --release -p graphbuff-embed --example embedbench -- --model path/to/model/minilm
//! cargo run --release -p graphbuff-embed --example embedbench --features gpu -- --model ... --device both
//! ```
//!
//! Options: `--model DIR` (default: the tiny test fixture, which measures overhead, not a
//! real model), `--threads N` (0: half the cores), `--reps N`, `--device cpu|gpu|both`,
//! `--quick` (fewer cases).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use graphbuff::{Database, EmbedOptions, Embedder};
use graphbuff_embed::{DeviceKind, EmbedderConfig, LocalEmbedder, ModelSpec, Registry};

const WORDS: &[&str] = &[
    "graph", "node", "edge", "vector", "index", "query", "text", "model", "token", "layer", "batch", "cosine",
    "similarity", "search", "store", "relationship", "property", "label", "pattern", "match",
];

/// `n` texts of about `words` words each, all different.
fn texts(n: usize, words: usize) -> Vec<String> {
    (0..n)
        .map(|i| (0..words).map(|j| WORDS[(i * 7 + j * 3 + j * j) % WORDS.len()]).collect::<Vec<_>>().join(" "))
        .collect()
}

struct Args {
    model: PathBuf,
    threads: usize,
    reps: usize,
    devices: Vec<DeviceKind>,
    quick: bool,
}

fn args() -> Args {
    let mut a = Args {
        model: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-bert"),
        threads: 0,
        reps: 3,
        devices: vec![DeviceKind::Cpu],
        quick: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| panic!("{arg} needs a value"));
        match arg.as_str() {
            "--model" => a.model = PathBuf::from(value()),
            "--threads" => a.threads = value().parse().expect("--threads N"),
            "--reps" => a.reps = value().parse().expect("--reps N"),
            "--device" => {
                a.devices = match value().as_str() {
                    "cpu" => vec![DeviceKind::Cpu],
                    "gpu" => vec![DeviceKind::Gpu],
                    "both" => vec![DeviceKind::Cpu, DeviceKind::Gpu],
                    d => panic!("--device cpu|gpu|both, not {d}"),
                }
            }
            "--quick" => a.quick = true,
            other => panic!("unknown argument {other} (see the file's docs)"),
        }
    }
    a
}

fn embedder(a: &Args, device: DeviceKind, batch_size: usize) -> Result<LocalEmbedder, String> {
    let name = a.model.file_name().and_then(|n| n.to_str()).unwrap_or("model").to_string();
    let spec = ModelSpec::read(&name, &a.model)?;
    LocalEmbedder::new(Registry::from_specs([spec]), EmbedderConfig { batch_size, threads: a.threads, device, ..Default::default() })
}

fn main() {
    let a = args();
    let threads = if a.threads == 0 { std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1)) } else { a.threads };
    println!(
        "embedbench: model {}, {threads} thread(s), {} rep(s), {} core(s)",
        a.model.display(),
        a.reps,
        std::thread::available_parallelism().map_or(1, |n| n.get())
    );
    let none = || Ok(());
    let opts = EmbedOptions::default();
    for &device in &a.devices {
        let name = if device == DeviceKind::Gpu { "gpu" } else { "cpu" };
        let e = match embedder(&a, device, 32) {
            Ok(e) => e,
            Err(err) => {
                println!("\n[{name}] skipped: {err}");
                continue;
            }
        };
        let t = Instant::now();
        e.embed(None, &["warm up"], &opts, &none).unwrap();
        println!("\n[{name}] first call (model load + one text): {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
        let max = e.models()[0].max_tokens;

        println!("{:>8} {:>8} {:>8} {:>10} {:>10} {:>11}", "words", "tokens", "batch", "ms/call", "texts/s", "tokens/s");
        let lengths: &[usize] = if a.quick { &[8, 100] } else { &[8, 40, 100, 400] };
        let batches: &[usize] = if a.quick { &[1, 32] } else { &[1, 8, 32, 64] };
        for &words in lengths {
            for &batch in batches {
                let e = embedder(&a, device, batch).unwrap();
                let n = (batch * 2).max(8);
                let ts = texts(n, words);
                let refs: Vec<&str> = ts.iter().map(String::as_str).collect();
                e.embed(None, &refs[..1], &opts, &none).unwrap(); // load
                let tokens: usize = refs.iter().map(|t| e.token_ids(None, t, &opts).unwrap().len()).sum();
                let t = Instant::now();
                for _ in 0..a.reps {
                    e.embed(None, &refs, &opts, &none).unwrap();
                }
                let secs = t.elapsed().as_secs_f64() / a.reps as f64;
                println!(
                    "{words:>8} {:>8} {batch:>8} {:>10.1} {:>10.1} {:>11.0}",
                    format!("~{}", (tokens / n).min(max)),
                    secs * 1e3,
                    n as f64 / secs,
                    tokens as f64 / secs
                );
            }
        }

        // Cypher end to end: 256 short texts.
        let n = if a.quick { 64 } else { 256 };
        let db = Database::in_memory();
        db.set_embedder(Arc::new(embedder(&a, device, 32).unwrap()));
        db.execute_with_params(
            "UNWIND $texts AS t CREATE (:Doc {text: t})",
            &[("texts".to_string(), graphbuff::Value::from(texts(n, 12)))].into_iter().collect(),
        )
        .unwrap();
        db.execute("RETURN vector.embed('warm up')").unwrap();
        let t = Instant::now();
        db.execute("MATCH (d:Doc) SET d.a = vector.embed(d.text)").unwrap();
        let per_row = t.elapsed().as_secs_f64();
        let t = Instant::now();
        db.execute(
            "MATCH (d:Doc) WITH collect(d) AS docs CALL vector.embedBatch([d IN docs | d.text]) YIELD index, vector \
             WITH docs[index] AS d, vector SET d.b = vector",
        )
        .unwrap();
        let batched = t.elapsed().as_secs_f64();
        println!(
            "cypher, {n} texts of 12 words: vector.embed per row {:.1} texts/s, vector.embedBatch {:.1} texts/s ({:.1}x)",
            n as f64 / per_row,
            n as f64 / batched,
            per_row / batched
        );
    }
}
