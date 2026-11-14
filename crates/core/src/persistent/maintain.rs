//! Online backup and the consistency check of a persistent store, both made on a
//! [`PersistentSnapshot`]: they lock the page cache a page (or a node) at a time, so
//! writers keep committing while they run.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::btree;
use super::pager::{PAGE_SIZE, PageKey};
use super::{
    FILE_NAMES, GROUP_REC, GROUPS, IndexMeta, NodeRec, PersistentSnapshot, Rd, WAL_FILE, WAL_MAGIC,
    get64, index_value_key, io, ix_entry, label_key, locate, lock_at, write_meta,
};
use crate::maintenance::{CheckReport, finish_backup, start_backup, write_synced};
use crate::memory::{Token, index_key};
use crate::store::{ConstraintKind, IndexEntity};
use crate::{BackupInfo, NodeId, RelId, Result, Value};

/// Pages copied per lock of the page cache during a backup.
const BACKUP_BATCH: u64 = 64;

impl PersistentSnapshot {
    /// Copies the snapshot's pages and metadata into the new directory `dest`.
    pub(super) fn write_backup(&self, dest: &Path) -> Result<BackupInfo> {
        start_backup(dest)?;
        let mut bytes = 0u64;
        let mut buf = vec![0u8; PAGE_SIZE * BACKUP_BATCH as usize];
        for (file, name) in FILE_NAMES.iter().enumerate() {
            let file = file as u8;
            let path = dest.join(name);
            let mut out = File::create(&path).map_err(io(&path, "creating"))?;
            let pages = lock_at(&self.pager, None).file_pages(file)?;
            // Pages past the last non-zero one are left out: they read as zeros anyway.
            let (mut written, mut kept) = (0u64, 0u64);
            let mut page = 0u64;
            while page < pages {
                let n = BACKUP_BATCH.min(pages - page);
                let mut last_used = None;
                {
                    let mut p = lock_at(&self.pager, None);
                    for i in 0..n {
                        let chunk = &mut buf[i as usize * PAGE_SIZE..(i as usize + 1) * PAGE_SIZE];
                        p.copy_page_at(PageKey { file, page: page + i }, self.at, chunk)?;
                        if chunk.iter().any(|&b| b != 0) {
                            last_used = Some(i);
                        }
                    }
                }
                if let Some(i) = last_used {
                    // Zero pages held back since the last write go out before this batch.
                    let end = page + i + 1;
                    while written < page {
                        out.write_all(&[0u8; PAGE_SIZE]).map_err(io(&path, "writing"))?;
                        written += 1;
                    }
                    out.write_all(&buf[..(end - page) as usize * PAGE_SIZE])
                        .map_err(io(&path, "writing"))?;
                    written = end;
                    kept = end;
                }
                page += n;
            }
            out.sync_all().map_err(io(&path, "syncing"))?;
            bytes += kept * PAGE_SIZE as u64;
        }
        write_synced(&dest.join(WAL_FILE), WAL_MAGIC)?;
        write_meta(dest, &self.meta)?;
        finish_backup(dest)?;
        let meta_len = std::fs::metadata(dest.join(super::META_FILE)).map_or(0, |m| m.len());
        Ok(BackupInfo {
            dir: dest.to_path_buf(),
            format: "persistent",
            nodes: self.meta.node_count,
            relationships: self.meta.rel_count,
            bytes: bytes + meta_len + WAL_MAGIC.len() as u64,
        })
    }

    /// Checks the snapshot's records, lists, counts, indexes and constraints.
    pub(super) fn check_store(&self) -> Result<CheckReport> {
        let mut c = Checker::new(self);
        c.nodes();
        c.relationships();
        c.free_lists();
        c.indexes();
        c.constraints();
        Ok(c.rep)
    }
}

/// A tree page to visit: page, depth, lower and upper key bound.
type TreeStep = (u64, usize, Option<Vec<u8>>, Option<Vec<u8>>);

/// Collected entries of one constraint: values (normalized) by their index key.
type Seen = BTreeMap<Vec<u8>, Vec<(u64, Vec<Value>)>>;

