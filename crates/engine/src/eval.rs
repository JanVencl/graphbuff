//! Expression evaluation.

use std::collections::BTreeMap;

use graphbuff_core::{Error, Result, Value};
use std::rc::Rc;

use graphbuff_cypher::ast::{BinaryOp, Expr, MapProjectionItem, Quantifier, Query, SubqueryKind, UnaryOp};

use crate::context::{Env, Executor, Row, Scope};
use crate::plan::QueryPlan;
use crate::ops::{add, arith, cypher_cmp, cypher_eq};

/// Three-valued boolean: `None` is null.
fn to_tri(v: Value, ctx: &str) -> Result<Option<bool>> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(b)),
        other => Err(Error::Type(format!(
            "expected a boolean for {ctx}, got {}",
            other.type_name()
        ))),
    }
}

fn from_tri(t: Option<bool>) -> Value {
    t.map_or(Value::Null, Value::Bool)
}

fn list_index(len: usize, i: i64) -> Option<usize> {
    let idx = if i < 0 { len as i64 + i } else { i };
    (0..len as i64).contains(&idx).then_some(idx as usize)
}

/// Clamp a slice bound (supports negative indices).
fn slice_bound(len: usize, i: i64) -> usize {
    let idx = if i < 0 { len as i64 + i } else { i };
    idx.clamp(0, len as i64) as usize
}

