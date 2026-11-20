//! Several named databases in one place: a catalog, database commands and sessions.
//!
//! A [`DatabaseManager`] owns a set of databases, each with its own [`StorageKind`].
//! Clients talk to it through a [`Session`], which has a current database (switched
//! with `USE name`) and also runs the database commands:
//!
//! ```text
//! CREATE [OR REPLACE] DATABASE name [IF NOT EXISTS] [OPTIONS {storage: 'hybrid', …}]
//! DROP DATABASE name [IF EXISTS] [DUMP DATA | DESTROY DATA]
//! ALTER DATABASE name [IF EXISTS] SET ACCESS READ ONLY | READ WRITE
//! SHOW DATABASES | SHOW DATABASE name | SHOW DEFAULT DATABASE
//! CREATE [OR REPLACE] ALIAS name [IF NOT EXISTS] FOR DATABASE target
//! ALTER ALIAS name [IF EXISTS] SET DATABASE TARGET target
//! DROP ALIAS name [IF EXISTS] FOR DATABASE
//! SHOW ALIASES [name] FOR DATABASES
//! USE name                      -- switch the session's current database
//! USE name MATCH … RETURN …     -- run one query against another database
//! ```
//!
//! An alias is another name for a database: everywhere a database name goes (`USE`,
//! the driver's database), the alias reaches its target. `DUMP DATA` writes a backup of
//! the database to `dumps/<name>-<unix seconds>/` in the data directory before it is
//! dropped.
//!
//! A manager opened on a directory keeps a `catalog` file there (the list of databases,
//! their storage settings and access, and the aliases) and stores each hybrid database under
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
const DUMPS_DIR: &str = "dumps";
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
    /// Set by `ALTER DATABASE … SET ACCESS READ ONLY`.
    pub read_only: bool,
    /// The aliases that name it, sorted.
    pub aliases: Vec<String>,
}

type Db = Arc<Database<BoxedStore>>;

struct Entry {
    storage: StorageKind,
    db: Db,
}

struct Catalog {
    default: String,
    dbs: BTreeMap<String, Entry>,
    /// Alias → database.
    aliases: BTreeMap<String, String>,
}

impl Catalog {
    /// The database `name` (already normalized) names: itself, or an alias's target.
    fn resolve<'a>(&'a self, name: &'a str) -> &'a str {
        self.aliases.get(name).map_or(name, String::as_str)
    }

    /// Fails if `name` is taken by a database or an alias.
    fn check_free(&self, name: &str) -> Result<()> {
        if self.dbs.contains_key(name) {
            return Err(Error::Constraint(format!("a database named `{name}` already exists")));
        }
        if let Some(target) = self.aliases.get(name) {
            return Err(Error::Constraint(format!("`{name}` is already an alias for database `{target}`")));
        }
        Ok(())
    }
}

/// What a catalog file lists.
struct CatalogFile {
    default: String,
    dbs: Vec<(String, StorageKind)>,
    read_only: Vec<String>,
    aliases: BTreeMap<String, String>,
}

