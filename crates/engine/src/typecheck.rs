//! Compile-time type checks of expressions (openCypher's `InvalidArgumentType`): an
//! operand whose type is known before the query runs (a literal, a list of them, a
//! node or relationship variable …) must suit its operator or function — `NOT 1`,
//! `[] AND true`, `'a' % 2`, `1 IN 'abc'`, `length(n)` for a node. Anything whose type
//! is not known is left to the runtime checks.

use std::collections::HashMap;

use graphbuff_core::{Error, Result, Value};
use graphbuff_cypher::ast::{BinaryOp, Expr, MapProjectionItem, UnaryOp};

/// A type known at compile time.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ty {
    /// Not known: any value may come.
    Any,
    Null,
    Bool,
    Int,
    Float,
    Str,
    /// A list with the given element type (`Any` when mixed or not known).
    List(Box<Ty>),
    Map,
    Node,
    Rel,
    Path,
}

impl Ty {
    fn name(&self) -> &'static str {
        match self {
            Ty::Any => "ANY",
            Ty::Null => "NULL",
            Ty::Bool => "BOOLEAN",
            Ty::Int => "INTEGER",
            Ty::Float => "FLOAT",
            Ty::Str => "STRING",
            Ty::List(_) => "LIST",
            Ty::Map => "MAP",
            Ty::Node => "NODE",
            Ty::Rel => "RELATIONSHIP",
            Ty::Path => "PATH",
        }
    }

    fn of_value(v: &Value) -> Ty {
        match v {
            Value::Null => Ty::Null,
            Value::Bool(_) => Ty::Bool,
            Value::Int(_) => Ty::Int,
            Value::Float(_) => Ty::Float,
            Value::String(_) => Ty::Str,
            Value::List(items) => Ty::List(Box::new(common(items.iter().map(Ty::of_value)))),
            Value::Map(_) => Ty::Map,
            Value::Node(_) => Ty::Node,
            Value::Rel(_) => Ty::Rel,
            Value::Path(_) => Ty::Path,
            Value::Temporal(_) | Value::Point(_) => Ty::Any,
        }
    }

    fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float)
    }

    /// May hold a value of a type `ok` accepts (`Any` and `Null` always may).
    fn allows(&self, ok: impl Fn(&Ty) -> bool) -> bool {
        matches!(self, Ty::Any | Ty::Null) || ok(self)
    }
}

/// The type every one of `tys` has, ignoring nulls; `Any` when they differ.
fn common(tys: impl Iterator<Item = Ty>) -> Ty {
    let mut out: Option<Ty> = None;
    for t in tys {
        if t == Ty::Null {
            continue;
        }
        match &out {
            None => out = Some(t),
            Some(o) if *o == t => {}
            Some(_) => return Ty::Any,
        }
    }
    out.unwrap_or(Ty::Any)
}

fn invalid(what: &str, t: &Ty) -> Error {
    Error::Semantic(format!("type mismatch: {what}, got {}", t.name()))
}

/// The variables in scope and their known types.
pub(crate) type TyEnv = HashMap<String, Ty>;

/// Checks a predicate (`WHERE …`): it must be a boolean.
pub(crate) fn check_predicate(e: &Expr, env: &TyEnv) -> Result<()> {
    let t = check(e, env)?;
    if !t.allows(|t| *t == Ty::Bool) {
        return Err(invalid("WHERE expects a BOOLEAN", &t));
    }
    Ok(())
}

/// Checks what `DELETE` deletes: nodes, relationships, paths, or lists of them.
pub(crate) fn check_deletable(e: &Expr, env: &TyEnv) -> Result<()> {
    let t = check(e, env)?;
    if matches!(t, Ty::Bool | Ty::Int | Ty::Float | Ty::Str | Ty::Map) {
        return Err(invalid("DELETE expects a NODE, RELATIONSHIP or PATH", &t));
    }
    Ok(())
}

