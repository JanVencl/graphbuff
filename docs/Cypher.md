# Supported Cypher

**openCypher TCK:** all 3,896 scenarios in the openCypher TCK 2024.3 pass (100 %).
The TCK is the official compatibility suite for openCypher. The TCK
checks errors only by category, not by detail code. The suite and the full table per
feature are in [`integration-tests/tck`](../integration-tests/tck/README.md).

* **Clauses:** `MATCH`, `OPTIONAL MATCH`, `WHERE`, `CREATE`, `MERGE` (+ `ON CREATE SET` / `ON MATCH SET`),
  `SET` (`n.p = v`, `n = {…}`, `n += {…}`, `n:Label`), `REMOVE`, `DELETE`, `DETACH DELETE`,
  `UNWIND`, `WITH`, `RETURN`, `DISTINCT`, `ORDER BY`, `SKIP`, `LIMIT`, `UNION [ALL]`,
  `FOREACH (x IN list | updates)`,
  `LOAD CSV [WITH HEADERS] FROM 'file:///data.csv' AS row [FIELDTERMINATOR ';']` (files inside
  the import directory: `Database::set_import_dir` / `DatabaseManager::set_import_dir`,
  `storage.import_dir` for the server, the current directory by default in the CLI; `http://`
  and `https://` URLs that start with an allowed prefix: `storage.load_csv_urls` for the
  server, `set_csv_urls` with your own opener when embedded; gzip files such as `data.csv.gz`
  are decompressed as they are read; each record is a list of strings, or a map by header; an
  unquoted empty field is `null`, `""` an empty string),
  `CALL { … }` subqueries (run once per row; import outer variables with `CALL (a, b) { … }`,
  `CALL (*) { … }` or a leading `WITH a, b`; they may change the graph, and a subquery without
  `RETURN` keeps each row once)
* **Batched updates:** `CALL { … } IN [n] [CONCURRENT] TRANSACTIONS [OF n ROWS] [ON ERROR
  CONTINUE | BREAK | FAIL] [REPORT STATUS AS s]` runs the subquery in batches of input rows
  (1000 by default), each in a transaction of its own that commits before the next starts and
  sees the ones before it, so one query can load or delete more than one transaction should
  hold:

  ```cypher
  LOAD CSV WITH HEADERS FROM 'file:///people.csv' AS row
  CALL (row) { CREATE (:Person {id: toInteger(row.id), name: row.name}) } IN TRANSACTIONS OF 10000 ROWS

  MATCH (n:Old) CALL (n) { DETACH DELETE n } IN TRANSACTIONS
  ```

  The input rows are read from the state the query started on, a batch at a time. When a batch
  fails, `ON ERROR FAIL` (the default) stops with the error, keeping the batches committed
  before; `CONTINUE` goes on with the next batch, and `BREAK` runs no more. With `CONTINUE` or
  `BREAK`, the rows of a batch that did not commit go on with `null` for what the subquery
  returns, and `REPORT STATUS AS s` adds to every row a map `s` with `started`, `committed`,
  `transactionId` and `errorMessage`. A timeout or cancellation stops the query, keeping the
  committed batches. Statistics add up all committed batches. It runs only as a query of its
  own, not in an explicit transaction, and the clauses before and after it may only read.
  Batches run one at a time (`CONCURRENT` is accepted). One per query, not in a `UNION`, not
  nested in a subquery; no `PROFILE`, no `ON ERROR RETRY`.
* **Procedures:** `CALL db.labels() YIELD label AS l WHERE l STARTS WITH 'P'`, anywhere in a
  query (one call per row; `YIELD` is required there). A query that is only a `CALL` returns
  every output, and may leave out `YIELD` and the parentheses (`CALL db.labels`; arguments then
  come from the parameters of the same names). Built in: `db.labels()`,
  `db.relationshipTypes()`, `db.propertyKeys()`, the schema procedures (below), `db.indexes()`, `db.constraints()`,
  `db.awaitIndexes()`, `dbms.procedures()`, `dbms.functions()`, `dbms.components()`, the
  vector and full-text index procedures (`db.index.vector.*`, `db.index.fulltext.*`), the
  embedding procedures `vector.embedBatch(texts, model?, options?)`, `vector.chunk(text, model?, options?)`, `vector.embedChunks(text, model?, options?)` and `vector.models()`
  ([Text embeddings](Embeddings.md)), and
  the writing `db.create.setNodeVectorProperty(node, key, vector)` and
  `db.create.setRelationshipVectorProperty(relationship, key, vector)` (the vector checked:
  1 to 4096 finite numbers, stored as floats); `SHOW PROCEDURES` lists them, with their
  `mode` (`READ` or `WRITE`). A query that calls a write procedure is a writing query (it
  needs the editor role), and may end with a call to a `VOID` procedure. `OPTIONAL CALL proc(…) YIELD …` and `OPTIONAL CALL { … }` keep
  an input row the call returns nothing for, with the new variables null (no `YIELD … WHERE`
  after `OPTIONAL CALL`: filter in a `WITH … WHERE`).
