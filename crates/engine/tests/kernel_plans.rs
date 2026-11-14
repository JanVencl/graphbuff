//! Kernel operators in plans: random queries give the same rows with kernels
//! forced on and off, on every kind of storage; the cost model and `EXPLAIN` show the
//! choice; writes and transactions stay on rows.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{BoxedStore, Database, GraphStore, KernelMode, ResultValue, StorageMode, kernel_stats};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-kplantest-{}-{}-{name}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, m: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % m
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn exec<S: GraphStore>(db: &Database<S>, q: &str) -> Vec<Vec<ResultValue>> {
    db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}")).rows
}

/// Nodes with labels `L`/`M`, integer `x` (many ties, some missing), integer `y`, float `f`
/// and a property `s` that is a string on some nodes; relationships `A`, `B`, `C` with
/// self-loops, parallel relationships and two-cycles; some deletes.
fn load<S: GraphStore>(db: &Database<S>, rng: &mut Rng) {
    let mut nodes = Vec::new();
    for i in 0..260 {
        let mut labels = String::new();
        if rng.chance(40) {
            labels.push_str(":L");
        }
        if rng.chance(30) {
            labels.push_str(":M");
        }
        let mut props = vec![format!("id: {i}"), format!("y: {}", rng.below(1000) as i64 - 500)];
        if rng.chance(85) {
            props.push(format!("x: {}", rng.below(12) as i64 - 4));
        }
        // Decimals that floats do not hold exactly, so sums depend on the order.
        props.push(format!("f: {}.{}", rng.below(50), rng.pick(&[1, 3, 7, 9])));
        props.push(if rng.chance(10) { "s: 'text'".into() } else { format!("s: {}", rng.below(9)) });
        nodes.push(format!("CREATE (:N{labels} {{{}}})", props.join(", ")));
    }
    for q in nodes {
        exec(db, &q);
    }
    let mut rels = Vec::new();
    for _ in 0..900 {
        let a = rng.below(260);
        let b = match rng.below(10) {
            0 => a,
            1 => (a + 1) % 260,
            _ => rng.below(260),
        };
        let t = *rng.pick(&["A", "A", "B", "C"]);
        rels.push(format!("[{a},{b},'{t}']"));
        if rng.chance(5) {
            rels.push(format!("[{b},{a},'{t}']"));
        }
    }
    for t in ["A", "B", "C"] {
        exec(
            db,
            &format!(
                "UNWIND [{}] AS r WITH r WHERE r[2] = '{t}' \
                 MATCH (a:N {{id: r[0]}}), (b:N {{id: r[1]}}) CREATE (a)-[:{t}]->(b)",
                rels.join(",")
            ),
        );
    }
    exec(db, "MATCH (n:N) WHERE n.id % 29 = 3 DETACH DELETE n");
    exec(db, "MATCH ()-[r:B]->() WHERE id(r) % 7 = 2 DELETE r");
}

fn node(rng: &mut Rng, var: &str) -> String {
    let labels = *rng.pick(&["", "", ":L", ":M", ":L:M"]);
    let props = if rng.chance(10) { format!(" {{x: {}}}", rng.below(6) as i64 - 2) } else { String::new() };
    format!("({var}{labels}{props})")
}

fn rel(rng: &mut Rng) -> String {
    let types = *rng.pick(&["", ":A", ":A|B", ":C", ":B"]);
    let var = if rng.chance(15) { "r" } else { "" };
    match rng.below(3) {
        0 => format!("-[{var}{types}]->"),
        1 => format!("<-[{var}{types}]-"),
        _ => format!("-[{var}{types}]-"),
    }
}

