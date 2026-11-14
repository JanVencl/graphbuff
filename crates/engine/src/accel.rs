//! Kernel execution inside query plans.
//!
//! The planner recognizes query parts that the batched kernels of
//! [`graphbuff_core::kernels`] answer exactly, estimates their cost both ways, and when
//! the kernels are cheaper puts an [`Operator::Kernel`] in the plan in place of the row
//! operators. The row operators stay in the plan as its fallback: a kernel that meets
//! something it cannot answer exactly (a property that is not a number, a sum that could
//! round differently, a count that overflows) reports `Unsupported`, and the fallback
//! runs instead, so results never differ.
//!
//! Recognized parts (the whole query part: `MATCH … [WHERE …] RETURN …`, no imports):
//!
//! * **Aggregates over a path of up to two hops:** one pattern of up to two single
//!   relationships (types and direction, no properties), distinct node variables with
//!   labels and constant property maps, a `WHERE` whose conjuncts each test one node
//!   (comparisons of a property with a constant or parameter, `IS [NOT] NULL`, labels,
//!   combined with `AND`, `OR`, `NOT`), and a `RETURN` of only `count(*)`, `count(x)`,
//!   and `count`, `sum`, `avg`, `min`, `max` of one property of the first or last node.
//!   Walk counts are propagated hop by hop over the CSR export; for two hops, walks that
//!   use one relationship twice (which Cypher paths may not) are subtracted. Numeric
//!   aggregates need an integer property whose sums stay below 2^53, where every order
//!   of addition gives the same result.
//! * **`ORDER BY` a property `LIMIT n` over one node pattern** whose row plan scans all
//!   nodes or a label (so its rows come in id order, and ties keep that order): the
//!   kernels filter and pick the first `skip + limit` nodes, and the row operators above
//!   project them.
//!
//! The kernels run on the CSR export and property columns of the committed state the
//! query reads, cached in a [`KernelCache`] until the next commit. Queries inside a
//! transaction, or that write, always run on rows.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use graphbuff_core::{Bits, CmpOp, Column, Csr, Direction, Error, GraphStore, Kernels, Mask, Result, Snapshot, Value, label_mask};
use graphbuff_cypher::ast::{BinaryOp, Clause, Expr, NodePattern, RelDirection, SingleQuery, UnaryOp};

use crate::context::{Executor, Frame, Scope};
use crate::plan::{MatchPlan, Operator, Start, Step};

/// Process-wide counts of kernel operators run ([`kernel_stats`]).
static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FALLBACKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many kernel operators have run in this process, and how many of them handed over
/// to their row operators (a property that is not an integer, say).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KernelStats {
    pub runs: u64,
    pub fallbacks: u64,
}

/// See [`KernelStats`].
pub fn kernel_stats() -> KernelStats {
    use std::sync::atomic::Ordering::Relaxed;
    KernelStats { runs: RUNS.load(Relaxed), fallbacks: FALLBACKS.load(Relaxed) }
}

/// When queries use kernels ([`crate::Database::set_kernel_mode`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum KernelMode {
    /// When the cost model expects them to be cheaper than rows (the default).
    #[default]
    Auto,
    /// Never: every query runs on rows.
    Off,
    /// Whenever a query part can use them, whatever the cost (for testing).
    Always,
}

// ---- cache ----------------------------------------------------------------------------------

/// The CSR export, property columns and label sets of one committed state, shared by the
/// queries that read it and replaced when a query reads a newer one.
#[derive(Default)]
pub(crate) struct KernelCache {
    state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    snapshot: Option<Weak<dyn GraphStore + Send + Sync>>,
    csr: Option<Arc<Csr>>,
    /// Built columns, and properties found not to be numeric (with the reason).
    columns: HashMap<String, std::result::Result<Arc<Column>, String>>,
    labels: HashMap<String, Arc<Bits>>,
}

impl CacheState {
    fn is_for(&self, snapshot: &Snapshot) -> bool {
        self.csr.is_some()
            && self.snapshot.as_ref().is_some_and(|w| std::ptr::addr_eq(w.as_ptr(), Arc::as_ptr(snapshot)))
    }

