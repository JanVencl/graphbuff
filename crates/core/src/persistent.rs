//! Persistent storage: the graph lives in paged files on disk, and only a bounded page
//! cache is held in memory, so graphs larger than RAM work.
//!
//! Directory layout:
//!
//! ```text
//! <dir>/LOCK       held for as long as the store is open (one process at a time)
//! <dir>/nodes.db   fixed 64-byte node records, at offset id * 64
//! <dir>/rels.db    fixed 64-byte relationship records, at offset id * 64
//! <dir>/blobs.db   128-byte blocks holding labels and properties, chained per entity
//! <dir>/index.db   B+tree pages: the label index and the property indexes
//! <dir>/groups.db  fixed 64-byte relationship group records of dense nodes, at id * 64
//! <dir>/meta       "GBMETA10", metadata (see below), CRC-32
//! <dir>/graph.wal  "GBPWAL10", then records: len u32 | crc u32 | metadata, page images
//! ```
//!
//! The metadata is small and kept in memory: id counters, entity counts, the token tables
//! for labels, relationship types and property keys, per-label and per-type counts,
//! free-space lists, the index definitions with their B+tree roots, and the constraint
//! definitions.
//!
//! ## Records
//! A node record holds a pointer to its blob (labels and properties) and the heads and
//! tails of two doubly linked lists threaded through the relationship records: its
//! outgoing and its incoming relationships, kept in creation order. Each relationship
//! record holds its type, its endpoints, its previous and next relationship in both
//! lists, and its own property blob. Deleting a relationship unlinks it in O(1).
//!
//! ## Dense nodes
//! Once a node has more than [`PersistentOptions::dense_threshold`] relationships it
//! becomes *dense*: its relationships are regrouped by type, and the node record points to
//! a chain of group records (in type order) instead of to two lists. Each group holds the
//! two lists of one type (threaded through the relationship records exactly as a sparse
//! node's are) and their degrees, including self-loops. A typed expansion of a dense node
//! then walks only the matching groups, and every degree is read from group records.
//! A node never becomes sparse again.
//!
//! ## Format versions
//! Version 2 added dense nodes (`groups.db`, a dense flag in node records, the group
//! allocator in the metadata). A version 1 store opens as a version 2 store without dense
//! nodes; its log is replayed and replaced, and the next checkpoint writes `GBMETAv2`.
//! Nodes become dense on their next new relationship. Version 1 software rejects the
//! upgraded files.
//!
//! Version 3 sorted the index keys and version 4 added index counts; older indexes are
//! rebuilt on open. Version 5 added constraint definitions to the metadata. Older
//! metadata reads as having none, and the log is replaced on open as above. Version 6
//! added relationship and composite indexes: an index definition names its entity and
//! lists its properties. Older definitions read as single-property node indexes, whose
//! keys are unchanged. Version 7 added temporal property values (a new value tag); its
//! metadata is laid out as in version 6, and older stores open unchanged. Version 8 added
//! point values (another value tag), laid out the same way. Version 9 added vector index
//! definitions at the end of the metadata (their vectors are kept in memory by the
//! engine); older metadata reads as having none, and an older log is replaced on open.
//! Version 10 added full-text index definitions after them, likewise.
//!
//! ## Index keys
//! A single-property index entry is the value's sort key, its exact encoding (or a lossy
//! prefix and checksum for a long value), then the entity id; equality seeks scan a
//! prefix and range seeks scan between sort keys. A composite entry is the exact (or
//! lossy) encoding of each value in turn, then the id; it only answers equality seeks
//! on all of its properties.
//!
//! ## Bulk import
//! [`BulkLoader`] fills a new store without the log: see [`bulk`].
//!
//! ## Durability
//! Writes go through the page cache ([`pager`]). A commit appends the metadata and the
//! after-image of every changed page to the redo log as one checksummed record; with
//! [`SyncPolicy::EveryCommit`] the log is fsynced before `commit` returns. A checkpoint
//! (explicit, automatic once the log passes `checkpoint_after_bytes`, and on close)
//! writes the dirty pages to the data files, fsyncs them, replaces `meta`, and empties
//! the log. On open, log records newer than `meta` are re-applied, and a torn last
//! record is ignored.

mod btree;
mod bulk;
mod maintain;
mod pager;

pub use bulk::BulkLoader;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LockResult, Mutex, MutexGuard};

use btree::{MAX_KEY, PageAlloc};
use pager::{
    DataFile, MAX_LOGGED_PAGES, PAGE_SIZE, PageKey, Pager, WAL_MAGIC, WAL_MAGIC_V1, WAL_MAGIC_V2, WAL_MAGIC_V3,
    WAL_MAGIC_V4, WAL_MAGIC_V5, WAL_MAGIC_V6, WAL_MAGIC_V7, WAL_MAGIC_V8, WAL_MAGIC_V9,
};

use crate::codec::{Decoder, Encoder, crc32};
use crate::hybrid::SyncPolicy;
use crate::memory::{Interner, Token, check_storable, index_key, missing_node, missing_rel};
use crate::store::{
    Adjacency, ConstraintDef, Direction, Expansion, GraphStore, IndexDef, IndexEntity, IndexStats, Scan,
    Snapshot, ValueRange,
};
use crate::{Error, NodeId, RelId, Result, Value};

const NODES: u8 = 0;
const RELS: u8 = 1;
const BLOBS: u8 = 2;
const GROUPS: u8 = 4;
const FILE_NAMES: [&str; 5] = ["nodes.db", "rels.db", "blobs.db", "index.db", "groups.db"];
const META_FILE: &str = "meta";
const META_TMP: &str = "meta.tmp";
const META_MAGIC: &[u8; 8] = b"GBMETA10";
const META_MAGIC_V9: &[u8; 8] = b"GBMETAv9";
const META_MAGIC_V8: &[u8; 8] = b"GBMETAv8";
const META_MAGIC_V7: &[u8; 8] = b"GBMETAv7";
const META_MAGIC_V6: &[u8; 8] = b"GBMETAv6";
const META_MAGIC_V5: &[u8; 8] = b"GBMETAv5";
const META_MAGIC_V4: &[u8; 8] = b"GBMETAv4";
const META_MAGIC_V3: &[u8; 8] = b"GBMETAv3";
const META_MAGIC_V2: &[u8; 8] = b"GBMETAv2";
const META_MAGIC_V1: &[u8; 8] = b"GBMETAv1";
/// Version of the property indexes: 1 = unsorted keys (before storage format 3), 2 =
/// sorted keys (see [`value_key`]), 3 = sorted keys plus entry and distinct-value counts
/// (storage format 4). Older indexes are rebuilt when the store is opened.
const INDEX_FORMAT: u8 = 3;
/// The storage format this code writes.
const FORMAT: u8 = 10;
const WAL_FILE: &str = "graph.wal";
const LOCK_FILE: &str = "LOCK";

const NODE_REC: usize = 64;
const REL_REC: usize = 64;
const GROUP_REC: usize = 64;
const BLOCK: usize = 128;
const BLOCK_PAYLOAD: usize = BLOCK - 8;
/// Encoded property values longer than this are indexed by a lossy prefix + checksum.
const MAX_INLINE_VALUE: usize = 256;

/// Default [`PersistentOptions::max_transaction_bytes`]: 1 GiB.
pub const DEFAULT_MAX_TRANSACTION_BYTES: u64 = 1 << 30;

/// Settings for a [`PersistentGraph`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistentOptions {
    /// Directory holding the data files and the log. Created if missing.
    pub dir: PathBuf,
    pub sync: SyncPolicy,
    /// Size of the page cache. The cache grows past it only while a transaction that
    /// changes more pages than fit is open.
    pub cache_bytes: u64,
    /// Take a checkpoint automatically once the log grows beyond this many bytes.
    /// `0` disables automatic checkpoints (they still happen on close).
    pub checkpoint_after_bytes: u64,
    /// A node with more relationships than this becomes dense: its relationships are
    /// grouped by type (see the module docs). Default 50.
    pub dense_threshold: u32,
    /// The most a transaction may change, in bytes of pages (rounded down to whole
    /// 8 KiB pages, at least one). A transaction keeps every page it changes in memory,
    /// plus its before-image, until it ends; changing one page more fails the statement
    /// with [`Error::TransactionTooLarge`] and rolls the transaction back. Default 1 GiB.
    /// `0` means only the hard limit of one log record (about 4 GiB).
    pub max_transaction_bytes: u64,
}

impl PersistentOptions {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        PersistentOptions {
            dir: dir.into(),
            sync: SyncPolicy::default(),
            cache_bytes: 64 << 20,
            checkpoint_after_bytes: 64 << 20,
            dense_threshold: 50,
            max_transaction_bytes: DEFAULT_MAX_TRANSACTION_BYTES,
        }
    }

    pub fn max_transaction_bytes(mut self, bytes: u64) -> Self {
        self.max_transaction_bytes = bytes;
        self
    }

    /// [`max_transaction_bytes`](Self::max_transaction_bytes) in pages.
    fn max_tx_pages(&self) -> usize {
        match self.max_transaction_bytes {
            0 => MAX_LOGGED_PAGES,
            b => usize::try_from(b / PAGE_SIZE as u64).unwrap_or(usize::MAX).clamp(1, MAX_LOGGED_PAGES),
        }
    }

    pub fn dense_threshold(mut self, relationships: u32) -> Self {
        self.dense_threshold = relationships;
        self
    }

    pub fn sync(mut self, sync: SyncPolicy) -> Self {
        self.sync = sync;
        self
    }

    pub fn cache_bytes(mut self, bytes: u64) -> Self {
        self.cache_bytes = bytes;
        self
    }

    pub fn checkpoint_after_bytes(mut self, bytes: u64) -> Self {
        self.checkpoint_after_bytes = bytes;
        self
    }
}

// ---- metadata -------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct IndexMeta {
    def: IndexDef,
    /// Label token (node index) or relationship type token.
    label: Token,
    /// Property key tokens, in the definition's order.
    keys: Vec<Token>,
    root: u64,
    /// Entries (indexed entities) and distinct values (format 4).
    entries: u64,
    distinct: u64,
}

#[derive(Clone, Debug, PartialEq, Default)]
struct Meta {
    /// Sequence number of the last committed transaction.
    seq: u64,
    next_node: u64,
    next_rel: u64,
    node_count: u64,
    rel_count: u64,
    labels: Interner,
    rel_types: Interner,
    keys: Interner,
    /// Nodes per label token.
    label_counts: Vec<u64>,
    /// Relationships per type token.
    type_counts: Vec<u64>,
    /// Block allocator of `blobs.db`.
    blobs: PageAlloc,
    /// Page allocator of `index.db`.
    index_pages: PageAlloc,
    /// Root of the label index (`label u32 BE | node u64 BE`).
    label_tree: u64,
    indexes: Vec<IndexMeta>,
    /// Record allocator of `groups.db` (format version 2).
    groups: PageAlloc,
    /// Key layout of the property indexes ([`INDEX_FORMAT`] once rebuilt).
    index_format: u8,
    /// Constraint definitions (format version 5).
    constraints: Vec<ConstraintDef>,
    /// Vector (format version 9) and full-text (10) index definitions: the engine keeps
    /// their contents.
    engine_indexes: Vec<IndexDef>,
}

impl Meta {
    fn encode(&self, e: &mut Encoder) {
        self.encode_version(e, FORMAT);
    }

    /// Encodes the metadata as storage format `version` writes it (older versions for
    /// upgrade tests).
    fn encode_version(&self, e: &mut Encoder, version: u8) {
        for v in [
            self.seq,
            self.next_node,
            self.next_rel,
            self.node_count,
            self.rel_count,
        ] {
            e.u64(v);
        }
        e.strings(&self.labels.names);
        e.strings(&self.rel_types.names);
        e.strings(&self.keys.names);
        for counts in [&self.label_counts, &self.type_counts] {
            e.len(counts.len());
            for &c in counts {
                e.u64(c);
            }
        }
        for a in [self.blobs, self.index_pages] {
            e.u64(a.next);
            e.u64(a.free);
        }
        e.u64(self.label_tree);
        e.len(self.indexes.len());
        for ix in &self.indexes {
            if version >= 6 {
                ix.def.encode(e);
                e.u32(ix.label);
                e.len(ix.keys.len());
                for &k in &ix.keys {
                    e.u32(k);
                }
            } else {
                e.str(&ix.def.name);
                e.str(&ix.def.label);
                e.str(&ix.def.properties[0]);
                e.u32(ix.label);
                e.u32(ix.keys[0]);
            }
            e.u64(ix.root);
            if version >= 4 {
                e.u64(ix.entries);
                e.u64(ix.distinct);
            }
        }
        if version >= 2 {
            e.u64(self.groups.next);
            e.u64(self.groups.free);
        }
        if version >= 3 {
            e.u8(self.index_format);
        }
        if version >= 5 {
            ConstraintDef::encode_list(&self.constraints, e);
        }
        if version >= 9 {
            IndexDef::encode_vector_list(&self.engine_indexes, e);
        }
        if version >= 10 {
            IndexDef::encode_fulltext_list(&self.engine_indexes, e);
        }
    }

    /// Decodes metadata written by storage format `version` (1 to 10).
    fn decode(d: &mut Decoder, version: u8) -> Result<Meta> {
        let mut m = Meta {
            seq: d.u64()?,
            next_node: d.u64()?,
            next_rel: d.u64()?,
            node_count: d.u64()?,
            rel_count: d.u64()?,
            ..Meta::default()
        };
        m.labels = Interner::from_names(d.strings()?, d)?;
        m.rel_types = Interner::from_names(d.strings()?, d)?;
        m.keys = Interner::from_names(d.strings()?, d)?;
        for counts in [&mut m.label_counts, &mut m.type_counts] {
            let n = d.len(8)?;
            *counts = (0..n).map(|_| d.u64()).collect::<Result<_>>()?;
        }
        if m.label_counts.len() != m.labels.names.len()
            || m.type_counts.len() != m.rel_types.names.len()
        {
            return Err(d.corrupt("token counts do not match the token tables"));
        }
        for a in [&mut m.blobs, &mut m.index_pages] {
            a.next = d.u64()?;
            a.free = d.u64()?;
        }
        m.label_tree = d.u64()?;
        let n = d.len(32)?;
        for _ in 0..n {
            let (def, label, keys) = if version >= 6 {
                let def = IndexDef::decode(d)?;
                let label = d.u32()?;
                let n = d.len(4)?;
                let keys: Vec<Token> = (0..n).map(|_| d.u32()).collect::<Result<_>>()?;
                (def, label, keys)
            } else {
                let def = IndexDef::decode_v1(d)?;
                let label = d.u32()?;
                (def, label, vec![d.u32()?])
            };
            let labels = match def.entity {
                IndexEntity::Node => &m.labels,
                IndexEntity::Relationship => &m.rel_types,
            };
            if label as usize >= labels.names.len()
                || keys.len() != def.properties.len()
                || keys.iter().any(|&k| k as usize >= m.keys.names.len())
            {
                return Err(d.corrupt("index token out of range"));
            }
            let root = d.u64()?;
            let (entries, distinct) = if version >= 4 { (d.u64()?, d.u64()?) } else { (0, 0) };
            m.indexes.push(IndexMeta {
                def,
                label,
                keys,
                root,
                entries,
                distinct,
            });
        }
        if version >= 2 {
            m.groups.next = d.u64()?;
            m.groups.free = d.u64()?;
            if m.groups.next == 0 || m.groups.free >= m.groups.next {
                return Err(d.corrupt("group allocator out of range"));
            }
        }
        m.index_format = if version >= 3 { d.u8()? } else { 1 };
        if version >= 5 {
            m.constraints = ConstraintDef::decode_list(d)?;
        }
        if version >= 9 {
            m.engine_indexes = IndexDef::decode_vector_list(d)?;
        }
        if version >= 10 {
            m.engine_indexes.extend(IndexDef::decode_fulltext_list(d)?);
        }
        Ok(m)
    }

