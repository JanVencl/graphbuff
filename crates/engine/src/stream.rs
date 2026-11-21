//! Pull-based execution of read operators.
//!
//! A read-only chain of the plan (everything between two updates) becomes a chain of
//! [`Stream`]s. A stream hands out rows in batches: [`Stream::next`] asks for *at most*
//! `max` rows, and every stream asks its own input for no more than it can use. A
//! `LIMIT 10` therefore asks for 10 rows, which asks the expansion below it for 10, which
//! reads only as much of the adjacency list as it needs. Operators that need all of their
//! input (aggregation, sorting) pull it in batches of [`BATCH`]; aggregation keeps only
//! its groups, not the rows.
//!
//! Streams borrow the store immutably for `'a`; updates run outside of them (see
//! `Executor::run_op`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use std::cmp::Ordering;
use std::ops::Bound;
use graphbuff_core::clock::Instant;

use graphbuff_core::{
    Adjacency, Direction, Error, IndexEntity, NodeId, Path, RelId, Result, Scan, Value, ValueRange,
};
use graphbuff_cypher::ast::{Expr, NodePattern, RelPattern, SortItem, VarLength};

/// What a [`Blocking`] stream does with all of its input: it takes the rows batch by
/// batch, keeping only what it needs, then hands out its own rows.
trait Collect {
    fn add(&mut self, ex: &Executor<'_>, batch: Vec<Row>) -> Result<()>;
    fn finish(&mut self, ex: &Executor<'_>) -> Result<Vec<Row>>;
}

/// Grouping and aggregation: keeps one entry per group, not the input rows.
struct Aggregation(Option<Grouping>);

impl Collect for Aggregation {
    fn add(&mut self, ex: &Executor<'_>, batch: Vec<Row>) -> Result<()> {
        let grouping = self.0.as_mut().expect("not finished");
        for row in batch {
            grouping.add(ex, row)?;
        }
        Ok(())
    }

    fn finish(&mut self, ex: &Executor<'_>) -> Result<Vec<Row>> {
        self.0.take().expect("finished once").finish(ex)
    }
}

/// `ORDER BY`: keeps every row with its sort keys, then sorts them (stably).
struct Sorting<'a> {
    keys: &'a [SortItem],
    items: &'a [Item],
    /// The input's scope and the source scope behind it.
    scope: Scope,
    src: Scope,
    keyed: Vec<(Vec<Value>, Row)>,
}

impl Collect for Sorting<'_> {
    fn add(&mut self, ex: &Executor<'_>, batch: Vec<Row>) -> Result<()> {
        for row in batch {
            let k = ex.sort_keys(self.keys, self.items, &self.scope, &self.src, &row)?;
            self.keyed.push((k, row));
        }
        Ok(())
    }

    fn finish(&mut self, _: &Executor<'_>) -> Result<Vec<Row>> {
        let keys = self.keys;
        let mut keyed = std::mem::take(&mut self.keyed);
        keyed.sort_by(|(a, _), (b, _)| compare_sort_keys(keys, a, b));
        Ok(keyed.into_iter().map(|(_, r)| r).collect())
    }
}

use crate::context::{Env, Executor, Frame, Row, Scope};
use crate::exec::Grouping;
use crate::exec::compare_sort_keys;
use crate::matcher::SeekKind;
use crate::ops::cypher_eq;
use crate::plan::{CompositeSeek, Item, MatchPlan, Operator, SeekSource, Start, Step, profile_key};

/// Rows per batch when a stream reads all of its input.
pub(crate) const BATCH: usize = 512;

pub(crate) type BoxStream<'a> = Box<dyn Stream<'a> + 'a>;

pub(crate) trait Stream<'a> {
    /// Variables of the rows this stream yields.
    fn scope(&self) -> &Scope;

    /// Between a projection and its `Select`: the source scope behind the projected
    /// values (see [`Frame::hidden`]).
    fn hidden(&self) -> Option<&Scope> {
        None
    }

    /// Up to `max` (at least 1) more rows, or `None` once the stream is exhausted.
    /// A returned batch is never empty.
    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>>;
}

/// Builds the stream for the read-only chain ending in `op`. `base` is the materialized
/// result of what is below the chain (the update it starts on, or the single empty row).
pub(crate) fn build<'a>(ex: &'a Executor<'_>, op: &'a Operator, base: Frame) -> Result<BoxStream<'a>> {
    let s = build_op(ex, op, base)?;
    Ok(match (&ex.profile, op) {
        // A non-optional match is measured per step; updates are measured where they run.
        (None, _)
        | (_, Operator::ProcedureCall { .. }) if op.is_update() => s,
        (None, _)
        | (_, Operator::Match { optional: false, .. })
        | (
            _,
            Operator::Create { .. }
            | Operator::Merge { .. }
            | Operator::Set { .. }
            | Operator::Remove { .. }
            | Operator::Delete { .. }
            | Operator::Call { updating: true, .. }
            | Operator::Foreach { .. },
        ) => s,
        _ => Profiled::wrap(s, profile_key(op)),
    })
}

fn build_op<'a>(ex: &'a Executor<'_>, op: &'a Operator, base: Frame) -> Result<BoxStream<'a>> {
    use Operator as O;
    Ok(match op {
        // A write procedure call is an update: the base, run before.
        O::ProcedureCall { .. } if op.is_update() => Box::new(Source {
            scope: base.scope,
            rows: base.rows.into(),
            hidden: None,
        }),
        O::Start
        | O::Create { .. }
        | O::Merge { .. }
        | O::Set { .. }
        | O::Remove { .. }
        | O::Delete { .. }
        | O::Call { updating: true, .. }
        | O::Foreach { .. } => {
            Box::new(Source {
                scope: base.scope,
                rows: base.rows.into(),
                hidden: None,
            })
        }
        O::Match {
            input,
            optional,
            plan,
        } => {
            let input = build(ex, input, base)?;
            build_match(ex, input, plan, *optional)
        }
        O::Kernel { op, fallback, .. } => match crate::accel::run(ex, op) {
            Ok(frame) => Box::new(Source {
                scope: frame.scope,
                rows: frame.rows.into(),
                hidden: frame.hidden,
            }),
            // The kernels cannot answer exactly: run the row operators.
            Err(Error::Unsupported(_)) => build(ex, fallback, base)?,
            Err(e) => return Err(e),
        },
        O::Unwind { input, expr, var } => {
            let input = build(ex, input, base)?;
            let mut scope = input.scope().clone();
            if scope.get(var).is_some() {
                return Err(Error::Semantic(format!("variable `{var}` already declared")));
            }
            let width = scope.len();
            scope.add(var);
            Box::new(Expanding::new(
                input,
                scope.clone(),
                UnwindStep {
                    expr,
                    in_scope: scope,
                    width,
                    row: Vec::new(),
                    items: Vec::new().into_iter(),
                },
            ))
        }
        O::Project { input, star, items } => {
            let input = build(ex, input, base)?;
            let (all, scope) = ex.projection_items(*star, items, input.scope())?;
            let hidden = input.scope().clone();
            Box::new(ProjectStream {
                input,
                all,
                scope,
                hidden,
            })
        }
        O::Aggregate { input, star, items } => {
            let input = build(ex, input, base)?;
            let (grouping, scope) = ex.grouping(*star, items, input.scope())?;
            let hidden = Some(input.scope().clone());
            Box::new(Blocking::new(input, scope, hidden, Aggregation(Some(grouping))))
        }
        O::Sort { input, keys, items } => {
            let input = build(ex, input, base)?;
            let (scope, hidden) = (input.scope().clone(), input.hidden().cloned());
            let sorting = Sorting {
                keys,
                items,
                scope: scope.clone(),
                src: hidden.clone().unwrap_or_default(),
                keyed: Vec::new(),
            };
            Box::new(Blocking::new(input, scope, hidden, sorting))
        }
        O::Top {
            input,
            keys,
            items,
            skip,
            limit,
        } => {
            let input = build(ex, input, base)?;
            let (scope, hidden) = (input.scope().clone(), input.hidden().cloned());
            Box::new(TopStream {
                input: Some(input),
                keys,
                items,
                skip: skip.as_ref(),
                limit,
                scope,
                hidden,
                out: VecDeque::new(),
            })
        }
        O::Distinct { input } => Box::new(DistinctStream {
            input: build(ex, input, base)?,
            seen: HashSet::new(),
        }),
        O::Skip { input, count } => Box::new(SkipStream {
            input: build(ex, input, base)?,
            count,
            left: None,
        }),
        O::Limit { input, count } => Box::new(LimitStream {
            input: build(ex, input, base)?,
            count,
            left: None,
        }),
        O::Select { input } => {
            let input = build(ex, input, base)?;
            let scope = input.scope().clone();
            let k = scope.len();
            Box::new(Map {
                input,
                scope,
                f: move |row: &mut Row| row.truncate(k),
            })
        }
        O::Filter { input, predicate } => Box::new(FilterStream {
            input: build(ex, input, base)?,
            predicate,
        }),
        O::LoadCsv {
            input,
            headers,
            url,
            var,
            terminator,
        } => {
            let input = build(ex, input, base)?;
            let in_scope = input.scope().clone();
            if in_scope.get(var).is_some() {
                return Err(Error::Semantic(format!("variable `{var}` already declared")));
            }
            let mut scope = in_scope.clone();
            let width = scope.len();
            scope.add(var);
            Box::new(Expanding::new(
                input,
                scope,
                CsvStep {
                    url,
                    headers: *headers,
                    terminator: *terminator,
                    in_scope,
                    width,
                    row: Vec::new(),
                    reader: None,
                    header: Vec::new(),
                },
            ))
        }
        O::ProcedureCall {
            input,
            procedure,
            args,
            fields,
            optional,
        } => {
            let input = build(ex, input, base)?;
            let in_scope = input.scope().clone();
            let mut scope = in_scope.clone();
            let width = scope.len();
            for (_, v) in fields {
                if scope.get(v).is_some() {
                    return Err(Error::Semantic(format!("variable `{v}` already declared")));
                }
                scope.add(v);
            }
            Box::new(Expanding::new(
                input,
                scope,
                ProcedureStep {
                    procedure,
                    args,
                    fields,
                    optional: *optional,
                    in_scope,
                    width,
                    out: Vec::new().into_iter(),
                },
            ))
        }
        O::Call {
            input,
            plan,
            imports,
            columns,
            returns,
            updating: false,
            optional,
        } => {
            let input = build(ex, input, base)?;
            let mut scope = input.scope().clone();
            let slots: Vec<usize> = imports
                .iter()
                .map(|v| scope.get(v).ok_or_else(|| Error::Semantic(format!("variable `{v}` not defined"))))
                .collect::<Result<_>>()?;
            let width = scope.len();
            for c in columns {
                scope.add(c);
            }
            Box::new(Expanding::new(
                input,
                scope,
                CallStep {
                    plan,
                    slots,
                    arg_scope: Scope::from_names(imports.iter().cloned())?,
                    columns,
                    returns: *returns,
                    optional: *optional,
                    width,
                    out: Vec::new().into_iter(),
                },
            ))
        }
    })
}