    /// The export of `snapshot`, building it (and dropping everything older) if needed.
    fn csr(&mut self, snapshot: &Snapshot) -> Result<Arc<Csr>> {
        if let (true, Some(csr)) = (self.is_for(snapshot), &self.csr) {
            return Ok(csr.clone());
        }
        // Drop the stale export before building, so that two never coexist.
        *self = CacheState::default();
        let csr = Arc::new(Csr::build(&**snapshot)?);
        self.snapshot = Some(Arc::downgrade(snapshot));
        self.csr = Some(csr.clone());
        Ok(csr)
    }
}

impl KernelCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The CSR export of `snapshot`: built on the first call after a commit, then shared.
    /// Callers that ask while it is built wait for it.
    pub fn csr(&self, snapshot: &Snapshot) -> Result<Arc<Csr>> {
        self.lock().csr(snapshot)
    }

    /// Property `key` of every node of `snapshot` as a column.
    pub fn column(&self, snapshot: &Snapshot, key: &str) -> Result<Arc<Column>> {
        let mut st = self.lock();
        let csr = st.csr(snapshot)?;
        if let Some(c) = st.columns.get(key) {
            return c.clone().map_err(Error::Unsupported);
        }
        let built = match Column::build(&**snapshot, &csr, key) {
            Ok(c) => Ok(Arc::new(c)),
            Err(Error::Unsupported(m)) => Err(m),
            Err(e) => return Err(e),
        };
        st.columns.insert(key.to_string(), built.clone());
        built.map_err(Error::Unsupported)
    }

    /// The nodes of `snapshot` with `label`.
    pub fn label(&self, snapshot: &Snapshot, label: &str) -> Result<Arc<Bits>> {
        let mut st = self.lock();
        let csr = st.csr(snapshot)?;
        if let Some(b) = st.labels.get(label) {
            return Ok(b.clone());
        }
        let bits = Arc::new(label_mask(&**snapshot, &csr, label)?);
        st.labels.insert(label.to_string(), bits.clone());
        Ok(bits)
    }

    /// What is cached for `snapshot`: whether the export is built, and per property
    /// whether its column is built (`Some(true)`), not numeric (`Some(false)`) or unknown.
    fn cached(&self, snapshot: &Snapshot, keys: &[&str], labels: &[&str]) -> (bool, Vec<Option<bool>>, usize) {
        let st = self.lock();
        if !st.is_for(snapshot) {
            return (false, vec![None; keys.len()], labels.len());
        }
        let cols = keys.iter().map(|k| st.columns.get(*k).map(|c| c.is_ok())).collect();
        let missing = labels.iter().filter(|l| !st.labels.contains_key(**l)).count();
        (true, cols, missing)
    }
}

/// What a read query needs to run kernels: the committed state it reads, its cache, the
/// backend and the mode.
#[derive(Clone)]
pub(crate) struct Accel {
    pub cache: Arc<KernelCache>,
    pub snapshot: Snapshot,
    pub kernels: Arc<dyn Kernels>,
    pub mode: KernelMode,
}

// ---- plans ----------------------------------------------------------------------------------

/// What an [`Operator::Kernel`] computes.
#[derive(Debug, Clone)]
pub(crate) enum KernelOp {
    Aggregate(AggPlan),
    Top(TopPlan),
}

/// Conditions on one node of a pattern: all labels, and every predicate true.
#[derive(Debug, Clone, Default)]
pub(crate) struct NodeSpec {
    labels: Vec<String>,
    preds: Vec<Pred>,
}

/// A predicate on one node, in the forms the kernels evaluate.
#[derive(Debug, Clone)]
enum Pred {
    Cmp { key: String, op: CmpOp, value: Const },
    IsNull { key: String, negated: bool },
    Labels(Vec<String>),
    And(Box<Pred>, Box<Pred>),
    Or(Box<Pred>, Box<Pred>),
    Not(Box<Pred>),
}

#[derive(Debug, Clone)]
enum Const {
    Value(Value),
    Param(String),
}

/// Aggregates over the walks of a path of up to two hops, at its last node.
#[derive(Debug, Clone)]
pub(crate) struct AggPlan {
    nodes: Vec<NodeSpec>,
    hops: Vec<(Vec<String>, Direction)>,
    /// Output column and aggregate, in `RETURN` order.
    aggs: Vec<(String, Agg)>,
    text: String,
}

