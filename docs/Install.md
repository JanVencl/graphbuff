# Install without Rust

## Docker

```bash
docker run --rm -p 127.0.0.1:7475:7475 -p 127.0.0.1:7687:7687 \
       -e GRAPHBUFF_ADMIN_PASSWORD=change-me ghcr.io/janvencl/graphbuff
```

```bash
curl -s -u admin:change-me http://127.0.0.1:7475/query -H 'Content-Type: application/json' \
     -d '{"query": "CREATE (:Person {name: \"Ann\"}) RETURN 1"}'
```

The image `ghcr.io/janvencl/graphbuff` (tags: `latest` and each version, such as `0.1.0`;
linux/amd64 and linux/arm64) runs `graphbuff-server` as an unprivileged user with the
config file `/etc/graphbuff/graphbuff.toml`
([packaging/docker/graphbuff.toml](../packaging/docker/graphbuff.toml)): HTTP/JSON on
`0.0.0.0:7475`, Bolt on `0.0.0.0:7687`, its databases in the volume `/data` and its log on
standard error (`docker logs`). To change a setting, mount your own copy of that file
over it (keep `data = "/data"`):

```bash
docker run -d --name graphbuff -p 127.0.0.1:7475:7475 -p 127.0.0.1:7687:7687 \
       -v graphbuff-data:/data -v ./graphbuff.toml:/etc/graphbuff/graphbuff.toml:ro \
       ghcr.io/janvencl/graphbuff
```

With `demo = "movies"` in `[storage]`, the graph of [examples/movies.cypher](../examples/movies.cypher),
which is compiled into the binary, is loaded into the default database (unless it already
holds data).

The image also has the [command-line client](CLI.md); run it in the server's container:

```bash
docker exec -it graphbuff graphbuff-cli -u admin
```

User accounts are on, kept in the system database in the `/data` volume, so clients log
in. On the first start, while there is no administrator, the server creates the administrator
`GRAPHBUFF_ADMIN_USER` (default `admin`) with the password in `GRAPHBUFF_ADMIN_PASSWORD`;
later starts ignore both, and the password is changed with Cypher (`ALTER CURRENT USER SET
PASSWORD FROM … TO …`). To keep the password out of the environment, use a secret file
instead:

```bash
docker run -d --name graphbuff -p 127.0.0.1:7687:7687 -v graphbuff-data:/data \
       -v ./admin-password.txt:/run/secrets/admin:ro \
       -e GRAPHBUFF_ADMIN_PASSWORD_FILE=/run/secrets/admin ghcr.io/janvencl/graphbuff
```

or, in Compose:

```yaml
services:
  graphbuff:
    image: ghcr.io/janvencl/graphbuff
    environment:
      GRAPHBUFF_ADMIN_PASSWORD_FILE: /run/secrets/graphbuff_admin
    secrets: [graphbuff_admin]
    volumes: [graphbuff-data:/data]
secrets:
  graphbuff_admin:
    file: ./admin-password.txt
volumes:
  graphbuff-data:
```

Without either, the server asks for the administrator when it runs in a terminal
(`docker run -it`), and otherwise stops with a message. The administrator can also be
created offline, before the first start (the image has no shell, so name the binary; it
asks for the name and password, or takes them from `GRAPHBUFF_ADMIN_USER` and
`GRAPHBUFF_ADMIN_PASSWORD` / `_FILE`):

```bash
docker run --rm -it -v graphbuff-data:/data --entrypoint graphbuff-server \
       ghcr.io/janvencl/graphbuff admin init
```

Lost the administrator's password? Stop the container, then set a new one the same way:

```bash
docker stop graphbuff
docker run --rm -it -v graphbuff-data:/data --entrypoint graphbuff-server \
       ghcr.io/janvencl/graphbuff admin reset-password admin
docker start graphbuff
```

`-e GRAPHBUFF_TOKEN=s3cret` adds a token that logs in as an administrator, besides the
accounts. Who can reach the ports is decided by `-p`: `-p 127.0.0.1:7475:7475` publishes a port to this machine
only. A bind mount for `/data` (`-v ./data:/data`) must be writable by the container's user,
uid 65532 (`sudo chown 65532 ./data` on Linux).

Name the volume to keep the databases when the container is removed (`--rm` above drops
them with the container):

```bash
docker run -d --name graphbuff -p 127.0.0.1:7475:7475 -p 127.0.0.1:7687:7687 \
       -v graphbuff-data:/data ghcr.io/janvencl/graphbuff
docker logs -f graphbuff                        # start-up, failed logins, errors
```

In your own config file, `slow_query_ms = 500` in `[logs]` logs queries that take 500 ms
or more, `mode = "memory"` in `[storage]` (without `data`) makes a throwaway server, and
`dir = "/data/logs"` in `[logs]` writes log files into the volume.

Build the image yourself with `docker build -t graphbuff .`.

## Prebuilt binaries

Each [release](https://github.com/JanVencl/graphbuff/releases) carries one archive per
platform, with a folder `graphbuff-VERSION-PLATFORM/` inside:

```text
graphbuff-VERSION-PLATFORM/
  graphbuff-server            the server (graphbuff-server.exe on Windows)
  graphbuff-cli               the command-line client (graphbuff-cli.exe on Windows)
  config/graphbuff.toml       its settings: edit, then restart the server
  README.md
  data/                       created on the first start: databases, accounts, logs
```

| Platform | Archive |
|----------|---------|
| Linux x86-64 (static, any distribution) | `graphbuff-linux-x86_64.tar.gz` |
| Linux ARM64 (static, any distribution) | `graphbuff-linux-aarch64.tar.gz` |
| macOS Apple silicon | `graphbuff-macos-arm64.tar.gz` |
| macOS Intel | `graphbuff-macos-x86_64.tar.gz` |
| Windows x86-64 | `graphbuff-windows-x86_64.zip` |

and `SHA256SUMS` with the checksums of all of them. The file names carry no version, so
`https://github.com/JanVencl/graphbuff/releases/latest/download/graphbuff-linux-x86_64.tar.gz`
always points at the newest release (a specific one: `.../releases/download/v0.1.0/...`).

```bash
curl -LO https://github.com/JanVencl/graphbuff/releases/latest/download/graphbuff-linux-x86_64.tar.gz
tar -xzf graphbuff-linux-x86_64.tar.gz
./graphbuff-*/graphbuff-server    # http://127.0.0.1:7475, data in graphbuff-*/data
```

To load the movies demo graph, set `demo = "movies"` in `[storage]` of
`config/graphbuff.toml` first.

The binaries are built with the `tls` feature (HTTPS and Bolt over TLS; see
[Users and TLS](Users.md)), not with `gpu`. Releases are cut by pushing a
tag `vX.Y.Z` that matches the workspace version ([release.yml](../.github/workflows/release.yml)).