    fn intern_label(&mut self, name: &str) -> Token {
        let t = self.labels.intern(name);
        if self.label_counts.len() <= t as usize {
            self.label_counts.resize(t as usize + 1, 0);
        }
        t
    }

    fn intern_type(&mut self, name: &str) -> Token {
        let t = self.rel_types.intern(name);
        if self.type_counts.len() <= t as usize {
            self.type_counts.resize(t as usize + 1, 0);
        }
        t
    }

    fn find_index(&self, entity: IndexEntity, label: &str, properties: &[String]) -> Option<&IndexMeta> {
        self.indexes.iter().find(|ix| ix.def.covers(entity, label, properties))
    }

    fn token_names(&self, table: &Interner, counts: &[u64]) -> Vec<String> {
        let mut out: Vec<String> = counts
            .iter()
            .enumerate()
            .filter(|(_, c)| **c > 0)
            .map(|(t, _)| table.name(t as Token).to_string())
            .collect();
        out.sort();
        out
    }
}

// ---- records ----------------------------------------------------------------------------

/// Relationship links store `id + 1`, so that `0` means "none".
///
/// A dense node keeps its first group id in `first_out` and zero in the other links; its
/// degrees are the totals over its groups.
#[derive(Clone, Copy, Debug, Default)]
struct NodeRec {
    dense: bool,
    blob: u64,
    first_out: u64,
    last_out: u64,
    first_in: u64,
    last_in: u64,
    out_deg: u32,
    in_deg: u32,
}

#[derive(Clone, Copy, Debug, Default)]
struct RelRec {
    rel_type: Token,
    start: u64,
    end: u64,
    prev_out: u64,
    next_out: u64,
    prev_in: u64,
    next_in: u64,
    blob: u64,
}

fn get64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Page and byte offset of fixed-size record `id`.
fn locate(file: u8, id: u64, size: usize) -> (PageKey, usize) {
    let per = (PAGE_SIZE / size) as u64;
    (
        PageKey {
            file,
            page: id / per,
        },
        (id % per) as usize * size,
    )
}

impl NodeRec {
    fn decode(b: &[u8]) -> Option<NodeRec> {
        (b[0] & 1 == 1).then(|| NodeRec {
            dense: b[0] & 2 == 2,
            blob: get64(b, 8),
            first_out: get64(b, 16),
            last_out: get64(b, 24),
            first_in: get64(b, 32),
            last_in: get64(b, 40),
            out_deg: get32(b, 48),
            in_deg: get32(b, 52),
        })
    }

    fn encode(&self, b: &mut [u8]) {
        b.fill(0);
        b[0] = 1 | if self.dense { 2 } else { 0 };
        put64(b, 8, self.blob);
        put64(b, 16, self.first_out);
        put64(b, 24, self.last_out);
        put64(b, 32, self.first_in);
        put64(b, 40, self.last_in);
        put32(b, 48, self.out_deg);
        put32(b, 52, self.in_deg);
    }
}

impl RelRec {
    fn decode(b: &[u8]) -> Option<RelRec> {
        (b[0] & 1 == 1).then(|| RelRec {
            rel_type: get32(b, 4),
            start: get64(b, 8),
            end: get64(b, 16),
            prev_out: get64(b, 24),
            next_out: get64(b, 32),
            prev_in: get64(b, 40),
            next_in: get64(b, 48),
            blob: get64(b, 56),
        })
    }

    fn encode(&self, b: &mut [u8]) {
        b.fill(0);
        b[0] = 1;
        put32(b, 4, self.rel_type);
        put64(b, 8, self.start);
        put64(b, 16, self.end);
        put64(b, 24, self.prev_out);
        put64(b, 32, self.next_out);
        put64(b, 40, self.prev_in);
        put64(b, 48, self.next_in);
        put64(b, 56, self.blob);
    }
}

/// A relationship group of a dense node: its relationships of one type, as an outgoing
/// and an incoming list threaded through the relationship records. Group ids start at 1,
/// so `0` means "none". A free record holds the next free id at byte 8.
#[derive(Clone, Copy, Debug, Default)]
struct GroupRec {
    rel_type: Token,
    /// Next group of the same node (higher type token).
    next: u64,
    first_out: u64,
    last_out: u64,
    first_in: u64,
    last_in: u64,
    out_deg: u32,
    in_deg: u32,
    /// Self-loops among the outgoing relationships (each is incoming as well).
    loops: u32,
}

impl GroupRec {
    fn decode(b: &[u8]) -> Option<GroupRec> {
        (b[0] & 1 == 1).then(|| GroupRec {
            rel_type: get32(b, 4),
            next: get64(b, 8),
            first_out: get64(b, 16),
            last_out: get64(b, 24),
            first_in: get64(b, 32),
            last_in: get64(b, 40),
            out_deg: get32(b, 48),
            in_deg: get32(b, 52),
            loops: get32(b, 56),
        })
    }

    fn encode(&self, b: &mut [u8]) {
        b.fill(0);
        b[0] = 1;
        put32(b, 4, self.rel_type);
        put64(b, 8, self.next);
        put64(b, 16, self.first_out);
        put64(b, 24, self.last_out);
        put64(b, 32, self.first_in);
        put64(b, 40, self.last_in);
        put32(b, 48, self.out_deg);
        put32(b, 52, self.in_deg);
        put32(b, 56, self.loops);
    }

    fn matches(&self, wanted: &Option<Vec<Token>>) -> bool {
        wanted.as_ref().is_none_or(|t| t.contains(&self.rel_type))
    }

    fn degree(&self, dir: Direction) -> usize {
        (match dir {
            Direction::Outgoing => self.out_deg,
            Direction::Incoming => self.in_deg,
            Direction::Both => self.out_deg + self.in_deg - self.loops,
        }) as usize
    }
}

/// Who holds the head and tail of one of a node's adjacency lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    /// A sparse node: its own record.
    Node(u64),
    /// A dense node: its group for one type.
    Group { node: u64, group: u64 },
}

fn add_degree(d: &mut u32, delta: i32) -> Result<()> {
    *d = d
        .checked_add_signed(delta)
        .ok_or_else(|| corrupt("relationship degree out of range"))?;
    Ok(())
}

type Props = BTreeMap<Token, Value>;

/// The contents of a node's blob.
#[derive(Clone, Debug, Default, PartialEq)]
struct NodeData {
    labels: Vec<Token>,
    props: Props,
}

fn encode_props(e: &mut Encoder, props: &Props) {
    e.len(props.len());
    for (k, v) in props {
        e.u32(*k);
        e.value(v);
    }
}

fn encode_node(nd: &NodeData) -> Vec<u8> {
    if nd.labels.is_empty() && nd.props.is_empty() {
        return Vec::new();
    }
    let mut e = Encoder::new();
    e.len(nd.labels.len());
    for &l in &nd.labels {
        e.u32(l);
    }
    encode_props(&mut e, &nd.props);
    e.buf
}

fn encode_rel_props(props: &Props) -> Vec<u8> {
    if props.is_empty() {
        return Vec::new();
    }
    let mut e = Encoder::new();
    encode_props(&mut e, props);
    e.buf
}

fn decode_token(d: &mut Decoder, table: &Interner) -> Result<Token> {
    let t = d.u32()?;
    if (t as usize) < table.names.len() {
        Ok(t)
    } else {
        Err(d.corrupt(format!("token {t} is out of range")))
    }
}

fn decode_props(d: &mut Decoder, keys: &Interner) -> Result<Props> {
    let n = d.len(5)?;
    let mut props = Props::new();
    for _ in 0..n {
        let k = decode_token(d, keys)?;
        props.insert(k, d.value()?);
    }
    Ok(props)
}

/// Label index key: `label u32 BE | node u64 BE`, so a label's nodes are contiguous and
/// in id order.
fn label_key(label: Token, node: u64) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[..4].copy_from_slice(&label.to_be_bytes());
    k[4..].copy_from_slice(&node.to_be_bytes());
    k
}

/// Property index key prefix of a value: its Cypher-equality normal form, encoded. The
/// encoding is self-delimiting, so no value's prefix is a prefix of another's. Long
/// values are shortened to a marker, a prefix and a checksum (`lossy`); seeks then
/// re-check their candidates.
/// Kinds of values in index keys; numbers and strings are stored in their sort order.
const KEY_NUMBER: u8 = 0x10;
const KEY_STRING: u8 = 0x20;
const KEY_OTHER: u8 = 0x30;
/// Bytes of a string that its sort key covers; longer strings share a key prefix.
const KEY_STRING_PREFIX: usize = 128;

/// Bytes that sort like the `f64`.
fn sortable_f64(f: f64) -> [u8; 8] {
    let b = f.to_bits();
    let b = if b >> 63 == 1 { !b } else { b | (1 << 63) };
    b.to_be_bytes()
}

/// The sort part of a string's key: its first bytes with `0x00` escaped as `00 FF`, then
/// `00 00`, or `00 01` if the string was cut. Shorter strings sort first, so cut keys
/// still sort like their strings (equal prefixes tie, and callers re-check).
fn push_string_key(s: &str, terminate: bool, out: &mut Vec<u8>) {
    let bytes = s.as_bytes();
    let cut = bytes.len() > KEY_STRING_PREFIX;
    for &b in &bytes[..bytes.len().min(KEY_STRING_PREFIX)] {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    if terminate {
        out.extend_from_slice(&[0, u8::from(cut)]);
    }
}

/// The part of a (normalized) value's key that sorts: numbers by value (as `f64`, so
/// integers beyond 2^53 share keys with neighbours), strings by their first bytes.
fn sort_key(v: &Value) -> Vec<u8> {
    match v {
        Value::Int(i) => [&[KEY_NUMBER][..], &sortable_f64(*i as f64)].concat(),
        Value::Float(f) => [&[KEY_NUMBER][..], &sortable_f64(*f)].concat(),
        Value::String(s) => {
            let mut k = vec![KEY_STRING];
            push_string_key(s, true, &mut k);
            k
        }
        _ => vec![KEY_OTHER],
    }
}

/// The key of a property value in an index: its sort key, then the exact value (or, for
/// a long value, a lossy prefix plus checksum). The node id follows in an entry key.
/// Returns whether the exact part is lossy, so a seek must re-check candidates.
fn value_key(v: &Value) -> (Vec<u8>, bool) {
    let v = index_key(v);
    let mut k = sort_key(&v);
    let mut e = Encoder::new();
    e.value(&v);
    if e.buf.len() <= MAX_INLINE_VALUE {
        k.extend_from_slice(&e.buf);
        return (k, false);
    }
    k.push(0xFE); // not a value tag
    k.extend_from_slice(&e.buf[..200]);
    k.extend_from_slice(&crc32(&e.buf).to_le_bytes());
    k.extend_from_slice(&(e.buf.len() as u32).to_le_bytes());
    (k, true)
}

/// The keys a range seek scans: `start..end` covers every entry whose value is in the
/// range, plus entries with equal sort keys. `None` if nothing can match.
fn range_keys(range: &ValueRange) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    match range {
        ValueRange::Prefix(p) => {
            let mut start = vec![KEY_STRING];
            push_string_key(p, false, &mut start);
            let end = btree::successor(&start);
            Some((start, end))
        }
        ValueRange::Between { lower, upper } => {
            let numeric = range.numeric()?;
            let value = |b: &Bound<Value>| match b {
                Bound::Included(v) | Bound::Excluded(v) => Some(index_key(v)),
                Bound::Unbounded => None,
            };
            let kind = [if numeric { KEY_NUMBER } else { KEY_STRING }];
            let start = value(lower).map_or_else(|| kind.to_vec(), |v| sort_key(&v));
            let end = match value(upper) {
                Some(v) => btree::successor(&sort_key(&v)),
                None => btree::successor(&kind),
            };
            Some((start, end))
        }
    }
}

fn label_key_node(k: &[u8]) -> u64 {
    u64::from_be_bytes(k[4..12].try_into().unwrap())
}

/// The key of a combination of property values in a composite index: each value's exact
/// encoding in turn, or, when it is long, a marker, a prefix, a checksum and the length
/// (fixed size, so the parts stay self-delimiting). Each part is limited so that the
/// whole key fits a B+tree key. Returns whether a part is lossy.
fn tuple_key(values: &[&Value]) -> (Vec<u8>, bool) {
    let budget = (MAX_KEY - 8) / values.len().max(1) - 9;
    let mut k = Vec::new();
    let mut lossy = false;
    for v in values {
        let mut e = Encoder::new();
        e.value(&index_key(v));
        if e.buf.len() <= budget {
            k.extend_from_slice(&e.buf);
        } else {
            lossy = true;
            k.push(0xFE); // not a value tag
            k.extend_from_slice(&e.buf[..budget - 8]);
            k.extend_from_slice(&crc32(&e.buf).to_le_bytes());
            k.extend_from_slice(&(e.buf.len() as u32).to_le_bytes());
        }
    }
    (k, lossy)
}

/// The key prefix an index uses for `values` (one per property), and whether it is lossy.
fn index_value_key(values: &[&Value]) -> (Vec<u8>, bool) {
    match values {
        [v] => value_key(v),
        _ => tuple_key(values),
    }
}

/// The entry key of an entity with `props` in `ix`, or `None` if it lacks a property.
fn ix_entry(ix: &IndexMeta, props: &Props, id: u64) -> Option<Vec<u8>> {
    let mut values = Vec::with_capacity(ix.keys.len());
    for k in &ix.keys {
        values.push(props.get(k)?);
    }
    let (mut k, _) = index_value_key(&values);
    k.extend_from_slice(&id.to_be_bytes());
    debug_assert!(k.len() <= MAX_KEY);
    Some(k)
}

fn corrupt(what: impl std::fmt::Display) -> Error {
    Error::Storage(format!("persistent store is corrupt: {what}"))
}

// ---- scans --------------------------------------------------------------------------------

/// Keys read from an index per lock of the page cache during a scan.
const SCAN_BATCH: usize = 512;

/// A scan that reads in batches. Each batch locks the page cache only while `fill` runs,
/// so the caller may make other reads (which lock it too) between items.
struct Batched<'a, T, F> {
    pager: &'a Shared,
    /// The snapshot version read, if any (see [`Pager::read_at`]).
    at: Option<u64>,
    /// Appends the next batch to the buffer; returns whether more batches may follow.
    fill: F,
    buf: Vec<T>,
    pos: usize,
    more: bool,
}

impl<'a, T, F> Batched<'a, T, F> {
    fn new(pager: &'a Shared, at: Option<u64>, fill: F) -> Self {
        Batched {
            pager,
            at,
            fill,
            buf: Vec::new(),
            pos: 0,
            more: true,
        }
    }
}

impl<T: Copy, F: FnMut(&mut Pager, &mut Vec<T>) -> Result<bool>> Iterator for Batched<'_, T, F> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Result<T>> {
        loop {
            if let Some(&id) = self.buf.get(self.pos) {
                self.pos += 1;
                return Some(Ok(id));
            }
            if !self.more {
                return None;
            }
            self.buf.clear();
            self.pos = 0;
            let mut p = lock_at(self.pager, self.at);
            match (self.fill)(&mut p, &mut self.buf) {
                Ok(more) => self.more = more,
                Err(e) => {
                    self.more = false;
                    return Some(Err(e));
                }
            }
        }
    }
}

/// The page cache of a store, shared with its snapshots.
struct Shared {
    pager: Mutex<Pager>,
    /// Snapshot readers waiting for the lock.
    waiting: AtomicUsize,
}

