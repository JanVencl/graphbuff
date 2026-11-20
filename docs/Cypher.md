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
  the import directory only: `Database::set_import_dir` / `DatabaseManager::set_import_dir`,
  `storage.import_dir` for the server, the current directory by default in the CLI; each record is a
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
  fits `FLOAT` and `NUMBER`).
* **Schema:** `CREATE [RANGE] INDEX [name] [IF NOT EXISTS] FOR (n:L) ON (n.p [, n.q …])`,
  `… FOR ()-[r:T]-() ON (r.p [, r.q …])` (relationship indexes), `CREATE INDEX ON :L(p [, q …])`,
  `DROP INDEX name [IF EXISTS]`, `DROP INDEX ON :L(p [, q …])`, `SHOW INDEXES` (name, entity
  type, label or type, properties, `RANGE` or `VECTOR`, options), vector indexes (see
  *Vectors* below), `CREATE CONSTRAINT [name] [IF NOT EXISTS] FOR (n:L) REQUIRE n.p IS UNIQUE` (also `IS NOT NULL`,
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
  (`vecbench`). More in [Vector search](Vectors.md).

**Not yet supported** (these return an `Unsupported` error rather than a wrong result):
procedures that change the graph, `CALL { … } IN TRANSACTIONS`, `LOAD CSV` from URLs other than local files, relationship
constraints,
spatial index seeks for `point.distance` / `point.withinBBox` (they filter instead), approximate
vector search (HNSW, quantization), the `VECTOR` value type, `db.create.setNodeVectorProperty`
(use `SET n.p = $vector`), the `trim(BOTH | LEADING | TRAILING 'x' FROM s)` form
(use `btrim` / `ltrim` / `rtrim` with a second argument).
