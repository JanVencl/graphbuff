# Users and TLS

Clients log in with a user name and password: HTTP Basic authentication
(`Authorization: Basic …`) or Bolt's `basic` scheme. Accounts cannot be turned off; a
`security.token` works alongside them, as an administrator. Programs may use
API tokens of a user instead of their password (below). The accounts live in the system database of
the data directory (`DATA/system/`, synced on every commit; in memory with
`mode = "memory"`), as Argon2id hashes (64 MiB, 3 passes, random salts) in the PHC string
format. A hash made with other parameters is replaced at the user's next login.

```bash
# the first administrator, offline, while no server uses the data directory (password
# from GRAPHBUFF_ADMIN_PASSWORD or _FILE, else asked for twice without echo on a
# terminal, else the first line of standard input); it reads the same config file as
# the server (or --config FILE), and refuses when there is an administrator already
graphbuff-server admin init root
graphbuff-server

# lost the password? set a new one, offline (from GRAPHBUFF_PASSWORD or asked for)
graphbuff-server admin reset-password root

# other accounts, offline, the same way (passwords as for reset-password)
graphbuff-server users list
graphbuff-server users add kim editor
graphbuff-server users grant kim admin     # also: password, revoke, remove
```

Or let the server create the first administrator when it starts and no user has the
admin role (Docker, Compose, CI, systemd): `GRAPHBUFF_ADMIN_USER` (default `admin`) and
`GRAPHBUFF_ADMIN_PASSWORD`, or `GRAPHBUFF_ADMIN_PASSWORD_FILE` naming a file that holds
the password (one trailing newline is ignored). Once there is an administrator these are
ignored, so they may stay set.

Without these variables, a server started in a terminal asks for the administrator's
name and password (twice, without echo). Without a terminal (a service, a container
without `-it`, CI) it does not start, and says what to do; neither does a server with
`mode = "memory"`, whose accounts would be lost at every stop. With `security.token` it
starts anyway, and the token holder creates the users.

```powershell
# Windows: the same, asking without echo (PowerShell 7)
$env:GRAPHBUFF_ADMIN_PASSWORD = Read-Host -MaskInput 'password'
graphbuff-server
```

Each user holds roles; the strongest decides what they may do, checked for every
statement (a role granted or revoked applies to open connections at once):

| Role | May |
|------|-----|
| `reader` | read queries, `EXPLAIN`, `SHOW DATABASES`, explicit transactions that only read |
| `editor` | also write data, and create and drop indexes and constraints |
| `admin` | also create, alter and drop databases and aliases, and manage users |

A user with no role may only log in, run `SHOW CURRENT USER` and change their own
password. Once there is an administrator, no change may leave none: dropping the last
one, revoking their role or replacing their account is refused. New passwords need at
least 8 characters and must differ from the user name. Administrators manage accounts with Neo4j's commands (on the `system` database
over Bolt, or anywhere in auto-commit): `CREATE [OR REPLACE] USER name [IF NOT EXISTS]
SET [PLAINTEXT] PASSWORD 'pw' | $pw [CHANGE NOT REQUIRED]`, `ALTER USER name SET PASSWORD
…`, `RENAME USER a TO b`, `DROP USER name [IF EXISTS]`, `GRANT ROLE[S] r, … [ON DATABASE
db] TO u, …`, `REVOKE ROLE[S] r [ON DATABASE db] FROM u`, `SHOW USERS`, `SHOW ROLES`; every
user may run `SHOW CURRENT USER` and `ALTER CURRENT USER SET PASSWORD FROM 'old' TO 'new'`.
Explicit HTTP transactions belong to the client that began them. Not supported: custom
roles and privileges (`GRANT MATCH …`), `CHANGE REQUIRED`, suspended users, home databases.

**Roles on one database.** `reader` and `editor` can be granted on a single database:

```cypher
GRANT ROLE editor ON DATABASE movies TO alice
GRANT ROLE reader ON DATABASE analytics TO alice
REVOKE ROLE editor ON DATABASE movies FROM alice
```

- On each database, a user may do what the strongest role they hold there allows: a
  role granted without `ON DATABASE` (or `ON DATABASES *`) counts everywhere, one granted
  `ON DATABASE db` only on `db`.
