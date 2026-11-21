//! `CALL { … } IN TRANSACTIONS`: a subquery that runs in batches of input rows, each in
//! a transaction of its own that commits before the next batch starts, so one query can
//! change more of the graph than a single transaction should hold.
//!
//! The query is split at the clause. The clauses before it (which must only read) run as
//! a stream on the committed state the query started on, and are pulled a batch at a
//! time. Each batch runs as
//!
//! ```text
//! UNWIND $rows AS row WITH row[0] AS a, row[1] AS b, … CALL { … } RETURN *
//! ```
//!
//! in a transaction of its own; the rows it returns (or, for a subquery that returns
//! nothing, the batch's own rows) go on to the clauses after the `CALL`, which must also
//! only read and run once all batches are done, on the state they left.

use std::collections::BTreeMap;

use graphbuff_core::{Error, GraphStore, Result, Value};
use graphbuff_cypher::ast::{
    CallScope, Clause, Expr, InTransactions, OnError, Projection, ProjectionItem, Query, QueryMode, SingleQuery,
};

use crate::context::{Env, Frame, Row, Scope, StoreRef};
use crate::interrupt::Interrupt;
use crate::plan::{PartPlan, QueryPlan};
use crate::{Database, QueryResult, QueryStats};

/// Input rows per transaction without `OF n ROWS`.
pub const DEFAULT_BATCH_SIZE: usize = 1000;

/// The parameter that carries a batch's rows, and the variable for one of them. Their
/// names have spaces, so no query can refer to them.
const ROWS: &str = " batch rows";
const ROW: &str = " batch row";
/// A column added so that `RETURN *` has something to return without variables.
const PAD: &str = " pad";

/// The positions of the columns a query can see (internal names start with a space).
fn visible(columns: &[String]) -> Vec<usize> {
    (0..columns.len()).filter(|&i| !columns[i].starts_with(' ')).collect()
}

fn pick(row: Row, keep: &[usize]) -> Row {
    if keep.len() == row.len() {
        return row;
    }
    let mut row = row;
    keep.iter().map(|&i| std::mem::take(&mut row[i])).collect()
}

/// The position of the `CALL { … } IN TRANSACTIONS` clause in `ast`, if it has one.
pub(crate) fn position(ast: &Query) -> Result<Option<usize>> {
    let mut found = None;
    for part in &ast.parts {
        for (i, c) in part.clauses.iter().enumerate() {
            if matches!(c, Clause::CallSubquery { in_transactions: Some(_), .. }) {
                if ast.parts.len() > 1 {
                    return Err(Error::Unsupported("CALL { … } IN TRANSACTIONS in a UNION is not supported".into()));
                }
                if found.is_some() {
                    return Err(Error::Unsupported(
                        "only one CALL { … } IN TRANSACTIONS per query is supported".into(),
                    ));
                }
                found = Some(i);
            }
        }
    }
    Ok(found)
}

/// The error for `CALL { … } IN TRANSACTIONS` inside an explicit transaction.
pub(crate) fn in_explicit_transaction() -> Error {
    Error::Semantic(
        "CALL { … } IN TRANSACTIONS commits transactions of its own, so it can only run as a query of its own, \
         not inside an explicit transaction"
            .into(),
    )
}

fn var(name: &str) -> Expr {
    Expr::Var(name.into())
}

fn projection(items: Vec<ProjectionItem>, star: bool) -> Projection {
    Projection {
        distinct: false,
        star,
        items,
        order_by: Vec::new(),
        skip: None,
        limit: None,
        where_: None,
    }
}

/// `UNWIND $rows AS row WITH row[0] AS names[0], …`: the rows of a parameter, as
/// variables.
fn unwind_rows(names: &[String]) -> Vec<Clause> {
    let mut clauses = vec![Clause::Unwind { expr: Expr::Param(ROWS.into()), var: ROW.into() }];
    if !names.is_empty() {
        let items = names
            .iter()
            .enumerate()
            .map(|(i, name)| ProjectionItem {
                expr: Expr::Index(Box::new(var(ROW)), Box::new(Expr::Literal(Value::Int(i as i64)))),
                alias: name.clone(),
            })
            .collect();
        clauses.push(Clause::With(projection(items, false)));
    }
    clauses
}

fn single(clauses: Vec<Clause>) -> Query {
    Query { parts: vec![SingleQuery { clauses }], union_all: false, mode: QueryMode::Run }
}