struct Checker<'a> {
    s: &'a PersistentSnapshot,
    rep: CheckReport,
    /// Blob blocks reached so far (from entities or the free list).
    blocks: Vec<bool>,
    /// Group records reached so far.
    groups: Vec<bool>,
    /// How many outgoing / incoming lists each relationship was found on.
    on_out: Vec<u8>,
    on_in: Vec<u8>,
    label_counts: Vec<u64>,
    type_counts: Vec<u64>,
    /// Expected label index keys, and expected keys of each property index.
    label_keys: Vec<Vec<u8>>,
    index_keys: Vec<Vec<Vec<u8>>>,
    /// Per constraint: nodes seen with all of its properties.
    constraint_seen: Vec<Seen>,
    /// Constraint label and property tokens (`None` when a name was never used).
    constraint_tokens: Vec<(Option<Token>, Vec<Option<Token>>)>,
}

impl<'a> Checker<'a> {
    fn new(s: &'a PersistentSnapshot) -> Self {
        let m = &s.meta;
        Checker {
            s,
            rep: CheckReport::default(),
            blocks: vec![false; m.blobs.next as usize],
            groups: vec![false; m.groups.next as usize],
            on_out: vec![0; m.next_rel as usize],
            on_in: vec![0; m.next_rel as usize],
            label_counts: vec![0; m.label_counts.len()],
            type_counts: vec![0; m.type_counts.len()],
            label_keys: Vec::new(),
            index_keys: vec![Vec::new(); m.indexes.len()],
            constraint_seen: vec![Seen::new(); m.constraints.len()],
            constraint_tokens: m
                .constraints
                .iter()
                .map(|c| (m.labels.get(&c.label), c.properties.iter().map(|p| m.keys.get(p)).collect()))
                .collect(),
        }
    }

    fn read<T>(&self, f: impl FnOnce(&mut Rd) -> Result<T>) -> Result<T> {
        self.s.reader(f)
    }

    /// Marks the blocks of the blob at `ptr` as used by `what`.
    fn blob_blocks(&mut self, ptr: u64, what: &str) {
        let mut b = ptr;
        let mut steps = 0u64;
        while b != 0 {
            if b as usize >= self.blocks.len() {
                self.rep.error(format!("{what}: blob block {b} is out of range"));
                return;
            }
            if self.blocks[b as usize] {
                self.rep.error(format!("{what}: blob block {b} is also used elsewhere"));
                return;
            }
            self.blocks[b as usize] = true;
            steps += 1;
            match self.read(|r| r.block(b)) {
                Ok((next, _)) => b = next,
                Err(e) => {
                    self.rep.error(format!("{what}: {e}"));
                    return;
                }
            }
            if steps > self.blocks.len() as u64 {
                self.rep.error(format!("{what}: blob chain has a cycle"));
                return;
            }
        }
    }

    // ---- nodes and adjacency ----

    fn nodes(&mut self) {
        let m = &self.s.meta;
        let mut live = 0u64;
        for id in 0..m.next_node {
            let rec = match self.read(|r| r.node(NodeId(id))) {
                Ok(Some(rec)) => rec,
                Ok(None) => continue,
                Err(e) => {
                    self.rep.error(format!("node {id}: {e}"));
                    continue;
                }
            };
            live += 1;
            let what = format!("node {id}");
            self.blob_blocks(rec.blob, &what);
            match self.read(|r| r.node_data(&rec)) {
                Ok(nd) => {
                    let mut labels = nd.labels.clone();
                    labels.sort_unstable();
                    labels.dedup();
                    if labels.len() != nd.labels.len() {
                        self.rep.error(format!("{what} has a label twice"));
                    }
                    for &l in &labels {
                        self.label_counts[l as usize] += 1;
                        self.label_keys.push(label_key(l, id).to_vec());
                    }
                    for (i, ix) in m.indexes.iter().enumerate() {
                        if ix.def.entity == IndexEntity::Node
                            && labels.contains(&ix.label)
                            && let Some(k) = ix_entry(ix, &nd.props, id)
                        {
                            self.index_keys[i].push(k);
                        }
                    }
                    self.constraint_node(id, &labels, &nd.props);
                }
                Err(e) => self.rep.error(format!("{what}: {e}")),
            }
            if let Err(e) = self.adjacency(id, &rec) {
                self.rep.error(format!("{what}: {e}"));
            }
        }
        self.rep.nodes = live;
        if live != m.node_count {
            self.rep
                .error(format!("{live} nodes are in use, but the metadata counts {}", m.node_count));
        }
        for (t, (&want, &got)) in m.label_counts.iter().zip(&self.label_counts).enumerate() {
            if want != got {
                self.rep.error(format!(
                    "label `{}` is on {got} nodes, but the metadata counts {want}",
                    m.labels.name(t as Token)
                ));
            }
        }
    }

