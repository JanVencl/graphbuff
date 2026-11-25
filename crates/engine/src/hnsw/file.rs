//! Keeping an HNSW graph in a file, so that it need not be built again when the database
//! is opened.
//!
//! The file holds the links, and per slot the entity and a hash of its vector, but not the
//! vectors: those are read from the store anyway, which is also what makes a saved graph
//! safe to use. Whatever happened since it was saved (commits that were lost in a crash or
//! replayed from a log, a restored backup, a version of graphbuff that does not know the
//! file), [`Graph::load`] gives a graph of exactly the vectors passed in: slots whose
//! vector is the same keep their links, and the others are taken out of the graph. Only
//! deleted slots, which still route searches, have their vectors in the file.
//!
//! ```text
//! "GBHNSW01" | key | dims, m | rng | entry | top | slots
//! per slot: entity (u64::MAX: deleted) | hash of its vector, or the vector of a deleted one
//! per slot: layers u8, per layer: count u32, slots u32…
//! checksum u64
//! ```
//!
//! `key` says what index the graph is of. Numbers are little-endian. A file that is not
//! exactly as expected is ignored.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use super::{Graph, MAX_LAYER, Near};

const MAGIC: &[u8; 8] = b"GBHNSW01";

/// The entity of a deleted slot.
const DELETED: u64 = u64::MAX;

/// A 64-bit hash of `bytes`, taken eight at a time. Each step is a bijection of the state,
/// so a change within eight bytes always changes the hash.
#[derive(Clone, Copy)]
struct Hash(u64);

impl Hash {
    fn new() -> Hash {
        Hash(0x9E37_79B9_7F4A_7C15)
    }

    fn word(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517C_C1B7_2722_0A95);
    }

    /// Adds `bytes`; only the last call may have a length that is no multiple of eight.
    fn bytes(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for w in words {
            self.word(u64::from_le_bytes(*w));
        }
        if !rest.is_empty() {
            let mut w = [0; 8];
            w[..rest.len()].copy_from_slice(rest);
            self.word(u64::from_le_bytes(w));
        }
    }

    fn finish(mut self, len: u64) -> u64 {
        self.word(len);
        self.0 ^ (self.0 >> 32)
    }
}

/// A hash of a vector.
fn vector_hash(v: &[f32]) -> u64 {
    let mut h = Hash::new();
    let (pairs, rest) = v.as_chunks::<2>();
    for [a, b] in pairs {
        h.word(u64::from(a.to_bits()) << 32 | u64::from(b.to_bits()));
    }
    for x in rest {
        h.word(u64::from(x.to_bits()));
    }
    h.finish(v.len() as u64)
}

/// Writes to a file through a buffer, hashing what it writes.
struct Writer {
    file: File,
    buf: Vec<u8>,
    hash: Hash,
    len: u64,
}

impl Writer {
    /// A multiple of eight.
    const BUFFER: usize = 1 << 16;

    fn bytes(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let n = bytes.len().min(Self::BUFFER - self.buf.len());
            self.buf.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.buf.len() == Self::BUFFER {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.hash.bytes(&self.buf);
        self.len += self.buf.len() as u64;
        self.file.write_all(&self.buf)?;
        self.buf.clear();
        Ok(())
    }

    fn u32(&mut self, v: u32) -> io::Result<()> {
        self.bytes(&v.to_le_bytes())
    }

    fn u64(&mut self, v: u64) -> io::Result<()> {
        self.bytes(&v.to_le_bytes())
    }
}

/// Reads values from the bytes of a file; `None` at its end.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }

    /// A count of things of at least `size` bytes each, all of which must still be there.
    fn count(&mut self, n: u64, size: usize) -> Option<usize> {
        let n = usize::try_from(n).ok()?;
        (n.checked_mul(size)? <= self.0.len()).then_some(n)
    }
}

/// A graph read from a file, of the vectors passed to [`Graph::load`].
pub(crate) struct Loaded {
    pub graph: Graph,
    /// The entity of each slot, and whether the slot is live.
    pub ids: Vec<u64>,
    /// Live slots when it was saved that had to be taken out: their vectors changed, or
    /// their entities are gone.
    pub dropped: usize,
}

