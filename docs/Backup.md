# Backup, restore and consistency checks

A backup is a directory that opens as a store. It holds the last commit before the backup
started, and is written while queries and transactions keep running and committing (it
reads a snapshot, copying pages a batch at a time without filling the page cache).
Persistent databases back up as persistent stores (their pages and metadata); in-memory
and hybrid ones as hybrid stores (a snapshot file).

Set `backup_dir = "DIR"` in `[storage]` of the server's config file; then admins can:

* **Back up:** `POST /db/{name}/backup` with `{"backup": "monday"}` writes `DIR/monday`.
* **Restore:** `POST /db/{name}/restore` with `{"backup": "monday", "overwrite": false}`
  creates (or replaces) the database from it.
* **Check:** `POST /db/{name}/check` checks a persistent database.

From Rust: `DatabaseManager::backup_database`, `restore_database` and `check_database`.

* **Restore** copies the backup (it stays usable) and checks a persistent one before the
  database is created. An existing database is replaced only if it is empty or with
  overwrite, and stays as it was if the restore fails.
* **Check** (persistent storage only) reads every record and verifies
  adjacency lists in both directions (links, list ends, endpoints, relationship groups of
  dense nodes, degrees and self-loop counts), node, relationship, label and type counts,
  blob chains and free lists, every B+tree (page use, key order, leaf links) against the
  entries the records call for, index counts, and constraints. Lost space (blocks neither
  used nor free) is a warning. 200,000 nodes and 400,000 relationships: check 1.0 s, backup
  0.4 s (102 MiB).
* **Safety:** a backup goes into a new or empty directory only, and holds a
  `BACKUP_INCOMPLETE` marker until it is complete; stores refuse to open (and restore
  refuses to read) a directory that still has one.
* **From Rust:** `Database::backup` / `Database::check`,
  `DatabaseManager::backup_database` / `restore_database` / `check_database`, and
  `GraphStore::backup` / `GraphStore::check` (call them on a snapshot for an online run).