/// `CALL proc(args) YIELD …`: the procedure's rows per input row.
struct ProcedureStep<'a> {
    procedure: &'a crate::procedures::Procedure,
    args: &'a [Expr],
    fields: &'a [(usize, String)],
    optional: bool,
    in_scope: Scope,
    width: usize,
    out: std::vec::IntoIter<Row>,
}

impl<'a> Expander<'a> for ProcedureStep<'a> {
    fn begin(&mut self, ex: &'a Executor<'_>, mut row: Row) -> Result<()> {
        row.truncate(self.width);
        let env = Env::new(&self.in_scope, &row);
        let args: Vec<Value> = self.args.iter().map(|a| ex.eval(a, &env)).collect::<Result<_>>()?;
        let ctx = crate::procedures::ProcedureContext {
            store: ex.store(),
            procedures: &ex.procedures,
            accel: ex.accel.as_ref(),
            interrupt: ex.interrupt.as_ref(),
            writes: None,
        };
        let rows = self.procedure.call(&ctx, &args)?;
        self.out = if self.procedure.signature.outputs.is_empty() {
            vec![row].into_iter()
        } else if self.optional && rows.is_empty() {
            row.extend(self.fields.iter().map(|_| Value::Null));
            vec![row].into_iter()
        } else {
            rows.into_iter()
                .map(|r| {
                    let mut o = row.clone();
                    o.extend(self.fields.iter().map(|(i, _)| r[*i].clone()));
                    o
                })
                .collect::<Vec<_>>()
                .into_iter()
        };
        Ok(())
    }

    fn next_row(&mut self, _: &'a Executor<'_>) -> Result<Option<Row>> {
        Ok(self.out.next())
    }
}

/// `LOAD CSV`: the records of the file named per input row.
struct CsvStep<'a> {
    url: &'a Expr,
    headers: bool,
    terminator: u8,
    in_scope: Scope,
    width: usize,
    row: Row,
    reader: Option<crate::csv::CsvReader>,
    header: Vec<String>,
}

impl<'a> Expander<'a> for CsvStep<'a> {
    fn begin(&mut self, ex: &'a Executor<'_>, mut row: Row) -> Result<()> {
        row.truncate(self.width);
        self.reader = None;
        let url = match ex.eval(self.url, &Env::new(&self.in_scope, &row))? {
            Value::String(s) => s,
            other => {
                return Err(Error::Type(format!(
                    "LOAD CSV needs a string URL, got {}",
                    other.type_name()
                )));
            }
        };
        let source = crate::csv::resolve(ex.import_dir.as_deref(), ex.csv_urls.as_ref(), &url)?;
        let mut reader = crate::csv::open(source, ex.csv_urls.as_ref(), self.terminator)?;
        if self.headers {
            match reader.next_record()? {
                Some(h) => self.header = h.into_iter().map(|(s, _)| s).collect(),
                None => return Ok(()), // an empty file: no rows
            }
        }
        self.reader = Some(reader);
        self.row = row;
        Ok(())
    }

    fn next_row(&mut self, _: &'a Executor<'_>) -> Result<Option<Row>> {
        let Some(reader) = &mut self.reader else {
            return Ok(None);
        };
        let Some(record) = reader.next_record()? else {
            return Ok(None);
        };
        // An unquoted empty field is null; `""` is an empty string.
        let value = |(s, quoted): (String, bool)| {
            if s.is_empty() && !quoted { Value::Null } else { Value::String(s) }
        };
        let v = if self.headers {
            let mut map = std::collections::BTreeMap::new();
            let mut fields = record.into_iter();
            for h in &self.header {
                map.insert(h.clone(), fields.next().map(value).unwrap_or(Value::Null));
            }
            Value::Map(map)
        } else {
            Value::List(record.into_iter().map(value).collect())
        };
        let mut r = self.row.clone();
        r.push(v);
        Ok(Some(r))
    }
}

/// A read-only `CALL { … }` subquery, run for each input row.
struct CallStep<'a> {
    plan: &'a crate::plan::QueryPlan,
    /// Input slots of the imported variables.
    slots: Vec<usize>,
    arg_scope: Scope,
    columns: &'a [String],
    returns: bool,
    optional: bool,
    /// Width of the input rows.
    width: usize,
    out: std::vec::IntoIter<Row>,
}

impl<'a> Expander<'a> for CallStep<'a> {
    fn begin(&mut self, ex: &'a Executor<'_>, mut row: Row) -> Result<()> {
        row.truncate(self.width);
        if !self.returns {
            self.out = vec![row].into_iter();
            return Ok(());
        }
        let arg: Row = self.slots.iter().map(|&i| row[i].clone()).collect();
        let (names, rows) = ex.stream_subquery(self.plan, &self.arg_scope, &arg, usize::MAX)?;
        if self.optional && rows.is_empty() {
            row.extend(self.columns.iter().map(|_| Value::Null));
            self.out = vec![row].into_iter();
            return Ok(());
        }
        let pos: Vec<usize> = self
            .columns
            .iter()
            .map(|c| names.iter().position(|x| x == c).expect("planned columns are returned"))
            .collect();
        self.out = rows
            .into_iter()
            .map(|r| {
                let mut o = row.clone();
                o.extend(pos.iter().map(|&i| r[i].clone()));
                o
            })
            .collect::<Vec<_>>()
            .into_iter();
        Ok(())
    }

    fn next_row(&mut self, _: &'a Executor<'_>) -> Result<Option<Row>> {
        Ok(self.out.next())
    }
}

// ---- basic streams ------------------------------------------------------------------------

/// Rows that are already materialized.
struct Source {
    scope: Scope,
    rows: VecDeque<Row>,
    hidden: Option<Scope>,
}

impl<'a> Stream<'a> for Source {
    fn scope(&self) -> &Scope {
        &self.scope
    }

    fn hidden(&self) -> Option<&Scope> {
        self.hidden.as_ref()
    }

    fn next(&mut self, _: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        if self.rows.is_empty() {
            return Ok(None);
        }
        let n = max.min(self.rows.len());
        Ok(Some(self.rows.drain(..n).collect()))
    }
}

/// A one-to-one row transformation that cannot fail.
struct Map<'a, F> {
    input: BoxStream<'a>,
    scope: Scope,
    f: F,
}

impl<'a, F: FnMut(&mut Row)> Stream<'a> for Map<'a, F> {
    fn scope(&self) -> &Scope {
        &self.scope
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        let Some(mut batch) = self.input.next(ex, max)? else {
            return Ok(None);
        };
        for row in &mut batch {
            (self.f)(row);
        }
        Ok(Some(batch))
    }
}

/// Binds a path variable (see [`build_path`]).
struct PathStream<'a> {
    input: BoxStream<'a>,
    target: usize,
    nodes: Vec<usize>,
    rels: Vec<(usize, bool)>,
}

impl<'a> Stream<'a> for PathStream<'a> {
    fn scope(&self) -> &Scope {
        self.input.scope()
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        let Some(mut batch) = self.input.next(ex, max)? else {
            return Ok(None);
        };
        for row in &mut batch {
            let path = build_path(ex, row, &self.nodes, &self.rels)?;
            row[self.target] = Value::Path(path);
        }
        Ok(Some(batch))
    }
}

/// A row filter; asks its input for `max` rows at a time until one passes.
struct FilterStream<'a> {
    input: BoxStream<'a>,
    predicate: &'a Expr,
}