/// Checks `e` and returns its type.
pub(crate) fn check(e: &Expr, env: &TyEnv) -> Result<Ty> {
    let with = |var: &str, list: &Expr| -> Result<TyEnv> {
        let elem = match check(list, env)? {
            Ty::List(e) => *e,
            _ => Ty::Any,
        };
        let mut inner = env.clone();
        inner.insert(var.to_string(), elem);
        Ok(inner)
    };
    Ok(match e {
        Expr::Literal(v) => Ty::of_value(v),
        Expr::Param(_) | Expr::CountStar | Expr::AggregateRef(_) | Expr::Subquery { .. } => Ty::Any,
        Expr::Var(v) => env.get(v).cloned().unwrap_or(Ty::Any),
        Expr::Property(base, _) => {
            let t = check(base, env)?;
            // On a path it is a compile-time error; on another value a type error, as
            // openCypher files them.
            if t == Ty::Path {
                return Err(invalid("a property needs a NODE, RELATIONSHIP or MAP", &t));
            }
            if matches!(t, Ty::Bool | Ty::Int | Ty::Float | Ty::Str | Ty::List(_)) {
                return Err(Error::Type(format!("type mismatch: a property needs a NODE, RELATIONSHIP or MAP, got {}", t.name())));
            }
            Ty::Any
        }
        Expr::Index(a, b) => {
            check(a, env)?;
            check(b, env)?;
            Ty::Any
        }
        Expr::Slice(a, b, c) => {
            let t = check(a, env)?;
            for x in [b, c].into_iter().flatten() {
                check(x, env)?;
            }
            match t {
                Ty::List(_) => t,
                _ => Ty::Any,
            }
        }
        Expr::List(items) => {
            let tys = items.iter().map(|i| check(i, env)).collect::<Result<Vec<_>>>()?;
            Ty::List(Box::new(common(tys.into_iter())))
        }
        Expr::Map(entries) => {
            for (_, v) in entries {
                check(v, env)?;
            }
            Ty::Map
        }
        Expr::MapProjection { base, items } => {
            check(base, env)?;
            for it in items {
                if let MapProjectionItem::Entry(_, v) = it {
                    check(v, env)?;
                }
            }
            Ty::Map
        }
        Expr::Unary(op, x) => {
            let t = check(x, env)?;
            match op {
                UnaryOp::Not => {
                    if !t.allows(|t| *t == Ty::Bool) {
                        return Err(invalid("NOT expects a BOOLEAN", &t));
                    }
                    Ty::Bool
                }
                UnaryOp::Neg | UnaryOp::Pos => {
                    if matches!(t, Ty::Bool | Ty::Str | Ty::List(_) | Ty::Map | Ty::Node | Ty::Rel | Ty::Path) {
                        return Err(invalid("unary minus and plus expect a number", &t));
                    }
                    if t.is_numeric() { t } else { Ty::Any }
                }
            }
        }
        Expr::Binary(op, a, b) => {
            let (ta, tb) = (check(a, env)?, check(b, env)?);
            binary(*op, &ta, &tb)?
        }
        Expr::IsNull { expr, .. } => {
            check(expr, env)?;
            Ty::Bool
        }
        Expr::HasLabels(x, _) | Expr::HasLabelExpr(x, _) => {
            check(x, env)?;
            Ty::Bool
        }
        Expr::PatternPredicate(_) => Ty::Bool,
        Expr::Function { name, args, .. } => {
            let tys = args.iter().map(|a| check(a, env)).collect::<Result<Vec<_>>>()?;
            function(name, &tys)?
        }
        Expr::Case { subject, whens, else_ } => {
            if let Some(s) = subject {
                check(s, env)?;
            }
            let mut results = Vec::new();
            for (w, t) in whens {
                let tw = check(w, env)?;
                if subject.is_none() && !tw.allows(|t| *t == Ty::Bool) {
                    return Err(invalid("CASE WHEN expects a BOOLEAN", &tw));
                }
                results.push(check(t, env)?);
            }
            if let Some(x) = else_ {
                results.push(check(x, env)?);
            }
            common(results.into_iter())
        }
        Expr::ListComprehension { var, list, filter, map } => {
            let inner = with(var, list)?;
            if let Some(f) = filter {
                check_predicate(f, &inner)?;
            }
            match map {
                Some(m) => Ty::List(Box::new(check(m, &inner)?)),
                None => check(list, env)?,
            }
        }
        Expr::Reduce { acc, init, var, list, expr } => {
            check(init, env)?;
            let mut inner = with(var, list)?;
            inner.insert(acc.clone(), Ty::Any);
            check(expr, &inner)?;
            Ty::Any
        }
        Expr::Quantified { var, list, pred, .. } => {
            let inner = with(var, list)?;
            check_predicate(pred, &inner)?;
            Ty::Bool
        }
    })
}

