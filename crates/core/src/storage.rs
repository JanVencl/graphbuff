//! Choosing a storage backend at runtime.

use std::path::PathBuf;

use crate::hybrid::{HybridGraph, HybridOptions};
use crate::persistent::{PersistentGraph, PersistentOptions};
use crate::store::BoxedStore;
use crate::{MemoryGraph, Result};

/// How a database stores its graph. Each database picks its own mode when it is opened.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum StorageMode {
    /// Everything in memory; nothing survives the process. Fastest, no files.
    #[default]
    InMemory,
    /// Everything in memory, made durable by a write-ahead log and snapshots in a
    /// directory. The graph must fit in RAM; it is reloaded on open.
    Hybrid(HybridOptions),
    /// Disk-resident pages with a bounded page cache, for graphs larger than RAM.
    Persistent(PersistentOptions),
}

impl StorageMode {
    /// Hybrid storage in `dir` with default options (fsync on every commit).
    pub fn hybrid(dir: impl Into<PathBuf>) -> Self {
        StorageMode::Hybrid(HybridOptions::new(dir))
    }

    /// Persistent storage in `dir` with default options (fsync on every commit,
    /// 64 MiB page cache).
    pub fn persistent(dir: impl Into<PathBuf>) -> Self {
        StorageMode::Persistent(PersistentOptions::new(dir))
    }
}

/// Opens the store described by `mode`.
pub fn open_store(mode: &StorageMode) -> Result<BoxedStore> {
    Ok(match mode {
        StorageMode::InMemory => Box::new(MemoryGraph::new()),
        StorageMode::Hybrid(opts) => Box::new(HybridGraph::open(opts.clone())?),
        StorageMode::Persistent(opts) => Box::new(PersistentGraph::open(opts.clone())?),
    })
}
