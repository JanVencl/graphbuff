//! Aggregation: extracting aggregate calls from projections and accumulating them.

use std::cmp::Ordering;
use std::collections::HashSet;

use graphbuff_core::{Error, Result, Value};
use graphbuff_cypher::ast::Expr;

use crate::ops::order_cmp;

/// One aggregate call found in a projection.
#[derive(Debug, Clone)]
pub(crate) struct AggSpec {
    pub name: String,
    pub distinct: bool,
    /// `None` for `count(*)`.
    pub arg: Option<Expr>,
}

/// Replaces each aggregate call in `e` with `Expr::AggregateRef(i)`, appending its spec to `aggs`.
pub(crate) fn extract_aggregates(e: &Expr, aggs: &mut Vec<AggSpec>) -> Result<Expr> {
    rewrite(e, &mut |x| {
        let spec = match x {
            Expr::CountStar => AggSpec {
                name: "count".into(),
                distinct: false,
                arg: None,
            },
            Expr::Function {
                name,
                distinct,
                args,
            } if x.is_aggregate_call() => {
                if name.starts_with("percentile") {
                    // The value and the percentile travel together as a two-element list.
                    if args.len() != 2 {
                        return Err(Error::Semantic(format!("{name}() expects exactly two arguments")));
                    }
                    if args.iter().any(Expr::contains_aggregate) {
                        return Err(Error::Semantic("aggregate functions cannot be nested".into()));
                    }
                    aggs.push(AggSpec {
                        name: name.clone(),
                        distinct: *distinct,
                        arg: Some(Expr::List(args.clone())),
                    });
                    return Ok(Some(Expr::AggregateRef(aggs.len() - 1)));
                }
                if args.len() != 1 {
                    return Err(Error::Semantic(format!(
                        "{name}() expects exactly one argument"
                    )));
                }
                if args[0].contains_aggregate() {
                    return Err(Error::Semantic(
                        "aggregate functions cannot be nested".into(),
                    ));
                }
                AggSpec {
                    name: name.clone(),
                    distinct: *distinct,
                    arg: Some(args[0].clone()),
                }
            }
            _ => return Ok(None),
        };
        aggs.push(spec);
        Ok(Some(Expr::AggregateRef(aggs.len() - 1)))
    })
}

/// Rebuilds `e` bottom-up, letting `f` replace any sub-expression (checked top-down first).
pub(crate) fn rewrite(e: &Expr, f: &mut dyn FnMut(&Expr) -> Result<Option<Expr>>) -> Result<Expr> {
    if let Some(replacement) = f(e)? {
        return Ok(replacement);
    }
    let mut r = |x: &Expr| rewrite(x, f).map(Box::new);
    Ok(match e {
        Expr::Literal(_)
        | Expr::Param(_)
        | Expr::Var(_)
        | Expr::CountStar
        | Expr::AggregateRef(_)
        | Expr::PatternPredicate(_)
        | Expr::Subquery { .. } => e.clone(),
        Expr::Property(t, k) => Expr::Property(r(t)?, k.clone()),
        Expr::Index(a, b) => Expr::Index(r(a)?, r(b)?),
        Expr::Slice(a, b, c) => Expr::Slice(
            r(a)?,
            b.as_ref().map(|x| r(x)).transpose()?,
            c.as_ref().map(|x| r(x)).transpose()?,
        ),
        Expr::List(items) => Expr::List(
            items
                .iter()
                .map(|x| r(x).map(|b| *b))
                .collect::<Result<_>>()?,
        ),
        Expr::Map(items) => Expr::Map(
            items
                .iter()
                .map(|(k, x)| Ok((k.clone(), *r(x)?)))
                .collect::<Result<_>>()?,
        ),
        Expr::Unary(op, x) => Expr::Unary(*op, r(x)?),
        Expr::Binary(op, a, b) => Expr::Binary(*op, r(a)?, r(b)?),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: r(expr)?,
            negated: *negated,
        },
        Expr::HasLabels(x, l) => Expr::HasLabels(r(x)?, l.clone()),
        Expr::Function {
            name,
            distinct,
            args,
        } => Expr::Function {
            name: name.clone(),
            distinct: *distinct,
            args: args
                .iter()
                .map(|x| r(x).map(|b| *b))
                .collect::<Result<_>>()?,
        },
        Expr::Case {
            subject,
            whens,
            else_,
        } => Expr::Case {
            subject: subject.as_ref().map(|x| r(x)).transpose()?,
            whens: whens
                .iter()
                .map(|(w, t)| Ok((*r(w)?, *r(t)?)))
                .collect::<Result<_>>()?,
            else_: else_.as_ref().map(|x| r(x)).transpose()?,
        },
        Expr::ListComprehension {
            var,
            list,
            filter,
            map,
        } => Expr::ListComprehension {
            var: var.clone(),
            list: r(list)?,
            filter: filter.as_ref().map(|x| r(x)).transpose()?,
            map: map.as_ref().map(|x| r(x)).transpose()?,
        },
        Expr::Quantified {
            kind,
            var,
            list,
            pred,
        } => Expr::Quantified {
            kind: *kind,
            var: var.clone(),
            list: r(list)?,
            pred: r(pred)?,
        },
        Expr::Reduce {
            acc,
            init,
            var,
            list,
            expr,
        } => Expr::Reduce {
            acc: acc.clone(),
            init: r(init)?,
            var: var.clone(),
            list: r(list)?,
            expr: r(expr)?,
        },
    })
}

