//! Core data model for graphbuff.
//!
//! This crate defines the pieces every other layer agrees on:
//!
//! * [`Value`] — the dynamically typed value used by queries and stored as properties,
//! * [`NodeId`] / [`RelId`] — stable entity identifiers,
//! * [`GraphStore`] — the storage abstraction the query engine runs against,
//! * [`MemoryGraph`] — the stage-1 in-memory implementation of [`GraphStore`],
//! * [`HybridGraph`] — a `MemoryGraph` made durable with a write-ahead log and snapshots,
//! * [`PersistentGraph`] — disk-resident paged storage with a bounded page cache,
//! * [`StorageMode`] / [`open_store`] — picking one of these at runtime,
//! * [`Csr`] — a compressed sparse row export of the adjacency, for GPU execution,
//! * [`Kernels`] / [`CpuKernels`] — batched expand, filter, aggregate and Top-N kernels over it.
//!
//! Later stages (persistent storage, GPU-accelerated execution) plug in by providing
//! additional [`GraphStore`] implementations, without touching the Cypher front end.

pub mod clock;
mod codec;
pub mod csr;
mod error;
pub mod kernels;
pub mod hybrid;
pub mod maintenance;
mod memory;
pub mod persistent;
pub mod spatial;
mod storage;
mod store;
pub mod temporal;
mod value;

pub use csr::{Csr, CsrAdjacency};
pub use error::{Error, Result};
pub use kernels::{
    Aggregates, Bits, CmpOp, Column, CpuKernels, ExpandBatch, Hop, Kernels, Mask, Num, label_mask,
};
pub use hybrid::{HybridGraph, HybridOptions, SyncPolicy};
pub use maintenance::{BackupInfo, CheckReport};
pub use memory::MemoryGraph;
pub use persistent::{BulkLoader, DEFAULT_MAX_TRANSACTION_BYTES, PersistentGraph, PersistentOptions};
pub use storage::{StorageMode, open_store};
pub use store::{
    Adjacency, BoxedStore, ConstraintDef, ConstraintKind, Direction, Expansion, GraphStore,
    IndexDef, IndexEntity, IndexStats, MAX_INDEX_PROPERTIES, Scan, Snapshot, ValueRange,
};
pub use spatial::{Crs, Point};
pub use temporal::{Duration, Temporal, TemporalKind};
pub use value::{NodeId, Path, RelId, Value, format_float, write_quoted};