* **Schema:** `db.schema.nodeTypeProperties()` (nodeType, nodeLabels, propertyName,
  propertyTypes, mandatory: per combination of labels) and `db.schema.relTypeProperties()`
  (relType, propertyName, propertyTypes, mandatory) give the properties and their types
  (Neo4j's names: `String`, `Long`, `Double`, `DoubleArray`, `Date`, …; `mandatory` when every
  node or relationship has it). `db.schema.visualization()` gives `nodes` (maps of a label's
  `name` and `count`) and `relationships` (maps of `type`, `start` and `end` labels: which
  types connect which labels); unlike Neo4j's, they are maps, not virtual nodes. They learn
  from the first 1000 nodes per label and relationships per type; pass
  `{sampleSize: n}` (or `-1` for all) to change that.
* **Introspection:** `SHOW [ALL | BUILT IN | USER DEFINED] FUNCTIONS` lists the built-in
  functions (name, category, signature, description, aggregating, isBuiltIn; filter with
  `CALL dbms.functions() YIELD … WHERE …`). On `graphbuff-server`, `SHOW SETTINGS [names]
  [YIELD … | WHERE …]` lists the config file's settings for administrators (name, value,
  isDynamic, defaultValue, description; the token's value is hidden).
  Applications add procedures in Rust with `Database::register_procedure` (or
  `DatabaseManager::register_procedure` for every database):

  ```rust
  use graphbuff::{ProcedureSignature, ProcedureType, Value};
  db.register_procedure(
      ProcedureSignature::new("my.range", ["n"], ["i"])
          .input_types([ProcedureType::Integer])
          .description("0 .. n-1"),
      |_ctx, args| {
          let n = match args[0] { Value::Int(n) => n, _ => 0 };
          Ok((0..n).map(|i| vec![Value::Int(i)]).collect())
      },
  )?;
  ```

  Parameters are `ANY` unless given types (`input_types`); an argument of another type
  fails the call, at compile time when it is a literal (`null` fits every type, an integer
  fits `FLOAT` and `NUMBER`). A procedure registered with `.writes()` may change the graph
  through its context (`ctx.set_property(&node, key, value)`, `ctx.add_label`,
  `ctx.remove_label`); the changes are made when the call returns, so the call for the next
  row sees them and the call itself does not. A read procedure that tries fails the query.
* **Schema:** `CREATE [RANGE] INDEX [name] [IF NOT EXISTS] FOR (n:L) ON (n.p [, n.q …])`,
  `… FOR ()-[r:T]-() ON (r.p [, r.q …])` (relationship indexes), `CREATE INDEX ON :L(p [, q …])`,
  `DROP INDEX name [IF EXISTS]`, `DROP INDEX ON :L(p [, q …])`, `SHOW INDEXES` (name, entity
  type, label or type, properties, `RANGE`, `VECTOR`, `FULLTEXT`, `POINT`, `TEXT` or `LOOKUP`,
  options), vector indexes (see *Vectors* below), full-text indexes (see *Full-text* below),
  point indexes (see *Spatial values* below), text indexes `CREATE TEXT INDEX [name] [IF NOT
  EXISTS] FOR (n:L) ON (n.p)` (or `FOR ()-[r:T]-() ON (r.p)`; one property; `MATCH` seeks them
  for `n.p STARTS WITH s`, `ENDS WITH s` and `CONTAINS s`, where a range index answers only
  `STARTS WITH`), lookup indexes `CREATE LOOKUP INDEX [name] [IF NOT EXISTS] FOR (n) ON EACH
  labels(n)` and `FOR ()-[r]-() ON [EACH] type(r)` (one of each; listed with null labels and
  properties, and accepted for compatibility: label and type scans need none), `CREATE CONSTRAINT [name] [IF NOT EXISTS] FOR (n:L) REQUIRE n.p IS UNIQUE` (also `IS NOT NULL`,
  `(n.a, n.b) IS UNIQUE | NODE KEY`, and property types `n.p IS :: INTEGER | FLOAT`
  (`IS TYPED`, `LIST<STRING NOT NULL>`); the older `ON … ASSERT …` form works too), the same
  on relationships `FOR ()-[r:T]-() REQUIRE r.p IS UNIQUE | NOT NULL | :: <type>` and
  `(r.a, r.b) IS RELATIONSHIP KEY`, `DROP CONSTRAINT name [IF EXISTS]`, `SHOW CONSTRAINTS`
  (name, type, label or type, properties, entity type, property type)
