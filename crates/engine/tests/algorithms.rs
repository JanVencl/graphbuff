//! Graph algorithm procedures against reference implementations over the edge
//! list Cypher returns, and against Cypher itself where it can say the same thing.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use graphbuff::{BoxedStore, Database, Error, GraphStore, ResultValue, StorageMode};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "graphbuff-algotest-{}-{}-{name}",
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

fn rows<S: GraphStore>(db: &Database<S>, q: &str) -> Vec<Vec<ResultValue>> {
    db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}")).rows
}

fn int(v: &ResultValue) -> i64 {
    match v {
        ResultValue::Int(i) => *i,
        v => panic!("not an integer: {v:?}"),
    }
}

fn float(v: &ResultValue) -> f64 {
    match v {
        ResultValue::Float(f) => *f,
        v => panic!("not a float: {v:?}"),
    }
}

/// Several components, self-loops, parallel and two-way relationships, two types, a few
/// isolated nodes, some deletes.
fn load<S: GraphStore>(db: &Database<S>) {
    rows(db, "UNWIND range(0, 199) AS i CREATE (:N {id: i})");
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = |m: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % m
    };
    let mut rels = Vec::new();
    for _ in 0..500 {
        // Mostly inside blocks of 40 ids, so there are several components.
        let a = next(190);
        let b = match next(12) {
            0 => a,
            1 => next(190),
            _ => a / 40 * 40 + next(40).min(189 - a / 40 * 40),
        };
        let t = if next(3) == 0 { "B" } else { "A" };
        rels.push(format!("[{a},{b},'{t}']"));
    }
    for t in ["A", "B"] {
        rows(
            db,
            &format!(
                "UNWIND [{}] AS r WITH r WHERE r[2] = '{t}' \
                 MATCH (a:N {{id: r[0]}}), (b:N {{id: r[1]}}) CREATE (a)-[:{t}]->(b)",
                rels.join(",")
            ),
        );
    }
    rows(db, "MATCH (n:N) WHERE n.id % 23 = 4 DETACH DELETE n");
}

/// Nodes (by id) and relationships (start, end, type), from Cypher.
fn edges<S: GraphStore>(db: &Database<S>) -> (Vec<i64>, Vec<(i64, i64, String)>) {
    let nodes = rows(db, "MATCH (n) RETURN id(n) ORDER BY id(n)").iter().map(|r| int(&r[0])).collect();
    let rels = rows(db, "MATCH (a)-[r]->(b) RETURN id(a), id(b), type(r)")
        .iter()
        .map(|r| {
            let ResultValue::String(t) = &r[2] else { panic!() };
            (int(&r[0]), int(&r[1]), t.clone())
        })
        .collect();
    (nodes, rels)
}

/// node id → value, from `CALL proc YIELD node, <field>`.
fn per_node<S: GraphStore>(db: &Database<S>, call: &str, field: &str) -> BTreeMap<i64, ResultValue> {
    rows(db, &format!("{call} YIELD node, {field} RETURN id(node), {field}"))
        .into_iter()
        .map(|mut r| (int(&r[0]), r.remove(1)))
        .collect()
}

/// Hops from each node: (from, to) pairs for `orientation` and `types` (empty = all).
fn hops(rels: &[(i64, i64, String)], types: &[&str], orientation: &str) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    for (a, b, t) in rels {
        if !types.is_empty() && !types.contains(&t.as_str()) {
            continue;
        }
        match orientation {
            "NATURAL" => out.push((*a, *b)),
            "REVERSE" => out.push((*b, *a)),
            _ => {
                out.push((*a, *b));
                if a != b {
                    out.push((*b, *a));
                }
            }
        }
    }
    out
}

