//! Kernels over the CSR export give the same answers as the Cypher engine.

use std::collections::{BTreeMap, BTreeSet};

use graphbuff::{
    Aggregates, CmpOp, Column, CpuKernels, Csr, Database, Direction, GraphStore, Kernels, Mask, MemoryGraph, NodeId,
    ResultValue, Value, label_mask,
};

const NODES: usize = 20_000;

fn rng(seed: &mut u64, m: u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed % m
}

/// Nodes with `id`, an integer or float (or no) `x`, an integer `y` and label `L` on some;
/// `:A` relationships without self-loops, `:B` with them.
fn graph() -> Database<MemoryGraph> {
    let mut g = MemoryGraph::new();
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    g.begin().unwrap();
    let mut nodes = Vec::with_capacity(NODES);
    for i in 0..NODES {
        let mut props = BTreeMap::from([("id".to_string(), Value::Int(i as i64))]);
        match rng(&mut seed, 5) {
            0 => {}
            1 | 2 => {
                props.insert("x".into(), Value::Int(rng(&mut seed, 200) as i64 - 100));
            }
            _ => {
                props.insert("x".into(), Value::Float(rng(&mut seed, 2000) as f64 / 7.0 - 140.0));
            }
        }
        props.insert("y".into(), Value::Int(rng(&mut seed, 1000) as i64));
        let labels: Vec<String> = if rng(&mut seed, 3) == 0 { vec!["L".into()] } else { vec![] };
        nodes.push(g.create_node(&labels, props).unwrap());
    }
    for _ in 0..3 * NODES {
        let a = nodes[rng(&mut seed, NODES as u64) as usize];
        let mut b = nodes[rng(&mut seed, NODES as u64) as usize];
        if b == a {
            b = nodes[(a.0 as usize + 1) % NODES];
        }
        g.create_relationship(a, b, "A", BTreeMap::new()).unwrap();
        if rng(&mut seed, 4) == 0 {
            let b = if rng(&mut seed, 5) == 0 { a } else { b };
            g.create_relationship(b, a, "B", BTreeMap::new()).unwrap();
        }
    }
    g.commit().unwrap();
    Database::with_store(g)
}

fn rows(db: &Database<MemoryGraph>, q: &str) -> Vec<Vec<ResultValue>> {
    db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}")).rows
}

fn value(v: &Value) -> ResultValue {
    match *v {
        Value::Null => ResultValue::Null,
        Value::Int(i) => ResultValue::Int(i),
        Value::Float(f) => ResultValue::Float(f),
        ref o => panic!("unexpected aggregate {o:?}"),
    }
}

fn aggregates(a: &Aggregates) -> Vec<ResultValue> {
    vec![ResultValue::Int(a.rows), ResultValue::Int(a.count), value(&a.sum), value(&a.avg), value(&a.min), value(&a.max)]
}

fn op_text(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "<>",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}

struct Fixture {
    db: Database<MemoryGraph>,
    csr: std::sync::Arc<Csr>,
    x: Column,
    y: Column,
    all: Vec<u32>,
}

fn fixture() -> Fixture {
    let db = graph();
    let csr = db.csr().unwrap();
    let (x, y) = db.read(|s| (Column::build(s, &csr, "x").unwrap(), Column::build(s, &csr, "y").unwrap()));
    let all = (0..csr.node_count() as u32).collect();
    Fixture { db, csr, x, y, all }
}

#[test]
fn expand_matches_the_export() {
    let f = fixture();
    for k in [CpuKernels::with_threads(1), CpuKernels::with_threads(4)] {
        for dir in [Direction::Outgoing, Direction::Incoming, Direction::Both] {
            for types in [vec![], vec!["A".to_string()], vec!["B".to_string(), "C".to_string()]] {
                let ids = f.csr.type_ids(&types);
                // Every node, then a shuffled batch with repeats.
                let mut sources = f.all.clone();
                sources.extend((0..5000u32).map(|i| i.wrapping_mul(7919) % NODES as u32));
                let batch = k.expand(&f.csr, &sources, dir, &ids);
                let mut want = Vec::new();
                for (row, &s) in sources.iter().enumerate() {
                    for e in f.csr.expand(f.csr.node_id(s), dir, &types) {
                        want.push((row as u32, f.csr.node_index(e.other).unwrap(), e.rel));
                    }
                }
                let got: Vec<_> =
                    (0..batch.len()).map(|i| (batch.rows[i], batch.targets[i], batch.rels[i])).collect();
                assert_eq!(got, want, "{dir:?} {types:?}");
            }
        }
    }
}

