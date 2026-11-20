//! # graphbuff
//!
//! A graph database engine queried with Cypher.
//!
//! ```
//! use graphbuff::Database;
//!
//! let db = Database::in_memory();
//! db.execute("CREATE (:Person {name: 'Ann'})-[:KNOWS]->(:Person {name: 'Bob'})").unwrap();
//! let result = db
//!     .execute("MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name AS from, b.name AS to")
//!     .unwrap();
//! assert_eq!(result.get(0, "to").and_then(|v| v.as_str()), Some("Bob"));
//! ```
//!
//! ## Architecture
//! * `graphbuff-cypher` parses query text into an AST,
//! * this crate executes the AST clause by clause against a [`GraphStore`],
//! * `graphbuff-core` defines the storage trait and the in-memory [`MemoryGraph`].
//!
//! Every query runs in its own transaction: if any part of a writing query fails,
//! all of its changes are rolled back. Read queries run on a snapshot of the last
//! commit, so they neither wait for a writing query nor see its changes before it
//! commits (see [`Database`]).

mod accel;
mod aggregate;
mod batched;
mod context;
mod csv;
mod dbms;
mod demo;
mod eval;
mod exec;
mod fulltext;
mod hnsw;
mod functions;
mod import;
mod index_cache;
mod matcher;
mod ops;
#[cfg(test)]
mod legacy;
mod typecheck;
mod plan;
mod procedures;
mod regex;
mod result;
mod stream;
mod transaction;
mod interrupt;
mod unicode;
mod vector;
pub mod json;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, RwLockWriteGuard, TryLockError};
use std::thread::ThreadId;

pub use import::{IdType, ImportFiles, ImportOptions, ImportSummary, import_csv, parse_index};
pub use demo::{DEMOS, DemoSummary, demo_script, load_demo};
pub use dbms::{DEFAULT_DATABASE, SYSTEM_DATABASE, DatabaseInfo, DatabaseManager, Session, StorageKind};
pub use graphbuff_core::{
    BackupInfo, BoxedStore, BulkLoader, CheckReport, Crs, Csr, CsrAdjacency, DEFAULT_MAX_TRANSACTION_BYTES, Direction, Error, GraphStore, HybridGraph, HybridOptions, IndexDef, IndexEntity, MemoryGraph,
    NodeId, Path, PersistentGraph, PersistentOptions, Point, RelId, Result, Scan, Snapshot, StorageMode,
    SyncPolicy, Temporal, Value, ValueRange, open_store, temporal, tz, tz::Tz,
};
pub use graphbuff_core::kernels::{
    self, Aggregates, Bits, CmpOp, Column, CpuKernels, ExpandBatch, Hop, Kernels, Mask, Num, label_mask,
};
pub use graphbuff_core::maintenance::BACKUP_MARKER;
pub use graphbuff_core::accounting;
pub use graphbuff_cypher as cypher;
pub use graphbuff_cypher::ast::{ExistingDatabase, QueryMode};
pub use procedures::{
    Procedure, ProcedureContext, ProcedureFn, ProcedureParam, ProcedureRows, ProcedureSignature, ProcedureType, Procedures,
};
pub use transaction::Transaction;
pub use accel::{KernelMode, KernelStats, kernel_stats};
#[cfg(feature = "gpu")]
pub use graphbuff_gpu::WgpuKernels;

/// The GPU kernel backend ([`WgpuKernels`] on the highest-performance adapter), for
/// [`Database::set_kernels`] or [`DatabaseManager::set_kernels`]. `Unsupported` when no
/// usable GPU is present, or graphbuff was built without the `gpu` feature.
pub fn gpu_kernels() -> Result<Arc<dyn Kernels>> {
    #[cfg(feature = "gpu")]
    {
        Ok(Arc::new(graphbuff_gpu::WgpuKernels::new()?))
    }
    #[cfg(not(feature = "gpu"))]
    {
        Err(Error::Unsupported("graphbuff was built without GPU support (the `gpu` feature)".into()))
    }
}
pub use interrupt::{CancelHandle, QueryOptions};
pub use result::{
    NodeData, PathData, PlanDescription, PlanLine, QueryResult, QueryStats, RelData, ResultValue,
};

use context::{Executor, StoreRef};
use interrupt::Interrupt;

