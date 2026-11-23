# HTTP server (Java and other languages)

`graphbuff-server` serves a `DatabaseManager` over HTTP/JSON, so any language can run
Cypher against graphbuff:

```bash
cargo run --release -p graphbuff-server     # settings from target/release/config/graphbuff.toml
```

What it serves, where it keeps its data and who may log in is set in its config file (see
[Configuration](Configuration.md)): for example `mode = "persistent"` (disk-resident) or
`mode = "memory"` (nothing written to disk) in `[storage]`, `bind = "0.0.0.0:7475"` in
`[server]`, `token = "s3cret"` in `[security]`, `demo = "movies"` in `[storage]` to start
with the movies demo graph.

| Request | Answer |
|---------|--------|
| `GET /health` | `{"status":"ok"}` (never needs the token) |
| `GET /metrics` | server and database metrics in the Prometheus text format; admins and the token only ([details](Features.md#metrics)) |
| `GET /db` | `{"databases":[{"name","storage","default","nodes","relationships"}]}` |
| `POST /db/{name}/query` | runs `{"query": "...", "params": {...}, "timeoutMs": 5000}` against database `name` (`params` and `timeoutMs` optional) |
| `POST /query` | the same, against the default database |
| `POST /db/{name}/tx` | begins a transaction: `201 {"transaction":{"id","database","path","timeoutMs"}}` |
| `POST /db/{name}/tx/{id}` | runs `{"query", "params", "timeoutMs"}` in it; answers like a query |
| `POST /db/{name}/tx/{id}/commit` | commits: `{"committed":true}` |
| `DELETE /db/{name}/tx/{id}` | rolls back (cancelling a running statement): `{"rolledBack":true}` |
| `/tx`, `/tx/{id}`, `/tx/{id}/commit` | the same, against the default database |
| `POST /db/{name}/backup` | online backup into `storage.backup_dir`: `{"backup": "name"}`; admins only ([details](Backup.md)) |
| `POST /db/{name}/restore` | creates the database from a backup: `{"backup": "name", "overwrite": false}`; admins only |
| `POST /db/{name}/check` | consistency check of a persistent database: `{"check":{"consistent","errors",...}}`; admins only |

A query returns `{"columns":[...],"rows":[[...]],"stats":{"nodesCreated":1,...,"containsUpdates":true}}`.
A read-only query's rows are written as they are produced: a result over 64 KiB is sent as
a chunked `200` response while the query runs. If the query fails after that, the object
ends with the error instead of `stats`: `{"columns":[...],"rows":[...],"error":{"code":...,"message":...}}`;
check for `error` in a `200` body too.
A query request is its own transaction. Database commands (`CREATE DATABASE`,
`SHOW DATABASES`, `USE x MATCH ...`) work through either query endpoint.

**From a browser app** on another origin, list that origin in `server.cors_origins`
(comma-separated, or `"*"`): `cors_origins = "https://app.example.com, http://localhost:5173"`.
Responses to requests from those origins carry `Access-Control-Allow-Origin` (also errors,
so the page can read them), and `OPTIONS` preflights are answered without credentials
(methods `GET, POST, DELETE`; headers `Authorization, Content-Type, Accept`). Other
origins get no CORS headers, so the browser keeps their pages from reading responses. Send
credentials as an `Authorization` header from the page; the server sets no cookies.

Transactions, timeouts, how values and errors look, security and the implementation are
described in [Features](Features.md).

From **Java**, [`examples/java/GraphbuffClient.java`](../examples/java/GraphbuffClient.java) is
a dependency-free client (Java 17+, `java.net.http`) with a runnable demo:

```bash
java examples/java/GraphbuffClient.java http://127.0.0.1:7475
```

```java
var db = new GraphbuffClient("http://127.0.0.1:7475", null);   // or a token
db.query("CREATE DATABASE movies IF NOT EXISTS", null);
db.query("movies", "CREATE (:Person {name: $name, age: $age})", Map.of("name", "Ann", "age", 31));
var r = db.query("movies", "MATCH (p:Person) RETURN p.name AS name, p.age AS age", null);
String name = (String) r.get(0, "name");
long age = (Long) r.get(0, "age");          // INTEGER -> Long, FLOAT -> Double
```

With Jackson or Gson in the project, `HttpClient` plus your JSON library is all you need.
Send `{"query": ..., "params": ...}` and read `columns` / `rows`.