#[derive(Debug, Clone)]
enum Agg {
    /// `count(*)` or `count(v)` of a pattern variable.
    Rows,
    Count(String),
    Sum(String),
    Avg(String),
    Min(String),
    Max(String),
}

impl Agg {
    fn key(&self) -> Option<&str> {
        match self {
            Agg::Rows => None,
            Agg::Count(k) | Agg::Sum(k) | Agg::Avg(k) | Agg::Min(k) | Agg::Max(k) => Some(k),
        }
    }
}

/// The first `skip + limit` nodes passing `node`, ordered by property `key`.
#[derive(Debug, Clone)]
pub(crate) struct TopPlan {
    var: String,
    node: NodeSpec,
    key: String,
    descending: bool,
    skip: Option<Expr>,
    limit: Expr,
}

impl KernelOp {
    /// One plan line.
    pub fn describe(&self, note: &str) -> String {
        match self {
            KernelOp::Aggregate(a) => format!("KernelAggregate {} ({note})", a.text),
            KernelOp::Top(t) => format!(
                "KernelTop {}.{}{} ({note})",
                t.var,
                t.key,
                if t.descending { " DESC" } else { "" }
            ),
        }
    }
}

// ---- recognition and choice -------------------------------------------------------------------

/// Cost units: one unit is about one row through the row engine.
const CSR_PER_REL: f64 = 0.6;
const CSR_PER_NODE: f64 = 0.5;
const COLUMN_PER_NODE: f64 = 0.4;
const LABEL_PER_NODE: f64 = 0.05;
const HOP_PER_REL: f64 = 0.01;
const PASS_PER_NODE: f64 = 0.01;
/// Below this many estimated rows, `Auto` keeps the row plan.
const MIN_ROWS: f64 = 10_000.0;

/// Puts an [`Operator::Kernel`] into `op` (the row plan of query part `q`) when the part
/// is one kernels answer and the cost model (or [`KernelMode::Always`]) prefers them.
pub(crate) fn accelerate(q: &SingleQuery, ex: &Executor, imported: &[String], mut op: Operator) -> Operator {
    let Some(accel) = &ex.accel else { return op };
    if accel.mode == KernelMode::Off || !imported.is_empty() {
        return op;
    }
    let Some(kop) = recognize(q, &op) else { return op };
    let Some(mplan) = find_match(&op) else { return op };
    // Row cost: the rows of every match step, plus the work above the match.
    let row_cost: f64 = mplan.estimates.iter().sum::<f64>() + mplan.estimates.last().copied().unwrap_or(0.0);
    let Some(kernel_cost) = kernel_cost(accel, ex.store(), &kop) else { return op };
    let chosen = match accel.mode {
        KernelMode::Always => true,
        _ => row_cost >= MIN_ROWS && kernel_cost < row_cost,
    };
    if !chosen {
        return op;
    }
    let note = format!("{}; cost {} vs {} by rows", accel.kernels.name(), short(kernel_cost), short(row_cost));
    let wrap = |fallback: Operator| Operator::Kernel {
        input: Box::new(Operator::Start),
        op: kop.clone(),
        fallback: Box::new(fallback),
        note: note.clone(),
    };
    // The slot to replace: the aggregation (above the match), or the match.
    let mut slot = op.input_mut();
    while let Some(s) = slot {
        let hit = matches!(
            (&kop, &**s),
            (KernelOp::Aggregate(_), Operator::Aggregate { .. }) | (KernelOp::Top(_), Operator::Match { .. })
        );
        if hit {
            let fallback = std::mem::replace(&mut **s, Operator::Start);
            **s = wrap(fallback);
            return op;
        }
        slot = s.input_mut();
    }
    op
}

fn short(x: f64) -> String {
    if x < 1e5 { format!("{x:.0}") } else { format!("{x:.1e}") }
}

fn find_match(op: &Operator) -> Option<&MatchPlan> {
    let mut o = Some(op);
    while let Some(x) = o {
        if let Operator::Match { plan, .. } = x {
            return Some(plan);
        }
        o = x.input();
    }
    None
}

