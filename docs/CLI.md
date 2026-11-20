# Command-line client

`graphbuff-cli` runs Cypher against a server over the HTTP API: statements on the command
line, scripts from files or standard input, or an interactive prompt.

```bash
cargo run --release -p graphbuff-cli -- -u admin              # prompt; asks for the password
graphbuff-cli -u admin "MATCH (n) RETURN count(n);"
graphbuff-cli -d movies --format csv -f report.cypher > report.csv
graphbuff-cli --token s3cret -P name=Ann "MATCH (p:Person {name: \$name}) RETURN p"
```

```text
graphbuff> CREATE DATABASE shop;
graphbuff> :use shop
shop> :begin
shop(tx)> CREATE (:Item {name: 'pen'})
        > RETURN 1 AS done;
shop(tx)> :commit
```

* **Connecting:** `-a URL` (default `http://127.0.0.1:7475`; `https://` (in the release
  binaries and the Docker image; from source, the `tls` or `tls-rustcrypto` feature) trusts `--cacert FILE`, `SSL_CERT_FILE` or the system's CA
  bundle), `-u NAME` with `-p PASSWORD` (asked for on a terminal), or `--token`. The
  environment variables `GRAPHBUFF_URL`, `GRAPHBUFF_USER`, `GRAPHBUFF_PASSWORD` and
  `GRAPHBUFF_TOKEN` stand in for them. At the prompt, a server that wants a login asks for one.
* **Input:** statements end with `;` and may span lines (a file's last one needs none).
  `USE name;` on its own switches the database for the statements after it. Lines
  starting with `:` are commands: `:use [NAME]`, `:dbs`, `:begin` / `:commit` /
  `:rollback`, `:param NAME EXPR` (evaluated by the server, then sent as `$NAME`),
  `:params [clear]`, `:format`, `:timeout [MS|off]`, `:source FILE`, `:help`, `:quit`.
  A transaction still open at the end is rolled back.
* **Output:** `--format table` (default on a terminal, with row count, time and what
  changed), `plain` (tab-separated, the default when piped), `csv`, or `json` (the server's
  reply, one line per statement). Notes and errors go to standard error; a syntax error
  points at its place in the statement.
* **Errors:** a script stops at the first failing statement unless `--continue`. Exit
  status 0 when all ran, 1 when a statement or command failed, 2 for wrong usage or when
  the server cannot be reached for the prompt.