* **Databases** (through a `Session`): `CREATE [OR REPLACE] DATABASE name [IF NOT EXISTS] [OPTIONS {…}]`,
  `DROP DATABASE name [IF EXISTS] [DUMP DATA]`, `ALTER DATABASE name SET ACCESS READ ONLY | READ WRITE`,
  `SHOW DATABASES`, `SHOW DATABASE name`, `SHOW DEFAULT DATABASE`, `USE name`, `USE name <query>`,
  `CREATE [OR REPLACE] ALIAS a FOR DATABASE db`, `ALTER ALIAS a SET DATABASE TARGET db`,
  `DROP ALIAS a FOR DATABASE`, `SHOW ALIASES FOR DATABASES` ([Databases](Databases.md))
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
  `{"$type": "point", "crs": …, "srid": …, "x": …, "y": …}`. `CREATE POINT INDEX [name] [IF NOT
  EXISTS] FOR (n:L) ON (n.p)` (also `FOR ()-[r:T]-() ON (r.p)`; ``OPTIONS {indexConfig:
  {`spatial.cartesian.min`: [x, y], …}}`` is accepted and ignored) indexes point properties, and
  `MATCH` starts with an index seek for `point.distance(n.p, $c) < $r` (or `<=`, either way
  round) and `point.withinBBox(n.p, $lowerLeft, $upperRight)` in its `WHERE`. A grid per
  coordinate system, sized to the points, is kept in memory for the last commit and updated
  with each commit's changes; inside a transaction, and in queries that write, the query
  scans the label instead
* **Text embeddings:** `vector.embed(text, model?, options?)` returns the embedding of a text
  by a local model, in a server with models installed ([Text embeddings](Embeddings.md)).
* **Vectors:** a vector is a list of numbers. `vector.similarity.cosine(a, b)` and
  `vector.similarity.euclidean(a, b)` score in [0, 1] as Neo4j does (`(1 + cos) / 2`,
  `1 / (1 + d²)`). ``CREATE VECTOR INDEX name [IF NOT EXISTS] FOR (n:L) ON n.p OPTIONS
  {indexConfig: {`vector.dimensions`: 1536, `vector.similarity_function`: 'cosine'}}`` (or
  `'euclidean'`; also `FOR ()-[r:T]-() ON r.p`) indexes the entities whose property is a list
  of exactly that many numbers (non-zero for cosine); anything else is skipped, never refused.
  `CALL db.index.vector.queryNodes('name', k, $vector) YIELD node, score` (and
  `db.index.vector.queryRelationships(…) YIELD relationship, score`) returns the k most similar,
  best first. The index is exact: the same entities and scores as `ORDER BY
  vector.similarity.cosine(n.p, $vector) DESC LIMIT k`. Read queries share an f32 copy of the
  vectors of the last commit (about 4 bytes per dimension per entity, built on the first query
  and updated with each commit's changes); inside a transaction the query scans what the transaction sees. With `gpu = true` in `[queries]`
  the scores are computed on the GPU (the vectors stay on the device between queries), then the
  rows near the cut are re-scored exactly, so the results do not change: on an RX 5700 XT,
  200,000 768-dimensional vectors answer in about 6 ms against 43 ms on 4 CPU threads
  (`vecbench`). `MATCH (d:L) SEARCH d IN (VECTOR INDEX name FOR $vector [WHERE filter] LIMIT k)
  [SCORE AS s]` finds the k most similar that pass a filter on the properties the index
  declares (`… ON n.p WITH [n.a, n.b]`). More in [Vector search](Vectors.md).
* **Full-text:** ``CREATE FULLTEXT INDEX name [IF NOT EXISTS] FOR (n:A|B) ON EACH [n.p, n.q]
  [OPTIONS {indexConfig: {`fulltext.analyzer`: 'english'}}]`` (also `FOR ()-[r:A|B]-()`)
  indexes STRING and LIST<STRING> properties. `CALL db.index.fulltext.queryNodes('name',
  'matrix AND "blade runner"', {limit: 10}) YIELD node, score` (and `queryRelationships`)
  returns the matches best first, scored with BM25. Lucene's classic query syntax (`AND` /
  `OR` / `NOT`, `+` / `-`, phrases, `field:`, `*` / `?` wildcards, fuzzy `~`, boosts `^n`)
  and seven analyzers (`standard-no-stop-words`, `standard`, `english`, `simple`, `stop`,
  `whitespace`, `keyword`; `db.index.fulltext.listAvailableAnalyzers()`). The inverted index
  is kept in memory and updated with each commit. More in [Full-text search](FullText.md).