    /// Walks every adjacency list of node `id`, checking links, ends, endpoints and
    /// degrees.
    fn adjacency(&mut self, id: u64, n: &NodeRec) -> Result<()> {
        if !n.dense {
            let (outs, _) = self.list(id, n.first_out, n.last_out, true, None)?;
            let (ins, _) = self.list(id, n.first_in, n.last_in, false, None)?;
            self.degree(id, "outgoing", None, n.out_deg, outs);
            self.degree(id, "incoming", None, n.in_deg, ins);
            return Ok(());
        }
        if n.last_out != 0 || n.first_in != 0 || n.last_in != 0 {
            self.rep.error(format!("dense node {id} has list ends set"));
        }
        let groups = self.read(|r| r.groups(n))?;
        let (mut total_out, mut total_in) = (0u64, 0u64);
        let mut prev_type = None;
        for (g, rec) in groups {
            if self.groups[g as usize] {
                self.rep.error(format!("node {id}: relationship group {g} is also used elsewhere"));
                continue;
            }
            self.groups[g as usize] = true;
            if prev_type.is_some_and(|t| t >= rec.rel_type) {
                self.rep.error(format!("node {id}: relationship groups are not in type order"));
            }
            prev_type = Some(rec.rel_type);
            if rec.out_deg == 0 && rec.in_deg == 0 {
                self.rep.error(format!("node {id}: relationship group {g} is empty"));
            }
            let (outs, loops) = self.list(id, rec.first_out, rec.last_out, true, Some(rec.rel_type))?;
            let (ins, _) = self.list(id, rec.first_in, rec.last_in, false, Some(rec.rel_type))?;
            let what = Some(g);
            self.degree(id, "outgoing", what, rec.out_deg, outs);
            self.degree(id, "incoming", what, rec.in_deg, ins);
            if u64::from(rec.loops) != loops {
                self.rep.error(format!(
                    "node {id}, group {g}: {loops} self-loops, but the group counts {}",
                    rec.loops
                ));
            }
            total_out += outs;
            total_in += ins;
        }
        self.degree(id, "outgoing", None, n.out_deg, total_out);
        self.degree(id, "incoming", None, n.in_deg, total_in);
        Ok(())
    }

    fn degree(&mut self, id: u64, dir: &str, group: Option<u64>, recorded: u32, found: u64) {
        if u64::from(recorded) != found {
            let place = group.map_or(String::new(), |g| format!(", group {g}"));
            self.rep.error(format!(
                "node {id}{place}: {found} {dir} relationships, but the degree is {recorded}"
            ));
        }
    }

    /// Walks one list of node `id`. Returns its length and its self-loops.
    fn list(&mut self, id: u64, first: u64, last: u64, out: bool, t: Option<Token>) -> Result<(u64, u64)> {
        let dir = if out { "outgoing" } else { "incoming" };
        let limit = self.s.meta.next_rel;
        let (mut link, mut prev, mut len, mut loops) = (first, 0u64, 0u64, 0u64);
        // Read a batch of records per lock.
        while link != 0 {
            let mut batch = Vec::new();
            let mut cursor = link;
            self.read(|r| {
                while cursor != 0 && batch.len() < 256 {
                    let rec = r.rel(RelId(cursor - 1))?;
                    let next = rec.map_or(0, |rec| if out { rec.next_out } else { rec.next_in });
                    batch.push((cursor, rec));
                    if rec.is_none() {
                        break;
                    }
                    cursor = next;
                }
                Ok(())
            })?;
            for (l, rec) in batch {
                len += 1;
                if len > limit {
                    self.rep.error(format!("node {id}: {dir} list has a cycle"));
                    return Ok((len, loops));
                }
                let Some(rec) = rec else {
                    self.rep
                        .error(format!("node {id}: {dir} list points at missing relationship {}", l - 1));
                    return Ok((len, loops));
                };
                let rid = l - 1;
                let (back, end, seen) = if out {
                    (rec.prev_out, rec.start, &mut self.on_out)
                } else {
                    (rec.prev_in, rec.end, &mut self.on_in)
                };
                seen[rid as usize] = seen[rid as usize].saturating_add(1);
                if back != prev {
                    self.rep.error(format!(
                        "relationship {rid}: previous {dir} link is {}, expected {}",
                        link_name(back),
                        link_name(prev)
                    ));
                }
                if end != id {
                    self.rep.error(format!("relationship {rid} is on the {dir} list of node {id}, but not attached there"));
                }
                if t.is_some_and(|t| t != rec.rel_type) {
                    self.rep.error(format!("relationship {rid} is in a group of another type"));
                }
                if rec.start == rec.end && out {
                    loops += 1;
                }
                prev = l;
                link = if out { rec.next_out } else { rec.next_in };
            }
        }
        if prev != last {
            self.rep.error(format!(
                "node {id}: {dir} list ends at {}, but its tail is {}",
                link_name(prev),
                link_name(last)
            ));
        }
        Ok((len, loops))
    }