/// A graph database: a [`GraphStore`] plus a Cypher query engine.
///
/// Queries that write run one at a time and are atomic (rolled back on error).
/// [`Database::begin`] opens a [`Transaction`] that groups several statements.
///
/// Read-only queries run concurrently with each other and with the writer: each runs on
/// a snapshot of the store as of the last commit ([`GraphStore::snapshot`]), so it never
/// waits for a writing query and sees either all or none of its changes. With a store
/// that cannot take snapshots, readers instead wait while a query writes.
///
/// The store type is either fixed at compile time (`Database<MemoryGraph>`, the default,
/// `Database<HybridGraph>` or `Database<PersistentGraph>`), or chosen at runtime with [`Database::open`], which
/// returns a `Database<BoxedStore>`.
pub struct Database<S: GraphStore = MemoryGraph> {
    store: RwLock<S>,
    /// The last committed state, which read queries run on; `None` if the store cannot
    /// take snapshots.
    committed: RwLock<Option<Snapshot>>,
    /// Where `LOAD CSV` reads files; `None` disables it.
    import_dir: RwLock<Option<PathBuf>>,
    /// The procedures `CALL` can run.
    procedures: RwLock<Arc<Procedures>>,
    /// The thread with an open [`Transaction`], which holds the store.
    tx_thread: Mutex<Option<ThreadId>>,
    /// The store's [`describe`](GraphStore::describe), taken when it was wrapped.
    description: String,
    /// The CSR export ([`Database::csr`]), columns and label sets of a committed state.
    kernel_cache: Arc<accel::KernelCache>,
    /// The vector index matrices of a committed state.
    vector_cache: Arc<vector::VectorCache>,
    /// The full-text index postings of a committed state.
    text_cache: Arc<fulltext::TextCache>,
    /// The kernel backend read queries use, and when they use it.
    kernels: RwLock<(Arc<dyn Kernels>, KernelMode)>,
}

/// Settings for [`Database::open`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DatabaseConfig {
    pub storage: StorageMode,
}

impl From<StorageMode> for DatabaseConfig {
    fn from(storage: StorageMode) -> Self {
        DatabaseConfig { storage }
    }
}

impl Database<MemoryGraph> {
    /// Creates an empty in-memory database.
    pub fn in_memory() -> Self {
        Self::with_store(MemoryGraph::new())
    }
}

impl Database<BoxedStore> {
    /// Opens a database with the storage mode chosen at runtime.
    ///
    /// ```no_run
    /// use graphbuff::{Database, StorageMode};
    ///
    /// let db = Database::open(StorageMode::hybrid("data/movies"))?;
    /// db.execute("CREATE (:Movie {title: 'Heat'})")?; // durable once this returns
    /// # Ok::<(), graphbuff::Error>(())
    /// ```
    pub fn open(config: impl Into<DatabaseConfig>) -> Result<Self> {
        Ok(Self::with_store(open_store(&config.into().storage)?))
    }
}

impl Default for Database<MemoryGraph> {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl<S: GraphStore> Database<S> {
    /// Wraps an existing store.
    pub fn with_store(mut store: S) -> Self {
        // What was written before is in the first snapshot.
        drop(store.take_changes());
        let committed = store.snapshot();
        let vector_cache = Arc::<vector::VectorCache>::default();
        let text_cache = Arc::<fulltext::TextCache>::default();
        if let Some(s) = &committed {
            vector_cache.published(s, graphbuff_core::Changes::unknown());
            text_cache.published(s, graphbuff_core::Changes::unknown());
        }
        Database {
            committed: RwLock::new(committed),
            description: store.describe(),
            store: RwLock::new(store),
            import_dir: RwLock::new(None),
            procedures: RwLock::new(Procedures::builtin()),
            tx_thread: Mutex::new(None),
            kernel_cache: Arc::default(),
            vector_cache,
            text_cache,
            kernels: RwLock::new((Arc::new(CpuKernels::new()), KernelMode::Auto)),
        }
    }

