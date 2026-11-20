# Multiple databases

A `DatabaseManager` holds several named databases, each with its own storage. Clients
use it through a `Session`, which has a current database and runs the database commands:

```cypher
CREATE DATABASE movies;                                   // default storage of the manager
CREATE DATABASE scratch IF NOT EXISTS OPTIONS {storage: 'memory'};
CREATE OR REPLACE DATABASE events
  OPTIONS {storage: 'hybrid', sync: 'os', checkpointAfterBytes: 16777216};
SHOW DATABASES;            // name, storage, default, current, nodes, relationships
SHOW DATABASE movies;
SHOW DEFAULT DATABASE;
USE movies;                               // switch the session's current database
USE scratch MATCH (n) RETURN count(n);    // run one query elsewhere, without switching
DROP DATABASE scratch IF EXISTS;
```

```rust
use graphbuff::DatabaseManager;

let mgr = DatabaseManager::open("data")?;   // or DatabaseManager::in_memory()
let mut s = mgr.session();                  // starts on the default database, `graphbuff`
s.execute("CREATE OR REPLACE DATABASE movies")?;
s.execute("USE movies")?;
s.execute("CREATE (:Movie {title: 'Heat'})")?;
```

* **`CREATE OR REPLACE DATABASE`** drops an existing database with all its data and
  creates it again empty, with the given options; if it doesn't exist it is just created.
  It cannot be combined with `IF NOT EXISTS`. Plain `CREATE DATABASE` fails if the name is
  taken; `IF NOT EXISTS` leaves the existing database alone.
* **Options:** `storage` is `'memory'`, `'hybrid'` or `'persistent'`.
  `sync` (`'commit'` or `'os'`) and `checkpointAfterBytes` apply to hybrid and persistent
  storage, `cacheBytes` and `maxTransactionBytes` (the transaction size cap) to persistent
  storage. Without `OPTIONS`, a manager opened on a
  directory creates hybrid databases (persistent ones with `mode = "persistent"` in the
  server's config file, or
  `DatabaseManager::open_with(dir, StorageKind::persistent())`) and an in-memory manager
  creates memory databases. `OPTIONS $map` works too, as does `$name`
  for the database name. `WAIT`/`NOWAIT` and `DESTROY DATA` are accepted and ignored.
* **Names** are case-insensitive, 1–63 characters (letters, digits, `.`, `-`, `_`),
  start with a letter, and need backticks if they contain `-`. Windows device names such
  as `con` are rejected because names are also folder names.
* **The default database** (`graphbuff`) cannot be dropped, but it can be replaced.
  Dropping a session's current database moves that session back to the default one.
* **On disk**, `data/catalog` lists the databases and their storage settings, and each
  hybrid or persistent database lives in `data/databases/<name>/`. Memory databases stay in the catalog
  across restarts but come back empty. A `LOCK` file keeps a second process out.
* A plain `Database` has no other databases, so it rejects `USE` and database commands
  with an `Unsupported` error.
