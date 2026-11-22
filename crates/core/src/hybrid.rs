//! Hybrid storage: the whole graph is held in memory, and made durable with a
//! write-ahead log plus periodic snapshots.
//!
//! Reads and writes run at [`MemoryGraph`] speed. Every committed transaction is appended
//! to the log as one checksummed record of logical operations (`create node 7 with
//! labels …`, `set property …`). A checkpoint writes the full graph to a snapshot file
//! and empties the log. On open, the snapshot is loaded and the log is replayed on top.
//!
//! Directory layout:
//!
//! ```text
//! <dir>/LOCK            held for as long as the store is open (one process at a time)
//! <dir>/graph.snapshot  "GBSNAPv9", last covered sequence number, graph, CRC-32
//! <dir>/graph.wal       "GBWAL_v9", then records: len u32 | crc u32 | seq u64, ops…
//! ```
//!
//! Only the current format (version 9) is read: a snapshot or log written by an older
//! version is rejected on open.
//!
//! A record that was only partly written when the process died fails its length or
//! checksum check and is cut off on the next open, so the store always recovers to the
//! last fully committed transaction. Records whose sequence number is already covered by
//! the snapshot (a crash between writing the snapshot and emptying the log) are skipped.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::codec::{Decoder, Encoder, crc32};
use crate::persistent::older_format;
use crate::store::{Adjacency, ConstraintDef, Direction, GraphStore, IndexDef, Scan, Snapshot};
use crate::{Error, MemoryGraph, NodeId, RelId, Result, Value};

const SNAPSHOT_MAGIC: &[u8; 8] = b"GBSNAPv9";
const WAL_MAGIC: &[u8; 8] = b"GBWAL_v9";
const SNAPSHOT_FILE: &str = "graph.snapshot";
const SNAPSHOT_TMP: &str = "graph.snapshot.tmp";
const WAL_FILE: &str = "graph.wal";
const LOCK_FILE: &str = "LOCK";

/// When committed data is forced to stable storage.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum SyncPolicy {
    /// `fsync` the log on every commit. A committed transaction survives power loss.
    #[default]
    EveryCommit,
    /// Hand the log to the OS on every commit and `fsync` only on checkpoint and close.
    /// Survives a process crash, but the last commits can be lost on power loss.
    /// Much faster for many small write transactions.
    OsBuffered,
}

/// Settings for a [`HybridGraph`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HybridOptions {
    /// Directory holding the snapshot and the log. Created if missing.
    pub dir: PathBuf,
    pub sync: SyncPolicy,
    /// Take a checkpoint automatically once the log grows beyond this many bytes.
    /// `0` disables automatic checkpoints.
    pub checkpoint_after_bytes: u64,
}

impl HybridOptions {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        HybridOptions {
            dir: dir.into(),
            sync: SyncPolicy::default(),
            checkpoint_after_bytes: 64 << 20,
        }
    }

    pub fn sync(mut self, sync: SyncPolicy) -> Self {
        self.sync = sync;
        self
    }

    pub fn checkpoint_after_bytes(mut self, bytes: u64) -> Self {
        self.checkpoint_after_bytes = bytes;
        self
    }
}

/// One logical write, as recorded in the log. Entities are named by id and names are
/// stored as strings, so the log does not depend on the store's internal token numbering.
#[derive(Clone, Debug, PartialEq)]
enum Op {
    CreateNode {
        id: NodeId,
        labels: Vec<String>,
        props: BTreeMap<String, Value>,
    },
    CreateRel {
        id: RelId,
        start: NodeId,
        end: NodeId,
        rel_type: String,
        props: BTreeMap<String, Value>,
    },
    DeleteNode {
        id: NodeId,
        detach: bool,
    },
    DeleteRel(RelId),
    SetNodeProp {
        id: NodeId,
        key: String,
        value: Value,
    },
    SetRelProp {
        id: RelId,
        key: String,
        value: Value,
    },
    AddLabel {
        id: NodeId,
        label: String,
    },
    RemoveLabel {
        id: NodeId,
        label: String,
    },
    CreateIndex(IndexDef),
    DropIndex(String),
    CreateConstraint(ConstraintDef),
    DropConstraint(String),
}

