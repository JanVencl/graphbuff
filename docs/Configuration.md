# Configuration

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
what it does ([packaging/config/graphbuff.toml](../packaging/config/graphbuff.toml)). Edit a
line, or remove the `#` in front of one, and restart the server. Relative paths are
relative to the file's folder, so the shipped `data = "../data"` puts the data folder next
to the program. A misspelt section or setting, or a value of the wrong kind, stops the
server with the file and line. The only settings outside the file are secrets for
containers: env `GRAPHBUFF_TOKEN` overrides `security.token`, and the first
administrator's `GRAPHBUFF_ADMIN_*` (see [Users and TLS](Users.md)).

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

[mcp]
bind = "127.0.0.1:7476"               # MCP endpoint for AI agents, at /mcp (off when not set)
# allowed_origins = "https://app.example.com"  # browser apps on these origins may call it
max_concurrent_per_user = 8           # tool calls one user may have running at once
calls_per_minute = 120                # tool calls per minute and API token (0: no limit)
max_rows = 100                        # rows a query tool returns (an agent may ask for up to max_rows_limit)
max_rows_limit = 1000
max_result_kb = 64                    # and at most this much of them
query_timeout_ms = 30000              # stop an agent's query that runs longer
# public_url = "https://mcp.example.com/mcp"   # OAuth for hosted agents (claude.ai, ChatGPT)
# trusted_proxies = "127.0.0.1"       # proxies whose X-Forwarded-For names the client
# oauth_clients = "my-agent https://agent.example.com/callback"  # clients registered here
# oauth_client_hosts = "claude.ai, chatgpt.com"  # fetch client metadata documents only from these
# memory_database = "memory"          # a memory agents share: remember, recall, forget
```

`[mcp]` is described in [MCP](MCP.md).