/// A set of named databases, each with its own storage. See the [module docs](self).
pub struct DatabaseManager {
    root: Option<PathBuf>,
    default_storage: StorageKind,
    catalog: RwLock<Catalog>,
    /// Where `LOAD CSV` reads files, for every database.
    import_dir: RwLock<Option<PathBuf>>,
    csv_urls: RwLock<Option<crate::CsvUrls>>,
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

fn parse_catalog(text: &str, path: &Path) -> Result<CatalogFile> {
    let bad =
        |line: usize, msg: &str| Error::Storage(format!("{} line {line}: {msg}", path.display()));
    let mut lines = text.lines().enumerate().map(|(i, l)| (i + 1, l.trim()));
    match lines.next() {
        Some((_, CATALOG_HEADER)) => {}
        _ => return Err(bad(1, "not a graphbuff catalog")),
    }
    let mut default = None;
    let mut dbs = Vec::new();
    let mut read_only = Vec::new();
    let mut aliases = BTreeMap::new();
    let name_at = |n: usize, name: &str| normalize_name(name).map_err(|_| bad(n, "invalid name"));
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
            ["access", name, "read-only"] => {
                let name = name_at(n, name)?;
                if !dbs.iter().any(|(d, _)| *d == name) {
                    return Err(bad(n, "access of a database not listed before it"));
                }
                read_only.push(name);
            }
            ["alias", alias, target] => {
                let (alias, target) = (name_at(n, alias)?, name_at(n, target)?);
                if !dbs.iter().any(|(d, _)| *d == target) {
                    return Err(bad(n, "alias for a database not listed before it"));
                }
                if dbs.iter().any(|(d, _)| *d == alias) || aliases.insert(alias, target).is_some() {
                    return Err(bad(n, "duplicate name"));
                }
            }
            _ => return Err(bad(n, "unrecognized line")),
        }
    }
    let default = default.ok_or_else(|| bad(0, "no default database"))?;
    if !dbs.iter().any(|(d, _)| *d == default) {
        return Err(bad(0, "the default database is not in the catalog"));
    }
    Ok(CatalogFile { default, dbs, read_only, aliases })
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
                aliases: BTreeMap::new(),
            }),
            import_dir: RwLock::new(None),
            csv_urls: RwLock::new(None),
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
                let file = parse_catalog(&text, &catalog_path)?;
                if file.dbs.iter().any(|(name, _)| name == SYSTEM_DATABASE) || file.aliases.contains_key(SYSTEM_DATABASE) {
                    return Err(Error::Storage(format!(
                        "{} lists a database named `{SYSTEM_DATABASE}`, a name now reserved for the system \
                         database; with the earlier graphbuff version, back it up, restore it under another \
                         name and drop it",
                        catalog_path.display()
                    )));
                }
                let mut dbs = BTreeMap::new();
                for (name, storage) in file.dbs {
                    let db = open_db(Some(&root), &name, &storage, false)
                        .map_err(|e| Error::Storage(format!("opening database `{name}`: {e}")))?;
                    db.set_read_only(file.read_only.contains(&name));
                    dbs.insert(name, Entry { storage, db });
                }
                Catalog { default: file.default, dbs, aliases: file.aliases }
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
                    aliases: BTreeMap::new(),
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
            csv_urls: RwLock::new(None),
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

    /// Lets `LOAD CSV` read web URLs in every database (see [`crate::CsvUrls`]); `None`
    /// (the default): only files.
    pub fn set_csv_urls(&self, urls: Option<crate::CsvUrls>) {
        let cat = self.catalog.read().unwrap_or_else(|e| e.into_inner());
        *self.csv_urls.write().unwrap_or_else(|e| e.into_inner()) = urls.clone();
        for entry in cat.dbs.values() {
            entry.db.set_csv_urls(urls.clone());
        }
    }

    /// The web URLs `LOAD CSV` may read, if any.
    pub fn csv_urls(&self) -> Option<crate::CsvUrls> {
        self.csv_urls.read().unwrap_or_else(|e| e.into_inner()).clone()
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

    /// The database called `name` (case-insensitive), or named by alias `name`. The
    /// system database is not returned (see [`system_database`](Self::system_database)).
    pub fn database(&self, name: &str) -> Result<Arc<Database<BoxedStore>>> {
        let key = name.to_ascii_lowercase();
        if key == SYSTEM_DATABASE {
            return Err(system_not_queryable());
        }
        let cat = self.read();
        cat.dbs
            .get(cat.resolve(&key))
            .map(|e| Arc::clone(&e.db))
            .ok_or_else(|| not_found(name))
    }

    /// The name of the database `name` names: itself (lowercased), or an alias's target.
    pub fn resolve_name(&self, name: &str) -> String {
        let key = name.to_ascii_lowercase();
        self.read().resolve(&key).to_string()
    }

    /// Every alias and the database it names, by alias.
    pub fn aliases(&self) -> Vec<(String, String)> {
        self.read().aliases.iter().map(|(a, t)| (a.clone(), t.clone())).collect()
    }

    /// The bytes of the files database `name` keeps on disk; `None` for an in-memory
    /// database, an unknown one, or a manager without a directory.
    pub fn disk_bytes(&self, name: &str) -> Option<u64> {
        let key = name.to_ascii_lowercase();
        let cat = self.read();
        let entry = cat.dbs.get(&key)?;
        if matches!(entry.storage, StorageKind::InMemory) {
            return None;
        }
        fn size(p: &Path) -> u64 {
            match fs::symlink_metadata(p) {
                Ok(m) if m.is_dir() => fs::read_dir(p)
                    .map(|entries| entries.flatten().map(|e| size(&e.path())).sum())
                    .unwrap_or(0),
                Ok(m) => m.len(),
                Err(_) => 0,
            }
        }
        let dir = self.root.as_ref()?.join(DATABASES_DIR).join(&key);
        Some(size(&dir))
    }

    pub fn databases(&self) -> Vec<DatabaseInfo> {
        let cat = self.read();
        cat.dbs
            .iter()
            .map(|(name, e)| info(&cat, name, e))
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
        Ok(info(&cat, &name, &cat.dbs[&name]))
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
        let mut read_only = false;
        if let Some(entry) = cat.dbs.remove(name) {
            read_only = entry.db.is_read_only();
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
        db.set_read_only(read_only);
        db.set_import_dir(self.import_dir());
        db.set_csv_urls(self.csv_urls());
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
        if let Some(target) = cat.aliases.get(&name) {
            return Err(Error::Constraint(format!("`{name}` is an alias for database `{target}`")));
        }
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
        db.set_csv_urls(self.csv_urls());
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
    /// `if_exists` was set. Fails while aliases name it.
    pub fn drop_database(&self, name: &str, if_exists: bool) -> Result<bool> {
        self.drop_database_with(name, if_exists, false)
    }

    /// [`drop_database`](Self::drop_database), first writing a backup of it to
    /// `dumps/<name>-<unix seconds>/` in the data directory when `dump` is set
    /// (`DROP DATABASE … DUMP DATA`).
    pub fn drop_database_with(&self, name: &str, if_exists: bool, dump: bool) -> Result<bool> {
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
        let aliases: Vec<&str> = cat.aliases.iter().filter(|(_, t)| **t == name).map(|(a, _)| a.as_str()).collect();
        if !aliases.is_empty() {
            return Err(Error::Constraint(format!(
                "database `{name}` has aliases ({}); drop them first",
                aliases.join(", ")
            )));
        }
        if dump {
            let Some(root) = &self.root else {
                return Err(Error::Unsupported("DUMP DATA needs a data directory".into()));
            };
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let dest = root.join(DUMPS_DIR).join(format!("{name}-{secs}"));
            cat.dbs[&name].db.backup(&dest)?;
        }
        self.remove(&mut cat, &name)?;
        Ok(true)
    }

    /// Sets database `name`'s access (`ALTER DATABASE … SET ACCESS`). Returns `false` if
    /// it does not exist and `if_exists` was set.
    pub fn alter_database(&self, name: &str, if_exists: bool, read_only: bool) -> Result<bool> {
        let name = normalize_name(name)?;
        check_not_system(&name, "alter")?;
        let cat = self.write();
        let Some(entry) = cat.dbs.get(&name) else {
            return if if_exists { Ok(false) } else { Err(not_found(&name)) };
        };
        let was = entry.db.is_read_only();
        entry.db.set_read_only(read_only);
        if let Err(e) = self.save(&cat) {
            entry.db.set_read_only(was);
            return Err(e);
        }
        Ok(true)
    }

    /// Makes `alias` another name for database `target`. Returns `false` only for
    /// [`ExistingDatabase::Keep`] when the alias exists; [`ExistingDatabase::Replace`]
    /// points an existing alias at `target`.
    pub fn create_alias(&self, alias: &str, target: &str, existing: ExistingDatabase) -> Result<bool> {
        let (alias, target) = (normalize_name(alias)?, normalize_name(target)?);
        check_not_system(&alias, "create an alias named")?;
        let mut cat = self.write();
        if cat.aliases.contains_key(&alias) {
            match existing {
                ExistingDatabase::Fail => return Err(Error::Constraint(format!("alias `{alias}` already exists"))),
                ExistingDatabase::Keep => return Ok(false),
                ExistingDatabase::Replace => {}
            }
        } else {
            cat.check_free(&alias)?;
        }
        self.set_alias(&mut cat, alias, Some(target))?;
        Ok(true)
    }

    /// Points `alias` at database `target`. Returns `false` if the alias does not exist
    /// and `if_exists` was set.
    pub fn alter_alias(&self, alias: &str, if_exists: bool, target: &str) -> Result<bool> {
        let (alias, target) = (normalize_name(alias)?, normalize_name(target)?);
        let mut cat = self.write();
        if !cat.aliases.contains_key(&alias) {
            return if if_exists { Ok(false) } else { Err(alias_not_found(&alias)) };
        }
        self.set_alias(&mut cat, alias, Some(target))?;
        Ok(true)
    }

    /// Removes `alias`. Returns `false` if it does not exist and `if_exists` was set.
    pub fn drop_alias(&self, alias: &str, if_exists: bool) -> Result<bool> {
        let alias = normalize_name(alias)?;
        let mut cat = self.write();
        if !cat.aliases.contains_key(&alias) {
            return if if_exists { Ok(false) } else { Err(alias_not_found(&alias)) };
        }
        self.set_alias(&mut cat, alias, None)?;
        Ok(true)
    }

    /// Sets (or with `None` removes) `alias` and saves the catalog, undoing the change if
    /// that fails. The target must be a database, not an alias.
    fn set_alias(&self, cat: &mut Catalog, alias: String, target: Option<String>) -> Result<()> {
        if let Some(t) = &target
            && !cat.dbs.contains_key(t)
        {
            return Err(if cat.aliases.contains_key(t) {
                Error::Constraint(format!("`{t}` is an alias; an alias must name a database"))
            } else {
                not_found(t)
            });
        }
        let old = match target {
            Some(t) => cat.aliases.insert(alias.clone(), t),
            None => cat.aliases.remove(&alias),
        };
        if let Err(e) = self.save(cat) {
            match old {
                Some(t) => cat.aliases.insert(alias, t),
                None => cat.aliases.remove(&alias),
            };
            return Err(e);
        }
        Ok(())
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
        db.set_csv_urls(self.csv_urls());
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
            if e.db.is_read_only() {
                text.push_str(&format!("access {name} read-only\n"));
            }
        }
        for (alias, target) in &cat.aliases {
            text.push_str(&format!("alias {alias} {target}\n"));
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

    /// Makes `name` (or the database alias `name` names) the current database. Fails if
    /// it does not exist.
    pub fn use_database(&mut self, name: &str) -> Result<()> {
        let name = normalize_name(name)?;
        self.mgr.database(&name)?;
        self.current = self.mgr.resolve_name(&name);
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
        self.run_statement(graphbuff_cypher::parse_statement(statement)?, params, options)
    }

    /// [`Session::execute_with_options`], handing the rows to `sink` as they are produced
    /// (see [`Database::execute_streaming`]); the result returned has no rows.
    pub fn execute_streaming(
        &mut self,
        statement: &str,
        params: &BTreeMap<String, Value>,
        options: &crate::QueryOptions,
        sink: &mut dyn crate::RowSink,
    ) -> Result<QueryResult> {
        match graphbuff_cypher::parse_statement(statement)? {
            Statement::Query { use_db, query } => {
                let db = match use_db {
                    Some(n) => self.mgr.database(&resolve(&n, params)?)?,
                    None => self.database()?,
                };
                db.execute_query_streaming(&query, params, options, sink)
            }
            other => self.run_statement(other, params, options)?.feed(sink),
        }
    }

    fn run_statement(
        &mut self,
        statement: Statement,
        params: &BTreeMap<String, Value>,
        options: &crate::QueryOptions,
    ) -> Result<QueryResult> {
        match statement {
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
            Statement::Settings(_) => Err(Error::Unsupported("SHOW SETTINGS needs graphbuff-server".into())),
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
            DatabaseCommand::Drop { name, if_exists, dump } => {
                let name = normalize_name(&resolve(&name, params)?)?;
                let dropped = self.mgr.drop_database_with(&name, if_exists, dump)?;
                if dropped && self.current == name {
                    self.current = self.mgr.default_database();
                }
                Ok(empty(QueryStats {
                    system_updates: dropped as usize,
                    ..Default::default()
                }))
            }
            DatabaseCommand::Alter { name, if_exists, read_only } => {
                let done = self.mgr.alter_database(&resolve(&name, params)?, if_exists, read_only)?;
                Ok(empty(QueryStats { system_updates: done as usize, ..Default::default() }))
            }
            DatabaseCommand::CreateAlias { alias, target, existing } => {
                let done = self.mgr.create_alias(&resolve(&alias, params)?, &resolve(&target, params)?, existing)?;
                Ok(empty(QueryStats { system_updates: done as usize, ..Default::default() }))
            }
            DatabaseCommand::AlterAlias { alias, if_exists, target } => {
                let done = self.mgr.alter_alias(&resolve(&alias, params)?, if_exists, &resolve(&target, params)?)?;
                Ok(empty(QueryStats { system_updates: done as usize, ..Default::default() }))
            }
            DatabaseCommand::DropAlias { alias, if_exists } => {
                let done = self.mgr.drop_alias(&resolve(&alias, params)?, if_exists)?;
                Ok(empty(QueryStats { system_updates: done as usize, ..Default::default() }))
            }
            DatabaseCommand::ShowAliases(name) => {
                let wanted = name.map(|n| resolve(&n, params)).transpose()?.map(|n| n.to_ascii_lowercase());
                let rows = self
                    .mgr
                    .aliases()
                    .into_iter()
                    .filter(|(a, _)| wanted.as_ref().is_none_or(|w| w == a))
                    .map(|(a, t)| vec![ResultValue::String(a), ResultValue::String(t)])
                    .collect();
                Ok(QueryResult {
                    columns: vec!["name".into(), "database".into()],
                    rows,
                    stats: QueryStats::default(),
                    plan: None,
                })
            }
            DatabaseCommand::Show(target) => {
                let wanted = match &target {
                    ShowDatabases::Named(n) => Some(self.mgr.resolve_name(&resolve(n, params)?)),
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
                            ResultValue::String(if d.read_only { "read-only" } else { "read-write" }.into()),
                            ResultValue::List(d.aliases.iter().cloned().map(ResultValue::String).collect()),
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
                        "access",
                        "aliases",
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

/// The [`DatabaseInfo`] of database `name`.
fn info(cat: &Catalog, name: &str, e: &Entry) -> DatabaseInfo {
    DatabaseInfo {
        name: name.to_string(),
        storage: e.storage.clone(),
        default: name == cat.default,
        nodes: e.db.node_count(),
        relationships: e.db.relationship_count(),
        read_only: e.db.is_read_only(),
        aliases: cat.aliases.iter().filter(|(_, t)| *t == name).map(|(a, _)| a.clone()).collect(),
    }
}

fn alias_not_found(alias: &str) -> Error {
    Error::Semantic(format!("alias `{alias}` does not exist"))
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