/// The kernels' cost, or `None` if a property they need is known not to be numeric.
fn kernel_cost(accel: &Accel, store: &dyn GraphStore, op: &KernelOp) -> Option<f64> {
    fn walk<'p>(p: &'p Pred, keys: &mut Vec<&'p str>, labels: &mut Vec<&'p str>) {
        match p {
            Pred::Cmp { key, .. } | Pred::IsNull { key, .. } => keys.push(key),
            Pred::Labels(l) => labels.extend(l.iter().map(String::as_str)),
            Pred::And(a, b) | Pred::Or(a, b) => {
                walk(a, keys, labels);
                walk(b, keys, labels);
            }
            Pred::Not(a) => walk(a, keys, labels),
        }
    }
    fn collect<'p>(spec: &'p NodeSpec, keys: &mut Vec<&'p str>, labels: &mut Vec<&'p str>) {
        labels.extend(spec.labels.iter().map(String::as_str));
        for p in &spec.preds {
            walk(p, keys, labels);
        }
    }
    let (mut keys, mut labels): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
    let hops = match op {
        KernelOp::Aggregate(a) => {
            for n in &a.nodes {
                collect(n, &mut keys, &mut labels);
            }
            keys.extend(a.aggs.iter().filter_map(|(_, g)| g.key()));
            a.hops.len()
        }
        KernelOp::Top(t) => {
            collect(&t.node, &mut keys, &mut labels);
            keys.push(&t.key);
            0
        }
    };
    keys.sort_unstable();
    keys.dedup();
    labels.sort_unstable();
    labels.dedup();
    let (built, cols, missing_labels) = accel.cache.cached(&accel.snapshot, &keys, &labels);
    if cols.contains(&Some(false)) {
        return None;
    }
    let n = store.node_count() as f64;
    let r = store.relationship_count() as f64;
    let mut cost = 0.0;
    if !built {
        cost += CSR_PER_REL * r + CSR_PER_NODE * n;
    }
    cost += cols.iter().filter(|c| c.is_none()).count() as f64 * COLUMN_PER_NODE * n;
    cost += missing_labels as f64 * LABEL_PER_NODE * n;
    cost += hops as f64 * HOP_PER_REL * r;
    cost += (keys.len() + labels.len() + hops + 1) as f64 * PASS_PER_NODE * n;
    Some(cost)
}

