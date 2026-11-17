//! Runs query plans ([`crate::plan`]).
//!
//! Read-only stretches of a plan run as pull-based streams ([`crate::stream`]). Updates
//! are eager: an update first materializes everything below it, applies itself to every
//! row, and hands its rows to the stream above. Every clause therefore sees the effects
//! of the clauses before it, and a `LIMIT` above an update never skips part of it.

use std::collections::{BTreeMap, HashMap, HashSet};
use graphbuff_core::clock::Instant;

use graphbuff_core::{
    ConstraintDef, Direction, Error, IndexDef, IndexEntity, NodeId, Path, RelId, Result, Value,
};
use graphbuff_cypher::ast::{
    Clause, Expr, IndexTarget, NodePattern, PatternPart, RelDirection, RemoveItem,
    SetItem, SortItem,
};

use crate::aggregate::{Accumulator, AggSpec, extract_aggregates};
use crate::context::{Env, Executor, Frame, Row, Scope};
use crate::matcher::MatchState;
use crate::ops::{cypher_eq, order_cmp};
use crate::plan::{Item, Operator, PartPlan, QueryPlan};

/// Raw result of a query before entities are materialized for the caller.
pub(crate) struct RawResult {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
}

impl Executor<'_> {
    #[cfg(test)]
    pub fn run(&mut self, q: &graphbuff_cypher::ast::Query) -> Result<RawResult> {
        let plan = QueryPlan::build(q, self)?;
        self.run_plan(&plan)
    }

    pub fn run_plan(&mut self, plan: &QueryPlan) -> Result<RawResult> {
        let mut parts = plan.parts.iter();
        let mut result = self.run_part(parts.next().expect("parser guarantees one part"))?;
        if plan.parts.len() == 1 {
            return Ok(result);
        }
        for part in parts {
            let next = self.run_part(part)?;
            if next.columns != result.columns {
                return Err(Error::Semantic(
                    "all sub queries in a UNION must have the same return column names".into(),
                ));
            }
            result.rows.extend(next.rows);
        }
        if !plan.union_all {
            let mut seen = HashSet::new();
            result.rows.retain(|r| seen.insert(r.clone()));
        }
        Ok(result)
    }

    fn run_part(&mut self, part: &PartPlan) -> Result<RawResult> {
        match part {
            PartPlan::Schema(clause) => self.exec_schema(clause),
            PartPlan::Pipeline { root, returns } => {
                let frame = self.run_op(root, None)?;
                Ok(if *returns {
                    RawResult {
                        columns: frame.scope.names().to_vec(),
                        rows: frame.rows,
                    }
                } else {
                    RawResult {
                        columns: Vec::new(),
                        rows: Vec::new(),
                    }
                })
            }
        }
    }

    /// Runs `op` and its inputs, producing at most `cap` rows (`None` = all of them).
    fn run_op(&mut self, op: &Operator, cap: Option<usize>) -> Result<Frame> {
        use Operator as O;
        let started = Instant::now();
        let frame = match op {
            O::Create { input, patterns } => {
                let frame = self.run_op(input, None)?;
                self.exec_create(patterns, frame)?
            }
            O::Merge {
                input,
                pattern,
                on_create,
                on_match,
            } => {
                let frame = self.run_op(input, None)?;
                self.exec_merge(pattern, on_create, on_match, frame)?
            }
            O::Set { input, items } => {
                let frame = self.run_op(input, None)?;
                for row in &frame.rows {
                    for item in items {
                        self.apply_set(item, &frame.scope, row)?;
                    }
                }
                frame
            }
            O::Remove { input, items } => {
                let frame = self.run_op(input, None)?;
                for row in &frame.rows {
                    for item in items {
                        self.apply_remove(item, &frame.scope, row)?;
                    }
                }
                frame
            }
            O::Delete {
                input,
                detach,
                exprs,
            } => {
                let frame = self.run_op(input, None)?;
                self.exec_delete(*detach, exprs, &frame)?;
                frame
            }
            O::Call {
                input,
                plan,
                imports,
                columns,
                returns,
                updating: true,
            } => {
                let frame = self.run_op(input, None)?;
                self.exec_call(frame, plan, imports, columns, *returns)?
            }
            O::Foreach {
                input,
                list,
                body,
                imports,
                ..
            } => {
                let frame = self.run_op(input, None)?;
                self.exec_foreach(&frame, list, body, imports)?;
                frame
            }
            _ => {
                // A read-only chain: materialize the update it starts on (if any), then
                // stream the rest.
                let base = match op.chain_base() {
                    Some(update) => self.run_op(update, None)?,
                    None => match &self.argument {
                        Some((scope, row)) => Frame::new(scope.clone(), vec![row.clone()]),
                        None => Frame::new(Scope::default(), vec![Vec::new()]),
                    },
                };
                let ex: &Executor = self;
                let mut stream = crate::stream::build(ex, op, base)?;
                let cap = cap.unwrap_or(usize::MAX);
                let mut rows = Vec::new();
                while rows.len() < cap {
                    match stream.next(ex, (cap - rows.len()).min(crate::stream::BATCH))? {
                        Some(batch) => rows.extend(batch),
                        None => break,
                    }
                }
                return Ok(Frame {
                    scope: stream.scope().clone(),
                    rows,
                    hidden: stream.hidden().cloned(),
                });
            }
        };
        // Only updates get here: they run eagerly, outside of streams.
        self.record(crate::plan::profile_key(op), frame.rows.len(), started.elapsed());
        Ok(frame)
    }

    /// Runs a `CALL { … }` subquery that changes the graph, once per row of `frame`, in
    /// order: each run sees the changes of the runs before it.
    fn exec_call(
        &mut self,
        frame: Frame,
        plan: &QueryPlan,
        imports: &[String],
        columns: &[String],
        returns: bool,
    ) -> Result<Frame> {
        let slots: Vec<usize> = imports
            .iter()
            .map(|v| {
                frame
                    .scope
                    .get(v)
                    .ok_or_else(|| Error::Semantic(format!("variable `{v}` not defined")))
            })
            .collect::<Result<_>>()?;
        let arg_scope = Scope::from_names(imports.iter().cloned())?;
        let mut scope = frame.scope.clone();
        let width = scope.len();
        for c in columns {
            scope.add(c);
        }
        let mut rows = Vec::new();
        for mut row in frame.rows {
            row.truncate(width);
            let arg: Row = slots.iter().map(|&i| row[i].clone()).collect();
            let prev = self.argument.replace((arg_scope.clone(), arg));
            let result = self.run_plan(plan);
            self.argument = prev;
            let result = result?;
            if !returns {
                rows.push(row);
                continue;
            }
            let pos: Vec<usize> = columns
                .iter()
                .map(|c| result.columns.iter().position(|x| x == c).expect("planned columns are returned"))
                .collect();
            for r in result.rows {
                let mut out = row.clone();
                out.extend(pos.iter().map(|&i| r[i].clone()));
                rows.push(out);
            }
        }
        Ok(Frame::new(scope, rows))
    }

    /// Runs a `FOREACH` body once per element of `list`, per row of `frame`.
    fn exec_foreach(&mut self, frame: &Frame, list: &Expr, body: &QueryPlan, imports: &[String]) -> Result<()> {
        let (outer, var) = imports.split_at(imports.len() - 1);
        let slots: Vec<usize> = outer
            .iter()
            .map(|v| {
                frame
                    .scope
                    .get(v)
                    .ok_or_else(|| Error::Semantic(format!("variable `{v}` not defined")))
            })
            .collect::<Result<_>>()?;
        let arg_scope = Scope::from_names(imports.iter().cloned())?;
        for row in &frame.rows {
            let items = match self.eval(list, &Env::new(&frame.scope, row))? {
                Value::Null => Vec::new(),
                Value::List(items) => items,
                other => {
                    return Err(Error::Type(format!(
                        "FOREACH needs a list for `{}`, got {}",
                        var[0],
                        other.type_name()
                    )));
                }
            };
            for item in items {
                let mut arg: Row = slots.iter().map(|&i| row[i].clone()).collect();
                arg.push(item);
                let prev = self.argument.replace((arg_scope.clone(), arg));
                let result = self.run_plan(body);
                self.argument = prev;
                result?;
            }
        }
        Ok(())
    }

    /// Runs a read-only subquery `plan` from the start row `row` (scope `scope`), and
    /// returns its columns and at most about `cap` of its rows.
    pub(crate) fn stream_subquery(&self, plan: &QueryPlan, scope: &Scope, row: &Row, cap: usize) -> Result<(Vec<String>, Vec<Row>)> {
        let mut columns: Option<Vec<String>> = None;
        let mut rows: Vec<Row> = Vec::new();
        for part in &plan.parts {
            let PartPlan::Pipeline { root, returns: true } = part else {
                continue;
            };
            let mut stream = crate::stream::build(self, root, Frame::new(scope.clone(), vec![row.clone()]))?;
            let names = stream.scope().names().to_vec();
            // Later parts are reordered to the first part's columns.
            let order: Vec<usize> = match &columns {
                None => (0..names.len()).collect(),
                Some(first) => first
                    .iter()
                    .map(|c| names.iter().position(|n| n == c).expect("UNION parts return the same columns"))
                    .collect(),
            };
            if columns.is_none() {
                columns = Some(names.clone());
            }
            let mut taken = 0;
            while taken < cap {
                match stream.next(self, (cap - taken).min(crate::stream::BATCH))? {
                    Some(batch) => {
                        taken += batch.len();
                        rows.extend(batch.into_iter().map(|r| order.iter().map(|&i| r[i].clone()).collect()));
                    }
                    None => break,
                }
            }
        }
        if plan.parts.len() > 1 && !plan.union_all {
            let mut seen = HashSet::new();
            rows.retain(|r| seen.insert(r.clone()));
        }
        Ok((columns.unwrap_or_default(), rows))
    }

    // ---- schema -----------------------------------------------------------------------

    fn exec_schema(&mut self, clause: &Clause) -> Result<RawResult> {
        let empty = RawResult {
            columns: Vec::new(),
            rows: Vec::new(),
        };
        match clause {
            Clause::CreateIndex {
                name,
                entity,
                label,
                properties,
                if_not_exists,
            } => {
                let def = IndexDef {
                    name: name
                        .clone()
                        .unwrap_or_else(|| format!("index_{label}_{}", properties.join("_"))),
                    entity: *entity,
                    label: label.clone(),
                    properties: properties.clone(),
                };
                let existing = self.store().indexes();
                let exists = existing
                    .iter()
                    .any(|d| d.name == def.name || d.covers(def.entity, &def.label, &def.properties));
                if exists && *if_not_exists {
                    return Ok(empty);
                }
                def.check_new(&existing)?;
                self.store_mut()?.create_index(def)?;
                self.stats.indexes_added += 1;
                Ok(empty)
            }
            Clause::DropIndex { target, if_exists } => {
                let name = match target {
                    IndexTarget::Name(n) => Some(n.clone()),
                    IndexTarget::On { label, properties } => self
                        .store()
                        .indexes()
                        .into_iter()
                        .find(|d| d.covers(IndexEntity::Node, label, properties))
                        .map(|d| d.name),
                };
                if let Some(n) = &name {
                    self.check_index_unused(n)?;
                }
                let dropped = match name {
                    Some(n) => self.store_mut()?.drop_index(&n)?,
                    None => false,
                };
                if !dropped && !*if_exists {
                    return Err(Error::Semantic(match target {
                        IndexTarget::Name(n) => format!("no index named `{n}`"),
                        IndexTarget::On { label, properties } => {
                            format!("no index on :{label}({})", properties.join(", "))
                        }
                    }));
                }
                if dropped {
                    self.stats.indexes_removed += 1;
                }
                Ok(empty)
            }
            Clause::ShowIndexes => Ok(RawResult {
                columns: ["name", "entityType", "labelOrType", "properties"]
                    .map(String::from)
                    .to_vec(),
                rows: self
                    .store()
                    .indexes()
                    .into_iter()
                    .map(|d| {
                        vec![
                            Value::String(d.name),
                            Value::String(d.entity.text().into()),
                            Value::String(d.label),
                            Value::List(d.properties.into_iter().map(Value::String).collect()),
                        ]
                    })
                    .collect(),
            }),
            Clause::CreateConstraint {
                name,
                kind,
                label,
                properties,
                if_not_exists,
            } => {
                let def = ConstraintDef {
                    name: name
                        .clone()
                        .unwrap_or_else(|| format!("constraint_{label}_{}", properties.join("_"))),
                    kind: *kind,
                    label: label.clone(),
                    properties: properties.clone(),
                };
                let existing = self.store().constraints();
                let clash = existing.iter().any(|c| {
                    c.name == def.name
                        || (c.kind == def.kind && c.label == def.label && c.properties == def.properties)
                });
                if clash && *if_not_exists {
                    return Ok(empty);
                }
                def.check_new(&existing)?;
                self.create_constraint(def)?;
                Ok(empty)
            }
            Clause::DropConstraint { name, if_exists } => {
                let Some(def) = self.store().constraints().into_iter().find(|c| c.name == *name) else {
                    if *if_exists {
                        return Ok(empty);
                    }
                    return Err(Error::Semantic(format!("no constraint named `{name}`")));
                };
                self.store_mut()?.drop_constraint(name)?;
                self.stats.constraints_removed += 1;
                // The index it created, if it is still the one it needs.
                if def.requires_unique() {
                    let owned = self.store().indexes().into_iter().any(|ix| {
                        ix.name == def.name && ix.covers(IndexEntity::Node, &def.label, &def.properties[..1])
                    });
                    if owned && self.check_index_unused(&def.name).is_ok() {
                        self.store_mut()?.drop_index(&def.name)?;
                        self.stats.indexes_removed += 1;
                    }
                }
                Ok(empty)
            }
            Clause::ShowProcedures => Ok(RawResult {
                columns: ["name", "signature", "description"].map(String::from).to_vec(),
                rows: self.procedures.listing(),
            }),
            Clause::ShowConstraints => Ok(RawResult {
                columns: ["name", "type", "label", "properties"].map(String::from).to_vec(),
                rows: self
                    .store()
                    .constraints()
                    .into_iter()
                    .map(|c| {
                        vec![
                            Value::String(c.name),
                            Value::String(c.kind.text().into()),
                            Value::String(c.label),
                            Value::List(c.properties.into_iter().map(Value::String).collect()),
                        ]
                    })
                    .collect(),
            }),
            _ => unreachable!("not a schema clause"),
        }
    }

    // ---- constraints ------------------------------------------------------------------

    /// Creates `def` (already checked against the existing constraints): first the index a
    /// uniqueness check seeks on, if there is none, then the constraint, then checks every
    /// node that has its label. On error, the query rolls all of it back.
    fn create_constraint(&mut self, def: ConstraintDef) -> Result<()> {
        if def.requires_unique() && !self.store().has_index(&def.label, &def.properties[0]) {
            self.store_mut()?.create_index(IndexDef::node(
                def.name.clone(),
                def.label.clone(),
                def.properties[0].clone(),
            ))?;
            self.stats.indexes_added += 1;
        }
        self.store_mut()?.create_constraint(def.clone())?;
        self.stats.constraints_added += 1;
        let nodes: Vec<NodeId> = self
            .store()
            .nodes_with_label(&def.label)
            .collect::<Result<_>>()?;
        for id in nodes {
            self.check_node(&def, id).map_err(|e| match e {
                Error::Constraint(msg) => Error::Constraint(format!(
                    "cannot create constraint `{}`: existing data violates it: {msg}",
                    def.name
                )),
                other => other,
            })?;
        }
        Ok(())
    }

    /// Fails if index `name` is the one a uniqueness constraint seeks on.
    fn check_index_unused(&self, name: &str) -> Result<()> {
        let Some(ix) = self.store().indexes().into_iter().find(|ix| ix.name == name) else {
            return Ok(());
        };
        match self.store().constraints().into_iter().find(|c| {
            c.requires_unique() && ix.covers(IndexEntity::Node, &c.label, &c.properties[..1])
        }) {
            Some(c) => Err(Error::Constraint(format!(
                "index `{name}` is used by constraint `{}`; drop the constraint first",
                c.name
            ))),
            None => Ok(()),
        }
    }

    /// Checks every node this query created or changed against the constraints of its
    /// labels. Runs once, after the last clause and before commit, so a query may pass
    /// through violating states (swap two unique values, create a node and then set its
    /// key) as long as it ends in a valid one.
    pub(crate) fn check_constraints(&mut self) -> Result<()> {
        let Some(touched) = self.touched.take() else {
            return Ok(());
        };
        let constraints = self.store().constraints();
        for id in touched {
            if !self.store().node_exists(id)? {
                continue;
            }
            let labels = self.store().node_labels(id)?;
            for c in constraints.iter().filter(|c| labels.contains(&c.label)) {
                self.check_node(c, id)?;
            }
        }
        Ok(())
    }

    /// Checks node `id`, which has `c`'s label, against `c`.
    fn check_node(&self, c: &ConstraintDef, id: NodeId) -> Result<()> {
        let store = self.store();
        let mut values = Vec::with_capacity(c.properties.len());
        for p in &c.properties {
            values.push(store.node_property(id, p)?);
        }
        if c.requires_properties()
            && let Some(i) = values.iter().position(Option::is_none)
        {
            return Err(Error::Constraint(format!(
                "node {id} with label `{}` must have the property `{}` (constraint `{}`)",
                c.label, c.properties[i], c.name
            )));
        }
        if !c.requires_unique() || values.iter().any(Option::is_none) {
            return Ok(());
        }
        let values: Vec<Value> = values.into_iter().flatten().collect();
        let candidates = match store.index_seek(&c.label, &c.properties[0], &values[0])? {
            Some(ids) => ids,
            None => store.nodes_with_label(&c.label).collect::<Result<_>>()?,
        };
        for other in candidates {
            if other == id {
                continue;
            }
            let mut same = true;
            for (p, v) in c.properties.iter().zip(&values) {
                let w = store.node_property(other, p)?;
                if !w.is_some_and(|w| cypher_eq(v, &w) == Some(true)) {
                    same = false;
                    break;
                }
            }
            if same {
                let shown: Vec<String> = c
                    .properties
                    .iter()
                    .zip(&values)
                    .map(|(p, v)| format!("{p} = {v}"))
                    .collect();
                return Err(Error::Constraint(format!(
                    "nodes {other} and {id} with label `{}` both have {} (constraint `{}`)",
                    c.label,
                    shown.join(", "),
                    c.name
                )));
            }
        }
        Ok(())
    }

    // ---- CREATE / MERGE ---------------------------------------------------------------

    pub(crate) fn exec_create(&mut self, patterns: &[PatternPart], frame: Frame) -> Result<Frame> {
        let (scope, old_len) = extend_scope(frame.scope, patterns.iter());
        let mut rows = Vec::with_capacity(frame.rows.len());
        for mut row in frame.rows {
            self.check_interrupt()?;
            row.resize(scope.len(), Value::Null);
            let mut bound = initial_bound(old_len, scope.len());
            for p in patterns {
                self.create_part(&scope, p, &mut row, &mut bound)?;
            }
            rows.push(row);
        }
        Ok(Frame::new(scope, rows))
    }

    pub(crate) fn exec_merge(
        &mut self,
        pattern: &PatternPart,
        on_create: &[SetItem],
        on_match: &[SetItem],
        frame: Frame,
    ) -> Result<Frame> {
        let (scope, old_len) = extend_scope(frame.scope, std::iter::once(pattern));
        let mut out = Vec::new();
        for mut row in frame.rows {
            self.check_interrupt()?;
            row.resize(scope.len(), Value::Null);
            let bound = initial_bound(old_len, scope.len());
            self.check_merge_props(&scope, pattern, &row)?;
            let mut matches: Vec<Row> = Vec::new();
            let mut st = MatchState::new(row.clone(), bound.clone());
            self.match_parts(
                &scope,
                std::slice::from_ref(pattern),
                0,
                &mut st,
                &mut |st| {
                    matches.push(st.row.clone());
                    Ok(true)
                },
            )?;
            if matches.is_empty() {
                let mut bound = bound;
                // An undirected relationship is created pointing left to right.
                let mut created = pattern.clone();
                for (r, _) in &mut created.chain {
                    if r.direction == RelDirection::Both {
                        r.direction = RelDirection::Right;
                    }
                }
                self.create_part(&scope, &created, &mut row, &mut bound)?;
                for item in on_create {
                    self.apply_set(item, &scope, &row)?;
                }
                out.push(row);
            } else {
                for m in matches {
                    for item in on_match {
                        self.apply_set(item, &scope, &m)?;
                    }
                    out.push(m);
                }
            }
        }
        Ok(Frame::new(scope, out))
    }

    /// MERGE cannot match or create a property whose value is null.
    fn check_merge_props(&self, scope: &Scope, pattern: &PatternPart, row: &Row) -> Result<()> {
        let nodes = std::iter::once(&pattern.start).chain(pattern.chain.iter().map(|(_, n)| n));
        let props = nodes
            .filter_map(|n| n.props.as_ref())
            .chain(pattern.chain.iter().filter_map(|(r, _)| r.props.as_ref()));
        for e in props {
            if let Some((k, _)) = self.eval_map(e, &Env::new(scope, row))?.iter().find(|(_, v)| v.is_null()) {
                return Err(Error::Semantic(format!("MERGE cannot use a null value for property `{k}`")));
            }
        }
        Ok(())
    }

    fn create_part(
        &mut self,
        scope: &Scope,
        p: &PatternPart,
        row: &mut Row,
        bound: &mut [bool],
    ) -> Result<()> {
        let start = self.create_or_get_node(scope, &p.start, row, bound)?;
        let mut path = Path {
            nodes: vec![start],
            rels: Vec::new(),
        };
        let mut prev = start;
        for (rp, np) in &p.chain {
            let next = self.create_or_get_node(scope, np, row, bound)?;
            if rp.length.is_some() {
                return Err(Error::Semantic(
                    "variable length relationships cannot be created".into(),
                ));
            }
            if rp.types.len() != 1 {
                return Err(Error::Semantic(
                    "exactly one relationship type must be specified to create a relationship"
                        .into(),
                ));
            }
            let (s, e) = match rp.direction {
                RelDirection::Right => (prev, next),
                RelDirection::Left => (next, prev),
                RelDirection::Both => {
                    return Err(Error::Semantic(
                        "only directed relationships can be created".into(),
                    ));
                }
            };
            let slot = rp.var.as_deref().and_then(|v| scope.get(v));
            if slot.is_some_and(|i| bound[i]) {
                return Err(Error::Semantic(format!(
                    "variable `{}` is already declared and cannot be used to create a new relationship",
                    rp.var.as_deref().unwrap_or_default()
                )));
            }
            let props = match &rp.props {
                Some(e) => self.eval_map(e, &Env::new(scope, row))?,
                None => BTreeMap::new(),
            };
            let n_props = props.values().filter(|v| !v.is_null()).count();
            let id = self
                .store_mut()?
                .create_relationship(s, e, &rp.types[0], props)?;
            self.stats.relationships_created += 1;
            self.stats.properties_set += n_props;
            if let Some(i) = slot {
                row[i] = Value::Rel(id);
                bound[i] = true;
            }
            path.rels.push(id);
            path.nodes.push(next);
            prev = next;
        }
        if let Some(i) = p.var.as_deref().and_then(|v| scope.get(v)) {
            row[i] = Value::Path(path);
            bound[i] = true;
        }
        Ok(())
    }

    fn create_or_get_node(
        &mut self,
        scope: &Scope,
        np: &NodePattern,
        row: &mut Row,
        bound: &mut [bool],
    ) -> Result<NodeId> {
        let slot = np.var.as_deref().and_then(|v| scope.get(v));
        if let Some(i) = slot.filter(|i| bound[*i]) {
            let name = np.var.as_deref().unwrap_or_default();
            return match &row[i] {
                Value::Node(id) => {
                    if !np.labels.is_empty() || np.props.is_some() {
                        return Err(Error::Semantic(format!(
                            "cannot create node `{name}` with labels or properties: the variable is already declared"
                        )));
                    }
                    Ok(*id)
                }
                Value::Null => Err(Error::Runtime(format!(
                    "cannot create a relationship to null node `{name}`"
                ))),
                other => Err(Error::Type(format!(
                    "variable `{name}` is a {}, expected a node",
                    other.type_name()
                ))),
            };
        }
        let props = match &np.props {
            Some(e) => self.eval_map(e, &Env::new(scope, row))?,
            None => BTreeMap::new(),
        };
        let n_props = props.values().filter(|v| !v.is_null()).count();
        let n_labels = np.labels.iter().collect::<HashSet<_>>().len();
        let id = self.store_mut()?.create_node(&np.labels, props)?;
        self.touch(id);
        self.stats.nodes_created += 1;
        self.stats.properties_set += n_props;
        self.stats.labels_added += n_labels;
        if let Some(i) = slot {
            row[i] = Value::Node(id);
            bound[i] = true;
        }
        Ok(id)
    }

    // ---- SET / REMOVE / DELETE --------------------------------------------------------

    fn lookup_var(&self, scope: &Scope, row: &[Value], var: &str) -> Result<Value> {
        Env::new(scope, row)
            .lookup(var)
            .cloned()
            .ok_or_else(|| Error::Semantic(format!("variable `{var}` not defined")))
    }

    fn set_property(&mut self, target: &Value, key: &str, value: Value) -> Result<()> {
        match target {
            Value::Null => return Ok(()),
            Value::Node(id) => {
                self.store_mut()?.set_node_property(*id, key, value)?;
                self.touch(*id);
            }
            Value::Rel(id) => self
                .store_mut()?
                .set_relationship_property(*id, key, value)?,
            other => {
                return Err(Error::Type(format!(
                    "cannot set properties on a {}",
                    other.type_name()
                )));
            }
        }
        self.stats.properties_set += 1;
        Ok(())
    }

    fn property_map(&self, v: Value) -> Result<BTreeMap<String, Value>> {
        match v {
            Value::Map(m) => Ok(m),
            Value::Null => Ok(BTreeMap::new()),
            Value::Node(id) => self.store().node_properties(id),
            Value::Rel(id) => self.store().relationship_properties(id),
            other => Err(Error::Type(format!(
                "expected a map of properties, got {}",
                other.type_name()
            ))),
        }
    }

    pub(crate) fn apply_set(&mut self, item: &SetItem, scope: &Scope, row: &[Value]) -> Result<()> {
        self.check_interrupt()?;
        match item {
            SetItem::Property { var, key, value } => {
                let target = self.lookup_var(scope, row, var)?;
                let v = self.eval(value, &Env::new(scope, row))?;
                self.set_property(&target, key, v)
            }
            SetItem::Replace { var, value } | SetItem::Merge { var, value } => {
                let target = self.lookup_var(scope, row, var)?;
                if target.is_null() {
                    return Ok(());
                }
                let new_props = self.property_map(self.eval(value, &Env::new(scope, row))?)?;
                if matches!(item, SetItem::Replace { .. }) {
                    let existing = self.property_map(target.clone())?;
                    for k in existing.keys().filter(|k| !new_props.contains_key(*k)) {
                        self.set_property(&target, k, Value::Null)?;
                    }
                }
                for (k, v) in new_props {
                    self.set_property(&target, &k, v)?;
                }
                Ok(())
            }
            SetItem::Labels { var, labels } => match self.lookup_var(scope, row, var)? {
                Value::Null => Ok(()),
                Value::Node(id) => {
                    for l in labels {
                        if self.store_mut()?.add_label(id, l)? {
                            self.stats.labels_added += 1;
                            self.touch(id);
                        }
                    }
                    Ok(())
                }
                other => Err(Error::Type(format!(
                    "cannot set labels on a {}",
                    other.type_name()
                ))),
            },
        }
    }

    pub(crate) fn apply_remove(&mut self, item: &RemoveItem, scope: &Scope, row: &[Value]) -> Result<()> {
        match item {
            RemoveItem::Property { var, key } => {
                let target = self.lookup_var(scope, row, var)?;
                let had = !self.property_of(&target, key)?.is_null();
                if had {
                    self.set_property(&target, key, Value::Null)?;
                }
                Ok(())
            }
            RemoveItem::Labels { var, labels } => match self.lookup_var(scope, row, var)? {
                Value::Null => Ok(()),
                Value::Node(id) => {
                    for l in labels {
                        if self.store_mut()?.remove_label(id, l)? {
                            self.stats.labels_removed += 1;
                        }
                    }
                    Ok(())
                }
                other => Err(Error::Type(format!(
                    "cannot remove labels from a {}",
                    other.type_name()
                ))),
            },
        }
    }

    pub(crate) fn exec_delete(&mut self, detach: bool, exprs: &[Expr], frame: &Frame) -> Result<()> {
        // Collect first, then delete relationships before nodes, so that
        // `MATCH (n)-[r]-() DELETE n, r` works regardless of item order.
        let mut nodes: Vec<NodeId> = Vec::new();
        let mut rels: Vec<RelId> = Vec::new();
        for row in &frame.rows {
            self.check_interrupt()?;
            let env = Env::new(&frame.scope, row);
            for e in exprs {
                match self.eval(e, &env)? {
                    Value::Null => {}
                    Value::Node(id) => nodes.push(id),
                    Value::Rel(id) => rels.push(id),
                    Value::Path(p) => {
                        rels.extend(p.rels);
                        nodes.extend(p.nodes);
                    }
                    other => {
                        return Err(Error::Type(format!(
                            "cannot delete a {}",
                            other.type_name()
                        )));
                    }
                }
            }
        }
        for r in rels {
            let ty = self.store().relationship_type(r)?;
            if self.store_mut()?.delete_relationship(r)? {
                self.stats.relationships_deleted += 1;
                if let Some(ty) = ty {
                    self.deleted_rels.insert(r, ty);
                }
            }
        }
        for n in nodes {
            let degree = if detach {
                self.store().degree(n, Direction::Both, &[])?
            } else {
                0
            };
            if self.store_mut()?.delete_node(n, detach)? {
                self.deleted_nodes.insert(n);
                self.stats.nodes_deleted += 1;
                self.stats.relationships_deleted += degree;
            }
        }
        Ok(())
    }

    // ---- UNWIND -------------------------------------------------------------------------

    // ---- WITH / RETURN ------------------------------------------------------------------

    /// The projection's items with `*` expanded, and its output scope.
    pub(crate) fn projection_items(&self, star: bool, items: &[Item], scope: &Scope) -> Result<(Vec<Item>, Scope)> {
        let mut all: Vec<Item> = Vec::new();
        if star {
            // `*` lists the variables in alphabetical order.
            let mut names = scope.names().to_vec();
            names.sort();
            for name in &names {
                all.push((name.clone(), Expr::Var(name.clone())));
            }
        }
        all.extend(items.iter().cloned());
        let out = Scope::from_names(all.iter().map(|(a, _)| a.clone()))?;
        Ok((all, out))
    }

    /// One row per group: projected values (keys and aggregates), then the group's first
    /// source row (hidden until `Select`).
    pub(crate) fn exec_aggregate(&self, star: bool, items: &[Item], frame: Frame) -> Result<Frame> {
        let (all, out_scope) = self.projection_items(star, items, &frame.scope)?;
        let mut aggs: Vec<AggSpec> = Vec::new();
        let rewritten: Vec<Expr> = all
            .iter()
            .map(|(_, e)| extract_aggregates(e, &mut aggs))
            .collect::<Result<_>>()?;
        let key_idx: Vec<usize> = (0..rewritten.len())
            .filter(|i| !rewritten[*i].contains_aggregate())
            .collect();
        // Variables used outside the aggregates of an aggregating item (`p` in
        // `p {.name, friends: collect(f)}` or `p.age + count(*)`) group the rows too.
        let mut implicit: Vec<usize> = Vec::new();
        for e in rewritten.iter().filter(|e| e.contains_aggregate()) {
            e.walk(&mut |x| {
                let names: &[String] = match x {
                    Expr::Var(v) => std::slice::from_ref(v),
                    Expr::Subquery { names, .. } => names,
                    _ => &[],
                };
                for n in names {
                    if let Some(i) = frame.scope.get(n)
                        && !implicit.contains(&i)
                    {
                        implicit.push(i);
                    }
                }
            });
        }
        let Frame { scope, rows, .. } = frame;
        let mut group_of: HashMap<Vec<Value>, usize> = HashMap::new();
        let mut groups: Vec<(Vec<Value>, Row, Vec<Accumulator>)> = Vec::new();
        for row in rows {
            self.check_interrupt()?;
            let env = Env::new(&scope, &row);
            let mut key = key_idx
                .iter()
                .map(|i| self.eval(&rewritten[*i], &env))
                .collect::<Result<Vec<_>>>()?;
            key.extend(implicit.iter().map(|&i| row[i].clone()));
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
        if groups.is_empty() && key_idx.is_empty() && implicit.is_empty() {
            let accs = aggs
                .iter()
                .map(Accumulator::new)
                .collect::<Result<Vec<_>>>()?;
            groups.push((Vec::new(), vec![Value::Null; scope.len()], accs));
        }
        let mut out = Vec::with_capacity(groups.len());
        for (key, first_row, accs) in groups {
            let agg_values: Vec<Value> = accs.into_iter().map(Accumulator::finish).collect();
            let mut vals = Vec::with_capacity(rewritten.len() + first_row.len());
            let mut keys = key.into_iter();
            for (i, e) in rewritten.iter().enumerate() {
                if key_idx.contains(&i) {
                    vals.push(keys.next().unwrap_or_default());
                } else {
                    let mut env = Env::new(&scope, &first_row);
                    env.aggs = Some(&agg_values);
                    vals.push(self.eval(e, &env)?);
                }
            }
            vals.extend(first_row);
            out.push(vals);
        }
        Ok(Frame {
            scope: out_scope,
            rows: out,
            hidden: Some(scope),
        })
    }

    /// Sorts projected rows. A key equal to a projection item reuses its value; other keys
    /// see the source row, with the projected aliases shadowing its variables.
    pub(crate) fn exec_sort(&self, keys: &[SortItem], items: &[Item], frame: Frame) -> Result<Frame> {
        let Frame { scope, rows, hidden } = frame;
        let src = hidden.unwrap_or_default();
        let mut keyed: Vec<(Vec<Value>, Row)> = Vec::with_capacity(rows.len());
        for row in rows {
            self.check_interrupt()?;
            keyed.push((self.sort_keys(keys, items, &scope, &src, &row)?, row));
        }
        keyed.sort_by(|(a, _), (b, _)| compare_sort_keys(keys, a, b));
        Ok(Frame {
            scope,
            rows: keyed.into_iter().map(|(_, r)| r).collect(),
            hidden: Some(src),
        })
    }

    /// The `ORDER BY` key values of one projected row (`scope` columns, then the source
    /// row in `src`). A key equal to a projection item reuses its value; other keys see
    /// the source row, with the projected aliases shadowing its variables.
    pub(crate) fn sort_keys(
        &self,
        keys: &[SortItem],
        items: &[Item],
        scope: &Scope,
        src: &Scope,
        row: &[Value],
    ) -> Result<Vec<Value>> {
        let k = scope.len();
        let star = k - items.len(); // `*` columns come before the explicit items
        let mut out = Vec::with_capacity(keys.len());
        for s in keys {
            if let Some(i) = items.iter().position(|(_, e)| *e == s.expr) {
                out.push(row[star + i].clone());
                continue;
            }
            if s.expr.contains_aggregate() {
                return Err(Error::Semantic(
                    "ORDER BY can only use aggregates that also appear in the projection".into(),
                ));
            }
            let mut env = Env::new(src, &row[k..]);
            env.locals = scope
                .names()
                .iter()
                .cloned()
                .zip(row[..k].iter().cloned())
                .collect();
            out.push(self.eval(&s.expr, &env)?);
        }
        Ok(out)
    }

    pub(crate) fn eval_count(&self, e: Option<&Expr>, what: &str) -> Result<Option<usize>> {
        let Some(e) = e else { return Ok(None) };
        let scope = Scope::default();
        match self.eval(e, &Env::new(&scope, &[]))? {
            Value::Int(n) if n >= 0 => Ok(Some(n as usize)),
            other => Err(Error::Semantic(format!(
                "{what} expects a non-negative integer, got {other}"
            ))),
        }
    }
}

/// Adds every variable introduced by `patterns` to `scope`; returns the new scope and the old length.
pub(crate) fn extend_scope<'p>(
    mut scope: Scope,
    patterns: impl Iterator<Item = &'p PatternPart>,
) -> (Scope, usize) {
    let old_len = scope.len();
    for p in patterns {
        for v in p.variables() {
            scope.add(v);
        }
    }
    (scope, old_len)
}

pub(crate) fn initial_bound(old_len: usize, new_len: usize) -> Vec<bool> {
    let mut b = vec![true; old_len];
    b.resize(new_len, false);
    b
}

/// Compares two rows' `ORDER BY` key values (from [`Executor::sort_keys`]).
pub(crate) fn compare_sort_keys(keys: &[SortItem], a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    for (i, s) in keys.iter().enumerate() {
        let o = order_cmp(&a[i], &b[i]);
        let o = if s.descending { o.reverse() } else { o };
        if o.is_ne() {
            return o;
        }
    }
    std::cmp::Ordering::Equal
}
