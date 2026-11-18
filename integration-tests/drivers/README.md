# Driver compatibility suite

Smoke tests that run the official Neo4j drivers against `graphbuff-server` over Bolt. Each suite starts its own server (HTTP and Bolt on free ports,
`--token` auth) and stops it at the end.

Build the server first. The suites use the binary named by `GRAPHBUFF_SERVER`, else
whichever of `target/release` and `target/debug` was built last:

```bash
cargo build -p graphbuff-server
```

## Python (`python/`, pytest)

```bash
cd integration-tests/drivers/python
python -m venv .venv
.venv/Scripts/pip install -r requirements.txt   # .venv/bin/pip elsewhere
.venv/Scripts/python -m pytest -q
```

`requirements.txt` pins driver 5.28 (the 5.x LTS line). To try another driver, install it
into the venv (`pip install neo4j==6.4.1`) and run again.

## Java (`java/`, Maven + JUnit 5, Java 17+)

```bash
cd integration-tests/drivers/java
mvn -B test
mvn -B test -Dneo4j.driver.version=6.0.5
```

## What is covered

Both suites check the same things: connectivity, server info and `neo4j://` routing;
basic and bearer auth and a wrong password; every value type both ways (integers at the
edges, long strings, nested lists and maps, all temporal types, points, nodes,
relationships, paths with reversed relationships; element ids that `elementId()`
accepts); fetch sizes and partly read results; summary counters, query types,
`EXPLAIN`/`PROFILE`; `execute_query` / `executableQuery`; managed (retrying) and explicit
transactions, commit, rollback, several open results in one transaction, metadata and
timeouts; bookmarks; concurrent sessions; syntax, constraint and runtime errors and
recovering from them; other databases, and database commands on the `system` database.
Java also runs the async API.

Python's `test_accounts.py` starts servers of its own: one with user accounts in a data
directory (made with `graphbuff-server users --data DIR add`), checking logins, roles
(`Forbidden`) and user management on the `system` database; and one with TLS, checking
`bolt+s` with a custom CA, `bolt+ssc`, `neo4j+ssc` and refused plain or untrusted
connections. The TLS tests are skipped unless the server was built with TLS:

```bash
cargo build -p graphbuff-server --features tls-rustcrypto
```

Last run (2026-11-13): Python driver 5.28.2 and 6.4.1, 31 tests; Java driver 5.28.5 and
6.0.5, 32 tests. All pass. Accounts and TLS (2026-11-13): Python 5.28.2, 38 tests, all pass.
