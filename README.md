![GraphBuff logo](graphbuff_github_social.png "GraphBuff")

[![Release](https://img.shields.io/github/v/release/JanVencl/graphbuff?color=blue)](https://github.com/JanVencl/graphbuff/releases)
[![Build](https://github.com/JanVencl/graphbuff/actions/workflows/release.yml/badge.svg)](https://github.com/JanVencl/graphbuff/actions/workflows/release.yml)
[![License: AGPL-3.0](https://img.shields.io/github/license/JanVencl/graphbuff?color=blue)](LICENSE)
[![Rust 1.100+](https://img.shields.io/badge/rust-1.100%2B-blue?logo=rust)](https://www.rust-lang.org)
[![Docker](https://img.shields.io/badge/docker-ghcr.io%2Fjanvencl%2Fgraphbuff-blue?logo=docker)](https://github.com/JanVencl/graphbuff/pkgs/container/graphbuff)
[![Website](https://img.shields.io/badge/web-graphbuff.com-informational)](https://graphbuff.com)
# GraphBuff

## Graph database engine written in Rust and queried with **Cypher**.

* High-performance, Low-memory footprint database.
* Query and store graphs with millions of nodes and billions of relationships

## Supported features
* **Cypher** queries ([supported Cypher](./docs/Cypher.md))
* **Bolt** ([Neo4j drivers](./docs/Bolt.md))
* **HTTP/JSON** ([REST API](./docs/HTTP.md))
* **user accounts** and **TLS** ([details](./docs/Users.md))
* **GPU** (wgpu; [kernels and algorithms](./docs/Kernels.md))
* **persistent storage** ([storage modes](./docs/Storage.md))
* **durable transactions**
* **bulk CSV import** ([details](./docs/Import.md))
* **vector search** ([details](./docs/Vectors.md)) and **text embeddings** with local models ([details](./docs/Embeddings.md))
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
| [Configuration](./docs/Configuration.md) | `graphbuff.toml` |
| [CLI](./docs/CLI.md) | `graphbuff-cli` |
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
| [Text embeddings](./docs/Embeddings.md) | `vector.embed` with local models, the model folder |
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
