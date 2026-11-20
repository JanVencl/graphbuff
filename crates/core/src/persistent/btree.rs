//! A disk B+tree of byte-string keys (a sorted set), stored in pages of the index file.
//!
//! Used for the label index and the property indexes. Keys are unique; the entity id is
//! part of the key. Leaves are linked left to right for prefix scans.
//!
//! Deletion removes the key from its leaf without rebalancing, so leaves may become
//! sparse or empty. Scans skip empty leaves; the space is reused once keys are inserted
//! into the same range again, and released when the index is dropped.
//!
//! Page layout:
//!
//! ```text
//! leaf:     1 u8 | count u16 | next leaf u64 | (key len u16, key)*
//! internal: 2 u8 | count u16 | child0 u64    | (key len u16, key, child u64)*
//! ```
//!
//! In an internal node, key `i` is the smallest key reachable through child `i + 1`.

use super::pager::{PAGE_SIZE, PageKey, Pager};
use crate::{Error, Result};

pub(crate) const INDEX_FILE: u8 = 3;
/// Longest key the tree accepts. Callers shorten longer keys (see `value_key`).
pub(crate) const MAX_KEY: usize = 1024;
const HEADER: usize = 11;

/// Page allocator of the index file. Page 0 is never used, so `0` means "no page".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PageAlloc {
    pub next: u64,
    /// Head of the list of free pages, linked through their first 8 bytes.
    pub free: u64,
}

impl Default for PageAlloc {
    fn default() -> Self {
        PageAlloc { next: 1, free: 0 }
    }
}

fn pk(page: u64) -> PageKey {
    PageKey {
        file: INDEX_FILE,
        page,
    }
}

fn corrupt(page: u64, what: &str) -> Error {
    Error::Storage(format!("index page {page} is corrupt: {what}"))
}

struct Node {
    leaf: bool,
    /// Next leaf (leaves only).
    next: u64,
    keys: Vec<Vec<u8>>,
    /// `keys.len() + 1` children (internal nodes only).
    children: Vec<u64>,
}

impl Node {
    fn leaf(keys: Vec<Vec<u8>>) -> Node {
        Node {
            leaf: true,
            next: 0,
            keys,
            children: Vec::new(),
        }
    }

    fn entry_size(&self, key: &[u8]) -> usize {
        2 + key.len() + if self.leaf { 0 } else { 8 }
    }

    fn size(&self) -> usize {
        HEADER + self.keys.iter().map(|k| self.entry_size(k)).sum::<usize>()
    }

    fn decode(page: u64, b: &[u8]) -> Result<Node> {
        let leaf = match b[0] {
            1 => true,
            2 => false,
            _ => return Err(corrupt(page, "unknown node kind")),
        };
        let count = u16::from_le_bytes([b[1], b[2]]) as usize;
        let first = u64::from_le_bytes(b[3..11].try_into().unwrap());
        let mut n = Node {
            leaf,
            next: if leaf { first } else { 0 },
            keys: Vec::with_capacity(count),
            children: if leaf { Vec::new() } else { vec![first] },
        };
        let mut pos = HEADER;
        for _ in 0..count {
            let klen = b
                .get(pos..pos + 2)
                .map(|s| u16::from_le_bytes([s[0], s[1]]) as usize)
                .ok_or_else(|| corrupt(page, "entry past the end"))?;
            pos += 2;
            let key = b
                .get(pos..pos + klen)
                .ok_or_else(|| corrupt(page, "key past the end"))?;
            n.keys.push(key.to_vec());
            pos += klen;
            if !leaf {
                let c = b
                    .get(pos..pos + 8)
                    .ok_or_else(|| corrupt(page, "child past the end"))?;
                n.children.push(u64::from_le_bytes(c.try_into().unwrap()));
                pos += 8;
            }
        }
        Ok(n)
    }

    fn encode(&self, b: &mut [u8]) {
        debug_assert!(self.size() <= PAGE_SIZE);
        b.fill(0);
        b[0] = if self.leaf { 1 } else { 2 };
        b[1..3].copy_from_slice(&(self.keys.len() as u16).to_le_bytes());
        let first = if self.leaf {
            self.next
        } else {
            self.children[0]
        };
        b[3..11].copy_from_slice(&first.to_le_bytes());
        let mut pos = HEADER;
        for (i, k) in self.keys.iter().enumerate() {
            b[pos..pos + 2].copy_from_slice(&(k.len() as u16).to_le_bytes());
            pos += 2;
            b[pos..pos + k.len()].copy_from_slice(k);
            pos += k.len();
            if !self.leaf {
                b[pos..pos + 8].copy_from_slice(&self.children[i + 1].to_le_bytes());
                pos += 8;
            }
        }
    }