    /// Adds a procedure that `CALL` can run (see [`Procedures::register`]). Fails if
    /// the signature is malformed or a procedure of that name exists.
    pub fn register_procedure<F>(&self, signature: ProcedureSignature, body: F) -> Result<()>
    where
        F: Fn(&ProcedureContext<'_>, &[Value]) -> Result<ProcedureRows> + Send + Sync + 'static,
    {
        let mut guard = self.procedures.write().unwrap_or_else(|e| e.into_inner());
        let mut next = Procedures::clone(&guard);
        next.register(signature, body)?;
        *guard = Arc::new(next);
        Ok(())
    }

    /// The procedures `CALL` can run: the built-ins and the registered ones.
    pub fn procedures(&self) -> Arc<Procedures> {
        self.procedures.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replaces the procedures `CALL` can run.
    pub fn set_procedures(&self, procedures: Arc<Procedures>) {
        *self.procedures.write().unwrap_or_else(|e| e.into_inner()) = procedures;
    }

    /// An executor over `store` with this database's settings.
    fn executor<'s>(&self, store: StoreRef<'s>, params: &'s BTreeMap<String, Value>) -> Executor<'s> {
        let mut ex = Executor::new(store, params);
        ex.import_dir = self.import_dir();
        ex.procedures = self.procedures();
        ex
    }

    /// Lets `LOAD CSV` read files in `dir` (and its subdirectories); `None` (the
    /// default) disables `LOAD CSV`. `file:///a/b.csv` and `a/b.csv` both mean
    /// `dir/a/b.csv`; paths that would leave `dir` are rejected.
    pub fn set_import_dir(&self, dir: Option<PathBuf>) {
        *self.import_dir.write().unwrap_or_else(|e| e.into_inner()) = dir;
    }

    /// The directory `LOAD CSV` reads from, if it is enabled.
    pub fn import_dir(&self) -> Option<PathBuf> {
        self.import_dir.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Runs a Cypher statement without parameters.
    pub fn execute(&self, query: &str) -> Result<QueryResult> {
        self.execute_with_params(query, &BTreeMap::new())
    }

    /// Runs a Cypher statement; `$name` in the query refers to `params["name"]`.
    pub fn execute_with_params(
        &self,
        query: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<QueryResult> {
        self.execute_query(&graphbuff_cypher::parse(query)?, params)
    }

    /// Runs a statement with a time limit or a [`CancelHandle`]: it fails with
    /// [`Error::Timeout`] or [`Error::Cancelled`] (and its changes are rolled back) if
    /// it runs too long or is cancelled, including while it waits for another writer.
    ///
    /// ```
    /// use std::time::Duration;
    /// use graphbuff::{Database, Error, QueryOptions};
    ///
    /// let db = Database::in_memory();
    /// db.execute("UNWIND range(1, 1000) AS i CREATE (:N)")?;
    /// // A billion rows.
    /// let slow = "MATCH (a:N), (b:N), (c:N) RETURN count(*)";
    /// let options = QueryOptions::default().with_timeout(Duration::from_millis(50));
    /// let r = db.execute_with_options(slow, &Default::default(), &options);
    /// assert!(matches!(r, Err(Error::Timeout(_))));
    /// # Ok::<(), Error>(())
    /// ```
    pub fn execute_with_options(
        &self,
        query: &str,
        params: &BTreeMap<String, Value>,
        options: &QueryOptions,
    ) -> Result<QueryResult> {
        let interrupt = Interrupt::start(options);
        let ast = graphbuff_cypher::parse(query)?;
        self.run(&ast, params, interrupt)
    }

    /// Runs an already parsed query.
    pub fn execute_query(
        &self,
        ast: &cypher::ast::Query,
        params: &BTreeMap<String, Value>,
    ) -> Result<QueryResult> {
        self.run(ast, params, None)
    }

    /// Runs an already parsed query with a time limit or a [`CancelHandle`] (see
    /// [`Database::execute_with_options`]).
    pub fn execute_query_with_options(
        &self,
        ast: &cypher::ast::Query,
        params: &BTreeMap<String, Value>,
        options: &QueryOptions,
    ) -> Result<QueryResult> {
        self.run(ast, params, Interrupt::start(options))
    }

    fn run(
        &self,
        ast: &cypher::ast::Query,
        params: &BTreeMap<String, Value>,
        interrupt: Option<Interrupt>,
    ) -> Result<QueryResult> {
        if let Some(i) = &interrupt {
            i.check_now()?;
        }
        if ast.mode != QueryMode::Explain
            && let Some(at) = batched::position(ast)?
        {
            self.check_no_transaction()?;
            return self.run_in_transactions(ast, at, params, interrupt);
        }
        if !writes(ast) || ast.mode == QueryMode::Explain {
            if self.snapshot().is_none() {
                self.check_no_transaction()?;
            }
            return self.view_with_kernels(params, |mut ex| {
                // Only read: the plan of a writing query under EXPLAIN is not run.
                if ast.mode == QueryMode::Explain {
                    return explain_result(ast, &ex);
                }
                if writes(ast) {
                    ex.accel = None;
                }
                ex.interrupt = interrupt;
                run_query(&mut ex, ast)
            });
        }

        self.check_no_transaction()?;
        let mut guard = self.lock_store(interrupt.as_ref())?;
        guard.begin()?;
        match self.run_on_store(&mut guard, ast, params, interrupt) {
            // A failed commit has already rolled the transaction back.
            Ok(r) => {
                guard.commit()?;
                let old = self.publish(&mut guard);
                // The old snapshot may be the last owner of the versions this query
                // replaced: free them without holding the store.
                drop(guard);
                drop(old);
                Ok(r)
            }
            Err(e) => {
                guard.rollback();
                Err(e)
            }
        }
    }

    /// Runs a statement on the store inside its open transaction, which the caller
    /// commits or rolls back.
    pub(crate) fn run_on_store(
        &self,
        store: &mut S,
        ast: &cypher::ast::Query,
        params: &BTreeMap<String, Value>,
        interrupt: Option<Interrupt>,
    ) -> Result<QueryResult> {
        if let Some(i) = &interrupt {
            i.check_now()?;
        }
        if ast.mode == QueryMode::Explain {
            return explain_result(ast, &self.executor(StoreRef::Read(store), params));
        }
        if batched::position(ast)?.is_some() {
            return Err(batched::in_explicit_transaction());
        }
        let store = if writes(ast) { StoreRef::Write(store) } else { StoreRef::Read(store) };
        let mut ex = self.executor(store, params);
        ex.interrupt = interrupt;
        run_query(&mut ex, ast)
    }

    /// Locks the store for writing. With an interrupt, the wait stops (and fails) when
    /// its time runs out or it is cancelled.
    fn lock_store(&self, interrupt: Option<&Interrupt>) -> Result<RwLockWriteGuard<'_, S>> {
        let Some(interrupt) = interrupt else {
            return Ok(self.store.write().unwrap_or_else(|e| e.into_inner()));
        };
        let mut pause = std::time::Duration::from_micros(50);
        loop {
            match self.store.try_write() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(e)) => return Ok(e.into_inner()),
                Err(TryLockError::WouldBlock) => {}
            }
            interrupt.check_now()?;
            let mut nap = pause;
            if let Some(d) = interrupt.deadline() {
                nap = nap.min(d.saturating_duration_since(graphbuff_core::clock::Instant::now()));
            }
            std::thread::sleep(nap);
            pause = (pause * 2).min(std::time::Duration::from_millis(5));
        }
    }

    /// Opens a transaction: the statements run through it see the changes of the ones
    /// before and commit or roll back together (see [`Transaction`]). Waits while another
    /// transaction or writing query runs; fails if this thread has one open already.
    pub fn begin(&self) -> Result<Transaction<'_, S>> {
        self.begin_with_options(&QueryOptions::default())
    }