#[test]
fn filter_and_aggregate_match_cypher() {
    let f = fixture();
    let k = CpuKernels::with_threads(4);
    let label = Mask::from_bits(f.db.read(|s| label_mask(s, &f.csr, "L")).unwrap());
    let consts = [Value::Int(0), Value::Int(-100), Value::Float(3.5), Value::Float(-0.0), Value::from("a")];
    let ops = [CmpOp::Eq, CmpOp::Ne, CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge];
    for c in &consts {
        let lit = match c {
            Value::String(s) => format!("'{s}'"),
            v => v.to_string(),
        };
        for op in ops {
            let m = k.filter(&f.x, op, c);
            let o = op_text(op);
            let cases = [
                (format!("MATCH (n) WHERE n.x {o} {lit}"), m.clone()),
                (format!("MATCH (n) WHERE NOT (n.x {o} {lit})"), m.not()),
                (format!("MATCH (n:L) WHERE n.x {o} {lit}"), label.and(&m)),
                (format!("MATCH (n) WHERE n.x {o} {lit} OR n.y < 100"), m.or(&k.filter(&f.y, CmpOp::Lt, &Value::Int(100)))),
            ];
            for (q, mask) in cases {
                let selected: Vec<u32> = k.select(&f.all, &mask).iter().map(|&p| f.all[p as usize]).collect();
                for col in ["x", "y"] {
                    let q = format!(
                        "{q} RETURN count(*), count(n.{col}), sum(n.{col}), avg(n.{col}), min(n.{col}), max(n.{col})"
                    );
                    let column = if col == "x" { &f.x } else { &f.y };
                    assert_eq!(rows(&f.db, &q)[0], aggregates(&k.aggregate(column, &selected)), "{q}");
                }
            }
        }
    }
}

#[test]
fn walk_counts_match_two_hop_cypher() {
    let f = fixture();
    let k = CpuKernels::with_threads(4);
    let a = f.csr.type_ids(&["A".to_string()]);
    // `:A` has no self-loops, so two-hop walks and paths are the same.
    for (dir, pattern) in [
        (Direction::Outgoing, "(s)-[:A]->()-[:A]->(n)"),
        (Direction::Incoming, "(s)<-[:A]-()<-[:A]-(n)"),
    ] {
        let one = k.propagate(&f.csr, &vec![1; NODES], dir, &a).unwrap();
        let two = k.propagate(&f.csr, &one, dir, &a).unwrap();
        let got = k.aggregate_counts(&f.y, &two).unwrap();
        let q = format!("MATCH {pattern} RETURN count(*), count(n.y), sum(n.y), avg(n.y), min(n.y), max(n.y)");
        let want = &rows(&f.db, &q)[0];
        let mut exact = aggregates(&got);
        // `avg` multiplies instead of adding row by row: compare it approximately.
        let (ResultValue::Float(x), ResultValue::Float(y)) = (&exact[3], &want[3]) else { panic!("avg") };
        assert!((x - y).abs() < 1e-9 * y.abs(), "{x} vs {y}");
        exact[3] = want[3].clone();
        assert_eq!(&exact, want, "{q}");
        // The same through row batches: expand twice, then aggregate the end nodes.
        let b1 = k.expand(&f.csr, &f.all, dir, &a);
        let b2 = k.expand(&f.csr, &b1.targets, dir, &a);
        let rows_form = k.aggregate(&f.y, &b2.targets);
        assert_eq!((rows_form.rows, &rows_form.sum, &rows_form.min, &rows_form.max), (got.rows, &got.sum, &got.min, &got.max));
    }
}

#[test]
fn top_n_matches_order_by_limit() {
    let f = fixture();
    let k = CpuKernels::new();
    for (desc, n) in [(false, 1), (false, 37), (true, 37), (true, 5000), (false, NODES + 1)] {
        let q = format!("MATCH (n) RETURN n.id ORDER BY n.x {} LIMIT {n}", if desc { "DESC" } else { "" });
        let want: Vec<i64> = rows(&f.db, &q)
            .iter()
            .map(|r| match r[0] {
                ResultValue::Int(i) => i,
                ref v => panic!("{v:?}"),
            })
            .collect();
        let got: Vec<i64> = k
            .top_n(&f.x, &f.all, n, desc)
            .into_iter()
            .map(|p| f.db.read(|s| s.node_property(f.csr.node_id(f.all[p]), "id").unwrap().unwrap()))
            .map(|v| v.as_int().unwrap())
            .collect();
        assert_eq!(got, want, "{q}");
    }
}

#[test]
fn bfs_matches_variable_length_reach() {
    let f = fixture();
    let k = CpuKernels::new();
    let both = f.csr.type_ids(&[]);
    for (start, dir, arrow) in [(0u32, Direction::Outgoing, "->"), (17, Direction::Both, "-")] {
        let depth = k.bfs(&f.csr, &[start], dir, &both, 3);
        let got: BTreeSet<NodeId> = (0..NODES as u32)
            .filter(|&i| i != start && depth[i as usize] <= 3)
            .map(|i| f.csr.node_id(i))
            .collect();
        let q = format!(
            "MATCH (s)-[*1..3]{}(m) WHERE id(s) = {} AND m <> s RETURN DISTINCT id(m)",
            arrow,
            f.csr.node_id(start).0
        );
        let want: BTreeSet<NodeId> = rows(&f.db, &q)
            .iter()
            .map(|r| match r[0] {
                ResultValue::Int(i) => NodeId(i as u64),
                ref v => panic!("{v:?}"),
            })
            .collect();
        assert_eq!(got, want, "{q}");
    }
}
