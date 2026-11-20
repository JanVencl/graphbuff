# CSR adjacency export

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

## Kernels

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

## Graph algorithms

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

## Kernels in query plans

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

## GPU kernels

The `gpu` feature adds `WgpuKernels` (crate `graphbuff-gpu`), a backend on
[wgpu](https://wgpu.rs): Vulkan, Direct3D 12 or Metal, whichever the machine has.

```bash
cargo run --release -p graphbuff-server --features gpu   # with gpu = true in [queries]
```

`gpu = true` in `[queries]` of the server's config file puts every database on the GPU backend; in code,
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
