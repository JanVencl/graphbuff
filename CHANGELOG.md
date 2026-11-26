# Changelog

Notable changes per release. Releases are also tagged (`vX.Y.Z`); the tag messages carry
the same notes.

## Unreleased

### Added

- Filtered vector search: `MATCH (d:Doc) SEARCH d IN (VECTOR INDEX name FOR $vector WHERE
  d.tenant = 5 LIMIT 10) SCORE AS score` finds the nearest entities that match a filter, on
  exact and HNSW indexes, for nodes and relationships. The properties to filter on are
  declared with the index: `CREATE VECTOR INDEX … ON d.embedding WITH [d.tenant, d.year]`.
  The syntax is Neo4j's (Cypher 25). See [Filtered search](docs/Vectors.md#filtered-search).
  Existing stores open as they are; 0.5 and older cannot open a store with such an index.
- The graphs of HNSW vector indexes are saved in the directories of hybrid and persistent
  databases, so that the first query after a restart does not build them again. See
  [Approximate indexes](docs/Vectors.md#approximate-indexes-hnsw).

- API tokens, for programs: `CREATE TOKEN laptop [FOR USER u] [ACCESS READ | WRITE]
  [ON DATABASE db, …] [EXPIRES IN n DAYS | HOURS | EXPIRES NEVER]`, `SHOW TOKENS`,
  `DROP TOKEN`, and offline `graphbuff-server tokens`. A token logs in as
  `Authorization: Bearer gbt_…` or with Bolt's `bearer` scheme, and may do what its user
  may, up to its own cap and never administration. It reads only and expires after 90 days
  unless made otherwise. See [Users and TLS](docs/Users.md).
- An MCP (Model Context Protocol) endpoint for AI agents such as Claude and Cursor: set
  `bind` in the new `[mcp]` section of the config file, and agents connect to
  `http://ADDRESS/mcp` with an API token. It speaks MCP 2026-07-28 and the
  `initialize`-based 2025-06-18 and 2025-11-25, without sessions. Tools: `list_databases`,
  `read_cypher` (refuses queries that write) and `write_cypher` (only for tokens that may
  write, with `dry_run`). Results are compact and capped (`max_rows`, `max_result_kb`), and
  a query is cancelled when its client leaves. Over MCP nobody is more than an editor:
  administration and `LOAD CSV` are refused. Tool calls are limited per token and user,
  and counted in `/metrics`. See [MCP](docs/MCP.md).

- Schema procedures, with Neo4j's names: `db.schema.nodeTypeProperties()`,
  `db.schema.relTypeProperties()` (properties and their types per label and relationship
  type) and `db.schema.visualization()` (which relationship types connect which labels).
  They sample 1000 nodes per label and relationships per type (`{sampleSize: n}`). The MCP
  endpoint has a `get_schema` tool built on them. See [Cypher](docs/Cypher.md).
- `graphbuff-server mcp`: MCP over standard input and output, for agents that start their
  MCP servers as programs (Claude Desktop). It passes messages to a running server's MCP
  endpoint (`--url`), with the API token in env `GRAPHBUFF_MCP_TOKEN`. See
  [MCP](docs/MCP.md#connecting-an-agent).
- MCP tools to explore a graph without writing Cypher: `get_node` (by id, or by label and
  property), `neighbors` (relationships per type, then neighbours spread over the types, or
  up to 3 hops) and `find_path` (shortest paths, up to 10 hops). See [MCP](docs/MCP.md).
- `vector.embedding_model` in a vector index's `indexConfig` records the embedding model of
  its vectors (checked against the installed models), shown by `SHOW INDEXES`. 0.5 and older
  cannot open a store with such an index. See [Vectors](docs/Vectors.md#vector-indexes).
- MCP `search` tool: nodes by meaning (vector index, the text embedded with the index's
  model), by words (full-text index) or both (reciprocal rank fusion), with filters on the
  index's filter properties. See [MCP](docs/MCP.md).
- OAuth 2.1 for hosted MCP clients (claude.ai, ChatGPT): with `mcp.public_url` set,
  `graphbuff-server` is its own authorization server. Users log in on its page and choose
  read or write and the databases; the client gets hour-long access tokens that work only
  at the MCP endpoint, and rotating refresh tokens. Clients are identified by client
  metadata documents or listed in `mcp.oauth_clients`; PKCE (S256) is required.
  `mcp.trusted_proxies` lets the login throttle see clients behind a proxy. See
  [MCP](docs/MCP.md#hosted-clients-oauth).
- A memory that MCP agents share: with `mcp.memory_database` set, the tools `remember`,
  `recall` and `forget` keep entities, facts about them and relations in that database,
  with who wrote each. Everyone who may read it may add to it, also with a read-only
  token. `recall` searches by meaning and by words (by words only without embedding
  models). See [MCP](docs/MCP.md#shared-memory).
- Two more embedding model architectures: `xlm-roberta` (multilingual-e5-base and
  -large, bge-m3) and `nomic-bert` (nomic-embed-text v1 and v1.5), set with
  `architecture` in `model.toml`. `fetch-model.py` reads the architecture from the
  model's `config.json` and knows `multilingual-e5-small`, `multilingual-e5-base` and
  `nomic-embed-text`. A model whose `config.json` names another `model_type` than its
  `architecture` is skipped, with the right architecture in the warning. See
  [Models](docs/Embeddings.md#models).

### Fixed

- A search of an HNSW vector index stops at the query's timeout or when it is cancelled;
  before, only once it was done.

## 0.5.0 — 2026-11-23

### Added

- Text embeddings with local models: `vector.embed(text, model?, options?)`,
  `CALL vector.embedBatch(texts, …)` and `CALL vector.models()`. They use BERT-family
  sentence encoders (MiniLM, BGE, E5, GTE) from Hugging Face files, run on the CPU or the
  GPU. Models go in the model folder (`[embedding]` in `graphbuff.toml`; `config/model/`
  in the release archives, `/models` in the Docker image), and `fetch-model.py` downloads
  a known one. Server feature `embed`, on in the release builds; `graphbuff_embed_*`
  metrics. See [Text embeddings](docs/Embeddings.md).
- Text indexes: `CREATE TEXT INDEX … FOR (n:L) ON (n.p)` (also on relationships). `MATCH`
  uses them to seek `n.p STARTS WITH $s`, `ENDS WITH $s` and `CONTAINS $s`.
- Lookup indexes: `CREATE LOOKUP INDEX … FOR (n) ON EACH labels(n)` and
  `FOR ()-[r]-() ON EACH type(r)`, listed by `SHOW INDEXES` (label and type scans need
  none).
- Write procedures: `db.create.setNodeVectorProperty` and
  `db.create.setRelationshipVectorProperty`, and procedures registered with
  `ProcedureSignature::writes()`, which set properties and labels through their context.
  `SHOW PROCEDURES` and `dbms.procedures()` gain a `mode` column. A query may end with a
  call to a `VOID` procedure.
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
- The documentation removed from the README is back as `docs/` pages; vector search is
  documented.

### Breaking

- Storage formats: persistent 12, hybrid snapshot and log 9, memory snapshot 9 (point,
  text and lookup index definitions). Stores written in older formats, including those of
  0.4, are no longer read: opening one fails with an error saying its format is no longer
  supported.
- `ProcedureSignature` has a new public field, `mode`: build signatures with
  `ProcedureSignature::new` (and `.writes()`) rather than a struct literal.

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
