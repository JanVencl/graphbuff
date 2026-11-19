//! Explicit transactions: several statements that commit or roll back together.

use std::collections::BTreeMap;
use std::sync::RwLockWriteGuard;

use graphbuff_core::{Error, GraphStore, Result, Value};

use crate::interrupt::Interrupt;
use crate::{Database, QueryOptions, QueryResult, cypher};

/// An open transaction on a [`Database`], from [`Database::begin`].
///
/// Its statements see each other's changes; nothing else does until
/// [`commit`](Transaction::commit). Read queries on the database keep running on the last
/// commit meanwhile, while other writers (queries that write, and other transactions) wait
/// until this one ends. Dropping an open transaction rolls it back.
///
/// A statement that fails rolls the whole transaction back: later calls then fail too.
///
/// ```
/// let db = graphbuff::Database::in_memory();
/// let mut tx = db.begin()?;
/// tx.execute("CREATE (:Account {id: 1, balance: 100})")?;
/// tx.execute("CREATE (:Account {id: 2, balance: 0})")?;
/// tx.execute("MATCH (a:Account {id: 1}), (b:Account {id: 2}) SET a.balance = 50, b.balance = 50")?;
/// // Not visible outside the transaction yet.
/// assert_eq!(db.node_count(), 0);
/// tx.commit()?;
/// assert_eq!(db.node_count(), 2);
/// # Ok::<(), graphbuff::Error>(())
/// ```
pub struct Transaction<'db, S: GraphStore> {
    db: &'db Database<S>,
    /// The locked store while the transaction is open.
    store: Option<RwLockWriteGuard<'db, S>>,
    /// Why the transaction ended early, if a statement failed.
    failed: Option<String>,
}

impl<'db, S: GraphStore> Transaction<'db, S> {
    pub(crate) fn new(db: &'db Database<S>, store: RwLockWriteGuard<'db, S>) -> Self {
        Transaction {
            db,
            store: Some(store),
            failed: None,
        }
    }

    /// Whether the transaction can still run statements and commit.
    pub fn is_open(&self) -> bool {
        self.store.is_some()
    }

    fn open_store(&mut self) -> Result<&mut RwLockWriteGuard<'db, S>> {
        match (&mut self.store, &self.failed) {
            (Some(store), _) => Ok(store),
            (None, Some(why)) => Err(Error::Runtime(format!(
                "the transaction was rolled back after an earlier statement failed: {why}"
            ))),
            (None, None) => Err(Error::Runtime("the transaction has ended".into())),
        }
    }

    /// Runs a statement without parameters in the transaction.
    pub fn execute(&mut self, query: &str) -> Result<QueryResult> {
        self.execute_with_params(query, &BTreeMap::new())
    }

    /// Runs a statement in the transaction; `$name` refers to `params["name"]`.
    pub fn execute_with_params(&mut self, query: &str, params: &BTreeMap<String, Value>) -> Result<QueryResult> {
        // A statement that does not parse changes nothing: the transaction stays open.
        self.open_store()?;
        let ast = graphbuff_cypher::parse(query)?;
        self.execute_query(&ast, params)
    }

    /// Runs an already parsed statement in the transaction.
    pub fn execute_query(&mut self, ast: &cypher::ast::Query, params: &BTreeMap<String, Value>) -> Result<QueryResult> {
        self.run(ast, params, None)
    }

    /// Runs a statement in the transaction with a time limit or a
    /// [`CancelHandle`](crate::CancelHandle). A statement that runs too long or is
    /// cancelled fails, and so rolls the whole transaction back.
    pub fn execute_with_options(
        &mut self,
        query: &str,
        params: &BTreeMap<String, Value>,
        options: &QueryOptions,
    ) -> Result<QueryResult> {
        let interrupt = Interrupt::start(options);
        self.open_store()?;
        let ast = graphbuff_cypher::parse(query)?;
        self.run(&ast, params, interrupt)
    }

    /// Runs an already parsed statement in the transaction with a time limit or a
    /// [`CancelHandle`](crate::CancelHandle) (see [`Transaction::execute_with_options`]).
    pub fn execute_query_with_options(
        &mut self,
        ast: &cypher::ast::Query,
        params: &BTreeMap<String, Value>,
        options: &QueryOptions,
    ) -> Result<QueryResult> {
        self.run(ast, params, Interrupt::start(options))
    }

    fn run(
        &mut self,
        ast: &cypher::ast::Query,
        params: &BTreeMap<String, Value>,
        interrupt: Option<Interrupt>,
    ) -> Result<QueryResult> {
        let db = self.db;
        let store = self.open_store()?;
        match db.run_on_store(&mut **store, ast, params, interrupt) {
            Ok(r) => Ok(r),
            Err(e) => {
                self.failed = Some(e.to_string());
                self.end();
                Err(e)
            }
        }
    }

    /// Makes the transaction's changes permanent and visible. If the store fails to
    /// commit, the transaction has been rolled back.
    pub fn commit(mut self) -> Result<()> {
        self.open_store()?;
        let mut store = self.store.take().expect("checked open");
        let committed = store.commit();
        let old = committed.is_ok().then(|| self.db.publish(&mut store)).flatten();
        drop(store);
        self.db.end_transaction();
        // The old snapshot may own the versions this transaction replaced.
        drop(old);
        committed
    }

    /// Discards the transaction's changes.
    pub fn rollback(mut self) {
        self.end();
    }

    /// Rolls back (if still open) and releases the store.
    fn end(&mut self) {
        if let Some(mut store) = self.store.take() {
            store.rollback();
            drop(store);
            self.db.end_transaction();
        }
    }
}

impl<S: GraphStore> Drop for Transaction<'_, S> {
    fn drop(&mut self) {
        self.end();
    }
}

impl<S: GraphStore> std::fmt::Debug for Transaction<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction").field("open", &self.is_open()).finish()
    }
}
