# Layout

```
crates/
  core/    graphbuff-core    Value model, GraphStore trait, MemoryGraph, HybridGraph,
                             PersistentGraph (pager, B+tree, bulk loader), StorageMode
  cypher/  graphbuff-cypher  lexer, parser, AST
  engine/  graphbuff         executor: pattern matcher, expressions, aggregation, Database API,
                             DatabaseManager + Session (multiple databases), CSV import
  gpu/     graphbuff-gpu     GPU kernel backend (`WgpuKernels`, WGSL shaders); optional
  server/  graphbuff-server  HTTP/JSON and Bolt server (`graphbuff-server` binary + embeddable `Server`)
  cli/     graphbuff-cli     command-line client for the server (`graphbuff-cli` binary)
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

## In-memory storage (`MemoryGraph`)
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

## Concurrency
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

## Execution
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
read all of their input; aggregation keeps only one entry per group (its key, accumulators
and first row), not the rows. Updates are eager: everything below an update runs to
completion first, so `SET`/`DELETE`/`CREATE` always apply to every row, as in Neo4j.

The result of a read-only query streams too (`Database::execute_streaming`, a `RowSink`):
its rows go to the caller batch by batch as the top stream yields them, so the server
writes them to Bolt `PULL`s or a chunked HTTP response without holding the whole result.
A query that writes runs and commits first, then hands over its rows.

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
