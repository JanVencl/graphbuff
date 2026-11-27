//! `search`: nodes by meaning (a vector index, the text embedded by the database's
//! model), by words (a full-text index), or both (reciprocal rank fusion).

use std::collections::BTreeMap;

use graphbuff::{GraphStore, IndexDef, IndexEntity, IndexKind, Value};

use crate::json::{Json, to_value};
use crate::users::Access;

use super::explore::{Checked, node, params, quote, read, text};
use super::rpc::string;
use super::tools::{Input, Output};

/// Results unless asked.
const K: i64 = 10;
/// The constant of reciprocal rank fusion: a result's score is the sum of
/// `1 / (RRF_K + rank)` over the lists it is in.
const RRF_K: f64 = 60.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Vector,
    Fulltext,
    Hybrid,
}

/// The indexes a search runs on.
struct Plan {
    mode: Mode,
    vector: Option<IndexDef>,
    fulltext: Option<IndexDef>,
}

fn names(defs: &[&IndexDef]) -> String {
    defs.iter().map(|d| format!("{} ({} on {})", d.name, d.kind.text().to_lowercase(), d.target())).collect::<Vec<_>>().join(", ")
}

/// Picks the indexes: the one named, or the only fitting one (`auto`: a label with both
/// kinds is searched both ways).
fn plan(indexes: &[IndexDef], named: Option<&str>, mode: Option<&str>) -> Result<Plan, String> {
    let vectors: Vec<&IndexDef> = indexes.iter().filter(|d| d.is_vector() && d.entity == IndexEntity::Node).collect();
    let texts: Vec<&IndexDef> = indexes.iter().filter(|d| d.is_fulltext() && d.entity == IndexEntity::Node).collect();
    let mode = match mode {
        None | Some("auto") => None,
        Some("vector") => Some(Mode::Vector),
        Some("fulltext") => Some(Mode::Fulltext),
        Some("hybrid") => Some(Mode::Hybrid),
        Some(m) => return Err(format!("`mode` is auto, vector, fulltext or hybrid, not `{m}`")),
    };
    let all: Vec<&IndexDef> = vectors.iter().chain(&texts).copied().collect();
    if all.is_empty() {
        return Err("this database has no vector or full-text index on nodes to search; read_cypher can still find \
                    nodes by their properties"
            .into());
    }
    // The other kind of index on the same label as `d`, if there is exactly one.
    let partner = |d: &IndexDef, of: &[&IndexDef]| -> Option<IndexDef> {
        let same: Vec<&&IndexDef> = of.iter().filter(|o| o.labels().any(|l| l == d.label)).collect();
        (same.len() == 1).then(|| (*same[0]).clone())
    };
    if let Some(n) = named {
        let Some(d) = all.iter().find(|d| d.name == n) else {
            return Err(format!("no vector or full-text index on nodes named `{n}`; the indexes are {}", names(&all)));
        };
        let d = (*d).clone();
        let (own, other) = if d.is_vector() { (Mode::Vector, &texts) } else { (Mode::Fulltext, &vectors) };
        return match mode {
            None => Ok(if d.is_vector() { Plan { mode: own, vector: Some(d), fulltext: None } } else { Plan { mode: own, vector: None, fulltext: Some(d) } }),
            Some(m) if m == own => Ok(if d.is_vector() { Plan { mode: m, vector: Some(d), fulltext: None } } else { Plan { mode: m, vector: None, fulltext: Some(d) } }),
            Some(Mode::Hybrid) => match partner(&d, other) {
                Some(p) if d.is_vector() => Ok(Plan { mode: Mode::Hybrid, vector: Some(d), fulltext: Some(p) }),
                Some(p) => Ok(Plan { mode: Mode::Hybrid, vector: Some(p), fulltext: Some(d) }),
                None => Err(format!("hybrid search needs a vector and a full-text index on `{}`; there is not exactly one other", d.label)),
            },
            Some(_) => Err(format!("`{n}` is a {} index; leave out `mode`, or name another index", d.kind.text().to_lowercase())),
        };
    }
    let pairs: Vec<(IndexDef, IndexDef)> =
        vectors.iter().filter_map(|v| partner(v, &texts).map(|t| ((*v).clone(), t))).collect();
    let only = |list: &[&IndexDef], what: &str| -> Result<IndexDef, String> {
        match list {
            [d] => Ok((*d).clone()),
            [] => Err(format!("there is no {what} index on nodes; the indexes are {}", names(&all))),
            _ => Err(format!("several {what} indexes fit; pass `index`, one of: {}", names(list))),
        }
    };
    match mode {
        Some(Mode::Vector) => Ok(Plan { mode: Mode::Vector, vector: Some(only(&vectors, "vector")?), fulltext: None }),
        Some(Mode::Fulltext) => Ok(Plan { mode: Mode::Fulltext, vector: None, fulltext: Some(only(&texts, "full-text")?) }),
        Some(Mode::Hybrid) => match pairs.as_slice() {
            [(v, t)] => Ok(Plan { mode: Mode::Hybrid, vector: Some(v.clone()), fulltext: Some(t.clone()) }),
            [] => Err(format!("hybrid search needs a vector and a full-text index on one label; the indexes are {}", names(&all))),
            _ => Err(format!("several labels have both kinds of index; pass `index`, one of: {}", names(&all))),
        },
        None => match (pairs.as_slice(), all.as_slice()) {
            ([(v, t)], _) => Ok(Plan { mode: Mode::Hybrid, vector: Some(v.clone()), fulltext: Some(t.clone()) }),
            (_, [d]) if d.is_vector() => Ok(Plan { mode: Mode::Vector, vector: Some((*d).clone()), fulltext: None }),
            (_, [d]) => Ok(Plan { mode: Mode::Fulltext, vector: None, fulltext: Some((*d).clone()) }),
            _ => Err(format!("several indexes fit; pass `index`, one of: {}", names(&all))),
        },
    }
}