    // ---- relationships ----

    fn relationships(&mut self) {
        let m = &self.s.meta;
        let mut live = 0u64;
        for id in 0..m.next_rel {
            let rec = match self.read(|r| r.rel(RelId(id))) {
                Ok(Some(rec)) => rec,
                Ok(None) => continue,
                Err(e) => {
                    self.rep.error(format!("relationship {id}: {e}"));
                    continue;
                }
            };
            live += 1;
            let what = format!("relationship {id}");
            if rec.rel_type as usize >= self.type_counts.len() {
                self.rep.error(format!("{what} has unknown type {}", rec.rel_type));
            } else {
                self.type_counts[rec.rel_type as usize] += 1;
            }
            for (end, node) in [("start", rec.start), ("end", rec.end)] {
                if !matches!(self.read(|r| r.node(NodeId(node))), Ok(Some(_))) {
                    self.rep.error(format!("{what}: {end} node {node} does not exist"));
                }
            }
            for (dir, seen) in [("outgoing", self.on_out[id as usize]), ("incoming", self.on_in[id as usize])] {
                if seen != 1 {
                    self.rep.error(format!("{what} is on {seen} {dir} lists, expected 1"));
                }
            }
            self.blob_blocks(rec.blob, &what);
            match self.read(|r| r.rel_props(&rec)) {
                Ok(props) => {
                    for (i, ix) in m.indexes.iter().enumerate() {
                        if ix.def.entity == IndexEntity::Relationship
                            && ix.label == rec.rel_type
                            && let Some(k) = ix_entry(ix, &props, id)
                        {
                            self.index_keys[i].push(k);
                        }
                    }
                }
                Err(e) => self.rep.error(format!("{what}: {e}")),
            }
        }
        self.rep.relationships = live;
        if live != m.rel_count {
            self.rep.error(format!(
                "{live} relationships are in use, but the metadata counts {}",
                m.rel_count
            ));
        }
        for (t, (&want, &got)) in m.type_counts.iter().zip(&self.type_counts).enumerate() {
            if want != got {
                self.rep.error(format!(
                    "type `{}` has {got} relationships, but the metadata counts {want}",
                    m.rel_types.name(t as Token)
                ));
            }
        }
    }

    // ---- free space ----

    fn free_lists(&mut self) {
        let m = &self.s.meta;
        // Blob blocks.
        let mut b = m.blobs.free;
        while b != 0 {
            if b as usize >= self.blocks.len() || self.blocks[b as usize] {
                self.rep.error(format!("free blob block {b} is out of range, in use or on a cycle"));
                break;
            }
            self.blocks[b as usize] = true;
            match self.read(|r| r.block(b)) {
                Ok((next, _)) => b = next,
                Err(e) => {
                    self.rep.error(format!("free blob block {b}: {e}"));
                    break;
                }
            }
        }
        let lost = self.blocks.iter().skip(1).filter(|&&u| !u).count();
        if lost > 0 {
            self.rep
                .warning(format!("{lost} blob blocks are neither in use nor free (space is lost, not data)"));
        }
        // Relationship groups.
        let mut g = m.groups.free;
        while g != 0 {
            if g as usize >= self.groups.len() || self.groups[g as usize] {
                self.rep.error(format!("free relationship group {g} is out of range, in use or on a cycle"));
                break;
            }
            self.groups[g as usize] = true;
            let (k, o) = locate(GROUPS, g, GROUP_REC);
            match self.read(|r| r.p.read(k, |b| (b[o] & 1 == 1, get64(b, o + 8)))) {
                Ok((false, next)) => g = next,
                Ok((true, _)) => {
                    self.rep.error(format!("free relationship group {g} is marked in use"));
                    break;
                }
                Err(e) => {
                    self.rep.error(format!("free relationship group {g}: {e}"));
                    break;
                }
            }
        }
        let lost = self.groups.iter().skip(1).filter(|&&u| !u).count();
        if lost > 0 {
            self.rep
                .warning(format!("{lost} relationship groups are neither in use nor free"));
        }
    }

    // ---- indexes ----