/// The kernel form of query part `q`, if it has one. `op` is its row plan.
fn recognize(q: &SingleQuery, op: &Operator) -> Option<KernelOp> {
    let [Clause::Match { optional: false, patterns, where_ }, Clause::Return(ret)] = q.clauses.as_slice() else {
        return None;
    };
    let [part] = patterns.as_slice() else { return None };
    if part.var.is_some() || part.shortest.is_some() || part.chain.len() > 2 || ret.distinct {
        return None;
    }
    // Node variables (distinct), relationship variables, conditions per node.
    let mut vars: Vec<Option<String>> = Vec::new();
    let mut nodes: Vec<NodeSpec> = Vec::new();
    let mut rel_vars: Vec<&str> = Vec::new();
    let mut hops = Vec::new();
    let mut add_node = |n: &NodePattern, vars: &mut Vec<Option<String>>| -> Option<()> {
        if let Some(v) = &n.var
            && vars.iter().any(|x| x.as_ref() == Some(v))
        {
            return None;
        }
        vars.push(n.var.clone());
        nodes.push(node_spec(n)?);
        Some(())
    };
    add_node(&part.start, &mut vars)?;
    for (rel, node) in &part.chain {
        if rel.length.is_some() || rel.props.is_some() {
            return None;
        }
        if let Some(v) = &rel.var {
            if vars.iter().any(|x| x.as_deref() == Some(v)) || rel_vars.contains(&v.as_str()) {
                return None;
            }
            rel_vars.push(v);
        }
        let dir = match rel.direction {
            RelDirection::Right => Direction::Outgoing,
            RelDirection::Left => Direction::Incoming,
            RelDirection::Both => Direction::Both,
        };
        hops.push((rel.types.clone(), dir));
        add_node(node, &mut vars)?;
    }
    let pos = |v: &str| vars.iter().position(|x| x.as_deref() == Some(v));
    if let Some(w) = where_ {
        let mut parts = Vec::new();
        conjuncts(w, &mut parts);
        for c in parts {
            let (i, p) = pred(c, &pos)?;
            nodes[i].preds.push(p);
        }
    }

    if !ret.items.is_empty() && ret.items.iter().all(|it| it.expr.is_aggregate_call()) {
        if ret.star || !ret.order_by.is_empty() || ret.skip.is_some() || ret.limit.is_some() {
            return None;
        }
        let mut at: Option<usize> = None;
        let mut aggs = Vec::new();
        for it in &ret.items {
            let (i, a) = agg(&it.expr, &pos, &rel_vars)?;
            if let Some(i) = i {
                if at.is_some_and(|a| a != i) {
                    return None;
                }
                at = Some(i);
            }
            aggs.push((it.alias.clone(), a));
        }
        let last = nodes.len() - 1;
        match at {
            Some(i) if i != 0 && i != last => return None,
            // Aggregated at the first node: walk the path the other way.
            Some(0) if last > 0 => {
                nodes.reverse();
                hops.reverse();
                for h in &mut hops {
                    h.1 = h.1.reverse();
                }
            }
            _ => {}
        }
        let items: Vec<&str> = ret.items.iter().map(|it| it.alias.as_str()).collect();
        let text = format!("{} over {}", items.join(", "), crate::plan::pattern_text(part));
        return Some(KernelOp::Aggregate(AggPlan { nodes, hops, aggs, text }));
    }

    // ORDER BY one property of a single node pattern, with a LIMIT.
    let (Some(limit), [key], true) = (&ret.limit, ret.order_by.as_slice(), part.chain.is_empty()) else {
        return None;
    };
    let var = vars[0].clone()?;
    if ret.items.iter().any(|it| it.expr.contains_aggregate()) {
        return None;
    }
    // An alias that renames the node would change what `var` means in ORDER BY.
    if ret.items.iter().any(|it| it.alias == var && it.expr != Expr::Var(var.clone())) {
        return None;
    }
    let prop = match &key.expr {
        Expr::Property(e, k) if **e == Expr::Var(var.clone()) => k.clone(),
        Expr::Var(alias) => match &ret.items.iter().find(|it| &it.alias == alias)?.expr {
            Expr::Property(e, k) if **e == Expr::Var(var.clone()) => k.clone(),
            _ => return None,
        },
        _ => return None,
    };
    // The row plan must produce its rows in id order, as the kernel's ties do.
    match find_match(op)?.steps.first()? {
        Step::NodeStart { start: Start::AllNodes | Start::LabelScan(_), .. } => {}
        _ => return None,
    }
    Some(KernelOp::Top(TopPlan {
        var,
        node: nodes.pop()?,
        key: prop,
        descending: key.descending,
        skip: ret.skip.clone(),
        limit: limit.clone(),
    }))
}

fn conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::Binary(BinaryOp::And, a, b) => {
            conjuncts(a, out);
            conjuncts(b, out);
        }
        e => out.push(e),
    }
}

fn node_spec(n: &NodePattern) -> Option<NodeSpec> {
    let mut spec = NodeSpec { labels: n.labels.clone(), preds: Vec::new() };
    match &n.props {
        None => {}
        Some(Expr::Map(items)) => {
            for (k, e) in items {
                spec.preds.push(Pred::Cmp { key: k.clone(), op: CmpOp::Eq, value: constant(e)? });
            }
        }
        Some(_) => return None,
    }
    Some(spec)
}

/// A literal (or a signed numeric literal) or a parameter.
fn constant(e: &Expr) -> Option<Const> {
    Some(match e {
        Expr::Literal(v) => Const::Value(v.clone()),
        Expr::Param(p) => Const::Param(p.clone()),
        Expr::Unary(UnaryOp::Neg, inner) => match &**inner {
            Expr::Literal(Value::Int(i)) => Const::Value(Value::Int(i.checked_neg()?)),
            Expr::Literal(Value::Float(f)) => Const::Value(Value::Float(-f)),
            _ => return None,
        },
        Expr::Unary(UnaryOp::Pos, inner) => match &**inner {
            Expr::Literal(v @ (Value::Int(_) | Value::Float(_))) => Const::Value(v.clone()),
            _ => return None,
        },
        _ => return None,
    })
}

