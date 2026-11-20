//! Execution context: variable scopes, rows, evaluation environments and store access.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;
use std::time::Duration;

use graphbuff_core::{Error, GraphStore, NodeId, RelId, Result, Value};

use crate::plan::QueryPlan;
use crate::result::QueryStats;

/// Ordered set of variable names; a row stores one value per name, by position.
#[derive(Clone, Debug, Default)]
pub(crate) struct Scope {
    names: Vec<String>,
    index: HashMap<String, usize>,
}

impl Scope {
    pub fn from_names(names: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut s = Scope::default();
        for n in names {
            if s.get(&n).is_some() {
                return Err(Error::Semantic(format!(
                    "multiple result columns with the same name `{n}` are not supported"
                )));
            }
            s.add(&n);
        }
        Ok(s)
    }

    pub fn get(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
    }

    /// Adds `name` if missing and returns its slot.
    pub fn add(&mut self, name: &str) -> usize {
        if let Some(i) = self.get(name) {
            return i;
        }
        let i = self.names.len();
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), i);
        i
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }
}

pub(crate) type Row = Vec<Value>;

/// The intermediate result flowing between operators.
pub(crate) struct Frame {
    pub scope: Scope,
    pub rows: Vec<Row>,
    /// Between a projection and its `Select`: the projection's input scope. Each row then
    /// holds the projected values (`scope`) followed by its source row (`hidden`).
    pub hidden: Option<Scope>,
}

impl Frame {
    pub fn new(scope: Scope, rows: Vec<Row>) -> Frame {
        Frame {
            scope,
            rows,
            hidden: None,
        }
    }
}

/// Variables visible to an expression.
pub(crate) struct Env<'a> {
    pub scope: &'a Scope,
    pub row: &'a [Value],
    /// Locals shadow scope variables (list comprehension variables, projected aliases).
    pub locals: Vec<(String, Value)>,
    /// Values of computed aggregates, referenced by `Expr::AggregateRef`.
    pub aggs: Option<&'a [Value]>,
}

impl<'a> Env<'a> {
    pub fn new(scope: &'a Scope, row: &'a [Value]) -> Self {
        Env {
            scope,
            row,
            locals: Vec::new(),
            aggs: None,
        }
    }

    pub fn lookup(&self, name: &str) -> Option<&Value> {
        if let Some((_, v)) = self.locals.iter().rev().find(|(n, _)| n == name) {
            return Some(v);
        }
        self.scope.get(name).and_then(|i| self.row.get(i))
    }

    pub fn with_local(&self, name: &str, value: Value) -> Env<'a> {
        let mut locals = self.locals.clone();
        locals.push((name.to_string(), value));
        Env {
            scope: self.scope,
            row: self.row,
            locals,
            aggs: self.aggs,
        }
    }
}

pub(crate) enum StoreRef<'s> {
    Read(&'s dyn GraphStore),
    Write(&'s mut dyn GraphStore),
}

/// Executes one parsed query against a store.
pub(crate) struct Executor<'s> {
    store: StoreRef<'s>,
    pub params: &'s BTreeMap<String, Value>,
    pub stats: QueryStats,
    /// Under `PROFILE`: rows and time per operator or match step, by its address.
    pub profile: Option<RefCell<Profile>>,
    /// Nodes this query created or changed (properties, labels), to check against the
    /// constraints before commit. `None` when the store has no constraints.
    pub touched: Option<BTreeSet<NodeId>>,
    /// While a `CALL { … }` subquery with updates runs: its start row (the imported
    /// variables), which replaces the single empty row at the bottom of its plan.
    pub argument: Option<(Scope, Row)>,
    /// Plans of `EXISTS` / `COUNT` / `COLLECT` subqueries, by the address of their query
    /// and the outer variables visible to them.
    pub subplans: RefCell<SubPlans>,
    /// Compiled `=~` patterns, by source.
    pub regexes: RefCell<HashMap<String, Rc<crate::regex::Regex>>>,
    /// Where `LOAD CSV` reads files; `None` disables it.
    pub import_dir: Option<std::path::PathBuf>,
    /// The web URLs `LOAD CSV` may read; `None`: none.
    pub csv_urls: Option<crate::csv::CsvUrls>,
    /// The statement's clock, `(days, nanos)` in UTC, read on first use: `date()`,
    /// `datetime()`, … return the same time throughout a query.
    pub now: std::cell::Cell<Option<(i64, i64)>>,
    /// The procedures `CALL` can run.
    pub procedures: std::sync::Arc<crate::procedures::Procedures>,
    /// The query's time limit and cancellation, if it has either.
    pub interrupt: Option<crate::interrupt::Interrupt>,
    /// Kernels for a read query on a committed state ([`crate::accel`]); `None` runs
    /// everything on rows.
    pub accel: Option<crate::accel::Accel>,
    /// Nodes this query deleted, and the relationships it deleted with their types:
    /// reading their properties or labels afterwards fails, `type(r)` still answers.
    pub deleted_nodes: std::collections::HashSet<NodeId>,
    pub deleted_rels: HashMap<RelId, String>,
}