/// `text` as words for a full-text query: everything but letters and digits is a space,
/// and the words that are operators (`AND`, `OR`, `NOT`, `TO`) are lowered, so that the
/// text is looked for as it is.
pub(crate) fn words(text: &str) -> String {
    let plain: String = text.chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    plain
        .split_whitespace()
        .map(|w| if ["AND", "OR", "NOT", "TO"].contains(&w) { w.to_lowercase() } else { w.to_string() })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `WHERE` of `filters` on variable `v`, with their values in `out`: `{property:
/// value | {gte, lte, gt, lt, in}}`, joined by `AND`. Only `allowed` properties, when
/// given (a vector index's filter properties).
fn filter(filters: &Json, v: &str, allowed: Option<&[String]>, out: &mut BTreeMap<String, Value>) -> Result<String, String> {
    let Json::Object(m) = filters else { return Err("`filters` must be an object of property names and values".into()) };
    let mut parts = Vec::new();
    for (prop, cond) in m {
        if let Some(a) = allowed
            && !a.contains(prop)
        {
            return Err(if a.is_empty() {
                "this vector index has no filter properties, so `filters` cannot be used with it".into()
            } else {
                format!("`{prop}` is not a filter property of the vector index; those are {}", a.join(", "))
            });
        }
        let mut add = |op: &str, value: &Json| {
            let p = format!("f{}", out.len());
            parts.push(format!("{v}.{} {op} ${p}", quote(prop)));
            out.insert(p, to_value(value.clone()));
        };
        match cond {
            Json::Object(ops) => {
                if ops.is_empty() {
                    return Err(format!("the filter on `{prop}` is empty"));
                }
                for (op, value) in ops {
                    let op = match op.as_str() {
                        "gte" => ">=",
                        "lte" => "<=",
                        "gt" => ">",
                        "lt" => "<",
                        "in" if matches!(value, Json::Array(_)) => "IN",
                        "in" => return Err(format!("`in` on `{prop}` needs a list")),
                        o => return Err(format!("unknown filter `{o}` on `{prop}`; the filters are gte, lte, gt, lt, in")),
                    };
                    add(op, value);
                }
            }
            Json::Null => return Err(format!("the filter on `{prop}` is null")),
            value => add("=", value),
        }
    }
    Ok(parts.join(" AND "))
}

/// The model that embeds the query: `requested`, else the index's, else the default
/// (`None`). A known model that cannot make vectors of the index's dimensions is an error
/// that names the fitting ones.
fn model(db: &graphbuff::Database<graphbuff::BoxedStore>, index: &IndexDef, requested: Option<String>) -> Result<Option<String>, String> {
    let IndexKind::Vector { dimensions, .. } = index.kind else { return Ok(None) };
    let model = requested.or_else(|| index.embedding_model.clone());
    let Some(embedder) = db.embedder() else {
        return Err("this server has no embedding models, so it cannot search by meaning; use mode fulltext, or \
                    install a model ([embedding] in the config file)"
            .into());
    };
    let models = embedder.models();
    if let Some(m) = &model {
        let fitting: Vec<&str> = models.iter().filter(|x| x.fits(dimensions as usize)).map(|x| x.name.as_str()).collect();
        match models.iter().find(|x| &x.name == m) {
            None => {
                return Err(format!(
                    "no embedding model `{m}` here; the models that fit index `{}` ({dimensions} dimensions) are: {}",
                    index.name,
                    if fitting.is_empty() { "none".to_string() } else { fitting.join(", ") }
                ));
            }
            Some(x) if !x.fits(dimensions as usize) => {
                return Err(format!(
                    "model `{m}` makes vectors of {} dimensions, but index `{}` has {dimensions}; the models that fit: {}",
                    x.dimensions,
                    index.name,
                    if fitting.is_empty() { "none".to_string() } else { fitting.join(", ") }
                ));
            }
            Some(_) => {}
        }
    }
    Ok(model)
}

fn indexes(db: &graphbuff::Database<graphbuff::BoxedStore>) -> Vec<IndexDef> {
    match db.snapshot() {
        Some(s) => s.indexes(),
        None => db.read(|s| s.indexes()),
    }
}

/// The node and score of each result row.
fn hits(rows: Vec<Json>) -> Vec<(i64, Json, f64)> {
    rows.into_iter()
        .filter_map(|r| {
            let Json::Object(mut m) = r else { return None };
            let score = match m.remove("score") {
                Some(Json::Float(f)) => f,
                Some(Json::Int(n)) => n as f64,
                _ => 0.0,
            };
            let node = m.remove("node")?;
            let id = match &node {
                Json::Object(n) => match n.get("id") {
                    Some(Json::Int(id)) => *id,
                    _ => return None,
                },
                _ => return None,
            };
            Some((id, node, score))
        })
        .collect()
}

pub(crate) fn search(i: &Input) -> Output {
    let run = || -> Result<Output, Output> {
        let e = Output::error;
        let c = Checked::new(i, &["text", "index", "mode", "k", "filters", "model", "database"]).map_err(e)?;
        let Some(query_text) = c.string("text").map_err(e)? else {
            return Err(e("`text` (what to look for) is required".into()));
        };
        let db_arg = c.database().map_err(e)?;
        let k = c.int("k", 1..=i.limits.max_rows_limit as i64, Some(K.min(i.limits.max_rows_limit as i64))).map_err(e)?;
        let ctx = i.ctx;
        let name = match &db_arg {
            None => ctx.mgr.default_database(),
            Some(d) => match ctx.mgr.database(d) {
                Ok(_) => ctx.mgr.resolve_name(&d.to_ascii_lowercase()),
                Err(_) => return Err(e(format!("database `{d}` does not exist; list_databases gives those you may use"))),
            },
        };
        if super::cap(ctx.access_on(i.who, &name)) < Access::Read {
            return Err(e(format!("you may not read database `{name}`; list_databases gives those you may use")));
        }
        let db = ctx.mgr.database(&name).map_err(|x| e(super::render::error_text(&x)))?;
        let p = plan(&indexes(&db), c.string("index").map_err(e)?.as_deref(), c.string("mode").map_err(e)?.as_deref()).map_err(e)?;
        let filters = c.args.get("filters").filter(|f| !matches!(f, Json::Null));
        let db_arg = Some(name.clone());
        let mut lists = Vec::new();
        let mut used_model = None;

        if let Some(v) = &p.vector {
            let m = model(&db, v, c.string("model").map_err(e)?).map_err(e)?;
            let mut ps = params([("text", Value::String(query_text.clone())), ("k", Value::Int(k))]);
            ps.insert("model".into(), m.clone().map_or(Value::Null, Value::String));
            // The index's length, so that a Matryoshka model cuts its vector to fit.
            let IndexKind::Vector { dimensions, .. } = v.kind else { unreachable!("a vector index") };
            ps.insert("dims".into(), Value::Int(i64::from(dimensions)));
            let mut w = String::new();
            if let Some(f) = filters {
                let cond = filter(f, "n", Some(v.filter_properties()), &mut ps).map_err(e)?;
                if !cond.is_empty() {
                    w = format!(" WHERE {cond}");
                }
            }
            let q = format!(
                "WITH vector.embed($text, $model, {{task: 'query', dimensions: $dims}}) AS v \
                 MATCH (n:{}) SEARCH n IN (VECTOR INDEX {} FOR v{w} LIMIT $k) SCORE AS score \
                 RETURN score, {} AS node",
                quote(&v.label),
                quote(&v.name),
                node("n")
            );
            let r = read(i, &db_arg, &q, ps, k as usize).map_err(|o| {
                if o.text.contains("dimension") {
                    e(format!("{} (the default model does not fit index `{}`: pass `model`; vector.models lists them)", o.text, v.name))
                } else {
                    o
                }
            })?;
            used_model = Some(m.unwrap_or_else(|| "the default model".into()));
            lists.push(hits(r.rows));
        }
        if let Some(t) = &p.fulltext {
            let w = words(&query_text);
            if w.is_empty() {
                if p.mode == Mode::Fulltext {
                    return Err(e("`text` has no words to look for".into()));
                }
            } else {
                let mut ps = params([("q", Value::String(w)), ("index", Value::String(t.name.clone()))]);
                let mut cond = String::new();
                if let Some(f) = filters {
                    let allowed = p.vector.as_ref().map(|v| v.filter_properties());
                    let x = filter(f, "n", allowed, &mut ps).map_err(e)?;
                    if !x.is_empty() {
                        cond = format!(" WHERE {x}");
                    }
                }
                let q = format!(
                    "CALL db.index.fulltext.queryNodes($index, $q) YIELD node AS n, score WITH n, score{cond} \
                     RETURN score, {} AS node LIMIT $k",
                    node("n")
                );
                ps.insert("k".into(), Value::Int(k));
                lists.push(hits(read(i, &db_arg, &q, ps, k as usize)?.rows));
            }
        }

        // One list, best first: as it is, or fused by rank.
        let results: Vec<Json> = if lists.len() == 1 {
            lists.remove(0).into_iter().map(|(_, n, s)| result(n, s)).collect()
        } else {
            let mut fused: Vec<(i64, Json, f64)> = Vec::new();
            for list in lists {
                for (rank, (id, n, _)) in list.into_iter().enumerate() {
                    let add = 1.0 / (RRF_K + rank as f64 + 1.0);
                    match fused.iter_mut().find(|(x, _, _)| *x == id) {
                        Some(f) => f.2 += add,
                        None => fused.push((id, n, add)),
                    }
                }
            }
            fused.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)));
            fused.truncate(k as usize);
            fused.into_iter().map(|(_, n, s)| result(n, s)).collect()
        };
        let mut out = vec![
            ("database", string(name)),
            (
                "mode",
                string(match p.mode {
                    Mode::Vector => "vector",
                    Mode::Fulltext => "fulltext",
                    Mode::Hybrid => "hybrid",
                }),
            ),
        ];
        let used: Vec<Json> = p.vector.iter().chain(&p.fulltext).map(|d| string(d.name.clone())).collect();
        out.push(("indexes", Json::Array(used)));
        if let Some(m) = used_model {
            out.push(("model", string(m)));
        }
        if p.mode == Mode::Hybrid {
            out.push(("scores", string("hybrid scores are reciprocal rank fusion (k = 60) of the vector and full-text ranks")));
        }
        out.push(("results", Json::Array(results)));
        Ok(text(out))
    };
    run().unwrap_or_else(|o| o)
}

fn result(node: Json, score: f64) -> Json {
    Json::Object(BTreeMap::from([("node".to_string(), node), ("score".to_string(), Json::Float(score))]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_searched_as_they_are() {
        assert_eq!(words("cats AND dogs: \"quoted\" (x) title:y *"), "cats and dogs quoted x title y");
        assert_eq!(words("C++ / C#"), "C C");
    }

    #[test]
    fn filters() {
        let mut ps = BTreeMap::new();
        let f = crate::json::parse(r#"{"year":{"gte":2020,"lt":2025},"tenant":5,"tag":{"in":["a","b"]}}"#).unwrap();
        let w = filter(&f, "n", None, &mut ps).unwrap();
        assert_eq!(w, "n.`tag` IN $f0 AND n.`tenant` = $f1 AND n.`year` >= $f2 AND n.`year` < $f3");
        assert!(filter(&f, "n", Some(&["year".into()]), &mut BTreeMap::new()).unwrap_err().contains("filter property"));
        assert!(filter(&crate::json::parse(r#"{"a":{"like":1}}"#).unwrap(), "n", None, &mut BTreeMap::new()).is_err());
    }
}