/// A predicate on one node variable (its position) in a form the kernels evaluate.
fn pred(e: &Expr, pos: &dyn Fn(&str) -> Option<usize>) -> Option<(usize, Pred)> {
    let prop = |e: &Expr| match e {
        Expr::Property(v, k) => match &**v {
            Expr::Var(v) => pos(v).map(|i| (i, k.clone())),
            _ => None,
        },
        _ => None,
    };
    Some(match e {
        Expr::Binary(op @ (BinaryOp::And | BinaryOp::Or), a, b) => {
            let (i, a) = pred(a, pos)?;
            let (j, b) = pred(b, pos)?;
            if i != j {
                return None;
            }
            let (a, b) = (Box::new(a), Box::new(b));
            (i, if *op == BinaryOp::And { Pred::And(a, b) } else { Pred::Or(a, b) })
        }
        Expr::Unary(UnaryOp::Not, a) => {
            let (i, a) = pred(a, pos)?;
            (i, Pred::Not(Box::new(a)))
        }
        Expr::Binary(op, a, b) => {
            let op = match op {
                BinaryOp::Eq => CmpOp::Eq,
                BinaryOp::Neq => CmpOp::Ne,
                BinaryOp::Lt => CmpOp::Lt,
                BinaryOp::Le => CmpOp::Le,
                BinaryOp::Gt => CmpOp::Gt,
                BinaryOp::Ge => CmpOp::Ge,
                _ => return None,
            };
            if let (Some((i, key)), Some(value)) = (prop(a), constant(b)) {
                (i, Pred::Cmp { key, op, value })
            } else if let (Some(value), Some((i, key))) = (constant(a), prop(b)) {
                (i, Pred::Cmp { key, op: op.flip(), value })
            } else {
                return None;
            }
        }
        Expr::IsNull { expr, negated } => {
            let (i, key) = prop(expr)?;
            (i, Pred::IsNull { key, negated: *negated })
        }
        Expr::HasLabels(v, labels) => match &**v {
            Expr::Var(v) => (pos(v)?, Pred::Labels(labels.clone())),
            _ => return None,
        },
        _ => return None,
    })
}

/// An aggregate the kernels compute, and the node position whose property it reads.
fn agg(e: &Expr, pos: &dyn Fn(&str) -> Option<usize>, rel_vars: &[&str]) -> Option<(Option<usize>, Agg)> {
    match e {
        Expr::CountStar => Some((None, Agg::Rows)),
        Expr::Function { name, distinct: false, args } => match args.as_slice() {
            [Expr::Var(v)] if name == "count" && (pos(v).is_some() || rel_vars.contains(&v.as_str())) => {
                Some((None, Agg::Rows))
            }
            [Expr::Property(v, k)] => {
                let Expr::Var(v) = &**v else { return None };
                let i = pos(v)?;
                let k = k.clone();
                let a = match name.as_str() {
                    "count" => Agg::Count(k),
                    "sum" => Agg::Sum(k),
                    "avg" => Agg::Avg(k),
                    "min" => Agg::Min(k),
                    "max" => Agg::Max(k),
                    _ => return None,
                };
                Some((Some(i), a))
            }
            _ => None,
        },
        _ => None,
    }
}

// ---- execution ------------------------------------------------------------------------------

/// Runs a kernel operator. `Unsupported` means the row plan must run instead.
pub(crate) fn run(ex: &Executor, op: &KernelOp) -> Result<Frame> {
    use std::sync::atomic::Ordering::Relaxed;
    RUNS.fetch_add(1, Relaxed);
    let out = run_kernels(ex, op);
    if matches!(out, Err(Error::Unsupported(_))) {
        FALLBACKS.fetch_add(1, Relaxed);
    }
    out
}

fn run_kernels(ex: &Executor, op: &KernelOp) -> Result<Frame> {
    let accel = ex.accel.as_ref().ok_or_else(|| Error::Unsupported("no kernels for this query".into()))?;
    let csr = accel.cache.csr(&accel.snapshot)?;
    ex.check_interrupt()?;
    match op {
        KernelOp::Aggregate(a) => run_aggregate(ex, accel, &csr, a),
        KernelOp::Top(t) => run_top(ex, accel, &csr, t),
    }
}

/// The nodes passing `spec`, or `None` if every node does.
fn node_mask(ex: &Executor, accel: &Accel, csr: &Csr, spec: &NodeSpec) -> Result<Option<Mask>> {
    let mut mask: Option<Mask> = None;
    let mut and = |m: Mask| {
        mask = Some(match mask.take() {
            Some(x) => x.and(&m),
            None => m,
        })
    };
    if !spec.labels.is_empty() {
        and(labels_mask(accel, csr, &spec.labels)?);
    }
    for p in &spec.preds {
        and(eval_pred(ex, accel, csr, p)?);
    }
    Ok(mask)
}