impl Shared {
    fn lock(&self) -> LockResult<MutexGuard<'_, Pager>> {
        self.pager.lock()
    }

    /// Locks for the writer, first yielding once if snapshot readers are waiting: the
    /// lock is not fair, and a writer that locks once per change would otherwise keep it
    /// from them for a whole transaction.
    fn lock_for_write(&self) -> MutexGuard<'_, Pager> {
        if self.waiting.load(Ordering::Relaxed) > 0 {
            std::thread::yield_now();
        }
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The pager, locked, reading at version `at` (see [`Pager::read_at`]) until unlocked.
struct AtGuard<'a>(MutexGuard<'a, Pager>);

fn lock_at(pager: &Shared, at: Option<u64>) -> AtGuard<'_> {
    let mut g = if at.is_some() {
        pager.waiting.fetch_add(1, Ordering::Relaxed);
        let g = pager.lock();
        pager.waiting.fetch_sub(1, Ordering::Relaxed);
        g
    } else {
        pager.lock()
    }
    .unwrap_or_else(|e| e.into_inner());
    g.read_at = at;
    AtGuard(g)
}

impl Drop for AtGuard<'_> {
    fn drop(&mut self) {
        self.0.read_at = None;
    }
}

impl std::ops::Deref for AtGuard<'_> {
    type Target = Pager;

    fn deref(&self) -> &Pager {
        &self.0
    }
}

impl std::ops::DerefMut for AtGuard<'_> {
    fn deref_mut(&mut self) -> &mut Pager {
        &mut self.0
    }
}

/// Live record ids of a record file, one page per batch.
fn record_scan(file: u8, size: usize, count: u64) -> impl FnMut(&mut Pager, &mut Vec<u64>) -> Result<bool> {
    let per = (PAGE_SIZE / size) as u64;
    let mut page = 0u64;
    move |p, out| {
        if page * per >= count {
            return Ok(false);
        }
        p.read(PageKey { file, page }, |b| {
            for slot in 0..per {
                let id = page * per + slot;
                if id >= count {
                    break;
                }
                if b[slot as usize * size] & 1 == 1 {
                    out.push(id);
                }
            }
        })?;
        page += 1;
        Ok(page * per < count)
    }
}

/// Nodes carrying a label, in id order, from the label index.
fn label_scan(root: u64, label: Token) -> impl FnMut(&mut Pager, &mut Vec<u64>) -> Result<bool> {
    let mut cursor = btree::Cursor::new(root, &label.to_be_bytes());
    let mut keys = Vec::new();
    move |p, out| {
        keys.clear();
        cursor.next_batch(p, SCAN_BATCH, &mut keys)?;
        out.extend(keys.iter().map(|k| label_key_node(k)));
        Ok(!cursor.done())
    }
}

/// Relationship records read per lock of the page cache during an expansion.
const EXPAND_BATCH: usize = 256;

/// The relationships of `node` in `dir` whose type is in `wanted` (`None` = any), read by
/// walking its adjacency lists a batch at a time: all outgoing lists (one per matching
/// group of a dense node), then the incoming ones. With [`Direction::Both`] a self-loop
/// is reported once, as outgoing.
fn adjacency_walk(
    m: &Meta,
    node: NodeId,
    dir: Direction,
    wanted: Option<Vec<Token>>,
) -> impl FnMut(&mut Pager, &mut Vec<Expansion>) -> Result<bool> + '_ {
    let want_out = matches!(dir, Direction::Outgoing | Direction::Both);
    let mut lists: Option<std::vec::IntoIter<(u64, bool)>> = None;
    let mut outgoing = true;
    let mut link = 0u64;
    // Links followed on the current list, to detect a cycle in a corrupt file.
    let mut walked = 0u64;
    move |p, out| {
        let mut r = Rd { p, m };
        if lists.is_none() {
            let Some(n) = r.node(node)? else {
                return Ok(false);
            };
            lists = Some(r.lists(&n, dir, &wanted)?.into_iter());
        }
        let mut steps = 0;
        loop {
            if link == 0 {
                match lists.as_mut().and_then(Iterator::next) {
                    Some((head, o)) => {
                        (link, outgoing, walked) = (head, o, 0);
                        continue;
                    }
                    None => return Ok(false),
                }
            }
            if steps == EXPAND_BATCH {
                return Ok(true);
            }
            walked += 1;
            if walked > m.rel_count {
                return Err(corrupt("adjacency list has a cycle"));
            }
            steps += 1;
            let rec = r.live_rel(link)?;
            let rel = RelId(link - 1);
            link = if outgoing { rec.next_out } else { rec.next_in };
            if !outgoing && want_out && rec.start == rec.end {
                continue; // self-loop already reported as outgoing
            }
            if wanted.as_ref().is_none_or(|t| t.contains(&rec.rel_type)) {
                let other = if outgoing { rec.end } else { rec.start };
                out.push(Expansion {
                    rel,
                    other: NodeId(other),
                });
            }
        }
    }
}

// ---- reading ------------------------------------------------------------------------------

/// Read access: the (locked) page cache and the metadata.
struct Rd<'a> {
    p: &'a mut Pager,
    m: &'a Meta,
}

impl Rd<'_> {
    fn node(&mut self, id: NodeId) -> Result<Option<NodeRec>> {
        if id.0 >= self.m.next_node {
            return Ok(None);
        }
        let (k, o) = locate(NODES, id.0, NODE_REC);
        self.p.read(k, |b| NodeRec::decode(&b[o..o + NODE_REC]))
    }

    fn rel(&mut self, id: RelId) -> Result<Option<RelRec>> {
        if id.0 >= self.m.next_rel {
            return Ok(None);
        }
        let (k, o) = locate(RELS, id.0, REL_REC);
        self.p.read(k, |b| RelRec::decode(&b[o..o + REL_REC]))
    }

    fn live_rel(&mut self, link: u64) -> Result<RelRec> {
        self.rel(RelId(link - 1))?
            .ok_or_else(|| corrupt(format!("adjacency points at missing relationship {}", link - 1)))
    }

    fn group(&mut self, g: u64) -> Result<GroupRec> {
        if g == 0 || g >= self.m.groups.next {
            return Err(corrupt(format!("relationship group {g} is out of range")));
        }
        let (k, o) = locate(GROUPS, g, GROUP_REC);
        self.p
            .read(k, |b| GroupRec::decode(&b[o..o + GROUP_REC]))?
            .ok_or_else(|| corrupt(format!("relationship group {g} is not in use")))
    }

    /// The groups of a dense node, in type order.
    fn groups(&mut self, n: &NodeRec) -> Result<Vec<(u64, GroupRec)>> {
        let mut out = Vec::new();
        let mut g = if n.dense { n.first_out } else { 0 };
        while g != 0 {
            if out.len() as u64 >= self.m.groups.next {
                return Err(corrupt("relationship group chain has a cycle"));
            }
            let rec = self.group(g)?;
            out.push((g, rec));
            g = rec.next;
        }
        Ok(out)
    }

    /// Heads of the lists an expansion of `n` in `dir` walks, outgoing ones first, with
    /// whether each is an outgoing list. For a dense node, only the matching groups.
    fn lists(&mut self, n: &NodeRec, dir: Direction, wanted: &Option<Vec<Token>>) -> Result<Vec<(u64, bool)>> {
        let want_out = matches!(dir, Direction::Outgoing | Direction::Both);
        let want_in = matches!(dir, Direction::Incoming | Direction::Both);
        let heads: Vec<(u64, u64)> = if n.dense {
            self.groups(n)?
                .into_iter()
                .filter(|(_, g)| g.matches(wanted))
                .map(|(_, g)| (g.first_out, g.first_in))
                .collect()
        } else {
            vec![(n.first_out, n.first_in)]
        };
        let mut out = Vec::with_capacity(heads.len() * 2);
        if want_out {
            out.extend(heads.iter().map(|h| (h.0, true)));
        }
        if want_in {
            out.extend(heads.iter().map(|h| (h.1, false)));
        }
        Ok(out)
    }

    /// The degree of `node` if it can be read from records without walking a list:
    /// always for a dense node, for a sparse one only untyped in one direction.
    fn quick_degree(&mut self, node: NodeId, dir: Direction, wanted: &Option<Vec<Token>>) -> Result<Option<usize>> {
        let Some(n) = self.node(node)? else {
            return Ok(Some(0));
        };
        if n.dense {
            let groups = self.groups(&n)?;
            return Ok(Some(
                groups
                    .iter()
                    .filter(|(_, g)| g.matches(wanted))
                    .map(|(_, g)| g.degree(dir))
                    .sum(),
            ));
        }
        Ok(match (dir, wanted) {
            (Direction::Outgoing, None) => Some(n.out_deg as usize),
            (Direction::Incoming, None) => Some(n.in_deg as usize),
            _ => None,
        })
    }

    /// Every relationship attached to `n` (self-loops once), in no particular order.
    fn attached(&mut self, n: &NodeRec) -> Result<Vec<RelId>> {
        let mut out = Vec::new();
        for (head, outgoing) in self.lists(n, Direction::Both, &None)? {
            out.extend(self.chain(head, outgoing)?.into_iter().map(|(r, _)| r));
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    fn block(&mut self, b: u64) -> Result<(u64, [u8; BLOCK_PAYLOAD])> {
        if b >= self.m.blobs.next {
            return Err(corrupt(format!("blob block {b} is out of range")));
        }
        let (k, o) = locate(BLOBS, b, BLOCK);
        self.p.read(k, |p| {
            let mut payload = [0u8; BLOCK_PAYLOAD];
            payload.copy_from_slice(&p[o + 8..o + BLOCK]);
            (get64(p, o), payload)
        })
    }

    fn blob(&mut self, ptr: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut total = 0usize;
        let mut b = ptr;
        let mut first = true;
        while b != 0 {
            let (next, payload) = self.block(b)?;
            if first {
                total = get32(&payload, 0) as usize;
                out.reserve(total);
                out.extend_from_slice(&payload[4..]);
                first = false;
            } else {
                out.extend_from_slice(&payload);
            }
            if out.len() >= total {
                out.truncate(total);
                return Ok(out);
            }
            b = next;
        }
        if ptr != 0 {
            return Err(corrupt(format!("blob {ptr} ends early")));
        }
        Ok(out)
    }

    fn node_data(&mut self, rec: &NodeRec) -> Result<NodeData> {
        let bytes = self.blob(rec.blob)?;
        if bytes.is_empty() {
            return Ok(NodeData::default());
        }
        let what = format!("node blob {}", rec.blob);
        let mut d = Decoder::new(&bytes, &what);
        let n = d.len(4)?;
        let labels = (0..n)
            .map(|_| decode_token(&mut d, &self.m.labels))
            .collect::<Result<_>>()?;
        let props = decode_props(&mut d, &self.m.keys)?;
        Ok(NodeData { labels, props })
    }

    fn rel_props(&mut self, rec: &RelRec) -> Result<Props> {
        let bytes = self.blob(rec.blob)?;
        if bytes.is_empty() {
            return Ok(Props::new());
        }
        let what = format!("relationship blob {}", rec.blob);
        decode_props(&mut Decoder::new(&bytes, &what), &self.m.keys)
    }

    /// The relationships of one adjacency list, in order.
    fn chain(&mut self, first: u64, outgoing: bool) -> Result<Vec<(RelId, RelRec)>> {
        let mut out = Vec::new();
        let mut link = first;
        while link != 0 {
            if out.len() as u64 > self.m.rel_count {
                return Err(corrupt("adjacency list has a cycle"));
            }
            let r = self.live_rel(link)?;
            out.push((RelId(link - 1), r));
            link = if outgoing { r.next_out } else { r.next_in };
        }
        Ok(out)
    }

    /// The properties of node or relationship `id`, or `None` if it does not exist.
    fn entity_props(&mut self, entity: IndexEntity, id: u64) -> Result<Option<Props>> {
        Ok(match entity {
            IndexEntity::Node => match self.node(NodeId(id))? {
                Some(rec) => Some(self.node_data(&rec)?.props),
                None => None,
            },
            IndexEntity::Relationship => match self.rel(RelId(id))? {
                Some(rec) => Some(self.rel_props(&rec)?),
                None => None,
            },
        })
    }

    fn index_seek(
        &mut self,
        entity: IndexEntity,
        label: &str,
        properties: &[String],
        values: &[Value],
    ) -> Result<Option<Vec<u64>>> {
        let Some(ix) = self.m.find_index(entity, label, properties) else {
            return Ok(None);
        };
        if values.len() != properties.len() {
            return Err(Error::Runtime("an index seek needs one value per property".into()));
        }
        if values.iter().any(Value::is_null) {
            return Ok(Some(Vec::new()));
        }
        let (root, keys) = (ix.root, ix.keys.clone());
        let refs: Vec<&Value> = values.iter().collect();
        let (prefix, lossy) = index_value_key(&refs);
        let mut ids = Vec::new();
        btree::scan_prefix(self.p, root, &prefix, |k| {
            ids.push(u64::from_be_bytes(k[k.len() - 8..].try_into().unwrap()));
        })?;
        if lossy {
            let want: Vec<Value> = values.iter().map(index_key).collect();
            let mut kept = Vec::with_capacity(ids.len());
            for id in ids {
                let Some(props) = self.entity_props(entity, id)? else { continue };
                let got: Vec<Option<Value>> = keys.iter().map(|k| props.get(k).map(index_key)).collect();
                if got.iter().zip(&want).all(|(g, w)| g.as_ref() == Some(w)) {
                    kept.push(id);
                }
            }
            ids = kept;
        }
        Ok(Some(ids))
    }

    fn index_range(
        &mut self,
        entity: IndexEntity,
        label: &str,
        property: &str,
        range: &ValueRange,
    ) -> Result<Option<Vec<u64>>> {
        let Some(ix) = self.m.find_index(entity, label, &[property.to_string()]) else {
            return Ok(None);
        };
        let Some((start, end)) = range_keys(range) else {
            return Ok(Some(Vec::new()));
        };
        let mut ids = Vec::new();
        btree::scan_range(self.p, ix.root, &start, end.as_deref(), |k| {
            ids.push(u64::from_be_bytes(k[k.len() - 8..].try_into().unwrap()));
        })?;
        ids.sort_unstable();
        ids.dedup();
        Ok(Some(ids))
    }
}

// ---- writing ------------------------------------------------------------------------------

/// Write access, inside an open transaction.
struct Tx<'a> {
    p: &'a mut Pager,
    m: &'a mut Meta,
    dense_threshold: u32,
}