- A query needs the role on the database it runs on: the one named in the request
  (`/db/{name}/query`, Bolt's `db`) or by `USE`, else the default database. So does an
  explicit transaction.
- `SHOW DATABASES` and `GET /db` need a role on some database (they list all databases).
- `admin` is held on every database only: creating and dropping databases, managing users,
  backups, metrics and terminating others' transactions stay with administrators.
- Granting needs an existing database; names are case-insensitive. Grants stay when a
  database is dropped (and apply again if one of that name is created); revoke them to
  remove them.
- `SHOW USERS` and `SHOW CURRENT USER` list them in the `databaseRoles` column, a map from
  database to roles.
- The system database keeps them in each user's `databaseRoles` property. Older versions
  ignore them, so after a downgrade those users have only the roles held everywhere.

**API tokens** let programs log in without a user's password. A user makes their own:

```cypher
CREATE TOKEN laptop                                            // read only, 90 days
CREATE TOKEN importer ACCESS WRITE ON DATABASE movies EXPIRES IN 12 HOURS
CREATE TOKEN agent FOR USER alice EXPIRES NEVER                // for another user: admins
SHOW TOKENS                                                    // one's own; admins: everyone's
DROP TOKEN laptop                                              // FOR USER alice: admins
```

`CREATE TOKEN` returns the token (`gbt_…`) in the column `token`. It is shown only this
once: the server keeps its SHA-256 hash. Send it as `Authorization: Bearer gbt_…`, with
Bolt's `bearer` scheme (Python: `auth=bearer_auth("gbt_…")`) or as `graphbuff-cli --token`.

- A token may do what its user may at that moment, but no more than its own cap:
  `ACCESS READ` (the default) reads, `ACCESS WRITE` also writes data, indexes and
  constraints. A role taken from the user applies to their tokens at once.
- `ON DATABASE db, …` limits it to these databases; without it, it works on every database
  its user may use.
- It is never an administrator, even an administrator's token. It manages no users, no
  passwords and no tokens; of the user commands it runs `SHOW CURRENT USER` only.
- It expires after 90 days, or `EXPIRES IN n DAYS | HOURS` (up to 3650 days), or
  `EXPIRES NEVER`. An expired token is refused, and stays listed until it is dropped.
- A user keeps their tokens when they are renamed or get a new password; drop the tokens
  too if the account may be compromised. Dropping or replacing a user drops them.
- A user sees and may terminate the queries of their tokens (`SHOW TRANSACTIONS`); a token
  sees only its own. Logs show `user[gbt_id]`.
- A user may have 100 tokens, with names that are unique for the user.
- `SHOW TOKENS` lists `name`, `user`, `access`, `databases` (empty: every one), `created`,
  `expires`, `lastUsed` and `id` (the public part of the token).
- An agent that logged in with OAuth ([MCP](MCP.md#hosted-clients-oauth)) has a token
  named `oauth-…`, renewed every hour while the agent uses it. Dropping it ends the agent's
  access.
- These commands are graphbuff's own; Neo4j has none like them. Older versions ignore
  tokens.

Offline, while no server uses the data folder, `graphbuff-server tokens` does the same. The
new token is the only output on standard output:

```bash
graphbuff-server tokens add alice agent --write --database movies --days 30
graphbuff-server tokens list
graphbuff-server tokens remove alice agent
```

**Failed logins** lock a client out for a while, over HTTP and Bolt alike. They are counted
per client address and user name, and per client address over all names. After
`security.max_failed_logins` (default 10) failures of one address and name, or three times
as many from one address over all names, its logins are refused for
`security.login_lockout_seconds` (default 60), and each further failure after a lockout
doubles that, up to an hour. While locked out, a login is refused without checking the
password, even a right one: HTTP answers `429 TooManyRequests` with `Retry-After`, Bolt
fails with `Neo.ClientError.Security.AuthenticationRateLimit`. A successful login clears
the count of its address and name; counts are forgotten after an hour without failures.
Wrong API tokens are counted per address and token id. Requests without credentials, and
expired API tokens, are not counted. `max_failed_logins = 0` turns this off. At
most one password hash per CPU is computed at once, so logins cannot exhaust the memory
the hashes take.

```toml
[security]
max_failed_logins = 10
login_lockout_seconds = 60
```

**TLS** serves HTTPS and Bolt over TLS (TLS 1.3 and 1.2) on the same ports. The release
binaries and the Docker image have it. Built from source, it is an optional feature, with
two choices of crypto provider for rustls:

```bash
cargo build --release -p graphbuff-server --features tls             # ring (needs a C compiler)
cargo build --release -p graphbuff-server --features tls-rustcrypto  # pure Rust (pre-release)
```

```toml
[server]
bind = "0.0.0.0:7475"
bolt = "0.0.0.0:7687"

[security]
tls_cert = "cert.pem"   # relative to the config file's folder
tls_key = "key.pem"
```

Drivers then connect with `bolt+s://` (or `bolt+ssc://` for a self-signed certificate).
`tls_allow_plain = true` also accepts clients without TLS on the same ports (the first byte
tells them apart); without it, plain HTTP requests get a `400` and plain Bolt
connections are closed.
