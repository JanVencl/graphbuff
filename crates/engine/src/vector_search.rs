//! `MATCH … SEARCH v IN (VECTOR INDEX name FOR query [WHERE …] LIMIT k) [SCORE AS s]`: what
//! the clause accepts, and running its search.

use graphbuff_core::{Error, IndexDef, IndexEntity, NodeId, RelId, Result, Value};
use graphbuff_cypher::ast::{BinaryOp, Expr, PatternPart, RelDirection, RelPattern, Search, UnaryOp};

use crate::context::{Env, Executor, Scope};
use crate::filter_column::{Compare, Predicate};
use crate::plan::{expr_text, expr_vars};
use crate::procedures::ProcedureContext;
use crate::typecheck::{self, TyEnv};

/// The largest `LIMIT`, as in Neo4j.
const MAX_LIMIT: i64 = i32::MAX as i64;

/// A planned search of a vector index.
#[derive(Debug, Clone)]
pub(crate) struct VectorSearch {
    /// The variable it binds to each entity found.
    pub var: String,
    /// The variable it binds to the entity's score.
    pub score: Option<String>,
    def: IndexDef,
    query: Expr,
    filter: Vec<Condition>,
    limit: Expr,
    /// For a relationship index: what the relationships found must match.
    pub rel: Option<RelPattern>,
}

/// A condition on one filter property.
#[derive(Debug, Clone)]
struct Condition {
    /// Which of the index's filter properties.
    column: usize,
    test: Test,
    value: Expr,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Test {
    Equal,
    In,
    Compare(Compare),
}

impl Test {
    /// The test of the comparison `op`, and of the same with its sides swapped.
    fn of(op: BinaryOp) -> Option<(Test, Option<Test>)> {
        let both = |a, b| (Test::Compare(a), Some(Test::Compare(b)));
        Some(match op {
            BinaryOp::Eq => (Test::Equal, Some(Test::Equal)),
            BinaryOp::Lt => both(Compare::Lt, Compare::Gt),
            BinaryOp::Le => both(Compare::Le, Compare::Ge),
            BinaryOp::Gt => both(Compare::Gt, Compare::Lt),
            BinaryOp::Ge => both(Compare::Ge, Compare::Le),
            BinaryOp::In => (Test::In, None),
            _ => return None,
        })
    }

    fn text(self) -> &'static str {
        match self {
            Test::Equal => "=",
            Test::In => "IN",
            Test::Compare(Compare::Lt) => "<",
            Test::Compare(Compare::Le) => "<=",
            Test::Compare(Compare::Gt) => ">",
            Test::Compare(Compare::Ge) => ">=",
        }
    }
}

fn mentions(e: &Expr, var: &str) -> bool {
    let mut vars = Default::default();
    expr_vars(e, &mut vars);
    vars.contains(var)
}

/// The key of `var.key`.
fn property_of<'e>(e: &'e Expr, var: &str) -> Option<&'e str> {
    match e {
        Expr::Property(of, key) if matches!(&**of, Expr::Var(v) if v == var) => Some(key),
        _ => None,
    }
}

impl VectorSearch {
    /// Plans `search` in a `MATCH` of `patterns`. `env`: the variables bound before it, the
    /// only ones its expressions may use besides the searched one.
    pub fn plan(ex: &Executor, search: &Search, patterns: &[PatternPart], env: &TyEnv) -> Result<VectorSearch> {
        let var = &search.var;
        for v in std::iter::once(var).chain(&search.score) {
            if env.contains_key(v) || (search.score.as_ref() == Some(var)) {
                return Err(Error::Semantic(format!("variable `{v}` already declared")));
            }
        }
        let (entity, rel) = searched(var, patterns)?;

        let Some(def) = ex.store().indexes().into_iter().find(|d| d.name == search.index && d.is_vector()) else {
            return Err(Error::Semantic(format!("SEARCH: there is no vector index named `{}`", search.index)));
        };
        if def.entity != entity {
            return Err(Error::Semantic(format!(
                "SEARCH: `{}` indexes {}s, and `{var}` is a {}",
                def.name,
                def.entity.text().to_lowercase(),
                entity.text().to_lowercase()
            )));
        }

        let known = |name: &str| ex.function_exists(name);
        let check = |e: &Expr, what: &str| -> Result<()> {
            if mentions(e, var) {
                return Err(Error::Semantic(format!("SEARCH: {what} cannot use `{var}`, the variable searched for")));
            }
            if e.contains_aggregate() {
                return Err(Error::Semantic(format!("SEARCH: {what} cannot use aggregate functions")));
            }
            typecheck::check_defined(e, env, &known)?;
            typecheck::check(e, env)?;
            Ok(())
        };
        check(&search.query, "the query vector")?;
        check(&search.limit, "LIMIT")?;
        let mut filter = Vec::new();
        if let Some(w) = &search.filter {
            conditions(w, var, &def, &mut filter)?;
            for c in &filter {
                check(&c.value, "a value in WHERE")?;
            }
        }
        Ok(VectorSearch {
            var: var.clone(),
            score: search.score.clone(),
            def,
            query: search.query.clone(),
            filter,
            limit: search.limit.clone(),
            rel,
        })
    }

