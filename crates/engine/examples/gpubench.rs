//! GPU kernel benchmark: the same kernels and Cypher queries on [`CpuKernels`]
//! (every core) and on [`WgpuKernels`], with results checked equal.
//!
//! ```text
//! cargo run --release -p graphbuff --features gpu --example gpubench
//! cargo run --release -p graphbuff --features gpu --example gpubench -- --people 200000 --reps 5
//! ```
//!
//! The graph (in memory): `--people` `(:P {id})` nodes, each with 10 `:FOLLOWS`
//! relationships skewed towards low ids and 3 `:LIKES` to uniformly chosen people, as in
//! `csrbench`; the default million people make 13 million relationships.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use graphbuff::{
    CpuKernels, Database, Direction, GraphStore, Hop, KernelMode, Kernels, MemoryGraph, NodeId, Result, Value,
    WgpuKernels,
};

const FOLLOWS: usize = 10;
const LIKES: usize = 3;

fn main() -> Result<()> {
    let (people, reps) = parse_args();
    let gpu = Arc::new(WgpuKernels::new()?);
    let cpu = Arc::new(CpuKernels::new());
    println!("GPU: {} (64-bit floats: {}); CPU: {} threads", gpu.adapter(), gpu.has_f64(), cpu.threads());

    let mut store = MemoryGraph::new();
    let t = Instant::now();
    load(&mut store, people)?;
    let db = Database::with_store(store);
    let csr = db.csr()?;
    println!(
        "graph: {} nodes, {} relationships (load {:.1?}), median of {reps}\n",
        csr.node_count(),
        csr.relationship_count(),
        t.elapsed()
    );
    let n = csr.node_count();
    let all = csr.type_ids(&[]);
    let follows = csr.type_ids(&["FOLLOWS".to_string()]);
    let backends: [(&str, Arc<dyn Kernels>); 2] = [("cpu", cpu.clone()), ("gpu", gpu.clone())];

    println!("  {:<40} {:>10} {:>10} {:>8}", "kernel", "cpu ms", "gpu ms", "speedup");
    let row = |name: &str, f: &dyn Fn(&dyn Kernels) -> Result<String>| -> Result<()> {
        // The first GPU call copies the export's layouts to the device: untimed.
        let mut outs = Vec::new();
        for (_, k) in &backends {
            outs.push(f(&**k)?);
        }
        assert_eq!(outs[0], outs[1], "{name}: CPU and GPU disagree");
        let mut times = Vec::new();
        for (_, k) in &backends {
            times.push(median(reps, || {
                let t = Instant::now();
                f(&**k)?;
                Ok(t.elapsed())
            })?);
        }
        println!(
            "  {:<40} {:>10.1} {:>10.1} {:>7.1}x",
            name,
            ms(times[0]),
            ms(times[1]),
            times[0].as_secs_f64() / times[1].as_secs_f64()
        );
        Ok(())
    };
    // Walk counts from every node, all hops in one `walk` call (on the GPU, the counts stay
    // on the device between hops).
    let walks = |k: &dyn Kernels, hops: usize, dir: Direction, types: &[u32]| -> Result<String> {
        let hops = vec![Hop { dir, types, mask: None }; hops];
        Ok(k.walk(&csr, &vec![1u64; n], &hops)?.iter().sum::<u64>().to_string())
    };
    row("1-hop walk, FOLLOWS out", &|k| walks(k, 1, Direction::Outgoing, &follows))?;
    row("2-hop walks, FOLLOWS out", &|k| walks(k, 2, Direction::Outgoing, &follows))?;
    row("3-hop walks, FOLLOWS out", &|k| walks(k, 3, Direction::Outgoing, &follows))?;
    row("3-hop walks, all types both ways", &|k| walks(k, 3, Direction::Both, &all))?;
    row("6-hop walks, LIKES out", &|k| walks(k, 6, Direction::Outgoing, &csr.type_ids(&["LIKES".to_string()])))?;
    row("BFS from node 0, both ways", &|k| {
        let d = k.bfs(&csr, &[0], Direction::Both, &all, u32::MAX);
        Ok(format!("{:?}", d.iter().filter(|&&x| x != u32::MAX).max()))
    })?;
    row("WCC", &|k| Ok(format!("{:?}", k.wcc(&csr, &all).iter().sum::<u32>())))?;
    row("PageRank, 20 iterations", &|k| {
        let (s, it) = k.page_rank(&csr, &all, Direction::Outgoing, 0.85, 20, 0.0);
        Ok(format!("{it} {:x}", s.iter().fold(0u64, |h, x| h.rotate_left(5) ^ x.to_bits())))
    })?;
    row("triangles", &|k| Ok(k.triangles(&csr, &all).iter().sum::<u64>().to_string()))?;

    // Cypher, with the planner choosing kernels (its cost model is the same for both
    // backends); columns and export built by an untimed first run.
    println!("\n  {:<40} {:>10} {:>10} {:>8}", "Cypher (kernel mode Auto)", "cpu ms", "gpu ms", "speedup");
    db.set_kernel_mode(KernelMode::Auto);
    for (name, q) in [
        ("2-hop count(*)", "MATCH (a)-[:FOLLOWS]->()-[:FOLLOWS]->(c) RETURN count(*)"),
        ("2-hop sum(c.id), any direction", "MATCH (a)-[:FOLLOWS]-()-[]-(c) RETURN sum(c.id)"),
        ("algo.pageRank (20 iterations)", "CALL algo.pageRank() YIELD score RETURN max(score)"),
        ("algo.wcc", "CALL algo.wcc() YIELD componentId RETURN count(DISTINCT componentId)"),
        ("algo.triangleCount", "CALL algo.triangleCount() YIELD triangleCount RETURN sum(triangleCount) / 3"),
    ] {
        let mut outs = Vec::new();
        let mut times = Vec::new();
        for (_, k) in &backends {
            db.set_kernels(k.clone());
            let first = db.execute(q)?.rows;
            times.push(median(reps, || {
                let t = Instant::now();
                db.execute(q)?;
                Ok(t.elapsed())
            })?);
            outs.push(first);
        }
        assert_eq!(outs[0], outs[1], "{q}: CPU and GPU disagree");
        println!(
            "  {:<40} {:>10.1} {:>10.1} {:>7.1}x",
            name,
            ms(times[0]),
            ms(times[1]),
            times[0].as_secs_f64() / times[1].as_secs_f64()
        );
    }
    println!("\nGPU kernel calls: {}", gpu.gpu_runs());
    Ok(())
}