impl<'a> Stream<'a> for FilterStream<'a> {
    fn scope(&self) -> &Scope {
        self.input.scope()
    }

    fn hidden(&self) -> Option<&Scope> {
        self.input.hidden()
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        loop {
            let Some(batch) = self.input.next(ex, max)? else {
                return Ok(None);
            };
            let scope = self.input.scope();
            let hidden = self.input.hidden();
            let mut out = Vec::new();
            for row in batch {
                let keep = match hidden {
                    // Between a projection and its `Select`: the projected values
                    // shadow the source row's variables.
                    Some(src) => {
                        let k = scope.len();
                        let mut env = Env::new(src, &row[k..]);
                        env.locals = scope.names().iter().cloned().zip(row[..k].iter().cloned()).collect();
                        ex.eval_predicate(self.predicate, &env)?
                    }
                    None => ex.eval_predicate(self.predicate, &Env::new(scope, &row))?,
                };
                if keep {
                    out.push(row);
                }
            }
            if !out.is_empty() {
                return Ok(Some(out));
            }
        }
    }
}

/// Projected values followed by the source row (hidden until `Select`).
struct ProjectStream<'a> {
    input: BoxStream<'a>,
    all: Vec<Item>,
    scope: Scope,
    hidden: Scope,
}

impl<'a> Stream<'a> for ProjectStream<'a> {
    fn scope(&self) -> &Scope {
        &self.scope
    }

    fn hidden(&self) -> Option<&Scope> {
        Some(&self.hidden)
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        let Some(batch) = self.input.next(ex, max)? else {
            return Ok(None);
        };
        let mut out = Vec::with_capacity(batch.len());
        for row in batch {
            let mut vals = {
                let env = Env::new(&self.hidden, &row);
                self.all
                    .iter()
                    .map(|(_, e)| ex.eval(e, &env))
                    .collect::<Result<Row>>()?
            };
            vals.extend(row);
            out.push(vals);
        }
        Ok(Some(out))
    }
}

/// Reads all of its input, then hands out the rows `op` makes of it.
struct Blocking<'a, W> {
    input: Option<BoxStream<'a>>,
    op: W,
    scope: Scope,
    hidden: Option<Scope>,
    out: VecDeque<Row>,
}

impl<'a, W> Blocking<'a, W> {
    fn new(input: BoxStream<'a>, scope: Scope, hidden: Option<Scope>, op: W) -> Self {
        Blocking {
            input: Some(input),
            op,
            scope,
            hidden,
            out: VecDeque::new(),
        }
    }
}

impl<'a, W: Collect> Stream<'a> for Blocking<'a, W> {
    fn scope(&self) -> &Scope {
        &self.scope
    }

    fn hidden(&self) -> Option<&Scope> {
        self.hidden.as_ref()
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        if let Some(mut input) = self.input.take() {
            while let Some(batch) = input.next(ex, BATCH)? {
                self.op.add(ex, batch)?;
            }
            self.out = self.op.finish(ex)?.into();
        }
        if self.out.is_empty() {
            return Ok(None);
        }
        let n = max.min(self.out.len());
        Ok(Some(self.out.drain(..n).collect()))
    }
}

/// `ORDER BY … LIMIT`: keeps only the best `skip + limit` rows of its input. Rows are
/// buffered with their sort keys and input position; whenever the buffer holds twice as
/// many as needed (at least [`BATCH`]), it is cut back to the best ones in linear time.
/// The input position breaks ties, so the result equals a stable sort's first rows.
struct TopStream<'a> {
    input: Option<BoxStream<'a>>,
    keys: &'a [SortItem],
    items: &'a [Item],
    skip: Option<&'a Expr>,
    limit: &'a Expr,
    scope: Scope,
    hidden: Option<Scope>,
    out: VecDeque<Row>,
}

impl<'a> TopStream<'a> {
    fn fill(&mut self, ex: &'a Executor<'_>, mut input: BoxStream<'a>) -> Result<()> {
        let skip = ex.eval_count(self.skip, "SKIP")?.unwrap_or(0);
        let limit = ex.eval_count(Some(self.limit), "LIMIT")?.unwrap_or(0);
        let n = skip.saturating_add(limit);
        if n == 0 {
            return Ok(());
        }
        let keys = self.keys;
        let cmp = |a: &(Vec<Value>, usize, Row), b: &(Vec<Value>, usize, Row)| {
            compare_sort_keys(keys, &a.0, &b.0).then(a.1.cmp(&b.1))
        };
        let cap = n.saturating_mul(2).max(BATCH);
        let src = input.hidden().cloned().unwrap_or_default();
        let mut buf: Vec<(Vec<Value>, usize, Row)> = Vec::new();
        let mut seq = 0usize;
        while let Some(batch) = input.next(ex, BATCH)? {
            for row in batch {
                let k = ex.sort_keys(keys, self.items, input.scope(), &src, &row)?;
                buf.push((k, seq, row));
                seq += 1;
            }
            if buf.len() >= cap && buf.len() > n {
                buf.select_nth_unstable_by(n, cmp);
                buf.truncate(n);
            }
        }
        buf.sort_unstable_by(cmp);
        buf.truncate(n);
        self.out = buf.into_iter().map(|(_, _, row)| row).collect();
        Ok(())
    }
}

impl<'a> Stream<'a> for TopStream<'a> {
    fn scope(&self) -> &Scope {
        &self.scope
    }

    fn hidden(&self) -> Option<&Scope> {
        self.hidden.as_ref()
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        if let Some(input) = self.input.take() {
            self.fill(ex, input)?;
        }
        if self.out.is_empty() {
            return Ok(None);
        }
        let n = max.min(self.out.len());
        Ok(Some(self.out.drain(..n).collect()))
    }
}

/// Keeps the first row of every distinct set of projected values.
struct DistinctStream<'a> {
    input: BoxStream<'a>,
    seen: HashSet<Row>,
}

impl<'a> Stream<'a> for DistinctStream<'a> {
    fn scope(&self) -> &Scope {
        self.input.scope()
    }

    fn hidden(&self) -> Option<&Scope> {
        self.input.hidden()
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        loop {
            let Some(batch) = self.input.next(ex, max)? else {
                return Ok(None);
            };
            let k = self.input.scope().len();
            let out: Vec<Row> = batch
                .into_iter()
                .filter(|r| self.seen.insert(r[..k].to_vec()))
                .collect();
            if !out.is_empty() {
                return Ok(Some(out));
            }
        }
    }
}

struct SkipStream<'a> {
    input: BoxStream<'a>,
    count: &'a Expr,
    /// Rows still to skip, once the count is evaluated.
    left: Option<usize>,
}

impl<'a> Stream<'a> for SkipStream<'a> {
    fn scope(&self) -> &Scope {
        self.input.scope()
    }

    fn hidden(&self) -> Option<&Scope> {
        self.input.hidden()
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        if self.left.is_none() {
            self.left = Some(ex.eval_count(Some(self.count), "SKIP")?.unwrap_or(0));
        }
        loop {
            let left = self.left.unwrap_or(0);
            let Some(mut batch) = self.input.next(ex, max.saturating_add(left))? else {
                return Ok(None);
            };
            let n = left.min(batch.len());
            batch.drain(..n);
            self.left = Some(left - n);
            if !batch.is_empty() {
                return Ok(Some(batch));
            }
        }
    }
}

struct LimitStream<'a> {
    input: BoxStream<'a>,
    count: &'a Expr,
    /// Rows still allowed, once the count is evaluated.
    left: Option<usize>,
}

impl<'a> Stream<'a> for LimitStream<'a> {
    fn scope(&self) -> &Scope {
        self.input.scope()
    }

    fn hidden(&self) -> Option<&Scope> {
        self.input.hidden()
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        if self.left.is_none() {
            self.left = Some(
                ex.eval_count(Some(self.count), "LIMIT")?
                    .unwrap_or(usize::MAX),
            );
        }
        let left = self.left.unwrap_or(0);
        if left == 0 {
            return Ok(None);
        }
        let Some(mut batch) = self.input.next(ex, max.min(left))? else {
            return Ok(None);
        };
        batch.truncate(left);
        self.left = Some(left - batch.len());
        Ok(Some(batch))
    }
}

// ---- per-row expansion ---------------------------------------------------------------------

/// A step that turns each input row into zero or more output rows, lazily.
trait Expander<'a> {
    fn begin(&mut self, ex: &'a Executor<'_>, row: Row) -> Result<()>;
    /// The next output row for the current input row, or `None` when it has no more.
    fn next_row(&mut self, ex: &'a Executor<'_>) -> Result<Option<Row>>;
}

/// Drives an [`Expander`] over the rows of its input.
struct Expanding<'a, E> {
    input: BoxStream<'a>,
    scope: Scope,
    step: E,
    pending: VecDeque<Row>,
    active: bool,
    input_done: bool,
}

impl<'a, E> Expanding<'a, E> {
    fn new(input: BoxStream<'a>, scope: Scope, step: E) -> Self {
        Expanding {
            input,
            scope,
            step,
            pending: VecDeque::new(),
            active: false,
            input_done: false,
        }
    }
}

impl<'a, E: Expander<'a>> Stream<'a> for Expanding<'a, E> {
    fn scope(&self) -> &Scope {
        &self.scope
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        let mut out = Vec::new();
        while out.len() < max {
            ex.check_interrupt()?;
            if self.active {
                match self.step.next_row(ex)? {
                    Some(r) => {
                        out.push(r);
                        continue;
                    }
                    None => self.active = false,
                }
            }
            let row = match self.pending.pop_front() {
                Some(r) => r,
                None if self.input_done => break,
                None => {
                    match self.input.next(ex, max - out.len())? {
                        Some(batch) => self.pending.extend(batch),
                        None => self.input_done = true,
                    }
                    continue;
                }
            };
            self.step.begin(ex, row)?;
            self.active = true;
        }
        Ok((!out.is_empty()).then_some(out))
    }
}