/// A positive integer option (`OF n ROWS`, `IN n CONCURRENT`), evaluated without variables.
fn count(db_eval: &dyn Fn(&Expr) -> Result<Value>, e: &Expr, what: &str) -> Result<usize> {
    match db_eval(e)? {
        Value::Int(n) if n >= 1 => Ok(n as usize),
        Value::Float(f) if f >= 1.0 && f.fract() == 0.0 && f <= usize::MAX as f64 => Ok(f as usize),
        o => Err(Error::Semantic(format!("{what} must be a positive integer, not {o}"))),
    }
}

/// The status map `REPORT STATUS AS var` gives each row.
fn status(started: bool, committed: bool, transaction: Option<usize>, error: Option<String>) -> Value {
    Value::Map(BTreeMap::from([
        ("started".to_string(), Value::Bool(started)),
        ("committed".to_string(), Value::Bool(committed)),
        (
            "transactionId".to_string(),
            transaction.map_or(Value::Null, |n| Value::String(format!("graphbuff-transaction-{n}"))),
        ),
        ("errorMessage".to_string(), error.map_or(Value::Null, Value::String)),
    ]))
}

/// The batches of one query, as they run.
struct Batches<'q> {
    /// The batch query (`UNWIND … CALL { … } [RETURN *]`).
    inner: Query,
    /// Whether the subquery returns rows (else each input row is kept once).
    returns: bool,
    /// Columns of the rows after the `CALL`, without the status column.
    columns: Vec<String>,
    /// Which columns of the batch query's rows those are (it may return internal ones).
    shown: Vec<usize>,
    /// Width of an input row.
    width: usize,
    options: &'q InTransactions,
    /// Keep the rows for the clauses after the `CALL` (there are some).
    keep: bool,
    rows: Vec<Row>,
    stats: QueryStats,
    /// Transactions started so far.
    started: usize,
    /// After a failure with `ON ERROR BREAK`: run no more batches.
    broken: bool,
}

impl Batches<'_> {
    /// Runs one batch of input rows in a transaction of its own.
    fn run<S: GraphStore>(
        &mut self,
        db: &Database<S>,
        batch: Vec<Row>,
        params: &BTreeMap<String, Value>,
        interrupt: &Option<Interrupt>,
    ) -> Result<()> {
        if let Some(i) = interrupt {
            i.check_now()?;
        }
        if self.broken {
            self.failed(batch, status(false, false, None, None));
            return Ok(());
        }
        self.started += 1;
        let mut params = params.clone();
        params.insert(ROWS.into(), Value::List(batch.iter().cloned().map(Value::List).collect()));
        match db.commit_batch(&self.inner, &params, interrupt.clone()) {
            Ok((rows, stats)) => {
                add_stats(&mut self.stats, &stats);
                if self.keep || self.options.report_status.is_some() {
                    let rows = if self.returns {
                        rows.into_iter().map(|r| pick(r, &self.shown)).collect()
                    } else {
                        batch
                    };
                    let done = status(true, true, Some(self.started), None);
                    self.keep_rows(rows, done);
                }
                Ok(())
            }
            Err(e @ (Error::Timeout(_) | Error::Cancelled(_) | Error::MemoryLimit(_))) => Err(e),
            Err(e) => match self.options.on_error {
                OnError::Fail => Err(e),
                OnError::Continue | OnError::Break => {
                    self.broken = self.options.on_error == OnError::Break;
                    self.failed(batch, status(true, false, Some(self.started), Some(e.to_string())));
                    Ok(())
                }
            },
        }
    }

    /// Keeps the rows of a batch that did not commit: its input rows, with nulls for
    /// what the subquery would have returned.
    fn failed(&mut self, batch: Vec<Row>, status: Value) {
        if !self.keep && self.options.report_status.is_none() {
            return;
        }
        let rows = batch
            .into_iter()
            .map(|input| {
                if !self.returns {
                    return input;
                }
                // `RETURN *` orders the columns by name: put each input value in its place.
                let mut row = vec![Value::Null; self.columns.len()];
                for (slot, v) in self.input_slots().into_iter().zip(input) {
                    row[slot] = v;
                }
                row
            })
            .collect();
        self.keep_rows(rows, status);
    }

    /// Where each input column sits among [`Batches::columns`].
    fn input_slots(&self) -> Vec<usize> {
        let unwind = &self.inner.parts[0].clauses;
        let names: Vec<&str> = match unwind.get(1) {
            Some(Clause::With(p)) => p.items.iter().map(|i| i.alias.as_str()).collect(),
            _ => Vec::new(),
        };
        debug_assert_eq!(names.len(), self.width);
        names
            .iter()
            .map(|n| self.columns.iter().position(|c| c == n).expect("RETURN * returns the input columns"))
            .collect()
    }

    fn keep_rows(&mut self, rows: Vec<Row>, status: Value) {
        if self.options.report_status.is_some() {
            self.rows.extend(rows.into_iter().map(|mut r| {
                r.push(status.clone());
                r
            }));
        } else {
            self.rows.extend(rows);
        }
    }
}

