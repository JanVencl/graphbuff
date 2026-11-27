//! Procedures run by `CALL name(args) YIELD …`, and the registry that names them.
//!
//! A procedure takes argument values and returns rows of output values, one value per
//! output field of its [`ProcedureSignature`]. A procedure sees the store through a
//! [`ProcedureContext`]. Procedures read, unless their signature says
//! [`ProcedureMode::Write`]: those change the graph through the context
//! ([`ProcedureContext::set_property`], [`add_label`](ProcedureContext::add_label),
//! [`remove_label`](ProcedureContext::remove_label)), and a query that calls one is a
//! writing query (it runs as a write and needs write access). Their changes are made when
//! the call returns, in order, so the next input row's call sees them; the call itself
//! does not. The built-in ones (`db.labels()`,
//! `db.relationshipTypes()`, `db.propertyKeys()`, the `db.schema.*` procedures, `db.indexes()`, `db.constraints()`,
//! `db.awaitIndexes()`, `dbms.procedures()`, `dbms.components()`, the writing
//! `db.create.setNodeVectorProperty()` and `db.create.setRelationshipVectorProperty()`, and the graph
//! algorithms `algo.pageRank`, `algo.wcc`, `algo.triangleCount`, `algo.bfs`) are in every
//! registry; applications add their own with [`crate::Database::register_procedure`].
//!
//! The graph algorithms run as kernels ([`graphbuff_core::Kernels`]) over the CSR export
//! of the state the query reads (cached per committed state; inside a transaction it is
//! built from the transaction's own view). Each takes an optional configuration map:
//! `relationshipTypes` (a type or a list; default all), `orientation` (`'NATURAL'`,
//! `'REVERSE'` or `'UNDIRECTED'`), and per algorithm `maxIterations`, `dampingFactor`,
//! `tolerance` (PageRank) or `maxDepth` (BFS).
//!
//! ```
//! use graphbuff::{Database, ProcedureSignature, Value};
//!
//! let db = Database::in_memory();
//! db.register_procedure(
//!     ProcedureSignature::new("my.range", ["n"], ["i"]).description("0 .. n-1"),
//!     |_ctx, args| {
//!         let n = match args[0] { Value::Int(n) => n, _ => 0 };
//!         Ok((0..n).map(|i| vec![Value::Int(i)]).collect())
//!     },
//! )?;
//! let r = db.execute("CALL my.range(3) YIELD i RETURN sum(i) AS total")?;
//! assert_eq!(r.get(0, "total").and_then(|v| v.as_int()), Some(3));
//! # Ok::<(), graphbuff::Error>(())
//! ```

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, OnceLock};

use graphbuff_core::{CpuKernels, Csr, Direction, Error, GraphStore, Kernels, NodeId, Result, Value};

/// The rows a procedure returns: one value per output field, in signature order.
pub type ProcedureRows = Vec<Vec<Value>>;

/// A procedure's body.
pub type ProcedureFn = dyn Fn(&ProcedureContext<'_>, &[Value]) -> Result<ProcedureRows> + Send + Sync;

/// A procedure's name, parameters and output fields.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcedureSignature {
    /// Dotted name, such as `db.labels` (case-sensitive).
    pub name: String,
    /// Parameters in order; trailing ones may have defaults.
    pub inputs: Vec<ProcedureParam>,
    /// Output field names in order. None for a `VOID` procedure, which leaves the rows
    /// of the query unchanged.
    pub outputs: Vec<String>,
    pub description: String,
    /// Whether the procedure only reads ([`ProcedureMode::Read`], the default) or may
    /// change the graph.
    pub mode: ProcedureMode,
}

/// What a procedure may do to the graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcedureMode {
    /// It only reads.
    #[default]
    Read,
    /// It may change the graph (through [`ProcedureContext::set_property`] and the like).
    Write,
}

impl ProcedureMode {
    /// `READ` or `WRITE`, as `SHOW PROCEDURES` lists it.
    pub fn name(self) -> &'static str {
        match self {
            ProcedureMode::Read => "READ",
            ProcedureMode::Write => "WRITE",
        }
    }
}

/// A procedure parameter: its name, its type and, if it is optional, its default.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcedureParam {
    pub name: String,
    pub ty: ProcedureType,
    pub default: Option<Value>,
}

/// The type of a procedure parameter. Every type takes `null`; `FLOAT` and `NUMBER` take
/// integers too. An argument of another type fails the call: at compile time when its
/// type is known then (a literal), else when the call runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcedureType {
    #[default]
    Any,
    Boolean,
    Integer,
    Float,
    Number,
    String,
    List,
    Map,
    Node,
    Relationship,
    Path,
}