    /// [`Database::begin`], giving up with [`Error::Timeout`] or [`Error::Cancelled`] if
    /// the wait for another writer outlasts `options`. The options limit only the wait,
    /// not the transaction: pass options to each statement for that.
    pub fn begin_with_options(&self, options: &QueryOptions) -> Result<Transaction<'_, S>> {
        let interrupt = Interrupt::start(options);
        if let Some(i) = &interrupt {
            i.check_now()?;
        }
        self.check_no_transaction()?;
        let mut guard = self.lock_store(interrupt.as_ref())?;
        guard.begin()?;
        *self.tx_thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::thread::current().id());
        Ok(Transaction::new(self, guard))
    }

    /// Called by a transaction that released the store.
    pub(crate) fn end_transaction(&self) {
        *self.tx_thread.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Fails if this thread holds an open transaction: waiting for the store would wait
    /// for itself.
    fn check_no_transaction(&self) -> Result<()> {
        if *self.tx_thread.lock().unwrap_or_else(|e| e.into_inner()) == Some(std::thread::current().id()) {
            return Err(Error::Runtime(
                "this thread has an open transaction: run the statement through it, or end it first".into(),
            ));
        }
        Ok(())
    }

    /// Makes the store's committed state the one read queries see, returning the snapshot
    /// it replaces. Called with the store locked for writing, so snapshots are published
    /// in commit order.
    fn publish(&self, store: &mut S) -> Option<Snapshot> {
        let changes = store.take_changes();
        let snapshot = store.snapshot()?;
        self.text_cache.published(&snapshot, changes.clone());
        self.vector_cache.published(&snapshot, changes);
        self.committed.write().unwrap_or_else(|e| e.into_inner()).replace(snapshot)
    }

    /// The last committed state, if the store takes snapshots.
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.committed.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Runs `f` on the last committed state: a snapshot, or the store itself (waiting
    /// for a writer) if the store takes no snapshots.
    fn view<R>(&self, f: impl FnOnce(&dyn GraphStore) -> R) -> R {
        match self.snapshot() {
            Some(snapshot) => f(&*snapshot),
            None => f(&*self.store.read().unwrap_or_else(|e| e.into_inner())),
        }
    }

    /// Runs `f` with an executor reading the last committed state, as
    /// [`view`](Self::view) does; with a snapshot, the executor can run kernels on it.
    fn view_with_kernels<R>(&self, params: &BTreeMap<String, Value>, f: impl FnOnce(Executor<'_>) -> R) -> R {
        match self.snapshot() {
            Some(snapshot) => {
                let mut ex = self.executor(StoreRef::Read(&*snapshot), params);
                let (kernels, mode) = self.kernels.read().unwrap_or_else(|e| e.into_inner()).clone();
                ex.accel = Some(accel::Accel {
                    cache: self.kernel_cache.clone(),
                    vectors: self.vector_cache.clone(),
                    texts: self.text_cache.clone(),
                    snapshot: snapshot.clone(),
                    kernels,
                    mode,
                });
                f(ex)
            }
            None => self.view(|s| f(self.executor(StoreRef::Read(s), params))),
        }
    }

    /// When read queries run parts of their plans as kernels over the CSR export (see
    /// [`KernelMode`]); `Auto` by default. Queries that write, and statements in a
    /// [`Transaction`], always run on rows.
    pub fn set_kernel_mode(&self, mode: KernelMode) {
        self.kernels.write().unwrap_or_else(|e| e.into_inner()).1 = mode;
    }

    pub fn kernel_mode(&self) -> KernelMode {
        self.kernels.read().unwrap_or_else(|e| e.into_inner()).1
    }

    /// The kernel backend read queries use ([`CpuKernels`] by default).
    pub fn set_kernels(&self, kernels: Arc<dyn Kernels>) {
        self.kernels.write().unwrap_or_else(|e| e.into_inner()).0 = kernels;
    }

    /// The execution plan of a query, as an indented operator tree, root first:
    ///
    /// ```
    /// let db = graphbuff::Database::in_memory();
    /// let plan = db.explain("MATCH (p:Person) RETURN p.name LIMIT 3").unwrap();
    /// assert_eq!(plan.lines().next(), Some("Select"));
    /// ```
    pub fn explain(&self, query: &str) -> Result<String> {
        let ast = graphbuff_cypher::parse(query)?;
        let params = BTreeMap::new();
        if self.snapshot().is_none() {
            self.check_no_transaction()?;
        }
        self.view_with_kernels(&params, |mut ex| {
            if writes(&ast) {
                ex.accel = None;
            }
            Ok(plan::QueryPlan::build(&ast, &ex)?.explain())
        })
    }

    /// Writes a backup of the last committed state into `dest`, a new or empty directory
    /// (see [`GraphStore::backup`]). With a store that takes snapshots it runs on one,
    /// so queries and transactions keep running and committing meanwhile.
    pub fn backup(&self, dest: &std::path::Path) -> Result<BackupInfo> {
        if self.snapshot().is_none() {
            self.check_no_transaction()?;
        }
        self.view(|s| s.backup(dest))
    }

    /// Checks the store's files for consistency (persistent storage only; see
    /// [`GraphStore::check`]). Runs on a snapshot, like [`backup`](Self::backup).
    pub fn check(&self) -> Result<CheckReport> {
        if self.snapshot().is_none() {
            self.check_no_transaction()?;
        }
        self.view(|s| s.check())
    }

    /// A compressed sparse row export of the last committed state's adjacency (see
    /// [`Csr`]), for GPU kernels and graph algorithms.
    ///
    /// With a store that takes snapshots, the export is built on the first call after a
    /// commit and shared by later calls until the next commit; callers that ask while it
    /// is built wait for it rather than build another. Writers keep committing during
    /// the build. A store without snapshots is exported anew on every call, waiting for
    /// a writer.
    pub fn csr(&self) -> Result<Arc<Csr>> {
        let Some(snapshot) = self.snapshot() else {
            self.check_no_transaction()?;
            return self.view(|s| Csr::build(s)).map(Arc::new);
        };
        self.kernel_cache.csr(&snapshot)
    }

    /// Compacts durable storage (snapshot + empty log). A no-op for in-memory storage.
    pub fn checkpoint(&self) -> Result<()> {
        self.check_no_transaction()?;
        self.store
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .checkpoint()
    }

    /// A short description of the storage backend.
    pub fn storage_description(&self) -> String {
        self.description.clone()
    }

    /// Runs `f` with shared access to the underlying store. Unlike a read query, this
    /// waits while a query or [`Transaction`] writes.
    ///
    /// # Panics
    /// If this thread has a transaction open (it would wait for itself).
    pub fn read<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        if let Err(e) = self.check_no_transaction() {
            panic!("Database::read: {e}");
        }
        f(&self.store.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// The vector and full-text indexes whose contents are cached in memory (built by a
    /// query on the last committed state, and kept up to date since).
    pub fn cached_indexes(&self) -> usize {
        self.vector_cache.len() + self.text_cache.len()
    }

    pub fn node_count(&self) -> usize {
        self.view(|s| s.node_count())
    }

    pub fn relationship_count(&self) -> usize {
        self.view(|s| s.relationship_count())
    }

    /// Consumes the database and returns the store.
    pub fn into_store(self) -> S {
        self.store.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

/// Whether a query has clauses that write.
fn writes(ast: &cypher::ast::Query) -> bool {
    ast.parts.iter().any(|p| p.clauses.iter().any(|c| c.is_updating()))
}

/// The result of an `EXPLAIN` query: the plan, nothing run.
fn explain_result(ast: &cypher::ast::Query, ex: &Executor) -> Result<QueryResult> {
    let plan = plan::QueryPlan::build(ast, ex)?.describe(None);
    Ok(QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        stats: QueryStats::default(),
        plan: Some(plan),
    })
}

/// Plans and runs `ast` (measuring it under `PROFILE`), then materializes the result.
fn run_query(ex: &mut Executor, ast: &cypher::ast::Query) -> Result<QueryResult> {
    let plan = plan::QueryPlan::build(ast, ex)?;
    if ast.mode == QueryMode::Profile {
        ex.profile = Some(Default::default());
    }
    let raw = ex.run_plan(&plan)?;
    ex.check_constraints()?;
    let mut result = finish(raw, ex)?;
    // Materializing the rows takes memory of its own.
    if let Some(i) = &ex.interrupt {
        i.check_now()?;
    }
    if let Some(profile) = &ex.profile {
        result.plan = Some(plan.describe(Some(&profile.borrow())));
    }
    Ok(result)
}

/// Materializes the result rows. Runs inside the query's transaction, so a failed read
/// here fails (and rolls back) the query.
fn finish(raw: exec::RawResult, ex: &Executor) -> Result<QueryResult> {
    let store = ex.store();
    Ok(QueryResult {
        columns: raw.columns,
        rows: raw
            .rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|v| ResultValue::materialize(v, store))
                    .collect()
            })
            .collect::<Result<_>>()?,
        stats: ex.stats.clone(),
        plan: None,
    })
}

/// Builds a parameter map: `params! { "name" => "Ann", "age" => 42 }`.
#[macro_export]
macro_rules! params {
    () => { ::std::collections::BTreeMap::<String, $crate::Value>::new() };
    ($($k:expr => $v:expr),+ $(,)?) => {{
        let mut m = ::std::collections::BTreeMap::<String, $crate::Value>::new();
        $( m.insert(($k).to_string(), $crate::Value::from($v)); )+
        m
    }};
}
