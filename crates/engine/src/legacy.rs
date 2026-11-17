//! The clause-by-clause executor that query plans replaced, kept for differential
//! testing: random queries must give the same rows (or the same error), the same update
//! statistics and the same graph through both. Updates call the same per-row code as
//! the planner's update operators, but here every clause materializes all of its rows
//! before the next one starts.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use graphbuff_core::{Error, Result, Value};
use graphbuff_cypher::ast::{Clause, Expr, PatternPart, Projection, Query};

use crate::aggregate::{Accumulator, AggSpec, extract_aggregates};
use crate::context::{Env, Executor, Frame, Row, Scope};
use crate::exec::{RawResult, extend_scope, initial_bound};
use crate::matcher::{MatchState, extract_hints};
use crate::ops::order_cmp;

/// A projected row: (output values, source row, aggregate values).
type ProjectedRow = (Row, Row, Vec<Value>);

impl Executor<'_> {
    // ---- MATCH ------------------------------------------------------------------------

    pub(crate) fn exec_match(
        &mut self,
        optional: bool,
        patterns: &[PatternPart],
        where_: Option<&Expr>,
        frame: Frame,
        cap: Option<usize>,
    ) -> Result<Frame> {
        let full = |out: &Vec<Row>| cap.is_some_and(|c| out.len() >= c);
        let (scope, old_len) = extend_scope(frame.scope, patterns.iter());
        let node_vars: HashSet<&str> = patterns
            .iter()
            .flat_map(|p| std::iter::once(&p.start).chain(p.chain.iter().map(|(_, n)| n)))
            .filter_map(|n| n.var.as_deref())
            .collect();
        let hints = Rc::new(extract_hints(where_, &node_vars));
        let ordered;
        let patterns = if patterns.len() > 1 {
            let bound: HashSet<String> = scope.names()[..old_len].iter().cloned().collect();
            ordered = self.order_parts(patterns, &bound, &hints);
            &ordered[..]
        } else {
            patterns
        };
        let mut out = Vec::new();
        for mut row in frame.rows {
            if full(&out) {
                break;
            }
            row.resize(scope.len(), Value::Null);
            let mut st = MatchState::new(row.clone(), initial_bound(old_len, scope.len()))
                .with_hints(Rc::clone(&hints));
            let mut produced = false;
            self.match_parts(&scope, patterns, 0, &mut st, &mut |st| {
                if let Some(w) = where_ {
                    if !self.eval_predicate(w, &Env::new(&scope, &st.row))? {
                        return Ok(true);
                    }
                }
                out.push(st.row.clone());
                produced = true;
                Ok(!full(&out)) // false stops the matcher
            })?;
            if optional && !produced {
                out.push(row);
            }
        }
        Ok(Frame::new(scope, out))
    }

    pub(crate) fn exec_unwind(&mut self, expr: &Expr, var: &str, frame: Frame, cap: Option<usize>) -> Result<Frame> {
        let mut scope = frame.scope;
        if scope.get(var).is_some() {
            return Err(Error::Semantic(format!(
                "variable `{var}` already declared"
            )));
        }
        let old_len = scope.len();
        scope.add(var);
        let mut rows = Vec::new();
        let cap = cap.unwrap_or(usize::MAX);
        for row in frame.rows {
            if rows.len() >= cap {
                break;
            }
            let v = self.eval(expr, &Env::new(&scope, &row[..old_len]))?;
            let items = match v {
                Value::Null => continue,
                Value::List(items) => items,
                other => vec![other],
            };
            for item in items.into_iter().take(cap - rows.len()) {
                let mut r = row.clone();
                r.push(item);
                rows.push(r);
            }
        }
        Ok(Frame::new(scope, rows))
    }

    /// Runs a query the old way (schema commands are not supported).
    pub(crate) fn run_legacy(&mut self, q: &Query) -> Result<RawResult> {
        // Validation is shared: it moved into the planner unchanged.
        crate::plan::QueryPlan::build(q, self)?;
        let mut result: Option<RawResult> = None;
        for part in &q.parts {
            let caps = self.legacy_row_caps(&part.clauses);
            let mut frame = Frame::new(Scope::default(), vec![Vec::new()]);
            for (clause, cap) in part.clauses.iter().zip(caps) {
                frame = match clause {
                    Clause::Match {
                        optional,
                        patterns,
                        where_,
                    } => self.exec_match(*optional, patterns, where_.as_ref(), frame, cap)?,
                    Clause::Unwind { expr, var } => self.exec_unwind(expr, var, frame, cap)?,
                    Clause::With(p) | Clause::Return(p) => self.legacy_projection(p, frame, cap)?,
                    Clause::Create { patterns } => self.exec_create(patterns, frame)?,
                    Clause::Merge {
                        pattern,
                        on_create,
                        on_match,
                    } => self.exec_merge(pattern, on_create, on_match, frame)?,
                    Clause::Set(items) => {
                        for row in &frame.rows {
                            for item in items {
                                self.apply_set(item, &frame.scope, row)?;
                            }
                        }
                        frame
                    }
                    Clause::Remove(items) => {
                        for row in &frame.rows {
                            for item in items {
                                self.apply_remove(item, &frame.scope, row)?;
                            }
                        }
                        frame
                    }
                    Clause::Delete { detach, exprs } => {
                        self.exec_delete(*detach, exprs, &frame)?;
                        frame
                    }
                    other => panic!("legacy executor does not run {}", other.name()),
                };
            }
            // A part that does not end in RETURN yields nothing.
            let next = if matches!(part.clauses.last(), Some(Clause::Return(_))) {
                RawResult {
                    columns: frame.scope.names().to_vec(),
                    rows: frame.rows,
                }
            } else {
                RawResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                }
            };
            result = Some(match result {
                None => next,
                Some(mut r) => {
                    if next.columns != r.columns {
                        return Err(Error::Semantic(
                            "all sub queries in a UNION must have the same return column names"
                                .into(),
                        ));
                    }
                    r.rows.extend(next.rows);
                    r
                }
            });
        }
        let mut result = result.expect("one part");
        if q.parts.len() > 1 && !q.union_all {
            let mut seen = HashSet::new();
            result.rows.retain(|r| seen.insert(r.clone()));
        }
        Ok(result)
    }

    fn legacy_row_caps(&self, clauses: &[Clause]) -> Vec<Option<usize>> {
        let mut caps = vec![None; clauses.len()];
        let mut need: Option<usize> = None;
        for (i, clause) in clauses.iter().enumerate().rev() {
            caps[i] = need;
            need = match clause {
                Clause::Return(p) | Clause::With(p) => self.legacy_projection_input_need(p, need),
                Clause::Match { optional: true, .. } => need,
                _ => None,
            };
        }
        caps
    }

    /// How many input rows projection `p` needs when `need` of its output rows are used.
    fn legacy_projection_input_need(&self, p: &Projection, need: Option<usize>) -> Option<usize> {
        if p.distinct
            || !p.order_by.is_empty()
            || p.items.iter().any(|it| it.expr.contains_aggregate())
        {
            return None;
        }
        // An invalid SKIP or LIMIT is reported when the projection runs.
        let skip = self.eval_count(p.skip.as_ref(), "SKIP").ok()?.unwrap_or(0);
        let limit = self.eval_count(p.limit.as_ref(), "LIMIT").ok()?;
        // `WITH ... WHERE` filters after SKIP and LIMIT, so only LIMIT bounds its input.
        let out = match (limit, need, p.where_.is_some()) {
            (Some(l), Some(n), false) => Some(l.min(n)),
            (Some(l), _, _) => Some(l),
            (None, n, false) => n,
            (None, _, true) => None,
        };
        out.map(|o| skip.saturating_add(o))
    }


    fn legacy_projection(&mut self, proj: &Projection, frame: Frame, cap: Option<usize>) -> Result<Frame> {
        let mut items: Vec<(String, Expr)> = Vec::new();
        if proj.star {
            // `*` lists the variables in alphabetical order.
            let mut names = frame.scope.names().to_vec();
            names.sort();
            for name in &names {
                items.push((name.clone(), Expr::Var(name.clone())));
            }
        }
        for it in &proj.items {
            items.push((it.alias.clone(), it.expr.clone()));
        }
        let out_scope = Scope::from_names(items.iter().map(|(a, _)| a.clone()))?;

        let mut aggs: Vec<AggSpec> = Vec::new();
        let rewritten: Vec<Expr> = items
            .iter()
            .map(|(_, e)| extract_aggregates(e, &mut aggs))
            .collect::<Result<_>>()?;

        let mut projected: Vec<ProjectedRow> = Vec::new();
        if aggs.is_empty() {
            // Rows past what SKIP and LIMIT keep are never evaluated.
            let needed = self.legacy_projection_input_need(proj, cap).unwrap_or(usize::MAX);
            for row in frame.rows.into_iter().take(needed) {
                let env = Env::new(&frame.scope, &row);
                let vals = rewritten
                    .iter()
                    .map(|e| self.eval(e, &env))
                    .collect::<Result<Row>>()?;
                projected.push((vals, row, Vec::new()));
            }
        } else {
            let key_idx: Vec<usize> = (0..rewritten.len())
                .filter(|i| !rewritten[*i].contains_aggregate())
                .collect();
            let mut group_of: HashMap<Vec<Value>, usize> = HashMap::new();
            let mut groups: Vec<(Vec<Value>, Row, Vec<Accumulator>)> = Vec::new();
            for row in frame.rows {
                let env = Env::new(&frame.scope, &row);
                let key = key_idx
                    .iter()
                    .map(|i| self.eval(&rewritten[*i], &env))
                    .collect::<Result<Vec<_>>>()?;
                let gi = match group_of.get(&key) {
                    Some(gi) => *gi,
                    None => {
                        let accs = aggs
                            .iter()
                            .map(Accumulator::new)
                            .collect::<Result<Vec<_>>>()?;
                        groups.push((key.clone(), row.clone(), accs));
                        group_of.insert(key, groups.len() - 1);
                        groups.len() - 1
                    }
                };
                for (ai, spec) in aggs.iter().enumerate() {
                    let v = match &spec.arg {
                        None => Value::Null,
                        Some(e) => self.eval(e, &env)?,
                    };
                    groups[gi].2[ai].update(v)?;
                }
            }
            if groups.is_empty() && key_idx.is_empty() {
                let accs = aggs
                    .iter()
                    .map(Accumulator::new)
                    .collect::<Result<Vec<_>>>()?;
                groups.push((Vec::new(), vec![Value::Null; frame.scope.len()], accs));
            }
            for (key, first_row, accs) in groups {
                let agg_values: Vec<Value> = accs.into_iter().map(Accumulator::finish).collect();
                let mut vals = Vec::with_capacity(rewritten.len());
                let mut keys = key.into_iter();
                for (i, e) in rewritten.iter().enumerate() {
                    if key_idx.contains(&i) {
                        vals.push(keys.next().unwrap_or_default());
                    } else {
                        let mut env = Env::new(&frame.scope, &first_row);
                        env.aggs = Some(&agg_values);
                        vals.push(self.eval(e, &env)?);
                    }
                }
                projected.push((vals, first_row, agg_values));
            }
        }

        if proj.distinct {
            let mut seen = HashSet::new();
            projected.retain(|(vals, _, _)| seen.insert(vals.clone()));
        }

        if !proj.order_by.is_empty() {
            let mut keyed: Vec<(Vec<Value>, ProjectedRow)> = Vec::with_capacity(projected.len());
            for entry in projected {
                let (vals, src, agg_values) = &entry;
                let mut sort_keys = Vec::with_capacity(proj.order_by.len());
                for s in &proj.order_by {
                    // `ORDER BY <same expression as a projection item>` reuses its value.
                    if let Some(i) = items.iter().position(|(_, e)| *e == s.expr) {
                        sort_keys.push(vals[i].clone());
                        continue;
                    }
                    if s.expr.contains_aggregate() {
                        return Err(Error::Semantic(
                            "ORDER BY can only use aggregates that also appear in the projection"
                                .into(),
                        ));
                    }
                    let mut env = Env::new(&frame.scope, src);
                    env.locals = out_scope
                        .names()
                        .iter()
                        .cloned()
                        .zip(vals.iter().cloned())
                        .collect();
                    env.aggs = Some(agg_values);
                    sort_keys.push(self.eval(&s.expr, &env)?);
                }
                keyed.push((sort_keys, entry));
            }
            keyed.sort_by(|(a, _), (b, _)| {
                for (i, s) in proj.order_by.iter().enumerate() {
                    let o = order_cmp(&a[i], &b[i]);
                    let o = if s.descending { o.reverse() } else { o };
                    if o.is_ne() {
                        return o;
                    }
                }
                std::cmp::Ordering::Equal
            });
            projected = keyed.into_iter().map(|(_, e)| e).collect();
        }

        let skip = self.eval_count(proj.skip.as_ref(), "SKIP")?.unwrap_or(0);
        let limit = self
            .eval_count(proj.limit.as_ref(), "LIMIT")?
            .unwrap_or(usize::MAX);
        let rows: Vec<Row> = projected
            .into_iter()
            .skip(skip)
            .take(limit)
            .map(|(v, _, _)| v)
            .collect();
        let mut frame = Frame::new(out_scope, rows);

        if let Some(w) = &proj.where_ {
            let mut kept = Vec::with_capacity(frame.rows.len());
            for row in frame.rows {
                if self.eval_predicate(w, &Env::new(&frame.scope, &row))? {
                    kept.push(row);
                }
            }
            frame.rows = kept;
        }
        Ok(frame)
    }

}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::context::{Executor, StoreRef};
    use crate::{Database, GraphStore};
    use graphbuff_core::Value as Val;

    /// `Database::explain` without the row estimates, for comparing plan shapes.
    fn shape(db: &Database, q: &str) -> graphbuff_core::Result<String> {
        let plan = db.explain(q)?;
        Ok(plan
            .lines()
            .map(|l| format!("{}
", l.find(" (~").map_or(l, |i| &l[..i])))
            .collect())
    }

    const GRAPH: &str = "
        CREATE (ann:Person {name: 'Ann', age: 34}),
               (bob:Person {name: 'Bob', age: 25}),
               (cat:Person {name: 'Cat', age: 41}),
               (dan:Person:Admin {name: 'Dan', age: 29}),
               (eve:Person {name: 'Eve'}),
               (fay:Person {name: 'Fay', age: 25}),
               (acme:Company {name: 'Acme'}),
               (ann)-[:KNOWS {since: 2010}]->(bob),
               (bob)-[:KNOWS {since: 2015}]->(cat),
               (cat)-[:KNOWS]->(ann),
               (dan)-[:KNOWS {since: 2001}]->(ann),
               (dan)-[:KNOWS]->(eve),
               (eve)-[:KNOWS {since: 2020}]->(eve),
               (ann)-[:WORKS_AT]->(acme),
               (fay)-[:WORKS_AT]->(acme),
               (bob)-[:LIKES {w: 1}]->(cat),
               (bob)-[:LIKES {w: 2}]->(cat),
               (cat)-[:LIKES {w: 1}]->(bob);
        CREATE INDEX person_name FOR (p:Person) ON (p.name);
        CREATE INDEX person_age FOR (p:Person) ON (p.age)";

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
        fn chance(&mut self, pct: u64) -> bool {
            self.next() % 100 < pct
        }
        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.below(items.len())]
        }
    }

    /// A variable visible to the next clause: a node, or a plain value.
    #[derive(Clone)]
    enum Var {
        Node(String),
        Value(String),
    }

    fn sources() -> Vec<(&'static str, Vec<Var>)> {
        let n = |s: &str| Var::Node(s.into());
        let v = |s: &str| Var::Value(s.into());
        vec![
            ("MATCH (p:Person)", vec![n("p")]),
            ("MATCH (p:Person)-[r:KNOWS]->(q)", vec![n("p"), n("q")]),
            ("MATCH (p:Person) OPTIONAL MATCH (p)-[:KNOWS]->(q)", vec![n("p"), n("q")]),
            (
                "UNWIND range(1, 12) AS x MATCH (p:Person) WHERE p.age > x * 3",
                vec![v("x"), n("p")],
            ),
            ("MATCH (p), (q:Person) WHERE id(p) < id(q)", vec![n("p"), n("q")]),
            ("MATCH (p)-[:KNOWS*1..2]->(q)", vec![n("p"), n("q")]),
            ("UNWIND [3, 1, null, 2, 1] AS x", vec![v("x")]),
            // Start-point choice: index seeks (inline, WHERE, IN, from a bound value),
            // a label only at the far end (reversed traversal), no labels at all.
            ("MATCH (p:Person {name: 'Bob'})-->(q)", vec![n("p"), n("q")]),
            ("MATCH (p)-[:KNOWS]->(q:Person) WHERE q.name = 'Ann'", vec![n("p"), n("q")]),
            ("MATCH (p:Person) WHERE p.name IN ['Ann', 'Eve', 'Zed'] MATCH (p)<--(q)", vec![n("p"), n("q")]),
            ("MATCH (a:Person)-->(b) MATCH (p:Person {name: b.name})", vec![n("a"), n("p")]),
            ("MATCH (p)-[:WORKS_AT]->(q:Company)", vec![n("p"), n("q")]),
            ("MATCH (p)--(q)", vec![n("p"), n("q")]),
            // Uniqueness, repeated variables, self-loops, parallel relationships.
            ("MATCH (p)-[:KNOWS]->(q)-[:KNOWS]->(p)", vec![n("p"), n("q")]),
            ("MATCH (p)-->(q), (q)-->(p)", vec![n("p"), n("q")]),
            ("MATCH (p)-[r]->(q), (p)-[s]->(q)", vec![n("p"), n("q")]),
            ("MATCH (p)-[r:LIKES {w: 1}]-(q)", vec![n("p"), n("q")]),
            ("MATCH (p)-[:KNOWS|LIKES]->(q)<-[:KNOWS]-(z)", vec![n("p"), n("q"), n("z")]),
            ("MATCH (p:Person)-[r]->(q) WITH p, r MATCH (a)-[r]->(b)", vec![n("p"), n("a"), n("b")]),
            ("MATCH (p:Person)-->(q) WITH p, q MATCH (p)-->(q)", vec![n("p"), n("q")]),
            // Variable length: zero hops, bounds, bound ends, paths, reversed.
            ("MATCH (p:Person)-[*0..2]->(q)", vec![n("p"), n("q")]),
            ("MATCH (p)-[*2]-(q:Admin)", vec![n("p"), n("q")]),
            ("MATCH (p)-[rs:KNOWS*]->(q) WHERE size(rs) > 1", vec![n("p"), n("q"), v("rs")]),
            ("MATCH (a:Person {name: 'Dan'}), (p:Person) MATCH (a)-[*1..3]->(p)", vec![n("a"), n("p")]),
            ("MATCH w = (p:Person)-[:KNOWS*1..2]->(q)", vec![n("p"), n("q"), v("w")]),
            ("MATCH w = (p)-[:KNOWS]->(q:Person)<-[:LIKES]-(z)", vec![n("p"), n("z"), v("w")]),
            ("MATCH w = (p)-[*0..1]-(q:Company)", vec![n("p"), v("w")]),
            // OPTIONAL: with WHERE, chained, on null-bound variables.
            ("MATCH (p:Person) OPTIONAL MATCH (p)-[r:KNOWS]->(q) WHERE q.age > 30", vec![n("p"), n("q")]),
            (
                "MATCH (p:Person) OPTIONAL MATCH (p)-[:LIKES]->(q) OPTIONAL MATCH (q)-[:KNOWS]->(z)",
                vec![n("p"), n("q"), n("z")],
            ),
            ("OPTIONAL MATCH (p:Nope) MATCH (p)-->(q)", vec![n("p"), n("q")]),
            ("OPTIONAL MATCH (p:Nope) OPTIONAL MATCH (p)-->(q)", vec![n("p"), n("q")]),
            ("MATCH (p:Person) WHERE (p)-[:LIKES]->() AND NOT (p)-[:WORKS_AT]->()", vec![n("p")]),
            // WHERE conjuncts placed at different steps.
            (
                "MATCH (p:Person)-[:KNOWS]->(q)-[:KNOWS]->(z) WHERE p.age > 25 AND z.name <> 'Ann' AND q.age IS NOT NULL",
                vec![n("p"), n("q"), n("z")],
            ),
            ("MATCH (p:Person), (q:Person) WHERE p.age < q.age AND p.name STARTS WITH 'A'", vec![n("p"), n("q")]),
            ("MATCH (p)-[r]->(q) WHERE type(r) = 'KNOWS' AND (q)-[:KNOWS]->(p)", vec![n("p"), n("q")]),
            ("MATCH (p:Person), (q) WHERE (p)-->(q) AND q.name <> p.name", vec![n("p"), n("q")]),
            ("UNWIND [20, 30] AS x MATCH (p:Person)-->(q) WHERE x < 25 AND q.age > x", vec![v("x"), n("q")]),
            (
                "MATCH (p:Person) OPTIONAL MATCH (p)-[:KNOWS]->(q)-[:KNOWS]->(z) WHERE p.age > 30 AND z.age > 30",
                vec![n("p"), n("q"), n("z")],
            ),
            (
                "MATCH (p:Person)-->(q) WHERE all(n IN [p.age, q.age] WHERE n > 20) AND p.name <> 'Bob' OR q.name = 'Eve'",
                vec![n("p"), n("q")],
            ),
            ("MATCH w = (p)-[*1..2]->(q) WHERE length(w) = 2 AND p.age > 26", vec![n("p"), v("w")]),
            // Joins of independent parts: keys, int/float keys, null keys,
            // relationship uniqueness across sides, several joins, inside OPTIONAL,
            // reused across input rows, and a part that seeks on the other side instead.
            ("MATCH (p:Person), (q:Person) WHERE p.age = q.age AND p <> q", vec![n("p"), n("q")]),
            ("MATCH (p:Person)-[r]->(x), (q:Person)-[s]->(y) WHERE p.age = q.age", vec![n("p"), n("x"), n("y")]),
            ("MATCH (p)-[r:KNOWS]->(), ()-[s:KNOWS]->(q) WHERE r.since = s.since + 0", vec![n("p"), n("q")]),
            ("MATCH (p:Person), (q:Person) WHERE p.age = q.age * 1.0", vec![n("p"), n("q")]),
            ("MATCH (p:Person), (q:Person) WHERE p.name = q.name AND q.age > 30", vec![n("p"), n("q")]),
            (
                "MATCH (p:Person) OPTIONAL MATCH (q:Company), (z)-[:WORKS_AT]->(q) WHERE z.age = p.age",
                vec![n("p"), n("q"), n("z")],
            ),
            ("MATCH (p:Person) MATCH (a:Admin), (b) WHERE b.age = a.age", vec![n("p"), n("a"), n("b")]),
            (
                "MATCH (p:Person), (q:Person), (z:Person) WHERE p.age = q.age AND q.age = z.age",
                vec![n("p"), n("q"), n("z")],
            ),
            ("MATCH (p:Person), w = (q)-[*1..2]->(z) WHERE p.age = z.age", vec![n("p"), v("w")]),
            ("MATCH (p:Person)-[r]->(x), (q)-[s*1..2]->(y) WHERE p.name = q.name", vec![n("p"), n("y")]),
            ("MATCH (p:Person), (q) WHERE p.age = q.age OR q:Company", vec![n("p"), n("q")]),
            // Range and prefix seeks.
            ("MATCH (p:Person) WHERE p.age > 26", vec![n("p")]),
            ("MATCH (p:Person) WHERE p.age >= 25 AND p.age < 34", vec![n("p")]),
            ("MATCH (p:Person) WHERE 30 > p.age", vec![n("p")]),
            ("MATCH (p:Person) WHERE p.age > 25.5", vec![n("p")]),
            ("MATCH (p:Person) WHERE p.age > 20 AND p.age > 28 AND p.age <= 34", vec![n("p")]),
            ("MATCH (p:Person) WHERE p.age > 30 AND p.age < 'z'", vec![n("p")]),
            ("MATCH (p:Person) WHERE p.age > null", vec![n("p")]),
            ("MATCH (p:Person) WHERE p.name STARTS WITH 'C'", vec![n("p")]),
            ("MATCH (p:Person) WHERE p.name STARTS WITH null", vec![n("p")]),
            ("MATCH (p:Person) WHERE p.name >= 'B' AND p.name < 'E'", vec![n("p")]),
            ("MATCH (p:Person) WHERE p.name STARTS WITH 'D' OR p.age < 26", vec![n("p")]),
            ("UNWIND [20, 30, null, 'x'] AS x MATCH (p:Person) WHERE p.age > x", vec![v("x"), n("p")]),
            ("MATCH (a:Admin) MATCH (p:Person) WHERE p.age < a.age", vec![n("a"), n("p")]),
            ("MATCH (p:Person)-[:KNOWS]->(q:Person) WHERE q.age < 30", vec![n("p"), n("q")]),
        ]
    }

    fn plain_exprs(vars: &[Var]) -> Vec<(String, bool)> {
        // (expression, is_node)
        let mut out = Vec::new();
        for var in vars {
            match var {
                Var::Node(n) => {
                    out.push((n.clone(), true));
                    out.push((format!("{n}.name"), false));
                    out.push((format!("{n}.age"), false));
                    out.push((format!("coalesce({n}.age, 0) % 3"), false));
                }
                Var::Value(v) => {
                    out.push((v.clone(), false));
                    out.push((format!("{v} * 2"), false));
                }
            }
        }
        out
    }

    /// One `WITH` or `RETURN` with random items, DISTINCT, ORDER BY, SKIP, LIMIT, WHERE.
    fn projection(rng: &mut Rng, vars: &[Var], last: bool) -> (String, Vec<Var>) {
        let plain = plain_exprs(vars);
        let aggs = ["count(*)", "count({e})", "collect({e})", "min({e})", "max({e})", "sum({n})"];
        let star = !last && rng.chance(10) || last && rng.chance(5);
        let mut items: Vec<(String, String, bool)> = Vec::new(); // (expr, alias, is_node)
        let mut next: Vec<Var> = if star { vars.to_vec() } else { Vec::new() };
        for i in 0..rng.below(3) + 1 {
            let (expr, is_node) = if rng.chance(30) {
                let e = &plain[rng.below(plain.len())].0;
                let numeric = plain
                    .iter()
                    .filter(|(x, _)| x.ends_with(".age") || x.ends_with("* 2"))
                    .map(|(x, _)| x.clone())
                    .collect::<Vec<_>>();
                let n = numeric.first().cloned().unwrap_or_else(|| "1".into());
                (rng.pick(&aggs).replace("{e}", e).replace("{n}", &n), false)
            } else {
                plain[rng.below(plain.len())].clone()
            };
            // Nodes keep their name so later clauses can use their properties.
            let alias = if is_node && !items.iter().any(|it| it.1 == expr) && !star {
                expr.clone()
            } else {
                format!("c{i}")
            };
            if items.iter().any(|it| it.1 == alias) {
                continue;
            }
            if star && vars.iter().any(|v| matches!(v, Var::Node(n) | Var::Value(n) if *n == alias)) {
                continue;
            }
            next.push(if is_node {
                Var::Node(alias.clone())
            } else {
                Var::Value(alias.clone())
            });
            items.push((expr, alias, is_node));
        }
        if items.is_empty() {
            next.push(Var::Value("c9".into()));
            items.push(("1".into(), "c9".into(), false));
        }
        let mut q = String::from(if last { "RETURN " } else { "WITH " });
        let distinct = rng.chance(30);
        if distinct {
            q.push_str("DISTINCT ");
        }
        // After DISTINCT or an aggregation, ORDER BY sees only the projected columns.
        let is_agg = |e: &str| ["count(", "collect(", "min(", "max(", "sum("].iter().any(|f| e.starts_with(f));
        let grouped = !star && (distinct || items.iter().any(|(e, _, _)| is_agg(e)));
        let mut parts: Vec<String> = Vec::new();
        if star {
            parts.push("*".into());
        }
        parts.extend(items.iter().map(|(e, a, _)| {
            if e == a { e.clone() } else { format!("{e} AS {a}") }
        }));
        q.push_str(&parts.join(", "));
        if rng.chance(50) {
            let mut keys = Vec::new();
            for _ in 0..rng.below(2) + 1 {
                let key = match rng.below(3) {
                    0 => items[rng.below(items.len())].1.clone(), // an alias
                    1 => items[rng.below(items.len())].0.clone(), // an item expression
                    _ if grouped => {
                        rng.below(plain.len());
                        items[0].1.clone()
                    }
                    _ => plain[rng.below(plain.len())].0.clone(), // a source expression
                };
                keys.push(if rng.chance(40) { format!("{key} DESC") } else { key });
            }
            q.push_str(" ORDER BY ");
            q.push_str(&keys.join(", "));
        }
        if rng.chance(25) {
            q.push_str(&format!(" SKIP {}", rng.below(4)));
        }
        if rng.chance(40) {
            q.push_str(&format!(" LIMIT {}", rng.below(6)));
        }
        if !last && rng.chance(20) {
            let v = match &next[rng.below(next.len())] {
                Var::Node(n) => format!("{n}.age IS NOT NULL"),
                Var::Value(v) => format!("{v} IS NOT NULL"),
            };
            q.push_str(&format!(" WHERE {v}"));
        }
        (q, next)
    }

    fn random_query(rng: &mut Rng) -> String {
        let srcs = sources();
        let (src, mut vars) = srcs[rng.below(srcs.len())].clone();
        let mut q = src.to_string();
        for _ in 0..rng.below(3) {
            let (with, next) = projection(rng, &vars, false);
            q.push(' ');
            q.push_str(&with);
            vars = next;
        }
        let (ret, _) = projection(rng, &vars, true);
        q.push(' ');
        q.push_str(&ret);
        if rng.chance(10) {
            let (src2, vars2) = srcs[rng.below(srcs.len())].clone();
            let (ret2, _) = projection(rng, &vars2, true);
            q.push_str(if rng.chance(50) { " UNION ALL " } else { " UNION " });
            q.push_str(src2);
            q.push(' ');
            q.push_str(&ret2);
        }
        q
    }

    /// A value with list order made irrelevant (if `unordered`), for comparing results.
    fn canonical(v: &Val, unordered: bool) -> String {
        match v {
            Val::List(items) if unordered => {
                let mut parts: Vec<String> = items.iter().map(|x| canonical(x, true)).collect();
                parts.sort();
                format!("[{}]", parts.join(", "))
            }
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn plans_match_the_clause_by_clause_executor() {
        let db = Database::in_memory();
        for stmt in graphbuff_cypher::split_statements(GRAPH).unwrap() {
            db.execute(stmt).unwrap();
        }
        let params = BTreeMap::new();
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let (mut ok, mut errors, mut lazier, mut reordered) = (0, 0, 0, 0);
        for i in 0..6000 {
            let q = random_query(&mut rng);
            let ast = graphbuff_cypher::parse(&q).unwrap_or_else(|e| panic!("query {i} `{q}`: {e}"));
            let (new, old) = db.read(|s| {
                let new = Executor::new(StoreRef::Read(s), &params).run(&ast);
                let old = Executor::new(StoreRef::Read(s), &params).run_legacy(&ast);
                (new, old)
            });
            if new.is_ok() {
                assert!(db.explain(&q).is_ok(), "EXPLAIN query {i} `{q}`");
            }
            // The one intended difference: under a LIMIT, streams evaluate only the rows
            // that are asked for, while the old executor evaluated more (rows it then
            // skipped, everything below a DISTINCT or below `LIMIT 0`) and could fail on
            // one of them. Such queries only have to agree when the old executor
            // succeeded; without a LIMIT they must always agree.
            let lazy = q.contains("LIMIT") && old.is_err();
            // The planner may match parts in another order or direction than the old
            // executor, and Cypher fixes row order only with ORDER BY. Without
            // SKIP / LIMIT, the rows must be the same multiset (lists from `collect`
            // compared as multisets too). With them, a different row order legitimately
            // picks different rows, so only success is compared.
            let subset = q.contains("LIMIT") || q.contains("SKIP");
            let unordered_lists = q.contains("collect(");
            match (new, old) {
                (Ok(a), Ok(b)) if subset => {
                    assert_eq!(a.columns, b.columns, "query {i} `{q}`");
                    if a.rows == b.rows {
                        ok += 1;
                    } else {
                        reordered += 1;
                    }
                }
                (Ok(a), Ok(b)) => {
                    assert_eq!(a.columns, b.columns, "query {i} `{q}`");
                    let canon = |rows: &[Vec<Val>]| {
                        let mut out: Vec<String> = rows
                            .iter()
                            .map(|r| format!("{:?}", r.iter().map(|v| canonical(v, unordered_lists)).collect::<Vec<_>>()))
                            .collect();
                        out.sort();
                        out
                    };
                    assert_eq!(canon(&a.rows), canon(&b.rows), "query {i} `{q}`");
                    ok += 1;
                }
                // A different evaluation order can meet a different failing row first.
                (Err(_), Err(_)) => errors += 1,
                _ if lazy => lazier += 1,
                (a, b) => panic!(
                    "query {i} `{q}`: planner {:?}, legacy {:?}",
                    a.map(|r| r.rows.len()),
                    b.map(|r| r.rows.len())
                ),
            }
        }
        eprintln!(
            "differential: {ok} equal results, {reordered} reordered under SKIP/LIMIT, {errors} errors, {lazier} lazier"
        );
        // The generator must mostly produce valid queries to be a useful test.
        assert!(ok > 3500, "only {ok} successful queries ({errors} errors)");
        assert!(lazier < 300, "{lazier} differences under a LIMIT");
    }

    // ---- writes -----------------------------------------------------------------------

    /// A random value expression over `vars` (never a node, so it can be a property).
    fn value_expr(rng: &mut Rng, vars: &[Var]) -> String {
        let plain: Vec<String> = plain_exprs(vars)
            .into_iter()
            .filter(|(e, node)| !node && !e.ends_with("* 2"))
            .map(|(e, _)| e)
            .collect();
        if plain.is_empty() || rng.chance(20) {
            return rng.pick(&["1", "'x'", "2.5", "null", "[1, 2]"]).to_string();
        }
        plain[rng.below(plain.len())].clone()
    }

    /// One update clause on `vars`; returns it and the variables visible after it.
    ///
    /// Each update must give the same graph whatever order the rows come in, because
    /// the planner may produce them in another order than the old executor. So a
    /// `SET` on `n` only reads `n` itself or constants (a node met on several rows with
    /// different values would otherwise keep whichever came last), and counters only add.
    fn update_clause(rng: &mut Rng, vars: &[Var]) -> (String, Vec<Var>) {
        let nodes: Vec<&str> = vars
            .iter()
            .filter_map(|v| match v {
                Var::Node(n) => Some(n.as_str()),
                Var::Value(_) => None,
            })
            .collect();
        let mut next = vars.to_vec();
        let n = if nodes.is_empty() { None } else { Some(nodes[rng.below(nodes.len())]) };
        let pick = if n.is_some() { rng.below(13) } else { 6 + rng.below(2) * 2 };
        let n = n.unwrap_or("");
        let clause = match pick {
            0 => format!("SET {n}.age = coalesce({n}.age, 0) + 1"),
            1 => format!("SET {n}.tag = {n}.name + '!', {n}.k = 1"),
            2 => format!("SET {n}:Seen"),
            3 => format!("SET {n} += {{k: 2, age: null}}"),
            4 => format!("REMOVE {n}.age"),
            5 => format!("REMOVE {n}:Admin, {n}.name"),
            6 => format!("CREATE (:New {{v: {}}})", value_expr(rng, vars)),
            7 => format!("CREATE ({n})-[:MADE {{v: {}}}]->(:New)", value_expr(rng, vars)),
            8 => {
                let m = format!("m{}", rng.below(1000));
                next.push(Var::Node(m.clone()));
                format!(
                    "MERGE ({m}:City {{name: coalesce({}, 'none')}}) ON CREATE SET {m}.c = 1 ON MATCH SET {m}.c = {m}.c + 1",
                    value_expr(rng, vars)
                )
            }
            9 => format!("MERGE ({n})-[:SEEN]->(:Mark {{v: 1}})"),
            10 | 11 => {
                // Deleted nodes cannot be read afterwards.
                next.retain(|v| !matches!(v, Var::Node(x) if x == n));
                format!("{}DELETE {n}", if pick == 10 { "DETACH " } else { "" })
            }
            _ => {
                let other = nodes[rng.below(nodes.len())];
                format!("CREATE ({n})-[:PAIR]->({other})")
            }
        };
        (clause, next)
    }

    /// Like [`projection`], but mostly without SKIP / LIMIT, under which results can
    /// only be compared for success.
    fn write_projection(rng: &mut Rng, vars: &[Var], last: bool) -> (String, Vec<Var>) {
        let keep_limits = rng.chance(20);
        loop {
            let (p, next) = projection(rng, vars, last);
            if keep_limits || !(p.contains("LIMIT") || p.contains("SKIP")) {
                return (p, next);
            }
        }
    }

    fn random_write_query(rng: &mut Rng) -> String {
        let srcs = sources();
        let (src, mut vars) = srcs[rng.below(srcs.len())].clone();
        let mut q = src.to_string();
        if rng.chance(30) {
            let (with, next) = write_projection(rng, &vars, false);
            q = format!("{q} {with}");
            vars = next;
        }
        for _ in 0..rng.below(3) + 1 {
            let (update, next) = update_clause(rng, &vars);
            q = format!("{q} {update}");
            vars = next;
            if !vars.is_empty() && rng.chance(20) {
                let (with, next) = write_projection(rng, &vars, false);
                q = format!("{q} {with}");
                vars = next;
            }
        }
        if vars.is_empty() {
            q.push_str(" RETURN count(*) AS c");
        } else if rng.chance(60) {
            let (ret, _) = write_projection(rng, &vars, true);
            q = format!("{q} {ret}");
        }
        q
    }

    /// Runs `ast` in its own transaction, through the planner or the old executor.
    fn run_write(
        db: &Database,
        ast: &graphbuff_cypher::ast::Query,
        legacy: bool,
    ) -> graphbuff_core::Result<crate::QueryResult> {
        let params = BTreeMap::new();
        if !legacy {
            return db.execute_query(ast, &params);
        }
        let mut guard = db.store.write().unwrap();
        GraphStore::begin(&mut *guard)?;
        let outcome = {
            let mut ex = Executor::new(StoreRef::Write(&mut *guard), &params);
            ex.run_legacy(ast).and_then(|raw| crate::finish(raw, &ex))
        };
        match outcome {
            Ok(r) => {
                GraphStore::commit(&mut *guard)?;
                let _ = db.publish(&guard);
                Ok(r)
            }
            Err(e) => {
                GraphStore::rollback(&mut *guard);
                Err(e)
            }
        }
    }

    /// Property maps without entity ids, lists as multisets if `unordered`.
    fn canonical_props(m: &BTreeMap<String, Val>, unordered: bool) -> String {
        m.iter()
            .map(|(k, v)| format!("{k}: {}", canonical(v, unordered)))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A result value without entity ids (new entities get ids in creation order, which
    /// follows row order).
    fn canonical_result(v: &crate::ResultValue, unordered: bool) -> String {
        use crate::ResultValue as R;
        let node = |n: &crate::NodeData| {
            format!("({}{{{}}})", n.labels.join(":"), canonical_props(&n.properties, unordered))
        };
        let rel = |r: &crate::RelData| {
            format!("[{}{{{}}}]", r.rel_type, canonical_props(&r.properties, unordered))
        };
        match v {
            R::Node(n) => node(n),
            R::Relationship(r) => rel(r),
            R::Path(p) => format!(
                "<{}|{}>",
                p.nodes.iter().map(node).collect::<String>(),
                p.relationships.iter().map(rel).collect::<String>()
            ),
            R::List(items) => {
                let mut parts: Vec<String> =
                    items.iter().map(|x| canonical_result(x, unordered)).collect();
                if unordered {
                    parts.sort();
                }
                format!("[{}]", parts.join(", "))
            }
            R::Map(m) => format!(
                "{{{}}}",
                m.iter()
                    .map(|(k, v)| format!("{k}: {}", canonical_result(v, unordered)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            other => format!("{other:?}"),
        }
    }

    /// The whole graph as sorted lines, without ids: nodes, then relationships with
    /// their end nodes.
    fn graph_dump(db: &Database, unordered: bool) -> Vec<String> {
        db.read(|s| {
            let node = |id| {
                let mut labels = s.node_labels(id).unwrap();
                labels.sort();
                let props = canonical_props(&s.node_properties(id).unwrap(), unordered);
                format!("({}{{{props}}})", labels.join(":"))
            };
            let mut out: Vec<String> = s.all_nodes().map(|id| node(id.unwrap())).collect();
            for r in s.all_relationships() {
                let r = r.unwrap();
                let (a, b) = s.relationship_endpoints(r).unwrap().unwrap();
                let props = canonical_props(&s.relationship_properties(r).unwrap(), unordered);
                out.push(format!(
                    "{}-[{}{{{props}}}]->{}",
                    node(a),
                    s.relationship_type(r).unwrap().unwrap(),
                    node(b)
                ));
            }
            out.sort();
            out
        })
    }

    fn graph_db() -> Database {
        let db = Database::in_memory();
        for stmt in graphbuff_cypher::split_statements(GRAPH).unwrap() {
            db.execute(stmt).unwrap();
        }
        db
    }

    #[test]
    fn writes_match_the_clause_by_clause_executor() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let (mut ok, mut errors, mut lazier, mut subset_only) = (0, 0, 0, 0);
        for i in 0..2500 {
            let q = random_write_query(&mut rng);
            let ast = graphbuff_cypher::parse(&q).unwrap_or_else(|e| panic!("query {i} `{q}`: {e}"));
            let (new_db, old_db) = (graph_db(), graph_db());
            let new = run_write(&new_db, &ast, false);
            if new.is_ok() {
                assert!(old_db.explain(&q).is_ok(), "EXPLAIN query {i} `{q}`");
            }
            let old = run_write(&old_db, &ast, true);
            // As for reads: LIMIT and SKIP pick rows by an order the two executors need
            // not share, and the planner evaluates fewer rows under a LIMIT.
            let subset = q.contains("LIMIT") || q.contains("SKIP");
            let unordered = q.contains("collect(");
            match (new, old) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a.columns, b.columns, "query {i} `{q}`");
                    if subset {
                        subset_only += 1;
                        continue;
                    }
                    let canon = |r: &crate::QueryResult| {
                        let mut rows: Vec<String> = r
                            .rows
                            .iter()
                            .map(|row| {
                                row.iter()
                                    .map(|v| canonical_result(v, unordered))
                                    .collect::<Vec<_>>()
                                    .join(" | ")
                            })
                            .collect();
                        rows.sort();
                        rows
                    };
                    assert_eq!(canon(&a), canon(&b), "rows of query {i} `{q}`");
                    assert_eq!(a.stats, b.stats, "statistics of query {i} `{q}`");
                    assert_eq!(
                        graph_dump(&new_db, unordered),
                        graph_dump(&old_db, unordered),
                        "graph after query {i} `{q}`"
                    );
                    ok += 1;
                }
                (Err(_), Err(_)) => {
                    // Both rolled back.
                    assert_eq!(
                        graph_dump(&new_db, false),
                        graph_dump(&old_db, false),
                        "graph after failed query {i} `{q}`"
                    );
                    errors += 1;
                }
                _ if subset => lazier += 1,
                (a, b) => panic!(
                    "query {i} `{q}`: planner {:?}, legacy {:?}",
                    a.map(|r| r.rows.len()),
                    b.map(|r| r.rows.len())
                ),
            }
        }
        eprintln!(
            "differential writes: {ok} equal, {subset_only} under SKIP/LIMIT, {errors} errors, {lazier} lazier"
        );
        assert!(ok > 1500, "only {ok} comparable successful queries ({errors} errors)");
    }

    #[test]
    fn explain_shows_the_operator_tree() {
        let db = Database::in_memory();
        let plan = shape(&db, "MATCH (p:Person)-[:KNOWS]->(f) WITH p, count(f) AS n WHERE n > 1 RETURN DISTINCT p.name ORDER BY p.name SKIP 1 LIMIT 3")
            .unwrap();
        let expected = "\
Select
  Limit 3
    Skip 1
      Top 1 + 3 by p.name
        Distinct
          Project p.name
            Filter
              Select
                Aggregate p, n
                  Expand(All) (p)-[:KNOWS]->(f)
                    NodeByLabelScan (p:Person)
                      Start
";
        assert_eq!(plan, expected);
        let union = shape(&db, "RETURN 1 AS x UNION ALL RETURN 2 AS x").unwrap();
        assert_eq!(
            union,
            "UnionAll\n  Select\n    Project x\n      Start\n  Select\n    Project x\n      Start\n"
        );
        assert!(shape(&db, "RETURN 1 AS x MATCH (n)").is_err());
    }

    #[test]
    fn independent_parts_become_joins() {
        let db = Database::in_memory();
        let lines = |q: &str| -> Vec<String> {
            shape(&db, q).unwrap().lines().map(|l| l.trim().to_string()).collect()
        };
        assert_eq!(
            lines("MATCH (a:A), (b:B) WHERE a.k = b.k + 1 AND b.x > 0 RETURN a"),
            vec![
                "Select",
                "Project a",
                "HashJoin a.k = …",
                "Filter", // b.x > 0, while the build side is made
                "NodeByLabelScan (b:B)",
                "NodeByLabelScan (a:A)",
                "Start",
            ]
        );
        assert_eq!(
            lines("MATCH (a:A), (b:B) RETURN a"),
            vec!["Select", "Project a", "CartesianProduct", "NodeByLabelScan (b:B)", "NodeByLabelScan (a:A)", "Start"]
        );
        // Connected parts stay a nested loop.
        assert!(!lines("MATCH (a:A), (a)-->(b) RETURN a").iter().any(|l| l.contains("Join") || l.contains("Cartesian")));
        // A part with an index seek on the other side's value stays an index nested loop.
        db.execute("CREATE INDEX FOR (b:B) ON (b.k)").unwrap();
        let seek = lines("MATCH (a:A), (b:B) WHERE b.k = a.k RETURN a");
        assert!(seek.iter().any(|l| l.starts_with("NodeIndexSeek (b:B)")), "{seek:?}");
        assert!(!seek.iter().any(|l| l.contains("HashJoin")), "{seek:?}");
    }

    #[test]
    fn comparisons_and_prefixes_use_index_range_seeks() {
        let db = Database::in_memory();
        db.execute("CREATE INDEX FOR (p:P) ON (p.age)").unwrap();
        db.execute("CREATE INDEX FOR (p:P) ON (p.name)").unwrap();
        let start = |q: &str| -> String {
            let plan = shape(&db, q).unwrap();
            plan.lines().rev().nth(1).expect("a start step").trim().to_string()
        };
        assert_eq!(
            start("MATCH (p:P) WHERE p.age > 3 AND 9 >= p.age RETURN p"),
            "NodeIndexSeek (p:P) on :P(age) > | :P(age) <="
        );
        assert_eq!(
            start("MATCH (p:P) WHERE p.name STARTS WITH 'A' RETURN p"),
            "NodeIndexSeek (p:P) on :P(name) STARTS WITH"
        );
        // No index on the property: a label scan.
        assert_eq!(start("MATCH (p:P) WHERE p.x > 3 RETURN p"), "NodeByLabelScan (p:P)");

        db.execute("UNWIND range(1, 30) AS i CREATE (:P {age: i, name: 'n' + toString(i)})")
            .unwrap();
        let count = |q: &str| db.execute(q).unwrap().rows[0][0].to_string();
        assert_eq!(count("MATCH (p:P) WHERE p.age > 3 AND 9 >= p.age RETURN count(p)"), "6");
        assert_eq!(count("MATCH (p:P) WHERE p.age >= 29.5 RETURN count(p)"), "1");
        assert_eq!(count("MATCH (p:P) WHERE p.name STARTS WITH 'n2' RETURN count(p)"), "11");
        assert_eq!(count("MATCH (p:P) WHERE p.name > 'n3' RETURN count(p)"), "7");
    }

    #[test]
    fn parts_and_directions_follow_the_estimates() {
        let db = Database::in_memory();
        db.execute("CREATE (h:Hub) WITH h UNWIND range(1, 50) AS i CREATE (h)-[:R]->(:Leaf {i: i})")
            .unwrap();
        db.execute("UNWIND range(1, 200) AS i CREATE (:Other)").unwrap();
        let lines = |q: &str| -> Vec<String> {
            shape(&db, q).unwrap().lines().map(|l| l.trim().to_string()).collect()
        };
        // The small part first, though it is written second; the Leaf part then only
        // checks the bound node.
        let plan = lines("MATCH (x:Leaf), (h:Hub)-[:R]->(x) RETURN x");
        assert_eq!(
            plan[plan.len() - 4..],
            ["Argument (x:Leaf)", "Expand(All) (h)-[:R]->(x)", "NodeByLabelScan (h:Hub)", "Start"],
            "{plan:?}"
        );
        // From the single Hub rather than the 50 leaves, whichever way it is written.
        let plan = lines("MATCH (x:Leaf)<-[:R]-(h:Hub) RETURN x");
        assert_eq!(plan[plan.len() - 2], "NodeByLabelScan (h:Hub)", "{plan:?}");
        // An unlabelled end (250 nodes) loses to a labelled one (50).
        let plan = lines("MATCH (n)-[:R]->(x:Leaf) RETURN x");
        assert_eq!(plan[plan.len() - 2], "NodeByLabelScan (x:Leaf)", "{plan:?}");
        // A bound end still starts the part.
        let plan = lines("MATCH (x:Leaf {i: 1}) MATCH (h)-[:R]->(x) RETURN h");
        assert!(plan.contains(&"Argument (x)".to_string()), "{plan:?}");
    }

    #[test]
    fn explain_estimates_rows_from_statistics() {
        let db = Database::in_memory();
        db.execute("UNWIND range(0, 99) AS i CREATE (:A {k: i % 10})").unwrap();
        db.execute("UNWIND range(0, 9) AS i CREATE (:B {i: i})").unwrap();
        db.execute("MATCH (a:A), (b:B) WHERE a.k * 2 = b.i * 2 AND a.k < 5 CREATE (a)-[:R]->(b)")
            .unwrap();
        db.execute("CREATE INDEX FOR (a:A) ON (a.k)").unwrap();
        // 110 nodes, 100 :A (10 distinct k), 10 :B, 50 :R relationships.
        let rows = |q: &str, step: &str| -> String {
            let plan = db.explain(q).unwrap();
            let line = plan
                .lines()
                .find(|l| l.trim_start().starts_with(step))
                .unwrap_or_else(|| panic!("no {step} in\n{plan}"));
            let est = &line[line.find("(~").expect("an estimate") + 2..];
            est[..est.find(' ').unwrap()].to_string()
        };
        assert_eq!(rows("MATCH (a:A) RETURN a", "NodeByLabelScan"), "100");
        assert_eq!(rows("MATCH (n) RETURN n", "AllNodesScan"), "110");
        // = seek: entries / distinct values.
        assert_eq!(rows("MATCH (a:A {k: 3}) RETURN a", "NodeIndexSeek"), "10");
        // Range seek: 30% of the entries; the WHERE filter then keeps half.
        assert_eq!(rows("MATCH (a:A) WHERE a.k > 5 RETURN a", "NodeIndexSeek"), "30");
        assert_eq!(rows("MATCH (a:A) WHERE a.k > 5 RETURN a", "Filter"), "15");
        // From the 10 :B nodes, 50/110 :R per node, 100/110 of targets labelled :A.
        assert_eq!(rows("MATCH (b:B)<-[:R]-(a:A) RETURN a", "NodeByLabelScan"), "10");
        assert_eq!(rows("MATCH (b:B)<-[:R]-(a:A) RETURN a", "Expand"), "4.1");
    }

    #[test]
    fn where_conjuncts_run_right_after_their_variables_are_bound() {
        let db = Database::in_memory();
        let plan = shape(&db, 
                "UNWIND [1] AS x MATCH (p:Person)-[r]->(q)-->(z) \
                 WHERE x > 0 AND z.age > 1 AND p.age > 2 AND type(r) = 'K' AND (q)-->(p) RETURN z",
            )
            .unwrap();
        // `x > 0` before the scan; `p.age > 2` after it; `type(r)` and the pattern
        // predicate after the first expansion; `z.age > 1` at the end.
        let order: Vec<&str> = plan.lines().map(str::trim).collect();
        assert_eq!(
            order,
            vec![
                "Select",
                "Project z",
                "Filter",
                "Expand(All) (q)-->(z)",
                "Filter",
                "Filter",
                "Expand(All) (p)-[r]->(q)",
                "Filter",
                "NodeByLabelScan (p:Person)",
                "Filter",
                "Unwind AS x",
                "Start",
            ],
            "{plan}"
        );
    }
}