    fn indexes(&mut self) {
        let m = &self.s.meta;
        let mut pages = vec![false; m.index_pages.next as usize];
        let mut expected = std::mem::take(&mut self.label_keys);
        expected.sort_unstable();
        if let Some(found) = self.tree("the label index", m.label_tree, &mut pages) {
            self.rep.index_entries += found.len() as u64;
            self.compare("the label index", &expected, &found);
        }
        for (i, ix) in m.indexes.iter().enumerate() {
            let what = format!("index `{}`", ix.def.name);
            let mut expected = std::mem::take(&mut self.index_keys[i]);
            expected.sort_unstable();
            let Some(found) = self.tree(&what, ix.root, &mut pages) else { continue };
            self.rep.index_entries += found.len() as u64;
            self.compare(&what, &expected, &found);
            self.index_counts(&what, ix, &found);
        }
        let mut p = m.index_pages.free;
        while p != 0 {
            if p as usize >= pages.len() || pages[p as usize] {
                self.rep.error(format!("free index page {p} is out of range, in use or on a cycle"));
                break;
            }
            pages[p as usize] = true;
            match self.read(|r| btree::free_next(r.p, p)) {
                Ok(next) => p = next,
                Err(e) => {
                    self.rep.error(format!("free index page {p}: {e}"));
                    break;
                }
            }
        }
        let lost = pages.iter().skip(1).filter(|&&u| !u).count();
        if lost > 0 {
            self.rep.warning(format!("{lost} index pages are neither in use nor free"));
        }
    }

    fn index_counts(&mut self, what: &str, ix: &IndexMeta, found: &[Vec<u8>]) {
        let mut distinct = 0u64;
        let mut last: Option<&[u8]> = None;
        for k in found {
            let value = &k[..k.len().saturating_sub(8)];
            if last != Some(value) {
                distinct += 1;
                last = Some(value);
            }
        }
        if ix.entries != found.len() as u64 || ix.distinct != distinct {
            self.rep.error(format!(
                "{what} has {} entries and {distinct} distinct values, but the metadata counts {} and {}",
                found.len(),
                ix.entries,
                ix.distinct
            ));
        }
    }

    /// Reports the keys missing from or extra in an index (both lists sorted).
    fn compare(&mut self, what: &str, expected: &[Vec<u8>], found: &[Vec<u8>]) {
        let (mut missing, mut extra) = (0u64, 0u64);
        let (mut i, mut j) = (0, 0);
        while i < expected.len() || j < found.len() {
            match (expected.get(i), found.get(j)) {
                (Some(a), Some(b)) if a == b => (i, j) = (i + 1, j + 1),
                (Some(a), Some(b)) if a < b => (missing, i) = (missing + 1, i + 1),
                (Some(_), None) => (missing, i) = (missing + 1, i + 1),
                _ => (extra, j) = (extra + 1, j + 1),
            }
        }
        if missing > 0 {
            self.rep.error(format!("{what} lacks {missing} entries"));
        }
        if extra > 0 {
            self.rep.error(format!("{what} has {extra} entries for missing or changed entities"));
        }
    }

    /// Walks a B+tree from `root`, checking page use, key order and leaf links. Returns
    /// its keys in order, or `None` if the tree is unreadable.
    fn tree(&mut self, what: &str, root: u64, pages: &mut [bool]) -> Option<Vec<Vec<u8>>> {
        if root == 0 {
            return Some(Vec::new());
        }
        let mut keys = Vec::new();
        let mut leaves: Vec<(u64, u64)> = Vec::new(); // (page, next)
        let mut leaf_depth = None;
        // (page, depth, lower bound, upper bound), visited depth first, left to right.
        let mut stack: Vec<TreeStep> = vec![(root, 0, None, None)];
        while let Some((page, depth, low, high)) = stack.pop() {
            if page == 0 || page as usize >= pages.len() {
                self.rep.error(format!("{what}: page {page} is out of range"));
                return None;
            }
            if pages[page as usize] {
                self.rep.error(format!("{what}: page {page} is used twice"));
                return None;
            }
            pages[page as usize] = true;
            let v = match self.read(|r| btree::view(r.p, page)) {
                Ok(v) => v,
                Err(e) => {
                    self.rep.error(format!("{what}: {e}"));
                    return None;
                }
            };
            if v.keys.windows(2).any(|w| w[0] >= w[1]) {
                self.rep.error(format!("{what}: keys of page {page} are out of order"));
            }
            let in_bounds = v.keys.iter().all(|k| {
                low.as_ref().is_none_or(|l| k >= l) && high.as_ref().is_none_or(|h| k < h)
            });
            if !in_bounds {
                self.rep.error(format!("{what}: page {page} has keys outside its parent's range"));
            }
            if v.leaf {
                if *leaf_depth.get_or_insert(depth) != depth {
                    self.rep.error(format!("{what}: leaves at different depths"));
                }
                leaves.push((page, v.next));
                keys.extend(v.keys);
            } else {
                if v.children.len() != v.keys.len() + 1 {
                    self.rep.error(format!("{what}: page {page} has the wrong number of children"));
                    return None;
                }
                // Pushed right to left, so they pop left to right.
                for c in (0..v.children.len()).rev() {
                    let lo = if c == 0 { low.clone() } else { Some(v.keys[c - 1].clone()) };
                    let hi = if c == v.keys.len() { high.clone() } else { Some(v.keys[c].clone()) };
                    stack.push((v.children[c], depth + 1, lo, hi));
                }
            }
        }
        for (i, &(page, next)) in leaves.iter().enumerate() {
            let want = leaves.get(i + 1).map_or(0, |l| l.0);
            if next != want {
                self.rep
                    .error(format!("{what}: leaf {page} links to {next}, expected {want}"));
            }
        }
        Some(keys)
    }