struct UnwindStep<'a> {
    expr: &'a Expr,
    in_scope: Scope,
    width: usize,
    row: Row,
    items: std::vec::IntoIter<Value>,
}

impl<'a> Expander<'a> for UnwindStep<'a> {
    fn begin(&mut self, ex: &'a Executor<'_>, row: Row) -> Result<()> {
        let v = ex.eval(self.expr, &Env::new(&self.in_scope, &row[..self.width]))?;
        self.items = match v {
            Value::Null => Vec::new(),
            Value::List(items) => items,
            other => vec![other],
        }
        .into_iter();
        self.row = row;
        Ok(())
    }

    fn next_row(&mut self, _: &'a Executor<'_>) -> Result<Option<Row>> {
        Ok(self.items.next().map(|item| {
            let mut r = self.row.clone();
            r.push(item);
            r
        }))
    }
}

// ---- MATCH ----------------------------------------------------------------------------------

fn build_match<'a>(ex: &'a Executor<'_>, input: BoxStream<'a>, plan: &'a MatchPlan, optional: bool) -> BoxStream<'a> {
    let mut scope = input.scope().clone();
    let in_width = scope.len();
    for n in &plan.named {
        scope.add(n);
    }
    let keep = scope.len();
    for h in &plan.hidden {
        scope.add(h);
    }
    let out_scope = Scope::from_names(scope.names()[..keep].iter().cloned())
        .expect("scope names are unique");
    let cache = JoinCache::default();
    if optional {
        return Box::new(OptionalStream {
            input,
            plan,
            scope: out_scope,
            inner: scope,
            in_width,
            cache,
            current: None,
            pending: VecDeque::new(),
            input_done: false,
        });
    }
    let width = scope.len();
    let widened: BoxStream<'a> = Box::new(Map {
        input,
        scope: scope.clone(),
        f: move |row: &mut Row| row.resize(width, Value::Null),
    });
    let steps = build_steps(ex, widened, &plan.steps, &scope, &cache);
    Box::new(Map {
        input: steps,
        scope: out_scope,
        f: move |row: &mut Row| row.truncate(keep),
    })
}

fn build_steps<'a>(
    ex: &'a Executor<'_>,
    mut s: BoxStream<'a>,
    steps: &'a [Step],
    scope: &Scope,
    cache: &JoinCache,
) -> BoxStream<'a> {
    let slot = |name: &str| scope.get(name).expect("planned variables are in scope");
    for step in steps {
        s = match step {
            Step::NodeStart { var, node, start } => Box::new(Expanding::new(
                s,
                scope.clone(),
                NodeStartStep {
                    scope: scope.clone(),
                    slot: slot(var),
                    var,
                    node,
                    start,
                    row: Vec::new(),
                    cands: None,
                },
            )),
            Step::RelSeek {
                rel,
                left,
                right,
                pattern,
                left_node,
                right_node,
                dir,
                sources,
                composite,
                used,
            } => Box::new(Expanding::new(
                s,
                scope.clone(),
                RelSeekStep {
                    scope: scope.clone(),
                    rel: slot(rel),
                    left: slot(left),
                    left_var: left,
                    right: slot(right),
                    right_var: right,
                    pattern,
                    left_node,
                    right_node,
                    dir: *dir,
                    sources,
                    composite,
                    used: used.iter().map(|u| slot(u)).collect(),
                    row: Vec::new(),
                    cands: None,
                    pending: None,
                },
            )),
            Step::Expand {
                from,
                rel,
                to,
                pattern,
                node,
                dir,
                rel_bound,
                to_bound,
                used,
                reversed,
            } => {
                let common = ExpandCommon {
                    scope: scope.clone(),
                    from: slot(from),
                    rel: slot(rel),
                    rel_var: rel,
                    to: slot(to),
                    to_var: to,
                    pattern,
                    node,
                    dir: *dir,
                    rel_bound: *rel_bound,
                    to_bound: *to_bound,
                    used: used.iter().map(|u| slot(u)).collect(),
                };
                match pattern.length {
                    None => Box::new(Expanding::new(
                        s,
                        scope.clone(),
                        ExpandStep {
                            c: common,
                            row: Vec::new(),
                            adj: None,
                            only: None,
                        },
                    )),
                    Some(VarLength { min, max }) => Box::new(Expanding::new(
                        s,
                        scope.clone(),
                        VarExpandStep {
                            c: common,
                            min,
                            max,
                            reversed: *reversed,
                            row: Vec::new(),
                            stack: Vec::new(),
                            rels: Vec::new(),
                            visit: None,
                        },
                    )),
                }
            }
            Step::BuildPath { var, nodes, rels } => Box::new(PathStream {
                input: s,
                target: slot(var),
                nodes: nodes.iter().map(|n| slot(n)).collect(),
                rels: rels.iter().map(|(r, v)| (slot(r), *v)).collect(),
            }),
            Step::Join {
                right,
                keys,
                left_rels,
                right_rels,
                ..
            } => {
                let mut copy: Vec<usize> = right.iter().flat_map(Step::binds).map(slot).collect();
                copy.sort_unstable();
                copy.dedup();
                Box::new(Expanding::new(
                    s,
                    scope.clone(),
                    JoinStep {
                        scope: scope.clone(),
                        right,
                        keys,
                        left_rels: left_rels.iter().map(|r| slot(r)).collect(),
                        right_rels: right_rels.iter().map(|r| slot(r)).collect(),
                        copy,
                        cache: cache.clone(),
                        id: step as *const Step as usize,
                        built: None,
                        row: Vec::new(),
                        cands: Vec::new(),
                        pos: 0,
                        left_keys: Vec::new(),
                    },
                ))
            }
            Step::ShortestPath {
                from,
                to,
                rel,
                path,
                pattern,
                dir,
                all,
                used,
                predicates,
            } => {
                let (min, max) = match pattern.length {
                    None => (1, Some(1)),
                    Some(VarLength { min, max }) => (min, max),
                };
                Box::new(Expanding::new(
                    s,
                    scope.clone(),
                    ShortestStep {
                        scope: scope.clone(),
                        from: slot(from),
                        from_var: from,
                        to: slot(to),
                        to_var: to,
                        rel: slot(rel),
                        list: pattern.length.is_some(),
                        path: path.as_deref().map(slot),
                        pattern,
                        dir: *dir,
                        all: *all,
                        min,
                        max,
                        used: used.iter().map(|u| slot(u)).collect(),
                        predicates,
                        row: Vec::new(),
                        found: Vec::new().into_iter(),
                    },
                ))
            }
            Step::Filter(w) => Box::new(FilterStream {
                input: s,
                predicate: w,
            }),
        };
        if ex.profile.is_some() {
            s = Profiled::wrap(s, profile_key(step));
        }
    }
    s
}

/// Under `PROFILE`: counts the rows a stream yields and the time spent in it (with its
/// inputs), into [`Executor::profile`].
struct Profiled<'a> {
    input: BoxStream<'a>,
    key: usize,
}

impl<'a> Profiled<'a> {
    fn wrap(input: BoxStream<'a>, key: usize) -> BoxStream<'a> {
        Box::new(Profiled { input, key })
    }
}

impl<'a> Stream<'a> for Profiled<'a> {
    fn scope(&self) -> &Scope {
        self.input.scope()
    }

    fn hidden(&self) -> Option<&Scope> {
        self.input.hidden()
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        let t = Instant::now();
        let out = self.input.next(ex, max);
        let rows = match &out {
            Ok(Some(batch)) => batch.len(),
            _ => 0,
        };
        ex.record(self.key, rows, t.elapsed());
        out
    }
}

// ---- joins ----------------------------------------------------------------------------------

/// Build sides of the joins of one `MATCH`, by step address. They depend on nothing bound
/// before them, so an `OPTIONAL MATCH` builds each once, not once per input row.
#[derive(Clone, Default)]
struct JoinCache(Rc<RefCell<HashMap<usize, Rc<Built>>>>);

/// A materialized build side: its rows (full width, only its own columns set), each row's
/// key values, and the rows by normalized key.
struct Built {
    rows: Vec<Row>,
    keys: Vec<Vec<Value>>,
    index: HashMap<Vec<Value>, Vec<usize>>,
}

/// A hash key under which Cypher-equal values meet: integral floats become integers
/// (`1 = 1.0`), inside lists and maps too. Candidates are re-checked with `=`.
fn hash_key(v: &Value) -> Value {
    match v {
        Value::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15 => Value::Int(*f as i64),
        Value::List(items) => Value::List(items.iter().map(hash_key).collect()),
        Value::Map(m) => Value::Map(m.iter().map(|(k, v)| (k.clone(), hash_key(v))).collect()),
        other => other.clone(),
    }
}

