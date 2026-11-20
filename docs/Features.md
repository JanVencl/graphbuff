# Features

How `graphbuff-server` behaves over HTTP/JSON: transactions, timeouts, values, errors,
security, where it keeps data and logs, and how it stops. Settings are named as
`section.setting` in `graphbuff.toml` (see [Configuration](../README.md#configuration)).

- [Transactions](#transactions)
- [Timeouts](#timeouts)
- [Values](#values)
- [Errors](#errors)
- [Security](#security)
- [Implementation](#implementation)
- [Data directory and logs](#data-directory-and-logs)
- [Stopping](#stopping)

## Transactions

Each query request runs as its own transaction. Database commands (`CREATE DATABASE`,
`SHOW DATABASES`, `USE x MATCH ...`) work through either query endpoint.

An explicit transaction (`POST /db/{name}/tx`) works like this:

- Its statements may come in separate requests, on any connection, and see each other's
  changes. Other clients see none of them until the commit.
- While it is open, other writers on its database wait (including `begin`); readers do not.
- A statement that fails rolls the whole transaction back; its error carries
  `"rolledBack": true`.
- A statement that does not parse, a database command, or `USE` of another database is
  rejected, and the transaction stays open.
- A transaction with no request for `queries.transaction_timeout_seconds` (default 60) is
  rolled back. Requests to a transaction that has ended answer `404 TransactionNotFound`.
- Shutting the server down rolls back open transactions.

## Timeouts

- `queries.query_timeout_ms` stops a query that runs longer (default: no limit). A request
  may ask for less with `"timeoutMs"`.
- The limit also covers waiting for another writer and waiting to begin a transaction.
- A query that runs out of time is rolled back and answers `408 Timeout`. Inside an explicit
  transaction, the whole transaction rolls back.
- Rolling back a transaction, or shutting down, cancels the statement it is running, which
  then answers `409 Cancelled`.

## Values

| Cypher | JSON |
|--------|------|
| INTEGER | integer |
| FLOAT | number with a fraction or exponent (always written with `.0` or an exponent) |
| NaN, ±Infinity | the strings `"NaN"`, `"Infinity"`, `"-Infinity"` |
| node | `{"$type":"node","id":0,"labels":["Person"],"properties":{...}}` |
| relationship | `{"$type":"relationship","id":0,"type":"KNOWS","start":0,"end":1,"properties":{...}}` |
| path | `{"$type":"path","nodes":[...],"relationships":[...]}` |

Numbers convert the same way in both directions.

## Errors

Errors have the form:

```json
{"error": {"code": "SyntaxError", "message": "...", "offset": 9}}
```

| Status | Codes |
|--------|-------|
| 400 | `SyntaxError`, `SemanticError`, `TypeError`, `RuntimeError`, `Unsupported`, `BadRequest` |
| 401 | missing or wrong credentials |
| 403 | `Forbidden`: the user's roles do not allow the statement |
| 404 | `DatabaseNotFound`, `TransactionNotFound` |
| 408 | `Timeout` |
| 409 | `ConstraintError`, `Cancelled` |
| 413 | body over 16 MiB, or `TransactionTooLarge` (transaction past its database's size cap; rolled back) |
| 429 | `TooManyRequests`: too many failed logins from this client; `Retry-After` says when to try again |
| 500 | `StorageError` |

## Security

- The server listens on localhost by default.
- Every request except `GET /health` needs a login: a user name and password, or
  `Authorization: Bearer <token>` when `security.token` (or env `GRAPHBUFF_TOKEN`) is set.
- HTTPS and Bolt over TLS are enabled with `security.tls_cert` and `security.tls_key`.
- `LOAD CSV` is off unless `storage.import_dir` is set, and then reads only files inside
  that folder.
- Failed logins lock the client out for a while: `security.max_failed_logins` (default 10)
  and `security.login_lockout_seconds` (default 60); see [Users and TLS](Users.md).
- Passwords are hashed with Argon2. Query parameters are never logged.

## Implementation

- Lives in `crates/server`: the standard library plus `argon2`, and rustls behind a TLS
  feature.
- One thread per connection (`server.max_connections`, default 256), HTTP/1.1 keep-alive,
  `Content-Length` or chunked bodies, `Expect: 100-continue`.
- `Server::start` embeds the server in a Rust program.

## Data directory and logs

Without `storage.data`, databases are kept in:

| Platform | Data directory |
|----------|----------------|
| Linux | `$XDG_DATA_HOME/graphbuff`, usually `~/.local/share/graphbuff`; `/var/lib/graphbuff` as root |
| macOS | `~/Library/Application Support/graphbuff` |
| Windows | `%LOCALAPPDATA%\graphbuff` |
| Docker image | `/data` (a volume) |

- The shipped config file sets it to the `data` folder next to the program. The start-up
  line names the directory in use.
- `DATA/system/` holds the user accounts (the system database).
- Only one process uses a data directory at a time. A second server, or an offline
  `users` / `admin` command, is refused while it is locked.
- `storage.mode`: `hybrid` (default; in-memory graph with a write-ahead log and snapshots),
  `persistent` (disk-resident, for graphs larger than RAM) or `memory` (nothing written).

Logs go to `DATA/logs` (`logs.dir`; `"-"` for standard error, the default with
`mode = "memory"` and in the Docker image):

| File | Contents |
|------|----------|
| `graphbuff.log` | start-up and settings (with the config file read), failed logins (client address and user name) and lockouts, refused connections, storage errors; every connection at `level = "debug"` |
| `query.log` | with `logs.slow_query_ms`: each query that took at least that long, with duration, protocol, database and user |

Levels: `error`, `warn`, `info` (default), `debug`. Each file rotates to `.1`, `.2`, … at
10 MiB, and five old files are kept. An embedded server gets the same log via
`graphbuff_server::Log` and `ServerConfig::log`.

```text
2026-11-15T18:00:00.123Z INFO  graphbuff-server 0.4.0 listening on http://127.0.0.1:7475 (data in /home/ann/.local/share/graphbuff)
2026-11-15T18:00:05.004Z WARN  failed login (bolt) from 10.0.0.7:51234 as `admin`
2026-11-15T18:00:09.871Z SLOW  1532 ms bolt movies alice MATCH (a)-[*..6]->(b) RETURN count(b)
```

## Stopping

SIGTERM or SIGINT (`docker stop`, `systemctl stop`, `kill`, Ctrl-C), or on Windows Ctrl-C,
Ctrl-Break or closing the console, stops the server cleanly:

1. stops accepting connections;
2. rolls back open explicit transactions;
3. gives running queries `server.shutdown_timeout_seconds` (default 5) to finish, then
   cancels and rolls back the rest;
4. checkpoints every durable database, so the next start has no log to replay;
5. exits with 0, or with 1 if a checkpoint failed (that database's log is kept and replayed
   on the next start).

A second signal ends the server at once. Committed data is safe either way, since every
commit is in the log. An embedded server gets the same steps from `Server::shutdown` (or
by dropping it) with `ServerConfig::shutdown_grace`, followed by
`DatabaseManager::checkpoint_all`.
