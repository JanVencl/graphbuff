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

The tools a client sees depend on its token: tools that write are listed only for tokens
that may write somewhere.

## What an agent may do

An agent may do what the token's user may, up to the token's own cap (`ACCESS READ` or
`WRITE`, and its databases), and never more than an editor: administrative statements
(users, roles, databases, settings, `TERMINATE TRANSACTIONS`) cannot be run over MCP,
whatever the user's role. A dropped token, or a role taken from its user, applies to the
next request.

## Limits

| Setting in `[mcp]` | Default | What |
|--------------------|---------|------|
| `max_concurrent_per_user` | 8 | Tool calls one user may have running at once, over all their tokens |
| `calls_per_minute` | 120 | Tool calls per minute and token (or server token); bursts up to this many are allowed. 0: no limit |

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