impl Tx<'_> {
    fn rd(&mut self) -> Rd<'_> {
        Rd {
            p: &mut *self.p,
            m: &*self.m,
        }
    }

    fn write_node_rec(&mut self, id: u64, rec: Option<&NodeRec>) -> Result<()> {
        let (k, o) = locate(NODES, id, NODE_REC);
        let b = &mut self.p.write(k)?[o..o + NODE_REC];
        match rec {
            Some(r) => r.encode(b),
            None => b.fill(0),
        }
        Ok(())
    }

    fn write_rel_rec(&mut self, id: u64, rec: Option<&RelRec>) -> Result<()> {
        let (k, o) = locate(RELS, id, REL_REC);
        let b = &mut self.p.write(k)?[o..o + REL_REC];
        match rec {
            Some(r) => r.encode(b),
            None => b.fill(0),
        }
        Ok(())
    }

    fn node(&mut self, id: u64) -> Result<NodeRec> {
        self.rd()
            .node(NodeId(id))?
            .ok_or_else(|| corrupt(format!("relationship endpoint {id} is missing")))
    }

    // ---- relationship groups and lists ----

    fn write_group(&mut self, g: u64, rec: &GroupRec) -> Result<()> {
        let (k, o) = locate(GROUPS, g, GROUP_REC);
        rec.encode(&mut self.p.write(k)?[o..o + GROUP_REC]);
        Ok(())
    }

    fn alloc_group(&mut self) -> Result<u64> {
        let a = &mut self.m.groups;
        if a.free == 0 {
            a.next += 1;
            return Ok(a.next - 1);
        }
        let g = a.free;
        let (k, o) = locate(GROUPS, g, GROUP_REC);
        let next = self.p.read(k, |b| get64(b, o + 8))?;
        if next >= self.m.groups.next {
            return Err(corrupt("group free list out of range"));
        }
        self.m.groups.free = next;
        Ok(g)
    }

    fn free_group(&mut self, g: u64) -> Result<()> {
        let (k, o) = locate(GROUPS, g, GROUP_REC);
        let page = self.p.write(k)?;
        page[o..o + GROUP_REC].fill(0);
        put64(page, o + 8, self.m.groups.free);
        self.m.groups.free = g;
        Ok(())
    }

    /// The owner of `node`'s lists for type `t`: the node if it is sparse, else its group
    /// for `t`. A missing group is created in type order if `create`, else it is an error.
    fn owner(&mut self, node: u64, t: Token, create: bool) -> Result<Owner> {
        let n = self.node(node)?;
        if !n.dense {
            return Ok(Owner::Node(node));
        }
        let (mut prev, mut g, mut steps) = (0u64, n.first_out, 0u64);
        while g != 0 {
            let rec = self.rd().group(g)?;
            if rec.rel_type == t {
                return Ok(Owner::Group { node, group: g });
            }
            if rec.rel_type > t {
                break;
            }
            steps += 1;
            if steps >= self.m.groups.next {
                return Err(corrupt("relationship group chain has a cycle"));
            }
            (prev, g) = (g, rec.next);
        }
        if !create {
            return Err(corrupt(format!("node {node} has no relationship group for type {t}")));
        }
        let new = self.alloc_group()?;
        self.write_group(
            new,
            &GroupRec {
                rel_type: t,
                next: g,
                ..GroupRec::default()
            },
        )?;
        if prev == 0 {
            let mut n = self.node(node)?;
            n.first_out = new;
            self.write_node_rec(node, Some(&n))?;
        } else {
            let mut p = self.rd().group(prev)?;
            p.next = new;
            self.write_group(prev, &p)?;
        }
        Ok(Owner::Group { node, group: new })
    }

    /// `(first, last)` of the outgoing (`out`) or incoming list of `o`.
    fn ends(&mut self, o: Owner, out: bool) -> Result<(u64, u64)> {
        Ok(match o {
            Owner::Node(n) => {
                let r = self.node(n)?;
                if out { (r.first_out, r.last_out) } else { (r.first_in, r.last_in) }
            }
            Owner::Group { group, .. } => {
                let g = self.rd().group(group)?;
                if out { (g.first_out, g.last_out) } else { (g.first_in, g.last_in) }
            }
        })
    }

    /// Sets the ends of a list of `o` and adds `delta` to its degree; for a group, to the
    /// node's total degree as well.
    fn set_ends(&mut self, o: Owner, out: bool, ends: (u64, u64), delta: i32) -> Result<()> {
        let node = match o {
            Owner::Node(n) => n,
            Owner::Group { node, group } => {
                let mut g = self.rd().group(group)?;
                if out {
                    (g.first_out, g.last_out) = ends;
                    add_degree(&mut g.out_deg, delta)?;
                } else {
                    (g.first_in, g.last_in) = ends;
                    add_degree(&mut g.in_deg, delta)?;
                }
                self.write_group(group, &g)?;
                node
            }
        };
        let mut r = self.node(node)?;
        if let Owner::Node(_) = o {
            if out {
                (r.first_out, r.last_out) = ends;
            } else {
                (r.first_in, r.last_in) = ends;
            }
        }
        add_degree(if out { &mut r.out_deg } else { &mut r.in_deg }, delta)?;
        self.write_node_rec(node, Some(&r))
    }

    /// Appends relationship `id` (its record already written) to a list of `o`.
    fn append(&mut self, o: Owner, id: u64, out: bool) -> Result<()> {
        let link = id + 1;
        let (first, last) = self.ends(o, out)?;
        let mut r = self.rd().live_rel(link)?;
        if out {
            (r.prev_out, r.next_out) = (last, 0);
        } else {
            (r.prev_in, r.next_in) = (last, 0);
        }
        self.write_rel_rec(id, Some(&r))?;
        if last != 0 {
            let mut p = self.rd().live_rel(last)?;
            if out {
                p.next_out = link;
            } else {
                p.next_in = link;
            }
            self.write_rel_rec(last - 1, Some(&p))?;
        }
        self.set_ends(o, out, (if first == 0 { link } else { first }, link), 1)
    }

    /// Unlinks relationship `id` from a list of `o`. Records are re-read before they are
    /// changed, because a self-loop sits on both lists of its node.
    fn unlink(&mut self, o: Owner, id: u64, out: bool) -> Result<()> {
        let r = self.rd().live_rel(id + 1)?;
        let (prev, next) = if out { (r.prev_out, r.next_out) } else { (r.prev_in, r.next_in) };
        if prev != 0 {
            let mut x = self.rd().live_rel(prev)?;
            if out {
                x.next_out = next;
            } else {
                x.next_in = next;
            }
            self.write_rel_rec(prev - 1, Some(&x))?;
        }
        if next != 0 {
            let mut x = self.rd().live_rel(next)?;
            if out {
                x.prev_out = prev;
            } else {
                x.prev_in = prev;
            }
            self.write_rel_rec(next - 1, Some(&x))?;
        }
        let (first, last) = self.ends(o, out)?;
        let ends = (
            if prev == 0 { next } else { first },
            if next == 0 { prev } else { last },
        );
        self.set_ends(o, out, ends, -1)
    }

    fn add_loops(&mut self, o: Owner, delta: i32) -> Result<()> {
        if let Owner::Group { group, .. } = o {
            let mut g = self.rd().group(group)?;
            add_degree(&mut g.loops, delta)?;
            self.write_group(group, &g)?;
        }
        Ok(())
    }

    /// Removes a group that no longer holds relationships from its node's chain.
    fn drop_group_if_empty(&mut self, o: Owner) -> Result<()> {
        let Owner::Group { node, group } = o else {
            return Ok(());
        };
        let g = self.rd().group(group)?;
        if g.out_deg > 0 || g.in_deg > 0 {
            return Ok(());
        }
        let mut n = self.node(node)?;
        if n.first_out == group {
            n.first_out = g.next;
            self.write_node_rec(node, Some(&n))?;
        } else {
            let prev = self
                .rd()
                .groups(&n)?
                .into_iter()
                .find(|(_, p)| p.next == group)
                .ok_or_else(|| corrupt(format!("relationship group {group} is not linked")))?;
            let mut p = prev.1;
            p.next = g.next;
            self.write_group(prev.0, &p)?;
        }
        self.free_group(group)
    }

    /// Turns a sparse node with more than `dense_threshold` relationships into a dense
    /// one, by re-appending its relationships (in their order) to per-type groups.
    fn densify_if_needed(&mut self, node: u64) -> Result<()> {
        let n = self.node(node)?;
        if n.dense || u64::from(n.out_deg) + u64::from(n.in_deg) <= u64::from(self.dense_threshold) {
            return Ok(());
        }
        let outs = self.rd().chain(n.first_out, true)?;
        let ins = self.rd().chain(n.first_in, false)?;
        let mut d = n;
        d.dense = true;
        (d.first_out, d.last_out, d.first_in, d.last_in) = (0, 0, 0, 0);
        (d.out_deg, d.in_deg) = (0, 0); // re-counted by `append`
        self.write_node_rec(node, Some(&d))?;
        for (rel, r) in outs {
            let o = self.owner(node, r.rel_type, true)?;
            self.append(o, rel.0, true)?;
        }
        for (rel, r) in ins {
            let o = self.owner(node, r.rel_type, true)?;
            self.append(o, rel.0, false)?;
            if r.start == r.end {
                self.add_loops(o, 1)?;
            }
        }
        Ok(())
    }

    // ---- blobs ----

    fn alloc_block(&mut self) -> Result<u64> {
        let a = &mut self.m.blobs;
        if a.free == 0 {
            a.next += 1;
            return Ok(a.next - 1);
        }
        let b = a.free;
        let (k, o) = locate(BLOBS, b, BLOCK);
        let next = self.p.read(k, |p| get64(p, o))?;
        self.m.blobs.free = next;
        Ok(b)
    }

    fn free_block(&mut self, b: u64) -> Result<()> {
        let (k, o) = locate(BLOBS, b, BLOCK);
        let page = self.p.write(k)?;
        page[o..o + BLOCK].fill(0);
        put64(page, o, self.m.blobs.free);
        self.m.blobs.free = b;
        Ok(())
    }

    fn block_ids(&mut self, first: u64) -> Result<Vec<u64>> {
        let mut ids = Vec::new();
        let mut b = first;
        while b != 0 {
            if ids.len() as u64 >= self.m.blobs.next {
                return Err(corrupt("blob chain has a cycle"));
            }
            ids.push(b);
            b = self.rd().block(b)?.0;
        }
        Ok(ids)
    }

    /// Replaces the blob at `old` (`0` = none) with `data`, reusing its blocks.
    /// Returns the new blob pointer (`0` for empty data).
    fn write_blob(&mut self, old: u64, data: &[u8]) -> Result<u64> {
        let old_blocks = self.block_ids(old)?;
        let mut content = Vec::with_capacity(4 + data.len());
        if !data.is_empty() {
            let len = u32::try_from(data.len())
                .map_err(|_| Error::Storage("entity properties exceed 4 GiB".into()))?;
            content.extend_from_slice(&len.to_le_bytes());
            content.extend_from_slice(data);
        }
        let chunks: Vec<&[u8]> = content.chunks(BLOCK_PAYLOAD).collect();
        let mut ids = Vec::with_capacity(chunks.len());
        for i in 0..chunks.len() {
            ids.push(match old_blocks.get(i) {
                Some(&b) => b,
                None => self.alloc_block()?,
            });
        }
        for &b in old_blocks.iter().skip(chunks.len()) {
            self.free_block(b)?;
        }
        for (i, chunk) in chunks.iter().enumerate() {
            let (k, o) = locate(BLOBS, ids[i], BLOCK);
            let page = self.p.write(k)?;
            page[o..o + BLOCK].fill(0);
            put64(page, o, ids.get(i + 1).copied().unwrap_or(0));
            page[o + 8..o + 8 + chunk.len()].copy_from_slice(chunk);
        }
        Ok(ids.first().copied().unwrap_or(0))
    }

    // ---- nodes ----

    fn index_entries(&self, id: u64, nd: &NodeData) -> Vec<(usize, Vec<u8>)> {
        self.m
            .indexes
            .iter()
            .enumerate()
            .filter(|(_, ix)| ix.def.entity == IndexEntity::Node && nd.labels.contains(&ix.label))
            .filter_map(|(i, ix)| ix_entry(ix, &nd.props, id).map(|k| (i, k)))
            .collect()
    }

    /// Whether some relationship index covers relationships of type `t`.
    fn has_rel_index(&self, t: Token) -> bool {
        self.m
            .indexes
            .iter()
            .any(|ix| ix.def.entity == IndexEntity::Relationship && ix.label == t)
    }

    fn rel_index_entries(&self, id: u64, t: Token, props: &Props) -> Vec<(usize, Vec<u8>)> {
        self.m
            .indexes
            .iter()
            .enumerate()
            .filter(|(_, ix)| ix.def.entity == IndexEntity::Relationship && ix.label == t)
            .filter_map(|(i, ix)| ix_entry(ix, props, id).map(|k| (i, k)))
            .collect()
    }

    /// Replaces index entries `before` with `after` (unchanged ones are left alone).
    fn swap_index_entries(&mut self, before: &[(usize, Vec<u8>)], after: &[(usize, Vec<u8>)]) -> Result<()> {
        for e in before.iter().filter(|e| !after.contains(e)) {
            self.index_remove(e.0, &e.1)?;
        }
        for e in after.iter().filter(|e| !before.contains(e)) {
            self.index_add(e.0, &e.1)?;
        }
        Ok(())
    }

    /// Moves node `id` from `old` to `new` contents: label index, label counts,
    /// property indexes and blob. Writes the record (marking it live).
    fn update_node(&mut self, id: u64, rec: &mut NodeRec, old: &NodeData, new: &NodeData) -> Result<()> {
        for &l in &old.labels {
            if !new.labels.contains(&l) {
                btree::remove(self.p, self.m.label_tree, &label_key(l, id))?;
                self.m.label_counts[l as usize] -= 1;
            }
        }
        for &l in &new.labels {
            if !old.labels.contains(&l) {
                btree::insert(
                    self.p,
                    &mut self.m.index_pages,
                    &mut self.m.label_tree,
                    &label_key(l, id),
                )?;
                self.m.label_counts[l as usize] += 1;
            }
        }
        if !self.m.indexes.is_empty() {
            let before = self.index_entries(id, old);
            let after = self.index_entries(id, new);
            self.swap_index_entries(&before, &after)?;
        }
        rec.blob = self.write_blob(rec.blob, &encode_node(new))?;
        self.write_node_rec(id, Some(rec))
    }

    fn intern_props(&mut self, props: BTreeMap<String, Value>) -> Result<Props> {
        for (k, v) in &props {
            check_storable(k, v)?;
        }
        Ok(props
            .into_iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (self.m.keys.intern(&k), v))
            .collect())
    }

    fn create_node(&mut self, labels: &[String], props: BTreeMap<String, Value>) -> Result<NodeId> {
        let props = self.intern_props(props)?;
        let mut tokens: Vec<Token> = Vec::with_capacity(labels.len());
        for l in labels {
            let t = self.m.intern_label(l);
            if !tokens.contains(&t) {
                tokens.push(t);
            }
        }
        let id = self.m.next_node;
        self.m.next_node += 1;
        self.m.node_count += 1;
        let new = NodeData {
            labels: tokens,
            props,
        };
        self.update_node(id, &mut NodeRec::default(), &NodeData::default(), &new)?;
        Ok(NodeId(id))
    }

    fn delete_node(&mut self, id: NodeId, detach: bool) -> Result<bool> {
        let Some(rec) = self.rd().node(id)? else {
            return Ok(false);
        };
        if rec.out_deg > 0 || rec.in_deg > 0 {
            if !detach {
                return Err(Error::Constraint(format!(
                    "cannot delete node {id} because it still has relationships; delete them first or use DETACH DELETE"
                )));
            }
            // Each relationship is unlinked from its other end only; this node's own lists
            // (and groups) go away with it, instead of being maintained one by one.
            let groups = self.rd().groups(&rec)?;
            for r in self.rd().attached(&rec)? {
                self.remove_rel(r, Some(id.0))?;
            }
            for (g, _) in groups {
                self.free_group(g)?;
            }
        }
        let mut rec = self.node(id.0)?;
        let data = self.rd().node_data(&rec)?;
        self.update_node(id.0, &mut rec, &data, &NodeData::default())?;
        self.write_node_rec(id.0, None)?;
        self.m.node_count -= 1;
        Ok(true)
    }

    fn set_node_prop(&mut self, id: NodeId, key: &str, value: Value) -> Result<()> {
        check_storable(key, &value)?;
        let Some(mut rec) = self.rd().node(id)? else {
            return Err(missing_node(id));
        };
        let k = self.m.keys.intern(key);
        let old = self.rd().node_data(&rec)?;
        let mut new = old.clone();
        if value.is_null() {
            new.props.remove(&k);
        } else {
            new.props.insert(k, value);
        }
        if new != old {
            self.update_node(id.0, &mut rec, &old, &new)?;
        }
        Ok(())
    }

    fn add_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        let Some(mut rec) = self.rd().node(id)? else {
            return Err(missing_node(id));
        };
        let t = self.m.intern_label(label);
        let old = self.rd().node_data(&rec)?;
        if old.labels.contains(&t) {
            return Ok(false);
        }
        let mut new = old.clone();
        new.labels.push(t);
        self.update_node(id.0, &mut rec, &old, &new)?;
        Ok(true)
    }

    fn remove_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        let Some(mut rec) = self.rd().node(id)? else {
            return Err(missing_node(id));
        };
        let Some(t) = self.m.labels.get(label) else {
            return Ok(false);
        };
        let old = self.rd().node_data(&rec)?;
        if !old.labels.contains(&t) {
            return Ok(false);
        }
        let mut new = old.clone();
        new.labels.retain(|l| *l != t);
        self.update_node(id.0, &mut rec, &old, &new)?;
        Ok(true)
    }

    // ---- relationships ----

    fn create_rel(
        &mut self,
        start: NodeId,
        end: NodeId,
        rel_type: &str,
        props: BTreeMap<String, Value>,
    ) -> Result<RelId> {
        if self.rd().node(start)?.is_none() {
            return Err(missing_node(start));
        }
        if self.rd().node(end)?.is_none() {
            return Err(missing_node(end));
        }
        let props = self.intern_props(props)?;
        let t = self.m.intern_type(rel_type);
        let id = self.m.next_rel;
        self.m.next_rel += 1;
        self.m.rel_count += 1;
        self.m.type_counts[t as usize] += 1;

        let r = RelRec {
            rel_type: t,
            start: start.0,
            end: end.0,
            blob: self.write_blob(0, &encode_rel_props(&props))?,
            ..RelRec::default()
        };

        self.write_rel_rec(id, Some(&r))?;
        if self.has_rel_index(t) {
            let after = self.rel_index_entries(id, t, &props);
            self.swap_index_entries(&[], &after)?;
        }
        let so = self.owner(start.0, t, true)?;
        self.append(so, id, true)?;
        let eo = self.owner(end.0, t, true)?;
        self.append(eo, id, false)?;
        if start == end {
            self.add_loops(so, 1)?;
        }
        self.densify_if_needed(start.0)?;
        if end != start {
            self.densify_if_needed(end.0)?;
        }
        Ok(RelId(id))
    }

    fn delete_rel(&mut self, id: RelId) -> Result<bool> {
        self.remove_rel(id, None)
    }

    /// Deletes relationship `id`. Its lists on node `dying` (a node being detach-deleted)
    /// are left as they are: the caller discards them as a whole.
    fn remove_rel(&mut self, id: RelId, dying: Option<u64>) -> Result<bool> {
        let Some(r) = self.rd().rel(id)? else {
            return Ok(false);
        };
        let mut owners: Vec<Owner> = Vec::with_capacity(2);
        if dying != Some(r.start) {
            let so = self.owner(r.start, r.rel_type, false)?;
            self.unlink(so, id.0, true)?;
            if r.start == r.end {
                self.add_loops(so, -1)?;
            }
            owners.push(so);
        }
        if dying != Some(r.end) {
            let eo = self.owner(r.end, r.rel_type, false)?;
            self.unlink(eo, id.0, false)?;
            if !owners.contains(&eo) {
                owners.push(eo);
            }
        }
        for o in owners {
            self.drop_group_if_empty(o)?;
        }
        if self.has_rel_index(r.rel_type) {
            let props = self.rd().rel_props(&r)?;
            let before = self.rel_index_entries(id.0, r.rel_type, &props);
            self.swap_index_entries(&before, &[])?;
        }

        self.write_blob(r.blob, &[])?;
        self.write_rel_rec(id.0, None)?;
        self.m.rel_count -= 1;
        self.m.type_counts[r.rel_type as usize] -= 1;
        Ok(true)
    }

    fn set_rel_prop(&mut self, id: RelId, key: &str, value: Value) -> Result<()> {
        check_storable(key, &value)?;
        let Some(mut rec) = self.rd().rel(id)? else {
            return Err(missing_rel(id));
        };
        let k = self.m.keys.intern(key);
        let old = self.rd().rel_props(&rec)?;
        let mut new = old.clone();
        if value.is_null() {
            new.remove(&k);
        } else {
            new.insert(k, value);
        }
        if new != old {
            if self.has_rel_index(rec.rel_type) {
                let before = self.rel_index_entries(id.0, rec.rel_type, &old);
                let after = self.rel_index_entries(id.0, rec.rel_type, &new);
                self.swap_index_entries(&before, &after)?;
            }
            rec.blob = self.write_blob(rec.blob, &encode_rel_props(&new))?;
            self.write_rel_rec(id.0, Some(&rec))?;
        }
        Ok(())
    }

    // ---- indexes ----

    fn create_index(&mut self, def: IndexDef) -> Result<()> {
        let mut existing: Vec<IndexDef> = self.m.indexes.iter().map(|ix| ix.def.clone()).collect();
        existing.extend(self.m.engine_indexes.iter().cloned());
        def.check_new(&existing)?;
        if def.is_vector() || def.is_fulltext() {
            self.m.engine_indexes.push(def);
            return Ok(());
        }
        let entity = def.entity;
        let label = match entity {
            IndexEntity::Node => self.m.intern_label(&def.label),
            IndexEntity::Relationship => self.m.intern_type(&def.label),
        };
        let keys = def.properties.iter().map(|p| self.m.keys.intern(p)).collect();
        self.m.indexes.push(IndexMeta {
            def,
            label,
            keys,
            root: 0,
            entries: 0,
            distinct: 0,
        });
        let i = self.m.indexes.len() - 1;
        if entity == IndexEntity::Relationship {
            for id in 0..self.m.next_rel {
                let Some(rec) = self.rd().rel(RelId(id))? else { continue };
                if rec.rel_type != label {
                    continue;
                }
                let props = self.rd().rel_props(&rec)?;
                if let Some(k) = ix_entry(&self.m.indexes[i], &props, id) {
                    self.index_add(i, &k)?;
                }
            }
            return Ok(());
        }
        // Walk the label in batches; the label tree is not changed while the new index
        // tree is filled, so the cursor stays valid.
        let mut cursor = btree::Cursor::new(self.m.label_tree, &label.to_be_bytes());
        let mut batch = Vec::new();
        while !cursor.done() {
            batch.clear();
            cursor.next_batch(self.p, SCAN_BATCH, &mut batch)?;
            for k in &batch {
                let id = label_key_node(k);
                let Some(rec) = self.rd().node(NodeId(id))? else {
                    return Err(corrupt(format!("label index lists missing node {id}")));
                };
                let props = self.rd().node_data(&rec)?.props;
                if let Some(k) = ix_entry(&self.m.indexes[i], &props, id) {
                    self.index_add(i, &k)?;
                }
            }
        }
        Ok(())
    }

    /// Whether index `i` holds an entry whose key starts with `value_key`.
    fn index_has_value(&mut self, i: usize, value_key: &[u8]) -> Result<bool> {
        let mut cursor = btree::Cursor::new(self.m.indexes[i].root, value_key);
        let mut found = Vec::new();
        cursor.next_batch(self.p, 1, &mut found)?;
        Ok(!found.is_empty())
    }

    /// Adds an entry key (value key + entity id) to index `i`, keeping its counts.
    fn index_add(&mut self, i: usize, key: &[u8]) -> Result<()> {
        let value = &key[..key.len() - 8];
        let new_value = !self.index_has_value(i, value)?;
        let m = &mut *self.m;
        if btree::insert(self.p, &mut m.index_pages, &mut m.indexes[i].root, key)? {
            m.indexes[i].entries += 1;
            m.indexes[i].distinct += u64::from(new_value);
        }
        Ok(())
    }

    /// Removes an entry key from index `i`, keeping its counts.
    fn index_remove(&mut self, i: usize, key: &[u8]) -> Result<()> {
        if btree::remove(self.p, self.m.indexes[i].root, key)? {
            let ix = &mut self.m.indexes[i];
            ix.entries = ix.entries.saturating_sub(1);
            if !self.index_has_value(i, &key[..key.len() - 8])? {
                let ix = &mut self.m.indexes[i];
                ix.distinct = ix.distinct.saturating_sub(1);
            }
        }
        Ok(())
    }

    fn drop_index(&mut self, name: &str) -> Result<bool> {
        if let Some(pos) = self.m.engine_indexes.iter().position(|d| d.name == name) {
            self.m.engine_indexes.remove(pos);
            return Ok(true);
        }
        let Some(pos) = self.m.indexes.iter().position(|ix| ix.def.name == name) else {
            return Ok(false);
        };
        let ix = self.m.indexes.remove(pos);
        btree::destroy(self.p, &mut self.m.index_pages, ix.root)?;
        Ok(true)
    }
}