impl ProcedureType {
    /// The type named `text` as signatures write it (`INTEGER`, `INTEGER?`, `LIST OF ANY?`,
    /// case-insensitive); `None` for an unknown name.
    pub fn parse(text: &str) -> Option<ProcedureType> {
        let t = text.trim().trim_end_matches('?').trim().to_ascii_uppercase();
        let t = if t.starts_with("LIST") { "LIST" } else { t.as_str() };
        Some(match t {
            "ANY" => ProcedureType::Any,
            "BOOLEAN" => ProcedureType::Boolean,
            "INTEGER" => ProcedureType::Integer,
            "FLOAT" => ProcedureType::Float,
            "NUMBER" => ProcedureType::Number,
            "STRING" => ProcedureType::String,
            "LIST" => ProcedureType::List,
            "MAP" => ProcedureType::Map,
            "NODE" => ProcedureType::Node,
            "RELATIONSHIP" => ProcedureType::Relationship,
            "PATH" => ProcedureType::Path,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            ProcedureType::Any => "ANY",
            ProcedureType::Boolean => "BOOLEAN",
            ProcedureType::Integer => "INTEGER",
            ProcedureType::Float => "FLOAT",
            ProcedureType::Number => "NUMBER",
            ProcedureType::String => "STRING",
            ProcedureType::List => "LIST",
            ProcedureType::Map => "MAP",
            ProcedureType::Node => "NODE",
            ProcedureType::Relationship => "RELATIONSHIP",
            ProcedureType::Path => "PATH",
        }
    }

    /// Whether an argument of this type may be `v`.
    pub fn accepts(self, v: &Value) -> bool {
        use ProcedureType as P;
        matches!(
            (self, v),
            (P::Any, _)
                | (_, Value::Null)
                | (P::Boolean, Value::Bool(_))
                | (P::Integer, Value::Int(_))
                | (P::Float | P::Number, Value::Int(_) | Value::Float(_))
                | (P::String, Value::String(_))
                | (P::List, Value::List(_))
                | (P::Map, Value::Map(_))
                | (P::Node, Value::Node(_))
                | (P::Relationship, Value::Rel(_))
                | (P::Path, Value::Path(_))
        )
    }
}

impl ProcedureSignature {
    /// A signature with required parameters `inputs` and output fields `outputs`.
    pub fn new<I, O>(name: &str, inputs: I, outputs: O) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
        O: IntoIterator,
        O::Item: Into<String>,
    {
        ProcedureSignature {
            name: name.to_string(),
            inputs: inputs
                .into_iter()
                .map(|n| ProcedureParam {
                    name: n.into(),
                    ty: ProcedureType::Any,
                    default: None,
                })
                .collect(),
            outputs: outputs.into_iter().map(Into::into).collect(),
            description: String::new(),
            mode: ProcedureMode::Read,
        }
    }

    /// Adds an optional parameter with a default value (after the others).
    pub fn optional(mut self, name: &str, default: impl Into<Value>) -> Self {
        self.inputs.push(ProcedureParam {
            name: name.to_string(),
            ty: ProcedureType::Any,
            default: Some(default.into()),
        });
        self
    }

    /// Gives the parameters types, in order (the rest stay `ANY`).
    pub fn input_types(mut self, types: impl IntoIterator<Item = ProcedureType>) -> Self {
        for (p, ty) in self.inputs.iter_mut().zip(types) {
            p.ty = ty;
        }
        self
    }

    pub fn description(mut self, text: &str) -> Self {
        self.description = text.to_string();
        self
    }

    /// Marks the procedure as one that changes the graph ([`ProcedureMode::Write`]).
    pub fn writes(mut self) -> Self {
        self.mode = ProcedureMode::Write;
        self
    }

    /// The number of parameters without a default.
    pub fn required(&self) -> usize {
        self.inputs.iter().filter(|p| p.default.is_none()).count()
    }

    /// `name(a, b = 1) :: (x, y)`, or `… :: VOID`.
    pub fn text(&self) -> String {
        let inputs: Vec<String> = self
            .inputs
            .iter()
            .map(|p| {
                let typed = match p.ty {
                    ProcedureType::Any => p.name.clone(),
                    t => format!("{} :: {}", p.name, t.name()),
                };
                match &p.default {
                    Some(d) => format!("{typed} = {d}"),
                    None => typed,
                }
            })
            .collect();
        let outputs = if self.outputs.is_empty() {
            "VOID".to_string()
        } else {
            format!("({})", self.outputs.join(", "))
        };
        format!("{}({}) :: {outputs}", self.name, inputs.join(", "))
    }

