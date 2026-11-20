# Storage modes

Each database chooses its storage when it is opened:

```rust
use graphbuff::{Database, HybridOptions, PersistentOptions, StorageMode, SyncPolicy};

let mem = Database::open(StorageMode::InMemory)?;              // volatile
let dur = Database::open(StorageMode::hybrid("data/movies"))?; // durable, fits in RAM
let fast = Database::open(StorageMode::Hybrid(
    HybridOptions::new("data/events")
        .sync(SyncPolicy::OsBuffered)          // no fsync per commit
        .checkpoint_after_bytes(16 << 20),     // snapshot once the log passes 16 MiB
))?;
let big = Database::open(StorageMode::Persistent(
    PersistentOptions::new("data/web")
        .cache_bytes(1 << 30),                 // 1 GiB page cache; the graph can be larger
))?;
```

| Mode | Where data lives | Survives restart | Status |
|------|------------------|------------------|--------|
| `InMemory` | RAM | no | done |
| `Hybrid` | RAM, plus a write-ahead log and snapshots in a directory | yes | done |
| `Persistent` | paged files on disk, with a bounded page cache (graphs larger than RAM) | yes | done |

`Database::open` returns a `Database<BoxedStore>`. When the mode is known at compile time,
`Database::with_store(HybridGraph::open(opts)?)` avoids the dynamic dispatch.

| | `Hybrid` | `Persistent` |
|---|---|---|
| Memory use | the whole graph | the page cache (`cache_bytes`, default 64 MiB) plus small metadata |
| Open time | load snapshot + replay log | read metadata (+ replay log after a crash) |
| Reads | `MemoryGraph` speed | cached pages at memory speed, misses read 8 KiB pages from disk |
| Log record per commit | logical operations (small) | full images of the changed pages (larger) |

**Hybrid storage** (`crates/core/src/hybrid.rs`) keeps a `MemoryGraph` and logs every
committed transaction as one checksummed record of logical operations. With
`SyncPolicy::EveryCommit` (the default) a query that returned is durable. A
checkpoint (`db.checkpoint()`, or automatically once the log
passes `checkpoint_after_bytes`) writes a snapshot by temp-file + rename and empties the
log. On open, the snapshot is loaded and the log replayed. A torn last record from a crash
is cut off, and records already covered by the snapshot are skipped. A `LOCK` file keeps a
second process from opening the same directory. If a log write fails and cannot be undone,
the store refuses further writes until it is reopened.

**Persistent storage** (`crates/core/src/persistent.rs`) keeps the graph in paged files:

```
<dir>/nodes.db   fixed 64-byte node records at id * 64: blob pointer, heads/tails of the
                 outgoing and incoming relationship lists, degrees
<dir>/rels.db    fixed 64-byte relationship records: type, endpoints, prev/next in both
                 endpoints' lists (doubly linked, creation order), blob pointer
<dir>/blobs.db   labels and properties, in chains of 128-byte blocks with a free list
<dir>/index.db   B+tree pages: the label index and every property index
<dir>/groups.db  fixed 64-byte relationship groups of dense nodes: type, next group,
                 heads/tails of that type's two lists, degrees, self-loop count
<dir>/meta       id counters, counts, token tables, free lists, index roots (+ CRC)
<dir>/graph.wal  redo log
```

All page access goes through an LRU **page cache** (`persistent/pager.rs`). Pages changed
by the open transaction are pinned and their before-images kept, so rollback is exact and
uncommitted data never reaches a data file. Read queries run on a **snapshot** that shares
the cache: it reads a pinned page's before-image, and the cache keeps the images that
later commits replaced for as long as a snapshot old enough to need them lives. A commit appends the metadata plus the
after-image of every changed page to the redo log as one checksummed record (fsynced per
`SyncPolicy`); afterwards those pages may be evicted and written back at any time, but
never before their log record is on disk. A checkpoint (explicit, after
`checkpoint_after_bytes` of log, and on close) writes back all dirty pages, fsyncs, replaces
`meta` and empties the log. A transaction may change at most `max_transaction_bytes` of
pages (default 1 GiB; it keeps each changed page and its before-image in memory until it
ends). Changing one page more fails the statement with `Error::TransactionTooLarge` and
rolls the transaction back, instead of growing the cache without bound; `0` leaves only
the limit of one log record (about 4 GiB). On open, log records newer than `meta` are re-applied, which also
repairs data pages torn by a crash; a torn last record is ignored.

A node with more than `dense_threshold` relationships (`PersistentOptions::dense_threshold`,
default 50) becomes **dense**: its relationships are regrouped by type and the node record
points to its chain of group records instead. A typed expansion then walks only the
matching groups, and every degree is read from the group records. Stores written by an
earlier version (format v1) open unchanged; their nodes become dense on their next new
relationship, and the next checkpoint writes the v2 metadata.

Property indexes use the same normalization as `MemoryGraph` (`1` and `1.0` share an
entry). Index keys start with a part that sorts like the value: numbers as `f64`, strings
by their first 128 bytes. That is what range and `STARTS WITH` seeks scan. The exact
value follows; values whose encoding exceeds 256 bytes are indexed by a prefix plus
checksum, and seeks re-check those candidates. The metadata keeps each index's entry
and distinct-value counts (format v4), which the planner's estimates use. Stores from
before format v4 (unsorted keys, or no counts) rebuild their indexes once when opened, in
a single transaction.
