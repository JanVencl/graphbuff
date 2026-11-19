//! Query plans: a tree of operators built from the AST, run by the executor.
//!
//! Each query part (the pieces of a `UNION`) becomes one operator tree. Leaves are
//! [`Operator::Start`], which yields one empty row; every other operator has exactly one
//! input. `RETURN` and `WITH` are split into their steps:
//!
//! ```text
//! Filter (WITH … WHERE)
//!   Select                 drops the source columns
//!     Limit / Skip
//!       Sort | Top         may read source columns and projected aliases
//!         Distinct
//!           Project | Aggregate    outputs = projected columns, then the source row
//! ```
//!
//! A projection keeps its input row behind its output columns (the frame's `hidden`
//! scope) until `Select`, so that `ORDER BY` can use variables that are not returned.
//!
//! `MATCH` is planned into steps ([`MatchPlan`]): one start step per pattern part (the
//! part's cheaper end: an already bound node, an index seek, the smallest label scan, or
//! all nodes; or a relationship found by a relationship index seek), then one expansion
//! per relationship, a path projection for `p = (…)`, and the `WHERE` filter. Anonymous nodes and relationships get hidden columns, dropped after
//! the match. Relationship uniqueness is checked against the relationships bound earlier
//! in the same `MATCH`.
//!
//! Read operators run as pull-based streams ([`crate::stream`]): each asks its input for
//! at most as many rows as it can still use, so `LIMIT` stops scans and expansions early.
//! Updates are eager: everything below an update is run to completion first, so an
//! update always applies to every row.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use graphbuff_core::{Direction, Error, IndexEntity, Result};
use graphbuff_core::Value;
use graphbuff_cypher::ast::{
    CallScope, MapProjectionItem, Clause, Expr, ProcedureYield, NodePattern, PatternPart, Projection, ProjectionItem, Query, RelDirection,
    RelPattern, RemoveItem, SetItem, SingleQuery, SortItem, SubqueryKind,
};

use crate::context::{Executor, Profile};
use crate::result::{PlanDescription, PlanLine};
use crate::matcher::{SeekKind, extract_hints};
use crate::procedures::Procedure;
use crate::typecheck::{self, Ty, TyEnv};

/// The plan of a whole query.
#[derive(Debug, Clone)]
pub(crate) struct QueryPlan {
    pub parts: Vec<PartPlan>,
    /// `UNION ALL` (keep duplicates) rather than `UNION`.
    pub union_all: bool,
}

/// The plan of one query part.
#[derive(Debug, Clone)]
pub(crate) enum PartPlan {
    /// An index command, which is always alone in its query.
    Schema(Clause),
    /// An operator tree; `returns` if the part ends with `RETURN` (else it yields no rows).
    Pipeline { root: Operator, returns: bool },
}

/// A projection item: output column name and expression.
pub(crate) type Item = (String, Expr);

#[derive(Debug, Clone)]
pub(crate) enum Operator {
    /// One empty row.
    Start,
    /// `MATCH` / `OPTIONAL MATCH`: every binding found by `plan`'s steps, per input row.
    /// An optional match yields the input row (new variables null) if there is none.
    Match {
        input: Box<Operator>,
        optional: bool,
        plan: MatchPlan,
    },
    Create {
        input: Box<Operator>,
        patterns: Vec<PatternPart>,
    },
    Merge {
        input: Box<Operator>,
        pattern: PatternPart,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },
    Set {
        input: Box<Operator>,
        items: Vec<SetItem>,
    },
    Remove {
        input: Box<Operator>,
        items: Vec<RemoveItem>,
    },
    Delete {
        input: Box<Operator>,
        detach: bool,
        exprs: Vec<Expr>,
    },
    Unwind {
        input: Box<Operator>,
        expr: Expr,
        var: String,
    },
    /// Evaluates `items` (after every variable in scope, with `star`) per row.
    Project {
        input: Box<Operator>,
        star: bool,
        items: Vec<Item>,
    },
    /// Groups by the items without aggregates and computes the aggregates per group.
    Aggregate {
        input: Box<Operator>,
        star: bool,
        items: Vec<Item>,
    },
    /// Keeps the first row of every distinct set of projected values.
    Distinct { input: Box<Operator> },
    /// Sorts by `keys`; a key equal to one of the projection's `items` reuses its value.
    Sort {
        input: Box<Operator>,
        keys: Vec<SortItem>,
        items: Vec<Item>,
    },
    /// `ORDER BY … LIMIT`: the first `skip + limit` rows of what `Sort` would yield, in
    /// the same order (ties keep their input order), without holding all of the input.
    Top {
        input: Box<Operator>,
        keys: Vec<SortItem>,
        items: Vec<Item>,
        skip: Option<Expr>,
        limit: Expr,
    },
    Skip {
        input: Box<Operator>,
        count: Expr,
    },
    Limit {
        input: Box<Operator>,
        count: Expr,
    },
    /// Drops the source columns a projection kept for `Sort`.
    Select { input: Box<Operator> },
    Filter {
        input: Box<Operator>,
        predicate: Expr,
    },
    /// `CALL { … }`: runs `plan` once per input row, starting from a row that holds the
    /// `imports` (values from the input row). With `returns`, each subquery row is the
    /// input row followed by `columns`; otherwise each input row is kept once. An
    /// `updating` subquery runs eagerly, like an update.
    Call {
        input: Box<Operator>,
        plan: Box<QueryPlan>,
        imports: Vec<String>,
        columns: Vec<String>,
        returns: bool,
        updating: bool,
    },
    /// `FOREACH (var IN list | …)`: per input row and element of `list`, runs `body`
    /// from a row holding `imports` (the variables in scope, then `var`). Passes the
    /// input rows through; runs eagerly, like an update.
    Foreach {
        input: Box<Operator>,
        var: String,
        list: Expr,
        body: Box<QueryPlan>,
        imports: Vec<String>,
    },
    /// `CALL proc(args) YIELD …`: per input row, a row per row the procedure returns,
    /// with the yielded `fields` (output index, variable) added. A `VOID` procedure
    /// (no fields) keeps each input row once.
    ProcedureCall {
        input: Box<Operator>,
        procedure: Arc<Procedure>,
        /// One per parameter (defaults filled in).
        args: Vec<Expr>,
        fields: Vec<(usize, String)>,
    },
    /// Computes `op` with batched kernels over the CSR export ([`crate::accel`]), in
    /// place of `fallback` (the row operators), which runs instead if the kernels cannot
    /// answer exactly. `input` is always `Start`. `note`: backend and costs, for plans.
    Kernel {
        input: Box<Operator>,
        op: crate::accel::KernelOp,
        fallback: Box<Operator>,
        note: String,
    },
    /// `LOAD CSV`: per input row, a row per record of the file `url` names, with the
    /// record in `var`.
    LoadCsv {
        input: Box<Operator>,
        headers: bool,
        url: Expr,
        var: String,
        terminator: u8,
    },
}

impl QueryPlan {
    /// Plans every part of `q`, checking clause order and placement rules. The store
    /// (label counts, indexes) and the parameters guide where pattern matching starts.
    pub fn build(q: &Query, ex: &Executor) -> Result<QueryPlan> {
        Ok(QueryPlan {
            parts: q
                .parts
                .iter()
                .map(|p| plan_part(p, ex, &[]).map(|(plan, _)| plan))
                .collect::<Result<_>>()?,
            union_all: q.union_all,
        })
    }

    /// Plans a subquery whose start row holds `imported` (outer variables). Returns the
    /// plan and the columns it returns (none for a unit subquery).
    pub fn build_subquery(q: &Query, ex: &Executor, imported: &[String]) -> Result<(QueryPlan, Vec<String>)> {
        let mut parts = Vec::new();
        let mut columns: Option<Vec<String>> = None;
        for p in &q.parts {
            let (plan, cols) = plan_part(p, ex, imported)?;
            if matches!(plan, PartPlan::Schema(_)) {
                return Err(Error::Semantic("index and constraint commands cannot be used in a subquery".into()));
            }
            match &columns {
                None => columns = Some(cols),
                Some(c) => {
                    let (mut a, mut b) = (c.clone(), cols);
                    a.sort();
                    b.sort();
                    if a != b {
                        return Err(Error::Semantic(
                            "all sub queries in a UNION must have the same return column names".into(),
                        ));
                    }
                }
            }
            parts.push(plan);
        }
        Ok((
            QueryPlan {
                parts,
                union_all: q.union_all,
            },
            columns.unwrap_or_default(),
        ))
    }

    /// Plans the query of an `EXISTS` / `COUNT` / `COLLECT` expression (see
    /// [`QueryPlan::build_subquery`]). Parts without a `RETURN` return a constant.
    pub fn build_expression_subquery(
        kind: SubqueryKind,
        q: &Query,
        ex: &Executor,
        imported: &[String],
    ) -> Result<QueryPlan> {
        let name = match kind {
            SubqueryKind::Exists => "EXISTS",
            SubqueryKind::Count => "COUNT",
            SubqueryKind::Collect => "COLLECT",
        };
        if q.is_updating() {
            return Err(Error::Semantic(format!("a {name} subquery cannot change the graph")));
        }
        let mut q = q.clone();
        for part in &mut q.parts {
            if matches!(part.clauses.last(), Some(Clause::Return(_))) {
                continue;
            }
            if kind == SubqueryKind::Collect {
                return Err(Error::Semantic("a COLLECT subquery must end with RETURN".into()));
            }
            part.clauses.push(Clause::Return(Projection {
                distinct: false,
                star: false,
                items: vec![ProjectionItem {
                    expr: Expr::Literal(Value::Int(1)),
                    alias: "\u{0}row".into(),
                }],
                order_by: Vec::new(),
                skip: None,
                limit: None,
                where_: None,
            }));
        }
        let (plan, columns) = Self::build_subquery(&q, ex, imported)?;
        if kind == SubqueryKind::Collect && columns.len() != 1 {
            return Err(Error::Semantic("a COLLECT subquery must return exactly one column".into()));
        }
        Ok(plan)
    }

    /// Whether running the plan can change the graph.
    pub fn is_updating(&self) -> bool {
        fn updating(op: &Operator) -> bool {
            op.is_update() || op.input().is_some_and(updating)
        }
        self.parts.iter().any(|p| match p {
            PartPlan::Schema(_) => true,
            PartPlan::Pipeline { root, .. } => updating(root),
        })
    }

    /// The plan as an indented operator tree, one operator per line, root first.
    pub fn explain(&self) -> String {
        self.describe(None).to_string()
    }

    /// The plan as lines, with what `profile` measured (if the query ran under
    /// `PROFILE`). Times are each operator's own: its inputs' times are subtracted.
    pub fn describe(&self, profile: Option<&Profile>) -> PlanDescription {
        let mut lines = Lines::default();
        let indent = if self.parts.len() > 1 {
            lines.push(0, if self.union_all { "UnionAll" } else { "Union" }.into(), None, None);
            1
        } else {
            0
        };
        for part in &self.parts {
            match part {
                PartPlan::Schema(c) => lines.push(indent, format!("Schema {}", c.name()), None, None),
                PartPlan::Pipeline { root, .. } => root.describe_into(&mut lines, indent),
            }
        }
        let mut out: Vec<PlanLine> = lines
            .0
            .into_iter()
            .map(|(depth, operator, estimated_rows, key)| {
                // A measured line that never ran (say, under `LIMIT 0`) used nothing.
                let measured = key
                    .and_then(|k| profile.map(|p| p.get(&k).copied().unwrap_or_default()));
                PlanLine {
                    depth,
                    operator,
                    estimated_rows,
                    rows: measured.map(|m| m.rows),
                    time: measured.map(|m| m.time),
                }
            })
            .collect();
        // Inclusive times → own times: subtract the inputs (the lines one level deeper,
        // up to the next line at this level or above).
        for i in 0..out.len() {
            let Some(total) = out[i].time else { continue };
            let d = out[i].depth;
            let inputs: std::time::Duration = out[i + 1..]
                .iter()
                .take_while(|l| l.depth > d)
                .filter(|l| l.depth == d + 1)
                .filter_map(|l| l.time)
                .sum();
            out[i].time = Some(total.saturating_sub(inputs));
        }
        PlanDescription {
            lines: out,
            profiled: profile.is_some(),
        }
    }
}