    fn check(&self) -> Result<()> {
        let bad = |why: &str| Err(Error::Semantic(format!("invalid procedure `{}`: {why}", self.name)));
        let ident = |s: &str| {
            !s.is_empty()
                && !s.starts_with(|c: char| c.is_ascii_digit())
                && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        };
        if !self.name.split('.').all(ident) {
            return bad("the name must be dot-separated identifiers");
        }
        if self.inputs.iter().skip_while(|p| p.default.is_none()).any(|p| p.default.is_none()) {
            return bad("parameters with defaults must come last");
        }
        let mut seen = std::collections::HashSet::new();
        if let Some(o) = self.outputs.iter().find(|o| !ident(o) || !seen.insert(o.as_str())) {
            return bad(&format!("output field `{o}` is not a unique identifier"));
        }
        Ok(())
    }
}

/// What a procedure can see while it runs.
pub struct ProcedureContext<'a> {
    pub(crate) store: &'a dyn GraphStore,
    pub(crate) procedures: &'a Procedures,
    pub(crate) accel: Option<&'a crate::accel::Accel>,
    pub(crate) interrupt: Option<&'a crate::interrupt::Interrupt>,
    pub(crate) embedder: Option<&'a Arc<dyn crate::embed::Embedder>>,
    /// The changes a [`ProcedureMode::Write`] procedure asked for; `None` for the others.
    pub(crate) writes: Option<RefCell<Vec<ProcedureWrite>>>,
}

/// A change a write procedure asked for, made when its call returns.
#[derive(Debug)]
pub(crate) enum ProcedureWrite {
    /// Sets (or with `Null` removes) a property of a node or relationship.
    Property { target: Value, key: String, value: Value },
    Label { node: NodeId, label: String, add: bool },
}

impl<'a> ProcedureContext<'a> {
    /// The graph as it was when the call started (without the call's own changes).
    pub fn store(&self) -> &'a dyn GraphStore {
        self.store
    }

    /// Sets property `key` of `target` (a node or relationship; `null` does nothing) to
    /// `value`, or removes it if `value` is `null`. Only for [`ProcedureMode::Write`]
    /// procedures; done when the call returns.
    pub fn set_property(&self, target: &Value, key: &str, value: Value) -> Result<()> {
        if !matches!(target, Value::Node(_) | Value::Rel(_) | Value::Null) {
            return Err(Error::Type(format!("cannot set properties on a {}", target.type_name())));
        }
        self.write(ProcedureWrite::Property { target: target.clone(), key: key.to_string(), value })
    }

    /// Adds `label` to `node`. Only for [`ProcedureMode::Write`] procedures; done when the
    /// call returns.
    pub fn add_label(&self, node: NodeId, label: &str) -> Result<()> {
        self.write(ProcedureWrite::Label { node, label: label.to_string(), add: true })
    }

    /// Removes `label` from `node`. Only for [`ProcedureMode::Write`] procedures; done
    /// when the call returns.
    pub fn remove_label(&self, node: NodeId, label: &str) -> Result<()> {
        self.write(ProcedureWrite::Label { node, label: label.to_string(), add: false })
    }

    fn write(&self, change: ProcedureWrite) -> Result<()> {
        match &self.writes {
            Some(w) => {
                w.borrow_mut().push(change);
                Ok(())
            }
            None => Err(Error::Semantic(
                "a read procedure cannot change the graph (register it with ProcedureSignature::writes)".into(),
            )),
        }
    }

    /// The registry the call was resolved in.
    pub fn procedures(&self) -> &'a Procedures {
        self.procedures
    }

    /// The CSR export of [`store`](Self::store): the cached one of the committed state
    /// a read query sees, or (inside a transaction) one built for this call.
    pub fn csr(&self) -> Result<Arc<Csr>> {
        match self.accel {
            Some(a) => a.cache.csr(&a.snapshot),
            None => Csr::build(self.store).map(Arc::new),
        }
    }

    /// The kernel backend of the database ([`CpuKernels`] if none is set).
    pub fn kernels(&self) -> Arc<dyn Kernels> {
        match self.accel {
            Some(a) => a.kernels.clone(),
            None => Arc::new(CpuKernels::new()),
        }
    }

    /// The vector index cache and the committed state the call reads, if it reads one
    /// (not inside a transaction).
    pub(crate) fn vectors(&self) -> Option<(&'a crate::vector::VectorCache, &'a graphbuff_core::Snapshot)> {
        self.accel.map(|a| (&*a.vectors, &a.snapshot))
    }

    /// The full-text index cache and the committed state the call reads, if it reads one
    /// (not inside a transaction).
    pub(crate) fn texts(&self) -> Option<(&'a crate::fulltext::TextCache, &'a graphbuff_core::Snapshot)> {
        self.accel.map(|a| (&*a.texts, &a.snapshot))
    }

    /// The database's [`crate::Embedder`]; `Unsupported` if none is set.
    pub fn embedder(&self) -> Result<&'a dyn crate::embed::Embedder> {
        self.embedder.map(|e| e.as_ref()).ok_or_else(crate::embed::not_available)
    }

    /// Fails if the query ran out of time or was cancelled; long procedures call it
    /// between steps.
    pub fn check_interrupt(&self) -> Result<()> {
        match self.interrupt {
            Some(i) => i.check_now(),
            None => Ok(()),
        }
    }
}

