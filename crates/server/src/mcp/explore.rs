//! `get_node`, `neighbors` and `find_path`: exploring a graph without writing Cypher.
//! They build Cypher with parameters (names quoted), and run it as `read_cypher` does,
//! so roles, timeouts and limits apply.

use std::collections::BTreeMap;

use graphbuff::Value;

use crate::json::{self, Json};

use super::cypher::{Args, execute};
use super::rpc::string;
use super::tools::{Input, Output};

/// Neighbours `neighbors` returns unless asked.
const NEIGHBORS: usize = 25;
/// Hops `find_path` looks unless asked, and at most.
const HOPS: i64 = 5;
const MAX_HOPS: i64 = 10;

/// A node as the tools show it.
const NODE: &str = "{id: id(N), labels: labels(N), properties: properties(N)}";

fn node(var: &str) -> String {
    NODE.replace('N', var)
}

/// `` `name` ``, quoted for Cypher.
fn quote(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// The checked arguments of a tool.
struct Checked<'a> {
    args: &'a BTreeMap<String, Json>,
}

impl<'a> Checked<'a> {
    fn new(i: &'a Input, known: &[&str]) -> Result<Checked<'a>, String> {
        if let Some(k) = i.args.keys().find(|k| !known.contains(&k.as_str())) {
            return Err(format!("unknown argument `{k}`; the arguments are {}", known.join(", ")));
        }
        Ok(Checked { args: i.args })
    }

    fn present(&self, name: &str) -> bool {
        !matches!(self.args.get(name), None | Some(Json::Null))
    }

    fn int(&self, name: &str, range: std::ops::RangeInclusive<i64>, default: Option<i64>) -> Result<i64, String> {
        match self.args.get(name) {
            None | Some(Json::Null) => default.ok_or_else(|| format!("`{name}` (a node id) is required")),
            Some(Json::Int(n)) if range.contains(n) => Ok(*n),
            Some(_) => Err(format!("`{name}` must be a whole number from {} to {}", range.start(), range.end())),
        }
    }

    fn id(&self, name: &str) -> Result<i64, String> {
        self.int(name, 0..=i64::MAX, None)
    }

    fn string(&self, name: &str) -> Result<Option<String>, String> {
        match self.args.get(name) {
            None | Some(Json::Null) => Ok(None),
            Some(Json::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
            Some(_) => Err(format!("`{name}` must be a string")),
        }
    }

    fn database(&self) -> Result<Option<String>, String> {
        self.string("database")
    }

    /// `[:A|B]`'s `:A|B`, or nothing for every type.
    fn types(&self) -> Result<String, String> {
        match self.args.get("types") {
            None | Some(Json::Null) => Ok(String::new()),
            Some(Json::Array(items)) if !items.is_empty() => {
                let names = items
                    .iter()
                    .map(|t| match t {
                        Json::String(s) if !s.is_empty() => Ok(quote(s)),
                        _ => Err("`types` must be a list of relationship type names".to_string()),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!(":{}", names.join("|")))
            }
            Some(_) => Err("`types` must be a non-empty list of relationship type names".into()),
        }
    }

    fn bool(&self, name: &str) -> Result<bool, String> {
        match self.args.get(name) {
            None | Some(Json::Null) => Ok(false),
            Some(Json::Bool(b)) => Ok(*b),
            Some(_) => Err(format!("`{name}` must be true or false")),
        }
    }
}

/// What a query gave: its database, its rows as objects of the column names, and
/// whether rows were left out.
struct Rows {
    database: String,
    rows: Vec<Json>,
    truncated: bool,
}

/// Runs `query` as a read, at most `max_rows` rows.
fn read(i: &Input, database: &Option<String>, query: &str, params: BTreeMap<String, Value>, max_rows: usize) -> Result<Rows, Output> {
    let a = Args { query: query.to_string(), params, database: database.clone(), max_rows, dry_run: false };
    let out = execute(i, &a, false);
    if out.is_error {
        return Err(out);
    }
    let Ok(Json::Object(mut m)) = json::parse(&out.text) else { return Err(Output::error("an unreadable result")) };
    let columns: Vec<String> = match m.remove("columns") {
        Some(Json::Array(c)) => c.into_iter().filter_map(|c| if let Json::String(s) = c { Some(s) } else { None }).collect(),
        _ => Vec::new(),
    };
    let rows = match m.remove("rows") {
        Some(Json::Array(rows)) => rows
            .into_iter()
            .map(|r| match r {
                Json::Array(values) => Json::Object(columns.iter().cloned().zip(values).collect()),
                other => other,
            })
            .collect(),
        _ => Vec::new(),
    };
    Ok(Rows {
        database: match m.remove("database") {
            Some(Json::String(d)) => d,
            _ => String::new(),
        },
        rows,
        truncated: matches!(m.get("truncated"), Some(Json::Bool(true))),
    })
}

fn params<const N: usize>(pairs: [(&str, Value); N]) -> BTreeMap<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// The text for the model: the fields in this order.
fn text(fields: Vec<(&str, Json)>) -> Output {
    let mut t = String::from("{");
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            t.push(',');
        }
        json::write_str(&mut t, k);
        t.push(':');
        super::rpc::write(&mut t, v);
    }
    t.push('}');
    Output::ok(t)
}

/// The value of a one-column row.
fn only(row: &Json) -> Json {
    match row {
        Json::Object(m) => m.values().next().cloned().unwrap_or(Json::Null),
        other => other.clone(),
    }
}

/// The node with id `id`, or an error that says there is none.
fn the_node(i: &Input, db: &Option<String>, id: i64) -> Result<(String, Json), Output> {
    let r = read(i, db, &format!("MATCH (n) WHERE id(n) = $id RETURN {} AS node", node("n")), params([("id", Value::Int(id))]), 1)?;
    match r.rows.first() {
        Some(row) => Ok((r.database, only(row))),
        None => Err(Output::error(format!(
            "there is no node with id {id} in `{}`; find one with get_node by label and property, or read_cypher",
            r.database
        ))),
    }
}

/// `(n)-[r…]->(m)` for `direction`.
fn pattern(direction: &str, types: &str) -> Result<String, String> {
    Ok(match direction {
        "out" => format!("(n)-[r{types}]->(m)"),
        "in" => format!("(n)<-[r{types}]-(m)"),
        "both" => format!("(n)-[r{types}]-(m)"),
        _ => return Err("`direction` must be out, in or both".into()),
    })
}

/// The relationships of node `id` per type and direction.
fn degrees(i: &Input, db: &Option<String>, id: i64, pattern: &str) -> Result<Vec<Json>, Output> {
    let q = format!(
        "MATCH {pattern} WHERE id(n) = $id \
         RETURN type(r) AS type, CASE WHEN id(startNode(r)) = $id THEN 'out' ELSE 'in' END AS direction, count(*) AS count \
         ORDER BY count DESC, type, direction"
    );
    Ok(read(i, db, &q, params([("id", Value::Int(id))]), 1000)?.rows)
}

pub(crate) fn get_node(i: &Input) -> Output {
    let run = || -> Result<Output, Output> {
        let c = Checked::new(i, &["id", "label", "property", "value", "database"]).map_err(Output::error)?;
        let db = c.database().map_err(Output::error)?;
        if c.present("id") {
            if c.present("label") || c.present("property") || c.present("value") {
                return Err(Output::error("pass `id`, or `label`, `property` and `value`, not both"));
            }
            let id = c.id("id").map_err(Output::error)?;
            let (database, n) = the_node(i, &db, id)?;
            let d = degrees(i, &db, id, "(n)-[r]-(m)")?;
            return Ok(text(vec![("database", string(database)), ("node", n), ("relationships", Json::Array(d))]));
        }
        let (Some(label), Some(property)) = (c.string("label").map_err(Output::error)?, c.string("property").map_err(Output::error)?) else {
            return Err(Output::error("pass `id`, or `label`, `property` and `value`"));
        };
        let Some(value) = c.args.get("value").filter(|v| !matches!(v, Json::Null)) else {
            return Err(Output::error("`value` is required with `label` and `property`"));
        };
        let q = format!("MATCH (n:{}) WHERE n.{} = $value RETURN {} AS node", quote(&label), quote(&property), node("n"));
        let r = read(i, &db, &q, params([("value", crate::json::to_value(value.clone()))]), 10)?;
        let mut out = vec![("database", string(r.database)), ("nodes", Json::Array(r.rows.iter().map(only).collect()))];
        if r.truncated {
            out.push(("note", string("more than 10 nodes match; showing 10")));
        } else if r.rows.is_empty() {
            out.push(("note", string("no node matches; get_schema lists the labels and properties")));
        }
        Ok(text(out))
    };
    run().unwrap_or_else(|e| e)
}

pub(crate) fn neighbors(i: &Input) -> Output {
    let run = || -> Result<Output, Output> {
        let c = Checked::new(i, &["id", "types", "direction", "depth", "limit", "database"]).map_err(Output::error)?;
        let db = c.database().map_err(Output::error)?;
        let id = c.id("id").map_err(Output::error)?;
        let types = c.types().map_err(Output::error)?;
        let direction = c.string("direction").map_err(Output::error)?.unwrap_or_else(|| "both".into());
        let pat = pattern(&direction, &types).map_err(Output::error)?;
        let depth = c.int("depth", 1..=3, Some(1)).map_err(Output::error)?;
        let max = i.limits.max_rows_limit as i64;
        let limit = c.int("limit", 1..=max, Some(NEIGHBORS.min(i.limits.max_rows_limit) as i64)).map_err(Output::error)? as usize;
        let (database, n) = the_node(i, &db, id)?;
        let d = degrees(i, &db, id, &pat)?;
        let total: i64 = d.iter().map(|r| if let Json::Object(m) = r { if let Some(Json::Int(c)) = m.get("count") { *c } else { 0 } } else { 0 }).sum();
        let mut found = Vec::new();
        let mut cut = false;
        if depth == 1 {
            // Spread over the types, so that a hub's biggest type does not hide the rest.
            let groups: Vec<(String, String)> = d
                .iter()
                .filter_map(|r| match r {
                    Json::Object(m) => match (m.get("type"), m.get("direction")) {
                        (Some(Json::String(t)), Some(Json::String(dir))) => Some((t.clone(), dir.clone())),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            let share = (limit / groups.len().max(1)).max(1);
            for (t, dir) in groups.iter().take(limit) {
                let p = pattern(dir, &format!(":{}", quote(t))).map_err(Output::error)?;
                let q = format!(
                    "MATCH {p} WHERE id(n) = $id RETURN type(r) AS type, '{dir}' AS direction, properties(r) AS relationship, {} AS node",
                    node("m")
                );
                let r = read(i, &db, &q, params([("id", Value::Int(id))]), share)?;
                cut |= r.truncated;
                found.extend(r.rows);
            }
            cut |= groups.len() > limit;
        } else {
            let arrow = match direction.as_str() {
                "out" => ("-", "->"),
                "in" => ("<-", "-"),
                _ => ("-", "-"),
            };
            let q = format!(
                "MATCH (n) WHERE id(n) = $id MATCH p = (n){}[{types}*1..{depth}]{}(m) WHERE m <> n \
                 WITH m, min(length(p)) AS hops RETURN hops, {} AS node ORDER BY hops, id(m)",
                arrow.0,
                arrow.1,
                node("m")
            );
            let r = read(i, &db, &q, params([("id", Value::Int(id))]), limit)?;
            cut = r.truncated;
            found = r.rows;
        }
        let mut out = vec![
            ("database", string(database)),
            ("node", n),
            ("relationships", Json::Array(d)),
            ("neighbors", Json::Array(found)),
        ];
        if cut {
            out.push((
                "note",
                string(format!(
                    "not every neighbour is shown ({total} relationships in all); pass `types`, `direction` or a bigger `limit`"
                )),
            ));
        }
        Ok(text(out))
    };
    run().unwrap_or_else(|e| e)
}

pub(crate) fn find_path(i: &Input) -> Output {
    let run = || -> Result<Output, Output> {
        let c = Checked::new(i, &["from", "to", "types", "max_hops", "all", "database"]).map_err(Output::error)?;
        let db = c.database().map_err(Output::error)?;
        let from = c.id("from").map_err(Output::error)?;
        let to = c.id("to").map_err(Output::error)?;
        let types = c.types().map_err(Output::error)?;
        let hops = c.int("max_hops", 1..=MAX_HOPS, Some(HOPS)).map_err(Output::error)?;
        let all = c.bool("all").map_err(Output::error)?;
        let (database, _) = the_node(i, &db, from)?;
        the_node(i, &db, to)?;
        if from == to {
            return Err(Output::error("`from` and `to` are the same node"));
        }
        let f = if all { "allShortestPaths" } else { "shortestPath" };
        let q = format!(
            "MATCH (a) WHERE id(a) = $from MATCH (b) WHERE id(b) = $to \
             MATCH p = {f}((a)-[{types}*..{hops}]-(b)) \
             RETURN length(p) AS hops, [x IN nodes(p) | {}] AS nodes, \
             [r IN relationships(p) | {{type: type(r), start: id(startNode(r)), end: id(endNode(r)), properties: properties(r)}}] AS relationships",
            node("x")
        );
        let max = if all { i.limits.max_rows } else { 1 };
        let r = read(i, &db, &q, params([("from", Value::Int(from)), ("to", Value::Int(to))]), max)?;
        let mut out = vec![("database", string(database)), ("paths", Json::Array(r.rows.clone()))];
        if r.rows.is_empty() {
            out.push(("note", string(format!("no path of at most {hops} relationships; try a bigger max_hops or fewer type limits"))));
        } else if r.truncated {
            out.push(("note", string(format!("more shortest paths than {max}; showing {max}"))));
        }
        Ok(text(out))
    };
    run().unwrap_or_else(|e| e)
}

#[cfg(test)]
mod tests {
    #[test]
    fn quoting() {
        assert_eq!(super::quote("a`b"), "`a``b`");
        assert_eq!(super::node("m"), "{id: id(m), labels: labels(m), properties: properties(m)}");
    }
}