impl Graph {
    /// Writes the graph, whose slots are of the entities `ids`, into `path`. `key` says
    /// what index it is of.
    pub fn save(&self, path: &Path, key: &[u8], ids: &[u64]) -> io::Result<()> {
        // Written in full before it takes the place of the file before it. It need not be
        // on disk for certain: a file cut short or damaged is ignored.
        let tmp = path.with_extension("tmp");
        let written = self.write(&tmp, key, ids).and_then(|()| std::fs::rename(&tmp, path));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    }

    fn write(&self, path: &Path, key: &[u8], ids: &[u64]) -> io::Result<()> {
        debug_assert_eq!(ids.len(), self.slots());
        let mut w =
            Writer { file: File::create(path)?, buf: Vec::with_capacity(Writer::BUFFER), hash: Hash::new(), len: 0 };
        w.bytes(MAGIC)?;
        w.u32(key.len() as u32)?;
        w.bytes(key)?;
        w.u32(self.dims as u32)?;
        w.u32(self.m as u32)?;
        w.u64(self.rng)?;
        w.u32(self.entry.unwrap_or(u32::MAX))?;
        w.u32(self.top as u32)?;
        w.u32(self.slots() as u32)?;
        for (slot, &id) in ids.iter().enumerate() {
            let v = self.vector(slot as u32);
            if self.deleted[slot] {
                w.u64(DELETED)?;
                for x in v {
                    w.u32(x.to_bits())?;
                }
            } else {
                w.u64(id)?;
                w.u64(vector_hash(v))?;
            }
        }
        for layers in &self.links {
            w.bytes(&[layers.len() as u8])?;
            for links in layers {
                w.u32(links.len() as u32)?;
                for &n in links {
                    w.u32(n)?;
                }
            }
        }
        w.flush()?;
        let checksum = w.hash.finish(w.len);
        w.file.write_all(&checksum.to_le_bytes())
    }

    /// The graph in the file `path`, of `vectors` (by entity): entities of the file whose
    /// vectors are among them, unchanged. `None` if there is no such file, or it is not
    /// of `key`, or it is damaged.
    pub fn load<'v>(
        path: &Path,
        key: &[u8],
        empty: Graph,
        vectors: impl Fn(u64) -> Option<&'v [f32]>,
    ) -> Option<Loaded> {
        let bytes = std::fs::read(path).ok()?;
        let (content, checksum) = bytes.split_at_checked(bytes.len().checked_sub(8)?)?;
        let mut hash = Hash::new();
        hash.bytes(content);
        if hash.finish(content.len() as u64).to_le_bytes() != checksum {
            return None;
        }
        read(&mut Reader(content), key, empty, vectors)
    }

    /// Takes the slots `gone`, whose vectors are not known, out of the graph: no search
    /// comes by them any more. Nodes linked to one are linked to its neighbours instead.
    fn take_out(&mut self, gone: &[bool]) {
        for slot in 0..self.slots() {
            if gone[slot] {
                continue;
            }
            for layer in 0..self.links[slot].len() {
                if !self.links[slot][layer].iter().any(|&n| gone[n as usize]) {
                    continue;
                }
                let v = self.vector(slot as u32);
                let mut near: Vec<Near> = self.links[slot][layer]
                    .iter()
                    .flat_map(|n| if gone[*n as usize] { &self.links[*n as usize][layer][..] } else { std::slice::from_ref(n) })
                    .filter(|&&n| !gone[n as usize] && n as usize != slot)
                    .map(|&n| Near(self.distance(v, self.vector(n)), n))
                    .collect();
                near.sort_unstable();
                near.dedup();
                // All of them if they fit, then the well spread ones and the closest others:
                // one of them may be the only way to its part of the graph.
                let max = if layer == 0 { 2 * self.m } else { self.m };
                let mut kept = if near.len() > max { self.select(&near, max) } else { Vec::new() };
                for n in &near {
                    if kept.len() < max && !kept.contains(&n.1) {
                        kept.push(n.1);
                    }
                }
                self.links[slot][layer] = kept;
            }
        }
        for (slot, _) in gone.iter().enumerate().filter(|(_, g)| **g) {
            self.links[slot].iter_mut().for_each(Vec::clear);
            self.remove(slot as u32);
        }
        if self.entry.is_some_and(|e| gone[e as usize]) {
            // The highest node that is left.
            let best = (0..self.slots()).filter(|&s| !gone[s]).max_by_key(|&s| self.links[s].len());
            self.entry = best.map(|s| s as u32);
            self.top = best.map_or(0, |s| self.links[s].len() - 1);
        }
    }
}