// ---- the store ----------------------------------------------------------------------------

/// A disk-resident graph with a bounded page cache.
/// See the [module docs](self) for the on-disk format and recovery rules.
pub struct PersistentGraph {
    opts: PersistentOptions,
    meta: Meta,
    /// Metadata at `begin`; `Some` while a transaction is open.
    saved: Option<Meta>,
    /// Shared with the store's snapshots.
    pager: Arc<Shared>,
    /// Set by [`PersistentGraph::abandon`]: drop without a checkpoint.
    abandoned: bool,
    /// The entities written since [`GraphStore::take_changes`].
    changes: crate::store::ChangeLog,
    _lock: File,
}

impl std::fmt::Debug for PersistentGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentGraph")
            .field("dir", &self.opts.dir)
            .field("nodes", &self.meta.node_count)
            .field("relationships", &self.meta.rel_count)
            .finish_non_exhaustive()
    }
}

fn io(path: &Path, what: &str) -> impl FnOnce(std::io::Error) -> Error {
    let ctx = format!("{what} {}", path.display());
    move |e| Error::io(ctx, e)
}

fn open_rw(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io(path, "opening"))
}

pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    // Makes a rename durable. Windows has no directory handles to sync; NTFS journals
    // the metadata change itself.
    #[cfg(unix)]
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io(dir, "syncing directory"))?;
    let _ = dir;
    Ok(())
}

fn load_meta(path: &Path) -> Result<Meta> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Meta {
                index_format: INDEX_FORMAT,
                ..Meta::default()
            });
        }
        Err(e) => return Err(io(path, "reading")(e)),
    };
    let what = path.display().to_string();
    let version = match bytes.get(..8) {
        Some(m) if bytes.len() >= 8 + 4 && m == META_MAGIC => 10,
        Some(m) if bytes.len() >= 8 + 4 && m == META_MAGIC_V9 => 9,
        Some(m) if bytes.len() >= 8 + 4 && m == META_MAGIC_V8 => 8,
        Some(m) if bytes.len() >= 8 + 4 && m == META_MAGIC_V7 => 7,
        Some(m) if bytes.len() >= 8 + 4 && m == META_MAGIC_V6 => 6,
        Some(m) if bytes.len() >= 8 + 4 && m == META_MAGIC_V5 => 5,
        Some(m) if bytes.len() >= 8 + 4 && m == META_MAGIC_V4 => 4,
        Some(m) if bytes.len() >= 8 + 4 && m == META_MAGIC_V3 => 3,
        Some(m) if bytes.len() >= 8 + 4 && m == META_MAGIC_V2 => 2,
        Some(m) if bytes.len() >= 8 + 4 && m == META_MAGIC_V1 => 1,
        _ => return Err(Error::Storage(format!("{what} is not a graphbuff metadata file"))),
    };
    let (body, tail) = bytes.split_at(bytes.len() - 4);
    if crc32(body) != u32::from_le_bytes(tail.try_into().unwrap()) {
        return Err(Error::Storage(format!("{what} is corrupt: checksum mismatch")));
    }
    let mut d = Decoder::new(&body[8..], &what);
    let m = Meta::decode(&mut d, version)?;
    if !d.is_empty() {
        return Err(d.corrupt("trailing data"));
    }
    Ok(m)
}

fn write_meta(dir: &Path, meta: &Meta) -> Result<()> {
    let mut e = Encoder::new();
    e.buf.extend_from_slice(META_MAGIC);
    meta.encode(&mut e);
    let crc = crc32(&e.buf);
    e.u32(crc);
    let tmp = dir.join(META_TMP);
    let mut f = File::create(&tmp).map_err(io(&tmp, "creating"))?;
    f.write_all(&e.buf)
        .and_then(|_| f.sync_all())
        .map_err(io(&tmp, "writing"))?;
    drop(f);
    let dst = dir.join(META_FILE);
    fs::rename(&tmp, &dst).map_err(io(&dst, "replacing"))?;
    sync_dir(dir)
}

/// Re-applies the log records newer than `meta` to the data files. Stops at the first
/// torn or corrupt record.
fn replay(bytes: &[u8], pager: &mut Pager, meta: &mut Meta, version: u8) -> Result<()> {
    let mut pos = WAL_MAGIC.len();
    while bytes.len() - pos >= 8 {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        let Some(payload) = bytes.get(pos + 8..pos + 8 + len) else {
            break; // torn: the record extends past the end of the file
        };
        if crc32(payload) != crc {
            break; // torn: partly written record
        }
        let what = format!("redo log record at byte {pos}");
        let mut d = Decoder::new(payload, &what);
        let mut m = Meta::decode(&mut d, version)?;
        if version < 2 {
            m.groups = meta.groups;
        }
        if m.seq > meta.seq {
            if m.seq != meta.seq + 1 {
                return Err(Error::Storage(format!(
                    "redo log skips from transaction {} to {}",
                    meta.seq, m.seq
                )));
            }
            let n = d.len(9 + PAGE_SIZE)?;
            for _ in 0..n {
                let file = d.u8()?;
                let page = d.u64()?;
                if file as usize >= FILE_NAMES.len() {
                    return Err(d.corrupt(format!("unknown file {file}")));
                }
                let data = d.bytes(PAGE_SIZE)?;
                pager.write_disk(PageKey { file, page }, data)?;
            }
            *meta = m;
        }
        pos += 8 + len;
    }
    Ok(())
}