impl Executor<'_> {
    /// Evaluates a predicate: only `true` passes; `null` and `false` don't.
    pub fn eval_predicate(&self, e: &Expr, env: &Env) -> Result<bool> {
        Ok(to_tri(self.eval(e, env)?, "WHERE")? == Some(true))
    }

    /// `EXISTS { … }`, `COUNT { … }`, `COLLECT { … }`: runs the subquery from a row with
    /// every variable visible here.
    fn eval_subquery(&self, kind: SubqueryKind, query: &Query, env: &Env) -> Result<Value> {
        let mut names: Vec<String> = Vec::new();
        let visible = env.scope.names().iter().chain(env.locals.iter().map(|(n, _)| n));
        for n in visible {
            if !n.starts_with('\u{0}') && !names.contains(n) {
                names.push(n.clone());
            }
        }
        let key = (query as *const Query as usize, names.clone());
        let cached = self.subplans.borrow().get(&key).cloned();
        let plan = match cached {
            Some(p) => p,
            None => {
                let p = Rc::new(QueryPlan::build_expression_subquery(kind, query, self, &names)?);
                self.subplans.borrow_mut().insert(key, p.clone());
                p
            }
        };
        let scope = Scope::from_names(names.iter().cloned())?;
        let row: Row = names
            .iter()
            .map(|n| env.lookup(n).cloned().unwrap_or(Value::Null))
            .collect();
        let cap = if kind == SubqueryKind::Exists { 1 } else { usize::MAX };
        let (_, rows) = self.stream_subquery(&plan, &scope, &row, cap)?;
        Ok(match kind {
            SubqueryKind::Exists => Value::Bool(!rows.is_empty()),
            SubqueryKind::Count => Value::Int(rows.len() as i64),
            SubqueryKind::Collect => Value::List(rows.into_iter().map(|mut r| r.swap_remove(0)).collect()),
        })
    }

    pub fn eval(&self, e: &Expr, env: &Env) -> Result<Value> {
        match e {
            Expr::Subquery { kind, query, .. } => self.eval_subquery(*kind, query, env),
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Param(p) => {
                self.params.get(p).cloned().ok_or_else(|| {
                    Error::Semantic(format!("expected parameter ${p} to be provided"))
                })
            }
            Expr::Var(name) => env
                .lookup(name)
                .cloned()
                .ok_or_else(|| Error::Semantic(format!("variable `{name}` not defined"))),
            Expr::Property(target, key) => {
                let v = self.eval(target, env)?;
                self.property_of(&v, key)
            }
            Expr::Index(target, idx) => {
                let t = self.eval(target, env)?;
                let i = self.eval(idx, env)?;
                match (&t, &i) {
                    (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                    (Value::List(items), Value::Int(n)) => {
                        Ok(list_index(items.len(), *n).map_or(Value::Null, |i| items[i].clone()))
                    }
                    (Value::Map(_) | Value::Node(_) | Value::Rel(_), Value::String(k)) => {
                        self.property_of(&t, k)
                    }
                    _ => Err(Error::Type(format!(
                        "cannot index {} with {}",
                        t.type_name(),
                        i.type_name()
                    ))),
                }
            }
            Expr::Slice(target, from, to) => {
                let t = self.eval(target, env)?;
                let from = from.as_ref().map(|f| self.eval(f, env)).transpose()?;
                let to = to.as_ref().map(|f| self.eval(f, env)).transpose()?;
                if t.is_null()
                    || from.as_ref().is_some_and(Value::is_null)
                    || to.as_ref().is_some_and(Value::is_null)
                {
                    return Ok(Value::Null);
                }
                let Value::List(items) = t else {
                    return Err(Error::Type(format!("cannot slice {}", t.type_name())));
                };
                let bound = |v: Option<Value>, default: usize| -> Result<usize> {
                    match v {
                        None => Ok(default),
                        Some(Value::Int(i)) => Ok(slice_bound(items.len(), i)),
                        Some(o) => Err(Error::Type(format!(
                            "slice bounds must be integers, got {}",
                            o.type_name()
                        ))),
                    }
                };
                let (a, b) = (bound(from, 0)?, bound(to, items.len())?);
                Ok(Value::List(if a < b {
                    items[a..b].to_vec()
                } else {
                    Vec::new()
                }))
            }
            Expr::List(items) => Ok(Value::List(
                items
                    .iter()
                    .map(|i| self.eval(i, env))
                    .collect::<Result<_>>()?,
            )),
            Expr::Map(items) => {
                let mut m = BTreeMap::new();
                for (k, v) in items {
                    m.insert(k.clone(), self.eval(v, env)?);
                }
                Ok(Value::Map(m))
            }
            Expr::Unary(op, inner) => {
                let v = self.eval(inner, env)?;
                match op {
                    UnaryOp::Not => Ok(from_tri(to_tri(v, "NOT")?.map(|b| !b))),
                    UnaryOp::Neg => match v {
                        Value::Null => Ok(Value::Null),
                        Value::Int(i) => i
                            .checked_neg()
                            .map(Value::Int)
                            .ok_or_else(|| Error::Runtime("integer overflow".into())),
                        Value::Float(f) => Ok(Value::Float(-f)),
                        o => Err(Error::Type(format!("cannot negate {}", o.type_name()))),
                    },
                    UnaryOp::Pos => match v {
                        Value::Null | Value::Int(_) | Value::Float(_) => Ok(v),
                        o => Err(Error::Type(format!(
                            "unary '+' expects a number, got {}",
                            o.type_name()
                        ))),
                    },
                }
            }
            Expr::Binary(op, l, r) => self.eval_binary(*op, l, r, env),
            Expr::IsNull { expr, negated } => {
                Ok(Value::Bool(self.eval(expr, env)?.is_null() != *negated))
            }
            Expr::HasLabels(target, labels) => match self.eval(target, env)? {
                Value::Null => Ok(Value::Null),
                Value::Node(id) => {
                    for l in labels {
                        if !self.store().node_has_label(id, l)? {
                            return Ok(Value::Bool(false));
                        }
                    }
                    Ok(Value::Bool(true))
                }
                Value::Rel(id) => {
                    let t = self.store().relationship_type(id)?;
                    Ok(Value::Bool(labels.iter().all(|l| t.as_deref() == Some(l.as_str()))))
                }
                o => Err(Error::Type(format!(
                    "label predicates require a node or a relationship, got {}",
                    o.type_name()
                ))),
            },
            Expr::HasLabelExpr(target, expr) => match self.eval(target, env)? {
                Value::Null => Ok(Value::Null),
                Value::Node(id) => Ok(Value::Bool(expr.matches(&self.store().node_labels(id)?))),
                Value::Rel(id) => {
                    let t: Vec<String> = self.store().relationship_type(id)?.into_iter().collect();
                    Ok(Value::Bool(expr.matches(&t)))
                }
                o => Err(Error::Type(format!(
                    "label expressions require a node or a relationship, got {}",
                    o.type_name()
                ))),
            },
            Expr::MapProjection { base, items } => {
                let b = self.eval(base, env)?;
                if b.is_null() {
                    return Ok(Value::Null);
                }
                if !matches!(b, Value::Node(_) | Value::Rel(_) | Value::Map(_)) {
                    return Err(Error::Type(format!(
                        "a map projection needs a node, relationship or map, got {}",
                        b.type_name()
                    )));
                }
                let mut out = BTreeMap::new();
                for item in items {
                    match item {
                        MapProjectionItem::Property(k) => {
                            out.insert(k.clone(), self.property_of(&b, k)?);
                        }
                        MapProjectionItem::AllProperties => {
                            let all = match &b {
                                Value::Node(id) => self.store().node_properties(*id)?,
                                Value::Rel(id) => self.store().relationship_properties(*id)?,
                                Value::Map(m) => m.clone(),
                                _ => unreachable!("checked above"),
                            };
                            out.extend(all);
                        }
                        MapProjectionItem::Entry(k, e) => {
                            out.insert(k.clone(), self.eval(e, env)?);
                        }
                        MapProjectionItem::Var(v) => {
                            out.insert(v.clone(), self.eval(&Expr::Var(v.clone()), env)?);
                        }
                    }
                }
                Ok(Value::Map(out))
            }
            Expr::Function { name, args, .. } if name == "exists" => {
                if args.len() != 1 {
                    return Err(Error::Semantic(
                        "exists() takes exactly one argument".into(),
                    ));
                }
                match &args[0] {
                    p @ Expr::PatternPredicate(_) => self.eval(p, env),
                    other => Ok(Value::Bool(!self.eval(other, env)?.is_null())),
                }
            }
            Expr::Function { .. } | Expr::CountStar if e.is_aggregate_call() => {
                Err(Error::Semantic(
                    "aggregate functions are only allowed in RETURN and WITH projections".into(),
                ))
            }
            Expr::Function { name, args, .. } => {
                let values = args
                    .iter()
                    .map(|a| self.eval(a, env))
                    .collect::<Result<Vec<_>>>()?;
                self.call_function(name, values)
            }
            Expr::CountStar => unreachable!("handled as aggregate above"),
            Expr::AggregateRef(i) => env.aggs.and_then(|a| a.get(*i)).cloned().ok_or_else(|| {
                Error::Semantic("aggregate used outside of an aggregation context".into())
            }),
            Expr::Case {
                subject,
                whens,
                else_,
            } => {
                let subject = subject.as_ref().map(|s| self.eval(s, env)).transpose()?;
                for (w, then) in whens {
                    let hit = match &subject {
                        Some(s) => cypher_eq(s, &self.eval(w, env)?) == Some(true),
                        None => to_tri(self.eval(w, env)?, "CASE WHEN")? == Some(true),
                    };
                    if hit {
                        return self.eval(then, env);
                    }
                }
                else_
                    .as_ref()
                    .map_or(Ok(Value::Null), |e| self.eval(e, env))
            }
            Expr::ListComprehension {
                var,
                list,
                filter,
                map,
            } => {
                let items = match self.eval(list, env)? {
                    Value::Null => return Ok(Value::Null),
                    Value::List(items) => items,
                    o => {
                        return Err(Error::Type(format!(
                            "list comprehension expects a list, got {}",
                            o.type_name()
                        )));
                    }
                };
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let child = env.with_local(var, item.clone());
                    if let Some(f) = filter
                        && !self.eval_predicate(f, &child)?
                    {
                        continue;
                    }
                    out.push(match map {
                        Some(m) => self.eval(m, &child)?,
                        None => item,
                    });
                }
                Ok(Value::List(out))
            }
            Expr::Reduce {
                acc,
                init,
                var,
                list,
                expr,
            } => {
                let items = match self.eval(list, env)? {
                    Value::Null => return Ok(Value::Null),
                    Value::List(items) => items,
                    o => {
                        return Err(Error::Type(format!(
                            "reduce() expects a list, got {}",
                            o.type_name()
                        )));
                    }
                };
                let mut value = self.eval(init, env)?;
                for item in items {
                    let child = env.with_local(acc, value).with_local(var, item);
                    value = self.eval(expr, &child)?;
                }
                Ok(value)
            }
            Expr::Quantified {
                kind,
                var,
                list,
                pred,
            } => {
                let items = match self.eval(list, env)? {
                    Value::Null => return Ok(Value::Null),
                    Value::List(items) => items,
                    o => {
                        return Err(Error::Type(format!(
                            "quantifier expects a list, got {}",
                            o.type_name()
                        )));
                    }
                };
                let (mut trues, mut nulls) = (0usize, 0usize);
                for item in &items {
                    match to_tri(
                        self.eval(pred, &env.with_local(var, item.clone()))?,
                        "quantifier",
                    )? {
                        Some(true) => trues += 1,
                        None => nulls += 1,
                        Some(false) => {}
                    }
                }
                let falses = items.len() - trues - nulls;
                Ok(match kind {
                    Quantifier::All if falses > 0 => Value::Bool(false),
                    Quantifier::All if nulls > 0 => Value::Null,
                    Quantifier::All => Value::Bool(true),
                    Quantifier::Any if trues > 0 => Value::Bool(true),
                    Quantifier::Any if nulls > 0 => Value::Null,
                    Quantifier::Any => Value::Bool(false),
                    Quantifier::None if trues > 0 => Value::Bool(false),
                    Quantifier::None if nulls > 0 => Value::Null,
                    Quantifier::None => Value::Bool(true),
                    Quantifier::Single if trues > 1 => Value::Bool(false),
                    Quantifier::Single if nulls > 0 => Value::Null,
                    Quantifier::Single => Value::Bool(trues == 1),
                })
            }
            Expr::PatternPredicate(part) => Ok(Value::Bool(self.pattern_exists(part, env)?)),
        }
    }

    fn eval_binary(&self, op: BinaryOp, l: &Expr, r: &Expr, env: &Env) -> Result<Value> {
        match op {
            BinaryOp::And => {
                let a = to_tri(self.eval(l, env)?, "AND")?;
                if a == Some(false) {
                    return Ok(Value::Bool(false));
                }
                let b = to_tri(self.eval(r, env)?, "AND")?;
                return Ok(match (a, b) {
                    (_, Some(false)) => Value::Bool(false),
                    (Some(true), Some(true)) => Value::Bool(true),
                    _ => Value::Null,
                });
            }
            BinaryOp::Or => {
                let a = to_tri(self.eval(l, env)?, "OR")?;
                if a == Some(true) {
                    return Ok(Value::Bool(true));
                }
                let b = to_tri(self.eval(r, env)?, "OR")?;
                return Ok(match (a, b) {
                    (_, Some(true)) => Value::Bool(true),
                    (Some(false), Some(false)) => Value::Bool(false),
                    _ => Value::Null,
                });
            }
            BinaryOp::Xor => {
                let a = to_tri(self.eval(l, env)?, "XOR")?;
                let b = to_tri(self.eval(r, env)?, "XOR")?;
                return Ok(match (a, b) {
                    (Some(x), Some(y)) => Value::Bool(x != y),
                    _ => Value::Null,
                });
            }
            _ => {}
        }

        let a = self.eval(l, env)?;
        let b = self.eval(r, env)?;
        match op {
            BinaryOp::Eq => Ok(from_tri(cypher_eq(&a, &b))),
            BinaryOp::Neq => Ok(from_tri(cypher_eq(&a, &b).map(|x| !x))),
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                if a.is_null() || b.is_null() {
                    return Ok(Value::Null);
                }
                // NaN is neither smaller nor larger than any number.
                let nan = |v: &Value| matches!(v, Value::Float(f) if f.is_nan());
                let number = |v: &Value| matches!(v, Value::Int(_) | Value::Float(_));
                if (nan(&a) || nan(&b)) && number(&a) && number(&b) {
                    return Ok(Value::Bool(false));
                }
                Ok(from_tri(cypher_cmp(&a, &b).map(|o| match op {
                    BinaryOp::Lt => o.is_lt(),
                    BinaryOp::Le => o.is_le(),
                    BinaryOp::Gt => o.is_gt(),
                    _ => o.is_ge(),
                })))
            }
            BinaryOp::Add => add(a, b),
            BinaryOp::Sub => arith("-", a, b),
            BinaryOp::Mul => arith("*", a, b),
            BinaryOp::Div => arith("/", a, b),
            BinaryOp::Mod => arith("%", a, b),
            BinaryOp::Pow => arith("^", a, b),
            BinaryOp::In => match b {
                Value::Null => Ok(Value::Null),
                Value::List(items) => {
                    if a.is_null() {
                        // Nothing is in an empty list, not even null.
                        return Ok(if items.is_empty() { Value::Bool(false) } else { Value::Null });
                    }
                    let mut unknown = false;
                    for item in &items {
                        match cypher_eq(&a, item) {
                            Some(true) => return Ok(Value::Bool(true)),
                            None => unknown = true,
                            Some(false) => {}
                        }
                    }
                    Ok(if unknown {
                        Value::Null
                    } else {
                        Value::Bool(false)
                    })
                }
                o => Err(Error::Type(format!(
                    "IN expects a list on the right, got {}",
                    o.type_name()
                ))),
            },
            BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains => match (&a, &b) {
                (Value::String(x), Value::String(y)) => Ok(Value::Bool(match op {
                    BinaryOp::StartsWith => x.starts_with(y.as_str()),
                    BinaryOp::EndsWith => x.ends_with(y.as_str()),
                    _ => x.contains(y.as_str()),
                })),
                _ => Ok(Value::Null),
            },
            BinaryOp::RegexMatch => match (&a, &b) {
                (Value::String(s), Value::String(re)) => {
                    let cached = self.regexes.borrow().get(re).cloned();
                    let regex = match cached {
                        Some(r) => r,
                        None => {
                            let r = Rc::new(crate::regex::Regex::new(re)?);
                            self.regexes.borrow_mut().insert(re.clone(), r.clone());
                            r
                        }
                    };
                    Ok(Value::Bool(regex.is_full_match(s)))
                }
                _ => Ok(Value::Null),
            },
            BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => unreachable!("handled above"),
        }
    }

    pub fn property_of(&self, v: &Value, key: &str) -> Result<Value> {
        self.check_not_deleted(v)?;
        Ok(match v {
            Value::Null => Value::Null,
            Value::Node(id) => self.store().node_property(*id, key)?.unwrap_or_default(),
            Value::Rel(id) => self
                .store()
                .relationship_property(*id, key)?
                .unwrap_or_default(),
            Value::Map(m) => m.get(key).cloned().unwrap_or_default(),
            Value::Temporal(t) => t.field(key).ok_or_else(|| {
                Error::Semantic(format!("a {} has no field `{key}`", t.type_name()))
            })?,
            Value::Point(p) => p.field(key).ok_or_else(|| {
                Error::Semantic(format!("a {} point has no field `{key}`", p.crs().name()))
            })?,
            o => {
                return Err(Error::Type(format!(
                    "cannot access property `{key}` on {}",
                    o.type_name()
                )));
            }
        })
    }
}