/// Every relationship a relationship column holds (one, or a variable-length list).
fn rels_in(v: &Value) -> Vec<RelId> {
    match v {
        Value::Rel(r) => vec![*r],
        Value::List(items) => items
            .iter()
            .filter_map(|i| match i {
                Value::Rel(r) => Some(*r),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

struct JoinStep<'a> {
    scope: Scope,
    right: &'a [Step],
    keys: &'a [(Expr, Expr)],
    left_rels: Vec<usize>,
    right_rels: Vec<usize>,
    /// Columns the build side binds, copied into the combined row.
    copy: Vec<usize>,
    cache: JoinCache,
    id: usize,
    built: Option<Rc<Built>>,
    row: Row,
    left_keys: Vec<Value>,
    cands: Vec<usize>,
    pos: usize,
}

impl<'a> JoinStep<'a> {
    fn build(&self, ex: &'a Executor<'_>) -> Result<Rc<Built>> {
        if let Some(b) = self.cache.0.borrow().get(&self.id) {
            return Ok(Rc::clone(b));
        }
        let source: BoxStream<'a> = Box::new(Source {
            scope: self.scope.clone(),
            rows: VecDeque::from([vec![Value::Null; self.scope.len()]]),
            hidden: None,
        });
        let mut stream = build_steps(ex, source, self.right, &self.scope, &self.cache);
        let mut rows = Vec::new();
        while let Some(batch) = stream.next(ex, BATCH)? {
            rows.extend(batch);
        }
        let mut keys = Vec::with_capacity(rows.len());
        let mut index: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
        for (i, row) in rows.iter().enumerate() {
            let env = Env::new(&self.scope, row);
            let k = self
                .keys
                .iter()
                .map(|(_, r)| ex.eval(r, &env))
                .collect::<Result<Vec<_>>>()?;
            index.entry(k.iter().map(hash_key).collect()).or_default().push(i);
            keys.push(k);
        }
        let built = Rc::new(Built { rows, keys, index });
        self.cache.0.borrow_mut().insert(self.id, Rc::clone(&built));
        Ok(built)
    }
}

impl<'a> Expander<'a> for JoinStep<'a> {
    fn begin(&mut self, ex: &'a Executor<'_>, row: Row) -> Result<()> {
        // Built on the first row: a join that gets no rows never runs its build side.
        let built = match &self.built {
            Some(b) => Rc::clone(b),
            None => {
                let b = self.build(ex)?;
                self.built = Some(Rc::clone(&b));
                b
            }
        };
        let env = Env::new(&self.scope, &row);
        self.left_keys = self
            .keys
            .iter()
            .map(|(l, _)| ex.eval(l, &env))
            .collect::<Result<Vec<_>>>()?;
        self.cands = if self.keys.is_empty() {
            (0..built.rows.len()).collect()
        } else {
            let k: Vec<Value> = self.left_keys.iter().map(hash_key).collect();
            built.index.get(&k).cloned().unwrap_or_default()
        };
        self.pos = 0;
        self.row = row;
        Ok(())
    }

    fn next_row(&mut self, ex: &'a Executor<'_>) -> Result<Option<Row>> {
        let built = self.built.as_ref().expect("built in begin");
        let used: Vec<RelId> = self.left_rels.iter().flat_map(|&i| rels_in(&self.row[i])).collect();
        while self.pos < self.cands.len() {
            ex.check_interrupt()?;
            let i = self.cands[self.pos];
            self.pos += 1;
            let equal = self
                .left_keys
                .iter()
                .zip(&built.keys[i])
                .all(|(l, r)| cypher_eq(l, r) == Some(true));
            if !equal {
                continue;
            }
            let right = &built.rows[i];
            if self
                .right_rels
                .iter()
                .flat_map(|&s| rels_in(&right[s]))
                .any(|r| used.contains(&r))
            {
                continue; // a relationship may appear only once in a MATCH
            }
            let mut out = self.row.clone();
            for &s in &self.copy {
                out[s] = right[s].clone();
            }
            return Ok(Some(out));
        }
        Ok(None)
    }
}

/// `OPTIONAL MATCH`: runs the steps once per input row; a row without any match is kept
/// with the new variables null.
struct OptionalStream<'a> {
    input: BoxStream<'a>,
    plan: &'a MatchPlan,
    /// The scope this stream yields, and the one its steps run in (with hidden columns).
    scope: Scope,
    inner: Scope,
    in_width: usize,
    cache: JoinCache,
    /// The steps for the current input row, the row, and whether it matched.
    current: Option<(BoxStream<'a>, Row, bool)>,
    pending: VecDeque<Row>,
    input_done: bool,
}

impl<'a> Stream<'a> for OptionalStream<'a> {
    fn scope(&self) -> &Scope {
        &self.scope
    }

    fn next(&mut self, ex: &'a Executor<'_>, max: usize) -> Result<Option<Vec<Row>>> {
        let keep = self.scope.len();
        let mut out = Vec::new();
        while out.len() < max {
            if let Some((steps, base, matched)) = &mut self.current {
                match steps.next(ex, max - out.len())? {
                    Some(batch) => {
                        *matched = true;
                        out.extend(batch.into_iter().map(|mut r| {
                            r.truncate(keep);
                            r
                        }));
                    }
                    None => {
                        if !*matched {
                            let mut r = std::mem::take(base);
                            r.resize(keep, Value::Null);
                            out.push(r);
                        }
                        self.current = None;
                    }
                }
                continue;
            }
            let row = match self.pending.pop_front() {
                Some(r) => r,
                None if self.input_done => break,
                None => {
                    match self.input.next(ex, max - out.len())? {
                        Some(batch) => self.pending.extend(batch),
                        None => self.input_done = true,
                    }
                    continue;
                }
            };
            debug_assert_eq!(row.len(), self.in_width);
            let mut wide = row.clone();
            wide.resize(self.inner.len(), Value::Null);
            let source: BoxStream<'a> = Box::new(Source {
                scope: self.inner.clone(),
                rows: VecDeque::from([wide]),
                hidden: None,
            });
            self.current = Some((
                build_steps(ex, source, &self.plan.steps, &self.inner, &self.cache),
                row,
                false,
            ));
        }
        Ok((!out.is_empty()).then_some(out))
    }
}

/// The tighter of two lower (or upper) bounds `(value, inclusive)`; `None` if they are
/// of different kinds (a number and a string), so nothing can satisfy both.
fn tighter(old: Option<(Value, bool)>, new: (Value, bool), lower: bool) -> Option<(Value, bool)> {
    let Some(old) = old else {
        return Some(new);
    };
    let num = |v: &Value| match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    };
    let ord = match (&old.0, &new.0) {
        (Value::String(a), Value::String(b)) => b.cmp(a),
        (a, b) => num(b)?.partial_cmp(&num(a)?).unwrap_or(Ordering::Equal),
    };
    let ord = if lower { ord } else { ord.reverse() };
    Some(match ord {
        Ordering::Greater => new,
        Ordering::Less => old,
        // Equal values: the exclusive bound is the tighter one.
        Ordering::Equal => (old.0, old.1 && new.1),
    })
}

/// The node in `v`, if any. A bound variable holding something else is a type error.
fn bound_node(v: &Value, var: &str) -> Result<Option<NodeId>> {
    match v {
        Value::Node(id) => Ok(Some(*id)),
        Value::Null => Ok(None),
        other => Err(Error::Type(format!(
            "variable `{var}` is already bound to a {}, expected a node",
            other.type_name()
        ))),
    }
}

/// Whether the property map `expected` (evaluated in the row) matches `get`.
fn props_ok(
    ex: &Executor<'_>,
    expected: &Option<Expr>,
    scope: &Scope,
    row: &[Value],
    get: impl Fn(&str) -> Result<Option<Value>>,
) -> Result<bool> {
    let Some(expr) = expected else {
        return Ok(true);
    };
    let map = ex.eval_map(expr, &Env::new(scope, row))?;
    for (k, v) in &map {
        let actual = get(k)?.unwrap_or_default();
        if cypher_eq(&actual, v) != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether node `id` can be bound to pattern `np`: labels, properties, and, if the
/// variable is already bound (`bound` slot), the same node.
fn node_ok(
    ex: &Executor<'_>,
    id: NodeId,
    np: &NodePattern,
    var: &str,
    bound: Option<usize>,
    scope: &Scope,
    row: &[Value],
) -> Result<bool> {
    if let Some(i) = bound
        && bound_node(&row[i], var)? != Some(id)
    {
        return Ok(false);
    }
    let store = ex.store();
    for l in &np.labels {
        if !store.node_has_label(id, l)? {
            return Ok(false);
        }
    }
    if let Some(e) = &np.label_expr
        && !e.matches(&store.node_labels(id)?)
    {
        return Ok(false);
    }
    props_ok(ex, &np.props, scope, row, |k| store.node_property(id, k))
}

fn rel_ok(ex: &Executor<'_>, id: RelId, rp: &RelPattern, scope: &Scope, row: &[Value]) -> Result<bool> {
    let store = ex.store();
    if let Some(e) = &rp.type_expr {
        let t: Vec<String> = store.relationship_type(id)?.into_iter().collect();
        if !e.matches(&t) {
            return Ok(false);
        }
    }
    props_ok(ex, &rp.props, scope, row, |k| store.relationship_property(id, k))
}

/// Whether relationship `rel` is already used by one of the `used` columns.
fn is_used(row: &[Value], used: &[usize], rel: RelId) -> bool {
    used.iter().any(|&i| match &row[i] {
        Value::Rel(r) => *r == rel,
        Value::List(items) => items.contains(&Value::Rel(rel)),
        _ => false,
    })
}

struct NodeStartStep<'a> {
    scope: Scope,
    slot: usize,
    var: &'a str,
    node: &'a NodePattern,
    start: &'a Start,
    row: Row,
    cands: Option<Scan<'a, NodeId>>,
}

impl<'a> NodeStartStep<'a> {
    fn candidates(&self, ex: &'a Executor<'_>, row: &[Value]) -> Result<Scan<'a, NodeId>> {
        let store = ex.store();
        let scan = |label: &Option<String>| -> Scan<'a, NodeId> {
            match label {
                Some(l) => store.nodes_with_label(l),
                None => store.all_nodes(),
            }
        };
        Ok(match self.start {
            Start::Bound => match bound_node(&row[self.slot], self.var)? {
                Some(id) => Box::new(std::iter::once(Ok(id))),
                None => Box::new(std::iter::empty()),
            },
            Start::LabelScan(l) => store.nodes_with_label(l),
            Start::AllNodes => store.all_nodes(),
            Start::Seek {
                sources,
                composite,
                fallback,
            } => match seek_ids(ex, &self.scope, row, IndexEntity::Node, sources, composite)? {
                Some(ids) => Box::new(ids.into_iter().map(|id| Ok(NodeId(id)))),
                None => scan(fallback),
            },
        })
    }
}

