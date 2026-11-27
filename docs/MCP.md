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
graphbuff-server 0.6.0 listening on http://127.0.0.1:7475, MCP on http://127.0.0.1:7476/mcp (…)
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

Clients that start their MCP servers as programs and talk over standard input and output
(Claude Desktop, for one) use `graphbuff-server mcp`, a bridge that passes each message to
the endpoint of a server that is already running. It never opens a data folder itself.

```json
{
  "mcpServers": {
    "graphbuff": {
      "command": "/path/to/graphbuff-server",
      "args": ["mcp", "--url", "http://127.0.0.1:7476/mcp"],
      "env": { "GRAPHBUFF_MCP_TOKEN": "gbt_…" }
    }
  }
}
```

`--url` defaults to `http://127.0.0.1:7476/mcp` (also env `GRAPHBUFF_MCP_URL`). For an
`https://` URL the bridge checks the server's certificate against `--cacert FILE` (PEM),
else env `SSL_CERT_FILE`, else the system's CA bundle on Linux and macOS; on Windows, pass
one of the first two. When the server cannot be reached, each request gets an error that
says so, and the bridge keeps running. A request the client cancels is stopped on the
server. The bridge ends when its input does.

The server token (`security.token`) works too, but gives the agent everything an editor may
do on every database; prefer an API token.

## Tools