impl Op {
    fn encode(&self, e: &mut Encoder) {
        match self {
            Op::CreateNode { id, labels, props } => {
                e.u8(1);
                e.u64(id.0);
                e.strings(labels);
                e.props(props);
            }
            Op::CreateRel {
                id,
                start,
                end,
                rel_type,
                props,
            } => {
                e.u8(2);
                e.u64(id.0);
                e.u64(start.0);
                e.u64(end.0);
                e.str(rel_type);
                e.props(props);
            }
            Op::DeleteNode { id, detach } => {
                e.u8(3);
                e.u64(id.0);
                e.bool(*detach);
            }
            Op::DeleteRel(id) => {
                e.u8(4);
                e.u64(id.0);
            }
            Op::SetNodeProp { id, key, value } => {
                e.u8(5);
                e.u64(id.0);
                e.str(key);
                e.value(value);
            }
            Op::SetRelProp { id, key, value } => {
                e.u8(6);
                e.u64(id.0);
                e.str(key);
                e.value(value);
            }
            Op::AddLabel { id, label } => {
                e.u8(7);
                e.u64(id.0);
                e.str(label);
            }
            Op::RemoveLabel { id, label } => {
                e.u8(8);
                e.u64(id.0);
                e.str(label);
            }
            Op::CreateIndex(def) if def.is_text() || def.is_lookup() => {
                e.u8(17);
                def.encode_tagged(e);
            }
            Op::CreateIndex(def) if def.is_point() => {
                e.u8(16);
                def.encode(e);
            }
            Op::CreateIndex(def) if def.is_fulltext() => {
                e.u8(15);
                def.encode_fulltext(e);
            }
            Op::CreateIndex(def) if def.is_vector() => {
                e.u8(14);
                def.encode_vector(e);
            }
            Op::CreateIndex(def) => {
                e.u8(13);
                def.encode(e);
            }
            Op::DropIndex(name) => {
                e.u8(10);
                e.str(name);
            }
            Op::CreateConstraint(def) => {
                e.u8(11);
                def.encode(e);
            }
            Op::DropConstraint(name) => {
                e.u8(12);
                e.str(name);
            }
        }
    }

    fn decode(d: &mut Decoder) -> Result<Op> {
        Ok(match d.u8()? {
            1 => Op::CreateNode {
                id: NodeId(d.u64()?),
                labels: d.strings()?,
                props: d.props()?,
            },
            2 => Op::CreateRel {
                id: RelId(d.u64()?),
                start: NodeId(d.u64()?),
                end: NodeId(d.u64()?),
                rel_type: d.str()?,
                props: d.props()?,
            },
            3 => Op::DeleteNode {
                id: NodeId(d.u64()?),
                detach: d.bool()?,
            },
            4 => Op::DeleteRel(RelId(d.u64()?)),
            5 => Op::SetNodeProp {
                id: NodeId(d.u64()?),
                key: d.str()?,
                value: d.value()?,
            },
            6 => Op::SetRelProp {
                id: RelId(d.u64()?),
                key: d.str()?,
                value: d.value()?,
            },
            7 => Op::AddLabel {
                id: NodeId(d.u64()?),
                label: d.str()?,
            },
            8 => Op::RemoveLabel {
                id: NodeId(d.u64()?),
                label: d.str()?,
            },
            13 => Op::CreateIndex(IndexDef::decode(d)?),
            14 => Op::CreateIndex(IndexDef::decode_vector(d)?),
            15 => Op::CreateIndex(IndexDef::decode_fulltext(d)?),
            16 => Op::CreateIndex(IndexDef::decode_point(d)?),
            17 => Op::CreateIndex(IndexDef::decode_tagged(d)?),
            10 => Op::DropIndex(d.str()?),
            11 => Op::CreateConstraint(ConstraintDef::decode(d)?),
            12 => Op::DropConstraint(d.str()?),
            t => return Err(d.corrupt(format!("unknown operation tag {t}"))),
        })
    }