    /// Splits an overfull node by bytes. Returns the separator and the right half.
    fn split(&mut self) -> (Vec<u8>, Node) {
        let total: usize = self.keys.iter().map(|k| self.entry_size(k)).sum();
        let mut acc = 0;
        let mut m = 0;
        for (i, k) in self.keys.iter().enumerate() {
            acc += self.entry_size(k);
            if acc >= total / 2 {
                m = i;
                break;
            }
        }
        if self.leaf {
            let m = m.clamp(1, self.keys.len() - 1);
            let right_keys = self.keys.split_off(m);
            let sep = right_keys[0].clone();
            let mut right = Node::leaf(right_keys);
            right.next = self.next;
            (sep, right)
        } else {
            let m = m.clamp(1, self.keys.len() - 2);
            let right_keys = self.keys.split_off(m + 1);
            let sep = self.keys.pop().expect("m >= 1");
            let right_children = self.children.split_off(m + 1);
            (
                sep,
                Node {
                    leaf: false,
                    next: 0,
                    keys: right_keys,
                    children: right_children,
                },
            )
        }
    }
}

fn read(p: &mut Pager, page: u64) -> Result<Node> {
    p.read(pk(page), |b| Node::decode(page, b))?
}

fn write(p: &mut Pager, page: u64, n: &Node) -> Result<()> {
    n.encode(p.write(pk(page))?);
    Ok(())
}

fn alloc(p: &mut Pager, a: &mut PageAlloc) -> Result<u64> {
    if a.free != 0 {
        let page = a.free;
        a.free = p.read(pk(page), |b| u64::from_le_bytes(b[0..8].try_into().unwrap()))?;
        Ok(page)
    } else {
        let page = a.next;
        a.next += 1;
        Ok(page)
    }
}

fn release(p: &mut Pager, a: &mut PageAlloc, page: u64) -> Result<()> {
    let b = p.write(pk(page))?;
    b.fill(0);
    b[0..8].copy_from_slice(&a.free.to_le_bytes());
    a.free = page;
    Ok(())
}

/// Number of keys `<= key`: the child to descend into.
fn child_index(keys: &[Vec<u8>], key: &[u8]) -> usize {
    keys.partition_point(|k| k.as_slice() <= key)
}

/// Inserts `key`. Returns `false` if it was already present.
pub(crate) fn insert(p: &mut Pager, a: &mut PageAlloc, root: &mut u64, key: &[u8]) -> Result<bool> {
    assert!(key.len() <= MAX_KEY, "index key too long");
    if *root == 0 {
        let page = alloc(p, a)?;
        write(p, page, &Node::leaf(vec![key.to_vec()]))?;
        *root = page;
        return Ok(true);
    }
    let (inserted, split) = insert_into(p, a, *root, key)?;
    if let Some((sep, right)) = split {
        let page = alloc(p, a)?;
        let new_root = Node {
            leaf: false,
            next: 0,
            keys: vec![sep],
            children: vec![*root, right],
        };
        write(p, page, &new_root)?;
        *root = page;
    }
    Ok(inserted)
}

type Split = Option<(Vec<u8>, u64)>;

fn insert_into(p: &mut Pager, a: &mut PageAlloc, page: u64, key: &[u8]) -> Result<(bool, Split)> {
    let mut n = read(p, page)?;
    if n.leaf {
        match n.keys.binary_search_by(|k| k.as_slice().cmp(key)) {
            Ok(_) => return Ok((false, None)),
            Err(i) => n.keys.insert(i, key.to_vec()),
        }
    } else {
        let i = child_index(&n.keys, key);
        let (inserted, split) = insert_into(p, a, n.children[i], key)?;
        let Some((sep, right)) = split else {
            return Ok((inserted, None));
        };
        n.keys.insert(i, sep);
        n.children.insert(i + 1, right);
    }
    if n.size() <= PAGE_SIZE {
        write(p, page, &n)?;
        return Ok((true, None));
    }
    let (sep, mut right) = n.split();
    let right_page = alloc(p, a)?;
    if n.leaf {
        right.next = n.next;
        n.next = right_page;
    }
    write(p, page, &n)?;
    write(p, right_page, &right)?;
    Ok((true, Some((sep, right_page))))
}