impl<'a> Expander<'a> for NodeStartStep<'a> {
    fn begin(&mut self, ex: &'a Executor<'_>, row: Row) -> Result<()> {
        self.cands = Some(self.candidates(ex, &row)?);
        self.row = row;
        Ok(())
    }

    fn next_row(&mut self, ex: &'a Executor<'_>) -> Result<Option<Row>> {
        let Some(cands) = &mut self.cands else {
            return Ok(None);
        };
        for c in cands {
            ex.check_interrupt()?;
            let c = c?;
            // For a bound start the candidate is the bound node itself.
            if !node_ok(ex, c, self.node, self.var, None, &self.scope, &self.row)? {
                continue;
            }
            let mut r = self.row.clone();
            r[self.slot] = Value::Node(c);
            return Ok(Some(r));
        }
        Ok(None)
    }
}

/// Ids found by the smallest of `sources` and `composite` (index seeks on `entity`),
/// evaluated in `row`; `None` if no seek value could be evaluated.
fn seek_ids(
    ex: &Executor<'_>,
    scope: &Scope,
    row: &[Value],
    entity: IndexEntity,
    sources: &[SeekSource],
    composite: &[CompositeSeek],
) -> Result<Option<Vec<u64>>> {
    let store = ex.store();
    let env = Env::new(scope, row);
    let mut best: Option<Vec<u64>> = None;
    let mut consider = |ids: Vec<u64>| {
        if best.as_ref().is_none_or(|b| ids.len() < b.len()) {
            best = Some(ids);
        }
    };
    // Comparisons on one property, combined: lower bound, upper bound, and
    // whether they can never hold together (a null bound, mixed kinds).
    type Combined<'s> = (&'s str, &'s str, Option<(Value, bool)>, Option<(Value, bool)>, bool);
    let mut ranges: Vec<Combined> = Vec::new();
    for src in sources {
        // Errors are left for the full property / WHERE check to report.
        let Ok(v) = ex.eval(&src.expr, &env) else {
            continue;
        };
        let values = match (src.kind, v) {
            (SeekKind::Eq, v) => vec![v],
            (SeekKind::In, Value::List(items)) => items,
            (SeekKind::In, Value::Null) => Vec::new(),
            (SeekKind::In, _) => continue,
            (SeekKind::StartsWith, Value::Null) => {
                consider(Vec::new());
                continue;
            }
            (SeekKind::StartsWith, Value::String(p)) => {
                let range = ValueRange::Prefix(p);
                if let Some(ids) = store.schema_index_range(entity, &src.label, &src.key, &range)? {
                    consider(ids);
                }
                continue;
            }
            (SeekKind::StartsWith, _) => continue,
            (kind @ (SeekKind::Near | SeekKind::WithinBBox), v) => {
                if let Some(ids) = point_seek(ex, entity, &src.label, &src.key, kind, v)? {
                    consider(ids);
                }
                continue;
            }
            (kind, v) => {
                // Only numbers and strings have range seeks; for anything else (a date,
                // a boolean) the filter decides, and no range is registered: an empty
                // one would wrongly find nothing.
                if !v.is_null() && !matches!(v, Value::Int(_) | Value::Float(_) | Value::String(_)) {
                    continue;
                }
                let i = match ranges
                    .iter()
                    .position(|r| r.0 == src.label && r.1 == src.key)
                {
                    Some(i) => i,
                    None => {
                        ranges.push((src.label.as_str(), src.key.as_str(), None, None, false));
                        ranges.len() - 1
                    }
                };
                let r = &mut ranges[i];
                if v.is_null() {
                    r.4 = true; // a comparison with null is never true
                    continue;
                }
                if !matches!(v, Value::Int(_) | Value::Float(_) | Value::String(_)) {
                    continue; // not seekable; the filter decides
                }
                let inclusive = matches!(kind, SeekKind::Le | SeekKind::Ge);
                let lower = matches!(kind, SeekKind::Gt | SeekKind::Ge);
                let slot = if lower { &mut r.2 } else { &mut r.3 };
                match tighter(slot.take(), (v, inclusive), lower) {
                    Some(b) => *slot = Some(b),
                    None => r.4 = true,
                }
                continue;
            }
        };
        let mut ids = Vec::new();
        for v in &values {
            let keys = std::slice::from_ref(&src.key);
        ids.extend(
            store
                .schema_index_seek(entity, &src.label, keys, std::slice::from_ref(v))?
                .unwrap_or_default(),
        );
        }
        if values.len() > 1 {
            ids.sort_unstable();
            ids.dedup();
        }
        consider(ids);
    }
    for (label, key, lower, upper, never) in ranges {
        if never {
            consider(Vec::new());
            continue;
        }
        let bound = |b: Option<(Value, bool)>| match b {
            Some((v, true)) => Bound::Included(v),
            Some((v, false)) => Bound::Excluded(v),
            None => Bound::Unbounded,
        };
        let range = ValueRange::Between {
            lower: bound(lower),
            upper: bound(upper),
        };
        if range.numeric().is_none() {
            consider(Vec::new()); // a number and a string bound: nothing fits
            continue;
        }
        if let Some(ids) = store.schema_index_range(entity, label, key, &range)? {
            consider(ids);
        }
    }
    for c in composite {
        // Errors are left for the full property / WHERE check to report.
        let Ok(values) = c.exprs.iter().map(|e| ex.eval(e, &env)).collect::<Result<Vec<_>>>() else {
            continue;
        };
        if let Some(ids) = store.schema_index_seek(entity, &c.label, &c.keys, &values)? {
            consider(ids);
        }
    }
    Ok(best)
}

/// The entities a point index seek finds: `value` is `[center, radius]` (`Near`) or
/// `[lowerLeft, upperRight]` (`WithinBBox`). `None` if the seek cannot run: no point
/// index contents to read (inside a transaction, or while writing), or values the
/// filter has to judge (not points and numbers); then the start falls back to a scan.
fn point_seek(
    ex: &Executor<'_>,
    entity: IndexEntity,
    label: &str,
    key: &str,
    kind: SeekKind,
    value: Value,
) -> Result<Option<Vec<u64>>> {
    let Some(accel) = ex.accel.as_ref() else {
        return Ok(None);
    };
    let Value::List(items) = value else { return Ok(None) };
    let [a, b] = items.as_slice() else { return Ok(None) };
    let key = [key.to_string()];
    let Some(def) = accel
        .snapshot
        .indexes()
        .into_iter()
        .find(|d| d.is_point() && d.same_schema(entity, label, &key))
    else {
        return Ok(None);
    };
    let interrupt = || ex.check_interrupt();
    let ids = match (kind, a, b) {
        // A null argument makes the predicate null: nothing passes.
        (_, Value::Null, _) | (_, _, Value::Null) => Vec::new(),
        (SeekKind::Near, Value::Point(center), Value::Int(_) | Value::Float(_)) => {
            let radius = match b {
                Value::Int(n) => *n as f64,
                Value::Float(f) => *f,
                _ => unreachable!(),
            };
            accel.points.get(&accel.snapshot, &def, &interrupt)?.near(center, radius)
        }
        (SeekKind::WithinBBox, Value::Point(lo), Value::Point(hi)) => {
            accel.points.get(&accel.snapshot, &def, &interrupt)?.in_bbox(lo, hi)
        }
        _ => return Ok(None),
    };
    Ok(Some(ids))
}

/// Relationships found by an index seek, bound with both ends (see `Step::RelSeek`).
struct RelSeekStep<'a> {
    scope: Scope,
    rel: usize,
    left: usize,
    left_var: &'a str,
    right: usize,
    right_var: &'a str,
    pattern: &'a RelPattern,
    left_node: &'a NodePattern,
    right_node: &'a NodePattern,
    dir: Direction,
    sources: &'a [SeekSource],
    composite: &'a [CompositeSeek],
    used: Vec<usize>,
    row: Row,
    cands: Option<Scan<'a, RelId>>,
    /// The second way round of an undirected relationship, still to be reported.
    pending: Option<(RelId, NodeId, NodeId)>,
}