impl PersistentGraph {
    /// Opens (or creates) the store in `opts.dir` and recovers its committed state.
    pub fn open(opts: PersistentOptions) -> Result<PersistentGraph> {
        let dir = opts.dir.clone();
        fs::create_dir_all(&dir).map_err(io(&dir, "creating"))?;

        let lock_path = dir.join(LOCK_FILE);
        let lock = open_rw(&lock_path)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Error::Storage(format!(
                    "database directory {} is already open in another process",
                    dir.display()
                )));
            }
            Err(fs::TryLockError::Error(e)) => return Err(io(&lock_path, "locking")(e)),
        }

        crate::maintenance::refuse_incomplete_backup(&dir)?;
        if dir.join(bulk::IMPORT_MARKER).exists() {
            return Err(Error::Storage(format!(
                "{} holds a bulk import that did not finish; delete the directory and import again",
                dir.display()
            )));
        }

        // A leftover temporary metadata file is from a checkpoint that never finished.
        let tmp = dir.join(META_TMP);
        if tmp.exists() {
            fs::remove_file(&tmp).map_err(io(&tmp, "removing"))?;
        }
        let mut meta = load_meta(&dir.join(META_FILE))?;

        let files = FILE_NAMES
            .iter()
            .map(|name| {
                let path = dir.join(name);
                Ok(DataFile {
                    file: open_rw(&path)?,
                    path,
                    unsynced: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let wal_path = dir.join(WAL_FILE);
        let mut wal = open_rw(&wal_path)?;
        let mut bytes = Vec::new();
        wal.read_to_end(&mut bytes)
            .map_err(io(&wal_path, "reading"))?;
        let wal_version = match bytes.get(..8) {
            None => FORMAT, // empty (or shorter than a header): a fresh log
            Some(m) if m == WAL_MAGIC => FORMAT,
            Some(m) if m == WAL_MAGIC_V9 => 9,
            Some(m) if m == WAL_MAGIC_V8 => 8,
            Some(m) if m == WAL_MAGIC_V7 => 7,
            Some(m) if m == WAL_MAGIC_V6 => 6,
            Some(m) if m == WAL_MAGIC_V5 => 5,
            Some(m) if m == WAL_MAGIC_V4 => 4,
            Some(m) if m == WAL_MAGIC_V3 => 3,
            Some(m) if m == WAL_MAGIC_V2 => 2,
            Some(m) if m == WAL_MAGIC_V1 => 1,
            Some(_) => {
                return Err(Error::Storage(format!(
                    "{} is not a graphbuff redo log",
                    wal_path.display()
                )));
            }
        };

        let mut pager = Pager::new(files, wal, wal_path, 0, opts.cache_bytes, opts.max_tx_pages());
        if bytes.len() > WAL_MAGIC.len() {
            replay(&bytes, &mut pager, &mut meta, wal_version)?;
            pager.sync_files()?;
            write_meta(&dir, &meta)?;
        }
        // An older log is replaced even when empty: records are appended to it.
        if bytes.len() != WAL_MAGIC.len() || wal_version < FORMAT {
            pager.reset_wal()?;
        } else {
            pager.set_wal_len(bytes.len() as u64);
        }

        let mut g = PersistentGraph {
            opts,
            meta,
            saved: None,
            pager: Arc::new(Shared {
                pager: Mutex::new(pager),
                waiting: AtomicUsize::new(0),
            }),
            abandoned: false,
            changes: Default::default(),
            _lock: lock,
        };
        if g.meta.index_format < INDEX_FORMAT {
            g.rebuild_indexes()?;
        }
        Ok(g)
    }

    /// Rebuilds every property index in the current key layout, in one transaction (a
    /// crash in between leaves the old indexes, and the next open tries again).
    fn rebuild_indexes(&mut self) -> Result<()> {
        let defs: Vec<IndexDef> = self.meta.indexes.iter().map(|ix| ix.def.clone()).collect();
        // Not the user's transaction: only the log record limits it.
        self.pager_mut().max_tx_pages = MAX_LOGGED_PAGES;
        let restore = self.opts.max_tx_pages();
        self.begin()?;
        let mut rebuild = || -> Result<()> {
            for d in &defs {
                self.drop_index(&d.name)?;
            }
            for d in &defs {
                self.create_index(d.clone())?;
            }
            self.meta.index_format = INDEX_FORMAT;
            Ok(())
        };
        let done = match rebuild() {
            Ok(()) => self.commit(),
            Err(e) => {
                self.rollback();
                Err(e)
            }
        };
        self.pager_mut().max_tx_pages = restore;
        done
    }

    /// A snapshot of the last committed state.
    fn persistent_snapshot(&self) -> PersistentSnapshot {
        // Inside a transaction the committed state is the one saved at `begin`.
        let meta = self.saved.clone().unwrap_or_else(|| self.meta.clone());
        let at = self.lock_pager().register();
        PersistentSnapshot {
            pager: self.pager.clone(),
            meta,
            at,
            description: self.describe(),
        }
    }

    /// The directory this store lives in.
    pub fn dir(&self) -> &Path {
        &self.opts.dir
    }

    /// Current size of the redo log in bytes.
    pub fn wal_size(&self) -> u64 {
        self.lock_pager().wal_len()
    }

    /// Sequence number of the last committed transaction.
    pub fn last_sequence(&self) -> u64 {
        self.meta.seq
    }

    /// Number of pages currently held in the cache.
    pub fn cached_pages(&self) -> usize {
        self.lock_pager().cached_pages()
    }

    /// Number of pages the cache holds before it starts evicting.
    pub fn cache_capacity(&self) -> usize {
        self.lock_pager().capacity()
    }

    /// Closes the store the way a crash would: cached pages are dropped without a
    /// checkpoint, and the next open recovers from the redo log. For tests.
    #[doc(hidden)]
    pub fn abandon(mut self) {
        self.abandoned = true;
    }

    fn lock_pager(&self) -> MutexGuard<'_, Pager> {
        self.pager.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pager_mut(&mut self) -> MutexGuard<'_, Pager> {
        self.lock_pager()
    }

    /// The snapshot version reads are made at: none, the store reads its current state.
    fn at(&self) -> Option<u64> {
        None
    }

    fn writable(&mut self) -> Result<()> {
        match &self.pager_mut().poisoned {
            None => Ok(()),
            Some(why) => Err(Error::Storage(format!(
                "store is read-only after an earlier write failure ({why}); reopen the database"
            ))),
        }
    }

    /// Resolves an expansion's type filter: `Some(None)` = any type, `None` = none of the
    /// requested types exist, so nothing can match.
    fn type_filter(&self, types: &[String]) -> Option<Option<Vec<Token>>> {
        if types.is_empty() {
            return Some(None);
        }
        let t: Vec<Token> = types.iter().filter_map(|t| self.meta.rel_types.get(t)).collect();
        (!t.is_empty()).then_some(Some(t))
    }

    /// Runs a read with the page cache locked.
    fn reader<T>(&self, f: impl FnOnce(&mut Rd) -> Result<T>) -> Result<T> {
        let mut p = lock_at(&self.pager, self.at());
        f(&mut Rd {
            p: &mut p,
            m: &self.meta,
        })
    }

    /// Runs one write. Inside a transaction it simply joins it; outside one (direct use
    /// of the store) the write is committed on its own.
    fn writer<T>(&mut self, f: impl FnOnce(&mut Tx) -> Result<T>) -> Result<T> {
        let auto = self.saved.is_none();
        if auto {
            self.begin()?;
        }
        let result = {
            let mut p = self.pager.lock_for_write();
            f(&mut Tx {
                p: &mut p,
                m: &mut self.meta,
                dense_threshold: self.opts.dense_threshold,
            })
        };
        match result {
            Ok(v) => {
                if auto {
                    self.commit()?;
                }
                Ok(v)
            }
            Err(e) => {
                if auto {
                    self.rollback();
                }
                Err(e)
            }
        }
    }
}

impl Drop for PersistentGraph {
    fn drop(&mut self) {
        if self.abandoned {
            return;
        }
        if self.saved.is_some() {
            self.rollback();
        }
        // A checkpoint makes the next open fast; if it fails, the log still has everything.
        if self.checkpoint().is_err() {
            let _ = self.pager_mut().sync_wal();
        }
    }
}

/// The read methods of [`GraphStore`], shared by [`PersistentGraph`] and
/// [`PersistentSnapshot`]: both have `meta`, `pager`, `reader`, `type_filter` and `at`.
macro_rules! persistent_reads {
    () => {
    fn node_exists(&self, id: NodeId) -> Result<bool> {
        self.reader(|r| Ok(r.node(id)?.is_some()))
    }

    fn relationship_exists(&self, id: RelId) -> Result<bool> {
        self.reader(|r| Ok(r.rel(id)?.is_some()))
    }

    fn node_labels(&self, id: NodeId) -> Result<Vec<String>> {
        self.reader(|r| {
            let Some(rec) = r.node(id)? else {
                return Ok(Vec::new());
            };
            Ok(r.node_data(&rec)?
                .labels
                .iter()
                .map(|l| r.m.labels.name(*l).to_string())
                .collect())
        })
    }

    fn node_has_label(&self, id: NodeId, label: &str) -> Result<bool> {
        let Some(t) = self.meta.labels.get(label) else {
            return Ok(false);
        };
        self.reader(|r| match r.node(id)? {
            Some(rec) => Ok(r.node_data(&rec)?.labels.contains(&t)),
            None => Ok(false),
        })
    }

    fn node_property(&self, id: NodeId, key: &str) -> Result<Option<Value>> {
        let Some(k) = self.meta.keys.get(key) else {
            return Ok(None);
        };
        self.reader(|r| match r.node(id)? {
            Some(rec) => Ok(r.node_data(&rec)?.props.remove(&k)),
            None => Ok(None),
        })
    }

    fn node_properties(&self, id: NodeId) -> Result<BTreeMap<String, Value>> {
        self.reader(|r| {
            let Some(rec) = r.node(id)? else {
                return Ok(BTreeMap::new());
            };
            Ok(r.node_data(&rec)?
                .props
                .into_iter()
                .map(|(k, v)| (r.m.keys.name(k).to_string(), v))
                .collect())
        })
    }

    fn relationship_type(&self, id: RelId) -> Result<Option<String>> {
        self.reader(|r| {
            Ok(r.rel(id)?
                .map(|rec| r.m.rel_types.name(rec.rel_type).to_string()))
        })
    }

    fn relationship_endpoints(&self, id: RelId) -> Result<Option<(NodeId, NodeId)>> {
        self.reader(|r| Ok(r.rel(id)?.map(|rec| (NodeId(rec.start), NodeId(rec.end)))))
    }

    fn relationship_property(&self, id: RelId, key: &str) -> Result<Option<Value>> {
        let Some(k) = self.meta.keys.get(key) else {
            return Ok(None);
        };
        self.reader(|r| match r.rel(id)? {
            Some(rec) => Ok(r.rel_props(&rec)?.remove(&k)),
            None => Ok(None),
        })
    }

    fn relationship_properties(&self, id: RelId) -> Result<BTreeMap<String, Value>> {
        self.reader(|r| {
            let Some(rec) = r.rel(id)? else {
                return Ok(BTreeMap::new());
            };
            Ok(r.rel_props(&rec)?
                .into_iter()
                .map(|(k, v)| (r.m.keys.name(k).to_string(), v))
                .collect())
        })
    }

    fn all_nodes(&self) -> Scan<'_, NodeId> {
        let scan = record_scan(NODES, NODE_REC, self.meta.next_node);
        Box::new(Batched::new(&self.pager, self.at(), scan).map(|r| r.map(NodeId)))
    }

    fn nodes_with_label(&self, label: &str) -> Scan<'_, NodeId> {
        let Some(t) = self.meta.labels.get(label) else {
            return Box::new(std::iter::empty());
        };
        let scan = label_scan(self.meta.label_tree, t);
        Box::new(Batched::new(&self.pager, self.at(), scan).map(|r| r.map(NodeId)))
    }

    fn label_count(&self, label: &str) -> usize {
        self.meta
            .labels
            .get(label)
            .map_or(0, |t| self.meta.label_counts[t as usize] as usize)
    }

    fn all_relationships(&self) -> Scan<'_, RelId> {
        let scan = record_scan(RELS, REL_REC, self.meta.next_rel);
        Box::new(Batched::new(&self.pager, self.at(), scan).map(|r| r.map(RelId)))
    }

    fn expand(&self, node: NodeId, dir: Direction, types: &[String]) -> Adjacency<'_> {
        let Some(wanted) = self.type_filter(types) else {
            return Box::new(std::iter::empty());
        };
        let walk = adjacency_walk(&self.meta, node, dir, wanted);
        Box::new(Batched::new(&self.pager, self.at(), walk))
    }

    fn degree(&self, node: NodeId, dir: Direction, types: &[String]) -> Result<usize> {
        let Some(wanted) = self.type_filter(types) else {
            return Ok(0);
        };
        match self.reader(|r| r.quick_degree(node, dir, &wanted))? {
            Some(d) => Ok(d),
            None => self.expand(node, dir, types).try_fold(0, |n, e| e.map(|_| n + 1)),
        }
    }

    fn node_count(&self) -> usize {
        self.meta.node_count as usize
    }

    fn relationship_count(&self) -> usize {
        self.meta.rel_count as usize
    }

    fn labels(&self) -> Vec<String> {
        self.meta
            .token_names(&self.meta.labels, &self.meta.label_counts)
    }

    fn relationship_types(&self) -> Vec<String> {
        self.meta
            .token_names(&self.meta.rel_types, &self.meta.type_counts)
    }

    fn relationship_type_count(&self, rel_type: &str) -> usize {
        self.meta
            .rel_types
            .get(rel_type)
            .map_or(0, |t| self.meta.type_counts[t as usize] as usize)
    }

    fn schema_index_stats(&self, entity: IndexEntity, label: &str, properties: &[String]) -> Option<IndexStats> {
        let ix = self.meta.find_index(entity, label, properties)?;
        Some(IndexStats {
            entries: ix.entries as usize,
            distinct: ix.distinct as usize,
        })
    }

    fn property_keys(&self) -> Vec<String> {
        let mut out = self.meta.keys.names.clone();
        out.sort();
        out
    }

    fn indexes(&self) -> Vec<IndexDef> {
        let mut out: Vec<IndexDef> = self.meta.indexes.iter().map(|ix| ix.def.clone()).collect();
        out.extend(self.meta.engine_indexes.iter().cloned());
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn constraints(&self) -> Vec<ConstraintDef> {
        let mut out = self.meta.constraints.clone();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn schema_index_seek(
        &self,
        entity: IndexEntity,
        label: &str,
        properties: &[String],
        values: &[Value],
    ) -> Result<Option<Vec<u64>>> {
        self.reader(|r| r.index_seek(entity, label, properties, values))
    }

    fn schema_index_range(
        &self,
        entity: IndexEntity,
        label: &str,
        property: &str,
        range: &ValueRange,
    ) -> Result<Option<Vec<u64>>> {
        self.reader(|r| r.index_range(entity, label, property, range))
    }
    };
}

impl GraphStore for PersistentGraph {
    persistent_reads!();

    fn create_node(&mut self, labels: &[String], props: BTreeMap<String, Value>) -> Result<NodeId> {
        let id = self.writer(|t| t.create_node(labels, props))?;
        self.changes.node(id);
        Ok(id)
    }

    fn create_relationship(
        &mut self,
        start: NodeId,
        end: NodeId,
        rel_type: &str,
        props: BTreeMap<String, Value>,
    ) -> Result<RelId> {
        let id = self.writer(|t| t.create_rel(start, end, rel_type, props))?;
        self.changes.rel(id);
        Ok(id)
    }

    fn delete_node(&mut self, id: NodeId, detach: bool) -> Result<bool> {
        // A detaching delete removes the relationships inside the transaction.
        let attached = if detach { self.expand_all(id, Direction::Both, &[])? } else { Vec::new() };
        let deleted = self.writer(|t| t.delete_node(id, detach))?;
        if deleted {
            self.changes.node(id);
            for e in attached {
                self.changes.rel(e.rel);
            }
        }
        Ok(deleted)
    }

    fn delete_relationship(&mut self, id: RelId) -> Result<bool> {
        let deleted = self.writer(|t| t.delete_rel(id))?;
        if deleted {
            self.changes.rel(id);
        }
        Ok(deleted)
    }

    fn set_node_property(&mut self, id: NodeId, key: &str, value: Value) -> Result<()> {
        self.writer(|t| t.set_node_prop(id, key, value))?;
        self.changes.node(id);
        Ok(())
    }

    fn set_relationship_property(&mut self, id: RelId, key: &str, value: Value) -> Result<()> {
        self.writer(|t| t.set_rel_prop(id, key, value))?;
        self.changes.rel(id);
        Ok(())
    }