/// Builds a new tree from `keys`, which must be strictly ascending, filling pages left
/// to right (bulk import). Returns the root, `0` for no keys.
pub(crate) fn build(p: &mut Pager, a: &mut PageAlloc, keys: impl IntoIterator<Item = Vec<u8>>) -> Result<u64> {
    // Leaves: each is written once the next one's page is known, for the link.
    let mut level: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut leaf = Node::leaf(Vec::new());
    let mut size = HEADER;
    let mut page = 0;
    for key in keys {
        assert!(key.len() <= MAX_KEY, "index key too long");
        debug_assert!(leaf.keys.last().is_none_or(|k| *k < key), "keys out of order");
        if page == 0 {
            page = alloc(p, a)?;
        } else if size + 2 + key.len() > PAGE_SIZE {
            let next = alloc(p, a)?;
            leaf.next = next;
            write(p, page, &leaf)?;
            level.push((std::mem::take(&mut leaf.keys)[0].clone(), page));
            leaf.next = 0;
            size = HEADER;
            page = next;
        }
        size += 2 + key.len();
        leaf.keys.push(key);
    }
    if page == 0 {
        return Ok(0);
    }
    write(p, page, &leaf)?;
    level.push((leaf.keys.swap_remove(0), page));

    // Internal levels, until one node is left. A node's first child adds no key.
    while level.len() > 1 {
        let mut groups: Vec<Vec<(Vec<u8>, u64)>> = Vec::new();
        let mut cur: Vec<(Vec<u8>, u64)> = Vec::new();
        let mut size = HEADER;
        for child in level {
            let entry = 2 + child.0.len() + 8;
            if !cur.is_empty() && size + entry > PAGE_SIZE {
                groups.push(std::mem::take(&mut cur));
                size = HEADER;
            }
            if !cur.is_empty() {
                size += entry;
            }
            cur.push(child);
        }
        groups.push(cur);
        // A last node with a single child has no key: give it one from its neighbour.
        if groups.len() > 1 && groups[groups.len() - 1].len() == 1 {
            let n = groups.len();
            let moved = groups[n - 2].pop().expect("a full node has children");
            groups[n - 1].insert(0, moved);
        }
        let mut next = Vec::with_capacity(groups.len());
        for g in groups {
            let mut it = g.into_iter();
            let (first_key, first_child) = it.next().expect("groups are not empty");
            let mut node = Node {
                leaf: false,
                next: 0,
                keys: Vec::new(),
                children: vec![first_child],
            };
            for (k, c) in it {
                node.keys.push(k);
                node.children.push(c);
            }
            let page = alloc(p, a)?;
            write(p, page, &node)?;
            next.push((first_key, page));
        }
        level = next;
    }
    Ok(level[0].1)
}

/// Removes `key`. Returns `false` if it was not present.
pub(crate) fn remove(p: &mut Pager, root: u64, key: &[u8]) -> Result<bool> {
    let mut page = root;
    if page == 0 {
        return Ok(false);
    }
    loop {
        let mut n = read(p, page)?;
        if !n.leaf {
            page = n.children[child_index(&n.keys, key)];
            continue;
        }
        return match n.keys.binary_search_by(|k| k.as_slice().cmp(key)) {
            Ok(i) => {
                n.keys.remove(i);
                write(p, page, &n)?;
                Ok(true)
            }
            Err(_) => Ok(false),
        };
    }
}

/// The smallest byte string greater than every string starting with `prefix`, or `None`
/// if there is none (the prefix is empty or all `0xFF`).
pub(crate) fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut v = prefix.to_vec();
    while let Some(last) = v.last_mut() {
        if *last == 0xFF {
            v.pop();
        } else {
            *last += 1;
            return Some(v);
        }
    }
    None
}

/// A resumable scan of the keys in `start..end`, in ascending order.
///
/// The cursor holds no borrow of the pager, so a caller can read a batch, release the
/// page cache, and continue later. The tree must not be modified while a cursor is
/// in use (stores guarantee this: scans borrow the store immutably).
pub(crate) struct Cursor {
    root: u64,
    start: Vec<u8>,
    /// Exclusive upper end; `None` = to the last key.
    end: Option<Vec<u8>>,
    /// Current leaf (`0` before the first batch).
    leaf: u64,
    /// Next position in `leaf`.
    idx: usize,
    done: bool,
}