fn binary(op: BinaryOp, a: &Ty, b: &Ty) -> Result<Ty> {
    use BinaryOp as B;
    let sym = match op {
        B::Or => "OR",
        B::Xor => "XOR",
        B::And => "AND",
        B::Sub => "-",
        B::Mul => "*",
        B::Div => "/",
        B::Mod => "%",
        B::Pow => "^",
        _ => "",
    };
    Ok(match op {
        B::Or | B::Xor | B::And => {
            for t in [a, b] {
                if !t.allows(|t| *t == Ty::Bool) {
                    return Err(invalid(&format!("{sym} expects BOOLEAN operands"), t));
                }
            }
            Ty::Bool
        }
        B::Sub | B::Mul | B::Div | B::Mod | B::Pow => {
            // Temporal values and durations take part in `-`, `*` and `/`; their type is
            // `Any` here, so only clearly wrong operands are rejected.
            for t in [a, b] {
                if matches!(t, Ty::Bool | Ty::Str | Ty::List(_) | Ty::Map | Ty::Node | Ty::Rel | Ty::Path) {
                    return Err(invalid(&format!("{sym} expects numbers"), t));
                }
            }
            match (a, b) {
                (Ty::Int, Ty::Int) if op != B::Pow => Ty::Int,
                (x, y) if x.is_numeric() && y.is_numeric() => Ty::Float,
                _ => Ty::Any,
            }
        }
        B::Add => match (a, b) {
            (Ty::Int, Ty::Int) => Ty::Int,
            (x, y) if x.is_numeric() && y.is_numeric() => Ty::Float,
            (Ty::Str, Ty::Str) => Ty::Str,
            _ => Ty::Any,
        },
        B::In => {
            if !b.allows(|t| matches!(t, Ty::List(_))) {
                return Err(invalid("IN expects a LIST on the right", b));
            }
            Ty::Bool
        }
        B::Eq | B::Neq | B::Lt | B::Le | B::Gt | B::Ge => Ty::Bool,
        B::StartsWith | B::EndsWith | B::Contains | B::RegexMatch => Ty::Bool,
    })
}

fn function(name: &str, args: &[Ty]) -> Result<Ty> {
    let arg = |i: usize| args.get(i).cloned().unwrap_or(Ty::Any);
    Ok(match name {
        "size" => {
            let t = arg(0);
            if !t.allows(|t| matches!(t, Ty::Str | Ty::List(_))) {
                return Err(invalid("size() expects a STRING or a LIST", &t));
            }
            Ty::Int
        }
        "length" => {
            let t = arg(0);
            if !t.allows(|t| matches!(t, Ty::Path | Ty::Str)) {
                return Err(invalid("length() expects a PATH", &t));
            }
            Ty::Int
        }
        "labels" => {
            let t = arg(0);
            if !t.allows(|t| *t == Ty::Node) {
                return Err(invalid("labels() expects a NODE", &t));
            }
            Ty::List(Box::new(Ty::Str))
        }
        "type" => {
            let t = arg(0);
            if !t.allows(|t| *t == Ty::Rel) {
                return Err(invalid("type() expects a RELATIONSHIP", &t));
            }
            Ty::Str
        }
        "properties" => {
            let t = arg(0);
            if !t.allows(|t| matches!(t, Ty::Node | Ty::Rel | Ty::Map)) {
                return Err(invalid("properties() expects a NODE, RELATIONSHIP or MAP", &t));
            }
            Ty::Map
        }
        "tostring" => Ty::Str,
        "count" => Ty::Int,
        "collect" => Ty::List(Box::new(Ty::Any)),
        _ => Ty::Any,
    })
}

