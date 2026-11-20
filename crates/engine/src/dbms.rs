//! Several named databases in one place: a catalog, database commands and sessions.
//!
//! A [`DatabaseManager`] owns a set of databases, each with its own [`StorageKind`].
//! Clients talk to it through a [`Session`], which has a current database (switched
//! with `USE name`) and also runs the database commands:
//!
//! ```text
//! CREATE [OR REPLACE] DATABASE name [IF NOT EXISTS] [OPTIONS {storage: 'hybrid', …}]
//! DROP DATABASE name [IF EXISTS]
//! SHOW DATABASES | SHOW DATABASE name | SHOW DEFAULT DATABASE
//! USE name                      -- switch the session's current database
//! USE name MATCH … RETURN …     -- run one query against another database
//! ```
//!
//! A manager opened on a directory keeps a `catalog` file there (the list of databases
//! and their storage settings) and stores each hybrid database under
//! `databases/<name>/`. Databases with `memory` storage stay in the catalog across
//! restarts but come back empty.
//!
//! The name `system` ([`SYSTEM_DATABASE`]) is reserved. With the `system-db` feature
//! every manager also has a system database, for the server's user accounts and roles
//! ([`DatabaseManager::system_database`]). It is not in the catalog: it lives in
//! `system/` of the data directory (hybrid storage, synced on every commit), or in
//! memory for an in-memory manager. It cannot be created, dropped, replaced, restored,
//! listed by `SHOW DATABASES` or queried with `USE`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use graphbuff_cypher::ast::{
    DatabaseCommand, DbName, ExistingDatabase, Expr, ShowDatabases, Statement,
};

use crate::{
    BoxedStore, DEFAULT_MAX_TRANSACTION_BYTES, Database, Error, HybridOptions, PersistentOptions, QueryResult, QueryStats, Result, ResultValue,
    StorageMode, SyncPolicy, Value,
};

/// Name of the database every manager starts with. It cannot be dropped.
pub const DEFAULT_DATABASE: &str = "graphbuff";

/// Name of the system database (see the [module docs](self)). Reserved in every build.
pub const SYSTEM_DATABASE: &str = "system";

/// Directory of the system database in the data directory (outside `databases/`, so a
/// leftover user database directory can never be mistaken for it).
#[cfg(feature = "system-db")]
const SYSTEM_DIR: &str = "system";

const CATALOG_FILE: &str = "catalog";
const CATALOG_TMP: &str = "catalog.tmp";
const CATALOG_HEADER: &str = "graphbuff-catalog 1";
const DATABASES_DIR: &str = "databases";
const LOCK_FILE: &str = "LOCK";

/// How one database stores its graph. Unlike [`StorageMode`] it carries no path: the
/// manager places each database's files under its own directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageKind {
    /// Volatile; the database is empty again after a restart.
    InMemory,
    /// In memory plus write-ahead log and snapshots. Needs a manager with a directory.
    Hybrid {
        sync: SyncPolicy,
        checkpoint_after_bytes: u64,
    },
    /// Disk-resident pages with a bounded page cache, for graphs larger than RAM.
    /// Needs a manager with a directory.
    Persistent {
        sync: SyncPolicy,
        checkpoint_after_bytes: u64,
        cache_bytes: u64,
        /// The most one transaction may change (see
        /// [`PersistentOptions::max_transaction_bytes`]).
        max_transaction_bytes: u64,
    },
}

impl StorageKind {
    /// Hybrid storage with default settings (fsync on every commit).
    pub fn hybrid() -> Self {
        let d = HybridOptions::new("");
        StorageKind::Hybrid {
            sync: d.sync,
            checkpoint_after_bytes: d.checkpoint_after_bytes,
        }
    }

    /// Persistent storage with default settings (fsync on every commit, 64 MiB cache).
    pub fn persistent() -> Self {
        let d = PersistentOptions::new("");
        StorageKind::Persistent {
            sync: d.sync,
            checkpoint_after_bytes: d.checkpoint_after_bytes,
            cache_bytes: d.cache_bytes,
            max_transaction_bytes: d.max_transaction_bytes,
        }
    }

    /// The name used in `OPTIONS {storage: …}` and `SHOW DATABASES`.
    pub fn name(&self) -> &'static str {
        match self {
            StorageKind::InMemory => "memory",
            StorageKind::Hybrid { .. } => "hybrid",
            StorageKind::Persistent { .. } => "persistent",
        }
    }
}

/// One row of [`DatabaseManager::databases`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseInfo {
    pub name: String,
    pub storage: StorageKind,
    pub default: bool,
    pub nodes: usize,
    pub relationships: usize,
}

type Db = Arc<Database<BoxedStore>>;

struct Entry {
    storage: StorageKind,
    db: Db,
}

struct Catalog {
    default: String,
    dbs: BTreeMap<String, Entry>,
}

/// A set of named databases, each with its own storage. See the [module docs](self).
pub struct DatabaseManager {
    root: Option<PathBuf>,
    default_storage: StorageKind,
    catalog: RwLock<Catalog>,
    /// Where `LOAD CSV` reads files, for every database.
    import_dir: RwLock<Option<PathBuf>>,
    /// The procedures `CALL` can run, in every database.
    procedures: RwLock<Arc<crate::Procedures>>,
    /// The kernel backend of every database, when not the default.
    kernels: RwLock<Option<Arc<dyn crate::Kernels>>>,
    /// The system database; `None` without the `system-db` feature.
    system: Option<Db>,
    _lock: Option<File>,
}

/// Fails for the reserved name `system` (`name` already normalized).
fn check_not_system(name: &str, what: &str) -> Result<()> {
    if name == SYSTEM_DATABASE {
        return Err(Error::Constraint(format!(
            "cannot {what} `{SYSTEM_DATABASE}`: the name is reserved for the system database"
        )));
    }
    Ok(())
}