fn predicate(rng: &mut Rng, var: &str, depth: u32) -> String {
    let leaf = |rng: &mut Rng| {
        let key = *rng.pick(&["x", "x", "y", "f", "s"]);
        let op = *rng.pick(&["=", "<>", "<", "<=", ">", ">="]);
        let c = match rng.below(6) {
            0 => "1.5".to_string(),
            1 => "-2".to_string(),
            2 => "null".to_string(),
            3 => "$p".to_string(),
            _ => format!("{}", rng.below(8) as i64 - 3),
        };
        match rng.below(10) {
            0 => format!("{var}.x IS NULL"),
            1 => format!("{var}.x IS NOT NULL"),
            2 => format!("{var}:L"),
            3 => format!("{c} {op} {var}.{key}"),
            _ => format!("{var}.{key} {op} {c}"),
        }
    };
    if depth == 0 || rng.chance(50) {
        return leaf(rng);
    }
    match rng.below(3) {
        0 => format!("NOT ({})", predicate(rng, var, depth - 1)),
        1 => format!("({} OR {})", predicate(rng, var, depth - 1), predicate(rng, var, depth - 1)),
        _ => format!("({} AND {})", predicate(rng, var, depth - 1), predicate(rng, var, depth - 1)),
    }
}

/// A random query of a shape kernels may answer.
fn query(rng: &mut Rng) -> String {
    if rng.chance(25) {
        let mut q = format!("MATCH {}", node(rng, "n"));
        if rng.chance(60) {
            q += &format!(" WHERE {}", predicate(rng, "n", 2));
        }
        let key = *rng.pick(&["x", "x", "f", "y", "s"]);
        let desc = if rng.chance(50) { " DESC" } else { "" };
        let skip = if rng.chance(30) { format!(" SKIP {}", rng.below(5)) } else { String::new() };
        return format!("{q} RETURN n.id, n.{key} AS k ORDER BY n.{key}{desc}{skip} LIMIT {}", rng.below(40));
    }
    let hops = rng.below(3) as usize;
    let vars = ["a", "b", "c"];
    let mut pattern = node(rng, vars[0]);
    for v in &vars[1..=hops] {
        pattern += &rel(rng);
        pattern += &node(rng, v);
    }
    let mut q = format!("MATCH {pattern}");
    let mut conds = Vec::new();
    for v in &vars[..=hops] {
        if rng.chance(35) {
            conds.push(predicate(rng, v, 2));
        }
    }
    if !conds.is_empty() {
        q += &format!(" WHERE {}", conds.join(" AND "));
    }
    let end = if rng.chance(50) { vars[0] } else { vars[hops] };
    let key = *rng.pick(&["x", "x", "y", "f", "s"]);
    let mut items = vec!["count(*) AS rows".to_string()];
    for f in ["count", "sum", "avg", "min", "max"] {
        if rng.chance(50) {
            items.push(format!("{f}({end}.{key}) AS {f}"));
        }
    }
    if rng.chance(20) {
        items.push(format!("count({end}) AS nodes"));
    }
    format!("{q} RETURN {}", items.join(", "))
}

fn differential(db: &Database<BoxedStore>, seed: u64) {
    gpu_backend(db);
    let mut rng = Rng(seed);
    load(db, &mut rng);
    let params = std::collections::BTreeMap::from([("p".to_string(), graphbuff::Value::Int(1))]);
    let mut kernel_plans = 0;
    for i in 0..600 {
        let q = query(&mut rng);
        db.set_kernel_mode(KernelMode::Off);
        let rows = db.execute_with_params(&q, &params);
        db.set_kernel_mode(KernelMode::Always);
        let kernels = db.execute_with_params(&q, &params);
        match (rows, kernels) {
            (Ok(a), Ok(b)) => assert_eq!(a.rows, b.rows, "query {i}: {q}"),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "query {i}: {q}"),
            (a, b) => panic!("query {i}: {q}\nrows: {a:?}\nkernels: {b:?}"),
        }
        if db.explain(&q).unwrap().contains("Kernel") {
            kernel_plans += 1;
        }
    }
    assert!(kernel_plans > 300, "only {kernel_plans} of 600 plans used kernels");
}

#[test]
fn kernels_match_rows_on_every_store() {
    let before = kernel_stats();
    differential(&Database::open(StorageMode::InMemory).unwrap(), 0x9e37_79b9_7f4a_7c15);
    let hybrid = TempDir::new("hybrid");
    differential(&Database::open(StorageMode::hybrid(&hybrid.0)).unwrap(), 0x2545_f491_4f6c_dd1d);
    let persistent = TempDir::new("persistent");
    differential(&Database::open(StorageMode::persistent(&persistent.0)).unwrap(), 0x1234_5678_9abc_def1);
    let after = kernel_stats();
    let (runs, fallbacks) = (after.runs - before.runs, after.fallbacks - before.fallbacks);
    // Floats and strings fall back; most others run on the kernels.
    assert!(runs > 1000 && fallbacks * 2 < runs, "{runs} kernel runs, {fallbacks} fell back");
}