    /// The variables the search binds.
    pub fn binds(&self) -> Vec<&str> {
        std::iter::once(self.var.as_str()).chain(self.score.as_deref()).collect()
    }

    /// The expressions the search evaluates.
    pub fn exprs(&self) -> impl Iterator<Item = &Expr> {
        [&self.query, &self.limit].into_iter().chain(self.filter.iter().map(|c| &c.value)).chain(
            self.rel.iter().filter_map(|r| r.props.as_ref()),
        )
    }

    /// About how many rows it yields.
    pub fn estimate(&self) -> f64 {
        match self.limit {
            Expr::Literal(Value::Int(n)) => n.max(0) as f64,
            _ => 10.0,
        }
    }

    pub fn describe(&self) -> String {
        let target = match self.def.entity {
            IndexEntity::Node => format!("({})", self.var),
            IndexEntity::Relationship => format!("[{}]", self.var),
        };
        let mut out = format!("VectorSearch {target} on {}", self.def.name);
        for (i, c) in self.filter.iter().enumerate() {
            let key = &self.def.filter_properties()[c.column];
            let join = if i == 0 { " WHERE" } else { " AND" };
            out += &format!("{join} {}.{key} {} {}", self.var, c.test.text(), expr_text(&c.value));
        }
        out += &format!(" LIMIT {}", expr_text(&self.limit));
        if let Some(s) = &self.score {
            out += &format!(" SCORE AS {s}");
        }
        out
    }

    /// Runs the search for `row`: the entities found with their scores, best first.
    pub fn run(&self, ex: &Executor<'_>, scope: &Scope, row: &[Value]) -> Result<Vec<(Value, f64)>> {
        let env = Env::new(scope, row);
        let k = match ex.eval(&self.limit, &env)? {
            Value::Int(k) if (0..=MAX_LIMIT).contains(&k) => k as usize,
            Value::Int(k) => return Err(Error::Semantic(format!("SEARCH: LIMIT must be from 0 to {MAX_LIMIT}, not {k}"))),
            o => return Err(Error::Type(format!("SEARCH: LIMIT must be an INTEGER, not {}", o.type_name()))),
        };
        let Some(q) = crate::vector::query_vector("SEARCH", &self.def, &ex.eval(&self.query, &env)?)? else {
            return Ok(Vec::new());
        };
        let filter: Vec<Predicate> =
            self.filter.iter().map(|c| self.predicate(c, ex.eval(&c.value, &env)?)).collect::<Result<_>>()?;
        let ctx = ProcedureContext {
            store: ex.store(),
            procedures: &ex.procedures,
            accel: ex.accel.as_ref(),
            interrupt: ex.interrupt.as_ref(),
            embedder: ex.embedder.as_ref(),
            writes: None,
        };
        let found = crate::vector::nearest(&ctx, &self.def, &q, k, &filter)?;
        Ok(found
            .into_iter()
            .map(|(score, id)| match self.def.entity {
                IndexEntity::Node => (Value::Node(NodeId(id)), score),
                IndexEntity::Relationship => (Value::Rel(RelId(id)), score),
            })
            .collect())
    }

    /// The predicate of `c`, whose right-hand side is `value`.
    fn predicate(&self, c: &Condition, value: Value) -> Result<Predicate> {
        let wrong = |v: &Value| {
            let key = &self.def.filter_properties()[c.column];
            Err(Error::Type(format!(
                "SEARCH: `{}.{key}` can be compared with numbers, strings, booleans and temporal values, not with a {}",
                self.var,
                v.type_name()
            )))
        };
        let scalar = |v: &Value| {
            matches!(
                v,
                Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::String(_) | Value::Temporal(_)
            )
        };
        Ok(match (c.test, value) {
            // `x IN null` is null: nothing passes.
            (Test::In, Value::Null) => Predicate::one_of(c.column, Vec::new()),
            (Test::In, Value::List(items)) => match items.iter().find(|v| !scalar(v)) {
                Some(v) => return wrong(v),
                None => Predicate::one_of(c.column, items),
            },
            (Test::In, v) => {
                return Err(Error::Type(format!("SEARCH: IN needs a list, not a {}", v.type_name())));
            }
            (_, v) if !scalar(&v) => return wrong(&v),
            (Test::Equal, v) => Predicate::equal(c.column, v),
            (Test::Compare(op), v) => Predicate::compare(c.column, op, v),
        })
    }
}

