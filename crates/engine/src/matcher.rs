//! Pattern matching: finds all bindings of a list of pattern parts in the graph.
//!
//! The matcher is a backtracking depth-first search. For each pattern part it picks
//! a starting end, finds candidates for it, then expands relationship by relationship.
//! Relationship uniqueness holds across all parts of one `MATCH`, as Cypher requires.
//!
//! Choosing where to start (cheapest first):
//! 1. a node variable that is already bound,
//! 2. an index seek: a labelled node with an equality / `IN` constraint on an indexed
//!    property, either inline (`(n:L {k: v})`) or pushed down from `WHERE`
//!    (`WHERE n.k = expr`, where `expr` only uses variables bound at that point),
//! 3. a scan of the node's smallest label,
//! 4. a scan of all nodes.
//!
//! Comma-separated parts are reordered so that parts connected to bound variables or
//! able to use an index go first; this turns `MATCH (a {..}), (b) WHERE b.k = a.k`
//! into a series of index seeks instead of a cartesian product.

use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

use graphbuff_core::{Direction, Error, NodeId, Path, RelId, Result, Scan, Value};
use graphbuff_cypher::ast::{BinaryOp, Expr, NodePattern, PatternPart, RelDirection, RelPattern};

use crate::context::{Env, Executor, Scope};
use crate::ops::cypher_eq;

/// Mutable state of one matching run.
pub(crate) struct MatchState {
    pub row: Vec<Value>,
    /// Which row slots hold a binding (pre-existing variables, or bound during this match).
    pub bound: Vec<bool>,
    /// Relationships already used by the current partial match.
    pub used: Vec<RelId>,
    /// Property constraints pushed down from `WHERE`, usable for index seeks.
    pub hints: Rc<Vec<SeekHint>>,
}