impl<'a> RelSeekStep<'a> {
    /// The row for `rel` with ends `left` / `right`, if they match the pattern's nodes.
    fn bind(&self, ex: &Executor<'_>, rel: RelId, left: NodeId, right: NodeId) -> Result<Option<Row>> {
        // `(a)-[r]->(a)`: both ends are one variable, so they must be one node.
        if self.left == self.right && left != right {
            return Ok(None);
        }
        if !node_ok(ex, left, self.left_node, self.left_var, None, &self.scope, &self.row)?
            || !node_ok(ex, right, self.right_node, self.right_var, None, &self.scope, &self.row)?
        {
            return Ok(None);
        }
        let mut r = self.row.clone();
        r[self.rel] = Value::Rel(rel);
        r[self.left] = Value::Node(left);
        r[self.right] = Value::Node(right);
        Ok(Some(r))
    }
}

impl<'a> Expander<'a> for RelSeekStep<'a> {
    fn begin(&mut self, ex: &'a Executor<'_>, row: Row) -> Result<()> {
        self.pending = None;
        let ids = seek_ids(ex, &self.scope, &row, IndexEntity::Relationship, self.sources, self.composite)?;
        self.cands = Some(match ids {
            Some(ids) => Box::new(ids.into_iter().map(|id| Ok(RelId(id)))),
            None => ex.store().all_relationships(), // no seek value: check every one
        });
        self.row = row;
        Ok(())
    }

    fn next_row(&mut self, ex: &'a Executor<'_>) -> Result<Option<Row>> {
        if let Some((rel, left, right)) = self.pending.take()
            && let Some(r) = self.bind(ex, rel, left, right)?
        {
            return Ok(Some(r));
        }
        let store = ex.store();
        while let Some(rel) = self.cands.as_mut().and_then(Iterator::next) {
            ex.check_interrupt()?;
            let rel = rel?;
            if is_used(&self.row, &self.used, rel) {
                continue;
            }
            let Some(t) = store.relationship_type(rel)? else { continue };
            if !self.pattern.types.contains(&t) || !rel_ok(ex, rel, self.pattern, &self.scope, &self.row)? {
                continue;
            }
            let Some((start, end)) = store.relationship_endpoints(rel)? else { continue };
            let (left, right) = match self.dir {
                Direction::Incoming => (end, start),
                _ => (start, end),
            };
            if self.dir == Direction::Both && start != end {
                self.pending = Some((rel, end, start));
            }
            if let Some(r) = self.bind(ex, rel, left, right)? {
                return Ok(Some(r));
            }
            if let Some((rel, left, right)) = self.pending.take()
                && let Some(r) = self.bind(ex, rel, left, right)?
            {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }
}

/// What a single-hop and a variable-length expansion share.
struct ExpandCommon<'a> {
    scope: Scope,
    from: usize,
    rel: usize,
    rel_var: &'a str,
    to: usize,
    to_var: &'a str,
    pattern: &'a RelPattern,
    node: &'a NodePattern,
    dir: graphbuff_core::Direction,
    rel_bound: bool,
    to_bound: bool,
    used: Vec<usize>,
}

impl ExpandCommon<'_> {
    fn start_node(&self, row: &[Value]) -> Option<NodeId> {
        match row[self.from] {
            Value::Node(id) => Some(id),
            _ => None,
        }
    }

    fn target_ok(&self, ex: &Executor<'_>, id: NodeId, row: &[Value]) -> Result<bool> {
        let bound = self.to_bound.then_some(self.to);
        node_ok(ex, id, self.node, self.to_var, bound, &self.scope, row)
    }
}

struct ExpandStep<'a> {
    c: ExpandCommon<'a>,
    row: Row,
    adj: Option<Adjacency<'a>>,
    /// The relationship a bound relationship variable holds.
    only: Option<RelId>,
}

impl<'a> Expander<'a> for ExpandStep<'a> {
    fn begin(&mut self, ex: &'a Executor<'_>, row: Row) -> Result<()> {
        self.adj = None;
        self.only = None;
        if self.c.rel_bound {
            match &row[self.c.rel] {
                Value::Rel(r) => self.only = Some(*r),
                Value::Null => {
                    self.row = row;
                    return Ok(());
                }
                other => {
                    return Err(Error::Type(format!(
                        "variable `{}` is already bound to a {}, expected a relationship",
                        self.c.rel_var,
                        other.type_name()
                    )));
                }
            }
        }
        if let Some(from) = self.c.start_node(&row) {
            self.adj = Some(ex.store().expand(from, self.c.dir, &self.c.pattern.types));
        }
        self.row = row;
        Ok(())
    }

    fn next_row(&mut self, ex: &'a Executor<'_>) -> Result<Option<Row>> {
        let Some(adj) = &mut self.adj else {
            return Ok(None);
        };
        for exp in adj {
            let exp = exp?;
            if is_used(&self.row, &self.c.used, exp.rel) || self.only.is_some_and(|b| b != exp.rel) {
                continue;
            }
            if !rel_ok(ex, exp.rel, self.c.pattern, &self.c.scope, &self.row)?
                || !self.c.target_ok(ex, exp.other, &self.row)?
            {
                continue;
            }
            let mut r = self.row.clone();
            if !self.c.rel_bound {
                r[self.c.rel] = Value::Rel(exp.rel);
            }
            if !self.c.to_bound {
                r[self.c.to] = Value::Node(exp.other);
            }
            return Ok(Some(r));
        }
        Ok(None)
    }
}

/// A depth-first walk over paths of `min..=max` relationships. Paths are reported in
/// pre-order (a path before its extensions), in adjacency order, as before.
struct VarExpandStep<'a> {
    c: ExpandCommon<'a>,
    min: u64,
    max: Option<u64>,
    reversed: bool,
    row: Row,
    /// One adjacency iterator per depth (index = depth).
    stack: Vec<Adjacency<'a>>,
    /// Relationships of the current path, in traversal order.
    rels: Vec<RelId>,
    /// The node just reached, still to be reported and expanded.
    visit: Option<NodeId>,
}

impl<'a> Expander<'a> for VarExpandStep<'a> {
    fn begin(&mut self, _: &'a Executor<'_>, row: Row) -> Result<()> {
        self.stack.clear();
        self.rels.clear();
        self.visit = self.c.start_node(&row);
        self.row = row;
        Ok(())
    }

    fn next_row(&mut self, ex: &'a Executor<'_>) -> Result<Option<Row>> {
        loop {
            ex.check_interrupt()?;
            if let Some(node) = self.visit.take() {
                let depth = self.rels.len() as u64;
                self.stack.push(if self.max.is_some_and(|m| depth >= m) {
                    Box::new(std::iter::empty())
                } else {
                    ex.store().expand(node, self.c.dir, &self.c.pattern.types)
                });
                if depth >= self.min && self.c.target_ok(ex, node, &self.row)? {
                    let mut seg = self.rels.clone();
                    if self.reversed {
                        seg.reverse();
                    }
                    let seg = Value::List(seg.into_iter().map(Value::Rel).collect());
                    if !self.c.rel_bound || self.row[self.c.rel] == seg {
                        let mut r = self.row.clone();
                        if !self.c.rel_bound {
                            r[self.c.rel] = seg;
                        }
                        if !self.c.to_bound {
                            r[self.c.to] = Value::Node(node);
                        }
                        return Ok(Some(r));
                    }
                }
                continue;
            }
            let Some(top) = self.stack.last_mut() else {
                return Ok(None);
            };
            match top.next() {
                None => {
                    self.stack.pop();
                    if !self.stack.is_empty() {
                        self.rels.pop();
                    }
                }
                Some(exp) => {
                    let exp = exp?;
                    if self.rels.contains(&exp.rel)
                        || is_used(&self.row, &self.c.used, exp.rel)
                        || !rel_ok(ex, exp.rel, self.c.pattern, &self.c.scope, &self.row)?
                    {
                        continue;
                    }
                    self.rels.push(exp.rel);
                    self.visit = Some(exp.other);
                }
            }
        }
    }
}

/// `shortestPath` / `allShortestPaths` from the node in `from` to the node in `to`.
///
/// Without predicates: a bidirectional breadth-first search that always grows the side
/// with the smaller frontier by one whole level, and stops at the first level where the
/// sides meet. Every node remembers how it was first reached (for `all`: every way it was
/// reached at that depth), and the paths are read back from the meeting nodes.
///
/// With predicates (`WHERE` conjuncts on the path or its relationships), the shortest path
/// that passes them may be longer than the shortest path. Paths are then tried by length,
/// from the shortest distance up, depth first, pruned by each node's distance to the end.
/// This can take exponential time when the predicates reject many paths.
struct ShortestStep<'a> {
    scope: Scope,
    from: usize,
    from_var: &'a str,
    to: usize,
    to_var: &'a str,
    rel: usize,
    /// The relationship column holds a list (`*` in the pattern).
    list: bool,
    path: Option<usize>,
    pattern: &'a RelPattern,
    dir: graphbuff_core::Direction,
    all: bool,
    min: u64,
    max: Option<u64>,
    used: Vec<usize>,
    predicates: &'a [Expr],
    row: Row,
    found: std::vec::IntoIter<Row>,
}

/// Per node reached by one side of the search: its depth and how it was reached (the
/// previous node and the relationship; none for the side's own end).
type Reached = HashMap<NodeId, (u64, Vec<(NodeId, RelId)>)>;