/// A registered procedure.
pub struct Procedure {
    pub signature: ProcedureSignature,
    body: Box<ProcedureFn>,
}

impl Procedure {
    /// Runs the procedure with `args` (one per parameter, defaults filled in) and checks
    /// that every row has one value per output field.
    pub(crate) fn call(&self, ctx: &ProcedureContext<'_>, args: &[Value]) -> Result<ProcedureRows> {
        for (p, a) in self.signature.inputs.iter().zip(args) {
            if !p.ty.accepts(a) {
                return Err(Error::Type(format!(
                    "procedure `{}` expects {} for `{}`, got {}",
                    self.signature.name,
                    p.ty.name(),
                    p.name,
                    a.type_name()
                )));
            }
        }
        let rows = (self.body)(ctx, args)?;
        let width = self.signature.outputs.len();
        if let Some(r) = rows.iter().find(|r| r.len() != width) {
            return Err(Error::Runtime(format!(
                "procedure `{}` returned a row of {} values for {width} output fields",
                self.signature.name,
                r.len()
            )));
        }
        Ok(rows)
    }
}

impl fmt::Debug for Procedure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Procedure({})", self.signature.text())
    }
}

/// A set of procedures by name: the built-ins plus any registered ones.
#[derive(Clone, Debug)]
pub struct Procedures {
    map: BTreeMap<String, Arc<Procedure>>,
}

impl Default for Procedures {
    fn default() -> Self {
        Self::builtin().as_ref().clone()
    }
}

impl Procedures {
    /// The built-in procedures (shared).
    pub(crate) fn builtin() -> Arc<Procedures> {
        static BUILTIN: OnceLock<Arc<Procedures>> = OnceLock::new();
        BUILTIN
            .get_or_init(|| {
                let mut p = Procedures { map: BTreeMap::new() };
                register_builtins(&mut p);
                Arc::new(p)
            })
            .clone()
    }