    fn add_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        let added = self.writer(|t| t.add_label(id, label))?;
        if added {
            self.changes.node(id);
        }
        Ok(added)
    }

    fn remove_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        let removed = self.writer(|t| t.remove_label(id, label))?;
        if removed {
            self.changes.node(id);
        }
        Ok(removed)
    }

    fn create_index(&mut self, def: IndexDef) -> Result<()> {
        self.writer(|t| t.create_index(def))
    }

    fn drop_index(&mut self, name: &str) -> Result<bool> {
        self.writer(|t| t.drop_index(name))
    }

    fn create_constraint(&mut self, def: ConstraintDef) -> Result<()> {
        self.writer(|t| {
            def.check_new(&t.m.constraints)?;
            t.m.constraints.push(def);
            Ok(())
        })
    }

    fn drop_constraint(&mut self, name: &str) -> Result<bool> {
        self.writer(|t| {
            let before = t.m.constraints.len();
            t.m.constraints.retain(|c| c.name != name);
            Ok(t.m.constraints.len() < before)
        })
    }

    fn snapshot(&self) -> Option<Snapshot> {
        Some(Arc::new(self.persistent_snapshot()))
    }

    fn backup(&self, dest: &Path) -> Result<crate::BackupInfo> {
        self.persistent_snapshot().write_backup(dest)
    }

    fn check(&self) -> Result<crate::CheckReport> {
        self.persistent_snapshot().check_store()
    }

    fn begin(&mut self) -> Result<()> {
        self.writable()?;
        if self.saved.is_some() {
            return Err(Error::Runtime("a transaction is already open".into()));
        }
        self.saved = Some(self.meta.clone());
        self.pager_mut().begin();
        self.changes.begin();
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let Some(saved) = self.saved.take() else {
            return Ok(());
        };
        self.changes.commit();
        if !self.pager_mut().changed() && self.meta == saved {
            self.pager_mut().rollback(); // nothing to undo; just ends the transaction
            return Ok(());
        }
        self.meta.seq = saved.seq + 1;
        let mut header = Encoder::new();
        self.meta.encode(&mut header);
        let sync = self.opts.sync == SyncPolicy::EveryCommit;
        let committed = self.pager_mut().commit(&header.buf, sync);
        if let Err(e) = committed {
            self.meta = saved;
            return Err(e);
        }
        let limit = self.opts.checkpoint_after_bytes;
        if limit > 0 && self.pager_mut().wal_len() > limit {
            // The commit is durable already; a failed checkpoint only leaves a longer
            // log, and is retried after the next commit.
            let _ = self.checkpoint();
        }
        Ok(())
    }

    fn rollback(&mut self) {
        if let Some(saved) = self.saved.take() {
            self.meta = saved;
        }
        self.pager_mut().rollback();
        self.changes.rollback();
    }

    fn take_changes(&mut self) -> crate::Changes {
        self.changes.take()
    }

    fn checkpoint(&mut self) -> Result<()> {
        if self.saved.is_some() {
            return Err(Error::Runtime(
                "cannot checkpoint while a transaction is open".into(),
            ));
        }
        self.writable()?;
        let mut p = self.pager.lock().unwrap_or_else(|e| e.into_inner());
        p.flush_all()?;
        write_meta(&self.opts.dir, &self.meta)?;
        p.reset_wal()
    }

    fn describe(&self) -> String {
        let sync = match self.opts.sync {
            SyncPolicy::EveryCommit => "fsync per commit",
            SyncPolicy::OsBuffered => "OS-buffered",
        };
        format!(
            "persistent: paged files in {} ({} MiB page cache, {sync})",
            self.opts.dir.display(),
            self.opts.cache_bytes >> 20
        )
    }
}

/// A read-only view of a [`PersistentGraph`] as of one commit ([`GraphStore::snapshot`]).
/// It shares the store's page cache, reading the pages that later commits changed from
/// images the cache keeps while the snapshot lives.
pub struct PersistentSnapshot {
    pager: Arc<Shared>,
    meta: Meta,
    /// The version this snapshot reads (see [`Pager::read_at`]).
    at: u64,
    description: String,
}

impl Drop for PersistentSnapshot {
    fn drop(&mut self) {
        self.pager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unregister(self.at);
    }
}

impl PersistentSnapshot {
    fn at(&self) -> Option<u64> {
        Some(self.at)
    }

    fn reader<T>(&self, f: impl FnOnce(&mut Rd) -> Result<T>) -> Result<T> {
        let mut p = lock_at(&self.pager, self.at());
        f(&mut Rd {
            p: &mut p,
            m: &self.meta,
        })
    }

    fn type_filter(&self, types: &[String]) -> Option<Option<Vec<Token>>> {
        if types.is_empty() {
            return Some(None);
        }
        let t: Vec<Token> = types.iter().filter_map(|t| self.meta.rel_types.get(t)).collect();
        (!t.is_empty()).then_some(Some(t))
    }
}

fn read_only() -> Error {
    Error::Runtime("a snapshot is read-only".into())
}

impl GraphStore for PersistentSnapshot {
    persistent_reads!();

    fn create_node(&mut self, _: &[String], _: BTreeMap<String, Value>) -> Result<NodeId> {
        Err(read_only())
    }

    fn create_relationship(&mut self, _: NodeId, _: NodeId, _: &str, _: BTreeMap<String, Value>) -> Result<RelId> {
        Err(read_only())
    }

    fn delete_node(&mut self, _: NodeId, _: bool) -> Result<bool> {
        Err(read_only())
    }

    fn delete_relationship(&mut self, _: RelId) -> Result<bool> {
        Err(read_only())
    }

    fn set_node_property(&mut self, _: NodeId, _: &str, _: Value) -> Result<()> {
        Err(read_only())
    }

    fn set_relationship_property(&mut self, _: RelId, _: &str, _: Value) -> Result<()> {
        Err(read_only())
    }

    fn add_label(&mut self, _: NodeId, _: &str) -> Result<bool> {
        Err(read_only())
    }

    fn remove_label(&mut self, _: NodeId, _: &str) -> Result<bool> {
        Err(read_only())
    }

    fn begin(&mut self) -> Result<()> {
        Err(read_only())
    }

    fn commit(&mut self) -> Result<()> {
        Ok(())
    }

    fn rollback(&mut self) {}

    fn describe(&self) -> String {
        self.description.clone()
    }

    fn backup(&self, dest: &Path) -> Result<crate::BackupInfo> {
        self.write_backup(dest)
    }

    fn check(&self) -> Result<crate::CheckReport> {
        self.check_store()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!(
                "graphbuff-dense-{}-{name}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&d);
            Dir(d)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn none() -> BTreeMap<String, Value> {
        BTreeMap::new()
    }

    fn props_of(g: &dyn GraphStore, label: &str) -> Vec<(u64, BTreeMap<String, Value>)> {
        g.nodes_with_label(label)
            .map(|n| {
                let n = n.unwrap();
                (n.0, g.node_properties(n).unwrap())
            })
            .collect()
    }

    #[test]
    fn snapshots_keep_reading_their_commit() {
        let dir = Dir::new("snapshots");
        // A tiny cache, so snapshot reads also go through evictions and disk reads.
        let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0).cache_bytes(16 * 8192)).unwrap();
        g.create_index(IndexDef::node("ix", "N", "i")).unwrap();
        g.begin().unwrap();
        let mut ids = Vec::new();
        for i in 0..3000 {
            let mut p = none();
            p.insert("i".into(), Value::Int(i));
            p.insert("pad".into(), Value::String("x".repeat(50)));
            ids.push(g.create_node(&["N".into()], p).unwrap());
        }
        for w in ids.windows(2) {
            g.create_relationship(w[0], w[1], "NEXT", none()).unwrap();
        }
        g.commit().unwrap();
        let first = g.snapshot().unwrap();
        let before = props_of(&*first, "N");
        let degree = first.degree(ids[10], Direction::Both, &[]).unwrap();

        // Changes in an open transaction are invisible to a snapshot taken meanwhile.
        g.begin().unwrap();
        for &n in ids.iter().step_by(3) {
            g.set_node_property(n, "i", Value::Int(-1)).unwrap();
        }
        g.delete_node(ids[10], true).unwrap();
        let during = g.snapshot().unwrap();
        assert_eq!(props_of(&*during, "N"), before);
        g.commit().unwrap();
        let second = g.snapshot().unwrap();
        let after = props_of(&*second, "N");
        assert_ne!(after, before);
        assert_eq!(after.len(), 2999);

        // Two more commits, one rolled back.
        g.begin().unwrap();
        for &n in &ids[100..200] {
            g.add_label(n, "M").unwrap();
        }
        g.commit().unwrap();
        g.begin().unwrap();
        g.set_node_property(ids[0], "i", Value::Int(77)).unwrap();
        g.rollback();

        for s in [&first, &during] {
            assert_eq!(props_of(&**s, "N"), before);
            assert_eq!(s.node_count(), 3000);
            assert_eq!(s.degree(ids[10], Direction::Both, &[]).unwrap(), degree);
            assert_eq!(s.index_seek("N", "i", &Value::Int(9)).unwrap(), Some(vec![ids[9]]));
            assert_eq!(s.label_count("M"), 0);
        }
        assert_eq!(props_of(&*second, "N"), after);
        assert_eq!(second.index_seek("N", "i", &Value::Int(9)).unwrap(), Some(vec![]));
        assert_eq!(g.label_count("M"), 100);

        assert!(g.lock_pager().history_pages() > 0);
        drop((first, during, second));
        assert_eq!(g.lock_pager().history_pages(), 0);
    }

    fn is_dense(g: &PersistentGraph, n: NodeId) -> bool {
        g.reader(|r| Ok(r.node(n)?.unwrap().dense)).unwrap()
    }

    fn group_count(g: &PersistentGraph, n: NodeId) -> usize {
        g.reader(|r| {
            let rec = r.node(n)?.unwrap();
            Ok(r.groups(&rec)?.len())
        })
        .unwrap()
    }

    fn degrees(g: &PersistentGraph, n: NodeId, types: &[String]) -> [usize; 3] {
        [Direction::Outgoing, Direction::Incoming, Direction::Both]
            .map(|d| g.degree(n, d, types).unwrap())
    }

    fn sorted_expand(g: &PersistentGraph, n: NodeId, d: Direction, types: &[String]) -> Vec<(u64, u64)> {
        let mut v: Vec<_> = g
            .expand_all(n, d, types)
            .unwrap()
            .iter()
            .map(|e| (e.rel.0, e.other.0))
            .collect();
        v.sort();
        v
    }

    /// A hub with 6 `A` out, 3 `B` in and 2 `A` self-loops, plus a leaf per relationship.
    fn hub(g: &mut PersistentGraph) -> NodeId {
        let h = g.create_node(&["Hub".into()], none()).unwrap();
        for i in 0..9 {
            let leaf = g.create_node(&[], none()).unwrap();
            if i < 6 {
                g.create_relationship(h, leaf, "A", none()).unwrap();
            } else {
                g.create_relationship(leaf, h, "B", none()).unwrap();
            }
        }
        g.create_relationship(h, h, "A", none()).unwrap();
        g.create_relationship(h, h, "A", none()).unwrap();
        h
    }