enum Acc {
    Count(i64),
    Sum {
        int: i64,
        float: f64,
        is_float: bool,
    },
    Avg {
        sum: f64,
        n: u64,
    },
    Min(Option<Value>),
    Max(Option<Value>),
    Collect(Vec<Value>),
    Stdev {
        values: Vec<f64>,
        sample: bool,
    },
    /// `percentileCont` (interpolated) or `percentileDisc` (an input value).
    Percentile {
        values: Vec<f64>,
        /// All values were integers (`percentileDisc` then returns an integer).
        ints: bool,
        p: Option<f64>,
        cont: bool,
    },
}

pub(crate) struct Accumulator {
    acc: Acc,
    count_star: bool,
    seen: Option<HashSet<Value>>,
}

impl Accumulator {
    pub fn new(spec: &AggSpec) -> Result<Self> {
        let acc = match spec.name.as_str() {
            "count" => Acc::Count(0),
            "sum" => Acc::Sum {
                int: 0,
                float: 0.0,
                is_float: false,
            },
            "avg" => Acc::Avg { sum: 0.0, n: 0 },
            "min" => Acc::Min(None),
            "max" => Acc::Max(None),
            "collect" => Acc::Collect(Vec::new()),
            "stdev" => Acc::Stdev {
                values: Vec::new(),
                sample: true,
            },
            "stdevp" => Acc::Stdev {
                values: Vec::new(),
                sample: false,
            },
            "percentilecont" | "percentiledisc" => Acc::Percentile {
                values: Vec::new(),
                ints: true,
                p: None,
                cont: spec.name == "percentilecont",
            },
            other => {
                return Err(Error::Semantic(format!(
                    "unknown aggregate function '{other}'"
                )));
            }
        };
        Ok(Accumulator {
            acc,
            count_star: spec.arg.is_none(),
            seen: spec.distinct.then(HashSet::new),
        })
    }