fn system_not_queryable() -> Error {
    Error::Constraint(format!(
        "the system database `{SYSTEM_DATABASE}` cannot be queried; it changes only through user and role commands"
    ))
}

/// The system database of an in-memory manager.
fn system_in_memory() -> Option<Db> {
    #[cfg(feature = "system-db")]
    {
        Some(Arc::new(Database::with_store(Box::new(crate::MemoryGraph::new()))))
    }
    #[cfg(not(feature = "system-db"))]
    {
        None
    }
}

/// Opens (or creates) the system database in `root/system/`.
#[cfg(feature = "system-db")]
fn system_on_disk(root: &Path) -> Result<Option<Db>> {
    let dir = root.join(SYSTEM_DIR);
    let mode = StorageMode::Hybrid(HybridOptions::new(&dir).sync(SyncPolicy::EveryCommit));
    let db = Database::open(mode).map_err(|e| Error::Storage(format!("opening the system database: {e}")))?;
    Ok(Some(Arc::new(db)))
}

#[cfg(not(feature = "system-db"))]
fn system_on_disk(_root: &Path) -> Result<Option<Db>> {
    Ok(None)
}

/// Checks a database name and returns its canonical (lower-case) form. Names are
/// case-insensitive and also used as directory names, so they are kept portable.
pub(crate) fn normalize_name(raw: &str) -> Result<String> {
    let name = raw.to_ascii_lowercase();
    let invalid = |why: &str| {
        Err(Error::Semantic(format!(
            "invalid database name `{raw}`: {why}"
        )))
    };
    if name.is_empty() || name.len() > 63 {
        return invalid("it must be 1 to 63 characters long");
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return invalid("it must start with a letter");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
    {
        return invalid("use only letters, digits, '.', '-' and '_'");
    }
    if name.ends_with('.') || name.contains("..") {
        return invalid("dots must separate name parts");
    }
    let stem = name.split('.').next().unwrap_or("");
    let reserved = matches!(stem, "con" | "prn" | "aux" | "nul")
        || ((stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        return invalid("it is a reserved device name on Windows");
    }
    Ok(name)
}

fn not_found(name: &str) -> Error {
    Error::Runtime(format!("database `{name}` does not exist"))
}

fn io(path: &Path, what: &str) -> impl FnOnce(std::io::Error) -> Error {
    let ctx = format!("{what} {}", path.display());
    move |e| Error::io(ctx, e)
}

/// Opens the store in `dir` with `storage` (hybrid or persistent), default settings.
fn open_dir(dir: &Path, storage: &StorageKind) -> Result<Database<BoxedStore>> {
    let mode = match storage {
        StorageKind::InMemory => return Err(Error::Unsupported("in-memory storage has no directory".into())),
        StorageKind::Hybrid { .. } => StorageMode::Hybrid(HybridOptions::new(dir)),
        StorageKind::Persistent { .. } => StorageMode::Persistent(PersistentOptions::new(dir)),
    };
    Database::open(mode)
}

fn open_db(root: Option<&Path>, name: &str, storage: &StorageKind, fresh: bool) -> Result<Db> {
    let dir = || -> Result<PathBuf> {
        let root = root.ok_or_else(|| {
            Error::Unsupported(format!(
                "{} storage needs a data directory; open the manager with DatabaseManager::open",
                storage.name()
            ))
        })?;
        let dir = root.join(DATABASES_DIR).join(name);
        // A directory left by a database that is no longer in the catalog (an earlier
        // drop that could not delete it) must not leak into a new database.
        if fresh && dir.exists() {
            fs::remove_dir_all(&dir).map_err(io(&dir, "removing stale"))?;
        }
        Ok(dir)
    };
    let mode = match storage {
        StorageKind::InMemory => StorageMode::InMemory,
        StorageKind::Hybrid {
            sync,
            checkpoint_after_bytes,
        } => StorageMode::Hybrid(
            HybridOptions::new(dir()?)
                .sync(*sync)
                .checkpoint_after_bytes(*checkpoint_after_bytes),
        ),
        StorageKind::Persistent {
            sync,
            checkpoint_after_bytes,
            cache_bytes,
            max_transaction_bytes,
        } => StorageMode::Persistent(
            PersistentOptions::new(dir()?)
                .sync(*sync)
                .checkpoint_after_bytes(*checkpoint_after_bytes)
                .cache_bytes(*cache_bytes)
                .max_transaction_bytes(*max_transaction_bytes),
        ),
    };
    Ok(Arc::new(Database::open(mode)?))
}

fn parse_catalog(text: &str, path: &Path) -> Result<(String, Vec<(String, StorageKind)>)> {
    let bad =
        |line: usize, msg: &str| Error::Storage(format!("{} line {line}: {msg}", path.display()));
    let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l.trim()));
    match lines.next() {
        Some((_, CATALOG_HEADER)) => {}
        _ => return Err(bad(1, "not a graphbuff catalog")),
    }
    let mut default = None;
    let mut dbs = Vec::new();
    for (n, line) in lines {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [] => {}
            ["default", name] => {
                default = Some(normalize_name(name).map_err(|_| bad(n, "invalid name"))?)
            }
            ["database", name, kind, settings @ ..] => {
                let name = normalize_name(name).map_err(|_| bad(n, "invalid name"))?;
                let mut storage = match *kind {
                    "memory" => StorageKind::InMemory,
                    "hybrid" => StorageKind::hybrid(),
                    "persistent" => StorageKind::persistent(),
                    _ => return Err(bad(n, "unknown storage kind")),
                };
                for s in settings {
                    let (sync, checkpoint_after_bytes, cache_bytes, max_transaction_bytes) = match &mut storage {
                        StorageKind::InMemory => {
                            return Err(bad(n, "settings are not valid for memory storage"));
                        }
                        StorageKind::Hybrid {
                            sync,
                            checkpoint_after_bytes,
                        } => (sync, checkpoint_after_bytes, None, None),
                        StorageKind::Persistent {
                            sync,
                            checkpoint_after_bytes,
                            cache_bytes,
                            max_transaction_bytes,
                        } => (sync, checkpoint_after_bytes, Some(cache_bytes), Some(max_transaction_bytes)),
                    };
                    match s.split_once('=') {
                        Some(("sync", "commit")) => *sync = SyncPolicy::EveryCommit,
                        Some(("sync", "os")) => *sync = SyncPolicy::OsBuffered,
                        Some(("checkpoint_after_bytes", v)) => {
                            *checkpoint_after_bytes = v
                                .parse()
                                .map_err(|_| bad(n, "invalid checkpoint_after_bytes"))?;
                        }
                        Some(("cache_bytes", v)) if cache_bytes.is_some() => {
                            *cache_bytes.unwrap() =
                                v.parse().map_err(|_| bad(n, "invalid cache_bytes"))?;
                        }
                        Some(("max_transaction_bytes", v)) if max_transaction_bytes.is_some() => {
                            *max_transaction_bytes.unwrap() = v
                                .parse()
                                .map_err(|_| bad(n, "invalid max_transaction_bytes"))?;
                        }
                        _ => return Err(bad(n, &format!("unknown setting `{s}`"))),
                    }
                }
                if dbs.iter().any(|(d, _)| *d == name) {
                    return Err(bad(n, "duplicate database"));
                }
                dbs.push((name, storage));
            }
            _ => return Err(bad(n, "unrecognized line")),
        }
    }
    let default = default.ok_or_else(|| bad(0, "no default database"))?;
    if !dbs.iter().any(|(d, _)| *d == default) {
        return Err(bad(0, "the default database is not in the catalog"));
    }
    Ok((default, dbs))
}