/// Plan lines being collected: depth, text, estimated rows, profile key.
#[derive(Default)]
struct Lines(Vec<(usize, String, Option<f64>, Option<usize>)>);

impl Lines {
    fn push(&mut self, depth: usize, text: String, estimate: Option<f64>, key: Option<usize>) {
        self.0.push((depth, text, estimate, key));
    }
}

/// The key under which `PROFILE` records a plan node: its address.
pub(crate) fn profile_key<T>(node: &T) -> usize {
    node as *const T as usize
}

/// Plans one query part whose start row holds `imported`; also returns the columns it
/// returns (none if it does not end with `RETURN`).
/// What a variable is known to hold at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarKind {
    Node,
    Rel,
    /// The relationships of a variable-length pattern.
    RelList,
    Path,
    /// A list (it may hold relationships for a variable-length pattern).
    List,
    /// A value that is none of the above (a literal, a map …).
    Value,
    /// Could be anything (a parameter, a function's result, an imported variable …).
    Unknown,
}

impl VarKind {
    fn name(self) -> &'static str {
        match self {
            VarKind::Node => "a node",
            VarKind::Rel => "a relationship",
            VarKind::RelList => "a list of relationships",
            VarKind::Path => "a path",
            VarKind::List => "a list",
            VarKind::Value => "a value",
            VarKind::Unknown => "a value",
        }
    }
}

/// What `e` is known to hold.
fn expr_kind(e: &Expr, kinds: &HashMap<String, VarKind>) -> VarKind {
    match e {
        Expr::Var(v) => kinds.get(v).copied().unwrap_or(VarKind::Unknown),
        Expr::Literal(Value::Null) => VarKind::Unknown,
        Expr::Literal(Value::List(_)) | Expr::List(_) | Expr::ListComprehension { .. } => VarKind::List,
        Expr::Literal(_) | Expr::Map(_) => VarKind::Value,
        _ => VarKind::Unknown,
    }
}

/// Compile-time checks of the variables in `patterns` (a `MATCH`, or a `CREATE` /
/// `MERGE` when `creating`) against what `kinds` knows, which it then extends: a
/// variable must keep its kind (`VariableTypeConflict`), a path variable and a
/// relationship that is created must be new, and a relationship is matched once per
/// clause (`VariableAlreadyBound`).
/// `var` declared as `want` in a pattern, given what `kinds` knows (updated).
fn check_kind(var: &str, want: VarKind, kinds: &mut HashMap<String, VarKind>) -> Result<()> {
    match kinds.get(var).copied() {
        Some(VarKind::List) if want == VarKind::RelList => Ok(()),
        Some(have) if have != want && have != VarKind::Unknown => Err(Error::Semantic(format!(
            "variable `{var}` is {} here but was already declared as {}",
            want.name(),
            have.name()
        ))),
        Some(_) if want == VarKind::Path => {
            Err(Error::Semantic(format!("variable `{var}` already declared; a path variable must be new")))
        }
        _ => {
            kinds.insert(var.to_string(), want);
            Ok(())
        }
    }
}

fn check_pattern_vars(patterns: &[PatternPart], kinds: &mut HashMap<String, VarKind>, creating: bool) -> Result<()> {
    let mut fresh_rels: HashSet<&str> = HashSet::new();
    let mut new_here: HashSet<&str> = HashSet::new();
    for part in patterns {
        let mut nodes: Vec<&NodePattern> = vec![&part.start];
        nodes.extend(part.chain.iter().map(|(_, n)| n));
        if let Some(p) = &part.var {
            check_kind(p, VarKind::Path, kinds)?;
            new_here.insert(p);
        }
        for (i, n) in nodes.iter().enumerate() {
            let Some(v) = &n.var else { continue };
            let was_bound = kinds.contains_key(v.as_str()) && !new_here.contains(v.as_str());
            check_kind(v, VarKind::Node, kinds)?;
            if creating && was_bound && (!n.labels.is_empty() || n.props.is_some() || (part.chain.is_empty() && i == 0)) {
                return Err(Error::Semantic(format!(
                    "variable `{v}` already declared; it cannot be created again with labels or properties"
                )));
            }
            if !was_bound {
                new_here.insert(v);
            }
        }
        for (r, _) in &part.chain {
            let Some(v) = &r.var else { continue };
            let was_bound = kinds.contains_key(v.as_str());
            let want = if r.length.is_some() { VarKind::RelList } else { VarKind::Rel };
            check_kind(v, want, kinds)?;
            if (creating && was_bound) || !fresh_rels.insert(v) {
                return Err(Error::Semantic(format!(
                    "variable `{v}` already declared; a relationship can only be used once in a pattern"
                )));
            }
        }
    }
    Ok(())
}

/// The compile-time types of the variables `kinds` knows.
fn ty_env(kinds: &HashMap<String, VarKind>) -> TyEnv {
    kinds
        .iter()
        .map(|(v, k)| {
            let t = match k {
                VarKind::Node => Ty::Node,
                VarKind::Rel => Ty::Rel,
                VarKind::RelList => Ty::List(Box::new(Ty::Rel)),
                VarKind::Path => Ty::Path,
                VarKind::List => Ty::List(Box::new(Ty::Any)),
                VarKind::Value | VarKind::Unknown => Ty::Any,
            };
            (v.clone(), t)
        })
        .collect()
}