fn check<S: GraphStore>(db: &Database<S>) {
    gpu_backend(db);
    load(db);
    let (nodes, rels) = edges(db);

    // PageRank, against the formula iterated over the edge list.
    for (types, orientation, cfg) in [
        (vec![], "NATURAL", "{}".to_string()),
        (vec!["A"], "REVERSE", "{relationshipTypes: 'A', orientation: 'REVERSE', maxIterations: 30}".to_string()),
        (vec!["A", "B"], "UNDIRECTED", "{relationshipTypes: ['A', 'B'], orientation: 'undirected', dampingFactor: 0.5}".to_string()),
    ] {
        let damping = if orientation == "UNDIRECTED" { 0.5 } else { 0.85 };
        let max = if orientation == "REVERSE" { 30 } else { 20 };
        let h = hops(&rels, &types, orientation);
        let mut degree: HashMap<i64, f64> = HashMap::new();
        for (a, _) in &h {
            *degree.entry(*a).or_default() += 1.0;
        }
        let mut score: HashMap<i64, f64> = nodes.iter().map(|n| (*n, 1.0 - damping)).collect();
        for _ in 0..max {
            let mut next: HashMap<i64, f64> = nodes.iter().map(|n| (*n, 0.0)).collect();
            for (a, b) in &h {
                *next.get_mut(b).unwrap() += score[a] / degree[a];
            }
            let moved = nodes.iter().map(|n| ((1.0 - damping + damping * next[n]) - score[n]).abs()).fold(0.0, f64::max);
            for n in &nodes {
                score.insert(*n, 1.0 - damping + damping * next[n]);
            }
            if moved <= 1e-7 {
                break;
            }
        }
        let got = per_node(db, &format!("CALL algo.pageRank({cfg})"), "score");
        assert_eq!(got.len(), nodes.len());
        for (n, s) in &got {
            assert!((float(s) - score[n]).abs() < 1e-9, "{cfg}: node {n}: {s:?} vs {}", score[n]);
        }
    }

    // Weakly connected components: the smallest id reachable over any relationship.
    for (types, cfg) in [(vec![], ""), (vec!["B"], "{relationshipTypes: ['B']}")] {
        let h = hops(&rels, &types, "UNDIRECTED");
        let mut adj: HashMap<i64, Vec<i64>> = HashMap::new();
        for (a, b) in h {
            adj.entry(a).or_default().push(b);
        }
        let got = per_node(db, &format!("CALL algo.wcc({cfg})"), "componentId");
        for &n in &nodes {
            let mut seen = BTreeSet::from([n]);
            let mut queue = VecDeque::from([n]);
            while let Some(x) = queue.pop_front() {
                for &y in adj.get(&x).into_iter().flatten() {
                    if seen.insert(y) {
                        queue.push_back(y);
                    }
                }
            }
            assert_eq!(int(&got[&n]), *seen.first().unwrap(), "{cfg}: node {n}");
        }
        let components = rows(db, &format!("CALL algo.wcc({cfg}) YIELD componentId RETURN count(DISTINCT componentId)"));
        assert!(int(&components[0][0]) > 4, "{cfg}: {components:?}");
    }

    // Triangles, against Cypher (distinct node triples, any direction, no self-loops).
    let cypher: BTreeMap<i64, i64> = rows(
        db,
        "MATCH (a)--(b)--(c)--(a) WHERE id(a) < id(b) AND id(b) < id(c) \
         WITH DISTINCT a, b, c UNWIND [a, b, c] AS n RETURN id(n), count(*)",
    )
    .iter()
    .map(|r| (int(&r[0]), int(&r[1])))
    .collect();
    let got = per_node(db, "CALL algo.triangleCount()", "triangleCount");
    for &n in &nodes {
        assert_eq!(int(&got[&n]), cypher.get(&n).copied().unwrap_or(0), "triangles at {n}");
    }
    assert!(cypher.values().sum::<i64>() > 30);

    // BFS: hop distances, against shortestPath.
    let start = nodes[1];
    let got = rows(db, &format!("MATCH (s) WHERE id(s) = {start} CALL algo.bfs(s, {{maxDepth: 3}}) YIELD node, depth RETURN id(node), depth"));
    let want = rows(
        db,
        &format!(
            "MATCH (s), (m) WHERE id(s) = {start} AND m <> s MATCH p = shortestPath((s)-[*..3]->(m)) \
             RETURN id(m), length(p) ORDER BY length(p), id(m)"
        ),
    );
    assert_eq!(got[0], [ResultValue::Int(start), ResultValue::Int(0)]);
    assert_eq!(&got[1..], &want[..]);
    assert!(want.len() > 5);
    let undirected = rows(db, &format!("MATCH (s) WHERE id(s) IN [{start}, {}] WITH collect(s) AS s CALL algo.bfs(s, {{orientation: 'UNDIRECTED', relationshipTypes: 'A'}}) YIELD depth RETURN max(depth), count(*)", nodes[2]));
    assert!(int(&undirected[0][1]) > want.len() as i64);
}

#[test]
fn memory_store() {
    check(&Database::open(StorageMode::InMemory).unwrap());
}

#[test]
fn persistent_store() {
    let dir = TempDir::new("persistent");
    check(&Database::<BoxedStore>::open(StorageMode::persistent(&dir.0)).unwrap());
}

#[test]
fn composes_errors_and_transactions() {
    let db = Database::in_memory();
    rows(&db, "CREATE (a:P {n: 'a'})-[:L]->(b:P {n: 'b'})-[:L]->(c:P {n: 'c'})-[:L]->(a), (b)-[:L]->(:P {n: 'd'})");
    let top = rows(&db, "CALL algo.pageRank() YIELD node, score RETURN node.n ORDER BY score DESC LIMIT 2");
    // b gets all of a's score, a all of c's, c and d half of b's each.
    assert_eq!(top, [[ResultValue::String("b".into())], [ResultValue::String("a".into())]]);
    let listed = rows(&db, "CALL dbms.procedures() YIELD name WHERE name STARTS WITH 'algo.' RETURN collect(name)");
    assert_eq!(
        listed[0][0],
        ResultValue::List(["algo.bfs", "algo.pageRank", "algo.triangleCount", "algo.wcc"].map(|s| ResultValue::String(s.into())).to_vec())
    );
    for (q, msg) in [
        ("CALL algo.wcc({colour: 1})", "unknown setting `colour`"),
        ("CALL algo.pageRank({orientation: 'SIDEWAYS'})", "orientation must be"),
        ("CALL algo.pageRank({dampingFactor: 1.5})", "dampingFactor"),
        ("CALL algo.pageRank({maxIterations: 'many'})", "must be a number"),
        ("CALL algo.bfs(1)", "sourceNode must be a node"),
    ] {
        let e = db.execute(q).unwrap_err();
        assert!(e.to_string().contains(msg), "{q}: {e}");
    }
    // Inside a transaction, the algorithms see its own changes.
    let mut tx = db.begin().unwrap();
    tx.execute("CREATE (:P {n: 'e'})-[:L]->(:P {n: 'f'})").unwrap();
    let r = tx.execute("CALL algo.wcc() YIELD componentId RETURN count(DISTINCT componentId)").unwrap();
    assert_eq!(r.rows[0][0], ResultValue::Int(2));
    tx.rollback();
    assert_eq!(rows(&db, "CALL algo.wcc() YIELD componentId RETURN count(DISTINCT componentId)")[0][0], ResultValue::Int(1));
    // An expired time limit stops the call.
    let opts = graphbuff::QueryOptions::default().with_timeout(std::time::Duration::ZERO);
    let e = db.execute_with_options("CALL algo.triangleCount()", &Default::default(), &opts).unwrap_err();
    assert!(matches!(e, Error::Timeout(_)), "{e}");
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