impl MatchState {
    pub fn new(row: Vec<Value>, bound: Vec<bool>) -> Self {
        MatchState {
            row,
            bound,
            used: Vec::new(),
            hints: Rc::default(),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // the legacy executor (tests) uses it
    pub fn with_hints(mut self, hints: Rc<Vec<SeekHint>>) -> Self {
        self.hints = hints;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeekKind {
    /// `n.key = expr`
    Eq,
    /// `n.key IN expr`
    In,
    /// `n.key < expr` (and `expr > n.key`)
    Lt,
    /// `n.key <= expr`
    Le,
    /// `n.key > expr`
    Gt,
    /// `n.key >= expr`
    Ge,
    /// `n.key STARTS WITH expr`
    StartsWith,
}

impl SeekKind {
    /// Equality-style seeks (`=`, `IN`) find few nodes; the others find ranges.
    pub(crate) fn is_exact(self) -> bool {
        matches!(self, SeekKind::Eq | SeekKind::In)
    }

    pub(crate) fn text(self) -> &'static str {
        match self {
            SeekKind::Eq => "=",
            SeekKind::In => "IN",
            SeekKind::Lt => "<",
            SeekKind::Le => "<=",
            SeekKind::Gt => ">",
            SeekKind::Ge => ">=",
            SeekKind::StartsWith => "STARTS WITH",
        }
    }
}

/// A `WHERE` conjunct that constrains a node or relationship property: `var.key = expr`, `var.key IN
/// expr`, a comparison (`var.key < expr`, either way round) or `var.key STARTS WITH expr`.
#[derive(Debug, Clone)]
pub(crate) struct SeekHint {
    pub var: String,
    pub key: String,
    pub kind: SeekKind,
    pub expr: Expr,
}

/// Extracts index-usable conjuncts from a `WHERE` clause. The `WHERE` is still evaluated
/// in full afterwards, so hints only narrow the candidates and never change results.
pub(crate) fn extract_hints(where_: Option<&Expr>, node_vars: &HashSet<&str>) -> Vec<SeekHint> {
    fn conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
        match e {
            Expr::Binary(BinaryOp::And, a, b) => {
                conjuncts(a, out);
                conjuncts(b, out);
            }
            other => out.push(other),
        }
    }
    fn refs_var(e: &Expr, var: &str) -> bool {
        let mut found = false;
        e.walk(&mut |x| found |= matches!(x, Expr::Var(v) if v == var));
        found
    }
    let as_prop = |e: &Expr| -> Option<(String, String)> {
        match e {
            Expr::Property(t, key) => match t.as_ref() {
                Expr::Var(v) if node_vars.contains(v.as_str()) => Some((v.clone(), key.clone())),
                _ => None,
            },
            _ => None,
        }
    };
    let mut parts = Vec::new();
    if let Some(w) = where_ {
        conjuncts(w, &mut parts);
    }
    let mut hints = Vec::new();
    for c in parts {
        let Expr::Binary(op, a, b) = c else { continue };
        use SeekKind as K;
        let candidates: Vec<(&Expr, &Expr, SeekKind)> = match op {
            BinaryOp::Eq => vec![(a, b, K::Eq), (b, a, K::Eq)],
            BinaryOp::In => vec![(a, b, K::In)],
            BinaryOp::Lt => vec![(a, b, K::Lt), (b, a, K::Gt)],
            BinaryOp::Le => vec![(a, b, K::Le), (b, a, K::Ge)],
            BinaryOp::Gt => vec![(a, b, K::Gt), (b, a, K::Lt)],
            BinaryOp::Ge => vec![(a, b, K::Ge), (b, a, K::Le)],
            BinaryOp::StartsWith => vec![(a, b, K::StartsWith)],
            _ => Vec::new(),
        };
        for (prop, other, kind) in candidates {
            if let Some((var, key)) = as_prop(prop) {
                if !refs_var(other, &var) {
                    hints.push(SeekHint {
                        var,
                        key,
                        kind,
                        expr: other.clone(),
                    });
                }
            }
        }
    }
    hints
}

/// Called for each complete match; return `Ok(false)` to stop the search.
pub(crate) type Emit<'e> = dyn FnMut(&MatchState) -> Result<bool> + 'e;

/// A pattern part laid out in the order it will be traversed.
struct Plan<'p> {
    nodes: Vec<&'p NodePattern>,
    rels: Vec<(&'p RelPattern, Direction)>,
    reversed: bool,
}

/// Path trail of the part currently being matched (in traversal order).
#[derive(Default)]
struct Trail {
    nodes: Vec<NodeId>,
    rels: Vec<RelId>,
}

fn to_direction(d: RelDirection) -> Direction {
    match d {
        RelDirection::Right => Direction::Outgoing,
        RelDirection::Left => Direction::Incoming,
        RelDirection::Both => Direction::Both,
    }
}

impl Executor<'_> {
    /// Enumerates all matches of `parts` (starting at `idx`) extending `st`.
    pub(crate) fn match_parts(
        &self,
        scope: &Scope,
        parts: &[PatternPart],
        idx: usize,
        st: &mut MatchState,
        emit: &mut Emit<'_>,
    ) -> Result<bool> {
        if idx == parts.len() {
            return emit(st);
        }
        let part = &parts[idx];
        let plan = self.plan_part(scope, part, st);
        let first = plan.nodes[0];

        // Scans are lazy: a match that stops early (EXISTS, a pattern predicate) reads
        // only as much of the store as it needs.
        let candidates: Scan<'_, NodeId> = match self.bound_node(scope, first, st)? {
            Some(Some(id)) => Box::new(std::iter::once(Ok(id))),
            Some(None) => return Ok(true), // bound to null: nothing can match
            None => match self.index_candidates(first, scope, st)? {
                Some(ids) => Box::new(ids.into_iter().map(Ok)),
                None => self.scan(first),
            },
        };

        let mut trail = Trail::default();
        for c in candidates {
            self.check_interrupt()?;
            let c = c?;
            if !self.node_matches(c, first, scope, st)? {
                continue;
            }
            let saved = bind(scope, st, first.var.as_deref(), Value::Node(c));
            trail.nodes.push(c);
            let cont = self.match_steps(scope, parts, idx, &plan, 0, c, st, &mut trail, emit)?;
            trail.nodes.pop();
            unbind(st, saved);
            if !cont {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether at least one match of `part` exists in the given environment.
    pub(crate) fn pattern_exists(&self, part: &PatternPart, env: &Env) -> Result<bool> {
        let mut scope = env.scope.clone();
        let mut row = env.row.to_vec();
        row.resize(scope.len(), Value::Null);
        let mut bound = vec![true; row.len()];
        for (name, v) in &env.locals {
            let i = scope.add(name);
            if i == row.len() {
                row.push(v.clone());
                bound.push(true);
            } else {
                row[i] = v.clone();
            }
        }
        for v in part.variables() {
            if scope.get(v).is_none() {
                scope.add(v);
                row.push(Value::Null);
                bound.push(false);
            }
        }
        let mut found = false;
        let mut st = MatchState::new(row, bound);
        self.match_parts(&scope, std::slice::from_ref(part), 0, &mut st, &mut |_| {
            found = true;
            Ok(false)
        })?;
        Ok(found)
    }

    fn plan_part<'p>(&self, scope: &Scope, part: &'p PatternPart, st: &MatchState) -> Plan<'p> {
        let mut nodes: Vec<&NodePattern> = vec![&part.start];
        let mut rels = Vec::new();
        for (r, n) in &part.chain {
            rels.push((r, to_direction(r.direction)));
            nodes.push(n);
        }
        let is_bound = |n: &NodePattern| {
            n.var
                .as_deref()
                .and_then(|v| scope.get(v))
                .is_some_and(|i| st.bound[i])
        };
        let bound_now = |name: &str| scope.get(name).is_some_and(|i| st.bound[i]);
        // Lower is cheaper: index seek, then the smallest label scan, then a full scan.
        let selectivity = |n: &NodePattern| {
            if self.seekable(n, &st.hints, &bound_now) {
                return 0;
            }
            n.labels
                .iter()
                .map(|l| self.store().label_count(l).saturating_add(1))
                .min()
                .unwrap_or(usize::MAX)
        };
        let (first, last) = (nodes[0], nodes[nodes.len() - 1]);
        let reverse = !rels.is_empty()
            && !is_bound(first)
            && (is_bound(last) || selectivity(last) < selectivity(first));
        if reverse {
            nodes.reverse();
            rels.reverse();
            for r in &mut rels {
                r.1 = r.1.reverse();
            }
        }
        Plan {
            nodes,
            rels,
            reversed: reverse,
        }
    }

    /// `Some(Some(id))` if the node variable is bound to a node, `Some(None)` if bound to
    /// null, `None` if unbound or anonymous.
    fn bound_node(
        &self,
        scope: &Scope,
        np: &NodePattern,
        st: &MatchState,
    ) -> Result<Option<Option<NodeId>>> {
        let Some(i) = np.var.as_deref().and_then(|v| scope.get(v)) else {
            return Ok(None);
        };
        if !st.bound[i] {
            return Ok(None);
        }
        match &st.row[i] {
            Value::Node(id) => Ok(Some(Some(*id))),
            Value::Null => Ok(Some(None)),
            other => Err(Error::Type(format!(
                "variable `{}` is already bound to a {}, expected a node",
                np.var.as_deref().unwrap_or_default(),
                other.type_name()
            ))),
        }
    }

    /// Equality-style constraints on `np`'s properties: inline map entries and `WHERE` hints.
    pub(crate) fn seek_sources(&self, np: &NodePattern, hints: &[SeekHint]) -> Vec<(String, SeekKind, Expr)> {
        self.seek_sources_of(&np.props, &np.var, hints)
    }

    /// Index-usable constraints on the properties of a node or relationship pattern with
    /// property map `props` and variable `var`.
    pub(crate) fn seek_sources_of(
        &self,
        props: &Option<Expr>,
        var: &Option<String>,
        hints: &[SeekHint],
    ) -> Vec<(String, SeekKind, Expr)> {
        let mut out = Vec::new();
        match props {
            Some(Expr::Map(items)) => {
                for (k, e) in items {
                    out.push((k.clone(), SeekKind::Eq, e.clone()));
                }
            }
            Some(Expr::Param(p)) => {
                if let Some(Value::Map(m)) = self.params.get(p) {
                    for (k, v) in m {
                        out.push((k.clone(), SeekKind::Eq, Expr::Literal(v.clone())));
                    }
                }
            }
            _ => {}
        }
        if let Some(var) = var {
            for h in hints.iter().filter(|h| h.var == *var) {
                out.push((h.key.clone(), h.kind, h.expr.clone()));
            }
        }
        out
    }

    /// Whether every variable `e` uses is bound (so it can be evaluated before the scan).
    pub(crate) fn deps_bound(e: &Expr, is_bound: &dyn Fn(&str) -> bool) -> bool {
        let mut ok = true;
        e.walk(&mut |x| match x {
            Expr::Var(v) => ok &= is_bound(v),
            Expr::PatternPredicate(_) | Expr::Subquery { .. } | Expr::AggregateRef(_) => ok = false,
            _ => {}
        });
        ok
    }

    /// Whether `np` could start with an index seek, given which variables are bound.
    pub(crate) fn seekable(
        &self,
        np: &NodePattern,
        hints: &[SeekHint],
        is_bound: &dyn Fn(&str) -> bool,
    ) -> bool {
        if np.labels.is_empty() {
            return false;
        }
        self.seek_sources(np, hints).iter().any(|(key, kind, e)| {
            kind.is_exact()
                && Self::deps_bound(e, is_bound)
                && np.labels.iter().any(|l| self.store().has_index(l, key))
        })
    }

    /// Candidate nodes for `np` from the most selective usable index, if any.
    fn index_candidates(
        &self,
        np: &NodePattern,
        scope: &Scope,
        st: &MatchState,
    ) -> Result<Option<Vec<NodeId>>> {
        if np.labels.is_empty() {
            return Ok(None);
        }
        let store = self.store();
        let bound_now = |name: &str| scope.get(name).is_some_and(|i| st.bound[i]);
        let env = Env::new(scope, &st.row);
        let mut best: Option<Vec<NodeId>> = None;
        for (key, kind, expr) in self.seek_sources(np, &st.hints) {
            let Some(label) = np.labels.iter().find(|l| store.has_index(l, &key)) else {
                continue;
            };
            if !Self::deps_bound(&expr, &bound_now) {
                continue;
            }
            // Errors are left for the full WHERE / property check to report.
            let Ok(v) = self.eval(&expr, &env) else {
                continue;
            };
            let values = match (kind, v) {
                (SeekKind::Eq, v) => vec![v],
                (SeekKind::In, Value::List(items)) => items,
                (SeekKind::In, Value::Null) => Vec::new(),
                (SeekKind::In, _) => continue,
                (_, _) => continue,
            };
            let mut ids = Vec::new();
            for v in &values {
                ids.extend(store.index_seek(label, &key, v)?.unwrap_or_default());
            }
            if values.len() > 1 {
                ids.sort_unstable();
                ids.dedup();
            }
            if best.as_ref().is_none_or(|b| ids.len() < b.len()) {
                best = Some(ids);
            }
        }
        Ok(best)
    }

    /// Orders comma-separated pattern parts so that each part is, where possible,
    /// connected to already-bound variables or able to start with an index seek.
    #[cfg_attr(not(test), allow(dead_code))] // the legacy executor (tests) orders parts this way
    pub(crate) fn order_parts(
        &self,
        parts: &[PatternPart],
        bound: &HashSet<String>,
        hints: &[SeekHint],
    ) -> Vec<PatternPart> {
        let mut placed = bound.clone();
        let mut remaining: Vec<&PatternPart> = parts.iter().collect();
        let mut out = Vec::with_capacity(parts.len());
        while !remaining.is_empty() {
            let is_bound = |name: &str| placed.contains(name);
            let score = |p: &PatternPart| -> usize {
                let mut nodes = vec![&p.start];
                nodes.extend(p.chain.iter().map(|(_, n)| n));
                if nodes.iter().any(|n| n.var.as_deref().is_some_and(is_bound)) {
                    return 0;
                }
                let ends = [nodes[0], nodes[nodes.len() - 1]];
                if ends.iter().any(|n| self.seekable(n, hints, &is_bound)) {
                    return 1;
                }
                if ends.iter().any(|n| !n.labels.is_empty()) {
                    2
                } else {
                    3
                }
            };
            let best = (0..remaining.len())
                .min_by_key(|i| score(remaining[*i]))
                .unwrap_or(0);
            let part = remaining.remove(best);
            for v in part.variables() {
                placed.insert(v.to_string());
            }
            out.push(part.clone());
        }
        out
    }

    fn scan(&self, np: &NodePattern) -> Scan<'_, NodeId> {
        let store = self.store();
        match np.labels.iter().min_by_key(|l| store.label_count(l)) {
            Some(label) => store.nodes_with_label(label),
            None => store.all_nodes(),
        }
    }

