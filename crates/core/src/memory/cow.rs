//! Copy-on-write containers for [`MemoryGraph`](super::MemoryGraph).
//!
//! Cloning one of these shares its whole contents in O(1) (or O(log n)): nodes are
//! reference-counted, and a write copies only the nodes on its path that are still
//! shared with a clone ([`Arc::make_mut`]). A snapshot of the graph is therefore a
//! cheap clone that later writes do not disturb, and a write that follows costs about
//! what it costs without the snapshot.
//!
//! * [`PVec`]: a vector (radix trie, 32 entries per node) for the record slabs and for
//!   adjacency lists,
//! * [`PMap`] / [`PSet`]: ordered maps and sets (B-trees) for the label and property
//!   indexes.

use std::borrow::Borrow;
use std::ops::Bound;
use std::sync::Arc;

const BITS: u32 = 5;
const WIDTH: usize = 1 << BITS;
const MASK: usize = WIDTH - 1;

#[derive(Debug, Clone)]
enum VNode<T> {
    Leaf(Vec<T>),
    Branch(Vec<Arc<VNode<T>>>),
}

/// A copy-on-write vector.
#[derive(Debug, Clone)]
pub(crate) struct PVec<T> {
    len: usize,
    /// Index bits below the root's children: 0 when the root is a leaf.
    shift: u32,
    /// `None` while empty, so empty vectors allocate nothing.
    root: Option<Arc<VNode<T>>>,
}

impl<T: Clone> Default for PVec<T> {
    fn default() -> Self {
        PVec {
            len: 0,
            shift: 0,
            root: None,
        }
    }
}

impl<T: Clone> PVec<T> {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The leaf holding index `i` (which must be in range).
    fn leaf(&self, i: usize) -> &[T] {
        let mut node = &**self.root.as_ref().expect("a non-empty vector");
        let mut shift = self.shift;
        loop {
            match node {
                VNode::Leaf(items) => return items,
                VNode::Branch(children) => {
                    node = &children[(i >> shift) & MASK];
                    shift -= BITS;
                }
            }
        }
    }

    pub(crate) fn get(&self, i: usize) -> Option<&T> {
        (i < self.len).then(|| &self.leaf(i)[i & MASK])
    }

    pub(crate) fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        if i >= self.len {
            return None;
        }
        let mut node = Arc::make_mut(self.root.as_mut()?);
        let mut shift = self.shift;
        loop {
            match node {
                VNode::Leaf(items) => return Some(&mut items[i & MASK]),
                VNode::Branch(children) => {
                    node = Arc::make_mut(&mut children[(i >> shift) & MASK]);
                    shift -= BITS;
                }
            }
        }
    }

    pub(crate) fn last(&self) -> Option<&T> {
        self.len.checked_sub(1).and_then(|i| self.get(i))
    }

    pub(crate) fn push(&mut self, value: T) {
        let i = self.len;
        let root = match &mut self.root {
            Some(root) => root,
            None => self.root.insert(Arc::new(VNode::Leaf(Vec::new()))),
        };
        if i == WIDTH << self.shift {
            // Full: the old root becomes the first child of a new one.
            let old = std::mem::replace(root, Arc::new(VNode::Branch(Vec::new())));
            if let VNode::Branch(children) = Arc::make_mut(root) {
                children.push(old);
            }
            self.shift += BITS;
        }
        let mut node = Arc::make_mut(root);
        let mut shift = self.shift;
        loop {
            match node {
                VNode::Leaf(items) => {
                    items.push(value);
                    break;
                }
                VNode::Branch(children) => {
                    let slot = (i >> shift) & MASK;
                    if slot == children.len() {
                        let child = if shift == BITS {
                            VNode::Leaf(Vec::with_capacity(WIDTH))
                        } else {
                            VNode::Branch(Vec::new())
                        };
                        children.push(Arc::new(child));
                    }
                    node = Arc::make_mut(&mut children[slot]);
                    shift -= BITS;
                }
            }
        }
        self.len += 1;
    }

    /// Removes the last element.
    pub(crate) fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        fn pop_in<T: Clone>(node: &mut Arc<VNode<T>>) -> (Option<T>, bool) {
            match Arc::make_mut(node) {
                VNode::Leaf(items) => {
                    let v = items.pop();
                    (v, items.is_empty())
                }
                VNode::Branch(children) => {
                    let last = children.last_mut().expect("branches are never empty");
                    let (v, empty) = pop_in(last);
                    if empty {
                        children.pop();
                    }
                    (v, children.is_empty())
                }
            }
        }
        let (v, _) = pop_in(self.root.as_mut().expect("a non-empty vector"));
        self.len -= 1;
        if self.len == 0 {
            *self = PVec::default();
        } else {
            // Drop root levels that have a single child.
            while self.shift > 0 && self.len <= WIDTH << (self.shift - BITS) {
                let child = match self.root.as_deref() {
                    Some(VNode::Branch(children)) => children[0].clone(),
                    _ => break,
                };
                self.root = Some(child);
                self.shift -= BITS;
            }
        }
        v
    }

    pub(crate) fn iter(&self) -> PVecIter<'_, T> {
        PVecIter {
            vec: self,
            leaf: &[],
            next: 0,
        }
    }

    /// Index of the first element for which `pred` is false, as
    /// [`slice::partition_point`]; the elements must be partitioned by `pred`.
    pub(crate) fn partition_point(&self, mut pred: impl FnMut(&T) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(self.get(mid).expect("in range")) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

impl<T: Clone> FromIterator<T> for PVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut v = PVec::default();
        for x in iter {
            v.push(x);
        }
        v
    }
}