    // ---- constraints ----

    fn constraint_node(&mut self, id: u64, labels: &[Token], props: &BTreeMap<Token, Value>) {
        let m = &self.s.meta;
        for (i, c) in m.constraints.iter().enumerate() {
            let (Some(label), keys) = &self.constraint_tokens[i] else { continue };
            if !labels.contains(label) {
                continue;
            }
            let values: Vec<Option<&Value>> = keys.iter().map(|k| k.and_then(|k| props.get(&k))).collect();
            let complete = values.iter().all(Option::is_some);
            match c.kind {
                ConstraintKind::NotNull | ConstraintKind::NodeKey if !complete => {
                    self.rep.error(format!(
                        "node {id} breaks constraint `{}`: it lacks a required property",
                        c.name
                    ));
                }
                _ => {}
            }
            if c.kind != ConstraintKind::NotNull && complete {
                let values: Vec<&Value> = values.into_iter().flatten().collect();
                let (key, _) = index_value_key(&values);
                let normal: Vec<Value> = values.iter().map(|v| index_key(v)).collect();
                self.constraint_seen[i].entry(key).or_default().push((id, normal));
            }
        }
    }

    fn constraints(&mut self) {
        let m = &self.s.meta;
        for (i, c) in m.constraints.iter().enumerate() {
            for group in self.constraint_seen[i].values() {
                for (a, (ida, va)) in group.iter().enumerate() {
                    if let Some((idb, _)) = group[a + 1..].iter().find(|(_, vb)| vb == va) {
                        self.rep.error(format!(
                            "nodes {ida} and {idb} break constraint `{}`: equal values",
                            c.name
                        ));
                        break;
                    }
                }
            }
        }
    }
}

