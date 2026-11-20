# Bolt (Neo4j drivers)

With `bolt = "ADDR"` in `[server]` the server also speaks Bolt 4.4 and 5.0–5.4, the
protocol of the official Neo4j drivers:

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
* **Tested with the official drivers:** Python 5.28 and 6.4, Java 5.28 and 6.0, by the
  smoke suites in [`integration-tests/drivers`](../integration-tests/drivers/README.md).
* **Not done:** results are computed whole by `RUN` and then paged by `PULL`; impersonation,
  notification filters, and access modes (a write in a `READ` session runs). The server
  agent reads `Neo4j/5.4.0-graphbuff-…`, the form drivers expect.