## Not supported

What is missing fails rather than giving a wrong result. Features graphbuff knows about
fail with an `Unsupported` error (HTTP `400`, `"code":"Unsupported"`); syntax it does not
know yet, mostly from Neo4j 5 and GQL, fails as a `SyntaxError` at the first word it does not
recognise. Where there is another way to write it, it is given.

**Clauses and patterns** (syntax errors):

| Missing | Instead |
|---------|---------|
| Quantified path patterns: `(a)-[:R]->{1,3}(b)`, `((a)-[:R]->(b)){1,2}`, `-[:R]->+` | variable-length relationships: `(a)-[:R*1..3]->(b)` |
| `SHORTEST k`, `ALL SHORTEST`, `ANY` path selectors | `shortestPath(…)`, `allShortestPaths(…)` |
| Match modes: `DIFFERENT RELATIONSHIPS`, `REPEATABLE ELEMENTS` | relationships are never repeated within a match, as in `DIFFERENT RELATIONSHIPS` |
| GQL clauses: `FINISH`, `FILTER`, `LET`, `WHEN … THEN`, `NEXT`, standalone `ORDER BY` / `OFFSET` | `WITH … WHERE`, `WITH x AS y`, `CASE`, `SKIP` |
| Dynamic labels and types: `n:$(expr)`, `[:$(expr)]` | none yet |
| Type predicates: `x IS :: STRING` | `valueType(x) = 'STRING NOT NULL'` |
| `USE graph.byName(…)` | `USE name` |
| `SEARCH` on a full-text index (`Unsupported`) | `db.index.fulltext.queryNodes` / `queryRelationships` |

**Schema:**

| Missing | Error |
|---------|-------|
| Property type constraints on `MAP`, `NODE`, `PATH`, `ANY` or nullable list elements (`LIST<INTEGER>`) | syntax error: properties cannot hold them, and lists of properties hold no nulls |
| Quantization of vector indexes, vector indexes on several labels, the `VECTOR` value type and `vector()` | vectors are lists of numbers; approximate search is `vector.hnsw.enabled` ([Vector search](Vectors.md)) |
| Full-text ranges, regular expressions and phrase slop (`[a TO b]`, `/re/`, `"a b"~2`) | `Unsupported` |
| Index or constraint settings graphbuff does not know | `Unsupported` |

**Expressions and procedures:**

| Missing | Instead |
|---------|---------|
| `trim(BOTH \| LEADING \| TRAILING 'x' FROM s)` (syntax error) | `btrim(s, 'x')`, `ltrim(s, 'x')`, `rtrim(s, 'x')` |
| Regular expressions with lookaround, atomic groups, backreferences, possessive quantifiers or nested classes (`RuntimeError`) | rewrite without them |
| Write procedures that create or delete nodes and relationships (a procedure's context sets properties and labels only) | `CREATE` / `DELETE` around the call |
| `apoc.*` beyond the functions listed above (`Unsupported`) | the built-in functions |
| `LOAD CSV` from URLs not allowed by `storage.load_csv_urls`, or other than `file:`, `http:` and `https:` | download the file into the import directory |
| Two result columns with the same name (`RETURN 1 AS a, 2 AS a`, `SemanticError`) | distinct names |

**`CALL { … } IN TRANSACTIONS`** (`Unsupported`): inside a `UNION`, under `PROFILE`, and
`ON ERROR RETRY`.

**Administration:**

| Missing | Error |
|---------|-------|
| `SHOW PRIVILEGES`, `SHOW FUNCTIONS EXECUTABLE`, `SHOW ALIASES … YIELD` | `Unsupported` |
| `ALTER DATABASE` other than `SET ACCESS`, remote aliases (`AT 'url'`), composite databases, `TOPOLOGY` | `Unsupported` |
| `CREATE ROLE`, `DENY`, privileges (`GRANT TRAVERSE …`): roles are the fixed `reader`, `editor` and `admin` | syntax error |
| Account options: `SET HOME DATABASE`, `SET STATUS SUSPENDED`, `SET ENCRYPTED PASSWORD`, `SET AUTH`, `CHANGE REQUIRED`, `SHOW ROLES WITH USERS` | `Unsupported` |
| User and role commands, `SHOW` / `TERMINATE TRANSACTIONS`, `SHOW SETTINGS` in the embedded engine | `Unsupported`: they need `graphbuff-server` |