fn read<'v>(r: &mut Reader<'_>, key: &[u8], mut g: Graph, vectors: impl Fn(u64) -> Option<&'v [f32]>) -> Option<Loaded> {
    if r.bytes(8)? != MAGIC {
        return None;
    }
    let len = r.u32()?;
    let len = r.count(u64::from(len), 1)?;
    if r.bytes(len)? != key || r.u32()? as usize != g.dims || r.u32()? as usize != g.m {
        return None;
    }
    g.rng = r.u64()?;
    let entry = r.u32()?;
    g.top = r.u32()? as usize;
    let slots = r.u32()?;
    let slots = r.count(u64::from(slots), 16)?;

    let mut ids = Vec::with_capacity(slots);
    let mut seen = HashSet::with_capacity(slots);
    let mut gone = vec![false; slots];
    for (slot, gone) in gone.iter_mut().enumerate() {
        let id = r.u64()?;
        let deleted = id == DELETED;
        ids.push(id);
        g.deleted.push(deleted);
        if deleted {
            g.removed += 1;
            for _ in 0..g.dims {
                let x = f32::from_bits(r.u32()?);
                g.vectors.push(x);
            }
            continue;
        }
        if !seen.insert(id) {
            return None;
        }
        let hash = r.u64()?;
        match vectors(id) {
            Some(v) if v.len() == g.dims && vector_hash(v) == hash => g.vectors.extend_from_slice(v),
            _ => {
                *gone = true;
                g.vectors.resize((slot + 1) * g.dims, 0.0);
            }
        }
    }
    for _ in 0..slots {
        let layers = usize::from(r.u8()?);
        if layers == 0 || layers > MAX_LAYER + 1 {
            return None;
        }
        let mut node = Vec::with_capacity(layers);
        for layer in 0..layers {
            let n = r.u32()?;
            let n = r.count(u64::from(n), 4)?;
            if n > if layer == 0 { 2 * g.m } else { g.m } {
                return None;
            }
            let links = (0..n).map(|_| r.u32()).collect::<Option<Vec<u32>>>()?;
            node.push(links);
        }
        g.links.push(node);
    }
    if !r.0.is_empty() {
        return None;
    }
    // Every link leads to a node of its layer, and searches start on the top one.
    let on_layer = |n: u32, layer: usize| g.links.get(n as usize).is_some_and(|l| l.len() > layer);
    let linked = g.links.iter().all(|l| l.iter().enumerate().all(|(layer, ns)| ns.iter().all(|&n| on_layer(n, layer))));
    let started = if slots == 0 { entry == u32::MAX } else { on_layer(entry, g.top) };
    if !linked || !started || g.links.iter().any(|l| l.len() > g.top + 1) {
        return None;
    }
    g.entry = (slots > 0).then_some(entry);

    let dropped = gone.iter().filter(|g| **g).count();
    if dropped > 0 {
        g.take_out(&gone);
    }
    Some(Loaded { graph: g, ids, dropped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hnsw::Metric;
    use crate::hnsw::tests::{points, unit};

    /// A file in the temporary directory, removed at the end.
    struct Temp(std::path::PathBuf);

    impl Temp {
        fn new(name: &str) -> Temp {
            Temp(std::env::temp_dir().join(format!("graphbuff-hnsw-{}-{name}.graph", std::process::id())))
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("tmp"));
        }
    }

    const DIMS: usize = 12;

    fn empty() -> Graph {
        Graph::new(DIMS, Metric::Cosine, 8, 64)
    }

    /// A graph of `n` vectors, entity `10 * slot`, with every seventh slot deleted.
    fn sample(n: usize) -> (Graph, Vec<u64>, Vec<Vec<f32>>) {
        let data: Vec<Vec<f32>> = points(n, DIMS, 77).iter().map(|v| unit(v)).collect();
        let mut g = empty();
        for v in &data {
            g.insert(v);
        }
        for s in (0..n).step_by(7) {
            g.remove(s as u32);
        }
        (g, (0..n as u64).map(|s| 10 * s).collect(), data)
    }

    fn same(a: &Graph, b: &Graph) {
        assert_eq!(a.vectors, b.vectors);
        assert_eq!(a.links, b.links);
        assert_eq!(a.deleted, b.deleted);
        assert_eq!((a.removed, a.entry, a.top, a.rng), (b.removed, b.entry, b.top, b.rng));
    }

    #[test]
    fn round_trip() {
        for n in [0, 1, 2, 700] {
            let t = Temp::new(&format!("round-{n}"));
            let (mut g, ids, data) = sample(n);
            g.save(&t.0, b"key", &ids).unwrap();
            assert!(!t.0.with_extension("tmp").exists());
            let l = Graph::load(&t.0, b"key", empty(), |id| data.get(id as usize / 10).map(|v| &v[..])).expect("loads");
            same(&g, &l.graph);
            assert_eq!(l.dropped, 0);
            for (slot, id) in l.ids.iter().enumerate() {
                assert_eq!(*id, if slot % 7 == 0 { DELETED } else { ids[slot] });
            }
            // It goes on as the saved one does.
            let mut l = l.graph;
            for v in points(50, DIMS, 5) {
                g.insert(&unit(&v));
                l.insert(&unit(&v));
            }
            same(&g, &l);
        }
    }

    #[test]
    fn changed_and_missing_vectors_are_taken_out() {
        let t = Temp::new("changed");
        let (g, ids, data) = sample(3000);
        g.save(&t.0, b"key", &ids).unwrap();
        // One in ten gone, one in ten changed, among them the entry.
        let entry = g.entry.unwrap() as usize;
        let state = |slot: usize| if slot == entry || slot % 10 == 3 { 1 } else { usize::from(slot % 10 == 4) * 2 };
        let other = unit(&points(1, DIMS, 1)[0]);
        let l = Graph::load(&t.0, b"key", empty(), |id| {
            let slot = id as usize / 10;
            match state(slot) {
                0 => Some(&data[slot][..]),
                1 => None,
                _ => Some(&other[..]),
            }
        })
        .expect("loads");
        let live = |s: usize| !s.is_multiple_of(7) && state(s) == 0;
        assert_eq!(l.graph.live(), (0..3000).filter(|&s| live(s)).count());
        assert_eq!(l.dropped, (0..3000).filter(|&s| s % 7 != 0 && state(s) != 0).count());
        // Nothing leads to a slot that was taken out, and the rest is found as before.
        let out = |s: u32| !s.is_multiple_of(7) && state(s as usize) != 0;
        assert!(l.graph.links.iter().flatten().flatten().all(|&n| !out(n)));
        assert!(l.graph.entry.is_some_and(|e| !out(e)));
        let mut hits = 0;
        for q in points(50, DIMS, 9) {
            let q = unit(&q);
            let mut exact: Vec<(f32, usize)> =
                (0..3000).filter(|&s| live(s)).map(|s| (g.distance(&q, &data[s]), s)).collect();
            exact.sort_by(|a, b| a.0.total_cmp(&b.0));
            let got = l.graph.search(&q, 10, 100);
            assert!(got.iter().all(|(_, s)| live(*s as usize)));
            hits += got.iter().filter(|(_, s)| exact[..10].iter().any(|e| e.1 == *s as usize)).count();
        }
        assert!(hits >= 475, "{hits} of 500");
        // All of them.
        let l = Graph::load(&t.0, b"key", empty(), |_| None).expect("loads");
        assert_eq!(l.graph.live(), 0);
        assert!(l.graph.search(&other, 10, 100).is_empty());
        let mut g = l.graph;
        g.insert(&other);
        assert_eq!(g.search(&other, 10, 100).len(), 1);
    }

    #[test]
    fn nodes_around_one_taken_out_are_linked() {
        // Two groups with a single node between them, on two layers.
        let mut g = Graph::new(1, Metric::Euclidean, 4, 16);
        g.vectors = vec![0.0, 1.0, 10.0, 20.0, 21.0];
        g.deleted = vec![false; 5];
        g.links = vec![
            vec![vec![1, 2]],
            vec![vec![0]],
            vec![vec![0, 3], vec![3]],
            vec![vec![2, 4], vec![2]],
            vec![vec![3]],
        ];
        (g.entry, g.top) = (Some(2), 1);
        assert_eq!(g.search(&[0.2], 5, 10).len(), 5);
        g.take_out(&[false, false, true, false, false]);
        // 0 and 3 are linked to each other instead; the entry is the highest one left.
        assert_eq!(g.links, [vec![vec![1, 3]], vec![vec![0]], vec![vec![], vec![]], vec![vec![4, 0], vec![]], vec![vec![3]]]);
        assert_eq!((g.entry, g.top, g.removed), (Some(3), 1, 1));
        assert_eq!(g.search(&[0.2], 5, 10).iter().map(|f| f.1).collect::<Vec<_>>(), [0, 1, 3, 4]);
    }

    #[test]
    fn other_and_damaged_files_are_ignored() {
        let t = Temp::new("damaged");
        let (g, ids, data) = sample(60);
        let load = |key: &[u8], empty: Graph| {
            Graph::load(&t.0, key, empty, |id| data.get(id as usize / 10).map(|v| &v[..])).is_some()
        };
        assert!(!load(b"key", empty()), "no file");
        g.save(&t.0, b"key", &ids).unwrap();
        assert!(load(b"key", empty()));
        assert!(!load(b"kex", empty()));
        assert!(!load(b"key2", empty()));
        assert!(!load(b"key", Graph::new(DIMS, Metric::Cosine, 9, 64)));
        assert!(!load(b"key", Graph::new(DIMS + 1, Metric::Cosine, 8, 64)));
        let good = std::fs::read(&t.0).unwrap();
        for at in 0..good.len() {
            let mut bad = good.clone();
            bad[at] ^= 1 << (at % 8);
            std::fs::write(&t.0, &bad).unwrap();
            assert!(!load(b"key", empty()), "byte {at}");
            std::fs::write(&t.0, &good[..at]).unwrap();
            assert!(!load(b"key", empty()), "cut at {at}");
        }
    }

    /// Damage the checksum does not show: whatever the bytes say, reading them neither
    /// panics nor gives a graph that searches would fail on.
    #[test]
    fn wrong_content_with_a_right_checksum() {
        let t = Temp::new("wrong");
        let (g, ids, data) = sample(60);
        g.save(&t.0, b"key", &ids).unwrap();
        let good = std::fs::read(&t.0).unwrap();
        let content = &good[..good.len() - 8];
        let q = unit(&points(1, DIMS, 4)[0]);
        let mut loaded = 0;
        for at in 0..content.len() {
            for with in [0u8, 1, 0x7F, 0xFF] {
                let mut bad = content.to_vec();
                bad[at] = with;
                let l = read(&mut Reader(&bad), b"key", empty(), |id| data.get(id as usize / 10).map(|v| &v[..]));
                if let Some(mut l) = l {
                    l.graph.search(&q, 10, 100);
                    l.graph.insert(&q);
                    loaded += 1;
                }
            }
            assert!(read(&mut Reader(&content[..at]), b"key", empty(), |_| None).is_none());
        }
        assert!(loaded > 0);
    }

    #[test]
    fn a_failed_save_leaves_the_file_before() {
        let t = Temp::new("failed");
        let (g, ids, _) = sample(10);
        g.save(&t.0, b"key", &ids).unwrap();
        let before = std::fs::read(&t.0).unwrap();
        // The temporary file cannot be created: its name is a folder's.
        let tmp = t.0.with_extension("tmp");
        std::fs::create_dir(&tmp).unwrap();
        assert!(sample(20).0.save(&t.0, b"key", &sample(20).1).is_err());
        std::fs::remove_dir(&tmp).unwrap();
        assert_eq!(std::fs::read(&t.0).unwrap(), before);
        // A folder that is gone is not made again.
        assert!(g.save(&std::env::temp_dir().join("graphbuff-no-such-dir").join("x.graph"), b"key", &ids).is_err());
        assert!(!std::env::temp_dir().join("graphbuff-no-such-dir").exists());
    }
}