fn median(reps: usize, mut f: impl FnMut() -> Result<Duration>) -> Result<Duration> {
    let mut times = (0..reps).map(|_| f()).collect::<Result<Vec<_>>>()?;
    times.sort();
    Ok(times[times.len() / 2])
}

fn load(s: &mut MemoryGraph, people: usize) -> Result<()> {
    s.begin()?;
    let mut ids: Vec<NodeId> = Vec::with_capacity(people);
    for i in 0..people {
        let props = BTreeMap::from([("id".to_string(), Value::Int(i as i64))]);
        ids.push(s.create_node(&["P".into()], props)?);
    }
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    for &p in &ids {
        for _ in 0..FOLLOWS {
            let u = (next(&mut rng) >> 11) as f64 / (1u64 << 53) as f64;
            s.create_relationship(p, ids[((u * u) * people as f64) as usize], "FOLLOWS", BTreeMap::new())?;
        }
        for _ in 0..LIKES {
            let target = ids[(next(&mut rng) % people as u64) as usize];
            s.create_relationship(p, target, "LIKES", BTreeMap::new())?;
        }
    }
    s.commit()
}

fn next(state: &mut u64) -> u64 {
    // xorshift64*
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn parse_args() -> (usize, usize) {
    let (mut people, mut reps) = (1_000_000, 3);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || -> usize {
            args.next().and_then(|v| v.replace('_', "").parse().ok()).unwrap_or_else(|| {
                eprintln!("usage: gpubench [--people N] [--reps N]");
                std::process::exit(2)
            })
        };
        match a.as_str() {
            "--people" => people = val().max(1),
            "--reps" => reps = val().max(1),
            _ => {
                eprintln!("usage: gpubench [--people N] [--reps N]");
                std::process::exit(2)
            }
        }
    }
    (people, reps)
}