/// Cached subquery plans: (query address, visible variables) → plan.
pub(crate) type SubPlans = HashMap<(usize, Vec<String>), Rc<QueryPlan>>;

/// What `PROFILE` measured, keyed by the address of an [`crate::plan::Operator`] or
/// [`crate::plan::Step`] (the plan outlives the run, so addresses are stable).
pub(crate) type Profile = HashMap<usize, OpProfile>;

/// Rows produced and time spent, including the operator's inputs.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OpProfile {
    pub rows: u64,
    pub time: Duration,
}

impl<'s> Executor<'s> {
    pub fn new(store: StoreRef<'s>, params: &'s BTreeMap<String, Value>) -> Self {
        let mut ex = Executor {
            store,
            params,
            stats: QueryStats::default(),
            profile: None,
            touched: None,
            argument: None,
            subplans: RefCell::default(),
            regexes: RefCell::default(),
            import_dir: None,
            csv_urls: None,
            now: std::cell::Cell::new(None),
            procedures: crate::procedures::Procedures::builtin(),
            interrupt: None,
            accel: None,
            deleted_nodes: Default::default(),
            deleted_rels: HashMap::new(),
        };
        if !ex.store().constraints().is_empty() {
            ex.touched = Some(BTreeSet::new());
        }
        ex
    }

    /// Fails if `v` is a node or relationship this query deleted.
    pub fn check_not_deleted(&self, v: &Value) -> Result<()> {
        let deleted = match v {
            Value::Node(id) => self.deleted_nodes.contains(id),
            Value::Rel(id) => self.deleted_rels.contains_key(id),
            _ => false,
        };
        if deleted {
            return Err(Error::Runtime(format!("the {} was deleted in this transaction", v.type_name().to_lowercase())));
        }
        Ok(())
    }

    /// The statement's current time (see [`Executor::now`]).
    pub fn statement_time(&self) -> (i64, i64) {
        match self.now.get() {
            Some(t) => t,
            None => {
                let t = graphbuff_core::temporal::now_utc();
                self.now.set(Some(t));
                t
            }
        }
    }

    /// Fails if the query was cancelled or ran out of time. Called once per row produced
    /// or candidate looked at; cheap.
    #[inline]
    pub fn check_interrupt(&self) -> Result<()> {
        match &self.interrupt {
            Some(i) => i.check(),
            None => Ok(()),
        }
    }

    /// Notes that node `id` was created or changed (see [`Executor::touched`]).
    pub fn touch(&mut self, id: NodeId) {
        if let Some(t) = &mut self.touched {
            t.insert(id);
        }
    }

    /// Adds `rows` and `time` to what the plan node at `key` has used so far.
    pub fn record(&self, key: usize, rows: usize, time: Duration) {
        if let Some(p) = &self.profile {
            let mut p = p.borrow_mut();
            let e = p.entry(key).or_default();
            e.rows += rows as u64;
            e.time += time;
        }
    }

    pub fn store(&self) -> &dyn GraphStore {
        match &self.store {
            StoreRef::Read(s) => *s,
            StoreRef::Write(s) => &**s,
        }
    }

    pub fn store_mut(&mut self) -> Result<&mut dyn GraphStore> {
        match &mut self.store {
            StoreRef::Write(s) => Ok(&mut **s),
            StoreRef::Read(_) => Err(Error::Runtime(
                "attempted to write in a read-only query".into(),
            )),
        }
    }
}