pub(crate) struct PVecIter<'a, T> {
    vec: &'a PVec<T>,
    /// The rest of the current leaf.
    leaf: &'a [T],
    /// Index of the first element after `leaf`.
    next: usize,
}

impl<'a, T: Clone> Iterator for PVecIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        if self.leaf.is_empty() {
            if self.next >= self.vec.len {
                return None;
            }
            let leaf = self.vec.leaf(self.next);
            self.leaf = &leaf[self.next & MASK..];
            self.next += self.leaf.len();
        }
        let (first, rest) = self.leaf.split_first()?;
        self.leaf = rest;
        Some(first)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.leaf.len() + (self.vec.len - self.next.min(self.vec.len));
        (n, Some(n))
    }
}

// ---- ordered map ----------------------------------------------------------------------

/// Entries per leaf and children per branch before a split.
const FANOUT: usize = 32;

#[derive(Debug, Clone)]
enum MNode<K, V> {
    Leaf(Vec<(K, V)>),
    /// `seps[i]` is at most every key in `children[i + 1]` and above every key in
    /// `children[..=i]`.
    Branch {
        seps: Vec<K>,
        children: Vec<Arc<MNode<K, V>>>,
    },
}

/// A node split off by an insert, with the separator key above it.
type Split<K, V> = Option<(K, Arc<MNode<K, V>>)>;

/// A branch's children during iteration, and the next one to visit.
type Level<'a, K, V> = (&'a [Arc<MNode<K, V>>], usize);

/// A copy-on-write ordered map (a B-tree). Underfull nodes are not merged after removals;
/// empty ones are dropped.
#[derive(Debug, Clone)]
pub(crate) struct PMap<K, V> {
    len: usize,
    root: Arc<MNode<K, V>>,
}

impl<K: Ord + Clone, V: Clone> Default for PMap<K, V> {
    fn default() -> Self {
        PMap {
            len: 0,
            root: Arc::new(MNode::Leaf(Vec::new())),
        }
    }
}

/// Which child of a branch may hold `key`.
fn child_for<K, Q>(seps: &[K], key: &Q) -> usize
where
    K: Ord + Borrow<Q>,
    Q: Ord + ?Sized,
{
    seps.partition_point(|s| s.borrow() <= key)
}