| Tool | What |
|------|------|
| `list_databases` | The databases the token may use: name, whether it is the default one, the access (`read` or `write`), and the numbers of nodes and relationships |
| `get_schema` | The labels and relationship types with their counts and properties (with types), the patterns `(:A)-[:TYPE]->(:B)` that occur, and the indexes and constraints; `labels` limits it to some labels |
| `search` | The nodes that best match a `text`: by meaning (a vector index; the text is embedded with the index's `vector.embedding_model`, or `model`, or the default model), by words (a full-text index), or both (`hybrid`). Optional: `index`, `mode` (`auto`, `vector`, `fulltext`, `hybrid`), `k` (10), `filters`, `model` |
| `get_node` | A node by `id`, with its numbers of relationships per type and direction; or the nodes with a `label` whose `property` has a `value` (up to 10) |
| `neighbors` | A node's relationships per type and direction, then up to `limit` (25) neighbours spread over the types; with `depth` 2 or 3, the nodes that many hops away and their distance. `types` and `direction` (`out`, `in`, `both`) filter |
| `find_path` | A shortest path between two nodes (`from`, `to`) of at most `max_hops` (5, at most 10) relationships, in either direction, optionally over some `types`; with `all: true`, every shortest path |
| `read_cypher` | Runs a Cypher query that only reads: `query`, and optionally `params`, `database`, `max_rows`. `EXPLAIN` and `PROFILE` work |
| `write_cypher` | Runs a Cypher query that may write data, indexes or constraints, with the same arguments and `dry_run`. Returns the update counts |

`get_schema` learns properties and patterns from up to 1000 nodes per label and
relationships per type (counts are exact), and keeps the answer for a minute, or until the
numbers of nodes or relationships change. A property type ending in `?` is one that not
every node or relationship has. A schema too big for `max_result_kb` is shown without
properties; `labels` then shows those of some labels.

`search` without `index` uses the only node index that fits; with `mode: auto` (the
default), a label with both a vector and a full-text index is searched both ways, and the two
rankings are fused (reciprocal rank fusion, k = 60), one entry per node. Several fitting
indexes are an error that names them. The text of a full-text search is looked for as words
(Lucene syntax in it is ignored). `filters` (`{"year": {"gte": 2020}, "tenant": 5}`, also
`lte`, `gt`, `lt`, `in`) narrow a vector search on the index's filter properties (`WITH
[d.year, …]`), and filter full-text results. A model whose dimensions do not fit the index
is an error that lists the models that do. Without embedding models, only full-text search
works.

`get_node`, `neighbors` and `find_path` build Cypher with parameters and run it as
`read_cypher` does, so the token's access, the time limit and the row limits apply. Spreading
`neighbors` over the types keeps a node with 100,000 relationships of one type from hiding
the others. Node ids may be used again after a node is deleted.

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

## Shared memory

With `memory_database` set in `[mcp]`, agents get a memory they share with each other and
with people: three tools keep entities, facts about them and relations between them in that
database, which is created on first use.

```toml
[mcp]
memory_database = "memory"
```

| Tool | What |
|------|------|
| `remember` | Adds `entities` (`name`, `type`, `observations`: short facts) and `relations` (`{from, type, to}`). An entity that exists is added to; a fact it has already is not repeated |
| `recall` | Entities with their observations and relations: by `query` (meaning and words), by `names`, or the latest ones. Each item says who wrote it (`by`) and when |
| `forget` | Deletes `entities` (with their observations and relations), `observations` (by the id recall gives, or `{entity, text}`) and `relations` |

- **Who:** everyone who may read the memory database sees every memory and may add to it
  and forget, also with a read-only token. These tools are the one way such a token
  writes, and only there: `write_cypher` still refuses it, and the graph stays read-only.
  A token limited to other databases (`ON DATABASE`) has no memory tools.
- **Authors:** every entity, observation and relation records its writer: the user, and
  the token or OAuth client (`ann (via https://claude.ai/…)`, `ann (token 3f2a…)`).
- **Search:** with an embedding model as `embedding.default_model` (or a single installed
  one), observations are embedded with it, and `recall` searches by meaning and by words
  (a vector index that records the model, and a full-text index); without models, by
  words only.
- **Limits:** names up to 200 bytes, observations up to 2 KiB, 100 items per call.
- **Storage:** `(:Memory:Entity {name, type, by, at})`, `(:Memory:Observation {text, by, at,
  embedding})-[:ABOUT]->(entity)` and `(a)-[:RELATES {type, by, at}]->(b)`, with a
  uniqueness constraint on entity names. People can read and fix it with Cypher, as any
  database, with the roles they have on it.
- **A risk to know:** an agent misled by text in the data it read can store a false
  "fact" that other agents then recall. Recall says that memories are notes, not
  instructions, and shows who wrote each; drop the memory database's roles from users
  whose agents read untrusted data, or keep it off.

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

## Hosted clients (OAuth)

Hosted agents such as claude.ai and ChatGPT cannot be given a fixed token everywhere.
For them, `graphbuff-server` is its own OAuth 2.1 authorization server, on the MCP port:
the user adds the server's URL in the agent, logs in on a page of this server with their
GraphBuff user name and password, and chooses what the agent may do.

It needs the endpoint to be reachable over HTTPS, usually through a proxy that terminates
TLS and forwards to `bind`:

```toml
[mcp]
bind = "127.0.0.1:7476"
public_url = "https://mcp.example.com/mcp"
trusted_proxies = "127.0.0.1"
```

| Setting in `[mcp]` | What |
|--------------------|------|
| `public_url` | The endpoint's URL as clients reach it: `https://HOST[:PORT]/mcp`. Turns OAuth on |
| `trusted_proxies` | Proxies (comma-separated addresses) whose `X-Forwarded-For` names the client: the login throttle and the logs then see the client, not the proxy |
| `oauth_clients` | Clients registered here: `"client_id redirect_uri [redirect_uri…]"`, `;` between clients. Clients with a client metadata document need no entry |
| `oauth_client_hosts` | Fetch client metadata documents only from these hosts (comma-separated). Default `claude.ai, chatgpt.com`: claude.ai, Claude Desktop, Claude Code, ChatGPT and Codex. `"*"`: any public host; `""`: none, only `oauth_clients` (the server then does not offer client metadata documents) |

Then, in claude.ai (Settings, Connectors, add a custom connector) or ChatGPT, give
`https://mcp.example.com/mcp`; the agent opens the login page.

- **The page** shows which client asks (its name and `client_id`) and where it will be sent
  back; for a client on this computer (a `localhost` redirect) it says so. The user picks
  read only, or read and write, and may limit the databases. Write needs a role that
  writes; nobody gets administration. Deny sends the client away with `access_denied`.
- **Tokens:** the client gets an access token that works for an hour, only at this
  endpoint (not on the HTTP or Bolt ports), and a refresh token that gets it new ones. Each
  refresh replaces both; a refresh token used a second time (it may have been stolen) ends
  the grant. A grant lasts at most 90 days, and 30 days without use. `SHOW TOKENS` lists
  the access token as `oauth-…`; `DROP TOKEN` ends the grant at once, as do dropping the
  user and `POST /oauth/revoke`.
- **Clients** are public (no secret; PKCE with S256 stands in for one). A client's
  `client_id` is either listed in `oauth_clients`, or an `https://` URL of its
  [client metadata document](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/client-registration),
  which this server fetches (public addresses only, no redirects, at most 64 KiB in 5
  seconds, four at a time) and keeps for 10 minutes; one that cannot be fetched or read is
  not tried again for a minute. claude.ai and ChatGPT use such documents. Such a document
  names its app as it likes, so the consent page shows the host the document is on, and
  warns when the access goes to another host. Dynamic
  client registration is not offered. Redirect URIs must match exactly, except that a
  `localhost` one may use any port.
- **Discovery:** `/.well-known/oauth-protected-resource/mcp` (RFC 9728) names the
  authorization server; `/.well-known/oauth-authorization-server` (RFC 8414) and
  `/.well-known/openid-configuration` describe it. A request to `/mcp` without a token
  answers `401` with `WWW-Authenticate: Bearer resource_metadata="…"`. The answer to a
  login carries `iss` (RFC 9207); `resource` (RFC 8707), when sent, must be `public_url`.
- **Security:** the server keeps no state for an open login page: its form carries the
  request, signed with a key made at each start, and a token that must match a cookie of
  the page, against forged requests. A page works for 10 minutes (and not after a
  restart), gives one code, and may not be framed. Failed logins count against
  `security.max_failed_logins` as elsewhere. `security.log` records each consent and
  denial, a code or refresh token used twice (the grant ends), and each revocation.
  API tokens keep working as before, also with OAuth on.

## Browsers and other machines

- Requests that carry an `Origin` header (from web pages) are refused with `403` unless the
  origin is listed in `allowed_origins`, a comma-separated list such as
  `"https://app.example.com"`, or `"*"`. Agents send no `Origin`.
- On a loopback address (the default), the `Host` header must name this machine
  (`localhost`, `127.0.0.1` or `[::1]`) or the host of `public_url`, so that a web page
  cannot reach the endpoint through DNS rebinding.
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
- The bridge speaks MCP over stdio: one JSON-RPC message per line. It adds the
  2026-07-28 headers from each body, and sends the version `initialize` agreed on with the
  requests of older clients.
- Not supported: batches, server-sent events and subscriptions, sampling, elicitation,
  roots, logging, and the older HTTP+SSE transport.