impl Cursor {
    /// The keys starting with `prefix`.
    pub fn new(root: u64, prefix: &[u8]) -> Cursor {
        Cursor::range(root, prefix.to_vec(), successor(prefix))
    }

    /// The keys `k` with `start <= k < end`.
    pub fn range(root: u64, start: Vec<u8>, end: Option<Vec<u8>>) -> Cursor {
        Cursor {
            root,
            start,
            end,
            leaf: 0,
            idx: 0,
            done: root == 0,
        }
    }

    /// Whether every matching key has been returned.
    pub fn done(&self) -> bool {
        self.done
    }

    /// Appends up to `max` further matching keys to `out`.
    pub fn next_batch(&mut self, p: &mut Pager, max: usize, out: &mut Vec<Vec<u8>>) -> Result<()> {
        if self.done {
            return Ok(());
        }
        if self.leaf == 0 {
            let mut page = self.root;
            loop {
                let n = read(p, page)?;
                if n.leaf {
                    self.idx = n.keys.partition_point(|k| k.as_slice() < self.start.as_slice());
                    break;
                }
                page = n.children[child_index(&n.keys, &self.start)];
            }
            self.leaf = page;
        }
        let mut taken = 0;
        loop {
            let n = read(p, self.leaf)?;
            while let Some(k) = n.keys.get(self.idx) {
                if self.end.as_deref().is_some_and(|e| k.as_slice() >= e) {
                    self.done = true;
                    return Ok(());
                }
                self.idx += 1;
                if k.as_slice() >= self.start.as_slice() {
                    out.push(k.clone());
                    taken += 1;
                    if taken == max {
                        return Ok(());
                    }
                }
            }
            if n.next == 0 {
                self.done = true;
                return Ok(());
            }
            self.leaf = n.next;
            self.idx = 0;
        }
    }
}

/// Calls `f` for every key starting with `prefix`, in ascending order.
pub(crate) fn scan_prefix(p: &mut Pager, root: u64, prefix: &[u8], f: impl FnMut(&[u8])) -> Result<()> {
    scan_range(p, root, prefix, successor(prefix).as_deref(), f)
}

/// Calls `f` for every key `k` with `start <= k < end` (`end` `None` = no upper end).
pub(crate) fn scan_range(
    p: &mut Pager,
    root: u64,
    start: &[u8],
    end: Option<&[u8]>,
    mut f: impl FnMut(&[u8]),
) -> Result<()> {
    let mut cursor = Cursor::range(root, start.to_vec(), end.map(<[u8]>::to_vec));
    let mut keys = Vec::new();
    while !cursor.done() {
        keys.clear();
        cursor.next_batch(p, 1024, &mut keys)?;
        keys.iter().for_each(|k| f(k));
    }
    Ok(())
}

/// The decoded contents of one tree page, for the consistency check.
pub(crate) struct PageView {
    pub leaf: bool,
    /// Next leaf (leaves only).
    pub next: u64,
    pub keys: Vec<Vec<u8>>,
    /// `keys.len() + 1` children (internal nodes only).
    pub children: Vec<u64>,
}

/// Reads and decodes tree page `page`.
pub(crate) fn view(p: &mut Pager, page: u64) -> Result<PageView> {
    let n = read(p, page)?;
    Ok(PageView {
        leaf: n.leaf,
        next: n.next,
        keys: n.keys,
        children: n.children,
    })
}

/// The next page on the free list after free page `page`.
pub(crate) fn free_next(p: &mut Pager, page: u64) -> Result<u64> {
    p.read(pk(page), |b| u64::from_le_bytes(b[0..8].try_into().unwrap()))
}

/// Releases every page of the tree.
pub(crate) fn destroy(p: &mut Pager, a: &mut PageAlloc, root: u64) -> Result<()> {
    if root == 0 {
        return Ok(());
    }
    let n = read(p, root)?;
    if !n.leaf {
        for c in n.children {
            destroy(p, a, c)?;
        }
    }
    release(p, a, root)
}

/// A B-tree page: `bytes`, zero-padded to a page as the pager reads it (see
/// [`crate::fuzzing`]).
#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_decode(bytes: &[u8]) {
    let mut page = vec![0; PAGE_SIZE];
    let n = bytes.len().min(PAGE_SIZE);
    page[..n].copy_from_slice(&bytes[..n]);
    let _ = Node::decode(1, &page);
}