    /// Re-applies a logged operation during recovery.
    fn replay(self, g: &mut MemoryGraph) -> Result<()> {
        fn same<T: PartialEq + std::fmt::Display>(what: &str, logged: T, got: T) -> Result<()> {
            if logged == got {
                Ok(())
            } else {
                Err(Error::Storage(format!(
                    "write-ahead log replay diverged: logged {what} {logged}, store assigned {got}"
                )))
            }
        }
        match self {
            Op::CreateNode { id, labels, props } => {
                same("node", id, g.create_node(&labels, props)?)
            }
            Op::CreateRel {
                id,
                start,
                end,
                rel_type,
                props,
            } => same(
                "relationship",
                id,
                g.create_relationship(start, end, &rel_type, props)?,
            ),
            Op::DeleteNode { id, detach } => g.delete_node(id, detach).map(drop),
            Op::DeleteRel(id) => g.delete_relationship(id).map(drop),
            Op::SetNodeProp { id, key, value } => g.set_node_property(id, &key, value),
            Op::SetRelProp { id, key, value } => g.set_relationship_property(id, &key, value),
            Op::AddLabel { id, label } => g.add_label(id, &label).map(drop),
            Op::RemoveLabel { id, label } => g.remove_label(id, &label).map(drop),
            Op::CreateIndex(def) => g.create_index(def),
            Op::DropIndex(name) => g.drop_index(&name).map(drop),
            Op::CreateConstraint(def) => g.create_constraint(def),
            Op::DropConstraint(name) => g.drop_constraint(&name).map(drop),
        }
    }
}

/// An in-memory graph made durable by a write-ahead log and snapshots.
/// See the [module docs](self) for the on-disk format and recovery rules.
#[derive(Debug)]
pub struct HybridGraph {
    mem: MemoryGraph,
    /// Operations of the open transaction; `None` when no transaction is open.
    pending: Option<Vec<Op>>,
    opts: HybridOptions,
    wal: File,
    /// Length of the valid part of the log; new records are written here.
    wal_len: u64,
    /// Sequence number of the last committed transaction.
    seq: u64,
    /// Set when a log write failed and could not be undone. The in-memory state may then
    /// differ from what is on disk, so further writes are refused until the store is reopened.
    poisoned: Option<String>,
    _lock: File,
}

fn io(path: &Path, what: &str) -> impl FnOnce(std::io::Error) -> Error {
    let ctx = format!("{what} {}", path.display());
    move |e| Error::io(ctx, e)
}

fn sync_dir(dir: &Path) -> Result<()> {
    // Makes a rename durable. Windows has no directory handles to sync; NTFS journals
    // the metadata change itself.
    #[cfg(unix)]
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io(dir, "syncing directory"))?;
    let _ = dir;
    Ok(())
}

impl HybridGraph {
    /// Opens (or creates) the store in `opts.dir` and recovers its committed state.
    pub fn open(opts: HybridOptions) -> Result<HybridGraph> {
        let dir = opts.dir.clone();
        fs::create_dir_all(&dir).map_err(io(&dir, "creating"))?;

        let lock_path = dir.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(io(&lock_path, "opening"))?;
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

        // A leftover temporary snapshot is from a checkpoint that never finished.
        let tmp = dir.join(SNAPSHOT_TMP);
        if tmp.exists() {
            fs::remove_file(&tmp).map_err(io(&tmp, "removing"))?;
        }

        let (mut mem, mut seq) = Self::load_snapshot(&dir.join(SNAPSHOT_FILE))?;

        let wal_path = dir.join(WAL_FILE);
        let mut wal = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&wal_path)
            .map_err(io(&wal_path, "opening"))?;
        let mut bytes = Vec::new();
        wal.read_to_end(&mut bytes)
            .map_err(io(&wal_path, "reading"))?;