fn add_stats(total: &mut QueryStats, s: &QueryStats) {
    total.nodes_created += s.nodes_created;
    total.nodes_deleted += s.nodes_deleted;
    total.relationships_created += s.relationships_created;
    total.relationships_deleted += s.relationships_deleted;
    total.properties_set += s.properties_set;
    total.labels_added += s.labels_added;
    total.labels_removed += s.labels_removed;
    total.indexes_added += s.indexes_added;
    total.indexes_removed += s.indexes_removed;
    total.constraints_added += s.constraints_added;
    total.constraints_removed += s.constraints_removed;
    total.system_updates += s.system_updates;
}

impl<S: GraphStore> Database<S> {
    /// Runs `ast`, whose clause `at` is `CALL { … } IN TRANSACTIONS`, as a query of its
    /// own (not in an explicit transaction).
    pub(crate) fn run_in_transactions(
        &self,
        ast: &Query,
        at: usize,
        params: &BTreeMap<String, Value>,
        interrupt: Option<Interrupt>,
    ) -> Result<QueryResult> {
        let clauses = &ast.parts[0].clauses;
        let Clause::CallSubquery { scope, query: sub, in_transactions: Some(options), .. } = &clauses[at] else {
            unreachable!("position() found it");
        };
        let (before, after) = (&clauses[..at], &clauses[at + 1..]);
        let procedures = self.procedures();
        if let Some(c) = before
            .iter()
            .chain(after)
            .find(|c| c.is_updating() || procedures.writes_in_clauses(std::slice::from_ref(*c)))
        {
            return Err(Error::Unsupported(format!(
                "{} outside CALL {{ … }} IN TRANSACTIONS: the clauses around it may only read; \
                 move the update into the subquery",
                c.name()
            )));
        }
        if ast.mode == QueryMode::Profile {
            return Err(Error::Unsupported("PROFILE of CALL { … } IN TRANSACTIONS is not supported".into()));
        }

        // Check the whole query (variables, clause order) and the options before running
        // anything.
        let batch_size = self.view(|store| -> Result<usize> {
            let ex = self.executor(StoreRef::Read(store), params);
            QueryPlan::build(ast, &ex)?;
            let scope = Scope::default();
            let eval = |e: &Expr| ex.eval(e, &Env::new(&scope, &[]));
            if let Some(Some(n)) = &options.concurrency {
                count(&eval, n, "IN … CONCURRENT TRANSACTIONS")?;
            }
            options.batch_size.as_ref().map_or(Ok(DEFAULT_BATCH_SIZE), |e| count(&eval, e, "OF … ROWS"))
        })?;

        // The clauses before the CALL, returning every variable they bind.
        let mut before = before.to_vec();
        let pad = ProjectionItem { expr: Expr::Literal(Value::Null), alias: PAD.into() };
        before.push(Clause::Return(projection(vec![pad], true)));
        let before = single(before);
        let returns = matches!(
            sub.parts.first().and_then(|p| p.clauses.last()),
            Some(Clause::Return(_))
        );
        let keep = !after.is_empty();

        let args = (scope, &**sub, options, returns, keep, batch_size);
        let Some(snapshot) = self.snapshot() else {
            // A store without snapshots: read the input rows first, then release it.
            let (names, rows) = self.view(|store| -> Result<(Vec<String>, Vec<Row>)> {
                let mut ex = self.executor(StoreRef::Read(store), params);
                ex.interrupt = interrupt.clone();
                let raw = ex.run_plan(&QueryPlan::build(&before, &ex)?)?;
                let keep = visible(&raw.columns);
                let names = keep.iter().map(|&i| raw.columns[i].clone()).collect();
                Ok((names, raw.rows.into_iter().map(|r| pick(r, &keep)).collect()))
            })?;
            return self.run_batches(args, names, &mut rows.into_iter(), after, params, &interrupt);
        };

        // Stream the input rows from the state the query started on.
        let mut ex = self.executor(StoreRef::Read(&*snapshot), params);
        ex.interrupt = interrupt.clone();
        let plan = QueryPlan::build(&before, &ex)?;
        let Some(PartPlan::Pipeline { root, .. }) = plan.parts.first() else {
            unreachable!("a query of reading clauses ending in RETURN");
        };
        let mut stream = crate::stream::build(&ex, root, Frame::new(Scope::default(), vec![Vec::new()]))?;
        let all = stream.scope().names().to_vec();
        let shown = visible(&all);
        let names = shown.iter().map(|&i| all[i].clone()).collect();
        let mut error = None;
        let mut rows = std::iter::from_fn(|| {
            if error.is_some() {
                return None;
            }
            match stream.next(&ex, crate::stream::BATCH) {
                Ok(batch) => batch,
                Err(e) => {
                    error = Some(e);
                    None
                }
            }
        })
        .flatten()
        .map(|r| pick(r, &shown));
        let result = self.run_batches(args, names, &mut rows, after, params, &interrupt);
        drop(rows);
        match error {
            Some(e) => Err(e),
            None => result,
        }
    }

