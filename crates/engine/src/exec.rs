//! Runs query plans ([`crate::plan`]).
//!
//! Read-only stretches of a plan run as pull-based streams ([`crate::stream`]). Updates
//! are eager: an update first materializes everything below it, applies itself to every
//! row, and hands its rows to the stream above. Every clause therefore sees the effects
//! of the clauses before it, and a `LIMIT` above an update never skips part of it.

use std::collections::{BTreeMap, HashMap, HashSet};
use graphbuff_core::clock::Instant;

use graphbuff_core::{
    ConstraintDef, ConstraintKind, Direction, Error, IndexDef, IndexEntity, NodeId, Path, RelId, Result, Value,
};
use graphbuff_cypher::ast::{IndexType, 
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

/// Where [`Executor::run_plan_into`] sends the result.
pub(crate) trait RawSink {
    /// The column names, once, before any row.
    fn columns(&mut self, columns: &[String]) -> Result<()>;
    /// The next rows (never empty); `ex` materializes their entities.
    fn rows(&mut self, ex: &Executor, rows: Vec<Row>) -> Result<()>;
}

/// Sends the first part's columns to `sink`; later parts (of a `UNION`) must match them.
fn same_columns(columns: &mut Option<Vec<String>>, these: &[String], sink: &mut dyn RawSink) -> Result<()> {
    match columns {
        None => {
            sink.columns(these)?;
            *columns = Some(these.to_vec());
            Ok(())
        }
        Some(c) if c.as_slice() != these => Err(Error::Semantic(
            "all sub queries in a UNION must have the same return column names".into(),
        )),
        Some(_) => Ok(()),
    }
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

    /// [`run_plan`](Self::run_plan), handing the result rows to `sink` batch by batch as
    /// the plan produces them instead of collecting them. Returns the column names.
    pub fn run_plan_into(&mut self, plan: &QueryPlan, sink: &mut dyn RawSink) -> Result<Vec<String>> {
        let mut columns: Option<Vec<String>> = None;
        // `UNION` (without `ALL`): the rows sent so far, to drop repeats.
        let mut seen = (plan.parts.len() > 1 && !plan.union_all).then(HashSet::new);
        let mut send = |ex: &Executor, rows: Vec<Row>, sink: &mut dyn RawSink| -> Result<()> {
            let rows = match &mut seen {
                Some(seen) => rows.into_iter().filter(|r| seen.insert(r.clone())).collect(),
                None => rows,
            };
            if rows.is_empty() { Ok(()) } else { sink.rows(ex, rows) }
        };
        for part in &plan.parts {
            match part {
                PartPlan::Pipeline { root, returns: true } if !root.is_update() => {
                    let base = self.chain_input(root)?;
                    let ex: &Executor = self;
                    let mut stream = crate::stream::build(ex, root, base)?;
                    same_columns(&mut columns, stream.scope().names(), sink)?;
                    while let Some(batch) = stream.next(ex, crate::stream::BATCH)? {
                        send(ex, batch, sink)?;
                    }
                }
                _ => {
                    let raw = self.run_part(part)?;
                    same_columns(&mut columns, &raw.columns, sink)?;
                    send(self, raw.rows, sink)?;
                }
            }
        }
        Ok(columns.unwrap_or_default())
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
                optional,
            } => {
                let frame = self.run_op(input, None)?;
                self.exec_call(frame, plan, imports, columns, *returns, *optional)?
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
            O::ProcedureCall {
                input,
                procedure,
                args,
                fields,
                optional,
            } if procedure.signature.mode == crate::procedures::ProcedureMode::Write => {
                let frame = self.run_op(input, None)?;
                self.exec_write_procedure(frame, procedure, args, fields, *optional)?
            }
            _ => {
                // A read-only chain: materialize the update it starts on (if any), then
                // stream the rest.
                let base = self.chain_input(op)?;
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

    /// The input of the read-only chain ending in `op`: the materialized result of the
    /// update it starts on, or the argument row (the single empty row at the top).
    fn chain_input(&mut self, op: &Operator) -> Result<Frame> {
        Ok(match op.chain_base() {
            Some(update) => self.run_op(update, None)?,
            None => match &self.argument {
                Some((scope, row)) => Frame::new(scope.clone(), vec![row.clone()]),
                None => Frame::new(Scope::default(), vec![Vec::new()]),
            },
        })
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
        optional: bool,
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
            if optional && result.rows.is_empty() {
                row.extend(columns.iter().map(|_| Value::Null));
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

    /// Runs a write procedure once per row of `frame`, in order, making the changes each
    /// call asks for when it returns (so the next call sees them).
    fn exec_write_procedure(
        &mut self,
        frame: Frame,
        procedure: &crate::procedures::Procedure,
        args: &[Expr],
        fields: &[(usize, String)],
        optional: bool,
    ) -> Result<Frame> {
        use crate::procedures::ProcedureWrite as W;
        let mut scope = frame.scope.clone();
        for (_, v) in fields {
            scope.add(v);
        }
        let mut rows = Vec::new();
        for row in frame.rows {
            self.check_interrupt()?;
            let env = Env::new(&frame.scope, &row);
            let values: Vec<Value> = args.iter().map(|a| self.eval(a, &env)).collect::<Result<_>>()?;
            let ctx = crate::procedures::ProcedureContext {
                store: self.store(),
                procedures: &self.procedures,
                accel: None,
                interrupt: self.interrupt.as_ref(),
                embedder: self.embedder.as_ref(),
                writes: Some(Default::default()),
            };
            let out = procedure.call(&ctx, &values)?;
            let writes = ctx.writes.expect("set above").into_inner();
            for w in writes {
                match w {
                    W::Property { target, key, value } => self.set_property(&target, &key, value)?,
                    W::Label { node, label, add: true } => {
                        if self.store_mut()?.add_label(node, &label)? {
                            self.stats.labels_added += 1;
                            self.touch(node);
                        }
                    }
                    W::Label { node, label, add: false } => {
                        if self.store_mut()?.remove_label(node, &label)? {
                            self.stats.labels_removed += 1;
                            self.touch(node);
                        }
                    }
                }
            }
            if procedure.signature.outputs.is_empty() {
                rows.push(row);
            } else if optional && out.is_empty() {
                let mut o = row;
                o.extend(fields.iter().map(|_| Value::Null));
                rows.push(o);
            } else {
                for r in out {
                    let mut o = row.clone();
                    o.extend(fields.iter().map(|(i, _)| r[*i].clone()));
                    rows.push(o);
                }
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
                labels,
                properties,
                if_not_exists,
                kind,
                options,
            } => {
                let options = match options {
                    Some(e) => {
                        let scope = Scope::default();
                        match self.eval(e, &Env::new(&scope, &[]))? {
                            Value::Map(m) => m,
                            Value::Null => BTreeMap::new(),
                            o => return Err(Error::Type(format!("OPTIONS must be a map, not {}", o.type_name()))),
                        }
                    }
                    None => BTreeMap::new(),
                };
                let prefix = match kind {
                    IndexType::Range => "index",
                    IndexType::Vector => "vector",
                    IndexType::Fulltext => "fulltext",
                    IndexType::Point => "point",
                    IndexType::Text => "text",
                    IndexType::Lookup => "lookup",
                };
                let mut labels = labels.iter();
                let label = labels.next().cloned().unwrap_or_default();
                let def = IndexDef {
                    name: name.clone().unwrap_or_else(|| {
                        if *kind == IndexType::Lookup {
                            return match entity {
                                IndexEntity::Node => "lookup_node_labels".into(),
                                IndexEntity::Relationship => "lookup_relationship_types".into(),
                            };
                        }
                        let mut parts = vec![prefix.to_string(), label.clone()];
                        parts.extend(labels.clone().cloned());
                        parts.extend(properties.iter().cloned());
                        parts.join("_")
                    }),
                    entity: *entity,
                    label,
                    extra_labels: labels.cloned().collect(),
                    properties: properties.clone(),
                    kind: crate::vector::index_kind(*kind, &options)?,
                };
                let existing = self.store().indexes();
                let exists = existing.iter().any(|d| d.name == def.name || d.same_index(&def));
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
                columns: ["name", "entityType", "labelOrType", "properties", "type", "options"]
                    .map(String::from)
                    .to_vec(),
                rows: self.store().indexes().into_iter().map(crate::vector::index_row).collect(),
            }),
            Clause::CreateConstraint {
                name,
                entity,
                kind,
                label,
                properties,
                if_not_exists,
            } => {
                let def = ConstraintDef {
                    name: name
                        .clone()
                        .unwrap_or_else(|| format!("constraint_{label}_{}", properties.join("_"))),
                    entity: *entity,
                    kind: *kind,
                    label: label.clone(),
                    properties: properties.clone(),
                };
                let existing = self.store().constraints();
                let clash = existing.iter().any(|c| c.name == def.name || c.same_target(&def));
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
                        ix.name == def.name && ix.covers(def.entity, &def.label, &def.properties[..1])
                    });
                    if owned && self.check_index_unused(&def.name).is_ok() {
                        self.store_mut()?.drop_index(&def.name)?;
                        self.stats.indexes_removed += 1;
                    }
                }
                Ok(empty)
            }
            Clause::ShowFunctions { user_defined } => Ok(RawResult {
                columns: crate::catalog::FUNCTION_COLUMNS.map(String::from).to_vec(),
                rows: if *user_defined { Vec::new() } else { crate::catalog::function_rows() },
            }),
            Clause::ShowProcedures => Ok(RawResult {
                columns: ["name", "signature", "description", "mode"].map(String::from).to_vec(),
                rows: self.procedures.listing(),
            }),
            Clause::ShowConstraints => Ok(RawResult {
                columns: CONSTRAINT_COLUMNS.map(String::from).to_vec(),
                rows: self.store().constraints().into_iter().map(constraint_row).collect(),
            }),
            _ => unreachable!("not a schema clause"),
        }
    }

    // ---- constraints ------------------------------------------------------------------

    /// Creates `def` (already checked against the existing constraints): first the index a
    /// uniqueness check seeks on, if there is none, then the constraint, then checks every
    /// entity that has its label (type). On error, the query rolls all of it back.
    fn create_constraint(&mut self, def: ConstraintDef) -> Result<()> {
        if def.requires_unique()
            && self.store().schema_index_stats(def.entity, &def.label, &def.properties[..1]).is_none()
        {
            let (name, label, property) = (def.name.clone(), def.label.clone(), def.properties[0].clone());
            self.store_mut()?.create_index(match def.entity {
                IndexEntity::Node => IndexDef::node(name, label, property),
                IndexEntity::Relationship => IndexDef::relationship(name, label, property),
            })?;
            self.stats.indexes_added += 1;
        }
        self.store_mut()?.create_constraint(def.clone())?;
        self.stats.constraints_added += 1;
        for id in self.entities_with(def.entity, &def.label)? {
            self.check_entity(&def, id).map_err(|e| match e {
                Error::Constraint(msg) => Error::Constraint(format!(
                    "cannot create constraint `{}`: existing data violates it: {msg}",
                    def.name
                )),
                other => other,
            })?;
        }
        Ok(())
    }

    /// The nodes with label `label`, or the relationships of type `label`, by id.
    fn entities_with(&self, entity: IndexEntity, label: &str) -> Result<Vec<u64>> {
        let store = self.store();
        match entity {
            IndexEntity::Node => store.nodes_with_label(label).map(|id| id.map(|id| id.0)).collect(),
            IndexEntity::Relationship => {
                let mut out = Vec::new();
                for id in store.all_relationships() {
                    let id = id?;
                    if store.relationship_type(id)?.as_deref() == Some(label) {
                        out.push(id.0);
                    }
                }
                Ok(out)
            }
        }
    }

    /// Fails if index `name` is the one a uniqueness constraint seeks on.
    fn check_index_unused(&self, name: &str) -> Result<()> {
        let Some(ix) = self.store().indexes().into_iter().find(|ix| ix.name == name) else {
            return Ok(());
        };
        match self.store().constraints().into_iter().find(|c| {
            c.requires_unique() && ix.covers(c.entity, &c.label, &c.properties[..1])
        }) {
            Some(c) => Err(Error::Constraint(format!(
                "index `{name}` is used by constraint `{}`; drop the constraint first",
                c.name
            ))),
            None => Ok(()),
        }
    }

    /// Checks every node and relationship this query created or changed against the
    /// constraints of its labels (type). Runs once, after the last clause and before
    /// commit, so a query may pass through violating states (swap two unique values,
    /// create a node and then set its key) as long as it ends in a valid one.
    pub(crate) fn check_constraints(&mut self) -> Result<()> {
        let nodes = self.touched.take().unwrap_or_default();
        let rels = self.touched_rels.take().unwrap_or_default();
        if nodes.is_empty() && rels.is_empty() {
            return Ok(());
        }
        let constraints = self.store().constraints();
        for id in nodes {
            if !self.store().node_exists(id)? {
                continue;
            }
            let labels = self.store().node_labels(id)?;
            for c in constraints.iter().filter(|c| c.entity == IndexEntity::Node && labels.contains(&c.label)) {
                self.check_entity(c, id.0)?;
            }
        }
        for id in rels {
            let Some(rel_type) = self.store().relationship_type(id)? else { continue };
            for c in constraints.iter().filter(|c| c.entity == IndexEntity::Relationship && c.label == rel_type) {
                self.check_entity(c, id.0)?;
            }
        }
        Ok(())
    }

    /// Checks entity `id` (a node with `c`'s label or a relationship of its type)
    /// against `c`.
    fn check_entity(&self, c: &ConstraintDef, id: u64) -> Result<()> {
        let store = self.store();
        let property = |id: u64, p: &str| match c.entity {
            IndexEntity::Node => store.node_property(NodeId(id), p),
            IndexEntity::Relationship => store.relationship_property(RelId(id), p),
        };
        let what = |id: u64| match c.entity {
            IndexEntity::Node => format!("node {id} with label `{}`", c.label),
            IndexEntity::Relationship => format!("relationship {id} of type `{}`", c.label),
        };
        let mut values = Vec::with_capacity(c.properties.len());
        for p in &c.properties {
            values.push(property(id, p)?);
        }
        if let ConstraintKind::Typed(t) = c.kind {
            return match &values[0] {
                Some(v) if !t.accepts(v) => Err(Error::Constraint(format!(
                    "{} has `{}` of type {}, but constraint `{}` requires {t}",
                    what(id),
                    c.properties[0],
                    v.type_name(),
                    c.name
                ))),
                _ => Ok(()),
            };
        }
        if c.requires_properties()
            && let Some(i) = values.iter().position(Option::is_none)
        {
            return Err(Error::Constraint(format!(
                "{} must have the property `{}` (constraint `{}`)",
                what(id),
                c.properties[i],
                c.name
            )));
        }
        if !c.requires_unique() || values.iter().any(Option::is_none) {
            return Ok(());
        }
        let values: Vec<Value> = values.into_iter().flatten().collect();
        let candidates = match store.schema_index_seek(c.entity, &c.label, &c.properties[..1], &values[..1])? {
            Some(ids) => ids,
            None => self.entities_with(c.entity, &c.label)?,
        };
        for other in candidates {
            if other == id {
                continue;
            }
            let mut same = true;
            for (p, v) in c.properties.iter().zip(&values) {
                let w = property(other, p)?;
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
                let (plural, label) = match c.entity {
                    IndexEntity::Node => ("nodes", format!("with label `{}`", c.label)),
                    IndexEntity::Relationship => ("relationships", format!("of type `{}`", c.label)),
                };
                return Err(Error::Constraint(format!(
                    "{plural} {other} and {id} {label} both have {} (constraint `{}`)",
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
            self.touch_rel(id);
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
            Value::Rel(id) => {
                self.store_mut()?.set_relationship_property(*id, key, value)?;
                self.touch_rel(*id);
            }
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

    /// The grouping of an aggregating projection over rows of `scope`, and the scope of
    /// its output rows.
    pub(crate) fn grouping(&self, star: bool, items: &[Item], scope: &Scope) -> Result<(Grouping, Scope)> {
        let (all, out_scope) = self.projection_items(star, items, scope)?;
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
                    if let Some(i) = scope.get(n)
                        && !implicit.contains(&i)
                    {
                        implicit.push(i);
                    }
                }
            });
        }
        let grouping = Grouping {
            scope: scope.clone(),
            rewritten,
            aggs,
            key_idx,
            implicit,
            group_of: HashMap::new(),
            groups: Vec::new(),
        };
        Ok((grouping, out_scope))
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

/// Aggregation as it goes: rows are added one at a time and only the groups are kept
/// (their keys, accumulators and first source row), not the rows.
pub(crate) struct Grouping {
    /// The scope of the input rows.
    scope: Scope,
    /// The projection items, aggregate calls replaced by `Expr::AggregateRef`.
    rewritten: Vec<Expr>,
    aggs: Vec<AggSpec>,
    /// The items without aggregates: the grouping keys.
    key_idx: Vec<usize>,
    /// Input columns used outside the aggregates of an aggregating item (they group too).
    implicit: Vec<usize>,
    group_of: HashMap<Vec<Value>, usize>,
    /// Per group, in order of appearance: its first source row and its accumulators.
    groups: Vec<(Row, Vec<Accumulator>)>,
}

impl Grouping {
    pub(crate) fn add(&mut self, ex: &Executor, row: Row) -> Result<()> {
        let env = Env::new(&self.scope, &row);
        let mut key = self
            .key_idx
            .iter()
            .map(|i| ex.eval(&self.rewritten[*i], &env))
            .collect::<Result<Vec<_>>>()?;
        key.extend(self.implicit.iter().map(|&i| row[i].clone()));
        let (gi, new) = match self.group_of.get(&key) {
            Some(gi) => (*gi, false),
            None => {
                let accs = self.aggs.iter().map(Accumulator::new).collect::<Result<Vec<_>>>()?;
                self.groups.push((Vec::new(), accs));
                self.group_of.insert(key, self.groups.len() - 1);
                (self.groups.len() - 1, true)
            }
        };
        for (ai, spec) in self.aggs.iter().enumerate() {
            let v = match &spec.arg {
                None => Value::Null,
                Some(e) => ex.eval(e, &env)?,
            };
            self.groups[gi].1[ai].update(v)?;
        }
        drop(env);
        if new {
            self.groups[gi].0 = row;
        }
        Ok(())
    }

    /// One row per group: projected values (keys and aggregates), then the group's first
    /// source row (hidden until `Select`).
    pub(crate) fn finish(self, ex: &Executor) -> Result<Vec<Row>> {
        let Grouping {
            scope,
            rewritten,
            aggs,
            key_idx,
            implicit,
            group_of,
            mut groups,
        } = self;
        let mut keys: Vec<Vec<Value>> = vec![Vec::new(); groups.len()];
        for (key, gi) in group_of {
            keys[gi] = key;
        }
        if groups.is_empty() && key_idx.is_empty() && implicit.is_empty() {
            let accs = aggs.iter().map(Accumulator::new).collect::<Result<Vec<_>>>()?;
            groups.push((vec![Value::Null; scope.len()], accs));
            keys.push(Vec::new());
        }
        let mut out = Vec::with_capacity(groups.len());
        for ((first_row, accs), key) in groups.into_iter().zip(keys) {
            let agg_values: Vec<Value> = accs.into_iter().map(Accumulator::finish).collect();
            let mut vals = Vec::with_capacity(rewritten.len() + first_row.len());
            let mut keys = key.into_iter();
            for (i, e) in rewritten.iter().enumerate() {
                if key_idx.contains(&i) {
                    vals.push(keys.next().unwrap_or_default());
                } else {
                    let mut env = Env::new(&scope, &first_row);
                    env.aggs = Some(&agg_values);
                    vals.push(ex.eval(e, &env)?);
                }
            }
            vals.extend(first_row);
            out.push(vals);
        }
        Ok(out)
    }
}

/// The columns of `SHOW CONSTRAINTS` and `db.constraints()`.
pub(crate) const CONSTRAINT_COLUMNS: [&str; 6] = ["name", "type", "label", "properties", "entityType", "propertyType"];

/// One row of `SHOW CONSTRAINTS`: `label` is the label or relationship type, and
/// `propertyType` is null unless the constraint is a property type constraint.
pub(crate) fn constraint_row(c: ConstraintDef) -> Vec<Value> {
    vec![
        Value::String(c.name),
        Value::String(c.kind.text(c.entity).into()),
        Value::String(c.label),
        Value::List(c.properties.into_iter().map(Value::String).collect()),
        Value::String(c.entity.text().into()),
        match c.kind {
            ConstraintKind::Typed(t) => Value::String(t.to_string()),
            _ => Value::Null,
        },
    ]
}
