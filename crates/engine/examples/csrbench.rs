//! CSR export benchmark: how long `Database::csr` takes to build and how
//! much memory it holds, and queries run through Cypher and through the CPU kernels
//! ([`CpuKernels`]): a whole-graph two-hop count (also as a plain loop over the CSR
//! arrays), a two-hop aggregate, a filtered aggregate and an `ORDER BY … LIMIT`.
//!
//! ```text
//! cargo run --release -p graphbuff --example csrbench
//! cargo run --release -p graphbuff --example csrbench -- --people 1000000 --reps 3
//! ```
//!
//! The graph: `--people` `(:P {id})` nodes, each with 10 `:FOLLOWS` relationships skewed
//! towards low ids (as in `adjbench`) and 3 `:LIKES` to uniformly chosen people.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use graphbuff::{
    CmpOp, Column, CpuKernels, Csr, Database, KernelMode, Kernels, ResultValue, Direction, GraphStore, MemoryGraph, NodeId, PersistentGraph, PersistentOptions,
    Result, SyncPolicy, Value,
};

const FOLLOWS: usize = 10;
const LIKES: usize = 3;
const BATCH: usize = 20_000;

struct Config {
    people: usize,
    reps: usize,
}

fn main() -> Result<()> {
    let cfg = parse_args();
    println!(
        "graph: {} people x ({FOLLOWS} FOLLOWS + {LIKES} LIKES), {} reps\n",
        cfg.people, cfg.reps
    );
    run("memory", MemoryGraph::new(), &cfg)?;
    let dir = std::env::temp_dir().join(format!("graphbuff-csrbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = PersistentGraph::open(
        PersistentOptions::new(&dir)
            .sync(SyncPolicy::OsBuffered)
            .cache_bytes(64 << 20)
            .checkpoint_after_bytes(256 << 20),
    )?;
    let outcome = run("persistent 64MiB", store, &cfg);
    let _ = std::fs::remove_dir_all(PathBuf::from(&dir));
    outcome
}

fn run<S: GraphStore>(name: &str, mut store: S, cfg: &Config) -> Result<()> {
    let t = Instant::now();
    load(&mut store, cfg)?;
    store.checkpoint()?;
    println!("== {name} (load {:.2?})", t.elapsed());
    let db = Database::with_store(store);
    // The "Cypher" lines run on rows; the planner's own use of kernels is timed at the end.
    db.set_kernel_mode(KernelMode::Off);

    // The first call after a commit builds; later ones share it.
    let builds = median(cfg.reps, || {
        db.execute("CREATE (:Tmp)")?; // a commit, so the next call rebuilds
        let t = Instant::now();
        db.csr()?;
        Ok(t.elapsed())
    })?;
    db.execute("MATCH (t:Tmp) DELETE t")?;
    let csr = db.csr()?;
    let cached = median(cfg.reps, || {
        let t = Instant::now();
        db.csr()?;
        Ok(t.elapsed())
    })?;
    println!(
        "  {:<34} {:>10.3} ms   {} nodes, {} rels, {:.1} MiB",
        "build",
        ms(builds),
        csr.node_count(),
        csr.relationship_count(),
        csr.memory_bytes() as f64 / (1 << 20) as f64
    );
    println!("  {:<34} {:>10.3} ms", "cached", ms(cached));

    let q = "MATCH (a)-[:FOLLOWS]->()-[:FOLLOWS]->(c) RETURN count(c)";
    let expected = match db.execute(q)?.rows[0][0] {
        graphbuff::ResultValue::Int(n) => n as u64,
        ref v => panic!("unexpected count {v:?}"),
    };
    let cypher = median(cfg.reps, || {
        let t = Instant::now();
        db.execute(q)?;
        Ok(t.elapsed())
    })?;
    let mut got = 0;
    let loops = median(cfg.reps, || {
        let t = Instant::now();
        got = two_hops(&csr, "FOLLOWS");
        Ok(t.elapsed())
    })?;
    assert_eq!(got, expected, "CSR and Cypher disagree");
    println!("  {:<34} {:>10.3} ms   {expected} paths", "2-hop count, Cypher", ms(cypher));
    println!("  {:<34} {:>10.3} ms", "2-hop count, CSR loop", ms(loops));

    let k = CpuKernels::new();
    let follows = csr.type_ids(&["FOLLOWS".to_string()]);
    let t = Instant::now();
    let id = db.read(|s| Column::build(s, &csr, "id"))?;
    println!("  {:<34} {:>10.3} ms", "column `id` build", ms(t.elapsed()));
    let all: Vec<u32> = (0..csr.node_count() as u32).collect();

    // Two hops, then an aggregate of the end nodes, through walk counts. FOLLOWS has a
    // few self-loops, which walks may use twice and Cypher paths may not, so the sums
    // differ slightly.
    let q = "MATCH (a)-[:FOLLOWS]->()-[:FOLLOWS]->(c) RETURN sum(c.id)";
    let want = db.execute(q)?.rows[0][0].clone();
    let cypher = median(cfg.reps, || {
        let t = Instant::now();
        db.execute(q)?;
        Ok(t.elapsed())
    })?;
    let mut got = Value::Null;
    let kernel = median(cfg.reps, || {
        let t = Instant::now();
        let one = k.propagate(&csr, &vec![1; csr.node_count()], Direction::Outgoing, &follows)?;
        let two = k.propagate(&csr, &one, Direction::Outgoing, &follows)?;
        got = k.aggregate_counts(&id, &two)?.sum;
        Ok(t.elapsed())
    })?;
    println!("  {:<34} {:>10.3} ms   {want:?}", "2-hop sum, Cypher", ms(cypher));
    println!("  {:<34} {:>10.3} ms   {got} (walks)", "2-hop sum, kernels", ms(kernel));

    // A filtered aggregate over every node.
    let limit = (cfg.people / 3) as i64;
    let q = format!("MATCH (n) WHERE n.id < {limit} RETURN count(*), sum(n.id), max(n.id)");
    let want = db.execute(&q)?.rows[0].clone();
    let cypher = median(cfg.reps, || {
        let t = Instant::now();
        db.execute(&q)?;
        Ok(t.elapsed())
    })?;
    let mut sum = Value::Null;
    let kernel = median(cfg.reps, || {
        let t = Instant::now();
        let mask = k.filter(&id, CmpOp::Lt, &Value::Int(limit));
        let rows = k.select(&all, &mask);
        sum = k.aggregate(&id, &rows).sum;
        Ok(t.elapsed())
    })?;
    assert_eq!(want[1], ResultValue::Int(sum.as_int().unwrap_or(-1)), "filter: kernels and Cypher disagree");
    println!("  {:<34} {:>10.3} ms", "filter + aggregate, Cypher", ms(cypher));
    println!("  {:<34} {:>10.3} ms", "filter + aggregate, kernels", ms(kernel));

    // Top-N over every node.
    let q = "MATCH (n) RETURN n.id ORDER BY n.id DESC LIMIT 10";
    let cypher = median(cfg.reps, || {
        let t = Instant::now();
        db.execute(q)?;
        Ok(t.elapsed())
    })?;
    let kernel = median(cfg.reps, || {
        let t = Instant::now();
        k.top_n(&id, &all, 10, true);
        Ok(t.elapsed())
    })?;
    println!("  {:<34} {:>10.3} ms", "ORDER BY DESC LIMIT 10, Cypher", ms(cypher));
    println!("  {:<34} {:>10.3} ms", "ORDER BY DESC LIMIT 10, kernels", ms(kernel));

    // The same queries through Cypher with the planner choosing. The export is
    // built; the first (untimed) run builds the columns.
    db.set_kernel_mode(KernelMode::Auto);
    for (name, q) in [
        ("2-hop sum", "MATCH (a)-[:FOLLOWS]->()-[:FOLLOWS]->(c) RETURN sum(c.id)".to_string()),
        ("filter + aggregate", format!("MATCH (n) WHERE n.id < {limit} RETURN count(*), sum(n.id), max(n.id)")),
        ("ORDER BY DESC LIMIT 10", q.to_string()),
    ] {
        let chosen = if db.explain(&q)?.contains("Kernel") { "kernels" } else { "rows" };
        let first = db.execute(&q)?.rows;
        db.set_kernel_mode(KernelMode::Off);
        assert_eq!(db.execute(&q)?.rows, first, "{q}: kernels and rows disagree");
        db.set_kernel_mode(KernelMode::Auto);
        let t = median(cfg.reps, || {
            let t = Instant::now();
            db.execute(&q)?;
            Ok(t.elapsed())
        })?;
        println!("  {:<34} {:>10.3} ms   ({chosen})", format!("{name}, Cypher auto"), ms(t));
    }

    // Graph algorithm procedures, on the cached export, all rows consumed.
    for (name, q) in [
        ("algo.pageRank (20 iterations)", "CALL algo.pageRank() YIELD score RETURN max(score)"),
        ("algo.wcc", "CALL algo.wcc() YIELD componentId RETURN count(DISTINCT componentId)"),
        ("algo.triangleCount", "CALL algo.triangleCount() YIELD triangleCount RETURN sum(triangleCount) / 3"),
        ("algo.bfs from node 0", "MATCH (p:P {id: 0}) CALL algo.bfs(p) YIELD depth RETURN max(depth), count(*)"),
    ] {
        let mut out = Vec::new();
        let t = median(cfg.reps, || {
            let t = Instant::now();
            out = db.execute(q)?.rows;
            Ok(t.elapsed())
        })?;
        println!("  {:<34} {:>10.3} ms   {:?}", name, ms(t), out[0]);
    }
    println!();
    Ok(())
}

/// Two-hop paths over one type: for every relationship `a -> b`, the out-degree of `b`,
/// less the relationship itself when it is a self-loop (Cypher uses a relationship at
/// most once per path).
fn two_hops(csr: &Csr, ty: &str) -> u64 {
    let Some(t) = csr.type_id(ty) else { return 0 };
    let adj = csr.adjacency(t, Direction::Outgoing);
    let (off, targets) = (adj.offsets(), adj.targets());
    let mut paths = 0u64;
    for a in 0..csr.node_count() {
        for &b in &targets[off[a] as usize..off[a + 1] as usize] {
            paths += u64::from(off[b as usize + 1] - off[b as usize]) - u64::from(b as usize == a);
        }
    }
    paths
}

fn median(reps: usize, mut f: impl FnMut() -> Result<Duration>) -> Result<Duration> {
    let mut times = (0..reps).map(|_| f()).collect::<Result<Vec<_>>>()?;
    times.sort();
    Ok(times[times.len() / 2])
}

fn load<S: GraphStore>(s: &mut S, cfg: &Config) -> Result<()> {
    let mut pending = 0usize;
    let mut tick = |s: &mut S| -> Result<()> {
        pending += 1;
        if pending >= BATCH {
            s.commit()?;
            s.begin()?;
            pending = 0;
        }
        Ok(())
    };
    s.begin()?;
    let mut people: Vec<NodeId> = Vec::with_capacity(cfg.people);
    for i in 0..cfg.people {
        let props = BTreeMap::from([("id".to_string(), Value::Int(i as i64))]);
        people.push(s.create_node(&["P".into()], props)?);
        tick(s)?;
    }
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    for &p in &people {
        for _ in 0..FOLLOWS {
            let u = (next(&mut rng) >> 11) as f64 / (1u64 << 53) as f64;
            let target = people[((u * u) * cfg.people as f64) as usize];
            s.create_relationship(p, target, "FOLLOWS", BTreeMap::new())?;
            tick(s)?;
        }
        for _ in 0..LIKES {
            let target = people[(next(&mut rng) % cfg.people as u64) as usize];
            s.create_relationship(p, target, "LIKES", BTreeMap::new())?;
            tick(s)?;
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

fn parse_args() -> Config {
    let mut cfg = Config { people: 100_000, reps: 3 };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || -> usize {
            args.next()
                .and_then(|v| v.replace('_', "").parse().ok())
                .unwrap_or_else(|| usage(&format!("{a} needs a number")))
        };
        match a.as_str() {
            "--people" => cfg.people = val().max(1),
            "--reps" => cfg.reps = val().max(1),
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }
    cfg
}

fn usage(err: &str) -> ! {
    if !err.is_empty() {
        eprintln!("error: {err}");
    }
    eprintln!("usage: csrbench [--people N] [--reps N]");
    std::process::exit(if err.is_empty() { 0 } else { 2 });
}