    /// Runs the batches over `input` (rows of the variables `names`), then the clauses
    /// `after` on their rows.
    fn run_batches(
        &self,
        (scope, sub, options, returns, keep, batch_size): (&CallScope, &Query, &InTransactions, bool, bool, usize),
        names: Vec<String>,
        input: &mut dyn Iterator<Item = Row>,
        after: &[Clause],
        params: &BTreeMap<String, Value>,
        interrupt: &Option<Interrupt>,
    ) -> Result<QueryResult> {
        let mut inner = unwind_rows(&names);
        inner.push(Clause::CallSubquery {
            scope: scope.clone(),
            query: Box::new(sub.clone()),
            in_transactions: None,
            optional: false,
        });
        if returns {
            inner.push(Clause::Return(projection(Vec::new(), true)));
        }
        let inner = single(inner);
        let (columns, shown) = if returns {
            let all = self.view(|store| {
                let ex = self.executor(StoreRef::Read(store), params);
                QueryPlan::build_subquery(&inner, &ex, &[]).map(|(_, columns)| columns)
            })?;
            let shown = visible(&all);
            (shown.iter().map(|&i| all[i].clone()).collect(), shown)
        } else {
            (names.clone(), (0..names.len()).collect())
        };
        let mut batches = Batches {
            inner,
            returns,
            columns,
            shown,
            width: names.len(),
            options,
            keep,
            rows: Vec::new(),
            stats: QueryStats::default(),
            started: 0,
            broken: false,
        };
        loop {
            let batch: Vec<Row> = (&mut *input).take(batch_size).collect();
            if batch.is_empty() {
                break;
            }
            batches.run(self, batch, params, interrupt)?;
        }

        let mut result = if after.is_empty() {
            QueryResult { columns: Vec::new(), rows: Vec::new(), stats: QueryStats::default(), plan: None }
        } else {
            let mut columns = batches.columns.clone();
            columns.extend(options.report_status.clone());
            let mut q = unwind_rows(&columns);
            q.extend(after.iter().cloned());
            let q = single(q);
            let mut params = params.clone();
            params.insert(ROWS.into(), Value::List(batches.rows.into_iter().map(Value::List).collect()));
            self.view(|store| {
                let mut ex = self.executor(StoreRef::Read(store), &params);
                ex.interrupt = interrupt.clone();
                crate::run_query(&mut ex, &q)
            })?
        };
        result.stats = batches.stats;
        Ok(result)
    }

    /// Runs `ast` in a transaction of its own and commits it: its raw rows and stats.
    fn commit_batch(
        &self,
        ast: &Query,
        params: &BTreeMap<String, Value>,
        interrupt: Option<Interrupt>,
    ) -> Result<(Vec<Row>, QueryStats)> {
        let mut guard = self.lock_store(interrupt.as_ref())?;
        guard.begin()?;
        let result = {
            let mut ex = self.executor(StoreRef::Write(&mut *guard), params);
            ex.interrupt = interrupt;
            QueryPlan::build(ast, &ex)
                .and_then(|plan| ex.run_plan(&plan))
                .and_then(|raw| ex.check_constraints().map(|()| (raw.rows, ex.stats.clone())))
        };
        match result {
            // A failed commit has already rolled the transaction back.
            Ok(r) => {
                guard.commit()?;
                let old = self.publish(&mut guard);
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
}
