# Changelog

Notable changes per release. Releases are also tagged (`vX.Y.Z`); the tag messages carry
the same notes.

## Unreleased

### Added

- Point indexes: `CREATE POINT INDEX … FOR (n:L) ON (n.p)` (also on relationships).
  `MATCH` uses them to seek `point.distance(n.p, $c) < $r` and
  `point.withinBBox(n.p, $lo, $hi)` instead of filtering every candidate.
- `ALTER DATABASE … SET ACCESS READ ONLY | READ WRITE`, database aliases
  (`CREATE` / `ALTER` / `DROP ALIAS`, `SHOW ALIASES`) and `DROP DATABASE … DUMP DATA`
  (a backup in `dumps/` first). `SHOW DATABASES` gains `access` and `aliases` columns.
- Relationship constraints (`FOR ()-[r:T]-() REQUIRE … IS UNIQUE | IS NOT NULL |
  IS RELATIONSHIP KEY`) and property type constraints (`REQUIRE n.p IS :: INTEGER | FLOAT`)
  on nodes and relationships. `SHOW CONSTRAINTS` gains `entityType` and `propertyType`.
- A release archive for ARM64 Linux, `graphbuff-linux-aarch64.tar.gz` (static, musl).
- `graphbuff-cli`, a command-line client for `graphbuff-server` (line editor, Cypher
  highlighting, colors), shipped in the release archives and the Docker image.
- `CALL { … } IN TRANSACTIONS` for batched updates (`OF n ROWS`, `ON ERROR`,
  `REPORT STATUS`).
- Full-text indexes: `CREATE FULLTEXT INDEX`, `db.index.fulltext.queryNodes` and
  `queryRelationships`.
- `SHOW TRANSACTIONS` and `TERMINATE TRANSACTIONS`.
- `GET /metrics`: Prometheus metrics, for administrators and the token.
- Roles on one database: `GRANT ROLE r ON DATABASE db TO user`.
- Bolt 5.6–5.8 and the manifest handshake.
- Per-query memory limit, `queries.max_memory_mb` (default: a quarter of the machine's
  memory); a query over it fails with `MemoryLimit` and is rolled back.
- Failed logins lock a client out for a while (`security.max_failed_logins`,
  `security.login_lockout_seconds`).
- Approximate vector indexes (HNSW): `vector.hnsw.enabled`, `vector.hnsw.m` and
  `vector.hnsw.ef_construction` in `indexConfig`. Candidates are rescored exactly; indexes
  without these settings stay exact.
- `server.cors_origins`: browser apps on the listed origins may call the HTTP API (CORS
  headers and preflights).
- `security.log`: an audit trail of user, role and database changes, terminated
  transactions, backups and restores (who, from where, outcome), and failed logins.
- `SHOW FUNCTIONS` and `dbms.functions()`; `SHOW SETTINGS` on the server (administrators);
  `OPTIONAL CALL` for procedures and subqueries.
- `LOAD CSV` reads gzip files, and `http://` / `https://` URLs under the prefixes in
  `storage.load_csv_urls` (redirects only within them); `Database::set_csv_urls` for
  embedders. Bulk import reads `.csv.gz` too.
- `Database::execute_streaming` and `Session::execute_streaming` with a `RowSink`, for
  rows handed over as a read-only query produces them.

### Changed

- Release binaries and the Docker image are built with TLS support.
- Minimum Rust version 1.100.
- Results stream: an auto-commit read-only query sends its rows to Bolt `PULL`s as they
  are produced, and over HTTP a result past 64 KiB goes out as a chunked `200`; a
  failure after that ends the JSON with `"error"` instead of `"stats"`.
- Aggregation keeps only its groups, not its input rows, so it needs far less memory.
- `SHOW INDEXES` lists `vector.hnsw.enabled` for every vector index.
- Storage formats: persistent 11, hybrid snapshot and log 8 (point index definitions).
  Stores of the older formats still open.
- The documentation removed from the README is back as `docs/` pages; vector search is
  documented.

## 0.4.0 — 2026-11-19

### Breaking

- `graphbuff-server` takes its settings from one TOML file, `graphbuff.toml`, with
  sections `[server]`, `[storage]`, `[security]`, `[queries]` and `[logs]`. The
  command-line options are gone except `--config FILE` and `--help`; an old option is
  refused with the setting that replaces it. `graphbuff.conf`, `GRAPHBUFF_DATA`,
  `GRAPHBUFF_LOG_DIR` and `GRAPHBUFF_LOG_LEVEL` are no longer read.
- User accounts live in the system database of the data directory and are always on:
  every request except `/health` needs a login. `--users`, `GRAPHBUFF_USERS` and
  `users.txt` are gone; a 0.3 `users.txt` is not read, so create the accounts again.
- The server needs an administrator to start: from `GRAPHBUFF_ADMIN_USER` /
  `GRAPHBUFF_ADMIN_PASSWORD[_FILE]`, a prompt on a terminal, or the offline
  `graphbuff-server admin init`. Passwords need at least 8 characters.

### Server

- Argon2id password hashes; older hashes are replaced at the next login.
- Offline `admin init` and `admin reset-password` on a data directory no server is using.
- Release archives ship `config/graphbuff.toml`; the Docker image reads
  `/etc/graphbuff/graphbuff.toml`.

### Cypher

- Vector indexes: `CREATE VECTOR INDEX`, `db.index.vector.queryNodes` and
  `queryRelationships` (exact top-k), `vector.similarity.cosine` and `.euclidean`;
  scoring on the GPU with the `gpu` feature.
- openCypher TCK: all 3,896 scenarios pass.

### Storage

Persistent format 9, hybrid snapshot and log 6; 0.3 databases open as they are.

## 0.3.0 — 2026-11-17

### Breaking

- The CLI (shell, import, backup, restore, check) is gone; `graphbuff-server` is the only
  program. Backup, restore and consistency checks run on a live server over HTTP; bulk
  CSV import is `DatabaseManager::import_database` in Rust.
- The Docker image keeps user accounts, so clients log in. Set
  `GRAPHBUFF_ADMIN_PASSWORD` (or `GRAPHBUFF_ADMIN_PASSWORD_FILE`) on the first start to
  create the administrator.

### Server

- The first administrator from `GRAPHBUFF_ADMIN_USER` and
  `GRAPHBUFF_ADMIN_PASSWORD[_FILE]` when there are no users; the users file is written
  mode 600 on Unix.
- A config file, a default data directory with logs, and a clean shutdown on SIGTERM
  and Ctrl-C.

### Cypher

- openCypher TCK: 3,894 of 3,896 scenarios.
- Map projections, pattern comprehensions, label expressions, named time zones, temporal
  truncation, and stricter compile-time checks.

## 0.2.0 — 2026-11-15

- Relicensed under `AGPL-3.0-only`.
- A WebAssembly build of the engine for browsers (`crates/wasm`).
- Cypher: `normalize()` and `IS NORMALIZED`.
- Bolt: connections close gracefully, so clients get the final reply.

## 0.1.0 — 2026-11-14

First release.