impl<K: Ord + Clone, V: Clone> PMap<K, V> {
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn get<Q: Ord + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        let mut node = &*self.root;
        loop {
            match node {
                MNode::Leaf(entries) => {
                    return entries
                        .binary_search_by(|(k, _)| k.borrow().cmp(key))
                        .ok()
                        .map(|i| &entries[i].1);
                }
                MNode::Branch { seps, children } => node = &children[child_for(seps, key)],
            }
        }
    }

    /// The value of `key`, made unshared. A miss may still copy the path to where the
    /// key would be.
    pub(crate) fn get_mut<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        let mut node = Arc::make_mut(&mut self.root);
        loop {
            match node {
                MNode::Leaf(entries) => {
                    let i = entries.binary_search_by(|(k, _)| k.borrow().cmp(key)).ok()?;
                    return Some(&mut entries[i].1);
                }
                MNode::Branch { seps, children } => {
                    let i = child_for(seps, key);
                    node = Arc::make_mut(&mut children[i]);
                }
            }
        }
    }

    /// Inserts or replaces; returns the old value.
    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        fn ins<K: Ord + Clone, V: Clone>(
            node: &mut Arc<MNode<K, V>>,
            key: K,
            value: V,
        ) -> (Option<V>, Split<K, V>) {
            match Arc::make_mut(node) {
                MNode::Leaf(entries) => match entries.binary_search_by(|(k, _)| k.cmp(&key)) {
                    Ok(i) => (Some(std::mem::replace(&mut entries[i].1, value)), None),
                    Err(i) => {
                        entries.insert(i, (key, value));
                        if entries.len() <= FANOUT {
                            return (None, None);
                        }
                        let right = entries.split_off(entries.len() / 2);
                        let sep = right[0].0.clone();
                        (None, Some((sep, Arc::new(MNode::Leaf(right)))))
                    }
                },
                MNode::Branch { seps, children } => {
                    let i = child_for(seps, &key);
                    let (old, split) = ins(&mut children[i], key, value);
                    if let Some((sep, right)) = split {
                        seps.insert(i, sep);
                        children.insert(i + 1, right);
                        if children.len() > FANOUT {
                            let mid = children.len() / 2;
                            let right_children = children.split_off(mid);
                            let mut right_seps = seps.split_off(mid - 1);
                            let up = right_seps.remove(0);
                            let right = MNode::Branch {
                                seps: right_seps,
                                children: right_children,
                            };
                            return (old, Some((up, Arc::new(right))));
                        }
                    }
                    (old, None)
                }
            }
        }
        let (old, split) = ins(&mut self.root, key, value);
        if let Some((sep, right)) = split {
            let left = std::mem::replace(&mut self.root, Arc::new(MNode::Leaf(Vec::new())));
            self.root = Arc::new(MNode::Branch {
                seps: vec![sep],
                children: vec![left, right],
            });
        }
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    pub(crate) fn remove<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        fn rem<K, V, Q>(node: &mut Arc<MNode<K, V>>, key: &Q) -> Option<V>
        where
            K: Ord + Clone + Borrow<Q>,
            V: Clone,
            Q: Ord + ?Sized,
        {
            match Arc::make_mut(node) {
                MNode::Leaf(entries) => {
                    let i = entries.binary_search_by(|(k, _)| k.borrow().cmp(key)).ok()?;
                    Some(entries.remove(i).1)
                }
                MNode::Branch { seps, children } => {
                    let i = child_for(seps, key);
                    let old = rem(&mut children[i], key)?;
                    let empty = match &*children[i] {
                        MNode::Leaf(e) => e.is_empty(),
                        MNode::Branch { children, .. } => children.is_empty(),
                    };
                    if empty && children.len() > 1 {
                        children.remove(i);
                        seps.remove(i.saturating_sub(1));
                    }
                    Some(old)
                }
            }
        }
        let old = rem(&mut self.root, key)?;
        self.len -= 1;
        loop {
            let only = match &*self.root {
                MNode::Branch { children, .. } if children.len() == 1 => children[0].clone(),
                _ => break,
            };
            self.root = only;
        }
        Some(old)
    }

    pub(crate) fn iter(&self) -> PMapIter<'_, K, V> {
        self.range(Bound::Unbounded)
    }

    /// The entries from `lower` on, in order; callers stop at their upper bound.
    pub(crate) fn range<Q: Ord + ?Sized>(&self, lower: Bound<&Q>) -> PMapIter<'_, K, V>
    where
        K: Borrow<Q>,
    {
        let mut stack = Vec::new();
        let mut node = &*self.root;
        loop {
            match node {
                MNode::Leaf(entries) => {
                    let start = match lower {
                        Bound::Unbounded => 0,
                        Bound::Included(k) => entries.partition_point(|(e, _)| e.borrow() < k),
                        Bound::Excluded(k) => entries.partition_point(|(e, _)| e.borrow() <= k),
                    };
                    return PMapIter {
                        stack,
                        leaf: &entries[start..],
                    };
                }
                MNode::Branch { seps, children } => {
                    let i = match lower {
                        Bound::Unbounded => 0,
                        Bound::Included(k) | Bound::Excluded(k) => child_for(seps, k),
                    };
                    stack.push((&children[..], i + 1));
                    node = &children[i];
                }
            }
        }
    }
}

pub(crate) struct PMapIter<'a, K, V> {
    /// Branches above the current leaf, with the next child to visit in each.
    stack: Vec<Level<'a, K, V>>,
    leaf: &'a [(K, V)],
}

impl<'a, K, V> Iterator for PMapIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        while self.leaf.is_empty() {
            let (children, i) = self.stack.last_mut()?;
            if *i >= children.len() {
                self.stack.pop();
                continue;
            }
            let mut node = &*children[*i];
            *i += 1;
            loop {
                match node {
                    MNode::Leaf(entries) => {
                        self.leaf = entries;
                        break;
                    }
                    MNode::Branch { children, .. } => {
                        self.stack.push((&children[..], 1));
                        node = &children[0];
                    }
                }
            }
        }
        let ((k, v), rest) = self.leaf.split_first()?;
        self.leaf = rest;
        Some((k, v))
    }
}