/// Compile-time check that a clause reads only variables in scope (`UndefinedVariable`).
fn check_clause_vars(clause: &Clause, env: &TyEnv, ex: &Executor) -> Result<()> {
    let known = |name: &str| ex.function_exists(name);
    let defined = |e: &Expr, env: &TyEnv| typecheck::check_defined(e, env, &known);
    let var = |v: &str| {
        if env.contains_key(v) {
            Ok(())
        } else {
            Err(Error::Semantic(format!("variable `{v}` not defined")))
        }
    };
    // A pattern is a predicate, not a value (`RETURN (n)-->()`).
    let value = |e: &Expr| {
        if matches!(e, Expr::PatternPredicate(_)) {
            Err(Error::Semantic(
                "a pattern can only be used as a predicate (in WHERE, or with exists())".into(),
            ))
        } else {
            Ok(())
        }
    };
    let set_items = |items: &[SetItem]| -> Result<()> {
        for it in items {
            match it {
                SetItem::Property { var: v, value: x, .. } | SetItem::Replace { var: v, value: x } | SetItem::Merge { var: v, value: x } => {
                    var(v)?;
                    value(x)?;
                    defined(x, env)?;
                }
                SetItem::Labels { var: v, .. } => var(v)?,
            }
        }
        Ok(())
    };
    let pattern_props = |parts: &[PatternPart]| -> Result<()> {
        for part in parts {
            let nodes = std::iter::once(&part.start).chain(part.chain.iter().map(|(_, n)| n));
            let props = nodes
                .filter_map(|n| n.props.as_ref())
                .chain(part.chain.iter().filter_map(|(r, _)| r.props.as_ref()));
            for p in props {
                defined(p, env)?;
            }
        }
        Ok(())
    };
    match clause {
        Clause::Match { patterns, where_, .. } => {
            let param_map = patterns.iter().any(|part| {
                std::iter::once(&part.start)
                    .chain(part.chain.iter().map(|(_, n)| n))
                    .any(|n| matches!(n.props, Some(Expr::Param(_))))
                    || part.chain.iter().any(|(r, _)| matches!(r.props, Some(Expr::Param(_))))
            });
            if param_map {
                return Err(Error::Semantic(
                    "a parameter cannot stand for the properties of a MATCH pattern; write `{key: $param.key}`".into(),
                ));
            }
            pattern_props(patterns)?;
            if let Some(w) = where_ {
                defined(w, env)?;
            }
        }
        Clause::Create { patterns } => pattern_props(patterns)?,
        Clause::Merge { pattern, on_create, on_match } => {
            pattern_props(std::slice::from_ref(pattern))?;
            set_items(on_create)?;
            set_items(on_match)?;
        }
        Clause::Set(items) => set_items(items)?,
        Clause::Remove(items) => {
            for it in items {
                match it {
                    RemoveItem::Property { var: v, .. } | RemoveItem::Labels { var: v, .. } => var(v)?,
                }
            }
        }
        Clause::Delete { exprs, .. } => exprs.iter().try_for_each(|e| defined(e, env))?,
        Clause::Unwind { expr, .. } => defined(expr, env)?,
        Clause::With(p) | Clause::Return(p) => {
            let mut after = env.clone();
            for it in &p.items {
                value(&it.expr)?;
                defined(&it.expr, env)?;
                after.insert(it.alias.clone(), Ty::Any);
            }
            // After aggregating or DISTINCT, ORDER BY sees only the projected columns.
            let grouped = p.distinct || p.items.iter().any(|it| it.expr.contains_aggregate());
            if grouped && !p.star {
                let only: TyEnv = p.items.iter().map(|it| (it.alias.clone(), Ty::Any)).collect();
                for s in &p.order_by {
                    defined(&sort_key_over_items(&s.expr, &p.items)?, &only)?;
                }
            } else {
                for s in &p.order_by {
                    defined(&s.expr, &after)?;
                }
            }
            if let Some(w) = &p.where_ {
                if grouped && !p.star {
                    let only: TyEnv = p.items.iter().map(|it| (it.alias.clone(), Ty::Any)).collect();
                    defined(&sort_key_over_items(w, &p.items)?, &only)?;
                } else {
                    defined(w, &after)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Compile-time type checks of a clause's expressions (see [`crate::typecheck`]).
fn check_clause_types(clause: &Clause, kinds: &HashMap<String, VarKind>, ex: &Executor) -> Result<()> {
    let env = ty_env(kinds);
    check_clause_vars(clause, &env, ex)?;
    match clause {
        Clause::Match { where_: Some(w), .. } => typecheck::check_predicate(w, &env)?,
        Clause::With(p) | Clause::Return(p) => {
            let mut after = env.clone();
            for it in &p.items {
                let t = typecheck::check(&it.expr, &env)?;
                after.insert(it.alias.clone(), t);
            }
            for s in &p.order_by {
                typecheck::check(&s.expr, &after)?;
            }
            if let Some(w) = &p.where_ {
                typecheck::check_predicate(w, &after)?;
            }
            for x in [&p.skip, &p.limit].into_iter().flatten() {
                typecheck::check(x, &env)?;
            }
        }
        Clause::Unwind { expr, .. } => {
            typecheck::check(expr, &env)?;
        }
        Clause::Delete { exprs, .. } => {
            for e in exprs {
                typecheck::check_deletable(e, &env)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn plan_part(q: &SingleQuery, ex: &Executor, imported: &[String]) -> Result<(PartPlan, Vec<String>)> {
    if q.clauses.iter().any(Clause::is_schema) {
        if q.clauses.len() > 1 {
            return Err(Error::Semantic(
                "index commands must be the only clause in their query".into(),
            ));
        }
        return Ok((PartPlan::Schema(q.clauses[0].clone()), Vec::new()));
    }
    validate_clause_order(q)?;
    let mut op = Operator::Start;
    // Variables in scope before each clause, in scope order.
    let mut bound: Vec<String> = imported.to_vec();
    let add = |bound: &mut Vec<String>, v: &str| {
        if !bound.iter().any(|b| b == v) {
            bound.push(v.to_string());
        }
    };
    // What each variable in scope is known to hold, for the compile-time checks of
    // patterns. Imported variables could hold anything.
    let mut kinds: HashMap<String, VarKind> = imported.iter().map(|v| (v.clone(), VarKind::Unknown)).collect();
    for clause in &q.clauses {
        match clause {
            Clause::Match { patterns, .. } => check_pattern_vars(patterns, &mut kinds, false)?,
            Clause::Create { patterns } => check_pattern_vars(patterns, &mut kinds, true)?,
            Clause::Merge { pattern, .. } => check_pattern_vars(std::slice::from_ref(pattern), &mut kinds, true)?,
            _ => {}
        }
        check_clause_types(clause, &kinds, ex)?;
        if let Clause::Return(p) = clause
            && p.star && p.items.is_empty() && bound.is_empty()
        {
            return Err(Error::Semantic("RETURN * needs at least one variable in scope".into()));
        }
        let input = Box::new(op);
        op = match clause {
            Clause::Match {
                optional,
                patterns,
                where_,
            } => Operator::Match {
                input,
                optional: *optional,
                plan: plan_match(ex, patterns, where_.as_ref(), &bound)?,
            },
            Clause::Create { patterns } if patterns.iter().any(|p| p.shortest.is_some()) => {
                return Err(Error::Semantic("shortestPath(...) cannot be used in CREATE".into()));
            }
            Clause::Merge { pattern, .. } if pattern.shortest.is_some() => {
                return Err(Error::Semantic("shortestPath(...) cannot be used in MERGE".into()));
            }
            Clause::Create { patterns } if patterns.iter().any(has_label_expr) => {
                return Err(Error::Semantic(
                    "CREATE needs plain labels (`:A:B` or `:A&B`) and a single relationship type".into(),
                ));
            }
            Clause::Merge { pattern, .. } if pattern.chain.iter().any(|(r, _)| r.types.len() != 1 || r.type_expr.is_some()) => {
                return Err(Error::Semantic("MERGE needs exactly one type for each relationship".into()));
            }
            Clause::Merge { pattern, .. } if has_label_expr(pattern) => {
                return Err(Error::Semantic(
                    "MERGE needs plain labels (`:A:B` or `:A&B`) and a single relationship type".into(),
                ));
            }
            Clause::Create { patterns } => Operator::Create {
                input,
                patterns: patterns.clone(),
            },
            Clause::Merge {
                pattern,
                on_create,
                on_match,
            } => Operator::Merge {
                input,
                pattern: pattern.clone(),
                on_create: on_create.clone(),
                on_match: on_match.clone(),
            },
            Clause::Set(items) => Operator::Set {
                input,
                items: items.clone(),
            },
            Clause::Remove(items) => Operator::Remove {
                input,
                items: items.clone(),
            },
            Clause::Delete { detach, exprs } => Operator::Delete {
                input,
                detach: *detach,
                exprs: exprs.clone(),
            },
            Clause::Unwind { expr, var } => Operator::Unwind {
                input,
                expr: expr.clone(),
                var: var.clone(),
            },
            Clause::With(p) | Clause::Return(p) => plan_projection(input, p),
            Clause::CallSubquery { scope, query } => {
                let imports = call_imports(scope, query, &bound)?;
                let (plan, columns) = QueryPlan::build_subquery(query, ex, &imports)?;
                if let Some(c) = columns.iter().find(|c| bound.contains(c)) {
                    return Err(Error::Semantic(format!(
                        "variable `{c}` already declared; a CALL subquery cannot return it"
                    )));
                }
                let returns = matches!(plan.parts.first(), Some(PartPlan::Pipeline { returns: true, .. }));
                let updating = plan.is_updating();
                Operator::Call {
                    input,
                    plan: Box::new(plan),
                    imports,
                    columns,
                    returns,
                    updating,
                }
            }
            Clause::Foreach { var, list, body } => {
                if bound.contains(var) {
                    return Err(Error::Semantic(format!("variable `{var}` already declared")));
                }
                if let Some(c) = body.iter().find(|c| {
                    !matches!(
                        c,
                        Clause::Create { .. }
                            | Clause::Merge { .. }
                            | Clause::Set(_)
                            | Clause::Remove(_)
                            | Clause::Delete { .. }
                            | Clause::Foreach { .. }
                    )
                }) {
                    return Err(Error::Semantic(format!(
                        "{} cannot be used inside FOREACH (only CREATE, MERGE, SET, REMOVE, DELETE and FOREACH)",
                        c.name()
                    )));
                }
                let mut imports = bound.clone();
                imports.push(var.clone());
                let (part, _) = plan_part(&SingleQuery { clauses: body.clone() }, ex, &imports)?;
                Operator::Foreach {
                    input,
                    var: var.clone(),
                    list: list.clone(),
                    body: Box::new(QueryPlan {
                        parts: vec![part],
                        union_all: false,
                    }),
                    imports,
                }
            }
            Clause::LoadCsv {
                headers,
                url,
                var,
                terminator,
            } => {
                if bound.contains(var) {
                    return Err(Error::Semantic(format!("variable `{var}` already declared")));
                }
                let terminator = match terminator.as_deref() {
                    None => b',',
                    Some(t) if t.len() == 1 && t.is_ascii() => t.as_bytes()[0],
                    Some(t) => {
                        return Err(Error::Semantic(format!(
                            "FIELDTERMINATOR must be a single ASCII character, not `{t}`"
                        )));
                    }
                };
                Operator::LoadCsv {
                    input,
                    headers: *headers,
                    url: url.clone(),
                    var: var.clone(),
                    terminator,
                }
            }
            Clause::CallProcedure { name, args, yields } => plan_procedure_call(
                ex,
                input,
                name,
                args.as_deref(),
                yields.as_ref(),
                &bound,
                q.clauses.len() == 1,
            )?,
            Clause::CreateIndex { .. }
            | Clause::DropIndex { .. }
            | Clause::ShowIndexes
            | Clause::CreateConstraint { .. }
            | Clause::DropConstraint { .. }
            | Clause::ShowConstraints
            | Clause::ShowProcedures => {
                unreachable!("schema clauses are planned above")
            }
        };
        match clause {
            Clause::Match { patterns, .. } | Clause::Create { patterns } => {
                for v in patterns.iter().flat_map(PatternPart::variables) {
                    add(&mut bound, v);
                }
            }
            Clause::Merge { pattern, .. } => {
                for v in pattern.variables() {
                    add(&mut bound, v);
                }
            }
            Clause::Unwind { var, .. } | Clause::LoadCsv { var, .. } => add(&mut bound, var),
            Clause::CallSubquery { .. } => {
                if let Operator::Call { columns, .. } = &op {
                    for c in columns {
                        add(&mut bound, c);
                    }
                }
            }
            Clause::CallProcedure { .. } => {
                let mut o = &op;
                if let Operator::Filter { input, .. } = o {
                    o = input;
                }
                if let Operator::ProcedureCall { fields, .. } = o {
                    for (_, v) in fields {
                        add(&mut bound, v);
                    }
                }
            }
            Clause::With(p) | Clause::Return(p) => {
                if p.star {
                    bound.sort();
                } else {
                    bound.clear();
                }
                for it in &p.items {
                    add(&mut bound, &it.alias);
                }
            }
            _ => {}
        }
        // Keep `kinds` in step with `bound`.
        let mut next: HashMap<String, VarKind> = HashMap::new();
        for v in &bound {
            let kind = match clause {
                Clause::With(p) | Clause::Return(p) => match p.items.iter().rev().find(|it| &it.alias == v) {
                    Some(it) => expr_kind(&it.expr, &kinds),
                    None => kinds.get(v).copied().unwrap_or(VarKind::Unknown),
                },
                _ => kinds.get(v).copied().unwrap_or(VarKind::Unknown),
            };
            next.insert(v.clone(), kind);
        }
        kinds = next;
    }
    // A standalone `CALL` returns what it yields.
    let returns = match q.clauses.last() {
        Some(Clause::Return(_)) => true,
        Some(Clause::CallProcedure { .. }) => q.clauses.len() == 1 && !bound.is_empty(),
        _ => false,
    };
    let columns = if returns { bound } else { Vec::new() };
    let op = crate::accel::accelerate(q, ex, imported, op);
    Ok((PartPlan::Pipeline { root: op, returns }, columns))
}

/// Plans `CALL name(args) [YIELD …]`. `standalone`: the call is the whole query part, so
/// it may leave out the parentheses (arguments come from the parameters of the same
/// names) and `YIELD` (every output is returned), and may use `YIELD *`.
fn plan_procedure_call(
    ex: &Executor,
    input: Box<Operator>,
    name: &str,
    args: Option<&[Expr]>,
    yields: Option<&ProcedureYield>,
    bound: &[String],
    standalone: bool,
) -> Result<Operator> {
    let procedure = ex.procedures.resolve(name)?.clone();
    let sig = &procedure.signature;
    let mut args: Vec<Expr> = match args {
        Some(a) => a.to_vec(),
        None if standalone => sig
            .inputs
            .iter()
            .map(|p| match &p.default {
                Some(d) if !ex.params.contains_key(&p.name) => Expr::Literal(d.clone()),
                _ => Expr::Param(p.name.clone()),
            })
            .collect(),
        None => {
            return Err(Error::Semantic(format!(
                "a procedure call inside a larger query needs its arguments in parentheses: `{name}(…)`"
            )));
        }
    };
    if args.len() < sig.required() || args.len() > sig.inputs.len() {
        let expected = if sig.required() == sig.inputs.len() {
            sig.inputs.len().to_string()
        } else {
            format!("{} to {}", sig.required(), sig.inputs.len())
        };
        return Err(Error::Semantic(format!(
            "procedure `{}` takes {expected} argument(s), got {}: {}",
            sig.name,
            args.len(),
            sig.text()
        )));
    }
    for (a, p) in args.iter().zip(&sig.inputs) {
        typecheck::check_procedure_arg(a, p.ty, &sig.name, &p.name)?;
    }
    for p in &sig.inputs[args.len()..] {
        args.push(Expr::Literal(p.default.clone().expect("only optional parameters are left")));
    }
    let all = || sig.outputs.iter().cloned().enumerate().collect::<Vec<_>>();
    let fields: Vec<(usize, String)> = match yields {
        None if standalone => all(),
        None if sig.outputs.is_empty() => Vec::new(),
        None => {
            return Err(Error::Semantic(format!(
                "a procedure call inside a larger query must use YIELD: {}",
                sig.text()
            )));
        }
        Some(_) if sig.outputs.is_empty() => {
            return Err(Error::Semantic(format!(
                "cannot YIELD from the VOID procedure `{}`",
                sig.name
            )));
        }
        Some(y) if y.star => {
            if !standalone {
                return Err(Error::Semantic(
                    "YIELD * can only be used in a standalone procedure call".into(),
                ));
            }
            all()
        }
        Some(y) => y
            .items
            .iter()
            .map(|(field, var)| {
                sig.outputs
                    .iter()
                    .position(|o| o == field)
                    .map(|i| (i, var.clone()))
                    .ok_or_else(|| {
                        Error::Semantic(format!(
                            "procedure `{}` has no output `{field}`: {}",
                            sig.name,
                            sig.text()
                        ))
                    })
            })
            .collect::<Result<_>>()?,
    };
    let mut seen = HashSet::new();
    for (_, v) in &fields {
        if bound.contains(v) || !seen.insert(v) {
            return Err(Error::Semantic(format!("variable `{v}` already declared")));
        }
    }
    let op = Operator::ProcedureCall {
        input,
        procedure,
        args,
        fields,
    };
    Ok(match yields.and_then(|y| y.where_.as_ref()) {
        Some(predicate) => {
            if predicate.contains_aggregate() {
                return Err(Error::Semantic("aggregate functions are not allowed in WHERE".into()));
            }
            Operator::Filter {
                input: Box::new(op),
                predicate: predicate.clone(),
            }
        }
        None => op,
    })
}

/// The outer variables (of `bound`) a `CALL { … }` subquery imports.
fn call_imports(scope: &CallScope, query: &Query, bound: &[String]) -> Result<Vec<String>> {
    match scope {
        CallScope::All => Ok(bound.to_vec()),
        CallScope::Vars(vars) => {
            for v in vars {
                if !bound.contains(v) {
                    return Err(Error::Semantic(format!("variable `{v}` not defined")));
                }
            }
            Ok(vars.clone())
        }
        CallScope::Implicit => {
            // An importing `WITH`: the first clause, projecting outer variables as
            // themselves (or `*`).
            let mut out: Vec<String> = Vec::new();
            for part in &query.parts {
                let Some(Clause::With(p)) = part.clauses.first() else {
                    continue;
                };
                let mut names: Vec<&String> = Vec::new();
                if p.star {
                    names.extend(bound);
                }
                for it in &p.items {
                    if let Expr::Var(v) = &it.expr
                        && *v == it.alias
                        && bound.contains(v)
                    {
                        names.push(v);
                    }
                }
                for n in names {
                    if !out.contains(n) {
                        out.push(n.clone());
                    }
                }
            }
            Ok(out)
        }
    }
}

fn plan_projection(input: Box<Operator>, p: &Projection) -> Operator {
    let items: Vec<Item> = p
        .items
        .iter()
        .map(|it| (it.alias.clone(), it.expr.clone()))
        .collect();
    let aggregates = items.iter().any(|(_, e)| e.contains_aggregate());
    let mut op = if aggregates {
        Operator::Aggregate {
            input,
            star: p.star,
            items: items.clone(),
        }
    } else {
        Operator::Project {
            input,
            star: p.star,
            items: items.clone(),
        }
    };
    if p.distinct {
        op = Operator::Distinct {
            input: Box::new(op),
        };
    }
    if !p.order_by.is_empty() {
        let keys: Vec<SortItem> = if aggregates {
            p.order_by
                .iter()
                .map(|s| SortItem {
                    expr: sort_key_over_items(&s.expr, &p.items).unwrap_or_else(|_| s.expr.clone()),
                    descending: s.descending,
                })
                .collect()
        } else {
            p.order_by.clone()
        };
        op = match &p.limit {
            Some(limit) => Operator::Top {
                input: Box::new(op),
                keys: keys.clone(),
                items,
                skip: p.skip.clone(),
                limit: limit.clone(),
            },
            None => Operator::Sort {
                input: Box::new(op),
                keys: keys.clone(),
                items,
            },
        };
    }
    if let Some(count) = &p.skip {
        op = Operator::Skip {
            input: Box::new(op),
            count: count.clone(),
        };
    }
    if let Some(count) = &p.limit {
        op = Operator::Limit {
            input: Box::new(op),
            count: count.clone(),
        };
    }
    // The WHERE of a WITH that neither aggregates nor is DISTINCT also sees the
    // variables coming in (`WITH c WHERE r IS NULL`), so it runs before `Select`.
    let sees_source = !aggregates && !p.distinct;
    let filter = |op: Operator| match &p.where_ {
        Some(w) => Operator::Filter {
            input: Box::new(op),
            // After DISTINCT or aggregating, parts equal to a projected item read its column.
            predicate: if sees_source { w.clone() } else { sort_key_over_items(w, &p.items).unwrap_or_else(|_| w.clone()) },
        },
        None => op,
    };
    if sees_source {
        op = filter(op);
    }
    op = Operator::Select {
        input: Box::new(op),
    };
    if !sees_source {
        op = filter(op);
    }
    op
}

// ---- MATCH ----------------------------------------------------------------------------

/// The steps that find the bindings of one `MATCH`, in execution order.
#[derive(Debug, Clone)]
pub(crate) struct MatchPlan {
    pub steps: Vec<Step>,
    /// Estimated rows after each step, per input row (see [`Estimator`]).
    pub estimates: Vec<f64>,
    /// Variables the match adds to the scope, in Cypher's order (as they appear).
    pub named: Vec<String>,
    /// Columns for anonymous nodes and relationships, after `named`; dropped at the end.
    pub hidden: Vec<String>,
}

/// How a pattern part finds candidates for its first node.
#[derive(Debug, Clone)]
pub(crate) enum Start {
    /// The variable is already bound (by an earlier clause or part).
    Bound,
    /// Index seeks; the smallest result is used. Comparisons on the same property are
    /// combined into one range seek. If no seek value can be evaluated, the fallback
    /// label is scanned instead.
    Seek {
        sources: Vec<SeekSource>,
        composite: Vec<CompositeSeek>,
        fallback: Option<String>,
    },
    LabelScan(String),
    AllNodes,
}

/// One index-usable constraint: `(:label {key: expr})` or a `WHERE` conjunct. For a
/// relationship seek, `label` is the relationship type.
#[derive(Debug, Clone)]
pub(crate) struct SeekSource {
    pub label: String,
    pub key: String,
    pub kind: SeekKind,
    pub expr: Expr,
}

/// An equality seek on a composite index: one value per indexed property, in order.
#[derive(Debug, Clone)]
pub(crate) struct CompositeSeek {
    pub label: String,
    pub keys: Vec<String>,
    pub exprs: Vec<Expr>,
}

/// Where a pattern part's traversal starts.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PartStart {
    /// At its first node, traversing left to right.
    First,
    /// At its last node, traversing right to left.
    Last,
    /// At relationship `i` (between nodes `i` and `i + 1`), found by an index seek;
    /// then right from node `i + 1` and left from node `i`.
    Rel(usize),
}

#[derive(Debug, Clone)]
pub(crate) enum Step {
    /// Binds `var` to each candidate that matches `node`'s labels and properties.
    NodeStart {
        var: String,
        node: NodePattern,
        start: Start,
    },
    /// Binds `rel` to each relationship a relationship index seek finds (the smallest
    /// result of `sources` and `composite`) that matches `pattern`, and `left` / `right`
    /// to its ends as the pattern reads left to right (`dir`: as written; an undirected
    /// relationship binds both ways round). None of the three is bound before.
    /// `used`: relationship columns bound earlier in this `MATCH` (uniqueness).
    RelSeek {
        rel: String,
        left: String,
        right: String,
        pattern: RelPattern,
        left_node: NodePattern,
        right_node: NodePattern,
        dir: Direction,
        sources: Vec<SeekSource>,
        composite: Vec<CompositeSeek>,
        used: Vec<String>,
    },
    /// From the node in `from`, follows relationships matching `pattern` in `dir` to a node
    /// matching `node`. `rel_bound` / `to_bound`: the variable already holds a value that
    /// must match. `used`: relationship columns bound earlier in this `MATCH` (uniqueness).
    /// With a variable length, `reversed` means the part is traversed right to left (the
    /// relationship list is still bound in pattern order).
    Expand {
        from: String,
        rel: String,
        to: String,
        pattern: RelPattern,
        node: NodePattern,
        dir: Direction,
        rel_bound: bool,
        to_bound: bool,
        used: Vec<String>,
        reversed: bool,
    },
    /// Binds `var` to the path through `nodes` and `rels` (pattern order; `true` marks a
    /// variable-length relationship, whose column holds a list).
    BuildPath {
        var: String,
        nodes: Vec<String>,
        rels: Vec<(String, bool)>,
    },
    /// A part of the `MATCH` that uses no variable bound before it: `right` finds its
    /// bindings once (built on first use, then reused for every row), and each row is
    /// combined with those whose `keys` are equal (Cypher `=`; with no keys, with all of
    /// them). `left_rels` / `right_rels`: relationship columns on either side, which must
    /// not share a relationship.
    Join {
        right: Vec<Step>,
        /// Estimated rows after each step of `right`.
        right_estimates: Vec<f64>,
        keys: Vec<(Expr, Expr)>,
        left_rels: Vec<String>,
        right_rels: Vec<String>,
    },
    /// `shortestPath` / `allShortestPaths` between the nodes in `from` and `to` (both
    /// bound by earlier steps): binds `rel` (a list with a variable length, else one
    /// relationship) and `path`. `predicates`: the `WHERE` conjuncts that use `rel` or
    /// `path`; the paths found are the shortest that pass them. `used`: relationship
    /// columns bound earlier in this `MATCH`, which the paths may not use.
    ShortestPath {
        from: String,
        to: String,
        rel: String,
        path: Option<String>,
        pattern: RelPattern,
        dir: Direction,
        all: bool,
        used: Vec<String>,
        predicates: Vec<Expr>,
    },
    /// The `WHERE` of the `MATCH`.
    Filter(Expr),
}

fn to_direction(d: RelDirection) -> Direction {
    match d {
        RelDirection::Right => Direction::Outgoing,
        RelDirection::Left => Direction::Incoming,
        RelDirection::Both => Direction::Both,
    }
}

/// Hidden column names cannot clash with Cypher variables.
fn is_hidden(name: &str) -> bool {
    name.starts_with('\u{0}')
}

fn plan_match(ex: &Executor, patterns: &[PatternPart], where_: Option<&Expr>, bound: &[String]) -> Result<MatchPlan> {
    let mut named: Vec<String> = Vec::new();
    for v in patterns.iter().flat_map(PatternPart::variables) {
        if !bound.iter().any(|b| b == v) && !named.iter().any(|n| n == v) {
            named.push(v.to_string());
        }
    }
    // Variables whose properties index seeks may constrain: nodes, and single relationships.
    let mut seek_vars: HashSet<&str> = patterns
        .iter()
        .flat_map(|p| std::iter::once(&p.start).chain(p.chain.iter().map(|(_, n)| n)))
        .filter_map(|n| n.var.as_deref())
        .collect();
    seek_vars.extend(
        patterns
            .iter()
            .flat_map(|p| p.chain.iter().map(|(r, _)| r))
            .filter(|r| r.length.is_none())
            .filter_map(|r| r.var.as_deref()),
    );
    let hints = extract_hints(where_, &seek_vars);
    let mut placed: HashSet<String> = bound.iter().cloned().collect();
    let mut hidden: Vec<String> = Vec::new();
    // Per part: its steps, what was bound before it, its relationship columns, and
    // whether it is independent of everything before it (a join candidate).
    type PartSteps = (Vec<Step>, HashSet<String>, Vec<String>, bool);
    let mut parts: Vec<PartSteps> = Vec::new();
    let mut rels_so_far: Vec<String> = Vec::new();
    // Shortest-path parts go last: they search between endpoints the others bind.
    let (shortest, mut remaining): (Vec<&PatternPart>, Vec<&PatternPart>) =
        patterns.iter().partition(|p| p.shortest.is_some());
    let estimator = Estimator { ex };
    while !remaining.is_empty() {
        // The next part and its direction: the cheapest by estimated rows over its steps,
        // given what is bound so far. Ties keep the written order and direction.
        let mut best: Option<(f64, usize, PartStart)> = None;
        for (i, part) in remaining.iter().enumerate() {
            let mut starts = vec![PartStart::First];
            if !part.chain.is_empty() {
                starts.push(PartStart::Last);
            }
            for ri in 0..part.chain.len() {
                if rel_seekable(ex, part, ri, &hints, &placed) {
                    starts.push(PartStart::Rel(ri));
                }
            }
            for how in starts {
                let (mut trial_placed, mut trial_hidden) = (placed.clone(), hidden.clone());
                let (steps, _) = plan_pattern_part(ex, part, how, &hints, &mut trial_placed, &mut trial_hidden, &rels_so_far);
                let cost: f64 = estimator.rows(&steps).iter().sum();
                if best.is_none_or(|(c, _, _)| cost < c) {
                    best = Some((cost, i, how));
                }
            }
        }
        let (_, i, how) = best.expect("a part remains");
        let part = remaining.remove(i);
        let before = placed.clone();
        let (mut steps, rels) = plan_pattern_part(ex, part, how, &hints, &mut placed, &mut hidden, &rels_so_far);
        rels_so_far.extend(rels.iter().cloned());
        let part_vars: HashSet<&str> = part.variables().into_iter().collect();
        let independent = !parts.is_empty()
            && part_vars.iter().all(|v| !before.contains(*v))
            && steps.iter().all(|s| {
                let mut reads = HashSet::new();
                s.reads(&mut reads);
                reads.iter().all(|r| part_vars.contains(r.as_str()))
            });
        if independent {
            // Relationships of earlier parts are checked when the sides are combined.
            for s in &mut steps {
                if let Step::Expand { used, .. } = s {
                    used.retain(|u| !before.contains(u));
                }
            }
        }
        parts.push((steps, before, rels, independent));
    }
    // Per shortest-path part: its index in `parts`, its own variables that only the search
    // binds (relationship and path), and the variables bound once it has run.
    let mut searches: Vec<(usize, Vec<String>, HashSet<String>)> = Vec::new();
    for part in shortest {
        let before = placed.clone();
        let (steps, rel, own) = plan_shortest_part(ex, part, &hints, &mut placed, &mut hidden, &rels_so_far)?;
        rels_so_far.push(rel.clone());
        searches.push((parts.len(), own, placed.clone()));
        parts.push((steps, before, vec![rel], false));
    }

    let mut pending: Vec<Expr> = Vec::new();
    if let Some(w) = where_ {
        let mut cs = Vec::new();
        conjuncts(w, &mut cs);
        pending = cs.into_iter().cloned().collect();
    }
    // Conjuncts on a shortest path's relationships or path run inside its search, when
    // everything else they use is bound by then.
    for (i, own, after) in &searches {
        let mut rest = Vec::new();
        for c in pending {
            let mut vars = HashSet::new();
            expr_vars(&c, &mut vars);
            let uses_own = vars.iter().any(|v| own.contains(v));
            let ready = vars.iter().all(|v| after.contains(v) || !named.contains(v));
            if uses_own && ready {
                for s in &mut parts[*i].0 {
                    if let Step::ShortestPath { predicates, .. } = s {
                        predicates.push(c.clone());
                    }
                }
            } else {
                rest.push(c);
            }
        }
        pending = rest;
    }
    let mut steps = Vec::new();
    let mut left_rels: Vec<String> = Vec::new();
    for (part_steps, before, rels, independent) in parts {
        if !independent {
            steps.extend(part_steps);
            left_rels.extend(rels);
            continue;
        }
        let own: Vec<String> = part_steps
            .iter()
            .flat_map(Step::binds)
            .filter(|v| !is_hidden(v))
            .map(str::to_string)
            .collect();
        let only_own = |vars: &HashSet<String>| !vars.is_empty() && vars.iter().all(|v| own.contains(v));
        let only_before = |vars: &HashSet<String>| !vars.is_empty() && vars.iter().all(|v| before.contains(v));
        let (mut inside, mut keys, mut rest) = (Vec::new(), Vec::new(), Vec::new());
        for c in pending {
            let mut vars = HashSet::new();
            expr_vars(&c, &mut vars);
            if only_own(&vars) {
                inside.push(c); // runs while the right side is built
                continue;
            }
            if let Expr::Binary(graphbuff_cypher::ast::BinaryOp::Eq, a, b) = &c {
                let (mut va, mut vb) = (HashSet::new(), HashSet::new());
                expr_vars(a, &mut va);
                expr_vars(b, &mut vb);
                if only_before(&va) && only_own(&vb) {
                    keys.push(((**a).clone(), (**b).clone()));
                    continue;
                }
                if only_before(&vb) && only_own(&va) {
                    keys.push(((**b).clone(), (**a).clone()));
                    continue;
                }
            }
            rest.push(c);
        }
        pending = rest;
        let right = place_filters(part_steps, inside, &own);
        let right_estimates = Estimator { ex }.rows(&right);
        steps.push(Step::Join {
            right,
            right_estimates,
            keys,
            left_rels: left_rels.clone(),
            right_rels: rels.clone(),
        });
        left_rels.extend(rels);
    }
    let steps = place_filters(steps, pending, &named);
    let estimates = Estimator { ex }.rows(&steps);
    Ok(MatchPlan {
        steps,
        estimates,
        named,
        hidden,
    })
}

/// Steps for a `shortestPath` part: its endpoints (bound or found like a part's first
/// node), then the search. Returns the steps, the relationship column, and the variables
/// only the search binds (named relationship and path).
fn plan_shortest_part(
    ex: &Executor,
    part: &PatternPart,
    hints: &[crate::matcher::SeekHint],
    placed: &mut HashSet<String>,
    hidden: &mut Vec<String>,
    rels_so_far: &[String],
) -> Result<(Vec<Step>, String, Vec<String>)> {
    let (rp, end) = &part.chain[0];
    let mut own = Vec::new();
    for v in [&rp.var, &part.var].into_iter().flatten() {
        if placed.contains(v) {
            return Err(Error::Semantic(format!(
                "variable `{v}` is already bound; shortestPath(...) must bind it"
            )));
        }
        own.push(v.clone());
    }
    let mut name_of = |var: &Option<String>| match var {
        Some(v) => v.clone(),
        None => {
            let h = format!("\u{0}{}", hidden.len());
            hidden.push(h.clone());
            h
        }
    };
    let (from, rel, to) = (name_of(&part.start.var), name_of(&rp.var), name_of(&end.var));
    let mut steps = Vec::new();
    for (var, np) in [(&from, &part.start), (&to, end)] {
        steps.push(Step::NodeStart {
            var: var.clone(),
            node: np.clone(),
            start: choose_start(ex, var, np, hints, placed),
        });
        placed.insert(var.clone());
    }
    steps.push(Step::ShortestPath {
        from,
        to,
        rel: rel.clone(),
        path: part.var.clone(),
        pattern: rp.clone(),
        dir: to_direction(rp.direction),
        all: part.shortest == Some(graphbuff_cypher::ast::Shortest::All),
        used: rels_so_far.to_vec(),
        predicates: Vec::new(),
    });
    placed.insert(rel.clone());
    placed.extend(part.var.clone());
    Ok((steps, rel, own))
}

// ---- estimates ------------------------------------------------------------------------

/// Share of rows a range seek (`<`, `>`, ...) is assumed to keep.
const RANGE_SELECTIVITY: f64 = 0.3;
/// Share of rows a `STARTS WITH` seek is assumed to keep.
const PREFIX_SELECTIVITY: f64 = 0.1;
/// Values an `IN` list is assumed to hold.
const IN_LIST_SIZE: f64 = 3.0;
/// Share of rows a `WHERE` conjunct is assumed to keep.
const FILTER_SELECTIVITY: f64 = 0.5;
/// Share of row pairs a join key is assumed to keep.
const JOIN_KEY_SELECTIVITY: f64 = 0.1;

/// Row estimates for match steps, from the store's statistics: node, label and
/// relationship-type counts, and index sizes. The numbers are deliberately simple: they
/// assume independence and average degrees, and fixed shares for ranges and filters.
pub(crate) struct Estimator<'e, 's> {
    pub ex: &'e Executor<'s>,
}

impl Estimator<'_, '_> {
    fn nodes(&self) -> f64 {
        self.ex.store().node_count().max(1) as f64
    }

    /// Share of all nodes that carry every one of `labels`.
    fn label_share(&self, labels: &[String]) -> f64 {
        let n = self.nodes();
        labels
            .iter()
            .map(|l| self.ex.store().label_count(l) as f64 / n)
            .product()
    }

    /// Entities with `label` (a label, or a relationship type).
    fn population(&self, entity: IndexEntity, label: &str) -> f64 {
        let store = self.ex.store();
        match entity {
            IndexEntity::Node => store.label_count(label) as f64,
            IndexEntity::Relationship => store.relationship_type_count(label) as f64,
        }
    }

    /// Entities a composite index seek is expected to find.
    fn composite_seek(&self, entity: IndexEntity, s: &CompositeSeek) -> f64 {
        match self.ex.store().schema_index_stats(entity, &s.label, &s.keys) {
            Some(st) => st.entries as f64 / st.distinct.max(1) as f64,
            None => self.population(entity, &s.label),
        }
    }

    /// The smallest of the seeks' expected results.
    fn seeks(&self, entity: IndexEntity, sources: &[SeekSource], composite: &[CompositeSeek]) -> f64 {
        let single = sources.iter().map(|s| self.seek(entity, s));
        let multi = composite.iter().map(|c| self.composite_seek(entity, c));
        single.chain(multi).fold(f64::INFINITY, f64::min)
    }

    /// Entities an index seek is expected to find.
    fn seek(&self, entity: IndexEntity, s: &SeekSource) -> f64 {
        let store = self.ex.store();
        let Some(st) = store.schema_index_stats(entity, &s.label, std::slice::from_ref(&s.key)) else {
            return self.population(entity, &s.label);
        };
        let entries = st.entries as f64;
        let per_value = entries / st.distinct.max(1) as f64;
        match s.kind {
            SeekKind::Eq => per_value,
            SeekKind::In => per_value * IN_LIST_SIZE,
            SeekKind::StartsWith => entries * PREFIX_SELECTIVITY,
            _ => entries * RANGE_SELECTIVITY,
        }
    }

    /// Candidates of a start step.
    fn start(&self, node: &NodePattern, start: &Start) -> f64 {
        match start {
            Start::Bound => 1.0,
            Start::AllNodes => self.nodes(),
            Start::LabelScan(l) => {
                let others: Vec<String> = node.labels.iter().filter(|x| *x != l).cloned().collect();
                self.ex.store().label_count(l) as f64 * self.label_share(&others)
            }
            Start::Seek { sources, composite, .. } => self.seeks(IndexEntity::Node, sources, composite),
        }
    }

    /// Average relationships per node of `types` (any type if empty) in `dir`.
    fn degree(&self, types: &[String], dir: Direction) -> f64 {
        let store = self.ex.store();
        let rels = if types.is_empty() {
            store.relationship_count()
        } else {
            types.iter().map(|t| store.relationship_type_count(t)).sum()
        };
        let d = rels as f64 / self.nodes();
        if dir == Direction::Both { 2.0 * d } else { d }
    }

    /// Rows after each of `steps`, starting from one row.
    pub fn rows(&self, steps: &[Step]) -> Vec<f64> {
        let mut rows = 1.0;
        let mut out = Vec::with_capacity(steps.len());
        for step in steps {
            rows *= match step {
                Step::NodeStart { node, start, .. } => self.start(node, start),
                Step::RelSeek {
                    left_node,
                    right_node,
                    dir,
                    sources,
                    composite,
                    ..
                } => {
                    let rels = self.seeks(IndexEntity::Relationship, sources, composite);
                    let ways = if *dir == Direction::Both { 2.0 } else { 1.0 };
                    rels * ways * self.label_share(&left_node.labels) * self.label_share(&right_node.labels)
                }
                Step::Expand {
                    pattern,
                    node,
                    dir,
                    to_bound,
                    ..
                } => {
                    let d = self.degree(&pattern.types, *dir);
                    let paths = match pattern.length {
                        None => d,
                        Some(len) => {
                            // Unbounded lengths are assumed to stop two hops past the minimum.
                            let max = len.max.unwrap_or(len.min.max(1) + 2).min(len.min + 8);
                            (len.min..=max).map(|k| d.powi(k as i32)).sum()
                        }
                    };
                    // An already bound end keeps one in every node; otherwise its labels.
                    let target = if *to_bound {
                        1.0 / self.nodes()
                    } else {
                        self.label_share(&node.labels)
                    };
                    paths * target
                }
                Step::BuildPath { .. } => 1.0,
                Step::ShortestPath { all, .. } => {
                    if *all { 2.0 } else { 1.0 }
                }
                Step::Join {
                    right_estimates,
                    keys,
                    ..
                } => {
                    let right = right_estimates.last().copied().unwrap_or(1.0);
                    right * JOIN_KEY_SELECTIVITY.powi(keys.len() as i32)
                }
                Step::Filter(_) => FILTER_SELECTIVITY,
            };
            out.push(rows);
        }
        out
    }
}

/// An estimate for EXPLAIN: whole numbers from 10 up, one decimal below.
/// Steps for one pattern part, traversed from where `how` says, given the variables
/// bound so far (`placed`, updated). `hidden` collects the names of anonymous columns;
/// `rels_so_far` are the relationship columns of earlier parts. Returns the steps and the
/// part's relationship columns.
#[allow(clippy::too_many_arguments)]
fn plan_pattern_part(
    ex: &Executor,
    part: &PatternPart,
    how: PartStart,
    hints: &[crate::matcher::SeekHint],
    placed: &mut HashSet<String>,
    hidden: &mut Vec<String>,
    rels_so_far: &[String],
) -> (Vec<Step>, Vec<String>) {
    let mut name_of = |var: &Option<String>| match var {
        Some(v) => v.clone(),
        None => {
            let h = format!("\u{0}{}", hidden.len());
            hidden.push(h.clone());
            h
        }
    };
    let mut nodes: Vec<(String, &NodePattern)> = vec![(name_of(&part.start.var), &part.start)];
    let mut rels: Vec<(String, &RelPattern)> = Vec::new();
    for (r, n) in &part.chain {
        rels.push((name_of(&r.var), r));
        nodes.push((name_of(&n.var), n));
    }
    let mut steps = Vec::new();
    let mut used: Vec<String> = rels_so_far.to_vec();
    let n = nodes.len();
    // The hops after the start: (from node, to node, traversed right to left).
    let hops: Vec<(usize, usize, bool)> = match how {
        PartStart::First | PartStart::Last => {
            let reversed = how == PartStart::Last;
            let first = if reversed { n - 1 } else { 0 };
            let (start_var, start_np) = (&nodes[first].0, nodes[first].1);
            steps.push(Step::NodeStart {
                var: start_var.clone(),
                node: start_np.clone(),
                start: choose_start(ex, start_var, start_np, hints, placed),
            });
            placed.insert(start_var.clone());
            if reversed {
                (1..n).rev().map(|i| (i, i - 1, true)).collect()
            } else {
                (0..n - 1).map(|i| (i, i + 1, false)).collect()
            }
        }
        PartStart::Rel(ri) => {
            let (rel, rp) = &rels[ri];
            let (sources, composite) = rel_seeks(ex, rp, hints, placed);
            steps.push(Step::RelSeek {
                rel: rel.clone(),
                left: nodes[ri].0.clone(),
                right: nodes[ri + 1].0.clone(),
                pattern: (*rp).clone(),
                left_node: nodes[ri].1.clone(),
                right_node: nodes[ri + 1].1.clone(),
                dir: to_direction(rp.direction),
                sources,
                composite,
                used: used.clone(),
            });
            used.push(rel.clone());
            placed.insert(rel.clone());
            placed.insert(nodes[ri].0.clone());
            placed.insert(nodes[ri + 1].0.clone());
            let right = (ri + 1..n - 1).map(|i| (i, i + 1, false));
            let left = (1..=ri).rev().map(|i| (i, i - 1, true));
            right.chain(left).collect()
        }
    };
    for (from, to, reversed) in hops {
        let ri = from.min(to); // relationship between nodes ri and ri + 1
        let (rel, rp) = &rels[ri];
        let mut dir = to_direction(rp.direction);
        if reversed {
            dir = dir.reverse();
        }
        let (to_var, to_np) = &nodes[to];
        steps.push(Step::Expand {
            from: nodes[from].0.clone(),
            rel: rel.clone(),
            to: to_var.clone(),
            pattern: (*rp).clone(),
            node: (*to_np).clone(),
            dir,
            rel_bound: placed.contains(rel),
            to_bound: placed.contains(to_var),
            used: used.clone(),
            reversed,
        });
        used.push(rel.clone());
        placed.insert(rel.clone());
        placed.insert(to_var.clone());
    }
    if let Some(pv) = &part.var {
        // A path variable that is already bound is not re-checked (as before).
        if !placed.contains(pv) {
            steps.push(Step::BuildPath {
                var: pv.clone(),
                nodes: nodes.iter().map(|(n, _)| n.clone()).collect(),
                rels: rels
                    .iter()
                    .map(|(r, rp)| (r.clone(), rp.length.is_some()))
                    .collect(),
            });
            placed.insert(pv.clone());
        }
    }
    (steps, rels.into_iter().map(|(r, _)| r).collect())
}

/// How to find candidates for the first node `var` of a part (`np`), given what is bound.
fn choose_start(
    ex: &Executor,
    var: &str,
    start_np: &NodePattern,
    hints: &[crate::matcher::SeekHint],
    placed: &HashSet<String>,
) -> Start {
    if placed.contains(var) {
        return Start::Bound;
    }
    let is_bound = |name: &str| placed.contains(name);
    let store = ex.store();
    let smallest = start_np
        .labels
        .iter()
        .min_by_key(|l| store.label_count(l))
        .cloned();
    let (sources, composite) = index_seeks(
        ex,
        IndexEntity::Node,
        &start_np.labels,
        ex.seek_sources(start_np, hints),
        &is_bound,
    );
    if !sources.is_empty() || !composite.is_empty() {
        Start::Seek {
            sources,
            composite,
            fallback: smallest,
        }
    } else if let Some(l) = smallest {
        Start::LabelScan(l)
    } else {
        Start::AllNodes
    }
}

/// The index seeks usable for an entity with one of `labels` (labels, or relationship
/// types) given its property constraints `found`: single-property seeks, and equality
/// seeks on composite indexes whose every property has an `=` constraint. Constraints
/// whose values need variables that are not bound yet are left out.
fn index_seeks(
    ex: &Executor,
    entity: IndexEntity,
    labels: &[String],
    found: Vec<(String, SeekKind, Expr)>,
    is_bound: &dyn Fn(&str) -> bool,
) -> (Vec<SeekSource>, Vec<CompositeSeek>) {
    if labels.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let store = ex.store();
    let found: Vec<(String, SeekKind, Expr)> = found
        .into_iter()
        .filter(|(_, _, e)| Executor::deps_bound(e, is_bound))
        .collect();
    let sources = found
        .iter()
        .filter_map(|(key, kind, expr)| {
            let label = labels.iter().find(|l| {
                store
                    .schema_index_stats(entity, l, std::slice::from_ref(key))
                    .is_some()
            })?;
            Some(SeekSource {
                label: label.clone(),
                key: key.clone(),
                kind: *kind,
                expr: expr.clone(),
            })
        })
        .collect();
    let mut composite = Vec::new();
    if found.len() > 1 {
        for ix in store.indexes() {
            if ix.entity != entity || ix.is_vector() || ix.properties.len() < 2 || !labels.contains(&ix.label) {
                continue;
            }
            let exprs: Option<Vec<Expr>> = ix
                .properties
                .iter()
                .map(|p| {
                    found
                        .iter()
                        .find(|(k, kind, _)| k == p && *kind == SeekKind::Eq)
                        .map(|(_, _, e)| e.clone())
                })
                .collect();
            if let Some(exprs) = exprs {
                composite.push(CompositeSeek {
                    label: ix.label,
                    keys: ix.properties,
                    exprs,
                });
            }
        }
    }
    (sources, composite)
}

/// The relationship index seeks for `rp` (one type, a single hop), given what is bound.
fn rel_seeks(
    ex: &Executor,
    rp: &RelPattern,
    hints: &[crate::matcher::SeekHint],
    placed: &HashSet<String>,
) -> (Vec<SeekSource>, Vec<CompositeSeek>) {
    if rp.length.is_some() || rp.types.len() != 1 {
        return (Vec::new(), Vec::new());
    }
    let is_bound = |name: &str| placed.contains(name);
    index_seeks(
        ex,
        IndexEntity::Relationship,
        &rp.types,
        ex.seek_sources_of(&rp.props, &rp.var, hints),
        &is_bound,
    )
}

/// Whether relationship `ri` of `part` can start the part with an index seek: none of it
/// and its two nodes is bound yet, and some relationship index applies.
fn rel_seekable(
    ex: &Executor,
    part: &PatternPart,
    ri: usize,
    hints: &[crate::matcher::SeekHint],
    placed: &HashSet<String>,
) -> bool {
    let (rp, right) = &part.chain[ri];
    let left = if ri == 0 { &part.start } else { &part.chain[ri - 1].1 };
    let bound = |v: &Option<String>| v.as_ref().is_some_and(|v| placed.contains(v));
    if bound(&rp.var) || bound(&left.var) || bound(&right.var) {
        return false;
    }
    let (sources, composite) = rel_seeks(ex, rp, hints, placed);
    !sources.is_empty() || !composite.is_empty()
}

/// Inserts the `WHERE`'s conjuncts into `steps`, each right after the step that binds the
/// last of the match's new variables it uses (before all steps if it uses none), so rows
/// that cannot pass are dropped before they are expanded further. Conjuncts at the same
/// place keep their order. A row passes all conjuncts exactly when it passes the whole
/// `WHERE` (an `AND` is true only if every side is).
fn place_filters(steps: Vec<Step>, conjuncts: Vec<Expr>, named: &[String]) -> Vec<Step> {
    if conjuncts.is_empty() {
        return steps;
    }
    // at[i]: conjuncts to run after step i - 1 (at[0]: before the first step).
    let mut at: Vec<Vec<Expr>> = vec![Vec::new(); steps.len() + 1];
    for c in conjuncts {
        let mut deps = HashSet::new();
        expr_vars(&c, &mut deps);
        deps.retain(|v| named.iter().any(|n| n == v));
        let mut bound: HashSet<&str> = HashSet::new();
        let mut pos = 0;
        if !deps.is_empty() {
            pos = steps.len();
            for (i, step) in steps.iter().enumerate() {
                bound.extend(step.binds());
                if deps.iter().all(|d| bound.contains(d.as_str())) {
                    pos = i + 1;
                    break;
                }
            }
        }
        at[pos].push(c);
    }
    let mut out = Vec::with_capacity(steps.len() + at.iter().map(Vec::len).sum::<usize>());
    let mut at = at.into_iter();
    out.extend(at.next().unwrap_or_default().into_iter().map(Step::Filter));
    for (step, filters) in steps.into_iter().zip(at) {
        out.push(step);
        out.extend(filters.into_iter().map(Step::Filter));
    }
    out
}

/// The `AND`-ed parts of `e`, left to right.
fn conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::Binary(graphbuff_cypher::ast::BinaryOp::And, a, b) => {
            conjuncts(a, out);
            conjuncts(b, out);
        }
        other => out.push(other),
    }
}

/// Every variable name `e` mentions, including inside pattern predicates. Names bound
/// locally (list comprehensions, quantifiers) are included too, which can only place a
/// filter later than necessary, never too early.
fn expr_vars(e: &Expr, out: &mut HashSet<String>) {
    e.walk(&mut |x| match x {
        Expr::Var(v) => {
            out.insert(v.clone());
        }
        Expr::Subquery { names, .. } => out.extend(names.iter().cloned()),
        Expr::PatternPredicate(part) => {
            out.extend(part.variables().into_iter().map(str::to_string));
            let nodes = std::iter::once(&part.start).chain(part.chain.iter().map(|(_, n)| n));
            let props = nodes
                .filter_map(|n| n.props.as_ref())
                .chain(part.chain.iter().filter_map(|(r, _)| r.props.as_ref()));
            for p in props {
                expr_vars(p, out);
            }
        }
        _ => {}
    });
}

impl Step {
    /// Variables this step binds.
    pub(crate) fn binds(&self) -> Vec<&str> {
        match self {
            Step::NodeStart { var, .. } => vec![var],
            Step::RelSeek { rel, left, right, .. } => vec![rel, left, right],
            Step::Expand { rel, to, .. } => vec![rel, to],
            Step::BuildPath { var, .. } => vec![var],
            Step::ShortestPath { rel, path, .. } => {
                let mut v = vec![rel.as_str()];
                v.extend(path.as_deref());
                v
            }
            Step::Join { right, .. } => right.iter().flat_map(Step::binds).collect(),
            Step::Filter(_) => Vec::new(),
        }
    }

    /// Variables this step's expressions read (property maps, seek values, filters).
    fn reads(&self, out: &mut HashSet<String>) {
        let props = |np: &NodePattern, out: &mut HashSet<String>| {
            if let Some(p) = &np.props {
                expr_vars(p, out);
            }
        };
        match self {
            Step::NodeStart { node, start, .. } => {
                props(node, out);
                if let Start::Seek { sources, composite, .. } = start {
                    for s in sources {
                        expr_vars(&s.expr, out);
                    }
                    for c in composite {
                        for e in &c.exprs {
                            expr_vars(e, out);
                        }
                    }
                }
            }
            Step::RelSeek {
                pattern,
                left_node,
                right_node,
                sources,
                composite,
                ..
            } => {
                props(left_node, out);
                props(right_node, out);
                if let Some(p) = &pattern.props {
                    expr_vars(p, out);
                }
                for s in sources {
                    expr_vars(&s.expr, out);
                }
                for c in composite {
                    for e in &c.exprs {
                        expr_vars(e, out);
                    }
                }
            }
            Step::Expand { pattern, node, .. } => {
                props(node, out);
                if let Some(p) = &pattern.props {
                    expr_vars(p, out);
                }
            }
            Step::BuildPath { .. } => {}
            Step::ShortestPath { pattern, predicates, .. } => {
                if let Some(p) = &pattern.props {
                    expr_vars(p, out);
                }
                for e in predicates {
                    expr_vars(e, out);
                }
            }
            Step::Join { right, keys, .. } => {
                for s in right {
                    s.reads(out);
                }
                for (l, r) in keys {
                    expr_vars(l, out);
                    expr_vars(r, out);
                }
            }
            Step::Filter(e) => expr_vars(e, out),
        }
    }
}

/// Where a projection may use aggregates: not inside a list comprehension's body, and in
/// `ORDER BY` only as an aggregate the projection also returns.
fn validate_aggregates(p: &Projection) -> Result<()> {
    let mut nested = false;
    for e in p.items.iter().map(|it| &it.expr).chain(p.order_by.iter().map(|s| &s.expr)) {
        e.walk(&mut |x| {
            if let Expr::ListComprehension { filter, map, .. } = x {
                nested |= filter.as_deref().is_some_and(Expr::contains_aggregate)
                    || map.as_deref().is_some_and(Expr::contains_aggregate);
            }
        });
    }
    if nested {
        return Err(Error::Semantic(
            "aggregate functions are not allowed inside a list comprehension".into(),
        ));
    }
    // An aggregate of a random value is not well defined.
    let mut random = false;
    for e in p.items.iter().map(|it| &it.expr) {
        e.walk(&mut |x| {
            if let Expr::Function { args, .. } = x
                && x.is_aggregate_call()
            {
                for a in args {
                    a.walk(&mut |y| random |= matches!(y, Expr::Function { name, .. } if name == "rand"));
                }
            }
        });
    }
    if random {
        return Err(Error::Semantic("rand() cannot be used inside an aggregate function".into()));
    }
    let aggregates = p.items.iter().any(|it| it.expr.contains_aggregate());
    if aggregates {
        // Grouping keys: the items without aggregates.
        let keys: Vec<&Expr> = p.items.iter().map(|it| &it.expr).filter(|e| !e.contains_aggregate()).collect();
        for it in p.items.iter().filter(|it| it.expr.contains_aggregate()) {
            check_grouping(&it.expr, &keys, &[], &mut Vec::new())?;
        }
        // ORDER BY may also use the projected columns by alias.
        let aliases: Vec<&str> = p.items.iter().map(|it| it.alias.as_str()).collect();
        for s in p.order_by.iter().filter(|s| s.expr.contains_aggregate()) {
            check_grouping(&s.expr, &keys, &aliases, &mut Vec::new())?;
        }
    }
    for s in &p.order_by {
        let key = if aggregates { sort_key_over_items(&s.expr, &p.items)? } else { s.expr.clone() };
        if key.contains_aggregate() {
            return Err(Error::Semantic(
                "ORDER BY can only use aggregates that also appear in the projection".into(),
            ));
        }
    }
    Ok(())
}

/// In an expression that aggregates, what is outside the aggregates must be fixed per
/// group: literals, parameters, grouping-key variables (or `aliases`), or a grouping key's
/// property (`n.p` when `n` or `n.p` is a key). Anything else is ambiguous
/// (`me.age + count(*)` without `me` as a key; `a.x + b.x + count(*)` even with
/// `a.x + b.x` as a key). A map projection's own variable is the exception:
/// `p {.name, friends: collect(f)}` groups by `p`.
fn check_grouping(e: &Expr, keys: &[&Expr], aliases: &[&str], local: &mut Vec<String>) -> Result<()> {
    let key_var = |v: &str| keys.iter().any(|k| matches!(k, Expr::Var(x) if x == v)) || aliases.contains(&v);
    let ambiguous = |what: &str| {
        Err(Error::Semantic(format!(
            "`{what}` is used next to an aggregate but is not a grouping key; \
             project it on its own (`WITH {what} AS x, …`) and use the alias"
        )))
    };
    match e {
        _ if e.is_aggregate_call() => Ok(()),
        Expr::Literal(_) | Expr::Param(_) | Expr::AggregateRef(_) | Expr::Subquery { .. } => Ok(()),
        Expr::Var(v) if key_var(v) || local.contains(v) => Ok(()),
        Expr::Var(v) => ambiguous(v),
        Expr::Property(base, key) => match base.as_ref() {
            Expr::Var(v) if key_var(v) || local.contains(v) || keys.contains(&e) => Ok(()),
            Expr::Var(v) => ambiguous(&format!("{v}.{key}")),
            other => check_grouping(other, keys, aliases, local),
        },
        Expr::MapProjection { base, items } => {
            if !matches!(base.as_ref(), Expr::Var(_)) {
                check_grouping(base, keys, aliases, local)?;
            }
            for item in items {
                match item {
                    MapProjectionItem::Entry(_, x) => check_grouping(x, keys, aliases, local)?,
                    MapProjectionItem::Var(v) if !key_var(v) && !local.contains(v) => return ambiguous(v),
                    _ => {}
                }
            }
            Ok(())
        }
        Expr::ListComprehension { var, list, filter, map } => {
            check_grouping(list, keys, aliases, local)?;
            local.push(var.clone());
            let r = [filter, map].into_iter().flatten().try_for_each(|x| check_grouping(x, keys, aliases, local));
            local.pop();
            r
        }
        Expr::Quantified { var, list, pred, .. } => {
            check_grouping(list, keys, aliases, local)?;
            local.push(var.clone());
            let r = check_grouping(pred, keys, aliases, local);
            local.pop();
            r
        }
        Expr::Reduce { acc, init, var, list, expr } => {
            check_grouping(init, keys, aliases, local)?;
            check_grouping(list, keys, aliases, local)?;
            local.push(acc.clone());
            local.push(var.clone());
            let r = check_grouping(expr, keys, aliases, local);
            local.truncate(local.len() - 2);
            r
        }
        _ => typecheck::children(e).into_iter().try_for_each(|x| check_grouping(x, keys, aliases, local)),
    }
}

/// A sort key of an aggregating projection with every part that is a projected item
/// read from that item's column instead (`age + count(x)` with `count(x) AS c`).
fn sort_key_over_items(e: &Expr, items: &[ProjectionItem]) -> Result<Expr> {
    crate::aggregate::rewrite(e, &mut |x| {
        Ok(items
            .iter()
            .rev()
            .find(|it| it.expr == *x && !matches!(x, Expr::Var(_)))
            .map(|it| Expr::Var(it.alias.clone())))
    })
}

fn validate_clause_order(q: &SingleQuery) -> Result<()> {
    let n = q.clauses.len();
    for (i, c) in q.clauses.iter().enumerate() {
        if matches!(c, Clause::Return(_)) && i + 1 != n {
            return Err(Error::Semantic(
                "RETURN can only be used at the end of the query".into(),
            ));
        }
    }
    for c in &q.clauses {
        let where_ = match c {
            Clause::Match { where_, .. } => where_.as_ref(),
            Clause::With(p) => p.where_.as_ref(),
            _ => None,
        };
        if where_.is_some_and(Expr::contains_aggregate) {
            return Err(Error::Semantic(
                "aggregate functions are not allowed in WHERE".into(),
            ));
        }
        if let Clause::With(p) | Clause::Return(p) = c {
            validate_aggregates(p)?;
        }
        if let Clause::CallProcedure { args: Some(args), .. } = c
            && args.iter().any(Expr::contains_aggregate)
        {
            return Err(Error::Semantic("aggregate functions are not allowed in procedure arguments".into()));
        }
    }
    let last = &q.clauses[n - 1];
    let standalone_call = n == 1 && matches!(last, Clause::CallProcedure { .. });
    if !matches!(last, Clause::Return(_)) && !last.is_updating() && !standalone_call {
        return Err(Error::Semantic(format!(
            "query cannot conclude with {} (must be RETURN or an update clause)",
            last.name()
        )));
    }
    Ok(())
}

// ---- EXPLAIN ------------------------------------------------------------------------------

impl Operator {
    pub fn is_update(&self) -> bool {
        matches!(
            self,
            Operator::Create { .. }
                | Operator::Merge { .. }
                | Operator::Set { .. }
                | Operator::Remove { .. }
                | Operator::Delete { .. }
                | Operator::Call { updating: true, .. }
                | Operator::Foreach { .. }
        )
    }

    /// For a read operator: the update its read-only chain starts on (the nearest update
    /// below it), or `None` if the chain goes down to `Start`.
    pub fn chain_base(&self) -> Option<&Operator> {
        let mut op = self.input();
        while let Some(o) = op {
            if o.is_update() {
                return Some(o);
            }
            op = o.input();
        }
        None
    }

    pub(crate) fn input_mut(&mut self) -> Option<&mut Box<Operator>> {
        use Operator as O;
        match self {
            O::Start => None,
            O::Match { input, .. }
            | O::Create { input, .. }
            | O::Merge { input, .. }
            | O::Set { input, .. }
            | O::Remove { input, .. }
            | O::Delete { input, .. }
            | O::Unwind { input, .. }
            | O::Project { input, .. }
            | O::Aggregate { input, .. }
            | O::Distinct { input }
            | O::Sort { input, .. }
            | O::Top { input, .. }
            | O::Skip { input, .. }
            | O::Limit { input, .. }
            | O::Select { input }
            | O::Filter { input, .. }
            | O::Call { input, .. }
            | O::Foreach { input, .. }
            | O::ProcedureCall { input, .. }
            | O::Kernel { input, .. }
            | O::LoadCsv { input, .. } => Some(input),
        }
    }

    pub(crate) fn input(&self) -> Option<&Operator> {
        use Operator as O;
        match self {
            O::Start => None,
            O::Match { input, .. }
            | O::Create { input, .. }
            | O::Merge { input, .. }
            | O::Set { input, .. }
            | O::Remove { input, .. }
            | O::Delete { input, .. }
            | O::Unwind { input, .. }
            | O::Project { input, .. }
            | O::Aggregate { input, .. }
            | O::Distinct { input }
            | O::Sort { input, .. }
            | O::Top { input, .. }
            | O::Skip { input, .. }
            | O::Limit { input, .. }
            | O::Select { input }
            | O::Filter { input, .. }
            | O::Call { input, .. }
            | O::Foreach { input, .. }
            | O::ProcedureCall { input, .. }
            | O::Kernel { input, .. }
            | O::LoadCsv { input, .. } => Some(input),
        }
    }

    /// One line describing this operator (without its input).
    fn describe(&self) -> String {
        use Operator as O;
        let items = |star: bool, items: &[Item]| {
            let mut names: Vec<&str> = Vec::new();
            if star {
                names.push("*");
            }
            names.extend(items.iter().map(|(a, _)| a.as_str()));
            names.join(", ")
        };
        let sort_keys = |keys: &[SortItem]| {
            keys.iter()
                .map(|k| format!("{}{}", expr_text(&k.expr), if k.descending { " DESC" } else { "" }))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            O::Start => "Start".into(),
            O::Match { optional: true, .. } => "Optional".into(),
            O::Match { .. } => unreachable!("a MATCH is explained as its steps"),
            O::Create { patterns, .. } => {
                let pats: Vec<String> = patterns.iter().map(pattern_text).collect();
                format!("Create {}", pats.join(", "))
            }
            O::Merge { pattern, .. } => format!("Merge {}", pattern_text(pattern)),
            O::Set { items, .. } => format!("Set ({} items)", items.len()),
            O::Remove { items, .. } => format!("Remove ({} items)", items.len()),
            O::Delete { detach, exprs, .. } => format!(
                "{} ({} items)",
                if *detach { "DetachDelete" } else { "Delete" },
                exprs.len()
            ),
            O::Unwind { var, .. } => format!("Unwind AS {var}"),
            O::Project { star, items: it, .. } => format!("Project {}", items(*star, it)),
            O::Aggregate { star, items: it, .. } => format!("Aggregate {}", items(*star, it)),
            O::Distinct { .. } => "Distinct".into(),
            O::Sort { keys, .. } => format!("Sort {}", sort_keys(keys)),
            O::Top { keys, skip, limit, .. } => match skip {
                Some(skip) => format!(
                    "Top {} + {} by {}",
                    expr_text(skip),
                    expr_text(limit),
                    sort_keys(keys)
                ),
                None => format!("Top {} by {}", expr_text(limit), sort_keys(keys)),
            },
            O::Skip { count, .. } => format!("Skip {}", expr_text(count)),
            O::Limit { count, .. } => format!("Limit {}", expr_text(count)),
            O::Select { .. } => "Select".into(),
            O::Filter { .. } => "Filter".into(),
            O::Call { imports, .. } if imports.is_empty() => "CallSubquery".into(),
            O::Call { imports, .. } => format!("CallSubquery ({})", imports.join(", ")),
            O::Foreach { var, .. } => format!("Foreach {var}"),
            O::Kernel { op, note, .. } => op.describe(note),
            O::LoadCsv { headers, var, .. } => {
                format!("LoadCsv{} AS {var}", if *headers { " WITH HEADERS" } else { "" })
            }
            O::ProcedureCall {
                procedure, fields, ..
            } => {
                let sig = &procedure.signature;
                let yields: Vec<String> = fields
                    .iter()
                    .map(|(i, v)| match &sig.outputs[*i] {
                        f if f == v => v.clone(),
                        f => format!("{f} AS {v}"),
                    })
                    .collect();
                if yields.is_empty() {
                    format!("ProcedureCall {}()", sig.name)
                } else {
                    format!("ProcedureCall {}() YIELD {}", sig.name, yields.join(", "))
                }
            }
        }
    }

    fn describe_into(&self, out: &mut Lines, mut depth: usize) {
        let mut op = Some(self);
        while let Some(o) = op {
            match o {
                // A match is its steps, last first, on top of its input.
                Operator::Match {
                    optional: false,
                    plan,
                    ..
                } => depth = describe_steps(&plan.steps, &plan.estimates, out, depth),
                // An optional match runs its steps per input row, from an `Argument`.
                Operator::Match {
                    optional: true,
                    plan,
                    ..
                } => {
                    out.push(depth, "Optional".into(), None, Some(profile_key(o)));
                    let d = describe_steps(&plan.steps, &plan.estimates, out, depth + 1);
                    out.push(d, "Argument".into(), None, None);
                    depth += 1;
                }
                // The subquery is a branch under the call, beside its input.
                Operator::Call { plan, .. } | Operator::Foreach { body: plan, .. } => {
                    out.push(depth, o.describe(), None, Some(profile_key(o)));
                    for part in &plan.parts {
                        if let PartPlan::Pipeline { root, .. } = part {
                            root.describe_into(out, depth + 1);
                        }
                    }
                    depth += 1;
                }
                _ => {
                    out.push(depth, o.describe(), None, Some(profile_key(o)));
                    depth += 1;
                }
            }
            op = o.input();
        }
    }
}

/// Adds match steps, last first, each one level deeper; returns the depth after them.
/// A join's build side is an extra branch under it. Each step carries its estimated rows
/// (per input row).
fn describe_steps(steps: &[Step], estimates: &[f64], out: &mut Lines, mut depth: usize) -> usize {
    for (step, rows) in steps.iter().zip(estimates).rev() {
        out.push(depth, step.describe(), Some(*rows), Some(profile_key(step)));
        if let Step::Join {
            right,
            right_estimates,
            ..
        } = step
        {
            describe_steps(right, right_estimates, out, depth + 1);
        }
        depth += 1;
    }
    depth
}

fn shown(name: &str) -> &str {
    if is_hidden(name) { "" } else { name }
}

impl Step {
    fn describe(&self) -> String {
        match self {
            Step::NodeStart { var, node, start } => {
                let n = node_text(&NodePattern {
                    var: Some(shown(var).to_string()).filter(|v| !v.is_empty()),
                    ..node.clone()
                });
                match start {
                    Start::Bound => format!("Argument {n}"),
                    Start::Seek { sources, composite, .. } => {
                        let keys = seek_text(sources, composite, |l| format!(":{l}"));
                        format!("NodeIndexSeek {n} on {keys}")
                    }
                    // Name the scanned label only when there is a choice.
                    Start::LabelScan(l) if node.labels.len() > 1 => {
                        format!("NodeByLabelScan {n} on :{l}")
                    }
                    Start::LabelScan(_) => format!("NodeByLabelScan {n}"),
                    Start::AllNodes => format!("AllNodesScan {n}"),
                }
            }
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
                ..
            } => {
                let named = |var: &str, np: &NodePattern| {
                    node_text(&NodePattern {
                        var: Some(shown(var).to_string()).filter(|v| !v.is_empty()),
                        ..np.clone()
                    })
                };
                let r = rel_text(&RelPattern {
                    var: Some(shown(rel).to_string()).filter(|v| !v.is_empty()),
                    ..pattern.clone()
                });
                let kind = if *dir == Direction::Both { "Undirected" } else { "Directed" };
                let keys = seek_text(sources, composite, |t| format!("[:{t}]"));
                format!(
                    "{kind}RelationshipIndexSeek {}{r}{} on {keys}",
                    named(left, left_node),
                    named(right, right_node)
                )
            }
            Step::Expand {
                from,
                rel,
                to,
                pattern,
                node,
                dir,
                to_bound,
                ..
            } => {
                let r = RelPattern {
                    var: Some(shown(rel).to_string()).filter(|v| !v.is_empty()),
                    direction: match dir {
                        Direction::Outgoing => RelDirection::Right,
                        Direction::Incoming => RelDirection::Left,
                        Direction::Both => RelDirection::Both,
                    },
                    ..pattern.clone()
                };
                let t = NodePattern {
                    var: Some(shown(to).to_string()).filter(|v| !v.is_empty()),
                    ..node.clone()
                };
                format!(
                    "{}({}) ({}){}{}",
                    if pattern.length.is_some() { "VarLengthExpand" } else { "Expand" },
                    if *to_bound { "Into" } else { "All" },
                    shown(from),
                    rel_text(&r),
                    node_text(&t)
                )
            }
            Step::BuildPath { var, .. } => format!("ProjectPath {var}"),
            Step::ShortestPath {
                from,
                to,
                rel,
                path,
                pattern,
                dir,
                all,
                predicates,
                ..
            } => {
                let r = RelPattern {
                    var: Some(shown(rel).to_string()).filter(|v| !v.is_empty()),
                    direction: match dir {
                        Direction::Outgoing => RelDirection::Right,
                        Direction::Incoming => RelDirection::Left,
                        Direction::Both => RelDirection::Both,
                    },
                    ..pattern.clone()
                };
                format!(
                    "{}{} ({}){}({}){}",
                    if *all { "AllShortestPaths" } else { "ShortestPath" },
                    path.as_ref().map(|p| format!(" {p} =")).unwrap_or_default(),
                    shown(from),
                    rel_text(&r),
                    shown(to),
                    if predicates.is_empty() { "" } else { " with predicates" }
                )
            }
            Step::Join { keys, .. } if keys.is_empty() => "CartesianProduct".into(),
            Step::Join { keys, .. } => format!(
                "HashJoin {}",
                keys.iter()
                    .map(|(l, r)| format!("{} = {}", expr_text(l), expr_text(r)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Step::Filter(_) => "Filter".into(),
        }
    }
}

/// A short rendering of an expression: names, literals and parameters; `…` otherwise.
fn expr_text(e: &Expr) -> String {
    match e {
        Expr::Literal(v) => v.to_string(),
        Expr::Param(p) => format!("${p}"),
        Expr::Var(v) => v.clone(),
        Expr::Property(t, k) => format!("{}.{k}", expr_text(t)),
        _ => "…".into(),
    }
}

/// The seeks of a start step for `EXPLAIN`: `:L(k) =` and `:L(a, b) =`, joined by `|`.
fn seek_text(sources: &[SeekSource], composite: &[CompositeSeek], label: impl Fn(&str) -> String) -> String {
    let single = sources
        .iter()
        .map(|s| format!("{}({}) {}", label(&s.label), s.key, s.kind.text()));
    let multi = composite
        .iter()
        .map(|c| format!("{}({}) =", label(&c.label), c.keys.join(", ")));
    single.chain(multi).collect::<Vec<_>>().join(" | ")
}

/// Whether a pattern uses a label or type expression beyond plain labels and types.
fn has_label_expr(p: &PatternPart) -> bool {
    p.start.label_expr.is_some()
        || p.chain
            .iter()
            .any(|(r, n)| r.type_expr.is_some() || n.label_expr.is_some())
}

fn node_text(n: &NodePattern) -> String {
    let mut s = String::from("(");
    if let Some(v) = &n.var {
        s.push_str(v);
    }
    match &n.label_expr {
        Some(e) => {
            s.push(':');
            s.push_str(&e.to_string());
        }
        None => {
            for l in &n.labels {
                s.push(':');
                s.push_str(l);
            }
        }
    }
    if n.props.is_some() {
        s.push_str(" {…}");
    }
    s.push(')');
    s
}

fn rel_text(r: &RelPattern) -> String {
    let mut inner = String::new();
    if let Some(v) = &r.var {
        inner.push_str(v);
    }
    if let Some(e) = &r.type_expr {
        inner.push(':');
        inner.push_str(&e.to_string());
    } else if !r.types.is_empty() {
        inner.push(':');
        inner.push_str(&r.types.join("|"));
    }
    if let Some(len) = r.length {
        inner.push('*');
        inner.push_str(&len.min.to_string());
        inner.push_str("..");
        if let Some(max) = len.max {
            inner.push_str(&max.to_string());
        }
    }
    if r.props.is_some() {
        inner.push_str(" {…}");
    }
    let body = if inner.is_empty() { String::new() } else { format!("[{inner}]") };
    match r.direction {
        RelDirection::Right => format!("-{body}->"),
        RelDirection::Left => format!("<-{body}-"),
        RelDirection::Both => format!("-{body}-"),
    }
}

pub(crate) fn pattern_text(p: &PatternPart) -> String {
    let mut s = String::new();
    if let Some(v) = &p.var {
        s.push_str(v);
        s.push_str(" = ");
    }
    s.push_str(&node_text(&p.start));
    for (r, n) in &p.chain {
        s.push_str(&rel_text(r));
        s.push_str(&node_text(n));
    }
    s
}