    #[test]
    fn node_turns_dense_past_the_threshold() {
        let dir = Dir::new("threshold");
        let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0).dense_threshold(4)).unwrap();
        let h = g.create_node(&[], none()).unwrap();
        let mut sparse = None;
        for i in 0..5 {
            let leaf = g.create_node(&[], none()).unwrap();
            g.create_relationship(h, leaf, if i % 2 == 0 { "A" } else { "B" }, none())
                .unwrap();
            if i == 3 {
                assert!(!is_dense(&g, h), "4 relationships: still sparse");
                sparse = Some(sorted_expand(&g, h, Direction::Both, &[]));
            }
        }
        assert!(is_dense(&g, h), "5 relationships: dense");
        assert_eq!(group_count(&g, h), 2);
        let all = sorted_expand(&g, h, Direction::Both, &[]);
        assert_eq!(all.len(), 5);
        assert!(sparse.unwrap().iter().all(|e| all.contains(e)));
        assert_eq!(degrees(&g, h, &["A".into()]), [3, 0, 3]);
        assert_eq!(degrees(&g, h, &["B".into()]), [2, 0, 2]);
        assert_eq!(degrees(&g, h, &[]), [5, 0, 5]);
        // Creation order is kept within a group.
        let a: Vec<u64> = g
            .expand_all(h, Direction::Outgoing, &["A".into()])
            .unwrap()
            .iter()
            .map(|e| e.rel.0)
            .collect();
        assert_eq!(a, vec![0, 2, 4]);
    }

    #[test]
    fn dense_degrees_count_self_loops_once_for_both() {
        let dir = Dir::new("loops");
        let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0).dense_threshold(3)).unwrap();
        let h = hub(&mut g);
        assert!(is_dense(&g, h));
        assert_eq!(degrees(&g, h, &[]), [8, 5, 11]);
        assert_eq!(degrees(&g, h, &["A".into()]), [8, 2, 8]);
        assert_eq!(degrees(&g, h, &["B".into()]), [0, 3, 3]);
        assert_eq!(degrees(&g, h, &["A".into(), "B".into()]), [8, 5, 11]);
        for d in [Direction::Outgoing, Direction::Incoming, Direction::Both] {
            assert_eq!(sorted_expand(&g, h, d, &[]).len(), g.degree(h, d, &[]).unwrap());
        }
    }

    #[test]
    fn emptied_groups_are_freed_and_reused() {
        let dir = Dir::new("groups");
        let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0).dense_threshold(0)).unwrap();
        let h = hub(&mut g);
        assert_eq!(group_count(&g, h), 2);
        let b: Vec<RelId> = g
            .expand_all(h, Direction::Incoming, &["B".into()])
            .unwrap()
            .iter()
            .map(|e| e.rel)
            .collect();
        for r in b {
            g.delete_relationship(r).unwrap();
        }
        assert_eq!(group_count(&g, h), 1, "group B is gone");
        let next = g.meta.groups.next;
        let leaf = g.create_node(&[], none()).unwrap();
        g.create_relationship(leaf, h, "C", none()).unwrap();
        assert_eq!(group_count(&g, h), 2);
        // Deleting the B relationships freed four groups (the hub's and each leaf's, as
        // every node is dense here); both new C groups reuse freed records.
        assert_ne!(g.meta.groups.free, 0);
        assert_eq!(g.meta.groups.next, next);

        // Detaching the hub frees all of its groups and leaves the leaves consistent.
        g.delete_node(h, true).unwrap();
        assert_eq!(g.relationship_count(), 0);
        for n in g.all_nodes().map(Result::unwrap).collect::<Vec<_>>() {
            assert_eq!(degrees(&g, n, &[]), [0, 0, 0]);
            assert_eq!(group_count(&g, n), 0);
        }
    }

    #[test]
    fn densifying_rolls_back_exactly() {
        let dir = Dir::new("rollback");
        let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0).dense_threshold(8)).unwrap();
        let h = g.create_node(&[], none()).unwrap();
        for _ in 0..8 {
            let leaf = g.create_node(&[], none()).unwrap();
            g.create_relationship(h, leaf, "A", none()).unwrap();
        }
        let before = sorted_expand(&g, h, Direction::Both, &[]);
        let meta = g.meta.clone();

        g.begin().unwrap();
        let leaf = g.create_node(&[], none()).unwrap();
        g.create_relationship(leaf, h, "B", none()).unwrap();
        assert!(is_dense(&g, h));
        g.rollback();

        assert!(!is_dense(&g, h));
        assert_eq!(g.meta, meta);
        assert_eq!(sorted_expand(&g, h, Direction::Both, &[]), before);
        assert_eq!(degrees(&g, h, &[]), [8, 0, 8]);
    }

    #[test]
    fn dense_nodes_survive_a_crash() {
        let dir = Dir::new("crash");
        let opts = PersistentOptions::new(&dir.0).dense_threshold(3);
        let (h, expected) = {
            let mut g = PersistentGraph::open(opts.clone()).unwrap();
            let h = hub(&mut g);
            let e = sorted_expand(&g, h, Direction::Both, &[]);
            g.abandon();
            (h, e)
        };
        let g = PersistentGraph::open(opts).unwrap();
        assert!(is_dense(&g, h));
        assert_eq!(sorted_expand(&g, h, Direction::Both, &[]), expected);
        assert_eq!(degrees(&g, h, &[]), [8, 5, 11]);
    }

    /// Rewrites a closed store's metadata and log in format version 1.
    fn downgrade_to_v1(dir: &Path) {
        let v1_meta = |m: &Meta| {
            assert_eq!(m.groups, PageAlloc::default(), "v1 stores have no groups");
            let mut e = Encoder::new();
            m.encode_version(&mut e, 1);
            e.buf
        };
        let meta_path = dir.join(META_FILE);
        if let Ok(bytes) = fs::read(&meta_path) {
            let body = &bytes[8..bytes.len() - 4];
            let m = Meta::decode(&mut Decoder::new(body, "meta"), 2).unwrap();
            let mut out = META_MAGIC_V1.to_vec();
            out.extend(v1_meta(&m));
            let crc = crc32(&out);
            out.extend(crc.to_le_bytes());
            fs::write(&meta_path, out).unwrap();
        }
        let wal = fs::read(dir.join(WAL_FILE)).unwrap();
        assert_eq!(&wal[..8], WAL_MAGIC);
        let mut out = WAL_MAGIC_V1.to_vec();
        let mut pos = 8;
        while pos < wal.len() {
            let len = u32::from_le_bytes(wal[pos..pos + 4].try_into().unwrap()) as usize;
            let payload = &wal[pos + 8..pos + 8 + len];
            let m = Meta::decode(&mut Decoder::new(payload, "record"), 2).unwrap();
            let mut e = Encoder::new();
            m.encode(&mut e);
            let mut new = v1_meta(&m);
            new.extend_from_slice(&payload[e.buf.len()..]);
            out.extend((new.len() as u32).to_le_bytes());
            out.extend(crc32(&new).to_le_bytes());
            out.extend(new);
            pos += 8 + len;
        }
        fs::write(dir.join(WAL_FILE), out).unwrap();
        let _ = fs::remove_file(dir.join(FILE_NAMES[GROUPS as usize]));
    }

    #[test]
    fn opens_a_clean_version_1_store() {
        let dir = Dir::new("v1-clean");
        let sparse = PersistentOptions::new(&dir.0).dense_threshold(1000);
        let (h, expected) = {
            let mut g = PersistentGraph::open(sparse).unwrap();
            let h = hub(&mut g);
            (h, sorted_expand(&g, h, Direction::Both, &[]))
        }; // closing checkpoints
        downgrade_to_v1(&dir.0);
        {
            let mut g =
                PersistentGraph::open(PersistentOptions::new(&dir.0).dense_threshold(3)).unwrap();
            assert_eq!(&fs::read(dir.0.join(WAL_FILE)).unwrap()[..8], WAL_MAGIC);
            assert!(!is_dense(&g, h), "nodes stay sparse until written");
            assert_eq!(sorted_expand(&g, h, Direction::Both, &[]), expected);
            assert_eq!(degrees(&g, h, &[]), [8, 5, 11]);
            let leaf = g.create_node(&[], none()).unwrap();
            g.create_relationship(h, leaf, "B", none()).unwrap();
            assert!(is_dense(&g, h), "a new relationship densifies");
            assert_eq!(degrees(&g, h, &[]), [9, 5, 12]);
        }
        assert_eq!(&fs::read(dir.0.join(META_FILE)).unwrap()[..8], META_MAGIC);
    }

    #[test]
    fn replays_a_version_1_log() {
        let dir = Dir::new("v1-crash");
        let sparse = PersistentOptions::new(&dir.0).dense_threshold(1000);
        let (h, expected) = {
            let mut g = PersistentGraph::open(sparse).unwrap();
            g.create_node(&[], none()).unwrap();
            g.checkpoint().unwrap();
            let h = hub(&mut g);
            let e = sorted_expand(&g, h, Direction::Both, &[]);
            g.abandon();
            (h, e)
        };
        downgrade_to_v1(&dir.0);
        let g = PersistentGraph::open(PersistentOptions::new(&dir.0).dense_threshold(3)).unwrap();
        assert_eq!(g.node_count(), 11);
        assert_eq!(sorted_expand(&g, h, Direction::Both, &[]), expected);
        assert_eq!(&fs::read(dir.0.join(WAL_FILE)).unwrap()[..8], WAL_MAGIC);
        assert_eq!(&fs::read(dir.0.join(META_FILE)).unwrap()[..8], META_MAGIC);
    }

    fn props(k: &str, v: Value) -> BTreeMap<String, Value> {
        BTreeMap::from([(k.to_string(), v)])
    }

    fn unique(name: &str, label: &str, prop: &str) -> ConstraintDef {
        ConstraintDef {
            name: name.into(),
            kind: crate::ConstraintKind::Unique,
            label: label.into(),
            properties: vec![prop.into()],
        }
    }

    #[test]
    fn constraints_are_durable_and_transactional() {
        let dir = Dir::new("constraints");
        {
            let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
            g.create_constraint(unique("a", "P", "x")).unwrap();
            assert!(g.create_constraint(unique("a", "P", "y")).is_err(), "name taken");
            assert!(g.create_constraint(unique("b", "P", "x")).is_err(), "equivalent");
            g.begin().unwrap();
            g.create_constraint(unique("b", "P", "y")).unwrap();
            assert!(g.drop_constraint("a").unwrap());
            g.rollback();
            assert_eq!(g.constraints(), vec![unique("a", "P", "x")]);
            g.create_constraint(unique("c", "Q", "z")).unwrap();
            g.abandon(); // no checkpoint: the log alone must restore them
        }
        {
            let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
            assert_eq!(g.constraints(), vec![unique("a", "P", "x"), unique("c", "Q", "z")]);
            assert!(g.drop_constraint("c").unwrap());
            assert!(!g.drop_constraint("c").unwrap());
        } // closing checkpoints
        let g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
        assert_eq!(g.constraints(), vec![unique("a", "P", "x")]);
    }

    #[test]
    fn opens_a_version_4_store() {
        let dir = Dir::new("v4");
        {
            let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
            g.create_node(&["P".into()], props("x", Value::Int(1))).unwrap();
        }
        let meta_path = dir.0.join(META_FILE);
        let bytes = fs::read(&meta_path).unwrap();
        let m = Meta::decode(&mut Decoder::new(&bytes[8..bytes.len() - 4], "meta"), FORMAT).unwrap();
        let mut e = Encoder::new();
        m.encode_version(&mut e, 4);
        let mut out = META_MAGIC_V4.to_vec();
        out.extend(e.buf);
        let crc = crc32(&out);
        out.extend(crc.to_le_bytes());
        fs::write(&meta_path, out).unwrap();
        fs::write(dir.0.join(WAL_FILE), WAL_MAGIC_V4).unwrap();
        {
            let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
            assert_eq!(&fs::read(dir.0.join(WAL_FILE)).unwrap()[..8], WAL_MAGIC);
            assert!(g.constraints().is_empty());
            assert_eq!(g.node_count(), 1);
            g.create_constraint(unique("a", "P", "x")).unwrap();
        }
        assert_eq!(&fs::read(&meta_path).unwrap()[..8], META_MAGIC);
        let g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
        assert_eq!(g.constraints(), vec![unique("a", "P", "x")]);
    }

    #[test]
    fn relationship_and_composite_indexes_survive_reopening() {
        let dir = Dir::new("rel-composite-index");
        let xy = ["x".to_string(), "y".to_string()];
        let w = ["w".to_string()];
        let long = "z".repeat(5000);
        let (a, r);
        {
            let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
            a = g.create_node(&["P".into()], props("x", Value::Int(1))).unwrap();
            g.set_node_property(a, "y", Value::String(long.clone())).unwrap();
            let b = g.create_node(&["P".into()], props("x", Value::Int(1))).unwrap();
            r = g.create_relationship(a, b, "R", props("w", Value::Int(5))).unwrap();
            g.create_index(IndexDef::relationship("r_w", "R", "w")).unwrap();
            g.create_index(IndexDef {
                name: "p_xy".into(),
                entity: IndexEntity::Node,
                label: "P".into(),
                properties: xy.to_vec(),
                extra_labels: Vec::new(),
                kind: Default::default(),
            })
            .unwrap();
            g.create_relationship(b, a, "R", props("w", Value::Int(6))).unwrap();
            g.checkpoint().unwrap();
            g.set_relationship_property(r, "w", Value::Int(7)).unwrap(); // in the log only
        }
        let g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
        assert_eq!(g.indexes().len(), 2);
        let rel = |v: i64| {
            g.schema_index_seek(IndexEntity::Relationship, "R", &w, &[Value::Int(v)]).unwrap().unwrap()
        };
        assert!(rel(5).is_empty());
        assert_eq!(rel(7), vec![r.0]);
        let range = ValueRange::Between {
            lower: Bound::Included(Value::Int(6)),
            upper: Bound::Unbounded,
        };
        let found = g.schema_index_range(IndexEntity::Relationship, "R", "w", &range).unwrap().unwrap();
        assert_eq!(found.len(), 2);
        let pair = |y: Value| {
            g.schema_index_seek(IndexEntity::Node, "P", &xy, &[Value::Int(1), y]).unwrap().unwrap()
        };
        assert_eq!(pair(Value::String(long.clone())), vec![a.0]);
        assert!(pair(Value::String(format!("{long}!"))).is_empty());
        assert_eq!(
            g.schema_index_stats(IndexEntity::Node, "P", &xy),
            Some(IndexStats { entries: 1, distinct: 1 })
        );
    }

    #[test]
    fn opens_a_version_2_store_and_rebuilds_its_indexes() {
        upgrades_indexes_from(2, META_MAGIC_V2, WAL_MAGIC_V2);
    }

    #[test]
    fn opens_a_version_3_store_and_counts_its_indexes() {
        upgrades_indexes_from(3, META_MAGIC_V3, WAL_MAGIC_V3);
    }

    /// Rewrites a closed store as storage format `version` and checks that opening it
    /// rebuilds the indexes: seeks, range seeks and counts work.
    fn upgrades_indexes_from(version: u8, meta_magic: &[u8; 8], wal_magic: &[u8; 8]) {
        let dir = Dir::new(&format!("v{version}-index"));
        let names = ["Ann", "Bob", "Cat", "Dan"];
        {
            let mut g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
            for (i, n) in names.iter().enumerate() {
                g.create_node(&["P".into()], props("name", Value::from(*n))).unwrap();
                g.create_node(&["P".into()], props("age", Value::Int(20 + i as i64))).unwrap();
            }
            for (name, prop) in [("by_name", "name"), ("by_age", "age")] {
                g.create_index(IndexDef::node(name, "P", prop))
                .unwrap();
            }
        } // closing checkpoints
        // Rewrite the metadata and the log header in the old format.
        let meta_path = dir.0.join(META_FILE);
        let bytes = fs::read(&meta_path).unwrap();
        let mut m = Meta::decode(&mut Decoder::new(&bytes[8..bytes.len() - 4], "meta"), FORMAT).unwrap();
        m.index_format = version - 1; // format 3 wrote index format 2
        let mut e = Encoder::new();
        m.encode_version(&mut e, version);
        let mut out = meta_magic.to_vec();
        out.extend(e.buf);
        let crc = crc32(&out);
        out.extend(crc.to_le_bytes());
        fs::write(&meta_path, out).unwrap();
        fs::write(dir.0.join(WAL_FILE), wal_magic).unwrap();

        {
            let g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
            assert_eq!(g.meta.index_format, INDEX_FORMAT);
            let names_of = |ids: Vec<NodeId>| -> Vec<Value> {
                ids.into_iter()
                    .map(|n| g.node_property(n, "name").unwrap().unwrap())
                    .collect()
            };
            let between = ValueRange::Between {
                lower: Bound::Included(Value::from("B")),
                upper: Bound::Excluded(Value::from("D")),
            };
            let got = g.index_range("P", "name", &between).unwrap().unwrap();
            assert_eq!(names_of(got), vec![Value::from("Bob"), Value::from("Cat")]);
            let ages = ValueRange::Between {
                lower: Bound::Included(Value::Int(22)),
                upper: Bound::Unbounded,
            };
            assert_eq!(g.index_range("P", "age", &ages).unwrap().unwrap().len(), 2);
            assert_eq!(g.index_seek("P", "name", &Value::from("Dan")).unwrap().unwrap().len(), 1);
            let mut defs: Vec<String> = g.indexes().into_iter().map(|d| d.name).collect();
            defs.sort();
            assert_eq!(defs, vec!["by_age", "by_name"]);
            let counts = IndexStats {
                entries: 4,
                distinct: 4,
            };
            assert_eq!(g.index_stats("P", "name"), Some(counts));
            assert_eq!(g.index_stats("P", "age"), Some(counts));
        }
        assert_eq!(&fs::read(&meta_path).unwrap()[..8], META_MAGIC);
        let g = PersistentGraph::open(PersistentOptions::new(&dir.0)).unwrap();
        assert_eq!(g.meta.index_format, INDEX_FORMAT);
    }

    #[test]
    fn string_and_number_keys_sort_like_their_values() {
        let long = "m".repeat(KEY_STRING_PREFIX + 10);
        let mut all: Vec<String> = ["", "a", "a\u{0}", "a\u{0}b", "ab", "b", "\u{0}"]
            .iter()
            .map(|w| w.to_string())
            .collect();
        all.push(long.clone());
        all.push(format!("{long}z"));
        all.push("m".repeat(KEY_STRING_PREFIX));
        for a in &all {
            for b in &all {
                let (ka, kb) = (sort_key(&Value::from(a.as_str())), sort_key(&Value::from(b.as_str())));
                // Keys never contradict the string order (cut strings may tie).
                if a < b {
                    assert!(ka <= kb, "{a:?} < {b:?}");
                }
            }
        }
        for (x, y) in [(-1.5, -1.0), (-1.0, 0.0), (0.0, 1e-9), (1.0, 2.0), (2.0, f64::INFINITY)] {
            assert!(sortable_f64(x) < sortable_f64(y), "{x} < {y}");
        }
    }

    /// Creates `n` padded `:N` nodes in one transaction.
    fn create_padded(g: &mut PersistentGraph, n: i64) -> Result<()> {
        g.begin()?;
        let created = (0..n).try_for_each(|i| {
            let mut p = none();
            p.insert("i".into(), Value::Int(i));
            p.insert("pad".into(), Value::String("x".repeat(200)));
            g.create_node(&["N".into()], p).map(|_| ())
        });
        match created {
            Ok(()) => g.commit(),
            Err(e) => {
                g.rollback();
                Err(e)
            }
        }
    }

    #[test]
    fn transaction_size_cap() {
        let dir = Dir::new("txcap");
        // 16-page cache, 64-page (512 KiB) cap.
        let opts = PersistentOptions::new(&dir.0).cache_bytes(0).max_transaction_bytes(64 * 8192);
        let mut g = PersistentGraph::open(opts.clone()).unwrap();
        g.create_index(IndexDef::node("ix", "N", "i")).unwrap();
        create_padded(&mut g, 100).unwrap();

        // About 250 bytes a node: 5000 nodes need far more than 64 pages.
        let err = create_padded(&mut g, 5000).unwrap_err();
        assert!(matches!(err, Error::TransactionTooLarge(_)), "{err}");
        assert!(err.to_string().contains("64 pages"), "{err}");
        // Rolled back exactly, and the cache is back within its capacity.
        assert_eq!(g.node_count(), 100);
        assert_eq!(g.index_seek("N", "i", &Value::Int(99)).unwrap().map(|v| v.len()), Some(1));
        {
            let p = g.pager_mut();
            assert!(p.cached_pages() <= p.capacity(), "{} > {}", p.cached_pages(), p.capacity());
        }
        // A write outside an explicit transaction is a transaction of its own, capped too.
        let big = Value::List((0..100_000).map(Value::Int).collect());
        let ids: Vec<NodeId> = g.nodes_with_label("N").map(|n| n.unwrap()).collect();
        let err = g.set_node_property(ids[0], "big", big).unwrap_err();
        assert!(matches!(err, Error::TransactionTooLarge(_)), "{err}");
        assert_eq!(g.node_property(ids[0], "big").unwrap(), None);

        // Smaller transactions are fine, and so is the same work in a store without a cap.
        for _ in 0..10 {
            create_padded(&mut g, 500).unwrap();
        }
        assert_eq!(g.node_count(), 5100);
        drop(g);
        let mut g = PersistentGraph::open(opts.max_transaction_bytes(0)).unwrap();
        assert_eq!(g.node_count(), 5100);
        create_padded(&mut g, 5000).unwrap();
        assert_eq!(g.node_count(), 10_100);
    }
}