/// A copy-on-write ordered set.
#[derive(Debug, Clone)]
pub(crate) struct PSet<K>(PMap<K, ()>);

impl<K: Ord + Clone> Default for PSet<K> {
    fn default() -> Self {
        PSet(PMap::default())
    }
}

impl<K: Ord + Clone> PSet<K> {
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns whether `k` was new.
    pub(crate) fn insert(&mut self, k: K) -> bool {
        self.0.insert(k, ()).is_none()
    }

    /// Returns whether `k` was present.
    pub(crate) fn remove(&mut self, k: &K) -> bool {
        self.0.remove(k).is_some()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &K> + '_ {
        self.0.iter().map(|(k, _)| k)
    }

    pub(crate) fn range(&self, lower: Bound<&K>) -> impl Iterator<Item = &K> + '_ {
        self.0.range(lower).map(|(k, _)| k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn pvec_matches_vec_and_clones_are_independent() {
        let mut v: PVec<u64> = PVec::default();
        let mut model = Vec::new();
        let mut snapshots = Vec::new();
        for i in 0..5000u64 {
            v.push(i);
            model.push(i);
            if i % 997 == 0 {
                snapshots.push((v.clone(), model.clone()));
            }
            if i % 7 == 0 {
                let j = (i * 31 % (model.len() as u64)) as usize;
                *v.get_mut(j).unwrap() += 1_000_000;
                model[j] += 1_000_000;
            }
            if i % 11 == 0 {
                assert_eq!(v.pop(), model.pop());
            }
        }
        assert_eq!(v.len(), model.len());
        assert_eq!(v.iter().copied().collect::<Vec<_>>(), model);
        for (s, m) in &snapshots {
            assert_eq!(s.iter().copied().collect::<Vec<_>>(), *m);
            assert_eq!(s.get(m.len() / 2), m.get(m.len() / 2));
        }
        while v.pop().is_some() {
            model.pop();
        }
        assert!(model.is_empty() && v.is_empty());
        assert_eq!(v.partition_point(|_| true), 0);
    }

    #[test]
    fn pmap_matches_btreemap_and_clones_are_independent() {
        let mut m: PMap<u64, u64> = PMap::default();
        let mut model = BTreeMap::new();
        let mut snapshots = Vec::new();
        let mut x = 12345u64;
        for i in 0..20_000u64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let k = x % 3000;
            if x.is_multiple_of(3) {
                assert_eq!(m.remove(&k), model.remove(&k));
            } else {
                assert_eq!(m.insert(k, i), model.insert(k, i));
            }
            if i % 4999 == 0 {
                snapshots.push((m.clone(), model.clone()));
            }
        }
        assert_eq!(m.len(), model.len());
        let all: Vec<(u64, u64)> = m.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(all, model.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>());
        for lower in [0u64, 1, 1500, 2999, 5000] {
            for b in [Bound::Included(&lower), Bound::Excluded(&lower)] {
                let got: Vec<u64> = m.range(b).map(|(k, _)| *k).collect();
                let want: Vec<u64> = model.range((b.cloned(), Bound::Unbounded)).map(|(k, _)| *k).collect();
                assert_eq!(got, want);
            }
        }
        for (s, sm) in &snapshots {
            assert_eq!(s.len(), sm.len());
            assert!(s.iter().map(|(k, v)| (*k, *v)).eq(sm.iter().map(|(k, v)| (*k, *v))));
        }
        for k in 0..3000 {
            assert_eq!(m.get(&k), model.get(&k));
            m.remove(&k);
        }
        assert!(m.is_empty() && m.iter().next().is_none());
    }
}

/// A copy-on-write vector for small lists inside records: cloning shares it, and
/// [`make_mut`](CowVec::make_mut) copies it if it is shared. Empty lists allocate nothing.
#[derive(Debug)]
pub(crate) struct CowVec<T>(Option<Arc<Vec<T>>>);

impl<T> Clone for CowVec<T> {
    fn clone(&self) -> Self {
        CowVec(self.0.clone())
    }
}

impl<T> Default for CowVec<T> {
    fn default() -> Self {
        CowVec(None)
    }
}

impl<T: Clone> CowVec<T> {
    pub(crate) fn make_mut(&mut self) -> &mut Vec<T> {
        Arc::make_mut(self.0.get_or_insert_with(Default::default))
    }
}

impl<T> std::ops::Deref for CowVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.0.as_deref().map_or(&[], Vec::as_slice)
    }
}

impl<T> From<Vec<T>> for CowVec<T> {
    fn from(v: Vec<T>) -> Self {
        CowVec((!v.is_empty()).then(|| Arc::new(v)))
    }
}
