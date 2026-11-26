# MCP (AI agents)

`graphbuff-server` serves the [Model Context Protocol](https://modelcontextprotocol.io) on a
port of its own, so that AI agents (Claude Code, Claude, Cursor, your own agents) can use
your graphs. It is off until you give it an address.

## Turning it on

In `graphbuff.toml` ([Configuration](Configuration.md)):

```toml
[mcp]
bind = "127.0.0.1:7476"
```

Restart the server; it says where the endpoint is:

```text
graphbuff-server 0.5.0 listening on http://127.0.0.1:7475, MCP on http://127.0.0.1:7476/mcp (…)
```

Agents log in with an [API token](Users.md), never a password. Create one for the user the
agent should act as. It reads only unless made with `ACCESS WRITE`:

```cypher
CREATE TOKEN claude ACCESS READ ON DATABASE movies
```

The token is shown once. `DROP TOKEN claude` takes the agent's access away at once.

## Connecting an agent

Claude Code:

```sh
claude mcp add --transport http graphbuff http://127.0.0.1:7476/mcp \
  --header "Authorization: Bearer gbt_…"
```

Clients configured with JSON (Cursor, and others that take a URL and headers):

```json
{
  "mcpServers": {
    "graphbuff": {
      "url": "http://127.0.0.1:7476/mcp",
      "headers": { "Authorization": "Bearer gbt_…" }
    }
  }
}
```

The server token (`security.token`) works too, but gives the agent everything an editor may
do on every database; prefer an API token.

## Tools

| Tool | What |
|------|------|
| `list_databases` | The databases the token may use: name, whether it is the default one, the access (`read` or `write`), and the numbers of nodes and relationships |
| `get_schema` | The labels and relationship types with their counts and properties (with types), the patterns `(:A)-[:TYPE]->(:B)` that occur, and the indexes and constraints; `labels` limits it to some labels |
| `read_cypher` | Runs a Cypher query that only reads: `query`, and optionally `params`, `database`, `max_rows`. `EXPLAIN` and `PROFILE` work |
| `write_cypher` | Runs a Cypher query that may write data, indexes or constraints, with the same arguments and `dry_run`. Returns the update counts |

`get_schema` learns properties and patterns from up to 1000 nodes per label and
relationships per type (counts are exact), and keeps the answer for a minute, or until the
numbers of nodes or relationships change. A property type ending in `?` is one that not
every node or relationship has. A schema too big for `max_result_kb` is shown without
properties; `labels` then shows those of some labels.

The tools a client sees depend on its token: `write_cypher` is listed only for tokens that
may write somewhere. A query that writes is refused by `read_cypher`, so an agent that
means to read cannot change data by mistake.

With `dry_run: true`, `write_cypher` runs the query in a transaction, reports its rows and
update counts, and rolls it back. It holds the database's write lock meanwhile, so it may
take at most 10 seconds. `CALL { … } IN TRANSACTIONS` cannot be a dry run.

Results are compact JSON text:

```json
{"database":"movies","columns":["title","released"],"rows":[["The Matrix",1999]],
 "truncated":true,"shown":100,"hint":"only the first 100 rows fit …","stats":{"nodesCreated":1}}
```

`truncated` and `hint` appear only when rows were left out, and `stats` only for queries
that changed something; a cut write also gives `rowCount`. Lists of more than 16 numbers
(embeddings) are shown as `"<vector 384>"`, and strings longer than 4 KiB are cut. A read
stops when its result is full; a write runs to its end. Errors (a syntax error, a refused
statement, a timeout) are tool results with `isError: true` and a message the agent can act
on.

When the client disconnects, its running query is cancelled and rolled back. Queries of
agents are listed by `SHOW TRANSACTIONS` with protocol `mcp`, and can be stopped with
`TERMINATE TRANSACTIONS`.

## What an agent may do

An agent may do what the token's user may, up to the token's own cap (`ACCESS READ` or
`WRITE`, and its databases), and never more than an editor. Over MCP:

| Statement | |
|-----------|---|
| Queries that read, `SHOW INDEXES`, `SHOW CONSTRAINTS`, read procedures | `read_cypher` (or `write_cypher`) |
| Queries that write, index and constraint commands, write procedures | `write_cypher` |
| `USE db` before a query | as the token may on `db` |
| `LOAD CSV` | refused: an instruction planted in the data could make an agent send data out in a URL |
| `SHOW DATABASES` | refused: `list_databases` gives the databases the token may use |
| User, role, token, database and alias commands, `SHOW` / `TERMINATE TRANSACTIONS`, `SHOW SETTINGS`, `USE` on its own | refused, whatever the user's role |

A dropped token, or a role taken from its user, applies to the next request.

## Limits

| Setting in `[mcp]` | Default | What |
|--------------------|---------|------|
| `max_concurrent_per_user` | 8 | Tool calls one user may have running at once, over all their tokens |
| `calls_per_minute` | 120 | Tool calls per minute and token (or server token); bursts up to this many are allowed. 0: no limit |
| `max_rows` | 100 | Rows `read_cypher` and `write_cypher` return, unless the call asks with `max_rows` |
| `max_rows_limit` | 1000 | The most rows a call may ask for |
| `max_result_kb` | 64 | The most KiB of rows a call returns |
| `query_timeout_ms` | 30000 | The longest an agent's query may run; never longer than `queries.query_timeout_ms` |

A call over a limit is answered as a tool error that says when to try again, so that the
agent can wait. Failed logins count against `security.max_failed_logins` as on the other
ports. Calls are counted in `/metrics` (`graphbuff_mcp_tool_calls_total`,
`graphbuff_mcp_tool_call_duration_seconds`; see [Features](Features.md)).

## Browsers and other machines

- Requests that carry an `Origin` header (from web pages) are refused with `403` unless the
  origin is listed in `allowed_origins`, a comma-separated list such as
  `"https://app.example.com"`, or `"*"`. Agents send no `Origin`.
- On a loopback address (the default), the `Host` header must name this machine
  (`localhost`, `127.0.0.1` or `[::1]`), so that a web page cannot reach the endpoint
  through DNS rebinding.
- To let agents on other machines in, bind another address and set up TLS
  (`security.tls_cert`, `tls_key`): the endpoint then speaks HTTPS with the same
  certificate. Without TLS, tokens cross the network in clear text, and the server warns.

## Protocol

- Streamable HTTP: one JSON-RPC message per `POST /mcp`, answered with one JSON object
  (never an event stream). `GET` and `DELETE` answer `405`; notifications `202`.
- Versions: 2026-07-28, where every request names its version in `_meta` and in the
  `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` headers (a mismatch is `400` with
  error `-32020`), and the `initialize`-based 2025-11-25 and 2025-06-18. `server/discover`
  lists them; an unknown version is `400` with error `-32022` and the supported list.
- No sessions: no `Mcp-Session-Id` is issued, so any server behind a load balancer can
  answer any request.
- Bad tool arguments are tool results with `isError: true`, so that the model can correct
  itself; unknown tools and malformed requests are JSON-RPC errors.
- Not supported: batches, server-sent events and subscriptions, sampling, elicitation,
  roots, logging, and the older HTTP+SSE transport.