fn labels_mask(accel: &Accel, csr: &Csr, labels: &[String]) -> Result<Mask> {
    let mut m = Mask::all(csr.node_count(), Some(true));
    for l in labels {
        m = m.and(&Mask::from_bits((*accel.cache.label(&accel.snapshot, l)?).clone()));
    }
    Ok(m)
}

fn eval_pred(ex: &Executor, accel: &Accel, csr: &Csr, p: &Pred) -> Result<Mask> {
    Ok(match p {
        Pred::Cmp { key, op, value } => {
            let value = match value {
                Const::Value(v) => v.clone(),
                Const::Param(name) => ex
                    .params
                    .get(name)
                    .cloned()
                    .ok_or_else(|| Error::Unsupported(format!("parameter ${name} is missing")))?,
            };
            accel.kernels.filter(&*accel.cache.column(&accel.snapshot, key)?, *op, &value)
        }
        Pred::IsNull { key, negated } => {
            let present = Mask::from_bits(accel.cache.column(&accel.snapshot, key)?.present().clone());
            if *negated { present } else { present.not() }
        }
        Pred::Labels(labels) => labels_mask(accel, csr, labels)?,
        Pred::And(a, b) => eval_pred(ex, accel, csr, a)?.and(&eval_pred(ex, accel, csr, b)?),
        Pred::Or(a, b) => eval_pred(ex, accel, csr, a)?.or(&eval_pred(ex, accel, csr, b)?),
        Pred::Not(a) => eval_pred(ex, accel, csr, a)?.not(),
    })
}

fn unsupported(m: &str) -> Error {
    Error::Unsupported(m.into())
}

fn run_aggregate(ex: &Executor, accel: &Accel, csr: &Csr, a: &AggPlan) -> Result<Frame> {
    let k = &*accel.kernels;
    let n = csr.node_count();
    let masks: Vec<Option<Mask>> =
        a.nodes.iter().map(|s| node_mask(ex, accel, csr, s)).collect::<Result<_>>()?;
    let passes = |i: usize, x: u32| masks[i].as_ref().is_none_or(|m| m.passes(x));
    let start: Vec<u64> = (0..n as u32).map(|x| u64::from(passes(0, x))).collect();
    let types: Vec<Vec<u32>> = a.hops.iter().map(|(t, _)| csr.type_ids(t)).collect();
    let hops: Vec<graphbuff_core::Hop> = a
        .hops
        .iter()
        .zip(&types)
        .enumerate()
        .map(|(h, ((_, dir), types))| graphbuff_core::Hop {
            dir: *dir,
            types,
            mask: masks[h + 1].as_ref().map(Mask::passing),
        })
        .collect();
    ex.check_interrupt()?;
    let mut counts = k.walk(csr, &start, &hops)?;
    if a.hops.len() == 2 {
        ex.check_interrupt()?;
        let reused = reused_walks(csr, &passes, &a.hops);
        for (c, r) in counts.iter_mut().zip(reused) {
            *c -= r;
        }
    }
    let rows: u64 = counts.iter().try_fold(0u64, |s, &c| s.checked_add(c)).ok_or_else(|| unsupported("count overflow"))?;
    let rows = i64::try_from(rows).map_err(|_| unsupported("count overflow"))?;
    let mut done: HashMap<&str, graphbuff_core::Aggregates> = HashMap::new();
    let mut values = Vec::with_capacity(a.aggs.len());
    for (_, g) in &a.aggs {
        let Some(key) = g.key() else {
            values.push(Value::Int(rows));
            continue;
        };
        let col = accel.cache.column(&accel.snapshot, key)?;
        if !matches!(g, Agg::Count(_)) {
            // Exact in any order of rows: integers whose sums stay below 2^53 (as
            // integers and as floats), with no integer / float ties in min and max.
            let bound = col.int_bound().ok_or_else(|| unsupported("not an integer column"))?;
            if u128::from(bound) * rows as u128 >= 1 << 53 {
                return Err(unsupported("sums too large to be order-independent"));
            }
        }
        if !done.contains_key(key) {
            done.insert(key, k.aggregate_counts(&col, &counts)?);
        }
        let r = &done[key];
        values.push(match g {
            Agg::Rows => unreachable!(),
            Agg::Count(_) => Value::Int(r.count),
            Agg::Sum(_) => r.sum.clone(),
            Agg::Avg(_) => r.avg.clone(),
            Agg::Min(_) => r.min.clone(),
            Agg::Max(_) => r.max.clone(),
        });
    }
    // Like an aggregation: the values, with an (empty) source scope behind them that the
    // `Select` above drops.
    Ok(Frame {
        scope: Scope::from_names(a.aggs.iter().map(|(n, _)| n.clone()))?,
        rows: vec![values],
        hidden: Some(Scope::default()),
    })
}