/// What `var` is in `patterns`: a node, or a relationship with its pattern. The `MATCH` of
/// a `SEARCH` has one pattern of at most one relationship, in which only `var` is named
/// and only `var` has labels, types or properties.
fn searched(var: &str, patterns: &[PatternPart]) -> Result<(IndexEntity, Option<RelPattern>)> {
    let wrong = |why: &str| Err(Error::Semantic(format!("the MATCH of a SEARCH {why}")));
    let [part] = patterns else { return wrong("takes a single pattern") };
    if part.shortest.is_some() {
        return wrong("cannot search for paths");
    }
    if part.var.is_some() {
        return wrong("cannot name its path");
    }
    if part.chain.len() > 1 || part.chain.iter().any(|(r, _)| r.length.is_some()) {
        return wrong("takes at most one relationship, of a fixed length");
    }
    let nodes: Vec<_> = std::iter::once(&part.start).chain(part.chain.iter().map(|(_, n)| n)).collect();
    let mut found = None;
    for n in nodes {
        match &n.var {
            Some(v) if v == var && found.is_none() => found = Some((IndexEntity::Node, None)),
            Some(_) => return wrong(&format!("can name only `{var}`")),
            None if n.labels.is_empty() && n.label_expr.is_none() && n.props.is_none() => {}
            None => return wrong(&format!("can have labels, types and properties only on `{var}`")),
        }
    }
    for (r, _) in &part.chain {
        match &r.var {
            Some(v) if v == var && found.is_none() => {
                if r.direction == RelDirection::Both {
                    return wrong(&format!("needs a direction for `{var}`, as in ()-[{var}]->()"));
                }
                found = Some((IndexEntity::Relationship, Some(r.clone())));
            }
            Some(_) => return wrong(&format!("can name only `{var}`")),
            None if r.types.is_empty() && r.type_expr.is_none() && r.props.is_none() => {}
            None => return wrong(&format!("can have labels, types and properties only on `{var}`")),
        }
    }
    match found {
        Some(f) => Ok(f),
        None => Err(Error::Semantic(format!("SEARCH: `{var}` is not in the pattern of its MATCH"))),
    }
}

/// Adds the conditions of the `WHERE` expression `e` to `out`.
fn conditions(e: &Expr, var: &str, def: &IndexDef, out: &mut Vec<Condition>) -> Result<()> {
    let column = |key: &str| {
        def.filter_properties().iter().position(|p| p == key).ok_or_else(|| {
            Error::Semantic(format!(
                "SEARCH: `{key}` is not a filter property of vector index `{}` (CREATE VECTOR INDEX … WITH [{var}.{key}] \
                 makes it one)",
                def.name
            ))
        })
    };
    let boolean = |key: &str, value: bool| {
        Ok(Condition { column: column(key)?, test: Test::Equal, value: Expr::Literal(Value::Bool(value)) })
    };
    match e {
        Expr::Binary(BinaryOp::And, a, b) => {
            conditions(a, var, def, out)?;
            return conditions(b, var, def, out);
        }
        Expr::Binary(op, a, b) => {
            if let Some((test, swapped)) = Test::of(*op) {
                if let Some(key) = property_of(a, var) {
                    out.push(Condition { column: column(key)?, test, value: (**b).clone() });
                    return Ok(());
                }
                if let (Some(test), Some(key)) = (swapped, property_of(b, var)) {
                    out.push(Condition { column: column(key)?, test, value: (**a).clone() });
                    return Ok(());
                }
            }
        }
        Expr::Unary(UnaryOp::Not, of) => {
            if let Some(key) = property_of(of, var) {
                out.push(boolean(key, false)?);
                return Ok(());
            }
        }
        e => {
            if let Some(key) = property_of(e, var) {
                out.push(boolean(key, true)?);
                return Ok(());
            }
        }
    }
    Err(Error::Semantic(format!(
        "the WHERE inside SEARCH takes comparisons (=, <, <=, >, >=, IN) of properties of `{var}` with values, joined by \
         AND, and boolean properties; other conditions go in the WHERE of the MATCH, which filters the results"
    )))
}