/// Fails with `variable … not defined` (openCypher's `UndefinedVariable`) if `e` reads a
/// variable that is neither in `env` nor bound inside `e` (by a list comprehension,
/// `reduce`, or a quantifier). Variables of a pattern predicate must all be bound: it
/// cannot introduce new ones. Subqueries are checked when they are planned.
/// `known_function` tells whether a (non-aggregate) function name exists.
pub(crate) fn check_defined(e: &Expr, env: &TyEnv, known_function: &dyn Fn(&str) -> bool) -> Result<()> {
    fn go(e: &Expr, env: &TyEnv, local: &mut Vec<String>, kf: &dyn Fn(&str) -> bool) -> Result<()> {
        let known = |v: &str, local: &Vec<String>| env.contains_key(v) || local.iter().any(|l| l == v);
        let undefined = |v: &str| Err(Error::Semantic(format!("variable `{v}` not defined")));
        let scoped = |var: &str, parts: &[Option<&Expr>], local: &mut Vec<String>| -> Result<()> {
            local.push(var.to_string());
            let r = parts.iter().flatten().try_for_each(|x| go(x, env, local, kf));
            local.pop();
            r
        };
        match e {
            Expr::Var(v) if !known(v, local) => return undefined(v),
            Expr::MapProjection { base, items } => {
                go(base, env, local, kf)?;
                for item in items {
                    match item {
                        MapProjectionItem::Entry(_, x) => go(x, env, local, kf)?,
                        MapProjectionItem::Var(v) if !known(v, local) => return undefined(v),
                        _ => {}
                    }
                }
            }
            Expr::PatternPredicate(part) => {
                if let Some(v) = part.variables().into_iter().find(|v| !known(v, local)) {
                    return undefined(v);
                }
                let nodes = std::iter::once(&part.start).chain(part.chain.iter().map(|(_, n)| n));
                let props = nodes
                    .filter_map(|n| n.props.as_ref())
                    .chain(part.chain.iter().filter_map(|(r, _)| r.props.as_ref()));
                for p in props {
                    go(p, env, local, kf)?;
                }
            }
            Expr::ListComprehension { var, list, filter, map } => {
                go(list, env, local, kf)?;
                scoped(var, &[filter.as_deref(), map.as_deref()], local)?;
            }
            Expr::Quantified { var, list, pred, .. } => {
                go(list, env, local, kf)?;
                scoped(var, &[Some(pred)], local)?;
            }
            Expr::Reduce { acc, init, var, list, expr } => {
                go(init, env, local, kf)?;
                go(list, env, local, kf)?;
                local.push(acc.clone());
                let r = scoped(var, &[Some(expr)], local);
                local.pop();
                r?;
            }
            Expr::Subquery { query, .. } if query.is_updating() => {
                return Err(Error::Semantic("an EXISTS, COUNT or COLLECT subquery cannot change the graph".into()));
            }
            Expr::Subquery { .. } => {}
            Expr::Function { name, args, .. } => {
                if !e.is_aggregate_call() && name != "exists" && !kf(name) {
                    return Err(Error::Semantic(format!("unknown function '{name}'")));
                }
                // A pattern is a predicate; only exists() takes one as its argument.
                if name != "exists" && args.iter().any(|a| matches!(a, Expr::PatternPredicate(_))) {
                    return Err(Error::Semantic(format!("{name}() cannot take a pattern; a pattern is a predicate")));
                }
                for a in args {
                    go(a, env, local, kf)?;
                }
            }
            _ => {
                for child in children(e) {
                    go(child, env, local, kf)?;
                }
            }
        }
        Ok(())
    }
    go(e, env, &mut Vec::new(), known_function)
}

/// The direct sub-expressions of `e` (not those of binders; see [`check_defined`]).
fn children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Property(x, _) | Expr::Unary(_, x) | Expr::HasLabels(x, _) | Expr::HasLabelExpr(x, _) => vec![x],
        Expr::IsNull { expr, .. } => vec![expr],
        Expr::Index(a, b) | Expr::Binary(_, a, b) => vec![a, b],
        Expr::Slice(a, b, c) => std::iter::once(&**a).chain(b.as_deref()).chain(c.as_deref()).collect(),
        Expr::List(items) | Expr::Function { args: items, .. } => items.iter().collect(),
        Expr::Map(items) => items.iter().map(|(_, x)| x).collect(),
        Expr::Case { subject, whens, else_ } => subject
            .as_deref()
            .into_iter()
            .chain(whens.iter().flat_map(|(w, t)| [w, t]))
            .chain(else_.as_deref())
            .collect(),
        _ => Vec::new(),
    }
}
