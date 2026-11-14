//! Adjacency benchmark: times traversal-heavy queries on each storage backend.
//!
//! ```text
//! cargo run --release -p graphbuff --example adjbench
//! cargo run --release -p graphbuff --example adjbench -- --likes 1000000 --people 100000 --reps 7
//! ```
//!
//! The graph has two parts:
//! * a supernode `(:Hub)` with `--likes` outgoing `:LIKES` relationships to `(:Leaf)` nodes,
//!   followed by 10 `:KNOWS` relationships (created last, so they sit at the end of its
//!   adjacency list),
//! * `--people` `(:P {id})` nodes, each with 10 outgoing `:FOLLOWS` relationships whose
//!   targets are skewed towards low ids, so a few people are followed by many.
//!
//! Each read query runs `--reps` times; the median is reported. `DETACH DELETE` of the hub
//! runs once at the end. Hybrid storage is not measured separately: it reads from the same
//! `MemoryGraph` as the in-memory store.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use graphbuff::{
    Database, GraphStore, IndexDef, MemoryGraph, NodeId, PersistentGraph, PersistentOptions,
    Result, SyncPolicy, Value,
};

struct Config {
    likes: usize,
    people: usize,
    reps: usize,
}

const FOLLOWS_PER_PERSON: usize = 10;
const KNOWS: usize = 10;
const BATCH: usize = 20_000;

const QUERIES: &[(&str, &str)] = &[
    ("typed expand on supernode", "MATCH (s:Hub)-[:KNOWS]->(x) RETURN count(x)"),
    ("supernode LIMIT 10", "MATCH (s:Hub)-[:LIKES]->(x) RETURN x LIMIT 10"),
    ("degree(supernode)", "MATCH (s:Hub) RETURN degree(s)"),
    (
        "friends of friends",
        "MATCH (a:P {id: 7})-[:FOLLOWS]->()-[:FOLLOWS]->(c) RETURN count(c)",
    ),
    (
        "var-length *1..3 count",
        "MATCH (a:P {id: 7})-[:FOLLOWS*1..3]->(c) RETURN count(c)",
    ),
    (
        "var-length *1..3 LIMIT 10",
        "MATCH (a:P {id: 7})-[:FOLLOWS*1..3]->(c) RETURN c LIMIT 10",
    ),
    (
        "incoming of a popular node",
        "MATCH (a:P {id: 0})<-[:FOLLOWS]-(b) RETURN count(b)",
    ),
    (
        "2 hops, filter on start",
        "MATCH (a:P)-[:FOLLOWS]->(b)-[:FOLLOWS]->(c) WHERE a.id % 500 = 7 RETURN count(c)",
    ),
    (
        "cartesian, filter on one side",
        "MATCH (a:P), (b:P) WHERE a.id < 3 AND b.id < 3 RETURN count(*)",
    ),
    (
        "index range, 10 of 20k",
        "MATCH (a:P) WHERE a.id >= 100 AND a.id < 110 RETURN count(a)",
    ),
    (
        "index range, then 2 hops",
        "MATCH (a:P)-[:FOLLOWS]->()-[:FOLLOWS]->(c) WHERE a.id < 20 RETURN count(c)",
    ),
    (
        "small part written last",
        "MATCH (x:Leaf), (h:Hub)-[:KNOWS]->(x) RETURN count(*)",
    ),
    (
        "value join, no index",
        "MATCH (a:P), (b:P) WHERE a.id < 50 AND a.id % 1000 = b.id % 1000 RETURN count(*)",
    ),
    (
        "ORDER BY LIMIT 10 of 200k",
        "MATCH (a:P)-[:FOLLOWS]->(b) RETURN a.id, b.id ORDER BY b.id DESC, a.id LIMIT 10",
    ),
];