    fn props_match(
        &self,
        expected: &Option<graphbuff_cypher::ast::Expr>,
        scope: &Scope,
        st: &MatchState,
        get: impl Fn(&str) -> Result<Option<Value>>,
    ) -> Result<bool> {
        let Some(expr) = expected else {
            return Ok(true);
        };
        let map = self.eval_map(expr, &Env::new(scope, &st.row))?;
        for (k, v) in &map {
            let actual = get(k)?.unwrap_or_default();
            if cypher_eq(&actual, v) != Some(true) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn node_matches(
        &self,
        id: NodeId,
        np: &NodePattern,
        scope: &Scope,
        st: &MatchState,
    ) -> Result<bool> {
        match self.bound_node(scope, np, st)? {
            Some(Some(b)) if b != id => return Ok(false),
            Some(None) => return Ok(false),
            _ => {}
        }
        let store = self.store();
        for l in &np.labels {
            if !store.node_has_label(id, l)? {
                return Ok(false);
            }
        }
        self.props_match(&np.props, scope, st, |k| store.node_property(id, k))
    }

    fn rel_props_match(
        &self,
        id: RelId,
        rp: &RelPattern,
        scope: &Scope,
        st: &MatchState,
    ) -> Result<bool> {
        let store = self.store();
        self.props_match(&rp.props, scope, st, |k| store.relationship_property(id, k))
    }

    #[allow(clippy::too_many_arguments)]
    fn match_steps(
        &self,
        scope: &Scope,
        parts: &[PatternPart],
        idx: usize,
        plan: &Plan,
        step: usize,
        cur: NodeId,
        st: &mut MatchState,
        trail: &mut Trail,
        emit: &mut Emit<'_>,
    ) -> Result<bool> {
        self.check_interrupt()?;
        if step == plan.rels.len() {
            let path_saved = match &parts[idx].var {
                Some(pv) => {
                    let mut path = Path {
                        nodes: trail.nodes.clone(),
                        rels: trail.rels.clone(),
                    };
                    if plan.reversed {
                        path.nodes.reverse();
                        path.rels.reverse();
                    }
                    bind(scope, st, Some(pv), Value::Path(path))
                }
                None => None,
            };
            let cont = self.match_parts(scope, parts, idx + 1, st, emit)?;
            unbind(st, path_saved);
            return Ok(cont);
        }

        let (rp, dir) = plan.rels[step];
        let target = plan.nodes[step + 1];

        if let Some(len) = rp.length {
            let seg_start = trail.rels.len();
            return self.expand_var_length(
                scope, parts, idx, plan, step, cur, len.min, len.max, seg_start, st, trail, emit,
            );
        }

        let rel_slot = rp.var.as_deref().and_then(|v| scope.get(v));
        let bound_rel = match rel_slot.filter(|i| st.bound[*i]) {
            Some(i) => match &st.row[i] {
                Value::Rel(r) => Some(*r),
                Value::Null => return Ok(true),
                other => {
                    return Err(Error::Type(format!(
                        "variable `{}` is already bound to a {}, expected a relationship",
                        rp.var.as_deref().unwrap_or_default(),
                        other.type_name()
                    )));
                }
            },
            None => None,
        };

        for exp in self.store().expand(cur, dir, &rp.types) {
            let exp = exp?;
            if st.used.contains(&exp.rel) || bound_rel.is_some_and(|b| b != exp.rel) {
                continue;
            }
            if !self.rel_props_match(exp.rel, rp, scope, st)?
                || !self.node_matches(exp.other, target, scope, st)?
            {
                continue;
            }
            let r_saved = bind(scope, st, rp.var.as_deref(), Value::Rel(exp.rel));
            let n_saved = bind(scope, st, target.var.as_deref(), Value::Node(exp.other));
            st.used.push(exp.rel);
            trail.rels.push(exp.rel);
            trail.nodes.push(exp.other);
            let cont = self.match_steps(
                scope,
                parts,
                idx,
                plan,
                step + 1,
                exp.other,
                st,
                trail,
                emit,
            )?;
            trail.nodes.pop();
            trail.rels.pop();
            st.used.pop();
            unbind(st, n_saved);
            unbind(st, r_saved);
            if !cont {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_var_length(
        &self,
        scope: &Scope,
        parts: &[PatternPart],
        idx: usize,
        plan: &Plan,
        step: usize,
        cur: NodeId,
        min: u64,
        max: Option<u64>,
        seg_start: usize,
        st: &mut MatchState,
        trail: &mut Trail,
        emit: &mut Emit<'_>,
    ) -> Result<bool> {
        let (rp, dir) = plan.rels[step];
        let target = plan.nodes[step + 1];
        let depth = (trail.rels.len() - seg_start) as u64;

        if depth >= min && self.node_matches(cur, target, scope, st)? {
            let mut seg: Vec<RelId> = trail.rels[seg_start..].to_vec();
            if plan.reversed {
                seg.reverse();
            }
            let seg_value = Value::List(seg.into_iter().map(Value::Rel).collect());
            let rel_slot = rp.var.as_deref().and_then(|v| scope.get(v));
            let rel_ok = match rel_slot.filter(|i| st.bound[*i]) {
                Some(i) => st.row[i] == seg_value,
                None => true,
            };
            if rel_ok {
                let r_saved = bind(scope, st, rp.var.as_deref(), seg_value);
                let n_saved = bind(scope, st, target.var.as_deref(), Value::Node(cur));
                let cont =
                    self.match_steps(scope, parts, idx, plan, step + 1, cur, st, trail, emit)?;
                unbind(st, n_saved);
                unbind(st, r_saved);
                if !cont {
                    return Ok(false);
                }
            }
        }

        if max.is_some_and(|m| depth >= m) {
            return Ok(true);
        }
        for exp in self.store().expand(cur, dir, &rp.types) {
            let exp = exp?;
            if st.used.contains(&exp.rel) || !self.rel_props_match(exp.rel, rp, scope, st)? {
                continue;
            }
            st.used.push(exp.rel);
            trail.rels.push(exp.rel);
            trail.nodes.push(exp.other);
            let cont = self.expand_var_length(
                scope, parts, idx, plan, step, exp.other, min, max, seg_start, st, trail, emit,
            )?;
            trail.nodes.pop();
            trail.rels.pop();
            st.used.pop();
            if !cont {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Evaluates a property map expression (map literal or parameter).
    pub(crate) fn eval_map(
        &self,
        expr: &graphbuff_cypher::ast::Expr,
        env: &Env,
    ) -> Result<BTreeMap<String, Value>> {
        match self.eval(expr, env)? {
            Value::Map(m) => Ok(m),
            Value::Null => Ok(BTreeMap::new()),
            other => Err(Error::Type(format!(
                "expected a map of properties, got {}",
                other.type_name()
            ))),
        }
    }
}

/// Binds `var` to `value` if it isn't bound yet; returns the slot to restore later.
fn bind(scope: &Scope, st: &mut MatchState, var: Option<&str>, value: Value) -> Option<usize> {
    let i = scope.get(var?)?;
    if st.bound[i] {
        return None;
    }
    st.row[i] = value;
    st.bound[i] = true;
    Some(i)
}

fn unbind(st: &mut MatchState, saved: Option<usize>) {
    if let Some(i) = saved {
        st.row[i] = Value::Null;
        st.bound[i] = false;
    }
}