    /// Adds a procedure. Fails if the signature is malformed or the name is taken.
    pub fn register<F>(&mut self, signature: ProcedureSignature, body: F) -> Result<()>
    where
        F: Fn(&ProcedureContext<'_>, &[Value]) -> Result<ProcedureRows> + Send + Sync + 'static,
    {
        signature.check()?;
        if self.map.contains_key(&signature.name) {
            return Err(Error::Semantic(format!(
                "a procedure named `{}` already exists",
                signature.name
            )));
        }
        self.map.insert(
            signature.name.clone(),
            Arc::new(Procedure {
                signature,
                body: Box::new(body),
            }),
        );
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Arc<Procedure>> {
        self.map.get(name)
    }

    /// Every procedure's signature, by name.
    pub fn signatures(&self) -> impl Iterator<Item = &ProcedureSignature> {
        self.map.values().map(|p| &p.signature)
    }

    /// Looks up `name` for a call, with a helpful error.
    pub(crate) fn resolve(&self, name: &str) -> Result<&Arc<Procedure>> {
        self.get(name).ok_or_else(|| {
            let hint = self
                .map
                .keys()
                .find(|k| k.eq_ignore_ascii_case(name))
                .map(|k| format!(" (did you mean `{k}`?)"))
                .unwrap_or_default();
            Error::Semantic(format!("there is no procedure named `{name}`{hint}"))
        })
    }

    /// Whether `query` calls a [`ProcedureMode::Write`] procedure of this registry (in
    /// any part, `CALL { … }` subquery or `FOREACH`).
    pub fn writes_in(&self, query: &graphbuff_cypher::ast::Query) -> bool {
        query.parts.iter().any(|part| self.writes_in_clauses(&part.clauses))
    }

    /// [`writes_in`](Self::writes_in) for a list of clauses.
    pub(crate) fn writes_in_clauses(&self, clauses: &[graphbuff_cypher::ast::Clause]) -> bool {
        use graphbuff_cypher::ast::Clause;
        clauses.iter().any(|c| match c {
            Clause::CallProcedure { name, .. } => {
                self.get(name).is_some_and(|x| x.signature.mode == ProcedureMode::Write)
            }
            Clause::CallSubquery { query, .. } => self.writes_in(query),
            Clause::Foreach { body, .. } => self.writes_in_clauses(body),
            _ => false,
        })
    }

    /// `SHOW PROCEDURES` / `dbms.procedures()` rows: name, signature, description, mode.
    pub(crate) fn listing(&self) -> ProcedureRows {
        self.signatures()
            .map(|s| {
                vec![
                    Value::String(s.name.clone()),
                    Value::String(s.text()),
                    Value::String(s.description.clone()),
                    Value::String(s.mode.name().into()),
                ]
            })
            .collect()
    }
}

/// The configuration map of a graph algorithm: relationship types, orientation and the
/// numbers in `numbers` (name and default), checked for unknown keys.
struct AlgoConfig {
    types: Vec<String>,
    dir: Direction,
    numbers: BTreeMap<&'static str, Value>,
}

impl AlgoConfig {
    fn parse(proc_name: &str, v: &Value, numbers: &[(&'static str, Value)]) -> Result<AlgoConfig> {
        let bad = |m: String| Error::Semantic(format!("{proc_name}: {m}"));
        let map = match v {
            Value::Null => BTreeMap::new(),
            Value::Map(m) => m.clone(),
            o => return Err(bad(format!("the configuration must be a map, not {}", o.type_name()))),
        };
        let mut cfg = AlgoConfig {
            types: Vec::new(),
            dir: Direction::Outgoing,
            numbers: numbers.iter().cloned().collect(),
        };
        for (k, v) in map {
            match (k.as_str(), v) {
                (_, Value::Null) => {}
                ("relationshipTypes", Value::String(t)) => cfg.types = if t == "*" { Vec::new() } else { vec![t] },
                ("relationshipTypes", Value::List(items)) => {
                    for t in items {
                        match t {
                            Value::String(t) if t == "*" => {
                                cfg.types.clear();
                                break;
                            }
                            Value::String(t) => cfg.types.push(t),
                            o => return Err(bad(format!("relationshipTypes holds {}, not a string", o.type_name()))),
                        }
                    }
                }
                ("orientation", Value::String(o)) => {
                    cfg.dir = match o.to_ascii_uppercase().as_str() {
                        "NATURAL" => Direction::Outgoing,
                        "REVERSE" => Direction::Incoming,
                        "UNDIRECTED" => Direction::Both,
                        _ => return Err(bad(format!("orientation must be NATURAL, REVERSE or UNDIRECTED, not '{o}'"))),
                    }
                }
                (key, v) => {
                    let Some(default) = cfg.numbers.get(key) else {
                        let mut known = vec!["relationshipTypes", "orientation"];
                        known.extend(numbers.iter().map(|(n, _)| *n));
                        return Err(bad(format!("unknown setting `{key}` (known: {})", known.join(", "))));
                    };
                    let ok = matches!((default, &v), (Value::Int(_), Value::Int(_)) | (Value::Float(_), Value::Int(_) | Value::Float(_)));
                    if !ok {
                        return Err(bad(format!("`{key}` must be a number, not {}", v.type_name())));
                    }
                    cfg.numbers.insert(numbers.iter().find(|(n, _)| *n == key).map(|(n, _)| *n).unwrap_or("?"), v);
                }
            }
        }
        Ok(cfg)
    }

    fn int(&self, key: &str) -> i64 {
        self.numbers.get(key).and_then(Value::as_int).unwrap_or(0)
    }

    fn float(&self, key: &str) -> f64 {
        self.numbers.get(key).and_then(Value::as_float).unwrap_or(0.0)
    }
}

/// One row per node of `csr`: the node and `value(index)`.
fn per_node(csr: &Csr, value: impl Fn(usize) -> Value) -> ProcedureRows {
    csr.nodes().iter().enumerate().map(|(i, n)| vec![Value::Node(*n), value(i)]).collect()
}

fn algo_page_rank(ctx: &ProcedureContext<'_>, args: &[Value]) -> Result<ProcedureRows> {
    let cfg = AlgoConfig::parse(
        "algo.pageRank",
        &args[0],
        &[("maxIterations", Value::Int(20)), ("dampingFactor", Value::Float(0.85)), ("tolerance", Value::Float(1e-7))],
    )?;
    let (iterations, damping, tolerance) = (cfg.int("maxIterations"), cfg.float("dampingFactor"), cfg.float("tolerance"));
    if !(1..=1_000_000).contains(&iterations) || !(0.0..1.0).contains(&damping) || tolerance.is_nan() || tolerance < 0.0 {
        return Err(Error::Semantic(
            "algo.pageRank: needs 1 <= maxIterations <= 1000000, 0 <= dampingFactor < 1 and tolerance >= 0".into(),
        ));
    }
    let csr = ctx.csr()?;
    ctx.check_interrupt()?;
    let types = csr.type_ids(&cfg.types);
    let (scores, _) = ctx.kernels().page_rank(&csr, &types, cfg.dir, damping, iterations as u32, tolerance);
    ctx.check_interrupt()?;
    Ok(per_node(&csr, |i| Value::Float(scores[i])))
}

fn algo_wcc(ctx: &ProcedureContext<'_>, args: &[Value]) -> Result<ProcedureRows> {
    let cfg = AlgoConfig::parse("algo.wcc", &args[0], &[])?;
    let csr = ctx.csr()?;
    ctx.check_interrupt()?;
    let comp = ctx.kernels().wcc(&csr, &csr.type_ids(&cfg.types));
    ctx.check_interrupt()?;
    Ok(per_node(&csr, |i| Value::Int(csr.node_id(comp[i]).0 as i64)))
}

fn algo_triangles(ctx: &ProcedureContext<'_>, args: &[Value]) -> Result<ProcedureRows> {
    let cfg = AlgoConfig::parse("algo.triangleCount", &args[0], &[])?;
    let csr = ctx.csr()?;
    ctx.check_interrupt()?;
    let t = ctx.kernels().triangles(&csr, &csr.type_ids(&cfg.types));
    ctx.check_interrupt()?;
    Ok(per_node(&csr, |i| Value::Int(t[i] as i64)))
}

fn algo_bfs(ctx: &ProcedureContext<'_>, args: &[Value]) -> Result<ProcedureRows> {
    let cfg = AlgoConfig::parse("algo.bfs", &args[1], &[("maxDepth", Value::Int(-1))])?;
    let csr = ctx.csr()?;
    let mut sources = Vec::new();
    let mut add = |v: &Value| -> Result<()> {
        match v {
            Value::Node(n) => {
                sources.extend(csr.node_index(*n));
                Ok(())
            }
            Value::Null => Ok(()),
            o => Err(Error::Type(format!("algo.bfs: sourceNode must be a node or a list of nodes, not {}", o.type_name()))),
        }
    };
    match &args[0] {
        Value::List(items) => items.iter().try_for_each(&mut add)?,
        v => add(v)?,
    }
    let max = match cfg.int("maxDepth") {
        d if d < 0 => u32::MAX - 1,
        d => d.min(i64::from(u32::MAX - 1)) as u32,
    };
    ctx.check_interrupt()?;
    let depth = ctx.kernels().bfs(&csr, &sources, cfg.dir, &csr.type_ids(&cfg.types), max);
    ctx.check_interrupt()?;
    let mut reached: Vec<(u32, NodeId)> = depth
        .iter()
        .enumerate()
        .filter(|(_, d)| **d != u32::MAX)
        .map(|(i, d)| (*d, csr.node_id(i as u32)))
        .collect();
    reached.sort_unstable();
    Ok(reached.into_iter().map(|(d, n)| vec![Value::Node(n), Value::Int(i64::from(d))]).collect())
}

fn strings(items: impl IntoIterator<Item = String>) -> Value {
    Value::List(items.into_iter().map(Value::String).collect())
}

const NONE: [&str; 0] = [];

type Builtin = fn(&ProcedureContext<'_>, &[Value]) -> Result<ProcedureRows>;

fn register_builtins(p: &mut Procedures) {
    let mut add = |sig: ProcedureSignature, f: Builtin| {
        p.register(sig, f).expect("built-in procedures are valid");
    };
    add(
        ProcedureSignature::new("db.labels", NONE, ["label"])
            .description("The labels of the nodes in the graph."),
        |ctx, _| {
            let s = ctx.store();
            let mut labels: Vec<String> = s.labels().into_iter().filter(|l| s.label_count(l) > 0).collect();
            labels.sort();
            Ok(labels.into_iter().map(|l| vec![Value::String(l)]).collect())
        },
    );
    add(
        ProcedureSignature::new("db.relationshipTypes", NONE, ["relationshipType"])
            .description("The types of the relationships in the graph."),
        |ctx, _| {
            let s = ctx.store();
            let mut types: Vec<String> = s
                .relationship_types()
                .into_iter()
                .filter(|t| s.relationship_type_count(t) > 0)
                .collect();
            types.sort();
            Ok(types.into_iter().map(|t| vec![Value::String(t)]).collect())
        },
    );
    add(
        ProcedureSignature::new("db.propertyKeys", NONE, ["propertyKey"])
            .description("The property keys used in the graph."),
        |ctx, _| {
            let mut keys = ctx.store().property_keys();
            keys.sort();
            keys.dedup();
            Ok(keys.into_iter().map(|k| vec![Value::String(k)]).collect())
        },
    );
    for (name, outputs, what, body) in [
        (
            "db.schema.nodeTypeProperties",
            &["nodeType", "nodeLabels", "propertyName", "propertyTypes", "mandatory"][..],
            "The properties of each combination of node labels, with their types and whether every node has them.",
            crate::schema::node_type_properties as Builtin,
        ),
        (
            "db.schema.relTypeProperties",
            &["relType", "propertyName", "propertyTypes", "mandatory"][..],
            "The properties of each relationship type, with their types and whether every relationship has them.",
            crate::schema::rel_type_properties,
        ),
        (
            "db.schema.visualization",
            &["nodes", "relationships"][..],
            "The labels (maps of name and count) and which relationship types connect them (maps of type, start, end).",
            crate::schema::visualization,
        ),
    ] {
        let what = format!("{what} Learnt from a sample; config: sampleSize (1000 per label or type; -1: all).");
        add(
            ProcedureSignature::new(name, NONE, outputs.iter().copied())
                .optional("config", Value::Null)
                .input_types([ProcedureType::Map])
                .description(&what),
            body,
        );
    }
    add(
        ProcedureSignature::new(
            "db.indexes",
            NONE,
            ["name", "entityType", "labelOrType", "properties", "type", "options"],
        )
        .description("The property, vector and full-text indexes (as SHOW INDEXES)."),
        |ctx, _| Ok(ctx.store().indexes().into_iter().map(crate::vector::index_row).collect()),
    );
    add(
        ProcedureSignature::new("db.index.vector.queryNodes", ["indexName", "numberOfNearestNeighbours", "query"], ["node", "score"])
            .description("The nodes of a vector index most similar to a vector, best first, with their scores."),
        |ctx, args| crate::vector::query_index(ctx, graphbuff_core::IndexEntity::Node, args),
    );
    add(
        ProcedureSignature::new(
            "db.index.vector.queryRelationships",
            ["indexName", "numberOfNearestNeighbours", "query"],
            ["relationship", "score"],
        )
        .description("The relationships of a vector index most similar to a vector, best first, with their scores."),
        |ctx, args| crate::vector::query_index(ctx, graphbuff_core::IndexEntity::Relationship, args),
    );
    add(
        ProcedureSignature::new("vector.embedBatch", ["texts"], ["index", "text", "vector"])
            .optional("model", Value::Null)
            .optional("options", Value::Null)
            .input_types([ProcedureType::List, ProcedureType::String, ProcedureType::Map])
            .description("Embeds a list of texts in one pass: one row per text, in order, with its vector (null for a null text)."),
        crate::embed::embed_batch,
    );
    add(
        ProcedureSignature::new("vector.chunk", ["text"], ["index", "text", "start", "length", "tokens"])
            .optional("model", Value::Null)
            .optional("options", Value::Null)
            .input_types([ProcedureType::String, ProcedureType::String, ProcedureType::Map])
            .description(
                "Splits a text into chunks that fit an embedding model, cut at paragraphs, sentences or words: \
                 one row per chunk with its character offset and length and its tokens.",
            ),
        crate::embed::chunk,
    );
    add(
        ProcedureSignature::new("vector.embedChunks", ["text"], ["index", "text", "start", "length", "tokens", "vector"])
            .optional("model", Value::Null)
            .optional("options", Value::Null)
            .input_types([ProcedureType::String, ProcedureType::String, ProcedureType::Map])
            .description("Splits a text into chunks as vector.chunk does and embeds them: one row per chunk, with its vector."),
        crate::embed::embed_chunks,
    );
    add(
        ProcedureSignature::new("vector.models", NONE, ["name", "dimensions", "maxTokens", "loaded", "matryoshka", "sparse", "multiVector"])
            .description("The installed embedding models."),
        crate::embed::models,
    );
    add(
        ProcedureSignature::new("db.create.setNodeVectorProperty", ["node", "key", "vector"], NONE)
            .input_types([ProcedureType::Node, ProcedureType::String, ProcedureType::List])
            .writes()
            .description("Sets a property of a node to a vector (a list of numbers, checked and stored as floats)."),
        |ctx, args| crate::vector::set_vector_property(ctx, graphbuff_core::IndexEntity::Node, args),
    );
    add(
        ProcedureSignature::new("db.create.setRelationshipVectorProperty", ["relationship", "key", "vector"], NONE)
            .input_types([ProcedureType::Relationship, ProcedureType::String, ProcedureType::List])
            .writes()
            .description("Sets a property of a relationship to a vector (a list of numbers, checked and stored as floats)."),
        |ctx, args| crate::vector::set_vector_property(ctx, graphbuff_core::IndexEntity::Relationship, args),
    );
    add(
        ProcedureSignature::new("db.index.fulltext.queryNodes", ["indexName", "queryString"], ["node", "score"])
            .optional("options", Value::Map(BTreeMap::new()))
            .description("The nodes of a full-text index matching a query (Lucene syntax), best first, with their scores."),
        |ctx, args| crate::fulltext::query_index(ctx, graphbuff_core::IndexEntity::Node, args),
    );
    add(
        ProcedureSignature::new("db.index.fulltext.queryRelationships", ["indexName", "queryString"], ["relationship", "score"])
            .optional("options", Value::Map(BTreeMap::new()))
            .description(
                "The relationships of a full-text index matching a query (Lucene syntax), best first, with their scores.",
            ),
        |ctx, args| crate::fulltext::query_index(ctx, graphbuff_core::IndexEntity::Relationship, args),
    );
    add(
        ProcedureSignature::new("db.index.fulltext.listAvailableAnalyzers", NONE, ["analyzer", "description", "stopwords"])
            .description("The analyzers a full-text index can use."),
        |_, _| Ok(crate::fulltext::list_analyzers()),
    );
    add(
        ProcedureSignature::new("db.index.fulltext.awaitEventuallyConsistentIndexRefresh", NONE, NONE)
            .description("Returns at once: full-text indexes are up to date when a transaction commits."),
        |_, _| Ok(Vec::new()),
    );
    add(
        ProcedureSignature::new("db.constraints", NONE, crate::exec::CONSTRAINT_COLUMNS)
            .description("The constraints (as SHOW CONSTRAINTS)."),
        |ctx, _| Ok(ctx.store().constraints().into_iter().map(crate::exec::constraint_row).collect()),
    );
    add(
        ProcedureSignature::new("db.awaitIndexes", NONE, NONE)
            .optional("timeOutSeconds", 300i64)
            .description("Waits for indexes to come online; they always are, so it returns at once."),
        |_, _| Ok(Vec::new()),
    );
    add(
        ProcedureSignature::new("dbms.procedures", NONE, ["name", "signature", "description", "mode"])
            .description("The procedures that CALL can run (as SHOW PROCEDURES)."),
        |ctx, _| Ok(ctx.procedures().listing()),
    );
    add(
        ProcedureSignature::new("dbms.functions", NONE, crate::catalog::FUNCTION_COLUMNS)
            .description("The built-in functions (as SHOW FUNCTIONS)."),
        |_, _| Ok(crate::catalog::function_rows()),
    );
    let config = || Value::Map(BTreeMap::new());
    add(
        ProcedureSignature::new("algo.pageRank", NONE, ["node", "score"])
            .optional("config", config())
            .description(
                "PageRank of every node (Neo4j GDS formula). Config: relationshipTypes, orientation, \
                 maxIterations (20), dampingFactor (0.85), tolerance (1e-7).",
            ),
        algo_page_rank,
    );
    add(
        ProcedureSignature::new("algo.wcc", NONE, ["node", "componentId"])
            .optional("config", config())
            .description(
                "Weakly connected components: per node, the id of the smallest node in its component. \
                 Config: relationshipTypes.",
            ),
        algo_wcc,
    );
    add(
        ProcedureSignature::new("algo.triangleCount", NONE, ["node", "triangleCount"])
            .optional("config", config())
            .description(
                "Triangles through each node, as an undirected simple graph. Config: relationshipTypes.",
            ),
        algo_triangles,
    );
    add(
        ProcedureSignature::new("algo.bfs", ["sourceNode"], ["node", "depth"])
            .optional("config", config())
            .description(
                "Breadth-first search from a node (or list of nodes): every node reached, with its hop \
                 distance, nearest first. Config: relationshipTypes, orientation, maxDepth (-1: no limit).",
            ),
        algo_bfs,
    );
    add(
        ProcedureSignature::new("dbms.components", NONE, ["name", "versions", "edition"])
            .description("The database product and its version."),
        |_, _| {
            Ok(vec![vec![
                Value::String("graphbuff".into()),
                strings([env!("CARGO_PKG_VERSION").to_string()]),
                Value::String("community".into()),
            ]])
        },
    );
}
