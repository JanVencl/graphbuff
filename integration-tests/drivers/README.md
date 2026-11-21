# Driver compatibility suite

Smoke tests that run the official Neo4j drivers against `graphbuff-server` over Bolt. Each suite starts its own server (HTTP and Bolt on free ports,
token auth, from a config file it writes) and stops it at the end.

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

## JavaScript (`javascript/`, `node:test`, Node 18+)

```bash
cd integration-tests/drivers/javascript
npm ci
npm test
npm install --no-save neo4j-driver@6.3.2 && npm test
```

## .NET (`dotnet/`, xUnit, .NET 8+ SDK)

```bash
cd integration-tests/drivers/dotnet
dotnet test
dotnet test -p:Neo4jDriverVersion=6.5.1
```

The project pins driver 5.28.4 (the 5.x LTS line).

## Go (`go/`, `go test`, Go 1.24+)

```bash
cd integration-tests/drivers/go
go mod tidy
go test -count=1 ./...
```

`go.mod` requires driver v5.28.6 (the 5.x LTS line). The suite uses the v5 API, which v6
keeps as deprecated aliases; to run it on v6, change the import path `/v5` to `/v6` in
`go.mod` and `*.go` (CI does this with `sed`).

## What is covered

All five suites check the same things: connectivity, server info and `neo4j://` routing;
basic and bearer auth and a wrong password; every value type both ways (integers at the
edges, long strings, nested lists and maps, all temporal types, points, nodes,
relationships, paths with reversed relationships; element ids that `elementId()`
accepts); fetch sizes and partly read results; summary counters, query types,
`EXPLAIN`/`PROFILE`; `execute_query` / `executableQuery`; managed (retrying) and explicit
transactions, commit, rollback, several open results in one transaction, metadata and
timeouts; bookmarks; concurrent sessions; syntax, constraint and runtime errors and
recovering from them; other databases, and database commands on the `system` database.
Java also runs the async API. JavaScript also checks `Integer` and `BigInt` results,
that a plain JS number is sent as a float, named time zones, async iteration, the
bookmark manager and the reactive (`rxSession`) API. .NET also checks named time zones,
that a C# `int` is sent as an integer, the bookmark manager and `ExecutableQuery` with
`WithMap`. Go also checks named time zones, Go `int` parameters and
`GetRecordValue`.

Python's `test_accounts.py` starts servers of its own: one with user accounts in a data
directory (made with `graphbuff-server users --config FILE add`), checking logins, roles
(`Forbidden`) and user management on the `system` database; and one with TLS, checking
`bolt+s` with a custom CA, `bolt+ssc`, `neo4j+ssc` and refused plain or untrusted
connections. The TLS tests are skipped unless the server was built with TLS:

```bash
cargo build -p graphbuff-server --features tls-rustcrypto
```

CI (`.github/workflows/drivers.yml`) runs every suite on both driver lines for each pull
request that changes the suites or the server. Last run (2026-11-21): Python 5.28.2 and
6.4.1 (38 passed, 4 TLS tests skipped); Java 5.28.5 and 6.0.5, 32 tests; JavaScript 5.28.3
and 6.3.2, 35 tests; .NET 5.28.4 and 6.5.1, 33 tests. All pass. Go: not run yet.
Accounts and TLS with a TLS build (2026-11-13): Python 5.28.2, 38 tests, all pass.