impl DatabaseManager {
    /// A manager without a directory: every database is in memory and the catalog
    /// itself is forgotten when the manager is dropped.
    pub fn in_memory() -> Self {
        let mut dbs = BTreeMap::new();
        dbs.insert(
            DEFAULT_DATABASE.to_string(),
            Entry {
                storage: StorageKind::InMemory,
                db: Arc::new(Database::with_store(Box::new(crate::MemoryGraph::new()))),
            },
        );
        DatabaseManager {
            root: None,
            default_storage: StorageKind::InMemory,
            catalog: RwLock::new(Catalog {
                default: DEFAULT_DATABASE.to_string(),
                dbs,
            }),
            import_dir: RwLock::new(None),
            procedures: RwLock::new(crate::Procedures::builtin()),
            kernels: RwLock::new(None),
            system: system_in_memory(),
            _lock: None,
        }
    }

    /// Opens (or creates) a manager in `root`. New databases use hybrid storage unless
    /// their `OPTIONS` say otherwise.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with(root, StorageKind::hybrid())
    }

    /// Like [`open`](Self::open), with the storage used for the default database when it
    /// is first created, and for `CREATE DATABASE` without a `storage` option.
    pub fn open_with(root: impl Into<PathBuf>, default_storage: StorageKind) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join(DATABASES_DIR)).map_err(io(&root, "creating"))?;

        let lock_path = root.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(io(&lock_path, "opening"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Error::Storage(format!(
                    "data directory {} is already open in another process",
                    root.display()
                )));
            }
            Err(fs::TryLockError::Error(e)) => return Err(io(&lock_path, "locking")(e)),
        }

        let catalog_path = root.join(CATALOG_FILE);
        let catalog = match fs::read_to_string(&catalog_path) {
            Ok(text) => {
                let (default, list) = parse_catalog(&text, &catalog_path)?;
                if list.iter().any(|(name, _)| name == SYSTEM_DATABASE) {
                    return Err(Error::Storage(format!(
                        "{} lists a database named `{SYSTEM_DATABASE}`, a name now reserved for the system \
                         database; with the earlier graphbuff version, back it up, restore it under another \
                         name and drop it",
                        catalog_path.display()
                    )));
                }
                let mut dbs = BTreeMap::new();
                for (name, storage) in list {
                    let db = open_db(Some(&root), &name, &storage, false)
                        .map_err(|e| Error::Storage(format!("opening database `{name}`: {e}")))?;
                    dbs.insert(name, Entry { storage, db });
                }
                Catalog { default, dbs }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let db = open_db(Some(&root), DEFAULT_DATABASE, &default_storage, false)?;
                let mut dbs = BTreeMap::new();
                dbs.insert(
                    DEFAULT_DATABASE.to_string(),
                    Entry {
                        storage: default_storage.clone(),
                        db,
                    },
                );
                Catalog {
                    default: DEFAULT_DATABASE.to_string(),
                    dbs,
                }
            }
            Err(e) => return Err(io(&catalog_path, "reading")(e)),
        };
        let system = system_on_disk(&root)?;

        let mgr = DatabaseManager {
            root: Some(root),
            default_storage,
            catalog: RwLock::new(catalog),
            import_dir: RwLock::new(None),
            procedures: RwLock::new(crate::Procedures::builtin()),
            kernels: RwLock::new(None),
            system,
            _lock: Some(lock),
        };
        mgr.save(&mgr.catalog.read().unwrap_or_else(|e| e.into_inner()))?;
        Ok(mgr)
    }

    /// The data directory, or `None` for an in-memory manager.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Starts a session whose current database is the default database.
    /// Lets `LOAD CSV` read files in `dir` in every database (see
    /// [`Database::set_import_dir`]); `None` (the default) disables it.
    pub fn set_import_dir(&self, dir: Option<PathBuf>) {
        let cat = self.catalog.read().unwrap_or_else(|e| e.into_inner());
        *self.import_dir.write().unwrap_or_else(|e| e.into_inner()) = dir.clone();
        for entry in cat.dbs.values() {
            entry.db.set_import_dir(dir.clone());
        }
    }

    /// The directory `LOAD CSV` reads from, if it is enabled.
    pub fn import_dir(&self) -> Option<PathBuf> {
        self.import_dir.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Adds a procedure that `CALL` can run in every database (see
    /// [`crate::Database::register_procedure`]).
    pub fn register_procedure<F>(&self, signature: crate::ProcedureSignature, body: F) -> Result<()>
    where
        F: Fn(&crate::ProcedureContext<'_>, &[Value]) -> Result<crate::ProcedureRows> + Send + Sync + 'static,
    {
        let cat = self.catalog.read().unwrap_or_else(|e| e.into_inner());
        let mut guard = self.procedures.write().unwrap_or_else(|e| e.into_inner());
        let mut next = crate::Procedures::clone(&guard);
        next.register(signature, body)?;
        let next = Arc::new(next);
        *guard = next.clone();
        for entry in cat.dbs.values() {
            entry.db.set_procedures(next.clone());
        }
        Ok(())
    }

    /// Sets the kernel backend of every database, now and as they are created or opened
    /// (see [`Database::set_kernels`]; [`crate::gpu_kernels`] gives the GPU's).
    pub fn set_kernels(&self, kernels: Arc<dyn crate::Kernels>) {
        let cat = self.catalog.read().unwrap_or_else(|e| e.into_inner());
        *self.kernels.write().unwrap_or_else(|e| e.into_inner()) = Some(kernels.clone());
        for entry in cat.dbs.values() {
            entry.db.set_kernels(kernels.clone());
        }
    }

    fn apply_kernels(&self, db: &Database<BoxedStore>) {
        if let Some(k) = self.kernels.read().unwrap_or_else(|e| e.into_inner()).clone() {
            db.set_kernels(k);
        }
    }

    /// The procedures `CALL` can run.
    pub fn procedures(&self) -> Arc<crate::Procedures> {
        self.procedures.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn session(&self) -> Session<'_> {
        Session {
            mgr: self,
            current: self.default_database(),
        }
    }

    pub fn default_database(&self) -> String {
        self.read().default.clone()
    }

    /// The system database (user accounts and roles), or `None` when the engine is built
    /// without the `system-db` feature. Only the server's account code should change it.
    pub fn system_database(&self) -> Option<Arc<Database<BoxedStore>>> {
        self.system.clone()
    }

    /// The database called `name` (case-insensitive). The system database is not
    /// returned (see [`system_database`](Self::system_database)).
    pub fn database(&self, name: &str) -> Result<Arc<Database<BoxedStore>>> {
        let key = name.to_ascii_lowercase();
        if key == SYSTEM_DATABASE {
            return Err(system_not_queryable());
        }
        self.read()
            .dbs
            .get(&key)
            .map(|e| Arc::clone(&e.db))
            .ok_or_else(|| not_found(name))
    }

    pub fn databases(&self) -> Vec<DatabaseInfo> {
        let cat = self.read();
        cat.dbs
            .iter()
            .map(|(name, e)| DatabaseInfo {
                name: name.clone(),
                storage: e.storage.clone(),
                default: *name == cat.default,
                nodes: e.db.node_count(),
                relationships: e.db.relationship_count(),
            })
            .collect()
    }

    /// Checkpoints every durable database (see [`Database::checkpoint`]), for example
    /// before the process exits, so the next start has no log to replay. Each waits for
    /// the transaction writing to it, if any. Returns each durable database's name and
    /// outcome (the system database's too); in-memory databases are skipped.
    pub fn checkpoint_all(&self) -> Vec<(String, Result<()>)> {
        let mut dbs: Vec<(String, Arc<Database<BoxedStore>>)> = self
            .read()
            .dbs
            .iter()
            .filter(|(_, e)| !matches!(e.storage, StorageKind::InMemory))
            .map(|(name, e)| (name.clone(), Arc::clone(&e.db)))
            .collect();
        if let (Some(_), Some(system)) = (&self.root, &self.system) {
            dbs.push((SYSTEM_DATABASE.to_string(), Arc::clone(system)));
        }
        dbs.into_iter().map(|(name, db)| (name, db.checkpoint())).collect()
    }

    /// Creates database `name` with persistent storage by bulk-importing CSV files
    /// ([`crate::import_csv`]), writing store records directly instead of running Cypher.
    /// `storage: None` uses the manager's default storage if it is persistent, else
    /// persistent defaults. An existing database is replaced only when it is empty or
    /// `overwrite` is set; it stays untouched if the import fails.
    pub fn import_database(
        &self,
        name: &str,
        storage: Option<StorageKind>,
        opts: &crate::ImportOptions,
        overwrite: bool,
    ) -> Result<crate::ImportSummary> {
        let name = normalize_name(name)?;
        check_not_system(&name, "import into")?;
        let root = self.root().ok_or_else(|| {
            Error::Unsupported("importing needs a data directory; open the manager with DatabaseManager::open".into())
        })?;
        let storage = match storage {
            Some(s @ StorageKind::Persistent { .. }) => s,
            Some(other) => {
                return Err(Error::Unsupported(format!(
                    "imports create persistent databases, not {}",
                    other.name()
                )));
            }
            None => match &self.default_storage {
                s @ StorageKind::Persistent { .. } => s.clone(),
                _ => StorageKind::persistent(),
            },
        };
        let StorageKind::Persistent { sync, .. } = &storage else {
            unreachable!("checked above")
        };
        self.check_replaceable(&self.read(), &name, overwrite)?;

        let tmp = self.staging_dir(root, "import", &name)?;
        let build = || -> Result<crate::ImportSummary> {
            let mut loader =
                crate::PersistentGraph::bulk_loader(PersistentOptions::new(&tmp).sync(*sync).cache_bytes(opts.cache_bytes))?;
            let summary = crate::import_csv(&mut loader, opts)?;
            loader.finish()?;
            Ok(summary)
        };
        let summary = match build() {
            Ok(s) => s,
            Err(e) => {
                let _ = fs::remove_dir_all(&tmp);
                return Err(e);
            }
        };
        self.install(root, &name, &tmp, storage, overwrite)?;
        Ok(summary)
    }

    /// Writes a backup of database `name` into `dest`, a new or empty directory, while
    /// queries keep running: the backup holds the last commit before it started
    /// ([`Database::backup`]). Persistent databases back up as persistent stores, the
    /// others as hybrid stores; [`restore_database`](Self::restore_database) reads both.
    /// The system database can be backed up too, but not restored.
    pub fn backup_database(&self, name: &str, dest: &Path) -> Result<crate::BackupInfo> {
        self.database_or_system(name)?.backup(dest)
    }

    /// Checks the files of persistent database `name` for consistency, while queries keep
    /// running ([`Database::check`]).
    pub fn check_database(&self, name: &str) -> Result<crate::CheckReport> {
        self.database_or_system(name)?.check()
    }

    /// Like [`database`](Self::database), but also gives the system database.
    fn database_or_system(&self, name: &str) -> Result<Arc<Database<BoxedStore>>> {
        match &self.system {
            Some(system) if name.eq_ignore_ascii_case(SYSTEM_DATABASE) => Ok(Arc::clone(system)),
            _ => self.database(name),
        }
    }

    /// Creates database `name` from the backup in `from` (written by
    /// [`backup_database`](Self::backup_database) or the server's backup endpoint). The backup is
    /// copied, so it stays usable. A persistent backup is checked for consistency first.
    /// The database gets the manager's default storage settings if they are of the
    /// backup's kind, else that kind's defaults. An existing database is replaced only
    /// when it is empty or `overwrite` is set; it stays untouched if the restore fails.
    pub fn restore_database(&self, name: &str, from: &Path, overwrite: bool) -> Result<DatabaseInfo> {
        let name = normalize_name(name)?;
        check_not_system(&name, "restore into")?;
        let root = self.root().ok_or_else(|| {
            Error::Unsupported("restoring needs a data directory; open the manager with DatabaseManager::open".into())
        })?;
        if from.join(crate::BACKUP_MARKER).exists() {
            return Err(Error::Runtime(format!(
                "{} holds a backup that did not finish; it cannot be restored",
                from.display()
            )));
        }
        let storage = if from.join("meta").is_file() {
            match &self.default_storage {
                s @ StorageKind::Persistent { .. } => s.clone(),
                _ => StorageKind::persistent(),
            }
        } else if from.join("graph.snapshot").is_file() {
            match &self.default_storage {
                s @ StorageKind::Hybrid { .. } => s.clone(),
                _ => StorageKind::hybrid(),
            }
        } else {
            return Err(Error::Runtime(format!(
                "{} is not a graphbuff backup (it has neither `meta` nor `graph.snapshot`)",
                from.display()
            )));
        };
        self.check_replaceable(&self.read(), &name, overwrite)?;

        let tmp = self.staging_dir(root, "restore", &name)?;
        let stage = || -> Result<()> {
            fs::create_dir_all(&tmp).map_err(io(&tmp, "creating"))?;
            for entry in fs::read_dir(from).map_err(io(from, "reading"))? {
                let entry = entry.map_err(io(from, "reading"))?;
                let file_name = entry.file_name();
                let is_file = entry.file_type().map_err(io(&entry.path(), "reading"))?.is_file();
                if !is_file || file_name == LOCK_FILE {
                    continue;
                }
                let dst = tmp.join(&file_name);
                fs::copy(entry.path(), &dst).map_err(io(&entry.path(), "copying"))?;
                // Windows syncs only handles opened for writing.
                OpenOptions::new()
                    .write(true)
                    .open(&dst)
                    .and_then(|f| f.sync_all())
                    .map_err(io(&dst, "syncing"))?;
            }
            // Opening recovers and validates the copy; a persistent one is also checked.
            let db = open_dir(&tmp, &storage)?;
            if matches!(storage, StorageKind::Persistent { .. }) {
                let report = db.check()?;
                if !report.is_consistent() {
                    return Err(Error::Storage(format!(
                        "the backup in {} is not consistent:\n{report}",
                        from.display()
                    )));
                }
            }
            Ok(())
        };
        if let Err(e) = stage() {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
        self.install(root, &name, &tmp, storage, overwrite)?;
        let cat = self.read();
        let entry = &cat.dbs[&name];
        Ok(DatabaseInfo {
            name: name.clone(),
            storage: entry.storage.clone(),
            default: cat.default == name,
            nodes: entry.db.node_count(),
            relationships: entry.db.relationship_count(),
        })
    }

    /// Fails if database `name` exists and is not empty, unless `overwrite`.
    fn check_replaceable(&self, cat: &Catalog, name: &str, overwrite: bool) -> Result<()> {
        match cat.dbs.get(name) {
            Some(e) if !overwrite && (e.db.node_count() > 0 || e.db.relationship_count() > 0) => {
                Err(Error::Constraint(format!(
                    "database `{name}` already exists and is not empty; use overwrite to replace it"
                )))
            }
            _ => Ok(()),
        }
    }

    /// An empty directory beside the databases to build database `name` in (`.` cannot
    /// start a database name).
    fn staging_dir(&self, root: &Path, what: &str, name: &str) -> Result<PathBuf> {
        let tmp = root.join(DATABASES_DIR).join(format!(".{what}-{name}"));
        if tmp.exists() {
            fs::remove_dir_all(&tmp).map_err(io(&tmp, "removing stale"))?;
        }
        Ok(tmp)
    }

    /// Makes the store built in `tmp` database `name`, replacing an existing one (see
    /// [`check_replaceable`](Self::check_replaceable)). Removes `tmp` on failure.
    fn install(&self, root: &Path, name: &str, tmp: &Path, storage: StorageKind, overwrite: bool) -> Result<()> {
        let mut cat = self.write();
        let fail = |e: Error| {
            let _ = fs::remove_dir_all(tmp);
            e
        };
        self.check_replaceable(&cat, name, overwrite).map_err(fail)?;
        let dir = root.join(DATABASES_DIR).join(name);
        if let Some(entry) = cat.dbs.remove(name) {
            match Arc::try_unwrap(entry.db) {
                Ok(db) => drop(db), // closes its files and releases its lock
                Err(db) => {
                    cat.dbs.insert(name.to_string(), Entry { storage: entry.storage, db });
                    return Err(fail(Error::Runtime(format!(
                        "database `{name}` is in use by a running query; try again"
                    ))));
                }
            }
        }
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(io(&dir, "removing"))?;
        }
        fs::rename(tmp, &dir).map_err(io(&dir, "moving the new database to"))?;
        let db = open_db(Some(root), name, &storage, false)?;
        db.set_import_dir(self.import_dir());
        db.set_procedures(self.procedures());
        self.apply_kernels(&db);
        cat.dbs.insert(name.to_string(), Entry { storage, db });
        self.save(&cat)
    }

    /// Creates a database. `storage: None` uses the manager's default storage.
    /// Returns `false` only for [`ExistingDatabase::Keep`] when the database exists.
    ///
    /// With [`ExistingDatabase::Replace`] an existing database and all its data are
    /// dropped first. If creating the new one then fails, the old one is gone.
    pub fn create_database(
        &self,
        name: &str,
        storage: Option<StorageKind>,
        existing: ExistingDatabase,
    ) -> Result<bool> {
        let name = normalize_name(name)?;
        check_not_system(&name, "create")?;
        let storage = storage.unwrap_or_else(|| self.default_storage.clone());
        let mut cat = self.write();
        if cat.dbs.contains_key(&name) {
            match existing {
                ExistingDatabase::Fail => {
                    return Err(Error::Constraint(format!(
                        "database `{name}` already exists"
                    )));
                }
                ExistingDatabase::Keep => return Ok(false),
                ExistingDatabase::Replace => self.remove(&mut cat, &name)?,
            }
        }
        let db = open_db(self.root(), &name, &storage, true)?;
        db.set_import_dir(self.import_dir());
        db.set_procedures(self.procedures());
        self.apply_kernels(&db);
        cat.dbs.insert(name.clone(), Entry { storage, db });
        if let Err(e) = self.save(&cat) {
            cat.dbs.remove(&name);
            self.remove_files(&name);
            return Err(e);
        }
        Ok(true)
    }

    /// Drops a database and deletes its data. Returns `false` if it did not exist and
    /// `if_exists` was set.
    pub fn drop_database(&self, name: &str, if_exists: bool) -> Result<bool> {
        let name = normalize_name(name)?;
        check_not_system(&name, "drop")?;
        let mut cat = self.write();
        if !cat.dbs.contains_key(&name) {
            return if if_exists {
                Ok(false)
            } else {
                Err(not_found(&name))
            };
        }
        if name == cat.default {
            return Err(Error::Constraint(format!(
                "cannot drop the default database `{name}`"
            )));
        }
        self.remove(&mut cat, &name)?;
        Ok(true)
    }

    /// Closes a database, removes it from the catalog and deletes its files.
    fn remove(&self, cat: &mut Catalog, name: &str) -> Result<()> {
        let entry = cat.dbs.remove(name).expect("caller checked");
        let db = match Arc::try_unwrap(entry.db) {
            Ok(db) => db,
            Err(db) => {
                cat.dbs.insert(
                    name.to_string(),
                    Entry {
                        storage: entry.storage,
                        db,
                    },
                );
                return Err(Error::Runtime(format!(
                    "database `{name}` is in use by a running query; try again"
                )));
            }
        };
        drop(db); // closes its files and releases its lock
        if let Err(e) = self.save(cat) {
            // Still listed on disk: bring it back so memory matches the catalog file.
            if let Ok(db) = open_db(self.root(), name, &entry.storage, false) {
                db.set_import_dir(self.import_dir());
                db.set_procedures(self.procedures());
        self.apply_kernels(&db);
                cat.dbs.insert(
                    name.to_string(),
                    Entry {
                        storage: entry.storage,
                        db,
                    },
                );
            }
            return Err(e);
        }
        self.remove_files(name);
        Ok(())
    }

    /// Best effort: a directory left behind is not in the catalog, and is wiped if a
    /// database with the same name is created later.
    fn remove_files(&self, name: &str) {
        if let Some(root) = &self.root {
            let _ = fs::remove_dir_all(root.join(DATABASES_DIR).join(name));
        }
    }

    fn save(&self, cat: &Catalog) -> Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let mut text = format!("{CATALOG_HEADER}\ndefault {}\n", cat.default);
        for (name, e) in &cat.dbs {
            text.push_str(&format!("database {name} {}", e.storage.name()));
            let sync_name = |sync: &SyncPolicy| match sync {
                SyncPolicy::EveryCommit => "commit",
                SyncPolicy::OsBuffered => "os",
            };
            match &e.storage {
                StorageKind::InMemory => {}
                StorageKind::Hybrid {
                    sync,
                    checkpoint_after_bytes,
                } => text.push_str(&format!(
                    " sync={} checkpoint_after_bytes={checkpoint_after_bytes}",
                    sync_name(sync)
                )),
                StorageKind::Persistent {
                    sync,
                    checkpoint_after_bytes,
                    cache_bytes,
                    max_transaction_bytes,
                } => {
                    text.push_str(&format!(
                        " sync={} checkpoint_after_bytes={checkpoint_after_bytes} cache_bytes={cache_bytes}",
                        sync_name(sync)
                    ));
                    // Written only when changed, so that older versions still read the catalog.
                    if *max_transaction_bytes != DEFAULT_MAX_TRANSACTION_BYTES {
                        text.push_str(&format!(" max_transaction_bytes={max_transaction_bytes}"));
                    }
                }
            }
            text.push('\n');
        }
        let tmp = root.join(CATALOG_TMP);
        let mut f = File::create(&tmp).map_err(io(&tmp, "creating"))?;
        f.write_all(text.as_bytes())
            .and_then(|_| f.sync_all())
            .map_err(io(&tmp, "writing"))?;
        drop(f);
        let dst = root.join(CATALOG_FILE);
        fs::rename(&tmp, &dst).map_err(io(&dst, "replacing"))
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Catalog> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Catalog> {
        self.catalog.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Turns `OPTIONS {…}` into a storage kind.
    fn storage_from_options(
        &self,
        options: &Expr,
        params: &BTreeMap<String, Value>,
    ) -> Result<StorageKind> {
        let lookup = |p: &str| {
            params
                .get(p)
                .cloned()
                .ok_or_else(|| Error::Semantic(format!("expected parameter ${p} to be provided")))
        };
        let map: BTreeMap<String, Value> = match options {
            Expr::Map(items) => items
                .iter()
                .map(|(k, e)| {
                    let v = match e {
                        Expr::Literal(v) => v.clone(),
                        Expr::Param(p) => lookup(p)?,
                        _ => {
                            return Err(Error::Semantic(
                                "OPTIONS values must be literals or parameters".into(),
                            ));
                        }
                    };
                    Ok((k.clone(), v))
                })
                .collect::<Result<_>>()?,
            Expr::Param(p) => match lookup(p)? {
                Value::Map(m) => m,
                other => {
                    return Err(Error::Type(format!(
                        "OPTIONS must be a map, got {}",
                        other.type_name()
                    )));
                }
            },
            _ => return Err(Error::Semantic("OPTIONS must be a map".into())),
        };

        let mut storage = match map.get("storage") {
            None => self.default_storage.clone(),
            Some(Value::String(s)) => match s.to_ascii_lowercase().as_str() {
                "memory" | "in-memory" | "inmemory" => StorageKind::InMemory,
                "hybrid" => match &self.default_storage {
                    h @ StorageKind::Hybrid { .. } => h.clone(),
                    _ => StorageKind::hybrid(),
                },
                "persistent" => match &self.default_storage {
                    p @ StorageKind::Persistent { .. } => p.clone(),
                    _ => StorageKind::persistent(),
                },
                _ => {
                    return Err(Error::Semantic(format!(
                        "unknown storage `{s}`; expected 'memory', 'hybrid' or 'persistent'"
                    )));
                }
            },
            Some(v) => {
                return Err(Error::Type(format!(
                    "option `storage` must be a string, got {}",
                    v.type_name()
                )));
            }
        };
        for (key, value) in &map {
            match key.as_str() {
                "storage" => {}
                "sync" | "checkpointAfterBytes" | "cacheBytes" | "maxTransactionBytes" => {
                    let persistent_only = matches!(key.as_str(), "cacheBytes" | "maxTransactionBytes");
                    let (sync, checkpoint_after_bytes, cache_bytes, max_transaction_bytes) = match &mut storage {
                        StorageKind::Hybrid {
                            sync,
                            checkpoint_after_bytes,
                        } if !persistent_only => (sync, checkpoint_after_bytes, None, None),
                        StorageKind::Persistent {
                            sync,
                            checkpoint_after_bytes,
                            cache_bytes,
                            max_transaction_bytes,
                        } => (sync, checkpoint_after_bytes, Some(cache_bytes), Some(max_transaction_bytes)),
                        _ if persistent_only => {
                            return Err(Error::Semantic(format!(
                                "option `{key}` only applies to persistent storage"
                            )));
                        }
                        _ => {
                            return Err(Error::Semantic(format!(
                                "option `{key}` only applies to hybrid and persistent storage"
                            )));
                        }
                    };
                    match (key.as_str(), value) {
                        ("sync", Value::String(s)) if s.eq_ignore_ascii_case("commit") => {
                            *sync = SyncPolicy::EveryCommit;
                        }
                        ("sync", Value::String(s)) if s.eq_ignore_ascii_case("os") => {
                            *sync = SyncPolicy::OsBuffered;
                        }
                        ("checkpointAfterBytes", Value::Int(n)) if *n >= 0 => {
                            *checkpoint_after_bytes = *n as u64;
                        }
                        ("cacheBytes", Value::Int(n)) if *n >= 0 => {
                            if let Some(c) = cache_bytes {
                                *c = *n as u64;
                            }
                        }
                        ("maxTransactionBytes", Value::Int(n)) if *n >= 0 => {
                            if let Some(m) = max_transaction_bytes {
                                *m = *n as u64;
                            }
                        }
                        ("sync", _) => {
                            return Err(Error::Semantic(
                                "option `sync` must be 'commit' or 'os'".into(),
                            ));
                        }
                        _ => {
                            return Err(Error::Semantic(format!(
                                "option `{key}` must be a non-negative integer"
                            )));
                        }
                    }
                }
                _ => {
                    return Err(Error::Semantic(format!(
                        "unknown database option `{key}`; supported: storage, sync, checkpointAfterBytes, cacheBytes, maxTransactionBytes"
                    )));
                }
            }
        }
        Ok(storage)
    }
}

/// A client's connection to a [`DatabaseManager`]: runs statements against the current
/// database and handles `USE` and the database commands.
pub struct Session<'m> {
    mgr: &'m DatabaseManager,
    current: String,
}

impl<'m> Session<'m> {
    pub fn manager(&self) -> &'m DatabaseManager {
        self.mgr
    }

    pub fn current_database(&self) -> &str {
        &self.current
    }

    /// Makes `name` the current database. Fails if it does not exist.
    pub fn use_database(&mut self, name: &str) -> Result<()> {
        let name = normalize_name(name)?;
        self.mgr.database(&name)?;
        self.current = name;
        Ok(())
    }

    /// The current database.
    pub fn database(&self) -> Result<Arc<Database<BoxedStore>>> {
        self.mgr.database(&self.current)
    }

    pub fn execute(&mut self, statement: &str) -> Result<QueryResult> {
        self.execute_with_params(statement, &BTreeMap::new())
    }

    /// Runs a query, `USE`, or a database command.
    pub fn execute_with_params(
        &mut self,
        statement: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<QueryResult> {
        self.execute_with_options(statement, params, &crate::QueryOptions::default())
    }

    /// Runs a query, `USE`, or a database command; a query runs with a time limit or a
    /// [`CancelHandle`](crate::CancelHandle) (see [`Database::execute_with_options`]).
    /// Database commands are not limited.
    pub fn execute_with_options(
        &mut self,
        statement: &str,
        params: &BTreeMap<String, Value>,
        options: &crate::QueryOptions,
    ) -> Result<QueryResult> {
        match graphbuff_cypher::parse_statement(statement)? {
            Statement::Query { use_db, query } => {
                let db = match use_db {
                    Some(n) => self.mgr.database(&resolve(&n, params)?)?,
                    None => self.database()?,
                };
                db.execute_query_with_options(&query, params, options)
            }
            Statement::Use(n) => {
                self.use_database(&resolve(&n, params)?)?;
                Ok(empty(QueryStats::default()))
            }
            Statement::Database(cmd) => self.run_command(cmd, params),
            Statement::User(cmd) => Err(Error::Unsupported(format!(
                "{} needs graphbuff-server with user accounts",
                cmd.name()
            ))),
            Statement::Transaction(cmd) => Err(Error::Unsupported(format!("{} needs graphbuff-server", cmd.name()))),
        }
    }

    fn run_command(
        &mut self,
        cmd: DatabaseCommand,
        params: &BTreeMap<String, Value>,
    ) -> Result<QueryResult> {
        match cmd {
            DatabaseCommand::Create {
                name,
                existing,
                options,
            } => {
                let name = normalize_name(&resolve(&name, params)?)?;
                let storage = options
                    .map(|o| self.mgr.storage_from_options(&o, params))
                    .transpose()?;
                let created = self.mgr.create_database(&name, storage, existing)?;
                Ok(empty(QueryStats {
                    system_updates: created as usize,
                    ..Default::default()
                }))
            }
            DatabaseCommand::Drop { name, if_exists } => {
                let name = normalize_name(&resolve(&name, params)?)?;
                let dropped = self.mgr.drop_database(&name, if_exists)?;
                if dropped && self.current == name {
                    self.current = self.mgr.default_database();
                }
                Ok(empty(QueryStats {
                    system_updates: dropped as usize,
                    ..Default::default()
                }))
            }
            DatabaseCommand::Show(target) => {
                let wanted = match &target {
                    ShowDatabases::Named(n) => Some(resolve(n, params)?.to_ascii_lowercase()),
                    _ => None,
                };
                let rows = self
                    .mgr
                    .databases()
                    .into_iter()
                    .filter(|d| match &target {
                        ShowDatabases::All => true,
                        ShowDatabases::Default => d.default,
                        ShowDatabases::Named(_) => Some(&d.name) == wanted.as_ref(),
                    })
                    .map(|d| {
                        vec![
                            ResultValue::String(d.name.clone()),
                            ResultValue::String(d.storage.name().into()),
                            ResultValue::Bool(d.default),
                            ResultValue::Bool(d.name == self.current),
                            ResultValue::Int(d.nodes as i64),
                            ResultValue::Int(d.relationships as i64),
                        ]
                    })
                    .collect();
                Ok(QueryResult {
                    columns: [
                        "name",
                        "storage",
                        "default",
                        "current",
                        "nodes",
                        "relationships",
                    ]
                    .map(String::from)
                    .to_vec(),
                    rows,
                    stats: QueryStats::default(),
                    plan: None,
                })
            }
        }
    }
}

fn resolve(name: &DbName, params: &BTreeMap<String, Value>) -> Result<String> {
    match name {
        DbName::Name(n) => Ok(n.clone()),
        DbName::Param(p) => match params.get(p) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(v) => Err(Error::Type(format!(
                "database name parameter ${p} must be a string, got {}",
                v.type_name()
            ))),
            None => Err(Error::Semantic(format!(
                "expected parameter ${p} to be provided"
            ))),
        },
    }
}

fn empty(stats: QueryStats) -> QueryResult {
    QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        stats,
        plan: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            normalize_name("Movies.2024-v_1").unwrap(),
            "movies.2024-v_1"
        );
        for bad in [
            "",
            "1abc",
            "a b",
            "a..b",
            "a.",
            "con",
            "Com1.x",
            "x/../y",
            &"a".repeat(64),
        ] {
            assert!(normalize_name(bad).is_err(), "{bad:?} accepted");
        }
    }
}