fn link_name(link: u64) -> String {
    if link == 0 { "none".into() } else { format!("relationship {}", link - 1) }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;

    use super::super::{NODE_REC, PersistentGraph, PersistentOptions, REL_REC};
    use crate::maintenance::BACKUP_MARKER;
    use crate::store::{ConstraintDef, ConstraintKind, Direction, GraphStore, IndexDef};
    use crate::{HybridGraph, HybridOptions, MemoryGraph, NodeId, RelId, Value};

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("graphbuff-maint-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            Dir(d)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn props(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    type Dump = (
        Vec<(u64, Vec<String>, BTreeMap<String, Value>)>,
        Vec<(u64, u64, u64, String, BTreeMap<String, Value>)>,
    );

    /// Every node and relationship with its contents, plus every node's degree.
    fn dump(g: &dyn GraphStore) -> Dump {
        let mut nodes = Vec::new();
        for n in g.all_nodes() {
            let n = n.unwrap();
            let mut labels = g.node_labels(n).unwrap();
            labels.sort();
            let mut p = g.node_properties(n).unwrap();
            p.insert("$deg".into(), Value::Int(g.degree(n, Direction::Both, &[]).unwrap() as i64));
            nodes.push((n.0, labels, p));
        }
        let mut rels = Vec::new();
        for r in g.all_relationships() {
            let r = r.unwrap();
            let (s, e) = g.relationship_endpoints(r).unwrap().unwrap();
            rels.push((
                r.0,
                s.0,
                e.0,
                g.relationship_type(r).unwrap().unwrap(),
                g.relationship_properties(r).unwrap(),
            ));
        }
        (nodes, rels)
    }

    /// A store with indexes, a constraint, a dense node, deletions (free space) and long
    /// property values.
    fn build(g: &mut PersistentGraph) -> Vec<NodeId> {
        g.create_index(IndexDef::node("pi", "P", "i")).unwrap();
        g.create_index(IndexDef::relationship("rw", "R", "w")).unwrap();
        g.create_constraint(ConstraintDef {
            name: "pk".into(),
            kind: ConstraintKind::Unique,
            label: "P".into(),
            properties: vec!["i".into()],
        })
        .unwrap();
        g.begin().unwrap();
        let hub = g.create_node(&["Hub".into()], props(&[])).unwrap();
        let mut ids = vec![hub];
        for i in 0..400 {
            let p = props(&[("i", Value::Int(i)), ("s", Value::String("x".repeat(i as usize % 300)))]);
            let n = g.create_node(&["P".into()], p).unwrap();
            let t = if i % 2 == 0 { "R" } else { "S" };
            g.create_relationship(hub, n, t, props(&[("w", Value::Int(i % 7))])).unwrap();
            if i % 5 == 0 {
                g.create_relationship(n, n, "SELF", props(&[])).unwrap();
            }
            ids.push(n);
        }
        g.commit().unwrap();
        for &n in ids.iter().skip(1).step_by(9) {
            g.delete_node(n, true).unwrap();
        }
        ids
    }

    fn opts(dir: &Dir) -> PersistentOptions {
        PersistentOptions::new(&dir.0).cache_bytes(16 * 8192).dense_threshold(50)
    }

    #[test]
    fn a_fresh_and_a_busy_store_are_consistent() {
        let dir = Dir::new("consistent");
        let mut g = PersistentGraph::open(opts(&dir)).unwrap();
        let empty = g.check().unwrap();
        assert!(empty.is_consistent(), "{empty}");
        build(&mut g);
        let r = g.check().unwrap();
        assert!(r.is_consistent(), "{r}");
        assert_eq!(r.nodes, g.node_count() as u64);
        assert_eq!(r.relationships, g.relationship_count() as u64);
        assert!(r.index_entries > 0);
        assert!(r.warnings.is_empty(), "{r}");
        drop(g);
        let g = PersistentGraph::open(opts(&dir)).unwrap();
        let r = g.check().unwrap();
        assert!(r.is_consistent(), "{r}");
    }

    #[test]
    fn backup_of_a_snapshot_ignores_later_commits() {
        let dir = Dir::new("online");
        let out = Dir::new("online-backup");
        let mut g = PersistentGraph::open(opts(&dir)).unwrap();
        let ids = build(&mut g);
        let snap = g.snapshot().unwrap();
        let want = dump(&*snap);

        // Later commits change pages, grow files and reach the data files (checkpoint),
        // all of which the backup must not see.
        g.begin().unwrap();
        let alive: Vec<NodeId> = ids.iter().copied().filter(|&n| g.node_exists(n).unwrap()).collect();
        for &n in alive.iter().skip(1).step_by(4) {
            g.set_node_property(n, "s", Value::String("changed".repeat(40))).unwrap();
        }
        for i in 0..300 {
            let n = g.create_node(&["New".into()], props(&[("k", Value::Int(i))])).unwrap();
            g.create_relationship(ids[0], n, "R", props(&[("w", Value::Int(1))])).unwrap();
        }
        g.commit().unwrap();
        g.delete_node(ids[4], true).unwrap();
        g.checkpoint().unwrap();

        let info = snap.backup(&out.0).unwrap();
        assert_eq!(info.format, "persistent");
        assert_eq!(info.nodes, want.0.len() as u64);
        assert!(!out.0.join(BACKUP_MARKER).exists());
        drop(snap);

        let b = PersistentGraph::open(PersistentOptions::new(&out.0)).unwrap();
        assert_eq!(dump(&b), want);
        let r = b.check().unwrap();
        assert!(r.is_consistent(), "{r}");
        // Indexes and the constraint came along.
        assert_eq!(b.index_seek("P", "i", &Value::Int(4)).unwrap().unwrap().len(), 1);
        assert_eq!(b.constraints().len(), 1);

        // The live store is unaffected, and a backup of the store itself sees its state.
        let out2 = Dir::new("online-backup-2");
        g.backup(&out2.0).unwrap();
        let live = dump(&g);
        drop(g);
        let b2 = PersistentGraph::open(PersistentOptions::new(&out2.0)).unwrap();
        assert_eq!(dump(&b2), live);
    }

    #[test]
    fn backups_need_an_empty_directory_and_must_finish() {
        let dir = Dir::new("marker");
        let out = Dir::new("marker-backup");
        let g = PersistentGraph::open(opts(&dir)).unwrap();
        fs::create_dir_all(&out.0).unwrap();
        fs::write(out.0.join("something"), b"x").unwrap();
        assert!(g.backup(&out.0).unwrap_err().to_string().contains("not empty"));

        fs::remove_file(out.0.join("something")).unwrap();
        fs::write(out.0.join(BACKUP_MARKER), b"").unwrap();
        let e = PersistentGraph::open(PersistentOptions::new(&out.0)).unwrap_err();
        assert!(e.to_string().contains("did not finish"), "{e}");
        let e = HybridGraph::open(HybridOptions::new(&out.0)).unwrap_err();
        assert!(e.to_string().contains("did not finish"), "{e}");
    }

    #[test]
    fn memory_and_hybrid_stores_back_up_as_hybrid_stores() {
        let out = Dir::new("mem-backup");
        let mut m = MemoryGraph::new();
        m.create_index(IndexDef::node("ax", "A", "x")).unwrap();
        let a = m.create_node(&["A".into()], props(&[("x", Value::Int(1))])).unwrap();
        m.create_relationship(a, a, "L", props(&[])).unwrap();
        // An open transaction's changes are left out.
        m.begin().unwrap();
        m.create_node(&["A".into()], props(&[])).unwrap();
        let info = m.backup(&out.0).unwrap();
        m.rollback();
        assert_eq!((info.format, info.nodes, info.relationships), ("hybrid", 1, 1));
        let h = HybridGraph::open(HybridOptions::new(&out.0)).unwrap();
        assert_eq!(dump(&h), dump(&m));
        assert_eq!(h.indexes().len(), 1);
        assert!(h.check().is_err(), "checks are for persistent stores");
    }

    /// Overwrites `len` bytes at `offset` of a store file with `f(old bytes)`.
    fn patch(path: PathBuf, offset: u64, len: usize, f: impl FnOnce(&mut [u8])) {
        let mut file = fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        let mut buf = vec![0u8; len];
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.read_exact(&mut buf).unwrap();
        f(&mut buf);
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&buf).unwrap();
    }

    fn corrupted(name: &str, damage: impl FnOnce(&Dir, &[NodeId], &[RelId])) -> crate::CheckReport {
        let dir = Dir::new(name);
        let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
        g.create_index(IndexDef::node("ax", "A", "x")).unwrap();
        let mut nodes = Vec::new();
        let mut rels = Vec::new();
        for i in 0..5 {
            nodes.push(g.create_node(&["A".into()], props(&[("x", Value::Int(i))])).unwrap());
        }
        for w in nodes.windows(2) {
            rels.push(g.create_relationship(w[0], w[1], "N", props(&[])).unwrap());
            rels.push(g.create_relationship(w[0], w[1], "N", props(&[])).unwrap());
        }
        drop(g); // checkpoints, so the files hold everything
        damage(&dir, &nodes, &rels);
        let g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
        g.check().unwrap()
    }

    #[test]
    fn check_finds_damage() {
        // A broken back link in an adjacency list (prev_out at byte 24).
        let r = corrupted("prev", |d, _, rels| {
            patch(d.0.join("rels.db"), rels[1].0 * REL_REC as u64 + 24, 8, |b| b.fill(0));
        });
        assert!(!r.is_consistent());
        assert!(r.errors.iter().any(|e| e.contains("previous outgoing link")), "{r}");

        // A node record wiped out under its relationships.
        let r = corrupted("node", |d, nodes, _| {
            patch(d.0.join("nodes.db"), nodes[2].0 * NODE_REC as u64, NODE_REC, |b| b.fill(0));
        });
        assert!(r.errors.iter().any(|e| e.contains("does not exist")), "{r}");
        assert!(r.errors.iter().any(|e| e.contains("metadata counts")), "{r}");
        assert!(r.errors.iter().any(|e| e.contains("the label index has")), "{r}");

        // A wrong degree (out_deg at byte 48).
        let r = corrupted("degree", |d, nodes, _| {
            patch(d.0.join("nodes.db"), nodes[0].0 * NODE_REC as u64 + 48, 4, |b| b[0] += 1);
        });
        assert!(r.errors.iter().any(|e| e.contains("but the degree is 3")), "{r}");
        assert_eq!(r.error_count, r.errors.len() as u64);
    }
}
