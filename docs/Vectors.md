# Vector search

graphbuff stores embeddings as ordinary list properties and finds the nearest ones with
a vector index. The Cypher surface is the one Neo4j 5 uses, so existing queries and
drivers work unchanged.

- [Similarity functions](#similarity-functions)
- [Vector indexes](#vector-indexes)
- [Querying an index](#querying-an-index)
- [What gets indexed](#what-gets-indexed)
- [How it works](#how-it-works)
- [Limits](#limits)

## Similarity functions

| Function | Score |
|----------|-------|
| `vector.similarity.cosine(a, b)` | `(1 + cos θ) / 2` |
| `vector.similarity.euclidean(a, b)` | `1 / (1 + d²)`, where `d` is the Euclidean distance |

Both return a FLOAT in [0, 1]; higher is more similar. They work in any query, with or
without an index.

- Arguments are lists of INTEGER or FLOAT of the same length (at least one element).
- `null` for either argument gives `null`; so does cosine with a zero vector.
- Different lengths, non-numbers, NaN or infinity are errors.

```cypher
RETURN vector.similarity.cosine([1, 0], [0, 1]) AS orthogonal,     // 0.5
       vector.similarity.euclidean([0, 0], [3, 4]) AS far          // 1 / 26
```

## Vector indexes

```cypher
CREATE VECTOR INDEX doc_embedding IF NOT EXISTS
FOR (d:Doc) ON d.embedding
OPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.similarity_function`: 'cosine'}}
```

- `FOR ()-[r:TYPE]-() ON r.prop` indexes relationships instead of nodes.
- `vector.dimensions` (1 to 4096) is required. `vector.similarity_function` is `'cosine'`
  (default) or `'euclidean'`. `vector.hnsw.enabled`, `vector.hnsw.m` and
  `vector.hnsw.ef_construction` make the index approximate (see
  [Approximate indexes](#approximate-indexes-hnsw)). `indexProvider` is accepted and
  ignored; any other option is an error.
- One property per index. Without a name, the index is called `vector_<Label>_<prop>`.
- A vector index and a range index may sit on the same property.
- `SHOW INDEXES` and `CALL db.indexes()` list it with type `VECTOR` and its options;
  `DROP INDEX name` removes it.
- Index definitions are kept by every storage mode, and creating or dropping one is
  part of its transaction.

## Querying an index

```cypher
CALL db.index.vector.queryNodes('doc_embedding', 10, $embedding)
YIELD node, score
RETURN node.title, score
```

`db.index.vector.queryRelationships(name, k, vector)` yields `relationship, score`.

- Returns at most `k` entities, best score first; equal scores in id order.
- `k` must be at least 1. The query vector must have the index's dimensions (and, for
  cosine, not be all zeros). An unknown index, or a node index queried for
  relationships (or the other way round), is an error.
- Filter after the call with `WHERE`; the filter runs on the `k` results, so ask for
  more than you need when it is selective.
- Inside a transaction that has written, the query sees those writes.

The results of an index without `vector.hnsw` options are **exact**. When every `Doc` has a
valid embedding, they are the same rows, scores and order as this query, only much faster:

```cypher
MATCH (d:Doc)
RETURN d, vector.similarity.cosine(d.embedding, $embedding) AS score
ORDER BY score DESC, id(d) LIMIT 10
```

## Approximate indexes (HNSW)

An exact index scores every vector, so its queries slow down linearly as the index grows.
An approximate index searches a graph of the vectors (HNSW, as Neo4j's vector indexes do)
and looks at a small part of them:

```cypher
CREATE VECTOR INDEX doc_embedding FOR (d:Doc) ON d.embedding
OPTIONS {indexConfig: {`vector.dimensions`: 768, `vector.hnsw.enabled`: true}}
```

- `vector.hnsw.m` (2 to 512, default 16): neighbours each vector keeps in the graph (twice
  that on the bottom layer). More gives better recall and a slower build.
- `vector.hnsw.ef_construction` (1 to 3200, default 100): candidates each insertion
  considers. More gives a better graph and a slower build.
- Giving `m` or `ef_construction` turns HNSW on; `vector.hnsw.enabled: false` with either
  is an error. `SHOW INDEXES` lists all three.
- A query takes the best `max(4 × k, 200)` candidates the graph search finds and scores
  them exactly from the stored property, so every score is the exact score and the order
  is exact among the results. A true neighbour the search misses can be absent: on
  clustered 128-dimension vectors (100,000 of them), recall@10 is 0.99, at about 0.9 ms
  per query against 12 ms for the exact index.
- The graph is built in memory on the index's first query (about 5 seconds for those
  100,000 vectors, on one core; uniformly random vectors, the worst case, take several
  times longer), and again after a restart. Commits keep it current: a changed vector is
  inserted anew and its old node is marked deleted; once deleted nodes outnumber live
  ones, the graph is built again.
- Inside an explicit transaction, the index is searched exactly, as the transaction sees
  the data.
- Memory: the vectors as for an exact index, plus about `2 × m` links of 4 bytes per vector.

## What gets indexed

Nodes with the label (or relationships of the type) whose property is a list of exactly
`vector.dimensions` finite numbers; for cosine, not all zeros. Anything else, such as a
missing property, a string or a list of the wrong length, is skipped silently: writes
never fail because of a vector index. Set vectors with plain `SET`:

```cypher
MATCH (d:Doc {id: $id}) SET d.embedding = $embedding
```

## How it works

This is how an exact index works; for HNSW, see above.

- The index keeps each indexed vector as one row of 32-bit floats in memory (normalized
  for cosine), built from the committed data on its first query.
- Each commit updates only the rows of the entities it changed, so the index stays
  current without rebuilding.
- A query scores every row (multithreaded on the CPU, or on the GPU with
  `queries.gpu = true` and the `gpu` build), then re-scores the candidates near the
  k-th score from the stored property in 64-bit floats. That keeps the results exact
  despite the 32-bit copy.
- Memory: 4 bytes × dimensions × indexed entities (1 million 768-dimension vectors take
  about 3 GiB).

## Limits

- An exact search scores all vectors, so its cost grows linearly with their number; an
  HNSW index does not, but is approximate. There is no quantization, and no filtered
  search (a `WHERE` runs on the `k` results).
- The in-memory rows and HNSW graphs are rebuilt after a restart, on each index's first
  query; the HNSW build runs on one core.
- Building a relationship vector index scans all relationships.
- `db.create.setNodeVectorProperty` / `setRelationshipVectorProperty` are not available;
  use `SET`.
- The Neo4j 2025 `VECTOR` value type and `SEARCH` clause are not supported.