    pub fn update(&mut self, v: Value) -> Result<()> {
        if self.count_star {
            if let Acc::Count(n) = &mut self.acc {
                *n += 1;
            }
            return Ok(());
        }
        if v.is_null() {
            return Ok(());
        }
        if let Acc::Percentile { values, ints, p, cont } = &mut self.acc {
            let name = if *cont { "percentileCont" } else { "percentileDisc" };
            let Value::List(mut pair) = v else { unreachable!("planned as a pair") };
            let pct = pair.pop().unwrap_or_default();
            let value = pair.pop().unwrap_or_default();
            if value.is_null() {
                return Ok(());
            }
            let pct = match pct.as_float() {
                Some(x) if (0.0..=1.0).contains(&x) => x,
                _ => {
                    return Err(Error::Runtime(format!(
                        "{name}() needs a percentile between 0.0 and 1.0, got {pct}"
                    )));
                }
            };
            p.get_or_insert(pct);
            if let Some(seen) = &mut self.seen
                && !seen.insert(value.clone())
            {
                return Ok(());
            }
            match value {
                Value::Int(i) => values.push(i as f64),
                Value::Float(f) => {
                    values.push(f);
                    *ints = false;
                }
                o => {
                    return Err(Error::Type(format!("{name}() expects numbers, got {}", o.type_name())));
                }
            }
            return Ok(());
        }
        if let Some(seen) = &mut self.seen {
            if !seen.insert(v.clone()) {
                return Ok(());
            }
        }
        match &mut self.acc {
            Acc::Count(n) => *n += 1,
            Acc::Sum {
                int,
                float,
                is_float,
            } => match v {
                Value::Int(i) => match int.checked_add(i) {
                    Some(s) => *int = s,
                    None => {
                        *float += i as f64;
                        *is_float = true;
                    }
                },
                Value::Float(f) => {
                    *float += f;
                    *is_float = true;
                }
                o => {
                    return Err(Error::Type(format!(
                        "sum() expects numbers, got {}",
                        o.type_name()
                    )));
                }
            },
            Acc::Avg { sum, n } => match v.as_float() {
                Some(f) => {
                    *sum += f;
                    *n += 1;
                }
                None => {
                    return Err(Error::Type(format!(
                        "avg() expects numbers, got {}",
                        v.type_name()
                    )));
                }
            },
            Acc::Min(cur) => {
                if cur
                    .as_ref()
                    .is_none_or(|c| order_cmp(&v, c) == Ordering::Less)
                {
                    *cur = Some(v);
                }
            }
            Acc::Max(cur) => {
                if cur
                    .as_ref()
                    .is_none_or(|c| order_cmp(&v, c) == Ordering::Greater)
                {
                    *cur = Some(v);
                }
            }
            Acc::Collect(items) => items.push(v),
            Acc::Percentile { .. } => unreachable!("handled above"),
            Acc::Stdev { values, .. } => match v.as_float() {
                Some(f) => values.push(f),
                None => {
                    return Err(Error::Type(format!(
                        "stdev() expects numbers, got {}",
                        v.type_name()
                    )));
                }
            },
        }
        Ok(())
    }

    pub fn finish(self) -> Value {
        match self.acc {
            Acc::Count(n) => Value::Int(n),
            Acc::Sum {
                int,
                float,
                is_float,
            } => {
                if is_float {
                    Value::Float(int as f64 + float)
                } else {
                    Value::Int(int)
                }
            }
            Acc::Avg { sum, n } => {
                if n == 0 {
                    Value::Null
                } else {
                    Value::Float(sum / n as f64)
                }
            }
            Acc::Min(v) | Acc::Max(v) => v.unwrap_or_default(),
            Acc::Collect(items) => Value::List(items),
            Acc::Percentile {
                mut values,
                ints,
                p,
                cont,
            } => {
                let (Some(p), false) = (p, values.is_empty()) else {
                    return Value::Null;
                };
                values.sort_by(f64::total_cmp);
                let n = values.len();
                if cont {
                    let pos = p * (n - 1) as f64;
                    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
                    Value::Float(values[lo] + (values[hi] - values[lo]) * (pos - lo as f64))
                } else {
                    // The smallest value with at least `p` of the values at or below it.
                    let i = ((p * n as f64).ceil() as usize).clamp(1, n) - 1;
                    if ints { Value::Int(values[i] as i64) } else { Value::Float(values[i]) }
                }
            }
            Acc::Stdev { values, sample } => {
                let n = values.len();
                let denom = if sample { n.saturating_sub(1) } else { n };
                if denom == 0 {
                    return Value::Float(0.0);
                }
                let mean = values.iter().sum::<f64>() / n as f64;
                let var = values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / denom as f64;
                Value::Float(var.sqrt())
            }
        }
    }
}