/// Per end node, the two-hop walks that use one relationship twice (Cypher paths use a
/// relationship at most once): back and forth over one relationship, or twice around a
/// self-loop. `passes(i, x)`: node `x` passes the conditions on path node `i`.
fn reused_walks(csr: &Csr, passes: &(dyn Fn(usize, u32) -> bool + Sync), hops: &[(Vec<String>, Direction)]) -> Vec<u64> {
    let ((t1, d1), (t2, d2)) = (&hops[0], &hops[1]);
    let second = csr.type_ids(t2);
    let common: Vec<u32> = csr.type_ids(t1).into_iter().filter(|t| second.contains(t)).collect();
    let out = |d: Direction| matches!(d, Direction::Outgoing | Direction::Both);
    let inc = |d: Direction| matches!(d, Direction::Incoming | Direction::Both);
    // Out over r = (x -> y), then back in over r; or in over r = (y -> x), then back out.
    let (there_out, there_in) = (out(*d1) && inc(*d2), inc(*d1) && out(*d2));
    let n = csr.node_count();
    if common.is_empty() {
        return vec![0; n];
    }
    let node = |x: u32| {
        if !passes(0, x) || !passes(2, x) {
            return 0;
        }
        let mut r = 0;
        for &t in &common {
            for &y in csr.adjacency(t, Direction::Outgoing).neighbors(x) {
                if y == x || there_out {
                    r += u64::from(passes(1, y));
                }
            }
            if there_in {
                for &y in csr.adjacency(t, Direction::Incoming).neighbors(x) {
                    if y != x {
                        r += u64::from(passes(1, y));
                    }
                }
            }
        }
        r
    };
    // Independent per node: contiguous ranges on threads once there is enough work.
    let threads = if n < 1 << 16 { 1 } else { std::thread::available_parallelism().map_or(1, |t| t.get()) };
    let step = n.div_ceil(threads).max(1);
    // No threads in a browser.
    #[cfg(target_arch = "wasm32")]
    if threads == 1 {
        return (0..n).map(|x| node(x as u32)).collect();
    }
    std::thread::scope(|s| {
        let parts: Vec<_> = (0..n)
            .step_by(step)
            .map(|lo| {
                let node = &node;
                s.spawn(move || (lo..(lo + step).min(n)).map(|x| node(x as u32)).collect::<Vec<u64>>())
            })
            .collect();
        parts.into_iter().flat_map(|p| p.join().expect("reuse count thread panicked")).collect()
    })
}

fn run_top(ex: &Executor, accel: &Accel, csr: &Csr, t: &TopPlan) -> Result<Frame> {
    let skip = ex.eval_count(t.skip.as_ref(), "SKIP")?.unwrap_or(0);
    let limit = ex.eval_count(Some(&t.limit), "LIMIT")?.unwrap_or(0);
    let col = accel.cache.column(&accel.snapshot, &t.key)?;
    let all: Vec<u32> = (0..csr.node_count() as u32).collect();
    let rows = match node_mask(ex, accel, csr, &t.node)? {
        Some(m) => accel.kernels.select(&all, &m),
        None => all,
    };
    ex.check_interrupt()?;
    let top = accel.kernels.top_n(&col, &rows, skip.saturating_add(limit), t.descending);
    Ok(Frame::new(
        Scope::from_names([t.var.clone()])?,
        top.into_iter().map(|p| vec![Value::Node(csr.node_id(rows[p]))]).collect(),
    ))
}