        let wal_len = if bytes.len() < WAL_MAGIC.len() {
            // Empty, or the header itself was torn: start a fresh log.
            wal.set_len(0).map_err(io(&wal_path, "truncating"))?;
            wal.seek(SeekFrom::Start(0))
                .and_then(|_| wal.write_all(WAL_MAGIC))
                .and_then(|_| wal.sync_all())
                .map_err(io(&wal_path, "initializing"))?;
            WAL_MAGIC.len() as u64
        } else {
            if bytes[..8].starts_with(b"GBWAL_") && &bytes[..8] != WAL_MAGIC {
                return Err(older_format(&wal_path.display().to_string()));
            }
            if &bytes[..8] != WAL_MAGIC {
                return Err(Error::Storage(format!(
                    "{} is not a graphbuff write-ahead log",
                    wal_path.display()
                )));
            }
            let valid = Self::replay(&bytes, &mut mem, &mut seq)?;
            if valid < bytes.len() {
                // Cut off a torn tail left by a crash in the middle of a commit.
                wal.set_len(valid as u64)
                    .and_then(|_| wal.sync_all())
                    .map_err(io(&wal_path, "truncating torn tail of"))?;
            }
            valid as u64
        };

        Ok(HybridGraph {
            mem,
            pending: None,
            opts,
            wal,
            wal_len,
            seq,
            poisoned: None,
            _lock: lock,
        })
    }

    fn load_snapshot(path: &Path) -> Result<(MemoryGraph, u64)> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((MemoryGraph::new(), 0));
            }
            Err(e) => return Err(io(path, "reading")(e)),
        };
        let what = path.display().to_string();
        match bytes.get(..8) {
            Some(m) if bytes.len() >= 8 + 8 + 4 && m == SNAPSHOT_MAGIC => {}
            Some(m) if m.starts_with(b"GBSNAP") => return Err(older_format(&what)),
            _ => {
                return Err(Error::Storage(format!(
                    "{what} is not a graphbuff snapshot"
                )));
            }
        }
        let (body, tail) = bytes.split_at(bytes.len() - 4);
        if crc32(body) != u32::from_le_bytes(tail.try_into().unwrap()) {
            return Err(Error::Storage(format!(
                "{what} is corrupt: checksum mismatch"
            )));
        }
        let mut d = Decoder::new(&body[8..], &what);
        let seq = d.u64()?;
        let g = MemoryGraph::decode_snapshot(&mut d)?;
        if !d.is_empty() {
            return Err(d.corrupt("trailing data"));
        }
        Ok((g, seq))
    }

    /// Applies every complete record after the snapshot's sequence number. Returns the
    /// byte length of the valid prefix of the log.
    fn replay(bytes: &[u8], mem: &mut MemoryGraph, seq: &mut u64) -> Result<usize> {
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
            let what = format!("write-ahead log record at byte {pos}");
            let mut d = Decoder::new(payload, &what);
            let rseq = d.u64()?;
            let n = d.len(1)?;
            let ops = (0..n)
                .map(|_| Op::decode(&mut d))
                .collect::<Result<Vec<_>>>()?;
            if rseq > *seq {
                if rseq != *seq + 1 {
                    return Err(Error::Storage(format!(
                        "write-ahead log skips from transaction {} to {rseq}",
                        *seq
                    )));
                }
                for op in ops {
                    op.replay(mem).map_err(|e| match e {
                        Error::Storage(m) => Error::Storage(m),
                        other => Error::Storage(format!("replaying transaction {rseq}: {other}")),
                    })?;
                }
                *seq = rseq;
            }
            pos += 8 + len;
        }
        Ok(pos)
    }

    /// The directory this store lives in.
    pub fn dir(&self) -> &Path {
        &self.opts.dir
    }

    /// Current size of the write-ahead log in bytes.
    pub fn wal_size(&self) -> u64 {
        self.wal_len
    }

    /// Sequence number of the last committed (logged) transaction.
    pub fn last_sequence(&self) -> u64 {
        self.seq
    }

    fn writable(&self) -> Result<()> {
        match &self.poisoned {
            None => Ok(()),
            Some(why) => Err(Error::Storage(format!(
                "store is read-only after an earlier write failure ({why}); reopen the database"
            ))),
        }
    }

    /// Runs one store write. Inside a transaction its log entry is queued for `commit`;
    /// outside one (direct use of the store) the write is committed on its own.
    fn logged<T>(
        &mut self,
        write: impl FnOnce(&mut MemoryGraph) -> Result<T>,
        op: impl FnOnce(&T) -> Option<Op>,
    ) -> Result<T> {
        let auto = self.pending.is_none();
        if auto {
            self.begin()?;
        }
        match write(&mut self.mem) {
            Ok(v) => {
                if let Some(op) = op(&v) {
                    self.pending.as_mut().expect("transaction is open").push(op);
                }
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

    fn append(&mut self, ops: &[Op]) -> Result<()> {
        let mut e = Encoder::new();
        e.u64(self.seq + 1);
        e.len(ops.len());
        for op in ops {
            op.encode(&mut e);
        }
        let len = u32::try_from(e.buf.len())
            .map_err(|_| Error::Storage("transaction is too large for one log record".into()))?;
        let mut frame = Vec::with_capacity(8 + e.buf.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&crc32(&e.buf).to_le_bytes());
        frame.extend_from_slice(&e.buf);

        let start = self.wal_len;
        let sync = self.opts.sync == SyncPolicy::EveryCommit;
        let written = self
            .wal
            .seek(SeekFrom::Start(start))
            .and_then(|_| self.wal.write_all(&frame))
            .and_then(|_| if sync { self.wal.sync_data() } else { Ok(()) });
        match written {
            Ok(()) => {
                self.wal_len += frame.len() as u64;
                Ok(())
            }
            Err(err) => {
                // Remove whatever part of the record reached the file, so that the log
                // matches the rolled-back memory state. If even that fails, the two may
                // disagree: refuse further writes.
                let undone = self.wal.set_len(start).and_then(|_| self.wal.sync_data());
                if let Err(undo_err) = undone {
                    self.poisoned = Some(format!("{err}; then {undo_err}"));
                }
                Err(Error::io("writing the write-ahead log", err))
            }
        }
    }

    fn write_snapshot(&self) -> Result<()> {
        let mut e = Encoder::new();
        e.buf.extend_from_slice(SNAPSHOT_MAGIC);
        e.u64(self.seq);
        self.mem.encode_snapshot(&mut e);
        let crc = crc32(&e.buf);
        e.u32(crc);

        let dir = &self.opts.dir;
        let tmp = dir.join(SNAPSHOT_TMP);
        let mut f = File::create(&tmp).map_err(io(&tmp, "creating"))?;
        f.write_all(&e.buf)
            .and_then(|_| f.sync_all())
            .map_err(io(&tmp, "writing"))?;
        drop(f);
        let dst = dir.join(SNAPSHOT_FILE);
        fs::rename(&tmp, &dst).map_err(io(&dst, "replacing"))?;
        sync_dir(dir)
    }
}

/// Writes `mem` into the backup directory `dest` as a hybrid store: a snapshot file
/// covering sequence number `seq`, and an empty log.
pub(crate) fn backup_memory(mem: &MemoryGraph, seq: u64, dest: &Path) -> Result<crate::BackupInfo> {
    let mut e = Encoder::new();
    e.buf.extend_from_slice(SNAPSHOT_MAGIC);
    e.u64(seq);
    mem.encode_snapshot(&mut e);
    let crc = crc32(&e.buf);
    e.u32(crc);

    crate::maintenance::start_backup(dest)?;
    crate::maintenance::write_synced(&dest.join(SNAPSHOT_FILE), &e.buf)?;
    crate::maintenance::write_synced(&dest.join(WAL_FILE), WAL_MAGIC)?;
    crate::maintenance::finish_backup(dest)?;
    Ok(crate::BackupInfo {
        dir: dest.to_path_buf(),
        format: "hybrid",
        nodes: mem.node_count() as u64,
        relationships: mem.relationship_count() as u64,
        bytes: (e.buf.len() + WAL_MAGIC.len()) as u64,
    })
}

impl Drop for HybridGraph {
    fn drop(&mut self) {
        if self.opts.sync == SyncPolicy::OsBuffered {
            let _ = self.wal.sync_data();
        }
    }
}

impl GraphStore for HybridGraph {
    fn create_node(&mut self, labels: &[String], props: BTreeMap<String, Value>) -> Result<NodeId> {
        let logged = props.clone();
        self.logged(
            |g| g.create_node(labels, props),
            |&id| {
                Some(Op::CreateNode {
                    id,
                    labels: labels.to_vec(),
                    props: logged,
                })
            },
        )
    }

    fn create_relationship(
        &mut self,
        start: NodeId,
        end: NodeId,
        rel_type: &str,
        props: BTreeMap<String, Value>,
    ) -> Result<RelId> {
        let logged = props.clone();
        self.logged(
            |g| g.create_relationship(start, end, rel_type, props),
            |&id| {
                Some(Op::CreateRel {
                    id,
                    start,
                    end,
                    rel_type: rel_type.to_string(),
                    props: logged,
                })
            },
        )
    }

    fn delete_node(&mut self, id: NodeId, detach: bool) -> Result<bool> {
        self.logged(
            |g| g.delete_node(id, detach),
            |&done| done.then_some(Op::DeleteNode { id, detach }),
        )
    }

    fn delete_relationship(&mut self, id: RelId) -> Result<bool> {
        self.logged(
            |g| g.delete_relationship(id),
            |&done| done.then_some(Op::DeleteRel(id)),
        )
    }

    fn set_node_property(&mut self, id: NodeId, key: &str, value: Value) -> Result<()> {
        let logged = value.clone();
        self.logged(
            |g| g.set_node_property(id, key, value),
            |_| {
                Some(Op::SetNodeProp {
                    id,
                    key: key.to_string(),
                    value: logged,
                })
            },
        )
    }

    fn set_relationship_property(&mut self, id: RelId, key: &str, value: Value) -> Result<()> {
        let logged = value.clone();
        self.logged(
            |g| g.set_relationship_property(id, key, value),
            |_| {
                Some(Op::SetRelProp {
                    id,
                    key: key.to_string(),
                    value: logged,
                })
            },
        )
    }

    fn add_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        self.logged(
            |g| g.add_label(id, label),
            |&done| {
                done.then(|| Op::AddLabel {
                    id,
                    label: label.to_string(),
                })
            },
        )
    }

    fn remove_label(&mut self, id: NodeId, label: &str) -> Result<bool> {
        self.logged(
            |g| g.remove_label(id, label),
            |&done| {
                done.then(|| Op::RemoveLabel {
                    id,
                    label: label.to_string(),
                })
            },
        )
    }

    fn node_exists(&self, id: NodeId) -> Result<bool> {
        self.mem.node_exists(id)
    }
    fn relationship_exists(&self, id: RelId) -> Result<bool> {
        self.mem.relationship_exists(id)
    }
    fn node_labels(&self, id: NodeId) -> Result<Vec<String>> {
        self.mem.node_labels(id)
    }
    fn node_has_label(&self, id: NodeId, label: &str) -> Result<bool> {
        self.mem.node_has_label(id, label)
    }
    fn node_property(&self, id: NodeId, key: &str) -> Result<Option<Value>> {
        self.mem.node_property(id, key)
    }
    fn node_properties(&self, id: NodeId) -> Result<BTreeMap<String, Value>> {
        self.mem.node_properties(id)
    }
    fn relationship_type(&self, id: RelId) -> Result<Option<String>> {
        self.mem.relationship_type(id)
    }
    fn relationship_endpoints(&self, id: RelId) -> Result<Option<(NodeId, NodeId)>> {
        self.mem.relationship_endpoints(id)
    }
    fn relationship_property(&self, id: RelId, key: &str) -> Result<Option<Value>> {
        self.mem.relationship_property(id, key)
    }
    fn relationship_properties(&self, id: RelId) -> Result<BTreeMap<String, Value>> {
        self.mem.relationship_properties(id)
    }
    fn all_nodes(&self) -> Scan<'_, NodeId> {
        self.mem.all_nodes()
    }
    fn nodes_with_label(&self, label: &str) -> Scan<'_, NodeId> {
        self.mem.nodes_with_label(label)
    }
    fn label_count(&self, label: &str) -> usize {
        self.mem.label_count(label)
    }
    fn all_relationships(&self) -> Scan<'_, RelId> {
        self.mem.all_relationships()
    }
    fn expand(&self, node: NodeId, dir: Direction, types: &[String]) -> Adjacency<'_> {
        self.mem.expand(node, dir, types)
    }
    fn degree(&self, node: NodeId, dir: Direction, types: &[String]) -> Result<usize> {
        self.mem.degree(node, dir, types)
    }
    fn node_count(&self) -> usize {
        self.mem.node_count()
    }
    fn relationship_count(&self) -> usize {
        self.mem.relationship_count()
    }
    fn labels(&self) -> Vec<String> {
        self.mem.labels()
    }
    fn relationship_types(&self) -> Vec<String> {
        self.mem.relationship_types()
    }
    fn property_keys(&self) -> Vec<String> {
        self.mem.property_keys()
    }

    fn create_index(&mut self, def: IndexDef) -> Result<()> {
        let logged = def.clone();
        self.logged(|g| g.create_index(def), |_| Some(Op::CreateIndex(logged)))
    }

    fn drop_index(&mut self, name: &str) -> Result<bool> {
        self.logged(
            |g| g.drop_index(name),
            |&done| done.then(|| Op::DropIndex(name.to_string())),
        )
    }

    fn indexes(&self) -> Vec<IndexDef> {
        self.mem.indexes()
    }
    fn create_constraint(&mut self, def: ConstraintDef) -> Result<()> {
        let logged = def.clone();
        self.logged(|g| g.create_constraint(def), |_| Some(Op::CreateConstraint(logged)))
    }
    fn drop_constraint(&mut self, name: &str) -> Result<bool> {
        self.logged(
            |g| g.drop_constraint(name),
            |&done| done.then(|| Op::DropConstraint(name.to_string())),
        )
    }
    fn constraints(&self) -> Vec<ConstraintDef> {
        self.mem.constraints()
    }
    fn schema_index_stats(
        &self,
        entity: crate::IndexEntity,
        label: &str,
        properties: &[String],
    ) -> Option<crate::IndexStats> {
        self.mem.schema_index_stats(entity, label, properties)
    }
    fn schema_index_seek(
        &self,
        entity: crate::IndexEntity,
        label: &str,
        properties: &[String],
        values: &[Value],
    ) -> Result<Option<Vec<u64>>> {
        self.mem.schema_index_seek(entity, label, properties, values)
    }
    fn schema_index_range(
        &self,
        entity: crate::IndexEntity,
        label: &str,
        property: &str,
        range: &crate::ValueRange,
    ) -> Result<Option<Vec<u64>>> {
        self.mem.schema_index_range(entity, label, property, range)
    }
    fn relationship_type_count(&self, rel_type: &str) -> usize {
        self.mem.relationship_type_count(rel_type)
    }

    fn snapshot(&self) -> Option<Snapshot> {
        self.mem.snapshot()
    }

    fn backup(&self, dest: &Path) -> Result<crate::BackupInfo> {
        if self.pending.is_some() {
            return Err(Error::Runtime(
                "cannot back up a store with an open transaction; back up a snapshot".into(),
            ));
        }
        backup_memory(&self.mem, self.seq, dest)
    }

    fn begin(&mut self) -> Result<()> {
        self.writable()?;
        if self.pending.is_some() {
            return Err(Error::Runtime("a transaction is already open".into()));
        }
        self.mem.begin()?;
        self.pending = Some(Vec::new());
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let Some(ops) = self.pending.take() else {
            return Ok(());
        };
        if !ops.is_empty() {
            if let Err(e) = self.append(&ops) {
                self.mem.rollback();
                return Err(e);
            }
            self.seq += 1;
        }
        self.mem.commit()?;
        let limit = self.opts.checkpoint_after_bytes;
        if limit > 0 && self.wal_len > limit {
            // The commit itself is durable already; a failed checkpoint only leaves a
            // longer log, and is retried after the next commit.
            let _ = self.checkpoint();
        }
        Ok(())
    }

    fn rollback(&mut self) {
        self.pending = None;
        self.mem.rollback();
    }

    fn take_changes(&mut self) -> crate::Changes {
        self.mem.take_changes()
    }

    fn checkpoint(&mut self) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::Runtime(
                "cannot checkpoint while a transaction is open".into(),
            ));
        }
        self.writable()?;
        self.write_snapshot()?;
        // The snapshot now covers every logged transaction; empty the log. If this fails
        // the old records stay, and are skipped on recovery by their sequence numbers.
        let header = WAL_MAGIC.len() as u64;
        let wal_path = self.opts.dir.join(WAL_FILE);
        self.wal
            .set_len(header)
            .map_err(io(&wal_path, "truncating"))?;
        self.wal_len = header;
        self.wal.sync_all().map_err(io(&wal_path, "syncing"))
    }

    fn describe(&self) -> String {
        let sync = match self.opts.sync {
            SyncPolicy::EveryCommit => "fsync per commit",
            SyncPolicy::OsBuffered => "OS-buffered",
        };
        format!(
            "hybrid: in-memory + write-ahead log in {} ({sync})",
            self.opts.dir.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConstraintDef, ConstraintKind};

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!(
                "graphbuff-hybrid-{}-{name}",
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

    fn key(name: &str) -> ConstraintDef {
        ConstraintDef {
            name: name.into(),
            entity: crate::IndexEntity::Node,
            kind: ConstraintKind::Key,
            label: "P".into(),
            properties: vec!["a".into(), "b".into()],
        }
    }

    #[test]
    fn constraints_survive_the_log_and_snapshots() {
        let dir = Dir::new("constraints");
        {
            let mut g = HybridGraph::open(HybridOptions::new(&dir.0)).unwrap();
            g.create_constraint(key("k1")).unwrap();
            g.checkpoint().unwrap();
            g.create_constraint(key("k2")).unwrap_err(); // equivalent to k1
            g.begin().unwrap();
            g.drop_constraint("k1").unwrap();
            g.rollback();
            g.create_constraint(ConstraintDef { name: "k3".into(), label: "Q".into(), ..key("") })
                .unwrap();
        }
        let g = HybridGraph::open(HybridOptions::new(&dir.0)).unwrap();
        let names: Vec<String> = g.constraints().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["k1", "k3"]);
    }

    #[test]
    fn rejects_an_older_format() {
        let dir = Dir::new("older-format");
        {
            let mut g = HybridGraph::open(HybridOptions::new(&dir.0)).unwrap();
            g.create_node(&[], BTreeMap::new()).unwrap();
            g.checkpoint().unwrap();
        }
        let wal = dir.0.join(WAL_FILE);
        fs::write(&wal, b"GBWAL_v8").unwrap();
        let e = HybridGraph::open(HybridOptions::new(&dir.0)).err().expect("rejected");
        assert!(e.to_string().contains("no longer supported"), "{e}");

        fs::write(&wal, WAL_MAGIC).unwrap();
        let path = dir.0.join(SNAPSHOT_FILE);
        let mut bytes = fs::read(&path).unwrap();
        bytes[..8].copy_from_slice(b"GBSNAPv8");
        fs::write(&path, bytes).unwrap();
        let e = HybridGraph::open(HybridOptions::new(&dir.0)).err().expect("rejected");
        assert!(e.to_string().contains("no longer supported"), "{e}");
    }
}

/// A snapshot body (see [`crate::fuzzing`]).
#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_snapshot(bytes: &[u8]) {
    let mut d = Decoder::new(bytes, "snapshot");
    if d.u64().is_ok() {
        // Up to 2^16 slots: larger counts are allocated as asked (see decode_snapshot_limited).
        let _ = MemoryGraph::decode_snapshot_limited(&mut d, 1 << 16);
    }
}

/// A write-ahead log record, replayed onto an empty graph (see [`crate::fuzzing`]).
#[cfg(feature = "fuzzing")]
pub(crate) fn fuzz_wal_record(bytes: &[u8]) {
    let mut d = Decoder::new(bytes, "record");
    let (Ok(_), Ok(n)) = (d.u64(), d.len(1)) else { return };
    let mut g = MemoryGraph::new();
    for _ in 0..n {
        let Ok(op) = Op::decode(&mut d) else { return };
        if op.replay(&mut g).is_err() {
            return;
        }
    }
}
