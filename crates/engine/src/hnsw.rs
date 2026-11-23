//! Approximate nearest neighbours: a hierarchical navigable small world graph (HNSW;
//! Malkov and Yashunin, 2016) over the vectors of an approximate vector index.
//!
//! Every vector lives in a slot and is a node of the bottom layer; a node is also on each
//! layer above with probability `1/m` per layer. A search descends greedily from the
//! entry point through the upper layers, then explores the bottom layer best first,
//! keeping the `ef` closest nodes found. An insertion searches the same way and links the
//! new node to neighbours chosen by the paper's heuristic (a candidate is kept only if it
//! is closer to the new node than to every neighbour kept so far), pruning the
//! neighbours' lists back to size with the same heuristic.
//!
//! Removal marks a slot deleted: it still routes searches but is never a result. The
//! owner rebuilds the graph once deleted slots outnumber live ones.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

/// How distances are measured: lower is closer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Metric {
    /// `1 - a·b` of unit vectors (cosine).
    Cosine,
    /// The squared euclidean distance.
    Euclidean,
}

/// The sum of `f` over the pairs of `a` and `b`, in eight separate sums (which the
/// compiler can keep in one vector register; a single running sum it may not reorder).
#[inline]
fn lanes(a: &[f32], b: &[f32], f: impl Fn(f32, f32) -> f32) -> f32 {
    let mut acc = [0.0f32; 8];
    let ((ca, ra), (cb, rb)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..8 {
            acc[i] += f(x[i], y[i]);
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        sum += f(*x, *y);
    }
    sum
}

/// The highest layer a node is put on.
const MAX_LAYER: usize = 16;

/// A node and its distance to what is searched for, ordered by distance (then slot).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Near(f32, u32);

impl Eq for Near {}

impl Ord for Near {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

impl PartialOrd for Near {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The slots seen by one search.
struct Visited(Vec<u64>);

impl Visited {
    fn new(n: usize) -> Visited {
        Visited(vec![0; n.div_ceil(64)])
    }

    /// Marks `i`; whether it was not marked before.
    fn insert(&mut self, i: u32) -> bool {
        let (w, b) = (i as usize / 64, 1u64 << (i % 64));
        let new = self.0[w] & b == 0;
        self.0[w] |= b;
        new
    }
}

#[derive(Clone)]
pub(crate) struct Graph {
    dims: usize,
    metric: Metric,
    /// Neighbours per node on the upper layers; twice that on the bottom one.
    m: usize,
    ef_construction: usize,
    /// `1 / ln(m)`: a node's layer is `floor(-ln(u) * level_mult)` for `u` uniform in (0, 1].
    level_mult: f64,
    /// The vectors, `dims` values per slot.
    vectors: Vec<f32>,
    /// The neighbours of each slot, per layer from the bottom.
    links: Vec<Vec<Vec<u32>>>,
    deleted: Vec<bool>,
    removed: usize,
    /// The node searches start from, on the top layer.
    entry: Option<u32>,
    top: usize,
    rng: u64,
}

impl Graph {
    pub fn new(dims: usize, metric: Metric, m: usize, ef_construction: usize) -> Graph {
        let m = m.max(2);
        Graph {
            dims,
            metric,
            m,
            ef_construction: ef_construction.max(1),
            level_mult: 1.0 / (m as f64).ln(),
            vectors: Vec::new(),
            links: Vec::new(),
            deleted: Vec::new(),
            removed: 0,
            entry: None,
            top: 0,
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// Slots in use, deleted or not.
    pub fn slots(&self) -> usize {
        self.deleted.len()
    }

    pub fn removed(&self) -> usize {
        self.removed
    }

    pub fn live(&self) -> usize {
        self.slots() - self.removed
    }

    pub fn vector(&self, slot: u32) -> &[f32] {
        let i = slot as usize * self.dims;
        &self.vectors[i..i + self.dims]
    }

    fn distance(&self, a: &[f32], b: &[f32]) -> f32 {
        match self.metric {
            Metric::Cosine => 1.0 - lanes(a, b, |x, y| x * y),
            Metric::Euclidean => lanes(a, b, |x, y| (x - y) * (x - y)),
        }
    }

    fn to(&self, q: &[f32], slot: u32) -> f32 {
        self.distance(q, self.vector(slot))
    }

    /// A random layer: 0 with probability `1 - 1/m`, each further one `1/m` as likely.
    fn random_layer(&mut self) -> usize {
        // splitmix64
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let u = ((z >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
        ((-u.ln() * self.level_mult) as usize).min(MAX_LAYER)
    }

    /// Follows the closest neighbour on `layer` from `from` while that gets closer to `q`.
    fn greedy(&self, q: &[f32], mut from: Near, layer: usize) -> Near {
        loop {
            let mut moved = false;
            for &n in &self.links[from.1 as usize][layer] {
                let d = self.to(q, n);
                if d < from.0 {
                    from = Near(d, n);
                    moved = true;
                }
            }
            if !moved {
                return from;
            }
        }
    }

    /// The (up to) `ef` nodes of `layer` that pass `accept` and are closest to `q`, found
    /// from `entries`, closest first. The others are explored but not returned. Gives up
    /// (`None`) after measuring `budget` distances.
    fn search_layer(
        &self,
        q: &[f32],
        entries: &[Near],
        ef: usize,
        layer: usize,
        accept: impl Fn(u32) -> bool,
        mut budget: usize,
    ) -> Option<Vec<Near>> {
        let mut visited = Visited::new(self.slots());
        let mut candidates: BinaryHeap<Reverse<Near>> = BinaryHeap::new();
        let mut found: BinaryHeap<Near> = BinaryHeap::new();
        for &e in entries {
            if visited.insert(e.1) {
                candidates.push(Reverse(e));
                if accept(e.1) {
                    found.push(e);
                }
            }
        }
        while found.len() > ef {
            found.pop();
        }
        while let Some(Reverse(c)) = candidates.pop() {
            if found.len() >= ef && found.peek().is_some_and(|f| c.0 > f.0) {
                break;
            }
            for &n in &self.links[c.1 as usize][layer] {
                if !visited.insert(n) {
                    continue;
                }
                budget = budget.checked_sub(1)?;
                let d = self.to(q, n);
                if found.len() < ef || found.peek().is_some_and(|f| d < f.0) {
                    candidates.push(Reverse(Near(d, n)));
                    if accept(n) {
                        found.push(Near(d, n));
                        if found.len() > ef {
                            found.pop();
                        }
                    }
                }
            }
        }
        Some(found.into_sorted_vec())
    }

    /// Up to `m` of `candidates` (closest first) as neighbours: each one kept is closer to
    /// the node than to any kept before it, which spreads the links in all directions.
    fn select(&self, candidates: &[Near], m: usize) -> Vec<u32> {
        let mut kept: Vec<u32> = Vec::with_capacity(m);
        for c in candidates {
            if kept.len() == m {
                break;
            }
            let v = self.vector(c.1);
            if kept.iter().all(|&k| self.distance(v, self.vector(k)) > c.0) {
                kept.push(c.1);
            }
        }
        kept
    }

    /// Adds `v` (`dims` values, unit length for cosine), returning its slot.
    pub fn insert(&mut self, v: &[f32]) -> u32 {
        debug_assert_eq!(v.len(), self.dims);
        let slot = self.slots() as u32;
        let layer = self.random_layer();
        self.vectors.extend_from_slice(v);
        self.deleted.push(false);
        self.links.push(vec![Vec::new(); layer + 1]);
        let Some(entry) = self.entry else {
            self.entry = Some(slot);
            self.top = layer;
            return slot;
        };
        let mut from = Near(self.to(v, entry), entry);
        for l in (layer + 1..=self.top).rev() {
            from = self.greedy(v, from, l);
        }
        let mut entries = vec![from];
        for l in (0..=layer.min(self.top)).rev() {
            let found = self
                .search_layer(v, &entries, self.ef_construction, l, |_| true, usize::MAX)
                .expect("no budget");
            let neighbours = self.select(&found, self.m);
            let max = if l == 0 { 2 * self.m } else { self.m };
            for &n in &neighbours {
                self.links[n as usize][l].push(slot);
                if self.links[n as usize][l].len() > max {
                    self.prune(n, l, max);
                }
            }
            self.links[slot as usize][l] = neighbours;
            entries = found;
        }
        if layer > self.top {
            self.entry = Some(slot);
            self.top = layer;
        }
        slot
    }

    /// Cuts the neighbours of `n` on `layer` back to `max`.
    fn prune(&mut self, n: u32, layer: usize, max: usize) {
        let v = self.vector(n);
        let mut candidates: Vec<Near> =
            self.links[n as usize][layer].iter().map(|&c| Near(self.distance(v, self.vector(c)), c)).collect();
        candidates.sort_unstable();
        let kept = self.select(&candidates, max);
        self.links[n as usize][layer] = kept;
    }

    /// Marks `slot` deleted.
    pub fn remove(&mut self, slot: u32) {
        if !std::mem::replace(&mut self.deleted[slot as usize], true) {
            self.removed += 1;
        }
    }

    /// The (up to) `k` live slots closest to `q` that a search keeping `ef` candidates
    /// finds, closest first, with their distances.
    pub fn search(&self, q: &[f32], k: usize, ef: usize) -> Vec<(f32, u32)> {
        self.search_where(q, k, ef, |_| true, usize::MAX).expect("no budget")
    }

    /// As [`search`](Self::search), but only of the slots that pass `accept`: the others
    /// still lead the search on. The fewer pass, the further it goes; it gives up (`None`)
    /// after measuring about `budget` distances.
    pub fn search_where(
        &self,
        q: &[f32],
        k: usize,
        ef: usize,
        accept: impl Fn(u32) -> bool,
        budget: usize,
    ) -> Option<Vec<(f32, u32)>> {
        let Some(entry) = self.entry else { return Some(Vec::new()) };
        let mut from = Near(self.to(q, entry), entry);
        for l in (1..=self.top).rev() {
            from = self.greedy(q, from, l);
        }
        let accept = |slot: u32| !self.deleted[slot as usize] && accept(slot);
        let mut found = self.search_layer(q, &[from], ef.max(k), 0, accept, budget)?;
        found.truncate(k);
        Some(found.into_iter().map(|n| (n.0, n.1)).collect())
    }
}

#[cfg(test)]
impl Graph {
    /// Leaves only the first `keep` slots reachable.
    pub fn cut_off(&mut self, keep: u32) {
        self.links.iter_mut().flatten().for_each(|links| links.retain(|&n| n < keep));
        self.entry = Some(0);
        self.top = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(n: usize, dims: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut s = seed;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        };
        (0..n).map(|_| (0..dims).map(|_| next()).collect()).collect()
    }

    fn unit(v: &[f32]) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }

    /// The share of the true `k` nearest that the graph finds, over `queries`.
    fn recall(g: &Graph, data: &[Vec<f32>], live: &[bool], queries: &[Vec<f32>], k: usize, ef: usize) -> f64 {
        let mut hits = 0;
        for q in queries {
            let mut exact: Vec<(f32, u32)> = data
                .iter()
                .enumerate()
                .filter(|(i, _)| live[*i])
                .map(|(i, v)| (g.distance(q, v), i as u32))
                .collect();
            exact.sort_by(|a, b| a.0.total_cmp(&b.0));
            let truth: Vec<u32> = exact.iter().take(k).map(|e| e.1).collect();
            let got = g.search(q, k, ef);
            assert!(got.iter().all(|(_, s)| live[*s as usize]));
            hits += got.iter().filter(|(_, s)| truth.contains(s)).count();
        }
        hits as f64 / (queries.len() * k) as f64
    }

    #[test]
    fn finds_the_nearest_neighbours() {
        for metric in [Metric::Euclidean, Metric::Cosine] {
            let prep = |v: Vec<f32>| if metric == Metric::Cosine { unit(&v) } else { v };
            let data: Vec<Vec<f32>> = points(3000, 24, 7).into_iter().map(prep).collect();
            let queries: Vec<Vec<f32>> = points(50, 24, 99).into_iter().map(prep).collect();
            let mut g = Graph::new(24, metric, 16, 100);
            for v in &data {
                g.insert(v);
            }
            let r = recall(&g, &data, &vec![true; data.len()], &queries, 10, 100);
            assert!(r >= 0.95, "{metric:?}: recall {r}");
        }
    }

    #[test]
    fn deleted_slots_are_not_found() {
        let data = points(2000, 8, 3);
        let mut g = Graph::new(8, Metric::Euclidean, 8, 64);
        for v in &data {
            g.insert(v);
        }
        let mut live = vec![true; data.len()];
        for i in (0..data.len()).step_by(3) {
            g.remove(i as u32);
            live[i] = false;
        }
        assert_eq!(g.live(), data.len() - data.len().div_ceil(3));
        let queries = points(30, 8, 11);
        let r = recall(&g, &data, &live, &queries, 10, 80);
        assert!(r >= 0.9, "recall {r}");
        // A deleted vector itself is not returned, though it is the closest.
        assert!(g.search(&data[0], 5, 50).iter().all(|(_, s)| *s != 0));
    }

    #[test]
    fn filtered_search() {
        let data = points(3000, 16, 5);
        let mut g = Graph::new(16, Metric::Euclidean, 16, 100);
        for v in &data {
            g.insert(v);
        }
        for i in (0..data.len()).step_by(11) {
            g.remove(i as u32);
        }
        let queries = points(20, 16, 23);
        for every in [2, 5, 20] {
            let passes = |s: u32| s.is_multiple_of(every);
            let live: Vec<bool> = (0..data.len()).map(|i| i % 11 != 0 && passes(i as u32)).collect();
            let mut hits = 0;
            for q in &queries {
                let mut exact: Vec<(f32, u32)> =
                    (0..data.len()).filter(|&i| live[i]).map(|i| (g.distance(q, &data[i]), i as u32)).collect();
                exact.sort_by(|a, b| a.0.total_cmp(&b.0));
                let got = g.search_where(q, 10, 100, passes, usize::MAX).expect("no budget");
                assert_eq!(got.len(), 10);
                assert!(got.iter().all(|(_, s)| live[*s as usize]));
                hits += got.iter().filter(|(_, s)| exact[..10].iter().any(|e| e.1 == *s)).count();
            }
            assert!(hits >= 190, "1 in {every}: {hits} of 200");
        }
        // It gives up when it takes too long, and finds nothing if nothing passes.
        assert!(g.search_where(&queries[0], 10, 100, |_| true, 50).is_none());
        assert_eq!(g.search_where(&queries[0], 10, 100, |_| false, usize::MAX), Some(vec![]));
    }

    #[test]
    fn empty_and_tiny_graphs() {
        let mut g = Graph::new(2, Metric::Euclidean, 16, 100);
        assert!(g.search(&[0.0, 0.0], 3, 10).is_empty());
        g.insert(&[1.0, 0.0]);
        g.insert(&[0.0, 1.0]);
        let r = g.search(&[0.9, 0.0], 5, 10);
        assert_eq!(r.iter().map(|x| x.1).collect::<Vec<_>>(), [0, 1]);
    }
}
