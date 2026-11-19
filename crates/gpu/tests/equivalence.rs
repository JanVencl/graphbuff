//! `WgpuKernels` against `CpuKernels`: every GPU kernel must give the CPU's result exactly
//! (PageRank bit for bit). Skipped, with a note, on machines without a usable GPU.

use std::collections::BTreeMap;

use graphbuff_core::{
    Bits, CpuKernels, Csr, Direction, GraphStore, Hop, Kernels, MemoryGraph, NodeId, VectorRows, VectorSimilarity,
};
use graphbuff_gpu::WgpuKernels;

fn gpu() -> Option<WgpuKernels> {
    match WgpuKernels::new() {
        Ok(k) => {
            eprintln!("GPU: {} (f64: {})", k.adapter(), k.has_f64());
            Some(k.with_min_work(0))
        }
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// `nodes` nodes and `rels` relationships of three types, skewed towards low node ids
/// (so some nodes are dense), with self-loops, parallel and two-way relationships, and
/// some deleted nodes (gaps in the ids).
fn graph(seed: u64, nodes: usize, rels: usize) -> Csr {
    let mut rng = Rng(seed);
    let mut g = MemoryGraph::new();
    let ids: Vec<NodeId> = (0..nodes).map(|_| g.create_node(&[], BTreeMap::new()).unwrap()).collect();
    let pick = |rng: &mut Rng| {
        let a = rng.below(nodes as u64);
        let b = rng.below(nodes as u64);
        (if rng.below(4) == 0 { a.min(b) / 8 } else { a }) as usize
    };
    for k in 0..rels {
        let a = pick(&mut rng);
        let b = match rng.below(20) {
            0 => a,
            _ => pick(&mut rng),
        };
        let ty = ["A", "B", "C"][rng.below(3) as usize];
        g.create_relationship(ids[a], ids[b], ty, BTreeMap::new()).unwrap();
        if k % 50 == 0 {
            g.create_relationship(ids[a], ids[b], ty, BTreeMap::new()).unwrap();
            g.create_relationship(ids[b], ids[a], ty, BTreeMap::new()).unwrap();
        }
    }
    for k in (nodes / 3..nodes).step_by(97) {
        g.delete_node(ids[k], true).unwrap();
    }
    Csr::build(&g).unwrap()
}

fn type_sets(csr: &Csr) -> Vec<Vec<u32>> {
    let all = csr.type_ids(&[]);
    let k = all.len().min(1);
    vec![all.clone(), all[..k].to_vec(), all[k..].to_vec(), vec![]]
}

const DIRS: [Direction; 3] = [Direction::Outgoing, Direction::Incoming, Direction::Both];

#[test]
fn graph_kernels_match_the_cpu() {
    let Some(gpu) = gpu() else { return };
    let cpu = CpuKernels::new();
    for (seed, nodes, rels) in [(1, 1, 0), (2, 7, 30), (3, 2_000, 9_000), (4, 70_000, 400_000)] {
        let csr = graph(seed, nodes, rels);
        let n = csr.node_count();
        let mut rng = Rng(seed * 31 + 7);
        for types in type_sets(&csr) {
            for dir in DIRS {
                // Walk counts over three hops, from all nodes and from some.
                for start in [vec![1u64; n], (0..n).map(|_| rng.below(3)).collect()] {
                    let (mut a, mut b) = (start.clone(), start);
                    for _ in 0..3 {
                        a = cpu.propagate(&csr, &a, dir, &types).unwrap();
                        b = gpu.propagate(&csr, &b, dir, &types).unwrap();
                        assert_eq!(a, b, "propagate {dir:?} {types:?} on {n} nodes");
                    }
                }
                // Several hops at once, with masks between them.
                let masks: Vec<Bits> = (0..3)
                    .map(|_| {
                        let mut m = Bits::new(n);
                        for x in 0..n as u32 {
                            m.set(x, rng.below(4) != 0);
                        }
                        m
                    })
                    .collect();
                let start: Vec<u64> = (0..n).map(|_| rng.below(2)).collect();
                let hops: Vec<Hop> = [Direction::Outgoing, dir, Direction::Both]
                    .iter()
                    .zip(&masks)
                    .enumerate()
                    .map(|(h, (&dir, m))| Hop { dir, types: &types, mask: (h != 1).then_some(m) })
                    .collect();
                assert_eq!(cpu.walk(&csr, &start, &hops).unwrap(), gpu.walk(&csr, &start, &hops).unwrap(), "walk");
                let sources: Vec<u32> = (0..3.min(n)).map(|_| rng.below(n as u64) as u32).collect();
                for depth in [0, 1, 3, u32::MAX] {
                    assert_eq!(
                        cpu.bfs(&csr, &sources, dir, &types, depth),
                        gpu.bfs(&csr, &sources, dir, &types, depth),
                        "bfs {dir:?} {types:?} depth {depth}"
                    );
                }
                for (damping, iterations, tolerance) in [(0.85, 20, 1e-7), (0.5, 3, 0.0)] {
                    let (a, ia) = cpu.page_rank(&csr, &types, dir, damping, iterations, tolerance);
                    let (b, ib) = gpu.page_rank(&csr, &types, dir, damping, iterations, tolerance);
                    assert_eq!(ia, ib, "page rank iterations {dir:?} {types:?}");
                    let (a, b): (Vec<u64>, Vec<u64>) =
                        (a.iter().map(|x| x.to_bits()).collect(), b.iter().map(|x| x.to_bits()).collect());
                    assert!(a == b, "page rank scores {dir:?} {types:?} on {n} nodes");
                }
            }
            assert_eq!(cpu.wcc(&csr, &types), gpu.wcc(&csr, &types), "wcc {types:?}");
            assert_eq!(cpu.triangles(&csr, &types), gpu.triangles(&csr, &types), "triangles {types:?}");
        }
    }
    assert!(gpu.gpu_runs() > 100, "kernels ran on the GPU: {}", gpu.gpu_runs());
    assert_eq!(gpu.error(), None);
}

#[test]
fn walk_count_overflow_is_unsupported() {
    let Some(gpu) = gpu() else { return };
    let csr = graph(5, 50, 400);
    let all = csr.type_ids(&[]);
    let mut counts = vec![0u64; csr.node_count()];
    // A node reached from two others with counts that add up past 2^64.
    let (j, a) = (0..csr.node_count() as u32)
        .find_map(|j| {
            let nb = csr.adjacency(all[0], Direction::Incoming).neighbors(j);
            (nb.len() >= 2 && nb[0] != j && nb[1] != j && nb[0] != nb[1]).then(|| (j, [nb[0], nb[1]]))
        })
        .expect("a node with two distinct in-neighbours");
    let _ = j;
    counts[a[0] as usize] = u64::MAX / 2 + 1;
    counts[a[1] as usize] = u64::MAX / 2 + 1;
    assert!(CpuKernels::new().propagate(&csr, &counts, Direction::Outgoing, &all[..1]).is_err());
    assert!(gpu.propagate(&csr, &counts, Direction::Outgoing, &all[..1]).is_err());
    // Carries between the halves, without overflow.
    counts[a[0] as usize] = u64::from(u32::MAX);
    counts[a[1] as usize] = 1 << 40;
    assert_eq!(
        CpuKernels::new().propagate(&csr, &counts, Direction::Outgoing, &all[..1]).unwrap(),
        gpu.propagate(&csr, &counts, Direction::Outgoing, &all[..1]).unwrap()
    );
}

#[test]
fn small_work_stays_on_the_cpu() {
    let Some(gpu) = gpu() else { return };
    let gpu = gpu.with_min_work(1_000_000);
    let csr = graph(6, 100, 500);
    let all = csr.type_ids(&[]);
    gpu.wcc(&csr, &all);
    gpu.propagate(&csr, &vec![1; csr.node_count()], Direction::Both, &all).unwrap();
    assert_eq!(gpu.gpu_runs(), 0);
}

/// Random rows of `dims` values in [-1, 1) (unit length with `unit`).
fn vector_rows(rng: &mut Rng, n: usize, dims: usize, unit: bool, scale: f64) -> (VectorRows, Vec<Vec<f64>>) {
    let mut exact = Vec::new();
    let mut data = Vec::new();
    for _ in 0..n {
        let mut v: Vec<f64> = (0..dims).map(|_| (rng.below(2001) as f64 / 1000.0 - 1.0) * scale).collect();
        if unit {
            let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt().max(1e-9);
            v.iter_mut().for_each(|x| *x /= norm);
        }
        data.extend(v.iter().map(|x| *x as f32));
        exact.push(v);
    }
    (VectorRows::new(dims, data), exact)
}

#[test]
fn vector_scores_are_within_their_bound() {
    let Some(gpu) = gpu() else { return };
    let cpu = CpuKernels::new();
    let mut rng = Rng(0x9e3779b97f4a7c15);
    for (dims, scale) in [(3, 1.0), (64, 1.0), (768, 1.0), (128, 1000.0)] {
        for similarity in [VectorSimilarity::Cosine, VectorSimilarity::Euclidean] {
            let unit = similarity == VectorSimilarity::Cosine;
            let (rows, exact) = vector_rows(&mut rng, 3000, dims, unit, scale);
            let (_, q) = vector_rows(&mut rng, 1, dims, unit, scale);
            let q = &q[0];
            let qn = q.iter().map(|x| x * x).sum::<f64>().sqrt();
            let before = gpu.gpu_runs();
            let on_gpu = gpu.vector_scores(&rows, q, similarity);
            assert_eq!(gpu.gpu_runs(), before + 1, "ran on the GPU");
            assert_eq!(on_gpu.f32_terms, dims as u32);
            let on_cpu = cpu.vector_scores(&rows, q, similarity);
            assert_eq!(on_cpu.f32_terms, 0);
            let (gb, cb) = (
                on_gpu.error_bound(similarity, rows.max_norm(), qn),
                on_cpu.error_bound(similarity, rows.max_norm(), qn),
            );
            for (i, row) in exact.iter().enumerate() {
                // In exact arithmetic, on the unrounded vectors.
                let truth: f64 = match similarity {
                    VectorSimilarity::Cosine => row.iter().zip(q).map(|(a, b)| a * b).sum(),
                    VectorSimilarity::Euclidean => row.iter().zip(q).map(|(a, b)| (a - b).powi(2)).sum(),
                };
                let what = format!("{similarity:?}, {dims} dims, scale {scale}, row {i}");
                assert!((on_gpu.values[i] - truth).abs() <= gb, "{what}: GPU {} vs {truth} (bound {gb})", on_gpu.values[i]);
                assert!((on_cpu.values[i] - truth).abs() <= cb, "{what}: CPU {} vs {truth} (bound {cb})", on_cpu.values[i]);
            }
        }
    }
    // Squared distances that f32 cannot hold stay on the CPU.
    let (rows, _) = vector_rows(&mut rng, 10, 4, false, 1e20);
    let before = gpu.gpu_runs();
    assert_eq!(gpu.vector_scores(&rows, &[0.0; 4], VectorSimilarity::Euclidean).f32_terms, 0);
    assert_eq!(gpu.gpu_runs(), before);
}