impl<'a> ShortestStep<'a> {
    /// Relationships from `node` in `dir` that the pattern allows, with their other ends.
    fn neighbors(&self, ex: &Executor<'_>, node: NodeId, dir: graphbuff_core::Direction) -> Result<Vec<(RelId, NodeId)>> {
        ex.check_interrupt()?;
        let mut out = Vec::new();
        for exp in ex.store().expand(node, dir, &self.pattern.types) {
            let exp = exp?;
            if is_used(&self.row, &self.used, exp.rel) || !rel_ok(ex, exp.rel, self.pattern, &self.scope, &self.row)? {
                continue;
            }
            out.push((exp.rel, exp.other));
        }
        Ok(out)
    }

    /// The row with `path` bound, if it passes the predicates.
    fn bind(&self, ex: &Executor<'_>, path: Path) -> Result<Option<Row>> {
        let mut r = self.row.clone();
        r[self.rel] = if self.list {
            Value::List(path.rels.iter().map(|&id| Value::Rel(id)).collect())
        } else {
            match path.rels.first() {
                Some(&id) => Value::Rel(id),
                None => Value::Null,
            }
        };
        if let Some(p) = self.path {
            r[p] = Value::Path(path);
        }
        for e in self.predicates {
            if !ex.eval_predicate(e, &Env::new(&self.scope, &r))? {
                return Ok(None);
            }
        }
        Ok(Some(r))
    }

    fn search(&self, ex: &Executor<'_>, a: NodeId, b: NodeId) -> Result<Vec<Row>> {
        if a == b {
            // Only the empty path; a path back to the start is never the shortest.
            if self.min > 0 {
                return Ok(Vec::new());
            }
            let empty = Path {
                nodes: vec![a],
                rels: Vec::new(),
            };
            return Ok(self.bind(ex, empty)?.into_iter().collect());
        }
        if self.predicates.is_empty() {
            let mut rows = Vec::new();
            for path in self.bfs(ex, a, b)? {
                rows.extend(self.bind(ex, path)?);
            }
            return Ok(rows);
        }
        self.filtered(ex, a, b)
    }

    fn bfs(&self, ex: &Executor<'_>, a: NodeId, b: NodeId) -> Result<Vec<Path>> {
        let mut seen: [Reached; 2] = [HashMap::new(), HashMap::new()];
        seen[0].insert(a, (0, Vec::new()));
        seen[1].insert(b, (0, Vec::new()));
        let mut frontier = [vec![a], vec![b]];
        let mut depth = [0u64; 2];
        loop {
            if self.max.is_some_and(|m| depth[0] + depth[1] >= m) {
                return Ok(Vec::new());
            }
            let side = if frontier[0].len() <= frontier[1].len() { 0 } else { 1 };
            if frontier[side].is_empty() {
                return Ok(Vec::new());
            }
            let d = depth[side] + 1;
            let dir = if side == 0 { self.dir } else { self.dir.reverse() };
            let mut next = Vec::new();
            for &u in &frontier[side] {
                for (r, v) in self.neighbors(ex, u, dir)? {
                    match seen[side].get_mut(&v) {
                        Some((dv, how)) => {
                            if *dv == d && self.all {
                                how.push((u, r));
                            }
                        }
                        None => {
                            seen[side].insert(v, (d, vec![(u, r)]));
                            next.push(v);
                        }
                    }
                }
            }
            depth[side] = d;
            // Nodes of the new level the other side reached: all at the other side's
            // current depth, or the sides would have met at an earlier level.
            let meets: Vec<NodeId> = next.iter().copied().filter(|v| seen[1 - side].contains_key(v)).collect();
            frontier[side] = next;
            if meets.is_empty() {
                continue;
            }
            let mut paths = Vec::new();
            for m in meets {
                let ends = halves(&seen[1], m, self.all);
                for (n0, r0) in halves(&seen[0], m, self.all) {
                    for (n1, r1) in &ends {
                        // n0: m back to a; n1: m on to b.
                        let mut nodes: Vec<NodeId> = n0.iter().rev().copied().collect();
                        nodes.extend(n1.iter().skip(1));
                        let mut rels: Vec<RelId> = r0.iter().rev().copied().collect();
                        rels.extend(r1);
                        paths.push(Path { nodes, rels });
                        if !self.all {
                            return Ok(paths);
                        }
                    }
                }
            }
            return Ok(paths);
        }
    }

    /// The shortest paths that pass the predicates (see the type's documentation).
    fn filtered(&self, ex: &Executor<'_>, a: NodeId, b: NodeId) -> Result<Vec<Row>> {
        // Distances to `b`, and the relationships near it (no path is longer).
        let back = self.dir.reverse();
        let mut dist: HashMap<NodeId, u64> = HashMap::from([(b, 0)]);
        let mut rels: HashSet<RelId> = HashSet::new();
        let mut frontier = vec![b];
        let mut d = 0;
        while !frontier.is_empty() && self.max.is_none_or(|m| d < m) {
            d += 1;
            let mut next = Vec::new();
            for &u in &frontier {
                for (r, v) in self.neighbors(ex, u, back)? {
                    rels.insert(r);
                    if let std::collections::hash_map::Entry::Vacant(e) = dist.entry(v) {
                        e.insert(d);
                        next.push(v);
                    }
                }
            }
            frontier = next;
        }
        let Some(&shortest) = dist.get(&a) else {
            return Ok(Vec::new());
        };
        let longest = self.max.unwrap_or(u64::MAX).min(rels.len() as u64);
        for len in shortest.max(1)..=longest {
            let mut found = Vec::new();
            let mut path = Path {
                nodes: vec![a],
                rels: Vec::new(),
            };
            self.walk(ex, b, len, &dist, &mut path, &mut found)?;
            if !found.is_empty() {
                return Ok(found);
            }
        }
        Ok(Vec::new())
    }

    /// Extends `path` depth first to every path of `len` relationships that ends at `b`
    /// and passes the predicates. Returns `true` to stop (one was found and only one is
    /// wanted).
    fn walk(
        &self,
        ex: &Executor<'_>,
        b: NodeId,
        len: u64,
        dist: &HashMap<NodeId, u64>,
        path: &mut Path,
        found: &mut Vec<Row>,
    ) -> Result<bool> {
        let here = *path.nodes.last().expect("a path has a node");
        let depth = path.rels.len() as u64;
        if depth == len {
            if here == b
                && let Some(row) = self.bind(ex, path.clone())?
            {
                found.push(row);
                return Ok(!self.all);
            }
            return Ok(false);
        }
        for (r, v) in self.neighbors(ex, here, self.dir)? {
            if path.rels.contains(&r) || dist.get(&v).is_none_or(|&dv| depth + 1 + dv > len) {
                continue;
            }
            path.rels.push(r);
            path.nodes.push(v);
            let stop = self.walk(ex, b, len, dist, path, found)?;
            path.rels.pop();
            path.nodes.pop();
            if stop {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The ways back from `m` to the end of one search side: nodes from `m` to that end, and
/// the relationships between them. Only the first way unless `all`.
fn halves(seen: &Reached, m: NodeId, all: bool) -> Vec<(Vec<NodeId>, Vec<RelId>)> {
    let how = &seen[&m].1;
    if how.is_empty() {
        return vec![(vec![m], Vec::new())];
    }
    let mut out = Vec::new();
    for &(prev, r) in if all { &how[..] } else { &how[..1] } {
        for (mut nodes, mut rels) in halves(seen, prev, all) {
            nodes.insert(0, m);
            rels.insert(0, r);
            out.push((nodes, rels));
        }
    }
    out
}

impl<'a> Expander<'a> for ShortestStep<'a> {
    fn begin(&mut self, ex: &'a Executor<'_>, row: Row) -> Result<()> {
        self.row = row;
        let a = bound_node(&self.row[self.from], self.from_var)?;
        let b = bound_node(&self.row[self.to], self.to_var)?;
        self.found = match (a, b) {
            (Some(a), Some(b)) => self.search(ex, a, b)?,
            _ => Vec::new(),
        }
        .into_iter();
        Ok(())
    }

    fn next_row(&mut self, _: &'a Executor<'_>) -> Result<Option<Row>> {
        Ok(self.found.next())
    }
}

/// The path through the pattern's columns: node slots, and relationship slots holding a
/// relationship or (variable length) a list of them in pattern order.
fn build_path(ex: &Executor<'_>, row: &[Value], nodes: &[usize], rels: &[(usize, bool)]) -> Result<Path> {
    let node_at = |i: usize| match row[nodes[i]] {
        Value::Node(id) => Ok(id),
        _ => Err(Error::Runtime("path node is not bound".into())),
    };
    let mut path = Path {
        nodes: vec![node_at(0)?],
        rels: Vec::new(),
    };
    for (i, &(slot, var_length)) in rels.iter().enumerate() {
        if var_length {
            let Value::List(items) = &row[slot] else {
                return Err(Error::Runtime("path segment is not bound".into()));
            };
            let mut cur = *path.nodes.last().expect("starts with a node");
            for item in items {
                let Value::Rel(r) = item else {
                    return Err(Error::Runtime("path segment holds a non-relationship".into()));
                };
                let (s, e) = ex
                    .store()
                    .relationship_endpoints(*r)?
                    .ok_or_else(|| Error::Runtime(format!("relationship {r} does not exist")))?;
                cur = if s == cur { e } else { s };
                path.rels.push(*r);
                path.nodes.push(cur);
            }
        } else {
            let Value::Rel(r) = row[slot] else {
                return Err(Error::Runtime("path relationship is not bound".into()));
            };
            path.rels.push(r);
            path.nodes.push(node_at(i + 1)?);
        }
    }
    Ok(path)
}
