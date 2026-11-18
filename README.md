![GraphBuff logo](graphbuff_github_social.png "GraphBuff")
## Graph database engine written in Rust and queried with **Cypher**.

- **Storage:** in memory, hybrid (in-memory graph with a write-ahead log and snapshots) or
  disk-resident paged storage for graphs larger than RAM
- **Cypher:** query planner, streaming execution, indexes, constraints, transactions
- **Protocols:** HTTP/JSON and Bolt (Neo4j drivers), user accounts, TLS
- **Tools:** online backup, restore and consistency check; bulk CSV import (Rust API)
- **GPU:** graph kernels and algorithms on wgpu (Vulkan, Direct3D 12, Metal)

## Quick start

Without a Rust toolchain, start the server with the movies demo graph loaded:

```bash
docker run --rm -p 127.0.0.1:7475:7475 -p 127.0.0.1:7687:7687 \
       -e GRAPHBUFF_ADMIN_PASSWORD=change-me ghcr.io/janvencl/graphbuff --demo movies
```

HTTP/JSON is then on `http://127.0.0.1:7475` and Bolt on `bolt://127.0.0.1:7687`, for the
user `admin` with the password `change-me`; see
[Install without Rust](#install-without-rust) for prebuilt binaries and
[Data directory and logs](#data-directory-and-logs) for where the server keeps your data.

From source:

```bash
cargo run --release -p graphbuff-server                            # HTTP/JSON server on 127.0.0.1:7475, durable
cargo run --release -p graphbuff-server -- --demo movies           # ... with the built-in movies graph
cargo run --release -p graphbuff-server -- --bolt 127.0.0.1:7687   # ... plus Bolt for Neo4j drivers
cargo run --release -p graphbuff-server -- --data ./data --persistent # durable in ./data, disk-resident storage
cargo test --workspace
```

As a library:

```rust
use graphbuff::{Database, params};

let db = Database::in_memory();
db.execute("CREATE (:Person {name: 'Ann'})-[:KNOWS]->(:Person {name: 'Bob'})")?;
let r = db.execute_with_params(
    "MATCH (a:Person {name: $name})-[:KNOWS]->(b) RETURN b.name AS friend",
    &params! { "name" => "Ann" },
)?;
println!("{}", r.to_table());
```

## Install without Rust

### Docker

```bash
docker run --rm -p 127.0.0.1:7475:7475 -p 127.0.0.1:7687:7687 \
       -e GRAPHBUFF_ADMIN_PASSWORD=change-me ghcr.io/janvencl/graphbuff --demo movies
```

```bash
curl -s -u admin:change-me http://127.0.0.1:7475/query -H 'Content-Type: application/json'      -d '{"query": "MATCH (p:Person)-[:ACTED_IN]->(m:Movie) RETURN p.name, m.title"}'
```

The image `ghcr.io/janvencl/graphbuff` (tags: `latest` and each version, such as `0.1.0`;
linux/amd64 and linux/arm64) runs `graphbuff-server --bind 0.0.0.0:7475 --bolt 0.0.0.0:7687`
as an unprivileged user, with its databases in the volume `/data` and its log on standard
error (`docker logs`); anything after the image name is added to that command line.
`--demo movies` loads the graph of [examples/movies.cypher](examples/movies.cypher), which
is compiled into the binary, into the default database (unless it already holds data).

User accounts are on, kept in the system database in the `/data` volume, so clients log
in. On the first start, while there are no accounts, the server creates the administrator
`GRAPHBUFF_ADMIN_USER` (default `admin`) with the password in `GRAPHBUFF_ADMIN_PASSWORD`;
later starts ignore both, and the password is changed with Cypher (`ALTER CURRENT USER SET
PASSWORD FROM … TO …`). To keep the password out of the environment, use a secret file
instead:

```bash
docker run -d --name graphbuff -p 127.0.0.1:7687:7687 -v graphbuff-data:/data \
       -v ./admin-password.txt:/run/secrets/admin:ro \
       -e GRAPHBUFF_ADMIN_PASSWORD_FILE=/run/secrets/admin ghcr.io/janvencl/graphbuff
```

or, in Compose:

```yaml
services:
  graphbuff:
    image: ghcr.io/janvencl/graphbuff
    environment:
      GRAPHBUFF_ADMIN_PASSWORD_FILE: /run/secrets/graphbuff_admin
    secrets: [graphbuff_admin]
    volumes: [graphbuff-data:/data]
secrets:
  graphbuff_admin:
    file: ./admin-password.txt
volumes:
  graphbuff-data:
```

Without either, the server warns that nobody can log in; add the administrator offline
(the image has no shell, so name the binary):

```bash
docker run --rm -it -v graphbuff-data:/data --entrypoint graphbuff-server \
       ghcr.io/janvencl/graphbuff users add admin admin
```

`-e GRAPHBUFF_USERS=off` turns accounts off: then `-e GRAPHBUFF_TOKEN=s3cret` makes one token
the only login, and with neither anyone who reaches the ports may do anything. Who can
reach them is decided by `-p`: `-p 127.0.0.1:7475:7475` publishes a port to this machine
only. A bind mount for `/data` (`-v ./data:/data`) must be writable by the container's user,
uid 65532 (`sudo chown 65532 ./data` on Linux).

Name the volume to keep the databases when the container is removed (`--rm` above drops
them with the container):

```bash
docker run -d --name graphbuff -p 127.0.0.1:7475:7475 -p 127.0.0.1:7687:7687 \
       -v graphbuff-data:/data ghcr.io/janvencl/graphbuff
docker logs -f graphbuff                        # start-up, failed logins, errors
```

Add `--slow-query 500` to log queries that take 500 ms or more, `--in-memory` for a
throwaway server, or `-e GRAPHBUFF_LOG_DIR=/data/logs` to write log files into the volume.

Build the image yourself with `docker build -t graphbuff .`.

### Prebuilt binaries

Each [release](https://github.com/JanVencl/graphbuff/releases) carries one archive per
platform, with `graphbuff-server` inside a folder
`graphbuff-VERSION-PLATFORM/`:

| Platform | Archive |
|----------|---------|
| Linux x86-64 (static, any distribution) | `graphbuff-linux-x86_64.tar.gz` |
| macOS Apple silicon | `graphbuff-macos-arm64.tar.gz` |
| macOS Intel | `graphbuff-macos-x86_64.tar.gz` |
| Windows x86-64 | `graphbuff-windows-x86_64.zip` |

and `SHA256SUMS` with the checksums of all four. The file names carry no version, so
`https://github.com/JanVencl/graphbuff/releases/latest/download/graphbuff-linux-x86_64.tar.gz`
always points at the newest release (a specific one: `.../releases/download/v0.1.0/...`).

```bash
curl -LO https://github.com/JanVencl/graphbuff/releases/latest/download/graphbuff-linux-x86_64.tar.gz
tar -xzf graphbuff-linux-x86_64.tar.gz
./graphbuff-*/graphbuff-server --demo movies    # http://127.0.0.1:7475
```

The binaries are plain builds: no `gpu` and no `tls` feature. Releases are cut by pushing a
tag `vX.Y.Z` that matches the workspace version ([release.yml](.github/workflows/release.yml)).

## Storage modes

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

## Multiple databases

A `DatabaseManager` holds several named databases, each with its own storage. Clients
use it through a `Session`, which has a current database and runs the database commands:

```cypher
CREATE DATABASE movies;                                   // default storage of the manager
CREATE DATABASE scratch IF NOT EXISTS OPTIONS {storage: 'memory'};
CREATE OR REPLACE DATABASE events
  OPTIONS {storage: 'hybrid', sync: 'os', checkpointAfterBytes: 16777216};
SHOW DATABASES;            // name, storage, default, current, nodes, relationships
SHOW DATABASE movies;
SHOW DEFAULT DATABASE;
USE movies;                               // switch the session's current database
USE scratch MATCH (n) RETURN count(n);    // run one query elsewhere, without switching
DROP DATABASE scratch IF EXISTS;
```

```rust
use graphbuff::DatabaseManager;

let mgr = DatabaseManager::open("data")?;   // or DatabaseManager::in_memory()
let mut s = mgr.session();                  // starts on the default database, `graphbuff`
s.execute("CREATE OR REPLACE DATABASE movies")?;
s.execute("USE movies")?;
s.execute("CREATE (:Movie {title: 'Heat'})")?;
```

* **`CREATE OR REPLACE DATABASE`** drops an existing database with all its data and
  creates it again empty, with the given options; if it doesn't exist it is just created.
  It cannot be combined with `IF NOT EXISTS`. Plain `CREATE DATABASE` fails if the name is
  taken; `IF NOT EXISTS` leaves the existing database alone.
* **Options:** `storage` is `'memory'`, `'hybrid'` or `'persistent'`.
  `sync` (`'commit'` or `'os'`) and `checkpointAfterBytes` apply to hybrid and persistent
  storage, `cacheBytes` and `maxTransactionBytes` (the transaction size cap) to persistent
  storage. Without `OPTIONS`, a manager opened on a
  directory creates hybrid databases (persistent ones with `graphbuff-server --persistent`, or
  `DatabaseManager::open_with(dir, StorageKind::persistent())`) and an in-memory manager
  creates memory databases. `OPTIONS $map` works too, as does `$name`
  for the database name. `WAIT`/`NOWAIT` and `DESTROY DATA` are accepted and ignored.
* **Names** are case-insensitive, 1–63 characters (letters, digits, `.`, `-`, `_`),
  start with a letter, and need backticks if they contain `-`. Windows device names such
  as `con` are rejected because names are also folder names.
* **The default database** (`graphbuff`) cannot be dropped, but it can be replaced.
  Dropping a session's current database moves that session back to the default one.
* **On disk**, `data/catalog` lists the databases and their storage settings, and each
  hybrid or persistent database lives in `data/databases/<name>/`. Memory databases stay in the catalog
  across restarts but come back empty. A `LOCK` file keeps a second process out.
* A plain `Database` has no other databases, so it rejects `USE` and database commands
  with an `Unsupported` error.

## Bulk import

`DatabaseManager::import_database` builds a new persistent database from CSV files in the
`neo4j-admin database import` format. It writes store records directly: no Cypher, no
redo log, no transaction size cap. On 100,000 nodes and 1,000,000 relationships (with
properties) it takes about 9 s, where `LOAD CSV` with `CREATE` takes about 140 s. The
server must not be running on the data directory (the catalog lock keeps it out).

```text
people_header.csv   id:ID(Person),name,born:int,tags:string[],:LABEL
follows.csv         :START_ID(Person),:END_ID(Person),since:date
```

* **Headers:** `name:ID(space)` or `:ID` (the import id; with a name it is also stored as
  a property), `:LABEL`, `:START_ID(space)`, `:END_ID(space)`, `:TYPE`, `:IGNORE`, and
  properties `name:type` where the type is `int`, `long`, `float`, `double`, `boolean`,
  `string`, `point`, `date`, `localtime`, `time`, `localdatetime`, `datetime` or
  `duration`, or an array such as `string[]`. A plain `name` is a string. The first
  record of a group's first file is the header; the group's other files have none.
* **Options** (`ImportOptions`): delimiter, array delimiter (also separates labels,
  default `;`), id type (string or integer), skipping bad relationships or duplicate
  nodes, indexes built after loading from sorted keys, cache size, fsync.
* **Target:** an existing database is replaced only if it is empty, or with overwrite. A
  failed import leaves it as it was.
* **Bare stores:** `PersistentGraph::bulk_loader`
  plus `import_csv` for a bare store. `BulkLoader` can also be filled directly
  (`create_node`, `create_relationship`, `create_index`, `finish`).

Nothing is durable until the import finishes. While it runs, the store directory holds an
`IMPORTING` marker, and opening a directory that still has one fails: delete it and import
again. Import ids are kept in memory (one map per id space), as are the label index and
property index keys until they are written. Constraints are not created by the import; add
them with `CREATE CONSTRAINT` afterwards (with an index from the import, it seeks instead of
building its own).

## Backup, restore and consistency checks

A backup is a directory that opens as a store. It holds the last commit before the backup
started, and is written while queries and transactions keep running and committing (it
reads a snapshot, copying pages a batch at a time without filling the page cache).
Persistent databases back up as persistent stores (their pages and metadata); in-memory
and hybrid ones as hybrid stores (a snapshot file).

Start the server with `--backup-dir DIR`; then admins can:

* **Back up:** `POST /db/{name}/backup` with `{"backup": "monday"}` writes `DIR/monday`.
* **Restore:** `POST /db/{name}/restore` with `{"backup": "monday", "overwrite": false}`
  creates (or replaces) the database from it.
* **Check:** `POST /db/{name}/check` checks a persistent database.

From Rust: `DatabaseManager::backup_database`, `restore_database` and `check_database`.

* **Restore** copies the backup (it stays usable) and checks a persistent one before the
  database is created. An existing database is replaced only if it is empty or with
  overwrite, and stays as it was if the restore fails.
* **Check** (persistent storage only) reads every record and verifies
  adjacency lists in both directions (links, list ends, endpoints, relationship groups of
  dense nodes, degrees and self-loop counts), node, relationship, label and type counts,
  blob chains and free lists, every B+tree (page use, key order, leaf links) against the
  entries the records call for, index counts, and constraints. Lost space (blocks neither
  used nor free) is a warning. 200,000 nodes and 400,000 relationships: check 1.0 s, backup
  0.4 s (102 MiB).
* **Safety:** a backup goes into a new or empty directory only, and holds a
  `BACKUP_INCOMPLETE` marker until it is complete; stores refuse to open (and restore
  refuses to read) a directory that still has one.
* **From Rust:** `Database::backup` / `Database::check`,
  `DatabaseManager::backup_database` / `restore_database` / `check_database`, and
  `GraphStore::backup` / `GraphStore::check` (call them on a snapshot for an online run).

## CSR adjacency export

`Database::csr()` returns the last committed state's relationships in compressed sparse row
form (`graphbuff::Csr`), the layout GPU kernels and graph algorithms work on:
nodes get dense `u32` indexes in id order, and each relationship type has an outgoing and
an incoming `CsrAdjacency` (`offsets`, `targets`, `rels`), rows sorted by target.

```rust
let csr = db.csr()?;
let follows = csr.type_id("FOLLOWS").unwrap();
let out = csr.adjacency(follows, graphbuff::Direction::Outgoing);
let i = csr.node_index(alice).unwrap();
for &j in out.neighbors(i) {
    println!("{}", csr.node_id(j).0);
}
```

The export is built on the first call after a commit, on a snapshot (writers keep
committing), and shared until the next commit. 100,000 nodes and 1.3 million
relationships: 0.26 s in memory, 0.6 s on disk, 32 MiB (`cargo run --release -p graphbuff
--example csrbench`). `Csr::build` exports any `GraphStore`.

### Kernels

`graphbuff::Kernels` is the interface an execution backend implements over the export;
`CpuKernels` is the CPU implementation (threads for independent work) and the fallback
when there is no GPU. They work on dense node indexes and flat arrays:

* `expand`: a batch of source rows to their relationships (`ExpandBatch`: input row,
  target, relationship), in `Csr::expand` order.
* `propagate`: one hop of walk counts per node (a whole-graph multi-hop `count(*)`; walks
  may reuse a relationship, Cypher paths may not). `bfs`: hop distances up to a depth.
* `filter`: a numeric property `Column` (`Column::build(store, &csr, "age")`) compared with
  a constant, three-valued like Cypher (`Mask`, with `and`/`or`/`not`; `label_mask` for
  labels); `select` keeps the rows that pass.
* `aggregate`: `count(*)`, `count`, `sum`, `avg`, `min`, `max` over rows, equal to the
  engine's results; `aggregate_counts` over walk counts. `top_n`: `ORDER BY … LIMIT n`.

```rust
let k = graphbuff::CpuKernels::new();
let age = db.read(|s| graphbuff::Column::build(s, &csr, "age"))?;
let all: Vec<u32> = (0..csr.node_count() as u32).collect();
let adults = k.select(&all, &k.filter(&age, graphbuff::CmpOp::Ge, &18.into()));
let stats = k.aggregate(&age, &adults); // stats.avg, stats.max, ...
```

On the `csrbench` graph (CPU, 100,000 nodes): a two-hop `sum` 6 ms (Cypher 15.6 s), a
filtered aggregate 1.5 ms (69 ms), `ORDER BY … LIMIT 10` 3 ms (68 ms).

### Graph algorithms

Procedures run graph algorithms as kernels over the export (cached per committed state; in
a transaction, built from what it sees). Each takes an optional configuration map with
`relationshipTypes` (a type or a list) and, where it applies, `orientation`
(`'NATURAL'`, `'REVERSE'`, `'UNDIRECTED'`):

| Procedure | Yields | Settings |
|---|---|---|
| `algo.pageRank(config)` | `node, score` | `maxIterations` (20), `dampingFactor` (0.85), `tolerance` (1e-7); Neo4j GDS formula |
| `algo.wcc(config)` | `node, componentId` | weakly connected components; the id of the smallest node in each |
| `algo.triangleCount(config)` | `node, triangleCount` | as an undirected simple graph |
| `algo.bfs(sourceNode, config)` | `node, depth` | from a node or a list; `maxDepth` (-1 = none); nearest first |

```cypher
CALL algo.pageRank({relationshipTypes: 'FOLLOWS'}) YIELD node, score
RETURN node.name, score ORDER BY score DESC LIMIT 10
```

On the `csrbench` graph (100,000 nodes, 1.3 million relationships, in memory): PageRank
104 ms, components 52 ms, triangles 184 ms.

### Kernels in query plans

Read queries use the kernels by themselves when the cost model expects them to be cheaper
(counting what the export and columns would cost to build if they are not cached yet).
`EXPLAIN` shows the choice:

```text
Select
  KernelAggregate count(*), sum(c.id) over (a)-[:FOLLOWS]->()-[:FOLLOWS]->(c) (cpu; cost … vs … by rows)
    Start
```

Two shapes are recognized, each as the whole `MATCH … RETURN`:

* aggregates (`count(*)`, `count`, `sum`, `avg`, `min`, `max` of the first or last node)
  over a pattern of up to two single relationships, with labels, constant property maps,
  and `WHERE` tests of one node at a time (comparisons with constants or parameters,
  `IS [NOT] NULL`, labels, `AND`/`OR`/`NOT`);
* `ORDER BY n.prop [DESC] [SKIP s] LIMIT l` over one node pattern.

Results are identical to the row engine: the row plan stays in the plan as a fallback and
runs whenever the kernels cannot be exact (a property that is not a number; `sum`, `avg`,
`min`, `max` over non-integers or sums near 2^53, whose results depend on the order of
rows). Queries that write and statements in a `Transaction` always run on rows.
`Database::set_kernel_mode(KernelMode::Off | Auto | Always)` switches it;
`set_kernels` plugs in another backend; `kernel_stats()` counts runs and fallbacks.
On the `csrbench` graph, Cypher's two-hop sum drops from 14 s to 8 ms, the filtered
aggregate from 63 to 2.4 ms and `ORDER BY … LIMIT 10` from 68 to 3.3 ms (in memory).

### GPU kernels

The `gpu` feature adds `WgpuKernels` (crate `graphbuff-gpu`), a backend on
[wgpu](https://wgpu.rs): Vulkan, Direct3D 12 or Metal, whichever the machine has.

```bash
cargo run --release -p graphbuff-server --features gpu -- --gpu
```

`--gpu` (`graphbuff-server`) puts every database on the GPU backend; in code,
`DatabaseManager::set_kernels(graphbuff::gpu_kernels()?)` or `Database::set_kernels`.
`EXPLAIN` then shows `(wgpu; …)`. Walk counts (multi-hop aggregates, `walk` / `propagate`),
BFS, connected components, triangle counts and, on devices with 64-bit floats, PageRank
run on the GPU, with the CPU's results exactly (PageRank bit for bit). Filters, aggregates,
Top-N and expansions stay on the CPU, as do kernels reading fewer than 262,144
relationships (`WgpuKernels::with_min_work`) and any the device cannot hold; after a device
error the backend runs everything on the CPU. On a Radeon RX 5700 XT against 4 CPU threads,
13 million relationships (`cargo run --release -p graphbuff --features gpu --example gpubench`):
three-hop walk counts 180 → 19 ms, BFS 510 → 21 ms, PageRank (20 iterations)
1,293 → 557 ms, Cypher's two-hop `count(*)` 154 → 31 ms.

On Windows with the GNU toolchain (`x86_64-pc-windows-gnu`), wgpu's Windows bindings need
an import-library tool and a linker that reads LLVM import libraries: put a copy of
`llvm-ar.exe` named `dlltool.exe` first on `PATH` (it is in rustup's `llvm-tools`
component), and link with the bundled `rust-lld`:

```bash
RUSTFLAGS="-C linker=rust-lld -C linker-flavor=ld.lld" cargo build --release -p graphbuff-server --features gpu
```

The MSVC toolchain and other platforms need nothing extra.

## HTTP server (Java and other languages)

`graphbuff-server` serves a `DatabaseManager` over HTTP/JSON, so any language can run
Cypher against graphbuff:

```bash
cargo run --release -p graphbuff-server                                  # durable, in the default data directory
cargo run --release -p graphbuff-server -- --data ./data --persistent    # in ./data, disk-resident storage
cargo run --release -p graphbuff-server -- --in-memory                   # nothing written to disk
cargo run --release -p graphbuff-server -- --bind 0.0.0.0:7475 --token s3cret
cargo run --release -p graphbuff-server -- --demo movies                 # start with the movies demo graph
```

| Request | Answer |
|---------|--------|
| `GET /health` | `{"status":"ok"}` (never needs the token) |
| `GET /db` | `{"databases":[{"name","storage","default","nodes","relationships"}]}` |
| `POST /db/{name}/query` | runs `{"query": "...", "params": {...}, "timeoutMs": 5000}` against database `name` (`params` and `timeoutMs` optional) |
| `POST /query` | the same, against the default database |
| `POST /db/{name}/tx` | begins a transaction: `201 {"transaction":{"id","database","path","timeoutMs"}}` |
| `POST /db/{name}/tx/{id}` | runs `{"query", "params", "timeoutMs"}` in it; answers like a query |
| `POST /db/{name}/tx/{id}/commit` | commits: `{"committed":true}` |
| `DELETE /db/{name}/tx/{id}` | rolls back (cancelling a running statement): `{"rolledBack":true}` |
| `/tx`, `/tx/{id}`, `/tx/{id}/commit` | the same, against the default database |
| `POST /db/{name}/backup` | online backup into `--backup-dir`: `{"backup": "name"}`; admins only ([details](#backup-restore-and-consistency-checks)) |
| `POST /db/{name}/restore` | creates the database from a backup: `{"backup": "name", "overwrite": false}`; admins only |
| `POST /db/{name}/check` | consistency check of a persistent database: `{"check":{"consistent","errors",...}}`; admins only |

A query returns `{"columns":[...],"rows":[[...]],"stats":{"nodesCreated":1,...,"containsUpdates":true}}`.
A query request is its own transaction. Database commands (`CREATE DATABASE`,
`SHOW DATABASES`, `USE x MATCH ...`) work through either query endpoint.

* **Transactions:** the statements of an explicit transaction may come in separate
  requests, on any connection, and see each other's changes; other clients see none of
  them until the commit. While one is open, other writers on its database wait (including
  `begin`), readers do not. A statement that fails rolls the whole transaction back, and
  its error carries `"rolledBack":true`. A statement that does not parse, a database
  command, or `USE` of another database is rejected and the transaction stays open. A
  transaction that gets no request for `--tx-timeout` seconds (default 60) is rolled
  back; requests to a transaction that has ended answer `404 TransactionNotFound`.
  Shutting the server down rolls back open transactions.
* **Timeouts:** `--query-timeout MS` stops a query that runs longer (default: no limit);
  a request may ask for less with `"timeoutMs"`. The limit also covers the wait for
  another writer, and the wait to begin a transaction. A query that runs out of time is
  rolled back and answers `408 Timeout`; in a transaction the whole transaction rolls
  back. Rolling back a transaction (and shutting down) cancels the statement it is
  running, which then answers `409 Cancelled`.

* **Values:** JSON integers become Cypher INTEGERs and numbers with a fraction or exponent
  become FLOATs, in both directions (floats are always written with `.0` or an exponent).
  NaN and infinities are returned as the strings `"NaN"`, `"Infinity"`, `"-Infinity"`.
  Nodes, relationships and paths are objects with a `"$type"`:
  `{"$type":"node","id":0,"labels":["Person"],"properties":{...}}`,
  `{"$type":"relationship","id":0,"type":"KNOWS","start":0,"end":1,"properties":{...}}`,
  `{"$type":"path","nodes":[...],"relationships":[...]}`.
* **Errors:** `{"error":{"code":"SyntaxError","message":"...","offset":9}}` with status 400
  (`SyntaxError`, `SemanticError`, `TypeError`, `RuntimeError`, `Unsupported`, `BadRequest`),
  404 (`DatabaseNotFound`, `TransactionNotFound`), 408 (`Timeout`), 409 (`ConstraintError`, `Cancelled`), 401 (missing or wrong credentials), 403
  (`Forbidden`: the user's roles do not allow the statement), 413 (body
  over 16 MiB, or `TransactionTooLarge`: a transaction past its database's size cap,
  rolled back), 500 (`StorageError`).
* **Security:** the server listens on localhost by default. With `--token` (or the
  `GRAPHBUFF_TOKEN` environment variable) every request except `/health` needs
  `Authorization: Bearer <token>`; with user accounts (on unless `--users off`), clients
  log in as users (see
  [Users and TLS](#users-and-tls)). `LOAD CSV` is off unless `--import-dir DIR` is
  given; it then reads only files inside `DIR`.
* **Implementation** (`crates/server`, standard library only, plus rustls with a TLS
  feature): one thread per
  connection (at most 256), HTTP/1.1 keep-alive, `Content-Length` or chunked bodies,
  `Expect: 100-continue`. `Server::start` embeds it in a Rust program.

From **Java**, [`examples/java/GraphbuffClient.java`](examples/java/GraphbuffClient.java) is
a dependency-free client (Java 17+, `java.net.http`) with a runnable demo:

```bash
java examples/java/GraphbuffClient.java http://127.0.0.1:7475
```

```java
var db = new GraphbuffClient("http://127.0.0.1:7475", null);   // or a token
db.query("CREATE DATABASE movies IF NOT EXISTS", null);
db.query("movies", "CREATE (:Person {name: $name, age: $age})", Map.of("name", "Ann", "age", 31));
var r = db.query("movies", "MATCH (p:Person) RETURN p.name AS name, p.age AS age", null);
String name = (String) r.get(0, "name");
long age = (Long) r.get(0, "age");          // INTEGER -> Long, FLOAT -> Double
```

With Jackson or Gson in the project, `HttpClient` plus your JSON library is all you need.
Send `{"query": ..., "params": ...}` and read `columns` / `rows`.

### Data directory and logs

The server keeps its databases on disk. Without `--data DIR` (or env `GRAPHBUFF_DATA`)
it uses:

| Platform | Data directory |
|----------|----------------|
| Linux | `$XDG_DATA_HOME/graphbuff`, usually `~/.local/share/graphbuff`; `/var/lib/graphbuff` when run as root |
| macOS | `~/Library/Application Support/graphbuff` |
| Windows | `%LOCALAPPDATA%\graphbuff` |
| Docker image | `/data` (a volume) |

The start-up line names the directory in use. New databases there use hybrid storage
(`--persistent`: disk-resident, for graphs larger than RAM). `--in-memory` keeps everything
in memory and writes nothing. Back up databases with the server's backup endpoint (see
[Backup, restore and consistency checks](#backup-restore-and-consistency-checks)).

Settings can also live in a config file, `graphbuff.conf`: one `key = value` per line, the
keys being the command-line options without `--`, `#` for comments, relative paths
relative to the file. The command line overrides the environment, which overrides the
file. `--config FILE` (or env `GRAPHBUFF_CONFIG`; `-` for none) picks the file; by default
it is:

| Platform | Config file |
|----------|-------------|
| Linux | `$XDG_CONFIG_HOME/graphbuff/graphbuff.conf`, usually `~/.config/graphbuff/graphbuff.conf`; `/etc/graphbuff/graphbuff.conf` when run as root |
| macOS | `~/Library/Application Support/graphbuff/graphbuff.conf` |
| Windows | `%LOCALAPPDATA%\graphbuff\graphbuff.conf` |
| Docker image | `/data/graphbuff.conf` |

A missing config file is created with every setting commented out. Example:

```text
data = /srv/graphbuff
log-dir = /var/log/graphbuff
bind = 0.0.0.0:7475
bolt = 0.0.0.0:7687
slow-query = 500
```

Logs go to `DATA/logs` (`--log-dir DIR` or env `GRAPHBUFF_LOG_DIR`; `-` for standard
error, the default with `--in-memory` and in the Docker image):

* `graphbuff.log`: start-up and settings, failed logins (with the client's address and
  user name), refused connections and storage errors; with `--log-level debug` also every
  connection. Levels: `error`, `warn`, `info` (default), `debug`; also env
  `GRAPHBUFF_LOG_LEVEL`.
* `query.log`: with `--slow-query MS`, every query that took at least `MS` milliseconds,
  with its duration, protocol, database and user. Parameters are never logged.

```text
2026-11-15T18:00:00.123Z INFO  graphbuff-server 0.3.0 listening on http://127.0.0.1:7475 (data in /home/ann/.local/share/graphbuff)
2026-11-15T18:00:05.004Z WARN  failed login (bolt) from 10.0.0.7:51234 as `admin`
2026-11-15T18:00:09.871Z SLOW  1532 ms bolt movies alice MATCH (a)-[*..6]->(b) RETURN count(b)
```

Each file moves to `.1` (older ones to `.2`, …) at 10 MiB; five old files are kept.
`graphbuff_server::Log` gives an embedded server the same log through `ServerConfig::log`.

### Stopping

SIGTERM or SIGINT (`docker stop`, `systemctl stop`, `kill`, Ctrl-C), or on Windows Ctrl-C,
Ctrl-Break or closing the console, stops the server cleanly: it stops accepting
connections, rolls back open explicit transactions, gives queries already running
`--shutdown-timeout SECS` (default 5) to finish and then cancels (and rolls back) the rest,
checkpoints every durable database so the next start has no log to replay, and exits 0 (1 if a checkpoint failed; that
database's log is kept and replayed on the next start). A
second signal ends it at once; committed data is safe either way, since every commit is in
the log. An embedded server gets the same steps from `Server::shutdown` (or dropping it),
with `ServerConfig::shutdown_grace`, followed by `DatabaseManager::checkpoint_all`.

## Bolt (Neo4j drivers)

With `--bolt ADDR` the server also speaks Bolt 4.4 and 5.0–5.4, the protocol of the
official Neo4j drivers:

```bash
cargo run --release -p graphbuff-server -- --bolt 127.0.0.1:7687 --token s3cret
```

```python
from neo4j import GraphDatabase
with GraphDatabase.driver("bolt://127.0.0.1:7687", auth=("neo4j", "s3cret")) as driver:
    records, summary, keys = driver.execute_query("MATCH (p:Person) RETURN p.name AS name")
```

* **Supported:** auto-commit queries and explicit transactions (`BEGIN`/`COMMIT`/
  `ROLLBACK`, several open results per transaction by `qid`), `PULL {n}` and `DISCARD`,
  pipelining, `RESET` (it interrupts the running statement), `LOGON`/`LOGOFF` (5.1+),
  `TELEMETRY` (5.4), `ROUTE` (this server is the whole cluster, so `neo4j://` URIs work),
  the `db` of a query or transaction, `tx_timeout`, and the 4.4 `utc` patch. Summaries
  carry counters under Neo4j's names, a query type, bookmarks, and `plan`/`profile` for
  `EXPLAIN`/`PROFILE`.
* **Values:** nodes, relationships (with element ids from Bolt 5, the strings
  `elementId()` returns) and paths,
  dates, times, date-times with offsets or named time zones, durations and points, in both
  directions. Byte arrays are refused.
* **Authentication:** `basic` with a user's name and password; with `--token`, also
  `basic` with the token as the password (any user name) or `bearer` with the token. With
  `--users off` and no token, any credentials pass.
* **Transactions and limits** behave as over HTTP: an explicit transaction holds its
  database's write lock until it ends, and is rolled back after `--tx-timeout` seconds
  without a message or when its connection closes; `--query-timeout` caps every statement.
  Error codes follow Neo4j's (`Neo.ClientError.Statement.SyntaxError`,
  `Neo.ClientError.Transaction.TransactionTimedOut`, ...).
* **Administration** goes to the `system` database, as with Neo4j (the name is
  reserved; it cannot be queried): `db: "system"` runs database and user commands only (`CREATE DATABASE`,
  `SHOW DATABASES`, `CREATE USER`, `GRANT ROLE`, …), in auto-commit or in a transaction
  (where each command takes effect when it runs, and a rollback does not undo it). Other
  databases also run them in auto-commit.
* **Tested with the official drivers:** Python 5.28 and 6.4, Java 5.28 and 6.0, by the
  smoke suites in [`integration-tests/drivers`](integration-tests/drivers/README.md).
* **Not done:** results are computed whole by `RUN` and then paged by `PULL`; impersonation,
  notification filters, and access modes (a write in a `READ` session runs). The server
  agent reads `Neo4j/5.4.0-graphbuff-…`, the form drivers expect.

## Users and TLS

User accounts are on unless `--users off`: clients log in with a user name and password,
HTTP Basic authentication (`Authorization: Basic …`) or Bolt's `basic` scheme. A `--token`
still works alongside, as an administrator. The accounts live in the system database of
the data directory (`DATA/system/`, synced on every commit; in memory with
`--in-memory`), as PBKDF2-HMAC-SHA-256 hashes (600,000 rounds, random salts).

```bash
# the first administrator, offline, while no server uses the data directory (password
# from GRAPHBUFF_PASSWORD, else asked for twice without echo on a terminal, else the
# first line of standard input); it finds the data directory as the server does
graphbuff-server users add root admin
graphbuff-server --bolt 127.0.0.1:7687
```

Or let the server create the first administrator when it starts without accounts
(Docker, Compose, CI, systemd): `GRAPHBUFF_ADMIN_USER` (default `admin`) and
`GRAPHBUFF_ADMIN_PASSWORD`, or `GRAPHBUFF_ADMIN_PASSWORD_FILE` naming a file that holds
the password (one trailing newline is ignored). Once there are accounts these are
ignored, so they may stay set; setting them with `--users off` is an error.

```powershell
# Windows: the same, asking without echo (PowerShell 7)
$env:GRAPHBUFF_ADMIN_PASSWORD = Read-Host -MaskInput 'password'
graphbuff-server
```

`GRAPHBUFF_USERS=off` (or `-`) turns accounts off from the environment; then only the
token, if set, logs in, and without one anyone who reaches the ports may do anything.

Each user holds roles; the strongest decides what they may do, checked for every
statement (a role granted or revoked applies to open connections at once):

| Role | May |
|------|-----|
| `reader` | read queries, `EXPLAIN`, `SHOW DATABASES`, explicit transactions that only read |
| `editor` | also write data, and create and drop indexes and constraints |
| `admin` | also create and drop databases, and manage users |

A user with no role may only log in, run `SHOW CURRENT USER` and change their own
password. Administrators manage accounts with Neo4j's commands (on the `system` database
over Bolt, or anywhere in auto-commit): `CREATE [OR REPLACE] USER name [IF NOT EXISTS]
SET [PLAINTEXT] PASSWORD 'pw' | $pw [CHANGE NOT REQUIRED]`, `ALTER USER name SET PASSWORD
…`, `RENAME USER a TO b`, `DROP USER name [IF EXISTS]`, `GRANT ROLE[S] r, … TO u, …`,
`REVOKE ROLE[S] r FROM u`, `SHOW USERS`, `SHOW ROLES`; every user may run `SHOW CURRENT
USER` and `ALTER CURRENT USER SET PASSWORD FROM 'old' TO 'new'`. Explicit HTTP
transactions belong to the client that began them. Not supported: custom roles and
privileges (`GRANT MATCH …`), `CHANGE REQUIRED`, suspended users, home databases.

**TLS** serves HTTPS and Bolt over TLS (TLS 1.3 and 1.2) on the same ports. It is an
optional build feature, with two choices of crypto provider for rustls:

```bash
cargo build --release -p graphbuff-server --features tls             # ring (needs a C compiler)
cargo build --release -p graphbuff-server --features tls-rustcrypto  # pure Rust (pre-release)
graphbuff-server --tls-cert cert.pem --tls-key key.pem --bolt 0.0.0.0:7687
```

Drivers then connect with `bolt+s://` (or `bolt+ssc://` for a self-signed certificate).
`--tls-allow-plain` also accepts clients without TLS on the same ports (the first byte
tells them apart); without it, plain HTTP requests get a `400` and plain Bolt
connections are closed.

## Layout

```
crates/
  core/    graphbuff-core    Value model, GraphStore trait, MemoryGraph, HybridGraph,
                             PersistentGraph (pager, B+tree, bulk loader), StorageMode
  cypher/  graphbuff-cypher  lexer, parser, AST
  engine/  graphbuff         executor: pattern matcher, expressions, aggregation, Database API,
                             DatabaseManager + Session (multiple databases), CSV import
  gpu/     graphbuff-gpu     GPU kernel backend (`WgpuKernels`, WGSL shaders); optional
  server/  graphbuff-server  HTTP/JSON and Bolt server (`graphbuff-server` binary + embeddable `Server`)
```

The **`GraphStore` trait** (`crates/core/src/store.rs`) is the seam between the query
engine and storage. Three stores implement it today. The GPU work sits beside it:
kernels over a read-only CSR export (`Kernels`, with CPU and wgpu backends), not a store.

Reads of entities, scans and traversals return `Result`, because a disk store can fail
to read a page. An I/O error or corrupt data fails the query (rolling back a write) with a
`StorageError` instead of producing a wrong answer. The catalog methods (`node_count`,
`labels`, `label_count`, `indexes`, …) stay infallible: every store keeps that metadata in
memory.

Full scans and label scans (`all_nodes`, `nodes_with_label`, `all_relationships`) return a
lazy `Scan` iterator instead of a `Vec`. The persistent store fills it in batches (one
record page, or 512 label-index keys, at a time), so scanning a huge label never holds it
in memory, and a match that stops early reads only what it needs.

### In-memory storage (`MemoryGraph`)
* Slab vectors of nodes/relationships indexed by id, with ids never reused.
* Index-free adjacency, grouped by relationship type: each node keeps one group per type
  with an outgoing and an incoming list of `(relationship, other node)` entries. A typed
  expansion reads only its group and never looks up relationship records, and `degree`
  (typed or not) is answered from list lengths. Removals leave tombstones that are
  compacted once they are the majority, so deleting a supernode is linear.
* Interned labels, relationship types and property keys.
* Label index (`label → sorted node set`) used by the planner to pick the cheapest scan.
* Optional single-property indexes (`(label, property, value) → nodes`). They are kept
  up to date on every write and rolled back with the rest of a failed transaction.
  Integral floats and integers share entries, so `n.id = 1.0` finds `id: 1`.
* Copy-on-write storage (`memory/cow.rs`): record slabs and adjacency lists are radix
  tries, the label and property indexes B-trees, all with reference-counted nodes. A
  clone of the graph shares everything and costs O(1); the first change after it copies
  only the touched paths. Snapshots for readers and rollback both use such clones.
  Every write query is atomic and rolls back on error.

### Concurrency
Queries that write run one at a time. Read queries never wait for them: each runs on a
snapshot of the last commit (`GraphStore::snapshot`), which the database takes after every
commit, so a reader sees all or nothing of each writing query. All three storage modes
take snapshots: the memory and hybrid stores clone their copy-on-write graph, the
persistent store reads page versions (see above). `Database::read`, which hands out the
store itself, still waits for a running write.

Several statements can run as one transaction:

```rust
let mut tx = db.begin()?;
tx.execute("MATCH (a:Account {id: 1}) SET a.balance = a.balance - 10")?;
tx.execute("MATCH (a:Account {id: 2}) SET a.balance = a.balance + 10")?;
tx.commit()?; // or tx.rollback(); dropping an open transaction rolls it back
```

Statements in a transaction see the changes of the ones before; nothing else does until
it commits. Other writers wait for it, readers do not. A statement that fails rolls the
whole transaction back (a syntax error does not). While a thread has a transaction open,
its own writes through `Database::execute` fail instead of waiting for themselves.

A query can have a time limit or be cancelled from another thread:

```rust
use graphbuff::{CancelHandle, QueryOptions};

let cancel = CancelHandle::new(); // clone it to another thread; `cancel.cancel()` stops the query
let options = QueryOptions::default().with_timeout(Duration::from_secs(5)).with_cancel(cancel.clone());
let result = db.execute_with_options("MATCH (a)-[*]->(b) RETURN count(*)", &params, &options);
// Err(Error::Timeout(_)) or Err(Error::Cancelled(_)); a writing query is rolled back
```

The limit counts from the call, including any wait for another writer
(`Database::begin_with_options` limits only that wait). The executor checks it once per
row or candidate it looks at, so a query stops within moments; work that does not go
row by row, such as building one huge list with `range()`, is not interrupted.
`Transaction::execute_with_options` and `Session::execute_with_options` take the same
options; a statement that stops rolls its transaction back.

### Execution
Each query part is compiled into a tree of operators (`crates/engine/src/plan.rs`), and
`Database::explain(query)` prints that tree, with each `MATCH` step's estimated rows
(per input row). Prefixing a query with `EXPLAIN` returns the same plan in
`QueryResult::plan` without running it. `PROFILE` runs it (updates included) and adds
the rows each operator produced and the time spent in it, without its inputs:

```text
PROFILE MATCH (a:Person)-[:KNOWS]->(b) RETURN b.name ORDER BY b.name LIMIT 2
Select [2 rows, 0.001 ms]
  Limit 2 [2 rows, 0.010 ms]
    Top 2 by b.name [2 rows, 0.050 ms]
      Project b.name [3 rows, 0.036 ms]
        Expand(All) (a)-[:KNOWS]->(b) (~2.4 rows) [3 rows, 0.097 ms]
          NodeByLabelScan (a:Person) (~4.0 rows) [4 rows, 0.063 ms]
            Start [1 row, 0.001 ms]
```

The CLI prints the plan above the result, and the HTTP API adds it as `plan`
(`{"profiled", "lines": [{"depth", "operator", "estimatedRows", "rows", "timeMs"}]}`). The estimates come from store statistics: node, label and
relationship-type counts, and index entry and distinct-value counts. A `MATCH` becomes a start step per pattern
part (`NodeIndexSeek`, `NodeByLabelScan`, `AllNodesScan`, or `Argument` for a bound
variable), then `Expand` / `VarLengthExpand` per relationship and `ProjectPath` for named
paths. Its `WHERE` is split at the `AND`s, and each part becomes a `Filter` right after
the step that binds the last variable it uses, so
`MATCH (a:P)-->(b)-->(c) WHERE a.id < 10` drops `a` candidates before expanding them, and
`MATCH (a:P), (b:P) WHERE a.id < 3 AND b.id < 3` filters each side before pairing them.

A pattern part that shares no variable with anything bound before it is computed once
and joined, instead of being matched again for every row on the left. An equality
between the two sides (`WHERE a.k = b.k + 1`) becomes a `HashJoin` key; with no such
equality it is a `CartesianProduct`. If the part could instead start with an index seek
on a value from the other side, it stays an index lookup per row.

Read operators run as pull-based streams (`crates/engine/src/stream.rs`): each asks its
input for at most as many rows as it can still use, so `MATCH (s)-[:LIKES]->(x) RETURN x
LIMIT 10` reads ten relationships, not the whole adjacency list. Sorting and aggregation
read all of their input. Updates are eager: everything below an update runs to
completion first, so `SET`/`DELETE`/`CREATE` always apply to every row, as in Neo4j.

A pattern part can start at either end, in one of these ways:

1. from a variable that is already bound,
2. with an **index seek** on an equality or `IN` constraint, taken either from an inline
   property map (`(u:User {id: $id})`) or from a `WHERE` conjunct (`WHERE u.id = a.id + 1`)
   whose right-hand side only uses variables bound by then; or an **index range seek** on
   comparisons (`WHERE u.age >= 18 AND u.age < 65`, combined into one range) or
   `STARTS WITH`. Range seeks return nodes in id order, like a label scan, so they never
   change the order of results,
3. with a scan of the smallest label,
4. with a scan of all nodes.

The planner chooses the order of comma-separated parts and the end each part starts
from by cost: it plans every remaining part both ways, estimates the rows of each step
(see `explain()`), and takes the cheapest, then repeats with what that part binds. So
`MATCH (x:Leaf), (h:Hub)-[:KNOWS]->(x)` starts at the one `:Hub` node, not at the
200,000 leaves, and a part connected to bound variables starts from them. With an
index, `MATCH (a), (b) WHERE b.id = a.id + 1` becomes one index seek per `a` instead of a
cartesian product. The full `WHERE` is
still evaluated afterwards, so indexes only narrow the candidates and never change
results. Relationships are unique within a `MATCH`, as Cypher requires. Read-only
queries share a read lock; write queries hold an exclusive one.

```cypher
CREATE INDEX user_id IF NOT EXISTS FOR (u:User) ON (u.id);
CREATE INDEX ON :User(email);          // legacy syntax; auto-named index_User_email
CREATE INDEX user_name FOR (u:User) ON (u.last, u.first);    // composite
CREATE INDEX rated_stars FOR ()-[r:RATED]-() ON (r.stars);   // relationship index
SHOW INDEXES;
DROP INDEX user_id;
DROP INDEX ON :User(email);
```

A composite index answers equality on all of its properties
(`MATCH (u:User {last: 'Doe', first: 'Ann'})`). A relationship index lets a pattern start
at the relationships it finds (`MATCH (a)-[r:RATED]->(m) WHERE r.stars = 5` or
`r.stars > 3`), binding both ends, and expands the rest of the pattern from there; the
planner picks it when it is the cheapest start. `EXPLAIN` shows these as
`NodeIndexSeek … on :User(last, first) =` and
`DirectedRelationshipIndexSeek … on [:RATED](stars) =`.

Constraints are checked at the end of every writing query, against the nodes it
created or changed, so a query may pass through an invalid state (such as swapping two
unique values) as long as it ends in a valid one. A violation fails the query with a
`ConstraintError` and rolls it back. Creating a constraint checks the existing data the
same way. `UNIQUE` and `NODE KEY` constraints seek on an index on their first property:
an existing one, or one they create under their own name, which is dropped with them.
Such an index cannot be dropped while the constraint needs it.

```cypher
CREATE CONSTRAINT user_id FOR (u:User) REQUIRE u.id IS UNIQUE;
CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS NOT NULL;   // named constraint_User_email
CREATE CONSTRAINT account IF NOT EXISTS FOR (a:Account) REQUIRE (a.bank, a.number) IS NODE KEY;
SHOW CONSTRAINTS;
DROP CONSTRAINT user_id;
```

## Supported Cypher

**openCypher TCK:** 3,894 of the 3,896 scenarios in the openCypher TCK 2024.3 pass (99.9 %).
The TCK is the official compatibility suite for openCypher. The two that fail expect a
compile-time error for a procedure argument of the wrong type; procedure arguments have no
declared types in graphbuff. The TCK
checks errors only by category, not by detail code. The suite and the full table per
feature are in [`integration-tests/tck`](integration-tests/tck/README.md).

* **Clauses:** `MATCH`, `OPTIONAL MATCH`, `WHERE`, `CREATE`, `MERGE` (+ `ON CREATE SET` / `ON MATCH SET`),
  `SET` (`n.p = v`, `n = {…}`, `n += {…}`, `n:Label`), `REMOVE`, `DELETE`, `DETACH DELETE`,
  `UNWIND`, `WITH`, `RETURN`, `DISTINCT`, `ORDER BY`, `SKIP`, `LIMIT`, `UNION [ALL]`,
  `FOREACH (x IN list | updates)`,
  `LOAD CSV [WITH HEADERS] FROM 'file:///data.csv' AS row [FIELDTERMINATOR ';']` (files inside
  the import directory only: `Database::set_import_dir` / `DatabaseManager::set_import_dir`,
  `--import-dir` for the server, the current directory by default in the CLI; each record is a
  list of strings, or a map by header; an unquoted empty field is `null`, `""` an empty string),
  `CALL { … }` subqueries (run once per row; import outer variables with `CALL (a, b) { … }`,
  `CALL (*) { … }` or a leading `WITH a, b`; they may change the graph, and a subquery without
  `RETURN` keeps each row once)
* **Procedures:** `CALL db.labels() YIELD label AS l WHERE l STARTS WITH 'P'`, anywhere in a
  query (one call per row; `YIELD` is required there). A query that is only a `CALL` returns
  every output, and may leave out `YIELD` and the parentheses (`CALL db.labels`; arguments then
  come from the parameters of the same names). Built in: `db.labels()`,
  `db.relationshipTypes()`, `db.propertyKeys()`, `db.indexes()`, `db.constraints()`,
  `db.awaitIndexes()`, `dbms.procedures()`, `dbms.components()`; `SHOW PROCEDURES` lists them.
  Applications add read-only procedures in Rust with `Database::register_procedure` (or
  `DatabaseManager::register_procedure` for every database):

  ```rust
  use graphbuff::{ProcedureSignature, Value};
  db.register_procedure(
      ProcedureSignature::new("my.range", ["n"], ["i"]).description("0 .. n-1"),
      |_ctx, args| {
          let n = match args[0] { Value::Int(n) => n, _ => 0 };
          Ok((0..n).map(|i| vec![Value::Int(i)]).collect())
      },
  )?;
  ```
* **Schema:** `CREATE [RANGE] INDEX [name] [IF NOT EXISTS] FOR (n:L) ON (n.p [, n.q …])`,
  `… FOR ()-[r:T]-() ON (r.p [, r.q …])` (relationship indexes), `CREATE INDEX ON :L(p [, q …])`,
  `DROP INDEX name [IF EXISTS]`, `DROP INDEX ON :L(p [, q …])`, `SHOW INDEXES`,
  `CREATE CONSTRAINT [name] [IF NOT EXISTS] FOR (n:L) REQUIRE n.p IS UNIQUE` (also `IS NOT NULL`,
  and `(n.a, n.b) IS UNIQUE | NODE KEY`; the older `ON … ASSERT …` form works too),
  `DROP CONSTRAINT name [IF EXISTS]`, `SHOW CONSTRAINTS`
* **Databases** (through a `Session`): `CREATE [OR REPLACE] DATABASE name [IF NOT EXISTS] [OPTIONS {…}]`,
  `DROP DATABASE name [IF EXISTS]`, `SHOW DATABASES`, `SHOW DATABASE name`, `SHOW DEFAULT DATABASE`,
  `USE name`, `USE name <query>`
* **Patterns:** labels, label expressions on nodes (`(n:A|B)`, `(n:A&B)`, `(n:A&!B)`, `(n:%)` for
  any label, `(n:!%)` for none, parentheses) and relationships (`[:A|B]`, `[:!A]`, `[:%]`; `[:A&B]`
  matches nothing), also in `WHERE n:A|B` and `WHERE r:T` (a relationship's type is its label;
  in a list comprehension's `WHERE`, write `(x:A|B)` since `|` starts the mapping),
  inline property maps, `$param` maps, directions `-->` `<--` `--`, variable length `*`, `*n`, `*min..max`, named paths `p = (…)`, pattern predicates in `WHERE`,
  `shortestPath((a)-[:T*]->(b))` and `allShortestPaths(…)` (a bidirectional breadth-first search;
  `WHERE` conditions on the path or its relationships are applied during the search, so the result
  is the shortest path that passes them)
* **Expressions:** integer literals in decimal, hexadecimal (`0x1F`) and octal (`0o17`), floats
  with or without integer digits (`.5`, `1e-3`), arithmetic, string concatenation, comparison chains, three-valued `AND/OR/XOR/NOT`,
  `IN`, `STARTS WITH`, `ENDS WITH`, `CONTAINS`, `=~` (Java-style regular expressions, whole-string
  match, flags such as `(?i)`; no lookaround or backreferences), `IS [NOT] NULL`, `IS [NOT] [NFC|NFD|NFKC|NFKD] NORMALIZED`, indexing and slicing, maps, lists,
  `CASE`, list comprehensions, pattern comprehensions (`[(a)-[:KNOWS]->(b) WHERE b.age > 30 | b.name]`,
  also with a named path), map projections (`n {.name, .*, age: n.born + 1, other}` on nodes,
  relationships and maps; `null` stays `null`), `reduce(acc = init, x IN list | expr)`, `all/any/none/single`, parameters, `EXISTS { … }`, `COUNT { … }` and
  `COLLECT { … }` subqueries (a pattern with an optional `WHERE`, or a full read-only query; they
  see every outer variable)
* **Aggregates:** `count`, `count(*)`, `sum`, `avg`, `min`, `max`, `collect`, `stdev`, `stdevp`,
  `percentileCont`, `percentileDisc` (with `DISTINCT`), implicit grouping by the items without
  aggregates. Next to an aggregate, only grouping keys, their properties, literals and
  parameters may appear (`RETURN me.age + count(*)` without `me` as a key is an error, as in
  Neo4j); a map projection groups by its own variable (`RETURN p {.name, friends:
  collect(f.name)}` gives a row per `p`)
* **Functions:** `id`, `elementId`, `labels`, `type`, `keys`, `properties`, `startNode`, `endNode`, `nodes`,
  `relationships`, `length`, `size`, `degree`, `head`, `last`, `tail`, `range`, `reverse`, `coalesce`, `isEmpty`,
  `toUpper` / `upper`, `toLower` / `lower`, `trim` / `btrim`, `ltrim`, `rtrim` (each with an optional
  string of characters to trim), `replace`, `substring`, `left`, `right`, `split`, `char_length`,
  `character_length`, `normalize(s [, NFC|NFD|NFKC|NFKD])` (Unicode 17.0), `toString`, `toStringOrNull`, `toInteger`, `toFloat`, `toBoolean` (and their
  `…OrNull` forms), `toIntegerList`, `toFloatList`, `toBooleanList`, `toStringList`, `nullIf`,
  `valueType`, `randomUUID`, `abs`, `sign`, `ceil`, `floor`, `round` (also
  `round(x, precision[, mode])` with `UP`, `DOWN`, `CEILING`, `FLOOR`, `HALF_UP` (default),
  `HALF_DOWN`, `HALF_EVEN`, rounding the number as written: `round(2.675, 2)` is `2.68`), `sqrt`,
  `exp`, `log`, `log10`, `sin`, `cos`, `tan`, `cot`, `asin`, `acos`, `atan`, `atan2`, `haversin`,
  `degrees`, `radians`, `isNaN`, `pi`, `e`, `rand`, `timestamp`, `exists`
* **`apoc`-style basics,** built in (no plugin): `apoc.coll.sum`, `apoc.coll.avg`, `apoc.coll.min`,
  `apoc.coll.max`, `apoc.coll.toSet`, `apoc.coll.sort`, `apoc.coll.flatten` (`true` for deep),
  `apoc.coll.contains`, `apoc.coll.indexOf`, `apoc.text.join`, `apoc.map.merge`,
  `apoc.map.fromPairs`. Other `apoc.*` names return `Unsupported`
* **Temporal values:** `date`, `localtime`, `time`, `localdatetime`, `datetime` and `duration`,
  built from ISO 8601 strings (`date('2015-07-21')`, `datetime('2015-07-21T21:40:32.142+01:00')`,
  `duration('P1Y2M10DT2H30M')`), from maps (`date({year: 2015, week: 30, dayOfWeek: 2})`,
  `datetime({date: d, hour: 10, timezone: '+02:00'})`, `duration({hours: 1.5})`), from each other
  (`date(dt)`), or with no argument for the current time (one clock per statement; also
  `date.statement()` and friends). Fields such as `d.year`, `d.week`, `t.hour`, `dt.offset`,
  `dt.epochMillis`, `dur.months`, `dur.minutesOfHour`. Arithmetic: temporal `±` duration (months
  first, clamped to the month's end; then days; then seconds), duration `±` duration, duration
  `*` and `/` a number; `duration.between(a, b)`, `duration.inMonths/inDays/inSeconds`,
  `datetime.fromepoch(s, ns)`, `datetime.fromepochmillis(ms)`, `date.truncate('week', d,
  {dayOfWeek: 2})` and the other `*.truncate` functions (`millennium` … `microsecond`). Output
  follows Neo4j: `12:00`, not `12:00:00`; durations drop trailing zeros (`PT0.5S`). Values of one kind compare (zoned
  values by instant: `datetime('…T01:00+01:00') = datetime('…T00:00Z')`); durations only test
  equality. They can be stored as properties (and in lists) and indexed for equality.
  **Time zones** are UTC offsets (`+01:00`, `+02:05:59`) or IANA names
  (`datetime('2015-07-21T21:40[Europe/Paris]')`, `datetime({…, timezone: 'Europe/Stockholm'})`,
  `datetime.statement('Asia/Tokyo')`). A named zone decides the offset from its rules and
  history (daylight saving time, local mean time before 1900): `dt.timezone` is the name,
  `dt.offset` the offset. A local time in a gap moves on by the gap; in an overlap the offset
  given (or else the earlier one) wins. Adding days or months moves the wall clock, adding
  hours moves the instant. The time zone database (IANA 2026f, all 598 zones) is built into
  the binary; `crates/core/tzdata/generate.py` rebuilds it from a newer release
* **Spatial values:** 2D and 3D points, Cartesian (`point({x: 1, y: 2})`,
  `point({x: 1, y: 2, z: 3})`) or WGS-84 (`point({longitude: 12.5, latitude: 56.1})`, with
  `height` for 3D), or chosen with `crs: 'cartesian' | 'cartesian-3d' | 'wgs-84' | 'wgs-84-3d'`
  or `srid: 7203 | 9157 | 4326 | 4979`. A null coordinate gives null. Fields `x`, `y`, `z`,
  `longitude`, `latitude`, `height`, `crs`, `srid`. `point.distance(a, b)`: straight line for
  Cartesian points, haversine metres for geographic ones (height difference added in 3D); null
  across coordinate systems. `point.withinBBox(p, lowerLeft, upperRight)`, where a geographic box
  may cross the 180th meridian. Points test equality but do not compare with `<` (null); `ORDER BY`
  sorts them after paths and before temporal values. They can be stored as properties (and in
  lists) and indexed for equality; HTTP returns them as
  `{"$type": "point", "crs": …, "srid": …, "x": …, "y": …}`

**Not yet supported** (these return an `Unsupported` error rather than a wrong result):
procedures that change the graph, `CALL { … } IN TRANSACTIONS`, `LOAD CSV` from URLs other than local files, relationship
constraints,
spatial index seeks for `point.distance` / `point.withinBBox` (they filter instead), the `trim(BOTH | LEADING | TRAILING 'x' FROM s)` form
(use `btrim` / `ltrim` / `rtrim` with a second argument).

## License

graphbuff is licensed under the GNU Affero General Public License v3.0 only
(`AGPL-3.0-only`). See [LICENSE](LICENSE).
