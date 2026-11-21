# Bolt (Neo4j drivers)

With `bolt = "ADDR"` in `[server]` the server also speaks Bolt 4.4, 5.0–5.4 and 5.6–5.8
(5.5 was never served by Neo4j either), the protocol of the official Neo4j drivers:

```toml
[server]
bolt = "127.0.0.1:7687"

[security]
token = "s3cret"
```

```python
from neo4j import GraphDatabase
with GraphDatabase.driver("bolt://127.0.0.1:7687", auth=("neo4j", "s3cret")) as driver:
    records, summary, keys = driver.execute_query("MATCH (p:Person) RETURN p.name AS name")
```

* **Supported:** auto-commit queries and explicit transactions (`BEGIN`/`COMMIT`/
  `ROLLBACK`, several open results per transaction by `qid`), `PULL {n}` and `DISCARD`,
  pipelining, `RESET` (it interrupts the running statement), `LOGON`/`LOGOFF` (5.1+),
  `TELEMETRY` (5.4), `ROUTE` (this server is the whole cluster, so `neo4j://` URIs work),
  the `db` of a query or transaction, `tx_timeout`, and the 4.4 `utc` patch. Summaries
  carry counters under Neo4j's names, a query type, bookmarks, and `plan`/`profile` for
  `EXPLAIN`/`PROFILE`.
* **Newer versions:** the manifest handshake (5.7+: the client asks for `00 00 01 FF`
  and picks from the versions listed; HELLO then reports `protocol_version`); GQL status
  objects in summaries (5.6+: `00000` successful completion, `02000` no data, `00001`
  omitted result, under `statuses`); GQL errors (5.7+: `neo4j_code`, `gql_status`,
  `description` and `diagnostic_record` with its `_classification`); and the home database
  in the answer to `BEGIN` and auto-commit `RUN` when the client names none (5.8, for the
  drivers' home database cache).
* **Values:** nodes, relationships (with element ids from Bolt 5, the strings
  `elementId()` returns) and paths,
  dates, times, date-times with offsets or named time zones, durations and points, in both
  directions. Byte arrays are refused.
* **Authentication:** `basic` with a user's name and password; with `security.token`, also
  `basic` with the token as the password (any user name) or `bearer` with the token.
* **Transactions and limits** behave as over HTTP: an explicit transaction holds its
  database's write lock until it ends, and is rolled back after
  `queries.transaction_timeout_seconds` without a message or when its connection closes;
  `queries.query_timeout_ms` caps every statement.
  Error codes follow Neo4j's (`Neo.ClientError.Statement.SyntaxError`,
  `Neo.ClientError.Transaction.TransactionTimedOut`, ...).
* **Administration** goes to the `system` database, as with Neo4j (the name is
  reserved; it cannot be queried): `db: "system"` runs database and user commands only (`CREATE DATABASE`,
  `SHOW DATABASES`, `CREATE USER`, `GRANT ROLE`, …), in auto-commit or in a transaction
  (where each command takes effect when it runs, and a rollback does not undo it). Other
  databases also run them in auto-commit.
* **Tested with the official drivers:** Python 5.28 and 6.4, Java 5.28 and 6.0,
  JavaScript 5.28 and 6.3 (all negotiate Bolt 5.8), by the smoke suites in
  [`integration-tests/drivers`](../integration-tests/drivers/README.md).
* **Results stream:** an auto-commit read-only query runs while `PULL`s take its rows; it
  waits for the next `PULL` once `n` rows are sent, and `DISCARD` (or `RESET`) stops it. A
  failure after rows went out answers the `PULL` that reaches it. Results in an explicit
  transaction, and of queries that write, are computed by `RUN` and then paged by `PULL`.
* **Not done:** Bolt 6; impersonation, notifications (none are produced, so notification filters are accepted
  and have nothing to filter), and access modes (a write in a `READ` session runs). GQL
  status codes for errors are GQL's general class codes (`42001` invalid syntax, `42000`,
  `22000`, `40000`, `50N42`) rather than Neo4j's finer ones. The server agent reads
  `Neo4j/5.8.0-graphbuff-…` (the negotiated version), the form drivers expect.