#[test]
fn cost_model_and_explain() {
    let db = Database::in_memory();
    exec(&db, "UNWIND range(1, 30000) AS i CREATE (:P {id: i, g: i % 7})");
    exec(&db, "CREATE INDEX FOR (p:P) ON (p.id)");
    exec(&db, "MATCH (a:P) WHERE a.id % 3 = 0 MATCH (b:P {id: a.id / 3 + 1}) CREATE (a)-[:F]->(b), (b)-[:F]->(a)");
    let two_hops = "MATCH (a)-[:F]->()-[:F]->(c) RETURN count(*), sum(c.g)";
    let filter = "MATCH (n:P) WHERE n.g = 3 RETURN count(*), max(n.id)";
    // Tiny queries stay on rows.
    assert!(!db.explain("MATCH (n:P {id: 5}) RETURN count(*)").unwrap().contains("Kernel"));
    // Two hops over 30,000 nodes: worth building the export.
    let plan = db.explain(two_hops).unwrap();
    assert!(plan.starts_with("Select\n  KernelAggregate count(*), sum(c.g) over (a)-[:F]->()-[:F]->(c) (cpu; cost"), "{plan}");
    // Once the export is built, it costs a filter nothing.
    let cost = |db: &Database, q: &str| -> f64 {
        let plan = db.explain(q).unwrap();
        let c = plan.split("cost ").nth(1).unwrap_or_else(|| panic!("{plan}"));
        c.split(' ').next().unwrap().parse().unwrap()
    };
    let unbuilt = cost(&db, filter);
    let want = exec(&db, two_hops);
    db.set_kernel_mode(KernelMode::Off);
    assert_eq!(exec(&db, two_hops), want);
    db.set_kernel_mode(KernelMode::Auto);
    let built = cost(&db, filter);
    assert!(built < unbuilt / 2.0, "{built} vs {unbuilt}");
    let profile = db.execute(&format!("PROFILE {filter}")).unwrap().plan.unwrap().to_string();
    assert!(profile.contains("KernelAggregate"), "{profile}");
    let plan = db.explain("MATCH (n:P) RETURN n.id ORDER BY n.g DESC, n.id LIMIT 3").unwrap();
    assert!(!plan.contains("Kernel"), "two sort keys: {plan}");
    let top = "MATCH (n:P) RETURN n.id ORDER BY n.g DESC LIMIT 3";
    assert!(db.explain(top).unwrap().contains("KernelTop n.g DESC"));
    assert_eq!(
        exec(&db, top),
        [[ResultValue::Int(6)], [ResultValue::Int(13)], [ResultValue::Int(20)]]
    );
    // A commit drops the cache.
    exec(&db, "CREATE (:P {id: 0, g: 3})");
    assert!(cost(&db, filter) >= unbuilt);
    db.set_kernel_mode(KernelMode::Off);
    assert!(!db.explain(two_hops).unwrap().contains("Kernel"));
    db.set_kernel_mode(KernelMode::Always);
    // Writing queries and transactions run on rows.
    assert!(!db.explain("MATCH (n:P) WITH count(*) AS c CREATE (:Q {c: c})").unwrap().contains("Kernel"));
    let mut tx = db.begin().unwrap();
    let plan = tx.execute(&format!("EXPLAIN {filter}")).unwrap().plan.unwrap().to_string();
    assert!(!plan.contains("Kernel"), "{plan}");
    let in_tx = tx.execute(filter).unwrap().rows;
    tx.rollback();
    assert_eq!(exec(&db, filter), in_tx);
}

/// With the `gpu` feature, the kernels run on the GPU when there is one, at every size.
fn gpu_backend<S: GraphStore>(db: &Database<S>) {
    #[cfg(feature = "gpu")]
    match graphbuff::WgpuKernels::new() {
        Ok(k) => {
            eprintln!("kernels on {}", k.adapter());
            db.set_kernels(std::sync::Arc::new(k.with_min_work(0)));
        }
        Err(e) => eprintln!("GPU kernels skipped: {e}"),
    }
    let _ = db;
}
