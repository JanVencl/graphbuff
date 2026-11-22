![GraphBuff logo](graphbuff_github_social.png "GraphBuff")

[![Release](https://img.shields.io/github/v/release/JanVencl/graphbuff?color=blue)](https://github.com/JanVencl/graphbuff/releases)
[![Build](https://github.com/JanVencl/graphbuff/actions/workflows/release.yml/badge.svg)](https://github.com/JanVencl/graphbuff/actions/workflows/release.yml)
[![License: AGPL-3.0](https://img.shields.io/github/license/JanVencl/graphbuff?color=blue)](LICENSE)
[![Rust 1.100+](https://img.shields.io/badge/rust-1.100%2B-blue?logo=rust)](https://www.rust-lang.org)
[![Docker](https://img.shields.io/badge/docker-ghcr.io%2Fjanvencl%2Fgraphbuff-blue?logo=docker)](https://github.com/JanVencl/graphbuff/pkgs/container/graphbuff)
[![Website](https://img.shields.io/badge/web-graphbuff.com-informational)](https://graphbuff.com)
# GraphBuff

## Graph database engine written in Rust and queried with **Cypher**.

* high performance, low memory footprint database
* you can query and store graphs with millions of nodes and billions of relationships

## Supported features
* **Cypher** queries ([supported Cypher](./docs/Cypher.md))
* **Bolt** ([Neo4j drivers](./docs/Bolt.md))
* **HTTP/JSON** ([REST API](./docs/HTTP.md))
* **user accounts** and **TLS** ([details](./docs/Users.md))
* **GPU** (wgpu; [kernels and algorithms](./docs/Kernels.md))
* **persistent storage** ([storage modes](./docs/Storage.md))
* **durable transactions**
* **bulk CSV import** ([details](./docs/Import.md))
* **vector search** ([details](./docs/Vectors.md))
* **full-text search** ([details](./docs/FullText.md))
* [More features](./docs/Features.md)

## Quick start
```bash
docker run --rm -p 127.0.0.1:7475:7475 -p 127.0.0.1:7687:7687 \
       -e GRAPHBUFF_ADMIN_PASSWORD=change-me ghcr.io/janvencl/graphbuff
```
Starts HTTP/JSON server on `http://127.0.0.1:7475` and Bolt server on `bolt://127.0.0.1:7687`.
Login with `admin` and the password `change-me`;

```bash
curl -s -u admin:change-me http://127.0.0.1:7475/query -H 'Content-Type: application/json' \
     -d '{"query": "CREATE (:Person {name: \"Ann\"}) RETURN 1"}'
```


| Request | Answer |
|---------|--------|
| `GET /health` | `{"status":"ok"}` (never needs the token) |
| `GET /metrics` | server and database metrics in the Prometheus text format; admins and the token only ([details](./docs/Features.md#metrics)) |
| `GET /db` | `{"databases":[{"name","storage","default","nodes","relationships"}]}` |
| `POST /db/{name}/query` | runs `{"query": "...", "params": {...}, "timeoutMs": 5000}` against database `name` (`params` and `timeoutMs` optional) |
| `POST /query` | the same, against the default database |
| `POST /db/{name}/tx` | begins a transaction: `201 {"transaction":{"id","database","path","timeoutMs"}}` |
| `POST /db/{name}/tx/{id}` | runs `{"query", "params", "timeoutMs"}` in it; answers like a query |
| `POST /db/{name}/tx/{id}/commit` | commits: `{"committed":true}` |
| `DELETE /db/{name}/tx/{id}` | rolls back (cancelling a running statement): `{"rolledBack":true}` |
| `/tx`, `/tx/{id}`, `/tx/{id}/commit` | the same, against the default database |
| `POST /db/{name}/backup` | online backup into `storage.backup_dir`: `{"backup": "name"}`; admins only ([details](./docs/Backup.md)) |
| `POST /db/{name}/restore` | creates the database from a backup: `{"backup": "name", "overwrite": false}`; admins only |
| `POST /db/{name}/check` | consistency check of a persistent database: `{"check":{"consistent","errors",...}}`; admins only |

A query returns `{"columns":[...],"rows":[[...]],"stats":{"nodesCreated":1,...,"containsUpdates":true}}`.

### Configuration

`graphbuff-server` takes its settings from one file, `graphbuff.toml` (TOML), not from
command-line options. It reads the first of:

1. the file named by `--config FILE` (or env `GRAPHBUFF_CONFIG`; `-` for the built-in
   defaults);
2. `config/graphbuff.toml` in the program's folder: the release archives ship it there;
3. the per-user (system) file:

| Platform | Config file |
|----------|-------------|
| Linux | `$XDG_CONFIG_HOME/graphbuff/graphbuff.toml`, usually `~/.config/graphbuff/graphbuff.toml`; `/etc/graphbuff/graphbuff.toml` when run as root |
| macOS | `~/Library/Application Support/graphbuff/graphbuff.toml` |
| Windows | `%LOCALAPPDATA%\graphbuff\graphbuff.toml` |
| Docker image | `/etc/graphbuff/graphbuff.toml` |

Without any of them, the server creates `config/graphbuff.toml` next to the program (or,
if that folder cannot be written, the per-user file) with every setting, its default and
what it does ([packaging/config/graphbuff.toml](packaging/config/graphbuff.toml)). Edit a
line, or remove the `#` in front of one, and restart the server. Relative paths are
relative to the file's folder, so the shipped `data = "../data"` puts the data folder next
to the program. A misspelt section or setting, or a value of the wrong kind, stops the
server with the file and line. The only settings outside the file are secrets for
containers: env `GRAPHBUFF_TOKEN` overrides `security.token`, and the first
administrator's `GRAPHBUFF_ADMIN_*` (see [Users and TLS](./docs/Users.md)).

```toml
[server]
bind = "0.0.0.0:7475"                 # HTTP/JSON; 127.0.0.1 (default) for this machine only
bolt = "0.0.0.0:7687"                 # also Bolt, for Neo4j drivers (off when not set)
max_connections = 256
keep_alive_seconds = 30
shutdown_timeout_seconds = 5
# cors_origins = "https://app.example.com"  # browser apps on these origins may call the HTTP API

[storage]
mode = "hybrid"                       # hybrid (default), persistent or memory
data = "/srv/graphbuff"               # databases, user accounts and (by default) logs
fsync = true                          # false: faster commits, the last ones may be lost in a crash
import_dir = "/srv/import"            # LOAD CSV reads files here (off when not set)
# load_csv_urls = "https://data.example.com/exports/"  # and web URLs starting with these
backup_dir = "/srv/backups"           # online backups and restores (off when not set)
# demo = "movies"                     # load the movies demo graph into an empty default database

[security]
# token = "change-me"                 # also log in with this token, as an administrator
tls_cert = "/etc/graphbuff/cert.pem"  # HTTPS and Bolt over TLS
tls_key = "/etc/graphbuff/key.pem"
tls_allow_plain = false
max_failed_logins = 10                # then the client is locked out (0: off)
login_lockout_seconds = 60            # doubling with each further failure, up to an hour

[queries]
transaction_timeout_seconds = 60
query_timeout_ms = 30000              # no limit when not set
max_memory_mb = 4096                  # per query; default: a quarter of the memory (0: off)
gpu = false                           # graph kernels on the GPU (`gpu` build feature)

[logs]
dir = "/var/log/graphbuff"            # "-" for standard error; default: DATA/logs
level = "info"                        # error, warn, info or debug
slow_query_ms = 500                   # log queries that take this long to query.log
```

## Command-line client

`graphbuff-cli` runs Cypher against a server over the HTTP API: statements on the command
line, scripts from files or standard input, or an interactive prompt. It ships next to
`graphbuff-server` in the release archives and the Docker image:

```bash
graphbuff-cli -u admin "MATCH (n) RETURN count(n);"
docker exec -it CONTAINER graphbuff-cli -u admin     # the server in a container
```

More in [Command-line client](./docs/CLI.md).

## Documentation

| Page | What |
|------|------|
| [Install without Rust](./docs/Install.md) | Docker image and prebuilt binaries |
| [Storage modes](./docs/Storage.md) | hybrid, persistent and in-memory storage |
| [Multiple databases](./docs/Databases.md) | `CREATE DATABASE`, `USE`, sessions |
| [Bulk import](./docs/Import.md) | `neo4j-admin import`-style CSV import |
| [Backup, restore and consistency checks](./docs/Backup.md) | online backups and checks |
| [CSR adjacency export](./docs/Kernels.md) | graph kernels, algorithms, GPU |
| [HTTP server](./docs/HTTP.md) | the REST API and a Java client |
| [Features](./docs/Features.md) | transactions, timeouts, values, errors, logs, stopping |
| [Command-line client](./docs/CLI.md) | `graphbuff-cli` |
| [Bolt](./docs/Bolt.md) | Neo4j drivers |
| [Users and TLS](./docs/Users.md) | accounts, roles, HTTPS and Bolt over TLS |
| [Vector search](./docs/Vectors.md) | similarity functions and vector indexes |
| [Full-text search](./docs/FullText.md) | full-text indexes, query syntax, analyzers |
| [Supported Cypher](./docs/Cypher.md) | clauses, functions, procedures, what is missing |
| [Layout](./docs/Architecture.md) | crates, storage internals, concurrency, execution |

## Contributing and security

Changes per release are in [CHANGELOG.md](CHANGELOG.md). To contribute, see
[CONTRIBUTING.md](CONTRIBUTING.md); to report a vulnerability, see
[SECURITY.md](SECURITY.md).

## License

graphbuff is licensed under the GNU Affero General Public License v3.0 only
(`AGPL-3.0-only`). See [LICENSE](LICENSE).