fn main() -> Result<()> {
    let cfg = parse_args();
    println!(
        "graph: hub with {} LIKES + {KNOWS} KNOWS, {} people x {FOLLOWS_PER_PERSON} FOLLOWS, {} reps\n",
        cfg.likes, cfg.people, cfg.reps
    );

    run("memory", MemoryGraph::new(), &cfg)?;

    for (name, cache) in [("persistent 64MiB", 64u64 << 20), ("persistent 2MiB", 2 << 20)] {
        let dir = temp_dir(name);
        let store = PersistentGraph::open(
            PersistentOptions::new(&dir)
                .sync(SyncPolicy::OsBuffered)
                .cache_bytes(cache)
                .checkpoint_after_bytes(256 << 20),
        )?;
        let outcome = run(name, store, &cfg);
        let _ = std::fs::remove_dir_all(&dir);
        outcome?;
    }
    Ok(())
}

fn run<S: GraphStore>(name: &str, mut store: S, cfg: &Config) -> Result<()> {
    let t = Instant::now();
    load(&mut store, cfg)?;
    store.checkpoint()?;
    println!("== {name} (load {:.2?})", t.elapsed());
    let db = Database::with_store(store);

    for (label, q) in QUERIES {
        let first = db.execute(q)?; // warm-up, and the answer for the table
        let answer = first
            .rows
            .first()
            .and_then(|r| r.first())
            .map(|v| format!("{v:?}"))
            .unwrap_or_default();
        let mut times: Vec<Duration> = (0..cfg.reps)
            .map(|_| {
                let t = Instant::now();
                db.execute(q).map(|_| t.elapsed())
            })
            .collect::<Result<_>>()?;
        times.sort();
        println!(
            "  {label:<28} {:>10.3} ms   rows={:<4} {}",
            ms(times[times.len() / 2]),
            first.rows.len(),
            truncate(&answer, 40)
        );
    }

    let t = Instant::now();
    db.execute("MATCH (s:Hub) DETACH DELETE s")?;
    println!("  {:<28} {:>10.3} ms   (once)\n", "DETACH DELETE supernode", ms(t.elapsed()));
    Ok(())
}

fn load<S: GraphStore>(s: &mut S, cfg: &Config) -> Result<()> {
    let none = BTreeMap::new;
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
    let hub = s.create_node(&["Hub".into()], none())?;
    for _ in 0..cfg.likes {
        let leaf = s.create_node(&["Leaf".into()], none())?;
        s.create_relationship(hub, leaf, "LIKES", none())?;
        tick(s)?;
    }
    for _ in 0..KNOWS {
        let friend = s.create_node(&["Leaf".into()], none())?;
        s.create_relationship(hub, friend, "KNOWS", none())?;
    }

    let mut people: Vec<NodeId> = Vec::with_capacity(cfg.people);
    for i in 0..cfg.people {
        let mut props = BTreeMap::new();
        props.insert("id".to_string(), Value::Int(i as i64));
        people.push(s.create_node(&["P".into()], props)?);
        tick(s)?;
    }
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    for &p in &people {
        for _ in 0..FOLLOWS_PER_PERSON {
            // Square of a uniform number in [0, 1): skewed towards low ids.
            let u = (next(&mut rng) >> 11) as f64 / (1u64 << 53) as f64;
            let target = people[((u * u) * cfg.people as f64) as usize];
            s.create_relationship(p, target, "FOLLOWS", none())?;
            tick(s)?;
        }
    }
    s.commit()?;

    s.begin()?;
    s.create_index(IndexDef::node("p_id", "P", "id"))?;
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

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).chain("…".chars()).collect()
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = std::env::temp_dir().join(format!("graphbuff-adjbench-{}-{slug}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn parse_args() -> Config {
    let mut cfg = Config {
        likes: 200_000,
        people: 20_000,
        reps: 5,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || -> usize {
            args.next()
                .and_then(|v| v.replace('_', "").parse().ok())
                .unwrap_or_else(|| usage(&format!("{a} needs a number")))
        };
        match a.as_str() {
            "--likes" => cfg.likes = val(),
            "--people" => cfg.people = val(),
            "--reps" => cfg.reps = val().max(1),
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }
    if cfg.people == 0 {
        usage("--people must be at least 1");
    }
    cfg
}

fn usage(err: &str) -> ! {
    if !err.is_empty() {
        eprintln!("error: {err}");
    }
    eprintln!("usage: adjbench [--likes N] [--people N] [--reps N]");
    std::process::exit(if err.is_empty() { 0 } else { 2 });
}
