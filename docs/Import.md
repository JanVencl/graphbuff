# Bulk import

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
* **Compressed files:** a file that is gzip (`people.csv.gz`) is decompressed as it is
  read, here and in `LOAD CSV`.
* **Bare stores:** `PersistentGraph::bulk_loader`
  plus `import_csv` for a bare store. `BulkLoader` can also be filled directly
  (`create_node`, `create_relationship`, `create_index`, `finish`).

Nothing is durable until the import finishes. While it runs, the store directory holds an
`IMPORTING` marker, and opening a directory that still has one fails: delete it and import
again. Import ids are kept in memory (one map per id space), as are the label index and
property index keys until they are written. Constraints are not created by the import; add
them with `CREATE CONSTRAINT` afterwards (with an index from the import, it seeks instead of
building its own).
